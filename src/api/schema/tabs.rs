use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::{default_true, AgentStatus};
use crate::cli::projection::{cli_params, Field, Input};
use crate::cli::workspace_bindings as bindings;

pub(crate) fn workspace_option(value_name: &'static str) -> Input<Option<String>> {
    Input::value("workspace", value_name, |slot, value| {
        *slot = Some(crate::cli::normalize_workspace_id(value));
        Ok(())
    })
}

pub(crate) fn tab_id() -> Input<String> {
    Input::positional("tab_id", 0, |slot, value| {
        *slot = crate::cli::normalize_tab_id(value);
        Ok(())
    })
    .value_name("TAB_ID")
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct TabCreateParams => TabCreateProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(workspace_option("WORKSPACE_ID").order(0)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String> => Field::new(None).input(bindings::optional_text("cwd", "PATH").path().order(1)),
        #[serde(default)]
        pub focus: bool => Field::new(false)
            .input(bindings::boolean("focus", true).order(4))
            .input(bindings::boolean("no-focus", false).order(5)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub label: Option<String> => Field::new(None).input(bindings::optional_text("label", "TEXT").order(2)),
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        pub env: HashMap<String, String> => Field::new(HashMap::new()).input(bindings::environment().order(3)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct TabListParams => TabListProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(workspace_option("WORKSPACE_ID")),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct TabRenameParams => TabRenameProjection {
        pub tab_id: String => Field::new(String::new()).input(tab_id()),
        pub label: String => Field::new(String::new()).input(bindings::label()),
    }
}

fn pane_id() -> Input<String> {
    Input::positional("pane_id", 0, |slot, value| {
        *slot = crate::cli::normalize_pane_id(value);
        Ok(())
    })
    .value_name("PANE_ID")
}

cli_params! {
    /// Guarded naming of the tab that holds a pane. Unlike `tab.rename`, this never
    /// replaces a name a person set unless the caller opts out of the guard.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct TabNameForPaneParams => TabNameForPaneProjection {
        /// Public pane id such as `w4B:p2`; the containing tab is named.
        pub pane_id: String => Field::new(String::new()).input(pane_id()),
        pub label: String => Field::new(String::new()).input(bindings::label()),
        /// Apply only when the tab has no custom name, or when its custom name was
        /// applied by an earlier `tab.name_for_pane` call. Defaults to true.
        #[serde(default = "default_true")]
        pub if_auto_named: bool => Field::new(true).input(
            bindings::boolean("even-if-named", false)
                .help("Replace a name set by a person or tab rename"),
        ),
        /// Apply only when the target pane is the tab's only pane. Defaults to true.
        #[serde(default = "default_true")]
        pub if_single_pane: bool => Field::new(true).input(
            bindings::boolean("even-if-shared", false)
                .help("Apply even when the tab holds other panes"),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TabNameForPaneReason {
    Applied,
    /// The tab already carries this label from an earlier `tab.name_for_pane`.
    Unchanged,
    /// A person or `tab.rename` named the tab and `if_auto_named` is set.
    CustomNamePresent,
    /// The tab holds other panes and `if_single_pane` is set.
    MultiplePanes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabMoveParams {
    pub tab_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}
