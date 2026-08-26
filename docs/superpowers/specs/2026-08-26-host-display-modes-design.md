# Host Display Modes Design

## Purpose

Task #136 makes the virtual DRM connector describe the useful modes of the
physical Windows monitor that contains the viewer. A mode is a width, height,
and integer refresh rate in hertz. The viewer may still request dynamic sizes
after a window resize, but GNOME and the viewer can also choose a real host
mode explicitly.

The first version supports one virtual output, host refresh rates through
144 Hz, and the existing 640x480 through 2560x1440 codec and DRM bounds. It
does not copy or synthesize the host EDID. Manufacturer identity, HDR, audio,
colour metadata, physical timings, and the host panel's physical dimensions do
not describe VMLord's virtual connector and remain out of scope.

## Mode model and fallback

`DisplayMode` on the display wire contains `width`, `height`, and
`refresh_hz`. A valid mode has non-zero refresh, satisfies the existing DRM
geometry bounds and CVT alignment, and does not exceed 144 Hz. Lists are
deduplicated by all three fields and sorted deterministically by pixel area,
width, height, then refresh.

The viewer enumerates the monitor nearest its window with
`MonitorFromWindow`, obtains its device name with `GetMonitorInfoW`, and walks
that device with `EnumDisplaySettingsW`. Windows exposes an integer
`dmDisplayFrequency`, so the protocol deliberately uses integer hertz rather
than pretending that the source distinguishes 59.94 from 60 Hz.

The current desktop mode is not assumed to be native. The viewer maps the
monitor to an active DisplayConfig target with `QueryDisplayConfig` and asks
`DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE)`
for the target's preferred width, height, and timing. Failure to resolve that
optional preference does not discard modes obtained from
`EnumDisplaySettingsW`.

Fallback selection is deterministic:

1. retain the selected mode when it is still in the new list;
2. otherwise choose 1920x1080@60 when the list contains it;
3. otherwise choose the greatest available resolution and, among its refresh
   variants, the greatest refresh rate;
4. if enumeration fails or filtering leaves no modes, publish and choose the
   synthetic 1920x1080@60 mode.

The same order applies at first connection and reconnect, with a persisted
per-VM selection considered before step 2. Fullscreen requests the selected
monitor's native mode when Windows reports a valid one; otherwise it uses the
same fallback order.

## Host monitoring and viewer UI

The window layer exposes a platform-neutral snapshot containing the monitor
identity, current mode, preferred/native mode, and normalized modes.
`WM_DISPLAYCHANGE`, a
move, or a DPI transition marks the snapshot stale. The main viewer loop
debounces monitor changes for 250 ms, matching resize debounce, and sends a new
list only when its normalized value changes.

The viewer's resolution control lists `width x height @ refresh Hz` entries.
Choosing one sends an explicit selection and persists it in the existing
per-VM viewer state. Resizing a restored window remains supported: after the
existing debounce it requests an admissible dynamic geometry at the refresh
rate of the selected mode. If that exact geometry is absent from the host list,
the guest may expose it as an additional temporary mode so the window remains
the authority for dynamic resolution.

## Protocol

The protobuf schema adds a display-timing message and two append-only control
records:

- `SetAvailableModes` carries the complete normalized list and preferred mode;
- `SetDisplayMode` carries an explicit width, height, and refresh selection.

The existing `SetResolution` remains valid for peers that only know dynamic
resolution and retains its width and height fields. Negotiation adds a
`HostDisplayModes` capability. A peer without it follows the task #120 path and
uses 1920x1080@60 semantics. Protocol minor version, descriptor, compatibility,
malformed-input, and golden fixtures change together.

The guest validates every received mode independently. An invalid entry
rejects the update rather than silently weakening the host's list; an empty
list is replaced with 1920x1080@60. Bounds are enforced both in the broker and
the module.

## Broker and DRM module

The broker owns the requested list and selection. It writes the normalized
list to a new writable module parameter and writes the active selection to the
existing `mode` parameter. Updating either causes the connector to be reprobed
with a hotplug event. The parameter format is a bounded comma-separated list
of `WIDTHxHEIGHT@HZ` values; its maximum item count and byte length are fixed
and checked before parsing or allocation.

`get_modes` creates every advertised timing with `drm_cvt_mode`. The active
selection is marked `DRM_MODE_TYPE_PREFERRED`. The virtual physical size keeps
the existing 96-DPI policy and follows the preferred geometry. The CRTC vblank
timer already derives its period from the committed mode and therefore follows
60, 120, or 144 Hz without a second clock source.

Replacing the available list does not claim that a mode has committed. The
capture framebuffer remains the authority for geometry, and the first frame of
a changed geometry remains a keyframe. The active refresh is reported over the
control channel once the guest observes the committed DRM mode; geometry-only
legacy peers continue to work.

## FPS gap diagnostics and settings

Application settings add a serde-defaulted display section with
`fps_gap_threshold_percent`. Its default is 50 and valid range is 1 through
100. The settings UI edits it as a percentage and all new visible text is
translated in both catalogues.

The setting is copied into the viewer launch parameters. The viewer measures
delivered FPS from successfully decoded and presented complete frames over a
rolling ten-second interval. It compares that value with the confirmed active
DRM refresh, not the requested refresh. No measurement is made while the
session is negotiating, reconnecting, waiting for a keyframe, minimized, or
without an active stream.

When delivered FPS stays strictly below the configured percentage of DRM
refresh for ten seconds, the viewer sends one warning through the existing
launch pipe. The application-side display worker converts it to a
`Subsystem::Display` user diagnostic naming the VM, active mode, DRM refresh,
measured FPS, and configured threshold. The warning is armed again only after
FPS recovers to or above the threshold for a complete measurement interval.
Ordinary samples remain `tracing` records and never enter the diagnostics
panel.

## Failure and compatibility behaviour

- A Windows enumeration error keeps the previous usable list; without one it
  uses 1920x1080@60.
- A monitor transition never clears a working mode before its replacement has
  passed validation.
- A rejected list or selection is a control error and does not end the display
  session.
- Reconnect resends the current host list and preferred selection before new
  user mode requests.
- A module parameter write failure produces a display diagnostic and leaves
  the last committed stream visible.
- Limits prevent an untrusted host message from creating an unbounded mode
  list or driving codec geometry outside its negotiated capacity.

## Testing and documentation

Portable Rust tests cover normalization, ordering, fallback, persistence,
protocol round trips, old-peer compatibility, bounds, reconnect replay, and
the hysteretic FPS-gap state machine. Windows tests cover `DEVMODEW` conversion
through a pure adapter and message/debounce behaviour without requiring a
particular monitor. Broker tests use temporary parameter files.

Kernel parsing and mode-list behaviour are covered by payload build tests and
the existing supported-kernel build matrix. Manual verification uses a
2560x1440 monitor with multiple refresh variants and checks GNOME mode listing,
viewer selection, fullscreen preference, input mapping, keyframes, monitor
movement, reconnect, and one-shot/rearmed low-FPS diagnostics.

`ARCHITECTURE.md` is updated to replace the single-mode task #120 contract with
the multi-mode contract while retaining dynamic resize and framebuffer truth.
