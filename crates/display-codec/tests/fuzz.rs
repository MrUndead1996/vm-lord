//! Arbitrary bytes against everything that faces a peer.
//!
//! Deterministic rather than a `cargo-fuzz` target: this repository builds on
//! stable, and a fuzzer nobody runs finds nothing. The seed is fixed, so a
//! failure reproduces exactly; the corpus is the golden vectors, so mutations
//! start from bytes that mean something.
//!
//! Two invariants: nothing panics, and nothing a payload says can change how
//! large a frame is -- geometry comes from the session, never from bytes on
//! the wire.

use std::{fs, path::Path};

use vmlord_display_codec::{Decoder, Geometry, PixelFormat, TileSize};

fn geometry() -> Geometry {
    Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

/// xorshift64*, so the corpus is the same on every machine and every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn corpus() -> Vec<Vec<u8>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    ["keyframe.bin", "delta.bin", "cursor.bin"]
        .iter()
        .map(|name| fs::read(dir.join(name)).unwrap_or_else(|_| panic!("the {name} vector")))
        .collect()
}

/// One mutation of `seed`: a flipped byte, a truncation, an extension, or two
/// vectors spliced together.
fn mutate(rng: &mut Rng, corpus: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = corpus[rng.below(corpus.len())].clone();
    if bytes.is_empty() {
        return bytes;
    }

    match rng.next() % 4 {
        0 => {
            let at = rng.below(bytes.len());
            bytes[at] ^= (rng.next() % 256) as u8;
        }
        1 => {
            let keep = rng.below(bytes.len());
            bytes.truncate(keep);
        }
        2 => {
            let extra = rng.below(64);
            for _ in 0..extra {
                bytes.push((rng.next() % 256) as u8);
            }
        }
        _ => {
            let other = &corpus[rng.below(corpus.len())];
            let at = rng.below(bytes.len());
            bytes.truncate(at);
            bytes.extend_from_slice(other);
        }
    }

    bytes
}

/// A decoder holding the golden keyframe, so that deltas reach their bodies.
fn primed(corpus: &[Vec<u8>]) -> Decoder {
    let mut decoder = Decoder::new(geometry());
    decoder.apply_keyframe(&corpus[0]).unwrap();
    decoder
}

#[test]
fn no_mutation_of_a_valid_payload_panics() {
    let corpus = corpus();
    let mut rng = Rng(0x5EED_1116);

    for _ in 0..5_000 {
        let bytes = mutate(&mut rng, &corpus);

        // A fresh decoder, which has no base.
        let mut fresh = Decoder::new(geometry());
        let _ = fresh.apply_keyframe(&bytes);
        let _ = fresh.apply_delta(&bytes);

        // And one that does.
        let mut held = primed(&corpus);
        let _ = held.apply_keyframe(&bytes);
        let _ = held.apply_delta(&bytes);

        let _ = Decoder::decode_cursor_image(&bytes);
        let _ = Decoder::decode_cursor_position(&bytes);
    }
}

#[test]
fn a_decoder_that_accepted_a_payload_holds_a_frame_of_the_right_size() {
    // Nothing a mutation can say may change how large a frame is: the
    // geometry comes from the session, never from a payload.
    let corpus = corpus();
    let mut rng = Rng(0x1116_5EED);
    let expected = geometry().frame_bytes();

    for _ in 0..5_000 {
        let bytes = mutate(&mut rng, &corpus);
        let mut decoder = primed(&corpus);

        if decoder.apply_keyframe(&bytes).is_ok() || decoder.apply_delta(&bytes).is_ok() {
            assert_eq!(decoder.frame().len(), expected);
        }
    }
}

#[test]
fn a_truncated_prefix_of_every_vector_is_refused_cleanly() {
    let corpus = corpus();

    for bytes in &corpus {
        for length in 0..bytes.len() {
            let prefix = &bytes[..length];
            let mut decoder = Decoder::new(geometry());

            let _ = decoder.apply_keyframe(prefix);
            let _ = decoder.apply_delta(prefix);
            let _ = Decoder::decode_cursor_image(prefix);
            let _ = Decoder::decode_cursor_position(prefix);
        }
    }
}
