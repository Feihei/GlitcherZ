//! Embedded CJK font.
//!
//! egui's `default_fonts` bundle covers Latin and Cyrillic only, so any CJK text
//! a user types (track names, clip names, imported file names) used to fall back
//! to the replacement glyph — tofu. We embed a Noto Sans SC subset and register
//! it as the **lowest-priority** fallback for both families: Latin keeps using
//! egui's default faces, so text metrics (and every layout tuned around them)
//! stay exactly as they were; the CJK face only supplies glyphs nothing else has.
//!
//! Why Noto Sans SC instead of Source Han Sans CN — same design, same metrics
//! (both come from the Adobe/Google project; the CJK "CN/SC" cuts are the same
//! glyphs). The difference is legal: the Source Han family name carries Adobe's
//! Reserved Font Name "Source", and a subset is a Modified Version under the
//! OFL, so it could not keep that name when redistributed. "Noto Sans SC"
//! contains no reserved name, so the subset ships under its own name.
//!
//! Coverage is every GBK character except two (`U+2252 ≒` and `U+2312 ⌒`, which
//! the upstream face does not have) plus the *whole* CJK Unified Ideographs base
//! block — 23,187 glyphs, 4.97 MB out of the 8.3 MB original. In practice: no
//! Chinese name can hit tofu, and the rare ideographs GBK lacks (玥, 喆, 龘 …)
//! are covered too. Greek, Cyrillic, Latin Extended-A and the emoji-ish
//! symbols come from egui's own faces, which sit earlier in the fallback chain.
//! CFF hinting is stripped because epaint rasterizes through `skrifa`, which
//! ignores CFF hints anyway. `assets/fonts/README.md` records the exact
//! regeneration command.

use eframe::egui;

/// Noto Sans SC Regular, subset. SIL Open Font License 1.1 (see
/// `assets/fonts/OFL.txt`).
const NOTO_SANS_SC: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-Regular-subset.otf");

/// Key this font is registered under in egui's font book.
const FONT_NAME: &str = "Noto Sans SC";

/// Install the embedded CJK fallback. Call once, before the first frame.
pub fn install(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

    // `Lowest` = append after the bundled faces, so a glyph is only taken from
    // here when none of them has it. Both families get it: monospace labels can
    // carry user text too (e.g. a renamed track under a numeric readout).
    let families = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
        .into_iter()
        .map(|family| InsertFontFamily {
            family,
            priority: FontPriority::Lowest,
        })
        .collect();

    ctx.add_font(FontInsert::new(
        FONT_NAME,
        egui::FontData::from_static(NOTO_SANS_SC),
        families,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context with the embedded font installed and one pass already run —
    /// fonts only exist after the first pass.
    fn ready_context() -> egui::Context {
        let ctx = egui::Context::default();
        install(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |_| {});
        // No renderer here to consume the atlas deltas; dropping them unhandled panics.
        output.textures_delta.clear();
        ctx
    }

    fn layout(ctx: &egui::Context, text: &str) -> std::sync::Arc<egui::Galley> {
        let font_id = egui::FontId::proportional(14.0);
        ctx.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_owned(), font_id, egui::Color32::WHITE))
    }

    #[test]
    fn embedded_font_renders_cjk_without_replacement_glyphs() {
        const TEXT: &str = "\u{9F13}\u{7EC4} Bass 01";
        let ctx = ready_context();
        let galley = layout(&ctx, TEXT);
        let glyphs: Vec<&egui::epaint::text::Glyph> = galley
            .rows
            .iter()
            .flat_map(|placed| placed.row.glyphs.iter())
            .collect();
        assert_eq!(glyphs.len(), TEXT.chars().count());
        for glyph in &glyphs {
            // U+FFFD is what epaint substitutes when no installed face has the
            // character — exactly the tofu this font exists to prevent.
            assert_ne!(glyph.chr, '\u{FFFD}', "missing glyph in {TEXT:?}");
            assert!(
                glyph.advance_width > 0.0,
                "{:?} has zero advance",
                glyph.chr
            );
        }
        // Rasterization really happened: the CJK glyph got an atlas slot.
        let cjk = glyphs
            .iter()
            .find(|glyph| glyph.chr == '\u{9F13}')
            .expect("CJK glyph present");
        assert!(
            cjk.uv_rect.size.x > 0.0 && cjk.uv_rect.size.y > 0.0,
            "CJK glyph was laid out but never rasterized"
        );
    }

    #[test]
    fn covers_gbk_and_ideographs_beyond_it() {
        // 龘/玥/喆 sit in the CJK base block but outside GBK; ① ㄅ ㈠ are GBK
        // decoration that the default egui faces do not carry.
        const TEXT: &str = "\u{9F98}\u{73A5}\u{5586} \u{2460}\u{3105}\u{3220}";
        let ctx = ready_context();
        let galley = layout(&ctx, TEXT);
        for glyph in galley
            .rows
            .iter()
            .flat_map(|placed| placed.row.glyphs.iter())
        {
            assert_ne!(
                glyph.chr, '\u{FFFD}',
                "missing glyph for {:?} in {TEXT:?}",
                glyph.chr
            );
        }
    }

    #[test]
    fn latin_row_height_stays_with_the_bundled_face() {
        // Noto Sans SC's face is 1.448 em tall versus Ubuntu-Light's 1.149 em.
        // Inserted as `Highest` it would inflate every Latin row by ~26%; the
        // row staying near the 14 pt default is what proves it went in last.
        let ctx = ready_context();
        let galley = layout(&ctx, "Track");
        let row_height = galley.rows.first().map_or(0.0, |placed| placed.row.size.y);
        assert!(row_height > 0.0);
        assert!(
            row_height < 14.0 * 1.3,
            "Latin row grew to {row_height} pt — font inserted above the defaults"
        );
    }
}
