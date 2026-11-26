use crate::config::GoogleAdsConfig;
use crate::error::ApiError;
use crate::http::send_json;
use crate::models::{
    format_compact_count, AccountMetrics, AudienceRecord, CampaignRecord, NormalizedStatus,
    OperationRecord, PlatformConnectionStatus, PlatformId, PlatformReport,
};
use crate::platforms::{
    date_window_ymd, extra_from_object, json_f64, json_i64, json_str, json_u64,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;

async fn resolve_access_token(client: &Client, cfg: &GoogleAdsConfig) -> Result<String, ApiError> {
    if let Some(tok) = &cfg.access_token {
        return Ok(tok.clone());
    }
    let client_id = cfg
        .client_id
        .as_deref()
        .ok_or_else(|| ApiError::Auth("Missing GOOGLE_ADS_CLIENT_ID".to_string()))?;
    let client_secret = cfg
        .client_secret
        .as_deref()
        .ok_or_else(|| ApiError::Auth("Missing GOOGLE_ADS_CLIENT_SECRET".to_string()))?;
    let refresh_token = cfg
        .refresh_token
        .as_deref()
        .ok_or_else(|| ApiError::Auth("Missing GOOGLE_ADS_REFRESH_TOKEN".to_string()))?;

    let body = format!(
        "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
        urlencoding::encode(client_id),
        urlencoding::encode(client_secret),
        urlencoding::encode(refresh_token),
    );
    let req = client
        .post(&cfg.oauth_token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body);
    let resp: Value = send_json(req, "google_oauth2_token").await?;
    json_str(&resp, "access_token")
        .ok_or_else(|| ApiError::Auth("OAuth2 response did not contain access_token".to_string()))
}

async fn search_stream(
    client: &Client,
    cfg: &GoogleAdsConfig,
    access_token: &str,
    query: &str,
) -> Result<Vec<Value>, ApiError> {
    let path = format!(
        "/{}/customers/{}/googleAds:searchStream",
        cfg.api_version, cfg.customer_id
    );
    let url = format!("{}{}", cfg.base_url, path);
    let mut req = client
        .post(&url)
        .bearer_auth(access_token)
        .header("developer-token", &cfg.developer_token)
        .json(&json!({ "query": query }));
    if let Some(login_cid) = &cfg.login_customer_id {
        req = req.header("login-customer-id", login_cid);
    }

    let val: Value = send_json(req, &path).await?;
    let mut rows = Vec::new();
    match val {
        Value::Array(batches) => {
            for batch in batches {
                if let Some(arr) = batch.get("results").and_then(Value::as_array) {
                    rows.extend(arr.iter().cloned());
                }
            }
        }
        Value::Object(map) => {
            if let Some(arr) = map.get("results").and_then(Value::as_array) {
                rows.extend(arr.iter().cloned());
            }
        }
        _ => {}
    }
    Ok(rows)
}

fn format_google_size_range(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() || s == "UNSPECIFIED" || s == "UNKNOWN" {
        return None;
    }
    let formatted = match s {
        "LESS_THAN_FIVE_HUNDRED" => "< 500".to_string(),
        "LESS_THAN_ONE_THOUSAND" => "< 1K".to_string(),
        "ONE_THOUSAND_TO_TEN_THOUSAND" => "1K–10K".to_string(),
        "TEN_THOUSAND_TO_FIFTY_THOUSAND" => "10K–50K".to_string(),
        "FIFTY_THOUSAND_TO_ONE_HUNDRED_THOUSAND" => "50K–100K".to_string(),
        "ONE_HUNDRED_THOUSAND_TO_THREE_HUNDRED_THOUSAND" => "100K–300K".to_string(),
        "THREE_HUNDRED_THOUSAND_TO_FIVE_HUNDRED_THOUSAND" => "300K–500K".to_string(),
        "FIVE_HUNDRED_THOUSAND_TO_ONE_MILLION" => "500K–1M".to_string(),
        "ONE_MILLION_TO_TWO_MILLION" => "1M–2M".to_string(),
        "TWO_MILLION_TO_THREE_MILLION" => "2M–3M".to_string(),
        "THREE_MILLION_TO_FIVE_MILLION" => "3M–5M".to_string(),
        "FIVE_MILLION_TO_TEN_MILLION" => "5M–10M".to_string(),
        "TEN_MILLION_TO_TWENTY_MILLION" => "10M–20M".to_string(),
        "TWENTY_MILLION_TO_THIRTY_MILLION" => "20M–30M".to_string(),
        "THIRTY_MILLION_TO_FIFTY_MILLION" => "30M–50M".to_string(),
        "OVER_FIFTY_MILLION" => "> 50M".to_string(),
        other => other.to_lowercase().replace('_', " "),
    };
    Some(formatted)
}

pub async fn collect(client: &Client, cfg: &GoogleAdsConfig, lookback_days: u32) -> PlatformReport {
    let start = Instant::now();
    let endpoint_base = format!("{}/{}", cfg.base_url, cfg.api_version);

    let access_token = match resolve_access_token(client, cfg).await {
        Ok(tok) => tok,
        Err(e) => {
            return PlatformReport::failed(
                PlatformId::Google,
                endpoint_base,
                start.elapsed().as_millis() as u64,
                e.to_string(),
            );
        }
    };

    let (since, until) = date_window_ymd(lookback_days);

    let customer_q = format!(
        "SELECT customer.id, customer.descriptive_name, customer.currency_code, customer.status, \
         metrics.cost_micros, metrics.impressions, metrics.clicks, metrics.conversions \
         FROM customer WHERE segments.date BETWEEN '{since}' AND '{until}'"
    );
    let campaign_q = format!(
        "SELECT campaign.id, campaign.name, campaign.status, campaign.advertising_channel_type, \
         metrics.cost_micros, metrics.impressions, metrics.clicks, metrics.conversions \
         FROM campaign WHERE segments.date BETWEEN '{since}' AND '{until}'"
    );
    let user_list_q =
        "SELECT user_list.id, user_list.resource_name, user_list.name, user_list.description, \
         user_list.type, user_list.membership_status, user_list.match_rate_percentage, \
         user_list.size_for_search, user_list.size_for_display, \
         user_list.size_range_for_search, user_list.size_range_for_display, \
         user_list.eligible_for_search, user_list.eligible_for_display \
         FROM user_list";
    let jobs_q = "SELECT offline_user_data_job.id, offline_user_data_job.resource_name, \
         offline_user_data_job.type, offline_user_data_job.status, \
         offline_user_data_job.failure_reason, \
         offline_user_data_job.customer_match_user_list_metadata.user_list \
         FROM offline_user_data_job";

    let (cust_res, camp_res, ul_res, jobs_res) = tokio::join!(
        search_stream(client, cfg, &access_token, &customer_q),
        search_stream(client, cfg, &access_token, &campaign_q),
        search_stream(client, cfg, &access_token, user_list_q),
        search_stream(client, cfg, &access_token, jobs_q),
    );

    if cust_res.is_err() && camp_res.is_err() && ul_res.is_err() {
        let msg = ul_res
            .err()
            .or_else(|| cust_res.err())
            .map(|e| e.to_string())
            .unwrap_or_else(|| "Google Ads query failed".to_string());
        return PlatformReport::failed(
            PlatformId::Google,
            endpoint_base,
            start.elapsed().as_millis() as u64,
            msg,
        );
    }

    let mut warnings = Vec::new();
    let mut account_name = None;
    let mut currency = None;
    let mut account_status = None;
    let mut total_spend = 0.0_f64;
    let mut has_spend = false;
    let mut total_impressions = 0_u64;
    let mut total_clicks = 0_u64;
    let mut total_conversions = 0.0_f64;

    match cust_res {
        Ok(rows) => {
            for row in rows {
                if let Some(c) = row.get("customer") {
                    if account_name.is_none() {
                        account_name = json_str(c, "descriptiveName");
                    }
                    if currency.is_none() {
                        currency = json_str(c, "currencyCode");
                    }
                    if account_status.is_none() {
                        account_status = json_str(c, "status");
                    }
                }
                if let Some(m) = row.get("metrics") {
                    if let Some(micros) = json_f64(m, "costMicros") {
                        total_spend += micros / 1_000_000.0;
                        has_spend = true;
                    }
                    total_impressions += json_u64(m, "impressions").unwrap_or(0);
                    total_clicks += json_u64(m, "clicks").unwrap_or(0);
                    total_conversions += json_f64(m, "conversions").unwrap_or(0.0);
                }
            }
        }
        Err(e) => warnings.push(format!("Customer spend query failed: {e}")),
    }

    let mut campaigns = Vec::new();
    match camp_res {
        Ok(rows) => {
            for row in rows {
                let c = match row.get("campaign") {
                    Some(v) => v,
                    None => continue,
                };
                let m = row.get("metrics");
                let id = json_str(c, "id").unwrap_or_default();
                let status = json_str(c, "status").unwrap_or_else(|| "UNKNOWN".to_string());
                let spend = m
                    .and_then(|v| json_f64(v, "costMicros"))
                    .map(|mi| mi / 1_000_000.0);
                let impressions = m.and_then(|v| json_u64(v, "impressions"));
                let clicks = m.and_then(|v| json_u64(v, "clicks"));
                let conversions = m.and_then(|v| json_f64(v, "conversions"));
                let ctr = match (impressions, clicks) {
                    (Some(i), Some(cl)) if i > 0 => Some((cl as f64 / i as f64) * 100.0),
                    _ => None,
                };
                let cpc = match (spend, clicks) {
                    (Some(sp), Some(cl)) if cl > 0 => Some(sp / cl as f64),
                    _ => None,
                };
                campaigns.push(CampaignRecord {
                    platform: PlatformId::Google,
                    id: id.clone(),
                    name: json_str(c, "name").unwrap_or(id),
                    normalized_status: NormalizedStatus::from_raw(&status),
                    status,
                    objective: json_str(c, "advertisingChannelType"),
                    spend,
                    currency: currency.clone(),
                    impressions,
                    clicks,
                    conversions,
                    ctr,
                    cpc,
                    extra: c
                        .as_object()
                        .map(|o| extra_from_object(o, &["id", "name", "status"]))
                        .unwrap_or_default(),
                });
            }
        }
        Err(e) => warnings.push(format!("Campaign query failed: {e}")),
    }

    // Group offline_user_data_job by target user_list resource name
    let mut jobs_by_user_list: HashMap<String, Vec<OperationRecord>> = HashMap::new();
    match jobs_res {
        Ok(rows) => {
            for row in rows {
                let j = match row.get("offlineUserDataJob") {
                    Some(v) => v,
                    None => continue,
                };
                let ul_res_name = j
                    .pointer("/customerMatchUserListMetadata/userList")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let status = json_str(j, "status").unwrap_or_else(|| "UNKNOWN".to_string());
                let op = OperationRecord {
                    id: json_str(j, "id")
                        .or_else(|| json_str(j, "resourceName"))
                        .unwrap_or_default(),
                    operation: json_str(j, "type")
                        .unwrap_or_else(|| "CUSTOMER_MATCH_USER_LIST".to_string()),
                    normalized_status: NormalizedStatus::from_raw(&status),
                    status,
                    created_at: None,
                    updated_at: None,
                    count: None,
                    error: json_str(j, "failureReason"),
                    extra: j
                        .as_object()
                        .map(|o| extra_from_object(o, &["id", "type", "status"]))
                        .unwrap_or_default(),
                };
                jobs_by_user_list.entry(ul_res_name).or_default().push(op);
            }
        }
        Err(e) => warnings.push(format!("Offline user data jobs query failed: {e}")),
    }

    let mut audiences = Vec::new();
    match ul_res {
        Ok(rows) => {
            for row in rows {
                let ul = match row.get("userList") {
                    Some(v) => v,
                    None => continue,
                };
                let id = json_str(ul, "id").unwrap_or_default();
                let res_name = json_str(ul, "resourceName")
                    .unwrap_or_else(|| format!("customers/{}/userLists/{id}", cfg.customer_id));
                let membership =
                    json_str(ul, "membershipStatus").unwrap_or_else(|| "UNKNOWN".to_string());
                let eligible_search = ul
                    .get("eligibleForSearch")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let eligible_display = ul
                    .get("eligibleForDisplay")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                let (status, normalized_status) = if membership.eq_ignore_ascii_case("OPEN") {
                    if eligible_search || eligible_display {
                        ("ready".to_string(), NormalizedStatus::Ready)
                    } else {
                        ("open_not_eligible".to_string(), NormalizedStatus::Warning)
                    }
                } else {
                    (membership.clone(), NormalizedStatus::from_raw(&membership))
                };

                let size_search = json_i64(ul, "sizeForSearch");
                let size_display = json_i64(ul, "sizeForDisplay");
                let matched_users = match (size_search, size_display) {
                    (Some(s), Some(d)) if s > 0 || d > 0 => Some(format!(
                        "Search: {} / Display: {}",
                        format_compact_count(s),
                        format_compact_count(d)
                    )),
                    (Some(s), _) if s > 0 => Some(format_compact_count(s)),
                    (_, Some(d)) if d > 0 => Some(format_compact_count(d)),
                    _ => format_google_size_range(
                        ul.get("sizeRangeForSearch").and_then(Value::as_str),
                    )
                    .or_else(|| {
                        format_google_size_range(
                            ul.get("sizeRangeForDisplay").and_then(Value::as_str),
                        )
                    }),
                };

                let ops = jobs_by_user_list.remove(&res_name).unwrap_or_default();

                audiences.push(AudienceRecord {
                    platform: PlatformId::Google,
                    id: id.clone(),
                    name: json_str(ul, "name").unwrap_or(id),
                    description: json_str(ul, "description"),
                    audience_type: json_str(ul, "type"),
                    status,
                    normalized_status,
                    matched_users,
                    matched_ids: None,
                    uploaded_ids: None,
                    invalid_ids: None,
                    match_rate_pct: json_f64(ul, "matchRatePercentage"),
                    revision: None,
                    created_at: None,
                    updated_at: None,
                    operations: ops,
                    operations_has_more: false,
                    operations_error: None,
                    extra: ul
                        .as_object()
                        .map(|o| extra_from_object(o, &[]))
                        .unwrap_or_default(),
                });
            }
        }
        Err(e) => warnings.push(format!("User lists query failed: {e}")),
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
        platform: PlatformId::Google,
        display_name: PlatformId::Google.display_name().to_string(),
        status,
        base_url: endpoint_base,
        account_id: Some(cfg.customer_id.clone()),
        account_name,
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
