//! The running wall: its screen, its connections, and the loop between them.
//!
//! Threads only read -- stdin, the observing connection, the attach
//! connection, and the occasional target listing -- and hand what they read
//! to the loop here, which owns every piece of state and is the only thing
//! that writes to the server or the screen.

use std::collections::HashMap;
use std::io::{self, Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use tracing::{debug, info, warn};

use super::keys::{sgr_mouse_report, KeyAction, WallKeys};
use super::layout::{self, TileGeometry};
use super::picker::{self, Picker, PickerOutcome};
use super::render::{self, WallView};
use super::state::{plan_attach, AttachStep, AttachTarget, WallState};
use super::targets::{self, WallTarget};
use super::TargetSource;
use crate::ipc::LocalStream;
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientMessage, ObservedTarget, RenderEncoding,
    ServerMessage, MAX_FRAME_SIZE, MAX_GRAPHICS_FRAME_SIZE,
};
use crate::raw_input::RawInputEvent;
use crate::terminal::TerminalRuntime;

/// How long the loop waits for something to happen before looking at the
/// screen size and the clock again. Also the shortest gap between two frames.
const TICK: Duration = Duration::from_millis(16);

/// How long to leave a terminal alone after its attach dropped for no stated
/// reason, so a server that keeps closing it is not asked again every frame.
const ATTACH_RETRY: Duration = Duration::from_secs(2);

/// How long a passing message stays up.
const STATUS_FOR: Duration = Duration::from_secs(5);

enum WallEvent {
    Input(Vec<u8>),
    Observed(ServerMessage),
    ObserveClosed,
    Attach {
        generation: u64,
        message: ServerMessage,
    },
    AttachClosed {
        generation: u64,
    },
    Targets(Vec<WallTarget>),
}

/// Lets the loop take the keyboard away from the stdin thread while fzf has
/// it. The thread holds the lock only across one wait-and-read, so taking it
/// is never more than one short wait away.
#[derive(Default)]
struct InputGate {
    paused: AtomicBool,
    turn: Mutex<()>,
}

impl InputGate {
    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        // Waits out a read already under way, so none of fzf's keys end up
        // here.
        drop(self.turn.lock());
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }
}

/// The attach connection holding the active tile's terminal.
struct AttachLink {
    generation: u64,
    target: AttachTarget,
    stream: LocalStream,
    /// Frames have arrived since it last moved, so the terminal is held at
    /// the tile's size and can be drawn as it is.
    live: bool,
}

pub(super) fn run(source: TargetSource) -> io::Result<i32> {
    super::super::init_logging();
    crate::logging::startup("client");
    let config = crate::config::Config::load().config;
    let keys = WallKeys::from_config(&config);
    let palette = crate::app::palette_for_config(&config);
    let mouse_capture = config.ui.mouse_capture;
    let mouse_scroll_lines = config.ui.mouse_scroll_lines();
    let remote_image_paste_key = super::super::client_remote_image_paste_key(&config);
    let is_remote = super::super::is_remote_client_process();

    let socket_path = crate::server::socket_paths::client_socket_path();
    info!(path = %socket_path.display(), "opening wall");
    let observe = match open_connection(80, 24) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("herdr: {err}");
            return Ok(1);
        }
    };

    let (tx, rx) = mpsc::channel::<WallEvent>();
    spawn_observe_reader(&observe, tx.clone())?;

    let quit = Arc::new(AtomicBool::new(false));
    let handler_quit = quit.clone();
    if let Err(err) = ctrlc::set_handler(move || handler_quit.store(true, Ordering::Release)) {
        warn!(%err, "failed to install termination handler");
    }

    let screen = Screen::enter(mouse_capture)?;
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = ratatui::Terminal::new(backend)?;
    let gate = Arc::new(InputGate::default());
    spawn_stdin_reader(tx.clone(), gate.clone(), quit.clone());

    let mut wall = Wall {
        source,
        keys,
        palette,
        mouse_capture,
        mouse_scroll_lines,
        remote_image_paste_key,
        is_remote,
        screen: Some(screen),
        terminal,
        gate,
        tx,
        observe,
        observed: Vec::new(),
        state: WallState::default(),
        runtimes: HashMap::new(),
        attach: None,
        next_generation: 1,
        attach_retry_at: None,
        picker: None,
        picker_targets: Vec::new(),
        prefix_pending: false,
        status: None,
        listing: Arc::new(AtomicBool::new(false)),
        next_listing: Instant::now(),
        dirty: true,
        last_size: (0, 0),
    };

    // A wall starts empty, so the first thing it does is ask what to show.
    let outcome = match wall.open_picker(true) {
        Ok(true) => wall.run_loop(&rx, &quit),
        Ok(false) => Ok(Exit::Quit),
        Err(err) => Err(err),
    };

    wall.release();
    let restored = wall.screen.take().map(Screen::leave).unwrap_or(Ok(()));
    crate::logging::shutdown("client");
    if let Err(err) = restored {
        warn!(%err, "failed to restore the terminal");
    }
    match outcome {
        Ok(Exit::Quit) => Ok(0),
        Ok(Exit::Lost(reason)) => {
            eprintln!("herdr: {reason}");
            Ok(1)
        }
        Err(err) => {
            eprintln!("herdr: {err}");
            Ok(1)
        }
    }
}

enum Exit {
    Quit,
    Lost(String),
}

struct Wall {
    source: TargetSource,
    keys: WallKeys,
    palette: crate::app::state::Palette,
    mouse_capture: bool,
    mouse_scroll_lines: usize,
    remote_image_paste_key: Option<(KeyCode, KeyModifiers)>,
    is_remote: bool,
    screen: Option<Screen>,
    terminal: ratatui::Terminal<CrosstermBackend<io::Stdout>>,
    gate: Arc<InputGate>,
    tx: Sender<WallEvent>,
    observe: LocalStream,
    /// The set the observer was last told, so it is only told again when it
    /// changes.
    observed: Vec<ObservedTarget>,
    state: WallState,
    /// The local copy of each tile's terminal, by terminal id.
    runtimes: HashMap<String, TerminalRuntime>,
    attach: Option<AttachLink>,
    next_generation: u64,
    attach_retry_at: Option<Instant>,
    picker: Option<Picker>,
    /// What the open built-in picker's lines stand for.
    picker_targets: Vec<WallTarget>,
    prefix_pending: bool,
    status: Option<(String, Instant)>,
    /// A listing is under way on its own thread.
    listing: Arc<AtomicBool>,
    next_listing: Instant,
    dirty: bool,
    last_size: (u16, u16),
}

impl Wall {
    fn run_loop(&mut self, rx: &Receiver<WallEvent>, quit: &AtomicBool) -> io::Result<Exit> {
        let mut last_draw = Instant::now() - TICK;
        loop {
            if quit.load(Ordering::Acquire) {
                return Ok(Exit::Quit);
            }
            match rx.recv_timeout(TICK) {
                Ok(event) => {
                    if let Some(exit) = self.handle(event)? {
                        return Ok(exit);
                    }
                    // Take whatever else has queued before drawing, so a burst
                    // of frames costs one draw.
                    while let Ok(event) = rx.try_recv() {
                        if let Some(exit) = self.handle(event)? {
                            return Ok(exit);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(Exit::Quit),
            }
            self.reconcile()?;
            self.maybe_list_targets();
            if self
                .status
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= STATUS_FOR)
            {
                self.status = None;
                self.dirty = true;
            }
            if self.dirty && last_draw.elapsed() >= TICK {
                self.draw()?;
                last_draw = Instant::now();
            }
        }
    }

    fn screen_area(&self) -> Rect {
        let (cols, rows) = crossterm::terminal::size().unwrap_or(self.last_size);
        Rect::new(0, 0, cols, rows)
    }

    fn geometry(&self) -> Vec<TileGeometry> {
        layout::tile_geometry(self.screen_area(), self.state.tiles().len())
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
        self.dirty = true;
    }

    fn handle(&mut self, event: WallEvent) -> io::Result<Option<Exit>> {
        match event {
            WallEvent::Input(data) => return self.handle_input(data),
            WallEvent::Observed(message) => return Ok(self.handle_observed(message)),
            WallEvent::ObserveClosed => {
                return Ok(Some(Exit::Lost("lost connection to server".to_owned())))
            }
            WallEvent::Attach {
                generation,
                message,
            } => self.handle_attach(generation, message),
            WallEvent::AttachClosed { generation } => {
                if self
                    .attach
                    .as_ref()
                    .is_some_and(|link| link.generation == generation)
                {
                    debug!(generation, "attach connection closed");
                    self.attach = None;
                    self.attach_retry_at = Some(Instant::now() + ATTACH_RETRY);
                    self.dirty = true;
                }
            }
            WallEvent::Targets(fresh) => {
                if self.state.refresh_targets(&fresh) {
                    self.dirty = true;
                }
            }
        }
        Ok(None)
    }

    fn handle_observed(&mut self, message: ServerMessage) -> Option<Exit> {
        match message {
            ServerMessage::ObservedTerminal(observed) => {
                if let Some(runtime) = self.runtimes.get(&observed.terminal_id) {
                    runtime.apply_streamed_bytes(&observed.frame.bytes);
                    self.dirty = true;
                }
            }
            ServerMessage::ObservedTerminalModes {
                terminal_id,
                application_cursor,
                kitty_keyboard_flags,
                ..
            } => {
                // Applied to the local copy, which is what keys for the tile
                // are encoded from.
                if let Some(runtime) = self.runtimes.get(&terminal_id) {
                    runtime.apply_streamed_bytes(&crate::remote::mirror_stream::input_mode_bytes(
                        application_cursor,
                        kitty_keyboard_flags,
                    ));
                }
            }
            ServerMessage::ObservedTerminalEnded {
                terminal_id,
                target,
                reason,
            } => {
                let ended = self.state.mark_ended(&terminal_id, reason.clone())
                    || self.state.mark_ended(&target, reason);
                if ended {
                    self.dirty = true;
                }
            }
            ServerMessage::ServerShutdown { reason } => {
                return Some(Exit::Lost(match reason {
                    Some(reason) => format!("server closed the wall: {reason}"),
                    None => "server shut down".to_owned(),
                }));
            }
            _ => {}
        }
        None
    }

    fn handle_attach(&mut self, generation: u64, message: ServerMessage) {
        let Some(link) = self.attach.as_mut() else {
            return;
        };
        if link.generation != generation {
            return;
        }
        match message {
            ServerMessage::Terminal(_) => {
                if !link.live {
                    link.live = true;
                    self.dirty = true;
                }
            }
            ServerMessage::Clipboard { data } => {
                super::super::forward_clipboard(&data);
            }
            ServerMessage::TerminalBell { count } => {
                let _ = crate::terminal_effects::write_terminal_bells(&mut io::stdout(), count);
            }
            ServerMessage::ServerShutdown { reason } => {
                let terminal_id = link.target.terminal_id.clone();
                let reason = reason.unwrap_or_default();
                info!(terminal_id = %terminal_id, reason = %reason, "attach ended");
                self.attach = None;
                if reason.contains("already has an attached client")
                    || reason.contains("taken over")
                {
                    // Something else is holding it -- `herdr focus`, or a
                    // mirror typing into it. Taking it would end that client,
                    // and the wall promises to leave other clients alone, so
                    // the tile stays watchable and stops asking.
                    self.state.mark_view_only(&terminal_id);
                    self.set_status("that terminal is attached elsewhere; showing it view only");
                } else if reason != "detached" {
                    self.attach_retry_at = Some(Instant::now() + ATTACH_RETRY);
                }
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Brings the connections in line with the state: the observer told the
    /// current set, a local copy for each terminal in it, and the attach
    /// holding the active tile.
    fn reconcile(&mut self) -> io::Result<()> {
        let area = self.screen_area();
        if (area.width, area.height) != self.last_size {
            self.last_size = (area.width, area.height);
            self.dirty = true;
        }
        let geometry = self.geometry();

        let set = self
            .state
            .observe_set(&geometry, (area.width.max(1), area.height.max(1)));
        for (terminal_id, cols, rows) in &set {
            match self.runtimes.get(terminal_id) {
                Some(runtime) => {
                    if runtime.current_size() != (*rows, *cols) {
                        runtime.resize(*rows, *cols, 0, 0);
                    }
                }
                None => {
                    let runtime = local_copy(*cols, *rows)?;
                    self.runtimes.insert(terminal_id.clone(), runtime);
                }
            }
        }
        self.runtimes
            .retain(|terminal_id, _| set.iter().any(|(id, _, _)| id == terminal_id));
        let targets: Vec<ObservedTarget> = set
            .into_iter()
            .map(|(target, cols, rows)| ObservedTarget {
                target,
                cols,
                rows,
                resize: false,
            })
            .collect();
        if targets != self.observed {
            write_message(
                &mut self.observe,
                &ClientMessage::ObserveTerminals {
                    targets: targets.clone(),
                },
            )?;
            self.observed = targets;
        }

        let desired = self.state.desired_attach(&geometry);
        let retry_due = self.attach_retry_at.is_none_or(|at| Instant::now() >= at);
        if self.attach.is_none() && !retry_due {
            return Ok(());
        }
        let steps = plan_attach(
            self.attach.as_ref().map(|link| &link.target),
            desired.as_ref(),
        );
        for step in steps {
            self.apply_attach_step(step);
        }
        Ok(())
    }

    fn apply_attach_step(&mut self, step: AttachStep) {
        self.dirty = true;
        match step {
            AttachStep::Open(target) => {
                let generation = self.next_generation;
                self.next_generation += 1;
                match open_attach(&target, generation, self.tx.clone()) {
                    Ok(stream) => {
                        self.attach_retry_at = None;
                        self.attach = Some(AttachLink {
                            generation,
                            target,
                            stream,
                            live: false,
                        });
                    }
                    Err(err) => {
                        warn!(%err, "could not open the attach connection");
                        self.attach_retry_at = Some(Instant::now() + ATTACH_RETRY);
                        self.set_status(format!("could not attach: {err}"));
                    }
                }
            }
            AttachStep::Retarget(terminal_id) => {
                let Some(link) = self.attach.as_mut() else {
                    return;
                };
                let moved = write_message(
                    &mut link.stream,
                    &ClientMessage::ControlTerminal {
                        target: terminal_id.clone(),
                        takeover: false,
                    },
                );
                link.target.terminal_id = terminal_id;
                link.live = false;
                if moved.is_err() {
                    self.attach = None;
                }
            }
            AttachStep::Resize { cols, rows } => {
                let Some(link) = self.attach.as_mut() else {
                    return;
                };
                let resized = write_message(
                    &mut link.stream,
                    &ClientMessage::Resize {
                        cols,
                        rows,
                        cell_width_px: 0,
                        cell_height_px: 0,
                    },
                );
                link.target.cols = cols;
                link.target.rows = rows;
                if resized.is_err() {
                    self.attach = None;
                }
            }
            AttachStep::Close => {
                if let Some(mut link) = self.attach.take() {
                    // The server answers a detach by closing the connection,
                    // which ends the reader thread too.
                    let _ = write_message(&mut link.stream, &ClientMessage::Detach);
                }
            }
        }
    }

    /// Lets go of everything the wall holds on the server.
    fn release(&mut self) {
        if let Some(mut link) = self.attach.take() {
            let _ = write_message(&mut link.stream, &ClientMessage::Detach);
        }
        let _ = write_message(&mut self.observe, &ClientMessage::Detach);
    }

    fn maybe_list_targets(&mut self) {
        if self.state.is_empty()
            || Instant::now() < self.next_listing
            || self.listing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.next_listing = Instant::now() + self.source.refresh_interval();
        let source = self.source.clone();
        let tx = self.tx.clone();
        let listing = self.listing.clone();
        let spawned = std::thread::Builder::new()
            .name("herdr-wall-list".to_owned())
            .spawn(move || {
                match source.list() {
                    Ok(targets) => {
                        let _ = tx.send(WallEvent::Targets(targets));
                    }
                    Err(err) => debug!(%err, "wall target refresh failed"),
                }
                listing.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.listing.store(false, Ordering::Release);
        }
    }

    /// Opens the picker over the wall. With fzf on PATH the screen is handed
    /// to fzf and the choice applied straight away; otherwise the built-in
    /// list opens and the loop drives it. Returns false when the wall should
    /// end: the very first pick was cancelled, so nothing was ever shown. A
    /// first list that cannot be made at all is an error; a later one only
    /// says so.
    fn open_picker(&mut self, first: bool) -> io::Result<bool> {
        self.prefix_pending = false;
        let listed = match self.source.list() {
            Ok(listed) => listed,
            Err(err) if first => {
                return Err(io::Error::other(format!(
                    "could not list what to show: {err}"
                )))
            }
            Err(err) => {
                self.set_status(format!("could not list targets: {err}"));
                return Ok(true);
            }
        };
        if listed.is_empty() {
            if first {
                return Err(io::Error::other(
                    "nothing to show: no agents, spaces or panes",
                ));
            }
            self.set_status("nothing to show: no agents, spaces or panes");
            return Ok(true);
        }
        self.state.refresh_targets(&listed);
        // Ordered here as well as where the list is made, so a list from a far
        // side one build behind still puts its agents first.
        let mut listed = listed;
        targets::order_by_recency(&mut listed);
        let now_ms = targets::unix_now_ms();

        if let Some(fzf) = picker::find_fzf() {
            // fzf takes the whole screen width less its pointer, marker and
            // scrollbar columns.
            let width = crossterm::terminal::size()
                .map(|(cols, _)| usize::from(cols))
                .unwrap_or(100)
                .saturating_sub(4);
            let input = targets::fzf_input(&listed, now_ms, width);
            let chosen = self.with_screen_handed_over(|| picker::run_fzf(&fzf, &input));
            let chosen = match chosen {
                Ok(Some(output)) => targets::parse_fzf_output(&output, listed.len()),
                Ok(None) => Vec::new(),
                Err(err) => {
                    self.set_status(format!("fzf failed: {err}"));
                    Vec::new()
                }
            };
            if chosen.is_empty() {
                return Ok(!(first && self.state.is_empty()));
            }
            for index in chosen {
                if let Some(target) = listed.get(index) {
                    self.state.add(target.clone());
                }
            }
            self.dirty = true;
            return Ok(true);
        }

        self.picker = Some(Picker::new(targets::picker_rows(&listed, now_ms)));
        self.picker_targets = listed;
        self.dirty = true;
        Ok(true)
    }

    /// Runs `run` with the terminal back in its normal state and the stdin
    /// thread kept off it, then takes the terminal back and redraws.
    fn with_screen_handed_over<T>(&mut self, run: impl FnOnce() -> T) -> T {
        self.gate.pause();
        if let Some(screen) = self.screen.take() {
            if let Err(err) = screen.leave() {
                warn!(%err, "could not hand the terminal over");
            }
        }
        let result = run();
        match Screen::enter(self.mouse_capture) {
            Ok(screen) => self.screen = Some(screen),
            Err(err) => warn!(%err, "could not take the terminal back"),
        }
        let _ = self.terminal.clear();
        self.gate.resume();
        self.dirty = true;
        result
    }

    fn handle_input(&mut self, data: Vec<u8>) -> io::Result<Option<Exit>> {
        if self.picker.is_some() {
            return Ok(self.handle_picker_input(&data));
        }

        let attached = self.attach.as_ref().is_some_and(|link| link.live);
        if attached {
            if super::super::should_bridge_clipboard_image_paste(
                &data,
                super::super::should_read_local_clipboard_on_empty_paste(self.is_remote, true),
                self.remote_image_paste_key,
            ) {
                if let Some(image) = crate::platform::read_clipboard_image() {
                    self.send_image(image);
                    return Ok(None);
                }
                info!("clipboard image paste trigger received, but local clipboard has no image");
            }
            if let Some(image) =
                super::super::read_image_file_from_terminal_drop(&data, self.is_remote)
            {
                self.send_image(image);
                return Ok(None);
            }
        }

        for event in crate::raw_input::parse_raw_input_bytes_sync(&data) {
            match event {
                RawInputEvent::Key(key) => {
                    if let Some(exit) = self.handle_key(key) {
                        return Ok(Some(exit));
                    }
                }
                RawInputEvent::Text(text) => {
                    self.prefix_pending = false;
                    self.send_input(text.as_str().as_bytes().to_vec());
                }
                RawInputEvent::Paste(text) => {
                    self.prefix_pending = false;
                    let mut bytes = b"\x1b[200~".to_vec();
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    self.send_input(bytes);
                }
                RawInputEvent::Mouse(mouse) => self.handle_mouse(mouse),
                _ => {}
            }
        }
        Ok(None)
    }

    fn handle_picker_input(&mut self, data: &[u8]) -> Option<Exit> {
        let picker = self.picker.as_mut()?;
        let mut outcome = PickerOutcome::Open;
        for event in crate::raw_input::parse_raw_input_bytes_sync(data) {
            outcome = match event {
                RawInputEvent::Key(key) => picker.handle_key(&key),
                RawInputEvent::Text(text) => {
                    picker.push_text(text.as_str());
                    PickerOutcome::Open
                }
                RawInputEvent::Paste(text) => {
                    picker.push_text(&text);
                    PickerOutcome::Open
                }
                _ => PickerOutcome::Open,
            };
            if outcome != PickerOutcome::Open {
                break;
            }
        }
        self.dirty = true;
        match outcome {
            PickerOutcome::Open => None,
            PickerOutcome::Picked(index) => {
                self.picker = None;
                if let Some(target) = self.picker_targets.get(index) {
                    self.state.add(target.clone());
                }
                self.picker_targets.clear();
                None
            }
            PickerOutcome::Cancelled => {
                self.picker = None;
                self.picker_targets.clear();
                // Cancelling the picker an empty wall opened with means there
                // was never anything to show.
                self.state.is_empty().then_some(Exit::Quit)
            }
        }
    }

    fn handle_key(&mut self, key: crate::input::TerminalKey) -> Option<Exit> {
        let action = self.keys.classify(&key, self.prefix_pending);
        if !matches!(action, KeyAction::Ignore | KeyAction::Forward) || self.prefix_pending {
            self.dirty = true;
        }
        match action {
            KeyAction::Forward => self.send_key(key),
            KeyAction::Ignore => {}
            KeyAction::BeginPrefix => self.prefix_pending = true,
            KeyAction::SendPrefix => {
                self.prefix_pending = false;
                self.send_key(key);
            }
            KeyAction::ForwardAfterPrefix => {
                self.prefix_pending = false;
                self.send_key(self.keys.prefix_key());
                self.send_key(key);
            }
            KeyAction::OpenPicker => {
                self.prefix_pending = false;
                // Not the first pick, so this only ever reports trouble in
                // the status line.
                let _ = self.open_picker(false);
            }
            KeyAction::Focus(direction) => {
                self.prefix_pending = false;
                let geometry = self.geometry();
                self.state.focus_direction(&geometry, direction);
            }
            KeyAction::Cycle(step) => {
                self.prefix_pending = false;
                self.state.cycle(step);
            }
            KeyAction::CloseTile => {
                self.prefix_pending = false;
                self.state.close_active();
            }
            KeyAction::Quit => return Some(Exit::Quit),
        }
        None
    }

    /// Sends a key to the active tile, encoded the way its program asked keys
    /// to be encoded. Unmodified page keys go as a scroll request instead, as
    /// they do for `herdr focus`, so the server can scroll the terminal's own
    /// history when the program there would not.
    fn send_key(&mut self, key: crate::input::TerminalKey) {
        let Some(link) = self.attach.as_ref() else {
            return;
        };
        let Some(runtime) = self.runtimes.get(&link.target.terminal_id) else {
            return;
        };
        let bytes = runtime.encode_terminal_key(key.clone());
        if bytes.is_empty() {
            return;
        }
        let page = match key.code {
            KeyCode::PageUp => Some(AttachScrollDirection::Up),
            KeyCode::PageDown => Some(AttachScrollDirection::Down),
            _ => None,
        };
        if let Some(direction) = page.filter(|_| key.modifiers.is_empty()) {
            if key.kind == KeyEventKind::Release {
                return;
            }
            let lines = link.target.rows.saturating_sub(1).max(1);
            self.send_attach(ClientMessage::AttachScroll {
                source: AttachScrollSource::PageKey { input: bytes },
                direction,
                lines,
                column: None,
                row: None,
                modifiers: KeyModifiers::empty().bits(),
            });
            return;
        }
        self.send_input(bytes);
    }

    fn send_input(&mut self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        self.send_attach(ClientMessage::Input { data });
    }

    fn send_image(&mut self, image: crate::platform::ClipboardImage) {
        if image.bytes.len() > protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD {
            self.set_status("that image is too large to paste");
            return;
        }
        self.send_attach(ClientMessage::ClipboardImage {
            extension: image.extension.to_owned(),
            data: image.bytes,
        });
    }

    fn send_attach(&mut self, message: ClientMessage) {
        let Some(link) = self.attach.as_mut() else {
            return;
        };
        if let Err(err) = write_message(&mut link.stream, &message) {
            warn!(%err, "attach connection write failed");
            self.attach = None;
            self.attach_retry_at = Some(Instant::now() + ATTACH_RETRY);
            self.dirty = true;
        }
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let geometry = self.geometry();
        let Some(index) = layout::tile_at(&geometry, mouse.column, mouse.row) else {
            return;
        };
        if self.state.active() != Some(index) {
            // A click on another tile makes it the active one and goes no
            // further: the program there did not see where it landed before.
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.state.activate(index);
                self.prefix_pending = false;
                self.dirty = true;
            }
            return;
        }
        let content = geometry[index].content;
        let inside = mouse.column >= content.x
            && mouse.column < content.right()
            && mouse.row >= content.y
            && mouse.row < content.bottom();
        if !inside || !self.attach.as_ref().is_some_and(|link| link.live) {
            return;
        }
        let column = mouse.column - content.x;
        let row = mouse.row - content.y;
        let direction = match mouse.kind {
            MouseEventKind::ScrollUp => Some(AttachScrollDirection::Up),
            MouseEventKind::ScrollDown => Some(AttachScrollDirection::Down),
            _ => None,
        };
        if let Some(direction) = direction {
            self.send_attach(ClientMessage::AttachScroll {
                source: AttachScrollSource::Wheel,
                direction,
                lines: self.mouse_scroll_lines.clamp(1, usize::from(u16::MAX)) as u16,
                column: Some(column),
                row: Some(row),
                modifiers: mouse.modifiers.bits(),
            });
            return;
        }
        if let Some(report) = sgr_mouse_report(mouse.kind, mouse.modifiers, column, row) {
            self.send_input(report);
        }
    }

    fn draw(&mut self) -> io::Result<()> {
        self.dirty = false;
        let geometry = self.geometry();
        let active_live = self.attach.as_ref().is_some_and(|link| {
            link.live
                && self
                    .state
                    .active_tile()
                    .is_some_and(|tile| tile.target.terminal_id == link.target.terminal_id)
        });
        let add = self.keys.add_label();
        let empty_hint = format!("{add} adds a tile · {}+q quits", self.keys.prefix_label());
        let picker_hint = "type to filter · ↑↓ move · enter add · esc cancel";
        let view = WallView {
            area: self.screen_area(),
            tiles: self.state.tiles(),
            geometry: &geometry,
            runtimes: &self.runtimes,
            active: self.state.active(),
            active_live,
            prefix_pending: self.prefix_pending,
            palette: &self.palette,
            picker: self.picker.as_ref(),
            picker_hint,
            empty_hint: &empty_hint,
            status: self.status.as_ref().map(|(message, _)| message.as_str()),
        };
        self.terminal.draw(|frame| render::render(frame, &view))?;
        Ok(())
    }
}

/// A local terminal fed by the observer, at the size it is being streamed at.
fn local_copy(cols: u16, rows: u16) -> io::Result<TerminalRuntime> {
    // Nothing reads these: the copy has no process to report on, and its
    // only requests -- a resize when the streamed area grows -- are the
    // wall's own, already sent on the observer.
    let (events, _) = tokio::sync::mpsc::channel(1);
    let (requests, _) = tokio::sync::mpsc::channel(1);
    TerminalRuntime::streamed(
        crate::layout::PaneId::from_raw(0),
        rows,
        cols,
        0,
        crate::terminal_theme::TerminalTheme::default(),
        events,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(crate::render_signal::RenderSignal::new()),
        requests,
    )
}

fn write_message(stream: &mut LocalStream, message: &ClientMessage) -> io::Result<()> {
    super::super::write_to_server(stream, message)
}

/// Connects to the server's client socket as a terminal-session client.
fn open_connection(cols: u16, rows: u16) -> Result<LocalStream, super::super::ClientError> {
    let socket_path = crate::server::socket_paths::client_socket_path();
    let mut stream = crate::ipc::connect_local_stream(&socket_path)
        .map_err(super::super::ClientError::ConnectionFailed)?;
    super::super::do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        false,
        RenderEncoding::TerminalAnsi,
        true,
    )?;
    stream
        .set_nonblocking(false)
        .map_err(super::super::ClientError::ConnectionFailed)?;
    Ok(stream)
}

fn spawn_observe_reader(stream: &LocalStream, tx: Sender<WallEvent>) -> io::Result<()> {
    let mut reader = stream.try_clone()?;
    std::thread::Builder::new()
        .name("herdr-wall-observe".to_owned())
        .spawn(move || loop {
            match protocol::read_message::<_, ServerMessage>(&mut reader, MAX_GRAPHICS_FRAME_SIZE) {
                Ok(message) => {
                    if tx.send(WallEvent::Observed(message)).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    debug!(%err, "observe connection ended");
                    let _ = tx.send(WallEvent::ObserveClosed);
                    return;
                }
            }
        })?;
    Ok(())
}

/// Opens an attach connection at `target`'s size and points it at its
/// terminal, without taking it from anything already holding it.
fn open_attach(
    target: &AttachTarget,
    generation: u64,
    tx: Sender<WallEvent>,
) -> io::Result<LocalStream> {
    let mut stream = open_connection(target.cols, target.rows)
        .map_err(|err| io::Error::other(err.to_string()))?;
    write_message(
        &mut stream,
        &ClientMessage::ControlTerminal {
            target: target.terminal_id.clone(),
            takeover: false,
        },
    )?;
    let mut reader = stream.try_clone()?;
    std::thread::Builder::new()
        .name("herdr-wall-attach".to_owned())
        .spawn(move || loop {
            match protocol::read_message::<_, ServerMessage>(&mut reader, MAX_FRAME_SIZE) {
                Ok(message) => {
                    if tx
                        .send(WallEvent::Attach {
                            generation,
                            message,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => {
                    let _ = tx.send(WallEvent::AttachClosed { generation });
                    return;
                }
            }
        })?;
    Ok(stream)
}

/// Reads stdin in framed chunks -- whole escape sequences, a lone escape only
/// once nothing follows it -- and passes them to the loop, except while the
/// gate is closed.
fn spawn_stdin_reader(tx: Sender<WallEvent>, gate: Arc<InputGate>, quit: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("herdr-wall-stdin".to_owned())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
            let mut scratch = [0u8; 4096];
            while !quit.load(Ordering::Acquire) {
                let turn = match gate.turn.lock() {
                    Ok(turn) => turn,
                    Err(_) => return,
                };
                if gate.paused.load(Ordering::Acquire) {
                    drop(turn);
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let mut chunks = Vec::new();
                match super::super::input::poll_read_ready(0, 50) {
                    Some(true) => match reader.read(&mut scratch) {
                        Ok(0) => return,
                        Ok(read) => chunks.extend(framer.push(&scratch[..read])),
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    },
                    Some(false) if framer.has_pending_input() => {
                        chunks.extend(framer.flush_timeout());
                    }
                    Some(false) | None => {}
                }
                if framer.has_pending_input()
                    && super::super::input::poll_read_ready(
                        0,
                        crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
                    ) == Some(false)
                {
                    chunks.extend(framer.flush_timeout());
                }
                drop(turn);
                for chunk in chunks {
                    if tx.send(WallEvent::Input(chunk)).is_err() {
                        return;
                    }
                }
            }
        });
    if let Err(err) = spawned {
        warn!(%err, "could not start reading the keyboard");
    }
}

/// The wall's hold on the terminal: raw mode, the alternate screen, the mouse,
/// bracketed paste and the keyboard protocol, given back on `leave`.
struct Screen {
    reset_modify_other_keys: bool,
    left: bool,
}

impl Screen {
    fn enter(mouse_capture: bool) -> io::Result<Self> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen)?;
        crate::terminal_modes::clear_host_mouse_reporting(&mut stdout)?;
        super::super::set_mouse_capture(mouse_capture, false)?;
        execute!(stdout, EnableBracketedPaste)?;
        // Negotiated as `herdr focus` does, so modified keys -- the image
        // paste key, a ctrl+shift binding -- arrive as themselves.
        super::super::push_keyboard_enhancement_flags()?;
        let modify_other_keys = crate::input::host_modify_other_keys_mode();
        if let Some(mode) = modify_other_keys {
            stdout.write_all(mode.set_sequence())?;
        }
        execute!(stdout, DisableLineWrap)?;
        stdout.flush()?;
        Ok(Self {
            reset_modify_other_keys: modify_other_keys.is_some(),
            left: false,
        })
    }

    fn leave(mut self) -> io::Result<()> {
        self.left = true;
        restore_screen(self.reset_modify_other_keys)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if !self.left {
            let _ = restore_screen(self.reset_modify_other_keys);
        }
    }
}

fn restore_screen(reset_modify_other_keys: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    if reset_modify_other_keys {
        let _ = stdout.write_all(b"\x1b[>4;0m");
    }
    let _ = super::super::pop_keyboard_enhancement_flags();
    let _ = execute!(stdout, EnableLineWrap, DisableBracketedPaste);
    let _ = super::super::set_mouse_capture(false, false);
    let _ = crate::terminal_modes::clear_host_mouse_reporting(&mut stdout);
    let _ = execute!(stdout, LeaveAlternateScreen);
    let raw = disable_raw_mode();
    stdout.write_all(b"\x1b[?25h\x1b[0 q")?;
    stdout.flush()?;
    raw
}
