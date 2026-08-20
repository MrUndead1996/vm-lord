# Display Protocol v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `vmlord-display-protocol`, the portable wire contract for VMLord's own display stack: record framing with per-channel limits, a Protobuf schema, mutual authentication over the existing per-VM secret, channel binding, and a transport-free session state machine both ends drive.

**Architecture:** One crate, no transport. Every record on all three channels carries the same 24-byte binary header; payloads are Protobuf on the control and input channels and for the frame channel's handshake, and opaque codec bytes for frames and cursors. The control channel runs a four-record mutual handshake whose transcript hash keys the frame and input channels, so a socket cannot be carried in from another session. A `Session` state machine holds the transcript, derives the keys and verifies the bindings, so the guest half (#115) and the host half (#117) cannot drift over what is hashed.

**Tech Stack:** Rust 2024, `prost` 0.14 + `protox` 0.9 (schema compiled in-process, no `protoc`), `hmac`/`sha2`/`hkdf`/`subtle`/`zeroize`/`getrandom` (pure Rust — the guest side links statically against musl with no C toolchain), `crc32c` 0.6, `base64` 0.23.

**Spec:** `docs/superpowers/specs/2026-08-20-display-protocol-v1-design.md`

## Global Constraints

- All new code is Rust. This crate is portable by construction: no Windows APIs, no Linux syscalls, no sockets, no threads, no async, no `unsafe`.
- The crate knows nothing about the codec's byte format. `Keyframe`, `TileDelta`, `CursorImage` and `CursorPosition` payloads are opaque bytes.
- No dependency on `vmlord-agent-protocol`. The two contracts are versioned independently; the negotiation rules are re-implemented, not shared.
- Every dependency must be pure Rust. Nothing that links a C library — that would cost the toolchain-free musl cross-compilation of the guest side.
- Protobuf compatibility rules, copied from `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`: never reuse or renumber a field; adding a message, field, enum value or `oneof` arm is a minor bump, removing or repurposing one is a major bump; every enum keeps a zero `*_UNSPECIFIED` value.
- Protocol version of this build: `major = 1`, `minor = 0`.
- Record header is 24 bytes, little-endian, layout fixed by the spec.
- Limits: control 65536 bytes, input 4096 bytes, frame `width * height * 4 + 65536` capped at 67108864.
- Domain strings, exactly: `"vmlord.display.v1.session"`, `"vmlord.display.v1.transcript"`, `"vmlord.display.v1.channel"`, role labels `"server"` and `"client"`.
- Tags are compared in constant time, through `subtle`. Never with `==`.
- Commit subjects are `TASK-118: <comment>`.
- Test command: `cargo test -p vmlord-display-protocol <filter>`. The crate is portable, so it runs natively; `cargo test-windows` is not needed for it. Never prefix commands with `timeout`.
- Compile check for the whole workspace: `cargo check-windows`.

---

### Task 1: The crate, the schema and the checked-in descriptor

The wire format is a schema plus a framing rule. This task lands the schema whole — every message on all three channels — with the build that compiles it without `protoc` and the test that holds the checked-in descriptor to it. Nothing here has behaviour yet; the deliverable is a crate that builds and a schema a reviewer can read in one sitting.

**Files:**
- Create: `crates/display-protocol/Cargo.toml`
- Create: `crates/display-protocol/build.rs`
- Create: `crates/display-protocol/proto/vmlord/display/v1/display.proto`
- Create: `crates/display-protocol/proto/display.descriptor.bin` (generated in step 4)
- Create: `crates/display-protocol/src/lib.rs`
- Create: `crates/display-protocol/tests/descriptor.rs`
- Modify: `Cargo.toml` (workspace `members` and `default-members`)

**Interfaces:**
- Consumes: nothing.
- Produces: the crate `vmlord-display-protocol`; module `vmlord_display_protocol::v1` holding the generated types; `vmlord_display_protocol::FILE_DESCRIPTOR_SET: &[u8]`.

- [ ] **Step 1: Add the crate to the workspace**

In `Cargo.toml`, add `"crates/display-protocol",` to `members` after `"crates/core",` and to `default-members` after `"crates/core",`. It belongs in both for the reason `agent-protocol` does: the host links it too, and it is portable enough to build in the default set.

- [ ] **Step 2: Write the manifest**

`crates/display-protocol/Cargo.toml`:

```toml
[package]
name = "vmlord-display-protocol"
version.workspace = true
edition.workspace = true
license.workspace = true
build = "build.rs"

[dependencies]
# Runtime only: the generated types are plain structs with `encode`/`decode`.
# The frame channel deliberately does not use them for pixel payloads.
prost = "0.14"

# The record checksum. Castagnoli rather than IEEE because it is the
# polynomial with hardware support on both ends of this socket.
crc32c = "0.6"

# What `keys` is made of. All pure Rust: the guest side of this protocol ships
# in a static musl binary built without a C toolchain.
base64 = "0.23"
getrandom = "0.3"
hkdf = "0.12"
hmac = "0.12"
sha2 = "0.10"
# Comparing a tag with `==` leaks where two tags first differ, which is enough
# to forge one a byte at a time.
subtle = "2.6"
zeroize = "1.9"

[build-dependencies]
prost = "0.14"
prost-build = "0.14"
# Compiles the `.proto` in-process, so no `protoc` has to be installed on
# either the Windows or the Linux side.
protox = "0.9"

[lints]
workspace = true
```

- [ ] **Step 3: Write the schema**

`crates/display-protocol/proto/vmlord/display/v1/display.proto`:

```proto
// The wire contract between VMLord's viewer on the host and the display
// services in a guest.
//
// Not a gRPC service, and not the whole wire format either: every message here
// travels as the payload of a 24-byte binary record header (see
// `vmlord_display_protocol::record`), and the frame channel's pixel payloads
// are codec bytes that never pass through this schema at all.
//
// Compatibility rules for anyone editing this file:
//
//   * never reuse or renumber a field -- the guest and the host are upgraded
//     separately;
//   * adding a message, a field, an enum value or a `oneof` arm is a minor
//     version bump; removing or repurposing one is a major bump;
//   * every enum keeps a zero `*_UNSPECIFIED` value, because proto3 cannot
//     tell "absent" from "first variant" without one;
//   * record type numbers are the `*Record` enums below. They are part of the
//     framing, not of any message, and are as unchangeable as a field number.

syntax = "proto3";

package vmlord.display.v1;

// The revision of this schema a peer implements.
message ProtocolVersion {
  // Bumped when an existing message changes meaning. Peers with differing
  // majors have nothing to negotiate.
  uint32 major = 1;
  // Bumped when something is added. A session runs at the lower of the two.
  uint32 minor = 2;
}

// An optional part of this revision, offered by both peers and used only if
// both have it.
enum Capability {
  CAPABILITY_UNSPECIFIED = 0;
  // The guest sends cursor shape and position as their own records rather
  // than drawing the cursor into the frame.
  CAPABILITY_CURSOR_STREAM = 1;
  // The guest can change its output geometry while a session runs.
  CAPABILITY_DYNAMIC_RESOLUTION = 2;
}

// How the encoder trades bandwidth against fidelity.
//
// The MVP guest announces MODE_DESKTOP alone, and MODE_AUTO names a host-side
// policy that resolves to MODE_DESKTOP until MODE_MOTION exists (task #123).
enum Mode {
  MODE_UNSPECIFIED = 0;
  MODE_AUTO = 1;
  MODE_DESKTOP = 2;
  MODE_MOTION = 3;
}

// How a decoded frame's bytes are laid out.
enum PixelFormat {
  PIXEL_FORMAT_UNSPECIFIED = 0;
  PIXEL_FORMAT_BGRA8888 = 1;
  PIXEL_FORMAT_XRGB8888 = 2;
}

// Why a session, a channel or a request was refused.
enum ErrorCode {
  ERROR_CODE_UNSPECIFIED = 0;
  ERROR_CODE_UNSUPPORTED_VERSION = 1;
  ERROR_CODE_UNAUTHENTICATED = 2;
  ERROR_CODE_UNKNOWN_SESSION = 3;
  ERROR_CODE_CHANNEL_BINDING_FAILED = 4;
  ERROR_CODE_MALFORMED_RECORD = 5;
  ERROR_CODE_RECORD_TOO_LARGE = 6;
  ERROR_CODE_CHECKSUM_MISMATCH = 7;
  ERROR_CODE_UNSUPPORTED_MODE = 8;
  ERROR_CODE_RESOLUTION_REJECTED = 9;
  ERROR_CODE_CAPTURE_FAILED = 10;
  ERROR_CODE_INTERNAL = 11;
}

// The `type` field of a record header on the control channel.
enum ControlRecord {
  CONTROL_RECORD_UNSPECIFIED = 0;
  CONTROL_RECORD_CLIENT_HELLO = 1;
  CONTROL_RECORD_SERVER_HELLO = 2;
  CONTROL_RECORD_SERVER_AUTH = 3;
  CONTROL_RECORD_CLIENT_AUTH = 4;
  CONTROL_RECORD_SET_MODE = 5;
  CONTROL_RECORD_SET_RESOLUTION = 6;
  CONTROL_RECORD_REQUEST_KEYFRAME = 7;
  CONTROL_RECORD_PING = 8;
  CONTROL_RECORD_PONG = 9;
  CONTROL_RECORD_END_SESSION = 10;
  CONTROL_RECORD_DISPLAY_STATE = 11;
  CONTROL_RECORD_ERROR = 12;
}

// The `type` field of a record header on the frame channel.
enum FrameRecord {
  FRAME_RECORD_UNSPECIFIED = 0;
  FRAME_RECORD_CHANNEL_HELLO = 1;
  FRAME_RECORD_CHANNEL_ACK = 2;
  FRAME_RECORD_CHANNEL_AUTH = 3;
  FRAME_RECORD_STREAM_CONFIG = 4;
  // The four below carry codec bytes, not a message from this schema.
  FRAME_RECORD_KEYFRAME = 5;
  FRAME_RECORD_TILE_DELTA = 6;
  FRAME_RECORD_CURSOR_IMAGE = 7;
  FRAME_RECORD_CURSOR_POSITION = 8;
  FRAME_RECORD_ERROR = 9;
}

// The `type` field of a record header on the input channel.
enum InputRecord {
  INPUT_RECORD_UNSPECIFIED = 0;
  INPUT_RECORD_CHANNEL_HELLO = 1;
  INPUT_RECORD_CHANNEL_ACK = 2;
  INPUT_RECORD_CHANNEL_AUTH = 3;
  INPUT_RECORD_KEY_EVENT = 4;
  INPUT_RECORD_POINTER_MOTION = 5;
  INPUT_RECORD_POINTER_BUTTON = 6;
  INPUT_RECORD_POINTER_SCROLL = 7;
  INPUT_RECORD_RELEASE_ALL = 8;
  INPUT_RECORD_ERROR = 9;
}

// The host's opening record, and the first half of the transcript.
message ClientHello {
  ProtocolVersion version = 1;
  repeated Capability capabilities = 2;
  // 16 random bytes naming this session. The guest looks a frame or input
  // channel up by it.
  bytes session_id = 3;
  // 32 random bytes, half of the session key's salt.
  bytes host_nonce = 4;
  Mode mode = 5;
  uint32 width = 6;
  uint32 height = 7;
  uint32 tile_size = 8;
}

// The guest's answer, and the second half of the transcript.
message ServerHello {
  // The revision the session runs at: the host's major, the lower minor.
  ProtocolVersion version = 1;
  // The intersection, in the guest's order.
  repeated Capability capabilities = 2;
  // 32 random bytes, the other half of the session key's salt.
  bytes guest_nonce = 3;
  repeated Mode modes = 4;
  repeated uint32 tile_sizes = 5;
  uint32 width = 6;
  uint32 height = 7;
}

// The guest proving it holds the VM's secret. Sent before the host's proof.
message ServerAuth {
  // HMAC-SHA256(session key, "server" || transcript).
  bytes tag = 1;
}

// The host proving the same, over the same transcript.
message ClientAuth {
  // HMAC-SHA256(session key, "client" || transcript).
  bytes tag = 1;
}

// The host opening a frame or input channel for an established session.
message ChannelHello {
  bytes session_id = 1;
  // 2 for frame, 3 for input, matching the record header's channel byte.
  uint32 channel = 2;
  uint32 generation = 3;
  bytes nonce = 4;
}

// The guest answering, and proving it holds the same channel key.
message ChannelAck {
  bytes nonce = 1;
  // HMAC-SHA256(channel key, "server" || channel || host nonce || guest nonce).
  bytes tag = 2;
}

// The host's proof on the same channel.
message ChannelAuth {
  // HMAC-SHA256(channel key, "client" || channel || host nonce || guest nonce).
  bytes tag = 1;
}

// The geometry of the frames that follow, on the frame channel and ahead of
// the keyframe it describes.
message StreamConfig {
  uint32 width = 1;
  uint32 height = 2;
  uint32 tile_size = 3;
  PixelFormat pixel_format = 4;
}

message SetMode {
  Mode mode = 1;
}

message SetResolution {
  uint32 width = 1;
  uint32 height = 2;
}

// The viewer's decoder has nothing to apply a delta to and needs a fresh
// keyframe. Recovery, not flow control.
message RequestKeyframe {}

message Ping {
  uint64 token = 1;
}

message Pong {
  // The token of the ping this answers.
  uint64 token = 1;
}

// The host is finished with the session and the guest may stop capturing.
message EndSession {}

// What the guest actually applied, which need not be what was asked for.
message DisplayState {
  uint32 width = 1;
  uint32 height = 2;
  uint32 tile_size = 3;
  Mode mode = 4;
}

message Error {
  ErrorCode code = 1;
  // For a log, not for a decision. Never parsed.
  string detail = 2;
}

message KeyEvent {
  // A Linux evdev keycode. The viewer does the translation.
  uint32 keycode = 1;
  bool pressed = 2;
}

message PointerMotion {
  // Guest pixels. Letterbox and scaling stay in the viewer.
  uint32 x = 1;
  uint32 y = 2;
}

message PointerButton {
  // A Linux evdev button code.
  uint32 button = 1;
  bool pressed = 2;
}

message PointerScroll {
  // Hundred-and-twentieths of a wheel detent, as high-resolution wheels report.
  sint32 horizontal = 1;
  sint32 vertical = 2;
}

// Release every key and button the guest believes is held.
message ReleaseAll {}
```

- [ ] **Step 4: Write the build script**

`crates/display-protocol/build.rs`:

```rust
//! Turns `proto/vmlord/display/v1/display.proto` into Rust, without `protoc`.
//!
//! `protox` parses the schema in-process and hands `prost-build` the same
//! `FileDescriptorSet` a `protoc` invocation would have. The descriptor is
//! also written out whole, so that `tests/descriptor.rs` can hold the
//! checked-in copy to it.

use std::{env, fs, path::PathBuf};

use prost::Message;

const PROTO: &str = "proto/vmlord/display/v1/display.proto";
const INCLUDE: &str = "proto";

fn main() {
    println!("cargo::rerun-if-changed={PROTO}");

    let descriptor_set = protox::compile([PROTO], [INCLUDE])
        .unwrap_or_else(|error| panic!("failed to compile {PROTO}: {error}"));

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::write(
        out_dir.join("display.descriptor.bin"),
        descriptor_set.encode_to_vec(),
    )
    .expect("failed to write the descriptor set");

    prost_build::Config::new()
        .compile_fds(descriptor_set)
        .expect("failed to generate Rust types");
}
```

- [ ] **Step 5: Write the crate root**

`crates/display-protocol/src/lib.rs`:

```rust
//! The wire contract between VMLord's display viewer on the host and the
//! display services in a guest.
//!
//! Portable by construction: no Windows APIs, no Linux syscalls, no transport.
//! It knows what a record is, how one is delimited, what proves a peer, and
//! what a session's states are; opening the three HvSocket services that carry
//! the bytes belongs to the host viewer and to the guest services.
//!
//! The schema lives in `proto/vmlord/display/v1/display.proto` and is compiled
//! at build time. [`FILE_DESCRIPTOR_SET`] is the same schema in the form other
//! tools read, checked in beside the `.proto` so that a change to the wire
//! format shows up in a diff.

/// The generated types for `vmlord.display.v1`.
///
/// The whole schema is one version module. A `v2` would be a second module
/// beside it rather than an edit of this one.
pub mod v1 {
    // Generated code is not written to this repository's standards and cannot
    // be, so it is not linted against them.
    #![allow(clippy::all, clippy::pedantic, missing_docs)]

    include!(concat!(env!("OUT_DIR"), "/vmlord.display.v1.rs"));
}

/// The compiled schema, for tools that read descriptor sets rather than Rust.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("../proto/display.descriptor.bin");
```

- [ ] **Step 6: Write the descriptor test**

`crates/display-protocol/tests/descriptor.rs`:

```rust
//! Holds the checked-in descriptor set to the `.proto` it was made from.
//!
//! To refresh it after an intentional change:
//!
//! ```text
//! VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-display-protocol
//! ```

use std::{env, fs, path::Path};

/// What `build.rs` compiled from the `.proto` on this run.
const GENERATED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/display.descriptor.bin"));

const CHECKED_IN: &str = "proto/display.descriptor.bin";

#[test]
fn the_checked_in_descriptor_set_matches_the_schema() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CHECKED_IN);

    if env::var_os("VMLORD_REFRESH_DESCRIPTOR").is_some() {
        fs::write(&path, GENERATED).expect("failed to refresh the descriptor set");
        return;
    }

    let checked_in = fs::read(&path).expect("failed to read the checked-in descriptor set");
    assert_eq!(
        checked_in, GENERATED,
        "{CHECKED_IN} is not what the .proto compiles to; refresh it with \
         VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-display-protocol"
    );
}

#[test]
fn the_crate_publishes_the_checked_in_descriptor_set() {
    assert_eq!(
        vmlord_display_protocol::FILE_DESCRIPTOR_SET,
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(CHECKED_IN))
            .expect("failed to read the checked-in descriptor set")
    );
}
```

- [ ] **Step 7: Generate the descriptor and run the tests**

The checked-in descriptor does not exist yet, so create it from the build's own output:

Run: `touch crates/display-protocol/proto/display.descriptor.bin && VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-display-protocol`
Expected: PASS, and `proto/display.descriptor.bin` is now non-empty.

Run: `cargo test -p vmlord-display-protocol`
Expected: PASS, 2 tests.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock crates/display-protocol
git commit -m "TASK-118: Add the display protocol schema"
```

---

### Task 2: The record header

The header is the one thing every record on every channel shares, and the first thing an untrusted peer's bytes reach. It parses from a fixed array with no allocation, refuses a `header_len` below 24, and reports how many bytes of a longer header a future minor would have a reader skip.

**Files:**
- Create: `crates/display-protocol/src/record.rs`
- Modify: `crates/display-protocol/src/lib.rs` (add `pub mod record;`)
- Test: `crates/display-protocol/src/record.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `record::HEADER_LEN: usize` = 24
  - `enum record::Channel { Control = 1, Frame = 2, Input = 3 }` with `Channel::from_wire(u8) -> Result<Channel, RecordError>` and `Channel::as_wire(self) -> u8`
  - `struct record::Header { channel: Channel, message_type: u16, length: u32, sequence: u32, base: u32, checksum: u32, generation: u32 }`
  - `Header::encode(&self) -> [u8; HEADER_LEN]`
  - `Header::decode(bytes: &[u8; HEADER_LEN]) -> Result<(Header, usize), RecordError>` — the `usize` is the count of extra header bytes a newer minor appended, which the caller must consume before the payload
  - `enum record::RecordError` with variants `MalformedHeader { header_len: u8 }`, `UnknownChannel { value: u8 }`, `TooLarge { channel: Channel, length: u32, cap: u32 }`, `ChecksumMismatch { expected: u32, found: u32 }`, `Closed`, `Idle`, `Io(std::io::Error)` — all seven declared now, with `Display` and `Error`, because task 3 fills the rest in and a half-declared error type is worse than a whole one

- [ ] **Step 1: Write the failing tests**

Create `crates/display-protocol/src/record.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            channel: Channel::Frame,
            message_type: 6,
            length: 4096,
            sequence: 17,
            base: 16,
            checksum: 0xDEAD_BEEF,
            generation: 2,
        }
    }

    #[test]
    fn a_header_survives_a_round_trip() {
        let (decoded, extra) = Header::decode(&header().encode()).expect("a header this crate encoded");

        assert_eq!(decoded, header());
        assert_eq!(extra, 0);
    }

    #[test]
    fn a_header_is_twenty_four_little_endian_bytes() {
        let bytes = header().encode();

        assert_eq!(bytes[0], 24);
        assert_eq!(bytes[1], 2);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 6);
        assert_eq!(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]), 4096);
    }

    #[test]
    fn a_longer_header_reports_the_bytes_a_reader_has_to_skip() {
        // What a future minor's writer produces: the same 24 bytes this build
        // knows, and four it does not.
        let mut bytes = header().encode();
        bytes[0] = 28;

        let (decoded, extra) = Header::decode(&bytes).expect("a header from a newer minor");

        assert_eq!(decoded, header());
        assert_eq!(extra, 4);
    }

    #[test]
    fn a_header_shorter_than_this_build_reads_is_refused() {
        let mut bytes = header().encode();
        bytes[0] = 23;

        assert!(matches!(
            Header::decode(&bytes),
            Err(RecordError::MalformedHeader { header_len: 23 })
        ));
    }

    #[test]
    fn a_channel_this_build_does_not_know_is_refused() {
        let mut bytes = header().encode();
        bytes[1] = 9;

        assert!(matches!(
            Header::decode(&bytes),
            Err(RecordError::UnknownChannel { value: 9 })
        ));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol record`
Expected: FAIL — the module does not compile, `Header` is not defined.

- [ ] **Step 3: Write the implementation**

Above the tests in `crates/display-protocol/src/record.rs`:

```rust
//! How a record is delimited on any of the three channels.
//!
//! A record is a fixed 24-byte little-endian header followed by `length`
//! payload bytes. Little-endian because both ends of these sockets are x86-64;
//! fixed rather than length-prefixed Protobuf because the frame channel's
//! payloads are megabytes of codec output that must reach the socket without
//! being copied through an encoder.
//!
//! The first byte is the header's own length. It is what lets v1.2 append a
//! field that v1.0 skips without losing the stream, and it is why there is no
//! magic number: the version is settled in the handshake, and four bytes per
//! frame to make a packet dump readable is not a trade worth making.

use std::{error::Error, fmt, io};

/// The width of the header this build writes and understands.
pub const HEADER_LEN: usize = 24;

/// Which of a session's three sockets a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Handshake, session control, liveness and errors.
    Control = 1,
    /// Frames and cursors, from the guest only.
    Frame = 2,
    /// Keyboard and pointer, from the host only.
    Input = 3,
}

impl Channel {
    /// The byte that names this channel in a header.
    #[must_use]
    pub fn as_wire(self) -> u8 {
        self as u8
    }

    /// Reads a channel out of a header.
    ///
    /// # Errors
    ///
    /// [`RecordError::UnknownChannel`] for any other value. Unlike a
    /// capability, an unknown channel cannot be ignored: there is no way to
    /// know what the payload behind it means.
    pub fn from_wire(value: u8) -> Result<Self, RecordError> {
        match value {
            1 => Ok(Self::Control),
            2 => Ok(Self::Frame),
            3 => Ok(Self::Input),
            value => Err(RecordError::UnknownChannel { value }),
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Control => "control",
            Self::Frame => "frame",
            Self::Input => "input",
        })
    }
}

/// What precedes every payload on every channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Which socket this record belongs to.
    pub channel: Channel,
    /// The record's type within its channel: one of the `*Record` enums in the
    /// schema.
    pub message_type: u16,
    /// The payload's length in bytes.
    pub length: u32,
    /// The record's position in its channel's stream, from zero.
    pub sequence: u32,
    /// For a tile delta, the `sequence` of the frame it builds on. Zero
    /// everywhere else, including on a keyframe, which builds on nothing.
    pub base: u32,
    /// CRC32C of the payload. A corruption check, not a signature.
    pub checksum: u32,
    /// Which generation of the session's frame and input channels this belongs
    /// to. Stale generations are rejected here, before a decoder or an input
    /// device sees them.
    pub generation: u32,
}

impl Header {
    /// The bytes that precede this record's payload.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];

        bytes[0] = HEADER_LEN as u8;
        bytes[1] = self.channel.as_wire();
        bytes[2..4].copy_from_slice(&self.message_type.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.length.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.base.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.checksum.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.generation.to_le_bytes());

        bytes
    }

    /// Reads a header, and says how much of a longer one is left to skip.
    ///
    /// The returned count is `header_len - HEADER_LEN`: bytes a newer minor
    /// appended, which this build does not understand and the caller must
    /// consume before the payload begins.
    ///
    /// # Errors
    ///
    /// [`RecordError::MalformedHeader`] if `header_len` is below
    /// [`HEADER_LEN`] -- a header this build cannot read at all, rather than
    /// one it can read part of -- and [`RecordError::UnknownChannel`] for a
    /// channel byte that names no socket.
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<(Self, usize), RecordError> {
        let header_len = bytes[0];
        if usize::from(header_len) < HEADER_LEN {
            return Err(RecordError::MalformedHeader { header_len });
        }

        let header = Self {
            channel: Channel::from_wire(bytes[1])?,
            message_type: u16::from_le_bytes([bytes[2], bytes[3]]),
            length: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            sequence: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            base: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            checksum: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
            generation: u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        };

        Ok((header, usize::from(header_len) - HEADER_LEN))
    }
}

/// A record that could not be moved between memory and a stream.
#[derive(Debug)]
pub enum RecordError {
    /// A header shorter than this build reads.
    MalformedHeader { header_len: u8 },
    /// A channel byte that names no socket.
    UnknownChannel { value: u8 },
    /// A payload larger than its channel allows.
    TooLarge {
        channel: Channel,
        length: u32,
        cap: u32,
    },
    /// A payload whose CRC32C is not the one its header announced.
    ChecksumMismatch { expected: u32, found: u32 },
    /// The peer closed the connection at a record boundary.
    Closed,
    /// The transport timed out before the peer started another record.
    Idle,
    /// The transport failed.
    Io(io::Error),
}

impl fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedHeader { header_len } => write!(
                formatter,
                "a record header of {header_len} bytes is shorter than the {HEADER_LEN} this build reads"
            ),
            Self::UnknownChannel { value } => {
                write!(formatter, "{value} names no display protocol channel")
            }
            Self::TooLarge {
                channel,
                length,
                cap,
            } => write!(
                formatter,
                "a {length}-byte payload on the {channel} channel exceeds its {cap}-byte limit"
            ),
            Self::ChecksumMismatch { expected, found } => write!(
                formatter,
                "a record announced checksum {expected:#010x} and carries {found:#010x}"
            ),
            Self::Closed => formatter.write_str("the display connection was closed"),
            Self::Idle => formatter.write_str("the display connection is idle"),
            Self::Io(error) => write!(formatter, "the display connection failed: {error}"),
        }
    }
}

impl Error for RecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
```

Add to `crates/display-protocol/src/lib.rs`, above `pub mod v1`:

```rust
pub mod record;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol record`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Add the display record header"
```

---

### Task 3: Limits, checksums and reading records off a stream

The header is parsed before anything is allocated, and what bounds the allocation is not a constant on the frame channel: it is what the session's geometry implies. This task adds `Limits`, the CRC32C check, and reads and writes over `Read`/`Write` that distinguish a peer that hung up cleanly from a stream that was cut.

**Files:**
- Modify: `crates/display-protocol/src/record.rs` (append; `mod tests` grows)
- Test: `crates/display-protocol/src/record.rs` `mod tests`

**Interfaces:**
- Consumes: `record::{Header, Channel, RecordError, HEADER_LEN}` from task 2.
- Produces:
  - `record::CONTROL_MAX_PAYLOAD: u32` = 65536, `record::INPUT_MAX_PAYLOAD: u32` = 4096, `record::FRAME_PAYLOAD_CEILING: u32` = 67108864, `record::FRAME_PAYLOAD_SLACK: u32` = 65536
  - `struct record::Limits` with `Limits::new(width: u32, height: u32) -> Limits`, `Limits::set_geometry(&mut self, width: u32, height: u32)`, `Limits::for_channel(&self, channel: Channel) -> u32`
  - `struct record::Record { header: Header, payload: Vec<u8> }` with `Record::new(channel: Channel, message_type: u16, sequence: u32, base: u32, generation: u32, payload: Vec<u8>) -> Record` filling `length` and `checksum`
  - `record::write<W: std::io::Write>(writer: &mut W, record: &Record, limits: &Limits) -> Result<(), RecordError>`
  - `record::read<R: std::io::Read>(reader: &mut R, limits: &Limits, payload: &mut Vec<u8>) -> Result<Header, RecordError>`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `crates/display-protocol/src/record.rs`:

```rust
    #[test]
    fn the_frame_cap_is_a_raw_frame_of_the_agreed_geometry_plus_slack() {
        let limits = Limits::new(2560, 1440);

        assert_eq!(limits.for_channel(Channel::Frame), 2560 * 1440 * 4 + 65536);
        assert_eq!(limits.for_channel(Channel::Control), 65536);
        assert_eq!(limits.for_channel(Channel::Input), 4096);
    }

    #[test]
    fn a_geometry_that_would_overflow_the_cap_is_held_at_the_ceiling() {
        let limits = Limits::new(u32::MAX, u32::MAX);

        assert_eq!(limits.for_channel(Channel::Frame), FRAME_PAYLOAD_CEILING);
    }

    #[test]
    fn a_resolution_change_moves_the_frame_cap() {
        let mut limits = Limits::new(1920, 1080);
        limits.set_geometry(1280, 720);

        assert_eq!(limits.for_channel(Channel::Frame), 1280 * 720 * 4 + 65536);
    }

    #[test]
    fn a_record_survives_a_round_trip_through_a_stream() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Control, 8, 3, 0, 0, b"payload".to_vec());

        let mut wire = Vec::new();
        write(&mut wire, &record, &limits).expect("a record within the control cap");

        let mut payload = Vec::new();
        let header = read(&mut wire.as_slice(), &limits, &mut payload).expect("what was written");

        assert_eq!(header, record.header);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn a_payload_over_its_channel_cap_is_never_written() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Input, 5, 0, 0, 0, vec![0u8; 4097]);

        let mut wire = Vec::new();
        let error = write(&mut wire, &record, &limits).expect_err("a payload over the input cap");

        assert!(matches!(
            error,
            RecordError::TooLarge {
                channel: Channel::Input,
                length: 4097,
                cap: 4096
            }
        ));
        assert!(wire.is_empty(), "nothing may reach the wire");
    }

    #[test]
    fn a_length_over_the_cap_is_refused_before_anything_is_allocated() {
        let limits = Limits::new(64, 64);
        let mut header = Record::new(Channel::Frame, 5, 0, 0, 0, Vec::new()).header;
        header.length = limits.for_channel(Channel::Frame) + 1;

        let mut payload = Vec::new();
        let error = read(&mut header.encode().as_slice(), &limits, &mut payload)
            .expect_err("a length over the frame cap");

        assert!(matches!(error, RecordError::TooLarge { .. }));
    }

    #[test]
    fn a_payload_that_does_not_match_its_checksum_is_refused() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Control, 8, 0, 0, 0, b"payload".to_vec());

        let mut wire = Vec::new();
        write(&mut wire, &record, &limits).expect("a record within the control cap");
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;

        let mut payload = Vec::new();
        let error = read(&mut wire.as_slice(), &limits, &mut payload).expect_err("a corrupt payload");

        assert!(matches!(error, RecordError::ChecksumMismatch { .. }));
    }

    #[test]
    fn a_peer_that_hangs_up_between_records_is_not_a_fault() {
        let limits = Limits::new(64, 64);
        let mut payload = Vec::new();

        let error = read(&mut [].as_slice(), &limits, &mut payload).expect_err("an empty stream");

        assert!(matches!(error, RecordError::Closed));
    }

    #[test]
    fn a_stream_cut_inside_a_header_is_a_fault() {
        let limits = Limits::new(64, 64);
        let mut payload = Vec::new();

        let error = read(&mut [24u8, 1, 0].as_slice(), &limits, &mut payload)
            .expect_err("a truncated header");

        assert!(matches!(error, RecordError::Io(_)));
    }

    #[test]
    fn the_extra_bytes_of_a_newer_minors_header_are_skipped() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Control, 8, 0, 0, 0, b"payload".to_vec());

        let mut wire = record.header.encode().to_vec();
        wire[0] = 28;
        wire.extend_from_slice(&[0xAA; 4]);
        wire.extend_from_slice(&record.payload);

        let mut payload = Vec::new();
        let header = read(&mut wire.as_slice(), &limits, &mut payload).expect("a newer minor's record");

        assert_eq!(header.message_type, 8);
        assert_eq!(payload, b"payload");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol record`
Expected: FAIL — `Limits`, `Record`, `read` and `write` are not defined.

- [ ] **Step 3: Write the implementation**

Append to `crates/display-protocol/src/record.rs`, before `mod tests`:

```rust
/// The most a control record may carry.
///
/// Fixed, because nothing on this channel is a payload: hellos, tags, mode
/// changes and errors.
pub const CONTROL_MAX_PAYLOAD: u32 = 64 * 1024;

/// The most an input record may carry.
pub const INPUT_MAX_PAYLOAD: u32 = 4 * 1024;

/// The most a frame record may carry whatever the geometry says.
///
/// A backstop against a geometry that is itself absurd, since the cap below is
/// computed from numbers a peer sent.
pub const FRAME_PAYLOAD_CEILING: u32 = 64 * 1024 * 1024;

/// What a frame record may carry beyond its uncompressed pixels.
///
/// A keyframe should never exceed its raw size, but a codec header, a cursor
/// and a tile map are not pixels, and refusing a frame for its metadata would
/// be a limit that fires on correct behaviour.
pub const FRAME_PAYLOAD_SLACK: u32 = 64 * 1024;

/// What a session's records may weigh, given what it agreed to display.
///
/// The frame cap is derived rather than fixed: a record larger than an
/// uncompressed frame of the agreed geometry is not a frame by definition, so
/// "oversized" says something about this session instead of naming a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    frame: u32,
}

impl Limits {
    /// The limits for a session displaying `width` by `height`.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let mut limits = Self { frame: 0 };
        limits.set_geometry(width, height);
        limits
    }

    /// Moves the frame cap to a new geometry, as a `StreamConfig` does.
    pub fn set_geometry(&mut self, width: u32, height: u32) {
        self.frame = width
            .saturating_mul(height)
            .saturating_mul(4)
            .saturating_add(FRAME_PAYLOAD_SLACK)
            .min(FRAME_PAYLOAD_CEILING);
    }

    /// The largest payload `channel` may carry in this session.
    #[must_use]
    pub fn for_channel(&self, channel: Channel) -> u32 {
        match channel {
            Channel::Control => CONTROL_MAX_PAYLOAD,
            Channel::Frame => self.frame,
            Channel::Input => INPUT_MAX_PAYLOAD,
        }
    }
}

/// A header and the payload it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// What precedes the payload on the wire.
    pub header: Header,
    /// A Protobuf message, or codec bytes on the frame channel's four pixel
    /// types.
    pub payload: Vec<u8>,
}

impl Record {
    /// Builds a record, filling in the length and the checksum.
    ///
    /// Those two are the header's own arithmetic rather than a caller's, which
    /// is what keeps a record from ever announcing a length it does not carry.
    #[must_use]
    pub fn new(
        channel: Channel,
        message_type: u16,
        sequence: u32,
        base: u32,
        generation: u32,
        payload: Vec<u8>,
    ) -> Self {
        let header = Header {
            channel,
            message_type,
            length: u32::try_from(payload.len()).unwrap_or(u32::MAX),
            sequence,
            base,
            checksum: crc32c::crc32c(&payload),
            generation,
        };

        Self { header, payload }
    }
}

/// Writes one record and flushes it.
///
/// Flushing belongs here rather than to the caller: a buffered transport that
/// holds a keystroke or a keyframe request back is a session that appears to
/// have frozen.
///
/// # Errors
///
/// [`RecordError::TooLarge`] if the payload exceeds its channel's cap, in
/// which case nothing is written -- a payload that cannot be framed must not
/// become a truncated one -- or [`RecordError::Io`] if the transport fails.
pub fn write<W: io::Write>(
    writer: &mut W,
    record: &Record,
    limits: &Limits,
) -> Result<(), RecordError> {
    let cap = limits.for_channel(record.header.channel);
    if record.header.length > cap {
        return Err(RecordError::TooLarge {
            channel: record.header.channel,
            length: record.header.length,
            cap,
        });
    }

    writer
        .write_all(&record.header.encode())
        .map_err(RecordError::Io)?;
    writer.write_all(&record.payload).map_err(RecordError::Io)?;
    writer.flush().map_err(RecordError::Io)
}

/// Reads one record, leaving its payload in `payload`.
///
/// The cap is enforced from the header, before `payload` is grown, and the
/// checksum after the bytes are in: the first bounds what a hostile peer can
/// make this side allocate, the second catches a transport that corrupted what
/// it carried.
///
/// # Errors
///
/// [`RecordError::Closed`] if the peer hung up at a record boundary, which is
/// how a session ends and is not by itself a fault; [`RecordError::Idle`] if
/// the transport timed out before a record began, so the caller may safely
/// send one of its own. [`RecordError::MalformedHeader`],
/// [`RecordError::UnknownChannel`], [`RecordError::TooLarge`],
/// [`RecordError::ChecksumMismatch`] and [`RecordError::Io`] all leave the
/// stream unusable and must be answered by closing it.
pub fn read<R: io::Read>(
    reader: &mut R,
    limits: &Limits,
    payload: &mut Vec<u8>,
) -> Result<Header, RecordError> {
    let mut bytes = [0u8; HEADER_LEN];
    read_header_bytes(reader, &mut bytes)?;

    let (header, extra) = Header::decode(&bytes)?;

    let cap = limits.for_channel(header.channel);
    if header.length > cap {
        return Err(RecordError::TooLarge {
            channel: header.channel,
            length: header.length,
            cap,
        });
    }

    if extra > 0 {
        io::copy(&mut reader.take(extra as u64), &mut io::sink()).map_err(RecordError::Io)?;
    }

    payload.clear();
    payload.resize(header.length as usize, 0);
    reader.read_exact(payload).map_err(RecordError::Io)?;

    let found = crc32c::crc32c(payload);
    if found != header.checksum {
        return Err(RecordError::ChecksumMismatch {
            expected: header.checksum,
            found,
        });
    }

    Ok(header)
}

/// Fills `bytes`, telling a connection that ended between records from one
/// that ended inside a header.
///
/// `Read::read_exact` reports both as `UnexpectedEof`, and they mean opposite
/// things: the first is a peer that finished, the second is a cut stream.
fn read_header_bytes<R: io::Read>(
    reader: &mut R,
    bytes: &mut [u8; HEADER_LEN],
) -> Result<(), RecordError> {
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) if filled == 0 => return Err(RecordError::Closed),
            Ok(0) => {
                return Err(RecordError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the connection ended part-way through a record header",
                )));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if filled == 0
                    && matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
            {
                return Err(RecordError::Idle);
            }
            Err(error) => return Err(RecordError::Io(error)),
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol record`
Expected: PASS, 15 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol Cargo.lock
git commit -m "TASK-118: Frame display records with per-session limits"
```

---

### Task 4: Version and capability negotiation

The rules are the agent protocol's, re-implemented rather than shared, because the two contracts are versioned apart. A differing major has nothing to negotiate; differing minors run at the lower; the capabilities are the intersection, and a number this build has never heard of is dropped rather than refused.

**Files:**
- Create: `crates/display-protocol/src/handshake.rs`
- Modify: `crates/display-protocol/src/lib.rs` (add `pub mod handshake;`)
- Test: `crates/display-protocol/src/handshake.rs` `mod tests`

**Interfaces:**
- Consumes: `v1::{Capability, ProtocolVersion}` from task 1.
- Produces:
  - `handshake::CURRENT_VERSION: ProtocolVersion` = `{ major: 1, minor: 0 }`
  - `handshake::negotiate_version(local: ProtocolVersion, remote: ProtocolVersion) -> Result<ProtocolVersion, VersionMismatch>`
  - `handshake::confirm_version(local: ProtocolVersion, chosen: ProtocolVersion) -> Result<ProtocolVersion, VersionMismatch>`
  - `handshake::agreed_capabilities(local: &[Capability], remote: &[i32]) -> Vec<Capability>`
  - `handshake::confirm_capabilities(local: &[Capability], chosen: &[i32]) -> Result<Vec<Capability>, UnofferedCapability>`
  - `struct handshake::VersionMismatch { local: ProtocolVersion, remote: ProtocolVersion }`, `struct handshake::UnofferedCapability { value: i32 }`, both `Display` + `Error`

- [ ] **Step 1: Write the failing tests**

Create `crates/display-protocol/src/handshake.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn version(major: u32, minor: u32) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    #[test]
    fn a_session_runs_at_the_older_peers_minor() {
        assert_eq!(
            negotiate_version(version(1, 4), version(1, 2)),
            Ok(version(1, 2))
        );
        assert_eq!(
            negotiate_version(version(1, 2), version(1, 4)),
            Ok(version(1, 2))
        );
    }

    #[test]
    fn differing_majors_leave_nothing_to_negotiate() {
        assert!(negotiate_version(version(1, 0), version(2, 0)).is_err());
    }

    #[test]
    fn a_revision_newer_than_this_build_claimed_is_not_one_it_can_be_held_to() {
        assert!(confirm_version(version(1, 2), version(1, 3)).is_err());
        assert_eq!(confirm_version(version(1, 2), version(1, 1)), Ok(version(1, 1)));
    }

    #[test]
    fn only_capabilities_both_peers_have_are_agreed() {
        let agreed = agreed_capabilities(
            &[Capability::CursorStream, Capability::DynamicResolution],
            &[i32::from(Capability::DynamicResolution)],
        );

        assert_eq!(agreed, vec![Capability::DynamicResolution]);
    }

    #[test]
    fn a_capability_this_build_has_never_heard_of_is_dropped() {
        let agreed = agreed_capabilities(&[Capability::CursorStream], &[9999]);

        assert!(agreed.is_empty());
    }

    #[test]
    fn a_peer_that_agreed_on_something_never_offered_is_refused() {
        let error = confirm_capabilities(
            &[Capability::CursorStream],
            &[i32::from(Capability::DynamicResolution)],
        )
        .expect_err("a capability this side did not offer");

        assert_eq!(error.value, i32::from(Capability::DynamicResolution));
    }

    #[test]
    fn confirming_accepts_what_was_offered() {
        assert_eq!(
            confirm_capabilities(
                &[Capability::CursorStream, Capability::DynamicResolution],
                &[i32::from(Capability::CursorStream)]
            ),
            Ok(vec![Capability::CursorStream])
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol handshake`
Expected: FAIL — the module does not compile.

- [ ] **Step 3: Write the implementation**

Above the tests in `crates/display-protocol/src/handshake.rs`:

```rust
//! What the two peers have to agree on before anything else is sent.
//!
//! Two agreements, answering different questions. The version says whether the
//! peers can talk at all and which revision of the schema they use; the
//! capabilities say which optional parts of that revision are worth sending.
//!
//! These rules are `vmlord-agent-protocol`'s rules, deliberately re-stated
//! here rather than depended on. Sharing twenty lines would tie two contracts
//! that have to be versioned apart: a display major must not drag the agent's
//! schema with it.

use std::{error::Error, fmt};

use crate::v1::{Capability, ProtocolVersion};

/// The revision of the schema this build implements.
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

impl ProtocolVersion {
    /// The revision this build implements.
    #[must_use]
    pub const fn current() -> Self {
        CURRENT_VERSION
    }
}

/// Settles on the revision both peers can speak.
///
/// # Errors
///
/// [`VersionMismatch`] if the majors differ. A major bump means an existing
/// message changed meaning, so there is nothing to negotiate down to and the
/// session is refused with
/// [`ErrorCode::UnsupportedVersion`](crate::v1::ErrorCode::UnsupportedVersion).
pub fn negotiate_version(
    local: ProtocolVersion,
    remote: ProtocolVersion,
) -> Result<ProtocolVersion, VersionMismatch> {
    if local.major != remote.major {
        return Err(VersionMismatch { local, remote });
    }

    Ok(ProtocolVersion {
        major: local.major,
        minor: local.minor.min(remote.minor),
    })
}

/// Checks the revision a peer answered a hello with.
///
/// # Errors
///
/// [`VersionMismatch`] if the majors differ or `chosen` is newer than
/// `local` -- a revision this side never claimed to speak is one it cannot be
/// held to, and there is no third round in this handshake.
pub fn confirm_version(
    local: ProtocolVersion,
    chosen: ProtocolVersion,
) -> Result<ProtocolVersion, VersionMismatch> {
    if local.major != chosen.major || chosen.minor > local.minor {
        return Err(VersionMismatch {
            local,
            remote: chosen,
        });
    }

    Ok(chosen)
}

/// The capabilities both peers have, in `local`'s order.
///
/// `remote` is raw wire values because that is what the generated field holds:
/// a newer peer may announce a capability this build has never heard of, and
/// the only sane reading of an unknown number is that it is not something both
/// sides have. Unspecified is dropped for the same reason -- it is proto3's
/// "absent", not a capability.
#[must_use]
pub fn agreed_capabilities(local: &[Capability], remote: &[i32]) -> Vec<Capability> {
    local
        .iter()
        .copied()
        .filter(|capability| *capability != Capability::Unspecified)
        .filter(|capability| remote.contains(&i32::from(*capability)))
        .collect()
}

/// Checks the capabilities a peer answered a hello with.
///
/// # Errors
///
/// [`UnofferedCapability`] carrying the first value this side never offered.
/// A peer claiming otherwise is claiming the session may carry messages
/// nothing here answers, which is worse than a session without that
/// capability.
pub fn confirm_capabilities(
    local: &[Capability],
    chosen: &[i32],
) -> Result<Vec<Capability>, UnofferedCapability> {
    chosen
        .iter()
        .map(|value| {
            Capability::try_from(*value)
                .ok()
                .filter(|capability| *capability != Capability::Unspecified)
                .filter(|capability| local.contains(capability))
                .ok_or(UnofferedCapability { value: *value })
        })
        .collect()
}

/// A peer that agreed on a capability this side never announced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnofferedCapability {
    /// The wire value, which may be one this build cannot name.
    pub value: i32,
}

impl fmt::Display for UnofferedCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the peer agreed on capability {}, which this build did not offer",
            self.value
        )
    }
}

impl Error for UnofferedCapability {}

/// Two peers whose major versions leave nothing to talk about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionMismatch {
    /// What this build implements.
    pub local: ProtocolVersion,
    /// What the peer offered or chose.
    pub remote: ProtocolVersion,
}

impl fmt::Display for VersionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "this build speaks display protocol {}.{} and the peer speaks {}.{}",
            self.local.major, self.local.minor, self.remote.major, self.remote.minor
        )
    }
}

impl Error for VersionMismatch {}
```

Add `pub mod handshake;` to `crates/display-protocol/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol handshake`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Negotiate the display protocol revision and capabilities"
```

---

### Task 5: Keys, transcript and tags

The cryptographic core, in one module so that both ends compute it with the same functions and cannot disagree about what is signed. The session key is derived from the per-VM secret; the transcript hashes the hello payloads as they crossed the wire; the channel keys hang off both.

**Files:**
- Create: `crates/display-protocol/src/keys.rs`
- Modify: `crates/display-protocol/src/lib.rs` (add `pub mod keys;`)
- Test: `crates/display-protocol/src/keys.rs` `mod tests`

**Interfaces:**
- Consumes: `record::Channel` from task 2.
- Produces:
  - `keys::SECRET_LEN: usize` = 32, `keys::NONCE_LEN: usize` = 32, `keys::SESSION_ID_LEN: usize` = 16, `keys::TAG_LEN: usize` = 32
  - `struct keys::Secret` — no `Debug`, no `Display` — with `Secret::generate() -> Secret`, `Secret::from_base64(&str) -> Result<Secret, SecretError>`, `Secret::to_base64(&self) -> zeroize::Zeroizing<String>`
  - `struct keys::SessionKey` and `struct keys::ChannelKey`, both opaque
  - `keys::session_key(secret: &Secret, session_id: &[u8; SESSION_ID_LEN], host_nonce: &[u8; NONCE_LEN], guest_nonce: &[u8; NONCE_LEN]) -> SessionKey`
  - `struct keys::Transcript` with `Transcript::new() -> Transcript`, `Transcript::record(&mut self, payload: &[u8])`, `Transcript::finish(&self) -> [u8; 32]`
  - `keys::channel_key(session: &SessionKey, transcript: &[u8; 32], channel: Channel) -> ChannelKey`
  - `struct keys::Tag([u8; TAG_LEN])` with `Tag::from_wire(&[u8]) -> Result<Tag, WrongLength>`, `Tag::as_bytes(&self) -> &[u8; TAG_LEN]`
  - `keys::control_tag(session: &SessionKey, role: Role, transcript: &[u8; 32]) -> Tag`
  - `keys::channel_tag(key: &ChannelKey, role: Role, channel: Channel, host_nonce: &[u8; NONCE_LEN], guest_nonce: &[u8; NONCE_LEN]) -> Tag`
  - `enum keys::Role { Host, Guest }` with `Role::label(self) -> &'static [u8]` returning `b"client"` for `Host` and `b"server"` for `Guest`
  - `keys::verify(expected: &Tag, offered: &Tag) -> bool` — constant time
  - `keys::random_bytes<const N: usize>() -> [u8; N]`
  - `struct keys::SecretError`, `struct keys::WrongLength { what: &'static str, len: usize }`, both `Display` + `Error`

- [ ] **Step 1: Write the failing tests**

Create `crates/display-protocol/src/keys.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Secret {
        Secret::from_base64(&Secret::generate().to_base64()).expect("what generate produced")
    }

    fn transcript_of(payloads: &[&[u8]]) -> [u8; 32] {
        let mut transcript = Transcript::new();
        for payload in payloads {
            transcript.record(payload);
        }
        transcript.finish()
    }

    #[test]
    fn a_secret_survives_the_form_it_is_delivered_in() {
        let secret = Secret::generate();
        let text = secret.to_base64();

        let read_back = Secret::from_base64(&format!("  {}\n", *text)).expect("surrounding space");

        assert_eq!(
            session_key(&secret, &[1; 16], &[2; 32], &[3; 32]).expose(),
            session_key(&read_back, &[1; 16], &[2; 32], &[3; 32]).expose()
        );
    }

    #[test]
    fn a_secret_that_is_not_thirty_two_bytes_is_refused() {
        assert!(Secret::from_base64("c2hvcnQ=").is_err());
        assert!(Secret::from_base64("not base64!").is_err());
    }

    #[test]
    fn a_session_key_changes_with_every_input() {
        let secret = secret();
        let base = session_key(&secret, &[1; 16], &[2; 32], &[3; 32]);

        assert_ne!(base.expose(), session_key(&secret, &[9; 16], &[2; 32], &[3; 32]).expose());
        assert_ne!(base.expose(), session_key(&secret, &[1; 16], &[9; 32], &[3; 32]).expose());
        assert_ne!(base.expose(), session_key(&secret, &[1; 16], &[2; 32], &[9; 32]).expose());
        assert_ne!(base.expose(), session_key(&Secret::generate(), &[1; 16], &[2; 32], &[3; 32]).expose());
    }

    #[test]
    fn a_transcript_depends_on_the_order_and_the_boundaries_of_what_it_recorded() {
        assert_ne!(
            transcript_of(&[b"client", b"server"]),
            transcript_of(&[b"server", b"client"])
        );
        // The length prefix is what keeps two records from sliding into one.
        assert_ne!(
            transcript_of(&[b"ab", b"c"]),
            transcript_of(&[b"a", b"bc"])
        );
    }

    #[test]
    fn the_two_roles_sign_the_same_transcript_differently() {
        let key = session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]);
        let transcript = transcript_of(&[b"client hello", b"server hello"]);

        assert_ne!(
            control_tag(&key, Role::Host, &transcript).as_bytes(),
            control_tag(&key, Role::Guest, &transcript).as_bytes()
        );
    }

    #[test]
    fn a_tag_is_only_good_for_the_transcript_it_was_made_over() {
        let key = session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]);
        let mine = control_tag(&key, Role::Guest, &transcript_of(&[b"hello"]));
        let theirs = control_tag(&key, Role::Guest, &transcript_of(&[b"hell0"]));

        assert!(verify(&mine, &mine));
        assert!(!verify(&mine, &theirs));
    }

    #[test]
    fn a_channel_key_is_bound_to_the_transcript_and_the_channel() {
        let key = session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]);
        let transcript = transcript_of(&[b"client hello", b"server hello"]);

        let frame = channel_key(&key, &transcript, Channel::Frame);
        let input = channel_key(&key, &transcript, Channel::Input);
        let other_session = channel_key(&key, &transcript_of(&[b"elsewhere"]), Channel::Frame);

        assert_ne!(frame.expose(), input.expose());
        assert_ne!(frame.expose(), other_session.expose());
    }

    #[test]
    fn a_channel_tag_covers_both_nonces() {
        let key = channel_key(
            &session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]),
            &transcript_of(&[b"hello"]),
            Channel::Frame,
        );

        let tag = channel_tag(&key, Role::Guest, Channel::Frame, &[4; 32], &[5; 32]);

        assert!(!verify(
            &tag,
            &channel_tag(&key, Role::Guest, Channel::Frame, &[4; 32], &[6; 32])
        ));
        assert!(!verify(
            &tag,
            &channel_tag(&key, Role::Guest, Channel::Input, &[4; 32], &[5; 32])
        ));
    }

    #[test]
    fn a_tag_of_the_wrong_length_is_refused_rather_than_padded() {
        assert!(Tag::from_wire(&[0u8; 31]).is_err());
        assert!(Tag::from_wire(&[0u8; 32]).is_ok());
    }
}
```

Note for the implementer: `SessionKey::expose` and `ChannelKey::expose` are `#[cfg(test)]`-only accessors returning `&[u8; 32]`. They exist so these tests can compare keys and must not be public API — the guest broker hands a `ChannelKey` to the session process as a value, not as bytes.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol keys`
Expected: FAIL — the module does not compile.

- [ ] **Step 3: Write the implementation**

Above the tests in `crates/display-protocol/src/keys.rs`:

```rust
//! Proving that the peer on the other end of a display session holds the VM's
//! secret, and keying the channels that hang off that proof.
//!
//! The root of trust is the per-VM secret the agent protocol already
//! mints -- 32 bytes written into the seed as `/etc/vmlord/agent.secret`,
//! root-only. It never travels on this protocol and it never reaches the
//! unprivileged capture process: the privileged broker in the guest derives a
//! session key from it and hands only that on. Compromising the session
//! process costs one session, not the VM's identity.
//!
//! Both ends compute everything here with the same functions, which is what
//! keeps them from disagreeing about what is being signed.

use std::{error::Error, fmt};

use base64::{Engine, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::record::Channel;

/// The width of a secret, a session key and a channel key.
pub const SECRET_LEN: usize = 32;

/// The width of each side's handshake nonce.
pub const NONCE_LEN: usize = 32;

/// The width of the identifier that names a session across its three sockets.
pub const SESSION_ID_LEN: usize = 16;

/// The width of a tag, which is HMAC-SHA-256's output.
pub const TAG_LEN: usize = 32;

/// Separates this protocol's session keys from every other use of the secret.
const SESSION_DOMAIN: &[u8] = b"vmlord.display.v1.session";

/// Separates the transcript hash from any other SHA-256 in the system.
const TRANSCRIPT_DOMAIN: &[u8] = b"vmlord.display.v1.transcript";

/// Separates a channel key from the session key it comes from.
const CHANNEL_DOMAIN: &[u8] = b"vmlord.display.v1.channel";

/// Which end of a session a tag speaks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// VMLord, which connects.
    Host,
    /// The guest's display services, which listen.
    Guest,
}

impl Role {
    /// What this role's tags are labelled with.
    ///
    /// The two labels are what keep a tag one side produced from being
    /// replayed back at it as the other side's proof.
    #[must_use]
    pub fn label(self) -> &'static [u8] {
        match self {
            Self::Host => b"client",
            Self::Guest => b"server",
        }
    }
}

/// A VM's shared secret.
///
/// No `Debug` and no `Display`, by design: the one thing this type must never
/// do is print itself.
pub struct Secret(Zeroizing<[u8; SECRET_LEN]>);

impl Secret {
    /// Mints a secret from the operating system's random source.
    #[must_use]
    pub fn generate() -> Self {
        Self(Zeroizing::new(random_bytes()))
    }

    /// Reads a secret back from the form it is stored and delivered in.
    ///
    /// Surrounding whitespace is ignored, because the file this comes out of
    /// is a text file whose ends may have been written by something that ends
    /// lines.
    ///
    /// # Errors
    ///
    /// [`SecretError`] if the text is not base64 or does not decode to exactly
    /// [`SECRET_LEN`] bytes. Both mean the secret is truncated or is not one,
    /// and neither is recovered from by padding.
    pub fn from_base64(text: &str) -> Result<Self, SecretError> {
        let bytes = Zeroizing::new(STANDARD.decode(text.trim()).map_err(|_| SecretError)?);
        let bytes: [u8; SECRET_LEN] = bytes.as_slice().try_into().map_err(|_| SecretError)?;

        Ok(Self(Zeroizing::new(bytes)))
    }

    /// The secret as base64, which is how it is written to a file.
    #[must_use]
    pub fn to_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.0.as_slice()))
    }
}

/// The key one display session's proofs and channel keys are built on.
pub struct SessionKey(Zeroizing<[u8; SECRET_LEN]>);

impl SessionKey {
    #[cfg(test)]
    fn expose(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }
}

/// The key one channel of one session proves itself with.
pub struct ChannelKey(Zeroizing<[u8; SECRET_LEN]>);

impl ChannelKey {
    #[cfg(test)]
    fn expose(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }
}

/// Derives the session key both peers authenticate with.
///
/// The nonces are the salt and the session id is in the info, so a key is good
/// for one session and no other; a recorded tag is worthless the moment the
/// next session draws its nonces.
#[must_use]
pub fn session_key(
    secret: &Secret,
    session_id: &[u8; SESSION_ID_LEN],
    host_nonce: &[u8; NONCE_LEN],
    guest_nonce: &[u8; NONCE_LEN],
) -> SessionKey {
    let mut salt = [0u8; NONCE_LEN * 2];
    salt[..NONCE_LEN].copy_from_slice(host_nonce);
    salt[NONCE_LEN..].copy_from_slice(guest_nonce);

    let mut info = Vec::with_capacity(SESSION_DOMAIN.len() + SESSION_ID_LEN);
    info.extend_from_slice(SESSION_DOMAIN);
    info.extend_from_slice(session_id);

    let mut key = Zeroizing::new([0u8; SECRET_LEN]);
    Hkdf::<Sha256>::new(Some(&salt), secret.0.as_slice())
        .expand(&info, key.as_mut_slice())
        .expect("32 bytes is far below HKDF-SHA-256's output limit");

    SessionKey(key)
}

/// The running hash of the handshake, over the bytes as they crossed the wire.
///
/// Protobuf does not promise that the same message encodes to the same bytes
/// twice, so a transcript over a re-encoded message is one two correct
/// implementations can disagree about. Every payload is length-prefixed into
/// the hash, so that two records cannot slide into one.
pub struct Transcript(Sha256);

impl Transcript {
    /// Starts a transcript, domain-separated from every other SHA-256.
    #[must_use]
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TRANSCRIPT_DOMAIN);
        Self(hasher)
    }

    /// Adds one handshake payload, exactly as it appeared on the wire.
    pub fn record(&mut self, payload: &[u8]) {
        self.0.update(u32::try_from(payload.len()).unwrap_or(u32::MAX).to_le_bytes());
        self.0.update(payload);
    }

    /// The hash of everything recorded so far.
    #[must_use]
    pub fn finish(&self) -> [u8; 32] {
        self.0.clone().finalize().into()
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

/// Derives the key a frame or input channel proves itself with.
///
/// It depends on the transcript, which is why a socket cannot be carried in
/// from another session or offered by a process that did not take part in the
/// control handshake.
#[must_use]
pub fn channel_key(session: &SessionKey, transcript: &[u8; 32], channel: Channel) -> ChannelKey {
    let mut info = Vec::with_capacity(CHANNEL_DOMAIN.len() + 32 + 1);
    info.extend_from_slice(CHANNEL_DOMAIN);
    info.extend_from_slice(transcript);
    info.push(channel.as_wire());

    let mut key = Zeroizing::new([0u8; SECRET_LEN]);
    Hkdf::<Sha256>::from_prk(session.0.as_slice())
        .expect("a 32-byte pseudo-random key is long enough for SHA-256")
        .expand(&info, key.as_mut_slice())
        .expect("32 bytes is far below HKDF-SHA-256's output limit");

    ChannelKey(key)
}

/// The proof that a peer holds the key a tag was computed under.
///
/// A tag says nothing about the key and is worthless once its session is over,
/// so unlike a [`Secret`] it may be copied and printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag([u8; TAG_LEN]);

impl Tag {
    /// The bytes to put in a `ServerAuth`, `ClientAuth`, `ChannelAck` or
    /// `ChannelAuth`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; TAG_LEN] {
        &self.0
    }

    /// Reads a tag out of a message that arrived on the wire.
    ///
    /// # Errors
    ///
    /// [`WrongLength`] for anything other than [`TAG_LEN`] bytes.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WrongLength> {
        Ok(Self(bytes.try_into().map_err(|_| WrongLength {
            what: "tag",
            len: bytes.len(),
        })?))
    }
}

/// The tag `role` puts on the control handshake's transcript.
#[must_use]
pub fn control_tag(session: &SessionKey, role: Role, transcript: &[u8; 32]) -> Tag {
    let mut mac = Hmac::<Sha256>::new_from_slice(session.0.as_slice())
        .expect("HMAC accepts a key of any length");
    mac.update(role.label());
    mac.update(transcript);

    Tag(mac.finalize().into_bytes().into())
}

/// The tag `role` puts on a frame or input channel's exchange.
#[must_use]
pub fn channel_tag(
    key: &ChannelKey,
    role: Role,
    channel: Channel,
    host_nonce: &[u8; NONCE_LEN],
    guest_nonce: &[u8; NONCE_LEN],
) -> Tag {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.0.as_slice()).expect("HMAC accepts a key of any length");
    mac.update(role.label());
    mac.update(&[channel.as_wire()]);
    mac.update(host_nonce);
    mac.update(guest_nonce);

    Tag(mac.finalize().into_bytes().into())
}

/// Compares two tags without leaking where they differ.
///
/// An early return on the first differing byte is how a tag gets forged a byte
/// at a time.
#[must_use]
pub fn verify(expected: &Tag, offered: &Tag) -> bool {
    expected.0.ct_eq(&offered.0).into()
}

/// Draws bytes from the operating system's random source.
///
/// # Panics
///
/// If the platform's random source fails. There is no session to open without
/// a fresh nonce, and continuing with a predictable one is worse than
/// stopping.
#[must_use]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system has a random source");
    bytes
}

/// Text that is not a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretError;

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a display secret must be {SECRET_LEN} base64-encoded bytes"
        )
    }
}

impl Error for SecretError {}

/// A fixed-width field that arrived at another width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrongLength {
    /// Which field: `"tag"`, `"nonce"` or `"session id"`.
    pub what: &'static str,
    /// What arrived.
    pub len: usize,
}

impl fmt::Display for WrongLength {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a {} of {} bytes is the wrong width", self.what, self.len)
    }
}

impl Error for WrongLength {}
```

Add `pub mod keys;` to `crates/display-protocol/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol keys`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol Cargo.lock
git commit -m "TASK-118: Derive display session and channel keys"
```

---

### Task 6: The control handshake state machine

One machine, two roles, so that the guest half and the host half cannot drift over what goes into the transcript and in what order. It consumes a record and produces the next one; it holds no socket.

**Files:**
- Create: `crates/display-protocol/src/session.rs`
- Modify: `crates/display-protocol/src/lib.rs` (add `pub mod session;`)
- Test: `crates/display-protocol/src/session.rs` `mod tests`

**Interfaces:**
- Consumes: `record::{Channel, Header, Record}`, `keys::*`, `handshake::*`, `v1::*`.
- Produces:
  - `struct session::Offer { capabilities: Vec<Capability>, mode: Mode, width: u32, height: u32, tile_size: u32 }` — what a host asks for, and (`capabilities`, `modes`, `tile_sizes`, geometry) what a guest supports, through `struct session::Support { capabilities: Vec<Capability>, modes: Vec<Mode>, tile_sizes: Vec<u32>, width: u32, height: u32 }`
  - `struct session::Session` with:
    - `Session::host(secret: &Secret, offer: Offer) -> (Session, Record)` — the `Record` is the `ClientHello`
    - `Session::guest(secret: &Secret, support: Support) -> Session`
    - `Session::handle(&mut self, header: &Header, payload: &[u8]) -> Result<Outcome, SessionError>`
    - `Session::negotiated(&self) -> Option<&Negotiated>`
    - `Session::session_id(&self) -> &[u8; SESSION_ID_LEN]`
  - `struct session::Outcome { reply: Option<Record>, event: Event }`
  - `enum session::Event { Continue, ControlEstablished }` (task 7 adds `ChannelBound`)
  - `struct session::Negotiated { version: ProtocolVersion, capabilities: Vec<Capability>, mode: Mode, width: u32, height: u32, tile_size: u32 }`
  - `enum session::SessionError { Version(VersionMismatch), Capability(UnofferedCapability), Field(WrongLength), Decode(prost::DecodeError), Unexpected { channel: Channel, message_type: u16 }, BadTag, UnsupportedMode(Mode), NoCommonTileSize }` with `Display` + `Error` and `SessionError::code(&self) -> ErrorCode`

- [ ] **Step 1: Write the failing tests**

Create `crates/display-protocol/src/session.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> Offer {
        Offer {
            capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
    }

    fn support() -> Support {
        Support {
            capabilities: vec![Capability::CursorStream],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        }
    }

    /// Runs the four-record handshake between a host and a guest that hold the
    /// same secret, returning both machines.
    fn handshake(secret: &Secret, offer: Offer, support: Support) -> (Session, Session) {
        let (mut host, client_hello) = Session::host(secret, offer);
        let mut guest = Session::guest(secret, support);

        let mut next = client_hello;
        let mut turn_is_guest = true;
        loop {
            let outcome = if turn_is_guest {
                guest.handle(&next.header, &next.payload).expect("a well-formed record")
            } else {
                host.handle(&next.header, &next.payload).expect("a well-formed record")
            };

            match outcome.reply {
                Some(reply) => {
                    next = reply;
                    turn_is_guest = !turn_is_guest;
                }
                None => break,
            }
        }

        (host, guest)
    }

    #[test]
    fn a_handshake_leaves_both_ends_agreeing_on_the_session() {
        let (host, guest) = handshake(&Secret::generate(), offer(), support());

        let host_side = host.negotiated().expect("an established host session");
        let guest_side = guest.negotiated().expect("an established guest session");

        assert_eq!(host_side.version, ProtocolVersion::current());
        assert_eq!(host_side.capabilities, vec![Capability::CursorStream]);
        assert_eq!(host_side.mode, Mode::Desktop);
        assert_eq!(host_side.width, 1920);
        assert_eq!(host_side.tile_size, 32);
        assert_eq!(host_side.capabilities, guest_side.capabilities);
        assert_eq!(host_side.tile_size, guest_side.tile_size);
        assert_eq!(host.session_id(), guest.session_id());
    }

    #[test]
    fn the_host_hears_the_guests_proof_before_it_sends_its_own() {
        let secret = Secret::generate();
        let (mut host, client_hello) = Session::host(&secret, offer());
        let mut guest = Session::guest(&secret, support());

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        assert_eq!(server_hello.header.message_type, ControlRecord::ServerHello as u16);

        let server_auth = guest.pending_auth().expect("the guest's proof, queued behind its hello");
        assert_eq!(server_auth.header.message_type, ControlRecord::ServerAuth as u16);

        // The host has not established anything yet: it has heard a hello.
        host.handle(&server_hello.header, &server_hello.payload)
            .expect("a well-formed server hello");
        assert!(host.negotiated().is_none());

        let outcome = host
            .handle(&server_auth.header, &server_auth.payload)
            .expect("a valid guest proof");
        assert_eq!(outcome.event, Event::ControlEstablished);
        assert_eq!(
            outcome.reply.expect("the host's proof").header.message_type,
            ControlRecord::ClientAuth as u16
        );
    }

    #[test]
    fn a_guest_that_holds_another_vms_secret_cannot_prove_itself() {
        let (mut host, client_hello) = Session::host(&Secret::generate(), offer());
        let mut guest = Session::guest(&Secret::generate(), support());

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        let server_auth = guest.pending_auth().expect("the guest's proof");

        host.handle(&server_hello.header, &server_hello.payload)
            .expect("a well-formed server hello");

        assert!(matches!(
            host.handle(&server_auth.header, &server_auth.payload),
            Err(SessionError::BadTag)
        ));
        assert!(host.negotiated().is_none());
    }

    #[test]
    fn a_host_that_cannot_prove_itself_leaves_the_guest_unestablished() {
        let secret = Secret::generate();
        let (_, client_hello) = Session::host(&secret, offer());
        let mut guest = Session::guest(&secret, support());

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        let _ = guest.pending_auth().expect("the guest's proof");

        let forged = Record::new(
            Channel::Control,
            ControlRecord::ClientAuth as u16,
            3,
            0,
            0,
            ClientAuth { tag: vec![0u8; TAG_LEN] }.encode_to_vec(),
        );
        let _ = server_hello;

        assert!(matches!(
            guest.handle(&forged.header, &forged.payload),
            Err(SessionError::BadTag)
        ));
        assert!(guest.negotiated().is_none());
    }

    #[test]
    fn a_mode_the_guest_does_not_support_is_refused_with_its_own_code() {
        let secret = Secret::generate();
        let mut wanted = offer();
        wanted.mode = Mode::Motion;

        let (_, client_hello) = Session::host(&secret, wanted);
        let mut guest = Session::guest(&secret, support());

        let error = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect_err("a mode this guest does not have");

        assert!(matches!(error, SessionError::UnsupportedMode(Mode::Motion)));
        assert_eq!(error.code(), ErrorCode::UnsupportedMode);
    }

    #[test]
    fn auto_resolves_to_desktop_because_that_is_all_the_mvp_guest_has() {
        let secret = Secret::generate();
        let mut wanted = offer();
        wanted.mode = Mode::Auto;

        let (host, guest) = handshake(&secret, wanted, support());

        assert_eq!(host.negotiated().expect("established").mode, Mode::Desktop);
        assert_eq!(guest.negotiated().expect("established").mode, Mode::Desktop);
    }

    #[test]
    fn a_tile_size_the_guest_does_not_have_falls_back_to_one_it_does() {
        let secret = Secret::generate();
        let mut wanted = offer();
        wanted.tile_size = 128;

        let (host, _) = handshake(&secret, wanted, support());

        assert_eq!(host.negotiated().expect("established").tile_size, 32);
    }

    #[test]
    fn a_record_out_of_turn_is_refused() {
        let secret = Secret::generate();
        let (mut host, _) = Session::host(&secret, offer());

        let ping = Record::new(
            Channel::Control,
            ControlRecord::Ping as u16,
            1,
            0,
            0,
            Ping { token: 1 }.encode_to_vec(),
        );

        assert!(matches!(
            host.handle(&ping.header, &ping.payload),
            Err(SessionError::Unexpected { .. })
        ));
    }

    #[test]
    fn a_payload_that_is_not_the_message_its_header_names_is_refused() {
        let secret = Secret::generate();
        let mut guest = Session::guest(&secret, support());

        let nonsense = Record::new(
            Channel::Control,
            ControlRecord::ClientHello as u16,
            0,
            0,
            0,
            vec![0xFF; 16],
        );

        assert!(matches!(
            guest.handle(&nonsense.header, &nonsense.payload),
            Err(SessionError::Decode(_))
        ));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol session`
Expected: FAIL — the module does not compile.

- [ ] **Step 3: Write the implementation**

Write `crates/display-protocol/src/session.rs` above the tests. Its shape:

```rust
//! The states a display session moves through, without a socket in sight.
//!
//! One machine with two roles rather than a set of functions each end calls in
//! its own order. The agent protocol can leave its sequence to its callers
//! because its handshake is one exchange over one socket; this one is three
//! sockets, two directions of proof, a transcript hash and a channel binding,
//! and if the guest services (#115) and the viewer (#117) each wrote their own
//! half they would drift over exactly what must not drift: what goes into the
//! hash, and in what order.

use std::{error::Error, fmt};

use prost::Message;

use crate::{
    handshake::{
        self, CURRENT_VERSION, UnofferedCapability, VersionMismatch,
    },
    keys::{
        self, ChannelKey, NONCE_LEN, Role, SESSION_ID_LEN, Secret, SessionKey, TAG_LEN, Tag,
        Transcript, WrongLength,
    },
    record::{Channel, Header, Record},
    v1::{
        Capability, ClientAuth, ClientHello, ControlRecord, ErrorCode, Mode, Ping,
        ProtocolVersion, ServerAuth, ServerHello,
    },
};
```

The rules the implementation must follow, each of which a test above pins:

1. `Session::host` draws a `session_id` and a `host_nonce` with `keys::random_bytes`, encodes a `ClientHello`, records its payload in the `Transcript`, and returns the record with `sequence` 0 on `Channel::Control` and `generation` 0.
2. `Session::guest` holds the secret and the `Support`, and starts in a state that accepts only a `ClientHello`.
3. On `ClientHello` the guest: negotiates the version (`handshake::negotiate_version`), intersects the capabilities (`handshake::agreed_capabilities`), resolves the mode (`Mode::Auto` becomes the first mode it supports, which for the MVP guest is `Mode::Desktop`; a mode it does not support is `SessionError::UnsupportedMode`), picks the tile size (the requested one if supported, otherwise the largest supported one that is not larger than the requested, otherwise the first supported — a guest with no tile sizes at all is `SessionError::NoCommonTileSize`), records the `ClientHello` payload in its transcript, builds and records its `ServerHello`, derives the session key, and queues a `ServerAuth` behind the hello. `handle` returns the `ServerHello` as `reply`; the queued proof is taken with `pending_auth()`.
4. `Session::pending_auth(&mut self) -> Option<Record>` returns the queued `ServerAuth` once, because two records leave the guest back-to-back and `Outcome` carries one reply. Sequence numbers on the control channel count up from 0 per direction.
5. On `ServerHello` the host confirms the version (`confirm_version`) and the capabilities (`confirm_capabilities`), reads the guest nonce (`WrongLength` if it is not `NONCE_LEN`), records the payload, and derives the session key. It establishes nothing and replies nothing.
6. On `ServerAuth` the host recomputes `keys::control_tag(&session_key, Role::Guest, &transcript)`, compares with `keys::verify` (`SessionError::BadTag` on failure, and the session stays unestablished), then replies `ClientAuth` with its own tag under `Role::Host` and returns `Event::ControlEstablished`.
7. On `ClientAuth` the guest verifies the host's tag the same way and returns `Event::ControlEstablished` with no reply. That `None` reply is what ends the loop in the tests' `handshake` helper.
8. The transcript covers the `ClientHello` and `ServerHello` payloads only — the two records the tags are computed over — and is finished once, when the session key is derived.
9. Any record whose `message_type` is not what the current state expects is `SessionError::Unexpected { channel, message_type }`. A payload that does not decode is `SessionError::Decode`.
10. `SessionError::code` maps: `Version` to `ErrorCode::UnsupportedVersion`, `Capability` and `Unexpected` and `Decode` and `Field` to `ErrorCode::MalformedRecord`, `BadTag` to `ErrorCode::Unauthenticated`, `UnsupportedMode` to `ErrorCode::UnsupportedMode`, `NoCommonTileSize` to `ErrorCode::ResolutionRejected`.
11. `Negotiated` is stored on both ends when they establish, and `negotiated()` returns `None` before that. Both ends' `Negotiated` must be equal — the tests compare the fields that matter.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol session`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Add the display control handshake"
```

---

### Task 7: Channel binding and generations

The frame and input sockets prove they belong to the session the control channel established, and a reconnection of either is a generation the other side can tell apart from what is still in flight.

**Files:**
- Modify: `crates/display-protocol/src/session.rs` (append; `mod tests` grows)
- Test: `crates/display-protocol/src/session.rs` `mod tests`

**Interfaces:**
- Consumes: everything task 6 produced.
- Produces:
  - `Session::open_channel(&mut self, channel: Channel) -> Result<Record, SessionError>` — host only; the record is a `ChannelHello` at the session's current generation for that channel
  - `Session::reconnect_channel(&mut self, channel: Channel) -> Result<Record, SessionError>` — host only; bumps the channel's generation and returns a fresh `ChannelHello`
  - `Session::generation(&self, channel: Channel) -> u32`
  - `Session::accept(&self, header: &Header) -> Result<(), SessionError>` — rejects a record whose generation is not the channel's current one
  - `Session::channel_key(&self, channel: Channel) -> Option<&ChannelKey>` — for the guest broker, which hands it to the process that owns that socket
  - `Event::ChannelBound(Channel)` added to `enum Event`
  - `SessionError::UnknownSession` and `SessionError::StaleGeneration { channel: Channel, expected: u32, found: u32 }` added, mapping to `ErrorCode::UnknownSession` and `ErrorCode::ChannelBindingFailed`
  - `SessionError::NotEstablished` added, mapping to `ErrorCode::Unauthenticated`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `crates/display-protocol/src/session.rs`:

```rust
    /// Drives a frame or input channel's three-record exchange to completion.
    fn bind(host: &mut Session, guest: &mut Session, channel: Channel) {
        let hello = host.open_channel(channel).expect("an established session");

        let ack = guest
            .handle(&hello.header, &hello.payload)
            .expect("a well-formed channel hello")
            .reply
            .expect("a channel ack");

        let outcome = host.handle(&ack.header, &ack.payload).expect("a valid guest proof");
        let auth = outcome.reply.expect("the host's channel proof");
        assert_eq!(outcome.event, Event::ChannelBound(channel));

        let outcome = guest.handle(&auth.header, &auth.payload).expect("a valid host proof");
        assert_eq!(outcome.event, Event::ChannelBound(channel));
        assert!(outcome.reply.is_none());
    }

    #[test]
    fn a_channel_binds_to_the_session_the_control_handshake_established() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());

        bind(&mut host, &mut guest, Channel::Frame);
        bind(&mut host, &mut guest, Channel::Input);

        assert!(host.channel_key(Channel::Frame).is_some());
        assert!(guest.channel_key(Channel::Input).is_some());
    }

    #[test]
    fn a_channel_offered_before_the_control_handshake_is_refused() {
        let (mut host, _) = Session::host(&Secret::generate(), offer());

        assert!(matches!(
            host.open_channel(Channel::Frame),
            Err(SessionError::NotEstablished)
        ));
    }

    #[test]
    fn a_channel_hello_naming_another_session_is_refused() {
        let secret = Secret::generate();
        let (mut host, mut guest) = handshake(&secret, offer(), support());
        let (mut other_host, _) = {
            let (host, hello) = Session::host(&secret, offer());
            (host, hello)
        };
        let _ = &mut other_host;

        let mut hello = host.open_channel(Channel::Frame).expect("an established session");
        let mut message = ChannelHello::decode(hello.payload.as_slice()).expect("what was built");
        message.session_id = vec![0xAA; SESSION_ID_LEN];
        hello = Record::new(
            Channel::Frame,
            FrameRecord::ChannelHello as u16,
            0,
            0,
            0,
            message.encode_to_vec(),
        );

        let error = guest
            .handle(&hello.header, &hello.payload)
            .expect_err("a hello for a session this guest never opened");

        assert!(matches!(error, SessionError::UnknownSession));
        assert_eq!(error.code(), ErrorCode::UnknownSession);
    }

    #[test]
    fn a_channel_key_from_another_session_does_not_bind() {
        let secret = Secret::generate();
        let (mut host, _) = handshake(&secret, offer(), support());
        let (_, mut other_guest) = handshake(&secret, offer(), support());

        let hello = host.open_channel(Channel::Frame).expect("an established session");
        // The other guest is a different session with a different transcript,
        // so even holding the same VM secret it cannot answer this hello --
        // and it refuses it by session id before the tags are even reached.
        assert!(matches!(
            other_guest.handle(&hello.header, &hello.payload),
            Err(SessionError::UnknownSession)
        ));
    }

    #[test]
    fn a_forged_channel_ack_does_not_bind_the_channel() {
        let (mut host, _) = handshake(&Secret::generate(), offer(), support());
        let hello = host.open_channel(Channel::Frame).expect("an established session");
        let _ = hello;

        let forged = Record::new(
            Channel::Frame,
            FrameRecord::ChannelAck as u16,
            0,
            0,
            0,
            ChannelAck {
                nonce: vec![7u8; NONCE_LEN],
                tag: vec![0u8; TAG_LEN],
            }
            .encode_to_vec(),
        );

        assert!(matches!(
            host.handle(&forged.header, &forged.payload),
            Err(SessionError::BadTag)
        ));
        assert!(host.channel_key(Channel::Frame).is_none());
    }

    #[test]
    fn a_reconnected_channel_runs_at_the_next_generation() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());
        bind(&mut host, &mut guest, Channel::Frame);

        assert_eq!(host.generation(Channel::Frame), 0);

        let hello = host.reconnect_channel(Channel::Frame).expect("an established session");
        assert_eq!(host.generation(Channel::Frame), 1);
        assert_eq!(hello.header.generation, 1);
    }

    #[test]
    fn a_record_from_a_generation_that_has_been_replaced_is_rejected() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());
        bind(&mut host, &mut guest, Channel::Frame);

        let stale = Record::new(Channel::Frame, FrameRecord::TileDelta as u16, 9, 8, 0, vec![1, 2, 3]);
        assert!(host.accept(&stale.header).is_ok());

        let _ = host.reconnect_channel(Channel::Frame).expect("an established session");

        let error = host.accept(&stale.header).expect_err("a record from the previous connection");
        assert!(matches!(
            error,
            SessionError::StaleGeneration {
                channel: Channel::Frame,
                expected: 1,
                found: 0
            }
        ));
    }

    #[test]
    fn the_control_channel_has_no_generations() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());
        bind(&mut host, &mut guest, Channel::Frame);
        let _ = host.reconnect_channel(Channel::Frame).expect("an established session");

        let ping = Record::new(Channel::Control, ControlRecord::Ping as u16, 4, 0, 0, Ping { token: 1 }.encode_to_vec());

        assert!(host.accept(&ping.header).is_ok());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol session`
Expected: FAIL — `open_channel`, `bind`, `accept` and the new variants are not defined.

- [ ] **Step 3: Write the implementation**

Extend `Session` with the rules below. Each is pinned by a test above.

1. `open_channel` and `reconnect_channel` are host-only and require `negotiated().is_some()`, otherwise `SessionError::NotEstablished`. `Channel::Control` is `SessionError::Unexpected` — it is not a channel that binds.
2. Both draw a fresh host nonce, store it against the channel, and build a `ChannelHello { session_id, channel: channel.as_wire() as u32, generation, nonce }` as a record on that channel with `sequence` 0 and the channel's `generation` in the header. `reconnect_channel` increments the generation first; `open_channel` leaves it as it is (0 the first time).
3. The guest, on a `ChannelHello`, refuses a `session_id` that is not its own with `SessionError::UnknownSession`, and one whose `channel` field disagrees with the record header with `SessionError::Unexpected`. It derives `keys::channel_key(&session_key, &transcript, channel)`, draws its own nonce, and replies `ChannelAck { nonce, tag }` where the tag is `keys::channel_tag(&key, Role::Guest, channel, &host_nonce, &guest_nonce)`. It stores the channel key and adopts the hello's generation. `Event::Continue`.
4. The host, on a `ChannelAck`, derives the same channel key, recomputes the guest's tag and compares it with `keys::verify`. On failure it is `SessionError::BadTag` and no key is stored. On success it stores the key, replies `ChannelAuth` with its own tag under `Role::Host`, and returns `Event::ChannelBound(channel)`.
5. The guest, on a `ChannelAuth`, verifies the host's tag the same way and returns `Event::ChannelBound(channel)` with no reply.
6. `accept` compares `header.generation` against the channel's current generation for the frame and input channels and returns `SessionError::StaleGeneration` on a mismatch. The control channel is exempt: it has no generations, because losing it ends the session rather than reconnecting a channel within one.
7. `channel_key` returns the stored key, which is `None` until that channel is bound.

The doc comment on `reconnect_channel` must say what the spec says: a frame channel that reconnects owes a `StreamConfig` and a `Keyframe` before any delta, and an input channel that reconnects owes a `ReleaseAll` — this crate cannot enforce either, and naming the obligation where the reconnection happens is the next best thing.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol session`
Expected: PASS, 17 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Bind the display frame and input channels to their session"
```

---

### Task 8: Golden vectors

The checked-in bytes of a complete handshake and one record of each type, so that an unintended change to the format fails a test instead of a VM. The handshake is run with a fixed secret and fixed nonces, which the crate allows only in tests.

**Files:**
- Create: `crates/display-protocol/tests/golden.rs`
- Create: `crates/display-protocol/tests/golden/handshake.bin` (generated in step 4)
- Create: `crates/display-protocol/tests/golden/records.bin` (generated in step 4)
- Modify: `crates/display-protocol/src/session.rs` (add the test-only deterministic constructors)

**Interfaces:**
- Consumes: everything up to task 7.
- Produces: `Session::host_with_randomness(secret: &Secret, offer: Offer, session_id: [u8; SESSION_ID_LEN], nonce: [u8; NONCE_LEN]) -> (Session, Record)` and `Session::guest_with_randomness(secret: &Secret, support: Support, nonce: [u8; NONCE_LEN]) -> Session`, both `#[doc(hidden)]` and both documented as existing for golden vectors alone.

- [ ] **Step 1: Write the deterministic constructors**

In `crates/display-protocol/src/session.rs`, factor `Session::host` and `Session::guest` so that the randomness is a parameter, and expose:

```rust
    /// The same as [`Session::host`], with the session id and nonce supplied.
    ///
    /// For golden vectors, which have to be reproducible, and for nothing
    /// else: a session whose nonce a caller chose is a session whose tags a
    /// caller can replay.
    #[doc(hidden)]
    #[must_use]
    pub fn host_with_randomness(
        secret: &Secret,
        offer: Offer,
        session_id: [u8; SESSION_ID_LEN],
        nonce: [u8; NONCE_LEN],
    ) -> (Self, Record) {
        // ... what `host` does, without drawing the two arrays
    }
```

and the matching `guest_with_randomness`. `Session::host` and `Session::guest` become thin wrappers that draw the randomness and call these.

- [ ] **Step 2: Write the failing test**

`crates/display-protocol/tests/golden.rs`:

```rust
//! The bytes this build puts on the wire, held still.
//!
//! A golden vector is the only test that fails when a change to the format is
//! correct in Rust and wrong on the wire -- a renumbered field, a reordered
//! transcript, a header field that moved. The guest and the host of a VMLord
//! release are upgraded separately, so the wire is where compatibility lives.
//!
//! To refresh after an intentional format change -- which is a major or minor
//! version bump, never a silent edit:
//!
//! ```text
//! VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-protocol --test golden
//! ```

use std::{env, fs, path::{Path, PathBuf}};

use vmlord_display_protocol::{
    keys::{NONCE_LEN, SESSION_ID_LEN, Secret},
    record::{self, Channel, Limits, Record},
    session::{Offer, Session, Support},
    v1::{Capability, FrameRecord, InputRecord, KeyEvent, Mode, PointerMotion, StreamConfig},
};
use prost::Message;

/// A secret nobody holds, so that these bytes may live in a public tree.
const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const SESSION_ID: [u8; SESSION_ID_LEN] = [0x11; SESSION_ID_LEN];
const HOST_NONCE: [u8; NONCE_LEN] = [0x22; NONCE_LEN];
const GUEST_NONCE: [u8; NONCE_LEN] = [0x33; NONCE_LEN];

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name)
}

fn hold(name: &str, produced: &[u8]) {
    let path = golden(name);

    if env::var_os("VMLORD_REFRESH_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().expect("a parent")).expect("failed to create tests/golden");
        fs::write(&path, produced).expect("failed to refresh a golden vector");
        return;
    }

    let held = fs::read(&path).expect("failed to read a golden vector");
    assert_eq!(
        held, produced,
        "the wire format changed; if that was intended, bump the protocol version and refresh with \
         VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-protocol --test golden"
    );
}

fn offer() -> Offer {
    Offer {
        capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
        mode: Mode::Desktop,
        width: 1920,
        height: 1080,
        tile_size: 32,
    }
}

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![16, 32, 64],
        width: 1920,
        height: 1080,
    }
}

#[test]
fn the_handshake_is_the_bytes_it_has_always_been() {
    let secret = Secret::from_base64(SECRET).expect("a fixed secret");
    let limits = Limits::new(1920, 1080);

    let (mut host, client_hello) =
        Session::host_with_randomness(&secret, offer(), SESSION_ID, HOST_NONCE);
    let mut guest = Session::guest_with_randomness(&secret, support(), GUEST_NONCE);

    let mut wire = Vec::new();
    record::write(&mut wire, &client_hello, &limits).expect("a client hello");

    let server_hello = guest
        .handle(&client_hello.header, &client_hello.payload)
        .expect("a well-formed client hello")
        .reply
        .expect("a server hello");
    record::write(&mut wire, &server_hello, &limits).expect("a server hello");

    let server_auth = guest.pending_auth().expect("the guest's proof");
    record::write(&mut wire, &server_auth, &limits).expect("a server auth");

    host.handle(&server_hello.header, &server_hello.payload)
        .expect("a well-formed server hello");
    let client_auth = host
        .handle(&server_auth.header, &server_auth.payload)
        .expect("a valid guest proof")
        .reply
        .expect("the host's proof");
    record::write(&mut wire, &client_auth, &limits).expect("a client auth");

    hold("handshake.bin", &wire);
}

#[test]
fn one_record_of_each_carrying_type_is_the_bytes_it_has_always_been() {
    let limits = Limits::new(1920, 1080);
    let mut wire = Vec::new();

    let stream_config = Record::new(
        Channel::Frame,
        FrameRecord::StreamConfig as u16,
        0,
        0,
        0,
        StreamConfig {
            width: 1920,
            height: 1080,
            tile_size: 32,
            pixel_format: vmlord_display_protocol::v1::PixelFormat::Bgra8888 as i32,
        }
        .encode_to_vec(),
    );
    record::write(&mut wire, &stream_config, &limits).expect("a stream config");

    let keyframe = Record::new(Channel::Frame, FrameRecord::Keyframe as u16, 1, 0, 0, vec![0xAB; 64]);
    record::write(&mut wire, &keyframe, &limits).expect("a keyframe");

    let delta = Record::new(Channel::Frame, FrameRecord::TileDelta as u16, 2, 1, 0, vec![0xCD; 32]);
    record::write(&mut wire, &delta, &limits).expect("a tile delta");

    let key = Record::new(
        Channel::Input,
        InputRecord::KeyEvent as u16,
        0,
        0,
        0,
        KeyEvent { keycode: 30, pressed: true }.encode_to_vec(),
    );
    record::write(&mut wire, &key, &limits).expect("a key event");

    let motion = Record::new(
        Channel::Input,
        InputRecord::PointerMotion as u16,
        1,
        0,
        0,
        PointerMotion { x: 640, y: 480 }.encode_to_vec(),
    );
    record::write(&mut wire, &motion, &limits).expect("a pointer motion");

    hold("records.bin", &wire);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p vmlord-display-protocol --test golden`
Expected: FAIL — `tests/golden/handshake.bin` does not exist.

- [ ] **Step 4: Generate the vectors and re-run**

Run: `VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-protocol --test golden`
Expected: PASS, and both files exist.

Run: `cargo test -p vmlord-display-protocol --test golden`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Hold the display wire format with golden vectors"
```

---

### Task 9: The malformed and oversized corpus

Everything a hostile or broken peer can put on the socket, in one place, each entry asserting the specific refusal rather than merely "an error".

**Files:**
- Create: `crates/display-protocol/tests/malformed.rs`

**Interfaces:**
- Consumes: everything up to task 8.
- Produces: nothing the crate exports.

- [ ] **Step 1: Write the failing test**

`crates/display-protocol/tests/malformed.rs`:

```rust
//! What a peer that is broken, hostile, or from another protocol can send.
//!
//! Each case asserts the specific refusal, not merely that something failed:
//! a checksum mismatch reported as a decode error would hide a transport
//! problem behind a schema one.

use prost::Message;
use vmlord_display_protocol::{
    keys::{SESSION_ID_LEN, Secret, TAG_LEN},
    record::{self, Channel, Header, Limits, Record, RecordError},
    session::{Event, Offer, Session, SessionError, Support},
    v1::{Capability, ChannelHello, ClientHello, ControlRecord, FrameRecord, Mode, ProtocolVersion},
};

fn limits() -> Limits {
    Limits::new(1920, 1080)
}

fn offer() -> Offer {
    Offer {
        capabilities: vec![Capability::CursorStream],
        mode: Mode::Desktop,
        width: 1920,
        height: 1080,
        tile_size: 32,
    }
}

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

#[test]
fn a_header_from_no_protocol_at_all_is_refused() {
    let mut payload = Vec::new();
    let error = record::read(&mut [0u8; 24].as_slice(), &limits(), &mut payload)
        .expect_err("a header of zeroes");

    assert!(matches!(error, RecordError::MalformedHeader { header_len: 0 }));
}

#[test]
fn a_frame_larger_than_the_agreed_geometry_is_refused_before_it_is_allocated() {
    let mut header = Record::new(Channel::Frame, FrameRecord::Keyframe as u16, 0, 0, 0, Vec::new()).header;
    header.length = 1920 * 1080 * 4 + 65537;

    let mut payload = Vec::new();
    let error = record::read(&mut header.encode().as_slice(), &limits(), &mut payload)
        .expect_err("a frame over the geometry-derived cap");

    assert!(matches!(error, RecordError::TooLarge { channel: Channel::Frame, .. }));
}

#[test]
fn a_control_record_over_its_fixed_cap_is_refused() {
    let mut header = Record::new(Channel::Control, ControlRecord::Ping as u16, 0, 0, 0, Vec::new()).header;
    header.length = 65537;

    let mut payload = Vec::new();
    let error = record::read(&mut header.encode().as_slice(), &limits(), &mut payload)
        .expect_err("a control record over its cap");

    assert!(matches!(error, RecordError::TooLarge { channel: Channel::Control, cap: 65536, .. }));
}

#[test]
fn a_truncated_payload_is_a_transport_fault_not_a_schema_one() {
    let record = Record::new(Channel::Control, ControlRecord::Ping as u16, 0, 0, 0, vec![1, 2, 3, 4]);
    let mut wire = record.header.encode().to_vec();
    wire.extend_from_slice(&record.payload[..2]);

    let mut payload = Vec::new();
    let error = record::read(&mut wire.as_slice(), &limits(), &mut payload).expect_err("half a payload");

    assert!(matches!(error, RecordError::Io(_)));
}

#[test]
fn a_flipped_bit_in_a_frame_is_caught_by_the_checksum() {
    let record = Record::new(Channel::Frame, FrameRecord::Keyframe as u16, 0, 0, 0, vec![0x5A; 512]);
    let mut wire = Vec::new();
    record::write(&mut wire, &record, &limits()).expect("a keyframe within the cap");
    wire[100] ^= 0x01;

    let mut payload = Vec::new();
    let error = record::read(&mut wire.as_slice(), &limits(), &mut payload).expect_err("a flipped bit");

    assert!(matches!(error, RecordError::ChecksumMismatch { .. }));
}

#[test]
fn a_hello_from_another_major_is_refused_with_the_version_code() {
    let mut guest = Session::guest(&Secret::generate(), support());

    let hello = Record::new(
        Channel::Control,
        ControlRecord::ClientHello as u16,
        0,
        0,
        0,
        ClientHello {
            version: Some(ProtocolVersion { major: 2, minor: 0 }),
            capabilities: Vec::new(),
            session_id: vec![0x11; SESSION_ID_LEN],
            host_nonce: vec![0x22; 32],
            mode: Mode::Desktop as i32,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
        .encode_to_vec(),
    );

    let error = guest.handle(&hello.header, &hello.payload).expect_err("a major this build has not");

    assert!(matches!(error, SessionError::Version(_)));
    assert_eq!(error.code(), vmlord_display_protocol::v1::ErrorCode::UnsupportedVersion);
}

#[test]
fn a_nonce_of_the_wrong_width_is_refused_rather_than_padded() {
    let mut guest = Session::guest(&Secret::generate(), support());

    let hello = Record::new(
        Channel::Control,
        ControlRecord::ClientHello as u16,
        0,
        0,
        0,
        ClientHello {
            version: Some(ProtocolVersion::current()),
            capabilities: Vec::new(),
            session_id: vec![0x11; SESSION_ID_LEN],
            host_nonce: vec![0x22; 8],
            mode: Mode::Desktop as i32,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
        .encode_to_vec(),
    );

    assert!(matches!(
        guest.handle(&hello.header, &hello.payload),
        Err(SessionError::Field(_))
    ));
}

#[test]
fn a_channel_hello_whose_field_disagrees_with_its_header_is_refused() {
    let secret = Secret::generate();
    let (mut host, mut guest) = {
        let (mut host, client_hello) = Session::host(&secret, offer());
        let mut guest = Session::guest(&secret, support());

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        let server_auth = guest.pending_auth().expect("the guest's proof");

        host.handle(&server_hello.header, &server_hello.payload).expect("a server hello");
        let client_auth = host
            .handle(&server_auth.header, &server_auth.payload)
            .expect("a valid guest proof")
            .reply
            .expect("the host's proof");
        let outcome = guest.handle(&client_auth.header, &client_auth.payload).expect("a valid host proof");
        assert_eq!(outcome.event, Event::ControlEstablished);

        (host, guest)
    };

    let hello = host.open_channel(Channel::Frame).expect("an established session");
    let mut message = ChannelHello::decode(hello.payload.as_slice()).expect("what was built");
    // Says input in the message, frame in the header.
    message.channel = u32::from(Channel::Input.as_wire());
    let forged = Record::new(
        Channel::Frame,
        FrameRecord::ChannelHello as u16,
        0,
        0,
        0,
        message.encode_to_vec(),
    );

    assert!(matches!(
        guest.handle(&forged.header, &forged.payload),
        Err(SessionError::Unexpected { .. })
    ));
}

#[test]
fn a_tag_of_the_wrong_width_never_reaches_a_comparison() {
    let secret = Secret::generate();
    let (mut host, client_hello) = Session::host(&secret, offer());
    let mut guest = Session::guest(&secret, support());

    let server_hello = guest
        .handle(&client_hello.header, &client_hello.payload)
        .expect("a well-formed client hello")
        .reply
        .expect("a server hello");
    host.handle(&server_hello.header, &server_hello.payload).expect("a server hello");

    let short = Record::new(
        Channel::Control,
        ControlRecord::ServerAuth as u16,
        2,
        0,
        0,
        vmlord_display_protocol::v1::ServerAuth { tag: vec![0u8; TAG_LEN - 1] }.encode_to_vec(),
    );

    assert!(matches!(
        host.handle(&short.header, &short.payload),
        Err(SessionError::Field(_))
    ));
}

#[test]
fn a_header_that_is_all_ones_allocates_nothing() {
    let mut payload = Vec::new();
    let error = record::read(&mut [0xFFu8; 24].as_slice(), &limits(), &mut payload)
        .expect_err("a header of ones");

    // The channel byte is checked before the length is trusted.
    assert!(matches!(error, RecordError::UnknownChannel { value: 0xFF }));
    assert!(payload.is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-display-protocol --test malformed`
Expected: FAIL if any refusal is wrong or missing. Fix the implementation, not the test, unless the test is wrong about the spec.

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p vmlord-display-protocol --test malformed`
Expected: PASS, 10 tests.

- [ ] **Step 4: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Refuse malformed and oversized display records"
```

---

### Task 10: Compatibility across minors

Two builds of this crate, a minor apart, must open a session. The test cannot import two crate versions, so it does what a real older peer does: sends the hellos an older minor would send, and reads the answer.

**Files:**
- Create: `crates/display-protocol/tests/compatibility.rs`

**Interfaces:**
- Consumes: everything up to task 9.
- Produces: nothing the crate exports.

- [ ] **Step 1: Write the failing test**

`crates/display-protocol/tests/compatibility.rs`:

```rust
//! A guest installed months ago still has to talk to today's host.
//!
//! There is no second version of this crate to link, so an older or newer peer
//! is played by hand: the hello it would send, and the answer this build gives
//! it.

use prost::Message;
use vmlord_display_protocol::{
    keys::{SESSION_ID_LEN, Secret},
    record::{Channel, Record},
    session::{Offer, Session, SessionError, Support},
    v1::{Capability, ClientHello, ControlRecord, Mode, ProtocolVersion, ServerHello},
};

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

fn hello_from(version: ProtocolVersion, capabilities: Vec<i32>) -> Record {
    Record::new(
        Channel::Control,
        ControlRecord::ClientHello as u16,
        0,
        0,
        0,
        ClientHello {
            version: Some(version),
            capabilities,
            session_id: vec![0x11; SESSION_ID_LEN],
            host_nonce: vec![0x22; 32],
            mode: Mode::Desktop as i32,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
        .encode_to_vec(),
    )
}

#[test]
fn a_newer_host_and_this_guest_settle_on_this_builds_minor() {
    let mut guest = Session::guest(&Secret::generate(), support());
    let newer = ProtocolVersion {
        major: ProtocolVersion::current().major,
        minor: ProtocolVersion::current().minor + 3,
    };

    let reply = guest
        .handle(&hello_from(newer, Vec::new()).header, &hello_from(newer, Vec::new()).payload)
        .expect("a hello from a newer minor")
        .reply
        .expect("a server hello");

    let answered = ServerHello::decode(reply.payload.as_slice()).expect("a server hello");

    assert_eq!(answered.version, Some(ProtocolVersion::current()));
}

#[test]
fn a_capability_from_a_newer_peer_is_dropped_rather_than_refused() {
    let mut guest = Session::guest(&Secret::generate(), support());
    let hello = hello_from(
        ProtocolVersion::current(),
        vec![i32::from(Capability::CursorStream), 4242],
    );

    let reply = guest
        .handle(&hello.header, &hello.payload)
        .expect("a hello naming a capability this build has never heard of")
        .reply
        .expect("a server hello");

    let answered = ServerHello::decode(reply.payload.as_slice()).expect("a server hello");

    assert_eq!(answered.capabilities, vec![i32::from(Capability::CursorStream)]);
}

#[test]
fn a_guest_that_agreed_on_something_the_host_never_offered_is_refused() {
    let secret = Secret::generate();
    let (mut host, _) = Session::host(
        &secret,
        Offer {
            capabilities: vec![Capability::CursorStream],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        },
    );

    let overreaching = Record::new(
        Channel::Control,
        ControlRecord::ServerHello as u16,
        0,
        0,
        0,
        ServerHello {
            version: Some(ProtocolVersion::current()),
            capabilities: vec![i32::from(Capability::DynamicResolution)],
            guest_nonce: vec![0x33; 32],
            modes: vec![Mode::Desktop as i32],
            tile_sizes: vec![32],
            width: 1920,
            height: 1080,
        }
        .encode_to_vec(),
    );

    assert!(matches!(
        host.handle(&overreaching.header, &overreaching.payload),
        Err(SessionError::Capability(_))
    ));
}

#[test]
fn a_record_type_from_a_newer_minor_is_refused_rather_than_guessed_at() {
    let mut guest = Session::guest(&Secret::generate(), support());
    let unknown = Record::new(Channel::Control, 4242, 0, 0, 0, Vec::new());

    assert!(matches!(
        guest.handle(&unknown.header, &unknown.payload),
        Err(SessionError::Unexpected { message_type: 4242, .. })
    ));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-display-protocol --test compatibility`
Expected: FAIL if any rule is wrong.

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p vmlord-display-protocol --test compatibility`
Expected: PASS, 4 tests.

- [ ] **Step 4: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Prove display protocol compatibility across minors"
```

---

### Task 11: Fuzzing the header parser and the session machine

The spec asks for fuzzing, and the repository has no nightly toolchain and no `cargo-fuzz` infrastructure. What it gets instead is a deterministic mutation harness that runs in `cargo test` on every machine: a seeded generator, the golden vectors as the corpus, and the two invariants that matter — nothing panics, and no channel key is ever handed out to input that did not authenticate.

**Files:**
- Create: `crates/display-protocol/tests/fuzz.rs`

**Interfaces:**
- Consumes: everything up to task 10, plus `tests/golden/*.bin` as the corpus.
- Produces: nothing the crate exports.

- [ ] **Step 1: Write the failing test**

`crates/display-protocol/tests/fuzz.rs`:

```rust
//! Arbitrary bytes against the two things that face an untrusted peer.
//!
//! Deterministic rather than a `cargo-fuzz` target: this repository builds on
//! stable, and a fuzzer nobody runs finds nothing. The seed is fixed, so a
//! failure here reproduces exactly; the corpus is the golden vectors, so the
//! mutations start from bytes that mean something.
//!
//! Two invariants: nothing panics, and no session hands out a channel key
//! unless a real handshake put one there.

use vmlord_display_protocol::{
    keys::Secret,
    record::{self, Channel, Limits},
    session::{Offer, Session, Support},
    v1::{Capability, Mode},
};

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
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    vec![
        std::fs::read(dir.join("handshake.bin")).expect("the handshake vector"),
        std::fs::read(dir.join("records.bin")).expect("the records vector"),
    ]
}

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

fn offer() -> Offer {
    Offer {
        capabilities: vec![Capability::CursorStream],
        mode: Mode::Desktop,
        width: 1920,
        height: 1080,
        tile_size: 32,
    }
}

#[test]
fn the_record_reader_survives_mutated_input() {
    let mut rng = Rng(0x5EED_1234_5678_9ABC);
    let limits = Limits::new(1920, 1080);
    let corpus = corpus();

    for _ in 0..20_000 {
        let mut bytes = corpus[rng.below(corpus.len())].clone();
        for _ in 0..1 + rng.below(8) {
            let at = rng.below(bytes.len());
            bytes[at] ^= (rng.next() & 0xFF) as u8;
        }
        bytes.truncate(rng.below(bytes.len() + 1));

        let mut payload = Vec::new();
        let mut cursor = bytes.as_slice();
        // Whatever it returns, it must return: a reader that panics on a
        // hostile guest is a viewer that dies on one.
        while record::read(&mut cursor, &limits, &mut payload).is_ok() {}
    }
}

#[test]
fn a_session_never_yields_a_channel_key_to_input_that_did_not_authenticate() {
    let mut rng = Rng(0x1234_5EED_9ABC_5678);
    let corpus = corpus();
    let limits = Limits::new(1920, 1080);

    for _ in 0..5_000 {
        let secret = Secret::generate();
        let mut guest = Session::guest(&secret, support());
        let (mut host, _) = Session::host(&secret, offer());

        let mut bytes = corpus[rng.below(corpus.len())].clone();
        for _ in 0..1 + rng.below(8) {
            let at = rng.below(bytes.len());
            bytes[at] ^= (rng.next() & 0xFF) as u8;
        }

        let mut cursor = bytes.as_slice();
        let mut payload = Vec::new();
        while let Ok(header) = record::read(&mut cursor, &limits, &mut payload) {
            let _ = guest.handle(&header, &payload);
            let _ = host.handle(&header, &payload);
        }

        for channel in [Channel::Frame, Channel::Input] {
            assert!(
                guest.channel_key(channel).is_none(),
                "a guest bound a channel to mutated input"
            );
            assert!(
                host.channel_key(channel).is_none(),
                "a host bound a channel to mutated input"
            );
        }
    }
}
```

- [ ] **Step 2: Run the test to verify it fails or passes**

Run: `cargo test -p vmlord-display-protocol --test fuzz`
Expected: PASS if the implementation is sound. A panic here is a real defect: fix `record` or `session`, never the harness. If the run takes more than about thirty seconds, lower the iteration counts rather than deleting cases.

- [ ] **Step 3: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-118: Fuzz the display record reader and session machine"
```

---

### Task 12: The architecture record

ARCHITECTURE.md is where a decision lives after its task closes. The display protocol gets a section beside the agent protocol's, and it must say the things a reader would otherwise have to reconstruct: why the direction is reversed, why the frame channel is not Protobuf, why authentication is mutual, and what is deliberately not encrypted.

**Files:**
- Modify: `ARCHITECTURE.md` (a new section after "The host end of the agent socket")

**Interfaces:**
- Consumes: the finished crate.
- Produces: nothing in code.

- [ ] **Step 1: Write the section**

Add `## The display protocol` to `ARCHITECTURE.md`, after the agent socket section, in the voice of the sections around it — prose that argues, not a feature list. It must cover:

- three services, `VMLD`/`VMLF`/`VMLI`, and that the guest listens while the host connects, with the reason: a display session's life is the viewer's life, and readiness comes over the agent channel instead;
- the 24-byte record header, `header_len` in place of a magic, and why frames are not carried in a Protobuf `bytes` field;
- the caps, and that the frame cap is derived from the agreed geometry rather than fixed;
- the four-record mutual handshake, the transcript over wire bytes, that the guest proves first, and that the session key is derived from the existing per-VM secret so the unprivileged capture process never holds it;
- channel binding through the transcript, and generations;
- what is not there: no per-record MAC, no encryption, and the reason -- a point-to-point stream inside the hypervisor, where confidentiality comes from the partition boundary;
- that the MVP guest announces `MODE_DESKTOP` alone and `MODE_AUTO` resolves to it.

- [ ] **Step 2: Check that nothing else in the document contradicts it**

Run: `grep -n "display" ARCHITECTURE.md`
Expected: the existing mentions of `asb_vm_open_display` and the display backend stub still describe the current state — this task does not change them, because the AppSandbox path is still what ships until #129.

- [ ] **Step 3: Verify the whole workspace still builds**

Run: `cargo check-windows`
Expected: PASS.

Run: `cargo test -p vmlord-display-protocol`
Expected: PASS, all tests.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "TASK-118: Record the display protocol in the architecture"
```

---

## Self-review notes

Spec coverage, section by section:

| Spec section | Task |
| --- | --- |
| Transport shape | 1 (ports in the schema comments), 12 |
| Records | 2 |
| Limits | 3 |
| Authentication | 5, 6 |
| Version and capabilities | 4, 10 |
| Channel binding | 5, 7 |
| Messages | 1 |
| Flow control | 1 (`RequestKeyframe`, `Ping`/`Pong` exist), 12 |
| Modes | 1, 6 |
| Recovery | 7 (generations, the obligations named on `reconnect_channel`), 12 |
| Errors | 1, 6, 7 |
| Threat model | 12 |
| Public surface | 2, 3, 4, 5, 6, 7 |
| Tests | 2, 3, 4, 5, 6, 7, 8, 9, 10, 11 |

Two deviations from the spec, both deliberate and both recorded here rather than buried:

1. **Fuzzing** is a deterministic mutation harness in `cargo test`, not `cargo-fuzz`. The repository builds on stable and has no fuzzing infrastructure; a nightly-only target nobody runs would satisfy the letter of the spec and none of its purpose. If `cargo-fuzz` arrives in this repository later, these invariants port to it unchanged.
2. **`Session::host_with_randomness`** exists for golden vectors. It is `#[doc(hidden)]` and documented as reproducibility-only, because a session whose nonce a caller chooses is a session whose tags a caller can replay.
