use crate::config::LineOaConfig;
use crate::error::ApiError;
use crate::http::send_json;
use crate::models::{
    format_compact_count, AccountMetrics, AudienceRecord, NormalizedStatus, OperationRecord,
    PlatformConnectionStatus, PlatformId, PlatformReport,
};
use crate::platforms::{extra_from_object, json_i64, json_str, json_u64, parse_timestamp};
use chrono::{Duration, FixedOffset, Utc};
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::time::Instant;

async fn line_get(
    client: &Client,
    cfg: &LineOaConfig,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value, ApiError> {
    let url = format!("{}{}", cfg.base_url, path);
    let req = client
        .get(&url)
        .bearer_auth(&cfg.channel_access_token)
        .header("Accept", "application/json")
        .query(query);
    send_json(req, path).await
}

async fn fetch_audience_jobs(
    client: &Client,
    cfg: &LineOaConfig,
    audience_group_id: &str,
) -> (Vec<OperationRecord>, Option<String>) {
    let path = format!(
        "/v2/bot/audienceGroup/{}",
        urlencoding::encode(audience_group_id)
    );
    match line_get(client, cfg, &path, &[]).await {
        Ok(res) => {
            let ops = res
                .get("jobs")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|j| {
                            let status =
                                json_str(j, "jobStatus").unwrap_or_else(|| "UNKNOWN".to_string());
                            let normalized_status = NormalizedStatus::from_raw(&status);
                            OperationRecord {
                                id: json_str(j, "audienceGroupJobId").unwrap_or_default(),
                                operation: json_str(j, "type")
                                    .unwrap_or_else(|| "DIFF_ADD".to_string()),
                                status,
                                normalized_status,
                                created_at: parse_timestamp(j.get("created")),
                                updated_at: None,
                                count: json_u64(j, "audienceCount"),
                                error: json_str(j, "failedType"),
                                extra: j
                                    .as_object()
                                    .map(|o| {
                                        extra_from_object(
                                            o,
                                            &[
                                                "audienceGroupJobId",
                                                "type",
                                                "jobStatus",
                                                "audienceCount",
                                                "created",
                                            ],
                                        )
                                    })
                                    .unwrap_or_default(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            (ops, None)
        }
        Err(e) => (Vec::new(), Some(e.to_string())),
    }
}

pub async fn collect(client: &Client, cfg: &LineOaConfig, concurrency: usize) -> PlatformReport {
    let start = Instant::now();
    // LINE insight data is indexed on UTC+9 (JST) and finalized for yesterday.
    let jst = FixedOffset::east_opt(9 * 3600).unwrap();
    let insight_date = (Utc::now().with_timezone(&jst) - Duration::days(1))
        .format("%Y%m%d")
        .to_string();

    let q_date = [("date", insight_date.as_str())];
    let (bot_res, quota_res, cons_res, fol_res, del_res, aud_res) = tokio::join!(
        line_get(client, cfg, "/v2/bot/info", &[]),
        line_get(client, cfg, "/v2/bot/message/quota", &[]),
        line_get(client, cfg, "/v2/bot/message/quota/consumption", &[]),
        line_get(client, cfg, "/v2/bot/insight/followers", &q_date),
        line_get(client, cfg, "/v2/bot/insight/message/delivery", &q_date),
        line_get(
            client,
            cfg,
            "/v2/bot/audienceGroup/list",
            &[
                ("page", "1"),
                ("size", "40"),
                ("includeOwnedAudienceGroups", "true"),
            ],
        ),
    );

    if quota_res.is_err() && cons_res.is_err() && aud_res.is_err() {
        let msg = aud_res
            .err()
            .or_else(|| quota_res.err())
            .map(|e| e.to_string())
            .unwrap_or_else(|| "LINE OA API request failed".to_string());
        return PlatformReport::failed(
            PlatformId::LineOa,
            &cfg.base_url,
            start.elapsed().as_millis() as u64,
            msg,
        );
    }

    let mut warnings = Vec::new();
    let mut account_id = None;
    let mut account_name = None;

    if let Ok(bot) = bot_res {
        account_id = json_str(&bot, "basicId").or_else(|| json_str(&bot, "userId"));
        account_name = json_str(&bot, "displayName");
    }

    let mut quota_type = None;
    let mut quota_limit = None;
    match quota_res {
        Ok(q) => {
            quota_type = json_str(&q, "type");
            quota_limit = json_u64(&q, "value");
        }
        Err(e) => warnings.push(format!("Message quota unavailable: {e}")),
    }

    let mut quota_used = None;
    match cons_res {
        Ok(c) => {
            quota_used = json_u64(&c, "totalUsage");
        }
        Err(e) => warnings.push(format!("Quota consumption unavailable: {e}")),
    }

    let mut followers = None;
    let mut targeted_reaches = None;
    let mut blocks = None;
    match fol_res {
        Ok(f) => {
            if json_str(&f, "status").as_deref() == Some("ready") {
                followers = json_u64(&f, "followers");
                targeted_reaches = json_u64(&f, "targetedReaches");
                blocks = json_u64(&f, "blocks");
            }
        }
        Err(e) => warnings.push(format!("Followers insight unavailable: {e}")),
    }

    let mut daily_deliveries: Option<u64> = None;
    if let Ok(d) = del_res {
        if json_str(&d, "status").as_deref() == Some("ready") {
            let sum = [
                "broadcast",
                "targeting",
                "autoResponse",
                "welcomeResponse",
                "chat",
                "apiBroadcast",
                "apiPush",
                "apiMulticast",
                "apiNarrowcast",
                "apiReply",
            ]
            .iter()
            .filter_map(|k| json_u64(&d, k))
            .sum::<u64>();
            daily_deliveries = Some(sum);
        }
    }

    let mut audiences = Vec::new();
    let mut audiences_has_more = false;
    match aud_res {
        Ok(res) => {
            audiences_has_more = res
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if audiences_has_more {
                warnings.push(
                    "More than 40 LINE OA audience groups exist. Only the first page is shown."
                        .to_string(),
                );
            }

            let raw_groups = res
                .get("audienceGroups")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let group_ids: Vec<(usize, String)> = raw_groups
                .iter()
                .enumerate()
                .map(|(idx, g)| (idx, json_str(g, "audienceGroupId").unwrap_or_default()))
                .collect();

            let job_results: Vec<(usize, Vec<OperationRecord>, Option<String>)> =
                stream::iter(group_ids)
                    .map(|(idx, id)| async move {
                        let (jobs, err) = fetch_audience_jobs(client, cfg, &id).await;
                        (idx, jobs, err)
                    })
                    .buffer_unordered(concurrency)
                    .collect()
                    .await;

            let mut jobs_by_idx = vec![(Vec::new(), None); raw_groups.len()];
            for (idx, jobs, err) in job_results {
                jobs_by_idx[idx] = (jobs, err);
            }

            for (g, (jobs, jobs_err)) in raw_groups.into_iter().zip(jobs_by_idx) {
                let id = json_str(&g, "audienceGroupId").unwrap_or_default();
                let desc = json_str(&g, "description");
                let status = json_str(&g, "status").unwrap_or_else(|| "UNKNOWN".to_string());
                let normalized_status = NormalizedStatus::from_raw(&status);
                let matched_users = json_i64(&g, "audienceCount").map(format_compact_count);

                audiences.push(AudienceRecord {
                    platform: PlatformId::LineOa,
                    id: id.clone(),
                    name: desc.clone().unwrap_or_else(|| format!("Audience {id}")),
                    description: json_str(&g, "createRoute").map(|r| format!("Route: {r}")),
                    audience_type: json_str(&g, "type"),
                    status,
                    normalized_status,
                    matched_users,
                    matched_ids: None,
                    uploaded_ids: None,
                    invalid_ids: None,
                    match_rate_pct: None,
                    revision: None,
                    created_at: parse_timestamp(g.get("created")),
                    updated_at: parse_timestamp(g.get("created")),
                    operations: jobs,
                    operations_has_more: false,
                    operations_error: jobs_err,
                    extra: g
                        .as_object()
                        .map(|o| extra_from_object(o, &[]))
                        .unwrap_or_default(),
                });
            }
        }
        Err(e) => warnings.push(format!("Audience groups unavailable: {e}")),
    }

    let metrics = Some(AccountMetrics {
        impressions: daily_deliveries,
        quota_type,
        quota_limit,
        quota_used,
        followers,
        targeted_reaches,
        blocks,
        date_range: Some(format!("Insight date {insight_date} (JST)")),
        ..Default::default()
    });

    let status = if warnings.is_empty() {
        PlatformConnectionStatus::Ok
    } else {
        PlatformConnectionStatus::Partial
    };

    PlatformReport {
        platform: PlatformId::LineOa,
        display_name: PlatformId::LineOa.display_name().to_string(),
        status,
        base_url: cfg.base_url.clone(),
        account_id,
        account_name,
        duration_ms: start.elapsed().as_millis() as u64,
        metrics,
        audiences,
        audiences_has_more,
        campaigns: Vec::new(),
        warnings,
        error: None,
        policy_revision: None,
    }
}
