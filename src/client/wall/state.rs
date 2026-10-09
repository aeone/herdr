//! The wall's own state: which tiles it has, which one is active, what it
//! asks the server to stream, and which terminal it holds.
//!
//! None of this is shared with the server. The server only ever sees one
//! observing connection naming the tiles' terminals and, while a tile is
//! active, one attach connection holding that tile's terminal -- the same two
//! things a mirror or `herdr focus` would open -- so nothing about a wall
//! appears in its workspaces or reaches another client.
//!
//! Pure data: deciding what to send is kept apart from sending it, so the
//! whole state machine can be tested without a server or a terminal.

use std::collections::BTreeMap;

use super::layout::{self, Direction, TileGeometry};
use super::targets::WallTarget;

/// One tile on the wall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tile {
    pub(crate) target: WallTarget,
    /// Why the terminal stopped being available, once it has.
    pub(crate) ended: Option<String>,
    /// Something else holds the terminal, so typing here was refused. The tile
    /// keeps showing it and stops asking.
    pub(crate) view_only: bool,
}

/// The terminal an attach should hold, at the size it should hold it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachTarget {
    pub(crate) terminal_id: String,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// One thing to do to the attach connection to make it hold what it should.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttachStep {
    /// Open a connection at this size and attach to this terminal.
    Open(AttachTarget),
    /// Move the open connection to another terminal. The server releases the
    /// one it leaves, which goes back to being sized by its own layout.
    Retarget(String),
    /// Resize the terminal the connection holds.
    Resize { cols: u16, rows: u16 },
    /// Let go of the terminal and close the connection.
    Close,
}

/// What to do to get from the attach that is open to the one that should be.
pub(crate) fn plan_attach(
    current: Option<&AttachTarget>,
    desired: Option<&AttachTarget>,
) -> Vec<AttachStep> {
    match (current, desired) {
        (None, None) => Vec::new(),
        (Some(_), None) => vec![AttachStep::Close],
        (None, Some(desired)) => vec![AttachStep::Open(desired.clone())],
        (Some(current), Some(desired)) => {
            let moving = current.terminal_id != desired.terminal_id;
            let resizing = (current.cols, current.rows) != (desired.cols, desired.rows);
            match (moving, resizing) {
                (false, false) => Vec::new(),
                (false, true) => vec![AttachStep::Resize {
                    cols: desired.cols,
                    rows: desired.rows,
                }],
                (true, false) => vec![AttachStep::Retarget(desired.terminal_id.clone())],
                // An attach takes its terminal to the connection's size, and a
                // resize applies to whatever the connection holds at the time.
                // Moving and then resizing would size the arriving terminal
                // twice, resizing and then moving would size the departing one
                // on its way out; each is a redraw a program did not need. A
                // fresh connection opened at the new size sizes only the
                // terminal arriving, and only once.
                (true, true) => vec![AttachStep::Close, AttachStep::Open(desired.clone())],
            }
        }
    }
}

/// The size to ask the server to render a terminal at.
///
/// Frames are rendered at the size asked for, not the terminal's own, so
/// asking for less than the terminal has cuts off its bottom rows -- where the
/// newest output is. Asking for more costs only blank cells. So the area is the
/// largest of: what it was already (a terminal released by the active tile
/// grows back to its own size before the next look at its size says so), the
/// terminal's last known size, and the active tile, which the terminal is
/// about to be resized to. A terminal whose size has never been reported is
/// guessed at the size of the screen the wall is on.
pub(crate) fn observe_area(
    previous: Option<(u16, u16)>,
    known: Option<(u16, u16)>,
    guess: (u16, u16),
    active_content: Option<(u16, u16)>,
) -> (u16, u16) {
    let mut area = previous.or(known).unwrap_or(guess);
    for size in [known, active_content].into_iter().flatten() {
        area = (area.0.max(size.0), area.1.max(size.1));
    }
    (area.0.max(1), area.1.max(1))
}

/// Everything the wall knows about its tiles.
#[derive(Debug, Default)]
pub(crate) struct WallState {
    tiles: Vec<Tile>,
    active: Option<usize>,
    /// The area each terminal is being streamed at, kept so it only grows.
    areas: BTreeMap<String, (u16, u16)>,
}

impl WallState {
    pub(crate) fn tiles(&self) -> &[Tile] {
        &self.tiles
    }

    pub(crate) fn active(&self) -> Option<usize> {
        self.active
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub(crate) fn active_tile(&self) -> Option<&Tile> {
        self.tiles.get(self.active?)
    }

    pub(crate) fn tile_for_terminal(&self, terminal_id: &str) -> Option<usize> {
        self.tiles
            .iter()
            .position(|tile| tile.target.terminal_id == terminal_id)
    }

    /// Adds a tile for `target` and makes it active, returning its index.
    ///
    /// A terminal already on the wall is not shown twice: picking it again
    /// activates its tile. One terminal can only have one size, so two tiles
    /// of it could never both be live.
    pub(crate) fn add(&mut self, target: WallTarget) -> usize {
        if let Some(index) = self.tile_for_terminal(&target.terminal_id) {
            let tile = &mut self.tiles[index];
            if tile.ended.is_none() {
                tile.target = target;
            }
            self.active = Some(index);
            return index;
        }
        self.tiles.push(Tile {
            target,
            ended: None,
            view_only: false,
        });
        let index = self.tiles.len() - 1;
        self.active = Some(index);
        index
    }

    /// Makes `index` the active tile. Returns whether anything changed.
    pub(crate) fn activate(&mut self, index: usize) -> bool {
        if index >= self.tiles.len() || self.active == Some(index) {
            return false;
        }
        self.active = Some(index);
        true
    }

    /// Moves the active tile `step` places along reading order, wrapping.
    pub(crate) fn cycle(&mut self, step: isize) -> bool {
        let count = self.tiles.len();
        let Some(active) = self.active else {
            return false;
        };
        if count < 2 {
            return false;
        }
        let count_i = count as isize;
        let next = (active as isize + step).rem_euclid(count_i) as usize;
        self.activate(next)
    }

    /// Moves the active tile to its neighbour in `direction`, if it has one.
    pub(crate) fn focus_direction(
        &mut self,
        geometry: &[TileGeometry],
        direction: Direction,
    ) -> bool {
        let Some(active) = self.active else {
            return false;
        };
        match layout::neighbor(geometry, active, direction) {
            Some(next) => self.activate(next),
            None => false,
        }
    }

    /// Closes the active tile. The tile in its place, or the one before it
    /// when it was last, becomes active; closing the last tile leaves none.
    pub(crate) fn close_active(&mut self) -> Option<Tile> {
        let active = self.active?;
        let tile = self.tiles.remove(active);
        if !self
            .tiles
            .iter()
            .any(|other| other.target.terminal_id == tile.target.terminal_id)
        {
            self.areas.remove(&tile.target.terminal_id);
        }
        self.active = if self.tiles.is_empty() {
            None
        } else {
            Some(active.min(self.tiles.len() - 1))
        };
        Some(tile)
    }

    /// Records that a terminal on the wall has gone. Its tile stays, saying
    /// so, until it is closed: a layout that rearranges itself under the
    /// pointer is worse than a tile that needs closing.
    pub(crate) fn mark_ended(&mut self, terminal_id: &str, reason: Option<String>) -> bool {
        let Some(index) = self.tile_for_terminal(terminal_id) else {
            return false;
        };
        let tile = &mut self.tiles[index];
        if tile.ended.is_some() {
            return false;
        }
        tile.ended = Some(reason.unwrap_or_else(|| "terminal ended".to_owned()));
        true
    }

    /// Records that typing into a terminal was refused because something else
    /// holds it.
    pub(crate) fn mark_view_only(&mut self, terminal_id: &str) -> bool {
        let Some(index) = self.tile_for_terminal(terminal_id) else {
            return false;
        };
        let changed = !self.tiles[index].view_only;
        self.tiles[index].view_only = true;
        changed
    }

    /// Takes in a fresh list of targets: sizes and titles move on, and a tile
    /// whose terminal is no longer listed is left for the stream to end.
    /// Returns whether any tile changed.
    pub(crate) fn refresh_targets(&mut self, fresh: &[WallTarget]) -> bool {
        let mut changed = false;
        for tile in &mut self.tiles {
            if tile.ended.is_some() {
                continue;
            }
            // Prefer a listing of the same kind and name the tile was picked
            // under; any listing of the terminal will do for its size.
            let same = fresh
                .iter()
                .find(|target| {
                    target.terminal_id == tile.target.terminal_id
                        && target.kind == tile.target.kind
                        && target.target == tile.target.target
                })
                .or_else(|| {
                    fresh
                        .iter()
                        .find(|target| target.terminal_id == tile.target.terminal_id)
                });
            let Some(fresh) = same else {
                continue;
            };
            if fresh.kind == tile.target.kind && fresh.target == tile.target.target {
                if *fresh != tile.target {
                    tile.target = fresh.clone();
                    changed = true;
                }
            } else if (fresh.cols, fresh.rows) != (tile.target.cols, tile.target.rows) {
                tile.target.cols = fresh.cols;
                tile.target.rows = fresh.rows;
                changed = true;
            }
        }
        changed
    }

    /// The terminal the attach connection should hold: the active tile's, at
    /// the size of its content, unless it has ended, was refused, or has no
    /// room to draw in.
    pub(crate) fn desired_attach(&self, geometry: &[TileGeometry]) -> Option<AttachTarget> {
        let active = self.active?;
        let tile = self.tiles.get(active)?;
        if tile.ended.is_some() || tile.view_only {
            return None;
        }
        let content = geometry.get(active)?.content;
        if content.width == 0 || content.height == 0 {
            return None;
        }
        Some(AttachTarget {
            terminal_id: tile.target.terminal_id.clone(),
            cols: content.width,
            rows: content.height,
        })
    }

    /// The terminals to stream and the area to stream each at, updating the
    /// areas remembered for next time. `guess` is the size of the wall's own
    /// screen, used for a terminal whose size was never reported.
    pub(crate) fn observe_set(
        &mut self,
        geometry: &[TileGeometry],
        guess: (u16, u16),
    ) -> Vec<(String, u16, u16)> {
        let mut set: Vec<(String, u16, u16)> = Vec::with_capacity(self.tiles.len());
        for (index, tile) in self.tiles.iter().enumerate() {
            if tile.ended.is_some() {
                continue;
            }
            let terminal_id = &tile.target.terminal_id;
            if set.iter().any(|(id, _, _)| id == terminal_id) {
                continue;
            }
            let active_content = (self.active == Some(index) && !tile.view_only)
                .then(|| geometry.get(index))
                .flatten()
                .map(|tile| (tile.content.width, tile.content.height));
            let area = observe_area(
                self.areas.get(terminal_id).copied(),
                tile.target.size(),
                guess,
                active_content,
            );
            self.areas.insert(terminal_id.clone(), area);
            set.push((terminal_id.clone(), area.0, area.1));
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::super::targets::{test_target, WallTargetKind};
    use super::*;

    fn target(name: &str, terminal_id: &str) -> WallTarget {
        test_target(WallTargetKind::Agent, name, terminal_id)
    }

    fn geometry(count: usize) -> Vec<TileGeometry> {
        layout::tile_geometry(Rect::new(0, 0, 120, 36), count)
    }

    #[test]
    fn a_new_wall_is_empty_and_holds_nothing() {
        let state = WallState::default();

        assert!(state.is_empty());
        assert_eq!(state.active(), None);
        assert_eq!(state.desired_attach(&[]), None);
    }

    #[test]
    fn adding_a_tile_activates_it() {
        let mut state = WallState::default();

        state.add(target("a", "t1"));
        state.add(target("b", "t2"));

        assert_eq!(state.tiles().len(), 2);
        assert_eq!(state.active(), Some(1));
    }

    #[test]
    fn picking_a_terminal_already_on_the_wall_activates_its_tile() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));

        let index = state.add(test_target(WallTargetKind::Space, "w1", "t1"));

        assert_eq!(index, 0);
        assert_eq!(state.tiles().len(), 2);
        assert_eq!(state.active(), Some(0));
    }

    #[test]
    fn closing_the_active_tile_activates_the_one_in_its_place() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        state.add(target("c", "t3"));
        state.activate(1);

        let closed = state.close_active().expect("closed");

        assert_eq!(closed.target.terminal_id, "t2");
        assert_eq!(state.active(), Some(1));
        assert_eq!(state.tiles()[1].target.terminal_id, "t3");
    }

    #[test]
    fn closing_the_last_tile_activates_the_one_before() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));

        state.close_active();
        assert_eq!(state.active(), Some(0));
        state.close_active();
        assert_eq!(state.active(), None);
        assert!(state.close_active().is_none());
    }

    #[test]
    fn cycling_wraps_around() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        state.add(target("c", "t3"));

        assert!(state.cycle(1));
        assert_eq!(state.active(), Some(0));
        assert!(state.cycle(-1));
        assert_eq!(state.active(), Some(2));
    }

    #[test]
    fn moving_by_direction_follows_the_grid() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        state.add(target("c", "t3"));
        state.activate(0);
        let grid = geometry(3);

        assert!(state.focus_direction(&grid, Direction::Right));
        assert_eq!(state.active(), Some(1));
        assert!(state.focus_direction(&grid, Direction::Down));
        assert_eq!(state.active(), Some(2));
        assert!(!state.focus_direction(&grid, Direction::Down));
        assert_eq!(state.active(), Some(2));
    }

    #[test]
    fn the_active_tile_is_held_at_its_content_size() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        let grid = geometry(2);

        let desired = state.desired_attach(&grid).expect("an attach");

        assert_eq!(
            desired,
            AttachTarget {
                terminal_id: "t2".into(),
                cols: grid[1].content.width,
                rows: grid[1].content.height,
            }
        );
    }

    #[test]
    fn an_ended_or_refused_tile_is_not_held() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        let grid = geometry(1);

        state.mark_view_only("t1");
        assert_eq!(state.desired_attach(&grid), None);

        let mut state = WallState::default();
        state.add(target("a", "t1"));
        assert!(state.mark_ended("t1", Some("gone".into())));
        assert_eq!(state.desired_attach(&grid), None);
        assert_eq!(state.tiles()[0].ended.as_deref(), Some("gone"));
    }

    #[test]
    fn opening_the_first_tile_opens_an_attach() {
        let desired = AttachTarget {
            terminal_id: "t1".into(),
            cols: 60,
            rows: 20,
        };

        assert_eq!(
            plan_attach(None, Some(&desired)),
            vec![AttachStep::Open(desired)]
        );
    }

    /// Switching tiles moves the one connection rather than opening another,
    /// and moving is what makes the server let go of the terminal left behind.
    #[test]
    fn switching_tiles_retargets_and_so_releases_the_previous_terminal() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        // An odd width leaves both tiles of the row the same content width
        // once the separator is taken.
        let grid = layout::tile_geometry(Rect::new(0, 0, 121, 36), 2);
        let before = state.desired_attach(&grid).expect("attach");

        state.activate(0);
        let after = state.desired_attach(&grid).expect("attach");

        assert_eq!(
            plan_attach(Some(&before), Some(&after)),
            vec![AttachStep::Retarget("t1".into())]
        );
    }

    /// Between tiles of different sizes the connection is replaced rather
    /// than moved and resized, so neither terminal is resized for nothing.
    #[test]
    fn switching_to_a_tile_of_another_size_reopens_at_that_size() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        state.add(target("c", "t3"));
        let grid = geometry(3);
        let before = state.desired_attach(&grid).expect("attach");

        state.activate(0);
        let after = state.desired_attach(&grid).expect("attach");

        assert_eq!(
            plan_attach(Some(&before), Some(&after)),
            vec![AttachStep::Close, AttachStep::Open(after.clone())]
        );
        assert_eq!((after.cols, after.rows), (59, 17));
    }

    #[test]
    fn a_new_tile_size_resizes_without_moving() {
        let before = AttachTarget {
            terminal_id: "t1".into(),
            cols: 120,
            rows: 35,
        };
        let after = AttachTarget {
            cols: 60,
            ..before.clone()
        };

        assert_eq!(
            plan_attach(Some(&before), Some(&after)),
            vec![AttachStep::Resize { cols: 60, rows: 35 }]
        );
        assert_eq!(plan_attach(Some(&before), Some(&before)), Vec::new());
    }

    #[test]
    fn closing_the_last_tile_closes_the_attach() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        let grid = geometry(1);
        let before = state.desired_attach(&grid);

        state.close_active();
        let after = state.desired_attach(&geometry(0));

        assert_eq!(
            plan_attach(before.as_ref(), after.as_ref()),
            vec![AttachStep::Close]
        );
    }

    #[test]
    fn picking_targets_grows_the_observed_set() {
        let mut state = WallState::default();
        let mut first = target("a", "t1");
        first.cols = Some(200);
        first.rows = Some(50);
        state.add(first);
        let set = state.observe_set(&geometry(1), (120, 36));
        assert_eq!(set, vec![("t1".to_owned(), 200, 50)]);

        state.add(target("b", "t2"));
        let set = state.observe_set(&geometry(2), (120, 36));

        // t2's size was never reported, so it is guessed at the screen's, and
        // as the active tile it is at least as big as the tile.
        assert_eq!(set.len(), 2);
        assert_eq!(set[0], ("t1".to_owned(), 200, 50));
        assert_eq!(set[1], ("t2".to_owned(), 120, 36));
    }

    #[test]
    fn closing_a_tile_drops_it_from_the_observed_set() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.add(target("b", "t2"));
        state.close_active();

        let set = state.observe_set(&geometry(1), (80, 24));

        // t1 is active again, so it is streamed at least at its tile's size.
        assert_eq!(set, vec![("t1".to_owned(), 120, 35)]);
    }

    #[test]
    fn an_ended_terminal_is_no_longer_observed() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        state.mark_ended("t1", None);

        assert!(state.observe_set(&geometry(1), (80, 24)).is_empty());
    }

    /// A terminal that shrank -- because the active tile is holding it --
    /// keeps being streamed at the larger area, so when it is let go and grows
    /// back its bottom rows are not cut off before the next listing says so.
    #[test]
    fn the_observed_area_only_grows() {
        assert_eq!(
            observe_area(Some((200, 50)), Some((60, 20)), (80, 24), None),
            (200, 50)
        );
        assert_eq!(
            observe_area(Some((80, 24)), Some((100, 30)), (80, 24), None),
            (100, 30)
        );
        assert_eq!(
            observe_area(None, None, (80, 24), Some((100, 10))),
            (100, 24)
        );
        assert_eq!(observe_area(None, Some((0, 0)), (0, 0), None), (1, 1));
    }

    #[test]
    fn a_fresh_listing_updates_sizes_and_titles() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        let mut fresh = target("a", "t1");
        fresh.cols = Some(90);
        fresh.rows = Some(30);
        fresh.detail = "working".into();

        assert!(state.refresh_targets(&[fresh.clone()]));
        assert_eq!(state.tiles()[0].target, fresh);
        assert!(!state.refresh_targets(&[fresh]));
    }

    #[test]
    fn a_listing_under_another_name_still_carries_the_size() {
        let mut state = WallState::default();
        state.add(target("a", "t1"));
        let mut other = test_target(WallTargetKind::Pane, "w1:p1", "t1");
        other.cols = Some(70);
        other.rows = Some(20);

        assert!(state.refresh_targets(&[other]));
        assert_eq!(state.tiles()[0].target.target, "a");
        assert_eq!(state.tiles()[0].target.size(), Some((70, 20)));
    }
}
