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

#endif /* VMLORD_COMPAT_H */
