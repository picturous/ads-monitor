use crate::config::OpenAiConfig;
use crate::error::ApiError;
use crate::http::send_json;
use crate::models::{
    AudienceRecord, NormalizedStatus, OperationRecord, PlatformConnectionStatus, PlatformId,
    PlatformReport,
};
use crate::platforms::{extra_from_object, json_str, parse_timestamp};
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::time::Instant;

const LIMIT: usize = 100;
const MAX_PAGES: usize = 10;

/// Format OpenAI range strings like "1m_5m", "under_1k", "over_10m", "none".
pub fn format_openai_range(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("none") {
        return Some("None".to_string());
    }
    let mut out = s.to_string();
    if let Some(rest) = out.strip_prefix("under_") {
        out = format!("< {rest}");
    } else if let Some(rest) = out.strip_prefix("over_") {
        out = format!("> {rest}");
    }
    out = out.replace("_to_", "–");
    let parts: Vec<&str> = out.split('_').collect();
    if parts.len() == 2
        && parts
            .iter()
            .all(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit()))
    {
        out = format!("{}–{}", parts[0], parts[1]);
    } else {
        out = out.replace('_', " ");
    }
    let mut formatted = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let prev_is_digit = i > 0 && chars[i - 1].is_ascii_digit();
        let next_is_word = i + 1 < chars.len() && chars[i + 1].is_alphanumeric();
        if prev_is_digit && !next_is_word && (c == 'k' || c == 'm' || c == 'b') {
            formatted.push(c.to_ascii_uppercase());
        } else {
            formatted.push(c);
        }
    }
    Some(formatted)
}

async fn api_get(client: &Client, cfg: &OpenAiConfig, path: &str) -> Result<Value, ApiError> {
    let url = format!("{}{}", cfg.base_url, path);
    let req = client
        .get(&url)
        .bearer_auth(&cfg.api_key)
        .header("Accept", "application/json");
    send_json(req, path).await
}

async fn fetch_operations(
    client: &Client,
    cfg: &OpenAiConfig,
    audience_id: &str,
) -> (Vec<OperationRecord>, bool, Option<String>) {
    let path = format!(
        "/custom_audiences/{}/operations?limit={LIMIT}",
        urlencoding::encode(audience_id)
    );
    match api_get(client, cfg, &path).await {
        Ok(res) => {
            let has_more = res
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let ops = res
                .get("data")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|o| {
                            let status =
                                json_str(o, "status").unwrap_or_else(|| "unknown".to_string());
                            let normalized_status = NormalizedStatus::from_raw(&status);
                            let extra = o
                                .as_object()
                                .map(|m| {
                                    extra_from_object(
                                        m,
                                        &[
                                            "operation_id",
                                            "operation",
                                            "status",
                                            "custom_audience_id",
                                            "object",
                                            "created_at",
                                            "updated_at",
                                        ],
                                    )
                                })
                                .unwrap_or_default();
                            OperationRecord {
                                id: json_str(o, "operation_id")
                                    .or_else(|| json_str(o, "id"))
                                    .unwrap_or_default(),
                                operation: json_str(o, "operation")
                                    .unwrap_or_else(|| "operation".to_string()),
                                status,
                                normalized_status,
                                created_at: parse_timestamp(o.get("created_at")),
                                updated_at: parse_timestamp(o.get("updated_at")),
                                count: o.get("count").and_then(Value::as_u64),
                                error: json_str(o, "error"),
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

pub async fn collect(client: &Client, cfg: &OpenAiConfig, concurrency: usize) -> PlatformReport {
    let start = Instant::now();
    if !cfg.options.enable_audiences {
        return PlatformReport {
            platform: PlatformId::OpenAi,
            display_name: PlatformId::OpenAi.display_name().to_string(),
            status: PlatformConnectionStatus::Ok,
            base_url: cfg.base_url.clone(),
            account_id: None,
            account_name: Some("OpenAI Ads".to_string()),
            duration_ms: 0,
            metrics: None,
            audiences: Vec::new(),
            audiences_has_more: false,
            audiences_total_before_filter: None,
            campaigns: Vec::new(),
            warnings: vec!["Audiences disabled via environment configuration.".to_string()],
            error: None,
            policy_revision: None,
        };
    }

    let mut raw_audiences = Vec::new();
    let mut audiences_has_more = false;
    let mut policy_revision = None;
    let mut after: Option<String> = None;

    for _ in 0..MAX_PAGES {
        let list_path = match &after {
            Some(cursor) => format!(
                "/custom_audiences?limit={LIMIT}&after={}",
                urlencoding::encode(cursor)
            ),
            None => format!("/custom_audiences?limit={LIMIT}"),
        };
        let res = match api_get(client, cfg, &list_path).await {
            Ok(v) => v,
            Err(e) => {
                if raw_audiences.is_empty() {
                    return PlatformReport::failed(
                        PlatformId::OpenAi,
                        &cfg.base_url,
                        start.elapsed().as_millis() as u64,
                        e.to_string(),
                    );
                }
                audiences_has_more = true;
                break;
            }
        };

        if policy_revision.is_none() {
            policy_revision = json_str(&res, "policy_revision");
        }
        let page_items = res
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let has_more = res
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next_cursor = res
            .get("last_id")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| page_items.last().and_then(|a| json_str(a, "id")));

        raw_audiences.extend(page_items);
        if !has_more || next_cursor.is_none() {
            audiences_has_more = false;
            break;
        }
        audiences_has_more = true;
        after = next_cursor;
    }

    let total_before_filter = raw_audiences.len();
    let filtered_raw: Vec<Value> = raw_audiences
        .into_iter()
        .filter(|aud| {
            let name = json_str(aud, "name").unwrap_or_default();
            let desc = json_str(aud, "description");
            cfg.options.matches_audience(&name, desc.as_deref())
        })
        .collect();

    // Only fetch operations for audiences that passed the name filter!
    let ids: Vec<(usize, String)> = filtered_raw
        .iter()
        .enumerate()
        .map(|(idx, aud)| (idx, json_str(aud, "id").unwrap_or_default()))
        .collect();

    let op_results: Vec<(usize, Vec<OperationRecord>, bool, Option<String>)> = stream::iter(ids)
        .map(|(idx, id)| async move {
            let (ops, has_more, err) = fetch_operations(client, cfg, &id).await;
            (idx, ops, has_more, err)
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut ops_by_idx = vec![(Vec::new(), false, None); filtered_raw.len()];
    for (idx, ops, has_more, err) in op_results {
        ops_by_idx[idx] = (ops, has_more, err);
    }

    let mut warnings = Vec::new();
    if audiences_has_more {
        warnings.push(format!(
            "More than {} OpenAI audiences exist. Showing first {MAX_PAGES} pages.",
            LIMIT * MAX_PAGES
        ));
    }

    let mut audiences = Vec::with_capacity(filtered_raw.len());
    let mut op_more_count = 0usize;
    let mut op_err_count = 0usize;

    for (aud, (ops, ops_has_more, ops_err)) in filtered_raw.into_iter().zip(ops_by_idx) {
        if ops_has_more {
            op_more_count += 1;
        }
        if ops_err.is_some() {
            op_err_count += 1;
        }
        let status = json_str(&aud, "status").unwrap_or_else(|| "unknown".to_string());
        let normalized_status = NormalizedStatus::from_raw(&status);
        let extra = aud
            .as_object()
            .map(|m| extra_from_object(m, &["object"]))
            .unwrap_or_default();

        audiences.push(AudienceRecord {
            platform: PlatformId::OpenAi,
            id: json_str(&aud, "id").unwrap_or_default(),
            name: json_str(&aud, "name").unwrap_or_else(|| "(unnamed)".to_string()),
            description: json_str(&aud, "description"),
            audience_type: Some("custom_audience".to_string()),
            status,
            normalized_status,
            matched_users: format_openai_range(
                aud.get("matched_user_count_range").and_then(Value::as_str),
            ),
            matched_ids: format_openai_range(
                aud.get("matched_identifier_count_range")
                    .and_then(Value::as_str),
            ),
            uploaded_ids: format_openai_range(
                aud.get("uploaded_identifier_count_range")
                    .and_then(Value::as_str),
            ),
            invalid_ids: format_openai_range(
                aud.get("invalid_identifier_count_range")
                    .and_then(Value::as_str),
            ),
            match_rate_pct: None,
            revision: json_str(&aud, "membership_revision"),
            created_at: parse_timestamp(aud.get("created_at")),
            updated_at: parse_timestamp(aud.get("updated_at")),
            operations: ops,
            operations_has_more: ops_has_more,
            operations_error: ops_err,
            extra,
        });
    }

    if op_more_count > 0 {
        warnings.push(format!(
            "{op_more_count} OpenAI audience(s) have more than {LIMIT} operations. Only the first page is shown."
        ));
    }
    if op_err_count > 0 {
        warnings.push(format!(
            "Couldn't load operations for {op_err_count} OpenAI audience(s)."
        ));
    }

    let status = if op_err_count > 0 {
        PlatformConnectionStatus::Partial
    } else {
        PlatformConnectionStatus::Ok
    };

    PlatformReport {
        platform: PlatformId::OpenAi,
        display_name: PlatformId::OpenAi.display_name().to_string(),
        status,
        base_url: cfg.base_url.clone(),
        account_id: None,
        account_name: Some("OpenAI Ads".to_string()),
        duration_ms: start.elapsed().as_millis() as u64,
        metrics: None,
        audiences,
        audiences_has_more,
        audiences_total_before_filter: if cfg.options.has_audience_filter() {
            Some(total_before_filter)
        } else {
            None
        },
        campaigns: Vec::new(),
        warnings,
        error: None,
        policy_revision,
    }
}
