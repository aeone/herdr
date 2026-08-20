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

/// The size of every mirror pane when the hosts were last told.
///
/// Compared on every render to decide whether any host needs telling again, so
/// it holds sizes in workspace order and nothing else: no names, no allocation
/// beyond the one vector.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MirrorLayoutStamp {
    /// Whether anything has been laid out, since nothing laid out means every
    /// size is still a guess and none of them may be asked for as a resize.
    laid_out: bool,
    /// How many terminals someone is looking at. A count is enough: what
    /// matters is noticing the answer moved, so the hosts are told again.
    watched: usize,
    sizes: Vec<(u16, u16)>,
}

/// One change reconcile wants to make to the local workspace list.
#[derive(Debug, Clone, PartialEq, Eq)]
// Creating carries everything needed to build a mirror and the other two carry
// an index, so the variants are lopsided by nature. The plan is a short-lived
// list built once per reconcile, so boxing would cost an allocation per mirror
// to save nothing that matters.
#[allow(clippy::large_enum_variant)]
pub(crate) enum MirrorAction {
    Create {
        key: String,
        /// The terminal id the polled host knows this pane by, which is not the
        /// key when the pane reached us through another host.
        remote_terminal: String,
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
                    remote_terminal: pane.terminal_id.clone(),
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
pub(crate) fn remote_mirror_record(
    space: &RemoteSpaceConfig,
    key: &str,
    remote_terminal: &str,
) -> RemoteMirror {
    RemoteMirror {
        disconnected: false,
        target: space.target.clone(),
        origin_target: None,
        remote_terminal: remote_terminal.to_string(),
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
    remote_terminal: &str,
    origin: &crate::remote::spaces::MirrorOrigin,
) -> RemoteMirror {
    RemoteMirror {
        origin_target: Some(origin.target.clone()),
        host_label: origin
            .label
            .clone()
            .unwrap_or_else(|| crate::remote::spaces::MirrorOrigin::host_key(&origin.target)),
        host_color: origin.color.clone(),
        ..remote_mirror_record(space, key, remote_terminal)
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
    /// The hosts this session mirrors: what config lists, unless mirroring has
    /// been switched off, in which case there are none. Every path that looks
    /// at configured hosts goes through here, so turning mirroring off is the
    /// same event as config dropping every host -- workers stop, mirrors close,
    /// and nothing dials out again until it is turned back on.
    pub(super) fn config_remote_spaces(&self) -> Vec<RemoteSpaceConfig> {
        self.remote_spaces
            .iter()
            .filter(|space| self.state.mirrors_host(&space.target))
            .cloned()
            .collect()
    }

    /// Switches one host's mirrors on or off, tearing down or restarting
    /// everything that implies. Returns whether it moved.
    pub(crate) fn set_host_mirrors_enabled(&mut self, target: &str, enabled: bool) -> bool {
        if self.state.mirrors_host(target) == enabled {
            return false;
        }
        if enabled {
            self.state.mirrors_off.remove(target);
        } else {
            self.state.mirrors_off.insert(target.to_owned());
        }
        if enabled {
            // Workers are started by the ordinary poll pass; bring it forward
            // so the sidebar repopulates now rather than at the next tick.
            self.start_remote_space_polls_if_due(std::time::Instant::now());
        } else {
            self.stop_unconfigured_remote_space_workers();
            self.close_mirrors_for_unconfigured_hosts();
            if let Some(stream) = self.mirror_streams.remove(target) {
                stream.stop();
            }
            if let Some(control) = self.mirror_controls.remove(target) {
                control.stop();
            }
            self.mirror_remote_herdr.remove(target);
            self.mirror_stream_retry.remove(target);
            // What a host can do is a fact about that host, so
            // `mirror_multiplex_unsupported` is left alone: switching back on
            // should not make us re-probe a build we already know is too old.
            self.state.remote_offline_hosts.remove(target);
        }
        true
    }

    /// Switches every configured host at once. Returns whether anything moved.
    pub(crate) fn set_all_mirrors_enabled(&mut self, enabled: bool) -> bool {
        let targets: Vec<String> = self
            .remote_spaces
            .iter()
            .map(|space| space.target.clone())
            .collect();
        let mut moved = false;
        for target in targets {
            moved |= self.set_host_mirrors_enabled(&target, enabled);
        }
        moved
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
        let configured = self.config_remote_spaces();
        self.remote_space_workers.retain(|target, worker| {
            let keep = configured.iter().any(|space| space.target == *target);
            if !keep {
                worker
                    .stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            keep
        });
    }

    /// Drops mirrors for hosts this session no longer mirrors. Called on config
    /// reload and when mirroring is switched off, where the dropped host will
    /// never be polled again.
    pub(crate) fn close_mirrors_for_unconfigured_hosts(&mut self) {
        let stale =
            mirrors_for_unconfigured_hosts(&self.state.workspaces, &self.config_remote_spaces());
        if stale.is_empty() {
            return;
        }
        for ws_idx in stale {
            self.close_mirror_at(ws_idx);
        }
        self.shutdown_detached_terminal_runtimes();
        self.release_controls_for_closed_mirrors();
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
        let mirror = remote_mirror_record(&space, &key, &created.pane.terminal_id);
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
        //
        // Every configured host, including one switched off: switching a host
        // off means its panes are not wanted, not that they should start
        // arriving the long way round. Filtering here took the host out of this
        // set, so the reflection reaching us through another host stopped being
        // recognised as its own and was kept -- switching a host off is what
        // made its panes appear, two hops away and wearing the other machine's
        // spelling of them.
        let mirrored_hosts: std::collections::HashSet<String> = self
            .remote_spaces
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
                    remote_terminal,
                    label,
                    argv,
                    agent,
                    origin,
                } => {
                    let mirror = match &origin {
                        Some(origin) => {
                            remote_mirror_record_for_origin(space, &key, &remote_terminal, origin)
                        }
                        None => remote_mirror_record(space, &key, &remote_terminal),
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
            // A handoff on the host rebuilds every mirror under new terminal
            // ids, so a claim held across one is left attached to a terminal
            // this side no longer shows.
            self.release_controls_for_closed_mirrors();
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
                mirror.target == target && mirror.remote_terminal == terminal_id
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
        if !self.claim_mirror_control(target, terminal_id) {
            return;
        }
        if let Err(err) = self.send_on_mirror_control(target, terminal_id, request) {
            // A control connection dies on the far side without warning -- the
            // host restarts, the terminal goes, ssh drops -- and the first this
            // side hears of it is a broken pipe on the way out. Dropping the
            // keystroke and waiting for the next one turned that into a claim
            // per keypress, each one a fresh attach on a terminal the host then
            // resized to match, so typing into a mirror after any hiccup left
            // the far side reflowing. One clean retry sends the keystroke that
            // found the fault rather than eating it.
            tracing::debug!(target, terminal_id, %err, "mirror input failed; claiming again");
            if !self.claim_mirror_control(target, terminal_id) {
                return;
            }
            if let Err(err) = self.send_on_mirror_control(target, terminal_id, request) {
                tracing::warn!(target, terminal_id, %err, "mirror input failed twice; giving up on it");
                self.release_mirror_control(target);
            }
        }
    }

    /// Ensures this machine holds `target`'s writable connection, opening one if
    /// it does not. Returns whether it now holds a usable one.
    fn claim_mirror_control(&mut self, target: &str, terminal_id: &str) -> bool {
        if self.mirror_controls.contains_key(target) {
            return true;
        }
        let Some(space) = self
            .config_remote_spaces()
            .into_iter()
            .find(|space| space.target == target)
        else {
            return false;
        };
        let Some(remote_herdr) = self.mirror_remote_herdr.get(target).cloned() else {
            tracing::debug!(target, "no host binary known yet; dropping mirror input");
            return false;
        };
        let size = self.mirror_pane_size_for(target, terminal_id);
        match crate::remote::mirror_stream::MirrorControl::spawn(
            &space,
            terminal_id,
            &remote_herdr,
            size,
        ) {
            Ok(control) => {
                tracing::info!(target, terminal_id, "claimed a mirrored terminal");
                self.mirror_controls.insert(target.to_owned(), control);
                true
            }
            Err(err) => {
                tracing::warn!(target, %err, "could not open a writable mirror connection");
                false
            }
        }
    }

    /// Sends one request on the held connection, sizing the terminal first when
    /// the claim is landing on it for the first time.
    ///
    /// A control connection is an attach, and a host sizes a terminal to its
    /// attach client -- so claiming one would otherwise resize it to whatever
    /// pty that ssh happened to get, undoing the size the mirror pane actually
    /// needs. The first thing a claim says is how big the pane is, and the same
    /// goes every time it moves to another pane.
    ///
    /// Any failure drops the connection, because a half-written one is worse
    /// than none: the next attempt would size a terminal it no longer holds.
    fn send_on_mirror_control(
        &mut self,
        target: &str,
        terminal_id: &str,
        request: &crate::pane::StreamedPaneRequest,
    ) -> std::io::Result<()> {
        let landing = self
            .mirror_controls
            .get(target)
            .is_some_and(|control| control.controlling() != terminal_id);
        let size = landing
            .then(|| self.mirror_pane_size_for(target, terminal_id))
            .flatten();
        let Some(control) = self.mirror_controls.get_mut(target) else {
            return Err(std::io::Error::other("no writable mirror connection"));
        };
        let result = size
            .map(|(rows, cols)| {
                control.send(
                    terminal_id,
                    &crate::pane::StreamedPaneRequest::Resize {
                        rows,
                        cols,
                        cell_width_px: 0,
                        cell_height_px: 0,
                    },
                )
            })
            .unwrap_or(Ok(()))
            .and_then(|()| control.send(terminal_id, request));
        if result.is_err() {
            self.release_mirror_control(target);
        }
        result
    }

    /// Lets go of a host's writable connection, if it holds one.
    fn release_mirror_control(&mut self, target: &str) {
        if let Some(control) = self.mirror_controls.remove(target) {
            control.stop();
        }
    }

    /// Lets go of any writable connection whose terminal is no longer mirrored
    /// here.
    ///
    /// A claim outlives the pane that made it -- closing a mirror, or switching
    /// its host off, leaves the connection attached to a terminal with nothing
    /// on this side to show it. The host keeps sizing that terminal to an attach
    /// nobody is looking at.
    pub(crate) fn release_controls_for_closed_mirrors(&mut self) {
        let stale: Vec<String> = self
            .mirror_controls
            .iter()
            .filter(|(target, control)| {
                !self.state.workspaces.iter().any(|workspace| {
                    workspace.remote_mirror.as_ref().is_some_and(|mirror| {
                        mirror.target == **target && mirror.remote_terminal == control.controlling()
                    })
                })
            })
            .map(|(target, _)| target.clone())
            .collect();
        for target in stale {
            tracing::debug!(target, "letting go of a claim on a mirror that has closed");
            self.release_mirror_control(&target);
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
        // And the same for the space actually being worked in. Closing leaves
        // `active` pointing at whatever the selection became, which is right
        // when someone closes the space they are in and wrong every time a
        // mirror goes: it threw the focus out of the pane being typed in and
        // into whichever space had taken the mirror's place.
        let active_id = self
            .state
            .active
            .filter(|active| *active != ws_idx)
            .and_then(|active| self.state.workspaces.get(active))
            .map(|workspace| workspace.id.clone());
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
        if let Some(active_id) = active_id {
            if let Some(restored) = self
                .state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == active_id)
            {
                self.state.active = Some(restored);
            }
        }
        self.state.remove_plugin_pane_records(pane_ids);
    }

    /// Creates a mirror fed by the host's shared connection.
    ///
    /// Nothing is spawned: no ssh, no remote process, no claim on the remote
    /// terminal. The pane is a terminal parser waiting for frames.
    fn create_streamed_mirror(&mut self, mirror: RemoteMirror, label: &str) -> std::io::Result<()> {
        let host = mirror.target.clone();
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
            let terminal_id = mirror.remote_terminal.clone();
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
        // This pane is empty, and the host is sending differences against the
        // last frame it sent -- so it has to be asked for the set again, even
        // when the set has not changed, or an idle terminal never repaints and
        // the mirror stays blank.
        if let Some(stream) = self.mirror_streams.get_mut(&host) {
            stream.forget_targets();
        }
        Ok(())
    }

    /// Dials any host whose shared mirror connection is down but wanted.
    ///
    /// Runs on a clock of this machine's own, because everything else about a
    /// mirror is driven by what the host pushes. A host whose feed has wedged
    /// pushes nothing, and used to be left with a dead connection and blank
    /// mirrors for as long as it stayed wedged.
    pub(crate) fn retry_mirror_streams(&mut self) {
        if self.mirror_streams.is_empty() && self.mirror_remote_herdr.is_empty() {
            return;
        }
        for space in self.config_remote_spaces() {
            if !self.mirrors_are_multiplexed(&space.target) {
                continue;
            }
            // Hosts holding a live connection are not skipped: this also
            // catches a set that was forgotten because a pane was rebuilt, so
            // the frame a blank pane needs does not wait for the host to say
            // something first.
            if self.mirror_stream_targets(&space.target).is_empty() {
                continue;
            }
            let Some(remote_herdr) = self.mirror_remote_herdr.get(&space.target).cloned() else {
                continue;
            };
            self.update_mirror_stream(&space, &remote_herdr);
        }
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
    /// The terminals to ask a host for, each at the size its own mirror pane
    /// holds.
    ///
    /// The size comes from the local terminal the frames are written into, not
    /// from the laid-out view. Those must agree or the host renders a screen
    /// that does not fit where it lands, and the view is the wrong place to ask:
    /// it holds whichever attached client rendered last, and only ever for the
    /// workspace on screen. The local terminal always has a size, is resized by
    /// exactly the same layout pass, and changes only when the pane really does.
    fn mirror_stream_targets(
        &self,
        target: &str,
    ) -> Vec<crate::remote::mirror_stream::MirrorStreamTarget> {
        let (estimated_rows, estimated_cols) = self.state.estimate_pane_size();
        // Nothing laid out yet means every size here is still the guess a pane
        // is created with, and a guess must never be asked for as a resize:
        // after a handoff, mirrors are rebuilt before the first render, and
        // asking then shrank real terminals on the other machine and reflowed
        // their output before the layout arrived a moment later and corrected
        // it.
        let laid_out = !self.state.view.pane_infos.is_empty();
        self.state
            .workspaces
            .iter()
            .filter_map(|workspace| {
                let mirror = workspace.remote_mirror.as_ref()?;
                if mirror.target != target {
                    return None;
                }
                let (rows, cols) = self
                    .mirror_pane_size(workspace)
                    .unwrap_or((estimated_rows, estimated_cols));
                // Whether this machine may size the terminal behind the mirror:
                // only if someone is looking at the pane it lands in, here or
                // through us. A hub with nobody at it has no pane to measure
                // and must not impose its guess.
                let resize = laid_out
                    && workspace
                        .terminal_id(workspace.root_pane)
                        .is_some_and(|local| {
                            self.state
                                .watched_for_someone
                                .contains(local.to_string().as_str())
                        });
                Some(crate::remote::mirror_stream::MirrorStreamTarget {
                    terminal_id: mirror.remote_terminal.clone(),
                    cols,
                    rows,
                    resize,
                })
            })
            .collect()
    }

    /// The size of the pane a host's terminal is mirrored into, found by the
    /// name that host knows it by.
    fn mirror_pane_size_for(&self, target: &str, remote_terminal: &str) -> Option<(u16, u16)> {
        let workspace = self.state.workspaces.iter().find(|workspace| {
            workspace.remote_mirror.as_ref().is_some_and(|mirror| {
                mirror.target == target && mirror.remote_terminal == remote_terminal
            })
        })?;
        self.mirror_pane_size(workspace)
    }

    /// The size of the local terminal a mirror's frames are written into.
    fn mirror_pane_size(&self, workspace: &Workspace) -> Option<(u16, u16)> {
        let terminal_id = workspace.terminal_id(workspace.root_pane)?;
        Some(self.terminal_runtimes.get(terminal_id)?.current_size())
    }

    /// Tells every open connection the sizes its terminals are now drawn at.
    ///
    /// Called after a render rather than on a timer: activating a pane, zooming
    /// it, switching workspace or resizing the window all change what size a
    /// mirror is shown at, and a host that is not told goes on sending frames
    /// cut for the old one. Opens nothing and dials nothing -- a host that is
    /// not already connected is left to the poll.
    pub(crate) fn refresh_mirror_stream_sizes(&mut self) {
        if self.mirror_streams.is_empty() {
            return;
        }
        // This runs on every render pass, so it has to be cheap when nothing has
        // moved: one walk collecting sizes, no allocation per mirror, and the
        // work below only when that differs from what the hosts were told.
        let stamp = MirrorLayoutStamp {
            laid_out: !self.state.view.pane_infos.is_empty(),
            watched: self.state.watched_for_someone.len(),
            sizes: self
                .state
                .workspaces
                .iter()
                .filter(|workspace| workspace.remote_mirror.is_some())
                .map(|workspace| self.mirror_pane_size(workspace).unwrap_or((0, 0)))
                .collect(),
        };
        if self.mirror_layout_stamp == stamp {
            return;
        }
        self.mirror_layout_stamp = stamp;

        let hosts: Vec<String> = self.mirror_streams.keys().cloned().collect();
        for host in hosts {
            let targets = self.mirror_stream_targets(&host);
            let Some(stream) = self.mirror_streams.get_mut(&host) else {
                continue;
            };
            if targets.is_empty() || stream.is_watching(&targets) {
                continue;
            }
            if let Err(err) = stream.set_targets(targets) {
                tracing::warn!(target = %host, %err, "could not resize a shared mirror connection");
                if let Some(stream) = self.mirror_streams.remove(&host) {
                    stream.stop();
                }
                self.defer_mirror_stream(&host);
            }
        }
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
    /// The key names the machine running the pane; the host we poll has to be
    /// asked for the terminal *it* knows. Keying by origin without separating
    /// the two asked lute for alleria's terminal id, which lute does not have,
    /// so every mirror reached through another host came up blank.
    #[test]
    fn a_mirror_through_a_hop_is_asked_of_the_hop_by_the_hop_s_own_terminal() {
        let space = space("lute");
        let pane = mirrored_pane("w2", "notes", "term-on-lute", "ryielle@alleria", "term-far");
        let origin = pane.origin.clone().expect("origin");

        let record = super::remote_mirror_record_for_origin(
            &space,
            &pane.mirror_key(&space.target),
            &pane.terminal_id,
            &origin,
        );

        assert_eq!(
            record.remote_terminal, "term-on-lute",
            "lute is asked for the terminal lute knows"
        );
        assert!(
            record.key.contains("term-far"),
            "while the mirror is still identified by the pane alleria runs"
        );
    }

    /// The guard that drops a reflection of a host you already mirror matches
    /// the origin's name and its terminal id, so both have to describe the same
    /// machine. Naming the hop beside the origin's terminal id described no
    /// machine at all, and every hop then added another copy of the same agent
    /// -- fourteen more valkyrie panes on each machine every thirty seconds.
    #[test]
    fn a_mirror_reached_through_a_hop_reports_the_machine_that_runs_it() {
        let space = space("workbox");
        let origin = crate::remote::spaces::MirrorOrigin {
            target: "ryielle@valkyrie".to_string(),
            workspace_id: "w3".to_string(),
            terminal_id: "term-far".to_string(),
            label: Some("val".to_string()),
            color: None,
        };

        let record = super::remote_mirror_record_for_origin(&space, "key", "term-hop", &origin);

        assert_eq!(
            record.origin_target.as_deref(),
            Some("ryielle@valkyrie"),
            "the origin is the machine running the pane, not the host we asked"
        );
        assert_eq!(record.target, "workbox", "which is still the host we poll");

        // Heard first-hand, there is no hop and the two are the same machine.
        let direct = super::remote_mirror_record(&space, "key", "term-hop");
        assert_eq!(direct.origin_target, None);
    }

    /// A mirror going must not move the person using the machine. Closing sets
    /// `active` to whatever the selection became, which is right when someone
    /// closes the space they are in -- and threw the focus out of the pane
    /// being typed in every time a mirror was rebuilt.
    #[tokio::test]
    async fn a_mirror_closing_leaves_the_pane_being_worked_in_alone() {
        let mut app = crate::app::tests::test_app();
        app.state.workspaces.clear();
        let doomed = crate::workspace::Workspace::test_new("a mirror");
        let decoy = crate::workspace::Workspace::test_new("someone else");
        let mine = crate::workspace::Workspace::test_new("mine");
        let mine_id = mine.id.clone();
        // The mirror sits before the space being used, so closing it shifts
        // every index after it -- and a decoy sits in between, so an index that
        // merely stayed put would land somewhere wrong rather than by luck.
        app.state.workspaces.push(doomed);
        app.state.workspaces.push(decoy);
        app.state.workspaces.push(mine);
        app.state.active = Some(2);
        app.state.selected = 2;

        app.close_mirror_at(0);

        let active_id = app
            .state
            .active
            .and_then(|active| app.state.workspaces.get(active))
            .map(|workspace| workspace.id.clone());
        assert_eq!(
            active_id,
            Some(mine_id.clone()),
            "the space being worked in should still be the active one"
        );
        let selected_id = app
            .state
            .workspaces
            .get(app.state.selected)
            .map(|workspace| workspace.id.clone());
        assert_eq!(selected_id, Some(mine_id), "and still the selected one");
    }

    /// Switching mirroring off has to be the same event as config dropping
    /// every host: the panes go, the workers stop and the shared connections
    /// are let go. A switch that only stopped new mirrors appearing would leave
    /// the sidebar full of panes nothing was updating.
    #[tokio::test]
    async fn switching_mirroring_off_closes_what_is_mirrored_and_stops_dialling() {
        let mut app = crate::app::tests::test_app();
        app.remote_spaces = vec![space("workbox")];
        app.state.workspaces.clear();
        app.state.workspaces.push(local("mine"));
        app.state
            .workspaces
            .push(mirror("workbox", "workbox\u{1f}w1\u{1f}term-1", "remote"));
        app.state.active = Some(0);
        app.state.selected = 0;
        app.mirror_streams.insert(
            "workbox".to_string(),
            crate::remote::mirror_stream::MirrorStream::test_without_a_host(1, None),
        );
        app.mirror_remote_herdr
            .insert("workbox".to_string(), "/usr/bin/herdr".to_string());
        app.state.remote_offline_hosts.insert("workbox".to_string());

        assert!(
            app.set_host_mirrors_enabled("workbox", false),
            "the switch should move"
        );

        assert!(
            app.config_remote_spaces().is_empty(),
            "no host is mirrored while the switch is off"
        );
        assert!(
            app.state
                .workspaces
                .iter()
                .all(|workspace| workspace.remote_mirror.is_none()),
            "every mirror pane should have been closed"
        );
        assert!(app.mirror_streams.is_empty());
        assert!(app.mirror_controls.is_empty());
        assert!(app.mirror_remote_herdr.is_empty());
        assert!(
            app.state.remote_offline_hosts.is_empty(),
            "a host that is not mirrored is not offline, it is simply not asked"
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "the local space is left alone"
        );

        // And the host is configured again the moment it is switched back on,
        // so the ordinary poll pass repopulates without a config reload.
        assert!(app.set_host_mirrors_enabled("workbox", true));
        assert_eq!(app.config_remote_spaces().len(), 1);
        assert!(
            !app.set_host_mirrors_enabled("workbox", true),
            "switching it to where it already is changes nothing"
        );
    }

    /// A watcher mirroring several hosts at once, each with one mirror pane of
    /// its own size and a connection already carrying it. This is the smallest
    /// arrangement that can show one host's mirrors disturbing another's, which
    /// no single-host test can: every size, every observe set and every stamp
    /// here is global, so a change meant for one host passes through all of
    /// them on its way out.
    fn watcher_mirroring(hosts: &[(&str, u16, u16)]) -> App {
        let mut app = crate::app::tests::test_app();
        app.multiplexed_mirrors = true;
        app.remote_spaces = hosts.iter().map(|(target, ..)| space(target)).collect();
        app.state.workspaces.clear();
        app.state.active = Some(0);
        app.state.selected = 0;
        for (target, cols, rows) in hosts {
            let key = format!("{target}\u{1f}w1\u{1f}term-1");
            let workspace = mirror(target, &key, target);
            let terminal_id = workspace
                .terminal_id(workspace.root_pane)
                .expect("a mirror pane has a terminal")
                .clone();
            app.state.workspaces.push(workspace);
            app.terminal_runtimes.insert(
                terminal_id,
                crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                    *cols,
                    *rows,
                    1 << 16,
                    b"",
                ),
            );
        }
        // Connections that already carry exactly what this layout asks for, so
        // anything they are told afterwards is a change and not a first word.
        for (target, ..) in hosts {
            let targets = app.mirror_stream_targets(target);
            app.mirror_streams.insert(
                (*target).to_string(),
                crate::remote::mirror_stream::MirrorStream::test_watching(targets),
            );
            app.mirror_remote_herdr
                .insert((*target).to_string(), "/usr/bin/herdr".to_string());
        }
        app
    }

    /// What a host is told to watch is what decides whether it repaints: naming
    /// the set again makes it start from a whole frame, because a mirror pane
    /// rebuilt on this side cannot read differences against a frame it never
    /// saw. So switching one host off must not name any other host's set. It
    /// went wrong the other way round in use -- switching one host off in the
    /// keybind overlay left another host's mirror garbled -- and every size and
    /// stamp involved is shared between hosts, so nothing but a second host in
    /// the test can hold this.
    #[tokio::test]
    async fn switching_one_host_off_never_says_a_word_to_the_others() {
        let mut app = watcher_mirroring(&[("workbox", 100, 40), ("keeper", 90, 30)]);
        let before = app.mirror_stream_targets("keeper");
        assert_eq!(before.len(), 1, "the other host is mirrored to begin with");

        assert!(app.set_host_mirrors_enabled("workbox", false));

        assert_eq!(
            app.mirror_stream_targets("keeper"),
            before,
            "the other host is still watched at the same size"
        );
        // Every render refreshes sizes, and the host that stayed on must fall
        // out of that walk untouched however many times it runs.
        for _ in 0..3 {
            app.refresh_mirror_stream_sizes();
        }
        let keeper = app
            .mirror_streams
            .get("keeper")
            .expect("the other host is still connected");
        assert!(
            keeper.told().is_empty(),
            "the host that was left on should never have been told again, but was: {:?}",
            keeper.told()
        );
    }

    /// Switching a host off has to stay off. Its feed can still be mid-flight
    /// when the switch moves, and a snapshot applied after the fact would
    /// rebuild every mirror that was just closed -- then the next pass would
    /// close them again, which is what flickering is.
    #[tokio::test]
    async fn a_host_switched_off_is_not_rebuilt_by_a_poll_still_in_flight() {
        let mut app = watcher_mirroring(&[("workbox", 100, 40), ("keeper", 90, 30)]);

        assert!(app.set_host_mirrors_enabled("workbox", false));

        for _ in 0..3 {
            app.handle_remote_spaces_polled(
                "workbox".to_string(),
                Ok(snapshot(vec![agent_pane("w1", "remote", "term-1")])),
            );
            app.refresh_mirror_stream_sizes();
            assert!(
                !app.state.workspaces.iter().any(|workspace| workspace
                    .remote_mirror
                    .as_ref()
                    .is_some_and(|mirror| mirror.target == "workbox")),
                "a host that is switched off should stay closed"
            );
        }
        assert!(!app.mirror_streams.contains_key("workbox"));
        assert!(!app.remote_space_workers.contains_key("workbox"));
        assert!(
            !app.state.remote_offline_hosts.contains("workbox"),
            "and it is not reported as unreachable either, it is simply not asked"
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "the other host keeps its mirror"
        );
    }

    /// A claim is an attach, and an attach sizes the terminal it lands on. So a
    /// claim that outlives the pane that made it leaves the host sizing a
    /// terminal for a viewer who is no longer there -- and the mirror on the
    /// far side reflows to fit nobody. Closing a mirror has to let it go.
    #[tokio::test]
    async fn a_claim_on_a_mirror_that_has_closed_is_let_go() {
        let mut app = watcher_mirroring(&[("workbox", 100, 40), ("keeper", 90, 30)]);
        app.mirror_controls.insert(
            "workbox".to_string(),
            crate::remote::mirror_stream::MirrorControl::test_already_dead("term-remote"),
        );
        app.mirror_controls.insert(
            "keeper".to_string(),
            crate::remote::mirror_stream::MirrorControl::test_already_dead("term-remote"),
        );

        // The host's own handoff rebuilds its mirrors under new terminal ids,
        // which is the common way a claim is orphaned without anyone asking.
        app.state.workspaces.retain(|workspace| {
            workspace
                .remote_mirror
                .as_ref()
                .is_none_or(|mirror| mirror.target != "workbox")
        });
        app.release_controls_for_closed_mirrors();

        assert!(
            !app.mirror_controls.contains_key("workbox"),
            "the claim on the closed mirror should have been let go"
        );
        assert!(
            app.mirror_controls.contains_key("keeper"),
            "and the claim on a mirror that is still open should be kept"
        );
    }

    /// A connection dies on the far side without warning and this side only
    /// learns of it on the way out. Keeping the dead one and dropping the
    /// keystroke made every following keypress claim afresh -- attach, size,
    /// fail, attach -- which is a resize storm on the host for as long as
    /// someone is typing. A failed write means the connection is gone.
    #[tokio::test]
    async fn a_write_that_finds_a_dead_claim_lets_go_of_it() {
        let mut app = watcher_mirroring(&[("workbox", 100, 40)]);
        app.mirror_controls.insert(
            "workbox".to_string(),
            crate::remote::mirror_stream::MirrorControl::test_already_dead("term-remote"),
        );
        // Nothing known about the host's binary, so the retry cannot dial out
        // and the test stays off the network.
        app.mirror_remote_herdr.clear();

        app.send_mirror_request(
            "workbox",
            "term-remote",
            &crate::pane::StreamedPaneRequest::Input(b"hello".to_vec().into()),
        );

        assert!(
            app.mirror_controls.is_empty(),
            "a claim that could not be written to is not a claim any more"
        );
    }

    /// Watching claims nothing, so laying a pane out must never take a terminal
    /// from whoever is typing into it elsewhere.
    #[tokio::test]
    async fn a_size_alone_never_claims_a_mirror() {
        let mut app = watcher_mirroring(&[("workbox", 100, 40)]);

        app.send_mirror_request(
            "workbox",
            "term-remote",
            &crate::pane::StreamedPaneRequest::Resize {
                rows: 40,
                cols: 100,
                cell_width_px: 0,
                cell_height_px: 0,
            },
        );

        assert!(
            app.mirror_controls.is_empty(),
            "a size on its own should not have opened a writable connection"
        );
    }

    /// Switching a host off must not be the thing that makes its panes appear.
    ///
    /// The fleet is a star: a leaf is mirrored by the hub, and this machine
    /// mirrors the hub, so every leaf pane arrives twice -- once straight from
    /// the leaf and once reflected through the hub. The reflection is dropped
    /// because the leaf is a host we mirror. Switching the leaf off took it out
    /// of that set, so the reflection stopped being recognised and came back:
    /// the same panes, two hops away, under the hub's spelling of them, popping
    /// in and out with the leaf's own sleep. Off means the machine's panes are
    /// not wanted, however they get here.
    #[tokio::test]
    async fn a_host_switched_off_stays_off_when_its_panes_arrive_through_another() {
        let mut app = crate::app::tests::test_app();
        app.remote_spaces = vec![space("hub"), space("leaf")];
        app.state.workspaces.clear();
        app.set_host_mirrors_enabled("leaf", false);

        // The hub reports one of its own panes and one it is mirroring for the
        // leaf, which is what a hub's snapshot looks like.
        let mut reflected = agent_pane("w9", "leaf work", "term-hop");
        reflected.origin = Some(crate::remote::spaces::MirrorOrigin {
            target: "leaf".to_string(),
            workspace_id: "w9".to_string(),
            terminal_id: "term-leaf".to_string(),
            label: Some("lf".to_string()),
            color: None,
        });
        let snapshot = snapshot(vec![agent_pane("w1", "hub work", "term-hub"), reflected]);

        app.reconcile_remote_mirrors(&space("hub"), &snapshot);

        let mirrored: Vec<Option<String>> = app
            .state
            .workspaces
            .iter()
            .filter_map(|workspace| workspace.remote_mirror.as_ref())
            .map(|mirror| mirror.origin_target.clone())
            .collect();
        assert_eq!(
            mirrored,
            vec![None],
            "only the hub's own pane should be mirrored; the switched-off host's \
             pane should not arrive through it"
        );
    }

    /// A host renders a mirror at the size it is given, so the size has to be
    /// the one the frames are written into. It used to be a guess -- the size of
    /// whichever pane happened to be first in the workspace on screen, measured
    /// to its outer edge -- so a host rendered a screen that did not fit the
    /// terminal receiving it. A screen written at the wrong width wraps wrongly,
    /// which is why such a mirror could not be read, could not be scrolled, and
    /// could not be recognised as a working agent until something happened to
    /// send the true size.
    #[tokio::test]
    async fn a_mirror_is_asked_for_at_the_size_of_the_terminal_it_lands_in() {
        let mut app = crate::app::tests::test_app();
        app.state.workspaces.clear();
        let mirrored = mirror("workbox", "workbox\u{1f}w1\u{1f}term-1", "remote");
        let terminal_id = mirrored
            .terminal_id(mirrored.root_pane)
            .expect("a mirror pane has a terminal")
            .clone();
        app.state.workspaces.push(mirrored);
        app.state.active = Some(0);
        app.terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(100, 40, 1 << 16, b""),
        );

        let targets = app.mirror_stream_targets("workbox");

        assert_eq!(targets.len(), 1);
        assert_eq!(
            (targets[0].cols, targets[0].rows),
            (100, 40),
            "the host should be asked for the size the frames are written into"
        );
    }

    /// The refresh runs on every render, so it must cost nothing when no mirror
    /// pane has changed size, and must fire when one has.
    #[tokio::test]
    async fn a_mirror_that_has_not_been_resized_does_not_retell_the_hosts() {
        let mut app = crate::app::tests::test_app();
        app.state.workspaces.clear();
        let mirrored = mirror("workbox", "workbox\u{1f}w1\u{1f}term-1", "remote");
        let terminal_id = mirrored
            .terminal_id(mirrored.root_pane)
            .expect("a mirror pane has a terminal")
            .clone();
        app.state.workspaces.push(mirrored);
        app.state.active = Some(0);
        app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(80, 24, 1 << 16, b""),
        );
        // A connection to some other host, so there is work to skip rather than
        // an empty map that would short-circuit before the stamp is taken.
        app.mirror_streams.insert(
            "elsewhere".to_string(),
            crate::remote::mirror_stream::MirrorStream::test_without_a_host(1, None),
        );

        app.refresh_mirror_stream_sizes();
        let settled = MirrorLayoutStamp {
            laid_out: false,
            watched: 0,
            sizes: vec![(24, 80)],
        };
        assert_eq!(app.mirror_layout_stamp, settled);

        // Unchanged: the stamp is what stops the walk, so it must still match.
        app.refresh_mirror_stream_sizes();
        assert_eq!(app.mirror_layout_stamp, settled);

        // Zooming or activating the pane resizes the terminal the frames land
        // in, and that is exactly what the host has to be told about.
        app.terminal_runtimes
            .get(&terminal_id)
            .expect("the mirror's terminal")
            .resize(50, 120, 0, 0);
        app.refresh_mirror_stream_sizes();
        assert_eq!(
            app.mirror_layout_stamp,
            MirrorLayoutStamp {
                laid_out: false,
                watched: 0,
                sizes: vec![(50, 120)],
            }
        );
    }

    /// A control connection is an attach, and a host sizes a terminal to its
    /// attach client. So claiming a mirror to type into it resized the terminal
    /// to whatever pty that ssh happened to get -- a 70 row pane typed into
    /// became a 40 row terminal on the other machine, which is the size the
    /// watching path had just been fixed to get right.
    #[tokio::test]
    async fn claiming_a_mirror_to_type_into_it_tells_the_host_how_big_the_pane_is() {
        let mut app = crate::app::tests::test_app();
        app.state.workspaces.clear();
        let mirrored = mirror("workbox", "workbox\u{1f}w1\u{1f}term-1", "remote");
        let terminal_id = mirrored
            .terminal_id(mirrored.root_pane)
            .expect("a mirror pane has a terminal")
            .clone();
        let remote_terminal = mirrored
            .remote_mirror
            .as_ref()
            .expect("a mirror")
            .remote_terminal
            .clone();
        app.state.workspaces.push(mirrored);
        app.terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(100, 70, 1 << 16, b""),
        );

        assert_eq!(
            app.mirror_pane_size_for("workbox", &remote_terminal),
            Some((70, 100)),
            "the size sent on claiming is the size of the pane being typed into"
        );
        assert_eq!(
            app.mirror_pane_size_for("workbox", "some-other-terminal"),
            None,
            "and nothing is claimed for a terminal this host does not mirror"
        );
    }

    /// A mirror is created at a guessed size, and after a handoff every mirror
    /// is rebuilt before the first render. Asking a host to resize to a guess
    /// shrank real terminals on the other machine and reflowed their output for
    /// the moment it took the layout to arrive.
    #[tokio::test]
    async fn a_size_that_is_still_a_guess_is_never_asked_for_as_a_resize() {
        let mut app = crate::app::tests::test_app();
        app.state.workspaces.clear();
        let mirrored = mirror("workbox", "workbox\u{1f}w1\u{1f}term-1", "remote");
        let terminal_id = mirrored
            .terminal_id(mirrored.root_pane)
            .expect("a mirror pane has a terminal")
            .clone();
        app.state.workspaces.push(mirrored);
        app.state.active = Some(0);
        app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(100, 70, 1 << 16, b""),
        );
        // Someone is looking, so the only thing standing between this and a
        // resize is whether the size has been laid out or merely guessed.
        app.state
            .watched_for_someone
            .insert(terminal_id.to_string());

        let targets = app.mirror_stream_targets("workbox");
        assert_eq!(targets.len(), 1);
        assert!(
            !targets[0].resize,
            "nothing laid out yet, so the size is a guess and must not resize a host"
        );

        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: crate::layout::PaneId::from_raw(1),
            rect: ratatui::layout::Rect::new(0, 0, 102, 72),
            inner_rect: ratatui::layout::Rect::new(1, 1, 100, 70),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        let targets = app.mirror_stream_targets("workbox");
        assert!(
            targets[0].resize,
            "once panes are laid out the size is real and may size the host"
        );
    }

    /// Mirrors reconcile on what a host pushes. A host that has stopped pushing
    /// still needs its connection dialled, or its mirrors stay blank for as
    /// long as it stays quiet -- which is how one wedged feed blanked a whole
    /// machine's view of another.
    #[tokio::test]
    async fn a_host_that_has_stopped_pushing_is_still_dialled() {
        let mut app = crate::app::tests::test_app();
        app.multiplexed_mirrors = true;
        app.mirror_remote_herdr
            .insert("quiet".to_string(), "/usr/bin/herdr".to_string());

        // Nothing mirrored, so there is nothing to dial for and no ssh is spawned.
        app.retry_mirror_streams();
        assert!(app.mirror_streams.is_empty());

        // A host that has fallen back to an attach per pane is not dialled on
        // the shared connection at all.
        app.mirror_streams.remove("quiet");
        app.mirror_multiplex_unsupported.insert(
            "quiet".to_string(),
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        );
        app.retry_mirror_streams();
        assert!(app.mirror_streams.is_empty());
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
        workspace.remote_mirror = Some(remote_mirror_record(&space(target), key, "term-remote"));
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
                workspace.remote_mirror = Some(remote_mirror_record(space, key, "term-remote"));
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
        workspace.remote_mirror = Some(remote_mirror_record(&space, &key, "term-remote"));
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

        let record =
            remote_mirror_record(&space, &pane.mirror_key(&space.target), &pane.terminal_id);

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

        let record = remote_mirror_record(&space, &key, "term-remote");

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
                workspace_id: format!("{origin_terminal}-ws"),
                terminal_id: origin_terminal.into(),
                label: None,
                color: None,
            }),
            ..agent_pane(workspace_id, label, terminal_id)
        }
    }

    /// An agent is one agent however far away it is heard from. Keying a
    /// mirror on the host that reported it meant the same pane reached through
    /// two hosts was a different mirror from the same pane reached directly --
    /// and worse, that handing off a hop re-keyed everything behind it, since
    /// every workspace id that hop reports changes. Two machines mirroring each
    /// other then showed each other their own panes, frozen and mangled, once
    /// the chain was two hops deep.
    #[test]
    fn a_mirror_keeps_one_identity_however_many_hosts_it_came_through() {
        let direct = agent_pane("w9", "notes", "term-far");
        let through_a_hop = mirrored_pane("w2", "notes", "term-hop", "ryi@sera", "term-far");
        let through_another_hop =
            mirrored_pane("w77", "notes", "term-other-hop", "sera", "term-far");

        assert_eq!(
            through_a_hop.mirror_key("workbox"),
            through_another_hop.mirror_key("elsewhere"),
            "the same agent heard from two different hops is one mirror"
        );
        assert_ne!(
            direct.mirror_key("ryi@sera"),
            through_a_hop.mirror_key("workbox"),
            "the direct key still names the workspace the origin reported"
        );
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
            "term-hop",
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

        let record =
            remote_mirror_record_for_origin(&space("pandora"), "k", "term-remote", &origin);

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

        // Keyed on sera, the machine that really runs it, rather than on the
        // hop we heard about it from.
        assert!(
            plan.iter().any(|action| matches!(
                action,
                MirrorAction::Create { key, .. }
                    if key == &mirrored_pane("w2", "notes", "term-hop", "ryi@sera", "term-far")
                        .mirror_key("workbox")
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
                    remote_terminal: "term-2".into(),
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
                remote_terminal: "term-7".into(),
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
                    remote_terminal: "term-2".into(),
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
