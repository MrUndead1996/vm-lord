# `vmlord_drm`

VMLord's own virtual display: the DKMS sources a display payload carries, and
what a guest builds to get a `/dev/dri/card*` a compositor will bind.

## What it is

One CRTC, one connector, a primary plane and a cursor plane, GEM shmem buffers,
an hrtimer vblank, atomic modesetting and PRIME export. Nothing scans out: the
framebuffer a compositor commits *is* the product, and VMLord's capture service
reads it as an ordinary DRM client -- `drmModeGetFB2`, `drmPrimeHandleToFD`,
`mmap` -- without taking DRM master.

The connector offers **one** mode, between 640x480 and 2560x1440, marked
preferred. Its size at load comes from the `width` and `height` parameters,
which `vmlord-agent` writes into `/etc/modprobe.d/vmlord-display.conf` from the
mode the host has stored for one VM; a size outside the bounds is refused with
a warning and falls back to 1920x1080. There is no vblank hardware to be in
phase with, so the timer is the output's only clock -- and without one a
compositor is never paced.

While the module runs, that size is `/sys/module/vmlord_drm/parameters/mode`,
written as `WxH` by the display broker when the host's window is resized. A
write validates the bounds, moves the preferred mode and hotplugs the
connector; the compositor, which is the DRM master, is what actually commits
the mode. Offering exactly one mode is what makes that commit certain: a
connector that kept the standard list would leave a compositor free to stay on
the mode it was already on, which is a window that was resized and a desktop
that was not. What it costs is a guest-side resolution picker -- and a picture
that disagreed with the window is what there would be if there were one.

Three properties are decisions rather than style, all three measured by task
#111 (`docs/display-drm-backend.md`):

* it is a platform device named `vmlord_drm`, not on the faux bus and with no
  `vkms` anywhere in its name -- `61-mutter.rules` matches `ID_PATH` and would
  tag it `mutter-device-ignore`;
* it does not set `DRIVER_CURSOR_HOTSPOT`, which mutter reads as a reason to
  hide a driver's cursor plane;
* its formats are XRGB8888 and ARGB8888 with `DRM_FORMAT_MOD_LINEAR` only,
  because a capture client that mmaps a buffer cannot detile anything else.

## What it is not

* **No guest-side mode list.** The host's window is the authority on this
  output's size; see above.
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
