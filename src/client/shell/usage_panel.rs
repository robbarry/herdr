//! Sidebar footer showing remaining provider subscription capacity.
//!
//! Each provider gets a header row, then one row per rate-limit window the
//! endpoint reported: a label, a bar filled with the share already used with
//! a tick at "now" and colored by whether usage runs behind, at, or ahead of
//! the window, and the time until the window resets when the sidebar is wide
//! enough. Stale readings render dimmed so a paused poller is visible.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use super::render::{display_width, put_right_text, put_text};
use super::{ClientShellConfig, ClientShellSnapshot};
use crate::app::state::Palette;

/// Rows the sections above must keep before the panel takes space.
const MIN_SECTIONS_HEIGHT: u16 = 12;
const MAX_WINDOWS_PER_PROVIDER: usize = 4;
const MAX_ROWS: usize = 10;
const HEADER_ROWS: u16 = 2;
const TOGGLE_ROWS: u16 = 1;
const MIN_BAR_WIDTH: u16 = 4;
const FILLED: &str = "█";
const EMPTY: &str = "░";
const TICK: &str = "│";

enum UsageRow<'a> {
    Provider {
        name: &'a str,
        plan_type: Option<&'a str>,
        stale: bool,
    },
    Window {
        label: &'a str,
        remaining_percent: u8,
        resets_in_seconds: Option<u64>,
        window_minutes: Option<u32>,
        severity: Option<&'a str>,
        stale: bool,
    },
}

fn rows(snapshot: &ClientShellSnapshot) -> Vec<UsageRow<'_>> {
    snapshot
        .account_usage
        .iter()
        .filter(|meter| !meter.windows.is_empty())
        .flat_map(|meter| {
            std::iter::once(UsageRow::Provider {
                name: &meter.provider,
                plan_type: meter.plan_type.as_deref(),
                stale: meter.stale,
            })
            .chain(meter.windows.iter().take(MAX_WINDOWS_PER_PROVIDER).map(
                move |window| UsageRow::Window {
                    label: &window.label,
                    remaining_percent: window.remaining_percent.min(100),
                    resets_in_seconds: window.resets_in_seconds,
                    window_minutes: window.window_minutes,
                    severity: window.severity.as_deref(),
                    stale: meter.stale,
                },
            ))
        })
        .take(MAX_ROWS)
        .collect()
}

pub(super) fn panel_height(snapshot: &ClientShellSnapshot) -> u16 {
    let rows = rows(snapshot).len();
    if rows == 0 {
        0
    } else {
        HEADER_ROWS + rows as u16 + TOGGLE_ROWS
    }
}

/// Splits the sidebar into the area the sections keep and the panel footer.
/// The footer is empty when there is nothing to show or too little height.
pub(super) fn reserve(area: Rect, snapshot: &ClientShellSnapshot) -> (Rect, Rect) {
    let height = panel_height(snapshot);
    if height == 0 || area.height < height + MIN_SECTIONS_HEIGHT {
        return (area, Rect::default());
    }
    (
        Rect::new(area.x, area.y, area.width, area.height - height),
        Rect::new(area.x, area.bottom() - height, area.width, height),
    )
}

/// Below this much remaining the window is nearly spent whatever the pace.
const EXHAUSTED_REMAINING_PERCENT: u8 = 10;
/// Used share may trail the elapsed share by this much and still count as
/// "at pace" rather than comfortably behind it.
const AT_PACE_BEHIND_PERCENT: f64 = 3.0;
/// Used share may lead the elapsed share by this much before it is clearly
/// burning faster than the window.
const AT_PACE_AHEAD_PERCENT: f64 = 5.0;

/// Share of the window already elapsed, 0.0 at reset and 1.0 just before the
/// next reset. None when the provider did not report the window length.
fn elapsed_share(resets_in_seconds: Option<u64>, window_minutes: Option<u32>) -> Option<f64> {
    let window_seconds = f64::from(window_minutes.filter(|minutes| *minutes > 0)?) * 60.0;
    let remaining = resets_in_seconds? as f64;
    Some((1.0 - remaining / window_seconds).clamp(0.0, 1.0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// Used share is behind where the window is: safe.
    Behind,
    /// Used share matches the window: this rate hits the limit at reset.
    Even,
    /// Used share is ahead of the window or the window is nearly spent.
    Ahead,
}

fn pace(remaining_percent: u8, elapsed: Option<f64>, severity: Option<&str>) -> Pace {
    if remaining_percent < EXHAUSTED_REMAINING_PERCENT {
        return Pace::Ahead;
    }
    let elevated = severity.is_some_and(|severity| !severity.eq_ignore_ascii_case("normal"));
    let Some(elapsed) = elapsed else {
        // Without a window length fall back to how much is left.
        return match remaining_percent {
            0..=20 => Pace::Ahead,
            21..=50 => Pace::Even,
            _ if elevated => Pace::Even,
            _ => Pace::Behind,
        };
    };
    let used = f64::from(100 - remaining_percent);
    let delta = used - elapsed * 100.0;
    if delta > AT_PACE_AHEAD_PERCENT {
        Pace::Ahead
    } else if delta >= -AT_PACE_BEHIND_PERCENT || elevated {
        Pace::Even
    } else {
        Pace::Behind
    }
}

fn pace_color(pace: Pace, stale: bool, palette: &Palette) -> Color {
    if stale {
        return palette.overlay0;
    }
    match pace {
        Pace::Behind => palette.green,
        Pace::Even => palette.yellow,
        Pace::Ahead => palette.red,
    }
}

pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
) {
    // Keep the divider column and a dedicated bottom row for the sidebar's
    // collapse toggle, so every usage bar gets the same available width.
    let content = Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(1),
        area.height.saturating_sub(TOGGLE_ROWS),
    );
    if content.is_empty() {
        return;
    }
    let palette = &config.palette;
    let rows = rows(snapshot);
    put_text(
        buffer,
        content.x,
        content.y,
        content.width,
        &"─".repeat(content.width as usize),
        Style::default().fg(palette.surface_dim),
    );
    if content.height < 2 {
        return;
    }
    put_text(
        buffer,
        content.x,
        content.y + 1,
        content.width,
        " usage",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );

    let label_width = rows
        .iter()
        .filter_map(|row| match row {
            UsageRow::Window { label, .. } => Some(display_width(label)),
            UsageRow::Provider { .. } => None,
        })
        .max()
        .unwrap_or(0)
        .min(content.width / 3);
    for (index, row) in rows.iter().enumerate() {
        let y = content.y + HEADER_ROWS + index as u16;
        if y >= content.bottom() {
            break;
        }
        let row_rect = Rect::new(content.x, y, content.width, 1);
        match row {
            UsageRow::Provider {
                name,
                plan_type,
                stale,
            } => {
                put_text(
                    buffer,
                    row_rect.x + 1,
                    y,
                    row_rect.width.saturating_sub(1),
                    name,
                    Style::default().fg(if *stale {
                        palette.overlay0
                    } else {
                        palette.text
                    }),
                );
                if *stale {
                    put_right_text(
                        buffer,
                        row_rect,
                        y,
                        "stale",
                        Style::default().fg(palette.yellow),
                    );
                } else if let Some(plan) = plan_type {
                    put_right_text(
                        buffer,
                        row_rect,
                        y,
                        plan,
                        Style::default().fg(palette.overlay0),
                    );
                }
            }
            UsageRow::Window {
                label,
                remaining_percent,
                resets_in_seconds,
                window_minutes,
                severity,
                stale,
            } => {
                let elapsed = elapsed_share(*resets_in_seconds, *window_minutes);
                let color = pace_color(
                    pace(*remaining_percent, elapsed, *severity),
                    *stale,
                    palette,
                );
                put_text(
                    buffer,
                    row_rect.x + 2,
                    y,
                    label_width,
                    label,
                    Style::default().fg(if *stale {
                        palette.overlay0
                    } else {
                        palette.subtext0
                    }),
                );
                let bar_x = row_rect.x + 2 + label_width + 1;
                // The bar has priority over the countdown: show the reset
                // time only when a readable bar still fits beside it.
                let countdown = resets_in_seconds
                    .map(|seconds| {
                        format!("{:>3}", crate::account_usage::format_countdown(seconds))
                    })
                    .filter(|countdown| {
                        row_rect.right().saturating_sub(bar_x)
                            >= MIN_BAR_WIDTH + 1 + display_width(countdown)
                    })
                    .unwrap_or_default();
                let countdown_width = display_width(&countdown);
                let countdown_style = Style::default().fg(if *stale {
                    palette.overlay0
                } else {
                    palette.subtext0
                });
                put_right_text(buffer, row_rect, y, &countdown, countdown_style);
                let bar_end = if countdown_width == 0 {
                    row_rect.right()
                } else {
                    row_rect.right().saturating_sub(countdown_width + 1)
                };
                let bar_width = bar_end.saturating_sub(bar_x);
                if bar_width < MIN_BAR_WIDTH {
                    continue;
                }
                // The bar fills with the share already used, like the
                // providers' own meters; its color says whether that pace
                // outruns the window. A tick marks where "now" falls in it.
                let used_percent = 100 - remaining_percent;
                let filled = (u32::from(bar_width) * u32::from(used_percent) / 100) as u16;
                let bar = format!(
                    "{}{}",
                    FILLED.repeat(filled as usize),
                    EMPTY.repeat((bar_width - filled) as usize)
                );
                put_text(
                    buffer,
                    bar_x,
                    y,
                    bar_width,
                    &bar,
                    Style::default().fg(color),
                );
                if let Some(elapsed) = elapsed {
                    let tick = ((f64::from(bar_width) * elapsed) as u16).min(bar_width - 1);
                    put_text(
                        buffer,
                        bar_x + tick,
                        y,
                        1,
                        TICK,
                        Style::default().fg(if *stale {
                            palette.overlay1
                        } else {
                            palette.text
                        }),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientShellAccountUsage, ClientShellAccountUsageWindow};

    fn snapshot(meters: Vec<ClientShellAccountUsage>) -> ClientShellSnapshot {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.account_usage = meters;
        snapshot
    }

    fn window(
        label: &str,
        remaining: u8,
        resets: Option<u64>,
        window_minutes: Option<u32>,
    ) -> ClientShellAccountUsageWindow {
        ClientShellAccountUsageWindow {
            label: label.into(),
            remaining_percent: remaining,
            resets_in_seconds: resets,
            severity: None,
            window_minutes,
        }
    }

    #[test]
    fn pace_compares_used_share_with_elapsed_share() {
        // 38% used, 58% of the window gone: comfortably behind.
        assert_eq!(pace(62, Some(0.58), None), Pace::Behind);
        // 60% used, 58% gone: at pace.
        assert_eq!(pace(40, Some(0.58), None), Pace::Even);
        // 56% used, 58% gone: still at pace (within the behind margin).
        assert_eq!(pace(44, Some(0.58), None), Pace::Even);
        // 85% used, 57% gone: ahead.
        assert_eq!(pace(15, Some(0.57), None), Pace::Ahead);
        // Nearly spent is red however the pace looks.
        assert_eq!(pace(9, Some(0.99), None), Pace::Ahead);
        // Unknown window length falls back to what is left.
        assert_eq!(pace(80, None, None), Pace::Behind);
        assert_eq!(pace(30, None, None), Pace::Even);
        assert_eq!(pace(15, None, None), Pace::Ahead);
        // A provider-flagged severity never reads as comfortable.
        assert_eq!(pace(90, Some(0.1), Some("warning")), Pace::Even);

        assert_eq!(
            elapsed_share(Some(7_500), Some(300)),
            Some(1.0 - 7_500.0 / 18_000.0)
        );
        assert_eq!(elapsed_share(Some(999_999), Some(300)), Some(0.0));
        assert_eq!(elapsed_share(None, Some(300)), None);
        assert_eq!(elapsed_share(Some(60), None), None);
    }

    fn meters() -> Vec<ClientShellAccountUsage> {
        vec![
            ClientShellAccountUsage {
                provider: "claude".into(),
                plan_type: None,
                stale: false,
                windows: vec![
                    window("5h", 62, Some(2 * 3600 + 5 * 60), Some(300)),
                    window("week", 15, Some(3 * 86_400), Some(10_080)),
                ],
            },
            ClientShellAccountUsage {
                provider: "codex".into(),
                plan_type: Some("pro".into()),
                stale: true,
                windows: vec![window("week", 82, None, Some(10_080))],
            },
        ]
    }

    fn config() -> ClientShellConfig {
        ClientShellConfig::from_config(&crate::config::Config::default())
    }

    fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect::<String>()
    }

    #[test]
    fn panel_is_absent_without_readings_or_room() {
        let empty = snapshot(Vec::new());
        assert_eq!(panel_height(&empty), 0);
        assert_eq!(
            reserve(Rect::new(0, 0, 30, 40), &empty),
            (Rect::new(0, 0, 30, 40), Rect::default())
        );

        let populated = snapshot(meters());
        assert_eq!(panel_height(&populated), HEADER_ROWS + 5 + TOGGLE_ROWS);
        assert_eq!(
            reserve(Rect::new(0, 0, 30, 12), &populated),
            (Rect::new(0, 0, 30, 12), Rect::default())
        );
        assert_eq!(
            reserve(Rect::new(0, 0, 30, 40), &populated),
            (Rect::new(0, 0, 30, 32), Rect::new(0, 32, 30, 8))
        );
    }

    #[test]
    fn rows_group_windows_under_provider_headers() {
        let snapshot = snapshot(meters());
        let config = config();
        let area = Rect::new(0, 0, 26, 8);
        let mut buffer = Buffer::empty(area);
        render(&mut buffer, area, &snapshot, &config);

        assert!(row_text(&buffer, 1, 25).starts_with(" usage"));
        assert_eq!(row_text(&buffer, 2, 25).trim_end(), " claude");

        let first = row_text(&buffer, 3, 25);
        assert!(first.starts_with("  5h "), "{first:?}");
        assert!(first.contains(FILLED), "{first:?}");
        assert!(first.contains(EMPTY), "{first:?}");
        assert!(first.trim_end().ends_with("2h"), "{first:?}");
        assert!(!first.contains('%'), "{first:?}");
        assert_eq!(buffer[(8, 3)].style().fg, Some(config.palette.green));
        // 58% of the 5h window has elapsed: the tick sits past the 38% fill.
        assert!(first.contains(TICK), "{first:?}");
        let bar = first.chars().skip(7).take(15).collect::<String>();
        let tick_at = bar.chars().position(|c| c.to_string() == TICK).unwrap();
        let filled = bar.chars().filter(|c| c.to_string() == FILLED).count();
        assert!(
            tick_at > filled,
            "tick {tick_at} should be past fill {filled}: {bar:?}"
        );

        let second = row_text(&buffer, 4, 25);
        assert!(second.trim_end().ends_with("3d"), "{second:?}");
        assert_eq!(buffer[(8, 4)].style().fg, Some(config.palette.red));

        let codex = row_text(&buffer, 5, 25);
        assert!(codex.starts_with(" codex"), "{codex:?}");
        assert!(codex.trim_end().ends_with("stale"), "{codex:?}");

        let third = row_text(&buffer, 6, 25);
        assert!(third.starts_with("  week"), "{third:?}");
        assert!(third.contains(FILLED), "{third:?}");
        assert!(!third.contains('%'), "{third:?}");
        assert_eq!(buffer[(3, 6)].style().fg, Some(config.palette.overlay0));
        // No countdown: the final bar uses the full width above the toggle.
        assert_eq!(buffer[(23, 6)].symbol(), EMPTY);
        assert_eq!(buffer[(24, 6)].symbol(), EMPTY);
        assert!(row_text(&buffer, 7, 25).trim().is_empty());
    }

    #[test]
    fn plan_type_shows_on_fresh_provider_rows() {
        let mut meters = meters();
        meters[1].stale = false;
        let snapshot = snapshot(meters);
        let area = Rect::new(0, 0, 26, 8);
        let mut buffer = Buffer::empty(area);
        render(&mut buffer, area, &snapshot, &config());
        let codex = row_text(&buffer, 5, 25);
        assert!(codex.trim_end().ends_with("pro"), "{codex:?}");
    }

    #[test]
    fn narrow_sidebars_keep_the_bar_and_drop_the_countdown() {
        let snapshot = snapshot(meters());
        let area = Rect::new(0, 0, 13, 8);
        let mut buffer = Buffer::empty(area);
        render(&mut buffer, area, &snapshot, &config());
        let first = row_text(&buffer, 3, 12);
        assert!(!first.contains("2h"), "{first:?}");
        assert!(first.contains(FILLED), "{first:?}");
    }

    #[test]
    fn usage_bars_align_above_a_dedicated_clickable_collapse_row() {
        use super::super::ClientShellState;
        use crate::raw_input::RawInputEvent;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut meters = meters();
        meters[0]
            .windows
            .push(window("fable", 80, Some(2 * 86_400), Some(10_080)));
        meters[1].stale = false;
        meters[1].windows[0].resets_in_seconds = Some(86_400);
        let snapshot = snapshot(meters);
        let mut state = ClientShellState::new(config());
        state.set_snapshot(Box::new(snapshot));
        let frame = state.compose(106, 30).expect("usage sidebar");
        let buffer = frame.to_ratatui_buffer().expect("rendered buffer");
        let toggle = state.hits.sidebar_toggle;
        assert_eq!(buffer[(toggle.x, toggle.y)].symbol(), "«");

        let bar_columns = |y| {
            (0..=toggle.x)
                .filter(|&x| matches!(buffer[(x, y)].symbol(), FILLED | EMPTY | TICK))
                .collect::<Vec<_>>()
        };
        let codex_y = toggle.y - 1;
        let codex_bar = bar_columns(codex_y);
        assert!(codex_bar.len() >= usize::from(MIN_BAR_WIDTH));
        // Three Claude windows precede the Codex provider header and window.
        for y in (codex_y - 4)..=(codex_y - 2) {
            assert_eq!(bar_columns(y), codex_bar, "bar at row {y}");
        }
        assert_eq!(buffer[(toggle.x, codex_y)].symbol(), "d");
        assert!((0..toggle.x).all(|x| buffer[(x, toggle.y)].symbol() == " "));

        state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x,
            row: codex_y,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(
            !state.sidebar_collapsed,
            "usage row must not collapse sidebar"
        );
        state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x,
            row: toggle.y,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(state.sidebar_collapsed, "footer toggle remains clickable");
    }
}
