//! Desktop shell for the first VMLord milestone.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui;
use vmlord_app::{BackendStatus, VmAction, WorkspaceApp};
use vmlord_core::{
    AgentStatus, AppSettings, BuildProgress, BuildStep, CloudImage, DiagnosticLevel, DownloadPhase,
    GpuMode, GuestDefaults, GuestReadinessTimeouts, Language, LogLevel, NetworkMode, Password,
    Provisioning, SshAccess, VmCreateRequest, VmDeleteRequest, VmSource, VmState, VmSummary,
    VmUpdateRequest, ubuntu,
};

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const VM_TABLE_COLUMN_COUNT: f32 = 9.0;

/// The releases the create form offers, newest first.
///
/// LTS only, which is the epic's boundary: an interim release is supported for
/// nine months, and a workspace outliving its own security updates is not what
/// a default should build. The list is written out rather than fetched --
/// Canonical publishes no machine-readable index of current releases -- and it
/// moves to the distribution profile when profiles come from JSON (#67).
///
/// All three are current: 26.04 is the newest LTS and the one a new VM gets by
/// default, 24.04 and 22.04 are still under standard support. Each was checked
/// against the file name the profile builds -- the server answers
/// `/releases/26.04/` with a redirect to its codename, and
/// `ubuntu-26.04-server-cloudimg-amd64.img` is listed in the `SHA256SUMS`
/// behind it, which is what the release resolver reads.
const UBUNTU_RELEASES: [&str; 3] = ["26.04", "24.04", "22.04"];

const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

/// The height a text field in a form claims.
///
/// Stated rather than left at zero: a widget added with no height of its own
/// makes its grid row shorter than what is drawn in it, and the row below then
/// starts inside it -- which is what made the combo box under "VM Name" and the
/// password field under "User name" overlap the fields above them.
const FIELD_HEIGHT: f32 = 24.0;

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
                edit_vm_form: None,
                delete_vm_form: None,
                settings_form: None,
            }))
        }),
    )
}

struct VmlordUi {
    application: WorkspaceApp,
    last_refresh: Instant,
    selected_vm_name: Option<String>,
    create_vm_form: Option<CreateVmForm>,
    edit_vm_form: Option<EditVmForm>,
    delete_vm_form: Option<DeleteVmForm>,
    settings_form: Option<SettingsForm>,
}

/// Where the new VM's system comes from, as the dialog's two radio buttons.
///
/// A copy of `VmSource`'s shape without its payload: a half-filled form is not
/// a source yet -- the fields for the other mode are still there, waiting for
/// the user to change their mind back -- and `VmSource` is built from it only
/// when the request is submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    CloudImage,
    LocalMedia,
}

/// Everything both kinds of source need, plus the provisioning only a cloud
/// image can carry.
///
/// The provisioning fields survive a switch to installation media rather than
/// being cleared: switching back is one click, and a form that forgets a typed
/// password because a radio button was pressed is the more annoying of the two
/// behaviours. What is not submitted is decided by `create_vm_request`, which
/// reads only the fields the chosen source has.
struct CreateVmForm {
    name: String,
    source_kind: SourceKind,
    /// Installation media: the path to the ISO the guest is installed from.
    image_path: String,
    /// Cloud image: the release, from [`UBUNTU_RELEASES`].
    release: String,
    username: String,
    /// Empty means no password at all: the guest is reachable by key only.
    password: String,
    ssh_enabled: bool,
    deploy_key: bool,
    locale: String,
    keyboard: String,
    timezone: String,
    disk_gb: u32,
    ram_mb: u32,
    cpu_cores: u32,
    gpu_mode: GpuMode,
    network_mode: NetworkMode,
    error: Option<String>,
}

struct SettingsForm {
    vm_storage_path: String,
    language: Language,
    log_file_path: String,
    log_level: LogLevel,
    /// Carried through the dialog unchanged: the settings form rebuilds the
    /// whole `AppSettings`, so a field it does not know about would be lost on
    /// every save. The widget for it arrives with the image download UI.
    image_cache_path: PathBuf,
    /// Carried through unchanged for the same reason as `image_cache_path`,
    /// and with no widget of its own on purpose: the readiness timeouts are
    /// edited in `settings.toml` on the rare occasion anyone needs to.
    guest_readiness: GuestReadinessTimeouts,
    error: Option<String>,
}

impl SettingsForm {
    fn from_settings(settings: &AppSettings) -> Self {
        Self {
            vm_storage_path: settings.vm_storage_path.display().to_string(),
            language: settings.language,
            log_file_path: settings.log_file_path.display().to_string(),
            log_level: settings.log_level,
            image_cache_path: settings.image_cache_path.clone(),
            guest_readiness: settings.guest_readiness,
            error: None,
        }
    }

    fn settings(&self) -> Result<AppSettings, String> {
        let vm_storage_path = self.vm_storage_path.trim();
        if vm_storage_path.is_empty() {
            return Err("VM storage path is required.".into());
        }
        let log_file_path = self.log_file_path.trim();
        if log_file_path.is_empty() {
            return Err("Log file path is required.".into());
        }

        Ok(AppSettings {
            vm_storage_path: PathBuf::from(vm_storage_path),
            language: self.language,
            log_file_path: PathBuf::from(log_file_path),
            log_level: self.log_level,
            image_cache_path: self.image_cache_path.clone(),
            guest_readiness: self.guest_readiness,
        })
    }
}

struct EditVmForm {
    name: String,
    ram_mb: u32,
    cpu_cores: u32,
    gpu_mode: GpuMode,
    network_mode: NetworkMode,
    error: Option<String>,
}

impl EditVmForm {
    fn from_vm(vm: &VmSummary) -> Self {
        Self {
            name: vm.name.clone(),
            ram_mb: vm.ram_mb,
            cpu_cores: vm.cpu_cores,
            gpu_mode: vm.gpu_mode,
            network_mode: vm.network_mode,
            error: None,
        }
    }
}

struct DeleteVmForm {
    vm_name: String,
    delete_disks: bool,
    error: Option<String>,
}

impl DeleteVmForm {
    fn for_vm(vm_name: &str) -> Self {
        Self {
            vm_name: vm_name.to_owned(),
            // Deleting the disks is what "delete the VM" normally means;
            // keeping them is the deliberate exception.
            delete_disks: true,
            error: None,
        }
    }
}

impl CreateVmForm {
    /// A form filled with what VMLord would do if nobody changed anything:
    /// the newest supported release, the distribution's own account name, and
    /// the host's locale, keyboard layout and timezone.
    fn new(guest_defaults: &GuestDefaults) -> Self {
        Self {
            name: "ubuntu".into(),
            source_kind: SourceKind::CloudImage,
            image_path: String::new(),
            release: UBUNTU_RELEASES[0].into(),
            username: ubuntu().default_user,
            password: String::new(),
            ssh_enabled: true,
            deploy_key: true,
            locale: guest_defaults.locale.clone(),
            keyboard: guest_defaults.keyboard.clone(),
            timezone: guest_defaults.timezone.clone(),
            disk_gb: 64,
            ram_mb: 4096,
            cpu_cores: 4,
            gpu_mode: GpuMode::Default,
            network_mode: NetworkMode::Nat,
            error: None,
        }
    }
}

enum CreateVmDialogAction {
    BrowseImage,
    Cancel,
    Submit(Box<VmCreateRequest>),
}

enum EditVmDialogAction {
    Cancel,
    Submit(VmUpdateRequest),
}

enum DeleteVmDialogAction {
    Cancel,
    Submit,
}

enum SettingsDialogAction {
    BrowseVmStorage,
    BrowseLogFile,
    Cancel,
    Submit(AppSettings),
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

                    let settings = ui.button("Settings");
                    settings.clone().on_hover_text("Open application settings");
                    if settings.clicked()
                        && let Some(current) = self.application.settings()
                    {
                        self.settings_form = Some(SettingsForm::from_settings(current));
                        self.create_vm_form = None;
                        self.edit_vm_form = None;
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
                VmAction::Create => {
                    self.create_vm_form =
                        Some(CreateVmForm::new(self.application.guest_defaults()));
                    self.edit_vm_form = None;
                }
                VmAction::Edit => {
                    if let Some(name) = self.selected_vm_name.as_deref()
                        && let Some(vm) = self.application.vms().iter().find(|vm| vm.name == name)
                    {
                        self.edit_vm_form = Some(EditVmForm::from_vm(vm));
                        self.create_vm_form = None;
                    }
                }
                VmAction::Start
                | VmAction::Stop
                | VmAction::ForceStop
                | VmAction::CancelCreate
                | VmAction::Connect
                | VmAction::Ssh => {
                    if let Some(name) = self.selected_vm_name.clone() {
                        let result = match action {
                            VmAction::Start => self.application.start_vm(&name),
                            VmAction::Stop => self.application.stop_vm(&name),
                            VmAction::ForceStop => self.application.force_stop_vm(&name),
                            VmAction::CancelCreate => self.application.cancel_create(&name),
                            VmAction::Connect => self.application.connect_display(&name),
                            VmAction::Ssh => self.application.open_ssh(&name),
                            _ => unreachable!("only supported VM actions reach this branch"),
                        };
                        if result.is_ok() {
                            self.last_refresh = Instant::now();
                        }
                    }
                }
                VmAction::Delete => {
                    if let Some(name) = self.selected_vm_name.clone() {
                        self.delete_vm_form = Some(DeleteVmForm::for_vm(&name));
                        self.create_vm_form = None;
                        self.edit_vm_form = None;
                    }
                }
            }
            context.request_repaint();
        }

        // Asked before the form is borrowed for drawing, and asked of the
        // backend: where a VM's key pair goes is the platform layer's to know.
        let ssh_key_path = self
            .create_vm_form
            .as_ref()
            .and_then(|form| self.application.ssh_key_path(form.name.trim()));
        let create_dialog_action = self.create_vm_form.as_mut().and_then(|form| {
            render_create_vm_dialog(
                context,
                form,
                self.application.vms(),
                ssh_key_path.as_deref(),
            )
        });
        match create_dialog_action {
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
                if let Err(error) = self.application.create_vm(*request) {
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

        let settings_dialog_action = self
            .settings_form
            .as_mut()
            .and_then(|form| render_settings_dialog(context, form));
        match settings_dialog_action {
            Some(SettingsDialogAction::BrowseVmStorage) => {
                match self.application.pick_vm_storage_directory() {
                    Ok(Some(path)) => {
                        if let Some(form) = &mut self.settings_form {
                            form.vm_storage_path = path;
                            form.error = None;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if let Some(form) = &mut self.settings_form {
                            form.error = Some(error.to_string());
                        }
                    }
                }
            }
            Some(SettingsDialogAction::BrowseLogFile) => match self.application.pick_log_file() {
                Ok(Some(path)) => {
                    if let Some(form) = &mut self.settings_form {
                        form.log_file_path = path;
                        form.error = None;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(form) = &mut self.settings_form {
                        form.error = Some(error.to_string());
                    }
                }
            },
            Some(SettingsDialogAction::Cancel) => self.settings_form = None,
            Some(SettingsDialogAction::Submit(settings)) => {
                if let Err(error) = self.application.update_settings(settings) {
                    if let Some(form) = &mut self.settings_form {
                        form.error = Some(error.to_string());
                    }
                } else {
                    self.settings_form = None;
                }
            }
            None => {}
        }

        let edit_dialog_action = self
            .edit_vm_form
            .as_mut()
            .and_then(|form| render_edit_vm_dialog(context, form));
        match edit_dialog_action {
            Some(EditVmDialogAction::Cancel) => self.edit_vm_form = None,
            Some(EditVmDialogAction::Submit(request)) => {
                if let Err(error) = self.application.update_vm(request) {
                    if let Some(form) = &mut self.edit_vm_form {
                        form.error = Some(error.to_string());
                    }
                } else {
                    self.edit_vm_form = None;
                    self.last_refresh = Instant::now();
                }
            }
            None => {}
        }

        let delete_dialog_action = self
            .delete_vm_form
            .as_mut()
            .and_then(|form| render_delete_vm_dialog(context, form));
        match delete_dialog_action {
            Some(DeleteVmDialogAction::Cancel) => self.delete_vm_form = None,
            Some(DeleteVmDialogAction::Submit) => {
                let request = self.delete_vm_form.as_ref().map(|form| VmDeleteRequest {
                    name: form.vm_name.clone(),
                    delete_disks: form.delete_disks,
                });
                if let Some(request) = request {
                    match self.application.delete_vm(request) {
                        Ok(()) => {
                            self.delete_vm_form = None;
                            self.selected_vm_name = None;
                            self.last_refresh = Instant::now();
                        }
                        Err(error) => {
                            if let Some(form) = &mut self.delete_vm_form {
                                form.error = Some(error.to_string());
                            }
                        }
                    }
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
    ssh_key_path: Option<&Path>,
) -> Option<CreateVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new("New Linux VM")
        .collapsible(false)
        .resizable(false)
        .default_width(620.0)
        .open(&mut open)
        .show(context, |ui| {
            // The form is taller than the main window when a cloud image is
            // chosen, and a dialog whose Create button is off-screen cannot be
            // submitted: the fields scroll, the buttons below do not.
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
                    ui.label("Create a persistent Linux workspace.");
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.strong("System");
                        ui.radio_value(
                            &mut form.source_kind,
                            SourceKind::CloudImage,
                            "Cloud image (ready to use)",
                        );
                        ui.radio_value(
                            &mut form.source_kind,
                            SourceKind::LocalMedia,
                            "Own ISO (installed by hand)",
                        );
                    });
                    match form.source_kind {
                        SourceKind::CloudImage => ui.small(
                            "The image is downloaded once and configured on the first boot: \
                             the user below, their login and the guest settings.",
                        ),
                        SourceKind::LocalMedia => ui.small(
                            "The installer runs in the VM and asks for the user, the password \
                             and the guest settings itself, so VMLord configures none of them.",
                        ),
                    };

                    ui.add_space(8.0);
                    egui::Grid::new("create-vm-form")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("VM Name");
                            ui.add_sized(
                                [260.0, FIELD_HEIGHT],
                                egui::TextEdit::singleline(&mut form.name),
                            );
                            ui.end_row();

                            match form.source_kind {
                                SourceKind::CloudImage => {
                                    ui.label("Distribution");
                                    // One entry until distribution profiles are read
                                    // from a file (#67); the guest's account name and
                                    // its admin group come from the same profile.
                                    egui::ComboBox::from_id_salt("create-vm-distribution")
                                        .selected_text(ubuntu().name)
                                        .show_ui(ui, |ui| {
                                            ui.label(ubuntu().name);
                                        });
                                    ui.end_row();

                                    ui.label("Release");
                                    egui::ComboBox::from_id_salt("create-vm-release")
                                        .selected_text(release_label(&form.release))
                                        .show_ui(ui, |ui| {
                                            for release in UBUNTU_RELEASES {
                                                ui.selectable_value(
                                                    &mut form.release,
                                                    release.to_owned(),
                                                    release_label(release),
                                                );
                                            }
                                        });
                                    ui.end_row();
                                }
                                SourceKind::LocalMedia => {
                                    ui.label("OS Image");
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [300.0, FIELD_HEIGHT],
                                            egui::TextEdit::singleline(&mut form.image_path)
                                                .hint_text("Path to ISO or VHDX..."),
                                        );
                                        if ui.button("Browse...").clicked() {
                                            action = Some(CreateVmDialogAction::BrowseImage);
                                        }
                                    });
                                    ui.end_row();
                                }
                            }

                            ui.label("HDD Size");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut form.disk_gb).range(1..=16_384));
                                ui.label("GiB");
                            });
                            ui.end_row();

                            ui.label("RAM Size");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut form.ram_mb).range(512..=1_048_576),
                                );
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
                                    ui.selectable_value(
                                        &mut form.gpu_mode,
                                        GpuMode::Default,
                                        "Default",
                                    );
                                    ui.selectable_value(
                                        &mut form.gpu_mode,
                                        GpuMode::TryAll,
                                        "Try all",
                                    );
                                    ui.selectable_value(&mut form.gpu_mode, GpuMode::None, "None");
                                });
                            ui.end_row();

                            ui.label("Network");
                            egui::ComboBox::from_id_salt("create-vm-network")
                                .selected_text(network_mode_label(form.network_mode))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut form.network_mode,
                                        NetworkMode::Nat,
                                        "NAT",
                                    );
                                    ui.selectable_value(
                                        &mut form.network_mode,
                                        NetworkMode::None,
                                        "None",
                                    );
                                });
                            ui.end_row();
                        });

                    if form.source_kind == SourceKind::CloudImage {
                        ui.add_space(10.0);
                        ui.separator();
                        ui.strong("Guest");
                        ui.add_space(4.0);
                        render_provisioning_fields(ui, form, ssh_key_path);
                    }

                    if let Some(error) = &form.error {
                        ui.add_space(4.0);
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                });

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let create = ui.add(
                    egui::Button::new(egui::RichText::new("Create VM").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(47, 158, 97)),
                );
                if create.clicked() {
                    match create_vm_request(form, existing_vms) {
                        Ok(request) => {
                            action = Some(CreateVmDialogAction::Submit(Box::new(request)))
                        }
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

/// The fields only a cloud image can carry: who the guest's user is, how they
/// log in, and the three settings cloud-init applies on the first boot.
fn render_provisioning_fields(
    ui: &mut egui::Ui,
    form: &mut CreateVmForm,
    ssh_key_path: Option<&Path>,
) {
    egui::Grid::new("create-vm-provisioning")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("User name");
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.username),
            );
            ui.end_row();

            ui.label("Password");
            ui.vertical(|ui| {
                ui.add_sized(
                    [260.0, FIELD_HEIGHT],
                    egui::TextEdit::singleline(&mut form.password)
                        .password(true)
                        .hint_text("Optional"),
                );
                if form.password.is_empty() {
                    ui.small(
                        "No password: the guest is reachable by SSH key only, \
                         and password logins are turned off. The COM1 console \
                         cannot log in either, so a guest whose network fails \
                         is out of reach.",
                    );
                }
            });
            ui.end_row();

            ui.label("SSH");
            ui.vertical(|ui| {
                ui.checkbox(&mut form.ssh_enabled, "Run an SSH server in the guest");
                ui.add_enabled_ui(form.ssh_enabled, |ui| {
                    ui.checkbox(
                        &mut form.deploy_key,
                        "Generate a key pair for this VM and install the public half",
                    );
                });
                if form.ssh_enabled && form.deploy_key {
                    match ssh_key_path {
                        Some(path) => {
                            ui.small(format!("Private key: {}", path.display()));
                        }
                        None => {
                            ui.small("The private key is stored in the VM's own folder.");
                        }
                    }
                }
            });
            ui.end_row();

            ui.label("Locale");
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.locale),
            );
            ui.end_row();

            ui.label("Keyboard layout");
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.keyboard),
            );
            ui.end_row();

            ui.label("Timezone");
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.timezone),
            );
            ui.end_row();
        });
    ui.small("The three settings above are filled in from this computer and applied to the guest.");
}

fn render_settings_dialog(
    context: &egui::Context,
    form: &mut SettingsForm,
) -> Option<SettingsDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new("Application settings")
        .collapsible(false)
        .resizable(false)
        .default_width(600.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label("Configure where VMLord stores VM data and diagnostic logs.");
            ui.add_space(8.0);
            egui::Grid::new("application-settings-form")
                .num_columns(2)
                .min_col_width(110.0)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.add_sized([110.0, 24.0], egui::Label::new("VM storage"));
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [310.0, 24.0],
                            egui::TextEdit::singleline(&mut form.vm_storage_path)
                                .hint_text("Directory for virtual machine data"),
                        );
                        if ui.button("Browse...").clicked() {
                            action = Some(SettingsDialogAction::BrowseVmStorage);
                        }
                    });
                    ui.end_row();

                    ui.add_sized([110.0, 24.0], egui::Label::new("Language"));
                    egui::ComboBox::from_id_salt("settings-language")
                        .width(310.0)
                        .selected_text(language_label(form.language))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut form.language, Language::EnUs, "English (US)");
                        });
                    ui.end_row();

                    ui.add_sized([110.0, 24.0], egui::Label::new("Log file"));
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [310.0, 24.0],
                            egui::TextEdit::singleline(&mut form.log_file_path)
                                .hint_text("Path to the log file"),
                        );
                        if ui.button("Browse...").clicked() {
                            action = Some(SettingsDialogAction::BrowseLogFile);
                        }
                    });
                    ui.end_row();

                    ui.add_sized([110.0, 24.0], egui::Label::new("Log level"));
                    egui::ComboBox::from_id_salt("settings-log-level")
                        .selected_text(log_level_label(form.log_level))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut form.log_level, LogLevel::Error, "Error");
                            ui.selectable_value(&mut form.log_level, LogLevel::Warn, "Warning");
                            ui.selectable_value(&mut form.log_level, LogLevel::Info, "Info");
                            ui.selectable_value(&mut form.log_level, LogLevel::Debug, "Debug");
                            ui.selectable_value(&mut form.log_level, LogLevel::Trace, "Trace");
                        });
                    ui.end_row();
                });

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = ui.add(
                    egui::Button::new(egui::RichText::new("Save").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(47, 158, 97))
                        .min_size(egui::vec2(88.0, 30.0)),
                );
                if save.clicked() {
                    match form.settings() {
                        Ok(settings) => action = Some(SettingsDialogAction::Submit(settings)),
                        Err(error) => form.error = Some(error),
                    }
                }
                let cancel = ui.add(
                    egui::Button::new(egui::RichText::new("Cancel").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(100, 100, 100))
                        .min_size(egui::vec2(88.0, 30.0)),
                );
                if cancel.clicked() {
                    action = Some(SettingsDialogAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(SettingsDialogAction::Cancel);
    }
    action
}

fn render_edit_vm_dialog(
    context: &egui::Context,
    form: &mut EditVmForm,
) -> Option<EditVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(format!("Edit VM: {}", form.name))
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label("Changes are saved to the VM configuration and take effect the next time the VM starts.");
            ui.small(
                "RAM and CPU are editable. GPU and network are not wired to the native backend yet. Disk size and VM name stay fixed and currently require recreating the VM.",
            );
            ui.add_space(8.0);

            egui::Grid::new("edit-vm-form")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("VM Name");
                    ui.label(&form.name);
                    ui.end_row();

                    ui.label("RAM Size");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut form.ram_mb).range(512..=1_048_576).speed(2));
                        ui.label("MiB");
                    });
                    ui.end_row();

                    ui.label("CPU Cores");
                    ui.add(egui::DragValue::new(&mut form.cpu_cores).range(1..=256));
                    ui.end_row();

                    ui.label("GPU");
                    egui::ComboBox::from_id_salt("edit-vm-gpu")
                        .selected_text(gpu_mode_label(form.gpu_mode))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut form.gpu_mode, GpuMode::Default, "Default");
                            ui.selectable_value(&mut form.gpu_mode, GpuMode::TryAll, "Try all");
                            ui.selectable_value(&mut form.gpu_mode, GpuMode::None, "None");
                        });
                    ui.end_row();

                    ui.label("Network");
                    egui::ComboBox::from_id_salt("edit-vm-network")
                        .selected_text(network_mode_label(form.network_mode))
                        .show_ui(ui, |ui| {
                            // The same two modes the create form offers: the
                            // native backend refuses the rest until #10, and an
                            // option that always fails is a poor way to say so.
                            ui.selectable_value(&mut form.network_mode, NetworkMode::Nat, "NAT");
                            ui.selectable_value(&mut form.network_mode, NetworkMode::None, "None");
                        });
                    ui.end_row();
                });

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = ui.add(
                    egui::Button::new(egui::RichText::new("Save changes").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(235, 134, 58)),
                );
                if save.clicked() {
                    match edit_vm_request(form) {
                        Ok(request) => action = Some(EditVmDialogAction::Submit(request)),
                        Err(error) => form.error = Some(error),
                    }
                }
                if ui.button("Cancel").clicked() {
                    action = Some(EditVmDialogAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(EditVmDialogAction::Cancel);
    }
    action
}

fn render_delete_vm_dialog(
    context: &egui::Context,
    form: &mut DeleteVmForm,
) -> Option<DeleteVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(format!("Delete VM: {}", form.vm_name))
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(format!(
                "VM \"{}\" and its stored configuration will be removed. This cannot be undone.",
                form.vm_name
            ));
            ui.add_space(8.0);
            ui.checkbox(&mut form.delete_disks, "Delete virtual disks");
            if form.delete_disks {
                ui.small("The VM's virtual disks are deleted with it. The image it was installed from is not touched.");
            } else {
                ui.small("The virtual disks are kept, so the VM's directory stays in place and a new VM cannot reuse that name.");
            }

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let delete = ui.add(
                    egui::Button::new(egui::RichText::new("Delete").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(192, 57, 43)),
                );
                if delete.clicked() {
                    action = Some(DeleteVmDialogAction::Submit);
                }
                if ui.button("Cancel").clicked() {
                    action = Some(DeleteVmDialogAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(DeleteVmDialogAction::Cancel);
    }
    action
}

/// Turns the form into the request, and reports why it is not one yet.
///
/// The rules themselves are not here: `VmCreateRequest::validate` owns them --
/// the VM name, the user name, the password, the guest settings -- and this
/// function's own check is the one the domain cannot make, because it is about
/// the list on screen rather than about the request.
fn create_vm_request(
    form: &CreateVmForm,
    existing_vms: &[VmSummary],
) -> Result<VmCreateRequest, String> {
    let name = form.name.trim();
    if existing_vms
        .iter()
        .any(|vm| vm.name.eq_ignore_ascii_case(name))
    {
        return Err("A VM with this name already exists.".into());
    }

    let request = VmCreateRequest {
        name: name.into(),
        source: create_vm_source(form),
        ram_mb: form.ram_mb,
        disk_gb: form.disk_gb,
        cpu_cores: form.cpu_cores,
        gpu_mode: form.gpu_mode,
        network_mode: form.network_mode,
    };
    request.validate().map_err(|error| error.to_string())?;
    Ok(request)
}

/// Reads the fields the chosen source has, and only those.
fn create_vm_source(form: &CreateVmForm) -> VmSource {
    match form.source_kind {
        SourceKind::LocalMedia => VmSource::LocalMedia {
            path: form.image_path.trim().into(),
        },
        SourceKind::CloudImage => VmSource::CloudImage {
            image: CloudImage {
                profile: ubuntu(),
                release: form.release.clone(),
            },
            provisioning: Provisioning {
                username: form.username.trim().into(),
                // Not trimmed, and empty rather than blank-checked: a space is
                // a character of a password, and "no password" is the field
                // being left alone.
                password: (!form.password.is_empty()).then(|| Password::new(form.password.clone())),
                ssh: if form.ssh_enabled {
                    SshAccess::Enabled {
                        deploy_key: form.deploy_key,
                    }
                } else {
                    SshAccess::Disabled
                },
                locale: form.locale.trim().into(),
                keyboard: form.keyboard.trim().into(),
                timezone: form.timezone.trim().into(),
            },
        },
    }
}

/// Names a release the way the distribution does.
fn release_label(release: &str) -> String {
    format!("{} {release} LTS", ubuntu().name)
}

fn edit_vm_request(form: &EditVmForm) -> Result<VmUpdateRequest, String> {
    if form.ram_mb < 512 || !form.ram_mb.is_multiple_of(2) {
        return Err("RAM must be an even number of MiB and at least 512 MiB.".into());
    }
    if form.cpu_cores == 0 {
        return Err("CPU cores must be at least 1.".into());
    }
    if matches!(form.gpu_mode, GpuMode::Unknown(_)) {
        return Err("The current GPU mode is not supported by the Rust UI yet.".into());
    }
    if matches!(form.network_mode, NetworkMode::Unknown(_)) {
        return Err("The current network mode is not supported by the Rust UI yet.".into());
    }

    Ok(VmUpdateRequest {
        name: form.name.clone(),
        ram_mb: form.ram_mb,
        cpu_cores: form.cpu_cores,
        gpu_mode: form.gpu_mode,
        network_mode: form.network_mode,
    })
}

fn log_level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "Error",
        LogLevel::Warn => "Warning",
        LogLevel::Info => "Info",
        LogLevel::Debug => "Debug",
        LogLevel::Trace => "Trace",
    }
}

fn language_label(language: Language) -> &'static str {
    match language {
        Language::EnUs => "English (US)",
    }
}

fn gpu_mode_label(mode: GpuMode) -> &'static str {
    match mode {
        GpuMode::None => "None",
        GpuMode::Default => "Default",
        GpuMode::TryAll => "Try all",
        GpuMode::Unknown(_) => "Unsupported",
    }
}

fn network_mode_label(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::None => "None",
        NetworkMode::Nat => "NAT",
        NetworkMode::External => "External",
        NetworkMode::Internal => "Internal",
        NetworkMode::Unknown(_) => "Unsupported",
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
                ui.label(vm_state_label(vm.state));
                render_agent_status(ui, agent_status(vm.state));
                ui.label(format!("{} cores", vm.cpu_cores));
                ui.label(format!("{} MiB", vm.ram_mb));
                ui.label(format!("{} GiB", vm.disk_gb));
                ui.label(gpu_mode_label(vm.gpu_mode));
                ui.label(network_mode_label(vm.network_mode));
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
    let vm = vms.iter().find(|vm| vm.name == *name)?;

    ui.add_space(12.0);
    ui.separator();
    ui.heading(format!("Selected VM: {}", vm.name));

    let primary_action = match vm.state {
        VmState::Stopped | VmState::Building { .. } => (VmAction::Start, "Start"),
        VmState::Starting | VmState::Running { .. } => (VmAction::Stop, "Stop"),
    };
    let is_running = matches!(vm.state, VmState::Running { .. });
    // A VM that is still being created has nothing to start, stop, edit or
    // delete yet: what exists of it is a directory the build still owns.
    let is_building = matches!(vm.state, VmState::Building { .. });
    let can_delete = matches!(vm.state, VmState::Stopped);
    let mut action = None;
    ui.horizontal(|ui| {
        action = render_action_group(
            ui,
            &[primary_action, (VmAction::ForceStop, "Force stop")],
            !is_building,
            Some("Available when the VM has finished building"),
        );
        ui.separator();
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Connect, "Connect")],
            is_running,
            Some("Available only when the VM is running"),
        ) {
            action = Some(clicked_action);
        }
        let ssh_ready = is_running && vm.ssh_port.is_some();
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Ssh, "SSH")],
            ssh_ready,
            Some("Available when the SSH proxy is ready"),
        ) {
            action = Some(clicked_action);
        }
        ui.separator();
        // The only thing that can be done to a VM while it is being built, and
        // the only time it can be done: the build rolls itself back and the
        // row disappears on its own.
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::CancelCreate, "Cancel creation")],
            is_building,
            Some("Available only while the VM is being created"),
        ) {
            action = Some(clicked_action);
        }
        ui.separator();
        // Editing a running VM is allowed; the change reaches it on its next
        // start. Deleting one is not.
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Edit, "Edit")],
            !is_building,
            Some("Changes to a running VM apply after a restart"),
        ) {
            action = Some(clicked_action);
        }
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Delete, "Delete")],
            can_delete,
            Some("Available only when the VM is stopped"),
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
            if let VmState::Building { progress } = vm.state {
                render_build_progress(ui, progress);
            }
            detail_row(
                ui,
                "Agent status",
                agent_status_label(agent_status(vm.state)).into(),
            );
            detail_row(
                ui,
                "Network type",
                network_mode_label(vm.network_mode).into(),
            );
            detail_row(ui, "CPU", format!("{} cores", vm.cpu_cores));
            detail_row(ui, "RAM", format!("{} MiB", vm.ram_mb));
            detail_row(ui, "Disk", format!("{} GiB", vm.disk_gb));
            detail_row(ui, "GPU", gpu_mode_label(vm.gpu_mode).into());
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
        VmAction::CancelCreate => {
            painter.circle_stroke(center, 6.0, stroke);
            painter.line_segment(
                [
                    egui::pos2(center.x - 4.2, center.y - 4.2),
                    egui::pos2(center.x + 4.2, center.y + 4.2),
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
        VmAction::CancelCreate => egui::Color32::from_rgb(235, 170, 64),
        VmAction::Connect | VmAction::Ssh => egui::Color32::from_rgb(85, 193, 233),
        VmAction::Edit => egui::Color32::from_rgb(235, 134, 58),
        VmAction::Delete => egui::Color32::LIGHT_GRAY,
    }
}

/// One row of the details grid for a VM that is still being built: a bar while
/// the image is being fetched, and the byte counts under it.
///
/// The bar appears only for the download, which is the one step that publishes
/// counts. The others draw no bar rather than an empty one, because a bar
/// standing at zero for two minutes says the opposite of what is happening.
fn render_build_progress(ui: &mut egui::Ui, progress: BuildProgress) {
    let Some(detail) = build_detail(progress) else {
        return;
    };

    ui.strong("Progress");
    ui.vertical(|ui| {
        if let Some(percent) = download_percentage(progress) {
            ui.add(
                egui::ProgressBar::new(percent as f32 / 100.0)
                    .desired_width(260.0)
                    .text(format!("{percent}%")),
            );
        }
        ui.label(detail);
    });
    ui.end_row();
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
        VmState::Stopped | VmState::Building { .. } | VmState::Starting => AgentStatus::Unknown,
    }
}

fn agent_status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Unknown => "Unknown",
        AgentStatus::Offline => "Offline",
        AgentStatus::Online => "Online",
    }
}

/// The status column's text: the step, and how far into it the build is when
/// that is a number.
///
/// The percentage is here and not only in the details panel because the list is
/// what a person watches while an image downloads, and selecting a row to see
/// whether anything is happening is not watching.
fn vm_state_label(state: VmState) -> String {
    let label = vm_state(state);
    match state {
        VmState::Building { progress } => match download_percentage(progress) {
            Some(percent) => format!("{label} {percent}%"),
            None => label.to_owned(),
        },
        _ => label.to_owned(),
    }
}

/// How far the image transfer has got, when it is a fraction of something
/// known.
///
/// A server that sent no length gives no denominator, and connecting and
/// hashing-complete are not fractions of anything -- hence `None` rather than a
/// zero that would read as no progress.
fn download_percentage(progress: BuildProgress) -> Option<u64> {
    match progress.download? {
        DownloadPhase::Downloading {
            downloaded,
            total: Some(total),
        } => Some(percentage(downloaded, total)),
        DownloadPhase::Verifying { hashed, total } => Some(percentage(hashed, total)),
        DownloadPhase::Downloading { total: None, .. }
        | DownloadPhase::Connecting
        | DownloadPhase::Completed => None,
    }
}

/// What a build is doing inside the step it reports, when there is more to say
/// than the step's own name.
///
/// Only the download has anything: writing the disk, provisioning and
/// registering publish no counts of their own, and inventing a percentage for
/// them would be inventing a denominator. `None` therefore means the step name
/// already says everything, not that progress was lost.
fn build_detail(progress: BuildProgress) -> Option<String> {
    Some(match progress.download? {
        DownloadPhase::Connecting => "Connecting to the image server".into(),
        DownloadPhase::Downloading {
            downloaded,
            total: Some(total),
        } => format!(
            "Downloaded {} of {} ({}%)",
            mebibytes(downloaded),
            mebibytes(total),
            percentage(downloaded, total)
        ),
        // A server that sent no length leaves nothing to divide by; the count
        // still shows the download is moving.
        DownloadPhase::Downloading {
            downloaded,
            total: None,
        } => format!("Downloaded {}", mebibytes(downloaded)),
        DownloadPhase::Verifying { hashed, total } => format!(
            "Checking the image: {} of {} ({}%)",
            mebibytes(hashed),
            mebibytes(total),
            percentage(hashed, total)
        ),
        DownloadPhase::Completed => "Image ready".into(),
    })
}

fn mebibytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / BYTES_PER_MIB)
}

/// A whole percentage, and never 100 before the last byte: a bar that reads
/// "100%" while the work continues is the one people wait on.
fn percentage(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (done.min(total) * 100 / total).min(if done < total { 99 } else { 100 })
}

fn vm_state(state: VmState) -> &'static str {
    match state {
        VmState::Stopped => "Stopped",
        VmState::Building { progress } => match progress.step {
            BuildStep::Downloading => "Building: downloading",
            BuildStep::WritingDisk => "Building: writing the disk",
            BuildStep::Provisioning => "Building: provisioning",
            BuildStep::Registering => "Building: registering",
            BuildStep::Starting => "Building: starting the VM",
            BuildStep::AwaitingGuest => "Building: waiting for the guest",
        },
        VmState::Starting => "Starting",
        VmState::Running { .. } => "Running",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creating a VM no longer ends at a registered compute system, so the two
    /// steps that follow have to be legible in the list rather than reading as
    /// a build that has stalled at "registering".
    #[test]
    fn the_steps_after_registering_have_labels_of_their_own() {
        use vmlord_core::{BuildProgress, BuildStep};

        let label = |step| {
            vm_state(VmState::Building {
                progress: BuildProgress {
                    step,
                    download: None,
                },
            })
        };

        assert_eq!(label(BuildStep::Starting), "Building: starting the VM");
        assert_eq!(
            label(BuildStep::AwaitingGuest),
            "Building: waiting for the guest"
        );
    }

    /// `Starting` and `Building` are different things, and the label said
    /// "Building" for `Starting` only because there was no building state yet.
    #[test]
    fn each_state_gets_its_own_label() {
        use vmlord_core::{BuildProgress, BuildStep};

        assert_eq!(vm_state(VmState::Stopped), "Stopped");
        assert_eq!(vm_state(VmState::Starting), "Starting");
        assert_eq!(
            vm_state(VmState::Building {
                progress: BuildProgress {
                    step: BuildStep::Downloading,
                    download: None,
                },
            }),
            "Building: downloading"
        );
        assert_eq!(
            vm_state(VmState::Building {
                progress: BuildProgress {
                    step: BuildStep::Registering,
                    download: None,
                },
            }),
            "Building: registering"
        );
    }

    fn cloud_form() -> CreateVmForm {
        CreateVmForm::new(&GuestDefaults {
            locale: "ru_RU.UTF-8".into(),
            keyboard: "ru".into(),
            timezone: "Europe/Moscow".into(),
        })
    }

    fn provisioning_of(request: &VmCreateRequest) -> &Provisioning {
        match &request.source {
            VmSource::CloudImage { provisioning, .. } => provisioning,
            VmSource::LocalMedia { .. } => panic!("expected a cloud image request"),
        }
    }

    #[test]
    fn a_new_form_offers_the_hosts_settings_and_the_distributions_own_account() {
        let form = cloud_form();

        assert_eq!(form.locale, "ru_RU.UTF-8");
        assert_eq!(form.keyboard, "ru");
        assert_eq!(form.timezone, "Europe/Moscow");
        assert_eq!(
            form.username,
            ubuntu().default_user,
            "the account a cloud image already expects is the one to offer"
        );
        assert_eq!(
            form.release, "26.04",
            "a new VM starts from the newest LTS, which is the first of the offered releases"
        );
        assert_eq!(form.release, UBUNTU_RELEASES[0]);
        for release in UBUNTU_RELEASES {
            assert!(
                vmlord_core::ubuntu()
                    .image_url(release)
                    .ends_with(&format!("ubuntu-{release}-server-cloudimg-amd64.img")),
                "the resolver has to be able to build a URL for {release}"
            );
        }
    }

    #[test]
    fn a_cloud_form_becomes_a_request_carrying_what_the_guest_is_configured_with() {
        let form = CreateVmForm {
            name: "  dev-linux  ".into(),
            username: " dev ".into(),
            password: "hunter2".into(),
            release: "22.04".into(),
            ..cloud_form()
        };

        let request = create_vm_request(&form, &[]).unwrap();

        assert_eq!(request.name, "dev-linux");
        let VmSource::CloudImage { image, .. } = &request.source else {
            panic!("expected a cloud image request");
        };
        assert_eq!(image.release, "22.04");
        assert_eq!(image.profile, ubuntu());
        let provisioning = provisioning_of(&request);
        assert_eq!(provisioning.username, "dev");
        assert_eq!(
            provisioning.password.as_ref().map(Password::as_str),
            Some("hunter2")
        );
        assert_eq!(provisioning.ssh, SshAccess::Enabled { deploy_key: true });
        assert_eq!(provisioning.locale, "ru_RU.UTF-8");
        assert_eq!(provisioning.keyboard, "ru");
        assert_eq!(provisioning.timezone, "Europe/Moscow");
    }

    /// An empty password field is a choice, not a missing value: the guest gets
    /// no password at all and cloud-init turns password logins off.
    #[test]
    fn an_untouched_password_field_means_a_key_only_login() {
        let request = create_vm_request(&cloud_form(), &[]).unwrap();

        assert_eq!(provisioning_of(&request).password, None);
    }

    /// A space is a character of a password, so the field is not trimmed the
    /// way the names beside it are.
    #[test]
    fn a_password_keeps_the_spaces_it_was_typed_with() {
        let form = CreateVmForm {
            password: " two words ".into(),
            ..cloud_form()
        };

        let request = create_vm_request(&form, &[]).unwrap();

        assert_eq!(
            provisioning_of(&request)
                .password
                .as_ref()
                .map(Password::as_str),
            Some(" two words ")
        );
    }

    #[test]
    fn the_two_ssh_toggles_reach_the_request() {
        let key_less = CreateVmForm {
            deploy_key: false,
            ..cloud_form()
        };
        assert_eq!(
            provisioning_of(&create_vm_request(&key_less, &[]).unwrap()).ssh,
            SshAccess::Enabled { deploy_key: false }
        );

        let no_ssh = CreateVmForm {
            ssh_enabled: false,
            password: "hunter2".into(),
            ..cloud_form()
        };
        assert_eq!(
            provisioning_of(&create_vm_request(&no_ssh, &[]).unwrap()).ssh,
            SshAccess::Disabled
        );
    }

    /// The form states the rules nowhere: it hands the request to the domain
    /// and shows what comes back, so a rule changed there changes here too.
    #[test]
    fn the_domains_own_words_are_what_the_form_shows() {
        let bad_username = CreateVmForm {
            username: "Dev Linux".into(),
            ..cloud_form()
        };
        assert!(
            create_vm_request(&bad_username, &[])
                .unwrap_err()
                .contains("user name")
        );

        let bad_name = CreateVmForm {
            name: "Dev_Linux".into(),
            ..cloud_form()
        };
        assert!(
            create_vm_request(&bad_name, &[])
                .unwrap_err()
                .contains("VM name")
        );

        let no_way_in = CreateVmForm {
            ssh_enabled: false,
            password: String::new(),
            ..cloud_form()
        };
        let error = create_vm_request(&no_way_in, &[]).unwrap_err();
        assert!(
            error.contains("password") && error.contains("SSH"),
            "{error}"
        );

        let no_locale = CreateVmForm {
            locale: "   ".into(),
            ..cloud_form()
        };
        assert!(
            create_vm_request(&no_locale, &[])
                .unwrap_err()
                .contains("locale")
        );
    }

    /// Installation media means a person installs the system by hand, so none
    /// of the guest fields are submitted even though the form still holds them.
    #[test]
    fn installation_media_submits_no_provisioning() {
        let form = CreateVmForm {
            source_kind: SourceKind::LocalMedia,
            image_path: " C:\\images\\ubuntu.iso ".into(),
            password: "hunter2".into(),
            ..cloud_form()
        };

        let request = create_vm_request(&form, &[]).unwrap();

        assert_eq!(
            request.source,
            VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into()
            }
        );
        assert!(
            !format!("{:?}", request.source).contains("hunter2"),
            "a password typed before the mode was switched must not travel with the ISO"
        );
    }

    #[test]
    fn installation_media_still_needs_its_image() {
        let form = CreateVmForm {
            source_kind: SourceKind::LocalMedia,
            ..cloud_form()
        };

        assert!(
            create_vm_request(&form, &[])
                .unwrap_err()
                .contains("image path")
        );
    }

    /// The one check the domain cannot make: it is about the list on screen,
    /// not about the request.
    #[test]
    fn a_name_already_in_the_list_is_refused_before_the_backend_sees_it() {
        let existing = [VmSummary {
            name: "DEV".into(),
            os_type: "Linux".into(),
            state: VmState::Stopped,
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::Nat,
            ip_address: None,
            ssh_port: None,
        }];
        let form = CreateVmForm {
            name: "dev".into(),
            ..cloud_form()
        };

        assert!(
            create_vm_request(&form, &existing)
                .unwrap_err()
                .contains("already exists")
        );
    }

    #[test]
    fn a_build_shows_its_download_and_says_nothing_when_there_is_nothing_to_say() {
        assert_eq!(
            build_detail(BuildProgress {
                step: BuildStep::Downloading,
                download: Some(DownloadPhase::Downloading {
                    downloaded: 50 * 1024 * 1024,
                    total: Some(200 * 1024 * 1024),
                }),
            }),
            Some("Downloaded 50.0 MiB of 200.0 MiB (25%)".into())
        );
        assert_eq!(
            build_detail(BuildProgress {
                step: BuildStep::WritingDisk,
                download: None,
            }),
            None,
            "a step that publishes no counts has nothing to add to its own name"
        );
    }

    /// The list is what a person watches while an image downloads, so the
    /// percentage has to be there and not only behind a selected row.
    #[test]
    fn the_status_column_carries_the_downloads_percentage() {
        let downloading = |download| {
            vm_state_label(VmState::Building {
                progress: BuildProgress {
                    step: BuildStep::Downloading,
                    download: Some(download),
                },
            })
        };

        assert_eq!(
            downloading(DownloadPhase::Downloading {
                downloaded: 25,
                total: Some(100),
            }),
            "Building: downloading 25%"
        );
        assert_eq!(
            downloading(DownloadPhase::Verifying {
                hashed: 50,
                total: 100,
            }),
            "Building: downloading 50%"
        );
        assert_eq!(
            downloading(DownloadPhase::Downloading {
                downloaded: 25,
                total: None,
            }),
            "Building: downloading",
            "a server that sent no length gives nothing to divide by"
        );
        assert_eq!(
            vm_state_label(VmState::Building {
                progress: BuildProgress {
                    step: BuildStep::WritingDisk,
                    download: None,
                },
            }),
            "Building: writing the disk"
        );
        assert_eq!(vm_state_label(VmState::Stopped), "Stopped");
    }

    /// A bar that reads 100% while the work goes on is the one people wait on.
    #[test]
    fn a_download_reaches_a_hundred_percent_only_with_its_last_byte() {
        assert_eq!(percentage(999_999, 1_000_000), 99);
        assert_eq!(percentage(1_000_000, 1_000_000), 100);
        assert_eq!(percentage(0, 0), 0, "a length of zero must not divide");
    }

    #[test]
    fn edit_vm_request_accepts_supported_modes() {
        let request = edit_vm_request(&EditVmForm {
            name: "dev".into(),
            ram_mb: 8192,
            cpu_cores: 8,
            gpu_mode: GpuMode::TryAll,
            network_mode: NetworkMode::Nat,
            error: None,
        })
        .unwrap();

        assert_eq!(request.name, "dev");
        assert_eq!(request.gpu_mode, GpuMode::TryAll);
        assert_eq!(request.network_mode, NetworkMode::Nat);
    }

    #[test]
    fn edit_vm_request_rejects_odd_ram() {
        let error = edit_vm_request(&EditVmForm {
            name: "dev".into(),
            ram_mb: 513,
            cpu_cores: 4,
            gpu_mode: GpuMode::Default,
            network_mode: NetworkMode::Nat,
            error: None,
        })
        .unwrap_err();

        assert_eq!(
            error,
            "RAM must be an even number of MiB and at least 512 MiB."
        );
    }
}
