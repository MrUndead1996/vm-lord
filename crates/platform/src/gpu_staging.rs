//! Turning a catalog entry into the payload directory a VM exports.
//!
//! Three steps that belong together and nowhere else: pick the entry for the
//! guest this VM was built from, prepare that generation in the shared cache, and
//! stage it into the VM's own `gpu-payload` child -- the exact directory
//! `gpu_exports` canonicalizes and offers as `vmlord.gpu.payload`.

use std::{path::Path, path::PathBuf, sync::atomic::AtomicBool};

use vmlord_gpu_payload::{
    GuestSelector, PayloadCatalog, PayloadError, PayloadProgress, PrepareRequest, StagedGpuPayload,
    ensure_staging_root, local_archive_path, prepare, stage_payload,
};

use crate::layout::gpu_payload_staging_directory;

/// Everything staging a payload for one VM needs.
pub struct StageGpuPayloadRequest<'a> {
    /// The directory holding the running executable; the shipped archive is
    /// found below it.
    pub executable_directory: &'a Path,
    /// The shared, content-addressed payload cache, common to every VM.
    pub cache_root: &'a Path,
    /// The VM's own directory. Its `gpu-payload` child is what gets filled.
    pub vm_directory: &'a Path,
    /// The guest this payload is for, as the host knows it before boot.
    pub guest: GuestSelector<'a>,
    pub progress: &'a dyn Fn(PayloadProgress),
    pub cancel: &'a AtomicBool,
}

/// Creates the VM's staging root and answers with it.
pub(crate) fn prepare_staging_root(vm_directory: &Path) -> Result<PathBuf, PayloadError> {
    let root = gpu_payload_staging_directory(vm_directory);
    ensure_staging_root(&root)?;
    Ok(root)
}

/// Stages the payload for `guest` into the VM's `gpu-payload` child.
///
/// A failure here is a failure of GPU support and not of the VM: assignment is
/// best effort by design, so the caller decides what a [`PayloadError`] means
/// for a start and nothing in this module touches lifecycle.
pub fn stage_for_vm(request: StageGpuPayloadRequest<'_>) -> Result<StagedGpuPayload, PayloadError> {
    let catalog = PayloadCatalog::embedded()?;
    let entry = catalog.select_for_guest(&request.guest)?;
    let archive = local_archive_path(request.executable_directory, entry.payload_id());
    let ready = prepare(PrepareRequest {
        entry,
        cache_root: request.cache_root,
        local_archive: Some(&archive),
        progress: request.progress,
        cancel: request.cancel,
    })?;
    let root = prepare_staging_root(request.vm_directory)?;
    stage_payload(&ready, &root, request.progress, request.cancel)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use vmlord_core::{GpuShare, RepositoryError};
    use vmlord_gpu_payload::{GuestSelector, PayloadError};

    use super::{StageGpuPayloadRequest, prepare_staging_root, stage_for_vm};
    use crate::gpu_exports::{ExportRoots, build_with_payload};

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-gpu-staging-{label}-{}-{sequence}",
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

    #[test]
    fn a_staged_generation_is_what_the_payload_share_exports() {
        let temporary = TemporaryDirectory::new("root");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();

        let root = prepare_staging_root(&vm).unwrap();

        assert_eq!(root, vm.join("gpu-payload"));
        assert!(root.join("generations").is_dir());
        assert!(root.join("ready").is_dir());

        let canonicalize = |path: &Path| {
            fs::canonicalize(path)
                .map_err(|error| RepositoryError::new(format!("{}: {error}", path.display())))
        };
        // No system roots: this test is about the per-VM child alone, and a
        // system directory that does not resolve leaves `ExportRoots` empty.
        let roots = ExportRoots::resolve(&temporary.path().join("no-system32"), &canonicalize);

        // A generation is what the guest mounts: `sources.json` lives at the
        // root of the share, and staging writes it inside the generation.
        let generation = root.join("generations").join("e7664769");
        fs::create_dir_all(&generation).unwrap();
        let exports =
            build_with_payload(&[], &roots, &vm, Some(&generation), &canonicalize).unwrap();
        assert_eq!(exports.manifest().shares[0], GpuShare::payload());

        // The staging root itself is not: a guest mounting it would find
        // `generations` and `ready` and no payload.
        assert!(
            build_with_payload(&[], &roots, &vm, Some(&root), &canonicalize).is_none(),
            "the staging root holds the machinery of a swap, not a payload"
        );
    }

    #[test]
    fn a_guest_the_catalog_has_nothing_for_stages_nothing() {
        let temporary = TemporaryDirectory::new("unsupported");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();

        let result = stage_for_vm(StageGpuPayloadRequest {
            executable_directory: temporary.path(),
            cache_root: &temporary.path().join("cache"),
            vm_directory: &vm,
            // A guest the shipped catalog cannot have an entry for, so that
            // this test says what it means whatever the catalog ships.
            guest: GuestSelector {
                distribution: "ubuntu",
                release: "1.04",
                architecture: "amd64",
            },
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(result, Err(PayloadError::NoPayloadForGuest { .. })));
        assert_eq!(fs::read_dir(&vm).unwrap().count(), 0);
    }
}
