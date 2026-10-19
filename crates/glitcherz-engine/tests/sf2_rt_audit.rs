//! S1 RT-safety audit spike for the `rustysynth-ext` SF2 synthesizer
//! (spec `.agents/spec/sf2-loader.md` S1).
//!
//! What is audited: `Synthesizer::note_on` / `note_off` / `render` and the
//! MIDI control surface (`process_midi_message`, `note_off_all`, `reset`)
//! must perform **zero heap allocations** so they can later run inside the
//! audio callback (AGENTS.md real-time iron rules). Construction-time
//! allocations (`SoundFont::new`, `Synthesizer::new`) are control-thread
//! work by design and are intentionally out of scope.
//!
//! How: a counting global allocator wraps every allocation of this test
//! binary; an [`AuditScope`] counts allocations between `begin()` and `end()`.
//! The fixture is a hand-built minimal SF2 byte stream (one looping sine
//! preset, bank 0 / program 0), so the audit runs in plain `cargo test`
//! with no external files. A second, `#[ignore]`d audit replays the same
//! scenarios against a real SoundFont when `GLITCHERZ_SF2_PATH` is set
//! (e.g. TimGM6mb.sf2), covering paths only real fonts exercise
//! (multi-region presets, key-range splits, preset fallback).
//!
//! Tests within this binary are serialized through [`AUDIT_LOCK`]: the
//! counter is global, so concurrent test threads would pollute each other.
//! Run via `cargo test -p glitcherz-engine --test sf2_rt_audit`.
//!
//! SAFETY: `unsafe impl GlobalAlloc` is required for the counting allocator.
//! It is a thin pass-through to `std::alloc::System` with two atomic ops and
//! performs no allocation itself, so it cannot recurse or deadlock. It is
//! linked only into this test binary; the engine crate itself stays
//! `#![forbid(unsafe_code)]`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rustysynth_ext::{SoundFont, Synthesizer, SynthesizerSettings};

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

static COUNT: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

// SAFETY: see the module docs 鈥?stateless pass-through around `System`.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded to the system allocator with a valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was returned by `System.alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded to the system allocator with a valid layout.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded to the system allocator with a valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Serializes the audit tests within this binary (the counter is global).
static AUDIT_LOCK: Mutex<()> = Mutex::new(());

/// Counts heap allocations until `end()` (or drop).
struct AuditScope;

impl AuditScope {
    fn begin() -> Self {
        COUNT.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::Relaxed);
        AuditScope
    }

    /// Stops counting and returns how many allocations were observed.
    fn end(&self) -> u64 {
        COUNTING.store(false, Ordering::Relaxed);
        COUNT.load(Ordering::Relaxed)
    }
}

impl Drop for AuditScope {
    fn drop(&mut self) {
        COUNTING.store(false, Ordering::Relaxed);
    }
}

fn assert_zero_alloc(scope: &AuditScope, what: &str) {
    let n = scope.end();
    assert!(n == 0, "{what} performed {n} heap allocations (expected 0)");
}

// ---------------------------------------------------------------------------
// Shared fixture (tests/common): hand-built minimal SF2 byte stream
// ---------------------------------------------------------------------------

mod common;

use common::minimal_sf2_bytes;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const SAMPLE_RATE: i32 = 48_000;
const BLOCK: usize = 128;
/// Small on purpose: 8 is the minimum polyphony, so tests can force voice
/// stealing without spraying hundreds of notes.
const POLYPHONY: usize = 8;

fn make_synth(bytes: &[u8], polyphony: usize) -> Synthesizer {
    let mut cursor = Cursor::new(bytes);
    let font = SoundFont::new(&mut cursor).expect("fixture sf2 must parse");
    let font = Arc::new(font);
    let mut settings = SynthesizerSettings::new(SAMPLE_RATE);
    settings.block_size = BLOCK;
    settings.maximum_polyphony = polyphony;
    settings.enable_reverb_and_chorus = true;
    Synthesizer::new(&font, &settings).expect("synthesizer must build")
}

fn stereo_buffers(frames: usize) -> (Vec<f32>, Vec<f32>) {
    (vec![0.0; frames], vec![0.0; frames])
}

fn peak(left: &[f32], right: &[f32]) -> f32 {
    left.iter().chain(right).fold(0.0f32, |m, s| m.max(s.abs()))
}

// ---------------------------------------------------------------------------
// Fixture sanity (not allocation audits themselves)
// ---------------------------------------------------------------------------

#[test]
fn minimal_fixture_parses_and_sounds() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut cursor = Cursor::new(&bytes[..]);
    let font = SoundFont::new(&mut cursor).expect("minimal sf2 must parse");
    assert_eq!(font.get_presets().len(), 1);
    assert_eq!(font.get_presets()[0].get_name(), "SinePreset");
    assert_eq!(font.get_presets()[0].get_bank_number(), 0);
    assert_eq!(font.get_presets()[0].get_patch_number(), 0);

    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);
    synth.note_on(0, 60, 100);
    synth.render(&mut l, &mut r);
    assert!(
        peak(&l, &r) > 0.01,
        "fixture must be audible (peak {})",
        peak(&l, &r)
    );
}

// ---------------------------------------------------------------------------
// RT-safety audits (the actual S1 deliverable)
// ---------------------------------------------------------------------------

#[test]
fn render_silence_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let scope = AuditScope::begin();
    synth.render(&mut l, &mut r);
    assert_zero_alloc(&scope, "silent render");
}

#[test]
fn single_note_lifecycle_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let scope = AuditScope::begin();

    synth.note_on(0, 60, 100);
    for _ in 0..8 {
        synth.render(&mut l, &mut r);
    }
    let peak_sounding = peak(&l, &r);
    synth.note_off(0, 60);
    // Enough blocks for the ~0.125 s release to finish and the voice to be
    // recycled (which exercises the collection's swap path). The tail is
    // dominated by reverb, so only require a strong fade, not digital zero.
    for _ in 0..128 {
        synth.render(&mut l, &mut r);
    }

    assert_zero_alloc(&scope, "single note lifecycle");
    assert!(
        peak(&l, &r) < peak_sounding * 0.05,
        "voice should fade out after release (peak {} vs {})",
        peak(&l, &r),
        peak_sounding
    );
}

#[test]
fn chord_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let scope = AuditScope::begin();

    // A 10-note chord against 8 voices: allocation-free, includes stealing.
    for key in [48, 52, 55, 60, 64, 67, 72, 76, 79, 84] {
        synth.note_on(0, key, 100);
    }
    for _ in 0..8 {
        synth.render(&mut l, &mut r);
    }
    synth.note_off_all(false);
    for _ in 0..8 {
        synth.render(&mut l, &mut r);
    }

    assert_zero_alloc(&scope, "chord + note_off_all(release)");
}

#[test]
fn voice_stealing_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let scope = AuditScope::begin();

    // 20 distinct keys into 8 voices: every note beyond the eighth must
    // recycle an existing voice (the priority-search path).
    for key in 36..56 {
        synth.note_on(0, key, 100);
    }
    for _ in 0..8 {
        synth.render(&mut l, &mut r);
    }

    assert_zero_alloc(&scope, "voice stealing");
}

#[test]
fn exclusive_class_reuse_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let scope = AuditScope::begin();

    synth.note_on(0, 60, 100);
    synth.render(&mut l, &mut r);
    // Same exclusive class on the fixture's single region: the second
    // note_on must reuse the active voice, not allocate a new one.
    synth.note_on(0, 67, 100);
    for _ in 0..4 {
        synth.render(&mut l, &mut r);
    }

    assert_zero_alloc(&scope, "exclusive class reuse");
}

#[test]
fn midi_control_surface_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let scope = AuditScope::begin();

    synth.process_midi_message(0, 0xB0, 0x00, 0); // bank select
    synth.process_midi_message(0, 0xC0, 0, 0); // program change
    synth.process_midi_message(0, 0x07, 100, 0); // channel volume
    synth.process_midi_message(0, 0x0A, 20, 0); // pan
    synth.process_midi_message(0, 0x0B, 127, 0); // expression
    synth.process_midi_message(0, 0x01, 40, 0); // modulation
    synth.process_midi_message(0, 0x40, 127, 0); // hold pedal
    synth.process_midi_message(0, 0xE0, 0, 64); // pitch bend
    synth.note_on(0, 60, 100);
    synth.render(&mut l, &mut r);
    synth.process_midi_message(0, 0x40, 0, 0); // hold pedal off
    synth.note_off(0, 60);
    synth.render(&mut l, &mut r);
    synth.reset_all_controllers();
    synth.render(&mut l, &mut r);

    assert_zero_alloc(&scope, "midi control surface");
}

#[test]
fn reset_is_allocation_free() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = minimal_sf2_bytes();
    let mut synth = make_synth(&bytes, POLYPHONY);
    let (mut l, mut r) = stereo_buffers(BLOCK);

    synth.note_on(0, 60, 100);
    synth.render(&mut l, &mut r);

    let scope = AuditScope::begin();
    synth.reset();
    synth.render(&mut l, &mut r);
    synth.note_off_all(true);
    synth.render(&mut l, &mut r);
    assert_zero_alloc(&scope, "reset + note_off_all(immediate)");
}

// ---------------------------------------------------------------------------
// Real-SoundFont audit (opt-in: needs a local .sf2 file)
// ---------------------------------------------------------------------------

/// Replays the audit against a real SoundFont (recommended: TimGM6mb.sf2).
/// Enable with the `GLITCHERZ_SF2_PATH` environment variable, e.g.
/// `GLITCHERZ_SF2_PATH=C:\sf2\TimGM6mb.sf2 cargo test -p glitcherz-engine --test sf2_rt_audit -- --ignored`
/// (fixture files are local data and intentionally not committed).
#[test]
#[ignore = "set GLITCHERZ_SF2_PATH to a real .sf2 file to run this audit"]
fn real_soundfont_audit() {
    let _guard = AUDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Ok(path) = std::env::var("GLITCHERZ_SF2_PATH") else {
        eprintln!("GLITCHERZ_SF2_PATH not set; skipping real-soundfont audit");
        return;
    };

    let file = File::open(&path).expect("GLITCHERZ_SF2_PATH must be readable");
    let mut reader = std::io::BufReader::new(file);
    let font = Arc::new(SoundFont::new(&mut reader).expect("sound font must parse"));

    let mut settings = SynthesizerSettings::new(SAMPLE_RATE);
    settings.block_size = BLOCK;
    settings.maximum_polyphony = 64;
    settings.enable_reverb_and_chorus = true;
    let mut synth = Synthesizer::new(&font, &settings).expect("synthesizer must build");
    let (mut l, mut r) = stereo_buffers(BLOCK);

    let presets = font.get_presets().len();
    let mut audible_presets = 0usize;

    for (index, preset) in font.get_presets().iter().enumerate() {
        // Built before the scope opens: formatting inside it would count
        // as an allocation of the audit itself.
        let what = format!("real soundfont preset {index} ({})", preset.get_name());

        let scope = AuditScope::begin();

        synth.process_midi_message(0, 0xB0, 0x00, preset.get_bank_number());
        synth.process_midi_message(0, 0xC0, preset.get_patch_number(), 0);
        for key in [48, 60, 64, 67, 72] {
            synth.note_on(0, key, 100);
        }
        for _ in 0..8 {
            synth.render(&mut l, &mut r);
        }
        if peak(&l, &r) > 0.001 {
            audible_presets += 1;
        }
        synth.note_off_all(false);
        for _ in 0..8 {
            synth.render(&mut l, &mut r);
        }

        assert_zero_alloc(&scope, &what);
    }

    assert!(presets > 0, "sound font must contain presets");
    assert!(
        audible_presets > 0,
        "at least one preset of the real sound font must be audible"
    );
}
