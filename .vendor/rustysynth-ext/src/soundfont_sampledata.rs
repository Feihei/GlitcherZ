use std::io::Read;

use crate::binary_reader::BinaryReader;
use crate::error::SoundFontError;
use crate::four_cc::FourCC;
use crate::read_counter::ReadCounter;
use crate::sample_header::SampleHeader;

#[non_exhaustive]
pub(crate) struct SoundFontSampleData {
    pub bits_per_sample: i32,
    pub wave_data: Vec<i16>,
}

impl SoundFontSampleData {
    /// Reads the `sdta` LIST chunk and returns the raw `smpl` sub-chunk bytes.
    ///
    /// The bytes are either little-endian 16-bit PCM (SoundFont2) or
    /// concatenated Ogg Vorbis streams (SoundFont3); they are turned into PCM
    /// by [`SoundFontSampleData::new`]. The 24-bit `sm24` sub-chunk, which
    /// only applies to SoundFont2, is discarded.
    pub(crate) fn read<R: Read>(reader: &mut R) -> Result<Vec<u8>, SoundFontError> {
        let chunk_id = BinaryReader::read_four_cc(reader)?;
        if chunk_id != b"LIST" {
            return Err(SoundFontError::ListChunkNotFound);
        }

        let end = BinaryReader::read_u32(reader)? as usize;
        let reader = &mut ReadCounter::new(reader);

        let list_type = BinaryReader::read_four_cc(reader)?;
        if list_type != b"sdta" {
            return Err(SoundFontError::InvalidListChunkType {
                expected: FourCC::from_bytes(*b"sdta"),
                actual: list_type,
            });
        }

        let mut sample_data: Option<Vec<u8>> = None;

        while reader.bytes_read() < end {
            let id = BinaryReader::read_four_cc(reader)?;
            let size = BinaryReader::read_u32(reader)? as usize;

            match id.as_bytes() {
                b"smpl" => sample_data = Some(BinaryReader::read_raw_data(reader, size)?),
                b"sm24" => BinaryReader::discard_data(reader, size)?,
                _ => return Err(SoundFontError::ListContainsUnknownId(id)),
            }
        }

        sample_data.ok_or(SoundFontError::SampleDataNotFound)
    }

    /// Turns the raw `smpl` bytes into 16-bit PCM sample data.
    ///
    /// SoundFont2 stores the samples as raw little-endian 16-bit PCM. A
    /// SoundFont3 instead stores each sample as an Ogg Vorbis stream and keeps
    /// the streams concatenated in the `smpl` chunk (identified by the `OggS`
    /// magic); those are decompressed here, which also rewrites `sample_headers`
    /// so that they reference the decoded data instead of the compressed bytes.
    pub(crate) fn new(
        sample_data: Vec<u8>,
        sample_headers: &mut [SampleHeader],
    ) -> Result<Self, SoundFontError> {
        // SoundFont3 compressed samples start with the "OggS" magic.
        if sample_data.len() >= 4 && &sample_data[..4] == b"OggS" {
            #[cfg(not(feature = "sf3"))]
            {
                let _ = sample_headers;
                return Err(SoundFontError::UnsupportedSampleFormat);
            }

            #[cfg(feature = "sf3")]
            {
                let wave_data =
                    crate::soundfont_sf3::decode_vorbis_samples(&sample_data, sample_headers)?;
                return Ok(Self {
                    bits_per_sample: 16,
                    wave_data,
                });
            }
        }

        // SoundFont2: the sample data is raw little-endian 16-bit PCM.
        // The output size is known, so allocate exactly once.
        let mut wave_data: Vec<i16> = Vec::with_capacity(sample_data.len() / 2);
        wave_data.extend(
            sample_data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| i16::from_le_bytes(*bytes)),
        );

        if wave_data.len() < 2 {
            return Err(SoundFontError::SampleDataNotFound);
        }

        Ok(Self {
            bits_per_sample: 16,
            wave_data,
        })
    }
}
