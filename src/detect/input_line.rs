//! Tells typed text from ghost text in an agent's prompt box.
//!
//! Claude Code draws its autosuggestion (and the empty-prompt hint) with SGR 2
//! after the prompt glyph; text a person typed keeps normal intensity. Plain
//! text output cannot tell them apart, so this works on cells that carry a
//! faint flag.

/// One screen cell of a prompt row: its text and whether it is drawn faint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputCell {
    pub text: String,
    pub faint: bool,
}

/// What the prompt box holds: text a person typed, and ghost text drawn after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputLine {
    pub draft: String,
    pub suggestion: String,
}

const CLAUDE_PROMPT_GLYPH: char = '❯';
const BORDER_GLYPH: char = '─';
const MIN_BORDER_RUN: usize = 8;
/// Claude indents continuation rows of a multi-line draft to sit under the text.
const CONTINUATION_INDENT: usize = 2;

fn is_border(row: &str) -> bool {
    row.chars().filter(|ch| *ch == BORDER_GLYPH).count() >= MIN_BORDER_RUN
        && row
            .chars()
            .all(|ch| ch == BORDER_GLYPH || ch.is_whitespace())
}

/// Finds the rows between the prompt box borders, as an inclusive-exclusive
/// range into `rows` (the bottom of the screen, trailing blanks trimmed).
///
/// The box is the bottom-most pair of border rows with a `❯` row right under
/// the upper one. Both borders are required, so a transcript echo of an old
/// prompt, a permission dialog or a plain shell never matches.
pub fn locate_claude_input_rows(rows: &[String]) -> Option<std::ops::Range<usize>> {
    let bottom = rows.iter().rposition(|row| is_border(row))?;
    let top = rows[..bottom].iter().rposition(|row| is_border(row))?;
    let first = top + 1;
    if first >= bottom || !rows[first].trim_start().starts_with(CLAUDE_PROMPT_GLYPH) {
        return None;
    }
    Some(first..bottom)
}

/// Splits the cells of the located input rows into draft and suggestion.
///
/// `rows` are the cells of each input row in order; `soft_wrapped[i]` says row
/// `i` continues onto row `i + 1` without a line break typed in between.
pub fn split_claude_input(rows: &[Vec<InputCell>], soft_wrapped: &[bool]) -> InputLine {
    let mut draft = String::new();
    let mut suggestion = String::new();
    let mut previous_wrapped = false;
    for (idx, cells) in rows.iter().enumerate() {
        let (text_start, max_indent) = if idx == 0 {
            // The glyph, then one blank (a no-break space in practice).
            let glyph = cells.iter().position(|cell| !is_blank_cell(cell));
            (glyph.map_or(cells.len(), |pos| pos + 1), 1)
        } else {
            (0, CONTINUATION_INDENT)
        };
        let indent = cells[text_start.min(cells.len())..]
            .iter()
            .take(max_indent)
            .take_while(|cell| is_blank_cell(cell))
            .count();
        let mut typed = String::new();
        let mut ghost = String::new();
        for cell in &cells[(text_start + indent).min(cells.len())..] {
            let target = if cell.faint { &mut ghost } else { &mut typed };
            target.push_str(if cell.text.is_empty() {
                " "
            } else {
                &cell.text
            });
        }
        let wrapped = soft_wrapped.get(idx).copied().unwrap_or(false);
        // A soft-wrapped row ends in the space the wrap broke at; keep it.
        let typed = if wrapped {
            typed.as_str()
        } else {
            typed.trim_end_matches(is_blank)
        };
        if idx > 0 && !previous_wrapped && !typed.is_empty() {
            draft.push('\n');
        }
        draft.push_str(typed);
        let ghost = ghost.trim_end_matches(is_blank);
        if !ghost.is_empty() {
            if !suggestion.is_empty() {
                suggestion.push('\n');
            }
            suggestion.push_str(ghost);
        }
        previous_wrapped = wrapped;
    }
    InputLine {
        draft: draft.trim_end_matches(is_blank).to_string(),
        suggestion,
    }
}

fn is_blank(ch: char) -> bool {
    ch == ' ' || ch == '\u{a0}'
}

fn is_blank_cell(cell: &InputCell) -> bool {
    cell.text.chars().all(is_blank)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(plain: &str, faint: &str) -> Vec<InputCell> {
        let cell = |ch: char, faint| InputCell {
            text: ch.to_string(),
            faint,
        };
        plain
            .chars()
            .map(|ch| cell(ch, false))
            .chain(faint.chars().map(|ch| cell(ch, true)))
            .collect()
    }

    fn border() -> String {
        "─".repeat(40)
    }

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn suggestion_after_glyph_is_not_a_draft() {
        let input = split_claude_input(&[row("❯\u{a0}", "it's rebuilt now")], &[false]);
        assert_eq!(input.draft, "");
        assert_eq!(input.suggestion, "it's rebuilt now");
    }

    #[test]
    fn typed_text_is_a_draft() {
        let input = split_claude_input(&[row("❯\u{a0}half typed", "")], &[false]);
        assert_eq!(input.draft, "half typed");
        assert_eq!(input.suggestion, "");
    }

    #[test]
    fn typed_text_followed_by_ghost_text_keeps_both() {
        let input = split_claude_input(&[row("❯\u{a0}git ", "status")], &[false]);
        assert_eq!(input.draft, "git");
        assert_eq!(input.suggestion, "status");
    }

    #[test]
    fn empty_prompt_has_neither() {
        let input = split_claude_input(&[row("❯\u{a0}", "")], &[false]);
        assert_eq!(input, InputLine::default());
    }

    #[test]
    fn multi_line_draft_drops_continuation_indent() {
        let input = split_claude_input(
            &[row("❯\u{a0}first", ""), row("  second", "")],
            &[false, false],
        );
        assert_eq!(input.draft, "first\nsecond");
    }

    #[test]
    fn soft_wrapped_draft_joins_without_a_break() {
        let input = split_claude_input(
            &[row("❯\u{a0}abc def ", ""), row("  ghi", "")],
            &[true, false],
        );
        assert_eq!(input.draft, "abc def ghi");
    }

    #[test]
    fn locates_the_bottom_prompt_box() {
        let b = border();
        let rows = rows(&[
            "❯ an old echoed prompt",
            "  reply",
            &b,
            "❯\u{a0}draft",
            &b,
            "  status line",
        ]);
        assert_eq!(locate_claude_input_rows(&rows), Some(3..4));
    }

    #[test]
    fn multi_line_box_spans_all_input_rows() {
        let b = border();
        let rows = rows(&[&b, "❯\u{a0}one", "  two", &b, "  status"]);
        assert_eq!(locate_claude_input_rows(&rows), Some(1..3));
    }

    #[test]
    fn no_box_means_no_input() {
        let b = border();
        assert_eq!(locate_claude_input_rows(&rows(&["$ ls", "file"])), None);
        assert_eq!(
            locate_claude_input_rows(&rows(&["❯ echoed", &b, "Do you want to proceed?"])),
            None
        );
    }
}
