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
