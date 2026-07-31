//! Application workflows shared by desktop, CLI, and future automation clients.

use std::fmt;

use vmlord_core::{
    AppSettings, Diagnostic, DiagnosticLevel, RepositoryError, SettingsError, SettingsStore,
    VmCreateRequest, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendStatus {
    Starting,
    Ready,
    Unavailable(String),
}

#[derive(Debug)]
pub enum SettingsUpdateError {
    NotInitialized,
    Save(SettingsError),
}

impl fmt::Display for SettingsUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized => formatter.write_str("application settings are not initialized"),
            Self::Save(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SettingsUpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotInitialized => None,
            Self::Save(error) => Some(error),
        }
    }
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

pub trait SettingsPathPicker {
    fn pick_vm_storage_directory(&mut self) -> Result<Option<String>, RepositoryError>;
    fn pick_log_file(&mut self) -> Result<Option<String>, RepositoryError>;
}

pub struct WorkspaceApp {
    repository: Box<dyn VmRepository>,
    image_picker: Option<Box<dyn ImagePicker>>,
    settings_path_picker: Option<Box<dyn SettingsPathPicker>>,
    settings: Option<SettingsContext>,
    status: BackendStatus,
    vms: Vec<VmSummary>,
    diagnostics: Vec<Diagnostic>,
}

struct SettingsContext {
    store: SettingsStore,
    current: AppSettings,
}

impl WorkspaceApp {
    #[must_use]
    pub fn new(repository: Box<dyn VmRepository>) -> Self {
        Self {
            repository,
            image_picker: None,
            settings_path_picker: None,
            settings: None,
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

    #[must_use]
    pub fn with_settings_path_picker(mut self, picker: Box<dyn SettingsPathPicker>) -> Self {
        self.settings_path_picker = Some(picker);
        self
    }

    #[must_use]
    pub fn with_settings(mut self, store: SettingsStore, settings: AppSettings) -> Self {
        self.settings = Some(SettingsContext {
            store,
            current: settings,
        });
        self
    }

    #[must_use]
    pub fn settings(&self) -> Option<&AppSettings> {
        self.settings.as_ref().map(|settings| &settings.current)
    }

    pub fn update_settings(&mut self, settings: AppSettings) -> Result<(), SettingsUpdateError> {
        let Some(context) = &mut self.settings else {
            return Err(SettingsUpdateError::NotInitialized);
        };
        context
            .store
            .save(&settings)
            .map_err(SettingsUpdateError::Save)?;
        log::info!(
            "application settings saved; log file is {} and level is {:?}",
            settings.log_file_path.display(),
            settings.log_level
        );
        context.current = settings;
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Info,
            message: "Application settings saved".into(),
        });
        Ok(())
    }

    pub fn pick_vm_storage_directory(&mut self) -> Result<Option<String>, RepositoryError> {
        let Some(picker) = &mut self.settings_path_picker else {
            return Err(RepositoryError::new(
                "the native directory picker is not available",
            ));
        };
        picker.pick_vm_storage_directory()
    }

    pub fn pick_log_file(&mut self) -> Result<Option<String>, RepositoryError> {
        let Some(picker) = &mut self.settings_path_picker else {
            return Err(RepositoryError::new(
                "the native log file picker is not available",
            ));
        };
        picker.pick_log_file()
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
        log::info!("initializing VM backend");
        match self.repository.initialize() {
            Ok(()) => {
                log::info!("VM backend initialized");
                self.status = BackendStatus::Ready;
                self.refresh();
                return;
            }
            Err(error) => {
                log::error!("failed to initialize VM backend: {error}");
                self.status = BackendStatus::Unavailable(error.to_string());
            }
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
        self.require_ready_backend("VM creation")?;

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

    pub fn update_vm(&mut self, request: VmUpdateRequest) -> Result<(), RepositoryError> {
        self.require_ready_backend("VM update")?;

        let vm_state = self
            .vms
            .iter()
            .find(|vm| vm.name == request.name)
            .map(|vm| vm.state)
            .ok_or_else(|| {
                RepositoryError::new(format!("VM \"{}\" was not found", request.name))
            })?;
        if !matches!(vm_state, VmState::Stopped) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" must be stopped before it can be edited",
                request.name
            ));
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: error.to_string(),
            });
            return Err(error);
        }
        if request.ram_mb < 512 || request.ram_mb % 2 != 0 {
            let error = RepositoryError::new(
                "RAM must be 2 MiB-aligned and at least 512 MiB for VM updates",
            );
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: error.to_string(),
            });
            return Err(error);
        }
        if request.cpu_cores == 0 {
            let error = RepositoryError::new("CPU cores must be at least 1 for VM updates");
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: error.to_string(),
            });
            return Err(error);
        }

        let name = request.name.clone();
        match self.repository.update_vm(request) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("VM \"{name}\" update accepted"),
                });
                self.refresh();
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to update VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    pub fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.run_vm_lifecycle_action(name, "start", |repository| repository.start_vm(name))
    }

    pub fn stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.run_vm_lifecycle_action(name, "stop", |repository| repository.stop_vm(name))
    }

    pub fn force_stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.run_vm_lifecycle_action(name, "force stop", |repository| {
            repository.force_stop_vm(name)
        })
    }

    pub fn connect_display(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("display connection")?;

        match self.repository.open_display(name) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("Display for VM \"{name}\" opened"),
                });
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to open display for VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    pub fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("SSH connection")?;

        match self.repository.open_ssh(name) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("SSH session for VM \"{name}\" started"),
                });
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to open SSH session for VM \"{name}\": {error}"),
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

    fn require_ready_backend(&mut self, action: &str) -> Result<(), RepositoryError> {
        if matches!(self.status, BackendStatus::Ready) {
            return Ok(());
        }

        let error = RepositoryError::new(format!("{action} requires a ready backend"));
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Error,
            message: error.to_string(),
        });
        Err(error)
    }

    fn run_vm_lifecycle_action(
        &mut self,
        name: &str,
        action: &str,
        operation: impl FnOnce(&mut dyn VmRepository) -> Result<(), RepositoryError>,
    ) -> Result<(), RepositoryError> {
        self.require_ready_backend(&format!("VM {action}"))?;
        log::info!("requesting VM {action} for {name}");

        match operation(self.repository.as_mut()) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("VM \"{name}\" {action} request accepted"),
                });
                self.refresh();
                Ok(())
            }
            Err(error) => {
                log::error!("failed to {action} VM {name}: {error}");
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to {action} VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
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

    fn update_vm(&mut self, _request: VmUpdateRequest) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }

    fn start_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }

    fn stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }

    fn force_stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }

    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use vmlord_core::{DiagnosticLevel, Language, LogLevel, VmState};

    struct FakeRepository {
        should_fail: bool,
        create_should_fail: bool,
        actions: Vec<String>,
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

        fn update_vm(&mut self, request: VmUpdateRequest) -> Result<(), RepositoryError> {
            self.actions.push(format!(
                "update:{}:{}:{}:{:?}:{:?}",
                request.name,
                request.ram_mb,
                request.cpu_cores,
                request.gpu_mode,
                request.network_mode
            ));
            Ok(())
        }

        fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("start:{name}"));
            Ok(())
        }

        fn stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("stop:{name}"));
            Ok(())
        }

        fn force_stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("force-stop:{name}"));
            Ok(())
        }

        fn open_display(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("display:{name}"));
            Ok(())
        }

        fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("ssh:{name}"));
            Ok(())
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
            actions: Vec::new(),
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
            actions: Vec::new(),
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
    fn update_vm_is_available_to_ui_clients() {
        let _: fn(&mut WorkspaceApp, VmUpdateRequest) -> Result<(), RepositoryError> =
            WorkspaceApp::update_vm;
    }

    #[test]
    fn lifecycle_actions_are_available_to_ui_clients() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            actions: Vec::new(),
        }));
        app.start();

        app.start_vm("dev").unwrap();
        app.stop_vm("dev").unwrap();
        app.force_stop_vm("dev").unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "VM \"dev\" start request accepted")
        );
        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "VM \"dev\" stop request accepted")
        );
        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.message == "VM \"dev\" force stop request accepted"
            })
        );
    }

    #[test]
    fn opens_display_through_repository() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            actions: Vec::new(),
        }));
        app.start();

        app.connect_display("dev").unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "Display for VM \"dev\" opened")
        );
    }

    #[test]
    fn opens_ssh_session_through_repository() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            actions: Vec::new(),
        }));
        app.start();

        app.open_ssh("dev").unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "SSH session for VM \"dev\" started")
        );
    }

    #[test]
    fn updates_and_persists_application_settings() {
        let directory = std::env::temp_dir().join(format!(
            "vmlord-app-settings-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = SettingsStore::new(directory.join("settings.toml"));
        let initial_settings = AppSettings {
            vm_storage_path: directory.join("vms"),
            language: Language::EnUs,
            log_file_path: directory.join("logs").join("vmlord.log"),
            log_level: LogLevel::Info,
        };
        let updated_settings = AppSettings {
            vm_storage_path: directory.join("virtual-machines"),
            language: Language::EnUs,
            log_file_path: directory.join("diagnostics").join("application.log"),
            log_level: LogLevel::Debug,
        };
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            actions: Vec::new(),
        }))
        .with_settings(store.clone(), initial_settings);

        app.update_settings(updated_settings.clone()).unwrap();

        assert_eq!(app.settings(), Some(&updated_settings));
        assert_eq!(store.load_or_create().unwrap(), updated_settings);
        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "Application settings saved")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn vm_action_is_logged() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            actions: Vec::new(),
        }));

        app.log_vm_action(VmAction::Create);

        assert_eq!(app.diagnostics()[0].message, "Create VM pressed");
    }
}
