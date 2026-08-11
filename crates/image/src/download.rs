//! Fetching a distribution image into the cache, intact.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use ureq::{Agent, Body, http::Response};
use vmlord_core::{DownloadPhase, ProgressPublisher, ProgressThrottle};

use crate::{
    cache::{cache_file_name, file_checksum, normalized_checksum},
    error::{DownloadError, io_error},
    http::{ResumeOutcome, build_agent, parse_content_range, resume_decision},
    part::PartFile,
};

/// How much of the body is read between two cancellation checks.
const READ_CHUNK: usize = 64 * 1024;

/// An image to fetch, and where to keep it.
pub struct ImageDownloadRequest<'a> {
    pub url: &'a str,
    /// The SHA256 the image must hash to, in hex.
    pub expected_sha256: &'a str,
    pub cache_directory: &'a Path,
}

/// Returns the path of the verified image in the cache, downloading it first if
/// the cache has not got it.
///
/// The checksum is verified on every call, cache hit included. That is the only
/// thing that distinguishes a complete image from one an interrupted download
/// left truncated, and it costs seconds against re-fetching hundreds of
/// megabytes.
pub fn fetch_image(
    request: ImageDownloadRequest<'_>,
    progress: &ProgressPublisher<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<PathBuf, DownloadError> {
    let expected =
        normalized_checksum(request.expected_sha256).inspect_err(|error| log::error!("{error}"))?;
    let mut throttle = ProgressThrottle::new(progress.clone());

    fs::create_dir_all(request.cache_directory)
        .map_err(io_error("create the image cache", request.cache_directory))?;

    let file_name = cache_file_name(request.url, &expected);
    let final_path = request.cache_directory.join(&file_name);
    let part_path = request.cache_directory.join(format!("{file_name}.part"));

    if final_path.is_file() && cache_hit(&final_path, &expected, &mut throttle, cancel)? {
        log::info!("using the cached image at {}", final_path.display());
        throttle.publish_now(DownloadPhase::Completed);
        return Ok(final_path);
    }

    let mut part = PartFile::open_locked(part_path).inspect_err(|error| log::error!("{error}"))?;

    log::info!("downloading {} into {}", request.url, final_path.display());
    download_into(&mut part, request.url, &mut throttle, cancel)
        .inspect_err(|error| log::error!("failed to download {}: {error}", request.url))?;

    let actual = part.checksum(&mut throttle, cancel)?;
    if actual != expected {
        // Truncate rather than delete: the file is open and holds the lock, and
        // its name is worth keeping for the next attempt.
        part.truncate()?;
        let error = DownloadError::ChecksumMismatch { expected, actual };
        log::error!("{error}");
        return Err(error);
    }

    publish_into_cache(&mut part, &final_path)?;
    log::info!("image ready at {}", final_path.display());
    throttle.publish_now(DownloadPhase::Completed);
    Ok(final_path)
}

/// Whether the cached file is the image it claims to be.
///
/// A file that is not is deleted here, so the caller can go straight on to
/// downloading it again.
fn cache_hit(
    final_path: &Path,
    expected: &str,
    throttle: &mut ProgressThrottle<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<bool, DownloadError> {
    let actual = file_checksum(final_path, throttle, cancel)?;
    if actual == expected {
        return Ok(true);
    }

    log::warn!(
        "the cached image at {} hashes to {actual} instead of {expected}; \
         discarding it and downloading again",
        final_path.display()
    );
    match fs::remove_file(final_path) {
        Ok(()) => Ok(false),
        // Another downloader reached the same conclusion first.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(DownloadError::Io {
            operation: "discard the corrupt cached image",
            path: final_path.to_path_buf(),
            source,
        }),
    }
}

/// Moves the verified partial file into its final name.
///
/// If the final file already exists, another downloader won the race. Both
/// files have the same content by construction -- the name is the checksum --
/// so the winner's copy is adopted rather than overwritten. `fs::rename` on
/// Windows replaces the target silently, and replacing a file the importer may
/// have open is exactly what this avoids.
///
/// Renaming a file this process still holds open is fine: Rust opens files with
/// `FILE_SHARE_DELETE` among its share flags, so `MoveFileEx` succeeds. The
/// handle then refers to the renamed file and keeps its lock until `fetch_image`
/// returns and drops it, which is before any caller sees the path.
fn publish_into_cache(part: &mut PartFile, final_path: &Path) -> Result<(), DownloadError> {
    if final_path.exists() {
        log::info!(
            "another download finished {} first; keeping its copy",
            final_path.display()
        );
        return part.truncate();
    }

    fs::rename(part.path(), final_path).map_err(io_error("publish the image", final_path))
}

/// Streams the body into the partial file, resuming where it can.
fn download_into(
    part: &mut PartFile,
    url: &str,
    throttle: &mut ProgressThrottle<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<(), DownloadError> {
    let agent = build_agent();
    throttle.publish_now(DownloadPhase::Connecting);

    let mut from = part.len()?;
    let mut response = send(&agent, url, from)?;

    if from > 0 {
        let content_range = header(&response, "content-range");
        match resume_decision(response.status().as_u16(), content_range.as_deref(), from)? {
            ResumeOutcome::Append { .. } => {
                log::debug!("resuming {url} at byte {from}");
            }
            ResumeOutcome::StartOver => {
                log::warn!(
                    "the server ignored the range request for {url}; downloading from the start"
                );
                part.truncate()?;
                from = 0;
            }
            ResumeOutcome::RangeUnsatisfiable => {
                log::warn!(
                    "the server rejected the range request for {url}; the partial file is stale"
                );
                part.truncate()?;
                from = 0;
                response = send(&agent, url, 0)?;
                let status = response.status().as_u16();
                if status != 200 {
                    return Err(DownloadError::UnexpectedStatus { status });
                }
            }
        }
    } else {
        let status = response.status().as_u16();
        if status != 200 {
            return Err(DownloadError::UnexpectedStatus { status });
        }
    }

    let total = total_length(&response, from);
    part.seek_to_end()?;

    let mut downloaded = from;
    throttle.publish_now(DownloadPhase::Downloading { downloaded, total });
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            log::debug!("the download of {url} was cancelled at byte {downloaded}");
            return Err(DownloadError::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|source| DownloadError::Http(format!("reading {url} failed: {source}")))?;
        if read == 0 {
            break;
        }
        part.write_all(&buffer[..read])?;
        downloaded += read as u64;
        throttle.publish(DownloadPhase::Downloading { downloaded, total });
    }
    throttle.publish_now(DownloadPhase::Downloading { downloaded, total });

    part.sync()?;
    log::debug!("{url} delivered {downloaded} bytes");
    Ok(())
}

/// Issues the request, asking to resume when `from` is past the start.
fn send(agent: &Agent, url: &str, from: u64) -> Result<Response<Body>, DownloadError> {
    let request = agent.get(url);
    let request = if from > 0 {
        request.header("Range", format!("bytes={from}-"))
    } else {
        request
    };
    request
        .call()
        .map_err(|source| DownloadError::Http(format!("requesting {url} failed: {source}")))
}

fn header(response: &Response<Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The full size of the image, when the server said enough to know it.
fn total_length(response: &Response<Body>, from: u64) -> Option<u64> {
    if let Some(range) = header(response, "content-range")
        && let Some((_, _, complete)) = parse_content_range(&range)
    {
        return complete;
    }
    header(response, "content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .map(|length| length + from)
}
