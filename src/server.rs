use crate::cache::SqliteCache;
use crate::collector::{collect_all, demo_snapshot};
use crate::config::AppConfig;
use crate::models::{CacheMeta, MonitorSnapshot};
use crate::render::render_html;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use cron::Schedule;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, RwLock};

#[derive(Debug, Default, Clone)]
struct RuntimeMeta {
    refreshing: bool,
    next_refresh_at: Option<i64>,
    snapshot_count: u64,
    last_refresh_error: Option<String>,
}

/// Normalize a 5-field Unix cron expression (`*/30 * * * *`) to the 6-field
/// format expected by the `cron` crate (`0 */30 * * * *`), while leaving
/// 6- or 7-field expressions untouched.
pub fn parse_cron_schedule(expr: &str) -> Result<Schedule, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("empty cron expression".to_string());
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    let normalized = if fields.len() == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    };
    Schedule::from_str(&normalized).map_err(|e| format!("invalid cron '{trimmed}': {e}"))
}

pub struct ServerState {
    pub client: Client,
    pub config: AppConfig,
    pub demo: bool,
    cache: Arc<SqliteCache>,
    cron_schedule: Option<Schedule>,
    latest: RwLock<Option<MonitorSnapshot>>,
    meta: RwLock<RuntimeMeta>,
    refresh_lock: AsyncMutex<()>,
    refresh_generation: AtomicU64,
}

impl ServerState {
    pub fn new(client: Client, config: AppConfig, demo: bool) -> Arc<Self> {
        let cache = if demo {
            SqliteCache::open_in_memory().expect("in-memory sqlite must initialize")
        } else {
            SqliteCache::open_or_memory(config.db_path.as_deref())
        };
        Self::with_cache(client, config, demo, cache)
    }

    pub fn with_cache(
        client: Client,
        config: AppConfig,
        demo: bool,
        cache: SqliteCache,
    ) -> Arc<Self> {
        let cron_schedule =
            config
                .refresh_cron
                .as_deref()
                .and_then(|expr| match parse_cron_schedule(expr) {
                    Ok(sched) => Some(sched),
                    Err(e) => {
                        tracing::warn!(
                            "{e}; falling back to refresh_interval_mins={}",
                            config.refresh_interval_mins
                        );
                        None
                    }
                });

        let initial_snapshot = match cache.latest_snapshot() {
            Ok(Some(mut snap)) => {
                snap.served = true;
                tracing::info!(
                    "Loaded cached snapshot from {} (generated_at={})",
                    cache.path_label(),
                    snap.generated_at
                );
                Some(snap)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("Failed to read cached snapshot: {e}");
                None
            }
        };

        let snapshot_count = cache.snapshot_count().unwrap_or(0);
        let now = Utc::now().timestamp();
        let next_refresh_at = Self::compute_next_refresh_ts(
            cron_schedule.as_ref(),
            config.refresh_interval_mins,
            initial_snapshot.as_ref().map(|s| s.generated_at),
            now,
        );

        Arc::new(Self {
            client,
            config,
            demo,
            cache: Arc::new(cache),
            cron_schedule,
            latest: RwLock::new(initial_snapshot),
            meta: RwLock::new(RuntimeMeta {
                refreshing: false,
                next_refresh_at,
                snapshot_count,
                last_refresh_error: None,
            }),
            refresh_lock: AsyncMutex::new(()),
            refresh_generation: AtomicU64::new(0),
        })
    }

    fn compute_next_refresh_ts(
        cron_schedule: Option<&Schedule>,
        refresh_interval_mins: u64,
        last_generated_at: Option<i64>,
        now: i64,
    ) -> Option<i64> {
        if let Some(sched) = cron_schedule {
            let from_dt = DateTime::from_timestamp(now, 0).unwrap_or_else(Utc::now);
            return sched.after(&from_dt).next().map(|dt| dt.timestamp());
        }
        if refresh_interval_mins == 0 {
            return None;
        }
        let interval_secs = (refresh_interval_mins as i64).saturating_mul(60);
        match last_generated_at {
            Some(gen_at) if gen_at + interval_secs > now => Some(gen_at + interval_secs),
            Some(_) => Some(now),
            None => Some(now),
        }
    }

    fn is_stale(&self, last_generated_at: Option<i64>, now: i64) -> bool {
        let Some(gen_at) = last_generated_at else {
            return true;
        };
        if let Some(sched) = &self.cron_schedule {
            if let Some(from_dt) = DateTime::from_timestamp(gen_at, 0) {
                if let Some(next_after_gen) = sched.after(&from_dt).next() {
                    return next_after_gen.timestamp() <= now;
                }
            }
            return false;
        }
        if self.config.refresh_interval_mins == 0 {
            return false;
        }
        let interval_secs = (self.config.refresh_interval_mins as i64).saturating_mul(60);
        now - gen_at >= interval_secs
    }

    pub fn db_path_label(&self) -> &str {
        self.cache.path_label()
    }

    async fn stamp_snapshot(&self, mut snap: MonitorSnapshot, cached: bool) -> MonitorSnapshot {
        let meta = self.meta.read().await.clone();
        snap.served = true;
        snap.cache = Some(CacheMeta {
            cached,
            refreshing: meta.refreshing,
            refresh_interval_mins: self.config.refresh_interval_mins,
            refresh_cron: self
                .cron_schedule
                .as_ref()
                .and_then(|_| self.config.refresh_cron.clone()),
            next_refresh_at: meta.next_refresh_at,
            snapshot_count: meta.snapshot_count,
            last_refresh_error: meta.last_refresh_error,
        });
        snap
    }

    /// Return the hot cached snapshot immediately if available; otherwise perform
    /// a single-flight live collection so the first visitor never sees blank data.
    pub async fn get_cached_or_fetch(&self) -> MonitorSnapshot {
        if let Some(snap) = self.latest.read().await.clone() {
            return self.stamp_snapshot(snap, true).await;
        }
        self.refresh_now().await
    }

    /// Trigger a single-flight refresh across all configured platforms and persist
    /// the resulting snapshot to SQLite. Concurrent callers coalesce onto the
    /// in-flight collection instead of stampeding upstream APIs.
    pub async fn refresh_now(&self) -> MonitorSnapshot {
        let gen_before = self.refresh_generation.load(Ordering::Acquire);
        let _guard = self.refresh_lock.lock().await;

        // If another caller just finished refreshing while we waited for the lock,
        // coalesce and reuse that brand-new snapshot instead of hitting upstream APIs again.
        if self.refresh_generation.load(Ordering::Acquire) != gen_before {
            if let Some(existing) = self.latest.read().await.clone() {
                return self.stamp_snapshot(existing, false).await;
            }
        }

        {
            let mut m = self.meta.write().await;
            m.refreshing = true;
        }

        let snap = if self.demo {
            demo_snapshot(self.config.lookback_days, true)
        } else {
            collect_all(&self.client, &self.config, true).await
        };

        let cache = Arc::clone(&self.cache);
        let retention = self.config.history_retention_days;
        let snap_for_db = snap.clone();
        let save_res =
            tokio::task::spawn_blocking(move || cache.save_snapshot(&snap_for_db, retention)).await;

        let now = Utc::now().timestamp();
        let next_ts = Self::compute_next_refresh_ts(
            self.cron_schedule.as_ref(),
            self.config.refresh_interval_mins,
            Some(snap.generated_at),
            now,
        );

        {
            let mut latest = self.latest.write().await;
            *latest = Some(snap.clone());
        }
        self.refresh_generation.fetch_add(1, Ordering::Release);

        {
            let mut m = self.meta.write().await;
            m.refreshing = false;
            m.next_refresh_at = next_ts;
            match save_res {
                Ok(Ok(count)) => {
                    m.snapshot_count = count;
                    m.last_refresh_error = None;
                }
                Ok(Err(e)) => {
                    tracing::warn!("SQLite cache save failed: {e}");
                    m.last_refresh_error = Some(e);
                }
                Err(e) => {
                    tracing::warn!("SQLite save task panicked: {e}");
                    m.last_refresh_error = Some(e.to_string());
                }
            }
        }

        self.stamp_snapshot(snap, false).await
    }

    /// Spawn the background auto-refresh task if either `refresh_cron` or
    /// `refresh_interval_mins > 0` is configured.
    pub fn spawn_background_refresh(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if self.cron_schedule.is_none() && self.config.refresh_interval_mins == 0 {
            tracing::info!("Background auto-refresh disabled (refresh_interval_mins=0)");
            return None;
        }

        let state = Arc::clone(self);
        Some(tokio::spawn(async move {
            // If there is no cached snapshot yet or the persisted one is stale,
            // refresh immediately in the background right after startup.
            let initial_gen = state.latest.read().await.as_ref().map(|s| s.generated_at);
            let now = Utc::now().timestamp();
            if state.is_stale(initial_gen, now) {
                tracing::info!("Running initial background refresh");
                let _ = state.refresh_now().await;
            }

            loop {
                let last_gen = state.latest.read().await.as_ref().map(|s| s.generated_at);
                let now = Utc::now().timestamp();
                let next_ts = match Self::compute_next_refresh_ts(
                    state.cron_schedule.as_ref(),
                    state.config.refresh_interval_mins,
                    last_gen,
                    now,
                ) {
                    Some(ts) if ts > now => ts,
                    Some(_) => {
                        if let Some(sched) = &state.cron_schedule {
                            let from_dt = DateTime::from_timestamp(now, 0).unwrap_or_else(Utc::now);
                            sched
                                .after(&from_dt)
                                .next()
                                .map(|dt| dt.timestamp())
                                .unwrap_or(now + 60)
                        } else {
                            now + (state.config.refresh_interval_mins as i64).saturating_mul(60)
                        }
                    }
                    None => break,
                };

                {
                    let mut m = state.meta.write().await;
                    m.next_refresh_at = Some(next_ts);
                }

                let sleep_secs = (next_ts - now).max(1) as u64;
                tracing::info!("Next scheduled refresh in {sleep_secs}s (at unix {next_ts})");
                tokio::time::sleep(Duration::from_secs(sleep_secs)).await;

                // In interval mode, if a manual refresh ran recently while we slept,
                // reschedule instead of running early.
                let latest_gen = state.latest.read().await.as_ref().map(|s| s.generated_at);
                let wake_now = Utc::now().timestamp();
                if state.cron_schedule.is_none() && !state.is_stale(latest_gen, wake_now) {
                    continue;
                }

                tracing::info!("Running scheduled background refresh");
                let _ = state.refresh_now().await;
            }
        }))
    }
}

#[derive(Debug, Default, Deserialize)]
struct SnapshotQuery {
    #[serde(default)]
    refresh: Option<String>,
}

impl SnapshotQuery {
    fn wants_refresh(&self) -> bool {
        self.refresh.as_deref().is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "force"
            )
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn index_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let snapshot = state.get_cached_or_fetch().await;
    match render_html(&snapshot) {
        Ok(html) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Html(html),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to render dashboard: {e}"),
        )
            .into_response(),
    }
}

async fn snapshot_handler(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<SnapshotQuery>,
) -> impl IntoResponse {
    let snapshot = if query.wants_refresh() {
        state.refresh_now().await
    } else {
        state.get_cached_or_fetch().await
    };
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(snapshot),
    )
}

async fn refresh_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let snapshot = state.refresh_now().await;
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(snapshot),
    )
}

async fn history_handler(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<HistoryQuery>,
) -> impl IntoResponse {
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let cache = Arc::clone(&state.cache);
    let rows = tokio::task::spawn_blocking(move || cache.list_history(limit)).await;
    match rows {
        Ok(Ok(snapshots)) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(json!({
                "count": snapshots.len(),
                "retention_days": state.config.history_retention_days,
                "db_path": state.db_path_label(),
                "snapshots": snapshots,
            })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn health_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let cached_at = state.latest.read().await.as_ref().map(|s| s.generated_at);
    let meta = state.meta.read().await.clone();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "demo_mode": state.demo,
        "lookback_days": state.config.lookback_days,
        "cached": cached_at.is_some(),
        "cached_at": cached_at,
        "refreshing": meta.refreshing,
        "refresh_interval_mins": state.config.refresh_interval_mins,
        "refresh_cron": state.cron_schedule.as_ref().and_then(|_| state.config.refresh_cron.clone()),
        "next_refresh_at": meta.next_refresh_at,
        "snapshot_count": meta.snapshot_count,
        "db_path": state.db_path_label(),
        "last_refresh_error": meta.last_refresh_error,
    }))
}

pub fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/api/snapshot", get(snapshot_handler))
        .route("/api/refresh", post(refresh_handler))
        .route("/api/history", get(history_handler))
        .route("/api/health", get(health_handler))
        .with_state(state)
}
