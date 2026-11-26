use crate::error::ApiError;
use reqwest::{Client, RequestBuilder};
use serde::de::DeserializeOwned;
use std::time::Duration;

pub const USER_AGENT: &str = concat!("ads-monitor/", env!("CARGO_PKG_VERSION"));

/// Build an HTTP client using the OS native TLS certificate store (rustls-tls-native-roots),
/// matching the Python implementation's `truststore` behavior for corporate TLS proxies/CAs.
pub fn build_http_client(timeout_secs: u64) -> Result<Client, ApiError> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| ApiError::Config(format!("Failed to build HTTP client: {e}")))
}

/// Execute a request and parse a JSON body, returning structured `ApiError` on HTTP or parse failure.
pub async fn send_json<T: DeserializeOwned>(
    req: RequestBuilder,
    endpoint_label: &str,
) -> Result<T, ApiError> {
    let resp = req.send().await.map_err(|e| ApiError::Network {
        endpoint: endpoint_label.to_string(),
        message: e.to_string(),
    })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| ApiError::Network {
        endpoint: endpoint_label.to_string(),
        message: format!("failed to read response body: {e}"),
    })?;

    if !status.is_success() {
        let preview: String = text.chars().take(500).collect();
        return Err(ApiError::HttpStatus {
            status: status.as_u16(),
            endpoint: endpoint_label.to_string(),
            body: preview,
        });
    }

    serde_json::from_str(&text).map_err(|e| ApiError::Parse {
        endpoint: endpoint_label.to_string(),
        message: format!("{e} (body: {})", text.chars().take(240).collect::<String>()),
    })
}
