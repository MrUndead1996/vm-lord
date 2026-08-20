# What two Ubuntu 24.04 guests answered

Two VMLord VMs, both Ubuntu 24.04.4 (kernel 6.8.0-137-generic), GPU off, no
`/dev/dxg`, Secure Boot off, lockdown `[none]`. The two runs agree line for
line, so what follows is one guest's answer, confirmed twice.

## The display device is not the one we assumed

There is no `hyperv_drm` in this image, and no `vkms` either:

    modinfo hyperv_drm -> Module hyperv_drm not found
    modinfo vkms       -> Module vkms not found
    /boot/config-6.8.0-137-generic: CONFIG_DRM_HYPERV=m, CONFIG_DRM_VKMS=m

The kernel builds both, the image ships neither -- they live in
`linux-modules-extra`, which `linux-image-virtual` does not pull in. What
drives the screen instead is the builtin `simpledrm`, bound to the
firmware framebuffer:

    [drm] Initialized simpledrm 1.0.0 for simple-framebuffer.0 on minor 0

So "just use the stock driver" is not a free option on the baseline image.
It is a package first.

## What simpledrm gives, measured

| question | answer |
| --- | --- |
| card exists, udev tags it for seat0 | yes -- `TAGS=:master-of-seat:seat:uaccess:`, `[MASTER] drm:card0` under seat0 |
| GDM binds it before login | yes -- mutter 46.2: "Added device '/dev/dri/card0' (simpledrm) using atomic mode setting" |
| modes up to 2560x1440 | no -- one mode, `1024x768`, and no mode setting at all |
| framebuffer readable from outside the compositor | yes -- see below |

Framebuffer size is reported as Width [1024, 4096], Height [768, 4096], but
the single connector offers exactly one mode and simpledrm cannot change it:
the resolution is whatever the firmware framebuffer was, which is VMLord's
`VideoMonitor` 1024x768 from `crates/platform/src/hcs_config.rs`.

One plane only (Primary, XRGB8888, LINEAR), no cursor plane, no overlay, no
writeback connector, no render node -- `Available nodes: primary`.

## Capture works, and this is the proof

A non-master root process read the live greeter's framebuffer through
`drmModeGetFB2` -> PRIME -> `mmap`, while mutter held DRM master:

    master held by this process: no (a compositor holds it)
    fb 37: 1024x768 XR24 modifier 0x0 pitch 4096
    copy of 3145728 bytes x60: 0.27 ms per frame (3670 fps ceiling, copy only)
    frame content: a picture (37746 of 786432 pixels differ from the first)

`greeter-24.04-simpledrm.png` is that capture. It is the GDM greeter, drawn
by mutter with no GPU at all, with the mouse pointer composited into the
primary plane -- there being no cursor plane, the pointer lands in the
capture for free.

This settles the earlier open question. The blank frame from the first
26.04 run was not the capture path failing; that guest's staged WSL Mesa
was in it. Without GPU-PV, software rendering, mutter draws and the read
sees exactly what is on screen.

## What is still open

- What `hyperv_drm` and `vkms` do once `linux-modules-extra` is installed --
  stage `extra` asks exactly that.
- Whether anything in this stack can be driven above 1024x768.

# What the `extra` stage answered

`linux-modules-extra-6.8.0-137-generic` -- 113 MB, one `apt-get install`, no
reboot -- and both candidates appear. Neither needed a rebuild, both loaded
live.

## hyperv_drm

    hv_vmbus: registering driver hyperv_drm
    hyperv_drm 5620e0c7-...: [drm] Synthvid Version major 3, minor 5
    [drm] Initialized hyperv_drm 1.0.0 for 5620e0c7-... on minor 1

It takes the display over from simpledrm outright: `card0` disappears,
`card1` is the only node left. It sits on vmbus, and udev tags it
`master-of-seat:seat:uaccess` -- seat0 is fine.

Then it stops:

| | |
| --- | --- |
| modes offered | 1024x768, 800x600, 800x600, 848x480, 640x480 |
| framebuffer size | Width [0, 1024], Height [0, 768] |
| 1920x1080 | `failed to find mode "1920x1080" for connector 31` |
| 2560x1440 | `failed to find mode "2560x1440" for connector 31` |
| planes | one, Primary. No cursor plane, no overlay |
| writeback connector | none |
| render node | none -- primary only |

The ceiling is not the driver being coy: 1024x768 is what the host declared
in `VideoMonitor`, and Synthvid hands the guest exactly that. Whether
raising `VIDEO_WIDTH`/`VIDEO_HEIGHT` in `crates/platform/src/hcs_config.rs`
raises this is the one question this guest cannot answer -- it needs a VM
built with a different HCS config.

(`failed to set mode: Permission denied` at 1024x768 is GDM holding DRM
master, not a driver limit.)

## vkms

    [drm] Initialized vkms 1.0.0 for vkms on minor 2

Everything hyperv_drm lacks, vkms has:

| | |
| --- | --- |
| framebuffer size | Width [10, 8192], Height [10, 8192] |
| cursor | a real Cursor plane; DRM_CAP_CURSOR 512x512 |
| writeback connector | yes -- `WRITEBACK_FB_ID`, `WRITEBACK_OUT_FENCE_PTR`, `WRITEBACK_PIXEL_FORMATS` |
| bus | `/sys/devices/platform/vkms` |

And one tag kills it as a display for GNOME:

    TAGS=:...:mutter-device-ignore:master-of-seat:seat:uaccess:

udev marks vkms as a device mutter must not use. This is the filtering
`asb_drm`'s own source comments predicted, seen for real: a virtual KMS
device is refused by name, not by shape. A driver of the same shape under a
different name is not on that list.

## Who tags vkms, exactly

    /usr/lib/udev/rules.d/61-mutter.rules:116:
    ENV{ID_PATH}=="platform-vkms", TAG+="mutter-device-ignore"

Mutter's own udev rule, and it matches on `ID_PATH` -- which udev builds
from the platform device's name. Not the driver, not the topology, not the
absence of a render node: the string `vkms`. A DRM device on the platform
bus under any other name gets `ID_PATH=platform-<name>`, matches no rule in
that file, and is a device mutter is willing to drive.

That is the whole argument for a module of our own over vkms. vkms already
proves the shape works in-kernel -- arbitrary resolution, a cursor plane, a
writeback connector, no hardware behind any of it. It is disqualified by
its name alone.

# The 1024x768 ceiling was ours

`VideoMonitor` in the VM's `config.json` hand-edited to 1920x1080, VM
restarted, nothing else changed. hyperv_drm now offers 23 modes:

    #0 1920x1080 60.00 ... type: preferred, driver
    #1 1680x1050 ... #18 1024x768 ... #22 640x480
    Framebuffer size: Width [0, 1920], Height [0, 1080]

So Synthvid hands the guest exactly what the host declared, and the guest's
ceiling is a number VMLord chooses at create time -- `VIDEO_WIDTH`/
`VIDEO_HEIGHT` in `crates/platform/src/hcs_config.rs`, today a constant.
2560x1440 is still refused (`failed to find mode`), because the host was
told 1920x1080; the limit follows the declaration, it is not fixed at
1024x768 as the first two runs suggested.

What does not change with resolution: hyperv_drm still exposes one plane
(Primary, XRGB8888, LINEAR), no cursor plane, no writeback, no render node,
and the mode set is fixed for the life of the VM -- a guest cannot ask for
more than the host declared at start.

Two more details worth keeping:

- At boot simpledrm comes up first (minor 0) and hyperv_drm takes the
  display from it (minor 1); afterwards only `card1` exists.
- `modetest -s` on hyperv_drm returns `Permission denied` (mutter holds DRM
  master) while the same command on vkms succeeds -- because mutter ignores
  vkms, its master is free. A DRM device GNOME refuses is a device a plain
  userspace process can own completely.

# The PoC's first run: the module does not build on 24.04

DKMS failed before anything reached the display, so the guest rebooted
unchanged -- `hyperv_drm` still on `card1`, no `asb_drm` anywhere.

The cause is not Hyper-V and not the DRM design: the AppSandbox sources
were written against the 26.04 kernel and use API that 24.04's 6.8 does not
have. Reproduced locally against 5.15 headers, three errors, in order:

    asb_drm_drv.c:215: initialization of 'int (*)(struct platform_device *)'
                       from incompatible pointer type 'void (*)(...)'
    asb_drm_mode.c:135: implicit declaration of function 'hrtimer_setup'
    asb_drm_plane.c:83:  'DRM_PLANE_NO_SCALING' undeclared

`platform_driver::remove` got its void return in 6.11, `hrtimer_setup()`
arrived in 6.15, and `DRM_PLANE_NO_SCALING` was renamed from
`DRM_PLANE_HELPER_NO_SCALING` in 6.1. `asb_drm-kernel-compat.patch` guards
all three by version; with it the module builds clean against
5.15.0-190-generic, which is a stricter test than 6.8.

This is the DKMS bill the decision predicted, arriving early: a module of
our own must build on every release VMLord supports, and the kernel API
under it moves. It is also why the PoC builds asb_drm rather than assuming
it.

# The PoC works: GDM draws on a module of ours, with a cursor plane

Second run of `poc`, with the compat patch. asb_drm built through DKMS,
installed, and after the reboot it owns the display:

    asb_drm: loading out-of-tree module taints kernel
    asb_drm asb_drm.0: writeback init failed (-38), continuing without
    [drm] Initialized asb_drm 1.0.0 for asb_drm.0 on minor 1
    asb_drm asb_drm.0: AppSandbox virtual display ready: 1920x1080@60Hz

`hyperv_drm` is blacklisted and gone; `/dev/dri` holds `card1` and, unlike
any stock candidate here, a render node `renderD128`. The device sits at
`/sys/devices/platform/asb_drm.0`, and its udev tags are
`master-of-seat:seat:uaccess` -- **no `mutter-device-ignore`**, which is the
whole point:

    ID_PATH=platform-asb_drm.0
    seat0: [MASTER] drm:card1  ->  card1-Virtual-1
    gnome-shell[1357]: Added device '/dev/dri/card1' (asb_drm)
                       using atomic mode setting

GDM bound it before anyone logged in. The capture, from a non-master root
process while mutter held master:

    planes: 2
      plane 31 (primary): fb 40: 1920x1080 XR24 pitch 7680
      plane 33 (cursor):  fb 42: 256x256  AR24 pitch 1024
    copy of 8294400 bytes x60: 0.73 ms per frame (1371 fps ceiling)
    frame content: a picture (37553 of 2073600 pixels differ)

`poc-greeter-1080p-asb_drm.png` is that frame. Compare it with
`greeter-24.04-simpledrm.png`: the pointer is missing from this one,
because it is on the cursor plane, in its own 256x256 ARGB buffer, which
the capture read separately. That is the composition the capture backend
has to do, and it is now a measured fact rather than a plan.

Costs, measured: 0.73 ms to copy a 1080p frame -- 2.7x the 1024x768 figure,
in line with the pixel count.

## What the run also found

`drm_info` **segfaults** on asb_drm, and the kernel WARNs in
`drm_copy_field()`, because `drm_driver::date` is NULL. Every kernel before
6.14 copies that field unconditionally. The patch now sets it; the fourth
hunk is the one no compile could have caught.

The probe leaned on `drm_info` to name the driver, so that crash also took
out the mode-setting section (`driver under test: none`). It now falls back
to the driver name in sysfs.

Still unmeasured, for want of that section: the mode list asb_drm offers
above 1920x1080. The module was loaded with `width=1920 height=1080` from
its own modprobe.d file, so this run proves the plumbing, not the ceiling.
