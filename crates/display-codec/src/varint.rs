//! LEB128 over `u32`, which is how a tile index and a compressed length are
//! written.
//!
//! A fixed four-byte field would be simpler and would cost more where it
//! matters: a delta's indices and lengths are small, and a keyframe's per-tile
//! overhead is measured against 64 KiB of slack in the record cap.

use crate::error::CodecError;

/// Appends `value` as a varint.
pub(crate) fn write(out: &mut Vec<u8>, value: u32) {
    let mut rest = value;
    while rest >= 0x80 {
        out.push((rest as u8) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// Reads a varint, and says how many bytes it took.
///
/// # Errors
///
/// [`CodecError::Truncated`] for a varint that runs off the end of `bytes` or
/// one longer than a `u32` can be.
pub(crate) fn read(bytes: &[u8]) -> Result<(u32, usize), CodecError> {
    let mut value = 0u32;
    let mut shift = 0;

    for (index, byte) in bytes.iter().enumerate() {
        // The fifth byte carries the top four bits, so anything above it -- or
        // any fifth byte with bits it cannot hold -- is not a u32.
        if shift == 28 && (byte & 0xF0) != 0 {
            return Err(CodecError::Truncated);
        }

        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }

        shift += 7;
        if shift > 28 {
            return Err(CodecError::Truncated);
        }
    }

    Err(CodecError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_the_length_boundaries() {
        for value in [0u32, 1, 127, 128, 16_383, 16_384, u32::MAX] {
            let mut bytes = Vec::new();
            write(&mut bytes, value);
            assert_eq!(read(&bytes).unwrap(), (value, bytes.len()));
        }
    }

    #[test]
    fn a_truncated_varint_is_an_error() {
        assert!(matches!(read(&[0x80]), Err(CodecError::Truncated)));
        assert!(matches!(read(&[]), Err(CodecError::Truncated)));
    }

    #[test]
    fn an_overlong_varint_is_an_error() {
        // Six continuation bytes cannot be a u32.
        assert!(matches!(
            read(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Err(CodecError::Truncated)
        ));
    }
}
