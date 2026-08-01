//! Keeps local mirror workspaces in step with a remote host's agent panes.
//!
//! Each mirror is an ordinary workspace whose single pane runs
//! `ssh -t <host> herdr terminal attach <terminal>`, so the remote agent is
//! fully interactive from the local sidebar. Mirrors are derived state: they
//! are never persisted, and every poll reconciles them against what the remote
//! actually reports.
//!
//! Planning is separated from applying so the reconcile rules are testable
//! without spawning PTYs or reaching the network.

use std::time::Duration;

use crate::config::RemoteSpaceConfig;
use crate::remote::spaces::{attach_argv, mirror_labels, RemoteSpaceSnapshot};
use crate::workspace::{RemoteMirror, Workspace};

use super::App;

/// One change reconcile wants to make to the local workspace list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorAction {
    Create {
        key: String,
        label: String,
        argv: Vec<String>,
        /// Agent the remote reported, passed to the mirror pane as the
        /// `HERDR_AGENT` hint so local detection can see past the ssh wrapper.
        agent: Option<String>,
    },
    Rename {
        ws_idx: usize,
        label: String,
    },
    Close {
        ws_idx: usize,
    },
}

/// Works out how local mirrors for one host differ from what it reports.
///
/// Only mirrors belonging to `space.target` are considered, so hosts stay
/// independent and an empty poll for one never disturbs another's spaces.
/// `Close` actions come first and in descending index order, so applying the
/// plan in order cannot invalidate a later index.
/// The remote's own workspace id, recovered from a mirror key.
///
/// The key is built by `RemoteAgentPane::mirror_key` as target, workspace and
/// terminal joined by unit separators.
fn remote_workspace_id(key: &str) -> Option<String> {
    key.split('\u{1f}').nth(1).map(str::to_string)
}

/// A rename sent to a host, waiting for that host to report it back.
#[derive(Debug, Clone)]
pub(crate) struct PendingMirrorRename {
    pub(crate) label: String,
    /// When to stop protecting the local name. A host that never reports the
    /// rename — because the call was lost, or someone renamed it again there —
    /// must not pin a name that nothing agrees with forever.
    pub(crate) expires_at: std::time::Instant,
}

/// How long a sent rename keeps the local name before the remote has the last
/// word. Long enough for an ssh round trip and the snapshot that follows it.
pub(crate) const MIRROR_RENAME_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

pub(crate) fn plan_remote_mirrors(
    workspaces: &[Workspace],
    space: &RemoteSpaceConfig,
    snapshot: &RemoteSpaceSnapshot,
    pinned: &std::collections::HashSet<String>,
    renaming: &std::collections::HashSet<String>,
) -> Vec<MirrorAction> {
    // A space the user created on this host is mirrored even with no agent in it
    // yet. Without this it would be planned as stale and closed on the very next
    // snapshot, so "new space on sera" would flicker and vanish.
    let mut panes = snapshot.panes.clone();
    panes.extend(
        snapshot
            .shell_panes
            .iter()
            .filter(|pane| pinned.contains(&pane.workspace_id))
            .cloned(),
    );

    let labels = mirror_labels(&panes);
    let desired: Vec<(String, String)> = panes
        .iter()
        .zip(labels)
        .map(|(pane, label)| (pane.mirror_key(&space.target), label))
        .collect();

    let mirror_for = |key: &str| {
        workspaces.iter().position(|workspace| {
            workspace
                .remote_mirror
                .as_ref()
                .is_some_and(|mirror| mirror.target == space.target && mirror.key == key)
        })
    };

    let mut plan: Vec<MirrorAction> = workspaces
        .iter()
        .enumerate()
        .filter(|(_, workspace)| {
            workspace.remote_mirror.as_ref().is_some_and(|mirror| {
                mirror.target == space.target && !desired.iter().any(|(key, _)| *key == mirror.key)
            })
        })
        .map(|(ws_idx, _)| MirrorAction::Close { ws_idx })
        .collect();
    plan.reverse();

    for (index, (key, label)) in desired.iter().enumerate() {
        match mirror_for(key) {
            // A remote workspace can be renamed, or gain a sibling that changes
            // how duplicate labels are disambiguated.
            Some(ws_idx) => {
                // A rename on its way to the host has not come back in a
                // snapshot yet, so the remote still reports the old label.
                // Overwriting now would undo the name mid-flight and then put
                // it back, which reads as the rename having failed.
                if workspaces[ws_idx].custom_name.as_deref() != Some(label.as_str())
                    && !renaming.contains(key)
                {
                    plan.push(MirrorAction::Rename {
                        ws_idx,
                        label: label.clone(),
                    });
                }
            }
            None => {
                let Some(pane) = panes.get(index) else {
                    continue;
                };
                plan.push(MirrorAction::Create {
                    key: key.clone(),
                    label: label.clone(),
                    argv: attach_argv(space, pane, &snapshot.remote_herdr),
                    agent: pane.agent.clone(),
                });
            }
        }
    }
    plan
}

/// Maps a remote's reported status onto local state plus its seen flag.
///
/// `done` is the remote's way of saying "idle, with output you have not looked
/// at", which locally is idle with `seen` cleared.
fn remote_state_and_seen(
    status: crate::api::schema::AgentStatus,
) -> (crate::detect::AgentState, bool) {
    use crate::api::schema::AgentStatus;
    use crate::detect::AgentState;
    match status {
        AgentStatus::Idle => (AgentState::Idle, true),
        AgentStatus::Done => (AgentState::Idle, false),
        AgentStatus::Working => (AgentState::Working, true),
        AgentStatus::Blocked => (AgentState::Blocked, true),
        AgentStatus::Unknown => (AgentState::Unknown, true),
    }
}

/// Builds the mirror record a created workspace carries.
///
/// Production and tests share this so a mirror's stored identity can never
/// drift from the identity `plan_remote_mirrors` looks it up by.
pub(crate) fn remote_mirror_record(space: &RemoteSpaceConfig, key: &str) -> RemoteMirror {
    RemoteMirror {
        disconnected: false,
        target: space.target.clone(),
        host_label: space.display_label().to_string(),
        host_color: space.color.clone(),
        key: key.to_string(),
    }
}

/// Mirrors whose host is no longer configured, in descending index order.
///
/// A host removed from config is never polled again, so reconcile never sees
/// it and its mirrors would otherwise stay in the sidebar for the life of the
/// server.
pub(crate) fn mirrors_for_unconfigured_hosts(
    workspaces: &[Workspace],
    configured: &[RemoteSpaceConfig],
) -> Vec<usize> {
    workspaces
        .iter()
        .enumerate()
        .filter(|(_, workspace)| {
            workspace
                .remote_mirror
                .as_ref()
                .is_some_and(|mirror| !configured.iter().any(|space| space.target == mirror.target))
        })
        .map(|(ws_idx, _)| ws_idx)
        .rev()
        .collect()
}

/// Long-lived per-host worker: hold the live feed when available, poll when not.
///
/// The feed pushes a snapshot on every change, so status and structure track
/// the host in real time. A remote without the feed (older binary) makes the
/// feed return immediately; the worker then polls on the host's interval and
/// re-probes the feed each cycle, so an upgraded remote upgrades to push.
fn run_remote_space_worker(
    space: RemoteSpaceConfig,
    manage_ssh_config: bool,
    event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use crate::remote::spaces::FeedOutcome;

    let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed);
    // `run_feed` asks whether to keep reading, which is the negation of the
    // stop flag. Handing it `stopped` directly ends the feed after its first
    // line, so every host silently falls back to interval polling.
    let keep_streaming = || !stopped();
    let emit = |result: std::io::Result<crate::remote::spaces::RemoteSpaceSnapshot>| {
        let _ = event_tx.blocking_send(crate::events::AppEvent::RemoteSpacesPolled {
            target: space.target.clone(),
            result: result.map_err(|err| err.to_string()),
        });
    };

    while !stopped() {
        let outcome =
            crate::remote::spaces::run_feed(&space, manage_ssh_config, emit, &keep_streaming);

        if stopped() {
            break;
        }

        match outcome {
            // The feed ended after streaming; reconnect after a short pause so a
            // dropped connection does not spin.
            Ok(FeedOutcome::Streamed) => sleep_unless_stopped(Duration::from_secs(2), &stop),
            // No feed on this remote (or it errored): poll once, then wait the
            // configured interval before trying the feed again.
            Ok(FeedOutcome::Unavailable) | Err(_) => {
                emit(crate::remote::spaces::discover(&space, manage_ssh_config));
                sleep_unless_stopped(space.poll_interval(), &stop);
            }
        }
    }
}

fn sleep_unless_stopped(total: Duration, stop: &std::sync::atomic::AtomicBool) {
    // Wake often so a config reload that stops the worker is noticed promptly.
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    while waited < total && !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(step.min(total - waited));
        waited += step;
    }
}

/// Whether a local entry points at the session this server is running as.
///
/// Mirroring your own session is unbounded recursion: each mirror pane is an
/// agent pane, so the next poll would mirror the mirrors.
pub(super) fn mirrors_own_session(space: &RemoteSpaceConfig) -> bool {
    let own = crate::session::active_name()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    let target = space
        .session
        .clone()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    own == target
}

/// A running per-host worker. Setting `stop` ends the worker's loop; the worker
/// tries the live push feed and falls back to polling for hosts without it.
pub(crate) struct RemoteSpaceWorker {
    pub(crate) stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl App {
    fn config_remote_spaces(&self) -> Vec<RemoteSpaceConfig> {
        self.remote_spaces.clone()
    }

    /// Ensures one long-lived worker is running per configured host.
    ///
    /// A worker holds a live event feed when the remote supports it, so mirrors
    /// track the host in real time, and falls back to interval polling when it
    /// does not. Results arrive as [`crate::events::AppEvent::RemoteSpacesPolled`],
    /// the same path a poll used, so reconcile is unchanged.
    pub(crate) fn start_remote_space_polls_if_due(&mut self, _now: std::time::Instant) {
        for space in self.config_remote_spaces() {
            // Mirroring the session we are running in would discover our own
            // mirror panes and mirror those in turn, without bound.
            if space.is_local() && mirrors_own_session(&space) {
                continue;
            }
            if self.remote_space_workers.contains_key(&space.target) {
                continue;
            }
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.remote_space_workers.insert(
                space.target.clone(),
                RemoteSpaceWorker { stop: stop.clone() },
            );

            let event_tx = self.event_tx.clone();
            let manage_ssh_config = self.manage_ssh_config;
            std::thread::spawn(move || {
                run_remote_space_worker(space, manage_ssh_config, event_tx, stop);
            });
        }
    }

    /// Stops worker threads whose host is no longer configured.
    pub(crate) fn stop_unconfigured_remote_space_workers(&mut self) {
        self.remote_space_workers.retain(|target, worker| {
            let keep = self
                .remote_spaces
                .iter()
                .any(|space| space.target == *target);
            if !keep {
                worker
                    .stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            keep
        });
    }

    /// Drops mirrors for hosts that config no longer lists. Called on config
    /// reload, where the removed host will never be polled again.
    pub(crate) fn close_mirrors_for_unconfigured_hosts(&mut self) {
        let stale = mirrors_for_unconfigured_hosts(&self.state.workspaces, &self.remote_spaces);
        if stale.is_empty() {
            return;
        }
        for ws_idx in stale {
            self.close_mirror_at(ws_idx);
        }
        self.shutdown_detached_terminal_runtimes();
    }

    /// Applies a completed poll. A failed poll only reschedules: existing
    /// mirrors are left in place so a brief network blip does not clear the
    /// sidebar and then repopulate it.
    pub(crate) fn handle_remote_spaces_polled(
        &mut self,
        target: String,
        result: Result<RemoteSpaceSnapshot, String>,
    ) {
        let Some(space) = self
            .config_remote_spaces()
            .into_iter()
            .find(|space| space.target == target)
        else {
            // The host was removed from config while an update was in flight.
            self.remote_space_workers.remove(&target);
            self.state.remote_offline_hosts.remove(&target);
            return;
        };

        match result {
            Ok(snapshot) => {
                self.state.remote_offline_hosts.remove(&target);
                self.reconcile_remote_mirrors(&space, &snapshot);
            }
            Err(err) => {
                // The host is unreachable; its mirrors stay put but are marked
                // offline so the sidebar can dim them and sort them down.
                self.state.remote_offline_hosts.insert(target.clone());
                tracing::warn!(target = %target, %err, "remote space update failed");
            }
        }
    }

    /// Asks a mirrored host to open a new space, off the event loop.
    ///
    /// Nothing appears in the sidebar until the host answers; an ssh round trip
    /// is too slow to hold the UI for, and a host that has gone away must not
    /// wedge it at all.
    pub(crate) fn request_remote_space(&mut self, target: String, label: Option<String>) {
        let Some(space) = self
            .config_remote_spaces()
            .into_iter()
            .find(|space| space.target == target)
        else {
            return;
        };
        let manage_ssh_config = self.manage_ssh_config;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = crate::remote::spaces::create_remote_workspace(
                &space,
                manage_ssh_config,
                label.as_deref(),
            );
            let _ = event_tx.blocking_send(crate::events::AppEvent::RemoteSpaceCreated {
                target: space.target.clone(),
                result: result.map_err(|err| err.to_string()),
            });
        });
    }

    /// Sends a mirror's new name to the host that owns the space.
    ///
    /// The mirror shows whatever label the remote reports, so a local rename
    /// alone is undone by the next snapshot. Renaming it there makes the new
    /// name the real one, and every other host mirroring the space follows.
    pub(crate) fn request_remote_rename(&mut self, ws_idx: usize, label: String) {
        let Some(mirror) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.remote_mirror.clone())
        else {
            return;
        };
        let Some(space) = self
            .config_remote_spaces()
            .into_iter()
            .find(|space| space.target == mirror.target)
        else {
            return;
        };
        let Some(workspace_id) = remote_workspace_id(&mirror.key) else {
            return;
        };
        self.pending_mirror_renames.insert(
            mirror.key.clone(),
            PendingMirrorRename {
                label: label.clone(),
                expires_at: std::time::Instant::now() + MIRROR_RENAME_GRACE,
            },
        );
        let manage_ssh_config = self.manage_ssh_config;
        let event_tx = self.event_tx.clone();
        let key = mirror.key;
        std::thread::spawn(move || {
            let result = crate::remote::spaces::rename_remote_workspace(
                &space,
                manage_ssh_config,
                &workspace_id,
                &label,
            );
            let _ = event_tx.blocking_send(crate::events::AppEvent::RemoteSpaceRenamed {
                target: space.target.clone(),
                key,
                result: result.map_err(|err| err.to_string()),
            });
        });
    }

    /// Applies the result of [`Self::request_remote_rename`].
    ///
    /// Only failure needs handling: dropping the pending entry hands the name
    /// back to the remote, so the next snapshot restores what the host still
    /// calls it rather than leaving a name that only exists here. A success is
    /// left pending until a snapshot carries the new label.
    pub(crate) fn handle_remote_space_renamed(
        &mut self,
        target: String,
        key: String,
        result: Result<(), String>,
    ) {
        if let Err(err) = result {
            tracing::warn!(target = %target, %err, "renaming a remote space failed");
            self.pending_mirror_renames.remove(&key);
        }
    }

    /// Applies the result of [`Self::request_remote_space`].
    pub(crate) fn handle_remote_space_created(
        &mut self,
        target: String,
        result: Result<crate::remote::spaces::CreatedRemoteSpace, String>,
    ) {
        let Some(space) = self
            .config_remote_spaces()
            .into_iter()
            .find(|space| space.target == target)
        else {
            return;
        };
        let created = match result {
            Ok(created) => created,
            Err(err) => {
                tracing::warn!(target = %target, %err, "creating a remote space failed");
                self.state.remote_offline_hosts.insert(target);
                return;
            }
        };

        // Pin it first: the space has no agent yet, so without the pin the very
        // next snapshot would plan it as stale and close it again.
        self.state
            .created_remote_workspaces
            .entry(target.clone())
            .or_default()
            .insert(created.pane.workspace_id.clone());
        self.state.remote_offline_hosts.remove(&target);

        // Mirror it from the create response instead of waiting for a snapshot,
        // so the space appears as soon as the host confirms it. Reconcile will
        // find this mirror already matching and leave it alone.
        let key = created.pane.mirror_key(&target);
        let label = created.pane.mirror_label();
        let argv = attach_argv(&space, &created.pane, &created.remote_herdr);
        let mirror = remote_mirror_record(&space, &key);
        if let Err(err) = self.create_remote_mirror(mirror, &label, &argv, None) {
            tracing::warn!(target = %target, %err, "mirroring a new remote space failed");
            return;
        }
        // Focus it, since the user just asked for it. Creating a local space
        // focuses it too, so this keeps the two paths feeling the same.
        if let Some(ws_idx) = self.state.workspaces.len().checked_sub(1) {
            self.state.switch_workspace(ws_idx);
            self.state.mode = crate::app::state::Mode::Terminal;
        }
    }

    /// Brings mirrors for one configured host in line with `snapshot`.
    pub(crate) fn reconcile_remote_mirrors(
        &mut self,
        space: &RemoteSpaceConfig,
        snapshot: &RemoteSpaceSnapshot,
    ) {
        // Mirrors kept as placeholders while the host was away have no live
        // pane. The host is answering again, so drop them and let the plan below
        // build them fresh — the key still matches, so nothing else would.
        let disconnected: Vec<usize> = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| {
                ws.remote_mirror
                    .as_ref()
                    .is_some_and(|mirror| mirror.target == space.target && mirror.disconnected)
            })
            .map(|(ws_idx, _)| ws_idx)
            .rev()
            .collect();
        for ws_idx in disconnected {
            self.close_mirror_at(ws_idx);
        }

        let pinned = self
            .state
            .created_remote_workspaces
            .get(&space.target)
            .cloned()
            .unwrap_or_default();
        // A pending rename is done with once the host reports the new label, or
        // once it has had long enough to and has not.
        let now = std::time::Instant::now();
        let reported: std::collections::HashMap<String, String> = snapshot
            .panes
            .iter()
            .chain(snapshot.shell_panes.iter())
            .map(|pane| (pane.mirror_key(&space.target), pane.workspace_label.clone()))
            .collect();
        self.pending_mirror_renames.retain(|key, pending| {
            now < pending.expires_at && reported.get(key) != Some(&pending.label)
        });
        let renaming: std::collections::HashSet<String> =
            self.pending_mirror_renames.keys().cloned().collect();
        let plan = plan_remote_mirrors(&self.state.workspaces, space, snapshot, &pinned, &renaming);

        let mut closed = 0usize;
        for action in plan {
            match action {
                MirrorAction::Close { ws_idx } => {
                    self.close_mirror_at(ws_idx);
                    closed += 1;
                }
                MirrorAction::Rename { ws_idx, label } => {
                    if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
                        workspace.custom_name = Some(label);
                    }
                }
                MirrorAction::Create {
                    key,
                    label,
                    argv,
                    agent,
                } => {
                    let mirror = remote_mirror_record(space, &key);
                    if let Err(err) =
                        self.create_remote_mirror(mirror, &label, &argv, agent.as_deref())
                    {
                        tracing::warn!(
                            target = %space.target,
                            %err,
                            "could not create remote mirror workspace"
                        );
                    }
                }
            }
        }
        if closed > 0 {
            self.shutdown_detached_terminal_runtimes();
        }
        // Always, even when the structure plan was empty: an agent changing
        // status (idle->working) is not a structure change, and its mirror must
        // still follow. Runs after the plan so brand-new mirrors already exist.
        self.report_remote_agent_states(space, snapshot);
    }

    /// Pushes each remote pane's reported status onto its mirror.
    ///
    /// The mirror's own screen detection only ever sees the attached copy and
    /// reports idle forever, so the remote — which has hook-level authority
    /// over its agents — is the trustworthy source of state here.
    fn report_remote_agent_states(
        &mut self,
        space: &RemoteSpaceConfig,
        snapshot: &RemoteSpaceSnapshot,
    ) {
        for pane in &snapshot.panes {
            let key = pane.mirror_key(&space.target);
            let Some(pane_id) =
                self.state
                    .workspaces
                    .iter()
                    .find(|workspace| {
                        workspace.remote_mirror.as_ref().is_some_and(|mirror| {
                            mirror.target == space.target && mirror.key == key
                        })
                    })
                    .and_then(|workspace| workspace.tabs.first())
                    .map(|tab| tab.root_pane)
            else {
                continue;
            };
            let Some(agent_label) = pane.agent.clone() else {
                continue;
            };
            let (state, seen) = remote_state_and_seen(pane.status);
            let remote_changed_at = pane.state_changed_at_ms;
            self.handle_internal_event(crate::events::AppEvent::HookStateReported {
                pane_id,
                source: crate::detect::REMOTE_MIRROR_HOOK_SOURCE.to_string(),
                agent_label,
                state,
                message: None,
                seq: None,
                session_ref: None,
            });
            if let Some(workspace) = self.state.workspaces.iter_mut().find(|workspace| {
                workspace
                    .tabs
                    .first()
                    .is_some_and(|tab| tab.root_pane == pane_id)
            }) {
                // "done" on the remote means idle with unseen output; mirror
                // that so the attention dot matches what the remote shows.
                if let Some(tab) = workspace.tabs.first_mut() {
                    if let Some(pane_state) = tab.panes.get_mut(&pane_id) {
                        pane_state.seen = seen;
                    }
                }
            }

            // Age the mirror by the remote's clock, not ours. Applying the
            // state above stamps it as changing now, and mirrors are rebuilt on
            // every reconnect and handoff, so without this every mirrored agent
            // reads as having just gone idle however long it has really sat.
            if let Some(changed_at) = remote_changed_at {
                let terminal_id = self
                    .state
                    .workspaces
                    .iter()
                    .find(|workspace| {
                        workspace
                            .tabs
                            .first()
                            .is_some_and(|tab| tab.root_pane == pane_id)
                    })
                    .and_then(|workspace| workspace.tabs.first())
                    .and_then(|tab| tab.terminal_id(pane_id))
                    .cloned();
                if let Some(terminal) =
                    terminal_id.and_then(|terminal_id| self.state.terminals.get_mut(&terminal_id))
                {
                    terminal.agent_state_changed_at_ms = Some(changed_at);
                }
            }
        }
    }

    fn close_mirror_at(&mut self, ws_idx: usize) {
        let pane_ids = self
            .state
            .workspaces
            .get(ws_idx)
            .map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // Closing goes through the selected workspace, so the user's own
        // selection has to be restored afterwards by identity: a mirror
        // disappearing must never move their cursor to another space.
        let selected_id = self
            .state
            .workspaces
            .get(self.state.selected)
            .map(|workspace| workspace.id.clone())
            .filter(|_| self.state.selected != ws_idx);
        self.state.selected = ws_idx;
        self.state.close_selected_workspace();
        if let Some(selected_id) = selected_id {
            if let Some(restored) = self
                .state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == selected_id)
            {
                self.state.selected = restored;
            }
        }
        self.state.remove_plugin_pane_records(pane_ids);
    }

    fn create_remote_mirror(
        &mut self,
        mirror: RemoteMirror,
        label: &str,
        argv: &[String],
        agent: Option<&str>,
    ) -> std::io::Result<()> {
        let (rows, cols) = self.state.estimate_pane_size();
        // The ssh client runs locally, so this cwd only sets where that local
        // process starts; it is never the remote agent's working directory.
        let cwd = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // The pane's foreground process is ssh, which hides the remote agent
        // from screen detection. HERDR_AGENT is Herdr's existing hint for
        // exactly that case, so the mirror reports the agent the remote named.
        let extra_env = agent
            .and_then(crate::detect::parse_agent_label)
            .map(|agent| {
                vec![(
                    "HERDR_AGENT".to_string(),
                    crate::detect::agent_label(agent).to_string(),
                )]
            })
            .unwrap_or_default();
        let (mut workspace, terminal, runtime) = Workspace::new_argv_command_with_extra_env(
            cwd,
            rows,
            cols,
            argv,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
            extra_env,
        )?;
        workspace.custom_name = Some(label.to_string());
        workspace.remote_mirror = Some(mirror);

        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        // Mirrors arrive without stealing focus; they are background context,
        // not something the user asked to switch to.
        self.state.workspaces.push(workspace);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::spaces::RemoteAgentPane;

    /// Planning with nothing pinned, which is the case for every test that is
    /// not specifically about user-created remote spaces. Shadows the real
    /// function so those tests read without a trailing empty set.
    fn plan_remote_mirrors(
        workspaces: &[Workspace],
        space: &RemoteSpaceConfig,
        snapshot: &RemoteSpaceSnapshot,
    ) -> Vec<MirrorAction> {
        super::plan_remote_mirrors(
            workspaces,
            space,
            snapshot,
            &Default::default(),
            &Default::default(),
        )
    }

    fn snapshot(panes: Vec<RemoteAgentPane>) -> RemoteSpaceSnapshot {
        RemoteSpaceSnapshot {
            remote_herdr: "/usr/bin/herdr".into(),
            panes,
            shell_panes: Vec::new(),
        }
    }

    /// A snapshot where the host is not mirroring agent-less spaces, so they
    /// arrive as candidates instead.
    fn snapshot_with_shells(
        panes: Vec<RemoteAgentPane>,
        shell_panes: Vec<RemoteAgentPane>,
    ) -> RemoteSpaceSnapshot {
        RemoteSpaceSnapshot {
            remote_herdr: "/usr/bin/herdr".into(),
            panes,
            shell_panes,
        }
    }

    fn agent_pane(workspace_id: &str, label: &str, terminal_id: &str) -> RemoteAgentPane {
        RemoteAgentPane {
            terminal_id: terminal_id.into(),
            workspace_id: workspace_id.into(),
            workspace_label: label.into(),
            agent: Some("claude".into()),
            status: crate::api::schema::AgentStatus::Idle,
            state_changed_at_ms: None,
        }
    }

    fn space(target: &str) -> RemoteSpaceConfig {
        RemoteSpaceConfig {
            target: target.into(),
            session: None,
            label: None,
            poll_seconds: 30,
            mirror_all: false,
            color: None,
        }
    }

    /// A local workspace, as the sidebar would already hold.
    fn local(name: &str) -> Workspace {
        Workspace::test_new(name)
    }

    /// A workspace standing in for an already-created mirror. Uses the same
    /// record constructor production does, so a mismatch between how a mirror
    /// is stored and how it is looked up cannot hide behind the helper.
    fn mirror(target: &str, key: &str, label: &str) -> Workspace {
        let mut workspace = Workspace::test_new(label);
        workspace.custom_name = Some(label.to_string());
        workspace.remote_mirror = Some(remote_mirror_record(&space(target), key));
        workspace
    }

    /// Applies a plan's creates the way `reconcile_remote_mirrors` does, minus
    /// the PTY, so a plan can be fed back through the planner.
    fn apply_creates(
        workspaces: &mut Vec<Workspace>,
        space: &RemoteSpaceConfig,
        plan: &[MirrorAction],
    ) {
        for action in plan {
            if let MirrorAction::Create { key, label, .. } = action {
                let mut workspace = Workspace::test_new(label);
                workspace.custom_name = Some(label.clone());
                workspace.remote_mirror = Some(remote_mirror_record(space, key));
                workspaces.push(workspace);
            }
        }
    }

    #[test]
    fn a_second_poll_after_creating_mirrors_is_a_no_op() {
        // Regression: the mirror record once stored the host label in the key
        // field, so no mirror ever matched and every poll closed and recreated
        // the whole set, churning panes and stealing the user's selection.
        let space = space("workbox");
        let snapshot = snapshot(vec![
            agent_pane("w1", "api", "term-1"),
            agent_pane("w2", "web", "term-2"),
        ]);
        let mut workspaces = vec![local("local")];

        let first = plan_remote_mirrors(&workspaces, &space, &snapshot);
        apply_creates(&mut workspaces, &space, &first);
        let second = plan_remote_mirrors(&workspaces, &space, &snapshot);

        assert_eq!(first.len(), 2);
        assert_eq!(second, Vec::new(), "a settled mirror set must not churn");
    }

    #[test]
    fn a_created_mirror_stores_the_key_the_planner_looks_it_up_by() {
        let space = space("workbox");
        let pane = agent_pane("w1", "api", "term-1");

        let record = remote_mirror_record(&space, &pane.mirror_key(&space.target));

        assert_eq!(record.key, pane.mirror_key("workbox"));
        assert_eq!(record.target, "workbox");
        assert_eq!(record.host_label, "workbox");
    }

    #[test]
    fn a_configured_label_does_not_leak_into_the_mirror_key() {
        let mut space = space("workbox.example.com");
        space.label = Some("box".into());
        let pane = agent_pane("w1", "api", "term-1");
        let key = pane.mirror_key(&space.target);

        let record = remote_mirror_record(&space, &key);

        assert_eq!(record.host_label, "box");
        assert_eq!(record.key, key);
        assert_ne!(record.key, record.host_label);
    }

    fn key_for(target: &str, workspace_id: &str, terminal_id: &str) -> String {
        agent_pane(workspace_id, "ignored", terminal_id).mirror_key(target)
    }

    /// A space the user asked for on a host is mirrored even with no agent in
    /// it, which is the whole point of creating one from the sidebar.
    #[test]
    fn plan_mirrors_a_pinned_agent_less_space() {
        let snapshot = snapshot_with_shells(vec![], vec![agent_pane("w7", "notes", "term-7")]);
        let pinned = std::collections::HashSet::from(["w7".to_string()]);

        let plan = super::plan_remote_mirrors(
            &[],
            &space("workbox"),
            &snapshot,
            &pinned,
            &Default::default(),
        );

        assert_eq!(
            plan,
            vec![MirrorAction::Create {
                key: key_for("workbox", "w7", "term-7"),
                label: "notes".into(),
                argv: attach_argv(
                    &space("workbox"),
                    &agent_pane("w7", "notes", "term-7"),
                    "/usr/bin/herdr"
                ),
                agent: Some("claude".into()),
            }]
        );
    }

    /// Without the pin the sidebar stays limited to agents, which is what a host
    /// without `mirror_all` is asking for.
    #[test]
    fn plan_ignores_agent_less_spaces_that_were_not_asked_for() {
        let snapshot = snapshot_with_shells(vec![], vec![agent_pane("w7", "notes", "term-7")]);

        let plan = plan_remote_mirrors(&[], &space("workbox"), &snapshot);

        assert_eq!(plan, Vec::new());
    }

    /// The regression that makes the feature usable at all: a created space must
    /// not be closed by the next snapshot that reports no agent in it.
    #[test]
    fn a_pinned_space_survives_the_snapshot_after_it_is_mirrored() {
        let snapshot = snapshot_with_shells(vec![], vec![agent_pane("w7", "notes", "term-7")]);
        let pinned = std::collections::HashSet::from(["w7".to_string()]);
        let workspaces = vec![mirror(
            "workbox",
            &key_for("workbox", "w7", "term-7"),
            "notes",
        )];

        let plan = super::plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot,
            &pinned,
            &Default::default(),
        );

        assert_eq!(plan, Vec::new());
    }

    #[test]
    fn plan_creates_a_mirror_for_each_remote_agent_pane() {
        let workspaces = vec![local("local")];

        let plan = plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![
                agent_pane("w1", "api", "term-1"),
                agent_pane("w2", "web", "term-2"),
            ]),
        );

        let labels: Vec<&str> = plan
            .iter()
            .filter_map(|action| match action {
                MirrorAction::Create { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["api", "web"]);
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn plan_attaches_each_mirror_to_its_remote_terminal() {
        let plan = plan_remote_mirrors(
            &[],
            &space("workbox"),
            &snapshot(vec![agent_pane("w1", "api", "term-1")]),
        );

        let MirrorAction::Create { argv, .. } = &plan[0] else {
            panic!("expected a create action, got {plan:?}");
        };
        assert_eq!(argv[0], "ssh");
        // Positions after argv[0] shift as ssh options are added, so assert the
        // ends: the host and the remote command are always last.
        assert_eq!(argv[argv.len() - 2], "workbox");
        assert!(
            argv[argv.len() - 1].contains("terminal attach 'term-1'"),
            "{argv:?}"
        );
    }

    #[test]
    fn plan_is_empty_when_mirrors_already_match_the_remote() {
        let workspaces = vec![
            local("local"),
            mirror("workbox", &key_for("workbox", "w1", "term-1"), "api"),
        ];

        let plan = plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![agent_pane("w1", "api", "term-1")]),
        );

        assert_eq!(plan, Vec::new());
    }

    #[test]
    fn plan_closes_mirrors_whose_remote_pane_disappeared() {
        let workspaces = vec![
            mirror("workbox", &key_for("workbox", "w1", "term-1"), "api"),
            mirror("workbox", &key_for("workbox", "w2", "term-2"), "web"),
        ];

        let plan = plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![agent_pane("w2", "web", "term-2")]),
        );

        assert_eq!(plan, vec![MirrorAction::Close { ws_idx: 0 }]);
    }

    #[test]
    fn plan_closes_in_descending_index_order_so_applying_stays_valid() {
        let workspaces = vec![
            local("local"),
            mirror("workbox", &key_for("workbox", "w1", "term-1"), "a"),
            mirror("workbox", &key_for("workbox", "w2", "term-2"), "b"),
            mirror("workbox", &key_for("workbox", "w3", "term-3"), "c"),
        ];

        let plan = plan_remote_mirrors(&workspaces, &space("workbox"), &snapshot(vec![]));

        assert_eq!(
            plan,
            vec![
                MirrorAction::Close { ws_idx: 3 },
                MirrorAction::Close { ws_idx: 2 },
                MirrorAction::Close { ws_idx: 1 },
            ]
        );
    }

    #[test]
    fn plan_never_touches_local_workspaces() {
        let workspaces = vec![local("one"), local("two")];

        let plan = plan_remote_mirrors(&workspaces, &space("workbox"), &snapshot(vec![]));

        assert_eq!(plan, Vec::new());
    }

    #[test]
    fn plan_only_touches_mirrors_from_the_polled_host() {
        let workspaces = vec![
            mirror("workbox", &key_for("workbox", "w1", "term-1"), "api"),
            mirror("other", &key_for("other", "w1", "term-9"), "logs"),
        ];

        // An empty poll for one host must leave the other host's mirrors alone.
        let plan = plan_remote_mirrors(&workspaces, &space("workbox"), &snapshot(vec![]));

        assert_eq!(plan, vec![MirrorAction::Close { ws_idx: 0 }]);
    }

    #[test]
    fn plan_renames_a_mirror_when_the_remote_workspace_is_renamed() {
        let workspaces = vec![mirror(
            "workbox",
            &key_for("workbox", "w1", "term-1"),
            "api",
        )];

        let plan = plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![agent_pane("w1", "api-server", "term-1")]),
        );

        assert_eq!(
            plan,
            vec![MirrorAction::Rename {
                ws_idx: 0,
                label: "api-server".into(),
            }]
        );
    }

    #[test]
    fn plan_leaves_a_mirror_alone_while_its_rename_is_on_its_way_to_the_host() {
        // Renamed here to "api-server"; the host still reports the old "api",
        // because the rename has not reached it yet.
        let key = key_for("workbox", "w1", "term-1");
        let workspaces = vec![mirror("workbox", &key, "api-server")];
        let renaming = std::collections::HashSet::from([key]);

        let plan = super::plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![agent_pane("w1", "api", "term-1")]),
            &Default::default(),
            &renaming,
        );

        assert!(
            plan.is_empty(),
            "a rename in flight must not be undone mid-way: {plan:?}"
        );
    }

    #[test]
    fn plan_takes_the_hosts_label_back_once_no_rename_is_in_flight() {
        // The same disagreement, with nothing pending: the host wins, which is
        // what makes a failed or expired rename fall back rather than stick.
        let workspaces = vec![mirror(
            "workbox",
            &key_for("workbox", "w1", "term-1"),
            "api-server",
        )];

        let plan = plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![agent_pane("w1", "api", "term-1")]),
        );

        assert_eq!(
            plan,
            vec![MirrorAction::Rename {
                ws_idx: 0,
                label: "api".into(),
            }]
        );
    }

    #[test]
    fn remote_workspace_id_comes_back_out_of_a_mirror_key() {
        assert_eq!(
            super::remote_workspace_id(&key_for("workbox", "w1", "term-1")),
            Some("w1".to_string())
        );
        assert_eq!(super::remote_workspace_id("not-a-key"), None);
    }

    #[test]
    fn plan_renames_when_a_new_sibling_forces_label_disambiguation() {
        let workspaces = vec![mirror(
            "workbox",
            &key_for("workbox", "w1", "term-1"),
            "lifestream",
        )];

        // A second space with the same remote name arrives, so the existing
        // mirror has to pick up a suffix too.
        let plan = plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot(vec![
                agent_pane("w1", "lifestream", "term-1"),
                agent_pane("w2", "lifestream", "term-2"),
            ]),
        );

        assert_eq!(
            plan,
            vec![
                MirrorAction::Rename {
                    ws_idx: 0,
                    label: "lifestream 1".into(),
                },
                MirrorAction::Create {
                    key: key_for("workbox", "w2", "term-2"),
                    label: "lifestream 2".into(),
                    argv: attach_argv(
                        &space("workbox"),
                        &agent_pane("w2", "lifestream", "term-2"),
                        "/usr/bin/herdr",
                    ),
                    agent: Some("claude".into()),
                },
            ]
        );
    }

    #[test]
    fn plan_carries_the_remote_agent_so_the_mirror_can_hint_past_ssh() {
        let plan = plan_remote_mirrors(
            &[],
            &space("workbox"),
            &snapshot(vec![agent_pane("w1", "api", "term-1")]),
        );

        let MirrorAction::Create { agent, .. } = &plan[0] else {
            panic!("expected a create action, got {plan:?}");
        };
        // The mirror pane's foreground process is ssh, so the agent name has to
        // travel with the plan for the HERDR_AGENT hint.
        assert_eq!(agent.as_deref(), Some("claude"));
    }

    #[test]
    fn removing_a_host_from_config_strands_no_mirrors() {
        let workspaces = vec![
            local("local"),
            mirror("workbox", &key_for("workbox", "w1", "t1"), "api"),
            mirror("other", &key_for("other", "w1", "t2"), "logs"),
            mirror("workbox", &key_for("workbox", "w2", "t3"), "web"),
        ];

        // Config now lists only "other"; nothing will ever poll workbox again.
        let stale = mirrors_for_unconfigured_hosts(&workspaces, &[space("other")]);

        // Descending order so applying them in sequence stays valid.
        assert_eq!(stale, vec![3, 1]);
    }

    #[test]
    fn configured_hosts_keep_their_mirrors() {
        let workspaces = vec![
            local("local"),
            mirror("workbox", &key_for("workbox", "w1", "t1"), "api"),
        ];

        let stale = mirrors_for_unconfigured_hosts(&workspaces, &[space("workbox")]);

        assert_eq!(stale, Vec::<usize>::new());
    }

    #[test]
    fn clearing_every_host_removes_every_mirror_but_no_local_space() {
        let workspaces = vec![
            local("local"),
            mirror("workbox", &key_for("workbox", "w1", "t1"), "api"),
        ];

        let stale = mirrors_for_unconfigured_hosts(&workspaces, &[]);

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn a_local_entry_for_our_own_session_is_refused() {
        // The default session is what an entry with no session name means, and
        // tests run without HERDR_SESSION set, so this is self-mirroring.
        let mut own = space("local");
        own.session = None;
        assert!(mirrors_own_session(&own));

        let mut other = space("local");
        other.session = Some("work".into());
        assert!(!mirrors_own_session(&other));
    }

    #[test]
    fn plan_uses_the_configured_label_instead_of_the_ssh_target() {
        let mut space = space("workbox.example.com");
        space.label = Some("box".into());

        let plan = plan_remote_mirrors(
            &[],
            &space,
            &snapshot(vec![agent_pane("w1", "api", "term-1")]),
        );

        let MirrorAction::Create { label, .. } = &plan[0] else {
            panic!("expected a create action, got {plan:?}");
        };
        assert_eq!(label, "api");
    }
}
