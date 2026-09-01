//! What both ends of the audio channel agree about, with no device in it.
//!
//! The guest reads periods off an ALSA loopback and the host pushes them into
//! WASAPI, but the rules that decide which periods travel, how many may wait
//! and where a period sits in the stream are the same at both ends and belong
//! to neither. They live here so that they can be tested without a guest, a
//! sound card or a window.

use std::collections::VecDeque;

/// How many periods of pure silence travel before the guest stops sending.
///
/// Ten periods is 100 ms at the pinned format: long enough that a quiet
/// passage does not make the stream flap, short enough that an idle desktop
/// stops costing bandwidth almost at once.
pub const SILENT_PERIODS_BEFORE_SUPPRESSION: u32 = 10;

/// How many periods may wait for the socket before the oldest is dropped.
///
/// Twenty periods is 200 ms. The capture never blocks on the socket: late
/// audio is worse than absent audio, and a reader that waits is a reader that
/// can hold something else up.
pub const RING_PERIODS: usize = 20;

/// How one sample is laid out, named after ALSA's own spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// 16-bit signed, little-endian: what the daemon pins when it opens first.
    S16Le,
    /// 32-bit signed, little-endian: what PipeWire pins when it opens first.
    S32Le,
    /// 32-bit float, little-endian.
    FloatLe,
}

impl SampleFormat {
    /// What one sample of this format weighs.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::S16Le => 2,
            Self::S32Le | Self::FloatLe => 4,
        }
    }
}

/// What the guest pinned on the loopback.
///
/// Not what it asked for: whichever half of the loopback opens first fixes the
/// other, so a daemon that starts at boot chooses this and a daemon that
/// restarts into a playing desktop is handed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    /// Frames per second.
    pub sample_rate: u32,
    /// Interleaved channels per frame.
    pub channels: u32,
    /// How one sample is laid out.
    pub sample_format: SampleFormat,
    /// How many frames one record carries.
    pub frames_per_period: u32,
}

impl Format {
    /// One frame's width in bytes.
    #[must_use]
    pub fn bytes_per_frame(&self) -> usize {
        self.sample_format.bytes() * self.channels as usize
    }

    /// One period's width in bytes, which is one record's payload.
    #[must_use]
    pub fn period_bytes(&self) -> usize {
        self.bytes_per_frame() * self.frames_per_period as usize
    }
}

/// One period on its way to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// How many frames were captured before this period, modulo 2^32.
    pub position: u32,
    /// The interleaved PCM itself.
    pub bytes: Vec<u8>,
}

/// The guest's side of the stream: what travels, and what waits.
pub struct Stream {
    format: Format,
    position: u32,
    silent_periods: u32,
    waiting: VecDeque<Sent>,
}

impl Stream {
    /// A stream that has captured nothing yet.
    #[must_use]
    pub fn new(format: Format) -> Self {
        Self {
            format,
            position: 0,
            silent_periods: 0,
            waiting: VecDeque::with_capacity(RING_PERIODS),
        }
    }

    /// The format this stream carries.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// How many frames have been captured, modulo 2^32.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.position
    }

    /// Takes one captured period, and says whether it is worth sending.
    ///
    /// The position advances either way. A suppressed period is a gap the host
    /// reads out of the next record's position, which is why silence needs no
    /// record of its own and no flag to end it.
    pub fn captured(&mut self, period: &[u8]) -> Option<Sent> {
        let position = self.position;
        let frames = u32::try_from(period.len() / self.format.bytes_per_frame()).unwrap_or(0);
        self.position = self.position.wrapping_add(frames);

        if period.iter().all(|byte| *byte == 0) {
            self.silent_periods = self.silent_periods.saturating_add(1);

            if self.silent_periods > SILENT_PERIODS_BEFORE_SUPPRESSION {
                return None;
            }
        } else {
            self.silent_periods = 0;
        }

        Some(Sent {
            position,
            bytes: period.to_vec(),
        })
    }

    /// Puts a period in the queue, dropping the oldest if it is full.
    pub fn queue(&mut self, sent: Sent) {
        if self.waiting.len() == RING_PERIODS {
            self.waiting.pop_front();
        }

        self.waiting.push_back(sent);
    }

    /// Takes the oldest period that is still waiting.
    pub fn take(&mut self) -> Option<Sent> {
        self.waiting.pop_front()
    }
}

/// How many frames lie between two stream positions.
///
/// Wrapping, because the position is 32 bits and comes round after about 24.8
/// hours at 48 kHz. That is correct for every gap a session can hold.
#[must_use]
pub fn frames_between(earlier: u32, later: u32) -> u32 {
    later.wrapping_sub(earlier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_s16() -> Format {
        Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S16Le,
            frames_per_period: 480,
        }
    }

    fn silence() -> Vec<u8> {
        vec![0u8; stereo_s16().period_bytes()]
    }

    fn sound() -> Vec<u8> {
        let mut bytes = silence();
        bytes[17] = 9;
        bytes
    }

    #[test]
    fn a_period_carries_its_position_and_advances_the_stream() {
        let mut stream = Stream::new(stereo_s16());

        let first = stream.captured(&sound()).expect("sound is sent");
        let second = stream.captured(&sound()).expect("sound is sent");

        assert_eq!(first.position, 0);
        assert_eq!(second.position, 480);
        assert_eq!(stream.position(), 960);
    }

    #[test]
    fn silence_is_suppressed_only_after_the_threshold_and_still_advances() {
        let mut stream = Stream::new(stereo_s16());

        for expected in 0..SILENT_PERIODS_BEFORE_SUPPRESSION {
            let sent = stream
                .captured(&silence())
                .expect("still within the threshold");

            assert_eq!(sent.position, expected * 480);
        }

        assert!(stream.captured(&silence()).is_none());
        assert!(stream.captured(&silence()).is_none());

        // The position keeps counting while nothing is sent, which is how the
        // host learns how long the silence was.
        let resumed = stream.captured(&sound()).expect("sound ends suppression");

        assert_eq!(resumed.position, 12 * 480);
    }

    #[test]
    fn sound_resets_the_silence_count() {
        let mut stream = Stream::new(stereo_s16());

        for _ in 0..SILENT_PERIODS_BEFORE_SUPPRESSION - 1 {
            assert!(stream.captured(&silence()).is_some());
        }

        assert!(stream.captured(&sound()).is_some());

        for _ in 0..SILENT_PERIODS_BEFORE_SUPPRESSION - 1 {
            assert!(stream.captured(&silence()).is_some(), "the count restarted");
        }
    }

    #[test]
    fn the_ring_drops_the_oldest_and_the_gap_is_visible_in_positions() {
        let mut stream = Stream::new(stereo_s16());

        for _ in 0..RING_PERIODS + 3 {
            if let Some(sent) = stream.captured(&sound()) {
                stream.queue(sent);
            }
        }

        let mut taken = Vec::new();
        while let Some(sent) = stream.take() {
            taken.push(sent.position);
        }

        assert_eq!(taken.len(), RING_PERIODS);
        // The first three were dropped, and the positions say so rather than
        // the host having to be told.
        assert_eq!(taken[0], 3 * 480);
        assert_eq!(taken[RING_PERIODS - 1], (RING_PERIODS + 2) as u32 * 480);
    }

    #[test]
    fn a_period_never_exceeds_the_record_cap() {
        assert!(stereo_s16().period_bytes() <= crate::record::AUDIO_MAX_PAYLOAD as usize);
    }

    #[test]
    fn positions_are_compared_across_the_wrap() {
        assert_eq!(frames_between(48_000, 48_480), 480);
        assert_eq!(frames_between(u32::MAX - 100, 380), 481);
        assert_eq!(frames_between(500, 500), 0);
    }
}
