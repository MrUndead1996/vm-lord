//! The files one conversion sends into a copied guest, bound to their hashes.
//!
//! Two things travel to a guest that is about to stop being an AppSandbox one:
//! a fixed program that does the work, and the values that program acts on.
//! Keeping them apart is the whole design. The program is the same bytes for
//! every import and is verified against a manifest before it is trusted; the
//! values -- a user name, a public key, which payload was chosen -- are a JSON
//! document the program reads. Nothing a person named ever becomes part of a
//! command, on either side of the connection.
//!
//! What the bundle carries is decided here and nowhere else: the agent VMLord
//! ships, the agent secret this VM was minted, and the display and GPU payloads
//! the release has for the guest that was actually observed. A release with
//! nothing for that guest is refused before anything is uploaded, because a
//! half-converted guest is worse than one that was never touched.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

use serde::Serialize;
use vmlord_core::RepositoryError;
use vmlord_display_payload::{
    DisplayPayloadCatalog, GuestSelector as DisplayGuestSelector, LOCAL_ARCHIVE_DIRECTORY,
    ProtocolVersionParts,
};
use vmlord_gpu_payload::{
    GuestSelector as GpuGuestSelector, PayloadCatalog, local_archive_path as gpu_archive_path,
};
use vmlord_payload::{Sha256Digest, release};

use super::conversion::{GuestIdentity, SecretText};

/// Where the bundle waits in the guest between the upload and the reboot.
///
/// Under `/var/lib` and not `/tmp`: a conversion resumes after a boot that went
/// wrong, and every step after the upload verifies itself against the manifest
/// that travelled with it. A bundle on a tmpfs would be gone exactly when a
/// resumption needs it.
pub(crate) const GUEST_BUNDLE_DIRECTORY: &str = "/var/lib/vmlord/convert";

/// Where the upload lands before anything roots it.
///
/// A fixed path rather than the bootstrap user's home: the home directory is
/// named after a user VMLord did not choose, and a path built out of that name
/// is the one thing this module exists to avoid.
pub(crate) const GUEST_STAGED_DIRECTORY: &str = "/tmp/vmlord-convert";

/// The name of the guest program, which is also how every step names it.
pub(crate) const GUEST_PROGRAM_NAME: &str = "vmlord-convert";

const MANIFEST_NAME: &str = "manifest.json";
const INPUT_NAME: &str = "input.json";
const AGENT_NAME: &str = "vmlord-agent";
const AGENT_SECRET_NAME: &str = "agent.secret";
const DISPLAY_ARCHIVE_NAME: &str = "display-payload.zip";
const GPU_ARCHIVE_NAME: &str = "gpu-payload.zip";

/// The AppSandbox guest units this conversion stops and disables.
///
/// Exactly the units the source application's own installer enables -- its
/// agent, the three daemons that agent's units order themselves after, and the
/// service its DRM module ships to evict `simpledrm`. A unit missing from this
/// list is a daemon left running against VMLord's; a unit that does not belong
/// on it is somebody else's service stopped for no reason.
const APPSANDBOX_UNITS: [&str; 5] = [
    "appsandbox-agent.service",
    "appsandbox-audio.service",
    "appsandbox-display.service",
    "appsandbox-input.service",
    "asb-evict-simpledrm.service",
];

/// The AppSandbox files this conversion removes once VMLord's own have been
/// validated in their place.
///
/// The binaries the source application installs under `/usr/local/bin`, the
/// units that start them, and the module configuration that makes its DRM
/// driver own the guest's display. Not its DKMS sources: removing those is
/// `dkms remove`'s business, and the program does that by name rather than by
/// deleting a directory tree it guessed at.
const OBSOLETE_APPSANDBOX_PATHS: [&str; 12] = [
    "/usr/local/bin/appsandbox-agent",
    "/usr/local/bin/appsandbox-audio",
    "/usr/local/bin/appsandbox-clipboard",
    "/usr/local/bin/appsandbox-display",
    "/usr/local/bin/appsandbox-input",
    "/etc/systemd/system/appsandbox-agent.service",
    "/etc/systemd/system/appsandbox-audio.service",
    "/etc/systemd/system/appsandbox-display.service",
    "/etc/systemd/system/appsandbox-input.service",
    "/etc/systemd/system/asb-evict-simpledrm.service",
    "/etc/modprobe.d/asb_drm.conf",
    "/etc/modules-load.d/asb_drm.conf",
];

/// The display protocol revision this build speaks, as selection needs it.
fn speaks() -> ProtocolVersionParts {
    ProtocolVersionParts {
        major: vmlord_display_protocol::handshake::CURRENT_VERSION.major,
        minor: vmlord_display_protocol::handshake::CURRENT_VERSION.minor,
    }
}

/// The document the guest program takes every outside value from.
#[derive(Debug, Serialize)]
struct GuestInput<'a> {
    guest_username: &'a str,
    vmlord_public_key: &'a str,
    display_payload_id: &'a str,
    gpu_payload_id: &'a str,
    appsandbox_units: [&'a str; 5],
    obsolete_paths: [&'a str; 12],
}

/// One file the bundle carries, as the guest checks it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct BundleEntry {
    name: String,
    sha256: String,
    size: u64,
}

impl BundleEntry {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    #[allow(dead_code)] // Read by the guest program rather than by the host.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }
}

/// The manifest as it is written, which is the only thing whose shape matters.
#[derive(Debug, Serialize)]
struct Manifest<'a> {
    files: &'a [BundleEntry],
}

/// What building one bundle needs.
pub(crate) struct BundleRequest<'a> {
    /// The directory holding the running executable; the shipped payload
    /// catalogs and archives are found below it.
    pub(crate) release_directory: &'a Path,
    /// The import's own staging directory. The bundle is built in a child of
    /// it, so an interrupted import leaves it where a resumption looks.
    pub(crate) staging_directory: &'a Path,
    /// VMLord's Linux agent, as the release ships it beside the executable.
    pub(crate) agent_binary: &'a Path,
    /// What the guest itself answered, which is what the payloads are chosen
    /// for.
    pub(crate) guest: &'a GuestIdentity,
    /// The guest user the bootstrap session connects as, and whose
    /// `authorized_keys` the VM's own key is installed into.
    pub(crate) guest_username: &'a str,
    /// The public half of the key pair the bootstrap already generated for this
    /// VM.
    pub(crate) vmlord_public_key: &'a str,
    /// This VM's agent secret, which travels as a file of its own so that it is
    /// never a value in a command, a log or the manifest.
    pub(crate) agent_secret: &'a SecretText,
}

/// A built bundle: a directory of files and the manifest that binds them.
#[derive(Debug)]
pub(crate) struct ConversionBundle {
    root: PathBuf,
    entries: Vec<BundleEntry>,
}

impl ConversionBundle {
    /// Assembles the bundle for `request` and writes its manifest.
    ///
    /// Everything is decided before anything is written: a release with no
    /// display or GPU payload for this guest, an agent that is not where it
    /// should be, or an archive whose bytes are not the ones the catalog
    /// published all stop the import here rather than inside a guest that has
    /// already had half of AppSandbox removed.
    pub(crate) fn build(request: &BundleRequest<'_>) -> Result<Self, RepositoryError> {
        let display = DisplayPayloadCatalog::from_release_directory(request.release_directory)
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the display payload catalog could not be read: {error}"
                ))
            })?;
        let display = display
            .select_for_guest(
                &DisplayGuestSelector {
                    distribution: request.guest.distribution(),
                    release: request.guest.release(),
                    architecture: request.guest.architecture(),
                },
                speaks(),
            )
            .map_err(|error| {
                RepositoryError::new(format!(
                    "this VMLord has no display payload for the guest it copied: {error}"
                ))
            })?;
        let gpu =
            PayloadCatalog::from_release_directory(request.release_directory).map_err(|error| {
                RepositoryError::new(format!(
                    "the GPU payload catalog could not be read: {error}"
                ))
            })?;
        let gpu = gpu
            .select_for_guest(&GpuGuestSelector {
                distribution: request.guest.distribution(),
                release: request.guest.release(),
                architecture: request.guest.architecture(),
            })
            .map_err(|error| {
                RepositoryError::new(format!(
                    "this VMLord has no gpu payload for the guest it copied: {error}"
                ))
            })?;

        if !request.agent_binary.is_file() {
            return Err(RepositoryError::new(format!(
                "the VMLord agent is not at {}, so there is no {AGENT_NAME} to install in the \
                 copied guest",
                request.agent_binary.display()
            )));
        }

        let display_archive = release::archive_path(
            request.release_directory,
            LOCAL_ARCHIVE_DIRECTORY,
            display.payload_id(),
        );
        let gpu_archive = gpu_archive_path(request.release_directory, gpu.payload_id());
        verify_archive(
            &display_archive,
            display.archive_sha256(),
            DISPLAY_ARCHIVE_NAME,
        )?;
        verify_archive(&gpu_archive, gpu.archive_sha256(), GPU_ARCHIVE_NAME)?;

        let root = request.staging_directory.join("bundle");
        replace_directory(&root)?;

        write_file(&root.join(GUEST_PROGRAM_NAME), GUEST_PROGRAM.as_bytes())?;
        write_file(
            &root.join(INPUT_NAME),
            &serde_json::to_vec_pretty(&GuestInput {
                guest_username: request.guest_username,
                vmlord_public_key: request.vmlord_public_key,
                display_payload_id: display.payload_id(),
                gpu_payload_id: gpu.payload_id(),
                appsandbox_units: APPSANDBOX_UNITS,
                obsolete_paths: OBSOLETE_APPSANDBOX_PATHS,
            })
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the conversion input could not be written: {error}"
                ))
            })?,
        )?;
        write_file(
            &root.join(AGENT_SECRET_NAME),
            request.agent_secret.expose().as_bytes(),
        )?;
        copy_file(request.agent_binary, &root.join(AGENT_NAME))?;
        copy_file(&display_archive, &root.join(DISPLAY_ARCHIVE_NAME))?;
        copy_file(&gpu_archive, &root.join(GPU_ARCHIVE_NAME))?;

        let mut entries = Vec::new();
        for name in [
            AGENT_SECRET_NAME,
            DISPLAY_ARCHIVE_NAME,
            GPU_ARCHIVE_NAME,
            INPUT_NAME,
            AGENT_NAME,
            GUEST_PROGRAM_NAME,
        ] {
            entries.push(entry_for(&root, name)?);
        }
        // Sorted, so that the same inputs produce the same document: a resumed
        // conversion compares a bundle it built again against the one the guest
        // is holding, and an order that depended on how a directory listed
        // would make two identical bundles look like two different ones.
        entries.sort_by(|left, right| left.name.cmp(&right.name));

        let manifest =
            serde_json::to_vec_pretty(&Manifest { files: &entries }).map_err(|error| {
                RepositoryError::new(format!(
                    "the conversion manifest could not be written: {error}"
                ))
            })?;
        write_file(&root.join(MANIFEST_NAME), &manifest)?;

        let root_shown = root.display().to_string();
        let display_payload_id = display.payload_id();
        let gpu_payload_id = gpu.payload_id();
        tracing::info!(
            "the conversion bundle at {root_shown} carries display payload \
             {display_payload_id} and gpu payload {gpu_payload_id}"
        );
        Ok(Self { root, entries })
    }

    /// The host directory the bundle was built in, which is what gets uploaded.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn entries(&self) -> &[BundleEntry] {
        &self.entries
    }

    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_NAME)
    }
}

/// Refuses an archive whose bytes are not the ones its entry was published
/// with.
fn verify_archive(path: &Path, expected: &Sha256Digest, name: &str) -> Result<(), RepositoryError> {
    let file = File::open(path).map_err(|error| {
        RepositoryError::new(format!(
            "the {name} of this release is missing at {}: {error}",
            path.display()
        ))
    })?;
    let found = Sha256Digest::hash_reader(file).map_err(|error| {
        RepositoryError::new(format!(
            "the {name} at {} could not be read: {error}",
            path.display()
        ))
    })?;
    if found.as_hex() != expected.as_hex() {
        return Err(RepositoryError::new(format!(
            "the {name} at {} is not the archive its catalog entry published",
            path.display()
        )));
    }
    Ok(())
}

/// An empty directory at `path`, whatever was there before.
///
/// A rebuild after an interrupted attempt must not inherit a half-written file:
/// what the manifest binds is what this build put there, and nothing else.
fn replace_directory(path: &Path) -> Result<(), RepositoryError> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RepositoryError::new(format!(
                "the conversion bundle at {} could not be cleared: {error}",
                path.display()
            )));
        }
    }
    fs::create_dir_all(path).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle directory {} could not be created: {error}",
            path.display()
        ))
    })
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), RepositoryError> {
    fs::write(path, contents).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle file {} could not be written: {error}",
            path.display()
        ))
    })
}

fn copy_file(from: &Path, to: &Path) -> Result<(), RepositoryError> {
    fs::copy(from, to).map(|_| ()).map_err(|error| {
        RepositoryError::new(format!(
            "{} could not be copied into the conversion bundle: {error}",
            from.display()
        ))
    })
}

fn entry_for(root: &Path, name: &str) -> Result<BundleEntry, RepositoryError> {
    let path = root.join(name);
    let size = fs::metadata(&path)
        .map_err(|error| {
            RepositoryError::new(format!(
                "the conversion bundle file {} could not be measured: {error}",
                path.display()
            ))
        })?
        .len();
    let file = File::open(&path).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle file {} could not be read back: {error}",
            path.display()
        ))
    })?;
    let sha256 = Sha256Digest::hash_reader(file).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle file {} could not be hashed: {error}",
            path.display()
        ))
    })?;
    Ok(BundleEntry {
        name: name.to_owned(),
        sha256: sha256.as_hex().to_owned(),
        size,
    })
}

/// The one program that runs inside the guest, byte for byte the same in every
/// import.
///
/// Fixed on purpose. It is uploaded, hashed against the manifest and only then
/// trusted, and it takes every name, path and key it acts on from `input.json`
/// beside it -- so the host never has to build a command out of a value it did
/// not choose, and the guest never has to parse one.
///
/// Python 3 rather than a shell: the input is JSON, and a shell that parsed
/// JSON would be a parser written twice and wrongly. Every guest this
/// conversion supports ships Python 3, because cloud-init -- which provisioned
/// it -- is written in it.
const GUEST_PROGRAM: &str = include_str!("convert.py");

#[cfg(test)]
pub(crate) use test_support::test_release_directory;

#[cfg(test)]
mod test_support {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::json;
    use vmlord_payload::Sha256Digest;

    const COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";

    fn digest_of(bytes: &[u8]) -> String {
        Sha256Digest::hash_reader(bytes)
            .unwrap()
            .as_hex()
            .to_owned()
    }

    /// A release directory carrying one GPU and one display payload for the
    /// guest the fixtures observe.
    ///
    /// Shared with the conversion's own tests: a runner test needs a release the
    /// bundle can be built from, and two fixtures for one release layout would be
    /// two things to keep in step.
    pub(crate) fn test_release_directory(root: &Path) -> PathBuf {
        let release = root.join("release");
        let gpu_archive = b"gpu payload archive".as_slice();
        let display_archive = b"display payload archive".as_slice();

        let gpu = release.join("gpu-payload");
        fs::create_dir_all(&gpu).unwrap();
        fs::write(gpu.join("ubuntu-24.04-amd64-6.8.0-31-v1.zip"), gpu_archive).unwrap();
        fs::write(
            gpu.join("ubuntu-24.04-amd64-6.8.0-31-v1.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 2,
                "payload_id": "ubuntu-24.04-amd64-6.8.0-31-v1",
                "target": {
                    "distribution": "ubuntu",
                    "release": "24.04",
                    "architecture": "amd64",
                    "kernel_release": "6.8.0-31-generic",
                    "payload_abi": 1
                },
                "expanded_size_limit": 4096,
                "file_count_limit": 16,
                "archive_sha256": digest_of(gpu_archive),
                "payload_manifest_sha256": digest_of(b"gpu manifest"),
                "required_renderers": ["d3d12-gallium"],
                "mesa_policy": "distro",
                "sources": [{ "url": "https://example.invalid/mesa", "commit": COMMIT, "version": "24.0" }],
                "licenses": [{ "spdx": "MIT", "path": "LICENSE" }]
            }))
            .unwrap(),
        )
        .unwrap();

        let display = release.join("display-payload");
        fs::create_dir_all(&display).unwrap();
        fs::write(
            display.join("display-ubuntu-24.04-amd64-0.1.0.zip"),
            display_archive,
        )
        .unwrap();
        fs::write(
            display.join("display-ubuntu-24.04-amd64-0.1.0.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "payload_id": "display-ubuntu-24.04-amd64-0.1.0",
                "version": "0.1.0",
                "target": {
                    "distribution": "ubuntu",
                    "release": "24.04",
                    "architecture": "amd64",
                    "payload_abi": 1
                },
                "proven_on": "6.8.0-31-generic",
                "protocol": { "major": 1, "min_minor": 0, "max_minor": 99 },
                "archive_sha256": digest_of(display_archive),
                "payload_manifest_sha256": digest_of(b"display manifest"),
                "expanded_size_limit": 4096,
                "file_count_limit": 16,
                "sources": [{ "url": "https://example.invalid/asb-drm", "commit": COMMIT, "version": "0.1.0" }],
                "licenses": [{ "spdx": "GPL-2.0-only", "path": "LICENSE" }]
            }))
            .unwrap(),
        )
        .unwrap();

        release
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::json;
    use uuid::Uuid;
    use vmlord_payload::Sha256Digest;

    use super::{BundleRequest, ConversionBundle, GUEST_BUNDLE_DIRECTORY, test_release_directory};
    use crate::appsandbox::conversion::{GuestIdentity, SecretText};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_root(label: &str) -> TempRoot {
        let path = std::env::temp_dir().join(format!(
            "vmlord-appsandbox-bundle-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }

    fn agent_binary(root: &Path) -> PathBuf {
        let path = root.join("vmlord-agent");
        fs::write(&path, b"static musl agent").unwrap();
        path
    }

    fn guest() -> GuestIdentity {
        GuestIdentity::observed(
            "ubuntu",
            "24.04",
            "x86_64",
            "6.8.0-31-generic",
            "Ubuntu 24.04.1 LTS",
        )
    }

    fn request<'a>(
        release: &'a Path,
        staging: &'a Path,
        agent: &'a Path,
        identity: &'a GuestIdentity,
        secret: &'a SecretText,
    ) -> BundleRequest<'a> {
        BundleRequest {
            release_directory: release,
            staging_directory: staging,
            agent_binary: agent,
            guest: identity,
            guest_username: "sandbox",
            vmlord_public_key: "ssh-ed25519 AAAAC3Nz vmlord",
            agent_secret: secret,
        }
    }

    fn build_in(root: &Path, label: &str) -> ConversionBundle {
        let release = test_release_directory(root);
        let staging = root.join(label);
        fs::create_dir_all(&staging).unwrap();
        let agent = agent_binary(root);
        let identity = guest();
        let secret = SecretText::new("c2VjcmV0");
        ConversionBundle::build(&request(&release, &staging, &agent, &identity, &secret)).unwrap()
    }

    #[test]
    fn a_bundle_binds_every_file_it_carries_to_a_hash_a_guest_can_check() {
        let root = temporary_root("manifest");
        let bundle = build_in(&root.0, "first");

        let names: Vec<&str> = bundle.entries().iter().map(|entry| entry.name()).collect();
        assert_eq!(
            names,
            vec![
                "agent.secret",
                "display-payload.zip",
                "gpu-payload.zip",
                "input.json",
                "vmlord-agent",
                "vmlord-convert",
            ],
            "every file the guest installs is bound to the manifest, in one stable order"
        );
        for entry in bundle.entries() {
            let bytes = fs::read(bundle.root().join(entry.name())).unwrap();
            assert_eq!(
                entry.sha256(),
                Sha256Digest::hash_reader(bytes.as_slice())
                    .unwrap()
                    .as_hex(),
                "{} is bound to the hash of what was written",
                entry.name()
            );
        }
        assert!(
            bundle.manifest_path().is_file(),
            "the guest verifies itself against a manifest that travels with it"
        );
    }

    /// Two builds of the same inputs must produce the same manifest, or a
    /// resumed conversion could not tell an unchanged bundle from a tampered
    /// one.
    #[test]
    fn a_manifest_is_the_same_document_for_the_same_inputs() {
        let root = temporary_root("deterministic");
        let first = build_in(&root.0, "first");
        let second = build_in(&root.0, "second");

        assert_ne!(first.root(), second.root());
        assert_eq!(
            fs::read(first.manifest_path()).unwrap(),
            fs::read(second.manifest_path()).unwrap()
        );
    }

    #[test]
    fn a_build_without_the_agent_binary_names_the_file_it_could_not_find() {
        let root = temporary_root("no-agent");
        let release = test_release_directory(&root.0);
        let staging = root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let missing = root.0.join("nowhere").join("vmlord-agent");
        let identity = guest();
        let secret = SecretText::new("c2VjcmV0");

        let error =
            ConversionBundle::build(&request(&release, &staging, &missing, &identity, &secret))
                .expect_err("a conversion that cannot install an agent is not one to start");

        assert!(error.to_string().contains("vmlord-agent"), "{error}");
    }

    #[test]
    fn a_build_for_a_guest_no_payload_covers_says_which_guest_had_none() {
        let root = temporary_root("no-payload");
        let release = test_release_directory(&root.0);
        let staging = root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let agent = agent_binary(&root.0);
        let secret = SecretText::new("c2VjcmV0");
        let identity =
            GuestIdentity::observed("fedora", "41", "x86_64", "6.11.0-1.fc41", "Fedora Linux 41");

        let error =
            ConversionBundle::build(&request(&release, &staging, &agent, &identity, &secret))
                .expect_err("a guest the release covers nothing for cannot be converted");

        assert!(error.to_string().contains("fedora"), "{error}");
        assert!(error.to_string().contains("41"), "{error}");
    }

    /// An archive whose bytes are not the ones the entry was published with is
    /// a broken release, and installing it inside a guest is the last place to
    /// find that out.
    #[test]
    fn a_build_refuses_an_archive_the_release_no_longer_matches() {
        let root = temporary_root("tampered");
        let release = test_release_directory(&root.0);
        fs::write(
            release
                .join("gpu-payload")
                .join("ubuntu-24.04-amd64-6.8.0-31-v1.zip"),
            b"something else",
        )
        .unwrap();
        let staging = root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let agent = agent_binary(&root.0);
        let identity = guest();
        let secret = SecretText::new("c2VjcmV0");

        let error =
            ConversionBundle::build(&request(&release, &staging, &agent, &identity, &secret))
                .expect_err("an archive that is not what the catalog published is not usable");

        assert!(error.to_string().contains("gpu"), "{error}");
    }

    /// The guest-side program takes every name and path it needs from this
    /// document, so that nothing a user chose is ever part of a command.
    #[test]
    fn the_user_chosen_values_travel_in_a_json_document_rather_than_a_command() {
        let root = temporary_root("input");
        let bundle = build_in(&root.0, "first");

        let input: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.root().join("input.json")).unwrap()).unwrap();

        assert_eq!(input["guest_username"], json!("sandbox"));
        assert_eq!(
            input["vmlord_public_key"],
            json!("ssh-ed25519 AAAAC3Nz vmlord")
        );
        assert_eq!(
            input["display_payload_id"],
            json!("display-ubuntu-24.04-amd64-0.1.0")
        );
        assert_eq!(
            input["gpu_payload_id"],
            json!("ubuntu-24.04-amd64-6.8.0-31-v1")
        );
    }

    /// The units to stop are data the guest program is handed rather than
    /// knowledge baked into it, and they are exactly AppSandbox's own: a unit
    /// this list got wrong is either a daemon left running against VMLord or a
    /// guest service stopped for no reason.
    #[test]
    fn the_input_document_names_the_exact_appsandbox_units_to_disable() {
        let root = temporary_root("units");
        let bundle = build_in(&root.0, "first");

        let input: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.root().join("input.json")).unwrap()).unwrap();

        assert_eq!(
            input["appsandbox_units"],
            json!([
                "appsandbox-agent.service",
                "appsandbox-audio.service",
                "appsandbox-display.service",
                "appsandbox-input.service",
                "asb-evict-simpledrm.service",
            ])
        );
        let obsolete = input["obsolete_paths"].as_array().unwrap();
        for path in [
            "/usr/local/bin/appsandbox-agent",
            "/etc/systemd/system/appsandbox-agent.service",
            "/etc/modprobe.d/asb_drm.conf",
        ] {
            assert!(
                obsolete.contains(&json!(path)),
                "{path} is AppSandbox's and must go: {obsolete:?}"
            );
        }
    }

    /// The agent secret reaches the guest as a file of its own, so that no
    /// command line, log or manifest ever holds the secret itself.
    #[test]
    fn the_agent_secret_travels_as_a_file_and_never_as_a_value_in_the_manifest() {
        let root = temporary_root("secret");
        let bundle = build_in(&root.0, "first");

        assert_eq!(
            fs::read_to_string(bundle.root().join("agent.secret")).unwrap(),
            "c2VjcmV0"
        );
        let manifest = fs::read_to_string(bundle.manifest_path()).unwrap();
        assert!(!manifest.contains("c2VjcmV0"), "{manifest}");
        let input = fs::read_to_string(bundle.root().join("input.json")).unwrap();
        assert!(!input.contains("c2VjcmV0"), "{input}");
    }

    /// The uploaded program is the only thing that runs in the guest, and every
    /// value it acts on comes out of `input.json`.
    #[test]
    fn the_guest_program_is_fixed_and_reads_its_values_from_the_input_document() {
        let root = temporary_root("script");
        let bundle = build_in(&root.0, "first");

        let script = fs::read_to_string(bundle.root().join("vmlord-convert")).unwrap();
        assert!(script.starts_with("#!/usr/bin/env python3"), "{script}");
        assert!(script.contains("input.json"), "{script}");
        assert!(script.contains(GUEST_BUNDLE_DIRECTORY), "{script}");
        for value in [
            "ssh-ed25519 AAAAC3Nz vmlord",
            "display-ubuntu-24.04-amd64-0.1.0",
            "ubuntu-24.04-amd64-6.8.0-31-v1",
        ] {
            assert!(
                !script.contains(value),
                "the program is the same bytes for every import, and {value} is not"
            );
        }
    }

    /// A second attempt writes into the same staging child, and a file left by
    /// an interrupted first attempt must not survive into the manifest.
    #[test]
    fn a_rebuild_replaces_whatever_an_interrupted_attempt_had_left() {
        let root = temporary_root("rebuild");
        let release = test_release_directory(&root.0);
        let staging = root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let agent = agent_binary(&root.0);
        let identity = guest();
        let secret = SecretText::new("c2VjcmV0");
        let first =
            ConversionBundle::build(&request(&release, &staging, &agent, &identity, &secret))
                .unwrap();
        fs::write(first.root().join("leftover"), b"half an upload").unwrap();

        let second =
            ConversionBundle::build(&request(&release, &staging, &agent, &identity, &secret))
                .unwrap();

        assert_eq!(first.root(), second.root());
        assert!(!second.root().join("leftover").exists());
        assert!(
            !second
                .entries()
                .iter()
                .any(|entry| entry.name() == "leftover")
        );
    }
}
