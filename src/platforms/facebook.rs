use crate::config::FacebookConfig;
use crate::error::ApiError;
use crate::http::send_json;
use crate::models::{
    format_count_range, AccountMetrics, AudienceRecord, CampaignRecord, NormalizedStatus,
    OperationRecord, PlatformConnectionStatus, PlatformId, PlatformReport,
};
use crate::platforms::{
    date_window_ymd, extra_from_object, json_f64, json_i64, json_str, json_u64, parse_timestamp,
};
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::time::Instant;

const AUDIENCE_FIELDS: &str = "id,name,description,subtype,delivery_status,operation_status,approximate_count_lower_bound,approximate_count_upper_bound,data_source,time_created,time_updated,time_content_updated";
const ACCOUNT_FIELDS: &str = "id,name,account_status,currency,amount_spent,balance,spend_cap";
const CAMPAIGN_FIELDS: &str = "id,name,status,effective_status,objective";
const MAX_AUDIENCE_PAGES: usize = 10;

fn minor_unit_divisor(currency: Option<&str>) -> f64 {
    match currency.unwrap_or("USD").to_ascii_uppercase().as_str() {
        "JPY" | "KRW" | "VND" | "TWD" | "CLP" | "PYG" => 1.0,
        _ => 100.0,
    }
}

fn map_account_status(code: Option<i64>) -> Option<String> {
    let s = match code? {
        1 => "ACTIVE",
        2 => "DISABLED",
        3 => "UNSETTLED",
        7 => "PENDING_RISK_REVIEW",
        8 => "PENDING_SETTLEMENT",
        9 => "IN_GRACE_PERIOD",
        100 => "PENDING_CLOSURE",
        101 => "CLOSED",
        201 => "ANY_ACTIVE",
        202 => "ANY_CLOSED",
        other => return Some(format!("STATUS_{other}")),
    };
    Some(s.to_string())
}

fn map_audience_status(aud: &Value) -> (String, NormalizedStatus) {
    let op_code = aud
        .pointer("/operation_status/code")
        .and_then(Value::as_i64);
    let op_desc = aud
        .pointer("/operation_status/description")
        .and_then(Value::as_str);
    let del_code = aud.pointer("/delivery_status/code").and_then(Value::as_i64);
    let del_desc = aud
        .pointer("/delivery_status/description")
        .and_then(Value::as_str);

    if op_code == Some(471) {
        return (
            op_desc.unwrap_or("Policy Flagged (471)").to_string(),
            NormalizedStatus::Failed,
        );
    }
    match del_code {
        Some(200) => {
            if matches!(op_code, Some(c) if c >= 400) {
                (
                    op_desc.unwrap_or("Warning").to_string(),
                    NormalizedStatus::Warning,
                )
            } else {
                ("ready".to_string(), NormalizedStatus::Ready)
            }
        }
        Some(300) => (
            del_desc.unwrap_or("too_small").to_string(),
            NormalizedStatus::Warning,
        ),
        Some(400..=599) => (
            del_desc.unwrap_or("unavailable").to_string(),
            NormalizedStatus::Failed,
        ),
        _ => {
            if let Some(d) = del_desc.or(op_desc) {
                (d.to_string(), NormalizedStatus::from_raw(d))
            } else {
                ("unknown".to_string(), NormalizedStatus::Neutral)
            }
        }
    }
}

async fn graph_get(
    client: &Client,
    cfg: &FacebookConfig,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value, ApiError> {
    let url = format!(
        "{}/{}/{}",
        cfg.base_url,
        cfg.api_version,
        path.trim_start_matches('/')
    );
    let req = client.get(&url).bearer_auth(&cfg.access_token).query(query);
    let val: Value = send_json(req, path).await?;
    if let Some(err) = val.get("error") {
        let code = json_str(err, "code").unwrap_or_else(|| "meta_error".to_string());
        let message = json_str(err, "message").unwrap_or_else(|| err.to_string());
        return Err(ApiError::PlatformApi {
            endpoint: path.to_string(),
            code,
            message,
        });
    }
    Ok(val)
}

async fn fetch_all_audiences(
    client: &Client,
    cfg: &FacebookConfig,
    aud_path: &str,
) -> Result<(Vec<Value>, bool), ApiError> {
    let mut items = Vec::new();
    let mut after: Option<String> = None;
    let mut has_more = false;

    for _ in 0..MAX_AUDIENCE_PAGES {
        let page = if let Some(cursor) = after.as_deref() {
            graph_get(
                client,
                cfg,
                aud_path,
                &[
                    ("fields", AUDIENCE_FIELDS),
                    ("limit", "100"),
                    ("after", cursor),
                ],
            )
            .await?
        } else {
            graph_get(
                client,
                cfg,
                aud_path,
                &[("fields", AUDIENCE_FIELDS), ("limit", "100")],
            )
            .await?
        };

        if let Some(arr) = page.get("data").and_then(Value::as_array) {
            items.extend(arr.iter().cloned());
        }

        let next_cursor = page
            .pointer("/paging/cursors/after")
            .and_then(Value::as_str)
            .map(String::from);
        let has_next = page.pointer("/paging/next").is_some();

        if has_next && next_cursor.is_some() {
            has_more = true;
            after = next_cursor;
        } else {
            has_more = false;
            break;
        }
    }

    Ok((items, has_more))
}

async fn fetch_sessions(
    client: &Client,
    cfg: &FacebookConfig,
    audience_id: &str,
    subtype: Option<&str>,
) -> (Vec<OperationRecord>, bool, Option<String>) {
    if let Some(st) = subtype {
        let u = st.to_ascii_uppercase();
        if u != "CUSTOM" && u != "DATA_FILE" && u != "OFFLINE_CONVERSION" {
            return (Vec::new(), false, None);
        }
    }

    let path = format!("{audience_id}/sessions");
    match graph_get(client, cfg, &path, &[]).await {
        Ok(res) => {
            let has_more = res.pointer("/paging/next").is_some();
            let ops = res
                .get("data")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|s| {
                            let progress = json_str(s, "progress")
                                .or_else(|| json_str(s, "stage"))
                                .unwrap_or_else(|| "completed".to_string());
                            let normalized_status = NormalizedStatus::from_raw(&progress);
                            let extra = s
                                .as_object()
                                .map(|m| extra_from_object(m, &["session_id"]))
                                .unwrap_or_default();
                            OperationRecord {
                                id: json_str(s, "session_id")
                                    .or_else(|| json_str(s, "id"))
                                    .unwrap_or_default(),
                                operation: json_str(s, "stage")
                                    .unwrap_or_else(|| "upload_session".to_string()),
                                status: progress,
                                normalized_status,
                                created_at: parse_timestamp(s.get("start_time")),
                                updated_at: parse_timestamp(s.get("end_time")),
                                count: json_u64(s, "num_received"),
                                error: json_u64(s, "num_invalid_entries").and_then(|inv| {
                                    if inv > 0 {
                                        Some(format!("{inv} invalid entries"))
                                    } else {
                                        None
                                    }
                                }),
                                extra,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            (ops, has_more, None)
        }
        Err(e) => (Vec::new(), false, Some(e.to_string())),
    }
}

pub async fn collect(
    client: &Client,
    cfg: &FacebookConfig,
    lookback_days: u32,
    concurrency: usize,
) -> PlatformReport {
    let start = Instant::now();
    let (since, until) = date_window_ymd(lookback_days);
    let time_range = format!(r#"{{"since":"{since}","until":"{until}"}}"#);
    let endpoint_base = format!("{}/{}", cfg.base_url, cfg.api_version);

    if !cfg.options.enable_audiences && !cfg.options.enable_campaigns {
        return PlatformReport {
            platform: PlatformId::Facebook,
            display_name: PlatformId::Facebook.display_name().to_string(),
            status: PlatformConnectionStatus::Ok,
            base_url: endpoint_base,
            account_id: Some(cfg.ad_account_ids.join(", ")),
            account_name: None,
            duration_ms: 0,
            metrics: None,
            audiences: Vec::new(),
            audiences_has_more: false,
            audiences_total_before_filter: None,
            campaigns: Vec::new(),
            warnings: vec![
                "Both audiences and campaigns disabled via environment configuration.".to_string(),
            ],
            error: None,
            policy_revision: None,
        };
    }

    let mut all_audiences = Vec::new();
    let mut total_audiences_before_filter = 0usize;
    let mut all_campaigns = Vec::new();
    let mut warnings = Vec::new();
    let mut account_names = Vec::new();
    let mut audiences_has_more = false;
    let mut had_any_success = false;
    let mut fatal_errors = Vec::new();

    let mut total_spend = 0.0_f64;
    let mut has_spend = false;
    let mut total_balance = 0.0_f64;
    let mut has_balance = false;
    let mut total_spend_cap = 0.0_f64;
    let mut has_spend_cap = false;
    let mut total_impressions = 0_u64;
    let mut total_clicks = 0_u64;
    let mut total_reach = 0_u64;
    let mut currency: Option<String> = None;
    let mut account_status: Option<String> = None;

    for acct_id in &cfg.ad_account_ids {
        let act_node = format!("act_{acct_id}");
        let insights_path = format!("{act_node}/insights");
        let aud_path = format!("{act_node}/customaudiences");
        let camp_path = format!("{act_node}/campaigns");
        let q_acct_ins = [
            ("level", "account"),
            ("fields", "spend,impressions,clicks,cpc,ctr,reach"),
            ("time_range", time_range.as_str()),
        ];
        let q_camp_ins = [
            ("level", "campaign"),
            (
                "fields",
                "campaign_id,campaign_name,spend,impressions,clicks,cpc,ctr",
            ),
            ("time_range", time_range.as_str()),
            ("limit", "100"),
        ];

        let (acct_res, insights_res, aud_res, camp_res, camp_ins_res) = tokio::join!(
            async {
                if cfg.options.enable_campaigns {
                    Some(graph_get(client, cfg, &act_node, &[("fields", ACCOUNT_FIELDS)]).await)
                } else {
                    None
                }
            },
            async {
                if cfg.options.enable_campaigns {
                    Some(graph_get(client, cfg, &insights_path, &q_acct_ins).await)
                } else {
                    None
                }
            },
            async {
                if cfg.options.enable_audiences {
                    Some(fetch_all_audiences(client, cfg, &aud_path).await)
                } else {
                    None
                }
            },
            async {
                if cfg.options.enable_campaigns {
                    Some(
                        graph_get(
                            client,
                            cfg,
                            &camp_path,
                            &[("fields", CAMPAIGN_FIELDS), ("limit", "100")],
                        )
                        .await,
                    )
                } else {
                    None
                }
            },
            async {
                if cfg.options.enable_campaigns {
                    Some(graph_get(client, cfg, &insights_path, &q_camp_ins).await)
                } else {
                    None
                }
            },
        );

        if let Some(res) = acct_res {
            match res {
                Ok(acct) => {
                    had_any_success = true;
                    if let Some(name) = json_str(&acct, "name") {
                        account_names.push(name);
                    }
                    let acct_curr = json_str(&acct, "currency");
                    let div = minor_unit_divisor(acct_curr.as_deref());
                    if currency.is_none() {
                        currency = acct_curr;
                    }
                    if account_status.is_none() {
                        account_status = map_account_status(json_i64(&acct, "account_status"));
                    }
                    if let Some(b) = json_f64(&acct, "balance") {
                        total_balance += b / div;
                        has_balance = true;
                    }
                    if let Some(cap) = json_f64(&acct, "spend_cap") {
                        if cap > 0.0 {
                            total_spend_cap += cap / div;
                            has_spend_cap = true;
                        }
                    }
                }
                Err(e) => fatal_errors.push(format!("act_{acct_id} account info: {e}")),
            }
        }

        if let Some(res) = insights_res {
            match res {
                Ok(ins) => {
                    had_any_success = true;
                    if let Some(row) = ins
                        .get("data")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                    {
                        if let Some(sp) = json_f64(row, "spend") {
                            total_spend += sp;
                            has_spend = true;
                        }
                        total_impressions += json_u64(row, "impressions").unwrap_or(0);
                        total_clicks += json_u64(row, "clicks").unwrap_or(0);
                        total_reach += json_u64(row, "reach").unwrap_or(0);
                    }
                }
                Err(e) => warnings.push(format!("act_{acct_id} insights unavailable: {e}")),
            }
        }

        let mut camp_metrics_map = std::collections::HashMap::new();
        if let Some(Ok(cins)) = camp_ins_res {
            if let Some(rows) = cins.get("data").and_then(Value::as_array) {
                for row in rows {
                    if let Some(cid) = json_str(row, "campaign_id") {
                        camp_metrics_map.insert(cid, row.clone());
                    }
                }
            }
        }

        if let Some(res) = camp_res {
            match res {
                Ok(camps) => {
                    had_any_success = true;
                    if let Some(arr) = camps.get("data").and_then(Value::as_array) {
                        for c in arr {
                            let id = json_str(c, "id").unwrap_or_default();
                            let status = json_str(c, "effective_status")
                                .or_else(|| json_str(c, "status"))
                                .unwrap_or_else(|| "UNKNOWN".to_string());
                            let normalized_status = NormalizedStatus::from_raw(&status);
                            let m = camp_metrics_map.get(&id);
                            all_campaigns.push(CampaignRecord {
                                platform: PlatformId::Facebook,
                                id: id.clone(),
                                name: json_str(c, "name").unwrap_or(id),
                                status,
                                normalized_status,
                                objective: json_str(c, "objective"),
                                spend: m.and_then(|v| json_f64(v, "spend")),
                                currency: currency.clone(),
                                impressions: m.and_then(|v| json_u64(v, "impressions")),
                                clicks: m.and_then(|v| json_u64(v, "clicks")),
                                conversions: None,
                                ctr: m.and_then(|v| json_f64(v, "ctr")),
                                cpc: m.and_then(|v| json_f64(v, "cpc")),
                                extra: c
                                    .as_object()
                                    .map(|o| extra_from_object(o, &["id", "name", "status"]))
                                    .unwrap_or_default(),
                            });
                        }
                    }
                }
                Err(e) => warnings.push(format!("act_{acct_id} campaigns unavailable: {e}")),
            }
        }

        if let Some(res) = aud_res {
            match res {
                Ok((raw_list, more)) => {
                    had_any_success = true;
                    if more {
                        audiences_has_more = true;
                    }
                    total_audiences_before_filter += raw_list.len();

                    // Filter BEFORE fetching /sessions so we don't make unnecessary sub-requests!
                    let filtered_list: Vec<Value> = raw_list
                        .into_iter()
                        .filter(|a| {
                            let name = json_str(a, "name").unwrap_or_default();
                            let desc = json_str(a, "description");
                            cfg.options.matches_audience(&name, desc.as_deref())
                        })
                        .collect();

                    let session_targets: Vec<(usize, String, Option<String>)> = filtered_list
                        .iter()
                        .enumerate()
                        .map(|(idx, a)| {
                            (
                                idx,
                                json_str(a, "id").unwrap_or_default(),
                                json_str(a, "subtype"),
                            )
                        })
                        .collect();

                    let session_results: Vec<(usize, Vec<OperationRecord>, bool, Option<String>)> =
                        stream::iter(session_targets)
                            .map(|(idx, id, subtype)| async move {
                                let (ops, more, err) =
                                    fetch_sessions(client, cfg, &id, subtype.as_deref()).await;
                                (idx, ops, more, err)
                            })
                            .buffer_unordered(concurrency)
                            .collect()
                            .await;

                    let mut sess_by_idx = vec![(Vec::new(), false, None); filtered_list.len()];
                    for (idx, ops, more, err) in session_results {
                        sess_by_idx[idx] = (ops, more, err);
                    }

                    for (a, (ops, ops_more, ops_err)) in filtered_list.into_iter().zip(sess_by_idx)
                    {
                        let (status, normalized_status) = map_audience_status(&a);
                        let lower = json_i64(&a, "approximate_count_lower_bound");
                        let upper = json_i64(&a, "approximate_count_upper_bound");
                        let matched_users = format_count_range(lower, upper);
                        let mut extra = a
                            .as_object()
                            .map(|o| extra_from_object(o, &[]))
                            .unwrap_or_default();
                        extra.insert("ad_account_id".to_string(), Value::String(acct_id.clone()));

                        all_audiences.push(AudienceRecord {
                            platform: PlatformId::Facebook,
                            id: json_str(&a, "id").unwrap_or_default(),
                            name: json_str(&a, "name").unwrap_or_else(|| "(unnamed)".to_string()),
                            description: json_str(&a, "description"),
                            audience_type: json_str(&a, "subtype"),
                            status,
                            normalized_status,
                            matched_users,
                            matched_ids: None,
                            uploaded_ids: None,
                            invalid_ids: None,
                            match_rate_pct: None,
                            revision: None,
                            created_at: parse_timestamp(a.get("time_created")),
                            updated_at: parse_timestamp(
                                a.get("time_content_updated")
                                    .or_else(|| a.get("time_updated")),
                            ),
                            operations: ops,
                            operations_has_more: ops_more,
                            operations_error: ops_err,
                            extra,
                        });
                    }
                }
                Err(e) => fatal_errors.push(format!("act_{acct_id} customaudiences: {e}")),
            }
        }
    }

    if !had_any_success && !fatal_errors.is_empty() {
        return PlatformReport::failed(
            PlatformId::Facebook,
            endpoint_base,
            start.elapsed().as_millis() as u64,
            fatal_errors.join("; "),
        );
    }

    if audiences_has_more {
        warnings.push(format!(
            "More than {} Meta custom audiences exist. Showing first {MAX_AUDIENCE_PAGES} pages per account.",
            MAX_AUDIENCE_PAGES * 100
        ));
    }
    for err in &fatal_errors {
        warnings.push(err.clone());
    }

    let metrics = if cfg.options.enable_campaigns {
        let active_campaigns = all_campaigns
            .iter()
            .filter(|c| c.normalized_status == NormalizedStatus::Ready)
            .count();

        let ctr = if total_impressions > 0 {
            Some((total_clicks as f64 / total_impressions as f64) * 100.0)
        } else {
            None
        };
        let cpc = if has_spend && total_clicks > 0 {
            Some(total_spend / total_clicks as f64)
        } else {
            None
        };

        Some(AccountMetrics {
            spend: if has_spend { Some(total_spend) } else { None },
            currency,
            balance: if has_balance {
                Some(total_balance)
            } else {
                None
            },
            spend_cap: if has_spend_cap {
                Some(total_spend_cap)
            } else {
                None
            },
            impressions: Some(total_impressions),
            clicks: Some(total_clicks),
            conversions: None,
            ctr,
            cpc,
            reach: if total_reach > 0 {
                Some(total_reach)
            } else {
                None
            },
            active_campaigns: Some(active_campaigns),
            total_campaigns: Some(all_campaigns.len()),
            account_status,
            date_range: Some(format!("{since}..{until}")),
            ..Default::default()
        })
    } else {
        None
    };

    let status = if !fatal_errors.is_empty() || !warnings.is_empty() {
        PlatformConnectionStatus::Partial
    } else {
        PlatformConnectionStatus::Ok
    };

    PlatformReport {
        platform: PlatformId::Facebook,
        display_name: PlatformId::Facebook.display_name().to_string(),
        status,
        base_url: endpoint_base,
        account_id: Some(cfg.ad_account_ids.join(", ")),
        account_name: if account_names.is_empty() {
            None
        } else {
            Some(account_names.join(", "))
        },
        duration_ms: start.elapsed().as_millis() as u64,
        metrics,
        audiences: all_audiences,
        audiences_has_more,
        audiences_total_before_filter: if cfg.options.enable_audiences
            && cfg.options.has_audience_filter()
        {
            Some(total_audiences_before_filter)
        } else {
            None
        },
        campaigns: all_campaigns,
        warnings,
        error: None,
        policy_revision: None,
    }
}
