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
use vmlord_app::ImagePicker;
use vmlord_core::{
    AgentStatus, Diagnostic, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError,
    VmCreateRequest, VmRepository, VmState, VmSummary,
};

type AsbVm = *mut c_void;
type AsbInit = unsafe extern "system" fn() -> i32;
type AsbVmCreate = unsafe extern "system" fn(*const AsbVmConfig) -> i32;
type AsbVmStart = unsafe extern "system" fn(AsbVm, i32, i32, *const u16) -> i32;
type AsbVmShutdown = unsafe extern "system" fn(AsbVm) -> i32;
type AsbVmStop = unsafe extern "system" fn(AsbVm) -> i32;

#[repr(C)]
struct AsbVmConfig {
    name: *const u16,
    os_type: *const u16,
    image_path: *const u16,
    template_name: *const u16,
    ram_mb: u32,
    hdd_gb: u32,
    cpu_cores: u32,
    gpu_mode: i32,
    network_mode: i32,
    net_adapter: *const u16,
    username: *const u16,
    password: *const u16,
    test_mode: i32,
    ssh_enabled: i32,
    ssh_deploy_key: i32,
    is_template: i32,
    linux_source: *const u16,
    locale: *const u16,
    keyboard: *const u16,
    timezone: *const u16,
}
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
    vm_create: AsbVmCreate,
    vm_start: AsbVmStart,
    vm_shutdown: AsbVmShutdown,
    vm_stop: AsbVmStop,
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

impl ImagePicker for WindowsImagePicker {
    fn pick_iso_image(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select Linux VM image")
            .add_filter("VM images", &["iso", "vhdx"])
            .pick_file()
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

    fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError> {
        if !self.initialized {
            return Err(RepositoryError::new("legacy backend is not initialized"));
        }

        let name = wide_string(&request.name);
        let os_type = wide_string("Linux");
        let image_path = wide_string(&request.image_path);
        let username = wide_string(&request.username);
        let password = wide_string(&request.password);
        let config = AsbVmConfig {
            name: name.as_ptr(),
            os_type: os_type.as_ptr(),
            image_path: image_path.as_ptr(),
            template_name: ptr::null(),
            ram_mb: request.ram_mb,
            hdd_gb: request.disk_gb,
            cpu_cores: request.cpu_cores,
            gpu_mode: gpu_mode_value(request.gpu_mode)?,
            network_mode: network_mode_value(request.network_mode)?,
            net_adapter: ptr::null(),
            username: username.as_ptr(),
            password: password.as_ptr(),
            test_mode: 1,
            ssh_enabled: i32::from(request.ssh_enabled),
            ssh_deploy_key: i32::from(request.ssh_deploy_key),
            is_template: 0,
            linux_source: ptr::null(),
            locale: ptr::null(),
            keyboard: ptr::null(),
            timezone: ptr::null(),
        };
        let result = unsafe { (self.api.vm_create)(&config) };
        if result < 0 {
            return Err(RepositoryError::new(format!(
                "AppSandbox VM creation failed (HRESULT 0x{:08X})",
                result as u32
            )));
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
            let ssh_port = ((self.api.vm_ssh_enabled)(vm) != 0).then(|| (self.api.vm_ssh_port)(vm));
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
                ssh_port,
            })
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
            vm_create: export!(b"asb_vm_create\0", AsbVmCreate),
            vm_start: export!(b"asb_vm_start\0", AsbVmStart),
            vm_shutdown: export!(b"asb_vm_shutdown\0", AsbVmShutdown),
            vm_stop: export!(b"asb_vm_stop\0", AsbVmStop),
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

fn wide_string(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn gpu_mode_value(mode: GpuMode) -> Result<i32, RepositoryError> {
    match mode {
        GpuMode::None => Ok(0),
        GpuMode::Default => Ok(1),
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
