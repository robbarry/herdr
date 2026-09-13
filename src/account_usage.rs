//! Provider account usage polling.
//!
//! Reads the server owner's own subscription rate-limit windows so clients can
//! show how much capacity is left before a provider pauses their agents. Both
//! sources return the same data each runtime already shows its user:
//!
//! - Claude: `GET https://api.anthropic.com/api/oauth/usage` with the Claude
//!   Code OAuth token (credentials file first, macOS Keychain fallback).
//! - Codex: `codex app-server` JSON-RPC `account/rateLimits/read`; the CLI
//!   authenticates from its own stored login and no task is run.
//!
//! The poller never makes a model call, never logs or persists a token, and a
//! failed poll keeps the last reading marked stale rather than surfacing an
//! error to render. Both endpoints are unofficial, so parsing is tolerant.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const POLL_INTERVAL: Duration = Duration::from_secs(4 * 60);
pub const INITIAL_DELAY: Duration = Duration::from_secs(15);
pub const BACKOFF_MAX: Duration = Duration::from_secs(32 * 60);
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_BETA_HEADER: &str = "oauth-2025-04-20";
const CLAUDE_SOURCE: &str = "claude_oauth_usage";
const CODEX_SOURCE: &str = "codex_app_server";
const CODEX_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_RESPONSE_BYTES: usize = 1 << 20;
const RETRY_AFTER_MAX: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountUsageProvider {
    Claude,
    Codex,
}

impl AccountUsageProvider {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// One rate-limit window as the provider reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsageWindow {
    /// Provider window kind: Claude `session`, `weekly_all`, `weekly_scoped`;
    /// Codex `primary`, `secondary`. Unknown kinds are kept verbatim.
    pub kind: String,
    /// Model family a scoped window applies to (Claude `weekly_scoped`).
    pub scope_model: Option<String>,
    pub used_percent: f64,
    pub resets_at_unix: Option<u64>,
    pub window_minutes: Option<u32>,
    pub severity: Option<String>,
    pub active: Option<bool>,
}

/// One provider account's usage reading.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsageMeter {
    pub provider: AccountUsageProvider,
    pub plan_type: Option<String>,
    pub windows: Vec<AccountUsageWindow>,
    pub fetched_at_unix: u64,
    pub source: &'static str,
}

impl AccountUsageMeter {
    pub fn is_stale(&self, now_unix: u64) -> bool {
        // A future fetched_at is clock skew; it must read stale, not fresh.
        self.fetched_at_unix > now_unix + 60
            || now_unix.saturating_sub(self.fetched_at_unix) > STALE_AFTER.as_secs()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The provider cannot be polled on this machine (no credentials or binary).
    Unavailable(String),
    /// A transient failure worth retrying with backoff.
    Failed(String),
    /// The provider asked for a cooldown before the next request.
    RateLimited { retry_after: Option<Duration> },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) | Self::Failed(reason) => f.write_str(reason),
            Self::RateLimited { retry_after } => match retry_after {
                Some(delay) => write!(f, "rate limited; retry after {}s", delay.as_secs()),
                None => f.write_str("rate limited"),
            },
        }
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

pub fn backoff_delay(failures: u32) -> Duration {
    if failures == 0 {
        return POLL_INTERVAL;
    }
    let shift = failures.min(6);
    (POLL_INTERVAL * (1u32 << shift)).min(BACKOFF_MAX)
}

/// Short label for a window: `5h`, `week`, or the scoped model family.
pub fn window_label(window: &AccountUsageWindow) -> String {
    if let Some(scope) = window
        .scope_model
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
    {
        return scope.to_lowercase();
    }
    match window.kind.as_str() {
        "session" | "five_hour" => return "5h".into(),
        "weekly_all" | "seven_day" => return "week".into(),
        _ => {}
    }
    match window.window_minutes {
        Some(minutes) if minutes % (24 * 60) == 0 && minutes >= 7 * 24 * 60 => {
            let days = minutes / (24 * 60);
            if days == 7 {
                "week".into()
            } else {
                format!("{days}d")
            }
        }
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
        None => window.kind.clone(),
    }
}

/// Countdown in its largest unit only: `6d`, `3h`, or `19m`.
pub fn format_countdown(seconds: u64) -> String {
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d")
    } else if hours > 0 {
        format!("{hours}h")
    } else {
        format!("{}m", minutes.max(1))
    }
}

pub fn remaining_percent(used_percent: f64) -> u8 {
    if !used_percent.is_finite() {
        return 0;
    }
    (100.0 - used_percent).round().clamp(0.0, 100.0) as u8
}

pub async fn fetch(
    provider: AccountUsageProvider,
    now_unix: u64,
) -> Result<AccountUsageMeter, FetchError> {
    match provider {
        AccountUsageProvider::Claude => fetch_claude(now_unix).await,
        AccountUsageProvider::Codex => fetch_codex(now_unix).await,
    }
}

// --- Claude -----------------------------------------------------------------

#[derive(Deserialize)]
struct ClaudeUsageResponse {
    five_hour: Option<ClaudeUsageTopLevelWindow>,
    seven_day: Option<ClaudeUsageTopLevelWindow>,
    #[serde(default)]
    limits: Vec<ClaudeUsageLimit>,
}

#[derive(Deserialize)]
struct ClaudeUsageTopLevelWindow {
    #[serde(default)]
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeUsageLimit {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    percent: f64,
    severity: Option<String>,
    resets_at: Option<String>,
    is_active: Option<bool>,
    scope: Option<ClaudeUsageScope>,
}

#[derive(Deserialize)]
struct ClaudeUsageScope {
    model: Option<ClaudeUsageScopeModel>,
}

#[derive(Deserialize)]
struct ClaudeUsageScopeModel {
    display_name: Option<String>,
}

pub fn parse_claude_usage(body: &[u8], now_unix: u64) -> Result<AccountUsageMeter, String> {
    let parsed: ClaudeUsageResponse =
        serde_json::from_slice(body).map_err(|err| format!("invalid usage JSON: {err}"))?;
    let mut windows = Vec::new();
    if parsed.limits.is_empty() {
        if let Some(window) = parsed.five_hour {
            windows.push(AccountUsageWindow {
                kind: "session".into(),
                scope_model: None,
                used_percent: window.utilization,
                resets_at_unix: window.resets_at.as_deref().and_then(parse_rfc3339_unix),
                window_minutes: Some(300),
                severity: None,
                active: None,
            });
        }
        if let Some(window) = parsed.seven_day {
            windows.push(AccountUsageWindow {
                kind: "weekly_all".into(),
                scope_model: None,
                used_percent: window.utilization,
                resets_at_unix: window.resets_at.as_deref().and_then(parse_rfc3339_unix),
                window_minutes: Some(7 * 24 * 60),
                severity: None,
                active: None,
            });
        }
    } else {
        for limit in parsed.limits {
            let kind = limit.kind.trim().to_owned();
            if kind.is_empty() {
                continue;
            }
            let window_minutes = match kind.as_str() {
                "session" => Some(300),
                "weekly_all" | "weekly_scoped" => Some(7 * 24 * 60),
                _ => None,
            };
            windows.push(AccountUsageWindow {
                scope_model: limit
                    .scope
                    .and_then(|scope| scope.model)
                    .and_then(|model| model.display_name)
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty()),
                used_percent: limit.percent,
                resets_at_unix: limit.resets_at.as_deref().and_then(parse_rfc3339_unix),
                window_minutes,
                severity: limit
                    .severity
                    .map(|severity| severity.trim().to_owned())
                    .filter(|severity| !severity.is_empty()),
                active: limit.is_active,
                kind,
            });
        }
    }
    if windows.is_empty() {
        return Err("usage response carried no windows".into());
    }
    Ok(AccountUsageMeter {
        provider: AccountUsageProvider::Claude,
        plan_type: None,
        windows,
        fetched_at_unix: now_unix,
        source: CLAUDE_SOURCE,
    })
}

fn parse_rfc3339_unix(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp()).ok()
}

fn claude_token_from_credentials_json(raw: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Credentials {
        #[serde(rename = "claudeAiOauth")]
        claude_ai_oauth: Option<OauthCredentials>,
    }
    #[derive(Deserialize)]
    struct OauthCredentials {
        #[serde(rename = "accessToken")]
        access_token: Option<String>,
    }
    let parsed: Credentials = serde_json::from_slice(raw).ok()?;
    let token = parsed.claude_ai_oauth?.access_token?;
    let token = token.trim();
    // The token is written into a curl config line; anything that could break
    // out of that quoted value is not a token we want to send.
    (!token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\'))
    .then(|| token.to_owned())
}

/// Resolves the Claude Code OAuth token: the credentials file first, then the
/// macOS Keychain item Claude Code writes. The token stays in memory only.
async fn claude_oauth_token() -> Result<String, FetchError> {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let path = std::path::PathBuf::from(home)
            .join(".claude")
            .join(".credentials.json");
        if let Ok(raw) = std::fs::read(&path) {
            if let Some(token) = claude_token_from_credentials_json(&raw) {
                return Ok(token);
            }
        }
    }
    let keychain = tokio::task::spawn_blocking(crate::platform::claude_code_keychain_credentials)
        .await
        .ok()
        .flatten();
    keychain
        .as_deref()
        .and_then(claude_token_from_credentials_json)
        .ok_or_else(|| FetchError::Unavailable("no Claude Code OAuth credentials found".into()))
}

struct HttpResponse {
    status: u16,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

/// Splits `curl -i` output into the final status, `Retry-After`, and body.
/// Intermediate header blocks (`100 Continue`, redirects) are skipped.
fn parse_http_response(raw: &[u8], now: OffsetDateTime) -> Result<HttpResponse, String> {
    let mut rest = raw;
    loop {
        let header_end = find(rest, b"\r\n\r\n")
            .map(|index| (index, 4))
            .or_else(|| find(rest, b"\n\n").map(|index| (index, 2)))
            .ok_or_else(|| "response carried no header terminator".to_owned())?;
        let (header_bytes, body) = rest.split_at(header_end.0);
        let body = &body[header_end.1..];
        let headers = String::from_utf8_lossy(header_bytes);
        let mut lines = headers.lines();
        let status_line = lines.next().unwrap_or_default();
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| "response carried no HTTP status".to_owned())?;
        if (100..200).contains(&status) || body.starts_with(b"HTTP/") {
            rest = body;
            continue;
        }
        let retry_after = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| parse_retry_after(value.trim(), now));
        return Ok(HttpResponse {
            status,
            retry_after,
            body: body.to_vec(),
        });
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_retry_after(value: &str, now: OffsetDateTime) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return (seconds > 0 && seconds <= RETRY_AFTER_MAX.as_secs())
            .then(|| Duration::from_secs(seconds));
    }
    let at = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822).ok()?;
    let delay = at - now;
    (delay.is_positive() && delay <= RETRY_AFTER_MAX)
        .then(|| Duration::from_secs(delay.whole_seconds().max(0) as u64))
}

async fn fetch_claude(now_unix: u64) -> Result<AccountUsageMeter, FetchError> {
    let token = claude_oauth_token().await?;
    let mut command = tokio::process::Command::from(crate::noninteractive_process::curl_command());
    command
        .args([
            "-sS",
            "-i",
            "--connect-timeout",
            "10",
            "--max-time",
            "20",
            "--max-filesize",
            &MAX_RESPONSE_BYTES.to_string(),
            "--config",
            "-",
            CLAUDE_USAGE_URL,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            FetchError::Unavailable("curl is not installed".into())
        } else {
            FetchError::Failed(format!("curl failed to start: {err}"))
        }
    })?;
    // The token travels through curl's config on stdin so it never appears in
    // the process argument list.
    let config = format!(
        "header = \"Authorization: Bearer {token}\"\nheader = \"anthropic-beta: {CLAUDE_BETA_HEADER}\"\n"
    );
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .await
            .map_err(|err| FetchError::Failed(format!("curl stdin: {err}")))?;
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|err| FetchError::Failed(format!("curl: {err}")))?;
    if !output.status.success() {
        return Err(FetchError::Failed(format!(
            "curl exited with {}",
            output.status
        )));
    }
    let response = parse_http_response(&output.stdout, OffsetDateTime::now_utc())
        .map_err(FetchError::Failed)?;
    match response.status {
        200 => parse_claude_usage(&response.body, now_unix).map_err(FetchError::Failed),
        429 => Err(FetchError::RateLimited {
            retry_after: response.retry_after,
        }),
        // The body is never included: on auth errors it can echo request context.
        status => Err(FetchError::Failed(format!(
            "usage endpoint status {status}"
        ))),
    }
}

// --- Codex ------------------------------------------------------------------

#[derive(Deserialize)]
struct CodexRateLimitsResult {
    #[serde(rename = "rateLimits")]
    rate_limits: Option<CodexRateLimits>,
}

#[derive(Deserialize)]
struct CodexRateLimits {
    #[serde(rename = "planType")]
    plan_type: Option<String>,
    primary: Option<CodexRateLimitWindow>,
    secondary: Option<CodexRateLimitWindow>,
}

/// Field shapes vary across Codex builds: the window length arrives as
/// `windowDurationMins` (older builds used `windowMinutes`) and `resetsAt` is a
/// Unix-epoch number on current builds and an RFC 3339 string on older ones.
#[derive(Deserialize)]
struct CodexRateLimitWindow {
    #[serde(rename = "usedPercent")]
    used_percent: Option<f64>,
    #[serde(rename = "windowDurationMins")]
    window_duration_mins: Option<u32>,
    #[serde(rename = "windowMinutes")]
    window_minutes: Option<u32>,
    #[serde(rename = "resetsAt")]
    resets_at: Option<serde_json::Value>,
}

impl CodexRateLimitWindow {
    fn into_window(self, kind: &str) -> Option<AccountUsageWindow> {
        let used_percent = self.used_percent?;
        let resets_at_unix = match self.resets_at {
            Some(serde_json::Value::Number(number)) => number
                .as_f64()
                .filter(|value| *value > 0.0)
                .map(|value| value as u64),
            Some(serde_json::Value::String(value)) => parse_rfc3339_unix(&value),
            _ => None,
        };
        Some(AccountUsageWindow {
            kind: kind.into(),
            scope_model: None,
            used_percent,
            resets_at_unix,
            window_minutes: self
                .window_duration_mins
                .or(self.window_minutes)
                .filter(|minutes| *minutes > 0),
            severity: None,
            active: None,
        })
    }
}

pub fn parse_codex_rate_limits(
    result: &serde_json::Value,
    now_unix: u64,
) -> Result<AccountUsageMeter, String> {
    let parsed: CodexRateLimitsResult = serde_json::from_value(result.clone())
        .map_err(|err| format!("invalid rate limits result: {err}"))?;
    let limits = parsed
        .rate_limits
        .ok_or_else(|| "rate limits result carried no rateLimits".to_owned())?;
    let windows = [
        limits
            .primary
            .and_then(|window| window.into_window("primary")),
        limits
            .secondary
            .and_then(|window| window.into_window("secondary")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if windows.is_empty() {
        return Err("codex rate limits carried no windows".into());
    }
    Ok(AccountUsageMeter {
        provider: AccountUsageProvider::Codex,
        plan_type: limits
            .plan_type
            .map(|plan| plan.trim().to_owned())
            .filter(|plan| !plan.is_empty()),
        windows,
        fetched_at_unix: now_unix,
        source: CODEX_SOURCE,
    })
}

async fn fetch_codex(now_unix: u64) -> Result<AccountUsageMeter, FetchError> {
    let mut command =
        tokio::process::Command::from(crate::noninteractive_process::command("codex"));
    command
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            FetchError::Unavailable("codex is not installed".into())
        } else {
            FetchError::Failed(format!("codex app-server failed to start: {err}"))
        }
    })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| FetchError::Failed("codex app-server stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| FetchError::Failed("codex app-server stdout unavailable".into()))?;
    let mut stdout = tokio::io::BufReader::new(stdout);
    let result = tokio::time::timeout(
        CODEX_TIMEOUT,
        codex_rate_limits_handshake(&mut stdin, &mut stdout),
    )
    .await
    .unwrap_or_else(|_| Err(FetchError::Failed("codex app-server timed out".into())));
    let _ = child.kill().await;
    let result = result?;
    parse_codex_rate_limits(&result, now_unix).map_err(FetchError::Failed)
}

/// Performs `initialize` -> `initialized` -> `account/rateLimits/read` over
/// newline-delimited JSON-RPC and returns the read result. Separate from the
/// process wrapper so the wire handling is testable against crafted streams.
async fn codex_rate_limits_handshake<W, R>(
    stdin: &mut W,
    stdout: &mut R,
) -> Result<serde_json::Value, FetchError>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    async fn send<W: AsyncWrite + Unpin>(
        stdin: &mut W,
        message: serde_json::Value,
    ) -> Result<(), FetchError> {
        let mut line = message.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| FetchError::Failed(format!("codex app-server stdin: {err}")))
    }

    async fn wait_for<R: AsyncBufRead + Unpin>(
        stdout: &mut R,
        id: u64,
        total_read: &mut usize,
    ) -> Result<serde_json::Value, FetchError> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = stdout
                .read_line(&mut line)
                .await
                .map_err(|err| FetchError::Failed(format!("codex app-server stream: {err}")))?;
            if read == 0 {
                return Err(FetchError::Failed(
                    "codex app-server stream ended before response".into(),
                ));
            }
            *total_read = total_read.saturating_add(read);
            if *total_read > MAX_RESPONSE_BYTES {
                return Err(FetchError::Failed(
                    "codex app-server stream exceeded the size limit".into(),
                ));
            }
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if message.get("error").is_some() {
                // Error payloads are never echoed: they can carry account context.
                return Err(FetchError::Failed(
                    "codex app-server returned an error".into(),
                ));
            }
            return Ok(message
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    let mut total_read = 0usize;
    send(
        stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "herdr",
                    "title": "Herdr account usage",
                    "version": crate::build_info::version(),
                },
                "capabilities": {"experimentalApi": true},
            },
        }),
    )
    .await?;
    wait_for(stdout, 1, &mut total_read).await?;
    send(
        stdin,
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized"}),
    )
    .await?;
    send(
        stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "account/rateLimits/read",
            "params": {},
        }),
    )
    .await?;
    wait_for(stdout, 2, &mut total_read).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_757_800_000;

    #[test]
    fn claude_limits_shape_maps_every_window() {
        let body = br#"{
            "five_hour": {"utilization": 2, "resets_at": "2026-09-14T00:19:59.711737+00:00"},
            "seven_day": {"utilization": 1, "resets_at": "2026-09-20T11:59:59.711758+00:00"},
            "limits": [
                {"kind": "session", "percent": 2, "severity": "normal", "resets_at": "2026-09-14T00:19:59.711737+00:00", "is_active": true},
                {"kind": "weekly_all", "percent": 1, "severity": "normal", "resets_at": "2026-09-20T11:59:59.711758+00:00", "is_active": false},
                {"kind": "weekly_scoped", "percent": 2, "severity": "normal", "resets_at": "2026-09-20T11:59:59.711967+00:00", "is_active": false, "scope": {"model": {"display_name": "Fable"}}}
            ],
            "extra_usage": {"is_enabled": false, "monthly_limit": 20000, "used_credits": 0, "decimal_places": 2}
        }"#;
        let meter = parse_claude_usage(body, NOW).unwrap();
        assert_eq!(meter.provider, AccountUsageProvider::Claude);
        assert_eq!(meter.source, CLAUDE_SOURCE);
        assert_eq!(meter.fetched_at_unix, NOW);
        let kinds = meter
            .windows
            .iter()
            .map(|window| window.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["session", "weekly_all", "weekly_scoped"]);
        assert_eq!(meter.windows[0].window_minutes, Some(300));
        assert_eq!(meter.windows[0].resets_at_unix, Some(1_789_345_199));
        assert_eq!(meter.windows[0].active, Some(true));
        assert_eq!(meter.windows[2].scope_model.as_deref(), Some("Fable"));
        assert_eq!(meter.windows[2].window_minutes, Some(10_080));
        assert_eq!(window_label(&meter.windows[0]), "5h");
        assert_eq!(window_label(&meter.windows[1]), "week");
        assert_eq!(window_label(&meter.windows[2]), "fable");
    }

    #[test]
    fn claude_legacy_shape_uses_top_level_windows() {
        let body = br#"{"five_hour": {"utilization": 40.5, "resets_at": "2026-09-14T00:19:59Z"}, "seven_day": {"utilization": 12}}"#;
        let meter = parse_claude_usage(body, NOW).unwrap();
        assert_eq!(meter.windows.len(), 2);
        assert_eq!(meter.windows[0].kind, "session");
        assert_eq!(meter.windows[0].used_percent, 40.5);
        assert_eq!(meter.windows[1].kind, "weekly_all");
        assert_eq!(meter.windows[1].resets_at_unix, None);
    }

    #[test]
    fn claude_response_without_windows_is_an_error() {
        assert!(parse_claude_usage(b"{}", NOW).is_err());
        assert!(parse_claude_usage(b"not json", NOW).is_err());
    }

    #[test]
    fn codex_rate_limits_tolerate_both_wire_shapes() {
        let current = serde_json::json!({
            "rateLimits": {
                "planType": "pro",
                "primary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1_789_810_366},
                "secondary": null,
                "credits": {"hasCredits": false, "unlimited": false, "balance": "0"}
            }
        });
        let meter = parse_codex_rate_limits(&current, NOW).unwrap();
        assert_eq!(meter.plan_type.as_deref(), Some("pro"));
        assert_eq!(meter.windows.len(), 1);
        assert_eq!(meter.windows[0].kind, "primary");
        assert_eq!(meter.windows[0].window_minutes, Some(10_080));
        assert_eq!(meter.windows[0].resets_at_unix, Some(1_789_810_366));
        assert_eq!(window_label(&meter.windows[0]), "week");

        let older = serde_json::json!({
            "rateLimits": {
                "primary": {"usedPercent": 5.5, "windowMinutes": 300, "resetsAt": "2026-09-14T00:19:59Z"},
                "secondary": {"usedPercent": 40, "windowMinutes": 10080}
            }
        });
        let meter = parse_codex_rate_limits(&older, NOW).unwrap();
        assert_eq!(meter.plan_type, None);
        assert_eq!(meter.windows[0].resets_at_unix, Some(1_789_345_199));
        assert_eq!(window_label(&meter.windows[0]), "5h");
        assert_eq!(meter.windows[1].resets_at_unix, None);
    }

    #[test]
    fn codex_result_without_windows_is_an_error() {
        assert!(parse_codex_rate_limits(&serde_json::json!({}), NOW).is_err());
        assert!(parse_codex_rate_limits(&serde_json::json!({"rateLimits": {}}), NOW).is_err());
    }

    #[test]
    fn http_response_parsing_skips_continue_blocks_and_reads_retry_after() {
        let now = OffsetDateTime::from_unix_timestamp(NOW as i64).unwrap();
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2 429 \r\ncontent-type: application/json\r\nRetry-After: 120\r\n\r\n{\"error\":1}";
        let response = parse_http_response(raw, now).unwrap();
        assert_eq!(response.status, 429);
        assert_eq!(response.retry_after, Some(Duration::from_secs(120)));
        assert_eq!(response.body, b"{\"error\":1}");

        let raw = b"HTTP/2 200\r\nretry-after: 99999999999\r\n\r\n{}";
        let response = parse_http_response(raw, now).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.retry_after, None);

        assert!(parse_http_response(b"garbage", now).is_err());
    }

    #[test]
    fn credentials_json_yields_only_safe_tokens() {
        assert_eq!(
            claude_token_from_credentials_json(
                br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc","refreshToken":"x"}}"#
            )
            .as_deref(),
            Some("sk-ant-oat01-abc")
        );
        assert_eq!(
            claude_token_from_credentials_json(
                br#"{"claudeAiOauth":{"accessToken":"bad\"quote"}}"#
            ),
            None
        );
        assert_eq!(claude_token_from_credentials_json(b"{}"), None);
    }

    #[test]
    fn backoff_doubles_until_the_cap() {
        assert_eq!(backoff_delay(0), POLL_INTERVAL);
        assert_eq!(backoff_delay(1), POLL_INTERVAL * 2);
        assert_eq!(backoff_delay(3), POLL_INTERVAL * 8);
        assert_eq!(backoff_delay(9), BACKOFF_MAX);
    }

    #[test]
    fn staleness_uses_the_fetch_age_and_rejects_future_readings() {
        let meter = AccountUsageMeter {
            provider: AccountUsageProvider::Codex,
            plan_type: None,
            windows: Vec::new(),
            fetched_at_unix: NOW,
            source: CODEX_SOURCE,
        };
        assert!(!meter.is_stale(NOW + 60));
        assert!(!meter.is_stale(NOW + STALE_AFTER.as_secs()));
        assert!(meter.is_stale(NOW + STALE_AFTER.as_secs() + 1));
        assert!(meter.is_stale(NOW - 120));
    }

    #[test]
    fn countdown_and_remaining_formatting() {
        assert_eq!(format_countdown(30), "1m");
        assert_eq!(format_countdown(12 * 60), "12m");
        assert_eq!(format_countdown(2 * 3600 + 5 * 60), "2h");
        assert_eq!(format_countdown(3 * 86_400 + 4 * 3600), "3d");
        assert_eq!(remaining_percent(38.4), 62);
        assert_eq!(remaining_percent(130.0), 0);
        assert_eq!(remaining_percent(f64::NAN), 0);
    }

    #[tokio::test]
    async fn codex_handshake_demuxes_notifications_and_ids() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (server_reader, mut server_writer) = tokio::io::split(server_side);
        let (client_reader, mut client_writer) = tokio::io::split(client_side);
        let server = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(server_reader).lines();
            let first = lines.next_line().await.unwrap().unwrap();
            assert!(first.contains("\"method\":\"initialize\""));
            server_writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"noise\",\"params\":{}}\nnot json\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"userAgent\":\"codex\"}}\n")
                .await
                .unwrap();
            let second = lines.next_line().await.unwrap().unwrap();
            assert!(second.contains("\"method\":\"initialized\""));
            let third = lines.next_line().await.unwrap().unwrap();
            assert!(third.contains("account/rateLimits/read"));
            server_writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"rateLimits\":{\"planType\":\"pro\",\"primary\":{\"usedPercent\":18,\"windowDurationMins\":10080,\"resetsAt\":1789810366}}}}\n")
                .await
                .unwrap();
        });
        let mut reader = tokio::io::BufReader::new(client_reader);
        let result = codex_rate_limits_handshake(&mut client_writer, &mut reader)
            .await
            .unwrap();
        server.await.unwrap();
        let meter = parse_codex_rate_limits(&result, NOW).unwrap();
        assert_eq!(meter.plan_type.as_deref(), Some("pro"));
        assert_eq!(meter.windows[0].used_percent, 18.0);
    }

    #[tokio::test]
    async fn codex_handshake_redacts_rpc_errors() {
        let (client_side, server_side) = tokio::io::duplex(8 * 1024);
        let (server_reader, mut server_writer) = tokio::io::split(server_side);
        let (client_reader, mut client_writer) = tokio::io::split(client_side);
        let server = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(server_reader).lines();
            let _ = lines.next_line().await.unwrap();
            server_writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"account secret-ish detail\"}}\n")
                .await
                .unwrap();
        });
        let mut reader = tokio::io::BufReader::new(client_reader);
        let error = codex_rate_limits_handshake(&mut client_writer, &mut reader)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(
            error,
            FetchError::Failed("codex app-server returned an error".into())
        );
    }
}
