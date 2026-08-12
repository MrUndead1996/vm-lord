//! Application workflows shared by desktop, CLI, and future automation clients.

use std::{fmt, path::PathBuf};

use vmlord_core::{
    AppSettings, Diagnostic, DiagnosticLevel, GuestDefaults, RepositoryError, SettingsError,
    SettingsStore, VmCreateRequest, VmDeleteRequest, VmRepository, VmState, VmSummary,
    VmUpdateRequest,
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
    /// Stop building a VM that is still being created.
    CancelCreate,
    Connect,
    Ssh,
    /// Reopen the serial console of a running VM.
    Console,
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
            Self::CancelCreate => "Cancel creation",
            Self::Connect => "Connect",
            // What it opens, not where: which terminal host ends up showing
            // the session is decided when it is launched.
            Self::Ssh => "Open SSH",
            Self::Console => "Open COM port",
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
    guest_defaults: GuestDefaults,
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
            guest_defaults: GuestDefaults::default(),
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

    /// Sets what a new VM's locale, keyboard layout and timezone start out as.
    ///
    /// Reading them from Windows is the composition root's job (#60); without
    /// that call the application offers [`GuestDefaults::default`], which is a
    /// guest that boots rather than a guest with no settings at all.
    #[must_use]
    pub fn with_guest_defaults(mut self, guest_defaults: GuestDefaults) -> Self {
        self.guest_defaults = guest_defaults;
        self
    }

    /// What a create form fills its guest settings with before anyone edits
    /// them.
    #[must_use]
    pub fn guest_defaults(&self) -> &GuestDefaults {
        &self.guest_defaults
    }

    /// Where the private half of the key pair VMLord generates for `name` is
    /// written.
    ///
    /// Answered by the backend rather than composed here: the on-disk layout of
    /// a VM is the platform layer's to decide, and the create form has to be
    /// able to show the path before the VM -- and therefore the file -- exists.
    #[must_use]
    pub fn ssh_key_path(&self, name: &str) -> Option<PathBuf> {
        self.repository.ssh_key_path(name)
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
        if request.ram_mb < 512 || !request.ram_mb.is_multiple_of(2) {
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

        // A VM keeps the configuration it booted with, so an edit made while
        // it runs only takes effect on its next start. The edit itself is
        // allowed: refusing it would force a stop just to change a setting.
        let applies_after_restart = !matches!(vm_state, VmState::Stopped);

        let name = request.name.clone();
        match self.repository.update_vm(request) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("VM \"{name}\" update accepted"),
                });
                if applies_after_restart {
                    self.diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Warning,
                        message: format!(
                            "VM \"{name}\" is running; the new configuration applies after a restart"
                        ),
                    });
                }
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

    /// Deletes a VM and every resource VMLord created for it.
    ///
    /// A VM that is not stopped is refused here rather than stopped
    /// automatically: deletion cannot be undone, so ending a running guest is a
    /// decision the user makes on purpose. The repository checks this again
    /// against HCS itself, because this list is a cache and can be stale.
    pub fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError> {
        self.require_ready_backend("VM deletion")?;

        let vm_state = self
            .vms
            .iter()
            .find(|vm| vm.name == request.name)
            .map(|vm| vm.state)
            .ok_or_else(|| {
                let error = RepositoryError::new(format!("VM \"{}\" was not found", request.name));
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: error.to_string(),
                });
                error
            })?;
        if !matches!(vm_state, VmState::Stopped) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is running; stop it before deleting it",
                request.name
            ));
            log::error!("{error}");
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: error.to_string(),
            });
            return Err(error);
        }

        let name = request.name.clone();
        let kept_disks = !request.delete_disks;
        log::info!("requesting deletion of VM {name}");

        match self.repository.delete_vm(request) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("VM \"{name}\" deleted"),
                });
                if kept_disks {
                    self.diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Warning,
                        message: format!(
                            "The disks of VM \"{name}\" were kept; its directory still exists \
                             and a new VM cannot reuse that name until it is removed"
                        ),
                    });
                }
                self.refresh();
                Ok(())
            }
            Err(error) => {
                log::error!("failed to delete VM {name}: {error}");
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to delete VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    /// Asks the backend to stop creating a VM.
    ///
    /// The build rolls itself back and leaves the list on its own, so there is
    /// nothing to refresh here: the next refresh is a second away and will
    /// find whatever the build made of the request.
    pub fn cancel_create(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("cancelling VM creation")?;

        match self.repository.cancel_create(name) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("Cancelling the creation of VM \"{name}\""),
                });
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to cancel the creation of VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
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

    /// Reopens the serial console of a running VM.
    ///
    /// The console opens by itself when a VM starts; this is how it comes back
    /// once its window has been closed, which is the only way into a guest
    /// that cannot be reached over the network.
    pub fn open_console(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("opening the COM port")?;

        match self.repository.open_console(name) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("COM port console for VM \"{name}\" opened"),
                });
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to open the COM port of VM \"{name}\": {error}"),
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

    fn delete_vm(&mut self, _request: VmDeleteRequest) -> Result<(), RepositoryError> {
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
        vm_is_running: bool,
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
                state: if self.vm_is_running {
                    VmState::Running {
                        agent_status: vmlord_core::AgentStatus::Unknown,
                    }
                } else {
                    VmState::Stopped
                },
                ram_mb: 4096,
                disk_gb: 64,
                cpu_cores: 4,
                gpu_mode: vmlord_core::GpuMode::None,
                network_mode: vmlord_core::NetworkMode::Nat,
                ip_address: None,
                ssh: vmlord_core::SshAvailability::Enabled(vmlord_core::SshConfig {
                    username: "user".into(),
                    port: vmlord_core::SshPort::DEFAULT,
                    authentication: vmlord_core::SshAuthentication::VmlordKey,
                }),
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

        fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError> {
            self.actions
                .push(format!("delete:{}:{}", request.name, request.delete_disks));
            Ok(())
        }

        fn ssh_key_path(&self, name: &str) -> Option<PathBuf> {
            Some(PathBuf::from("/vms").join(name).join("id_ed25519"))
        }

        fn open_display(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("display:{name}"));
            Ok(())
        }

        fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("ssh:{name}"));
            if !self.vm_is_running {
                return Err(RepositoryError::new(format!(
                    "cannot open an SSH session to VM \"{name}\": the guest does not \
                     answer on port 22: connection refused"
                )));
            }
            Ok(())
        }

        fn open_console(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("console:{name}"));
            if !self.vm_is_running {
                return Err(RepositoryError::new(format!(
                    "VM \"{name}\" is stopped, so it has no COM1 port to open"
                )));
            }
            Ok(())
        }

        fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
            vec![Diagnostic {
                level: DiagnosticLevel::Info,
                message: "ready".into(),
            }]
        }
    }

    /// The button that calls this arrives with #65; the contract arrives here,
    /// so that adding the button is adding a button.
    #[test]
    fn cancelling_a_creation_a_backend_cannot_cancel_is_reported() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));
        app.start();

        let error = app
            .cancel_create("dev")
            .expect_err("the fake backend inherits the trait's refusal");

        assert!(!error.to_string().is_empty());
        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Error && diagnostic.message.contains("dev")
            }),
            "the user has to be told the cancellation did not happen"
        );
    }

    /// The create form fills three fields from this, and it is the composition
    /// root -- not the form -- that knows what the host says (#60).
    #[test]
    fn guest_defaults_are_offered_and_can_be_replaced_by_the_composition_root() {
        let app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));

        assert_eq!(app.guest_defaults(), &GuestDefaults::default());

        let from_host = GuestDefaults {
            locale: "ru_RU.UTF-8".into(),
            keyboard: "ru".into(),
            timezone: "Europe/Moscow".into(),
        };
        let app = app.with_guest_defaults(from_host.clone());

        assert_eq!(app.guest_defaults(), &from_host);
    }

    /// Where a VM's key pair goes is the backend's to say, and the form shows
    /// it before the VM -- and therefore the file -- exists.
    #[test]
    fn the_key_path_of_a_vm_comes_from_the_backend() {
        let app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));

        assert_eq!(
            app.ssh_key_path("dev"),
            Some(PathBuf::from("/vms").join("dev").join("id_ed25519"))
        );
        assert_eq!(
            WorkspaceApp::new(unavailable_repository("no backend")).ssh_key_path("dev"),
            None,
            "a backend that gives VMs no keys of their own answers nothing"
        );
    }

    #[test]
    fn start_loads_vm_list() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
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
            vm_is_running: false,
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
            vm_is_running: false,
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
            vm_is_running: false,
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
            vm_is_running: true,
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

    /// The preflight check that stopped a session -- a missing Windows feature,
    /// a port that did not answer, a key that is gone -- is the only account of
    /// it anyone gets: once the terminal is up, everything else OpenSSH has to
    /// say goes into that window. So it reaches the diagnostics whole, rather
    /// than as a failure this layer describes in its own words.
    #[test]
    fn a_refused_ssh_session_is_reported_with_the_backends_own_reason() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));
        app.start();

        let error = app.open_ssh("dev").unwrap_err();

        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Error
                    && diagnostic.message
                        == format!("Failed to open SSH session for VM \"dev\": {error}")
                    && diagnostic.message.contains("does not answer on port 22")
            }),
            "{:?}",
            app.diagnostics()
        );
    }

    #[test]
    fn opens_the_com_port_through_the_repository() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: true,
            actions: Vec::new(),
        }));
        app.start();

        app.open_console("dev").unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "COM port console for VM \"dev\" opened")
        );
    }

    /// A refusal -- a stopped VM, or a console that is already open -- is the
    /// backend's message to pass on, not one to invent here.
    #[test]
    fn a_refused_com_port_is_reported_with_the_reason() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));
        app.start();

        app.open_console("dev").unwrap_err();

        assert!(
            app.diagnostics().iter().any(|diagnostic| diagnostic.message
                == "Failed to open the COM port of VM \"dev\": VM \"dev\" is stopped, \
                    so it has no COM1 port to open"),
            "{:?}",
            app.diagnostics()
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
            image_cache_path: directory.join("images"),
            guest_readiness: vmlord_core::GuestReadinessTimeouts::default(),
        };
        let updated_settings = AppSettings {
            vm_storage_path: directory.join("virtual-machines"),
            language: Language::EnUs,
            log_file_path: directory.join("diagnostics").join("application.log"),
            log_level: LogLevel::Debug,
            image_cache_path: directory.join("cached-images"),
            guest_readiness: vmlord_core::GuestReadinessTimeouts::default(),
        };
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
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
            vm_is_running: false,
            actions: Vec::new(),
        }));

        app.log_vm_action(VmAction::Create);

        assert_eq!(app.diagnostics()[0].message, "Create VM pressed");
    }

    fn app_with(vm_is_running: bool) -> WorkspaceApp {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running,
            actions: Vec::new(),
        }));
        app.start();
        app
    }

    fn delete_request(delete_disks: bool) -> VmDeleteRequest {
        VmDeleteRequest {
            name: "dev".into(),
            delete_disks,
        }
    }

    #[test]
    fn deletes_a_stopped_vm_through_the_repository() {
        let mut app = app_with(false);

        app.delete_vm(delete_request(true)).unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "VM \"dev\" deleted")
        );
    }

    #[test]
    fn refuses_to_delete_a_running_vm() {
        let mut app = app_with(true);

        let error = app
            .delete_vm(delete_request(true))
            .expect_err("a running VM must not be deleted");

        assert!(error.to_string().contains("running"));
        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
        );
    }

    #[test]
    fn refuses_to_delete_an_unknown_vm() {
        let mut app = app_with(false);

        let error = app
            .delete_vm(VmDeleteRequest {
                name: "missing-vm".into(),
                delete_disks: true,
            })
            .expect_err("an unknown VM must not be deleted");

        assert!(error.to_string().contains("missing-vm"));
    }

    #[test]
    fn refuses_to_delete_without_a_ready_backend() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: true,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));
        app.start();

        let error = app
            .delete_vm(delete_request(true))
            .expect_err("deletion needs a ready backend");

        assert!(error.to_string().contains("ready backend"));
    }

    #[test]
    fn warns_when_the_disks_are_kept() {
        let mut app = app_with(false);

        app.delete_vm(delete_request(false)).unwrap();

        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Warning && diagnostic.message.contains("disks")
            }),
            "keeping the disks leaves the VM directory behind and the user must be told"
        );
    }

    #[test]
    fn deleting_with_the_disks_does_not_warn_about_them() {
        let mut app = app_with(false);

        app.delete_vm(delete_request(true)).unwrap();

        assert!(!app.diagnostics().iter().any(|diagnostic| {
            diagnostic.level == DiagnosticLevel::Warning && diagnostic.message.contains("disks")
        }));
    }
}
