//! `decode(encode) == current`, over every scene and every geometry that a
//! session may agree on.

use vmlord_display_codec::{
    Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, TileSize,
    scenes::{Generator, Scene},
};

const GEOMETRIES: [(u32, u32); 4] = [
    (64, 64),     // exactly one tile at 64
    (1280, 720),  // a multiple of 16 but not of 64 in height
    (100, 70),    // clipped on both edges at every tile size
    (2560, 1440), // the largest mode the MVP offers
];

fn drive(scene: Scene, geometry: Geometry, hints: bool, frames: usize) {
    let mut generator = Generator::new(scene, geometry, 0x5EED);
    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    let mut decoder = Decoder::new(geometry);
    let stride = geometry.width() as usize * 4;

    for _ in 0..frames {
        let pixels = generator.next_frame().to_vec();
        let damage = generator.damage().to_vec();
        encoder
            .submit(
                Frame {
                    pixels: &pixels,
                    stride,
                },
                hints.then_some(damage.as_slice()),
            )
            .unwrap();

        while let Some(payload) = encoder.next_payload() {
            match payload {
                Payload::Keyframe(bytes) => {
                    let bytes = bytes.to_vec();
                    decoder.apply_keyframe(&bytes).unwrap();
                }
                Payload::TileDelta(bytes) => {
                    let bytes = bytes.to_vec();
                    decoder.apply_delta(&bytes).unwrap();
                }
                _ => panic!("no cursor in this scene"),
            }
        }

        assert_eq!(
            decoder.frame(),
            pixels.as_slice(),
            "{} at {}x{} tile {}",
            scene.name(),
            geometry.width(),
            geometry.height(),
            geometry.tile_size().as_pixels()
        );
    }
}

#[test]
fn every_scene_round_trips_at_every_tile_size() {
    for scene in Scene::ALL {
        for (width, height) in GEOMETRIES {
            for tile in [TileSize::Sixteen, TileSize::ThirtyTwo, TileSize::SixtyFour] {
                let geometry = Geometry::new(width, height, tile, PixelFormat::Bgra8888).unwrap();
                drive(scene, geometry, false, 8);
            }
        }
    }
}

#[test]
fn every_scene_round_trips_with_damage_hints() {
    for scene in Scene::ALL {
        let geometry =
            Geometry::new(1280, 720, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();
        drive(scene, geometry, true, 16);
    }
}

#[test]
fn a_generator_is_reproducible_from_its_seed() {
    let geometry = Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();
    let mut left = Generator::new(Scene::Typing, geometry, 7);
    let mut right = Generator::new(Scene::Typing, geometry, 7);

    for _ in 0..5 {
        assert_eq!(left.next_frame(), right.next_frame());
    }
}

#[test]
fn encoding_the_same_scene_twice_produces_the_same_bytes() {
    // Determinism is what the golden vectors rest on, and what lets a guest
    // and a host built on different machines agree.
    let geometry = Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();

    let encode = || {
        let mut generator = Generator::new(Scene::MovingWindow, geometry, 11);
        let mut encoder = Encoder::new(EncoderConfig::new(geometry));
        let mut payloads = Vec::new();

        for _ in 0..6 {
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

        payloads
    };

    assert_eq!(encode(), encode());
}
