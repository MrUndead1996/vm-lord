# Keyboard and mouse input design

## Purpose

Task #119 makes the display interactive. Everything the events travel over
already exists: `#118` defined the input channel, its five records and its
authentication; `#115` binds the input socket in the guest and reads its
records; `#117` binds the same socket in the viewer and already sends
`ReleaseAll` as the first record after every bind. Two ends are missing. In
the guest, `read_input` decodes nothing and drops what it reads -- there is no
`/dev/uinput` device to put the events on. In the viewer, no key press or
mouse movement ever becomes a record: the window sees one message,
`WM_LBUTTONUP`, and only to find the two buttons on the failed screen.

What this task delivers is those two ends and the policy between them: which
events are sent, when they are sent, and what is guaranteed to happen when
they stop being sent. The last is the part that matters most. A remote desktop
that drops a key event has a glitch; one that leaves a key held has a guest
typing `aaaaaaaa` into a password field with no way to stop it.

Letterboxing, fullscreen and resolution changes remain #120's. This task
introduces the one value that the mapping needs -- where the picture sits on
the client area -- and computes it as today's crop, so that #120 replaces one
function rather than two copies of the same arithmetic.

## Decisions

### Scan codes, not virtual keys

The viewer derives the evdev keycode from the keyboard's **scan code**, not
from the Windows virtual-key code.

AppSandbox's `appsandbox-input.c` translated `VK_*`, and that is the one part
of it not worth preserving. A virtual key is the result of applying the
*host's* layout; the guest then applies its own. On a non-US host layout the
`VK_OEM_*` keys land on the wrong guest keys, and `Ctrl`+letter breaks wherever
the two layouts disagree on where that letter lives. A scan code is a position
on the keyboard, evdev keycodes are essentially the same set-1 map, and the
translation is a table. The layout stays entirely the guest's business: it
decides what sits at that position.

The source is `lParam` of the keyboard message -- bits 16-23 are the make
code, bit 24 is the extended flag -- and the same fields of `KBDLLHOOKSTRUCT`
under the hook. Two sequences need naming rather than table lookup: `PrtScn`
arrives as `E0 2A E0 37` and `Pause` as the `E1` pair, and both collapse to one
keycode (`KEY_SYSRQ`, `KEY_PAUSE`).

### A low-level hook while the window has focus

Windows never delivers `Super`, `Alt+Tab`, `Ctrl+Esc` or `Alt+Esc` to a window:
the shell takes them first. For a GNOME desktop that is a real loss --
`Super` is the key that opens Activities.

The viewer therefore installs a `WH_KEYBOARD_LL` hook on `WM_SETFOCUS` and
removes it on `WM_KILLFOCUS`. The API is documented and needs no elevation.
The hook runs on the thread that installed it, which is the message pump --
the one thread in the process where, by construction, nothing blocks. That is
not a coincidence to rely on quietly: a low-level hook that thinks for longer
than `LowLevelHooksTimeout` is silently removed by the system, so the callback
does one thing, hands the event to the state machine, and returns.

While the hook is installed the keyboard belongs to the guest, `Alt+F4`
included. Two things are therefore reserved and never forwarded:

* **`Ctrl+Alt+Left Shift`** releases the keyboard: the hook comes off,
  Windows behaves normally again, and the guest is sent `ReleaseAll`. Neither
  GNOME nor Windows binds it.
* **`Ctrl+Alt+Del`** the hook cannot see at all. It is the Secure Attention
  Sequence, the kernel refuses to route it, and the epic forbids reaching for
  undocumented means. It is a viewer action instead (below).

### One privileged creator, two devices, one owner of each descriptor

`/dev/uinput` needs root; the session process runs as `vmlord-display`. The
broker creates the devices and hands their descriptors to the session over the
existing `SCM_RIGHTS` socket, and the session writes `input_event` structures
into them directly.

The alternative -- the broker keeping the descriptors and the session
forwarding events to it -- was rejected. It withholds no authority worth
withholding, because relaying whatever the host sends is precisely the
session's job; it puts a hop and a second message format on the path of every
mouse movement; and it makes the broker responsible for remembering what was
held when the session dies.

Handing the descriptor over buys the crash guarantee for free. When the
session process dies its descriptors close, the kernel unregisters the uinput
device, and `input_dev_release_keys` sends a release for everything held. "No
stuck keys after a crash" is then a property of the kernel rather than of our
diligence.

Two devices, `VMLord Keyboard` and `VMLord Pointer`, not AppSandbox's single
hybrid node. libinput classifies a device by its capability bits, and a node
carrying keys, `ABS_X`/`ABS_Y` and `BTN_TOUCH` at once is resolved by
heuristics that have changed between releases. This MVP must behave the same
on 22.04, 24.04 and 26.04; two unambiguous nodes remove the heuristic from the
question. The cost is one extra `UI_DEV_CREATE` and one extra descriptor.

The devices are created once when the broker starts and live for the guest's
lifetime. Neither an input-channel rebind nor a whole new session recreates
them, so the desktop never sees its keyboard disconnect mid-session.

### A fixed absolute range

`ABS_X` and `ABS_Y` are declared `0..32767` permanently, and the session scales
the record's guest pixels into that range using the geometry it already holds.

Declaring the range as `0..width-1` would mean recreating the device on every
resolution change -- which #120 makes an ordinary event -- and GNOME would see
the pointer disappear and reappear each time. At 2560 pixels the fixed range is
twelve steps per pixel, so nothing is lost. This is also what AppSandbox did,
and the one part of its device setup worth keeping unchanged.

### Both wheel resolutions

`PointerScroll` carries hundred-and-twentieths of a detent. The pointer emits
`REL_WHEEL_HI_RES`/`REL_HWHEEL_HI_RES` with that value unchanged -- the kernel's
unit for those axes is exactly 1/120 -- and additionally emits the accumulated
whole `REL_WHEEL`/`REL_HWHEEL` detents, keeping the remainder, so that slow
scrolling is not lost to applications that read only the discrete axis.

### Input failure is not a VM failure

If `/dev/uinput` is absent or `UI_DEV_CREATE` fails, the broker logs it, sends
the host a `Report`, and carries on without input devices. The session then
discards input records exactly as it does today. A display without a keyboard
is worth more than a VM that refused to show one, and this matches the rule
#114 set for the DRM side: a failure degrades the display, it does not break
the VM.

## Architecture

### Guest

**`crates/display-services/src/uinput.rs`** (new). Hand-rolled ioctls in the
style of `drm/uapi.rs`; `libc` is already a dependency and no C toolchain is
added.

Two halves in one module, split by who may call them:

* `create()` -- opens `/dev/uinput` twice, sets the capability bits, runs
  `UI_DEV_SETUP`, `UI_ABS_SETUP` and `UI_DEV_CREATE`, and returns the two
  `OwnedFd`s. The broker's, because it needs root.
* `Keyboard<W>` and `Pointer<W>` over an already-created descriptor, with
  `key`, `button`, `motion`, `scroll` and `release_all`. The session's.

Both are generic over `Write` so that a test can put a `Vec<u8>` where the
device goes and assert the exact byte stream: the `SYN_REPORT` closing each
group, the scaling of coordinates, the wheel's carried remainder.

Each keeps the set of what it currently holds, which is what `release_all`
releases -- no more, and nothing it never pressed. A keycode outside the
declared set is counted and dropped rather than written.

**`ipc.rs` and the broker schema.** One new message, `InputDevices`, sent by
the broker in answer to `Attach`, carrying the two descriptors. Descriptors are
named by the message they arrive on rather than by position, as `Snapshot`
already does. Frame buffers never ride on this message, so the existing
descriptor ceiling is untouched.

**`broker_main.rs`.** Creates the devices at startup, alongside waiting for the
card, and answers `Attach` with them. A failure here is a log line and a
`Report`, not an exit.

**`session_main.rs`.** `read_input` stops discarding. The generation filter
stays first, unchanged. Then the record is decoded by `message_type` and
applied: `KeyEvent` to the keyboard, `PointerMotion` scaled and applied to the
pointer, `PointerButton` and `PointerScroll` to the pointer, `ReleaseAll` to
both. A type this build has no name for is counted and ignored, per the
protocol's forward-compatibility rule. `close_input`, for whatever reason it is
called, calls `release_all` on both devices before anything else.

### Viewer

**`src/placement.rs`** (new, portable). `place(stream, client) -> Placement`
and `Placement::to_guest(x, y) -> Option<(u32, u32)>`. Today `place` returns
the crop the renderer already performs -- origin at the top left, size the
smaller of frame and client area. `#120` replaces that one function with
letterboxing and both consumers follow. `d3d.rs`'s `blit` takes its region from
`Placement` instead of computing its own, so a second copy of the arithmetic
never exists. A point outside the placement maps to `None`: today that is
"past the edge of a cropped picture", tomorrow "in a black bar", and the code
is the same either way.

**`src/input.rs`** (new, portable). Two things, neither of which touches a
Windows API:

* the set-1 to evdev table, including the `E0` page and the two `E1`
  sequences;
* the policy state machine. It is fed raw window facts (focus gained/lost,
  pointer moved to a client point, button down/up, wheel, hook key event,
  channel lost) and produces protocol events. It holds what is pressed, so
  that every `ReleaseAll` it emits is preceded by knowing what to release; it
  coalesces motion, keeping only the newest position per drain; it keeps the
  pointer stream alive while a button is held even after the cursor leaves the
  picture, so that a drag inside GNOME does not break at the window edge.

Tested on any platform, which is most of this task's behaviour.

**`src/windows/hook.rs`** (new). Installs and removes `WH_KEYBOARD_LL` and
routes its callback into the state machine. The only new `unsafe` in the
viewer besides the window messages themselves.

**`window.rs`.** Handles `WM_MOUSEMOVE`, the six button messages,
`WM_MOUSEWHEEL`, `WM_MOUSEHWHEEL`, `WM_SETFOCUS`, `WM_KILLFOCUS` and
`WM_MOUSELEAVE`; subscribes to the last through `TrackMouseEvent`, which is
the only way it arrives; calls `SetCapture` while a button is held and
`ReleaseCapture` when the last one lifts. Every one becomes a `UiEvent::Input`
and nothing is decided on the pump.

**`main.rs`.** The pump feeds those into the state machine and sends what
comes out as `Order::Input`. `drive` starts draining the whole order queue each
pass instead of taking one order per iteration -- with a 2 ms sleep in the
loop, one-at-a-time would throttle pointer motion to 500 events a second and
add latency under exactly the load that matters.

**`live.rs`.** `send_input(event)` writes the record with the channel's next
sequence and current generation. A write that fails closes the input socket;
the existing bind path reconnects it at the next generation, independently of
the frame channel, and the `ReleaseAll` that already opens every bind covers
whatever was held when it broke.

### Viewer actions

The window's system menu (`GetSystemMenu` plus `AppendMenuW`, arriving as
`WM_SYSCOMMAND`) gains two items:

* **Send Ctrl+Alt+Del** -- three presses and three releases, in order.
* **Release keyboard** -- the visible equivalent of `Ctrl+Alt+Left Shift`,
  for a user who does not know the combination.

## Data flow

A key travels: hook or window message -> scan code and extended flag ->
`input.rs` table -> `KeyEvent` -> `Order::Input` -> session thread ->
`live.send_input` -> input channel record -> guest `read_input` -> generation
check -> `Keyboard::key` -> `EV_KEY` plus `SYN_REPORT` -> the guest's evdev
device -> libinput -> mutter.

A click travels the same path with one step more at the front: the client
point passes through `Placement::to_guest` first, and a point that maps to
`None` is dropped before it becomes a record.

## Failure and recovery

| What happens | What the user sees |
| --- | --- |
| Window loses focus | Hook removed, `ReleaseAll` sent, keyboard back to Windows |
| `Ctrl+Alt+Left Shift` | The same, deliberately |
| Cursor leaves the picture, nothing held | Motion stops being sent; buttons already up |
| Cursor leaves the picture, button held | Stream continues until the button lifts |
| Input channel write fails | Socket closed, rebound at the next generation, `ReleaseAll` first |
| Input records from an old generation | Rejected at the header, never reach a device |
| Session process killed | Descriptors close, the kernel releases every held key |
| `/dev/uinput` unavailable | Display works, input does not, host gets a `Report` |

## Testing

Automatic:

* the scan-code table on known codes, both pages and both `E1` sequences;
* the policy machine: focus gained and lost, hover in and out, a drag that
  leaves the picture and returns, release-all on every path that owes one,
  motion coalescing;
* `placement` at the edges, at odd geometries, and outside the picture;
* `Keyboard` and `Pointer` byte streams: event grouping and `SYN_REPORT`,
  coordinate scaling, wheel accumulation and remainder, a keycode outside the
  declared set;
* the existing `session_main` harness, extended to assert that a record sent
  on the input channel reaches the device -- and that one from a stale
  generation does not.

By hand, as the task requires, and stated so it is not mistaken for covered by
the above: logging in through GDM, and confirming no key is held after a
disconnect and after `kill -9` of the session process.

Not covered here: a real Hyper-V partition, a real GNOME session under load,
and input latency figures. Those are #128's matrix.

## Out of scope

Letterbox and the placement `#120` will compute, fullscreen, dynamic
resolution and saved window state (#120); Connect wiring and diagnostics
(#121); the E2E matrix and performance gates (#128); clipboard, audio and
multi-monitor, which are not in v1 at all. Relative pointer mode and pointer
confinement are not planned: the device is absolute, which is what a windowed
remote desktop wants.
