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
    /// Everything else worth matching a query against: kind, status, space,
    /// terminal title, cwd. This build's picker does not show it -- it lays
    /// the same facts out from the fields below -- but it keeps its old
    /// columnar form so an older wall reading this list still has a line.
    #[serde(default)]
    pub(crate) detail: String,
    /// The terminal's size when the list was made. Absent from a server too old
    /// to report it, in which case a tile asks for frames at a guess.
    #[serde(default)]
    pub(crate) cols: Option<u16>,
    #[serde(default)]
    pub(crate) rows: Option<u16>,
    /// What a person calls it: an agent's reported or given name, a space's
    /// label, a pane's label or title. Empty from a server that predates the
    /// field, and the picker falls back to `label`.
    #[serde(default)]
    pub(crate) title: String,
    /// The label of the space it is in, for agents and panes.
    #[serde(default)]
    pub(crate) space: String,
    /// The host it really runs on, when it is a mirror of a pane elsewhere.
    #[serde(default)]
    pub(crate) host: Option<String>,
    /// The API's agent status word -- idle, working, blocked, done, shells,
    /// unknown -- or empty for a plain pane.
    #[serde(default)]
    pub(crate) status: String,
    /// Unix ms of the last sign of use: when its agent last changed state,
    /// which is the moment a prompt set it working or it finished and handed
    /// back. For a space, the latest of its panes'. Absent when nothing was
    /// recorded, and from a server too old to send it; such targets sort after
    /// the ones that have it.
    #[serde(default)]
    pub(crate) last_used_ms: Option<u64>,
}

impl WallTarget {
    /// The terminal's size as `(cols, rows)`, when the list knew it.
    pub(crate) fn size(&self) -> Option<(u16, u16)> {
        match (self.cols, self.rows) {
            (Some(cols), Some(rows)) if cols > 0 && rows > 0 => Some((cols, rows)),
            _ => None,
        }
    }

    /// The name shown for it, falling back to the tile label for a list from
    /// a server that did not send one.
    fn display_title(&self) -> &str {
        let title = self.title.trim();
        if title.is_empty() {
            self.label.trim()
        } else {
            title
        }
    }

    /// Where it is: the space, after the host when it runs on another one.
    fn place(&self) -> String {
        let space = self.space.trim();
        match self.host.as_deref().map(str::trim) {
            Some(host) if !host.is_empty() && !space.is_empty() => format!("{host} · {space}"),
            Some(host) if !host.is_empty() => host.to_owned(),
            _ => space.to_owned(),
        }
    }

    /// The mark a line starts with. An agent's says what it is doing, in
    /// shapes that read without colour, since fzf shows them plain; a space or
    /// a pane gets a fixed outline mark instead, so the agents stand out.
    fn glyph(&self) -> &'static str {
        match self.kind {
            WallTargetKind::Space => "◇",
            WallTargetKind::Pane => "▫",
            WallTargetKind::Agent => match self.status.as_str() {
                "working" => "●",
                "blocked" => "×",
                "done" => "✓",
                "shells" => "◐",
                "idle" => "○",
                _ => "·",
            },
        }
    }
}

/// Puts the list in the order a person looks for things in: agents first,
/// since they are what is usually watched, then spaces, then plain panes; and
/// within each, most recently used first. Targets with no recorded use keep
/// their listed order after the rest, so a list from an older server reads as
/// it always did.
pub(crate) fn order_by_recency(targets: &mut [WallTarget]) {
    targets.sort_by_key(|target| {
        let kind = match target.kind {
            WallTargetKind::Agent => 0,
            WallTargetKind::Space => 1,
            WallTargetKind::Pane => 2,
        };
        // Reverse puts the newest first, and `None` -- which orders below any
        // `Some` -- last.
        (kind, std::cmp::Reverse(target.last_used_ms))
    });
}

/// How long ago `at_ms` was, as a picker shows it: "just now", "5m ago",
/// "3h ago", "2d ago", "6w ago". Empty when nothing was recorded. A time in
/// the future -- another host's clock running ahead -- reads as just now.
pub(crate) fn format_age(now_ms: u64, at_ms: Option<u64>) -> String {
    let Some(at_ms) = at_ms else {
        return String::new();
    };
    const MINUTE: u64 = 60 * 1000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    match now_ms.saturating_sub(at_ms) {
        age if age < MINUTE => "just now".to_owned(),
        age if age < HOUR => format!("{}m ago", age / MINUTE),
        age if age < DAY => format!("{}h ago", age / HOUR),
        age if age < 2 * WEEK => format!("{}d ago", age / DAY),
        age => format!("{}w ago", age / WEEK),
    }
}

/// Unix ms now, for aging the list. A clock before 1970 reads as 0, which
/// makes every age "just now" rather than failing the picker.
pub(crate) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One picker entry, in columns, before it is laid out to a width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerRow {
    pub(crate) glyph: &'static str,
    pub(crate) title: String,
    pub(crate) place: String,
    pub(crate) status: String,
    pub(crate) age: String,
    /// The name it can be typed as, kept out of the line but matched against.
    pub(crate) id: String,
    /// Spaces and panes are drawn dimmer than agents.
    pub(crate) dim: bool,
    /// What a query is matched against, lowercased: the visible columns, the
    /// id and the target's detail, so a pane id or a cwd still finds it.
    pub(crate) search: String,
}

impl PickerRow {
    /// A row that is only a title, for tests of the picker itself.
    #[cfg(test)]
    pub(crate) fn plain(text: &str) -> Self {
        Self {
            glyph: "·",
            title: text.to_owned(),
            place: String::new(),
            status: String::new(),
            age: String::new(),
            id: String::new(),
            dim: false,
            search: text.to_lowercase(),
        }
    }
}

/// The picker's rows for `targets`, in the order given.
///
/// Ids stay out of the line, except where two entries would otherwise read
/// the same -- two unnamed claudes in one space, say -- and there the id is
/// what tells them apart, so it joins the title.
pub(crate) fn picker_rows(targets: &[WallTarget], now_ms: u64) -> Vec<PickerRow> {
    let mut seen: std::collections::HashMap<(&str, String), usize> =
        std::collections::HashMap::new();
    for target in targets {
        *seen
            .entry((target.display_title(), target.place()))
            .or_default() += 1;
    }
    targets
        .iter()
        .map(|target| {
            let place = target.place();
            let ambiguous = seen
                .get(&(target.display_title(), place.clone()))
                .is_some_and(|count| *count > 1);
            let mut title = target.display_title().to_owned();
            if ambiguous && title != target.target {
                title = format!("{title} ({})", target.target);
            }
            let status = target.status.trim().to_owned();
            let search = [
                target.kind.as_str(),
                &title,
                &place,
                &status,
                &target.target,
                &target.detail,
            ]
            .join(" ")
            .to_lowercase();
            PickerRow {
                glyph: target.glyph(),
                title,
                place,
                status,
                age: format_age(now_ms, target.last_used_ms),
                id: target.target.clone(),
                dim: target.kind != WallTargetKind::Agent,
                search,
            }
        })
        .collect()
}

/// Lays `rows` out as aligned lines no wider than `width` cells.
///
/// Columns are sized across all the rows, not per line, so they line up
/// however the list is scrolled or filtered. Status and age are short and
/// kept whole; when the rest does not fit, the place gives way to a third of
/// the room and the title takes what is left, each cut with an ellipsis.
pub(crate) fn format_rows(rows: &[PickerRow], width: usize) -> Vec<String> {
    use crate::ui::{display_width, truncate_end};

    let widest = |column: fn(&PickerRow) -> &str| {
        rows.iter()
            .map(|row| display_width(column(row)))
            .max()
            .unwrap_or(0)
    };
    let title_natural = widest(|row| &row.title);
    let place_natural = widest(|row| &row.place);
    let status_width = widest(|row| &row.status);
    let age_width = widest(|row| &row.age);

    // The glyph and its space, then a two-cell gap before each further column
    // that has anything in it.
    let gap = |column_width: usize| if column_width > 0 { 2 } else { 0 };
    let fixed =
        2 + gap(place_natural) + gap(status_width) + status_width + gap(age_width) + age_width;
    let room = width.saturating_sub(fixed).max(8);
    let (title_width, place_width) = if title_natural + place_natural <= room {
        (title_natural, place_natural)
    } else {
        let place_width = place_natural.min((room / 3).max(6)).min(room);
        let title_width = title_natural.min(room - place_width);
        // A short title gives the room it does not need back to the place.
        (title_width, place_natural.min(room - title_width))
    };

    let pad = |text: &str, column_width: usize| {
        let text = truncate_end(text, column_width);
        let fill = column_width.saturating_sub(display_width(&text));
        format!("{text}{}", " ".repeat(fill))
    };
    rows.iter()
        .map(|row| {
            let mut line = format!("{} {}", row.glyph, pad(&row.title, title_width));
            if place_natural > 0 {
                line.push_str("  ");
                line.push_str(&pad(&row.place, place_width));
            }
            if status_width > 0 {
                line.push_str("  ");
                line.push_str(&pad(&row.status, status_width));
            }
            if age_width > 0 {
                // Right-aligned, so "2m ago" and "13h ago" end in one column.
                let fill = age_width.saturating_sub(display_width(&row.age));
                line.push_str("  ");
                line.push_str(&" ".repeat(fill));
                line.push_str(&row.age);
            }
            truncate_end(line.trim_end(), width)
        })
        .collect()
}

/// Reads the list `herdr wall --targets-json` printed.
pub(crate) fn parse_targets_json(raw: &str) -> Result<Vec<WallTarget>, String> {
    serde_json::from_str(raw.trim()).map_err(|err| format!("unexpected target list: {err}"))
}

/// The lines fed to fzf: the target's index, a tab, its picker line laid out
/// to `width`, and its id, dimmed.
///
/// fzf is told to show only what follows the tab and prints whole lines back,
/// so the index survives the round trip however the line is filtered, and two
/// targets with the same text are still told apart. The id trails the line
/// rather than hiding in a field of its own because fzf matches only what it
/// shows: on a narrow screen it runs off the edge, but typing it still finds
/// the line. A space's or pane's mark is dimmed too, so the agents stand out.
pub(crate) fn fzf_input(targets: &[WallTarget], now_ms: u64, width: usize) -> String {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";
    // Escapes are fzf's to interpret under --ansi, so none may come from a
    // title; tabs and newlines would split the line.
    let clean = |text: &str| text.replace(['\t', '\n', '\r', '\x1b'], " ");
    let rows = picker_rows(targets, now_ms);
    let lines = format_rows(&rows, width);
    let mut input = String::new();
    for (index, (row, line)) in rows.iter().zip(lines).enumerate() {
        let line = clean(&line);
        input.push_str(&index.to_string());
        input.push('\t');
        match line.strip_prefix(row.glyph) {
            Some(rest) if row.dim => {
                input.push_str(DIM);
                input.push_str(row.glyph);
                input.push_str(RESET);
                input.push_str(rest);
            }
            _ => input.push_str(&line),
        }
        input.push_str("  ");
        input.push_str(DIM);
        input.push_str(&clean(&row.id));
        input.push_str(RESET);
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
        title: String::new(),
        space: String::new(),
        host: None,
        status: String::new(),
        last_used_ms: None,
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
        let input = fzf_input(&targets, 0, 80);
        let lines: Vec<&str> = input.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("1\t"));
        assert!(lines[1].contains("space w2"));

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
    fn a_picker_line_has_no_tab_or_escape_to_confuse_fzf() {
        let mut target = test_target(WallTargetKind::Agent, "a\tb", "t");
        target.title = "title\twith tab\x1b[31m".into();

        let input = fzf_input(&[target], 0, 80);

        assert_eq!(input.matches('\t').count(), 1);
        // Only the dimming fzf_input adds itself: on and off around the id.
        assert_eq!(input.matches('\x1b').count(), 2);
    }

    const MINUTE: u64 = 60 * 1000;
    const NOW: u64 = 1_750_000_000_000;

    fn agent(target: &str, title: &str, status: &str, used: Option<u64>) -> WallTarget {
        let mut agent = test_target(WallTargetKind::Agent, target, &format!("t-{target}"));
        agent.title = title.into();
        agent.space = "rycelia".into();
        agent.status = status.into();
        agent.last_used_ms = used;
        agent
    }

    fn names(targets: &[WallTarget]) -> Vec<&str> {
        targets
            .iter()
            .map(|target| target.target.as_str())
            .collect()
    }

    #[test]
    fn agents_come_first_and_each_kind_is_newest_first() {
        let mut space_new = test_target(WallTargetKind::Space, "w-new", "t1");
        space_new.last_used_ms = Some(NOW);
        let mut space_old = test_target(WallTargetKind::Space, "w-old", "t2");
        space_old.last_used_ms = Some(NOW - 60 * MINUTE);
        let mut pane_used = test_target(WallTargetKind::Pane, "p-used", "t3");
        pane_used.last_used_ms = Some(NOW - 5 * MINUTE);
        let mut targets = vec![
            test_target(WallTargetKind::Pane, "p-never", "t4"),
            space_old,
            agent("a-never", "", "idle", None),
            pane_used,
            agent("a-old", "", "idle", Some(NOW - 30 * MINUTE)),
            space_new,
            agent("a-new", "", "working", Some(NOW - MINUTE)),
        ];

        order_by_recency(&mut targets);

        assert_eq!(
            names(&targets),
            vec!["a-new", "a-old", "a-never", "w-new", "w-old", "p-used", "p-never"]
        );
    }

    /// A list from a server that sends no times keeps its own order, which
    /// already put agents first.
    #[test]
    fn a_list_without_times_keeps_its_order() {
        let mut targets = vec![
            agent("a1", "", "idle", None),
            agent("a2", "", "idle", None),
            test_target(WallTargetKind::Space, "w1", "t"),
        ];

        order_by_recency(&mut targets);

        assert_eq!(names(&targets), vec!["a1", "a2", "w1"]);
    }

    #[test]
    fn ages_read_like_a_person_would_say_them() {
        assert_eq!(format_age(NOW, None), "");
        assert_eq!(format_age(NOW, Some(NOW - 20_000)), "just now");
        assert_eq!(format_age(NOW, Some(NOW + 90_000)), "just now");
        assert_eq!(format_age(NOW, Some(NOW - 2 * MINUTE)), "2m ago");
        assert_eq!(format_age(NOW, Some(NOW - 59 * MINUTE)), "59m ago");
        assert_eq!(format_age(NOW, Some(NOW - 3 * 60 * MINUTE)), "3h ago");
        assert_eq!(format_age(NOW, Some(NOW - 50 * 60 * MINUTE)), "2d ago");
        assert_eq!(format_age(NOW, Some(NOW - 21 * 24 * 60 * MINUTE)), "3w ago");
    }

    #[test]
    fn an_agent_line_reads_name_place_status_age() {
        let mut working = agent(
            "w11S:p1",
            "SlidePad internal frames window",
            "working",
            Some(NOW - 2 * MINUTE),
        );
        working.host = Some("val".into());
        let idle = agent("w2:p1", "docs", "idle", Some(NOW - 3 * 60 * MINUTE));

        let lines = format_rows(&picker_rows(&[working, idle], NOW), 100);

        assert_eq!(
            lines,
            vec![
                "● SlidePad internal frames window  val · rycelia  working  2m ago",
                "○ docs                             rycelia        idle     3h ago",
            ]
        );
        // The id is not in the line, but it is matched against.
        assert!(!lines[0].contains("w11S"));
        assert!(picker_rows(&[agent("w11S:p1", "x", "idle", None)], NOW)[0]
            .search
            .contains("w11s:p1"));
    }

    #[test]
    fn a_narrow_list_cuts_the_title_and_place_but_keeps_status_and_age() {
        let mut target = agent(
            "w1:p1",
            "a very long agent title that cannot possibly fit",
            "working",
            Some(NOW - 2 * MINUTE),
        );
        target.space = "a rather long space label".into();

        let lines = format_rows(&picker_rows(&[target], NOW), 50);

        assert_eq!(crate::ui::display_width(&lines[0]), 50);
        assert!(lines[0].ends_with("working  2m ago"), "{}", lines[0]);
        assert!(lines[0].contains('…'));
        assert!(lines[0].starts_with("● a very long"));
    }

    #[test]
    fn identical_lines_are_told_apart_by_their_ids() {
        let rows = picker_rows(
            &[
                agent("w1:p1", "claude", "idle", None),
                agent("w1:p2", "claude", "idle", None),
                agent("w1:p3", "codex", "idle", None),
            ],
            NOW,
        );

        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, vec!["claude (w1:p1)", "claude (w1:p2)", "codex"]);
    }

    #[test]
    fn spaces_and_panes_are_dim_and_marked_by_kind() {
        let mut space = test_target(WallTargetKind::Space, "w1", "t1");
        space.title = "parser".into();
        space.status = "working".into();
        let mut pane = test_target(WallTargetKind::Pane, "w1:p2", "t2");
        pane.title = "htop".into();
        pane.space = "parser".into();

        let rows = picker_rows(&[space, pane], NOW);

        assert_eq!((rows[0].glyph, rows[0].dim), ("◇", true));
        assert_eq!((rows[1].glyph, rows[1].dim), ("▫", true));
        assert_eq!(rows[1].place, "parser");
    }

    /// A list from a server that predates the friendlier picker has none of
    /// its fields; the line falls back to the tile label.
    #[test]
    fn a_list_from_an_older_server_still_reads() {
        let raw = r#"[{"kind":"agent","target":"claude1","terminal_id":"t1","label":"claude claude1 · parser","detail":"claude   idle     parser"}]"#;

        let parsed = parse_targets_json(raw).expect("parses");

        assert_eq!(parsed[0].last_used_ms, None);
        assert_eq!(parsed[0].host, None);
        let rows = picker_rows(&parsed, NOW);
        assert_eq!(rows[0].title, "claude claude1 · parser");
        assert_eq!(format_rows(&rows, 80), vec!["· claude claude1 · parser"]);
    }

    #[test]
    fn the_new_fields_round_trip() {
        let mut target = agent("w1:p1", "fixer", "working", Some(NOW));
        target.host = Some("sera".into());
        let printed = serde_json::to_string(&vec![target.clone()]).expect("serializes");

        assert_eq!(parse_targets_json(&printed).expect("parses"), vec![target]);
    }
}
