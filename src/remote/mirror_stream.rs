//! One connection per mirrored host, carrying every pane of it.
//!
//! A mirror used to be an `ssh` running `herdr terminal attach` per remote
//! pane: a process, a PTY pair, a remote process, a connection and an
//! *exclusive* claim on that terminal, all per pane. Thirty mirrors meant
//! thirty of each, which is what put a 64 GB host against its descriptor
//! ceiling and forced the whole fleet to mirror in a star, since two machines
//! cannot both hold a pane.
//!
//! This is the other shape: one `ssh` per host running
//! `herdr terminal session observe-many`, which takes the set of terminals to
//! watch on stdin and streams back frames tagged with the terminal they belong
//! to. Watching claims nothing, so any number of machines may watch the same
//! host, and the set can change as panes come and go without reopening
//! anything.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};

use base64::Engine as _;

use crate::config::RemoteSpaceConfig;
use crate::events::AppEvent;

/// A terminal to watch, and the size to render it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MirrorStreamTarget {
    pub(crate) terminal_id: String,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    /// Whether someone is looking at the pane this lands in, and so whether the
    /// host may size its terminal to match.
    pub(crate) resize: bool,
}

/// The line asking the host to watch exactly this set of terminals.
pub(crate) fn observe_request_line(targets: &[MirrorStreamTarget]) -> String {
    let targets: Vec<serde_json::Value> = targets
        .iter()
        .map(|target| {
            serde_json::json!({
                "target": target.terminal_id,
                "cols": target.cols.max(1),
                "rows": target.rows.max(1),
                "resize": target.resize,
            })
        })
        .collect();
    let mut line = serde_json::json!({"type": "terminal.observe", "targets": targets}).to_string();
    line.push('\n');
    line
}

/// What one line from the host means to us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorStreamLine {
    Frame {
        terminal_id: String,
        bytes: Vec<u8>,
    },
    Ended {
        terminal_id: String,
        reason: Option<String>,
    },
    /// How the host terminal wants keys encoded; see [`input_mode_bytes`].
    Modes {
        terminal_id: String,
        application_cursor: bool,
        kitty_keyboard_flags: u16,
    },
    Closed {
        reason: Option<String>,
    },
}

/// Reads one line of the host's answer.
///
/// Anything unrecognised is dropped rather than ending the stream: a host one
/// version ahead may say things this build has no name for, and the panes it
/// does understand should keep drawing.
pub(crate) fn parse_stream_line(line: &str) -> Option<MirrorStreamLine> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value.get("type")?.as_str()? {
        "terminal.frame" => {
            let terminal_id = value.get("terminal_id")?.as_str()?.to_owned();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value.get("bytes")?.as_str()?)
                .ok()?;
            Some(MirrorStreamLine::Frame { terminal_id, bytes })
        }
        "terminal.ended" => Some(MirrorStreamLine::Ended {
            terminal_id: value.get("terminal_id")?.as_str()?.to_owned(),
            reason: value
                .get("reason")
                .and_then(|reason| reason.as_str())
                .map(str::to_owned),
        }),
        "terminal.modes" => Some(MirrorStreamLine::Modes {
            terminal_id: value.get("terminal_id")?.as_str()?.to_owned(),
            application_cursor: value.get("application_cursor")?.as_bool()?,
            kitty_keyboard_flags: u16::try_from(value.get("kitty_keyboard_flags")?.as_u64()?)
                .ok()?,
        }),
        "terminal.closed" => Some(MirrorStreamLine::Closed {
            reason: value
                .get("reason")
                .and_then(|reason| reason.as_str())
                .map(str::to_owned),
        }),
        _ => None,
    }
}

/// The escape sequences that put a terminal in these key-encoding modes: DECCKM
/// set or reset, and the kitty keyboard flags replaced outright (`CSI = f ; 1 u`)
/// rather than pushed, so repeating them never grows the local terminal's flag
/// stack.
pub(crate) fn input_mode_bytes(application_cursor: bool, kitty_keyboard_flags: u16) -> Vec<u8> {
    let decckm = if application_cursor {
        "\x1b[?1h"
    } else {
        "\x1b[?1l"
    };
    format!("{decckm}\x1b[={kitty_keyboard_flags};1u").into_bytes()
}

/// Whether what the far side said is a build that does not know the command.
///
/// A host too old prints the usage for the commands it does have. Anything
/// else -- ssh failing, a host asleep, a server mid-restart -- says nothing
/// about what that host can do once it is back, and must not cost it the
/// shared connection.
pub(crate) fn complaint_means_too_old(complaint: Option<&str>) -> bool {
    complaint.is_some_and(|complaint| {
        let complaint = complaint.to_ascii_lowercase();
        complaint.contains("usage:")
            || complaint.contains("unexpected argument")
            || complaint.contains("unknown command")
    })
}

/// A live connection to one host.
pub(crate) struct MirrorStream {
    child: Child,
    stdin: Option<ChildStdin>,
    watching: Vec<MirrorStreamTarget>,
    /// Terminals whose copy on this side was rebuilt since the host was last
    /// told the set, so the next telling marks them to start over.
    fresh: std::collections::HashSet<String>,
    /// Frames this connection has carried, which is how a host that cannot do
    /// this at all is told from one whose link dropped.
    frames: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The last thing the far side said on stderr, which is where a host too
    /// old for this command prints its usage.
    complaint: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Every set this connection has been told to watch, kept only in tests so
    /// one host's churn can be asserted against another's quiet.
    #[cfg(test)]
    told: Vec<Vec<MirrorStreamTarget>>,
}

impl MirrorStream {
    /// Opens the connection and starts a thread reading its frames.
    pub(crate) fn spawn(
        space: &RemoteSpaceConfig,
        remote_herdr: &str,
        events: tokio::sync::mpsc::Sender<AppEvent>,
    ) -> std::io::Result<Self> {
        let argv = crate::remote::spaces::observe_many_argv(space, remote_herdr);
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::other("empty observe command"))?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("observe stream has no stdout"))?;

        let frames = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reader_frames = frames.clone();
        let complaint = std::sync::Arc::new(std::sync::Mutex::new(None));
        if let Some(stderr) = child.stderr.take() {
            let complaint = complaint.clone();
            std::thread::Builder::new()
                .name("herdr-mirror-err".to_string())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if let Ok(mut last) = complaint.lock() {
                            *last = Some(line);
                        }
                    }
                })?;
        }

        let target = space.target.clone();
        let reader_target = target.clone();
        std::thread::Builder::new()
            .name(format!("herdr-mirror-{}", short_thread_name(&target)))
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let Some(parsed) = parse_stream_line(&line) else {
                        continue;
                    };
                    let event = match parsed {
                        MirrorStreamLine::Frame { terminal_id, bytes } => {
                            reader_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            AppEvent::MirrorFrame {
                                target: reader_target.clone(),
                                terminal_id,
                                bytes,
                            }
                        }
                        MirrorStreamLine::Ended {
                            terminal_id,
                            reason,
                        } => AppEvent::MirrorTerminalEnded {
                            target: reader_target.clone(),
                            terminal_id,
                            reason,
                        },
                        // Applied as the escape sequences that set them, into
                        // the same local terminal the frames draw into, which
                        // is what keys typed at the mirror are encoded from.
                        MirrorStreamLine::Modes {
                            terminal_id,
                            application_cursor,
                            kitty_keyboard_flags,
                        } => AppEvent::MirrorFrame {
                            target: reader_target.clone(),
                            terminal_id,
                            bytes: input_mode_bytes(application_cursor, kitty_keyboard_flags),
                        },
                        MirrorStreamLine::Closed { reason } => AppEvent::MirrorStreamClosed {
                            target: reader_target.clone(),
                            reason,
                        },
                    };
                    if events.blocking_send(event).is_err() {
                        return;
                    }
                }
                let _ = events.blocking_send(AppEvent::MirrorStreamClosed {
                    target: reader_target,
                    reason: None,
                });
            })?;

        let _ = target;
        Ok(Self {
            child,
            stdin,
            watching: Vec::new(),
            fresh: std::collections::HashSet::new(),
            frames,
            complaint,
            #[cfg(test)]
            told: Vec::new(),
        })
    }

    /// A stream with no host behind it, for testing what happens when one says
    /// nothing.
    #[cfg(test)]
    pub(crate) fn test_without_a_host(frames: u64, complaint: Option<&str>) -> Self {
        Self {
            child: std::process::Command::new("true")
                .spawn()
                .expect("spawning `true` should work"),
            stdin: None,
            watching: Vec::new(),
            fresh: std::collections::HashSet::new(),
            frames: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(frames)),
            complaint: std::sync::Arc::new(std::sync::Mutex::new(complaint.map(str::to_owned))),
            told: Vec::new(),
        }
    }

    /// A connection already watching `targets`, with no host behind it.
    ///
    /// Enough to tell "this host was told again" from "this host was left
    /// alone", which is the difference between a mirror that repaints and one
    /// that does not.
    #[cfg(test)]
    pub(crate) fn test_watching(targets: Vec<MirrorStreamTarget>) -> Self {
        let mut stream = Self::test_without_a_host(1, None);
        stream.watching = targets;
        stream
    }

    /// The sets this connection has been told to watch since it was made.
    #[cfg(test)]
    pub(crate) fn told(&self) -> &[Vec<MirrorStreamTarget>] {
        &self.told
    }

    /// Whether this connection ever carried a frame.
    ///
    /// A host too old to know `observe-many` prints its usage and exits, which
    /// looks like a dropped connection except that nothing ever came down it.
    pub(crate) fn carried_a_frame(&self) -> bool {
        self.frames.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// The last thing the far side complained about, if anything.
    pub(crate) fn complaint(&self) -> Option<String> {
        self.complaint.lock().ok().and_then(|last| last.clone())
    }

    /// Whether this stream is already watching exactly these terminals.
    pub(crate) fn is_watching(&self, targets: &[MirrorStreamTarget]) -> bool {
        self.watching == targets
    }

    /// Replaces the set of terminals this connection carries.
    /// Forgets what this connection was last told to watch, so the next
    /// reconcile names the set again even though it has not changed.
    ///
    /// The host sends only what changed since the frame it last sent each
    /// watcher. A mirror pane rebuilt on this side -- which every handoff of
    /// the host causes, because mirror identity carries the host's workspace id
    /// -- starts empty and would receive those differences against a frame it
    /// never saw, leaving an idle terminal blank for as long as it stays idle.
    /// Naming the set again is what makes the host start from a whole frame.
    pub(crate) fn forget_targets(&mut self) {
        self.watching.clear();
    }

    /// The copy of `terminal_id` on this side was rebuilt: the next telling of
    /// the set first leaves it out and then names it, so the host drops what it
    /// had for it and starts over with a whole frame and its key modes, instead
    /// of carrying on from a baseline the new copy never had. Naming the set
    /// again alone does not do that -- a watcher re-sends its whole set
    /// whenever any pane changes size, and the host rightly keeps every
    /// unchanged terminal's baseline when it does. Leaving it out needs nothing
    /// from the host that any version does not already do.
    pub(crate) fn restart_target(&mut self, terminal_id: &str) {
        self.fresh.insert(terminal_id.to_owned());
        self.forget_targets();
    }

    pub(crate) fn set_targets(&mut self, targets: Vec<MirrorStreamTarget>) -> std::io::Result<()> {
        let mut line = String::new();
        if !self.fresh.is_empty() {
            let without_fresh: Vec<MirrorStreamTarget> = targets
                .iter()
                .filter(|target| !self.fresh.contains(&target.terminal_id))
                .cloned()
                .collect();
            line.push_str(&observe_request_line(&without_fresh));
        }
        line.push_str(&observe_request_line(&targets));
        #[cfg(test)]
        self.told.push(targets.clone());
        match self.stdin.as_mut() {
            Some(stdin) => {
                stdin.write_all(line.as_bytes())?;
                stdin.flush()?;
            }
            // A test connection has no host to write to, but which sets it was
            // told is the whole point of one, so it is recorded either way.
            None if cfg!(test) => {}
            None => return Err(std::io::Error::other("observe stream stdin is closed")),
        }
        self.watching = targets;
        self.fresh.clear();
        Ok(())
    }

    /// Closes the connection, which ends every mirror it was carrying.
    pub(crate) fn stop(mut self) {
        // Closing stdin is the polite ask; the kill is for a host that has
        // stopped listening, such as one that went to sleep mid-stream.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The one writable connection a host gets, moved from pane to pane.
///
/// A terminal takes one controller at a time, so this is claimed when a mirror
/// is typed into and moved when another is. Watching is unaffected: the frames
/// keep arriving on the shared connection whatever this is pointed at.
pub(crate) struct MirrorControl {
    child: Child,
    stdin: Option<ChildStdin>,
    controlling: String,
}

impl MirrorControl {
    /// `size` is the pane the claim is landing in, as `(rows, cols)`. It is
    /// carried in the command because attaching is what sizes the terminal:
    /// correcting it afterwards is a visible shrink and back again.
    pub(crate) fn spawn(
        space: &RemoteSpaceConfig,
        terminal_id: &str,
        remote_herdr: &str,
        size: Option<(u16, u16)>,
    ) -> std::io::Result<Self> {
        let argv = crate::remote::spaces::control_argv(space, terminal_id, remote_herdr, size);
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::other("empty control command"))?;
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take();
        // A refused claim is reported on stderr and nowhere else, and losing it
        // leaves a mirror that silently will not take typing.
        if let Some(stderr) = child.stderr.take() {
            std::thread::Builder::new()
                .name("herdr-mirror-ctl".to_string())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        tracing::warn!(line = %line, "writable mirror connection said");
                    }
                })?;
        }
        Ok(Self {
            child,
            stdin,
            controlling: terminal_id.to_owned(),
        })
    }

    /// The terminal this connection currently holds.
    pub(crate) fn controlling(&self) -> &str {
        &self.controlling
    }

    /// A claim whose far side has already gone, which is what every real one
    /// eventually becomes: the host restarts, the terminal ends, ssh drops, and
    /// this side only finds out on the next write.
    #[cfg(test)]
    pub(crate) fn test_already_dead(terminal_id: &str) -> Self {
        Self {
            child: std::process::Command::new("true")
                .spawn()
                .expect("spawning `true` should work"),
            stdin: None,
            controlling: terminal_id.to_owned(),
        }
    }

    /// Sends one request, moving the claim first if it is for another terminal.
    /// Points this connection at `terminal_id` if it is not already there.
    ///
    /// What is usually holding a mirrored pane is an older mirror of ours, and
    /// the machine being typed at should win.
    fn take_control_of(&mut self, terminal_id: &str) -> std::io::Result<()> {
        if self.controlling == terminal_id {
            return Ok(());
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("control stream stdin is closed"));
        };
        let line = serde_json::json!({
            "type": "terminal.control",
            "target": terminal_id,
            "takeover": true,
        })
        .to_string();
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        self.controlling = terminal_id.to_owned();
        Ok(())
    }

    pub(crate) fn send(
        &mut self,
        terminal_id: &str,
        request: &crate::pane::StreamedPaneRequest,
    ) -> std::io::Result<()> {
        self.take_control_of(terminal_id)?;
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("control stream stdin is closed"));
        };
        let line = match request {
            crate::pane::StreamedPaneRequest::Input(bytes) => serde_json::json!({
                "type": "terminal.input",
                "bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
            crate::pane::StreamedPaneRequest::Resize {
                rows,
                cols,
                cell_width_px,
                cell_height_px,
            } => serde_json::json!({
                "type": "terminal.resize",
                "cols": cols.max(&1),
                "rows": rows.max(&1),
                "cell_width_px": cell_width_px,
                "cell_height_px": cell_height_px,
            }),
            crate::pane::StreamedPaneRequest::Scroll { up, lines } => serde_json::json!({
                "type": "terminal.scroll",
                "direction": if *up { "up" } else { "down" },
                "lines": (*lines).max(1),
                "source": "wheel",
            }),
        }
        .to_string();
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    /// Hands the host an image to stage on itself and paste into the terminal
    /// this connection controls.
    ///
    /// The bytes go to the host rather than the path, because the path is the
    /// one thing that cannot travel: the file is staged on whichever machine
    /// took the paste, and an agent on another machine has nothing to open. The
    /// host stages its own copy and types its own path, which is the same thing
    /// it does for any attached client.
    pub(crate) fn send_image(
        &mut self,
        terminal_id: &str,
        extension: &str,
        data: &[u8],
    ) -> std::io::Result<()> {
        self.take_control_of(terminal_id)?;
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("control stream stdin is closed"));
        };
        let line = serde_json::json!({
            "type": "terminal.image",
            "extension": extension,
            "data": base64::engine::general_purpose::STANDARD.encode(data),
        })
        .to_string();
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    pub(crate) fn stop(mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn short_thread_name(target: &str) -> String {
    target
        .rsplit('@')
        .next()
        .unwrap_or(target)
        .chars()
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rebuilt pane needs a whole frame, and the host only sends one when it
    /// is asked for the set again -- so an unchanged set must still be askable.
    #[test]
    fn forgetting_the_set_makes_an_unchanged_one_worth_naming_again() {
        let mut stream = MirrorStream::test_without_a_host(1, None);
        let targets = vec![MirrorStreamTarget {
            terminal_id: "term_1".to_owned(),
            cols: 80,
            rows: 24,
            resize: false,
        }];
        stream.watching = targets.clone();
        assert!(
            stream.is_watching(&targets),
            "an unchanged set is normally left alone"
        );

        stream.forget_targets();

        assert!(
            !stream.is_watching(&targets),
            "after a pane is rebuilt the same set has to be named again"
        );
    }

    /// The difference between a host that cannot do this and one that is
    /// merely away is what it said, not that it said nothing.
    #[test]
    fn only_a_usage_complaint_means_the_host_is_too_old() {
        assert!(complaint_means_too_old(Some(
            "usage: herdr terminal session observe <target> [--cols N] [--rows N]"
        )));
        assert!(complaint_means_too_old(Some(
            "unexpected argument: observe-many"
        )));

        assert!(!complaint_means_too_old(None));
        assert!(!complaint_means_too_old(Some(
            "ssh: connect to host lute port 22: Connection refused"
        )));
        assert!(!complaint_means_too_old(Some(
            "Connection to lute closed by remote host."
        )));
    }

    #[test]
    fn the_observe_line_carries_a_size_per_terminal() {
        let line = observe_request_line(&[
            MirrorStreamTarget {
                terminal_id: "term_a".into(),
                cols: 100,
                rows: 30,
                resize: false,
            },
            MirrorStreamTarget {
                terminal_id: "term_b".into(),
                cols: 40,
                rows: 8,
                resize: false,
            },
        ]);
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(value["type"], "terminal.observe");
        assert_eq!(value["targets"][0]["target"], "term_a");
        assert_eq!(value["targets"][0]["cols"], 100);
        assert_eq!(value["targets"][1]["rows"], 8);
        assert!(line.ends_with('\n'), "the host reads a line at a time");
    }

    /// A pane can be laid out at zero height mid-resize, and a terminal asked
    /// for at zero rows renders nothing at all.
    #[test]
    fn an_empty_size_is_asked_for_as_one_cell() {
        let line = observe_request_line(&[MirrorStreamTarget {
            terminal_id: "term_a".into(),
            cols: 0,
            rows: 0,
            resize: false,
        }]);
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(value["targets"][0]["cols"], 1);
        assert_eq!(value["targets"][0]["rows"], 1);
    }

    /// A terminal rebuilt here is left out of the next set and then named, so
    /// the host starts it over; the telling after that is a plain one.
    #[test]
    fn a_rebuilt_terminal_is_dropped_then_named_once() {
        let target = |id: &str| MirrorStreamTarget {
            terminal_id: id.into(),
            cols: 80,
            rows: 24,
            resize: false,
        };
        let mut stream = MirrorStream::test_watching(vec![target("term-a"), target("term-b")]);

        stream.restart_target("term-b");
        assert!(!stream.is_watching(&[target("term-a"), target("term-b")]));
        stream
            .set_targets(vec![target("term-a"), target("term-b")])
            .expect("a test connection records the set");
        assert!(stream.fresh.is_empty(), "marked once, then plain");
    }

    #[test]
    fn a_modes_line_is_read_as_the_modes_it_names() {
        assert_eq!(
            parse_stream_line(
                r#"{"type":"terminal.modes","terminal_id":"term-a","target":"term-a","application_cursor":true,"kitty_keyboard_flags":31}"#
            ),
            Some(MirrorStreamLine::Modes {
                terminal_id: "term-a".into(),
                application_cursor: true,
                kitty_keyboard_flags: 31,
            })
        );
        assert_eq!(input_mode_bytes(true, 31), b"\x1b[?1h\x1b[=31;1u");
        assert_eq!(input_mode_bytes(false, 0), b"\x1b[?1l\x1b[=0;1u");
    }

    #[test]
    fn a_frame_line_decodes_to_its_terminal_and_bytes() {
        let parsed = parse_stream_line(
            r#"{"type":"terminal.frame","terminal_id":"term_a","target":"w1:p1","seq":3,"encoding":"ansi","width":40,"height":8,"full":false,"bytes":"aGVsbG8="}"#,
        )
        .expect("a frame");
        assert_eq!(
            parsed,
            MirrorStreamLine::Frame {
                terminal_id: "term_a".into(),
                bytes: b"hello".to_vec(),
            }
        );
    }

    #[test]
    fn an_ended_line_names_the_terminal_that_went() {
        let parsed = parse_stream_line(
            r#"{"type":"terminal.ended","terminal_id":"term_a","target":"w1:p1","reason":"terminal is gone"}"#,
        )
        .expect("an ended line");
        assert_eq!(
            parsed,
            MirrorStreamLine::Ended {
                terminal_id: "term_a".into(),
                reason: Some("terminal is gone".into()),
            }
        );
    }

    /// A host one build ahead may say things this one has no name for, and the
    /// panes it does understand should keep drawing.
    #[test]
    fn an_unknown_line_is_dropped_rather_than_ending_the_stream() {
        assert_eq!(parse_stream_line(r#"{"type":"terminal.future"}"#), None);
        assert_eq!(parse_stream_line("not json at all"), None);
        assert_eq!(
            parse_stream_line(r#"{"type":"terminal.frame","terminal_id":"a","bytes":"!!!"}"#),
            None,
            "a frame whose bytes will not decode is not a frame"
        );
    }
}
