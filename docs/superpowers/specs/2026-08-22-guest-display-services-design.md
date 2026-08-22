# Guest display services design

## Purpose

Everything the display stack needs inside a guest now exists except the two
programs that connect it. Task #114 gave the guest an output with a cursor
plane and a real vblank; #116 gave a codec that turns framebuffers into the
frame channel's payloads; #118 gave the wire contract and the session state
machine; #113 gave a versioned payload with an empty `content/services/`
directory reserved for exactly this task.

This task fills that directory. Two programs: a privileged broker that owns the
DRM device and the VM's secret, and an unprivileged process that captures,
encodes and speaks the frame and input channels. When it is done, a host that
connects to the guest's three vsock ports sees an authenticated session and a
live desktop.

The Windows viewer is #117, keyboard and mouse are #119, resolution changes are
#120, and wiring Connect through the application is #121. Nothing here knows
those exist beyond the contract they share.

## Decisions

* **Two processes, and the privileged one is small.** `drmModeGetFB2` requires
  `CAP_SYS_ADMIN`; encoding a 2560x1440 desktop at sixty frames a second does
  not. The broker holds the capability and the secret; the process that runs
  hot holds neither.
* **The broker never touches the frame or input sockets.** It hands out
  `ChannelKey`s and the unprivileged process binds its own channels, which is
  what `Session::channel_key`'s own documentation describes -- "for the process
  that owns that socket".
* **Pixels cross the privilege boundary as read-only dma-bufs**, passed by
  `SCM_RIGHTS` and cached by `fb_id`. Root exports descriptors; it never
  copies a frame. The alternative -- root copying into shared memory -- puts
  850 MB/s of memcpy at 1440p60 inside the privileged process and closes the
  door on #122's zero-copy path, since a memfd is nothing a GPU encoder can
  take.
* **Input is listened to and dropped.** The input channel completes its
  handshake and its records are decoded and discarded; `/dev/uinput` is not
  opened. #119 adds the broker's second family of typed operations and the
  consumer. A guest that answers only two of three ports would make #117 and
  #121 wait for #119.
* **No damage in the MVP.** A non-master DRM client has no cheap source of it,
  the encoder compares tiles anyway, and a "the fb id did not change" gate
  would freeze the picture whenever a compositor draws into the same buffer
  twice. `CapturedFrame` carries the field; capture always reports `None`.
* **Services are built by the host toolchain, not in the payload container.**
  A static musl binary is identical for 22.04, 24.04 and 26.04; the container
  exists to prove the module compiles against a release's headers, and a Rust
  toolchain inside it would be a third toolchain for no gain.
* **The payload's protocol range becomes a checked claim.** `pack` refuses an
  archive whose recipe range does not contain the version this build speaks.

## Process topology

Two binaries from one crate, `crates/display-services`, both statically linked
against musl, both carried in `content/services/`.

`vmlord-display-broker` runs as root. It finds the card whose
`device/driver` link names `vmlord_drm` -- not by number, since a guest with
`hyperv_drm` also has a `card0` -- waits for it to appear rather than failing
when it has not, reads `/etc/vmlord/agent.secret`, listens on `VMLD`
(`0x564D4C44`), drives `Session::guest` and then holds the control channel. It
owns the DRM device and the vblank clock.

`vmlord-display-session` runs as the system user `vmlord-display` with an empty
capability bounding set. It listens on `VMLF` (`0x564D4C46`) and `VMLI`
(`0x564D4C49`), binds those channels with the keys the broker gives it,
composites, encodes and writes frames.

The guest listens and the host connects, which is #118's decision and the
opposite of the agent protocol: a session lives as long as a viewer window, so
no viewer means no connection and no capture.

## The broker's socket

One root-owned `AF_UNIX`/`SOCK_SEQPACKET` socket at `/run/vmlord/display.sock`,
mode 0660, group `vmlord-display`. `SOCK_SEQPACKET` is chosen for its message
boundaries: `SCM_RIGHTS` is attached to a datagram, so no framing of our own is
needed. Every `accept` checks `SO_PEERCRED` and closes a peer whose uid is not
the service user's -- on every connection, not once at startup.

The operations are typed and few. This is a private interface between two
binaries shipped in one payload at one version, so it is not versioned and
carries no negotiation; it is prost-encoded because the workspace already has
prost and a hand-rolled encoding would be a second way to describe messages.

| Direction | Message | Meaning |
| --- | --- | --- |
| session to broker | `Attach` | The process introduces itself; the broker answers with the current state. |
| broker to session | `SessionOpened` | `session_id`, `Negotiated`, `frame_key`, `input_key`. |
| broker to session | `SessionClosed` | Control was lost or `EndSession` arrived. Stop capturing, release everything. |
| session to broker | `NextFrame` | Asks for the next snapshot. The reply arrives at the next vblank; other messages may arrive before it. |
| broker to session | `FrameSnapshot` | Sequence, per-plane layout, and a dma-buf for each buffer the session has not seen. |
| broker to session | `KeyframeRequested`, `ModeChanged`, `ResolutionChanged` | Relayed control records. |
| session to broker | `Report` | What the session applied or failed at, so the broker can send `DisplayState` or `Error`. |

There is no "give me the device" and no raw ioctl passthrough. A dma-buf
exported without `DRM_RDWR` is a read-only buffer, not control of a device.

## Capture

The broker opens the card without taking DRM master -- the compositor holds it
-- and sets `DRM_CLIENT_CAP_UNIVERSAL_PLANES`. Its clock is
`DRM_IOCTL_WAIT_VBLANK` with a relative count of one: #114's hrtimer, which
also numbers the snapshots.

Each tick it walks the planes and, for those with an `fb_id` on our CRTC,
reads:

* `DRM_IOCTL_MODE_GETFB2` for size, format, modifier, pitches, offsets and the
  GEM handle;
* `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` on the plane for `CRTC_X`, `CRTC_Y` and
  the `SRC_*` values, because `GETPLANE` does not report position.

The handle becomes a descriptor through `DRM_IOCTL_PRIME_HANDLE_TO_FD` with
`O_CLOEXEC` and without `DRM_RDWR`, and the handle is closed with `GEM_CLOSE`
immediately, so that `GETFB2` does not accumulate handles in the broker's file.
Descriptors are cached by `fb_id` and dropped when an `fb_id` leaves the walk.

Formats are `XRGB8888` and `ARGB8888` with `DRM_FORMAT_MOD_LINEAR` only, which
is what #111 fixed and #114 implements. Anything else is not a session failure
but an `Error(CAPTURE_FAILED)` with a plain reason: it means the module or the
compositor changed underneath us.

All DRM ioctls live in one Linux-specific module with `unsafe` allowed for the
crate the way `crates/agent` allows it, and no system `libdrm` is added: the
structures are the kernel's uapi, written out by hand, and a linked libdrm
would cost the toolchain-free cross-compilation that `cargo agent` and
`cargo display-services` both rest on.

### The cursor

The module deliberately does not set `DRIVER_CURSOR_HOTSPOT`, so no hotspot is
readable -- and none is needed. Mutter places the plane where the image is
drawn, so the hotspot reported to the viewer is `(0, 0)` and the position is
`CRTC_X`/`CRTC_Y`. Those are signed and go negative at the left and top edges,
because #114 gives the cursor plane `can_position`; the session process crops
the bitmap by the rows and columns that fall outside and clamps the reported
position to zero. A cursor plane with no `fb_id` is a hidden cursor:
`CursorPosition { visible: false }`.

What happens next depends on the handshake. With `CURSOR_STREAM` agreed the
cursor travels as its own `CursorImage` and `CursorPosition` records -- the
bitmap only when the cursor plane's `fb_id` changes, the position only when the
coordinates change -- and is not mixed into the primary plane, so a viewer moves
the pointer without waiting for a frame. Without it, the same bitmap is
alpha-blended into the captured frame before `Encoder::submit`. Both paths are
implemented: the first is what #117 will use, the second is what makes the
guest honest about a capability it offered and the peer declined.

### The captured frame

One type crosses out of the DRM module:

```rust
pub struct CapturedFrame {
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub damage: Option<Vec<Rect>>,
    pub backing: Backing,
}

pub enum Backing {
    /// `mmap(PROT_READ, MAP_SHARED)` over the dma-buf, with
    /// `DMA_BUF_IOCTL_SYNC` around every read. Unmapped and closed on drop.
    Cpu(MappedBuffer),
}
```

`Backing` is an enum with one variant rather than a comment promising one: it
is where #122 adds a descriptor handed on without being mapped. No second
variant appears in this task.

## The session

The broker accepts `VMLD`, runs `Session::guest` to `Event::ControlEstablished`
and builds its `Support` from facts rather than wishes:

* `capabilities`: `CURSOR_STREAM`, because the cursor plane exists, and
  `DYNAMIC_RESOLUTION`, because the mode list exists. Applying a resolution
  change is #120; until then `SetResolution` is answered with a `DisplayState`
  carrying the current geometry, which is a true statement about what was
  applied.
* `modes`: `Desktop` alone. `SetMode(Motion)` is `Error(UNSUPPORTED_MODE)`, and
  `Auto` is resolved by the session machine to the guest's first mode.
* `tile_sizes`: the three the codec can produce.
* `width`, `height`: what the CRTC is at.

It then takes `channel_key(Frame)`, `channel_key(Input)`, the `Negotiated` and
the `session_id`, and sends `SessionOpened`.

The unprivileged process listens on both its ports for its whole life, not from
the moment a session opens: the host may connect to the frame port before
`SessionOpened` arrives, and such a connection waits for a key rather than
being refused. It binds each channel itself with three records and
`keys::channel_tag`: `ChannelHello` with our `session_id` and a generation
strictly greater than the last, `ChannelAck` with its nonce and tag, and the
host's `ChannelAuth`, compared in constant time. A wrong `session_id`, a
generation that did not advance or a tag that does not check closes the socket,
and the reason goes to the host on control through the broker.

### What the guest owes on a disconnect

Three obligations the protocol crate cannot enforce:

* **Control is lost:** the broker sends `SessionClosed`; the session process
  stops capturing, drops the encoder, closes its frame and input sockets and
  releases every mapping and descriptor. There is no session without control.
* **Frame reconnects:** a new generation, sequences from zero, and
  `StreamConfig` and a keyframe before anything else. This is a state, not an
  intention: the encoder is constructed fresh for the new socket and
  `request_keyframe` is called before the first `next_payload`.
* **Input drops:** the guest performs release-all itself without waiting to be
  asked. Under this task's scope there is nothing held, and the handler says so
  in one journal line rather than pretending to act; it is the same socket-close
  handler #119 hangs the real release-all on.

### Flow control

Nothing acknowledges a frame, and the loop is built for that. One `poll`: the
broker descriptor always, the frame descriptor for writability only while
unwritten bytes remain. A `FrameSnapshot` becomes `Encoder::submit`, which
displaces an unencoded frame and accumulates its hint, and `NextFrame` goes out
again. Encoding happens only when the socket has drained: `next_payload` yields
frame, cursor bitmap, cursor position, in that order, and they are written as
`FRAME_RECORD_*` with `base` set to the sequence of the record a delta builds
on. A viewer that falls behind receives one current frame, not a backlog.

`RequestKeyframe` and `Ping` arrive on control: the broker relays the first and
answers the second itself, without waking capture.

### Threads

The broker runs three: the control loop on vsock, the IPC accept loop, and the
vblank loop. The session process runs two: the `poll` loop above, and one
thread for the input socket, whose blocking read has nothing to do with frames.

## Packaging

`cargo display-services` builds
`-p vmlord-display-services --target x86_64-unknown-linux-musl --release`. The
crate stays out of `default-members` for the reason `crates/agent` does: it is
a guest program that cannot be built for the host target.

`payloads/display/prepare.sh` gains a required `--services <directory>` and,
after the container step, copies the two binaries and the units into
`prepared/content/services/`. Its clean-tree check widens from
`payloads/display` to include `crates/display-services`, because the commit in
`sources.json` now describes the binaries too, and a commit that describes half
a payload is the thing that check exists to prevent.

`cargo xtask display-payload pack` links `vmlord-display-protocol` and refuses
to pack when `handshake::CURRENT_VERSION` falls outside the recipe's `protocol`
range. The host already declines a catalog entry whose range does not cover its
version; this makes the guest's half of that claim checked at packing rather
than discovered in a VM.

### Units

`vmlord-display-broker.service`: root,
`ConditionPathExists=/etc/vmlord/agent.secret`, `Restart=on-failure`,
`RestartSec=2`, `StartLimitIntervalSec=60`, `StartLimitBurst=5`,
`CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_OVERRIDE`,
`RestrictAddressFamilies=AF_VSOCK AF_UNIX`, `NoNewPrivileges=yes`,
`ProtectHome=yes`, `ProtectSystem=strict` with `/run/vmlord` writable.

`vmlord-display-session.service`: `User=vmlord-display`, an empty
`CapabilityBoundingSet`, the same restrictions, and
`After=vmlord-display-broker.service` without `BindsTo` -- a broker restart must
not take the session down, and the session reconnects to the IPC socket with
backoff.

Waiting for DRM and for GDM happens inside the broker rather than in unit
dependencies. A card appears after the module loads, and failing into a restart
while it has not would spend the crash-loop budget on a normal state.

### The recipe

`SERVICES` stops being `Skipped`. It creates the system user (`getent`, then
`useradd --system --shell /usr/sbin/nologin`), compares the sha256 of the
payload's files with what is installed under `/usr/local/lib/vmlord/`, skips
the copy when they match and says so, and otherwise copies the binaries,
installs the units, runs `daemon-reload` and enables them.

`SERVICES_START` restarts both units and waits, within the short budget, for
both to be active and for the broker's socket to exist.

A failure in either stage is `Failed`, ends the recipe and leaves the display
degraded while the VM keeps running -- #113's machinery, unchanged. And
`verify()` gains the half that was reserved for this task: the same two checks,
so a payload update now rolls back on broken services and not only on a broken
module.

## Tests

The development machine is WSL2, so musl binaries run natively and
`cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl` runs
ordinary tests rather than cross-compiling blind.

Genuinely covered:

* **The privilege boundary.** Real `AF_UNIX`/`SOCK_SEQPACKET` sockets: a peer
  with the wrong uid is refused by `SO_PEERCRED` rather than by file mode, and
  `SCM_RIGHTS` is exercised with a `memfd` in place of a dma-buf -- the kernel
  does not care which descriptor it is, and what matters is that the control
  message is built and parsed correctly and that no descriptor leaks.
* **Cursor composition and cropping.** A pure function: alpha blending,
  negative `CRTC_X`/`CRTC_Y` at the left and top edges, a cursor clipped by the
  right edge, a hidden cursor. A property test that composition never writes
  outside the frame.
* **The whole frame pipeline without DRM.** The loop takes `CapturedFrame`
  values rather than a source, so a test feeds synthetic frames and puts
  `Session::host` from the same crate on the other end -- a real host, not a
  stub. Asserted: `StreamConfig` and a keyframe are first on a new socket; a
  delta's `base` names a record that was sent rather than a frame that was
  captured; a slow socket produces one delta covering everything accumulated
  rather than a queue; a frame reconnect at `generation + 1` starts again with
  `StreamConfig` and a keyframe; losing control stops capture and closes both
  sockets; a record from a stale generation is refused.
* **The recipe stages**, the way `display_kernel.rs`'s other stages already are:
  a stand-in filesystem and stand-in commands, idempotency when the digests
  match, and a failed stage that reports `Failed` rather than panicking.

Not covered here, and belonging to #128 -- stated plainly so it is not mistaken
for done: a real mutter putting the pointer on the cursor plane; GDM before
login; 2560x1440; kernels 6.8 and 7.x; behaviour under systemd across crash
loops and restarts; and every FPS, latency, CPU and memory threshold.
`cargo display-bench` measures the codec and says nothing about the cost of
capture, which can only be measured in a guest.

The `AF_VSOCK` listener can be tested locally only if WSL has `vsock_loopback`
and `VMADDR_CID_LOCAL`. That is checked during implementation; if the module is
absent the listener test runs over `AF_UNIX`, and the fact that vsock binds at
all remains #128's to prove. It is recorded here as unproven rather than
proven.

## Out of scope

* `/dev/uinput` and any input consumer (#119).
* Applying a resolution change (#120).
* Registering the three HvSocket services in the HCS configuration (#121).
* Zero-copy capture (#122), the Motion codec (#123), audio (#124), clipboard
  (#125), multi-monitor (#130).
