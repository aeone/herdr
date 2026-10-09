//! Where each tile of a wall goes.
//!
//! A wall is a grid laid out afresh whenever a tile comes or goes: the smallest
//! square that holds every tile (two across for up to four, three for up to
//! nine), filled from the top, so a grid that is not full is short in its last
//! row. Every row gets an equal share of the height and every tile in a row an
//! equal share of its width -- the last row's tiles are wider when it is short.
//!
//! Pure arithmetic over rectangles, so the shape of a wall can be tested
//! without a terminal.

use ratatui::layout::Rect;

/// One tile's place on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TileGeometry {
    /// Everything the tile owns, title and separator included.
    pub(crate) outer: Rect,
    /// The tile's title row, its top row.
    pub(crate) title: Rect,
    /// Where the terminal is drawn.
    pub(crate) content: Rect,
    /// The column between this tile and the next one in its row.
    pub(crate) separator: Option<Rect>,
}

/// A direction to move the active tile in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Lays `count` tiles out over `area`, in reading order.
pub(crate) fn tile_geometry(area: Rect, count: usize) -> Vec<TileGeometry> {
    if count == 0 || area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let mut columns = 1usize;
    while columns * columns < count {
        columns += 1;
    }
    let rows = count.div_ceil(columns);

    let heights = split_even(area.height, rows);
    let mut tiles = Vec::with_capacity(count);
    let mut y = area.y;
    for (row, height) in heights.into_iter().enumerate() {
        let in_row = columns.min(count - row * columns);
        let widths = split_even(area.width, in_row);
        let mut x = area.x;
        for (column, width) in widths.into_iter().enumerate() {
            let outer = Rect::new(x, y, width, height);
            let last_in_row = column + 1 == in_row;
            // The column to the right of every tile but the last in its row is
            // a separator, so neighbouring screens do not run into each other.
            let separator =
                (!last_in_row && width > 1).then(|| Rect::new(x + width - 1, y, 1, height));
            let body_width = width - u16::from(separator.is_some());
            let title = Rect::new(x, y, body_width, height.min(1));
            let content = Rect::new(x, y.saturating_add(1), body_width, height.saturating_sub(1));
            tiles.push(TileGeometry {
                outer,
                title,
                content,
                separator,
            });
            x = x.saturating_add(width);
        }
        y = y.saturating_add(height);
    }
    tiles
}

/// Splits `total` cells into `parts` shares that differ by at most one, the
/// larger ones first.
fn split_even(total: u16, parts: usize) -> Vec<u16> {
    if parts == 0 {
        return Vec::new();
    }
    let parts_u16 = u16::try_from(parts).unwrap_or(u16::MAX);
    let base = total / parts_u16;
    let extra = usize::from(total % parts_u16);
    (0..parts)
        .map(|index| base + u16::from(index < extra))
        .collect()
}

/// The tile under a screen cell.
pub(crate) fn tile_at(tiles: &[TileGeometry], column: u16, row: u16) -> Option<usize> {
    tiles.iter().position(|tile| {
        let outer = tile.outer;
        column >= outer.x && column < outer.right() && row >= outer.y && row < outer.bottom()
    })
}

/// The tile next to `from` in `direction`: among those wholly on that side
/// and sharing some of its rows (or columns), the nearest, and of equally near
/// ones the one sharing the most.
pub(crate) fn neighbor(tiles: &[TileGeometry], from: usize, direction: Direction) -> Option<usize> {
    let current = tiles.get(from)?.outer;
    let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| a1.min(b1).saturating_sub(a0.max(b0));
    tiles
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != from)
        .filter_map(|(index, tile)| {
            let other = tile.outer;
            let (distance, shared) = match direction {
                Direction::Left if other.right() <= current.x => (
                    current.x - other.right(),
                    overlap(current.y, current.bottom(), other.y, other.bottom()),
                ),
                Direction::Right if other.x >= current.right() => (
                    other.x - current.right(),
                    overlap(current.y, current.bottom(), other.y, other.bottom()),
                ),
                Direction::Up if other.bottom() <= current.y => (
                    current.y - other.bottom(),
                    overlap(current.x, current.right(), other.x, other.right()),
                ),
                Direction::Down if other.y >= current.bottom() => (
                    other.y - current.bottom(),
                    overlap(current.x, current.right(), other.x, other.right()),
                ),
                _ => return None,
            };
            (shared > 0).then_some((index, distance, shared))
        })
        .min_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)))
        .map(|(index, _, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outers(area: Rect, count: usize) -> Vec<Rect> {
        tile_geometry(area, count)
            .into_iter()
            .map(|tile| tile.outer)
            .collect()
    }

    #[test]
    fn one_tile_takes_the_whole_screen_with_a_title_row() {
        let tiles = tile_geometry(Rect::new(0, 0, 120, 36), 1);

        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].outer, Rect::new(0, 0, 120, 36));
        assert_eq!(tiles[0].title, Rect::new(0, 0, 120, 1));
        assert_eq!(tiles[0].content, Rect::new(0, 1, 120, 35));
        assert_eq!(tiles[0].separator, None);
    }

    #[test]
    fn two_tiles_sit_side_by_side_with_a_separator_between() {
        let tiles = tile_geometry(Rect::new(0, 0, 121, 36), 2);

        assert_eq!(tiles[0].outer, Rect::new(0, 0, 61, 36));
        assert_eq!(tiles[1].outer, Rect::new(61, 0, 60, 36));
        assert_eq!(tiles[0].separator, Some(Rect::new(60, 0, 1, 36)));
        assert_eq!(tiles[0].content, Rect::new(0, 1, 60, 35));
        assert_eq!(tiles[1].separator, None);
        assert_eq!(tiles[1].content, Rect::new(61, 1, 60, 35));
    }

    /// Three tiles make a two-by-two grid one short, and the short last row's
    /// tile takes the whole width rather than leaving a hole.
    #[test]
    fn a_short_last_row_spreads_across_the_width() {
        assert_eq!(
            outers(Rect::new(0, 0, 120, 36), 3),
            vec![
                Rect::new(0, 0, 60, 18),
                Rect::new(60, 0, 60, 18),
                Rect::new(0, 18, 120, 18),
            ]
        );
    }

    #[test]
    fn five_tiles_make_three_columns_and_two_rows() {
        let tiles = outers(Rect::new(0, 0, 90, 30), 5);

        assert_eq!(tiles.len(), 5);
        assert_eq!(tiles[0], Rect::new(0, 0, 30, 15));
        assert_eq!(tiles[2], Rect::new(60, 0, 30, 15));
        assert_eq!(tiles[3], Rect::new(0, 15, 45, 15));
        assert_eq!(tiles[4], Rect::new(45, 15, 45, 15));
    }

    #[test]
    fn the_layout_follows_the_count_as_tiles_come_and_go() {
        let area = Rect::new(0, 0, 100, 40);
        for count in 1..=10 {
            let tiles = tile_geometry(area, count);
            assert_eq!(tiles.len(), count);
            let covered: u32 = tiles
                .iter()
                .map(|tile| u32::from(tile.outer.width) * u32::from(tile.outer.height))
                .sum();
            assert_eq!(
                covered,
                100 * 40,
                "{count} tiles leave no gap and no overlap"
            );
        }
    }

    #[test]
    fn nothing_is_laid_out_on_an_empty_screen() {
        assert!(tile_geometry(Rect::new(0, 0, 0, 10), 3).is_empty());
        assert!(tile_geometry(Rect::new(0, 0, 80, 24), 0).is_empty());
    }

    #[test]
    fn a_click_finds_the_tile_under_it() {
        let tiles = tile_geometry(Rect::new(0, 0, 120, 36), 3);

        assert_eq!(tile_at(&tiles, 5, 5), Some(0));
        assert_eq!(tile_at(&tiles, 60, 0), Some(1));
        assert_eq!(tile_at(&tiles, 119, 35), Some(2));
        assert_eq!(tile_at(&tiles, 120, 0), None);
    }

    #[test]
    fn neighbours_are_found_in_each_direction() {
        // 0 1
        //  2
        let tiles = tile_geometry(Rect::new(0, 0, 120, 36), 3);

        assert_eq!(neighbor(&tiles, 0, Direction::Right), Some(1));
        assert_eq!(neighbor(&tiles, 1, Direction::Left), Some(0));
        assert_eq!(neighbor(&tiles, 0, Direction::Down), Some(2));
        assert_eq!(neighbor(&tiles, 1, Direction::Down), Some(2));
        assert_eq!(neighbor(&tiles, 2, Direction::Up), Some(0));
        assert_eq!(neighbor(&tiles, 0, Direction::Left), None);
        assert_eq!(neighbor(&tiles, 2, Direction::Down), None);
    }
}
