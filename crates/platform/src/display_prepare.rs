//! Getting a VM's display payload ready before the system it belongs to is
//! built.
//!
//! Two steps that belong together and nowhere else: stage the payload this
//! guest gets, and turn the staged generation into the one share the VM is
//! offered. What either failure means is here too, because a display payload
//! that cannot be prepared is a degraded display and never a failed start.

use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use vmlord_core::{
    DesktopProfile, DisplayFailure, DisplayStage, DisplayStatusCode, RepositoryError,
};
use vmlord_payload::PayloadError;

use crate::{
    display_exports::{self, DisplayExport},
    display_staging::{StageDisplayPayloadRequest, stage_for_vm},
    metadata::VmComputeSystemMapping,
};

/// What preparing a VM's display payload came to.
pub(crate) struct PreparedDisplay {
    /// The share to write into the configuration this system is built from.
    ///
    /// `None` is a VM whose payload could not be staged, which is still a VM
    /// that starts.
    pub(crate) export: Option<DisplayExport>,
    /// Why there is no share, when there is none.
    pub(crate) failure: Option<DisplayFailure>,
    /// The version this release could offer this guest, when it has one.
    ///
    /// Recorded even when the export failed: what a release carries is what a
    /// person is told about, and a payload that was selected and could not be
    /// staged is a different sentence from a release that carries none.
    pub(crate) available_version: Option<String>,
}

/// Stages the payload and builds the export, for a VM that asked for a desktop.
///
/// `None` is a headless VM: there is nothing to prepare and nothing to say
/// about it. Everything else answers with something -- including a VM this
/// release carries no payload for, which is a `failure` and a start that
/// carries on regardless.
pub(crate) fn prepare(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    executable_directory: &Path,
    cache_root: &Path,
    canonicalize: &dyn Fn(&Path) -> Result<PathBuf, RepositoryError>,
) -> Option<PreparedDisplay> {
    if !matches!(mapping.desktop_profile, DesktopProfile::Gnome) {
        return None;
    }

    // A VM whose source recorded no guest is one nothing can be selected for:
    // installation media becomes whatever the person installing it chose, and
    // the catalog is keyed by what a guest *is*.
    let Some(target) = mapping.guest_target.as_ref() else {
        tracing::info!(
            "VM \"{}\" records no guest, so no display payload can be chosen for it",
            mapping.vm_name
        );
        return Some(PreparedDisplay {
            export: None,
            failure: Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadMissing,
                "this VM records no guest a display payload could be chosen for",
            )),
            available_version: None,
        });
    };
    let guest = target.display_selector();
    let staged = match stage_for_vm(StageDisplayPayloadRequest {
        executable_directory,
        cache_root,
        vm_directory,
        guest,
        progress: &|_| {},
        cancel: &AtomicBool::new(false),
    }) {
        Ok(staged) => staged,
        Err(error) => {
            let failure = failure_for(&error);
            tracing::warn!(
                "VM \"{}\" starts without a display payload: {error}",
                mapping.vm_name
            );
            return Some(PreparedDisplay {
                export: None,
                failure: Some(failure),
                available_version: None,
            });
        }
    };

    let export = display_exports::build(vm_directory, Some(&staged.active), canonicalize);
    if export.is_none() {
        tracing::warn!(
            "VM \"{}\" staged a display payload at {} that cannot be exported",
            mapping.vm_name,
            staged.active.display()
        );
    }
    let failure = export.is_none().then(|| {
        DisplayFailure::new(
            DisplayStage::Payload,
            DisplayStatusCode::PayloadInvalid,
            "the staged display payload could not be offered to the VM",
        )
    });
    Some(PreparedDisplay {
        export,
        failure,
        available_version: Some(staged.version),
    })
}

/// What a staging failure means for the display.
///
/// Two causes and not one per error: a release that carries no payload for this
/// guest will carry none on the next start either, and everything else --  a
/// digest that did not match, a limit that was exceeded, a disk that was full
/// -- is a payload that is there and is not usable.
fn failure_for(error: &PayloadError) -> DisplayFailure {
    let code = match error {
        PayloadError::NoPayloadForGuest { .. } => DisplayStatusCode::PayloadMissing,
        _ => DisplayStatusCode::PayloadInvalid,
    };
    DisplayFailure::new(DisplayStage::Payload, code, error.to_string())
}

#[cfg(test)]
mod tests {
    use vmlord_core::DisplayStatusCode;
    use vmlord_payload::PayloadError;

    use super::failure_for;

    #[test]
    fn a_release_with_nothing_for_this_guest_is_a_missing_payload() {
        let failure = failure_for(&PayloadError::NoPayloadForGuest {
            distribution: "ubuntu".into(),
            release: "24.04".into(),
            architecture: "amd64".into(),
        });

        assert_eq!(failure.code, DisplayStatusCode::PayloadMissing);
        assert!(
            !failure.code.is_retryable(),
            "the next start finds the same release"
        );
        assert!(failure.message.contains("24.04"));
    }

    #[test]
    fn a_payload_that_is_there_and_broken_is_invalid_rather_than_missing() {
        let failure = failure_for(&PayloadError::InvalidCatalog("bad entry".into()));

        assert_eq!(failure.code, DisplayStatusCode::PayloadInvalid);
    }
}
