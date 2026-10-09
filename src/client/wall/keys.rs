//! Which keys the wall keeps for itself.
//!
//! Everything else goes to the active tile. The wall's own keys follow the
//! prefix, as herdr's do, and reuse herdr's bindings for the same ideas so
//! nothing new has to be learned: the pane focus keys (and arrows) move
//! between tiles, `close_pane` closes one, `cycle_pane_*` steps through them,
//! and `detach` -- or `q`, as `herdr focus` has it -- leaves. `wall_add` opens
//! the picker, as does `goto`, which is what herdr's own "pick something" key
//! is and which keeps the wall usable while `wall_add` is unset. Bindings are
//! read from this machine's config, like the attach escape and the image
//! paste key, never from the server being looked at.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

use super::layout::Direction;
use crate::config::ActionKeybinds;
use crate::input::TerminalKey;

type KeyCombo = (KeyCode, KeyModifiers);

/// The wall's keys, resolved from config.
#[derive(Debug, Clone)]
pub(crate) struct WallKeys {
    prefix: KeyCombo,
    add: ActionKeybinds,
    goto: ActionKeybinds,
    close: ActionKeybinds,
    detach: ActionKeybinds,
    left: ActionKeybinds,
    down: ActionKeybinds,
    up: ActionKeybinds,
    right: ActionKeybinds,
    next: ActionKeybinds,
    previous: ActionKeybinds,
}

/// What one key means to the wall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyAction {
    /// Send it to the active tile.
    Forward,
    /// The prefix: the next key is the wall's.
    BeginPrefix,
    /// A key after the prefix that the wall has no use for: the prefix and
    /// the key both go to the active tile, as `herdr focus` sends them.
    ForwardAfterPrefix,
    /// The prefix twice: send the prefix itself.
    SendPrefix,
    OpenPicker,
    Focus(Direction),
    Cycle(isize),
    CloseTile,
    Quit,
    /// Nothing to do, and nothing to send.
    Ignore,
}

impl WallKeys {
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let keybinds = config.keybinds();
        Self {
            prefix: config.prefix_key(),
            add: keybinds.wall_add,
            goto: keybinds.goto,
            close: keybinds.close_pane,
            detach: keybinds.detach,
            left: keybinds.focus_pane_left,
            down: keybinds.focus_pane_down,
            up: keybinds.focus_pane_up,
            right: keybinds.focus_pane_right,
            next: keybinds.cycle_pane_next,
            previous: keybinds.cycle_pane_previous,
        }
    }

    /// How to say "add a tile" in a hint: the configured keys, or the
    /// navigator key when none are.
    pub(crate) fn add_label(&self) -> String {
        self.add
            .label()
            .or_else(|| self.goto.label())
            .unwrap_or_else(|| "prefix+g".to_owned())
    }

    /// The prefix as a key, to pass it on to the tile.
    pub(crate) fn prefix_key(&self) -> TerminalKey {
        TerminalKey::new(self.prefix.0, self.prefix.1)
    }

    pub(crate) fn prefix_label(&self) -> String {
        crate::config::format_key_combo(self.prefix)
    }

    pub(crate) fn classify(&self, key: &TerminalKey, prefix_pending: bool) -> KeyAction {
        if key.kind == KeyEventKind::Release || matches!(key.code, KeyCode::Modifier(_)) {
            return if prefix_pending {
                KeyAction::Ignore
            } else {
                KeyAction::Forward
            };
        }
        let is_prefix = crate::config::terminal_key_matches_combo(key, self.prefix);
        if !prefix_pending {
            if is_prefix {
                return KeyAction::BeginPrefix;
            }
            if self.add.matches_direct_key(key) || self.goto.matches_direct_key(key) {
                return KeyAction::OpenPicker;
            }
            return KeyAction::Forward;
        }

        if is_prefix {
            return KeyAction::SendPrefix;
        }
        if self.detach.matches_prefix_key(key)
            || (key.modifiers.is_empty() && key.code == KeyCode::Char('q'))
        {
            return KeyAction::Quit;
        }
        if self.add.matches_prefix_key(key) || self.goto.matches_prefix_key(key) {
            return KeyAction::OpenPicker;
        }
        if self.close.matches_prefix_key(key) {
            return KeyAction::CloseTile;
        }
        if self.next.matches_prefix_key(key) {
            return KeyAction::Cycle(1);
        }
        if self.previous.matches_prefix_key(key) {
            return KeyAction::Cycle(-1);
        }
        let direction = match key.code {
            KeyCode::Left => Some(Direction::Left),
            KeyCode::Right => Some(Direction::Right),
            KeyCode::Up => Some(Direction::Up),
            KeyCode::Down => Some(Direction::Down),
            _ if self.left.matches_prefix_key(key) => Some(Direction::Left),
            _ if self.down.matches_prefix_key(key) => Some(Direction::Down),
            _ if self.up.matches_prefix_key(key) => Some(Direction::Up),
            _ if self.right.matches_prefix_key(key) => Some(Direction::Right),
            _ => None,
        };
        match direction {
            Some(direction) => KeyAction::Focus(direction),
            None => KeyAction::ForwardAfterPrefix,
        }
    }
}

/// A mouse event as an SGR report at `column`, `row` (zero-based, relative to
/// the tile's content), the form a terminal sends once a program asks for the
/// mouse. Clicks in the active tile are re-addressed to it with this; whether
/// the program there wants them is the server's to decide, as it is for
/// `herdr focus`. The wheel is not reported here: it goes as a scroll request.
pub(crate) fn sgr_mouse_report(
    kind: crossterm::event::MouseEventKind,
    modifiers: KeyModifiers,
    column: u16,
    row: u16,
) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind};

    let button = |button: MouseButton| match button {
        MouseButton::Left => 0u16,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match kind {
        MouseEventKind::Down(pressed) => (button(pressed), false),
        MouseEventKind::Up(released) => (button(released), true),
        MouseEventKind::Drag(held) => (button(held) + 32, false),
        MouseEventKind::Moved => (35, false),
        _ => return None,
    };
    let mut code = code;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let end = if release { 'm' } else { 'M' };
    Some(
        format!(
            "\x1b[<{code};{};{}{end}",
            u32::from(column) + 1,
            u32::from(row) + 1
        )
        .into_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use crossterm::event::{MouseButton, MouseEventKind};

    use super::*;

    #[test]
    fn a_click_is_reported_at_its_place_in_the_tile() {
        assert_eq!(
            sgr_mouse_report(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::empty(),
                4,
                2
            ),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        assert_eq!(
            sgr_mouse_report(
                MouseEventKind::Up(MouseButton::Right),
                KeyModifiers::CONTROL,
                0,
                0
            ),
            Some(b"\x1b[<18;1;1m".to_vec())
        );
        assert_eq!(
            sgr_mouse_report(MouseEventKind::ScrollUp, KeyModifiers::empty(), 0, 0),
            None
        );
    }

    fn keys(toml: &str) -> WallKeys {
        let config: crate::config::Config = toml::from_str(toml).expect("config parses");
        WallKeys::from_config(&config)
    }

    fn key(code: KeyCode) -> TerminalKey {
        TerminalKey::new(code, KeyModifiers::empty())
    }

    fn ctrl(ch: char) -> TerminalKey {
        TerminalKey::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    #[test]
    fn plain_keys_go_to_the_tile() {
        let keys = keys("");

        assert_eq!(
            keys.classify(&key(KeyCode::Char('a')), false),
            KeyAction::Forward
        );
        assert_eq!(keys.classify(&key(KeyCode::Up), false), KeyAction::Forward);
    }

    #[test]
    fn the_prefix_then_q_quits_as_focus_does() {
        let keys = keys("");

        assert_eq!(keys.classify(&ctrl('b'), false), KeyAction::BeginPrefix);
        assert_eq!(
            keys.classify(&key(KeyCode::Char('q')), true),
            KeyAction::Quit
        );
    }

    #[test]
    fn prefix_twice_sends_the_prefix() {
        assert_eq!(keys("").classify(&ctrl('b'), true), KeyAction::SendPrefix);
    }

    #[test]
    fn arrows_and_the_pane_focus_keys_move_between_tiles() {
        let keys = keys("");

        assert_eq!(
            keys.classify(&key(KeyCode::Left), true),
            KeyAction::Focus(Direction::Left)
        );
        assert_eq!(
            keys.classify(&key(KeyCode::Char('j')), true),
            KeyAction::Focus(Direction::Down)
        );
        assert_eq!(keys.classify(&key(KeyCode::Tab), true), KeyAction::Cycle(1));
    }

    #[test]
    fn close_pane_closes_a_tile() {
        assert_eq!(
            keys("").classify(&key(KeyCode::Char('x')), true),
            KeyAction::CloseTile
        );
    }

    #[test]
    fn the_navigator_key_opens_the_picker_while_wall_add_is_unset() {
        let keys = keys("");

        assert_eq!(
            keys.classify(&key(KeyCode::Char('g')), true),
            KeyAction::OpenPicker
        );
        assert_eq!(keys.add_label(), "prefix+g");
    }

    #[test]
    fn a_configured_wall_add_opens_the_picker_directly_and_after_the_prefix() {
        let keys = keys(
            r#"
[keys]
wall_add = ["ctrl+shift+g", "prefix+y"]
"#,
        );
        let ctrl_shift_g = TerminalKey::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )
        .with_shifted_codepoint('G' as u32);

        assert_eq!(keys.classify(&ctrl_shift_g, false), KeyAction::OpenPicker);
        assert_eq!(
            keys.classify(&key(KeyCode::Char('y')), true),
            KeyAction::OpenPicker
        );
        // Plain ctrl+g still belongs to whatever is running in the tile.
        assert_eq!(keys.classify(&ctrl('g'), false), KeyAction::Forward);
        assert_eq!(keys.add_label(), "ctrl+shift+g / prefix+y");
    }

    #[test]
    fn an_unknown_key_after_the_prefix_is_passed_on_with_it() {
        assert_eq!(
            keys("").classify(&key(KeyCode::Char('w')), true),
            KeyAction::ForwardAfterPrefix
        );
    }

    #[test]
    fn a_custom_prefix_is_honoured() {
        let keys = keys(
            r#"
[keys]
prefix = "ctrl+a"
"#,
        );

        assert_eq!(keys.classify(&ctrl('a'), false), KeyAction::BeginPrefix);
        assert_eq!(keys.classify(&ctrl('b'), false), KeyAction::Forward);
        assert_eq!(keys.prefix_label(), "ctrl+a");
    }
}
