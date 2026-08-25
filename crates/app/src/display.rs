//! Turning what a VM asked of its desktop, what installing it made of it and
//! what the guest last said into the status a person reads.
//!
//! Here rather than in a backend or in the UI, for the reason the GPU's
//! derivation is here: a backend reports what it saw and must not have to name
//! a state, the UI paints a state and must not have to work one out, and a
//! pure function between the two can be tested without a host or a guest.

use std::time::SystemTime;

use vmlord_core::{
    DesktopProfile, DisplayFailure, DisplayProvisioning, DisplayStage, DisplayState,
    DisplayStatusCode, GuestDisplayReport, VmDisplayFacts, VmDisplayStatus, VmState,
};

/// Reads what the display stack is doing for a VM from what it asked for, how
/// far installing that got, what state the VM is in and what the guest last
/// reported.
///
/// `now` dates the result only when there is nothing observed to date it by --
/// a VM whose guest has never reported has no observation, and a status still
/// has to say when it was taken.
#[must_use]
pub fn derive_status(
    profile: DesktopProfile,
    provisioning: &DisplayProvisioning,
    state: VmState,
    facts: &VmDisplayFacts,
    now: SystemTime,
) -> VmDisplayStatus {
    let observed_at = facts.observed_at.unwrap_or(now);
    let status = |state: DisplayState,
                  stage: DisplayStage,
                  code: DisplayStatusCode,
                  message: String,
                  can_retry: bool| VmDisplayStatus {
        state,
        stage,
        code,
        message,
        running_version: facts
            .payload
            .loaded
            .clone()
            .or(facts.payload.installed.clone()),
        available_version: facts
            .payload
            .update_available()
            .then(|| facts.payload.available.clone())
            .flatten(),
        guest: guest_detail(facts),
        can_retry,
        updating: facts.update_in_flight,
        observed_at,
    };

    if !profile.wants_desktop() {
        return status(
            DisplayState::Disabled,
            DisplayStage::Idle,
            DisplayStatusCode::ProfileHeadless,
            "This VM has no desktop.".into(),
            false,
        );
    }

    // A desktop that is not installed is the whole story, whether or not the
    // VM runs: the VM works, the desktop does not, and this is the state a
    // retry is offered from.
    if let DisplayProvisioning::Degraded(failure) = provisioning {
        return status(
            DisplayState::Degraded,
            failure.stage,
            failure.code,
            failure.message.clone(),
            provisioning.can_retry(),
        );
    }

    // `NotRequested` beside a desktop profile is a VM whose installation was
    // never recorded -- an older mapping, or a backend that does not record
    // one. It is not evidence that no desktop was asked for: the profile
    // already said one was.
    if !matches!(provisioning, DisplayProvisioning::Ready) {
        return status(
            DisplayState::Provisioning,
            DisplayStage::Provisioning,
            DisplayStatusCode::ProvisioningPending,
            "The desktop has not finished installing.".into(),
            false,
        );
    }

    // The desktop is installed, so a stopped VM is not a problem to report:
    // it displays nothing because it runs nothing.
    if !matches!(state, VmState::Starting | VmState::Running { .. }) {
        return status(
            DisplayState::Disabled,
            DisplayStage::Idle,
            DisplayStatusCode::VmNotRunning,
            "The VM is not running; its desktop starts with it.".into(),
            false,
        );
    }

    // The payload, between the desktop and the guest's services. A module that
    // will not build and a desktop that never installed are both a degraded
    // display and are not the same problem, so the one a person can act on is
    // the one already returned above -- and this is the next one down.
    if let Some(failure) = payload_failure(facts) {
        // An update that rolled back is the one payload failure that is not a
        // degradation: the display works, on the version that was working
        // before, and saying otherwise would paint a working desktop as a
        // broken one.
        let rolled_back = failure.code == DisplayStatusCode::PayloadUpdateRolledBack;
        return status(
            if rolled_back {
                DisplayState::Ready
            } else {
                DisplayState::Degraded
            },
            failure.stage,
            failure.code,
            failure.message.clone(),
            failure.code.is_retryable(),
        );
    }

    match facts.guest.as_ref() {
        None | Some(GuestDisplayReport::ServicesPending) => status(
            DisplayState::WaitingForGuest,
            DisplayStage::Guest,
            DisplayStatusCode::GuestPending,
            "The desktop is installed; waiting for the guest to offer it.".into(),
            false,
        ),
        Some(GuestDisplayReport::Ready(_)) => status(
            DisplayState::Ready,
            DisplayStage::Guest,
            DisplayStatusCode::GuestReady,
            "The guest offers its desktop.".into(),
            false,
        ),
        // A guest that has the desktop and cannot run it is degraded rather
        // than failed, for the same reason a failed installation is: the VM
        // itself is working, and only the desktop is missing.
        Some(GuestDisplayReport::Failed(failure)) => status(
            DisplayState::Degraded,
            failure.stage,
            failure.code,
            failure.message.clone(),
            failure.code.is_retryable(),
        ),
    }
}

/// What the payload half last failed at, if anything.
///
/// A rolled-back update is reported even though the display works, because a
/// person who asked for an update is owed the answer; every other cause here is
/// a display that is not working.
fn payload_failure(facts: &VmDisplayFacts) -> Option<&DisplayFailure> {
    facts.failure.as_ref()
}

fn guest_detail(facts: &VmDisplayFacts) -> Option<vmlord_core::GuestDisplayDetail> {
    match facts.guest.as_ref()? {
        GuestDisplayReport::Ready(detail) => Some(detail.clone()),
        GuestDisplayReport::ServicesPending | GuestDisplayReport::Failed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use vmlord_core::{
        AgentStatus, DisplayFailure, DisplayPayloadFacts, GuestDisplayDetail, GuestDisplayReport,
        VmDisplayFacts,
    };

    use super::{
        DesktopProfile, DisplayProvisioning, DisplayStage, DisplayState, DisplayStatusCode,
        VmState, derive_status,
    };

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn running() -> VmState {
        VmState::Running {
            agent_status: AgentStatus::Online,
        }
    }

    fn ready_guest() -> VmDisplayFacts {
        VmDisplayFacts {
            guest: Some(GuestDisplayReport::Ready(GuestDisplayDetail {
                compositor: Some("gnome-shell".into()),
                output: Some("Virtual-1".into()),
            })),
            observed_at: Some(now() - Duration::from_secs(5)),
            ..VmDisplayFacts::default()
        }
    }

    #[test]
    fn a_headless_vm_has_nothing_to_display() {
        let status = derive_status(
            DesktopProfile::Headless,
            &DisplayProvisioning::NotRequested,
            running(),
            &VmDisplayFacts::default(),
            now(),
        );
        assert_eq!(status.state, DisplayState::Disabled);
        assert_eq!(status.code, DisplayStatusCode::ProfileHeadless);
        assert!(!status.can_retry);
    }

    #[test]
    fn a_desktop_that_is_still_installing_says_so() {
        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Pending,
            VmState::Starting,
            &VmDisplayFacts::default(),
            now(),
        );
        assert_eq!(status.state, DisplayState::Provisioning);
        assert_eq!(status.stage, DisplayStage::Provisioning);
        assert_eq!(status.code, DisplayStatusCode::ProvisioningPending);
        assert_eq!(status.observed_at, now());
    }

    #[test]
    fn a_desktop_that_failed_to_install_is_degraded_and_retryable_even_while_stopped() {
        let provisioning = DisplayProvisioning::Degraded(DisplayFailure::new(
            DisplayStage::Provisioning,
            DisplayStatusCode::PackageDownloadFailed,
            "archive.ubuntu.com did not answer",
        ));
        let status = derive_status(
            DesktopProfile::Gnome,
            &provisioning,
            VmState::Stopped,
            &VmDisplayFacts::default(),
            now(),
        );
        assert_eq!(status.state, DisplayState::Degraded);
        assert_eq!(status.code, DisplayStatusCode::PackageDownloadFailed);
        assert_eq!(status.message, "archive.ubuntu.com did not answer");
        assert!(status.can_retry);
    }

    #[test]
    fn a_cause_a_retry_cannot_get_past_offers_none() {
        let provisioning = DisplayProvisioning::Degraded(DisplayFailure::new(
            DisplayStage::Provisioning,
            DisplayStatusCode::ProfileUnsupported,
            "this release publishes no GNOME packages",
        ));
        let status = derive_status(
            DesktopProfile::Gnome,
            &provisioning,
            running(),
            &VmDisplayFacts::default(),
            now(),
        );
        assert_eq!(status.state, DisplayState::Degraded);
        assert!(!status.can_retry);
    }

    #[test]
    fn a_stopped_vm_with_a_desktop_is_disabled_rather_than_broken() {
        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Ready,
            VmState::Stopped,
            &ready_guest(),
            now(),
        );
        assert_eq!(status.state, DisplayState::Disabled);
        assert_eq!(status.code, DisplayStatusCode::VmNotRunning);
    }

    #[test]
    fn a_running_vm_waits_for_the_guest_before_it_offers_a_display() {
        for facts in [
            VmDisplayFacts::default(),
            VmDisplayFacts {
                guest: Some(GuestDisplayReport::ServicesPending),
                observed_at: Some(now()),
                ..VmDisplayFacts::default()
            },
        ] {
            let status = derive_status(
                DesktopProfile::Gnome,
                &DisplayProvisioning::Ready,
                running(),
                &facts,
                now(),
            );
            assert_eq!(status.state, DisplayState::WaitingForGuest);
            assert_eq!(status.stage, DisplayStage::Guest);
            assert_eq!(status.code, DisplayStatusCode::GuestPending);
            assert!(!status.is_connectable());
            assert_eq!(status.guest, None);
        }
    }

    #[test]
    fn a_guest_that_offers_its_desktop_is_connectable() {
        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Ready,
            running(),
            &ready_guest(),
            now(),
        );
        assert_eq!(status.state, DisplayState::Ready);
        assert_eq!(status.code, DisplayStatusCode::GuestReady);
        assert!(status.is_connectable());
        assert_eq!(
            status.guest.expect("detail").output.as_deref(),
            Some("Virtual-1")
        );
        assert_eq!(status.observed_at, now() - Duration::from_secs(5));
    }

    #[test]
    fn a_payload_that_would_not_build_is_degraded_and_says_so() {
        let facts = VmDisplayFacts {
            update_in_flight: false,
            payload: DisplayPayloadFacts {
                available: Some("0.1.0".into()),
                ..DisplayPayloadFacts::default()
            },
            failure: Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadBuildFailed,
                "dkms build failed for kernel 6.8.0-137-generic",
            )),
            observed_at: Some(now()),
            ..VmDisplayFacts::default()
        };

        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Ready,
            running(),
            &facts,
            now(),
        );

        assert_eq!(status.state, DisplayState::Degraded);
        assert_eq!(status.stage, DisplayStage::Payload);
        assert_eq!(status.code, DisplayStatusCode::PayloadBuildFailed);
        assert!(status.can_retry);
    }

    #[test]
    fn a_newer_payload_in_the_release_is_offered_beside_a_working_display() {
        let facts = VmDisplayFacts {
            payload: DisplayPayloadFacts {
                installed: Some("0.1.0".into()),
                loaded: Some("0.1.0".into()),
                available: Some("0.2.0".into()),
                previous: None,
            },
            guest: Some(GuestDisplayReport::Ready(GuestDisplayDetail::default())),
            observed_at: Some(now()),
            ..VmDisplayFacts::default()
        };

        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Ready,
            running(),
            &facts,
            now(),
        );

        assert_eq!(
            status.state,
            DisplayState::Ready,
            "an offer is not a degradation"
        );
        assert_eq!(status.running_version.as_deref(), Some("0.1.0"));
        assert_eq!(status.available_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn a_desktop_that_never_installed_reads_as_the_desktop_and_not_the_payload() {
        let facts = VmDisplayFacts {
            failure: Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadMissing,
                "this build carries no display payload for ubuntu 24.04 amd64",
            )),
            observed_at: Some(now()),
            ..VmDisplayFacts::default()
        };

        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Degraded(DisplayFailure::new(
                DisplayStage::Provisioning,
                DisplayStatusCode::PackageDownloadFailed,
                "could not reach archive.ubuntu.com",
            )),
            running(),
            &facts,
            now(),
        );

        assert_eq!(status.code, DisplayStatusCode::PackageDownloadFailed);
    }

    #[test]
    fn a_rolled_back_update_is_a_working_display_that_says_what_happened() {
        let facts = VmDisplayFacts {
            payload: DisplayPayloadFacts {
                installed: Some("0.1.0".into()),
                loaded: Some("0.1.0".into()),
                available: Some("0.2.0".into()),
                previous: None,
            },
            guest: Some(GuestDisplayReport::Ready(GuestDisplayDetail::default())),
            failure: Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadUpdateRolledBack,
                "0.2.0 did not verify; 0.1.0 is running",
            )),
            observed_at: Some(now()),
            update_in_flight: false,
        };

        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Ready,
            running(),
            &facts,
            now(),
        );

        assert_eq!(
            status.state,
            DisplayState::Ready,
            "the display works; the update did not"
        );
        assert_eq!(status.code, DisplayStatusCode::PayloadUpdateRolledBack);
        assert!(status.message.contains("0.1.0"));
        assert_eq!(status.running_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn a_guest_whose_display_services_are_down_is_degraded_and_retryable() {
        let facts = VmDisplayFacts {
            guest: Some(GuestDisplayReport::Failed(DisplayFailure::new(
                DisplayStage::Guest,
                DisplayStatusCode::GuestServicesFailed,
                "vmlord-display.service is not running",
            ))),
            observed_at: Some(now()),
            ..VmDisplayFacts::default()
        };
        let status = derive_status(
            DesktopProfile::Gnome,
            &DisplayProvisioning::Ready,
            running(),
            &facts,
            now(),
        );
        assert_eq!(status.state, DisplayState::Degraded);
        assert_eq!(status.stage, DisplayStage::Guest);
        assert!(status.can_retry);
    }
}
