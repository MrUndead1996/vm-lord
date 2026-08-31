# `vmlord_drm`

VMLord's own virtual display: the DKMS sources a display payload carries, and
what a guest builds to get a `/dev/dri/card*` a compositor will bind.

## What it is

One CRTC, one connector, a primary plane and a cursor plane, GEM shmem buffers,
an hrtimer vblank, atomic modesetting and PRIME export. Nothing scans out: the
framebuffer a compositor commits *is* the product, and VMLord's capture service
reads it as an ordinary DRM client -- `drmModeGetFB2`, `drmPrimeHandleToFD`,
`mmap` -- without taking DRM master.

The connector offers the host monitor's modes, between 640x480 and 2560x1440
at 1 to 144 Hz, with the host's selection marked preferred. What it offers at
load is the single mode the `width`, `height` and `refresh` parameters name,
which `vmlord-agent` writes into `/etc/modprobe.d/vmlord-display.conf` from the
mode the host has stored for one VM; a mode outside the bounds is refused with
a warning and falls back to 1920x1080@60. There is no vblank hardware to be in
phase with, so the timer is the output's only clock -- and without one a
compositor is never paced.

While the module runs, two parameters carry that:

* `/sys/module/vmlord_drm/parameters/modes`, comma-separated `WxH@HZ`, is the
  whole list the connector offers. The display broker writes the normalized
  modes of the monitor the viewer's window is on -- at most 32 of them and at
  most 512 bytes, which are limits the host agrees to rather than discovers.
  The write is parsed into a fixed array and validated in full before the list
  is swapped, so a write this module refuses leaves the guest offering the
  modes it already had rather than half a list.
* `/sys/module/vmlord_drm/parameters/mode`, one `WxH@HZ`, is the mode marked
  preferred. It need not be one of the list: a window being dragged asks for a
  geometry nobody enumerated, and the host's window stays the authority on this
  output's size.

Either write validates the bounds and hotplugs the connector; the compositor,
which is the DRM master, is what actually commits a mode. The preferred mode is
offered first and marked, which is what a compositor follows when a hotplug
takes the mode it was on away.

The smallest *framebuffer* the device accepts is 64x64, which is deliberately
not the smallest mode it offers. `mode_config`'s minimum bounds every buffer
created on the device, a compositor's 256x256 cursor included, so the mode
bounds cannot be reused there: with 640x480 in that field the cursor's ADDFB2
came back `EINVAL`, mutter would not light an output whose cursor plane it
could not fill, and the desktop stayed black (task #131).

Three properties are decisions rather than style, all three measured by task
#111 (`docs/display-drm-backend.md`):

* it is a platform device named `vmlord_drm`, not on the faux bus and with no
  `vkms` anywhere in its name -- `61-mutter.rules` matches `ID_PATH` and would
  tag it `mutter-device-ignore`;
* it does not set `DRIVER_CURSOR_HOTSPOT`, which mutter reads as a reason to
  hide a driver's cursor plane;
* its formats are XRGB8888 and ARGB8888 with `DRM_FORMAT_MOD_LINEAR` only,
  because a capture client that mmaps a buffer cannot detile anything else.

Each plane also carries the immutable `VMLORD_GENERATION` property. Its value
is owned by the driver and advances in `atomic_update`, allowing capture to
distinguish a real compositor commit from the synthetic 60 Hz vblank clock
without reading the full framebuffer.

Beside it, each plane carries the immutable `VMLORD_PLANE_COMMITS` property:
that plane's own commit count. The generation orders commits across the device
but cannot count one plane's -- two updates of the primary and one update of
each plane both move the primary's generation by two -- and capture needs the
count to know whether the damage it is about to read describes every change
since it last looked.

The primary plane also enables the core's `FB_DAMAGE_CLIPS`, which is what a
compositor uses to say what it repainted. Capture reads it as a blob and hands
it to the encoder as a hint, so an idle desktop is not compared eight megabytes
at a time. It is trusted only for the commit immediately after the one already
encoded: damage describes one commit's change against the framebuffer before
it, so a commit nobody read is a change nothing recorded. The property is
`DRM_MODE_PROP_ATOMIC`, so a client sees it only after asking for
`DRM_CLIENT_CAP_ATOMIC` -- as `CRTC_X` and `CRTC_Y` are, which is why capture
asks for the capability even though it commits nothing.

## What is shipped beside it

Two files that are configuration rather than code, both copied into a guest by
`MODULE_LOAD`:

* `vmlord-display-unbind-simpledrm.service` unbinds `simple-framebuffer`, which
  is builtin and so cannot be blacklisted;
* `vmlord-display-compositor-mesa.conf` is a drop-in on
  `org.gnome.Shell@.service` that keeps the compositor on the distribution's
  Mesa. Under GPU-PV the payload's Mesa renders through `/dev/dxg` and cannot
  hand a buffer to a foreign KMS device, so a compositor left on it binds this
  device and then cannot draw on it. Applications keep the GPU; only the
  compositor is moved off it.

## What it is not

* **No mode list of its own.** Every mode this connector offers came from the
  host: the monitor the viewer's window sits on, normalized, plus whatever the
  window is currently asking for. The connector never invents the standard VESA
  list, because a mode the host cannot present is one a guest must not pick.
* **No synthesized EDID.** The connector reports a physical size at 96 DPI and
  no monitor name, so GNOME Settings calls it an unknown display. A hand-built
  128-byte block would fix the name and would cost a fifth version guard across
  an API that moved in 6.7; it is a nicety, deferred.
* **One CRTC and one connector**, so one monitor. Multi-monitor is task #130.
* **No capture.** Task #115's service reads this device from outside, as an
  ordinary DRM client, and composites the cursor plane onto the primary --
  which is what having a cursor plane at all costs.

## Kernels

Ubuntu 22.04 runs 5.15, 24.04 runs 6.8, 26.04 runs 7.x. All four API moves task
#111 measured are guarded. Two live at their definition sites in
`vmlord_drm.c`, because both are struct initializers and an `#if` around a
field reads better than a macro that hides one: `platform_driver::remove`
(which returned `int` until 6.11 and lived in `::remove_new` from 6.1) and
`drm_driver::date` (removed in 6.14, and a `WARN` plus a segfault in `drm_info`
if left NULL before that). Two live in `vmlord_compat.h`:
`DRM_PLANE_HELPER_NO_SCALING` was renamed in 6.1, and `hrtimer_setup()`
replaced `hrtimer_init` plus a function assignment in 6.15 -- the latter as
`vmlord_hrtimer_setup`, because it is a statement rather than a field.

## Building

The build that matters is the one in `payloads/display/ubuntu-<release>-amd64`:
a pinned container per release, whose success is the proof that the module
compiles for that release. Against a local kernel's headers:

```sh
make -C /lib/modules/$(uname -r)/build M=$PWD modules
```

The module reports its payload's version through `MODULE_VERSION`, which is
what `/sys/module/vmlord_drm/version` answers and what the recipe compares an
update against. `Kbuild` defaults `VMLORD_VERSION` to `0.0.0-dev` so the
command above works on a checkout; the Dockerfile rewrites that default with
the payload's real version when it lays a payload out.
