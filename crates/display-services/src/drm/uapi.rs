//! The kernel's DRM ABI, written out rather than linked.
//!
//! No system `libdrm`: linking one would cost the toolchain-free
//! cross-compilation the whole guest side rests on, and what is needed here is
//! seven ioctls and eight structures. Every item is named after the kernel's
//! own spelling, so that a reader can find it in `drm.h`, `drm_mode.h`,
//! `drm_fourcc.h` and `dma-buf.h`.

/// `_IOC(_IOC_WRITE, ..)`, the encoding `_IOW` builds.
#[must_use]
pub const fn io_write(kind: u32, number: u32, size: u32) -> libc::c_ulong {
    ((1 << 30) | (size << 16) | (kind << 8) | number) as libc::c_ulong
}

/// `_IOC(_IOC_READ | _IOC_WRITE, ..)`, the encoding `_IOWR` builds.
#[must_use]
pub const fn io_write_read(kind: u32, number: u32, size: u32) -> libc::c_ulong {
    ((3 << 30) | (size << 16) | (kind << 8) | number) as libc::c_ulong
}

/// The `'d'` every DRM request is built on.
const DRM: u32 = 0x64;

/// The `'b'` dma-buf's own requests are built on.
const DMA_BUF: u32 = 0x62;

/// Releases a GEM handle `GETFB2` created in this file.
pub const DRM_IOCTL_GEM_CLOSE: libc::c_ulong = io_write(DRM, 0x09, size_of::<DrmGemClose>() as u32);

/// Asks for behaviour a plain client does not get, such as universal planes.
pub const DRM_IOCTL_SET_CLIENT_CAP: libc::c_ulong =
    io_write(DRM, 0x0d, size_of::<DrmSetClientCap>() as u32);

/// Exports a GEM handle as a dma-buf descriptor.
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: libc::c_ulong =
    io_write_read(DRM, 0x2d, size_of::<DrmPrimeHandle>() as u32);

/// Waits for the output's clock, which for task #114's module is an hrtimer.
pub const DRM_IOCTL_WAIT_VBLANK: libc::c_ulong =
    io_write_read(DRM, 0x3a, size_of::<DrmWaitVblank>() as u32);

/// Lists the device's planes, once universal planes are asked for.
pub const DRM_IOCTL_MODE_GETPLANERESOURCES: libc::c_ulong =
    io_write_read(DRM, 0xb5, size_of::<DrmModeGetPlaneRes>() as u32);

/// One plane's current CRTC and framebuffer.
pub const DRM_IOCTL_MODE_GETPLANE: libc::c_ulong =
    io_write_read(DRM, 0xb6, size_of::<DrmModeGetPlane>() as u32);

/// One object's properties and their current values.
pub const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: libc::c_ulong =
    io_write_read(DRM, 0xb9, size_of::<DrmModeObjGetProperties>() as u32);

/// One property's name, which is how a property id is recognised.
pub const DRM_IOCTL_MODE_GETPROPERTY: libc::c_ulong =
    io_write_read(DRM, 0xaa, size_of::<DrmModeGetProperty>() as u32);

/// A framebuffer's layout and its GEM handles. Needs `CAP_SYS_ADMIN`.
pub const DRM_IOCTL_MODE_GETFB2: libc::c_ulong =
    io_write_read(DRM, 0xce, size_of::<DrmModeFbCmd2>() as u32);

/// Brackets a CPU read of a dma-buf so its caches are coherent.
pub const DMA_BUF_IOCTL_SYNC: libc::c_ulong =
    io_write(DMA_BUF, 0x00, size_of::<DmaBufSync>() as u32);

/// Blue, green, red, one ignored byte. What a desktop's primary plane is.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3458_5220;

/// The same with an alpha channel. What a cursor plane is.
pub const DRM_FORMAT_ARGB8888: u32 = 0x3443_5241;

/// The only modifier a capture that mmaps a buffer can read.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Lets a client see the cursor and overlay planes, not just the primary.
pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;

/// `DRM_MODE_OBJECT_PLANE`, for the object whose properties are being read.
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;

/// Waits for a vblank this many from now, rather than for an absolute one.
pub const DRM_VBLANK_RELATIVE: u32 = 0x1;

/// The width of a property name, which is fixed by the ABI.
pub const DRM_PROP_NAME_LEN: usize = 32;

/// The start of a CPU read of a dma-buf.
pub const DMA_BUF_SYNC_READ: u64 = 1 << 0;
/// The read is beginning.
pub const DMA_BUF_SYNC_START: u64 = 0 << 2;
/// The read is over.
pub const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// `struct drm_gem_close`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

/// `struct drm_set_client_cap`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_prime_handle`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    /// `O_CLOEXEC` alone. Never `DRM_RDWR`: that is what makes the exported
    /// buffer read-only, which is the whole promise the broker makes.
    pub flags: u32,
    pub fd: i32,
}

/// `union drm_wait_vblank`, in its reply shape, which is the larger arm.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmWaitVblank {
    pub kind: u32,
    pub sequence: u32,
    pub tval_sec: i64,
    pub tval_usec: i64,
}

/// `struct drm_mode_get_plane_res`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
    pub pad: u32,
}

/// `struct drm_mode_get_plane`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

/// `struct drm_mode_obj_get_properties`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmModeObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
    pub pad: u32,
}

/// `struct drm_mode_get_property`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmModeGetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [libc::c_char; DRM_PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

impl Default for DrmModeGetProperty {
    fn default() -> Self {
        Self {
            values_ptr: 0,
            enum_blob_ptr: 0,
            prop_id: 0,
            flags: 0,
            name: [0; DRM_PROP_NAME_LEN],
            count_values: 0,
            count_enum_blobs: 0,
        }
    }
}

/// `struct drm_mode_fb_cmd2`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

/// `struct dma_buf_sync`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DmaBufSync {
    pub flags: u64,
}
