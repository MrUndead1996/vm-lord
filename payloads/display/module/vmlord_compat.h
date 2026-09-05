/* SPDX-License-Identifier: GPL-2.0 */
/*
 * What moved between the kernels VMLord supports.
 *
 * Ubuntu 22.04 runs 5.15, 24.04 runs 6.8 and 26.04 runs 7.x, and the DRM API
 * does not stand still across that span. Each guard here names the release that
 * moved the thing it guards; the two guards that belong to a struct initializer
 * -- platform_driver::remove and drm_driver::date -- live at their definition
 * sites in vmlord_drm.c instead, because an #if around a field reads better
 * than a macro that hides one.
 */

#ifndef VMLORD_COMPAT_H
#define VMLORD_COMPAT_H

#include <linux/hrtimer.h>
#include <linux/version.h>

#include <drm/drm_plane_helper.h>

/* Renamed from DRM_PLANE_HELPER_NO_SCALING in 6.1. */
#ifndef DRM_PLANE_NO_SCALING
#define DRM_PLANE_NO_SCALING DRM_PLANE_HELPER_NO_SCALING
#endif

/*
 * drm_simple_encoder_funcs_cleanup lost its last user in 6.x, and an encoder
 * that owns nothing needs no funcs at all -- but 5.15 will not take NULL.
 */
static const struct drm_encoder_funcs vmlord_encoder_funcs = {
	.destroy = drm_encoder_cleanup,
};

/*
 * drm_atomic_state was renamed drm_atomic_commit in 7.2, tree-wide and with no
 * alias left behind: the tag with the old name does not exist on such a kernel,
 * which is why a build against it fails on the helper vtables rather than on
 * anything this module does with the object. The hooks here only hand the
 * pointer back to the accessors, so naming the type is the whole difference.
 *
 * Arch is what reached 7.2 first; the Ubuntu releases the payload is gated on
 * are still below it, which is why this did not surface until a guest built
 * the module itself.
 */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(7, 2, 0)
#define vmlord_atomic_state drm_atomic_commit
#else
#define vmlord_atomic_state drm_atomic_state
#endif

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

#endif /* VMLORD_COMPAT_H */
