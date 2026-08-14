use crate::{CatalogEntry, PayloadError, Sha256Digest};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, TryLockError},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const DOWNLOAD_GLOBAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_RECEIVE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadProgress {
    Connecting,
    Downloading { downloaded: u64, total: u64 },
    Verifying { hashed: u64, total: u64 },
    Extracting { files: u64, total: u64 },
    Staging { files: u64, total: u64 },
    Ready,
}

pub(crate) struct LockedArchive {
    file: File,
    path: PathBuf,
    entry: CatalogEntry,
}

fn production_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(DOWNLOAD_GLOBAL_TIMEOUT))
        .timeout_connect(Some(DOWNLOAD_CONNECT_TIMEOUT))
        .timeout_recv_body(Some(DOWNLOAD_RECEIVE_TIMEOUT))
        .build()
        .into()
}

impl LockedArchive {
    pub(crate) fn acquire(cache_root: &Path, entry: &CatalogEntry) -> Result<Self, PayloadError> {
        fs::create_dir_all(cache_root).map_err(|error| {
            PayloadError::io("create GPU payload cache", cache_root.into(), error)
        })?;
        let path = cache_root.join(format!("{}.zip.part", entry.archive_sha256()));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .map_err(|error| PayloadError::io("open partial archive", path.clone(), error))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file,
                path,
                entry: entry.clone(),
            }),
            Err(TryLockError::WouldBlock) => Err(PayloadError::AlreadyInProgress { path }),
            Err(TryLockError::Error(error)) => {
                Err(PayloadError::io("lock partial archive", path, error))
            }
        }
    }

    pub(crate) fn download(
        &mut self,
        progress: &dyn Fn(PayloadProgress),
        cancel: &AtomicBool,
    ) -> Result<(), PayloadError> {
        let url = self.entry.archive_url().to_owned();
        self.download_with(&production_agent(), &url, progress, cancel)
    }

    #[cfg(test)]
    fn download_from_loopback(
        &mut self,
        url: &str,
        progress: &dyn Fn(PayloadProgress),
        cancel: &AtomicBool,
    ) -> Result<(), PayloadError> {
        self.download_with(&ureq::Agent::new_with_defaults(), url, progress, cancel)
    }

    fn download_with(
        &mut self,
        agent: &ureq::Agent,
        url: &str,
        progress: &dyn Fn(PayloadProgress),
        cancel: &AtomicBool,
    ) -> Result<(), PayloadError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        self.file.set_len(0).map_err(|error| {
            PayloadError::io("truncate partial archive", self.path.clone(), error)
        })?;
        self.file.seek(SeekFrom::Start(0)).map_err(|error| {
            PayloadError::io("rewind partial archive", self.path.clone(), error)
        })?;
        progress(PayloadProgress::Connecting);
        let body = agent
            .get(url)
            .call()
            .map_err(|error| {
                PayloadError::Http(format!(
                    "could not download payload {}: {error}",
                    self.entry.payload_id()
                ))
            })?
            .into_body();
        let mut body = body.into_reader();
        let mut buffer = [0; 64 * 1024];
        let mut downloaded = 0;
        while downloaded < self.entry.archive_size() {
            if cancel.load(Ordering::Relaxed) {
                return Err(PayloadError::Cancelled);
            }
            let remaining =
                (self.entry.archive_size() - downloaded).min(buffer.len() as u64) as usize;
            let count = body.read(&mut buffer[..remaining]).map_err(|error| {
                PayloadError::Http(format!(
                    "could not read payload {}: {error}",
                    self.entry.payload_id()
                ))
            })?;
            if count == 0 {
                break;
            }
            self.file.write_all(&buffer[..count]).map_err(|error| {
                PayloadError::io("write partial archive", self.path.clone(), error)
            })?;
            downloaded += count as u64;
            progress(PayloadProgress::Downloading {
                downloaded,
                total: self.entry.archive_size(),
            });
        }
        let mut extra = [0; 1];
        if downloaded == self.entry.archive_size()
            && body.read(&mut extra).map_err(|error| {
                PayloadError::Http(format!(
                    "could not read payload {}: {error}",
                    self.entry.payload_id()
                ))
            })? > 0
        {
            downloaded += 1;
        }
        if downloaded != self.entry.archive_size() {
            return Err(PayloadError::ArchiveSizeMismatch {
                expected: self.entry.archive_size(),
                actual: downloaded,
            });
        }
        self.file
            .sync_all()
            .map_err(|error| PayloadError::io("flush partial archive", self.path.clone(), error))?;
        Ok(())
    }

    pub(crate) fn verify(
        &mut self,
        progress: &dyn Fn(PayloadProgress),
        cancel: &AtomicBool,
    ) -> Result<(), PayloadError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| {
            PayloadError::io("rewind partial archive", self.path.clone(), error)
        })?;
        progress(PayloadProgress::Verifying {
            hashed: 0,
            total: self.entry.archive_size(),
        });
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut hashed = 0_u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(PayloadError::Cancelled);
            }
            let count = self.file.read(&mut buffer).map_err(|error| {
                PayloadError::io("hash partial archive", self.path.clone(), error)
            })?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            hashed = hashed.checked_add(count as u64).ok_or_else(|| {
                PayloadError::ArchiveSizeMismatch {
                    expected: self.entry.archive_size(),
                    actual: u64::MAX,
                }
            })?;
            progress(PayloadProgress::Verifying {
                hashed,
                total: self.entry.archive_size(),
            });
        }
        let actual = Sha256Digest::from_bytes(hash.finalize().into())?;
        if actual != *self.entry.archive_sha256() {
            self.file.set_len(0).map_err(|error| {
                PayloadError::io(
                    "truncate mismatched partial archive",
                    self.path.clone(),
                    error,
                )
            })?;
            return Err(PayloadError::DigestMismatch {
                subject: format!("payload {} archive", self.entry.payload_id()),
                expected: self.entry.archive_sha256().clone(),
                actual,
            });
        }
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        DOWNLOAD_CONNECT_TIMEOUT, DOWNLOAD_GLOBAL_TIMEOUT, DOWNLOAD_RECEIVE_TIMEOUT, LockedArchive,
        production_agent,
    };
    use crate::{PayloadCatalog, PayloadError, PayloadProgress, Sha256Digest};

    struct FixtureServer {
        url: String,
    }

    impl FixtureServer {
        fn start(body: &'static [u8]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/payload.zip", listener.local_addr().unwrap());
            thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    read_request(&stream);
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).unwrap();
                    stream.write_all(body).unwrap();
                }
            });
            Self { url }
        }

        fn url(&self) -> &str {
            &self.url
        }
    }

    fn read_request(stream: &TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
        }
    }

    fn entry() -> crate::CatalogEntry {
        PayloadCatalog::from_json(br#"{"schema_version":1,"entries":[{"payload_id":"test","target":{"distribution":"ubuntu","release":"26.04","architecture":"amd64","kernel_release":"test","payload_abi":1},"archive_url":"https://example.test/payload.zip","archive_size":7,"expanded_size_limit":8,"file_count_limit":1,"archive_sha256":"0000000000000000000000000000000000000000000000000000000000000000","payload_manifest_sha256":"0000000000000000000000000000000000000000000000000000000000000000","required_renderers":["d3d12-gallium"],"mesa_policy":"bundled","vmlord_revision":"14794180686c2fb6307fbe359c359bec765249f3","builder_version":"vmlord-gpu-payload 1","sources":[{"url":"https://github.com/example/source","commit":"14794180686c2fb6307fbe359c359bec765249f3","version":"1"}],"licenses":[{"spdx":"MIT","path":"licenses/MIT.txt"}]}]}"#).unwrap().entries()[0].clone()
    }

    fn entry_for(bytes: &[u8]) -> crate::CatalogEntry {
        let digest = Sha256Digest::hash_reader(bytes).unwrap();
        let catalog = serde_json::json!({
            "schema_version": 1,
            "entries": [{
                "payload_id": "test",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "test",
                    "payload_abi": 1
                },
                "archive_url": "https://example.test/payload.zip",
                "archive_size": bytes.len(),
                "expanded_size_limit": bytes.len(),
                "file_count_limit": 1,
                "archive_sha256": digest,
                "payload_manifest_sha256": digest,
                "required_renderers": ["d3d12-gallium"],
                "mesa_policy": "bundled",
                "vmlord_revision": "14794180686c2fb6307fbe359c359bec765249f3",
                "builder_version": "vmlord-gpu-payload 1",
                "sources": [{
                    "url": "https://github.com/example/source",
                    "commit": "14794180686c2fb6307fbe359c359bec765249f3",
                    "version": "1"
                }],
                "licenses": [{"spdx": "MIT", "path": "licenses/MIT.txt"}]
            }]
        });
        PayloadCatalog::from_json(&serde_json::to_vec(&catalog).unwrap())
            .unwrap()
            .entries()[0]
            .clone()
    }

    #[test]
    fn a_second_preparer_is_refused_while_the_digest_lock_is_held() {
        let root = std::env::temp_dir().join(format!(
            "vmlord-gpu-payload-lock-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let entry = entry();
        let first = LockedArchive::acquire(&root, &entry).unwrap();
        assert!(matches!(
            LockedArchive::acquire(&root, &entry),
            Err(PayloadError::AlreadyInProgress { .. })
        ));
        drop(first);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_is_checked_before_connecting() {
        let root = std::env::temp_dir().join(format!(
            "vmlord-gpu-payload-cancel-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut archive = LockedArchive::acquire(&root, &entry()).unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            archive.download(&|_| {}, &cancelled),
            Err(PayloadError::Cancelled)
        ));
        drop(archive);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_verification_checks_cancellation_between_64_kib_chunks() {
        let root = std::env::temp_dir().join(format!(
            "vmlord-gpu-payload-verify-cancel-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bytes = vec![b'x'; 2 * 64 * 1024];
        let mut archive = LockedArchive::acquire(&root, &entry_for(&bytes)).unwrap();
        archive.file.write_all(&bytes).unwrap();
        let cancelled = AtomicBool::new(false);
        let events = Mutex::new(Vec::new());
        let progress = |event| {
            events.lock().unwrap().push(event);
            if event
                == (PayloadProgress::Verifying {
                    hashed: 64 * 1024,
                    total: bytes.len() as u64,
                })
            {
                cancelled.store(true, Ordering::Relaxed);
            }
        };

        assert!(matches!(
            archive.verify(&progress, &cancelled),
            Err(PayloadError::Cancelled)
        ));
        assert_eq!(
            *events.lock().unwrap(),
            [
                PayloadProgress::Verifying {
                    hashed: 0,
                    total: bytes.len() as u64,
                },
                PayloadProgress::Verifying {
                    hashed: 64 * 1024,
                    total: bytes.len() as u64,
                }
            ]
        );
        drop(archive);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_verification_reports_each_hashed_chunk_through_completion() {
        let root = std::env::temp_dir().join(format!(
            "vmlord-gpu-payload-verify-progress-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bytes = vec![b'x'; 64 * 1024 + 1];
        let mut archive = LockedArchive::acquire(&root, &entry_for(&bytes)).unwrap();
        archive.file.write_all(&bytes).unwrap();
        let events = Mutex::new(Vec::new());

        archive
            .verify(
                &|event| events.lock().unwrap().push(event),
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            [
                PayloadProgress::Verifying {
                    hashed: 0,
                    total: bytes.len() as u64,
                },
                PayloadProgress::Verifying {
                    hashed: 64 * 1024,
                    total: bytes.len() as u64,
                },
                PayloadProgress::Verifying {
                    hashed: bytes.len() as u64,
                    total: bytes.len() as u64,
                }
            ]
        );
        drop(archive);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_production_agent_refuses_plain_http() {
        let server = FixtureServer::start(b"archive");

        let error = production_agent().get(server.url()).call().unwrap_err();

        assert!(matches!(error, ureq::Error::RequireHttpsOnly(_)));
    }

    #[test]
    fn the_production_agent_bounds_the_whole_download_and_network_phases() {
        let agent = production_agent();
        let config = agent.config();
        let timeouts = config.timeouts();

        assert!(config.https_only());
        assert_eq!(timeouts.global, Some(DOWNLOAD_GLOBAL_TIMEOUT));
        assert_eq!(timeouts.connect, Some(DOWNLOAD_CONNECT_TIMEOUT));
        assert_eq!(timeouts.recv_body, Some(DOWNLOAD_RECEIVE_TIMEOUT));
    }

    #[test]
    fn loopback_downloads_still_exercise_the_real_bounded_reader() {
        let root = std::env::temp_dir().join(format!(
            "vmlord-gpu-payload-loopback-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let server = FixtureServer::start(b"archive!");
        let mut archive = LockedArchive::acquire(&root, &entry()).unwrap();

        let error = archive
            .download_from_loopback(server.url(), &|_| {}, &AtomicBool::new(false))
            .unwrap_err();

        assert!(matches!(
            error,
            PayloadError::ArchiveSizeMismatch {
                expected: 7,
                actual: 8
            }
        ));
        drop(archive);
        fs::remove_dir_all(root).unwrap();
    }
}
