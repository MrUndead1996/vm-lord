//! The queue keeps current state and drops what is stale -- and what may be
//! dropped is a *captured* frame, never an encoded one.

use vmlord_display_codec::{
    CursorImage, CursorPosition, Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload,
    PixelFormat, TileSize,
};

fn geometry() -> Geometry {
    Geometry::new(128, 96, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

const STRIDE: usize = 128 * 4;

fn frame(fill: u8) -> Vec<u8> {
    vec![fill; 128 * 96 * 4]
}

fn submit(encoder: &mut Encoder, pixels: &[u8]) {
    encoder
        .submit(
            Frame {
                pixels,
                stride: STRIDE,
            },
            None,
        )
        .unwrap();
}

#[test]
fn a_frame_submitted_twice_encodes_only_the_newer_one() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    let mut decoder = Decoder::new(geometry());

    submit(&mut encoder, &frame(1));
    submit(&mut encoder, &frame(2));

    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();
    decoder.apply_keyframe(&bytes).unwrap();

    assert_eq!(decoder.frame(), frame(2).as_slice());
    assert!(
        encoder.next_payload().is_none(),
        "the older frame is gone, not queued"
    );
}

#[test]
fn the_reference_advances_only_when_a_payload_is_taken() {
    // The invariant the whole design rests on: what the encoder believes the
    // far side holds is the last payload the caller was handed.
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    let mut decoder = Decoder::new(geometry());

    submit(&mut encoder, &frame(1));
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();
    decoder.apply_keyframe(&bytes).unwrap();

    // Three captures arrive while the socket is busy; only the last survives.
    submit(&mut encoder, &frame(2));
    submit(&mut encoder, &frame(3));
    submit(&mut encoder, &frame(4));

    let Some(Payload::TileDelta(bytes)) = encoder.next_payload() else {
        panic!("a delta");
    };
    let bytes = bytes.to_vec();
    decoder.apply_delta(&bytes).unwrap();

    assert_eq!(decoder.frame(), frame(4).as_slice());
}

#[test]
fn a_keyframe_request_outranks_a_pending_cursor_move() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    submit(&mut encoder, &frame(1));
    let _ = encoder.next_payload();

    submit(&mut encoder, &frame(2));
    encoder.submit_cursor_position(CursorPosition {
        x: 1,
        y: 1,
        visible: true,
    });
    encoder
        .submit_cursor_image(CursorImage {
            pixels: &vec![0u8; 16 * 16 * 4],
            width: 16,
            height: 16,
            hotspot_x: 0,
            hotspot_y: 0,
        })
        .unwrap();
    encoder.request_keyframe();

    // The frame first -- and a keyframe, because one was asked for.
    assert!(matches!(encoder.next_payload(), Some(Payload::Keyframe(_))));
    assert!(matches!(
        encoder.next_payload(),
        Some(Payload::CursorImage(_))
    ));
    assert!(matches!(
        encoder.next_payload(),
        Some(Payload::CursorPosition(_))
    ));
    assert!(encoder.next_payload().is_none());
}

#[test]
fn a_keyframe_request_with_no_pending_frame_reuses_the_last_one() {
    // A viewer that lost synchronisation must not wait for the guest to
    // repaint something.
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    submit(&mut encoder, &frame(9));
    let _ = encoder.next_payload();

    encoder.request_keyframe();
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    decoder.apply_keyframe(&bytes).unwrap();
    assert_eq!(decoder.frame(), frame(9).as_slice());

    // And the request is spent, not standing.
    assert!(encoder.next_payload().is_none());
}

#[test]
fn the_protective_keyframe_arrives_on_its_interval() {
    let mut config = EncoderConfig::new(geometry());
    config.keyframe_interval = 3;
    let mut encoder = Encoder::new(config);

    let mut kinds = Vec::new();
    for index in 0..7u8 {
        let mut pixels = frame(0);
        pixels[0] = index;
        submit(&mut encoder, &pixels);
        match encoder.next_payload() {
            Some(Payload::Keyframe(_)) => kinds.push('K'),
            Some(Payload::TileDelta(_)) => kinds.push('D'),
            other => panic!("unexpected {other:?}"),
        }
    }

    assert_eq!(kinds, vec!['K', 'D', 'D', 'K', 'D', 'D', 'K']);
}

#[test]
fn an_interval_of_zero_never_forces_a_keyframe() {
    let mut config = EncoderConfig::new(geometry());
    config.keyframe_interval = 0;
    let mut encoder = Encoder::new(config);

    submit(&mut encoder, &frame(0));
    assert!(matches!(encoder.next_payload(), Some(Payload::Keyframe(_))));

    for index in 1..20u8 {
        let mut pixels = frame(0);
        pixels[0] = index;
        submit(&mut encoder, &pixels);
        assert!(matches!(
            encoder.next_payload(),
            Some(Payload::TileDelta(_))
        ));
    }
}

#[test]
fn the_latest_cursor_image_wins() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    for fill in [1u8, 2, 3] {
        encoder
            .submit_cursor_image(CursorImage {
                pixels: &vec![fill; 16 * 16 * 4],
                width: 16,
                height: 16,
                hotspot_x: 0,
                hotspot_y: 0,
            })
            .unwrap();
    }

    let Some(Payload::CursorImage(bytes)) = encoder.next_payload() else {
        panic!("a cursor image");
    };
    let image = Decoder::decode_cursor_image(bytes).unwrap();
    assert!(image.pixels.iter().all(|byte| *byte == 3));
    assert!(encoder.next_payload().is_none());
}
