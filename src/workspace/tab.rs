use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

pub(crate) type DetachedPane = (PaneId, TerminalId);

pub(crate) struct MovedPane {
    pub pane_id: PaneId,
    pub pane_state: PaneState,
}

pub struct NewPane {
    pub pane_id: PaneId,
    pub terminal: TerminalState,
    pub runtime: TerminalRuntime,
}

enum SplitCommand<'a> {
    Shell {
        command: &'a str,
        launch_env: &'a PaneLaunchEnv,
    },
    Argv {
        argv: &'a [String],
        launch_env: &'a PaneLaunchEnv,
    },
}

/// Splits `root` into a grid of `count` tiles and returns them in reading
/// order, `root` first.
///
/// Columns are the smallest square that holds them all (two across for up to
/// four, three for up to nine) and the rows fill in from the top, so a grid
/// that is not full is short in its last row. Ratios are set so every row and
/// every tile within a row gets an equal share, despite the splits being
/// binary.
pub(crate) fn tile_grid(layout: &mut TileLayout, root: PaneId, count: usize) -> Vec<PaneId> {
    if count <= 1 {
        return vec![root];
    }
    let mut columns = 1;
    while columns * columns < count {
        columns += 1;
    }
    let rows = count.div_ceil(columns);

    let mut row_heads = vec![root];
    let mut rest = root;
    for row in 1..rows {
        let remaining = (rows - row + 1) as f32;
        match layout.split_pane(rest, Direction::Vertical, 1.0 / remaining) {
            Some(next) => {
                row_heads.push(next);
                rest = next;
            }
            None => break,
        }
    }

    let mut tiles = Vec::with_capacity(count);
    for (row, head) in row_heads.into_iter().enumerate() {
        let in_row = columns.min(count - row * columns);
        tiles.push(head);
        let mut rest = head;
        for column in 1..in_row {
            let remaining = (in_row - column + 1) as f32;
            match layout.split_pane(rest, Direction::Horizontal, 1.0 / remaining) {
                Some(next) => {
                    tiles.push(next);
                    rest = next;
                }
                None => break,
            }
        }
    }
    tiles
}

/// Lays `tiles` out afresh as the grid `tile_grid` makes for that many, in
/// the order given, keeping focus on the pane that had it.
///
/// Splitting one tile to make room would leave the grid lopsided -- one tile
/// halved while the rest stay whole -- so the shape is rebuilt with new ids
/// and each place then handed to the pane that belongs in it.
pub(crate) fn regrid(layout: &mut TileLayout, tiles: &[PaneId]) {
    let Some(&first) = tiles.first() else {
        return;
    };
    let focused = layout.focused();
    let (mut grid, root) = TileLayout::new();
    let places = tile_grid(&mut grid, root, tiles.len());
    if places.len() != tiles.len() {
        // tile_grid only comes up short if a split it just made vanished,
        // which cannot happen; keep the old layout rather than lose a pane.
        return;
    }
    for (place, tile) in places.iter().zip(tiles) {
        grid.rename_pane(*place, *tile);
    }
    grid.focus_pane(if tiles.contains(&focused) {
        focused
    } else {
        first
    });
    *layout = grid;
}

pub struct Tab {
    pub custom_name: Option<String>,
    /// Which remote pane this tab mirrors, when its space is a mirror.
    pub remote_mirror: Option<crate::workspace::RemoteMirrorTab>,
    pub number: usize,
    /// Identity source for this tab's pane tree.
    pub root_pane: PaneId,
    pub layout: TileLayout,
    /// Pane viewport state — always present, testable without PTYs.
    pub panes: HashMap<PaneId, PaneState>,
    #[cfg(test)]
    pub runtimes: HashMap<PaneId, TerminalRuntime>,
    pub zoomed: bool,
    pub events: mpsc::Sender<AppEvent>,
    pub(crate) render_notify: Arc<Notify>,
    pub(crate) render_dirty: Arc<RenderSignal>,
}

impl Tab {
    // Tab construction threads pane runtime geometry, host context, and render hooks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_runtime(
            number,
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            events,
            render_notify,
            render_dirty,
            None,
        )
    }

    // Command tab construction mirrors the shell tab runtime arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn new_argv_command(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_runtime(
            number,
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            events,
            render_notify,
            render_dirty,
            Some(argv),
        )
    }

    /// A tab whose single pane is fed by another host rather than a process.
    #[allow(clippy::too_many_arguments)]
    pub fn new_streamed(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
        requests: mpsc::Sender<crate::pane::StreamedPaneRequest>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (layout, root_id) = TileLayout::new();
        let runtime = TerminalRuntime::streamed(
            root_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events.clone(),
            render_notify.clone(),
            render_dirty.clone(),
            requests,
        )?;
        let terminal_id = TerminalId::alloc();
        let terminal = TerminalState::new(terminal_id.clone(), initial_cwd);
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));

        Ok((
            Self {
                custom_name: None,
                remote_mirror: None,
                number,
                root_pane: root_id,
                layout,
                panes,
                #[cfg(test)]
                runtimes: HashMap::new(),
                zoomed: false,
                events,
                render_notify,
                render_dirty,
            },
            terminal,
            runtime,
        ))
    }

    /// A tab tiled with one view per target, in reading order.
    ///
    /// Every tile is a view pane: it shows its target and holds a placeholder
    /// terminal of its own (see `view_tile`).
    #[allow(clippy::too_many_arguments)]
    pub fn new_wall(
        number: usize,
        targets: &[TerminalId],
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<(Self, Vec<NewPane>)> {
        let (layout, root_id) = TileLayout::new();
        let mut tab = Self {
            custom_name: None,
            remote_mirror: None,
            number,
            root_pane: root_id,
            layout,
            panes: HashMap::new(),
            #[cfg(test)]
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        let pane_ids = tile_grid(&mut tab.layout, root_id, targets.len());
        let mut tiles = Vec::with_capacity(pane_ids.len());
        for (pane_id, target) in pane_ids.into_iter().zip(targets) {
            tiles.push(tab.view_tile(
                pane_id,
                target,
                initial_cwd.clone(),
                rows,
                cols,
                host_terminal_theme,
            )?);
        }
        tab.layout.focus_pane(root_id);
        Ok((tab, tiles))
    }

    /// Adds a view of `target` to a tab of views, and lays every tile out
    /// again as the grid `tile_grid` makes for one more.
    ///
    /// The existing tiles keep what they show and their reading order, and
    /// the new one goes last. Focus stays where it was: adding to a wall is
    /// done while looking at something else, and moving focus would hand the
    /// typing size to the new tile's terminal.
    pub fn add_view(
        &mut self,
        target: &TerminalId,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<NewPane> {
        let pane_id = PaneId::alloc();
        let tile = self.view_tile(
            pane_id,
            target,
            initial_cwd,
            rows,
            cols,
            host_terminal_theme,
        )?;
        let mut order = self.layout.pane_ids();
        order.push(pane_id);
        regrid(&mut self.layout, &order);
        Ok(tile)
    }

    /// Makes `pane_id` a view of `target`, with a placeholder terminal of
    /// its own, and returns the placeholder for the caller to register.
    ///
    /// The placeholder is a streamed runtime nobody feeds, and what it would
    /// send back -- input, resizes -- goes into a channel whose receiver is
    /// dropped here, so it goes nowhere. Input never reaches it anyway: a
    /// view types into the terminal it shows.
    fn view_tile(
        &mut self,
        pane_id: PaneId,
        target: &TerminalId,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<NewPane> {
        let (requests, _) = mpsc::channel(1);
        let runtime = TerminalRuntime::streamed(
            pane_id,
            rows,
            cols,
            // The placeholder only ever holds a line saying its view has
            // ended, so it needs no scrollback to speak of.
            0,
            host_terminal_theme,
            self.events.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
            requests,
        )?;
        let terminal_id = TerminalId::alloc();
        let terminal = TerminalState::new(terminal_id.clone(), initial_cwd);
        self.panes
            .insert(pane_id, PaneState::view(terminal_id, target.clone()));
        Ok(NewPane {
            pane_id,
            terminal,
            runtime,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_runtime(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
        argv: Option<&[String]>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (layout, root_id) = TileLayout::new();
        let runtime = if let Some(argv) = argv {
            TerminalRuntime::spawn_argv_command(
                root_id,
                rows,
                cols,
                initial_cwd.clone(),
                argv,
                launch_env,
                crate::pane::AgentDetection::Enabled,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                events.clone(),
                render_notify.clone(),
                render_dirty.clone(),
            )?
        } else {
            TerminalRuntime::spawn(
                root_id,
                rows,
                cols,
                initial_cwd.clone(),
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                launch_env,
                events.clone(),
                render_notify.clone(),
                render_dirty.clone(),
            )?
        };

        let terminal_id = TerminalId::alloc();
        let terminal = match argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), initial_cwd).with_launch_argv(argv.to_vec())
            }
            None => TerminalState::new(terminal_id.clone(), initial_cwd),
        };
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));

        Ok((
            Self {
                custom_name: None,
                remote_mirror: None,
                number,
                root_pane: root_id,
                layout,
                panes,
                #[cfg(test)]
                runtimes: HashMap::new(),
                zoomed: false,
                events,
                render_notify,
                render_dirty,
            },
            terminal,
            runtime,
        ))
    }

    pub fn is_auto_named(&self) -> bool {
        self.custom_name.is_none()
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    #[cfg(test)]
    pub fn split_focused(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
    ) -> std::io::Result<NewPane> {
        self.split_pane_with_runtime(
            self.layout.focused(),
            true,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            None,
        )
    }

    pub fn split_focused_command(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        command: &str,
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    ) -> std::io::Result<NewPane> {
        self.split_pane_with_runtime(
            self.layout.focused(),
            true,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            Some(SplitCommand::Shell {
                command,
                launch_env,
            }),
        )
    }

    /// Split `target` with a shell pane. Focus moves to the new pane only when
    /// `focus_new_pane` is set; a spawn failure rolls the layout back without
    /// touching focus or its history.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn split_pane_shell(
        &mut self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
    ) -> std::io::Result<NewPane> {
        self.split_pane_with_runtime(
            target,
            focus_new_pane,
            direction,
            ratio,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            None,
        )
    }

    /// Split `target` with an argv-command pane. Same focus contract as
    /// `split_pane_shell`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn split_pane_argv(
        &mut self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    ) -> std::io::Result<NewPane> {
        self.split_pane_with_runtime(
            target,
            focus_new_pane,
            direction,
            ratio,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            Some(SplitCommand::Argv { argv, launch_env }),
        )
    }

    // Split construction threads geometry, host context, launch policy, and command state.
    #[allow(clippy::too_many_arguments)]
    fn split_pane_with_runtime(
        &mut self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        command: Option<SplitCommand<'_>>,
    ) -> std::io::Result<NewPane> {
        let Some(new_id) = self
            .layout
            .split_pane(target, direction, ratio.unwrap_or(0.5))
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "split target pane is not in the layout",
            ));
        };
        let actual_cwd =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let launch_argv = if let Some(SplitCommand::Argv { argv, .. }) = &command {
            Some((*argv).to_vec())
        } else {
            None
        };
        let runtime = match command {
            Some(SplitCommand::Shell {
                command,
                launch_env,
            }) => TerminalRuntime::spawn_shell_command(
                new_id,
                rows,
                cols,
                actual_cwd.clone(),
                command,
                launch_env,
                crate::pane::AgentDetection::Enabled,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                self.events.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            ),
            Some(SplitCommand::Argv { argv, launch_env }) => TerminalRuntime::spawn_argv_command(
                new_id,
                rows,
                cols,
                actual_cwd.clone(),
                argv,
                launch_env,
                crate::pane::AgentDetection::Enabled,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                self.events.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            ),
            None => TerminalRuntime::spawn(
                new_id,
                rows,
                cols,
                actual_cwd.clone(),
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                launch_env,
                self.events.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            ),
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(err) => {
                self.layout.close_pane(new_id);
                return Err(err);
            }
        };
        let terminal_id = TerminalId::alloc();
        let terminal = match launch_argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), actual_cwd).with_launch_argv(argv)
            }
            None => TerminalState::new(terminal_id.clone(), actual_cwd),
        };
        if focus_new_pane {
            self.layout.focus_pane(new_id);
        }
        self.panes.insert(new_id, PaneState::new(terminal_id));
        self.zoomed = false;
        Ok(NewPane {
            pane_id: new_id,
            terminal,
            runtime,
        })
    }

    #[cfg(test)]
    pub fn close_focused(&mut self) -> Option<DetachedPane> {
        let pane_id = self.layout.focused();
        self.detach_pane(pane_id)
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        self.detach_pane(pane_id)
    }

    pub fn remove_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        self.detach_pane(pane_id)
    }

    pub(crate) fn from_existing_pane(
        number: usize,
        custom_name: Option<String>,
        moved: MovedPane,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> Self {
        let mut panes = HashMap::new();
        let pane_id = moved.pane_id;
        panes.insert(pane_id, moved.pane_state);
        Self {
            custom_name,
            remote_mirror: None,
            number,
            root_pane: pane_id,
            layout: TileLayout::from_saved(Node::Pane(pane_id), pane_id),
            panes,
            #[cfg(test)]
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        }
    }

    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<MovedPane> {
        if !self.panes.contains_key(&pane_id) {
            return None;
        }

        if self.layout.pane_count() > 1 {
            let next_root = self.promoted_root_if_needed(pane_id);
            self.layout.close_pane(pane_id);
            if let Some(next_root) = next_root {
                self.root_pane = next_root;
            }
        }

        let pane_state = self.panes.remove(&pane_id)?;
        self.zoomed = false;
        Some(MovedPane {
            pane_id,
            pane_state,
        })
    }

    pub(crate) fn insert_existing_pane(
        &mut self,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> Result<PaneId, MovedPane> {
        if !self
            .layout
            .insert_pane_near(target_pane_id, moved.pane_id, direction, ratio, focus)
        {
            return Err(moved);
        }
        let pane_id = moved.pane_id;
        self.panes.insert(pane_id, moved.pane_state);
        self.zoomed = false;
        Ok(pane_id)
    }

    fn detach_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        if self.layout.pane_count() <= 1 {
            return None;
        }

        let next_root = self.promoted_root_if_needed(pane_id);

        self.layout.close_pane(pane_id);

        let pane = self.panes.remove(&pane_id)?;
        let terminal_id = pane.attached_terminal_id;
        self.zoomed = false;
        if let Some(next_root) = next_root {
            self.root_pane = next_root;
        }
        Some((pane_id, terminal_id))
    }

    fn promoted_root_if_needed(&self, closing: PaneId) -> Option<PaneId> {
        if self.root_pane != closing {
            return None;
        }
        self.layout.pane_ids().into_iter().find(|id| *id != closing)
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.panes
            .get(&pane_id)
            .map(|pane| &pane.attached_terminal_id)
    }

    pub fn cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(|rt| rt.cwd())
            .or_else(|| {
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.cwd.clone())
            })
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(|rt| rt.foreground_cwd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    /// Tile rects in reading order, for a grid laid out in a 120x40 area.
    fn grid_rects(count: usize) -> Vec<Rect> {
        let (mut layout, root) = TileLayout::new();
        let tiles = tile_grid(&mut layout, root, count);
        assert_eq!(tiles.len(), count);
        assert_eq!(tiles[0], root);
        let rects = layout.panes(Rect::new(0, 0, 120, 40));
        tiles
            .iter()
            .map(|id| {
                rects
                    .iter()
                    .find(|info| info.id == *id)
                    .map(|info| info.rect)
                    .expect("every tile is laid out")
            })
            .collect()
    }

    #[test]
    fn one_target_is_the_whole_tab() {
        assert_eq!(grid_rects(1), vec![Rect::new(0, 0, 120, 40)]);
    }

    #[test]
    fn four_targets_make_two_even_rows_of_two() {
        let rects = grid_rects(4);
        assert_eq!(rects[0], Rect::new(0, 0, 60, 20));
        assert_eq!(rects[1], Rect::new(60, 0, 60, 20));
        assert_eq!(rects[2], Rect::new(0, 20, 60, 20));
        assert_eq!(rects[3], Rect::new(60, 20, 60, 20));
    }

    #[test]
    fn a_grid_that_is_not_full_is_short_in_its_last_row() {
        let rects = grid_rects(5);
        // Three across, two rows: three tiles on top, two below.
        let top: Vec<_> = rects.iter().filter(|rect| rect.y == 0).collect();
        let bottom: Vec<_> = rects.iter().filter(|rect| rect.y == 20).collect();
        assert_eq!(top.len(), 3);
        assert_eq!(bottom.len(), 2);
        assert!(top.iter().all(|rect| rect.width == 40));
        assert!(bottom.iter().all(|rect| rect.width == 60));
    }

    #[test]
    fn tiles_read_left_to_right_then_top_to_bottom() {
        let rects = grid_rects(9);
        for (index, rect) in rects.iter().enumerate() {
            assert_eq!(rect.x / 40, (index % 3) as u16, "tile {index}");
            assert_eq!(rect.y / 13, (index / 3) as u16, "tile {index}");
        }
    }
}
