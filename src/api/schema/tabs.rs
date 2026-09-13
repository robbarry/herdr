use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::{default_true, AgentStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct TabListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabRenameParams {
    pub tab_id: String,
    pub label: String,
}

/// Guarded naming of the tab that holds a pane. Unlike `tab.rename`, this never
/// replaces a name a person set unless the caller opts out of the guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabNameForPaneParams {
    /// Public pane id such as `w4B:p2`; the containing tab is named.
    pub pane_id: String,
    pub label: String,
    /// Apply only when the tab has no custom name, or when its custom name was
    /// applied by an earlier `tab.name_for_pane` call. Defaults to true.
    #[serde(default = "default_true")]
    pub if_auto_named: bool,
    /// Apply only when the target pane is the tab's only pane. Defaults to true.
    #[serde(default = "default_true")]
    pub if_single_pane: bool,
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
