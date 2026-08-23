# Native Windows display viewer implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `vmlord-display.exe` — a separate Win32/D3D11 process that opens one display session against a VMLord guest, proves itself with a one-shot derived credential, and puts the guest's desktop on screen.

**Architecture:** One new crate, `crates/display-viewer`, producing one binary. VMLord holds the VM secret and drives the control handshake; the viewer relays handshake bytes over the anonymous pipes it was launched with, and is handed two `ChannelKey`s when the handshake completes. From there it is autonomous: three `AF_HYPERV` sockets (control, frame, input) driven by `vmlord_display_protocol::Session`, decoded by `vmlord_display_codec::Decoder`, uploaded to a D3D11 texture as dirty rectangles, with a Direct2D status overlay over the non-running states. `unsafe` lives in four modules under `src/windows/` and nowhere else.

**Tech Stack:** Rust 2024, `x86_64-pc-windows-msvc` (release) / `x86_64-pc-windows-gnu` (check and test from WSL), the `windows` crate for Win32/WinSock/D3D11/D2D, `prost`/`protox` for the launch-pipe schema, `vmlord-display-protocol`, `vmlord-display-codec`, `vmlord-core` for settings and logging.

**Spec:** `docs/superpowers/specs/2026-08-23-display-viewer-design.md`

## Global Constraints

* Task number for every commit subject: `TASK-117: <comment>`.
* The crate is in workspace `members` **and** `default-members`, beside the other Windows-side crates.
* The crate keeps `[lints] workspace = true` — `unsafe_code = "deny"`. It is re-allowed **only** on the four module declarations in `src/windows/mod.rs`: `window`, `d3d`, `hvsocket`, `ipc`. No other module in the crate may contain `unsafe`, and every `unsafe` block carries a `// SAFETY:` comment.
* **The master secret never enters this process.** No `Secret`, no `SessionKey`. Only `ChannelKey` bytes, over the inherited launch pipes, once per session.
* Nothing sensitive or structural travels on the command line or in the environment. The viewer takes **no** command-line arguments.
* Launch-pipe transport: `u32` little-endian length prefix, then a `vmlord.display.viewer.v1.Envelope`. Cap: `MAX_MESSAGE = 1 MiB`, enforced from the prefix before any allocation.
* vsock ports, fixed by #118: control `VMLD` = `0x564D_4C44`, frame `VMLF` = `0x564D_4C46`, input `VMLI` = `0x564D_4C49`. The service GUID is `GUID::from_values(port, 0xfacb, 0x11e6, [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3])`.
* One retry budget governs every non-running state: `RETRY_BUDGET = 30s` of active retry from the moment the state began, then `Failed` with working Retry and Cancel buttons.
* Ping every `PING_INTERVAL = 5s`; control is dead when a Pong is `PONG_TIMEOUT = 10s` overdue.
* D3D device loss is recovered at most `MAX_DEVICE_LOSSES = 3` times per session.
* **Never log framebuffer or cursor pixel content.** Logs carry sizes, sequences, states, error codes and geometry. There is no screenshot feature in this build; do not add one.
* Named mutex: `Local\VMLord.Display.{runtime-id}`. Named pipe: `\\.\pipe\vmlord-display.{runtime-id}`, default DACL of the launching user.
* Window title: `{vm name} - VMLord Display`.
* Follow AGENTS.md: small modules, explicit code, no traits with a single implementation, documentation updated in the same branch.
* Commands: `cargo check-windows` to compile-check from WSL, `cargo test-windows` to build and run the tests. Never `cargo test` on the Linux host — the crate is `#[cfg(not(windows))] compile_error!` like `vmlord-com1`.
* One deliberate refinement of the spec's crate layout: the spec's single `session.rs` is two files here — `relay.rs` (the handshake relay) and `live.rs` (the established session). They have different peers, different failure modes and different tests, and one file doing both would be the largest in the crate. `duplex.rs` is a third, test-only file: two ends of a socket in memory, which is what lets the session loop be tested without a partition.
* Out of scope, and must not be implemented here: keyboard and mouse input (#119), letterbox/fullscreen/dynamic resolution/saved window state (#120), HCS service registration and the Connect wiring that launches this binary (#121).

---

### Task 1: The crate, the launch schema and the logger

**Files:**
- Create: `crates/display-viewer/Cargo.toml`
- Create: `crates/display-viewer/build.rs`
- Create: `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto`
- Create: `crates/display-viewer/src/lib.rs`
- Create: `crates/display-viewer/src/launch.rs`
- Create: `crates/display-viewer/src/log.rs`
- Modify: `Cargo.toml` (workspace `members` and `default-members`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `vmlord_display_viewer::launch::{Message, Command, LaunchParameters, Handover, Link, LaunchError, REVISION, MAX_MESSAGE, encode, decode}`
  - `Message` variants: `Launch(LaunchParameters)`, `RelayToViewer(Vec<u8>)`, `RelayFromViewer(Vec<u8>)`, `Handover(Handover)`, `RequestRelay { token: Vec<u8> }`, `Command(Command)`
  - `Command` variants: `Focus`, `Close`
  - `Link::new(reader, writer)`, `Link::read(&mut self) -> Result<Message, LaunchError>`, `Link::write(&mut self, &Message) -> Result<(), LaunchError>`
  - `vmlord_display_viewer::log::{initialize, redaction_rule}` and `#[cfg(test)] log::capture`

- [ ] **Step 1: Write the schema**

Create `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto`:

```protobuf
// What VMLord and one viewer process say to each other over the two anonymous
// pipes the viewer was launched with.
//
// Private to this pair of processes: the wire contract with the guest is
// `vmlord.display.v1`, and nothing here crosses a socket. The compatibility
// rules of that schema apply here too -- never reuse or renumber a field.

syntax = "proto3";

package vmlord.display.viewer.v1;

// One message on a launch pipe. `revision` names the contract, so a mismatched
// pair of processes fails at the first message rather than at a garbled stream.
message Envelope {
  uint32 revision = 1;

  oneof kind {
    LaunchParameters launch = 2;
    Relay relay_to_viewer = 3;
    Relay relay_from_viewer = 4;
    Handover handover = 5;
    RequestRelay request_relay = 6;
    Command command = 7;
  }
}

// Everything the viewer is told at startup. Sent once, first.
message LaunchParameters {
  // For the window title and the log; never parsed.
  string vm_name = 1;
  // The 16 bytes of the compute system's runtime id GUID.
  bytes runtime_id = 2;
  uint32 control_port = 3;
  uint32 frame_port = 4;
  uint32 input_port = 5;
  // What VMLord offered, so the viewer can size its first window before the
  // handshake settles anything.
  uint32 width = 6;
  uint32 height = 7;
  uint32 tile_size = 8;
  // The right to ask for a new session over these pipes. Not a key.
  bytes token = 9;
  // The `ClientHello` record -- header and payload -- to write to the control
  // socket once it connects.
  bytes client_hello = 10;
}

// Handshake bytes, verbatim. The viewer parses none of them.
message Relay {
  bytes bytes = 1;
}

// The one-shot derived credential. Sent once per handshake; relay mode ends
// here and VMLord drops its own session.
message Handover {
  bytes session_id = 1;
  bytes frame_key = 2;
  bytes input_key = 3;
  uint32 version_major = 4;
  uint32 version_minor = 5;
  repeated vmlord.display.v1.Capability capabilities = 6;
  vmlord.display.v1.Mode mode = 7;
  uint32 width = 8;
  uint32 height = 9;
  uint32 tile_size = 10;
  // The next sequence the host's control channel is on, so that the viewer
  // carries on the numbering VMLord left off at.
  uint32 control_sequence = 11;
}

// The viewer asking for a new session after control was lost. The token is
// what makes this answerable only by the VMLord instance that spawned it.
message RequestRelay {
  bytes token = 1;
}

message Command {
  Kind kind = 1;

  enum Kind {
    KIND_UNSPECIFIED = 0;
    KIND_FOCUS = 1;
    KIND_CLOSE = 2;
  }
}
```

Add the import that `Capability` and `Mode` come from, directly under `package`:

```protobuf
import "vmlord/display/v1/display.proto";
```

- [ ] **Step 2: Write the manifest and the build script**

Create `crates/display-viewer/Cargo.toml`:

```toml
[package]
name = "vmlord-display-viewer"
version.workspace = true
edition.workspace = true
license.workspace = true
build = "build.rs"

# The window VMLord opens on a VM's display. A process of its own because a
# session outlives the application that started it.
[[bin]]
name = "vmlord-display"
path = "src/main.rs"
test = false
bench = false

[lib]
name = "vmlord_display_viewer"
path = "src/lib.rs"

[dependencies]
log.workspace = true
prost = "0.14"
# Settings and the application log, the same two `vmlord-com1` takes.
vmlord-core = { path = "../core" }
vmlord-display-codec = { path = "../display-codec" }
vmlord-display-protocol = { path = "../display-protocol" }
windows = { workspace = true, features = [
    "Win32_Foundation",
    "Win32_Graphics_Direct2D",
    "Win32_Graphics_Direct2D_Common",
    "Win32_Graphics_Direct3D",
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_DirectWrite",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_Graphics_Gdi",
    "Win32_Networking_WinSock",
    "Win32_Security",
    "Win32_Storage_FileSystem",
    "Win32_System_LibraryLoader",
    "Win32_System_Pipes",
    "Win32_System_Threading",
    "Win32_UI_WindowsAndMessaging",
] }

[build-dependencies]
prost-build = "0.14"
# Compiles the `.proto` in-process, so no `protoc` has to be installed.
protox = "0.9"

[lints]
workspace = true
```

Create `crates/display-viewer/build.rs`:

```rust
//! Turns the launch pipes' private schema into Rust, without `protoc`.
//!
//! The same in-process `protox` compile the other display crates use. The
//! include path reaches into `display-protocol`'s `proto` directory because the
//! hand-over names that schema's `Capability` and `Mode` rather than restating
//! them.

const PROTO: &str = "proto/vmlord/display/viewer/viewer.proto";
const INCLUDE: &str = "proto";
const PROTOCOL_INCLUDE: &str = "../display-protocol/proto";

fn main() {
    println!("cargo::rerun-if-changed={PROTO}");
    println!("cargo::rerun-if-changed={PROTOCOL_INCLUDE}");

    let descriptor_set = protox::compile([PROTO], [INCLUDE, PROTOCOL_INCLUDE])
        .unwrap_or_else(|error| panic!("failed to compile {PROTO}: {error}"));

    prost_build::Config::new()
        .compile_fds(descriptor_set)
        .expect("failed to generate Rust types");
}
```

- [ ] **Step 3: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/display-viewer",` to `members` (after `"crates/display-services",`) and to `default-members` (after `"crates/display-protocol",`).

- [ ] **Step 4: Write the failing test**

Create `crates/display-viewer/src/launch.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::{Command, Handover, LaunchError, LaunchParameters, Link, Message, decode, encode};

    fn parameters() -> LaunchParameters {
        LaunchParameters {
            vm_name: "ubuntu-24.04".to_owned(),
            runtime_id: [7; 16],
            control_port: 0x564D_4C44,
            frame_port: 0x564D_4C46,
            input_port: 0x564D_4C49,
            width: 1920,
            height: 1080,
            tile_size: 32,
            token: vec![9; 32],
            client_hello: vec![1, 2, 3, 4],
        }
    }

    fn handover() -> Handover {
        Handover {
            session_id: vec![3; 16],
            frame_key: vec![4; 32],
            input_key: vec![5; 32],
            version_major: 1,
            version_minor: 0,
            capabilities: vec![1],
            mode: 2,
            width: 1920,
            height: 1080,
            tile_size: 32,
            control_sequence: 2,
        }
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        let messages = [
            Message::Launch(parameters()),
            Message::RelayToViewer(vec![0xaa; 64]),
            Message::RelayFromViewer(vec![0xbb; 64]),
            Message::Handover(handover()),
            Message::RequestRelay { token: vec![9; 32] },
            Message::Command(Command::Focus),
            Message::Command(Command::Close),
        ];

        for message in messages {
            assert_eq!(
                decode(&encode(&message)).expect("a message this build wrote"),
                message
            );
        }
    }

    #[test]
    fn a_message_from_another_revision_is_refused() {
        let mut bytes = encode(&Message::Command(Command::Focus));
        // Field 1, varint: the revision is the first two bytes of the envelope.
        assert_eq!(bytes[0], 0x08);
        bytes[1] = 99;

        assert!(matches!(
            decode(&bytes),
            Err(LaunchError::Revision { found: 99, .. })
        ));
    }

    #[test]
    fn an_envelope_naming_no_message_is_refused() {
        // Revision 1 and nothing else.
        assert!(matches!(decode(&[0x08, 0x01]), Err(LaunchError::Empty)));
    }

    #[test]
    fn bytes_that_are_not_an_envelope_are_refused() {
        assert!(decode(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn a_link_carries_messages_both_ways() {
        let mut pipe = Vec::new();
        {
            let mut link = Link::new(io::empty(), &mut pipe);
            link.write(&Message::Command(Command::Close))
                .expect("an in-memory writer");
        }

        let mut link = Link::new(pipe.as_slice(), io::sink());
        assert_eq!(
            link.read().expect("what was just written"),
            Message::Command(Command::Close)
        );
    }

    #[test]
    fn a_link_whose_parent_is_gone_reports_a_closed_pipe() {
        let mut link = Link::new(io::empty(), io::sink());

        assert!(matches!(link.read(), Err(LaunchError::Closed)));
    }

    #[test]
    fn a_length_prefix_above_the_cap_is_refused_before_anything_is_allocated() {
        let prefix = (super::MAX_MESSAGE + 1).to_le_bytes();
        let mut link = Link::new(prefix.as_slice(), io::sink());

        assert!(matches!(link.read(), Err(LaunchError::TooLarge { .. })));
    }

    use std::io;
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find function `encode` in this scope` and the rest.

- [ ] **Step 6: Write the implementation**

Prepend to `crates/display-viewer/src/launch.rs`:

```rust
//! The contract between VMLord and one viewer process.
//!
//! Two anonymous pipes, wired as this process's stdin and stdout by whoever
//! spawned it. Nothing structural and nothing sensitive is on the command line
//! or in the environment, which is what keeps a channel key out of a process
//! listing.
//!
//! Every message names the revision it was written against, so a VMLord and a
//! viewer that disagree fail at the first message rather than part-way through
//! a stream neither can parse.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
};

use prost::Message as _;

use crate::viewer::v1::{self as wire, envelope};

/// The revision of the launch contract this build speaks.
pub const REVISION: u32 = 1;

/// The largest message a launch pipe may carry.
///
/// A hand-over is a few hundred bytes and a relay is a control record, whose
/// own cap is 64 KiB. A megabyte is far above both and far below anything that
/// would matter if the pipe were ever fed nonsense.
pub const MAX_MESSAGE: u32 = 1024 * 1024;

/// What VMLord and a viewer say to each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Everything the viewer is told at startup. Once, first.
    Launch(LaunchParameters),
    /// Handshake bytes to write to the control socket, verbatim.
    RelayToViewer(Vec<u8>),
    /// Handshake bytes read off the control socket, verbatim.
    RelayFromViewer(Vec<u8>),
    /// The one-shot derived credential. Relay mode ends here.
    Handover(Handover),
    /// The viewer asking for a new session after control was lost.
    RequestRelay {
        /// The right to ask, carried since launch.
        token: Vec<u8>,
    },
    /// Something for the window rather than for the session.
    Command(Command),
}

/// What VMLord asks the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Bring the window to the front. What a repeated Connect means.
    Focus,
    /// Close the session and exit, the way the close button does.
    Close,
}

/// Everything the viewer is told at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchParameters {
    /// For the title bar and the log. Never parsed.
    pub vm_name: String,
    /// The compute system's runtime id, which is what an HvSocket address names.
    pub runtime_id: [u8; 16],
    /// The vsock port the control service listens on.
    pub control_port: u32,
    /// The vsock port the frame service listens on.
    pub frame_port: u32,
    /// The vsock port the input service listens on.
    pub input_port: u32,
    /// The width VMLord offered, for the window before the handshake settles.
    pub width: u32,
    /// The height VMLord offered.
    pub height: u32,
    /// The tile size VMLord offered.
    pub tile_size: u32,
    /// The right to ask for a new session over these pipes.
    pub token: Vec<u8>,
    /// The `ClientHello` record to write once the control socket connects.
    pub client_hello: Vec<u8>,
}

/// The one-shot derived credential, and what the handshake settled on.
///
/// Two channel keys, good for one session and no longer. The VM's secret is
/// not here and never crosses this pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handover {
    /// The 16 bytes that name the session across its three sockets.
    pub session_id: Vec<u8>,
    /// The key the frame socket proves itself with.
    pub frame_key: Vec<u8>,
    /// The key the input socket proves itself with.
    pub input_key: Vec<u8>,
    /// The major of the revision the session runs at.
    pub version_major: u32,
    /// The minor of the revision the session runs at.
    pub version_minor: u32,
    /// The capabilities both peers have, as `vmlord.display.v1.Capability`.
    pub capabilities: Vec<i32>,
    /// The mode the guest resolved to, as `vmlord.display.v1.Mode`.
    pub mode: i32,
    /// The width the session displays.
    pub width: u32,
    /// The height the session displays.
    pub height: u32,
    /// The tile size the frame stream uses for the life of the session.
    pub tile_size: u32,
    /// The sequence the host's control channel carries on from.
    pub control_sequence: u32,
}

/// Turns a message into the bytes an envelope carries, without the prefix.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    let kind = match message {
        Message::Launch(parameters) => envelope::Kind::Launch(wire::LaunchParameters {
            vm_name: parameters.vm_name.clone(),
            runtime_id: parameters.runtime_id.to_vec(),
            control_port: parameters.control_port,
            frame_port: parameters.frame_port,
            input_port: parameters.input_port,
            width: parameters.width,
            height: parameters.height,
            tile_size: parameters.tile_size,
            token: parameters.token.clone(),
            client_hello: parameters.client_hello.clone(),
        }),
        Message::RelayToViewer(bytes) => envelope::Kind::RelayToViewer(wire::Relay {
            bytes: bytes.clone(),
        }),
        Message::RelayFromViewer(bytes) => envelope::Kind::RelayFromViewer(wire::Relay {
            bytes: bytes.clone(),
        }),
        Message::Handover(handover) => envelope::Kind::Handover(wire::Handover {
            session_id: handover.session_id.clone(),
            frame_key: handover.frame_key.clone(),
            input_key: handover.input_key.clone(),
            version_major: handover.version_major,
            version_minor: handover.version_minor,
            capabilities: handover.capabilities.clone(),
            mode: handover.mode,
            width: handover.width,
            height: handover.height,
            tile_size: handover.tile_size,
            control_sequence: handover.control_sequence,
        }),
        Message::RequestRelay { token } => envelope::Kind::RequestRelay(wire::RequestRelay {
            token: token.clone(),
        }),
        Message::Command(command) => envelope::Kind::Command(wire::Command {
            kind: match command {
                Command::Focus => wire::command::Kind::Focus as i32,
                Command::Close => wire::command::Kind::Close as i32,
            },
        }),
    };

    wire::Envelope {
        revision: REVISION,
        kind: Some(kind),
    }
    .encode_to_vec()
}

/// Reads a message back out of an envelope's bytes.
///
/// # Errors
///
/// [`LaunchError::Decode`] for bytes that are not an envelope,
/// [`LaunchError::Revision`] for one written against another contract,
/// [`LaunchError::Empty`] for an envelope naming no message, and
/// [`LaunchError::Field`] for a fixed-width field that arrived at another
/// width.
pub fn decode(bytes: &[u8]) -> Result<Message, LaunchError> {
    let envelope = wire::Envelope::decode(bytes).map_err(LaunchError::Decode)?;
    if envelope.revision != REVISION {
        return Err(LaunchError::Revision {
            expected: REVISION,
            found: envelope.revision,
        });
    }

    let message = match envelope.kind.ok_or(LaunchError::Empty)? {
        envelope::Kind::Launch(parameters) => Message::Launch(LaunchParameters {
            vm_name: parameters.vm_name,
            runtime_id: parameters.runtime_id.as_slice().try_into().map_err(|_| {
                LaunchError::Field {
                    what: "runtime id",
                    len: parameters.runtime_id.len(),
                }
            })?,
            control_port: parameters.control_port,
            frame_port: parameters.frame_port,
            input_port: parameters.input_port,
            width: parameters.width,
            height: parameters.height,
            tile_size: parameters.tile_size,
            token: parameters.token,
            client_hello: parameters.client_hello,
        }),
        envelope::Kind::RelayToViewer(relay) => Message::RelayToViewer(relay.bytes),
        envelope::Kind::RelayFromViewer(relay) => Message::RelayFromViewer(relay.bytes),
        envelope::Kind::Handover(handover) => Message::Handover(Handover {
            session_id: handover.session_id,
            frame_key: handover.frame_key,
            input_key: handover.input_key,
            version_major: handover.version_major,
            version_minor: handover.version_minor,
            capabilities: handover.capabilities,
            mode: handover.mode,
            width: handover.width,
            height: handover.height,
            tile_size: handover.tile_size,
            control_sequence: handover.control_sequence,
        }),
        envelope::Kind::RequestRelay(request) => Message::RequestRelay {
            token: request.token,
        },
        envelope::Kind::Command(command) => Message::Command(
            match wire::command::Kind::try_from(command.kind) {
                Ok(wire::command::Kind::Focus) => Command::Focus,
                Ok(wire::command::Kind::Close) => Command::Close,
                _ => return Err(LaunchError::Empty),
            },
        ),
    };

    Ok(message)
}

/// The pair of pipes, framed.
///
/// Generic over the two halves so that a test can put them in memory: what the
/// binary passes is standard input and standard output.
pub struct Link<R: Read, W: Write> {
    reader: R,
    writer: W,
    payload: Vec<u8>,
}

impl<R: Read, W: Write> Link<R, W> {
    /// A link over one reader and one writer.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            payload: Vec::new(),
        }
    }

    /// Waits for the next message.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Closed`] when the far end hung up at a message boundary,
    /// which is what a VMLord that exited looks like; [`LaunchError::TooLarge`]
    /// for a prefix above [`MAX_MESSAGE`], refused before anything is
    /// allocated; and whatever [`decode`] can return.
    pub fn read(&mut self) -> Result<Message, LaunchError> {
        let mut prefix = [0u8; 4];
        let mut filled = 0;
        while filled < prefix.len() {
            match self.reader.read(&mut prefix[filled..]) {
                Ok(0) if filled == 0 => return Err(LaunchError::Closed),
                Ok(0) => {
                    return Err(LaunchError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "a launch pipe ended part-way through a length prefix",
                    )));
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(LaunchError::Io(error)),
            }
        }

        let length = u32::from_le_bytes(prefix);
        if length > MAX_MESSAGE {
            return Err(LaunchError::TooLarge {
                length,
                cap: MAX_MESSAGE,
            });
        }

        self.payload.clear();
        self.payload.resize(length as usize, 0);
        self.reader
            .read_exact(&mut self.payload)
            .map_err(LaunchError::Io)?;

        decode(&self.payload)
    }

    /// Writes one message and flushes it.
    ///
    /// Flushing belongs here: a buffered pipe that holds a relay back is a
    /// handshake that appears to have stalled.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Io`] if the pipe failed.
    pub fn write(&mut self, message: &Message) -> Result<(), LaunchError> {
        let bytes = encode(message);
        let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        if length > MAX_MESSAGE {
            return Err(LaunchError::TooLarge {
                length,
                cap: MAX_MESSAGE,
            });
        }

        self.writer
            .write_all(&length.to_le_bytes())
            .map_err(LaunchError::Io)?;
        self.writer.write_all(&bytes).map_err(LaunchError::Io)?;
        self.writer.flush().map_err(LaunchError::Io)
    }
}

/// Why a launch pipe could not be used.
#[derive(Debug)]
pub enum LaunchError {
    /// The far end hung up at a message boundary.
    Closed,
    /// A message from another revision of this contract.
    Revision {
        /// What this build speaks.
        expected: u32,
        /// What arrived.
        found: u32,
    },
    /// An envelope that names no message, or a command this build has no name
    /// for.
    Empty,
    /// A fixed-width field arrived at another width.
    Field {
        /// Which field.
        what: &'static str,
        /// How long it was.
        len: usize,
    },
    /// A message longer than the pipe's cap.
    TooLarge {
        /// What the prefix announced.
        length: u32,
        /// What this build allows.
        cap: u32,
    },
    /// The bytes are not an envelope.
    Decode(prost::DecodeError),
    /// The pipe failed.
    Io(io::Error),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("the launch pipe was closed by VMLord"),
            Self::Revision { expected, found } => write!(
                formatter,
                "a launch message of revision {found} arrived where this build speaks {expected}"
            ),
            Self::Empty => formatter.write_str("a launch message names nothing this build knows"),
            Self::Field { what, len } => {
                write!(formatter, "a {what} of {len} bytes is the wrong width")
            }
            Self::TooLarge { length, cap } => write!(
                formatter,
                "a {length}-byte launch message exceeds the {cap}-byte limit"
            ),
            Self::Decode(error) => write!(formatter, "a launch message is unreadable: {error}"),
            Self::Io(error) => write!(formatter, "a launch pipe failed: {error}"),
        }
    }
}

impl Error for LaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
```

- [ ] **Step 7: Write the logger and the crate root**

Create `crates/display-viewer/src/log.rs`:

```rust
//! Where this process writes what it did, and what it must never write.
//!
//! # The redaction rule
//!
//! Decoded pixels, cursor bitmaps and the codec payloads they came from never
//! reach a log record. What may: sizes, sequences, generations, geometry,
//! session states, error codes and the text of an `Error` record. There is no
//! screenshot feature in this build, and adding one would need a warning to the
//! user before it wrote anything -- the rule is stated here so that nobody adds
//! one quietly.

/// Brings the application log up, so that a viewer's story lands in the same
/// file as VMLord's.
///
/// A viewer that cannot log still shows a desktop: a failure here is reported
/// to standard error and nothing else. Losing the log is not worth losing the
/// session.
pub fn initialize() {
    let settings = vmlord_core::SettingsStore::for_current_user()
        .and_then(|store| store.load_or_create());

    match settings {
        Ok(settings) => {
            if let Err(error) = vmlord_core::initialize_logging(&settings) {
                eprintln!("VMLord Display: the log could not be opened: {error}");
            }
        }
        Err(error) => eprintln!("VMLord Display: settings could not be read: {error}"),
    }
}

/// A logger that keeps every record, for the tests that assert what is not in
/// them.
#[cfg(test)]
pub mod capture {
    use std::sync::{Mutex, OnceLock};

    use log::{Level, LevelFilter, Log, Metadata, Record};

    static RECORDS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

    struct Capture;

    impl Log for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &Record<'_>) {
            if record.level() <= Level::Trace {
                records()
                    .lock()
                    .expect("no test panics while holding the log")
                    .push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    fn records() -> &'static Mutex<Vec<String>> {
        RECORDS.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Installs the capturing logger. Safe to call from every test.
    pub fn install() {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            let _ = log::set_logger(&Capture);
            log::set_max_level(LevelFilter::Trace);
        });
    }

    /// Everything logged so far, joined.
    pub fn text() -> String {
        records()
            .lock()
            .expect("no test panics while holding the log")
            .join("\n")
    }
}
```

Create `crates/display-viewer/src/lib.rs`:

```rust
//! The host end of one VMLord display session.
//!
//! A process of its own rather than part of VMLord: a session outlives the
//! application that started it, and a crash in either must leave the other
//! standing. What is here is everything but the window -- the launch contract,
//! the session states, the decode path -- and `src/windows/` is the four
//! modules that touch Win32.

pub mod launch;
pub mod log;

/// The generated types for the launch pipes' private schema.
pub mod viewer {
    /// One version module, the way the wire contract has one.
    pub mod v1 {
        // Generated code is not written to this repository's standards and
        // cannot be, so it is not linted against them.
        #![allow(clippy::all, clippy::pedantic, missing_docs)]

        include!(concat!(env!("OUT_DIR"), "/vmlord.display.viewer.v1.rs"));
    }
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — seven tests in `launch::tests`.

- [ ] **Step 9: Commit**

```bash
git add crates/display-viewer Cargo.toml
git commit -m "TASK-117: Add the display viewer crate and its launch contract"
```

---

### Task 2: The established host session

**Files:**
- Modify: `crates/display-protocol/src/session.rs`
- Test: `crates/display-protocol/src/session.rs` (its own `mod tests`)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `vmlord_display_protocol::session::HandedOver { session_id: [u8; SESSION_ID_LEN], negotiated: Negotiated, frame_key: ChannelKey, input_key: ChannelKey, control_sequence: u32 }`
  - `Session::established_host(handed_over: HandedOver) -> Session`
  - `Session::control_sequence(&self) -> u32`

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` at the bottom of `crates/display-protocol/src/session.rs`:

```rust
    /// Runs a full handshake and returns the host's session, the guest's, and
    /// what a hand-over to another process would carry.
    fn handshaken() -> (Session, Session, HandedOver) {
        let secret = Secret::generate();
        let (mut host, hello) = Session::host(
            &secret,
            Offer {
                capabilities: vec![Capability::CursorStream],
                mode: Mode::Auto,
                width: 1920,
                height: 1080,
                tile_size: 32,
            },
        );
        let mut guest = Session::guest(
            &secret,
            Support {
                capabilities: vec![Capability::CursorStream],
                modes: vec![Mode::Desktop],
                tile_sizes: vec![16, 32, 64],
                width: 1920,
                height: 1080,
            },
        );

        let server_hello = guest
            .handle(&hello.header, &hello.payload)
            .expect("a hello this guest can answer")
            .reply
            .expect("the guest answers a hello");
        let server_auth = guest.pending_auth().expect("the guest queued its proof");

        host.handle(&server_hello.header, &server_hello.payload)
            .expect("an answer this host offered");
        let client_auth = host
            .handle(&server_auth.header, &server_auth.payload)
            .expect("a proof this host can check")
            .reply
            .expect("the host answers with its own proof");
        guest
            .handle(&client_auth.header, &client_auth.payload)
            .expect("a proof this guest can check");

        let handed_over = HandedOver {
            session_id: *host.session_id(),
            negotiated: host.negotiated().expect("an established host").clone(),
            frame_key: host
                .derive_channel_key(Channel::Frame)
                .expect("an established host"),
            input_key: host
                .derive_channel_key(Channel::Input)
                .expect("an established host"),
            control_sequence: host.control_sequence(),
        };

        (host, guest, handed_over)
    }

    /// Drives one channel bind between a handed-over host and a guest.
    fn bind(host: &mut Session, guest: &mut Session, hello: Record) -> Event {
        let ack = guest
            .handle(&hello.header, &hello.payload)
            .expect("a channel hello this guest can answer")
            .reply
            .expect("the guest answers a channel hello");
        let outcome = host
            .handle(&ack.header, &ack.payload)
            .expect("an ack this host can check");
        let auth = outcome.reply.expect("the host answers with its own proof");
        guest
            .handle(&auth.header, &auth.payload)
            .expect("a proof this guest can check");

        outcome.event
    }

    #[test]
    fn a_handed_over_session_binds_its_channels_without_the_secret() {
        let (_, mut guest, handed_over) = handshaken();
        let mut viewer = Session::established_host(handed_over);

        let hello = viewer
            .open_channel(Channel::Frame)
            .expect("an established host opens channels");
        assert_eq!(
            bind(&mut viewer, &mut guest, hello),
            Event::ChannelBound(Channel::Frame)
        );

        let hello = viewer
            .open_channel(Channel::Input)
            .expect("an established host opens channels");
        assert_eq!(
            bind(&mut viewer, &mut guest, hello),
            Event::ChannelBound(Channel::Input)
        );
    }

    #[test]
    fn a_handed_over_session_reconnects_a_channel_at_the_next_generation() {
        let (_, mut guest, handed_over) = handshaken();
        let mut viewer = Session::established_host(handed_over);

        let hello = viewer.open_channel(Channel::Frame).expect("generation 0");
        bind(&mut viewer, &mut guest, hello);
        assert_eq!(viewer.generation(Channel::Frame), 0);

        // The key survives the reconnect: it was handed over rather than
        // derived, and there is no session key here to derive it again from.
        let hello = viewer
            .reconnect_channel(Channel::Frame)
            .expect("a channel may be replaced");
        assert_eq!(viewer.generation(Channel::Frame), 1);
        assert_eq!(
            bind(&mut viewer, &mut guest, hello),
            Event::ChannelBound(Channel::Frame)
        );
    }

    #[test]
    fn a_handed_over_session_refuses_a_record_from_the_generation_it_replaced() {
        let (_, mut guest, handed_over) = handshaken();
        let mut viewer = Session::established_host(handed_over);

        let hello = viewer.open_channel(Channel::Frame).expect("generation 0");
        bind(&mut viewer, &mut guest, hello);
        let hello = viewer
            .reconnect_channel(Channel::Frame)
            .expect("a channel may be replaced");
        bind(&mut viewer, &mut guest, hello);

        let stale = Record::new(Channel::Frame, FrameRecord::Keyframe as u16, 0, 0, 0, vec![]);
        assert!(matches!(
            viewer.accept(&stale.header),
            Err(SessionError::StaleGeneration {
                expected: 1,
                found: 0,
                ..
            })
        ));
    }

    #[test]
    fn a_handed_over_session_carries_on_the_control_numbering_it_was_given() {
        let (host, _, handed_over) = handshaken();
        // The host wrote a `ClientHello` and a `ClientAuth`, so the next
        // control record it would write is sequence 2.
        assert_eq!(host.control_sequence(), 2);

        let mut viewer = Session::established_host(handed_over);
        let ping = viewer.control_record(ControlRecord::Ping, Vec::new());

        assert_eq!(ping.header.sequence, 2);
        assert_eq!(viewer.control_sequence(), 3);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol`
Expected: FAIL — `cannot find struct `HandedOver`` and `no method named `control_sequence``.

- [ ] **Step 3: Make the secret optional**

In `crates/display-protocol/src/session.rs`, change the field and its two constructors:

```rust
pub struct Session {
    role: Role,
    state: State,
    /// The VM's secret, which only a session that runs its own handshake has.
    /// A session built from a hand-over never sees one -- see
    /// [`Session::established_host`].
    secret: Option<Secret>,
```

In `host_with_randomness` and `guest_with_randomness`, replace `secret: secret.duplicate(),` with `secret: Some(secret.duplicate()),`.

In `derive_session_key`, replace the `&self.secret` argument:

```rust
        let key = keys::session_key(
            self.secret
                .as_ref()
                .expect("a session that handshakes holds the secret"),
            &self.session_id,
            &self.host_nonce.expect("the client hello carried it"),
            &self.guest_nonce.expect("the server hello carried it"),
        );
```

Add the handed-over keys beside the channel state, in the same `struct Session`:

```rust
    /// The channel keys a hand-over carried, which outlive a reconnect.
    ///
    /// A session that handshook derives these from its session key and its
    /// transcript whenever it needs them. One built from a hand-over has
    /// neither, so it keeps what it was given: a channel key depends on the
    /// session and the channel, never on the generation, so replacing a socket
    /// does not replace it.
    handover_keys: [Option<ChannelKey>; 2],
```

Initialise it as `handover_keys: [None, None],` in both existing constructors.

- [ ] **Step 4: Add the constructor and the accessor**

Add to `impl Session`, directly after `guest_with_randomness`:

```rust
    /// Builds the established host half of a session another process handshook.
    ///
    /// The process that holds the VM's secret runs the four control records and
    /// hands the result here: what the handshake settled on, the session id,
    /// the two channel keys, and where the control channel's numbering had got
    /// to. This session derives nothing and holds no secret, which is the whole
    /// point -- a viewer that is compromised loses one session's channel keys
    /// and cannot open a second.
    ///
    /// The state is the one [`Session::handle`] would have reached, so
    /// everything downstream -- [`Session::open_channel`],
    /// [`Session::reconnect_channel`], [`Session::accept`] -- is this crate's
    /// arithmetic rather than a caller's.
    #[must_use]
    pub fn established_host(handed_over: HandedOver) -> Self {
        Self {
            role: Role::Host,
            state: State::Established,
            secret: None,
            offer: None,
            support: None,
            session_id: handed_over.session_id,
            host_nonce: None,
            guest_nonce: None,
            transcript: Transcript::new(),
            transcript_hash: None,
            session_key: None,
            negotiated: Some(handed_over.negotiated),
            pending: None,
            pending_auth: None,
            control_sequence: handed_over.control_sequence,
            channels: [ChannelState::default(), ChannelState::default()],
            handover_keys: [Some(handed_over.frame_key), Some(handed_over.input_key)],
        }
    }

    /// The sequence this session's next control record will carry.
    ///
    /// What a hand-over passes on, so that the process taking a session over
    /// does not restart a stream the guest has already seen part of.
    #[must_use]
    pub fn control_sequence(&self) -> u32 {
        self.control_sequence
    }
```

Add the type, directly above `impl Session`:

```rust
/// An established session, as it crosses from one process to another.
///
/// Everything the receiving process needs and nothing it does not: no secret,
/// no session key, no transcript. See [`Session::established_host`].
pub struct HandedOver {
    /// The 16 bytes that name the session across its three sockets.
    pub session_id: [u8; SESSION_ID_LEN],
    /// What the control handshake settled on.
    pub negotiated: Negotiated,
    /// The key the frame socket proves itself with.
    pub frame_key: ChannelKey,
    /// The key the input socket proves itself with.
    pub input_key: ChannelKey,
    /// The sequence the control channel carries on from.
    pub control_sequence: u32,
}
```

- [ ] **Step 5: Let a handed-over key answer for a channel**

Replace `established_channel_key` in `crates/display-protocol/src/session.rs`:

```rust
    /// Derives this session's key for `channel`, where it must be there.
    ///
    /// The infallible half of [`Session::derive_channel_key`], for the handlers
    /// that only run once the session is established. A session built from a
    /// hand-over has no session key to derive from and answers with the key it
    /// was given.
    fn established_channel_key(&self, channel: Channel) -> ChannelKey {
        let index = self
            .channel_index(channel)
            .expect("a channel key is never asked for on control");

        if let Some(key) = self.handover_keys[index].as_ref() {
            return ChannelKey::from_bytes(*key.to_bytes());
        }

        keys::channel_key(
            self.session_key
                .as_ref()
                .expect("an established session derived one"),
            &self
                .transcript_hash
                .expect("an established session finished its transcript"),
            channel,
        )
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol`
Expected: PASS — the four new tests and every existing one.

- [ ] **Step 7: Record the crossing in the module documentation**

Add to the module comment at the top of `crates/display-protocol/src/session.rs`, after the existing paragraph:

```rust
//! A session need not be run by the process that opened it.
//! [`Session::established_host`] takes a [`HandedOver`] -- what the handshake
//! settled on, the session id, two channel keys and the control sequence -- and
//! produces the established host half without a secret. That is how VMLord
//! keeps the VM's secret while the viewer keeps the sockets, and it is the
//! host's mirror of what the guest's broker does for its capture process.
```

- [ ] **Step 8: Commit**

```bash
git add crates/display-protocol/src/session.rs
git commit -m "TASK-117: Build an established host session from a hand-over"
```

---

### Task 3: The status machine and the overlay's geometry

**Files:**
- Create: `crates/display-viewer/src/status.rs`
- Modify: `crates/display-viewer/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `vmlord_display_viewer::status::{Status, Event, Progress, Button, RETRY_BUDGET, buttons, hit_test}`
  - `Status` variants: `Starting`, `Waiting`, `Authenticating`, `Running`, `Reconnecting`, `Failed(String)`, `Gone`
  - `Event` variants: `Connected`, `Established`, `ChannelLost`, `ControlLost`, `NoParent`, `PartitionGone`, `Retry`
  - `Progress::new(now: Instant) -> Progress`, `Progress::status(&self) -> &Status`, `Progress::on(&mut self, event: Event, now: Instant)`, `Progress::tick(&mut self, now: Instant)`, `Progress::is_running(&self) -> bool`, `Progress::label(&self) -> &str`
  - `Button` variants: `Retry`, `Cancel`
  - `buttons(width: i32, height: i32) -> [(Button, (i32, i32, i32, i32)); 2]`, `hit_test(width: i32, height: i32, x: i32, y: i32) -> Option<Button>`

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/status.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Button, Event, Progress, RETRY_BUDGET, Status, hit_test};

    #[test]
    fn a_viewer_starts_by_waiting_for_the_guest() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        assert_eq!(progress.status(), &Status::Starting);

        progress.tick(now + Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);
    }

    #[test]
    fn a_connection_authenticates_and_then_runs() {
        let now = Instant::now();
        let mut progress = Progress::new(now);

        progress.on(Event::Connected, now);
        assert_eq!(progress.status(), &Status::Authenticating);

        progress.on(Event::Established, now);
        assert_eq!(progress.status(), &Status::Running);
        assert!(progress.is_running());
    }

    #[test]
    fn a_running_session_that_loses_a_channel_reconnects() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::Connected, now);
        progress.on(Event::Established, now);

        progress.on(Event::ChannelLost, now);
        assert_eq!(progress.status(), &Status::Reconnecting);
        assert!(!progress.is_running());
    }

    #[test]
    fn a_running_session_that_loses_control_reconnects_too() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::Connected, now);
        progress.on(Event::Established, now);

        progress.on(Event::ControlLost, now);
        assert_eq!(progress.status(), &Status::Reconnecting);
    }

    #[test]
    fn a_state_that_never_succeeds_fails_when_its_budget_runs_out() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.tick(now + Duration::from_millis(1));

        progress.tick(now + RETRY_BUDGET - Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);

        progress.tick(now + RETRY_BUDGET);
        assert!(matches!(progress.status(), Status::Failed(_)));
    }

    #[test]
    fn the_budget_starts_again_at_every_state_it_governs() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.tick(now + Duration::from_millis(1));

        let late = now + RETRY_BUDGET - Duration::from_millis(1);
        progress.on(Event::Connected, late);

        // Authenticating began at `late`, so the budget it runs under is its
        // own rather than what was left of the wait's.
        progress.tick(late + RETRY_BUDGET - Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Authenticating);
        progress.tick(late + RETRY_BUDGET);
        assert!(matches!(progress.status(), Status::Failed(_)));
    }

    #[test]
    fn retry_starts_the_cycle_again_with_a_fresh_budget() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.tick(now + RETRY_BUDGET);
        assert!(matches!(progress.status(), Status::Failed(_)));

        let pressed = now + RETRY_BUDGET + Duration::from_secs(60);
        progress.on(Event::Retry, pressed);
        assert_eq!(progress.status(), &Status::Starting);

        progress.tick(pressed + Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);
        progress.tick(pressed + RETRY_BUDGET - Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);
    }

    #[test]
    fn a_failed_state_stays_failed_until_it_is_retried() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::ControlLost, now);
        progress.tick(now + RETRY_BUDGET);

        let Status::Failed(first) = progress.status().clone() else {
            panic!("the budget ran out");
        };
        progress.tick(now + RETRY_BUDGET * 4);

        assert_eq!(progress.status(), &Status::Failed(first));
    }

    #[test]
    fn a_viewer_whose_parent_is_gone_fails_at_once_rather_than_waiting() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::Connected, now);
        progress.on(Event::Established, now);

        progress.on(Event::NoParent, now);

        assert!(matches!(progress.status(), Status::Failed(_)));
    }

    #[test]
    fn a_stopped_vm_is_not_a_failure() {
        let now = Instant::now();
        let mut progress = Progress::new(now);

        progress.on(Event::PartitionGone, now);

        assert_eq!(progress.status(), &Status::Gone);
        // And it stays there: nothing is retried for a VM that is not running.
        progress.tick(now + RETRY_BUDGET * 4);
        assert_eq!(progress.status(), &Status::Gone);
    }

    #[test]
    fn every_state_has_a_word_for_itself() {
        let now = Instant::now();
        let mut progress = Progress::new(now);

        for (event, label) in [
            (Event::Connected, "Authenticating"),
            (Event::Established, "Running"),
            (Event::ChannelLost, "Reconnecting"),
        ] {
            progress.on(event, now);
            assert_eq!(progress.label(), label);
        }
    }

    #[test]
    fn the_buttons_are_hit_tested_by_rectangle() {
        let (width, height) = (800, 600);
        let mut found = Vec::new();

        for (button, (x, y, w, h)) in super::buttons(width, height) {
            // The middle of each rectangle answers with its own button.
            assert_eq!(hit_test(width, height, x + w / 2, y + h / 2), Some(button));
            found.push(button);
        }

        assert_eq!(found, vec![Button::Retry, Button::Cancel]);
        assert_eq!(hit_test(width, height, 0, 0), None);
        assert_eq!(hit_test(width, height, width, height), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `Progress` in this scope` and the rest.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/display-viewer/src/status.rs`:

```rust
//! Where the session is, in the words the window puts on the screen.
//!
//! One budget governs every path into a non-running state: thirty seconds of
//! active retry from the moment the state began, and then a `Failed` screen
//! with two working buttons. The clock is a parameter rather than a field, so
//! that the table below is tested at whatever time a test likes and the window
//! passes the time it drew at.

use std::time::{Duration, Instant};

/// How long a state that is not running retries before it gives up.
///
/// The ticket's thirty seconds. Long enough for a guest whose services are
/// restarting, short enough that a user is not left watching a word.
pub const RETRY_BUDGET: Duration = Duration::from_secs(30);

/// The height of a button on the failed screen, in pixels.
const BUTTON_HEIGHT: i32 = 36;

/// The width of a button on the failed screen, in pixels.
const BUTTON_WIDTH: i32 = 120;

/// The gap between the two buttons, in pixels.
const BUTTON_GAP: i32 = 16;

/// How far below the middle of the window the buttons sit.
const BUTTON_OFFSET: i32 = 48;

/// What the viewer is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Spawned, and about to try the control socket.
    Starting,
    /// Trying the control socket. The guest's services are absent or restarting.
    Waiting,
    /// Connected, and relaying a handshake it does not parse.
    Authenticating,
    /// Frames are decoding. The overlay is gone.
    Running,
    /// Something dropped and is being replaced.
    Reconnecting,
    /// The budget ran out, or something happened that patience will not fix.
    Failed(String),
    /// The VM is not running. Not a failure, and not retried.
    Gone,
}

impl Status {
    /// Whether this state runs under [`RETRY_BUDGET`].
    fn is_retrying(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Waiting | Self::Authenticating | Self::Reconnecting
        )
    }
}

/// What happened to the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The control socket connected and the handshake is under way.
    Connected,
    /// Both peers proved themselves and the frame channel is bound.
    Established,
    /// A frame or input socket dropped and will be rebound.
    ChannelLost,
    /// Control dropped, which needs a new session and so a new handshake.
    ControlLost,
    /// A new session is needed and VMLord is not there to run the handshake.
    NoParent,
    /// The compute system is gone: a stopped VM rather than a fault.
    PartitionGone,
    /// The user pressed Retry.
    Retry,
}

/// The state machine behind the overlay.
pub struct Progress {
    status: Status,
    /// When the current state began, which is what its budget is measured from.
    entered: Instant,
}

impl Progress {
    /// A viewer that has just started.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            status: Status::Starting,
            entered: now,
        }
    }

    /// Where the session is.
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Whether frames are on the screen.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.status, Status::Running)
    }

    /// The word the overlay puts on the screen.
    #[must_use]
    pub fn label(&self) -> &str {
        match self.status {
            Status::Starting => "Starting",
            Status::Waiting => "Waiting",
            Status::Authenticating => "Authenticating",
            Status::Running => "Running",
            Status::Reconnecting => "Reconnecting",
            Status::Failed(_) => "Failed",
            Status::Gone => "Closing",
        }
    }

    /// Moves the machine on, if `event` means anything where it is.
    pub fn on(&mut self, event: Event, now: Instant) {
        let next = match (&self.status, event) {
            // A VM that is not running closes the window; nothing is retried.
            (_, Event::PartitionGone) => Status::Gone,
            (Status::Gone, _) => return,
            (_, Event::Retry) => Status::Starting,
            (_, Event::NoParent) => Status::Failed(
                "VMLord is no longer running, and a new session needs it".to_owned(),
            ),
            (Status::Failed(_), _) => return,
            (_, Event::Connected) => Status::Authenticating,
            (_, Event::Established) => Status::Running,
            (_, Event::ChannelLost | Event::ControlLost) => Status::Reconnecting,
        };

        self.enter(next, now);
    }

    /// Lets time pass, which is the only thing that produces a failure.
    pub fn tick(&mut self, now: Instant) {
        if matches!(self.status, Status::Starting) {
            self.enter(Status::Waiting, now);
            return;
        }

        if !self.status.is_retrying() {
            return;
        }

        if now.duration_since(self.entered) >= RETRY_BUDGET {
            let reason = format!(
                "{} for {} seconds without reaching the guest's display services",
                self.label(),
                RETRY_BUDGET.as_secs()
            );
            self.enter(Status::Failed(reason), now);
        }
    }

    /// Enters a state, restarting the budget with it.
    fn enter(&mut self, status: Status, now: Instant) {
        if self.status == status {
            return;
        }

        log::info!("the display session is {status:?}");
        self.status = status;
        self.entered = now;
    }
}

/// A button on the failed screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Start the cycle again with a fresh budget.
    Retry,
    /// Close the window.
    Cancel,
}

/// Where the two buttons sit in a window of this size, as `(x, y, w, h)`.
///
/// Plain arithmetic rather than a control: there are two rectangles on one
/// screen, and a hit test over them is shorter than anything that would draw
/// them for us -- and it is testable without a window.
#[must_use]
pub fn buttons(width: i32, height: i32) -> [(Button, (i32, i32, i32, i32)); 2] {
    let y = height / 2 + BUTTON_OFFSET;
    let total = BUTTON_WIDTH * 2 + BUTTON_GAP;
    let left = (width - total) / 2;

    [
        (Button::Retry, (left, y, BUTTON_WIDTH, BUTTON_HEIGHT)),
        (
            Button::Cancel,
            (
                left + BUTTON_WIDTH + BUTTON_GAP,
                y,
                BUTTON_WIDTH,
                BUTTON_HEIGHT,
            ),
        ),
    ]
}

/// Which button, if any, a click at `(x, y)` landed on.
#[must_use]
pub fn hit_test(width: i32, height: i32, x: i32, y: i32) -> Option<Button> {
    buttons(width, height)
        .into_iter()
        .find(|(_, (bx, by, bw, bh))| x >= *bx && x < bx + bw && y >= *by && y < by + bh)
        .map(|(button, _)| button)
}
```

- [ ] **Step 4: Declare the module**

In `crates/display-viewer/src/lib.rs`, add `pub mod status;` after `pub mod log;`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — twelve tests in `status::tests`, seven in `launch::tests`.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/src/status.rs crates/display-viewer/src/lib.rs
git commit -m "TASK-117: Add the display viewer's status machine"
```

---

### Task 4: The video pipeline

**Files:**
- Create: `crates/display-viewer/src/video.rs`
- Modify: `crates/display-viewer/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: `vmlord_display_viewer::log::capture` (Task 1).
- Produces:
  - `vmlord_display_viewer::video::{Video, Update, VideoError}`
  - `Video::new() -> Video`
  - `Video::apply(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError>`
  - `Video::geometry(&self) -> Option<Geometry>`, `Video::frame(&self) -> Option<&[u8]>`
  - `Update` variants: `Nothing`, `Configured(Geometry)`, `Damage(Vec<Rect>)`, `Cursor(OwnedCursorImage)`, `Moved(CursorPosition)`
  - `VideoError` variants: `Rebind(String)`, `Fatal(String)`

The tests drive the real encoder against the real decoder: a stub on either side would only prove the stub agrees with itself. `vmlord_display_codec::scenes` is unconditionally public, so no dev-dependency and no feature is needed.

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/video.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use prost::Message as _;
    use vmlord_display_codec::{
        Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, Rect, TileSize,
        scenes::{Generator, Scene},
    };
    use vmlord_display_protocol::{
        record::{Channel, Header, Record},
        v1::{FrameRecord, PixelFormat as WireFormat, StreamConfig},
    };

    use super::{Update, Video, VideoError};

    fn geometry() -> Geometry {
        Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows")
    }

    fn config_record(width: u32, height: u32, tile_size: u32) -> Record {
        let config = StreamConfig {
            width,
            height,
            tile_size,
            pixel_format: WireFormat::Bgra8888 as i32,
        };

        Record::new(
            Channel::Frame,
            FrameRecord::StreamConfig as u16,
            0,
            0,
            0,
            config.encode_to_vec(),
        )
    }

    fn frame_record(kind: FrameRecord, sequence: u32, base: u32, payload: Vec<u8>) -> Record {
        Record::new(Channel::Frame, kind as u16, sequence, base, 0, payload)
    }

    /// The records a real encoder produces for one scene, in order.
    fn stream(frames: usize) -> Vec<Record> {
        let geometry = geometry();
        let mut encoder = Encoder::new(EncoderConfig::new(geometry));
        let mut generator = Generator::new(Scene::Typing, geometry, 7);
        let mut records = Vec::new();
        let mut sequence = 1;
        let mut last_frame = 0;

        for _ in 0..frames {
            let pixels = generator.next_frame().to_vec();
            let damage = generator.damage().to_vec();
            encoder
                .submit(
                    Frame {
                        pixels: &pixels,
                        stride: geometry.width() as usize * 4,
                    },
                    Some(&damage),
                )
                .expect("a frame of this geometry");

            while let Some(payload) = encoder.next_payload() {
                let (kind, is_frame, bytes) = match payload {
                    Payload::Keyframe(bytes) => (FrameRecord::Keyframe, true, bytes.to_vec()),
                    Payload::TileDelta(bytes) => (FrameRecord::TileDelta, true, bytes.to_vec()),
                    Payload::CursorImage(bytes) => (FrameRecord::CursorImage, false, bytes.to_vec()),
                    Payload::CursorPosition(bytes) => {
                        (FrameRecord::CursorPosition, false, bytes.to_vec())
                    }
                };
                let base = if kind == FrameRecord::TileDelta {
                    last_frame
                } else {
                    0
                };
                records.push(frame_record(kind, sequence, base, bytes));
                if is_frame {
                    last_frame = sequence;
                }
                sequence += 1;
            }
        }

        records
    }

    #[test]
    fn a_stream_config_builds_the_decoder_the_frames_need() {
        let mut video = Video::new();
        let record = config_record(320, 200, 32);

        let update = video
            .apply(&record.header, &record.payload)
            .expect("a config the codec allows");

        assert_eq!(update, Update::Configured(geometry()));
        assert_eq!(video.geometry(), Some(geometry()));
    }

    #[test]
    fn a_second_stream_config_replaces_the_decoder() {
        let mut video = Video::new();
        let first = config_record(320, 200, 32);
        video.apply(&first.header, &first.payload).expect("a config");

        let second = config_record(640, 480, 64);
        video
            .apply(&second.header, &second.payload)
            .expect("a config");

        let replaced = Geometry::new(640, 480, TileSize::SixtyFour, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows");
        assert_eq!(video.geometry(), Some(replaced));
    }

    #[test]
    fn a_geometry_the_codec_refuses_is_fatal_rather_than_a_rebind() {
        let mut video = Video::new();
        let record = config_record(320, 200, 48);

        assert!(matches!(
            video.apply(&record.header, &record.payload),
            Err(VideoError::Fatal(_))
        ));
    }

    #[test]
    fn a_keyframe_and_its_deltas_decode_into_the_rectangles_that_changed() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video.apply(&config.header, &config.payload).expect("a config");

        let mut frames = 0;
        for record in stream(4) {
            let update = video
                .apply(&record.header, &record.payload)
                .expect("a record this encoder wrote");

            if let Update::Damage(damage) = update {
                assert!(!damage.is_empty(), "a frame record changed nothing");
                for rect in damage {
                    assert!(rect.x + rect.width <= 320);
                    assert!(rect.y + rect.height <= 200);
                }
                frames += 1;
            }
        }

        assert!(frames >= 2, "the encoder wrote a keyframe and some deltas");
        assert_eq!(video.frame().map(<[u8]>::len), Some(320 * 200 * 4));
    }

    #[test]
    fn a_delta_before_any_keyframe_asks_for_one_by_rebinding() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video.apply(&config.header, &config.payload).expect("a config");

        let delta = stream(2)
            .into_iter()
            .find(|record| record.header.message_type == FrameRecord::TileDelta as u16)
            .expect("the scene produced a delta");
        // Its base names the keyframe, which this decoder never received.
        let update = video.apply(&delta.header, &delta.payload);

        assert!(matches!(update, Err(VideoError::Rebind(_))));
    }

    #[test]
    fn a_delta_built_on_a_record_this_connection_never_sent_is_refused() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video.apply(&config.header, &config.payload).expect("a config");

        let records = stream(3);
        for record in &records {
            if record.header.message_type == FrameRecord::TileDelta as u16 {
                // The same delta, claiming to build on a frame that was never
                // sent. The picture it would produce is wrong in a way no
                // error surfaces, so the channel is rebound instead.
                let mut header = record.header;
                header.base = header.sequence + 100;

                assert!(matches!(
                    video.apply(&header, &record.payload),
                    Err(VideoError::Rebind(_))
                ));
                return;
            }
            video
                .apply(&record.header, &record.payload)
                .expect("a record this encoder wrote");
        }

        panic!("the scene produced no delta");
    }

    #[test]
    fn a_corrupted_payload_rebinds_rather_than_ending_the_session() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video.apply(&config.header, &config.payload).expect("a config");

        let mut keyframe = stream(1)
            .into_iter()
            .find(|record| record.header.message_type == FrameRecord::Keyframe as u16)
            .expect("the first frame is a keyframe");
        keyframe.payload.truncate(keyframe.payload.len() / 2);

        assert!(matches!(
            video.apply(&keyframe.header, &keyframe.payload),
            Err(VideoError::Rebind(_))
        ));
    }

    #[test]
    fn a_frame_record_before_any_stream_config_is_a_rebind() {
        let mut video = Video::new();
        let keyframe = frame_record(FrameRecord::Keyframe, 1, 0, vec![0; 8]);

        assert!(matches!(
            video.apply(&keyframe.header, &keyframe.payload),
            Err(VideoError::Rebind(_))
        ));
    }

    #[test]
    fn a_record_this_build_has_no_name_for_changes_nothing() {
        let mut video = Video::new();
        let unknown = frame_record(FrameRecord::Unspecified, 1, 0, vec![1, 2, 3]);

        assert_eq!(
            video
                .apply(&unknown.header, &unknown.payload)
                .expect("an unknown record is not a fault"),
            Update::Nothing
        );
    }

    #[test]
    fn a_decoded_stream_never_reaches_the_log() {
        crate::log::capture::install();

        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video.apply(&config.header, &config.payload).expect("a config");
        for record in stream(4) {
            let _ = video.apply(&record.header, &record.payload);
        }

        let text = crate::log::capture::text();
        let pixels = video.frame().expect("a decoded frame").to_vec();
        // Sixteen bytes is four pixels: long enough that a match is not a
        // coincidence, short enough to catch a partial dump.
        for window in pixels.chunks_exact(16).take(64) {
            let hex: String = window.iter().map(|byte| format!("{byte:02x}")).collect();
            assert!(
                !text.contains(&hex),
                "framebuffer content reached the log"
            );
        }
        assert!(!text.is_empty(), "the decode path logged nothing at all");
    }

    #[test]
    fn the_damage_a_delta_reports_is_the_damage_the_encoder_wrote() {
        let geometry = geometry();
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video.apply(&config.header, &config.payload).expect("a config");

        let mut tiles: Vec<Rect> = Vec::new();
        for record in stream(3) {
            if let Ok(Update::Damage(damage)) = video.apply(&record.header, &record.payload) {
                tiles = damage.to_vec();
            }
        }

        // Every rectangle a delta reports is one of the grid's tiles.
        let grid: Vec<Rect> = (0..geometry.tile_count())
            .filter_map(|index| geometry.tile(index))
            .collect();
        for rect in tiles {
            assert!(grid.contains(&rect), "{rect:?} is not a tile of the grid");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `Video` in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/display-viewer/src/video.rs`:

```rust
//! The frame channel's records, turned into pixels and the rectangles that
//! changed.
//!
//! Everything here arrives from another machine, so nothing here trusts it: the
//! codec checks every length against the geometry it was built for, and this
//! module checks what the codec cannot -- that a delta builds on a record this
//! connection actually sent.
//!
//! Two kinds of failure, and the difference matters. A [`VideoError::Rebind`]
//! is a frame channel that cannot continue but a session that can: close the
//! socket, reconnect at the next generation, and the guest owes a
//! `StreamConfig` and a keyframe again. A [`VideoError::Fatal`] is a stream
//! this build cannot display at all.

use prost::Message as _;
use vmlord_display_codec::{
    CodecError, CursorPosition, Decoder, Geometry, OwnedCursorImage, PixelFormat, Rect, TileSize,
};
use vmlord_display_protocol::{
    record::Header,
    v1::{FrameRecord, PixelFormat as WireFormat, StreamConfig},
};

/// What one frame record meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// Nothing the window has to draw.
    Nothing,
    /// The stream's geometry, which the window sizes its texture to.
    Configured(Geometry),
    /// The rectangles of the frame that changed.
    Damage(Vec<Rect>),
    /// A new cursor bitmap.
    Cursor(OwnedCursorImage),
    /// Where the cursor is now.
    Moved(CursorPosition),
}

/// Why a frame record could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoError {
    /// The channel cannot continue, but the session can.
    Rebind(String),
    /// The stream is one this build cannot display.
    Fatal(String),
}

/// The decode half of one frame channel.
pub struct Video {
    decoder: Option<Decoder>,
    /// The sequence of the last frame record applied, which is what a delta's
    /// base must name.
    last_frame: Option<u32>,
}

impl Video {
    /// A video pipeline with no stream yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            decoder: None,
            last_frame: None,
        }
    }

    /// The geometry the current stream is at.
    #[must_use]
    pub fn geometry(&self) -> Option<Geometry> {
        self.decoder.as_ref().map(Decoder::geometry)
    }

    /// The frame as it now stands, four bytes per pixel.
    #[must_use]
    pub fn frame(&self) -> Option<&[u8]> {
        self.decoder.as_ref().map(Decoder::frame)
    }

    /// Applies one record of the frame channel.
    ///
    /// # Errors
    ///
    /// [`VideoError::Rebind`] for anything that leaves the picture wrong but
    /// the session usable -- a missing base, a payload the codec refuses, a
    /// frame record before any `StreamConfig` -- and [`VideoError::Fatal`] for
    /// a geometry this build cannot decode at all.
    pub fn apply(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError> {
        match FrameRecord::try_from(i32::from(header.message_type)) {
            Ok(FrameRecord::StreamConfig) => self.configure(payload),
            Ok(FrameRecord::Keyframe) => self.keyframe(header, payload),
            Ok(FrameRecord::TileDelta) => self.delta(header, payload),
            Ok(FrameRecord::CursorImage) => Decoder::decode_cursor_image(payload)
                .map(Update::Cursor)
                .map_err(|error| Self::rebind("a cursor bitmap", error)),
            Ok(FrameRecord::CursorPosition) => Decoder::decode_cursor_position(payload)
                .map(Update::Moved)
                .map_err(|error| Self::rebind("a cursor position", error)),
            _ => {
                log::debug!(
                    "a frame record of type {} is one this build does not read",
                    header.message_type
                );
                Ok(Update::Nothing)
            }
        }
    }

    /// Builds a decoder for the stream a `StreamConfig` describes.
    ///
    /// A second config replaces both the decoder and the frame: geometry never
    /// changes inside an encoder, so a new geometry is a new stream.
    fn configure(&mut self, payload: &[u8]) -> Result<Update, VideoError> {
        let config = StreamConfig::decode(payload)
            .map_err(|error| VideoError::Rebind(format!("a stream config is unreadable: {error}")))?;

        let tile_size = TileSize::from_pixels(config.tile_size)
            .map_err(|error| VideoError::Fatal(format!("the stream's tile size: {error}")))?;
        let pixel_format = match WireFormat::try_from(config.pixel_format) {
            Ok(WireFormat::Bgra8888) => PixelFormat::Bgra8888,
            Ok(WireFormat::Xrgb8888) => PixelFormat::Xrgb8888,
            _ => {
                return Err(VideoError::Fatal(
                    "the stream names a pixel format this build cannot draw".to_owned(),
                ));
            }
        };

        let geometry = Geometry::new(config.width, config.height, tile_size, pixel_format)
            .map_err(|error| VideoError::Fatal(format!("the stream's geometry: {error}")))?;

        log::info!(
            "the display stream is {}x{}, {}-pixel tiles, {pixel_format:?}",
            geometry.width(),
            geometry.height(),
            geometry.tile_size().as_pixels()
        );
        self.decoder = Some(Decoder::new(geometry));
        self.last_frame = None;

        Ok(Update::Configured(geometry))
    }

    /// Applies a whole frame.
    fn keyframe(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError> {
        let decoder = self.decoder.as_mut().ok_or_else(Self::no_stream)?;
        let damage = decoder
            .apply_keyframe(payload)
            .map_err(|error| Self::rebind("a keyframe", error))?
            .to_vec();

        self.last_frame = Some(header.sequence);
        log::trace!(
            "keyframe {} restored {} tiles",
            header.sequence,
            damage.len()
        );

        Ok(Update::Damage(damage))
    }

    /// Applies the tiles a delta carries.
    fn delta(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError> {
        if self.last_frame != Some(header.base) {
            return Err(VideoError::Rebind(format!(
                "a delta builds on record {}, which this connection never applied",
                header.base
            )));
        }

        let decoder = self.decoder.as_mut().ok_or_else(Self::no_stream)?;
        let damage = decoder
            .apply_delta(payload)
            .map_err(|error| Self::rebind("a tile delta", error))?
            .to_vec();

        self.last_frame = Some(header.sequence);
        log::trace!("delta {} changed {} tiles", header.sequence, damage.len());

        Ok(Update::Damage(damage))
    }

    /// A frame record that arrived before the stream it belongs to.
    fn no_stream() -> VideoError {
        VideoError::Rebind("a frame record arrived before any stream config".to_owned())
    }

    /// A payload the codec refused. Sizes and reasons, never bytes.
    fn rebind(what: &str, error: CodecError) -> VideoError {
        VideoError::Rebind(format!("{what} could not be decoded: {error}"))
    }
}

impl Default for Video {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Declare the module**

In `crates/display-viewer/src/lib.rs`, add `pub mod video;` after `pub mod status;`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — eleven tests in `video::tests`.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/src/video.rs crates/display-viewer/src/lib.rs crates/display-viewer/Cargo.toml
git commit -m "TASK-117: Decode the frame channel into dirty rectangles"
```

---

### Task 5: The relay driver

**Files:**
- Create: `crates/display-viewer/src/duplex.rs`
- Create: `crates/display-viewer/src/relay.rs`
- Modify: `crates/display-viewer/src/lib.rs` (declare both modules)

**Interfaces:**
- Consumes: `launch::{Message, Handover}` (Task 1).
- Produces:
  - `vmlord_display_viewer::relay::{Relay, RelayError, RECORD_CEILING}`
  - `Relay::new(socket: &mut S, inbox: &Receiver<Message>, outbox: &Sender<Message>) -> Relay<'_, S>`
  - `Relay::run(&mut self, hello: &[u8], deadline: Instant) -> Result<Handover, RelayError>`
  - `RelayError` variants: `Timeout`, `NoParent`, `Cancelled`, `Socket(String)`, `TooLarge(u32)`
  - `#[cfg(test)] vmlord_display_viewer::duplex::pair() -> (Duplex, Duplex)`

The viewer never parses a handshake record. It frames one — twenty-four header bytes, the length the header announces, and whatever a newer minor appended — and passes the bytes on. Framing is what a stream needs; parsing is what VMLord does.

Messages reach the relay over an `mpsc` channel rather than off the pipe directly: the pipe reader is a thread of its own in the finished binary, and a channel is what a test can fill deterministically.

- [ ] **Step 1: Write the test transport**

Create `crates/display-viewer/src/duplex.rs`:

```rust
//! Two ends of a socket, in memory.
//!
//! Reads answer `WouldBlock` when there is nothing yet, which is what the
//! record reader turns into `RecordError::Idle` -- the same thing a bounded
//! HvSocket read reports when the peer is simply quiet. That is what lets a
//! test drive the same loop the window does.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{Arc, Mutex},
};

/// One end of an in-memory socket.
pub struct Duplex {
    incoming: Arc<Mutex<VecDeque<u8>>>,
    outgoing: Arc<Mutex<VecDeque<u8>>>,
    /// Whether the other end has been dropped.
    closed: Arc<Mutex<bool>>,
}

/// A connected pair.
#[must_use]
pub fn pair() -> (Duplex, Duplex) {
    let left = Arc::new(Mutex::new(VecDeque::new()));
    let right = Arc::new(Mutex::new(VecDeque::new()));
    let closed = Arc::new(Mutex::new(false));

    (
        Duplex {
            incoming: Arc::clone(&left),
            outgoing: Arc::clone(&right),
            closed: Arc::clone(&closed),
        },
        Duplex {
            incoming: right,
            outgoing: left,
            closed,
        },
    )
}

impl Duplex {
    /// Marks the pair closed, the way a peer that hung up does.
    pub fn close(&self) {
        *self.closed.lock().expect("no test panics holding it") = true;
    }
}

impl Read for Duplex {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut incoming = self.incoming.lock().expect("no test panics holding it");
        if incoming.is_empty() {
            if *self.closed.lock().expect("no test panics holding it") {
                return Ok(0);
            }
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }

        let mut read = 0;
        while read < buffer.len() {
            match incoming.pop_front() {
                Some(byte) => {
                    buffer[read] = byte;
                    read += 1;
                }
                None => break,
            }
        }

        Ok(read)
    }
}

impl Write for Duplex {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.outgoing
            .lock()
            .expect("no test panics holding it")
            .extend(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
```

- [ ] **Step 2: Write the failing test**

Create `crates/display-viewer/src/relay.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Channel, Limits},
        session::{Event, Session, Support},
        v1::{Capability, Mode},
    };

    use super::{Relay, RelayError};
    use crate::{duplex, launch::Message};

    fn support() -> Support {
        Support {
            capabilities: vec![Capability::CursorStream],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        }
    }

    /// Answers a handshake as a guest would, on one in-memory socket.
    ///
    /// Returns once the guest's session is established, so a test can assert
    /// on both ends of the same handshake.
    fn guest_thread(
        mut socket: duplex::Duplex,
        secret: Secret,
    ) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || {
            let mut session = Session::guest(&secret, support());
            let limits = Limits::new(0, 0);
            let deadline = Instant::now() + Duration::from_secs(5);

            while Instant::now() < deadline {
                let mut payload = Vec::new();
                let header = match record::read(&mut socket, &limits, &mut payload) {
                    Ok(header) => header,
                    Err(record::RecordError::Idle) => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => return false,
                };

                let Ok(outcome) = session.handle(&header, &payload) else {
                    return false;
                };
                if let Some(reply) = outcome.reply {
                    let _ = record::write(&mut socket, &reply, &limits);
                }
                if let Some(auth) = session.pending_auth() {
                    let _ = record::write(&mut socket, &auth, &limits);
                }
                if outcome.event == Event::ControlEstablished {
                    return true;
                }
            }

            false
        })
    }

    #[test]
    fn a_handshake_completes_through_the_relay() {
        let secret = Secret::generate();
        let guest_secret =
            Secret::from_base64(secret.to_base64().as_str()).expect("the same secret");
        let (mut host_socket, guest_socket) = duplex::pair();
        let guest = guest_thread(guest_socket, guest_secret);

        let (to_viewer, inbox) = mpsc::channel();
        let (outbox, from_viewer) = mpsc::channel();

        // What VMLord holds: the secret, the session, and the `ClientHello`
        // whose bytes the launch parameters carry to the viewer.
        let (mut session, hello) = Session::host(
            &secret,
            vmlord_display_protocol::session::Offer {
                capabilities: vec![Capability::CursorStream],
                mode: Mode::Auto,
                width: 1920,
                height: 1080,
                tile_size: 32,
            },
        );
        let mut hello_bytes = hello.header.encode().to_vec();
        hello_bytes.extend_from_slice(&hello.payload);

        let vmlord = std::thread::spawn(move || {
            let limits = Limits::new(0, 0);

            while let Ok(message) = from_viewer.recv_timeout(Duration::from_secs(5)) {
                let Message::RelayFromViewer(bytes) = message else {
                    continue;
                };
                let mut cursor = bytes.as_slice();
                let mut payload = Vec::new();
                let header = record::read(&mut cursor, &limits, &mut payload)
                    .expect("the viewer framed a whole record");
                let outcome = session.handle(&header, &payload).expect("a valid record");

                if let Some(reply) = outcome.reply {
                    let mut out = reply.header.encode().to_vec();
                    out.extend_from_slice(&reply.payload);
                    let _ = to_viewer.send(Message::RelayToViewer(out));
                }
                if outcome.event == Event::ControlEstablished {
                    let negotiated = session.negotiated().expect("established").clone();
                    let _ = to_viewer.send(Message::Handover(crate::launch::Handover {
                        session_id: session.session_id().to_vec(),
                        frame_key: session
                            .derive_channel_key(Channel::Frame)
                            .expect("established")
                            .to_bytes()
                            .to_vec(),
                        input_key: session
                            .derive_channel_key(Channel::Input)
                            .expect("established")
                            .to_bytes()
                            .to_vec(),
                        version_major: negotiated.version.major,
                        version_minor: negotiated.version.minor,
                        capabilities: negotiated
                            .capabilities
                            .iter()
                            .map(|capability| i32::from(*capability))
                            .collect(),
                        mode: i32::from(negotiated.mode),
                        width: negotiated.width,
                        height: negotiated.height,
                        tile_size: negotiated.tile_size,
                        control_sequence: session.control_sequence(),
                    }));
                    return true;
                }
            }

            false
        });

        let mut relay = Relay::new(&mut host_socket, &inbox, &outbox);
        let handover = relay
            .run(&hello_bytes, Instant::now() + Duration::from_secs(5))
            .expect("a handshake this guest can answer");

        assert_eq!(handover.session_id.len(), 16);
        assert_eq!(handover.frame_key.len(), 32);
        assert_eq!(handover.input_key.len(), 32);
        assert_eq!(handover.width, 1920);
        assert!(guest.join().expect("the guest thread"));
        assert!(vmlord.join().expect("the VMLord thread"));
    }

    #[test]
    fn a_silent_guest_times_out_rather_than_hanging() {
        let (mut socket, _guest) = duplex::pair();
        let (_to_viewer, inbox) = mpsc::channel();
        let (outbox, _from_viewer) = mpsc::channel();

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);
        let started = Instant::now();
        let outcome = relay.run(&[], started + Duration::from_millis(200));

        assert!(matches!(outcome, Err(RelayError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_parent_that_dies_mid_relay_aborts_the_attempt() {
        let (mut socket, _guest) = duplex::pair();
        let (to_viewer, inbox) = mpsc::channel::<Message>();
        let (outbox, _from_viewer) = mpsc::channel();
        drop(to_viewer);

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);

        assert!(matches!(
            relay.run(&[], Instant::now() + Duration::from_secs(5)),
            Err(RelayError::NoParent)
        ));
    }

    #[test]
    fn a_close_command_during_a_handshake_stops_it() {
        let (mut socket, _guest) = duplex::pair();
        let (to_viewer, inbox) = mpsc::channel();
        let (outbox, _from_viewer) = mpsc::channel();
        to_viewer
            .send(Message::Command(crate::launch::Command::Close))
            .expect("the channel is open");

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);

        assert!(matches!(
            relay.run(&[], Instant::now() + Duration::from_secs(5)),
            Err(RelayError::Cancelled)
        ));
    }

    #[test]
    fn a_record_larger_than_the_ceiling_is_refused_before_it_is_read() {
        let (mut socket, mut guest) = duplex::pair();
        let (_to_viewer, inbox) = mpsc::channel();
        let (outbox, _from_viewer) = mpsc::channel();

        // A well-formed header announcing a payload no control record may have.
        let mut header = [0u8; 24];
        header[0] = 24;
        header[1] = Channel::Control.as_wire();
        header[4..8].copy_from_slice(&(super::RECORD_CEILING + 1).to_le_bytes());
        std::io::Write::write_all(&mut guest, &header).expect("an in-memory socket");

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);

        assert!(matches!(
            relay.run(&[], Instant::now() + Duration::from_secs(5)),
            Err(RelayError::TooLarge(_))
        ));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `Relay` in this scope`.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/display-viewer/src/relay.rs`:

```rust
//! Carrying a handshake between the control socket and VMLord.
//!
//! The viewer holds the socket and VMLord holds the secret, so neither can run
//! the handshake alone. What happens here is the smallest thing that lets them:
//! bytes off the socket go up the pipe, bytes down the pipe go onto the socket,
//! and the viewer parses none of them. It frames records -- a stream has to be
//! cut somewhere -- and reads no further into them than the length.
//!
//! Every wait is bounded by the deadline the caller chose, which is the
//! `Authenticating` state's share of the retry budget.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    sync::mpsc::{Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use vmlord_display_protocol::record::{CONTROL_MAX_PAYLOAD, HEADER_LEN};

use crate::launch::{Command, Handover, Message};

/// The most a relayed record may carry.
///
/// The control channel's own cap. A handshake record is a few hundred bytes;
/// this is what a peer cannot make the viewer allocate past.
pub const RECORD_CEILING: u32 = CONTROL_MAX_PAYLOAD;

/// How long the loop sleeps when neither end had anything.
const IDLE_SLEEP: Duration = Duration::from_millis(5);

/// The handshake relay for one control socket.
pub struct Relay<'a, S: Read + Write> {
    socket: &'a mut S,
    inbox: &'a Receiver<Message>,
    outbox: &'a Sender<Message>,
    bytes: Vec<u8>,
}

impl<'a, S: Read + Write> Relay<'a, S> {
    /// A relay over one socket and one pair of launch-pipe channels.
    pub fn new(socket: &'a mut S, inbox: &'a Receiver<Message>, outbox: &'a Sender<Message>) -> Self {
        Self {
            socket,
            inbox,
            outbox,
            bytes: Vec::new(),
        }
    }

    /// Writes `hello` and shuttles bytes until a hand-over arrives.
    ///
    /// # Errors
    ///
    /// [`RelayError::Timeout`] if `deadline` passed first,
    /// [`RelayError::NoParent`] if the launch pipes closed,
    /// [`RelayError::Cancelled`] if VMLord asked the window to close,
    /// [`RelayError::TooLarge`] for a record above [`RECORD_CEILING`], and
    /// [`RelayError::Socket`] if the control socket failed.
    pub fn run(&mut self, hello: &[u8], deadline: Instant) -> Result<Handover, RelayError> {
        self.socket
            .write_all(hello)
            .and_then(|()| self.socket.flush())
            .map_err(|error| RelayError::Socket(error.to_string()))?;

        while Instant::now() < deadline {
            let mut idle = true;

            match self.read_record() {
                Ok(Some(bytes)) => {
                    idle = false;
                    self.outbox
                        .send(Message::RelayFromViewer(bytes))
                        .map_err(|_| RelayError::NoParent)?;
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }

            loop {
                match self.inbox.try_recv() {
                    Ok(Message::RelayToViewer(bytes)) => {
                        idle = false;
                        self.socket
                            .write_all(&bytes)
                            .and_then(|()| self.socket.flush())
                            .map_err(|error| RelayError::Socket(error.to_string()))?;
                    }
                    Ok(Message::Handover(handover)) => return Ok(handover),
                    Ok(Message::Command(Command::Close)) => return Err(RelayError::Cancelled),
                    // A focus during a handshake is the window's business, and
                    // the pipe thread has already acted on it.
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(RelayError::NoParent),
                }
            }

            if idle {
                thread::sleep(IDLE_SLEEP);
            }
        }

        Err(RelayError::Timeout)
    }

    /// Frames one record off the socket, or `None` if none has arrived.
    ///
    /// Nothing here reads past the header's length: what a record means is
    /// VMLord's business.
    fn read_record(&mut self) -> Result<Option<Vec<u8>>, RelayError> {
        let mut header = [0u8; HEADER_LEN];
        match self.fill(&mut header) {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(error) => return Err(error),
        }

        let header_len = usize::from(header[0]);
        if header_len < HEADER_LEN {
            return Err(RelayError::Socket(format!(
                "a record header of {header_len} bytes is shorter than this build reads"
            )));
        }
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if length > RECORD_CEILING {
            return Err(RelayError::TooLarge(length));
        }

        self.bytes.clear();
        self.bytes.extend_from_slice(&header);
        // Whatever a newer minor appended to the header, then the payload.
        self.bytes
            .resize(header_len + length as usize, 0);
        let rest = &mut self.bytes[HEADER_LEN..];
        self.socket
            .read_exact(rest)
            .map_err(|error| RelayError::Socket(error.to_string()))?;

        Ok(Some(self.bytes.clone()))
    }

    /// Fills `bytes`, answering `false` for a socket that is merely quiet.
    fn fill(&mut self, bytes: &mut [u8]) -> Result<bool, RelayError> {
        let mut filled = 0;
        while filled < bytes.len() {
            match self.socket.read(&mut bytes[filled..]) {
                Ok(0) if filled == 0 => {
                    return Err(RelayError::Socket(
                        "the guest closed the control connection".to_owned(),
                    ));
                }
                Ok(0) => {
                    return Err(RelayError::Socket(
                        "the control connection ended part-way through a record".to_owned(),
                    ));
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
                    return Ok(false);
                }
                Err(error) => return Err(RelayError::Socket(error.to_string())),
            }
        }

        Ok(true)
    }
}

/// Why a handshake did not complete.
#[derive(Debug)]
pub enum RelayError {
    /// The deadline passed. The state's budget answers for what happens next.
    Timeout,
    /// The launch pipes closed: VMLord is gone, and a session needs it.
    NoParent,
    /// VMLord asked the window to close while the handshake ran.
    Cancelled,
    /// A record above the control channel's cap.
    TooLarge(u32),
    /// The control socket failed, or the guest closed it.
    Socket(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("the handshake did not finish in time"),
            Self::NoParent => {
                formatter.write_str("VMLord is no longer there to run the handshake")
            }
            Self::Cancelled => formatter.write_str("the handshake was cancelled"),
            Self::TooLarge(length) => write!(
                formatter,
                "a {length}-byte handshake record exceeds the {RECORD_CEILING}-byte limit"
            ),
            Self::Socket(detail) => write!(formatter, "the control socket failed: {detail}"),
        }
    }
}

impl Error for RelayError {}
```

- [ ] **Step 5: Declare the modules**

In `crates/display-viewer/src/lib.rs`, add:

```rust
#[cfg(test)]
pub(crate) mod duplex;
pub mod relay;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — five tests in `relay::tests`.

- [ ] **Step 7: Commit**

```bash
git add crates/display-viewer/src/relay.rs crates/display-viewer/src/duplex.rs crates/display-viewer/src/lib.rs
git commit -m "TASK-117: Relay the display handshake between VMLord and the guest"
```

---

### Task 6: The established session loop

**Files:**
- Create: `crates/display-viewer/src/live.rs`
- Modify: `crates/display-viewer/src/lib.rs` (declare the module)
- Modify: `crates/display-protocol/src/session.rs` (two sequence accessors — Step 4)

**Interfaces:**
- Consumes: `launch::Handover` (Task 1), `Session::established_host` and `HandedOver` (Task 2), `video::{Video, Update, VideoError}` (Task 4), `status::Event` (Task 3).
- Produces:
  - `vmlord_display_viewer::live::{Live, Signal, PING_INTERVAL, PONG_TIMEOUT, BIND_BACKOFF}`
  - `Live::new(handover: Handover, control: S, connect: C, now: Instant) -> Result<Live<S, C>, String>` where `S: Read + Write`, `C: FnMut(Channel) -> Result<S, String>`
  - `Live::pump(&mut self, now: Instant, signals: &mut Vec<Signal>)`
  - `Live::request_keyframe(&mut self)`, `Live::end(&mut self)`, `Live::video(&self) -> &Video`
  - `Signal` variants: `Configured(Geometry)`, `Damage(Vec<Rect>)`, `Cursor(OwnedCursorImage)`, `Moved(CursorPosition)`, `Status(status::Event)`, `Ended(String)`
  - `Session::take_control_sequence(&mut self) -> u32` and `Session::take_channel_sequence(&mut self, Channel) -> Result<u32, SessionError>` in `vmlord-display-protocol`

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/live.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::{
        io::Write as _,
        time::{Duration, Instant},
    };

    use prost::Message as _;
    use vmlord_display_codec::{Encoder, EncoderConfig, Frame, Geometry, PixelFormat, TileSize};
    use vmlord_display_protocol::{
        keys::{self, ChannelKey, Role, Tag},
        record::{self, Channel, Limits, Record},
        v1::{
            ChannelAck, ChannelAuth, ChannelHello, ControlRecord, FrameRecord, InputRecord,
            PixelFormat as WireFormat, Ping, Pong, StreamConfig,
        },
    };

    use super::{Live, PONG_TIMEOUT, Signal};
    use crate::{
        duplex::{self, Duplex},
        launch::Handover,
    };

    const SESSION_ID: [u8; 16] = [7; 16];
    const FRAME_KEY: [u8; 32] = [1; 32];
    const INPUT_KEY: [u8; 32] = [2; 32];

    fn handover() -> Handover {
        Handover {
            session_id: SESSION_ID.to_vec(),
            frame_key: FRAME_KEY.to_vec(),
            input_key: INPUT_KEY.to_vec(),
            version_major: 1,
            version_minor: 0,
            capabilities: vec![1],
            mode: 2,
            width: 320,
            height: 200,
            tile_size: 32,
            control_sequence: 2,
        }
    }

    fn geometry() -> Geometry {
        Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows")
    }

    /// The guest half of a bind, done with a channel key and nothing else --
    /// which is all the guest's capture process has.
    fn accept_bind(socket: &mut Duplex, channel: Channel, key: &ChannelKey) -> u32 {
        let limits = Limits::new(0, 0);
        let mut payload = Vec::new();
        let header = wait_for_record(socket, &limits, &mut payload);
        assert_eq!(header.message_type, FrameRecord::ChannelHello as u16);

        let hello = ChannelHello::decode(payload.as_slice()).expect("a channel hello");
        assert_eq!(hello.session_id, SESSION_ID);
        let host_nonce: [u8; 32] = hello.nonce.as_slice().try_into().expect("a 32-byte nonce");
        let guest_nonce = [9u8; 32];

        let ack = ChannelAck {
            nonce: guest_nonce.to_vec(),
            tag: keys::channel_tag(key, Role::Guest, channel, &host_nonce, &guest_nonce)
                .as_bytes()
                .to_vec(),
        };
        record::write(
            socket,
            &Record::new(
                channel,
                FrameRecord::ChannelAck as u16,
                1,
                0,
                hello.generation,
                ack.encode_to_vec(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        let header = wait_for_record(socket, &limits, &mut payload);
        assert_eq!(header.message_type, FrameRecord::ChannelAuth as u16);
        let auth = ChannelAuth::decode(payload.as_slice()).expect("a channel auth");
        let expected =
            keys::channel_tag(key, Role::Host, channel, &host_nonce, &guest_nonce);
        assert!(keys::verify(
            &expected,
            &Tag::from_wire(&auth.tag).expect("a 32-byte tag")
        ));

        hello.generation
    }

    /// Reads one record, spinning while the socket is merely quiet.
    fn wait_for_record(
        socket: &mut Duplex,
        limits: &Limits,
        payload: &mut Vec<u8>,
    ) -> record::Header {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match record::read(socket, limits, payload) {
                Ok(header) => return header,
                Err(record::RecordError::Idle) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("the socket failed: {error}"),
            }
        }
    }

    fn stream_config_record(sequence: u32, generation: u32) -> Record {
        let config = StreamConfig {
            width: 320,
            height: 200,
            tile_size: 32,
            pixel_format: WireFormat::Bgra8888 as i32,
        };

        Record::new(
            Channel::Frame,
            FrameRecord::StreamConfig as u16,
            sequence,
            0,
            generation,
            config.encode_to_vec(),
        )
    }

    fn keyframe_record(sequence: u32, generation: u32) -> Record {
        let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
        let pixels = vec![0x40; geometry().frame_bytes()];
        encoder
            .submit(
                Frame {
                    pixels: &pixels,
                    stride: geometry().width() as usize * 4,
                },
                None,
            )
            .expect("a frame of this geometry");
        let payload = match encoder.next_payload().expect("the first payload") {
            vmlord_display_codec::Payload::Keyframe(bytes) => bytes.to_vec(),
            other => panic!("the first payload is a keyframe, not {other:?}"),
        };

        Record::new(
            Channel::Frame,
            FrameRecord::Keyframe as u16,
            sequence,
            0,
            generation,
            payload,
        )
    }

    /// A live session with all three sockets connected and bound.
    struct Harness {
        control: Duplex,
        frame: Duplex,
        input: Duplex,
    }

    fn start(now: Instant) -> (Live<Duplex, impl FnMut(Channel) -> Result<Duplex, String>>, Harness) {
        let (host_control, control) = duplex::pair();
        let (host_frame, frame) = duplex::pair();
        let (host_input, input) = duplex::pair();

        let mut sockets = vec![(Channel::Input, host_input), (Channel::Frame, host_frame)];
        let connect = move |channel: Channel| {
            let index = sockets
                .iter()
                .position(|(kind, _)| *kind == channel)
                .ok_or_else(|| format!("no more {channel} sockets"))?;
            Ok(sockets.remove(index).1)
        };

        let live = Live::new(handover(), host_control, connect, now).expect("a hand-over");

        (live, Harness { control, frame, input })
    }

    #[test]
    fn both_channels_bind_at_generation_zero() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            let frame = accept_bind(&mut harness.frame, Channel::Frame, &ChannelKey::from_bytes(FRAME_KEY));
            let input = accept_bind(&mut harness.input, Channel::Input, &ChannelKey::from_bytes(INPUT_KEY));

            // What a freshly bound input channel owes: the guest has just
            // released everything it held, and the first record says so.
            let limits = Limits::new(0, 0);
            let mut payload = Vec::new();
            let header = wait_for_record(&mut harness.input, &limits, &mut payload);
            assert_eq!(header.message_type, InputRecord::ReleaseAll as u16);

            (frame, input, harness)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::Established)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }

        let (frame_generation, input_generation, _) = guest.join().expect("the guest thread");
        assert_eq!((frame_generation, input_generation), (0, 0));
    }

    #[test]
    fn a_stream_config_and_a_keyframe_put_pixels_on_the_screen() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            accept_bind(&mut harness.frame, Channel::Frame, &ChannelKey::from_bytes(FRAME_KEY));
            accept_bind(&mut harness.input, Channel::Input, &ChannelKey::from_bytes(INPUT_KEY));
            let limits = Limits::new(320, 200);
            record::write(&mut harness.frame, &stream_config_record(3, 0), &limits)
                .expect("an in-memory socket");
            record::write(&mut harness.frame, &keyframe_record(4, 0), &limits)
                .expect("an in-memory socket");
            harness
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals.iter().any(|signal| matches!(signal, Signal::Damage(_)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }
        guest.join().expect("the guest thread");

        assert!(signals
            .iter()
            .any(|signal| matches!(signal, Signal::Configured(_))));
        assert!(signals.iter().any(|signal| matches!(signal, Signal::Damage(_))));
        assert_eq!(live.video().geometry(), Some(geometry()));
    }

    #[test]
    fn a_corrupted_frame_record_rebinds_the_channel_at_the_next_generation() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            accept_bind(&mut harness.frame, Channel::Frame, &ChannelKey::from_bytes(FRAME_KEY));
            accept_bind(&mut harness.input, Channel::Input, &ChannelKey::from_bytes(INPUT_KEY));
            let limits = Limits::new(320, 200);
            record::write(&mut harness.frame, &stream_config_record(3, 0), &limits)
                .expect("an in-memory socket");
            // A keyframe whose payload was cut: the codec refuses it, and the
            // channel cannot continue.
            let mut broken = keyframe_record(4, 0);
            broken.payload.truncate(4);
            let broken = Record::new(
                Channel::Frame,
                FrameRecord::Keyframe as u16,
                4,
                0,
                0,
                broken.payload,
            );
            record::write(&mut harness.frame, &broken, &limits).expect("an in-memory socket");
            harness
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ChannelLost)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }
        guest.join().expect("the guest thread");

        assert!(signals
            .iter()
            .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ChannelLost))));
    }

    #[test]
    fn a_ping_is_answered_and_a_missing_pong_expires_control() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();
        let limits = Limits::new(0, 0);

        // The first pump writes the first ping.
        live.pump(now, &mut signals);
        let mut payload = Vec::new();
        let header = wait_for_record(&mut harness.control, &limits, &mut payload);
        assert_eq!(header.message_type, ControlRecord::Ping as u16);
        let token = Ping::decode(payload.as_slice()).expect("a ping").token;
        assert_eq!(header.sequence, 2, "the hand-over's control sequence");

        record::write(
            &mut harness.control,
            &Record::new(
                Channel::Control,
                ControlRecord::Pong as u16,
                0,
                0,
                0,
                Pong { token }.encode_to_vec(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        live.pump(Instant::now(), &mut signals);
        assert!(!signals
            .iter()
            .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ControlLost))));

        // The next ping goes unanswered.
        live.pump(now + super::PING_INTERVAL, &mut signals);
        live.pump(now + super::PING_INTERVAL + PONG_TIMEOUT, &mut signals);

        assert!(signals
            .iter()
            .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ControlLost))));
    }

    #[test]
    fn a_guest_that_ends_the_session_is_reported_rather_than_retried() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();
        let limits = Limits::new(0, 0);

        record::write(
            &mut harness.control,
            &Record::new(
                Channel::Control,
                ControlRecord::EndSession as u16,
                0,
                0,
                0,
                Vec::new(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        live.pump(Instant::now(), &mut signals);

        assert!(signals.iter().any(|signal| matches!(signal, Signal::Ended(_))));
    }

    #[test]
    fn an_error_record_is_logged_and_the_session_carries_on() {
        crate::log::capture::install();
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();
        let limits = Limits::new(0, 0);

        record::write(
            &mut harness.control,
            &Record::new(
                Channel::Control,
                ControlRecord::Error as u16,
                0,
                0,
                0,
                vmlord_display_protocol::v1::Error {
                    code: vmlord_display_protocol::v1::ErrorCode::CaptureFailed as i32,
                    detail: "the compositor stopped".to_owned(),
                }
                .encode_to_vec(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        live.pump(Instant::now(), &mut signals);

        assert!(!signals.iter().any(|signal| matches!(signal, Signal::Ended(_))));
        assert!(crate::log::capture::text().contains("the compositor stopped"));
    }

    #[test]
    fn ending_a_session_tells_the_guest_before_the_sockets_close() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let limits = Limits::new(0, 0);

        live.end();

        let mut payload = Vec::new();
        let mut header = wait_for_record(&mut harness.control, &limits, &mut payload);
        while header.message_type != ControlRecord::EndSession as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        assert_eq!(header.channel, Channel::Control);
    }

    #[test]
    fn a_keyframe_request_reaches_the_guest_on_the_control_channel() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let limits = Limits::new(0, 0);

        live.request_keyframe();

        let mut payload = Vec::new();
        let mut header = wait_for_record(&mut harness.control, &limits, &mut payload);
        while header.message_type != ControlRecord::RequestKeyframe as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        assert_eq!(header.channel, Channel::Control);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `Live` in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/display-viewer/src/live.rs`:

```rust
//! One established session, over three sockets.
//!
//! The session machine is the protocol crate's, built from the hand-over rather
//! than from a handshake this process ran: generations, sequences and the
//! three-record binds are its arithmetic, and what is here is the order things
//! happen in and what to do when one of them fails.
//!
//! Nothing blocks. [`Live::pump`] does whatever can be done without waiting and
//! returns; the window calls it between messages, and a test calls it in a
//! loop. Every read is one a quiet socket answers `Idle` to.

use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};

use prost::Message as _;
use vmlord_display_codec::{CursorPosition, Geometry, OwnedCursorImage, Rect};
use vmlord_display_protocol::{
    keys::ChannelKey,
    record::{self, Channel, Limits, Record, RecordError},
    session::{Event as SessionEvent, HandedOver, Negotiated, Session, SessionError},
    v1::{Capability, ControlRecord, DisplayState, Error as ErrorRecord, InputRecord, Mode, Ping,
         Pong, ProtocolVersion},
};

use crate::{
    launch::Handover,
    status::Event,
    video::{Update, Video, VideoError},
};

/// How often the viewer proves the control socket is still there.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How overdue a pong may be before control is treated as dead.
pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a failed bind waits before it is tried again.
pub const BIND_BACKOFF: Duration = Duration::from_secs(1);

/// What one pump produced.
#[derive(Debug)]
pub enum Signal {
    /// The stream's geometry. The window sizes its texture to it.
    Configured(Geometry),
    /// The rectangles of the frame that changed.
    Damage(Vec<Rect>),
    /// A new cursor bitmap.
    Cursor(OwnedCursorImage),
    /// Where the cursor is now.
    Moved(CursorPosition),
    /// Something the status machine has to know.
    Status(Event),
    /// The session is over, for the reason given. Fit for a log.
    Ended(String),
}

/// One session, driven by the window's message loop.
pub struct Live<S: Read + Write, C: FnMut(Channel) -> Result<S, String>> {
    session: Session,
    control: S,
    frame: Option<S>,
    input: Option<S>,
    connect: C,
    video: Video,
    /// The frame channel's caps, which the negotiated geometry sizes.
    limits: Limits,
    /// The control channel's caps, which nothing sizes.
    control_limits: Limits,
    /// Whether the window has been told the session is running.
    announced: bool,
    next_ping: Instant,
    /// The token of a ping still waiting for its pong, and when it went out.
    outstanding: Option<(u64, Instant)>,
    ping_token: u64,
    /// When a channel that failed to bind may be tried again.
    next_bind: Instant,
    payload: Vec<u8>,
}

impl<S: Read + Write, C: FnMut(Channel) -> Result<S, String>> Live<S, C> {
    /// Takes a session over from VMLord.
    ///
    /// `connect` opens one socket for a channel; the viewer calls it for the
    /// first bind and for every rebind, so that reconnecting a channel is the
    /// same code path as opening it.
    ///
    /// # Errors
    ///
    /// A message naming the field the hand-over got wrong: a session id or a
    /// key of the wrong width, or a version, mode or capability this build has
    /// no name for.
    pub fn new(handover: Handover, control: S, connect: C, now: Instant) -> Result<Self, String> {
        let session_id = handover
            .session_id
            .as_slice()
            .try_into()
            .map_err(|_| "the hand-over's session id is not sixteen bytes".to_owned())?;
        let frame_key = channel_key(&handover.frame_key, "frame")?;
        let input_key = channel_key(&handover.input_key, "input")?;

        let negotiated = Negotiated {
            version: ProtocolVersion {
                major: handover.version_major,
                minor: handover.version_minor,
            },
            capabilities: handover
                .capabilities
                .iter()
                .filter_map(|value| Capability::try_from(*value).ok())
                .collect(),
            mode: Mode::try_from(handover.mode)
                .map_err(|_| "the hand-over names a mode this build has no name for".to_owned())?,
            width: handover.width,
            height: handover.height,
            tile_size: handover.tile_size,
        };
        let limits = Limits::new(negotiated.width, negotiated.height);

        log::info!(
            "the display session is {}x{} at {}-pixel tiles, mode {:?}",
            negotiated.width,
            negotiated.height,
            negotiated.tile_size,
            negotiated.mode
        );

        Ok(Self {
            session: Session::established_host(HandedOver {
                session_id,
                negotiated,
                frame_key,
                input_key,
                control_sequence: handover.control_sequence,
            }),
            control,
            frame: None,
            input: None,
            connect,
            video: Video::new(),
            limits,
            control_limits: Limits::new(0, 0),
            announced: false,
            next_ping: now,
            outstanding: None,
            ping_token: 0,
            next_bind: now,
            payload: Vec::new(),
        })
    }

    /// What has been decoded so far.
    #[must_use]
    pub fn video(&self) -> &Video {
        &self.video
    }

    /// Does whatever can be done without waiting.
    pub fn pump(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        self.bind_channels(now, signals);
        self.read_control(now, signals);
        self.beat(now, signals);
        self.read_frames(now, signals);
    }

    /// Asks the guest for a whole frame.
    ///
    /// Recovery, not flow control: the decoder has nothing to apply a delta to.
    pub fn request_keyframe(&mut self) {
        self.write_control(ControlRecord::RequestKeyframe, Vec::new());
    }

    /// Tells the guest the session is over, best effort.
    ///
    /// The guest may stop capturing without waiting for the sockets to drop,
    /// and a failed write means it will find out the other way.
    pub fn end(&mut self) {
        self.write_control(ControlRecord::EndSession, Vec::new());
    }

    /// Opens and binds whichever of the two channels is not bound.
    fn bind_channels(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        if now < self.next_bind {
            return;
        }

        for channel in [Channel::Frame, Channel::Input] {
            if self.socket(channel).is_some() {
                continue;
            }

            match self.bind(channel) {
                Ok(()) => {
                    log::info!(
                        "the {channel} channel bound at generation {}",
                        self.session.generation(channel)
                    );
                    if channel == Channel::Frame && !self.announced {
                        self.announced = true;
                        signals.push(Signal::Status(Event::Established));
                    }
                }
                Err(reason) => {
                    log::debug!("the {channel} channel could not bind: {reason}");
                    self.next_bind = now + BIND_BACKOFF;
                    return;
                }
            }
        }
    }

    /// Opens one socket and runs the three-record bind on it.
    fn bind(&mut self, channel: Channel) -> Result<(), String> {
        let mut socket = (self.connect)(channel)?;

        let hello = self
            .session
            .open_channel(channel)
            .map_err(|error: SessionError| error.to_string())?;
        record::write(&mut socket, &hello, &self.control_limits)
            .map_err(|error| error.to_string())?;

        // The ack, then this side's proof. Both are small and both are owed
        // straight away, so a socket that is quiet here is one that is not
        // going to bind.
        let mut payload = Vec::new();
        let header = record::read(&mut socket, &self.control_limits, &mut payload)
            .map_err(|error| error.to_string())?;
        let outcome = self
            .session
            .handle(&header, &payload)
            .map_err(|error| error.to_string())?;
        if let Some(reply) = outcome.reply {
            record::write(&mut socket, &reply, &self.control_limits)
                .map_err(|error| error.to_string())?;
        }
        if outcome.event != SessionEvent::ChannelBound(channel) {
            return Err(format!("the {channel} channel did not bind"));
        }

        if channel == Channel::Input {
            // What a freshly bound input channel owes, per the protocol's
            // recovery rule: the guest has just released everything it held, so
            // the first record says so. Harmless on a first bind, and the one
            // thing that keeps a key held across a reconnect from staying down.
            let sequence = self
                .session
                .take_channel_sequence(channel)
                .map_err(|error| error.to_string())?;
            let release = Record::new(
                Channel::Input,
                InputRecord::ReleaseAll as u16,
                sequence,
                0,
                self.session.generation(channel),
                Vec::new(),
            );
            record::write(&mut socket, &release, &self.control_limits)
                .map_err(|error| error.to_string())?;
        }

        match channel {
            Channel::Frame => self.frame = Some(socket),
            Channel::Input => self.input = Some(socket),
            Channel::Control => unreachable!("control is not bound"),
        }

        Ok(())
    }

    /// Reads whatever the control channel has to say.
    fn read_control(&mut self, _now: Instant, signals: &mut Vec<Signal>) {
        loop {
            let mut payload = std::mem::take(&mut self.payload);
            let header = match record::read(&mut self.control, &self.control_limits, &mut payload) {
                Ok(header) => header,
                Err(RecordError::Idle) => {
                    self.payload = payload;
                    return;
                }
                Err(error) => {
                    self.payload = payload;
                    signals.push(Signal::Status(Event::ControlLost));
                    signals.push(Signal::Ended(format!("control was lost: {error}")));
                    return;
                }
            };

            match ControlRecord::try_from(i32::from(header.message_type)) {
                Ok(ControlRecord::Pong) => {
                    let token = Pong::decode(payload.as_slice()).map(|pong| pong.token).ok();
                    if self.outstanding.map(|(sent, _)| sent) == token {
                        self.outstanding = None;
                    }
                }
                Ok(ControlRecord::Ping) => {
                    let token = Ping::decode(payload.as_slice())
                        .map(|ping| ping.token)
                        .unwrap_or_default();
                    self.write_control(ControlRecord::Pong, Pong { token }.encode_to_vec());
                }
                Ok(ControlRecord::DisplayState) => {
                    if let Ok(state) = DisplayState::decode(payload.as_slice()) {
                        log::info!(
                            "the guest reports {}x{} at {}-pixel tiles",
                            state.width,
                            state.height,
                            state.tile_size
                        );
                    }
                }
                Ok(ControlRecord::Error) => {
                    if let Ok(error) = ErrorRecord::decode(payload.as_slice()) {
                        log::warn!(
                            "the guest reported display error {}: {}",
                            error.code,
                            error.detail
                        );
                    }
                }
                Ok(ControlRecord::EndSession) => {
                    signals.push(Signal::Ended("the guest ended the session".to_owned()));
                    self.payload = payload;
                    return;
                }
                _ => log::debug!(
                    "a control record of type {} is one this build does not read",
                    header.message_type
                ),
            }

            self.payload = payload;
        }
    }

    /// Sends a ping when one is due, and gives up when one goes unanswered.
    fn beat(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        if let Some((token, sent)) = self.outstanding
            && now.duration_since(sent) >= PONG_TIMEOUT
        {
            log::warn!("ping {token} went unanswered for {}s", PONG_TIMEOUT.as_secs());
            signals.push(Signal::Status(Event::ControlLost));
            signals.push(Signal::Ended("the guest stopped answering pings".to_owned()));
            self.outstanding = None;
            return;
        }

        if now < self.next_ping {
            return;
        }

        self.ping_token = self.ping_token.wrapping_add(1);
        let token = self.ping_token;
        self.write_control(ControlRecord::Ping, Ping { token }.encode_to_vec());
        self.outstanding.get_or_insert((token, now));
        self.next_ping = now + PING_INTERVAL;
    }

    /// Reads whatever the frame channel has, and decodes it.
    fn read_frames(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        loop {
            let Some(socket) = self.frame.as_mut() else {
                return;
            };

            let mut payload = std::mem::take(&mut self.payload);
            let header = match record::read(socket, &self.limits, &mut payload) {
                Ok(header) => header,
                Err(RecordError::Idle) => {
                    self.payload = payload;
                    return;
                }
                Err(error) => {
                    self.payload = payload;
                    self.rebind(now, signals, &error.to_string());
                    return;
                }
            };

            if let Err(error) = self.session.accept(&header) {
                self.payload = payload;
                self.rebind(now, signals, &error.to_string());
                return;
            }

            match self.video.apply(&header, &payload) {
                Ok(Update::Nothing) => {}
                Ok(Update::Configured(geometry)) => {
                    self.limits.set_geometry(geometry.width(), geometry.height());
                    signals.push(Signal::Configured(geometry));
                }
                Ok(Update::Damage(damage)) => signals.push(Signal::Damage(damage)),
                Ok(Update::Cursor(image)) => signals.push(Signal::Cursor(image)),
                Ok(Update::Moved(position)) => signals.push(Signal::Moved(position)),
                Err(VideoError::Rebind(reason)) => {
                    self.payload = payload;
                    self.rebind(now, signals, &reason);
                    return;
                }
                Err(VideoError::Fatal(reason)) => {
                    self.payload = payload;
                    signals.push(Signal::Ended(reason));
                    return;
                }
            }

            self.payload = payload;
        }
    }

    /// Drops the frame socket and asks for a replacement at the next generation.
    ///
    /// What the reconnected channel owes -- a `StreamConfig` and a keyframe --
    /// is the guest's obligation, which is why nothing is requested here.
    fn rebind(&mut self, now: Instant, signals: &mut Vec<Signal>, reason: &str) {
        log::warn!("the frame channel is being replaced: {reason}");
        self.frame = None;
        self.video = Video::new();
        self.next_bind = now;

        if let Err(error) = self.session.reconnect_channel(Channel::Frame) {
            signals.push(Signal::Ended(format!(
                "the frame channel cannot be replaced: {error}"
            )));
            return;
        }

        signals.push(Signal::Status(Event::ChannelLost));
    }

    /// Which socket a channel is on.
    fn socket(&self, channel: Channel) -> Option<&S> {
        match channel {
            Channel::Frame => self.frame.as_ref(),
            Channel::Input => self.input.as_ref(),
            Channel::Control => Some(&self.control),
        }
    }

    /// One control record of this side's own.
    ///
    /// A failed write is logged and nothing else: a control socket that cannot
    /// be written to is one the next read reports as lost.
    fn write_control(&mut self, message_type: ControlRecord, payload: Vec<u8>) {
        let sequence = self.session.control_sequence();
        let record = Record::new(Channel::Control, message_type as u16, sequence, 0, 0, payload);
        // Keeping the machine's counter in step: it is the one thing about a
        // control record this side chooses.
        let _ = self.session.take_control_sequence();

        if let Err(error) = record::write(&mut self.control, &record, &self.control_limits) {
            log::debug!("a {message_type:?} record could not be written: {error}");
        }
    }
}

/// Reads a channel key out of a hand-over.
fn channel_key(bytes: &[u8], what: &str) -> Result<ChannelKey, String> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("the hand-over's {what} key is not thirty-two bytes"))?;

    Ok(ChannelKey::from_bytes(bytes))
}
```

- [ ] **Step 4: Give the session machine the counter this loop needs**

`write_control` above needs the control sequence to advance. Add to `impl Session` in `crates/display-protocol/src/session.rs`, beside `control_sequence`:

```rust
    /// Takes the next control sequence, advancing the counter.
    ///
    /// The session machine numbers the records it produces itself; this is for
    /// the records a caller produces on an established session -- pings, pongs,
    /// keyframe requests and the end of a session -- so that one counter serves
    /// the whole channel.
    pub fn take_control_sequence(&mut self) -> u32 {
        let sequence = self.control_sequence;
        self.control_sequence += 1;

        sequence
    }
```

And the same for a bound channel, which the input channel's `ReleaseAll` needs:

```rust
    /// Takes the next sequence on a bound channel, advancing the counter.
    ///
    /// The counterpart of [`Session::take_control_sequence`] for the records a
    /// caller writes on a frame or input socket, so that the binding records
    /// and the traffic after them share one counter.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unexpected`] for [`Channel::Control`], which has its own.
    pub fn take_channel_sequence(&mut self, channel: Channel) -> Result<u32, SessionError> {
        let index = self.channel_index(channel)?;
        let sequence = self.channels[index].sequence;
        self.channels[index].sequence += 1;

        Ok(sequence)
    }
```

Then simplify `Live::write_control` to use it:

```rust
    fn write_control(&mut self, message_type: ControlRecord, payload: Vec<u8>) {
        let sequence = self.session.take_control_sequence();
        let record = Record::new(Channel::Control, message_type as u16, sequence, 0, 0, payload);

        if let Err(error) = record::write(&mut self.control, &record, &self.control_limits) {
            log::debug!("a {message_type:?} record could not be written: {error}");
        }
    }
```

- [ ] **Step 5: Declare the module**

In `crates/display-viewer/src/lib.rs`, add `pub mod live;` after `pub mod launch;`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer` and `cargo test -p vmlord-display-protocol`
Expected: PASS — eight tests in `live::tests`, and the protocol crate's suite unchanged.

- [ ] **Step 7: Commit**

```bash
git add crates/display-viewer/src/live.rs crates/display-viewer/src/lib.rs crates/display-protocol/src/session.rs
git commit -m "TASK-117: Run an established display session over three sockets"
```

---

### Task 7: The HvSocket connector

**Files:**
- Create: `crates/display-viewer/src/windows/mod.rs`
- Create: `crates/display-viewer/src/windows/hvsocket.rs`
- Modify: `crates/display-viewer/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `vmlord_display_viewer::windows::hvsocket::{HvSocket, ConnectError, CONTROL_PORT, FRAME_PORT, INPUT_PORT, vsock_service_id, READ_POLL, CONNECT_TIMEOUT}`
  - `HvSocket::connect(runtime_id: &[u8; 16], port: u32, timeout: Duration) -> Result<HvSocket, ConnectError>`
  - `impl Read for HvSocket`, `impl Write for HvSocket` — a quiet socket answers `io::ErrorKind::WouldBlock`, which `record::read` reports as `RecordError::Idle`
  - `ConnectError` variants: `PartitionGone`, `Refused(String)`, `Failed(String)`

The runtime id crosses the launch pipe as the sixteen bytes of `Uuid::as_bytes`, and is rebuilt here with `GUID::from_u128(u128::from_be_bytes(*runtime_id))` — the same value `vmlord-platform` builds with `GUID::from_u128(runtime_id.as_u128())`.

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/windows/hvsocket.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CONTROL_PORT, ConnectError, FRAME_PORT, HvSocket, INPUT_PORT, vsock_service_id};

    #[test]
    fn the_three_ports_are_the_ones_the_guest_listens_on() {
        assert_eq!(CONTROL_PORT, 0x564D_4C44);
        assert_eq!(FRAME_PORT, 0x564D_4C46);
        assert_eq!(INPUT_PORT, 0x564D_4C49);
    }

    #[test]
    fn a_service_guid_is_the_template_hyper_v_maps_a_vsock_port_through() {
        // The same template `vmlord-platform` derives the agent's service from:
        // the port becomes the first field, and the rest is the constant Linux
        // integration uses.
        assert_eq!(
            format!("{:?}", vsock_service_id(CONTROL_PORT)),
            "564D4C44-FACB-11E6-BD58-64006A7986D3"
        );
        assert_ne!(vsock_service_id(FRAME_PORT), vsock_service_id(INPUT_PORT));
    }

    #[test]
    fn a_connect_to_no_partition_fails_inside_its_timeout() {
        let started = Instant::now();
        let outcome = HvSocket::connect(&[0; 16], CONTROL_PORT, Duration::from_millis(500));

        assert!(matches!(
            outcome,
            Err(ConnectError::PartitionGone | ConnectError::Refused(_) | ConnectError::Failed(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a connect that cannot succeed must not hang"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `file not found for module `windows``.

- [ ] **Step 3: Write the module door**

Create `crates/display-viewer/src/windows/mod.rs`:

```rust
//! The four modules that touch Win32, and the only `unsafe` in this crate.
//!
//! The workspace denies `unsafe_code`; each declaration below re-allows it for
//! one module and says what crosses that door. Everything else in the crate is
//! safe Rust, and every decision the viewer makes lives on that side.

/// Winsock, and the `AF_HYPERV` addresses the metadata does not describe.
#[allow(unsafe_code)]
pub mod hvsocket;

/// A named mutex and a named pipe: one window per VM, focused rather than
/// duplicated.
#[allow(unsafe_code)]
pub mod ipc;

/// A window class, a message pump, and the messages the session posts into it.
#[allow(unsafe_code)]
pub mod window;

/// A D3D11 device, a swapchain, one texture, and the Direct2D overlay over it.
#[allow(unsafe_code)]
pub mod d3d;
```

**Note for the implementer:** `ipc`, `window` and `d3d` are Tasks 8, 9 and 10. Until each exists, comment its declaration out rather than leaving the crate unbuildable, and uncomment it in the task that creates it.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/display-viewer/src/windows/hvsocket.rs`:

```rust
//! The host end of the three sockets a guest's display services listen on.
//!
//! The mirror of `vmlord-platform`'s agent socket: there the guest connects and
//! the host listens, here the host connects and the guest listens, which is
//! #118's decision -- a display session is opened by the person who pressed
//! Connect, and the guest's services are already running when they do.
//!
//! An address is a pair of GUIDs: which partition, and which service. The
//! service half is derived from a vsock port, because the guest is Linux and
//! spells an HvSocket address as `AF_VSOCK` with a port number.
//!
//! Every wait is bounded. A connect that cannot succeed says so inside its
//! timeout, and a read on a quiet socket answers `WouldBlock` rather than
//! parking the thread that owns the session.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    mem,
    time::Duration,
};

use windows::{
    Win32::Networking::WinSock::{
        AF_HYPERV, FD_SET, FIONBIO, SEND_RECV_FLAGS, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR,
        TIMEVAL, WSADATA, WSAEBADF, WSAECONNREFUSED, WSAENETDOWN, WSAENETUNREACH, WSAEWOULDBLOCK,
        WSAGetLastError, WSAStartup, closesocket, connect, ioctlsocket, recv, select, send, socket,
    },
    core::GUID,
};

/// Where the host opens a session and keeps it alive. `"VMLD"`.
pub const CONTROL_PORT: u32 = 0x564D_4C44;

/// Where the frames arrive. `"VMLF"`.
pub const FRAME_PORT: u32 = 0x564D_4C46;

/// Where keys and pointer events go back. `"VMLI"`.
pub const INPUT_PORT: u32 = 0x564D_4C49;

/// The protocol number an HvSocket stream is opened with.
///
/// `HV_PROTOCOL_RAW` from `hvsocket.h`, which the Windows metadata does not
/// carry, so it is spelled here.
const HV_PROTOCOL_RAW: i32 = 1;

/// How long a connect attempt waits before it is a failure to report.
///
/// A guest whose services are up answers immediately; this is what bounds the
/// case where the partition is there and nothing is listening on it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a read waits before letting its caller do something else.
///
/// A quarter of a second, matching the agent socket's poll: it is how long
/// closing a window takes, and four wakeups a second on an idle desktop is what
/// it costs.
pub const READ_POLL: Duration = Duration::from_millis(250);

/// The service GUID a Linux guest's vsock `port` arrives on.
///
/// Hyper-V maps `AF_VSOCK` ports onto HvSocket services through a fixed
/// template: the port becomes the first field of the GUID and the rest is the
/// constant Linux integration uses. Derived rather than invented, which is what
/// lets the guest keep speaking plain vsock.
#[must_use]
pub fn vsock_service_id(port: u32) -> GUID {
    GUID::from_values(
        port,
        0xfacb,
        0x11e6,
        [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
    )
}

/// An HvSocket address: which partition, and which service on it.
///
/// `SOCKADDR_HV` from `hvsocket.h`. The Windows metadata does not describe it,
/// so the layout is spelled out; it is stable, and Winsock reads it by size.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: GUID,
    service_id: GUID,
}

/// One connection to one service of one VM.
pub struct HvSocket {
    socket: SOCKET,
}

impl HvSocket {
    /// Connects to `port` on the partition `runtime_id` names.
    ///
    /// The socket is put into non-blocking mode for the connect and left there:
    /// reads poll with `select`, and a caller that has nothing to read gets
    /// `WouldBlock` rather than a parked thread.
    ///
    /// # Errors
    ///
    /// [`ConnectError::PartitionGone`] when the compute system is not there --
    /// a stopped VM, which is not a failure -- [`ConnectError::Refused`] when
    /// the partition is there and nothing is listening, which is a guest whose
    /// services are still starting, and [`ConnectError::Failed`] for anything
    /// else.
    pub fn connect(
        runtime_id: &[u8; 16],
        port: u32,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        initialize_winsock()?;

        // SAFETY: A plain socket creation; the returned handle is owned by the
        // `HvSocket` built below, which closes it exactly once.
        let handle = unsafe { socket(AF_HYPERV.into(), SOCK_STREAM, HV_PROTOCOL_RAW) }
            .map_err(|error| ConnectError::Failed(error.to_string()))?;
        let stream = Self { socket: handle };
        stream.set_non_blocking()?;

        let address = SockaddrHv {
            family: AF_HYPERV,
            reserved: 0,
            vm_id: GUID::from_u128(u128::from_be_bytes(*runtime_id)),
            service_id: vsock_service_id(port),
        };

        // SAFETY: `address` is a valid `SOCKADDR_HV` living across the call, and
        // its length is what Winsock expects for an `AF_HYPERV` address.
        let started = unsafe {
            connect(
                stream.socket,
                (&raw const address).cast::<SOCKADDR>(),
                i32::try_from(mem::size_of::<SockaddrHv>()).expect("an address is 36 bytes"),
            )
        };
        if started == SOCKET_ERROR {
            let code = last_error_code();
            if code != WSAEWOULDBLOCK.0 {
                return Err(ConnectError::classify(code));
            }
        }

        stream.wait_writable(timeout)?;

        log::debug!("connected to vsock port {port:#x} of partition {address:?}", address = address.vm_id);
        Ok(stream)
    }

    /// Puts the socket into non-blocking mode.
    fn set_non_blocking(&self) -> Result<(), ConnectError> {
        let mut enabled: u32 = 1;
        // SAFETY: `self.socket` is owned and `enabled` outlives the call.
        let set = unsafe { ioctlsocket(self.socket, FIONBIO, &raw mut enabled) };
        if set == SOCKET_ERROR {
            return Err(ConnectError::Failed(format!(
                "the socket could not be made non-blocking: Winsock error {}",
                last_error_code()
            )));
        }

        Ok(())
    }

    /// Waits for a non-blocking connect to finish, or for `timeout` to pass.
    fn wait_writable(&self, timeout: Duration) -> Result<(), ConnectError> {
        let mut writable = FD_SET {
            fd_count: 1,
            ..Default::default()
        };
        writable.fd_array[0] = self.socket;
        let mut failed = writable;
        let timeout = timeval(timeout);

        // SAFETY: both sets name this owned socket and outlive the call, as does
        // `timeout`. Windows ignores the first argument to `select`.
        let ready = unsafe {
            select(
                0,
                None,
                Some(&mut writable),
                Some(&mut failed),
                Some(&raw const timeout),
            )
        };
        match ready {
            0 => Err(ConnectError::Refused(
                "nothing answered on the guest's display service".to_owned(),
            )),
            SOCKET_ERROR => Err(ConnectError::classify(last_error_code())),
            _ if failed.fd_count > 0 => Err(ConnectError::classify(last_error_code())),
            _ => Ok(()),
        }
    }
}

impl Read for HvSocket {
    /// Reads what has arrived, polling no longer than [`READ_POLL`].
    ///
    /// The wait is `select` rather than `SO_RCVTIMEO`: HvSocket can signal a
    /// receive timeout as a clean read, which is indistinguishable from the
    /// guest closing the connection. A poll that expires becomes `WouldBlock`,
    /// which the record reader reports as an idle connection.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut readable = FD_SET {
            fd_count: 1,
            ..Default::default()
        };
        readable.fd_array[0] = self.socket;
        let timeout = timeval(READ_POLL);

        // SAFETY: `readable` names this owned socket and outlives the call, as
        // does `timeout`.
        let ready = unsafe { select(0, Some(&mut readable), None, None, Some(&raw const timeout)) };
        match ready {
            0 => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
            SOCKET_ERROR => return Err(io::Error::from_raw_os_error(last_error_code())),
            _ => {}
        }

        // SAFETY: `self.socket` is owned and `buffer` is valid for writes for
        // its own length. `select` just reported it readable.
        let read = unsafe { recv(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if read >= 0 {
            return Ok(read as usize);
        }

        let code = last_error_code();
        if code == WSAEWOULDBLOCK.0 {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Err(io::Error::from_raw_os_error(code))
    }
}

impl Write for HvSocket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: `self.socket` is owned and `buffer` is valid for reads for
        // its own length.
        let written = unsafe { send(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if written < 0 {
            let code = last_error_code();
            if code == WSAEWOULDBLOCK.0 {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            return Err(io::Error::from_raw_os_error(code));
        }

        Ok(written as usize)
    }

    /// Nothing is buffered on this side: `send` hands the bytes to the
    /// transport, and there is no user-space buffer left to push.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for HvSocket {
    fn drop(&mut self) {
        // SAFETY: This stream exclusively owns the socket and closes it once.
        unsafe { closesocket(self.socket) };
    }
}

/// Why a socket could not be opened.
#[derive(Debug)]
pub enum ConnectError {
    /// The compute system is not there. A stopped VM, not a fault.
    PartitionGone,
    /// The partition is there and nothing is listening.
    Refused(String),
    /// Winsock refused the socket or the address.
    Failed(String),
}

impl ConnectError {
    /// Sorts a Winsock error into the three answers that matter.
    ///
    /// The distinction the viewer acts on is "the VM is gone" against "the
    /// guest is not ready", because the first closes the window quietly and the
    /// second is retried. Verified against a live partition in #121; if a
    /// stopped VM reports something else, this is the one place to change.
    fn classify(code: i32) -> Self {
        if code == WSAENETUNREACH.0 || code == WSAENETDOWN.0 || code == WSAEBADF.0 {
            return Self::PartitionGone;
        }
        if code == WSAECONNREFUSED.0 {
            return Self::Refused(format!("Winsock error {code}"));
        }

        Self::Failed(format!("Winsock error {code}"))
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PartitionGone => formatter.write_str("the VM is not running"),
            Self::Refused(detail) => {
                write!(formatter, "the guest's display service is not up: {detail}")
            }
            Self::Failed(detail) => write!(formatter, "the socket could not be opened: {detail}"),
        }
    }
}

impl Error for ConnectError {}

/// Brings Winsock up once for the process.
fn initialize_winsock() -> Result<(), ConnectError> {
    use std::sync::OnceLock;
    static WINSOCK: OnceLock<Result<(), String>> = OnceLock::new();

    WINSOCK
        .get_or_init(|| {
            let mut data = WSADATA::default();
            // SAFETY: `data` is a valid `WSADATA` for the duration of the call.
            let result = unsafe { WSAStartup(0x0202, &raw mut data) };
            if result == 0 {
                Ok(())
            } else {
                Err(format!("WSAStartup failed with {result}"))
            }
        })
        .clone()
        .map_err(ConnectError::Failed)
}

/// The Winsock error the last call left behind.
fn last_error_code() -> i32 {
    // SAFETY: A thread-local read of the last Winsock error.
    unsafe { WSAGetLastError() }.0
}

/// Splits a duration the way `select` wants it.
fn timeval(duration: Duration) -> TIMEVAL {
    TIMEVAL {
        tv_sec: i32::try_from(duration.as_secs()).unwrap_or(i32::MAX),
        tv_usec: i32::try_from(duration.subsec_micros()).expect("under a million microseconds"),
    }
}
```

- [ ] **Step 5: Declare the module**

In `crates/display-viewer/src/lib.rs`, add:

```rust
#[cfg(windows)]
pub mod windows;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — three tests in `windows::hvsocket::tests`.

- [ ] **Step 7: Commit**

```bash
git add crates/display-viewer/src/windows crates/display-viewer/src/lib.rs
git commit -m "TASK-117: Connect the viewer's three HvSocket channels"
```

---

### Task 8: One window per VM

**Files:**
- Create: `crates/display-viewer/src/windows/ipc.rs`
- Modify: `crates/display-viewer/src/windows/mod.rs` (uncomment the `ipc` declaration)

**Interfaces:**
- Consumes: `launch::{Command, Message, Link}` (Task 1).
- Produces:
  - `vmlord_display_viewer::windows::ipc::{SingleInstance, CommandServer, mutex_name, pipe_name, send_command, IpcError}`
  - `SingleInstance::take(runtime_id: &[u8; 16]) -> Result<Option<SingleInstance>, IpcError>` — `None` means another viewer already has this VM
  - `CommandServer::start(runtime_id: &[u8; 16], sink: Sender<Command>) -> Result<CommandServer, IpcError>`
  - `send_command(runtime_id: &[u8; 16], command: Command) -> Result<(), IpcError>`
  - `mutex_name(runtime_id) -> String`, `pipe_name(runtime_id) -> String`

The pipe carries `Command` and nothing else. Its authentication is the default DACL of the launching user, which is enough for two operations a same-user process could perform anyway. Asking for a **new session** is deliberately not on this pipe: that goes over the launch pipes with the token, and so can only be answered by the VMLord instance that spawned this viewer.

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/windows/ipc.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::{CommandServer, SingleInstance, mutex_name, pipe_name, send_command};
    use crate::launch::{Command, Message};

    /// A runtime id nothing else in the test process uses.
    fn runtime_id(tag: u8) -> [u8; 16] {
        let mut id = [0xab; 16];
        id[15] = tag;
        id
    }

    #[test]
    fn the_names_are_per_vm_and_local_to_this_session() {
        let name = mutex_name(&runtime_id(1));
        assert!(name.starts_with("Local\\VMLord.Display."));
        assert_ne!(name, mutex_name(&runtime_id(2)));

        let pipe = pipe_name(&runtime_id(1));
        assert!(pipe.starts_with("\\\\.\\pipe\\vmlord-display."));
        assert_ne!(pipe, pipe_name(&runtime_id(2)));
    }

    #[test]
    fn a_second_viewer_for_the_same_vm_finds_the_mutex_taken() {
        let id = runtime_id(3);
        let first = SingleInstance::take(&id)
            .expect("the mutex can be created")
            .expect("nothing else holds it");

        assert!(
            SingleInstance::take(&id)
                .expect("the mutex can be opened")
                .is_none(),
            "a second viewer must find the first"
        );

        drop(first);
        assert!(
            SingleInstance::take(&id)
                .expect("the mutex can be created")
                .is_some(),
            "the mutex is released when the viewer exits"
        );
    }

    #[test]
    fn two_vms_get_a_window_each() {
        let _first = SingleInstance::take(&runtime_id(4))
            .expect("the mutex can be created")
            .expect("nothing else holds it");
        let second = SingleInstance::take(&runtime_id(5)).expect("the mutex can be created");

        assert!(second.is_some());
    }

    #[test]
    fn the_pipe_delivers_focus_and_close() {
        let id = runtime_id(6);
        let (sink, commands) = mpsc::channel();
        let _server = CommandServer::start(&id, sink).expect("the pipe can be created");

        send_command(&id, Command::Focus).expect("the server is listening");
        send_command(&id, Command::Close).expect("the server is listening");

        assert_eq!(
            commands
                .recv_timeout(Duration::from_secs(5))
                .expect("a focus"),
            Command::Focus
        );
        assert_eq!(
            commands
                .recv_timeout(Duration::from_secs(5))
                .expect("a close"),
            Command::Close
        );
    }

    #[test]
    fn a_request_for_a_new_session_is_not_answerable_on_this_pipe() {
        let id = runtime_id(7);
        let (sink, commands) = mpsc::channel();
        let _server = CommandServer::start(&id, sink).expect("the pipe can be created");

        super::send_message(&id, &Message::RequestRelay { token: vec![1; 32] })
            .expect("the server is listening");
        // Something the server does accept, so that the test is not waiting on
        // a message it already refused.
        send_command(&id, Command::Focus).expect("the server is listening");

        assert_eq!(
            commands
                .recv_timeout(Duration::from_secs(5))
                .expect("the focus that followed"),
            Command::Focus,
            "a refresh must not be delivered as a command"
        );
    }

    #[test]
    fn a_pipe_nobody_is_serving_reports_it_rather_than_hanging() {
        let started = Instant::now();

        assert!(send_command(&runtime_id(8), Command::Focus).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `SingleInstance` in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/display-viewer/src/windows/ipc.rs`:

```rust
//! One window per VM, and the two things a later VMLord can ask it to do.
//!
//! A named mutex answers the question "is there already a viewer for this VM?"
//! before anything else happens, so a second Connect finds a window rather than
//! opening one. A named pipe answers everything after that: `Focus` brings the
//! window forward, `Close` shuts the session down.
//!
//! The pipe's authentication is the default DACL of the launching user. That is
//! the right amount for these two operations -- a same-user process could
//! foreground a window or close it without asking us -- and it is deliberately
//! not enough for anything else: asking for a **new session** goes over the
//! launch pipes with the token, so only the VMLord instance that spawned this
//! viewer can be the one to run a handshake for it.
//!
//! The pipe server belongs to the viewer, so it outlives the VMLord that
//! started it and is found by a later one. That is the repeated-Connect case
//! that matters.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
            HANDLE,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING, ReadFile, WriteFile,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_ACCESS_DUPLEX,
                PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Threading::CreateMutexW,
        },
    },
    core::{HSTRING, PCWSTR},
};

use crate::launch::{Command, Link, Message};

/// The most a command message may be. A command is a dozen bytes.
const MAX_COMMAND: usize = 4096;

/// How many bytes the pipe's buffers hold.
const PIPE_BUFFER: u32 = 4096;

/// The mutex one viewer holds for the life of its process.
#[must_use]
pub fn mutex_name(runtime_id: &[u8; 16]) -> String {
    format!("Local\\VMLord.Display.{}", hyphenated(runtime_id))
}

/// The pipe one viewer listens on.
#[must_use]
pub fn pipe_name(runtime_id: &[u8; 16]) -> String {
    format!("\\\\.\\pipe\\vmlord-display.{}", hyphenated(runtime_id))
}

/// The runtime id as the text a name carries.
fn hyphenated(runtime_id: &[u8; 16]) -> String {
    let hex: String = runtime_id.iter().map(|byte| format!("{byte:02x}")).collect();

    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The claim on one VM's window, held for the life of the process.
pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// Takes the claim, or reports that another viewer has it.
    ///
    /// `Ok(None)` is the repeated-Connect case: a viewer for this VM is already
    /// running, and what the caller should do is focus it through the pipe.
    ///
    /// # Errors
    ///
    /// [`IpcError::Win32`] if the mutex could not be created at all.
    pub fn take(runtime_id: &[u8; 16]) -> Result<Option<Self>, IpcError> {
        let name = HSTRING::from(mutex_name(runtime_id));
        // SAFETY: `name` is a NUL-terminated wide string living across the call.
        // The returned handle is owned by the `SingleInstance` below.
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
            .map_err(|error| IpcError::Win32(error.to_string()))?;

        // SAFETY: A thread-local read of the last error, which `CreateMutexW`
        // sets to `ERROR_ALREADY_EXISTS` when it opened rather than created.
        let existed = unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS;
        if existed {
            // SAFETY: The handle is owned here and closed exactly once.
            unsafe { let _ = CloseHandle(handle); };
            return Ok(None);
        }

        Ok(Some(Self { handle }))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: This instance owns the handle and closes it once.
        unsafe { let _ = CloseHandle(self.handle); };
    }
}

/// A handle on its way to the thread that will own it.
///
/// `HANDLE` is a raw pointer and so is not `Send`. Moving one to a thread that
/// then owns it exclusively is sound, and this is where that is said out loud.
struct SendHandle(HANDLE);

// SAFETY: the handle is moved, not shared: the thread it reaches is the only
// one that uses it, and the only one that closes it.
unsafe impl Send for SendHandle {}

/// The pipe a later VMLord asks this window to focus or close through.
pub struct CommandServer {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    name: String,
}

impl CommandServer {
    /// Starts serving, on a thread of its own.
    ///
    /// # Errors
    ///
    /// [`IpcError::Win32`] if the pipe could not be created, which is what a
    /// second viewer for the same VM would see -- and cannot happen, because
    /// [`SingleInstance`] has already answered that question.
    pub fn start(runtime_id: &[u8; 16], sink: Sender<Command>) -> Result<Self, IpcError> {
        let name = pipe_name(runtime_id);
        // Created here rather than on the thread, so that a failure is the
        // caller's to see.
        let first = create_pipe(&name)?;
        let running = Arc::new(AtomicBool::new(true));

        let thread = {
            let running = Arc::clone(&running);
            let first = SendHandle(first);
            thread::spawn(move || serve(first.0, &sink, &running))
        };

        Ok(Self {
            running,
            thread: Some(thread),
            name,
        })
    }
}

impl Drop for CommandServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // A connection of our own, so that the blocking `ConnectNamedPipe`
        // returns and the thread sees that it should stop. Its failure is
        // expected once the thread has already gone.
        let _ = connect_pipe(&self.name);

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serves one connection at a time until the server is dropped.
///
/// One pipe instance, reused: a viewer serves one VMLord, and a second
/// connection waits its turn rather than being answered out of order.
fn serve(pipe: HANDLE, sink: &Sender<Command>, running: &Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        // SAFETY: `pipe` is an owned pipe instance in the listening state.
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        if connected.is_err() {
            // SAFETY: A thread-local read of the last error. A client that got
            // in before the call is `ERROR_PIPE_CONNECTED`, which is a
            // connection rather than a failure.
            let code = unsafe { windows::Win32::Foundation::GetLastError() };
            if code != ERROR_PIPE_CONNECTED {
                break;
            }
        }

        if running.load(Ordering::Relaxed) {
            // Borrowed: the listening instance is closed once, below, after the
            // loop -- not at the end of every connection.
            let mut handle = PipeHandle::borrowed(pipe);
            let mut link = Link::new(&mut handle, io::sink());
            match link.read() {
                Ok(Message::Command(command)) => {
                    log::info!("the viewer was asked to {command:?}");
                    if sink.send(command).is_err() {
                        break;
                    }
                }
                Ok(other) => log::warn!(
                    "a {other:?} arrived on the command pipe, which answers commands only"
                ),
                Err(error) => log::debug!("a command could not be read: {error}"),
            }
        }

        // SAFETY: `pipe` is owned by this thread and is connected.
        unsafe { let _ = DisconnectNamedPipe(pipe); };
    }

    // SAFETY: The server owns the pipe and closes it once.
    unsafe { let _ = CloseHandle(pipe); };
}


/// Creates the listening end of the command pipe.
fn create_pipe(name: &str) -> Result<HANDLE, IpcError> {
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is a NUL-terminated wide string living across the call.
    // `None` for the security attributes is the default DACL of the launching
    // user, which is the authentication this pipe relies on.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            None,
        )
    };
    if handle.is_invalid() {
        return Err(IpcError::Win32(format!(
            "the command pipe {name} could not be created"
        )));
    }

    Ok(handle)
}

/// Opens the client end of a viewer's command pipe.
fn connect_pipe(name: &str) -> Result<PipeHandle, IpcError> {
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is a NUL-terminated wide string living across the call, and
    // the returned handle is owned by the `PipeHandle` below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            (GENERIC_READ.0 | GENERIC_WRITE.0) as u32,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| IpcError::Unreachable(error.to_string()))?;

    Ok(PipeHandle::owned(handle))
}

/// Asks the viewer of `runtime_id` to do something.
///
/// # Errors
///
/// [`IpcError::Unreachable`] if no viewer is listening, which is how a caller
/// learns that the window it expected is gone.
pub fn send_command(runtime_id: &[u8; 16], command: Command) -> Result<(), IpcError> {
    send_message(runtime_id, &Message::Command(command))
}

/// Writes one message to a viewer's command pipe.
///
/// Public to the crate so that the tests can send something the server refuses;
/// production code sends commands.
pub fn send_message(runtime_id: &[u8; 16], message: &Message) -> Result<(), IpcError> {
    let mut handle = connect_pipe(&pipe_name(runtime_id))?;
    let mut link = Link::new(io::empty(), &mut handle);

    link.write(message)
        .map_err(|error| IpcError::Unreachable(error.to_string()))
}

/// A pipe handle that reads and writes like a stream.
/// A pipe handle that reads and writes like a stream.
///
/// `owned` says who closes it: the client end this module opened closes on
/// drop, and the server's listening instance does not -- it is reused for the
/// next connection and closed once, when the loop ends.
struct PipeHandle {
    handle: HANDLE,
    owned: bool,
}

impl PipeHandle {
    /// A handle this wrapper closes.
    fn owned(handle: HANDLE) -> Self {
        Self { handle, owned: true }
    }

    /// A handle somebody else closes.
    fn borrowed(handle: HANDLE) -> Self {
        Self { handle, owned: false }
    }
}

impl Read for PipeHandle {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let take = buffer.len().min(MAX_COMMAND);
        // SAFETY: `self.handle` is a valid pipe handle and `buffer` is valid for
        // writes for `take` bytes.
        unsafe { ReadFile(self.handle, Some(&mut buffer[..take]), Some(&raw mut read), None) }
            .map_err(|error| io::Error::other(error.to_string()))?;

        Ok(read as usize)
    }
}

impl Write for PipeHandle {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: `self.handle` is a valid pipe handle and `buffer` is valid for
        // reads for its own length.
        unsafe { WriteFile(self.handle, Some(buffer), Some(&raw mut written), None) }
            .map_err(|error| io::Error::other(error.to_string()))?;

        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }

        // SAFETY: an owned handle, closed exactly once.
        unsafe { let _ = CloseHandle(self.handle); };
    }
}


/// Why a viewer could not be claimed or reached.
#[derive(Debug)]
pub enum IpcError {
    /// A Win32 call refused.
    Win32(String),
    /// No viewer is listening on that VM's pipe.
    Unreachable(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32(detail) => write!(formatter, "a Windows call refused: {detail}"),
            Self::Unreachable(detail) => {
                write!(formatter, "no display viewer is listening: {detail}")
            }
        }
    }
}

impl Error for IpcError {}
```

- [ ] **Step 4: Uncomment the declaration**

In `crates/display-viewer/src/windows/mod.rs`, uncomment `pub mod ipc;`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — six tests in `windows::ipc::tests`.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/src/windows
git commit -m "TASK-117: Give each VM one viewer window, focused through a pipe"
```

---

### Task 9: The window

**Files:**
- Create: `crates/display-viewer/src/windows/window.rs`
- Modify: `crates/display-viewer/src/windows/mod.rs` (uncomment the `window` declaration)

**Interfaces:**
- Consumes: `status::{Button, hit_test}` (Task 3).
- Produces:
  - `vmlord_display_viewer::windows::window::{Window, Poster, UiEvent, Shared, WM_SIGNAL, WM_FOCUS_REQUEST, WM_CLOSE_REQUEST}`
  - `Window::open(title: &str, width: i32, height: i32, shared: Arc<Shared>) -> Result<Window, String>`
  - `Window::handle(&self) -> HWND`, `Window::client_size(&self) -> (i32, i32)`, `Window::focus(&self)`, `Window::poster(&self) -> Poster`, `Window::pump(&self) -> bool`
  - `Poster::post(&self, message: u32)` — `Send`, so the session thread can wake the pump
  - `Shared { failed: AtomicBool, events: Sender<UiEvent> }`
  - `UiEvent` variants: `Pressed(Button)`, `Resized(i32, i32)`, `Closing`

The window is on the thread that pumps it, which is the main thread. Everything else — sockets, decode, the launch pipes — is on threads of its own and reaches the window by posting a message. That is what keeps the buttons alive on a Failed screen: the pump never stopped, because nothing that can block runs on it.

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/windows/window.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering, mpsc};

    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_LBUTTONUP};

    use super::{Shared, UiEvent, WM_SIGNAL, Window};
    use crate::status::{self, Button};

    fn shared() -> (Arc<Shared>, mpsc::Receiver<UiEvent>) {
        let (events, received) = mpsc::channel();
        (Arc::new(Shared::new(events)), received)
    }

    #[test]
    fn a_window_opens_at_the_size_it_was_asked_for() {
        let (shared, _events) = shared();
        let window = Window::open("test - VMLord Display", 640, 480, shared)
            .expect("a window class and a window");

        assert_eq!(window.client_size(), (640, 480));
    }

    #[test]
    fn a_posted_signal_reaches_the_pump() {
        let (shared, _events) = shared();
        let window = Window::open("test - VMLord Display", 320, 240, shared).expect("a window");
        let poster = window.poster();

        poster.post(WM_SIGNAL);

        // The pump runs the queue dry and answers whether the window is still
        // open. A posted signal is not a quit.
        assert!(window.pump());
    }

    #[test]
    fn a_click_on_retry_is_reported_only_while_the_failed_screen_is_up() {
        let (shared, events) = shared();
        let window = Window::open("test - VMLord Display", 800, 600, Arc::clone(&shared))
            .expect("a window");
        let (_, (x, y, w, h)) = status::buttons(800, 600)[0];
        let point = ((y + h / 2) << 16) | (x + w / 2);

        // Nothing is on screen but the picture: a click is not a button.
        // SAFETY: the window is open and owned by this test.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_LBUTTONUP,
                Some(windows::Win32::Foundation::WPARAM(0)),
                Some(windows::Win32::Foundation::LPARAM(point as isize)),
            );
        }
        assert!(events.try_recv().is_err());

        shared.failed.store(true, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_LBUTTONUP,
                Some(windows::Win32::Foundation::WPARAM(0)),
                Some(windows::Win32::Foundation::LPARAM(point as isize)),
            );
        }

        assert_eq!(events.try_recv().expect("a press"), UiEvent::Pressed(Button::Retry));
    }

    #[test]
    fn closing_the_window_is_reported_before_the_pump_ends() {
        let (shared, events) = shared();
        let window = Window::open("test - VMLord Display", 320, 240, shared).expect("a window");

        // SAFETY: the window is open and owned by this test.
        unsafe {
            SendMessageW(
                window.handle(),
                windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                None,
                None,
            );
        }

        assert_eq!(events.try_recv().expect("a closing"), UiEvent::Closing);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `Window` in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/display-viewer/src/windows/window.rs` a module with:

* A module comment saying: the window lives on the thread that pumps it; nothing that can block runs there; the session thread reaches it by posting `WM_SIGNAL`.
* Constants `WM_SIGNAL = WM_APP + 1`, `WM_FOCUS_REQUEST = WM_APP + 2`, `WM_CLOSE_REQUEST = WM_APP + 3`.
* `pub struct Shared { pub failed: AtomicBool, events: Sender<UiEvent> }` with `Shared::new(events)` and `Shared::report(&self, event: UiEvent)` (a send failure is logged and dropped: a window whose reader is gone is one that is closing anyway).
* `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum UiEvent { Pressed(Button), Resized(i32, i32), Closing }`.
* `pub struct Window { hwnd: HWND, shared: Arc<Shared> }` and `pub struct Poster(isize)` with `unsafe impl Send for Poster {}` — a window handle is process-wide and `PostMessageW` is the documented way to reach a window from another thread.
* `Window::open`:
  * registers the class `VMLordDisplayWindow` once, through a `OnceLock<()>`, with `wnd_proc` below, `hInstance` from `GetModuleHandleW(None)`, a black `HBRUSH` background and `LoadCursorW(None, IDC_ARROW)`;
  * `AdjustWindowRect` for `WS_OVERLAPPEDWINDOW` so the **client** area is the size asked for;
  * `CreateWindowExW` with the title, `CW_USEDEFAULT` position, then `SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::into_raw(shared) as isize)`;
  * `ShowWindow(hwnd, SW_SHOW)` and `UpdateWindow(hwnd)`.
* `Window::client_size` — `GetClientRect`, returning `(right - left, bottom - top)`.
* `Window::focus` — `ShowWindow(hwnd, SW_RESTORE)` then `SetForegroundWindow(hwnd)`.
* `Window::poster` — `Poster(self.hwnd.0 as isize)`; `Poster::post` calls `PostMessageW(HWND(self.0 as *mut _), message, None, None)`.
* `Window::pump` — `PeekMessageW(..., PM_REMOVE)` in a loop, `TranslateMessage` and `DispatchMessageW` for each, returning `false` when `WM_QUIT` arrives.
* `wnd_proc`, an `extern "system" fn`, reading the `Arc<Shared>` back from `GWLP_USERDATA` **without** taking ownership (`ManuallyDrop`), and handling:
  * `WM_LBUTTONUP` — only while `shared.failed` is set: `GetClientRect`, then `status::hit_test(width, height, x, y)`, reporting `UiEvent::Pressed`;
  * `WM_SIZE` — `UiEvent::Resized(LOWORD(lparam), HIWORD(lparam))`;
  * `WM_CLOSE` — `UiEvent::Closing`, then `DestroyWindow`;
  * `WM_DESTROY` — `PostQuitMessage(0)`, and drop the `Arc` taken back out of `GWLP_USERDATA`;
  * `WM_ERASEBKGND` — return `1`, because the renderer paints every pixel and letting Windows erase first is a flash of black on every resize;
  * everything else — `DefWindowProcW`.

Every `unsafe` block carries a `// SAFETY:` comment naming what makes it sound: the handle is owned by this `Window`, the pointer came from `Arc::into_raw` and is not freed while the window lives, the string is NUL-terminated and outlives the call.

- [ ] **Step 4: Uncomment the declaration**

In `crates/display-viewer/src/windows/mod.rs`, uncomment `pub mod window;`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — four tests in `windows::window::tests`.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/src/windows
git commit -m "TASK-117: Open the viewer's window and pump its messages"
```

---

### Task 10: The renderer

**Files:**
- Create: `crates/display-viewer/src/windows/d3d.rs`
- Modify: `crates/display-viewer/src/windows/mod.rs` (uncomment the `d3d` declaration)
- Modify: `crates/display-viewer/src/video.rs` (the cursor bitmap conversion, which is arithmetic and belongs in safe code)

**Interfaces:**
- Consumes: `video::{Video, premultiplied}` (Task 4 plus this task's addition), `status::{Progress, Status, buttons}` (Task 3).
- Produces:
  - `vmlord_display_viewer::video::premultiplied(image: &OwnedCursorImage) -> Vec<u8>`
  - `vmlord_display_viewer::windows::d3d::{Renderer, MAX_DEVICE_LOSSES}`
  - `Renderer::open(hwnd: HWND) -> Result<Renderer, String>`
  - `Renderer::configure(&mut self, geometry: Geometry) -> Result<(), String>`
  - `Renderer::upload(&mut self, frame: &[u8], damage: &[Rect]) -> Result<(), String>`
  - `Renderer::set_cursor(&mut self, image: &OwnedCursorImage) -> Result<(), String>`, `Renderer::show_cursor(&mut self, visible: bool)`
  - `Renderer::present(&mut self, progress: &Progress, vm_name: &str) -> Result<(), String>`
  - `Renderer::resize_swapchain(&mut self, width: u32, height: u32) -> Result<(), String>`
  - `Renderer::recover(&mut self) -> Result<bool, String>` — `Ok(false)` once `MAX_DEVICE_LOSSES` is spent

- [ ] **Step 1: Write the failing test for the cursor arithmetic**

Add to the `mod tests` in `crates/display-viewer/src/video.rs`:

```rust
    #[test]
    fn a_cursor_bitmap_is_premultiplied_without_reading_past_its_rows() {
        use vmlord_display_codec::OwnedCursorImage;

        let image = OwnedCursorImage {
            // Two pixels: opaque white, then half-transparent white.
            pixels: vec![255, 255, 255, 255, 255, 255, 255, 128],
            width: 2,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
        };

        let bytes = super::premultiplied(&image);

        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[0..4], &[255, 255, 255, 255]);
        // 255 * 128 / 255 == 128, in every channel but alpha.
        assert_eq!(&bytes[4..8], &[128, 128, 128, 128]);
    }

    #[test]
    fn a_cursor_bitmap_whose_pixels_do_not_match_its_size_is_padded_rather_than_read_past() {
        use vmlord_display_codec::OwnedCursorImage;

        let image = OwnedCursorImage {
            pixels: vec![255; 4],
            width: 4,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
        };

        assert_eq!(super::premultiplied(&image).len(), 4 * 4 * 4);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find function `premultiplied``.

- [ ] **Step 3: Write the cursor arithmetic**

Add to `crates/display-viewer/src/video.rs`, after `impl Default for Video`:

```rust
/// A cursor bitmap as an alpha icon wants it: BGRA, premultiplied.
///
/// The codec hands over straight alpha, and `CreateIconIndirect` composites
/// premultiplied. Doing the multiply here rather than in the renderer keeps it
/// where it can be tested without a device -- and keeps a bitmap that does not
/// match its own dimensions from being read past: the output is always
/// `width * height * 4` bytes, and a short input is padded with transparency.
#[must_use]
pub fn premultiplied(image: &OwnedCursorImage) -> Vec<u8> {
    let pixels = image.width as usize * image.height as usize;
    let mut out = vec![0u8; pixels * 4];

    for (index, chunk) in image.pixels.chunks_exact(4).take(pixels).enumerate() {
        let alpha = u32::from(chunk[3]);
        let target = &mut out[index * 4..index * 4 + 4];
        for channel in 0..3 {
            target[channel] = u8::try_from(u32::from(chunk[channel]) * alpha / 255)
                .expect("a product of two bytes divided by 255");
        }
        target[3] = chunk[3];
    }

    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — the two new tests in `video::tests`.

- [ ] **Step 5: Write the failing test for the renderer**

Create `crates/display-viewer/src/windows/d3d.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        time::Instant,
    };

    use vmlord_display_codec::{Geometry, PixelFormat, Rect, TileSize};

    use super::Renderer;
    use crate::{
        status::Progress,
        windows::window::{Shared, Window},
    };

    fn window() -> Window {
        let (events, received) = mpsc::channel();
        // The receiver is leaked deliberately: the window outlives it in the
        // binary, and a dropped receiver would only make `report` log.
        std::mem::forget(received);

        Window::open("renderer test - VMLord Display", 640, 480, Arc::new(Shared::new(events)))
            .expect("a window")
    }

    fn geometry(width: u32, height: u32) -> Geometry {
        Geometry::new(width, height, TileSize::ThirtyTwo, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows")
    }

    #[test]
    fn a_renderer_opens_on_this_machine() {
        let window = window();

        // Hardware where there is any, WARP where there is not: a test host and
        // a headless build agent both have to be able to run this.
        Renderer::open(window.handle()).expect("a device, hardware or WARP");
    }

    #[test]
    fn a_stream_config_sizes_the_texture_and_a_second_one_replaces_it() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");

        renderer.configure(geometry(320, 200)).expect("a texture");
        assert_eq!(renderer.stream_size(), Some((320, 200)));

        renderer.configure(geometry(640, 480)).expect("a texture");
        assert_eq!(renderer.stream_size(), Some((640, 480)));
    }

    #[test]
    fn only_the_rectangles_that_changed_are_uploaded_and_they_are_clipped() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");
        let geometry = geometry(100, 60);
        renderer.configure(geometry).expect("a texture");

        let frame = vec![0x7f; geometry.frame_bytes()];
        // The last column and row are narrower than a tile, and a rectangle
        // that runs past the edge is clipped rather than refused.
        let damage = [
            Rect { x: 0, y: 0, width: 32, height: 32 },
            Rect { x: 96, y: 32, width: 32, height: 32 },
        ];

        renderer.upload(&frame, &damage).expect("an upload");
        assert_eq!(renderer.uploaded_rectangles(), 2);
    }

    #[test]
    fn an_upload_before_any_stream_config_is_refused_rather_than_guessed_at() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");

        assert!(renderer.upload(&[0; 16], &[Rect { x: 0, y: 0, width: 2, height: 2 }]).is_err());
    }

    #[test]
    fn the_overlay_presents_in_every_state_that_shows_one() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");
        let now = Instant::now();
        let mut progress = Progress::new(now);

        renderer.present(&progress, "ubuntu-24.04").expect("a present");
        progress.tick(now + crate::status::RETRY_BUDGET);
        renderer.present(&progress, "ubuntu-24.04").expect("a present");
    }

    #[test]
    fn a_device_is_recovered_a_bounded_number_of_times() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");

        for _ in 0..super::MAX_DEVICE_LOSSES {
            assert!(renderer.recover().expect("a rebuilt device"));
        }
        assert!(
            !renderer.recover().expect("the count is not an error"),
            "a fourth loss in one session is not recovered from"
        );
    }
}
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find type `Renderer` in this scope`.

- [ ] **Step 7: Write the implementation**

Prepend to `crates/display-viewer/src/windows/d3d.rs` a module with:

* A module comment saying: D3D11 draws the picture, Direct2D draws the overlay, and the renderer uploads damage rather than frames — a typing desktop moves kilobytes where a naive pipeline moves a frame.
* `pub const MAX_DEVICE_LOSSES: u32 = 3;` with the reason: a fourth loss in one session is something patience will not fix.
* `pub struct Renderer` holding: `hwnd`, `device: ID3D11Device`, `context: ID3D11DeviceContext`, `swapchain: IDXGISwapChain1`, `target: Option<ID3D11RenderTargetView>`, `texture: Option<ID3D11Texture2D>`, `stream: Option<Geometry>`, `d2d: ID2D1Factory1`, `dwrite: IDWriteFactory`, `brush`/`text_format` built per present, `cursor: Option<HCURSOR>`, `losses: u32`, `uploaded: usize`.
* `Renderer::open(hwnd)`:
  * `D3D11CreateDevice` with `D3D_DRIVER_TYPE_HARDWARE` and `D3D11_CREATE_DEVICE_BGRA_SUPPORT`, falling back to `D3D_DRIVER_TYPE_WARP` — a build agent has no GPU, and a viewer that refuses to open on one is a viewer nobody can test;
  * `CreateDXGIFactory2` → `CreateSwapChainForHwnd` with `DXGI_FORMAT_B8G8R8A8_UNORM`, `DXGI_SWAP_EFFECT_FLIP_DISCARD`, two buffers;
  * `D2D1CreateFactory` and `DWriteCreateFactory` once, kept for the overlay.
* `Renderer::configure(geometry)` — creates one `D3D11_USAGE_DEFAULT` `B8G8R8A8_UNORM` texture of the stream's size with a matching shader resource view, records `stream`, and drops whatever was there. Geometry never changes inside an encoder, so a new geometry is a new texture.
* `Renderer::upload(frame, damage)`:
  * `Err` when `stream` is `None` — a frame with no geometry is a frame this build will not guess at;
  * for each rectangle: clip to the stream's size, skip an empty result, and issue one `UpdateSubresource` with a `D3D11_BOX` and a source pointer offset to `rect.y * stride + rect.x * 4`, `row_pitch = stride`;
  * records `uploaded` for the test, and logs the count and the total bytes — never the bytes themselves.
* `Renderer::set_cursor(image)` — `video::premultiplied`, a 32-bit `CreateBitmap` colour bitmap plus a monochrome mask, `CreateIconIndirect` with `fIcon: false` and the hotspot, `SetClassLongPtrW(GCLP_HCURSOR)`, destroying the previous `HCURSOR`. Sizes are capped at `MAX_CURSOR_DIMENSION`, which the codec already enforces. `show_cursor(false)` clears the class cursor.
* `Renderer::present(progress, vm_name)`:
  * clears to a plain dark ground;
  * while `progress.is_running()`, draws the stream texture over the client area with a full-screen triangle pair (or `CopySubresourceRegion` into the back buffer when the sizes match — either is acceptable, the stretch is #120's to replace with letterboxing);
  * otherwise draws the overlay through Direct2D on a `ID2D1DeviceContext` over the back buffer's `IDXGISurface`: `progress.label()` centred, the VM's name beneath it, and in `Status::Failed(reason)` the reason plus the two rectangles from `status::buttons`;
  * `Present(1, 0)` — vsync-locked, so a static desktop costs one present and no GPU work;
  * maps `DXGI_ERROR_DEVICE_REMOVED` and `DXGI_ERROR_DEVICE_RESET` to an `Err` whose text names device loss, so the caller can call `recover`.
* `Renderer::resize_swapchain(width, height)` — releases the target view, `ResizeBuffers`, rebuilds the view.
* `Renderer::recover()` — `Ok(false)` once `losses` reaches `MAX_DEVICE_LOSSES`; otherwise increments, rebuilds device, swapchain, target and texture (from `stream`), and returns `Ok(true)`. The caller asks the guest for a keyframe: the old device held the only copy of what was on screen.
* `Renderer::stream_size()` and `Renderer::uploaded_rectangles()` for the tests.
* `impl Drop` destroying the cursor icon.

Every `unsafe` block carries a `// SAFETY:` comment. No pixel bytes are ever passed to `log`.

- [ ] **Step 8: Uncomment the declaration**

In `crates/display-viewer/src/windows/mod.rs`, uncomment `pub mod d3d;`.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS — six tests in `windows::d3d::tests`.

- [ ] **Step 10: Commit**

```bash
git add crates/display-viewer/src/windows crates/display-viewer/src/video.rs
git commit -m "TASK-117: Render the guest's desktop and its status overlay"
```

---

### Task 11: Composition, the distribution and the documentation

**Files:**
- Create: `crates/display-viewer/src/main.rs`
- Modify: `crates/xtask/src/main.rs:27-34` (the `ARTIFACTS` table)
- Modify: `ARCHITECTURE.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: everything from Tasks 1–10.
- Produces: `vmlord-display.exe`.

- [ ] **Step 1: Write the failing test**

Add to `crates/display-viewer/src/launch.rs`'s `mod tests`:

```rust
    #[test]
    fn a_viewer_started_without_launch_parameters_says_how_it_is_started() {
        // What a double-click from Explorer produces: no parent, no pipe, no
        // first message.
        let mut link = Link::new(io::empty(), io::sink());
        let outcome = super::first_parameters(&mut link);

        let message = outcome.expect_err("there are no parameters on an empty pipe");
        assert!(
            message.contains("VMLord"),
            "the message must name the only supported way to start this program"
        );
    }

    #[test]
    fn a_first_message_that_is_not_launch_parameters_is_refused() {
        let mut pipe = Vec::new();
        {
            let mut link = Link::new(io::empty(), &mut pipe);
            link.write(&Message::Command(Command::Focus))
                .expect("an in-memory writer");
        }

        let mut link = Link::new(pipe.as_slice(), io::sink());

        assert!(super::first_parameters(&mut link).is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: FAIL — `cannot find function `first_parameters``.

- [ ] **Step 3: Write the first-message check**

Add to `crates/display-viewer/src/launch.rs`, after `decode`:

```rust
/// Reads the one message a viewer must be started with.
///
/// A viewer launched with no usable standard input -- double-clicked from
/// Explorer, say -- has no VM to talk to and invents none. What comes back is
/// the message to put in the error window before exiting.
///
/// # Errors
///
/// The text to show the user, for a pipe with nothing on it or a first message
/// that is not [`Message::Launch`].
pub fn first_parameters<R: Read, W: Write>(
    link: &mut Link<R, W>,
) -> Result<LaunchParameters, String> {
    match link.read() {
        Ok(Message::Launch(parameters)) => Ok(parameters),
        Ok(other) => Err(format!(
            "VMLord Display was started with a {other:?} rather than its launch parameters. \
             It is opened from VMLord, through Connect on a VM's display."
        )),
        Err(error) => Err(format!(
            "VMLord Display cannot be started on its own ({error}). \
             It is opened from VMLord, through Connect on a VM's display."
        )),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS.

- [ ] **Step 5: Write the composition**

Create `crates/display-viewer/src/main.rs`:

```rust
//! The window VMLord opens on a VM's display.
//!
//! Nothing is decided here. The threads are chosen, wired to each other, and
//! the message pump is run; every rule the viewer follows lives in the library
//! beside this file.
//!
//! Three threads and what each owns:
//!
//! * the **main** thread owns the window, the renderer and the status machine,
//!   and never blocks -- which is what keeps the buttons on a `Failed` screen
//!   alive;
//! * the **pipe** thread owns standard input, turning launch messages into
//!   channel sends and posting `WM_SIGNAL` at the window;
//! * the **session** thread owns the three sockets, the protocol machine and
//!   the decoder, and posts `WM_SIGNAL` when it has something to draw.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("vmlord-display supports Windows only");

fn main() {
    // ... see the steps below
}
```

Then write `main` to do, in order:

1. `vmlord_display_viewer::log::initialize()`.
2. `let mut link = Link::new(io::stdin(), io::stdout());` and `launch::first_parameters(&mut link)`. On `Err(message)`: `MessageBoxW` with the text, `MB_ICONERROR | MB_OK`, title `VMLord Display`, then `ExitCode::FAILURE`. (`MessageBoxW` is one more `unsafe` call; put it in `windows::window` as `pub fn report(message: &str)` so this file stays safe.)
3. `SingleInstance::take(&parameters.runtime_id)`. On `Ok(None)`: `ipc::send_command(&parameters.runtime_id, Command::Focus)`, log that a viewer was already running, exit `SUCCESS`. That is the repeated-Connect case.
4. Open the window at `parameters.width` × `parameters.height`, title `"{vm_name} - VMLord Display"`, with a fresh `Shared`.
5. Start `CommandServer::start(&parameters.runtime_id, command_sender)`.
6. Split the link: `io::stdin()` goes to the pipe thread, which loops `Link::read` and sends each `Message` into `to_session`, posting `WM_SIGNAL`; a `LaunchError::Closed` sends a marker and ends the thread. Writes go through a `Mutex<Stdout>`-backed sender shared with the session thread.
7. Start the session thread with `parameters`, the two channels and a `Poster`. It runs this loop:
   * connect the control socket with `HvSocket::connect(&runtime_id, CONTROL_PORT, CONNECT_TIMEOUT)`, retrying on `Refused`, reporting `status::Event::PartitionGone` on `PartitionGone`;
   * on connect, report `Event::Connected` and run `Relay::run(&parameters.client_hello, deadline)`;
   * on a hand-over, build `Live::new(handover, control, connect, now)` where `connect` is a closure calling `HvSocket::connect` for the channel's port, and pump it, forwarding `Signal`s to the main thread;
   * on `Signal::Ended` or a control loss, send `Message::RequestRelay { token }` up the pipe, wait for the next `RelayToViewer` (which carries the fresh `ClientHello`) and start again; if the pipes are gone, report `Event::NoParent`;
   * a `Retry` from the main thread restarts the whole loop with a fresh budget.
8. Run the pump on the main thread: `while window.pump()` — for each iteration, drain the signal channel, apply signals to the `Progress` and to the `Renderer`, `progress.tick(Instant::now())`, set `shared.failed` from the status, handle `UiEvent`s (`Pressed(Retry)` → send `Retry` to the session thread; `Pressed(Cancel)` and `Closing` → begin shutdown; `Resized` → `renderer.resize_swapchain`), handle `Command::Focus` (`window.focus()`) and `Command::Close` (shutdown), then `renderer.present(&progress, &vm_name)`. A device-loss error calls `renderer.recover()`; `Ok(true)` asks the session thread for a keyframe, `Ok(false)` moves the status to `Failed`.
9. Shutdown: tell the session thread to `Live::end()` (best effort, half a second at most), join it, drop the `CommandServer`, drop the `SingleInstance`, return `ExitCode::SUCCESS`.

The `Status::Gone` state exits without a `Failed` screen: a stopped VM is not a failure.

- [ ] **Step 6: Collect the binary into the distribution**

In `crates/xtask/src/main.rs`, change `ARTIFACTS` to five entries:

```rust
/// What `dist` collects, as (target directory, file name) pairs.
const ARTIFACTS: [(&str, &str); 5] = [
    (APP_TARGET, "vmlord.exe"),
    (APP_TARGET, "vmlord-com1.exe"),
    // The display window, opened by VMLord for one VM at a time. It ships
    // beside `vmlord.exe` because that is where the launcher looks for it.
    (APP_TARGET, "vmlord-display.exe"),
    (APP_TARGET, "appsandbox_core.dll"),
    (AGENT_TARGET, "vmlord-agent"),
];
```

And add the build, beside the existing two in `dist`:

```rust
    cargo(
        &workspace,
        &[
            "build",
            "-p",
            "vmlord-display-viewer",
            "--release",
            "--target",
            APP_TARGET,
        ],
    )?;
```

- [ ] **Step 7: Record the viewer in the architecture**

Add a section to `ARCHITECTURE.md`, beside the guest display services, saying:

* `crates/display-viewer` produces `vmlord-display.exe`, one process per display session, launched by VMLord with two anonymous pipes as its standard input and output;
* the master secret stays in VMLord: it drives the control handshake through the viewer's relay and hands over two `ChannelKey`s, which are good for one session;
* the viewer owns the three HvSocket connections for the life of the session, so closing or crashing VMLord does not take the desktop down, and a crashed viewer does not touch the VM;
* one window per VM, held by a named mutex and reachable on a named pipe, so a repeated Connect focuses rather than duplicates;
* `unsafe` lives in `src/windows/{hvsocket, ipc, window, d3d}.rs` and nowhere else in the crate;
* what is deliberately not there yet: input (#119), letterbox, fullscreen and dynamic resolution (#120), and the Connect wiring and HCS service entries (#121).

- [ ] **Step 8: Record the command in the README**

In `README.md`'s command table, note that `cargo dist` now also builds `vmlord-display.exe`, and that the viewer is checked and tested with the same `cargo check-windows` and `cargo test-windows` as the rest of the Windows side.

- [ ] **Step 9: Verify the whole crate**

Run: `cargo check-windows`
Expected: no errors and no warnings.

Run: `cargo test-windows -p vmlord-display-viewer` and `cargo test -p vmlord-display-protocol`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/display-viewer crates/xtask/src/main.rs ARCHITECTURE.md README.md
git commit -m "TASK-117: Compose the display viewer and ship it with the release"
```

---

## What this plan does not cover

Stated so that it is not mistaken for done, and matching the spec's own boundary:

* a real Hyper-V partition, a real GDM greeter on the far end, 2560×1440 throughput and latency, multi-monitor hosts and GPU driver churn — those are #121's integration and #128's matrix;
* keyboard and mouse input (#119);
* letterbox, fullscreen, dynamic resolution and saved window state (#120);
* the HCS service table entries, the Connect path through UI → app → core → platform, launching the viewer and structured diagnostics (#121);
* audio, clipboard, multi-monitor and the Motion codec, which are not in v1 at all.
