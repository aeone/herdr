//! Choosing what to put on the wall.
//!
//! fzf when it is installed, because people who have it already know how to
//! drive it and it is what the `hf` shell helper uses: the wall hands the
//! terminal over, fzf reads the list on stdin and the keyboard from the tty,
//! and the wall takes the terminal back. Without fzf there is a small list of
//! its own -- type to filter, arrows to move, enter to add, esc to cancel --
//! drawn over the wall.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::state::Palette;
use crate::input::TerminalKey;

/// What a key did to the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    /// Still choosing.
    Open,
    /// This entry was chosen.
    Picked(usize),
    /// Closed without choosing.
    Cancelled,
}

/// The built-in picker: a list of lines, a query, and a cursor.
#[derive(Debug, Clone)]
pub(crate) struct Picker {
    lines: Vec<String>,
    query: String,
    /// Indexes of the lines the query matches, in list order.
    matches: Vec<usize>,
    /// Position in `matches`.
    cursor: usize,
}

impl Picker {
    pub(crate) fn new(lines: Vec<String>) -> Self {
        let matches = (0..lines.len()).collect();
        Self {
            lines,
            query: String::new(),
            matches,
            cursor: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    #[cfg(test)]
    pub(crate) fn matches(&self) -> &[usize] {
        &self.matches
    }

    /// The entry under the cursor, as an index into the list.
    pub(crate) fn selected(&self) -> Option<usize> {
        self.matches.get(self.cursor).copied()
    }

    /// Adds typed or pasted text to the query.
    pub(crate) fn push_text(&mut self, text: &str) {
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if text.is_empty() {
            return;
        }
        self.query.push_str(&text);
        self.refilter();
    }

    pub(crate) fn handle_key(&mut self, key: &TerminalKey) -> PickerOutcome {
        if key.kind == KeyEventKind::Release {
            return PickerOutcome::Open;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PickerOutcome::Cancelled,
            KeyCode::Char('c' | 'g') if ctrl => return PickerOutcome::Cancelled,
            KeyCode::Enter => {
                return match self.selected() {
                    Some(index) => PickerOutcome::Picked(index),
                    None => PickerOutcome::Open,
                }
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('p' | 'k') if ctrl => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Tab => self.move_cursor(1),
            KeyCode::Char('n' | 'j') if ctrl => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.refilter();
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.refilter();
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let mut text = String::new();
                text.push(ch);
                self.push_text(&text);
            }
            _ => {}
        }
        PickerOutcome::Open
    }

    fn move_cursor(&mut self, step: isize) {
        if self.matches.is_empty() {
            self.cursor = 0;
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.cursor = (self.cursor as isize + step).clamp(0, last) as usize;
    }

    fn refilter(&mut self) {
        let terms: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        // Ranked, not just filtered: with loose matching, a long name can
        // contain a short query's letters in order, and in list order that put
        // an unrelated agent above the pane whose name *is* the query -- where
        // enter picks it. Whole-word hits come first, then substrings, then
        // letters in order, each kept in list order.
        let mut ranked: Vec<(u8, usize)> = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let line = line.to_lowercase();
                terms
                    .iter()
                    .map(|term| match_rank(term, &line))
                    .try_fold(0, |worst, rank| rank.map(|rank| worst.max(rank)))
                    .map(|rank| (rank, index))
            })
            .collect();
        ranked.sort();
        self.matches = ranked.into_iter().map(|(_, index)| index).collect();
        self.cursor = 0;
    }

    /// Draws the picker as a panel over `area`.
    pub(crate) fn render(&self, frame: &mut Frame, area: Rect, palette: &Palette, hint: &str) {
        let width = area.width.saturating_sub(8).clamp(20, 100);
        let height = (self.lines.len() as u16)
            .saturating_add(5)
            .clamp(8, area.height.saturating_sub(4).max(8));
        let Some(popup) = crate::ui::centered_popup_rect(area, width, height) else {
            return;
        };
        let Some(inner) =
            crate::ui::render_panel_shell(frame, popup, palette.accent, palette.panel_bg)
        else {
            return;
        };
        if inner.height < 3 {
            return;
        }

        let base = Style::default().bg(palette.panel_bg).fg(palette.text);
        let search = Line::from(vec![
            Span::styled(
                "add to wall > ",
                Style::default()
                    .fg(palette.accent)
                    .bg(palette.panel_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.query.clone(), base),
        ]);
        frame.render_widget(
            Paragraph::new(search).style(base),
            Rect { height: 1, ..inner },
        );
        let cursor_x = inner
            .x
            .saturating_add(14)
            .saturating_add(crate::ui::display_width_u16(&self.query))
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position((cursor_x, inner.y));

        let footer = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
        let body = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(2),
        );
        let visible = usize::from(body.height);
        let first = self.cursor.saturating_sub(visible.saturating_sub(1));
        let mut rows: Vec<Line> = Vec::with_capacity(visible);
        for (offset, index) in self.matches.iter().skip(first).take(visible).enumerate() {
            let selected = first + offset == self.cursor;
            let style = if selected {
                Style::default()
                    .fg(palette.text)
                    .bg(palette.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.subtext0).bg(palette.panel_bg)
            };
            let marker = if selected { "▸ " } else { "  " };
            let line = crate::ui::truncate_end(
                &format!("{marker}{}", self.lines[*index]),
                usize::from(body.width),
            );
            rows.push(Line::from(Span::styled(
                format!("{line:<width$}", width = usize::from(body.width)),
                style,
            )));
        }
        if self.matches.is_empty() {
            rows.push(Line::from(Span::styled(
                "  nothing matches",
                Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            )));
        }
        frame.render_widget(Paragraph::new(rows).style(base), body);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                crate::ui::truncate_end(hint, usize::from(footer.width)),
                Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            )))
            .style(base),
            footer,
        );
    }
}

/// Whether every character of `needle` appears in `haystack` in order, the

/// How well `term` matches `line`: 0 for a whole word, 1 for a substring, 2
/// for its letters in order, `None` for no match.
fn match_rank(term: &str, line: &str) -> Option<u8> {
    if line
        .split(|ch: char| !ch.is_alphanumeric() && ch != ':' && ch != '-' && ch != '_')
        .any(|word| word == term)
    {
        Some(0)
    } else if line.contains(term) {
        Some(1)
    } else if is_subsequence(term, line) {
        Some(2)
    } else {
        None
    }
}

/// loose match fzf makes by default.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle.chars().all(|wanted| haystack.any(|ch| ch == wanted))
}

/// Where fzf is, if it is on PATH.
pub(crate) fn find_fzf() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("fzf"))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Runs fzf over `input` (see `targets::fzf_input`) and returns what it
/// printed, or `None` when it was cancelled.
///
/// The caller must have handed the terminal back first: fzf reads keys from
/// the tty and draws on it, and the wall's raw mode, alternate screen and
/// keyboard protocol would all be in its way.
pub(crate) fn run_fzf(fzf: &std::path::Path, input: &str) -> std::io::Result<Option<String>> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new(fzf)
        .args([
            "--multi",
            "--delimiter=\t",
            "--with-nth=2..",
            "--prompt=herdr wall> ",
            "--header=enter: add tile   tab: mark several   esc: cancel",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A list fzf stops reading is not an error worth failing on: it has
        // what it needs to show, or the person has already left.
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        // 1 is "nothing matched", 130 is esc or ctrl+c: both are a choice of
        // nothing.
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> TerminalKey {
        TerminalKey::new(code, KeyModifiers::empty())
    }

    fn picker() -> Picker {
        Picker::new(vec![
            "agent  claude1        working  fix the parser".into(),
            "space  w2             docs".into(),
            "pane   w3:p1          htop".into(),
        ])
    }

    #[test]
    fn enter_picks_the_first_entry_by_default() {
        let mut picker = picker();

        assert_eq!(
            picker.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::Picked(0)
        );
    }

    #[test]
    fn typing_filters_the_list() {
        let mut picker = picker();
        for ch in "htp".chars() {
            picker.handle_key(&key(KeyCode::Char(ch)));
        }

        assert_eq!(picker.query(), "htp");
        assert_eq!(picker.matches(), &[2]);
        assert_eq!(
            picker.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::Picked(2)
        );
    }

    /// The case that picked a real agent instead of the pane being asked for:
    /// a long name holding the query's letters in order must not outrank the
    /// line whose name is the query.
    #[test]
    fn a_name_matching_the_query_outranks_one_that_merely_contains_its_letters() {
        let mut picker = Picker::new(vec![
            "agent  w13Z:p1  claude  idle  6928C1-balmy_bluetooth-high-brightness-rgbw-floodlight"
                .into(),
            "space  w16K     walltest_old  unknown".into(),
            "pane   w16K:p1  walltest  ryi@pandora:/tmp".into(),
        ]);
        picker.push_text("walltest");

        assert_eq!(picker.matches(), &[2, 1, 0]);
        assert_eq!(
            picker.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::Picked(2)
        );
    }

    #[test]
    fn every_word_of_the_query_must_match() {
        let mut picker = picker();
        picker.push_text("space docs");
        assert_eq!(picker.matches(), &[1]);

        picker.push_text(" nothing");
        assert!(picker.matches().is_empty());
        assert_eq!(picker.handle_key(&key(KeyCode::Enter)), PickerOutcome::Open);
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut picker = picker();
        picker.push_text("zz");
        assert!(picker.matches().is_empty());

        picker.handle_key(&key(KeyCode::Backspace));
        picker.handle_key(&key(KeyCode::Backspace));

        assert_eq!(picker.matches().len(), 3);
    }

    #[test]
    fn arrows_move_and_stop_at_the_ends() {
        let mut picker = picker();
        picker.handle_key(&key(KeyCode::Up));
        assert_eq!(picker.selected(), Some(0));
        picker.handle_key(&key(KeyCode::Down));
        picker.handle_key(&key(KeyCode::Down));
        picker.handle_key(&key(KeyCode::Down));
        assert_eq!(picker.selected(), Some(2));
    }

    #[test]
    fn esc_cancels() {
        let mut picker = picker();

        assert_eq!(
            picker.handle_key(&key(KeyCode::Esc)),
            PickerOutcome::Cancelled
        );
        assert_eq!(
            picker.handle_key(&TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PickerOutcome::Cancelled
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_loose() {
        assert!(is_subsequence("cld", "claude1"));
        assert!(!is_subsequence("dlc", "claude1"));
        let mut picker = picker();
        picker.push_text("CLAUDE");
        assert_eq!(picker.matches(), &[0]);
    }
}
