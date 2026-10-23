use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentMarkParams, AgentPromptParams, AgentRenameParams, AgentSeenParams, AgentSendKeysParams,
    AgentStartParams, AgentTarget, PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

/// How much of an unsent draft an error message quotes.
const DRAFT_PREVIEW_CHARS: usize = 120;

fn draft_preview(draft: &str) -> String {
    let mut preview: String = draft.chars().take(DRAFT_PREVIEW_CHARS).collect();
    if draft.chars().count() > DRAFT_PREVIEW_CHARS {
        preview.push('…');
    }
    format!("{preview:?}")
}

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let mut agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        if let Ok(resolved) = self.resolve_agent_target(&target.target) {
            agent.input_line = self.agent_input(resolved.ws_idx, resolved.pane_id);
        }

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    /// Marks any pane, not only one running an agent: the mark key does, and a
    /// pane between agents keeps its mark for the next one. So the target is
    /// resolved as a terminal (pane id, terminal id, agent name or kind), and
    /// the answer is the pane rather than an `AgentInfo` that a plain shell
    /// would not have.
    pub(super) fn handle_agent_mark(&mut self, id: String, params: AgentMarkParams) -> String {
        let level = match params.level {
            None => None,
            Some(level @ 0..=3) => Some(crate::app::MarkLevel::from(level)),
            Some(level) => {
                return encode_error(
                    id,
                    "invalid_mark_level",
                    format!("mark level must be 0, 1, 2, 3 or null, got {level}"),
                )
            }
        };
        let resolved = match self.resolve_terminal_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        if !self.set_agent_mark(resolved.ws_idx, resolved.pane_id, level) {
            return agent_not_found(id, &params.target);
        }
        let Some(pane) = self.pane_info(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    /// Marks an agent seen or unseen where it is, without focusing it: a
    /// client reading the agent somewhere else (a phone, say) can say so
    /// without moving the focus in front of whoever sits at this herdr.
    pub(super) fn handle_agent_seen(&mut self, id: String, params: AgentSeenParams) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        match self.set_agent_seen(resolved.ws_idx, resolved.pane_id, params.seen) {
            Ok(()) => {}
            Err(crate::app::agents::AgentSeenError::NotIdle) => {
                return encode_error(
                    id,
                    "agent_not_idle",
                    format!(
                        "agent {} is not idle; only an idle agent can be marked unseen",
                        params.target
                    ),
                )
            }
            Err(crate::app::agents::AgentSeenError::NotFound) => {
                return agent_not_found(id, &params.target)
            }
        }
        match self.agent_info(resolved.ws_idx, resolved.pane_id) {
            Some(agent) => encode_success(id, ResponseResult::AgentInfo { agent }),
            None => agent_not_found(id, &params.target),
        }
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(super) fn handle_agent_prompt(&mut self, id: String, params: AgentPromptParams) -> String {
        if params.text.is_empty() {
            return encode_error(id, "empty_agent_prompt", "agent prompt must not be empty");
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return agent_not_found(id, &params.target);
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return encode_error(
                id,
                "agent_blocked",
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            );
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return agent_not_ready(id, &params.target);
        };
        if terminal.managed_agent_launch_pending() {
            return agent_not_ready(id, &params.target);
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            );
        }
        if !params.force {
            if let Some(input) = self.agent_input(resolved.ws_idx, resolved.pane_id) {
                if !input.draft.is_empty() {
                    return encode_error(
                        id,
                        "agent_has_draft",
                        format!(
                            "agent {} has unsent typed input ({}); sending would append to it. \
                             Pass --force to send anyway",
                            params.target,
                            draft_preview(&input.draft)
                        ),
                    );
                }
            }
        }
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => return encode_error(id, "agent_prompt_failed", err.to_string()),
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return encode_error(id, "agent_prompt_failed", err.to_string());
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(text)) {
            return encode_error(id, "agent_prompt_failed", err.to_string());
        }
        runtime.send_bytes_after(Bytes::from(enter), AGENT_PROMPT_SUBMIT_DELAY);
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        encode_success(id, ResponseResult::AgentPrompted { agent })
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let mut snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );
        // Detection reads stay raw: manifests are tuned against them.
        if params.format == crate::api::schema::ReadFormat::Text
            && params.source != crate::api::schema::ReadSource::Detection
            && params.ghost != crate::api::schema::GhostText::Keep
        {
            if let Some(input) = self.agent_input(resolved.ws_idx, resolved.pane_id) {
                snapshot.text = crate::app::api_helpers::apply_ghost_text(
                    &snapshot.text,
                    &input.suggestion,
                    params.ghost,
                );
            }
        }

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 1,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
                force: false,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = app.handle_agent_prompt(
            "req-raw".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                force: false,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert!(rx.try_recv().is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = app.handle_agent_prompt(
            "req-label".into(),
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
                force: false,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
                force: false,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                force: false,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
    }

    /// A Claude Code screen: a reply, then the prompt box with `prompt_row`
    /// drawn between two borders, then the status line.
    fn claude_screen(prompt_row: &str) -> Vec<u8> {
        let border = "\u{2500}".repeat(60);
        format!("\x1b[0mreply text\r\n{border}\r\n{prompt_row}\r\n{border}\r\n  status line")
            .into_bytes()
    }

    const SUGGESTION_ROW: &str = "\u{276f}\u{a0}\x1b[0m\x1b[2mit's rebuilt now\x1b[0m";
    const DRAFT_ROW: &str = "\u{276f}\u{a0}half typed note";

    /// Detecting Claude reads the global agent manifest cache, which the
    /// manifest tests repoint through `XDG_*`; hold the same lock meanwhile.
    fn app_with_claude_prompt(
        prompt_row: &str,
    ) -> (
        App,
        String,
        tokio::sync::mpsc::Receiver<Bytes>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        let (runtime, rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 4,
            );
        runtime.test_process_pty_bytes(&claude_screen(prompt_row));
        app.state.insert_test_runtime(pane_id, runtime);
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        (app, public_pane_id, rx, guard)
    }

    fn prompt_params(target: &str, force: bool) -> AgentPromptParams {
        AgentPromptParams {
            target: target.into(),
            text: "next".into(),
            wait: None,
            force,
        }
    }

    fn error_code(response: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(response).unwrap();
        value["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn agent_get_reports_suggestion_apart_from_draft() {
        let (mut app, pane, _rx, env) = app_with_claude_prompt(SUGGESTION_ROW);
        let response = app.handle_agent_get("req".into(), AgentTarget { target: pane });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info");
        };
        let input = agent.input_line.expect("claude prompt box is readable");
        assert_eq!(input.draft, "");
        assert_eq!(input.suggestion, "it's rebuilt now");

        drop(env);
        let (mut app, pane, _rx, _env) = app_with_claude_prompt(DRAFT_ROW);
        let response = app.handle_agent_get("req".into(), AgentTarget { target: pane });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info");
        };
        let input = agent.input_line.expect("claude prompt box is readable");
        assert_eq!(input.draft, "half typed note");
        assert_eq!(input.suggestion, "");
    }

    #[tokio::test]
    async fn agent_prompt_refuses_a_real_draft_unless_forced() {
        let (mut app, pane, mut rx, _env) = app_with_claude_prompt(DRAFT_ROW);

        let refused = app.handle_agent_prompt("req".into(), prompt_params(&pane, false));
        assert_eq!(error_code(&refused), "agent_has_draft");
        assert!(refused.contains("half typed note"));
        assert!(rx.try_recv().is_err(), "nothing may be written");

        let forced = app.handle_agent_prompt("req".into(), prompt_params(&pane, true));
        let forced: SuccessResponse = serde_json::from_str(&forced).unwrap();
        assert!(matches!(
            forced.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn agent_prompt_goes_ahead_over_a_mere_suggestion() {
        let (mut app, pane, mut rx, _env) = app_with_claude_prompt(SUGGESTION_ROW);

        let response = app.handle_agent_prompt("req".into(), prompt_params(&pane, false));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn agent_read_marks_or_drops_ghost_text() {
        use crate::api::schema::{AgentReadParams, GhostText, ReadFormat, ReadSource};

        let read = |ghost| {
            let (mut app, pane, _rx, _env) = app_with_claude_prompt(SUGGESTION_ROW);
            let response = app.handle_agent_read(
                "req".into(),
                AgentReadParams {
                    target: pane,
                    source: ReadSource::Visible,
                    lines: None,
                    format: ReadFormat::Text,
                    strip_ansi: true,
                    ghost,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::PaneRead { read } = success.result else {
                panic!("expected pane read");
            };
            read.text
        };

        assert!(read(GhostText::Keep).contains("\u{276f}\u{a0}it's rebuilt now"));
        let marked = read(GhostText::Mark);
        assert!(marked.contains("\u{276f}\u{a0}\u{27e8}suggestion: it's rebuilt now\u{27e9}"));
        let dropped = read(GhostText::Drop);
        assert!(!dropped.contains("rebuilt"));
        assert!(dropped.contains("\n\u{276f}\n"));
    }

    #[tokio::test]
    async fn agent_input_is_unknown_outside_claude() {
        let (mut app, pane, _rx, _env) = app_with_claude_prompt(DRAFT_ROW);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::OpenCode), AgentState::Idle);

        assert_eq!(app.agent_input(0, pane_id), None);
        let response = app.handle_agent_prompt("req".into(), prompt_params(&pane, false));
        assert!(!response.contains("agent_has_draft"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req-pending".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                force: false,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    /// The agent sits in a workspace nobody is looking at, `done`.
    fn app_with_done_agent_in_the_background() -> (App, crate::layout::PaneId, String) {
        let mut app = app_with_agent();
        app.state.workspaces.push(Workspace::test_new("front"));
        app.state.ensure_test_terminals();
        app.state.active = Some(1);
        app.state.selected = 1;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        let target = app.public_pane_id(0, pane_id).unwrap();
        (app, pane_id, target)
    }

    fn seen_response(app: &mut App, target: &str, seen: bool) -> String {
        app.handle_agent_seen(
            "req".into(),
            AgentSeenParams {
                target: target.to_string(),
                seen,
            },
        )
    }

    fn agent_from(response: &str) -> crate::api::schema::AgentInfo {
        let success: SuccessResponse = serde_json::from_str(response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response, got {response}");
        };
        agent
    }

    fn status_changes(app: &App) -> Vec<AgentStatus> {
        app.event_hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                crate::api::schema::EventData::PaneAgentStatusChanged { agent_status, .. } => {
                    Some(agent_status)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn agent_seen_marks_a_done_agent_idle_without_moving_focus() {
        let (mut app, pane_id, target) = app_with_done_agent_in_the_background();

        let agent = agent_from(&seen_response(&mut app, &target, true));

        assert_eq!(agent.agent_status, AgentStatus::Idle);
        assert!(app.state.workspaces[0].tabs[0].panes[&pane_id].seen);
        assert_eq!(app.state.active, Some(1), "focus stays where it was");
        assert_eq!(app.state.selected, 1);
        assert_eq!(status_changes(&app), vec![AgentStatus::Idle]);

        // Already seen: nothing changes, so nothing is announced.
        seen_response(&mut app, &target, true);
        assert_eq!(status_changes(&app), vec![AgentStatus::Idle]);
    }

    #[test]
    fn agent_seen_false_marks_an_idle_agent_done_again() {
        let (mut app, _pane_id, target) = app_with_done_agent_in_the_background();
        seen_response(&mut app, &target, true);

        let agent = agent_from(&seen_response(&mut app, &target, false));

        assert_eq!(agent.agent_status, AgentStatus::Done);
        assert_eq!(app.state.active, Some(1));
        assert_eq!(
            status_changes(&app),
            vec![AgentStatus::Idle, AgentStatus::Done]
        );
    }

    #[test]
    fn agent_seen_false_refuses_an_agent_that_is_not_idle() {
        let (mut app, pane_id, target) = app_with_done_agent_in_the_background();
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Working);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = true;

        let response = seen_response(&mut app, &target, false);

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_idle");
        assert!(app.state.workspaces[0].tabs[0].panes[&pane_id].seen);
        assert!(status_changes(&app).is_empty());
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }

    fn pane_updated_marks(app: &App) -> Vec<Option<u8>> {
        app.event_hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                crate::api::schema::EventData::PaneUpdated { pane } => Some(pane.agent_mark),
                _ => None,
            })
            .collect()
    }

    fn mark_response(app: &mut App, target: &str, level: Option<u8>) -> String {
        app.handle_agent_mark(
            "req".into(),
            AgentMarkParams {
                target: target.to_string(),
                level,
            },
        )
    }

    fn pane_from(response: &str) -> crate::api::schema::PaneInfo {
        let success: SuccessResponse = serde_json::from_str(response).unwrap();
        let ResponseResult::PaneInfo { pane } = success.result else {
            panic!("expected pane info response, got {response}");
        };
        pane
    }

    #[test]
    fn agent_mark_sets_and_clears_the_mark_and_announces_each_change() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let target = app.public_pane_id(0, pane_id).unwrap();
        app.state.session_dirty = false;

        let pane = pane_from(&mark_response(&mut app, &target, Some(2)));
        assert_eq!(pane.agent_mark, Some(2));
        assert_eq!(
            app.state.agent_mark(0, pane_id),
            Some(crate::app::MarkLevel::Medium)
        );
        assert!(app.state.session_dirty, "a mark is saved with the session");
        assert_eq!(app.pane_info(0, pane_id).unwrap().agent_mark, Some(2));

        let pane = pane_from(&mark_response(&mut app, &target, Some(0)));
        assert_eq!(pane.agent_mark, Some(0), "parked is a mark, not no mark");

        // Setting the level it already has changes nothing, so says nothing.
        pane_from(&mark_response(&mut app, &target, Some(0)));

        let pane = pane_from(&mark_response(&mut app, &target, None));
        assert_eq!(pane.agent_mark, None);
        assert!(
            app.state.agent_marks.is_empty(),
            "clearing leaves no dead key"
        );

        assert_eq!(pane_updated_marks(&app), vec![Some(2), Some(0), None]);
    }

    #[test]
    fn agent_mark_rejects_levels_past_high() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let target = app.public_pane_id(0, pane_id).unwrap();

        let response = mark_response(&mut app, &target, Some(4));
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_mark_level");
        assert!(app.state.agent_marks.is_empty());
        assert!(pane_updated_marks(&app).is_empty());
    }

    #[test]
    fn agent_mark_reports_an_unknown_target() {
        let mut app = app_with_agent();
        let response = mark_response(&mut app, "nobody", Some(3));
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
    }

    #[test]
    fn agent_mark_on_a_mirror_is_kept_under_the_remote_pane() {
        let mut app = app_with_agent();
        let mut workspace = Workspace::test_new("mirrored");
        workspace.remote_mirror = Some(crate::workspace::RemoteMirror {
            disconnected: false,
            target: "workbox".to_string(),
            origin_target: None,
            host_label: "wb".to_string(),
            host_color: None,
            key: "workbox\u{1f}w1".to_string(),
            remote_terminal: "term-1".to_string(),
        });
        workspace.tabs[0].remote_mirror = Some(crate::workspace::RemoteMirrorTab {
            key: "workbox\u{1f}w1\u{1f}term-1".to_string(),
            remote_terminal: "term-1".to_string(),
            remote_pane: "w1:p1".to_string(),
            disconnected: false,
        });
        app.state.workspaces.push(workspace);
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        let target = app.public_pane_id(1, pane_id).unwrap();

        let pane = pane_from(&mark_response(&mut app, &target, Some(3)));
        assert_eq!(pane.agent_mark, Some(3));
        assert_eq!(
            app.state
                .agent_marks
                .get("workbox\u{1f}w1\u{1f}term-1")
                .copied(),
            Some(crate::app::MarkLevel::High)
        );
    }

    #[test]
    fn the_mark_keys_announce_the_marks_they_set() {
        let mut app = app_with_agent();
        app.cycle_focused_agent_mark();
        assert_eq!(pane_updated_marks(&app), vec![Some(3)]);

        app.cycle_space_mark(0);
        let workspace_marks: Vec<_> = app
            .event_hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                crate::api::schema::EventData::WorkspaceUpdated { workspace } => {
                    Some(workspace.mark)
                }
                _ => None,
            })
            .collect();
        assert_eq!(workspace_marks, vec![Some(3)]);
        assert_eq!(app.workspace_info(0).mark, Some(3));
    }
}
