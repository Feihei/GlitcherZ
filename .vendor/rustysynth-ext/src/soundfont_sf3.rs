use std::io::Cursor;

use lewton::inside_ogg::OggStreamReader;

use crate::error::SoundFontError;
use crate::sample_header::SampleHeader;

/// Number of zero-valued guard frames appended after every decoded sample.
///
/// The SoundFont 2 specification requires at least 46 zero sample points after
/// each sample. They keep the oscillator's `data[index + 1]` interpolation
/// reads in bounds and stop one sample's tail from reading into the next.
const GUARD_FRAMES: usize = 46;

/// Decodes the concatenated Ogg Vorbis sample streams of a SoundFont3 into a
/// single shared 16-bit PCM buffer and rewrites each sample header to index it.
///
/// In a SoundFont3, `start`/`end` of a compressed sample are byte offsets into
/// the `smpl` sub-chunk delimiting that sample's Ogg Vorbis stream (`end` is
/// exclusive), while `start_loop`/`end_loop` are sample-frame offsets relative
/// to the beginning of the individual decompressed sample. After decoding, all
/// four fields are absolute indices into the returned PCM buffer, matching the
/// layout used by SoundFont2.
///
/// This must run before the instrument regions are built, since the regions
/// copy the sample offsets while they are created.
pub(crate) fn decode_vorbis_samples(
    smpl: &[u8],
    sample_headers: &mut [SampleHeader],
) -> Result<Vec<i16>, SoundFontError> {
    let mut wave_data: Vec<i16> = Vec::new();

    for header in sample_headers.iter_mut() {
        let start = header.start;
        let end = header.end;

        if start < 0 || end < start || end as usize > smpl.len() {
            return Err(SoundFontError::SampleDecompressionFailed(format!(
                "the compressed data range of sample '{}' is out of bounds ({start}..{end})",
                header.name
            )));
        }

        // The loop points of a SoundFont3 are offsets relative to the start of
        // the decompressed sample, so they must not be negative. The sanity
        // check in `SoundFont::sanity_check` only catches a negative offset on
        // the first sample (where it is not shifted by the pool base), so
        // reject them here for every sample before adding the base offset.
        // This mirrors the SoundFont2 path, which also rejects negative loop
        // points unconditionally.
        if header.start_loop < 0 || header.end_loop < 0 {
            return Err(SoundFontError::SampleDecompressionFailed(format!(
                "the loop points of sample '{}' are invalid ({}..{})",
                header.name, header.start_loop, header.end_loop
            )));
        }

        let decoded = decode_stream(&smpl[start as usize..end as usize])?;

        let base = wave_data.len() as i32;
        let length = decoded.len() as i32;

        wave_data.extend_from_slice(&decoded);
        wave_data.resize(wave_data.len() + GUARD_FRAMES, 0);

        // Rewrite the header to index the decoded pool.
        header.start = base;
        header.end = base + length;
        header.start_loop += base;
        header.end_loop += base;
    }

    Ok(wave_data)
}

/// Decompresses one Ogg Vorbis stream into 16-bit PCM samples.
///
/// SoundFont samples are mono, so the interleaved read of a mono stream is the
/// sample itself.
fn decode_stream(stream: &[u8]) -> Result<Vec<i16>, SoundFontError> {
    let mut reader = OggStreamReader::new(Cursor::new(stream))
        .map_err(|err| SoundFontError::SampleDecompressionFailed(err.to_string()))?;

    if reader.ident_hdr.audio_channels != 1 {
        return Err(SoundFontError::SampleDecompressionFailed(format!(
            "expected a mono sample, but found {} channels",
            reader.ident_hdr.audio_channels
        )));
    }

    let mut samples: Vec<i16> = Vec::new();
    while let Some(packet) = reader
        .read_dec_packet_itl()
        .map_err(|err| SoundFontError::SampleDecompressionFailed(err.to_string()))?
    {
        samples.extend_from_slice(&packet);
    }

    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;

    fn samples_dir_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("samples")
    }

    /// The raw `smpl` sub-chunk bytes of `samples/dummy.sf3` (a small,
    /// self-contained Ogg Vorbis stream).
    fn dummy_smpl() -> Vec<u8> {
        let path = samples_dir_path().join("dummy.sf3");
        let bytes = fs::read(&path).unwrap();

        // smpl sub-chunk: 4-byte ID, 4-byte little-endian size, raw data.
        let smpl_pos = bytes
            .windows(4)
            .position(|window| window == b"smpl")
            .expect("the smpl sub-chunk was not found in dummy.sf3");
        let smpl_size =
            u32::from_le_bytes(bytes[smpl_pos + 4..smpl_pos + 8].try_into().unwrap()) as usize;
        let smpl = &bytes[smpl_pos + 8..smpl_pos + 8 + smpl_size];
        assert_eq!(&smpl[..4], b"OggS");
        smpl.to_vec()
    }

    #[test]
    fn decodes_the_sample_of_dummy_sf3() {
        let bytes = fs::read(samples_dir_path().join("dummy.sf3")).unwrap();
        let smpl = dummy_smpl();

        // shdr sub-chunk: 46-byte SampleHeader records.
        // dwStart @ +20, dwEnd @ +24, dwStartloop @ +28, dwEndloop @ +32.
        let shdr_pos = bytes
            .windows(4)
            .position(|window| window == b"shdr")
            .expect("the shdr sub-chunk was not found in dummy.sf3");
        let record = shdr_pos + 8;
        let start = i32::from_le_bytes(bytes[record + 20..record + 24].try_into().unwrap());
        let end = i32::from_le_bytes(bytes[record + 24..record + 28].try_into().unwrap());
        let start_loop = i32::from_le_bytes(bytes[record + 28..record + 32].try_into().unwrap());
        let end_loop = i32::from_le_bytes(bytes[record + 32..record + 36].try_into().unwrap());

        let mut headers = vec![SampleHeader {
            name: String::from("440_sine"),
            start,
            end,
            start_loop,
            end_loop,
            sample_rate: 44100,
            original_pitch: 60,
            pitch_correction: 0,
            link: 0,
            sample_type: 17,
        }];

        let wave_data = decode_vorbis_samples(&smpl, &mut headers).unwrap();

        assert!(
            !wave_data.is_empty(),
            "the decoded PCM pool must not be empty"
        );
        assert_eq!(
            headers[0].start, 0,
            "the first sample starts at pool offset 0"
        );
        assert!(headers[0].end > 0);
        assert!(headers[0].end as usize <= wave_data.len());
        assert!(headers[0].start_loop >= 0);
        assert!(headers[0].end_loop as usize <= wave_data.len());
    }

    #[test]
    fn rejects_negative_loop_points_on_any_sample() {
        let smpl = dummy_smpl();

        // The first header is valid, so the failure must come from the second
        // one even though it is shifted by the pool base (i.e. its negative
        // relative loop point is not caught by the sanity check in SoundFont).
        let mut headers = vec![
            SampleHeader {
                name: String::from("440_sine"),
                start: 0,
                end: smpl.len() as i32,
                start_loop: 0,
                end_loop: 100,
                sample_rate: 44100,
                original_pitch: 60,
                pitch_correction: 0,
                link: 0,
                sample_type: 17,
            },
            SampleHeader {
                name: String::from("bad_loops"),
                start: 0,
                end: smpl.len() as i32,
                start_loop: -1,
                end_loop: 100,
                sample_rate: 44100,
                original_pitch: 60,
                pitch_correction: 0,
                link: 0,
                sample_type: 17,
            },
        ];

        let err = decode_vorbis_samples(&smpl, &mut headers).unwrap_err();
        assert!(matches!(err, SoundFontError::SampleDecompressionFailed(_)));
    }
}
