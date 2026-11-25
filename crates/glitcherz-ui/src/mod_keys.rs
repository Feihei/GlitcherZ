//! Command-modifier ("mod1") helpers (F-050, spec `.agents/spec/platform-keys.md`).
//!
//! macOS reports `Cmd` and physical `Control` as two different modifiers,
//! while Windows / Linux report both as `Ctrl`. Any shortcut written with
//! [`egui::Modifiers::CTRL`] or a bare `modifiers.ctrl` read therefore dies
//! on `Cmd` on macOS. This module is the **single** place that decision
//! lives (spec D3); business code asks it instead of spelling the modifier
//! out itself.
//!
//! `mod1` = the platform command modifier: `⌘` on macOS, `Ctrl` on
//! Windows / Linux. Physical `Control` keeps working on every platform
//! (spec D1), so both spellings are tried.

use eframe::egui;

/// Whether a modifier snapshot holds the command modifier: `Cmd` on macOS,
/// `Ctrl` on Windows / Linux, plus physical `Control` anywhere (spec D1).
pub(crate) fn mod1(modifiers: egui::Modifiers) -> bool {
    modifiers.ctrl || modifiers.command
}

/// [`mod1`] for the live input state (held-key reads).
pub(crate) fn mod1_held(i: &egui::InputState) -> bool {
    mod1(i.modifiers)
}

/// Consumes a command-modifier press of `key` (spec D1). No `Shift` / `Alt`
/// is required by the pattern, so call the `Shift` variant first when the
/// same key also has a shifted shortcut — `consume_key` matches modifiers
/// logically and would otherwise swallow it.
pub(crate) fn consume_mod1(i: &mut egui::InputState, key: egui::Key) -> bool {
    consume_mod1_with(i, egui::Modifiers::NONE, key)
}

/// Consumes a `Shift` + command-modifier press of `key` (spec D1). Always
/// call this before [`consume_mod1`] for the same key.
pub(crate) fn consume_mod1_shift(i: &mut egui::InputState, key: egui::Key) -> bool {
    consume_mod1_with(i, egui::Modifiers::SHIFT, key)
}

/// Consumes `extra` + command-modifier press of `key`. Both spellings of the
/// command modifier are tried, most specific first: [`egui::Modifiers::COMMAND`]
/// (`Cmd` on macOS, `Ctrl` elsewhere) and then [`egui::Modifiers::CTRL`]
/// (the physical `Control` key on macOS).
pub(crate) fn consume_mod1_with(
    i: &mut egui::InputState,
    extra: egui::Modifiers,
    key: egui::Key,
) -> bool {
    if i.consume_key(extra.plus(egui::Modifiers::COMMAND), key) {
        return true;
    }
    i.consume_key(extra.plus(egui::Modifiers::CTRL), key)
}

/// The label for the command modifier in UI copy: `⌘` on macOS, `Ctrl`
/// elsewhere (spec D12). `Alt` / `Shift` stay literal — the `⌥` / `⌃` glyphs
/// are not in the font stack (`glossary.md`).
pub(crate) fn mod_key_label() -> &'static str {
    mod_key_label_for(std::env::consts::OS)
}

/// [`mod_key_label`] with the OS injected, so both branches are testable on
/// any host.
fn mod_key_label_for(os: &str) -> &'static str {
    if os == "macos" { "⌘" } else { "Ctrl" }
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::*;

    fn context_with(events: Vec<egui::Event>) -> egui::Context {
        let ctx = egui::Context::default();
        let out = ctx.run_ui(
            egui::RawInput {
                events,
                ..egui::RawInput::default()
            },
            |_| {},
        );
        out.drop_without_applying_deltas();
        ctx
    }

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn ctrl() -> egui::Modifiers {
        egui::Modifiers {
            ctrl: true,
            command: true,
            ..egui::Modifiers::NONE
        }
    }

    fn mac_cmd() -> egui::Modifiers {
        egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..egui::Modifiers::NONE
        }
    }

    fn mac_control() -> egui::Modifiers {
        egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::NONE
        }
    }

    #[test]
    fn all_three_command_spellings_are_accepted() {
        for (name, modifiers) in [
            ("win/linux Ctrl", ctrl()),
            ("macOS Cmd", mac_cmd()),
            ("macOS physical Control", mac_control()),
        ] {
            let ctx = context_with(vec![key_event(egui::Key::Z, modifiers)]);
            assert!(
                ctx.input_mut(|i| consume_mod1(i, egui::Key::Z)),
                "{name} must consume the shortcut"
            );
        }
    }

    #[test]
    fn shift_variant_is_matched_before_the_bare_key() {
        // Both events in one frame: the Shift variant must eat only its own.
        let mut shift_cmd = mac_cmd();
        shift_cmd.shift = true;
        let ctx = context_with(vec![
            key_event(egui::Key::A, shift_cmd),
            key_event(egui::Key::A, mac_cmd()),
        ]);
        assert!(ctx.input_mut(|i| consume_mod1_shift(i, egui::Key::A)));
        assert!(ctx.input_mut(|i| consume_mod1(i, egui::Key::A)));

        // A plain press is not a Shift press.
        let ctx = context_with(vec![key_event(egui::Key::A, ctrl())]);
        assert!(!ctx.input_mut(|i| consume_mod1_shift(i, egui::Key::A)));
        assert!(ctx.input_mut(|i| consume_mod1(i, egui::Key::A)));
    }

    #[test]
    fn unmodified_keys_are_not_command_shortcuts() {
        let ctx = context_with(vec![key_event(egui::Key::A, egui::Modifiers::NONE)]);
        assert!(!ctx.input_mut(|i| consume_mod1(i, egui::Key::A)));
    }

    #[test]
    fn mod1_held_accepts_command_and_control() {
        let ctx = context_with(vec![egui::Event::ModifiersChanged(mac_cmd())]);
        assert!(ctx.input(mod1_held));
        let ctx = context_with(vec![egui::Event::ModifiersChanged(mac_control())]);
        assert!(ctx.input(mod1_held));
        let ctx = context_with(vec![egui::Event::ModifiersChanged(egui::Modifiers::SHIFT)]);
        assert!(!ctx.input(mod1_held));
    }

    #[test]
    fn label_is_command_on_macos_and_ctrl_elsewhere() {
        assert_eq!(mod_key_label_for("macos"), "⌘");
        assert_eq!(mod_key_label_for("windows"), "Ctrl");
        assert_eq!(mod_key_label_for("linux"), "Ctrl");
    }
}
