use std::{
    collections::{HashSet, VecDeque},
    ffi::c_void,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    ptr, slice,
    sync::{Mutex, MutexGuard},
};

use libloading::Library;
use vmlord_app::{ImagePicker, SettingsPathPicker};
use vmlord_core::{
    AgentStatus, Diagnostic, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError,
    SshAuthentication, SshAvailability, SshConfig, SshPort, VmCreateRequest, VmRepository, VmState,
    VmSummary, VmUpdateRequest,
};

type AsbVm = *mut c_void;
type AsbInit = unsafe extern "system" fn() -> i32;
type AsbVmStart = unsafe extern "system" fn(AsbVm, i32, i32, *const u16) -> i32;
type AsbVmShutdown = unsafe extern "system" fn(AsbVm) -> i32;
type AsbVmStop = unsafe extern "system" fn(AsbVm) -> i32;
type AsbVmOpenDisplay = unsafe extern "system" fn(AsbVm) -> i32;
type AsbVmSetDword = unsafe extern "system" fn(AsbVm, u32) -> i32;
type AsbVmSetInt = unsafe extern "system" fn(AsbVm, i32) -> i32;

type AsbDetach = unsafe extern "system" fn();
type AsbReconnectRunning = unsafe extern "system" fn();
type AsbSetCallback = unsafe extern "system" fn(Option<Callback>, *mut c_void);
type AsbVmCount = unsafe extern "system" fn() -> i32;
type AsbVmGet = unsafe extern "system" fn(i32) -> AsbVm;
type AsbVmString = unsafe extern "system" fn(AsbVm) -> *const u16;
type AsbVmBool = unsafe extern "system" fn(AsbVm) -> i32;
type AsbVmDword = unsafe extern "system" fn(AsbVm) -> u32;
type AsbVmInt = unsafe extern "system" fn(AsbVm) -> i32;
type Callback = unsafe extern "system" fn(*const u16, *mut c_void);

struct Api {
    init: AsbInit,
    vm_set_ram: Option<AsbVmSetDword>,
    vm_set_cpu: Option<AsbVmSetDword>,
    vm_set_gpu: Option<AsbVmSetInt>,
    vm_set_network: Option<AsbVmSetInt>,
    vm_start: AsbVmStart,
    vm_shutdown: AsbVmShutdown,
    vm_stop: AsbVmStop,
    vm_open_display: Option<AsbVmOpenDisplay>,
    detach: AsbDetach,
    reconnect_running: AsbReconnectRunning,
    set_log_callback: AsbSetCallback,
    set_alert_callback: AsbSetCallback,
    vm_count: AsbVmCount,
    vm_get: AsbVmGet,
    vm_name: AsbVmString,
    vm_os_type: AsbVmString,
    vm_is_running: AsbVmBool,
    vm_agent_online: AsbVmBool,
    vm_is_building: AsbVmBool,
    vm_ram_mb: AsbVmDword,
    vm_hdd_gb: AsbVmDword,
    vm_cpu_cores: AsbVmDword,
    vm_gpu_mode: AsbVmInt,
    vm_network_mode: AsbVmInt,
    vm_admin_user: Option<AsbVmString>,
    vm_ssh_enabled: AsbVmBool,
    vm_ssh_deploy_key: Option<AsbVmBool>,
    vm_ssh_port: AsbVmDword,
}

struct CallbackContext {
    diagnostics: Mutex<VecDeque<Diagnostic>>,
    running_without_handle: Mutex<HashSet<String>>,
}

pub struct WindowsImagePicker;

impl WindowsImagePicker {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for WindowsImagePicker {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagePicker for WindowsImagePicker {
    fn pick_iso_image(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select Linux VM image")
            .add_filter("VM images", &["iso", "vhdx"])
            .pick_file()
            .map(|path| path.to_string_lossy().into_owned()))
    }
}

pub struct WindowsSettingsPathPicker;

impl WindowsSettingsPathPicker {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for WindowsSettingsPathPicker {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsPathPicker for WindowsSettingsPathPicker {
    fn pick_vm_storage_directory(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select VM storage directory")
            .pick_folder()
            .map(|path| path.to_string_lossy().into_owned()))
    }

    fn pick_log_file(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select log file")
            .set_file_name("vmlord.log")
            .save_file()
            .map(|path| path.to_string_lossy().into_owned()))
    }
}

pub struct AppSandboxBackend {
    // Kept after Api: API function pointers must never outlive this loaded library.
    api: Api,
    _library: Library,
    callbacks: Box<CallbackContext>,
    initialized: bool,
}

impl AppSandboxBackend {
    /// Loads `appsandbox_core.dll` from the directory that contains the executable.
    pub fn load_from_executable_dir() -> Result<Self, RepositoryError> {
        let executable = std::env::current_exe().map_err(|error| {
            RepositoryError::new(format!("cannot determine VMLord executable path: {error}"))
        })?;
        let directory = executable.parent().ok_or_else(|| {
            RepositoryError::new("VMLord executable path has no parent directory")
        })?;
        Self::load_from(directory.join("appsandbox_core.dll"))
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(RepositoryError::new(format!(
                "legacy backend was not found at {}",
                path.display()
            )));
        }

        // Loading a native DLL is isolated to this platform adapter.
        let library = unsafe { Library::new(path) }.map_err(|error| {
            RepositoryError::new(format!(
                "cannot load legacy backend {}: {error}",
                path.display()
            ))
        })?;
        let api = Api::load(&library)?;

        Ok(Self {
            api,
            _library: library,
            callbacks: Box::new(CallbackContext {
                diagnostics: Mutex::new(VecDeque::new()),
                running_without_handle: Mutex::new(HashSet::new()),
            }),
            initialized: false,
        })
    }

    fn callbacks_ptr(&mut self) -> *mut c_void {
        self.callbacks.as_mut() as *mut CallbackContext as *mut c_void
    }

    fn diagnostics_lock(&self) -> MutexGuard<'_, VecDeque<Diagnostic>> {
        self.callbacks
            .diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn running_without_handle_lock(&self) -> MutexGuard<'_, HashSet<String>> {
        self.callbacks
            .running_without_handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl VmRepository for AppSandboxBackend {
    fn initialize(&mut self) -> Result<(), RepositoryError> {
        if self.initialized {
            return Ok(());
        }

        let context = self.callbacks_ptr();
        unsafe {
            (self.api.set_log_callback)(Some(log_callback), context);
            (self.api.set_alert_callback)(Some(alert_callback), context);
        }

        let result = unsafe { (self.api.init)() };
        if result < 0 {
            unsafe {
                (self.api.set_log_callback)(None, ptr::null_mut());
                (self.api.set_alert_callback)(None, ptr::null_mut());
            }
            return Err(RepositoryError::new(format!(
                "AppSandbox backend initialization failed (HRESULT 0x{:08X})",
                result as u32
            )));
        }

        // `asb_init` restores persisted VM definitions but does not reconnect
        // to VMs launched by an earlier process.
        unsafe { (self.api.reconnect_running)() };
        self.initialized = true;
        Ok(())
    }

    fn create_vm(&mut self, _request: VmCreateRequest) -> Result<(), RepositoryError> {
        // AppSandbox builds a Linux disk by handing the medium to
        // `iso-patch.exe`, a host-side Ubuntu installer VMLord no longer ships:
        // the native backend imports a cloud image instead. Nothing else in the
        // DLL creates a VM, so this refuses rather than failing inside the C
        // code on a missing executable. Everything the backend still does with
        // VMs it did not create -- list, start, stop, edit, delete -- is
        // untouched; the legacy backend is transitional (AGENTS.md) and only
        // runs when `VMLORD_BACKEND=legacy` asks for it.
        Err(RepositoryError::new(
            "the legacy AppSandbox backend can no longer create VMs; unset VMLORD_BACKEND to \
             create one with the native backend",
        ))
    }

    fn update_vm(&mut self, request: VmUpdateRequest) -> Result<(), RepositoryError> {
        let vm = self.vm_by_name(&request.name)?;
        if unsafe { (self.api.vm_is_building)(vm) != 0 } {
            return Err(RepositoryError::new(format!(
                "VM \"{}\" is still building; wait for the build to finish before editing it",
                request.name
            )));
        }
        if unsafe { (self.api.vm_is_running)(vm) != 0 } {
            return Err(RepositoryError::new(format!(
                "VM \"{}\" is running; stop it before editing",
                request.name
            )));
        }

        if unsafe { (self.api.vm_ram_mb)(vm) } != request.ram_mb {
            let set_ram = self.api.vm_set_ram.ok_or_else(|| {
                RepositoryError::new(
                    "the installed AppSandbox backend is too old for VM updates; update appsandbox_core.dll",
                )
            })?;
            let result = unsafe { set_ram(vm, request.ram_mb) };
            check_update_result("RAM", &request.name, result)?;
        }
        if unsafe { (self.api.vm_cpu_cores)(vm) } != request.cpu_cores {
            let set_cpu = self.api.vm_set_cpu.ok_or_else(|| {
                RepositoryError::new(
                    "the installed AppSandbox backend is too old for VM updates; update appsandbox_core.dll",
                )
            })?;
            let result = unsafe { set_cpu(vm, request.cpu_cores) };
            check_update_result("CPU", &request.name, result)?;
        }
        if gpu_mode(unsafe { (self.api.vm_gpu_mode)(vm) }) != request.gpu_mode {
            let set_gpu = self.api.vm_set_gpu.ok_or_else(|| {
                RepositoryError::new(
                    "the installed AppSandbox backend is too old for VM updates; update appsandbox_core.dll",
                )
            })?;
            let result = unsafe { set_gpu(vm, gpu_mode_value(request.gpu_mode)?) };
            check_update_result("GPU", &request.name, result)?;
        }
        if network_mode(unsafe { (self.api.vm_network_mode)(vm) }) != request.network_mode {
            let set_network = self.api.vm_set_network.ok_or_else(|| {
                RepositoryError::new(
                    "the installed AppSandbox backend is too old for VM updates; update appsandbox_core.dll",
                )
            })?;
            let result = unsafe { set_network(vm, network_mode_value(request.network_mode)?) };
            check_update_result("network", &request.name, result)?;
        }

        Ok(())
    }

    fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        let vm = self.vm_by_name(name)?;
        let result = unsafe { (self.api.vm_start)(vm, -1, -1, ptr::null()) };
        check_lifecycle_result("start", name, result)
    }

    fn stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        let vm = self.vm_by_name(name)?;
        let result = unsafe { (self.api.vm_shutdown)(vm) };
        check_lifecycle_result("stop", name, result)
    }

    fn force_stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        let vm = self.vm_by_name(name)?;
        let result = unsafe { (self.api.vm_stop)(vm) };
        check_lifecycle_result("force stop", name, result)
    }

    fn delete_vm(&mut self, request: vmlord_core::VmDeleteRequest) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(format!(
            "the legacy AppSandbox backend cannot delete VM \"{}\"; \
             run VMLord on the native HCS backend to delete VMs",
            request.name
        )))
    }

    fn open_display(&mut self, name: &str) -> Result<(), RepositoryError> {
        let vm = self.vm_by_name(name)?;
        if unsafe { (self.api.vm_is_running)(vm) == 0 } {
            return Err(RepositoryError::new(format!(
                "VM \"{name}\" is not running"
            )));
        }
        let open_display = self.api.vm_open_display.ok_or_else(|| {
            RepositoryError::new(
                "the installed AppSandbox backend is too old for display connections; update appsandbox_core.dll",
            )
        })?;
        let result = unsafe { open_display(vm) };
        if result < 0 {
            return Err(RepositoryError::new(format!(
                "AppSandbox failed to open the display for VM \"{name}\" (HRESULT 0x{:08X})",
                result as u32
            )));
        }

        Ok(())
    }

    fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
        let vm = self.vm_by_name(name)?;
        let is_running = unsafe { (self.api.vm_is_running)(vm) != 0 };
        if !is_running {
            return Err(RepositoryError::new(format!(
                "VM \"{name}\" is not running"
            )));
        }
        if unsafe { (self.api.vm_ssh_enabled)(vm) == 0 } {
            return Err(RepositoryError::new(format!(
                "SSH is disabled for VM \"{name}\""
            )));
        }

        let port = unsafe { (self.api.vm_ssh_port)(vm) };
        let port = u16::try_from(port)
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| {
                RepositoryError::new(format!(
                    "SSH is not ready for VM \"{name}\"; wait for the guest agent to finish setup"
                ))
            })?;
        let admin_user = self.api.vm_admin_user.ok_or_else(|| {
            RepositoryError::new(
                "the installed AppSandbox backend is too old for SSH connections; update appsandbox_core.dll",
            )
        })?;
        let deploy_key = self.api.vm_ssh_deploy_key.ok_or_else(|| {
            RepositoryError::new(
                "the installed AppSandbox backend is too old for SSH connections; update appsandbox_core.dll",
            )
        })?;
        let username = unsafe { wide_ptr_to_string(admin_user(vm))? };
        if username.is_empty() {
            return Err(RepositoryError::new(format!(
                "VM \"{name}\" has no SSH username"
            )));
        }
        let use_deploy_key = unsafe { deploy_key(vm) != 0 };

        launch_ssh_terminal(&username, port, use_deploy_key)
    }

    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
        if !self.initialized {
            return Err(RepositoryError::new("legacy backend is not initialized"));
        }

        let count = unsafe { (self.api.vm_count)() };
        if count < 0 {
            return Err(RepositoryError::new(
                "legacy backend returned an invalid VM count",
            ));
        }

        (0..count)
            .filter_map(|index| {
                let vm = unsafe { (self.api.vm_get)(index) };
                (!vm.is_null()).then(|| self.vm_summary(vm))
            })
            .collect::<Result<Vec<_>, _>>()
    }

    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        self.diagnostics_lock().drain(..).collect()
    }
}

impl AppSandboxBackend {
    fn vm_by_name(&self, name: &str) -> Result<AsbVm, RepositoryError> {
        if !self.initialized {
            return Err(RepositoryError::new("legacy backend is not initialized"));
        }

        let count = unsafe { (self.api.vm_count)() };
        if count < 0 {
            return Err(RepositoryError::new(
                "legacy backend returned an invalid VM count",
            ));
        }

        for index in 0..count {
            let vm = unsafe { (self.api.vm_get)(index) };
            if vm.is_null() {
                continue;
            }
            let vm_name = unsafe { wide_ptr_to_string((self.api.vm_name)(vm))? };
            if vm_name == name {
                return Ok(vm);
            }
        }

        Err(RepositoryError::new(format!("VM \"{name}\" was not found")))
    }

    fn vm_summary(&self, vm: AsbVm) -> Result<VmSummary, RepositoryError> {
        // The handle and each returned string pointer originate from AppSandbox.
        unsafe {
            let name = wide_ptr_to_string((self.api.vm_name)(vm))?;
            let running = (self.api.vm_is_running)(vm) != 0;
            let building = (self.api.vm_is_building)(vm) != 0;
            let agent_online = (self.api.vm_agent_online)(vm) != 0;
            let ssh = self.ssh_availability(vm);
            let running_without_handle = self.running_without_handle_lock().contains(&name);

            Ok(VmSummary {
                name,
                os_type: wide_ptr_to_string((self.api.vm_os_type)(vm))?,
                state: if building {
                    VmState::Starting
                } else if running {
                    let agent_status = if agent_online {
                        AgentStatus::Online
                    } else if running_without_handle {
                        AgentStatus::Unknown
                    } else {
                        AgentStatus::Offline
                    };
                    VmState::Running { agent_status }
                } else {
                    VmState::Stopped
                },
                ram_mb: (self.api.vm_ram_mb)(vm),
                disk_gb: (self.api.vm_hdd_gb)(vm),
                cpu_cores: (self.api.vm_cpu_cores)(vm),
                gpu_mode: gpu_mode((self.api.vm_gpu_mode)(vm)),
                network_mode: network_mode((self.api.vm_network_mode)(vm)),
                // The temporary AppSandbox FFI does not expose a guest IP address yet.
                ip_address: None,
                ssh,
            })
        }
    }

    /// What AppSandbox knows about a VM's SSH access, as the domain spells it.
    ///
    /// Every piece has to be there for the answer to be `Enabled`: an older
    /// `appsandbox_core.dll` has no user name to give, and a VM whose agent has
    /// not finished its setup reports a port of zero. Both are a connection
    /// that cannot be made, and neither is an error worth losing the VM from
    /// the list over -- [`AppSandboxBackend::open_ssh`] is where a person who
    /// asks for one anyway is told which of the two it was.
    fn ssh_availability(&self, vm: AsbVm) -> SshAvailability {
        // The handle and the returned string pointer originate from AppSandbox.
        unsafe {
            if (self.api.vm_ssh_enabled)(vm) == 0 {
                return SshAvailability::Disabled;
            }
            let (Some(admin_user), Some(deploy_key)) =
                (self.api.vm_admin_user, self.api.vm_ssh_deploy_key)
            else {
                return SshAvailability::Disabled;
            };
            let Ok(username) = wide_ptr_to_string(admin_user(vm)) else {
                return SshAvailability::Disabled;
            };
            let port = u16::try_from((self.api.vm_ssh_port)(vm))
                .ok()
                .and_then(|port| SshPort::new(port).ok());
            let Some(port) = port else {
                return SshAvailability::Disabled;
            };

            let config = SshConfig {
                username,
                port,
                authentication: if deploy_key(vm) != 0 {
                    SshAuthentication::VmlordKey
                } else {
                    SshAuthentication::Password
                },
            };
            if config.validate().is_err() {
                return SshAvailability::Disabled;
            }
            SshAvailability::Enabled(config)
        }
    }
}

impl Drop for AppSandboxBackend {
    fn drop(&mut self) {
        unsafe {
            (self.api.set_log_callback)(None, ptr::null_mut());
            (self.api.set_alert_callback)(None, ptr::null_mut());
            if self.initialized {
                (self.api.detach)();
            }
        }
    }
}

impl Api {
    fn load(library: &Library) -> Result<Self, RepositoryError> {
        macro_rules! export {
            ($name:literal, $type:ty) => {
                *unsafe { library.get::<$type>($name) }.map_err(|error| {
                    RepositoryError::new(format!(
                        "legacy backend is missing export {}: {error}",
                        String::from_utf8_lossy($name)
                    ))
                })?
            };
        }
        macro_rules! optional_export {
            ($name:literal, $type:ty) => {
                unsafe { library.get::<$type>($name) }
                    .ok()
                    .map(|function| *function)
            };
        }

        Ok(Self {
            init: export!(b"asb_init\0", AsbInit),
            vm_set_ram: optional_export!(b"asb_vm_set_ram\0", AsbVmSetDword),
            vm_set_cpu: optional_export!(b"asb_vm_set_cpu\0", AsbVmSetDword),
            vm_set_gpu: optional_export!(b"asb_vm_set_gpu\0", AsbVmSetInt),
            vm_set_network: optional_export!(b"asb_vm_set_network\0", AsbVmSetInt),
            vm_start: export!(b"asb_vm_start\0", AsbVmStart),
            vm_shutdown: export!(b"asb_vm_shutdown\0", AsbVmShutdown),
            vm_stop: export!(b"asb_vm_stop\0", AsbVmStop),
            vm_open_display: optional_export!(b"asb_vm_open_display\0", AsbVmOpenDisplay),
            detach: export!(b"asb_detach\0", AsbDetach),
            reconnect_running: export!(b"asb_reconnect_running\0", AsbReconnectRunning),
            set_log_callback: export!(b"asb_set_log_callback\0", AsbSetCallback),
            set_alert_callback: export!(b"asb_set_alert_callback\0", AsbSetCallback),
            vm_count: export!(b"asb_vm_count\0", AsbVmCount),
            vm_get: export!(b"asb_vm_get\0", AsbVmGet),
            vm_name: export!(b"asb_vm_name\0", AsbVmString),
            vm_os_type: export!(b"asb_vm_os_type\0", AsbVmString),
            vm_is_running: export!(b"asb_vm_is_running\0", AsbVmBool),
            vm_agent_online: export!(b"asb_vm_agent_online\0", AsbVmBool),
            vm_is_building: export!(b"asb_vm_is_building\0", AsbVmBool),
            vm_ram_mb: export!(b"asb_vm_ram_mb\0", AsbVmDword),
            vm_hdd_gb: export!(b"asb_vm_hdd_gb\0", AsbVmDword),
            vm_cpu_cores: export!(b"asb_vm_cpu_cores\0", AsbVmDword),
            vm_gpu_mode: export!(b"asb_vm_gpu_mode\0", AsbVmInt),
            vm_network_mode: export!(b"asb_vm_network_mode\0", AsbVmInt),
            vm_admin_user: optional_export!(b"asb_vm_admin_user\0", AsbVmString),
            vm_ssh_enabled: export!(b"asb_vm_ssh_enabled\0", AsbVmBool),
            vm_ssh_deploy_key: optional_export!(b"asb_vm_ssh_deploy_key\0", AsbVmBool),
            vm_ssh_port: export!(b"asb_vm_ssh_port\0", AsbVmDword),
        })
    }
}

const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

fn launch_ssh_terminal(
    username: &str,
    port: u16,
    use_deploy_key: bool,
) -> Result<(), RepositoryError> {
    let ssh_executable = ssh_executable_path()?;
    let mut ssh_arguments = vec![
        "-p".into(),
        port.to_string(),
        format!("{username}@localhost"),
    ];

    if use_deploy_key {
        let private_key = appsandbox_ssh_key_path();
        if !private_key.is_file() {
            return Err(RepositoryError::new(format!(
                "AppSandbox SSH key was not found at {}",
                private_key.display()
            )));
        }
        let known_hosts = std::env::temp_dir().join("appsandbox_known_hosts");
        ssh_arguments.splice(
            0..0,
            [
                "-i".into(),
                private_key.to_string_lossy().into_owned(),
                "-o".into(),
                "IdentitiesOnly=yes".into(),
                "-o".into(),
                "StrictHostKeyChecking=no".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", known_hosts.display()),
            ],
        );
    }

    let ssh_command = std::iter::once(powershell_literal(&ssh_executable.to_string_lossy()))
        .chain(
            ssh_arguments
                .iter()
                .map(|argument| powershell_literal(argument)),
        )
        .collect::<Vec<_>>()
        .join(" ");
    let powershell_command = format!("& {ssh_command}");
    match Command::new("wt.exe")
        .args([
            "-w",
            "0",
            "new-tab",
            "--title",
            "VMLord SSH",
            "powershell.exe",
        ])
        .args(["-NoExit", "-Command"])
        .arg(&powershell_command)
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(windows_terminal_error) => Command::new("powershell.exe")
            .creation_flags(CREATE_NEW_CONSOLE)
            .args(["-NoExit", "-Command"])
            .arg(powershell_command)
            .spawn()
            .map(|_| ())
            .map_err(|powershell_error| {
                RepositoryError::new(format!(
                    "cannot open Windows Terminal ({windows_terminal_error}) or PowerShell ({powershell_error})"
                ))
            }),
    }
}

fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn ssh_executable_path() -> Result<PathBuf, RepositoryError> {
    let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
        RepositoryError::new("the SystemRoot environment variable is unavailable")
    })?;
    let executable = PathBuf::from(system_root)
        .join("System32")
        .join("OpenSSH")
        .join("ssh.exe");
    if executable.is_file() {
        Ok(executable)
    } else {
        Err(RepositoryError::new(format!(
            "Windows OpenSSH client was not found at {}",
            executable.display()
        )))
    }
}

fn appsandbox_ssh_key_path() -> PathBuf {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    program_data
        .join("AppSandbox")
        .join("ssh")
        .join("id_appsandbox")
}

fn check_lifecycle_result(action: &str, name: &str, result: i32) -> Result<(), RepositoryError> {
    if result < 0 {
        return Err(RepositoryError::new(format!(
            "AppSandbox failed to {action} VM \"{name}\" (HRESULT 0x{:08X})",
            result as u32
        )));
    }
    Ok(())
}

fn check_update_result(field: &str, name: &str, result: i32) -> Result<(), RepositoryError> {
    if result < 0 {
        return Err(RepositoryError::new(format!(
            "AppSandbox failed to update {field} for VM \"{name}\" (HRESULT 0x{:08X})",
            result as u32
        )));
    }
    Ok(())
}

unsafe extern "system" fn log_callback(message: *const u16, user_data: *mut c_void) {
    push_callback_diagnostic(message, user_data, DiagnosticLevel::Info);
}

unsafe extern "system" fn alert_callback(message: *const u16, user_data: *mut c_void) {
    push_callback_diagnostic(message, user_data, DiagnosticLevel::Error);
}

fn push_callback_diagnostic(message: *const u16, user_data: *mut c_void, level: DiagnosticLevel) {
    if user_data.is_null() {
        return;
    }
    let context = unsafe { &*(user_data as *const CallbackContext) };
    let message =
        wide_ptr_to_string(message).unwrap_or_else(|_| "invalid backend callback message".into());

    if let Some(name) = reconnect_running_without_handle_vm_name(&message) {
        let mut running_without_handle = context
            .running_without_handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        running_without_handle.insert(name.to_owned());
    }

    let mut diagnostics = context
        .diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    diagnostics.push_back(Diagnostic { level, message });
}

fn reconnect_running_without_handle_vm_name(message: &str) -> Option<&str> {
    let prefix = "Reconnect: \"";
    let suffix = "\" is running (via enumeration, no handle).";
    message
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
}

fn wide_ptr_to_string(pointer: *const u16) -> Result<String, RepositoryError> {
    if pointer.is_null() {
        return Ok(String::new());
    }
    const MAX_WIDE_STRING: usize = 32_768;
    let length = (0..MAX_WIDE_STRING)
        .find(|&index| unsafe { *pointer.add(index) == 0 })
        .ok_or_else(|| RepositoryError::new("backend returned an unterminated UTF-16 string"))?;
    Ok(String::from_utf16_lossy(unsafe {
        slice::from_raw_parts(pointer, length)
    }))
}

fn gpu_mode_value(mode: GpuMode) -> Result<i32, RepositoryError> {
    match mode {
        GpuMode::None => Ok(0),
        GpuMode::Default => Ok(1),
        GpuMode::TryAll => Ok(2),
        GpuMode::Unknown(value) => Err(RepositoryError::new(format!(
            "unsupported GPU mode: {value}"
        ))),
    }
}

fn network_mode_value(mode: NetworkMode) -> Result<i32, RepositoryError> {
    match mode {
        NetworkMode::None => Ok(0),
        NetworkMode::Nat => Ok(1),
        NetworkMode::External => Ok(2),
        NetworkMode::Internal => Ok(3),
        NetworkMode::Unknown(value) => Err(RepositoryError::new(format!(
            "unsupported network mode: {value}"
        ))),
    }
}

fn gpu_mode(value: i32) -> GpuMode {
    match value {
        0 => GpuMode::None,
        1 => GpuMode::Default,
        2 => GpuMode::TryAll,
        other => GpuMode::Unknown(other),
    }
}

fn network_mode(value: i32) -> NetworkMode {
    match value {
        0 => NetworkMode::None,
        1 => NetworkMode::Nat,
        2 => NetworkMode::External,
        3 => NetworkMode::Internal,
        other => NetworkMode::Unknown(other),
    }
}

#[cfg(test)]
mod tests {
    use super::reconnect_running_without_handle_vm_name;

    #[test]
    fn parses_reconnect_running_without_handle_message() {
        assert_eq!(
            reconnect_running_without_handle_vm_name(
                "Reconnect: \"ubuntu\" is running (via enumeration, no handle)."
            ),
            Some("ubuntu")
        );
    }

    #[test]
    fn ignores_other_messages() {
        assert_eq!(
            reconnect_running_without_handle_vm_name("Reconnect: \"ubuntu\" is running."),
            None
        );
    }
}
