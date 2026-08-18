//! UI-independent domain types and repository boundary for VMLord.

pub mod distro;
pub mod gpu;
pub mod logging;
pub mod progress;
pub mod provisioning;
pub mod settings;
pub mod ssh;

pub use distro::{DistroProfile, SshDaemon, SshUnits, ubuntu};
pub use gpu::{
    GpuAssignment, GpuAvailability, GpuFailure, GpuMode, GpuShare, GpuShareManifest, GpuShareRole,
    GpuStage, GpuState, GpuStatusCode, GuestGpuDetail, GuestGpuReport, HostGpuAdapter,
    HostGpuCapabilities, NativeGpuDetail, VmGpuFacts, VmGpuStatus, GPU_PAYLOAD_SHARE, WSL_LIB_SHARE,
};
pub use logging::{LoggingError, initialize as initialize_logging};
pub use progress::{
    BuildMonitor, BuildProgress, BuildStep, DownloadPhase, ProgressPublisher, ProgressThrottle,
};
pub use provisioning::{
    CloudImage, GuestDefaults, Password, Provisioning, SshAccess, VmSource, validate_username,
    validate_vm_name,
};
pub use settings::{
    AppSettings, GuestReadinessTimeouts, Language, LogLevel, SettingsError, SettingsStore,
};
pub use ssh::{SshAuthentication, SshAvailability, SshConfig, SshEndpoint, SshPort};

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmCreateRequest {
    pub name: String,
    /// Where the system comes from, and -- for a cloud image -- what VMLord
    /// promises to configure inside it.
    pub source: VmSource,
    pub ram_mb: u32,
    pub disk_gb: u32,
    pub cpu_cores: u32,
    pub gpu_mode: GpuMode,
    pub network_mode: NetworkMode,
}

impl VmCreateRequest {
    /// Validates the fields required to provision a VM, before any
    /// filesystem or Windows API side effect is attempted.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        validate_vm_name(&self.name)?;
        self.source.validate()?;
        if self.ram_mb == 0 {
            return Err(RepositoryError::new("VM RAM must be greater than zero"));
        }
        if self.disk_gb == 0 {
            return Err(RepositoryError::new(
                "VM disk size must be greater than zero",
            ));
        }
        if self.cpu_cores == 0 {
            return Err(RepositoryError::new(
                "VM CPU core count must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmUpdateRequest {
    pub name: String,
    pub ram_mb: u32,
    pub cpu_cores: u32,
    pub gpu_mode: GpuMode,
    pub network_mode: NetworkMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmDeleteRequest {
    pub name: String,
    /// Whether the VM's virtual disks are removed along with it.
    ///
    /// Keeping them leaves the VM's directory in place, so a later VM of the
    /// same name cannot reuse that directory.
    pub delete_disks: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmSummary {
    pub name: String,
    pub os_type: String,
    pub state: VmState,
    pub ram_mb: u32,
    pub disk_gb: u32,
    pub cpu_cores: u32,
    /// What the VM asks of the host's GPU: desired state, stored with the VM
    /// and unchanged by whatever a start makes of it.
    pub gpu_mode: GpuMode,
    /// What a backend has observed about that GPU, if anything.
    ///
    /// Facts, not a verdict: `vmlord_app` turns these into the
    /// `VmGpuStatus` a person reads, so the backend never has to name a state.
    pub gpu: VmGpuFacts,
    pub network_mode: NetworkMode,
    pub ip_address: Option<std::net::IpAddr>,
    /// Whether this VM can be reached over SSH, and with what.
    ///
    /// A capability rather than a port: a missing port used to mean "SSH is
    /// off", "the VM is not running" and "this backend does not know" at once,
    /// and every reader had to pick one.
    pub ssh: SshAvailability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    Stopped,
    /// The VM is being created: nothing of it exists yet that could be
    /// started, stopped or deleted.
    Building {
        progress: BuildProgress,
    },
    Starting,
    Running {
        agent_status: AgentStatus,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Offline,
    Online,
    Unknown,
}

/// How a VM is attached to the network.
///
/// Serializable because the platform layer records the mode a VM was created
/// with, and a start has to know whether to give the VM an endpoint. The
/// variant names are therefore an on-disk format: renaming one changes what
/// already-stored VMs read back as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkMode {
    #[default]
    None,
    Nat,
    External,
    Internal,
    Unknown(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryError {
    message: String,
}

impl RepositoryError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RepositoryError {}

pub trait VmRepository {
    fn initialize(&mut self) -> Result<(), RepositoryError>;
    fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError>;
    fn update_vm(&mut self, request: VmUpdateRequest) -> Result<(), RepositoryError>;
    fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError>;
    fn stop_vm(&mut self, name: &str) -> Result<(), RepositoryError>;
    fn force_stop_vm(&mut self, name: &str) -> Result<(), RepositoryError>;
    /// Removes the VM and every resource VMLord created for it.
    ///
    /// Required rather than defaulted: a backend that cannot delete VMs has to
    /// say so, not inherit silence.
    fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError>;
    /// Stops a VM that is still being created, undoing what has been built.
    ///
    /// Defaulted rather than required: a backend that creates VMs
    /// synchronously has nothing in flight to cancel, and saying so is the
    /// honest answer. Deletion is deliberately not made to double as this --
    /// removing a VM that does not exist yet is a different operation with a
    /// different outcome.
    fn cancel_create(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(
            "this backend creates VMs in the foreground, so there is nothing to cancel",
        ))
    }
    /// Where the private half of the VM's own SSH key pair is, or will be.
    ///
    /// A path rather than a file: the create form shows it beside the toggle
    /// that asks for a key pair, which is before any file exists. `None` is
    /// what a backend answers when it does not give VMs keys of their own.
    fn ssh_key_path(&self, _name: &str) -> Option<std::path::PathBuf> {
        None
    }
    fn open_display(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(
            "display connections are not supported by this backend",
        ))
    }
    fn open_ssh(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(
            "SSH connections are not supported by this backend",
        ))
    }
    /// Opens the serial console of a running VM, appending to its log.
    ///
    /// Defaulted rather than required: a backend with no serial port has no
    /// console to open, and saying so is the honest answer. The VM has to be
    /// running -- the pipe the console reads exists only while its compute
    /// system does.
    fn open_console(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(
            "serial consoles are not supported by this backend",
        ))
    }
    /// What the host can do for GPU-PV, as far as it can be told without
    /// starting a VM.
    ///
    /// Defaulted rather than required, and an error rather than an empty
    /// report: a backend that cannot inspect the host does not thereby know
    /// the host has nothing. "This backend cannot tell you" and "this host
    /// cannot do it" are different answers and a reader has to be able to tell
    /// them apart.
    fn host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError> {
        Err(RepositoryError::new(
            "host GPU capabilities are not supported by this backend",
        ))
    }
    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError>;
    fn take_diagnostics(&mut self) -> Vec<Diagnostic>;
}

#[cfg(test)]
mod tests {
    use super::{
        Diagnostic, GpuMode, NetworkMode, RepositoryError, VmCreateRequest, VmDeleteRequest,
        VmRepository, VmSource, VmSummary, VmUpdateRequest,
    };

    fn valid_request() -> VmCreateRequest {
        VmCreateRequest {
            name: "dev-linux".into(),
            source: VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into(),
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    #[test]
    fn accepts_a_fully_populated_request() {
        assert!(valid_request().validate().is_ok());
    }

    #[test]
    fn rejects_an_empty_name() {
        let request = VmCreateRequest {
            name: "  ".into(),
            ..valid_request()
        };
        assert!(request.validate().unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn rejects_an_empty_image_path() {
        let request = VmCreateRequest {
            source: VmSource::LocalMedia {
                path: String::new(),
            },
            ..valid_request()
        };
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("image path")
        );
    }

    #[test]
    fn rejects_provisioning_the_source_refuses() {
        use super::{CloudImage, Provisioning, SshAccess, SshPort, distro::ubuntu};

        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "Invalid".into(),
                    password: None,
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ..valid_request()
        };

        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("user name"),
            "the request must ask its source to validate itself"
        );
    }

    #[test]
    fn rejects_zero_ram_disk_or_cpu_cores() {
        assert!(
            VmCreateRequest {
                ram_mb: 0,
                ..valid_request()
            }
            .validate()
            .is_err()
        );
        assert!(
            VmCreateRequest {
                disk_gb: 0,
                ..valid_request()
            }
            .validate()
            .is_err()
        );
        assert!(
            VmCreateRequest {
                cpu_cores: 0,
                ..valid_request()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_backend_that_cannot_inspect_the_host_says_so() {
        struct SilentBackend;

        impl VmRepository for SilentBackend {
            fn initialize(&mut self) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn create_vm(&mut self, _request: VmCreateRequest) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn update_vm(&mut self, _request: VmUpdateRequest) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn start_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn force_stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn delete_vm(&mut self, _request: VmDeleteRequest) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
                Ok(Vec::new())
            }
            fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
                Vec::new()
            }
        }

        let error = SilentBackend
            .host_gpu_capabilities()
            .expect_err("the default must not claim to know the host");

        assert!(
            error.to_string().contains("not supported by this backend"),
            "a backend that cannot answer has to say so rather than report an empty host: {error}"
        );
    }
}
