use crate::models::PlatformId;
use std::env;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub lookback_days: u32,
    pub concurrency: usize,
    pub timeout_secs: u64,
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
}

#[derive(Debug, Clone)]
pub struct FacebookConfig {
    pub access_token: String,
    pub ad_account_ids: Vec<String>,
    pub api_version: String,
    pub base_url: String,
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
}

#[derive(Debug, Clone)]
pub struct TikTokConfig {
    pub access_token: String,
    pub advertiser_id: String,
    pub bc_id: Option<String>,
    pub base_url: String,
}

#[derive(Debug, Clone)]
pub struct LineOaConfig {
    pub channel_access_token: String,
    pub base_url: String,
}

#[derive(Debug, Clone)]
pub struct LineAdsConfig {
    pub access_key: String,
    pub secret_key: String,
    pub account_id: String,
    pub base_url: String,
}

fn env_non_empty(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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

        let openai = env_non_empty("OPENAI_API_KEY").map(|api_key| OpenAiConfig {
            api_key,
            base_url: strip_trailing_slash(
                env_non_empty("OPENAI_ADS_BASE_URL")
                    .unwrap_or_else(|| "https://api.ads.openai.com/v1".to_string()),
            ),
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
            }),
            _ => None,
        };

        let line_oa = env_non_empty("LINE_OA_CHANNEL_ACCESS_TOKEN").map(|channel_access_token| {
            LineOaConfig {
                channel_access_token,
                base_url: strip_trailing_slash(
                    env_non_empty("LINE_OA_BASE_URL")
                        .unwrap_or_else(|| "https://api.line.me".to_string()),
                ),
            }
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
