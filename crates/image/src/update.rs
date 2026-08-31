use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::{Agent, Body, http::Response};
use vmlord_core::{
    DownloadPhase, ProgressPublisher, ProgressThrottle, ReleaseManifest, ValidatedUpdate,
};

use crate::error::UpdateDownloadError;

/// The largest GitHub release document accepted into memory.
const MAX_RELEASE_BYTES: u64 = 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/MrUndead1996/vm-lord/releases/latest";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";

static NEXT_PART_ID: AtomicU64 = AtomicU64::new(0);

/// A published release and the manifest that defines its verified installer.
#[derive(Clone, Debug)]
pub struct GitHubRelease {
    pub manifest: ReleaseManifest,
    pub release_notes: String,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    body: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

struct ReleaseSource {
    release_notes: String,
    manifest_url: String,
}

fn parse_release_response(status: u16, body: &[u8]) -> Result<ReleaseSource, UpdateDownloadError> {
    if status != 200 {
        return Err(UpdateDownloadError::UnexpectedStatus { status });
    }
    if body.len() as u64 > MAX_RELEASE_BYTES {
        return Err(UpdateDownloadError::ResponseTooLarge {
            limit: MAX_RELEASE_BYTES,
        });
    }

    let release: ReleaseResponse = serde_json::from_slice(body)
        .map_err(|source| UpdateDownloadError::MalformedRelease(source.to_string()))?;
    if release.draft || release.prerelease {
        return Err(UpdateDownloadError::UnpublishedRelease);
    }
    let manifest_url = release
        .assets
        .into_iter()
        .find(|asset| asset.name == "release-manifest.json")
        .map(|asset| asset.browser_download_url)
        .ok_or(UpdateDownloadError::ManifestAssetMissing)?;

    Ok(ReleaseSource {
        release_notes: release.body,
        manifest_url,
    })
}

fn validate_downloaded_size(actual: u64, expected: u64) -> Result<(), UpdateDownloadError> {
    if actual == expected {
        Ok(())
    } else {
        Err(UpdateDownloadError::SizeMismatch { expected, actual })
    }
}

/// Fetches the current published release and its manifest from GitHub.
pub fn fetch_latest_release() -> Result<GitHubRelease, UpdateDownloadError> {
    fetch_latest_release_from(LATEST_RELEASE_URL)
}

fn fetch_latest_release_from(url: &str) -> Result<GitHubRelease, UpdateDownloadError> {
    let agent = crate::http::build_agent();
    let response = send_github_request(&agent, url)?;
    let release_body = read_response(response, MAX_RELEASE_BYTES, url)?;
    let source = parse_release_response(200, &release_body)?;

    tracing::debug!("fetching the update manifest from {}", source.manifest_url);
    let response = send_github_request(&agent, &source.manifest_url)?;
    let manifest_body = read_response(response, MAX_RELEASE_BYTES, &source.manifest_url)?;
    let manifest = serde_json::from_slice(&manifest_body)
        .map_err(|source| UpdateDownloadError::MalformedRelease(source.to_string()))?;

    Ok(GitHubRelease {
        manifest,
        release_notes: source.release_notes,
    })
}

/// Downloads a validated installer, verifies its bytes, and publishes it into
/// `directory` under its release-specific name.
pub fn fetch_update_installer(
    update: &ValidatedUpdate,
    directory: &Path,
    progress: &ProgressPublisher<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<PathBuf, UpdateDownloadError> {
    fs::create_dir_all(directory).map_err(|source| UpdateDownloadError::Io {
        operation: "create the update directory",
        path: directory.to_path_buf(),
        source,
    })?;

    let (mut part, part_path) = create_unique_part(directory)?;
    let result = download_and_verify_installer(&mut part, &part_path, update, progress, cancel);
    drop(part);

    if let Err(error) = result {
        remove_partial(&part_path);
        return Err(error);
    }

    let final_path = directory.join(format!("VMLord-{}-x86_64-setup.exe", update.version));
    fs::rename(&part_path, &final_path).map_err(|source| UpdateDownloadError::Io {
        operation: "publish the verified installer",
        path: final_path.clone(),
        source,
    })?;
    tracing::info!("verified update installer at {}", final_path.display());
    progress.publish(DownloadPhase::Completed);
    Ok(final_path)
}

fn send_github_request(agent: &Agent, url: &str) -> Result<Response<Body>, UpdateDownloadError> {
    let response = agent
        .get(url)
        .header("Accept", GITHUB_ACCEPT)
        .call()
        .map_err(|source| {
            UpdateDownloadError::Http(format!("requesting {url} failed: {source}"))
        })?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(UpdateDownloadError::UnexpectedStatus { status });
    }
    Ok(response)
}

fn read_response(
    mut response: Response<Body>,
    limit: u64,
    url: &str,
) -> Result<Vec<u8>, UpdateDownloadError> {
    let mut reader = response.body_mut().as_reader();
    let mut body = Vec::new();
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        let read = reader.read(&mut buffer).map_err(|source| {
            UpdateDownloadError::Http(format!("reading {url} failed: {source}"))
        })?;
        if read == 0 {
            return Ok(body);
        }
        if body.len() as u64 + read as u64 > limit {
            return Err(UpdateDownloadError::ResponseTooLarge { limit });
        }
        body.extend_from_slice(&buffer[..read]);
    }
}

fn create_unique_part(directory: &Path) -> Result<(File, PathBuf), UpdateDownloadError> {
    for _ in 0..1024 {
        let part_id = NEXT_PART_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "VMLord-update-{}-{part_id}.part",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(UpdateDownloadError::Io {
                    operation: "create the installer partial download",
                    path,
                    source,
                });
            }
        }
    }

    Err(UpdateDownloadError::Http(
        "could not allocate a unique installer partial download".to_owned(),
    ))
}

fn download_and_verify_installer(
    part: &mut File,
    part_path: &Path,
    update: &ValidatedUpdate,
    progress: &ProgressPublisher<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<(), UpdateDownloadError> {
    let mut throttle = ProgressThrottle::new(progress.clone());
    throttle.publish_now(DownloadPhase::Connecting);
    let agent = crate::http::build_agent();
    let mut response = agent.get(&update.installer.url).call().map_err(|source| {
        UpdateDownloadError::Http(format!(
            "requesting {} failed: {source}",
            update.installer.url
        ))
    })?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(UpdateDownloadError::UnexpectedStatus { status });
    }

    let mut reader = response.body_mut().as_reader();
    let mut buffer = [0u8; READ_CHUNK];
    let mut downloaded = 0u64;
    throttle.publish_now(DownloadPhase::Downloading {
        downloaded,
        total: Some(update.installer.size),
    });
    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::debug!("the update download was cancelled at byte {downloaded}");
            return Err(UpdateDownloadError::Cancelled);
        }
        let read = reader.read(&mut buffer).map_err(|source| {
            UpdateDownloadError::Http(format!("reading {} failed: {source}", update.installer.url))
        })?;
        if read == 0 {
            break;
        }
        let next = downloaded + read as u64;
        if next > update.installer.size {
            return validate_downloaded_size(next, update.installer.size);
        }
        part.write_all(&buffer[..read])
            .map_err(|source| UpdateDownloadError::Io {
                operation: "write the installer partial download",
                path: part_path.to_path_buf(),
                source,
            })?;
        downloaded = next;
        throttle.publish(DownloadPhase::Downloading {
            downloaded,
            total: Some(update.installer.size),
        });
    }
    throttle.publish_now(DownloadPhase::Downloading {
        downloaded,
        total: Some(update.installer.size),
    });
    validate_downloaded_size(downloaded, update.installer.size)?;
    part.sync_all().map_err(|source| UpdateDownloadError::Io {
        operation: "flush the installer partial download",
        path: part_path.to_path_buf(),
        source,
    })?;

    verify_installer(part_path, update, &mut throttle, cancel)
}

fn verify_installer(
    path: &Path,
    update: &ValidatedUpdate,
    progress: &mut ProgressThrottle<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<(), UpdateDownloadError> {
    let mut file = File::open(path).map_err(|source| UpdateDownloadError::Io {
        operation: "open the installer partial download for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; READ_CHUNK];
    let mut hashed = 0u64;
    progress.publish_now(DownloadPhase::Verifying {
        hashed,
        total: update.installer.size,
    });
    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::debug!("the update verification was cancelled at byte {hashed}");
            return Err(UpdateDownloadError::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(|source| UpdateDownloadError::Io {
                operation: "read the installer partial download for hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
        progress.publish(DownloadPhase::Verifying {
            hashed,
            total: update.installer.size,
        });
    }
    progress.publish_now(DownloadPhase::Verifying {
        hashed,
        total: update.installer.size,
    });
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual == update.installer.sha256 {
        Ok(())
    } else {
        Err(UpdateDownloadError::ChecksumMismatch {
            expected: update.installer.sha256.clone(),
            actual,
        })
    }
}

fn remove_partial(path: &Path) {
    if let Err(source) = fs::remove_file(path)
        && source.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            "failed to remove invalid installer partial {}: {source}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        sync::atomic::AtomicBool,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use vmlord_core::{DownloadPhase, InstallerAsset, ProgressPublisher, ValidatedUpdate};

    use super::{
        UpdateDownloadError, fetch_latest_release_from, fetch_update_installer,
        parse_release_response, validate_downloaded_size,
    };

    const RELEASE: &str = r#"{
        "draft": false,
        "prerelease": false,
        "body": "Fixes update downloads.",
        "assets": [{
            "name": "release-manifest.json",
            "browser_download_url": "https://example.test/release-manifest.json"
        }]
    }"#;

    #[test]
    fn a_published_release_exposes_its_notes_and_manifest_asset() {
        let release = parse_release_response(200, RELEASE.as_bytes()).unwrap();

        assert_eq!(release.release_notes, "Fixes update downloads.");
        assert_eq!(
            release.manifest_url,
            "https://example.test/release-manifest.json"
        );
    }

    #[test]
    fn drafts_and_prereleases_are_refused() {
        for field in ["draft", "prerelease"] {
            let body = RELEASE.replacen(
                &format!("\"{field}\": false"),
                &format!("\"{field}\": true"),
                1,
            );

            assert!(matches!(
                parse_release_response(200, body.as_bytes()),
                Err(UpdateDownloadError::UnpublishedRelease)
            ));
        }
    }

    #[test]
    fn a_release_without_its_manifest_asset_is_refused() {
        let body = RELEASE.replace("release-manifest.json", "other-file.txt");

        assert!(matches!(
            parse_release_response(200, body.as_bytes()),
            Err(UpdateDownloadError::ManifestAssetMissing)
        ));
    }

    #[test]
    fn an_oversized_release_document_is_refused() {
        let body = vec![b' '; 1_048_577];

        assert!(matches!(
            parse_release_response(200, &body),
            Err(UpdateDownloadError::ResponseTooLarge { .. })
        ));
    }

    #[test]
    fn a_non_successful_release_response_is_refused() {
        assert!(matches!(
            parse_release_response(503, RELEASE.as_bytes()),
            Err(UpdateDownloadError::UnexpectedStatus { status: 503 })
        ));
    }

    #[test]
    fn bytes_past_the_declared_installer_size_are_refused() {
        let error = validate_downloaded_size(101, 100).unwrap_err();

        assert!(matches!(error, UpdateDownloadError::SizeMismatch { .. }));
    }

    #[test]
    fn local_release_and_manifest_responses_are_combined() {
        let server = serve(|base| {
            let release = format!(
                r#"{{"draft":false,"prerelease":false,"body":"Release notes","assets":[{{"name":"release-manifest.json","browser_download_url":"{base}/release-manifest.json"}}]}}"#
            );
            vec![
                response(200, release.as_bytes()),
                response(200, manifest().as_bytes()),
            ]
        });

        let release = fetch_latest_release_from(&format!("{}/latest", server.base)).unwrap();

        assert_eq!(release.release_notes, "Release notes");
        assert_eq!(release.manifest.version.to_string(), "0.2.0");
        server.finish();
    }

    #[test]
    fn an_interrupted_installer_body_is_refused_and_its_partial_is_removed() {
        let server = serve(|_| vec![response_with_length(200, b"short", 10)]);
        let directory = temporary_directory("interrupted");

        let error = fetch_update_installer(
            &update(format!("{}/installer.exe", server.base), 10, "a".repeat(64)),
            &directory,
            &ProgressPublisher::<DownloadPhase>::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(matches!(error, UpdateDownloadError::Http(_)));
        assert!(directory.read_dir().unwrap().next().is_none());
        server.finish();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_removes_the_partial_installer() {
        let server = serve(|_| vec![response(200, b"installer")]);
        let directory = temporary_directory("cancelled");
        let cancel = AtomicBool::new(true);

        let error = fetch_update_installer(
            &update(format!("{}/installer.exe", server.base), 9, "a".repeat(64)),
            &directory,
            &ProgressPublisher::<DownloadPhase>::default(),
            &cancel,
        )
        .unwrap_err();

        assert!(matches!(error, UpdateDownloadError::Cancelled));
        assert!(directory.read_dir().unwrap().next().is_none());
        server.finish();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_hash_mismatch_removes_the_partial_installer() {
        let server = serve(|_| vec![response(200, b"installer")]);
        let directory = temporary_directory("hash-mismatch");

        let error = fetch_update_installer(
            &update(format!("{}/installer.exe", server.base), 9, "a".repeat(64)),
            &directory,
            &ProgressPublisher::<DownloadPhase>::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            UpdateDownloadError::ChecksumMismatch { .. }
        ));
        assert!(directory.read_dir().unwrap().next().is_none());
        server.finish();
        fs::remove_dir_all(directory).unwrap();
    }

    fn manifest() -> String {
        r#"{
            "schema": 1,
            "version": "0.2.0",
            "installer": {
                "url": "https://github.com/MrUndead1996/vm-lord/releases/download/v0.2.0/VMLord-0.2.0-x86_64-setup.exe",
                "size": 9,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }"#
        .to_owned()
    }

    fn update(url: String, size: u64, sha256: String) -> ValidatedUpdate {
        ValidatedUpdate {
            version: "0.2.0".parse().unwrap(),
            installer: InstallerAsset { url, size, sha256 },
        }
    }

    fn temporary_directory(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vmlord-update-{tag}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    struct FixtureServer {
        base: String,
        worker: thread::JoinHandle<()>,
    }

    impl FixtureServer {
        fn finish(self) {
            self.worker.join().unwrap();
        }
    }

    fn serve(build: impl FnOnce(&str) -> Vec<Vec<u8>> + Send + 'static) -> FixtureServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let responses = build(&base);
        let worker = thread::spawn(move || {
            for response in responses {
                let (mut connection, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _request = connection.read(&mut request).unwrap();
                connection.write_all(&response).unwrap();
                connection.flush().unwrap();
            }
        });

        FixtureServer { base, worker }
    }

    fn response(status: u16, body: &[u8]) -> Vec<u8> {
        response_with_length(status, body, body.len() as u64)
    }

    fn response_with_length(status: u16, body: &[u8], length: u64) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }
}
