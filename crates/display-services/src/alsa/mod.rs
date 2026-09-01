//! The guest's ALSA capture, and the kernel ABI it is written against.
//!
//! What the desktop plays reaches this crate through `snd-aloop`: the
//! compositor's audio goes into the loopback's playback half, and the kernel
//! hands it back on the capture half, which is what [`Capture`] reads. No
//! PipeWire in the hot path, and no C library anywhere.

pub mod uapi;

use std::{error::Error, ffi::CString, fmt, io, os::fd::RawFd};

use vmlord_display_protocol::audio::{Format, SampleFormat};

use uapi::{
    SNDRV_PCM_IOCTL_HW_PARAMS, SNDRV_PCM_IOCTL_HW_REFINE, SNDRV_PCM_IOCTL_PREPARE,
    SNDRV_PCM_IOCTL_PVERSION, SNDRV_PCM_IOCTL_READI_FRAMES, SNDRV_PCM_IOCTL_START,
    SNDRV_PCM_IOCTL_SW_PARAMS, SndPcmHwParams, SndPcmSwParams, SndXferi,
};

/// The capture half of the first loopback cable.
///
/// `snd-aloop` pairs playback subdevice N on device 0 with capture subdevice N
/// on device 1, and the WirePlumber rule this payload ships routes the desktop
/// into the matching playback half.
pub const DEFAULT_DEVICE: &str = "/dev/snd/pcmC0D1c";

/// What the daemon asks for when nothing has pinned the loopback yet.
///
/// 480 frames is 10 ms, which the spike measured as the whole of the capture
/// latency: a read waits one period and nothing more.
pub const WANTED: Format = Format {
    sample_rate: 48_000,
    channels: 2,
    sample_format: SampleFormat::S16Le,
    frames_per_period: 480,
};

/// How many periods the ring between the kernel and this process holds.
pub const PERIODS: u32 = 4;

/// Why a capture could not be opened, or could not go on.
#[derive(Debug)]
pub enum CaptureError {
    /// The device node would not open. A guest without `snd-aloop` lands here.
    Open(io::Error),
    /// The kernel would not narrow the parameters.
    Refine(io::Error),
    /// It would not accept the narrowed ones.
    Params(io::Error),
    /// The configured stream would not start.
    Start(io::Error),
    /// A transfer failed for something other than an xrun.
    Read(io::Error),
    /// The loopback is pinned to something no record type can carry.
    Unsupported(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "the loopback would not open: {error}"),
            Self::Refine(error) => {
                write!(formatter, "the loopback refused every format: {error}")
            }
            Self::Params(error) => {
                write!(
                    formatter,
                    "the loopback refused its own parameters: {error}"
                )
            }
            Self::Start(error) => write!(formatter, "the capture would not start: {error}"),
            Self::Read(error) => write!(formatter, "the capture failed: {error}"),
            Self::Unsupported(what) => {
                write!(
                    formatter,
                    "the loopback is pinned to {what}, which this build cannot carry"
                )
            }
        }
    }
}

impl Error for CaptureError {}

/// Where the ring's arithmetic wraps.
///
/// The kernel wants a multiple of the buffer size that a stream will not reach
/// in any session, so that `stop_threshold` can be set to it and mean "never
/// stop": the loopback clocks silence when nothing plays, and a capture that
/// stopped itself on a quiet desktop would need restarting for no reason.
#[must_use]
pub fn boundary(buffer_frames: libc::c_ulong) -> libc::c_ulong {
    let mut boundary = buffer_frames.max(1);

    while boundary * 2 < libc::c_ulong::MAX / 2 {
        boundary *= 2;
    }

    boundary
}

/// Which sample format a refined mask left, if this build can carry it.
///
/// The preference order is the daemon's own: `S16_LE` is what it pins when it
/// opens first, and the wider formats are what it accepts when PipeWire got
/// there first. A mask holding none of the three is not a format to guess at
/// -- sending bytes the host would misread is worse than reporting nothing.
#[must_use]
pub fn format_from_mask(bits: &[u32; 8]) -> Option<SampleFormat> {
    let holds = |value: u32| bits[(value / 32) as usize] & (1 << (value % 32)) != 0;

    if holds(uapi::FORMAT_S16_LE) {
        Some(SampleFormat::S16Le)
    } else if holds(uapi::FORMAT_S32_LE) {
        Some(SampleFormat::S32Le)
    } else if holds(uapi::FORMAT_FLOAT_LE) {
        Some(SampleFormat::FloatLe)
    } else {
        None
    }
}

/// One `ioctl`, with `-1` turned into the error it stands for.
fn call(fd: RawFd, request: libc::c_ulong, argument: *mut libc::c_void) -> io::Result<()> {
    // SAFETY: `request` names a structure of the size encoded in it, and
    // `argument` points at one of exactly that type -- the pairing is what
    // every constant in `uapi` exists to keep.
    let result = unsafe { libc::ioctl(fd, request as _, argument) };

    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// The loopback's capture half, configured and running.
pub struct Capture {
    fd: RawFd,
    format: Format,
}

impl Capture {
    /// Opens `path` and starts capturing.
    ///
    /// `wanted` is a preference, not a demand. Whichever half of the loopback
    /// opens first fixes the other, so a daemon that starts at boot gets what
    /// it asked for and one that restarts into a playing desktop is handed
    /// what PipeWire pinned. Either way [`Capture::format`] is what is really
    /// running, and it is what the host is told.
    ///
    /// # Errors
    ///
    /// [`CaptureError`] naming the step that failed: a guest without
    /// `snd-aloop` fails at [`CaptureError::Open`], and a loopback pinned to a
    /// format no record can carry at [`CaptureError::Unsupported`].
    pub fn open(path: &str, wanted: Format) -> Result<Self, CaptureError> {
        let device = CString::new(path).map_err(|_| {
            CaptureError::Unsupported(format!("a device path with a NUL in it ({path})"))
        })?;
        // SAFETY: `device` is a NUL-terminated path that outlives the call.
        let fd = unsafe { libc::open(device.as_ptr(), libc::O_RDONLY) };

        if fd < 0 {
            return Err(CaptureError::Open(io::Error::last_os_error()));
        }

        match Self::configure(fd, wanted) {
            Ok(format) => Ok(Self { fd, format }),
            Err(error) => {
                // SAFETY: `fd` is ours and nothing else holds it.
                unsafe { libc::close(fd) };
                Err(error)
            }
        }
    }

    /// What the stream is really running at.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// Reads one period into `into`, and says how many frames arrived.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Read`] for anything but an xrun. An xrun is not an
    /// error: the loopback lost some frames, the stream is re-prepared, and
    /// zero frames come back so that the caller's stream position records the
    /// gap rather than pretending there was none.
    pub fn read(&mut self, into: &mut [u8]) -> Result<u32, CaptureError> {
        let frames = into.len() / self.format.bytes_per_frame();
        let mut transfer = SndXferi {
            result: 0,
            buf: into.as_mut_ptr().cast(),
            frames: frames as libc::c_ulong,
        };

        match call(
            self.fd,
            SNDRV_PCM_IOCTL_READI_FRAMES,
            std::ptr::from_mut(&mut transfer).cast(),
        ) {
            Ok(()) => Ok(u32::try_from(transfer.result.max(0)).unwrap_or(0)),
            Err(error) if error.raw_os_error() == Some(libc::EPIPE) => {
                self.recover()?;
                Ok(0)
            }
            Err(error) => Err(CaptureError::Read(error)),
        }
    }

    /// Re-prepares and restarts a stream that has xrun.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Start`] when the kernel will not restart it, which is a
    /// capture that has to be opened again.
    pub fn recover(&mut self) -> Result<(), CaptureError> {
        call(self.fd, SNDRV_PCM_IOCTL_PREPARE, std::ptr::null_mut())
            .and_then(|()| call(self.fd, SNDRV_PCM_IOCTL_START, std::ptr::null_mut()))
            .map_err(CaptureError::Start)
    }

    /// Narrows the parameters, configures the stream and starts it.
    fn configure(fd: RawFd, wanted: Format) -> Result<Format, CaptureError> {
        let mut version: i32 = 0;
        call(
            fd,
            SNDRV_PCM_IOCTL_PVERSION,
            std::ptr::from_mut(&mut version).cast(),
        )
        .map_err(CaptureError::Refine)?;

        let format = Self::refine(fd, wanted)?;
        let mut params = Self::wishes(format);
        call(
            fd,
            SNDRV_PCM_IOCTL_HW_PARAMS,
            std::ptr::from_mut(&mut params).cast(),
        )
        .map_err(CaptureError::Params)?;

        let buffer = libc::c_ulong::from(uapi::interval(&params, uapi::BUFFER_SIZE).1);
        let mut software: SndPcmSwParams = unsafe { std::mem::zeroed() };
        software.period_step = 1;
        software.avail_min = libc::c_ulong::from(format.frames_per_period);
        software.start_threshold = 1;
        software.stop_threshold = boundary(buffer);
        software.boundary = boundary(buffer);
        software.proto = version as u32;
        call(
            fd,
            SNDRV_PCM_IOCTL_SW_PARAMS,
            std::ptr::from_mut(&mut software).cast(),
        )
        .map_err(CaptureError::Params)?;

        call(fd, SNDRV_PCM_IOCTL_PREPARE, std::ptr::null_mut()).map_err(CaptureError::Start)?;
        call(fd, SNDRV_PCM_IOCTL_START, std::ptr::null_mut()).map_err(CaptureError::Start)?;

        Ok(format)
    }

    /// Asks the kernel what it will accept, preferring `wanted`.
    fn refine(fd: RawFd, wanted: Format) -> Result<Format, CaptureError> {
        let mut probe = Self::wishes(wanted);

        if call(
            fd,
            SNDRV_PCM_IOCTL_HW_REFINE,
            std::ptr::from_mut(&mut probe).cast(),
        )
        .is_ok()
        {
            return Ok(wanted);
        }

        // The playback half is already open and has pinned the other one. Ask
        // again for anything at all, and take what comes back.
        let mut open = uapi::params_any();
        uapi::set_mask(&mut open, uapi::ACCESS, uapi::ACCESS_RW_INTERLEAVED);
        call(
            fd,
            SNDRV_PCM_IOCTL_HW_REFINE,
            std::ptr::from_mut(&mut open).cast(),
        )
        .map_err(CaptureError::Refine)?;

        let sample_format = format_from_mask(&open.masks[uapi::FORMAT].bits).ok_or_else(|| {
            CaptureError::Unsupported("a sample format no record names".to_owned())
        })?;
        let (channels, _) = uapi::interval(&open, uapi::CHANNELS);
        let (rate, _) = uapi::interval(&open, uapi::RATE);
        let pinned = Format {
            sample_rate: rate,
            channels,
            sample_format,
            // A tenth of the pinned rate's hundredth: ten milliseconds, in
            // whatever rate the other half chose.
            frames_per_period: (rate / 100).max(1),
        };

        let mut second = Self::wishes(pinned);
        call(
            fd,
            SNDRV_PCM_IOCTL_HW_REFINE,
            std::ptr::from_mut(&mut second).cast(),
        )
        .map_err(CaptureError::Refine)?;

        Ok(pinned)
    }

    /// The parameter set that asks for exactly `format`.
    fn wishes(format: Format) -> SndPcmHwParams {
        let mut params = uapi::params_any();
        uapi::set_mask(&mut params, uapi::ACCESS, uapi::ACCESS_RW_INTERLEAVED);
        uapi::set_mask(
            &mut params,
            uapi::FORMAT,
            match format.sample_format {
                SampleFormat::S16Le => uapi::FORMAT_S16_LE,
                SampleFormat::S32Le => uapi::FORMAT_S32_LE,
                SampleFormat::FloatLe => uapi::FORMAT_FLOAT_LE,
            },
        );
        uapi::set_exact(&mut params, uapi::CHANNELS, format.channels);
        uapi::set_exact(&mut params, uapi::RATE, format.sample_rate);
        uapi::set_exact(&mut params, uapi::PERIOD_SIZE, format.frames_per_period);
        uapi::set_exact(&mut params, uapi::PERIODS, PERIODS);
        params
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: `fd` is ours, opened in `open` and closed exactly here.
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_a_power_of_two_multiple_of_the_buffer() {
        let boundary = boundary(1920);

        assert_eq!(boundary % 1920, 0);
        assert!(boundary > 1920);
        assert!(boundary <= libc::c_ulong::MAX / 2);
        assert!((boundary / 1920).is_power_of_two());
    }

    #[test]
    fn a_refined_format_mask_names_the_format_that_was_pinned() {
        let mut bits = [0u32; 8];
        bits[0] = 1 << uapi::FORMAT_S32_LE;

        assert_eq!(format_from_mask(&bits), Some(SampleFormat::S32Le));
    }

    #[test]
    fn a_format_this_build_cannot_carry_is_refused_rather_than_guessed() {
        let mut bits = [0u32; 8];
        bits[0] = 1 << 6; // S24_LE, which no record type names.

        assert_eq!(format_from_mask(&bits), None);
    }

    #[test]
    fn the_wanted_format_is_preferred_when_the_device_still_offers_it() {
        let mut bits = [0u32; 8];
        bits[0] = (1 << uapi::FORMAT_S16_LE) | (1 << uapi::FORMAT_S32_LE);

        // Nothing has pinned the loopback yet, so both are on offer and the
        // daemon takes the one it would have chosen.
        assert_eq!(format_from_mask(&bits), Some(SampleFormat::S16Le));
    }
}
