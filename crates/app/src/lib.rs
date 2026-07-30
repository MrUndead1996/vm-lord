//! Application workflows shared by desktop, CLI, and future automation clients.

use vmlord_core::{
    Diagnostic, DiagnosticLevel, RepositoryError, VmCreateRequest, VmRepository, VmSummary,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendStatus {
    Starting,
    Ready,
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmAction {
    Create,
    Start,
    Stop,
    ForceStop,
    Connect,
    Ssh,
    Edit,
    Delete,
}

impl VmAction {
    const fn label(self) -> &'static str {
        match self {
            Self::Create => "Create VM",
            Self::Start => "Start",
            Self::Stop => "Stop",
            Self::ForceStop => "Force stop",
            Self::Connect => "Connect",
            Self::Ssh => "SSH",
            Self::Edit => "Edit",
            Self::Delete => "Delete",
        }
    }
}

pub trait ImagePicker {
    fn pick_iso_image(&mut self) -> Result<Option<String>, RepositoryError>;
}

pub struct WorkspaceApp {
    repository: Box<dyn VmRepository>,
    image_picker: Option<Box<dyn ImagePicker>>,
    status: BackendStatus,
    vms: Vec<VmSummary>,
    diagnostics: Vec<Diagnostic>,
}

impl WorkspaceApp {
    #[must_use]
    pub fn new(repository: Box<dyn VmRepository>) -> Self {
        Self {
            repository,
            image_picker: None,
            status: BackendStatus::Starting,
            vms: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_image_picker(mut self, image_picker: Box<dyn ImagePicker>) -> Self {
        self.image_picker = Some(image_picker);
        self
    }

    pub fn pick_iso_image(&mut self) -> Result<Option<String>, RepositoryError> {
        let Some(image_picker) = &mut self.image_picker else {
            return Err(RepositoryError::new(
                "the native image picker is not available",
            ));
        };
        image_picker.pick_iso_image()
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

    pub fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError> {
        if !matches!(self.status, BackendStatus::Ready) {
            let error = RepositoryError::new("VM creation requires a ready backend");
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: error.to_string(),
            });
            return Err(error);
        }

        match self.repository.create_vm(request) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: "VM creation accepted".into(),
                });
                self.refresh();
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to create VM: {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
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

    pub fn log_vm_action(&mut self, action: VmAction) {
        self.diagnostics.push(Diagnostic {
            level: vmlord_core::DiagnosticLevel::Info,
            message: format!("{} pressed", action.label()),
        });
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

    fn create_vm(&mut self, _request: VmCreateRequest) -> Result<(), RepositoryError> {
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
        create_should_fail: bool,
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

        fn create_vm(&mut self, _request: VmCreateRequest) -> Result<(), RepositoryError> {
            if self.create_should_fail {
                Err(RepositoryError::new("creation failed"))
            } else {
                Ok(())
            }
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
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
        }));
        app.start();
        assert_eq!(app.status(), &BackendStatus::Ready);
        assert_eq!(app.vms().len(), 1);
        assert_eq!(app.diagnostics().len(), 1);
    }

    #[test]
    fn initialization_error_is_visible() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: true,
            create_should_fail: false,
        }));
        app.start();
        assert_eq!(
            app.status(),
            &BackendStatus::Unavailable("unavailable".into())
        );
    }

    #[test]
    fn create_vm_is_available_to_ui_clients() {
        let _: fn(&mut WorkspaceApp, VmCreateRequest) -> Result<(), RepositoryError> =
            WorkspaceApp::create_vm;
    }

    #[test]
    fn vm_action_is_logged() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
        }));

        app.log_vm_action(VmAction::Create);

        assert_eq!(app.diagnostics()[0].message, "Create VM pressed");
    }
}
