//! Real-time SF2 voice rendering processor (spec
//! `.agents/spec/sf2-loader.md` S3 / D2).
//!
//! [`Sf2VoiceProcessor`] wraps a `rustysynth-ext` [`Synthesizer`] behind the
//! [`Processor`] trait so an SF2 sound source can be installed as a graph
//! node via the usual voice-swap path (`AudioEngine::set_external_voice`).
//!
//! RT safety (S1 audit, spec §7): every method that can run on the audio
//! callback — `process`, `note_event`, `set_param` — is a pure pass-through
//! to audited allocation-free synthesizer calls plus plain field writes.
//! All heap memory (synthesizer voice pool, stereo scratch) is allocated in
//! [`Sf2VoiceProcessor::new`] on the control thread.

use std::sync::Arc;

use rustysynth_ext::{SoundFont, Synthesizer, SynthesizerSettings};

use crate::realtime::FIXED_BLOCK_SIZE;
use crate::realtime::buffer::AudioBuffer;
use crate::realtime::commands::{NoteEvent, ParamId};
use crate::realtime::context::ProcessContext;
use crate::realtime::processor::Processor;
use crate::sf2::Sf2Error;

/// MIDI channel all events are played on. Melodic only: v1 passes drum
/// banks (bank 128) straight through on the same channel without the GM
/// percussion-channel special case (spec D7).
const CHANNEL: i32 = 0;

/// Voice budget of one SF2 node (the rustysynth-ext default; the S1 audit
/// exercised real fonts at this polyphony with zero allocations).
pub const SF2_POLYPHONY: usize = 64;

/// Polyphonic SoundFont voice: one [`Synthesizer`] instance plus the
/// pre-allocated stereo render scratch.
///
/// Note tracking: the synthesizer itself is fully polyphonic, but the
/// engine's transport sweep (`RtGraph::note_off_all`) releases held notes
/// with a blanket `NoteEvent::Off { note: 0 }`, which a pitch-matched
/// receiver would otherwise ignore (stuck notes). The processor therefore
/// keeps a `held` bitset (one bit per MIDI key) and interprets
/// `Off { note: 0 }` as "release everything" — but only while key 0 itself
/// is not held, so a genuine release of MIDI note 0 (C-1) stays
/// pitch-matched.
#[derive(Debug)]
pub struct Sf2VoiceProcessor {
    synth: Synthesizer,
    left: Box<[f32]>,
    right: Box<[f32]>,
    held: u128,
}

impl Sf2VoiceProcessor {
    /// Builds the voice for an already-loaded sound font.
    /// **Control thread only** (voice-pool allocation, see D3).
    ///
    /// The synthesizer's internal block size is pinned to
    /// [`FIXED_BLOCK_SIZE`] so per-block renders align with its internal
    /// block boundaries. Out-of-range sample rates are rejected as
    /// [`Sf2Error::Synth`] — the caller falls back to the built-in voice.
    pub fn new(font: &Arc<SoundFont>, sample_rate: f32) -> Result<Self, Sf2Error> {
        let mut settings = SynthesizerSettings::new(sample_rate as i32);
        settings.block_size = FIXED_BLOCK_SIZE;
        settings.maximum_polyphony = SF2_POLYPHONY;
        settings.enable_reverb_and_chorus = true;
        let mut synth = Synthesizer::new(font, &settings)?;
        // U13: the crate default is 0.5 (a hidden -6 dB that pushed every
        // node towards "too quiet"). Unity here makes `1.0` mean the font's
        // own level, and `ParamId::Sf2Gain` is the only trim on top of it.
        synth.set_master_volume(1.0);
        Ok(Self {
            synth,
            left: vec![0.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
            right: vec![0.0; FIXED_BLOCK_SIZE].into_boxed_slice(),
            held: 0,
        })
    }
}

impl Processor for Sf2VoiceProcessor {
    /// Renders one block into the node buffer. RT-safe: `render` was audited
    /// allocation-free; both scratch slices are `FIXED_BLOCK_SIZE` long, so
    /// the synthesizer's equal-length contract holds by construction (its
    /// only panic path is statically excluded). Generator semantics — the
    /// output overwrites the (pre-cleared) buffer.
    fn process(&mut self, _ctx: &ProcessContext, buffer: &mut AudioBuffer) {
        // A graph with a larger block size than the scratch would leave the
        // tail silent (clamped here instead of panicking).
        let frames = buffer.len().min(self.left.len());
        self.synth
            .render(&mut self.left[..frames], &mut self.right[..frames]);
        copy_channel(buffer, 0, &self.left[..frames]);
        copy_channel(buffer, 1, &self.right[..frames]);
    }

    /// Forwards note events on [`CHANNEL`]. RT-safe: `note_on`/`note_off`
    /// are audited allocation-free; the bitset is plain integer arithmetic.
    /// MIDI keys are 0..=127 — wider `u8` notes would overflow the bitset
    /// shift, so they are kept out of the bitmap (the synthesizer ignores
    /// them anyway: no key range matches, silent by design).
    fn note_event(&mut self, event: NoteEvent) -> bool {
        match event {
            NoteEvent::On { note, velocity } => {
                let velocity = velocity_to_midi(velocity);
                if velocity == 0 {
                    // MIDI semantics: note-on with velocity 0 is a note-off.
                    if note < 128 {
                        self.held &= !(1u128 << note);
                    }
                    self.synth.note_off(CHANNEL, note as i32);
                } else {
                    if note < 128 {
                        self.held |= 1u128 << note;
                    }
                    self.synth.note_on(CHANNEL, note as i32, velocity);
                }
            }
            NoteEvent::Off { note } => {
                if note == 0 && self.held & 1 == 0 {
                    // Blanket transport sweep with no held key 0: release
                    // every still-held note (fixed 128-iteration loop; the
                    // bitmap only ever holds keys 0..=127).
                    for key in 0..128 {
                        if self.held >> key & 1 != 0 {
                            self.synth.note_off(CHANNEL, key);
                        }
                    }
                    self.held = 0;
                } else {
                    if note < 128 {
                        self.held &= !(1u128 << note);
                    }
                    self.synth.note_off(CHANNEL, note as i32);
                }
            }
        }
        true
    }

    /// U4a: a control-track curve drives a CC. The channel argument is
    /// ignored on purpose — this node's notes all go to [`CHANNEL`], so a CC
    /// must land there too (honouring a foreign channel would silently do
    /// nothing for this synth). RT-safe: plain field writes.
    fn midi_cc(&mut self, _channel: u8, controller: u8, value: u8) -> bool {
        self.synth.process_midi_message(
            CHANNEL,
            0xB0,
            i32::from(controller & 0x7F),
            i32::from(value & 0x7F),
        );
        true
    }

    /// P0: pitch bend from a clip's controller events (spec
    /// `roll-multiclip.md` D14). Same channel policy as
    /// [`Self::midi_cc`] — this node's fixed channel wins. The wire format is
    /// 14 bits centred at 8192, LSB first.
    fn midi_pitch_bend(&mut self, _channel: u8, value: i16) -> bool {
        let raw = i32::from(value.clamp(-8192, 8191)) + 8192;
        self.synth
            .process_midi_message(CHANNEL, 0xE0, raw & 0x7F, (raw >> 7) & 0x7F);
        true
    }

    /// Applies SF2 parameters. RT-safe: `process_midi_message` and
    /// `set_master_volume` are audited allocation-free plain field writes.
    fn set_param(&mut self, id: ParamId, value: f32) -> bool {
        match id {
            // Bank select (MIDI CC #0): latched until the next program
            // change, mirroring MIDI semantics.
            ParamId::Sf2BankSelect => {
                self.synth
                    .process_midi_message(CHANNEL, 0xB0, 0x00, param_to_int(value));
            }
            ParamId::Sf2ProgramChange => {
                self.synth
                    .process_midi_message(CHANNEL, 0xC0, param_to_int(value), 0);
            }
            ParamId::Sf2Gain => {
                // Node trim on top of the font's own level (U13): 0..2 so a
                // quiet font can be pushed up. Non-finite values are dropped
                // instead of poisoning the render gain (NaN would propagate
                // into every sample).
                if value.is_finite() {
                    self.synth.set_master_volume(value.clamp(0.0, 2.0));
                }
            }
            _ => return false,
        }
        true
    }
}

/// Copies `src` into channel `ch` of `buffer`, clipped to the shorter of
/// the two lengths (leftover tail stays zero from the graph's pre-clear).
/// RT-safe: memcpy only, out-of-range channels are skipped.
fn copy_channel(buffer: &mut AudioBuffer, ch: usize, src: &[f32]) {
    if let Some(dst) = buffer.channel_mut(ch) {
        let n = dst.len().min(src.len());
        dst[..n].copy_from_slice(&src[..n]);
    }
}

/// Maps the engine's 0..1 note velocity onto MIDI 0..127.
/// NaN degrades to 0 (= note-off); out-of-range values saturate.
fn velocity_to_midi(velocity: f32) -> i32 {
    (velocity * 127.0).round().clamp(0.0, 127.0) as i32
}

/// Reads a literal integer (bank / program number) from a parameter value.
/// NaN degrades to 0; unknown addresses fall back gracefully inside the
/// synthesizer (GM set, then the default preset).
fn param_to_int(value: f32) -> i32 {
    value.round() as i32
}

#[cfg(test)]
mod tests {
    use super::{param_to_int, velocity_to_midi};

    #[test]
    fn velocity_maps_onto_midi_range() {
        assert_eq!(velocity_to_midi(0.0), 0);
        assert_eq!(velocity_to_midi(1.0), 127);
        assert_eq!(velocity_to_midi(0.5), 64);
        assert_eq!(velocity_to_midi(2.0), 127, "over-range saturates");
        assert_eq!(velocity_to_midi(-1.0), 0, "under-range saturates");
        assert_eq!(velocity_to_midi(f32::NAN), 0, "NaN degrades to note-off");
        assert_eq!(velocity_to_midi(f32::INFINITY), 127);
    }

    #[test]
    fn param_ints_round_and_degrade() {
        assert_eq!(param_to_int(0.0), 0);
        assert_eq!(param_to_int(1.0), 1);
        assert_eq!(param_to_int(1.4), 1);
        assert_eq!(param_to_int(1.5), 2);
        assert_eq!(param_to_int(255.0), 255);
        assert_eq!(param_to_int(f32::NAN), 0, "NaN degrades to 0");
    }
}
