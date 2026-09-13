use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, TabCreateParams, TabListParams,
    TabMoveParams, TabNameForPaneParams, TabNameForPaneReason, TabRenameParams, TabTarget,
};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> String {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(_) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            self.tab_list_info(ws_idx)
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        tabs.push(tab);
                    }
                }
            }
            tabs
        };

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> String {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = match super::env::normalize_launch_env(env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                if let Some(label) = label {
                    let workspace_id = self.state.workspaces[ws_idx].id.clone();
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                        crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
                    });
                    if let Some(tab) = self
                        .state
                        .workspaces
                        .get_mut(ws_idx)
                        .and_then(|ws| ws.tabs.get_mut(tab_idx))
                    {
                        tab.set_custom_name(label);
                        crate::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                if focus {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state.mode = Mode::Terminal;
                }
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                encode_success(
                    id,
                    self.tab_created_result(ws_idx, tab_idx)
                        .expect("new tab should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "tab_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabRenamed,
            data: EventData::TabRenamed {
                tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    /// Name the tab that holds `pane_id` without clobbering a person's name.
    /// Every guard outcome is a success response with `applied: false`; only an
    /// unknown pane or an empty label is an error.
    pub(super) fn handle_tab_name_for_pane(
        &mut self,
        id: String,
        params: TabNameForPaneParams,
    ) -> String {
        let TabNameForPaneParams {
            pane_id,
            label,
            if_auto_named,
            if_single_pane,
        } = params;
        let label = label.trim().to_string();
        if label.is_empty() {
            return encode_error(id, "invalid_label", "label must not be empty");
        }
        let Some((ws_idx, pane)) = self.parse_pane_id(&pane_id) else {
            return pane_not_found(id, &pane_id);
        };
        let Some(tab_idx) = self.state.workspaces[ws_idx].find_tab_index_for_pane(pane) else {
            return pane_not_found(id, &pane_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return pane_not_found(id, &pane_id);
        };
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return pane_not_found(id, &pane_id);
        };

        let reason = if tab.is_named_for_pane() && tab.custom_name.as_deref() == Some(&label) {
            TabNameForPaneReason::Unchanged
        } else if if_single_pane && tab.panes.len() != 1 {
            TabNameForPaneReason::MultiplePanes
        } else if if_auto_named && !tab.is_auto_named() && !tab.is_named_for_pane() {
            TabNameForPaneReason::CustomNamePresent
        } else {
            tab.set_name_for_pane(label.clone());
            TabNameForPaneReason::Applied
        };
        let applied = reason == TabNameForPaneReason::Applied;
        if applied {
            crate::logging::tab_renamed(&workspace_id, &tab_id);
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabRenamed,
                data: EventData::TabRenamed {
                    tab_id: tab_id.clone(),
                    workspace_id: workspace_id.clone(),
                    label: label.clone(),
                },
            });
        }
        let label = self.state.workspaces[ws_idx]
            .tab_display_name(tab_idx)
            .unwrap_or(label);

        encode_success(
            id,
            ResponseResult::TabNameForPane {
                applied,
                tab_id,
                workspace_id,
                label,
                reason,
            },
        )
    }

    pub(super) fn handle_tab_move(&mut self, id: String, params: TabMoveParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &params.tab_id);
        };
        if params.insert_index > ws.tabs.len() {
            return encode_error(
                id,
                "tab_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let tab_id = self
            .public_tab_id(ws_idx, tab_idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&ws.id, tab_idx + 1));
        let workspace_id = self.public_workspace_id(ws_idx);
        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let tabs = self.tab_list_info(ws_idx);
        if moved {
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabMoved,
                data: EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index,
                    tabs: tabs.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let closes_workspace = ws.tabs.len() <= 1;
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids = ws
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();

        if closes_workspace {
            if let Err(response) = self.require_restored_group_close_ready(
                &id,
                &self.state.workspace_close_indices(ws_idx),
            ) {
                return response;
            }
            if self.state.confirm_implicit_worktree_group_close(ws_idx) {
                return encode_error(
                    id,
                    "confirmation_required",
                    "closing this tab would close a worktree group",
                );
            }
            let workspace = self.workspace_info(ws_idx);
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return encode_success(id, ResponseResult::Ok {});
        }

        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        if !ws.close_tab(tab_idx) {
            return encode_error(
                id,
                "tab_close_failed",
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    fn tab_list_info(&self, ws_idx: usize) -> Vec<crate::api::schema::TabInfo> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                (0..ws.tabs.len())
                    .filter_map(|idx| self.tab_info(ws_idx, idx))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> String {
    encode_error(id, "tab_not_found", format!("tab {tab_id} not found"))
}

fn pane_not_found(id: String, pane_id: &str) -> String {
    encode_error(id, "not_found", format!("pane {pane_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::SuccessResponse,
        config::{Config, ShellModeConfig},
        workspace::Workspace,
    };

    #[test]
    fn api_tab_close_last_tab_closes_workspace_and_emits_both_events() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let response = app.handle_tab_close(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active.is_none());
        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            [EventKind::TabClosed, EventKind::WorkspaceClosed]
        );
        assert!(matches!(
            &events[0].1.data,
            EventData::TabClosed {
                tab_id: closed_tab_id,
                workspace_id: closed_workspace_id,
            } if closed_tab_id == &tab_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[1].1.data,
            EventData::WorkspaceClosed {
                workspace_id: closed_workspace_id,
                workspace: Some(workspace),
            } if closed_workspace_id == &workspace_id
                && workspace.workspace_id == workspace_id
        ));
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_id = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_tab_move(
            "req".into(),
            TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabList { tabs } = success.result else {
            panic!("expected tab list");
        };
        assert_eq!(app.state.workspaces[0].tabs[2].root_pane, moved_root);
        assert_eq!(tabs[2].tab_id, app.public_tab_id(0, 2).unwrap());
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index: 3,
                    tabs,
                } if tab_id == &moved_id
                    && workspace_id == &app.public_workspace_id(0)
                    && tabs[2].tab_id == moved_id
            )
        }));
    }

    fn app_with_single_pane_tab() -> (App, crate::api::EventHub, String) {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let root_pane = app.state.workspaces[0].tabs[0].root_pane;
        let pane_id = app.public_pane_id(0, root_pane).unwrap();
        (app, event_hub, pane_id)
    }

    fn name_for_pane(
        app: &mut App,
        pane_id: &str,
        label: &str,
        if_auto_named: bool,
        if_single_pane: bool,
    ) -> ResponseResult {
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "req".into(),
            method: crate::api::schema::Method::TabNameForPane(TabNameForPaneParams {
                pane_id: pane_id.into(),
                label: label.into(),
                if_auto_named,
                if_single_pane,
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        success.result
    }

    fn rename_tab(app: &mut App, tab_id: &str, label: &str) {
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "rename".into(),
            method: crate::api::schema::Method::TabRename(TabRenameParams {
                tab_id: tab_id.into(),
                label: label.into(),
            }),
        });
        serde_json::from_str::<SuccessResponse>(&response).unwrap();
    }

    #[test]
    fn api_tab_name_for_pane_names_auto_named_single_pane_tab() {
        let (mut app, event_hub, pane_id) = app_with_single_pane_tab();
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let result = name_for_pane(&mut app, &pane_id, "  dott ", true, true);

        assert_eq!(
            result,
            ResponseResult::TabNameForPane {
                applied: true,
                tab_id: tab_id.clone(),
                workspace_id: workspace_id.clone(),
                label: "dott".into(),
                reason: TabNameForPaneReason::Applied,
            }
        );
        let tab = &app.state.workspaces[0].tabs[0];
        assert_eq!(tab.custom_name.as_deref(), Some("dott"));
        assert!(tab.is_named_for_pane());
        assert_eq!(
            app.state.workspaces[0].tab_display_name(0).as_deref(),
            Some("dott")
        );
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| matches!(
            &event.data,
            EventData::TabRenamed {
                tab_id: renamed_tab,
                workspace_id: renamed_workspace,
                label,
            } if renamed_tab == &tab_id && renamed_workspace == &workspace_id && label == "dott"
        )));
    }

    #[test]
    fn api_tab_name_for_pane_reports_unchanged_for_repeat_label() {
        let (mut app, event_hub, pane_id) = app_with_single_pane_tab();
        name_for_pane(&mut app, &pane_id, "dott", true, true);
        let events_after_first = event_hub.events_after(0).len();

        let result = name_for_pane(&mut app, &pane_id, "dott", true, true);

        assert!(matches!(
            result,
            ResponseResult::TabNameForPane {
                applied: false,
                reason: TabNameForPaneReason::Unchanged,
                ref label,
                ..
            } if label == "dott"
        ));
        assert_eq!(event_hub.events_after(0).len(), events_after_first);
    }

    #[test]
    fn api_tab_name_for_pane_restamps_its_own_earlier_name() {
        let (mut app, _event_hub, pane_id) = app_with_single_pane_tab();
        name_for_pane(&mut app, &pane_id, "dott", true, true);

        let result = name_for_pane(&mut app, &pane_id, "dott (resumed)", true, true);

        assert!(matches!(
            result,
            ResponseResult::TabNameForPane {
                applied: true,
                reason: TabNameForPaneReason::Applied,
                ..
            }
        ));
        assert_eq!(
            app.state.workspaces[0].tabs[0].custom_name.as_deref(),
            Some("dott (resumed)")
        );
    }

    #[test]
    fn api_tab_name_for_pane_keeps_user_names_unless_overridden() {
        let (mut app, event_hub, pane_id) = app_with_single_pane_tab();
        let tab_id = app.public_tab_id(0, 0).unwrap();
        rename_tab(&mut app, &tab_id, "review");
        let events_after_rename = event_hub.events_after(0).len();

        let guarded = name_for_pane(&mut app, &pane_id, "dott", true, true);

        assert_eq!(
            guarded,
            ResponseResult::TabNameForPane {
                applied: false,
                tab_id: tab_id.clone(),
                workspace_id: app.public_workspace_id(0),
                label: "review".into(),
                reason: TabNameForPaneReason::CustomNamePresent,
            }
        );
        assert_eq!(
            app.state.workspaces[0].tabs[0].custom_name.as_deref(),
            Some("review")
        );
        assert_eq!(event_hub.events_after(0).len(), events_after_rename);

        let forced = name_for_pane(&mut app, &pane_id, "dott", false, true);

        assert!(matches!(
            forced,
            ResponseResult::TabNameForPane {
                applied: true,
                reason: TabNameForPaneReason::Applied,
                ..
            }
        ));
        assert_eq!(
            app.state.workspaces[0].tabs[0].custom_name.as_deref(),
            Some("dott")
        );
        assert!(app.state.workspaces[0].tabs[0].is_named_for_pane());
    }

    #[test]
    fn api_tab_name_for_pane_skips_shared_tabs_unless_overridden() {
        let (mut app, _event_hub, pane_id) = app_with_single_pane_tab();
        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);

        let guarded = name_for_pane(&mut app, &pane_id, "dott", true, true);

        assert!(matches!(
            guarded,
            ResponseResult::TabNameForPane {
                applied: false,
                reason: TabNameForPaneReason::MultiplePanes,
                ref label,
                ..
            } if label == "1"
        ));
        assert!(app.state.workspaces[0].tabs[0].is_auto_named());

        let forced = name_for_pane(&mut app, &pane_id, "dott", true, false);

        assert!(matches!(
            forced,
            ResponseResult::TabNameForPane {
                applied: true,
                reason: TabNameForPaneReason::Applied,
                ..
            }
        ));
        assert_eq!(
            app.state.workspaces[0].tabs[0].custom_name.as_deref(),
            Some("dott")
        );
    }

    #[test]
    fn api_tab_name_for_pane_checks_unchanged_before_guards() {
        let (mut app, _event_hub, pane_id) = app_with_single_pane_tab();
        name_for_pane(&mut app, &pane_id, "dott", true, true);
        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);

        let result = name_for_pane(&mut app, &pane_id, "dott", true, true);

        assert!(matches!(
            result,
            ResponseResult::TabNameForPane {
                applied: false,
                reason: TabNameForPaneReason::Unchanged,
                ..
            }
        ));
    }

    #[test]
    fn api_tab_rename_resets_name_for_pane_provenance() {
        let (mut app, _event_hub, pane_id) = app_with_single_pane_tab();
        let tab_id = app.public_tab_id(0, 0).unwrap();
        name_for_pane(&mut app, &pane_id, "dott", true, true);
        rename_tab(&mut app, &tab_id, "dott");

        assert!(!app.state.workspaces[0].tabs[0].is_named_for_pane());
        let result = name_for_pane(&mut app, &pane_id, "dott", true, true);
        assert!(matches!(
            result,
            ResponseResult::TabNameForPane {
                applied: false,
                reason: TabNameForPaneReason::CustomNamePresent,
                ..
            }
        ));
    }

    #[test]
    fn api_tab_name_for_pane_rejects_unknown_pane_and_empty_label() {
        let (mut app, _event_hub, pane_id) = app_with_single_pane_tab();

        let unknown = app.handle_api_request(crate::api::schema::Request {
            id: "req".into(),
            method: crate::api::schema::Method::TabNameForPane(TabNameForPaneParams {
                pane_id: "w9:p9".into(),
                label: "dott".into(),
                if_auto_named: true,
                if_single_pane: true,
            }),
        });
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&unknown).unwrap();
        assert_eq!(error.error.code, "not_found");

        let empty = app.handle_api_request(crate::api::schema::Request {
            id: "req".into(),
            method: crate::api::schema::Method::TabNameForPane(TabNameForPaneParams {
                pane_id,
                label: "   ".into(),
                if_auto_named: true,
                if_single_pane: true,
            }),
        });
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&empty).unwrap();
        assert_eq!(error.error.code, "invalid_label");
        assert!(app.state.workspaces[0].tabs[0].is_auto_named());
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let cached_cwd = std::env::temp_dir();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cached_cwd.clone();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::TabCreated { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created.terminal_id(created.root_pane).unwrap();
        let created_cwd = &app.state.terminals.get(created_terminal_id).unwrap().cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&cached_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }
}
