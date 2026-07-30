use std::{
    collections::VecDeque,
    ffi::c_void,
    path::Path,
    ptr, slice,
    sync::{Mutex, MutexGuard},
};

use libloading::Library;
use vmlord_core::{
    Diagnostic, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError, VmRepository, VmState,
    VmSummary,
};

type AsbVm = *mut c_void;
type AsbInit = unsafe extern "system" fn() -> i32;
type AsbDetach = unsafe extern "system" fn();
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
    detach: AsbDetach,
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
    vm_ssh_enabled: AsbVmBool,
    vm_ssh_port: AsbVmDword,
}

struct CallbackContext {
    diagnostics: Mutex<VecDeque<Diagnostic>>,
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

        self.initialized = true;
        Ok(())
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
    fn vm_summary(&self, vm: AsbVm) -> Result<VmSummary, RepositoryError> {
        // The handle and each returned string pointer originate from AppSandbox.
        unsafe {
            let running = (self.api.vm_is_running)(vm) != 0;
            let building = (self.api.vm_is_building)(vm) != 0;
            let agent_online = (self.api.vm_agent_online)(vm) != 0;
            let ssh_port = ((self.api.vm_ssh_enabled)(vm) != 0).then(|| (self.api.vm_ssh_port)(vm));

            Ok(VmSummary {
                name: wide_ptr_to_string((self.api.vm_name)(vm))?,
                os_type: wide_ptr_to_string((self.api.vm_os_type)(vm))?,
                state: if building {
                    VmState::Starting
                } else if running {
                    VmState::Running { agent_online }
                } else {
                    VmState::Stopped
                },
                ram_mb: (self.api.vm_ram_mb)(vm),
                disk_gb: (self.api.vm_hdd_gb)(vm),
                cpu_cores: (self.api.vm_cpu_cores)(vm),
                gpu_mode: gpu_mode((self.api.vm_gpu_mode)(vm)),
                network_mode: network_mode((self.api.vm_network_mode)(vm)),
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

        Ok(Self {
            init: export!(b"asb_init\0", AsbInit),
            detach: export!(b"asb_detach\0", AsbDetach),
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
            vm_ssh_enabled: export!(b"asb_vm_ssh_enabled\0", AsbVmBool),
            vm_ssh_port: export!(b"asb_vm_ssh_port\0", AsbVmDword),
        })
    }
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
    let mut diagnostics = context
        .diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    diagnostics.push_back(Diagnostic { level, message });
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
