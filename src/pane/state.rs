use crate::terminal::TerminalId;

/// Viewport state for a pane.
///
/// Terminal identity, cwd, labels, and agent metadata live in TerminalState.
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    /// Set when this pane is a view of a terminal that lives in another pane:
    /// it draws that terminal's screen and sends it input, while
    /// `attached_terminal_id` stays a placeholder of its own.
    ///
    /// The placeholder is what keeps the rest of herdr honest. Hundreds of
    /// places assume a terminal belongs to exactly one pane -- closing a pane
    /// shuts its terminal down, an agent is reported once, a pane id resolves
    /// to one terminal -- so pointing a second pane at the same terminal would
    /// have each of them act on it twice. Only the lookups that draw, size and
    /// type follow this field instead.
    pub view_of: Option<TerminalId>,
    /// Whether the user has seen this pane since its last state change to Idle.
    /// False = "Done" (agent finished while user was in another workspace).
    pub seen: bool,
    /// Whether unmodified right-click gestures should be forwarded to the pane application.
    pub right_click_passthrough: bool,
}

impl PaneState {
    pub fn new(attached_terminal_id: TerminalId) -> Self {
        Self {
            attached_terminal_id,
            view_of: None,
            seen: true,
            right_click_passthrough: false,
        }
    }

    /// A pane that shows `target` while holding `placeholder` as its own.
    pub fn view(placeholder: TerminalId, target: TerminalId) -> Self {
        Self {
            view_of: Some(target),
            ..Self::new(placeholder)
        }
    }

    /// The terminal this pane draws and types into: the one it views, if it
    /// is a view, and its own otherwise.
    pub fn effective_terminal_id(&self) -> &TerminalId {
        self.view_of.as_ref().unwrap_or(&self.attached_terminal_id)
    }
}
