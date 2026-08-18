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
        /// Set when this pane reached us through another host. The mirror is
        /// still reconciled against the host we poll, but it is shown under the
        /// machine actually running it.
        origin: Option<crate::remote::spaces::MirrorOrigin>,
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

/// Which pin keeps a pane the user just created alive across the next poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatedPin {
    /// A whole new space: mirror its candidate pane until an agent runs there.
    Workspace,
    /// A tab in a space that is mirrored already: mirror just this pane.
    Pane,
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
    pinned_panes: &std::collections::HashSet<String>,
    mirrored_hosts: &std::collections::HashSet<String>,
    local_terminals: &std::collections::HashSet<String>,
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
            .filter(|pane| {
                pinned.contains(&pane.workspace_id) || pinned_panes.contains(&pane.terminal_id)
            })
            .cloned(),
    );
    // A tab the user created on this host, in a space that is mirrored already.
    // Only the pane itself is pinned, so the rest of that space's plain shells
    // stay out of the sidebar the way they always have.
    panes.extend(
        snapshot
            .extra_panes
            .iter()
            .filter(|pane| pinned_panes.contains(&pane.terminal_id))
            .cloned(),
    );

    // A pane that is itself a mirror of a host we also mirror is that host's
    // agent seen a second time, one hop further away. Drop it and keep the one
    // that comes straight from the machine running it: it carries that host's
    // own colour and label, and its pane is one ssh hop rather than two.
    //
    // A mirror of a host we do *not* mirror is kept, since it is the only way
    // that agent reaches this sidebar at all.
    //
    // The same goes doubly for a mirror of *us*: two hosts mirroring each other
    // is an ordinary thing to want, and without this our own panes come home
    // wearing a remote host's name and sit beside themselves in the sidebar.
    // Matched on the terminal id rather than the host name because a name is
    // whatever the other machine happens to call us -- `pandora`, `ryi@pandora`,
    // a tailnet address -- while the terminal it names is either one of ours or
    // it is not.
    panes.retain(|pane| {
        pane.origin.as_ref().is_none_or(|origin| {
            !local_terminals.contains(&origin.terminal_id)
                && !mirrored_hosts.contains(&crate::remote::spaces::MirrorOrigin::host_key(
                    &origin.target,
                ))
        })
    });

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
                    origin: pane.origin.clone(),
                });
            }
        }
    }
    plan
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

/// The same record, named after the machine the pane really runs on.
///
/// A pane reached through another host is still reconciled against the host we
/// poll -- that is what `target` and `key` are for -- but showing it under that
/// host says the wrong thing: the agent is not there, and the sidebar reads as
/// though one machine is running everything. Only the display changes.
///
/// The reporting host passes on how it labels the origin, so the name and
/// colour match what that machine is called everywhere else. Failing that, the
/// host part of the target is a better answer than the hop's name, and leaving
/// the colour unset lets the sidebar derive its usual one from that name.
pub(crate) fn remote_mirror_record_for_origin(
    space: &RemoteSpaceConfig,
    key: &str,
    origin: &crate::remote::spaces::MirrorOrigin,
) -> RemoteMirror {
    RemoteMirror {
        host_label: origin
            .label
            .clone()
            .unwrap_or_else(|| crate::remote::spaces::MirrorOrigin::host_key(&origin.target)),
        host_color: origin.color.clone(),
        ..remote_mirror_record(space, key)
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
                result: result.map(Box::new).map_err(|err| err.to_string()),
            });
        });
    }

    /// Asks the host that owns the active mirror for another tab in that space.
    ///
    /// Returns false when the active space is not a mirror, so the caller falls
    /// back to creating the tab here. A tab made locally inside a mirror would
    /// run on this machine rather than the one the space belongs to, and
    /// reconcile closes a mirror's whole workspace when the remote pane goes
    /// away, so it would disappear with it.
    #[cfg(unix)]
    pub(crate) fn request_remote_tab(&mut self, label: Option<&str>) -> bool {
        let Some(mirror) = self
            .state
            .active
            .and_then(|ws_idx| self.state.workspaces.get(ws_idx))
            .and_then(|workspace| workspace.remote_mirror.clone())
        else {
            return false;
        };
        let Some(space) = self
            .config_remote_spaces()
            .into_iter()
            .find(|space| space.target == mirror.target)
        else {
            return false;
        };
        let Some(workspace_id) = remote_workspace_id(&mirror.key) else {
            return false;
        };
        let manage_ssh_config = self.manage_ssh_config;
        let event_tx = self.event_tx.clone();
        let label = label.map(str::to_string);
        std::thread::spawn(move || {
            let result = crate::remote::spaces::create_remote_tab(
                &space,
                manage_ssh_config,
                &workspace_id,
                label.as_deref(),
            );
            let _ = event_tx.blocking_send(crate::events::AppEvent::RemoteTabCreated {
                target: space.target.clone(),
                result: result.map(Box::new).map_err(|err| err.to_string()),
            });
        });
        true
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
        result: Result<Box<crate::remote::spaces::CreatedRemoteSpace>, String>,
    ) {
        self.mirror_created_remote_pane(target, result, CreatedPin::Workspace);
    }

    /// Applies the result of [`Self::request_remote_tab`].
    ///
    /// Same handling as a created space: the pane is mirrored straight from the
    /// response and focused. Only the pin differs — the space is mirrored
    /// already, so pinning it would pull in its other shells too.
    #[cfg(unix)]
    pub(crate) fn handle_remote_tab_created(
        &mut self,
        target: String,
        result: Result<Box<crate::remote::spaces::CreatedRemoteSpace>, String>,
    ) {
        self.mirror_created_remote_pane(target, result, CreatedPin::Pane);
    }

    fn mirror_created_remote_pane(
        &mut self,
        target: String,
        result: Result<Box<crate::remote::spaces::CreatedRemoteSpace>, String>,
        pin: CreatedPin,
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

        // Pin it first: the new pane runs no agent yet, so without the pin the
        // very next snapshot would plan it as stale and close it again.
        match pin {
            CreatedPin::Workspace => self
                .state
                .created_remote_workspaces
                .entry(target.clone())
                .or_default()
                .insert(created.pane.workspace_id.clone()),
            CreatedPin::Pane => self
                .state
                .created_remote_panes
                .entry(target.clone())
                .or_default()
                .insert(created.pane.terminal_id.clone()),
        };
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
        let pinned_panes = self
            .state
            .created_remote_panes
            .get(&space.target)
            .cloned()
            .unwrap_or_default();
        // Every host we are configured to mirror, so a reflection of one of
        // them can be told from a reflection of somewhere we cannot reach.
        let mirrored_hosts: std::collections::HashSet<String> = self
            .config_remote_spaces()
            .iter()
            .map(|configured| crate::remote::spaces::MirrorOrigin::host_key(&configured.target))
            .collect();
        // Terminals this machine owns, so a reflection of one of them can be
        // recognised whatever the reporting host calls us.
        let local_terminals: std::collections::HashSet<String> = self
            .state
            .terminals
            .keys()
            .map(|terminal_id| terminal_id.to_string())
            .collect();
        let plan = plan_remote_mirrors(
            &self.state.workspaces,
            space,
            snapshot,
            &pinned,
            &pinned_panes,
            &mirrored_hosts,
            &local_terminals,
            &renaming,
        );

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
                    origin,
                } => {
                    let mirror = match &origin {
                        Some(origin) => remote_mirror_record_for_origin(space, &key, origin),
                        None => remote_mirror_record(space, &key),
                    };
                    let created = if self.mirrors_are_multiplexed(&space.target) {
                        self.create_streamed_mirror(mirror, &label)
                    } else {
                        self.create_remote_mirror(mirror, &label, &argv, agent.as_deref())
                    };
                    if let Err(err) = created {
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
        if self.mirrors_are_multiplexed(&space.target) {
            self.mirror_remote_herdr
                .insert(space.target.clone(), snapshot.remote_herdr.clone());
            // After the plan, so the set named to the host is the set that now
            // exists here rather than the one that did a moment ago.
            self.update_mirror_stream(space, &snapshot.remote_herdr);
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
        // Mirrors come and go; their unread marks should not outlive them.
        let live: std::collections::HashSet<String> = snapshot
            .panes
            .iter()
            .map(|pane| pane.mirror_key(&space.target))
            .collect();
        let prefix = format!("{}\u{1f}", space.target);
        self.mirror_unseen_marks
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));

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
            let (state, seen) = crate::app::pane_state_and_seen(pane.status);
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
            // "done" on the remote means idle with output nobody has read, and
            // the host keeps saying it on every poll until someone reads it
            // *there*. Reading the mirror here is a local act the host never
            // hears about, so applying its answer unconditionally would undo the
            // read a poll later. An unread mark is therefore only laid down once
            // per remote change: the same "done" repeated leaves the mirror as
            // the reader left it, and a newer one marks it unread again.
            let apply_unseen = if seen {
                self.mirror_unseen_marks.remove(&key);
                true
            } else {
                match remote_changed_at {
                    // A host too old to stamp its changes cannot be told apart
                    // from one repeating itself, so it keeps the old behaviour.
                    None => true,
                    Some(changed_at) => {
                        let fresh = self
                            .mirror_unseen_marks
                            .get(&key)
                            .is_none_or(|applied| changed_at > *applied);
                        if fresh {
                            self.mirror_unseen_marks.insert(key.clone(), changed_at);
                        }
                        fresh
                    }
                }
            };
            if apply_unseen {
                if let Some(workspace) = self.state.workspaces.iter_mut().find(|workspace| {
                    workspace
                        .tabs
                        .first()
                        .is_some_and(|tab| tab.root_pane == pane_id)
                }) {
                    if let Some(tab) = workspace.tabs.first_mut() {
                        if let Some(pane_state) = tab.panes.get_mut(&pane_id) {
                            pane_state.seen = seen;
                        }
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

    /// Finds the mirror of one remote terminal, by the host and the id that
    /// host knows it by.
    fn mirror_index_for_terminal(&self, target: &str, terminal_id: &str) -> Option<usize> {
        self.state.workspaces.iter().position(|workspace| {
            workspace.remote_mirror.as_ref().is_some_and(|mirror| {
                mirror.target == target
                    && crate::remote::spaces::RemoteAgentPane::split_key(&mirror.key)
                        .is_some_and(|(_, remote_terminal)| remote_terminal == terminal_id)
            })
        })
    }

    /// Draws a frame that arrived over a host's shared connection.
    pub(crate) fn apply_mirror_frame(&mut self, target: &str, terminal_id: &str, bytes: &[u8]) {
        // Hearing from the host at all is what proves it is answering, whether
        // or not this particular frame still has a pane to land in, so the next
        // failure starts counting from nothing again.
        self.mirror_stream_retry.remove(target);
        let Some(ws_idx) = self.mirror_index_for_terminal(target, terminal_id) else {
            // A frame for a mirror that has since been closed. The host is told
            // the new set on the next reconcile, so this settles by itself.
            return;
        };
        let Some(local_terminal) = self.state.workspaces.get(ws_idx).and_then(|workspace| {
            let pane_id = workspace.root_pane;
            workspace.terminal_id(pane_id).cloned()
        }) else {
            return;
        };
        let Some(runtime) = self.terminal_runtimes.get(&local_terminal) else {
            return;
        };
        if !runtime.apply_streamed_bytes(bytes) {
            tracing::debug!(
                target = %target,
                terminal_id,
                "dropped a mirror frame for a pane that is not streamed"
            );
        }
    }

    /// One mirrored terminal has gone on its host.
    ///
    /// The rest of that host's mirrors are unaffected: they share a connection,
    /// not a fate.
    pub(crate) fn handle_mirror_terminal_ended(
        &mut self,
        target: &str,
        terminal_id: &str,
        reason: Option<&str>,
    ) {
        let Some(ws_idx) = self.mirror_index_for_terminal(target, terminal_id) else {
            return;
        };
        tracing::debug!(target = %target, terminal_id, reason, "a mirrored terminal ended");
        if self.state.keeps_offline_mirrors() {
            if let Some(mirror) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|workspace| workspace.remote_mirror.as_mut())
            {
                mirror.disconnected = true;
                return;
            }
        }
        self.close_mirror_at(ws_idx);
        self.shutdown_detached_terminal_runtimes();
    }

    /// Sends something typed into a mirror back to the host that owns it.
    ///
    /// The writable connection is opened on the first keystroke rather than
    /// with the mirror: most mirrored panes are watched and never typed into,
    /// and a claim held for one of those is a claim nobody else can have.
    pub(crate) fn send_mirror_request(
        &mut self,
        target: &str,
        terminal_id: &str,
        request: &crate::pane::StreamedPaneRequest,
    ) {
        // Watching claims nothing: a size is only worth sending once this
        // machine already holds the terminal, and claiming one because a pane
        // was laid out would take it from whoever is actually typing.
        if !self.mirror_controls.contains_key(target)
            && matches!(request, crate::pane::StreamedPaneRequest::Resize { .. })
        {
            return;
        }
        if !self.mirror_controls.contains_key(target) {
            let Some(space) = self
                .config_remote_spaces()
                .into_iter()
                .find(|space| space.target == target)
            else {
                return;
            };
            let Some(remote_herdr) = self.mirror_remote_herdr.get(target).cloned() else {
                tracing::debug!(target, "no host binary known yet; dropping mirror input");
                return;
            };
            match crate::remote::mirror_stream::MirrorControl::spawn(
                &space,
                terminal_id,
                &remote_herdr,
            ) {
                Ok(control) => {
                    tracing::info!(target, terminal_id, "claimed a mirrored terminal");
                    self.mirror_controls.insert(target.to_owned(), control);
                }
                Err(err) => {
                    tracing::warn!(target, %err, "could not open a writable mirror connection");
                    return;
                }
            }
        }
        let Some(control) = self.mirror_controls.get_mut(target) else {
            return;
        };
        if let Err(err) = control.send(terminal_id, request) {
            tracing::warn!(target, terminal_id, %err, "mirror input failed; reopening on the next one");
            if let Some(control) = self.mirror_controls.remove(target) {
                control.stop();
            }
        }
    }

    /// The whole connection to a host has closed.
    ///
    /// Every mirror it carried is now stale, and the next reconcile opens the
    /// connection again. Nothing is torn down here when offline mirrors are
    /// kept, which is the point of keeping them.
    pub(crate) fn handle_mirror_stream_closed(&mut self, target: &str, reason: Option<&str>) {
        tracing::info!(target = %target, reason, "mirror stream closed");
        if let Some(stream) = self.mirror_streams.get(target) {
            // A host too old for this prints its usage and exits. So does a
            // host that is asleep, or one whose server is restarting -- with a
            // different complaint, and that is the whole difference. Going by
            // "no frames arrived" alone put a host on the per-pane attach for
            // an hour because it was being deployed to at the time.
            let complaint = stream.complaint();
            if !stream.carried_a_frame()
                && crate::remote::mirror_stream::complaint_means_too_old(complaint.as_deref())
            {
                tracing::warn!(
                    target,
                    complaint,
                    "host is too old to stream its mirrors; falling back to one attach per pane"
                );
                self.mirror_multiplex_unsupported.insert(
                    target.to_owned(),
                    std::time::Instant::now() + Self::MULTIPLEX_RETRY_AFTER,
                );
            }
        }
        self.mirror_streams.remove(target);
        if let Some(control) = self.mirror_controls.remove(target) {
            control.stop();
        }
        self.defer_mirror_stream(target);
        if self.state.keeps_offline_mirrors() {
            for workspace in self.state.workspaces.iter_mut() {
                if let Some(mirror) = workspace.remote_mirror.as_mut() {
                    if mirror.target == target {
                        mirror.disconnected = true;
                    }
                }
            }
            return;
        }
        let stale: Vec<usize> = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| {
                workspace
                    .remote_mirror
                    .as_ref()
                    .is_some_and(|mirror| mirror.target == target)
            })
            .map(|(ws_idx, _)| ws_idx)
            .rev()
            .collect();
        let closed = !stale.is_empty();
        for ws_idx in stale {
            self.close_mirror_at(ws_idx);
        }
        if closed {
            self.shutdown_detached_terminal_runtimes();
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

    /// Creates a mirror fed by the host's shared connection.
    ///
    /// Nothing is spawned: no ssh, no remote process, no claim on the remote
    /// terminal. The pane is a terminal parser waiting for frames.
    fn create_streamed_mirror(&mut self, mirror: RemoteMirror, label: &str) -> std::io::Result<()> {
        let (rows, cols) = self.state.estimate_pane_size();
        let cwd = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Input and resizes a mirror produces belong to the host. They are
        // tagged with the terminal they came from here, since the pane itself
        // only knows it is a pane.
        let (requests, mut request_rx) = tokio::sync::mpsc::channel(16);
        {
            let events = self.event_tx.clone();
            let target = mirror.target.clone();
            let Some((_, remote_terminal)) =
                crate::remote::spaces::RemoteAgentPane::split_key(&mirror.key)
            else {
                return Err(std::io::Error::other("mirror key has no terminal id"));
            };
            let terminal_id = remote_terminal.to_owned();
            tokio::spawn(async move {
                while let Some(request) = request_rx.recv().await {
                    if events
                        .send(crate::events::AppEvent::MirrorRequest {
                            target: target.clone(),
                            terminal_id: terminal_id.clone(),
                            request,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        let (mut workspace, terminal, runtime) = Workspace::new_streamed_mirror(
            cwd,
            rows,
            cols,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
            requests,
        )?;
        workspace.custom_name = Some(label.to_string());
        workspace.remote_mirror = Some(mirror);
        // Which agent this is comes from the host, applied to every mirror by
        // `report_remote_agent_states` right after this. The attach path needs
        // `HERDR_AGENT` because its pane's foreground process is ssh and local
        // detection would otherwise see nothing; a streamed pane runs no
        // detection at all, so there is nothing to tell.

        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.workspaces.push(workspace);
        Ok(())
    }

    /// How long to wait before dialling a host again, after this many failures
    /// in a row.
    ///
    /// Doubling from two seconds and capped at a minute: long enough that a
    /// host which is simply away costs nothing, short enough that one which
    /// blinked is back within a poll.
    fn mirror_retry_delay(failures: u32) -> Duration {
        Duration::from_secs(2u64.saturating_pow(failures.clamp(1, 5)))
    }

    /// Notes that a host's connection failed, and when to try it again.
    fn defer_mirror_stream(&mut self, target: &str) {
        let failures = self
            .mirror_stream_retry
            .get(target)
            .map(|(_, failures)| failures.saturating_add(1))
            .unwrap_or(1);
        let delay = Self::mirror_retry_delay(failures);
        self.mirror_stream_retry.insert(
            target.to_owned(),
            (std::time::Instant::now() + delay, failures),
        );
        tracing::debug!(
            target,
            failures,
            seconds = delay.as_secs(),
            "waiting before opening this host's mirror connection again"
        );
    }

    /// How long a host that could not stream keeps being mirrored the old way
    /// before it is asked again. Deploys are rare; an hour of attach after one
    /// is cheaper than dialling a host that cannot answer every half minute.
    const MULTIPLEX_RETRY_AFTER: Duration = Duration::from_secs(3600);

    /// Whether this host's mirrors share one connection.
    ///
    /// Configured on, minus the hosts that have shown they cannot: the fleet
    /// runs mixed builds as a matter of course, so falling back per host is the
    /// normal case rather than a failure.
    fn mirrors_are_multiplexed(&self, target: &str) -> bool {
        if !self.multiplexed_mirrors {
            return false;
        }
        match self.mirror_multiplex_unsupported.get(target) {
            // Asked again once in a while, because the answer changes: a host
            // is old until someone deploys to it, and noticing that should not
            // need this machine to be restarted.
            Some(retry_at) => std::time::Instant::now() >= *retry_at,
            None => true,
        }
    }

    /// Every terminal we currently mirror of one host, with the size to render
    /// each at.
    fn mirror_stream_targets(
        &self,
        target: &str,
    ) -> Vec<crate::remote::mirror_stream::MirrorStreamTarget> {
        let (rows, cols) = self.state.estimate_pane_size();
        self.state
            .workspaces
            .iter()
            .filter_map(|workspace| {
                let mirror = workspace.remote_mirror.as_ref()?;
                if mirror.target != target {
                    return None;
                }
                let (_, terminal_id) =
                    crate::remote::spaces::RemoteAgentPane::split_key(&mirror.key)?;
                Some(crate::remote::mirror_stream::MirrorStreamTarget {
                    terminal_id: terminal_id.to_owned(),
                    cols,
                    rows,
                })
            })
            .collect()
    }

    /// Opens the host's connection if it is not open, and tells it which
    /// terminals to send.
    fn update_mirror_stream(&mut self, space: &RemoteSpaceConfig, remote_herdr: &str) {
        let targets = self.mirror_stream_targets(&space.target);
        if targets.is_empty() {
            if let Some(stream) = self.mirror_streams.remove(&space.target) {
                stream.stop();
            }
            return;
        }
        if !self.mirror_streams.contains_key(&space.target) {
            if let Some((retry_at, _)) = self.mirror_stream_retry.get(&space.target) {
                if std::time::Instant::now() < *retry_at {
                    return;
                }
            }
            match crate::remote::mirror_stream::MirrorStream::spawn(
                space,
                remote_herdr,
                self.event_tx.clone(),
            ) {
                Ok(stream) => {
                    tracing::info!(target = %space.target, "opened a shared mirror connection");
                    self.mirror_streams.insert(space.target.clone(), stream);
                }
                Err(err) => {
                    tracing::warn!(target = %space.target, %err, "could not open a shared mirror connection");
                    self.defer_mirror_stream(&space.target);
                    return;
                }
            }
        }
        let Some(stream) = self.mirror_streams.get_mut(&space.target) else {
            return;
        };
        if stream.is_watching(&targets) {
            return;
        }
        let count = targets.len();
        if let Err(err) = stream.set_targets(targets) {
            tracing::warn!(target = %space.target, %err, "could not update a shared mirror connection");
            if let Some(stream) = self.mirror_streams.remove(&space.target) {
                stream.stop();
            }
            self.defer_mirror_stream(&space.target);
            return;
        }
        tracing::debug!(target = %space.target, count, "asked a host for its mirrored terminals");
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

    /// Reconcile runs on every snapshot a host sends rather than on a timer, so
    /// a host that is down must cost less each time rather than the same every
    /// time.
    #[test]
    fn dialling_a_host_that_is_down_backs_off_and_stops_growing() {
        let delays: Vec<u64> = (1..=7)
            .map(|failures| App::mirror_retry_delay(failures).as_secs())
            .collect();
        assert_eq!(delays, vec![2, 4, 8, 16, 32, 32, 32]);
        assert_eq!(
            App::mirror_retry_delay(0).as_secs(),
            2,
            "the first failure still waits"
        );
    }

    /// A host that is answering must not be carrying a grudge from an earlier
    /// blip: the next failure starts counting from nothing.
    #[tokio::test]
    async fn a_frame_clears_the_backoff_for_that_host() {
        let mut app = crate::app::tests::test_app();
        app.multiplexed_mirrors = true;
        app.mirror_stream_retry.insert(
            "workbox".to_string(),
            (std::time::Instant::now() + Duration::from_secs(30), 4),
        );

        // No mirror of this terminal exists, which is enough: the point is that
        // hearing from the host at all is what clears it.
        app.apply_mirror_frame("workbox", "term-1", b"hello");

        assert!(!app.mirror_stream_retry.contains_key("workbox"));
    }

    /// The fleet runs mixed builds as a matter of course -- two of its machines
    /// sleep for days -- so a host that cannot stream many terminals at once
    /// must go back to an attach per pane rather than lose its mirrors.
    #[tokio::test]
    async fn a_host_that_streams_nothing_goes_back_to_one_attach_per_pane() {
        let mut app = crate::app::tests::test_app();
        app.multiplexed_mirrors = true;
        app.mirror_streams.insert(
            "sleepy".to_string(),
            crate::remote::mirror_stream::MirrorStream::test_without_a_host(
                0,
                Some("usage: herdr terminal session observe <target>"),
            ),
        );
        assert!(app.mirrors_are_multiplexed("sleepy"));

        app.handle_mirror_stream_closed("sleepy", None);

        assert!(
            !app.mirrors_are_multiplexed("sleepy"),
            "a host that said nothing should be mirrored the old way"
        );
        assert!(app.mirror_streams.is_empty());

        // A host is only old until someone deploys to it, and noticing that
        // should not need this machine to be restarted.
        app.mirror_multiplex_unsupported
            .insert("sleepy".to_string(), std::time::Instant::now());
        assert!(
            app.mirrors_are_multiplexed("sleepy"),
            "once the wait is up the host is asked again"
        );
    }

    /// A connection that was working and then dropped says nothing about what
    /// the host can do, and must not cost it the shared connection for ever.
    #[tokio::test]
    async fn a_host_whose_connection_merely_dropped_keeps_the_shared_one() {
        let mut app = crate::app::tests::test_app();
        app.multiplexed_mirrors = true;
        app.mirror_streams.insert(
            "flaky".to_string(),
            crate::remote::mirror_stream::MirrorStream::test_without_a_host(42, None),
        );

        app.handle_mirror_stream_closed("flaky", Some("connection reset"));

        assert!(
            app.mirrors_are_multiplexed("flaky"),
            "a dropped connection is not a host that cannot do this"
        );
    }
    /// Deploying to a host means restarting its server, and a connection
    /// opened in that window carries no frames either. This cost lute an hour
    /// on the per-pane attach for the crime of being deployed to.
    #[tokio::test]
    async fn a_host_that_was_only_restarting_keeps_the_shared_one() {
        let mut app = crate::app::tests::test_app();
        app.multiplexed_mirrors = true;
        app.mirror_streams.insert(
            "lute".to_string(),
            crate::remote::mirror_stream::MirrorStream::test_without_a_host(
                0,
                Some("Connection to lute closed by remote host."),
            ),
        );

        app.handle_mirror_stream_closed("lute", None);

        assert!(
            app.mirrors_are_multiplexed("lute"),
            "a host that was mid-handoff is asked again, not written off"
        );
        assert!(
            app.mirror_stream_retry.contains_key("lute"),
            "and it waits before dialling back"
        );
    }

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
            &Default::default(),
            &Default::default(),
            &Default::default(),
        )
    }

    fn snapshot(panes: Vec<RemoteAgentPane>) -> RemoteSpaceSnapshot {
        RemoteSpaceSnapshot {
            remote_herdr: "/usr/bin/herdr".into(),
            panes,
            shell_panes: Vec::new(),
            extra_panes: Vec::new(),
        }
    }

    /// A snapshot carrying agent-less panes that stand for nothing on their own:
    /// the second and later panes of a space, and shells beside an agent.
    fn snapshot_with_extras(
        panes: Vec<RemoteAgentPane>,
        extra_panes: Vec<RemoteAgentPane>,
    ) -> RemoteSpaceSnapshot {
        RemoteSpaceSnapshot {
            remote_herdr: "/usr/bin/herdr".into(),
            panes,
            shell_panes: Vec::new(),
            extra_panes,
        }
    }

    fn shell_pane(workspace_id: &str, label: &str, terminal_id: &str) -> RemoteAgentPane {
        RemoteAgentPane {
            agent: None,
            ..agent_pane(workspace_id, label, terminal_id)
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
            extra_panes: Vec::new(),
        }
    }

    fn agent_pane(workspace_id: &str, label: &str, terminal_id: &str) -> RemoteAgentPane {
        RemoteAgentPane {
            terminal_id: terminal_id.into(),
            workspace_id: workspace_id.into(),
            workspace_label: label.into(),
            agent: Some("claude".into()),
            status: crate::api::schema::AgentStatus::Idle,
            origin: None,
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

    /// A host says "done" -- idle with output nobody has read -- on every poll
    /// until someone reads it *there*. Reading the mirror is local and the host
    /// never hears about it, so re-applying its answer would undo the read a
    /// poll later. This is the bug that made a mirror marked read pop back to
    /// unread with nothing having changed.
    #[test]
    fn reading_a_mirror_survives_the_host_repeating_itself() {
        let space = space("workbox");
        let mut app = crate::app::tests::test_app();
        let key = {
            let pane = agent_pane("w1", "api", "term-1");
            pane.mirror_key(&space.target)
        };
        let mut workspace = Workspace::test_new("api");
        workspace.remote_mirror = Some(remote_mirror_record(&space, &key));
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces.push(workspace);

        let done_at = |changed_at: u64| {
            let mut pane = agent_pane("w1", "api", "term-1");
            pane.status = crate::api::schema::AgentStatus::Done;
            pane.state_changed_at_ms = Some(changed_at);
            snapshot(vec![pane])
        };
        let seen = |app: &App| app.state.workspaces[0].tabs[0].panes[&pane_id].seen;

        // First sight of this output: unread, as the host says.
        app.report_remote_agent_states(&space, &done_at(1_000));
        assert!(!seen(&app), "new output should arrive unread");

        // The user reads it here. The host is none the wiser and keeps saying
        // "done", so the next poll must leave the mark alone.
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .expect("pane")
            .seen = true;
        app.report_remote_agent_states(&space, &done_at(1_000));
        assert!(seen(&app), "an unchanged host must not undo a local read");

        // Output the reader has not seen is a change, and marks it unread again.
        app.report_remote_agent_states(&space, &done_at(2_000));
        assert!(!seen(&app), "newer remote output should mark it unread");
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

    fn mirrored_pane(
        workspace_id: &str,
        label: &str,
        terminal_id: &str,
        origin_target: &str,
        origin_terminal: &str,
    ) -> RemoteAgentPane {
        RemoteAgentPane {
            origin: Some(crate::remote::spaces::MirrorOrigin {
                target: origin_target.into(),
                terminal_id: origin_terminal.into(),
                label: None,
                color: None,
            }),
            ..agent_pane(workspace_id, label, terminal_id)
        }
    }

    /// Mirroring two hosts that also mirror each other must not show their
    /// agents twice. The copy that comes straight from the machine running it
    /// is the one to keep: its own colour and label, and one ssh hop not two.
    #[test]
    fn plan_ignores_a_reflection_of_a_host_we_mirror_ourselves() {
        // workbox reports one of its own agents and one it mirrors from sera,
        // spelled with a login name we do not use.
        let snapshot = snapshot(vec![
            agent_pane("w1", "api", "term-local"),
            mirrored_pane("w2", "notes", "term-hop", "ryi@sera", "term-far"),
        ]);
        let mirrored_hosts =
            std::collections::HashSet::from(["workbox".to_string(), "sera".to_string()]);

        let plan = super::plan_remote_mirrors(
            &[],
            &space("workbox"),
            &snapshot,
            &Default::default(),
            &Default::default(),
            &mirrored_hosts,
            &Default::default(),
            &Default::default(),
        );

        assert_eq!(
            plan.iter()
                .filter_map(|action| match action {
                    MirrorAction::Create { key, .. } => Some(key.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![key_for("workbox", "w1", "term-local")]
        );
    }

    /// A pane reached through another host belongs to the machine running it,
    /// and must say so. Showing it under the hop reads as though one machine is
    /// running everything, and the agent is not there at all.
    #[test]
    fn a_reflection_is_shown_under_the_host_that_really_runs_it() {
        let mut pane = mirrored_pane("w2", "notes", "term-hop", "ryielle@valkyrie", "term-far");
        let origin = pane.origin.as_mut().expect("origin");
        origin.label = Some("val".into());
        origin.color = Some("#cba6f7".into());
        let origin = pane.origin.clone().expect("origin");

        let record = remote_mirror_record_for_origin(
            &space("pandora"),
            &key_for("pandora", "w2", "term-hop"),
            &origin,
        );

        // Named and coloured as valkyrie...
        assert_eq!(record.host_label, "val");
        assert_eq!(record.host_color.as_deref(), Some("#cba6f7"));
        // ...but still reconciled against the host we actually poll.
        assert_eq!(record.target, "pandora");
        assert_eq!(record.key, key_for("pandora", "w2", "term-hop"));
    }

    /// A host too old to pass on its labels still should not lend its own name
    /// to someone else's pane. The target names the machine; use that.
    #[test]
    fn a_reflection_without_a_label_falls_back_to_the_origin_host_name() {
        let pane = mirrored_pane("w2", "notes", "term-hop", "ryielle@valkyrie", "term-far");
        let origin = pane.origin.clone().expect("origin");

        let record = remote_mirror_record_for_origin(&space("pandora"), "k", &origin);

        assert_eq!(record.host_label, "valkyrie");
        // Left unset so the sidebar derives its usual per-host colour.
        assert_eq!(record.host_color, None);
    }

    /// Two machines mirroring each other is an ordinary thing to want, and it
    /// used to mean seeing your own agents twice: the other host reports your
    /// panes back to you as its mirrors, wearing its name. A pane reflecting a
    /// terminal this machine owns is us, however the other host addresses us.
    #[test]
    fn plan_drops_a_reflection_of_one_of_our_own_terminals() {
        let snapshot = snapshot(vec![
            mirrored_pane("w2", "notes", "term-hop", "ryi@pandora", "term-mine"),
            mirrored_pane("w3", "api", "term-hop2", "ryi@sera", "term-theirs"),
        ]);
        let local_terminals = std::collections::HashSet::from(["term-mine".to_string()]);

        let plan = super::plan_remote_mirrors(
            &[],
            &space("lute"),
            &snapshot,
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &local_terminals,
            &Default::default(),
        );

        // Only sera's agent is worth mirroring; ours is already right here.
        let created: Vec<&str> = plan
            .iter()
            .filter_map(|action| match action {
                MirrorAction::Create { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(created, ["api"]);
    }

    /// The same reflection when we do not mirror its host: keeping it is the
    /// only way that agent reaches this sidebar at all.
    #[test]
    fn plan_keeps_a_reflection_of_a_host_we_cannot_reach() {
        let snapshot = snapshot(vec![mirrored_pane(
            "w2", "notes", "term-hop", "ryi@sera", "term-far",
        )]);
        let mirrored_hosts = std::collections::HashSet::from(["workbox".to_string()]);

        let plan = super::plan_remote_mirrors(
            &[],
            &space("workbox"),
            &snapshot,
            &Default::default(),
            &Default::default(),
            &mirrored_hosts,
            &Default::default(),
            &Default::default(),
        );

        assert!(
            plan.iter().any(|action| matches!(
                action,
                MirrorAction::Create { key, .. } if key == &key_for("workbox", "w2", "term-hop")
            )),
            "{plan:?}"
        );
    }

    /// A tab the user asked for on a mirrored space becomes another mirror of
    /// that space, beside the one it was asked from. It is pinned by pane, not
    /// by space, so nothing else living in that remote space comes with it.
    #[test]
    fn plan_mirrors_a_pinned_pane_from_an_already_mirrored_space() {
        let snapshot = snapshot_with_extras(
            vec![agent_pane("w1", "api", "term-1")],
            vec![
                shell_pane("w1", "api", "term-2"),
                shell_pane("w1", "api", "term-unwanted"),
            ],
        );
        let pinned_panes = std::collections::HashSet::from(["term-2".to_string()]);
        let mut workspaces = vec![mirror(
            "workbox",
            &key_for("workbox", "w1", "term-1"),
            "api",
        )];

        let plan = super::plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot,
            &Default::default(),
            &pinned_panes,
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );

        // Two mirrors of one remote space now, so the shared label is
        // disambiguated and the existing mirror is renamed to match.
        assert_eq!(
            plan,
            vec![
                MirrorAction::Rename {
                    ws_idx: 0,
                    label: "api 1".into(),
                },
                MirrorAction::Create {
                    key: key_for("workbox", "w1", "term-2"),
                    label: "api 2".into(),
                    argv: attach_argv(
                        &space("workbox"),
                        &shell_pane("w1", "api", "term-2"),
                        "/usr/bin/herdr"
                    ),
                    agent: None,
                    origin: None,
                },
            ]
        );

        // And the next poll leaves it alone rather than closing it again.
        for action in &plan {
            if let MirrorAction::Rename { ws_idx, label } = action {
                workspaces[*ws_idx].custom_name = Some(label.clone());
            }
        }
        apply_creates(&mut workspaces, &space("workbox"), &plan);
        let plan = super::plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot,
            &Default::default(),
            &pinned_panes,
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(plan, Vec::new());
    }

    /// The panes nobody asked for stay out of the sidebar. A remote space can
    /// hold any number of plain shells, and mirroring them all would bury the
    /// agents the sidebar exists for.
    #[test]
    fn plan_ignores_unpinned_panes_from_a_mirrored_space() {
        let snapshot = snapshot_with_extras(
            vec![agent_pane("w1", "api", "term-1")],
            vec![shell_pane("w1", "api", "term-2")],
        );
        let workspaces = vec![mirror(
            "workbox",
            &key_for("workbox", "w1", "term-1"),
            "api",
        )];

        let plan = super::plan_remote_mirrors(
            &workspaces,
            &space("workbox"),
            &snapshot,
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );

        assert_eq!(plan, Vec::new());
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
            &Default::default(),
            &Default::default(),
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
                origin: None,
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
            &Default::default(),
            &Default::default(),
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
            &Default::default(),
            &Default::default(),
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
                    origin: None,
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
