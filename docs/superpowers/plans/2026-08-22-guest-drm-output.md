# Guest DRM output implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn VMLord's minimal DRM module into the output a GNOME desktop is meant to draw on — a cursor plane, modes up to 2560x1440, a real hrtimer vblank, a declared module version — and give it a first mode that belongs to the VM rather than to a constant every VM shares.

**Architecture:** Four tasks change only `payloads/display/module/`, each verified by compiling against 5.15 (Ubuntu 22.04's kernel, the oldest of the three and the one where the DRM API differs most). Five tasks then carry a stored mode from a VM's mapping, through a new field on the display recipe request, to the `/etc/modprobe.d` file the agent now writes itself. One task updates the documentation.

**Tech Stack:** C against the Linux DRM/KMS atomic helpers (kernels 5.15, 6.8, 7.x); Rust 2024 across `vmlord-core`, `vmlord-platform`, `vmlord-agent` and `vmlord-agent-protocol`; prost/protox for the agent protocol; DKMS and Docker for packaging.

**Spec:** `docs/superpowers/specs/2026-08-22-guest-drm-output-design.md`

## Global Constraints

- The module must compile against **5.15, 6.8 and 7.x**. Only 5.15 can be compiled on this machine; 6.8 and 7.x need `payloads/display/prepare.sh`'s container, and there is no container runtime here. They stay **unverified**, and the final report must say so rather than imply otherwise.
- `DRIVER_CURSOR_HOTSPOT` must **never** be set. Mutter hides the cursor plane of drivers that declare it.
- Plane formats stay **XRGB8888/ARGB8888 with `DRM_FORMAT_MOD_LINEAR` only**. A capture client that mmaps a buffer cannot detile anything else.
- The platform device stays named **`vmlord_drm`** on the **platform bus**. Mutter's `61-mutter.rules` tags `platform-vkms` on `ID_PATH`.
- Mode bounds are **640x480 minimum, 2560x1440 maximum**, in three places that must agree: the module's `mode_config`, `vmlord_core::DisplayMode`, and the agent's own check.
- The fallback mode is **1920x1080** wherever a mode is absent or unusable.
- `vmlord-agent` depends on `libc`, `serde_json`, `sha2` and `vmlord-agent-protocol` and on **nothing else** — it cross-compiles to static musl. Do not add `vmlord-core` to it.
- Every failure of this output is a **degraded display and a running VM**. Nothing here may stop a VM, break SSH or break COM1.
- Commit messages are prefixed **`TASK-114: `** and end with the `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` trailer.
- Run `cargo fmt`, `cargo clippy --all-targets` and `cargo test` before each Rust commit.

**The local module build**, used by every C task:

```bash
cd /tmp/claude-1000/-home-machi-vm-lord/e7182cb0-058d-4b92-afbd-3584827186f3/scratchpad \
  && rm -rf mod && cp -r /home/machi/vm-lord/payloads/display/module mod \
  && make -C /lib/modules/5.15.0-190-generic/build M=$PWD/mod modules
```

Expected on success: `CC [M] .../vmlord_drm.o`, `LD [M] .../vmlord_drm.ko`, and no warnings. The `Skipping BTF generation ... due to unavailability of vmlinux` line is normal and not a failure.

---

### Task 1: The cursor plane

**Files:**
- Modify: `payloads/display/module/vmlord_drm.c`

**Interfaces:**
- Consumes: nothing.
- Produces: `struct vmlord_device` with a `cursor` field; `vmlord_cursor_formats`; a `vmlord_plane_atomic_check` that branches on `plane->type`. Task 2 adds fields to the same struct.

- [ ] **Step 1: Add the cursor's formats beside the primary's**

In `vmlord_drm.c`, after `vmlord_formats`:

```c
/*
 * A cursor has an alpha channel by definition, so it gets the one format that
 * has one. The linear-only rule of task #111 applies to every plane a capture
 * client mmaps, and a cursor is one of them: with a cursor plane present,
 * mutter puts the pointer here and leaves it out of the primary plane, and
 * compositing the two is the capture backend's job.
 */
static const uint32_t vmlord_cursor_formats[] = {
	DRM_FORMAT_ARGB8888,
};
```

- [ ] **Step 2: Give the device a cursor plane**

Add the field to `struct vmlord_device`, after `primary`:

```c
	struct drm_plane cursor;
```

- [ ] **Step 3: Let the check position a cursor and not a primary**

Replace the `return` at the end of `vmlord_plane_atomic_check`:

```c
	/*
	 * A cursor is positioned by definition -- it moves across the desktop
	 * without the CRTC being reconfigured -- and it is moved while the
	 * CRTC is off as readily as while it is on. A primary plane that could
	 * be positioned would be a primary plane offset from the framebuffer
	 * capture reads, which is why the primary gets neither.
	 */
	cursor = plane->type == DRM_PLANE_TYPE_CURSOR;

	return drm_atomic_helper_check_plane_state(new_state, crtc_state,
						   DRM_PLANE_NO_SCALING,
						   DRM_PLANE_NO_SCALING,
						   cursor, cursor);
```

and declare `bool cursor;` with the function's other locals. Add `#include <linux/types.h>` only if the build complains — the DRM headers already pull `bool` in.

- [ ] **Step 4: Tell userspace how big a cursor may be**

In `vmlord_mode_config_init`, after `preferred_depth`:

```c
	/*
	 * The size task #111 measured mutter asking this output for. A
	 * compositor that is told nothing assumes 64x64 and draws a cursor
	 * that is a quarter of the size it meant.
	 */
	drm->mode_config.cursor_width = 256;
	drm->mode_config.cursor_height = 256;
```

- [ ] **Step 5: Initialise the plane and hand it to the CRTC**

In `vmlord_pipe_init`, between the primary's `drm_plane_helper_add` and `drm_crtc_init_with_planes`:

```c
	error = drm_universal_plane_init(drm, &vmlord->cursor, 1,
					 &vmlord_plane_funcs,
					 vmlord_cursor_formats,
					 ARRAY_SIZE(vmlord_cursor_formats),
					 vmlord_modifiers,
					 DRM_PLANE_TYPE_CURSOR, "cursor");
	if (error)
		return error;
	drm_plane_helper_add(&vmlord->cursor, &vmlord_plane_helper_funcs);
```

and change the CRTC's initialisation to take it:

```c
	error = drm_crtc_init_with_planes(drm, &vmlord->crtc, &vmlord->primary,
					  &vmlord->cursor, &vmlord_crtc_funcs,
					  NULL);
```

- [ ] **Step 6: Update the file's header comment**

The comment at the top of `vmlord_drm.c` says "One CRTC, one connector, one primary plane". Change that clause to "One CRTC, one connector, a primary plane and a cursor plane". Leave the three decisions below it exactly as they are — they are still the decisions.

- [ ] **Step 7: Compile against 5.15**

Run the local module build from Global Constraints.
Expected: PASS, no warnings.

- [ ] **Step 8: Commit**

```bash
git add payloads/display/module/vmlord_drm.c
git commit -m "TASK-114: Give the virtual output a cursor plane

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: The hrtimer vblank

**Files:**
- Modify: `payloads/display/module/vmlord_compat.h`
- Modify: `payloads/display/module/vmlord_drm.c`

**Interfaces:**
- Consumes: `struct vmlord_device` from Task 1.
- Produces: `vmlord_hrtimer_setup(struct hrtimer *, enum hrtimer_restart (*)(struct hrtimer *))` in `vmlord_compat.h`; `vmlord_device::vblank_timer` and `::period`.

- [ ] **Step 1: Add the fourth version guard**

In `vmlord_compat.h`, add `#include <linux/hrtimer.h>` beside `#include <linux/version.h>`, and this before the closing `#endif`:

```c
/*
 * hrtimer_setup() replaced hrtimer_init plus a function assignment in 6.15.
 * This is the fourth of the four moves task #111 measured, and the only one
 * that is a statement rather than a struct field -- which is why it is wrapped
 * here and the other two are #if'd at their definition sites.
 */
static inline void
vmlord_hrtimer_setup(struct hrtimer *timer,
		     enum hrtimer_restart (*function)(struct hrtimer *))
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 15, 0)
	hrtimer_setup(timer, function, CLOCK_MONOTONIC, HRTIMER_MODE_REL);
#else
	hrtimer_init(timer, CLOCK_MONOTONIC, HRTIMER_MODE_REL);
	timer->function = function;
#endif
}
```

The header comment above says "the two guards that belong to a struct initializer ... live at their definition sites in `vmlord_drm.c` instead". Leave it; it is still true.

- [ ] **Step 2: Give the device a timer and a period**

In `vmlord_drm.c`, add `#include <linux/ktime.h>` to the `linux/` includes, and to `struct vmlord_device`:

```c
	struct hrtimer vblank_timer;
	/* How long one frame lasts at the enabled mode's refresh rate. */
	ktime_t period;
```

- [ ] **Step 3: Write the timer and the two vblank callbacks**

In `vmlord_drm.c`, above the CRTC section's `vmlord_crtc_atomic_enable`:

```c
/*
 * The output's only clock.
 *
 * Nothing scans out here, so there is no hardware vblank to report and no
 * counter to read: this timer is what makes a commit take a frame's worth of
 * time instead of none at all. Without it a compositor is never paced, and a
 * frame stream with no clock behind it is one the capture service of task #115
 * would have to invent a clock for.
 */
static enum hrtimer_restart vmlord_vblank_timer(struct hrtimer *timer)
{
	struct vmlord_device *vmlord =
		container_of(timer, struct vmlord_device, vblank_timer);

	drm_crtc_handle_vblank(&vmlord->crtc);
	hrtimer_forward_now(timer, vmlord->period);

	return HRTIMER_RESTART;
}

static int vmlord_crtc_enable_vblank(struct drm_crtc *crtc)
{
	struct vmlord_device *vmlord =
		container_of(crtc, struct vmlord_device, crtc);

	hrtimer_start(&vmlord->vblank_timer, vmlord->period, HRTIMER_MODE_REL);

	return 0;
}

static void vmlord_crtc_disable_vblank(struct drm_crtc *crtc)
{
	struct vmlord_device *vmlord =
		container_of(crtc, struct vmlord_device, crtc);

	hrtimer_cancel(&vmlord->vblank_timer);
}
```

- [ ] **Step 4: Compute the period when the CRTC comes up**

Replace the empty `vmlord_crtc_atomic_enable` and `vmlord_crtc_atomic_disable`:

```c
static void vmlord_crtc_atomic_enable(struct drm_crtc *crtc,
				      struct drm_atomic_state *state)
{
	struct vmlord_device *vmlord =
		container_of(crtc, struct vmlord_device, crtc);
	int vrefresh = drm_mode_vrefresh(&crtc->state->adjusted_mode);

	/*
	 * The period from the refresh rate of the mode being enabled, and
	 * deliberately not framedur_ns off the vblank structure: how that
	 * structure is reached moved between the kernels this module supports,
	 * and dividing by a refresh rate did not move anywhere. A mode that
	 * reports no refresh rate at all would be a division by zero, so 60 is
	 * the floor rather than an assumption.
	 */
	if (vrefresh <= 0)
		vrefresh = 60;
	vmlord->period = ns_to_ktime(NSEC_PER_SEC / vrefresh);

	/*
	 * Runs .enable_vblank when a client is already waiting, which is what
	 * arms the timer. Arming it here as well would arm it twice.
	 */
	drm_crtc_vblank_on(crtc);
}

static void vmlord_crtc_atomic_disable(struct drm_crtc *crtc,
				       struct drm_atomic_state *state)
{
	drm_crtc_vblank_off(crtc);
}
```

- [ ] **Step 5: Arm the commit's event against the vblank**

Replace `vmlord_crtc_atomic_flush` and its comment entirely:

```c
/*
 * Hands the commit's event to the vblank that will complete it.
 *
 * The fallback below is not tidiness. A compositor waiting on an event that
 * never arrives stops drawing, so a vblank that could not be enabled must not
 * be able to freeze a desktop: the event is sent outright instead, which is
 * what this driver did for every commit before it had a clock.
 */
static void vmlord_crtc_atomic_flush(struct drm_crtc *crtc,
				     struct drm_atomic_state *state)
{
	struct drm_pending_vblank_event *event = crtc->state->event;

	if (!event)
		return;

	crtc->state->event = NULL;

	spin_lock_irq(&crtc->dev->event_lock);
	if (drm_crtc_vblank_get(crtc) != 0)
		drm_crtc_send_vblank_event(crtc, event);
	else
		drm_crtc_arm_vblank_event(crtc, event);
	spin_unlock_irq(&crtc->dev->event_lock);
}
```

`drm_crtc_arm_vblank_event` keeps the reference `drm_crtc_vblank_get` took; it is dropped when the event is sent. Do not add a `drm_crtc_vblank_put`.

- [ ] **Step 6: Register the callbacks**

In `vmlord_crtc_funcs`, after `.page_flip`:

```c
	.enable_vblank = vmlord_crtc_enable_vblank,
	.disable_vblank = vmlord_crtc_disable_vblank,
```

Do **not** add `.get_vblank_timestamp`: it needs a `.get_scanout_position` for a device that does not scan out, and without it DRM timestamps at handle time, which is the truth here.

- [ ] **Step 7: Set the timer up and initialise vblank in probe**

In `vmlord_probe`, between `platform_set_drvdata` and `vmlord_mode_config_init`:

```c
	/*
	 * A period before any mode is enabled, so that a vblank enabled early
	 * has something to run at. atomic_enable replaces it with the real one.
	 */
	vmlord->period = ns_to_ktime(NSEC_PER_SEC / 60);
	vmlord_hrtimer_setup(&vmlord->vblank_timer, vmlord_vblank_timer);
```

and between `vmlord_pipe_init` and `drm_mode_config_reset`:

```c
	error = drm_vblank_init(&vmlord->drm, 1);
	if (error)
		return error;
```

- [ ] **Step 8: Stop the timer on remove**

In `vmlord_remove`, after `drm_atomic_helper_shutdown`:

```c
	/*
	 * The shutdown above disables the CRTC, which cancels the timer through
	 * .disable_vblank. This is the belt to that braces: a timer still armed
	 * when the device is freed fires into memory that is gone.
	 */
	hrtimer_cancel(&vmlord->vblank_timer);
```

- [ ] **Step 9: Compile against 5.15**

Run the local module build from Global Constraints.
Expected: PASS, no warnings. A failure naming `hrtimer_setup` means the 6.15 guard is inverted; a failure naming `NSEC_PER_SEC` means `<linux/ktime.h>` is missing.

- [ ] **Step 10: Commit**

```bash
git add payloads/display/module/vmlord_compat.h payloads/display/module/vmlord_drm.c
git commit -m "TASK-114: Pace the virtual output with an hrtimer vblank

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: The mode list and the physical size

**Files:**
- Modify: `payloads/display/module/vmlord_drm.c`

**Interfaces:**
- Consumes: nothing from Tasks 1-2.
- Produces: `VMLORD_MAX_WIDTH` / `VMLORD_MAX_HEIGHT` / `VMLORD_MIN_WIDTH` / `VMLORD_MIN_HEIGHT`, which Task 5's `vmlord_core::DisplayMode` and Task 9's agent check must agree with.

- [ ] **Step 1: Name the bounds**

In `vmlord_drm.c`, after `#define VMLORD_DRIVER_NAME`:

```c
/*
 * What this output offers, and the same numbers vmlord_core::DisplayMode and
 * vmlord-agent carry. A mode this module will not drive is a mode a compositor
 * should not be offered and a host should not store.
 */
#define VMLORD_MIN_WIDTH  640
#define VMLORD_MIN_HEIGHT 480
#define VMLORD_MAX_WIDTH  2560
#define VMLORD_MAX_HEIGHT 1440
```

- [ ] **Step 2: Add the millimetre conversion**

Above `vmlord_connector_get_modes`:

```c
/*
 * A pixel count as millimetres at 96 DPI: px * 25.4 / 96, in integers, because
 * a kernel module has no floating point. 96 is a choice and not a measurement
 * -- there is no glass -- but it is the one that makes a compositor pick scale
 * 1 for every size this output offers.
 */
static u32 vmlord_millimetres(u32 pixels)
{
	return DIV_ROUND_CLOSEST(pixels * 254u, 960u);
}
```

Add `#include <linux/kernel.h>` to the `linux/` includes for `DIV_ROUND_CLOSEST` — `linux/math.h`, where it lives in newer kernels, does not exist in 5.15.

- [ ] **Step 3: Widen the mode list and give the connector a size**

Replace the body of `vmlord_connector_get_modes` and rewrite its doc comment:

```c
/*
 * The modes this output offers.
 *
 * The standard list up to what this module will drive, plus the module's own
 * size marked preferred, so that a compositor started before anything has been
 * configured comes up at the size the host asked for and can still be resized
 * within the list.
 *
 * No EDID is synthesized. The two things one would buy are a physical size and
 * a monitor name; the size is a field and is set here, and the name costs a
 * hand-built 128-byte block plus a fifth version guard across an API that moved
 * in 6.7 -- which is a deferred nicety, not an MVP.
 */
static int vmlord_connector_get_modes(struct drm_connector *connector)
{
	struct drm_display_mode *mode;
	int count;

	/*
	 * Set on every probe rather than once at connector init: fill_modes is
	 * entitled to reset display_info, and get_modes runs inside every probe.
	 */
	connector->display_info.width_mm = vmlord_millimetres(width);
	connector->display_info.height_mm = vmlord_millimetres(height);

	count = drm_add_modes_noedid(connector, VMLORD_MAX_WIDTH,
				     VMLORD_MAX_HEIGHT);
	mode = drm_cvt_mode(connector->dev, width, height, 60, false, false,
			    false);
	if (mode) {
		mode->type |= DRM_MODE_TYPE_PREFERRED;
		drm_mode_probed_add(connector, mode);
		count++;
	}

	return count;
}
```

- [ ] **Step 4: Lower the mode_config ceiling to what is offered**

In `vmlord_mode_config_init`, replace the four bound assignments:

```c
	drm->mode_config.min_width = VMLORD_MIN_WIDTH;
	drm->mode_config.min_height = VMLORD_MIN_HEIGHT;
	drm->mode_config.max_width = VMLORD_MAX_WIDTH;
	drm->mode_config.max_height = VMLORD_MAX_HEIGHT;
```

The ceiling comes **down** from 4096, and that is the point: `drm_add_modes_noedid` clamps to these, and a framebuffer larger than anything this output drives is one nothing can use.

- [ ] **Step 5: Refuse a module parameter outside the bounds**

In `vmlord_probe`, immediately after `platform_set_drvdata`:

```c
	/*
	 * The parameters are writable by whoever installs the modprobe.d file,
	 * and a size outside the bounds would produce a preferred mode the
	 * mode_config rejects -- a device that exists and shows nothing. The
	 * fallback is a working desktop, and the warning is how somebody finds
	 * out why it is not the size they asked for.
	 */
	if (width < VMLORD_MIN_WIDTH || width > VMLORD_MAX_WIDTH ||
	    height < VMLORD_MIN_HEIGHT || height > VMLORD_MAX_HEIGHT) {
		dev_warn(&pdev->dev,
			 "%ux%u is outside %ux%u..%ux%u; using 1920x1080\n",
			 width, height, VMLORD_MIN_WIDTH, VMLORD_MIN_HEIGHT,
			 VMLORD_MAX_WIDTH, VMLORD_MAX_HEIGHT);
		width = 1920;
		height = 1080;
	}
```

- [ ] **Step 6: Compile against 5.15**

Run the local module build from Global Constraints.
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add payloads/display/module/vmlord_drm.c
git commit -m "TASK-114: Offer modes up to 2560x1440 and a physical size

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: The module's declared version

**Files:**
- Modify: `payloads/display/module/Kbuild`
- Modify: `payloads/display/module/vmlord_drm.c`
- Modify: `payloads/display/Dockerfile`

**Interfaces:**
- Consumes: nothing.
- Produces: `/sys/module/vmlord_drm/version`, which `crates/agent/src/display_kernel.rs`'s `loaded_version()` already reads and `verify()` already compares.

This closes a defect: the module declares no version today, so that file does not exist, `loaded_version()` is always `None`, and `verify()` fails every update with "does not say which version is loaded" — which rolls back every update task #113 is asked to make.

- [ ] **Step 1: Let Kbuild carry a version**

Replace `payloads/display/module/Kbuild` entirely:

```make
# The payload version this module reports as its own.
#
# `payloads/display/Dockerfile` rewrites the default below when it lays a
# payload out, so a packed module reports the version of the payload that
# carries it. The `?=` is what keeps a plain
# `make -C /lib/modules/$(uname -r)/build M=$PWD modules` working on a
# checkout, which is how this module is compiled during development.
VMLORD_VERSION ?= 0.0.0-dev

obj-m += vmlord_drm.o
ccflags-y += -I$(src) -DVMLORD_DRM_VERSION=\"$(VMLORD_VERSION)\"
```

- [ ] **Step 2: Declare the version in the module**

In `vmlord_drm.c`, after `#define VMLORD_DRIVER_NAME`:

```c
/*
 * What /sys/module/vmlord_drm/version answers, and what the recipe's update
 * verification compares against. Kbuild defines it from the payload's version;
 * the fallback is what a module built by hand out of a checkout reports.
 */
#ifndef VMLORD_DRM_VERSION
#define VMLORD_DRM_VERSION "0.0.0-dev"
#endif
```

and beside the other `MODULE_*` macros at the end of the file:

```c
MODULE_VERSION(VMLORD_DRM_VERSION);
```

- [ ] **Step 3: Verify the fallback reaches the module**

Run the local module build from Global Constraints, then:

```bash
modinfo /tmp/claude-1000/-home-machi-vm-lord/e7182cb0-058d-4b92-afbd-3584827186f3/scratchpad/mod/vmlord_drm.ko | grep -E '^version'
```

Expected: `version:        0.0.0-dev`

- [ ] **Step 4: Verify a substituted version reaches the module**

```bash
cd /tmp/claude-1000/-home-machi-vm-lord/e7182cb0-058d-4b92-afbd-3584827186f3/scratchpad \
  && make -C /lib/modules/5.15.0-190-generic/build M=$PWD/mod VMLORD_VERSION=0.9.9 modules \
  && modinfo mod/vmlord_drm.ko | grep -E '^version'
```

Expected: `version:        0.9.9`

If it still says `0.0.0-dev`, the object was not rebuilt — `rm -rf mod` and re-copy first.

- [ ] **Step 5: Substitute the real version when a payload is laid out**

In `payloads/display/Dockerfile`'s `layout` stage, remove `/src/module/Kbuild` from the `cp` list and add a `sed` beside the one that already writes `dkms.conf`:

```dockerfile
    cp /src/module/vmlord_drm.c /src/module/vmlord_compat.h \
       /src/module/vmlord-display.conf /src/module/vmlord-display-unbind-simpledrm.service \
       /output/prepared/content/drm/; \
    sed "s/^VMLORD_VERSION ?= .*/VMLORD_VERSION ?= ${VERSION}/" /src/module/Kbuild \
       > /output/prepared/content/drm/Kbuild; \
    sed "s/@VERSION@/${VERSION}/" /src/module/dkms.conf.in \
       > /output/prepared/content/drm/dkms.conf; \
```

DKMS keeps its default `MAKE`. Overriding it would mean writing the build command by hand against `${dkms_tree}` and `${kernel_source_dir}` — and getting that wrong is a build that fails inside a guest, on a release that cannot be compiled here.

- [ ] **Step 6: Check the sed against the file it edits**

```bash
sed "s/^VMLORD_VERSION ?= .*/VMLORD_VERSION ?= 1.2.3/" payloads/display/module/Kbuild | grep VMLORD_VERSION
```

Expected: `VMLORD_VERSION ?= 1.2.3` — one line, and no other line changed.

- [ ] **Step 7: Commit**

```bash
git add payloads/display/module/Kbuild payloads/display/module/vmlord_drm.c payloads/display/Dockerfile
git commit -m "TASK-114: Make the module declare the payload version it was built from

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: `DisplayMode` in the core

**Files:**
- Modify: `crates/core/src/display.rs`
- Modify: `crates/core/src/lib.rs` (the re-export list)

**Interfaces:**
- Consumes: the bounds Task 3 named.
- Produces: `vmlord_core::DisplayMode` with `DisplayMode::new(width: u32, height: u32) -> Option<Self>`, `width(&self) -> u32`, `height(&self) -> u32`, `Serialize` (and **not** `Deserialize` — Task 7 reads it forgivingly); `MIN_DISPLAY_WIDTH`, `MIN_DISPLAY_HEIGHT`, `MAX_DISPLAY_WIDTH`, `MAX_DISPLAY_HEIGHT`.

- [ ] **Step 1: Write the failing test**

At the end of `crates/core/src/display.rs`, inside the existing `#[cfg(test)] mod tests` (or in a new one if the file has none — check first with `grep -n 'cfg(test)' crates/core/src/display.rs`):

```rust
    #[test]
    fn a_display_mode_is_one_the_module_will_actually_drive() {
        assert_eq!(
            DisplayMode::new(1920, 1080).map(|mode| (mode.width(), mode.height())),
            Some((1920, 1080))
        );
        assert_eq!(
            DisplayMode::new(2560, 1440).map(|mode| (mode.width(), mode.height())),
            Some((2560, 1440)),
            "the largest mode this task promises is a mode"
        );
        assert_eq!(
            DisplayMode::new(640, 480).map(|mode| (mode.width(), mode.height())),
            Some((640, 480))
        );

        assert_eq!(DisplayMode::new(3840, 2160), None, "above the ceiling");
        assert_eq!(DisplayMode::new(320, 240), None, "below the floor");
        assert_eq!(DisplayMode::new(1920, 0), None, "a height of nothing");
        assert_eq!(DisplayMode::new(0, 0), None);
    }

    #[test]
    fn a_display_mode_is_stored_as_two_numbers() {
        let json = serde_json::to_string(&DisplayMode::new(1600, 900).unwrap()).unwrap();

        assert_eq!(json, r#"{"width":1600,"height":900}"#);
    }
```

Add `DisplayMode` to that module's `use super::{...}` list.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-core display_mode`
Expected: FAIL — `cannot find type DisplayMode`.

If `serde_json` is not a dev-dependency of `vmlord-core`, add it: `cargo add --dev serde_json -p vmlord-core`.

- [ ] **Step 3: Write the type**

In `crates/core/src/display.rs`, after `DesktopProfile`'s `impl std::fmt::Display`:

```rust
/// The smallest and largest output VMLord's DRM module offers.
///
/// The same numbers `vmlord_drm`'s `mode_config` carries and `vmlord-agent`
/// checks against. A mode stored here that the module will not drive is a mode
/// nothing can honour, so this is where a stored one is refused.
pub const MIN_DISPLAY_WIDTH: u32 = 640;
/// The shortest output VMLord's DRM module offers. See [`MIN_DISPLAY_WIDTH`].
pub const MIN_DISPLAY_HEIGHT: u32 = 480;
/// The widest output VMLord's DRM module offers. See [`MIN_DISPLAY_WIDTH`].
pub const MAX_DISPLAY_WIDTH: u32 = 2560;
/// The tallest output VMLord's DRM module offers. See [`MIN_DISPLAY_WIDTH`].
pub const MAX_DISPLAY_HEIGHT: u32 = 1440;

/// The mode a VM's display comes up at, in pixels.
///
/// Its fields are private and its constructor refuses anything the module will
/// not drive, so a value of this type is a mode that can be honoured rather
/// than a pair of numbers that might be one.
///
/// `Serialize` and not `Deserialize`: it is stored on a VM's mapping, and a
/// stored mode that has become unusable must read as *no* mode rather than
/// make the whole mapping unreadable. `metadata.rs` does that reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DisplayMode {
    width: u32,
    height: u32,
}

impl DisplayMode {
    /// The mode, or `None` when it is outside what the module offers.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        if width < MIN_DISPLAY_WIDTH
            || width > MAX_DISPLAY_WIDTH
            || height < MIN_DISPLAY_HEIGHT
            || height > MAX_DISPLAY_HEIGHT
        {
            return None;
        }
        Some(Self { width, height })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }
}

impl std::fmt::Display for DisplayMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}
```

- [ ] **Step 4: Re-export it**

`crates/core/src/lib.rs` re-exports the display types. Find the `pub use display::{...}` line and add `DisplayMode`, `MAX_DISPLAY_HEIGHT`, `MAX_DISPLAY_WIDTH`, `MIN_DISPLAY_HEIGHT`, `MIN_DISPLAY_WIDTH` in alphabetical order with the rest.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-core`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add crates/core/src/display.rs crates/core/src/lib.rs crates/core/Cargo.toml Cargo.lock
git commit -m "TASK-114: Add a display mode a VM can be stored with

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The mode on the wire

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto:495-497`
- Modify: `crates/agent-protocol/proto/agent.descriptor.bin` (regenerated, not edited)

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_agent_protocol::v1::DisplayMode { width: u32, height: u32 }` and `ApplyDisplayRecipeRequest { initial_mode: Option<DisplayMode> }`. Tasks 8 and 9 both use these names.

- [ ] **Step 1: Add the field and the message**

In `agent.proto`, replace:

```proto
// Everything the guest needs in order to decide is in the guest or in the
// payload it mounted one message earlier, so this request carries nothing.
message ApplyDisplayRecipeRequest {}
```

with:

```proto
// Everything else the guest needs in order to decide is in the guest or in the
// payload it mounted one message earlier.
message ApplyDisplayRecipeRequest {
  // The mode this VM's output is to come up at, as the host has it stored.
  //
  // Absent is a VM nothing has been saved for, which is every VM until task
  // #120 saves one, and the guest answers absence with its own fallback. A
  // message rather than two uint32s precisely so that absence and a width of
  // zero are different things: proto3 scalars have no absence, and a zero
  // width read as a mode is an output nobody can see.
  DisplayMode initial_mode = 1;
}

// One output mode, in pixels.
message DisplayMode {
  uint32 width = 1;
  uint32 height = 2;
}
```

- [ ] **Step 2: Run the descriptor test to verify it fails**

Run: `cargo test -p vmlord-agent-protocol`
Expected: FAIL — `proto/agent.descriptor.bin is not what the .proto compiles to`.

- [ ] **Step 3: Refresh the checked-in descriptor**

Run: `VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-agent-protocol`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent-protocol`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent-protocol/proto
git commit -m "TASK-114: Carry the VM's initial display mode to the guest

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: The mode stored with the VM

**Files:**
- Modify: `crates/platform/src/metadata.rs`
- Modify: every file that builds a `VmComputeSystemMapping` literal — the compiler names them

**Interfaces:**
- Consumes: `vmlord_core::DisplayMode` from Task 5.
- Produces: `VmComputeSystemMapping::display_mode: Option<DisplayMode>`, which Task 8 reads.

- [ ] **Step 1: Write the failing test**

In `crates/platform/src/metadata.rs`'s `mod tests`:

```rust
    #[test]
    fn a_stored_display_mode_survives_a_round_trip() {
        let mut written = mapping(Uuid::nil(), "vm", "hcs");
        written.display_mode = DisplayMode::new(2560, 1440);

        let json = serde_json::to_string(&written).unwrap();
        let read: VmComputeSystemMapping = serde_json::from_str(&json).unwrap();

        assert_eq!(read.display_mode, DisplayMode::new(2560, 1440));
    }

    #[test]
    fn a_mapping_with_no_display_mode_reads_as_no_mode() {
        let json = serde_json::to_string(&mapping(Uuid::nil(), "vm", "hcs")).unwrap();
        let stripped = json.replace(r#""display_mode":null,"#, "");
        let read: VmComputeSystemMapping = serde_json::from_str(&stripped).unwrap();

        assert_eq!(
            read.display_mode, None,
            "every VM today, and every VM written before this field existed"
        );
    }

    #[test]
    fn a_stored_mode_the_module_will_not_drive_reads_as_no_mode() {
        let json = serde_json::to_string(&mapping(Uuid::nil(), "vm", "hcs"))
            .unwrap()
            .replace(
                r#""display_mode":null"#,
                r#""display_mode":{"width":7680,"height":4320}"#,
            );

        let read: VmComputeSystemMapping = serde_json::from_str(&json)
            .expect("one unusable field must not cost VMLord the whole VM");

        assert_eq!(read.display_mode, None);
    }
```

Add `DisplayMode` to the test module's imports.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform display_mode`
Expected: FAIL — `no field display_mode on type VmComputeSystemMapping`.

- [ ] **Step 3: Add the field**

In `VmComputeSystemMapping`, after `display_provisioning`:

```rust
    /// The mode this VM's output comes up at, when one has been saved.
    ///
    /// `None` is every VM today: nothing writes this field until task #120
    /// saves the size somebody resized a viewer to. The guest answers `None`
    /// with 1920x1080, which is what every VM has come up at so far, so an
    /// absent field and a mapping written before this field existed read the
    /// same and neither needs a migration.
    #[serde(default, deserialize_with = "forgiving_display_mode")]
    pub display_mode: Option<DisplayMode>,
```

and beside `fn no_desktop()`:

```rust
/// Reads a stored display mode, and reads an unusable one as no mode at all.
///
/// A mode outside what the module drives cannot be honoured, and the fallback
/// is a working desktop. A mapping that refuses to parse, on the other hand,
/// is a VM VMLord loses entirely -- so one bad field is worth the fallback and
/// is not worth the VM.
fn forgiving_display_mode<'de, D>(deserializer: D) -> Result<Option<DisplayMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Stored {
        width: u32,
        height: u32,
    }

    let Some(stored) = Option::<Stored>::deserialize(deserializer)? else {
        return Ok(None);
    };
    Ok(DisplayMode::new(stored.width, stored.height))
}
```

Add `DisplayMode` to the file's `use vmlord_core::{...}` and `Deserializer` usage via the fully-qualified `serde::Deserializer` above (no new import needed beyond `serde::Deserialize`, which the file already imports).

- [ ] **Step 4: Fix every construction site**

Run: `cargo build -p vmlord-platform --all-targets`

The compiler lists every `VmComputeSystemMapping` literal missing the field. Add `display_mode: None,` to each, after `display_provisioning`. Most are behind a per-module `fn mapping(...)` test helper, so the count of real edits is far below the number of call sites. Do not invent values: `None` is what every VM has.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add crates/platform
git commit -m "TASK-114: Store a display mode with each VM

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: The host sends the mode

**Files:**
- Modify: `crates/platform/src/agent_session.rs` (`SessionWork`, `apply_display_recipe`, tests)
- Modify: `crates/platform/src/agent.rs` (`serve`, `AgentConnection::start`)

**Interfaces:**
- Consumes: `VmComputeSystemMapping::display_mode` (Task 7), `vmlord_agent_protocol::v1::DisplayMode` (Task 6).
- Produces: `SessionWork::display_mode: Option<DisplayMode>` (the **core** type — it is converted to the protocol type inside `apply_display_recipe`).

`AgentConnection::start` already takes `mapping: &VmComputeSystemMapping`, so no signature there changes.

- [ ] **Step 1: Write the failing test**

In `crates/platform/src/agent_session.rs`'s `mod tests`, change `display_work` to take a mode and add the assertions. Replace the helper:

```rust
    /// The work a display test does: no GPU, one display share, a stored mode
    /// or none, and a sink that keeps what came back.
    fn display_work<'a>(
        share: Option<&'a DisplayShare>,
        mode: Option<vmlord_core::DisplayMode>,
        display: GuestDisplaySink<'a>,
    ) -> SessionWork<'a> {
        SessionWork {
            gpu_shares: None,
            display_share: share,
            display_mode: mode,
            gpu: &|_| {},
            display,
            updates: None,
        }
    }
```

and add a test:

```rust
    #[test]
    fn a_vm_with_a_stored_mode_asks_the_guest_for_it() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
                    mount_point: "/opt/vmlord/display-payload".to_owned(),
                    state: i32::from(DisplayMountState::Mounted),
                    message: "mounted".to_owned(),
                }),
            }),
        ));

        let share = display_share();
        let mode = vmlord_core::DisplayMode::new(2560, 1440);
        let _ = serve(
            &mut guest,
            &session,
            display_work(Some(&share), mode, &|_| {}),
            VM,
        );

        let asked = guest.answer_to(super::DISPLAY_APPLY_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the recipe should have been sent as a request");
        };
        let Some(request::Kind::ApplyDisplayRecipe(ref apply)) = request.kind else {
            panic!("the recipe should have been an apply request");
        };
        let sent = apply.initial_mode.as_ref().expect("the stored mode");
        assert_eq!((sent.width, sent.height), (2560, 1440));
    }

    #[test]
    fn a_vm_with_no_stored_mode_asks_the_guest_for_nothing() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
                    mount_point: "/opt/vmlord/display-payload".to_owned(),
                    state: i32::from(DisplayMountState::Mounted),
                    message: "mounted".to_owned(),
                }),
            }),
        ));

        let share = display_share();
        let _ = serve(
            &mut guest,
            &session,
            display_work(Some(&share), None, &|_| {}),
            VM,
        );

        let asked = guest.answer_to(super::DISPLAY_APPLY_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the recipe should have been sent as a request");
        };
        let Some(request::Kind::ApplyDisplayRecipe(ref apply)) = request.kind else {
            panic!("the recipe should have been an apply request");
        };
        assert_eq!(
            apply.initial_mode, None,
            "absence is what the guest answers with its own fallback"
        );
    }
```

Update the two existing `display_work(...)` call sites to pass `None` as the new second argument.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform stored_mode`
Expected: FAIL — `struct SessionWork has no field named display_mode`.

- [ ] **Step 3: Add the field to `SessionWork`**

In `agent_session.rs`:

```rust
    /// The mode this VM's output is to come up at, if one is stored with it.
    ///
    /// It belongs to the run for the reason the share does: a module parameter
    /// is read once, when the module loads, so every session of a run carries
    /// the same mode and a changed one reaches the output through a reload.
    pub(crate) display_mode: Option<DisplayMode>,
```

after `display_share`, and add `DisplayMode` to the file's `use vmlord_core::{...}`.

- [ ] **Step 4: Put the mode in the request**

Change `apply_display_recipe` to take it and send it:

```rust
fn apply_display_recipe<S: Read + Write>(
    stream: &mut S,
    mode: Option<DisplayMode>,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let request = Envelope::request(
        DISPLAY_APPLY_REQUEST_ID,
        request::Kind::ApplyDisplayRecipe(ApplyDisplayRecipeRequest {
            initial_mode: mode.map(|mode| vmlord_agent_protocol::v1::DisplayMode {
                width: mode.width(),
                height: mode.height(),
            }),
        }),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    log::debug!(
        "VMLord asked the agent of VM \"{vm_name}\" to apply its display recipe{}",
        match mode {
            Some(mode) => format!(" at {mode}"),
            None => String::new(),
        }
    );

    Ok(Some(DISPLAY_APPLY_REQUEST_ID))
}
```

and its one call site in `serve`:

```rust
                    pending_display_recipe =
                        apply_display_recipe(stream, work.display_mode, vm_name, &mut buffer)?;
```

- [ ] **Step 5: Carry it from the mapping to the session**

In `crates/platform/src/agent.rs`:

- add a `display_mode: Option<DisplayMode>` parameter to `fn serve`, after `display_share`, and pass it into `SessionWork` as `display_mode`;
- in `AgentConnection::start`, read it before the thread is spawned, beside `let vm_id = mapping.vm_id;`:

```rust
        let display_mode = mapping.display_mode;
```

  and move it into the closure alongside `display_share`, passing `display_mode` to `serve`;
- add `DisplayMode` to that file's `use vmlord_core::{...}`.

`serve` already carries `#[expect(clippy::too_many_arguments, ...)]`, so one more argument needs no new attribute.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add crates/platform
git commit -m "TASK-114: Tell the guest which mode its output starts at

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: The guest writes its own modprobe options

**Files:**
- Modify: `crates/agent/src/display_recipe.rs`
- Modify: `crates/agent/src/display_kernel.rs`
- Modify: `crates/agent/src/session.rs`
- Modify: `crates/agent/src/main.rs`
- Delete: `payloads/display/module/vmlord-display.conf`
- Modify: `payloads/display/Dockerfile`

**Interfaces:**
- Consumes: `ApplyDisplayRecipeRequest::initial_mode` (Task 6).
- Produces: `display_recipe::{FALLBACK_MODE, wanted_mode, modprobe_options, parse_module_parameters, needs_reload}`; `display_kernel::apply(&AtomicBool, Option<(u32, u32)>)`.

- [ ] **Step 1: Write the failing tests**

In `crates/agent/src/display_recipe.rs`'s `mod tests`:

```rust
    #[test]
    fn a_mode_the_module_will_not_drive_falls_back() {
        assert_eq!(wanted_mode(Some((2560, 1440))), (2560, 1440));
        assert_eq!(wanted_mode(Some((640, 480))), (640, 480));
        assert_eq!(wanted_mode(None), FALLBACK_MODE);
        assert_eq!(
            wanted_mode(Some((3840, 2160))),
            FALLBACK_MODE,
            "the host sends only what it validated, and the guest checks anyway"
        );
        assert_eq!(wanted_mode(Some((0, 0))), FALLBACK_MODE);
    }

    #[test]
    fn the_modprobe_options_name_the_module_and_the_mode() {
        let options = modprobe_options(1600, 900);

        assert!(options.ends_with("options vmlord_drm width=1600 height=900\n"));
        assert!(
            options.starts_with('#'),
            "a file VMLord wrote should say so to whoever finds it"
        );
    }

    #[test]
    fn the_loaded_mode_is_what_the_module_says_it_is() {
        assert_eq!(parse_module_parameters("1920\n", "1080\n"), Some((1920, 1080)));
        assert_eq!(parse_module_parameters("", ""), None);
        assert_eq!(parse_module_parameters("wide", "1080"), None);
    }

    #[test]
    fn only_a_mode_that_is_known_to_differ_costs_a_reload() {
        assert!(needs_reload(Some((1920, 1080)), (2560, 1440)));
        assert!(!needs_reload(Some((1920, 1080)), (1920, 1080)));
        assert!(
            !needs_reload(None, (2560, 1440)),
            "a module that does not say must not be dropped on a guess"
        );
    }
```

Add `FALLBACK_MODE, modprobe_options, needs_reload, parse_module_parameters, wanted_mode` to the test module's `use super::{...}`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent display`
Expected: FAIL — `unresolved imports`.

- [ ] **Step 3: Write the four functions**

In `crates/agent/src/display_recipe.rs`, after `parse_module_version`:

```rust
/// What the output comes up at when the host has not said, or has said
/// something this module will not drive.
pub const FALLBACK_MODE: (u32, u32) = (1920, 1080);

/// The bounds `vmlord_drm`'s `mode_config` carries.
///
/// Written out a second time rather than taken from `vmlord-core`: this crate
/// depends on `libc`, `serde_json`, `sha2` and the protocol crate and on
/// nothing else, because it cross-compiles to static musl, and a dependency
/// for four numbers would be the wrong trade. Change these and
/// `vmlord_core::MIN_DISPLAY_WIDTH` and `VMLORD_MIN_WIDTH` in `vmlord_drm.c`
/// together.
const MIN_WIDTH: u32 = 640;
const MIN_HEIGHT: u32 = 480;
const MAX_WIDTH: u32 = 2560;
const MAX_HEIGHT: u32 = 1440;

/// The mode to bring the output up at, given what the host asked for.
///
/// The host sends only modes it validated, and this checks again anyway: a
/// module parameter outside what the module drives is a device that exists and
/// shows nothing, and the fallback is a working desktop.
#[must_use]
pub fn wanted_mode(asked: Option<(u32, u32)>) -> (u32, u32) {
    match asked {
        Some((width, height))
            if (MIN_WIDTH..=MAX_WIDTH).contains(&width)
                && (MIN_HEIGHT..=MAX_HEIGHT).contains(&height) =>
        {
            (width, height)
        }
        _ => FALLBACK_MODE,
    }
}

/// What `/etc/modprobe.d/vmlord-display.conf` says, for a mode.
///
/// Written by the guest from what the host asked for, rather than copied out
/// of the payload: the size belongs to one VM and a payload is shared by all
/// of them.
#[must_use]
pub fn modprobe_options(width: u32, height: u32) -> String {
    format!(
        "# Written by vmlord-agent from the mode this VM has stored.\n\
         # The output comes up at this size; changing it needs the module reloaded.\n\
         options {MODULE} width={width} height={height}\n"
    )
}

/// The size the loaded module was given, out of its `parameters` directory.
///
/// `None` is a module that does not say: absent files, or text that is not a
/// number. Deliberately not a guess -- see [`needs_reload`].
#[must_use]
pub fn parse_module_parameters(width: &str, height: &str) -> Option<(u32, u32)> {
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// Whether a module that is already up has to be reloaded to reach `wanted`.
///
/// A module parameter is read once, when the module loads, so a stored mode
/// that changed under a running module reaches the output no other way. A
/// module that does not say what it was loaded with is left alone: a reload on
/// a guess is a desktop dropped for nothing.
#[must_use]
pub fn needs_reload(loaded: Option<(u32, u32)>, wanted: (u32, u32)) -> bool {
    loaded.is_some_and(|loaded| loaded != wanted)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent display`
Expected: PASS.

- [ ] **Step 5: Write the options file and reload on a changed mode**

In `crates/agent/src/display_kernel.rs`:

- add the two parameter paths beside `MODULE_VERSION`:

```rust
const MODULE_PARAM_WIDTH: &str = "/sys/module/vmlord_drm/parameters/width";
const MODULE_PARAM_HEIGHT: &str = "/sys/module/vmlord_drm/parameters/height";
```

- extend the `display_recipe` import with `modprobe_options, needs_reload, parse_module_parameters, wanted_mode`. Not `FALLBACK_MODE`: `wanted_mode` already resolves absence, and this file never needs the constant itself;
- thread the mode through: `pub fn apply(stopping: &AtomicBool, mode: Option<(u32, u32)>)`, `fn run_stages(report: &mut Report, stopping: &AtomicBool, mode: Option<(u32, u32)>)`, and `load_stage(report, mode)?` at its call site. `update()` and `run_update()` are **unchanged**: they reload rather than load, and `apply` writes the options file on every session before an update can be asked for;
- in `load_stage(report: &mut Report, mode: Option<(u32, u32)>)`, replace the copy of `vmlord-display.conf` and the loaded short-circuit:

```rust
    let wanted = wanted_mode(mode);
    let prepared = write_if_different(Path::new(MODULES_LOAD), &format!("{MODULE}\n"))
        .map_err(|error| format!("{MODULES_LOAD} could not be written: {error}"))
        .and_then(|()| {
            write_if_different(
                Path::new(MODPROBE_OPTIONS),
                &modprobe_options(wanted.0, wanted.1),
            )
            .map_err(|error| format!("{MODPROBE_OPTIONS} could not be written: {error}"))
        })
        .and_then(|()| copy("vmlord-display-unbind-simpledrm.service", UNBIND_UNIT));
```

  and:

```rust
    if module_is_loaded(&read(Path::new("/proc/modules"))) {
        let loaded = parse_module_parameters(
            &read(Path::new(MODULE_PARAM_WIDTH)),
            &read(Path::new(MODULE_PARAM_HEIGHT)),
        );
        if needs_reload(loaded, wanted) {
            // The stored mode changed under a module that is already up, and a
            // module parameter is read once. A reload that fails is a failed
            // stage and a degraded display -- and a VM that keeps running.
            return reload_module(report);
        }
        report.skipped(
            DisplayRecipeStep::ModuleLoad,
            format!("{MODULE} is loaded at {}x{}", wanted.0, wanted.1),
        );
        return Ok(());
    }
```

  and change the final `report.ok` message to `format!("loaded {MODULE} at {}x{} and asked for it on every boot", wanted.0, wanted.1)`.

The `copy` closure keeps only one caller. Leave it a closure; a second caller arrives with task #115's services.

- [ ] **Step 6: Hand the request's mode to the recipe**

In `crates/agent/src/session.rs`, change the handler's type:

```rust
    /// Runs the guest's display recipe, at the mode the host asked for.
    pub apply_display_recipe:
        &'a mut dyn FnMut(Option<(u32, u32)>) -> ApplyDisplayRecipeResponse,
```

and its call site:

```rust
            Body::Request(request::Kind::ApplyDisplayRecipe(ref request))
                if session.capabilities.contains(&Capability::Display) =>
            {
                let mode = request
                    .initial_mode
                    .as_ref()
                    .map(|mode| (mode.width, mode.height));
                let report = Envelope::response(
                    request_id,
                    response::Kind::ApplyDisplayRecipe((handlers.apply_display_recipe)(mode)),
                );
                frame::write(stream, &report, buffer).map_err(SessionError::Frame)?;
            }
```

In `crates/agent/src/main.rs`:

```rust
            apply_display_recipe: &mut |mode| {
                let (stages, versions) = display_kernel::apply(&STOPPING, mode);
                ApplyDisplayRecipeResponse {
                    stages,
                    versions: Some(versions),
                }
            },
```

Any other `Handlers` literal — `session.rs`'s own tests — needs the closure's argument added.

- [ ] **Step 7: Drop the payload's copy of the mode**

```bash
git rm payloads/display/module/vmlord-display.conf
```

and remove `/src/module/vmlord-display.conf` from the `cp` list in `payloads/display/Dockerfile`'s `layout` stage. The constant now lives in `display_recipe::FALLBACK_MODE` and in the module's own parameter defaults, and a third copy that nothing reads is worse than either.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS across the workspace.

- [ ] **Step 9: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add crates/agent payloads/display
git commit -m "TASK-114: Bring the output up at the mode the VM has stored

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 10: What the documentation now says

**Files:**
- Modify: `payloads/display/module/README.md`
- Modify: `docs/display-drm-backend.md`
- Modify: `ARCHITECTURE.md:3017-3022`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing code depends on.

- [ ] **Step 1: Rewrite the module README's two stale sections**

In `payloads/display/module/README.md`:

- "What it is" — the first line becomes "One CRTC, one connector, a primary plane and a cursor plane, GEM shmem buffers, an hrtimer vblank, atomic modesetting and PRIME export."
- "What it is not, yet" — the whole section is about task #114 and is now done. Replace it with what is *still* not here: no synthesized EDID, so the connector reports a physical size at 96 DPI and no monitor name; one CRTC and one connector, so no multi-monitor (#130); no capture, which is #115's and reads this device as an ordinary DRM client.
- "Kernels" — the paragraph saying the `hrtimer_setup` guard "belongs with the vblank work of task #114" becomes a statement that it is in `vmlord_compat.h` as `vmlord_hrtimer_setup`, and the count moves from "two API moves ... and one in `vmlord_compat.h`" to two in `vmlord_drm.c` and two in `vmlord_compat.h`.
- "Building" — note that `VMLORD_VERSION` defaults to `0.0.0-dev` and that a packed payload's `Kbuild` carries the payload's version.

- [ ] **Step 2: Close out the research document's open list**

In `docs/display-drm-backend.md`, the final "What remains is not research" list has four bullets. Two are now done and must say so rather than be deleted — this document is a record:

- the module's name and packaging: done in #113;
- the mode list above 1920x1080: done in #114, which offers the standard list up to 2560x1440 and refuses a module parameter outside 640x480..2560x1440;
- 22.04 and 26.04 builds: still open, and 22.04's 5.15 now compiles on the development machine after every change, which is evidence and still not a booted guest;
- the capture backend: still #115.

Do not touch the measurements above that list. They are what the document is for.

- [ ] **Step 3: Update the architecture's display sections**

In `ARCHITECTURE.md`, the paragraph beginning "`vmlord_drm` itself is one CRTC, one connector, a primary plane" ends "The cursor plane, the mode list and a real vblank are task #114's." Rewrite the paragraph so that it describes what the module now is — a cursor plane mutter puts the pointer on, modes up to 2560x1440 with a physical size and no EDID, and an hrtimer vblank that paces commits — and keeps the three #111 decisions unchanged. Add that the module declares the payload's version, which is what the update verification compares against.

In the recipe paragraph above it, `MODULE_LOAD` is described as "`modules-load.d`, the modprobe options, the unit that unbinds `simple-framebuffer`, and `modprobe`". Say that the modprobe options are now written by the guest from the mode the host sent, that an absent mode is 1920x1080, and that a mode that changed under a loaded module costs a reload.

- [ ] **Step 4: Check the documentation against the code**

```bash
grep -rn "task #114\|#114's\|is task #114" ARCHITECTURE.md docs/ payloads/
```

Expected: no line that still promises #114 as future work, except inside `docs/superpowers/` (the spec and this plan, which are records of the task and stay as written).

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md docs/display-drm-backend.md payloads/display/module/README.md
git commit -m "TASK-114: Record the guest DRM output in the documentation

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Finishing

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — all clean.
- [ ] The module compiles against 5.15 from a clean copy.
- [ ] The report to the user states plainly that **6.8 and 7.x are unverified** for want of a container runtime, and that no part of this task has run inside a guest — the runtime proof is #128's Ubuntu matrix.
