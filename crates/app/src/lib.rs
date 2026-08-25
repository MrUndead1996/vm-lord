//! Application workflows shared by desktop, CLI, and future automation clients.

pub mod display;
pub mod gpu;

use std::{collections::HashMap, fmt, path::PathBuf, time::SystemTime};

use vmlord_core::{
    AppSettings, Diagnostic, DiagnosticsSink, GuestDefaults, HostGpuCapabilities, RepositoryError,
    SettingsError, SettingsStore, Subsystem, VmCreateRequest, VmDeleteRequest, VmDisplayStatus,
    VmGpuStatus, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

pub use display::derive_status as derive_display_status;
pub use gpu::derive_status as derive_gpu_status;

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
    /// Move a running VM's display payload to a newer version.
    UpdateDisplay,
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
            Self::UpdateDisplay => "Update display",
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
    /// What GPU-PV is doing for each listed VM, keyed by VM name.
    ///
    /// Derived once per refresh rather than on every read, so that a status
    /// keeps the time its facts were taken instead of ageing forward under a
    /// UI that redraws sixty times a second.
    gpu_status: HashMap<String, VmGpuStatus>,
    /// What the display stack is doing for each listed VM, keyed by VM name.
    ///
    /// Derived once per refresh for the same reason `gpu_status` is.
    display_status: HashMap<String, VmDisplayStatus>,
    /// What this host can do for GPU-PV, read once when the backend comes up.
    ///
    /// `None` is a backend that could not answer, which is not the same as a
    /// host that cannot do it, and the two must not read the same way to a
    /// person choosing a GPU mode.
    ///
    /// Not re-read on refresh: the read walks SetupAPI and the filesystem, a
    /// form redraws sixty times a second, and a host does not change between
    /// two openings of a dialog.
    host_gpu: Option<HostGpuCapabilities>,
    /// The records already read out of the sink.
    ///
    /// Held rather than shown straight from the sink because the panel is a
    /// history and `take` empties what it returns.
    diagnostics: Vec<Diagnostic>,
    /// Where the diagnostics layer leaves records, when one was installed.
    ///
    /// `None` in tests and in a process that brought no panel up: a
    /// `WorkspaceApp` without a sink simply has nothing to read.
    sink: Option<DiagnosticsSink>,
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
            gpu_status: HashMap::new(),
            display_status: HashMap::new(),
            host_gpu: None,
            diagnostics: Vec::new(),
            sink: None,
        }
    }

    /// Reads the panel's records from `sink`.
    ///
    /// Given rather than made here: the sink belongs to the subscriber the
    /// composition root installed, and there is exactly one of it.
    #[must_use]
    pub fn with_diagnostics(mut self, sink: DiagnosticsSink) -> Self {
        self.sink = Some(sink);
        self
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
        tracing::info!(
            "application settings saved; log file is {} and level is {:?}",
            settings.log_file_path.display(),
            settings.log_level
        );
        context.current = settings;
        vmlord_core::diagnostic!(Info, Subsystem::App, "Application settings saved");
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
        tracing::info!("initializing VM backend");
        match self.repository.initialize() {
            Ok(()) => {
                tracing::info!("VM backend initialized");
                self.status = BackendStatus::Ready;
                self.host_gpu = self.read_host_gpu();
                self.refresh();
                return;
            }
            Err(error) => {
                tracing::error!("failed to initialize VM backend: {error}");
                self.status = BackendStatus::Unavailable(error.to_string());
            }
        }
        self.collect_diagnostics();
    }

    /// What the host can do for GPU-PV, as the backend reported it at startup.
    ///
    /// `None` means nobody could be asked -- a backend that does not implement
    /// the call at all. Claiming a GPU is unavailable where we merely could not
    /// ask would be a different answer, and the wrong one.
    #[must_use]
    pub fn host_gpu_capabilities(&self) -> Option<&HostGpuCapabilities> {
        self.host_gpu.as_ref()
    }

    fn read_host_gpu(&self) -> Option<HostGpuCapabilities> {
        match self.repository.host_gpu_capabilities() {
            Ok(capabilities) => Some(capabilities),
            Err(error) => {
                tracing::info!("this backend does not report host GPU capabilities: {error}");
                None
            }
        }
    }

    pub fn refresh(&mut self) {
        if !matches!(self.status, BackendStatus::Ready) {
            return;
        }

        match self.repository.list_vms() {
            Ok(vms) => {
                let now = SystemTime::now();
                self.gpu_status = vms
                    .iter()
                    .map(|vm| {
                        (
                            vm.name.clone(),
                            gpu::derive_status(vm.gpu_mode, vm.state, &vm.gpu, now),
                        )
                    })
                    .collect();
                self.display_status = vms
                    .iter()
                    .map(|vm| {
                        (
                            vm.name.clone(),
                            display::derive_status(
                                vm.desktop_profile,
                                &vm.display_provisioning,
                                vm.state,
                                &vm.display,
                                now,
                            ),
                        )
                    })
                    .collect();
                self.vms = vms;
            }
            Err(error) => self.status = BackendStatus::Unavailable(error.to_string()),
        }
        self.collect_diagnostics();
    }

    pub fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError> {
        self.require_ready_backend("VM creation")?;

        match self.repository.create_vm(request) {
            Ok(()) => {
                vmlord_core::diagnostic!(Info, Subsystem::Hcs, "VM creation accepted");
                self.refresh();
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Hcs,
                    code = error.code().unwrap_or_default(),
                    "Failed to create VM: {error}"
                );
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
            vmlord_core::diagnostic!(Error, Subsystem::App, "{error}");
            return Err(error);
        }
        if request.cpu_cores == 0 {
            let error = RepositoryError::new("CPU cores must be at least 1 for VM updates");
            vmlord_core::diagnostic!(Error, Subsystem::App, "{error}");
            return Err(error);
        }

        // A VM keeps the configuration it booted with, so an edit made while
        // it runs only takes effect on its next start. The edit itself is
        // allowed: refusing it would force a stop just to change a setting.
        let applies_after_restart = !matches!(vm_state, VmState::Stopped);

        let name = request.name.clone();
        match self.repository.update_vm(request) {
            Ok(()) => {
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Hcs,
                    vm = name,
                    "VM \"{name}\" update accepted"
                );
                if applies_after_restart {
                    vmlord_core::diagnostic!(
                        Warning,
                        Subsystem::Hcs,
                        vm = name,
                        "VM \"{name}\" is running; the new configuration applies after a restart"
                    );
                }
                self.refresh();
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Hcs,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to update VM \"{name}\": {error}"
                );
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
                vmlord_core::diagnostic!(Error, Subsystem::App, "{error}");
                error
            })?;
        if !matches!(vm_state, VmState::Stopped) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is running; stop it before deleting it",
                request.name
            ));
            tracing::error!("{error}");
            vmlord_core::diagnostic!(Error, Subsystem::App, "{error}");
            return Err(error);
        }

        let name = request.name.clone();
        let kept_disks = !request.delete_disks;
        tracing::info!("requesting deletion of VM {name}");

        match self.repository.delete_vm(request) {
            Ok(()) => {
                vmlord_core::diagnostic!(Info, Subsystem::Hcs, vm = name, "VM \"{name}\" deleted");
                if kept_disks {
                    vmlord_core::diagnostic!(
                        Warning,
                        Subsystem::Hcs,
                        vm = name,
                        "The disks of VM \"{name}\" were kept; its directory still exists \
                         and a new VM cannot reuse that name until it is removed"
                    );
                }
                self.refresh();
                Ok(())
            }
            Err(error) => {
                tracing::error!("failed to delete VM {name}: {error}");
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Hcs,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to delete VM \"{name}\": {error}"
                );
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
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Hcs,
                    vm = name,
                    "Cancelling the creation of VM \"{name}\""
                );
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Hcs,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to cancel the creation of VM \"{name}\": {error}"
                );
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    pub fn connect_display(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("display connection")?;

        match self.repository.open_display(name) {
            Ok(()) => {
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Display,
                    vm = name,
                    "Display for VM \"{name}\" opened"
                );
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Display,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to open display for VM \"{name}\": {error}"
                );
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    /// Asks for a running VM's display payload to be moved to the newest
    /// version this build carries for it.
    ///
    /// `Ok` is a request the backend accepted, not a payload that moved: the
    /// guest builds a kernel module with DKMS to answer, which is minutes, and
    /// a window that redraws sixty times a second cannot wait on one. The VM
    /// reports itself as updating while that runs, and how it ended arrives in
    /// the diagnostics from the backend -- including the guest that could not
    /// verify the new version and brought the previous one back, which is a
    /// working display and a failed update.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] when there is nobody to ask -- a VM that is not
    /// running, one with no agent session, one already being updated.
    pub fn update_display_payload(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("display payload update")?;
        self.log_vm_action(VmAction::UpdateDisplay);

        match self.repository.update_display_payload(name) {
            Ok(()) => {
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Display,
                    vm = name,
                    "Updating the display payload of VM \"{name}\""
                );
                // Refreshed so that the VM shows as updating from this click
                // rather than from the next tick: what the button does next is
                // decided by the status this derives.
                self.refresh();
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Display,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to update the display payload of VM \"{name}\": {error}"
                );
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    /// Asks for an interactive SSH session into a running guest.
    ///
    /// Both outcomes are collected from the backend, which is what makes this
    /// different from the other actions here. A session that opened says
    /// nothing to VMLord afterwards -- it is a process in a window of its own --
    /// so the command it was opened with is the only account of it there will
    /// be, and it belongs in the log beside the failures.
    ///
    /// What comes back here is only whether the request was accepted: reaching
    /// the guest takes seconds the UI's thread cannot spend, so the backend
    /// does it on its own and reports what happened into the same diagnostics.
    pub fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("SSH connection")?;

        match self.repository.open_ssh(name) {
            Ok(()) => {
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Network,
                    vm = name,
                    "Opening an SSH session for VM \"{name}\""
                );
                self.collect_diagnostics();
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Network,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to open SSH session for VM \"{name}\": {error}"
                );
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
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Display,
                    vm = name,
                    "COM port console for VM \"{name}\" opened"
                );
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Display,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to open the COM port of VM \"{name}\": {error}"
                );
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

    /// What GPU-PV was doing for `vm_name` as of the last refresh.
    ///
    /// `None` for a name the last listing did not contain: a VM VMLord does
    /// not know about has no GPU to report on, and answering "disabled" would
    /// make a vanished VM look like a configured one.
    #[must_use]
    pub fn gpu_status(&self, vm_name: &str) -> Option<&VmGpuStatus> {
        self.gpu_status.get(vm_name)
    }

    /// What the display stack is doing for one VM.
    ///
    /// `None` for a name the last listing did not contain, for the same reason
    /// [`Self::gpu_status`] answers `None` for one.
    #[must_use]
    pub fn display_status(&self, vm_name: &str) -> Option<&VmDisplayStatus> {
        self.display_status.get(vm_name)
    }

    #[must_use]
    /// The panel's records, newest last.
    ///
    /// Takes `&mut self` because it reads the sink first: a record written a
    /// moment ago -- the answer to the click that is being drawn -- would
    /// otherwise not appear until the next refresh, which is a whole second of
    /// a button that looks like it did nothing.
    pub fn diagnostics(&mut self) -> &[Diagnostic] {
        self.read_records();
        &self.diagnostics
    }

    pub fn log_vm_action(&mut self, action: VmAction) {
        vmlord_core::diagnostic!(Info, Subsystem::App, "{} pressed", action.label());
    }

    fn require_ready_backend(&mut self, action: &str) -> Result<(), RepositoryError> {
        if matches!(self.status, BackendStatus::Ready) {
            return Ok(());
        }

        let error = RepositoryError::new(format!("{action} requires a ready backend"));
        vmlord_core::diagnostic!(Error, Subsystem::App, "{error}");
        Err(error)
    }

    fn run_vm_lifecycle_action(
        &mut self,
        name: &str,
        action: &str,
        operation: impl FnOnce(&mut dyn VmRepository) -> Result<(), RepositoryError>,
    ) -> Result<(), RepositoryError> {
        self.require_ready_backend(&format!("VM {action}"))?;
        tracing::info!("requesting VM {action} for {name}");

        match operation(self.repository.as_mut()) {
            Ok(()) => {
                vmlord_core::diagnostic!(
                    Info,
                    Subsystem::Hcs,
                    vm = name,
                    "VM \"{name}\" {action} request accepted"
                );
                self.refresh();
                Ok(())
            }
            Err(error) => {
                vmlord_core::diagnostic!(
                    Error,
                    Subsystem::Hcs,
                    vm = name,
                    code = error.code().unwrap_or_default(),
                    "Failed to {action} VM \"{name}\": {error}"
                );
                self.collect_diagnostics();
                Err(error)
            }
        }
    }

    /// Reaps what the backend has finished, then reads what was recorded.
    ///
    /// The reaping comes first and happens whether or not there is a sink:
    /// `refresh` is where finished builds and starts are adopted and answered
    /// shutdowns give up their handles, and skipping it would leak them. The
    /// records are the second half, and a `WorkspaceApp` without a panel has
    /// none to read.
    fn collect_diagnostics(&mut self) {
        self.repository.refresh();
        self.read_records();
    }

    /// Moves whatever the layer has recorded into the panel's history.
    ///
    /// Separate from `collect_diagnostics` because reading records is cheap and
    /// reaping is not: the panel is drawn sixty times a second, and doing the
    /// backend's housekeeping that often would be absurd.
    fn read_records(&mut self) {
        let Some(sink) = &self.sink else {
            return;
        };
        self.diagnostics.extend(sink.take());
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

    fn refresh(&mut self) {}
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use vmlord_core::{DiagnosticLevel, HostGpuCapabilities, Language, LogLevel, VmState};

    /// A backend that works: every test names only what it changes about it.
    #[derive(Default)]
    struct FakeRepository {
        should_fail: bool,
        create_should_fail: bool,
        vm_is_running: bool,
        /// What the VM asks of the GPU, and what the backend saw of it.
        gpu_mode: vmlord_core::GpuMode,
        gpu: vmlord_core::VmGpuFacts,
        /// The desktop the VM was created with, and how far installing it got.
        desktop_profile: vmlord_core::DesktopProfile,
        display_provisioning: vmlord_core::DisplayProvisioning,
        /// What the backend has observed of this VM's display, which an update
        /// rewrites the way the native backend does.
        display: vmlord_core::VmDisplayFacts,
        /// Whether this backend can answer for the host at all, and how often
        /// it has been asked.
        reports_host_gpu: bool,
        host_gpu_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        actions: Vec<String>,
    }

    impl VmRepository for FakeRepository {
        fn host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError> {
            self.host_gpu_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !self.reports_host_gpu {
                return Err(RepositoryError::new(
                    "host GPU capabilities are not supported by this backend",
                ));
            }
            Ok(HostGpuCapabilities {
                assignment: vmlord_core::GpuAvailability::Available,
                linux_payload: vmlord_core::GpuAvailability::Available,
                adapters: Vec::new(),
            })
        }

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
                gpu_mode: self.gpu_mode,
                gpu: self.gpu.clone(),
                desktop_profile: self.desktop_profile,
                display_provisioning: self.display_provisioning.clone(),
                display: self.display.clone(),
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

        /// Updates the way the native backend does: only a running VM can be
        /// asked, because what verifies an update is its own guest.
        fn update_display_payload(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("update-display:{name}"));
            if !self.vm_is_running {
                return Err(RepositoryError::new(format!(
                    "VM \"{name}\" is not running, so its display payload cannot be updated"
                )));
            }
            // Accepted, the way the native backend accepts it: a worker
            // starts, the VM reports itself as updating, and what the guest
            // made of it arrives later and from somewhere else.
            self.display.update_in_flight = true;
            Ok(())
        }

        /// Opens a session the way the native backend does: the command it was
        /// opened with is left in the diagnostics, and a refusal names the
        /// preflight check that stopped it.
        fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
            self.actions.push(format!("ssh:{name}"));
            if !self.vm_is_running {
                return Err(RepositoryError::new(format!(
                    "cannot open an SSH session to VM \"{name}\": the guest does not \
                     answer on port 22: connection refused"
                )));
            }
            vmlord_core::diagnostic!(
                Info,
                Subsystem::Network,
                vm = name,
                "SSH session for VM \"{name}\": ssh.exe -p 22 -l user 172.30.0.5"
            );
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

        fn refresh(&mut self) {
            vmlord_core::diagnostic!(Info, Subsystem::App, "ready");
        }
    }

    /// The button that calls this arrives with #65; the contract arrives here,
    /// so that adding the button is adding a button.
    /// Installs a diagnostics sink for the rest of this test, and hands it back
    /// to be given to the application.
    ///
    /// Thread-local through `set_default`, so tests running side by side read
    /// their own records; the guard has to be kept alive, which is why it comes
    /// back with the sink.
    #[must_use]
    fn records() -> (DiagnosticsSink, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;

        let sink = DiagnosticsSink::new();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(vmlord_core::DiagnosticsLayer::new(sink.clone())),
        );
        (sink, guard)
    }

    #[test]
    fn cancelling_a_creation_a_backend_cannot_cancel_is_reported() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
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

    /// The stored profile and the stored provisioning are enough for a status:
    /// a desktop whose packages never arrived reads as degraded and offers a
    /// retry, on a VM that is otherwise fine.
    #[test]
    fn the_application_layer_reads_a_display_status_out_of_what_the_backend_stored() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            vm_is_running: true,
            desktop_profile: vmlord_core::DesktopProfile::Gnome,
            display_provisioning: vmlord_core::DisplayProvisioning::Degraded(
                vmlord_core::DisplayFailure::new(
                    vmlord_core::DisplayStage::Provisioning,
                    vmlord_core::DisplayStatusCode::PackageDownloadFailed,
                    "archive.ubuntu.com did not answer",
                ),
            ),
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
        app.start();

        let status = app
            .display_status("dev")
            .expect("the listed VM has a status");

        assert_eq!(status.state, vmlord_core::DisplayState::Degraded);
        assert!(status.can_retry);
        assert_eq!(
            app.vms()[0].desktop_profile,
            vmlord_core::DesktopProfile::Gnome,
            "the desired profile is reported as it was stored, whatever installing it did"
        );
        assert_eq!(app.display_status("absent"), None);
    }

    /// The backend reports facts and the application layer says what they
    /// mean: a VM that renders on its GPU is `GuestReady` without the backend
    /// ever having named a state.
    #[test]
    fn the_application_layer_reads_a_gpu_status_out_of_the_backend_s_facts() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            vm_is_running: true,
            gpu_mode: vmlord_core::GpuMode::Mirror,
            gpu: vmlord_core::VmGpuFacts {
                assignment: Some(vmlord_core::GpuAssignment::Complete(
                    vmlord_core::NativeGpuDetail {
                        adapter: Some("NVIDIA RTX 4070".into()),
                        adapters: 2,
                    },
                )),
                guest: Some(vmlord_core::GuestGpuReport::Ready(
                    vmlord_core::GuestGpuDetail {
                        driver: Some("dxgkrnl".into()),
                        render_node: Some("/dev/dri/renderD128".into()),
                    },
                )),
                observed_at: None,
            },
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
        app.start();

        let status = app.gpu_status("dev").expect("the listed VM has a status");

        assert_eq!(status.state, vmlord_core::GpuState::GuestReady);
        assert_eq!(status.code, vmlord_core::GpuStatusCode::GuestReady);
        assert_eq!(
            app.vms()[0].gpu_mode,
            vmlord_core::GpuMode::Mirror,
            "the desired mode is reported as it was stored, not as the runtime found it"
        );
    }

    /// The status is derived per refresh, so a VM that is no longer listed has
    /// none rather than a stale one.
    #[test]
    fn a_vm_the_backend_does_not_list_has_no_gpu_status() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
        app.start();

        assert!(app.gpu_status("never-created").is_none());
    }

    /// The create form fills three fields from this, and it is the composition
    /// root -- not the form -- that knows what the host says (#60).
    #[test]
    fn guest_defaults_are_offered_and_can_be_replaced_by_the_composition_root() {
        let (sink, _guard) = records();
        let app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);

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
        let (sink, _guard) = records();
        let app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);

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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
        app.start();
        assert_eq!(app.status(), &BackendStatus::Ready);
        assert_eq!(app.vms().len(), 1);
        assert_eq!(app.diagnostics().len(), 1);
    }

    #[test]
    fn initialization_error_is_visible() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: true,
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
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

    /// A VM whose desktop is installed and whose guest reports one version
    /// while the release carries another: the only VM an update is offered for.
    fn updatable_repository() -> FakeRepository {
        FakeRepository {
            vm_is_running: true,
            display_provisioning: vmlord_core::DisplayProvisioning::Ready,
            display: vmlord_core::VmDisplayFacts {
                guest: Some(vmlord_core::GuestDisplayReport::Ready(
                    vmlord_core::GuestDisplayDetail::default(),
                )),
                payload: vmlord_core::DisplayPayloadFacts {
                    installed: Some("0.1.4".into()),
                    previous: None,
                    loaded: Some("0.1.4".into()),
                    available: Some("0.1.5".into()),
                },
                failure: None,
                observed_at: None,
                update_in_flight: false,
            },
            ..FakeRepository::default()
        }
    }

    #[test]
    fn updates_a_display_payload_through_the_repository() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(updatable_repository())).with_diagnostics(sink);
        app.start();

        app.update_display_payload("dev")
            .expect("a running VM can be asked");

        let accepted = app
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message == "Updating the display payload of VM \"dev\"");
        assert!(
            accepted,
            "the request is worth one line: {:?}",
            app.diagnostics()
        );
        assert!(
            !app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("updated")),
            "nothing has been updated yet: the guest has not answered"
        );
    }

    /// What the button reads to stop offering a second update, and what the
    /// panel reads to say one is under way.
    #[test]
    fn a_vm_being_updated_reports_itself_as_updating() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(updatable_repository())).with_diagnostics(sink);
        app.start();
        assert!(
            !app.display_status("dev")
                .expect("a derived status")
                .updating
        );

        app.update_display_payload("dev").expect("accepted");

        assert!(
            app.display_status("dev")
                .expect("a derived status")
                .updating,
            "the refresh the click ends with is what shows the update in flight"
        );
    }

    #[test]
    fn a_display_payload_update_of_a_stopped_vm_is_refused_and_says_why() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
        app.start();

        let error = app
            .update_display_payload("dev")
            .expect_err("there is nobody to ask");

        assert!(error.to_string().contains("not running"));
        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error
                    && diagnostic.message.contains("Failed to update")),
        );
    }

    #[test]
    fn opens_display_through_repository() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            vm_is_running: true,
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
        app.start();

        app.open_ssh("dev").unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "Opening an SSH session for VM \"dev\"")
        );
    }

    /// A session that opened is a process in a window of its own and says
    /// nothing back, so the command it was opened with is the only account of
    /// it there will ever be. It is the backend's to compose -- which key,
    /// which known-hosts file, which port -- and this layer's to carry into the
    /// log a person actually reads.
    #[test]
    fn the_command_a_session_opened_with_reaches_the_log() {
        let (sink, _guard) = records();
        let mut app = app_with(true, sink);

        app.open_ssh("dev").unwrap();

        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Info
                    && diagnostic.message.contains("ssh.exe")
                    && diagnostic.message.contains("-l user")
            }),
            "{:?}",
            app.diagnostics()
        );
    }

    /// The preflight check that stopped a session -- a missing Windows feature,
    /// a port that did not answer, a key that is gone -- is the only account of
    /// it anyone gets: once the terminal is up, everything else OpenSSH has to
    /// say goes into that window. So it reaches the diagnostics whole, rather
    /// than as a failure this layer describes in its own words.
    #[test]
    fn a_refused_ssh_session_is_reported_with_the_backends_own_reason() {
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            vm_is_running: true,
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);
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
        let (sink, _guard) = records();
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
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default()))
            .with_diagnostics(sink)
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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);

        app.log_vm_action(VmAction::Create);

        assert_eq!(app.diagnostics()[0].message, "Create VM pressed");
    }

    /// An application already started, reading the sink its caller installed.
    fn app_with(vm_is_running: bool, sink: DiagnosticsSink) -> WorkspaceApp {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            vm_is_running,
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
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
        let (sink, _guard) = records();
        let mut app = app_with(false, sink);

        app.delete_vm(delete_request(true)).unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "VM \"dev\" deleted")
        );
    }

    #[test]
    fn refuses_to_delete_a_running_vm() {
        let (sink, _guard) = records();
        let mut app = app_with(true, sink);

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
        let (sink, _guard) = records();
        let mut app = app_with(false, sink);

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
        let (sink, _guard) = records();
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: true,
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);
        app.start();

        let error = app
            .delete_vm(delete_request(true))
            .expect_err("deletion needs a ready backend");

        assert!(error.to_string().contains("ready backend"));
    }

    #[test]
    fn warns_when_the_disks_are_kept() {
        let (sink, _guard) = records();
        let mut app = app_with(false, sink);

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
        let (sink, _guard) = records();
        let mut app = app_with(false, sink);

        app.delete_vm(delete_request(true)).unwrap();

        assert!(!app.diagnostics().iter().any(|diagnostic| {
            diagnostic.level == DiagnosticLevel::Warning && diagnostic.message.contains("disks")
        }));
    }
    #[test]
    fn the_host_is_read_once_when_the_backend_comes_up() {
        let (sink, _guard) = records();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut application = WorkspaceApp::new(Box::new(FakeRepository {
            reports_host_gpu: true,
            host_gpu_reads: std::sync::Arc::clone(&reads),
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);

        application.start();
        application.refresh();
        application.refresh();

        assert_eq!(
            reads.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "SetupAPI and the filesystem are not walked once per frame"
        );
        assert!(application.host_gpu_capabilities().is_some());
    }

    #[test]
    fn a_backend_that_cannot_answer_leaves_the_host_unknown() {
        let (sink, _guard) = records();
        let mut application =
            WorkspaceApp::new(Box::new(FakeRepository::default())).with_diagnostics(sink);

        application.start();

        assert!(
            application.host_gpu_capabilities().is_none(),
            "\"this backend cannot tell you\" is not \"this host cannot do it\""
        );
    }

    #[test]
    fn a_backend_that_never_came_up_is_never_asked_about_the_host() {
        let (sink, _guard) = records();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut application = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: true,
            reports_host_gpu: true,
            host_gpu_reads: std::sync::Arc::clone(&reads),
            ..FakeRepository::default()
        }))
        .with_diagnostics(sink);

        application.start();

        assert_eq!(
            reads.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a backend that could not initialize has nothing to say about the host"
        );
        assert!(application.host_gpu_capabilities().is_none());
    }
}
