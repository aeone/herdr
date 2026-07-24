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
pub(crate) fn plan_remote_mirrors(
    workspaces: &[Workspace],
    space: &RemoteSpaceConfig,
    snapshot: &RemoteSpaceSnapshot,
) -> Vec<MirrorAction> {
    let labels = mirror_labels(&snapshot.panes);
    let desired: Vec<(String, String)> = snapshot
        .panes
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
                if workspaces[ws_idx].custom_name.as_deref() != Some(label.as_str()) {
                    plan.push(MirrorAction::Rename {
                        ws_idx,
                        label: label.clone(),
                    });
                }
            }
            None => {
                let Some(pane) = snapshot.panes.get(index) else {
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

/// Builds the mirror record a created workspace carries.
///
/// Production and tests share this so a mirror's stored identity can never
/// drift from the identity `plan_remote_mirrors` looks it up by.
pub(crate) fn remote_mirror_record(space: &RemoteSpaceConfig, key: &str) -> RemoteMirror {
    RemoteMirror {
        target: space.target.clone(),
        host_label: space.display_label().to_string(),
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

/// Whether a local entry points at the session this server is running as.
///
/// Mirroring your own session is unbounded recursion: each mirror pane is an
/// agent pane, so the next poll would mirror the mirrors.
fn mirrors_own_session(space: &RemoteSpaceConfig) -> bool {
    let own = crate::session::active_name()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    let target = space
        .session
        .clone()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    own == target
}

/// Per-host polling bookkeeping, so a slow or unreachable host neither blocks
/// the render loop nor stacks up overlapping ssh calls.
#[derive(Debug, Default)]
pub(crate) struct RemoteSpacePoll {
    pub(crate) in_flight: bool,
    pub(crate) next_poll: Option<std::time::Instant>,
}

impl App {
    fn config_remote_spaces(&self) -> Vec<RemoteSpaceConfig> {
        self.remote_spaces.clone()
    }

    /// Starts a background poll for every configured host whose interval has
    /// elapsed. Each host is polled on its own thread; results come back as
    /// [`crate::events::AppEvent::RemoteSpacesPolled`].
    pub(crate) fn start_remote_space_polls_if_due(&mut self, now: std::time::Instant) {
        let spaces = self.config_remote_spaces();
        for space in spaces {
            // Mirroring the session we are running in would discover our own
            // mirror panes and mirror those in turn, without bound.
            if space.is_local() && mirrors_own_session(&space) {
                continue;
            }
            let poll = self
                .remote_space_polls
                .entry(space.target.clone())
                .or_default();
            if poll.in_flight || poll.next_poll.is_some_and(|deadline| now < deadline) {
                continue;
            }
            poll.in_flight = true;
            // The next deadline is set when the result lands, so a poll that
            // takes longer than the interval cannot immediately re-fire.
            poll.next_poll = None;

            let event_tx = self.event_tx.clone();
            let manage_ssh_config = self.manage_ssh_config;
            std::thread::spawn(move || {
                let result = crate::remote::spaces::discover(&space, manage_ssh_config)
                    .map_err(|err| err.to_string());
                let _ = event_tx.blocking_send(crate::events::AppEvent::RemoteSpacesPolled {
                    target: space.target.clone(),
                    result,
                });
            });
        }
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
            // The host was removed from config while its poll was in flight.
            self.remote_space_polls.remove(&target);
            return;
        };

        match result {
            Ok(snapshot) => self.reconcile_remote_mirrors(&space, &snapshot),
            Err(err) => {
                tracing::warn!(target = %target, %err, "remote space poll failed");
            }
        }

        let poll = self.remote_space_polls.entry(target).or_default();
        poll.in_flight = false;
        poll.next_poll = std::time::Instant::now().checked_add(space.poll_interval());
    }

    /// Brings mirrors for one configured host in line with `snapshot`.
    pub(crate) fn reconcile_remote_mirrors(
        &mut self,
        space: &RemoteSpaceConfig,
        snapshot: &RemoteSpaceSnapshot,
    ) {
        let plan = plan_remote_mirrors(&self.state.workspaces, space, snapshot);
        if plan.is_empty() {
            return;
        }
        tracing::info!(
            target = %space.target,
            panes = snapshot.panes.len(),
            creates = plan.iter().filter(|a| matches!(a, MirrorAction::Create { .. })).count(),
            closes = plan.iter().filter(|a| matches!(a, MirrorAction::Close { .. })).count(),
            renames = plan.iter().filter(|a| matches!(a, MirrorAction::Rename { .. })).count(),
            mirrors = self.state.workspaces.iter().filter(|w| w.remote_mirror.is_some()).count(),
            "reconciling remote mirrors"
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

    fn snapshot(panes: Vec<RemoteAgentPane>) -> RemoteSpaceSnapshot {
        RemoteSpaceSnapshot {
            remote_herdr: "/usr/bin/herdr".into(),
            panes,
        }
    }

    fn agent_pane(workspace_id: &str, label: &str, terminal_id: &str) -> RemoteAgentPane {
        RemoteAgentPane {
            terminal_id: terminal_id.into(),
            workspace_id: workspace_id.into(),
            workspace_label: label.into(),
            agent: Some("claude".into()),
        }
    }

    fn space(target: &str) -> RemoteSpaceConfig {
        RemoteSpaceConfig {
            target: target.into(),
            session: None,
            label: None,
            poll_seconds: 30,
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
        assert_eq!(argv[2], "workbox");
        assert!(argv[3].contains("terminal attach 'term-1'"), "{}", argv[3]);
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
