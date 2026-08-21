// SPDX-License-Identifier: GPL-2.0
/*
 * VMLord virtual display.
 *
 * One CRTC, one connector, one primary plane, GEM shmem buffers and PRIME
 * export: the smallest DRM device a Wayland compositor will bind and a capture
 * client can read out of. Nothing here scans out anywhere -- the framebuffer a
 * compositor commits is the product, and VMLord's capture service reads it as
 * an ordinary DRM client through drmModeGetFB2 and a PRIME fd.
 *
 * Three things about this driver are decisions rather than style, and task
 * #111 measured all three:
 *
 *   - it is a platform device under its own name. Mutter's udev rules tag
 *     `platform-vkms` with `mutter-device-ignore`, matched on ID_PATH, so a
 *     virtual display named after vkms -- or registered on the faux bus, whose
 *     devices get similar treatment -- is a display no compositor will use;
 *   - it does not set DRIVER_CURSOR_HOTSPOT. Mutter hides the cursor plane of
 *     drivers that declare it unless they are on its allowlist;
 *   - its formats are XRGB8888 and ARGB8888 with the linear modifier only,
 *     because a capture client that mmaps the buffer cannot detile anything
 *     else.
 */

#include <linux/module.h>
#include <linux/moduleparam.h>
#include <linux/platform_device.h>
#include <linux/version.h>

#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_connector.h>
#include <drm/drm_crtc.h>
#include <drm/drm_drv.h>
/*
 * Every DRM header this file needs is included by name, and none is relied on
 * to arrive through another: 5.15 pulled drm_edid.h in transitively and 6.8
 * does not, which is one implicit declaration and a build that fails.
 */
#include <drm/drm_edid.h>
#include <drm/drm_encoder.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_gem.h>
#include <drm/drm_gem_shmem_helper.h>
#include <drm/drm_managed.h>
#include <drm/drm_modes.h>
#include <drm/drm_modeset_helper_vtables.h>
#include <drm/drm_plane.h>
#include <drm/drm_print.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_vblank.h>

#include "vmlord_compat.h"

#define VMLORD_DRIVER_NAME "vmlord_drm"

static unsigned int width = 1920;
static unsigned int height = 1080;

module_param(width, uint, 0444);
MODULE_PARM_DESC(width, "width of the virtual output in pixels");
module_param(height, uint, 0444);
MODULE_PARM_DESC(height, "height of the virtual output in pixels");

struct vmlord_device {
	struct drm_device drm;
	struct drm_crtc crtc;
	struct drm_encoder encoder;
	struct drm_connector connector;
	struct drm_plane primary;
};

static const uint32_t vmlord_formats[] = {
	DRM_FORMAT_XRGB8888,
	DRM_FORMAT_ARGB8888,
};

static const uint64_t vmlord_modifiers[] = {
	DRM_FORMAT_MOD_LINEAR,
	DRM_FORMAT_MOD_INVALID,
};

/* ------------------------------------------------------------------ plane */

static int vmlord_plane_atomic_check(struct drm_plane *plane,
				     struct drm_atomic_state *state)
{
	struct drm_plane_state *new_state =
		drm_atomic_get_new_plane_state(state, plane);
	struct drm_crtc_state *crtc_state;

	if (!new_state->crtc)
		return 0;

	crtc_state = drm_atomic_get_new_crtc_state(state, new_state->crtc);
	if (!crtc_state)
		return -EINVAL;

	return drm_atomic_helper_check_plane_state(new_state, crtc_state,
						   DRM_PLANE_NO_SCALING,
						   DRM_PLANE_NO_SCALING,
						   false, false);
}

/*
 * Nothing. There is no scanout engine to program: the framebuffer bound to
 * this plane's state is what a capture client walks the planes to find.
 */
static void vmlord_plane_atomic_update(struct drm_plane *plane,
				       struct drm_atomic_state *state)
{
}

static const struct drm_plane_helper_funcs vmlord_plane_helper_funcs = {
	.atomic_check = vmlord_plane_atomic_check,
	.atomic_update = vmlord_plane_atomic_update,
};

static const struct drm_plane_funcs vmlord_plane_funcs = {
	.update_plane = drm_atomic_helper_update_plane,
	.disable_plane = drm_atomic_helper_disable_plane,
	.destroy = drm_plane_cleanup,
	.reset = drm_atomic_helper_plane_reset,
	.atomic_duplicate_state = drm_atomic_helper_plane_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_plane_destroy_state,
};

/* ------------------------------------------------------------------- crtc */

static void vmlord_crtc_atomic_enable(struct drm_crtc *crtc,
				      struct drm_atomic_state *state)
{
}

static void vmlord_crtc_atomic_disable(struct drm_crtc *crtc,
				       struct drm_atomic_state *state)
{
}

/*
 * Completes the commit immediately.
 *
 * This driver reports no vblank counter and arms no timer -- there is no
 * refresh to be in phase with until task #114 gives the output one. A
 * compositor waiting on the event of a commit that will never be answered is a
 * compositor that stops drawing, so the event is sent here, in the flush, and
 * the commit is as complete as it is ever going to be.
 */
static void vmlord_crtc_atomic_flush(struct drm_crtc *crtc,
				     struct drm_atomic_state *state)
{
	struct drm_pending_vblank_event *event = crtc->state->event;

	if (!event)
		return;

	crtc->state->event = NULL;
	spin_lock_irq(&crtc->dev->event_lock);
	drm_crtc_send_vblank_event(crtc, event);
	spin_unlock_irq(&crtc->dev->event_lock);
}

static const struct drm_crtc_helper_funcs vmlord_crtc_helper_funcs = {
	.atomic_enable = vmlord_crtc_atomic_enable,
	.atomic_disable = vmlord_crtc_atomic_disable,
	.atomic_flush = vmlord_crtc_atomic_flush,
};

static const struct drm_crtc_funcs vmlord_crtc_funcs = {
	.set_config = drm_atomic_helper_set_config,
	.page_flip = drm_atomic_helper_page_flip,
	.destroy = drm_crtc_cleanup,
	.reset = drm_atomic_helper_crtc_reset,
	.atomic_duplicate_state = drm_atomic_helper_crtc_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_crtc_destroy_state,
};

/* -------------------------------------------------------------- connector */

/*
 * The modes this output offers.
 *
 * The module's own size, marked preferred, plus the standard list below it, so
 * that a compositor started before anything has been configured comes up at
 * the size the host asked for. A synthesized EDID -- which is what would carry
 * a physical size and a richer mode list -- belongs with the resizing work of
 * task #114, and inventing one here would be a mode list nothing has been
 * measured against.
 */
static int vmlord_connector_get_modes(struct drm_connector *connector)
{
	struct drm_display_mode *mode;
	int count;

	count = drm_add_modes_noedid(connector, width, height);
	mode = drm_cvt_mode(connector->dev, width, height, 60, false, false,
			    false);
	if (mode) {
		mode->type |= DRM_MODE_TYPE_PREFERRED;
		drm_mode_probed_add(connector, mode);
		count++;
	}

	return count;
}

static const struct drm_connector_helper_funcs vmlord_connector_helper_funcs = {
	.get_modes = vmlord_connector_get_modes,
};

static const struct drm_connector_funcs vmlord_connector_funcs = {
	.fill_modes = drm_helper_probe_single_connector_modes,
	.destroy = drm_connector_cleanup,
	.reset = drm_atomic_helper_connector_reset,
	.atomic_duplicate_state = drm_atomic_helper_connector_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_connector_destroy_state,
};

/* ------------------------------------------------------------------ device */

static const struct drm_mode_config_funcs vmlord_mode_config_funcs = {
	.fb_create = drm_gem_fb_create,
	.atomic_check = drm_atomic_helper_check,
	.atomic_commit = drm_atomic_helper_commit,
};

/*
 * The generic GEM fops: open, mmap, poll, ioctl. Not the shmem-specific
 * variant, which 5.15 does not have -- shmem's own behaviour arrives through
 * DRM_GEM_SHMEM_DRIVER_OPS and the object's funcs.
 */
DEFINE_DRM_GEM_FOPS(vmlord_drm_fops);

static struct drm_driver vmlord_drm_driver = {
	.driver_features = DRIVER_GEM | DRIVER_MODESET | DRIVER_ATOMIC,
	.fops = &vmlord_drm_fops,
	.name = VMLORD_DRIVER_NAME,
	.desc = "VMLord virtual display",
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 14, 0)
	/*
	 * drm_version() copies drm_driver::date unconditionally until 6.14
	 * removed the field. Left NULL it WARNs in drm_copy_field() and hands
	 * userspace a NULL string, which segfaults drm_info -- and nothing
	 * catches it at build time.
	 */
	.date = "20260821",
#endif
	DRM_GEM_SHMEM_DRIVER_OPS,
};

static int vmlord_mode_config_init(struct vmlord_device *vmlord)
{
	struct drm_device *drm = &vmlord->drm;
	int error;

	error = drmm_mode_config_init(drm);
	if (error)
		return error;

	drm->mode_config.min_width = 640;
	drm->mode_config.min_height = 480;
	drm->mode_config.max_width = 4096;
	drm->mode_config.max_height = 4096;
	drm->mode_config.preferred_depth = 24;
	drm->mode_config.funcs = &vmlord_mode_config_funcs;

	return 0;
}

static int vmlord_pipe_init(struct vmlord_device *vmlord)
{
	struct drm_device *drm = &vmlord->drm;
	int error;

	error = drm_universal_plane_init(drm, &vmlord->primary, 1,
					 &vmlord_plane_funcs, vmlord_formats,
					 ARRAY_SIZE(vmlord_formats),
					 vmlord_modifiers,
					 DRM_PLANE_TYPE_PRIMARY, "primary");
	if (error)
		return error;
	drm_plane_helper_add(&vmlord->primary, &vmlord_plane_helper_funcs);

	error = drm_crtc_init_with_planes(drm, &vmlord->crtc, &vmlord->primary,
					  NULL, &vmlord_crtc_funcs, NULL);
	if (error)
		return error;
	drm_crtc_helper_add(&vmlord->crtc, &vmlord_crtc_helper_funcs);

	vmlord->encoder.possible_crtcs = drm_crtc_mask(&vmlord->crtc);
	error = drm_encoder_init(drm, &vmlord->encoder, &vmlord_encoder_funcs,
				 DRM_MODE_ENCODER_VIRTUAL, NULL);
	if (error)
		return error;

	error = drm_connector_init(drm, &vmlord->connector,
				   &vmlord_connector_funcs,
				   DRM_MODE_CONNECTOR_VIRTUAL);
	if (error)
		return error;
	drm_connector_helper_add(&vmlord->connector,
				 &vmlord_connector_helper_funcs);
	vmlord->connector.status = connector_status_connected;

	return drm_connector_attach_encoder(&vmlord->connector,
					    &vmlord->encoder);
}

static int vmlord_probe(struct platform_device *pdev)
{
	struct vmlord_device *vmlord;
	int error;

	vmlord = devm_drm_dev_alloc(&pdev->dev, &vmlord_drm_driver,
				    struct vmlord_device, drm);
	if (IS_ERR(vmlord))
		return PTR_ERR(vmlord);

	platform_set_drvdata(pdev, vmlord);

	error = vmlord_mode_config_init(vmlord);
	if (error)
		return error;

	error = vmlord_pipe_init(vmlord);
	if (error)
		return error;

	drm_mode_config_reset(&vmlord->drm);

	error = drm_dev_register(&vmlord->drm, 0);
	if (error)
		return error;

	drm_info(&vmlord->drm, "VMLord virtual display at %ux%u\n", width,
		 height);
	return 0;
}

static void vmlord_remove(struct platform_device *pdev)
{
	struct vmlord_device *vmlord = platform_get_drvdata(pdev);

	drm_dev_unplug(&vmlord->drm);
	drm_atomic_helper_shutdown(&vmlord->drm);
}

#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 1, 0)
/*
 * platform_driver::remove returned int until 6.11, and between 6.1 and 6.10
 * the void-returning callback lived in ::remove_new. Only the oldest of the
 * three needs a wrapper.
 */
static int vmlord_remove_int(struct platform_device *pdev)
{
	vmlord_remove(pdev);
	return 0;
}
#endif

static struct platform_driver vmlord_platform_driver = {
	.driver = {
		.name = VMLORD_DRIVER_NAME,
	},
	.probe = vmlord_probe,
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 11, 0)
	.remove = vmlord_remove,
#elif LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
	.remove_new = vmlord_remove,
#else
	.remove = vmlord_remove_int,
#endif
};

static struct platform_device *vmlord_platform_device;

static int __init vmlord_drm_init(void)
{
	int error;

	error = platform_driver_register(&vmlord_platform_driver);
	if (error)
		return error;

	vmlord_platform_device =
		platform_device_register_simple(VMLORD_DRIVER_NAME, 0, NULL, 0);
	if (IS_ERR(vmlord_platform_device)) {
		platform_driver_unregister(&vmlord_platform_driver);
		return PTR_ERR(vmlord_platform_device);
	}

	return 0;
}

static void __exit vmlord_drm_exit(void)
{
	platform_device_unregister(vmlord_platform_device);
	platform_driver_unregister(&vmlord_platform_driver);
}

module_init(vmlord_drm_init);
module_exit(vmlord_drm_exit);

MODULE_AUTHOR("VMLord contributors");
MODULE_DESCRIPTION("VMLord virtual display");
MODULE_LICENSE("GPL");
