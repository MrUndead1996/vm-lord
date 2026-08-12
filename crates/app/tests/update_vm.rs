use std::sync::{Arc, Mutex};

use vmlord_app::{BackendStatus, WorkspaceApp};
use vmlord_core::{
    AgentStatus, Diagnostic, GpuMode, NetworkMode, RepositoryError, SshAuthentication,
    SshAvailability, SshConfig, SshPort, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

#[derive(Clone, Default)]
struct SharedState {
    update_requests: Arc<Mutex<Vec<VmUpdateRequest>>>,
}

struct FakeRepository {
    vm_state: VmState,
    shared: SharedState,
}

impl VmRepository for FakeRepository {
    fn initialize(&mut self) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn create_vm(&mut self, _request: vmlord_core::VmCreateRequest) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn update_vm(&mut self, request: VmUpdateRequest) -> Result<(), RepositoryError> {
        self.shared.update_requests.lock().unwrap().push(request);
        Ok(())
    }

    fn start_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn force_stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn delete_vm(&mut self, _request: vmlord_core::VmDeleteRequest) -> Result<(), RepositoryError> {
        Ok(())
    }

    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
        Ok(vec![VmSummary {
            name: "dev".into(),
            os_type: "Linux".into(),
            state: self.vm_state,
            ram_mb: 4096,
            disk_gb: 64,
            cpu_cores: 4,
            gpu_mode: GpuMode::Default,
            network_mode: NetworkMode::Nat,
            ip_address: None,
            ssh: SshAvailability::Enabled(SshConfig {
                username: "user".into(),
                port: SshPort::DEFAULT,
                authentication: SshAuthentication::VmlordKey,
            }),
        }])
    }

    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        Vec::new()
    }
}

fn update_request() -> VmUpdateRequest {
    VmUpdateRequest {
        name: "dev".into(),
        ram_mb: 8192,
        cpu_cores: 8,
        gpu_mode: GpuMode::TryAll,
        network_mode: NetworkMode::Nat,
    }
}

#[test]
fn update_vm_calls_repository_and_keeps_backend_ready() {
    let shared = SharedState::default();
    let mut app = WorkspaceApp::new(Box::new(FakeRepository {
        vm_state: VmState::Stopped,
        shared: shared.clone(),
    }));
    app.start();

    app.update_vm(update_request()).unwrap();

    let recorded = shared.update_requests.lock().unwrap();
    assert_eq!(recorded.as_slice(), &[update_request()]);
    assert_eq!(app.status(), &BackendStatus::Ready);
    assert!(
        app.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message == "VM \"dev\" update accepted")
    );
}

#[test]
fn update_vm_accepts_a_running_vm_and_warns_that_it_needs_a_restart() {
    let shared = SharedState::default();
    let mut app = WorkspaceApp::new(Box::new(FakeRepository {
        vm_state: VmState::Running {
            agent_status: AgentStatus::Online,
        },
        shared: shared.clone(),
    }));
    app.start();

    app.update_vm(update_request()).unwrap();

    assert_eq!(
        shared.update_requests.lock().unwrap().as_slice(),
        &[update_request()]
    );
    assert!(app.diagnostics().iter().any(|diagnostic| {
        diagnostic.message == "VM \"dev\" is running; the new configuration applies after a restart"
    }));
}

#[test]
fn update_vm_does_not_warn_about_a_restart_for_a_stopped_vm() {
    let shared = SharedState::default();
    let mut app = WorkspaceApp::new(Box::new(FakeRepository {
        vm_state: VmState::Stopped,
        shared: shared.clone(),
    }));
    app.start();

    app.update_vm(update_request()).unwrap();

    assert!(
        !app.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("applies after a restart"))
    );
}

#[test]
fn update_vm_rejects_an_unknown_vm_before_repository_call() {
    let shared = SharedState::default();
    let mut app = WorkspaceApp::new(Box::new(FakeRepository {
        vm_state: VmState::Stopped,
        shared: shared.clone(),
    }));
    app.start();

    let mut request = update_request();
    request.name = "missing".into();
    let error = app.update_vm(request).unwrap_err();

    assert_eq!(error.to_string(), "VM \"missing\" was not found");
    assert!(shared.update_requests.lock().unwrap().is_empty());
}

#[test]
fn update_vm_rejects_invalid_ram_before_repository_call() {
    let shared = SharedState::default();
    let mut app = WorkspaceApp::new(Box::new(FakeRepository {
        vm_state: VmState::Stopped,
        shared: shared.clone(),
    }));
    app.start();

    let mut request = update_request();
    request.ram_mb = 513;
    let error = app.update_vm(request).unwrap_err();

    assert_eq!(
        error.to_string(),
        "RAM must be 2 MiB-aligned and at least 512 MiB for VM updates"
    );
    assert!(shared.update_requests.lock().unwrap().is_empty());
}
