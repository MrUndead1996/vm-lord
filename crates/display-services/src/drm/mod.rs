//! The one place this crate speaks to a DRM device.
//!
//! An ordinary DRM client and never the master -- the compositor holds that --
//! which is what task #111 proved a capture backend can be. What it needs
//! beyond an ordinary client is `CAP_SYS_ADMIN`, because `GETFB2` will not hand
//! a framebuffer's handles to anyone else; that capability is the entire reason
//! this code runs in a separate, privileged process.
//!
//! Nothing below leaves this module. What the unprivileged process is given is
//! a read-only dma-buf and a layout, never a device descriptor and never an
//! ioctl of its own.

pub mod uapi;

use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    path::Path,
    ptr,
};

use crate::ipc::PlaneKind;

use uapi::{
    DMA_BUF_IOCTL_SYNC, DRM_CLIENT_CAP_UNIVERSAL_PLANES, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DRM_IOCTL_DROP_MASTER, DRM_IOCTL_GEM_CLOSE,
    DRM_IOCTL_MODE_GETFB2,
    DRM_IOCTL_MODE_GETPLANE, DRM_IOCTL_MODE_GETPLANERESOURCES, DRM_IOCTL_MODE_GETPROPERTY,
    DRM_IOCTL_MODE_OBJ_GETPROPERTIES, DRM_IOCTL_PRIME_HANDLE_TO_FD, DRM_IOCTL_SET_CLIENT_CAP,
    DRM_IOCTL_WAIT_VBLANK, DRM_MODE_OBJECT_PLANE, DRM_VBLANK_RELATIVE, DmaBufSync, DrmGemClose,
    DrmModeFbCmd2, DrmModeGetPlane, DrmModeGetPlaneRes, DrmModeGetProperty,
    DrmModeObjGetProperties, DrmPrimeHandle, DrmSetClientCap, DrmWaitVblank,
};

/// Where the kernel lists the DRM devices a machine has.
pub const DRM_CLASS: &str = "/sys/class/drm";

/// Where their device nodes are.
pub const DRM_DEVICES: &str = "/dev/dri";

/// `DRM_PLANE_TYPE_PRIMARY`.
const PLANE_TYPE_PRIMARY: u64 = 1;
/// `DRM_PLANE_TYPE_CURSOR`.
const PLANE_TYPE_CURSOR: u64 = 2;

/// One plane's framebuffer and where it sits, at one vblank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneState {
    /// Which plane this is.
    pub kind: PlaneKind,
    /// The framebuffer id, which also names the exported descriptor.
    pub fb_id: u32,
    /// The framebuffer's width in pixels.
    pub width: u32,
    /// The framebuffer's height in pixels.
    pub height: u32,
    /// Bytes per row, which is not promised to be `width * 4`.
    pub stride: u32,
    /// The DRM fourcc.
    pub format: u32,
    /// The plane's left edge on the CRTC, negative at the left edge.
    pub x: i32,
    /// The plane's top edge on the CRTC, negative at the top edge.
    pub y: i32,
}

/// The guest's own output, opened for reading.
pub struct Device {
    descriptor: OwnedFd,
    /// The exported buffers, by framebuffer id. Exporting costs a syscall and a
    /// descriptor, and a compositor cycles through a handful of buffers, so
    /// they are kept rather than re-exported every frame.
    buffers: HashMap<u32, OwnedFd>,
    /// Property names by id, learned once. Ids are per device and do not move.
    property_names: HashMap<u32, String>,
}

/// The card whose driver has this name, if the machine has one.
///
/// By driver rather than by number: a guest that also has `hyperv_drm` has a
/// `/dev/dri/card0` that is not ours.
///
/// # Errors
///
/// [`io::Error`] if the class directory exists but cannot be read. A machine
/// with no DRM class at all -- this development machine, and a guest whose
/// module has not loaded yet -- is `Ok(None)`, because the broker waits for a
/// card rather than failing without one.
pub fn card_named(driver: &str, sysfs_class: &Path) -> io::Result<Option<String>> {
    let entries = match fs::read_dir(sysfs_class) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    let mut cards: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") {
            continue;
        }
        let link = entry.path().join("device").join("driver");
        let Ok(target) = fs::read_link(&link) else {
            continue;
        };
        if target.file_name().is_some_and(|found| found == driver) {
            cards.push(name);
        }
    }

    cards.sort();
    Ok(cards.into_iter().next())
}

/// Whether this build can read a framebuffer of this format and modifier.
///
/// Task #111 fixed the answer and task #114 implements it: a capture that mmaps
/// a buffer cannot detile anything, and nothing here composites a planar
/// format.
#[must_use]
pub fn format_is_mappable(format: u32, modifier: u64) -> bool {
    matches!(format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) && modifier == DRM_FORMAT_MOD_LINEAR
}

impl Device {
    /// Opens the card this module drives, if it is there yet.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the card is there and cannot be opened or will not
    /// grant universal planes.
    pub fn find(driver: &str, sysfs_class: &Path, dev_root: &Path) -> io::Result<Option<Self>> {
        let Some(card) = card_named(driver, sysfs_class)? else {
            return Ok(None);
        };

        let path = crate::unix::c_string(&dev_root.join(card))?;
        // SAFETY: `path` is a NUL-terminated path that lives across the call.
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `open` returned a descriptor this process now owns.
        let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };

        // The kernel makes the first client to open a primary node its master,
        // asked for or not, and this broker starts at boot -- before any
        // compositor. Holding it would answer the compositor's `SET_MASTER`
        // with `EBUSY`, and the desktop would light some other card instead.
        // Nothing here needs it: reading planes and waiting for the clock are
        // an ordinary client's business.
        //
        // SAFETY: `descriptor` is an owned descriptor for a DRM primary node,
        // and `DROP_MASTER` takes no argument -- the null pointer is what the
        // ABI expects for an `_IO` request.
        let dropped = unsafe { libc::ioctl(descriptor.as_raw_fd(), DRM_IOCTL_DROP_MASTER as _, 0) };
        if dropped < 0 {
            // `EINVAL` is the answer when this process was not master, which is
            // the ordinary case for a broker that restarted while the desktop
            // was up. Anything else is worth a line and is still not fatal.
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) {
                eprintln!("vmlord-display-broker: could not drop DRM master: {error}");
            }
        }

        let mut capability = DrmSetClientCap {
            capability: DRM_CLIENT_CAP_UNIVERSAL_PLANES,
            value: 1,
        };
        ioctl(
            descriptor.as_raw_fd(),
            DRM_IOCTL_SET_CLIENT_CAP,
            &mut capability,
        )?;

        Ok(Some(Self {
            descriptor,
            buffers: HashMap::new(),
            property_names: HashMap::new(),
        }))
    }

    /// Waits for the next vblank and returns the sequence it carried.
    ///
    /// This is task #114's hrtimer, and it is the only clock this output has.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the wait fails for anything but a signal.
    pub fn wait_vblank(&self) -> io::Result<u32> {
        loop {
            let mut request = DrmWaitVblank {
                kind: DRM_VBLANK_RELATIVE,
                sequence: 1,
                ..DrmWaitVblank::default()
            };
            match ioctl(
                self.descriptor.as_raw_fd(),
                DRM_IOCTL_WAIT_VBLANK,
                &mut request,
            ) {
                Ok(()) => return Ok(request.sequence),
                Err(error)
                    if matches!(error.kind(), ErrorKind::Interrupted)
                        || error.raw_os_error() == Some(libc::EAGAIN) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// What the planes hold right now.
    ///
    /// Planes with no framebuffer are left out, which is how a hidden cursor
    /// reports itself.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a format or modifier this build cannot
    /// map -- the module or the compositor changed underneath us -- and
    /// [`io::Error`] for a failed ioctl.
    pub fn snapshot(&mut self) -> io::Result<Vec<PlaneState>> {
        let mut states = Vec::new();
        let mut seen = Vec::new();

        for plane_id in self.plane_ids()? {
            let mut plane = DrmModeGetPlane {
                plane_id,
                ..DrmModeGetPlane::default()
            };
            ioctl(
                self.descriptor.as_raw_fd(),
                DRM_IOCTL_MODE_GETPLANE,
                &mut plane,
            )?;
            if plane.fb_id == 0 || plane.crtc_id == 0 {
                continue;
            }

            let properties = self.plane_properties(plane_id)?;
            let kind = match properties.get("type").copied() {
                Some(PLANE_TYPE_PRIMARY) => PlaneKind::Primary,
                Some(PLANE_TYPE_CURSOR) => PlaneKind::Cursor,
                // An overlay is not something this build composites, and task
                // #114's output has none. Ignoring it is right; failing on it
                // would make a future overlay break the desktop.
                _ => continue,
            };

            let framebuffer = self.framebuffer(plane.fb_id)?;
            if !format_is_mappable(framebuffer.pixel_format, framebuffer.modifier[0]) {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "framebuffer {} is format {:#010x} modifier {:#018x}, which this build cannot map",
                        plane.fb_id, framebuffer.pixel_format, framebuffer.modifier[0]
                    ),
                ));
            }

            self.export(plane.fb_id, framebuffer.handles[0])?;
            seen.push(plane.fb_id);
            states.push(PlaneState {
                kind,
                fb_id: plane.fb_id,
                width: framebuffer.width,
                height: framebuffer.height,
                stride: framebuffer.pitches[0],
                format: framebuffer.pixel_format,
                x: signed(properties.get("CRTC_X").copied().unwrap_or(0)),
                y: signed(properties.get("CRTC_Y").copied().unwrap_or(0)),
            });
        }

        // A framebuffer nothing scans out any more is a descriptor nobody will
        // ask for again.
        self.buffers.retain(|fb_id, _| seen.contains(fb_id));

        Ok(states)
    }

    /// The exported buffer behind a framebuffer id, for sending on.
    #[must_use]
    pub fn buffer(&self, fb_id: u32) -> Option<BorrowedFd<'_>> {
        self.buffers.get(&fb_id).map(AsFd::as_fd)
    }

    fn plane_ids(&self) -> io::Result<Vec<u32>> {
        let mut resources = DrmModeGetPlaneRes::default();
        ioctl(
            self.descriptor.as_raw_fd(),
            DRM_IOCTL_MODE_GETPLANERESOURCES,
            &mut resources,
        )?;

        let mut ids = vec![0u32; resources.count_planes as usize];
        if ids.is_empty() {
            return Ok(ids);
        }
        resources.plane_id_ptr = ids.as_mut_ptr() as u64;
        ioctl(
            self.descriptor.as_raw_fd(),
            DRM_IOCTL_MODE_GETPLANERESOURCES,
            &mut resources,
        )?;
        ids.truncate(resources.count_planes as usize);

        Ok(ids)
    }

    /// One plane's properties, by name.
    ///
    /// `GETPLANE` reports the framebuffer and the CRTC but not the position, so
    /// the position has to come from the property values.
    fn plane_properties(&mut self, plane_id: u32) -> io::Result<HashMap<String, u64>> {
        let mut request = DrmModeObjGetProperties {
            obj_id: plane_id,
            obj_type: DRM_MODE_OBJECT_PLANE,
            ..DrmModeObjGetProperties::default()
        };
        ioctl(
            self.descriptor.as_raw_fd(),
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
            &mut request,
        )?;

        let count = request.count_props as usize;
        let mut ids = vec![0u32; count];
        let mut values = vec![0u64; count];
        if count > 0 {
            request.props_ptr = ids.as_mut_ptr() as u64;
            request.prop_values_ptr = values.as_mut_ptr() as u64;
            ioctl(
                self.descriptor.as_raw_fd(),
                DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
                &mut request,
            )?;
        }

        let mut properties = HashMap::new();
        for (id, value) in ids.into_iter().zip(values) {
            let name = self.property_name(id)?;
            properties.insert(name, value);
        }

        Ok(properties)
    }

    /// A property's name, asked for once per device.
    fn property_name(&mut self, prop_id: u32) -> io::Result<String> {
        if let Some(name) = self.property_names.get(&prop_id) {
            return Ok(name.clone());
        }

        let mut request = DrmModeGetProperty {
            prop_id,
            ..DrmModeGetProperty::default()
        };
        ioctl(
            self.descriptor.as_raw_fd(),
            DRM_IOCTL_MODE_GETPROPERTY,
            &mut request,
        )?;

        // The kernel writes a NUL-terminated name into a fixed field, and a
        // name that filled it has no terminator, which is why the length is
        // bounded by the field rather than trusted.
        let bytes: Vec<u8> = request
            .name
            .iter()
            .map(|byte| *byte as u8)
            .take_while(|byte| *byte != 0)
            .collect();
        let name = String::from_utf8_lossy(&bytes).into_owned();
        self.property_names.insert(prop_id, name.clone());

        Ok(name)
    }

    fn framebuffer(&self, fb_id: u32) -> io::Result<DrmModeFbCmd2> {
        let mut request = DrmModeFbCmd2 {
            fb_id,
            ..DrmModeFbCmd2::default()
        };
        ioctl(
            self.descriptor.as_raw_fd(),
            DRM_IOCTL_MODE_GETFB2,
            &mut request,
        )?;

        Ok(request)
    }

    /// Exports a framebuffer's first buffer object, unless it already is.
    ///
    /// The handle `GETFB2` created is closed either way: handles live in this
    /// file, and a walk that leaked one per frame would exhaust the device.
    fn export(&mut self, fb_id: u32, handle: u32) -> io::Result<()> {
        if handle == 0 {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!("framebuffer {fb_id} reported no buffer object"),
            ));
        }

        let exported = if self.buffers.contains_key(&fb_id) {
            Ok(None)
        } else {
            self.prime(handle).map(Some)
        };

        let mut close = DrmGemClose { handle, pad: 0 };
        let _ = ioctl(self.descriptor.as_raw_fd(), DRM_IOCTL_GEM_CLOSE, &mut close);

        if let Some(descriptor) = exported? {
            self.buffers.insert(fb_id, descriptor);
        }

        Ok(())
    }

    fn prime(&self, handle: u32) -> io::Result<OwnedFd> {
        let mut request = DrmPrimeHandle {
            handle,
            // Close-on-exec and nothing else. Without `DRM_RDWR` the exported
            // buffer cannot be mapped writable, which is what makes handing it
            // to an unprivileged process a read of the desktop and not control
            // of it.
            flags: libc::O_CLOEXEC as u32,
            fd: -1,
        };
        ioctl(
            self.descriptor.as_raw_fd(),
            DRM_IOCTL_PRIME_HANDLE_TO_FD,
            &mut request,
        )?;

        // SAFETY: the kernel filled in a descriptor this process now owns.
        Ok(unsafe { OwnedFd::from_raw_fd(request.fd) })
    }
}

/// Brackets a CPU read of a dma-buf.
///
/// A descriptor that does not implement the call -- anything that is not a
/// dma-buf -- is not a reason to drop the desktop, so the error is the caller's
/// to report once rather than to fail on.
///
/// # Errors
///
/// [`io::Error`] from the ioctl.
pub fn sync_buffer(descriptor: libc::c_int, flags: u64) -> io::Result<()> {
    let mut request = DmaBufSync { flags };
    ioctl(descriptor, DMA_BUF_IOCTL_SYNC, &mut request)
}

/// One ioctl, retried through signals.
fn ioctl<T>(descriptor: libc::c_int, request: libc::c_ulong, argument: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `argument` is a live, correctly shaped value for `request`,
        // whose size is part of the request number itself, and the descriptor
        // is one this process owns.
        let result = unsafe { libc::ioctl(descriptor, request as _, ptr::from_mut(argument)) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A `CRTC_X` or `CRTC_Y` property value, which the ABI carries as an unsigned
/// word and the kernel means as a signed one.
fn signed(value: u64) -> i32 {
    value as u32 as i32
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{
        card_named,
        uapi::{
            DRM_IOCTL_DROP_MASTER, DRM_IOCTL_GEM_CLOSE, DRM_IOCTL_MODE_GETFB2,
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES, DRM_IOCTL_PRIME_HANDLE_TO_FD,
            DRM_IOCTL_SET_CLIENT_CAP, DRM_IOCTL_WAIT_VBLANK, DmaBufSync, DrmGemClose,
            DrmModeFbCmd2, DrmModeGetPlane, DrmModeObjGetProperties, DrmPrimeHandle,
            DrmSetClientCap, DrmWaitVblank, io_none, io_write, io_write_read,
        },
    };

    fn temporary(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("vmlord-display-drm-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn the_request_arithmetic_reproduces_the_numbers_the_kernel_publishes() {
        // Read out of `drm.h` on a machine with the headers installed. A wrong
        // request number is an ioctl the kernel refuses with EINVAL and no clue
        // why, so these are spelled out rather than trusted.
        assert_eq!(io_write(0x64, 0x09, 8), 0x4008_6409);
        assert_eq!(io_write_read(0x64, 0x2d, 12), 0xc00c_642d);
        assert_eq!(io_none(0x64, 0x1f), 0x0000_641f);

        assert_eq!(DRM_IOCTL_GEM_CLOSE, 0x4008_6409);
        assert_eq!(DRM_IOCTL_SET_CLIENT_CAP, 0x4010_640d);
        assert_eq!(DRM_IOCTL_PRIME_HANDLE_TO_FD, 0xc00c_642d);
        assert_eq!(DRM_IOCTL_WAIT_VBLANK, 0xc018_643a);
        assert_eq!(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, 0xc020_64b9);
        assert_eq!(DRM_IOCTL_MODE_GETFB2, 0xc068_64ce);
        // `_IO('d', 0x1f)`: no size and no direction. Encoded as `_IOW` it
        // would be a request the kernel has never heard of, and the master
        // this process holds would stay held.
        assert_eq!(DRM_IOCTL_DROP_MASTER, 0x0000_641f);
    }

    #[test]
    fn the_structures_are_the_width_the_request_numbers_encode() {
        assert_eq!(size_of::<DrmGemClose>(), 8);
        assert_eq!(size_of::<DrmSetClientCap>(), 16);
        assert_eq!(size_of::<DrmPrimeHandle>(), 12);
        assert_eq!(size_of::<DrmWaitVblank>(), 24);
        assert_eq!(size_of::<DrmModeGetPlane>(), 32);
        assert_eq!(size_of::<DrmModeObjGetProperties>(), 32);
        assert_eq!(size_of::<DrmModeFbCmd2>(), 104);
        assert_eq!(size_of::<DmaBufSync>(), 8);
    }

    #[test]
    fn the_card_is_found_by_driver_name_and_not_by_number() {
        // A guest that also has hyperv_drm has a card0 that is not ours, which
        // is the whole reason this walks sysfs instead of opening a path.
        let root = temporary("cards");
        let sysfs = root.join("class/drm");
        for (card, driver) in [("card0", "hyperv_drm"), ("card1", "vmlord_drm")] {
            let device = sysfs.join(card).join("device");
            fs::create_dir_all(&device).unwrap();
            std::os::unix::fs::symlink(
                root.join("bus/platform/drivers").join(driver),
                device.join("driver"),
            )
            .unwrap();
        }

        assert_eq!(
            card_named("vmlord_drm", &sysfs).unwrap(),
            Some("card1".to_owned())
        );
        assert_eq!(card_named("nouveau", &sysfs).unwrap(), None);
    }

    #[test]
    fn a_sysfs_with_no_drm_class_at_all_is_no_card_and_not_an_error() {
        // The state of this development machine, and of a guest whose module
        // has not loaded yet. The broker waits for a card; it must not fail.
        assert_eq!(
            card_named("vmlord_drm", &temporary("empty").join("absent")).unwrap(),
            None
        );
    }

    #[test]
    fn only_the_two_linear_formats_this_build_can_map_are_accepted() {
        use super::uapi::{DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888};

        assert!(super::format_is_mappable(
            DRM_FORMAT_XRGB8888,
            DRM_FORMAT_MOD_LINEAR
        ));
        assert!(super::format_is_mappable(
            DRM_FORMAT_ARGB8888,
            DRM_FORMAT_MOD_LINEAR
        ));
        assert!(
            !super::format_is_mappable(DRM_FORMAT_XRGB8888, 1),
            "a capture that mmaps a buffer cannot detile anything but linear"
        );
        assert!(
            !super::format_is_mappable(0x3231_564e, DRM_FORMAT_MOD_LINEAR),
            "NV12 is not something this build composites"
        );
    }
}
