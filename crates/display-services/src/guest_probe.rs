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

use std::{io, path::Path, time::Instant};

use vmlord_display_codec::{Decoder, Geometry, PixelFormat, TileSize};
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

/// Reads `--reads`.
fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<u32, String> {
    let mut values = arguments.into_iter();
    let mut reads = READS;

    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--reads" => {
                reads = values
                    .next()
                    .ok_or_else(|| "missing value for --reads".to_owned())?
                    .parse()
                    .map_err(|_| "--reads wants a number".to_owned())?;
            }
            _ => return Err(format!("unknown argument `{flag}`")),
        }
    }

    if reads == 0 {
        return Err("--reads wants at least one read".to_owned());
    }

    Ok(reads)
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

/// Reads the desktop and reports what the coherency bracket costs.
///
/// # Errors
///
/// The message the card, a snapshot or a mapping failed with, including the
/// case where no `vmlord_drm` card exists -- which is every machine that is not
/// a VMLord guest.
pub fn run<I: IntoIterator<Item = String>>(arguments: I) -> Result<(), String> {
    let reads = parse(arguments)?;

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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{READS, parse};

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn the_default_is_two_hundred_reads() {
        assert_eq!(parse(arguments(&[])).unwrap(), READS);
        assert_eq!(parse(arguments(&["--reads", "5"])).unwrap(), 5);
    }

    #[test]
    fn an_unknown_or_empty_argument_is_refused() {
        assert!(parse(arguments(&["--reads", "0"])).is_err());
        assert!(parse(arguments(&["--reads"])).is_err());
        assert!(parse(arguments(&["--nope"])).is_err());
    }
}
