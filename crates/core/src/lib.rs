//! UI-independent domain types and repository boundary for VMLord.

pub mod distro;
pub mod logging;
pub mod progress;
pub mod settings;

pub use distro::{DistroProfile, ubuntu};
pub use logging::{LoggingError, initialize as initialize_logging};
pub use progress::{DownloadPhase, ProgressPublisher, ProgressThrottle};
pub use settings::{AppSettings, Language, LogLevel, SettingsError, SettingsStore};

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmCreateRequest {
    pub name: String,
    pub image_path: String,
    pub ram_mb: u32,
    pub disk_gb: u32,
    pub cpu_cores: u32,
    pub gpu_mode: GpuMode,
    pub network_mode: NetworkMode,
    pub username: String,
    pub password: String,
    pub ssh_enabled: bool,
    pub ssh_deploy_key: bool,
}

impl VmCreateRequest {
    /// Validates the fields required to provision a VM, before any
    /// filesystem or Windows API side effect is attempted.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        if self.name.trim().is_empty() {
            return Err(RepositoryError::new("VM name must not be empty"));
        }
        if self.image_path.trim().is_empty() {
            return Err(RepositoryError::new("VM image path must not be empty"));
        }
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
    pub gpu_mode: GpuMode,
    pub network_mode: NetworkMode,
    pub ip_address: Option<std::net::IpAddr>,
    pub ssh_port: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    Stopped,
    Starting,
    Running { agent_status: AgentStatus },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Offline,
    Online,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuMode {
    None,
    Default,
    TryAll,
    Unknown(i32),
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
    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError>;
    fn take_diagnostics(&mut self) -> Vec<Diagnostic>;
}

#[cfg(test)]
mod tests {
    use super::{GpuMode, NetworkMode, VmCreateRequest};

    fn valid_request() -> VmCreateRequest {
        VmCreateRequest {
            name: "dev-linux".into(),
            image_path: "C:\\images\\ubuntu.iso".into(),
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
            username: "admin".into(),
            password: "secret".into(),
            ssh_enabled: false,
            ssh_deploy_key: false,
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
            image_path: String::new(),
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
}
