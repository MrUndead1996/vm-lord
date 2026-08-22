//! The bytes this build produces, held still.
//!
//! A golden vector is the only test that fails when a format change is correct
//! in Rust and wrong on the wire. The guest and the host of a VMLord release
//! are upgraded separately, so the wire is where compatibility lives.
//!
//! To refresh after an intentional format change -- a version bump, never a
//! silent edit:
//!
//! ```text
//! VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-codec --test golden
//! ```

use std::{env, fs, path::PathBuf};

use vmlord_display_codec::{
    CursorImage, Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, TileSize,
    scenes::{Generator, Scene},
};

fn geometry() -> Geometry {
    Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

fn compare(name: &str, bytes: &[u8]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);

    if env::var_os("VMLORD_REFRESH_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        return;
    }

    let expected = fs::read(&path).unwrap_or_else(|_| panic!("missing vector {name}"));
    assert_eq!(bytes, expected.as_slice(), "{name} changed");
}

/// The first two payloads of a fixed scene: a keyframe and the delta after it.
fn frame_payloads() -> (Vec<u8>, Vec<u8>) {
    let geometry = geometry();
    let mut generator = Generator::new(Scene::MovingWindow, geometry, 7);
    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    let mut payloads = Vec::new();

    for _ in 0..2 {
        let pixels = generator.next_frame().to_vec();
        encoder
            .submit(
                Frame {
                    pixels: &pixels,
                    stride: 320 * 4,
                },
                None,
            )
            .unwrap();

        while let Some(payload) = encoder.next_payload() {
            payloads.push(match payload {
                Payload::Keyframe(bytes) | Payload::TileDelta(bytes) => bytes.to_vec(),
                _ => panic!("no cursor in this scene"),
            });
        }
    }

    (payloads[0].clone(), payloads[1].clone())
}

#[test]
fn a_keyframe_is_what_it_was() {
    let (keyframe, _) = frame_payloads();

    // A refreshed vector must be a valid one.
    let mut decoder = Decoder::new(geometry());
    decoder.apply_keyframe(&keyframe).unwrap();

    compare("keyframe.bin", &keyframe);
}

#[test]
fn a_delta_is_what_it_was() {
    let (keyframe, delta) = frame_payloads();

    let mut decoder = Decoder::new(geometry());
    decoder.apply_keyframe(&keyframe).unwrap();
    decoder.apply_delta(&delta).unwrap();

    compare("delta.bin", &delta);
}

#[test]
fn a_cursor_image_is_what_it_was() {
    // A cursor with a transparent border and a solid core: compressible, but
    // not uniformly so.
    let mut pixels = vec![0u8; 24 * 24 * 4];
    for y in 4..20 {
        for x in 4..20 {
            let offset = (y * 24 + x) * 4;
            pixels[offset..offset + 4].copy_from_slice(&0xFFEE_EEEEu32.to_le_bytes());
        }
    }

    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder
        .submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 24,
            height: 24,
            hotspot_x: 2,
            hotspot_y: 3,
        })
        .unwrap();

    let Some(Payload::CursorImage(bytes)) = encoder.next_payload() else {
        panic!("a cursor image");
    };
    let bytes = bytes.to_vec();
    assert_eq!(Decoder::decode_cursor_image(&bytes).unwrap().pixels, pixels);

    compare("cursor.bin", &bytes);
}
