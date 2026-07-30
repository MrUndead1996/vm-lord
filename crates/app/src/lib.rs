//! Application workflows shared by desktop, CLI, and future automation clients.

use vmlord_core::{Diagnostic, RepositoryError, VmRepository, VmSummary};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendStatus {
    Starting,
    Ready,
    Unavailable(String),
}

pub struct WorkspaceApp {
    repository: Box<dyn VmRepository>,
    status: BackendStatus,
    vms: Vec<VmSummary>,
    diagnostics: Vec<Diagnostic>,
}

impl WorkspaceApp {
    #[must_use]
    pub fn new(repository: Box<dyn VmRepository>) -> Self {
        Self {
            repository,
            status: BackendStatus::Starting,
            vms: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn start(&mut self) {
        match self.repository.initialize() {
            Ok(()) => {
                self.status = BackendStatus::Ready;
                self.refresh();
                return;
            }
            Err(error) => self.status = BackendStatus::Unavailable(error.to_string()),
        }
        self.collect_diagnostics();
    }

    pub fn refresh(&mut self) {
        if !matches!(self.status, BackendStatus::Ready) {
            return;
        }

        match self.repository.list_vms() {
            Ok(vms) => self.vms = vms,
            Err(error) => self.status = BackendStatus::Unavailable(error.to_string()),
        }
        self.collect_diagnostics();
    }

    #[must_use]
    pub fn status(&self) -> &BackendStatus {
        &self.status
    }

    #[must_use]
    pub fn vms(&self) -> &[VmSummary] {
        &self.vms
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    fn collect_diagnostics(&mut self) {
        self.diagnostics.extend(self.repository.take_diagnostics());
        const MAX_DIAGNOSTICS: usize = 100;
        if self.diagnostics.len() > MAX_DIAGNOSTICS {
            self.diagnostics
                .drain(..self.diagnostics.len() - MAX_DIAGNOSTICS);
        }
    }
}

pub fn unavailable_repository(message: impl Into<String>) -> Box<dyn VmRepository> {
    Box::new(UnavailableRepository {
        message: message.into(),
    })
}

struct UnavailableRepository {
    message: String,
}

impl VmRepository for UnavailableRepository {
    fn initialize(&mut self) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }

    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }

    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmlord_core::{DiagnosticLevel, VmState};

    struct FakeRepository {
        should_fail: bool,
    }

    impl VmRepository for FakeRepository {
        fn initialize(&mut self) -> Result<(), RepositoryError> {
            if self.should_fail {
                Err(RepositoryError::new("unavailable"))
            } else {
                Ok(())
            }
        }

        fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
            Ok(vec![VmSummary {
                name: "dev".into(),
                os_type: "Linux".into(),
                state: VmState::Stopped,
                ram_mb: 4096,
                disk_gb: 64,
                cpu_cores: 4,
                gpu_mode: vmlord_core::GpuMode::None,
                network_mode: vmlord_core::NetworkMode::Nat,
                ip_address: None,
                ssh_port: Some(22),
            }])
        }

        fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
            vec![Diagnostic {
                level: DiagnosticLevel::Info,
                message: "ready".into(),
            }]
        }
    }

    #[test]
    fn start_loads_vm_list() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository { should_fail: false }));
        app.start();
        assert_eq!(app.status(), &BackendStatus::Ready);
        assert_eq!(app.vms().len(), 1);
        assert_eq!(app.diagnostics().len(), 1);
    }

    #[test]
    fn initialization_error_is_visible() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository { should_fail: true }));
        app.start();
        assert_eq!(
            app.status(),
            &BackendStatus::Unavailable("unavailable".into())
        );
    }
}
