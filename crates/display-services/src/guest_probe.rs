//! What a real dma-buf's coherency call costs, measured in a guest.
//!
//! `cargo display-pipeline-bench` runs anywhere, and its mapped rows are over a
//! memfd -- which answers `DMA_BUF_IOCTL_SYNC` with `ENOTTY`, so what it
//! measures is a mapping's cost with the sync skipped. The call is only real on
//! a buffer a DRM driver exported, and the only place one of those exists is a
//! guest with `vmlord_drm` loaded and a compositor committing to it.
//!
//! So this reads the desktop that is actually on screen, over and over, and
//! reports what the bracket around each read adds. Nothing is written: the
//! descriptor is exported without `DRM_RDWR`, exactly as capture takes it.
//!
//! It then runs that same desktop through the pipeline the session uses, in
//! both cursor modes, and decodes what came out to check it is the frame that
//! went in. `cargo display-pipeline-bench` does this on synthetic scenes and on
//! whatever host built it; here the pixels are a real GNOME desktop, the cursor
//! is the plane mutter actually committed, and the machine is the guest.
//!
//! It needs the rights `DRM_IOCTL_MODE_GETFB2` needs, which in a guest means
//! running it under `sudo`.

use std::{collections::HashMap, io, path::Path, time::Instant};

use vmlord_display_codec::{Decoder, Geometry, PixelFormat, Rect, TileSize};
use vmlord_display_protocol::{
    record::{self, Limits},
    v1,
};

use crate::{
    capture::{Backing, CapturedFrame, MappedBuffer},
    cursor,
    drm::{DRM_CLASS, DRM_DEVICES, Device},
    ipc::PlaneKind,
    pipeline::Pipeline,
};

/// The driver whose card this reads. `hyperv_drm` is the pre-boot console and
/// is not what a session captures.
const DRIVER: &str = "vmlord_drm";

/// How many times each plane is read, unless `--reads` names another count.
const READS: u32 = 200;

/// How many commits the damage check watches, unless `--damage` names another
/// count. Two hundred is a few seconds of a desktop that is doing something.
const DAMAGE_FRAMES: u32 = 200;

/// How many vblanks the damage check waits through with nothing committed
/// before it gives up and reports what it has.
///
/// A guest with a still screen commits nothing at all, and a probe that waited
/// for frames that are not coming is a probe that never returns.
const DAMAGE_STALL: u32 = 600;

/// What the probe was asked to do.
struct Arguments {
    reads: u32,
    /// Commits to run the damage check over. Zero skips it, which is what a
    /// guest with nothing moving on screen wants: the check waits for commits
    /// that are not coming.
    damage: u32,
}

/// What one plane's reads came to.
struct Report {
    kind: &'static str,
    width: u32,
    height: u32,
    stride: u32,
    bytes: usize,
    /// The mean of a read bracketed by the coherency calls, as capture does it.
    mean_synced_ms: f64,
    /// The mean of the same read with the bracket left off.
    mean_bare_ms: f64,
    /// The mean of the two ioctls alone, with nothing read between them.
    mean_sync_only_ms: f64,
}

/// Reads `--reads` and `--damage`.
fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Arguments, String> {
    let mut values = arguments.into_iter();
    let mut parsed = Arguments {
        reads: READS,
        damage: DAMAGE_FRAMES,
    };

    while let Some(flag) = values.next() {
        let mut number = |name: &str| -> Result<u32, String> {
            values
                .next()
                .ok_or_else(|| format!("missing value for {name}"))?
                .parse()
                .map_err(|_| format!("{name} wants a number"))
        };
        match flag.as_str() {
            "--reads" => parsed.reads = number("--reads")?,
            "--damage" => parsed.damage = number("--damage")?,
            _ => return Err(format!("unknown argument `{flag}`")),
        }
    }

    if parsed.reads == 0 {
        return Err("--reads wants at least one read".to_owned());
    }

    Ok(parsed)
}

/// Sums a buffer's bytes.
///
/// Something has to be done with what was read, or the whole loop is a mapping
/// nobody touched and the pages are never faulted in. A sum over words is close
/// to what the encoder's own first pass does.
fn digest(bytes: &[u8]) -> u64 {
    bytes
        .chunks_exact(4)
        .map(|word| u64::from(u32::from_le_bytes([word[0], word[1], word[2], word[3]])))
        .fold(0u64, u64::wrapping_add)
}

/// Measures one plane's buffer.
fn measure(
    mapped: &MappedBuffer,
    kind: PlaneKind,
    plane: &crate::drm::PlaneState,
    reads: u32,
) -> Report {
    // One read before the clocks start, so the cost of faulting the mapping in
    // does not land on whichever measurement happened to run first.
    let mut sink = mapped.read(digest);

    // Interleaved rather than one loop after the other. Run in sequence the
    // second loop inherits whatever state the first left, and the difference
    // being looked for here is smaller than that.
    let mut synced = 0u128;
    let mut bare = 0u128;
    for _ in 0..reads {
        let started = Instant::now();
        sink = sink.wrapping_add(mapped.read(digest));
        synced += started.elapsed().as_nanos();

        let started = Instant::now();
        sink = sink.wrapping_add(mapped.bytes(digest));
        bare += started.elapsed().as_nanos();
    }

    let started = Instant::now();
    for _ in 0..reads {
        mapped.read(|_| ());
    }
    let sync_only = started.elapsed().as_nanos();

    // Keeps the sums from being dead code without printing them: a digest that
    // is never used is a loop the optimiser is free to delete.
    if sink == u64::MAX {
        eprintln!("vmlord-display-guest-probe: an unlikely digest, printed so the reads are kept");
    }

    let reads_f = f64::from(reads);
    let mean = |nanos: u128| nanos as f64 / reads_f / 1e6;

    Report {
        kind: match kind {
            PlaneKind::Primary => "primary",
            PlaneKind::Cursor => "cursor",
        },
        width: plane.width,
        height: plane.height,
        stride: plane.stride,
        bytes: mapped.len(),
        mean_synced_ms: mean(synced),
        mean_bare_ms: mean(bare),
        mean_sync_only_ms: mean(sync_only),
    }
}

/// What one cursor mode's pass over the live desktop came to.
struct Pass {
    cursor: &'static str,
    mean_submit_ms: f64,
    mean_drain_ms: f64,
    bytes: usize,
    /// Whether the decoder rebuilt the frame the encoder was given. The whole
    /// point of running this here: the round trip is checked on the pixels a
    /// desktop actually has, not on a scene written to exercise the codec.
    round_trips: bool,
    /// Whether the decoded frame differs from the captured one at all, which
    /// in the drawn mode is the pointer having been composited in.
    pointer_changed_pixels: bool,
}

/// Runs the live desktop through the pipeline once per cursor mode.
///
/// The frame is submitted `passes` times so the means are not one sample, and
/// the record is decoded on the first pass, where the payload is the keyframe.
fn through_pipeline(
    device: &Device,
    primary: &crate::drm::PlaneState,
    cursor_plane: Option<&crate::drm::PlaneState>,
    passes: u32,
    stream: bool,
) -> Result<Pass, String> {
    let geometry = Geometry::new(
        primary.width,
        primary.height,
        TileSize::ThirtyTwo,
        PixelFormat::Xrgb8888,
    )
    .map_err(|error| format!("the desktop is not a geometry the codec accepts: {error}"))?;

    let length = primary.stride as usize * primary.height as usize;
    let descriptor = device
        .buffer(primary.fb_id)
        .ok_or_else(|| "the primary plane's buffer is not exported".to_owned())?;
    let mut mapped = MappedBuffer::map(descriptor, length)
        .map_err(|error| format!("the desktop would not map: {error}"))?;

    let limits = Limits::new(primary.width, primary.height);
    let mut pipeline = Pipeline::new(geometry, 1, stream);
    let mut tail = Vec::new();
    pipeline
        .write_stream_config(&mut tail, &limits)
        .map_err(|error| format!("the stream config would not be written: {error}"))?;
    tail.clear();

    // The cursor as the plane holds it, read once: what this measures is the
    // frame's cost, and re-reading a bitmap that has not changed is the thing
    // the session should stop doing anyway.
    let bitmap = cursor_plane.and_then(|plane| {
        let bytes = plane.stride as usize * plane.height as usize;
        let descriptor = device.buffer(plane.fb_id)?;
        let mapped = MappedBuffer::map(descriptor, bytes).ok()?;
        Some((
            mapped.read(<[u8]>::to_vec),
            plane.width,
            plane.height,
            plane.x,
            plane.y,
        ))
    });

    let mut submit_nanos = 0u128;
    let mut drain_nanos = 0u128;
    let mut bytes = 0usize;
    let mut round_trips = true;
    let mut pointer_changed_pixels = false;
    let mut placement = None;

    for pass in 0..passes {
        if let Some((pixels, width, height, x, y)) = &bitmap {
            let at = cursor::place(*x, *y, *width, *height, primary.width, primary.height);
            pipeline
                .submit_cursor(Some((pixels, *width, *height)), &at)
                .map_err(|error| format!("the cursor was refused: {error}"))?;
            placement = Some(at);
        }

        // The mapping is moved in and back out, as the session does it.
        let frame = CapturedFrame {
            sequence: u64::from(pass),
            width: primary.width,
            height: primary.height,
            stride: primary.stride,
            format: PixelFormat::Xrgb8888,
            damage: None,
            backing: Backing::Cpu(mapped),
        };
        let started = Instant::now();
        let staged = pipeline.submit_frame(&frame);
        submit_nanos += started.elapsed().as_nanos();
        let Backing::Cpu(returned) = frame.backing else {
            unreachable!("the backing this frame was given is the one it holds")
        };
        mapped = returned;
        staged.map_err(|error| format!("the desktop was refused: {error}"))?;

        let started = Instant::now();
        while pipeline
            .write_next(&mut tail, &limits)
            .map_err(|error| format!("a record would not be written: {error}"))?
        {}
        drain_nanos += started.elapsed().as_nanos();

        if pass == 0 {
            let mut reader = tail.as_slice();
            let mut payload = Vec::new();
            let header = record::read(&mut reader, &limits, &mut payload)
                .map_err(|error| format!("the record would not read back: {error}"))?;
            let mut decoder = Decoder::new(geometry);
            let applied = header.message_type == v1::FrameRecord::Keyframe as u16
                && decoder.apply_keyframe(&payload).is_ok();
            // What "round trips" means differs by mode, and both are worth
            // checking on a real desktop: a streamed frame must come back
            // exactly as captured, and a drawn one must come back as the
            // desktop everywhere the pointer is not.
            let captured = mapped.read(<[u8]>::to_vec);
            let decoded = decoder.frame();
            round_trips = applied
                && decoded.len() == captured.len()
                && if stream {
                    // Nothing was drawn over it, so it must come back byte for
                    // byte.
                    decoded == captured.as_slice()
                } else {
                    // A composite must leave every row the pointer does not
                    // reach exactly as it was. Which rows those are comes from
                    // the placement, not from a guess about where mutter put
                    // the cursor.
                    let stride = primary.stride as usize;
                    let below = placement
                        .map_or(0, |place| (place.y + place.crop.height) as usize * stride);
                    decoded[below..] == captured[below..]
                };
            // Whether the pointer actually landed. Reported rather than
            // asserted: a desktop whose cursor is hidden or fully transparent
            // has nothing to draw, and that is not a failure.
            pointer_changed_pixels = decoded != captured.as_slice();
            bytes = tail.len();
        }
        tail.clear();
    }

    let passes_f = f64::from(passes);

    Ok(Pass {
        cursor: if stream { "stream" } else { "drawn" },
        mean_submit_ms: submit_nanos as f64 / passes_f / 1e6,
        mean_drain_ms: drain_nanos as f64 / passes_f / 1e6,
        bytes,
        round_trips,
        pointer_changed_pixels,
    })
}

/// What running the live desktop past the encoder twice came to.
struct DamageReport {
    frames: u32,
    /// Whether the desktop stopped committing before the asked-for count was
    /// reached, which is a still screen rather than a fault.
    stalled: bool,
    /// Frames whose damage the driver and the compositor between them could
    /// account for in full.
    trusted: u32,
    /// The mean share of the frame those rectangles covered.
    mean_coverage: f64,
    trusting_bytes: usize,
    comparing_bytes: usize,
    /// The mean time each pipeline spent encoding a frame. The hint spares no
    /// bytes -- an honest one selects the same tiles the comparison would --
    /// so what it saves is here, in the comparison it made unnecessary.
    trusting_ms: f64,
    comparing_ms: f64,
    /// Whether the viewer that was told only about the damage ended up with
    /// the same picture as the viewer that compared every tile.
    ///
    /// This is the question the whole scheme rests on. Damage that misses a
    /// change is not an error anywhere -- the encoder is told a smaller truth
    /// and believes it -- so the only way to find out is to encode the same
    /// desktop both ways and compare what a decoder rebuilt.
    identical: bool,
    /// The frame the two first disagreed at, if they did.
    diverged_at: Option<u32>,
    /// The box that first differed, and what the compositor had said changed.
    /// Together these say whether the damage was merely late or somewhere else
    /// entirely.
    diverged_box: Option<(u32, u32, u32, u32)>,
    diverged_damage: Option<Vec<Rect>>,
    /// The first frame where the pixels changed somewhere the compositor's
    /// damage did not mention, with that box and what it did mention.
    ///
    /// Compared against the frame before rather than against an encoder's
    /// reference, so this says what the compositor got wrong without any of
    /// the codec in the way.
    uncovered_at: Option<u32>,
    uncovered_box: Option<(u32, u32, u32, u32)>,
    uncovered_damage: Option<Vec<Rect>>,
}

/// The box of pixels that changed and that no rectangle in `damage` covers.
fn uncovered(
    previous: &[u8],
    current: &[u8],
    width: u32,
    damage: &[Rect],
) -> Option<(u32, u32, u32, u32)> {
    let mut box_of: Option<(u32, u32, u32, u32)> = None;
    for (index, (before, after)) in previous
        .chunks_exact(4)
        .zip(current.chunks_exact(4))
        .enumerate()
    {
        if before == after {
            continue;
        }
        let x = index as u32 % width;
        let y = index as u32 / width;
        if damage.iter().any(|rect| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        }) {
            continue;
        }
        box_of = Some(match box_of {
            None => (x, y, x + 1, y + 1),
            Some((x1, y1, x2, y2)) => (x1.min(x), y1.min(y), x2.max(x + 1), y2.max(y + 1)),
        });
    }

    box_of
}

/// The box two frames differ in, in pixels, or `None` if they do not.
fn difference(left: &[u8], right: &[u8], width: u32) -> Option<(u32, u32, u32, u32)> {
    let row = width as usize * 4;
    let mut box_of: Option<(u32, u32, u32, u32)> = None;
    for (index, (a, b)) in left.chunks_exact(4).zip(right.chunks_exact(4)).enumerate() {
        if a == b {
            continue;
        }
        let x = (index % (row / 4)) as u32;
        let y = (index / (row / 4)) as u32;
        box_of = Some(match box_of {
            None => (x, y, x + 1, y + 1),
            Some((x1, y1, x2, y2)) => (x1.min(x), y1.min(y), x2.max(x + 1), y2.max(y + 1)),
        });
    }

    box_of
}

/// Encodes the live desktop twice at once: trusting damage, and comparing.
///
/// Both pipelines see the same captured frames in the same order, so what is
/// left between them is the hint. A viewer is simulated for each, and the two
/// pictures are compared after every frame.
fn verify_damage(device: &mut Device, frames: u32) -> Result<DamageReport, String> {
    let mut buffers: HashMap<u32, MappedBuffer> = HashMap::new();
    let mut geometry: Option<Geometry> = None;
    let mut limits = Limits::new(1, 1);
    let mut trusting: Option<Pipeline> = None;
    let mut comparing: Option<Pipeline> = None;
    let mut trusting_view: Option<Decoder> = None;
    let mut comparing_view: Option<Decoder> = None;

    let mut previous_commits: Option<u64> = None;
    let mut report = DamageReport {
        frames: 0,
        stalled: false,
        trusted: 0,
        mean_coverage: 0.0,
        trusting_bytes: 0,
        comparing_bytes: 0,
        trusting_ms: 0.0,
        comparing_ms: 0.0,
        identical: true,
        diverged_at: None,
        diverged_box: None,
        diverged_damage: None,
        uncovered_at: None,
        uncovered_box: None,
        uncovered_damage: None,
    };
    let mut before: Option<Vec<u8>> = None;
    let mut coverage = 0.0f64;
    let (mut trusting_nanos, mut comparing_nanos) = (0u128, 0u128);

    let mut tail = Vec::new();
    let mut idle = 0u32;
    while report.frames < frames {
        if idle >= DAMAGE_STALL {
            break;
        }
        if device.wait_vblank().is_err() {
            // The output's clock is off, which is a blanked desktop rather
            // than a fault. Waiting a frame's worth keeps this from spinning.
            std::thread::sleep(std::time::Duration::from_millis(16));
            idle += 1;
            continue;
        }
        let snapshot = device
            .snapshot()
            .map_err(|error| format!("the planes could not be read: {error} (try sudo)"))?;
        let Some(primary) = snapshot
            .planes
            .iter()
            .find(|plane| plane.kind == PlaneKind::Primary)
        else {
            continue;
        };

        // The session's rule, applied here so this measures what ships: damage
        // counts only for the very next commit after the one already encoded.
        let damage = match (previous_commits, primary.commits) {
            (Some(previous), Some(current)) if current == previous + 1 => primary.damage.clone(),
            _ => None,
        };
        if primary.commits == previous_commits {
            idle += 1;
            continue;
        }
        idle = 0;
        previous_commits = primary.commits;

        let shape = Geometry::new(
            primary.width,
            primary.height,
            TileSize::ThirtyTwo,
            PixelFormat::Xrgb8888,
        )
        .map_err(|error| format!("the desktop is not a geometry the codec accepts: {error}"))?;
        if geometry != Some(shape) {
            geometry = Some(shape);
            limits = Limits::new(primary.width, primary.height);
            trusting = Some(Pipeline::new(shape, 1, true));
            comparing = Some(Pipeline::new(shape, 1, true));
            trusting_view = Some(Decoder::new(shape));
            comparing_view = Some(Decoder::new(shape));
            buffers.clear();
        }

        // A framebuffer id the kernel handed to a different buffer is an id
        // whose mapping means nothing any more. The session is told the same
        // thing and remaps; a probe that did not would read one buffer while
        // believing it was reading another.
        if primary.fresh {
            buffers.remove(&primary.fb_id);
        }

        // Taken out of the table for the length of the frame, as the session
        // does it: a captured frame owns its backing, and this mapping
        // outlives the frame.
        let mapped = match buffers.remove(&primary.fb_id) {
            Some(mapped) => mapped,
            None => {
                let length = primary.stride as usize * primary.height as usize;
                let Some(descriptor) = device.buffer(primary.fb_id) else {
                    continue;
                };
                MappedBuffer::map(descriptor, length)
                    .map_err(|error| format!("the desktop would not map: {error}"))?
            }
        };

        if let Some(rects) = damage.as_deref() {
            report.trusted += 1;
            let area: u64 = rects
                .iter()
                .map(|rect| u64::from(rect.width) * u64::from(rect.height))
                .sum();
            coverage += area as f64 / f64::from(primary.width * primary.height);
        }

        // Read once into a copy, because the two submissions must see the
        // same pixels. The compositor goes on painting into the buffers it
        // cycles through, so two reads of the same mapping a millisecond apart
        // are not promised to agree -- and a difference from that would look
        // exactly like damage that lied.
        let pixels = mapped.read(<[u8]>::to_vec);
        buffers.insert(primary.fb_id, mapped);

        let mut frame = CapturedFrame {
            sequence: u64::from(report.frames),
            width: primary.width,
            height: primary.height,
            stride: primary.stride,
            format: PixelFormat::Xrgb8888,
            damage,
            backing: Backing::Owned(pixels),
        };

        trusting
            .as_mut()
            .expect("a pipeline was built with the geometry")
            .submit_frame(&frame)
            .map_err(|error| format!("the desktop was refused: {error}"))?;

        let reported = frame.damage.clone();
        if report.uncovered_at.is_none()
            && let (Some(previous), Some(rects), Backing::Owned(pixels)) =
                (before.as_deref(), reported.as_deref(), &frame.backing)
            && let Some(box_of) = uncovered(previous, pixels, primary.width, rects)
        {
            report.uncovered_at = Some(report.frames);
            report.uncovered_box = Some(box_of);
            report.uncovered_damage = reported.clone();
        }
        if let Backing::Owned(pixels) = &frame.backing {
            before = Some(pixels.clone());
        }

        frame.damage = None;
        comparing
            .as_mut()
            .expect("a pipeline was built with the geometry")
            .submit_frame(&frame)
            .map_err(|error| format!("the desktop was refused: {error}"))?;

        let started = Instant::now();
        report.trusting_bytes += drain(
            trusting.as_mut().expect("built above"),
            trusting_view.as_mut().expect("built above"),
            &limits,
            &mut tail,
        )?;
        trusting_nanos += started.elapsed().as_nanos();

        let started = Instant::now();
        report.comparing_bytes += drain(
            comparing.as_mut().expect("built above"),
            comparing_view.as_mut().expect("built above"),
            &limits,
            &mut tail,
        )?;
        comparing_nanos += started.elapsed().as_nanos();

        if report.identical {
            let left = trusting_view.as_ref().expect("built above").frame();
            let right = comparing_view.as_ref().expect("built above").frame();
            if let Some(box_of) = difference(left, right, primary.width) {
                report.identical = false;
                report.diverged_at = Some(report.frames);
                report.diverged_box = Some(box_of);
                report.diverged_damage = reported;
            }
        }

        report.frames += 1;
    }

    report.stalled = report.frames < frames;
    if report.trusted > 0 {
        report.mean_coverage = coverage / f64::from(report.trusted);
    }
    report.trusting_ms = trusting_nanos as f64 / f64::from(report.frames.max(1)) / 1e6;
    report.comparing_ms = comparing_nanos as f64 / f64::from(report.frames.max(1)) / 1e6;

    Ok(report)
}

/// Drains one pipeline into its viewer, and says how many bytes it wrote.
fn drain(
    pipeline: &mut Pipeline,
    view: &mut Decoder,
    limits: &Limits,
    tail: &mut Vec<u8>,
) -> Result<usize, String> {
    tail.clear();
    while pipeline
        .write_next(tail, limits)
        .map_err(|error| format!("a record would not be written: {error}"))?
    {}

    let written = tail.len();
    let mut reader = tail.as_slice();
    let mut payload = Vec::new();
    while let Ok(header) = record::read(&mut reader, limits, &mut payload) {
        if header.message_type == v1::FrameRecord::Keyframe as u16 {
            view.apply_keyframe(&payload)
                .map_err(|error| format!("a keyframe would not decode: {error}"))?;
        } else if header.message_type == v1::FrameRecord::TileDelta as u16 {
            view.apply_delta(&payload)
                .map_err(|error| format!("a delta would not decode: {error}"))?;
        }
    }

    Ok(written)
}

/// Reads the desktop and reports what the coherency bracket costs.
///
/// # Errors
///
/// The message the card, a snapshot or a mapping failed with, including the
/// case where no `vmlord_drm` card exists -- which is every machine that is not
/// a VMLord guest.
pub fn run<I: IntoIterator<Item = String>>(arguments: I) -> Result<(), String> {
    let Arguments { reads, damage } = parse(arguments)?;

    let mut device = Device::find(DRIVER, Path::new(DRM_CLASS), Path::new(DRM_DEVICES))
        .map_err(|error: io::Error| format!("the card could not be opened: {error}"))?
        .ok_or_else(|| format!("no card is driven by {DRIVER}; this is not a VMLord guest"))?;
    let snapshot = device
        .snapshot()
        .map_err(|error| format!("the planes could not be read: {error} (try sudo)"))?;

    let planes = snapshot.planes.clone();
    let mut reports = Vec::new();
    for plane in &planes {
        let length = plane.stride as usize * plane.height as usize;
        let Some(descriptor) = device.buffer(plane.fb_id) else {
            continue;
        };
        let mapped = MappedBuffer::map(descriptor, length)
            .map_err(|error| format!("a framebuffer would not map: {error}"))?;
        reports.push(measure(&mapped, plane.kind, plane, reads));
    }

    if reports.is_empty() {
        return Err("no plane has a framebuffer attached; is anything on screen?".to_owned());
    }

    println!(
        "{reads} reads per plane, generations {}\n",
        if snapshot.generation_supported {
            "supported"
        } else {
            "absent"
        }
    );
    println!(
        "{:<9}{:>12}{:>9}{:>11}{:>12}{:>11}{:>12}",
        "plane", "size", "stride", "bytes", "synced ms", "bare ms", "sync-only"
    );
    for report in reports {
        println!(
            "{:<9}{:>12}{:>9}{:>11}{:>12.3}{:>11.3}{:>12.3}",
            report.kind,
            format!("{}x{}", report.width, report.height),
            report.stride,
            report.bytes,
            report.mean_synced_ms,
            report.mean_bare_ms,
            report.mean_sync_only_ms,
        );
    }

    let primary = planes
        .iter()
        .find(|plane| plane.kind == PlaneKind::Primary)
        .ok_or_else(|| "no primary plane; is anything on screen?".to_owned())?;
    let cursor_plane = planes.iter().find(|plane| plane.kind == PlaneKind::Cursor);

    println!("\n{reads} passes of the live desktop through the pipeline\n");
    println!(
        "{:<9}{:>12}{:>11}{:>12}{:>13}{:>16}",
        "cursor", "submit ms", "drain ms", "bytes", "round trips", "pointer drawn"
    );
    for stream in [true, false] {
        let pass = through_pipeline(&device, primary, cursor_plane, reads, stream)?;
        println!(
            "{:<9}{:>12.3}{:>11.3}{:>12}{:>13}{:>16}",
            pass.cursor,
            pass.mean_submit_ms,
            pass.mean_drain_ms,
            pass.bytes,
            pass.round_trips,
            pass.pointer_changed_pixels,
        );
    }

    if damage == 0 {
        return Ok(());
    }

    println!("\n{damage} commits encoded twice: trusting the compositor's damage, and comparing\n");
    let report = verify_damage(&mut device, damage)?;
    println!(
        "{:<9}{:>13}{:>11}{:>13}{:>12}{:>13}{:>12}{:>11}",
        "damage",
        "with damage",
        "coverage",
        "trusting ms",
        "bytes",
        "comparing ms",
        "bytes",
        "identical"
    );
    println!(
        "{:<9}{:>13}{:>11}{:>13.3}{:>12}{:>13.3}{:>12}{:>11}",
        format!("{} frames", report.frames),
        report.trusted,
        format!("{:.2}%", report.mean_coverage * 100.0),
        report.trusting_ms,
        report.trusting_bytes,
        report.comparing_ms,
        report.comparing_bytes,
        report.identical,
    );
    if report.stalled {
        println!(
            "\nthe desktop stopped committing after {} frames: move something on screen, or ask \
             for fewer with --damage",
            report.frames
        );
    }
    if let Some(frame) = report.uncovered_at {
        println!(
            "\nat frame {frame} the desktop changed over {:?}, which the compositor's reported \
             {:?} does not cover",
            report.uncovered_box, report.uncovered_damage
        );
    }
    if let Some(frame) = report.diverged_at {
        println!(
            "\nthe two viewers first disagreed at frame {frame}, over {:?}, where the compositor \
             had reported {:?}",
            report.diverged_box, report.diverged_damage
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DAMAGE_FRAMES, READS, parse};

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn the_default_is_two_hundred_reads() {
        assert_eq!(parse(arguments(&[])).unwrap().reads, READS);
        assert_eq!(parse(arguments(&["--reads", "5"])).unwrap().reads, 5);
    }

    #[test]
    fn the_damage_check_can_be_asked_for_by_length_or_skipped() {
        assert_eq!(parse(arguments(&[])).unwrap().damage, DAMAGE_FRAMES);
        assert_eq!(parse(arguments(&["--damage", "40"])).unwrap().damage, 40);
        // Zero is not an error here: a guest with a still screen has no
        // commits to watch, and the read measurements are still worth having.
        assert_eq!(parse(arguments(&["--damage", "0"])).unwrap().damage, 0);
    }

    #[test]
    fn an_unknown_or_empty_argument_is_refused() {
        assert!(parse(arguments(&["--reads", "0"])).is_err());
        assert!(parse(arguments(&["--reads"])).is_err());
        assert!(parse(arguments(&["--nope"])).is_err());
    }
}
