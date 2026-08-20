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
