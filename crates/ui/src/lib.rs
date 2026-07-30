//! Desktop shell for the first VMLord milestone.

use std::time::{Duration, Instant};

use eframe::egui;
use vmlord_app::{BackendStatus, VmAction, WorkspaceApp};
use vmlord_core::{
    AgentStatus, DiagnosticLevel, GpuMode, NetworkMode, VmCreateRequest, VmState, VmSummary,
};

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
                selected_vm_name: None,
                create_vm_form: None,
            }))
        }),
    )
}

struct VmlordUi {
    application: WorkspaceApp,
    last_refresh: Instant,
    selected_vm_name: Option<String>,
    create_vm_form: Option<CreateVmForm>,
}

struct CreateVmForm {
    name: String,
    image_path: String,
    disk_gb: u32,
    ram_mb: u32,
    cpu_cores: u32,
    gpu_mode: GpuMode,
    network_mode: NetworkMode,
    username: String,
    password: String,
    password_confirmation: String,
    ssh_enabled: bool,
    ssh_deploy_key: bool,
    error: Option<String>,
}

impl Default for CreateVmForm {
    fn default() -> Self {
        Self {
            name: "ubuntu".into(),
            image_path: String::new(),
            disk_gb: 64,
            ram_mb: 4096,
            cpu_cores: 4,
            gpu_mode: GpuMode::Default,
            network_mode: NetworkMode::Nat,
            username: "user".into(),
            password: String::new(),
            password_confirmation: String::new(),
            ssh_enabled: false,
            ssh_deploy_key: false,
            error: None,
        }
    }
}

enum CreateVmDialogAction {
    BrowseImage,
    Cancel,
    Submit(VmCreateRequest),
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

        let action = egui::CentralPanel::default().show(context, |ui| {
            let mut selected_action = None;

            ui.heading("VMLord");
            ui.label("Linux workspaces on Windows");
            ui.separator();

            ui.horizontal(|ui| {
                render_backend_status(ui, self.application.status());
                let can_refresh = matches!(self.application.status(), BackendStatus::Ready);
                let refresh = render_refresh_icon(ui, can_refresh);
                if can_refresh {
                    refresh.clone().on_hover_text("Refresh");
                } else {
                    refresh
                        .clone()
                        .on_disabled_hover_text("Available when the backend is ready");
                }
                if refresh.clicked() {
                    self.application.refresh();
                    self.last_refresh = Instant::now();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let create = ui.add_enabled(
                        can_refresh,
                        egui::Button::new(
                            egui::RichText::new("Create VM").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(47, 158, 97)),
                    );
                    if can_refresh {
                        create.clone().on_hover_text("Create a virtual machine");
                    } else {
                        create
                            .clone()
                            .on_disabled_hover_text("Available when the backend is ready");
                    }
                    if create.clicked() {
                        selected_action = Some(VmAction::Create);
                    }
                });
            });

            ui.add_space(12.0);
            render_vm_list(ui, self.application.vms(), &mut self.selected_vm_name);
            if let Some(action) =
                render_selected_vm(ui, self.application.vms(), &self.selected_vm_name)
            {
                selected_action = Some(action);
            }
            ui.add_space(12.0);
            render_diagnostics(ui, self.application.diagnostics());
            selected_action
        });

        if let Some(action) = action.inner {
            match action {
                VmAction::Create => self.create_vm_form = Some(CreateVmForm::default()),
                VmAction::Start | VmAction::Stop | VmAction::ForceStop => {
                    if let Some(name) = self.selected_vm_name.clone() {
                        let result = match action {
                            VmAction::Start => self.application.start_vm(&name),
                            VmAction::Stop => self.application.stop_vm(&name),
                            VmAction::ForceStop => self.application.force_stop_vm(&name),
                            _ => unreachable!("only lifecycle actions reach this branch"),
                        };
                        if result.is_ok() {
                            self.last_refresh = Instant::now();
                        }
                    }
                }
                _ => self.application.log_vm_action(action),
            }
            context.request_repaint();
        }

        let dialog_action = self
            .create_vm_form
            .as_mut()
            .and_then(|form| render_create_vm_dialog(context, form, self.application.vms()));
        match dialog_action {
            Some(CreateVmDialogAction::BrowseImage) => match self.application.pick_iso_image() {
                Ok(Some(path)) => {
                    if let Some(form) = &mut self.create_vm_form {
                        form.image_path = path;
                        form.error = None;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(form) = &mut self.create_vm_form {
                        form.error = Some(error.to_string());
                    }
                }
            },
            Some(CreateVmDialogAction::Cancel) => self.create_vm_form = None,
            Some(CreateVmDialogAction::Submit(request)) => {
                if let Err(error) = self.application.create_vm(request) {
                    if let Some(form) = &mut self.create_vm_form {
                        form.error = Some(error.to_string());
                    }
                } else {
                    self.create_vm_form = None;
                    self.last_refresh = Instant::now();
                }
            }
            None => {}
        }
    }
}

fn render_create_vm_dialog(
    context: &egui::Context,
    form: &mut CreateVmForm,
    existing_vms: &[VmSummary],
) -> Option<CreateVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new("New Linux VM")
        .collapsible(false)
        .resizable(false)
        .default_width(620.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label("Create a persistent Linux workspace from an ISO image.");
            ui.add_space(4.0);
            egui::Grid::new("create-vm-form")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("VM Name");
                    ui.add_sized([260.0, 0.0], egui::TextEdit::singleline(&mut form.name));
                    ui.end_row();

                    ui.label("OS Image");
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [300.0, 0.0],
                            egui::TextEdit::singleline(&mut form.image_path)
                                .hint_text("Path to ISO or VHDX..."),
                        );
                        if ui.button("Browse...").clicked() {
                            action = Some(CreateVmDialogAction::BrowseImage);
                        }
                    });
                    ui.end_row();

                    ui.label("HDD Size");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut form.disk_gb).range(1..=16_384));
                        ui.label("GiB");
                    });
                    ui.end_row();

                    ui.label("RAM Size");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut form.ram_mb).range(512..=1_048_576));
                        ui.label("MiB");
                    });
                    ui.end_row();

                    ui.label("CPU Cores");
                    ui.add(egui::DragValue::new(&mut form.cpu_cores).range(1..=256));
                    ui.end_row();

                    ui.label("GPU");
                    egui::ComboBox::from_id_salt("create-vm-gpu")
                        .selected_text(gpu_mode_label(form.gpu_mode))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut form.gpu_mode, GpuMode::Default, "Default");
                            ui.selectable_value(&mut form.gpu_mode, GpuMode::None, "None");
                        });
                    ui.end_row();

                    ui.label("Network");
                    egui::ComboBox::from_id_salt("create-vm-network")
                        .selected_text(network_mode_label(form.network_mode))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut form.network_mode, NetworkMode::Nat, "NAT");
                            ui.selectable_value(&mut form.network_mode, NetworkMode::None, "None");
                        });
                    ui.end_row();

                    ui.label("Username");
                    ui.add_sized([260.0, 0.0], egui::TextEdit::singleline(&mut form.username));
                    ui.end_row();

                    ui.label("Password");
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [140.0, 0.0],
                            egui::TextEdit::singleline(&mut form.password).password(true),
                        );
                        ui.label("Confirm");
                        ui.add_sized(
                            [140.0, 0.0],
                            egui::TextEdit::singleline(&mut form.password_confirmation)
                                .password(true),
                        );
                    });
                    ui.end_row();
                });

            ui.horizontal(|ui| {
                ui.label("Options");
                ui.checkbox(&mut form.ssh_enabled, "SSH Server");
                ui.add_enabled_ui(form.ssh_enabled, |ui| {
                    ui.checkbox(&mut form.ssh_deploy_key, "Deploy SSH key");
                });
            });
            if !form.ssh_enabled {
                form.ssh_deploy_key = false;
            }

            if let Some(error) = &form.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let create = ui.add(
                    egui::Button::new(egui::RichText::new("Create VM").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(47, 158, 97)),
                );
                if create.clicked() {
                    match create_vm_request(form, existing_vms) {
                        Ok(request) => action = Some(CreateVmDialogAction::Submit(request)),
                        Err(error) => form.error = Some(error),
                    }
                }
                if ui.button("Cancel").clicked() {
                    action = Some(CreateVmDialogAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(CreateVmDialogAction::Cancel);
    }
    action
}

fn create_vm_request(
    form: &CreateVmForm,
    existing_vms: &[VmSummary],
) -> Result<VmCreateRequest, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("VM name is required.".into());
    }
    if name.len() > 63
        || name.starts_with('-')
        || name.ends_with('-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Use a lowercase Linux hostname of up to 63 characters.".into());
    }
    if existing_vms
        .iter()
        .any(|vm| vm.name.eq_ignore_ascii_case(name))
    {
        return Err("A VM with this name already exists.".into());
    }
    if form.image_path.trim().is_empty() {
        return Err("A Linux ISO path is required.".into());
    }
    if form.disk_gb == 0 || form.ram_mb == 0 || form.cpu_cores == 0 {
        return Err("Disk, RAM, and CPU values must be greater than zero.".into());
    }

    let username = form.username.trim();
    if username.is_empty()
        || username.len() > 32
        || !username
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                0 => byte.is_ascii_lowercase() || byte == b'_',
                _ => {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                }
            })
    {
        return Err("Use a valid lowercase Linux username.".into());
    }
    if form.password.is_empty() {
        return Err("Password is required.".into());
    }
    if form.password != form.password_confirmation {
        return Err("Passwords do not match.".into());
    }

    Ok(VmCreateRequest {
        name: name.into(),
        image_path: form.image_path.trim().into(),
        ram_mb: form.ram_mb,
        disk_gb: form.disk_gb,
        cpu_cores: form.cpu_cores,
        gpu_mode: form.gpu_mode,
        network_mode: form.network_mode,
        username: username.into(),
        password: form.password.clone(),
        ssh_enabled: form.ssh_enabled,
        ssh_deploy_key: form.ssh_deploy_key,
    })
}

fn gpu_mode_label(mode: GpuMode) -> &'static str {
    match mode {
        GpuMode::None => "None",
        GpuMode::Default => "Default",
        GpuMode::Unknown(_) => "Unsupported",
    }
}

fn network_mode_label(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::None => "None",
        NetworkMode::Nat => "NAT",
        NetworkMode::External | NetworkMode::Internal | NetworkMode::Unknown(_) => "Unsupported",
    }
}

fn render_refresh_icon(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 24.0), sense);
    let painter = ui.painter();
    if enabled && response.hovered() {
        painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.bg_fill);
    }

    let color = if enabled {
        egui::Color32::from_rgb(85, 193, 233)
    } else {
        egui::Color32::from_gray(100)
    };
    let center = rect.center();
    let stroke = egui::Stroke::new(1.6_f32, color);
    painter.circle_stroke(center, 5.5, stroke);
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(center.x + 6.5, center.y - 5.0),
            egui::pos2(center.x + 6.5, center.y + 1.0),
            egui::pos2(center.x + 1.0, center.y - 2.0),
        ],
        color,
        egui::Stroke::NONE,
    ));

    response
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

fn render_vm_list(ui: &mut egui::Ui, vms: &[VmSummary], selected_vm_name: &mut Option<String>) {
    ui.heading("Workspaces");
    if vms.is_empty() {
        *selected_vm_name = None;
        ui.weak("No virtual machines found.");
        return;
    }

    if selected_vm_name
        .as_ref()
        .is_some_and(|name| !vms.iter().any(|vm| &vm.name == name))
    {
        *selected_vm_name = None;
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
                let is_selected = selected_vm_name.as_deref() == Some(vm.name.as_str());
                if ui.selectable_label(is_selected, &vm.name).clicked() {
                    *selected_vm_name = Some(vm.name.clone());
                }
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

fn render_selected_vm(
    ui: &mut egui::Ui,
    vms: &[VmSummary],
    selected_vm_name: &Option<String>,
) -> Option<VmAction> {
    let Some(name) = selected_vm_name else {
        return None;
    };
    let Some(vm) = vms.iter().find(|vm| vm.name == *name) else {
        return None;
    };

    ui.add_space(12.0);
    ui.separator();
    ui.heading(format!("Selected VM: {}", vm.name));

    let primary_action = match vm.state {
        VmState::Stopped => (VmAction::Start, "Start"),
        VmState::Starting | VmState::Running { .. } => (VmAction::Stop, "Stop"),
    };
    let is_running = matches!(vm.state, VmState::Running { .. });
    let mut action = None;
    ui.horizontal(|ui| {
        action = render_action_group(
            ui,
            &[primary_action, (VmAction::ForceStop, "Force stop")],
            true,
            None,
        );
        ui.separator();
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Connect, "Connect"), (VmAction::Ssh, "SSH")],
            is_running,
            Some("Available only when the VM is running"),
        ) {
            action = Some(clicked_action);
        }
        ui.separator();
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Edit, "Edit"), (VmAction::Delete, "Delete")],
            !is_running,
            Some("Unavailable while the VM is running"),
        ) {
            action = Some(clicked_action);
        }
    });

    ui.add_space(8.0);
    egui::Grid::new("selected-vm-details")
        .num_columns(2)
        .spacing([24.0, 6.0])
        .show(ui, |ui| {
            detail_row(
                ui,
                "IP address",
                vm.ip_address
                    .map_or_else(|| "Unavailable".into(), |ip| ip.to_string()),
            );
            detail_row(ui, "Operating system", vm.os_type.clone());
            detail_row(ui, "Status", vm_state(vm.state).into());
            detail_row(
                ui,
                "Agent status",
                agent_status_label(agent_status(vm.state)).into(),
            );
            detail_row(ui, "Network type", format!("{:?}", vm.network_mode));
            detail_row(ui, "CPU", format!("{} cores", vm.cpu_cores));
            detail_row(ui, "RAM", format!("{} MiB", vm.ram_mb));
            detail_row(ui, "Disk", format!("{} GiB", vm.disk_gb));
            detail_row(ui, "GPU", format!("{:?}", vm.gpu_mode));
            detail_row(
                ui,
                "SSH port",
                vm.ssh_port
                    .map_or_else(|| "Disabled".into(), |port| port.to_string()),
            );
        });

    action
}

fn render_action_group(
    ui: &mut egui::Ui,
    actions: &[(VmAction, &str)],
    enabled: bool,
    disabled_tooltip: Option<&str>,
) -> Option<VmAction> {
    let mut selected_action = None;
    ui.horizontal(|ui| {
        for (action, label) in actions {
            let response = render_action_icon(ui, *action, enabled);
            let tooltip = disabled_tooltip
                .map(|reason| format!("{label}: {reason}"))
                .unwrap_or_else(|| (*label).into());
            if enabled {
                response.clone().on_hover_text(tooltip);
                if response.clicked() {
                    selected_action = Some(*action);
                }
            } else {
                response.on_hover_text(tooltip);
            }
        }
    });
    selected_action
}

fn render_action_icon(ui: &mut egui::Ui, action: VmAction, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 24.0), sense);
    let painter = ui.painter();
    if enabled && response.hovered() {
        painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.bg_fill);
    }

    let color = if enabled {
        action_color(action)
    } else {
        action_color(action).gamma_multiply(0.45)
    };
    let stroke = egui::Stroke::new(1.6_f32, color);
    let center = rect.center();

    match action {
        VmAction::Create => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(12.0, 12.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 3.5, center.y),
                    egui::pos2(center.x + 3.5, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 3.5),
                    egui::pos2(center.x, center.y + 3.5),
                ],
                stroke,
            );
        }
        VmAction::Start => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(center.x - 4.0, center.y - 6.0),
                    egui::pos2(center.x - 4.0, center.y + 6.0),
                    egui::pos2(center.x + 6.0, center.y),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        VmAction::Stop => {
            painter.circle_stroke(center, 5.5, stroke);
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 7.0),
                    egui::pos2(center.x, center.y),
                ],
                stroke,
            );
        }
        VmAction::ForceStop => {
            painter.line_segment(
                [
                    egui::pos2(center.x - 5.0, center.y - 5.0),
                    egui::pos2(center.x + 5.0, center.y + 5.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 5.0, center.y - 5.0),
                    egui::pos2(center.x - 5.0, center.y + 5.0),
                ],
                stroke,
            );
        }
        VmAction::Connect => {
            let screen = egui::Rect::from_center_size(center, egui::vec2(12.0, 8.0));
            painter.rect_stroke(screen, 0.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(center.x, screen.bottom()),
                    egui::pos2(center.x, screen.bottom() + 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 3.5, screen.bottom() + 3.0),
                    egui::pos2(center.x + 3.5, screen.bottom() + 3.0),
                ],
                stroke,
            );
        }
        VmAction::Ssh => {
            painter.line_segment(
                [
                    egui::pos2(center.x - 5.0, center.y - 4.0),
                    egui::pos2(center.x - 1.0, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.0, center.y),
                    egui::pos2(center.x - 5.0, center.y + 4.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 1.0, center.y + 4.0),
                    egui::pos2(center.x + 6.0, center.y + 4.0),
                ],
                stroke,
            );
        }
        VmAction::Edit => {
            painter.line_segment(
                [
                    egui::pos2(center.x - 5.0, center.y + 5.0),
                    egui::pos2(center.x + 5.0, center.y - 5.0),
                ],
                egui::Stroke::new(3.0_f32, color),
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 6.0, center.y + 6.0),
                    egui::pos2(center.x - 3.0, center.y + 5.0),
                ],
                stroke,
            );
        }
        VmAction::Delete => {
            let bin = egui::Rect::from_center_size(
                egui::pos2(center.x, center.y + 1.5),
                egui::vec2(9.0, 10.0),
            );
            painter.rect_stroke(bin, 0.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(center.x - 6.0, center.y - 5.0),
                    egui::pos2(center.x + 6.0, center.y - 5.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 2.0, center.y - 7.0),
                    egui::pos2(center.x + 2.0, center.y - 7.0),
                ],
                stroke,
            );
        }
    }

    response
}

fn action_color(action: VmAction) -> egui::Color32 {
    match action {
        VmAction::Create | VmAction::Start => egui::Color32::from_rgb(84, 158, 230),
        VmAction::Stop => egui::Color32::from_rgb(235, 210, 64),
        VmAction::ForceStop => egui::Color32::from_rgb(225, 70, 70),
        VmAction::Connect | VmAction::Ssh => egui::Color32::from_rgb(85, 193, 233),
        VmAction::Edit => egui::Color32::from_rgb(235, 134, 58),
        VmAction::Delete => egui::Color32::LIGHT_GRAY,
    }
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: String) {
    ui.strong(label);
    ui.label(value);
    ui.end_row();
}

fn render_diagnostics(ui: &mut egui::Ui, diagnostics: &[vmlord_core::Diagnostic]) {
    ui.collapsing("Log", |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for diagnostic in diagnostics {
                    let color = match diagnostic.level {
                        DiagnosticLevel::Info => egui::Color32::LIGHT_GRAY,
                        DiagnosticLevel::Warning => egui::Color32::YELLOW,
                        DiagnosticLevel::Error => egui::Color32::LIGHT_RED,
                    };
                    ui.colored_label(color, &diagnostic.message);
                }
            });
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

fn agent_status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Unknown => "Unknown",
        AgentStatus::Offline => "Offline",
        AgentStatus::Online => "Online",
    }
}

fn vm_state(state: VmState) -> &'static str {
    match state {
        VmState::Stopped => "Stopped",
        VmState::Starting => "Building",
        VmState::Running { .. } => "Running",
    }
}
