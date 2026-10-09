//! Walls: workspaces tiled with views of terminals that live elsewhere.
//!
//! A view pane shows another pane's terminal -- a local one, or a mirror of a
//! remote one, since a mirror is a local terminal fed over a connection. It
//! keeps a placeholder terminal of its own so that everything that assumes a
//! terminal belongs to one pane goes on being true; only drawing, sizing and
//! typing follow the view (see `PaneState::view_of`).
//!
//! Walls are runtime-only and are not persisted, the same as mirrors: the
//! terminals a wall shows need not come back under the same ids.

use crate::api::schema::{Method, WorkspaceWallAddParams, WorkspaceWallParams};
use crate::app::state::AppState;
use crate::app::{App, Mode};
use crate::terminal::TerminalId;
use crate::workspace::Workspace;

impl AppState {
    /// The terminal a wall naming `terminal_id` should show.
    ///
    /// A view's placeholder stands for the terminal the view shows, so naming
    /// one -- a wall of a wall, or a wall naming its own tiles -- shows that
    /// terminal rather than an empty placeholder. A view that has ended shows
    /// nothing any more, and neither does an id no pane holds.
    pub(crate) fn resolve_wall_target(&self, terminal_id: &str) -> Option<TerminalId> {
        self.workspaces.iter().find_map(|workspace| {
            workspace.tabs.iter().find_map(|tab| {
                let pane = tab
                    .panes
                    .values()
                    .find(|pane| pane.attached_terminal_id.as_str() == terminal_id)?;
                match &pane.view_of {
                    Some(target) => Some(target.clone()),
                    None if workspace.wall.is_some() => None,
                    None => Some(pane.attached_terminal_id.clone()),
                }
            })
        })
    }

    /// Ends every view of a terminal that is going away, and gives back its
    /// size if a view held it. Returns the placeholders of the views that
    /// ended, so the caller can say so in them.
    pub(crate) fn end_views_of(&mut self, terminal_id: &TerminalId) -> Vec<TerminalId> {
        let mut ended = Vec::new();
        for workspace in &mut self.workspaces {
            if workspace.wall.is_none() {
                continue;
            }
            for tab in &mut workspace.tabs {
                for pane in tab.panes.values_mut() {
                    if pane.view_of.as_ref() == Some(terminal_id) {
                        pane.view_of = None;
                        ended.push(pane.attached_terminal_id.clone());
                    }
                }
            }
        }
        if self
            .view_size_claim
            .as_ref()
            .is_some_and(|claim| &claim.target == terminal_id)
        {
            self.view_size_claim = None;
        }
        ended
    }

    /// Whether some view on screen shows `terminal_id`.
    ///
    /// A terminal's own pane is often hidden while a wall shows it, and output
    /// from a hidden pane is not drawn. Without this, every tile of a wall
    /// would freeze on whatever it showed when its terminal's home was last
    /// on screen.
    pub(crate) fn view_on_screen_shows(&self, terminal_id: &TerminalId) -> bool {
        let Some(tab) = self
            .active
            .and_then(|ws_idx| self.workspaces.get(ws_idx))
            .filter(|workspace| workspace.wall.is_some())
            .and_then(|workspace| workspace.active_tab())
        else {
            return false;
        };
        if tab.zoomed {
            return tab
                .panes
                .get(&tab.layout.focused())
                .is_some_and(|pane| pane.view_of.as_ref() == Some(terminal_id));
        }
        tab.panes
            .values()
            .any(|pane| pane.view_of.as_ref() == Some(terminal_id))
    }
}

/// Where a view's placeholder says it is. Nothing runs in it, but a
/// terminal has to be somewhere, and a wall's views come from anywhere.
fn wall_cwd() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// What a view says in place of a terminal that has gone.
const VIEW_ENDED_MESSAGE: &[u8] =
    b"\r\n\x1b[2m[view ended: the terminal it showed has closed]\x1b[0m";

impl App {
    /// Opens a workspace tiled with a view of each terminal, in order.
    ///
    /// Fails, opening nothing, when the list is empty or names a terminal no
    /// pane holds. Errors are `(code, message)` for the API to pass on.
    pub(crate) fn create_wall(
        &mut self,
        terminal_ids: &[String],
        label: Option<String>,
        focus: bool,
    ) -> Result<usize, (String, String)> {
        if terminal_ids.is_empty() {
            return Err((
                "invalid_params".into(),
                "a wall needs at least one terminal".into(),
            ));
        }
        let mut targets = Vec::with_capacity(terminal_ids.len());
        for terminal_id in terminal_ids {
            let Some(target) = self.state.resolve_wall_target(terminal_id) else {
                return Err((
                    "terminal_not_found".into(),
                    format!("terminal {terminal_id} not found"),
                ));
            };
            targets.push(target);
        }

        let (rows, cols) = self.state.estimate_pane_size();
        let (mut workspace, placeholders) = Workspace::new_wall(
            targets,
            wall_cwd(),
            rows,
            cols,
            self.state.host_terminal_theme,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )
        .map_err(|err| ("workspace_create_failed".to_string(), err.to_string()))?;
        workspace.custom_name = Some(label.unwrap_or_else(|| "wall".to_string()));
        for (terminal, runtime) in placeholders {
            self.terminal_runtimes.insert(terminal.id.clone(), runtime);
            self.state.terminals.insert(terminal.id.clone(), terminal);
        }
        let pane_ids = workspace.tabs[0].layout.pane_ids();
        let workspace_id = workspace.id.clone();
        let root_pane = workspace.tabs[0].root_pane.raw();
        self.state.workspaces.push(workspace);
        let idx = self.state.workspaces.len() - 1;
        for pane_id in pane_ids {
            self.state.remove_alias_shadowed_by_new_pane(pane_id);
        }
        crate::logging::workspace_created(&workspace_id, root_pane);
        if focus || self.state.active.is_none() {
            self.state.switch_workspace(idx);
            self.state.mode = Mode::Terminal;
        }
        Ok(idx)
    }

    /// Adds a tile to the wall at `ws_idx` for each terminal, in order, and
    /// returns the new tiles. The wall is re-tiled into the grid for its new
    /// count, and focus stays on the tile that had it.
    ///
    /// Terminals are resolved as `create_wall` resolves them, and all of them
    /// before anything is added, so a bad id adds nothing. A terminal the wall
    /// already shows gets a second tile: it is no harder to close one than to
    /// explain why nothing happened. Errors are `(code, message)` for the API
    /// to pass on.
    pub(crate) fn add_to_wall(
        &mut self,
        ws_idx: usize,
        terminal_ids: &[String],
    ) -> Result<Vec<crate::layout::PaneId>, (String, String)> {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return Err(("workspace_not_found".into(), "workspace not found".into()));
        };
        if workspace.wall.is_none() {
            return Err((
                "not_a_wall".into(),
                format!("workspace {} is not a wall", workspace.id),
            ));
        }
        if terminal_ids.is_empty() {
            return Err((
                "invalid_params".into(),
                "nothing to add: no terminal ids given".into(),
            ));
        }
        let mut targets = Vec::with_capacity(terminal_ids.len());
        for terminal_id in terminal_ids {
            let Some(target) = self.state.resolve_wall_target(terminal_id) else {
                return Err((
                    "terminal_not_found".into(),
                    format!("terminal {terminal_id} not found"),
                ));
            };
            targets.push(target);
        }

        let (rows, cols) = self.state.estimate_pane_size();
        let theme = self.state.host_terminal_theme;
        let mut added = Vec::with_capacity(targets.len());
        for target in &targets {
            let tile = self.state.workspaces[ws_idx]
                .add_wall_view(target, wall_cwd(), rows, cols, theme)
                .map_err(|err| ("wall_add_failed".to_string(), err.to_string()))?;
            self.terminal_runtimes
                .insert(tile.terminal.id.clone(), tile.runtime);
            self.state
                .terminals
                .insert(tile.terminal.id.clone(), tile.terminal);
            self.state.remove_alias_shadowed_by_new_pane(tile.pane_id);
            added.push(tile.pane_id);
        }
        Ok(added)
    }

    /// Carries out what the navigator asked to put on a wall, through the
    /// API so it is announced like any other change. Returns whether there
    /// was anything to do.
    pub(crate) fn apply_requested_wall_add(&mut self) -> bool {
        let Some(request) = self.state.request_wall_add.take() else {
            return false;
        };
        let terminal_ids = vec![request.terminal_id.to_string()];
        let response = match request.wall_workspace_id {
            Some(workspace_id) => self.dispatch_runtime_mutation(
                "tui.workspace.wall_add",
                Method::WorkspaceWallAdd(WorkspaceWallAddParams {
                    workspace_id: Some(workspace_id),
                    terminal_ids,
                }),
            ),
            None => self.dispatch_runtime_mutation(
                "tui.workspace.create_wall",
                Method::WorkspaceCreateWall(WorkspaceWallParams {
                    terminal_ids,
                    focus: true,
                    label: None,
                }),
            ),
        };
        if let Ok(error) = serde_json::from_str::<crate::api::schema::ErrorResponse>(&response) {
            tracing::warn!(
                code = %error.error.code,
                message = %error.error.message,
                "could not put the chosen terminal on a wall"
            );
        }
        true
    }

    /// Ends the views of a terminal that is shutting down, leaving a line in
    /// each saying why it went still.
    pub(crate) fn end_views_of_terminal(&mut self, terminal_id: &TerminalId) {
        for placeholder in self.state.end_views_of(terminal_id) {
            if let Some(runtime) = self.terminal_runtimes.get(&placeholder) {
                runtime.apply_streamed_bytes(VIEW_ENDED_MESSAGE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::{NavigatorPurpose, NavigatorTarget};
    use crate::layout::PaneId;
    use crate::terminal::TerminalRuntime;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    const AREA: Rect = Rect::new(0, 0, 120, 40);

    /// Two terminals with homes of their own, and a wall of both.
    struct Fixture {
        state: AppState,
        target: TerminalId,
        other: TerminalId,
        tiles: Vec<PaneId>,
    }

    impl Fixture {
        fn new() -> Self {
            let mut home = Workspace::test_new("home");
            let home_pane = home.tabs[0].root_pane;
            // In the tab rather than the workspace, so the background layout
            // pass finds it the way it finds a real runtime.
            home.tabs[0].runtimes.insert(
                home_pane,
                TerminalRuntime::test_with_screen_bytes(80, 24, b"home"),
            );
            let target = home.terminal_id(home_pane).expect("home terminal").clone();

            let mut elsewhere = Workspace::test_new("elsewhere");
            let other_pane = elsewhere.tabs[0].root_pane;
            elsewhere.tabs[0].runtimes.insert(
                other_pane,
                TerminalRuntime::test_with_screen_bytes(80, 24, b"other"),
            );
            let other = elsewhere
                .terminal_id(other_pane)
                .expect("other terminal")
                .clone();

            let wall = Workspace::test_wall("wall", &[target.clone(), other.clone()]);
            let tiles = wall.tabs[0].layout.pane_ids();

            let mut state = AppState::test_new();
            state.workspaces = vec![home, elsewhere, wall];
            state.active = Some(0);
            state.mode = Mode::Terminal;
            Self {
                state,
                target,
                other,
                tiles,
            }
        }

        fn layout(&mut self) {
            crate::ui::compute_view(&mut self.state, AREA);
        }

        fn size_of(&self, terminal_id: &TerminalId) -> (u16, u16) {
            self.state
                .runtime_for_terminal(
                    &crate::terminal::TerminalRuntimeRegistry::new(),
                    terminal_id,
                )
                .expect("runtime")
                .current_size()
        }

        fn show_wall(&mut self, mode: Mode, focused_tile: usize) {
            self.state.active = Some(2);
            self.state.mode = mode;
            let tile = self.tiles[focused_tile];
            self.state.workspaces[2].tabs[0].layout.focus_pane(tile);
        }

        fn tile_size(&self, tile: usize) -> (u16, u16) {
            let info = self
                .state
                .view
                .pane_infos
                .iter()
                .find(|info| info.id == self.tiles[tile])
                .expect("tile is laid out");
            (info.inner_rect.height, info.inner_rect.width)
        }

        /// The size the terminal's own pane lays it out at.
        fn home_size(&mut self) -> (u16, u16) {
            let (active, mode) = (self.state.active, self.state.mode);
            self.state.active = Some(0);
            self.layout();
            let size = self.size_of(&self.target);
            self.state.active = active;
            self.state.mode = mode;
            size
        }
    }

    #[tokio::test]
    async fn a_wall_looked_at_but_not_typed_into_leaves_its_terminals_alone() {
        let mut fixture = Fixture::new();
        let home = fixture.home_size();

        fixture.show_wall(Mode::Navigate, 0);
        fixture.layout();

        assert_eq!(fixture.state.view_size_claim, None);
        assert_eq!(fixture.size_of(&fixture.target), home);
        assert_ne!(fixture.tile_size(0), home);
    }

    #[tokio::test]
    async fn the_view_being_typed_into_takes_its_terminal_s_size() {
        let mut fixture = Fixture::new();
        let home = fixture.home_size();

        fixture.show_wall(Mode::Terminal, 0);
        fixture.layout();

        assert_eq!(fixture.size_of(&fixture.target), fixture.tile_size(0));
        // The other tile is only watched, so its terminal keeps the size its
        // own pane gives it.
        assert_eq!(fixture.size_of(&fixture.other), home);
    }

    #[tokio::test]
    async fn moving_on_from_a_view_gives_the_size_back() {
        let mut fixture = Fixture::new();
        let home = fixture.home_size();
        fixture.show_wall(Mode::Terminal, 0);
        fixture.layout();
        assert_eq!(fixture.size_of(&fixture.target), fixture.tile_size(0));

        fixture.show_wall(Mode::Terminal, 1);
        fixture.layout();

        assert_eq!(fixture.size_of(&fixture.target), home);
        assert_eq!(fixture.size_of(&fixture.other), fixture.tile_size(1));
    }

    #[tokio::test]
    async fn leaving_the_wall_gives_the_size_back() {
        let mut fixture = Fixture::new();
        let home = fixture.home_size();
        fixture.show_wall(Mode::Terminal, 0);
        fixture.layout();

        fixture.state.active = Some(1);
        fixture.layout();

        assert_eq!(fixture.state.view_size_claim, None);
        assert_eq!(fixture.size_of(&fixture.target), home);
    }

    #[tokio::test]
    async fn pressing_the_prefix_keeps_the_size_where_it_is() {
        let mut fixture = Fixture::new();
        fixture.home_size();
        fixture.show_wall(Mode::Terminal, 0);
        fixture.layout();
        let tile = fixture.tile_size(0);

        fixture.state.mode = Mode::Prefix;
        fixture.layout();

        assert!(fixture.state.view_size_claim.is_some());
        assert_eq!(fixture.size_of(&fixture.target), tile);
    }

    #[tokio::test]
    async fn a_terminal_held_by_an_attach_client_is_not_resized_by_a_view() {
        let mut fixture = Fixture::new();
        let home = fixture.home_size();
        fixture
            .state
            .direct_attach_resize_locks
            .insert(fixture.target.clone());

        fixture.show_wall(Mode::Terminal, 0);
        fixture.layout();

        assert_eq!(fixture.size_of(&fixture.target), home);
    }

    #[tokio::test]
    async fn the_same_terminal_in_two_tiles_is_sized_by_the_one_typed_into() {
        let mut fixture = Fixture::new();
        let home = fixture.home_size();
        let twice =
            Workspace::test_wall("twice", &[fixture.target.clone(), fixture.target.clone()]);
        fixture.tiles = twice.tabs[0].layout.pane_ids();
        fixture.state.workspaces[2] = twice;

        fixture.show_wall(Mode::Terminal, 1);
        fixture.layout();
        assert_eq!(fixture.size_of(&fixture.target), fixture.tile_size(1));

        fixture.show_wall(Mode::Navigate, 0);
        fixture.state.view_size_claim = None;
        fixture.layout();
        assert_eq!(fixture.size_of(&fixture.target), home);
    }

    #[tokio::test]
    async fn a_long_line_is_drawn_rewrapped_to_the_tile_and_anchored_to_its_bottom() {
        let runtime = TerminalRuntime::test_with_screen_bytes(20, 5, b"abcdefghijklmnopqrstuvwxyz");
        let mut terminal = Terminal::new(TestBackend::new(10, 4)).expect("test terminal");
        terminal
            .draw(|frame| runtime.render_rewrapped(frame, Rect::new(0, 0, 10, 4)))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..4)
            .map(|y| {
                (0..10)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(
            rows,
            vec!["          ", "abcdefghij", "klmnopqrst", "uvwxyz    "]
        );
    }

    #[tokio::test]
    async fn a_watched_view_is_drawn_through_the_whole_ui_without_resizing() {
        let mut fixture = Fixture::new();
        fixture
            .state
            .runtime_for_terminal(
                &crate::terminal::TerminalRuntimeRegistry::new(),
                &fixture.target,
            )
            .expect("runtime")
            .test_process_pty_bytes(b"\r\nthe target's output");
        fixture.home_size();
        fixture.show_wall(Mode::Navigate, 1);
        fixture.layout();

        let mut terminal =
            Terminal::new(TestBackend::new(AREA.width, AREA.height)).expect("test terminal");
        terminal
            .draw(|frame| crate::ui::render(&fixture.state, frame))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("the target's output"), "{text}");
    }

    #[tokio::test]
    async fn a_view_on_screen_shows_its_terminal_while_the_terminal_s_pane_is_hidden() {
        let mut fixture = Fixture::new();
        assert!(!fixture.state.view_on_screen_shows(&fixture.target));
        fixture.show_wall(Mode::Navigate, 0);
        assert!(fixture.state.view_on_screen_shows(&fixture.target));
        assert!(fixture.state.view_on_screen_shows(&fixture.other));

        fixture.state.workspaces[2].tabs[0].zoomed = true;
        fixture.show_wall(Mode::Navigate, 1);
        assert!(!fixture.state.view_on_screen_shows(&fixture.target));
        assert!(fixture.state.view_on_screen_shows(&fixture.other));
    }

    #[tokio::test]
    async fn a_view_s_placeholder_stands_for_what_it_shows() {
        let fixture = Fixture::new();
        let placeholder = fixture.state.workspaces[2].tabs[0].panes[&fixture.tiles[0]]
            .attached_terminal_id
            .clone();

        assert_eq!(
            fixture.state.resolve_wall_target(placeholder.as_str()),
            Some(fixture.target.clone())
        );
        assert_eq!(
            fixture.state.resolve_wall_target(fixture.target.as_str()),
            Some(fixture.target.clone())
        );
        assert_eq!(fixture.state.resolve_wall_target("term_missing"), None);
    }

    #[tokio::test]
    async fn a_terminal_that_goes_ends_its_views_and_gives_back_its_size() {
        let mut fixture = Fixture::new();
        fixture.home_size();
        fixture.show_wall(Mode::Terminal, 0);
        fixture.layout();
        assert!(fixture.state.view_size_claim.is_some());

        let ended = fixture.state.end_views_of(&fixture.target);

        assert_eq!(ended.len(), 1);
        assert_eq!(fixture.state.view_size_claim, None);
        let wall = &fixture.state.workspaces[2];
        assert_eq!(wall.tabs[0].panes[&fixture.tiles[0]].view_of, None);
        assert_eq!(
            wall.tabs[0].panes[&fixture.tiles[1]].view_of,
            Some(fixture.other.clone())
        );
        // An ended view stands for nothing, so a wall cannot be made of it.
        assert_eq!(fixture.state.resolve_wall_target(ended[0].as_str()), None);
    }

    #[tokio::test]
    async fn a_wall_of_a_wall_shows_the_terminals_themselves() {
        let mut app = crate::app::tests::test_app();
        let fixture = Fixture::new();
        let placeholder = fixture.state.workspaces[2].tabs[0].panes[&fixture.tiles[1]]
            .attached_terminal_id
            .clone();
        app.state.workspaces = fixture.state.workspaces;

        let index = app
            .create_wall(&[placeholder.to_string()], Some("again".into()), false)
            .expect("wall");

        let wall = &app.state.workspaces[index];
        assert_eq!(wall.custom_name.as_deref(), Some("again"));
        let root = wall.tabs[0].root_pane;
        assert_eq!(
            wall.tabs[0].panes[&root].view_of,
            Some(fixture.other.clone())
        );
    }

    /// An app holding the fixture's workspaces: two homes and a wall of both,
    /// with the wall active.
    fn app_with_wall() -> (App, Fixture) {
        let mut app = crate::app::tests::test_app();
        let mut fixture = Fixture::new();
        app.state.workspaces = std::mem::take(&mut fixture.state.workspaces);
        app.state.active = Some(2);
        app.state.mode = Mode::Terminal;
        (app, fixture)
    }

    /// Selects the navigator row for `target` and chooses it, as enter does.
    fn choose_in_navigator(app: &mut App, target: NavigatorTarget) -> bool {
        let rows = app.state.navigator_rows_from(&app.terminal_runtimes);
        app.state.navigator.selected = rows
            .iter()
            .position(|row| row.target == target)
            .expect("the navigator lists the target");
        app.state
            .accept_navigator_selection_from(&app.terminal_runtimes)
    }

    fn pane_row(app: &App, ws_idx: usize) -> NavigatorTarget {
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        NavigatorTarget::Pane {
            ws_idx,
            tab_idx: 0,
            pane_id,
        }
    }

    fn shown_by(workspace: &Workspace) -> Vec<Option<TerminalId>> {
        let tab = &workspace.tabs[0];
        tab.layout
            .pane_ids()
            .iter()
            .map(|pane_id| tab.panes[pane_id].view_of.clone())
            .collect()
    }

    #[tokio::test]
    async fn choosing_a_pane_while_on_a_wall_adds_a_tile_for_it() {
        let (mut app, fixture) = app_with_wall();
        app.state.workspaces[2].tabs[0]
            .layout
            .focus_pane(fixture.tiles[1]);
        app.state
            .open_wall_add_navigator_from(&app.terminal_runtimes);
        let target = pane_row(&app, 1);

        assert!(choose_in_navigator(&mut app, target));
        assert_eq!(app.state.mode, Mode::Terminal);
        assert!(app.apply_requested_wall_add());

        let wall = &app.state.workspaces[2];
        assert_eq!(app.state.workspaces.len(), 3);
        assert_eq!(app.state.active, Some(2));
        assert_eq!(
            shown_by(wall),
            vec![
                Some(fixture.target.clone()),
                Some(fixture.other.clone()),
                Some(fixture.other.clone()),
            ]
        );
        // Focus stays on the tile that had it, so nothing changes size.
        assert_eq!(wall.tabs[0].layout.focused(), fixture.tiles[1]);
        let added = wall.tabs[0].layout.pane_ids()[2];
        let placeholder = wall.tabs[0].panes[&added].attached_terminal_id.clone();
        assert!(app.terminal_runtimes.get(&placeholder).is_some());
        assert!(app.state.terminals.contains_key(&placeholder));
        assert_eq!(wall.public_pane_numbers.len(), 3);
    }

    #[tokio::test]
    async fn a_wall_with_new_tiles_is_laid_out_as_an_even_grid() {
        let (mut app, fixture) = app_with_wall();
        app.add_to_wall(2, &[fixture.target.to_string(), fixture.other.to_string()])
            .expect("added");

        let tab = &app.state.workspaces[2].tabs[0];
        let rects: Vec<_> = tab
            .layout
            .panes(AREA)
            .into_iter()
            .map(|info| info.rect)
            .collect();
        assert_eq!(rects.len(), 4);
        assert!(rects
            .iter()
            .all(|rect| rect.width == 60 && rect.height == 20));
        // The tiles that were there keep their place at the top.
        assert_eq!(&tab.layout.pane_ids()[..2], &fixture.tiles[..]);
    }

    #[tokio::test]
    async fn choosing_a_tile_of_the_wall_adds_what_it_shows() {
        let (mut app, fixture) = app_with_wall();
        app.state
            .open_wall_add_navigator_from(&app.terminal_runtimes);
        let tile = NavigatorTarget::Pane {
            ws_idx: 2,
            tab_idx: 0,
            pane_id: fixture.tiles[1],
        };

        assert!(choose_in_navigator(&mut app, tile));
        app.apply_requested_wall_add();

        let shown = shown_by(&app.state.workspaces[2]);
        assert_eq!(shown.len(), 3);
        assert_eq!(shown[2], Some(fixture.other.clone()));
    }

    #[tokio::test]
    async fn choosing_a_space_or_tab_adds_its_focused_pane() {
        let (app, fixture) = app_with_wall();
        assert_eq!(
            app.state
                .navigator_wall_target(&NavigatorTarget::Workspace { ws_idx: 0 }),
            Some(fixture.target.clone())
        );
        assert_eq!(
            app.state.navigator_wall_target(&NavigatorTarget::Tab {
                ws_idx: 1,
                tab_idx: 0
            }),
            Some(fixture.other.clone())
        );
    }

    #[tokio::test]
    async fn choosing_a_pane_away_from_any_wall_opens_a_new_one() {
        let (mut app, fixture) = app_with_wall();
        app.state.active = Some(0);
        app.state
            .open_wall_add_navigator_from(&app.terminal_runtimes);
        let target = pane_row(&app, 1);

        assert!(choose_in_navigator(&mut app, target));
        app.apply_requested_wall_add();

        assert_eq!(app.state.workspaces.len(), 4);
        assert_eq!(app.state.active, Some(3));
        let wall = &app.state.workspaces[3];
        assert!(wall.wall.is_some());
        assert_eq!(shown_by(wall), vec![Some(fixture.other.clone())]);
        // The wall that was not active is left as it was.
        assert_eq!(app.state.workspaces[2].tabs[0].panes.len(), 2);
    }

    #[tokio::test]
    async fn the_navigator_opened_normally_goes_to_what_is_chosen() {
        let (mut app, _fixture) = app_with_wall();
        app.state
            .open_wall_add_navigator_from(&app.terminal_runtimes);
        assert!(matches!(
            app.state.navigator.purpose,
            NavigatorPurpose::AddToWall {
                wall_workspace_id: Some(_)
            }
        ));
        app.state.mode = Mode::Terminal;

        app.state.open_navigator_from(&app.terminal_runtimes);
        assert_eq!(app.state.navigator.purpose, NavigatorPurpose::Goto);
        let target = pane_row(&app, 1);
        assert!(choose_in_navigator(&mut app, target));

        assert_eq!(app.state.request_wall_add, None);
        assert_eq!(app.state.active, Some(1));
    }

    #[tokio::test]
    async fn adding_to_a_wall_refuses_what_it_cannot_show_and_adds_nothing() {
        let (mut app, fixture) = app_with_wall();

        let unknown = app.add_to_wall(2, &[fixture.other.to_string(), "term_missing".into()]);
        assert_eq!(
            unknown.map_err(|(code, _)| code),
            Err("terminal_not_found".to_string())
        );
        let not_a_wall = app.add_to_wall(0, &[fixture.other.to_string()]);
        assert_eq!(
            not_a_wall.map_err(|(code, _)| code),
            Err("not_a_wall".to_string())
        );

        assert_eq!(app.state.workspaces[2].tabs[0].panes.len(), 2);
        assert_eq!(app.state.workspaces[0].tabs[0].panes.len(), 1);
    }

    /// An app with the target's terminal registered the way a real one is,
    /// and a wall made by `create_wall`, so its placeholders are real too.
    fn app_with_live_target() -> (App, Fixture, usize) {
        let (mut app, fixture) = app_with_wall();
        app.terminal_runtimes.insert(
            fixture.target.clone(),
            TerminalRuntime::test_with_screen_bytes(80, 24, b"target"),
        );
        app.state.terminals.insert(
            fixture.target.clone(),
            crate::terminal::TerminalState::new(fixture.target.clone(), "/".into()),
        );
        let index = app
            .create_wall(
                &[fixture.target.to_string(), fixture.other.to_string()],
                None,
                true,
            )
            .expect("wall");
        (app, fixture, index)
    }

    fn target_is_alive(app: &App, target: &TerminalId) -> bool {
        app.terminal_runtimes.get(target).is_some()
            && app.state.terminals.contains_key(target)
            && !app.state.terminal_runtime_shutdowns.contains(target)
            && app.state.workspaces[0].tabs[0]
                .panes
                .values()
                .any(|pane| &pane.attached_terminal_id == target)
    }

    #[tokio::test]
    async fn closing_a_tile_closes_the_view_and_not_the_terminal_it_shows() {
        let (mut app, fixture, index) = app_with_live_target();
        let tab = &app.state.workspaces[index].tabs[0];
        let tile = tab.layout.focused();
        let placeholder = tab.panes[&tile].attached_terminal_id.clone();
        assert_eq!(tab.panes[&tile].view_of, Some(fixture.target.clone()));

        app.close_focused_pane_via_api_requires_confirmation();

        let tab = &app.state.workspaces[index].tabs[0];
        assert_eq!(tab.panes.len(), 1);
        assert!(app.terminal_runtimes.get(&placeholder).is_none());
        assert!(!app.state.terminals.contains_key(&placeholder));
        assert!(target_is_alive(&app, &fixture.target));
    }

    #[tokio::test]
    async fn closing_the_last_tile_closes_the_wall_and_not_the_terminal_it_shows() {
        let (mut app, fixture, index) = app_with_live_target();
        app.close_focused_pane_via_api_requires_confirmation();
        assert_eq!(app.state.workspaces.len(), index + 1);

        app.close_focused_pane_via_api_requires_confirmation();

        assert_eq!(app.state.workspaces.len(), index);
        assert!(target_is_alive(&app, &fixture.target));
    }

    #[tokio::test]
    async fn a_view_whose_terminal_closes_says_so() {
        let mut app = crate::app::tests::test_app();
        let fixture = Fixture::new();
        app.state.workspaces = fixture.state.workspaces;
        let index = app
            .create_wall(&[fixture.target.to_string()], None, false)
            .expect("wall");

        app.shutdown_terminal_runtime(fixture.target.clone());

        let wall = &app.state.workspaces[index];
        let root = wall.tabs[0].root_pane;
        let pane = &wall.tabs[0].panes[&root];
        assert_eq!(pane.view_of, None);
        let placeholder = app
            .terminal_runtimes
            .get(&pane.attached_terminal_id)
            .expect("placeholder");
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("test terminal");
        terminal
            .draw(|frame| placeholder.render(frame, Rect::new(0, 0, 80, 6), false))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("view ended"), "{text}");
    }
}
