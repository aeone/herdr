//! What a wall can show, and how a list of it travels and is picked from.
//!
//! The list is built from the API the way `herdr focus` resolves a name --
//! agents, spaces and panes, each standing for one terminal -- but where it is
//! built depends on where the server is: here for a local wall, and on the far
//! side of ssh for `herdr wall --remote`, whose bridge forwards only the client
//! socket. So the list is plain data with a JSON form, printed by
//! `herdr wall --targets-json` and read back by whichever side needs it.

use serde::{Deserialize, Serialize};

/// What kind of name a target was listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WallTargetKind {
    Agent,
    Space,
    Pane,
}

impl WallTargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Space => "space",
            Self::Pane => "pane",
        }
    }
}

/// One thing a tile can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WallTarget {
    pub(crate) kind: WallTargetKind,
    /// The name a person would type for it, as `herdr focus` accepts it.
    pub(crate) target: String,
    /// The terminal it stands for. Several targets often stand for the same
    /// one -- an agent, its pane and the space it is in -- and a tile is
    /// keyed by this, not by the name it was picked under.
    pub(crate) terminal_id: String,
    /// What a tile showing it is titled.
    pub(crate) label: String,
    /// The rest of the picker line: status, title, cwd.
    #[serde(default)]
    pub(crate) detail: String,
    /// The terminal's size when the list was made. Absent from a server too old
    /// to report it, in which case a tile asks for frames at a guess.
    #[serde(default)]
    pub(crate) cols: Option<u16>,
    #[serde(default)]
    pub(crate) rows: Option<u16>,
}

impl WallTarget {
    /// The terminal's size as `(cols, rows)`, when the list knew it.
    pub(crate) fn size(&self) -> Option<(u16, u16)> {
        match (self.cols, self.rows) {
            (Some(cols), Some(rows)) if cols > 0 && rows > 0 => Some((cols, rows)),
            _ => None,
        }
    }

    /// The line a picker shows for this target: kind, name, then detail, in
    /// columns so a list of them reads as a table.
    pub(crate) fn picker_line(&self) -> String {
        let line = format!(
            "{:<6} {:<14} {}",
            self.kind.as_str(),
            self.target,
            self.detail
        );
        line.trim_end().to_owned()
    }
}

/// Reads the list `herdr wall --targets-json` printed.
pub(crate) fn parse_targets_json(raw: &str) -> Result<Vec<WallTarget>, String> {
    serde_json::from_str(raw.trim()).map_err(|err| format!("unexpected target list: {err}"))
}

/// The lines fed to fzf: the target's index, a tab, and its picker line.
///
/// fzf is told to show only what follows the tab and prints whole lines back,
/// so the index survives the round trip however the line is filtered, and two
/// targets with the same text are still told apart.
pub(crate) fn fzf_input(targets: &[WallTarget]) -> String {
    let mut input = String::new();
    for (index, target) in targets.iter().enumerate() {
        input.push_str(&index.to_string());
        input.push('\t');
        input.push_str(&target.picker_line().replace(['\t', '\n'], " "));
        input.push('\n');
    }
    input
}

/// The targets fzf printed back, as indexes into the list it was fed, in the
/// order they were chosen. Lines that do not start with a known index are
/// dropped rather than trusted.
pub(crate) fn parse_fzf_output(output: &str, count: usize) -> Vec<usize> {
    output
        .lines()
        .filter_map(|line| line.split('\t').next()?.trim().parse::<usize>().ok())
        .filter(|index| *index < count)
        .collect()
}

#[cfg(test)]
pub(crate) fn test_target(kind: WallTargetKind, target: &str, terminal_id: &str) -> WallTarget {
    WallTarget {
        kind,
        target: target.to_owned(),
        terminal_id: terminal_id.to_owned(),
        label: format!("{} {target}", kind.as_str()),
        detail: String::new(),
        cols: None,
        rows: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_list_reads_back_what_was_printed() {
        let mut agent = test_target(WallTargetKind::Agent, "claude1", "term_1");
        agent.cols = Some(120);
        agent.rows = Some(40);
        let space = test_target(WallTargetKind::Space, "w2", "term_2");
        let printed = serde_json::to_string(&vec![agent.clone(), space.clone()]).unwrap();

        let parsed = parse_targets_json(&printed).expect("parses");

        assert_eq!(parsed, vec![agent, space]);
        assert_eq!(parsed[0].size(), Some((120, 40)));
        assert_eq!(parsed[1].size(), None);
    }

    /// A far side one build behind may leave the size out, and one ahead may
    /// add fields this build has no name for; neither may lose the list.
    #[test]
    fn a_target_list_tolerates_missing_and_unknown_fields() {
        let raw = r#"[{"kind":"pane","target":"w1:p1","terminal_id":"t1","label":"pane w1:p1","future":true}]"#;

        let parsed = parse_targets_json(raw).expect("parses");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].detail, "");
        assert_eq!(parsed[0].size(), None);
    }

    #[test]
    fn garbage_is_an_error_not_an_empty_list() {
        assert!(parse_targets_json("no herdr found on this host").is_err());
    }

    #[test]
    fn fzf_round_trips_indexes_through_its_output() {
        let targets = vec![
            test_target(WallTargetKind::Agent, "claude1", "t1"),
            test_target(WallTargetKind::Space, "w2", "t2"),
            test_target(WallTargetKind::Pane, "w3:p1", "t3"),
        ];
        let input = fzf_input(&targets);
        let lines: Vec<&str> = input.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("1\tspace  w2"));

        // fzf prints the chosen lines whole, in the order they were marked.
        let output = format!("{}\n{}\n", lines[2], lines[0]);

        assert_eq!(parse_fzf_output(&output, targets.len()), vec![2, 0]);
    }

    #[test]
    fn fzf_output_with_unknown_indexes_is_ignored() {
        assert_eq!(
            parse_fzf_output("7\tpane x\nnot a line\n\n", 3),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn a_picker_line_has_no_tab_to_confuse_fzf() {
        let mut target = test_target(WallTargetKind::Agent, "a", "t");
        target.detail = "title\twith tab".into();

        assert_eq!(fzf_input(&[target]).matches('\t').count(), 1);
    }
}
