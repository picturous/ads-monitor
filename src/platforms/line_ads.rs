use crate::config::LineAdsConfig;
use crate::error::ApiError;
use crate::http::send_json;
use crate::models::{
    format_compact_count, AccountMetrics, AudienceRecord, CampaignRecord, NormalizedStatus,
    PlatformConnectionStatus, PlatformId, PlatformReport,
};
use crate::platforms::{
    date_window_ymd, extra_from_object, json_f64, json_i64, json_str, json_u64, parse_timestamp,
};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Instant;

type HmacSha256 = Hmac<Sha256>;
const MAX_AUDIENCE_PAGES: usize = 20;

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Generate a LINE Ads API v3 JWS (HMAC-SHA256) Authorization token and GMT Date header.
///
/// Official LINE Ads API v3 spec:
/// - Header: `base64url({"alg":"HS256","kid":"<access_key>","typ":"text/plain"})`
/// - Payload: `base64url("<sha256_hex(body)>\n<content_type>\n<yyyyMMdd>\n<canonical_uri_path_only>")`
///   NOTE: Query parameters MUST be excluded from `canonical_uri`.
/// - Signature: `base64url(HMAC-SHA256(secret_key, "<header>.<payload>"))`
pub fn sign_line_ads_request(
    access_key: &str,
    secret_key: &str,
    request_path: &str,
    content_type: &str,
    body: &[u8],
    now_utc: DateTime<Utc>,
) -> Result<(String, String), ApiError> {
    // Strip any query string from the canonical URI per LINE Ads API v3 specification.
    let canonical_uri = request_path.split('?').next().unwrap_or(request_path);

    let header_json = serde_json::json!({
        "alg": "HS256",
        "kid": access_key,
        "typ": "text/plain"
    });
    let header_b64 = URL_SAFE.encode(header_json.to_string().as_bytes());

    let body_hash = sha256_hex(body);
    let payload_date = now_utc.format("%Y%m%d").to_string();
    let payload_str = format!("{body_hash}\n{content_type}\n{payload_date}\n{canonical_uri}");
    let payload_b64 = URL_SAFE.encode(payload_str.as_bytes());

    let signing_input = format!("{header_b64}.{payload_b64}");
    let mut mac = HmacSha256::new_from_slice(secret_key.as_bytes())
        .map_err(|e| ApiError::Auth(format!("Invalid LINE Ads secret key: {e}")))?;
    mac.update(signing_input.as_bytes());
    let sig_b64 = URL_SAFE.encode(mac.finalize().into_bytes());

    let token = format!("{signing_input}.{sig_b64}");
    let date_header = now_utc.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    Ok((token, date_header))
}

async fn line_ads_get(
    client: &Client,
    cfg: &LineAdsConfig,
    path_only: &str,
    query: &[(&str, &str)],
) -> Result<Value, ApiError> {
    let (token, date_header) = sign_line_ads_request(
        &cfg.access_key,
        &cfg.secret_key,
        path_only,
        "",
        b"",
        Utc::now(),
    )?;
    let url = format!("{}{}", cfg.base_url, path_only);
    let req = client
        .get(&url)
        .header("Date", date_header)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .query(query);
    send_json(req, path_only).await
}

fn extract_items(v: &Value) -> Vec<Value> {
    if let Some(arr) = v.as_array() {
        return arr.clone();
    }
    v.get("audienceGroups")
        .or_else(|| v.get("datas"))
        .or_else(|| v.get("data"))
        .or_else(|| v.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

async fn fetch_all_custom_audiences(
    client: &Client,
    cfg: &LineAdsConfig,
    aud_path: &str,
) -> Result<(Vec<Value>, bool), ApiError> {
    let mut items = Vec::new();
    let mut has_more = false;

    for page in 1..=MAX_AUDIENCE_PAGES {
        let page_str = page.to_string();
        let res = line_ads_get(
            client,
            cfg,
            aud_path,
            &[("page", page_str.as_str()), ("size", "100")],
        )
        .await?;

        let page_items = extract_items(&res);
        items.extend(page_items);

        let has_next = res
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if has_next {
            has_more = true;
        } else {
            has_more = false;
            break;
        }
    }

    Ok((items, has_more))
}

pub async fn collect(client: &Client, cfg: &LineAdsConfig, lookback_days: u32) -> PlatformReport {
    let start = Instant::now();
    let (since, until) = date_window_ymd(lookback_days);
    let endpoint_base = format!("{}/api/v3", cfg.base_url);

    if !cfg.options.enable_audiences && !cfg.options.enable_campaigns {
        return PlatformReport {
            platform: PlatformId::LineAds,
            display_name: PlatformId::LineAds.display_name().to_string(),
            status: PlatformConnectionStatus::Ok,
            base_url: endpoint_base,
            account_id: Some(cfg.account_id.clone()),
            account_name: Some(format!("LINE Ads ({})", cfg.account_id)),
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

    let acct_enc = urlencoding::encode(&cfg.account_id);
    // LINE Ads API v3 uses /custom-audiences (hyphenated)
    let aud_path = format!("/api/v3/adaccounts/{acct_enc}/custom-audiences");
    let camp_path = format!("/api/v3/adaccounts/{acct_enc}/campaigns");
    let report_path = format!("/api/v3/adaccounts/{acct_enc}/reports/online/campaign");
    let q_report = [("since", since.as_str()), ("until", until.as_str())];

    let (aud_res, camp_res, rep_res) = tokio::join!(
        async {
            if cfg.options.enable_audiences {
                Some(fetch_all_custom_audiences(client, cfg, &aud_path).await)
            } else {
                None
            }
        },
        async {
            if cfg.options.enable_campaigns {
                Some(line_ads_get(client, cfg, &camp_path, &[]).await)
            } else {
                None
            }
        },
        async {
            if cfg.options.enable_campaigns {
                Some(line_ads_get(client, cfg, &report_path, &q_report).await)
            } else {
                None
            }
        },
    );

    let any_ok = matches!(aud_res, Some(Ok(_)))
        || matches!(camp_res, Some(Ok(_)))
        || matches!(rep_res, Some(Ok(_)));

    if !any_ok {
        let msg = aud_res
            .and_then(Result::err)
            .or_else(|| rep_res.and_then(Result::err))
            .or_else(|| camp_res.and_then(Result::err))
            .map(|e| e.to_string())
            .unwrap_or_else(|| "LINE Ads API request failed".to_string());
        return PlatformReport::failed(
            PlatformId::LineAds,
            endpoint_base,
            start.elapsed().as_millis() as u64,
            msg,
        );
    }

    let mut warnings = Vec::new();
    let mut total_spend = 0.0_f64;
    let mut has_spend = false;
    let mut total_impressions = 0_u64;
    let mut total_clicks = 0_u64;
    let mut total_conversions = 0.0_f64;
    let mut currency = None;

    let mut stats_by_campaign: HashMap<String, Value> = HashMap::new();
    if let Some(res) = rep_res {
        match res {
            Ok(data) => {
                for row in extract_items(&data) {
                    let stats = row.get("statistics").unwrap_or(&row);
                    let camp = row.get("campaign");
                    if currency.is_none() {
                        currency = json_str(stats, "currency")
                            .or_else(|| camp.and_then(|c| json_str(c, "currency")));
                    }
                    if let Some(cost) = json_f64(stats, "cost") {
                        total_spend += cost;
                        has_spend = true;
                    }
                    total_impressions += json_u64(stats, "imp")
                        .or_else(|| json_u64(stats, "impressions"))
                        .unwrap_or(0);
                    total_clicks += json_u64(stats, "click")
                        .or_else(|| json_u64(stats, "clicks"))
                        .unwrap_or(0);
                    total_conversions += json_f64(stats, "cv")
                        .or_else(|| json_f64(stats, "conversions"))
                        .unwrap_or(0.0);

                    if let Some(cid) = camp
                        .and_then(|c| json_str(c, "id"))
                        .or_else(|| json_str(&row, "campaignId"))
                    {
                        stats_by_campaign.insert(cid, stats.clone());
                    }
                }
            }
            Err(e) => warnings.push(format!("Campaign online report unavailable: {e}")),
        }
    }

    let mut campaigns = Vec::new();
    if let Some(res) = camp_res {
        match res {
            Ok(data) => {
                for c in extract_items(&data) {
                    let id = json_str(&c, "id").unwrap_or_default();
                    let status = json_str(&c, "configuredStatus")
                        .or_else(|| json_str(&c, "deliveryStatus"))
                        .or_else(|| json_str(&c, "status"))
                        .unwrap_or_else(|| "UNKNOWN".to_string());
                    let normalized_status = NormalizedStatus::from_raw(&status);
                    let st = stats_by_campaign.get(&id);
                    let spend = st.and_then(|s| json_f64(s, "cost"));
                    let impressions =
                        st.and_then(|s| json_u64(s, "imp").or_else(|| json_u64(s, "impressions")));
                    let clicks =
                        st.and_then(|s| json_u64(s, "click").or_else(|| json_u64(s, "clicks")));
                    let conversions =
                        st.and_then(|s| json_f64(s, "cv").or_else(|| json_f64(s, "conversions")));
                    let ctr = st.and_then(|s| json_f64(s, "ctr"));
                    let cpc = st.and_then(|s| json_f64(s, "cpc"));

                    campaigns.push(CampaignRecord {
                        platform: PlatformId::LineAds,
                        id: id.clone(),
                        name: json_str(&c, "name").unwrap_or(id),
                        status,
                        normalized_status,
                        objective: json_str(&c, "campaignObjective"),
                        spend,
                        currency: currency.clone(),
                        impressions,
                        clicks,
                        conversions,
                        ctr,
                        cpc,
                        extra: c
                            .as_object()
                            .map(|o| extra_from_object(o, &["id", "name"]))
                            .unwrap_or_default(),
                    });
                }
            }
            Err(e) => warnings.push(format!("Campaign list unavailable: {e}")),
        }
    }

    let mut audiences = Vec::new();
    let mut audiences_has_more = false;
    let mut total_before_filter = 0usize;
    if let Some(res) = aud_res {
        match res {
            Ok((raw_items, has_more)) => {
                audiences_has_more = has_more;
                if audiences_has_more {
                    warnings.push(format!(
                        "More than {} LINE Ads custom audiences exist. Showing first {MAX_AUDIENCE_PAGES} pages.",
                        MAX_AUDIENCE_PAGES * 100
                    ));
                }
                total_before_filter = raw_items.len();

                for a in raw_items {
                    let id = json_str(&a, "id")
                        .or_else(|| json_str(&a, "audienceId"))
                        .unwrap_or_default();
                    let name = json_str(&a, "name").unwrap_or_else(|| id.clone());
                    let description = json_str(&a, "description")
                        .or_else(|| json_str(&a, "failedType").map(|f| format!("Failed: {f}")));
                    if !cfg.options.matches_audience(&name, description.as_deref()) {
                        continue;
                    }

                    let status = json_str(&a, "status")
                        .or_else(|| json_str(&a, "availabilityStatus"))
                        .unwrap_or_else(|| "UNKNOWN".to_string());
                    let normalized_status = NormalizedStatus::from_raw(&status);
                    let matched_users = json_i64(&a, "estimatedAudienceSize")
                        .or_else(|| json_i64(&a, "audienceSize"))
                        .or_else(|| json_i64(&a, "size"))
                        .map(format_compact_count);

                    audiences.push(AudienceRecord {
                        platform: PlatformId::LineAds,
                        id,
                        name,
                        description,
                        audience_type: json_str(&a, "functionType")
                            .or_else(|| json_str(&a, "type"))
                            .or_else(|| json_str(&a, "sourceType")),
                        status,
                        normalized_status,
                        matched_users,
                        matched_ids: None,
                        uploaded_ids: None,
                        invalid_ids: None,
                        match_rate_pct: None,
                        revision: None,
                        created_at: parse_timestamp(
                            a.get("created")
                                .or_else(|| a.get("createdDate"))
                                .or_else(|| a.get("createdAt")),
                        ),
                        updated_at: parse_timestamp(
                            a.get("modified")
                                .or_else(|| a.get("updated"))
                                .or_else(|| a.get("updatedDate"))
                                .or_else(|| a.get("updatedAt"))
                                .or_else(|| a.get("created")),
                        ),
                        operations: Vec::new(),
                        operations_has_more: false,
                        operations_error: None,
                        extra: a
                            .as_object()
                            .map(|o| extra_from_object(o, &[]))
                            .unwrap_or_default(),
                    });
                }
            }
            Err(e) => warnings.push(format!("Custom audiences list unavailable: {e}")),
        }
    }

    let metrics = if cfg.options.enable_campaigns {
        let active_campaigns = campaigns
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
            impressions: Some(total_impressions),
            clicks: Some(total_clicks),
            conversions: if total_conversions > 0.0 {
                Some(total_conversions)
            } else {
                None
            },
            ctr,
            cpc,
            active_campaigns: Some(active_campaigns),
            total_campaigns: Some(campaigns.len()),
            date_range: Some(format!("{since}..{until}")),
            ..Default::default()
        })
    } else {
        None
    };

    let status = if warnings.is_empty() {
        PlatformConnectionStatus::Ok
    } else {
        PlatformConnectionStatus::Partial
    };

    PlatformReport {
        platform: PlatformId::LineAds,
        display_name: PlatformId::LineAds.display_name().to_string(),
        status,
        base_url: endpoint_base,
        account_id: Some(cfg.account_id.clone()),
        account_name: Some(format!("LINE Ads ({})", cfg.account_id)),
        duration_ms: start.elapsed().as_millis() as u64,
        metrics,
        audiences,
        audiences_has_more,
        audiences_total_before_filter: if cfg.options.enable_audiences
            && cfg.options.has_audience_filter()
        {
            Some(total_before_filter)
        } else {
            None
        },
        campaigns,
        warnings,
        error: None,
        policy_revision: None,
    }
}
