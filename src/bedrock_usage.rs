//! Read-only adapters for the server owner's Bedrock spend caps.
//!
//! Two systems cap Bedrock keys. DJ's Bedrock Access Hub caps the per-user DJ
//! key (`bedrock-usage --json`), and the landing-zone `bedrock-spend-cap`
//! Lambda caps each named key per UTC day (`bedrock-cap status --json`, one
//! JSON object per line). Both become one list of capped keys.

use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

use crate::account_usage::{AccountUsageMeter, AccountUsageProvider, FetchError};

pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BYTES: u64 = 1024 * 1024;
/// The DJ key's IAM name is a generated `BedrockAPIKey-…`, so it gets a short
/// fixed label instead of its prefix.
const HUB_LABEL: &str = "dj";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpendingKey {
    /// Key or cap name; clients show its beginning.
    pub label: String,
    /// Length of the capped window: 1 is the UTC day, more is rolling.
    pub window_days: u16,
    pub spent_usd: f64,
    pub cap_usd: Option<f64>,
    /// The cap system has cut this key off.
    pub denied: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpendingBudget {
    pub keys: Vec<SpendingKey>,
    /// Oldest source timestamp among the sources that answered.
    pub data_updated_at_unix: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commands {
    pub hub: Vec<String>,
    pub caps: Vec<String>,
}

#[derive(Deserialize)]
struct HubSnapshot {
    #[serde(rename = "lastSyncAt")]
    last_sync_at: String,
    budget: HubWindow,
    budget_7d: HubWindow,
    budget_28d: Option<HubWindow>,
    suspension: HubSuspension,
}

#[derive(Deserialize)]
struct HubSuspension {
    is_suspended: bool,
}

#[derive(Deserialize)]
struct HubWindow {
    #[serde(alias = "spent_today")]
    spent: f64,
    cap: Option<f64>,
}

#[derive(Deserialize)]
struct CapStatus {
    cap: String,
    spend_usd: f64,
    cap_usd: Option<f64>,
    denied: bool,
    as_of: String,
}

fn valid_amount(n: f64) -> bool {
    n.is_finite() && (0.0..=(u64::MAX / 100) as f64).contains(&n)
}

fn key(
    label: &str,
    window_days: u16,
    spent: f64,
    cap: Option<f64>,
    denied: bool,
) -> Result<SpendingKey, FetchError> {
    if !std::iter::once(spent).chain(cap).all(valid_amount) {
        return Err(FetchError::Failed("invalid Bedrock budget amounts".into()));
    }
    Ok(SpendingKey {
        label: label.to_owned(),
        window_days,
        spent_usd: spent,
        cap_usd: cap,
        denied,
    })
}

fn unix_time(raw: &str) -> Result<u64, FetchError> {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .ok()
        .and_then(|t| u64::try_from(t.unix_timestamp()).ok())
        .ok_or_else(|| FetchError::Failed("invalid Bedrock data timestamp".into()))
}

/// Parses `bedrock-usage --json`. The day row always shows; rolling windows
/// only when DJ caps them.
pub fn parse_hub(raw: &[u8]) -> Result<(Vec<SpendingKey>, u64), FetchError> {
    // Never include raw output in errors: the response contains account data.
    let snapshot: HubSnapshot = serde_json::from_slice(raw)
        .map_err(|_| FetchError::Failed("invalid Bedrock hub JSON".into()))?;
    let updated = unix_time(&snapshot.last_sync_at)?;
    let denied = snapshot.suspension.is_suspended;
    let mut keys = vec![key(
        HUB_LABEL,
        1,
        snapshot.budget.spent,
        snapshot.budget.cap,
        denied,
    )?];
    for (days, window) in [(7, Some(snapshot.budget_7d)), (28, snapshot.budget_28d)] {
        if let Some(window) = window.filter(|w| w.cap.is_some()) {
            keys.push(key(
                &format!("{HUB_LABEL} {days}d"),
                days,
                window.spent,
                window.cap,
                denied,
            )?);
        }
    }
    Ok((keys, updated))
}

/// Parses `bedrock-cap status --json`: one object per landing-zone cap.
pub fn parse_caps(raw: &[u8]) -> Result<(Vec<SpendingKey>, u64), FetchError> {
    let mut keys = Vec::new();
    let mut oldest = None::<u64>;
    for line in raw.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let status: CapStatus = serde_json::from_slice(line)
            .map_err(|_| FetchError::Failed("invalid Bedrock cap JSON".into()))?;
        let updated = unix_time(&status.as_of)?;
        oldest = Some(oldest.map_or(updated, |o| o.min(updated)));
        keys.push(key(
            &status.cap,
            1,
            status.spend_usd,
            status.cap_usd,
            status.denied,
        )?);
    }
    let oldest = oldest.ok_or_else(|| FetchError::Failed("no Bedrock caps reported".into()))?;
    Ok((keys, oldest))
}

/// Polls both systems. One failing source keeps the other's keys; only when
/// nothing answers is the whole reading an error.
pub async fn fetch(commands: &Commands, now_unix: u64) -> Result<AccountUsageMeter, FetchError> {
    let (hub, caps) = tokio::join!(
        async {
            match run(&commands.hub, "hub").await {
                Ok(raw) => parse_hub(&raw),
                Err(e) => Err(e),
            }
        },
        async {
            match run(&commands.caps, "cap").await {
                Ok(raw) => parse_caps(&raw),
                Err(e) => Err(e),
            }
        },
    );
    let mut keys = Vec::new();
    let mut oldest = None::<u64>;
    let mut errors = Vec::new();
    for result in [hub, caps] {
        match result {
            Ok((found, updated)) => {
                keys.extend(found);
                oldest = Some(oldest.map_or(updated, |o| o.min(updated)));
            }
            Err(e) => errors.push(e),
        }
    }
    let Some(updated) = oldest else {
        let unavailable = errors
            .iter()
            .all(|e| matches!(e, FetchError::Unavailable(_)));
        let reason = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if unavailable {
            FetchError::Unavailable(reason)
        } else {
            FetchError::Failed(reason)
        });
    };
    for error in errors {
        tracing::warn!(%error, "one Bedrock spend source failed; showing the other");
    }
    Ok(AccountUsageMeter {
        provider: AccountUsageProvider::Bedrock,
        plan_type: None,
        windows: Vec::new(),
        budget: Some(SpendingBudget {
            keys,
            data_updated_at_unix: updated,
        }),
        fetched_at_unix: now_unix,
        source: "bedrock_spend_caps",
    })
}

/// Runs one configured argv and returns its stdout. Errors never carry the
/// command's output or stderr.
async fn run(command: &[String], name: &str) -> Result<Vec<u8>, FetchError> {
    let Some(program) = command.first().filter(|s| !s.trim().is_empty()) else {
        return Err(FetchError::Unavailable(format!(
            "Bedrock {name} command is empty"
        )));
    };
    let mut process =
        tokio::process::Command::from(crate::noninteractive_process::command(program));
    process
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = process.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FetchError::Unavailable(format!("Bedrock {name} command not found"))
        } else {
            FetchError::Failed(format!("could not start Bedrock {name} command"))
        }
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| FetchError::Failed(format!("Bedrock {name} stdout unavailable")))?;
    let result = tokio::time::timeout(TIMEOUT, async {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| FetchError::Failed(format!("could not read Bedrock {name} output")))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(FetchError::Failed(format!(
                "Bedrock {name} output exceeded size limit"
            )));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| FetchError::Failed(format!("Bedrock {name} command wait failed")))?;
        if !status.success() {
            return Err(FetchError::Failed(format!("Bedrock {name} command failed")));
        }
        Ok(bytes)
    })
    .await;
    // Also kill and reap on read/size errors or timeout, not only on drop.
    if !matches!(&result, Ok(Ok(_))) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result.map_err(|_| FetchError::Failed(format!("Bedrock {name} command timed out")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> serde_json::Value {
        serde_json::json!({
            "lastSyncAt":"2026-11-02T17:54:34Z",
            "suspension":{"is_suspended":true},
            "budget":{"spent_today":300.21,"cap":300,"remaining":0,"pct_used":100},
            "budget_7d":{"spent":960.73,"cap":1050,"remaining":89.27,"pct_used":91.5},
            "budget_28d":{"spent":1735.8,"cap":null,"remaining":null,"pct_used":null}
        })
    }

    const CAPS: &str = concat!(
        r#"{"cap": "pundit", "spend_usd": 216.17, "cap_usd": 300.0, "denied": false, "as_of": "2026-11-02T17:54:00+00:00", "tokens": {}}"#,
        "\n",
        r#"{"cap": "rob-agents", "spend_usd": 800.5, "cap_usd": 800.0, "denied": true, "as_of": "2026-11-02T17:55:00+00:00"}"#,
        "\n"
    );

    fn sh(output: &str) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$1\"".into(),
            "fixture".into(),
            output.into(),
        ]
    }

    #[test]
    fn hub_reports_dj_day_and_only_capped_rolling_windows() {
        let (keys, updated) = parse_hub(&serde_json::to_vec(&hub()).unwrap()).unwrap();
        let labels: Vec<_> = keys.iter().map(|k| k.label.as_str()).collect();
        assert_eq!(labels, ["dj", "dj 7d"]);
        assert_eq!(keys[0].window_days, 1);
        assert_eq!(keys[0].spent_usd, 300.21);
        assert_eq!(keys[1].window_days, 7);
        assert_eq!(keys[1].cap_usd, Some(1050.0));
        assert!(keys.iter().all(|k| k.denied));
        assert_eq!(updated, 1_793_642_074);
    }

    #[test]
    fn caps_report_one_daily_key_per_name_and_the_oldest_time() {
        let (keys, updated) = parse_caps(CAPS.as_bytes()).unwrap();
        let labels: Vec<_> = keys.iter().map(|k| k.label.as_str()).collect();
        assert_eq!(labels, ["pundit", "rob-agents"]);
        assert!(keys.iter().all(|k| k.window_days == 1));
        assert!(!keys[0].denied && keys[1].denied);
        assert_eq!(updated, 1_793_642_040);
    }

    #[test]
    fn absent_or_invalid_data_is_not_a_zero_balance() {
        assert!(parse_hub(b"{}").is_err());
        assert!(parse_caps(b"").is_err());
        assert!(parse_caps(b"not json\n").is_err());
        let mut value = hub();
        value["budget"]["spent_today"] = serde_json::json!(-1);
        assert!(parse_hub(&serde_json::to_vec(&value).unwrap()).is_err());
        value = hub();
        value["budget_7d"]["cap"] = serde_json::Value::Null;
        let (keys, _) = parse_hub(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(keys.len(), 1);
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
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn both_sources_merge_and_one_failure_keeps_the_other() {
        let commands = Commands {
            hub: sh(&hub().to_string()),
            caps: sh(CAPS),
        };
        let meter = fetch(&commands, 1_793_642_074).await.unwrap();
        let budget = meter.budget.unwrap();
        let labels: Vec<_> = budget.keys.iter().map(|k| k.label.as_str()).collect();
        assert_eq!(labels, ["dj", "dj 7d", "pundit", "rob-agents"]);
        assert_eq!(budget.data_updated_at_unix, 1_793_642_040);

        let failed = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo sensitive-details >&2; exit 1".into(),
        ];
        let partial = fetch(
            &Commands {
                hub: failed.clone(),
                caps: sh(CAPS),
            },
            0,
        )
        .await
        .unwrap();
        assert_eq!(partial.budget.unwrap().keys.len(), 2);

        let none = fetch(
            &Commands {
                hub: failed,
                caps: Vec::new(),
            },
            0,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(none.contains("Bedrock hub command failed"), "{none}");
        assert!(!none.contains("sensitive"), "{none}");
        assert!(matches!(
            fetch(&Commands::default(), 0).await,
            Err(FetchError::Unavailable(_))
        ));
    }
}
