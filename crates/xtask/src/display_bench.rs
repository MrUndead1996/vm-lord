//! `cargo display-bench`: what the desktop codec costs on each of its scenes.
//!
//! The output is a table on stdout, not a stored baseline. It exists to answer
//! whether a third method such as LZ4 would earn its place, and to catch an
//! obviously wrong default tile size -- not to gate anything on a number that
//! depends on the machine it was measured on.

use std::time::Instant;

use vmlord_display_codec::{
    Decoder, Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, TileSize,
    scenes::{Generator, Scene},
};

/// The resolution the table is measured at.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// What one scene came to.
struct Report {
    scene: &'static str,
    frames: u32,
    keyframes: u32,
    mean_bytes: f64,
    /// The mean over deltas alone. The all-frames mean of a quiet scene is
    /// mostly its one keyframe divided by the frame count, which says nothing
    /// about what steady state costs.
    mean_delta_bytes: f64,
    worst_bytes: u64,
    ratio: f64,
    mean_encode_ms: f64,
    worst_encode_ms: f64,
    mean_decode_ms: f64,
}

/// What `display-bench` was asked for.
struct Arguments {
    frames: u32,
    tile: TileSize,
}

/// Reads `--frames` and `--tile`.
fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Arguments, String> {
    let mut values = arguments.into_iter();
    let mut parsed = Arguments {
        frames: 300,
        tile: TileSize::ThirtyTwo,
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
            _ => return Err(format!("unknown argument `{flag}`")),
        }
    }

    if parsed.frames == 0 {
        return Err("--frames wants at least one frame".to_owned());
    }

    Ok(parsed)
}

/// The geometry the table is measured at.
fn geometry(tile: TileSize) -> Geometry {
    Geometry::new(WIDTH, HEIGHT, tile, PixelFormat::Bgra8888)
        .expect("1920x1080 is a geometry the codec accepts")
}

/// Drives one scene through the codec, verifying the round trip as it goes.
fn measure(scene: Scene, geometry: Geometry, frames: u32) -> Result<Report, String> {
    let mut generator = Generator::new(scene, geometry, 0x5EED);
    let mut encoder = Encoder::new(EncoderConfig::new(geometry));
    let mut decoder = Decoder::new(geometry);
    let stride = geometry.width() as usize * 4;

    let mut keyframes = 0;
    let mut deltas = 0u32;
    let mut delta_bytes = 0u64;
    let mut total_bytes = 0u64;
    let mut worst_bytes = 0u64;
    let mut encode_nanos = 0u128;
    let mut worst_encode_nanos = 0u128;
    let mut decode_nanos = 0u128;

    for _ in 0..frames {
        let pixels = generator.next_frame().to_vec();
        encoder
            .submit(
                Frame {
                    pixels: &pixels,
                    stride,
                },
                None,
            )
            .map_err(|error| format!("{scene:?}: {error}"))?;

        let started = Instant::now();
        let payload = encoder.next_payload().map(|payload| match payload {
            Payload::Keyframe(bytes) => (true, bytes.to_vec()),
            Payload::TileDelta(bytes) => (false, bytes.to_vec()),
            _ => unreachable!("no cursor is submitted here"),
        });
        let elapsed = started.elapsed().as_nanos();

        encode_nanos += elapsed;
        worst_encode_nanos = worst_encode_nanos.max(elapsed);

        let Some((keyframe, bytes)) = payload else {
            continue;
        };

        total_bytes += bytes.len() as u64;
        worst_bytes = worst_bytes.max(bytes.len() as u64);
        keyframes += u32::from(keyframe);
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
        applied.map_err(|error| format!("{scene:?}: {error}"))?;

        if decoder.frame() != pixels.as_slice() {
            return Err(format!(
                "{scene:?}: the decoded frame is not the captured one"
            ));
        }
    }

    let frames_f = f64::from(frames);
    let raw = geometry.frame_bytes() as f64 * frames_f;

    Ok(Report {
        scene: scene.name(),
        frames,
        keyframes,
        mean_bytes: total_bytes as f64 / frames_f,
        mean_delta_bytes: if deltas == 0 {
            0.0
        } else {
            delta_bytes as f64 / f64::from(deltas)
        },
        worst_bytes,
        ratio: if total_bytes == 0 {
            f64::INFINITY
        } else {
            raw / total_bytes as f64
        },
        mean_encode_ms: encode_nanos as f64 / frames_f / 1e6,
        worst_encode_ms: worst_encode_nanos as f64 / 1e6,
        mean_decode_ms: decode_nanos as f64 / frames_f / 1e6,
    })
}

/// Measures every scene and prints the table.
pub(crate) fn run<I: IntoIterator<Item = String>>(arguments: I) -> Result<(), String> {
    let arguments = parse(arguments)?;
    let geometry = geometry(arguments.tile);

    println!(
        "{}x{}, tile {}, {} frames per scene\n",
        geometry.width(),
        geometry.height(),
        geometry.tile_size().as_pixels(),
        arguments.frames
    );
    println!(
        "{:<18}{:>10}{:>12}{:>13}{:>9}{:>12}{:>10}{:>11}{:>9}",
        "scene",
        "keyframes",
        "mean bytes",
        "mean delta",
        "ratio",
        "worst bytes",
        "enc ms",
        "worst enc",
        "dec ms"
    );

    for scene in Scene::ALL {
        let report = measure(scene, geometry, arguments.frames)?;
        println!(
            "{:<18}{:>4}/{:<5}{:>12.0}{:>13.0}{:>9.1}{:>12}{:>10.2}{:>11.2}{:>9.2}",
            report.scene,
            report.keyframes,
            report.frames,
            report.mean_bytes,
            report.mean_delta_bytes,
            report.ratio,
            report.worst_bytes,
            report.mean_encode_ms,
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
        let report = measure(Scene::Typing, geometry(TileSize::ThirtyTwo), 4).unwrap();

        assert_eq!(report.frames, 4);
        assert!(report.keyframes >= 1);
        assert!(report.mean_bytes > 0.0);
    }

    #[test]
    fn a_static_desktop_sends_only_its_keyframe() {
        let report = measure(Scene::StaticDesktop, geometry(TileSize::ThirtyTwo), 4).unwrap();

        assert_eq!(report.keyframes, 1);
    }

    #[test]
    fn the_defaults_are_three_hundred_frames_at_tile_thirty_two() {
        let parsed = parse(arguments(&[])).unwrap();

        assert_eq!(parsed.frames, 300);
        assert_eq!(parsed.tile, TileSize::ThirtyTwo);
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        assert!(parse(arguments(&["--nope"])).is_err());
        assert!(parse(arguments(&["--tile", "48"])).is_err());
        assert!(parse(arguments(&["--frames", "0"])).is_err());
        assert!(parse(arguments(&["--frames"])).is_err());
    }
}
