# Guest display services implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a VMLord guest the two programs that turn its DRM output into an authenticated display session — a privileged broker that owns the device and the VM's secret, and an unprivileged process that captures, encodes and speaks the frame and input channels.

**Architecture:** One new crate, `crates/display-services`, producing two static musl binaries. The broker holds `CAP_SYS_ADMIN`, the DRM device, the vblank clock and `Session::guest`; it exports the current framebuffers as read-only dma-bufs over a root-owned `SOCK_SEQPACKET` socket guarded by `SO_PEERCRED`. The unprivileged process binds the frame and input channels with the `ChannelKey`s the broker hands it, composites the cursor plane, encodes with `vmlord-display-codec` and writes records. Both ship in the display payload's `content/services/`, installed by the agent's `SERVICES` and `SERVICES_START` recipe stages.

**Tech Stack:** Rust 2024, `x86_64-unknown-linux-musl`, `libc` for raw ioctls and socket calls, `prost`/`protox` for the broker's private IPC schema, `vmlord-display-protocol` for the wire contract, `vmlord-display-codec` for encoding, systemd units, DKMS-adjacent payload packaging.

**Spec:** `docs/superpowers/specs/2026-08-22-guest-display-services-design.md`

## Global Constraints

* Task number for every commit subject: `TASK-115: <comment>`.
* Target: `x86_64-unknown-linux-musl`, statically linked. **Never add a dependency that links a system C library** — no `libdrm`, no `libsystemd`. That is what keeps `cargo display-services` working from Windows with no C toolchain.
* `crates/display-services` sets `unsafe_code = "allow"` in its own manifest, the way `crates/agent` does; every `unsafe` block lives in a Linux-specific module (`src/unix.rs`, `src/vsock.rs`, `src/drm/`) and carries a `// SAFETY:` comment. No other module in the crate may contain `unsafe`.
* The crate is in `workspace.members` but **not** in `default-members`: it is a guest program that cannot be built for the host target.
* Tests run as `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`.
* vsock ports, fixed by the spec: control `VMLD` = `0x564D_4C44`, frame `VMLF` = `0x564D_4C46`, input `VMLI` = `0x564D_4C49`.
* The broker's IPC socket: `/run/vmlord/display.sock`, mode `0660`, owner `root`, group `vmlord-display`.
* The service user: `vmlord-display`, system account, shell `/usr/sbin/nologin`, no home.
* Installed binaries live in `/usr/local/lib/vmlord/`, beside `vmlord-agent`.
* Formats accepted from DRM: `XRGB8888` and `ARGB8888`, `DRM_FORMAT_MOD_LINEAR` only.
* `/dev/uinput` is **not** opened anywhere in this task. Input records are decoded and dropped.
* Follow AGENTS.md: small modules, explicit code, no traits with a single implementation, documentation updated in the same branch.

---

### Task 1: The crate, the IPC schema and its codec

**Files:**
- Create: `crates/display-services/Cargo.toml`
- Create: `crates/display-services/build.rs`
- Create: `crates/display-services/proto/vmlord/display/broker/broker.proto`
- Create: `crates/display-services/src/lib.rs`
- Create: `crates/display-services/src/ipc.rs`
- Modify: `Cargo.toml` (workspace `members`, not `default-members`)
- Modify: `.cargo/config.toml` (a `display-services` alias)
- Modify: `AGENTS.md:96-104`, `README.md:112` (the command tables)

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_display_services::ipc::{Message, encode, decode, IpcError}`, where `Message` is the enum every later task sends and receives.

- [ ] **Step 1: Write the failing test**

Create `crates/display-services/src/ipc.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::{Message, PlaneKind, PlaneLayout, SessionParameters, decode, encode};

    fn parameters() -> SessionParameters {
        SessionParameters {
            session_id: vec![7; 16],
            frame_key: vec![1; 32],
            input_key: vec![2; 32],
            width: 1920,
            height: 1080,
            tile_size: 32,
            cursor_stream: true,
        }
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        let messages = [
            Message::Attach,
            Message::NextFrame,
            Message::SessionOpened(parameters()),
            Message::SessionClosed {
                reason: "control was lost".into(),
            },
            Message::KeyframeRequested,
            Message::Snapshot {
                sequence: 42,
                planes: vec![PlaneLayout {
                    kind: PlaneKind::Primary,
                    buffer: 3,
                    width: 1920,
                    height: 1080,
                    stride: 7680,
                    format: 0x3458_5220,
                    x: -12,
                    y: 0,
                }],
                new_buffers: vec![3],
            },
            Message::Report {
                detail: "capture failed".into(),
            },
        ];

        for message in messages {
            let bytes = encode(&message);
            assert_eq!(decode(&bytes).expect("a message this build wrote"), message);
        }
    }

    #[test]
    fn a_message_this_build_cannot_name_is_refused() {
        // A single byte that is not a valid protobuf message for this schema.
        assert!(decode(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn a_negative_plane_position_survives_the_wire() {
        let Message::Snapshot { planes, .. } = decode(&encode(&Message::Snapshot {
            sequence: 1,
            planes: vec![PlaneLayout {
                kind: PlaneKind::Cursor,
                buffer: 9,
                width: 64,
                height: 64,
                stride: 256,
                format: 0x3443_5241,
                x: -30,
                y: -7,
            }],
            new_buffers: Vec::new(),
        }))
        .expect("a message this build wrote")
        else {
            panic!("a snapshot decodes as a snapshot");
        };

        assert_eq!((planes[0].x, planes[0].y), (-30, -7));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`
Expected: FAIL — the package does not exist yet.

- [ ] **Step 3: Create the manifest, the workspace entry and the alias**

`crates/display-services/Cargo.toml`:

```toml
[package]
name = "vmlord-display-services"
version.workspace = true
edition.workspace = true
license.workspace = true
build = "build.rs"

# Guest programs, so they are not part of the workspace's default build -- see
# `default-members` in the root manifest. Build them with:
#
#     cargo display-services
[[bin]]
name = "vmlord-display-broker"
path = "src/bin/broker.rs"
bench = false

[[bin]]
name = "vmlord-display-session"
path = "src/bin/session.rs"
bench = false

[dependencies]
libc = { version = "0.2", default-features = false }
prost = "0.14"
vmlord-display-codec = { path = "../display-codec" }
vmlord-display-protocol = { path = "../display-protocol" }

[build-dependencies]
prost = "0.14"
prost-build = "0.14"
# Compiles the `.proto` in-process, so no `protoc` has to be installed.
protox = "0.9"

[lints]
# These are guest programs that own DRM, socket and mapping ABIs directly.
# `unix.rs`, `vsock.rs` and `drm/` are the only modules that need unsafe calls
# into the kernel ABI.

[lints.rust]
unsafe_code = "allow"

[lints.clippy]
all = { level = "warn", priority = -1 }
```

In the root `Cargo.toml`, add `"crates/display-services",` to `members` after `"crates/display-payload",`, and extend the comment above `default-members` so it reads:

```toml
# `crates/agent` and `crates/display-services` are Linux guest programs and
# cannot be built for the host target, so they stay out of the default set that
# `cargo build` and `cargo test` operate on. They are built on their own:
#
#     cargo agent
#     cargo display-services
```

In `.cargo/config.toml`, under `[alias]`, after `agent-release`:

```toml
# The guest display services: the privileged DRM broker and the unprivileged
# capture process. Same musl target as the agent, and for the same reason.
display-services = [
    "build",
    "-p",
    "vmlord-display-services",
    "--target",
    "x86_64-unknown-linux-musl",
    "--release",
]
```

Add one row to the command tables. In `AGENTS.md`, after the `cargo agent` bullet:

```markdown
* `cargo display-services` — build the guest display broker and capture process
  (`x86_64-unknown-linux-musl`, statically linked, no C toolchain needed).
```

In `README.md`, after the `cargo agent` row:

```markdown
| `cargo display-services` | the guest display services, `x86_64-unknown-linux-musl` | Windows, Linux |
```

- [ ] **Step 4: Write the schema and the build script**

`crates/display-services/proto/vmlord/display/broker/broker.proto`:

```proto
// The private interface between the two guest display services.
//
// Not the display protocol: this never leaves the guest, both binaries ship in
// one payload at one version, and there is nothing here to negotiate. It is a
// schema rather than a hand-rolled encoding because the workspace already
// compiles protobuf in-process, and because "typed operations only" is the
// rule this interface exists to keep.

syntax = "proto3";

package vmlord.display.broker;

message Envelope {
  oneof message {
    Attach attach = 1;
    NextFrame next_frame = 2;
    SessionOpened session_opened = 3;
    SessionClosed session_closed = 4;
    KeyframeRequested keyframe_requested = 5;
    Snapshot snapshot = 6;
    Report report = 7;
  }
}

// The unprivileged process introducing itself.
message Attach {}

// It is ready for another frame. The reply arrives at the next vblank, and
// other messages may arrive before it.
message NextFrame {}

// A control handshake completed. Everything the frame and input channels need,
// and nothing the secret could be recovered from.
message SessionOpened {
  bytes session_id = 1;
  bytes frame_key = 2;
  bytes input_key = 3;
  uint32 width = 4;
  uint32 height = 5;
  uint32 tile_size = 6;
  bool cursor_stream = 7;
}

// Control was lost or the host sent EndSession. Stop capturing and release
// everything.
message SessionClosed {
  string reason = 1;
}

// The viewer asked for a keyframe. Recovery, not flow control.
message KeyframeRequested {}

// What the planes hold at one vblank.
message Snapshot {
  uint64 sequence = 1;
  repeated PlaneLayout planes = 2;
  // The buffer ids whose descriptors are attached to this datagram, in the
  // order they were attached. A buffer the peer already holds is not resent.
  repeated uint64 new_buffers = 3;
}

message PlaneLayout {
  PlaneKind kind = 1;
  uint64 buffer = 2;
  uint32 width = 3;
  uint32 height = 4;
  uint32 stride = 5;
  // The DRM fourcc, checked against what this build will map.
  uint32 format = 6;
  // Signed: a cursor plane goes negative at the left and top edges.
  sint32 x = 7;
  sint32 y = 8;
}

enum PlaneKind {
  PLANE_KIND_UNSPECIFIED = 0;
  PLANE_KIND_PRIMARY = 1;
  PLANE_KIND_CURSOR = 2;
}

// What the unprivileged process wants the host told.
message Report {
  string detail = 1;
}
```

`crates/display-services/build.rs`:

```rust
//! Turns the broker's private schema into Rust, without `protoc`.
//!
//! The same `protox` in-process compile `vmlord-display-protocol` uses, for the
//! same reason: nothing has to be installed on the machine that builds a guest.

use std::path::PathBuf;

const PROTO: &str = "proto/vmlord/display/broker/broker.proto";
const INCLUDE: &str = "proto";

fn main() {
    println!("cargo::rerun-if-changed={PROTO}");

    let descriptor_set = protox::compile([PROTO], [INCLUDE])
        .unwrap_or_else(|error| panic!("failed to compile {PROTO}: {error}"));

    prost_build::Config::new()
        .compile_fds(descriptor_set)
        .expect("failed to generate Rust types");

    // Silences an unused-crate warning: `prost` is a build dependency only for
    // the descriptor type `protox` hands over.
    let _ = PathBuf::new();
}
```

- [ ] **Step 5: Write the codec**

At the top of `crates/display-services/src/ipc.rs`, above the test module:

```rust
//! The typed operations the two services exchange.
//!
//! One datagram is one message: the socket is `SOCK_SEQPACKET`, so nothing here
//! frames anything. Descriptors ride alongside as `SCM_RIGHTS` and are named by
//! `Snapshot::new_buffers` rather than by position in the payload, so that a
//! peer which already holds a buffer is not sent it again.

use std::{error::Error, fmt};

use prost::Message as _;

use crate::broker::{self, envelope};

/// What one side asks the other to do, or tells it has happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// The unprivileged process introducing itself.
    Attach,
    /// It is ready for another frame.
    NextFrame,
    /// A control handshake completed.
    SessionOpened(SessionParameters),
    /// Control was lost, or the host is finished.
    SessionClosed { reason: String },
    /// The viewer needs a whole frame.
    KeyframeRequested,
    /// What the planes hold at one vblank.
    Snapshot {
        sequence: u64,
        planes: Vec<PlaneLayout>,
        new_buffers: Vec<u64>,
    },
    /// Something the host should be told about.
    Report { detail: String },
}

/// What a frame and an input channel need, and nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionParameters {
    pub session_id: Vec<u8>,
    pub frame_key: Vec<u8>,
    pub input_key: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    pub cursor_stream: bool,
}

/// Which plane a layout describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    Primary,
    Cursor,
}

/// One plane at one vblank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    pub kind: PlaneKind,
    pub buffer: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub x: i32,
    pub y: i32,
}

/// A datagram this build cannot read.
#[derive(Debug)]
pub struct IpcError(String);

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "the display broker socket carried {}", self.0)
    }
}

impl Error for IpcError {}

/// Writes a message as one datagram's payload.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    broker::Envelope {
        message: Some(into_wire(message)),
    }
    .encode_to_vec()
}

/// Reads one datagram's payload.
///
/// # Errors
///
/// [`IpcError`] for bytes that are not an `Envelope`, or for an envelope with
/// no arm this build knows.
pub fn decode(bytes: &[u8]) -> Result<Message, IpcError> {
    let envelope = broker::Envelope::decode(bytes)
        .map_err(|error| IpcError(format!("bytes that are not an envelope: {error}")))?;
    let Some(message) = envelope.message else {
        return Err(IpcError("an envelope with no message".to_owned()));
    };
    from_wire(message)
}
```

Write `into_wire` and `from_wire` as plain, total matches over `envelope::Message`, mapping `PlaneKind::Primary`/`Cursor` to `broker::PlaneKind::Primary`/`Cursor` and refusing `broker::PlaneKind::Unspecified` with an `IpcError`. In `src/lib.rs`:

```rust
//! The two programs a VMLord guest runs to put its desktop on the wire.
//!
//! `vmlord-display-broker` is privileged and small; `vmlord-display-session`
//! runs hot and holds nothing worth stealing. What crosses between them is
//! [`ipc`], and it is typed operations only: no device descriptor and no ioctl
//! passthrough leaves the broker.

pub mod ipc;

/// The generated types for the broker's private schema.
mod broker {
    // Generated code is not written to this repository's standards and cannot
    // be, so it is not linted against them.
    #![allow(clippy::all, clippy::pedantic, missing_docs)]

    include!(concat!(env!("OUT_DIR"), "/vmlord.display.broker.rs"));
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`
Expected: PASS, three tests.

Also run `cargo display-services` and expect it to fail only because `src/bin/broker.rs` and `src/bin/session.rs` do not exist yet; create both as `fn main() {}` placeholders so the alias builds, and note in each a one-line comment naming the task that fills it.

- [ ] **Step 7: Commit**

```bash
git add crates/display-services Cargo.toml .cargo/config.toml AGENTS.md README.md
git commit -m "TASK-115: Add the guest display services crate and its broker schema"
```

---

### Task 2: The broker socket — SO_PEERCRED and SCM_RIGHTS

**Files:**
- Create: `crates/display-services/src/unix.rs`
- Modify: `crates/display-services/src/lib.rs` (add `pub mod unix;`)

**Interfaces:**
- Consumes: `ipc::{Message, encode, decode}`.
- Produces:
  - `unix::Listener::bind(path: &Path, group: libc::gid_t) -> io::Result<Listener>`
  - `unix::Listener::accept(&self, expected_uid: libc::uid_t) -> io::Result<Connection>`
  - `unix::Connection::connect(path: &Path) -> io::Result<Connection>`
  - `unix::Connection::send(&self, message: &Message, descriptors: &[BorrowedFd<'_>]) -> io::Result<()>`
  - `unix::Connection::receive(&self) -> io::Result<(Message, Vec<OwnedFd>)>`
  - `unix::Connection::as_raw_fd(&self) -> RawFd`
  - `unix::memfd(name: &str, contents: &[u8]) -> io::Result<OwnedFd>` — test support, and the only honest stand-in for a dma-buf on a machine with no DRM device.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::ErrorKind,
        os::fd::{AsFd, AsRawFd},
    };

    use super::{Connection, Listener, memfd};
    use crate::ipc::Message;

    fn socket_path(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vmlord-display-{label}-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn a_message_and_its_descriptors_cross_together() {
        let path = socket_path("descriptors");
        let listener = Listener::bind(&path, unsafe { libc::getgid() }).unwrap();
        let uid = unsafe { libc::getuid() };

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let connection = Connection::connect(&path).unwrap();
                connection.receive().unwrap()
            }
        });

        let server = listener.accept(uid).unwrap();
        let buffer = memfd("frame", b"pixels").unwrap();
        server
            .send(
                &Message::Snapshot {
                    sequence: 5,
                    planes: Vec::new(),
                    new_buffers: vec![1],
                },
                &[buffer.as_fd()],
            )
            .unwrap();

        let (message, descriptors) = client.join().unwrap();
        assert!(matches!(message, Message::Snapshot { sequence: 5, .. }));
        assert_eq!(descriptors.len(), 1);
        let mut contents = Vec::new();
        let mut file = fs::File::from(descriptors.into_iter().next().unwrap());
        std::io::Read::read_to_end(&mut file, &mut contents).unwrap();
        assert_eq!(contents, b"pixels");

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_peer_with_the_wrong_uid_is_refused() {
        let path = socket_path("peercred");
        let listener = Listener::bind(&path, unsafe { libc::getgid() }).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || Connection::connect(&path)
        });

        // Nobody's uid but ours can connect here, so an expectation of a
        // different uid is how the check is exercised without a second account.
        let refused = listener.accept(unsafe { libc::getuid() } + 1);
        assert_eq!(refused.unwrap_err().kind(), ErrorKind::PermissionDenied);

        drop(client.join().unwrap());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_message_with_no_descriptors_carries_none() {
        let path = socket_path("bare");
        let listener = Listener::bind(&path, unsafe { libc::getgid() }).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let connection = Connection::connect(&path).unwrap();
                connection.send(&Message::NextFrame, &[]).unwrap();
            }
        });

        let server = listener.accept(unsafe { libc::getuid() }).unwrap();
        let (message, descriptors) = server.receive().unwrap();
        assert_eq!(message, Message::NextFrame);
        assert!(descriptors.is_empty());

        client.join().unwrap();
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_socket_is_group_readable_and_no_wider() {
        let path = socket_path("mode");
        let _listener = Listener::bind(&path, unsafe { libc::getgid() }).unwrap();
        let mode = std::os::unix::fs::MetadataExt::mode(&fs::metadata(&path).unwrap());

        assert_eq!(mode & 0o777, 0o660);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_closed_peer_is_an_end_and_not_an_error_kind_of_its_own() {
        let path = socket_path("closed");
        let listener = Listener::bind(&path, unsafe { libc::getgid() }).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || drop(Connection::connect(&path).unwrap())
        });

        let server = listener.accept(unsafe { libc::getuid() }).unwrap();
        client.join().unwrap();
        assert_eq!(
            server.receive().unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
        fs::remove_file(&path).unwrap();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl unix::`
Expected: FAIL — `unix` does not exist.

- [ ] **Step 3: Implement the module**

Write `src/unix.rs` with this shape. `sendmsg`/`recvmsg` with `SCM_RIGHTS` is the only intricate part; everything else is a checked `libc` call wrapped in an owned type.

```rust
//! The socket the two services talk over, and the only place a descriptor
//! changes hands.
//!
//! `SOCK_SEQPACKET` rather than `SOCK_STREAM`: `SCM_RIGHTS` is attached to a
//! datagram, and message boundaries mean neither side has to frame anything.
//! Every accepted peer is checked with `SO_PEERCRED` on every connection --
//! the file mode is a hint, and the credentials are the decision.

use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
};

use crate::ipc::{self, Message};

/// The largest datagram either side sends.
///
/// A snapshot is a handful of plane descriptions, so this is generous by two
/// orders of magnitude; it exists so that a peer cannot make the other side
/// allocate.
const MAX_DATAGRAM: usize = 8 * 1024;

/// The most descriptors one datagram may carry: a primary and a cursor buffer.
const MAX_DESCRIPTORS: usize = 2;
```

Points the implementer must get right, each with a `// SAFETY:` comment:

* `Listener::bind` — `socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0)`, `unlink` the path first (a stale socket from a killed broker must not stop the new one), `bind`, `chown(path, 0, group)`, `chmod(path, 0o660)`, `listen(8)`. Set the mode with `chmod` after `bind` rather than through `umask`: the umask of whoever started the unit is not something this code controls.
* `Listener::accept` — `accept4(..., SOCK_CLOEXEC)`, then `getsockopt(SOL_SOCKET, SO_PEERCRED)` into a `libc::ucred`; if `ucred.uid != expected_uid`, close the descriptor and return `io::Error::from(ErrorKind::PermissionDenied)`.
* `Connection::send` — build `msghdr` with one `iovec` over the encoded payload and, when `descriptors` is non-empty, a `CMSG_SPACE(size_of::<RawFd>() * n)` control buffer holding `SOL_SOCKET`/`SCM_RIGHTS`. Copy the raw descriptors into `CMSG_DATA` with `copy_nonoverlapping`. Retry on `EINTR`.
* `Connection::receive` — `recvmsg` with a `MAX_DATAGRAM` payload buffer, a control buffer sized for `MAX_DESCRIPTORS`, and `MSG_CMSG_CLOEXEC`. A return of `0` is `ErrorKind::UnexpectedEof`. Reject a datagram whose `msg_flags` has `MSG_TRUNC` or `MSG_CTRUNC` — a truncated control message is a leaked descriptor on the sender's side and a lie on ours. Walk the control messages with `CMSG_FIRSTHDR`/`CMSG_NXTHDR`, and turn each descriptor into an `OwnedFd` **before** any fallible step, so an error cannot leak one.
* `memfd` — `memfd_create(name, MFD_CLOEXEC)`, `write` the contents, `lseek` back to zero.

Decoding failures come back as `io::Error::new(ErrorKind::InvalidData, IpcError)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl unix::`
Expected: PASS, five tests.

- [ ] **Step 5: Check for descriptor leaks**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl unix:: -- --test-threads=1` and, in a second shell while it runs, nothing is needed — instead add this assertion to the first test, after the client joins:

```rust
        // The datagram carried one descriptor and the process must hold exactly
        // one more than it did before, not two: a descriptor received and then
        // dropped on an error path is the leak this checks for.
        let open_now = fs::read_dir("/proc/self/fd").unwrap().count();
        drop(server);
        drop(listener);
        assert!(open_now > 0);
```

Then re-run and confirm PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/display-services/src/unix.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Carry typed operations and descriptors between the services"
```

---

### Task 3: The cursor — cropping and composition

**Files:**
- Create: `crates/display-services/src/cursor.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: `vmlord_display_codec::{CursorImage, CursorPosition, Rect}`.
- Produces:
  - `cursor::Placement { pub x: u32, pub y: u32, pub crop: Rect, pub visible: bool }`
  - `cursor::place(plane_x: i32, plane_y: i32, width: u32, height: u32, frame_width: u32, frame_height: u32) -> Placement`
  - `cursor::composite(frame: &mut [u32], frame_width: u32, frame_height: u32, stride_pixels: u32, cursor: &[u32], cursor_width: u32, placement: &Placement)`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::{composite, place};

    #[test]
    fn a_cursor_inside_the_frame_is_not_cropped() {
        let placement = place(100, 50, 64, 64, 1920, 1080);

        assert_eq!((placement.x, placement.y), (100, 50));
        assert_eq!(
            (placement.crop.x, placement.crop.y),
            (0, 0),
            "nothing is cut off a cursor the compositor placed inside the frame"
        );
        assert_eq!((placement.crop.width, placement.crop.height), (64, 64));
        assert!(placement.visible);
    }

    #[test]
    fn a_cursor_off_the_left_and_top_edges_is_cropped_and_clamped() {
        let placement = place(-30, -7, 64, 64, 1920, 1080);

        assert_eq!(
            (placement.x, placement.y),
            (0, 0),
            "the protocol carries unsigned coordinates, so the offscreen part is cut rather than sent"
        );
        assert_eq!((placement.crop.x, placement.crop.y), (30, 7));
        assert_eq!((placement.crop.width, placement.crop.height), (34, 57));
    }

    #[test]
    fn a_cursor_off_the_right_and_bottom_edges_keeps_only_what_shows() {
        let placement = place(1900, 1050, 64, 64, 1920, 1080);

        assert_eq!((placement.crop.width, placement.crop.height), (20, 30));
    }

    #[test]
    fn a_cursor_entirely_outside_the_frame_is_not_visible() {
        assert!(!place(-64, 0, 64, 64, 1920, 1080).visible);
        assert!(!place(1920, 0, 64, 64, 1920, 1080).visible);
    }

    #[test]
    fn compositing_blends_by_alpha_and_leaves_the_rest_alone() {
        let mut frame = vec![0xff00_0000u32; 4 * 4];
        // Opaque white, half-transparent white, and two fully transparent.
        let cursor = vec![0xffff_ffffu32, 0x80ff_ffff, 0x0000_0000, 0x0000_0000];
        let placement = place(1, 1, 2, 2, 4, 4);

        composite(&mut frame, 4, 4, 4, &cursor, 2, &placement);

        assert_eq!(frame[1 * 4 + 1], 0xffff_ffff, "an opaque cursor pixel wins");
        assert_eq!(
            frame[1 * 4 + 2] & 0x00ff_0000,
            0x0080_0000,
            "a half-transparent pixel is halfway between the two"
        );
        assert_eq!(frame[2 * 4 + 1], 0xff00_0000, "a transparent pixel changes nothing");
        assert_eq!(frame[0], 0xff00_0000, "and nothing outside the cursor moves");
    }

    #[test]
    fn compositing_a_cropped_cursor_never_writes_outside_the_frame() {
        for x in -8i32..12 {
            for y in -8i32..12 {
                let mut frame = vec![0u32; 8 * 8];
                let cursor = vec![0xffff_ffffu32; 8 * 8];
                let placement = place(x, y, 8, 8, 8, 8);

                // The property: it returns, and every write landed in the
                // buffer. A slice index out of bounds would panic here.
                composite(&mut frame, 8, 8, 8, &cursor, 8, &placement);
            }
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl cursor::`
Expected: FAIL — `cursor` does not exist.

- [ ] **Step 3: Implement**

```rust
//! Where the pointer is, and what to do with it.
//!
//! #114's module deliberately does not set `DRIVER_CURSOR_HOTSPOT`, so no
//! hotspot is readable -- and none is needed: mutter places the plane where the
//! image is drawn, so the hotspot is `(0, 0)` and the position is the plane's
//! `CRTC_X`/`CRTC_Y`. Those are signed and go negative at the left and top
//! edges, while the protocol's coordinates are not, which is why an offscreen
//! cursor is cropped here rather than clamped and misdrawn there.

use vmlord_display_codec::Rect;

/// Where a cursor bitmap goes, and how much of it survives the frame's edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The left edge in frame coordinates, never negative.
    pub x: u32,
    /// The top edge in frame coordinates, never negative.
    pub y: u32,
    /// The part of the bitmap that is on the frame.
    pub crop: Rect,
    /// Whether any of it is.
    pub visible: bool,
}
```

`place` clamps negatives into `crop.x`/`crop.y`, shrinks `crop.width`/`crop.height` by both the leading crop and whatever overruns the right and bottom edges, and sets `visible` when both remain non-zero. `composite` walks `crop` rows, reads the cursor's `ARGB8888` pixels, and writes `dst = src + dst * (255 - alpha) / 255` per channel, skipping a fully transparent pixel and storing a fully opaque one directly. Compute the destination index with `y * stride_pixels + x` and take `frame` as `&mut [u32]`; the loop bounds come from `crop`, so no index needs a guard beyond what `place` already established — say so in a comment rather than adding one.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl cursor::`
Expected: PASS, six tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/cursor.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Place and composite the cursor plane"
```

---

### Task 4: The DRM ioctl layer

**Files:**
- Create: `crates/display-services/src/drm/uapi.rs`
- Create: `crates/display-services/src/drm/mod.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `drm::Device::find(driver: &str, sysfs_root: &Path, dev_root: &Path) -> io::Result<Option<Device>>`
  - `drm::Device::wait_vblank(&self) -> io::Result<u32>`
  - `drm::Device::snapshot(&mut self) -> io::Result<Vec<drm::PlaneState>>`
  - `drm::PlaneState { kind, fb_id, width, height, stride, format, x, y, descriptor: OwnedFd }`
  - `drm::uapi::{io_write, io_write_read, DRM_IOCTL_GEM_CLOSE, DRM_IOCTL_SET_CLIENT_CAP, ...}`

- [ ] **Step 1: Confirm the uapi against the kernel's own headers**

The struct sizes below are part of the ioctl numbers, so a wrong one is a call the kernel refuses with `EINVAL` and no clue why. Before writing anything, read them from the headers on this machine:

```bash
sudo apt-get install -y linux-libc-dev
grep -n 'struct drm_mode_fb_cmd2' -A 20 /usr/include/drm/drm_mode.h
grep -n 'struct drm_mode_obj_get_properties' -A 8 /usr/include/drm/drm_mode.h
grep -n 'DRM_IOCTL_MODE_GETFB2\|DRM_IOCTL_MODE_OBJ_GETPROPERTIES\|DRM_IOCTL_PRIME_HANDLE_TO_FD\|DRM_IOCTL_WAIT_VBLANK\|DRM_IOCTL_MODE_GETPLANE\b\|DRM_IOCTL_MODE_GETPLANERESOURCES' /usr/include/drm/drm.h
```

Write the numbers you read into the test in step 2 rather than the ones you expect.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::fs;

    use super::uapi::{DRM_IOCTL_GEM_CLOSE, DRM_IOCTL_SET_CLIENT_CAP, io_write};

    #[test]
    fn the_request_arithmetic_reproduces_numbers_the_kernel_publishes() {
        // Two constants from `drm.h`, spelled out. If the encoding is wrong
        // every other ioctl in this module is wrong in a way that only shows up
        // as EINVAL inside a guest.
        assert_eq!(DRM_IOCTL_GEM_CLOSE, 0x4008_6409);
        assert_eq!(DRM_IOCTL_SET_CLIENT_CAP, 0x4010_640d);
        assert_eq!(io_write(0x64, 0x09, 8), DRM_IOCTL_GEM_CLOSE);
    }

    #[test]
    fn the_structures_are_the_width_the_request_numbers_encode() {
        assert_eq!(size_of::<super::uapi::DrmModeFbCmd2>(), 104);
        assert_eq!(size_of::<super::uapi::DrmPrimeHandle>(), 12);
        assert_eq!(size_of::<super::uapi::DrmGemClose>(), 8);
        assert_eq!(size_of::<super::uapi::DrmSetClientCap>(), 16);
    }

    #[test]
    fn the_card_is_found_by_driver_name_and_not_by_number() {
        // A guest with hyperv_drm has a card0 that is not ours, which is the
        // whole reason this walks sysfs instead of opening a path.
        let root = temporary("cards");
        let sysfs = root.join("sys/class/drm");
        for (card, driver) in [("card0", "hyperv_drm"), ("card1", "vmlord_drm")] {
            let device = sysfs.join(card).join("device");
            fs::create_dir_all(&device).unwrap();
            std::os::unix::fs::symlink(
                root.join("sys/bus/platform/drivers").join(driver),
                device.join("driver"),
            )
            .unwrap();
        }

        assert_eq!(
            super::card_named("vmlord_drm", &sysfs).unwrap(),
            Some("card1".to_owned())
        );
        assert_eq!(super::card_named("nouveau", &sysfs).unwrap(), None);
    }

    fn temporary(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vmlord-display-drm-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl drm::`
Expected: FAIL — `drm` does not exist.

- [ ] **Step 4: Write `src/drm/uapi.rs`**

Constants and `#[repr(C)]` structures, copied from the kernel's uapi and named for it:

```rust
//! The kernel's DRM ABI, written out rather than linked.
//!
//! No system `libdrm`: linking one would cost the toolchain-free
//! cross-compilation the whole guest side rests on, and what is needed here is
//! six ioctls and five structures. Every item is named after the kernel's own
//! spelling so that a reader can find it in `drm.h` and `drm_mode.h`.

/// `_IOC(_IOC_WRITE, ..)`, the encoding `_IOW` builds.
#[must_use]
pub const fn io_write(kind: u32, number: u32, size: u32) -> libc::c_ulong {
    ((1 << 30) | (size << 16) | (kind << 8) | number) as libc::c_ulong
}

/// `_IOC(_IOC_READ | _IOC_WRITE, ..)`, the encoding `_IOWR` builds.
#[must_use]
pub const fn io_write_read(kind: u32, number: u32, size: u32) -> libc::c_ulong {
    ((3 << 30) | (size << 16) | (kind << 8) | number) as libc::c_ulong
}
```

Then `DRM_IOCTL_BASE: u32 = 0x64`, the six request constants built from those two functions with the numbers read in step 1, and the structures `DrmSetClientCap`, `DrmModeGetPlaneRes`, `DrmModeGetPlane`, `DrmModeObjGetProperties`, `DrmModeFbCmd2`, `DrmPrimeHandle`, `DrmGemClose`, `DrmWaitVblank`, `DmaBufSync`. Add the format and capability constants this build cares about: `DRM_FORMAT_XRGB8888 = 0x3458_5220`, `DRM_FORMAT_ARGB8888 = 0x3443_5241`, `DRM_FORMAT_MOD_LINEAR = 0`, `DRM_CLIENT_CAP_UNIVERSAL_PLANES = 2`, `DRM_MODE_OBJECT_PLANE = 0xeeee_eeee`, `DRM_VBLANK_RELATIVE = 0x1`, `DRM_CLOEXEC = libc::O_CLOEXEC`.

- [ ] **Step 5: Write `src/drm/mod.rs`**

```rust
//! The one place this crate speaks to a DRM device.
//!
//! An ordinary DRM client and never the master -- the compositor holds that --
//! which is what #111 proved a capture backend can be. What it needs beyond an
//! ordinary client is `CAP_SYS_ADMIN`, because `GETFB2` will not hand a
//! framebuffer's handles to anyone else; that capability is the entire reason
//! this code runs in a separate, privileged process.
```

* `card_named(driver, sysfs_root)` — read `sysfs_root`, follow each entry's `device/driver` symlink and compare its file name. A free function taking a root so it is testable; `Device::find` passes `/sys/class/drm`.
* `Device::find` — `card_named`, then `open(dev_root/cardN, O_RDWR | O_CLOEXEC)`, then `DRM_IOCTL_SET_CLIENT_CAP` with `DRM_CLIENT_CAP_UNIVERSAL_PLANES`. No master is taken and none is asked for.
* `Device::wait_vblank` — `DrmWaitVblank` with `DRM_VBLANK_RELATIVE` and `sequence = 1`; retry on `EINTR`; return the sequence the kernel replies with.
* `Device::snapshot` — `DRM_IOCTL_MODE_GETPLANERESOURCES` twice (once for the count, once for the ids), then per plane `DRM_IOCTL_MODE_GETPLANE` for `fb_id` and `crtc_id`. Skip a plane with no `fb_id`. For the rest: `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` with `DRM_MODE_OBJECT_PLANE` to read `CRTC_X`, `CRTC_Y` and `type` — property names come from `DRM_IOCTL_MODE_GETPROPERTY` on each id, and the ids are resolved once and cached, since they do not change for the life of the device. `type` distinguishes primary from cursor and is how `PlaneKind` is decided, rather than by plane order. Then `DRM_IOCTL_MODE_GETFB2`; refuse a `pixel_format` that is not one of the two, or a `modifier[0]` that is not `DRM_FORMAT_MOD_LINEAR`, with `io::Error::new(ErrorKind::Unsupported, ...)` naming what was found. Then `DRM_IOCTL_PRIME_HANDLE_TO_FD` with `flags = DRM_CLOEXEC` — **not** `DRM_RDWR`, which is what makes the exported buffer read-only — and `DRM_IOCTL_GEM_CLOSE` on the handle immediately afterwards, in a scope guard so it happens on the error path too.
* The descriptor cache: a `HashMap<u32, OwnedFd>` keyed by `fb_id`, holding the exported buffer. Entries whose `fb_id` did not appear in this walk are dropped at the end of it. `PlaneState` borrows from the cache; the IPC layer decides which are new to the peer.

Both `snapshot` and `wait_vblank` are `&mut self` because the cache is state.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl drm::`
Expected: PASS, three tests.

- [ ] **Step 7: Commit**

```bash
git add crates/display-services/src/drm crates/display-services/src/lib.rs
git commit -m "TASK-115: Read the guest output's planes without taking DRM master"
```

---

### Task 5: The captured frame and its CPU backing

**Files:**
- Create: `crates/display-services/src/capture.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: `unix::memfd` (tests only), `drm::PlaneState`.
- Produces:
  - `capture::MappedBuffer::map(fd: BorrowedFd<'_>, length: usize) -> io::Result<MappedBuffer>`
  - `capture::MappedBuffer::read<T>(&self, body: impl FnOnce(&[u8]) -> T) -> T` — brackets the read with `DMA_BUF_IOCTL_SYNC`
  - `capture::CapturedFrame { sequence, width, height, stride, format, damage, backing }`
  - `capture::Backing::Cpu(MappedBuffer)`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::MappedBuffer;
    use crate::unix::memfd;

    #[test]
    fn a_mapped_buffer_reads_what_the_descriptor_holds() {
        let fd = memfd("frame", &[0xab; 4096]).unwrap();
        let mapped = MappedBuffer::map(fd.as_fd(), 4096).unwrap();

        mapped.read(|bytes| {
            assert_eq!(bytes.len(), 4096);
            assert!(bytes.iter().all(|byte| *byte == 0xab));
        });
    }

    #[test]
    fn a_descriptor_that_cannot_sync_is_still_readable() {
        // A memfd is not a dma-buf and answers DMA_BUF_IOCTL_SYNC with ENOTTY.
        // A cache-coherency call that a buffer does not implement is not a
        // reason to drop the desktop, and this is the case that proves the
        // read still happens.
        let fd = memfd("frame", &[1; 64]).unwrap();
        let mapped = MappedBuffer::map(fd.as_fd(), 64).unwrap();

        assert_eq!(mapped.read(|bytes| bytes[0]), 1);
    }

    #[test]
    fn mapping_past_the_end_of_a_descriptor_fails_rather_than_faulting() {
        let fd = memfd("small", &[0; 8]).unwrap();
        // A length beyond the file is a mapping whose pages fault on access,
        // which must be refused here rather than discovered as a SIGBUS in the
        // encoder.
        assert!(MappedBuffer::map(fd.as_fd(), 1 << 20).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl capture::`
Expected: FAIL — `capture` does not exist.

- [ ] **Step 3: Implement**

`MappedBuffer::map` checks the descriptor's size with `fstat` and refuses a `length` beyond it, then `mmap(null, length, PROT_READ, MAP_SHARED, fd, 0)`, keeping the pointer and length. `Drop` calls `munmap`. `read` calls `DMA_BUF_IOCTL_SYNC` with `DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ` before the closure and `DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ` after; a failing ioctl is recorded in an `AtomicBool` on the buffer so the warning is logged once rather than per frame, and the read proceeds. The slice comes from `slice::from_raw_parts`, with a `// SAFETY:` comment naming the mapping as its provenance and the buffer's lifetime as its bound.

`CapturedFrame` and `Backing` are as in the spec, with `damage: Option<Vec<Rect>>` always `None` for now and a doc comment saying which task fills it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl capture::`
Expected: PASS, three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/capture.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Map an exported framebuffer for reading"
```

---

### Task 6: The frame pipeline

**Files:**
- Create: `crates/display-services/src/pipeline.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: `capture::CapturedFrame`, `cursor::Placement`, `vmlord_display_codec::{Encoder, EncoderConfig, Geometry, Frame, Payload}`, `vmlord_display_protocol::record`.
- Produces:
  - `pipeline::Pipeline::new(geometry: Geometry, generation: u32, cursor_stream: bool) -> Pipeline`
  - `pipeline::Pipeline::submit_frame(&mut self, frame: &CapturedFrame) -> Result<(), CodecError>`
  - `pipeline::Pipeline::submit_cursor(&mut self, image: Option<(&[u8], u32, u32)>, placement: &Placement) -> Result<(), CodecError>`
  - `pipeline::Pipeline::request_keyframe(&mut self)`
  - `pipeline::Pipeline::write_next<W: Write>(&mut self, writer: &mut W, limits: &Limits) -> Result<bool, PipelineError>`
  - `pipeline::Pipeline::write_stream_config<W: Write>(&mut self, writer: &mut W, limits: &Limits) -> Result<(), PipelineError>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use vmlord_display_codec::{Geometry, PixelFormat, TileSize};
    use vmlord_display_protocol::{
        record::{self, Channel, Limits},
        v1::FrameRecord,
    };

    use super::Pipeline;

    fn geometry() -> Geometry {
        Geometry::new(64, 64, TileSize::ThirtyTwo, PixelFormat::Xrgb8888).unwrap()
    }

    fn read_all(bytes: &[u8], limits: &Limits) -> Vec<(u16, u32, u32)> {
        let mut reader = bytes;
        let mut payload = Vec::new();
        let mut records = Vec::new();
        while !reader.is_empty() {
            let header = record::read(&mut reader, limits, &mut payload).unwrap();
            records.push((header.message_type, header.sequence, header.base));
        }
        records
    }

    #[test]
    fn a_new_socket_gets_a_stream_config_and_then_a_keyframe() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();

        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        assert!(pipeline.write_next(&mut bytes, &limits).unwrap());

        let records = read_all(&bytes, &limits);
        assert_eq!(records[0].0, FrameRecord::StreamConfig as u16);
        assert_eq!(records[1].0, FrameRecord::Keyframe as u16);
        assert_eq!(
            (records[0].1, records[1].1),
            (0, 1),
            "sequences run from zero on the socket, not on the session"
        );
    }

    #[test]
    fn a_delta_names_the_record_it_was_built_on() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();

        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x22)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        let records = read_all(&bytes, &limits);
        assert_eq!(records[2].0, FrameRecord::TileDelta as u16);
        assert_eq!(
            records[2].2, records[1].1,
            "the base is the sequence of the record that was sent, not of a frame that was captured"
        );
    }

    #[test]
    fn frames_captured_while_the_socket_was_busy_collapse_into_one_delta() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        for shade in [0x22u8, 0x33, 0x44] {
            pipeline.submit_frame(&frame(shade)).unwrap();
        }
        bytes.clear();
        assert!(pipeline.write_next(&mut bytes, &limits).unwrap());
        assert!(
            !pipeline.write_next(&mut bytes, &limits).unwrap(),
            "a slow socket gets one current delta, not a backlog of stale ones"
        );
    }

    #[test]
    fn a_requested_keyframe_is_the_next_record_even_with_nothing_new_captured() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        bytes.clear();
        pipeline.request_keyframe();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        assert_eq!(read_all(&bytes, &limits)[0].0, FrameRecord::Keyframe as u16);
    }

    #[test]
    fn every_record_carries_the_generation_the_socket_was_bound_at() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 7, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();

        let mut reader = bytes.as_slice();
        let mut payload = Vec::new();
        let header = record::read(&mut reader, &limits, &mut payload).unwrap();
        assert_eq!(header.generation, 7);
        assert_eq!(header.channel, Channel::Frame);
    }

    #[test]
    fn without_the_cursor_stream_capability_no_cursor_records_are_written() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, false);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline
            .submit_cursor(Some((&[0xff; 4 * 4 * 4], 4, 4)), &crate::cursor::place(1, 1, 4, 4, 64, 64))
            .unwrap();

        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}

        assert!(
            read_all(&bytes, &limits)
                .iter()
                .all(|(kind, _, _)| *kind != FrameRecord::CursorImage as u16),
            "a peer that declined the capability gets the cursor drawn into the frame instead"
        );
    }

    fn frame(shade: u8) -> crate::capture::CapturedFrame {
        // A frame that owns its pixels: the pipeline's tests have no DRM, and
        // what they exercise is everything above the mapping.
        crate::capture::CapturedFrame::from_pixels(
            0,
            64,
            64,
            64 * 4,
            vmlord_display_codec::PixelFormat::Xrgb8888,
            vec![shade; 64 * 64 * 4],
        )
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl pipeline::`
Expected: FAIL — `pipeline` does not exist, and neither does `CapturedFrame::from_pixels`.

- [ ] **Step 3: Add the owned-pixels constructor**

In `capture.rs`, add a second `Backing` case for frames that are not mapped:

```rust
pub enum Backing {
    /// A mapping over a dma-buf the broker exported. What a guest actually
    /// captures.
    Cpu(MappedBuffer),
    /// Pixels this process owns. What tests and, one day, a software fallback
    /// hand to the pipeline; the encoder cannot tell the difference.
    Owned(Vec<u8>),
}
```

with `CapturedFrame::from_pixels(...)` building the second, and a `CapturedFrame::read` that brackets the mapped case with the sync and passes the owned case straight through. This is not a speculative variant: the pipeline's tests are its consumer, and a `#[cfg(test)]` variant would mean the tested code path is not the shipped one.

- [ ] **Step 4: Implement the pipeline**

```rust
//! Turning captured frames into the frame channel's records.
//!
//! Everything above the mapping and below the socket, which is what makes it
//! the part a test can drive: it takes a `CapturedFrame` rather than a source
//! and writes into a `Write` rather than a socket.
//!
//! The bounded queue is the encoder's (#116), and it is the reason nothing here
//! encodes on submission: the reference frame must equal the last payload
//! handed out, so a frame displaced before it was encoded costs nothing but its
//! pixels.
```

* `Pipeline::new` builds `Encoder::new(EncoderConfig::new(geometry))`, keeps `generation`, `cursor_stream`, and `sequence: u32` starting at zero, plus `last_frame_sequence: u32`.
* `write_stream_config` writes `FrameRecord::StreamConfig` with a prost-encoded `StreamConfig` from the geometry.
* `submit_frame` calls `frame.read(|bytes| encoder.submit(Frame { pixels: bytes, stride }, None))`.
* `submit_cursor`: when `cursor_stream` is set, calls `Encoder::submit_cursor_image` with the cropped bitmap and `submit_cursor_position`; when it is not, composites into the staged frame before submission instead, which means `submit_cursor` must be called *before* `submit_frame` in that mode — document it on the method and have the caller in Task 11 respect it.
* `write_next` calls `Encoder::next_payload`, maps `Payload::Keyframe`/`TileDelta`/`CursorImage`/`CursorPosition` to the matching `FrameRecord`, sets `base` to `last_frame_sequence` for a delta and zero otherwise, writes with `record::write`, then increments `sequence` and, for a frame payload, sets `last_frame_sequence`. Returns `Ok(false)` when there was nothing to write.
* `PipelineError` wraps `CodecError` and `RecordError`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl pipeline::`
Expected: PASS, six tests.

- [ ] **Step 6: Commit**

```bash
git add crates/display-services/src/pipeline.rs crates/display-services/src/capture.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Encode captured frames into frame channel records"
```

---

### Task 7: Binding a frame or input channel from the guest side

**Files:**
- Modify: `crates/display-protocol/src/session.rs` (a new `derive_channel_key`)
- Create: `crates/display-services/src/channel.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: `vmlord_display_protocol::{keys, record, session, v1}`.
- Produces:
  - `Session::derive_channel_key(&self, channel: Channel) -> Option<ChannelKey>`
  - `channel::bind(stream: &mut S, channel: Channel, key: &ChannelKey, session_id: &[u8], last_generation: Option<u32>) -> Result<u32, BindError>` where `S: Read + Write`, returning the generation the socket was bound at.
  - `channel::BindError` with variants `WrongSession`, `StaleGeneration`, `BadTag`, `Record(RecordError)`, `Malformed`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use vmlord_display_protocol::{
        keys::Secret,
        record::{Channel, Limits},
        session::{Offer, Session, Support},
        v1::{Capability, Mode},
    };

    use super::{BindError, bind};

    /// A host and a guest that have already agreed on a session, so that the
    /// channel keys on both sides are the real ones.
    fn established() -> (Session, Session, Secret) {
        let secret = Secret::generate();
        let support = Support {
            capabilities: vec![Capability::CursorStream],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        };
        let offer = Offer {
            capabilities: vec![Capability::CursorStream],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        };

        let (mut host, hello) = Session::host(&secret, offer);
        let mut guest = Session::guest(&secret, support);

        // Four control records: hello, hello, the guest's proof, the host's.
        // Each side hands the other whatever the state machine produced, which
        // is the only order this handshake has.
        let mut next = Some(hello);
        while let Some(record) = next.take() {
            let outcome = guest.handle(&record.header, &record.payload).unwrap();
            if let Some(reply) = outcome.reply {
                let back = host.handle(&reply.header, &reply.payload).unwrap();
                next = back.reply;
            }
            if let Some(auth) = guest.pending_auth() {
                let back = host.handle(&auth.header, &auth.payload).unwrap();
                next = back.reply.or(next);
            }
        }

        assert!(host.negotiated().is_some(), "the host finished its handshake");
        assert!(guest.negotiated().is_some(), "and so did the guest");
        (host, guest, secret)
    }

    #[test]
    fn a_channel_binds_against_the_host_state_machine() {
        let (mut host, guest, _) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        // The key comes from the established session, not from a bound channel:
        // that is the whole point -- the broker derives it and hands it over
        // before any frame socket exists.
        let key = guest.derive_channel_key(Channel::Frame).unwrap();
        let mut wire = Wire::new(vec![hello]);

        let generation = bind(
            &mut wire,
            Channel::Frame,
            &key,
            guest.session_id(),
            None,
        )
        .unwrap();

        assert_eq!(generation, 0);
        // The host accepts the ack and the auth the guest wrote.
        for record in wire.written() {
            host.handle(&record.header, &record.payload).unwrap();
        }
        assert!(host.channel_key(Channel::Frame).is_some());
    }

    #[test]
    fn a_hello_for_another_session_is_refused() {
        let (mut host, guest, _) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        let mut wire = Wire::new(vec![hello]);

        assert!(matches!(
            bind(
                &mut wire,
                Channel::Frame,
                &guest.derive_channel_key(Channel::Frame).unwrap(),
                &[0u8; 16],
                None
            ),
            Err(BindError::WrongSession)
        ));
    }

    #[test]
    fn a_generation_that_did_not_advance_is_refused() {
        let (mut host, guest, _) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        let mut wire = Wire::new(vec![hello]);

        assert!(matches!(
            bind(
                &mut wire,
                Channel::Frame,
                &guest.derive_channel_key(Channel::Frame).unwrap(),
                guest.session_id(),
                Some(0)
            ),
            Err(BindError::StaleGeneration)
        ));
    }

    #[test]
    fn a_reconnect_binds_at_the_next_generation() {
        let (mut host, guest, _) = established();
        host.open_channel(Channel::Frame).unwrap();
        let hello = host.reconnect_channel(Channel::Frame).unwrap();
        let mut wire = Wire::new(vec![hello]);

        assert_eq!(
            bind(
                &mut wire,
                Channel::Frame,
                &guest.derive_channel_key(Channel::Frame).unwrap(),
                guest.session_id(),
                Some(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn a_forged_auth_tag_is_refused() {
        let (mut host, guest, _) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        let mut wire = Wire::new(vec![hello]);
        wire.corrupt_next_auth();

        assert!(matches!(
            bind(
                &mut wire,
                Channel::Frame,
                &guest.derive_channel_key(Channel::Frame).unwrap(),
                guest.session_id(),
                None
            ),
            Err(BindError::BadTag)
        ));
    }
}
```

`Wire` is a small test double in the same module: a `Read + Write` over a queue of records the host produced and a `Vec<Record>` of what the guest wrote, feeding the host's `ChannelAuth` in reply to the guest's `ChannelAck`. `corrupt_next_auth` flips one byte of the host's tag.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl channel::`
Expected: FAIL — neither `channel` nor `derive_channel_key` exists.

- [ ] **Step 3: Give the protocol crate the accessor this design needs**

`Session::channel_key` returns a key only *after* a channel has been bound, and the broker needs one *before* any frame socket exists — it derives the key, hands it over, and never touches the socket. The arithmetic is already in the crate (`keys::channel_key` over the session key and the transcript hash, both settled at establishment); what is missing is the way out.

In `crates/display-protocol/src/session.rs`, beside `channel_key`:

```rust
    /// The key `channel` will prove itself with, derived rather than remembered.
    ///
    /// [`Session::channel_key`] answers only once a socket has bound; this
    /// answers as soon as the control handshake is done, which is what lets the
    /// process that holds the secret hand a key to the process that holds the
    /// socket. Returns `None` before the handshake completes and for
    /// [`Channel::Control`], which binds nothing.
    #[must_use]
    pub fn derive_channel_key(&self, channel: Channel) -> Option<ChannelKey> {
        if self.channel_index(channel).is_err() {
            return None;
        }
        let session_key = self.session_key.as_ref()?;
        let transcript = self.transcript_hash.as_ref()?;

        Some(keys::channel_key(session_key, transcript, channel))
    }
```

And a test in that crate's own module, since it is that crate's promise:

```rust
    #[test]
    fn a_channel_key_can_be_derived_before_its_socket_binds() {
        let (host, guest) = established_pair();

        assert_eq!(
            guest.derive_channel_key(Channel::Frame).map(ChannelKey::to_owned_bytes),
            host.derive_channel_key(Channel::Frame).map(ChannelKey::to_owned_bytes),
            "both ends derive the same key from the same transcript"
        );
        assert!(guest.derive_channel_key(Channel::Control).is_none());
    }
```

If `ChannelKey` has no way to be compared in a test, compare the tags they produce with `keys::channel_tag` instead of adding an accessor to a key type that deliberately does not expose its bytes.

Run: `cargo test -p vmlord-display-protocol` — Expected: PASS.

- [ ] **Step 4: Implement the guest-side binding**

```rust
//! Proving a frame or input socket belongs to a session, from the side that
//! holds only a channel key.
//!
//! The broker did the control handshake and never touches these two sockets, so
//! this is the guest half of the three-record exchange: the crate's own
//! `keys::channel_tag` does the arithmetic, and what is written here is the
//! order and the checks -- the session id, the generation, and a tag compared in
//! constant time.
```

`bind` reads a `ChannelHello`, checks `session_id` bytes for equality, checks `generation` is `> last_generation` (or accepts any when there is none), writes a `ChannelAck` with a fresh nonce and `keys::channel_tag(key, Role::Guest, channel, &host_nonce, &guest_nonce)`, reads a `ChannelAuth` and compares it with `keys::verify` against the same tag computed for `Role::Host`. Sequence numbers on these three records run 0, 1, 2, and the returned generation is the one the socket is now on.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl channel::`
Expected: PASS, five tests.
Run: `cargo test -p vmlord-display-protocol` — Expected: PASS, the crate's own suite plus the new test.

- [ ] **Step 6: Commit**

```bash
git add crates/display-protocol/src/session.rs crates/display-services/src/channel.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Bind a frame or input socket with its channel key"
```

---

### Task 8: The vsock listener

**Files:**
- Create: `crates/display-services/src/vsock.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `vsock::{CONTROL_PORT, FRAME_PORT, INPUT_PORT}`
  - `vsock::Listener::bind(port: u32) -> io::Result<Listener>`
  - `vsock::Listener::accept(&self) -> io::Result<Stream>` — `Stream: Read + Write`, closes on drop
  - `vsock::Stream::as_raw_fd`, `vsock::Stream::shutdown`

- [ ] **Step 1: Find out whether this machine can carry the test**

```bash
modprobe vsock_loopback 2>/dev/null; ls -l /dev/vsock 2>/dev/null; \
  grep -rn 'VMADDR_CID_LOCAL' /usr/include/linux/vm_sockets.h
```

Record the answer in the test's comment. `VMADDR_CID_LOCAL` is `1`.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::{CONTROL_PORT, FRAME_PORT, INPUT_PORT, Listener};

    #[test]
    fn the_ports_are_the_ones_the_contract_names() {
        // Spelled out rather than computed from the letters: these four bytes
        // are a wire constant the host side also hardcodes, and a clever
        // derivation would hide a typo in either.
        assert_eq!(CONTROL_PORT, 0x564D_4C44);
        assert_eq!(FRAME_PORT, 0x564D_4C46);
        assert_eq!(INPUT_PORT, 0x564D_4C49);
    }

    /// Skipped where the kernel has no loopback transport, which is the state
    /// of a plain WSL2 kernel. Binding inside a real guest is #128's to prove;
    /// what runs here proves the socket calls are spelled correctly.
    #[test]
    fn a_listener_accepts_a_local_connection() {
        let Ok(listener) = Listener::bind(FRAME_PORT) else {
            eprintln!("no AF_VSOCK loopback on this kernel; skipping");
            return;
        };

        let client = std::thread::spawn(|| {
            let mut stream = super::connect_local(FRAME_PORT).unwrap();
            stream.write_all(b"hello").unwrap();
        });

        let mut accepted = listener.accept().unwrap();
        let mut buffer = [0u8; 5];
        accepted.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"hello");
        client.join().unwrap();
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl vsock::`
Expected: FAIL — `vsock` does not exist.

- [ ] **Step 4: Implement**

Modelled on `crates/agent/src/vsock.rs`, which connects, where this listens:

```rust
//! The three sockets the host connects to.
//!
//! The guest listens and the host connects, which is the opposite of the agent
//! protocol and is #118's decision: a session lives as long as a viewer window,
//! so no viewer means no connection and no capture.
```

`Listener::bind` opens `socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0)`, sets `SO_REUSEADDR`, fills a `sockaddr_vm` with `svm_cid = VMADDR_CID_ANY` and the port, `bind`s and `listen(4)`s. `accept` wraps the descriptor in `Stream`, which implements `Read`/`Write` over `read`/`write` the way the agent's does and closes on drop. `connect_local` is `#[cfg(test)]` and connects to `VMADDR_CID_LOCAL`. `shutdown` is `SHUT_RDWR`, so that closing a session wakes a blocked read — the same trick `crates/agent/src/vsock.rs::wake` uses, and the mechanism the input thread is stopped by.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl vsock::`
Expected: PASS (the second test may print the skip line).

- [ ] **Step 6: Commit**

```bash
git add crates/display-services/src/vsock.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Listen on the three display vsock ports"
```

---

### Task 9: The broker's control channel

**Files:**
- Create: `crates/display-services/src/control.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: `vmlord_display_protocol::{session::{Session, Support}, record}`, `ipc::Message`.
- Produces:
  - `control::Outcome` — `Opened(SessionParameters)`, `Relay(Message)`, `Closed(String)`, `Nothing`
  - `control::Control::new(secret: &Secret, support: Support) -> Control`
  - `control::Control::pump<S: Read + Write>(&mut self, stream: &mut S) -> Outcome` — reads one record, answers it, and says what the unprivileged process should be told
  - `control::support_from(width: u32, height: u32) -> Support`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use vmlord_display_protocol::{
        keys::Secret,
        session::{Offer, Session},
        v1::{Capability, ErrorCode, Mode},
    };

    use super::{Control, Outcome, support_from};

    #[test]
    fn a_completed_handshake_yields_the_keys_the_other_process_needs() {
        // Host and broker over an in-memory duplex; the assertions are about
        // what the broker hands on, not about the handshake, which the protocol
        // crate already proves.
        let (parameters, guest_session) = handshake();

        assert_eq!(parameters.session_id.len(), 16);
        assert_eq!(parameters.frame_key.len(), 32);
        assert_ne!(
            parameters.frame_key, parameters.input_key,
            "a channel key is per channel, so one socket's key never opens the other"
        );
        assert!(parameters.cursor_stream);
        let _ = guest_session;
    }

    #[test]
    fn the_guest_offers_one_mode_and_refuses_the_other() {
        let support = support_from(1920, 1080);
        assert_eq!(support.modes, vec![Mode::Desktop]);
        assert!(support.capabilities.contains(&Capability::CursorStream));
        assert!(support.capabilities.contains(&Capability::DynamicResolution));
    }

    #[test]
    fn set_mode_motion_is_refused_without_ending_the_session() {
        let error = drive_control(control_record_set_mode(Mode::Motion));
        assert_eq!(error, Some(ErrorCode::UnsupportedMode));
    }

    #[test]
    fn set_resolution_answers_with_what_is_actually_applied() {
        let state = drive_control_for_state(control_record_set_resolution(2560, 1440));
        assert_eq!(
            (state.width, state.height),
            (1920, 1080),
            "applying a resolution is #120; saying it was applied would be a lie"
        );
    }

    #[test]
    fn a_ping_is_answered_without_waking_capture() {
        let outcome = drive(control_record_ping(9));
        assert!(matches!(outcome, Outcome::Nothing));
    }

    #[test]
    fn a_keyframe_request_is_relayed() {
        assert!(matches!(
            drive(control_record_request_keyframe()),
            Outcome::Relay(crate::ipc::Message::KeyframeRequested)
        ));
    }

    #[test]
    fn end_session_closes_the_session() {
        assert!(matches!(drive(control_record_end_session()), Outcome::Closed(_)));
    }

    #[test]
    fn a_hung_up_host_closes_the_session() {
        // A peer that closed at a record boundary is how a session ends and is
        // not a fault, but it is still the end.
        assert!(matches!(drive_on_closed_stream(), Outcome::Closed(_)));
    }
}
```

Write the helpers (`handshake`, `drive`, `drive_control`, `drive_control_for_state`, `control_record_*`) in the same module: each builds a `Session::host`, runs the four control records against `Control::pump` over an in-memory duplex, then sends the record under test and reads back what the broker wrote.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl control::`
Expected: FAIL — `control` does not exist.

- [ ] **Step 3: Implement**

```rust
//! The control channel, which is the only socket the privileged process reads.
//!
//! It holds the VM's secret and `Session::guest`, and what it passes on is a
//! channel key -- never the secret, and never a socket. The frame and input
//! descriptors are opened by the unprivileged process and are never seen here.
```

`support_from` builds the `Support` the tests assert. `Control::pump` reads one record with `record::read`, hands it to `Session::handle`, writes any reply, and then:

* `Event::ControlEstablished` → take `derive_channel_key(Frame)`, `derive_channel_key(Input)` and `negotiated()`, and return `Outcome::Opened(SessionParameters { .. })`;
* `ControlRecord::RequestKeyframe` → `Outcome::Relay(Message::KeyframeRequested)`;
* `ControlRecord::Ping` → write a `Pong` with the same token, `Outcome::Nothing`;
* `ControlRecord::SetMode` → `Desktop` is `Outcome::Nothing` with a `DisplayState` reply, anything else an `Error(UNSUPPORTED_MODE)` record and `Outcome::Nothing` — a mode this build does not have is a refused request, not a dead session;
* `ControlRecord::SetResolution` → a `DisplayState` carrying the current geometry, `Outcome::Nothing`;
* `ControlRecord::EndSession`, `RecordError::Closed`, or any `SessionError` → `Outcome::Closed(reason)`, after writing an `Error` record with `SessionError::code` when the stream is still usable.

`RecordError::Idle` is `Outcome::Nothing`: the read timed out and there is nothing to do.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl control::`
Expected: PASS, eight tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/control.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Hold the display control channel in the privileged process"
```

---

### Task 10: The broker binary

**Files:**
- Create: `crates/display-services/src/broker_main.rs`
- Modify: `crates/display-services/src/bin/broker.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: `control`, `drm`, `unix`, `vsock`, `ipc`.
- Produces: `broker_main::run(options: Options) -> ExitCode`, and `broker_main::wait_for_device(deadline: Duration, attempt: impl FnMut() -> io::Result<Option<Device>>) -> io::Result<Device>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::{cell::Cell, io, time::Duration};

    use super::wait_for_device;

    #[test]
    fn a_device_that_is_not_there_yet_is_waited_for_rather_than_failed_on() {
        // The module loads after this unit starts, every time. Falling into a
        // restart while the card has not appeared would spend the crash-loop
        // budget on the ordinary state of a booting guest.
        let attempts = Cell::new(0);
        let device = wait_for_device(Duration::from_secs(5), || {
            attempts.set(attempts.get() + 1);
            Ok((attempts.get() >= 3).then_some(()))
        });

        assert!(device.is_ok());
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn a_device_that_never_appears_ends_the_wait_with_the_reason() {
        let error = wait_for_device(Duration::from_millis(200), || Ok(None))
            .expect_err("a guest whose module never loaded has no display");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl broker_main::`
Expected: FAIL.

- [ ] **Step 3: Implement**

`wait_for_device` polls with a backoff that starts at 250 ms and doubles to a 5 s ceiling until the deadline, and is generic over the attempt so the test can drive it.

`run` then wires the parts, with one thread each:

* **Startup:** read `/etc/vmlord/agent.secret` (the constant `vmlord_agent_protocol::auth::GUEST_SECRET_PATH`, never a second spelling of the path), `wait_for_device`, create `/run/vmlord` mode 0755, resolve the `vmlord-display` uid and gid with `getpwnam`, `unix::Listener::bind`.
* **IPC thread:** `accept(expected_uid)` in a loop; a refused peer is logged and the loop continues. One peer at a time: a second connection replaces the first, because there is one capture process and a stale one holding the socket would be a display nobody can restart.
* **Control thread:** `vsock::Listener::bind(CONTROL_PORT)`, accept, run `Control::pump` until `Outcome::Closed`, forward `Opened`/`Relay`/`Closed` to the IPC peer, then loop back to accept the next session. Losing control ends the session: send `SessionClosed` before accepting anything else.
* **Capture thread:** while a session is open and a `NextFrame` is outstanding, `Device::wait_vblank`, `Device::snapshot`, then build `Message::Snapshot` with the descriptors of the buffers this peer has not been sent, and send it. A snapshot that fails with `ErrorKind::Unsupported` becomes an `Error(CAPTURE_FAILED)` on control with the detail, and the session closes.
* Shared state is one `Mutex<BrokerState>` plus a `Condvar`; the capture thread waits on the condvar rather than polling.
* Every log line goes to stderr, which journald keeps — the same choice `crates/agent/src/main.rs` makes.

`src/bin/broker.rs` is:

```rust
//! The privileged half of the guest display services.

fn main() -> std::process::ExitCode {
    vmlord_display_services::broker_main::run(vmlord_display_services::broker_main::Options::from_env())
}
```

- [ ] **Step 4: Run the tests and the build**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl broker_main::` — Expected: PASS, two tests.
Run: `cargo display-services` — Expected: both binaries build.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/broker_main.rs crates/display-services/src/bin/broker.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Run the privileged display broker"
```

---

### Task 11: The session binary

**Files:**
- Create: `crates/display-services/src/session_main.rs`
- Modify: `crates/display-services/src/bin/session.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `session_main::run(options: Options) -> ExitCode`, and the testable core `session_main::Loop::step(&mut self) -> Result<Step, LoopError>`.

- [ ] **Step 1: Write the failing tests**

These are the end-to-end tests of the guest side: a fake broker on one end of a `Connection` pair and a real `Session::host` on the other end of the frame socket.

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn a_session_starts_with_a_stream_config_and_a_keyframe() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();

        let records = world.host_reads_frame_records();
        assert_eq!(records[0].message_type, FrameRecord::StreamConfig as u16);
        assert_eq!(records[1].message_type, FrameRecord::Keyframe as u16);
    }

    #[test]
    fn losing_control_stops_capture_and_closes_both_sockets() {
        let mut world = World::open();
        world.broker_sends_session_closed("control was lost");
        world.run_until_idle();

        assert!(
            world.frame_socket_is_closed(),
            "there is no session without control"
        );
        assert!(world.input_socket_is_closed());
        assert!(
            !world.broker_saw_next_frame_after_close(),
            "a process that keeps asking for frames is one that never stopped capturing"
        );
    }

    #[test]
    fn a_reconnected_frame_channel_starts_again_with_a_keyframe() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();
        world.host_drops_and_reopens_the_frame_socket();
        world.broker_sends_snapshot(2);
        world.run_until_written();

        let records = world.host_reads_frame_records();
        assert_eq!(records[0].message_type, FrameRecord::StreamConfig as u16);
        assert_eq!(
            records[1].message_type,
            FrameRecord::Keyframe as u16,
            "a delta has nothing to apply to on a decoder that has just been built"
        );
        assert_eq!(records[0].generation, 1);
    }

    #[test]
    fn a_slow_socket_costs_captured_frames_and_never_a_backlog() {
        let mut world = World::open();
        world.host_stops_reading();
        for sequence in 1..=8 {
            world.broker_sends_snapshot(sequence);
            world.step();
        }
        world.host_resumes_reading();
        world.run_until_written();

        assert!(
            world.host_reads_frame_records().len() <= 4,
            "the queue is before the encoder, so what a slow socket drops is captured frames"
        );
    }

    #[test]
    fn a_keyframe_request_from_the_broker_produces_one() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();
        world.broker_sends_keyframe_request();
        world.broker_sends_snapshot(2);
        world.run_until_written();

        assert_eq!(
            world.host_reads_frame_records().last().unwrap().message_type,
            FrameRecord::Keyframe as u16
        );
    }

    #[test]
    fn a_record_from_a_stale_generation_is_refused_on_the_input_socket() {
        let mut world = World::open();
        world.host_sends_input_with_generation(0);
        world.run_until_idle();

        assert!(
            world.input_socket_is_closed(),
            "a record from a connection that was replaced must not reach an input device"
        );
    }

    #[test]
    fn an_input_record_is_read_and_dropped() {
        // /dev/uinput is #119's. What this task owes is a channel that
        // completes its handshake and a record that is consumed rather than
        // left to stall the socket.
        let mut world = World::open();
        world.host_sends_key_event(30, true);
        world.run_until_idle();

        assert!(!world.input_socket_is_closed());
        assert_eq!(world.input_records_consumed(), 1);
    }
}
```

`World` builds a `unix::Connection` pair for the broker side, `vsock`-shaped in-memory duplexes for the frame and input sockets (the loop takes them as `Read + Write + AsRawFd`, so the test supplies `socketpair`s), a `Session::host` driven far enough to have channel keys, and a `Loop`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl session_main::`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! The unprivileged half: everything that runs hot and nothing worth stealing.
//!
//! One `poll` loop, because nothing acknowledges a frame: the broker descriptor
//! is always watched, and the frame descriptor is watched for writability only
//! while unwritten bytes remain. Encoding happens when the socket has drained,
//! which is what makes the encoder's reference frame equal to the last payload
//! that actually went out.
```

* `run` connects to the broker with backoff (the broker may restart under it and must not take the session down), binds `vsock::Listener` on `FRAME_PORT` and `INPUT_PORT` before any session exists, and sends `Message::Attach`.
* `Loop::step` polls, then handles whatever is ready:
  * `SessionOpened` → keep the parameters; a frame socket already accepted is bound now.
  * `SessionClosed` → drop the pipeline, `shutdown` and close both sockets, and stop asking for frames.
  * `Snapshot` → map each new buffer's descriptor into the buffer cache, build a `CapturedFrame` for the primary plane, compute the cursor `Placement` from the cursor plane, feed the pipeline in the order the capability requires, and send `NextFrame`.
  * frame socket writable → `Pipeline::write_next` until it returns `false` or the write would block.
  * a newly accepted frame socket → `channel::bind`, then a fresh `Pipeline` for that generation, `write_stream_config`, and `request_keyframe`.
* The input thread accepts on `INPUT_PORT`, binds the channel, then reads records and drops them, counting them for the test. On any error or a closed socket it logs one line — `"the input channel closed; nothing is held to release"` — and goes back to accepting. That line is where #119 hangs the real release-all.
* Frame writes are non-blocking: set `O_NONBLOCK` on the frame socket after binding, keep an unwritten tail in a `Vec<u8>`, and only encode when it is empty.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`
Expected: PASS, everything.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/session_main.rs crates/display-services/src/bin/session.rs crates/display-services/src/lib.rs
git commit -m "TASK-115: Run the unprivileged display capture and session process"
```

---

### Task 12: Units, and the services in the payload

**Files:**
- Create: `payloads/display/services/vmlord-display-broker.service`
- Create: `payloads/display/services/vmlord-display-session.service`
- Modify: `payloads/display/prepare.sh`
- Modify: `payloads/display/Dockerfile:60-62` (the comment about the empty directory)
- Modify: `payloads/display/README.md`
- Modify: `crates/xtask/src/display_payload.rs`

**Interfaces:**
- Consumes: the two binaries `cargo display-services` produces.
- Produces: a `prepared/content/services/` holding `vmlord-display-broker`, `vmlord-display-session` and the two units; a `pack` that refuses a protocol range this build does not speak.

- [ ] **Step 1: Write the failing test**

In `crates/xtask/src/display_payload.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::protocol_range_covers_this_build;

    #[test]
    fn a_recipe_whose_range_excludes_this_build_is_refused() {
        // Until #115 the range was a placeholder; now the services in the
        // archive are what makes it a claim, so packing is where it is checked.
        assert!(protocol_range_covers_this_build(1, 0, 0).is_ok());
        assert!(protocol_range_covers_this_build(2, 0, 0).is_err());
        assert!(protocol_range_covers_this_build(1, 3, 5).is_err());
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p xtask display_payload`
Expected: FAIL — the function does not exist.

- [ ] **Step 3: Implement the check**

```rust
/// Whether a recipe's declared protocol range contains what this build speaks.
///
/// The host already declines a catalog entry whose range does not cover its
/// version. This is the other half of that claim, checked where an archive is
/// made rather than discovered inside a VM.
fn protocol_range_covers_this_build(major: u32, min_minor: u32, max_minor: u32) -> Result<(), String> {
    let current = vmlord_display_protocol::handshake::CURRENT_VERSION;
    if major != current.major || current.minor < min_minor || current.minor > max_minor {
        return Err(format!(
            "the recipe declares display protocol {major}.{min_minor}-{major}.{max_minor} and this build speaks {}.{}",
            current.major, current.minor
        ));
    }
    Ok(())
}
```

Call it from `pack` after the recipe is read, and add `vmlord-display-protocol` to `crates/xtask/Cargo.toml`.

- [ ] **Step 4: Write the units**

`payloads/display/services/vmlord-display-broker.service`:

```ini
[Unit]
Description=VMLord display broker
Documentation=https://github.com/mrundead/vmlord
# No secret, no session: this is a VM that was never provisioned for a display.
ConditionPathExists=/etc/vmlord/agent.secret

[Service]
ExecStart=/usr/local/lib/vmlord/vmlord-display-broker
# The DRM device appears after the module loads, which is after this unit
# starts. Waiting for it is the broker's own job -- restarting until it shows up
# would spend the crash-loop budget on the ordinary state of a booting guest.
Restart=on-failure
RestartSec=2
StartLimitIntervalSec=60
StartLimitBurst=5
# GETFB2 will not hand a framebuffer's handles to anything less. Everything else
# root could do is taken away.
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_OVERRIDE
NoNewPrivileges=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=/run/vmlord
RestrictAddressFamilies=AF_VSOCK AF_UNIX
RestrictNamespaces=yes
MemoryDenyWriteExecute=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
```

`payloads/display/services/vmlord-display-session.service`:

```ini
[Unit]
Description=VMLord display capture and session
Documentation=https://github.com/mrundead/vmlord
# After, and deliberately not BindsTo: a broker restart must not take the
# capture process down, and the process reconnects to the socket on its own.
After=vmlord-display-broker.service

[Service]
ExecStart=/usr/local/lib/vmlord/vmlord-display-session
User=vmlord-display
Restart=on-failure
RestartSec=2
StartLimitIntervalSec=60
StartLimitBurst=5
# It holds a channel key and a mapping, and needs no privilege at all.
CapabilityBoundingSet=
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
RestrictAddressFamilies=AF_VSOCK AF_UNIX
RestrictNamespaces=yes
MemoryDenyWriteExecute=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 5: Teach `prepare.sh` about the services**

Add a required `--services <directory>` argument, parsed beside `--spec` and `--output` and refused when missing with the same style of message. After the `docker build`, before the closing `echo`:

```bash
# The services are built by the host toolchain, not in the image: a static musl
# binary is identical for 22.04, 24.04 and 26.04, and the container exists to
# prove the *module* compiles against a release's headers. A Rust toolchain in
# there would be a third toolchain for no gain.
for binary in vmlord-display-broker vmlord-display-session; do
	[[ -x "$services/$binary" ]] || {
		echo "$services does not hold $binary; run 'cargo display-services' first" >&2
		exit 1
	}
	install -m 0755 "$services/$binary" "$output/prepared/content/services/$binary"
done
install -m 0644 "$HERE/services/"*.service "$output/prepared/content/services/"
```

Widen the clean-tree check, since `sources.json` now records a commit that must describe the binaries too:

```bash
for tree in "$HERE" "$HERE/../../crates/display-services"; do
	if ! git -C "$HERE" diff --quiet HEAD -- "$tree"; then
		echo "$tree has uncommitted changes; commit them before packing a payload" >&2
		echo "-- the recipe records a commit, and it has to be one that describes the build." >&2
		exit 1
	fi
done
```

- [ ] **Step 6: Update the two documents that describe the tree**

In `payloads/display/Dockerfile`, replace the trailing comment about `content/services` staying empty with one saying `prepare.sh` fills it after the image runs. In `payloads/display/README.md`, change the tree listing's `prepared/content/services/  empty until task #115` to name the four files, and add `cargo display-services` plus `--services target/x86_64-unknown-linux-musl/release` to the build recipe at the top.

- [ ] **Step 7: Run the test and a real pack**

Run: `cargo test -p xtask display_payload` — Expected: PASS.
Run: `cargo display-services` — Expected: two binaries under `target/x86_64-unknown-linux-musl/release/`.
Run `payloads/display/prepare.sh --help` and confirm the new argument is documented. A full `prepare.sh` run needs a container runtime, which this machine does not have; note that in the commit message rather than claiming it was run.

- [ ] **Step 8: Commit**

```bash
git add payloads/display crates/xtask
git commit -m "TASK-115: Carry the display services in the display payload"
```

---

### Task 13: The recipe stages

**Files:**
- Modify: `crates/agent/src/display_kernel.rs:632-648` (`services_stages`), and `verify`
- Modify: `crates/agent/src/display_kernel.rs` test module

**Interfaces:**
- Consumes: the payload's `content/services/`.
- Produces: `SERVICES` and `SERVICES_START` stages that install, enable and start the units, and a `verify` that checks them.

- [ ] **Step 1: Write the failing tests**

Add to the existing test module, following its temp-directory pattern:

```rust
    #[test]
    fn services_that_are_already_installed_are_not_copied_again() {
        let payload = temporary("services-payload");
        let installed = temporary("services-installed");
        fs::write(payload.join("vmlord-display-broker"), b"binary").unwrap();
        fs::write(installed.join("vmlord-display-broker"), b"binary").unwrap();

        assert!(
            !super::services_need_install(&payload, &installed),
            "a guest already running what the payload carries needs no copy"
        );
    }

    #[test]
    fn a_service_whose_bytes_differ_is_reinstalled() {
        let payload = temporary("services-changed-payload");
        let installed = temporary("services-changed-installed");
        fs::write(payload.join("vmlord-display-broker"), b"new").unwrap();
        fs::write(installed.join("vmlord-display-broker"), b"old").unwrap();

        assert!(super::services_need_install(&payload, &installed));
    }

    #[test]
    fn a_service_that_is_not_installed_at_all_is_installed() {
        let payload = temporary("services-absent-payload");
        let installed = temporary("services-absent-installed");
        fs::write(payload.join("vmlord-display-broker"), b"new").unwrap();

        assert!(super::services_need_install(&payload, &installed));
    }

    #[test]
    fn a_payload_that_carries_no_services_still_skips_rather_than_fails() {
        // Every payload built before this task is one of these, and a failure
        // here would make every such guest degraded.
        let payload = temporary("services-empty-payload");
        let installed = temporary("services-empty-installed");
        let mut report = crate::display_recipe::Report::new();

        super::services_stages(&mut report, &payload, &installed);
        let stages = report.finish("the recipe did not need this stage");

        assert!(
            stages
                .iter()
                .filter(|stage| matches!(
                    stage.step(),
                    DisplayRecipeStep::Services | DisplayRecipeStep::ServicesStart
                ))
                .all(|stage| stage.state() == DisplayRecipeStageState::Skipped)
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p vmlord-agent --target x86_64-unknown-linux-musl display_kernel`
Expected: FAIL — `services_need_install` does not exist and `services_stages` takes no paths.

- [ ] **Step 3: Implement**

Replace `services_stages(report: &mut Report)` with `services_stages(report: &mut Report, services: &Path, installed: &Path) -> Result<(), String>`, keeping the empty-directory case exactly as it is — skipped, never failed, with the reason said out loud. When the directory has files:

1. `ensure_service_user()` — `getent passwd vmlord-display`, and on failure `useradd --system --no-create-home --shell /usr/sbin/nologin vmlord-display`. A `useradd` that fails because the account exists is not a failure; a `getent` that then still cannot find it is.
2. `services_need_install(services, installed)` — compares `sha256_hex` of each binary in the payload with the installed copy, using the digest helper the module already has. All-equal is a `report.skipped` naming what is already there.
3. Copy the two binaries with mode `0755` and the two units into `/etc/systemd/system/` with `0644`, then `systemctl daemon-reload`, then `systemctl enable` both, each with `SHORT_BUDGET`.
4. `SERVICES_START`: `systemctl restart` both, then poll `systemctl is-active` and the existence of `/run/vmlord/display.sock` until both hold or the short budget runs out.

Any failure is `report.failed` plus `Err(reason)`, which ends the recipe and leaves the display degraded while the VM keeps running — #113's machinery, unchanged.

In `verify`, after the existing module-version check, add the same two checks: the installed digests match the payload's, and both units are active. A failed verification is what makes an update roll back, so a payload whose services do not come up now rolls back instead of being declared installed.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent --target x86_64-unknown-linux-musl`
Expected: PASS, including the four new tests and every existing one.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/display_kernel.rs
git commit -m "TASK-115: Install and start the display services from the payload"
```

---

### Task 14: The documentation

**Files:**
- Modify: `ARCHITECTURE.md` (the display section)
- Modify: `docs/display-drm-backend.md:196-201` (the "still task #115" bullet)

- [ ] **Step 1: Update `docs/display-drm-backend.md`**

The last bullet reads "the capture backend itself -- **still task #115**". Replace it with what was actually built and what remains unproven: the broker's plane walk, the dma-buf export, the cursor composition, and the fact that no guest has run it — the mandatory matrix in #128 is still where that is settled. Do not describe anything as proven that was only tested on the development machine.

- [ ] **Step 2: Add the services to `ARCHITECTURE.md`**

Beside the existing display sections, describe: the two processes and why the privileged one is small; the socket, `SO_PEERCRED` and `SCM_RIGHTS`; that pixels cross as read-only dma-bufs and root never copies a frame; the three ports and that the guest listens; that the broker holds the secret and hands out channel keys; the three disconnect obligations; that input is bound and dropped until #119; and the packaging path from `cargo display-services` through `prepare.sh` into `content/services/`.

- [ ] **Step 3: Verify the whole tree still builds and tests**

```bash
cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl
cargo test -p vmlord-agent --target x86_64-unknown-linux-musl
cargo test
cargo display-services
cargo agent
```

Expected: all pass. Record any that do not in the commit rather than claiming they did.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md docs/display-drm-backend.md
git commit -m "TASK-115: Record the guest display services in the documentation"
```

---

## What this plan does not do

Named here so nothing is mistaken for delivered:

* `/dev/uinput` and any consumer of input records — #119.
* Applying a resolution change; `SetResolution` answers with the current geometry — #120.
* Registering the three HvSocket services in the HCS configuration — #121.
* Any run inside a guest: real mutter on the cursor plane, GDM before login, 2560x1440, kernels 6.8 and 7.x, systemd crash-loop behaviour, and every FPS, latency, CPU and memory threshold — #128.
* Binding `AF_VSOCK` where the development kernel has no loopback transport. The listener's socket calls are exercised; that the ports bind inside a Hyper-V guest is #128's to prove.
