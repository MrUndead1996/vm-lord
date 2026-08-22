//! The baseline compressor, over 32-bit pixels rather than bytes.
//!
//! A desktop repeats whole pixels, not bytes, and a byte-oriented coder would
//! have to rediscover that four times per pixel. Under `XorZrle` a tile that
//! changed in one corner is mostly zeros, which is the shape this is best at.
//!
//! A control varint precedes each run: the low bit picks literal (0) or repeat
//! (1), the rest is the count minus one.

use crate::{error::CodecError, varint};

/// The longest run a single control may describe.
///
/// Bounding it keeps a control varint short and keeps a corrupt count from
/// naming a run no tile could hold.
pub(crate) const MAX_RUN: usize = 65_536;

/// Appends the compressed form of `pixels`.
pub(crate) fn encode(pixels: &[u32], out: &mut Vec<u8>) {
    let mut index = 0;

    while index < pixels.len() {
        let pixel = pixels[index];
        let mut run = 1;
        while index + run < pixels.len() && pixels[index + run] == pixel && run < MAX_RUN {
            run += 1;
        }

        if run > 1 {
            varint::write(out, (((run - 1) as u32) << 1) | 1);
            out.extend_from_slice(&pixel.to_le_bytes());
            index += run;
            continue;
        }

        // A literal reaches to the next pixel that repeats: breaking out of one
        // early would cost a control of its own for nothing.
        let start = index;
        while index < pixels.len()
            && index - start < MAX_RUN
            && !(index + 1 < pixels.len() && pixels[index] == pixels[index + 1])
        {
            index += 1;
        }

        let count = index - start;
        varint::write(out, ((count - 1) as u32) << 1);
        for pixel in &pixels[start..index] {
            out.extend_from_slice(&pixel.to_le_bytes());
        }
    }
}

/// Fills `out` from a compressed stream.
///
/// # Errors
///
/// [`CodecError::Truncated`] if the stream ends before `out` is full,
/// [`CodecError::RunOverflow`] if a run reaches past its end, and
/// [`CodecError::TrailingBytes`] if bytes remain once it is full.
pub(crate) fn decode(bytes: &[u8], out: &mut [u32]) -> Result<(), CodecError> {
    let mut read = 0;
    let mut filled = 0;

    while filled < out.len() {
        let (control, width) = varint::read(&bytes[read..])?;
        read += width;

        let count = (control >> 1) as usize + 1;
        if count > MAX_RUN || count > out.len() - filled {
            return Err(CodecError::RunOverflow);
        }

        if control & 1 == 1 {
            let pixel = pixel_at(bytes, read)?;
            read += 4;
            out[filled..filled + count].fill(pixel);
        } else {
            for slot in &mut out[filled..filled + count] {
                *slot = pixel_at(bytes, read)?;
                read += 4;
            }
        }

        filled += count;
    }

    if read == bytes.len() {
        Ok(())
    } else {
        Err(CodecError::TrailingBytes)
    }
}

/// One little-endian pixel, or [`CodecError::Truncated`] if it is not there.
fn pixel_at(bytes: &[u8], offset: usize) -> Result<u32, CodecError> {
    bytes
        .get(offset..offset + 4)
        .ok_or(CodecError::Truncated)
        .map(|pixel| u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(pixels: &[u32]) -> usize {
        let mut bytes = Vec::new();
        encode(pixels, &mut bytes);

        let mut back = vec![0u32; pixels.len()];
        decode(&bytes, &mut back).unwrap();

        assert_eq!(back, pixels);
        bytes.len()
    }

    #[test]
    fn a_flat_tile_costs_a_control_and_a_pixel() {
        // A two-byte control for 1024 repeats, then the pixel itself.
        assert_eq!(round_trip(&[0x00FF_00FFu32; 1024]), 6);
    }

    #[test]
    fn a_noisy_tile_costs_its_pixels_and_a_little() {
        let pixels: Vec<u32> = (0..1024u32)
            .map(|index| index.wrapping_mul(2_654_435_761))
            .collect();

        assert!(round_trip(&pixels) <= pixels.len() * 4 + 8);
    }

    #[test]
    fn mixed_runs_and_literals_round_trip() {
        let mut pixels = vec![0u32; 300];
        pixels[100] = 7;
        pixels[101] = 9;
        pixels[102] = 9;
        round_trip(&pixels);
    }

    #[test]
    fn runs_are_split_at_the_maximum() {
        let pixels = vec![5u32; MAX_RUN + 10];
        round_trip(&pixels);
    }

    #[test]
    fn a_truncated_stream_is_an_error() {
        let mut bytes = Vec::new();
        encode(&[1u32, 2, 3, 4], &mut bytes);
        bytes.truncate(bytes.len() - 1);

        let mut back = [0u32; 4];
        assert!(matches!(
            decode(&bytes, &mut back),
            Err(CodecError::Truncated)
        ));
    }

    #[test]
    fn a_run_past_the_end_of_the_tile_is_an_error() {
        let mut bytes = Vec::new();
        encode(&[1u32; 64], &mut bytes);

        let mut back = [0u32; 32];
        assert!(matches!(
            decode(&bytes, &mut back),
            Err(CodecError::RunOverflow)
        ));
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = Vec::new();
        encode(&[1u32; 8], &mut bytes);
        bytes.push(0);

        let mut back = [0u32; 8];
        assert!(matches!(
            decode(&bytes, &mut back),
            Err(CodecError::TrailingBytes)
        ));
    }

    #[test]
    fn an_unfilled_tile_is_truncated() {
        let mut bytes = Vec::new();
        encode(&[1u32; 8], &mut bytes);

        let mut back = [0u32; 16];
        assert!(matches!(
            decode(&bytes, &mut back),
            Err(CodecError::Truncated)
        ));
    }
}
