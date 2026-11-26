use crate::models::PlatformId;
use std::env;

/// Per-platform collection options (feature toggles + audience name filter).
#[derive(Debug, Clone)]
pub struct CollectOptions {
    pub enable_audiences: bool,
    pub enable_campaigns: bool,
    /// Case-insensitive substring patterns; when non-empty, only audiences whose
    /// name (or description) contains at least one pattern are loaded & expanded.
    pub audience_filter: Vec<String>,
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self {
            enable_audiences: true,
            enable_campaigns: true,
            audience_filter: Vec::new(),
        }
    }
}

impl CollectOptions {
    pub fn matches_audience(&self, name: &str, description: Option<&str>) -> bool {
        if self.audience_filter.is_empty() {
            return true;
        }
        let name_lc = name.to_ascii_lowercase();
        let desc_lc = description.map(|d| d.to_ascii_lowercase());
        self.audience_filter.iter().any(|pat| {
            let p = pat.to_ascii_lowercase();
            name_lc.contains(&p) || desc_lc.as_deref().is_some_and(|d| d.contains(&p))
        })
    }

    pub fn has_audience_filter(&self) -> bool {
        !self.audience_filter.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub lookback_days: u32,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub db_path: Option<String>,
    pub refresh_interval_mins: u64,
    pub refresh_cron: Option<String>,
    pub history_retention_days: u32,
    pub enable_audiences: bool,
    pub enable_campaigns: bool,
    pub audience_filter: Vec<String>,
    pub selected_platforms: Vec<PlatformId>,
    pub openai: Option<OpenAiConfig>,
    pub facebook: Option<FacebookConfig>,
    pub google: Option<GoogleAdsConfig>,
    pub tiktok: Option<TikTokConfig>,
    pub line_oa: Option<LineOaConfig>,
    pub line_ads: Option<LineAdsConfig>,
}

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: String,
    pub options: CollectOptions,
}

#[derive(Debug, Clone)]
pub struct FacebookConfig {
    pub access_token: String,
    pub ad_account_ids: Vec<String>,
    pub api_version: String,
    pub base_url: String,
    pub options: CollectOptions,
}

#[derive(Debug, Clone)]
pub struct GoogleAdsConfig {
    pub customer_id: String,
    pub developer_token: String,
    pub login_customer_id: Option<String>,
    pub access_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub refresh_token: Option<String>,
    pub api_version: String,
    pub base_url: String,
    pub oauth_token_url: String,
    pub options: CollectOptions,
}

#[derive(Debug, Clone)]
pub struct TikTokConfig {
    pub access_token: String,
    pub advertiser_id: String,
    pub bc_id: Option<String>,
    pub base_url: String,
    pub options: CollectOptions,
}

#[derive(Debug, Clone)]
pub enum LineOaAuth {
    /// Static long-lived or pre-issued channel access token.
    AccessToken(String),
    /// Issue & auto-refresh 15-minute stateless tokens via POST /oauth2/v3/token.
    Stateless {
        channel_id: String,
        channel_secret: String,
    },
}

#[derive(Debug, Clone)]
pub struct LineOaConfig {
    pub auth: LineOaAuth,
    pub base_url: String,
    pub options: CollectOptions,
}

#[derive(Debug, Clone)]
pub struct LineAdsConfig {
    pub access_key: String,
    pub secret_key: String,
    pub account_id: String,
    pub base_url: String,
    pub options: CollectOptions,
}

fn env_non_empty(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_bool(keys: &[&str], default: bool) -> bool {
    for key in keys {
        if let Some(raw) = env_non_empty(key) {
            match raw.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => return true,
                "0" | "false" | "no" | "off" => return false,
                _ => {}
            }
        }
    }
    default
}

fn parse_csv_patterns(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_audience_filter(keys: &[&str], fallback: &[String]) -> Vec<String> {
    for key in keys {
        if let Some(raw) = env_non_empty(key) {
            return parse_csv_patterns(&raw);
        }
    }
    fallback.to_vec()
}

fn strip_trailing_slash(url: String) -> String {
    url.trim_end_matches('/').to_string()
}

impl AppConfig {
    pub fn from_env(
        lookback_days_override: Option<u32>,
        concurrency_override: Option<usize>,
        timeout_override: Option<u64>,
        selected_platforms: Vec<PlatformId>,
    ) -> Self {
        let lookback_days = lookback_days_override
            .or_else(|| env_non_empty("ADS_MONITOR_DAYS").and_then(|v| v.parse().ok()))
            .unwrap_or(30);

        let concurrency = concurrency_override
            .or_else(|| env_non_empty("ADS_MONITOR_WORKERS").and_then(|v| v.parse().ok()))
            .unwrap_or(8)
            .max(1);

        let timeout_secs = timeout_override
            .or_else(|| env_non_empty("ADS_MONITOR_TIMEOUT_SECS").and_then(|v| v.parse().ok()))
            .unwrap_or(30)
            .max(1);

        let db_path = match env_non_empty("ADS_MONITOR_DB_PATH") {
            Some(raw)
                if matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "none" | "off" | "disabled" | "memory" | ":memory:"
                ) =>
            {
                None
            }
            Some(raw) => Some(raw),
            None => Some("data/ads-monitor.db".to_string()),
        };

        let refresh_interval_mins = env_non_empty("ADS_MONITOR_REFRESH_INTERVAL_MINS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        let refresh_cron = env_non_empty("ADS_MONITOR_REFRESH_CRON");

        let history_retention_days = env_non_empty("ADS_MONITOR_HISTORY_RETENTION_DAYS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        let enable_audiences = env_bool(&["ADS_MONITOR_ENABLE_AUDIENCES"], true);
        let enable_campaigns = env_bool(&["ADS_MONITOR_ENABLE_CAMPAIGNS"], true);
        let audience_filter = env_non_empty("ADS_MONITOR_AUDIENCE_FILTER")
            .map(|s| parse_csv_patterns(&s))
            .unwrap_or_default();

        let openai = env_non_empty("OPENAI_API_KEY").map(|api_key| OpenAiConfig {
            api_key,
            base_url: strip_trailing_slash(
                env_non_empty("OPENAI_ADS_BASE_URL")
                    .unwrap_or_else(|| "https://api.ads.openai.com/v1".to_string()),
            ),
            options: CollectOptions {
                enable_audiences: env_bool(&["OPENAI_ENABLE_AUDIENCES"], enable_audiences),
                enable_campaigns: false,
                audience_filter: env_audience_filter(&["OPENAI_AUDIENCE_FILTER"], &audience_filter),
            },
        });

        let facebook = match (
            env_non_empty("META_ACCESS_TOKEN").or_else(|| env_non_empty("FACEBOOK_ACCESS_TOKEN")),
            env_non_empty("META_AD_ACCOUNT_ID").or_else(|| env_non_empty("FACEBOOK_AD_ACCOUNT_ID")),
        ) {
            (Some(access_token), Some(raw_ids)) => {
                let ad_account_ids: Vec<String> = raw_ids
                    .split(',')
                    .map(|s| s.trim().trim_start_matches("act_").to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if ad_account_ids.is_empty() {
                    None
                } else {
                    Some(FacebookConfig {
                        access_token,
                        ad_account_ids,
                        api_version: env_non_empty("META_API_VERSION")
                            .or_else(|| env_non_empty("FACEBOOK_API_VERSION"))
                            .unwrap_or_else(|| "v25.0".to_string()),
                        base_url: strip_trailing_slash(
                            env_non_empty("META_BASE_URL")
                                .or_else(|| env_non_empty("FACEBOOK_BASE_URL"))
                                .unwrap_or_else(|| "https://graph.facebook.com".to_string()),
                        ),
                        options: CollectOptions {
                            enable_audiences: env_bool(
                                &["META_ENABLE_AUDIENCES", "FACEBOOK_ENABLE_AUDIENCES"],
                                enable_audiences,
                            ),
                            enable_campaigns: env_bool(
                                &["META_ENABLE_CAMPAIGNS", "FACEBOOK_ENABLE_CAMPAIGNS"],
                                enable_campaigns,
                            ),
                            audience_filter: env_audience_filter(
                                &["META_AUDIENCE_FILTER", "FACEBOOK_AUDIENCE_FILTER"],
                                &audience_filter,
                            ),
                        },
                    })
                }
            }
            _ => None,
        };

        let google = match (
            env_non_empty("GOOGLE_ADS_CUSTOMER_ID"),
            env_non_empty("GOOGLE_ADS_DEVELOPER_TOKEN"),
        ) {
            (Some(raw_cid), Some(developer_token)) => {
                let access_token = env_non_empty("GOOGLE_ADS_ACCESS_TOKEN");
                let client_id = env_non_empty("GOOGLE_ADS_CLIENT_ID");
                let client_secret = env_non_empty("GOOGLE_ADS_CLIENT_SECRET");
                let refresh_token = env_non_empty("GOOGLE_ADS_REFRESH_TOKEN");
                let has_oauth =
                    client_id.is_some() && client_secret.is_some() && refresh_token.is_some();

                if access_token.is_some() || has_oauth {
                    Some(GoogleAdsConfig {
                        customer_id: raw_cid.replace('-', ""),
                        developer_token,
                        login_customer_id: env_non_empty("GOOGLE_ADS_LOGIN_CUSTOMER_ID")
                            .map(|s| s.replace('-', "")),
                        access_token,
                        client_id,
                        client_secret,
                        refresh_token,
                        api_version: env_non_empty("GOOGLE_ADS_API_VERSION")
                            .unwrap_or_else(|| "v20".to_string()),
                        base_url: strip_trailing_slash(
                            env_non_empty("GOOGLE_ADS_BASE_URL")
                                .unwrap_or_else(|| "https://googleads.googleapis.com".to_string()),
                        ),
                        oauth_token_url: env_non_empty("GOOGLE_OAUTH_TOKEN_URL")
                            .unwrap_or_else(|| "https://oauth2.googleapis.com/token".to_string()),
                        options: CollectOptions {
                            enable_audiences: env_bool(
                                &["GOOGLE_ADS_ENABLE_AUDIENCES"],
                                enable_audiences,
                            ),
                            enable_campaigns: env_bool(
                                &["GOOGLE_ADS_ENABLE_CAMPAIGNS"],
                                enable_campaigns,
                            ),
                            audience_filter: env_audience_filter(
                                &["GOOGLE_ADS_AUDIENCE_FILTER"],
                                &audience_filter,
                            ),
                        },
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        let tiktok = match (
            env_non_empty("TIKTOK_ACCESS_TOKEN"),
            env_non_empty("TIKTOK_ADVERTISER_ID"),
        ) {
            (Some(access_token), Some(advertiser_id)) => Some(TikTokConfig {
                access_token,
                advertiser_id,
                bc_id: env_non_empty("TIKTOK_BC_ID"),
                base_url: strip_trailing_slash(
                    env_non_empty("TIKTOK_BASE_URL")
                        .unwrap_or_else(|| "https://business-api.tiktok.com".to_string()),
                ),
                options: CollectOptions {
                    enable_audiences: env_bool(&["TIKTOK_ENABLE_AUDIENCES"], enable_audiences),
                    enable_campaigns: env_bool(&["TIKTOK_ENABLE_CAMPAIGNS"], enable_campaigns),
                    audience_filter: env_audience_filter(
                        &["TIKTOK_AUDIENCE_FILTER"],
                        &audience_filter,
                    ),
                },
            }),
            _ => None,
        };

        let line_oa_auth = match (
            env_non_empty("LINE_OA_CHANNEL_ID"),
            env_non_empty("LINE_OA_CHANNEL_SECRET"),
        ) {
            (Some(channel_id), Some(channel_secret)) => Some(LineOaAuth::Stateless {
                channel_id,
                channel_secret,
            }),
            _ => env_non_empty("LINE_OA_CHANNEL_ACCESS_TOKEN").map(LineOaAuth::AccessToken),
        };

        let line_oa = line_oa_auth.map(|auth| LineOaConfig {
            auth,
            base_url: strip_trailing_slash(
                env_non_empty("LINE_OA_BASE_URL")
                    .unwrap_or_else(|| "https://api.line.me".to_string()),
            ),
            options: CollectOptions {
                enable_audiences: env_bool(&["LINE_OA_ENABLE_AUDIENCES"], enable_audiences),
                enable_campaigns: env_bool(
                    &["LINE_OA_ENABLE_CAMPAIGNS", "LINE_OA_ENABLE_INSIGHTS"],
                    enable_campaigns,
                ),
                audience_filter: env_audience_filter(
                    &["LINE_OA_AUDIENCE_FILTER"],
                    &audience_filter,
                ),
            },
        });

        let line_ads = match (
            env_non_empty("LINE_ADS_ACCESS_KEY"),
            env_non_empty("LINE_ADS_SECRET_KEY"),
            env_non_empty("LINE_ADS_ACCOUNT_ID"),
        ) {
            (Some(access_key), Some(secret_key), Some(account_id)) => Some(LineAdsConfig {
                access_key,
                secret_key,
                account_id,
                base_url: strip_trailing_slash(
                    env_non_empty("LINE_ADS_BASE_URL")
                        .unwrap_or_else(|| "https://ads.line.me".to_string()),
                ),
                options: CollectOptions {
                    enable_audiences: env_bool(&["LINE_ADS_ENABLE_AUDIENCES"], enable_audiences),
                    enable_campaigns: env_bool(&["LINE_ADS_ENABLE_CAMPAIGNS"], enable_campaigns),
                    audience_filter: env_audience_filter(
                        &["LINE_ADS_AUDIENCE_FILTER"],
                        &audience_filter,
                    ),
                },
            }),
            _ => None,
        };

        let selected_platforms = if selected_platforms.is_empty() {
            PlatformId::ALL.to_vec()
        } else {
            selected_platforms
        };

        Self {
            lookback_days,
            concurrency,
            timeout_secs,
            db_path,
            refresh_interval_mins,
            refresh_cron,
            history_retention_days,
            enable_audiences,
            enable_campaigns,
            audience_filter,
            selected_platforms,
            openai,
            facebook,
            google,
            tiktok,
            line_oa,
            line_ads,
        }
    }

    pub fn has_any_configured(&self) -> bool {
        self.openai.is_some()
            || self.facebook.is_some()
            || self.google.is_some()
            || self.tiktok.is_some()
            || self.line_oa.is_some()
            || self.line_ads.is_some()
    }
}
