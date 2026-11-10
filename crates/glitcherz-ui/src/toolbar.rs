//! I3: the edit-toolbar vocabulary shared by the piano roll and the timeline.
//!
//! Both views expose the same *kinds* of tool, and the distinction is the
//! point of I-D2 — mixing the two in one flat row of identical buttons is
//! what made "is Draw still on?" unanswerable:
//!
//! - **Mode** ([`mode_group`]): mutually exclusive and persistent. Exactly
//!   one of a group is on at a time, and the active one is *filled*, so "what
//!   will my next drag do" is readable at a glance. The roll's group is
//!   `[Select][Draw]`; the timeline has no persistent mode at all (I-D5 —
//!   creating a clip there is a double-click, not a mode).
//! - **Action** ([`action_button`]): one-shot. It never stays down: an armed
//!   action lasts until it is consumed (a click, a stroke), cancelled
//!   (Esc / RMB) or replaced by another arm. While armed it is *outlined* in
//!   the accent colour rather than filled — an outline reads as "loaded, not
//!   latched", which is exactly the difference to a mode.
//!
//! Every button carries its shortcut in the hover text: the shortcuts are
//! focus-gated per view, so the toolbar is the discoverable half of the same
//! vocabulary.

use eframe::egui;

/// The accent an armed one-shot tool is outlined in. Same amber the roll's
/// modal chip and the timeline's selection already use, so "armed" reads the
/// same everywhere.
pub const ARMED: egui::Color32 = egui::Color32::from_rgb(240, 190, 80);

/// Draws a group of mutually exclusive, persistent modes and returns the index
/// the user switched to (`None` when nothing was clicked).
///
/// The exclusivity rule lives here rather than in each view: the group can
/// only ever report *one* index, so no caller can leave two modes on at once.
/// Clicking the active mode is a no-op — a mode never toggles itself off into
/// "nothing", which is why this is not two `selectable_label`s.
pub fn mode_group(ui: &mut egui::Ui, current: usize, items: &[(&str, &str)]) -> Option<usize> {
    let mut picked = None;
    for (index, (label, shortcut)) in items.iter().enumerate() {
        let active = index == current;
        let response = ui
            .add(egui::Button::new(egui::RichText::new(*label).strong()).selected(active))
            .on_hover_text(format!("{label} — persistent mode ({shortcut})"));
        if response.clicked() && !active {
            picked = Some(index);
        }
    }
    picked
}

/// A one-shot action button: never latched, outlined while armed.
///
/// `hint` is the gesture summary ("the selection follows the pointer · LMB
/// confirms · Esc cancels"); the button only ever *arms* — the caller decides
/// what a click means when the tool is already armed (normally: cancel).
pub fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    shortcut: &str,
    armed: bool,
    hint: &str,
) -> egui::Response {
    let text = egui::RichText::new(label);
    let mut button = if armed {
        egui::Button::new(text.color(ARMED)).stroke(egui::Stroke::new(1.0, ARMED))
    } else {
        egui::Button::new(text)
    };
    // Stable row height: an outlined button must not resize the row the roll
    // below is measured against.
    button = button.min_size(egui::vec2(0.0, ui.spacing().interact_size.y));
    ui.add(button)
        .on_hover_text(format!("{label} — one-shot ({shortcut})\n{hint}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One click (hover → press → hold → release), the way a platform
    /// delivers it, at `x`. Returns what the group reported.
    fn click(ctx: &egui::Context, time: &mut f64, x: f32, current: usize) -> Option<usize> {
        let event = |pressed: bool| egui::Event::PointerButton {
            pos: egui::pos2(x, 16.0),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let frames = [
            vec![egui::Event::PointerMoved(egui::pos2(x, 16.0))],
            vec![egui::Event::PointerMoved(egui::pos2(x, 16.0)), event(true)],
            vec![egui::Event::PointerMoved(egui::pos2(x, 16.0))],
            vec![egui::Event::PointerMoved(egui::pos2(x, 16.0)), event(false)],
        ];
        let mut picked = None;
        for events in frames {
            *time += 0.1;
            let raw = egui::RawInput {
                events,
                time: Some(*time),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 120.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        picked = mode_group(
                            ui,
                            current,
                            &[("Select", "d toggles"), ("Draw", "d"), ("Erase", "")],
                        );
                    });
                });
            });
            // A `FullOutput` panics on drop while its texture deltas are
            // unapplied (epaint's debug assertion) — the frame's font atlas
            // arrives as a delta even headless.
            out.textures_delta.clear();
        }
        picked
    }

    /// The group reports *only* the newly clicked index: clicking the active
    /// mode is a no-op, so a mode can never toggle itself off into "nothing"
    /// (which is what two `selectable_label`s would do).
    #[test]
    fn the_mode_group_reports_only_a_newly_clicked_index() {
        let ctx = egui::Context::default();
        let mut time = 0.0;
        // Find the three buttons' centres by probing the row: the group is
        // laid out left to right, and the index it reports *is* the button
        // under the pointer, so the probe needs no layout knowledge.
        let mut seen = Vec::new();
        let mut x = 4.0;
        while x < 300.0 {
            if let Some(index) = click(&ctx, &mut time, x, 0) {
                seen.push((x, index));
            }
            x += 4.0;
        }
        assert!(
            seen.iter().any(|(_, index)| *index == 1),
            "the second mode must be reachable and report index 1: {seen:?}"
        );
        assert!(
            seen.iter().any(|(_, index)| *index == 2),
            "the third mode must be reachable too: {seen:?}"
        );
        // Clicking the mode that is already on changes nothing.
        let (x, _) = seen
            .iter()
            .find(|(_, index)| *index == 1)
            .copied()
            .expect("index 1 was reachable");
        assert_eq!(
            click(&ctx, &mut time, x, 1),
            None,
            "clicking the active mode is not a toggle"
        );
    }
}
