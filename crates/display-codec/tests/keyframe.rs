//! A keyframe carries a whole frame and needs nothing before it.

use vmlord_display_codec::{
    Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, TileSize,
};

fn geometry() -> Geometry {
    Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

/// A frame whose pixels differ per position, so a wrong tile placement shows.
fn gradient(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&(x * 7 + y * 131).to_le_bytes());
        }
    }
    pixels
}

#[test]
fn a_keyframe_round_trips() {
    let pixels = gradient(100, 70);
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder
        .submit(
            Frame {
                pixels: &pixels,
                stride: 400,
            },
            None,
        )
        .unwrap();

    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("the first payload is a keyframe");
    };
    let bytes = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    let damage = decoder.apply_keyframe(&bytes).unwrap().to_vec();

    assert_eq!(decoder.frame(), pixels.as_slice());
    assert_eq!(damage.len(), geometry().tile_count() as usize);
}

#[test]
fn a_flat_keyframe_is_far_smaller_than_raw() {
    let pixels = vec![0u8; 100 * 70 * 4];
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder
        .submit(
            Frame {
                pixels: &pixels,
                stride: 400,
            },
            None,
        )
        .unwrap();

    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    assert!(bytes.len() < pixels.len() / 10);
}

#[test]
fn a_raw_keyframe_stays_inside_the_records_slack() {
    // The protocol caps a frame record at width * height * 4 + 64 KiB. The
    // worst case is a keyframe whose every tile is incompressible.
    let geometry = Geometry::new(2560, 1440, TileSize::Sixteen, PixelFormat::Bgra8888).unwrap();
    let mut pixels = vec![0u8; 2560 * 1440 * 4];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for chunk in pixels.chunks_exact_mut(4) {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        chunk.copy_from_slice(&(state as u32).to_le_bytes());
    }

    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    encoder
        .submit(
            Frame {
                pixels: &pixels,
                stride: 2560 * 4,
            },
            None,
        )
        .unwrap();

    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    assert!(
        bytes.len() <= pixels.len() + 64 * 1024,
        "{} bytes",
        bytes.len()
    );
}

#[test]
fn a_frame_of_the_wrong_size_is_refused() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    let short = vec![0u8; 10];

    assert!(
        encoder
            .submit(
                Frame {
                    pixels: &short,
                    stride: 400
                },
                None
            )
            .is_err()
    );
}

#[test]
fn a_stride_wider_than_the_frame_is_honoured() {
    // Capture backends pad rows; the padding must not reach the wire.
    let stride = 512;
    let mut padded = vec![0xCDu8; stride * 70];
    let pixels = gradient(100, 70);
    for y in 0..70 {
        let row = &pixels[y * 400..(y + 1) * 400];
        padded[y * stride..y * stride + 400].copy_from_slice(row);
    }

    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder
        .submit(
            Frame {
                pixels: &padded,
                stride,
            },
            None,
        )
        .unwrap();
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    decoder.apply_keyframe(&bytes).unwrap();
    assert_eq!(decoder.frame(), pixels.as_slice());
}

#[test]
fn nothing_is_produced_without_a_submitted_frame() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    assert!(encoder.next_payload().is_none());
}
