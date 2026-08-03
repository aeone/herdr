//! Key handling for jump mode.
//!
//! The labels themselves live in [`crate::app::jump`]; this is only what to do
//! with the keys typed against them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::jump::{jump_entries, resolve, JumpKey, JumpTarget};
use crate::app::state::Mode;
use crate::app::App;

impl App {
    /// Opens jump mode with nothing typed yet.
    pub(crate) fn open_jump(&mut self) {
        self.state.jump_input.clear();
        self.state.mode = Mode::Jump;
    }

    pub(crate) fn handle_jump_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.leave_jump(),
            // Backspace takes back a keystroke rather than dropping out, so a
            // mistyped first half of a two-character label is recoverable.
            KeyCode::Backspace => {
                if self.state.jump_input.pop().is_none() {
                    self.leave_jump();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let mut input = std::mem::take(&mut self.state.jump_input);
                input.extend(c.to_lowercase());
                let entries = jump_entries(&self.state);
                match resolve(&entries, &input) {
                    JumpKey::Go(target) => {
                        self.leave_jump();
                        self.jump_to(target);
                    }
                    JumpKey::Pending => self.state.jump_input = input,
                    // Leaving on a key that matches nothing keeps a mistype from
                    // being swallowed silently: the mode closes and the next key
                    // means what it usually does.
                    JumpKey::Miss => self.leave_jump(),
                }
            }
            _ => self.leave_jump(),
        }
    }

    fn leave_jump(&mut self) {
        self.state.jump_input.clear();
        self.state.mode = Mode::Terminal;
    }

    fn jump_to(&mut self, target: JumpTarget) {
        match target {
            JumpTarget::Space { ws_idx } => {
                self.focus_workspace_idx_via_api(ws_idx);
            }
            JumpTarget::Agent {
                ws_idx,
                pane_id,
                index,
            } => {
                self.focus_pane_internal_via_api(ws_idx, pane_id);
                self.state.ensure_agent_panel_entry_visible(index);
            }
        }
    }
}
