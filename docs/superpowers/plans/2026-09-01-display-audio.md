# Display Audio Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A VMLord display session carries the guest's sound to the host, on a fifth channel of its own, with mute and host device changes handled in the viewer.

**Architecture:** A guest system daemon reads the `snd-aloop` capture half through hand-written ALSA ioctls (no `libasound`), suppresses silence, and ships raw PCM periods over a fifth authenticated HvSocket channel. The host viewer renders them on a WASAPI thread that rebuilds its endpoint — but never its channel — when the host's default output changes. All limits and stream arithmetic live in a portable module inside `display-protocol` so they are testable without a guest, a sound card or a window.

**Tech Stack:** Rust; `prost` for the schema; hand-written kernel ABI (`SNDRV_PCM_IOCTL_*`) as in `crates/display-services/src/drm/uapi.rs`; the `windows` crate for WASAPI (`Win32_Media_Audio`); systemd units and WirePlumber configuration in the display payload.

**Spec:** `docs/superpowers/specs/2026-09-01-display-audio-design.md`

## Global Constraints

* Guest binaries build for `x86_64-unknown-linux-musl` with **no C toolchain**: never link `libasound`, `libpipewire` or any system C library. Guest deps stay pure Rust (`libc` crate is fine; it does not force a toolchain).
* All application code in Rust; no C, no FFI layer (AGENTS.md).
* `unsafe` is confined to platform modules: `display-services/src/alsa/` in the guest, `display-viewer/src/windows/` on the host.
* Log through `tracing` in host code and `eprintln!` in guest services (existing pattern in `clipboard_main.rs`). User-facing events go through `vmlord_core::diagnostic!`.
* **No PCM sample bytes in any log line, at any level, on either side.**
* Channel: `Channel::Audio = 5`, port `VMLS` `0x564D_4C53`, service GUID `564D4C53-FACB-11E6-BD58-64006A7986D3`.
* `AUDIO_MAX_PAYLOAD = 16 * 1024`.
* Default pinned capture format: `S16_LE`, 48000 Hz, 2 channels, 480-frame periods, 4 periods (1920-frame buffer).
* Silence suppression threshold: 10 consecutive silent periods (100 ms). Guest ring: 20 periods (200 ms).
* Commit subjects are `TASK-124: <comment>`. Work happens on branch `TASK-124-display-audio` (already created).
* Build/test commands: `cargo test -p vmlord-display-protocol`, `cargo display-services`, `cargo check-windows`, `cargo test-windows`.

---

### Task 1: The fifth channel in the protocol

**Files:**
- Modify: `crates/display-protocol/proto/vmlord/display/v1/display.proto`
- Modify: `crates/display-protocol/src/record.rs` (`Channel`, `Channel::from_wire`, `Display`, `Limits::for_channel`, new `AUDIO_MAX_PAYLOAD`)
- Modify: `crates/display-protocol/src/session.rs` (`channels: [ChannelState; 4]`, `handover_keys: [Option<ChannelKey>; 4]`)
- Test: the `#[cfg(test)] mod tests` already at the bottom of `record.rs` and `session.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Channel::Audio`, `AUDIO_MAX_PAYLOAD: u32`, proto enums `AudioRecord`, `SampleFormat`, message `AudioFormat`, `Capability::Audio` (`CAPABILITY_AUDIO = 6`).

- [ ] **Step 1: Write the failing test**

In `crates/display-protocol/src/record.rs`, in the existing test module:

```rust
#[test]
fn audio_is_the_fifth_channel() {
    assert_eq!(Channel::Audio.as_wire(), 5);
    assert_eq!(Channel::from_wire(5).unwrap(), Channel::Audio);
    assert_eq!(Channel::Audio.to_string(), "audio");

    let limits = Limits::new(1920, 1080);
    assert_eq!(limits.for_channel(Channel::Audio), AUDIO_MAX_PAYLOAD);
    assert_eq!(AUDIO_MAX_PAYLOAD, 16 * 1024);
}

#[test]
fn an_audio_record_survives_a_round_trip_with_its_stream_position() {
    // `base` carries the number of frames captured before this record, the
    // way a tile delta carries the sequence it builds on.
    let record = Record::new(Channel::Audio, 5, 7, 48_000, 2, vec![3u8; 3840]);
    let bytes = record.header.encode();
    let (header, extra) = Header::decode(&bytes).unwrap();

    assert_eq!(extra, 0);
    assert_eq!(header.channel, Channel::Audio);
    assert_eq!(header.base, 48_000);
    assert_eq!(header.length, 3840);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vmlord-display-protocol audio_is_the_fifth_channel`
Expected: FAIL — `no variant named Audio found for enum Channel`.

- [ ] **Step 3: Extend the schema**

In `display.proto`, add to `Capability`:

```proto
  // The session carries the guest's audio output on a fifth channel.
  // Announced by a guest whose payload ships the audio daemon, whether or
  // not anything is playing: a session commonly opens at the login screen,
  // and a capability cannot be renegotiated once the handshake settles it.
  CAPABILITY_AUDIO = 6;
```

and add, beside the other per-channel record enums:

```proto
// The `type` field of a record header on the audio channel.
//
// The first three match every other bound channel's, because a bind is the
// same three records everywhere.
enum AudioRecord {
  AUDIO_RECORD_UNSPECIFIED = 0;
  AUDIO_RECORD_CHANNEL_HELLO = 1;
  AUDIO_RECORD_CHANNEL_ACK = 2;
  AUDIO_RECORD_CHANNEL_AUTH = 3;
  AUDIO_RECORD_FORMAT = 4;
  // Carries interleaved PCM, not a message from this schema. Its stream
  // position rides in the header's `base` field.
  AUDIO_RECORD_DATA = 5;
  AUDIO_RECORD_ERROR = 6;
}

// How one sample is laid out. Named after ALSA's own spelling.
enum SampleFormat {
  SAMPLE_FORMAT_UNSPECIFIED = 0;
  SAMPLE_FORMAT_S16_LE = 1;
  SAMPLE_FORMAT_S32_LE = 2;
  SAMPLE_FORMAT_FLOAT_LE = 3;
}

// What the guest pinned on the loopback, sent once after the bind and again
// whenever the device is reopened with different parameters.
message AudioFormat {
  uint32 sample_rate = 1;
  uint32 channels = 2;
  SampleFormat sample_format = 3;
  // How many frames one AUDIO_RECORD_DATA record carries.
  uint32 frames_per_period = 4;
}
```

- [ ] **Step 4: Add the channel**

In `record.rs`: add `Audio = 5` to `Channel` with the doc comment `/// The guest's audio output, from the guest only.`, extend `from_wire` with `5 => Ok(Self::Audio)`, extend the `Display` impl with `Self::Audio => "audio"`, add

```rust
/// The most an audio record may carry.
///
/// A 480-frame period of S32 stereo is 3840 bytes; the rest is room for a
/// longer period the guest pinned rather than chose.
pub const AUDIO_MAX_PAYLOAD: u32 = 16 * 1024;
```

and extend `Limits::for_channel` with `Channel::Audio => AUDIO_MAX_PAYLOAD`. Update the module doc's "four channels" wording to five.

In `session.rs`: widen `channels` to `[ChannelState; 4]` and `handover_keys` to `[Option<ChannelKey>; 4]`, and update the comment above `channels` to read `/// Per channel, in `Channel` order: frame, input, clipboard, then audio.` Follow the existing index arithmetic (`channel as usize - 2`) — verify by reading `take_channel_sequence`.

- [ ] **Step 5: Extend the golden encodings and the fuzz target**

The four existing channels each have a golden encoding and appear in the record
fuzz target; find them with `grep -rn "golden" crates/display-protocol` and
`ls crates/display-protocol/fuzz/fuzz_targets` (if the fuzz targets live
elsewhere, `grep -rn "fuzz_target" crates | head`). Add the audio channel to
both, so that a header whose channel byte is 5 is exercised by the same
machinery as the other four.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p vmlord-display-protocol`
Expected: PASS, including the existing clipboard and session tests.

- [ ] **Step 7: Regenerate the checked-in descriptor set**

Run: `cargo build -p vmlord-display-protocol`
Then confirm `git status` shows `crates/display-protocol/proto/display.descriptor.bin` modified. If the build does not rewrite it, read `crates/display-protocol/build.rs` and follow whatever it does to emit the descriptor.

- [ ] **Step 8: Commit**

```bash
git add crates/display-protocol
git commit -m "TASK-124: Add the audio channel to the display protocol"
```

---

### Task 2: The portable audio module

**Files:**
- Create: `crates/display-protocol/src/audio.rs`
- Modify: `crates/display-protocol/src/lib.rs` (add `pub mod audio;`)
- Test: `#[cfg(test)] mod tests` inside `audio.rs`

**Interfaces:**
- Consumes: `Channel::Audio`, `AUDIO_MAX_PAYLOAD` from Task 1.
- Produces:
  - `pub const SILENT_PERIODS_BEFORE_SUPPRESSION: u32 = 10;`
  - `pub const RING_PERIODS: usize = 20;`
  - `pub struct Format { pub sample_rate: u32, pub channels: u32, pub sample_format: SampleFormat, pub frames_per_period: u32 }` with `pub fn bytes_per_frame(&self) -> usize` and `pub fn period_bytes(&self) -> usize`
  - `pub struct Stream` with `pub fn new(format: Format) -> Self`, `pub fn captured(&mut self, period: &[u8]) -> Option<Sent>`, `pub fn queue(&mut self, sent: Sent)`, `pub fn take(&mut self) -> Option<Sent>`, `pub fn position(&self) -> u32`
  - `pub struct Sent { pub position: u32, pub bytes: Vec<u8> }`
  - `pub fn frames_between(earlier: u32, later: u32) -> u32`

- [ ] **Step 1: Write the failing tests**

Create `crates/display-protocol/src/audio.rs` with only the test module at first:

```rust
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
            let sent = stream.captured(&silence()).expect("still within the threshold");
            assert_eq!(sent.position, expected * 480);
        }

        assert!(stream.captured(&silence()).is_none());
        assert!(stream.captured(&silence()).is_none());

        // The stream position keeps counting, so the host sees the gap.
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
        // The first three periods were dropped, and the positions say so.
        assert_eq!(taken[0], 3 * 480);
        assert_eq!(taken[RING_PERIODS - 1], (RING_PERIODS + 2) * 480);
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-protocol audio::`
Expected: FAIL — the module does not compile, `Format` and `Stream` are undefined.

- [ ] **Step 3: Write the module**

Above the test module in `audio.rs`:

```rust
//! What both ends of the audio channel agree about, with no device and no
//! socket in it.
//!
//! The guest reads periods off a loopback and the host pushes them into
//! WASAPI, but the rules that decide which periods travel, how many may wait,
//! and where a period sits in the stream are the same on both sides and belong
//! to neither. They live here so that they can be tested without a guest, a
//! sound card or a window.

use std::collections::VecDeque;

/// How many periods of pure silence pass before the guest stops sending.
///
/// Ten periods is 100 ms at the pinned format: long enough that a quiet
/// passage does not make the stream flap, short enough that an idle desktop
/// stops costing bandwidth almost at once.
pub const SILENT_PERIODS_BEFORE_SUPPRESSION: u32 = 10;

/// How many periods may wait for the socket before the oldest is dropped.
///
/// Twenty periods is 200 ms. The reader never blocks on the socket: late audio
/// is worse than absent audio, and a capture that waits is a capture that can
/// hold up something else.
pub const RING_PERIODS: usize = 20;

/// How one sample is laid out.
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
    /// How many bytes one sample of this format occupies.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::S16Le => 2,
            Self::S32Le | Self::FloatLe => 4,
        }
    }
}

/// What the guest pinned on the loopback.
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

    /// How many frames have been captured, modulo 2^32.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.position
    }

    /// Takes one captured period, and says whether it is worth sending.
    ///
    /// The position advances either way: a suppressed period is a gap the host
    /// reads out of the next record's position, which is why silence needs no
    /// record of its own.
    pub fn captured(&mut self, period: &[u8]) -> Option<Sent> {
        let position = self.position;
        let frames = (period.len() / self.format.bytes_per_frame()) as u32;
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
/// Wrapping, because the position is 32 bits and wraps after about 24.8 hours
/// at 48 kHz. This is correct for any gap shorter than 2^32 frames, which is
/// every gap that can occur in a session.
#[must_use]
pub fn frames_between(earlier: u32, later: u32) -> u32 {
    later.wrapping_sub(earlier)
}
```

Note the off-by-one the tests pin down: the threshold counts silent periods, and suppression begins on the period *after* the tenth, so ten silent periods still travel.

Add `pub mod audio;` to `lib.rs`, in alphabetical order before `pub mod clipboard;`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-display-protocol audio::`
Expected: PASS, six tests.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p vmlord-display-protocol --all-targets`
Expected: no warnings from `audio.rs`.

- [ ] **Step 6: Commit**

```bash
git add crates/display-protocol/src/audio.rs crates/display-protocol/src/lib.rs
git commit -m "TASK-124: Add the portable audio stream rules"
```

---

### Task 3: The ALSA ABI, written out

**Files:**
- Create: `crates/display-services/src/alsa/uapi.rs`
- Create: `crates/display-services/src/alsa/mod.rs`
- Modify: `crates/display-services/src/lib.rs` (add `pub mod alsa;`)
- Test: `#[cfg(test)] mod tests` inside `uapi.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `SndPcmHwParams`, `SndPcmSwParams`, `SndXferi`, `SNDRV_PCM_IOCTL_PVERSION/HW_REFINE/HW_PARAMS/SW_PARAMS/PREPARE/START/DROP/READI_FRAMES`, `params_any() -> SndPcmHwParams`, `set_mask(&mut SndPcmHwParams, usize, u32)`, `set_exact(&mut SndPcmHwParams, usize, u32)`, `interval(&SndPcmHwParams, usize) -> (u32, u32)`, the index constants `ACCESS`, `FORMAT`, `CHANNELS`, `RATE`, `PERIOD_SIZE`, `PERIODS`, `BUFFER_SIZE`, and `ACCESS_RW_INTERLEAVED`, `FORMAT_S16_LE`, `FORMAT_S32_LE`, `FORMAT_FLOAT_LE`.

**Reference:** the spike binary that proved these numbers is throwaway and is not in the repository; its findings are in the spec's "What the spike established". `READI_FRAMES` is `_IOR`, not `_IOW`. Mirror the style of `crates/display-services/src/drm/uapi.rs` exactly: `io_none`/`io_read`/`io_write`/`io_write_read` helpers, kernel spellings for every name, and a doc comment saying why the ABI is written out rather than linked.

- [ ] **Step 1: Write the failing test**

In `uapi.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // The kernel reads these structures by offset. A field added, reordered or
    // widened here is a guest that fails at HW_PARAMS with EINVAL and no
    // explanation, so the layout is pinned by a test rather than by care.
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
        // 'A' << 8, with the direction bits and the size the kernel expects.
        assert_eq!(SNDRV_PCM_IOCTL_PREPARE, 0x0000_4140);
        assert_eq!(SNDRV_PCM_IOCTL_START, 0x0000_4142);
        // READI_FRAMES is a read: the kernel writes `result` back. Encoding it
        // as a write costs an ENOTTY that says nothing about why.
        assert_eq!(
            SNDRV_PCM_IOCTL_READI_FRAMES,
            io_read(0x51, size_of::<SndXferi>() as u32)
        );
    }

    #[test]
    fn an_any_parameter_set_asks_for_everything() {
        let params = params_any();

        assert!(params.masks.iter().all(|mask| mask.bits == [!0u32; 8]));
        assert!(params.intervals.iter().all(|i| i.min == 0 && i.max == u32::MAX));
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl alsa::`
Expected: FAIL — module not found.

- [ ] **Step 3: Write the ABI**

Write `uapi.rs` with the layout the spike proved:

```rust
//! The kernel's PCM ABI, written out rather than linked.
//!
//! No `libasound`: linking one would cost the toolchain-free
//! cross-compilation the whole guest side rests on, and what is needed here is
//! eight ioctls and three structures. Every item is named after the kernel's
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

/// `struct snd_mask`: 256 bits naming which values of a parameter are allowed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SndMask {
    /// The bits themselves, one per enumerated value.
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
```

…continuing with `SndPcmHwParams` (`flags`, `masks: [SndMask; 3]`, `mres: [SndMask; 5]`, `intervals: [SndInterval; 12]`, `ires: [SndInterval; 9]`, `rmask`, `cmask`, `info`, `msbits`, `rate_num`, `rate_den`, `fifo_size: libc::c_ulong`, `reserved: [u8; 64]`), `SndPcmSwParams` (`tstamp_mode: i32`, `period_step`, `sleep_min`, then `avail_min`, `xfer_align`, `start_threshold`, `stop_threshold`, `silence_threshold`, `silence_size`, `boundary` as `libc::c_ulong`, then `proto`, `tstamp_type`, `reserved: [u8; 56]`), `SndXferi` (`result: libc::c_long`, `buf: *mut libc::c_void`, `frames: libc::c_ulong`), the eight request constants (`PVERSION` `io_read(0x00, 4)`, `HW_REFINE` `io_write_read(0x10, …)`, `HW_PARAMS` `io_write_read(0x11, …)`, `SW_PARAMS` `io_write_read(0x13, …)`, `PREPARE` `io_none(0x40)`, `START` `io_none(0x42)`, `DROP` `io_none(0x43)`, `READI_FRAMES` `io_read(0x51, …)`), the parameter index constants (masks: `ACCESS = 0`, `FORMAT = 1`, `SUBFORMAT = 2`; intervals, already offset by the kernel's first-interval number: `SAMPLE_BITS = 0`, `FRAME_BITS = 1`, `CHANNELS = 2`, `RATE = 3`, `PERIOD_SIZE = 5`, `PERIODS = 7`, `BUFFER_SIZE = 9`), the values `ACCESS_RW_INTERLEAVED = 3`, `FORMAT_S16_LE = 2`, `FORMAT_S32_LE = 10`, `FORMAT_FLOAT_LE = 14`, and the three helpers `params_any`, `set_mask`, `set_exact`, `interval` with the bodies the spike used.

Leave `mod.rs` as `pub mod uapi;` plus its module doc for now; Task 4 fills it.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl alsa::`
Expected: PASS, four tests. If a size assertion fails, fix the structure, not the assertion: the numbers come from the kernel header.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/alsa crates/display-services/src/lib.rs
git commit -m "TASK-124: Write out the kernel PCM ABI"
```

---

### Task 4: The capture device

**Files:**
- Modify: `crates/display-services/src/alsa/mod.rs`
- Test: `#[cfg(test)] mod tests` inside `mod.rs` (parameter arithmetic only; opening a device needs a guest)

**Interfaces:**
- Consumes: everything from Task 3; `Format`, `SampleFormat` from Task 2.
- Produces:
  - `pub struct Capture` with `pub fn open(path: &str, wanted: Format) -> Result<Self, CaptureError>`, `pub fn format(&self) -> Format`, `pub fn read(&mut self, into: &mut [u8]) -> Result<usize, CaptureError>`, `pub fn recover(&mut self) -> Result<(), CaptureError>`
  - `pub enum CaptureError { Open(io::Error), Refine(io::Error), Params(io::Error), Start(io::Error), Read(io::Error), Unsupported(String) }` with `Display`
  - `pub const DEFAULT_DEVICE: &str = "/dev/snd/pcmC0D1c";`
  - `pub fn boundary(buffer_frames: libc::c_ulong) -> libc::c_ulong`
  - `pub fn format_from_mask(bits: &[u32; 8]) -> Option<SampleFormat>`

**Behaviour to implement, from the spec:** open the device; `HW_REFINE` with the wanted format; if the refine narrows to something else (because PipeWire opened the playback half first and pinned it), take what is offered rather than failing — read the format, channels and rate back out of the refined parameters and report them through `format()`. Then `HW_PARAMS`, `SW_PARAMS` (`start_threshold = 1`, `stop_threshold = boundary`, `avail_min = period`, `proto` from `PVERSION`), `PREPARE`, `START`. `read` issues `READI_FRAMES` and returns frames read; on `EPIPE` (an xrun) it returns `Ok(0)` after a `PREPARE`+`START` via `recover`, because an xrun on a loopback is a gap, not a failure.

- [ ] **Step 1: Write the failing tests**

```rust
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl alsa::`
Expected: FAIL — `boundary` and `format_from_mask` are undefined.

- [ ] **Step 3: Implement the device**

Write `Capture` and the helpers. `boundary` doubles the buffer size while `boundary * 2 < libc::c_ulong::MAX / 2`, exactly as the spike did. `format_from_mask` returns `Some` only for `FORMAT_S16_LE`, `FORMAT_S32_LE` and `FORMAT_FLOAT_LE`, and `None` for anything else, so that an unexpected pinning is reported as `CaptureError::Unsupported` rather than sent as bytes the host will misread. All `unsafe` stays in this module; every ioctl goes through one small `fn call(fd, request, argument) -> io::Result<()>` helper that turns `-1` into `io::Error::last_os_error()`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl alsa::`
Expected: PASS, seven tests across both files.

- [ ] **Step 5: Check it builds for the guest target**

Run: `cargo display-services`
Expected: builds clean.

- [ ] **Step 6: Commit**

```bash
git add crates/display-services/src/alsa/mod.rs
git commit -m "TASK-124: Open and read the loopback capture device"
```

---

### Task 5: The broker hands out the audio key

**Files:**
- Modify: `crates/display-services/src/ipc.rs` (new `Message::AudioOpened`, encode/decode)
- Modify: `crates/display-services/src/broker_main.rs` (`AUDIO_SOCKET_PATH`, `audio_socket` option, `audio_peer`, `audio` key state, a listener thread, hand-out on open and clear on close)
- Modify: `crates/display-services/src/broker.rs` schema if the IPC messages are Protobuf (read `broker::envelope` before editing)
- Modify: `crates/display-services/src/vsock.rs` (`pub const AUDIO_PORT: u32 = 0x564D_4C53;`)
- Test: the existing test modules in `ipc.rs` and `broker_main.rs`

**Interfaces:**
- Consumes: `Channel::Audio` (Task 1).
- Produces: `Message::AudioOpened { session_id: Vec<u8>, audio_key: Vec<u8> }`, `vsock::AUDIO_PORT`, the socket path `/run/vmlord/display-audio.sock`.

**Pattern to follow:** `ClipboardOpened` and `serve_clipboard_peers` in `broker_main.rs` are the template, with one deliberate difference — the clipboard socket is authorised against the uid of the active graphical session because a selection belongs to whoever is at the screen. Audio does not: the daemon is a system service running as `vmlord-display`, so bind the socket with `Listener::bind(path, group_of("vmlord-display"))` and accept with `Listener::accept(expected_uid)` against that account's uid, the way the session socket does. Read how the session socket resolves its uid and copy that.

- [ ] **Step 1: Write the failing test**

In `ipc.rs`'s test module:

```rust
#[test]
fn an_audio_opened_message_survives_a_round_trip() {
    let message = Message::AudioOpened {
        session_id: vec![1; 16],
        audio_key: vec![2; 32],
    };

    assert_eq!(decode(&encode(&message)).unwrap(), message);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl ipc::`
Expected: FAIL — no variant `AudioOpened`.

- [ ] **Step 3: Add the message and the socket**

Add the variant with the doc comment `/// A control handshake completed, as the audio daemon needs it.`, extend `encode`/`decode` beside `ClipboardOpened`, add `AUDIO_PORT` to `vsock.rs`, and wire the broker: a second constant `const AUDIO_SOCKET_PATH: &str = "/run/vmlord/display-audio.sock";`, an `audio_socket` option read from `VMLORD_DISPLAY_AUDIO_SOCKET`, `audio_peer: Option<Arc<Connection>>` and `audio: Option<(Vec<u8>, Vec<u8>)>` on `Shared`, a `serve_audio_peers` thread in the same scope as the clipboard's, the key derived in `open_session` with `Channel::Audio`, and the state cleared where `state.clipboard = None` is cleared.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/ipc.rs crates/display-services/src/broker_main.rs crates/display-services/src/vsock.rs
git commit -m "TASK-124: Hand the audio channel key to a fourth guest process"
```

---

### Task 6: The guest audio daemon

**Files:**
- Create: `crates/display-services/src/audio_main.rs`
- Create: `crates/display-services/src/bin/audio.rs` (three lines, mirroring `src/bin/clipboard.rs`)
- Modify: `crates/display-services/Cargo.toml` (a `[[bin]]` named `vmlord-display-audio`)
- Modify: `crates/display-services/src/lib.rs` (`pub mod audio_main;`)
- Test: `#[cfg(test)] mod tests` inside `audio_main.rs`

**Interfaces:**
- Consumes: `alsa::Capture` (Task 4), `audio::{Format, Stream, Sent}` (Task 2), `Message::AudioOpened` (Task 5), `channel::bind`, `vsock::{Listener, AUDIO_PORT}`.
- Produces: the binary `vmlord-display-audio`.

**Order that matters, from the spec:** bind the vsock channel **before** opening ALSA, so a guest with no `snd-aloop` answers `AudioError` with a reason instead of never appearing. Then send `AudioFormat` with what was actually pinned, then loop: read a period, `Stream::captured`, `Stream::queue`, drain into records of type `AUDIO_RECORD_DATA` with `base` set to `Sent::position`. Never block the reader on the socket — set the socket non-blocking and drop from the ring when it would block. A second `AudioFormat` goes out whenever the device is reopened with different parameters.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use vmlord_display_protocol::audio::{Format, SampleFormat};

    #[test]
    fn a_data_record_carries_the_period_and_its_position_in_the_header() {
        let record = data_record(
            &Sent { position: 4800, bytes: vec![7u8; 1920] },
            3,
            2,
        );

        assert_eq!(record.header.channel, Channel::Audio);
        assert_eq!(record.header.message_type, AudioRecord::Data as u16);
        assert_eq!(record.header.base, 4800);
        assert_eq!(record.header.sequence, 3);
        assert_eq!(record.header.generation, 2);
        assert_eq!(record.payload.len(), 1920);
    }

    #[test]
    fn the_format_record_says_what_was_pinned() {
        let format = Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S32Le,
            frames_per_period: 480,
        };

        let message = format_message(format);

        assert_eq!(message.sample_rate, 48_000);
        assert_eq!(message.channels, 2);
        assert_eq!(message.sample_format, v1::SampleFormat::S32Le as i32);
        assert_eq!(message.frames_per_period, 480);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl audio_main::`
Expected: FAIL — `data_record` and `format_message` are undefined.

- [ ] **Step 3: Write the daemon**

`audio_main.rs` follows `clipboard_main.rs`'s shape: a `pub fn main()` that connects to `/run/vmlord/display-audio.sock`, waits for `Message::AudioOpened`, binds `AUDIO_PORT`, runs `channel::bind` with the key, then runs the capture loop. Keep `data_record` and `format_message` as small free functions so the test above can reach them. Log the format, frame counts, positions and outcomes — never a payload byte.

`src/bin/audio.rs`:

```rust
//! The guest's audio daemon. See `vmlord_display_services::audio_main`.

fn main() {
    vmlord_display_services::audio_main::main();
}
```

`Cargo.toml`, beside the other three:

```toml
[[bin]]
name = "vmlord-display-audio"
path = "src/bin/audio.rs"
bench = false
```

- [ ] **Step 4: Write the two tests the spec asks for by name**

The first is domain separation — what `keys::channel_key` exists for. Follow the
equivalent clipboard test (`grep -rn "frame key" crates/display-services/src/channel.rs`
and the tests below it) and add:

```rust
#[test]
fn an_audio_socket_that_offers_a_frame_key_does_not_bind() {
    let session = SessionKey::from_bytes([4; 32]);
    let transcript = [9u8; 32];
    let audio = keys::channel_key(&session, &transcript, Channel::Audio);
    let frame = keys::channel_key(&session, &transcript, Channel::Frame);

    assert_ne!(audio.to_bytes(), frame.to_bytes());
    // …then drive `channel::bind` over a duplex with the frame key and assert
    // it fails, the way the clipboard's bind test does.
}
```

The second is the logging rule, and it is a test rather than a convention
because a rule nobody checks is a rule that decays:

```rust
#[test]
fn no_log_line_carries_a_sample() {
    let period: Vec<u8> = (0..1920u32).map(|n| (n % 251) as u8 + 1).collect();

    let lines = describe_period(&Sent { position: 4800, bytes: period.clone() });

    assert!(lines.contains("4800"), "the position is worth logging");
    assert!(lines.contains("1920"), "so is the byte count");
    for window in period.windows(8) {
        assert!(
            !lines.as_bytes().windows(8).any(|seen| seen == window),
            "no run of the payload reaches a log line"
        );
    }
}
```

`describe_period` is the one function every audio log line goes through: it
returns the format, the position, the byte count and the outcome, and it is the
only thing the daemon is allowed to pass to `eprintln!` about a period.

- [ ] **Step 5: Run the tests and build the guest binaries**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl` then `cargo display-services`
Expected: PASS, and four binaries in `target/x86_64-unknown-linux-musl/release/`.

- [ ] **Step 6: Commit**

```bash
git add crates/display-services
git commit -m "TASK-124: Add the guest audio daemon"
```

---

### Task 7: The payload carries the daemon, the module and the routing

**Files:**
- Create: `payloads/display/services/vmlord-display-audio.service`
- Create: `payloads/display/audio/vmlord-audio-modules.conf` (for `/etc/modules-load.d/`)
- Create: `payloads/display/audio/vmlord-audio-modprobe.conf` (for `/etc/modprobe.d/`)
- Create: `payloads/display/audio/51-vmlord-loopback.conf` (a WirePlumber 0.5 rule)
- Create: `payloads/display/audio/51-vmlord-loopback.lua` (the same rule for WirePlumber 0.4)
- Modify: `payloads/display/prepare.sh` (the binary list, and installing the three configuration files)
- Modify: `payloads/display/README.md` (the tree description)
- Modify: `crates/agent/src/display_kernel.rs` (`SERVICE_BINARIES`, `SYSTEM_UNITS`, the install steps for the new configuration files, and the `modprobe snd-aloop` stage)
- Test: the existing test module at the bottom of `display_kernel.rs`

**Interfaces:**
- Consumes: the `vmlord-display-audio` binary (Task 6).
- Produces: an installed, enabled `vmlord-display-audio.service`; `snd-aloop` loaded at boot; one loopback cable; the loopback hidden as a source.

- [ ] **Step 1: Write the failing test**

In `display_kernel.rs`'s test module:

```rust
#[test]
fn the_audio_daemon_is_installed_and_started_as_a_system_unit() {
    assert!(super::SERVICE_BINARIES.contains(&"vmlord-display-audio"));
    assert!(super::SYSTEM_UNITS.contains(&"vmlord-display-audio.service"));
    // It is not a user unit: audio does not belong to whoever is at the
    // screen, and the daemon must run before anyone logs in.
    assert!(!super::USER_UNITS.contains(&"vmlord-display-audio.service"));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p vmlord-agent the_audio_daemon_is_installed`
Expected: FAIL — the arrays do not hold it, and their lengths are fixed at `[&str; 3]` and `[&str; 2]`.

- [ ] **Step 3: Write the unit**

`payloads/display/services/vmlord-display-audio.service`:

```ini
[Unit]
Description=VMLord display audio
Documentation=https://github.com/mrundead/vmlord
# After, and deliberately not BindsTo: a broker restart must not take the
# capture down, and the daemon reconnects to the socket on its own.
After=vmlord-display-broker.service

# The crash-loop budget. It belongs to the unit and not to the service:
# systemd reads these two here, and answers them under [Service] with
# `Unknown key ... ignoring` -- a rate limit that silently is not one.
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=/usr/local/lib/vmlord/vmlord-display-audio
User=vmlord-display
# /dev/snd/* is root:audio 0660 plus an ACL for the user of the active seat.
# The group is what lets a system daemon open the loopback without belonging
# to anyone's session -- which is the whole reason audio is not a user unit.
SupplementaryGroups=audio
Restart=on-failure
RestartSec=2
# It holds one channel key and one PCM device.
CapabilityBoundingSet=
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
RestrictAddressFamilies=AF_VSOCK AF_UNIX
RestrictNamespaces=yes
MemoryDenyWriteExecute=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
```

`payloads/display/audio/vmlord-audio-modules.conf`:

```
# Shipped at /etc/modules-load.d/vmlord-audio.conf
# The ALSA loopback is what the desktop plays into and what
# vmlord-display-audio captures out of, with no PipeWire in the hot path.
snd-aloop
```

`payloads/display/audio/vmlord-audio-modprobe.conf`:

```
# Shipped at /etc/modprobe.d/vmlord-audio.conf
# One cable, not the default two: the second shows up in GNOME as a duplicate
# output device that plays into nothing anyone is capturing.
options snd-aloop index=0 pcm_substreams=1
```

The loopback's capture side must never be offered to the user as a microphone,
and WirePlumber says so in two different languages: 26.04 carries 0.5.13, whose
drop-ins are SPA-JSON in `/etc/wireplumber/wireplumber.conf.d/` (the shipped
`/usr/share/wireplumber/wireplumber.conf.d/alsa-vm.conf` is the template), while
22.04 carries 0.4, whose rules are Lua in `/etc/wireplumber/main.lua.d/`. Both
forms ship, and the recipe installs whichever the guest understands.

`payloads/display/audio/51-vmlord-loopback.conf`, for WirePlumber 0.5:

```
# Shipped at /etc/wireplumber/wireplumber.conf.d/51-vmlord-loopback.conf
# The loopback's capture side is where vmlord-display-audio reads the desktop
# from. It is not a microphone, and a VM that offers it as one gets a source
# that records whatever is playing.

monitor.alsa.rules = [
  {
    matches = [
      {
        node.name = "~alsa_input.platform-snd_aloop.*"
      }
    ]
    actions = {
      update-props = {
        node.disabled = true
      }
    }
  }
]
```

`payloads/display/audio/51-vmlord-loopback.lua`, for WirePlumber 0.4:

```lua
-- Shipped at /etc/wireplumber/main.lua.d/51-vmlord-loopback.lua
-- The same rule as 51-vmlord-loopback.conf, in the language 0.4 reads: the
-- loopback's capture side is what vmlord-display-audio reads, not a microphone.
table.insert(alsa_monitor.rules, {
  matches = {
    {
      { "node.name", "matches", "alsa_input.platform-snd_aloop.*" },
    },
  },
  apply_properties = {
    ["node.disabled"] = true,
  },
})
```

The recipe picks between them by directory: install the `.conf` when
`/usr/share/wireplumber/wireplumber.conf.d` exists, and the `.lua` when
`/usr/share/wireplumber/main.lua.d` does. Installing both is harmless — each
version ignores the other's directory — but choosing keeps a guest's
configuration honest about what is actually read.

- [ ] **Step 4: Wire prepare.sh and the recipe**

Add `vmlord-display-audio` to the `for binary in …` list in `prepare.sh`, install `payloads/display/audio/*` into `prepared/content/audio/`, widen `SERVICE_BINARIES` to `[&str; 4]` and `SYSTEM_UNITS` to `[&str; 3]` in `display_kernel.rs`, and add the recipe steps that copy the three configuration files into `/etc/modules-load.d/`, `/etc/modprobe.d/` and the WirePlumber directory, then `modprobe snd-aloop` so the first boot after provisioning has audio without a reboot. Follow the existing copy-and-verify step style in that file, and give the new steps `SHORT_BUDGET`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-agent`
Expected: PASS.

- [ ] **Step 6: Rebuild a payload to prove the tree is complete**

Run: `./rebuild_payload.sh`
Expected: the Docker build succeeds and `target/display-payload/prepared/content/services/` holds four binaries and four unit files, and `…/content/audio/` holds the three configuration files.

- [ ] **Step 7: Commit**

```bash
git add payloads/display crates/agent/src/display_kernel.rs
git commit -m "TASK-124: Ship the audio daemon and the loopback configuration"
```

---

### Task 8: The host opens the fifth service

**Files:**
- Modify: `crates/platform/src/hcs_config.rs` (`DISPLAY_AUDIO_SERVICE_KEY`, its entry in the service table)
- Modify: `crates/platform/src/display_session.rs` (`DISPLAY_AUDIO_VSOCK_PORT`, `audio_port` in the parameters, `audio_key` in the hand-over)
- Modify: `crates/display-viewer/src/launch.rs` (`audio_port` on the launch parameters, `audio_key` on the hand-over, and the encode/decode both directions)
- Modify: `crates/display-viewer/build.rs`'s schema if the launch pipe's proto is a separate file — read it first
- Modify: `crates/display-viewer/src/live.rs` (`channel_key(&handover.audio_key, "audio")` into `HandedOver`)
- Test: the existing test modules in `hcs_config.rs`, `display_session.rs` and `launch.rs`

**Interfaces:**
- Consumes: `Channel::Audio` (Task 1), the broker's key hand-out (Task 5).
- Produces: `audio_port: u32` and `audio_key: Vec<u8>` reaching the viewer.

- [ ] **Step 1: Write the failing tests**

In `display_session.rs`'s tests, beside the clipboard assertions:

```rust
#[test]
fn the_hand_over_carries_a_distinct_audio_key() {
    // …build a session the way the clipboard test above does…
    assert_eq!(handover.audio_key.len(), 32);
    assert_ne!(handover.audio_key, handover.frame_key);
    assert_ne!(handover.audio_key, handover.clipboard_key);
}

#[test]
fn the_parameters_name_the_audio_port() {
    assert_eq!(parameters.audio_port, 0x564D_4C53);
}
```

In `hcs_config.rs`'s tests, extend whatever asserts the service table's contents with `564D4C53-FACB-11E6-BD58-64006A7986D3`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p vmlord-platform` and `cargo test -p vmlord-display-viewer`
Expected: FAIL — no `audio_key` field.

- [ ] **Step 3: Add the fifth service everywhere**

Mirror `clipboard` in each file. The service table entry gets the same `BindSecurityDescriptor` as the other four. In `live.rs`, `Live::new` derives the fourth channel key and passes it into `Session::established_host`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-platform && cargo check-windows`
Expected: PASS and a clean check.

- [ ] **Step 5: Commit**

```bash
git add crates/platform crates/display-viewer
git commit -m "TASK-124: Register the audio service and carry its key to the viewer"
```

---

### Task 9: The WASAPI renderer

**Files:**
- Create: `crates/display-viewer/src/windows/audio.rs`
- Modify: `crates/display-viewer/src/windows/mod.rs` (`pub mod audio;`)
- Modify: `crates/display-viewer/Cargo.toml` (add `"Win32_Media_Audio"` and `"Win32_System_Com_StructuredStorage"` to the `windows` features if the endpoint enumeration needs it — add only what fails to compile without)
- Test: `#[cfg(test)] mod tests` inside `audio.rs` for the format conversion; the device paths need Windows and are covered by the manual matrix

**Interfaces:**
- Consumes: `audio::{Format, SampleFormat, frames_between}` (Task 2).
- Produces:
  - `pub struct Renderer` with `pub fn new(format: Format) -> Result<Self, RendererError>`, `pub fn play(&mut self, position: u32, pcm: &[u8])`, `pub fn rebuild(&mut self) -> Result<(), RendererError>`, `pub fn set_muted(&mut self, muted: bool)`, `pub fn parked(&self) -> bool`
  - `pub fn wave_format(format: Format) -> WAVEFORMATEX`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use vmlord_display_protocol::audio::{Format, SampleFormat};

    #[test]
    fn a_pinned_format_becomes_the_wave_format_wasapi_converts_from() {
        let wave = wave_format(Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S16Le,
            frames_per_period: 480,
        });

        assert_eq!(wave.wFormatTag, WAVE_FORMAT_PCM as u16);
        assert_eq!(wave.nChannels, 2);
        assert_eq!(wave.nSamplesPerSec, 48_000);
        assert_eq!(wave.wBitsPerSample, 16);
        assert_eq!(wave.nBlockAlign, 4);
        assert_eq!(wave.nAvgBytesPerSec, 48_000 * 4);
    }

    #[test]
    fn a_float_format_is_tagged_as_float_rather_than_pcm() {
        let wave = wave_format(Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::FloatLe,
            frames_per_period: 480,
        });

        assert_eq!(wave.wFormatTag, WAVE_FORMAT_IEEE_FLOAT as u16);
        assert_eq!(wave.wBitsPerSample, 32);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test-windows -p vmlord-display-viewer audio::`
Expected: FAIL — `wave_format` is undefined.

- [ ] **Step 3: Write the renderer**

`Renderer::new` activates the default `eRender`/`eConsole` endpoint through `IMMDeviceEnumerator`, initialises `IAudioClient` in `AUDCLNT_SHAREMODE_SHARED` with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT` and the `wave_format` above — the guest's format, not the endpoint's mix format, because WASAPI is what converts — takes `IAudioRenderClient`, and starts.

`play` drops the period rather than waiting when `GetCurrentPadding` leaves no room: late audio is worse than absent audio. It also drops when muted, after taking the bytes — a muted renderer keeps draining so that unmuting is instant. It uses `frames_between(self.last_position, position)` only to log a gap, never to insert silence: the endpoint's own clock covers a gap.

`rebuild` stops and releases `IAudioRenderClient`, `IAudioClient` and `IMMDevice`, then activates the new default endpoint and initialises it with the same `WAVEFORMATEX`. If that fails, or there is no endpoint at all, it parks: `parked()` becomes true, `play` discards, and one `diagnostic!` goes out rather than one per period.

All `unsafe` and every COM call stay in this file.

- [ ] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-display-viewer audio::`
Expected: PASS, two tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer/src/windows/audio.rs crates/display-viewer/src/windows/mod.rs crates/display-viewer/Cargo.toml
git commit -m "TASK-124: Render guest audio through WASAPI"
```

---

### Task 10: Device changes rebuild the endpoint, never the channel

**Files:**
- Modify: `crates/display-viewer/src/windows/audio.rs` (the `IMMNotificationClient` implementation and its registration)
- Test: `#[cfg(test)] mod tests` in `audio.rs`

**Interfaces:**
- Consumes: `Renderer` (Task 9).
- Produces: `pub struct DeviceWatch` with `pub fn register(enumerator: &IMMDeviceEnumerator) -> Result<Self, RendererError>`, `pub fn take_request(&self) -> bool` (an `Arc<AtomicBool>` swapped to false under the hood), `pub fn request(&self)` (what the callbacks call) and `#[cfg(test)] pub fn for_test() -> Self` (the flag without COM, so the arithmetic is testable off Windows).

**The decision this task implements, from the spec:** AppSandbox restarts the whole channel when the host's default endpoint changes — its renderer and its socket share a loop, so it has no way to restart one without the other. Here only the endpoint is rebuilt and the channel stays bound: the guest neither knows nor should know that a host user plugged in headphones, the wire format does not change, and a rebind costs a round trip that can fail.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_notification_asks_for_a_rebuild_exactly_once() {
    let watch = DeviceWatch::for_test();

    watch.request();
    watch.request();

    assert!(watch.take_request(), "the first take sees the request");
    assert!(!watch.take_request(), "and it is not seen twice");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test-windows -p vmlord-display-viewer audio::`
Expected: FAIL — `DeviceWatch` is undefined.

- [ ] **Step 3: Implement the watch**

Implement `IMMNotificationClient` with the `windows` crate's `#[implement]` attribute. Handle three callbacks and no others: `OnDefaultDeviceChanged` (only for `eRender`), `OnDeviceStateChanged` and `OnDeviceAdded` — the last so that a host which had no endpoint at all revives when one appears. Each sets the atomic flag; the callbacks arrive on someone else's thread and must do nothing more. Register with `RegisterEndpointNotificationCallback` and unregister on drop. `for_test` builds the flag without COM so the arithmetic is testable off Windows.

Then, in the audio thread's loop (Task 11 wires it), call `renderer.rebuild()` whenever `watch.take_request()` is true, before the next `play`.

- [ ] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-display-viewer audio::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer/src/windows/audio.rs
git commit -m "TASK-124: Rebuild the audio endpoint when the host's default output changes"
```

---

### Task 11: The audio thread, and mute

**Files:**
- Create: `crates/display-viewer/src/audio/mod.rs` (the thread, its parameters, its channel bind — mirroring `windows/clipboard.rs`'s `spawn`)
- Modify: `crates/display-viewer/src/lib.rs` (`pub mod audio;`)
- Modify: `crates/display-viewer/src/main.rs` (`start_audio`/`stop_audio` beside `start_clipboard`/`stop_clipboard`)
- Modify: `crates/display-viewer/src/windows/window.rs` (`SC_MUTE_AUDIO`, the menu item, the `WM_SYSCOMMAND` arm, the check mark)
- Modify: `crates/display-viewer/src/state.rs` (`muted: bool` on `WindowState`, its `parse` and `render`)
- Modify: `crates/ui/locales/*` if the menu string goes through `t!` — check how "Fullscreen" is spelled in `window.rs` first; if the viewer's menu strings are literals, follow that and do not introduce `t!` here
- Test: the existing test modules in `state.rs` and `window.rs`

**Interfaces:**
- Consumes: `Renderer`, `DeviceWatch` (Tasks 9–10), `Capability::Audio` (Task 1), `audio_port`/`audio_key` (Task 8).
- Produces: `pub fn spawn(parameters: Parameters) -> (JoinHandle<()>, Sender<Mute>)`, `pub struct Parameters { pub runtime_id: u128, pub port: u32, pub handover: Handover }`, `pub enum Mute { On, Off }`.

- [ ] **Step 1: Write the failing tests**

In `state.rs`:

```rust
#[test]
fn the_muted_flag_survives_a_save_and_a_load() {
    let state = WindowState {
        muted: true,
        ..WindowState::default()
    };

    assert!(WindowState::parse(&state.render()).muted);
}

#[test]
fn a_state_file_written_before_audio_existed_loads_unmuted() {
    let earlier = "size=1920x1080\nfullscreen=false\nquality=desktop\n";

    assert!(!WindowState::parse(earlier).muted);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer state::`
Expected: FAIL — no field `muted`.

- [ ] **Step 3: Add the state, the menu item and the thread**

`WindowState` gains `muted: bool`, defaulting false and parsed from `muted=true|false`; an older file without the key loads unmuted, which the second test pins.

`window.rs` gains `pub const SC_MUTE_AUDIO: usize = 0x9060;` (the low four bits must stay clear — `WM_SYSCOMMAND` masks them), the menu item beside `SC_FULLSCREEN`, a `WM_SYSCOMMAND` arm that toggles and calls `CheckMenuItem`, and a `UiEvent` that reaches the session thread the way the other menu commands do.

`audio/mod.rs::spawn` opens the socket for `Channel::Audio`, binds it with the hand-over's key, builds a `Renderer` from the first `AudioFormat` record, then loops: rebuild when `watch.take_request()`, read a record, and `play(header.base, &payload)`. A second `AudioFormat` mid-stream rebuilds the renderer around the new format. `Mute` messages set `renderer.set_muted`.

`main.rs` gains `start_audio`, which returns `None` when the hand-over does not carry `Capability::Audio` — the same shape and the same reasoning as `start_clipboard` — and `stop_audio`, which drops the sender.

- [ ] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-display-viewer && cargo check-windows`
Expected: PASS and a clean check.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer crates/ui
git commit -m "TASK-124: Play the guest's audio in the viewer, with mute"
```

---

### Task 12: Documentation

**Files:**
- Modify: `ARCHITECTURE.md` (the display session's channels, the guest services, the payload's contents, and what never reaches a record)
- Modify: `README.md` if it lists what a display session carries
- Modify: the compatibility matrix, troubleshooting and user guide under `docs/` — find them with `grep -rl "clipboard" docs | grep -v superpowers`
- Modify: `payloads/display/README.md` (already touched in Task 7; confirm it names four binaries and the audio configuration)

- [ ] **Step 1: Find every place that says a session has four channels**

Run: `grep -rn "four channels\|three sockets\|four sockets" --include=*.md --include=*.rs . | grep -v target`
Expected: a list; every hit is either updated or is a historical statement about AppSandbox that must stay.

- [ ] **Step 2: Write the documentation**

Cover: the fifth channel and its GUID; that the guest daemon pins the format and why whoever opens first wins; that silence is not sent; that mute is host-side; that a host device change rebuilds the endpoint and not the channel; that there is no microphone; and the troubleshooting entry for a guest whose `snd-aloop` is missing (`AudioError`, what the journal says, `modprobe snd-aloop`).

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md README.md docs payloads/display/README.md
git commit -m "TASK-124: Document display audio"
```

---

### Task 13: The manual matrix on a guest

**Files:** none — this task produces evidence, not code.

**Prerequisites:** the `test` VM (Ubuntu 26.04, GNOME profile), a rebuilt display payload from Task 7 installed in it, and a host with at least two audio output devices.

- [ ] **Step 1: Provision and start a session**

Install the rebuilt payload, reboot the guest, confirm `systemctl status vmlord-display-audio` is running and `cat /proc/asound/cards` shows `Loopback`, then connect the display from VMLord.

- [ ] **Step 2: Work through the matrix, recording the outcome of each**

- [ ] Sound plays in GNOME and is heard on the host.
- [ ] An idle desktop sends nothing: the daemon's log shows suppression, and the channel stays bound.
- [ ] Mute silences within a period; unmute resumes immediately.
- [ ] **The host's default output is changed while sound is playing** — audio continues on the new device within a second, the viewer's log shows a rebuild, and the log does *not* show a channel rebind.
- [ ] The only host endpoint is disabled: the viewer parks, reports once, and revives when it is re-enabled.
- [ ] `systemctl restart vmlord-display-audio` in the guest: the channel rebinds and sound returns.
- [ ] `systemctl --user restart pipewire` in the guest: sound returns without touching the daemon.
- [ ] The session is reconnected from the viewer: audio comes back with it.
- [ ] The display's frame rate under continuous audio matches the rate without it (compare with the viewer's own FPS reporting).
- [ ] A guest with `snd-aloop` removed (`rmmod snd-aloop` after stopping the daemon) reports `AudioError`, and the reason reaches VMLord's diagnostics.
- [ ] `journalctl -u vmlord-display-audio` and the viewer log contain no PCM bytes.

- [ ] **Step 3: Record the results in the task**

Post the matrix and its outcomes as a comment on Vikunja task #124, noting the guest release and kernel version they were taken on.

---

## Notes for the executor

* Ubuntu 22.04 and 24.04 are not part of this plan's verification; the release matrix belongs to #128. Both WirePlumber configuration forms ship (Task 7), so a 22.04 guest is configured even though nobody has watched one do it.
* If Task 4's `HW_REFINE` narrows to a format `format_from_mask` refuses, that is the "PipeWire opened first with something unexpected" case: report `AudioError` with the refused format named, and do not send bytes the host would misread.
* A merge request is opened only after the user's explicit approval, assigned to `mrundead` with a review requested from `mrundead` (AGENTS.md).
