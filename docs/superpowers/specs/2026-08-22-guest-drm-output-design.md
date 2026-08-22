# Guest DRM output design

## Purpose

Task #113 delivered the display payload: a versioned archive, a DKMS package,
a recipe that builds and loads a module, and a status model that turns every
way that can fail into a named degradation. What it shipped inside that archive
was the smallest module that would load -- one CRTC, one connector, a primary
plane, and a commit that completes itself because there is no clock to complete
it against.

Task #114 makes that output the one a desktop is actually meant to draw on: a
cursor plane, a mode list a remote desktop can resize inside, a real refresh,
and a first mode that belongs to the VM rather than to a constant in a file
every VM shares.

Everything here is the guest's own output. Capture, transport and the viewer
are #115 and #117, and nothing in this task knows they exist.

## Decisions

* The module gains a **cursor plane**, and capture composites it. This is the
  cost of the decision #111 already made: with a cursor plane present mutter
  puts the pointer on it and leaves it out of the primary plane, which is the
  composition #116's codec and #115's capture are written against.
* The module gains a **real hrtimer vblank**. Today's immediate flush is a
  compositor that never waits and therefore never paces; a frame stream with no
  clock behind it is one #115 would have to invent a clock for.
* Modes run **up to 2560x1440**, from `drm_add_modes_noedid` plus one preferred
  CVT mode at the configured size.
* **No synthesized EDID.** It is not in the task, and the two things it buys --
  a physical size and a monitor name -- cost differently. The physical size is
  a field on `display_info` and is taken. The monitor name costs a hand-built
  128-byte block and a fifth version guard across a kernel API that moved in
  6.7, and is deferred.
* The **initial mode is stored per VM and written by the host**, through a new
  field on the recipe request. The payload stops carrying a modprobe.d file: a
  constant that lives in two places, one of them dead, is worse than one.
* The module **declares its version**. It does not today, so
  `/sys/module/vmlord_drm/version` does not exist, and #113's update
  verification rolls back every update it is asked to make.
* An **output that fails is a degraded display**, which is #113's machinery and
  needs no new code -- only the tests that prove it, and one case #113 does not
  have: a reload that fails on a new mode.

## The module

### The cursor plane

`struct vmlord_device` gains `struct drm_plane cursor`, initialised beside the
primary and passed to `drm_crtc_init_with_planes` as the CRTC's cursor.

* Formats: `DRM_FORMAT_ARGB8888` only, with `DRM_FORMAT_MOD_LINEAR` -- a cursor
  has an alpha channel by definition, and the linear-only rule of #111 applies
  to every plane a capture client mmaps.
* `atomic_check` calls `drm_atomic_helper_check_plane_state` with
  `can_position = true`; the primary keeps `false`. That is the only difference
  between the two checks, so they share one function that branches on
  `plane->type`.
* `atomic_update` does nothing, exactly as the primary's does. There is no
  scanout engine on either plane; the framebuffer bound to the state *is* the
  product.
* `mode_config.cursor_width` and `cursor_height` become 256, which is the size
  #111 measured mutter asking for.
* `DRIVER_CURSOR_HOTSPOT` stays unset. Mutter hides the cursor plane of drivers
  that declare it unless they are on its allowlist, so declaring it would undo
  this whole section.

### The vblank

`struct vmlord_device` gains `struct hrtimer vblank_timer` and a `period_ns`.

* `drm_vblank_init(drm, 1)` runs after `vmlord_mode_config_init` and before
  `drm_dev_register`.
* `drm_crtc_funcs` gains `.enable_vblank` and `.disable_vblank`.
  `.get_vblank_timestamp` is **not** provided: it would require a
  `.get_scanout_position` for a device that does not scan out, and without it
  DRM timestamps at handle time, which is the truth for a virtual output.
* The period is `NSEC_PER_SEC / drm_mode_vrefresh(adjusted_mode)`, computed in
  `atomic_enable`. Deliberately not `framedur_ns` off the vblank structure:
  how that structure is reached moved between the kernels this module supports,
  and dividing by a refresh rate did not move anywhere.
* `atomic_enable` computes the period and calls `drm_crtc_vblank_on`;
  `atomic_disable` calls `drm_crtc_vblank_off`. The timer itself is armed in
  `.enable_vblank` and cancelled in `.disable_vblank`, which is where DRM
  reaches it: `drm_crtc_vblank_on` runs the enable callback when a client is
  already waiting, so arming the timer in `atomic_enable` as well would arm it
  twice.
* The timer function calls `drm_crtc_handle_vblank`, `hrtimer_forward_now` and
  returns `HRTIMER_RESTART`.
* `atomic_flush` stops sending the event itself. It takes
  `drm_crtc_vblank_get`; on success it arms the event with
  `drm_crtc_arm_vblank_event` and puts the reference back, and on failure it
  sends the event immediately as today. The fallback stays because a compositor
  waiting on an event that will never arrive is a compositor that stops
  drawing, and a vblank that could not be enabled must not be able to freeze a
  desktop.
* The fourth version guard #111 measured arrives with the timer:
  `hrtimer_setup()` replaced `hrtimer_init` plus a function assignment in 6.15.
  It lives in `vmlord_compat.h` as a static inline, because unlike the two
  guards in `vmlord_drm.c` it is a statement rather than a struct field.

### The modes

`vmlord_connector_get_modes` becomes:

* `drm_add_modes_noedid(connector, 2560, 1440)` -- the standard list, bounded by
  what this task promises rather than by the module's own size;
* the CVT mode at `width`x`height`, marked `DRM_MODE_TYPE_PREFERRED`, as today;
* `connector->display_info.width_mm` and `height_mm`, set from `width` and
  `height` at 96 DPI (`px * 254 / 960`, integer, no floating point in a kernel
  module). Set here rather than once at connector init because `fill_modes` is
  entitled to reset `display_info`, and `get_modes` runs inside every probe.

`mode_config.max_width` and `max_height` rise from 4096 to 2560 and 1440. That
is a lowering, and it is the point: a mode this module will not drive is a mode
a compositor should not be offered.

`mode_config.min_width`/`min_height` stay at 640x480.

### The version

* `Kbuild` gains `VMLORD_VERSION ?= 0.0.0-dev` and
  `ccflags-y += -DVMLORD_DRM_VERSION=\"$(VMLORD_VERSION)\"`.
* `vmlord_drm.c` gains `MODULE_VERSION(VMLORD_DRM_VERSION)`.
* The Dockerfile's layout stage rewrites the default line to the payload's real
  version, beside where it already substitutes `dkms.conf`.

DKMS keeps its default `MAKE`. Overriding it would mean writing the build
command by hand against `${dkms_tree}` and `${kernel_source_dir}`, and getting
that wrong is a build that fails inside a guest, on a release we cannot compile
for here -- a risk taken for nothing, since a `?=` default and a `sed` reach
the same place.

The `?=` is what keeps `make -C /lib/modules/$(uname -r)/build M=$PWD modules`
working on a checkout, which is how this module is compile-tested during
development.

## The initial mode

### The type

`vmlord_core::DisplayMode { width: u32, height: u32 }`, `Serialize` and
`Deserialize`, with a constructor that refuses anything outside 640x480 ..
2560x1440 -- the same bounds the module's `mode_config` carries, because a
stored mode the module will not offer is a mode nothing can honour.

### The path

```
VmComputeSystemMapping.display_mode: Option<DisplayMode>   (stored, per VM)
  -> AgentWorker::start (already holds the mapping)
  -> SessionWork.display_mode                              (beside display_share)
  -> ApplyDisplayRecipeRequest.initial_mode                (new proto field)
  -> the agent
```

`None` on the host and an absent message on the wire mean the same thing, and
the agent answers both with 1920x1080. Proto3 has no absence for scalars, which
is why the field is a message rather than two `uint32`s: a zero width would
otherwise be indistinguishable from "the host said nothing", and the two must
not be.

**Nothing writes this field in this task.** #120 does, when a person resizes a
viewer and the size is saved. Until then every VM reads `None` and gets the
fallback, which is the same picture as today and a path that exists for the
task that needs it.

`#[serde(default)]` on the mapping field, as every field there has: a mapping
written before this field existed reads as `None`, which is the fallback.

### The recipe

The agent checks the mode it was sent against the same bounds, and falls back
when it is outside them. The constants are written out a second time rather
than shared: `vmlord-agent` depends on `libc`, `serde_json`, `sha2` and the
protocol crate and on nothing else, because it cross-compiles to static musl,
and a dependency on `vmlord-core` for two numbers would be the wrong trade. The
duplication is commented at both ends.

`display_recipe` gains three pure functions, testable without a guest:

* one that renders the contents of `/etc/modprobe.d/vmlord-display.conf` from a
  mode;
* one that reads `/sys/module/vmlord_drm/parameters/width` and `height`;
* one that says whether what is loaded has to be reloaded to reach what is
  wanted. A module that does not say what it was loaded with is never
  reloaded: a reload on a guess is a desktop dropped for nothing.

`display_kernel::load_stage` writes that file itself instead of copying
`vmlord-display.conf` out of the payload. When the module is already loaded and
its parameters disagree with the wanted mode, it does `modprobe -r` then
`modprobe`, and the `MODULE_LOAD` stage says so. A reload that fails is a
`Failed` stage and a degraded display, and the VM keeps running -- SSH, COM1 and
everything else are untouched, which is the rule this whole stack is built on.

`vmlord-display.conf` leaves `payloads/display/module/`, the Dockerfile's copy
list, and the payload.

## Degraded

No new status codes and no new host code. `display-payload-module-not-loaded`
and `display-payload-no-device`, with the `MODULE_LOAD` and `DEVICE` stages
behind them, already describe every way this output fails: a module that will
not load, and a module that loaded without producing a device. A probe that
fails inside `vmlord_probe` -- `drm_vblank_init` returning an error, a plane
that will not initialise -- is a module that loaded and left no
`/dev/dri/card*` behind, which is exactly `display-payload-no-device`.

What this task adds is the case #113 has no path for: a module that was loaded,
was asked to reload for a changed mode, and did not come back. That is
`MODULE_LOAD` in `Failed` with the reload's own output attached, and it is a new
test rather than a new code.

## Testing

**Rust.**

* `DisplayMode` refuses out-of-bounds sizes and round-trips through serde.
* `VmComputeSystemMapping` round-trips with the field present and reads `None`
  when it is absent.
* The modprobe.d rendering is exact, for a given mode and for the fallback.
* Parameter parsing: matching, differing, absent, and unparsable -- absent and
  unparsable must not provoke a reload, because a reload on a guess is a
  desktop dropped for nothing.
* The session carries the mode when there is one and carries nothing when there
  is not; the agent's fallback is 1920x1080 in both the absent cases.
* Whether a reload is owed: a mismatch owes one, a match does not, and a module
  that does not say what it was loaded with never does.

`load_stage` itself gets no test, and that is the existing boundary rather than
a new exemption: it runs `modprobe` and `systemctl` through `command::run`
directly, and `display_kernel`'s tests today cover only the parts that are
functions of text and files. What is testable is the decision, and the decision
is the part that can be wrong. The stage's own reporting -- a failed reload as
`MODULE_LOAD` in `Failed`, a degraded display, and a VM that keeps running --
is straight-line code through the same `report.failed` path #113 already uses
everywhere else, and it is proven for real in #128's matrix.

**C.** There is no unit test for a kernel module. The test is that it compiles,
per release, and the proof is per release:

* 5.15, which is 22.04's kernel, compiles locally on the development machine
  after every step. It is the oldest of the three and the one where the DRM API
  differs most, so it catches the most.
* 6.8 (24.04) and 7.x (26.04) compile only in `payloads/display/prepare.sh`'s
  container, and there is no container runtime on the development machine. They
  stay **unverified** at the end of this task, and the task says so rather than
  implying otherwise.

Runtime behaviour -- a cursor plane mutter actually binds, a vblank a
compositor actually paces against, GDM at 2560x1440 -- needs a guest, and
proving it is #128's mandatory Ubuntu matrix. This task ships the output; it
does not claim the matrix.

## Out of scope

* Dynamic resolution, and anything that writes `display_mode` -- #120.
* Capture of the cursor plane and its composition -- #115.
* A synthesized EDID, and the monitor name that comes with one.
* `VIDEO_WIDTH`/`VIDEO_HEIGHT` in `hcs_config.rs`, which bound the console
  before this module loads. #111 recorded that they belong in a VM's
  configuration; moving them is not this task, and the module's own mode list
  does not depend on them.
* Multi-monitor -- #130, and the module stays at one CRTC and one connector.
