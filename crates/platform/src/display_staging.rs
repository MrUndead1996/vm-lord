//! Turning a display catalog entry into the payload directory a VM exports.
//!
//! The display twin of `gpu_staging`, and separate from it deliberately: the
//! two catalogs select on different things, their failures mean different
//! things to a VM, and a function serving both would be a function with two
//! modes. What they do share is everything underneath -- the content-addressed
//! cache, the expansion limits and the atomic publication are
//! `vmlord-payload`'s, once.

use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use vmlord_display_payload::{
    DisplayPayloadCatalog, GuestSelector, LOCAL_ARCHIVE_DIRECTORY, ProtocolVersionParts,
};
use vmlord_payload::{
    PayloadError, PayloadProgress, PrepareRequest, StagedPayload, ensure_staging_root, prepare,
    publish_active, release, stage_payload,
};

use crate::layout::{display_payload_active_directory, display_payload_staging_directory};

/// Everything staging a display payload for one VM needs.
pub struct StageDisplayPayloadRequest<'a> {
    /// The directory holding the running executable; the shipped archive is
    /// found below it.
    pub executable_directory: &'a Path,
    /// The shared, content-addressed payload cache, common to every VM.
    pub cache_root: &'a Path,
    /// The VM's own directory. Its `display-payload` child is what gets filled.
    pub vm_directory: &'a Path,
    /// The guest this payload is for, as the host knows it before boot.
    pub guest: GuestSelector<'a>,
    pub progress: &'a dyn Fn(PayloadProgress),
    pub cancel: &'a AtomicBool,
}

/// The display protocol revision this build implements.
///
/// Read here and passed into selection, so that `vmlord-display-payload` needs
/// no dependency on the protocol crate and a test can ask what selection does
/// at a revision this build does not have.
fn speaks() -> ProtocolVersionParts {
    ProtocolVersionParts {
        major: vmlord_display_protocol::handshake::CURRENT_VERSION.major,
        minor: vmlord_display_protocol::handshake::CURRENT_VERSION.minor,
    }
}

/// Creates the VM's staging root and answers with it.
pub(crate) fn prepare_staging_root(vm_directory: &Path) -> Result<PathBuf, PayloadError> {
    let root = display_payload_staging_directory(vm_directory);
    ensure_staging_root(&root)?;
    Ok(root)
}

/// A staged display payload, and the version it carries.
///
/// The version travels beside the staged directory because it is what the
/// status compares against what the guest reported, and the staging root knows
/// only a payload ID -- which contains the version and is not one.
#[derive(Debug)]
pub struct StagedDisplayPayload {
    pub staged: StagedPayload,
    pub version: String,
    /// The stable path the VM exports, which is what the guest mounts.
    pub active: PathBuf,
}

/// Stages the display payload for `guest` into the VM's `display-payload` child.
///
/// A failure here is a failure of the display and not of the VM: the caller
/// decides what a [`PayloadError`] means for a start, and nothing in this
/// module touches lifecycle.
///
/// # Errors
///
/// [`PayloadError::NoPayloadForGuest`] when the release carries nothing for
/// this guest that this build can speak to, and whatever verification,
/// expansion or staging reported otherwise.
pub fn stage_for_vm(
    request: StageDisplayPayloadRequest<'_>,
) -> Result<StagedDisplayPayload, PayloadError> {
    let catalog = DisplayPayloadCatalog::from_release_directory(request.executable_directory)?;
    let entry = catalog.select_for_guest(&request.guest, speaks())?;
    let version = entry.version().to_string();
    // Provenance is read out loud and is not a condition: a display payload
    // carries DKMS sources and static binaries, so the release it was built on
    // says where it was proven rather than who it may serve. When those differ,
    // the guest is running a payload nobody built a target for, and the log is
    // the only place that is visible.
    let target = entry.target();
    if target.was_built_for(&request.guest) {
        tracing::debug!(
            "display payload {} was built for {} {} and proven on {}",
            entry.payload_id(),
            target.distribution,
            target.release,
            entry.proven_on()
        );
    } else {
        tracing::info!(
            "display payload {} serves {} {}, having been built for {} {} and proven on {}",
            entry.payload_id(),
            request.guest.distribution,
            request.guest.release,
            target.distribution,
            target.release,
            entry.proven_on()
        );
    }
    let archive = release::archive_path(
        request.executable_directory,
        LOCAL_ARCHIVE_DIRECTORY,
        entry.payload_id(),
    );
    let ready = prepare(PrepareRequest {
        entry,
        cache_root: request.cache_root,
        archive: &archive,
        progress: request.progress,
        cancel: request.cancel,
    })?;
    let root = prepare_staging_root(request.vm_directory)?;
    let staged = stage_payload(&ready, &root, request.progress, request.cancel)?;
    // The generation is what a swap is made atomic by; the active directory is
    // what a boot can export. Both, because an update publishes into the second
    // and the first is what proves it arrived intact.
    let active = display_payload_active_directory(request.vm_directory);
    publish_active(&ready, &active, request.cancel)?;
    Ok(StagedDisplayPayload {
        staged,
        version,
        active,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use vmlord_display_payload::GuestSelector;
    use vmlord_payload::PayloadError;

    use super::{StageDisplayPayloadRequest, prepare_staging_root, stage_for_vm};

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-display-staging-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ubuntu_2404() -> GuestSelector<'static> {
        GuestSelector {
            distribution: "ubuntu",
            release: "24.04",
            architecture: "amd64",
        }
    }

    #[test]
    fn a_display_staging_root_is_the_vms_own_child_and_not_the_gpus() {
        let temporary = TemporaryDirectory::new("root");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();

        let root = prepare_staging_root(&vm).unwrap();

        assert_eq!(root, vm.join("display-payload"));
        assert!(root.join("generations").is_dir());
        assert!(root.join("ready").is_dir());
        assert!(
            !vm.join("gpu-payload").exists(),
            "staging a display payload must not touch the GPU's directory"
        );
    }

    #[test]
    fn a_release_with_no_display_payload_says_which_guest_had_none() {
        let temporary = TemporaryDirectory::new("empty");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();

        let error = stage_for_vm(StageDisplayPayloadRequest {
            executable_directory: temporary.path(),
            cache_root: &temporary.path().join("cache"),
            vm_directory: &vm,
            guest: ubuntu_2404(),
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        })
        .expect_err("a build that ships no display payload has none for anyone");

        assert!(matches!(error, PayloadError::NoPayloadForGuest { .. }));
        assert!(error.to_string().contains("24.04"));
    }

    #[test]
    fn a_broken_release_fails_rather_than_reads_as_empty() {
        let temporary = TemporaryDirectory::new("broken");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();
        let payloads = temporary.path().join("display-payload");
        fs::create_dir(&payloads).unwrap();
        fs::write(payloads.join("something.json"), b"{not json").unwrap();

        let error = stage_for_vm(StageDisplayPayloadRequest {
            executable_directory: temporary.path(),
            cache_root: &temporary.path().join("cache"),
            vm_directory: &vm,
            guest: ubuntu_2404(),
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        })
        .expect_err("a file that is there and wrong is a broken release");

        assert!(matches!(error, PayloadError::InvalidCatalog(_)));
    }
}
