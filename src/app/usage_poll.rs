//! Scheduling for the provider account usage poller.
//!
//! The fetches themselves live in `crate::account_usage`; this module owns the
//! per-provider cadence, backoff, and the hand-off of results into `AppState`.

use std::time::Instant;

use super::App;
use crate::account_usage::{self, AccountUsageMeter, AccountUsageProvider, FetchError};
use crate::events::AppEvent;

struct ProviderPoll {
    provider: AccountUsageProvider,
    failures: u32,
    next_at: Instant,
    task: Option<tokio::task::AbortHandle>,
    unavailable_logged: bool,
    /// The first successful reading has been logged.
    announced: bool,
}

pub(crate) struct Runtime {
    enabled: bool,
    generation: u64,
    providers: Vec<ProviderPoll>,
}

impl Runtime {
    pub(super) fn new(enabled: bool) -> Self {
        let mut runtime = Self {
            enabled: false,
            generation: 0,
            providers: Vec::new(),
        };
        runtime.set_enabled(enabled, Instant::now());
        runtime
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool, now: Instant) {
        self.abort_tasks();
        self.generation = self.generation.wrapping_add(1);
        self.enabled = enabled;
        self.providers = if enabled {
            AccountUsageProvider::ALL
                .into_iter()
                .map(|provider| ProviderPoll {
                    provider,
                    failures: 0,
                    next_at: now + account_usage::INITIAL_DELAY,
                    task: None,
                    unavailable_logged: false,
                    announced: false,
                })
                .collect()
        } else {
            Vec::new()
        };
    }

    fn abort_tasks(&mut self) {
        for poll in &mut self.providers {
            if let Some(task) = poll.task.take() {
                task.abort();
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.abort_tasks();
    }
}

impl App {
    /// Applies the config toggle. Returns true when the visible readings changed.
    pub(crate) fn configure_account_usage(&mut self, enabled: bool) -> bool {
        let enabled = enabled && self.policy.background_updates;
        if enabled == self.usage_poll.enabled {
            return false;
        }
        self.usage_poll.set_enabled(enabled, Instant::now());
        if !enabled && !self.state.account_usage.is_empty() {
            self.state.account_usage.clear();
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            return true;
        }
        false
    }

    /// Starts any due provider fetch. Results arrive as `AppEvent::AccountUsageFetched`.
    pub(crate) fn handle_account_usage_tasks(&mut self, now: Instant) {
        if !self.usage_poll.enabled {
            return;
        }
        let generation = self.usage_poll.generation;
        for poll in &mut self.usage_poll.providers {
            if poll.task.is_some() || now < poll.next_at {
                continue;
            }
            let event_tx = self.event_tx.clone();
            let provider = poll.provider;
            let task = tokio::spawn(async move {
                let result = account_usage::fetch(provider, account_usage::unix_now()).await;
                let _ = event_tx
                    .send(AppEvent::AccountUsageFetched {
                        generation,
                        provider,
                        result,
                    })
                    .await;
            });
            poll.task = Some(task.abort_handle());
        }
    }

    pub(super) fn handle_account_usage_fetched(
        &mut self,
        generation: u64,
        provider: AccountUsageProvider,
        result: Result<AccountUsageMeter, FetchError>,
    ) -> bool {
        if generation != self.usage_poll.generation {
            return false;
        }
        let Some(poll) = self
            .usage_poll
            .providers
            .iter_mut()
            .find(|poll| poll.provider == provider)
        else {
            return false;
        };
        poll.task = None;
        let now = Instant::now();
        let changed = match result {
            Ok(meter) => {
                if poll.failures > 0 || !poll.announced {
                    tracing::info!(
                        provider = provider.label(),
                        source = meter.source,
                        windows = meter.windows.len(),
                        "account usage reading received"
                    );
                }
                poll.announced = true;
                poll.failures = 0;
                poll.unavailable_logged = false;
                poll.next_at = now + account_usage::POLL_INTERVAL;
                self.state.replace_account_usage(meter)
            }
            Err(FetchError::Unavailable(reason)) => {
                poll.failures = poll.failures.saturating_add(1);
                poll.next_at = now + account_usage::BACKOFF_MAX;
                if !poll.unavailable_logged {
                    poll.unavailable_logged = true;
                    tracing::info!(
                        provider = provider.label(),
                        reason,
                        "account usage polling inactive"
                    );
                }
                false
            }
            Err(FetchError::RateLimited { retry_after }) => {
                poll.failures = poll.failures.saturating_add(1);
                let delay = account_usage::backoff_delay(poll.failures)
                    .max(retry_after.unwrap_or_default());
                poll.next_at = now + delay;
                tracing::warn!(
                    provider = provider.label(),
                    retry_seconds = delay.as_secs(),
                    "account usage poll rate limited"
                );
                false
            }
            Err(FetchError::Failed(reason)) => {
                poll.failures = poll.failures.saturating_add(1);
                poll.next_at = now + account_usage::backoff_delay(poll.failures);
                tracing::warn!(
                    provider = provider.label(),
                    reason,
                    failures = poll.failures,
                    "account usage poll failed"
                );
                false
            }
        };
        // A retained reading crosses the stale threshold without any event of
        // its own; a failed retry is the moment to let clients dim it.
        let now_unix = account_usage::unix_now();
        let changed = changed
            || self
                .state
                .account_usage
                .iter()
                .any(|meter| meter.provider == provider && meter.is_stale(now_unix));
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }
}

impl super::AppState {
    /// Stores a fresh reading, keeping one entry per provider in provider order.
    pub(crate) fn replace_account_usage(&mut self, meter: AccountUsageMeter) -> bool {
        if let Some(existing) = self
            .account_usage
            .iter_mut()
            .find(|existing| existing.provider == meter.provider)
        {
            if *existing == meter {
                return false;
            }
            *existing = meter;
        } else {
            self.account_usage.push(meter);
            self.account_usage.sort_by_key(|meter| {
                AccountUsageProvider::ALL
                    .iter()
                    .position(|provider| *provider == meter.provider)
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_usage::AccountUsageWindow;
    use crate::app::AppState;

    fn meter(provider: AccountUsageProvider, used: f64) -> AccountUsageMeter {
        AccountUsageMeter {
            provider,
            plan_type: None,
            windows: vec![AccountUsageWindow {
                kind: "session".into(),
                scope_model: None,
                used_percent: used,
                resets_at_unix: None,
                window_minutes: Some(300),
                severity: None,
                active: None,
            }],
            fetched_at_unix: 1_757_800_000,
            source: "test",
        }
    }

    #[test]
    fn readings_are_kept_one_per_provider_in_provider_order() {
        let mut state = AppState::test_new();
        assert!(state.replace_account_usage(meter(AccountUsageProvider::Codex, 10.0)));
        assert!(state.replace_account_usage(meter(AccountUsageProvider::Claude, 20.0)));
        assert_eq!(
            state
                .account_usage
                .iter()
                .map(|meter| meter.provider)
                .collect::<Vec<_>>(),
            vec![AccountUsageProvider::Claude, AccountUsageProvider::Codex]
        );
        assert!(!state.replace_account_usage(meter(AccountUsageProvider::Claude, 20.0)));
        assert!(state.replace_account_usage(meter(AccountUsageProvider::Claude, 25.0)));
        assert_eq!(state.account_usage.len(), 2);
        assert_eq!(state.account_usage[0].windows[0].used_percent, 25.0);
    }

    #[test]
    fn disabled_runtime_schedules_nothing() {
        let runtime = Runtime::new(false);
        assert!(!runtime.enabled());
        assert!(runtime.providers.is_empty());
        let runtime = Runtime::new(true);
        assert!(runtime.enabled());
        assert_eq!(runtime.providers.len(), AccountUsageProvider::ALL.len());
    }
}
