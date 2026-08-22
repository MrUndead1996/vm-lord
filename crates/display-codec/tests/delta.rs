//! What a delta carries, and what it must refuse.

use vmlord_display_codec::{
    CodecError, Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, Rect,
    TileSize,
};

fn geometry() -> Geometry {
    Geometry::new(128, 96, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

const STRIDE: usize = 128 * 4;

struct Pair {
    encoder: Encoder,
    decoder: Decoder,
}

impl Pair {
    fn new() -> Self {
        Self {
            encoder: Encoder::new(EncoderConfig::new(geometry())),
            decoder: Decoder::new(geometry()),
        }
    }

    /// Submits a frame, applies whatever came out, and returns the payload's
    /// size and the rectangles the decoder reported.
    fn round(&mut self, pixels: &[u8], damage: Option<&[Rect]>) -> Option<(usize, Vec<Rect>)> {
        self.encoder
            .submit(
                Frame {
                    pixels,
                    stride: STRIDE,
                },
                damage,
            )
            .unwrap();
        let payload = self.encoder.next_payload()?;
        let (bytes, keyframe) = match payload {
            Payload::Keyframe(bytes) => (bytes.to_vec(), true),
            Payload::TileDelta(bytes) => (bytes.to_vec(), false),
            _ => panic!("a frame payload"),
        };

        let damage = if keyframe {
            self.decoder.apply_keyframe(&bytes).unwrap().to_vec()
        } else {
            self.decoder.apply_delta(&bytes).unwrap().to_vec()
        };

        Some((bytes.len(), damage))
    }
}

fn blank() -> Vec<u8> {
    vec![0u8; 128 * 96 * 4]
}

#[test]
fn one_changed_tile_costs_one_tile() {
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let mut next = blank();
    // Tile (1, 1) -- pixel (40, 40).
    let offset = (40 * 128 + 40) * 4;
    next[offset..offset + 4].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());

    let (size, damage) = pair.round(&next, None).unwrap();
    assert_eq!(pair.decoder.frame(), next.as_slice());
    assert_eq!(
        damage,
        vec![Rect {
            x: 32,
            y: 32,
            width: 32,
            height: 32
        }]
    );
    assert!(size < 512, "{size} bytes for a one-pixel change");
}

#[test]
fn an_unchanged_frame_produces_nothing() {
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    assert!(pair.round(&blank(), None).is_none());
}

#[test]
fn a_damage_hint_limits_the_tiles_compared() {
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let next = vec![0x40u8; 128 * 96 * 4];

    // The hint covers one tile, so only that tile may travel -- and the
    // decoder must match the encoder's belief, not the submitted frame.
    let hint = [Rect {
        x: 0,
        y: 0,
        width: 32,
        height: 32,
    }];
    let (_, damage) = pair.round(&next, Some(&hint)).unwrap();

    assert_eq!(
        damage,
        vec![Rect {
            x: 0,
            y: 0,
            width: 32,
            height: 32
        }]
    );
}

#[test]
fn a_hint_that_misses_a_change_does_not_desynchronise_the_stream() {
    // An under-reporting capture backend loses pixels until a later hint or a
    // keyframe, which is a bug over there. What must hold here is that the
    // encoder's reference and the decoder's frame stay identical.
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let mut next = blank();
    next[(70 * 128 + 70) * 4] = 0xFF;
    let hint = [Rect {
        x: 0,
        y: 0,
        width: 32,
        height: 32,
    }];
    assert!(pair.round(&next, Some(&hint)).is_none());
    assert_eq!(pair.decoder.frame(), blank().as_slice());

    // A later hint that does cover the tile brings both sides back together.
    let hint = [Rect {
        x: 64,
        y: 64,
        width: 32,
        height: 32,
    }];
    pair.round(&next, Some(&hint)).unwrap();
    assert_eq!(pair.decoder.frame(), next.as_slice());
}

#[test]
fn hints_accumulate_across_a_displaced_frame() {
    // A frame dropped before it was encoded still changed pixels the newer
    // frame keeps. Its hint is the only record of where they were, so hints
    // are unioned until a payload is actually produced.
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let mut first = blank();
    first[(10 * 128 + 10) * 4] = 0xFF;
    pair.encoder
        .submit(
            Frame {
                pixels: &first,
                stride: STRIDE,
            },
            Some(&[Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            }]),
        )
        .unwrap();

    let mut second = first.clone();
    second[(70 * 128 + 70) * 4] = 0xFF;
    pair.encoder
        .submit(
            Frame {
                pixels: &second,
                stride: STRIDE,
            },
            Some(&[Rect {
                x: 64,
                y: 64,
                width: 32,
                height: 32,
            }]),
        )
        .unwrap();

    let Some(Payload::TileDelta(bytes)) = pair.encoder.next_payload() else {
        panic!("a delta");
    };
    let bytes = bytes.to_vec();
    pair.decoder.apply_delta(&bytes).unwrap();

    assert_eq!(pair.decoder.frame(), second.as_slice());
}

#[test]
fn a_hint_outside_the_frame_is_clipped_not_refused() {
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let mut next = blank();
    next[0] = 0xFF;
    let hint = [Rect {
        x: 0,
        y: 0,
        width: 4096,
        height: 4096,
    }];
    assert!(pair.round(&next, Some(&hint)).is_some());

    let mut later = next.clone();
    later[4] = 0xFF;
    let beyond = [Rect {
        x: 9000,
        y: 9000,
        width: 32,
        height: 32,
    }];
    assert!(pair.round(&later, Some(&beyond)).is_none());
}

#[test]
fn a_delta_before_a_keyframe_has_no_base() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder
        .submit(
            Frame {
                pixels: &blank(),
                stride: STRIDE,
            },
            None,
        )
        .unwrap();
    let Some(Payload::Keyframe(_)) = encoder.next_payload() else {
        panic!("a keyframe");
    };

    let mut next = blank();
    next[0] = 0xFF;
    encoder
        .submit(
            Frame {
                pixels: &next,
                stride: STRIDE,
            },
            None,
        )
        .unwrap();
    let Some(Payload::TileDelta(bytes)) = encoder.next_payload() else {
        panic!("a delta");
    };
    let bytes = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    assert!(matches!(
        decoder.apply_delta(&bytes),
        Err(CodecError::NoBase)
    ));
}

#[test]
fn a_keyframe_applied_as_a_delta_is_refused_and_the_reverse_too() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder
        .submit(
            Frame {
                pixels: &blank(),
                stride: STRIDE,
            },
            None,
        )
        .unwrap();
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let keyframe = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    assert!(matches!(
        decoder.apply_delta(&keyframe),
        Err(CodecError::WrongPayloadKind)
    ));
    decoder.apply_keyframe(&keyframe).unwrap();

    let mut next = blank();
    next[0] = 0xFF;
    encoder
        .submit(
            Frame {
                pixels: &next,
                stride: STRIDE,
            },
            None,
        )
        .unwrap();
    let Some(Payload::TileDelta(bytes)) = encoder.next_payload() else {
        panic!("a delta");
    };
    let delta = bytes.to_vec();
    assert!(matches!(
        decoder.apply_keyframe(&delta),
        Err(CodecError::WrongPayloadKind)
    ));
}

#[test]
fn a_moving_block_costs_far_less_than_a_keyframe() {
    let mut pair = Pair::new();
    let mut background = blank();
    for (index, chunk) in background.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&(index as u32).wrapping_mul(2_654_435_761).to_le_bytes());
    }
    let (keyframe_size, _) = pair.round(&background, None).unwrap();

    let mut moved = background.clone();
    for y in 40..60 {
        for x in 40..60 {
            let offset = (y * 128 + x) * 4;
            moved[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
        }
    }
    let (delta_size, _) = pair.round(&moved, None).unwrap();

    assert!(
        delta_size * 4 < keyframe_size,
        "delta {delta_size}, keyframe {keyframe_size}"
    );
}
