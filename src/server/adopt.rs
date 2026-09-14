//! Taking over a running server's panes without its cooperation.
//!
//! Live handoff needs the old server to answer `SIGUSR1` and pass its pane
//! terminals across a socket. A server that is wedged, or that has to leave the
//! cgroup it was started in, cannot be asked. Adoption does the same job from
//! outside: stop the old server, copy each pane's PTY master out of its
//! descriptor table with `pidfd_getfd(2)`, restore the last saved session with
//! those terminals imported, and only then kill the old server. The processes
//! in the panes never see a hangup, because a terminal stays open while any copy
//! of its master does.
//!
//! What cannot be carried is what lived only in the old server's parser:
//! alternate screen, mouse and keyboard modes, and screen history. Adopted panes
//! start blank in default modes and pick modes back up when their program next
//! sets them; the redraw nudge on first attach gets most TUIs to repaint.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::handoff_runtime::{HandoffRuntimeState, ImportedHandoffRuntime};
use crate::persist::SessionSnapshot;
use crate::platform::ProcessHandle;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// A terminal the old server holds, and what is running on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldTerminal {
    pub fd: RawFd,
    pub tty_index: u32,
    pub leader_pid: Option<u32>,
    pub pane_id: Option<String>,
}

/// A held terminal matched to a pane in the saved session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdoptedTerminal {
    pub raw_pane_id: u32,
    pub leader_pid: u32,
    pub fd: RawFd,
    pub tty_index: u32,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AdoptionPlan {
    pub adopted: Vec<AdoptedTerminal>,
    /// Terminals with a live shell that no saved pane accounts for. Killing the
    /// old server would hang these up, so any at all stops the adoption.
    pub unaccounted: Vec<HeldTerminal>,
    /// Terminals with nothing running on them; their panes had already exited.
    pub idle: Vec<HeldTerminal>,
}

pub(crate) struct TakenPanes {
    pub snapshot: SessionSnapshot,
    pub imports: HashMap<u32, ImportedHandoffRuntime>,
    pub old_server: StoppedServer,
}

/// The old server, stopped. Dropping this without [`StoppedServer::retire`]
/// resumes it, so every way out of a failed adoption leaves it running.
pub(crate) struct StoppedServer {
    handle: ProcessHandle,
    retired: bool,
}

impl StoppedServer {
    fn stop(handle: ProcessHandle) -> io::Result<Self> {
        handle.send_signal(libc::SIGSTOP)?;
        let server = Self {
            handle,
            retired: false,
        };
        let pid = server.handle.pid();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while crate::platform::process_state(pid) != Some('T') {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("server {pid} did not stop within {STOP_TIMEOUT:?}"),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        info!(pid, "stopped server for adoption");
        Ok(server)
    }

    /// Kills the old server and waits for it to exit. After this the adopter
    /// holds the only copies of the pane terminals, so there is no going back.
    pub(crate) fn retire(mut self) -> io::Result<()> {
        self.retired = true;
        let pid = self.handle.pid();
        self.handle.send_signal(libc::SIGKILL)?;
        if !self.handle.wait_exited(EXIT_TIMEOUT)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("server {pid} did not exit within {EXIT_TIMEOUT:?} of SIGKILL"),
            ));
        }
        info!(pid, "retired adopted server");
        Ok(())
    }
}

impl Drop for StoppedServer {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        let pid = self.handle.pid();
        match self.handle.send_signal(libc::SIGCONT) {
            Ok(()) => info!(pid, "adoption abandoned; resumed the old server"),
            Err(err) => {
                warn!(pid, err = %err, "adoption abandoned but the old server could not be resumed")
            }
        }
    }
}

/// Stops server `pid` and copies out the terminal of every pane in its last
/// saved session. The old server stays stopped until the returned
/// [`StoppedServer`] is retired or dropped.
pub(crate) fn take_panes(pid: u32) -> io::Result<TakenPanes> {
    check_adoptable(pid)?;
    let handle = ProcessHandle::open(pid)
        .map_err(|err| with_context(err, format!("cannot open server {pid}")))?;
    let old_server = StoppedServer::stop(handle)?;

    // Read once the server is stopped, so it cannot open or save anything
    // between the snapshot and the descriptor scan.
    let snapshot = crate::persist::load().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no readable saved session in {}",
                crate::session::data_dir().display()
            ),
        )
    })?;
    let masters = crate::platform::held_pty_masters(pid)
        .map_err(|err| with_context(err, format!("cannot list descriptors of server {pid}")))?;
    let leaders = crate::platform::pts_session_leaders();
    let terminals = masters
        .into_iter()
        .map(|(fd, tty_index)| {
            let leader_pid = leaders.get(&tty_index).copied();
            HeldTerminal {
                fd,
                tty_index,
                leader_pid,
                pane_id: leader_pid.and_then(|leader| {
                    crate::platform::process_env_var(
                        leader,
                        crate::integration::HERDR_PANE_ID_ENV_VAR,
                    )
                }),
            }
        })
        .collect();

    let plan = plan(&snapshot, terminals);
    if !plan.unaccounted.is_empty() {
        let listed = plan
            .unaccounted
            .iter()
            .map(|terminal| {
                format!(
                    "pts/{} (pid {}, pane {})",
                    terminal.tty_index,
                    terminal.leader_pid.unwrap_or_default(),
                    terminal.pane_id.as_deref().unwrap_or("unset")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(io::Error::other(format!(
            "{} terminal(s) in server {pid} match no pane in the saved session: {listed}. \
             Nothing was taken and the server was resumed. A pane opened in the last few \
             seconds is not saved yet; try again shortly.",
            plan.unaccounted.len()
        )));
    }

    let mut imports = HashMap::with_capacity(plan.adopted.len());
    for adopted in &plan.adopted {
        match copy_terminal(&old_server.handle, adopted) {
            Ok(import) => {
                imports.insert(adopted.raw_pane_id, import);
            }
            Err(err) => {
                close_imports(imports);
                return Err(with_context(
                    err,
                    format!("cannot copy pts/{} out of server {pid}", adopted.tty_index),
                ));
            }
        }
    }
    info!(
        pid,
        adopted = imports.len(),
        idle = plan.idle.len(),
        "copied pane terminals out of stopped server"
    );
    Ok(TakenPanes {
        snapshot,
        imports,
        old_server,
    })
}

pub(crate) fn plan(snapshot: &SessionSnapshot, terminals: Vec<HeldTerminal>) -> AdoptionPlan {
    plan_with_index(&saved_panes_by_public_id(snapshot), terminals)
}

/// Raw ids of saved panes, keyed by the public id their shell was started with
/// as `HERDR_PANE_ID`.
fn saved_panes_by_public_id(snapshot: &SessionSnapshot) -> HashMap<String, u32> {
    let mut by_public_id = HashMap::new();
    for workspace in &snapshot.workspaces {
        let Some(workspace_id) = workspace.id.as_deref() else {
            continue;
        };
        let in_tabs: HashSet<u32> = workspace
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.keys().copied())
            .collect();
        for (&raw, &number) in &workspace.public_pane_numbers {
            if in_tabs.contains(&raw) {
                by_public_id.insert(
                    crate::workspace::public_pane_id_for_number(workspace_id, number),
                    raw,
                );
            }
        }
    }
    by_public_id
}

fn plan_with_index(
    by_public_id: &HashMap<String, u32>,
    terminals: Vec<HeldTerminal>,
) -> AdoptionPlan {
    let mut plan = AdoptionPlan::default();
    let mut claimed = HashSet::new();
    for terminal in terminals {
        let Some(leader_pid) = terminal.leader_pid else {
            plan.idle.push(terminal);
            continue;
        };
        // A pane moved to another workspace keeps the id it was started with,
        // which then names nothing; that lands here as unaccounted too.
        match terminal
            .pane_id
            .as_ref()
            .and_then(|pane_id| by_public_id.get(pane_id))
            .copied()
        {
            Some(raw_pane_id) if claimed.insert(raw_pane_id) => {
                plan.adopted.push(AdoptedTerminal {
                    raw_pane_id,
                    leader_pid,
                    fd: terminal.fd,
                    tty_index: terminal.tty_index,
                })
            }
            _ => plan.unaccounted.push(terminal),
        }
    }
    plan
}

fn copy_terminal(
    server: &ProcessHandle,
    adopted: &AdoptedTerminal,
) -> io::Result<ImportedHandoffRuntime> {
    let fd = server.copy_fd(adopted.fd)?;
    let (rows, cols, width_px, height_px) = crate::platform::pty_window_size(fd.as_raw_fd())?;
    Ok(ImportedHandoffRuntime {
        master_fd: fd.into_raw_fd(),
        state: runtime_state(adopted, rows, cols, width_px, height_px),
    })
}

/// What a handoff would have sent for this pane, as far as it is visible from
/// outside the old server.
fn runtime_state(
    adopted: &AdoptedTerminal,
    rows: u16,
    cols: u16,
    width_px: u16,
    height_px: u16,
) -> HandoffRuntimeState {
    let rows = rows.max(1);
    let cols = cols.max(1);
    HandoffRuntimeState {
        pane_id: adopted.raw_pane_id,
        child_pid: adopted.leader_pid,
        rows,
        cols,
        cell_width_px: u32::from(width_px) / u32::from(cols),
        cell_height_px: u32::from(height_px) / u32::from(rows),
        keyboard_protocol_flags: 0,
        keyboard_protocol_ansi: None,
        input_state: None,
        terminal_title: None,
        initial_history_ansi: None,
    }
}

fn close_imports(imports: HashMap<u32, ImportedHandoffRuntime>) {
    for import in imports.into_values() {
        // SAFETY: each descriptor was copied into this process by
        // `copy_terminal` and has not been handed to anything else.
        drop(unsafe { OwnedFd::from_raw_fd(import.master_fd) });
    }
}

fn check_adoptable(pid: u32) -> io::Result<()> {
    if pid == std::process::id() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a server cannot adopt itself",
        ));
    }
    let args = crate::platform::process_args(pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))?;
    if !is_server_command(&args) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("process {pid} is not a herdr server: {}", args.join(" ")),
        ));
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    if crate::platform::process_uid(pid) != Some(uid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("server {pid} belongs to another user"),
        ));
    }
    let theirs = session_of(
        &args,
        crate::platform::process_env_var(pid, crate::session::SESSION_ENV_VAR),
    );
    let ours = crate::session::active_name();
    if theirs != ours {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "server {pid} serves session {}, but this process would serve {}; \
                 set {} to match",
                describe_session(theirs.as_deref()),
                describe_session(ours.as_deref()),
                crate::session::SESSION_ENV_VAR
            ),
        ));
    }
    Ok(())
}

/// `herdr server`, with or without flags, and not a `herdr server <subcommand>`
/// CLI call.
fn is_server_command(args: &[String]) -> bool {
    let program_is_herdr = args
        .first()
        .and_then(|program| Path::new(program).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("herdr"));
    let Some(server_at) = args.iter().position(|arg| arg == "server") else {
        return false;
    };
    program_is_herdr
        && args
            .get(server_at + 1)
            .is_none_or(|next| next.starts_with("--"))
}

/// The session a server process serves, from its `--session` flag or the
/// environment it was started with.
fn session_of(args: &[String], env_session: Option<String>) -> Option<String> {
    let mut from_flag = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--session" {
            from_flag = iter.next().cloned();
        } else if let Some(name) = arg.strip_prefix("--session=") {
            from_flag = Some(name.to_string());
        }
    }
    from_flag
        .or(env_session)
        .filter(|name| name != crate::session::DEFAULT_SESSION_NAME)
}

fn describe_session(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("'{name}'"),
        None => "default".to_string(),
    }
}

fn with_context(err: io::Error, context: String) -> io::Error {
    io::Error::new(err.kind(), format!("{context}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(
        fd: RawFd,
        tty_index: u32,
        leader_pid: Option<u32>,
        pane_id: Option<&str>,
    ) -> HeldTerminal {
        HeldTerminal {
            fd,
            tty_index,
            leader_pid,
            pane_id: pane_id.map(str::to_string),
        }
    }

    fn index(entries: &[(&str, u32)]) -> HashMap<String, u32> {
        entries
            .iter()
            .map(|(public_id, raw)| (public_id.to_string(), *raw))
            .collect()
    }

    #[test]
    fn terminals_are_matched_to_saved_panes_by_the_id_their_shell_was_given() {
        let plan = plan_with_index(
            &index(&[("w54:p1", 23), ("w54:p2", 24)]),
            vec![
                held(40, 78, Some(1034546), Some("w54:p2")),
                held(41, 79, Some(1034547), Some("w54:p1")),
            ],
        );
        assert_eq!(
            plan.adopted,
            vec![
                AdoptedTerminal {
                    raw_pane_id: 24,
                    leader_pid: 1034546,
                    fd: 40,
                    tty_index: 78
                },
                AdoptedTerminal {
                    raw_pane_id: 23,
                    leader_pid: 1034547,
                    fd: 41,
                    tty_index: 79
                },
            ]
        );
        assert!(plan.unaccounted.is_empty());
        assert!(plan.idle.is_empty());
    }

    #[test]
    fn saved_panes_are_indexed_by_public_id_only_when_they_sit_in_a_tab() {
        let snapshot: SessionSnapshot = serde_json::from_str(
            r#"{
                "workspaces": [
                    {
                        "id": "w54",
                        "identity_cwd": "/",
                        "public_pane_numbers": {"23": 1, "24": 2, "99": 3},
                        "tabs": [{
                            "layout": {"Split": {
                                "direction": "Vertical",
                                "ratio": 0.5,
                                "first": {"Pane": 23},
                                "second": {"Pane": 24}
                            }},
                            "panes": {"23": {"cwd": "/"}, "24": {"cwd": "/"}},
                            "zoomed": false
                        }]
                    },
                    {
                        "identity_cwd": "/",
                        "public_pane_numbers": {"5": 1},
                        "tabs": [{
                            "layout": {"Pane": 5},
                            "panes": {"5": {"cwd": "/"}},
                            "zoomed": false
                        }]
                    }
                ],
                "active": 0,
                "selected": 0
            }"#,
        )
        .expect("snapshot parses");
        assert_eq!(
            saved_panes_by_public_id(&snapshot),
            index(&[("w54:p1", 23), ("w54:p2", 24)])
        );
    }

    #[test]
    fn a_terminal_with_nothing_running_is_idle_not_unaccounted() {
        let plan = plan_with_index(&index(&[("w54:p1", 23)]), vec![held(40, 78, None, None)]);
        assert!(plan.adopted.is_empty());
        assert!(plan.unaccounted.is_empty());
        assert_eq!(plan.idle, vec![held(40, 78, None, None)]);
    }

    #[test]
    fn live_terminals_no_saved_pane_accounts_for_are_unaccounted() {
        let plan = plan_with_index(
            &index(&[("w54:p1", 23)]),
            vec![
                // Opened after the last save, or moved to another workspace.
                held(40, 78, Some(100), Some("w54:p9")),
                // Started by something other than herdr.
                held(41, 79, Some(101), None),
                // Two shells claiming one pane: only the first gets it.
                held(42, 80, Some(102), Some("w54:p1")),
                held(43, 81, Some(103), Some("w54:p1")),
            ],
        );
        assert_eq!(plan.adopted.len(), 1);
        assert_eq!(plan.adopted[0].leader_pid, 102);
        assert_eq!(
            plan.unaccounted
                .iter()
                .map(|terminal| terminal.fd)
                .collect::<Vec<_>>(),
            vec![40, 41, 43]
        );
    }

    #[test]
    fn runtime_state_derives_cell_size_and_never_reports_an_empty_grid() {
        let adopted = AdoptedTerminal {
            raw_pane_id: 7,
            leader_pid: 99,
            fd: 3,
            tty_index: 4,
        };
        let state = runtime_state(&adopted, 50, 200, 1800, 1000);
        assert_eq!((state.pane_id, state.child_pid), (7, 99));
        assert_eq!((state.rows, state.cols), (50, 200));
        assert_eq!((state.cell_width_px, state.cell_height_px), (9, 20));
        assert!(state.input_state.is_none());

        let empty = runtime_state(&adopted, 0, 0, 0, 0);
        assert_eq!((empty.rows, empty.cols), (1, 1));
        assert_eq!((empty.cell_width_px, empty.cell_height_px), (0, 0));
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn server_processes_are_told_apart_from_server_cli_calls() {
        assert!(is_server_command(&args(&[
            "/home/ryi/.local/bin/herdr",
            "server"
        ])));
        assert!(is_server_command(&args(&[
            "herdr",
            "server",
            "--handoff-import",
            "/tmp/sock",
            "token"
        ])));
        assert!(is_server_command(&args(&[
            "herdr",
            "--session",
            "work",
            "server"
        ])));
        assert!(!is_server_command(&args(&["herdr", "server", "stop"])));
        assert!(!is_server_command(&args(&["herdr"])));
        assert!(!is_server_command(&args(&["/usr/bin/zsh", "server"])));
    }

    #[test]
    fn a_server_session_comes_from_its_flag_before_its_environment() {
        assert_eq!(session_of(&args(&["herdr", "server"]), None), None);
        assert_eq!(
            session_of(&args(&["herdr", "server"]), Some("work".to_string())),
            Some("work".to_string())
        );
        assert_eq!(
            session_of(
                &args(&["herdr", "--session", "play", "server"]),
                Some("work".to_string())
            ),
            Some("play".to_string())
        );
        assert_eq!(
            session_of(&args(&["herdr", "--session=default", "server"]), None),
            None
        );
    }
}
