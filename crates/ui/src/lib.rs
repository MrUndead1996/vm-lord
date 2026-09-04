//! Desktop shell for the first VMLord milestone.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui;
use rust_i18n::t;
use vmlord_app::{AvailableUpdate, BackendStatus, UpdateState, VmAction, WorkspaceApp};
use vmlord_core::{
    Advisory, AgentStatus, AppSettings, BuildProgress, BuildStep, CloudImage, DesktopProfile,
    DiagnosticLevel, DisplaySettings, DisplayState, DistroProfile, DownloadPhase,
    FileClipboardSettings, GpuMode, GpuState, GuestDefaults, GuestDesktop, GuestReadinessTimeouts,
    HostGpuCapabilities, Language, LogLevel, NetworkMode, Password, Provisioning, SshAccess,
    SshAuthentication, SshConfig, SshPort, VmCreateRequest, VmDeleteRequest, VmDisplayStatus,
    VmGpuStatus, VmSource, VmState, VmSummary, VmUpdateRequest,
};

// The catalogues in `locales/`, embedded at compile time. English is the
// fallback, so a key missing from another catalogue shows English rather than
// its own name -- and the parity test keeps that from happening unnoticed.
rust_i18n::i18n!("locales", fallback = "en-US");

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// What a warning beside a form field is painted in.
///
/// A warning and not an error: everything it marks is a choice the backend
/// will accept and carry out with less than was asked for.
const WARNING_COLOR: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xA0, 0x30);
const VM_TABLE_COLUMN_COUNT: f32 = 9.0;

const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

fn application_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/vmlord.png"))
        .expect("the embedded VMLord icon is a valid PNG")
}

/// The height a text field in a form claims.
///
/// Stated rather than left at zero: a widget added with no height of its own
/// makes its grid row shorter than what is drawn in it, and the row below then
/// starts inside it -- which is what made the combo box under "VM Name" and the
/// password field under "User name" overlap the fields above them.
const FIELD_HEIGHT: f32 = 24.0;

pub fn run(application: WorkspaceApp) -> eframe::Result<()> {
    // Settings that failed to load leave the locale at the fallback, which is
    // where a fresh installation starts anyway.
    if let Some(settings) = application.settings() {
        rust_i18n::set_locale(settings.language.code());
    }
    // Where the window was left. `eframe` writes the place, the size and
    // whether it was maximised into this file and applies them before the
    // window is shown; the inner size below is what a first run opens at,
    // and what a session with no profile to write to opens at every time.
    let window_state = application.window_state_path();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_icon(application_icon()),
        persist_window: window_state.is_some(),
        persistence_path: window_state,
        ..Default::default()
    };
    eframe::run_native(
        "VMLord",
        options,
        Box::new(move |_| {
            // A first run opens the settings window by itself, filled in from
            // the settings that were just created: the paths and the default
            // distribution are the choices a fresh installation is asking
            // about, and finding them is otherwise the user's problem.
            let settings_form = application
                .first_run()
                .then(|| application.settings().map(SettingsForm::first_run))
                .flatten();
            Ok(Box::new(VmlordUi {
                application,
                last_refresh: Instant::now(),
                selected_vm_name: None,
                create_vm_form: None,
                edit_vm_form: None,
                delete_vm_form: None,
                settings_form,
                install_confirmation: None,
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
    /// The version whose installer is waiting on a confirmation, if any.
    ///
    /// The version and not the installer path: the application layer already
    /// holds the verified file, and the dialog only needs what to name.
    install_confirmation: Option<String>,
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
    profile: DistroProfile,
    /// The catalogue identifier `profile` was loaded under.
    ///
    /// A name the form generated is the previous identifier, so a switch of
    /// distribution can tell a field nobody touched from one someone typed
    /// over -- see `select_distro`.
    distro_id: String,
    name: String,
    source_kind: SourceKind,
    /// Installation media: the path to the ISO the guest is installed from.
    image_path: String,
    /// Cloud image: one of the releases named by `profile`.
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
    log_directory: String,
    log_level: LogLevel,
    /// Carried through the dialog unchanged: the settings form rebuilds the
    /// whole `AppSettings`, so a field it does not know about would be lost on
    /// every save. The widget for it arrives with the image download UI.
    image_cache_path: PathBuf,
    /// Identifier of the distribution profile used for new cloud-image VMs.
    default_distro: String,
    /// Carried through unchanged for the same reason as `image_cache_path`,
    /// and with no widget of its own on purpose: the readiness timeouts are
    /// edited in `settings.toml` on the rare occasion anyone needs to.
    guest_readiness: GuestReadinessTimeouts,
    /// TOML-only in task 139; the settings dialog must preserve it unchanged.
    clipboard_files: FileClipboardSettings,
    display: DisplaySettings,
    last_automatic_update_check: Option<String>,
    /// Whether this window was opened by the application starting for the
    /// first time rather than by the Settings button.
    ///
    /// It changes nothing that is saved; it only adds the line explaining why
    /// the window is open at all, which is the difference between a settings
    /// dialog and a settings dialog nobody asked for.
    first_run: bool,
    error: Option<String>,
}

impl SettingsForm {
    fn from_settings(settings: &AppSettings) -> Self {
        Self {
            vm_storage_path: settings.vm_storage_path.display().to_string(),
            language: settings.language,
            log_directory: settings.log_directory.display().to_string(),
            log_level: settings.log_level,
            image_cache_path: settings.image_cache_path.clone(),
            default_distro: settings.default_distro.clone(),
            guest_readiness: settings.guest_readiness,
            clipboard_files: settings.clipboard_files,
            display: settings.display,
            last_automatic_update_check: settings.last_automatic_update_check.clone(),
            first_run: false,
            error: None,
        }
    }

    /// The same form, marked as the one a first run opens by itself.
    fn first_run(settings: &AppSettings) -> Self {
        Self {
            first_run: true,
            ..Self::from_settings(settings)
        }
    }

    fn settings(&self) -> Result<AppSettings, String> {
        let vm_storage_path = self.vm_storage_path.trim();
        if vm_storage_path.is_empty() {
            return Err(t!("settings.vm_storage_required").to_string());
        }
        let log_directory = self.log_directory.trim();
        if log_directory.is_empty() {
            return Err(t!("settings.log_directory_required").to_string());
        }

        self.display
            .validate()
            .map_err(|_| t!("settings.fps_gap_threshold_invalid").to_string())?;

        Ok(AppSettings {
            vm_storage_path: PathBuf::from(vm_storage_path),
            language: self.language,
            log_directory: PathBuf::from(log_directory),
            log_level: self.log_level,
            image_cache_path: self.image_cache_path.clone(),
            default_distro: self.default_distro.clone(),
            guest_readiness: self.guest_readiness,
            clipboard_files: self.clipboard_files,
            display: self.display,
            last_automatic_update_check: self.last_automatic_update_check.clone(),
        })
    }
}

struct EditVmForm {
    name: String,
    ram_mb: u32,
    /// The size the system disk is being edited to, in GiB.
    disk_gb: u32,
    /// The size the disk had when the form opened.
    ///
    /// Kept beside the edited value because a disk only grows: it is the floor
    /// the field may not be dragged below, and the edited value has already
    /// moved away from it by the time anything asks.
    stored_disk_gb: u32,
    cpu_cores: u32,
    gpu_mode: GpuMode,
    network_mode: NetworkMode,
    /// The SSH access the VM was created with, or none at all.
    ///
    /// Kept whole rather than as a port alone: whether the port may be edited
    /// depends on how the VM logs in, and a form holding only a number would
    /// have to ask the list again to find out.
    ssh: Option<SshConfig>,
    /// The port being edited, as a plain `u16` for the same reason the create
    /// form holds one: a field on its way from 22 to 2222 passes through
    /// values nobody means, 0 among them.
    ssh_port: u16,
    /// What the VM was doing when the form was opened, which decides whether
    /// its GPU mode may be touched at all.
    state: VmState,
    error: Option<String>,
}

impl EditVmForm {
    fn from_vm(vm: &VmSummary) -> Self {
        let ssh = vm.ssh.config().cloned();
        Self {
            name: vm.name.clone(),
            ram_mb: vm.ram_mb,
            disk_gb: vm.disk_gb,
            stored_disk_gb: vm.disk_gb,
            cpu_cores: vm.cpu_cores,
            gpu_mode: vm.gpu_mode,
            network_mode: vm.network_mode,
            ssh_port: ssh.as_ref().map_or(SshPort::DEFAULT, |ssh| ssh.port).get(),
            ssh,
            state: vm.state,
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
    /// a VM named after the distribution, the newest supported release, the
    /// distribution's own account name, and the host's locale, keyboard layout
    /// and timezone.
    fn new(distro_id: &str, profile: &DistroProfile, guest_defaults: &GuestDefaults) -> Self {
        Self {
            profile: profile.clone(),
            distro_id: distro_id.to_owned(),
            name: distro_id.to_owned(),
            source_kind: SourceKind::CloudImage,
            image_path: String::new(),
            release: profile.releases.first().cloned().unwrap_or_default(),
            username: profile.default_user.clone(),
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

    /// Switches distribution to the profile the catalogue holds under
    /// `distro_id`.
    ///
    /// A field nobody touched is recognised by still holding what the form
    /// generated -- the previous identifier as the VM name, the previous
    /// profile's own account -- and is replaced; something typed over it is
    /// kept. The release is the one field that always restarts: releases of
    /// two distributions never overlap, so the new profile's first is the
    /// only release that can be right.
    fn select_distro(&mut self, distro_id: &str, profile: &DistroProfile) {
        if self.username == self.profile.default_user {
            self.username = profile.default_user.clone();
        }
        if self.name == self.distro_id {
            self.name = distro_id.to_owned();
        }
        self.release = profile.releases.first().cloned().unwrap_or_default();
        self.profile = profile.clone();
        self.distro_id = distro_id.to_owned();
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
    BrowseLogDirectory,
    /// Look for a newer release now, ignoring the daily throttle.
    CheckUpdates,
    DownloadUpdate,
    CancelUpdate,
    /// Open the confirmation that installing closes VMLord. The install itself
    /// is never started straight from this button.
    RequestInstall,
    Cancel,
    /// Boxed: the settings are wider than every other action put together,
    /// and this enum is returned by value from each dialogue frame.
    Submit(Box<AppSettings>),
}

/// The answer to "installing closes VMLord -- continue?".
enum InstallConfirmationAction {
    Cancel,
    Install,
}

impl eframe::App for VmlordUi {
    /// What is remembered is the window, not what was on screen inside it.
    ///
    /// egui's own memory holds scroll offsets and the state of every widget
    /// that has any, and a form restored halfway through is not what leaving
    /// the application and coming back means here.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The dialogs are windows rather than widgets of this `Ui`, and they
        // are addressed through the context; `eframe` hands out the `Ui` and
        // leaves the clone to whoever needs the one behind it.
        let context = ui.ctx().clone();
        context.request_repaint_after(AUTO_REFRESH_INTERVAL);
        // The update worker reports through a channel the application layer
        // drains here, so a check or a download that finished between frames
        // is on screen in this one.
        self.application.poll_update();
        if matches!(self.application.status(), BackendStatus::Ready)
            && self.last_refresh.elapsed() >= AUTO_REFRESH_INTERVAL
        {
            self.application.refresh();
            self.last_refresh = Instant::now();
        }

        let action = egui::CentralPanel::default().show(ui, |ui| {
            let mut selected_action = None;

            ui.heading("VMLord");
            ui.label(t!("app.subtitle").to_string());
            ui.separator();

            ui.horizontal(|ui| {
                render_backend_status(ui, self.application.status());
                let can_refresh = matches!(self.application.status(), BackendStatus::Ready);
                let refresh = render_refresh_icon(ui, can_refresh);
                if can_refresh {
                    refresh.clone().on_hover_text(t!("app.refresh").to_string());
                } else {
                    refresh
                        .clone()
                        .on_disabled_hover_text(t!("app.refresh_hint").to_string());
                }
                if refresh.clicked() {
                    self.application.refresh();
                    self.last_refresh = Instant::now();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let create = ui.add_enabled(
                        can_refresh,
                        egui::Button::new(
                            egui::RichText::new(t!("app.create_vm").to_string())
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(47, 158, 97)),
                    );
                    if can_refresh {
                        create
                            .clone()
                            .on_hover_text(t!("app.create_vm_hint").to_string());
                    } else {
                        create
                            .clone()
                            .on_disabled_hover_text(t!("app.refresh_hint").to_string());
                    }
                    if create.clicked() {
                        selected_action = Some(VmAction::Create);
                    }

                    let settings = ui.button(t!("app.settings").to_string());
                    settings
                        .clone()
                        .on_hover_text(t!("app.settings_hint").to_string());
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
                    self.create_vm_form = self.application.distro_profile().map(|(id, profile)| {
                        CreateVmForm::new(id, profile, self.application.guest_defaults())
                    });
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
        let distro_profiles = self.application.distro_profiles().collect::<Vec<_>>();
        let create_dialog_action = self.create_vm_form.as_mut().and_then(|form| {
            render_create_vm_dialog(
                &context,
                form,
                self.application.vms(),
                &distro_profiles,
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

        let distro_options = self.application.distro_options().collect::<Vec<_>>();
        let update_state = self.application.update_state().clone();
        let settings_dialog_action = self.settings_form.as_mut().and_then(|form| {
            render_settings_dialog(&context, form, &distro_options, &update_state)
        });
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
            Some(SettingsDialogAction::BrowseLogDirectory) => {
                match self.application.pick_log_directory() {
                    Ok(Some(path)) => {
                        if let Some(form) = &mut self.settings_form {
                            form.log_directory = path;
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
            Some(SettingsDialogAction::CheckUpdates) => {
                if let Err(error) = self.application.check_for_updates()
                    && let Some(form) = &mut self.settings_form
                {
                    form.error = Some(error.to_string());
                }
            }
            Some(SettingsDialogAction::DownloadUpdate) => {
                if let Err(error) = self.application.download_update()
                    && let Some(form) = &mut self.settings_form
                {
                    form.error = Some(error.to_string());
                }
            }
            Some(SettingsDialogAction::CancelUpdate) => {
                if let Err(error) = self.application.cancel_update()
                    && let Some(form) = &mut self.settings_form
                {
                    form.error = Some(error.to_string());
                }
            }
            Some(SettingsDialogAction::RequestInstall) => {
                if let UpdateState::Ready { update, .. } = self.application.update_state() {
                    self.install_confirmation = Some(update.validated.version.to_string());
                }
            }
            Some(SettingsDialogAction::Cancel) => self.settings_form = None,
            Some(SettingsDialogAction::Submit(settings)) => {
                let language = settings.language;
                if let Err(error) = self.application.update_settings(*settings) {
                    if let Some(form) = &mut self.settings_form {
                        form.error = Some(error.to_string());
                    }
                } else {
                    // egui rebuilds every frame from the catalogue, so this is
                    // the whole of switching language: no restart, no reload.
                    rust_i18n::set_locale(language.code());
                    self.settings_form = None;
                }
            }
            None => {}
        }

        let confirmation_action = self
            .install_confirmation
            .as_deref()
            .and_then(|version| render_install_confirmation(&context, version));
        match confirmation_action {
            Some(InstallConfirmationAction::Cancel) => self.install_confirmation = None,
            Some(InstallConfirmationAction::Install) => {
                self.install_confirmation = None;
                match self.application.install_update() {
                    // Windows has the installer; staying open would only put
                    // this executable in the way of replacing it.
                    Ok(true) => {
                        context.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        if let Some(form) = &mut self.settings_form {
                            form.error = Some(error.to_string());
                        }
                    }
                }
            }
            None => {}
        }

        let host_gpu = self.application.host_gpu_capabilities();
        let edit_dialog_action = self
            .edit_vm_form
            .as_mut()
            .and_then(|form| render_edit_vm_dialog(&context, form, host_gpu));
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
            .and_then(|form| render_delete_vm_dialog(&context, form));
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
    distros: &[(&str, &DistroProfile)],
    ssh_key_path: Option<&Path>,
    host_gpu: Option<&HostGpuCapabilities>,
) -> Option<CreateVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(t!("create_vm.title").to_string())
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
                    ui.label(t!("create_vm.description").to_string());
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.strong(t!("create_vm.system").to_string());
                        ui.radio_value(
                            &mut form.source_kind,
                            SourceKind::CloudImage,
                            t!("create_vm.cloud_image").to_string(),
                        );
                        ui.radio_value(
                            &mut form.source_kind,
                            SourceKind::LocalMedia,
                            t!("create_vm.own_iso").to_string(),
                        );
                    });
                    match form.source_kind {
                        SourceKind::CloudImage => {
                            ui.small(t!("create_vm.cloud_image_note").to_string())
                        }
                        SourceKind::LocalMedia => {
                            ui.small(t!("create_vm.own_iso_note").to_string())
                        }
                    };

                    ui.add_space(8.0);
                    egui::Grid::new("create-vm-form")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label(t!("create_vm.vm_name").to_string());
                            ui.add_sized(
                                [260.0, FIELD_HEIGHT],
                                egui::TextEdit::singleline(&mut form.name),
                            );
                            ui.end_row();

                            match form.source_kind {
                                SourceKind::CloudImage => {
                                    ui.label(t!("create_vm.distribution").to_string());
                                    let mut selected = form.distro_id.clone();
                                    egui::ComboBox::from_id_salt("create-vm-distribution")
                                        .selected_text(&form.profile.name)
                                        .show_ui(ui, |ui| {
                                            for (id, profile) in distros {
                                                ui.selectable_value(
                                                    &mut selected,
                                                    (*id).to_owned(),
                                                    &profile.name,
                                                );
                                            }
                                        });
                                    if selected != form.distro_id
                                        && let Some((id, profile)) = distros
                                            .iter()
                                            .find(|(distro_id, _)| **distro_id == selected)
                                            .copied()
                                    {
                                        form.select_distro(id, profile);
                                    }
                                    ui.end_row();

                                    ui.label(t!("create_vm.release").to_string());
                                    egui::ComboBox::from_id_salt("create-vm-release")
                                        .selected_text(release_label(&form.profile, &form.release))
                                        .show_ui(ui, |ui| {
                                            for release in &form.profile.releases {
                                                ui.selectable_value(
                                                    &mut form.release,
                                                    release.to_owned(),
                                                    release_label(&form.profile, release),
                                                );
                                            }
                                        });
                                    ui.end_row();
                                }
                                SourceKind::LocalMedia => {
                                    ui.label(t!("create_vm.os_image").to_string());
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [300.0, FIELD_HEIGHT],
                                            egui::TextEdit::singleline(&mut form.image_path)
                                                .hint_text(
                                                    t!("create_vm.os_image_hint").to_string(),
                                                ),
                                        );
                                        if ui.button(t!("common.browse").to_string()).clicked() {
                                            action = Some(CreateVmDialogAction::BrowseImage);
                                        }
                                    });
                                    ui.end_row();
                                }
                            }

                            ui.label(t!("create_vm.hdd_size").to_string());
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut form.disk_gb).range(1..=16_384));
                                ui.label("GiB");
                            });
                            ui.end_row();

                            ui.label(t!("create_vm.ram_size").to_string());
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut form.ram_mb).range(512..=1_048_576),
                                );
                                ui.label("MiB");
                            });
                            ui.end_row();

                            ui.label(t!("create_vm.cpu_cores").to_string());
                            ui.add(egui::DragValue::new(&mut form.cpu_cores).range(1..=256));
                            ui.end_row();

                            ui.label("GPU");
                            egui::ComboBox::from_id_salt("create-vm-gpu")
                                .selected_text(gpu_mode_label(form.gpu_mode))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut form.gpu_mode,
                                        GpuMode::Default,
                                        gpu_mode_label(GpuMode::Default),
                                    );
                                    ui.selectable_value(
                                        &mut form.gpu_mode,
                                        GpuMode::Mirror,
                                        gpu_mode_label(GpuMode::Mirror),
                                    );
                                    ui.selectable_value(
                                        &mut form.gpu_mode,
                                        GpuMode::None,
                                        gpu_mode_label(GpuMode::None),
                                    );
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
                                ui.label(t!("create_vm.desktop").to_string());
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

                            ui.label(t!("create_vm.network").to_string());
                            egui::ComboBox::from_id_salt("create-vm-network")
                                .selected_text(network_mode_label(form.network_mode))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut form.network_mode,
                                        NetworkMode::Nat,
                                        network_mode_label(NetworkMode::Nat),
                                    );
                                    ui.selectable_value(
                                        &mut form.network_mode,
                                        NetworkMode::None,
                                        network_mode_label(NetworkMode::None),
                                    );
                                });
                            ui.end_row();
                        });

                    if form.source_kind == SourceKind::CloudImage {
                        ui.add_space(10.0);
                        ui.separator();
                        ui.strong(t!("create_vm.guest").to_string());
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
                    egui::Button::new(
                        egui::RichText::new(t!("app.create_vm").to_string())
                            .color(egui::Color32::WHITE),
                    )
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
                if ui.button(t!("common.cancel").to_string()).clicked() {
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
            ui.label(t!("create_vm.user_name").to_string());
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.username),
            );
            ui.end_row();

            ui.label(t!("create_vm.password").to_string());
            ui.vertical(|ui| {
                ui.add_sized(
                    [260.0, FIELD_HEIGHT],
                    egui::TextEdit::singleline(&mut form.password)
                        .password(true)
                        .hint_text(t!("create_vm.password_optional").to_string()),
                );
                if form.password.is_empty() {
                    ui.small(t!("create_vm.no_password_note").to_string());
                }
            });
            ui.end_row();

            ui.label("SSH");
            ui.vertical(|ui| {
                ui.checkbox(
                    &mut form.ssh_enabled,
                    t!("create_vm.ssh_server").to_string(),
                );
                ui.add_enabled_ui(form.ssh_enabled, |ui| {
                    ui.checkbox(&mut form.deploy_key, t!("create_vm.ssh_key").to_string());
                    ui.horizontal(|ui| {
                        ui.label(t!("create_vm.port").to_string());
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
                        ui.small(t!("create_vm.port_note").to_string());
                    }
                });
                if form.ssh_enabled && form.deploy_key {
                    match ssh_key_path {
                        Some(path) => {
                            ui.small(
                                t!("create_vm.private_key", path = path.display()).to_string(),
                            );
                        }
                        None => {
                            ui.small(t!("create_vm.private_key_note").to_string());
                        }
                    }
                }
            });
            ui.end_row();

            ui.label(t!("create_vm.locale").to_string());
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.locale),
            );
            ui.end_row();

            ui.label(t!("create_vm.keyboard").to_string());
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.keyboard),
            );
            ui.end_row();

            ui.label(t!("create_vm.timezone").to_string());
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.timezone),
            );
            ui.end_row();
        });
    ui.small(t!("create_vm.guest_defaults_note").to_string());
}

fn render_settings_dialog(
    context: &egui::Context,
    form: &mut SettingsForm,
    distro_options: &[(&str, &str)],
    update_state: &UpdateState,
) -> Option<SettingsDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(t!("settings.title").to_string())
        .collapsible(false)
        .resizable(false)
        .default_width(600.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(t!("settings.description").to_string());
            if form.first_run {
                ui.add_space(4.0);
                ui.strong(t!("settings.first_run_notice").to_string());
                ui.label(t!("settings.first_run_hint").to_string());
            }
            ui.add_space(8.0);
            egui::Grid::new("application-settings-form")
                .num_columns(2)
                .min_col_width(110.0)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.add_sized(
                        [110.0, 24.0],
                        egui::Label::new(t!("settings.vm_storage").to_string()),
                    );
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [310.0, 24.0],
                            egui::TextEdit::singleline(&mut form.vm_storage_path)
                                .hint_text(t!("settings.vm_storage_hint").to_string()),
                        );
                        if ui.button(t!("common.browse").to_string()).clicked() {
                            action = Some(SettingsDialogAction::BrowseVmStorage);
                        }
                    });
                    ui.end_row();

                    ui.add_sized(
                        [110.0, 24.0],
                        egui::Label::new(t!("settings.fps_gap_threshold").to_string()),
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut form.display.fps_gap_threshold_percent)
                                .range(1..=100)
                                .suffix("%"),
                        );
                        ui.label(t!("settings.fps_gap_threshold_hint").to_string());
                    });
                    ui.end_row();

                    ui.add_sized(
                        [110.0, 24.0],
                        egui::Label::new(t!("settings.default_distro").to_string()),
                    );
                    egui::ComboBox::from_id_salt("settings-default-distro")
                        .width(310.0)
                        .selected_text(distro_label(distro_options, &form.default_distro))
                        .show_ui(ui, |ui| {
                            for &(id, name) in distro_options {
                                ui.selectable_value(&mut form.default_distro, id.to_owned(), name);
                            }
                        });
                    ui.end_row();

                    ui.add_sized(
                        [110.0, 24.0],
                        egui::Label::new(t!("settings.language").to_string()),
                    );
                    egui::ComboBox::from_id_salt("settings-language")
                        .width(310.0)
                        .selected_text(language_label(form.language))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut form.language,
                                Language::EnUs,
                                language_label(Language::EnUs),
                            );
                            ui.selectable_value(
                                &mut form.language,
                                Language::RuRu,
                                language_label(Language::RuRu),
                            );
                        });
                    ui.end_row();

                    ui.add_sized(
                        [110.0, 24.0],
                        egui::Label::new(t!("settings.log_directory").to_string()),
                    );
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [310.0, 24.0],
                            egui::TextEdit::singleline(&mut form.log_directory)
                                .hint_text(t!("settings.log_directory_hint").to_string()),
                        );
                        if ui.button(t!("common.browse").to_string()).clicked() {
                            action = Some(SettingsDialogAction::BrowseLogDirectory);
                        }
                    });
                    ui.end_row();

                    ui.add_sized(
                        [110.0, 24.0],
                        egui::Label::new(t!("settings.log_level").to_string()),
                    );
                    egui::ComboBox::from_id_salt("settings-log-level")
                        .selected_text(log_level_label(form.log_level))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut form.log_level,
                                LogLevel::Error,
                                log_level_label(LogLevel::Error),
                            );
                            ui.selectable_value(
                                &mut form.log_level,
                                LogLevel::Warn,
                                log_level_label(LogLevel::Warn),
                            );
                            ui.selectable_value(
                                &mut form.log_level,
                                LogLevel::Info,
                                log_level_label(LogLevel::Info),
                            );
                            ui.selectable_value(
                                &mut form.log_level,
                                LogLevel::Debug,
                                log_level_label(LogLevel::Debug),
                            );
                            ui.selectable_value(
                                &mut form.log_level,
                                LogLevel::Trace,
                                log_level_label(LogLevel::Trace),
                            );
                        });
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.separator();
            if let Some(update_action) = render_update_section(ui, update_state) {
                action = Some(update_action);
            }

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = ui.add(
                    egui::Button::new(
                        egui::RichText::new(t!("common.save").to_string())
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(47, 158, 97))
                    .min_size(egui::vec2(88.0, 30.0)),
                );
                if save.clicked() {
                    match form.settings() {
                        Ok(settings) => {
                            action = Some(SettingsDialogAction::Submit(Box::new(settings)))
                        }
                        Err(error) => form.error = Some(error),
                    }
                }
                let cancel = ui.add(
                    egui::Button::new(
                        egui::RichText::new(t!("common.cancel").to_string())
                            .color(egui::Color32::WHITE),
                    )
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

/// The Updates part of the settings window: what the update state says, and
/// the one step it offers next.
///
/// Every decision here comes from [`update_presentation`]; this draws it and
/// nothing else, which is why the section needs no state of its own.
fn render_update_section(ui: &mut egui::Ui, state: &UpdateState) -> Option<SettingsDialogAction> {
    let mut action = None;
    let presentation = update_presentation(state);

    ui.add_space(4.0);
    ui.strong(t!("updates.section").to_string());
    ui.label(presentation.status);
    if let Some(percent) = presentation.percent {
        ui.add(
            egui::ProgressBar::new(percent as f32 / 100.0)
                .desired_width(300.0)
                .text(t!("selected_vm.percent", percent = percent).to_string()),
        );
    }
    if let Some(detail) = presentation.detail {
        ui.label(detail);
    }

    ui.horizontal(|ui| {
        if let Some(offer) = presentation.action
            && ui.button(update_offer_label(offer)).clicked()
        {
            action = Some(match offer {
                UpdateOffer::Check | UpdateOffer::Retry => SettingsDialogAction::CheckUpdates,
                UpdateOffer::Download => SettingsDialogAction::DownloadUpdate,
                UpdateOffer::Install => SettingsDialogAction::RequestInstall,
            });
        }
        if presentation.cancellable && ui.button(t!("common.cancel").to_string()).clicked() {
            action = Some(SettingsDialogAction::CancelUpdate);
        }
    });
    ui.label(t!("updates.unsigned_note").to_string());

    action
}

/// The confirmation between a verified installer and running it.
///
/// Its own window rather than a button in the section: launching the installer
/// closes VMLord, and that is not something a mis-click on a settings page
/// should be able to do.
fn render_install_confirmation(
    context: &egui::Context,
    version: &str,
) -> Option<InstallConfirmationAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(t!("updates.confirm_title").to_string())
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(t!("updates.confirm_body", version = version).to_string());
            ui.add_space(8.0);
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let install = ui.add(
                    egui::Button::new(
                        egui::RichText::new(t!("updates.confirm_install").to_string())
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(47, 158, 97))
                    .min_size(egui::vec2(88.0, 30.0)),
                );
                if install.clicked() {
                    action = Some(InstallConfirmationAction::Install);
                }
                let cancel = ui.add(
                    egui::Button::new(
                        egui::RichText::new(t!("common.cancel").to_string())
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(100, 100, 100))
                    .min_size(egui::vec2(88.0, 30.0)),
                );
                if cancel.clicked() {
                    action = Some(InstallConfirmationAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(InstallConfirmationAction::Cancel);
    }
    action
}

fn distro_label<'a>(options: &'a [(&str, &str)], selected: &'a str) -> &'a str {
    options
        .iter()
        .find_map(|&(id, name)| (id == selected).then_some(name))
        .unwrap_or(selected)
}

fn render_edit_vm_dialog(
    context: &egui::Context,
    form: &mut EditVmForm,
    host_gpu: Option<&HostGpuCapabilities>,
) -> Option<EditVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(t!("edit_vm.title", name = form.name).to_string())
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(t!("edit_vm.description").to_string());
            ui.small(t!("edit_vm.scope_note").to_string());
            ui.add_space(8.0);

            egui::Grid::new("edit-vm-form")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label(t!("create_vm.vm_name").to_string());
                    ui.label(&form.name);
                    ui.end_row();

                    ui.label(t!("create_vm.ram_size").to_string());
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut form.ram_mb)
                                .range(512..=1_048_576)
                                .speed(2),
                        );
                        ui.label("MiB");
                    });
                    ui.end_row();

                    ui.label(t!("create_vm.hdd_size").to_string());
                    let disk_locked = disk_size_locked(&form.state);
                    ui.add_enabled_ui(disk_locked.is_none(), |ui| {
                        ui.horizontal(|ui| {
                            // The range starts at the size the VM has: a disk
                            // only grows, and a field that can be dragged into
                            // a refusal is a field that lies.
                            let size = ui.add(
                                egui::DragValue::new(&mut form.disk_gb)
                                    .range(form.stored_disk_gb..=16_384),
                            );
                            if let Some(reason) = &disk_locked {
                                size.on_disabled_hover_text(reason.clone());
                            }
                            ui.label("GiB");
                        });
                    });
                    ui.end_row();

                    ui.label("");
                    ui.small(t!("edit_vm.disk_note").to_string());
                    ui.end_row();

                    ui.label(t!("create_vm.cpu_cores").to_string());
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
                                    gpu_mode_label(GpuMode::Default),
                                );
                                ui.selectable_value(
                                    &mut form.gpu_mode,
                                    GpuMode::Mirror,
                                    gpu_mode_label(GpuMode::Mirror),
                                );
                                ui.selectable_value(
                                    &mut form.gpu_mode,
                                    GpuMode::None,
                                    gpu_mode_label(GpuMode::None),
                                );
                            });
                        // The reason before the click rather than after it: the
                        // backend refuses this change under a live VM, and a
                        // control that looks available and is not is worse than
                        // one that says why.
                        if let Some(reason) = &locked {
                            combo.response.on_disabled_hover_text(reason.clone());
                        }
                    });
                    ui.end_row();

                    for warning in gpu_capability_warnings(host_gpu, form.gpu_mode) {
                        ui.label("");
                        ui.colored_label(WARNING_COLOR, warning);
                        ui.end_row();
                    }

                    ui.label(t!("create_vm.network").to_string());
                    egui::ComboBox::from_id_salt("edit-vm-network")
                        .selected_text(network_mode_label(form.network_mode))
                        .show_ui(ui, |ui| {
                            // The same two modes the create form offers: the
                            // native backend refuses the rest until #10, and an
                            // option that always fails is a poor way to say so.
                            ui.selectable_value(
                                &mut form.network_mode,
                                NetworkMode::Nat,
                                network_mode_label(NetworkMode::Nat),
                            );
                            ui.selectable_value(
                                &mut form.network_mode,
                                NetworkMode::None,
                                network_mode_label(NetworkMode::None),
                            );
                        });
                    ui.end_row();

                    // Only a VM that has an SSH daemon has a port to move. One
                    // created without SSH gets no row at all rather than a
                    // disabled one: there is nothing there to enable.
                    if let Some(ssh) = form.ssh.clone() {
                        ui.label(t!("edit_vm.ssh_port").to_string());
                        let locked = ssh_port_locked(&form.state, ssh.authentication);
                        ui.add_enabled_ui(locked.is_none(), |ui| {
                            let port = ui.add(
                                egui::DragValue::new(&mut form.ssh_port)
                                    .range(1..=65_535)
                                    .speed(1),
                            );
                            // The reason before the click, as with the GPU
                            // mode: this change is made inside the running
                            // guest, and a control that looks available and is
                            // not is worse than one that says why.
                            if let Some(reason) = &locked {
                                port.on_disabled_hover_text(reason.clone());
                            }
                        });
                        ui.end_row();

                        if locked.is_none() && form.ssh_port != ssh.port.get() {
                            ui.label("");
                            ui.small(t!("edit_vm.ssh_port_note").to_string());
                            ui.end_row();
                        }
                    }
                });

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = ui.add(
                    egui::Button::new(
                        egui::RichText::new(t!("edit_vm.save").to_string())
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(235, 134, 58)),
                );
                if save.clicked() {
                    match edit_vm_request(form) {
                        Ok(request) => action = Some(EditVmDialogAction::Submit(request)),
                        Err(error) => form.error = Some(error),
                    }
                }
                if ui.button(t!("common.cancel").to_string()).clicked() {
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
    egui::Window::new(t!("delete_vm.title", name = form.vm_name).to_string())
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(t!("delete_vm.description", name = form.vm_name).to_string());
            ui.add_space(8.0);
            ui.checkbox(
                &mut form.delete_disks,
                t!("delete_vm.delete_disks").to_string(),
            );
            if form.delete_disks {
                ui.small(t!("delete_vm.disks_deleted").to_string());
            } else {
                ui.small(t!("delete_vm.disks_kept").to_string());
            }

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let delete = ui.add(
                    egui::Button::new(
                        egui::RichText::new(t!("actions.delete").to_string())
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(192, 57, 43)),
                );
                if delete.clicked() {
                    action = Some(DeleteVmDialogAction::Submit);
                }
                if ui.button(t!("common.cancel").to_string()).clicked() {
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
        return Err(t!("create_vm.name_taken").to_string());
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
                profile: form.profile.clone(),
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
fn release_label(profile: &DistroProfile, release: &str) -> String {
    t!(
        "create_vm.release_label",
        distribution = &profile.name,
        release = release
    )
    .to_string()
}

fn edit_vm_request(form: &EditVmForm) -> Result<VmUpdateRequest, String> {
    if form.ram_mb < 512 || !form.ram_mb.is_multiple_of(2) {
        return Err(t!("edit_vm.ram_invalid").to_string());
    }
    if form.cpu_cores == 0 {
        return Err(t!("edit_vm.cores_invalid").to_string());
    }
    if matches!(form.gpu_mode, GpuMode::Unknown(_)) {
        return Err(t!("edit_vm.gpu_mode_unsupported").to_string());
    }
    if matches!(form.network_mode, NetworkMode::Unknown(_)) {
        return Err(t!("edit_vm.network_mode_unsupported").to_string());
    }

    // A VM with no SSH access carries no port: there is no daemon to move, and
    // a number sent for one would be a request the backend has to refuse.
    let ssh_port = form
        .ssh
        .as_ref()
        .map(|_| SshPort::new(form.ssh_port).map_err(|error| error.to_string()))
        .transpose()?;

    Ok(VmUpdateRequest {
        name: form.name.clone(),
        ram_mb: form.ram_mb,
        disk_gb: form.disk_gb,
        cpu_cores: form.cpu_cores,
        gpu_mode: form.gpu_mode,
        network_mode: form.network_mode,
        ssh_port,
    })
}

fn log_level_label(level: LogLevel) -> String {
    match level {
        LogLevel::Error => t!("log_level.error"),
        LogLevel::Warn => t!("log_level.warning"),
        LogLevel::Info => t!("log_level.info"),
        LogLevel::Debug => t!("log_level.debug"),
        LogLevel::Trace => t!("log_level.trace"),
    }
    .to_string()
}

/// Each language is named in itself, and neither name is translated: a user
/// who cannot read the language on screen has to find their own in this list.
fn language_label(language: Language) -> &'static str {
    match language {
        Language::EnUs => "English (US)",
        Language::RuRu => "Русский",
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
        return t!("common.unknown").to_string();
    };

    let mut detail = t!(
        "selected_vm.status_detail",
        label = gpu_state_label(status.state),
        message = status.message
    )
    .to_string();
    if let Some(adapter) = status
        .native
        .as_ref()
        .and_then(|native| native.adapter.as_ref())
    {
        detail.push_str(&t!("selected_vm.adapter", adapter = adapter));
    }
    if let Some(node) = status
        .guest
        .as_ref()
        .and_then(|guest| guest.render_node.as_ref())
    {
        detail.push_str(&t!("selected_vm.render_node", node = node));
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
        warnings.push(t!("create_vm.no_gpu_adapter").to_string());
    }
    if !capabilities.linux_payload.is_available() {
        warnings.push(t!("create_vm.no_linux_payload").to_string());
    }
    warnings
}

/// Why the GPU mode cannot be changed right now, when it cannot.
///
/// The mode is applied while the compute system is prepared and started, so a
/// change under a live VM would leave a stored mode that does not describe the
/// GPU the guest actually has. RAM and CPU are different: they are read from
/// the configuration on the next start, and nothing claims otherwise.
fn gpu_mode_locked(state: &VmState) -> Option<String> {
    match state {
        VmState::Stopped => None,
        _ => Some(t!("actions.gpu_mode_locked").to_string()),
    }
}

/// Why the disk cannot be grown right now, when it cannot.
///
/// Unlike RAM and CPU, the disk is not a stored setting a later start reads:
/// the VHDX itself is resized, and Hyper-V holds a running VM's disk open
/// exclusively, so only a stopped VM has a disk anything may do to it.
fn disk_size_locked(state: &VmState) -> Option<String> {
    match state {
        VmState::Stopped => None,
        _ => Some(t!("actions.disk_size_locked").to_string()),
    }
}

/// Why this VM's SSH port may not be edited right now, if it may not.
///
/// The port is not a stored setting a later start reads: it lives in files
/// inside the guest, so changing it means reaching a running guest with a
/// credential VMLord can present on its own. A stopped VM has nothing to reach,
/// and a password-mode VM has nothing to present -- neither is a refusal worth
/// discovering after the click.
fn ssh_port_locked(state: &VmState, authentication: SshAuthentication) -> Option<String> {
    if authentication == SshAuthentication::Password {
        return Some(t!("actions.ssh_port_password_locked").to_string());
    }
    match state {
        VmState::Running { .. } => None,
        _ => Some(t!("actions.ssh_port_locked").to_string()),
    }
}

/// Which of the three forms a counted noun takes.
///
/// Russian inflects a counted noun three ways -- 1 ядро, 2 ядра, 5 ядер --
/// and the catalogue backend carries no plural rules. A rule engine would be a
/// large answer to one string: the core count is the only place in the UI
/// where a number stands before a noun that bends. English is served by the
/// same three keys, because its rule -- one against everything else -- is a
/// coarsening of this one, and `en-US.toml` repeats itself in the last two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PluralForm {
    One,
    Few,
    Many,
}

fn plural_form(count: u32) -> PluralForm {
    let last_two = count % 100;
    if (11..=14).contains(&last_two) {
        return PluralForm::Many;
    }
    match count % 10 {
        1 => PluralForm::One,
        2..=4 => PluralForm::Few,
        _ => PluralForm::Many,
    }
}

fn cores_label(count: u32) -> String {
    match plural_form(count) {
        PluralForm::One => t!("vm_table.cores_one", count = count),
        PluralForm::Few => t!("vm_table.cores_few", count = count),
        PluralForm::Many => t!("vm_table.cores_many", count = count),
    }
    .to_string()
}

fn gpu_state_label(state: GpuState) -> String {
    match state {
        GpuState::Disabled => t!("common.disabled"),
        GpuState::WaitingForGuest => t!("gpu_state.waiting_for_guest"),
        GpuState::Assigned => t!("gpu_state.assigned"),
        GpuState::GuestReady => t!("gpu_state.ready"),
        GpuState::Degraded => t!("gpu_state.degraded"),
        GpuState::Failed => t!("gpu_state.failed"),
    }
    .to_string()
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
        .into_iter()
        .map(advisory_text)
        .collect()
}

/// The words for a piece of advice the domain has given.
///
/// The domain decides when to advise and carries the numbers; the sentence is
/// the UI's, because the UI is what has a catalogue and a language.
fn advisory_text(advisory: Advisory) -> String {
    match advisory {
        Advisory::DesktopNeedsCores { required, actual } => t!(
            "advisory.desktop_needs_cores",
            required = required,
            actual = actual
        ),
        Advisory::DesktopNeedsRam {
            required_gib,
            actual_mb,
        } => t!(
            "advisory.desktop_needs_ram",
            required = required_gib,
            actual = actual_mb
        ),
        Advisory::DesktopNeedsCoresAndRam {
            required_cores,
            actual_cores,
            required_gib,
            actual_mb,
        } => t!(
            "advisory.desktop_needs_cores_and_ram",
            required_cores = required_cores,
            actual_cores = actual_cores,
            required_gib = required_gib,
            actual_mb = actual_mb
        ),
        Advisory::DesktopNeedsPassword => t!("advisory.desktop_needs_password"),
    }
    .to_string()
}

fn desktop_profile_label(profile: DesktopProfile) -> String {
    match profile {
        DesktopProfile::Headless => t!("desktop_profile.headless"),
        DesktopProfile::Gnome => t!("desktop_profile.gnome"),
    }
    .to_string()
}

/// What to show beside a VM's desktop, in one line.
///
/// The desktop the guest was found to have is appended rather than shown
/// instead of the state: the row already says what the VM asked for, and a VM
/// asking for GNOME whose guest reports something else is exactly the sentence
/// that needs both halves of.
fn display_status_detail(profile: DesktopProfile, status: Option<&VmDisplayStatus>) -> String {
    let Some(status) = status else {
        return desktop_profile_label(profile);
    };
    let mut detail = t!(
        "selected_vm.status_detail",
        label = display_state_label(status.state),
        message = status.message
    )
    .to_string();
    if let Some(found) = status.desktop.as_ref().and_then(GuestDesktop::summary) {
        detail.push_str(&t!("selected_vm.desktop_found", desktop = found));
    }
    if status.can_retry {
        detail.push_str(&t!("selected_vm.desktop_reinstallable"));
    }
    detail
}

fn display_state_label(state: DisplayState) -> String {
    match state {
        DisplayState::Disabled => t!("common.disabled"),
        DisplayState::Provisioning => t!("display_state.installing"),
        // The same wait the GPU reports, named the same way.
        DisplayState::WaitingForGuest => t!("gpu_state.waiting_for_guest"),
        DisplayState::Ready => t!("display_state.ready"),
        DisplayState::Degraded => t!("display_state.degraded"),
    }
    .to_string()
}

fn gpu_mode_label(mode: GpuMode) -> String {
    match mode {
        GpuMode::None => t!("common.none"),
        GpuMode::Default => t!("common.default"),
        GpuMode::Mirror => t!("gpu_mode.mirror"),
        GpuMode::Unknown(_) => t!("gpu_mode.unsupported"),
    }
    .to_string()
}

fn network_mode_label(mode: NetworkMode) -> String {
    match mode {
        NetworkMode::None => t!("common.none"),
        NetworkMode::Nat => t!("network_mode.nat"),
        NetworkMode::External => t!("network_mode.external"),
        NetworkMode::Internal => t!("network_mode.internal"),
        NetworkMode::Unknown(_) => t!("network_mode.unsupported"),
    }
    .to_string()
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
        BackendStatus::Starting => ui.label(t!("app.backend_starting").to_string()),
        BackendStatus::Ready => ui.colored_label(
            egui::Color32::LIGHT_GREEN,
            t!("app.backend_ready").to_string(),
        ),
        BackendStatus::Unavailable(message) => ui.colored_label(
            egui::Color32::LIGHT_RED,
            t!("app.backend_unavailable", message = message).to_string(),
        ),
    };
}

fn render_vm_list(ui: &mut egui::Ui, vms: &[VmSummary], selected_vm_name: &mut Option<String>) {
    ui.heading(t!("vm_table.title").to_string());
    if vms.is_empty() {
        *selected_vm_name = None;
        ui.weak(t!("vm_table.empty").to_string());
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
            ui.strong(t!("vm_table.name").to_string());
            ui.strong(t!("vm_table.os").to_string());
            ui.strong(t!("vm_table.status").to_string());
            ui.strong(t!("vm_table.agent_status").to_string());
            ui.strong(t!("vm_table.cpu").to_string());
            ui.strong(t!("vm_table.ram").to_string());
            ui.strong(t!("vm_table.disk").to_string());
            ui.strong("GPU");
            ui.strong(t!("vm_table.network_type").to_string());
            ui.end_row();
            for vm in vms {
                let is_selected = selected_vm_name.as_deref() == Some(vm.name.as_str());
                if ui.selectable_label(is_selected, &vm.name).clicked() {
                    *selected_vm_name = Some(vm.name.clone());
                }
                ui.label(&vm.os_type);
                ui.label(vm_state_label(vm.state));
                render_agent_status(ui, agent_status(vm.state));
                ui.label(cores_label(vm.cpu_cores));
                ui.label(t!("vm_table.mebibytes", count = vm.ram_mb).to_string());
                ui.label(t!("vm_table.gibibytes", count = vm.disk_gb).to_string());
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
fn ssh_action_label() -> String {
    t!("actions.open_ssh").to_string()
}

/// What the SSH action can offer for one VM right now.
///
/// Three answers rather than a boolean, because "no button" and "a button that
/// cannot be pressed yet" are different things to see: the first says this VM
/// was created without SSH and never will have it, the second says to wait, and
/// names what for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SshOffer {
    /// This VM has no SSH access at all, so there is no action to show.
    Absent,
    /// SSH is configured, but the guest cannot be reached yet.
    Waiting(String),
    Ready,
}

impl SshOffer {
    /// The tooltip of a button that cannot be pressed, and nothing for one that
    /// can.
    fn waiting_for(&self) -> Option<&str> {
        match self {
            Self::Waiting(reason) => Some(reason.as_str()),
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
        VmState::Building { .. } => SshOffer::Waiting(t!("actions.after_start").to_string()),
        VmState::Stopped | VmState::Starting => {
            SshOffer::Waiting(t!("actions.while_running").to_string())
        }
        VmState::Running { .. } if vm.ip_address.is_none() => {
            SshOffer::Waiting(t!("actions.needs_address").to_string())
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
        return t!("common.disabled").to_string();
    };

    match vm.ip_address {
        Some(address) => t!(
            "ssh.endpoint",
            user = ssh.username,
            host = address,
            port = ssh.port,
            login = ssh.authentication
        ),
        None => t!(
            "ssh.endpoint_pending",
            user = ssh.username,
            port = ssh.port,
            login = ssh.authentication
        ),
    }
    .to_string()
}

/// Whether Connect is offered, and what to say when it is not.
///
/// The display's own status rather than "the VM is running": a running VM
/// whose desktop is still installing has nothing to open a window on, and a
/// running VM whose guest has not offered its display would leave a viewer
/// retrying a service nothing binds. The sentence explaining either is the
/// application layer's, which is why this reads one rather than writing one.
fn connect_offer(status: Option<&VmDisplayStatus>) -> (bool, Option<String>) {
    match status {
        Some(status) if status.is_connectable() => (true, None),
        Some(status) => (false, Some(status.message.clone())),
        // A VM the application has derived nothing for yet -- one refresh old
        // at most. Offering a window on it would be offering a guess.
        None => (false, Some(t!("actions.display_not_reported").to_string())),
    }
}

/// Whether Update display is offered, and what the button says either way.
///
/// Four facts have to line up before the guest can be asked, and the backend
/// checks them again before it stages anything: the VM runs, its guest has
/// reported the payload version it has, this release carries a different one,
/// and no update of this VM is already running. Which of them is missing
/// belongs here rather than in a refusal after the click: an update is minutes
/// of a guest building a kernel module, and a person deciding whether to start
/// one is deciding about those minutes.
///
/// The sentence for a display that has nothing to report yet is the application
/// layer's, for the reason [`connect_offer`] takes it from there: installing,
/// waiting for the guest and a desktop that failed are three different answers
/// and none of them is the UI's to word.
fn update_display_offer(state: VmState, status: Option<&VmDisplayStatus>) -> (bool, String) {
    if !matches!(state, VmState::Running { .. }) {
        return (false, t!("actions.only_while_running").to_string());
    }
    let Some(status) = status else {
        return (false, t!("actions.display_not_reported").to_string());
    };
    // Before the versions, because they are the ones the update started from:
    // what a second press would ask for is a version already being moved to.
    if status.updating {
        return (false, t!("actions.display_update_running").to_string());
    }
    let Some(running) = status.running_version.as_deref() else {
        return (false, status.message.clone());
    };

    match status.available_version.as_deref() {
        Some(available) => (
            true,
            t!(
                "actions.display_update_offer",
                running = running,
                available = available
            )
            .to_string(),
        ),
        None => (
            false,
            t!("actions.display_up_to_date", running = running).to_string(),
        ),
    }
}

/// What the details panel says about the display payload versions.
///
/// Beside the status rather than inside it: whether there is an update to make
/// is a fact about the VM a person reads before reaching for the button, and a
/// tooltip is only found by someone who already suspects it.
fn display_payload_detail(status: Option<&VmDisplayStatus>) -> String {
    let Some(status) = status else {
        return t!("selected_vm.not_reported").to_string();
    };

    match (
        status.running_version.as_deref(),
        status.available_version.as_deref(),
    ) {
        (Some(running), Some(available)) if status.updating => t!(
            "selected_vm.payload_updating",
            running = running,
            available = available
        )
        .to_string(),
        (Some(running), Some(available)) => t!(
            "selected_vm.payload_offered",
            running = running,
            available = available
        )
        .to_string(),
        (Some(running), None) => running.to_owned(),
        (None, Some(available)) => {
            t!("selected_vm.payload_none_yet", available = available).to_string()
        }
        (None, None) => t!("selected_vm.not_reported").to_string(),
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
    ui.heading(t!("selected_vm.title", name = vm.name).to_string());

    let start = t!("actions.start").to_string();
    let stop = t!("actions.stop").to_string();
    let primary_action = match vm.state {
        VmState::Stopped | VmState::Building { .. } => (VmAction::Start, start.as_str()),
        VmState::Starting | VmState::Running { .. } => (VmAction::Stop, stop.as_str()),
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
            &[
                primary_action,
                (VmAction::ForceStop, &t!("actions.force_stop")),
            ],
            !is_building,
            Some(&t!("actions.after_build")),
        );
        ui.separator();
        let (can_connect, waiting_for) = connect_offer(display_status);
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Connect, &t!("actions.connect"))],
            can_connect,
            waiting_for.as_deref(),
        ) {
            action = Some(clicked_action);
        }
        // Beside Connect because it is about the same window: the payload is
        // what draws it, and moving it to a newer version is the one thing
        // about the display a start does not do by itself.
        let (can_update, update_offer) = update_display_offer(vm.state, display_status);
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::UpdateDisplay, &t!("actions.update_display"))],
            can_update,
            Some(update_offer.as_str()),
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
                &[(VmAction::Ssh, &ssh_action_label())],
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
            &[(VmAction::Console, &t!("actions.open_com_port"))],
            is_running,
            Some(&t!("actions.only_while_running")),
        ) {
            action = Some(clicked_action);
        }
        ui.separator();
        // The only thing that can be done to a VM while it is being built, and
        // the only time it can be done: the build rolls itself back and the
        // row disappears on its own.
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::CancelCreate, &t!("actions.cancel_creation"))],
            is_building,
            Some(&t!("actions.only_while_building")),
        ) {
            action = Some(clicked_action);
        }
        ui.separator();
        // Editing a running VM is allowed; the change reaches it on its next
        // start. Deleting one is not.
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Edit, &t!("actions.edit"))],
            !is_building,
            Some(&t!("actions.restart_needed")),
        ) {
            action = Some(clicked_action);
        }
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Delete, &t!("actions.delete"))],
            can_delete,
            Some(&t!("actions.only_while_stopped")),
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
                &t!("selected_vm.ip_address"),
                vm.ip_address
                    .map_or_else(|| t!("common.unavailable").to_string(), |ip| ip.to_string()),
            );
            detail_row(ui, &t!("selected_vm.operating_system"), vm.os_type.clone());
            detail_row(ui, &t!("vm_table.status"), vm_state(vm.state));
            if let VmState::Building { progress } = vm.state {
                render_build_progress(ui, progress);
            }
            detail_row(
                ui,
                &t!("vm_table.agent_status"),
                agent_status_label(agent_status(vm.state)),
            );
            detail_row(
                ui,
                &t!("vm_table.network_type"),
                network_mode_label(vm.network_mode),
            );
            detail_row(ui, &t!("vm_table.cpu"), cores_label(vm.cpu_cores));
            detail_row(
                ui,
                &t!("vm_table.ram"),
                t!("vm_table.mebibytes", count = vm.ram_mb).to_string(),
            );
            detail_row(
                ui,
                &t!("vm_table.disk"),
                t!("vm_table.gibibytes", count = vm.disk_gb).to_string(),
            );
            detail_row(ui, "GPU", gpu_mode_label(vm.gpu_mode));
            detail_row(
                ui,
                &t!("selected_vm.gpu_status"),
                gpu_status_detail(gpu_status),
            );
            detail_row(
                ui,
                &t!("create_vm.desktop"),
                desktop_profile_label(vm.desktop_profile),
            );
            detail_row(
                ui,
                &t!("selected_vm.desktop_status"),
                display_status_detail(vm.desktop_profile, display_status),
            );
            if vm.desktop_profile.wants_desktop() {
                detail_row(
                    ui,
                    &t!("selected_vm.display_payload"),
                    display_payload_detail(display_status),
                );
            }
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
                .map(|reason| {
                    t!("selected_vm.locked_reason", label = label, reason = reason).to_string()
                })
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

    ui.strong(t!("selected_vm.progress").to_string());
    ui.vertical(|ui| {
        if let Some(percent) = download_percentage(progress) {
            ui.add(
                egui::ProgressBar::new(percent as f32 / 100.0)
                    .desired_width(260.0)
                    .text(t!("selected_vm.percent", percent = percent).to_string()),
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

/// One record as the panel shows it.
///
/// The moment comes first because the panel's whole use is lining an event up
/// with the same event in `vmlord.log`, and the code comes last because it is
/// what a reader copies into a search.
fn diagnostic_line(diagnostic: &vmlord_core::Diagnostic) -> String {
    let mut line = format!(
        "[{}] {}",
        vmlord_core::format_timestamp(diagnostic.at),
        diagnostic.message
    );
    if let Some(vm) = &diagnostic.vm {
        line.push_str(&format!(" ({vm})"));
    }
    if let Some(code) = diagnostic.code {
        line.push_str(&format!(" [0x{code:08X}]"));
    }
    line
}

fn render_diagnostics(ui: &mut egui::Ui, diagnostics: &[vmlord_core::Diagnostic]) {
    ui.collapsing(t!("diagnostics.title").to_string(), |ui| {
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
                    ui.colored_label(color, diagnostic_line(diagnostic));
                }
            });
    });
}

fn render_agent_status(ui: &mut egui::Ui, status: AgentStatus) {
    let color = match status {
        AgentStatus::Unknown => egui::Color32::GRAY,
        AgentStatus::Offline => egui::Color32::LIGHT_RED,
        AgentStatus::Online => egui::Color32::LIGHT_GREEN,
    };
    let label = agent_status_label(status);
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

fn agent_status_label(status: AgentStatus) -> String {
    match status {
        AgentStatus::Unknown => t!("common.unknown"),
        AgentStatus::Offline => t!("agent_status.offline"),
        AgentStatus::Online => t!("agent_status.online"),
    }
    .to_string()
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
            Some(percent) => {
                t!("vm_state.with_percentage", label = label, percent = percent).to_string()
            }
            None => label,
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
    phase_percentage(progress.download?)
}

/// The share of a transfer that is done, when the phase publishes both halves
/// of the fraction.
///
/// Shared with the application-update section: a downloaded installer and a
/// downloaded cloud image are the same transfer to a progress bar.
fn phase_percentage(phase: DownloadPhase) -> Option<u64> {
    match phase {
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
    Some(download_detail(progress.download?))
}

/// What a transfer is doing right now, in bytes a person can compare with the
/// size they were told to expect.
fn download_detail(phase: DownloadPhase) -> String {
    match phase {
        DownloadPhase::Connecting => t!("build.connecting").to_string(),
        DownloadPhase::Downloading {
            downloaded,
            total: Some(total),
        } => t!(
            "build.downloaded_of",
            done = mebibytes(downloaded),
            total = mebibytes(total),
            percent = percentage(downloaded, total)
        )
        .to_string(),
        // A server that sent no length leaves nothing to divide by; the count
        // still shows the download is moving.
        DownloadPhase::Downloading {
            downloaded,
            total: None,
        } => t!("build.downloaded", done = mebibytes(downloaded)).to_string(),
        DownloadPhase::Verifying { hashed, total } => t!(
            "build.checking",
            done = mebibytes(hashed),
            total = mebibytes(total),
            percent = percentage(hashed, total)
        )
        .to_string(),
        DownloadPhase::Completed => t!("build.image_ready").to_string(),
    }
}

/// Everything the Updates section shows for one application-update state.
///
/// Separated from the egui code because these are decisions, not drawing: what
/// a person is told and what they are allowed to press follows from the state
/// alone, and that is what the tests hold onto. `render_update_section` then
/// only lays this out.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdatePresentation {
    /// The single line above the buttons: the phase, and the version it is about.
    status: String,
    /// Release notes, transfer counts, or the message of a failure.
    detail: Option<String>,
    /// The fraction a bar is drawn at, when the phase publishes counts.
    percent: Option<u64>,
    /// The button that starts the next step, when the state has a next step.
    action: Option<UpdateOffer>,
    /// Whether the work in flight can be called off.
    cancellable: bool,
}

/// The one thing the Updates section offers to start.
///
/// `Retry` is not `Check` even though both end in a check: after a failure the
/// button says so, and a section that offers "Check for updates" under an error
/// reads as if the error had been forgotten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateOffer {
    Check,
    Download,
    Install,
    Retry,
}

/// What the Updates section shows for the state the application reports.
fn update_presentation(state: &UpdateState) -> UpdatePresentation {
    match state {
        UpdateState::Idle => UpdatePresentation {
            status: t!("updates.idle").to_string(),
            detail: None,
            percent: None,
            action: Some(UpdateOffer::Check),
            cancellable: false,
        },
        UpdateState::Checking => UpdatePresentation {
            status: t!("updates.checking").to_string(),
            detail: None,
            percent: None,
            action: None,
            cancellable: false,
        },
        UpdateState::Available(update) => UpdatePresentation {
            status: t!("updates.available", version = update.validated.version).to_string(),
            detail: release_notes(update),
            percent: None,
            action: Some(UpdateOffer::Download),
            cancellable: false,
        },
        UpdateState::Downloading { update, progress } => UpdatePresentation {
            status: t!("updates.downloading", version = update.validated.version).to_string(),
            // Before the first byte there is no phase to report; the status
            // line already says a download is under way.
            detail: progress.map(download_detail),
            percent: progress.and_then(phase_percentage),
            action: None,
            cancellable: true,
        },
        UpdateState::Ready {
            update,
            installing: false,
            ..
        } => UpdatePresentation {
            status: t!("updates.ready", version = update.validated.version).to_string(),
            detail: Some(t!("updates.ready_hint").to_string()),
            percent: None,
            action: Some(UpdateOffer::Install),
            cancellable: false,
        },
        // Windows has the installer and this process is on its way out, so
        // there is nothing left to press and nothing left to call off.
        UpdateState::Ready {
            update,
            installing: true,
            ..
        } => UpdatePresentation {
            status: t!("updates.installing", version = update.validated.version).to_string(),
            detail: Some(t!("updates.installing_hint").to_string()),
            percent: None,
            action: None,
            cancellable: false,
        },
        UpdateState::Failed { message } => UpdatePresentation {
            status: t!("updates.failed").to_string(),
            detail: Some(message.clone()),
            percent: None,
            action: Some(UpdateOffer::Retry),
            cancellable: false,
        },
    }
}

/// The notes a person reads before accepting a version, or nothing when the
/// release carried none: an empty box under the version says less than no box.
fn release_notes(update: &AvailableUpdate) -> Option<String> {
    let notes = update.release_notes.trim();
    (!notes.is_empty()).then(|| notes.to_string())
}

/// The label on the button that starts the next step.
fn update_offer_label(offer: UpdateOffer) -> String {
    match offer {
        UpdateOffer::Check => t!("updates.check"),
        UpdateOffer::Download => t!("updates.download"),
        UpdateOffer::Install => t!("updates.install"),
        UpdateOffer::Retry => t!("updates.retry"),
    }
    .to_string()
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

fn vm_state(state: VmState) -> String {
    match state {
        VmState::Stopped => t!("vm_state.stopped"),
        VmState::Building { progress } => match progress.step {
            BuildStep::Downloading => t!("vm_state.building_downloading"),
            BuildStep::WritingDisk => t!("vm_state.building_writing_disk"),
            BuildStep::Provisioning => t!("vm_state.building_provisioning"),
            BuildStep::Registering => t!("vm_state.building_registering"),
            BuildStep::Starting => t!("vm_state.building_starting"),
            BuildStep::AwaitingGuest => t!("vm_state.building_waiting"),
        },
        VmState::Starting => t!("vm_state.starting"),
        VmState::Running { .. } => t!("vm_state.running"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use rust_i18n::t;

    /// Every key of one catalogue exists in the other.
    ///
    /// A forgotten translation would otherwise fall back to English silently,
    /// which looks like a rendering bug months later rather than a missing
    /// line in a pull request.
    #[test]
    fn the_catalogues_agree_on_their_keys() {
        let english = catalogue_keys(include_str!("../locales/en-US.toml"));
        let russian = catalogue_keys(include_str!("../locales/ru-RU.toml"));

        let missing_in_russian: Vec<_> = english.difference(&russian).collect();
        let missing_in_english: Vec<_> = russian.difference(&english).collect();

        assert!(
            missing_in_russian.is_empty(),
            "not translated: {missing_in_russian:?}"
        );
        assert!(
            missing_in_english.is_empty(),
            "no English original: {missing_in_english:?}"
        );
    }

    /// The dotted paths of every string in a catalogue.
    fn catalogue_keys(document: &str) -> std::collections::BTreeSet<String> {
        fn walk(prefix: &str, value: &toml::Value, keys: &mut std::collections::BTreeSet<String>) {
            match value {
                toml::Value::Table(table) => {
                    for (key, value) in table {
                        let path = if prefix.is_empty() {
                            key.clone()
                        } else {
                            format!("{prefix}.{key}")
                        };
                        walk(&path, value, keys);
                    }
                }
                _ => {
                    keys.insert(prefix.to_string());
                }
            }
        }

        let document: toml::Value = toml::from_str(document).expect("catalogue parses");
        let mut keys = std::collections::BTreeSet::new();
        walk("", &document, &mut keys);
        keys
    }

    #[test]
    fn a_core_count_takes_the_form_russian_asks_for() {
        assert_eq!(
            t!("vm_table.cores_one", locale = "ru-RU", count = 1),
            "1 ядро"
        );
        assert_eq!(
            t!("vm_table.cores_few", locale = "ru-RU", count = 2),
            "2 ядра"
        );
        assert_eq!(
            t!("vm_table.cores_many", locale = "ru-RU", count = 5),
            "5 ядер"
        );
    }

    #[test]
    fn the_plural_form_follows_the_count() {
        use super::{PluralForm, plural_form};

        assert_eq!(plural_form(1), PluralForm::One);
        assert_eq!(plural_form(2), PluralForm::Few);
        assert_eq!(plural_form(5), PluralForm::Many);
        assert_eq!(plural_form(11), PluralForm::Many);
        assert_eq!(plural_form(21), PluralForm::One);
        assert_eq!(plural_form(0), PluralForm::Many);
    }

    /// The advice keeps the domain's numbers and takes the UI's words.
    #[test]
    fn an_advisory_is_worded_by_the_catalogue() {
        use super::{Advisory, advisory_text};

        assert_eq!(
            advisory_text(Advisory::DesktopNeedsCores {
                required: 2,
                actual: 1
            }),
            "A GNOME desktop is slow below 2 CPU cores; this VM has 1."
        );
        assert_eq!(
            t!("advisory.desktop_needs_password", locale = "ru-RU"),
            "У ВМ с рабочим столом без пароля нечего ввести на экране входа; задайте его здесь или позже по SSH."
        );
    }

    #[test]
    fn the_actions_are_translated() {
        assert_eq!(t!("actions.start", locale = "ru-RU"), "Запустить");
        assert_eq!(
            t!(
                "ssh.endpoint",
                locale = "ru-RU",
                user = "dev",
                host = "172.30.0.5",
                port = 2222,
                login = "key"
            ),
            "dev@172.30.0.5:2222 (вход: key)"
        );
    }

    #[test]
    fn the_settings_dialog_is_translated() {
        assert_eq!(
            t!("settings.title", locale = "ru-RU"),
            "Настройки приложения"
        );
        assert_ne!(
            t!("settings.title", locale = "ru-RU"),
            t!("settings.title", locale = "en-US")
        );
    }

    #[test]
    fn a_record_is_shown_with_its_moment_its_vm_and_its_code() {
        // The panel exists to be lined up against `vmlord.log`; without the
        // stamp there is nothing to line up.
        let line = super::diagnostic_line(&vmlord_core::Diagnostic {
            level: vmlord_core::DiagnosticLevel::Error,
            subsystem: vmlord_core::Subsystem::Hcs,
            vm: Some("dev-linux".into()),
            code: Some(0x803B_0014),
            at: std::time::UNIX_EPOCH,
            message: "the endpoint was already attached".into(),
        });

        assert_eq!(
            line,
            "[1970-01-01T00:00:00.000Z] the endpoint was already attached \
             (dev-linux) [0x803B0014]"
        );
    }

    #[test]
    fn a_record_about_no_vm_in_particular_shows_neither_a_name_nor_a_code() {
        let line = super::diagnostic_line(&vmlord_core::Diagnostic {
            level: vmlord_core::DiagnosticLevel::Info,
            subsystem: vmlord_core::Subsystem::App,
            vm: None,
            code: None,
            at: std::time::UNIX_EPOCH,
            message: "Application settings saved".into(),
        });

        assert_eq!(
            line,
            "[1970-01-01T00:00:00.000Z] Application settings saved"
        );
    }

    use std::net::{IpAddr, Ipv4Addr};

    use vmlord_core::{
        DisplayStage, DisplayState, DisplayStatusCode, GpuAvailability, GpuFailure, GpuStatusCode,
        InstallerAsset, SshAuthentication, SshAvailability, SshConfig, ValidatedUpdate, VmGpuFacts,
        ubuntu,
    };

    use super::*;

    #[test]
    fn application_icon_decodes_to_rgba_pixels() {
        let icon = application_icon();

        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

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
            desktop: None,
            can_retry: false,
            updating: false,
            observed_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_desktop_row_says_what_the_guest_has_beside_what_it_was_asked_for() {
        let mut status = display_status(DisplayState::Ready, "The guest offers its desktop.");
        status.desktop = Some(vmlord_core::GuestDesktop {
            session: Some("Hyprland".to_owned()),
            session_type: Some("wayland".to_owned()),
            display_manager: None,
        });

        let detail = display_status_detail(DesktopProfile::Gnome, Some(&status));

        assert!(
            detail.contains("Hyprland, wayland"),
            "a guest running something other than what was asked for says so: {detail}"
        );
    }

    #[test]
    fn a_desktop_the_guest_never_reported_adds_nothing_to_the_row() {
        let status = display_status(DisplayState::Provisioning, "The desktop is installing.");

        let detail = display_status_detail(DesktopProfile::Gnome, Some(&status));

        assert_eq!(detail, "Installing: The desktop is installing.");
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
                Some("The desktop is installed; waiting for the guest to offer it.".to_owned())
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

    /// One derived display status with versions in it, which is what an
    /// update is decided from.
    fn payload_status(running: Option<&str>, available: Option<&str>) -> VmDisplayStatus {
        VmDisplayStatus {
            running_version: running.map(str::to_owned),
            available_version: available.map(str::to_owned),
            ..display_status(DisplayState::Ready, "The guest offers its desktop.")
        }
    }

    /// The three facts the backend checks before it touches anything, checked
    /// here so that a person reads them instead of a refusal after the click.
    #[test]
    fn a_display_payload_update_is_offered_only_when_there_is_one_to_make() {
        let running = VmState::Running {
            agent_status: AgentStatus::Online,
        };

        let (offered, reason) =
            update_display_offer(running, Some(&payload_status(Some("0.1.4"), Some("0.1.5"))));
        assert!(offered);
        assert!(
            reason.contains("0.1.4") && reason.contains("0.1.5"),
            "the offer names both versions: {reason}"
        );

        let (offered, reason) = update_display_offer(
            VmState::Stopped,
            Some(&payload_status(Some("0.1.4"), Some("0.1.5"))),
        );
        assert!(!offered, "a stopped VM has nobody to ask");
        assert!(reason.contains("running"), "{reason}");

        let (offered, reason) =
            update_display_offer(running, Some(&payload_status(Some("0.1.5"), None)));
        assert!(
            !offered,
            "a release with nothing else to offer offers nothing"
        );
        assert!(reason.contains("0.1.5"), "{reason}");
    }

    /// An update takes minutes of a guest building a kernel module, and a
    /// second press during them would ask for a version already being moved to.
    #[test]
    fn a_vm_already_updating_is_not_offered_a_second_update() {
        let updating = VmDisplayStatus {
            updating: true,
            ..payload_status(Some("0.1.4"), Some("0.1.5"))
        };

        let (offered, reason) = update_display_offer(
            VmState::Running {
                agent_status: AgentStatus::Online,
            },
            Some(&updating),
        );

        assert!(!offered);
        assert!(reason.contains("already"), "{reason}");
        assert_eq!(
            display_payload_detail(Some(&updating)),
            "0.1.4 (updating to 0.1.5)",
            "the panel says an update is under way, not that one is available"
        );
    }

    /// A guest that has not reported its payload yet is explained by the
    /// application layer's own sentence rather than by one invented here.
    #[test]
    fn an_unreported_payload_says_what_the_display_is_waiting_for() {
        let waiting = VmDisplayStatus {
            running_version: None,
            available_version: Some("0.1.5".into()),
            ..display_status(
                DisplayState::WaitingForGuest,
                "The desktop is installed; waiting for the guest to offer it.",
            )
        };

        assert_eq!(
            update_display_offer(
                VmState::Running {
                    agent_status: AgentStatus::Online
                },
                Some(&waiting)
            ),
            (
                false,
                "The desktop is installed; waiting for the guest to offer it.".to_owned()
            )
        );

        let (offered, reason) = update_display_offer(
            VmState::Running {
                agent_status: AgentStatus::Online,
            },
            None,
        );
        assert!(!offered);
        assert!(!reason.is_empty());
    }

    /// The versions are in the panel as well as in the tooltip: whether an
    /// update is there to be made is a fact about the VM, not a hover.
    #[test]
    fn the_details_state_both_payload_versions() {
        assert_eq!(
            display_payload_detail(Some(&payload_status(Some("0.1.4"), Some("0.1.5")))),
            "0.1.4 (this release offers 0.1.5)"
        );
        assert_eq!(
            display_payload_detail(Some(&payload_status(Some("0.1.5"), None))),
            "0.1.5"
        );
        assert_eq!(
            display_payload_detail(Some(&payload_status(None, None))),
            "Not reported"
        );
        assert_eq!(display_payload_detail(None), "Not reported");
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
        CreateVmForm::new(
            "ubuntu",
            &ubuntu(),
            &GuestDefaults {
                locale: "ru_RU.UTF-8".into(),
                keyboard: "ru".into(),
                timezone: "Europe/Moscow".into(),
            },
        )
    }

    /// The second shipped profile, the way the catalogue would load it.
    fn arch() -> DistroProfile {
        let mut profile = ubuntu();
        profile.name = "Arch Linux".into();
        profile.releases = vec!["rolling".into()];
        profile.default_user = "arch".into();
        profile
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
        assert_eq!(form.release, form.profile.releases[0]);
        for release in &form.profile.releases {
            assert!(
                form.profile
                    .image_url(release)
                    .ends_with(&format!("ubuntu-{release}-server-cloudimg-amd64.img")),
                "the resolver has to be able to build a URL for {release}"
            );
        }
    }

    #[test]
    fn a_new_form_uses_the_profile_selected_by_application_settings() {
        let mut profile = ubuntu();
        profile.name = "Fedora".into();
        profile.releases = vec!["42".into(), "41".into()];
        profile.default_user = "fedora".into();

        let form = CreateVmForm::new("fedora", &profile, &GuestDefaults::default());

        assert_eq!(form.release, "42");
        assert_eq!(form.username, "fedora");
        assert_eq!(form.name, "fedora");
    }

    /// A field nobody touched follows the distribution out; the release is the
    /// one field that restarts even when it was chosen, because releases of
    /// two distributions never overlap.
    #[test]
    fn a_distro_switch_resets_the_fields_the_form_generated() {
        let mut form = cloud_form();
        form.select_distro("arch", &arch());

        assert_eq!(form.distro_id, "arch");
        assert_eq!(form.release, "rolling");
        assert_eq!(form.username, "arch");
        assert_eq!(form.name, "arch");
    }

    #[test]
    fn a_distro_switch_keeps_the_fields_someone_edited() {
        let mut form = CreateVmForm {
            name: "my-vm".into(),
            username: "custom".into(),
            ..cloud_form()
        };
        form.select_distro("arch", &arch());

        assert_eq!(form.name, "my-vm");
        assert_eq!(form.username, "custom");
        assert_eq!(form.release, "rolling");
    }

    #[test]
    fn a_switched_form_builds_a_request_with_the_new_profile() {
        let mut form = cloud_form();
        form.select_distro("arch", &arch());

        let request = create_vm_request(&form, &[]).unwrap();

        let VmSource::CloudImage { image, .. } = &request.source else {
            panic!("expected a cloud image request");
        };
        assert_eq!(image.profile, arch());
        assert_eq!(image.release, "rolling");
        assert_eq!(provisioning_of(&request).username, "arch");
    }

    #[test]
    fn distro_setting_shows_the_profile_name_for_its_stored_identifier() {
        let options = [("fedora", "Fedora Linux"), ("ubuntu", "Ubuntu")];

        assert_eq!(distro_label(&options, "fedora"), "Fedora Linux");
        assert_eq!(distro_label(&options, "missing"), "missing");
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

    /// An edit form of a VM whose SSH access is `ssh`, with everything else
    /// left as it was opened.
    fn edit_form(ssh: Option<SshConfig>) -> EditVmForm {
        EditVmForm {
            name: "dev".into(),
            ram_mb: 2048,
            disk_gb: 20,
            stored_disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::Default,
            network_mode: NetworkMode::Nat,
            ssh_port: ssh.as_ref().map_or(SshPort::DEFAULT, |ssh| ssh.port).get(),
            ssh,
            state: VmState::Running {
                agent_status: AgentStatus::Online,
            },
            error: None,
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
        assert_eq!(ssh_action_label(), "Open SSH");
        assert!(!ssh_action_label().contains("Terminal"));
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
            disk_gb: 20,
            stored_disk_gb: 20,
            cpu_cores: 8,
            gpu_mode: GpuMode::Mirror,
            network_mode: NetworkMode::Nat,
            ssh: None,
            ssh_port: SshPort::DEFAULT.get(),
            error: None,
            state: VmState::Stopped,
        })
        .unwrap();

        assert_eq!(request.name, "dev");
        assert_eq!(request.gpu_mode, GpuMode::Mirror);
        assert_eq!(request.network_mode, NetworkMode::Nat);
    }

    /// A VM that has an SSH daemon carries its port in every update, so that
    /// the backend can tell "still 22" from "moved to 22".
    #[test]
    fn the_edited_ssh_port_reaches_the_request() {
        let request = edit_vm_request(&EditVmForm {
            ssh_port: 2222,
            ..edit_form(Some(ssh_config()))
        })
        .unwrap();

        assert_eq!(request.ssh_port, Some(SshPort::new(2222).unwrap()));
    }

    /// A VM created without SSH has no daemon to move, and a port sent for one
    /// would be a request the backend has to refuse.
    #[test]
    fn a_vm_without_ssh_sends_no_port() {
        let request = edit_vm_request(&edit_form(None)).unwrap();

        assert_eq!(request.ssh_port, None);
    }

    /// The widget clamps to `1..=65535`, and a form is still a form: what the
    /// domain refuses must be refused here rather than sent.
    #[test]
    fn edit_vm_request_rejects_a_port_nothing_can_connect_to() {
        let error = edit_vm_request(&EditVmForm {
            ssh_port: 0,
            ..edit_form(Some(ssh_config()))
        })
        .unwrap_err();

        assert!(error.contains("SSH port"), "got {error}");
    }

    #[test]
    fn a_form_opens_on_the_port_the_vm_listens_on() {
        let form = EditVmForm::from_vm(&VmSummary {
            ssh: SshAvailability::Enabled(SshConfig {
                port: SshPort::new(2222).unwrap(),
                ..ssh_config()
            }),
            ..vm_summary()
        });

        assert_eq!(form.ssh_port, 2222);
    }

    /// The port is changed inside the running guest, so a VM that is not
    /// running has nothing to change -- said before the click rather than
    /// after it.
    #[test]
    fn a_stopped_vm_says_why_its_ssh_port_cannot_be_edited() {
        let reason = ssh_port_locked(&VmState::Stopped, SshAuthentication::VmlordKey)
            .expect("a stopped VM cannot be reconfigured");

        assert!(reason.contains("Start the VM"), "got {reason}");
        assert_eq!(
            ssh_port_locked(
                &VmState::Running {
                    agent_status: AgentStatus::Online,
                },
                SshAuthentication::VmlordKey
            ),
            None
        );
    }

    /// Nobody is at a prompt when VMLord runs the reconfiguration, and key
    /// mode is the only credential it can present on its own.
    #[test]
    fn a_password_vm_says_why_its_ssh_port_cannot_be_edited() {
        let reason = ssh_port_locked(
            &VmState::Running {
                agent_status: AgentStatus::Online,
            },
            SshAuthentication::Password,
        )
        .expect("a password cannot be typed into a command VMLord runs");

        assert!(reason.contains("password"), "got {reason}");
    }

    /// The disk file is resized then and there, and Hyper-V holds a running
    /// VM's VHDX open exclusively -- so the field is closed while the VM
    /// lives, and says why before the drag rather than after the click.
    #[test]
    fn a_live_vm_says_why_its_disk_cannot_be_grown() {
        let reason = disk_size_locked(&VmState::Running {
            agent_status: AgentStatus::Online,
        })
        .expect("a running VM holds its own disk open");

        assert!(reason.contains("Stop the VM"), "got {reason}");
        assert_eq!(disk_size_locked(&VmState::Stopped), None);
    }

    #[test]
    fn a_form_opens_on_the_size_the_disk_has() {
        let form = EditVmForm::from_vm(&VmSummary {
            disk_gb: 20,
            ..vm_summary()
        });

        assert_eq!(form.disk_gb, 20);
        assert_eq!(
            form.stored_disk_gb, 20,
            "the size it opened on is the floor the field may not go below"
        );
    }

    #[test]
    fn edit_vm_request_carries_the_disk_size() {
        let request = edit_vm_request(&EditVmForm {
            disk_gb: 40,
            ..edit_form(None)
        })
        .unwrap();

        assert_eq!(request.disk_gb, 40);
    }

    #[test]
    fn edit_vm_request_rejects_odd_ram() {
        let error = edit_vm_request(&EditVmForm {
            name: "dev".into(),
            ram_mb: 513,
            disk_gb: 20,
            stored_disk_gb: 20,
            cpu_cores: 4,
            gpu_mode: GpuMode::Default,
            network_mode: NetworkMode::Nat,
            ssh: None,
            ssh_port: SshPort::DEFAULT.get(),
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

    /// A validated update, as the application layer hands one over.
    fn available_update(version: &str) -> AvailableUpdate {
        AvailableUpdate {
            validated: ValidatedUpdate {
                version: version.parse().expect("the fixture version parses"),
                installer: InstallerAsset {
                    url: format!(
                        "https://github.com/MrUndead1996/vm-lord/releases/download/v{version}/vmlord-setup.exe"
                    ),
                    size: 40 * 1024 * 1024,
                    sha256: "a".repeat(64),
                },
            },
            release_notes: "Fixes the installer".into(),
        }
    }

    /// Nothing has been asked for yet, so the only thing on offer is asking.
    #[test]
    fn an_idle_update_state_offers_a_check() {
        let presentation = update_presentation(&UpdateState::Idle);

        assert_eq!(presentation.action, Some(UpdateOffer::Check));
        assert!(!presentation.cancellable);
        assert_eq!(presentation.percent, None);
    }

    /// A check in flight offers nothing: a second one would be refused anyway.
    #[test]
    fn a_running_check_offers_no_action() {
        let presentation = update_presentation(&UpdateState::Checking);

        assert_eq!(presentation.action, None);
        assert!(!presentation.cancellable);
    }

    /// The version and its notes are what a person decides on, so both are on
    /// screen before the download button they decide with.
    #[test]
    fn an_available_update_offers_a_download_with_its_notes() {
        let presentation = update_presentation(&UpdateState::Available(available_update("1.4.0")));

        assert_eq!(presentation.action, Some(UpdateOffer::Download));
        assert!(
            presentation.status.contains("1.4.0"),
            "{}",
            presentation.status
        );
        assert_eq!(presentation.detail.as_deref(), Some("Fixes the installer"));
    }

    /// A download is the one phase a person can call off, and the one that
    /// publishes counts to draw a bar from.
    #[test]
    fn a_running_download_offers_cancellation_and_progress() {
        let presentation = update_presentation(&UpdateState::Downloading {
            update: available_update("1.4.0"),
            progress: Some(DownloadPhase::Downloading {
                downloaded: 10 * 1024 * 1024,
                total: Some(40 * 1024 * 1024),
            }),
        });

        assert_eq!(presentation.action, None);
        assert!(presentation.cancellable);
        assert_eq!(presentation.percent, Some(25));
        assert!(presentation.detail.is_some());
    }

    /// A verified installer on disk is still not run without being asked to.
    #[test]
    fn a_ready_installer_offers_an_install() {
        let presentation = update_presentation(&UpdateState::Ready {
            update: available_update("1.4.0"),
            installer: PathBuf::from(r"C:\Temp\vmlord-setup.exe"),
            installing: false,
        });

        assert_eq!(presentation.action, Some(UpdateOffer::Install));
        assert!(!presentation.cancellable);
    }

    /// Once Windows has been handed the installer there is nothing left to
    /// press: the application is on its way out.
    #[test]
    fn a_launching_installer_offers_nothing() {
        let presentation = update_presentation(&UpdateState::Ready {
            update: available_update("1.4.0"),
            installer: PathBuf::from(r"C:\Temp\vmlord-setup.exe"),
            installing: true,
        });

        assert_eq!(presentation.action, None);
        assert!(!presentation.cancellable);
    }

    /// A failure says what went wrong and offers the one thing that can follow
    /// it, rather than leaving the section stuck.
    #[test]
    fn a_failed_update_offers_a_retry_and_says_why() {
        let presentation = update_presentation(&UpdateState::Failed {
            message: "the installer hash did not match".into(),
        });

        assert_eq!(presentation.action, Some(UpdateOffer::Retry));
        assert_eq!(
            presentation.detail.as_deref(),
            Some("the installer hash did not match")
        );
    }

    /// The first run opens the settings window filled in from the settings
    /// that were just created, so nothing is offered as blank.
    #[test]
    fn a_first_run_settings_form_carries_the_current_settings() {
        let settings = AppSettings {
            vm_storage_path: PathBuf::from(r"C:\VMLord\VMs"),
            language: Language::EnUs,
            log_directory: PathBuf::from(r"C:\VMLord\Logs"),
            log_level: LogLevel::Info,
            image_cache_path: PathBuf::from(r"C:\VMLord\Images"),
            default_distro: "ubuntu".into(),
            guest_readiness: GuestReadinessTimeouts::default(),
            clipboard_files: FileClipboardSettings::default(),
            display: DisplaySettings::default(),
            last_automatic_update_check: None,
        };

        let form = SettingsForm::first_run(&settings);

        assert!(form.first_run);
        assert_eq!(form.vm_storage_path, r"C:\VMLord\VMs");
    }
}
