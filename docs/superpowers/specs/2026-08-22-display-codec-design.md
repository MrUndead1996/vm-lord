# Lossless desktop codec design

## Purpose

Task #116 gives the display stack the thing that turns a captured guest
framebuffer into the bytes the frame channel carries: a portable, safe-Rust
crate with an encoder, a decoder and the bounded queue that keeps a slow viewer
from being served history.

It is written against the frame contract of task #118 and against nothing else.
It does not capture, does not open a socket, does not know what DRM is and does
not know what Windows is. Task #115 will feed it pixels in the guest; task #117
will hand it bytes in the Windows viewer. Both must be able to build it
unchanged — the guest side inside a static musl binary produced without a C
toolchain.

## What the protocol already settled

`vmlord-display-protocol` treats a `Keyframe`, a `TileDelta`, a `CursorImage`
and a `CursorPosition` as opaque bytes and writes them to the socket as they
are. What lives in the record header must not be repeated inside the codec's
payload:

* `sequence` — the frame's identity;
* `base` — the sequence a delta applies to, zero on a keyframe;
* `checksum` — CRC32C over the payload;
* `generation` — which connection of the frame channel this belongs to.

Geometry is settled too, by a `StreamConfig` record sent ahead of the keyframe
it describes: `width`, `height`, `tile_size` and `pixel_format`. Tile size is
negotiated in the handshake and constant for the session, so the codec never
has to change it mid-stream.

A frame record is capped at `width * height * 4 + 64 KiB`, under an absolute
64 MiB. A raw keyframe fits, with the 64 KiB as the only room for metadata.

The only back edge is `RequestKeyframe`. Nothing acknowledges a frame, and the
protocol does no flow control, so the bounded queue is this crate's obligation.

## Decisions

* **One crate, no dependencies.** `crates/display-codec`
  (`vmlord-display-codec`), safe Rust under the workspace's
  `unsafe_code = "deny"`, in `default-members` because the host viewer links it,
  and buildable for `x86_64-unknown-linux-musl` because the guest does.
* **Raw and ZRLE are mandatory; LZ4 is not in the MVP.** Task #116 permits LZ4
  only after a benchmark says it earns its place. This design ships the
  benchmark, not LZ4.
* **The queue sits before the encoder, not after it.** See *The bounded queue*.
  This is the one place where the obvious arrangement is wrong, and it is what
  shapes the encoder's API.
* **The encoder is deterministic by construction**: for each tile it evaluates
  every permitted encoding and keeps the shortest, breaking ties by the lower
  method number. No heuristics, no tuning constants in the hot path, and the
  same input produces the same bytes on every machine and every run.
* **Pixels are opaque 32-bit units.** `BGRA8888` and `XRGB8888` differ to a
  viewer, not to a run-length coder. The codec carries the format for
  validation and never interprets a channel.

## The payload format

### The tile container

A keyframe and a tile delta are the same container, distinguished by a flag.
Eight bytes of header, then tile records:

```text
offset  size  field
0       1     format version, 1
1       1     flags: bit 0 set on a keyframe
2       2     tile columns, u16 little-endian
4       2     tile rows,    u16 little-endian
6       2     reserved, zero, checked on decode
```

The grid is derivable from `StreamConfig` and is duplicated here on purpose:
four bytes turn a `StreamConfig`/frame mismatch from a silently wrong picture
into a named error. The two reserved bytes keep the header a multiple of four
and are validated as zero, so a later format version cannot quietly reuse them.

**A keyframe carries every tile, in raster order, with no index.** The methods
permitted are `Raw` (0) and `Zrle` (1). A keyframe builds on nothing, so `XorZrle`
is not available to it.

**A delta carries only tiles that changed**, each as:

```text
tile index, LEB128 varint
method,     u8
length,     LEB128 varint — compressed methods only
data,       `length` bytes, or the tile's raw size for `Raw`
```

Indices are strictly increasing, which is checked on decode: it makes a delta
canonical and catches a shuffled payload.

**`Raw` carries no length field.** The length follows from the grid, the tile
size and the tile's position — edge tiles at the right and bottom edges are
clipped, so their size is `w * h * 4` for the clipped `w` and `h`. This is not
a micro-optimisation. At tile size 16 a 2560x1440 frame has 14400 tiles, and a
four-byte length on each would spend 57 KB of the record cap's 64 KB of slack —
in exactly the case that approaches the cap, an all-`Raw` keyframe. Without it
the worst-case overhead of a raw keyframe is one byte per tile.

### ZRLE

The baseline compressor, written here rather than borrowed, operating on 32-bit
pixels rather than bytes: a run of identical pixels is what a desktop produces,
and a byte-oriented coder would have to rediscover that four times per pixel.

The stream is a sequence of control varints, each followed by its data:

* `n & 1 == 0` — a literal run of `(n >> 1) + 1` pixels, followed by that many
  little-endian `u32`s;
* `n & 1 == 1` — a repeat run of `(n >> 1) + 1` copies of the single `u32` that
  follows.

The encoder emits a repeat run whenever it is not longer than the literal run
it replaces, and splits runs at 65536 pixels so a control varint stays short.
Decoding stops when the tile is full; trailing bytes are an error, as is a run
that would overflow the tile.

`XorZrle` is the same stream over `current XOR previous` for the tile. A tile
that changed in a corner is mostly zeros under XOR, which is the shape ZRLE is
best at; the decoder XORs the result back onto the tile it holds.

### The cursor

A separate stream with separate state, never mixed into the tile container.

`CursorImage`:

```text
0   1   format version, 1
1   1   method: Raw or Zrle
2   2   width,      u16
4   2   height,     u16
6   2   hotspot x,  u16
8   2   hotspot y,  u16
10  ..  data
```

Width and height are capped at 256, which bounds the record without consulting
the frame geometry.

`CursorPosition` is fixed at six bytes: version, visible flag, `x` and `y` as
`u16`. It is sent far more often than any other cursor record, so it costs no
varint and no length.

## The bounded queue

Task #116 asks for a bounded queue that keeps current state and discards stale
frames. The subtlety is *which* frames may be discarded.

Discarding an already-encoded delta is wrong. The next delta would be encoded
against a frame the viewer never received, and the decoder would apply it to
the wrong base: a picture that drifts silently, with no error anywhere and no
`RequestKeyframe` to recover it, because nothing detected a problem.

So the queue holds **captured** frames, and encoding happens at drain time:

* `Encoder::submit(frame, damage)` copies the frame into the encoder's staging
  buffer, replacing whatever had not yet been encoded. Two buffers, swapped;
  no allocation per frame.
* `Encoder::next_payload()` is called when the transport is ready to write. It
  encodes the staged frame, if there is one, and only then advances the
  encoder's reference frame.

The encoder's reference is therefore always the last payload the caller was
given — which is the last one written to the socket, since a failed write ends
the channel's generation and the next one begins with a keyframe anyway.

The cursor has two slots of its own, image and position, each latest-wins, and
a keyframe request is a flag rather than a queued item. `next_payload()` drains
in a fixed order — a pending keyframe request first, then the frame, then the
cursor image, then the cursor position — so that a viewer that lost
synchronisation is not made to wait behind a cursor move.

## The encoder

```rust
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub tile_size: TileSize,   // Sixteen, ThirtyTwo (default), SixtyFour
    pub pixel_format: PixelFormat,
}

pub struct EncoderConfig {
    pub geometry: Geometry,
    /// A protective keyframe every N frames. 300 by default.
    pub keyframe_interval: u32,
}

pub struct Frame<'a> {
    pub pixels: &'a [u8],
    /// Bytes per row, which capture backends do not promise equals width * 4.
    pub stride: usize,
}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Self;
    pub fn submit(&mut self, frame: Frame<'_>, damage: Option<&[Rect]>) -> Result<(), CodecError>;
    pub fn submit_cursor_image(&mut self, cursor: CursorImage<'_>) -> Result<(), CodecError>;
    pub fn submit_cursor_position(&mut self, position: CursorPosition);
    pub fn request_keyframe(&mut self);
    pub fn next_payload(&mut self) -> Option<Payload<'_>>;
}

pub enum Payload<'a> {
    Keyframe(&'a [u8]),
    TileDelta(&'a [u8]),
    CursorImage(&'a [u8]),
    CursorPosition(&'a [u8]),
}
```

A `Payload` borrows the encoder's output buffer, which is reused: the caller
writes it to the socket and asks for the next one. The variants name the frame
record types of the protocol without depending on the protocol crate — the
codec stays a leaf, and the guest services map the variant to
`FRAME_RECORD_KEYFRAME` and its neighbours.

`damage` is a hint, not a fact: a DRM damage rectangle says a region *may* have
changed. Tiles it covers are compared against the reference; tiles it does not
cover are skipped. With `None` every tile is compared. A frame whose comparison
finds nothing changed produces no payload at all — a static desktop sends
nothing.

A keyframe is emitted on the first frame, on `request_keyframe`, and every
`keyframe_interval` frames. Geometry never changes within an `Encoder`: a
resolution change is a new `StreamConfig`, hence a new encoder, which is also
what makes the reference frame's size an invariant rather than a check.

## The decoder

```rust
impl Decoder {
    pub fn new(geometry: Geometry) -> Self;
    pub fn apply_keyframe(&mut self, payload: &[u8]) -> Result<&[Rect], CodecError>;
    pub fn apply_delta(&mut self, payload: &[u8]) -> Result<&[Rect], CodecError>;
    pub fn decode_cursor_image(payload: &[u8]) -> Result<OwnedCursorImage, CodecError>;
    pub fn decode_cursor_position(payload: &[u8]) -> Result<CursorPosition, CodecError>;
    pub fn frame(&self) -> &[u8];
}
```

Applying returns the rectangles that changed, so a viewer uploads only dirty
tiles to its texture rather than a whole frame. `apply_delta` before any
keyframe is `CodecError::NoBase` — the case where the viewer sends
`RequestKeyframe` — never a silent no-op.

The decoder trusts nothing in a payload. Every length, index and run is checked
against the geometry it was constructed with, and every error is a returned
`CodecError`, never a panic:

* `UnknownVersion`, `UnknownMethod`
* `GridMismatch` — the header's columns and rows are not this geometry's
* `TileIndexOutOfRange`, `TileIndexNotIncreasing`
* `Truncated`, `TrailingBytes`
* `RunOverflow` — a ZRLE run longer than the tile it fills
* `NoBase` — a delta with nothing to apply it to
* `WrongPayloadKind` — a keyframe applied as a delta, or the reverse
* `CursorTooLarge`

## Scenes

`scenes` is an ordinary public module, not a feature: the property tests, the
golden vectors and the benchmark all need the same deterministic workloads, and
a guest binary that calls none of them drops it at link time.

Each scene is a generator producing successive frames from a fixed seed:
`static_desktop`, `typing`, `scrolling`, `moving_window`, `fullscreen_video`.
They are synthetic — flat regions, moving glyph blocks, a shifting viewport,
noise — chosen to exercise the encoder's decision, not to look like a desktop.

## Tests

Following `vmlord-display-protocol`'s conventions, and its reasoning: no
`proptest`, no `criterion`, no `cargo-fuzz`. This repository builds on stable
and runs its tests in `cargo test`.

* `roundtrip` — the property `decode(encode) == current` over every scene, over
  every tile size, and over geometries that are not multiples of the tile size,
  with and without damage hints. Includes damage hints that under-report, which
  must not corrupt the stream: a hint that misses a changed region is a bug in
  the capture backend, and the property that holds is that the decoder still
  matches what the encoder believed it sent.
* `golden` — the bytes this build produces, held still, refreshed deliberately
  with `VMLORD_REFRESH_GOLDEN=1`.
* `malformed` — a corpus of hand-built payloads, one per `CodecError`, each
  asserting the specific error rather than merely "an error".
* `fuzz` — deterministic xorshift mutations of the golden vectors against both
  decoders. Two invariants: nothing panics, and no decoder reports success on
  a payload it did not fully consume.

## Benchmarks

`cargo display-bench`, an alias for
`run --release -p xtask -- display-bench`. xtask links the codec directly and
runs in release, so no nested `cargo` invocation and no debug-build numbers.

It runs the five scenes and prints, per scene: bytes per frame (mean and
worst), compression ratio against raw, encode milliseconds (mean and worst),
decode milliseconds, and the share of frames that were keyframes. It verifies
the round trip as it goes, so a benchmark run is also a long test.

The output is a table on stdout, not a stored baseline: this task's benchmark
exists to answer the LZ4 question and to catch an obviously wrong tile size
default, not to gate CI on a number that depends on the machine.

## Out of scope

* LZ4, and any codec beyond Raw and ZRLE.
* Motion codec (#123) — this crate is `MODE_DESKTOP` only.
* Capture, DRM, sockets, threads: no `std::thread`, no timers, no I/O.
* Multi-monitor (#130): one framebuffer, one cursor.
