//! Drawing the wall.
//!
//! Only draws: what to show is decided before this is called, and nothing
//! here changes the wall. A tile that is not being typed into shows its
//! terminal re-wrapped to the tile and anchored to the bottom, since the
//! terminal stays at its own size; the active tile, whose terminal has been
//! sized to it, is drawn cell for cell with its cursor.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::layout::TileGeometry;
use super::picker::Picker;
use super::state::Tile;
use crate::app::state::Palette;
use crate::terminal::TerminalRuntime;

/// Everything one frame of the wall is drawn from.
pub(crate) struct WallView<'a> {
    pub(crate) area: Rect,
    pub(crate) tiles: &'a [Tile],
    pub(crate) geometry: &'a [TileGeometry],
    pub(crate) runtimes: &'a HashMap<String, TerminalRuntime>,
    pub(crate) active: Option<usize>,
    /// Whether the active tile's terminal is held at the tile's size, and so
    /// can be drawn as it is rather than re-wrapped.
    pub(crate) active_live: bool,
    /// The prefix key was pressed and the next key is the wall's.
    pub(crate) prefix_pending: bool,
    pub(crate) palette: &'a Palette,
    pub(crate) picker: Option<&'a Picker>,
    pub(crate) picker_hint: &'a str,
    /// What an empty wall says.
    pub(crate) empty_hint: &'a str,
    /// A passing message, such as a target list that could not be fetched.
    pub(crate) status: Option<&'a str>,
}

pub(crate) fn render(frame: &mut Frame, view: &WallView) {
    let palette = view.palette;
    let base = Style::default().fg(palette.text);
    if view.tiles.is_empty() {
        let mut lines = vec![Line::from(Span::styled(
            "herdr wall",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))];
        lines.push(Line::from(Span::styled(
            view.empty_hint,
            Style::default().fg(palette.overlay0),
        )));
        let height = view.area.height.min(lines.len() as u16);
        let y = view.area.y + view.area.height.saturating_sub(height) / 2;
        frame.render_widget(
            Paragraph::new(lines)
                .style(base)
                .alignment(ratatui::layout::Alignment::Center),
            Rect::new(view.area.x, y, view.area.width, height),
        );
    }

    for (index, (tile, geometry)) in view.tiles.iter().zip(view.geometry).enumerate() {
        let active = view.active == Some(index);
        render_title(frame, view, tile, geometry, active);
        if let Some(separator) = geometry.separator {
            let line: Vec<Line> = (0..separator.height)
                .map(|_| Line::from(Span::styled("│", Style::default().fg(palette.surface_dim))))
                .collect();
            frame.render_widget(Paragraph::new(line), separator);
        }
        let content = geometry.content;
        if content.width == 0 || content.height == 0 {
            continue;
        }
        if let Some(reason) = &tile.ended {
            render_note(frame, content, &format!("[{reason}]"), palette);
            continue;
        }
        let Some(runtime) = view.runtimes.get(&tile.target.terminal_id) else {
            continue;
        };
        if active && view.active_live && view.picker.is_none() {
            runtime.render(frame, content, true);
        } else {
            runtime.render_rewrapped(frame, content);
        }
    }

    if let Some(picker) = view.picker {
        picker.render(frame, view.area, palette, view.picker_hint);
    }
    if let Some(status) = view.status {
        let width = crate::ui::display_width_u16(status)
            .saturating_add(2)
            .min(view.area.width);
        let rect = Rect::new(
            view.area.right().saturating_sub(width),
            view.area.bottom().saturating_sub(1),
            width,
            1.min(view.area.height),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {status} "),
                Style::default().fg(palette.panel_bg).bg(palette.yellow),
            )),
            rect,
        );
    }
}

/// A tile's title row: its label in the border style herdr gives pane
/// titles, accented and bold on the active tile, with a rule out to the edge.
fn render_title(
    frame: &mut Frame,
    view: &WallView,
    tile: &Tile,
    geometry: &TileGeometry,
    active: bool,
) {
    let palette = view.palette;
    let area = geometry.title;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let color = if active {
        palette.accent
    } else {
        palette.overlay0
    };
    let mut label = tile.target.label.clone();
    if tile.view_only {
        label.push_str(" · view only: held by another client");
    }
    if active && view.prefix_pending {
        label.push_str(" · prefix");
    }
    let title = crate::ui::pane_border_title(&label, area.width.saturating_add(2), active)
        .unwrap_or_default();
    let rule_style = Style::default().fg(if active {
        palette.accent
    } else {
        palette.surface_dim
    });
    let mut title_style = Style::default().fg(color);
    if active {
        title_style = title_style.add_modifier(Modifier::BOLD);
    }
    let lead = if active { "━" } else { "─" };
    let used = 1 + crate::ui::display_width_u16(&title);
    let rest = usize::from(area.width.saturating_sub(used));
    let line = Line::from(vec![
        Span::styled(lead, rule_style),
        Span::styled(title, title_style),
        Span::styled(lead.repeat(rest), rule_style),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_note(frame: &mut Frame, area: Rect, text: &str, palette: &Palette) {
    let y = area.y + area.height / 2;
    frame.render_widget(
        Paragraph::new(Span::styled(
            text.to_owned(),
            Style::default().fg(palette.overlay0),
        ))
        .alignment(ratatui::layout::Alignment::Center),
        Rect::new(area.x, y, area.width, 1),
    );
}
