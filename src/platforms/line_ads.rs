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
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Instant;

type HmacSha256 = Hmac<Sha256>;

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
/// Format:
/// - Header: `base64url({"alg":"HS256","kid":"<access_key>","typ":"text/plain"})`
/// - Payload: `base64url("<sha256_hex(body)>\n<content_type>\n<yyyyMMdd>\n<canonical_path_and_query>")`
/// - Signature: `base64url(HMAC-SHA256(secret_key, "<header>.<payload>"))`
pub fn sign_line_ads_request(
    access_key: &str,
    secret_key: &str,
    canonical_path: &str,
    content_type: &str,
    body: &[u8],
    now_utc: DateTime<Utc>,
) -> Result<(String, String), ApiError> {
    let header_json = serde_json::json!({
        "alg": "HS256",
        "kid": access_key,
        "typ": "text/plain"
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(header_json.to_string().as_bytes());

    let body_hash = sha256_hex(body);
    let payload_date = now_utc.format("%Y%m%d").to_string();
    let payload_str = format!("{body_hash}\n{content_type}\n{payload_date}\n{canonical_path}");
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_str.as_bytes());

    let signing_input = format!("{header_b64}.{payload_b64}");
    let mut mac = HmacSha256::new_from_slice(secret_key.as_bytes())
        .map_err(|e| ApiError::Auth(format!("Invalid LINE Ads secret key: {e}")))?;
    mac.update(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    let token = format!("{signing_input}.{sig_b64}");
    let date_header = now_utc.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    Ok((token, date_header))
}

async fn line_ads_get(
    client: &Client,
    cfg: &LineAdsConfig,
    canonical_path: &str,
) -> Result<Value, ApiError> {
    let (token, date_header) = sign_line_ads_request(
        &cfg.access_key,
        &cfg.secret_key,
        canonical_path,
        "",
        b"",
        Utc::now(),
    )?;
    let url = format!("{}{}", cfg.base_url, canonical_path);
    let req = client
        .get(&url)
        .header("Date", date_header)
        .bearer_auth(token)
        .header("Accept", "application/json");
    send_json(req, canonical_path).await
}

fn extract_items(v: &Value) -> Vec<Value> {
    v.get("datas")
        .or_else(|| v.get("data"))
        .or_else(|| v.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

pub async fn collect(client: &Client, cfg: &LineAdsConfig, lookback_days: u32) -> PlatformReport {
    let start = Instant::now();
    let (since, until) = date_window_ymd(lookback_days);
    let endpoint_base = format!("{}/api/v3", cfg.base_url);
    let acct_enc = urlencoding::encode(&cfg.account_id);

    let aud_path = format!("/api/v3/adaccounts/{acct_enc}/audiences");
    let camp_path = format!("/api/v3/adaccounts/{acct_enc}/campaigns");
    let report_path = format!(
        "/api/v3/adaccounts/{acct_enc}/reports/online/campaign?since={since}&until={until}"
    );

    let (aud_res, camp_res, rep_res) = tokio::join!(
        line_ads_get(client, cfg, &aud_path),
        line_ads_get(client, cfg, &camp_path),
        line_ads_get(client, cfg, &report_path),
    );

    if aud_res.is_err() && camp_res.is_err() && rep_res.is_err() {
        let msg = aud_res
            .err()
            .or_else(|| rep_res.err())
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

    // Campaign stats from online report keyed by campaign ID
    let mut stats_by_campaign: HashMap<String, Value> = HashMap::new();
    match rep_res {
        Ok(res) => {
            for row in extract_items(&res) {
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

    let mut campaigns = Vec::new();
    match camp_res {
        Ok(res) => {
            for c in extract_items(&res) {
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

    let mut audiences = Vec::new();
    match aud_res {
        Ok(res) => {
            for a in extract_items(&res) {
                let id = json_str(&a, "id")
                    .or_else(|| json_str(&a, "audienceId"))
                    .unwrap_or_default();
                let status = json_str(&a, "status")
                    .or_else(|| json_str(&a, "availabilityStatus"))
                    .unwrap_or_else(|| "UNKNOWN".to_string());
                let normalized_status = NormalizedStatus::from_raw(&status);
                let matched_users = json_i64(&a, "audienceSize")
                    .or_else(|| json_i64(&a, "size"))
                    .map(format_compact_count);

                audiences.push(AudienceRecord {
                    platform: PlatformId::LineAds,
                    id: id.clone(),
                    name: json_str(&a, "name").unwrap_or(id),
                    description: json_str(&a, "description"),
                    audience_type: json_str(&a, "type").or_else(|| json_str(&a, "sourceType")),
                    status,
                    normalized_status,
                    matched_users,
                    matched_ids: None,
                    uploaded_ids: None,
                    invalid_ids: None,
                    match_rate_pct: None,
                    revision: None,
                    created_at: parse_timestamp(
                        a.get("createdDate").or_else(|| a.get("createdAt")),
                    ),
                    updated_at: parse_timestamp(
                        a.get("updatedDate").or_else(|| a.get("updatedAt")),
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
        Err(e) => warnings.push(format!("Audiences list unavailable: {e}")),
    }

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

    let metrics = Some(AccountMetrics {
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
    });

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
        audiences_has_more: false,
        campaigns,
        warnings,
        error: None,
        policy_revision: None,
    }
}
