use crate::collector::{collect_all, demo_snapshot};
use crate::config::AppConfig;
use crate::models::MonitorSnapshot;
use crate::render::render_html;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use reqwest::Client;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServerState {
    pub client: Client,
    pub config: AppConfig,
    pub demo: bool,
}

async fn fetch_snapshot(state: &ServerState) -> MonitorSnapshot {
    if state.demo {
        demo_snapshot(state.config.lookback_days, true)
    } else {
        collect_all(&state.client, &state.config, true).await
    }
}

async fn index_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let snapshot = fetch_snapshot(&state).await;
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

async fn snapshot_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let snapshot = fetch_snapshot(&state).await;
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(snapshot),
    )
}

async fn health_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "demo_mode": state.demo,
        "lookback_days": state.config.lookback_days,
    }))
}

pub fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/api/snapshot", get(snapshot_handler))
        .route("/api/health", get(health_handler))
        .with_state(state)
}
