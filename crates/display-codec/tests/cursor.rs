//! The cursor is its own stream, with its own state and its own limits.

use vmlord_display_codec::{
    CodecError, CursorImage, CursorPosition, Decoder, Encoder, EncoderConfig, Geometry,
    MAX_CURSOR_DIMENSION, Payload, PixelFormat, TileSize,
};

fn encoder() -> Encoder {
    let geometry = Geometry::new(128, 96, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();
    Encoder::new(EncoderConfig::new(geometry))
}

#[test]
fn a_cursor_image_round_trips() {
    let pixels = vec![0xA5u8; 32 * 32 * 4];
    let mut encoder = encoder();
    encoder
        .submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 32,
            height: 32,
            hotspot_x: 4,
            hotspot_y: 6,
        })
        .unwrap();

    let Some(Payload::CursorImage(bytes)) = encoder.next_payload() else {
        panic!("a cursor image");
    };
    let image = Decoder::decode_cursor_image(bytes).unwrap();

    assert_eq!(image.width, 32);
    assert_eq!(image.hotspot_y, 6);
    assert_eq!(image.pixels, pixels);
}

#[test]
fn a_noisy_cursor_image_round_trips_as_raw() {
    // Incompressible pixels must not grow the record.
    let mut pixels = vec![0u8; 16 * 16 * 4];
    let mut state = 0x9E37_79B9u32;
    for chunk in pixels.chunks_exact_mut(4) {
        state = state.wrapping_mul(2_654_435_761).wrapping_add(1);
        chunk.copy_from_slice(&state.to_le_bytes());
    }

    let mut encoder = encoder();
    encoder
        .submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 16,
            height: 16,
            hotspot_x: 0,
            hotspot_y: 0,
        })
        .unwrap();

    let Some(Payload::CursorImage(bytes)) = encoder.next_payload() else {
        panic!("a cursor image");
    };
    assert_eq!(bytes.len(), 10 + pixels.len());
    assert_eq!(Decoder::decode_cursor_image(bytes).unwrap().pixels, pixels);
}

#[test]
fn a_cursor_position_is_six_bytes() {
    let mut encoder = encoder();
    encoder.submit_cursor_position(CursorPosition {
        x: 700,
        y: 400,
        visible: false,
    });

    let Some(Payload::CursorPosition(bytes)) = encoder.next_payload() else {
        panic!("a cursor position");
    };
    assert_eq!(bytes.len(), 6);

    let position = Decoder::decode_cursor_position(bytes).unwrap();
    assert_eq!(
        position,
        CursorPosition {
            x: 700,
            y: 400,
            visible: false
        }
    );
}

#[test]
fn an_oversized_cursor_is_refused_on_both_sides() {
    let side = MAX_CURSOR_DIMENSION + 1;
    let pixels = vec![0u8; (side * side * 4) as usize];
    let mut encoder = encoder();

    assert!(matches!(
        encoder.submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: side,
            height: side,
            hotspot_x: 0,
            hotspot_y: 0,
        }),
        Err(CodecError::CursorTooLarge)
    ));

    // And a payload claiming it, built by hand, is refused by the decoder.
    let mut bytes = vec![1u8, 0];
    bytes.extend_from_slice(&(side as u16).to_le_bytes());
    bytes.extend_from_slice(&(side as u16).to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    assert!(matches!(
        Decoder::decode_cursor_image(&bytes),
        Err(CodecError::CursorTooLarge)
    ));
}

#[test]
fn a_cursor_image_of_the_wrong_pixel_count_is_refused() {
    let mut encoder = encoder();
    let pixels = vec![0u8; 10];

    assert!(
        encoder
            .submit_cursor_image(CursorImage {
                pixels: &pixels,
                width: 32,
                height: 32,
                hotspot_x: 0,
                hotspot_y: 0,
            })
            .is_err()
    );
}

#[test]
fn a_hotspot_outside_the_image_is_refused() {
    let mut encoder = encoder();
    let pixels = vec![0u8; 32 * 32 * 4];

    assert!(matches!(
        encoder.submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 32,
            height: 32,
            hotspot_x: 32,
            hotspot_y: 0,
        }),
        Err(CodecError::CursorTooLarge)
    ));
}

#[test]
fn a_truncated_cursor_image_is_an_error() {
    let pixels = vec![0x11u8; 16 * 16 * 4];
    let mut encoder = encoder();
    encoder
        .submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 16,
            height: 16,
            hotspot_x: 0,
            hotspot_y: 0,
        })
        .unwrap();
    let Some(Payload::CursorImage(bytes)) = encoder.next_payload() else {
        panic!("a cursor image");
    };
    let short = bytes[..bytes.len() - 1].to_vec();

    assert!(Decoder::decode_cursor_image(&short).is_err());
}

#[test]
fn a_cursor_position_of_the_wrong_length_is_an_error() {
    assert!(matches!(
        Decoder::decode_cursor_position(&[1, 1, 0, 0, 0]),
        Err(CodecError::Truncated)
    ));
    assert!(matches!(
        Decoder::decode_cursor_position(&[1, 1, 0, 0, 0, 0, 0]),
        Err(CodecError::TrailingBytes)
    ));
    assert!(matches!(
        Decoder::decode_cursor_position(&[2, 1, 0, 0, 0, 0]),
        Err(CodecError::UnknownVersion { version: 2 })
    ));
}
