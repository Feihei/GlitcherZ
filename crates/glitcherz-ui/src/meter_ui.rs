//! Time-signature editor shared by the transport bar and (later) the timeline
//! meter markers.
//!
//! Met series, spec `.agents/spec/meter-map.md` §8. The document accepts any
//! valid [`TimeSignature`]; this widget offers the editable range the user
//! asked for (1–12 beats, note values 1/2…1/16) and shows imported values
//! outside it **read-only** instead of clamping them — clamping would silently
//! rewrite a MIDI import's meter.

use eframe::egui;
use glitcherz_model::TimeSignature;

/// Lowest beats-per-bar the editor accepts.
pub const UI_NUMERATOR_MIN: u8 = 1;
/// Highest beats-per-bar the editor accepts.
pub const UI_NUMERATOR_MAX: u8 = 12;
/// Note values (beat-unit denominators) the editor offers.
pub const UI_DENOMINATORS: [u8; 4] = [2, 4, 8, 16];

/// Whether `meter` can be edited with the widget's ranges.
pub fn is_ui_editable(meter: &TimeSignature) -> bool {
    (UI_NUMERATOR_MIN..=UI_NUMERATOR_MAX).contains(&meter.numerator)
        && UI_DENOMINATORS.contains(&meter.denominator)
}

/// One frame of the meter editor.
#[derive(Clone, Copy, Debug, Default)]
pub struct MeterEdit {
    /// The newly chosen value, or `None` while nothing changed.
    pub value: Option<TimeSignature>,
    /// The numerator drag just ended (callers end the merged undo gesture).
    pub drag_stopped: bool,
}

/// Draws `numerator / denominator` and reports a change.
///
/// The caller writes the document only when `value` is `Some`, which keeps the
/// "no write on an unchanged frame" rule (and the dirty flag quiet).
pub fn meter_editor(ui: &mut egui::Ui, meter: TimeSignature) -> MeterEdit {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;

        let mut numerator = i32::from(meter.numerator);
        let numerator_response = ui
            .add_enabled(
                (UI_NUMERATOR_MIN..=UI_NUMERATOR_MAX).contains(&meter.numerator),
                egui::DragValue::new(&mut numerator)
                    .range(i32::from(UI_NUMERATOR_MIN)..=i32::from(UI_NUMERATOR_MAX))
                    .speed(0.05)
                    .max_decimals(0),
            )
            .on_hover_text("Beats per bar (1-12)");

        ui.label("/");

        let mut denominator = meter.denominator;
        let combo = egui::ComboBox::from_id_salt("glitcherz_meter_den")
            .selected_text(denominator.to_string())
            .width(40.0)
            .show_ui(ui, |ui| {
                let mut changed = false;
                for value in UI_DENOMINATORS {
                    if ui
                        .selectable_value(&mut denominator, value, value.to_string())
                        .clicked()
                    {
                        changed = true;
                    }
                }
                if !UI_DENOMINATORS.contains(&meter.denominator)
                    && ui
                        .selectable_value(
                            &mut denominator,
                            meter.denominator,
                            format!("{} (imported)", meter.denominator),
                        )
                        .clicked()
                {
                    changed = true;
                }
                changed
            });
        let denominator_changed = combo.inner.unwrap_or(false);
        combo
            .response
            .on_hover_text("Note value per beat: 1/2, 1/4, 1/8 or 1/16");

        if !is_ui_editable(&meter) {
            ui.label(
                egui::RichText::new("imported")
                    .small()
                    .color(egui::Color32::from_rgb(230, 190, 70)),
            )
            .on_hover_text(
                "This meter is outside the editable range (1-12 beats, 1/2..1/16). \
                 It is kept as-is so imported files roundtrip unchanged.",
            );
        }

        let changed = numerator_response.changed() || denominator_changed;
        MeterEdit {
            value: changed.then(|| TimeSignature::new(numerator as u8, denominator)),
            drag_stopped: numerator_response.drag_stopped(),
        }
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(numerator: u8, denominator: u8) -> TimeSignature {
        TimeSignature::new(numerator, denominator)
    }

    #[test]
    fn offered_ranges_are_editable() {
        for denominator in UI_DENOMINATORS {
            assert!(is_ui_editable(&meter(UI_NUMERATOR_MIN, denominator)));
            assert!(is_ui_editable(&meter(UI_NUMERATOR_MAX, denominator)));
        }
        assert!(is_ui_editable(&meter(4, 4)));
        assert!(is_ui_editable(&meter(6, 8)));
        assert!(is_ui_editable(&meter(12, 16)));
    }

    #[test]
    fn imported_meters_stay_read_only() {
        // Legal in the document (SMF can carry them) but outside the editor:
        // the widget must display them unchanged, never clamp to 4/4.
        assert!(!is_ui_editable(&meter(13, 8)));
        assert!(!is_ui_editable(&meter(16, 4)));
        assert!(!is_ui_editable(&meter(4, 32)));
        assert!(!is_ui_editable(&meter(7, 1)));
    }
}
