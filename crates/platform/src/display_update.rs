//! Moving one VM's display payload to a newer version, on request.
//!
//! Nothing here happens on a start. A start installs what is missing and
//! rebuilds what a kernel upgrade broke; a version change is an action, with an
//! answer, and this is where that answer is decided.
//!
//! The order matters and is the whole of it: refuse before anything is touched
//! when there is nothing newer or nobody to ask, publish the new version into
//! the directory the VM already exports, ask the guest, and turn what it says
//! into the display's status. A guest that could not verify what it installed
//! rolls back on its own -- and a rollback that worked is a working display.

use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use vmlord_agent_protocol::v1::DisplayUpdateOutcome;
use vmlord_core::{
    DiagnosticLevel, DisplayFailure, DisplayPayloadFacts, DisplayStage, DisplayStatusCode,
    RepositoryError,
};
use vmlord_display_payload::GuestSelector;
use vmlord_payload::PayloadProgress;

use crate::{
    agent::DisplayUpdateAnswer,
    display_staging::{StageDisplayPayloadRequest, stage_for_vm},
};

/// Everything an update needs that this module does not decide for itself.
pub(crate) struct UpdateRequest<'a> {
    pub(crate) vm_name: &'a str,
    pub(crate) vm_directory: &'a Path,
    pub(crate) executable_directory: &'a Path,
    pub(crate) cache_root: &'a Path,
    pub(crate) guest: GuestSelector<'a>,
    /// What the guest last said it has, which is what "newer" is measured
    /// against.
    pub(crate) installed: Option<String>,
    /// Whether the VM is running at all: there is nobody to ask otherwise.
    pub(crate) running: bool,
    pub(crate) progress: &'a dyn Fn(PayloadProgress),
    /// Asks the guest, and answers with what it made of the request.
    pub(crate) ask: &'a dyn Fn(&str) -> Result<DisplayUpdateAnswer, RepositoryError>,
}

/// What an update came to, as the display's own facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateOutcome {
    pub(crate) payload: DisplayPayloadFacts,
    /// The failure to record, which a successful update does not have.
    pub(crate) failure: Option<DisplayFailure>,
}

/// Stages the newest version this release carries and asks the guest to move
/// to it.
///
/// # Errors
///
/// [`RepositoryError`] when there is nothing to move to, nobody to ask, or the
/// new version could not be staged. None of those change what the guest is
/// running: an update that was refused is an update that did not happen.
pub(crate) fn run(request: &UpdateRequest<'_>) -> Result<UpdateOutcome, RepositoryError> {
    if !request.running {
        return Err(RepositoryError::new(format!(
            "VM \"{}\" is not running, so its display payload cannot be updated",
            request.vm_name
        )));
    }

    // Staged before anything is asked of the guest, because staging is what
    // decides whether there is a newer version at all -- and it is the half
    // that can be undone by doing nothing.
    let staged = stage_for_vm(StageDisplayPayloadRequest {
        executable_directory: request.executable_directory,
        cache_root: request.cache_root,
        vm_directory: request.vm_directory,
        guest: request.guest,
        progress: request.progress,
        cancel: &AtomicBool::new(false),
    })
    .map_err(|error| {
        RepositoryError::new(format!(
            "the display payload of VM \"{}\" could not be prepared: {error}",
            request.vm_name
        ))
    })?;

    if request.installed.as_deref() == Some(staged.version.as_str()) {
        return Err(RepositoryError::new(format!(
            "VM \"{}\" already runs display payload {}",
            request.vm_name, staged.version
        )));
    }

    let answer = (request.ask)(&staged.version)?;
    Ok(outcome_of(&answer, staged.version))
}

/// What one answer means for the display's facts.
fn outcome_of(answer: &DisplayUpdateAnswer, available: String) -> UpdateOutcome {
    let report = &answer.report;
    let payload = DisplayPayloadFacts {
        installed: report.installed.clone(),
        previous: report.previous.clone(),
        loaded: report.loaded.clone(),
        available: Some(available),
    };
    let failure = match answer.outcome {
        // Nothing to record: the display works, on the version that was asked
        // for, and a failure beside it would be a fact about nothing.
        DisplayUpdateOutcome::Updated => None,
        _ => report.failure.clone().or_else(|| {
            Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadUpdateFailed,
                "the guest did not say what its update came to",
            ))
        }),
    };
    UpdateOutcome { payload, failure }
}

/// What one finished update is worth saying about it, as a level and a line.
///
/// Here rather than at the caller because this module is what knows the
/// difference: the guest answers every request, and three different things can
/// be true of an answer. An update that took is an ordinary line; one the guest
/// could not verify and rolled back is a warning, because the display works and
/// the version a person asked for is not the one running; one that left neither
/// version loaded is an error, because nothing displays.
pub(crate) fn report(vm_name: &str, outcome: &UpdateOutcome) -> (DiagnosticLevel, String) {
    let Some(failure) = outcome.failure.as_ref() else {
        return (
            DiagnosticLevel::Info,
            match outcome.payload.installed.as_deref() {
                Some(version) => {
                    format!("Display payload of VM \"{vm_name}\" updated to {version}")
                }
                None => format!("Display payload of VM \"{vm_name}\" updated"),
            },
        );
    };

    if failure.code == DisplayStatusCode::PayloadUpdateRolledBack {
        return (
            DiagnosticLevel::Warning,
            format!(
                "Display payload of VM \"{vm_name}\" was not updated: {}",
                failure.message
            ),
        );
    }

    (
        DiagnosticLevel::Error,
        format!(
            "Failed to update the display payload of VM \"{vm_name}\": {}",
            failure.message
        ),
    )
}

/// Where a VM's staged payloads live, for the caller that owns the roots.
pub(crate) fn cache_root(storage_root: &Path) -> PathBuf {
    crate::layout::payload_cache_root(storage_root)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use vmlord_agent_protocol::v1::DisplayUpdateOutcome;
    use vmlord_core::{DisplayFailure, DisplayStage, DisplayStatusCode};
    use vmlord_display_payload::GuestSelector;

    use super::{UpdateOutcome, UpdateRequest, outcome_of, report, run};
    use crate::agent::DisplayUpdateAnswer;
    use crate::agent_session::GuestDisplayPayloadReport;

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    fn temporary(label: &str) -> PathBuf {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vmlord-display-update-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn request<'a>(
        root: &'a Path,
        cache: &'a Path,
        vm: &'a Path,
        installed: Option<String>,
        running: bool,
        ask: &'a dyn Fn(&str) -> Result<DisplayUpdateAnswer, vmlord_core::RepositoryError>,
    ) -> UpdateRequest<'a> {
        UpdateRequest {
            vm_name: "dev-linux",
            vm_directory: vm,
            executable_directory: root,
            cache_root: cache,
            guest: GuestSelector {
                distribution: "ubuntu",
                release: "24.04",
                architecture: "amd64",
            },
            installed,
            running,
            progress: &|_| {},
            ask,
        }
    }

    /// One finished update, as the guest's answer left it.
    fn outcome(installed: Option<&str>, failure: Option<DisplayFailure>) -> UpdateOutcome {
        UpdateOutcome {
            payload: vmlord_core::DisplayPayloadFacts {
                installed: installed.map(str::to_owned),
                previous: None,
                loaded: installed.map(str::to_owned),
                available: Some("0.2.0".into()),
            },
            failure,
        }
    }

    #[test]
    fn an_update_that_took_is_one_ordinary_line_naming_the_version() {
        let (level, message) = report("dev-linux", &outcome(Some("0.2.0"), None));

        assert_eq!(level, vmlord_core::DiagnosticLevel::Info);
        assert!(message.contains("dev-linux") && message.contains("0.2.0"), "{message}");
    }

    /// The answer the `Ok` of an update does not carry: the guest could not
    /// verify the new version and came back on the old one.
    #[test]
    fn an_update_that_rolled_back_is_a_warning_and_not_an_update() {
        let (level, message) = report(
            "dev-linux",
            &outcome(
                Some("0.1.0"),
                Some(DisplayFailure::new(
                    DisplayStage::Payload,
                    DisplayStatusCode::PayloadUpdateRolledBack,
                    "0.2.0 did not verify; 0.1.0 is running",
                )),
            ),
        );

        assert_eq!(
            level,
            vmlord_core::DiagnosticLevel::Warning,
            "the display works, so this is not an error: {message}"
        );
        assert!(message.contains("not updated"), "{message}");
        assert!(message.contains("0.1.0 is running"), "{message}");
    }

    #[test]
    fn an_update_that_left_nothing_running_is_an_error() {
        let (level, _) = report(
            "dev-linux",
            &outcome(
                None,
                Some(DisplayFailure::new(
                    DisplayStage::Payload,
                    DisplayStatusCode::PayloadUpdateFailed,
                    "the module did not load and the previous version did not come back",
                )),
            ),
        );

        assert_eq!(level, vmlord_core::DiagnosticLevel::Error);
    }

    #[test]
    fn a_stopped_vm_is_refused_before_anything_is_staged() {
        let root = temporary("stopped");
        let vm = root.join("vm");
        fs::create_dir_all(&vm).unwrap();

        let error = run(&request(
            &root,
            &root.join("cache"),
            &vm,
            None,
            false,
            &|_| panic!("a stopped VM has nobody to ask"),
        ))
        .expect_err("there is nobody to ask");

        assert!(error.to_string().contains("not running"));
        assert!(
            !vm.join("display-payload").exists(),
            "nothing is staged for a VM that could not be asked"
        );
    }

    #[test]
    fn a_release_with_nothing_to_move_to_is_refused() {
        let root = temporary("nothing");
        let vm = root.join("vm");
        fs::create_dir_all(&vm).unwrap();

        let error = run(&request(
            &root,
            &root.join("cache"),
            &vm,
            Some("0.1.0".into()),
            true,
            &|_| panic!("there is nothing newer to ask for"),
        ))
        .expect_err("this release carries no display payload at all");

        assert!(error.to_string().contains("could not be prepared"));
    }

    #[test]
    fn an_update_that_worked_records_the_version_and_no_failure() {
        let answer = DisplayUpdateAnswer {
            outcome: DisplayUpdateOutcome::Updated,
            report: GuestDisplayPayloadReport {
                installed: Some("0.2.0".into()),
                previous: Some("0.1.0".into()),
                loaded: Some("0.2.0".into()),
                failure: None,
                guest: None,
            },
        };

        let outcome = outcome_of(&answer, "0.2.0".to_owned());

        assert_eq!(outcome.payload.loaded.as_deref(), Some("0.2.0"));
        assert_eq!(outcome.payload.available.as_deref(), Some("0.2.0"));
        assert!(!outcome.payload.update_available());
        assert_eq!(outcome.failure, None);
    }

    #[test]
    fn an_update_that_rolled_back_keeps_the_guests_own_words() {
        let answer = DisplayUpdateAnswer {
            outcome: DisplayUpdateOutcome::RolledBack,
            report: GuestDisplayPayloadReport {
                installed: Some("0.1.0".into()),
                previous: None,
                loaded: Some("0.1.0".into()),
                failure: Some(DisplayFailure::new(
                    DisplayStage::Payload,
                    DisplayStatusCode::PayloadUpdateRolledBack,
                    "0.2.0 did not verify; 0.1.0 is running again",
                )),
                guest: None,
            },
        };

        let outcome = outcome_of(&answer, "0.2.0".to_owned());

        let failure = outcome.failure.expect("a rollback is worth reporting");
        assert_eq!(failure.code, DisplayStatusCode::PayloadUpdateRolledBack);
        assert!(failure.message.contains("0.1.0 is running again"));
        assert_eq!(outcome.payload.loaded.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn an_update_that_said_nothing_is_still_a_failure_with_a_cause() {
        let answer = DisplayUpdateAnswer {
            outcome: DisplayUpdateOutcome::Failed,
            report: GuestDisplayPayloadReport::default(),
        };

        let outcome = outcome_of(&answer, "0.2.0".to_owned());

        assert_eq!(
            outcome.failure.expect("a failure needs a cause").code,
            DisplayStatusCode::PayloadUpdateFailed
        );
    }
}
