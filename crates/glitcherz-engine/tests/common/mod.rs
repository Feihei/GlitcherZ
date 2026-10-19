//! Shared test fixture: a hand-built minimal but valid SF2 byte stream.
//!
//! One instrument, one preset (bank/program configurable), playing a looping
//! sine sample (480 samples, five full periods at 48 kHz) with exclusive
//! class 1 and a short release (~0.125 s). No external files needed, so
//! every integration test in this crate can run in plain `cargo test`.

// Generator ids as numbered by rustysynth-ext's `GeneratorType` table.
const GEN_RELEASE_VOLUME_ENVELOPE: u16 = 38;
const GEN_INSTRUMENT: u16 = 41;
// Only used by `two_preset_sf2_bytes`, which not every test binary pulls
// in — the shared `mod common` is compiled per binary.
#[allow(dead_code)]
const GEN_KEY_RANGE: u16 = 43;
const GEN_SAMPLE_ID: u16 = 53;
const GEN_SAMPLE_MODES: u16 = 54;
const GEN_EXCLUSIVE_CLASS: u16 = 57;

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Writes a fixed 20-byte NUL-padded SF2 name field.
fn put_name(out: &mut Vec<u8>, name: &str) {
    let mut field = [0u8; 20];
    let n = name.len().min(19);
    field[..n].copy_from_slice(&name.as_bytes()[..n]);
    out.extend_from_slice(&field);
}

/// Writes `id` + little-endian size + body (a RIFF sub-chunk).
fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

/// Writes `LIST` + size + kind + body.
fn list(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(b"LIST");
    let size = (body.len() + 4) as u32;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
}

/// Emits the shared instrument/sample sections (`inst`, `ibag`, `imod`,
/// `igen`, `shdr`): one instrument ("Sine"), one zone, one looping sine
/// sample with exclusive class 1 and a ~0.125 s release. Used by every
/// fixture variant; only the preset-side tables differ.
fn put_instrument_sections(pdta: &mut Vec<u8>) {
    // inst: instrument record + EOI terminator (22 bytes each).
    let mut inst = Vec::new();
    put_name(&mut inst, "Sine");
    put_u16(&mut inst, 0); // first ibag index
    put_name(&mut inst, "EOI");
    put_u16(&mut inst, 1); // terminator bag index
    chunk(pdta, b"inst", &inst);

    // ibag: instrument zone + terminator (4 bytes each). The terminator
    // generator index must point past all four igen records below.
    let mut ibag = Vec::new();
    put_u16(&mut ibag, 0); // first igen index
    put_u16(&mut ibag, 0); // first imod index
    put_u16(&mut ibag, 4); // terminator generator index
    put_u16(&mut ibag, 0);
    chunk(pdta, b"ibag", &ibag);

    // imod: terminator only (10 bytes, discarded by the parser).
    chunk(pdta, b"imod", &[0u8; 10]);

    // igen: sample modes / release / exclusive class / sample id +
    // terminator (4 bytes each). `sampleID` must be the zone's last
    // generator, otherwise the zone would be treated as a global zone.
    let mut igen = Vec::new();
    put_u16(&mut igen, GEN_SAMPLE_MODES);
    put_u16(&mut igen, 1); // loop continuous
    put_u16(&mut igen, GEN_RELEASE_VOLUME_ENVELOPE);
    put_u16(&mut igen, 0xF200); // -3600: ~0.125 s release
    put_u16(&mut igen, GEN_EXCLUSIVE_CLASS);
    put_u16(&mut igen, 1);
    put_u16(&mut igen, GEN_SAMPLE_ID);
    put_u16(&mut igen, 0);
    put_u16(&mut igen, 0);
    put_u16(&mut igen, 0);
    chunk(pdta, b"igen", &igen);

    // shdr: sample record + EOS terminator (46 bytes each).
    let mut shdr = Vec::new();
    put_name(&mut shdr, "SineSample");
    put_i32(&mut shdr, 0); // start
    put_i32(&mut shdr, 479); // end (exclusive of the 480-sample wave)
    put_i32(&mut shdr, 100); // loop start
    put_i32(&mut shdr, 400); // loop end
    put_i32(&mut shdr, 48_000); // sample rate
    shdr.push(60); // original pitch (C4)
    shdr.push(0); // pitch correction
    put_u16(&mut shdr, 0); // link
    put_u16(&mut shdr, 1); // sample type: mono
    put_name(&mut shdr, "EOS");
    put_i32(&mut shdr, 0);
    put_i32(&mut shdr, 0);
    put_i32(&mut shdr, 0);
    put_i32(&mut shdr, 0);
    put_i32(&mut shdr, 0);
    shdr.push(0);
    shdr.push(0);
    put_u16(&mut shdr, 0);
    put_u16(&mut shdr, 0);
    chunk(pdta, b"shdr", &shdr);
}

/// The minimal SF2 bytes with the single preset placed at `bank:program`.
pub fn minimal_sf2_bytes_at(bank: u16, program: u16) -> Vec<u8> {
    let wave: Vec<i16> = (0..480u32)
        .map(|i| {
            let phase = i as f32 * 5.0 * std::f32::consts::TAU / 480.0;
            (phase.sin() * 20_000.0) as i16
        })
        .collect();

    let mut riff = Vec::new();

    // INFO: just the format version (2.1).
    let mut info = Vec::new();
    chunk(&mut info, b"ifil", &[2, 0, 1, 0]);
    list(&mut riff, b"INFO", &info);

    // sdta: raw little-endian 16-bit PCM samples.
    let mut sdta = Vec::new();
    let mut smpl_body = Vec::new();
    for s in &wave {
        smpl_body.extend_from_slice(&s.to_le_bytes());
    }
    chunk(&mut sdta, b"smpl", &smpl_body);
    list(&mut riff, b"sdta", &sdta);

    // pdta: one preset zone -> instrument 0; one instrument zone ->
    // sample 0 (looping, exclusive, short release).
    let mut pdta = Vec::new();

    // phdr: preset record + EOP terminator (38 bytes each).
    let mut phdr = Vec::new();
    put_name(&mut phdr, "SinePreset");
    put_u16(&mut phdr, program); // patch number
    put_u16(&mut phdr, bank); // bank number
    put_u16(&mut phdr, 0); // first pbag index
    put_i32(&mut phdr, 0); // library
    put_i32(&mut phdr, 0); // genre
    put_i32(&mut phdr, 0); // morphology
    put_name(&mut phdr, "EOP");
    put_u16(&mut phdr, 0);
    put_u16(&mut phdr, 0);
    put_u16(&mut phdr, 1); // terminator bag index
    put_i32(&mut phdr, 0);
    put_i32(&mut phdr, 0);
    put_i32(&mut phdr, 0);
    chunk(&mut pdta, b"phdr", &phdr);

    // pbag: preset zone + terminator (4 bytes each).
    let mut pbag = Vec::new();
    put_u16(&mut pbag, 0); // first pgen index
    put_u16(&mut pbag, 0); // first pmod index
    put_u16(&mut pbag, 1); // terminator generator index
    put_u16(&mut pbag, 0);
    chunk(&mut pdta, b"pbag", &pbag);

    // pmod: terminator only (10 bytes, discarded by the parser).
    chunk(&mut pdta, b"pmod", &[0u8; 10]);

    // pgen: instrument generator + terminator (4 bytes each).
    let mut pgen = Vec::new();
    put_u16(&mut pgen, GEN_INSTRUMENT);
    put_u16(&mut pgen, 0);
    put_u16(&mut pgen, 0);
    put_u16(&mut pgen, 0);
    chunk(&mut pdta, b"pgen", &pgen);

    put_instrument_sections(&mut pdta);

    list(&mut riff, b"pdta", &pdta);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    let size = (riff.len() + 4) as u32;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(b"sfbk");
    out.extend_from_slice(&riff);
    out
}

/// The minimal SF2 bytes with the single preset at bank 0 / program 0.
pub fn minimal_sf2_bytes() -> Vec<u8> {
    minimal_sf2_bytes_at(0, 0)
}

/// Two-preset fixture for bank/program addressing tests.
///
/// - Preset `0:0` ("SinePreset"): full key range (the same voice as
///   [`minimal_sf2_bytes`]).
/// - Preset `0:1` ("LowKeys"): same instrument, but a preset-zone
///   `keyRange` of 20..=40 — notes outside that range stay silent.
///
/// Both presets share the single instrument, so the audible difference is
/// purely the preset-level key range. Chunk layout mirrors
/// [`minimal_sf2_bytes_at`]; only `phdr`/`pbag`/`pgen` grow.
// Not every test binary including this module uses this fixture.
#[allow(dead_code)]
pub fn two_preset_sf2_bytes() -> Vec<u8> {
    let wave: Vec<i16> = (0..480u32)
        .map(|i| {
            let phase = i as f32 * 5.0 * std::f32::consts::TAU / 480.0;
            (phase.sin() * 20_000.0) as i16
        })
        .collect();

    let mut riff = Vec::new();

    // INFO: just the format version (2.1).
    let mut info = Vec::new();
    chunk(&mut info, b"ifil", &[2, 0, 1, 0]);
    list(&mut riff, b"INFO", &info);

    // sdta: raw little-endian 16-bit PCM samples.
    let mut sdta = Vec::new();
    let mut smpl_body = Vec::new();
    for s in &wave {
        smpl_body.extend_from_slice(&s.to_le_bytes());
    }
    chunk(&mut sdta, b"smpl", &smpl_body);
    list(&mut riff, b"sdta", &sdta);

    let mut pdta = Vec::new();

    // phdr: two preset records + EOP terminator. Each preset owns exactly
    // one bag; the EOP's bag index terminates preset B's span.
    let mut phdr = Vec::new();
    put_name(&mut phdr, "SinePreset");
    put_u16(&mut phdr, 0); // patch number
    put_u16(&mut phdr, 0); // bank number
    put_u16(&mut phdr, 0); // first pbag index
    put_i32(&mut phdr, 0); // library
    put_i32(&mut phdr, 0); // genre
    put_i32(&mut phdr, 0); // morphology
    put_name(&mut phdr, "LowKeys");
    put_u16(&mut phdr, 1); // patch number
    put_u16(&mut phdr, 0); // bank number
    put_u16(&mut phdr, 1); // first pbag index
    put_i32(&mut phdr, 0);
    put_i32(&mut phdr, 0);
    put_i32(&mut phdr, 0);
    put_name(&mut phdr, "EOP");
    put_u16(&mut phdr, 0);
    put_u16(&mut phdr, 0);
    put_u16(&mut phdr, 2); // terminator bag index
    put_i32(&mut phdr, 0);
    put_i32(&mut phdr, 0);
    put_i32(&mut phdr, 0);
    chunk(&mut pdta, b"phdr", &phdr);

    // pbag: preset A's zone (pgen 0..1), preset B's zone (pgen 1..3) and
    // the terminator (pgen 3..3, dropped by the parser).
    let mut pbag = Vec::new();
    put_u16(&mut pbag, 0); // A: first pgen index
    put_u16(&mut pbag, 0);
    put_u16(&mut pbag, 1); // B: first pgen index
    put_u16(&mut pbag, 0);
    put_u16(&mut pbag, 3); // terminator generator index
    put_u16(&mut pbag, 0);
    chunk(&mut pdta, b"pbag", &pbag);

    // pmod: terminator only (10 bytes, discarded by the parser).
    chunk(&mut pdta, b"pmod", &[0u8; 10]);

    // pgen: A = [instrument 0]; B = [keyRange 20..=40, instrument 0] (the
    // instrument generator must terminate a preset zone).
    let mut pgen = Vec::new();
    put_u16(&mut pgen, GEN_INSTRUMENT);
    put_u16(&mut pgen, 0);
    put_u16(&mut pgen, GEN_KEY_RANGE);
    put_u16(&mut pgen, 20 | 40 << 8); // lo byte = start, hi byte = end
    put_u16(&mut pgen, GEN_INSTRUMENT);
    put_u16(&mut pgen, 0);
    put_u16(&mut pgen, 0); // terminator record
    put_u16(&mut pgen, 0);
    chunk(&mut pdta, b"pgen", &pgen);

    put_instrument_sections(&mut pdta);

    list(&mut riff, b"pdta", &pdta);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    let size = (riff.len() + 4) as u32;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(b"sfbk");
    out.extend_from_slice(&riff);
    out
}
