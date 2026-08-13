//! Turning the GPU facts a backend observed into the status a person reads.
//!
//! This lives in the application layer rather than in a backend or in the UI
//! for the same reason on both ends: a backend reports what it saw and must
//! not have to name a state, and the UI paints a state and must not have to
//! work one out. Everything between the two is here, in one pure function that
//! can be tested without a host.

use std::time::SystemTime;

use vmlord_core::{
    GpuAssignment, GpuMode, GpuStage, GpuState, GpuStatusCode, GuestGpuReport, VmGpuFacts,
    VmGpuStatus, VmState,
};

/// Reads what GPU-PV is doing for a VM from what was asked of it, what state
/// the VM is in, and what has been observed.
///
/// `now` dates the result only when there is nothing observed to date it by --
/// a VM that has never run has no observation, and a status still has to say
/// when it was taken.
#[must_use]
pub fn derive_status(
    mode: GpuMode,
    state: VmState,
    facts: &VmGpuFacts,
    now: SystemTime,
) -> VmGpuStatus {
    let observed_at = facts.observed_at.unwrap_or(now);
    let status = |state: GpuState, stage: GpuStage, code: GpuStatusCode, message: String| {
        VmGpuStatus {
            state,
            stage,
            code,
            message,
            native: native_detail(facts),
            guest: guest_detail(facts),
            observed_at,
        }
    };

    match mode {
        GpuMode::None => {
            return status(
                GpuState::Disabled,
                GpuStage::Idle,
                GpuStatusCode::ModeDisabled,
                "This VM does not use GPU-PV.".into(),
            );
        }
        // A mode this build cannot apply is a failure rather than a silent
        // `None`: the VM was configured to have a GPU and will not get one.
        GpuMode::Unknown(value) => {
            return status(
                GpuState::Failed,
                GpuStage::Idle,
                GpuStatusCode::ModeUnsupported,
                format!("GPU mode {value} is not supported by this build."),
            );
        }
        GpuMode::Default | GpuMode::Mirror => {}
    }

    // A stopped VM has nothing attached to it, whatever it asks for. Saying so
    // is not the same as saying the GPU failed, and the two must not share a
    // state.
    if !matches!(state, VmState::Starting | VmState::Running { .. }) {
        return status(
            GpuState::Disabled,
            GpuStage::Idle,
            GpuStatusCode::VmNotRunning,
            "The VM is not running; its GPU is attached on the next start.".into(),
        );
    }

    let Some(assignment) = facts.assignment.as_ref() else {
        return status(
            GpuState::WaitingForGuest,
            GpuStage::Assignment,
            GpuStatusCode::AssignmentPending,
            "The host has not attached the GPU yet.".into(),
        );
    };

    let partial_reason = match assignment {
        GpuAssignment::Failed(reason) => {
            return status(
                GpuState::Failed,
                GpuStage::Assignment,
                reason.code,
                reason.message.clone(),
            );
        }
        GpuAssignment::Partial { reason, .. } => Some(reason),
        GpuAssignment::Complete(_) => None,
    };

    match facts.guest.as_ref() {
        None => match partial_reason {
            Some(reason) => status(
                GpuState::WaitingForGuest,
                GpuStage::Guest,
                GpuStatusCode::AssignmentPartial,
                format!("{} Waiting for the guest to report.", reason.message),
            ),
            None => status(
                GpuState::WaitingForGuest,
                GpuStage::Guest,
                GpuStatusCode::GuestPending,
                "The GPU is attached; waiting for the guest to report.".into(),
            ),
        },
        Some(GuestGpuReport::Failed(reason)) => status(
            GpuState::Failed,
            GpuStage::Guest,
            reason.code,
            reason.message.clone(),
        ),
        Some(GuestGpuReport::DevicePresent(_)) => status(
            GpuState::Assigned,
            GpuStage::Guest,
            GpuStatusCode::GuestDevicePresent,
            "The guest sees the GPU, but nothing renders on it yet.".into(),
        ),
        // A partial assignment that works is degraded, not ready: the VM runs
        // with less GPU than it asked for, and only this state says so.
        Some(GuestGpuReport::Ready(_)) => match partial_reason {
            Some(reason) => status(
                GpuState::Degraded,
                GpuStage::Guest,
                GpuStatusCode::AssignmentPartial,
                format!("The guest renders on the GPU. {}", reason.message),
            ),
            None => status(
                GpuState::GuestReady,
                GpuStage::Guest,
                GpuStatusCode::GuestReady,
                "The guest renders on the GPU.".into(),
            ),
        },
    }
}

fn native_detail(facts: &VmGpuFacts) -> Option<vmlord_core::NativeGpuDetail> {
    match facts.assignment.as_ref()? {
        GpuAssignment::Complete(detail) | GpuAssignment::Partial { detail, .. } => {
            Some(detail.clone())
        }
        GpuAssignment::Failed(_) => None,
    }
}

fn guest_detail(facts: &VmGpuFacts) -> Option<vmlord_core::GuestGpuDetail> {
    match facts.guest.as_ref()? {
        GuestGpuReport::DevicePresent(detail) | GuestGpuReport::Ready(detail) => {
            Some(detail.clone())
        }
        GuestGpuReport::Failed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::derive_status;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use vmlord_core::{
        AgentStatus, GpuAssignment, GpuFailure, GpuMode, GpuStage, GpuState, GpuStatusCode,
        GuestGpuDetail, GuestGpuReport, NativeGpuDetail, VmGpuFacts, VmState,
    };

    const NOW: SystemTime = UNIX_EPOCH;

    fn running() -> VmState {
        VmState::Running {
            agent_status: AgentStatus::Online,
        }
    }

    fn attached() -> NativeGpuDetail {
        NativeGpuDetail {
            adapter: Some("NVIDIA RTX 4070".into()),
            adapters: 1,
        }
    }

    fn rendering() -> GuestGpuDetail {
        GuestGpuDetail {
            driver: Some("dxgkrnl".into()),
            render_node: Some("/dev/dri/renderD128".into()),
        }
    }

    #[test]
    fn a_vm_without_a_gpu_is_disabled_whatever_it_is_doing() {
        let status = derive_status(GpuMode::None, running(), &VmGpuFacts::default(), NOW);

        assert_eq!(status.state, GpuState::Disabled);
        assert_eq!(status.code, GpuStatusCode::ModeDisabled);
        assert_eq!(status.stage, GpuStage::Idle);
    }

    #[test]
    fn a_stopped_vm_that_asks_for_a_gpu_is_disabled_rather_than_failed() {
        let status = derive_status(
            GpuMode::Default,
            VmState::Stopped,
            &VmGpuFacts::default(),
            NOW,
        );

        assert_eq!(status.state, GpuState::Disabled);
        assert_eq!(status.code, GpuStatusCode::VmNotRunning);
        assert!(
            status.message.contains("next start"),
            "a stopped VM has to be told when its GPU arrives: {}",
            status.message
        );
    }

    #[test]
    fn a_mode_this_build_cannot_apply_fails() {
        let status = derive_status(GpuMode::Unknown(7), running(), &VmGpuFacts::default(), NOW);

        assert_eq!(status.state, GpuState::Failed);
        assert_eq!(status.code, GpuStatusCode::ModeUnsupported);
    }

    #[test]
    fn a_starting_vm_with_nothing_observed_waits_on_the_host() {
        let status = derive_status(
            GpuMode::Mirror,
            VmState::Starting,
            &VmGpuFacts::default(),
            NOW,
        );

        assert_eq!(status.state, GpuState::WaitingForGuest);
        assert_eq!(status.stage, GpuStage::Assignment);
        assert_eq!(status.code, GpuStatusCode::AssignmentPending);
    }

    #[test]
    fn an_attached_gpu_waits_on_the_guest() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Complete(attached())),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Default, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::WaitingForGuest);
        assert_eq!(status.stage, GpuStage::Guest);
        assert_eq!(status.code, GpuStatusCode::GuestPending);
        assert_eq!(status.native, Some(attached()));
    }

    #[test]
    fn a_host_that_could_not_attach_anything_fails_with_its_own_reason() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Failed(GpuFailure::new(
                GpuStatusCode::AssignmentFailed,
                "no GPU-PV capable adapter was found",
            ))),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Default, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::Failed);
        assert_eq!(status.stage, GpuStage::Assignment);
        assert_eq!(status.message, "no GPU-PV capable adapter was found");
        assert_eq!(
            status.native, None,
            "nothing was attached, so there is no adapter to report"
        );
    }

    #[test]
    fn a_guest_that_only_sees_the_device_is_assigned_not_ready() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Complete(attached())),
            guest: Some(GuestGpuReport::DevicePresent(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: None,
            })),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Default, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::Assigned);
        assert_eq!(status.code, GpuStatusCode::GuestDevicePresent);
    }

    #[test]
    fn a_guest_that_renders_is_ready() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Complete(attached())),
            guest: Some(GuestGpuReport::Ready(rendering())),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Default, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::GuestReady);
        assert_eq!(status.code, GpuStatusCode::GuestReady);
        assert_eq!(status.guest, Some(rendering()));
    }

    #[test]
    fn a_partial_assignment_that_renders_is_degraded_and_says_what_is_missing() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Partial {
                detail: attached(),
                reason: GpuFailure::new(
                    GpuStatusCode::AssignmentPartial,
                    "the second adapter could not be attached",
                ),
            }),
            guest: Some(GuestGpuReport::Ready(rendering())),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Mirror, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::Degraded);
        assert_eq!(status.code, GpuStatusCode::AssignmentPartial);
        assert!(
            status.message.contains("second adapter"),
            "a degraded GPU has to say what it is missing: {}",
            status.message
        );
    }

    #[test]
    fn a_guest_that_cannot_use_the_device_fails_with_its_own_reason() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Complete(attached())),
            guest: Some(GuestGpuReport::Failed(GpuFailure::new(
                GpuStatusCode::GuestFailed,
                "the guest kernel has no dxgkrnl module",
            ))),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Default, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::Failed);
        assert_eq!(status.stage, GpuStage::Guest);
        assert_eq!(status.message, "the guest kernel has no dxgkrnl module");
    }

    #[test]
    fn a_status_is_dated_by_the_observation_behind_it() {
        let observed = UNIX_EPOCH + Duration::from_secs(1_000);
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Complete(attached())),
            guest: Some(GuestGpuReport::Ready(rendering())),
            observed_at: Some(observed),
        };

        let status = derive_status(
            GpuMode::Default,
            running(),
            &facts,
            UNIX_EPOCH + Duration::from_secs(9_999),
        );

        assert_eq!(status.observed_at, observed);
    }

    #[test]
    fn a_status_with_nothing_observed_is_dated_now() {
        let now = UNIX_EPOCH + Duration::from_secs(42);

        let status = derive_status(GpuMode::None, VmState::Stopped, &VmGpuFacts::default(), now);

        assert_eq!(status.observed_at, now);
    }
}
