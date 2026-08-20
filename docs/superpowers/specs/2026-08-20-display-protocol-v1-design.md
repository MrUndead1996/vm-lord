# Display protocol v1 design

## Goal

Task #118 designs the wire contract VMLord's own display stack runs on, and
designs it once, before either end of it exists. Three tasks are waiting on
this contract and none of them may invent its own half of it: #116 encodes the
pixels this protocol carries, #115 serves it from inside the guest, #117 draws
what arrives on the host, and #119 sends input back along it.

The contract is portable by construction. It knows nothing about DRM, nothing
about the codec's byte format, nothing about HvSocket, and nothing about
Windows. What it knows is what a message is, how one is delimited, who is
allowed to send it, and what happens when a connection ends.

## Scope

This task produces `vmlord-display-protocol`: the schema, the record framing,
the limits, the mutual authentication and channel binding, and a
transport-free session state machine that both ends drive. It produces the
tests that hold that format still -- round-trip, compatibility, malformed and
oversized corpora, golden vectors, fuzz targets -- and the ARCHITECTURE.md
section that explains it.

It produces no sockets, no capture, no rendering, no codec. `Keyframe` and
`TileDelta` payloads are opaque bytes here; what is inside them is #116's
decision, and this crate must never grow an opinion about it.

## Transport shape

Three services, three vsock ports in the guest, named the way the agent's
`VMLA` is named: `VMLD` (control, `0x564D4C44`), `VMLF` (frame,
`0x564D4C46`), `VMLI` (input, `0x564D4C49`). Registering them in
`Devices/HvSocket/HvSocketConfig/ServiceTable` belongs to task #121.

**The guest listens and the host connects**, which is the opposite of the
agent protocol and is deliberate. The agent's connection is a standing report
that lives as long as the VM; a display session begins when a user presses
Connect and ends when they close the window. Making the socket's lifetime the
session's lifetime means there is no "is the stream currently on?" state to
keep in the protocol: no viewer, no connection, no capture.

The one thing the agent's direction gave for free -- the guest knowing when it
is ready -- is bought back outside this protocol. The guest reports display
readiness over the agent channel (task #112), and the UI keeps Connect
disabled until it does. What is left for the viewer is a short bounded retry,
covering only the race where a user connects while the guest service is
restarting; it is not an open-ended wait for a guest that may never arrive.

## Records

Every record on every channel begins with the same 24-byte little-endian
header:

| offset | field | meaning |
| --- | --- | --- |
| 0 | `u8 header_len` | 24 in v1.0 |
| 1 | `u8 channel` | control 1, frame 2, input 3 |
| 2 | `u16 type` | message type within the channel |
| 4 | `u32 length` | payload bytes that follow |
| 8 | `u32 sequence` | per channel, from zero |
| 12 | `u32 base` | frame deltas: the `sequence` this applies to |
| 16 | `u32 checksum` | CRC32C of the payload |
| 20 | `u32 generation` | session generation |

The payload is a Protobuf message on the control and input channels and for
the frame channel's own handshake and `StreamConfig`; for `Keyframe`,
`TileDelta`, `CursorImage` and `CursorPosition` it is codec bytes, written to
the socket as they were produced. One framing rule, three vocabularies.

`base` is meaningful only on `TileDelta`, where it names the `sequence` of the
frame the delta builds on; it is zero everywhere else, including on
`Keyframe`, which builds on nothing.

That split is the reason the frame channel is not Protobuf all the way down. A
1440p keyframe is megabytes; carrying it in a `bytes` field costs a full copy
of it through the encoder on the sending side and another on the receiving
side, every frame, in the only part of VMLord where the wire format meets real
bandwidth. The frame channel is also the least changeable part of the
contract -- a frame is a sequence, a base and some bytes -- while the control
and input channels are the changeable ones and are cold. Each channel gets the
format its nature asks for.

`header_len` occupies the first byte instead of a magic number. A magic would
cost four bytes per frame to make packet dumps readable, and the version is
already settled in the handshake; `header_len` buys the thing a magic cannot,
which is room for v1.2 to append a field that v1.0 skips without losing the
stream.

CRC32C is a corruption check, not a signature. Nothing after the handshake is
authenticated per record, for the reasons under "Threat model" below.

## Limits

Control records are capped at 64 KiB and input records at 4 KiB, both fixed.

The frame cap is not a constant. It is `width * height * 4 + 64 KiB` for the
geometry the session has agreed on, recomputed whenever the geometry changes,
under an absolute ceiling of 64 MiB. Until the first `StreamConfig` arrives,
the geometry named in the handshake is what the cap is derived from. A record
larger than an uncompressed
frame of the agreed size is not a frame by definition, so "oversized" stops
being a magic number and becomes a statement about the session.

A record that exceeds its cap is unrecoverable, as it is in the agent
protocol: the stream is parked on a body of unknown length and cannot be
resynchronised, so the connection closes. The cap is enforced before anything
is allocated, on both the reading and the writing side.

## Authentication

The root of trust is the per-VM secret that already exists: 32 bytes minted at
creation, delivered through the seed as `/etc/vmlord/agent.secret`, root-only.
It is not copied, not re-minted, and not handed to a second file for a second
user.

The capture and encode process in the guest is unprivileged by design (#115),
and must not be able to read that secret. So the privileged broker -- already
root, because it needs DRM and uinput -- derives a session key from it and
passes only that to the session process over its local IPC:

    K = HKDF-SHA256(secret,
                    salt = host_nonce || guest_nonce,
                    info = "vmlord.display.v1.session" || session_id)

Compromising the session process yields one session's key, not the VM's
identity, and no new long-lived secret exists to be minted, delivered, guarded
or deleted.

Authentication is mutual, and it has to be. The host now connects to a socket
inside the guest, so any process in the guest can squat the port before the
real service does; the host must be able to tell the real service from a
squatter, exactly as the guest must be able to tell VMLord from anything else
that reached the socket.

Four control records open a session:

1. `ClientHello` (host): version `major`/`minor`, offered capabilities,
   `session_id` (16 random bytes), `host_nonce` (32 bytes), the wanted mode
   and geometry.
2. `ServerHello` (guest): the chosen version, the agreed capabilities, the
   modes and tile sizes it supports, `guest_nonce` (32 bytes).
3. `ServerAuth` (guest): `HMAC-SHA256(K, "server" || H)`.
4. `ClientAuth` (host): `HMAC-SHA256(K, "client" || H)`.

where

    H = SHA256("vmlord.display.v1.transcript"
               || len32(ClientHello) || ClientHello
               || len32(ServerHello) || ServerHello)

over the payload bytes **as they arrived on the wire**. Protobuf does not
promise that the same message encodes to the same bytes twice, so a transcript
over a re-encoded message is a transcript two correct implementations can
disagree about. This is also why the tags are separate records rather than
fields inside the hellos: a tag inside `ServerHello` would force one side to
re-encode the message with the field cleared in order to hash it.

Tags are compared in constant time. An early return on the first differing
byte is how a tag gets forged a byte at a time.

**The guest proves first.** The host must not act on an unauthenticated
peer -- not derive channel keys, not show the user a window. A tag harvested by
a fake host is worth nothing: it is a MAC over a transcript whose nonces will
be different next time.

## Version and capabilities

The rules are the agent protocol's, and are the same rules for the same
reasons: differing majors leave nothing to negotiate and the session is
refused with `UNSUPPORTED_VERSION`; a session between differing minors runs at
the lower one, so the older side decides how new the conversation can be; the
agreed capabilities are the intersection, and a capability number this build
has never heard of is dropped rather than refused, which is what lets a newer
guest talk to an older host at all. A peer that agrees on a capability the
other side never offered is refused, because it has claimed the session may
carry messages nothing here answers.

`vmlord-display-protocol` implements these rules itself rather than depending
on `vmlord-agent-protocol`. Sharing twenty lines of negotiation would tie two
contracts that must be versioned independently: a display major must not drag
the agent's schema with it.

## Channel binding

A channel key is derived from the session key and the transcript:

    CK = HKDF-SHA256(K, info = "vmlord.display.v1.channel" || H || channel)

The frame and input channels each run a three-record exchange of their own:
the host sends `ChannelHello { session_id, channel, generation, nonce_c }`,
the guest looks the session up by `session_id` and refuses one it does not
know, answers `ChannelAck { nonce_s, tag_s }`, and the host closes with
`ChannelAuth { tag_c }`. Both tags are HMACs under `CK` over the channel and
both nonces.

Because `CK` depends on `H`, a socket cannot be carried over from another
session or offered by a process that did not take part in the control
handshake.

`generation` counts reconnections of the frame and input channels within a
live session. Records carrying a stale generation are rejected from the header
without reaching a decoder or an input device.

## Messages

**Control, host to guest:** `SetMode`, `SetResolution`, `RequestKeyframe`,
`Ping`, `EndSession`.
**Control, guest to host:** `DisplayState` (what was actually applied),
`Pong`, `Error`.

Liveness lives here and only here. Since the frame stream is not
acknowledged, `Ping`/`Pong` with a timeout is what separates a slow viewer
from a dead one.

**Frame, guest to host:** `StreamConfig`, `Keyframe`, `TileDelta`,
`CursorImage`, `CursorPosition`.

`StreamConfig { width, height, tile_size, pixel_format }` must precede the
frames it describes. Geometry travels in the frame stream rather than on the
control channel because a resolution change and the frames that follow it
would otherwise race between two sockets; in-band, they are ordered by
construction. It is also what lets the host compute the frame cap before
accepting bytes. The protocol does not look inside `Keyframe` or `TileDelta`;
it negotiates `tile_size` but never interprets it.

**Input, host to guest:** `KeyEvent`, `PointerMotion` (absolute, in guest
pixels), `PointerButton`, `PointerScroll`, `ReleaseAll`.
**Input, guest to host:** `Error`.

Letterbox and scaling stay in the viewer (#120); what crosses this wire is
always a guest pixel.

## Flow control

The guest regulates the stream and the host says nothing about its rate. The
encoder keeps a bounded queue (#116): a newer frame displaces an older one
that has not been sent, so what is queued is always current state. When the
socket blocks, the guest accumulates damage and sends one delta covering it
when the socket drains. A viewer that falls behind receives one fresh frame,
not a backlog of stale ones, and latency bounds itself.

Credit-based flow control was rejected: it adds a round trip to the hot path
and does not remove the need for the dropping queue, so it is a second
mechanism on top of the one that does the work.

The two back edges that do exist are not rate control. `RequestKeyframe` is
recovery -- a decoder that has lost synchronisation has nothing to apply a
delta to -- and `Ping`/`Pong` is liveness.

## Modes

`Auto`, `Desktop` and `Motion` exist in the contract. The guest announces what
it supports in `ServerHello`, and the MVP guest announces exactly one:
`Desktop`. `SetMode(Motion)` is answered with `Error(UNSUPPORTED_MODE)`.

`Auto` names a host-side policy, and in the MVP that policy resolves to
`Desktop`. Documentation says so plainly: a deferred capability must not be
made to look like an implemented one.

## Recovery

**Control is lost: the session is over.** The guest stops capturing and
releases everything it holds, the host closes the other two sockets, and the
viewer shows its waiting state and starts again with a new `session_id`.

**Frame is lost:** it reconnects within the same session at `generation + 1`,
and `StreamConfig` and a `Keyframe` must be the first records on it. A delta
has nothing to apply to.

**Input is lost:** the guest performs a release-all on its own, without
waiting to be asked -- a key stuck down is worse than a lost session -- and the
host sends `ReleaseAll` as the first record after the channel is
re-authenticated.

## Errors

An enumeration, not strings: `UNSUPPORTED_VERSION`, `UNAUTHENTICATED`,
`UNKNOWN_SESSION`, `CHANNEL_BINDING_FAILED`, `MALFORMED_RECORD`,
`RECORD_TOO_LARGE`, `CHECKSUM_MISMATCH`, `UNSUPPORTED_MODE`,
`RESOLUTION_REJECTED`, `CAPTURE_FAILED`, `INTERNAL`, with optional diagnostic
text beside the code.

Errors travel on the control channel. On the frame and input channels a fatal
error closes the socket, and the reason follows on control if control is still
up.

## Threat model

Authentication is an event of the handshake. After it, the stream is neither
encrypted nor authenticated per record, and the spec states this as a decision
rather than leaving it as a default.

This is a point-to-point stream inside the hypervisor, not a network. Injecting
into an established HvSocket stream requires a privilege under which
everything else is already lost, and confidentiality here is provided by the
partition boundary. A MAC on every frame would cost a standing percentage of
CPU in the hot path against a threat this transport does not have. The agent
protocol makes the same trade for the same reason: HMAC in the handshake, and
a plain stream after it.

What the handshake does defend against is the threat the reversed connection
direction introduces: a process inside the guest squatting the service port,
and anything on the host that reaches the socket. Both are answered before a
single frame or keystroke moves.

## Public surface

- **framing** -- header encode and decode, per-channel caps, CRC, and reads
  that distinguish a peer that closed cleanly from a truncated stream.
- **messages** -- the generated `vmlord.display.v1` types, with a checked-in
  descriptor set and a test that fails when it stops matching the `.proto`, as
  in `vmlord-agent-protocol`.
- **`Session`** -- a transport-free state machine with a host role and a guest
  role, which consumes an incoming record and produces an outgoing record and
  a decision, keeps the transcript, derives the channel keys, and verifies
  bindings.

The state machine is a departure from the agent protocol, which exposes
negotiation as free functions and leaves the sequence to its callers. The
agent's handshake is one exchange over one socket, with nowhere to drift.
This one is three sockets, two directions of proof, a transcript hash and a
channel binding; if #115 writes the guest half and #117 writes the host half,
they will drift over exactly what must not drift -- what goes into the hash and
in what order. One machine, two roles, and both ends agree by construction.

No sockets, no threads, no async, no `unsafe`.

## Tests

- **Round-trip:** every record encodes and decodes to itself.
- **Compatibility:** a newer minor talks to an older one at the older minor;
  an unknown capability is dropped; an unoffered capability is refused; a
  differing major is refused.
- **Malformed and oversized:** a corpus of a wrong `header_len`, a length over
  the cap, a length over the geometry-derived frame cap, trailing bytes, a
  truncated Protobuf body, a bad CRC, a stale generation, an unknown
  `session_id`, a tag of the wrong length, and a forged tag.
- **Golden:** the checked-in bytes of a complete handshake and one record of
  each type, so that an unintended change to the format fails a test rather
  than a VM.
- **Fuzz:** the header parser and `Session`, which must neither panic nor
  release channel keys on arbitrary input.

## Out of scope

The codec's byte format (#116), the guest services (#115), the viewer (#117),
input semantics and focus policy (#119), dynamic resolution UX (#120), the
HCS service table entries (#121). Audio, clipboard, multi-monitor, the Motion
codec and zero-copy are not in v1, and nothing is reserved for them beyond the
mode enumeration.
