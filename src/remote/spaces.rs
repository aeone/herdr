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
    /// Status the remote reports. The remote has hook-level authority over its
    /// own agents, so this is more accurate than screen-detecting the attached
    /// copy, which never sees anything but idle.
    pub(crate) status: crate::api::schema::AgentStatus,
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

    /// Name for the mirrored local space. The host is not baked in: it is a
    /// separate `remote_host` sidebar token so it can be styled on its own.
    pub(crate) fn mirror_label(&self) -> String {
        self.workspace_label.clone()
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
    if space.is_local() {
        // No ssh hop, so the attach runs the local binary directly. The pane
        // would otherwise inherit this server's own session and socket
        // overrides and attach to itself, so they are cleared explicitly.
        let mut argv = vec![
            "env".to_string(),
            "-u".to_string(),
            "HERDR_SOCKET_PATH".to_string(),
            "-u".to_string(),
            "HERDR_CLIENT_SOCKET_PATH".to_string(),
        ];
        match space.session.as_deref() {
            Some(session) => argv.push(format!("{}={session}", crate::session::SESSION_ENV_VAR)),
            None => {
                argv.push("-u".to_string());
                argv.push(crate::session::SESSION_ENV_VAR.to_string());
            }
        }
        argv.push(remote_herdr.to_string());
        argv.push("terminal".to_string());
        argv.push("attach".to_string());
        argv.push(pane.terminal_id.clone());
        return argv;
    }
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

/// Names for every mirrored pane in a snapshot, aligned with `panes`.
///
/// Remote workspace labels are not unique — a host can easily have several
/// spaces called `lifestream`, and one space can host several agents. Names
/// that collide get a numeric suffix in snapshot order so each mirror stays
/// distinguishable in the sidebar.
pub(crate) fn mirror_labels(panes: &[RemoteAgentPane]) -> Vec<String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for pane in panes {
        *counts.entry(pane.workspace_label.as_str()).or_default() += 1;
    }

    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    panes
        .iter()
        .map(|pane| {
            let base = pane.mirror_label();
            if counts
                .get(pane.workspace_label.as_str())
                .is_none_or(|total| *total <= 1)
            {
                return base;
            }
            let ordinal = seen
                .entry(pane.workspace_label.as_str())
                .and_modify(|seen| *seen += 1)
                .or_insert(1);
            format!("{base} {ordinal}")
        })
        .collect()
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
    let script = discovery_script(space.session.as_deref());
    let output = if space.is_local() {
        local_sh_output(&script)?
    } else {
        RemoteSsh::new(space.target.clone(), manage_ssh_config).sh_output(&script)?
    };
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "remote space discovery failed on {}: {}",
            space.target,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_discovery_output(&String::from_utf8_lossy(&output.stdout), space.mirror_all)
}

/// Runs the same discovery script through a local shell, for `target = "local"`.
fn local_sh_output(script: &str) -> io::Result<std::process::Output> {
    use std::io::Write;

    let mut child = std::process::Command::new("/bin/sh")
        .arg("-s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A local mirror must not inherit this server's own session or socket
        // overrides, or it would discover itself. The script re-exports the
        // configured session when one is set.
        .env_remove(crate::session::SESSION_ENV_VAR)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(script.as_bytes())?;
    }
    drop(child.stdin.take());
    child.wait_with_output()
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
fn parse_discovery_output(stdout: &str, mirror_all: bool) -> io::Result<RemoteSpaceSnapshot> {
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
    let panes = parse_mirror_panes(panes_line, &workspace_labels, mirror_all)?;
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

/// Panes to mirror from a pane list.
///
/// Agent panes are always mirrored. With `mirror_all`, every other space is
/// mirrored too by its first pane, so agent-less shells show up as well; the
/// default keeps the sidebar to just the agents.
fn parse_mirror_panes(
    line: &str,
    workspace_labels: &std::collections::HashMap<String, String>,
    mirror_all: bool,
) -> io::Result<Vec<RemoteAgentPane>> {
    let ResponseResult::PaneList { panes } = parse_result(line, "pane list")? else {
        return Err(io::Error::other(
            "remote pane list had an unexpected result type",
        ));
    };
    let is_agent =
        |pane: &crate::api::schema::PaneInfo| pane.agent.is_some() || pane.display_agent.is_some();
    let agent_workspaces: std::collections::HashSet<String> = panes
        .iter()
        .filter(|pane| is_agent(pane))
        .map(|pane| pane.workspace_id.clone())
        .collect();
    // One shell mirror per agent-less workspace, from its first listed pane.
    let mut shell_workspaces = std::collections::HashSet::new();
    Ok(panes
        .into_iter()
        .filter(|pane| {
            if is_agent(pane) {
                return true;
            }
            if !mirror_all || agent_workspaces.contains(&pane.workspace_id) {
                return false;
            }
            shell_workspaces.insert(pane.workspace_id.clone())
        })
        .map(|pane| {
            let workspace_label = workspace_labels
                .get(&pane.workspace_id)
                .cloned()
                .unwrap_or_else(|| pane.workspace_id.clone());
            RemoteAgentPane {
                status: pane.agent_status,
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
            mirror_all: false,
            color: None,
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

        let snapshot = parse_discovery_output(&stdout, false).expect("parses live macOS output");

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

        let snapshot = parse_discovery_output(&stdout, false).expect("parses");

        assert_eq!(snapshot.remote_herdr, "/home/you/.local/bin/herdr");
        assert_eq!(
            snapshot.panes,
            vec![RemoteAgentPane {
                terminal_id: "term-1".into(),
                workspace_id: "w1".into(),
                workspace_label: "api-server".into(),
                agent: Some("claude".into()),
                status: crate::api::schema::AgentStatus::Working,
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

        let snapshot = parse_discovery_output(stdout, false).expect("parses live output");

        // The shell pane in w2 carries no agent and must not be mirrored.
        assert_eq!(
            snapshot.panes,
            vec![
                RemoteAgentPane {
                    terminal_id: "term_656ccaeee911e1".into(),
                    workspace_id: "w1".into(),
                    workspace_label: "lifestream".into(),
                    agent: Some("claude".into()),
                    status: crate::api::schema::AgentStatus::Idle,
                },
                RemoteAgentPane {
                    terminal_id: "term_656e5c429826d3".into(),
                    workspace_id: "w3".into(),
                    workspace_label: "emf".into(),
                    agent: Some("claude".into()),
                    status: crate::api::schema::AgentStatus::Done,
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
    fn mirror_all_includes_agent_less_spaces_by_their_first_pane() {
        let workspaces = r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"active_tab_id":"w1:t1","agent_status":"idle","focused":false,"label":"api","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"},{"active_tab_id":"w2:t1","agent_status":"unknown","focused":false,"label":"scratch","number":2,"pane_count":2,"tab_count":1,"workspace_id":"w2"}]}}"#;
        // w1 has an agent; w2 is a two-pane shell workspace with none.
        let panes = r#"{"id":"cli:pane:list","result":{"panes":[{"agent":"claude","agent_status":"idle","focused":false,"pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","terminal_id":"term-a","workspace_id":"w1"},{"agent_status":"unknown","focused":false,"pane_id":"w2:p1","revision":0,"tab_id":"w2:t1","terminal_id":"term-b","workspace_id":"w2"},{"agent_status":"unknown","focused":false,"pane_id":"w2:p2","revision":0,"tab_id":"w2:t1","terminal_id":"term-c","workspace_id":"w2"}],"type":"pane_list"}}"#;
        let stdout = format!(
            "herdr
{workspaces}
{panes}
"
        );

        let agents_only = parse_discovery_output(&stdout, false).expect("parses");
        assert_eq!(
            agents_only
                .panes
                .iter()
                .map(|p| p.workspace_label.as_str())
                .collect::<Vec<_>>(),
            ["api"]
        );

        let all = parse_discovery_output(&stdout, true).expect("parses");
        // The shell workspace appears once, via its first pane, with no agent.
        assert_eq!(
            all.panes
                .iter()
                .map(|p| (
                    p.workspace_label.as_str(),
                    p.terminal_id.as_str(),
                    p.agent.is_some()
                ))
                .collect::<Vec<_>>(),
            [("api", "term-a", true), ("scratch", "term-b", false)]
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

        let snapshot = parse_discovery_output(&stdout, false).expect("parses");

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

        let err = parse_discovery_output(&stdout, false).expect_err("propagates remote error");

        assert!(err.to_string().contains("server_unavailable"), "{err}");
    }

    fn pane(workspace_id: &str, workspace_label: &str, terminal_id: &str) -> RemoteAgentPane {
        RemoteAgentPane {
            terminal_id: terminal_id.into(),
            workspace_id: workspace_id.into(),
            workspace_label: workspace_label.into(),
            agent: Some("claude".into()),
            status: crate::api::schema::AgentStatus::Idle,
        }
    }

    #[test]
    fn mirror_labels_disambiguate_repeated_remote_workspace_names() {
        // The real macOS host had five spaces named "lifestream".
        let panes = [
            pane("w1", "lifestream", "term-1"),
            pane("wK", "baby-names", "term-2"),
            pane("wJ", "lifestream", "term-3"),
        ];

        assert_eq!(
            mirror_labels(&panes),
            ["lifestream 1", "baby-names", "lifestream 2"]
        );
    }

    #[test]
    fn mirror_labels_disambiguate_two_agents_in_one_remote_workspace() {
        let panes = [
            pane("w6", "rycelia", "term-1"),
            pane("w6", "rycelia", "term-2"),
        ];

        let labels = mirror_labels(&panes);

        assert_eq!(labels, ["rycelia 1", "rycelia 2"]);
    }

    #[test]
    fn mirror_labels_leave_unique_names_unsuffixed() {
        let panes = [pane("wK", "baby-names", "term-1")];

        assert_eq!(mirror_labels(&panes), ["baby-names"]);
    }

    #[test]
    fn mirror_identity_is_stable_per_host_workspace_and_terminal() {
        let pane = RemoteAgentPane {
            terminal_id: "term-1".into(),
            workspace_id: "w1".into(),
            workspace_label: "api-server".into(),
            agent: Some("claude".into()),
            status: crate::api::schema::AgentStatus::Idle,
        };

        assert_eq!(pane.mirror_key("workbox"), pane.mirror_key("workbox"));
        assert_ne!(pane.mirror_key("workbox"), pane.mirror_key("other"));
        assert_eq!(pane.mirror_label(), "api-server");
    }

    #[test]
    fn attach_argv_targets_the_remote_terminal_through_a_pty() {
        let pane = RemoteAgentPane {
            terminal_id: "term-1".into(),
            workspace_id: "w1".into(),
            workspace_label: "api-server".into(),
            agent: None,
            status: crate::api::schema::AgentStatus::Idle,
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
    fn local_attach_argv_skips_ssh_and_clears_inherited_session() {
        let mut space = space("local");
        space.session = None;
        let pane = pane("w1", "api", "term-1");

        let argv = attach_argv(&space, &pane, "/usr/bin/herdr");

        assert_eq!(argv[0], "env");
        // Without these the pane inherits the running server's session and
        // sockets, and attaches to itself instead of the other session.
        assert!(argv.contains(&"-u".to_string()));
        assert!(argv.contains(&"HERDR_SESSION".to_string()));
        assert!(argv.contains(&"HERDR_SOCKET_PATH".to_string()));
        assert!(!argv.contains(&"ssh".to_string()));
        assert_eq!(argv[argv.len() - 3..], ["terminal", "attach", "term-1"]);
    }

    #[test]
    fn local_attach_argv_targets_a_named_local_session() {
        let mut space = space("local");
        space.session = Some("work".into());

        let argv = attach_argv(&space, &pane("w1", "api", "term-1"), "herdr");

        assert!(argv.contains(&"HERDR_SESSION=work".to_string()));
        assert!(!argv.contains(&"ssh".to_string()));
    }

    #[test]
    fn only_the_local_target_skips_ssh() {
        assert!(space("local").is_local());
        assert!(!space("workbox").is_local());
        assert_eq!(
            attach_argv(&space("workbox"), &pane("w1", "a", "t"), "herdr")[0],
            "ssh"
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
            status: crate::api::schema::AgentStatus::Idle,
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
