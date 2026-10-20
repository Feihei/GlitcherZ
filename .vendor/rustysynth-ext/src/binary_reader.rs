use std::cmp;
use std::io;
use std::io::ErrorKind;
use std::io::Read;

use crate::four_cc::FourCC;

#[non_exhaustive]
pub(crate) struct BinaryReader {}

impl BinaryReader {
    pub(crate) fn read_i8<R: Read>(reader: &mut R) -> Result<i8, io::Error> {
        let mut data: [u8; 1] = [0; 1];
        reader.read_exact(&mut data)?;
        Ok(i8::from_le_bytes(data))
    }

    pub(crate) fn read_u8<R: Read>(reader: &mut R) -> Result<u8, io::Error> {
        let mut data: [u8; 1] = [0; 1];
        reader.read_exact(&mut data)?;
        Ok(u8::from_le_bytes(data))
    }

    pub(crate) fn read_i16<R: Read>(reader: &mut R) -> Result<i16, io::Error> {
        let mut data: [u8; 2] = [0; 2];
        reader.read_exact(&mut data)?;
        Ok(i16::from_le_bytes(data))
    }

    pub(crate) fn read_u16<R: Read>(reader: &mut R) -> Result<u16, io::Error> {
        let mut data: [u8; 2] = [0; 2];
        reader.read_exact(&mut data)?;
        Ok(u16::from_le_bytes(data))
    }

    pub(crate) fn read_i32<R: Read>(reader: &mut R) -> Result<i32, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(i32::from_le_bytes(data))
    }

    pub(crate) fn read_u32<R: Read>(reader: &mut R) -> Result<u32, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(u32::from_le_bytes(data))
    }

    pub(crate) fn read_i16_big_endian<R: Read>(reader: &mut R) -> Result<i16, io::Error> {
        let mut data: [u8; 2] = [0; 2];
        reader.read_exact(&mut data)?;
        Ok(i16::from_be_bytes(data))
    }

    pub(crate) fn read_i32_big_endian<R: Read>(reader: &mut R) -> Result<i32, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(i32::from_be_bytes(data))
    }

    pub(crate) fn read_i32_variable_length<R: Read>(reader: &mut R) -> Result<i32, io::Error> {
        let mut acc: i32 = 0;
        let mut count: i32 = 0;

        loop {
            let value = BinaryReader::read_u8(reader)? as i32;
            acc = (acc << 7) | (value & 127);
            if (value & 128) == 0 {
                break;
            }
            count += 1;
            if count == 4 {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "the length of the value must be equal to or less than 4",
                ));
            }
        }

        Ok(acc)
    }

    pub(crate) fn read_four_cc<R: Read>(reader: &mut R) -> Result<FourCC, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(FourCC::from_bytes(data))
    }

    pub(crate) fn read_fixed_length_string<R: Read>(
        reader: &mut R,
        length: usize,
    ) -> Result<String, io::Error> {
        let mut data: Vec<u8> = vec![0; length];
        reader.read_exact(&mut data)?;

        let mut actual_length: usize = 0;
        for value in &data {
            if *value == 0 {
                break;
            }
            actual_length += 1;
        }

        // Replace non-ASCII characters with '?'.
        // Tabs and returns are preserved.
        for value in &mut data[0..actual_length] {
            if !(9..=126).contains(value) {
                *value = 63; // '?'
            }
        }

        data.truncate(actual_length);

        // Every byte is a valid ASCII character now (9..=126 or '?'), so this cannot fail.
        Ok(String::from_utf8(data).unwrap())
    }

    pub(crate) fn discard_data<R: Read>(reader: &mut R, mut size: usize) -> Result<(), io::Error> {
        // Read into a small stack buffer instead of allocating a Vec of the full size
        // just to throw the bytes away.
        let mut data: [u8; 4096] = [0; 4096];

        while size > 0 {
            let chunk = cmp::min(size, data.len());
            reader.read_exact(&mut data[..chunk])?;
            size -= chunk;
        }

        Ok(())
    }

    /// Reads a chunk of raw bytes as-is.
    ///
    /// The sample data of a SoundFont2 is little-endian 16-bit PCM, while a
    /// SoundFont3 stores Ogg Vorbis streams instead, so the sample data chunk
    /// is read as raw bytes and interpreted later. Reading raw bytes also
    /// keeps odd-sized chunks (legal in SoundFont3) safe.
    pub(crate) fn read_raw_data<R: Read>(
        reader: &mut R,
        size: usize,
    ) -> Result<Vec<u8>, io::Error> {
        let mut data: Vec<u8> = vec![0; size];
        reader.read_exact(&mut data)?;
        Ok(data)
    }
}
