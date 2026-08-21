# `vmlord_drm`

VMLord's own virtual display: the DKMS sources a display payload carries, and
what a guest builds to get a `/dev/dri/card*` a compositor will bind.

## What it is

One CRTC, one connector, one primary plane, GEM shmem buffers, atomic
modesetting and PRIME export. Nothing scans out: the framebuffer a compositor
commits *is* the product, and VMLord's capture service reads it as an ordinary
DRM client -- `drmModeGetFB2`, `drmPrimeHandleToFD`, `mmap` -- without taking
DRM master.

Three properties are decisions rather than style, all three measured by task
#111 (`docs/display-drm-backend.md`):

* it is a platform device named `vmlord_drm`, not on the faux bus and with no
  `vkms` anywhere in its name -- `61-mutter.rules` matches `ID_PATH` and would
  tag it `mutter-device-ignore`;
* it does not set `DRIVER_CURSOR_HOTSPOT`, which mutter reads as a reason to
  hide a driver's cursor plane;
* its formats are XRGB8888 and ARGB8888 with `DRM_FORMAT_MOD_LINEAR` only,
  because a capture client that mmaps a buffer cannot detile anything else.

## What it is not, yet

Task #113 delivers delivery: versioning, verification, DKMS and rollback. The
output itself is task #114, which adds the cursor plane, the mode list up to
2560x1440, a real hrtimer vblank in place of the immediate flush here, and the
behaviour of an output that fails. Until then a compositor draws its own
pointer into the primary plane, which is a working desktop rather than a broken
one.

## Kernels

Ubuntu 22.04 runs 5.15, 24.04 runs 6.8, 26.04 runs 7.x. Two API moves are
guarded at their definition sites in `vmlord_drm.c` -- `platform_driver::remove`
(which returned `int` until 6.11 and lived in `::remove_new` from 6.1) and
`drm_driver::date` (removed in 6.14, and a `WARN` plus a segfault in `drm_info`
if left NULL before that) -- and one in `vmlord_compat.h`:
`DRM_PLANE_HELPER_NO_SCALING` was renamed in 6.1.

Task #111 also measured a fourth: `hrtimer_setup()` replaced `hrtimer_init`
plus a function assignment in 6.15. There is no timer here yet, so the guard
belongs with the vblank work of task #114 rather than as dead code now.

## Building

The build that matters is the one in `payloads/display/ubuntu-<release>-amd64`:
a pinned container per release, whose success is the proof that the module
compiles for that release. Against a local kernel's headers:

```sh
make -C /lib/modules/$(uname -r)/build M=$PWD modules
```
