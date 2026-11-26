use crate::config::TikTokConfig;
use crate::error::ApiError;
use crate::http::send_json;
use crate::models::{
    format_compact_count, AccountMetrics, AudienceRecord, CampaignRecord, NormalizedStatus,
    OperationRecord, PlatformConnectionStatus, PlatformId, PlatformReport,
};
use crate::platforms::{
    date_window_ymd, extra_from_object, json_f64, json_i64, json_str, json_u64, parse_timestamp,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

async fn tiktok_get(
    client: &Client,
    cfg: &TikTokConfig,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value, ApiError> {
    let url = format!("{}{}", cfg.base_url, path);
    let req = client
        .get(&url)
        .header("Access-Token", &cfg.access_token)
        .header("Accept", "application/json")
        .query(query);
    let val: Value = send_json(req, path).await?;
    let code = val.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let message = json_str(&val, "message").unwrap_or_else(|| "TikTok API error".to_string());
        return Err(ApiError::PlatformApi {
            endpoint: path.to_string(),
            code: code.to_string(),
            message,
        });
    }
    Ok(val.get("data").cloned().unwrap_or(Value::Null))
}

fn normalize_tiktok_audience_status(raw: Option<&Value>) -> (String, NormalizedStatus) {
    match raw {
        Some(Value::String(s)) => (s.clone(), NormalizedStatus::from_raw(s)),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(1) => ("not_ready".to_string(), NormalizedStatus::Warning),
            Some(2) => ("ready".to_string(), NormalizedStatus::Ready),
            Some(3) => ("expired".to_string(), NormalizedStatus::Failed),
            Some(4) => ("processing".to_string(), NormalizedStatus::Processing),
            Some(other) => (format!("status_{other}"), NormalizedStatus::Neutral),
            None => ("unknown".to_string(), NormalizedStatus::Neutral),
        },
        _ => ("unknown".to_string(), NormalizedStatus::Neutral),
    }
}

pub async fn collect(client: &Client, cfg: &TikTokConfig, lookback_days: u32) -> PlatformReport {
    let start = Instant::now();
    let (since, until) = date_window_ymd(lookback_days);
    let endpoint_base = format!("{}/open_api/v1.3", cfg.base_url);

    let adv_id = cfg.advertiser_id.as_str();
    let q_paged = [
        ("advertiser_id", adv_id),
        ("page", "1"),
        ("page_size", "100"),
    ];
    let q_adv_rep = [
        ("advertiser_id", adv_id),
        ("report_type", "BASIC"),
        ("data_level", "AUCTION_ADVERTISER"),
        ("dimensions", r#"["advertiser_id"]"#),
        (
            "metrics",
            r#"["spend","impressions","clicks","conversion","ctr","cpc"]"#,
        ),
        ("start_date", since.as_str()),
        ("end_date", until.as_str()),
        ("page", "1"),
        ("page_size", "100"),
    ];
    let q_camp_rep = [
        ("advertiser_id", adv_id),
        ("report_type", "BASIC"),
        ("data_level", "AUCTION_CAMPAIGN"),
        ("dimensions", r#"["campaign_id"]"#),
        (
            "metrics",
            r#"["spend","impressions","clicks","conversion","ctr","cpc"]"#,
        ),
        ("start_date", since.as_str()),
        ("end_date", until.as_str()),
        ("page", "1"),
        ("page_size", "100"),
    ];
    let (aud_res, adv_rep_res, camp_res, camp_rep_res, bal_res) = tokio::join!(
        tiktok_get(
            client,
            cfg,
            "/open_api/v1.3/dmp/custom_audience/list/",
            &q_paged,
        ),
        tiktok_get(
            client,
            cfg,
            "/open_api/v1.3/report/integrated/get/",
            &q_adv_rep,
        ),
        tiktok_get(client, cfg, "/open_api/v1.3/campaign/get/", &q_paged),
        tiktok_get(
            client,
            cfg,
            "/open_api/v1.3/report/integrated/get/",
            &q_camp_rep,
        ),
        async {
            match &cfg.bc_id {
                Some(bc_id) => Some(
                    tiktok_get(
                        client,
                        cfg,
                        "/open_api/v1.3/advertiser/balance/get/",
                        &[
                            ("bc_id", bc_id.as_str()),
                            ("page", "1"),
                            ("page_size", "100"),
                        ],
                    )
                    .await,
                ),
                None => None,
            }
        }
    );

    if aud_res.is_err() && adv_rep_res.is_err() && camp_res.is_err() {
        let msg = aud_res
            .err()
            .or_else(|| adv_rep_res.err())
            .map(|e| e.to_string())
            .unwrap_or_else(|| "TikTok Ads API request failed".to_string());
        return PlatformReport::failed(
            PlatformId::Tiktok,
            endpoint_base,
            start.elapsed().as_millis() as u64,
            msg,
        );
    }

    let mut warnings = Vec::new();
    let mut currency = None;
    let mut balance = None;
    let mut account_name = None;
    let mut account_status = None;

    if let Some(res) = bal_res {
        match res {
            Ok(data) => {
                if let Some(list) = data
                    .get("advertiser_account_list")
                    .and_then(Value::as_array)
                {
                    if let Some(row) = list.iter().find(|r| {
                        json_str(r, "advertiser_id").as_deref() == Some(cfg.advertiser_id.as_str())
                    }) {
                        account_name = json_str(row, "advertiser_name");
                        account_status = json_str(row, "account_status");
                        currency = json_str(row, "currency");
                        balance = json_f64(row, "valid_account_balance")
                            .or_else(|| json_f64(row, "account_balance"))
                            .or_else(|| json_f64(row, "cash_balance"));
                    }
                }
            }
            Err(e) => warnings.push(format!("Advertiser balance unavailable: {e}")),
        }
    }

    let mut spend = None;
    let mut impressions = None;
    let mut clicks = None;
    let mut conversions = None;
    let mut ctr = None;
    let mut cpc = None;

    match adv_rep_res {
        Ok(data) => {
            if let Some(row) = data
                .get("list")
                .and_then(Value::as_array)
                .and_then(|l| l.first())
            {
                let m = row.get("metrics").unwrap_or(row);
                spend = json_f64(m, "spend");
                impressions = json_u64(m, "impressions");
                clicks = json_u64(m, "clicks");
                conversions = json_f64(m, "conversion");
                ctr = json_f64(m, "ctr");
                cpc = json_f64(m, "cpc");
            }
        }
        Err(e) => warnings.push(format!("Advertiser spend report unavailable: {e}")),
    }

    let mut camp_metrics_map: HashMap<String, Value> = HashMap::new();
    if let Ok(data) = camp_rep_res {
        if let Some(rows) = data.get("list").and_then(Value::as_array) {
            for r in rows {
                if let Some(cid) = r
                    .pointer("/dimensions/campaign_id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| json_str(r, "campaign_id"))
                {
                    if let Some(m) = r.get("metrics") {
                        camp_metrics_map.insert(cid, m.clone());
                    }
                }
            }
        }
    }

    let mut campaigns = Vec::new();
    match camp_res {
        Ok(data) => {
            if let Some(rows) = data.get("list").and_then(Value::as_array) {
                for c in rows {
                    let id = json_str(c, "campaign_id").unwrap_or_default();
                    let raw_status = json_str(c, "operation_status")
                        .or_else(|| json_str(c, "secondary_status"))
                        .unwrap_or_else(|| "UNKNOWN".to_string());
                    let normalized_status = match raw_status.to_ascii_uppercase().as_str() {
                        "ENABLE" => NormalizedStatus::Ready,
                        "DISABLE" => NormalizedStatus::Warning,
                        "DELETE" => NormalizedStatus::Failed,
                        other => NormalizedStatus::from_raw(other),
                    };
                    let m = camp_metrics_map.get(&id);
                    campaigns.push(CampaignRecord {
                        platform: PlatformId::Tiktok,
                        id: id.clone(),
                        name: json_str(c, "campaign_name").unwrap_or(id),
                        status: raw_status,
                        normalized_status,
                        objective: json_str(c, "objective_type"),
                        spend: m.and_then(|v| json_f64(v, "spend")),
                        currency: currency.clone(),
                        impressions: m.and_then(|v| json_u64(v, "impressions")),
                        clicks: m.and_then(|v| json_u64(v, "clicks")),
                        conversions: m.and_then(|v| json_f64(v, "conversion")),
                        ctr: m.and_then(|v| json_f64(v, "ctr")),
                        cpc: m.and_then(|v| json_f64(v, "cpc")),
                        extra: c
                            .as_object()
                            .map(|o| extra_from_object(o, &["campaign_id", "campaign_name"]))
                            .unwrap_or_default(),
                    });
                }
            }
        }
        Err(e) => warnings.push(format!("Campaign list unavailable: {e}")),
    }

    let mut audiences = Vec::new();
    let mut audiences_has_more = false;
    match aud_res {
        Ok(data) => {
            let total_page = data
                .pointer("/page_info/total_page")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            audiences_has_more = total_page > 1;
            if audiences_has_more {
                warnings.push(
                    "More than 100 TikTok custom audiences exist. Only the first page is shown."
                        .to_string(),
                );
            }

            let raw_list = data
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            // Fetch detail + audience_history in one batched call (up to 100 IDs)
            let ids: Vec<String> = raw_list
                .iter()
                .filter_map(|a| {
                    json_str(a, "custom_audience_id").or_else(|| json_str(a, "audience_id"))
                })
                .collect();

            let mut history_by_id: HashMap<String, Vec<OperationRecord>> = HashMap::new();
            let mut detail_error: Option<String> = None;

            if !ids.is_empty() {
                let ids_json = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string());
                match tiktok_get(
                    client,
                    cfg,
                    "/open_api/v1.3/dmp/custom_audience/get/",
                    &[
                        ("advertiser_id", cfg.advertiser_id.as_str()),
                        ("custom_audience_ids", ids_json.as_str()),
                    ],
                )
                .await
                {
                    Ok(detail_data) => {
                        if let Some(items) = detail_data.get("list").and_then(Value::as_array) {
                            for item in items {
                                let aid = json_str(item, "custom_audience_id")
                                    .or_else(|| json_str(item, "audience_id"))
                                    .unwrap_or_default();
                                if let Some(hist) =
                                    item.get("audience_history").and_then(Value::as_array)
                                {
                                    let ops = hist
                                        .iter()
                                        .enumerate()
                                        .map(|(idx, h)| {
                                            let action = json_str(h, "action")
                                                .unwrap_or_else(|| "update".to_string());
                                            let status = json_str(h, "status")
                                                .unwrap_or_else(|| "completed".to_string());
                                            OperationRecord {
                                                id: json_str(h, "id").unwrap_or_else(|| {
                                                    format!("{aid}-h{}", idx + 1)
                                                }),
                                                operation: action,
                                                normalized_status: NormalizedStatus::from_raw(
                                                    &status,
                                                ),
                                                status,
                                                created_at: parse_timestamp(h.get("create_time")),
                                                updated_at: None,
                                                count: None,
                                                error: None,
                                                extra: h
                                                    .as_object()
                                                    .map(|o| extra_from_object(o, &["action"]))
                                                    .unwrap_or_default(),
                                            }
                                        })
                                        .collect();
                                    history_by_id.insert(aid, ops);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let msg = format!("Could not fetch audience history: {e}");
                        warnings.push(msg.clone());
                        detail_error = Some(msg);
                    }
                }
            }

            for a in raw_list {
                let id = json_str(&a, "custom_audience_id")
                    .or_else(|| json_str(&a, "audience_id"))
                    .unwrap_or_default();
                let (status, normalized_status) = normalize_tiktok_audience_status(a.get("status"));
                let matched_users = json_i64(&a, "cover_num").map(format_compact_count);
                let ops = history_by_id.remove(&id).unwrap_or_default();

                audiences.push(AudienceRecord {
                    platform: PlatformId::Tiktok,
                    id: id.clone(),
                    name: json_str(&a, "name")
                        .or_else(|| json_str(&a, "custom_audience_name"))
                        .unwrap_or(id),
                    description: json_str(&a, "calculate_type")
                        .map(|ct| format!("Calculate type: {ct}")),
                    audience_type: json_str(&a, "audience_type"),
                    status,
                    normalized_status,
                    matched_users,
                    matched_ids: None,
                    uploaded_ids: None,
                    invalid_ids: None,
                    match_rate_pct: None,
                    revision: None,
                    created_at: parse_timestamp(a.get("create_time")),
                    updated_at: parse_timestamp(
                        a.get("modify_time").or_else(|| a.get("create_time")),
                    ),
                    operations: ops,
                    operations_has_more: false,
                    operations_error: detail_error.clone(),
                    extra: a
                        .as_object()
                        .map(|o| extra_from_object(o, &[]))
                        .unwrap_or_default(),
                });
            }
        }
        Err(e) => warnings.push(format!("Custom audiences unavailable: {e}")),
    }

    let active_campaigns = campaigns
        .iter()
        .filter(|c| c.normalized_status == NormalizedStatus::Ready)
        .count();

    let metrics = Some(AccountMetrics {
        spend,
        currency,
        balance,
        impressions,
        clicks,
        conversions,
        ctr,
        cpc,
        active_campaigns: Some(active_campaigns),
        total_campaigns: Some(campaigns.len()),
        account_status,
        date_range: Some(format!("{since}..{until}")),
        ..Default::default()
    });

    let status = if warnings.is_empty() {
        PlatformConnectionStatus::Ok
    } else {
        PlatformConnectionStatus::Partial
    };

    PlatformReport {
        platform: PlatformId::Tiktok,
        display_name: PlatformId::Tiktok.display_name().to_string(),
        status,
        base_url: endpoint_base,
        account_id: Some(cfg.advertiser_id.clone()),
        account_name,
        duration_ms: start.elapsed().as_millis() as u64,
        metrics,
        audiences,
        audiences_has_more,
        campaigns,
        warnings,
        error: None,
        policy_revision: None,
    }
}
