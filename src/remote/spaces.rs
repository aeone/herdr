//! Discovery for remote Herdr agent panes mirrored into the local Space list.
//!
//! The local server polls each configured host over SSH, asks the remote Herdr
//! for its workspaces and panes through the normal JSON API, and reports the
//! agent panes it found. Turning those into local workspaces is the caller's
//! job; this module only reads.

// The reconcile loop that turns these snapshots into local workspaces is not
// wired up yet. Until it is, nothing outside the module's own tests calls in.
#![allow(dead_code)]

use std::io;

use crate::api::schema::response::ResponseResult;
use crate::config::RemoteSpaceConfig;

use super::unix::RemoteSsh;

/// One remote agent pane, reduced to what a mirrored local pane needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteAgentPane {
    /// Remote terminal id, the argument `herdr terminal attach` takes.
    pub(crate) terminal_id: String,
    /// Remote workspace id, used to keep mirrors stable across polls.
    pub(crate) workspace_id: String,
    /// Remote workspace name, or the workspace id when the name is unknown.
    pub(crate) workspace_label: String,
    /// Detected or reported agent name, when the remote knows one.
    pub(crate) agent: Option<String>,
}

impl RemoteAgentPane {
    /// Stable identity for a mirrored pane, so a poll that returns the same
    /// remote pane reuses its local workspace instead of recreating it.
    pub(crate) fn mirror_key(&self, target: &str) -> String {
        format!(
            "{target}\u{1f}{}\u{1f}{}",
            self.workspace_id, self.terminal_id
        )
    }

    /// Name for the mirrored local space, prefixed so it reads as remote.
    pub(crate) fn mirror_label(&self, label: &str) -> String {
        format!("{label}/{}", self.workspace_label)
    }
}

/// Argv a mirrored pane runs to attach to the remote pane's terminal.
///
/// `-t` forces a PTY so the remote attach client sees a real terminal, which
/// it needs to forward raw input.
pub(crate) fn attach_argv(
    space: &RemoteSpaceConfig,
    pane: &RemoteAgentPane,
    remote_herdr: &str,
) -> Vec<String> {
    let mut argv = vec!["ssh".to_string(), "-t".to_string(), space.target.clone()];
    let mut remote = String::new();
    if let Some(session) = space.session.as_deref() {
        remote.push_str(&format!(
            "{}={} ",
            crate::session::SESSION_ENV_VAR,
            shell_quote(session)
        ));
    }
    remote.push_str(&format!(
        "{} terminal attach {}",
        shell_quote(remote_herdr),
        shell_quote(&pane.terminal_id)
    ));
    argv.push(remote);
    argv
}

/// What one poll of a host found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSpaceSnapshot {
    /// Remote path to the herdr binary, reused when building attach commands.
    pub(crate) remote_herdr: String,
    pub(crate) panes: Vec<RemoteAgentPane>,
}

/// Polls one configured host and returns its agent panes.
///
/// Errors are the caller's signal that the host is unreachable or has no
/// running Herdr; mirrored spaces for that host should be left alone rather
/// than torn down, so a flaky link does not thrash the sidebar.
pub(crate) fn discover(
    space: &RemoteSpaceConfig,
    manage_ssh_config: bool,
) -> io::Result<RemoteSpaceSnapshot> {
    let ssh = RemoteSsh::new(space.target.clone(), manage_ssh_config);
    let output = ssh.sh_output(&discovery_script(space.session.as_deref()))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "remote space discovery failed on {}: {}",
            space.target,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_discovery_output(&String::from_utf8_lossy(&output.stdout))
}

/// Shell run on the remote host. It resolves a herdr binary the same way a
/// non-login `ssh host herdr ...` would fail to, then prints the binary path
/// followed by the two JSON list responses, one per line.
fn discovery_script(session: Option<&str>) -> String {
    let mut script = String::from(
        r#"set -e
herdr_bin=$(command -v herdr 2>/dev/null || true)
if [ -z "$herdr_bin" ]; then
  for candidate in "${HOME:-}/.local/bin/herdr" "${HOME:-}/.cargo/bin/herdr" /usr/local/bin/herdr /opt/homebrew/bin/herdr; do
    if [ -x "$candidate" ]; then
      herdr_bin=$candidate
      break
    fi
  done
fi
if [ -z "$herdr_bin" ]; then
  echo "no herdr binary found on remote PATH" >&2
  exit 127
fi
"#,
    );
    if let Some(session) = session {
        script.push_str(&format!(
            "{}={}\nexport {}\n",
            crate::session::SESSION_ENV_VAR,
            shell_quote(session),
            crate::session::SESSION_ENV_VAR
        ));
    }
    script.push_str(
        r#"printf '%s\n' "$herdr_bin"
"$herdr_bin" workspace list
"$herdr_bin" pane list
"#,
    );
    script
}

/// Parses the three-line discovery output: binary path, workspace list JSON,
/// pane list JSON.
fn parse_discovery_output(stdout: &str) -> io::Result<RemoteSpaceSnapshot> {
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let remote_herdr = lines
        .next()
        .ok_or_else(|| io::Error::other("remote space discovery returned no output"))?
        .trim()
        .to_string();
    let workspaces_line = lines
        .next()
        .ok_or_else(|| io::Error::other("remote space discovery returned no workspace list"))?;
    let panes_line = lines
        .next()
        .ok_or_else(|| io::Error::other("remote space discovery returned no pane list"))?;

    let workspace_labels = parse_workspace_labels(workspaces_line)?;
    let panes = parse_agent_panes(panes_line, &workspace_labels)?;
    Ok(RemoteSpaceSnapshot {
        remote_herdr,
        panes,
    })
}

fn parse_result(line: &str, what: &str) -> io::Result<ResponseResult> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|err| io::Error::other(format!("remote {what} was not valid JSON: {err}")))?;
    if let Some(error) = value.get("error") {
        return Err(io::Error::other(format!("remote {what} failed: {error}")));
    }
    let result = value
        .get("result")
        .ok_or_else(|| io::Error::other(format!("remote {what} had no result")))?;
    serde_json::from_value(result.clone())
        .map_err(|err| io::Error::other(format!("remote {what} had an unexpected shape: {err}")))
}

fn parse_workspace_labels(line: &str) -> io::Result<std::collections::HashMap<String, String>> {
    let ResponseResult::WorkspaceList { workspaces } = parse_result(line, "workspace list")? else {
        return Err(io::Error::other(
            "remote workspace list had an unexpected result type",
        ));
    };
    Ok(workspaces
        .into_iter()
        .map(|workspace| (workspace.workspace_id, workspace.label))
        .collect())
}

fn parse_agent_panes(
    line: &str,
    workspace_labels: &std::collections::HashMap<String, String>,
) -> io::Result<Vec<RemoteAgentPane>> {
    let ResponseResult::PaneList { panes } = parse_result(line, "pane list")? else {
        return Err(io::Error::other(
            "remote pane list had an unexpected result type",
        ));
    };
    Ok(panes
        .into_iter()
        // Only panes the remote recognizes as agents are worth mirroring; a
        // bare shell is not something you want cluttering the local sidebar.
        .filter(|pane| pane.agent.is_some() || pane.display_agent.is_some())
        .map(|pane| {
            let workspace_label = workspace_labels
                .get(&pane.workspace_id)
                .cloned()
                .unwrap_or_else(|| pane.workspace_id.clone());
            RemoteAgentPane {
                terminal_id: pane.terminal_id,
                workspace_id: pane.workspace_id,
                workspace_label,
                agent: pane.display_agent.or(pane.agent),
            }
        })
        .collect())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(target: &str) -> RemoteSpaceConfig {
        RemoteSpaceConfig {
            target: target.to_string(),
            session: None,
            label: None,
            poll_seconds: 30,
        }
    }

    fn workspace_list_json() -> String {
        serde_json::json!({
            "id": "cli:workspace:list",
            "result": {
                "type": "workspace_list",
                "workspaces": [{
                    "workspace_id": "w1",
                    "number": 1,
                    "label": "api-server",
                    "focused": true,
                    "pane_count": 1,
                    "tab_count": 1,
                    "active_tab_id": "w1t1",
                    "agent_status": "working"
                }]
            }
        })
        .to_string()
    }

    fn pane_list_json() -> String {
        serde_json::json!({
            "id": "cli:pane:list",
            "result": {
                "type": "pane_list",
                "panes": [
                    {
                        "pane_id": "w1t1p1",
                        "terminal_id": "term-1",
                        "workspace_id": "w1",
                        "tab_id": "w1t1",
                        "focused": true,
                        "agent": "claude",
                        "agent_status": "working",
                        "revision": 1
                    },
                    {
                        "pane_id": "w1t1p2",
                        "terminal_id": "term-2",
                        "workspace_id": "w1",
                        "tab_id": "w1t1",
                        "focused": false,
                        "agent_status": "unknown",
                        "revision": 1
                    }
                ]
            }
        })
        .to_string()
    }

    /// Captured from `herdr` on a real macOS host, where most panes are plain
    /// shells and agent panes carry a nested `agent_session` object. The live
    /// host reported 26 panes across 23 workspaces with only 4 agents, which is
    /// the ratio that makes the agent-only filter worth having.
    #[test]
    fn discovery_ignores_shell_panes_and_tolerates_agent_session_metadata() {
        let workspaces = r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"active_tab_id":"w1:t1","agent_status":"unknown","focused":false,"label":"lifestream","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"},{"active_tab_id":"wK:t1","agent_status":"idle","focused":false,"label":"baby-names","number":17,"pane_count":1,"tab_count":1,"workspace_id":"wK"},{"active_tab_id":"wV:t1","agent_status":"blocked","focused":true,"label":"save-reddit","number":22,"pane_count":1,"tab_count":1,"workspace_id":"wV"}]}}"#;
        let panes = r#"{"id":"cli:pane:list","result":{"panes":[{"agent_status":"unknown","cwd":"/Users/ryielle/lifestream","focused":false,"pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","terminal_id":"term_shell","workspace_id":"w1"},{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"2ed90e44-493d-4c47-97d1-55c55c9ba47d"},"agent_status":"idle","cwd":"/Users/ryielle/lifestream/baby-names","focused":false,"pane_id":"wK:p1","revision":0,"tab_id":"wK:t1","terminal_id":"term_baby","workspace_id":"wK"},{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"cd06f84f-138f-4c9f-86e8-bceecfe1ef1e"},"agent_status":"blocked","cwd":"/Users/ryielle/lifestream","focused":true,"pane_id":"wV:p1","revision":0,"tab_id":"wV:t1","terminal_id":"term_reddit","workspace_id":"wV"}],"type":"pane_list"}}"#;
        let stdout = format!("/opt/homebrew/bin/herdr\n{workspaces}\n{panes}\n");

        let snapshot = parse_discovery_output(&stdout).expect("parses live macOS output");

        assert_eq!(snapshot.remote_herdr, "/opt/homebrew/bin/herdr");
        assert_eq!(
            snapshot
                .panes
                .iter()
                .map(|pane| pane.workspace_label.as_str())
                .collect::<Vec<_>>(),
            ["baby-names", "save-reddit"]
        );
    }

    #[test]
    fn discovery_keeps_agent_panes_and_resolves_workspace_labels() {
        let stdout = format!(
            "/home/you/.local/bin/herdr\n{}\n{}\n",
            workspace_list_json(),
            pane_list_json()
        );

        let snapshot = parse_discovery_output(&stdout).expect("parses");

        assert_eq!(snapshot.remote_herdr, "/home/you/.local/bin/herdr");
        assert_eq!(
            snapshot.panes,
            vec![RemoteAgentPane {
                terminal_id: "term-1".into(),
                workspace_id: "w1".into(),
                workspace_label: "api-server".into(),
                agent: Some("claude".into()),
            }]
        );
    }

    /// Captured from `herdr 0.7.5` on a real remote host, so the parser is
    /// pinned to output the CLI actually produces: keys in serialized order
    /// with the `type` tag trailing, and a shell pane with no agent field.
    #[test]
    fn discovery_parses_a_captured_live_remote_response() {
        let stdout = concat!(
            "/home/ryi/.local/bin/herdr\n",
            r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"active_tab_id":"w1:t1","agent_status":"idle","focused":false,"label":"lifestream","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"},{"active_tab_id":"w2:t1","agent_status":"unknown","focused":true,"label":"lifestream","number":2,"pane_count":1,"tab_count":1,"workspace_id":"w2"},{"active_tab_id":"w3:t1","agent_status":"done","focused":false,"label":"emf","number":3,"pane_count":1,"tab_count":1,"workspace_id":"w3"}]}}"#,
            "\n",
            r#"{"id":"cli:pane:list","result":{"panes":[{"agent":"claude","agent_status":"idle","cwd":"/home/ryi/lifestream","focused":false,"foreground_cwd":"/home/ryi/lifestream","pane_id":"w1:p1","revision":12,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":50},"tab_id":"w1:t1","terminal_id":"term_656ccaeee911e1","terminal_title":"View Google Calendar access","terminal_title_stripped":"View Google Calendar access","workspace_id":"w1"},{"agent_status":"unknown","cwd":"/home/ryi/lifestream","focused":true,"foreground_cwd":"/home/ryi/lifestream","pane_id":"w2:p1","revision":14,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":50},"tab_id":"w2:t1","terminal_id":"term_656ccbdfeb7f82","terminal_title":"ryi@sera:~/lifestream","terminal_title_stripped":"ryi@sera:~/lifestream","workspace_id":"w2"},{"agent":"claude","agent_status":"done","cwd":"/home/ryi/lifestream/emf","focused":false,"foreground_cwd":"/home/ryi/lifestream/emf","pane_id":"w3:p1","revision":4,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":50},"tab_id":"w3:t1","terminal_id":"term_656e5c429826d3","terminal_title":"Configure Herdr keybindings","terminal_title_stripped":"Configure Herdr keybindings","workspace_id":"w3"}],"type":"pane_list"}}"#,
            "\n",
        );

        let snapshot = parse_discovery_output(stdout).expect("parses live output");

        // The shell pane in w2 carries no agent and must not be mirrored.
        assert_eq!(
            snapshot.panes,
            vec![
                RemoteAgentPane {
                    terminal_id: "term_656ccaeee911e1".into(),
                    workspace_id: "w1".into(),
                    workspace_label: "lifestream".into(),
                    agent: Some("claude".into()),
                },
                RemoteAgentPane {
                    terminal_id: "term_656e5c429826d3".into(),
                    workspace_id: "w3".into(),
                    workspace_label: "emf".into(),
                    agent: Some("claude".into()),
                },
            ]
        );
        // Duplicate remote workspace labels must still yield distinct mirrors.
        assert_ne!(
            snapshot.panes[0].mirror_key("sera"),
            snapshot.panes[1].mirror_key("sera")
        );
    }

    #[test]
    fn discovery_falls_back_to_workspace_id_when_the_label_is_missing() {
        let workspaces = serde_json::json!({
            "id": "cli:workspace:list",
            "result": {"type": "workspace_list", "workspaces": []}
        })
        .to_string();
        let stdout = format!("herdr\n{}\n{}\n", workspaces, pane_list_json());

        let snapshot = parse_discovery_output(&stdout).expect("parses");

        assert_eq!(snapshot.panes[0].workspace_label, "w1");
    }

    #[test]
    fn discovery_reports_a_remote_api_error_instead_of_an_empty_mirror() {
        let error = serde_json::json!({
            "id": "cli:pane:list",
            "error": {"code": "server_unavailable", "message": "no server"}
        })
        .to_string();
        let stdout = format!("herdr\n{}\n{}\n", workspace_list_json(), error);

        let err = parse_discovery_output(&stdout).expect_err("propagates remote error");

        assert!(err.to_string().contains("server_unavailable"), "{err}");
    }

    #[test]
    fn mirror_identity_is_stable_per_host_workspace_and_terminal() {
        let pane = RemoteAgentPane {
            terminal_id: "term-1".into(),
            workspace_id: "w1".into(),
            workspace_label: "api-server".into(),
            agent: Some("claude".into()),
        };

        assert_eq!(pane.mirror_key("workbox"), pane.mirror_key("workbox"));
        assert_ne!(pane.mirror_key("workbox"), pane.mirror_key("other"));
        assert_eq!(pane.mirror_label("box"), "box/api-server");
    }

    #[test]
    fn attach_argv_targets_the_remote_terminal_through_a_pty() {
        let pane = RemoteAgentPane {
            terminal_id: "term-1".into(),
            workspace_id: "w1".into(),
            workspace_label: "api-server".into(),
            agent: None,
        };

        let argv = attach_argv(&space("workbox"), &pane, "/home/you/.local/bin/herdr");

        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-t");
        assert_eq!(argv[2], "workbox");
        assert_eq!(
            argv[3],
            "'/home/you/.local/bin/herdr' terminal attach 'term-1'"
        );
    }

    #[test]
    fn attach_argv_carries_an_explicit_remote_session() {
        let mut space = space("workbox");
        space.session = Some("agents".into());
        let pane = RemoteAgentPane {
            terminal_id: "term-1".into(),
            workspace_id: "w1".into(),
            workspace_label: "api-server".into(),
            agent: None,
        };

        let argv = attach_argv(&space, &pane, "herdr");

        assert!(
            argv[3].starts_with("HERDR_SESSION='agents' "),
            "{}",
            argv[3]
        );
    }

    #[test]
    fn discovery_script_exports_only_an_explicit_session() {
        assert!(!discovery_script(None).contains("HERDR_SESSION"));
        assert!(discovery_script(Some("agents")).contains("HERDR_SESSION='agents'"));
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
