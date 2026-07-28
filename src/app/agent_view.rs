use std::cmp::Ordering;

use crate::api::schema::{
    AgentStatus, AgentViewBuiltinField, AgentViewBuiltinSortField, AgentViewContext,
    AgentViewField, AgentViewFilter, AgentViewSetParams, AgentViewSort, AgentViewSortField,
    AgentViewSortOrder, AgentViewValue,
};
use crate::ui::AgentPanelEntry;

use super::{AppState, Mode};

const MAX_FILTER_DEPTH: usize = 8;
const MAX_FILTER_NODES: usize = 64;
const MAX_FILTER_VALUES: usize = 32;
const MAX_SORT_FIELDS: usize = 8;
const MAX_SOURCE_CHARS: usize = 120;
const MAX_LABEL_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum EvalValue {
    String(String),
    Bool(bool),
    Number(u64),
}

pub(crate) fn validate_agent_view(spec: &mut AgentViewSetParams) -> Result<(), String> {
    spec.source = normalize_source(&spec.source)?;
    spec.label = spec
        .label
        .take()
        .map(|label| normalize_label(&label))
        .transpose()?;

    let mut nodes = 0;
    if let Some(filter) = &spec.filter {
        validate_filter(filter, 1, &mut nodes)?;
    }
    if spec.sort.len() > MAX_SORT_FIELDS {
        return Err(format!(
            "agent view sort may contain at most {MAX_SORT_FIELDS} fields"
        ));
    }
    for sort in &spec.sort {
        validate_sort_field(&sort.field)?;
    }
    Ok(())
}

pub(crate) fn validate_agent_view_source(source: &str) -> Result<String, String> {
    normalize_source(source)
}

/// A heading in the status-grouped agent panel.
///
/// Idle is subdivided by age because "idle" spans an agent that paused a minute
/// ago and one abandoned two months back, and those want very different
/// attention. The other states are not subdivided: a blocked agent is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AgentGroup {
    Blocked,
    Working,
    Idle(crate::app::state::IdleAge),
    Unknown,
    /// Agents on a host we cannot currently reach. Their reported state is
    /// whatever we last saw and may be arbitrarily stale, so grouping them by
    /// it would be a lie; they get their own group at the bottom instead.
    Offline,
}

impl AgentGroup {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Blocked => "blocked".to_string(),
            Self::Working => "working".to_string(),
            Self::Idle(age) => format!("idle · {}", age.label()),
            Self::Unknown => "unknown".to_string(),
            Self::Offline => "offline".to_string(),
        }
    }

    pub(crate) fn of_with_offline(entry: &AgentPanelEntry, now_ms: u64, offline: bool) -> Self {
        use crate::detect::AgentState;
        if offline {
            return Self::Offline;
        }
        match entry.state {
            AgentState::Blocked => Self::Blocked,
            AgentState::Working => Self::Working,
            AgentState::Idle => Self::Idle(crate::app::state::IdleAge::from_changed_at(
                now_ms,
                entry.agent_state_changed_at_ms,
            )),
            AgentState::Unknown => Self::Unknown,
        }
    }
}

/// Sorts entries into status groups, and reports which group each one landed
/// in so the renderer can head each run.
///
/// Returned groups are parallel to `entries` after sorting: `groups[i]` is the
/// group of `entries[i]`. Keeping entries a flat list matters — the indexed
/// switch keys and click handling address agents by position, and they must not
/// have to know about headings.
pub(crate) fn group_by_status(
    entries: &mut [AgentPanelEntry],
    now_ms: u64,
    offline: &dyn Fn(&AgentPanelEntry) -> bool,
) -> Vec<AgentGroup> {
    entries.sort_by_key(|entry| {
        let group = AgentGroup::of_with_offline(entry, now_ms, offline(entry));
        (
            group,
            // Unread output first inside a group: there is something to look at.
            entry.seen,
            // Then most recently changed first, so a group reads newest-down.
            std::cmp::Reverse(entry.agent_state_changed_at_ms),
        )
    });
    entries
        .iter()
        .map(|entry| AgentGroup::of_with_offline(entry, now_ms, offline(entry)))
        .collect()
}

pub(crate) fn apply_agent_view(app: &AppState, entries: &mut Vec<AgentPanelEntry>) {
    if let Some(spec) = app.agent_view_override.as_ref() {
        if let Some(filter) = &spec.filter {
            entries.retain(|entry| matches_filter(app, entry, filter));
        }
        if !spec.sort.is_empty() {
            entries.sort_by(|left, right| compare_entries(app, left, right, &spec.sort));
            return;
        }
    }

    if matches!(
        app.agent_panel_sort,
        crate::app::state::AgentPanelSort::Status
    ) {
        let offline = |entry: &AgentPanelEntry| {
            app.workspaces
                .get(entry.ws_idx)
                .and_then(|ws| ws.remote_mirror.as_ref())
                .is_some_and(|mirror| app.remote_offline_hosts.contains(&mirror.target))
        };
        group_by_status(entries, crate::app::state::unix_millis_now(), &offline);
        return;
    }

    if matches!(
        app.agent_panel_sort,
        crate::app::state::AgentPanelSort::Priority
    ) {
        entries.sort_by_key(|entry| {
            (
                std::cmp::Reverse(super::api_helpers::tab_attention_priority(
                    entry.state,
                    entry.seen,
                )),
                std::cmp::Reverse(entry.last_agent_state_change_seq),
            )
        });
    }
}

pub(crate) fn presented_workspace_idx(app: &AppState) -> Option<usize> {
    if app.mode == Mode::Navigate {
        app.workspaces.get(app.selected).map(|_| app.selected)
    } else {
        app.active
    }
}

fn normalize_source(source: &str) -> Result<String, String> {
    let source = source.trim();
    if source.is_empty()
        || source.chars().count() > MAX_SOURCE_CHARS
        || !source
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '.' | '_' | '-'))
    {
        return Err(format!(
            "agent view source must be non-empty, at most {MAX_SOURCE_CHARS} characters, and contain only ASCII letters, digits, colon, dot, underscore, or hyphen"
        ));
    }
    Ok(source.to_string())
}

fn normalize_label(label: &str) -> Result<String, String> {
    let label = label
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!(
            "agent view label must be non-empty and at most {MAX_LABEL_CHARS} characters"
        ));
    }
    Ok(label)
}

fn validate_filter(
    filter: &AgentViewFilter,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), String> {
    if depth > MAX_FILTER_DEPTH {
        return Err(format!(
            "agent view filter may be nested at most {MAX_FILTER_DEPTH} levels"
        ));
    }
    *nodes += 1;
    if *nodes > MAX_FILTER_NODES {
        return Err(format!(
            "agent view filter may contain at most {MAX_FILTER_NODES} nodes"
        ));
    }

    match filter {
        AgentViewFilter::All { filters } | AgentViewFilter::Any { filters } => {
            if filters.is_empty() {
                return Err("agent view all/any filters must not be empty".to_string());
            }
            for filter in filters {
                validate_filter(filter, depth + 1, nodes)?;
            }
        }
        AgentViewFilter::Not { filter } => validate_filter(filter, depth + 1, nodes)?,
        AgentViewFilter::Eq { field, value } => validate_field_value(field, value)?,
        AgentViewFilter::In { field, values } => {
            if values.is_empty() || values.len() > MAX_FILTER_VALUES {
                return Err(format!(
                    "agent view in filters require 1 to {MAX_FILTER_VALUES} values"
                ));
            }
            for value in values {
                validate_field_value(field, value)?;
            }
        }
        AgentViewFilter::Exists { field } => validate_field(field)?,
    }
    Ok(())
}

fn validate_field(field: &AgentViewField) -> Result<(), String> {
    if let AgentViewField::Token { token } = field {
        validate_token(token)?;
    }
    Ok(())
}

fn validate_field_value(field: &AgentViewField, value: &AgentViewValue) -> Result<(), String> {
    validate_field(field)?;
    match (field, value) {
        (
            AgentViewField::Builtin(AgentViewBuiltinField::WorkspaceId),
            AgentViewValue::Context {
                context: AgentViewContext::CurrentWorkspaceId,
            },
        )
        | (
            AgentViewField::Builtin(AgentViewBuiltinField::TabId),
            AgentViewValue::Context {
                context: AgentViewContext::CurrentTabId,
            },
        ) => Ok(()),
        (_, AgentViewValue::Context { .. }) => {
            Err("agent view context type does not match the selected field".to_string())
        }
        (AgentViewField::Builtin(AgentViewBuiltinField::Seen), AgentViewValue::Bool(_))
        | (
            AgentViewField::Builtin(AgentViewBuiltinField::StateChangeSeq),
            AgentViewValue::Number(_),
        ) => Ok(()),
        (
            AgentViewField::Builtin(
                AgentViewBuiltinField::Status
                | AgentViewBuiltinField::WorkspaceId
                | AgentViewBuiltinField::TabId
                | AgentViewBuiltinField::PaneId
                | AgentViewBuiltinField::Agent,
            )
            | AgentViewField::Token { .. },
            AgentViewValue::String(value),
        ) => {
            if matches!(
                field,
                AgentViewField::Builtin(AgentViewBuiltinField::Status)
            ) && !matches!(
                value.as_str(),
                "idle" | "working" | "blocked" | "done" | "unknown"
            ) {
                return Err(format!("unknown agent status `{value}`"));
            }
            Ok(())
        }
        _ => Err("agent view value type does not match the selected field".to_string()),
    }
}

fn validate_sort_field(field: &AgentViewSortField) -> Result<(), String> {
    if let AgentViewSortField::Token { token } = field {
        validate_token(token)?;
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.is_empty()
        || token.len() > 32
        || !token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(format!("invalid agent view token `{token}`"));
    }
    Ok(())
}

fn matches_filter(app: &AppState, entry: &AgentPanelEntry, filter: &AgentViewFilter) -> bool {
    match filter {
        AgentViewFilter::All { filters } => filters
            .iter()
            .all(|filter| matches_filter(app, entry, filter)),
        AgentViewFilter::Any { filters } => filters
            .iter()
            .any(|filter| matches_filter(app, entry, filter)),
        AgentViewFilter::Not { filter } => !matches_filter(app, entry, filter),
        AgentViewFilter::Eq { field, value } => {
            field_value(app, entry, field) == operand_value(app, value)
        }
        AgentViewFilter::In { field, values } => {
            let actual = field_value(app, entry, field);
            values
                .iter()
                .any(|value| actual == operand_value(app, value))
        }
        AgentViewFilter::Exists { field } => field_value(app, entry, field).is_some(),
    }
}

fn compare_entries(
    app: &AppState,
    left: &AgentPanelEntry,
    right: &AgentPanelEntry,
    sorts: &[AgentViewSort],
) -> Ordering {
    for sort in sorts {
        let left = sort_value(app, left, &sort.field);
        let right = sort_value(app, right, &sort.field);
        let ordering = compare_optional_values(left, right, sort.order);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_optional_values(
    left: Option<EvalValue>,
    right: Option<EvalValue>,
    order: AgentViewSortOrder,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => {
            let ordering = left.cmp(&right);
            if matches!(order, AgentViewSortOrder::Desc) {
                ordering.reverse()
            } else {
                ordering
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn field_value(
    app: &AppState,
    entry: &AgentPanelEntry,
    field: &AgentViewField,
) -> Option<EvalValue> {
    match field {
        AgentViewField::Builtin(field) => builtin_field_value(app, entry, *field),
        AgentViewField::Token { token } => entry.tokens.get(token).cloned().map(EvalValue::String),
    }
}

fn builtin_field_value(
    app: &AppState,
    entry: &AgentPanelEntry,
    field: AgentViewBuiltinField,
) -> Option<EvalValue> {
    match field {
        AgentViewBuiltinField::Status => {
            Some(EvalValue::String(status_name(entry.state, entry.seen)))
        }
        AgentViewBuiltinField::WorkspaceId => app
            .workspaces
            .get(entry.ws_idx)
            .map(|workspace| EvalValue::String(workspace.id.clone())),
        AgentViewBuiltinField::TabId => public_tab_id(app, entry).map(EvalValue::String),
        AgentViewBuiltinField::PaneId => public_pane_id(app, entry).map(EvalValue::String),
        AgentViewBuiltinField::Agent => entry.agent_kind_label.clone().map(EvalValue::String),
        AgentViewBuiltinField::Seen => Some(EvalValue::Bool(entry.seen)),
        AgentViewBuiltinField::StateChangeSeq => {
            entry.last_agent_state_change_seq.map(EvalValue::Number)
        }
    }
}

fn operand_value(app: &AppState, value: &AgentViewValue) -> Option<EvalValue> {
    match value {
        AgentViewValue::String(value) => Some(EvalValue::String(value.clone())),
        AgentViewValue::Bool(value) => Some(EvalValue::Bool(*value)),
        AgentViewValue::Number(value) => Some(EvalValue::Number(*value)),
        AgentViewValue::Context { context } => context_value(app, *context),
    }
}

fn context_value(app: &AppState, context: AgentViewContext) -> Option<EvalValue> {
    let ws_idx = presented_workspace_idx(app)?;
    let workspace = app.workspaces.get(ws_idx)?;
    match context {
        AgentViewContext::CurrentWorkspaceId => Some(EvalValue::String(workspace.id.clone())),
        AgentViewContext::CurrentTabId => {
            let tab_number = workspace.public_tab_number(workspace.active_tab)?;
            Some(EvalValue::String(
                crate::workspace::public_tab_id_for_number(&workspace.id, tab_number),
            ))
        }
    }
}

fn sort_value(
    app: &AppState,
    entry: &AgentPanelEntry,
    field: &AgentViewSortField,
) -> Option<EvalValue> {
    match field {
        AgentViewSortField::Token { token } => {
            entry.tokens.get(token).cloned().map(EvalValue::String)
        }
        AgentViewSortField::Builtin(field) => match field {
            AgentViewBuiltinSortField::WorkspaceOrder => {
                Some(EvalValue::Number(entry.ws_idx as u64))
            }
            AgentViewBuiltinSortField::TabOrder => app
                .workspaces
                .get(entry.ws_idx)
                .and_then(|workspace| workspace.public_tab_number(entry.tab_idx))
                .map(|number| EvalValue::Number(number as u64)),
            AgentViewBuiltinSortField::PaneOrder => app
                .workspaces
                .get(entry.ws_idx)
                .and_then(|workspace| workspace.public_pane_number(entry.pane_id))
                .map(|number| EvalValue::Number(number as u64)),
            AgentViewBuiltinSortField::Attention => Some(EvalValue::Number(u64::from(
                super::api_helpers::tab_attention_priority(entry.state, entry.seen),
            ))),
            AgentViewBuiltinSortField::Status => {
                Some(EvalValue::String(status_name(entry.state, entry.seen)))
            }
            AgentViewBuiltinSortField::Agent => {
                entry.agent_kind_label.clone().map(EvalValue::String)
            }
            AgentViewBuiltinSortField::Seen => Some(EvalValue::Bool(entry.seen)),
            AgentViewBuiltinSortField::StateChangeSeq => {
                entry.last_agent_state_change_seq.map(EvalValue::Number)
            }
        },
    }
}

fn status_name(state: crate::detect::AgentState, seen: bool) -> String {
    let status = match (state, seen) {
        (crate::detect::AgentState::Idle, false) => AgentStatus::Done,
        (crate::detect::AgentState::Idle, true) => AgentStatus::Idle,
        (crate::detect::AgentState::Working, _) => AgentStatus::Working,
        (crate::detect::AgentState::Blocked, _) => AgentStatus::Blocked,
        (crate::detect::AgentState::Unknown, _) => AgentStatus::Unknown,
    };
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
    .to_string()
}

fn public_tab_id(app: &AppState, entry: &AgentPanelEntry) -> Option<String> {
    let workspace = app.workspaces.get(entry.ws_idx)?;
    let number = workspace.public_tab_number(entry.tab_idx)?;
    Some(crate::workspace::public_tab_id_for_number(
        &workspace.id,
        number,
    ))
}

fn public_pane_id(app: &AppState, entry: &AgentPanelEntry) -> Option<String> {
    let workspace = app.workspaces.get(entry.ws_idx)?;
    let number = workspace.public_pane_number(entry.pane_id)?;
    Some(crate::workspace::public_pane_id_for_number(
        &workspace.id,
        number,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentViewBuiltinSortField, AgentViewSortField};
    use crate::app::state::IdleAge;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    fn state_with_agents() -> AppState {
        let mut state = AppState::test_new();
        state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        state.ensure_test_terminals();
        state.active = Some(0);
        state.selected = 0;
        for (ws_idx, agent_state) in [(0, AgentState::Idle), (1, AgentState::Working)] {
            let pane_id = state.workspaces[ws_idx].tabs[0].root_pane;
            let terminal_id = state.workspaces[ws_idx].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            let terminal = state.terminals.get_mut(&terminal_id).unwrap();
            terminal.detected_agent = Some(Agent::Claude);
            terminal.state = agent_state;
        }
        state
    }

    fn current_workspace_view() -> AgentViewSetParams {
        AgentViewSetParams {
            source: "example.views".to_string(),
            label: Some("current".to_string()),
            filter: Some(AgentViewFilter::Eq {
                field: AgentViewField::Builtin(AgentViewBuiltinField::WorkspaceId),
                value: AgentViewValue::Context {
                    context: AgentViewContext::CurrentWorkspaceId,
                },
            }),
            sort: Vec::new(),
        }
    }

    #[test]
    fn current_workspace_filter_tracks_presented_workspace() {
        let mut state = state_with_agents();
        state.agent_view_override = Some(current_workspace_view());

        assert_eq!(crate::ui::agent_panel_entries(&state)[0].ws_idx, 0);

        state.mode = Mode::Navigate;
        state.selected = 1;
        let entries = crate::ui::agent_panel_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ws_idx, 1);

        state.mode = Mode::Settings;
        let entries = crate::ui::agent_panel_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ws_idx, 0);
    }

    #[test]
    fn boolean_filter_and_custom_sort_define_canonical_entries() {
        let mut state = state_with_agents();
        let first_pane = state.workspaces[0].tabs[0].root_pane;
        let first_terminal = state.workspaces[0].tabs[0].panes[&first_pane]
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&first_terminal).unwrap().state = AgentState::Working;
        state.agent_view_override = Some(AgentViewSetParams {
            source: "example.views".to_string(),
            label: None,
            filter: Some(AgentViewFilter::All {
                filters: vec![
                    AgentViewFilter::Eq {
                        field: AgentViewField::Builtin(AgentViewBuiltinField::Status),
                        value: AgentViewValue::String("working".to_string()),
                    },
                    AgentViewFilter::Not {
                        filter: Box::new(AgentViewFilter::Eq {
                            field: AgentViewField::Builtin(AgentViewBuiltinField::WorkspaceId),
                            value: AgentViewValue::String("missing".to_string()),
                        }),
                    },
                ],
            }),
            sort: vec![AgentViewSort {
                field: AgentViewSortField::Builtin(AgentViewBuiltinSortField::WorkspaceOrder),
                order: AgentViewSortOrder::Desc,
            }],
        });

        let entries = crate::ui::agent_panel_entries(&state);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].ws_idx, 1);
        assert_eq!(entries[1].ws_idx, 0);
    }

    #[test]
    fn agent_filter_matches_custom_lifecycle_agent_label() {
        let mut state = state_with_agents();
        let pane_id = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_hook_authority(
                "test".to_string(),
                "custom-agent".to_string(),
                AgentState::Working,
                None,
                None,
            );
        state.agent_view_override = Some(AgentViewSetParams {
            source: "example.views".to_string(),
            label: None,
            filter: Some(AgentViewFilter::Eq {
                field: AgentViewField::Builtin(AgentViewBuiltinField::Agent),
                value: AgentViewValue::String("custom-agent".to_string()),
            }),
            sort: Vec::new(),
        });

        let entries = crate::ui::agent_panel_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].agent_kind_label.as_deref(), Some("custom-agent"));
    }

    #[test]
    fn validation_rejects_mismatched_context_type() {
        let mut spec = AgentViewSetParams {
            source: "example.views".to_string(),
            label: None,
            filter: Some(AgentViewFilter::Eq {
                field: AgentViewField::Builtin(AgentViewBuiltinField::Status),
                value: AgentViewValue::Context {
                    context: AgentViewContext::CurrentWorkspaceId,
                },
            }),
            sort: Vec::new(),
        };

        assert!(validate_agent_view(&mut spec)
            .unwrap_err()
            .contains("context type"));
    }

    const HOUR_MS: u64 = 60 * 60 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;

    fn idle_entry(label: &str, age_ms: u64, now_ms: u64) -> AgentPanelEntry {
        entry_at(label, AgentState::Idle, true, Some(now_ms - age_ms))
    }

    fn entry_at(
        label: &str,
        state: AgentState,
        seen: bool,
        changed_at_ms: Option<u64>,
    ) -> AgentPanelEntry {
        AgentPanelEntry {
            remote_host: None,
            remote_host_color: None,
            ws_idx: 0,
            tab_idx: 0,
            pane_id: crate::layout::PaneId::from_raw(1),
            primary_label: label.to_string(),
            primary_tab_label: None,
            pane_label: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_label: None,
            agent_kind_label: None,
            agent: None,
            state,
            seen,
            last_agent_state_change_seq: None,
            agent_state_changed_at_ms: changed_at_ms,
            state_labels: std::collections::HashMap::new(),
            tokens: std::collections::HashMap::new(),
        }
    }

    /// The buckets are half-open: an agent idle for exactly 24h belongs to the
    /// next group up, not "today".
    #[test]
    fn idle_age_buckets_split_on_their_boundaries() {
        let now = 100 * DAY_MS;
        let cases = [
            (0, IdleAge::Day),
            (DAY_MS - 1, IdleAge::Day),
            (DAY_MS, IdleAge::ThreeDays),
            (3 * DAY_MS - 1, IdleAge::ThreeDays),
            (3 * DAY_MS, IdleAge::Week),
            (7 * DAY_MS, IdleAge::TwoWeeks),
            (14 * DAY_MS, IdleAge::FourWeeks),
            (28 * DAY_MS, IdleAge::TwoMonths),
            (56 * DAY_MS, IdleAge::Older),
            (99 * DAY_MS, IdleAge::Older),
        ];
        for (age, expected) in cases {
            assert_eq!(
                IdleAge::from_changed_at(now, Some(now - age)),
                expected,
                "age {age}ms"
            );
        }
    }

    /// An agent restored from a snapshot written before this field existed has
    /// no age. It must not masquerade as freshly idle.
    #[test]
    fn missing_timestamp_is_unknown_not_fresh() {
        assert_eq!(IdleAge::from_changed_at(DAY_MS, None), IdleAge::Unknown);
        assert!(IdleAge::Unknown > IdleAge::Older);
    }

    /// Clocks move backwards (NTP, a snapshot copied between machines). A
    /// future timestamp must read as just-now, not wrap to an enormous age.
    #[test]
    fn a_future_timestamp_does_not_wrap_to_ancient() {
        let now = 10 * DAY_MS;
        assert_eq!(
            IdleAge::from_changed_at(now, Some(now + DAY_MS)),
            IdleAge::Day
        );
    }

    #[test]
    fn status_grouping_orders_blocked_then_working_then_idle_by_age() {
        let now = 100 * DAY_MS;
        let mut entries = vec![
            idle_entry("idle-old", 40 * DAY_MS, now),
            entry_at("working", AgentState::Working, true, Some(now)),
            idle_entry("idle-today", HOUR_MS, now),
            entry_at("blocked", AgentState::Blocked, true, Some(now)),
            entry_at("unknown-state", AgentState::Unknown, true, Some(now)),
        ];

        let groups = group_by_status(&mut entries, now, &|_| false);

        let order: Vec<&str> = entries.iter().map(|e| e.primary_label.as_str()).collect();
        assert_eq!(
            order,
            [
                "blocked",
                "working",
                "idle-today",
                "idle-old",
                "unknown-state"
            ]
        );
        assert_eq!(groups[0], AgentGroup::Blocked);
        assert_eq!(groups[2], AgentGroup::Idle(IdleAge::Day));
        assert_eq!(groups[3], AgentGroup::Idle(IdleAge::TwoMonths));
        assert_eq!(groups[4], AgentGroup::Unknown);
    }

    /// Unread output is the reason to look at an idle agent, so it leads its
    /// bucket rather than being buried by recency.
    #[test]
    fn unseen_agents_lead_their_bucket() {
        let now = 100 * DAY_MS;
        let mut entries = vec![
            entry_at("seen-newer", AgentState::Idle, true, Some(now - HOUR_MS)),
            entry_at(
                "unseen-older",
                AgentState::Idle,
                false,
                Some(now - 5 * HOUR_MS),
            ),
        ];

        group_by_status(&mut entries, now, &|_| false);

        assert_eq!(entries[0].primary_label, "unseen-older");
    }

    /// Offline agents used to keep their state group, so after they were sunk
    /// to the bottom the panel showed "idle · today", then other groups, then
    /// "idle · today" again for the unreachable host. Groups have to be one
    /// contiguous run each or every repeat draws a second heading.
    #[test]
    fn offline_agents_form_one_group_at_the_end() {
        let now = 100 * DAY_MS;
        let mut entries = vec![
            entry_at("remote-idle", AgentState::Idle, true, Some(now - HOUR_MS)),
            entry_at("local-idle", AgentState::Idle, true, Some(now - HOUR_MS)),
            entry_at("local-working", AgentState::Working, true, Some(now)),
            entry_at("remote-working", AgentState::Working, true, Some(now)),
        ];
        let offline = |entry: &AgentPanelEntry| entry.primary_label.starts_with("remote-");

        let groups = group_by_status(&mut entries, now, &offline);

        let order: Vec<&str> = entries.iter().map(|e| e.primary_label.as_str()).collect();
        assert_eq!(
            order,
            // Within the offline group, newest change first, as everywhere else.
            [
                "local-working",
                "local-idle",
                "remote-working",
                "remote-idle"
            ]
        );
        assert_eq!(groups[2], AgentGroup::Offline);
        assert_eq!(groups[3], AgentGroup::Offline);

        // No group may appear in two separate runs.
        let mut runs = Vec::new();
        for group in &groups {
            if runs.last() != Some(&group) {
                runs.push(group);
            }
        }
        let mut seen = runs.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(runs.len(), seen.len(), "a group was split into two runs");
    }

    #[test]
    fn offline_group_is_labelled_offline_not_by_stale_state() {
        assert_eq!(AgentGroup::Offline.label(), "offline");
    }

    /// `groups[i]` must describe `entries[i]` after sorting, or headers render
    /// against the wrong agents.
    #[test]
    fn groups_stay_aligned_with_entries() {
        let now = 100 * DAY_MS;
        let mut entries = vec![
            idle_entry("a", 40 * DAY_MS, now),
            entry_at("b", AgentState::Blocked, true, Some(now)),
            idle_entry("c", HOUR_MS, now),
        ];

        let groups = group_by_status(&mut entries, now, &|_| false);

        assert_eq!(groups.len(), entries.len());
        for (entry, group) in entries.iter().zip(&groups) {
            assert_eq!(AgentGroup::of_with_offline(entry, now, false), *group);
        }
    }
}
