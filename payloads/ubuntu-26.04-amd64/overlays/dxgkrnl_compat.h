/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Compatibility shims for building dxgkrnl against a distribution kernel.
 *
 * dxgkrnl is written against microsoft/WSL2-Linux-Kernel, whose
 * <linux/hyperv.h> declares the GUIDs of the two GPU paravirtualization
 * channels. Ubuntu's kernel headers carry neither, because the driver was
 * never merged upstream, so the two definitions travel with the driver
 * instead. They are copied verbatim from the same pinned commit the sources
 * come from, and are guarded so that a kernel which one day declares them
 * wins over this file.
 *
 * Kbuild force-includes this header into every translation unit, which is
 * what lets the driver sources stay byte-for-byte upstream.
 */
#ifndef VMLORD_DXGKRNL_COMPAT_H
#define VMLORD_DXGKRNL_COMPAT_H

#include <linux/hyperv.h>

/*
 * GPU paravirtualization global DXGK channel
 * {DDE9CBC0-5060-4436-9448-EA1254A5D177}
 */
#ifndef HV_GPUP_DXGK_GLOBAL_GUID
#define HV_GPUP_DXGK_GLOBAL_GUID \
	.guid = GUID_INIT(0xdde9cbc0, 0x5060, 0x4436, 0x94, 0x48, \
			  0xea, 0x12, 0x54, 0xa5, 0xd1, 0x77)
#endif

/*
 * GPU paravirtualization per virtual GPU DXGK channel
 * {6E382D18-3336-4F4B-ACC4-2B7703D4DF4A}
 */
#ifndef HV_GPUP_DXGK_VGPU_GUID
#define HV_GPUP_DXGK_VGPU_GUID \
	.guid = GUID_INIT(0x6e382d18, 0x3336, 0x4f4b, 0xac, 0xc4, \
			  0x2b, 0x77, 0x3, 0xd4, 0xdf, 0x4a)
#endif

#endif /* VMLORD_DXGKRNL_COMPAT_H */
