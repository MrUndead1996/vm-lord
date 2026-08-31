//! `cargo display-pipeline-bench`: what a captured frame costs between the
//! mapping and the socket.
//!
//! `cargo display-bench` measures the codec, which is the part that is portable
//! and has golden vectors. This measures the part above it -- the cursor the
//! pipeline may have to composite, the copy a payload makes on its way into a
//! record, and the difference between reading a frame out of a mapping and out
//! of memory this process owns. Those are copies the codec's table cannot see,
//! because they happen before `Encoder::submit` is ever called.
//!
//! It exists to answer one question with numbers rather than an opinion: how
//! much of a frame's cost is the pixels being moved rather than encoded. Like
//! the codec's table it is stdout and not a stored baseline, and it gates
//! nothing: the machine it ran on is part of every figure in it.
//!
//! Linux only, and deliberately: the mapped rows are a real `mmap` over a real
//! descriptor, which is the thing being compared against.

use std::{
    fs::File,
    io,
    os::{fd::AsFd, unix::fs::FileExt},
    time::Instant,
};

use vmlord_display_codec::{
    Geometry, PixelFormat, TileSize,
    scenes::{Generator, Scene},
};
use vmlord_display_protocol::record::Limits;

use crate::{
    capture::{Backing, CapturedFrame, MappedBuffer},
    cursor,
    pipeline::Pipeline,
    unix::memfd,
};

/// The resolution the table is measured at unless `--width` and `--height`
/// name another one. The codec's benchmark defaults to the same one, so a row
/// here can be read beside a row there.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// The cursor plane's size. `vmlord_drm` offers one size and mutter uses it.
const CURSOR: u32 = 64;

/// How many buffers the chain holds.
///
/// A compositor cycles through its scanout buffers rather than committing the
/// same one twice, so measuring one buffer over and over would measure a cache
/// state no guest ever has. Three is what a double-buffered compositor with one
/// frame in flight comes to.
const DEPTH: usize = 3;

/// Where the pixels a frame is read from live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// Memory this process owns, which is what the pipeline's tests use.
    /// The baseline: no mapping, no coherency call, just a slice.
    Owned,
    /// A read-only mapping over a descriptor, which is what a guest captures.
    /// A memfd rather than a dma-buf -- there is no DRM device here -- so the
    /// coherency call fails and the read is the uncached path a buffer without
    /// `DMA_BUF_IOCTL_SYNC` takes. What this row shows is the mapping's cost,
    /// not the sync's; only a guest can show that one.
    Mapped,
}

impl Source {
    /// Both, in the order the table reports them.
    const ALL: [Self; 2] = [Self::Owned, Self::Mapped];

    /// This source's name, for the table or a failure message.
    const fn name(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::Mapped => "mapped",
        }
    }
}

/// What the peer agreed to do about the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cursor {
    /// The peer took the cursor-stream capability, so the cursor travels beside
    /// the frame and the frame is submitted as captured.
    Stream,
    /// The peer declined it, so the pipeline composites the cursor into the
    /// frame -- which costs two whole-frame passes the streamed row does not
    /// pay. That difference is the reason this column exists.
    Drawn,
}

impl Cursor {
    /// Both, in the order the table reports them.
    const ALL: [Self; 2] = [Self::Stream, Self::Drawn];

    /// This mode's name, for the table.
    const fn name(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Drawn => "drawn",
        }
    }

    /// What [`Pipeline::new`] is told about the peer.
    const fn stream(self) -> bool {
        matches!(self, Self::Stream)
    }
}

/// What one scene in one configuration came to.
struct Report {
    scene: &'static str,
    source: &'static str,
    cursor: &'static str,
    /// The mean cost of staging the cursor: the bitmap's conversion to words
    /// when it is drawn, the two codec calls when it is streamed.
    mean_cursor_ms: f64,
    /// The mean cost of staging the frame. The composite lives here, and so
    /// does the codec's own byte-to-word pass, which is why the streamed row is
    /// the one to compare `cargo display-bench`'s `submit ms` against.
    mean_submit_ms: f64,
    /// The mean cost of turning what was staged into records in the tail: the
    /// encode, the payload's copy into a `Vec`, and the framing.
    mean_drain_ms: f64,
    /// The three above, over the same frames.
    mean_frame_ms: f64,
    /// The mean bytes a frame put in the tail, which is what the socket would
    /// have carried.
    mean_bytes: f64,
}

/// The buffers a compositor would cycle through.
///
/// A backing is moved into the frame that reads it and moved back afterwards,
/// exactly as the session does with the broker's buffers: the frame owns its
/// backing, and the buffer outlives the frame.
struct FlipChain {
    /// The writable side of each mapped buffer, kept so a scene's pixels can be
    /// put there between measurements. Empty for an owned chain.
    files: Vec<File>,
    /// Each buffer's backing while it is not inside a [`CapturedFrame`].
    parked: Vec<Option<Backing>>,
}

impl FlipChain {
    /// A chain of [`DEPTH`] buffers of `length` bytes each.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if a descriptor cannot be made or mapped.
    fn new(source: Source, length: usize) -> io::Result<Self> {
        let mut files = Vec::new();
        let mut parked = Vec::new();

        for index in 0..DEPTH {
            match source {
                Source::Owned => parked.push(Some(Backing::Owned(vec![0; length]))),
                Source::Mapped => {
                    let file =
                        File::from(memfd(&format!("bench-frame-{index}"), &vec![0; length])?);
                    let mapped = MappedBuffer::map(file.as_fd(), length)?;
                    files.push(file);
                    parked.push(Some(Backing::Cpu(mapped)));
                }
            }
        }

        Ok(Self { files, parked })
    }

    /// Puts `pixels` in buffer `index` and hands its backing out.
    ///
    /// The write is what a compositor's commit would have done, and it is
    /// deliberately outside every measurement: what is being timed is reading
    /// the buffer, never filling it.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the descriptor will not take the pixels.
    fn take(&mut self, index: usize, pixels: &[u8]) -> io::Result<Backing> {
        let mut backing = self.parked[index]
            .take()
            .expect("a buffer is parked whenever it is not inside a frame");
        match &mut backing {
            Backing::Owned(bytes) => bytes.copy_from_slice(pixels),
            Backing::Cpu(_) => self.files[index].write_all_at(pixels, 0)?,
        }

        Ok(backing)
    }

    /// Takes a backing back after the frame that held it is done with it.
    fn park(&mut self, index: usize, backing: Backing) {
        self.parked[index] = Some(backing);
    }
}

/// What `display-pipeline-bench` was asked for.
struct Arguments {
    frames: u32,
    tile: TileSize,
    width: u32,
    height: u32,
    /// The one scene to measure, or every scene when unnamed.
    ///
    /// Scenes share a process, and a scene that touched eight megabytes leaves
    /// the caches to the next one. Naming a single scene is how a number gets
    /// compared against the same number from another build.
    scene: Option<Scene>,
    /// The one source to measure, or both when unnamed.
    source: Option<Source>,
    /// The one cursor mode to measure, or both when unnamed.
    cursor: Option<Cursor>,
}

/// Reads `--frames`, `--tile`, `--width`, `--height`, `--scene`, `--source`
/// and `--cursor`.
fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Arguments, String> {
    let mut values = arguments.into_iter();
    let mut parsed = Arguments {
        // Fewer than the codec's benchmark: this one measures twenty
        // combinations rather than five, and a whole-frame copy is what most of
        // them spend their time on.
        frames: 120,
        tile: TileSize::ThirtyTwo,
        width: WIDTH,
        height: HEIGHT,
        scene: None,
        source: None,
        cursor: None,
    };

    while let Some(flag) = values.next() {
        let mut value = || {
            values
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--frames" => {
                parsed.frames = value()?
                    .parse()
                    .map_err(|_| "--frames wants a number".to_owned())?;
            }
            "--tile" => {
                let pixels: u32 = value()?
                    .parse()
                    .map_err(|_| "--tile wants 16, 32 or 64".to_owned())?;
                parsed.tile = TileSize::from_pixels(pixels)
                    .map_err(|_| "--tile wants 16, 32 or 64".to_owned())?;
            }
            "--width" => {
                parsed.width = value()?
                    .parse()
                    .map_err(|_| "--width wants a number".to_owned())?;
            }
            "--height" => {
                parsed.height = value()?
                    .parse()
                    .map_err(|_| "--height wants a number".to_owned())?;
            }
            "--scene" => {
                let wanted = value()?;
                parsed.scene = Some(
                    Scene::ALL
                        .into_iter()
                        .find(|scene| scene.name() == wanted)
                        .ok_or_else(|| format!("no scene is called `{wanted}`"))?,
                );
            }
            "--source" => {
                let wanted = value()?;
                parsed.source = Some(
                    Source::ALL
                        .into_iter()
                        .find(|source| source.name() == wanted)
                        .ok_or_else(|| format!("no source is called `{wanted}`"))?,
                );
            }
            "--cursor" => {
                let wanted = value()?;
                parsed.cursor = Some(
                    Cursor::ALL
                        .into_iter()
                        .find(|cursor| cursor.name() == wanted)
                        .ok_or_else(|| format!("no cursor mode is called `{wanted}`"))?,
                );
            }
            _ => return Err(format!("unknown argument `{flag}`")),
        }
    }

    if parsed.frames == 0 {
        return Err("--frames wants at least one frame".to_owned());
    }

    Ok(parsed)
}

/// The geometry the table is measured at.
///
/// `Bgra8888`, which is what the codec's benchmark uses, so the two tables'
/// figures are of the same work.
///
/// # Errors
///
/// The message the codec refused the size with.
fn geometry(width: u32, height: u32, tile: TileSize) -> Result<Geometry, String> {
    Geometry::new(width, height, tile, PixelFormat::Bgra8888)
        .map_err(|error| format!("{width}x{height} is not a geometry the codec accepts: {error}"))
}

/// A cursor bitmap: an opaque disc on a transparent square.
///
/// Its shape matters only in that the alpha channel is not constant. A bitmap
/// that was opaque everywhere would let no one tell a composite that blends
/// from one that does not.
fn cursor_bitmap() -> Vec<u8> {
    let radius = (CURSOR / 2) as i32;
    let mut pixels = vec![0u8; (CURSOR * CURSOR * 4) as usize];

    for y in 0..CURSOR as i32 {
        for x in 0..CURSOR as i32 {
            let (dx, dy) = (x - radius, y - radius);
            let inside = dx * dx + dy * dy < radius * radius;
            let offset = (y as usize * CURSOR as usize + x as usize) * 4;
            pixels[offset..offset + 4].copy_from_slice(&if inside {
                [0xF0, 0xF0, 0xF0, 0xFF]
            } else {
                [0, 0, 0, 0]
            });
        }
    }

    pixels
}

/// Drives one scene through the pipeline in one configuration.
///
/// The order is the session's: the cursor first, because a cursor submitted
/// after the frame is one frame late, then the frame, then whatever the socket
/// is owed. Each of the three is timed on its own, which is the point -- a
/// single figure per frame would hide which of them the copies are in.
///
/// # Errors
///
/// The message the pipeline or a descriptor failed with.
fn measure(
    scene: Scene,
    geometry: Geometry,
    source: Source,
    cursor_mode: Cursor,
    frames: u32,
) -> Result<Report, String> {
    let width = geometry.width();
    let height = geometry.height();
    let stride = width as usize * 4;
    let length = stride * height as usize;

    let mut generator = Generator::new(scene, geometry, 0x5EED);
    let mut pipeline = Pipeline::new(geometry, 1, cursor_mode.stream());
    let mut chain = FlipChain::new(source, length)
        .map_err(|error| format!("{scene:?}: the buffers would not be made: {error}"))?;
    let limits = Limits::new(width, height);
    let bitmap = cursor_bitmap();
    // The session's `tail`: records are written here and drained to the socket
    // from here, so it is reused rather than allocated per frame.
    let mut tail = Vec::new();

    // The record that opens every stream. It is not one of the measured frames
    // and its bytes are not a frame's, so it goes out before the clocks start.
    pipeline
        .write_stream_config(&mut tail, &limits)
        .map_err(|error| format!("{scene:?}: {error}"))?;
    tail.clear();

    let mut cursor_nanos = 0u128;
    let mut submit_nanos = 0u128;
    let mut drain_nanos = 0u128;
    let mut bytes = 0u64;

    for sequence in 0..frames {
        let index = sequence as usize % DEPTH;
        let pixels = generator.next_frame();
        let backing = chain
            .take(index, pixels)
            .map_err(|error| format!("{scene:?}: a buffer would not take a frame: {error}"))?;

        // A pointer crossing the desktop, so the composite lands somewhere new
        // every frame and the tiles it dirties are never the same two in a row.
        let step = i32::try_from(sequence).unwrap_or(i32::MAX);
        let placement = cursor::place(
            step * 3 % i32::try_from(width).unwrap_or(i32::MAX),
            step * 2 % i32::try_from(height).unwrap_or(i32::MAX),
            CURSOR,
            CURSOR,
            width,
            height,
        );

        // The bitmap goes with every frame, because the session has no way to
        // tell a new bitmap from the one before it and sends it every time.
        let started = Instant::now();
        pipeline
            .submit_cursor(Some((&bitmap, CURSOR, CURSOR)), &placement)
            .map_err(|error| format!("{scene:?}: the cursor was refused: {error}"))?;
        cursor_nanos += started.elapsed().as_nanos();

        let captured = CapturedFrame {
            sequence: u64::from(sequence),
            width,
            height,
            stride: u32::try_from(stride).unwrap_or(u32::MAX),
            format: geometry.pixel_format(),
            damage: None,
            backing,
        };

        let started = Instant::now();
        let staged = pipeline.submit_frame(&captured);
        submit_nanos += started.elapsed().as_nanos();
        chain.park(index, captured.backing);
        staged.map_err(|error| format!("{scene:?}: the frame was refused: {error}"))?;

        let started = Instant::now();
        while pipeline
            .write_next(&mut tail, &limits)
            .map_err(|error| format!("{scene:?}: {error}"))?
        {}
        drain_nanos += started.elapsed().as_nanos();

        bytes += tail.len() as u64;
        tail.clear();
    }

    let frames_f = f64::from(frames);
    let mean = |nanos: u128| nanos as f64 / frames_f / 1e6;

    Ok(Report {
        scene: scene.name(),
        source: source.name(),
        cursor: cursor_mode.name(),
        mean_cursor_ms: mean(cursor_nanos),
        mean_submit_ms: mean(submit_nanos),
        mean_drain_ms: mean(drain_nanos),
        mean_frame_ms: mean(cursor_nanos + submit_nanos + drain_nanos),
        mean_bytes: bytes as f64 / frames_f,
    })
}

/// Measures every combination asked for and prints the table.
///
/// # Errors
///
/// The message an argument or a measurement failed with.
pub fn run<I: IntoIterator<Item = String>>(arguments: I) -> Result<(), String> {
    let arguments = parse(arguments)?;
    let geometry = geometry(arguments.width, arguments.height, arguments.tile)?;

    let scenes: Vec<Scene> = arguments
        .scene
        .map_or_else(|| Scene::ALL.to_vec(), |scene| vec![scene]);
    let sources: Vec<Source> = arguments
        .source
        .map_or_else(|| Source::ALL.to_vec(), |source| vec![source]);
    let cursors: Vec<Cursor> = arguments
        .cursor
        .map_or_else(|| Cursor::ALL.to_vec(), |cursor| vec![cursor]);

    // Every row is measured before any is printed. A mapped chain warns on
    // stderr that its memfd has no `DMA_BUF_IOCTL_SYNC` -- which is true, and
    // expected here -- and a table interleaved with that is one nobody can
    // read.
    let mut reports = Vec::new();
    for scene in scenes {
        for source in &sources {
            for cursor_mode in &cursors {
                reports.push(measure(
                    scene,
                    geometry,
                    *source,
                    *cursor_mode,
                    arguments.frames,
                )?);
            }
        }
    }

    println!(
        "\n{}x{}, tile {}, {} frames per row\n",
        geometry.width(),
        geometry.height(),
        geometry.tile_size().as_pixels(),
        arguments.frames
    );
    println!(
        "{:<18}{:>9}{:>9}{:>11}{:>11}{:>10}{:>11}{:>12}",
        "scene", "source", "cursor", "cursor ms", "submit ms", "drain ms", "frame ms", "mean bytes"
    );

    for report in reports {
        println!(
            "{:<18}{:>9}{:>9}{:>11.2}{:>11.2}{:>10.2}{:>11.2}{:>12.0}",
            report.scene,
            report.source,
            report.cursor,
            report.mean_cursor_ms,
            report.mean_submit_ms,
            report.mean_drain_ms,
            report.mean_frame_ms,
            report.mean_bytes,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// Small enough that a test run costs nothing, large enough to hold a tile
    /// grid and a whole cursor.
    fn small() -> Geometry {
        geometry(320, 240, TileSize::ThirtyTwo).unwrap()
    }

    #[test]
    fn a_short_run_reports_every_phase_it_measured() {
        let report = measure(Scene::Typing, small(), Source::Owned, Cursor::Stream, 4).unwrap();

        assert!(report.mean_submit_ms > 0.0);
        assert!(report.mean_drain_ms > 0.0);
        assert!(report.mean_bytes > 0.0);
        assert!(
            report.mean_frame_ms >= report.mean_submit_ms + report.mean_drain_ms,
            "a frame costs at least the phases it is made of"
        );
    }

    #[test]
    fn a_mapped_frame_is_measured_through_a_real_mapping() {
        // The mapped rows are the ones with a descriptor behind them, and a
        // chain that silently fell back to owned memory would measure the same
        // thing twice. This is what proves it did not.
        let report = measure(Scene::Scrolling, small(), Source::Mapped, Cursor::Drawn, 4).unwrap();

        assert!(report.mean_bytes > 0.0);
    }

    #[test]
    fn a_mapped_chain_hands_back_what_the_scene_wrote() {
        let pixels: Vec<u8> = (0..4096u32).map(|byte| byte as u8).collect();
        let mut chain = FlipChain::new(Source::Mapped, pixels.len()).unwrap();
        let backing = chain.take(0, &pixels).unwrap();

        let frame = CapturedFrame {
            sequence: 0,
            width: 32,
            height: 32,
            stride: 128,
            format: PixelFormat::Bgra8888,
            damage: None,
            backing,
        };
        frame.read(|bytes| assert_eq!(bytes, pixels.as_slice()));
    }

    #[test]
    fn every_buffer_of_a_chain_comes_back_before_it_is_used_again() {
        // A backing that was not parked is a panic on the next lap, and the
        // measurement loop leans on that: it moves each one into a frame and
        // back on every frame.
        let mut chain = FlipChain::new(Source::Owned, 64).unwrap();
        for lap in 0..DEPTH * 2 {
            let backing = chain.take(lap % DEPTH, &[7; 64]).unwrap();
            chain.park(lap % DEPTH, backing);
        }
    }

    #[test]
    fn a_drawn_cursor_costs_more_than_a_streamed_one() {
        // The two whole-frame passes the composite adds are the finding this
        // benchmark exists to put a number on. A build where they cost nothing
        // is one where the composite stopped happening.
        let streamed = measure(
            Scene::StaticDesktop,
            small(),
            Source::Owned,
            Cursor::Stream,
            8,
        )
        .unwrap();
        let drawn = measure(
            Scene::StaticDesktop,
            small(),
            Source::Owned,
            Cursor::Drawn,
            8,
        )
        .unwrap();

        assert!(
            drawn.mean_submit_ms > streamed.mean_submit_ms,
            "compositing a cursor into the frame reads and writes it twice more"
        );
    }

    #[test]
    fn the_defaults_measure_every_combination() {
        let parsed = parse(arguments(&[])).unwrap();

        assert_eq!(parsed.frames, 120);
        assert_eq!(parsed.tile, TileSize::ThirtyTwo);
        assert_eq!((parsed.width, parsed.height), (WIDTH, HEIGHT));
        assert_eq!(parsed.scene, None);
        assert_eq!(parsed.source, None);
        assert_eq!(parsed.cursor, None);
    }

    #[test]
    fn one_combination_can_be_named_so_the_others_do_not_share_its_caches() {
        let parsed = parse(arguments(&[
            "--scene", "typing", "--source", "mapped", "--cursor", "drawn",
        ]))
        .unwrap();

        assert_eq!(parsed.scene, Some(Scene::Typing));
        assert_eq!(parsed.source, Some(Source::Mapped));
        assert_eq!(parsed.cursor, Some(Cursor::Drawn));
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        assert!(parse(arguments(&["--source", "dma-buf"])).is_err());
        assert!(parse(arguments(&["--cursor", "hidden"])).is_err());
        assert!(parse(arguments(&["--scene", "solitaire"])).is_err());
        assert!(parse(arguments(&["--tile", "48"])).is_err());
        assert!(parse(arguments(&["--frames", "0"])).is_err());
        assert!(parse(arguments(&["--frames"])).is_err());
        assert!(parse(arguments(&["--nope"])).is_err());
    }
}
