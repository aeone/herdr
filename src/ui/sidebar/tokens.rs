use super::AgentPanelEntry;
use crate::config::{
    AgentSidebarToken, AgentsSidebarConfig, SidebarTokenStyle, SpaceSidebarToken,
    SpacesSidebarConfig,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedToken {
    pub kind: ResolvedTokenKind,
    pub style: SidebarTokenStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResolvedTokenKind {
    StateIcon,
    StateText(String),
    Number(String),
    Workspace(String),
    Tab(String),
    Pane(String),
    Agent(String),
    RemoteHost {
        text: String,
        /// Configured host colour, resolved; None falls back to a hashed hue.
        color: Option<ratatui::style::Color>,
    },
    TerminalTitle(String),
    Branch(String),
    GitStatus {
        ahead: usize,
        behind: usize,
    },
    Custom(String),
}

impl ResolvedToken {
    fn new(kind: ResolvedTokenKind, style: SidebarTokenStyle) -> Self {
        Self { kind, style }
    }

    #[cfg(test)]
    pub(super) fn unstyled(kind: ResolvedTokenKind) -> Self {
        Self::new(kind, SidebarTokenStyle::default())
    }
}

pub(super) fn agent_rows(
    config: &AgentsSidebarConfig,
    entry: &AgentPanelEntry,
    state_text: &str,
    switch_label: Option<&str>,
) -> Vec<Vec<ResolvedToken>> {
    config
        .rows_for_agent(entry.agent)
        .iter()
        .filter_map(|row| {
            let resolved =
                row.iter()
                    .filter_map(|configured| {
                        let (token, style) = configured.parts();
                        let kind = match token {
                            AgentSidebarToken::StateIcon => Some(ResolvedTokenKind::StateIcon),
                            AgentSidebarToken::StateText => {
                                Some(ResolvedTokenKind::StateText(state_text.to_string()))
                            }
                            AgentSidebarToken::Number => {
                                Some(ResolvedTokenKind::Number(switch_label_text(switch_label)))
                            }
                            AgentSidebarToken::Workspace => {
                                Some(ResolvedTokenKind::Workspace(entry.primary_label.clone()))
                            }
                            AgentSidebarToken::Tab => {
                                entry.primary_tab_label.clone().map(ResolvedTokenKind::Tab)
                            }
                            AgentSidebarToken::Pane => {
                                entry.pane_label.clone().map(ResolvedTokenKind::Pane)
                            }
                            AgentSidebarToken::Agent => {
                                entry.agent_label.clone().map(ResolvedTokenKind::Agent)
                            }
                            AgentSidebarToken::RemoteHost => {
                                entry.remote_host.clone().map(|text| {
                                    ResolvedTokenKind::RemoteHost {
                                        text,
                                        color: entry.remote_host_color,
                                    }
                                })
                            }
                            AgentSidebarToken::TerminalTitle => entry
                                .terminal_title
                                .clone()
                                .map(ResolvedTokenKind::TerminalTitle),
                            AgentSidebarToken::TerminalTitleStripped => entry
                                .terminal_title_stripped
                                .clone()
                                .map(ResolvedTokenKind::TerminalTitle),
                            AgentSidebarToken::Custom(name) => entry
                                .tokens
                                .get(name)
                                .cloned()
                                .map(ResolvedTokenKind::Custom),
                            AgentSidebarToken::Styled { .. } => None,
                        }?;
                        Some(ResolvedToken::new(kind, style))
                    })
                    .collect::<Vec<_>>();
            (!resolved.is_empty()).then_some(resolved)
        })
        .collect()
}

pub(super) struct SpaceTokenContext<'a> {
    pub workspace: &'a str,
    /// Jump label for this space, when it has one.
    pub switch_label: Option<&'a str>,
    /// Host prefix when this space mirrors a remote Herdr.
    pub remote_host: Option<&'a str>,
    /// Resolved colour for that prefix, if the host configured one.
    pub remote_host_color: Option<ratatui::style::Color>,
    pub branch: Option<&'a str>,
    pub state_text: &'a str,
    pub ahead_behind: Option<(usize, usize)>,
    pub tokens: &'a std::collections::HashMap<String, String>,
    pub suppress_git_details: bool,
}

pub(super) fn space_rows(
    config: &SpacesSidebarConfig,
    context: SpaceTokenContext<'_>,
) -> Vec<Vec<ResolvedToken>> {
    config
        .rows
        .iter()
        .filter_map(|row| {
            let resolved = row
                .iter()
                .filter_map(|configured| {
                    let (token, style) = configured.parts();
                    let kind = match token {
                        SpaceSidebarToken::StateIcon => Some(ResolvedTokenKind::StateIcon),
                        SpaceSidebarToken::StateText => {
                            Some(ResolvedTokenKind::StateText(context.state_text.to_string()))
                        }
                        SpaceSidebarToken::Number => Some(ResolvedTokenKind::Number(
                            switch_label_text(context.switch_label),
                        )),
                        SpaceSidebarToken::Workspace => {
                            Some(ResolvedTokenKind::Workspace(context.workspace.to_string()))
                        }
                        SpaceSidebarToken::RemoteHost => {
                            context
                                .remote_host
                                .map(|host| ResolvedTokenKind::RemoteHost {
                                    text: host.to_string(),
                                    color: context.remote_host_color,
                                })
                        }
                        SpaceSidebarToken::Branch if !context.suppress_git_details => context
                            .branch
                            .map(|branch| ResolvedTokenKind::Branch(branch.to_string())),
                        SpaceSidebarToken::Branch => None,
                        SpaceSidebarToken::GitStatus if !context.suppress_git_details => context
                            .ahead_behind
                            .filter(|(ahead, behind)| *ahead > 0 || *behind > 0)
                            .map(|(ahead, behind)| ResolvedTokenKind::GitStatus { ahead, behind }),
                        SpaceSidebarToken::GitStatus => None,
                        SpaceSidebarToken::Custom(name) => context
                            .tokens
                            .get(name)
                            .cloned()
                            .map(ResolvedTokenKind::Custom),
                        SpaceSidebarToken::Styled { .. } => None,
                    }?;
                    Some(ResolvedToken::new(kind, style))
                })
                .collect::<Vec<_>>();
            (!resolved.is_empty()).then_some(resolved)
        })
        .collect()
}

/// The jump label as it is drawn, or a space holding its column.
///
/// Only the first entries get a label, so omitting the token entirely would
/// pull every row past them two columns left and break the alignment the label
/// is there to provide.
fn switch_label_text(switch_label: Option<&str>) -> String {
    switch_label.unwrap_or(" ").to_string()
}

pub(super) fn separator(previous: &ResolvedToken, current: &ResolvedToken) -> &'static str {
    if matches!(
        previous.kind,
        ResolvedTokenKind::StateIcon | ResolvedTokenKind::Number(_)
    ) || matches!(current.kind, ResolvedTokenKind::GitStatus { .. })
    {
        // The number reads as a label on the row, like the state icon, so it
        // gets a plain space rather than the " · " separator.
        " "
    } else if matches!(previous.kind, ResolvedTokenKind::RemoteHost { .. }) {
        // The host reads as a tight prefix on the name that follows it, so the
        // dot hugs both sides: "sera·lifestream", not "sera · lifestream".
        "·"
    } else {
        " · "
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentSidebarToken, SpaceSidebarToken};
    use crate::detect::AgentState;

    fn entry() -> AgentPanelEntry {
        AgentPanelEntry {
            remote_host: None,
            remote_host_color: None,
            ws_idx: 0,
            tab_idx: 0,
            pane_id: crate::layout::PaneId::from_raw(1),
            primary_label: "repo".into(),
            primary_tab_label: None,
            pane_label: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_label: Some("pi".into()),
            agent_kind_label: Some("pi".into()),
            agent: Some(crate::detect::Agent::Pi),
            state: AgentState::Working,
            seen: true,
            last_agent_state_change_seq: None,
            agent_state_changed_at_ms: None,
            state_labels: std::collections::HashMap::new(),
            tokens: std::collections::HashMap::new(),
        }
    }

    /// Entries past the ninth have no switch number, and the column has to stay
    /// reserved or their text sits two columns left of everything above it.
    #[test]
    fn an_absent_switch_number_still_holds_its_column() {
        let entry = entry();
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::StateIcon,
                AgentSidebarToken::Number,
                AgentSidebarToken::Workspace,
            ]],
            rows_by_agent: Default::default(),
            row_gap: 0,
        };

        let numbered = agent_rows(&config, &entry, "idle", Some("3"));
        let unnumbered = agent_rows(&config, &entry, "idle", None);

        assert_eq!(numbered[0][1].kind, ResolvedTokenKind::Number("3".into()));
        assert_eq!(unnumbered[0][1].kind, ResolvedTokenKind::Number(" ".into()));
        // Same token count either way, so the workspace name lands in the same
        // column in both rows.
        assert_eq!(numbered[0].len(), unnumbered[0].len());
    }

    #[test]
    fn missing_custom_tokens_elide_rows_and_separators() {
        let entry = entry();
        let config = AgentsSidebarConfig {
            rows: vec![
                vec![
                    AgentSidebarToken::StateIcon,
                    AgentSidebarToken::Custom("missing".into()),
                ],
                vec![AgentSidebarToken::Custom("missing".into())],
                vec![AgentSidebarToken::Agent],
            ],
            ..Default::default()
        };

        let rows = agent_rows(&config, &entry, "working", None);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec![ResolvedToken::unstyled(ResolvedTokenKind::StateIcon)]
        );
        assert_eq!(
            rows[1],
            vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "pi".into()
            ))]
        );
    }

    #[test]
    fn state_text_and_arbitrary_values_are_independent_tokens() {
        let mut entry = entry();
        entry
            .tokens
            .insert("summary".into(), "reviewing auth".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::StateText,
                AgentSidebarToken::Custom("summary".into()),
            ]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &entry, "deep in the mines", None),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::StateText("deep in the mines".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Custom("reviewing auth".into())),
            ]]
        );
    }

    #[test]
    fn terminal_title_builtins_are_distinct_from_custom_tokens() {
        let mut entry = entry();
        entry.terminal_title = Some("⠋ raw title".into());
        entry.terminal_title_stripped = Some("raw title".into());
        entry
            .tokens
            .insert("terminal_title".into(), "custom title".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::TerminalTitle,
                AgentSidebarToken::TerminalTitleStripped,
                AgentSidebarToken::Custom("terminal_title".into()),
            ]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &entry, "working", None),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::TerminalTitle("⠋ raw title".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::TerminalTitle("raw title".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Custom("custom title".into())),
            ]]
        );
    }

    #[test]
    fn known_agent_override_replaces_default_rows() {
        let mut config = AgentsSidebarConfig {
            rows: vec![vec![AgentSidebarToken::Workspace]],
            ..Default::default()
        };
        config
            .rows_by_agent
            .insert("pi".into(), vec![vec![AgentSidebarToken::Agent]]);
        let mut pi = entry();
        pi.agent_label = Some("renamed pi".into());

        assert_eq!(
            agent_rows(&config, &pi, "working", None),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "renamed pi".into()
            ))]]
        );

        pi.agent = None;
        assert_eq!(
            agent_rows(&config, &pi, "working", None),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Workspace(
                "repo".into()
            ))]]
        );
    }

    #[test]
    fn grouped_children_suppress_all_builtin_git_details() {
        let config = SpacesSidebarConfig::default();

        assert_eq!(
            space_rows(
                &config,
                SpaceTokenContext {
                    switch_label: None,
                    remote_host: None,
                    remote_host_color: None,
                    workspace: "feature",
                    branch: Some("worktree/feature"),
                    state_text: "idle",
                    ahead_behind: Some((2, 1)),
                    tokens: &std::collections::HashMap::new(),
                    suppress_git_details: true,
                },
            ),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::StateIcon),
                ResolvedToken::unstyled(ResolvedTokenKind::Workspace("feature".into())),
            ]]
        );
    }

    #[test]
    fn workspace_custom_token_can_replace_git_specific_details() {
        let tokens = std::collections::HashMap::from([("jj_status".into(), "2 changes".into())]);
        let config = SpacesSidebarConfig {
            rows: vec![vec![SpaceSidebarToken::Custom("jj_status".into())]],
            ..Default::default()
        };

        assert_eq!(
            space_rows(
                &config,
                SpaceTokenContext {
                    switch_label: None,
                    remote_host: None,
                    remote_host_color: None,
                    workspace: "repo",
                    branch: None,
                    state_text: "idle",
                    ahead_behind: None,
                    tokens: &tokens,
                    suppress_git_details: false,
                },
            ),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Custom(
                "2 changes".into()
            ))]]
        );
    }
    #[test]
    fn number_token_shows_the_switch_position_or_holds_its_column() {
        let config = SpacesSidebarConfig {
            rows: vec![vec![
                SpaceSidebarToken::Number,
                SpaceSidebarToken::Workspace,
            ]],
            ..SpacesSidebarConfig::default()
        };
        let tokens = std::collections::HashMap::new();
        let ctx = |n: Option<&'static str>| SpaceTokenContext {
            switch_label: n,
            workspace: "repo",
            remote_host: None,
            remote_host_color: None,
            branch: None,
            state_text: "idle",
            ahead_behind: None,
            tokens: &tokens,
            suppress_git_details: false,
        };

        let numbered = space_rows(&config, ctx(Some("3")));
        assert_eq!(numbered[0][0].kind, ResolvedTokenKind::Number("3".into()));

        // No position (past 9) -> the column is held by a blank rather than
        // dropped, so the workspace name stays aligned with the rows above it.
        let unnumbered = space_rows(&config, ctx(None));
        assert_eq!(unnumbered[0][0].kind, ResolvedTokenKind::Number(" ".into()));
        assert_eq!(
            unnumbered[0][1].kind,
            ResolvedTokenKind::Workspace("repo".into())
        );
    }

    #[test]
    fn host_prefix_hugs_the_name_with_a_tight_dot() {
        let host = ResolvedToken::unstyled(ResolvedTokenKind::RemoteHost {
            text: "sera".into(),
            color: None,
        });
        let name = ResolvedToken::unstyled(ResolvedTokenKind::Workspace("lifestream".into()));

        assert_eq!(separator(&host, &name), "\u{b7}");
    }

    #[test]
    fn number_gets_a_plain_space_not_a_dot() {
        let number = ResolvedToken::unstyled(ResolvedTokenKind::Number("3".into()));
        let icon = ResolvedToken::unstyled(ResolvedTokenKind::StateIcon);
        assert_eq!(separator(&number, &icon), " ");
    }

    #[test]
    fn ordinary_tokens_keep_spaced_dots() {
        let branch = ResolvedToken::unstyled(ResolvedTokenKind::Branch("main".into()));
        let workspace = ResolvedToken::unstyled(ResolvedTokenKind::Workspace("repo".into()));

        assert_eq!(separator(&workspace, &branch), " \u{b7} ");
    }

    #[test]
    fn a_configured_host_colour_rides_on_the_resolved_token() {
        let config = SpacesSidebarConfig {
            rows: vec![vec![
                SpaceSidebarToken::RemoteHost,
                SpaceSidebarToken::Workspace,
            ]],
            ..SpacesSidebarConfig::default()
        };
        let tokens = std::collections::HashMap::new();
        let rows = space_rows(
            &config,
            SpaceTokenContext {
                switch_label: None,
                workspace: "lifestream",
                remote_host: Some("sera"),
                remote_host_color: Some(ratatui::style::Color::Blue),
                branch: None,
                state_text: "idle",
                ahead_behind: None,
                tokens: &tokens,
                suppress_git_details: false,
            },
        );

        let host = &rows[0][0];
        assert_eq!(
            host.kind,
            ResolvedTokenKind::RemoteHost {
                text: "sera".into(),
                color: Some(ratatui::style::Color::Blue),
            }
        );
    }
}
