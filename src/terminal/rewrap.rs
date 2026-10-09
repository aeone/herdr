//! Laying a terminal's screen out again for a tile of a different size.
//!
//! A view pane shows a terminal that lives somewhere else, at whatever size
//! that terminal currently is. Resizing it to fit every tile that shows it is
//! not an option -- it can only have one size, and a program redraws on every
//! change -- so a tile that does not own the size draws the screen re-wrapped
//! instead: the terminal's own soft wraps are undone, each logical line is
//! broken again at the tile's width, and the result is anchored to the bottom,
//! where the newest output is.
//!
//! This module is the arithmetic only. It knows nothing about cells or the
//! terminal core, so it can be tested without either.

use std::ops::Range;

/// One row of the source screen, as far as re-wrapping cares.
///
/// Rows index into a flat list of glyphs (a wide character is one glyph two
/// columns wide; the spacer cells beside and before it are not glyphs at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowShape {
    /// Index of the row's first glyph.
    pub start: usize,
    /// How many of the row's glyphs to draw. A soft-wrapped row keeps every
    /// glyph, since the line runs on into the next row; a row the program
    /// ended keeps them up to its last non-blank one.
    pub len: usize,
    /// Whether the terminal wrapped this row onto the next, rather than the
    /// program ending the line here.
    pub soft_wrapped: bool,
}

/// Plans which glyphs land on which row of a tile `width` columns by `height`
/// rows.
///
/// Returns at most `height` glyph ranges, top to bottom. When there are fewer,
/// the caller leaves the top of the tile blank so the last line still sits on
/// its bottom row. Blank rows at the bottom of the screen are dropped first,
/// except those up to `keep_through` (the cursor row), so a prompt waiting on
/// an empty line stays in view.
pub(crate) fn plan_rewrap(
    rows: &[RowShape],
    widths: &[u8],
    keep_through: Option<usize>,
    width: u16,
    height: u16,
) -> Vec<Range<usize>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let height = usize::from(height);

    let mut end = rows.len();
    while let Some(last) = end.checked_sub(1) {
        let row = rows[last];
        let blank = row.len == 0 && !row.soft_wrapped;
        let pinned = keep_through.is_some_and(|keep| last <= keep);
        if !blank || pinned {
            break;
        }
        end = last;
    }
    let rows = &rows[..end];

    // Walked from the bottom, one logical line at a time, so a long scrollback
    // of lines that would never fit costs nothing to skip.
    let mut planned = Vec::with_capacity(height);
    let mut line_rows = Vec::new();
    let mut line_end = rows.len();
    while line_end > 0 && planned.len() < height {
        let mut line_start = line_end - 1;
        while line_start > 0 && rows[line_start - 1].soft_wrapped {
            line_start -= 1;
        }
        let first = rows[line_start];
        let last = rows[line_end - 1];
        // A soft-wrapped row keeps all its glyphs, so the line's glyphs are
        // one contiguous run from its first row's start to its last row's end.
        let glyphs = first.start..last.start + last.len;
        line_rows.clear();
        break_line(glyphs, widths, usize::from(width), &mut line_rows);
        for row in line_rows.drain(..).rev() {
            if planned.len() == height {
                break;
            }
            planned.push(row);
        }
        line_end = line_start;
    }
    planned.reverse();
    planned
}

/// Breaks one logical line into rows no wider than `width`.
///
/// A wide glyph that would straddle the last column moves to the next row
/// whole, as a terminal does. One wider than the tile itself gets a row to
/// itself rather than an endless run of empty ones; the drawing clips it.
fn break_line(glyphs: Range<usize>, widths: &[u8], width: usize, out: &mut Vec<Range<usize>>) {
    let mut row_start = glyphs.start;
    let mut col = 0usize;
    for index in glyphs.clone() {
        let glyph_width = usize::from(widths.get(index).copied().unwrap_or(1).max(1));
        if col > 0 && col + glyph_width > width {
            out.push(row_start..index);
            row_start = index;
            col = 0;
        }
        col += glyph_width;
    }
    out.push(row_start..glyphs.end);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds rows and glyph widths from text, one entry per source row. A
    /// trailing `\` marks a soft-wrapped row; `W` stands for a wide glyph.
    fn screen(rows: &[&str]) -> (Vec<RowShape>, Vec<u8>) {
        let mut shapes = Vec::new();
        let mut widths = Vec::new();
        for row in rows {
            let (text, soft_wrapped) = match row.strip_suffix('\\') {
                Some(text) => (text, true),
                None => (*row, false),
            };
            let start = widths.len();
            widths.extend(text.chars().map(|ch| if ch == 'W' { 2 } else { 1 }));
            let len = if soft_wrapped {
                text.chars().count()
            } else {
                text.trim_end().chars().count()
            };
            shapes.push(RowShape {
                start,
                len,
                soft_wrapped,
            });
        }
        (shapes, widths)
    }

    fn lens(plan: &[Range<usize>]) -> Vec<usize> {
        plan.iter().map(|row| row.len()).collect()
    }

    #[test]
    fn a_soft_wrapped_line_is_broken_again_at_the_tile_width() {
        let (rows, widths) = screen(&["abcdef\\", "gh"]);
        let plan = plan_rewrap(&rows, &widths, None, 4, 4);
        assert_eq!(plan, vec![0..4, 4..8]);
    }

    #[test]
    fn a_line_the_program_ended_is_not_joined_to_the_next() {
        let (rows, widths) = screen(&["abc", "de"]);
        let plan = plan_rewrap(&rows, &widths, None, 10, 4);
        assert_eq!(plan, vec![0..3, 3..5]);
    }

    #[test]
    fn the_newest_lines_are_kept_when_they_do_not_all_fit() {
        let (rows, widths) = screen(&["one", "two", "three", "four"]);
        let plan = plan_rewrap(&rows, &widths, None, 10, 2);
        assert_eq!(
            plan,
            vec![
                rows[2].start..rows[2].start + 5,
                rows[3].start..rows[3].start + 4
            ]
        );
    }

    #[test]
    fn a_line_cut_by_the_top_of_the_tile_keeps_its_end() {
        let (rows, widths) = screen(&["abcdefghij"]);
        let plan = plan_rewrap(&rows, &widths, None, 4, 2);
        // Three rows of four would be needed; the bottom two are what show.
        assert_eq!(plan, vec![4..8, 8..10]);
    }

    #[test]
    fn a_wide_glyph_that_would_straddle_the_edge_moves_down_whole() {
        let (rows, widths) = screen(&["abcW"]);
        let plan = plan_rewrap(&rows, &widths, None, 4, 4);
        assert_eq!(plan, vec![0..3, 3..4]);
    }

    #[test]
    fn a_tile_larger_than_the_screen_shows_it_unchanged() {
        let (rows, widths) = screen(&["$ ls", "a b c", "$ "]);
        let plan = plan_rewrap(&rows, &widths, Some(2), 80, 10);
        assert_eq!(lens(&plan), vec![4, 5, 1]);
    }

    #[test]
    fn blank_rows_below_the_cursor_are_dropped() {
        let (rows, widths) = screen(&["$ ls", "out", "$", "", "", ""]);
        let plan = plan_rewrap(&rows, &widths, Some(2), 10, 10);
        assert_eq!(lens(&plan), vec![4, 3, 1]);
    }

    #[test]
    fn a_blank_row_holding_the_cursor_stays() {
        let (rows, widths) = screen(&["out", "", "", ""]);
        let plan = plan_rewrap(&rows, &widths, Some(1), 10, 10);
        assert_eq!(lens(&plan), vec![3, 0]);
    }

    #[test]
    fn a_glyph_wider_than_the_tile_gets_a_row_of_its_own() {
        let (rows, widths) = screen(&["WW"]);
        let plan = plan_rewrap(&rows, &widths, None, 1, 4);
        assert_eq!(plan, vec![0..1, 1..2]);
    }

    #[test]
    fn an_empty_tile_plans_nothing() {
        let (rows, widths) = screen(&["abc"]);
        assert!(plan_rewrap(&rows, &widths, None, 0, 4).is_empty());
        assert!(plan_rewrap(&rows, &widths, None, 4, 0).is_empty());
    }
}
