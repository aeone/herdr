use std::{
    collections::{HashSet, VecDeque},
    io::Write,
    os::fd::RawFd,
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
};

use super::{
    read_limited_reader, ClipboardCommand, ClipboardImage, ForegroundJob, ForegroundProcess,
    LimitedRead, Signal,
};

pub(crate) use super::unix_common::{
    configure_status_command, create_remote_private_dir, create_remote_ssh_config_dir,
    create_remote_ssh_config_file, hostname, local_datetime, remote_bridge_endpoint_path,
    remote_private_temp_base, remote_reattach_argument, remote_reattach_program,
    remote_ssh_config_paths, set_default_plugin_pane_pwd, status_commands_supported,
    StatusCommandGuard,
};

const WSL_MARKER_ENV_VARS: &[&str] = &["WSL_DISTRO_NAME", "WSL_INTEROP"];
const PROCESS_DETECTION_ENV_VAR: &str = "HERDR_PROCESS_DETECTION";
const CHILD_GROUPS_SCAN_LIMIT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessDetectionMode {
    Native,
    ChildGroups,
}

/// Descriptors a busy hub needs: roughly 14 per mirrored pane, so this carries
/// a few hundred panes. Capped by the hard limit at the point of use.
const SERVER_NOFILE_LIMIT_TARGET: libc::rlim_t = 65536;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcGroupMember {
    pid: u32,
    comm: String,
}

/// Linux ships a 1024 soft limit against a hard limit in the hundreds of
/// thousands, and this was a no-op here until a hub holding 65 mirrored panes
/// ran out at roughly 14 descriptors a pane -- on a machine with 64 GB of RAM
/// and eleven idle cores. The hard limit is left alone; only the soft one moves,
/// which needs no privilege.
pub fn raise_server_nofile_limit() {
    match raise_nofile_limit(SERVER_NOFILE_LIMIT_TARGET) {
        Ok(None) => {}
        Ok(Some((previous, target))) => {
            tracing::info!(previous, target, "raised server file descriptor soft limit")
        }
        Err(err) => tracing::warn!(err = %err, "failed to raise server file descriptor limit"),
    }
}

fn raise_nofile_limit(
    target: libc::rlim_t,
) -> std::io::Result<Option<(libc::rlim_t, libc::rlim_t)>> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `getrlimit` writes a `rlimit` through the pointer and nothing else.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: the call above succeeded, so the value is initialised.
    let mut limit = unsafe { limit.assume_init() };
    let Some(target) = target_nofile_soft_limit(limit.rlim_cur, limit.rlim_max, target) else {
        return Ok(None);
    };

    let previous = limit.rlim_cur;
    limit.rlim_cur = target;
    // SAFETY: `limit` is a fully initialised `rlimit` and the soft limit is
    // never raised above the hard one, which is what would make this fail.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(Some((previous, target)))
}

/// The soft limit to ask for: the target, capped by whatever the hard limit
/// allows, and only when it would be a raise.
fn target_nofile_soft_limit(
    current: libc::rlim_t,
    hard: libc::rlim_t,
    target: libc::rlim_t,
) -> Option<libc::rlim_t> {
    let target = if hard == libc::RLIM_INFINITY {
        target
    } else {
        target.min(hard)
    };

    (current < target).then_some(target)
}

pub(crate) fn should_draw_host_cursor_by_default() -> bool {
    running_inside_wsl()
}

pub(crate) fn should_query_host_terminal_palette() -> bool {
    !running_inside_wsl()
}

fn running_inside_wsl() -> bool {
    proc_file_indicates_wsl("/proc/sys/kernel/osrelease")
        || proc_file_indicates_wsl("/proc/version")
        || WSL_MARKER_ENV_VARS
            .iter()
            .any(|key| std::env::var_os(key).is_some())
        || std::path::Path::new("/run/WSL").exists()
}

fn proc_file_indicates_wsl(path: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text_indicates_wsl(&text))
        .unwrap_or(false)
}

fn text_indicates_wsl(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("microsoft") || text.contains("wsl")
}

fn parse_process_detection_mode(value: Option<&str>) -> Result<ProcessDetectionMode, &str> {
    match value {
        None | Some("") | Some("native") => Ok(ProcessDetectionMode::Native),
        Some("child-groups") => Ok(ProcessDetectionMode::ChildGroups),
        Some(value) => Err(value),
    }
}

fn process_detection_mode() -> ProcessDetectionMode {
    static MODE: OnceLock<ProcessDetectionMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let value = std::env::var(PROCESS_DETECTION_ENV_VAR).ok();
        parse_process_detection_mode(value.as_deref()).unwrap_or_else(|value| {
            tracing::warn!(
                variable = PROCESS_DETECTION_ENV_VAR,
                %value,
                "unknown process detection mode; using native detection"
            );
            ProcessDetectionMode::Native
        })
    })
}

fn raw_command_argv(command: &str, flag: &str) -> Vec<std::ffi::OsString> {
    vec!["/bin/sh".into(), flag.into(), command.into()]
}

pub(crate) fn detached_custom_command_process_platform(command: &str) -> std::process::Command {
    let argv = raw_command_argv(command, "-lc");
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

pub(crate) fn pane_custom_command_pty_builder_platform(
    command: &str,
) -> portable_pty::CommandBuilder {
    portable_pty::CommandBuilder::from_argv(raw_command_argv(command, "-c"))
}

pub(crate) fn scrollback_editor_argv(path: &std::path::Path) -> std::io::Result<Vec<String>> {
    let quoted_path = shell_quote(&path.display().to_string());
    let command = format!(
        r#"scrollback_file={quoted_path}; eval "${{EDITOR:-vi}} \"\$scrollback_file\""; status=$?; rm -f "$scrollback_file"; exit $status"#
    );
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), command])
}

pub(crate) fn interactive_shell_command(argv: &[String], shell_name: &str) -> Option<String> {
    super::interactive_unix_shell_command(argv, shell_name, shell_quote)
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Collect the foreground terminal job for a given child PID.
pub(crate) fn available_pane_shell(child_pid: u32) -> Option<String> {
    super::available_pane_shell_from_job(child_pid, foreground_job(child_pid)?)
}

pub fn foreground_job(child_pid: u32) -> Option<ForegroundJob> {
    if let Some(tpgid) = foreground_process_group_id(child_pid) {
        return foreground_job_for_group(child_pid, tpgid);
    }

    if process_detection_mode() != ProcessDetectionMode::ChildGroups {
        return None;
    }

    foreground_job_for_group(child_pid, child_groups_foreground_process_group(child_pid)?)
}

fn foreground_job_for_group(child_pid: u32, process_group_id: u32) -> Option<ForegroundJob> {
    let members = foreground_process_group_members(child_pid, process_group_id)?;
    let processes = members
        .into_iter()
        .map(|member| {
            let argv = process_argv(member.pid);
            ForegroundProcess {
                pid: member.pid,
                name: member.comm,
                argv0: None,
                cmdline: argv.as_ref().map(|parts| parts.join(" ")),
                argv,
            }
        })
        .collect::<Vec<_>>();

    if processes.is_empty() {
        return None;
    }

    Some(ForegroundJob {
        process_group_id,
        processes,
    })
}

/// Best-effort foreground group for environments that do not expose terminal
/// foreground groups. This mode is explicit because background jobs cannot be
/// distinguished from foreground jobs without the native terminal signal.
fn child_groups_foreground_process_group(child_pid: u32) -> Option<u32> {
    let shell_group_id = process_pgrp_and_comm(child_pid)
        .map(|(pgrp, _)| pgrp)
        .filter(|pgrp| *pgrp > 0)? as u32;

    child_groups_foreground_process_group_with(
        child_pid,
        shell_group_id,
        process_task_ids,
        process_task_children,
        |pid| process_pgrp_and_comm(pid).map(|(pgrp, _)| pgrp),
    )
}

fn child_groups_foreground_process_group_with(
    child_pid: u32,
    shell_group_id: u32,
    mut task_ids: impl FnMut(u32) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32) -> Vec<u32>,
    mut process_group_id: impl FnMut(u32) -> Option<i32>,
) -> Option<u32> {
    let mut newest = None;
    let mut scanned = 0usize;
    for tid in task_ids(child_pid) {
        for child in task_children(child_pid, tid) {
            if scanned >= CHILD_GROUPS_SCAN_LIMIT {
                return None;
            }
            scanned += 1;

            let Some(pgrp) = process_group_id(child) else {
                continue;
            };
            if pgrp <= 0 {
                continue;
            }
            let pgrp = pgrp as u32;
            if pgrp == shell_group_id {
                continue;
            }
            newest = Some(newest.map_or(pgrp, |current: u32| current.max(pgrp)));
        }
    }
    newest.or(Some(shell_group_id))
}

fn foreground_process_group_members(
    child_pid: u32,
    process_group_id: u32,
) -> Option<Vec<ProcGroupMember>> {
    foreground_process_group_members_with(
        child_pid,
        process_group_id,
        process_task_ids,
        process_task_children,
        live_process_group_member,
    )
}

fn foreground_process_group_members_with(
    child_pid: u32,
    process_group_id: u32,
    task_ids: impl FnMut(u32) -> Vec<u32>,
    task_children: impl FnMut(u32, u32) -> Vec<u32>,
    mut live_member: impl FnMut(u32, u32) -> Option<ProcGroupMember>,
) -> Option<Vec<ProcGroupMember>> {
    let mut members = process_tree_pids([child_pid, process_group_id], task_ids, task_children)
        .into_iter()
        .filter_map(|pid| live_member(process_group_id, pid))
        .collect::<Vec<_>>();
    members.sort_unstable_by_key(|member| member.pid);
    (!members.is_empty()).then_some(members)
}

fn process_tree_pids(
    roots: impl IntoIterator<Item = u32>,
    mut task_ids: impl FnMut(u32) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32) -> Vec<u32>,
) -> Vec<u32> {
    let mut pending = VecDeque::new();
    let mut visited = HashSet::new();
    for pid in roots {
        if pid > 0 && visited.insert(pid) {
            pending.push_back(pid);
        }
    }

    let mut pids = Vec::new();
    while let Some(pid) = pending.pop_front() {
        pids.push(pid);
        for tid in task_ids(pid) {
            for child_pid in task_children(pid, tid) {
                if child_pid > 0 && visited.insert(child_pid) {
                    pending.push_back(child_pid);
                }
            }
        }
    }
    pids
}

fn process_task_ids(pid: u32) -> Vec<u32> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| numeric_file_name(&entry))
        .collect()
}

fn process_task_children(pid: u32, tid: u32) -> Vec<u32> {
    let Some(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/children")).ok()
    else {
        return Vec::new();
    };
    children
        .split_whitespace()
        .filter_map(|child| child.parse::<u32>().ok())
        .collect()
}

fn numeric_file_name(entry: &std::fs::DirEntry) -> Option<u32> {
    let file_name = entry.file_name();
    let value = file_name.to_str()?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn live_process_group_member(process_group_id: u32, pid: u32) -> Option<ProcGroupMember> {
    let (pgrp, comm) = process_pgrp_and_comm(pid)?;
    (pgrp > 0 && pgrp as u32 == process_group_id).then_some(ProcGroupMember { pid, comm })
}

pub fn foreground_group_leader_job(process_group_id: u32) -> Option<ForegroundJob> {
    let (pgrp, name) = process_pgrp_and_comm(process_group_id)?;
    if pgrp as u32 != process_group_id {
        return None;
    }

    let argv = process_argv(process_group_id);
    Some(ForegroundJob {
        process_group_id,
        processes: vec![ForegroundProcess {
            pid: process_group_id,
            name,
            argv0: None,
            cmdline: argv.as_ref().map(|parts| parts.join(" ")),
            argv,
        }],
    })
}

pub fn foreground_process_group_id(child_pid: u32) -> Option<u32> {
    // /proc/<pid>/stat format: "pid (comm) state ppid pgrp session tty_nr tpgid ..."
    // The (comm) field can contain spaces and parens, so we find the last ')' first.
    let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After (comm): state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5)
    let tpgid: i32 = fields.get(5)?.parse().ok()?;
    (tpgid > 0).then_some(tpgid as u32)
}

pub fn foreground_process_group_id_for_tty_fd(fd: RawFd) -> Option<u32> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    (pgid > 0).then_some(pgid as u32)
}

fn process_pgrp_and_comm(pid: u32) -> Option<(i32, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_pgrp_and_comm_from_stat(&stat)
}

fn process_pgrp_and_comm_from_stat(stat: &str) -> Option<(i32, String)> {
    let close = stat.rfind(')')?;
    let comm = stat.get(1 + stat.find('(')?..close)?.to_string();
    let rest = stat.get(close + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let pgrp: i32 = fields.get(2)?.parse().ok()?;
    Some((pgrp, comm))
}

fn process_argv(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let parts: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Get the current working directory of a process.
/// Uses /proc/<pid>/cwd symlink.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Read a Herdr agent identity hint from a process environment.
pub fn process_agent_hint(pid: u32) -> Option<crate::detect::Agent> {
    if pid == 0 {
        return None;
    }
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    super::parse_agent_env_hint(&environ)
}

pub fn session_processes(child_pid: u32) -> Vec<u32> {
    let Some(session_id) = process_session_id(child_pid) else {
        return Vec::new();
    };

    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if process_session_id(pid) == Some(session_id) {
            pids.push(pid);
        }
    }
    pids
}

pub fn signal_processes(pids: &[u32], signal: Signal) {
    let sig = match signal {
        Signal::Hangup => libc::SIGHUP,
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };

    for &pid in pids {
        if pid == 0 {
            continue;
        }
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

pub fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub fn write_clipboard(bytes: &[u8]) -> bool {
    for command in clipboard_commands() {
        if run_clipboard_command(&command, bytes) {
            return true;
        }
    }
    false
}

pub fn read_clipboard_text() -> Option<String> {
    for command in read_clipboard_text_commands() {
        if let Some(text) = read_clipboard_text_with_command(&command) {
            return Some(text);
        }
    }
    None
}

pub fn open_url(url: &str) -> std::io::Result<Option<std::process::Child>> {
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Some)
}

pub fn read_clipboard_image() -> Option<ClipboardImage> {
    for (mime, extension) in [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/jpg", "jpg"),
        ("image/gif", "gif"),
        ("image/webp", "webp"),
        ("image/bmp", "bmp"),
    ] {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            if let Some(image) =
                read_validated_clipboard_image("wl-paste", &["--type", mime], extension)
            {
                return Some(image);
            }
        }

        if std::env::var_os("DISPLAY").is_some() {
            if let Some(image) = read_validated_clipboard_image(
                "xclip",
                &["-selection", "clipboard", "-t", mime, "-o"],
                extension,
            ) {
                return Some(image);
            }
        }
    }

    None
}

fn read_validated_clipboard_image(
    program: &str,
    args: &[&str],
    extension: &'static str,
) -> Option<ClipboardImage> {
    let bytes = read_clipboard_image_with_command(program, args)?;
    if !bytes_match_image_signature(extension, &bytes) {
        return None;
    }
    Some(ClipboardImage { bytes, extension })
}

fn bytes_match_image_signature(extension: &str, bytes: &[u8]) -> bool {
    match extension {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP",
        "bmp" => {
            if bytes.len() < 26 || !bytes.starts_with(b"BM") {
                return false;
            }
            let offset = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
            (26..=bytes.len()).contains(&offset)
        }
        _ => false,
    }
}

/// Show a native desktop notification through libnotify's command-line helper.
pub fn show_desktop_notification(title: &str, body: Option<&str>) -> std::io::Result<bool> {
    show_desktop_notification_with_command(title, body, |program| Command::new(program))
}

fn show_desktop_notification_with_command(
    title: &str,
    body: Option<&str>,
    mut command: impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(false);
    }

    let mut cmd = command("notify-send");
    cmd.arg("--").arg(title);
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        cmd.arg(body);
    }
    run_notification_command(cmd)
}

fn run_notification_command(mut command: Command) -> std::io::Result<bool> {
    let status = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };

    Ok(status.success())
}

fn read_clipboard_image_with_command(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new(program);
    command.args(args);
    read_clipboard_image_with_spawned_command(command)
}

fn read_clipboard_image_with_spawned_command(command: Command) -> Option<Vec<u8>> {
    read_clipboard_image_with_spawned_command_max(
        command,
        crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD,
    )
}

fn read_clipboard_image_with_spawned_command_max(
    mut command: Command,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;

    let read = match read_limited_reader(stdout, max_bytes) {
        Ok(read) => read,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    if read == LimitedRead::Oversized {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }

    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }

    match read {
        LimitedRead::Complete(bytes) => Some(bytes),
        LimitedRead::Empty | LimitedRead::Oversized => None,
    }
}

fn clipboard_commands() -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            args: &["--type", "text/plain;charset=utf-8"],
        });
    }

    if std::env::var_os("DISPLAY").is_some() {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-in"],
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--input"],
        });
    }

    commands
}

fn read_clipboard_text_commands() -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain;charset=utf-8"],
        });
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain"],
        });
    }

    if std::env::var_os("DISPLAY").is_some() {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-out"],
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--output"],
        });
    }

    commands
}

fn read_clipboard_text_with_command(command: &ClipboardCommand) -> Option<String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

    let mut child = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let read = match read_limited_reader(stdout, MAX_CLIPBOARD_TEXT_BYTES) {
        Ok(LimitedRead::Oversized) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Ok(read) => read,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }

    match read {
        LimitedRead::Complete(bytes) => String::from_utf8(bytes).ok(),
        LimitedRead::Empty => None,
        LimitedRead::Oversized => unreachable!("oversized clipboard text is handled before wait"),
    }
}

fn run_clipboard_command(command: &ClipboardCommand, bytes: &[u8]) -> bool {
    let mut child = match Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(bytes).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    drop(stdin);

    child.wait().map(|status| status.success()).unwrap_or(false)
}

fn process_session_id(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(3)?.parse().ok()
}

/// `HERDR_PANE_CGROUPS=0` turns per-pane cgroups off.
const PANE_CGROUPS_ENV_VAR: &str = "HERDR_PANE_CGROUPS";
const CGROUP_MOUNT: &str = "/sys/fs/cgroup";
/// The subgroup a delegated server runs in (systemd `DelegateSubgroup=server`).
const SERVER_CGROUP_NAME: &str = "server";
const PANE_CGROUP_PREFIX: &str = "pane-";
/// How long a new pane cgroup is kept while empty, waiting for its child.
const PANE_CGROUP_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

static PANE_CGROUPS: OnceLock<Option<PaneCgroups>> = OnceLock::new();

/// A cgroup of its own for each pane, beside the server's.
///
/// systemd-oomd kills whole leaf cgroups. With every pane in the server's
/// cgroup, one runaway process in one pane makes the server and every other pane
/// the thing it kills. Split per pane, the runaway's cgroup is the one with the
/// memory and the reclaim, so it is the one that goes, and the server's leaf,
/// which holds little, is left alone.
///
/// Only used when the server's cgroup is delegated to it: a unit with
/// `Delegate=yes` and `DelegateSubgroup=server` starts the server in
/// `<unit>/server`, owned by the service user, and panes go beside it as
/// `<unit>/pane-<server pid>-<n>`.
struct PaneCgroups {
    root: PathBuf,
    next: std::sync::atomic::AtomicU64,
}

impl PaneCgroups {
    fn detect() -> Option<Self> {
        if std::env::var(PANE_CGROUPS_ENV_VAR).as_deref() == Ok("0") {
            return None;
        }
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let relative = delegated_pane_cgroup_root(&own)?;
        let root = std::path::Path::new(CGROUP_MOUNT).join(relative.trim_start_matches('/'));
        // Panes need the memory and pids controllers to be accounted and
        // killed on their own. This write also fails when the root is not
        // ours, so it doubles as the delegation check.
        if let Err(err) = std::fs::write(root.join("cgroup.subtree_control"), "+memory +pids") {
            tracing::info!(
                root = %root.display(),
                err = %err,
                "per-pane cgroups unavailable; panes share the server's cgroup"
            );
            return None;
        }
        let cgroups = Self {
            root,
            next: std::sync::atomic::AtomicU64::new(1),
        };
        cgroups.remove_empty();
        tracing::info!(root = %cgroups.root.display(), "each pane gets its own cgroup");
        Some(cgroups)
    }

    fn create(&self) -> std::io::Result<PathBuf> {
        self.remove_empty();
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self
            .root
            .join(format!("{PANE_CGROUP_PREFIX}{}-{n}", std::process::id()));
        std::fs::create_dir(&dir)?;
        Ok(dir)
    }

    /// Removes pane cgroups whose processes have all exited. The kernel refuses
    /// to remove a populated cgroup, so attempting each one is the check.
    /// Recently created ones are left alone: their child may not have joined
    /// yet.
    fn remove_empty(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let is_pane = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(PANE_CGROUP_PREFIX));
            let settled = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= PANE_CGROUP_JOIN_GRACE);
            if is_pane && settled {
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
}

/// Where pane cgroups go, given `/proc/self/cgroup`: the parent of the
/// server's own cgroup, when that cgroup is a delegated `server` subgroup.
fn delegated_pane_cgroup_root(proc_self_cgroup: &str) -> Option<&str> {
    let path = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    let (parent, name) = path.rsplit_once('/')?;
    (name == SERVER_CGROUP_NAME && !parent.is_empty()).then_some(parent)
}

/// Puts the process `command` spawns into a cgroup of its own, when the server
/// runs delegated. See [`PaneCgroups`].
pub(crate) fn place_in_pane_cgroup_platform(command: &mut portable_pty::CommandBuilder) {
    let Some(cgroups) = PANE_CGROUPS.get_or_init(PaneCgroups::detect) else {
        return;
    };
    match cgroups.create() {
        Ok(dir) => command.join_cgroup(Some(dir)),
        Err(err) => tracing::warn!(
            root = %cgroups.root.display(),
            err = %err,
            "failed to create a pane cgroup; the pane shares the server's"
        ),
    }
}

/// Unix98 pty slaves are character major 136, and the pts number is the minor.
const PTY_SLAVE_MAJOR: u64 = 136;

pub(crate) fn process_args(pid: u32) -> Option<Vec<String>> {
    process_argv(pid)
}

/// The one-letter state from `/proc/<pid>/stat`: `R`, `S`, `T`, `Z` and so on.
pub(crate) fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 2..)?.chars().next()
}

pub(crate) fn process_uid(pid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}"))
        .ok()
        .map(|metadata| metadata.uid())
}

/// A variable from the environment `pid` was started with.
pub(crate) fn process_env_var(pid: u32, name: &str) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ_var(&environ, name)
}

fn environ_var(environ: &[u8], name: &str) -> Option<String> {
    environ.split(|&byte| byte == 0).find_map(|entry| {
        let value = entry.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
        String::from_utf8(value.to_vec()).ok()
    })
}

/// Every pseudo-terminal master `pid` holds, as `(descriptor, pts number)`.
pub(crate) fn held_pty_masters(pid: u32) -> std::io::Result<Vec<(RawFd, u32)>> {
    let mut masters = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let entry = entry?;
        let Some(fd) = numeric_file_name(&entry) else {
            continue;
        };
        let is_ptmx = std::fs::read_link(entry.path()).is_ok_and(|target| {
            target.as_os_str() == "/dev/ptmx" || target.as_os_str() == "/dev/pts/ptmx"
        });
        if !is_ptmx {
            continue;
        }
        let Some(tty_index) = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))
            .ok()
            .as_deref()
            .and_then(fdinfo_tty_index)
        else {
            continue;
        };
        masters.push((fd as RawFd, tty_index));
    }
    masters.sort_unstable();
    Ok(masters)
}

fn fdinfo_tty_index(fdinfo: &str) -> Option<u32> {
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("tty-index:"))
        .and_then(|value| value.trim().parse().ok())
}

/// The session leader on each `/dev/pts/<n>`, keyed by `n`.
///
/// A pane's shell leads its terminal's session whoever its parent is, which is
/// why this scans sessions rather than a server's children: after a live handoff
/// the shells belong to init while their terminals belong to the new server.
pub(crate) fn pts_session_leaders() -> std::collections::HashMap<u32, u32> {
    let mut leaders = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return leaders;
    };
    for entry in entries.flatten() {
        let Some(pid) = numeric_file_name(&entry) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some(pts) = pts_led_by(pid, &stat) {
            leaders.insert(pts, pid);
        }
    }
    leaders
}

fn pts_led_by(pid: u32, stat: &str) -> Option<u32> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After (comm): state(0) ppid(1) pgrp(2) session(3) tty_nr(4)
    if *fields.first()? == "Z" {
        return None;
    }
    let session: u32 = fields.get(3)?.parse().ok()?;
    let tty_nr: u64 = fields.get(4)?.parse().ok()?;
    if session != pid {
        return None;
    }
    let major = (tty_nr >> 8) & 0xfff;
    let minor = (tty_nr & 0xff) | ((tty_nr >> 20) << 8);
    (major == PTY_SLAVE_MAJOR).then_some(minor as u32)
}

/// The size a terminal reports: `(rows, cols, width_px, height_px)`.
pub(crate) fn pty_window_size(fd: RawFd) -> std::io::Result<(u16, u16, u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one winsize through the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((size.ws_row, size.ws_col, size.ws_xpixel, size.ws_ypixel))
}

/// A process held by pidfd, so signals and descriptor copies reach the process
/// that was opened even if its pid is reused later.
pub(crate) struct ProcessHandle {
    pid: u32,
    pidfd: std::os::fd::OwnedFd,
}

impl ProcessHandle {
    pub(crate) fn open(pid: u32) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        let target = libc::pid_t::try_from(pid)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor
        // or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, target, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the kernel just returned this descriptor and nothing else owns it.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as RawFd) };
        Ok(Self { pid, pidfd })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// A close-on-exec copy of the process's descriptor `fd`, via
    /// `pidfd_getfd(2)`. That needs ptrace access to the process: the same user
    /// and, unless the caller is its ancestor, `kernel.yama.ptrace_scope` 0.
    pub(crate) fn copy_fd(&self, fd: RawFd) -> std::io::Result<std::os::fd::OwnedFd> {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: pidfd_getfd takes a pidfd, a descriptor number in that process
        // and flags, and returns a new descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_getfd, self.pidfd.as_raw_fd(), fd, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the kernel just returned this descriptor and nothing else owns it.
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as RawFd) })
    }

    pub(crate) fn send_signal(&self, signal: libc::c_int) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: pidfd_send_signal with a null siginfo behaves like kill(2) on
        // the process the pidfd refers to.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits up to `timeout` for the process to exit, returning whether it has.
    /// Works for processes that are not the caller's children.
    pub(crate) fn wait_exited(&self, timeout: std::time::Duration) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut pollfd = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        loop {
            // SAFETY: one valid pollfd, and its count.
            let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return Ok(ready > 0);
        }
    }
}

#[cfg(test)]
mod adoption_tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[test]
    fn pane_cgroups_go_beside_a_delegated_server_subgroup_only() {
        let delegated = "0::/user.slice/user-1000.slice/herdr-server.service/server\n";
        assert_eq!(
            delegated_pane_cgroup_root(delegated),
            Some("/user.slice/user-1000.slice/herdr-server.service")
        );
        // A hybrid hierarchy lists v1 controllers first; only the v2 line counts.
        let hybrid = "12:pids:/user.slice\n0::/system.slice/herdr.service/server\n";
        assert_eq!(
            delegated_pane_cgroup_root(hybrid),
            Some("/system.slice/herdr.service")
        );
        // Not delegated: an ssh session scope, or a unit without the subgroup.
        assert_eq!(
            delegated_pane_cgroup_root("0::/user.slice/user-1000.slice/session-19.scope\n"),
            None
        );
        assert_eq!(
            delegated_pane_cgroup_root("0::/user.slice/user-1000.slice/herdr-server.service\n"),
            None
        );
        // Never the root of the hierarchy.
        assert_eq!(delegated_pane_cgroup_root("0::/server\n"), None);
        assert_eq!(delegated_pane_cgroup_root("0::/\n"), None);
    }

    #[test]
    fn fdinfo_tty_index_reads_the_pts_number() {
        let fdinfo = "pos:\t0\nflags:\t02100002\nmnt_id:\t32\nino:\t5\ntty-index:\t78\n";
        assert_eq!(fdinfo_tty_index(fdinfo), Some(78));
        assert_eq!(fdinfo_tty_index("pos:\t0\nflags:\t02\n"), None);
    }

    #[test]
    fn a_session_leader_is_found_on_its_pts_and_nothing_else_is() {
        // tty_nr 34894 is major 136, minor 78: /dev/pts/78.
        let leader = "1034546 (zsh) S 2751997 1034546 1034546 34894 1035682 4194560";
        assert_eq!(pts_led_by(1034546, leader), Some(78));
        let member = "1035682 (claude) S 1034546 1035682 1034546 34894 1035682 4194560";
        assert_eq!(pts_led_by(1035682, member), None);
        let zombie = "1034546 (zsh) Z 2751997 1034546 1034546 34894 -1 4194560";
        assert_eq!(pts_led_by(1034546, zombie), None);
        let no_tty = "900 (sshd) S 1 900 900 0 -1 4194560";
        assert_eq!(pts_led_by(900, no_tty), None);
        let awkward_comm = "77 (a) b (c)) S 1 77 77 34894 -1 0";
        assert_eq!(pts_led_by(77, awkward_comm), Some(78));
    }

    #[test]
    fn environ_var_matches_the_whole_name() {
        let environ = b"HERDR_PANE_ID_OLD=x\0HERDR_PANE_ID=w54:p1\0HOME=/home/ryi\0";
        assert_eq!(
            environ_var(environ, "HERDR_PANE_ID").as_deref(),
            Some("w54:p1")
        );
        assert_eq!(environ_var(environ, "HERDR_PANE"), None);
    }

    #[test]
    fn a_pty_master_copied_out_of_a_process_names_the_same_terminal() {
        // SAFETY: plain pty allocation; the descriptor is owned immediately.
        let raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(
            raw >= 0,
            "posix_openpt: {}",
            std::io::Error::last_os_error()
        );
        let master = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(unsafe { libc::grantpt(master.as_raw_fd()) }, 0);
        assert_eq!(unsafe { libc::unlockpt(master.as_raw_fd()) }, 0);
        let size = libc::winsize {
            ws_row: 31,
            ws_col: 97,
            ws_xpixel: 970,
            ws_ypixel: 620,
        };
        assert_eq!(
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );

        let pid = std::process::id();
        let tty_index = held_pty_masters(pid)
            .expect("list own descriptors")
            .into_iter()
            .find(|(fd, _)| *fd == master.as_raw_fd())
            .map(|(_, index)| index)
            .expect("own pty master is listed");

        let handle = ProcessHandle::open(pid).expect("pidfd_open on self");
        let copy = handle
            .copy_fd(master.as_raw_fd())
            .expect("pidfd_getfd on self");
        assert_ne!(copy.as_raw_fd(), master.as_raw_fd());
        let copied_index = held_pty_masters(pid)
            .expect("list own descriptors")
            .into_iter()
            .find(|(fd, _)| *fd == copy.as_raw_fd())
            .map(|(_, index)| index);
        assert_eq!(copied_index, Some(tty_index));
        assert_eq!(
            pty_window_size(copy.as_raw_fd()).expect("TIOCGWINSZ"),
            (31, 97, 970, 620)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::{cell::RefCell, collections::HashMap};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn wsl_marker_detection_matches_kernel_release_text() {
        assert!(text_indicates_wsl("5.15.167.4-microsoft-standard-WSL2"));
        assert!(text_indicates_wsl("4.4.0-19041-Microsoft"));
        assert!(!text_indicates_wsl("6.8.0-64-generic"));
        assert!(!text_indicates_wsl(""));
    }

    #[test]
    fn process_detection_mode_requires_explicit_child_groups_value() {
        assert_eq!(
            parse_process_detection_mode(None),
            Ok(ProcessDetectionMode::Native)
        );
        assert_eq!(
            parse_process_detection_mode(Some("")),
            Ok(ProcessDetectionMode::Native)
        );
        assert_eq!(
            parse_process_detection_mode(Some("native")),
            Ok(ProcessDetectionMode::Native)
        );
        assert_eq!(
            parse_process_detection_mode(Some("child-groups")),
            Ok(ProcessDetectionMode::ChildGroups)
        );
        assert_eq!(parse_process_detection_mode(Some("gvisor")), Err("gvisor"));
    }

    #[test]
    fn child_groups_foreground_group_picks_the_newest_job() {
        let tasks = HashMap::from([(100, vec![100])]);
        let children = HashMap::from([((100, 100), vec![200, 300])]);
        let groups = HashMap::from([(200, 200), (300, 300)]);

        let group = child_groups_foreground_process_group_with(
            100,
            100,
            |pid| tasks.get(&pid).cloned().unwrap_or_default(),
            |pid, tid| children.get(&(pid, tid)).cloned().unwrap_or_default(),
            |pid| groups.get(&pid).copied(),
        );

        assert_eq!(group, Some(300));
    }

    #[test]
    fn child_groups_foreground_group_returns_to_the_shell_group() {
        let tasks = HashMap::from([(100, vec![100])]);
        let children = HashMap::from([((100, 100), vec![150, 160])]);
        let groups = HashMap::from([(150, 90), (160, 90)]);

        let group = child_groups_foreground_process_group_with(
            100,
            90,
            |pid| tasks.get(&pid).cloned().unwrap_or_default(),
            |pid, tid| children.get(&(pid, tid)).cloned().unwrap_or_default(),
            |pid| groups.get(&pid).copied(),
        );

        assert_eq!(group, Some(90));
    }

    #[test]
    fn child_groups_foreground_group_skips_the_shell_group() {
        let tasks = HashMap::from([(100, vec![100])]);
        let children = HashMap::from([((100, 100), vec![150, 160, 300])]);
        let groups = HashMap::from([(150, 90), (160, 90), (300, 300)]);

        let group = child_groups_foreground_process_group_with(
            100,
            90,
            |pid| tasks.get(&pid).cloned().unwrap_or_default(),
            |pid, tid| children.get(&(pid, tid)).cloned().unwrap_or_default(),
            |pid| groups.get(&pid).copied(),
        );

        assert_eq!(group, Some(300));
    }

    #[test]
    fn child_groups_foreground_group_fails_closed_at_the_scan_limit() {
        let children: Vec<u32> = (1..=(CHILD_GROUPS_SCAN_LIMIT as u32 + 10)).collect();
        let mut inspected = 0usize;

        let group = child_groups_foreground_process_group_with(
            100,
            100,
            |_| vec![100],
            |_, _| children.clone(),
            |pid| {
                inspected += 1;
                Some(pid as i32)
            },
        );

        assert_eq!(inspected, CHILD_GROUPS_SCAN_LIMIT);
        assert_eq!(group, None);
    }

    #[test]
    fn foreground_members_follow_the_pane_tree_and_filter_by_process_group() {
        let tasks = HashMap::from([
            (100, vec![100, 101]),
            (200, vec![200]),
            (201, vec![201]),
            (210, vec![210]),
            (220, vec![220]),
            (221, vec![221]),
            (300, vec![300]),
        ]);
        let children = HashMap::from([
            ((100, 100), vec![200, 201, 300]),
            ((100, 101), vec![210]),
            ((200, 200), vec![220]),
            ((220, 220), vec![221]),
        ]);
        let processes = HashMap::from([
            (100, (100, "shell")),
            (200, (200, "leader")),
            (201, (200, "pipeline")),
            (210, (200, "thread-child")),
            (220, (220, "intermediate")),
            (221, (200, "nested-agent")),
            (300, (300, "background")),
            (9999, (200, "unrelated-host-process")),
        ]);
        let task_reads = RefCell::new(Vec::new());
        let child_reads = RefCell::new(Vec::new());
        let member_reads = RefCell::new(Vec::new());

        let members = foreground_process_group_members_with(
            100,
            200,
            |pid| {
                task_reads.borrow_mut().push(pid);
                tasks.get(&pid).cloned().unwrap_or_default()
            },
            |pid, tid| {
                child_reads.borrow_mut().push((pid, tid));
                children.get(&(pid, tid)).cloned().unwrap_or_default()
            },
            |process_group_id, pid| {
                member_reads.borrow_mut().push(pid);
                let (pgrp, comm) = processes.get(&pid)?;
                (*pgrp == process_group_id).then(|| ProcGroupMember {
                    pid,
                    comm: (*comm).to_string(),
                })
            },
        )
        .unwrap();

        assert_eq!(
            members
                .into_iter()
                .map(|member| (member.pid, member.comm))
                .collect::<Vec<_>>(),
            vec![
                (200, "leader".to_string()),
                (201, "pipeline".to_string()),
                (210, "thread-child".to_string()),
                (221, "nested-agent".to_string()),
            ]
        );
        assert!(child_reads.borrow().contains(&(100, 101)));
        assert!(task_reads.borrow().contains(&220));
        assert!(!task_reads.borrow().contains(&9999));
        assert!(!member_reads.borrow().contains(&9999));
    }

    #[test]
    fn foreground_members_degrade_to_the_direct_group_leader() {
        let members = foreground_process_group_members_with(
            100,
            200,
            |_| Vec::new(),
            |_, _| Vec::new(),
            |process_group_id, pid| {
                (pid == process_group_id).then(|| ProcGroupMember {
                    pid,
                    comm: "leader".to_string(),
                })
            },
        )
        .unwrap();

        assert_eq!(
            members,
            vec![ProcGroupMember {
                pid: 200,
                comm: "leader".to_string()
            }]
        );
    }

    #[test]
    fn foreground_members_observe_new_children_without_a_snapshot_cache() {
        let children = RefCell::new(HashMap::from([((100, 100), vec![200])]));
        let discover = || {
            foreground_process_group_members_with(
                100,
                200,
                |pid| vec![pid],
                |pid, tid| {
                    children
                        .borrow()
                        .get(&(pid, tid))
                        .cloned()
                        .unwrap_or_default()
                },
                |process_group_id, pid| {
                    [200, 201]
                        .contains(&pid)
                        .then(|| ProcGroupMember {
                            pid,
                            comm: format!("member-{pid}"),
                        })
                        .filter(|_| process_group_id == 200)
                },
            )
            .unwrap()
            .into_iter()
            .map(|member| member.pid)
            .collect::<Vec<_>>()
        };

        assert_eq!(discover(), vec![200]);
        children.borrow_mut().insert((100, 100), vec![200, 201]);
        assert_eq!(discover(), vec![200, 201]);
    }

    #[test]
    fn proc_stat_parsing_keeps_group_leader_inputs_live() {
        assert_eq!(
            process_pgrp_and_comm_from_stat("123 (name with ) paren) S 1 456 789 0 456"),
            Some((456, "name with ) paren".to_string()))
        );
    }

    #[test]
    fn clipboard_commands_prefer_wayland_when_available() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
            std::env::remove_var("DISPLAY");
        }
        let commands = clipboard_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "wl-copy");
    }

    #[test]
    fn clipboard_commands_include_x11_fallbacks() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::set_var("DISPLAY", ":0");
        }
        let commands = clipboard_commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].program, "xclip");
        assert_eq!(commands[1].program, "xsel");
    }

    #[test]
    fn read_clipboard_text_commands_include_session_backends() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
            std::env::set_var("DISPLAY", ":0");
        }

        let commands = read_clipboard_text_commands();
        assert_eq!(commands[0].program, "wl-paste");
        assert_eq!(commands[1].program, "wl-paste");
        assert_eq!(commands[2].program, "xclip");
        assert_eq!(commands[3].program, "xsel");
    }

    #[test]
    fn read_clipboard_text_with_command_reads_utf8() {
        let command = ClipboardCommand {
            program: "printf",
            args: &["feature/linear-302"],
        };

        assert_eq!(
            read_clipboard_text_with_command(&command).as_deref(),
            Some("feature/linear-302")
        );
    }

    #[test]
    fn read_clipboard_text_with_command_rejects_oversized_output() {
        let command = ClipboardCommand {
            program: "sh",
            args: &["-c", "yes x | head -c 1048578"],
        };

        assert_eq!(read_clipboard_text_with_command(&command), None);
    }

    #[test]
    fn read_clipboard_image_with_spawned_command_reads_under_limit() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf image");

        assert_eq!(
            read_clipboard_image_with_spawned_command_max(command, 16),
            Some(b"image".to_vec())
        );
    }

    #[test]
    fn read_clipboard_image_with_spawned_command_rejects_over_limit() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf oversized");

        assert_eq!(
            read_clipboard_image_with_spawned_command_max(command, 4),
            None
        );
    }

    #[test]
    fn read_clipboard_image_rejects_xclip_text_served_for_image_target() {
        let _guard = env_lock().lock().unwrap();
        let temp_dir =
            std::env::temp_dir().join(format!("herdr-fake-xclip-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let fake_xclip = temp_dir.join("xclip");
        std::fs::write(&fake_xclip, "#!/bin/sh\nprintf '# Tasks'\n")
            .expect("fake xclip should be written");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&fake_xclip)
                .expect("fake xclip metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&fake_xclip, permissions)
                .expect("fake xclip should be executable");
        }

        let old_path = std::env::var_os("PATH");
        let test_path = match old_path.as_ref() {
            Some(path) => {
                let mut paths = vec![temp_dir.clone()];
                paths.extend(std::env::split_paths(path));
                std::env::join_paths(paths).expect("test path should be valid")
            }
            None => temp_dir.clone().into_os_string(),
        };

        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::set_var("DISPLAY", ":0");
            std::env::set_var("PATH", test_path);
        }

        let result = read_clipboard_image();

        unsafe {
            match old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = std::fs::remove_file(fake_xclip);
        let _ = std::fs::remove_dir(temp_dir);

        assert_eq!(result, None);
    }

    #[test]
    fn read_clipboard_image_rejects_wayland_xclip_fallback_text_for_image_target() {
        let _guard = env_lock().lock().unwrap();
        let temp_dir =
            std::env::temp_dir().join(format!("herdr-fake-wayland-xclip-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let fake_wl_paste = temp_dir.join("wl-paste");
        let fake_xclip = temp_dir.join("xclip");
        std::fs::write(&fake_wl_paste, "#!/bin/sh\nexit 1\n")
            .expect("fake wl-paste should be written");
        std::fs::write(&fake_xclip, "#!/bin/sh\nprintf '# Tasks'\n")
            .expect("fake xclip should be written");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            for command in [&fake_wl_paste, &fake_xclip] {
                let mut permissions = std::fs::metadata(command)
                    .expect("fake clipboard command metadata")
                    .permissions();
                permissions.set_mode(0o700);
                std::fs::set_permissions(command, permissions)
                    .expect("fake clipboard command should be executable");
            }
        }

        let old_path = std::env::var_os("PATH");
        let test_path = match old_path.as_ref() {
            Some(path) => {
                let mut paths = vec![temp_dir.clone()];
                paths.extend(std::env::split_paths(path));
                std::env::join_paths(paths).expect("test path should be valid")
            }
            None => temp_dir.clone().into_os_string(),
        };

        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
            std::env::set_var("DISPLAY", ":0");
            std::env::set_var("PATH", test_path);
        }

        let result = read_clipboard_image();

        unsafe {
            match old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = std::fs::remove_file(fake_wl_paste);
        let _ = std::fs::remove_file(fake_xclip);
        let _ = std::fs::remove_dir(temp_dir);

        assert_eq!(result, None);
    }

    #[test]
    fn read_validated_clipboard_image_accepts_real_png_payload() {
        assert_eq!(
            read_validated_clipboard_image(
                "sh",
                &["-c", "printf '\\211PNG\\r\\n\\032\\nrest-of-image'"],
                "png"
            ),
            Some(ClipboardImage {
                bytes: b"\x89PNG\r\n\x1a\nrest-of-image".to_vec(),
                extension: "png",
            })
        );
    }

    #[test]
    fn image_signatures_match_only_their_format() {
        assert!(bytes_match_image_signature("png", b"\x89PNG\r\n\x1a\n..."));
        assert!(bytes_match_image_signature(
            "jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0]
        ));
        assert!(bytes_match_image_signature("gif", b"GIF87a..."));
        assert!(bytes_match_image_signature("gif", b"GIF89a..."));
        assert!(bytes_match_image_signature(
            "webp",
            b"RIFF\x10\x00\x00\x00WEBPVP8 "
        ));

        let mut bmp = vec![0u8; 26];
        bmp[..2].copy_from_slice(b"BM");
        bmp[10] = 26;
        assert!(bytes_match_image_signature("bmp", &bmp));

        assert!(!bytes_match_image_signature("png", b"# Tasks"));
        assert!(!bytes_match_image_signature("jpg", b"plain clipboard text"));
        assert!(!bytes_match_image_signature("gif", b""));
        assert!(!bytes_match_image_signature("webp", b"RIFF but not webp"));
        assert!(!bytes_match_image_signature("bmp", b"\x89PNG\r\n\x1a\n"));
        assert!(!bytes_match_image_signature(
            "bmp",
            b"BM text is not a bitmap"
        ));
        assert!(!bytes_match_image_signature("svg", b"<svg></svg>"));
    }

    #[test]
    fn desktop_notification_separates_option_like_titles() {
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::set_var("DISPLAY", ":0");
        }

        let path =
            std::env::temp_dir().join(format!("herdr-notify-send-args-{}", std::process::id()));
        let script = "printf '%s\\n' \"$@\" > \"$HERDR_NOTIFY_ARGS\"";
        let shown = show_desktop_notification_with_command("-danger", Some("body"), |_| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(script)
                .arg("notify-send")
                .env("HERDR_NOTIFY_ARGS", &path);
            cmd
        })
        .expect("notification command should run");

        assert!(shown);
        let args = std::fs::read_to_string(&path).expect("args file");
        let _ = std::fs::remove_file(&path);
        assert_eq!(args, "--\n-danger\nbody\n");
    }

    #[test]
    fn scrollback_editor_argv_preserves_unix_editor_shell_semantics() {
        let path = std::path::Path::new("/tmp/herdr scrollback.txt");
        let argv = scrollback_editor_argv(path).unwrap();

        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("EDITOR:-vi"));
        assert!(argv[2].contains("/tmp/herdr scrollback.txt"));
    }
}
