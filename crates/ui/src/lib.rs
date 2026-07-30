//! Read-only desktop shell for the first VMLord milestone.

use std::time::{Duration, Instant};

use eframe::egui;
use vmlord_app::{BackendStatus, WorkspaceApp};
use vmlord_core::{AgentStatus, DiagnosticLevel, VmState, VmSummary};

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const VM_TABLE_COLUMN_COUNT: f32 = 9.0;

pub fn run(application: WorkspaceApp) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "VMLord",
        options,
        Box::new(move |_| {
            Ok(Box::new(VmlordUi {
                application,
                last_refresh: Instant::now(),
            }))
        }),
    )
}

struct VmlordUi {
    application: WorkspaceApp,
    last_refresh: Instant,
}

impl eframe::App for VmlordUi {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        context.request_repaint_after(AUTO_REFRESH_INTERVAL);
        if matches!(self.application.status(), BackendStatus::Ready)
            && self.last_refresh.elapsed() >= AUTO_REFRESH_INTERVAL
        {
            self.application.refresh();
            self.last_refresh = Instant::now();
        }

        egui::CentralPanel::default().show(context, |ui| {
            ui.heading("VMLord");
            ui.label("Linux workspaces on Windows");
            ui.separator();

            ui.horizontal(|ui| {
                render_backend_status(ui, self.application.status());
                if ui
                    .add_enabled(
                        matches!(self.application.status(), BackendStatus::Ready),
                        egui::Button::new("Refresh"),
                    )
                    .clicked()
                {
                    self.application.refresh();
                    self.last_refresh = Instant::now();
                }
            });

            ui.add_space(12.0);
            render_vm_list(ui, self.application.vms());
            ui.add_space(12.0);
            render_diagnostics(ui, self.application.diagnostics());
        });
    }
}

fn render_backend_status(ui: &mut egui::Ui, status: &BackendStatus) {
    match status {
        BackendStatus::Starting => ui.label("Backend: starting…"),
        BackendStatus::Ready => ui.colored_label(egui::Color32::LIGHT_GREEN, "Backend: ready"),
        BackendStatus::Unavailable(message) => ui.colored_label(
            egui::Color32::LIGHT_RED,
            format!("Backend unavailable: {message}"),
        ),
    };
}

fn render_vm_list(ui: &mut egui::Ui, vms: &[VmSummary]) {
    ui.heading("Workspaces");
    if vms.is_empty() {
        ui.weak("No virtual machines found.");
        return;
    }

    let column_spacing = ui.spacing().item_spacing.x;
    let min_column_width = (ui.available_width() - column_spacing * (VM_TABLE_COLUMN_COUNT - 1.0))
        / VM_TABLE_COLUMN_COUNT;

    egui::Grid::new("vm-list")
        .striped(true)
        .num_columns(VM_TABLE_COLUMN_COUNT as usize)
        .min_col_width(min_column_width)
        .show(ui, |ui| {
            ui.strong("Name");
            ui.strong("OS");
            ui.strong("Status");
            ui.strong("Agent status");
            ui.strong("CPU");
            ui.strong("RAM");
            ui.strong("Disk");
            ui.strong("GPU");
            ui.strong("Network type");
            ui.end_row();
            for vm in vms {
                ui.label(&vm.name);
                ui.label(&vm.os_type);
                ui.label(vm_state(vm.state));
                render_agent_status(ui, agent_status(vm.state));
                ui.label(format!("{} cores", vm.cpu_cores));
                ui.label(format!("{} MiB", vm.ram_mb));
                ui.label(format!("{} GiB", vm.disk_gb));
                ui.label(format!("{:?}", vm.gpu_mode));
                ui.label(format!("{:?}", vm.network_mode));
                ui.end_row();
            }
        });
}

fn render_diagnostics(ui: &mut egui::Ui, diagnostics: &[vmlord_core::Diagnostic]) {
    if diagnostics.is_empty() {
        return;
    }
    ui.collapsing("Backend diagnostics", |ui| {
        for diagnostic in diagnostics.iter().rev().take(20) {
            let color = match diagnostic.level {
                DiagnosticLevel::Info => egui::Color32::LIGHT_GRAY,
                DiagnosticLevel::Warning => egui::Color32::YELLOW,
                DiagnosticLevel::Error => egui::Color32::LIGHT_RED,
            };
            ui.colored_label(color, &diagnostic.message);
        }
    });
}

fn render_agent_status(ui: &mut egui::Ui, status: AgentStatus) {
    let (color, label) = match status {
        AgentStatus::Unknown => (egui::Color32::GRAY, "Unknown"),
        AgentStatus::Offline => (egui::Color32::LIGHT_RED, "Offline"),
        AgentStatus::Online => (egui::Color32::LIGHT_GREEN, "Online"),
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
    response.on_hover_text(label);
}

fn agent_status(state: VmState) -> AgentStatus {
    match state {
        VmState::Running { agent_status } => agent_status,
        VmState::Stopped | VmState::Starting => AgentStatus::Unknown,
    }
}

fn vm_state(state: VmState) -> &'static str {
    match state {
        VmState::Stopped => "Stopped",
        VmState::Starting => "Building",
        VmState::Running { .. } => "Running",
    }
}
