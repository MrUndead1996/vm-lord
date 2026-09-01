//! The kernel's PCM ABI, written out rather than linked.
//!
//! No system `libasound`: linking one would cost the toolchain-free
//! cross-compilation the whole guest side rests on, and what is needed here is
//! eight requests and three structures. Every item is named after the kernel's
//! own spelling, so that a reader can find it in `include/uapi/sound/asound.h`.

/// The `'A'` every PCM request is built on.
const PCM: u32 = 0x41;

/// `_IOC(_IOC_NONE, ..)`, the encoding `_IO` builds.
#[must_use]
pub const fn io_none(number: u32) -> libc::c_ulong {
    ((PCM << 8) | number) as libc::c_ulong
}

/// `_IOC(_IOC_READ, ..)`, the encoding `_IOR` builds.
#[must_use]
pub const fn io_read(number: u32, size: u32) -> libc::c_ulong {
    ((2 << 30) | (size << 16) | (PCM << 8) | number) as libc::c_ulong
}

/// `_IOC(_IOC_READ | _IOC_WRITE, ..)`, the encoding `_IOWR` builds.
#[must_use]
pub const fn io_write_read(number: u32, size: u32) -> libc::c_ulong {
    ((3 << 30) | (size << 16) | (PCM << 8) | number) as libc::c_ulong
}

/// `struct snd_mask`: which values of an enumerated parameter are allowed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndMask {
    /// One bit per value, low word first.
    pub bits: [u32; 8],
}

/// `struct snd_interval`: the range a numeric parameter may take.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndInterval {
    /// The lowest value allowed.
    pub min: u32,
    /// The highest value allowed.
    pub max: u32,
    /// `openmin`, `openmax`, `integer` and `empty`, in that bit order.
    pub flags: u32,
}

/// `openmin`: the minimum is exclusive.
pub const INTERVAL_OPENMIN: u32 = 1 << 0;
/// `openmax`: the maximum is exclusive.
pub const INTERVAL_OPENMAX: u32 = 1 << 1;
/// `integer`: every value in the range is a whole number.
pub const INTERVAL_INTEGER: u32 = 1 << 2;

/// `struct snd_pcm_hw_params`: what a stream's hardware parameters may be.
///
/// The kernel narrows this in place: a caller fills it with everything it
/// would accept, and `HW_REFINE` or `HW_PARAMS` answers with what is left.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmHwParams {
    /// `SNDRV_PCM_HW_PARAMS_*`, none of which this build sets.
    pub flags: u32,
    /// Access, format and subformat, in that order.
    pub masks: [SndMask; 3],
    /// Space the kernel reserves for masks it does not yet have.
    pub mres: [SndMask; 5],
    /// The twelve numeric parameters, from sample bits to tick time.
    pub intervals: [SndInterval; 12],
    /// Space the kernel reserves for intervals it does not yet have.
    pub ires: [SndInterval; 9],
    /// Which parameters the caller wants refined.
    pub rmask: u32,
    /// Which parameters the kernel changed.
    pub cmask: u32,
    /// `SNDRV_PCM_INFO_*` for the configured stream.
    pub info: u32,
    /// Significant bits per sample, when it is not all of them.
    pub msbits: u32,
    /// The rate as a fraction, numerator.
    pub rate_num: u32,
    /// The rate as a fraction, denominator.
    pub rate_den: u32,
    /// The hardware FIFO's size in frames.
    pub fifo_size: libc::c_ulong,
    /// Reserved by the kernel, and zero here.
    pub reserved: [u8; 64],
}

/// `struct snd_pcm_sw_params`: when the kernel wakes a caller and when it stops.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndPcmSwParams {
    /// Whether timestamps are taken.
    pub tstamp_mode: i32,
    /// How many periods a transfer advances by.
    pub period_step: u32,
    /// Obsolete, and zero.
    pub sleep_min: u32,
    /// How many frames must be available before a caller is woken.
    pub avail_min: libc::c_ulong,
    /// Obsolete, and zero.
    pub xfer_align: libc::c_ulong,
    /// How many frames must be queued before the stream starts.
    pub start_threshold: libc::c_ulong,
    /// How far the stream may drift before the kernel stops it.
    pub stop_threshold: libc::c_ulong,
    /// When the kernel fills a gap with silence.
    pub silence_threshold: libc::c_ulong,
    /// How much silence it fills a gap with.
    pub silence_size: libc::c_ulong,
    /// Where the ring's arithmetic wraps.
    pub boundary: libc::c_ulong,
    /// The protocol version the caller was built against.
    pub proto: u32,
    /// Which clock timestamps come from.
    pub tstamp_type: u32,
    /// Reserved by the kernel, and zero here.
    pub reserved: [u8; 56],
}

/// `struct snd_xferi`: one interleaved transfer.
#[repr(C)]
pub struct SndXferi {
    /// How many frames moved, written by the kernel.
    pub result: libc::c_long,
    /// Where they go.
    pub buf: *mut libc::c_void,
    /// How many were asked for.
    pub frames: libc::c_ulong,
}

/// The ABI version this device speaks.
pub const SNDRV_PCM_IOCTL_PVERSION: libc::c_ulong = io_read(0x00, size_of::<i32>() as u32);

/// Narrows a parameter set without configuring anything.
pub const SNDRV_PCM_IOCTL_HW_REFINE: libc::c_ulong =
    io_write_read(0x10, size_of::<SndPcmHwParams>() as u32);

/// Narrows a parameter set and configures the stream from what is left.
pub const SNDRV_PCM_IOCTL_HW_PARAMS: libc::c_ulong =
    io_write_read(0x11, size_of::<SndPcmHwParams>() as u32);

/// Sets the thresholds a configured stream runs by.
pub const SNDRV_PCM_IOCTL_SW_PARAMS: libc::c_ulong =
    io_write_read(0x13, size_of::<SndPcmSwParams>() as u32);

/// Readies a configured stream, and re-readies one that has xrun.
pub const SNDRV_PCM_IOCTL_PREPARE: libc::c_ulong = io_none(0x40);

/// Starts a prepared stream.
pub const SNDRV_PCM_IOCTL_START: libc::c_ulong = io_none(0x42);

/// Stops a stream and discards what it holds.
pub const SNDRV_PCM_IOCTL_DROP: libc::c_ulong = io_none(0x43);

/// Reads interleaved frames.
///
/// `_IOR`, not `_IOW`: the kernel writes `result` back into the structure.
/// Encoding it as a write costs an `ENOTTY` that says nothing about why.
pub const SNDRV_PCM_IOCTL_READI_FRAMES: libc::c_ulong = io_read(0x51, size_of::<SndXferi>() as u32);

/// Where the access mask lives in [`SndPcmHwParams::masks`].
pub const ACCESS: usize = 0;
/// Where the format mask lives.
pub const FORMAT: usize = 1;
/// Where the subformat mask lives.
pub const SUBFORMAT: usize = 2;

/// Where sample bits live in [`SndPcmHwParams::intervals`].
///
/// The indices below are the kernel's parameter numbers with its first
/// interval subtracted, because the intervals are their own array here.
pub const SAMPLE_BITS: usize = 0;
/// Where frame bits live.
pub const FRAME_BITS: usize = 1;
/// Where the channel count lives.
pub const CHANNELS: usize = 2;
/// Where the sample rate lives.
pub const RATE: usize = 3;
/// Where the period size in frames lives.
pub const PERIOD_SIZE: usize = 5;
/// Where the period count lives.
pub const PERIODS: usize = 7;
/// Where the buffer size in frames lives.
pub const BUFFER_SIZE: usize = 9;

/// `SNDRV_PCM_ACCESS_RW_INTERLEAVED`: read frames, do not map them.
pub const ACCESS_RW_INTERLEAVED: u32 = 3;
/// `SNDRV_PCM_FORMAT_S16_LE`.
pub const FORMAT_S16_LE: u32 = 2;
/// `SNDRV_PCM_FORMAT_S32_LE`.
pub const FORMAT_S32_LE: u32 = 10;
/// `SNDRV_PCM_FORMAT_FLOAT_LE`.
pub const FORMAT_FLOAT_LE: u32 = 14;

/// A parameter set that would accept anything the device offers.
#[must_use]
pub fn params_any() -> SndPcmHwParams {
    let mut params: SndPcmHwParams = unsafe { std::mem::zeroed() };

    for mask in &mut params.masks {
        mask.bits = [!0u32; 8];
    }

    for interval in &mut params.intervals {
        interval.min = 0;
        interval.max = !0u32;
        interval.flags = 0;
    }

    params.rmask = !0u32;
    params
}

/// Narrows a mask to one value.
pub fn set_mask(params: &mut SndPcmHwParams, index: usize, value: u32) {
    params.masks[index].bits = [0; 8];
    params.masks[index].bits[(value / 32) as usize] = 1 << (value % 32);
}

/// Narrows an interval to one number.
pub fn set_exact(params: &mut SndPcmHwParams, index: usize, value: u32) {
    params.intervals[index] = SndInterval {
        min: value,
        max: value,
        flags: INTERVAL_INTEGER,
    };
}

/// What an interval was narrowed to.
#[must_use]
pub fn interval(params: &SndPcmHwParams, index: usize) -> (u32, u32) {
    (params.intervals[index].min, params.intervals[index].max)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The kernel reads these structures by offset. A field added, reordered or
    // widened is a guest that fails at HW_PARAMS with EINVAL and no
    // explanation, so the layout is held by a test rather than by care.
    #[test]
    fn the_structures_match_the_kernel_layout() {
        assert_eq!(size_of::<SndMask>(), 32);
        assert_eq!(size_of::<SndInterval>(), 12);
        assert_eq!(size_of::<SndPcmHwParams>(), 608);
        assert_eq!(size_of::<SndPcmSwParams>(), 136);
        assert_eq!(size_of::<SndXferi>(), 24);
    }

    #[test]
    fn the_requests_are_encoded_the_way_asound_h_encodes_them() {
        assert_eq!(SNDRV_PCM_IOCTL_PREPARE, 0x0000_4140);
        assert_eq!(SNDRV_PCM_IOCTL_START, 0x0000_4142);
        assert_eq!(
            SNDRV_PCM_IOCTL_READI_FRAMES,
            io_read(0x51, size_of::<SndXferi>() as u32)
        );
    }

    #[test]
    fn an_any_parameter_set_asks_for_everything() {
        let params = params_any();

        assert!(params.masks.iter().all(|mask| mask.bits == [!0u32; 8]));
        assert!(
            params
                .intervals
                .iter()
                .all(|interval| interval.min == 0 && interval.max == u32::MAX)
        );
        assert_eq!(params.rmask, u32::MAX);
    }

    #[test]
    fn a_mask_names_one_value_and_an_interval_pins_one_number() {
        let mut params = params_any();
        set_mask(&mut params, FORMAT, FORMAT_S16_LE);
        set_exact(&mut params, RATE, 48_000);

        assert_eq!(params.masks[FORMAT].bits[0], 1 << FORMAT_S16_LE);
        assert_eq!(params.masks[FORMAT].bits[1..], [0; 7]);
        assert_eq!(interval(&params, RATE), (48_000, 48_000));
    }
}
