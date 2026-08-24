//! Desktop shell for the first VMLord milestone.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui;
use vmlord_app::{BackendStatus, VmAction, WorkspaceApp};
use vmlord_core::{
    AgentStatus, AppSettings, BuildProgress, BuildStep, CloudImage, DesktopProfile,
    DiagnosticLevel, DisplayState, DownloadPhase, GpuMode, GpuState, GuestDefaults,
    GuestReadinessTimeouts, HostGpuCapabilities, Language, LogLevel, NetworkMode, Password,
    Provisioning, SshAccess, SshPort, VmCreateRequest, VmDeleteRequest, VmDisplayStatus,
    VmGpuStatus, VmSource, VmState, VmSummary, VmUpdateRequest, ubuntu,
};

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// What a warning beside a form field is painted in.
///
/// A warning and not an error: everything it marks is a choice the backend
/// will accept and carry out with less than was asked for.
const WARNING_COLOR: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xA0, 0x30);
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
    /// The port the guest's SSH daemon is asked to listen on.
    ///
    /// A plain `u16` rather than an [`SshPort`]: a field being edited passes
    /// through values nobody means -- 0 among them, on the way from 22 to 2222
    /// -- and the domain type exists precisely so that those cannot be spelled.
    /// It becomes an `SshPort` when the form is submitted, or the submission
    /// fails.
    ssh_port: u16,
    locale: String,
    keyboard: String,
    timezone: String,
    /// The desktop cloud-init is asked to install.
    ///
    /// Kept on the form even while installation media is chosen, so that
    /// switching back to a cloud image does not forget what was picked; the
    /// request built from the form reads it only for a cloud image, where a
    /// seed exists to install it.
    desktop: DesktopProfile,
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
    /// What the VM was doing when the form was opened, which decides whether
    /// its GPU mode may be touched at all.
    state: VmState,
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
            state: vm.state.clone(),
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
            ssh_port: SshPort::DEFAULT.get(),
            locale: guest_defaults.locale.clone(),
            keyboard: guest_defaults.keyboard.clone(),
            timezone: guest_defaults.timezone.clone(),
            // A new VM comes with a desktop unless someone says otherwise:
            // the profile's own default, not a choice of this dialog's.
            desktop: DesktopProfile::default(),
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
            let gpu_status = self
                .selected_vm_name
                .as_deref()
                .and_then(|name| self.application.gpu_status(name));
            let display_status = self
                .selected_vm_name
                .as_deref()
                .and_then(|name| self.application.display_status(name));
            if let Some(action) = render_selected_vm(
                ui,
                self.application.vms(),
                &self.selected_vm_name,
                gpu_status,
                display_status,
            ) {
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
                | VmAction::Ssh
                | VmAction::Console
                | VmAction::UpdateDisplay => {
                    if let Some(name) = self.selected_vm_name.clone() {
                        let result = match action {
                            VmAction::Start => self.application.start_vm(&name),
                            VmAction::Stop => self.application.stop_vm(&name),
                            VmAction::ForceStop => self.application.force_stop_vm(&name),
                            VmAction::CancelCreate => self.application.cancel_create(&name),
                            VmAction::Connect => self.application.connect_display(&name),
                            VmAction::Ssh => self.application.open_ssh(&name),
                            VmAction::Console => self.application.open_console(&name),
                            VmAction::UpdateDisplay => {
                                self.application.update_display_payload(&name)
                            }
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
        let host_gpu = self.application.host_gpu_capabilities();
        let create_dialog_action = self.create_vm_form.as_mut().and_then(|form| {
            render_create_vm_dialog(
                context,
                form,
                self.application.vms(),
                ssh_key_path.as_deref(),
                host_gpu,
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

        let host_gpu = self.application.host_gpu_capabilities();
        let edit_dialog_action = self
            .edit_vm_form
            .as_mut()
            .and_then(|form| render_edit_vm_dialog(context, form, host_gpu));
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
    host_gpu: Option<&HostGpuCapabilities>,
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
                                        GpuMode::Mirror,
                                        "Mirror",
                                    );
                                    ui.selectable_value(&mut form.gpu_mode, GpuMode::None, "None");
                                });
                            ui.end_row();

                            for warning in gpu_capability_warnings(host_gpu, form.gpu_mode) {
                                ui.label("");
                                ui.colored_label(WARNING_COLOR, warning);
                                ui.end_row();
                            }

                            // Only a cloud image can be given a desktop: a
                            // hand-installed system gets no seed of VMLord's,
                            // so there would be nothing to install it with.
                            if form.source_kind == SourceKind::CloudImage {
                                ui.label("Desktop");
                                egui::ComboBox::from_id_salt("create-vm-desktop")
                                    .selected_text(desktop_profile_label(form.desktop))
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut form.desktop,
                                            DesktopProfile::Gnome,
                                            desktop_profile_label(DesktopProfile::Gnome),
                                        );
                                        ui.selectable_value(
                                            &mut form.desktop,
                                            DesktopProfile::Headless,
                                            desktop_profile_label(DesktopProfile::Headless),
                                        );
                                    });
                                ui.end_row();

                                // Advice from the domain, not a rule of the
                                // dialog: a desktop on a small VM is built and
                                // warned about, never refused.
                                for advisory in create_vm_advisories(form) {
                                    ui.label("");
                                    ui.colored_label(WARNING_COLOR, advisory);
                                    ui.end_row();
                                }
                            }

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
                    ui.horizontal(|ui| {
                        ui.label("Port");
                        // The range is the domain's own `1..=65535`, and the
                        // widget clamps to it, so the field cannot be dragged
                        // to a port nothing can be reached on. A typed 0 is
                        // still refused on submit -- clamping is the widget's
                        // courtesy, not the rule.
                        ui.add(
                            egui::DragValue::new(&mut form.ssh_port)
                                .range(1..=u16::MAX)
                                .speed(1.0),
                        );
                    });
                    if form.ssh_port != SshPort::DEFAULT.get() {
                        ui.small(
                            "The port is fixed when the VM is created; \
                             connections VMLord opens use it automatically.",
                        );
                    }
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
    host_gpu: Option<&HostGpuCapabilities>,
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
                "RAM, CPU and GPU are editable; the GPU mode only while the VM is stopped. \
                 Network is not wired to the native backend yet. Disk size and VM name stay \
                 fixed and currently require recreating the VM.",
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
                    let locked = gpu_mode_locked(&form.state);
                    ui.add_enabled_ui(locked.is_none(), |ui| {
                        let combo = egui::ComboBox::from_id_salt("edit-vm-gpu")
                            .selected_text(gpu_mode_label(form.gpu_mode))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut form.gpu_mode,
                                    GpuMode::Default,
                                    "Default",
                                );
                                ui.selectable_value(&mut form.gpu_mode, GpuMode::Mirror, "Mirror");
                                ui.selectable_value(&mut form.gpu_mode, GpuMode::None, "None");
                            });
                        // The reason before the click rather than after it: the
                        // backend refuses this change under a live VM, and a
                        // control that looks available and is not is worse than
                        // one that says why.
                        if let Some(reason) = locked {
                            combo.response.on_disabled_hover_text(reason);
                        }
                    });
                    ui.end_row();

                    for warning in gpu_capability_warnings(host_gpu, form.gpu_mode) {
                        ui.label("");
                        ui.colored_label(WARNING_COLOR, warning);
                        ui.end_row();
                    }

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
        source: create_vm_source(form)?,
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
///
/// Fallible because of the port: everything else the form holds is already the
/// shape the domain wants, while a port is a number that has to become an
/// [`SshPort`] -- and installation media has no port at all, so the failure
/// cannot even arise on that branch.
fn create_vm_source(form: &CreateVmForm) -> Result<VmSource, String> {
    Ok(match form.source_kind {
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
                        port: SshPort::new(form.ssh_port).map_err(|error| error.to_string())?,
                    }
                } else {
                    // A guest with no daemon is asked for no port: whatever
                    // the field happens to hold is not part of the request.
                    SshAccess::Disabled
                },
                locale: form.locale.trim().into(),
                keyboard: form.keyboard.trim().into(),
                timezone: form.timezone.trim().into(),
                desktop: form.desktop,
            },
        },
    })
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

/// What the GPU is doing right now, as opposed to what the VM asks of it.
///
/// Two rows rather than one: a VM configured for `Mirror` whose guest has not
/// come up yet is not a VM without a GPU, and a single line could only say one
/// of the two.
fn gpu_status_detail(status: Option<&VmGpuStatus>) -> String {
    // Only a VM the last refresh did not list has no status, and that VM is
    // not on screen to be asked about.
    let Some(status) = status else {
        return "Unknown".into();
    };

    let mut detail = format!("{}: {}", gpu_state_label(status.state), status.message);
    if let Some(adapter) = status
        .native
        .as_ref()
        .and_then(|native| native.adapter.as_ref())
    {
        detail.push_str(&format!(" Adapter: {adapter}."));
    }
    if let Some(node) = status
        .guest
        .as_ref()
        .and_then(|guest| guest.render_node.as_ref())
    {
        detail.push_str(&format!(" Render node: {node}."));
    }
    // The stable code, so that what is on screen can be found in the log. Only
    // where something is wrong: a working GPU needs no identifier to match a
    // line nobody is looking for.
    if matches!(status.state, GpuState::Failed | GpuState::Degraded) {
        detail.push_str(&format!(" ({})", status.code));
    }
    detail
}

/// What is worth saying about this host before a VM asks it for a GPU.
///
/// Warnings and never refusals: GPU is applied best effort, so a host that
/// cannot deliver produces a VM that starts and says why, not a form that
/// cannot be submitted.
///
/// `None` capabilities say nothing at all. A backend that could not be asked
/// has not reported an absence, and claiming the GPU is unavailable where we
/// merely could not ask would be a different answer.
fn gpu_capability_warnings(
    capabilities: Option<&HostGpuCapabilities>,
    mode: GpuMode,
) -> Vec<String> {
    if matches!(mode, GpuMode::None) {
        return Vec::new();
    }
    let Some(capabilities) = capabilities else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    if !capabilities.assignment.is_available() {
        warnings.push(
            "This host presents no GPU partition adapter, so the VM will start without a GPU."
                .to_owned(),
        );
    }
    if !capabilities.linux_payload.is_available() {
        warnings.push(
            "The Linux GPU userspace is not installed on this host, so the guest will see the \
             device but will not render on it."
                .to_owned(),
        );
    }
    warnings
}

/// Why the GPU mode cannot be changed right now, when it cannot.
///
/// The mode is applied while the compute system is prepared and started, so a
/// change under a live VM would leave a stored mode that does not describe the
/// GPU the guest actually has. RAM and CPU are different: they are read from
/// the configuration on the next start, and nothing claims otherwise.
fn gpu_mode_locked(state: &VmState) -> Option<&'static str> {
    match state {
        VmState::Stopped => None,
        _ => Some("Stop the VM to change its GPU mode."),
    }
}

fn gpu_state_label(state: GpuState) -> &'static str {
    match state {
        GpuState::Disabled => "Disabled",
        GpuState::WaitingForGuest => "Waiting for guest",
        GpuState::Assigned => "Assigned",
        GpuState::GuestReady => "Ready",
        GpuState::Degraded => "Degraded",
        GpuState::Failed => "Failed",
    }
}

/// What the domain has to say about the VM this form describes, if anything.
///
/// The advice itself belongs to `VmCreateRequest`; this only asks for it. A
/// form that is not yet a request has nothing to advise about -- the fields it
/// is missing are what the error under the buttons is for.
fn create_vm_advisories(form: &CreateVmForm) -> Vec<String> {
    create_vm_request(form, &[])
        .map(|request| request.advisories())
        .unwrap_or_default()
}

fn desktop_profile_label(profile: DesktopProfile) -> &'static str {
    match profile {
        DesktopProfile::Headless => "None (headless)",
        DesktopProfile::Gnome => "GNOME",
    }
}

/// What to show beside a VM's desktop, in one line.
fn display_status_detail(profile: DesktopProfile, status: Option<&VmDisplayStatus>) -> String {
    let Some(status) = status else {
        return desktop_profile_label(profile).to_owned();
    };
    let mut detail = format!("{}: {}", display_state_label(status.state), status.message);
    if status.can_retry {
        detail.push_str(" The desktop can be installed again.");
    }
    detail
}

fn display_state_label(state: DisplayState) -> &'static str {
    match state {
        DisplayState::Disabled => "Disabled",
        DisplayState::Provisioning => "Installing",
        DisplayState::WaitingForGuest => "Waiting for guest",
        DisplayState::Ready => "Ready",
        DisplayState::Degraded => "Degraded",
    }
}

fn gpu_mode_label(mode: GpuMode) -> &'static str {
    match mode {
        GpuMode::None => "None",
        GpuMode::Default => "Default",
        GpuMode::Mirror => "Mirror",
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

/// What the button says.
///
/// "Open SSH" and not "Open in Windows Terminal": which terminal host the
/// session lands in is decided when it is launched, and a button that named one
/// would be wrong on the machine where the other answers.
const SSH_ACTION_LABEL: &str = "Open SSH";

/// What the SSH action can offer for one VM right now.
///
/// Three answers rather than a boolean, because "no button" and "a button that
/// cannot be pressed yet" are different things to see: the first says this VM
/// was created without SSH and never will have it, the second says to wait, and
/// names what for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SshOffer {
    /// This VM has no SSH access at all, so there is no action to show.
    Absent,
    /// SSH is configured, but the guest cannot be reached yet.
    Waiting(&'static str),
    Ready,
}

impl SshOffer {
    /// The tooltip of a button that cannot be pressed, and nothing for one that
    /// can.
    const fn waiting_for(self) -> Option<&'static str> {
        match self {
            Self::Waiting(reason) => Some(reason),
            Self::Absent | Self::Ready => None,
        }
    }
}

/// Whether a session into `vm` can be opened, and what stops it.
///
/// The two things a connection needs are the two things checked here: SSH the
/// VM was created with, and a guest that is up and addressable. Both are read
/// from the summary the list was drawn from, which is a refresh old -- the
/// backend asks HCS again when the button is pressed, and refuses with the
/// reason it finds. This only decides what is worth offering.
fn ssh_offer(vm: &VmSummary) -> SshOffer {
    if !vm.ssh.is_enabled() {
        return SshOffer::Absent;
    }

    match vm.state {
        VmState::Building { .. } => {
            SshOffer::Waiting("Available once the VM has been built and started")
        }
        VmState::Stopped | VmState::Starting => {
            SshOffer::Waiting("Available when the VM is running")
        }
        VmState::Running { .. } if vm.ip_address.is_none() => {
            SshOffer::Waiting("Available when the guest has an address on the VMLord network")
        }
        VmState::Running { .. } => SshOffer::Ready,
    }
}

/// What the details panel says about a VM's SSH access.
///
/// The endpoint itself once there is one -- the same `user@address:port` a
/// person would type -- and the configuration without an address before that,
/// because the address is HNS's to hand out when the VM starts and a remembered
/// one would be a guess.
fn ssh_detail(vm: &VmSummary) -> String {
    let Some(ssh) = vm.ssh.config() else {
        return "Disabled".into();
    };

    match vm.ip_address {
        Some(address) => format!(
            "{}@{}:{} ({} login)",
            ssh.username, address, ssh.port, ssh.authentication
        ),
        None => format!(
            "{} on port {} ({} login); the address appears when the VM is running",
            ssh.username, ssh.port, ssh.authentication
        ),
    }
}

/// Whether Connect is offered, and what to say when it is not.
///
/// The display's own status rather than "the VM is running": a running VM
/// whose desktop is still installing has nothing to open a window on, and a
/// running VM whose guest has not offered its display would leave a viewer
/// retrying a service nothing binds. The sentence explaining either is the
/// application layer's, which is why this reads one rather than writing one.
fn connect_offer(status: Option<&VmDisplayStatus>) -> (bool, Option<&str>) {
    match status {
        Some(status) if status.is_connectable() => (true, None),
        Some(status) => (false, Some(status.message.as_str())),
        // A VM the application has derived nothing for yet -- one refresh old
        // at most. Offering a window on it would be offering a guess.
        None => (
            false,
            Some("The display of this VM has not been reported yet"),
        ),
    }
}

fn render_selected_vm(
    ui: &mut egui::Ui,
    vms: &[VmSummary],
    selected_vm_name: &Option<String>,
    gpu_status: Option<&VmGpuStatus>,
    display_status: Option<&VmDisplayStatus>,
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
        let (can_connect, waiting_for) = connect_offer(display_status);
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Connect, "Connect")],
            can_connect,
            waiting_for,
        ) {
            action = Some(clicked_action);
        }
        // A VM created without SSH has no session to offer, so it gets no
        // button; one that has SSH keeps the button in every state and says
        // what it is still waiting for.
        let ssh = ssh_offer(vm);
        if ssh != SshOffer::Absent
            && let Some(clicked_action) = render_action_group(
                ui,
                &[(VmAction::Ssh, SSH_ACTION_LABEL)],
                ssh == SshOffer::Ready,
                ssh.waiting_for(),
            )
        {
            action = Some(clicked_action);
        }
        // The way in when nothing else works: the console needs no network, no
        // address and no sshd, only a running compute system to own the pipe.
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Console, "Open COM port")],
            is_running,
            Some("Available only when the VM is running"),
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
            detail_row(ui, "GPU status", gpu_status_detail(gpu_status));
            detail_row(
                ui,
                "Desktop",
                desktop_profile_label(vm.desktop_profile).into(),
            );
            detail_row(
                ui,
                "Desktop status",
                display_status_detail(vm.desktop_profile, display_status),
            );
            detail_row(ui, "SSH", ssh_detail(vm));
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
        VmAction::UpdateDisplay => {
            // A screen with an arrow into it: the display, and something new
            // arriving in it.
            let screen = egui::Rect::from_center_size(center, egui::vec2(14.0, 10.0));
            painter.rect_stroke(screen, 2.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 7.0),
                    egui::pos2(center.x, center.y - 1.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 3.0, center.y - 4.0),
                    egui::pos2(center.x, center.y - 1.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 3.0, center.y - 4.0),
                    egui::pos2(center.x, center.y - 1.0),
                ],
                stroke,
            );
        }
        VmAction::Console => {
            // The D-sub connector on the back of a machine: what COM1 looks
            // like to the person who needs it.
            let shell = egui::Rect::from_center_size(center, egui::vec2(15.0, 9.0));
            painter.rect_stroke(shell, 4.0, stroke, egui::StrokeKind::Inside);
            for offset in [-4.0_f32, 0.0, 4.0] {
                painter.circle_filled(egui::pos2(center.x + offset, center.y - 1.6), 1.0, color);
            }
            for offset in [-2.0_f32, 2.0] {
                painter.circle_filled(egui::pos2(center.x + offset, center.y + 1.8), 1.0, color);
            }
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
        VmAction::Connect | VmAction::Ssh | VmAction::Console | VmAction::UpdateDisplay => {
            egui::Color32::from_rgb(85, 193, 233)
        }
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
    use std::net::{IpAddr, Ipv4Addr};

    use vmlord_core::{
        DisplayStage, DisplayState, DisplayStatusCode, GpuAvailability, GpuFailure, GpuStatusCode,
        SshAuthentication, SshAvailability, SshConfig, VmGpuFacts,
    };

    use super::*;

    /// One derived display status, as the application layer hands it over.
    fn display_status(state: DisplayState, message: &str) -> VmDisplayStatus {
        VmDisplayStatus {
            state,
            stage: DisplayStage::Guest,
            code: DisplayStatusCode::GuestReady,
            running_version: None,
            available_version: None,
            message: message.to_owned(),
            guest: None,
            can_retry: false,
            observed_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn connect_is_offered_only_when_the_display_can_be_connected_to() {
        let ready = display_status(DisplayState::Ready, "The guest offers its desktop.");

        assert_eq!(connect_offer(Some(&ready)), (true, None));
    }

    #[test]
    fn a_display_that_is_not_ready_says_what_it_is_waiting_for() {
        let waiting = display_status(
            DisplayState::WaitingForGuest,
            "The desktop is installed; waiting for the guest to offer it.",
        );

        assert_eq!(
            connect_offer(Some(&waiting)),
            (
                false,
                Some("The desktop is installed; waiting for the guest to offer it.")
            ),
            "the reason is the application layer's sentence, not one invented here"
        );
    }

    #[test]
    fn a_vm_with_no_derived_status_is_not_offered_a_window() {
        let (offered, reason) = connect_offer(None);

        assert!(!offered);
        assert!(reason.is_some());
    }

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
        assert_eq!(
            provisioning.ssh,
            SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT
            }
        );
        assert_eq!(provisioning.locale, "ru_RU.UTF-8");
        assert_eq!(provisioning.keyboard, "ru");
        assert_eq!(provisioning.timezone, "Europe/Moscow");
        assert_eq!(provisioning.desktop, DesktopProfile::Gnome);
    }

    /// The form starts on a desktop, and a headless VM is the choice someone
    /// makes -- with the same form still able to describe one.
    #[test]
    fn the_chosen_desktop_reaches_the_request() {
        assert_eq!(cloud_form().desktop, DesktopProfile::Gnome);

        let form = CreateVmForm {
            desktop: DesktopProfile::Headless,
            ..cloud_form()
        };
        let request = create_vm_request(&form, &[]).unwrap();

        assert_eq!(provisioning_of(&request).desktop, DesktopProfile::Headless);
        assert_eq!(request.desktop_profile(), DesktopProfile::Headless);
    }

    /// Installation media has no seed to install a desktop from, so whatever
    /// the form remembers of one stays out of the request.
    #[test]
    fn installation_media_carries_no_desktop_whatever_the_form_holds() {
        let form = CreateVmForm {
            source_kind: SourceKind::LocalMedia,
            image_path: "C:\\images\\ubuntu.iso".into(),
            desktop: DesktopProfile::Gnome,
            ..cloud_form()
        };

        let request = create_vm_request(&form, &[]).unwrap();

        assert!(matches!(request.source, VmSource::LocalMedia { .. }));
        assert_eq!(request.desktop_profile(), DesktopProfile::Headless);
        assert!(request.advisories().is_empty());
    }

    /// A small desktop VM is warned about and still built: the advice comes
    /// from the domain, and the dialog only paints it.
    #[test]
    fn a_small_desktop_vm_is_advised_against_rather_than_refused() {
        let form = CreateVmForm {
            ram_mb: 1024,
            cpu_cores: 1,
            password: "hunter2".into(),
            ..cloud_form()
        };

        assert!(create_vm_request(&form, &[]).is_ok());
        assert_eq!(create_vm_advisories(&form).len(), 1);
        assert!(create_vm_advisories(&cloud_form()).len() <= 1);
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
        // With no key deployed, the password is the only way in, and the domain
        // insists on one.
        let key_less = CreateVmForm {
            deploy_key: false,
            password: "hunter2".into(),
            ..cloud_form()
        };
        assert_eq!(
            provisioning_of(&create_vm_request(&key_less, &[]).unwrap()).ssh,
            SshAccess::Enabled {
                deploy_key: false,
                port: SshPort::DEFAULT
            }
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

    /// A new form offers the port a guest normally listens on, and a typed one
    /// reaches the request unchanged.
    #[test]
    fn the_chosen_ssh_port_reaches_the_request() {
        assert_eq!(cloud_form().ssh_port, 22);

        for port in [1, 22, 2222, 65535] {
            let form = CreateVmForm {
                ssh_port: port,
                ..cloud_form()
            };

            assert_eq!(
                provisioning_of(&create_vm_request(&form, &[]).unwrap()).ssh,
                SshAccess::Enabled {
                    deploy_key: true,
                    port: SshPort::new(port).unwrap()
                }
            );
        }
    }

    /// The widget clamps, but the rule is the domain's: a form that arrived at
    /// zero some other way is still refused, and with the domain's own words.
    #[test]
    fn a_port_nothing_can_be_reached_on_is_refused() {
        let form = CreateVmForm {
            ssh_port: 0,
            ..cloud_form()
        };

        let error = create_vm_request(&form, &[]).unwrap_err();

        assert!(error.contains("SSH port"), "{error}");
    }

    /// Turning SSH off leaves the port field on screen holding whatever it held;
    /// what it holds is then not part of the request, valid or not.
    #[test]
    fn a_guest_without_ssh_carries_no_port_whatever_the_field_holds() {
        let form = CreateVmForm {
            ssh_enabled: false,
            ssh_port: 0,
            password: "hunter2".into(),
            ..cloud_form()
        };

        assert_eq!(
            provisioning_of(&create_vm_request(&form, &[]).unwrap()).ssh,
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
            // A port a cloud image would be refused for: installation media
            // has no SSH controls at all, so it is not even looked at.
            ssh_port: 0,
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

    /// A VM as the list holds one: stopped, without an address, and with no way
    /// in over SSH. Every SSH test says which of those three it changes.
    fn vm_summary() -> VmSummary {
        VmSummary {
            name: "dev".into(),
            os_type: "Linux".into(),
            state: VmState::Stopped,
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            gpu: VmGpuFacts::default(),
            desktop_profile: DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display: vmlord_core::VmDisplayFacts::default(),
            network_mode: NetworkMode::Nat,
            ip_address: None,
            ssh: SshAvailability::Disabled,
        }
    }

    fn ssh_config() -> SshConfig {
        SshConfig {
            username: "dev".into(),
            port: SshPort::DEFAULT,
            authentication: SshAuthentication::VmlordKey,
        }
    }

    fn address() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(172, 30, 0, 5))
    }

    /// A VM created without SSH offers no session to open: the button is not
    /// there to be hovered, rather than there and grey.
    #[test]
    fn a_vm_without_ssh_offers_no_session_at_all() {
        assert_eq!(ssh_offer(&vm_summary()), SshOffer::Absent);
        assert_eq!(
            ssh_offer(&VmSummary {
                state: VmState::Running {
                    agent_status: AgentStatus::Online
                },
                ip_address: Some(address()),
                ..vm_summary()
            }),
            SshOffer::Absent,
            "a running VM with an address still has nothing to log into"
        );
    }

    /// The capability belongs to the VM and the button to its state: a stopped
    /// VM with SSH configured shows the action, and says what it is waiting for.
    #[test]
    fn a_configured_vm_shows_the_action_before_it_can_be_used() {
        let configured = VmSummary {
            ssh: SshAvailability::Enabled(ssh_config()),
            ..vm_summary()
        };

        let SshOffer::Waiting(stopped) = ssh_offer(&configured) else {
            panic!("a stopped VM shows the action and waits");
        };
        assert!(stopped.contains("running"), "{stopped}");

        let SshOffer::Waiting(building) = ssh_offer(&VmSummary {
            state: VmState::Building {
                progress: BuildProgress {
                    step: BuildStep::WritingDisk,
                    download: None,
                },
            },
            ..configured.clone()
        }) else {
            panic!("a VM still being built has no guest yet");
        };
        assert!(building.contains("built"), "{building}");

        let SshOffer::Waiting(starting) = ssh_offer(&VmSummary {
            state: VmState::Starting,
            ..configured.clone()
        }) else {
            panic!("a VM on its way up has no guest yet");
        };
        assert!(starting.contains("running"), "{starting}");
    }

    /// Running is not enough: without an address on the VMLord network there is
    /// nothing for a client to dial, and the tooltip says so rather than letting
    /// the click fail in the backend.
    #[test]
    fn a_running_vm_without_an_address_is_not_ready() {
        let running = VmSummary {
            state: VmState::Running {
                agent_status: AgentStatus::Unknown,
            },
            ssh: SshAvailability::Enabled(ssh_config()),
            ..vm_summary()
        };

        let SshOffer::Waiting(reason) = ssh_offer(&running) else {
            panic!("a VM with no address cannot be connected to");
        };
        assert!(reason.contains("address"), "{reason}");

        assert_eq!(
            ssh_offer(&VmSummary {
                ip_address: Some(address()),
                ..running
            }),
            SshOffer::Ready
        );
    }

    /// What the details panel says about SSH, which is the endpoint itself once
    /// there is one to state.
    #[test]
    fn the_details_state_the_endpoint_a_session_would_use() {
        assert_eq!(ssh_detail(&vm_summary()), "Disabled");

        let configured = VmSummary {
            ssh: SshAvailability::Enabled(SshConfig {
                port: SshPort::new(2222).unwrap(),
                ..ssh_config()
            }),
            ..vm_summary()
        };

        let stopped = ssh_detail(&configured);
        assert!(
            stopped.contains("dev") && stopped.contains("2222") && stopped.contains("key login"),
            "{stopped}"
        );
        assert!(
            stopped.contains("address"),
            "a VM with no address says why it shows none: {stopped}"
        );

        assert_eq!(
            ssh_detail(&VmSummary {
                state: VmState::Running {
                    agent_status: AgentStatus::Online
                },
                ip_address: Some(address()),
                ..configured
            }),
            "dev@172.30.0.5:2222 (key login)"
        );
    }

    /// The button says what it does and promises no particular terminal: which
    /// window the session lands in is the platform layer's business.
    #[test]
    fn the_action_is_named_after_what_it_opens() {
        assert_eq!(SSH_ACTION_LABEL, "Open SSH");
        assert!(!format!("{SSH_ACTION_LABEL:?}").contains("Terminal"));
    }

    /// The one check the domain cannot make: it is about the list on screen,
    /// not about the request.
    #[test]
    fn a_name_already_in_the_list_is_refused_before_the_backend_sees_it() {
        let existing = [VmSummary {
            name: "DEV".into(),
            ..vm_summary()
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
            gpu_mode: GpuMode::Mirror,
            network_mode: NetworkMode::Nat,
            error: None,
            state: VmState::Stopped,
        })
        .unwrap();

        assert_eq!(request.name, "dev");
        assert_eq!(request.gpu_mode, GpuMode::Mirror);
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
            state: VmState::Stopped,
        })
        .unwrap_err();

        assert_eq!(
            error,
            "RAM must be an even number of MiB and at least 512 MiB."
        );
    }
    fn host(assignment: GpuAvailability, linux_payload: GpuAvailability) -> HostGpuCapabilities {
        HostGpuCapabilities {
            assignment,
            linux_payload,
            adapters: Vec::new(),
        }
    }

    fn unavailable(code: GpuStatusCode, message: &str) -> GpuAvailability {
        GpuAvailability::Unavailable(GpuFailure::new(code, message))
    }

    #[test]
    fn a_vm_without_a_gpu_is_warned_about_nothing() {
        let capabilities = host(
            unavailable(GpuStatusCode::HostNoAdapter, "no adapter"),
            GpuAvailability::Available,
        );

        assert!(
            gpu_capability_warnings(Some(&capabilities), GpuMode::None).is_empty(),
            "a VM with no GPU has no reason to read about the DriverStore"
        );
    }

    #[test]
    fn a_host_without_an_adapter_warns_and_does_not_refuse() {
        let capabilities = host(
            unavailable(GpuStatusCode::HostNoAdapter, "no adapter"),
            GpuAvailability::Available,
        );

        let warnings = gpu_capability_warnings(Some(&capabilities), GpuMode::Default);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("without a GPU"), "{}", warnings[0]);
    }

    #[test]
    fn a_host_without_the_linux_payload_warns_about_the_guest_and_not_the_host() {
        let capabilities = host(
            GpuAvailability::Available,
            unavailable(GpuStatusCode::HostLinuxPayloadMissing, "no payload"),
        );

        let warnings = gpu_capability_warnings(Some(&capabilities), GpuMode::Mirror);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("render"), "{}", warnings[0]);
    }

    #[test]
    fn a_host_that_is_short_of_both_says_both() {
        let capabilities = host(
            unavailable(GpuStatusCode::HostNoAdapter, "no adapter"),
            unavailable(GpuStatusCode::HostLinuxPayloadMissing, "no payload"),
        );

        assert_eq!(
            gpu_capability_warnings(Some(&capabilities), GpuMode::Default).len(),
            2
        );
    }

    #[test]
    fn a_backend_that_could_not_be_asked_says_nothing_at_all() {
        assert!(
            gpu_capability_warnings(None, GpuMode::Default).is_empty(),
            "claiming a GPU is unavailable where we could not ask is worse than silence"
        );
    }

    #[test]
    fn the_gpu_mode_is_locked_while_the_vm_is_not_stopped() {
        assert!(
            gpu_mode_locked(&VmState::Running {
                agent_status: AgentStatus::Online
            })
            .is_some()
        );
        assert!(gpu_mode_locked(&VmState::Starting).is_some());
        assert!(gpu_mode_locked(&VmState::Stopped).is_none());
    }

    fn status_of(state: GpuState, code: GpuStatusCode, message: &str) -> VmGpuStatus {
        VmGpuStatus {
            state,
            stage: vmlord_core::GpuStage::Guest,
            code,
            message: message.into(),
            native: Some(vmlord_core::NativeGpuDetail {
                adapter: Some("NVIDIA RTX 4070".into()),
                adapters: 1,
            }),
            guest: Some(vmlord_core::GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            }),
            observed_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn an_active_gpu_names_the_adapter_and_the_render_node() {
        let detail = gpu_status_detail(Some(&status_of(
            GpuState::GuestReady,
            GpuStatusCode::GuestReady,
            "The guest renders on the GPU.",
        )));

        assert!(detail.contains("NVIDIA RTX 4070"), "{detail}");
        assert!(detail.contains("/dev/dri/renderD128"), "{detail}");
        assert!(
            !detail.contains("gpu-guest-ready"),
            "a working GPU needs no identifier to match a line nobody looks for: {detail}"
        );
    }

    #[test]
    fn a_failed_gpu_shows_the_code_the_log_uses() {
        let detail = gpu_status_detail(Some(&status_of(
            GpuState::Failed,
            GpuStatusCode::GuestFailed,
            "the guest kernel has no dxgkrnl module",
        )));

        assert!(
            detail.contains("gpu-guest-failed"),
            "the screen and the log have to be matchable: {detail}"
        );
    }

    #[test]
    fn a_degraded_gpu_shows_its_code_too() {
        let detail = gpu_status_detail(Some(&status_of(
            GpuState::Degraded,
            GpuStatusCode::AssignmentPartial,
            "one of two adapters",
        )));

        assert!(detail.contains("gpu-assignment-partial"), "{detail}");
    }
}
