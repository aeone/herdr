//! One-keystroke jump labels for the sidebar.
//!
//! `switch_workspace` and `focus_agent` are bound to 1-9, which reaches the
//! first nine entries of each list. Past that the number column is blank and
//! the only way across is the navigator or repeated next/previous. Jump mode
//! labels every entry instead, spaces and agents in one namespace, so any of
//! them is one or two keys away.

/// Label characters, in the order they are handed out.
///
/// Digits first so the first nine entries keep the labels `switch_workspace`
/// and `focus_agent` already give them, then letters. Both are typed without a
/// modifier, which is the whole point of the mode.
const ALPHABET: &[u8] = b"123456789abcdefghijklmnopqrstuvwxyz";

/// Assigns a label to each of `count` entries, in list order.
///
/// The labels are prefix-free: no label is the start of another, so a keystroke
/// is either the whole label or unambiguously the first half of a longer one.
/// That is what lets the mode act the instant a label completes, with no timeout
/// to distinguish "a" from the start of "ab".
///
/// Single characters are used while they last. When there are more entries than
/// characters, the tail of the alphabet is spent on prefixes instead — the last
/// characters, so the entries at the top of the list keep the shortest labels.
pub(crate) fn jump_labels(count: usize) -> Vec<String> {
    let alphabet: Vec<char> = ALPHABET.iter().map(|byte| *byte as char).collect();
    let width = alphabet.len();
    if count <= width {
        return alphabet.iter().take(count).map(|c| c.to_string()).collect();
    }

    // Each character spent as a prefix costs one single-character label and buys
    // `width` two-character ones. Take the fewest that cover everything.
    let prefixes = (1..=width)
        .find(|n| (width - n) + n * width >= count)
        .unwrap_or(width);
    let singles = width - prefixes;

    let mut labels: Vec<String> = alphabet
        .iter()
        .take(singles)
        .map(|c| c.to_string())
        .collect();
    for prefix in alphabet.iter().skip(singles) {
        for second in &alphabet {
            if labels.len() == count {
                return labels;
            }
            labels.push(format!("{prefix}{second}"));
        }
    }
    labels
}

/// Somewhere a jump can land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JumpTarget {
    Space {
        ws_idx: usize,
    },
    Agent {
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        /// Position in the Agent panel, so the jump can scroll it into view.
        index: usize,
    },
}

/// Every entry a jump can reach, labelled, in the order the sidebar draws them.
///
/// Spaces then agents, one namespace across both, so a label means exactly one
/// place and the keystroke never depends on which panel you were looking at.
/// Derived rather than stored: the renderer and the key handler both call this,
/// so they cannot disagree about what a label means.
pub(crate) fn jump_entries(app: &crate::app::AppState) -> Vec<(String, JumpTarget)> {
    let spaces = app.visible_workspace_order();
    let agents = crate::ui::agent_panel_entries(app);
    let targets = spaces
        .into_iter()
        .map(|ws_idx| JumpTarget::Space { ws_idx })
        .chain(
            agents
                .iter()
                .enumerate()
                .map(|(index, entry)| JumpTarget::Agent {
                    ws_idx: entry.ws_idx,
                    pane_id: entry.pane_id,
                    index,
                }),
        )
        .collect::<Vec<_>>();
    jump_labels(targets.len())
        .into_iter()
        .zip(targets)
        .collect()
}

/// The labels as the sidebar shows them, keyed by what they point at.
///
/// Labels are padded to a common width so a two-character one does not shift
/// the rest of its row; the unpadded label is what keys are matched against.
pub(crate) struct JumpLabels {
    spaces: std::collections::HashMap<usize, String>,
    agents: std::collections::HashMap<crate::layout::PaneId, String>,
}

impl JumpLabels {
    pub(crate) fn for_app(app: &crate::app::AppState) -> Self {
        let entries = jump_entries(app);
        let width = entries
            .iter()
            .map(|(label, _)| label.chars().count())
            .max()
            .unwrap_or(0);
        let mut spaces = std::collections::HashMap::new();
        let mut agents = std::collections::HashMap::new();
        for (label, target) in entries {
            let padded = format!("{label:<width$}");
            match target {
                JumpTarget::Space { ws_idx } => {
                    spaces.insert(ws_idx, padded);
                }
                JumpTarget::Agent { pane_id, .. } => {
                    agents.insert(pane_id, padded);
                }
            }
        }
        Self { spaces, agents }
    }

    pub(crate) fn space(&self, ws_idx: usize) -> Option<&str> {
        self.spaces.get(&ws_idx).map(String::as_str)
    }

    pub(crate) fn agent(&self, pane_id: crate::layout::PaneId) -> Option<&str> {
        self.agents.get(&pane_id).map(String::as_str)
    }
}

/// What pressing a key in jump mode should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JumpKey {
    /// The typed keys name one entry. Go there and leave the mode.
    Go(JumpTarget),
    /// A prefix of at least one label: wait for the next key.
    Pending,
    /// Nothing starts with this. Leave the mode rather than swallow the key.
    Miss,
}

/// Resolves the keys typed so far against the labels on screen.
pub(crate) fn resolve(entries: &[(String, JumpTarget)], input: &str) -> JumpKey {
    if let Some((_, target)) = entries.iter().find(|(label, _)| label == input) {
        return JumpKey::Go(*target);
    }
    if entries.iter().any(|(label, _)| label.starts_with(input)) {
        return JumpKey::Pending;
    }
    JumpKey::Miss
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(labels: &[&str]) -> Vec<(String, JumpTarget)> {
        labels
            .iter()
            .enumerate()
            .map(|(i, label)| (label.to_string(), JumpTarget::Space { ws_idx: i }))
            .collect()
    }

    #[test]
    fn a_complete_label_goes_straight_there() {
        let entries = entries(&["1", "2", "a"]);

        assert_eq!(
            resolve(&entries, "a"),
            JumpKey::Go(JumpTarget::Space { ws_idx: 2 })
        );
    }

    #[test]
    fn the_first_half_of_a_two_key_label_waits_without_being_ambiguous() {
        // "z" is not a label of its own, which is what lets it wait for a
        // second key without a timeout deciding for it.
        let entries = entries(&["1", "za", "zb"]);

        assert_eq!(resolve(&entries, "z"), JumpKey::Pending);
        assert_eq!(
            resolve(&entries, "zb"),
            JumpKey::Go(JumpTarget::Space { ws_idx: 2 })
        );
    }

    #[test]
    fn a_key_matching_nothing_is_a_miss_rather_than_being_swallowed() {
        let entries = entries(&["1", "2"]);

        assert_eq!(resolve(&entries, "q"), JumpKey::Miss);
        assert_eq!(resolve(&entries, "1q"), JumpKey::Miss);
    }

    #[test]
    fn short_lists_get_one_character_each_starting_with_the_digits() {
        let labels = jump_labels(12);

        assert_eq!(labels[0], "1");
        assert_eq!(labels[8], "9");
        // The tenth entry is the first the existing 1-9 bindings cannot reach.
        assert_eq!(labels[9], "a");
        assert_eq!(labels[11], "c");
    }

    #[test]
    fn a_list_that_exactly_fills_the_alphabet_uses_no_prefixes() {
        let labels = jump_labels(ALPHABET.len());

        assert_eq!(labels.len(), ALPHABET.len());
        assert!(labels.iter().all(|label| label.chars().count() == 1));
        assert_eq!(labels.last().map(String::as_str), Some("z"));
    }

    #[test]
    fn a_longer_list_spends_the_last_characters_on_prefixes() {
        // 67 entries: one prefix character covers it, so only "z" is given up.
        let labels = jump_labels(67);

        assert_eq!(labels.len(), 67);
        assert_eq!(labels[33], "y");
        assert_eq!(labels[34], "z1");
        assert_eq!(labels[35], "z2");
        assert!(!labels.contains(&"z".to_string()));
    }

    #[test]
    fn labels_are_prefix_free_so_no_keystroke_has_to_wait() {
        for count in [1, 34, 35, 36, 67, 200, 500] {
            let labels = jump_labels(count);
            assert_eq!(labels.len(), count, "{count} entries");
            for (i, label) in labels.iter().enumerate() {
                for (j, other) in labels.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    assert!(
                        !other.starts_with(label.as_str()),
                        "{count} entries: {label:?} is a prefix of {other:?}, \
                         so pressing it could not act without waiting"
                    );
                }
            }
        }
    }

    #[test]
    fn labels_are_unique() {
        for count in [10, 35, 67, 500] {
            let labels = jump_labels(count);
            let unique: std::collections::HashSet<&String> = labels.iter().collect();
            assert_eq!(unique.len(), labels.len(), "{count} entries");
        }
    }

    #[test]
    fn a_list_too_long_for_two_characters_is_capped_rather_than_mislabelled() {
        // 35 prefixes * 35 each is the ceiling. Beyond it the extra entries get
        // no label at all, which is visible, rather than a duplicate one.
        let labels = jump_labels(5_000);

        assert!(labels.len() <= ALPHABET.len() * ALPHABET.len());
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }
}
