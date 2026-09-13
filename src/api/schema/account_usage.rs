use serde::{Deserialize, Serialize};

pub use crate::account_usage::AccountUsageProvider;

/// One provider rate-limit window as reported by the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountUsageWindowInfo {
    /// Provider window kind: Claude `session`, `weekly_all`, `weekly_scoped`;
    /// Codex `primary`, `secondary`. Other kinds are passed through verbatim.
    pub kind: String,
    /// Short human label such as `5h`, `week`, or the scoped model family.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_model: Option<String>,
    pub used_percent: f64,
    pub remaining_percent: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// One provider account's latest usage reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountUsageInfo {
    pub provider: AccountUsageProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    pub windows: Vec<AccountUsageWindowInfo>,
    pub fetched_at_unix: u64,
    /// True when the reading is older than the freshness window or the last
    /// poll failed long enough ago that the numbers may no longer hold.
    pub stale: bool,
    pub source: String,
}

impl AccountUsageInfo {
    pub fn from_meter(meter: &crate::account_usage::AccountUsageMeter, now_unix: u64) -> Self {
        Self {
            provider: meter.provider,
            plan_type: meter.plan_type.clone(),
            windows: meter
                .windows
                .iter()
                .map(|window| AccountUsageWindowInfo {
                    kind: window.kind.clone(),
                    label: crate::account_usage::window_label(window),
                    scope_model: window.scope_model.clone(),
                    used_percent: window.used_percent,
                    remaining_percent: crate::account_usage::remaining_percent(window.used_percent),
                    resets_at_unix: window.resets_at_unix,
                    window_minutes: window.window_minutes,
                    severity: window.severity.clone(),
                    active: window.active,
                })
                .collect(),
            fetched_at_unix: meter.fetched_at_unix,
            stale: meter.is_stale(now_unix),
            source: meter.source.to_owned(),
        }
    }
}
