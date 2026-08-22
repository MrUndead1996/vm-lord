//! One hand-built payload per way of being wrong.
//!
//! Each test asserts the specific error, not merely that there was one: an
//! error is what a session acts on, and "something was wrong" is not a
//! diagnosis a viewer can log or a guest can answer.

use vmlord_display_codec::{CodecError, Decoder, Geometry, PixelFormat, TileSize};

/// 128x96 at tile 32 is a 4x3 grid of whole tiles, so a tile is 32 * 32
/// pixels and every index below 12 is valid.
fn geometry() -> Geometry {
    Geometry::new(128, 96, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

const TILE_PIXELS: usize = 32 * 32;

/// A container header: version, flags, columns, rows, two reserved bytes.
fn header(keyframe: bool) -> Vec<u8> {
    vec![1, u8::from(keyframe), 4, 0, 3, 0, 0, 0]
}

/// One whole tile as a single ZRLE repeat run of `colour`.
///
/// The control varint is `(count - 1) << 1 | 1`, which for a 1024-pixel tile
/// needs two bytes.
fn flat_tile(colour: u32) -> Vec<u8> {
    let control = ((TILE_PIXELS as u32 - 1) << 1) | 1;
    let mut run = vec![(control as u8) | 0x80, (control >> 7) as u8];
    run.extend_from_slice(&colour.to_le_bytes());
    run
}

/// A decoder that already holds a keyframe, so that a delta reaches its body.
fn primed() -> Decoder {
    let mut decoder = Decoder::new(geometry());
    let mut keyframe = header(true);
    for _ in 0..12 {
        let run = flat_tile(0);
        keyframe.push(1);
        keyframe.push(run.len() as u8);
        keyframe.extend_from_slice(&run);
    }

    decoder.apply_keyframe(&keyframe).unwrap();
    decoder
}

#[test]
fn a_payload_shorter_than_a_header_is_truncated() {
    let mut decoder = Decoder::new(geometry());

    assert!(matches!(
        decoder.apply_keyframe(&[1, 1, 4]),
        Err(CodecError::Truncated)
    ));
}

#[test]
fn a_future_format_version_is_named() {
    let mut decoder = Decoder::new(geometry());
    let mut payload = header(true);
    payload[0] = 2;

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::UnknownVersion { version: 2 })
    ));
}

#[test]
fn reserved_header_bytes_must_be_zero() {
    let mut decoder = Decoder::new(geometry());
    let mut payload = header(true);
    payload[7] = 0x80;

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::TrailingBytes)
    ));
}

#[test]
fn another_streams_grid_is_named() {
    let mut decoder = Decoder::new(geometry());
    let mut payload = header(true);
    payload[2] = 40;

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::GridMismatch {
            columns: 40,
            rows: 3
        })
    ));
}

#[test]
fn a_keyframe_that_ends_early_is_truncated() {
    let mut decoder = Decoder::new(geometry());
    let mut payload = header(true);
    payload.push(0); // Raw, and then nothing.

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::Truncated)
    ));
}

#[test]
fn a_keyframe_with_extra_bytes_is_refused() {
    let mut decoder = primed();
    let mut payload = header(true);
    for _ in 0..12 {
        let run = flat_tile(0);
        payload.push(1);
        payload.push(run.len() as u8);
        payload.extend_from_slice(&run);
    }
    payload.push(0xFF);

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::TrailingBytes)
    ));
}

#[test]
fn an_unknown_method_is_named() {
    let mut decoder = Decoder::new(geometry());
    let mut payload = header(true);
    payload.push(9);

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::UnknownMethod { method: 9 })
    ));
}

#[test]
fn a_keyframe_cannot_carry_a_difference() {
    // XorZrle depends on a previous tile, which a keyframe by definition has
    // not got.
    let mut decoder = Decoder::new(geometry());
    let mut payload = header(true);
    payload.push(2);

    assert!(matches!(
        decoder.apply_keyframe(&payload),
        Err(CodecError::UnknownMethod { method: 2 })
    ));
}

#[test]
fn a_delta_before_a_keyframe_has_no_base() {
    let mut decoder = Decoder::new(geometry());

    assert!(matches!(
        decoder.apply_delta(&header(false)),
        Err(CodecError::NoBase)
    ));
}

#[test]
fn a_keyframe_applied_as_a_delta_is_the_wrong_kind() {
    let mut decoder = primed();

    assert!(matches!(
        decoder.apply_delta(&header(true)),
        Err(CodecError::WrongPayloadKind)
    ));
    assert!(matches!(
        decoder.apply_keyframe(&header(false)),
        Err(CodecError::WrongPayloadKind)
    ));
}

#[test]
fn a_tile_index_past_the_grid_is_named() {
    let mut decoder = primed();
    let mut payload = header(false);
    payload.push(12); // The grid holds indices 0..12.
    payload.push(0);

    assert!(matches!(
        decoder.apply_delta(&payload),
        Err(CodecError::TileIndexOutOfRange { index: 12 })
    ));
}

#[test]
fn repeated_tile_indices_are_refused() {
    let mut decoder = primed();
    let mut payload = header(false);
    let run = flat_tile(0x0403_0201);

    for _ in 0..2 {
        payload.push(0);
        payload.push(1);
        payload.push(run.len() as u8);
        payload.extend_from_slice(&run);
    }

    assert!(matches!(
        decoder.apply_delta(&payload),
        Err(CodecError::TileIndexNotIncreasing { index: 0 })
    ));
}

#[test]
fn a_run_longer_than_its_tile_is_named() {
    let mut decoder = primed();
    let mut payload = header(false);
    payload.push(0);
    payload.push(1);

    // A repeat run of 4096 pixels into a 1024-pixel tile.
    let control = ((4096u32 - 1) << 1) | 1;
    let run = [
        (control as u8) | 0x80,
        ((control >> 7) as u8) | 0x80,
        (control >> 14) as u8,
        7,
        7,
        7,
        7,
    ];
    payload.push(run.len() as u8);
    payload.extend_from_slice(&run);

    assert!(matches!(
        decoder.apply_delta(&payload),
        Err(CodecError::RunOverflow)
    ));
}

#[test]
fn an_oversized_cursor_is_named() {
    let mut bytes = vec![1u8, 0];
    bytes.extend_from_slice(&300u16.to_le_bytes());
    bytes.extend_from_slice(&300u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());

    assert!(matches!(
        Decoder::decode_cursor_image(&bytes),
        Err(CodecError::CursorTooLarge)
    ));
}

#[test]
fn a_cursor_of_an_unknown_method_is_named() {
    let mut bytes = vec![1u8, 7];
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());

    assert!(matches!(
        Decoder::decode_cursor_image(&bytes),
        Err(CodecError::UnknownMethod { method: 7 })
    ));
}

#[test]
fn a_truncated_cursor_bitmap_is_named() {
    let mut bytes = vec![1u8, 0];
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);

    assert!(matches!(
        Decoder::decode_cursor_image(&bytes),
        Err(CodecError::Truncated)
    ));
}
