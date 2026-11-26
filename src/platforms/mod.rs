pub mod facebook;
pub mod google;
pub mod line_ads;
pub mod line_oa;
pub mod openai;
pub mod tiktok;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub fn json_str(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

pub fn json_i64(v: &Value, key: &str) -> Option<i64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn json_u64(v: &Value, key: &str) -> Option<u64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().or_else(|| {
            n.as_i64()
                .and_then(|i| if i >= 0 { Some(i as u64) } else { None })
        }),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

pub fn json_f64(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Parse Unix seconds, Unix milliseconds, RFC 3339, or "YYYY-MM-DD HH:MM:SS" into Unix seconds.
pub fn parse_timestamp(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => {
            let raw = n.as_i64()?;
            if raw <= 0 {
                return None;
            }
            if raw > 10_000_000_000 {
                Some(raw / 1000)
            } else {
                Some(raw)
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(raw) = s.parse::<i64>() {
                if raw <= 0 {
                    return None;
                }
                return Some(if raw > 10_000_000_000 {
                    raw / 1000
                } else {
                    raw
                });
            }
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp());
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
                return Some(dt.and_utc().timestamp());
            }
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp());
            }
            None
        }
        _ => None,
    }
}

pub fn extra_from_object(obj: &Map<String, Value>, skip: &[&str]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        if !skip.contains(&k.as_str()) && !v.is_null() {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

pub fn date_window_ymd(days: u32) -> (String, String) {
    let end = Utc::now().date_naive();
    let start = end - chrono::Duration::days(i64::from(days.saturating_sub(1)));
    (
        start.format("%Y-%m-%d").to_string(),
        end.format("%Y-%m-%d").to_string(),
    )
}
