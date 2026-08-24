# Display clipboard design

## Purpose

Task #125 gives a display session a clipboard: text, HTML and images copied in
the guest paste on the host, and the other way round. Everything else the
session does is already built -- #118 settled the protocol, #115 made the guest
listen, #117 and #119 made the window interactive, #121 wired Connect -- and
nothing in any of it touches a selection.

The reason this is not a small addition is that the whole display stack is
deliberately session-blind. Frames are read off a DRM device, input is written
to uinput devices, and `vmlord-display-session` runs as the system account
`vmlord-display` with `ProtectHome=yes`. None of those can see a clipboard: a
selection exists only inside a logged-in user's compositor. So a clipboard
needs a component the stack does not have -- a process inside the user's GNOME
session -- and a way to reach it that does not put the VM's secret there.

AppSandbox is the precedent to answer to. `src/backend_win/vm_clipboard.c` and
`tools/linux/agent/appsandbox-clipboard.c` sync text, images, HTML and files
over two vsock ports, focus-gated from the display window. This task matches
that coverage except for files, which are #139.

## What the spike established

The mechanism was proved against a running guest -- the `test` VM, Ubuntu 26.04,
GNOME Shell 50.1, Wayland -- before this design was written, because the guest
half rests entirely on one D-Bus interface behaving in a way no documentation
promises.

`org.gnome.Mutter.RemoteDesktop` is the same interface `gnome-remote-desktop`
uses, and it carries a clipboard: `EnableClipboard(a{sv})`,
`DisableClipboard()`, `SetSelection(a{sv})`, `SelectionWrite(u) -> fd`,
`SelectionWriteDone(u,b)`, `SelectionRead(s) -> fd`, with the signals
`SelectionOwnerChanged(a{sv})` and `SelectionTransfer(s,u)`. It has been in the
interface since GNOME 42, which is the whole compatibility matrix.

What the spike settled, in order:

* `CreateSession` followed by `Start()` succeeds with **no ScreenCast session**.
  A session may exist for the clipboard alone, which is what makes a small
  daemon possible rather than a screen-sharing stack.
* `EnableClipboard({})` makes a client a listener; `EnableClipboard` with
  `mime-types` makes it the owner. Mutter refuses `SelectionRead` on a selection
  the caller owns -- "Tried to read own selection" -- so the two states are
  distinct and the daemon must be in the listening one whenever the guest is
  the source.
* A full round trip crossed: the owning client received
  `SelectionTransfer(mime, serial)`, answered with `SelectionWrite(serial)`,
  wrote to the returned descriptor and called `SelectionWriteDone`; the reading
  client's `SelectionRead` produced those bytes.
* The descriptor `SelectionRead` returns is **non-blocking**. The first read
  returned `EAGAIN` and the bytes arrived later. A synchronous guest daemon
  would either spin or lose data, so the daemon is built around a poll loop --
  which is also the natural place for the size cap and the cancellation this
  task asks for.

GNOME 42 and 46 were not exercised; the interface is the same there, and the
release matrix is #128's business rather than this task's.

## Decisions

### A fourth channel, not a fifth message on control

Clipboard traffic must not delay a frame or a keystroke, which the task states
and which rules out the control channel: control carries the handshake, mode
changes, resolution and liveness, and a 4 MiB image ahead of a `Pong` is a
session that looks frozen. `CONTROL_MAX_PAYLOAD` is 64 KiB anyway.

So the session gets a fourth socket, exactly parallel to the three that exist:

| | port | service GUID |
| --- | --- | --- |
| control | `VMLD` `0x564D_4C44` | `564D4C44-FACB-11E6-BD58-64006A7986D3` |
| frame | `VMLF` `0x564D_4C46` | `564D4C46-…` |
| input | `VMLI` `0x564D_4C49` | `564D4C49-…` |
| clipboard | `VMLC` `0x564D_4C43` | `564D4C43-FACB-11E6-BD58-64006A7986D3` |

`Channel::Clipboard = 4` joins the record header's channel byte, and the entry
is listed on every VM for the reason #121 gave for the other three: a service
table entry is the partition's permission for a service to exist, not a claim
that anything is listening.

The keying needs no new cryptography at all. `keys::channel_key` is already
parameterised by the channel byte, so a clipboard key is derived from the
session key and the transcript the same way a frame key is, and the bind
exchange on the socket is the same three records. What grows is `Session`'s
per-channel array, from two entries to three.

Unlike frame and input, this channel carries records in both directions. That
is not new either -- control does -- and it needs no new counter: each end
numbers the records it writes with its own `sequence`, so two directions on one
socket are two streams that never share a number.

### The guest end is a user daemon, and it binds its own socket

`vmlord-display-session` binds `VMLF` and `VMLI` itself and proves them with
keys the broker sent it over `/run/vmlord/display-broker.sock`. The clipboard
end works the same way, which means no descriptor passing and no new IPC shape:

* a new binary, `vmlord-display-clipboard`, installed beside the other two;
* started by a **user** unit, `WantedBy=graphical-session.target`, because only
  a process in the user's session can reach that user's session bus, and the
  session bus is where Mutter is;
* its unit is installed to `/etc/systemd/user` and enabled with
  `systemctl --global enable`, so it starts in the session of whoever logs in
  rather than for one account the recipe would have to name;
* it connects to a second broker socket,
  `/run/vmlord/display-clipboard.sock`, receives the clipboard channel key, and
  binds `VMLC` on its own.

A separate process rather than a thread of `vmlord-display-session`: the
session process runs as a system account that must not be in a user session,
and a blocking D-Bus call or a guest application that never answers a selection
transfer must not be able to stall capture. Process isolation is what makes
"clipboard traffic does not block frames" a structural property instead of a
promise about scheduling.

It speaks D-Bus with `zbus`, which is pure Rust. The guest binaries are built
for `x86_64-unknown-linux-musl` with no C toolchain, and AGENTS.md forbids
spending that; a GTK or libwayland dependency would.

This is a different route from AppSandbox's, which drives the X11 CLIPBOARD
selection over XCB and relies on Mutter to bridge it to Wayland -- and which
needs XWayland running and steals GDM's `.mutter-Xwaylandauth` cookie to work
at the login screen. The D-Bus route needs neither, and it stops at the login
screen, which costs nothing: there is nothing to copy at GDM.

### The broker gives the channel to whoever is at the screen

The clipboard socket cannot be owned by file permissions the way the session
socket is. The session socket is group-owned by `vmlord-display` because
exactly one system account connects to it; the clipboard daemon runs as
whichever human logged in, and that name is not known when the VM is
provisioned.

So the socket is authorised by peer credentials rather than by mode bits, which
is the check the broker already performs on every accept -- `Listener::accept`
takes the uid it expects and compares it with `SO_PEERCRED`. For this socket
the expected uid is not a constant: it is the uid of the active graphical
session on `seat0`, read from logind at the moment of the accept.

The clipboard therefore belongs to the person sitting at the virtual screen,
and to nobody else. A second user logged in over SSH cannot take it, and a
daemon left running by a user who has since switched away stops being
authorised without anything having to notice and evict it.

### Pull, in both directions

Each side announces what it has and sends nothing until the other asks:

| record | direction | meaning |
| --- | --- | --- |
| `ClipboardOffer{serial, mime_types}` | both | my selection changed; here is what I can produce |
| `ClipboardRequest{serial, mime_type, transfer}` | both | send me that one, as this transfer |
| `ClipboardData{transfer, bytes, last}` | both | a chunk of it |
| `ClipboardCancel{transfer, reason}` | both | that transfer is over and its bytes are not coming |

Pull is what Mutter's own model is (`SelectionOwnerChanged` then
`SelectionRead`), and what Win32 is under its delayed rendering. More
importantly it is what keeps a 30 MiB image copied in the guest off the wire
until somebody actually pastes it on the host.

`serial` names the offer a request answers, so a request that raced a newer
offer is refused rather than answered with the wrong selection. `transfer`
names one transfer for the length of its chunks, which is what `ClipboardCancel`
addresses and what makes cancellation possible at all.

### The formats, and what is refused

| guest mime | Windows format | conversion |
| --- | --- | --- |
| `text/plain;charset=utf-8` | `CF_UNICODETEXT` | UTF-8 ↔ UTF-16LE |
| `text/html` | registered `HTML Format` | the CF_HTML envelope is built and parsed |
| `image/bmp` | `CF_DIB`, `CF_DIBV5` | the `BITMAPFILEHEADER` is added and removed |
| `image/png` | `CF_DIB` | decoded and encoded with the pure-Rust `png` crate |

The three groups are AppSandbox's coverage minus files. `image/png` is there
because GTK applications frequently offer PNG and nothing else, and the host
clipboard has no PNG format that Windows applications read; `image/bmp` is
preferred when the offer carries both, because a DIB is a BMP without its file
header and needs no codec. The PNG codec is a viewer dependency and only a
viewer one: the guest daemon moves whatever bytes Mutter hands it and converts
nothing, so the musl build gains no dependency from this row.

`CF_TEXT` and its CP1252 conversion are not carried. It is the path for
applications that predate Unicode, and every target in the compatibility matrix
is Ubuntu 22.04 or newer against a current Windows.

Two things are refused deliberately:

* **Arbitrary registered formats.** AppSandbox passes any registered clipboard
  format through by name as opaque bytes. This task asks for an allowlist, and
  an allowlist is the point: opaque pass-through is an unbounded channel between
  a guest and its host, offered to whatever either side happens to register.
* **Files.** `text/uri-list` is never offered and an offer that names it is
  ignored, on both sides. File transfer needs a model this design does not have
  -- where received files are written, who removes them, what happens to
  symlinks, to names Windows will not accept, to a cancel halfway through a
  gigabyte -- and it is task #139.

An offer whose mime types are all outside the allowlist is dropped without a
request and without an error record. It is not a fault: a guest copying a
spreadsheet's internal format is behaving correctly.

### Focus is what enables the clipboard

An offer is acted on only while the viewer window holds keyboard focus, which
is the same rule AppSandbox implements from `WM_SETFOCUS` and `WM_KILLFOCUS`
with its `SYNC_ENABLE` message.

The viewer announces the host's selection when its window gains focus, not when
the host clipboard changes in the background; a change while the window is
unfocused is remembered and announced on the next focus. Losing focus cancels
every transfer in flight in either direction. A VM in the background can
therefore neither read what its user copies elsewhere nor quietly replace what
is on their clipboard.

The guest end needs no notion of focus of its own: with no offers arriving and
no requests answered it simply has nothing to do.

### Echo suppression, which is not optional

Without it the clipboard oscillates. The host applies the guest's selection,
Windows answers with `WM_CLIPBOARDUPDATE`, the viewer treats it as a fresh host
selection and offers it back, and the guest does the same in reverse.

Both ends suppress with what they already have rather than a flag:

* the host records `GetClipboardSequenceNumber` immediately after it sets the
  clipboard, and ignores the update whose sequence is that one;
* the guest reads `session-is-owner` out of `SelectionOwnerChanged` -- Mutter
  states whose selection it now is, so an ownership change the daemon caused
  is distinguishable from one an application caused, with nothing to keep in
  sync.

### Limits, and what a transfer may cost

| | limit |
| --- | --- |
| record payload | 64 KiB (`CLIPBOARD_MAX_PAYLOAD`) |
| one text or HTML transfer | 8 MiB |
| one image transfer | 32 MiB |
| offered mime types | 16 |
| transfers in flight, per direction | 1 |
| transfer inactivity | 5 s |

AppSandbox's cap is a flat 64 MiB. That is a number which either never fires or
lets one copy in a guest make a viewer allocate 64 MiB, so the caps here are per
kind and smaller. Exceeding one cancels the transfer with a reason; it does not
fault the channel, because an oversized selection is an ordinary thing for a
user to have.

A new offer supersedes the transfer that was running, which is the second thing
`ClipboardCancel` exists for: copying twice quickly must not queue two
transfers.

### The contents never reach a log

Every log line about the clipboard carries the mime type, the byte count, the
transfer id and the outcome. No line carries a byte of the selection, at any
level, on either side, and the test for it is part of this task rather than a
convention to be remembered.

### The capability says what the build has, not what is attached

`CAPABILITY_CLIPBOARD` is announced by a guest that ships this daemon,
regardless of whether one is connected to the broker at the time.

The alternative -- announcing it only while a daemon is attached -- fails on the
ordinary case. A session commonly opens at the GDM login screen, where there is
no user session and no daemon; the user then logs in, the daemon starts, and
the capability was already settled in a handshake minutes earlier. Since a
capability cannot be renegotiated without a new session, tying it to the daemon
would mean the clipboard never works for anyone who connected before logging
in.

With no daemon attached, the guest sends no offers and drops the host's. That
is the honest behaviour for a screen that nobody is logged into.

## Components

| where | what changes |
| --- | --- |
| `display-protocol` | `Channel::Clipboard`, `ClipboardRecord`, the four messages, `CAPABILITY_CLIPBOARD`, `CLIPBOARD_MAX_PAYLOAD`, a third per-channel slot in `Session` |
| `display-protocol` | a new portable `clipboard` module: the offer/request/chunk/cancel state machine, the allowlist and every limit, with no D-Bus, no Win32 and no socket in it |
| `display-services` | `vmlord-display-clipboard`: the Mutter adapter, the vsock bind, the poll loop |
| `display-services` | the broker's second socket, authorised against the active graphical session's uid, and the clipboard key in what it sends |
| `payloads/display` | `vmlord-display-clipboard.service`, a user unit |
| `agent` | the new binary and unit in the install lists, and the recipe stage that enables it |
| `display-viewer` | the clipboard thread, its message-only window, the Win32 format conversions, focus wiring from the pump |
| `platform` | the fourth service table entry, `clipboard_port` in the launch parameters, the clipboard key in the hand-over |
| `docs` | architecture, compatibility, user guide, troubleshooting |

The state machine in `display-protocol` is the piece that carries the rules, and
it is deliberately on the portable side of the tree: it is the only way both
ends enforce the same limits, and it is what makes the limits testable without
a guest, a compositor or a window.

## Testing

* The state machine: offers, requests, chunking, both caps, the mime allowlist,
  a request against a superseded serial, a cancel mid-transfer, a second offer
  superseding a transfer, an inactivity timeout. Pure unit tests.
* The records: golden encodings, malformed payloads and the fuzz target, beside
  the ones the three existing channels have.
* The bind: a clipboard socket proving itself with a frame key must fail, which
  is the property `channel_key`'s domain separation exists for.
* Conversions: UTF-8 ↔ UTF-16LE with an embedded NUL and a lone surrogate, the
  CF_HTML envelope in both directions, DIB ↔ BMP header munging, a PNG decoded
  and re-encoded.
* The broker's authorisation: a peer whose uid is not the active session's is
  refused.
* No-logging: a capture of the log during a transfer contains none of the
  transferred bytes.
* End to end on the `test` VM, by hand: text, HTML and an image each way, a copy
  during a paste, a copy while the window is unfocused, a guest daemon killed
  mid-session, an oversized selection.

## Out of scope

* File transfer -- task #139.
* `CF_TEXT`, arbitrary registered formats, and any opaque pass-through.
* The clipboard at the GDM login screen.
* Clipboard history, or more than one selection: the primary selection
  (middle-click paste) is not carried.
* The GNOME 42 and 46 legs of the release matrix, which #128 owns.
