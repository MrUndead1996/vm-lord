//! The guest's keyboard and pointer, as `/dev/uinput` devices.
//!
//! Two halves, split by who may call them. [`create`] needs root and belongs
//! to the broker; [`Keyboard`] and [`Pointer`] write into a descriptor that
//! has already been created, and belong to the session process the broker
//! hands it to.
//!
//! Two devices rather than one: libinput classifies a device by its capability
//! bits, and a node carrying keys, absolute axes and buttons at once is
//! resolved by heuristics that have changed between releases. Ubuntu 22.04,
//! 24.04 and 26.04 must behave the same, so each node is unambiguous.
//!
//! Nothing here is `unsafe` below [`create`]: an `input_event` is written out
//! byte by byte, which also makes the event stream something a test can read.

use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::OpenOptions,
    io::{self, Write},
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
};

use crate::drm::uapi::io_write;

/// `EV_SYN`.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY`.
pub const EV_KEY: u16 = 0x01;
/// `EV_REL`.
pub const EV_REL: u16 = 0x02;
/// `EV_ABS`.
pub const EV_ABS: u16 = 0x03;

/// `ABS_X`.
const ABS_X: u16 = 0x00;
/// `ABS_Y`.
const ABS_Y: u16 = 0x01;
/// `REL_HWHEEL`.
const REL_HWHEEL: u16 = 0x06;
/// `REL_WHEEL`.
const REL_WHEEL: u16 = 0x08;
/// `REL_WHEEL_HI_RES`.
const REL_WHEEL_HI_RES: u16 = 0x0b;
/// `REL_HWHEEL_HI_RES`.
const REL_HWHEEL_HI_RES: u16 = 0x0c;
/// `SYN_REPORT`.
const SYN_REPORT: u16 = 0x00;

/// `BTN_LEFT`.
pub const BTN_LEFT: u16 = 0x110;
/// `BTN_EXTRA`, the highest button this build sends.
pub const BTN_EXTRA: u16 = 0x114;

/// The absolute axes' maximum, fixed for the life of the device.
///
/// Never derived from the resolution: #120 changes that at runtime, and a
/// device recreated to follow it would disconnect the guest's pointer.
pub const ABS_RANGE: i32 = 32767;

/// How many steps that range is, which is one more than its maximum.
///
/// The unit libinput divides a screen into: an axis reading `value` on a
/// screen `size` wide is at `value * size / ABS_STEPS` pixels across it.
const ABS_STEPS: u64 = ABS_RANGE as u64 + 1;

/// One wheel detent, in the hundred-and-twentieths both the wire and the
/// kernel's high-resolution axes count in.
pub const DETENT: i32 = 120;

/// The highest keycode this build sends, and the keyboard's declared ceiling.
pub const KEY_MAX_SENT: u16 = 127;

/// How many bytes one `input_event` is on this target.
const EVENT_SIZE: usize = 24;

/// One event, written out rather than transmuted.
///
/// The timestamp stays zero: the kernel fills it in, and reading a clock per
/// mouse movement to have it overwritten is work for nothing.
fn encode(kind: u16, code: u16, value: i32) -> [u8; EVENT_SIZE] {
    let mut bytes = [0u8; EVENT_SIZE];
    bytes[16..18].copy_from_slice(&kind.to_ne_bytes());
    bytes[18..20].copy_from_slice(&code.to_ne_bytes());
    bytes[20..24].copy_from_slice(&value.to_ne_bytes());

    bytes
}

/// Writes one group and the report that closes it.
///
/// One write, not one per event: a group the kernel sees whole is a group
/// libinput sees as one motion rather than as an axis at a time.
fn emit<W: Write>(device: &mut W, group: &[[u8; EVENT_SIZE]]) -> io::Result<()> {
    if group.is_empty() {
        return Ok(());
    }

    let mut bytes = Vec::with_capacity((group.len() + 1) * EVENT_SIZE);
    for event in group {
        bytes.extend_from_slice(event);
    }
    bytes.extend_from_slice(&encode(EV_SYN, SYN_REPORT, 0));

    device.write_all(&bytes)
}

/// The guest's keyboard.
pub struct Keyboard<W: Write> {
    device: W,
    held: BTreeSet<u16>,
}

impl<W: Write> Keyboard<W> {
    /// A keyboard over a descriptor [`create`] has already made a device of.
    pub const fn new(device: W) -> Self {
        Self {
            device,
            held: BTreeSet::new(),
        }
    }

    /// Presses or releases one key.
    ///
    /// A keycode this build never sends is dropped: the device declared a
    /// ceiling, and writing past it would be a key nobody can have pressed.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn key(&mut self, keycode: u16, pressed: bool) -> io::Result<()> {
        if keycode == 0 || keycode > KEY_MAX_SENT {
            return Ok(());
        }

        if pressed {
            self.held.insert(keycode);
        } else {
            self.held.remove(&keycode);
        }

        emit(
            &mut self.device,
            &[encode(EV_KEY, keycode, i32::from(pressed))],
        )
    }

    /// Releases every key this device believes is down.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn release_all(&mut self) -> io::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }

        let group: Vec<_> = std::mem::take(&mut self.held)
            .into_iter()
            .map(|keycode| encode(EV_KEY, keycode, 0))
            .collect();

        emit(&mut self.device, &group)
    }

    /// What has been written, for the tests that read it back.
    #[cfg(test)]
    fn device(&self) -> &W {
        &self.device
    }
}

/// The guest's absolute pointer.
pub struct Pointer<W: Write> {
    device: W,
    held: BTreeSet<u16>,
    /// The fraction of a detent not yet worth a whole one, per axis.
    horizontal: i32,
    vertical: i32,
}

impl<W: Write> Pointer<W> {
    /// A pointer over a descriptor [`create`] has already made a device of.
    pub const fn new(device: W) -> Self {
        Self {
            device,
            held: BTreeSet::new(),
            horizontal: 0,
            vertical: 0,
        }
    }

    /// Moves the pointer to one guest pixel of a screen this size.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn motion(&mut self, x: u32, y: u32, width: u32, height: u32) -> io::Result<()> {
        let group = [
            encode(EV_ABS, ABS_X, scale(x, width)),
            encode(EV_ABS, ABS_Y, scale(y, height)),
        ];

        emit(&mut self.device, &group)
    }

    /// Presses or releases one button.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn button(&mut self, button: u16, pressed: bool) -> io::Result<()> {
        if !(BTN_LEFT..=BTN_EXTRA).contains(&button) {
            return Ok(());
        }

        if pressed {
            self.held.insert(button);
        } else {
            self.held.remove(&button);
        }

        emit(
            &mut self.device,
            &[encode(EV_KEY, button, i32::from(pressed))],
        )
    }

    /// Turns the wheel by hundred-and-twentieths of a detent.
    ///
    /// Both resolutions travel: the high-resolution axis in the unit it
    /// arrived in, and the whole detents it adds up to, with the remainder
    /// carried so that slow scrolling is not lost on an application that reads
    /// only the discrete axis.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn scroll(&mut self, horizontal: i32, vertical: i32) -> io::Result<()> {
        let mut group = Vec::new();
        if horizontal != 0 {
            group.push(encode(EV_REL, REL_HWHEEL_HI_RES, horizontal));
            self.horizontal = self.horizontal.saturating_add(horizontal);
            let detents = self.horizontal / DETENT;
            if detents != 0 {
                self.horizontal -= detents * DETENT;
                group.push(encode(EV_REL, REL_HWHEEL, detents));
            }
        }
        if vertical != 0 {
            group.push(encode(EV_REL, REL_WHEEL_HI_RES, vertical));
            self.vertical = self.vertical.saturating_add(vertical);
            let detents = self.vertical / DETENT;
            if detents != 0 {
                self.vertical -= detents * DETENT;
                group.push(encode(EV_REL, REL_WHEEL, detents));
            }
        }

        emit(&mut self.device, &group)
    }

    /// Releases every button this device believes is down.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn release_all(&mut self) -> io::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }

        let group: Vec<_> = std::mem::take(&mut self.held)
            .into_iter()
            .map(|button| encode(EV_KEY, button, 0))
            .collect();

        emit(&mut self.device, &group)
    }

    /// What has been written, for the tests that read it back.
    #[cfg(test)]
    fn device(&self) -> &W {
        &self.device
    }
}

/// One guest pixel onto the fixed absolute range, inside the pixel.
///
/// Inside it rather than on its edge, because the value makes a round trip and
/// has to come back as the pixel it left as. libinput reads an absolute axis
/// as `value * size / (maximum + 1)`, so the range is [`ABS_STEPS`] steps
/// across the screen and pixel `p` owns the steps in `[p * steps / size,
/// (p + 1) * steps / size)`. An edge-anchored value sits on the boundary of
/// that interval and the arithmetic on the way back drops it into the pixel
/// next door: at 1920 wide, pixel 1 used to arrive as `17`, which reads back
/// as `0.996` -- pixel 0. Aiming well inside the interval leaves a third of a
/// pixel of slack either way, and every pixel of every mode this build offers
/// comes back as itself.
///
/// [`OFFSET`] is a little short of the middle rather than the middle itself,
/// because the pointer's position is also what the compositor places the
/// cursor plane by, and the viewer measures the cursor's hotspot from that
/// plane -- see the viewer's `cursor.rs`. A pointer sitting on exactly half a
/// pixel is a plane position a compositor may round either way, and the
/// measurement would come back a pixel short as often as not.
fn scale(value: u32, size: u32) -> i32 {
    let size = u64::from(size.max(1));
    let value = u64::from(value).min(size - 1);
    // (value + OFFSET) * steps / size, rounded, in whole numbers throughout.
    let numerator = (value * OFFSET.1 + OFFSET.0) * ABS_STEPS;
    let denominator = size * OFFSET.1;
    let scaled = (numerator + denominator / 2) / denominator;

    i32::try_from(scaled).unwrap_or(ABS_RANGE).min(ABS_RANGE)
}

/// Where in a pixel the pointer is put, as a fraction of one.
///
/// Three eighths: far enough from either edge that no rounding on the way back
/// leaves the pixel, and short enough of a half that the position rounds down
/// whichever rule does the rounding.
const OFFSET: (u64, u64) = (3, 8);

/// Where the kernel's uinput device is.
pub const DEVICE_PATH: &str = "/dev/uinput";

/// The `'U'` every uinput request is built on.
const UINPUT: u32 = 0x55;

/// `_IO(UINPUT_IOCTL_BASE, 1)`.
const UI_DEV_CREATE: libc::c_ulong = ((UINPUT << 8) | 1) as libc::c_ulong;
/// `_IOW(UINPUT_IOCTL_BASE, 3, struct uinput_setup)`.
const UI_DEV_SETUP: libc::c_ulong = io_write(UINPUT, 3, size_of::<UinputSetup>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 4, struct uinput_abs_setup)`.
const UI_ABS_SETUP: libc::c_ulong = io_write(UINPUT, 4, size_of::<UinputAbsSetup>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 100, int)`.
const UI_SET_EVBIT: libc::c_ulong = io_write(UINPUT, 100, size_of::<libc::c_int>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 101, int)`.
const UI_SET_KEYBIT: libc::c_ulong = io_write(UINPUT, 101, size_of::<libc::c_int>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 102, int)`.
const UI_SET_RELBIT: libc::c_ulong = io_write(UINPUT, 102, size_of::<libc::c_int>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 103, int)`.
const UI_SET_ABSBIT: libc::c_ulong = io_write(UINPUT, 103, size_of::<libc::c_int>() as u32);

/// `BUS_VIRTUAL`, which is what a device with no bus behind it reports.
const BUS_VIRTUAL: u16 = 0x06;

/// The vendor both devices are known by.
const VENDOR: u16 = 0x564d;
/// The keyboard's product id.
const KEYBOARD_PRODUCT: u16 = 0x0001;
/// The pointer's.
const POINTER_PRODUCT: u16 = 0x0002;

/// `struct input_id`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct uinput_setup`.
#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id: InputId,
    name: [libc::c_char; 80],
    ff_effects_max: u32,
}

/// `struct input_absinfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `struct uinput_abs_setup`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

/// Creates the guest's keyboard and pointer, in that order.
///
/// The broker's, because `/dev/uinput` is root's. The descriptors are what it
/// hands the session process: while one is held the device exists, and when
/// the last copy closes the kernel unregisters it and releases every key it
/// believed was down. That is what makes a killed session leave no key stuck.
///
/// # Errors
///
/// Whatever the device refused: an absent `/dev/uinput`, a denied open, or a
/// rejected `UI_DEV_CREATE`. The broker logs it and carries on without input.
pub fn create(path: &Path) -> io::Result<(OwnedFd, OwnedFd)> {
    let keyboard = create_keyboard(path)?;
    let pointer = create_pointer(path)?;

    Ok((keyboard, pointer))
}

/// One node with keys and nothing else, so that libinput has no choice to make.
fn create_keyboard(path: &Path) -> io::Result<OwnedFd> {
    let device = OpenOptions::new().write(true).open(path)?;
    let fd = device.as_raw_fd();

    set_bit(fd, UI_SET_EVBIT, u32::from(EV_KEY))?;
    // Every code the viewer's table can produce, and no more: a device that
    // declares a key it never sends is a device libinput reasons wrongly about.
    for keycode in 1..=u32::from(KEY_MAX_SENT) {
        set_bit(fd, UI_SET_KEYBIT, keycode)?;
    }

    setup(fd, "VMLord Keyboard", KEYBOARD_PRODUCT)?;
    create_device(fd)?;

    Ok(device.into())
}

/// One node with absolute axes, buttons and wheels, and no keyboard in it.
fn create_pointer(path: &Path) -> io::Result<OwnedFd> {
    let device = OpenOptions::new().write(true).open(path)?;
    let fd = device.as_raw_fd();

    set_bit(fd, UI_SET_EVBIT, u32::from(EV_KEY))?;
    for button in u32::from(BTN_LEFT)..=u32::from(BTN_EXTRA) {
        set_bit(fd, UI_SET_KEYBIT, button)?;
    }
    set_bit(fd, UI_SET_EVBIT, u32::from(EV_ABS))?;
    set_bit(fd, UI_SET_ABSBIT, u32::from(ABS_X))?;
    set_bit(fd, UI_SET_ABSBIT, u32::from(ABS_Y))?;
    set_bit(fd, UI_SET_EVBIT, u32::from(EV_REL))?;
    for axis in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
        set_bit(fd, UI_SET_RELBIT, u32::from(axis))?;
    }

    for code in [ABS_X, ABS_Y] {
        let absolute = UinputAbsSetup {
            code,
            absinfo: InputAbsinfo {
                maximum: ABS_RANGE,
                ..InputAbsinfo::default()
            },
        };
        // SAFETY: `absolute` lives across the call, and the request names its
        // own size, which is what the kernel copies.
        let result = unsafe { libc::ioctl(fd, UI_ABS_SETUP as _, &raw const absolute) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    setup(fd, "VMLord Pointer", POINTER_PRODUCT)?;
    create_device(fd)?;

    Ok(device.into())
}

/// One `UI_SET_*BIT`.
fn set_bit(fd: libc::c_int, request: libc::c_ulong, bit: u32) -> io::Result<()> {
    // SAFETY: an ioctl on a descriptor this function's caller owns, with the
    // integer argument every `UI_SET_*BIT` request takes.
    let result = unsafe { libc::ioctl(fd, request as _, libc::c_int::try_from(bit).unwrap_or(0)) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Names the device, which is what a user sees in `libinput list-devices`.
fn setup(fd: libc::c_int, name: &str, product: u16) -> io::Result<()> {
    let mut setup = UinputSetup {
        id: InputId {
            bustype: BUS_VIRTUAL,
            vendor: VENDOR,
            product,
            version: 1,
        },
        name: [0; 80],
        ff_effects_max: 0,
    };
    let text =
        CString::new(name).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    for (slot, byte) in setup.name.iter_mut().zip(text.as_bytes()) {
        *slot = *byte as libc::c_char;
    }

    // SAFETY: `setup` lives across the call and is the structure the request
    // names the size of.
    let result = unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &raw const setup) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// `UI_DEV_CREATE`, after which the node exists.
fn create_device(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: an ioctl with no argument on a descriptor the caller owns.
    let result = unsafe { libc::ioctl(fd, UI_DEV_CREATE as _) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ABS_RANGE, BTN_LEFT, EV_KEY, EV_REL, EV_SYN, Keyboard, Pointer, scale};

    /// The `(type, code, value)` triples one write put on the wire.
    fn events(bytes: &[u8]) -> Vec<(u16, u16, i32)> {
        bytes
            .chunks_exact(24)
            .map(|event| {
                (
                    u16::from_ne_bytes([event[16], event[17]]),
                    u16::from_ne_bytes([event[18], event[19]]),
                    i32::from_ne_bytes([event[20], event[21], event[22], event[23]]),
                )
            })
            .collect()
    }

    #[test]
    fn a_key_is_one_event_and_the_report_that_closes_it() {
        let mut keyboard = Keyboard::new(Vec::new());
        keyboard.key(30, true).expect("a key goes down");

        assert_eq!(
            events(keyboard.device()),
            vec![(EV_KEY, 30, 1), (EV_SYN, 0, 0)]
        );
    }

    #[test]
    fn a_key_this_build_never_sends_is_dropped_rather_than_written() {
        let mut keyboard = Keyboard::new(Vec::new());
        keyboard.key(400, true).expect("a keycode with no key");

        assert!(events(keyboard.device()).is_empty());
    }

    #[test]
    fn a_release_all_releases_what_is_held_and_nothing_else() {
        let mut keyboard = Keyboard::new(Vec::new());
        keyboard.key(30, true).expect("a");
        keyboard.key(42, true).expect("shift");
        keyboard.key(30, false).expect("a again");
        let held = keyboard.device().len();

        keyboard.release_all().expect("a release");

        assert_eq!(
            events(&keyboard.device()[held..]),
            vec![(EV_KEY, 42, 0), (EV_SYN, 0, 0)]
        );

        // And a second release owes nothing.
        let released = keyboard.device().len();
        keyboard.release_all().expect("a second release");
        assert_eq!(keyboard.device().len(), released);
    }

    /// Where libinput reads an axis of this value as being, in pixels.
    ///
    /// `value * size / (maximum + 1)`, which is what a compositor is handed.
    fn read_back(value: i32, size: u32) -> u32 {
        u32::try_from(i64::from(value) * i64::from(size) / (ABS_RANGE as i64 + 1))
            .expect("a pixel")
    }

    #[test]
    fn motion_is_scaled_onto_the_fixed_absolute_range() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.motion(0, 0, 1920, 1080).expect("the top left");
        pointer
            .motion(1919, 1079, 1920, 1080)
            .expect("the bottom right");
        pointer.motion(960, 540, 1920, 1080).expect("the middle");

        let events = events(pointer.device());
        // Inside the range at both ends, because a pixel's centre is half a
        // pixel in from the screen's edge.
        assert!((0..ABS_RANGE).contains(&events[0].2), "{events:?}");
        assert!((0..ABS_RANGE).contains(&events[3].2), "{events:?}");
        assert_eq!(read_back(events[0].2, 1920), 0);
        assert_eq!(read_back(events[1].2, 1080), 0);
        assert_eq!(read_back(events[3].2, 1920), 1919);
        assert_eq!(read_back(events[4].2, 1080), 1079);
        assert_eq!(read_back(events[6].2, 1920), 960);
        assert_eq!(read_back(events[7].2, 1080), 540);
    }

    #[test]
    fn every_pixel_of_every_mode_comes_back_as_itself() {
        // The whole point of the scale: what the host names, the guest's
        // pointer stands on. A pixel out is a click on the button next door.
        for size in [640, 800, 1024, 1280, 1366, 1600, 1920, 2560] {
            for pixel in 0..size {
                assert_eq!(
                    read_back(scale(pixel, size), size),
                    pixel,
                    "pixel {pixel} of {size}"
                );
            }
        }
    }

    #[test]
    fn the_pointer_sits_short_of_the_middle_of_its_pixel() {
        // What the cursor plane is placed by, and so what the viewer measures
        // a hotspot from: a position on exactly half a pixel is one a
        // compositor may round either way.
        for size in [640, 1280, 1920, 2560] {
            for pixel in [0, 1, size / 2, size - 2, size - 1] {
                let thousandths = i64::from(scale(pixel, size)) * i64::from(size) * 1000
                    / (ABS_RANGE as i64 + 1)
                    % 1000;
                assert!(
                    (250..500).contains(&thousandths),
                    "pixel {pixel} of {size} sits at .{thousandths}"
                );
            }
        }
    }

    #[test]
    fn motion_past_the_edge_is_pulled_back_onto_the_screen() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.motion(4000, 4000, 1920, 1080).expect("motion");

        let events = events(pointer.device());
        assert_eq!(read_back(events[0].2, 1920), 1919);
        assert_eq!(read_back(events[1].2, 1080), 1079);
    }

    #[test]
    fn a_screen_with_no_pixels_is_not_divided_by_it() {
        assert_eq!(scale(0, 0), scale(0, 1));
    }

    #[test]
    fn a_whole_detent_travels_on_both_axes_of_the_wheel() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.scroll(0, 120).expect("one detent up");

        assert_eq!(
            events(pointer.device()),
            vec![(EV_REL, 0x0b, 120), (EV_REL, 8, 1), (EV_SYN, 0, 0)]
        );
    }

    #[test]
    fn a_slow_wheel_carries_its_remainder_until_it_makes_a_detent() {
        let mut pointer = Pointer::new(Vec::new());
        for _ in 0..3 {
            pointer.scroll(0, 40).expect("a third of a detent");
        }

        let events = events(pointer.device());
        let detents: Vec<_> = events.iter().filter(|event| event.1 == 8).collect();
        assert_eq!(
            detents,
            vec![&(EV_REL, 8, 1)],
            "three thirds are one detent"
        );
        assert_eq!(
            events.iter().filter(|event| event.1 == 0x0b).count(),
            3,
            "and every one of them travelled at full resolution"
        );
    }

    #[test]
    fn a_wheel_turned_the_other_way_carries_its_remainder_the_same() {
        let mut pointer = Pointer::new(Vec::new());
        for _ in 0..3 {
            pointer.scroll(0, -40).expect("a third of a detent");
        }

        let events = events(pointer.device());
        let detents: Vec<_> = events.iter().filter(|event| event.1 == 8).collect();
        assert_eq!(detents, vec![&(EV_REL, 8, -1)]);
    }

    #[test]
    fn a_button_this_build_never_sends_is_dropped() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.button(0x999, true).expect("a button with no name");

        assert!(events(pointer.device()).is_empty());
    }

    #[test]
    fn a_pointer_release_all_lifts_the_buttons_that_are_down() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.button(BTN_LEFT, true).expect("a press");
        let held = pointer.device().len();

        pointer.release_all().expect("a release");

        assert_eq!(
            events(&pointer.device()[held..]),
            vec![(EV_KEY, BTN_LEFT, 0), (EV_SYN, 0, 0)]
        );
    }

    #[test]
    fn the_request_numbers_match_the_kernels_own_encoding() {
        // Written out from `linux/uinput.h`. A structure that grew or shrank
        // here would send a request the kernel does not answer, and the only
        // symptom would be a device that never appears.
        assert_eq!(super::UI_DEV_CREATE, 0x5501);
        assert_eq!(super::UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(super::UI_ABS_SETUP, 0x401c_5504);
        assert_eq!(super::UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(super::UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(super::UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(super::UI_SET_ABSBIT, 0x4004_5567);
    }

    #[test]
    fn a_device_that_cannot_be_opened_is_an_error_rather_than_a_panic() {
        let missing = std::path::Path::new("/nonexistent/uinput");

        assert!(super::create(missing).is_err());
    }
}
