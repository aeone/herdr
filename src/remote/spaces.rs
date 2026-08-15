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
use std::io::BufRead;

use crate::api::schema::response::ResponseResult;
use crate::config::RemoteSpaceConfig;

use super::unix::RemoteSsh;

/// The pane a remote mirror stands for, on the host that really runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MirrorOrigin {
    /// The ssh target as the *reporting* host spells it, which need not match
    /// how we spell the same machine.
    pub(crate) target: String,
    /// The terminal id on the origin host: the identity that survives a hop.
    pub(crate) terminal_id: String,
    /// How the reporting host displays that origin, when it knows.
    pub(crate) label: Option<String>,
    pub(crate) color: Option<String>,
}

impl MirrorOrigin {
    /// A host name two configurations can be compared on.
    ///
    /// One host may be reached as `alleria` and another may write
    /// `ryielle@alleria` for the same machine, and neither spelling is more
    /// correct. The login name is not part of which machine it is.
    pub(crate) fn host_key(target: &str) -> String {
        target
            .rsplit('@')
            .next()
            .unwrap_or(target)
            .to_ascii_lowercase()
    }
}

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
    /// Set when this remote pane is itself a mirror of a pane somewhere else.
    /// Kept so a chain of hosts mirroring each other does not show the same
    /// agent once per hop.
    pub(crate) origin: Option<MirrorOrigin>,
    /// When the remote says this agent last changed state, in unix ms.
    ///
    /// Carried across so the sidebar ages a mirrored agent by what happened on
    /// its own host. Applying the state locally would restamp it as changing
    /// now, and since mirrors are rebuilt on every reconnect and handoff, every
    /// mirrored agent would read as having just gone idle.
    pub(crate) state_changed_at_ms: Option<u64>,
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

    /// Splits a mirror key back into the origin's workspace and terminal ids.
    ///
    /// The key is built by [`RemoteAgentPane::mirror_key`], and the terminal id
    /// in it is the one the origin host knows the pane by. That is the identity
    /// that survives being mirrored, so it is what tells a pane from a
    /// reflection of it.
    pub(crate) fn split_key(key: &str) -> Option<(&str, &str)> {
        let mut parts = key.split('\u{1f}');
        let _target = parts.next()?;
        Some((parts.next()?, parts.next()?))
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
    // Same liveness options the discovery ssh uses. Without them a mirror whose
    // host vanishes holds a dead connection open and shows stale output forever;
    // with them the attach fails within about a minute and can be rebuilt.
    let mut argv = vec![
        "ssh".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=4".to_string(),
        "-o".to_string(),
        "ConnectTimeout=15".to_string(),
        "-t".to_string(),
        space.target.clone(),
    ];
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
    /// Agent-less remote spaces, one candidate pane each, that this host is not
    /// mirroring wholesale. A space the user explicitly asked for is mirrored
    /// from here even though nothing is running an agent in it yet.
    pub(crate) shell_panes: Vec<RemoteAgentPane>,
    /// Every other agent-less pane: the second and later panes of a space, and
    /// the plain shells sharing a space with an agent. Nothing mirrors these by
    /// default — one candidate is enough to stand for a space in the sidebar —
    /// but a tab the user asked for on this host is one of them, and is
    /// mirrored by pane rather than by space.
    pub(crate) extra_panes: Vec<RemoteAgentPane>,
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
    let script = discovery_script(space.session.as_deref(), space.is_local());
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

/// What a feed attempt ended up doing, so the caller knows whether the remote
/// supports the push feed or should fall back to polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedOutcome {
    /// The feed streamed at least one block before ending — the remote has it.
    Streamed,
    /// The feed produced no blocks (old binary, missing command, or an error).
    Unavailable,
}

/// Runs `herdr agent feed` on the host and calls `on_snapshot` for each pushed
/// block, parsing it exactly as a poll response.
///
/// Returns when the feed process ends or `should_continue` goes false. The feed
/// pushes a block on startup and on every relevant change, so this is the live
/// alternative to [`discover`].
pub(crate) fn run_feed(
    space: &RemoteSpaceConfig,
    manage_ssh_config: bool,
    mut on_snapshot: impl FnMut(io::Result<RemoteSpaceSnapshot>),
    should_continue: &dyn Fn() -> bool,
) -> io::Result<FeedOutcome> {
    let script = feed_script(space.session.as_deref(), space.is_local());
    let mut child = if space.is_local() {
        spawn_local_streaming_script(&script)?
    } else {
        RemoteSsh::new(space.target.clone(), manage_ssh_config).spawn_streaming_script(&script)?
    };

    let mut streamed = false;
    if let Some(stdout) = child.stdout.take() {
        let reader = std::io::BufReader::new(stdout);
        let mut block = Vec::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() {
                block.push(line);
                // A block is the same three lines a poll returns: binary path,
                // workspace list, pane list.
                if block.len() == 3 {
                    streamed = true;
                    on_snapshot(parse_discovery_output(&block.join("\n"), space.mirror_all));
                    block.clear();
                }
            }
            // Checked after the line is consumed, not before: the read has
            // already blocked, so discarding what it returned only loses a
            // snapshot without stopping any sooner.
            if !should_continue() {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(if streamed {
        FeedOutcome::Streamed
    } else {
        FeedOutcome::Unavailable
    })
}

fn spawn_local_streaming_script(script: &str) -> io::Result<std::process::Child> {
    use std::io::Write;
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .env_remove(crate::session::SESSION_ENV_VAR)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
        stdin.flush()?;
    }
    Ok(child)
}

/// Like [`discovery_script`] but execs the streaming `agent feed` instead of
/// the one-shot list commands.
fn feed_script(session: Option<&str>, local: bool) -> String {
    let mut script = script_prologue(session, local);
    script.push_str("exec \"$herdr_bin\" agent feed\n");
    script
}

/// `set -e`, then the target session, then a herdr binary in `$herdr_bin`.
///
/// The session export has to come before the binary search: locally that search
/// probes the session being mirrored, and without the export it would probe
/// this machine's default session instead.
fn script_prologue(session: Option<&str>, local: bool) -> String {
    let mut script = String::from("set -e\n");
    if let Some(session) = session {
        script.push_str(&format!(
            "{}={}\nexport {}\n",
            crate::session::SESSION_ENV_VAR,
            shell_quote(session),
            crate::session::SESSION_ENV_VAR
        ));
    }
    script.push_str(&herdr_bin_script(local));
    script
}

/// Shell fragment that puts a herdr binary in `$herdr_bin`.
///
/// A local mirror talks to another session's server on this same machine, and
/// that server can be a different herdr build than this one. The CLI refuses to
/// talk across a protocol version gap, so try each candidate against the target
/// session and keep the first that answers: the running binary when mirroring a
/// session owned by a build like this one, an installed binary when mirroring a
/// session owned by that install. Over ssh the remote resolves its own from
/// PATH and common install paths, where binary and server always agree.
fn herdr_bin_script(local: bool) -> String {
    if local {
        let current = std::env::current_exe()
            .ok()
            .and_then(|path| path.to_str().map(str::to_string))
            .unwrap_or_else(|| "herdr".to_string());
        return format!(
            r#"herdr_bin=
for candidate in {} $(command -v herdr 2>/dev/null || true) "${{HOME:-}}/.local/bin/herdr" "${{HOME:-}}/.cargo/bin/herdr" /usr/local/bin/herdr /opt/homebrew/bin/herdr; do
  [ -x "$candidate" ] || continue
  if "$candidate" workspace list >/dev/null 2>&1; then
    herdr_bin=$candidate
    break
  fi
done
if [ -z "$herdr_bin" ]; then
  echo "no herdr binary on this machine speaks the mirrored session's protocol" >&2
  exit 1
fi
"#,
            shell_quote(&current)
        );
    }
    String::from(
        r#"herdr_bin=$(command -v herdr 2>/dev/null || true)
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
    )
}

/// Shell run on the remote host. It resolves a herdr binary the same way a
/// non-login `ssh host herdr ...` would fail to, then prints the binary path
/// followed by the two JSON list responses, one per line.
fn discovery_script(session: Option<&str>, local: bool) -> String {
    let mut script = script_prologue(session, local);
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
    let (panes, shell_panes, extra_panes) =
        parse_mirror_panes(panes_line, &workspace_labels, mirror_all)?;
    Ok(RemoteSpaceSnapshot {
        remote_herdr,
        panes,
        shell_panes,
        extra_panes,
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

/// Splits a pane list into the panes to mirror, the agent-less spaces that could
/// be mirrored on request, and the agent-less panes behind them.
///
/// Agent panes are always mirrored. Every agent-less space is also reduced to a
/// single candidate, its first listed pane. With `mirror_all` those candidates
/// are mirrored outright; otherwise they are returned separately, so a caller
/// holding a space the user asked for by name can mirror just that one while the
/// sidebar stays limited to agents.
///
/// The panes neither rule picks are returned too rather than dropped. They are
/// not sidebar material on their own, but a tab the user asked for on this host
/// lands among them, and the caller mirrors that one by name.
fn parse_mirror_panes(
    line: &str,
    workspace_labels: &std::collections::HashMap<String, String>,
    mirror_all: bool,
) -> io::Result<(
    Vec<RemoteAgentPane>,
    Vec<RemoteAgentPane>,
    Vec<RemoteAgentPane>,
)> {
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

    let to_mirror = |pane: crate::api::schema::PaneInfo| {
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
            origin: pane.mirror_origin.map(|origin| MirrorOrigin {
                target: origin.target,
                terminal_id: origin.terminal_id,
                label: origin.label,
                color: origin.color,
            }),
            state_changed_at_ms: pane.agent_state_changed_at_ms,
        }
    };

    // A host mirroring its own session reports the reflection and the real pane
    // side by side. The reflection is dropped here rather than downstream,
    // because both are on this one host and nothing further along can tell them
    // apart -- the identity that gives it away is the origin terminal id.
    let direct_terminals: std::collections::HashSet<&str> = panes
        .iter()
        .filter(|pane| pane.mirror_origin.is_none())
        .map(|pane| pane.terminal_id.as_str())
        .collect();
    let panes: Vec<_> = panes
        .iter()
        .filter(|pane| {
            pane.mirror_origin
                .as_ref()
                .is_none_or(|origin| !direct_terminals.contains(origin.terminal_id.as_str()))
        })
        .cloned()
        .collect();

    let mut mirrored = Vec::new();
    let mut shells = Vec::new();
    let mut extra = Vec::new();
    // One candidate per agent-less workspace, from its first listed pane.
    let mut seen_shell_workspaces = std::collections::HashSet::new();
    for pane in panes {
        if is_agent(&pane) {
            mirrored.push(to_mirror(pane));
            continue;
        }
        let is_space_candidate = !agent_workspaces.contains(&pane.workspace_id)
            && seen_shell_workspaces.insert(pane.workspace_id.clone());
        if !is_space_candidate {
            extra.push(to_mirror(pane));
        } else if mirror_all {
            mirrored.push(to_mirror(pane));
        } else {
            shells.push(to_mirror(pane));
        }
    }
    Ok((mirrored, shells, extra))
}

/// A space just opened on a mirrored host, with everything needed to mirror it
/// without waiting for the next snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreatedRemoteSpace {
    pub(crate) remote_herdr: String,
    pub(crate) pane: RemoteAgentPane,
}

/// Creates a space on a mirrored host and returns the pane to mirror.
///
/// `workspace create` has been in the CLI since well before the mirroring
/// feature, so this works against a stock remote — no fork needed on the host.
/// The new space is left unfocused: the user is driving a local sidebar, and
/// stealing focus on the remote would move someone else's cursor.
pub(crate) fn create_remote_workspace(
    space: &RemoteSpaceConfig,
    manage_ssh_config: bool,
    label: Option<&str>,
) -> io::Result<CreatedRemoteSpace> {
    let script = create_workspace_script(space.session.as_deref(), space.is_local(), label);
    let output = if space.is_local() {
        local_sh_output(&script)?
    } else {
        RemoteSsh::new(space.target.clone(), manage_ssh_config).sh_output(&script)?
    };
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "creating a space on {} failed: {}",
            space.target,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_created_workspace(&String::from_utf8_lossy(&output.stdout))
}

/// Creates a tab in a space on a mirrored host and returns the pane to mirror.
///
/// A mirror stands for one remote pane, so a tab asked for on a mirror is not a
/// local tab: it is another pane on the host, mirrored beside the one it was
/// asked from. Creating it locally would put the shell on this machine instead,
/// and reconcile would take it down with the mirror the next time the remote
/// pane went away.
pub(crate) fn create_remote_tab(
    space: &RemoteSpaceConfig,
    manage_ssh_config: bool,
    workspace_id: &str,
    label: Option<&str>,
) -> io::Result<CreatedRemoteSpace> {
    let script = create_tab_script(
        space.session.as_deref(),
        space.is_local(),
        workspace_id,
        label,
    );
    let output = if space.is_local() {
        local_sh_output(&script)?
    } else {
        RemoteSsh::new(space.target.clone(), manage_ssh_config).sh_output(&script)?
    };
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "creating a tab on {} failed: {}",
            space.target,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_created_tab(&String::from_utf8_lossy(&output.stdout))
}

/// Renames a workspace on the mirrored host.
///
/// A mirror shows the remote's own label, so the only way for a new name to
/// survive reconcile is for the remote to start reporting it. Renaming there
/// also means every other host mirroring the same space sees the change.
pub(crate) fn rename_remote_workspace(
    space: &RemoteSpaceConfig,
    manage_ssh_config: bool,
    workspace_id: &str,
    label: &str,
) -> io::Result<()> {
    let script = rename_workspace_script(
        space.session.as_deref(),
        space.is_local(),
        workspace_id,
        label,
    );
    let output = if space.is_local() {
        local_sh_output(&script)?
    } else {
        RemoteSsh::new(space.target.clone(), manage_ssh_config).sh_output(&script)?
    };
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "renaming a space on {} failed: {}",
            space.target,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn rename_workspace_script(
    session: Option<&str>,
    local: bool,
    workspace_id: &str,
    label: &str,
) -> String {
    let mut script = script_prologue(session, local);
    // No `--` separator: `workspace rename` takes its label as a variadic
    // positional, which keeps the separator as a value rather than consuming
    // it, so it would end up at the front of the name.
    script.push_str(&format!(
        "\"$herdr_bin\" workspace rename {} {}\n",
        shell_quote(workspace_id),
        shell_quote(label)
    ));
    script
}

fn create_workspace_script(session: Option<&str>, local: bool, label: Option<&str>) -> String {
    let mut script = script_prologue(session, local);
    // Same first line as discovery: the mirror's attach command needs the remote
    // binary path, and the create response does not carry it.
    script.push_str("printf '%s\\n' \"$herdr_bin\"\n");
    script.push_str("\"$herdr_bin\" workspace create --no-focus");
    if let Some(label) = label {
        script.push_str(&format!(" --label {}", shell_quote(label)));
    }
    script.push('\n');
    script
}

/// Turns a `workspace create` response into the pane to mirror.
///
/// The response carries the new space's root pane, so the mirror can be built
/// straight away instead of waiting for the next snapshot to notice it.
fn parse_created_workspace(stdout: &str) -> io::Result<CreatedRemoteSpace> {
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let remote_herdr = lines
        .next()
        .ok_or_else(|| io::Error::other("remote workspace create returned no output"))?
        .trim()
        .to_string();
    let created = lines
        .next()
        .ok_or_else(|| io::Error::other("remote workspace create returned no response"))?;
    let ResponseResult::WorkspaceCreated {
        workspace,
        root_pane,
        ..
    } = parse_result(created, "workspace create")?
    else {
        return Err(io::Error::other(
            "remote workspace create had an unexpected result type",
        ));
    };
    Ok(CreatedRemoteSpace {
        remote_herdr,
        pane: RemoteAgentPane {
            status: root_pane.agent_status,
            terminal_id: root_pane.terminal_id,
            workspace_id: root_pane.workspace_id,
            workspace_label: workspace.label,
            agent: None,
            origin: None,
            state_changed_at_ms: root_pane.agent_state_changed_at_ms,
        },
    })
}

fn create_tab_script(
    session: Option<&str>,
    local: bool,
    workspace_id: &str,
    label: Option<&str>,
) -> String {
    let mut script = script_prologue(session, local);
    // Same first line as discovery: the mirror's attach command needs the remote
    // binary path, and the create response does not carry it.
    script.push_str("printf '%s\\n' \"$herdr_bin\"\n");
    script.push_str(&format!(
        "\"$herdr_bin\" tab create --workspace {} --no-focus",
        shell_quote(workspace_id)
    ));
    if let Some(label) = label {
        script.push_str(&format!(" --label {}", shell_quote(label)));
    }
    script.push('\n');
    // `tab create` names a tab, and a mirror is named after its space. The label
    // has to come from this same call: waiting for the next poll would leave the
    // new mirror showing a raw workspace id until it arrived.
    script.push_str("\"$herdr_bin\" workspace list\n");
    script
}

/// Turns a `tab create` response into the pane to mirror.
///
/// The response carries the new tab's root pane but not its space's name, so
/// the trailing `workspace list` supplies the label.
fn parse_created_tab(stdout: &str) -> io::Result<CreatedRemoteSpace> {
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let remote_herdr = lines
        .next()
        .ok_or_else(|| io::Error::other("remote tab create returned no output"))?
        .trim()
        .to_string();
    let created = lines
        .next()
        .ok_or_else(|| io::Error::other("remote tab create returned no response"))?;
    let workspaces = lines
        .next()
        .ok_or_else(|| io::Error::other("remote tab create returned no workspace list"))?;
    let ResponseResult::TabCreated { root_pane, .. } = parse_result(created, "tab create")? else {
        return Err(io::Error::other(
            "remote tab create had an unexpected result type",
        ));
    };
    let workspace_label = parse_workspace_labels(workspaces)?
        .get(&root_pane.workspace_id)
        .cloned()
        .unwrap_or_else(|| root_pane.workspace_id.clone());
    Ok(CreatedRemoteSpace {
        remote_herdr,
        pane: RemoteAgentPane {
            status: root_pane.agent_status,
            terminal_id: root_pane.terminal_id,
            workspace_id: root_pane.workspace_id,
            workspace_label,
            agent: None,
            origin: None,
            state_changed_at_ms: root_pane.agent_state_changed_at_ms,
        },
    })
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
                origin: None,
                state_changed_at_ms: None,
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
                    origin: None,
                    state_changed_at_ms: None,
                },
                RemoteAgentPane {
                    terminal_id: "term_656e5c429826d3".into(),
                    workspace_id: "w3".into(),
                    workspace_label: "emf".into(),
                    agent: Some("claude".into()),
                    status: crate::api::schema::AgentStatus::Done,
                    origin: None,
                    state_changed_at_ms: None,
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
            origin: None,
            state_changed_at_ms: None,
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
            origin: None,
            state_changed_at_ms: None,
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
            origin: None,
            state_changed_at_ms: None,
        };

        let argv = attach_argv(&space("workbox"), &pane, "/home/you/.local/bin/herdr");

        assert_eq!(argv[0], "ssh");
        // -t forces a pty; the keepalives stop a vanished host freezing the
        // mirror on stale output. Asserted by presence, not position, so adding
        // an option does not break the test.
        assert!(argv.contains(&"-t".to_string()), "{argv:?}");
        assert!(
            argv.contains(&"ServerAliveInterval=15".to_string()),
            "{argv:?}"
        );
        assert!(argv.contains(&"ConnectTimeout=15".to_string()), "{argv:?}");
        assert_eq!(argv[argv.len() - 2], "workbox");
        assert_eq!(
            argv[argv.len() - 1],
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
            origin: None,
            state_changed_at_ms: None,
        };

        let argv = attach_argv(&space, &pane, "herdr");

        assert!(
            argv[argv.len() - 1].starts_with("HERDR_SESSION='agents' "),
            "{argv:?}"
        );
    }

    #[test]
    fn discovery_script_exports_only_an_explicit_session() {
        assert!(!discovery_script(None, false).contains("HERDR_SESSION"));
        assert!(discovery_script(Some("agents"), false).contains("HERDR_SESSION='agents'"));
    }

    /// A local mirror can point at a session owned by a different herdr build,
    /// which the CLI refuses to talk to across a protocol version gap. Pinning
    /// one binary makes every update of such a session fail, so the script has
    /// to try candidates and keep one that answers.
    #[test]
    fn local_scripts_probe_candidate_binaries_instead_of_pinning_one() {
        let script = discovery_script(Some("default"), true);
        assert!(
            script.contains("workspace list >/dev/null 2>&1"),
            "{script}"
        );
        assert!(script.contains("herdr_bin=$candidate"), "{script}");
        // The installed locations matter: mirroring the release-owned session
        // only works through the release binary.
        assert!(script.contains("/.local/bin/herdr"), "{script}");
    }

    /// The local probe runs `workspace list` against the mirrored session, so an
    /// export placed after it would silently probe the default session instead.
    #[test]
    fn local_scripts_export_the_session_before_probing() {
        for script in [
            discovery_script(Some("agents"), true),
            feed_script(Some("agents"), true),
        ] {
            let export = script
                .find("HERDR_SESSION='agents'")
                .expect("session export");
            let probe = script.find("workspace list").expect("probe");
            assert!(export < probe, "{script}");
        }
    }

    /// Creating a space must not steal the remote's focus, and has to report the
    /// binary path so the mirror's attach command can be built from one round
    /// trip instead of two.
    #[test]
    fn create_workspace_script_leaves_the_remote_unfocused_and_reports_its_binary() {
        let script = create_workspace_script(Some("agents"), false, Some("notes"));

        assert!(script.contains("workspace create --no-focus"), "{script}");
        assert!(script.contains("--label 'notes'"), "{script}");
        assert!(script.contains("printf '%s\\n' \"$herdr_bin\""), "{script}");
    }

    #[test]
    fn created_workspace_is_parsed_into_the_pane_to_mirror() {
        let stdout = concat!(
            "/opt/homebrew/bin/herdr\n",
            r#"{"id":"cli:workspace:create","result":{"type":"workspace_created","#,
            r#""workspace":{"active_tab_id":"wX:t1","agent_status":"unknown","focused":false,"#,
            r#""label":"notes","number":3,"pane_count":1,"tab_count":1,"workspace_id":"wX"},"#,
            r#""tab":{"agent_status":"unknown","focused":false,"label":"1","number":1,"#,
            r#""pane_count":1,"tab_id":"wX:t1","workspace_id":"wX"},"#,
            r#""root_pane":{"agent_status":"unknown","cwd":"/home/ryi","focused":false,"#,
            r#""pane_id":"wX:p1","revision":0,"tab_id":"wX:t1","#,
            r#""terminal_id":"term_abc","workspace_id":"wX"}}}"#,
        );

        let created = parse_created_workspace(stdout).expect("parse");

        assert_eq!(created.remote_herdr, "/opt/homebrew/bin/herdr");
        assert_eq!(created.pane.workspace_id, "wX");
        assert_eq!(created.pane.terminal_id, "term_abc");
        assert_eq!(created.pane.workspace_label, "notes");
        // Nothing is running in it yet, so it must not claim an agent.
        assert_eq!(created.pane.agent, None);
    }

    /// A tab is created in a named space and, like a space, without stealing the
    /// remote's focus. The trailing `workspace list` is what names the mirror.
    #[test]
    fn create_tab_script_targets_the_space_and_asks_for_its_label() {
        let script = create_tab_script(Some("agents"), false, "wX", Some("notes"));

        assert!(
            script.contains("tab create --workspace 'wX' --no-focus"),
            "{script}"
        );
        assert!(script.contains("--label 'notes'"), "{script}");
        assert!(script.contains("printf '%s\\n' \"$herdr_bin\""), "{script}");
        assert!(script.contains("workspace list"), "{script}");
    }

    #[test]
    fn created_tab_is_parsed_into_the_pane_to_mirror() {
        let stdout = concat!(
            "/opt/homebrew/bin/herdr\n",
            r#"{"id":"cli:tab:create","result":{"type":"tab_created","#,
            r#""tab":{"agent_status":"unknown","focused":false,"label":"2","number":2,"#,
            r#""pane_count":1,"tab_id":"wX:t2","workspace_id":"wX"},"#,
            r#""root_pane":{"agent_status":"unknown","cwd":"/home/ryi","focused":false,"#,
            r#""pane_id":"wX:p2","revision":0,"tab_id":"wX:t2","#,
            r#""terminal_id":"term_new","workspace_id":"wX"}}}"#,
            "\n",
            r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"#,
            r#""active_tab_id":"wX:t1","agent_status":"unknown","focused":false,"#,
            r#""label":"notes","number":3,"pane_count":2,"tab_count":2,"workspace_id":"wX"}]}}"#,
        );

        let created = parse_created_tab(stdout).expect("parse");

        assert_eq!(created.remote_herdr, "/opt/homebrew/bin/herdr");
        assert_eq!(created.pane.workspace_id, "wX");
        assert_eq!(created.pane.terminal_id, "term_new");
        // Named after the space it joined, not after the tab: that is what the
        // sidebar shows a mirror as.
        assert_eq!(created.pane.workspace_label, "notes");
        assert_eq!(created.pane.agent, None);
    }

    /// A host that mirrors its own session reports the reflection beside the
    /// pane it reflects. Both are on that one host, so nothing downstream can
    /// tell them apart -- the reflection has to go here.
    #[test]
    fn a_hosts_reflection_of_its_own_pane_is_dropped_beside_the_real_one() {
        let labels = std::collections::HashMap::from([
            ("w1".to_string(), "api".to_string()),
            ("w9".to_string(), "api".to_string()),
        ]);
        let line = concat!(
            r#"{"id":"x","result":{"type":"pane_list","panes":[{"#,
            r#""agent":"claude","agent_status":"idle","focused":false,"#,
            r#""pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","#,
            r#""terminal_id":"term-real","workspace_id":"w1"},{"#,
            // The same pane, mirrored by this host from its own session.
            r#""agent":"claude","agent_status":"idle","focused":false,"#,
            r#""pane_id":"w9:p1","revision":0,"tab_id":"w9:t1","#,
            r#""mirror_origin":{"target":"local","workspace_id":"w1","#,
            r#""terminal_id":"term-real"},"#,
            r#""terminal_id":"term-reflection","workspace_id":"w9"}]}}"#,
        );

        let (mirrored, _, _) = parse_mirror_panes(line, &labels, false).expect("parse");

        assert_eq!(
            mirrored.iter().map(|p| &p.terminal_id).collect::<Vec<_>>(),
            ["term-real"]
        );
    }

    /// A reflection of a host we cannot see ourselves is still worth having:
    /// dropping it would lose that agent entirely rather than de-duplicate it.
    #[test]
    fn a_reflection_of_another_host_survives_discovery() {
        let labels = std::collections::HashMap::from([("w2".to_string(), "api".to_string())]);
        let line = concat!(
            r#"{"id":"x","result":{"type":"pane_list","panes":[{"#,
            r#""agent":"claude","agent_status":"idle","focused":false,"#,
            r#""pane_id":"w2:p1","revision":0,"tab_id":"w2:t1","#,
            r#""mirror_origin":{"target":"ryielle@alleria","workspace_id":"wA","#,
            r#""terminal_id":"term-far"},"#,
            r#""terminal_id":"term-hop","workspace_id":"w2"}]}}"#,
        );

        let (mirrored, _, _) = parse_mirror_panes(line, &labels, false).expect("parse");

        assert_eq!(mirrored.len(), 1);
        assert_eq!(
            mirrored[0].origin.as_ref().map(|o| o.terminal_id.as_str()),
            Some("term-far")
        );
    }

    /// One machine, two spellings. Neither is more correct, so the login name
    /// cannot be part of deciding which machine a target names.
    #[test]
    fn host_key_ignores_the_login_name_and_case() {
        assert_eq!(
            MirrorOrigin::host_key("ryielle@alleria"),
            MirrorOrigin::host_key("Alleria")
        );
        assert_ne!(
            MirrorOrigin::host_key("sera"),
            MirrorOrigin::host_key("valkyrie")
        );
    }

    /// The panes neither the agent rule nor the one-per-space rule picks are
    /// still reported, so a tab created on the host can be mirrored by name.
    #[test]
    fn panes_behind_a_space_candidate_are_reported_as_extras() {
        let labels = std::collections::HashMap::from([
            ("w1".to_string(), "api".to_string()),
            ("w2".to_string(), "notes".to_string()),
        ]);
        let line = concat!(
            r#"{"id":"x","result":{"type":"pane_list","panes":[{"#,
            r#""agent":"claude","agent_status":"idle","cwd":"/w","focused":false,"#,
            r#""pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","#,
            r#""terminal_id":"term-1","workspace_id":"w1"},{"#,
            // A plain shell sharing a space with that agent.
            r#""agent_status":"unknown","cwd":"/w","focused":false,"#,
            r#""pane_id":"w1:p2","revision":0,"tab_id":"w1:t2","#,
            r#""terminal_id":"term-2","workspace_id":"w1"},{"#,
            r#""agent_status":"unknown","cwd":"/w","focused":false,"#,
            r#""pane_id":"w2:p1","revision":0,"tab_id":"w2:t1","#,
            r#""terminal_id":"term-3","workspace_id":"w2"},{"#,
            // The second pane of an agent-less space.
            r#""agent_status":"unknown","cwd":"/w","focused":false,"#,
            r#""pane_id":"w2:p2","revision":0,"tab_id":"w2:t2","#,
            r#""terminal_id":"term-4","workspace_id":"w2"}]}}"#,
        );

        let (mirrored, shells, extra) = parse_mirror_panes(line, &labels, false).expect("parse");

        assert_eq!(
            mirrored.iter().map(|p| &p.terminal_id).collect::<Vec<_>>(),
            ["term-1"]
        );
        assert_eq!(
            shells.iter().map(|p| &p.terminal_id).collect::<Vec<_>>(),
            ["term-3"]
        );
        assert_eq!(
            extra.iter().map(|p| &p.terminal_id).collect::<Vec<_>>(),
            ["term-2", "term-4"]
        );

        // `mirror_all` moves the space candidate, and only that, into the
        // mirrored list; the extras are unchanged either way.
        let (mirrored, shells, extra) = parse_mirror_panes(line, &labels, true).expect("parse");
        assert_eq!(
            mirrored.iter().map(|p| &p.terminal_id).collect::<Vec<_>>(),
            ["term-1", "term-3"]
        );
        assert!(shells.is_empty());
        assert_eq!(
            extra.iter().map(|p| &p.terminal_id).collect::<Vec<_>>(),
            ["term-2", "term-4"]
        );
    }

    /// Without `mirror_all` the agent-less spaces are still reported, just
    /// separately, so a space the user asked for can be mirrored on its own.
    #[test]
    fn agent_less_spaces_are_reported_separately_when_not_mirroring_everything() {
        let labels = std::collections::HashMap::from([
            ("w1".to_string(), "api".to_string()),
            ("w2".to_string(), "notes".to_string()),
        ]);
        let line = concat!(
            r#"{"id":"x","result":{"type":"pane_list","panes":[{"#,
            r#""agent":"claude","agent_status":"idle","cwd":"/w","focused":false,"#,
            r#""pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","#,
            r#""terminal_id":"term-1","workspace_id":"w1"},{"#,
            r#""agent_status":"unknown","cwd":"/w","focused":false,"#,
            r#""pane_id":"w2:p1","revision":0,"tab_id":"w2:t1","#,
            r#""terminal_id":"term-2","workspace_id":"w2"}]}}"#,
        );

        let (mirrored, shells, _) = parse_mirror_panes(line, &labels, false).expect("parse");

        assert_eq!(
            mirrored.iter().map(|p| &p.workspace_id).collect::<Vec<_>>(),
            ["w1"]
        );
        assert_eq!(
            shells.iter().map(|p| &p.workspace_id).collect::<Vec<_>>(),
            ["w2"]
        );

        // With mirror_all the same space is mirrored outright instead.
        let (mirrored, shells, _) = parse_mirror_panes(line, &labels, true).expect("parse");
        assert_eq!(mirrored.len(), 2);
        assert!(shells.is_empty());
    }

    /// A mirrored agent must be aged by the remote's clock. Mirrors are rebuilt
    /// on every reconnect and handoff, so ageing them locally makes every
    /// mirrored agent look like it went idle moments ago however long it has
    /// really sat there.
    #[test]
    fn discovery_carries_the_remote_state_change_time() {
        let labels = std::collections::HashMap::from([("w1".to_string(), "api".to_string())]);
        let line = concat!(
            r#"{"id":"x","result":{"type":"pane_list","panes":[{"#,
            r#""agent":"claude","agent_status":"idle","cwd":"/w","focused":false,"#,
            r#""pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","#,
            r#""agent_state_changed_at_ms":1750000000000,"#,
            r#""terminal_id":"term-1","workspace_id":"w1"}]}}"#,
        );

        let (mirrored, _, _) = parse_mirror_panes(line, &labels, false).expect("parse");

        assert_eq!(mirrored[0].state_changed_at_ms, Some(1750000000000));
    }

    /// A remote that does not report one leaves the age unknown rather than
    /// inventing "just now".
    #[test]
    fn discovery_tolerates_a_remote_without_a_state_change_time() {
        let labels = std::collections::HashMap::from([("w1".to_string(), "api".to_string())]);
        let line = concat!(
            r#"{"id":"x","result":{"type":"pane_list","panes":[{"#,
            r#""agent":"claude","agent_status":"idle","cwd":"/w","focused":false,"#,
            r#""pane_id":"w1:p1","revision":0,"tab_id":"w1:t1","#,
            r#""terminal_id":"term-1","workspace_id":"w1"}]}}"#,
        );

        let (mirrored, _, _) = parse_mirror_panes(line, &labels, false).expect("parse");

        assert_eq!(mirrored[0].state_changed_at_ms, None);
    }

    #[test]
    fn rename_script_passes_the_label_as_one_argument_and_no_separator() {
        let script = super::rename_workspace_script(None, false, "w8", "nocturne probe");

        assert!(
            script.contains("workspace rename 'w8' 'nocturne probe'"),
            "{script}"
        );
        // `label` is a variadic positional, so a `--` would be kept as a value
        // and land at the front of the name instead of separating anything.
        assert!(!script.contains(" -- "), "{script}");
    }

    #[test]
    fn rename_script_quotes_a_label_that_would_otherwise_run_as_shell() {
        let script = super::rename_workspace_script(None, false, "w8", "a'; rm -rf ~; echo '");

        assert!(!script.contains("rm -rf ~;\n"), "{script}");
        assert!(script.contains("workspace rename 'w8' 'a'"), "{script}");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
