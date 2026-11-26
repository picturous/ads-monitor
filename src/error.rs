use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP {status} on {endpoint}: {body}")]
    HttpStatus {
        status: u16,
        endpoint: String,
        body: String,
    },

    #[error("Network error on {endpoint}: {message}")]
    Network { endpoint: String, message: String },

    #[error("JSON parse error on {endpoint}: {message}")]
    Parse { endpoint: String, message: String },

    #[error("API error on {endpoint} (code {code}): {message}")]
    PlatformApi {
        endpoint: String,
        code: String,
        message: String,
    },

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Configuration error: {0}")]
    Config(String),
}
