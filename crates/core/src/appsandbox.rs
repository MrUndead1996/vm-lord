//! Domain types for importing a completed AppSandbox Linux VM.
//!
//! The source identity is deliberately opaque. The platform layer resolves it
//! to the AppSandbox files it owns, so an application or UI caller cannot turn
//! an import request into an arbitrary host-path operation.

use serde::{Deserialize, Serialize};

use crate::{GpuMode, NetworkMode, RepositoryError, validate_vm_name};

/// A stable, platform-generated identity for one AppSandbox VM source.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppSandboxSourceId(String);

impl AppSandboxSourceId {
    /// Builds an identity from the stable hash the platform generated.
    pub fn from_stable_hash(hash: impl Into<String>) -> Result<Self, RepositoryError> {
        let hash = hash.into();
        (!hash.is_empty())
            .then_some(Self(hash))
            .ok_or_else(|| RepositoryError::new("AppSandbox source identity must not be empty"))
    }

    /// The stable hash for repository-owned source lookup.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a discovered AppSandbox VM cannot be imported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppSandboxIncompatibility {
    NotLinux,
    Template,
    InstallationIncomplete,
    Running,
    SshDisabled,
    SshKeyNotDeployed,
    SourceDiskMissing,
    SourceDiskMismatch,
    UnsupportedNetworkMode,
    UnsupportedGpuMode,
    InvalidSshPort,
    DuplicateSource,
}

/// Whether a discovered VM is ready for import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppSandboxCompatibility {
    Compatible,
    Incompatible(Vec<AppSandboxIncompatibility>),
}

/// A source VM the platform discovered, without its private host paths or key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSandboxVmCandidate {
    pub source_id: AppSandboxSourceId,
    pub name: String,
    pub ram_mb: u32,
    pub disk_gb: u32,
    pub cpu_cores: u32,
    pub network_mode: NetworkMode,
    pub gpu_mode: GpuMode,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub compatibility: AppSandboxCompatibility,
}

impl AppSandboxVmCandidate {
    /// Validates resource values that must be usable by a created VM.
    pub fn validate(&self) -> Result<(), RepositoryError> {
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

/// A request to copy and convert one previously discovered source VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSandboxImportRequest {
    pub source_id: AppSandboxSourceId,
    pub destination_name: String,
}

impl AppSandboxImportRequest {
    /// Validates the UI-controlled portion of an import request.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        validate_vm_name(&self.destination_name)?;
        if self.source_id.as_str().is_empty() {
            return Err(RepositoryError::new(
                "AppSandbox source identity must not be empty",
            ));
        }
        Ok(())
    }
}

/// An import retained for explicit recovery instead of shown as a healthy VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteAppSandboxImport {
    pub destination_name: String,
    pub stage: crate::progress::AppSandboxImportStage,
}
