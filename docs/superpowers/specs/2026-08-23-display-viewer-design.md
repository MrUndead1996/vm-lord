# Native Windows display viewer design

## Purpose

Task #117 builds the host end of the display stack: `vmlord-display.exe`, a
separate process that opens one display session. Everything it talks to and
everything it decodes already exists -- `vmlord-display-protocol` (#118) is the
wire contract and the session machine, `vmlord-display-codec` (#116) turns the
frame channel's payloads back into pixels, and the guest services (#115)
listen on the three vsock ports and serve an authenticated stream. What does
not exist yet is the program that connects to them, proves itself, and puts
the desktop on screen.

The viewer is a process of its own, not part of VMLord, for the reason the
epic states: a session outlives the application that started it. VMLord can be
closed while a VM's desktop is on screen, and closing it must not take the
desktop down; a crash in either must leave the other standing.

What this task delivers is the binary, the contract it is launched with, and
the tests that hold both still. Wiring Connect through UI -> app -> core ->
platform, registering the three services in the HCS configuration and
launching the real executable from VMLord belong to #121; keyboard and mouse
belong to #119; letterboxing, fullscreen and resolution changes belong to
#120. The viewer is built and proven here against simulated peers -- a real
`Session::guest` and a real encoder in-process, the same way #115 tested its
half -- so that #121 wires a thing that already works.

## Decisions

* **One crate, one binary.** `crates/display-viewer` (`vmlord-display-viewer`)
  produces `vmlord-display.exe`. It sits in `default-members` beside the other
  Windows-side crates and is checked by `cargo check-windows` and tested by
  `cargo test-windows` like them; `cargo dist` collects the release binary.
* **`unsafe` is confined harder than `platform` confines it.** The workspace
  denies `unsafe_code`; this crate re-allows it only inside
  `windows::{window, d3d, hvsocket, ipc}` -- four modules, each behind a door
  that says what crosses it. Everything else is safe Rust, and the safe half
  is where every decision the ticket names lives.
* **The master secret never enters the viewer.** VMLord holds the secret,
  drives the control handshake, and hands the viewer a one-shot derived
  credential once both peers have proved themselves. See *The credential
  boundary*.
* **One session machine, not two.** The viewer drives
  `vmlord_display_protocol::Session` rather than re-implementing its counters;
  the protocol crate gains a documented way to construct the established host
  half from a hand-over. See *The established session*.
* **The guest listens and the host connects**, which is #118's decision. The
  viewer opens three `AF_HYPERV` sockets, each addressed to the VM's runtime
  id and the service GUID the platform already derives from a vsock port --
  `VMLD` control, `VMLF` frame, `VMLI` input.
* **Bounded waits everywhere.** No blocking call without a timeout it chose:
  connect attempts, handshake relays, reads between pings, the EndSession
  grace period. A viewer that cannot reach anything shows its state within
  thirty seconds and keeps pumping messages; nothing hangs.
* **Never log framebuffer content.** Logs carry sizes, sequences, states and
  error codes. There is no screenshot feature in this build, so there is
  nothing to warn about; the rule is stated so that nobody adds one quietly.
* **The renderer uploads damage, not frames.** `apply_keyframe` and
  `apply_delta` return the rectangles that changed; those rectangles are what
  reaches the GPU.

## The credential boundary

The protocol derives a session key from the VM's secret and both nonces, so
nothing short of the secret can take part in a handshake -- and a handshake is
also the only way to obtain the keys the frame and input channels prove
themselves with. The guest resolves this tension by splitting privilege: the
root broker holds the secret and the control socket, and hands the
unprivileged capture process one channel key per socket, good for one session
and no longer. The host resolves it the same way:

1. VMLord builds `Session::host(secret, offer)` and puts the resulting
   `ClientHello` bytes into the viewer's launch parameters.
2. The viewer connects the control socket and sends those bytes.
3. Until the handshake ends, the viewer relays control-channel bytes verbatim
   across its launch pipes; VMLord feeds them to `Session::handle` and sends
   back whatever the machine produces. The viewer shuttles bytes and paints
   `Authenticating`; it parses nothing.
4. On `ControlEstablished`, VMLord derives the two channel keys and sends the
   hand-over: the `Negotiated`, the session id, and a `ChannelKey` each for
   frame and input. Relay mode ends. VMLord drops its `Session`.
5. From there the viewer is autonomous. Ping and Pong, RequestKeyframe,
   EndSession, DisplayState and Error are plain post-handshake records that
   need no key; frame and input rebinds use the handed-over keys at the next
   generation. Closing VMLord takes none of that away, because the secret was
   never needed after step 4.

This is the ticket's "one-shot derived credential": the keys cross an anonymous
pipe whose write end the child inherited -- never a command line, an
environment block, a file, or any other process -- and they die with the
session. It is also why `ChannelKey::{to_bytes, from_bytes}` exist and why
`SessionKey` has no such method: the hand-over is the sanctioned crossing, and
the protocol crate's own documentation describes this exact trade.

A full loss of control ends the session, and starting a new one needs a new
handshake, hence VMLord. The viewer asks it to re-enter relay mode over the
launch pipes; see *Reconnect*. When VMLord is gone, the viewer says Failed
with Retry/Cancel rather than pretending to wait forever -- which is the
honest answer, since the session it lost is gone regardless.

## Launch

VMLord creates two anonymous pipes with inheritable handles and spawns
`vmlord-display.exe` with them wired as the child's stdin and stdout. Nothing
sensitive or structural travels on the command line or in the environment; the
command line stays empty enough to be an accident in a process listing.

The pipes then carry length-prefixed Protobuf messages, schema owned by this
crate (`proto/vmlord/display/v1/viewer.proto`) the way the broker's IPC schema
lives in `crates/display-services`:

| Direction | Message | Meaning |
| --- | --- | --- |
| app -> viewer | `LaunchParameters` | Once, first. Version, VM name, runtime id GUID, the three vsock ports, the offer (mode, tile size, offered capabilities), the IPC token, the first `ClientHello` record bytes. |
| app -> viewer | `RelayToViewer` | Handshake bytes to write to the control socket. |
| viewer -> app | `RelayFromViewer` | Handshake bytes read from the control socket. |
| app -> viewer | `CredentialHandover` | Once per handshake: `Negotiated`, session id, frame key, input key. Ends relay mode. |
| viewer -> app | `RequestRelay` | Asks for a new session: fresh `ClientHello`, relay mode again. |
| app -> viewer | `Command` | `Focus` or `Close`. |

Every message names the protocol revision and the message kind it carries, so
a mismatched pair of processes fails at the first message instead of at a
garbled stream. A viewer launched with no usable stdin -- double-clicked from
Explorer, say -- shows an error window naming the only supported way to start
it and exits; it has no VM to talk to and invents none.

## The established session

`display-protocol` gains a constructor that builds the established host side
of a session from a hand-over -- negotiated parameters, session id, two
channel keys. It sets the same state `Session::handle` would have reached,
and everything downstream is the machine's own logic rather than the
viewer's arithmetic: `open_channel` and `reconnect_channel` produce the
three-record binds, `accept` rejects stale generations from the header,
sequences advance per channel. The alternative -- the viewer keeping its own
generation and sequence counters beside the protocol crate's -- is exactly
the drift the session machine was written to prevent.

With it, the viewer's session thread runs one loop over three sockets:

* **Control.** Ping every five seconds with a monotonically increasing token;
  a Pong overdue by ten seconds marks control dead. `DisplayState` and `Error`
  land in the log. EndSession goes out when the viewer closes (below).
* **Frame.** `StreamConfig` sizes the limits and builds a fresh
  `Decoder`; a second `StreamConfig` replaces both, because geometry never
  changes inside an encoder and the same is true backwards. Keyframes and
  deltas go to the decoder, and the rectangles it returns join a batch bound
  for the render thread. `CodecError::NoBase`, a checksum failure, a malformed
  record or a stale generation all mean the channel cannot continue: close it
  and reconnect at `generation + 1`, which makes `StreamConfig` and a
  keyframe the guest's obligation again. A delta whose base names a record
  this connection never sent is treated the same way -- the picture it would
  produce is wrong in a way no error surfaces.
* **Input.** Bound with its own key and left alone. `ReleaseAll` is sent
  first, per the recovery rule, and harmless on a first bind; further records
  on this channel arrive only when #119 exists.

Channel rebinds retry on a one-second backoff under the same thirty-second
budget every unestablished state runs under (below); control loss is not a
rebind but a new session, and goes through `RequestRelay`.

## Reconnect

The states the overlay shows are the ticket's: Starting, Waiting,
Authenticating, Reconnecting, Failed, and the running picture itself. One
budget governs every path into a non-running state: thirty seconds of active
retry from the moment the state began, then Failed with two working buttons.

| Event | State | What happens |
| --- | --- | --- |
| Spawned, connecting | Starting -> Waiting | Control connect attempts on a short backoff; refusal means the guest service is restarting or absent. |
| Connected, relaying | Authenticating | The handshake runs under a per-record timeout; a refused version or a bad tag lands here as a failure, not a hang. |
| Established | Running | Frames decode; the overlay disappears. |
| Frame or input lost | Reconnecting | Rebind at the next generation; StreamConfig plus keyframe restore the picture, usually within a second. |
| Budget exhausted | Failed | Retry restarts the cycle with a fresh budget; Cancel exits. Buttons stay live because the message pump never stopped. |
| Control lost | Reconnecting | `RequestRelay` over the launch pipes; if VMLord answers, a new session begins; if the pipes are dead, Failed directly. |
| Reset | Reconnecting | Same as control lost: the guest's reboot drops all three sockets, and the viewer rides it until the services return or the budget ends. |
| Partition gone | -- | Connect attempts fail because the compute system no longer exists: a stopped VM is not a failure, and the viewer exits quietly instead of counting out a budget. |

Retry is honest about what it can fix: a guest that is coming back gets
reconnected, a stopped VM closes the window, and everything else lands on a
Failed screen with buttons that work.

## The local IPC and the per-VM mutex

At startup the viewer takes a named mutex, `Local\VMLord.Display.{runtime-id}`,
held for the life of the process. VMLord presses Connect twice and the second
press finds the mutex instead of a second window.

For everything past detection the viewer listens on a named pipe,
`\\.\pipe\vmlord-display.{runtime-id}`, created with the default DACL of the
launching user -- the authentication for `Focus` and `Close`, which do
nothing a same-user process could not do anyway. `Focus` brings the window to
the front; `Close` is the graceful shutdown path below. The pipe server is
owned by the viewer, so it survives VMLord exiting and is found by a later
VMLord instance, which is the repeated-connect case that matters.

Refresh -- a new session after control loss -- is deliberately narrower: it is
answered only over the launch pipes, only with the IPC token
`LaunchParameters` carried, and therefore only by the VMLord instance that
spawned this viewer. A token grants the right to *ask*; the keys still come
from whoever holds the secret. A viewer whose parent is gone and whose session
later dies shows Failed, and a freshly started VMLord resolves it the ordinary
way: Close the stranded window through the pipe, spawn a new viewer.

## Rendering

D3D11 renders; Direct2D writes on top; Win32 owns the window. All three
interactions live in the `windows` modules.

The device holds one `B8G8R8A8_UNORM` texture sized to the current
`StreamConfig`. Decode results arrive as batches of dirty rectangles over a
channel from the session thread; the render thread maps a staging upload per
batch and issues one `UpdateSubresource` per rectangle with a box clipped to
the texture, so a typing desktop moves kilobytes where a naive pipeline moves
a frame. Present is vsync-locked; a static desktop presents nothing new and
costs a present.

The window opens at a default size before the first handshake and takes the
negotiated geometry when it arrives; user resizing stretches the image until
#120 replaces stretching with letterboxing and input mapping. Fullscreen is
#120's.

Device loss -- `DXGI_ERROR_DEVICE_REMOVED` or `RESET` from Present, removal
messages from the swapchain -- is recovered up to three times per session:
recreate device and swapchain, rebuild the texture, request a keyframe, and
wait for the guest to resend what the old device held. A fourth loss in one
session is a Failed state with a reason, because something is wrong that
patience will not fix.

The cursor arrives as `CursorImage` plus `CursorPosition` records when
CURSOR_STREAM is agreed, which the viewer always offers. Each new image
becomes an alpha `HCURSOR` once -- hotspot included, size capped at the
protocol's 256 pixels -- and positions move it; a hidden cursor clears it.
Windows composites the pointer outside D3D, which costs no GPU work and
cannot tear.

## The status overlay

While no frame stream runs, the window shows one word-sized fact about itself
-- Starting, Waiting, Authenticating, Reconnecting -- on a plain dark ground,
with the VM's name beneath it. In Failed the same ground carries the reason
and two buttons, Retry and Cancel, hit-tested by rectangle and drawn by D2D.
The overlay is drawn only while it is shown; the running picture never has UI
on top of it. Window title: `{vm name} - VMLord Display`.

## Lifetime

| Trigger | Result |
| --- | --- |
| User closes the window | EndSession on control, best effort, half a second at most; sockets closed; mutex released; exit. |
| `Close` command on the pipe | The same path. |
| VMLord stops the VM | `Close` arrives on the pipe (#121 wires it); even without it, connect attempts begin failing with partition-gone, which exits quietly rather than showing Failed -- a stopped VM is not a failure. |
| VMLord crashes | Nothing: the session stands on the viewer's sockets. A later VMLord finds the mutex and focuses or closes through the pipe. |
| Viewer crashes | Nothing: the guest sees all three sockets drop, performs its disconnect obligations, stops capturing. A VM is never affected by a dead window. |
| VM reset | Sockets drop together; Reconnecting rides the outage; services return; a fresh session opens. |
| Guest service restart | Frame or input rebinds within the session; a broker restart costs control and becomes a new session. |

## The crate

```text
crates/display-viewer/
  proto/vmlord/display/v1/viewer.proto   launch pipes and commands
  src/
    main.rs        composition: parse launch, choose threads, run the pump
    launch.rs      LaunchParameters, the pipe messages, the token
    status.rs      the state machine behind the overlay and its budget
    session.rs     relay driver, established-session loop, reconnect policy
    video.rs       decoder lifecycle, dirty batches, cursor bookkeeping
    log.rs         file logger; the redaction rule lives here
    windows/
      mod.rs
      hvsocket.rs  AF_HYPERV connect, bounded reads and writes, three channels
      window.rs    class, message pump, resize, focus, hit testing
      d3d.rs       device, swapchain, texture upload, D2D overlay, HCURSOR
      ipc.rs       named mutex, named-pipe server, token check
```

Dependencies beyond the workspace: `windows` with the graphics, WinSock and
pipe features this crate actually names, and the workspace's `log`. No new
runtime dependency enters the tree; the codec and the protocol crate are the
same ones the guest links.

## Tests

Following the repository's conventions: stable Rust, plain `cargo test`,
peers made of real types rather than stubs.

* **Relay.** A test VMLord (real `Session::host`) and a test guest (real
  `Session::guest` plus the real encoder, over in-memory duplex streams)
  bracket the viewer's relay driver: the handshake completes through it, a
  wrong-key guest fails it, a silent guest times out, a parent that dies
  mid-relay aborts the attempt.
* **Established session.** With the hand-over constructor: binds complete at
  generation 0; a rebind increments and refuses stale generations;
  StreamConfig rebuilds the decoder; a delta-first stream triggers
  RequestKeyframe; a checksum failure rebinds the frame channel; ping tokens
  round-trip and a missing pong expires control; EndSession leaves the guest.
* **Status.** Every transition in the table above, including the budget: a
  state that never succeeds lands on Failed after thirty simulated seconds,
  and Retry restarts it.
* **Launch.** Parameters parse and refuse mismatches; empty stdin produces
  the error window's message and nothing else.
* **IPC.** Mutex acquisition conflicts; the pipe loopback delivers Focus,
  Close and a rejected refresh without a token.
* **HvSocket.** Service GUID derivation matches the platform's template for
  the three ports; bounded-read semantics match `hvsocket.rs`'s.
* **Video.** Codec scenes decoded into batches assert the rectangles match
  the encoder's damage and clip correctly at odd geometries; cursor bitmaps
  convert to premultiplied icon data without reading outside their rows.
* **Logging.** A capture-logger assertion that no test stream's pixel bytes
  appear in the log output.

Not covered here, and stated so it is not mistaken for done: a real Hyper-V
partition, a real GDM greeter on the far end, 2560x1440 throughput and
latency, multi-monitor hosts, GPU driver churn. Those are #121's integration
and #128's matrix.

## Out of scope

Keyboard and mouse input (#119); letterbox, fullscreen, dynamic resolution
and saved window state (#120); the HCS service table entries, Connect wiring,
viewer launching and structured diagnostics (#121); the E2E matrix and
performance gates (#128); audio, clipboard, multi-monitor and the Motion
codec, which are not in v1 at all.
