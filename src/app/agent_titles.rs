//! Claude Code's session title, shown as the agent's name.
//!
//! Claude keeps a session's title in its transcript, not anywhere a hook or the
//! screen reports it: a `custom-title` record once someone runs `/rename`, and an
//! `ai-title` record it generates from the conversation otherwise. The terminal
//! title carries whichever is current too, but cannot say which one it is, and a
//! mirrored pane has no terminal title at all. So the server reads the
//! transcript itself, off the main loop, and reports the title as the pane's
//! `display_agent` the same way a hook would.
//!
//! Both records are written again every few dozen lines, so the end of the file
//! is enough; the whole file is read only when the end has neither.
//!
//! The session is the one the Claude hook reported when that integration is
//! installed. Without it, Claude's own `sessions/<pid>.json` names the session
//! each running Claude process has open, so the pane's Claude process is found
//! once and its file read after that.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::App;
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::terminal::TerminalId;

pub(crate) const AGENT_TITLE_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const AGENT_TITLE_SOURCE: &str = "herdr:claude-title";
const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;
const MAX_TITLE_CHARS: usize = 80;

#[derive(Default)]
pub(crate) struct AgentTitles {
    last_refresh: Option<Instant>,
    in_flight: bool,
    known: HashMap<TerminalId, KnownTitle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KnownTitle {
    session_id: String,
    /// The Claude process whose `sessions/<pid>.json` named the session, when
    /// no hook did.
    claude_pid: Option<u32>,
    transcript: Option<Transcript>,
    title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Transcript {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

#[derive(Debug)]
pub(crate) struct AgentTitleRefresh {
    terminal_id: TerminalId,
    pane_id: PaneId,
    known: KnownTitle,
}

struct AgentTitleJob {
    terminal_id: TerminalId,
    pane_id: PaneId,
    reported_session_id: Option<String>,
    shell_pid: Option<u32>,
    previous: Option<KnownTitle>,
}

impl App {
    pub(crate) fn start_agent_title_refresh_if_due(&mut self, now: Instant) {
        if self
            .agent_title_refresh_deadline()
            .is_none_or(|deadline| now < deadline)
        {
            return;
        }
        self.agent_titles.last_refresh = Some(now);
        let jobs = self.agent_title_jobs();
        let live: std::collections::HashSet<_> =
            jobs.iter().map(|job| job.terminal_id.clone()).collect();
        self.agent_titles
            .known
            .retain(|terminal_id, _| live.contains(terminal_id));
        if jobs.is_empty() {
            return;
        }
        let Ok(claude_dir) = crate::integration::claude_dir() else {
            return;
        };
        self.agent_titles.in_flight = true;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let projects = claude_dir.join("projects");
            let sessions = claude_dir.join("sessions");
            let refreshed = jobs
                .into_iter()
                .filter_map(|job| {
                    let (session_id, claude_pid) = match job.reported_session_id {
                        Some(session_id) => (session_id, None),
                        None => {
                            let (session_id, pid) = session_of_pane_process(
                                &sessions,
                                job.shell_pid?,
                                job.previous.as_ref().and_then(|known| known.claude_pid),
                            )?;
                            (session_id, Some(pid))
                        }
                    };
                    let previous = job.previous.filter(|known| known.session_id == session_id);
                    Some(AgentTitleRefresh {
                        known: refresh_title(&projects, &session_id, claude_pid, previous),
                        terminal_id: job.terminal_id,
                        pane_id: job.pane_id,
                    })
                })
                .collect();
            let _ = event_tx.blocking_send(AppEvent::AgentTitlesRefreshed { refreshed });
        });
    }

    pub(crate) fn agent_title_refresh_deadline(&self) -> Option<Instant> {
        let any_claude = self
            .state
            .terminals
            .values()
            .any(|terminal| terminal.effective_agent_label() == Some("claude"));
        if self.agent_titles.in_flight || !any_claude {
            return None;
        }
        Some(
            self.agent_titles
                .last_refresh
                .map_or_else(Instant::now, |last| last + AGENT_TITLE_REFRESH_INTERVAL),
        )
    }

    pub(crate) fn handle_agent_titles_refreshed(&mut self, refreshed: Vec<AgentTitleRefresh>) {
        self.agent_titles.in_flight = false;
        for AgentTitleRefresh {
            terminal_id,
            pane_id,
            known,
        } in refreshed
        {
            let still_same_session = self
                .state
                .terminals
                .get(&terminal_id)
                .filter(|terminal| terminal.effective_agent_label() == Some("claude"))
                .is_some_and(|terminal| {
                    reported_claude_session_id(terminal)
                        .is_none_or(|session_id| session_id == known.session_id)
                });
            if !still_same_session {
                continue;
            }
            let previous_title = self
                .agent_titles
                .known
                .insert(terminal_id, known.clone())
                .and_then(|previous| previous.title);
            if previous_title == known.title {
                continue;
            }
            tracing::info!(pane = pane_id.raw(), title = ?known.title, "claude session title changed");
            self.handle_internal_event(AppEvent::HookMetadataReported {
                pane_id,
                source: AGENT_TITLE_SOURCE.into(),
                agent_label: Some("claude".into()),
                applies_to_source: None,
                title: None,
                clear_display_agent: known.title.is_none(),
                display_agent: known.title,
                state_labels: HashMap::new(),
                clear_title: false,
                clear_state_labels: false,
                seq: None,
                ttl: None,
            });
        }
    }

    fn agent_title_jobs(&self) -> Vec<AgentTitleJob> {
        self.state
            .workspaces
            .iter()
            .filter(|workspace| workspace.remote_mirror.is_none())
            .flat_map(|workspace| {
                workspace.tabs.iter().flat_map(|tab| {
                    tab.panes.iter().filter_map(|(pane_id, pane)| {
                        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
                        if terminal.effective_agent_label() != Some("claude") {
                            return None;
                        }
                        let reported_session_id = reported_claude_session_id(terminal);
                        let shell_pid = self
                            .terminal_runtimes
                            .get(&terminal.id)
                            .and_then(|runtime| runtime.child_pid());
                        if reported_session_id.is_none() && shell_pid.is_none() {
                            return None;
                        }
                        Some(AgentTitleJob {
                            terminal_id: terminal.id.clone(),
                            pane_id: *pane_id,
                            reported_session_id,
                            shell_pid,
                            previous: self.agent_titles.known.get(&terminal.id).cloned(),
                        })
                    })
                })
            })
            .collect()
    }
}

/// The Claude session the Claude hook reported for a pane, if it is installed.
fn reported_claude_session_id(terminal: &crate::terminal::TerminalState) -> Option<String> {
    let from_hook = terminal
        .hook_authority
        .as_ref()
        .filter(|authority| authority.agent_label == "claude")
        .and_then(|authority| authority.session_ref.as_ref());
    let persisted = terminal
        .persisted_agent_session
        .as_ref()
        .filter(|session| session.agent == "claude")
        .map(|session| &session.session_ref);
    from_hook
        .or(persisted)
        .filter(|session_ref| session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id)
        .map(|session_ref| session_ref.value.clone())
}

/// The session named by `sessions/<pid>.json` for the Claude process in a
/// pane: the one found last time while its file still exists, else whichever
/// process in the pane's foreground job has a file.
fn session_of_pane_process(
    sessions: &Path,
    shell_pid: u32,
    last_claude_pid: Option<u32>,
) -> Option<(String, u32)> {
    if let Some(pid) = last_claude_pid {
        if let Some(session_id) = session_of_process(sessions, pid) {
            return Some((session_id, pid));
        }
    }
    let job = crate::detect::foreground_job(shell_pid)?;
    std::iter::once(shell_pid)
        .chain(job.processes.iter().map(|process| process.pid))
        .find_map(|pid| session_of_process(sessions, pid).map(|session_id| (session_id, pid)))
}

fn session_of_process(sessions: &Path, pid: u32) -> Option<String> {
    let bytes = std::fs::read(sessions.join(format!("{pid}.json"))).ok()?;
    let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if record.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(pid)) {
        return None;
    }
    record
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string)
}

fn refresh_title(
    projects: &Path,
    session_id: &str,
    claude_pid: Option<u32>,
    previous: Option<KnownTitle>,
) -> KnownTitle {
    let path = previous
        .as_ref()
        .and_then(|known| known.transcript.as_ref())
        .map(|transcript| transcript.path.clone())
        .filter(|path| path.is_file())
        .or_else(|| find_transcript(projects, session_id));
    let transcript = path.and_then(|path| {
        let metadata = std::fs::metadata(&path).ok()?;
        Some(Transcript {
            modified: metadata.modified().ok()?,
            len: metadata.len(),
            path,
        })
    });
    if let Some(previous) = previous.filter(|previous| previous.transcript == transcript) {
        return KnownTitle {
            claude_pid,
            ..previous
        };
    }
    let title = transcript
        .as_ref()
        .and_then(|transcript| read_title(&transcript.path, transcript.len));
    KnownTitle {
        session_id: session_id.to_string(),
        claude_pid,
        transcript,
        title,
    }
}

/// `projects/<directory the session started in, flattened>/<id>.jsonl`. The
/// flattening is Claude's to decide, so the directory is found, not derived.
fn find_transcript(projects: &Path, session_id: &str) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&file_name))
        .find(|path| path.is_file())
}

fn read_title(path: &Path, len: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    if len > TRANSCRIPT_TAIL_BYTES {
        file.seek(SeekFrom::Start(len - TRANSCRIPT_TAIL_BYTES))
            .ok()?;
        let mut tail = Vec::new();
        file.by_ref()
            .take(TRANSCRIPT_TAIL_BYTES)
            .read_to_end(&mut tail)
            .ok()?;
        if let Some(title) = title_from_transcript(&tail) {
            return Some(title);
        }
        file.seek(SeekFrom::Start(0)).ok()?;
    }
    let mut whole = Vec::new();
    file.read_to_end(&mut whole).ok()?;
    title_from_transcript(&whole)
}

/// The last `/rename` title, else the last generated one. A partial first line
/// from reading only the tail simply fails to parse.
fn title_from_transcript(bytes: &[u8]) -> Option<String> {
    let mut custom = None;
    let mut generated = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let is_custom = contains(line, br#""type":"custom-title""#);
        if !is_custom && !contains(line, br#""type":"ai-title""#) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let field = if is_custom { "customTitle" } else { "aiTitle" };
        let Some(title) = record.get(field).and_then(|value| value.as_str()) else {
            continue;
        };
        if let Some(title) = presentable(title) {
            if is_custom {
                custom = Some(title);
            } else {
                generated = Some(title);
            }
        }
    }
    custom.or(generated)
}

fn presentable(title: &str) -> Option<String> {
    let title: String = title
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TITLE_CHARS)
        .collect();
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, field: &str, title: &str) -> String {
        format!(r#"{{"type":"{kind}","{field}":"{title}","sessionId":"s"}}"#)
    }

    #[test]
    fn a_rename_wins_over_a_later_generated_title() {
        let transcript = [
            line("ai-title", "aiTitle", "Alleria GPU driver wedged"),
            r#"{"type":"user","message":"hi"}"#.to_string(),
            line("custom-title", "customTitle", "fix-alleria"),
            line("ai-title", "aiTitle", "Alleria GPU driver wedged"),
        ]
        .join("\n");

        assert_eq!(
            title_from_transcript(transcript.as_bytes()),
            Some("fix-alleria".into())
        );
    }

    #[test]
    fn the_latest_generated_title_is_used_until_a_rename() {
        let transcript = [
            line("ai-title", "aiTitle", "First guess"),
            line("ai-title", "aiTitle", "Better title"),
        ]
        .join("\n");

        assert_eq!(
            title_from_transcript(transcript.as_bytes()),
            Some("Better title".into())
        );
    }

    #[test]
    fn a_transcript_without_titles_has_none() {
        assert_eq!(
            title_from_transcript(br#"{"type":"user","message":"hi"}"#),
            None
        );
    }

    #[test]
    fn a_claude_process_file_names_its_session_only_for_its_own_pid() {
        let dir =
            std::env::temp_dir().join(format!("herdr-agent-title-pid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("4242.json"),
            r#"{"pid":4242,"sessionId":"56f81608","cwd":"/tmp"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("4243.json"), r#"{"pid":1,"sessionId":"stale"}"#).unwrap();

        assert_eq!(session_of_process(&dir, 4242), Some("56f81608".into()));
        assert_eq!(session_of_process(&dir, 4243), None);
        assert_eq!(session_of_process(&dir, 4244), None);
        assert_eq!(
            session_of_pane_process(&dir, u32::MAX, Some(4242)),
            Some(("56f81608".into(), 4242))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn titles_are_found_beyond_the_tail_when_the_tail_has_none() {
        let dir = std::env::temp_dir().join(format!("herdr-agent-title-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("-home-someone-project")).unwrap();
        let path = dir.join("-home-someone-project").join("abc.jsonl");
        let filler = format!(r#"{{"type":"user","message":"{}"}}"#, "x".repeat(1024));
        let mut transcript = line("custom-title", "customTitle", "early-name");
        for _ in 0..(TRANSCRIPT_TAIL_BYTES / 1024 + 8) {
            transcript.push('\n');
            transcript.push_str(&filler);
        }
        std::fs::write(&path, transcript).unwrap();

        let known = refresh_title(&dir, "abc", None, None);

        assert_eq!(known.title, Some("early-name".into()));
        assert_eq!(known.transcript.map(|t| t.path), Some(path));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unchanged_transcript_is_not_read_again() {
        let dir = std::env::temp_dir().join(format!("herdr-agent-title-rs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("p")).unwrap();
        let path = dir.join("p").join("abc.jsonl");
        std::fs::write(&path, line("ai-title", "aiTitle", "On disk")).unwrap();
        let first = refresh_title(&dir, "abc", None, None);
        let mut remembered = first.clone();
        remembered.title = Some("Remembered".into());

        let second = refresh_title(&dir, "abc", None, Some(remembered));

        assert_eq!(first.title, Some("On disk".into()));
        assert_eq!(second.title, Some("Remembered".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
