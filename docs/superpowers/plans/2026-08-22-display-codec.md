# Lossless desktop codec implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `vmlord-display-codec`, a dependency-free safe-Rust crate that
encodes a captured guest framebuffer into the opaque payloads of the display
protocol's frame channel and decodes them back, with a bounded queue that keeps
a slow viewer served with current state.

**Architecture:** A keyframe and a tile delta share one container format: an
8-byte header plus per-tile records encoded as `Raw`, `Zrle`, or (deltas only)
`XorZrle`, the shortest encoding winning per tile. The encoder holds a
latest-wins staging slot for captured frames and encodes at drain time, so its
reference frame is always the last payload the caller was handed. The decoder
validates every field against its geometry and returns errors, never panics.

**Tech Stack:** Rust 2024, no dependencies, `unsafe_code = "deny"` from the
workspace. Tests are plain `cargo test`; benchmarks run through `xtask`.

**Spec:** `docs/superpowers/specs/2026-08-22-display-codec-design.md`

## Global Constraints

- Crate `crates/display-codec`, package name `vmlord-display-codec`, library
  path `vmlord_display_codec`.
- **No dependencies at all** — neither runtime nor dev. Not `proptest`, not
  `criterion`, not `cargo-fuzz`, not `lz4`.
- `edition.workspace = true` (2024), `version/license.workspace = true`,
  `[lints] workspace = true`.
- Must build for `x86_64-unknown-linux-musl` (guest) and be listed in both
  `members` and `default-members` of the root `Cargo.toml` (host viewer).
- No `unsafe`, no `std::thread`, no timers, no I/O, no `panic!`/`unwrap`/
  `expect` in library code. Tests may `unwrap`.
- Every public item carries a doc comment; the workspace lints are strict about
  it. Comments explain *why*, matching the prose style of
  `crates/display-protocol`.
- Format version byte is `1`; methods are `Raw = 0`, `Zrle = 1`, `XorZrle = 2`.
- Commit subjects are `TASK-116: <comment>`.
- Run `cargo fmt` and `cargo clippy -p vmlord-display-codec --all-targets`
  clean before every commit.

---

### Task 1: Crate skeleton, errors and geometry

**Files:**
- Create: `crates/display-codec/Cargo.toml`
- Create: `crates/display-codec/src/lib.rs`
- Create: `crates/display-codec/src/error.rs`
- Create: `crates/display-codec/src/geometry.rs`
- Modify: `Cargo.toml` (workspace `members` and `default-members`)

**Interfaces:**
- Consumes: nothing.
- Produces: `CodecError`, `TileSize`, `PixelFormat`, `Geometry`, `Rect`, with
  `Geometry::{new, columns, rows, tile_count, tile, frame_bytes}` and
  `Rect { x, y, width, height }` (all `u32`).

- [ ] **Step 1: Write the failing test**

Append to `crates/display-codec/src/geometry.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_covers_a_size_that_is_not_a_multiple_of_the_tile() {
        let geometry = Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();

        assert_eq!(geometry.columns(), 4);
        assert_eq!(geometry.rows(), 3);
        assert_eq!(geometry.tile_count(), 12);
    }

    #[test]
    fn edge_tiles_are_clipped() {
        let geometry = Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();

        assert_eq!(geometry.tile(0), Some(Rect { x: 0, y: 0, width: 32, height: 32 }));
        // Last column: 100 - 96 = 4 wide. Last row: 70 - 64 = 6 high.
        assert_eq!(geometry.tile(3), Some(Rect { x: 96, y: 0, width: 4, height: 32 }));
        assert_eq!(geometry.tile(11), Some(Rect { x: 96, y: 64, width: 4, height: 6 }));
        assert_eq!(geometry.tile(12), None);
    }

    #[test]
    fn a_zero_or_oversized_dimension_is_refused() {
        assert!(matches!(
            Geometry::new(0, 720, TileSize::ThirtyTwo, PixelFormat::Bgra8888),
            Err(CodecError::Geometry { .. })
        ));
        assert!(matches!(
            Geometry::new(1280, MAX_DIMENSION + 1, TileSize::ThirtyTwo, PixelFormat::Bgra8888),
            Err(CodecError::Geometry { .. })
        ));
    }

    #[test]
    fn a_tile_size_is_one_of_three() {
        assert_eq!(TileSize::from_pixels(64).unwrap(), TileSize::SixtyFour);
        assert!(TileSize::from_pixels(48).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec`
Expected: FAIL — the package does not exist yet, then compile errors as the
items are filled in.

- [ ] **Step 3: Write minimal implementation**

`crates/display-codec/Cargo.toml`:

```toml
[package]
name = "vmlord-display-codec"
version.workspace = true
edition.workspace = true
license.workspace = true

# No dependencies, deliberately: this crate is linked into a Windows viewer and
# into a static musl guest binary built without a C toolchain, and its output
# must be byte-identical on both.
[dependencies]

[lints]
workspace = true
```

Root `Cargo.toml`: add `"crates/display-codec"` to `members` and to
`default-members`, in alphabetical position beside `crates/display-payload`.

`crates/display-codec/src/lib.rs`:

```rust
//! The lossless desktop codec of VMLord's display stack.
//!
//! Turns a captured guest framebuffer into the opaque payloads the display
//! protocol's frame channel carries, and turns them back into pixels. It
//! knows nothing of capture, of DRM, of sockets or of Windows: the guest
//! services and the host viewer are both built against it unchanged.
//!
//! What the record header already carries -- sequence, base, checksum,
//! generation -- is not repeated here, and geometry arrives out of band in a
//! `StreamConfig`, which is why [`Geometry`] is constructor input rather than
//! something a payload may change.

pub mod error;
pub mod geometry;

pub use error::CodecError;
pub use geometry::{Geometry, PixelFormat, Rect, TileSize, MAX_DIMENSION};
```

`crates/display-codec/src/error.rs`: an enum with the variants the spec lists
(`Geometry { detail: &'static str }`, `UnknownVersion { version: u8 }`,
`UnknownMethod { method: u8 }`, `GridMismatch { columns: u16, rows: u16 }`,
`TileIndexOutOfRange { index: u32 }`, `TileIndexNotIncreasing { index: u32 }`,
`Truncated`, `TrailingBytes`, `RunOverflow`, `NoBase`, `CursorTooLarge`,
`FrameSize { expected: usize, actual: usize }`), plus `impl Display` and
`impl std::error::Error`. Later tasks add no variants beyond these.

`crates/display-codec/src/geometry.rs`:

```rust
/// The largest width or height this codec will encode.
///
/// Not a display limit -- a bound that keeps a grid inside `u16` and every
/// pixel count inside `u32` without a checked multiplication in the hot path.
pub const MAX_DIMENSION: u32 = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileSize {
    Sixteen = 16,
    ThirtyTwo = 32,
    SixtyFour = 64,
}

impl TileSize {
    #[must_use]
    pub fn as_pixels(self) -> u32 {
        self as u32
    }

    /// # Errors
    ///
    /// [`CodecError::Geometry`] for any size the handshake cannot agree on.
    pub fn from_pixels(pixels: u32) -> Result<Self, CodecError> { /* 16/32/64 */ }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8888,
    Xrgb8888,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry { /* width, height, tile_size, pixel_format, columns, rows */ }
```

`Geometry::new` rejects a zero or above-`MAX_DIMENSION` dimension, precomputes
`columns` and `rows` as `width.div_ceil(tile)` and `height.div_ceil(tile)`, and
stores them. `tile(index)` returns the clipped `Rect`, `None` past the grid.
`frame_bytes()` is `width * height * 4` as `usize`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS, 4 tests.

Run: `cargo build -p vmlord-display-codec --target x86_64-unknown-linux-musl`
Expected: builds — the guest target must work from task one.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add Cargo.toml Cargo.lock crates/display-codec
git commit -m "TASK-116: Add the display codec crate with its geometry"
```

---

### Task 2: Varints and the ZRLE baseline

**Files:**
- Create: `crates/display-codec/src/varint.rs`
- Create: `crates/display-codec/src/zrle.rs`
- Modify: `crates/display-codec/src/lib.rs` (declare both modules)

**Interfaces:**
- Consumes: `CodecError` from Task 1.
- Produces:
  - `varint::write(out: &mut Vec<u8>, value: u32)`
  - `varint::read(bytes: &[u8]) -> Result<(u32, usize), CodecError>` — value and
    bytes consumed
  - `zrle::encode(pixels: &[u32], out: &mut Vec<u8>)`
  - `zrle::decode(bytes: &[u8], out: &mut [u32]) -> Result<(), CodecError>`
  - `zrle::MAX_RUN: usize = 65_536`

  Both modules are `pub(crate)`.

- [ ] **Step 1: Write the failing test**

In `crates/display-codec/src/varint.rs`:

```rust
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
```

In `crates/display-codec/src/zrle.rs`:

```rust
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
        assert_eq!(round_trip(&[0x00FF_00FFu32; 1024]), 3);
    }

    #[test]
    fn a_noisy_tile_costs_its_pixels_and_a_little() {
        let pixels: Vec<u32> = (0..1024u32).map(|index| index.wrapping_mul(2_654_435_761)).collect();
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
        assert!(matches!(decode(&bytes, &mut back), Err(CodecError::Truncated)));
    }

    #[test]
    fn a_run_past_the_end_of_the_tile_is_an_error() {
        let mut bytes = Vec::new();
        encode(&[1u32; 64], &mut bytes);

        let mut back = [0u32; 32];
        assert!(matches!(decode(&bytes, &mut back), Err(CodecError::RunOverflow)));
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = Vec::new();
        encode(&[1u32; 8], &mut bytes);
        bytes.push(0);

        let mut back = [0u32; 8];
        assert!(matches!(decode(&bytes, &mut back), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn an_unfilled_tile_is_truncated() {
        let mut bytes = Vec::new();
        encode(&[1u32; 8], &mut bytes);

        let mut back = [0u32; 16];
        assert!(matches!(decode(&bytes, &mut back), Err(CodecError::Truncated)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec`
Expected: FAIL — `encode`/`decode`/`write`/`read` are not defined.

- [ ] **Step 3: Write minimal implementation**

`varint.rs` is LEB128 over `u32`: seven bits per byte, high bit as
continuation, at most five bytes. A sixth continuation byte or a run off the
end of the slice is `CodecError::Truncated`.

`zrle.rs`:

```rust
//! The baseline compressor, over 32-bit pixels rather than bytes.
//!
//! A desktop repeats whole pixels, not bytes, and a byte-oriented coder would
//! have to rediscover that four times per pixel. Under `XorZrle` a tile that
//! changed in one corner is mostly zeros, which is the shape this is best at.
//!
//! A control varint precedes each run: the low bit picks literal (0) or
//! repeat (1), the rest is the count minus one.

/// The longest run a single control may describe.
pub(crate) const MAX_RUN: usize = 65_536;

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

        // A literal run reaches to the next pixel that repeats, because
        // breaking out of a literal costs a control of its own.
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
```

`decode` mirrors it: read a control, check the count against the space left in
`out` (`RunOverflow` if it does not fit), fill, and at the end require both the
tile full (`Truncated` otherwise) and the input exhausted (`TrailingBytes`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS, all varint and zrle tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Add the ZRLE baseline and its varints"
```

---

### Task 3: The tile container framing

**Files:**
- Create: `crates/display-codec/src/container.rs`
- Modify: `crates/display-codec/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: `Geometry`, `CodecError`, `varint`.
- Produces (all `pub(crate)`):
  - `FORMAT_VERSION: u8 = 1`, `HEADER_LEN: usize = 8`, `FLAG_KEYFRAME: u8 = 1`
  - `Method { Raw = 0, Zrle = 1, XorZrle = 2 }` with `as_byte`/`from_byte`
  - `write_header(out: &mut Vec<u8>, keyframe: bool, geometry: &Geometry)`
  - `read_header(bytes: &[u8], geometry: &Geometry) -> Result<(bool, &[u8]), CodecError>`
    — the keyframe flag and the remaining body

- [ ] **Step 1: Write the failing test**

In `crates/display-codec/src/container.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{PixelFormat, TileSize};

    fn geometry() -> Geometry {
        Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
    }

    #[test]
    fn a_header_round_trips_and_names_a_keyframe() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, true, &geometry());
        bytes.push(0xAB);

        assert_eq!(bytes.len(), HEADER_LEN + 1);
        let (keyframe, body) = read_header(&bytes, &geometry()).unwrap();
        assert!(keyframe);
        assert_eq!(body, &[0xAB]);
    }

    #[test]
    fn a_grid_from_another_geometry_is_refused() {
        let other = Geometry::new(1280, 720, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();
        let mut bytes = Vec::new();
        write_header(&mut bytes, false, &other);

        assert!(matches!(
            read_header(&bytes, &geometry()),
            Err(CodecError::GridMismatch { .. })
        ));
    }

    #[test]
    fn a_future_version_is_refused() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, false, &geometry());
        bytes[0] = 2;

        assert!(matches!(
            read_header(&bytes, &geometry()),
            Err(CodecError::UnknownVersion { version: 2 })
        ));
    }

    #[test]
    fn reserved_bytes_must_be_zero() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, false, &geometry());
        bytes[6] = 1;

        assert!(matches!(read_header(&bytes, &geometry()), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn a_short_header_is_truncated() {
        assert!(matches!(read_header(&[1, 0, 4], &geometry()), Err(CodecError::Truncated)));
    }

    #[test]
    fn methods_map_to_their_bytes() {
        assert_eq!(Method::from_byte(2).unwrap(), Method::XorZrle);
        assert!(matches!(Method::from_byte(3), Err(CodecError::UnknownMethod { method: 3 })));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --lib container`
Expected: FAIL — module items undefined.

- [ ] **Step 3: Write minimal implementation**

```rust
//! The bytes a keyframe and a tile delta share.
//!
//! Eight bytes of header, then tile records. The grid is derivable from the
//! session's `StreamConfig` and is repeated here on purpose: four bytes turn a
//! `StreamConfig`/frame mismatch from a silently wrong picture into a named
//! error. The two reserved bytes keep the header a multiple of four and are
//! checked as zero, so a later version cannot quietly reuse them.

pub(crate) const FORMAT_VERSION: u8 = 1;
pub(crate) const HEADER_LEN: usize = 8;
pub(crate) const FLAG_KEYFRAME: u8 = 1;
```

`write_header` writes version, flags, `columns` and `rows` as little-endian
`u16`, and two zero bytes. `read_header` checks the length, the version, the
reserved bytes (`TrailingBytes`) and the grid against `geometry`
(`GridMismatch`), returning the flag and `&bytes[HEADER_LEN..]`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Add the tile container header"
```

---

### Task 4: Keyframes — encoder and decoder

**Files:**
- Create: `crates/display-codec/src/encoder.rs`
- Create: `crates/display-codec/src/decoder.rs`
- Modify: `crates/display-codec/src/lib.rs` (declare and re-export)

**Interfaces:**
- Consumes: `container`, `zrle`, `varint`, `Geometry`, `CodecError`.
- Produces:
  - `Frame<'a> { pixels: &'a [u8], stride: usize }`
  - `EncoderConfig { geometry: Geometry, keyframe_interval: u32 }` with
    `EncoderConfig::new(geometry) -> Self` defaulting the interval to 300
  - `Encoder::new(EncoderConfig) -> Encoder`
  - `Encoder::submit(&mut self, frame: Frame<'_>, damage: Option<&[Rect]>) -> Result<(), CodecError>`
  - `Encoder::next_payload(&mut self) -> Option<Payload<'_>>`
  - `Payload<'a> { Keyframe(&'a [u8]), TileDelta(&'a [u8]), CursorImage(&'a [u8]), CursorPosition(&'a [u8]) }`
  - `Decoder::new(Geometry) -> Decoder`,
    `Decoder::apply_keyframe(&mut self, &[u8]) -> Result<&[Rect], CodecError>`,
    `Decoder::frame(&self) -> &[u8]`

  In this task `submit` stages the frame and `next_payload` always produces a
  keyframe; deltas arrive in Task 5. `damage` is accepted and ignored here.

- [ ] **Step 1: Write the failing test**

Create `crates/display-codec/tests/keyframe.rs`:

```rust
//! A keyframe carries a whole frame and needs nothing before it.

use vmlord_display_codec::{
    Decoder, Encoder, EncoderConfig, Frame, Geometry, PixelFormat, Payload, TileSize,
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
    encoder.submit(Frame { pixels: &pixels, stride: 400 }, None).unwrap();

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
    encoder.submit(Frame { pixels: &pixels, stride: 400 }, None).unwrap();

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
    let mut pixels = vec![0u8; (2560 * 1440 * 4) as usize];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for chunk in pixels.chunks_exact_mut(4) {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        chunk.copy_from_slice(&(state as u32).to_le_bytes());
    }

    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    encoder.submit(Frame { pixels: &pixels, stride: 2560 * 4 }, None).unwrap();

    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    assert!(bytes.len() <= pixels.len() + 64 * 1024, "{} bytes", bytes.len());
}

#[test]
fn a_frame_of_the_wrong_size_is_refused() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    let short = vec![0u8; 10];

    assert!(encoder.submit(Frame { pixels: &short, stride: 400 }, None).is_err());
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
    encoder.submit(Frame { pixels: &padded, stride }, None).unwrap();
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --test keyframe`
Expected: FAIL — `Encoder`, `Decoder`, `Payload` are not defined.

- [ ] **Step 3: Write minimal implementation**

Both sides hold the frame as `Vec<u32>` in raster order — the codec's unit is a
pixel, and gathering a tile from a `u8` slice on every comparison would pay for
the byte view four times over. `submit` converts once, checking that
`pixels.len() >= stride * height` and `stride >= width * 4`, else
`CodecError::FrameSize`.

Encoding a tile, in `encoder.rs`:

```rust
/// The shortest encoding of one tile, ties going to the lower method.
///
/// Evaluating every candidate rather than guessing is what makes the output
/// deterministic: the same pixels produce the same bytes on every machine,
/// which is what the golden vectors and the decoder's tests rest on.
fn encode_tile(&mut self, tile: &[u32], previous: Option<&[u32]>) -> Method { /* ... */ }
```

For a keyframe the candidates are `Raw` (the tile's `u32`s little-endian, no
length field) and `Zrle` (a varint length then the stream). `write_header`,
then every tile in raster order with no index.

`decoder.rs` reads the header, requires the keyframe flag for
`apply_keyframe`, then reads exactly `tile_count` tiles, scattering each into
its `Rect` of the framebuffer and recording that `Rect` in a reusable
`Vec<Rect>` returned as `&[Rect]`. Any byte left over is `TrailingBytes`.
`frame()` returns the framebuffer as `&[u8]` (kept as a parallel `Vec<u8>`
written when tiles are scattered, so the viewer needs no conversion).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS, including the record-slack test.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Encode and decode keyframes"
```

---

### Task 5: Tile deltas, XOR and damage hints

**Files:**
- Modify: `crates/display-codec/src/encoder.rs`
- Modify: `crates/display-codec/src/decoder.rs`
- Create: `crates/display-codec/tests/delta.rs`

**Interfaces:**
- Consumes: everything from Task 4.
- Produces: `Decoder::apply_delta(&mut self, &[u8]) -> Result<&[Rect], CodecError>`;
  `Encoder::next_payload` now yields `Payload::TileDelta` after the first
  keyframe, and yields `None` when nothing changed.

- [ ] **Step 1: Write the failing test**

Create `crates/display-codec/tests/delta.rs`:

```rust
//! What a delta carries, and what it must refuse.

use vmlord_display_codec::{
    CodecError, Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, Rect,
    TileSize,
};

fn geometry() -> Geometry {
    Geometry::new(128, 96, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

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
        self.encoder.submit(Frame { pixels, stride: 128 * 4 }, damage).unwrap();
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

        assert_eq!(self.decoder.frame(), pixels);
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
    assert_eq!(damage, vec![Rect { x: 32, y: 32, width: 32, height: 32 }]);
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

    let mut next = blank();
    for byte in next.iter_mut() {
        *byte = 0x40;
    }

    // The hint covers one tile, so only that tile may travel -- and the
    // decoder must match the encoder's belief, not the submitted frame.
    let hint = [Rect { x: 0, y: 0, width: 32, height: 32 }];
    pair.encoder.submit(Frame { pixels: &next, stride: 512 }, Some(&hint)).unwrap();
    let Some(Payload::TileDelta(bytes)) = pair.encoder.next_payload() else {
        panic!("a delta");
    };
    let bytes = bytes.to_vec();
    let damage = pair.decoder.apply_delta(&bytes).unwrap().to_vec();

    assert_eq!(damage, vec![Rect { x: 0, y: 0, width: 32, height: 32 }]);
}

#[test]
fn a_hint_that_misses_a_change_does_not_desynchronise_the_stream() {
    // An under-reporting capture backend loses pixels until the next
    // keyframe, which is a bug over there. What must hold here is that the
    // encoder's reference and the decoder's frame stay identical.
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let mut next = blank();
    next[(70 * 128 + 70) * 4] = 0xFF;
    let hint = [Rect { x: 0, y: 0, width: 32, height: 32 }];
    pair.encoder.submit(Frame { pixels: &next, stride: 512 }, Some(&hint)).unwrap();
    assert!(pair.encoder.next_payload().is_none());

    // A later hint that does cover the tile brings both sides back together.
    let hint = [Rect { x: 64, y: 64, width: 32, height: 32 }];
    pair.encoder.submit(Frame { pixels: &next, stride: 512 }, Some(&hint)).unwrap();
    let Some(Payload::TileDelta(bytes)) = pair.encoder.next_payload() else {
        panic!("a delta");
    };
    let bytes = bytes.to_vec();
    pair.decoder.apply_delta(&bytes).unwrap();
    assert_eq!(pair.decoder.frame(), next.as_slice());
}

#[test]
fn a_hint_outside_the_frame_is_clipped_not_refused() {
    let mut pair = Pair::new();
    pair.round(&blank(), None).unwrap();

    let mut next = blank();
    next[0] = 0xFF;
    let hint = [Rect { x: 0, y: 0, width: 4096, height: 4096 }];
    pair.encoder.submit(Frame { pixels: &next, stride: 512 }, Some(&hint)).unwrap();
    assert!(pair.encoder.next_payload().is_some());
}

#[test]
fn a_delta_before_a_keyframe_has_no_base() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder.submit(Frame { pixels: &blank(), stride: 512 }, None).unwrap();
    let Some(Payload::Keyframe(_)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let mut next = blank();
    next[0] = 0xFF;
    encoder.submit(Frame { pixels: &next, stride: 512 }, None).unwrap();
    let Some(Payload::TileDelta(bytes)) = encoder.next_payload() else {
        panic!("a delta");
    };
    let bytes = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    assert!(matches!(decoder.apply_delta(&bytes), Err(CodecError::NoBase)));
}

#[test]
fn a_keyframe_applied_as_a_delta_is_refused_and_the_reverse_too() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder.submit(Frame { pixels: &blank(), stride: 512 }, None).unwrap();
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let keyframe = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    assert!(decoder.apply_delta(&keyframe).is_err());
    decoder.apply_keyframe(&keyframe).unwrap();

    let mut next = blank();
    next[0] = 0xFF;
    encoder.submit(Frame { pixels: &next, stride: 512 }, None).unwrap();
    let Some(Payload::TileDelta(bytes)) = encoder.next_payload() else {
        panic!("a delta");
    };
    let delta = bytes.to_vec();
    assert!(decoder.apply_keyframe(&delta).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --test delta`
Expected: FAIL — `apply_delta` undefined, `next_payload` always keyframes.

- [ ] **Step 3: Write minimal implementation**

Encoder: after the reference frame exists, walk the tiles the hint selects (all
of them when `damage` is `None`), comparing each against the reference. A hint
rectangle is clipped to the frame and expanded to the tiles it touches; a hint
entirely outside the frame selects nothing. Changed tiles are written in
increasing index order as `varint(index)`, method byte, then the data, with
candidates `Raw`, `Zrle` and `XorZrle` — the last being `zrle::encode` over
`current XOR reference`. A delta with no tiles is not emitted at all, and the
reference is not advanced by tiles that were never compared.

Decoder: `apply_delta` requires a prior keyframe (`NoBase`) and the keyframe
flag clear; it reads `(index, method)` pairs until the body is exhausted,
checking `index < tile_count` (`TileIndexOutOfRange`) and strictly increasing
(`TileIndexNotIncreasing`). `XorZrle` decodes into scratch and XORs onto the
tile held.

A keyframe flag that contradicts the call is its own condition — neither
`NoBase` nor `GridMismatch` names it — so **add `CodecError::WrongPayloadKind`**
to `error.rs` in this task and return it from both `apply_keyframe` given a
delta and `apply_delta` given a keyframe. `NoBase` stays what it is: a delta
that is well-formed but has nothing to build on.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Encode and decode tile deltas"
```

---

### Task 6: Scenes and the round-trip property

**Files:**
- Create: `crates/display-codec/src/scenes.rs`
- Create: `crates/display-codec/tests/roundtrip.rs`
- Modify: `crates/display-codec/src/lib.rs` (declare and re-export `scenes`)

**Interfaces:**
- Consumes: `Geometry`, `Encoder`, `Decoder`.
- Produces:
  - `scenes::Scene` — a trait-free enum: `Scene { StaticDesktop, Typing, Scrolling, MovingWindow, FullscreenVideo }`
    with `Scene::ALL: [Scene; 5]`, `Scene::name(self) -> &'static str`
  - `scenes::Generator::new(scene: Scene, geometry: Geometry, seed: u64) -> Generator`
  - `Generator::next_frame(&mut self) -> &[u8]` — a full frame, `width * 4` stride
  - `Generator::damage(&self) -> &[Rect]` — the rectangles the scene actually
    touched in the last frame, for tests and benchmarks that exercise hints

- [ ] **Step 1: Write the failing test**

Create `crates/display-codec/tests/roundtrip.rs`:

```rust
//! `decode(encode) == current`, over every scene and every geometry that the
//! session may agree on.

use vmlord_display_codec::{
    scenes::{Generator, Scene},
    Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, TileSize,
};

const GEOMETRIES: [(u32, u32); 4] = [
    (64, 64),      // exactly one tile at 64
    (1280, 720),   // a multiple of 16 but not of 64 in height
    (100, 70),     // clipped on both edges at every tile size
    (2560, 1440),  // the largest mode the MVP offers
];

fn drive(scene: Scene, geometry: Geometry, hints: bool, frames: usize) {
    let mut generator = Generator::new(scene, geometry, 0x5EED);
    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    let mut decoder = Decoder::new(geometry);
    let stride = (geometry.width() * 4) as usize;

    for _ in 0..frames {
        let pixels = generator.next_frame().to_vec();
        let damage = generator.damage().to_vec();
        encoder
            .submit(Frame { pixels: &pixels, stride }, hints.then_some(damage.as_slice()))
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

        assert_eq!(decoder.frame(), pixels.as_slice(), "{} at {:?}", scene.name(), geometry);
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --test roundtrip`
Expected: FAIL — `scenes` does not exist; `Geometry::width()` may need adding.

- [ ] **Step 3: Write minimal implementation**

`scenes.rs` is an ordinary public module, not a feature: the property tests,
the golden vectors and the benchmark need the same deterministic workloads, and
a guest binary that calls none of them drops it at link time.

One xorshift64\* generator, seeded per `Generator`, exactly as
`crates/display-protocol/tests/fuzz.rs` does it. The scenes:

- `StaticDesktop` — a fixed background of flat panels; after the first frame
  nothing changes.
- `Typing` — the background plus one 8x16 block per frame appended along a
  line, wrapping to a new line at the edge.
- `Scrolling` — the background shifted up by 40 pixels per frame, with a new
  noisy row entering at the bottom.
- `MovingWindow` — a 400x300 filled rectangle moving diagonally by 7 pixels a
  frame over the background.
- `FullscreenVideo` — every pixel redrawn from the generator each frame.

`damage()` reports what the scene wrote, which for `FullscreenVideo` is the
whole frame and for `StaticDesktop` is empty after the first frame.

Add `Geometry::width()`, `height()`, `tile_size()` and `pixel_format()`
accessors if the fields are private.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec --test roundtrip`
Expected: PASS. This test is the slow one; it should still finish well inside a
minute in debug.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Add the benchmark scenes and the round-trip property"
```

---

### Task 7: The cursor stream

**Files:**
- Create: `crates/display-codec/src/cursor.rs`
- Modify: `crates/display-codec/src/encoder.rs`, `src/decoder.rs`, `src/lib.rs`
- Create: `crates/display-codec/tests/cursor.rs`

**Interfaces:**
- Consumes: `zrle`, `varint`, `CodecError`.
- Produces:
  - `CursorImage<'a> { pixels: &'a [u8], width: u32, height: u32, hotspot_x: u32, hotspot_y: u32 }`
  - `OwnedCursorImage { pixels: Vec<u8>, width: u32, height: u32, hotspot_x: u32, hotspot_y: u32 }`
  - `CursorPosition { x: u32, y: u32, visible: bool }`
  - `MAX_CURSOR_DIMENSION: u32 = 256`
  - `Encoder::submit_cursor_image`, `Encoder::submit_cursor_position`
  - `Decoder::decode_cursor_image(&[u8]) -> Result<OwnedCursorImage, CodecError>`,
    `Decoder::decode_cursor_position(&[u8]) -> Result<CursorPosition, CodecError>`
    (associated functions — the cursor keeps no decoder state)

- [ ] **Step 1: Write the failing test**

Create `crates/display-codec/tests/cursor.rs`:

```rust
//! The cursor is its own stream, with its own state and its own limits.

use vmlord_display_codec::{
    CodecError, CursorImage, CursorPosition, Decoder, Encoder, EncoderConfig, Geometry,
    Payload, PixelFormat, TileSize, MAX_CURSOR_DIMENSION,
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
fn a_cursor_position_is_six_bytes() {
    let mut encoder = encoder();
    encoder.submit_cursor_position(CursorPosition { x: 700, y: 400, visible: false });

    let Some(Payload::CursorPosition(bytes)) = encoder.next_payload() else {
        panic!("a cursor position");
    };
    assert_eq!(bytes.len(), 6);

    let position = Decoder::decode_cursor_position(bytes).unwrap();
    assert_eq!(position, CursorPosition { x: 700, y: 400, visible: false });
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

    assert!(encoder
        .submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 32,
            height: 32,
            hotspot_x: 0,
            hotspot_y: 0,
        })
        .is_err());
}

#[test]
fn a_hotspot_outside_the_image_is_refused() {
    let mut encoder = encoder();
    let pixels = vec![0u8; 32 * 32 * 4];

    assert!(encoder
        .submit_cursor_image(CursorImage {
            pixels: &pixels,
            width: 32,
            height: 32,
            hotspot_x: 32,
            hotspot_y: 0,
        })
        .is_err());
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --test cursor`
Expected: FAIL — cursor items undefined.

- [ ] **Step 3: Write minimal implementation**

`cursor.rs` writes the ten-byte image header of the spec (version, method,
width, height, hotspot x, hotspot y as little-endian `u16`) followed by `Raw`
or `Zrle` data, whichever is shorter, and the fixed six-byte position record
(version, visible flag, `x`, `y`). Both directions check the dimension cap and
the hotspot; the decoder additionally checks that the decoded pixel count is
exactly `width * height`.

In the encoder these are two latest-wins slots, replaced rather than queued.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Add the cursor stream"
```

---

### Task 8: The bounded queue and the keyframe policy

**Files:**
- Create: `crates/display-codec/src/queue.rs`
- Modify: `crates/display-codec/src/encoder.rs` (delegate staging to `queue`)
- Create: `crates/display-codec/tests/queue.rs`

**Interfaces:**
- Consumes: `Encoder`, `Payload`, `Geometry`.
- Produces: `Encoder::request_keyframe(&mut self)`; the documented drain order;
  `EncoderConfig::keyframe_interval` honoured. `queue::Staging` is
  `pub(crate)`: two frame buffers swapped on submit, plus the cursor slots and
  the keyframe-request flag.

- [ ] **Step 1: Write the failing test**

Create `crates/display-codec/tests/queue.rs`:

```rust
//! The queue keeps current state and drops what is stale -- and what may be
//! dropped is a *captured* frame, never an encoded one.

use vmlord_display_codec::{
    CursorImage, CursorPosition, Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload,
    PixelFormat, TileSize,
};

fn geometry() -> Geometry {
    Geometry::new(128, 96, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
}

fn frame(fill: u8) -> Vec<u8> {
    vec![fill; 128 * 96 * 4]
}

#[test]
fn a_frame_submitted_twice_encodes_only_the_newer_one() {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    let mut decoder = Decoder::new(geometry());

    encoder.submit(Frame { pixels: &frame(1), stride: 512 }, None).unwrap();
    encoder.submit(Frame { pixels: &frame(2), stride: 512 }, None).unwrap();

    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();
    decoder.apply_keyframe(&bytes).unwrap();

    assert_eq!(decoder.frame(), frame(2).as_slice());
    assert!(encoder.next_payload().is_none(), "the older frame is gone, not queued");
}

#[test]
fn the_reference_advances_only_when_a_payload_is_taken() {
    // The invariant the whole design rests on: what the encoder believes the
    // far side holds is the last payload the caller was handed.
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    let mut decoder = Decoder::new(geometry());

    encoder.submit(Frame { pixels: &frame(1), stride: 512 }, None).unwrap();
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();
    decoder.apply_keyframe(&bytes).unwrap();

    // Three captures arrive while the socket is busy; only the last survives.
    encoder.submit(Frame { pixels: &frame(2), stride: 512 }, None).unwrap();
    encoder.submit(Frame { pixels: &frame(3), stride: 512 }, None).unwrap();
    encoder.submit(Frame { pixels: &frame(4), stride: 512 }, None).unwrap();

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
    encoder.submit(Frame { pixels: &frame(1), stride: 512 }, None).unwrap();
    let _ = encoder.next_payload();

    encoder.submit(Frame { pixels: &frame(2), stride: 512 }, None).unwrap();
    encoder.submit_cursor_position(CursorPosition { x: 1, y: 1, visible: true });
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

    // Frame first -- and a keyframe, because one was asked for.
    assert!(matches!(encoder.next_payload(), Some(Payload::Keyframe(_))));
    assert!(matches!(encoder.next_payload(), Some(Payload::CursorImage(_))));
    assert!(matches!(encoder.next_payload(), Some(Payload::CursorPosition(_))));
    assert!(encoder.next_payload().is_none());
}

#[test]
fn a_keyframe_request_with_no_pending_frame_reuses_the_last_one() {
    // A viewer that lost synchronisation must not wait for the guest to
    // repaint something.
    let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
    encoder.submit(Frame { pixels: &frame(9), stride: 512 }, None).unwrap();
    let _ = encoder.next_payload();

    encoder.request_keyframe();
    let Some(Payload::Keyframe(bytes)) = encoder.next_payload() else {
        panic!("a keyframe");
    };
    let bytes = bytes.to_vec();

    let mut decoder = Decoder::new(geometry());
    decoder.apply_keyframe(&bytes).unwrap();
    assert_eq!(decoder.frame(), frame(9).as_slice());
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
        encoder.submit(Frame { pixels: &pixels, stride: 512 }, None).unwrap();
        match encoder.next_payload() {
            Some(Payload::Keyframe(_)) => kinds.push('K'),
            Some(Payload::TileDelta(_)) => kinds.push('D'),
            other => panic!("unexpected {other:?}"),
        }
    }

    assert_eq!(kinds, vec!['K', 'D', 'D', 'K', 'D', 'D', 'K']);
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --test queue`
Expected: FAIL — `request_keyframe` undefined and the interval unhonoured.

- [ ] **Step 3: Write minimal implementation**

`queue.rs` holds the staging state and documents the reasoning:

```rust
//! The bounded queue, which sits *before* the encoder.
//!
//! Discarding an encoded delta would be silent corruption: the next delta
//! would be encoded against a frame the viewer never received, applied to the
//! wrong base, and nothing anywhere would detect it. So what a slow socket
//! discards is a captured frame, and encoding happens when the caller asks for
//! a payload -- which makes the encoder's reference frame, by construction,
//! the last payload the caller was handed.
```

`Staging` owns two `Vec<u32>` frame buffers (`pending` and a spare to swap
into), a `pending: bool`, the two cursor slots and `keyframe_requested`.
`Encoder::next_payload` drains in order: a pending frame (as a keyframe if
requested, if the interval is due, or if there is no reference yet — otherwise
as a delta), then a keyframe with no pending frame if one was requested and a
reference exists, then the cursor image, then the cursor position.

The interval counts encoded frames since the last keyframe.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Bound the frame queue and set the keyframe policy"
```

---

### Task 9: Golden vectors, the malformed corpus and the fuzz test

**Files:**
- Create: `crates/display-codec/tests/golden.rs`
- Create: `crates/display-codec/tests/golden/` (generated `.bin` files)
- Create: `crates/display-codec/tests/malformed.rs`
- Create: `crates/display-codec/tests/fuzz.rs`

**Interfaces:**
- Consumes: the whole public API.
- Produces: `tests/golden/keyframe.bin`, `tests/golden/delta.bin`,
  `tests/golden/cursor.bin` — the corpus the fuzz test mutates.

- [ ] **Step 1: Write the failing test**

`tests/golden.rs`, modelled on `crates/display-protocol/tests/golden.rs`:

```rust
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
    scenes::{Generator, Scene},
    CursorImage, Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat,
    TileSize,
};

fn compare(name: &str, bytes: &[u8]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    if env::var_os("VMLORD_REFRESH_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        return;
    }

    let expected = fs::read(&path).unwrap_or_else(|_| panic!("missing vector {name}"));
    assert_eq!(bytes, expected.as_slice(), "{name} changed");
}
```

Three tests build a fixed keyframe, a fixed delta and a fixed cursor image from
`Scene::MovingWindow` at 320x200, tile 32, seed 7, and call `compare`. Each
also decodes what it wrote, so a refreshed vector is a valid one.

`tests/malformed.rs` — one test per `CodecError` variant reachable from a
payload, each asserting the exact variant: a truncated container, a body with
trailing bytes, version 2, a grid from another geometry, non-zero reserved
bytes, an unknown method byte, a tile index past the grid, indices out of
order, a ZRLE run past the tile, a delta with no base, a keyframe applied as a
delta, an oversized cursor and a truncated cursor.

`tests/fuzz.rs` — the display protocol's shape:

```rust
//! Arbitrary bytes against everything that faces a peer.
//!
//! Deterministic rather than a `cargo-fuzz` target: this repository builds on
//! stable, and a fuzzer nobody runs finds nothing. The seed is fixed, so a
//! failure reproduces exactly; the corpus is the golden vectors, so mutations
//! start from bytes that mean something.
//!
//! Two invariants: nothing panics, and no decoder reports success on a payload
//! it did not fully consume.
```

It mutates the three vectors — flipping bytes, truncating, extending, splicing
— and feeds each result to `apply_keyframe`, `apply_delta`,
`decode_cursor_image` and `decode_cursor_position` on a fresh decoder and on a
decoder that already holds a keyframe, asserting only that the call returns.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-codec --test golden`
Expected: FAIL — "missing vector keyframe.bin".

- [ ] **Step 3: Write minimal implementation**

Generate the vectors, then read them back under the normal run:

```bash
VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-codec --test golden
```

Fix whatever `malformed` and `fuzz` turn up in the library — a panic found here
is a decoder bug, not a test to weaken.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-codec`
Expected: PASS, with the three `.bin` files present and tracked.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p vmlord-display-codec --all-targets
git add crates/display-codec
git commit -m "TASK-116: Pin the codec bytes and fuzz the decoders"
```

---

### Task 10: The benchmark

**Files:**
- Create: `crates/xtask/src/display_bench.rs`
- Modify: `crates/xtask/src/main.rs` (declare the module, dispatch the task)
- Modify: `crates/xtask/Cargo.toml` (depend on `vmlord-display-codec`)
- Modify: `.cargo/config.toml` (the alias)

**Interfaces:**
- Consumes: `scenes`, `Encoder`, `Decoder`, `Payload`.
- Produces: `cargo display-bench`, optionally `cargo display-bench --frames N`
  and `--tile 16|32|64`.

- [ ] **Step 1: Write the failing test**

The deliverable is a program, and its correctness check is the round trip it
performs. Add to `crates/xtask/src/display_bench.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_of_every_scene_round_trips_and_reports() {
        let report = measure(Scene::Typing, geometry(TileSize::ThirtyTwo), 4).unwrap();

        assert_eq!(report.frames, 4);
        assert!(report.keyframes >= 1);
        assert!(report.mean_bytes > 0.0);
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        assert!(parse(["--nope".to_owned()].into_iter()).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xtask`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write minimal implementation**

`crates/xtask/Cargo.toml` gains
`vmlord-display-codec = { path = "../display-codec" }`.

`.cargo/config.toml`:

```toml
# The codec benchmark. Release, and in xtask's own process: a debug build
# measures the wrong thing, and a nested `cargo run` would only hide that.
display-bench = ["run", "--release", "-p", "xtask", "--", "display-bench"]
```

`main.rs` dispatches `Some("display-bench") => display_bench::run(env::args().skip(2))`,
in the existing match, and declares `mod display_bench;`.

`display_bench.rs` holds:

```rust
/// What one scene came to.
struct Report {
    scene: &'static str,
    frames: u32,
    keyframes: u32,
    mean_bytes: f64,
    worst_bytes: u64,
    ratio: f64,
    mean_encode_ms: f64,
    worst_encode_ms: f64,
    mean_decode_ms: f64,
}

fn measure(scene: Scene, geometry: Geometry, frames: u32) -> Result<Report, String>;
```

`measure` drives the encoder and decoder exactly as `tests/roundtrip.rs` does,
timing with `std::time::Instant`, asserting `decoder.frame()` against the
submitted frame each iteration and returning an `Err` rather than panicking if
they diverge. `run` parses `--frames` (default 300) and `--tile` (default 32),
measures all five scenes at 1920x1080, and prints a fixed-width table plus a
line naming the geometry and frame count.

The table is stdout, not a stored baseline: this benchmark exists to answer the
LZ4 question and to catch an obviously wrong default tile size, not to gate CI
on a number that depends on the machine.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p xtask`
Expected: PASS.

Run: `cargo display-bench --frames 60`
Expected: a table of five scenes with plausible numbers — a static desktop near
zero bytes per frame, fullscreen video the largest.

- [ ] **Step 5: Commit**

```bash
cargo fmt
cargo clippy -p xtask --all-targets
git add .cargo/config.toml crates/xtask Cargo.lock
git commit -m "TASK-116: Add the display codec benchmark"
```

---

### Task 11: Documentation and the benchmark's verdict

**Files:**
- Modify: `ARCHITECTURE.md` (a new section after "The display protocol")
- Modify: `AGENTS.md` (the `Commands` list gains `cargo display-bench`)

**Interfaces:**
- Consumes: the benchmark numbers from Task 10.
- Produces: nothing code depends on.

- [ ] **Step 1: Collect the evidence**

Run: `cargo display-bench --frames 300` at tile 16, 32 and 64:

```bash
cargo display-bench --frames 300 --tile 16
cargo display-bench --frames 300 --tile 32
cargo display-bench --frames 300 --tile 64
```

Keep the three tables; they are what the documentation's claims rest on.

- [ ] **Step 2: Write the architecture section**

Add "### The desktop codec" to `ARCHITECTURE.md`, after the display protocol
section, in that document's voice — prose, no bullet lists of API names.
It must state:

- what the crate is and what it deliberately does not know;
- the container: one format for keyframes and deltas, tiles in raster order
  without indices on a keyframe, increasing indices on a delta;
- why `Raw` carries no length — the 64 KiB of record slack against 14400 tiles
  at tile size 16;
- why ZRLE works on pixels rather than bytes, and what XOR buys;
- that the encoder is deterministic by evaluating every candidate;
- **why the queue precedes the encoder**, with the silent-corruption argument;
- the keyframe policy: first frame, request, interval;
- that damage is a hint and comparison is still performed;
- what the benchmark measured, with the real numbers, and the resulting
  decision on LZ4 and on the default tile size.

- [ ] **Step 3: Update AGENTS.md**

Add to the `Commands` list:

```markdown
* `cargo display-bench` — run the desktop codec's benchmark scenes.
```

- [ ] **Step 4: Verify the whole workspace**

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
cargo build -p vmlord-display-codec --target x86_64-unknown-linux-musl
cargo check-windows
```

Expected: all clean. The musl build is what proves the crate is portable; the
Windows check is what proves the host still builds with it in
`default-members`.

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md AGENTS.md
git commit -m "TASK-116: Record the desktop codec in the architecture"
```

---

## Self-review

**Spec coverage.** Crate and dependency rules — Task 1, enforced by the global
constraints. Container format — Tasks 3 to 5. ZRLE and XOR — Tasks 2 and 5.
Cursor — Task 7. Bounded queue and keyframe policy — Task 8. Damage hints —
Task 5, including the under-reporting case the spec calls out. Scenes — Task 6.
Round-trip, golden, malformed and fuzz tests — Tasks 6 and 9. Benchmark and
`cargo display-bench` — Task 10. Record-cap arithmetic — asserted in Task 4.
Out-of-scope items appear nowhere, which is the intent.

**Placeholders.** None: every step names its files, its commands and its
expected output, and every test step carries the test.

**Type consistency.** `Geometry`, `Rect`, `TileSize`, `PixelFormat`,
`CodecError`, `Frame`, `EncoderConfig`, `Encoder`, `Decoder`, `Payload`,
`CursorImage`, `OwnedCursorImage`, `CursorPosition` are introduced once and
used with the same names and signatures throughout. One variant,
`CodecError::WrongPayloadKind`, is added in Task 5 rather than Task 1, and the
step says so explicitly.
