//! UI-independent domain types and repository boundary for VMLord.

use std::fmt;

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
    Unknown(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkMode {
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
    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError>;
    fn take_diagnostics(&mut self) -> Vec<Diagnostic>;
}
