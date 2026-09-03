//! `cargo display-bench`: what the desktop codec costs on each of its scenes.
//!
//! The output is a table on stdout, not a stored baseline. It exists to answer
//! whether a third method such as LZ4 would earn its place, and to catch an
//! obviously wrong default tile size -- not to gate anything on a number that
//! depends on the machine it was measured on.

use std::{fs, path::PathBuf, time::Instant};

use vmlord_display_codec::{
    Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, TileSize,
    scenes::{Generator, Scene},
};

/// The resolution the table is measured at unless `--width` and `--height`
/// name another one.
///
/// A default rather than the only choice: what a refresh costs is what a
/// frame costs, and a frame's cost is its pixel count. A cap on the refresh
/// the viewer publishes is read off this table at more than one size.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// How many frames of each scene are measured unless `--frames` names another
/// count. A recording is measured over its own length instead.
const FRAMES: u32 = 300;

/// Where a run's frames come from.
///
/// The synthetic scenes settle the codec's own decisions; a file of captured
/// frames is the only thing that says what a desktop costs. See
/// `vmlord-display-guest-probe --record`, which writes one.
enum Source {
    Synthetic(Generator),
    /// Frames read from a file, `width * height * 4` bytes each and packed.
    ///
    /// Held whole rather than streamed: a run measures the same frames more
    /// than once, and reading them back off a disk between passes would put
    /// the disk in the timings.
    Recorded {
        frames: Vec<Vec<u8>>,
        next: usize,
    },
}

impl Source {
    /// The next frame, cycling once a recording runs out.
    fn next_frame(&mut self) -> Vec<u8> {
        match self {
            Self::Synthetic(generator) => generator.next_frame().to_vec(),
            Self::Recorded { frames, next } => {
                let frame = frames[*next % frames.len()].clone();
                *next += 1;
                frame
            }
        }
    }
}

/// Reads a recording into frames of `bytes` each.
fn recorded(path: &PathBuf, bytes: usize) -> Result<Vec<Vec<u8>>, String> {
    let raw =
        fs::read(path).map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    if raw.len() < bytes {
        return Err(format!(
            "{} holds {} bytes, which is less than one {bytes}-byte frame: name the size the \
             recording was made at with --width and --height",
            path.display(),
            raw.len()
        ));
    }

    Ok(raw.chunks_exact(bytes).map(<[u8]>::to_vec).collect())
}

/// What one scene came to.
struct Report {
    scene: String,
    frames: u32,
    keyframes: u32,
    mean_bytes: f64,
    /// The mean over deltas alone. The all-frames mean of a quiet scene is
    /// mostly its one keyframe divided by the frame count, which says nothing
    /// about what steady state costs.
    mean_delta_bytes: f64,
    /// What the first keyframe cost. The number the protective interval used
    /// to pay every time it came round, and the one the synthetic scenes get
    /// wrong: a desktop's wallpaper is a photograph, and run-length coding
    /// does nothing to a photograph.
    keyframe_bytes: u64,
    worst_bytes: u64,
    ratio: f64,
    mean_submit_ms: f64,
    mean_encode_ms: f64,
    mean_frame_ms: f64,
    worst_encode_ms: f64,
    mean_decode_ms: f64,
}

/// What `display-bench` was asked for.
struct Arguments {
    /// The run length, or `None` to take the default -- which for a recording
    /// is the number of frames it holds.
    frames: Option<u32>,
    tile: TileSize,
    width: u32,
    height: u32,
    /// The one scene to measure, or every scene when unnamed.
    ///
    /// Scenes share a process, and a scene that touched eight megabytes leaves
    /// the caches to the next one. Naming a single scene is how a number gets
    /// compared against the same number from another build.
    scene: Option<Scene>,
    /// A file of captured frames to measure instead of the scenes.
    raw: Option<PathBuf>,
}

/// Reads `--frames`, `--tile`, `--width`, `--height` and `--scene`.
fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Arguments, String> {
    let mut values = arguments.into_iter();
    let mut parsed = Arguments {
        frames: None,
        tile: TileSize::ThirtyTwo,
        width: WIDTH,
        height: HEIGHT,
        scene: None,
        raw: None,
    };

    while let Some(flag) = values.next() {
        let mut value = || {
            values
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--frames" => {
                parsed.frames = Some(
                    value()?
                        .parse()
                        .map_err(|_| "--frames wants a number".to_owned())?,
                );
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
            "--raw" => parsed.raw = Some(PathBuf::from(value()?)),
            _ => return Err(format!("unknown argument `{flag}`")),
        }
    }

    if parsed.frames == Some(0) {
        return Err("--frames wants at least one frame".to_owned());
    }
    if parsed.raw.is_some() && parsed.scene.is_some() {
        return Err("--raw measures a recording, so it cannot also name a scene".to_owned());
    }

    Ok(parsed)
}

/// The geometry the table is measured at.
///
/// # Errors
///
/// The message the codec refused the size with, which is what a `--width` of
/// nothing looks like.
fn geometry(width: u32, height: u32, tile: TileSize) -> Result<Geometry, String> {
    Geometry::new(width, height, tile, PixelFormat::Bgra8888)
        .map_err(|error| format!("{width}x{height} is not a geometry the codec accepts: {error}"))
}

/// Drives one source through the codec, verifying the round trip as it goes.
fn measure(
    name: &str,
    mut source: Source,
    geometry: Geometry,
    frames: u32,
) -> Result<Report, String> {
    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    let mut decoder = Decoder::new(geometry);
    let stride = geometry.width() as usize * 4;

    let mut keyframes = 0;
    let mut keyframe_bytes = 0u64;
    let mut deltas = 0u32;
    let mut delta_bytes = 0u64;
    let mut total_bytes = 0u64;
    let mut worst_bytes = 0u64;
    let mut submit_nanos = 0u128;
    let mut encode_nanos = 0u128;
    let mut frame_nanos = 0u128;
    let mut worst_encode_nanos = 0u128;
    let mut decode_nanos = 0u128;

    for _ in 0..frames {
        let pixels = source.next_frame();
        let frame_started = Instant::now();
        let started = Instant::now();
        encoder
            .submit(
                Frame {
                    pixels: &pixels,
                    stride,
                },
                None,
            )
            .map_err(|error| format!("{name}: {error}"))?;
        submit_nanos += started.elapsed().as_nanos();

        let started = Instant::now();
        let payload = encoder.next_payload().map(|payload| match payload {
            Payload::Keyframe(bytes) => (true, bytes.to_vec()),
            Payload::TileDelta(bytes) => (false, bytes.to_vec()),
            _ => unreachable!("no cursor is submitted here"),
        });
        let elapsed = started.elapsed().as_nanos();

        encode_nanos += elapsed;
        frame_nanos += frame_started.elapsed().as_nanos();
        worst_encode_nanos = worst_encode_nanos.max(elapsed);

        let Some((keyframe, bytes)) = payload else {
            continue;
        };

        total_bytes += bytes.len() as u64;
        worst_bytes = worst_bytes.max(bytes.len() as u64);
        keyframes += u32::from(keyframe);
        if keyframe && keyframe_bytes == 0 {
            keyframe_bytes = bytes.len() as u64;
        }
        if !keyframe {
            deltas += 1;
            delta_bytes += bytes.len() as u64;
        }

        let started = Instant::now();
        let applied = if keyframe {
            decoder.apply_keyframe(&bytes)
        } else {
            decoder.apply_delta(&bytes)
        };
        decode_nanos += started.elapsed().as_nanos();
        applied.map_err(|error| format!("{name}: {error}"))?;

        if decoder.frame() != pixels.as_slice() {
            return Err(format!("{name}: the decoded frame is not the captured one"));
        }
    }

    let frames_f = f64::from(frames);
    let raw = geometry.frame_bytes() as f64 * frames_f;

    Ok(Report {
        scene: name.to_owned(),
        frames,
        keyframes,
        mean_bytes: total_bytes as f64 / frames_f,
        mean_delta_bytes: if deltas == 0 {
            0.0
        } else {
            delta_bytes as f64 / f64::from(deltas)
        },
        keyframe_bytes,
        worst_bytes,
        ratio: if total_bytes == 0 {
            f64::INFINITY
        } else {
            raw / total_bytes as f64
        },
        mean_submit_ms: submit_nanos as f64 / frames_f / 1e6,
        mean_encode_ms: encode_nanos as f64 / frames_f / 1e6,
        mean_frame_ms: frame_nanos as f64 / frames_f / 1e6,
        worst_encode_ms: worst_encode_nanos as f64 / 1e6,
        mean_decode_ms: decode_nanos as f64 / frames_f / 1e6,
    })
}

/// Measures every scene and prints the table.
pub(crate) fn run<I: IntoIterator<Item = String>>(arguments: I) -> Result<(), String> {
    let arguments = parse(arguments)?;
    let geometry = geometry(arguments.width, arguments.height, arguments.tile)?;

    let mut reports = Vec::new();
    if let Some(path) = &arguments.raw {
        let frames = recorded(path, geometry.frame_bytes())?;
        // A recording's own length is the run length unless one was named:
        // asking for three hundred frames of a thirty-frame capture would
        // measure the same thirty frames ten times over and call it a scene.
        let count = arguments.frames.unwrap_or(frames.len() as u32);
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        println!(
            "{}x{}, tile {}, {count} frames of {} captured\n",
            geometry.width(),
            geometry.height(),
            geometry.tile_size().as_pixels(),
            frames.len(),
        );
        reports.push(measure(
            &name,
            Source::Recorded { frames, next: 0 },
            geometry,
            count,
        )?);
    } else {
        let count = arguments.frames.unwrap_or(FRAMES);
        println!(
            "{}x{}, tile {}, {count} frames per scene\n",
            geometry.width(),
            geometry.height(),
            geometry.tile_size().as_pixels(),
        );
        let scenes: Vec<Scene> = arguments
            .scene
            .map_or_else(|| Scene::ALL.to_vec(), |scene| vec![scene]);
        for scene in scenes {
            reports.push(measure(
                scene.name(),
                Source::Synthetic(Generator::new(scene, geometry, 0x5EED)),
                geometry,
                count,
            )?);
        }
    }

    println!(
        "{:<18}{:>10}{:>14}{:>12}{:>13}{:>9}{:>12}{:>11}{:>10}{:>10}{:>11}{:>9}",
        "scene",
        "keyframes",
        "keyframe bytes",
        "mean bytes",
        "mean delta",
        "ratio",
        "worst bytes",
        "submit ms",
        "enc ms",
        "frame ms",
        "worst enc",
        "dec ms"
    );

    for report in reports {
        println!(
            "{:<18}{:>4}/{:<5}{:>14}{:>12.0}{:>13.0}{:>9.1}{:>12}{:>11.2}{:>10.2}{:>10.2}{:>11.2}\
             {:>9.2}",
            report.scene,
            report.keyframes,
            report.frames,
            report.keyframe_bytes,
            report.mean_bytes,
            report.mean_delta_bytes,
            report.ratio,
            report.worst_bytes,
            report.mean_submit_ms,
            report.mean_encode_ms,
            report.mean_frame_ms,
            report.worst_encode_ms,
            report.mean_decode_ms,
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

    #[test]
    fn a_short_run_of_a_scene_round_trips_and_reports() {
        let geometry = geometry(WIDTH, HEIGHT, TileSize::ThirtyTwo).unwrap();
        let report = measure(
            "typing",
            Source::Synthetic(Generator::new(Scene::Typing, geometry, 0x5EED)),
            geometry,
            4,
        )
        .unwrap();

        assert_eq!(report.frames, 4);
        assert!(report.keyframes >= 1);
        assert!(report.mean_bytes > 0.0);
        assert!(report.mean_submit_ms > 0.0);
        assert!(report.mean_frame_ms >= report.mean_submit_ms);
        assert!(report.mean_frame_ms >= report.mean_encode_ms);
    }

    #[test]
    fn a_static_desktop_sends_only_its_keyframe() {
        let geometry = geometry(WIDTH, HEIGHT, TileSize::ThirtyTwo).unwrap();
        let report = measure(
            "static desktop",
            Source::Synthetic(Generator::new(Scene::StaticDesktop, geometry, 0x5EED)),
            geometry,
            4,
        )
        .unwrap();

        assert_eq!(report.keyframes, 1);
        assert!(report.keyframe_bytes > 0);
    }

    #[test]
    fn the_defaults_are_an_unnamed_run_length_at_tile_thirty_two() {
        let parsed = parse(arguments(&[])).unwrap();

        // `None` rather than `FRAMES`, because a recording's own length is
        // the default for a recording.
        assert_eq!(parsed.frames, None);
        assert_eq!(parsed.tile, TileSize::ThirtyTwo);
        assert_eq!((parsed.width, parsed.height), (WIDTH, HEIGHT));
    }

    #[test]
    fn a_size_can_be_named_and_one_the_codec_refuses_is_reported() {
        let parsed = parse(arguments(&["--width", "1280", "--height", "720"])).unwrap();

        assert_eq!((parsed.width, parsed.height), (1280, 720));
        assert!(geometry(parsed.width, parsed.height, parsed.tile).is_ok());
        // A partial tile is a geometry the codec does accept, so the size that
        // proves the error path is one with no pixels in it.
        assert!(geometry(0, 720, TileSize::ThirtyTwo).is_err());
    }

    #[test]
    fn one_scene_can_be_named_so_the_others_do_not_share_its_caches() {
        let parsed = parse(arguments(&["--scene", "typing"])).unwrap();

        assert_eq!(parsed.scene, Some(Scene::Typing));
        assert_eq!(
            parse(arguments(&[])).unwrap().scene,
            None,
            "naming no scene measures all of them"
        );
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        assert!(parse(arguments(&["--scene", "solitaire"])).is_err());
        assert!(parse(arguments(&["--nope"])).is_err());
        assert!(parse(arguments(&["--tile", "48"])).is_err());
        assert!(parse(arguments(&["--frames", "0"])).is_err());
        assert!(parse(arguments(&["--raw", "x.raw", "--scene", "typing"])).is_err());
        assert!(parse(arguments(&["--frames"])).is_err());
    }
}
