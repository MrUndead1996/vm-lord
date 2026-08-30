//! Postconditions of the first ordinary VMLord boot of an imported guest.
//!
//! A conversion succeeding over the bootstrap SSH session proves only what
//! was written to disk.  This verifier proves that the next boot can use it:
//! VMLord's SSH key, the agent and, when requested, the display and GPU paths.

use vmlord_core::{DesktopProfile, GpuMode, RepositoryError};

type Check = Box<dyn Fn() -> Result<(), RepositoryError> + Send + Sync>;

/// Which optional services the completed import promises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VerificationRequest {
    pub(crate) desktop_profile: DesktopProfile,
    pub(crate) gpu_mode: GpuMode,
}

/// Runs second-boot checks at the boundaries that own their protocols.
pub(crate) struct Verification {
    ssh: Check,
    agent: Check,
    display: Check,
    gpu: Check,
}

impl Verification {
    pub(crate) fn new(
        ssh: impl Fn() -> Result<(), RepositoryError> + Send + Sync + 'static,
        agent: impl Fn() -> Result<(), RepositoryError> + Send + Sync + 'static,
        display: impl Fn() -> Result<(), RepositoryError> + Send + Sync + 'static,
        gpu: impl Fn() -> Result<(), RepositoryError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            ssh: Box::new(ssh),
            agent: Box::new(agent),
            display: Box::new(display),
            gpu: Box::new(gpu),
        }
    }

    /// Verifies the promises ordinary metadata will publish, in dependency
    /// order: SSH reaches the guest, the VMLord agent authenticates, then the
    /// optional services that agent drives answer.
    pub(crate) fn run(&self, request: VerificationRequest) -> Result<(), RepositoryError> {
        (self.ssh)()?;
        (self.agent)()?;
        if request.desktop_profile.wants_desktop() {
            (self.display)()?;
        }
        if request.gpu_mode != GpuMode::None {
            (self.gpu)()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use vmlord_core::{DesktopProfile, GpuMode, RepositoryError};

    use super::{Verification, VerificationRequest};

    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<&'static str>>>);

    impl Calls {
        fn record(&self, name: &'static str) -> Result<(), RepositoryError> {
            self.0.lock().unwrap().push(name);
            Ok(())
        }

        fn snapshot(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
    }

    fn verification(calls: &Calls) -> Verification {
        Verification::new(
            {
                let calls = calls.clone();
                move || calls.record("ssh")
            },
            {
                let calls = calls.clone();
                move || calls.record("agent")
            },
            {
                let calls = calls.clone();
                move || calls.record("display")
            },
            {
                let calls = calls.clone();
                move || calls.record("gpu")
            },
        )
    }

    #[test]
    fn a_full_import_verifies_ssh_agent_display_and_gpu() {
        let calls = Calls::default();

        verification(&calls)
            .run(VerificationRequest {
                desktop_profile: DesktopProfile::Gnome,
                gpu_mode: GpuMode::Default,
            })
            .unwrap();

        assert_eq!(calls.snapshot(), ["ssh", "agent", "display", "gpu"]);
    }

    #[test]
    fn a_headless_cpu_only_import_does_not_invent_display_or_gpu_checks() {
        let calls = Calls::default();

        verification(&calls)
            .run(VerificationRequest {
                desktop_profile: DesktopProfile::Headless,
                gpu_mode: GpuMode::None,
            })
            .unwrap();

        assert_eq!(calls.snapshot(), ["ssh", "agent"]);
    }

    #[test]
    fn verification_stops_at_the_first_failed_postcondition() {
        let calls = Calls::default();
        let verification = Verification::new(
            {
                let calls = calls.clone();
                move || calls.record("ssh")
            },
            {
                let calls = calls.clone();
                move || {
                    calls.record("agent")?;
                    Err(RepositoryError::new("agent did not authenticate"))
                }
            },
            {
                let calls = calls.clone();
                move || calls.record("display")
            },
            {
                let calls = calls.clone();
                move || calls.record("gpu")
            },
        );

        let error = verification
            .run(VerificationRequest {
                desktop_profile: DesktopProfile::Gnome,
                gpu_mode: GpuMode::Default,
            })
            .unwrap_err();

        assert!(error.to_string().contains("agent"));
        assert_eq!(calls.snapshot(), ["ssh", "agent"]);
    }
}
