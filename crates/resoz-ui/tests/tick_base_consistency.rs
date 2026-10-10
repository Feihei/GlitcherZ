//! The engine and the document each define the tick base (the engine must not
//! depend on `resoz-model`: the audio thread stays independent of the document
//! layer). This test locks the two definitions together — they drifted before
//! (96 PPQ in both, but three separate copies of the constants).

use resoz_engine::realtime::{MIN_VELOCITY, TICKS_PER_BAR, TICKS_PER_QUARTER, TICKS_PER_STEP};
use resoz_model::{DEFAULT_VELOCITY, Note};

#[test]
fn tick_base_matches_the_document() {
    assert_eq!(TICKS_PER_QUARTER, resoz_model::TICKS_PER_QUARTER);
    assert_eq!(TICKS_PER_STEP, resoz_model::TICKS_PER_STEP);
    assert_eq!(TICKS_PER_BAR, resoz_model::TICKS_PER_BAR);
    assert_eq!(MIN_VELOCITY, resoz_model::MIN_VELOCITY);
    assert_eq!(DEFAULT_VELOCITY, 100);
}

#[test]
fn engine_bar_length_is_derived_not_hardcoded() {
    // 4/4 at 960 PPQ.
    assert_eq!(TICKS_PER_QUARTER, 960);
    assert_eq!(TICKS_PER_STEP, 240);
    assert_eq!(TICKS_PER_BAR, 3840);
    assert_eq!(TICKS_PER_BAR, TICKS_PER_STEP * 16);
}

#[test]
fn document_note_converts_into_an_rte_note_without_loss() {
    use resoz_engine::realtime::RtNote;

    let n = Note {
        start: 240,
        length: 480,
        note: 64,
        channel: 3,
        velocity: 100,
    };
    let rt = RtNote::new(n.start, n.length, n.note, n.channel, n.velocity);
    assert_eq!(rt.start_ticks, 240);
    assert_eq!(rt.length_ticks, 480);
    assert_eq!(rt.note, 64);
    assert_eq!(rt.channel, 3);
    assert_eq!(rt.velocity, 100);
    assert!((rt.velocity_f32() - n.velocity_f32()).abs() < 1e-6);
}

#[test]
fn out_of_range_document_values_are_clamped_at_the_engine_edge() {
    use resoz_engine::realtime::RtNote;

    // A drag gesture can leave a negative start or a zero length in the
    // document; the audio thread must never see them.
    let rt = RtNote::new(-500, 0, 200, 99, 0);
    assert_eq!(rt.start_ticks, 0);
    assert_eq!(rt.length_ticks, 1);
    assert_eq!(rt.note, 127);
    assert_eq!(rt.channel, 15);
    assert_eq!(rt.velocity, MIN_VELOCITY);
}
