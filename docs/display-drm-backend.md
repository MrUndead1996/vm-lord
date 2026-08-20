# Which DRM backend VMLord's display stack gets

The decision behind task #111, and the measurements it rests on. Every
number here comes from a real VMLord VM: Ubuntu 24.04.4, kernel
6.8.0-137-generic, GPU off, Secure Boot off. The raw logs and the captured
frame are in `spikes/task-111-drm/results/`.

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
`spikes/task-111-drm/results/greeter-24.04-simpledrm.png`.

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

Ubuntu 24.04 amd64 is proven, and is the first target. 22.04 (5.15) and
26.04 are untested for the module build: the GEM shmem helpers and the
plane helper signatures move between those kernels, so each release needs
its own build, which is what DKMS is for.

## The proof of one virtual output

Done, on the target guest. `asb_drm` built through DKMS on 24.04's 6.8 (with
`spikes/task-111-drm/asb_drm-kernel-compat.patch`, four version guards),
displaced `hyperv_drm`, and came up as `/dev/dri/card1` on
`/sys/devices/platform/asb_drm.0` with a render node beside it. udev tagged
it `master-of-seat:seat:uaccess` and nothing else -- no
`mutter-device-ignore` -- and mutter 46.2 bound it before login:

    Added device '/dev/dri/card1' (asb_drm) using atomic mode setting

A non-master root process then read the greeter off it: primary plane
1920x1080 XRGB8888 at 0.73 ms a frame, cursor plane 256x256 ARGB8888 in a
buffer of its own. `spikes/task-111-drm/results/poc-greeter-1080p-asb_drm.png`
is that frame, and the pointer is missing from it -- it lives on the cursor
plane now, which is exactly the composition this document says the capture
backend owes us.

So the shape holds end to end on the first proven target. What remains is
not research:

- the module's own name and packaging under VMLord, rather than AppSandbox's;
- the mode list above 1920x1080 -- this run pinned the module to that size
  through its modprobe.d file, so the ceiling is still unmeasured;
- 22.04 and 26.04 builds (the patch compiles clean against 5.15, which is
  evidence, not proof, for 22.04);
- the capture backend itself, which is task #9's next step, not this one.
