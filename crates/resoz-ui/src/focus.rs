//! Hover-follow keyboard focus (Blender-style context).
//!
//! The keyboard context follows the pointer: hovering a registered keyboard
//! surface (piano roll, timeline lanes) hands it focus; hovering a neutral
//! chrome zone (transport, status bar, view tabs) releases it; hovering
//! nothing registered keeps the last owner (sticky). Two situations freeze
//! the whole mechanism, so it can never fight a gesture or an in-progress
//! text edit:
//!
//! 1. **Pointer button down** — a drag belongs to whichever surface started
//!    it; reassigning focus mid-gesture would drop arrow-key nudging and
//!    other focus-gated keys under the user's hands.
//! 2. **A text-editing widget holds focus** (egui reports this via
//!    [`egui::Context::text_edit_focused`]) — e.g. a `DragValue` inline
//!    editor. Stealing focus from it would abort the edit mid-typing.
//!
//! Focus held by other widgets (buttons clicked a moment ago, `DragValue`s
//! in button mode) is NOT protected: the pointer marks where the user's
//! attention is, so hovering a keyboard surface steals from them freely.
//!
//! The router resolves at the START of a frame using the rects registered
//! during the PREVIOUS frame (layout is stable frame-to-frame — the same
//! assumption egui itself makes), so `has_focus()` checks in the widget pass
//! see the hover decision with zero lag.

use eframe::egui;

/// Hover-follow focus arbiter. Owned by the app; keyboard surfaces register
/// every frame, and [`FocusRouter::begin_frame`] applies the decision.
pub struct FocusRouter {
    /// Keyboard surfaces as `(focus anchor id, hit rect)`, registered during
    /// the widget pass. Later registrations win on overlap (drawn on top).
    keyboard: Vec<(egui::Id, egui::Rect)>,
    /// Chrome rects: hovering here releases any registered surface's focus.
    neutral: Vec<egui::Rect>,
    /// Last pointer position seen inside the window. egui clears its own
    /// pointer position on `CursorLeft` and on touch end (`Event::PointerGone`
    /// → `latest_pos() == None`), and remote-desktop sessions produce those
    /// constantly: many clients forward mouse input via touch/pointer
    /// injection (the pointer "vanishes" after every gesture) or park the
    /// host cursor when the user stops moving. Without retention the router
    /// would be positionless between gestures and hover-follow would never
    /// fire — the last position seen inside the window is the best available
    /// proxy for where the user's attention is, and matches the designed
    /// "outside the window → sticky" semantics anyway.
    last_pos: Option<egui::Pos2>,
}

impl Default for FocusRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusRouter {
    pub fn new() -> Self {
        Self {
            keyboard: Vec::new(),
            neutral: Vec::new(),
            last_pos: None,
        }
    }

    /// Registers a keyboard surface for this frame. `id` is the widget whose
    /// `has_focus()` the surface's key handling checks; `rect` is its hover
    /// hit area.
    pub fn register_keyboard(&mut self, id: egui::Id, rect: egui::Rect) {
        self.keyboard.push((id, rect));
    }

    /// Registers a chrome area this frame: hovering it surrenders the focus
    /// of whatever registered surface holds it (never the focus of widgets
    /// inside the area itself — those are foreign ids and left alone).
    pub fn register_neutral(&mut self, rect: egui::Rect) {
        self.neutral.push(rect);
    }

    /// `(keyboard surfaces, neutral rects)` registered so far this frame —
    /// diagnostics only (`RESOZ_FOCUS_DEBUG=1` status-bar readout).
    pub fn registry_size(&self) -> (usize, usize) {
        (self.keyboard.len(), self.neutral.len())
    }

    /// Retained last-known pointer position (diagnostics: shows whether the
    /// session delivers live pointer positions or only gesture bursts).
    pub fn last_known_pos(&self) -> Option<egui::Pos2> {
        self.last_pos
    }

    /// Resolves hover-follow focus for this frame, using the registrations
    /// collected during the previous frame. Call once per frame, before the
    /// panels are laid out and before anything reads `has_focus()`.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        let focused = ctx.memory(|m| m.focused());
        let registered = |id: egui::Id| self.keyboard.iter().any(|(k, _)| *k == id);

        // Freeze conditions. A foreign id (not one of ours) that is an
        // active text editor must never be stolen from; everything else
        // (buttons, foreign ids without text state) is fair game.
        let protected = focused.is_some_and(|id| !registered(id)) && ctx.text_edit_focused();
        let any_down = ctx.input(|i| i.pointer.any_down());
        let pointer = ctx.input(|i| i.pointer.latest_pos());
        if pointer.is_some() {
            self.last_pos = pointer;
        }
        // Fall back to the retained position when egui reports the pointer
        // as gone (remote/touch sessions — see `last_pos` docs).
        let pointer = pointer.or(self.last_pos);

        if !protected && !any_down {
            if let Some(pos) = pointer {
                // Last match wins: regions registered later are drawn on top.
                let desired = self
                    .keyboard
                    .iter()
                    .rev()
                    .find(|(_, rect)| rect.contains(pos))
                    .map(|(id, _)| *id);
                if let Some(id) = desired {
                    // `request_focus` silently replaces any non-protected
                    // current owner (registry member, None, or a mere button).
                    if focused != Some(id) {
                        ctx.memory_mut(|m| m.request_focus(id));
                    }
                } else if self.neutral.iter().any(|rect| rect.contains(pos)) {
                    // Chrome hover releases only OUR surfaces' focus; foreign
                    // widgets (the button under the pointer itself) keep it.
                    if let Some(id) = focused {
                        if registered(id) {
                            ctx.memory_mut(|m| m.surrender_focus(id));
                        }
                    }
                }
                // Pointer over nothing registered → sticky: the last owner
                // keeps the keyboard (gaps, sub-panels, dead space).
            }
        }

        self.keyboard.clear();
        self.neutral.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The freeze logic is exercised through the egui frame tests in
    // `tests/timeline_focus.rs` and `tests/roll_state_machine.rs`; these
    // unit checks pin the pure decision table.

    #[test]
    fn registration_clears_each_frame() {
        let ctx = egui::Context::default();
        let mut router = FocusRouter::new();
        let mut ran = false;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            router.register_keyboard(
                egui::Id::new("a"),
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0)),
            );
            router.register_neutral(egui::Rect::from_min_size(
                egui::pos2(20.0, 0.0),
                egui::vec2(10.0, 10.0),
            ));
            router.begin_frame(ui.ctx());
            // Cleared after the pass.
            assert!(router.keyboard.is_empty());
            assert!(router.neutral.is_empty());
            ran = true;
        });
        out.textures_delta.clear();
        assert!(ran);
    }
}
