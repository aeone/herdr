use std::os::fd::{FromRawFd, OwnedFd};

use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

use crate::pty::fd;

pub(crate) struct SpawnedPty {
    pub master_fd: OwnedFd,
    pub child: Box<dyn Child + Send + Sync>,
}

pub(crate) fn spawn_with_portable_pty(
    rows: u16,
    cols: u16,
    cmd: CommandBuilder,
) -> std::io::Result<SpawnedPty> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let master_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| std::io::Error::other("pty master fd is unavailable"))?;
    let actor_fd = fd::duplicate_cloexec_fd(master_fd)?;
    let actor_fd = unsafe { OwnedFd::from_raw_fd(actor_fd) };
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    drop(pair);

    Ok(SpawnedPty {
        master_fd: actor_fd,
        child,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn parent_pty_fd_targets() -> Vec<String> {
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return Vec::new();
        };
        let mut targets: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .map(|target| target.to_string_lossy().into_owned())
            .filter(|target| target.starts_with("/dev/pts/") || target == "/dev/ptmx")
            .collect();
        targets.sort();
        targets
    }

    fn parent_pty_fd_count() -> usize {
        parent_pty_fd_targets().len()
    }

    const PTY_FD_CHILD_ENV: &str = "HERDR_PTY_FD_TEST_CHILD";

    /// Counts every pty fd in the process, so any other test opening a pty at the same time
    /// skews it. The check therefore runs in a child process that executes only this test.
    #[test]
    fn portable_pty_setup_leaves_one_parent_pty_fd() {
        if std::env::var_os(PTY_FD_CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test exe"))
                .args([
                    "--exact",
                    "pty::backend::unix::tests::portable_pty_setup_leaves_one_parent_pty_fd",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(PTY_FD_CHILD_ENV, "1")
                .output()
                .expect("run pty fd check in a child process");
            assert!(
                output.status.success(),
                "pty fd check failed in child process:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let before = parent_pty_fd_count();
        let mut cmd = CommandBuilder::new("/bin/cat");
        cmd.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);

        let mut spawned =
            spawn_with_portable_pty(24, 80, cmd).expect("portable pty setup succeeds");
        let after_spawn = parent_pty_fd_count();

        assert_eq!(
            after_spawn,
            before + 1,
            "portable-pty setup should leave only the Herdr-owned master fd in the parent: {:?}",
            parent_pty_fd_targets()
        );

        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        drop(spawned.master_fd);
    }
}
