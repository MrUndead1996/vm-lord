# Which DRM backend VMLord's display stack gets

The decision behind task #111, and the measurements it rests on. Every
number here comes from a real VMLord VM: Ubuntu 24.04.4, kernel
6.8.0-137-generic, GPU off, Secure Boot off.

The probe that produced them -- a staged shell script and a C capture tool --
was research, and is not carried in the tree. It lives in history at
`ead0c20`, under `spikes/task-111-drm/`, along with the full logs of every
run; `git show ead0c20:spikes/task-111-drm/probe.sh` brings it back. What
that probe found, and the two frames it captured, are here.

## The decision

**VMLord ships its own minimal DRM module**, in the shape of AppSandbox's
`asb_drm`: one CRTC, one connector with a synthesized EDID, a primary plane
and a cursor plane, atomic modesetting, hrtimer vblank, GEM shmem buffers,
PRIME export. Delivered as DKMS. `hyperv_drm` stays as the pre-boot console
and is displaced once our module loads.

Neither stock candidate can be the desktop's output device:

| | simpledrm | hyperv_drm | vkms | ours |
| --- | --- | --- | --- | --- |
| present in the cloud image | yes (builtin) | no -- `linux-modules-extra`, 113 MB | no -- same package | DKMS |
| GDM binds it before login | yes | yes | **no** | yes |
| resolution | one mode, fixed | what the host declared at VM start | 10..8192 | ours to choose |
| resizable while running | no | no | yes | yes |
| cursor plane | no | no | yes | yes |
| writeback connector | no | no | yes | optional |
| framebuffer readable by a capture client | yes | yes | yes | yes |

`vkms` is the shape we want and it is disqualified by one line of udev:

    /usr/lib/udev/rules.d/61-mutter.rules:116
    ENV{ID_PATH}=="platform-vkms", TAG+="mutter-device-ignore"

The match is on `ID_PATH`, which udev builds from the platform device's
name. Mutter refuses vkms for being called vkms -- not for being virtual,
not for lacking a render node. A device on the same bus under another name
matches nothing in that file. That is the entire distance between "in-tree,
free" and "write our own", and it is also why our module must not be named
after vkms or registered on the faux bus, which `asb_drm`'s own comments
already warned about.

`hyperv_drm` fails on two counts that no configuration fixes. Its mode list
is whatever the host declared in the HCS `VideoMonitor` section, fixed for
the life of the VM -- a guest cannot resize its own display, which a remote
desktop must do. And it exposes a single Primary plane, so mutter logs
"Couldn't find suitable cursor plane format" and falls back to a software
cursor.

## What VMLord must change either way

`VIDEO_WIDTH`/`VIDEO_HEIGHT` in `crates/platform/src/hcs_config.rs` are the
constants 1024 and 768, and they are the guest's ceiling until our module
loads: the console, the initramfs, the boot splash and any GDM that comes
up before the module is in place all run at that size. Editing them in a
VM's `config.json` to 1920x1080 and restarting moves the whole mode list
with them, so this is a number that belongs in the VM's configuration, not
in a constant.

## The boundary between DRM output and the capture backend

The kernel module exports nothing capture-specific. Capture is an ordinary
DRM client:

1. open `/dev/dri/card*` -- **without** taking DRM master, which the
   compositor holds;
2. `DRM_CLIENT_CAP_UNIVERSAL_PLANES`, then walk the planes;
3. per plane with an fb: `drmModeGetFB2` -> `drmPrimeHandleToFD` ->
   `mmap(PROT_READ, MAP_SHARED)`;
4. `DMA_BUF_IOCTL_SYNC` around each read.

This is proven, not projected: a non-master root process read the live GDM
greeter out of mutter's framebuffer while mutter was running, at 0.27 ms
per 1024x768 XRGB8888 frame -- copy only, one core. The capture is
[`greeter-1024x768-simpledrm.png`](display-drm-backend/greeter-1024x768-simpledrm.png).

Consequences for the module's design:

- **The cursor must be composited by whoever captures.** With a cursor
  plane present, mutter puts the pointer on it and it is absent from the
  primary plane; the capture walks planes and composites. (Where there is
  no cursor plane -- simpledrm, hyperv_drm -- mutter draws the pointer into
  the primary plane and capture gets it for free. That is the one thing the
  poorer drivers do better.)
- **No writeback connector is needed.** Writeback is driven by atomic
  commits, and a capture client that is not DRM master cannot commit.
  `asb_drm` returns `-ENOSYS` from its writeback hook for exactly this
  reason and walks planes instead. vkms has a real writeback connector and
  it would still be unusable from outside the compositor.
- **The module must not set `DRIVER_CURSOR_HOTSPOT`.** Mutter hides the
  cursor plane of drivers that declare it unless they are on its allowlist
  (qxl, vboxvideo, virtio_gpu, vmwgfx).
- Formats stay XRGB8888/ARGB8888 with `DRM_FORMAT_MOD_LINEAR` only: a
  capture that mmaps the buffer cannot detile anything else.

## Privileges and packaging

- `drmModeGetFB2` requires **CAP_SYS_ADMIN**. Membership in `video` is not
  enough -- the capture service runs privileged, or with that one
  capability.
- The module needs `dkms` and `linux-headers-$(uname -r)`: three packages
  on top of a VMLord VM, and the headers are already present in guests
  that got the desktop.
- VMLord VMs boot with **Secure Boot disabled** and kernel lockdown
  `[none]`, so an unsigned DKMS module loads. If Secure Boot is ever turned
  on for VMLord VMs, the module needs a MOK-enrolled signature and this
  decision needs revisiting.
- `simpledrm` is builtin (`CONFIG_DRM_SIMPLEDRM=y`), so blacklisting it is
  a no-op; it has to be unbound from `simple-framebuffer` by a unit, the
  way `asb_drm`'s deploy does.

## Target releases

Ubuntu 22.04, 24.04 and 26.04 amd64 are proven by the native display parity
matrix. GEM shmem helpers and plane helper signatures move between their
kernels, so each release still builds the same DKMS source against its own
headers. See [display compatibility](display-compatibility.md) for the supported
user-facing matrix; this document records the original backend decision.

## What the module has to guard by kernel version

The four the PoC needed, and the exact reason for each. `asb_drm` was
written against 26.04's kernel; three of these are compile errors on 6.8 and
5.15, and the fourth only appears at runtime.

```c
/* platform_driver::remove returned int until 6.11, and between 6.1 and 6.10
 * the void-returning callback lived in ::remove_new. */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 11, 0)
	.remove = asb_remove,
#elif LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
	.remove_new = asb_remove,
#else
	.remove = asb_remove_int,   /* thin int-returning wrapper */
#endif

/* hrtimer_setup() replaced hrtimer_init + function assignment in 6.15. */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 15, 0)
	hrtimer_setup(&asb->vblank_timer, asb_vblank_timer_fn,
	              CLOCK_MONOTONIC, HRTIMER_MODE_REL);
#else
	hrtimer_init(&asb->vblank_timer, CLOCK_MONOTONIC, HRTIMER_MODE_REL);
	asb->vblank_timer.function = asb_vblank_timer_fn;
#endif

/* Renamed from DRM_PLANE_HELPER_NO_SCALING in 6.1. */
#ifndef DRM_PLANE_NO_SCALING
#define DRM_PLANE_NO_SCALING DRM_PLANE_HELPER_NO_SCALING
#endif

/* drm_version() copies drm_driver::date unconditionally until 6.14 removed
 * the field. Left NULL it WARNs in drm_copy_field() and hands userspace a
 * NULL string, which segfaults drm_info -- and nothing catches it at build
 * time. */
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 14, 0)
	.date = "20260820",
#endif
```

This is the DKMS bill in concrete form: four kernel APIs moved across the
three releases VMLord means to support, and the module has to straddle all
of them.

## The proof of one virtual output

Done, on the target guest. `asb_drm` built through DKMS on 24.04's 6.8 (with
four version guards, below),
displaced `hyperv_drm`, and came up as `/dev/dri/card1` on
`/sys/devices/platform/asb_drm.0` with a render node beside it. udev tagged
it `master-of-seat:seat:uaccess` and nothing else -- no
`mutter-device-ignore` -- and mutter 46.2 bound it before login:

    Added device '/dev/dri/card1' (asb_drm) using atomic mode setting

A non-master root process then read the greeter off it: primary plane
1920x1080 XRGB8888 at 0.73 ms a frame, cursor plane 256x256 ARGB8888 in a
buffer of its own.
[`greeter-1920x1080-asb_drm.png`](display-drm-backend/greeter-1920x1080-asb_drm.png)
is that frame, and the pointer is missing from it -- it lives on the cursor
plane now, which is exactly the composition this document says the capture
backend owes us.

So the shape holds end to end on the first proven target. What remained was
not research, and this is where each of those four stands:

- the module's own name and packaging under VMLord, rather than AppSandbox's
  -- **done** in task #113: `vmlord_drm`, shipped as the DKMS package
  `vmlord-display` inside a versioned display payload;
- the mode list above 1920x1080 -- **done** in task #114. The connector offers
  the standard list up to 2560x1440, and a `width`/`height` outside
  640x480..2560x1440 is refused with a warning and falls back to 1920x1080.
  The size is no longer pinned in a file the payload carries: `vmlord-agent`
  writes the modprobe.d options from the mode the host has stored for that one
  VM;
- 22.04 and 26.04 builds -- **still open**. 5.15 now compiles on the
  development machine after every change to the module, which makes 22.04 the
  best-evidenced of the three and still not a booted guest. 6.8 and 7.x compile
  only in `payloads/display/prepare.sh`'s container, and the runtime proof for
  all three is task #128's mandatory matrix;
- the capture backend itself -- **built** in task #115, and unproven in a guest.
  What exists is two programs in `crates/display-services`, both static musl
  binaries. The privileged one opens the card by driver name rather than by
  number, takes `DRM_CLIENT_CAP_UNIVERSAL_PLANES` without ever becoming DRM
  master, waits on the vblank the module's hrtimer drives, walks the planes with
  `GETPLANERESOURCES`/`GETPLANE`/`OBJ_GETPROPERTIES` and reads each framebuffer
  with `GETFB2` -- which is the one call that needs `CAP_SYS_ADMIN`, and the
  whole reason the privileged half is a separate process. Each framebuffer is
  exported once with `PRIME_HANDLE_TO_FD` and **without** `DRM_RDWR`, so what
  the unprivileged half receives is a buffer it cannot write. The cursor plane
  task #114 added is composited by `cursor::place`/`cursor::composite`, which
  crop rather than clamp, since `CRTC_X`/`CRTC_Y` go negative at the left and
  top edges while the protocol's coordinates do not; a peer that took
  `CAPABILITY_CURSOR_STREAM` gets the cursor as its own records instead.

  What is proven is only what a development machine can prove: the ioctl
  request numbers and structure widths are checked against this machine's own
  `drm.h` and `drm_mode.h`, the cursor arithmetic and the frame pipeline are
  covered by unit tests, and the session process is driven end to end against a
  real `Session::host` over socketpairs. **No guest has run either binary.**
  There is no vsock loopback on the development kernel, so even the listener
  test skips; the runtime proof is task #128's mandatory matrix, along with a
  real mutter putting the pointer on the cursor plane, GDM before login, and
  2560x1440.
