//! Read-only adapter for the server owner's `bedrock-usage --json` CLI.

use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

use crate::account_usage::{AccountUsageMeter, AccountUsageProvider, FetchError};

pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpendingWindow {
    pub spent_usd: f64,
    pub cap_usd: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub used_percent: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpendingBudget {
    pub daily: SpendingWindow,
    pub rolling_7d: SpendingWindow,
    pub suspended: bool,
    pub data_updated_at_unix: u64,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(rename = "lastSyncAt")]
    last_sync_at: String,
    budget: RawWindow,
    budget_7d: RawWindow,
    suspension: Suspension,
}

#[derive(Deserialize)]
struct Suspension {
    is_suspended: bool,
}

#[derive(Deserialize)]
struct RawWindow {
    #[serde(alias = "spent_today")]
    spent: f64,
    cap: Option<f64>,
    remaining: Option<f64>,
    pct_used: Option<f64>,
}

impl RawWindow {
    fn into_window(self) -> Result<SpendingWindow, FetchError> {
        if std::iter::once(self.spent)
            .chain(self.cap)
            .chain(self.remaining)
            .chain(self.pct_used)
            .any(|n| !n.is_finite() || n < 0.0 || n > (u64::MAX / 100) as f64)
        {
            return Err(FetchError::Failed("invalid Bedrock budget amounts".into()));
        }
        Ok(SpendingWindow {
            spent_usd: self.spent,
            cap_usd: self.cap,
            remaining_usd: self.remaining,
            used_percent: self.pct_used,
        })
    }
}

pub fn parse(raw: &[u8], now_unix: u64) -> Result<AccountUsageMeter, FetchError> {
    // Never include raw output in errors: the CLI response contains account data.
    let snapshot: Snapshot = serde_json::from_slice(raw)
        .map_err(|_| FetchError::Failed("invalid Bedrock usage JSON".into()))?;
    let updated = time::OffsetDateTime::parse(
        &snapshot.last_sync_at,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()
    .and_then(|t| u64::try_from(t.unix_timestamp()).ok())
    .ok_or_else(|| FetchError::Failed("invalid Bedrock data timestamp".into()))?;
    Ok(AccountUsageMeter {
        provider: AccountUsageProvider::Bedrock,
        plan_type: None,
        windows: Vec::new(),
        budget: Some(SpendingBudget {
            daily: snapshot.budget.into_window()?,
            rolling_7d: snapshot.budget_7d.into_window()?,
            suspended: snapshot.suspension.is_suspended,
            data_updated_at_unix: updated,
        }),
        fetched_at_unix: now_unix,
        source: "bedrock_usage_cli",
    })
}

pub async fn fetch(command: &[String], now_unix: u64) -> Result<AccountUsageMeter, FetchError> {
    let Some(program) = command.first().filter(|s| !s.trim().is_empty()) else {
        return Err(FetchError::Unavailable(
            "Bedrock usage command is empty".into(),
        ));
    };
    let mut process =
        tokio::process::Command::from(crate::noninteractive_process::command(program));
    process
        .args(&command[1..])
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = process.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FetchError::Unavailable("Bedrock usage command not found".into())
        } else {
            FetchError::Failed("could not start Bedrock usage command".into())
        }
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| FetchError::Failed("Bedrock usage stdout unavailable".into()))?;
    let result = tokio::time::timeout(TIMEOUT, async {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| FetchError::Failed("could not read Bedrock usage output".into()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(FetchError::Failed(
                "Bedrock usage output exceeded size limit".into(),
            ));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| FetchError::Failed("Bedrock usage command wait failed".into()))?;
        if !status.success() {
            return Err(FetchError::Failed("Bedrock usage command failed".into()));
        }
        parse(&bytes, now_unix)
    })
    .await;
    // Also kill and reap on read/size errors or timeout, not only on drop.
    if !matches!(&result, Ok(Ok(_))) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result.map_err(|_| FetchError::Failed("Bedrock usage command timed out".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::json!({
            "lastSyncAt":"2026-11-02T17:54:34Z",
            "suspension":{"is_suspended":true},
            "budget":{"spent_today":300.21,"cap":300,"remaining":0,"pct_used":100},
            "budget_7d":{"spent":960.73,"cap":1050,"remaining":89.27,"pct_used":91.5}
        })
    }

    #[test]
    fn preserves_spending_limits_suspension_and_source_freshness() {
        let meter = parse(&serde_json::to_vec(&fixture()).unwrap(), 1_793_642_074).unwrap();
        let budget = meter.budget.as_ref().unwrap();
        assert_eq!(budget.daily.spent_usd, 300.21);
        assert_eq!(budget.daily.remaining_usd, Some(0.0));
        assert_eq!(budget.rolling_7d.cap_usd, Some(1050.0));
        assert!(budget.suspended);
        assert!(!meter.is_stale(budget.data_updated_at_unix));
        assert!(meter.is_stale(budget.data_updated_at_unix + 901));
        assert!(meter.windows.is_empty()); // No invented seven-day reset date.
    }

    #[test]
    fn absent_or_invalid_data_is_not_a_zero_balance() {
        assert!(parse(b"{}", 0).is_err());
        let mut value = fixture();
        value["budget"]["spent_today"] = serde_json::json!(-1);
        assert!(parse(&serde_json::to_vec(&value).unwrap(), 0).is_err());
        value = fixture();
        value["budget_7d"]["cap"] = serde_json::Value::Null;
        value["budget_7d"]["remaining"] = serde_json::Value::Null;
        value["budget_7d"]["pct_used"] = serde_json::Value::Null;
        let meter = parse(&serde_json::to_vec(&value).unwrap(), 0).unwrap();
        assert_eq!(meter.budget.unwrap().rolling_7d.remaining_usd, None);
    }

    #[test]
    fn bedrock_polls_every_five_minutes_without_changing_subscription_cadence() {
        assert_eq!(
            AccountUsageProvider::Bedrock.poll_interval(),
            Duration::from_secs(300)
        );
        assert_eq!(
            AccountUsageProvider::Claude.poll_interval(),
            Duration::from_secs(240)
        );
        assert_eq!(
            AccountUsageProvider::Codex.poll_interval(),
            Duration::from_secs(240)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_adds_json_argument_and_redacts_failures() {
        let raw = fixture().to_string();
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("test \"$1\" = --json || exit 42; printf '%s' '{raw}'"),
            "fixture".into(),
        ];
        assert!(fetch(&command, 1_793_642_074).await.is_ok());
        let failed = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo sensitive-details >&2; exit 1".into(),
        ];
        assert_eq!(
            fetch(&failed, 0).await.unwrap_err().to_string(),
            "Bedrock usage command failed"
        );
        assert!(matches!(
            fetch(&[], 0).await,
            Err(FetchError::Unavailable(_))
        ));
    }
}
