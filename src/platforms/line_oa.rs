use crate::config::{LineOaAuth, LineOaConfig};
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
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::RwLock;

const MAX_AUDIENCE_PAGES: usize = 15;

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

fn token_cache() -> &'static RwLock<HashMap<(String, String), CachedToken>> {
    static CACHE: OnceLock<RwLock<HashMap<(String, String), CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Issue a short-lived (15-minute) stateless channel access token via LINE Messaging API v3.
pub async fn issue_stateless_token(
    client: &Client,
    base_url: &str,
    channel_id: &str,
    channel_secret: &str,
) -> Result<(String, u64), ApiError> {
    let url = format!("{}/oauth2/v3/token", base_url.trim_end_matches('/'));
    let body = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}",
        urlencoding::encode(channel_id),
        urlencoding::encode(channel_secret),
    );
    let req = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body);
    let resp: Value = send_json(req, "/oauth2/v3/token").await?;
    let token = json_str(&resp, "access_token").ok_or_else(|| {
        ApiError::Auth("LINE stateless token response missing access_token".into())
    })?;
    let expires_in = json_u64(&resp, "expires_in").unwrap_or(900);
    Ok((token, expires_in))
}

async fn resolve_bearer_token(
    client: &Client,
    cfg: &LineOaConfig,
    force_refresh: bool,
) -> Result<String, ApiError> {
    match &cfg.auth {
        LineOaAuth::AccessToken(tok) => Ok(tok.clone()),
        LineOaAuth::Stateless {
            channel_id,
            channel_secret,
        } => {
            let key = (cfg.base_url.clone(), channel_id.clone());
            if !force_refresh {
                let guard = token_cache().read().await;
                if let Some(entry) = guard.get(&key) {
                    if Instant::now() < entry.expires_at {
                        return Ok(entry.access_token.clone());
                    }
                }
            }

            let (access_token, expires_in) =
                issue_stateless_token(client, &cfg.base_url, channel_id, channel_secret).await?;
            // Refresh 60s before expiry (or half-life if TTL <= 120s)
            let safe_ttl = if expires_in > 120 {
                expires_in - 60
            } else {
                expires_in / 2
            };
            let entry = CachedToken {
                access_token: access_token.clone(),
                expires_at: Instant::now() + StdDuration::from_secs(safe_ttl),
            };
            token_cache().write().await.insert(key, entry);
            Ok(access_token)
        }
    }
}

async fn line_get(
    client: &Client,
    cfg: &LineOaConfig,
    token: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value, ApiError> {
    let url = format!("{}{}", cfg.base_url, path);
    let req = client
        .get(&url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .query(query);
    match send_json(req, path).await {
        Err(ApiError::HttpStatus { status: 401, .. })
            if matches!(cfg.auth, LineOaAuth::Stateless { .. }) =>
        {
            // Token may have just expired; issue a fresh stateless token and retry once.
            let fresh = resolve_bearer_token(client, cfg, true).await?;
            let retry_req = client
                .get(&url)
                .bearer_auth(&fresh)
                .header("Accept", "application/json")
                .query(query);
            send_json(retry_req, path).await
        }
        other => other,
    }
}

async fn fetch_all_audience_groups(
    client: &Client,
    cfg: &LineOaConfig,
    token: &str,
) -> Result<(Vec<Value>, bool), ApiError> {
    let mut items = Vec::new();
    let mut has_more = false;

    for page in 1..=MAX_AUDIENCE_PAGES {
        let page_str = page.to_string();
        let res = line_get(
            client,
            cfg,
            token,
            "/v2/bot/audienceGroup/list",
            &[
                ("page", page_str.as_str()),
                ("size", "40"),
                ("includeOwnedAudienceGroups", "true"),
            ],
        )
        .await?;

        if let Some(groups) = res.get("audienceGroups").and_then(Value::as_array) {
            items.extend(groups.iter().cloned());
        }

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

async fn fetch_audience_jobs(
    client: &Client,
    cfg: &LineOaConfig,
    token: &str,
    audience_group_id: &str,
) -> (Vec<OperationRecord>, Option<String>) {
    let path = format!(
        "/v2/bot/audienceGroup/{}",
        urlencoding::encode(audience_group_id)
    );
    match line_get(client, cfg, token, &path, &[]).await {
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

    if !cfg.options.enable_audiences && !cfg.options.enable_campaigns {
        return PlatformReport {
            platform: PlatformId::LineOa,
            display_name: PlatformId::LineOa.display_name().to_string(),
            status: PlatformConnectionStatus::Ok,
            base_url: cfg.base_url.clone(),
            account_id: None,
            account_name: None,
            duration_ms: 0,
            metrics: None,
            audiences: Vec::new(),
            audiences_has_more: false,
            audiences_total_before_filter: None,
            campaigns: Vec::new(),
            warnings: vec![
                "Both audiences and insights disabled via environment configuration.".to_string(),
            ],
            error: None,
            policy_revision: None,
        };
    }

    let token = match resolve_bearer_token(client, cfg, false).await {
        Ok(t) => t,
        Err(e) => {
            return PlatformReport::failed(
                PlatformId::LineOa,
                &cfg.base_url,
                start.elapsed().as_millis() as u64,
                e.to_string(),
            );
        }
    };

    let jst = FixedOffset::east_opt(9 * 3600).unwrap();
    let insight_date = (Utc::now().with_timezone(&jst) - Duration::days(1))
        .format("%Y%m%d")
        .to_string();

    let q_date = [("date", insight_date.as_str())];
    let (bot_res, quota_res, cons_res, fol_res, del_res, aud_res) = tokio::join!(
        line_get(client, cfg, &token, "/v2/bot/info", &[]),
        async {
            if cfg.options.enable_campaigns {
                Some(line_get(client, cfg, &token, "/v2/bot/message/quota", &[]).await)
            } else {
                None
            }
        },
        async {
            if cfg.options.enable_campaigns {
                Some(
                    line_get(
                        client,
                        cfg,
                        &token,
                        "/v2/bot/message/quota/consumption",
                        &[],
                    )
                    .await,
                )
            } else {
                None
            }
        },
        async {
            if cfg.options.enable_campaigns {
                Some(line_get(client, cfg, &token, "/v2/bot/insight/followers", &q_date).await)
            } else {
                None
            }
        },
        async {
            if cfg.options.enable_campaigns {
                Some(
                    line_get(
                        client,
                        cfg,
                        &token,
                        "/v2/bot/insight/message/delivery",
                        &q_date,
                    )
                    .await,
                )
            } else {
                None
            }
        },
        async {
            if cfg.options.enable_audiences {
                Some(fetch_all_audience_groups(client, cfg, &token).await)
            } else {
                None
            }
        },
    );

    let any_ok = matches!(quota_res, Some(Ok(_)))
        || matches!(cons_res, Some(Ok(_)))
        || matches!(aud_res, Some(Ok(_)))
        || bot_res.is_ok();

    if !any_ok {
        let msg = aud_res
            .and_then(Result::err)
            .or_else(|| quota_res.and_then(Result::err))
            .or_else(|| bot_res.err())
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
    if let Some(res) = quota_res {
        match res {
            Ok(q) => {
                quota_type = json_str(&q, "type");
                quota_limit = json_u64(&q, "value");
            }
            Err(e) => warnings.push(format!("Message quota unavailable: {e}")),
        }
    }

    let mut quota_used = None;
    if let Some(res) = cons_res {
        match res {
            Ok(c) => {
                quota_used = json_u64(&c, "totalUsage");
            }
            Err(e) => warnings.push(format!("Quota consumption unavailable: {e}")),
        }
    }

    let mut followers = None;
    let mut targeted_reaches = None;
    let mut blocks = None;
    if let Some(res) = fol_res {
        match res {
            Ok(f) => {
                if json_str(&f, "status").as_deref() == Some("ready") {
                    followers = json_u64(&f, "followers");
                    targeted_reaches = json_u64(&f, "targetedReaches");
                    blocks = json_u64(&f, "blocks");
                }
            }
            Err(e) => warnings.push(format!("Followers insight unavailable: {e}")),
        }
    }

    let mut daily_deliveries: Option<u64> = None;
    if let Some(Ok(d)) = del_res {
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
    let mut total_before_filter = 0usize;
    if let Some(res) = aud_res {
        match res {
            Ok((raw_groups, has_more)) => {
                audiences_has_more = has_more;
                if audiences_has_more {
                    warnings.push(format!(
                        "More than {} LINE OA audience groups exist. Showing first {MAX_AUDIENCE_PAGES} pages.",
                        MAX_AUDIENCE_PAGES * 40
                    ));
                }
                total_before_filter = raw_groups.len();

                let filtered_groups: Vec<Value> = raw_groups
                    .into_iter()
                    .filter(|g| {
                        let id = json_str(g, "audienceGroupId").unwrap_or_default();
                        let desc =
                            json_str(g, "description").unwrap_or_else(|| format!("Audience {id}"));
                        let route = json_str(g, "createRoute");
                        cfg.options.matches_audience(&desc, route.as_deref())
                    })
                    .collect();

                let group_ids: Vec<(usize, String)> = filtered_groups
                    .iter()
                    .enumerate()
                    .map(|(idx, g)| (idx, json_str(g, "audienceGroupId").unwrap_or_default()))
                    .collect();

                let token_ref = token.as_str();
                let job_results: Vec<(usize, Vec<OperationRecord>, Option<String>)> =
                    stream::iter(group_ids)
                        .map(|(idx, id)| async move {
                            let (jobs, err) =
                                fetch_audience_jobs(client, cfg, token_ref, &id).await;
                            (idx, jobs, err)
                        })
                        .buffer_unordered(concurrency)
                        .collect()
                        .await;

                let mut jobs_by_idx = vec![(Vec::new(), None); filtered_groups.len()];
                for (idx, jobs, err) in job_results {
                    jobs_by_idx[idx] = (jobs, err);
                }

                for (g, (jobs, jobs_err)) in filtered_groups.into_iter().zip(jobs_by_idx) {
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
    }

    let metrics = if cfg.options.enable_campaigns {
        Some(AccountMetrics {
            impressions: daily_deliveries,
            quota_type,
            quota_limit,
            quota_used,
            followers,
            targeted_reaches,
            blocks,
            date_range: Some(format!("Insight date {insight_date} (JST)")),
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
        audiences_total_before_filter: if cfg.options.enable_audiences
            && cfg.options.has_audience_filter()
        {
            Some(total_before_filter)
        } else {
            None
        },
        campaigns: Vec::new(),
        warnings,
        error: None,
        policy_revision: None,
    }
}
