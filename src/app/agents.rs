use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{terminal_targets::TerminalTargetError, App};
use crate::api::schema::AgentStartParams;

const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
const INVALID_AGENT_TIMEOUT_MESSAGE: &str =
    "agent start timeout must be greater than 3000ms and at most 300000ms";
const INVALID_AGENT_NAME_MESSAGE: &str = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";

/// What became of a name typed at a local agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalAgentRename {
    Named,
    /// Nothing in this pane to name; the caller falls back to the pane label.
    NotAnAgent,
    /// Not a name an agent may have: lowercase letters, digits, `-` and `_`,
    /// starting with a letter, at most 32 characters.
    BadName,
    /// Another agent is already called this.
    Taken,
}

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn reconcile_managed_agent_target(&mut self, target: &str) {
        let Ok(resolved) = self.resolve_agent_target(target) else {
            return;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return;
        };
        let changed = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .is_some_and(|terminal| terminal.reconcile_managed_agent_at(Instant::now(), false));
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
            self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        }
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.mark_active_tab_seen();
        self.state.settle_terminal_mode_after_focus();
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    /// Applies a name typed at the rename-agent prompt, wherever it belongs.
    ///
    /// A mirrored agent is named on the machine that runs it, and the name
    /// comes back on the next poll as the host's answer. A local one is named
    /// here, through the same door the API uses. Returns whether there was one
    /// to apply.
    pub(super) fn apply_pending_agent_rename(&mut self) -> bool {
        let Some((ws_idx, pane_id, label)) = self.state.pending_agent_rename.take() else {
            return false;
        };
        // A name bound for another machine is checked here as well, because the
        // machine that would refuse it is not the one the user is looking at:
        // over there a bad name is a line in a log, and here it is nothing
        // happening at all.
        if self.pane_runs_an_agent(ws_idx, pane_id) && !valid_agent_name(&label) {
            self.warn_bad_agent_name(&label);
            return true;
        }
        #[cfg(unix)]
        let sent = self.request_remote_agent_rename(ws_idx, pane_id, Some(label.clone()));
        #[cfg(not(unix))]
        let sent = false;
        if sent {
            return true;
        }
        // Not a mirror: it is an agent running here, so name it the way the API
        // names one. Setting the pane's label instead -- which is what this did
        // -- put the name in a field the agent panel does not show, so nothing
        // appeared to happen.
        match self.rename_local_agent(ws_idx, pane_id, label.clone()) {
            LocalAgentRename::Named => {}
            // No agent here to name, so name the pane rather than dropping what
            // was typed.
            LocalAgentRename::NotAnAgent => {
                if let Some(terminal_id) = self
                    .state
                    .workspaces
                    .get(ws_idx)
                    .and_then(|workspace| workspace.terminal_id(pane_id))
                    .cloned()
                {
                    if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                        terminal.set_manual_label(label);
                        self.state.mark_session_dirty();
                    }
                }
            }
            LocalAgentRename::BadName => self.warn_bad_agent_name(&label),
            LocalAgentRename::Taken => {
                self.state.toast = Some(crate::app::state::ToastNotification {
                    kind: crate::app::state::ToastKind::NeedsAttention,
                    title: format!("another agent is already called {label}"),
                    context: "agent names are unique in a session".to_string(),
                    position: None,
                    target: None,
                });
            }
        }
        true
    }

    /// Whether this pane runs an agent, mirrored or not.
    ///
    /// A mirror's hook authority is the host's answer, so this is true of a
    /// mirrored agent as well -- which is the point: the name rules are the
    /// host's rules, and the check has to reach panes we do not run ourselves.
    fn pane_runs_an_agent(&self, ws_idx: usize, pane_id: crate::layout::PaneId) -> bool {
        self.state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
            .is_some_and(|terminal| {
                terminal.effective_agent_label().is_some()
                    && !terminal.managed_agent_launch_pending()
            })
    }

    /// Says a name was refused. A refusal that shows nothing is the same
    /// experience as the bug this replaces.
    fn warn_bad_agent_name(&mut self, label: &str) {
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::NeedsAttention,
            title: format!("{label} is not a name an agent can have"),
            context: "lowercase letters, digits, - and _, starting with a letter".to_string(),
            position: None,
            target: None,
        });
    }

    /// Names the agent in one pane, the way `agents.rename` does over the API.
    ///
    /// This is a different field from the pane's manual label, and the sidebar
    /// reads them through different tokens: `agent` shows the name set here,
    /// `pane` shows the label -- and only behind the agent's own title, which
    /// an agent rewrites constantly. Setting the label was what "rename agent"
    /// used to do for a local agent, so the name was stored and never seen.
    pub(super) fn rename_local_agent(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        name: String,
    ) -> LocalAgentRename {
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
        else {
            return LocalAgentRename::NotAnAgent;
        };
        let is_agent = self
            .state
            .terminals
            .get(&terminal_id)
            .is_some_and(|terminal| {
                terminal.effective_agent_label().is_some()
                    && !terminal.managed_agent_launch_pending()
            });
        if !is_agent {
            return LocalAgentRename::NotAnAgent;
        }
        if !valid_agent_name(&name) {
            return LocalAgentRename::BadName;
        }
        if !self
            .agent_name_conflicts(&name, &terminal_id.to_string())
            .is_empty()
        {
            return LocalAgentRename::Taken;
        }
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return LocalAgentRename::NotAnAgent;
        };
        terminal.set_agent_name(name);
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(ws_idx, pane_id);
        LocalAgentRename::Named
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<crate::api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_agent_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = match name {
            Some(name) if valid_agent_name(&name) => Some(name),
            Some(_) => return Err(AgentRenameError::InvalidName),
            None => None,
        };

        if let Some(name) = normalized_name.as_deref() {
            let conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id)
        else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(AgentRenameError::PendingLaunch);
        }
        if terminal.effective_agent_label().is_none() {
            return Err(AgentRenameError::NotAgent);
        }
        match normalized_name.clone() {
            Some(name) => terminal.set_agent_name(name),
            None => terminal.clear_agent_name(),
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        // A mirrored pane belongs to another machine, so the name is passed on
        // to it. The local write above stands until the next poll, which is
        // either the host agreeing or the host's own answer replacing it --
        // both of which are the truth, and neither of which a name written only
        // here would have survived. Passing it on is also what carries a rename
        // down a chain of hosts: each hop names its own mirror and forwards,
        // until the machine that really runs the agent hears it.
        #[cfg(unix)]
        self.request_remote_agent_rename(resolved.ws_idx, resolved.pane_id, normalized_name);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let name = params.name;
        if !valid_agent_name(&name) {
            return Err(AgentStartError::InvalidName);
        }
        let Some(kind) = crate::detect::parse_agent_label(&params.kind) else {
            return Err(AgentStartError::UnsupportedKind(params.kind));
        };
        if params
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(AgentStartError::InvalidArgument);
        }
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return Err(AgentStartError::TargetNotFound(params.pane_id));
        };
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let runtime = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        let shell_name = available_shell_name(runtime)
            .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;

        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args);
        let command = crate::platform::interactive_shell_command(&argv, &shell_name)
            .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64),
        );
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentStartError::InvalidTimeout);
        }

        let now = Instant::now();
        let terminal = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        terminal.begin_managed_agent(name.clone(), kind, now, AGENT_START_SETTLE_DELAY, timeout);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            terminal.clear_agent_name();
            return Err(AgentStartError::InputFailed(err.to_string()));
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();

        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or(AgentStartError::TargetUnavailable(params.pane_id))?;
        Ok((agent, argv))
    }

    pub(super) fn agent_start_error_body(
        &self,
        err: AgentStartError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentStartError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentStartError::UnsupportedKind(kind) => crate::api::schema::ErrorBody {
                code: "unsupported_agent_kind".into(),
                message: format!("unsupported interactive agent kind {kind}"),
            },
            AgentStartError::InvalidArgument => crate::api::schema::ErrorBody {
                code: "invalid_agent_argument".into(),
                message: "agent arguments cannot be encoded safely for the target shell".into(),
            },
            AgentStartError::InvalidTimeout => crate::api::schema::ErrorBody {
                code: "invalid_agent_timeout".into(),
                message: INVALID_AGENT_TIMEOUT_MESSAGE.into(),
            },
            AgentStartError::TargetNotFound(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_not_found".into(),
                message: format!("agent target pane {target} not found"),
            },
            AgentStartError::TargetBusy(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_busy".into(),
                message: format!("agent target pane {target} is not an available shell"),
            },
            AgentStartError::TargetUnavailable(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_unavailable".into(),
                message: format!("agent target pane {target} has no live terminal"),
            },
            AgentStartError::InputFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_input_failed".into(),
                message,
            },
            AgentStartError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_target_error_body(
        &self,
        err: TerminalTargetError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            TerminalTargetError::NotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: format!("agent target {target} not found"),
            },
            TerminalTargetError::Ambiguous { target, candidates } => {
                crate::api::schema::ErrorBody {
                    code: "agent_target_ambiguous".into(),
                    message: format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                }
            }
        }
    }

    pub(super) fn agent_rename_error_body(
        &self,
        err: AgentRenameError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error_body(err),
            AgentRenameError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentRenameError::NotAgent => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: "agent target does not currently host an agent".into(),
            },
            AgentRenameError::PendingLaunch => crate::api::schema::ErrorBody {
                code: "agent_launch_pending".into(),
                message: "agent name cannot change while startup is pending".into(),
            },
            AgentRenameError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        Some(crate::api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone(),
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session: pane.agent_session,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: terminal.managed_agent_launch_pending(),
            interactive_ready: terminal.managed_agent_interactive_ready(),
            state_change_seq: terminal.last_agent_state_change_seq.unwrap_or(0),
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
        })
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

fn available_shell_name(runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return Some("sh".into());
    }
    crate::platform::available_pane_shell(runtime.child_pid()?)
}

pub(super) fn runtime_hosts_agent(
    runtime: &crate::terminal::TerminalRuntime,
    expected: crate::detect::Agent,
) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return true;
    }
    live_runtime_agent(runtime) == Some(expected)
}

fn live_runtime_agent(runtime: &crate::terminal::TerminalRuntime) -> Option<crate::detect::Agent> {
    let job = crate::detect::foreground_job(runtime.child_pid()?)?;
    crate::detect::identify_agent_in_job(&job)
        .map(|(agent, _)| agent)
        .or_else(|| {
            job.processes
                .iter()
                .find_map(|process| crate::platform::process_agent_hint(process.pid))
        })
}

pub(super) enum AgentStartError {
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    InvalidTimeout,
    TargetNotFound(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    InvalidName,
    NotAgent,
    PendingLaunch,
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

#[cfg(test)]
mod tests {
    use super::valid_agent_name;
    use super::LocalAgentRename;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    /// One local space holding one pane with an agent running in it.
    fn app_with_a_local_agent() -> crate::app::App {
        let mut app = crate::app::tests::test_app();
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        app
    }

    fn only_pane(app: &crate::app::App) -> (crate::layout::PaneId, crate::terminal::TerminalId) {
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        (pane_id, terminal_id)
    }

    /// What the agent panel would draw for the first agent it lists. Asking the
    /// panel rather than a field is the point: the bug was a name stored in a
    /// field the panel does not read.
    fn agent_shown_first(app: &crate::app::App) -> Option<String> {
        crate::ui::agent_panel_entries(&app.state)
            .first()
            .and_then(|entry| entry.agent_label.clone())
    }

    /// The name has to land in the field the agent panel reads.
    ///
    /// This used to set the pane's manual label, which the panel shows only
    /// through the `pane` token and only when the agent is not reporting a
    /// title of its own -- so on a machine whose rows do not list that token,
    /// renaming an agent looked like it did nothing at all.
    #[test]
    fn naming_a_local_agent_names_the_agent_and_not_its_pane() {
        let mut app = app_with_a_local_agent();
        let (pane_id, terminal_id) = only_pane(&app);
        app.state.pending_agent_rename = Some((0, pane_id, "scarlet".to_string()));

        assert!(app.apply_pending_agent_rename());

        assert_eq!(
            agent_shown_first(&app).as_deref(),
            Some("scarlet"),
            "the agent panel has to show the name that was typed"
        );
        assert_eq!(
            app.state.terminals[&terminal_id].manual_label, None,
            "and the pane keeps whatever label it had"
        );
    }

    /// The whole way round, in one test: press the key, type a name, commit it,
    /// and read the panel that gets drawn -- then press the key again and check
    /// the box offers back the name that is on screen.
    ///
    /// Every earlier test here stopped at one end of that. They set a field and
    /// read the same field back, so a rename that wrote one field and displayed
    /// another satisfied all of them at once, twice: first when the name went
    /// to the pane's label instead of the agent's name, and again when the
    /// prompt prefilled from the label while the panel drew the name. Neither
    /// half is wrong on its own terms. What was wrong was the seam, and a test
    /// that never crosses a seam cannot see it.
    #[tokio::test]
    async fn naming_an_agent_shows_that_name_and_offers_it_back() {
        let mut app = app_with_a_local_agent();

        app.open_rename_focused_agent();
        app.state.name_input = "scarlet".into();
        app.handle_rename_key_via_api(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));
        assert!(
            app.apply_pending_agent_rename(),
            "committing the prompt should leave a rename to apply"
        );

        let drawn = crate::ui::agent_panel_rows_for_test(&app.state, 24, 8).join("\n");
        assert!(
            drawn.contains("scarlet"),
            "the panel should draw the name that was typed: {drawn:?}"
        );

        app.open_rename_focused_agent();
        assert_eq!(
            app.state.name_input, "scarlet",
            "and the prompt should offer back the name the panel is showing"
        );
    }

    /// Agent names are a small grammar, and a name outside it is refused. A
    /// refusal nobody can see is the same experience as the bug this replaces,
    /// so it has to say something.
    #[test]
    fn a_name_an_agent_cannot_have_is_refused_out_loud() {
        let mut app = app_with_a_local_agent();
        let (pane_id, terminal_id) = only_pane(&app);
        app.state.pending_agent_rename = Some((0, pane_id, "Scarlet Two".to_string()));

        assert!(app.apply_pending_agent_rename());

        let _ = terminal_id;
        assert_eq!(
            agent_shown_first(&app).as_deref(),
            Some("claude"),
            "the agent keeps the name it had"
        );
        assert!(
            app.state
                .toast
                .as_ref()
                .is_some_and(|toast| toast.title.contains("Scarlet Two")),
            "a refusal has to be visible"
        );
    }

    /// A pane with no agent in it still has a label worth setting, rather than
    /// losing what was typed.
    #[test]
    fn a_pane_with_no_agent_keeps_the_name_as_its_label() {
        let mut app = app_with_a_local_agent();
        let (pane_id, terminal_id) = only_pane(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(None, AgentState::Unknown);
        app.state.pending_agent_rename = Some((0, pane_id, "notes".to_string()));

        assert!(app.apply_pending_agent_rename());

        assert_eq!(
            app.state.terminals[&terminal_id].manual_label.as_deref(),
            Some("notes")
        );
    }

    /// Two agents in one session cannot share a name, and being told so beats
    /// the rename quietly doing nothing.
    #[test]
    fn a_name_another_agent_already_has_is_refused_out_loud() {
        let mut app = app_with_a_local_agent();
        app.state.workspaces.push(Workspace::test_new("other"));
        app.state.ensure_test_terminals();
        let other_pane = app.state.workspaces[1].tabs[0].root_pane;
        let other_terminal = app.state.workspaces[1]
            .terminal_id(other_pane)
            .unwrap()
            .clone();
        let terminal = app.state.terminals.get_mut(&other_terminal).unwrap();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        terminal.set_agent_name("scarlet".into());

        let (pane_id, terminal_id) = only_pane(&app);
        app.state.pending_agent_rename = Some((0, pane_id, "scarlet".to_string()));

        assert!(app.apply_pending_agent_rename());

        let _ = terminal_id;
        assert_eq!(agent_shown_first(&app).as_deref(), Some("claude"));
        assert!(app
            .state
            .toast
            .as_ref()
            .is_some_and(|toast| toast.title.contains("already called scarlet")));
    }

    /// And nothing pending is not something to apply.
    #[test]
    fn nothing_pending_applies_nothing() {
        let mut app = app_with_a_local_agent();
        assert!(!app.apply_pending_agent_rename());
    }

    #[allow(unused)]
    fn _local_agent_rename_variants_are_used(r: LocalAgentRename) -> bool {
        matches!(r, LocalAgentRename::Named)
    }

    #[test]
    fn agent_names_use_a_small_cli_safe_grammar() {
        for name in ["a", "reviewer-one", "reviewer_2", &"a".repeat(32)] {
            assert!(valid_agent_name(name), "expected {name:?} to be valid");
        }
        for name in [
            "",
            " reviewer",
            "reviewer ",
            "reviewer one",
            "Reviewer",
            "1reviewer",
            "reviewer.one",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "expected {name:?} to be invalid");
        }
    }
}
