//! `herdr agent feed` — a live push feed of this server's agent panes.
//!
//! Emits the same three-line block a remote-space poll would fetch (herdr
//! binary path, `workspace list` JSON, `pane list` JSON), once at startup and
//! again whenever anything relevant changes. A remote-space mirror consumes
//! this over SSH so it tracks the host live instead of polling on a timer.
//!
//! Structure changes (panes and workspaces appearing or leaving) come from
//! global event subscriptions; per-agent-pane status changes come from
//! `pane.agent_status_changed` subscriptions, which the feed rebuilds whenever
//! the set of agent panes changes.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::api::schema::{EmptyParams, Method, Request};
use crate::ipc::connect_local_stream;

/// Debounce window so a burst of related events produces one block, not many.
const COALESCE_WINDOW: Duration = Duration::from_millis(120);

/// How long a status watch waits on a quiet connection before checking whether
/// it is still wanted.
const WATCH_READ_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn run_agent_feed(args: &[String]) -> std::io::Result<i32> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!("herdr agent feed");
        eprintln!("  stream this server's agent panes as they change, for remote-space mirroring.");
        return Ok(0);
    }

    let herdr_bin = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| "herdr".to_string());

    let (tx, rx) = mpsc::channel::<Change>();

    // Global structure events wake the feed whenever the pane/workspace set
    // could have changed. Their payload is ignored; the feed re-reads state.
    spawn_structure_watch(tx.clone());

    // A slow heartbeat re-emits even if an event was missed — for example when
    // the server had no workspaces yet as the subscription connected. This is
    // the safety net under the event-driven path, so a mirror is never more
    // than one interval stale.
    spawn_heartbeat(tx.clone());

    let mut stdout = std::io::stdout().lock();
    let mut agent_pane_ids = emit_block(&mut stdout, &herdr_bin)?;
    let mut status_watch = StatusWatch::new(&agent_pane_ids, &tx);

    loop {
        // Block until something changes, then drain and coalesce the burst.
        if rx.recv().is_err() {
            return Ok(0);
        }
        while rx.recv_timeout(COALESCE_WINDOW).is_ok() {}

        let new_agent_pane_ids = emit_block(&mut stdout, &herdr_bin)?;
        if new_agent_pane_ids != agent_pane_ids {
            // The agent-pane set moved, so the per-pane status watches have to
            // follow it.
            agent_pane_ids = new_agent_pane_ids;
            status_watch = StatusWatch::new(&agent_pane_ids, &tx);
        }
        let _ = &status_watch;
    }
}

#[derive(Debug)]
struct Change;

/// Writes one three-line block and returns the current agent pane ids.
fn emit_block(stdout: &mut impl Write, herdr_bin: &str) -> std::io::Result<Vec<String>> {
    let workspaces = request_result_line(Method::WorkspaceList(EmptyParams::default()));
    let panes = request_result_line(Method::PaneList(Default::default()));
    let agent_pane_ids = agent_pane_ids_from(&panes);

    writeln!(stdout, "{herdr_bin}")?;
    writeln!(stdout, "{workspaces}")?;
    writeln!(stdout, "{panes}")?;
    stdout.flush()?;
    Ok(agent_pane_ids)
}

/// Sends a one-shot request and returns the raw JSON response line.
///
/// Errors are serialized as an error response so the consumer sees the same
/// shape it would from a failed poll rather than a dropped line.
fn request_result_line(method: Method) -> String {
    match super::send_request(&Request {
        id: "cli:agent:feed".into(),
        method,
    }) {
        Ok(value) => value.to_string(),
        Err(err) => serde_json::json!({
            "id": "cli:agent:feed",
            "error": { "code": "feed_request_failed", "message": err.to_string() }
        })
        .to_string(),
    }
}

fn agent_pane_ids_from(pane_list_line: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(pane_list_line) else {
        return Vec::new();
    };
    let Some(panes) = value
        .get("result")
        .and_then(|result| result.get("panes"))
        .and_then(|panes| panes.as_array())
    else {
        return Vec::new();
    };
    panes
        .iter()
        .filter(|pane| pane.get("agent").is_some() || pane.get("display_agent").is_some())
        .filter_map(|pane| pane.get("pane_id").and_then(|id| id.as_str()))
        .map(str::to_string)
        .collect()
}

/// How often the feed re-emits regardless of events, as a safety net.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// Signals a re-emit every [`HEARTBEAT`], so a missed event self-heals.
fn spawn_heartbeat(tx: mpsc::Sender<Change>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(HEARTBEAT);
        if tx.send(Change).is_err() {
            break;
        }
    });
}

/// Global structure-event subscription. Any event on it signals a change.
fn spawn_structure_watch(tx: mpsc::Sender<Change>) {
    // Never stopped: the structure watch lives as long as the feed itself.
    let subscriptions = serde_json::json!([
        { "type": "workspace.created" },
        { "type": "workspace.closed" },
        { "type": "workspace.renamed" },
        { "type": "pane.created" },
        { "type": "pane.closed" },
        { "type": "pane.exited" },
        { "type": "pane.agent_detected" },
    ]);
    spawn_subscription_watch(subscriptions, tx, Arc::new(AtomicBool::new(false)));
}

/// Per-agent-pane status subscriptions. Dropping this joins nothing — the
/// watcher threads exit on their own when their connection is closed, which the
/// server does when the client goes away; a stale thread just re-signals a
/// change that produces an identical block, which is harmless.
/// The per-pane status subscriptions, which end when this is dropped.
///
/// One connection and one thread per agent pane, rebuilt whenever the set of
/// agent panes changes -- which on a machine holding mirrors is often. Owning
/// them is the whole point: when this held nothing, every rebuild left its
/// threads and connections running for the life of the feed, and a host whose
/// mirrors were churning reached the descriptor limit inside a quarter of an
/// hour, taking the server it was watching along with it.
struct StatusWatch {
    stop: Arc<AtomicBool>,
}

impl StatusWatch {
    fn new(agent_pane_ids: &[String], tx: &mpsc::Sender<Change>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        for pane_id in agent_pane_ids {
            let subscriptions = serde_json::json!([
                { "type": "pane.agent_status_changed", "pane_id": pane_id }
            ]);
            spawn_subscription_watch(subscriptions, tx.clone(), Arc::clone(&stop));
        }
        Self { stop }
    }
}

impl Drop for StatusWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Opens a subscription connection and signals `tx` on every event line,
/// reconnecting if the server was not ready yet or the stream drops.
fn spawn_subscription_watch(
    subscriptions: serde_json::Value,
    tx: mpsc::Sender<Change>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if !watch_subscription_once(&subscriptions, &tx, &stop) {
                // tx is closed — the feed is shutting down.
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// One subscription connection. Returns false only when `tx` is closed, so the
/// caller stops; any connection error returns true so it retries.
fn watch_subscription_once(
    subscriptions: &serde_json::Value,
    tx: &mpsc::Sender<Change>,
    stop: &AtomicBool,
) -> bool {
    let Ok(mut stream) = connect_local_stream(&crate::api::socket_path()) else {
        return true;
    };
    // Read in short waits rather than blocking for ever, so a watch that is no
    // longer wanted lets go of its connection promptly instead of when its pane
    // next happens to do something.
    {
        use interprocess::local_socket::traits::Stream as _;
        let _ = stream.set_recv_timeout(Some(WATCH_READ_TIMEOUT));
    }
    let request = serde_json::json!({
        "id": "cli:agent:feed:sub",
        "method": "events.subscribe",
        "params": { "subscriptions": subscriptions },
    });
    if stream
        .write_all(format!("{request}\n").as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return true;
    }
    // Read a line at a time rather than through `lines()`, so a wait that times
    // out is a quiet moment -- keep the connection, keep any half-read line, and
    // go round again -- rather than a reason to reconnect every couple of
    // seconds. The only thing a timeout is for is noticing `stop`.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        match reader.read_line(&mut line) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return true,
        }
        if !line.ends_with('\n') {
            // A line still arriving; wait for the rest of it.
            continue;
        }
        let complete = std::mem::take(&mut line);
        let complete = complete.trim();
        if complete.is_empty() {
            continue;
        }
        // The first line is the subscription_started ack, not an event.
        if complete.contains("subscription_started") {
            continue;
        }
        if tx.send(Change).is_err() {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watches are rebuilt whenever the set of agent panes changes, which
    /// on a machine holding mirrors is constant. Dropping a generation has to
    /// end it: when this struct owned nothing, every rebuild left its threads
    /// and connections running for the life of the feed, and a host whose
    /// mirrors were churning reached the descriptor limit within the quarter
    /// hour -- with the server it was watching holding the other end of each.
    #[test]
    fn dropping_a_generation_of_watches_stops_them() {
        let (tx, _rx) = mpsc::channel::<Change>();
        let watch = StatusWatch::new(&[], &tx);
        let stop = Arc::clone(&watch.stop);
        assert!(!stop.load(Ordering::Relaxed));

        drop(watch);

        assert!(
            stop.load(Ordering::Relaxed),
            "a replaced generation of watches should be told to stop"
        );
    }
}
