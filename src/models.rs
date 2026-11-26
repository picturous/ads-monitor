use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformId {
    OpenAi,
    Facebook,
    Google,
    Tiktok,
    LineOa,
    LineAds,
}

impl PlatformId {
    pub const ALL: [PlatformId; 6] = [
        PlatformId::OpenAi,
        PlatformId::Facebook,
        PlatformId::Google,
        PlatformId::Tiktok,
        PlatformId::LineOa,
        PlatformId::LineAds,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            PlatformId::OpenAi => "open_ai",
            PlatformId::Facebook => "facebook",
            PlatformId::Google => "google",
            PlatformId::Tiktok => "tiktok",
            PlatformId::LineOa => "line_oa",
            PlatformId::LineAds => "line_ads",
        }
    }

    pub fn slug(&self) -> &'static str {
        match self {
            PlatformId::OpenAi => "openai",
            PlatformId::Facebook => "facebook",
            PlatformId::Google => "google",
            PlatformId::Tiktok => "tiktok",
            PlatformId::LineOa => "lineoa",
            PlatformId::LineAds => "lineads",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            PlatformId::OpenAi => "OpenAI Ads",
            PlatformId::Facebook => "Meta / Facebook Ads",
            PlatformId::Google => "Google Ads",
            PlatformId::Tiktok => "TikTok Ads",
            PlatformId::LineOa => "LINE OA",
            PlatformId::LineAds => "LINE Ads",
        }
    }
}

impl fmt::Display for PlatformId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

impl FromStr for PlatformId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "open_ai" | "openai_ads" => Ok(PlatformId::OpenAi),
            "facebook" | "fb" | "meta" | "meta_ads" | "facebook_ads" => Ok(PlatformId::Facebook),
            "google" | "google_ads" | "gads" => Ok(PlatformId::Google),
            "tiktok" | "tiktok_ads" | "tt" => Ok(PlatformId::Tiktok),
            "lineoa" | "line_oa" | "line-oa" | "line_messaging" => Ok(PlatformId::LineOa),
            "lineads" | "line_ads" | "line-ads" => Ok(PlatformId::LineAds),
            other => Err(format!(
                "Unknown platform '{other}'. Valid values: openai, facebook, google, tiktok, lineoa, lineads"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformConnectionStatus {
    Ok,
    Partial,
    Error,
    Unconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedStatus {
    Ready,
    Processing,
    Failed,
    Warning,
    Neutral,
}

impl NormalizedStatus {
    pub fn from_raw(status: &str) -> Self {
        let s = status.trim().to_ascii_lowercase();
        match s.as_str() {
            "ready" | "succeeded" | "success" | "completed" | "finished" | "active" | "enabled"
            | "normal" | "open" | "available" | "delivering" | "valid" => NormalizedStatus::Ready,
            "processing" | "pending" | "in_progress" | "queued" | "running" | "building"
            | "working" | "activating" | "updating" | "calculating" | "populating"
            | "in_review" => NormalizedStatus::Processing,
            "failed" | "error" | "canceled" | "cancelled" | "rejected" | "expired" | "disabled"
            | "removed" | "deleted" | "flagged" | "restricted" | "suspended" | "blocked" => {
                NormalizedStatus::Failed
            }
            "paused" | "inactive" | "too_small" | "limited" | "closed" | "with_issues"
            | "not_ready" | "empty" => NormalizedStatus::Warning,
            _ => NormalizedStatus::Neutral,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorSnapshot {
    pub generated_at: i64,
    pub lookback_days: u32,
    pub served: bool,
    pub demo_mode: bool,
    pub audiences_enabled: bool,
    pub campaigns_enabled: bool,
    pub audience_filter: Vec<String>,
    pub summary: GlobalSummary,
    pub platforms: Vec<PlatformReport>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalSummary {
    pub configured_platforms: usize,
    pub healthy_platforms: usize,
    pub error_platforms: usize,
    pub total_audiences: usize,
    pub ready_audiences: usize,
    pub processing_audiences: usize,
    pub failed_audiences: usize,
    pub total_operations: usize,
    pub succeeded_operations: usize,
    pub processing_operations: usize,
    pub failed_operations: usize,
    pub total_campaigns: usize,
    pub active_campaigns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformReport {
    pub platform: PlatformId,
    pub display_name: String,
    pub status: PlatformConnectionStatus,
    pub base_url: String,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub duration_ms: u64,
    pub metrics: Option<AccountMetrics>,
    pub audiences: Vec<AudienceRecord>,
    pub audiences_has_more: bool,
    pub audiences_total_before_filter: Option<usize>,
    pub campaigns: Vec<CampaignRecord>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub policy_revision: Option<String>,
}

impl PlatformReport {
    pub fn unconfigured(platform: PlatformId, base_url: impl Into<String>, hint: &str) -> Self {
        Self {
            platform,
            display_name: platform.display_name().to_string(),
            status: PlatformConnectionStatus::Unconfigured,
            base_url: base_url.into(),
            account_id: None,
            account_name: None,
            duration_ms: 0,
            metrics: None,
            audiences: Vec::new(),
            audiences_has_more: false,
            audiences_total_before_filter: None,
            campaigns: Vec::new(),
            warnings: vec![hint.to_string()],
            error: None,
            policy_revision: None,
        }
    }

    pub fn failed(
        platform: PlatformId,
        base_url: impl Into<String>,
        duration_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            platform,
            display_name: platform.display_name().to_string(),
            status: PlatformConnectionStatus::Error,
            base_url: base_url.into(),
            account_id: None,
            account_name: None,
            duration_ms,
            metrics: None,
            audiences: Vec::new(),
            audiences_has_more: false,
            audiences_total_before_filter: None,
            campaigns: Vec::new(),
            warnings: Vec::new(),
            error: Some(error.into()),
            policy_revision: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountMetrics {
    pub spend: Option<f64>,
    pub currency: Option<String>,
    pub balance: Option<f64>,
    pub spend_cap: Option<f64>,
    pub impressions: Option<u64>,
    pub clicks: Option<u64>,
    pub conversions: Option<f64>,
    pub ctr: Option<f64>,
    pub cpc: Option<f64>,
    pub reach: Option<u64>,
    pub active_campaigns: Option<usize>,
    pub total_campaigns: Option<usize>,
    pub account_status: Option<String>,
    // LINE OA specific metrics
    pub quota_type: Option<String>,
    pub quota_limit: Option<u64>,
    pub quota_used: Option<u64>,
    pub followers: Option<u64>,
    pub targeted_reaches: Option<u64>,
    pub blocks: Option<u64>,
    pub date_range: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignRecord {
    pub platform: PlatformId,
    pub id: String,
    pub name: String,
    pub status: String,
    pub normalized_status: NormalizedStatus,
    pub objective: Option<String>,
    pub spend: Option<f64>,
    pub currency: Option<String>,
    pub impressions: Option<u64>,
    pub clicks: Option<u64>,
    pub conversions: Option<f64>,
    pub ctr: Option<f64>,
    pub cpc: Option<f64>,
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudienceRecord {
    pub platform: PlatformId,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub audience_type: Option<String>,
    pub status: String,
    pub normalized_status: NormalizedStatus,
    pub matched_users: Option<String>,
    pub matched_ids: Option<String>,
    pub uploaded_ids: Option<String>,
    pub invalid_ids: Option<String>,
    pub match_rate_pct: Option<f64>,
    pub revision: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub operations: Vec<OperationRecord>,
    pub operations_has_more: bool,
    pub operations_error: Option<String>,
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub operation: String,
    pub status: String,
    pub normalized_status: NormalizedStatus,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub count: Option<u64>,
    pub error: Option<String>,
    pub extra: BTreeMap<String, Value>,
}

impl GlobalSummary {
    pub fn from_reports(reports: &[PlatformReport]) -> Self {
        let mut s = Self::default();
        for r in reports {
            match r.status {
                PlatformConnectionStatus::Ok => {
                    s.configured_platforms += 1;
                    s.healthy_platforms += 1;
                }
                PlatformConnectionStatus::Partial => {
                    s.configured_platforms += 1;
                    s.healthy_platforms += 1;
                }
                PlatformConnectionStatus::Error => {
                    s.configured_platforms += 1;
                    s.error_platforms += 1;
                }
                PlatformConnectionStatus::Unconfigured => {}
            }

            for a in &r.audiences {
                s.total_audiences += 1;
                match a.normalized_status {
                    NormalizedStatus::Ready => s.ready_audiences += 1,
                    NormalizedStatus::Processing => s.processing_audiences += 1,
                    NormalizedStatus::Failed => s.failed_audiences += 1,
                    _ => {}
                }
                for op in &a.operations {
                    s.total_operations += 1;
                    match op.normalized_status {
                        NormalizedStatus::Ready => s.succeeded_operations += 1,
                        NormalizedStatus::Processing => s.processing_operations += 1,
                        NormalizedStatus::Failed => s.failed_operations += 1,
                        _ => {}
                    }
                }
            }

            for c in &r.campaigns {
                s.total_campaigns += 1;
                if c.normalized_status == NormalizedStatus::Ready {
                    s.active_campaigns += 1;
                }
            }
        }
        s
    }
}

/// Format an integer count into compact human-readable notation (e.g., 12500 -> "12.5K").
pub fn format_compact_count(n: i64) -> String {
    if n < 0 {
        return "—".to_string();
    }
    if n >= 1_000_000 {
        let v = n as f64 / 1_000_000.0;
        if (v - v.round()).abs() < 0.05 {
            format!("{}M", v.round() as i64)
        } else {
            format!("{v:.1}M")
        }
    } else if n >= 1_000 {
        let v = n as f64 / 1_000.0;
        if (v - v.round()).abs() < 0.05 {
            format!("{}K", v.round() as i64)
        } else {
            format!("{v:.1}K")
        }
    } else {
        n.to_string()
    }
}

/// Format lower..upper range neatly.
pub fn format_count_range(lower: Option<i64>, upper: Option<i64>) -> Option<String> {
    match (lower, upper) {
        (Some(l), Some(u)) if l >= 0 && u >= 0 => {
            if l == u {
                Some(format_compact_count(l))
            } else {
                Some(format!(
                    "{}–{}",
                    format_compact_count(l),
                    format_compact_count(u)
                ))
            }
        }
        (Some(l), _) if l >= 0 => Some(format!("> {}", format_compact_count(l))),
        (_, Some(u)) if u >= 0 => Some(format!("< {}", format_compact_count(u))),
        _ => None,
    }
}
