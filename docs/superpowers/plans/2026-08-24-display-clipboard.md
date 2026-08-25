# Display clipboard implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a display session a bidirectional clipboard for text, HTML and
images, on a fourth authenticated channel that cannot delay a frame or a
keystroke.

**Architecture:** A fourth vsock channel (`VMLC`) joins control, frame and
input, keyed exactly as the other two bound channels are. In the guest a new
user-session daemon, `vmlord-display-clipboard`, drives GNOME's clipboard
through `org.gnome.Mutter.RemoteDesktop` and binds that channel itself with a
key the broker hands it over a second Unix socket, authorised against the uid
of the active graphical session. In the viewer a dedicated thread owns the
socket and the Win32 clipboard. Both ends share one portable state machine in
`display-protocol` that holds the allowlist, the caps and the cancellation
rules, so neither end can enforce something the other does not.

**Tech Stack:** Rust; prost/protobuf for the wire schema; `zbus` 5 (blocking
API, `async-io`, no C dependencies) for D-Bus in the guest; `png` 0.18 for
image conversion in the viewer; Win32 clipboard APIs through the `windows`
crate; `libc` for vsock, Unix sockets and `SO_PEERCRED`.

**Spec:** `docs/superpowers/specs/2026-08-24-display-clipboard-design.md`

## Global Constraints

- Guest binaries build for `x86_64-unknown-linux-musl` with no C toolchain.
  Never add a guest dependency that links a system C library (AGENTS.md).
  `zbus = { version = "5", default-features = false, features = ["async-io", "blocking-api"] }`
  was verified to satisfy this.
- Never reuse or renumber a Protobuf field or a record type number. Adding a
  message, field, enum value or `oneof` arm is a minor version bump.
- Every enum keeps a zero `*_UNSPECIFIED` value.
- No clipboard content in any log line, at any level, on either side. Log the
  mime type, the byte count, the transfer id and the outcome.
- Commit subjects are `TASK-125: <comment>`; every commit ends with
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Work happens on the branch `task-125-display-clipboard`, which exists. Do not
  open a merge request; the user asks for one explicitly.
- Build and test commands are the Cargo aliases: `cargo test -p <crate>` for
  portable crates, `cargo display-services` for the guest binaries,
  `cargo check-windows` and `cargo test-windows` for the Windows side. Never
  prefix a command with `timeout`.
- The limits, verbatim: record payload 64 KiB; one text or HTML transfer
  8 MiB; one image transfer 32 MiB; at most 16 offered mime types; one
  transfer in flight per direction; 5 s transfer inactivity.
- The allowlist, verbatim: `text/plain;charset=utf-8`, `text/html`,
  `image/bmp`, `image/png`. `text/uri-list` is never offered and an offer
  naming it is ignored. No opaque pass-through of registered formats.

---

## File Structure

| file | responsibility |
| --- | --- |
| `crates/display-protocol/proto/vmlord/display/v1/display.proto` | the four clipboard messages, the record enum, the capability |
| `crates/display-protocol/src/record.rs` | `Channel::Clipboard`, its payload cap |
| `crates/display-protocol/src/session.rs` | a third bound-channel slot, the clipboard key in a hand-over |
| `crates/display-protocol/src/clipboard.rs` | **new** — the allowlist, the caps and the transfer state machine, portable |
| `crates/display-services/proto/vmlord/display/broker/broker.proto` | `ClipboardOpened` |
| `crates/display-services/src/ipc.rs` | its typed form |
| `crates/display-services/src/seat.rs` | **new** — the uid of the active graphical session |
| `crates/display-services/src/unix.rs` | a listener a peer of unknown uid may reach, and an accept that takes a predicate |
| `crates/display-services/src/broker_main.rs` | the second socket and the thread that serves it |
| `crates/display-services/src/mutter.rs` | **new** — the D-Bus adapter: sessions, selections, descriptors |
| `crates/display-services/src/clipboard_main.rs` | **new** — the daemon: bind, poll, drive the state machine |
| `crates/display-services/src/bin/clipboard.rs` | **new** — its entry point |
| `payloads/display/services/vmlord-display-clipboard.service` | **new** — the user unit |
| `payloads/display/prepare.sh` | the third binary in the payload |
| `crates/agent/src/display_kernel.rs` | install and enable the user unit |
| `crates/platform/src/hvsocket.rs` | `VMLC`, a fourth service id |
| `crates/platform/src/display_session.rs` | the capability, the port, the clipboard key |
| `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto` | `clipboard_port`, `clipboard_key` |
| `crates/display-viewer/src/clipboard/win32.rs` | **new** — formats and conversions, unit-testable |
| `crates/display-viewer/src/clipboard/mod.rs` | **new** — the clipboard thread and its message-only window |
| `crates/display-viewer/src/main.rs` | starting that thread, and focus wiring |

---

### Task 1: The fourth channel in the record layer

**Files:**
- Modify: `crates/display-protocol/src/record.rs:22-50` (the `Channel` enum), `:216-270` (the caps and `Limits`)
- Modify: `crates/display-protocol/proto/vmlord/display/v1/display.proto`
- Test: `crates/display-protocol/src/record.rs` (its `mod tests`), `crates/display-protocol/tests/malformed.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Channel::Clipboard` (wire byte 4), `record::CLIPBOARD_MAX_PAYLOAD: u32`,
  `Limits::for_channel(Channel::Clipboard) == CLIPBOARD_MAX_PAYLOAD`,
  and in `v1`: `Capability::Clipboard`, `ClipboardRecord`, `ClipboardOffer`,
  `ClipboardRequest`, `ClipboardData`, `ClipboardCancel`, `CancelReason`.

- [ ] **Step 1: Write the failing test**

In `crates/display-protocol/src/record.rs`, inside `mod tests`:

```rust
#[test]
fn a_clipboard_channel_survives_a_round_trip() {
    let header = Header {
        channel: Channel::Clipboard,
        message_type: 7,
        length: 3,
        sequence: 9,
        base: 0,
        checksum: 0x1234_5678,
        generation: 2,
    };

    let (decoded, extra) = Header::decode(&header.encode()).expect("a header this build wrote");

    assert_eq!(decoded, header);
    assert_eq!(extra, 0);
    assert_eq!(Channel::Clipboard.as_wire(), 4);
    assert_eq!(Channel::Clipboard.to_string(), "clipboard");
}

#[test]
fn a_clipboard_record_is_capped_at_sixty_four_kibibytes() {
    let limits = Limits::new(1920, 1080);

    assert_eq!(limits.for_channel(Channel::Clipboard), CLIPBOARD_MAX_PAYLOAD);
    assert_eq!(CLIPBOARD_MAX_PAYLOAD, 64 * 1024);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-protocol record::tests::a_clipboard`
Expected: FAIL — `no variant named Clipboard found for enum Channel`.

- [ ] **Step 3: Write minimal implementation**

In `record.rs`, add the variant, its wire byte, its name and its cap:

```rust
pub enum Channel {
    /// Handshake, session control, liveness and errors.
    Control = 1,
    /// Frames and cursors, from the guest only.
    Frame = 2,
    /// Keyboard and pointer, from the host only.
    Input = 3,
    /// Selections, in both directions.
    Clipboard = 4,
}
```

`Channel::from_wire` gains `4 => Ok(Self::Clipboard)`, `Display` gains
`Self::Clipboard => "clipboard"`, and beside `INPUT_MAX_PAYLOAD`:

```rust
/// The most a clipboard record may carry.
///
/// A selection is chunked to fit rather than sized by the session: the cap is
/// what one record may hold, and `clipboard::MAX_TEXT_TRANSFER` and
/// `clipboard::MAX_IMAGE_TRANSFER` are what a whole transfer may.
pub const CLIPBOARD_MAX_PAYLOAD: u32 = 64 * 1024;
```

`Limits::for_channel` gains `Channel::Clipboard => CLIPBOARD_MAX_PAYLOAD`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p vmlord-display-protocol`
Expected: PASS. The `malformed.rs` test that asserts an unknown channel byte is
refused must still pass; if it used 4 as its unknown byte, change it to 5.

- [ ] **Step 5: Add the schema**

In `display.proto`, extend `Capability`, add the record enum and the four
messages. Append only — nothing is renumbered:

```protobuf
enum Capability {
  CAPABILITY_UNSPECIFIED = 0;
  CAPABILITY_CURSOR_STREAM = 1;
  CAPABILITY_DYNAMIC_RESOLUTION = 2;
  // The session carries selections on a fourth channel. Announced by a guest
  // whose payload ships the clipboard daemon, whether or not one is attached:
  // a session commonly opens at the login screen, where no user session and
  // therefore no daemon exists yet, and a capability cannot be renegotiated.
  CAPABILITY_CLIPBOARD = 3;
}

// The `type` field of a record header on the clipboard channel.
enum ClipboardRecord {
  CLIPBOARD_RECORD_UNSPECIFIED = 0;
  CLIPBOARD_RECORD_CHANNEL_HELLO = 1;
  CLIPBOARD_RECORD_CHANNEL_ACK = 2;
  CLIPBOARD_RECORD_CHANNEL_AUTH = 3;
  CLIPBOARD_RECORD_OFFER = 4;
  CLIPBOARD_RECORD_REQUEST = 5;
  CLIPBOARD_RECORD_DATA = 6;
  CLIPBOARD_RECORD_CANCEL = 7;
  CLIPBOARD_RECORD_ERROR = 8;
}

// Why a transfer stopped before its last chunk.
enum CancelReason {
  CANCEL_REASON_UNSPECIFIED = 0;
  // A newer offer replaced the selection this transfer was carrying.
  CANCEL_REASON_SUPERSEDED = 1;
  // The transfer passed the cap for its kind.
  CANCEL_REASON_TOO_LARGE = 2;
  // The viewer's window lost focus.
  CANCEL_REASON_FOCUS_LOST = 3;
  // The side that offered it can no longer produce it.
  CANCEL_REASON_UNAVAILABLE = 4;
  // Nothing moved for five seconds.
  CANCEL_REASON_TIMED_OUT = 5;
}

// My selection changed; here is what I can produce for it.
message ClipboardOffer {
  // Names this selection. A request that names an older one is refused rather
  // than answered with the wrong contents.
  uint32 serial = 1;
  // At most sixteen, and only what the allowlist names.
  repeated string mime_types = 2;
}

// Send me one of them, as this transfer.
message ClipboardRequest {
  uint32 serial = 1;
  string mime_type = 2;
  uint32 transfer = 3;
}

// A chunk of it, in order. `last` ends the transfer.
message ClipboardData {
  uint32 transfer = 1;
  bytes chunk = 2;
  bool last = 3;
}

// That transfer is over and its bytes are not coming.
message ClipboardCancel {
  uint32 transfer = 1;
  CancelReason reason = 2;
}
```

- [ ] **Step 6: Regenerate the checked-in descriptor set and verify**

Run: `cargo test -p vmlord-display-protocol`
The build script compiles the schema; `tests/descriptor.rs` compares the
checked-in `proto/display.descriptor.bin` with the compiled one. If it fails,
regenerate the file the way that test's failure message says, then rerun.
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-125: Add the clipboard channel to the wire contract

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: A third bound channel in the session machine

**Files:**
- Modify: `crates/display-protocol/src/session.rs:130-180` (the struct and `HandedOver`), `:237-310` (the three constructors), `:380-410` (`handle`), `:850-860` (`channel_index`)
- Test: `crates/display-protocol/src/session.rs` (its `mod tests`)

**Interfaces:**
- Consumes: `Channel::Clipboard` from Task 1.
- Produces: `HandedOver { session_id, negotiated, frame_key, input_key, clipboard_key, control_sequence }` — note the new third key;
  `Session::open_channel(Channel::Clipboard)`, `Session::reconnect_channel(Channel::Clipboard)`,
  `Session::derive_channel_key(Channel::Clipboard)`, `Session::take_channel_sequence(Channel::Clipboard)`
  all work as they do for frame and input.

- [ ] **Step 1: Write the failing test**

In `session.rs`, inside `mod tests`, beside the existing channel tests:

```rust
#[test]
fn a_clipboard_channel_binds_like_any_other() {
    let (mut host, mut guest) = established_pair();

    let hello = host
        .open_channel(Channel::Clipboard)
        .expect("a clipboard hello");
    let ack = guest
        .handle(&hello.header, &hello.payload)
        .expect("the guest answers a clipboard hello")
        .reply
        .expect("an ack");
    let auth = host
        .handle(&ack.header, &ack.payload)
        .expect("the host answers an ack")
        .reply
        .expect("an auth");
    let outcome = guest
        .handle(&auth.header, &auth.payload)
        .expect("the guest checks the host's proof");

    assert_eq!(outcome.event, Event::ChannelBound(Channel::Clipboard));
}

#[test]
fn a_clipboard_key_is_not_a_frame_key() {
    let (host, _) = established_pair();

    let clipboard = host
        .derive_channel_key(Channel::Clipboard)
        .expect("a clipboard key");
    let frame = host.derive_channel_key(Channel::Frame).expect("a frame key");

    assert_ne!(clipboard.to_bytes().as_slice(), frame.to_bytes().as_slice());
}
```

`established_pair()` is the existing helper that runs the four control records;
reuse it under whatever name it already has in that module rather than adding
another.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-protocol session::tests::a_clipboard`
Expected: FAIL — `channel_index` returns `SessionError::Unexpected` for
`Channel::Clipboard`, so `open_channel` errors.

- [ ] **Step 3: Write minimal implementation**

In `session.rs`:

```rust
fn channel_index(&self, channel: Channel) -> Result<usize, SessionError> {
    match channel {
        Channel::Frame => Ok(0),
        Channel::Input => Ok(1),
        Channel::Clipboard => Ok(2),
        Channel::Control => Err(SessionError::Unexpected {
            channel,
            message_type: 0,
        }),
    }
}
```

Both arrays grow: `channels: [ChannelState; 3]` and
`handover_keys: [Option<ChannelKey>; 3]`. Every constructor gains the third
element — `ChannelState::default()` in the first two, and in
`established_host`:

```rust
handover_keys: [
    Some(handed_over.frame_key),
    Some(handed_over.input_key),
    Some(handed_over.clipboard_key),
],
```

`HandedOver` gains, with a doc comment in the file's voice:

```rust
    /// The key the clipboard socket proves itself with.
    pub clipboard_key: ChannelKey,
```

In `handle`, the arm that accepts a channel hello widens:

```rust
(State::Established, Channel::Frame | Channel::Input | Channel::Clipboard, message_type)
    if message_type == FrameRecord::ChannelHello as u16 =>
{
    self.on_channel_hello(header.channel, payload)
}
```

`FrameRecord::ChannelHello`, `ChannelAck` and `ChannelAuth` are 1, 2 and 3, and
so are `ClipboardRecord`'s: the bind exchange is the same three numbers on
every bound channel, which is why this arm reads the frame enum for all of
them. Leave that as it is and say so in a comment.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vmlord-display-protocol`
Expected: PASS, including the existing golden vectors — nothing in this task
changes bytes on the wire for the three older channels.

- [ ] **Step 5: Commit**

```bash
git add crates/display-protocol/src/session.rs
git commit -m "TASK-125: Bind a clipboard channel like frame and input

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: The allowlist and the transfer state machine

This is the piece both ends share. It has no D-Bus, no Win32, no socket and no
clock of its own — every method takes `now` — so all of it is testable here.

**Files:**
- Create: `crates/display-protocol/src/clipboard.rs`
- Modify: `crates/display-protocol/src/lib.rs` (add `pub mod clipboard;`)
- Test: `crates/display-protocol/src/clipboard.rs` (its `mod tests`)

**Interfaces:**
- Consumes: `Channel::Clipboard`, the four messages from Task 1.
- Produces:

```rust
pub enum Kind { Text, Html, Bmp, Png }
impl Kind {
    pub fn from_mime(mime: &str) -> Option<Self>;
    pub fn mime(self) -> &'static str;
    pub fn cap(self) -> usize;
}
pub const MAX_MIME_TYPES: usize;
pub const MAX_TEXT_TRANSFER: usize;
pub const MAX_IMAGE_TRANSFER: usize;
pub const CHUNK: usize;
pub const IDLE: Duration;

pub struct Piece { pub kind: Kind, pub bytes: Vec<u8> }

pub enum Op {
    Send(Message),
    Produce { kind: Kind, transfer: u32 },
    Apply { pieces: Vec<Piece> },
}

pub enum Message {
    Offer { serial: u32, mime_types: Vec<&'static str> },
    Request { serial: u32, mime_type: &'static str, transfer: u32 },
    Data { transfer: u32, chunk: Vec<u8>, last: bool },
    Cancel { transfer: u32, reason: CancelReason },
}

pub struct Exchange;
impl Exchange {
    pub fn new() -> Self;
    pub fn local_offer(&mut self, kinds: &[Kind], now: Instant) -> Vec<Op>;
    pub fn produced(&mut self, transfer: u32, bytes: Vec<u8>, now: Instant) -> Vec<Op>;
    pub fn unavailable(&mut self, transfer: u32) -> Vec<Op>;
    pub fn peer_offer(&mut self, serial: u32, mime_types: &[String], now: Instant) -> Vec<Op>;
    pub fn peer_request(&mut self, serial: u32, mime_type: &str, transfer: u32, now: Instant) -> Vec<Op>;
    pub fn peer_data(&mut self, transfer: u32, chunk: &[u8], last: bool, now: Instant) -> Vec<Op>;
    pub fn peer_cancel(&mut self, transfer: u32, reason: CancelReason) -> Vec<Op>;
    pub fn focus_lost(&mut self, now: Instant) -> Vec<Op>;
    pub fn tick(&mut self, now: Instant) -> Vec<Op>;
}
```

- [ ] **Step 1: Write the failing tests for the allowlist**

Create `crates/display-protocol/src/clipboard.rs` with the module documentation
and this test module only, so the first run fails on the types:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_names_four_kinds_and_nothing_else() {
        assert_eq!(Kind::from_mime("text/plain;charset=utf-8"), Some(Kind::Text));
        assert_eq!(Kind::from_mime("text/html"), Some(Kind::Html));
        assert_eq!(Kind::from_mime("image/bmp"), Some(Kind::Bmp));
        assert_eq!(Kind::from_mime("image/png"), Some(Kind::Png));

        // Files are refused by policy, not by omission: task #139 owns them.
        assert_eq!(Kind::from_mime("text/uri-list"), None);
        assert_eq!(Kind::from_mime("text/plain"), None);
        assert_eq!(Kind::from_mime("application/x-anything"), None);
    }

    #[test]
    fn text_and_images_have_their_own_caps() {
        assert_eq!(Kind::Text.cap(), 8 * 1024 * 1024);
        assert_eq!(Kind::Html.cap(), 8 * 1024 * 1024);
        assert_eq!(Kind::Bmp.cap(), 32 * 1024 * 1024);
        assert_eq!(Kind::Png.cap(), 32 * 1024 * 1024);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vmlord-display-protocol clipboard::`
Expected: FAIL — `cannot find type Kind in this scope` (after adding
`pub mod clipboard;` to `lib.rs`, without which it fails to compile at all).

- [ ] **Step 3: Implement the allowlist**

```rust
/// What one side may put on the other's clipboard.
///
/// An allowlist rather than a pass-through: AppSandbox forwards any registered
/// Windows format by name, and that is an unbounded channel between a guest and
/// its host. Four kinds cover what people actually copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// UTF-8 text.
    Text,
    /// HTML, which Windows carries inside a CF_HTML envelope.
    Html,
    /// A BMP, which is a DIB with a file header in front of it.
    Bmp,
    /// A PNG, which many GTK applications offer and no Windows format holds.
    Png,
}

impl Kind {
    /// The kind a mime type names, if the allowlist has one.
    #[must_use]
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            TEXT_MIME => Some(Self::Text),
            HTML_MIME => Some(Self::Html),
            BMP_MIME => Some(Self::Bmp),
            PNG_MIME => Some(Self::Png),
            _ => None,
        }
    }

    /// What this kind is called on the wire.
    #[must_use]
    pub fn mime(self) -> &'static str { /* the four constants */ }

    /// The most one transfer of this kind may carry.
    #[must_use]
    pub fn cap(self) -> usize {
        match self {
            Self::Text | Self::Html => MAX_TEXT_TRANSFER,
            Self::Bmp | Self::Png => MAX_IMAGE_TRANSFER,
        }
    }
}
```

with the constants above it:

```rust
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
pub const HTML_MIME: &str = "text/html";
pub const BMP_MIME: &str = "image/bmp";
pub const PNG_MIME: &str = "image/png";

/// The most mime types an offer may name.
pub const MAX_MIME_TYPES: usize = 16;
/// The most one text or HTML transfer may carry.
pub const MAX_TEXT_TRANSFER: usize = 8 * 1024 * 1024;
/// The most one image transfer may carry.
pub const MAX_IMAGE_TRANSFER: usize = 32 * 1024 * 1024;
/// How much of a transfer one record carries. Below `CLIPBOARD_MAX_PAYLOAD`
/// with room for the message's own fields.
pub const CHUNK: usize = 60 * 1024;
/// How long a transfer may make no progress before it is cancelled.
pub const IDLE: Duration = Duration::from_secs(5);
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vmlord-display-protocol clipboard::`
Expected: PASS.

- [ ] **Step 5: Write the failing tests for one transfer in each direction**

```rust
    fn t0() -> Instant { Instant::now() }

    #[test]
    fn a_local_offer_is_announced_and_served_on_request() {
        let mut exchange = Exchange::new();
        let now = t0();

        let ops = exchange.local_offer(&[Kind::Text, Kind::Html], now);
        let Op::Send(Message::Offer { serial, mime_types }) = &ops[0] else {
            panic!("an offer is announced");
        };
        assert_eq!(mime_types, &vec![TEXT_MIME, HTML_MIME]);
        let serial = *serial;

        let ops = exchange.peer_request(serial, TEXT_MIME, 1, now);
        assert!(matches!(
            ops.as_slice(),
            [Op::Produce { kind: Kind::Text, transfer: 1 }]
        ));

        let ops = exchange.produced(1, b"hello".to_vec(), now);
        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Data { transfer: 1, last: true, .. })]
        ));
    }

    #[test]
    fn a_request_against_a_superseded_offer_is_refused() {
        let mut exchange = Exchange::new();
        let now = t0();

        let first = match &exchange.local_offer(&[Kind::Text], now)[0] {
            Op::Send(Message::Offer { serial, .. }) => *serial,
            _ => panic!("an offer"),
        };
        let _ = exchange.local_offer(&[Kind::Text], now);

        let ops = exchange.peer_request(first, TEXT_MIME, 7, now);

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel { transfer: 7, reason: CancelReason::Superseded })]
        ));
    }

    #[test]
    fn a_peer_offer_is_pulled_and_applied_once_every_kind_has_arrived() {
        let mut exchange = Exchange::new();
        let now = t0();

        let ops = exchange.peer_offer(
            4,
            &[TEXT_MIME.to_owned(), HTML_MIME.to_owned()],
            now,
        );
        let Op::Send(Message::Request { mime_type, transfer, .. }) = &ops[0] else {
            panic!("the first kind is requested");
        };
        assert_eq!(*mime_type, TEXT_MIME);
        let first = *transfer;

        // One transfer in flight: the second kind is not requested yet.
        assert_eq!(ops.len(), 1);

        let ops = exchange.peer_data(first, b"plain", true, now);
        let Op::Send(Message::Request { mime_type, transfer, .. }) = &ops[0] else {
            panic!("the second kind follows the first");
        };
        assert_eq!(*mime_type, HTML_MIME);
        let second = *transfer;

        let ops = exchange.peer_data(second, b"<i>rich</i>", true, now);
        let Op::Apply { pieces } = &ops[0] else {
            panic!("both kinds are applied together");
        };
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].kind, Kind::Text);
        assert_eq!(pieces[0].bytes, b"plain");
        assert_eq!(pieces[1].kind, Kind::Html);
    }

    #[test]
    fn only_one_image_kind_is_pulled_and_bmp_wins() {
        let mut exchange = Exchange::new();

        let ops = exchange.peer_offer(
            1,
            &[PNG_MIME.to_owned(), BMP_MIME.to_owned()],
            t0(),
        );

        let Op::Send(Message::Request { mime_type, .. }) = &ops[0] else {
            panic!("an image is requested");
        };
        assert_eq!(*mime_type, BMP_MIME);
    }

    #[test]
    fn an_offer_of_nothing_allowed_is_dropped_without_a_word() {
        let mut exchange = Exchange::new();

        let ops = exchange.peer_offer(
            1,
            &["text/uri-list".to_owned(), "application/x-lotus".to_owned()],
            t0(),
        );

        assert!(ops.is_empty());
    }
```

- [ ] **Step 6: Run to verify they fail, then implement the machine**

Run: `cargo test -p vmlord-display-protocol clipboard::`
Expected: FAIL — `Exchange` does not exist.

Implement it as two halves inside one type. The outgoing half holds the serial
of the local selection and the transfer it is serving; the incoming half holds
the peer's serial, the queue of kinds still to pull, the transfer in flight,
the bytes so far and the pieces already complete. Both halves record the
`Instant` of their last progress, which is what `tick` measures.

Rules the tests above pin, and which the implementation must not widen:

* `local_offer` bumps the serial, cancels an outgoing transfer that was running
  with `Superseded`, and emits one `Offer` naming the kinds in the order
  `Text, Html, Bmp, Png`.
* `peer_request` answers with `Produce` only when the serial is the current one
  and the mime is in the allowlist; otherwise `Cancel` with `Superseded` or
  `Unavailable`.
* `produced` refuses a body past `kind.cap()` with `Cancel { TooLarge }`,
  otherwise splits it into `CHUNK`-sized `Data` messages, the final one with
  `last: true`. A body of zero bytes is one empty `Data` with `last: true`.
* `peer_offer` selects the allowed kinds, keeping `Text`, `Html` and exactly one
  image — `Bmp` if offered, else `Png` — cancels an incoming transfer that was
  running, and requests the first kind. Offers naming more than
  `MAX_MIME_TYPES` types are truncated to the first `MAX_MIME_TYPES` before
  selection.
* `peer_data` appends, cancels with `TooLarge` if the accumulation passes the
  cap, and on `last` either requests the next kind or emits one `Apply` with
  every piece in the order they were pulled.
* `peer_cancel` drops whichever side the transfer belongs to and, for an
  incoming one, abandons the whole offer rather than pulling the next kind: a
  peer that cannot produce one format of a selection has a selection this side
  should not half-apply.
* `focus_lost` cancels both directions with `FocusLost` and forgets the peer's
  offer, so the next focus starts clean.
* `tick` cancels a transfer whose last progress is older than `IDLE` with
  `TimedOut`.
* Transfer ids come from a counter that never repeats within a session, so a
  late `Data` from a cancelled transfer is ignored rather than misapplied.

- [ ] **Step 7: Run to verify they pass**

Run: `cargo test -p vmlord-display-protocol clipboard::`
Expected: PASS.

- [ ] **Step 8: Write the failing tests for the caps, cancellation and idleness**

```rust
    #[test]
    fn a_body_past_its_cap_is_cancelled_rather_than_sent() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = match &exchange.local_offer(&[Kind::Text], now)[0] {
            Op::Send(Message::Offer { serial, .. }) => *serial,
            _ => panic!("an offer"),
        };
        let _ = exchange.peer_request(serial, TEXT_MIME, 3, now);

        let ops = exchange.produced(3, vec![b'x'; MAX_TEXT_TRANSFER + 1], now);

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel { transfer: 3, reason: CancelReason::TooLarge })]
        ));
    }

    #[test]
    fn an_arriving_body_past_its_cap_is_cancelled_mid_stream() {
        let mut exchange = Exchange::new();
        let now = t0();
        let transfer = match &exchange.peer_offer(1, &[TEXT_MIME.to_owned()], now)[0] {
            Op::Send(Message::Request { transfer, .. }) => *transfer,
            _ => panic!("a request"),
        };

        let mut ops = Vec::new();
        for _ in 0..=(MAX_TEXT_TRANSFER / CHUNK) + 1 {
            ops = exchange.peer_data(transfer, &vec![b'x'; CHUNK], false, now);
            if !ops.is_empty() {
                break;
            }
        }

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel { reason: CancelReason::TooLarge, .. })]
        ));
    }

    #[test]
    fn losing_focus_cancels_both_directions() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = match &exchange.local_offer(&[Kind::Text], now)[0] {
            Op::Send(Message::Offer { serial, .. }) => *serial,
            _ => panic!("an offer"),
        };
        let _ = exchange.peer_request(serial, TEXT_MIME, 5, now);
        let _ = exchange.peer_offer(9, &[TEXT_MIME.to_owned()], now);

        let ops = exchange.focus_lost(now);

        let cancels = ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    Op::Send(Message::Cancel { reason: CancelReason::FocusLost, .. })
                )
            })
            .count();
        assert_eq!(cancels, 2);
    }

    #[test]
    fn a_transfer_that_stops_moving_times_out() {
        let mut exchange = Exchange::new();
        let now = t0();
        let _ = exchange.peer_offer(1, &[TEXT_MIME.to_owned()], now);

        let ops = exchange.tick(now + IDLE + Duration::from_millis(1));

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel { reason: CancelReason::TimedOut, .. })]
        ));
    }

    #[test]
    fn a_chunked_body_arrives_in_order_and_whole() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = match &exchange.local_offer(&[Kind::Bmp], now)[0] {
            Op::Send(Message::Offer { serial, .. }) => *serial,
            _ => panic!("an offer"),
        };
        let _ = exchange.peer_request(serial, BMP_MIME, 2, now);
        let body: Vec<u8> = (0..CHUNK * 2 + 17).map(|index| index as u8).collect();

        let ops = exchange.produced(2, body.clone(), now);

        let mut rebuilt = Vec::new();
        let mut ended = false;
        for op in &ops {
            let Op::Send(Message::Data { chunk, last, .. }) = op else {
                panic!("only data");
            };
            assert!(chunk.len() <= CHUNK);
            rebuilt.extend_from_slice(chunk);
            ended = *last;
        }
        assert!(ended);
        assert_eq!(rebuilt, body);
    }
```

- [ ] **Step 9: Run, implement what is missing, run again**

Run: `cargo test -p vmlord-display-protocol clipboard::`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/display-protocol/src/clipboard.rs crates/display-protocol/src/lib.rs
git commit -m "TASK-125: Add the clipboard transfer state machine

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: The broker's second socket, and who may take it

**Files:**
- Create: `crates/display-services/src/seat.rs`
- Modify: `crates/display-services/src/unix.rs:41-120`, `crates/display-services/src/lib.rs`
- Modify: `crates/display-services/proto/vmlord/display/broker/broker.proto`
- Modify: `crates/display-services/src/ipc.rs`
- Modify: `crates/display-services/src/broker_main.rs:198-350`
- Test: the `mod tests` in `seat.rs`, `unix.rs` and `ipc.rs`

**Interfaces:**
- Consumes: Task 2's `Session::derive_channel_key(Channel::Clipboard)`.
- Produces: `seat::active_graphical_uid() -> Option<libc::uid_t>`;
  `unix::Listener::bind_open(path: &Path) -> io::Result<Listener>`;
  `unix::Listener::accept_where(&self, allow: impl Fn(libc::uid_t) -> bool) -> io::Result<Connection>`;
  `ipc::Message::ClipboardOpened { session_id: Vec<u8>, clipboard_key: Vec<u8> }`;
  the broker's `--clipboard-socket` option, default `/run/vmlord/display-clipboard.sock`.

- [ ] **Step 1: Write the failing test for the seat lookup**

Create `crates/display-services/src/seat.rs` with this test module:

```rust
#[cfg(test)]
mod tests {
    use super::uid_of_active_graphical_session;

    const WAYLAND: &str = "UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=1\nSTATE=active\n";
    const TTY: &str = "UID=1000\nSEAT=seat0\nTYPE=tty\nACTIVE=1\nSTATE=active\n";
    const INACTIVE: &str = "UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=0\nSTATE=online\n";
    const REMOTE: &str = "UID=1001\nTYPE=wayland\nACTIVE=1\nSTATE=active\n";

    #[test]
    fn an_active_graphical_session_on_the_seat_names_its_uid() {
        assert_eq!(uid_of_active_graphical_session(WAYLAND), Some(1000));
        assert_eq!(
            uid_of_active_graphical_session("UID=1000\nSEAT=seat0\nTYPE=x11\nACTIVE=1\n"),
            Some(1000)
        );
    }

    #[test]
    fn nothing_else_does() {
        // A console login has no clipboard, an inactive session is not the one
        // on screen, and a session with no seat is not at the screen at all.
        assert_eq!(uid_of_active_graphical_session(TTY), None);
        assert_eq!(uid_of_active_graphical_session(INACTIVE), None);
        assert_eq!(uid_of_active_graphical_session(REMOTE), None);
        assert_eq!(uid_of_active_graphical_session(""), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vmlord-display-services seat::`
Expected: FAIL — the module is not declared and the function does not exist.

- [ ] **Step 3: Implement the seat lookup**

`seat.rs`, declared in `lib.rs` as `pub mod seat;`:

```rust
//! Which user is at the virtual screen.
//!
//! The clipboard socket cannot be owned by a group the way the session socket
//! is: the daemon on the other end runs as whichever human logged in, and that
//! name is not known when the VM is provisioned. What is known at the moment of
//! an accept is who logind says is at `seat0`, so that is what the socket is
//! authorised against.
//!
//! Read out of `/run/systemd/sessions` rather than asked of logind over D-Bus:
//! the broker is a small privileged process that talks to no bus, and these
//! files are `KEY=value` lines it can read without one. A guest whose files
//! cannot be read has no clipboard, which is the safe end of the failure.
```

```rust
/// The uid of the active graphical session on `seat0`, if there is one.
#[must_use]
pub fn active_graphical_uid() -> Option<libc::uid_t> {
    let entries = std::fs::read_dir("/run/systemd/sessions").ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(uid) = uid_of_active_graphical_session(&text) {
            return Some(uid);
        }
    }

    None
}

/// The parsing half, which a test can reach without a running logind.
fn uid_of_active_graphical_session(text: &str) -> Option<libc::uid_t> {
    let mut uid = None;
    let (mut seat, mut graphical, mut active) = (false, false, false);

    for line in text.lines() {
        match line.split_once('=') {
            Some(("UID", value)) => uid = value.trim().parse().ok(),
            Some(("SEAT", value)) => seat = value.trim() == "seat0",
            Some(("TYPE", value)) => graphical = matches!(value.trim(), "wayland" | "x11"),
            Some(("ACTIVE", value)) => active = value.trim() == "1",
            _ => {}
        }
    }

    (seat && graphical && active).then_some(uid?)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vmlord-display-services seat::`
Expected: PASS.

- [ ] **Step 5: Write the failing test for the open listener**

In `unix.rs`'s `mod tests`, beside the existing listener tests:

```rust
    #[test]
    fn an_open_listener_accepts_the_uid_a_predicate_allows() {
        let path = temporary_socket_path();
        let listener = Listener::bind_open(&path).unwrap();
        let mine = own_uid();

        let client = std::thread::spawn({
            let path = path.clone();
            move || Connection::connect(&path).unwrap()
        });
        let accepted = listener.accept_where(|uid| uid == mine).unwrap();

        drop(accepted);
        drop(client.join().unwrap());
    }

    #[test]
    fn an_open_listener_refuses_the_uid_a_predicate_denies() {
        let path = temporary_socket_path();
        let listener = Listener::bind_open(&path).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || Connection::connect(&path)
        });
        let refused = listener.accept_where(|_| false);

        assert_eq!(
            refused.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let _ = client.join().unwrap();
    }
```

Reuse whatever the existing tests use to make a path and to learn the current
uid — `own_gid()` has a sibling or is one line away; do not add a second helper
that does the same thing.

- [ ] **Step 6: Run to verify it fails, then implement**

Run: `cargo test -p vmlord-display-services unix::`
Expected: FAIL — `bind_open` and `accept_where` do not exist.

Refactor `Listener::bind` into a private `bind_with(path, group, mode)` and give
it two public faces, keeping the existing one's behaviour exactly:

```rust
    /// Binds a socket a peer of unknown uid may reach.
    ///
    /// Mode `0666` and no group, because the process that connects is the
    /// clipboard daemon of whoever logged in and its uid is not known here.
    /// What guards this socket is [`Listener::accept_where`], which is a
    /// stronger check than a mode bit: it names the session at the screen
    /// rather than a set of accounts.
    pub fn bind_open(path: &Path) -> io::Result<Self> {
        Self::bind_with(path, libc::gid_t::MAX, 0o666)
    }
```

and

```rust
    /// Accepts one peer and refuses it unless `allow` says its uid may.
    ///
    /// The general form of [`Listener::accept`], for a socket whose peer is not
    /// one fixed account.
    pub fn accept_where(&self, allow: impl Fn(libc::uid_t) -> bool) -> io::Result<Connection> {
```

`accept(expected_uid)` becomes `self.accept_where(|uid| uid == expected_uid)`
with the message it already produces; keep that message, because a test asserts
it or a log reads it.

- [ ] **Step 7: Run to verify it passes**

Run: `cargo test -p vmlord-display-services unix::`
Expected: PASS.

- [ ] **Step 8: Add the IPC message**

In `broker.proto`:

```protobuf
    ClipboardOpened clipboard_opened = 10;
```

```protobuf
// A control handshake completed, for the clipboard daemon: the session it
// belongs to and the one channel key that daemon may use. The frame and input
// keys are not here -- the daemon has no business with those sockets.
message ClipboardOpened {
  bytes session_id = 1;
  bytes clipboard_key = 2;
}
```

In `ipc.rs` add the variant, its encode arm and its decode arm beside
`SessionOpened`:

```rust
    /// A control handshake completed, as the clipboard daemon needs it.
    ClipboardOpened {
        /// The 16 bytes that name the session across its four sockets.
        session_id: Vec<u8>,
        /// The key the clipboard socket proves itself with.
        clipboard_key: Vec<u8>,
    },
```

and a round-trip test beside the existing ones:

```rust
    #[test]
    fn a_clipboard_opened_survives_a_round_trip() {
        let message = Message::ClipboardOpened {
            session_id: vec![7; 16],
            clipboard_key: vec![9; 32],
        };

        let bytes = encode(&message);

        assert_eq!(decode(&bytes).unwrap(), message);
    }
```

using whatever the module's existing round-trip test calls those two functions.

- [ ] **Step 9: Serve the socket in the broker**

In `broker_main.rs`:

* `Options` gains `clipboard_socket: PathBuf`, defaulting to
  `/run/vmlord/display-clipboard.sock`, parsed like `socket` is.
* `BrokerState` gains `clipboard_peer: Option<Arc<Connection>>` and
  `clipboard: Option<(Vec<u8>, Vec<u8>)>` — the session id and clipboard key of
  the session that is open, so a daemon that attaches mid-session is told at
  once, exactly as `adopt_peer` does for the capture process.
* `serve` binds it with `Listener::bind_open` and spawns
  `serve_clipboard_peers`, which loops on
  `listener.accept_where(|uid| seat::active_graphical_uid() == Some(uid))`,
  logs and continues on `PermissionDenied`, sends `ClipboardOpened` if a session
  is open, and then reads that peer until it goes away. A peer sends only
  `Attach` and `Report`; anything else is logged and ignored, the way
  `read_peer` already does.
* Where the control handshake fills `state.session` and sends `SessionOpened`,
  derive the clipboard key too and send `ClipboardOpened` to the clipboard peer.
  `Control::opened` is where the other two keys are derived; add the third
  there and carry it in `Outcome::Opened` as a second field rather than
  widening `SessionParameters`, which is the capture process's message and must
  not carry a key that process may not have.
* Where `SessionClosed` goes to the capture peer, send it to the clipboard peer
  too.

- [ ] **Step 10: Verify the guest builds and its tests pass**

Run: `cargo test -p vmlord-display-services`
Expected: PASS.
Run: `cargo display-services`
Expected: the two existing binaries still build for musl.

- [ ] **Step 11: Commit**

```bash
git add crates/display-services
git commit -m "TASK-125: Hand the clipboard channel to the session at the screen

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The Mutter adapter

**Files:**
- Create: `crates/display-services/src/mutter.rs`
- Modify: `crates/display-services/Cargo.toml`, `crates/display-services/src/lib.rs`
- Test: the `mod tests` in `mutter.rs` (parsing and mapping only — the bus is not available under test)

**Interfaces:**
- Consumes: `clipboard::Kind` from Task 3.
- Produces:

```rust
pub struct Clipboard;                     // one RemoteDesktop session
pub enum Event {
    /// The guest's selection changed and this side does not own it.
    PeerOffer { kinds: Vec<Kind> },
    /// Something in the guest wants the selection this side owns.
    Transfer { kind: Kind, serial: u32 },
    /// The compositor closed the session; the daemon must open another.
    Closed,
}
impl Clipboard {
    pub fn open() -> Result<(Self, Receiver<Event>), MutterError>;
    pub fn listen(&self) -> Result<(), MutterError>;              // EnableClipboard({})
    pub fn own(&self, kinds: &[Kind]) -> Result<(), MutterError>; // SetSelection
    pub fn read(&self, kind: Kind, cap: usize) -> Result<Vec<u8>, MutterError>;
    pub fn write(&self, serial: u32, bytes: &[u8]) -> Result<(), MutterError>;
    pub fn refuse(&self, serial: u32) -> Result<(), MutterError>;
}
```

- [ ] **Step 1: Add the dependency**

In `crates/display-services/Cargo.toml`:

```toml
# GNOME's clipboard lives in the compositor, and `org.gnome.Mutter.RemoteDesktop`
# is the interface that reaches it -- the same one gnome-remote-desktop uses. No
# default features, so nothing here links a system C library and the musl build
# stays toolchain-free.
zbus = { version = "5", default-features = false, features = ["async-io", "blocking-api"] }
```

Run: `cargo display-services`
Expected: it builds. If the resolver picks a zbus that wants a C library,
stop and report rather than adding one.

- [ ] **Step 2: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_mime_types_reach_a_kind() {
        let offered = vec![
            "text/uri-list".to_owned(),
            "image/png".to_owned(),
            "text/plain;charset=utf-8".to_owned(),
        ];

        assert_eq!(kinds_of(&offered), vec![Kind::Text, Kind::Png]);
    }

    #[test]
    fn a_read_stops_at_its_cap() {
        // `drain` is the poll loop that fills a buffer from the descriptor
        // `SelectionRead` returned; a fake reader stands in for the descriptor.
        let mut source = std::io::Cursor::new(vec![b'x'; 40]);

        assert!(matches!(drain(&mut source, 16), Err(MutterError::TooLarge)));
    }
}
```

`kinds_of` returns the allowlisted kinds in the canonical order
(`Text, Html, Bmp, Png`), which is what makes the daemon's offers deterministic.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p vmlord-display-services mutter::`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Implement the adapter**

The module's documentation states what the spike established, because none of
it is obvious from the interface:

```rust
//! GNOME's clipboard, through the one interface that reaches it from outside a
//! Wayland client.
//!
//! `org.gnome.Mutter.RemoteDesktop` carries a clipboard and has since GNOME 42,
//! which is the whole compatibility matrix. Three things about it are worth
//! knowing before reading this file, all of them established against a running
//! guest rather than read out of documentation:
//!
//!   * a session may be created and started with no ScreenCast session beside
//!     it, which is why this is a small daemon rather than a screen-sharing
//!     stack;
//!   * `EnableClipboard` with no mime types makes this a listener, and with
//!     mime types makes it the owner -- and Mutter refuses `SelectionRead` on a
//!     selection the caller owns, so the two states are not interchangeable;
//!   * the descriptor `SelectionRead` returns is non-blocking, and the first
//!     read of it usually returns `EAGAIN`. Reading it is a poll loop, which is
//!     also where the cap lives.
```

The implementation:

* `open()` calls `CreateSession` on `/org/gnome/Mutter/RemoteDesktop`, `Start()`
  on the returned path, subscribes to `SelectionOwnerChanged`,
  `SelectionTransfer` and `Closed`, and spawns the thread that turns those
  signals into `Event`s on an `mpsc::Sender`. A `SelectionOwnerChanged` whose
  `session-is-owner` is true is dropped: that is this side's own ownership
  coming back, and forwarding it is the echo the spec forbids.
* `listen()` is `EnableClipboard` with an empty options dictionary.
* `own(kinds)` is `SetSelection` with `mime-types`.
* `read(kind, cap)` is `SelectionRead` followed by `drain`, which polls the
  descriptor with `libc::poll`, appends, stops at `cap` with
  `MutterError::TooLarge`, and gives up after five seconds with
  `MutterError::Idle`.
* `write(serial, bytes)` is `SelectionWrite`, a full write to the returned
  descriptor, then `SelectionWriteDone(serial, true)`.
* `refuse(serial)` is `SelectionWriteDone(serial, false)` — what answers a
  transfer whose bytes the host would not send.
* Every error is a `MutterError` with a message that names the call, never the
  contents.

- [ ] **Step 5: Run to verify it passes and the guest still builds**

Run: `cargo test -p vmlord-display-services mutter::`
Expected: PASS.
Run: `cargo display-services`
Expected: builds.

- [ ] **Step 6: Commit**

```bash
git add crates/display-services
git commit -m "TASK-125: Reach GNOME's clipboard through Mutter

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The guest clipboard daemon

**Files:**
- Create: `crates/display-services/src/clipboard_main.rs`, `crates/display-services/src/bin/clipboard.rs`
- Modify: `crates/display-services/src/lib.rs`, `crates/display-services/src/channel.rs` (if the bind helper names the frame enum in a way that excludes this channel)
- Test: the `mod tests` in `clipboard_main.rs`

**Interfaces:**
- Consumes: `ipc::Message::ClipboardOpened`, `clipboard::Exchange`, `mutter::Clipboard`, `vsock::Listener`, `channel::bind`.
- Produces: the binary `vmlord-display-clipboard`.

- [ ] **Step 1: Add the vsock port and the binary**

In `vsock.rs`, beside the three ports:

```rust
/// Where selections cross, in both directions. `"VMLC"`.
pub const CLIPBOARD_PORT: u32 = 0x564D_4C43;
```

`src/bin/clipboard.rs` mirrors the two existing entry points exactly:

```rust
//! The guest's clipboard daemon. One user session's, and no more.

fn main() -> std::process::ExitCode {
    vmlord_display_services::clipboard_main::run(vmlord_display_services::clipboard_main::Options::from_args())
}
```

Match whatever shape `src/bin/broker.rs` uses rather than inventing a second
one.

- [ ] **Step 2: Write the failing test**

In `clipboard_main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use vmlord_display_protocol::clipboard::{Kind, Message, Op};

    #[test]
    fn an_op_becomes_the_record_its_type_names() {
        let record = record_of(
            &Message::Offer {
                serial: 3,
                mime_types: vec![Kind::Text.mime()],
            },
            0,
            0,
        );

        assert_eq!(record.header.channel, Channel::Clipboard);
        assert_eq!(
            record.header.message_type,
            ClipboardRecord::Offer as u16
        );
    }

    #[test]
    fn a_record_becomes_the_call_its_type_names() {
        let offer = ClipboardOffer {
            serial: 4,
            mime_types: vec![Kind::Text.mime().to_owned()],
        };
        let record = record_of_offer(&offer);

        let parsed = parse(&record.header, &record.payload).expect("a clipboard record");

        assert!(matches!(parsed, Incoming::Offer { serial: 4, .. }));
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p vmlord-display-services clipboard_main::`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Implement the daemon**

Structure, in the shape the rest of this crate uses — threads and channels, no
async runtime:

* `Options::from_args` parses `--broker-socket` (default
  `/run/vmlord/display-clipboard.sock`) and nothing else.
* `run` connects to the broker socket, sends `Attach`, and waits for
  `ClipboardOpened`. A broker that is not there yet is retried every two
  seconds: the daemon starts with the user's session, which is usually long
  after the broker.
* On `ClipboardOpened` it binds `vsock::Listener::bind(CLIPBOARD_PORT)` if it
  has not already, accepts one host connection, and proves the channel with
  `channel::bind`, which is the same three-record exchange the frame and input
  sockets use — pass `Channel::Clipboard` and the key from the message.
* Three threads and one loop:
  * the socket reader turns records into `Incoming` values on an `mpsc`;
  * the Mutter adapter's thread produces `mutter::Event`s on another;
  * the main loop `recv_timeout`s on both with a 500 ms timeout, so
    `Exchange::tick` runs even when nothing arrives, and writes every `Op::Send`
    to the socket.
* The `Op`s map as follows, and nowhere else in the daemon does policy live:
  * `Op::Send(message)` — write the record.
  * `Op::Produce { kind, transfer }` — `mutter.read(kind, kind.cap())`, then
    `exchange.produced(transfer, bytes, now)`; on `TooLarge` or any error,
    `exchange.unavailable(transfer)`.
  * `Op::Apply { pieces }` — `mutter.own(kinds)` and keep the pieces, so the
    `Transfer` signal that follows can be answered with `mutter.write`.
* `mutter::Event::PeerOffer { kinds }` becomes
  `exchange.local_offer(&kinds, now)` — from the daemon's point of view the
  guest's selection is the local one.
* `mutter::Event::Transfer { kind, serial }` is answered from the pieces the
  last `Apply` left; if there are none for that kind, `mutter.refuse(serial)`.
* A socket that drops returns to the accept loop; the exchange is rebuilt,
  because a new host connection is a new generation and nothing may carry over.
* Every log line names a mime type, a byte count and an outcome. Write a test
  that a `Vec<u8>` sink of the log during a transfer of `b"secret"` contains no
  `secret`, if the crate's logging is already testable that way; otherwise keep
  the rule visible by never passing a body to a formatting macro, and note it
  in the module documentation.

- [ ] **Step 5: Announce the capability**

The guest has to say it has a clipboard, and this is the build that does.
In `control.rs`, `support_from` gains it:

```rust
        capabilities: vec![
            Capability::CursorStream,
            Capability::DynamicResolution,
            // Announced by the build, not by an attached daemon: a session
            // commonly opens at the login screen, where no user session and
            // therefore no daemon exists, and a capability settled in the
            // handshake cannot be renegotiated when one appears. With no daemon
            // attached the guest simply offers nothing.
            Capability::Clipboard,
        ],
```

Add the assertion to whichever test in that module checks what a guest
announces:

```rust
        assert!(support_from(1920, 1080).capabilities.contains(&Capability::Clipboard));
```

- [ ] **Step 6: Run to verify it passes and it builds for the guest**

Run: `cargo test -p vmlord-display-services`
Expected: PASS.
Run: `cargo display-services`
Expected: three binaries in `target/x86_64-unknown-linux-musl/release`.

- [ ] **Step 7: Commit**

```bash
git add crates/display-services
git commit -m "TASK-125: Add the guest clipboard daemon

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Shipping the daemon in the payload

**Files:**
- Create: `payloads/display/services/vmlord-display-clipboard.service`
- Modify: `payloads/display/prepare.sh:139`
- Modify: `crates/agent/src/display_kernel.rs:63-70`, `:765-815`
- Test: `crates/agent/src/display_kernel.rs`'s `mod tests`

**Interfaces:**
- Consumes: the binary from Task 6.
- Produces: a payload whose `content/services/` holds three binaries and three
  units, and a recipe that installs the user unit and enables it globally.

- [ ] **Step 1: Write the user unit**

`payloads/display/services/vmlord-display-clipboard.service`:

```ini
[Unit]
Description=VMLord display clipboard
Documentation=https://github.com/mrundead/vmlord
# A user unit: the clipboard exists inside a compositor, and only a process in
# the user's session can reach that user's session bus. Started with the
# graphical session and stopped with it.
After=graphical-session.target
PartOf=graphical-session.target

# The crash-loop budget, under [Unit] for the reason the other two units give.
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=/usr/local/lib/vmlord/vmlord-display-clipboard
# The broker may not be up yet, and the socket may not exist for minutes after
# a login; waiting for it is the daemon's own job.
Restart=on-failure
RestartSec=3

[Install]
WantedBy=graphical-session.target
```

- [ ] **Step 2: Put it in the payload**

In `prepare.sh`, the loop that installs binaries gains the third name:

```bash
for binary in vmlord-display-broker vmlord-display-session vmlord-display-clipboard; do
```

The `.service` files are already installed by a glob, so the unit needs no
second change.

- [ ] **Step 3: Write the failing test for the recipe**

In `display_kernel.rs`'s `mod tests`, extend whichever test asserts the
installed set — if there is none, add:

```rust
    #[test]
    fn the_payload_carries_three_services_and_the_clipboard_is_a_user_unit() {
        assert_eq!(SERVICE_BINARIES.len(), 3);
        assert!(SERVICE_BINARIES.contains(&"vmlord-display-clipboard"));
        assert_eq!(SYSTEM_UNITS.len(), 2);
        assert_eq!(USER_UNITS, ["vmlord-display-clipboard.service"]);
    }
```

- [ ] **Step 4: Run to verify it fails, then implement**

Run: `cargo test -p vmlord-agent display_kernel::`
Expected: FAIL — `SERVICE_UNITS` is one constant and `USER_UNITS` does not
exist.

Split the unit list, keeping the existing name for the system half or renaming
both consistently across the file:

```rust
/// The three programs, in the order they are started.
const SERVICE_BINARIES: [&str; 3] = [
    "vmlord-display-broker",
    "vmlord-display-session",
    "vmlord-display-clipboard",
];
/// The units systemd starts at boot.
const SYSTEM_UNITS: [&str; 2] = [
    "vmlord-display-broker.service",
    "vmlord-display-session.service",
];
/// The unit that starts inside a user's graphical session.
const USER_UNITS: [&str; 1] = ["vmlord-display-clipboard.service"];
/// Where user units go.
const SYSTEMD_USER_UNITS: &str = "/etc/systemd/user";
```

`install_services` installs the system units where it does now and the user
units into `SYSTEMD_USER_UNITS`, then enables them differently:

```rust
    // `--global` rather than `--user`: the recipe runs as root, outside any
    // user session, and the unit has to be wanted by whichever session starts
    // next. Enabling it per user would mean the recipe knowing a name that is
    // not decided until someone logs in.
    for unit in USER_UNITS {
        let enabled = command::run("systemctl", &["--global", "enable", unit], &[], SHORT_BUDGET);
        if !enabled.succeeded() {
            return Err(failure(&format!("systemctl --global enable {unit}"), &enabled));
        }
    }
```

`start_services` is untouched: it waits for the two system units and the broker
socket, and a user unit that starts at the next login is not something a recipe
running as root can wait for. Say that in a comment, because its absence there
would otherwise read as an omission.

`services_need_install` compares the payload against what is installed; make
sure the third binary and the user unit are part of that comparison, or a
payload update will leave a stale daemon behind.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p vmlord-agent`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add payloads crates/agent
git commit -m "TASK-125: Ship the clipboard daemon in the display payload

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: The host's fourth service, port and key

**Files:**
- Modify: `crates/platform/src/hvsocket.rs:56-80`, `crates/platform/src/hcs_config.rs:166-182`
- Modify: `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto`
- Modify: `crates/platform/src/display_session.rs:100-115`, `:230-275`
- Test: `crates/platform/src/hvsocket.rs`, `crates/platform/src/hcs_config.rs` and `crates/platform/src/display_session.rs` `mod tests`

**Interfaces:**
- Consumes: Task 2's `Session::derive_channel_key(Channel::Clipboard)`.
- Produces: `hvsocket::DISPLAY_CLIPBOARD_VSOCK_PORT`, `display_service_ids() -> [GUID; 4]`,
  `LaunchParameters.clipboard_port` (field 11), `Handover.clipboard_key` (field 12),
  and `Capability::Clipboard` in the host's offer.

- [ ] **Step 1: Write the failing tests**

In `hvsocket.rs`'s tests:

```rust
        assert_eq!(DISPLAY_CLIPBOARD_VSOCK_PORT, 0x564D_4C43);
        assert_eq!(display_service_ids().len(), 4);
```

In `hcs_config.rs`'s tests, beside the existing service-table assertions:

```rust
    const DISPLAY_CLIPBOARD_SERVICE_KEY: &str = "564D4C43-FACB-11E6-BD58-64006A7986D3";

    #[test]
    fn a_built_vm_lists_the_clipboard_service() {
        let table = service_table_of_a_built_vm();

        assert!(table.contains_key(DISPLAY_CLIPBOARD_SERVICE_KEY));
    }
```

In `display_session.rs`'s tests:

```rust
        assert_eq!(parameters.clipboard_port, 0x564D_4C43);
        assert_eq!(handover.clipboard_key.len(), 32);
```

and, in whichever test builds the offer:

```rust
        assert!(offer.capabilities.contains(&Capability::Clipboard));
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu` — or
`cargo test-windows -p vmlord-platform`, which is the alias for the same thing.
Expected: FAIL — none of the four names exist.

- [ ] **Step 3: Implement**

`hvsocket.rs`:

```rust
/// The vsock port a guest's display clipboard service listens on -- `VMLC`.
pub(crate) const DISPLAY_CLIPBOARD_VSOCK_PORT: u32 = 0x564D_4C43;
```

`display_service_ids` returns four ids and its doc comment says four; the
service table follows on its own, because it iterates.

The viewer schema gains two fields, appended:

```protobuf
  uint32 clipboard_port = 11;
```

```protobuf
  bytes clipboard_key = 12;
```

`display_session.rs`: the offer's capabilities gain `Capability::Clipboard`,
`LaunchParameters` gains `clipboard_port: DISPLAY_CLIPBOARD_VSOCK_PORT`, and
`hand_over` derives the third key beside the other two:

```rust
        let clipboard = session
            .derive_channel_key(Channel::Clipboard)
            .ok_or_else(|| missing("clipboard key"))?;
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform crates/display-viewer/proto
git commit -m "TASK-125: List the clipboard service and hand over its key

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: The Windows clipboard formats

Conversions only: no window, no socket, no thread. Everything here is testable
on its own, and the next task uses it.

**Files:**
- Create: `crates/display-viewer/src/clipboard/win32.rs`
- Modify: `crates/display-viewer/Cargo.toml`, `crates/display-viewer/src/lib.rs`
- Test: `crates/display-viewer/src/clipboard/win32.rs`'s `mod tests`

**Interfaces:**
- Consumes: `clipboard::{Kind, Piece}` from Task 3.
- Produces:

```rust
pub fn utf16_of(text: &[u8]) -> Vec<u16>;
pub fn utf8_of(units: &[u16]) -> Vec<u8>;
pub fn cf_html_of(html: &[u8]) -> Vec<u8>;
pub fn html_of_cf_html(envelope: &[u8]) -> Option<Vec<u8>>;
pub fn dib_of_bmp(bmp: &[u8]) -> Option<Vec<u8>>;
pub fn bmp_of_dib(dib: &[u8]) -> Vec<u8>;
pub fn bmp_of_png(png: &[u8]) -> Result<Vec<u8>, ImageError>;
pub fn png_of_bmp(bmp: &[u8]) -> Result<Vec<u8>, ImageError>;
```

- [ ] **Step 1: Add the dependency**

In `crates/display-viewer/Cargo.toml`:

```toml
# The clipboard's only codec. GTK applications often offer a picture as PNG and
# nothing else, and no Windows clipboard format holds one; a DIB does, and this
# is what gets between them. Already in the lock file at this version.
png = "0.18"
```

and the `windows` crate's features gain `Win32_System_DataExchange` and
`Win32_System_Memory`, which is what `OpenClipboard`, `SetClipboardData`,
`GlobalAlloc` and `GlobalLock` live behind.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_crosses_the_encodings_unchanged() {
        let text = "ёлка — ok\r\n".as_bytes();

        assert_eq!(utf8_of(&utf16_of(text)), text);
    }

    #[test]
    fn a_lone_surrogate_does_not_panic_and_does_not_vanish_silently() {
        let broken = [0xD800u16, b'a' as u16];

        let text = utf8_of(&broken);

        assert!(text.ends_with(b"a"));
    }

    #[test]
    fn an_html_envelope_round_trips() {
        let html = b"<b>hi</b>";

        let envelope = cf_html_of(html);

        assert!(envelope.starts_with(b"Version:0.9"));
        assert_eq!(html_of_cf_html(&envelope).as_deref(), Some(&html[..]));
    }

    #[test]
    fn an_envelope_offsets_point_at_the_fragment() {
        let envelope = cf_html_of(b"<p>x</p>");
        let text = String::from_utf8(envelope.clone()).expect("ascii headers");

        let start: usize = header_value(&text, "StartFragment").expect("a start offset");
        let end: usize = header_value(&text, "EndFragment").expect("an end offset");

        assert_eq!(&envelope[start..end], b"<p>x</p>");
    }

    #[test]
    fn a_dib_is_a_bmp_without_its_file_header() {
        let bmp = smallest_bmp();

        let dib = dib_of_bmp(&bmp).expect("a bmp this build wrote");

        assert_eq!(dib.len(), bmp.len() - 14);
        assert_eq!(bmp_of_dib(&dib), bmp);
    }

    #[test]
    fn a_truncated_bmp_is_refused_rather_than_sliced() {
        assert_eq!(dib_of_bmp(b"BM"), None);
    }

    #[test]
    fn a_picture_survives_png_and_back() {
        let bmp = smallest_bmp();

        let png = png_of_bmp(&bmp).expect("an encodable picture");
        let back = bmp_of_png(&png).expect("a decodable picture");

        assert_eq!(pixels_of(&back), pixels_of(&bmp));
    }
}
```

`smallest_bmp()` builds a 2×2 24-bit BMP by hand in the test module;
`header_value` and `pixels_of` are test helpers in the same module.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer clipboard::win32`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Implement the conversions**

Points the implementation must get right, each of which one test above pins:

* `utf16_of` appends a terminating NUL, because `CF_UNICODETEXT` is
  NUL-terminated; `utf8_of` stops at the first NUL and uses
  `String::from_utf16_lossy`, so a lone surrogate becomes the replacement
  character rather than a panic or a truncation.
* `cf_html_of` writes the `Version`, `StartHTML`, `EndHTML`, `StartFragment`
  and `EndFragment` headers with ten-digit zero-padded offsets computed after
  the header block's own length is known, wraps the fragment in
  `<!--StartFragment-->` and `<!--EndFragment-->`, and is ASCII throughout.
* `html_of_cf_html` reads the two fragment offsets and returns the slice
  between them, or `None` for an envelope whose headers are missing or whose
  offsets do not lie inside the buffer.
* `dib_of_bmp` checks the `BM` magic and that the file is at least 14 + 40
  bytes before dropping the file header; `bmp_of_dib` computes the file size
  and the pixel offset from the DIB header rather than assuming 40 bytes, since
  a `CF_DIBV5` header is 124.
* `png_of_bmp` and `bmp_of_png` go through the `png` crate and a plain BMP
  writer; both refuse a picture whose dimensions would overflow
  `MAX_IMAGE_TRANSFER` rather than allocating it.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer clipboard::win32`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer
git commit -m "TASK-125: Convert between clipboard formats

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 10: The viewer's clipboard thread

**Files:**
- Create: `crates/display-viewer/src/clipboard/mod.rs`
- Modify: `crates/display-viewer/src/launch.rs` (the two new fields), `crates/display-viewer/src/main.rs:180-230` (start the thread), `:620-640` (focus)
- Test: `crates/display-viewer/src/clipboard/mod.rs`'s `mod tests`

**Interfaces:**
- Consumes: Task 3's `Exchange`, Task 9's conversions, Task 8's
  `clipboard_port` and `clipboard_key`.
- Produces: `clipboard::spawn(Parameters) -> (JoinHandle<()>, Sender<Focus>)`,
  where `Focus` is `Gained` or `Lost`.

- [ ] **Step 1: Write the failing test**

The thread's policy is the `Exchange`, which Task 3 tested; what is left to
test here is the mapping between `Piece`s and Windows formats, which needs no
window:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use vmlord_display_protocol::clipboard::{Kind, Piece};

    #[test]
    fn pieces_become_the_formats_windows_names() {
        let pieces = vec![
            Piece { kind: Kind::Text, bytes: b"hi".to_vec() },
            Piece { kind: Kind::Html, bytes: b"<b>hi</b>".to_vec() },
            Piece { kind: Kind::Bmp, bytes: smallest_bmp() },
        ];

        let formats = formats_of(&pieces);

        assert_eq!(formats.len(), 3);
        assert!(formats.iter().any(|(id, _)| *id == CF_UNICODETEXT));
        assert!(formats.iter().any(|(id, _)| *id == CF_DIB));
    }

    #[test]
    fn a_picture_that_will_not_convert_is_dropped_rather_than_failing_the_paste() {
        let pieces = vec![
            Piece { kind: Kind::Text, bytes: b"hi".to_vec() },
            Piece { kind: Kind::Png, bytes: b"not a png".to_vec() },
        ];

        let formats = formats_of(&pieces);

        assert_eq!(formats.len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test-windows -p vmlord-display-viewer clipboard::tests`
Expected: FAIL — `formats_of` does not exist.

- [ ] **Step 3: Implement the thread**

* `spawn` starts one thread that owns everything: the HvSocket connection to
  `clipboard_port`, a `Session::established_host` built from the same hand-over
  the session thread got — the viewer holds all three keys, so this is not a
  second credential — and a message-only window created with
  `HWND_MESSAGE` and registered with `AddClipboardFormatListener`.
* Its loop pumps that window's messages, reads records off the socket, and
  drives the `Exchange`:
  * `WM_CLIPBOARDUPDATE` whose `GetClipboardSequenceNumber` is the one this
    thread last set is ignored — that is the echo of this side's own write;
  * otherwise, if the window has focus, the available formats are enumerated
    into `Kind`s and `exchange.local_offer` runs. If it does not have focus, the
    change is remembered and offered on the next `Focus::Gained`.
  * `Op::Produce` reads the format from the clipboard and converts it;
  * `Op::Apply` opens the clipboard once, empties it, sets every format
    `formats_of` produced, and records the new sequence number;
  * `Op::Send` writes a record, numbering it with
    `session.take_channel_sequence(Channel::Clipboard)` and the channel's
    generation, exactly as `Live::send_input` does for input.
* `Focus::Lost` runs `exchange.focus_lost`; `Focus::Gained` re-offers whatever
  the host holds.
* A socket that fails is rebound through `Session::reconnect_channel` with the
  same backoff `Live::bind_channels` uses; a viewer whose clipboard channel
  never binds still shows a picture and still types, and says so once in the
  log.
* The thread ends when its `Sender<Focus>` is dropped, which is when the window
  is closing.
* No log line carries a byte of a selection. The thread logs the kind, the byte
  count and the outcome, and never passes a body or a converted buffer to a
  formatting macro -- including at `debug` and `trace`, where a stray
  `{pieces:?}` would print everything the user copied.

In `main.rs`, `Report::FocusGained` and `Report::FocusLost` — which the pump
already receives — also send `Focus::Gained` and `Focus::Lost` down that
channel, beside the hook they already install and drop.

- [ ] **Step 4: Run to verify it passes and the viewer builds**

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS.
Run: `cargo check-windows`
Expected: the whole Windows application compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer
git commit -m "TASK-125: Carry the clipboard in the display window

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 11: End to end on a guest, then the documentation

**Files:**
- Modify: `ARCHITECTURE.md`, `docs/display-compatibility.md`, `docs/display-user-guide.md`, `docs/display-troubleshooting.md`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing further code depends on.

- [ ] **Step 1: Build the payload and put it in a guest**

Run: `cargo display-services`, then `payloads/display/prepare.sh` the way
`docs/display-drm-backend.md` describes, and update the `test` VM through the
application's usual display payload path. The guest is reached with the
`ssh.exe` command the application prints; WSL cannot route to it directly.

- [ ] **Step 2: Check the daemon is running in the session**

```bash
systemctl --user status vmlord-display-clipboard.service
```
Expected: active, with no clipboard content anywhere in its journal.

- [ ] **Step 3: Exercise the matrix by hand**

With a display window open and focused, confirm each of these, and write down
what happened:

1. text copied in the guest pastes on the host, and the other way;
2. HTML copied from a browser keeps its formatting in WordPad or Word;
3. an image copied in the guest pastes into Paint, and a Windows screenshot
   pastes into a GNOME application;
4. a copy made while the window is unfocused reaches the guest only after the
   window is focused again;
5. an oversized selection is refused without the session dropping;
6. the daemon killed mid-session leaves the picture and the keyboard working,
   and a restarted daemon works again;
7. no line of either journal or of the viewer's log holds a byte of anything
   copied.

- [ ] **Step 4: Write the documentation**

* `ARCHITECTURE.md` — the fourth channel, the daemon, the broker's second
  socket and the authorisation rule, in the voice of the display sections
  already there.
* `docs/display-compatibility.md` — the line that says clipboard is not part of
  the MVP display contract is now wrong for clipboard: say what is carried
  (text, HTML, images), what is not (files, task #139), and that the clipboard
  needs a logged-in GNOME session.
* `docs/display-user-guide.md` — how it behaves: it follows the window's focus,
  and it is not a file transfer.
* `docs/display-troubleshooting.md` — nothing pastes: check that a user is
  logged in, that `systemctl --user status vmlord-display-clipboard` is active,
  that the window has focus, and that the selection is not larger than the cap.

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md docs
git commit -m "TASK-125: Document the display clipboard

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

- [ ] **Step 6: Final verification**

Run: `cargo test -p vmlord-display-protocol -p vmlord-display-services -p vmlord-agent`
Run: `cargo test-windows`
Run: `cargo check-windows`
Run: `cargo display-services`
Expected: all pass. Report the end-to-end results from Step 3 with the output,
not a summary of it.
