//! `herdr wall` — several terminals tiled in one client, private to it.
//!
//! The counterpart of `herdr focus` for more than one thing at a time. Like
//! focus it is a client of its own rather than server state: nothing it shows
//! appears in the session's workspaces, and other clients go on as they were.
//! It is made of two connections the server already serves:
//!
//! - one multiplexed observer (`ObserveTerminals`) carrying a frame stream for
//!   every tile's terminal, rendered at the terminal's own size and never
//!   resizing it. Each stream feeds a local copy of the terminal, which a tile
//!   that is only being watched draws re-wrapped to its own width and
//!   anchored to the bottom;
//! - while a tile is active, one terminal attach holding that tile's terminal
//!   at the tile's size, which is what lets a program redraw for the space it
//!   actually has, and where keys, pastes and clicks go. Switching tiles moves
//!   the attach, which releases the terminal it leaves so its own layout sizes
//!   it again -- the same size claim `herdr focus` makes, and no other.
//!
//! Keys are encoded against the local copy of the active terminal, which the
//! observer keeps in the target's cursor-key and keyboard-protocol modes, the
//! same way a mirror types into a terminal on another host.

mod keys;
pub(crate) mod layout;
pub(crate) mod picker;
mod render;
pub(crate) mod state;
pub(crate) mod targets;

#[cfg(unix)]
mod session;

pub(crate) use targets::{parse_targets_json, WallTarget, WallTargetKind};

/// Where the list of things to put on the wall comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetSource {
    /// The local server's API.
    Local,
    /// `herdr wall --targets-json` run on the far side of ssh, for a wall
    /// whose client socket is bridged from another machine. The bridge carries
    /// only the client socket, so the API there cannot be asked directly.
    Remote {
        ssh_target: String,
        remote_herdr: String,
    },
}

impl TargetSource {
    pub(crate) fn list(&self) -> std::io::Result<Vec<WallTarget>> {
        match self {
            Self::Local => crate::cli::list_wall_targets(),
            Self::Remote {
                ssh_target,
                remote_herdr,
            } => crate::remote::list_wall_targets_over_ssh(ssh_target, remote_herdr),
        }
    }

    /// How often to look at the targets' sizes again while tiles are up. A
    /// local listing is one request on a socket; a remote one is an ssh
    /// command, so it is asked far less often.
    fn refresh_interval(&self) -> std::time::Duration {
        match self {
            Self::Local => std::time::Duration::from_secs(2),
            Self::Remote { .. } => std::time::Duration::from_secs(15),
        }
    }
}

/// Runs the wall until it is quit. Returns the process exit code.
#[cfg(unix)]
pub(crate) fn run_wall(source: TargetSource) -> std::io::Result<i32> {
    session::run(source)
}

/// The wall types into terminals as raw bytes, as `herdr focus` does, and that
/// path is Unix only until Windows gets a semantic attach.
#[cfg(windows)]
pub(crate) fn run_wall(_source: TargetSource) -> std::io::Result<i32> {
    eprintln!("herdr wall is not supported on Windows yet");
    Ok(1)
}
