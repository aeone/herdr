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
        "terminal.closed" => Some(MirrorStreamLine::Closed {
            reason: value
                .get("reason")
                .and_then(|reason| reason.as_str())
                .map(str::to_owned),
        }),
        _ => None,
    }
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
    /// Frames this connection has carried, which is how a host that cannot do
    /// this at all is told from one whose link dropped.
    frames: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The last thing the far side said on stderr, which is where a host too
    /// old for this command prints its usage.
    complaint: std::sync::Arc<std::sync::Mutex<Option<String>>>,
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
            frames,
            complaint,
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
            frames: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(frames)),
            complaint: std::sync::Arc::new(std::sync::Mutex::new(complaint.map(str::to_owned))),
        }
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

    pub(crate) fn set_targets(&mut self, targets: Vec<MirrorStreamTarget>) -> std::io::Result<()> {
        let line = observe_request_line(&targets);
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("observe stream stdin is closed"));
        };
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        self.watching = targets;
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
    pub(crate) fn spawn(
        space: &RemoteSpaceConfig,
        terminal_id: &str,
        remote_herdr: &str,
    ) -> std::io::Result<Self> {
        let argv = crate::remote::spaces::control_argv(space, terminal_id, remote_herdr);
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

    /// Sends one request, moving the claim first if it is for another terminal.
    pub(crate) fn send(
        &mut self,
        terminal_id: &str,
        request: &crate::pane::StreamedPaneRequest,
    ) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("control stream stdin is closed"));
        };
        if self.controlling != terminal_id {
            // Takeover: what is usually holding a mirrored pane is an older
            // mirror of ours, and the machine being typed at should win.
            let line = serde_json::json!({
                "type": "terminal.control",
                "target": terminal_id,
                "takeover": true,
            })
            .to_string();
            stdin.write_all(line.as_bytes())?;
            stdin.write_all(b"\n")?;
            self.controlling = terminal_id.to_owned();
        }
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
        }
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
            },
            MirrorStreamTarget {
                terminal_id: "term_b".into(),
                cols: 40,
                rows: 8,
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
        }]);
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(value["targets"][0]["cols"], 1);
        assert_eq!(value["targets"][0]["rows"], 1);
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
