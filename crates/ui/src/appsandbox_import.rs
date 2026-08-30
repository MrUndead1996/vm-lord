//! The dialog that copies a finished AppSandbox Linux VM into VMLord.
//!
//! Two lists in one window, because they are two halves of the same job: the
//! AppSandbox VMs this host has, and the imports that stopped partway and are
//! waiting to be retried or discarded. Nothing here decides anything -- every
//! judgement about whether a VM can be imported, and every path or key it was
//! found through, belongs to the layers below and reaches this one only as the
//! domain values they chose to publish.

use eframe::egui;
use rust_i18n::t;
use vmlord_app::ImportWorkflow;
use vmlord_core::{
    AppSandboxCompatibility, AppSandboxImportProgress, AppSandboxImportRequest,
    AppSandboxImportStage, AppSandboxIncompatibility, AppSandboxSourceId, AppSandboxVmCandidate,
    IncompleteAppSandboxImport, RepositoryError, VmSummary,
};

const FIELD_HEIGHT: f32 = 22.0;
const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The working headroom an import is asked to have beyond the copy itself.
const HEADROOM_GIB: u32 = 10;

/// What the dialog asks the application to do.
pub(crate) enum AppSandboxImportAction {
    Discover,
    Cancel,
    Select(AppSandboxSourceId),
    Submit(AppSandboxImportRequest),
    StopImport(String),
    Retry(String),
    Discard(String),
}

/// The one field this dialog owns, and the last refusal it was given.
///
/// The chosen source and the discovered list live in the application: they
/// survive this window being closed and reopened, and rebuilding them from a
/// dialog would mean walking another application's files on every redraw.
#[derive(Default)]
pub(crate) struct AppSandboxImportForm {
    pub(crate) destination_name: String,
    pub(crate) error: Option<String>,
}

impl AppSandboxImportForm {
    /// The form a freshly chosen candidate fills in.
    ///
    /// The name is offered, not imposed: the source keeps its own name whatever
    /// happens here, so the copy may be called anything, and it has to be
    /// renamed whenever a VM of that name already exists.
    pub(crate) fn from_candidate(candidate: &AppSandboxVmCandidate) -> Self {
        Self {
            destination_name: candidate.name.clone(),
            error: None,
        }
    }

    /// The request this form and `candidate` make, or why they make none.
    pub(crate) fn request(
        &self,
        candidate: &AppSandboxVmCandidate,
    ) -> Result<AppSandboxImportRequest, RepositoryError> {
        let request = AppSandboxImportRequest {
            source_id: candidate.source_id.clone(),
            destination_name: self.destination_name.trim().to_owned(),
        };
        request.validate()?;
        Ok(request)
    }
}

/// Why a discovered VM cannot be imported, in the words of the catalogue.
pub(crate) fn compatibility_reason(compatibility: &AppSandboxCompatibility) -> String {
    match compatibility {
        AppSandboxCompatibility::Compatible => t!("appsandbox_import.compatible").to_string(),
        AppSandboxCompatibility::Incompatible(reasons) => reasons
            .iter()
            .map(incompatibility_reason)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

const fn incompatibility_key(reason: &AppSandboxIncompatibility) -> &'static str {
    match reason {
        AppSandboxIncompatibility::NotLinux => "appsandbox_import.reason_not_linux",
        AppSandboxIncompatibility::Template => "appsandbox_import.reason_template",
        AppSandboxIncompatibility::InstallationIncomplete => {
            "appsandbox_import.reason_installation_incomplete"
        }
        AppSandboxIncompatibility::Running => "appsandbox_import.reason_running",
        AppSandboxIncompatibility::SshDisabled => "appsandbox_import.reason_ssh_disabled",
        AppSandboxIncompatibility::SshKeyNotDeployed => "appsandbox_import.reason_ssh_no_key",
        AppSandboxIncompatibility::SourceDiskMissing => "appsandbox_import.reason_disk_missing",
        AppSandboxIncompatibility::SourceDiskMismatch => "appsandbox_import.reason_disk_mismatch",
        AppSandboxIncompatibility::UnsupportedNetworkMode => "appsandbox_import.reason_network",
        AppSandboxIncompatibility::UnsupportedGpuMode => "appsandbox_import.reason_gpu",
        AppSandboxIncompatibility::InvalidSshPort => "appsandbox_import.reason_ssh_port",
        AppSandboxIncompatibility::DuplicateSource => "appsandbox_import.reason_duplicate",
    }
}

fn incompatibility_reason(reason: &AppSandboxIncompatibility) -> String {
    t!(incompatibility_key(reason)).to_string()
}

/// What an import is doing, in the words of the catalogue.
pub(crate) const fn stage_key(stage: AppSandboxImportStage) -> &'static str {
    match stage {
        AppSandboxImportStage::Validating => "appsandbox_import.stage_validating",
        AppSandboxImportStage::Copying => "appsandbox_import.stage_copying",
        AppSandboxImportStage::Creating => "appsandbox_import.stage_creating",
        AppSandboxImportStage::BootstrapStarting => "appsandbox_import.stage_bootstrap_starting",
        AppSandboxImportStage::Converting => "appsandbox_import.stage_converting",
        AppSandboxImportStage::Restarting => "appsandbox_import.stage_restarting",
        AppSandboxImportStage::Verifying => "appsandbox_import.stage_verifying",
        AppSandboxImportStage::NeedsAttention => "appsandbox_import.stage_needs_attention",
        AppSandboxImportStage::Complete => "appsandbox_import.stage_complete",
    }
}

pub(crate) fn stage_label(stage: AppSandboxImportStage) -> String {
    t!(stage_key(stage)).to_string()
}

/// How far the copy has got, when the copy is what is happening.
///
/// `None` at every other stage on purpose: a stale byte count shown beside a
/// later stage reads as a copy that is still running.
pub(crate) fn copy_progress(progress: AppSandboxImportProgress) -> Option<String> {
    if progress.stage != AppSandboxImportStage::Copying {
        return None;
    }
    let total = progress.total_bytes?;
    Some(
        t!(
            "appsandbox_import.copied",
            copied = format!("{:.1}", progress.copied_bytes as f64 / BYTES_PER_GIB),
            total = format!("{:.1}", total as f64 / BYTES_PER_GIB)
        )
        .to_string(),
    )
}

/// Whether the name typed into the form may be submitted.
///
/// The backend refuses a duplicate too, and would be right to. Refusing it here
/// as well is what lets the person see it before they press the button, in the
/// window where the fix is one keystroke.
pub(crate) fn name_conflict(name: &str, existing: &[VmSummary]) -> bool {
    let name = name.trim();
    !name.is_empty() && existing.iter().any(|vm| vm.name.eq_ignore_ascii_case(name))
}

/// What stopping an import costs, which depends on how far it got.
///
/// Before the copied guest is changed, stopping removes everything the import
/// made. After it, the copy is kept for an explicit retry or discard, because
/// a guest that was half converted is not one to silently throw away.
pub(crate) const fn cancel_key(stage: AppSandboxImportStage) -> &'static str {
    match stage {
        AppSandboxImportStage::Validating
        | AppSandboxImportStage::Copying
        | AppSandboxImportStage::Creating
        | AppSandboxImportStage::BootstrapStarting => "appsandbox_import.cancel_rolls_back",
        AppSandboxImportStage::Converting
        | AppSandboxImportStage::Restarting
        | AppSandboxImportStage::Verifying
        | AppSandboxImportStage::NeedsAttention
        | AppSandboxImportStage::Complete => "appsandbox_import.cancel_keeps_copy",
    }
}

/// The window.
pub(crate) fn render(
    context: &egui::Context,
    form: &mut AppSandboxImportForm,
    workflow: &ImportWorkflow,
    existing_vms: &[VmSummary],
    running: &[(&str, AppSandboxImportProgress)],
) -> Option<AppSandboxImportAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(t!("appsandbox_import.title").to_string())
        .collapsible(false)
        .resizable(false)
        .default_width(640.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(t!("appsandbox_import.description").to_string());
            ui.add_space(4.0);
            // Said before anything is chosen, because it is the promise the
            // whole feature rests on.
            ui.small(t!("appsandbox_import.source_untouched").to_string());
            ui.add_space(8.0);

            if ui
                .button(t!("appsandbox_import.discover").to_string())
                .clicked()
            {
                action = Some(AppSandboxImportAction::Discover);
            }
            ui.add_space(8.0);

            if let Some(chosen) = render_candidates(ui, workflow) {
                action = Some(chosen);
            }

            if let Some(candidate) = workflow.selected() {
                ui.add_space(8.0);
                ui.separator();
                if let Some(chosen) = render_form(ui, form, candidate, existing_vms) {
                    action = Some(chosen);
                }
            }

            if !running.is_empty() {
                ui.add_space(8.0);
                ui.separator();
                if let Some(chosen) = render_running(ui, running) {
                    action = Some(chosen);
                }
            }

            if !workflow.incomplete().is_empty() {
                ui.add_space(8.0);
                ui.separator();
                if let Some(chosen) = render_incomplete(ui, workflow.incomplete()) {
                    action = Some(chosen);
                }
            }

            if let Some(error) = &form.error {
                ui.add_space(6.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });

    if !open {
        return Some(AppSandboxImportAction::Cancel);
    }
    action
}

fn render_candidates(
    ui: &mut egui::Ui,
    workflow: &ImportWorkflow,
) -> Option<AppSandboxImportAction> {
    let mut action = None;
    if workflow.candidates().is_empty() {
        ui.small(t!("appsandbox_import.no_candidates").to_string());
        return None;
    }
    egui::ScrollArea::vertical()
        .id_salt("appsandbox-candidates")
        .max_height(200.0)
        .show(ui, |ui| {
            egui::Grid::new("appsandbox-candidate-grid")
                .num_columns(4)
                .striped(true)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.strong(t!("appsandbox_import.column_name").to_string());
                    ui.strong(t!("appsandbox_import.column_resources").to_string());
                    ui.strong(t!("appsandbox_import.column_state").to_string());
                    ui.label("");
                    ui.end_row();

                    for candidate in workflow.candidates() {
                        let chosen = workflow
                            .selected()
                            .is_some_and(|selected| selected.source_id == candidate.source_id);
                        if ui.selectable_label(chosen, &candidate.name).clicked() {
                            // Selectable whatever its compatibility: a VM that
                            // cannot be imported is still one whose reason the
                            // person came here to read.
                            action =
                                Some(AppSandboxImportAction::Select(candidate.source_id.clone()));
                        }
                        ui.label(t!(
                            "appsandbox_import.resources",
                            ram = candidate.ram_mb,
                            cores = candidate.cpu_cores,
                            disk = candidate.disk_gb
                        ));
                        let reason = compatibility_reason(&candidate.compatibility);
                        if candidate.compatibility == AppSandboxCompatibility::Compatible {
                            ui.label(reason);
                        } else {
                            ui.colored_label(egui::Color32::LIGHT_RED, reason);
                        }
                        ui.label("");
                        ui.end_row();
                    }
                });
        });
    action
}

fn render_form(
    ui: &mut egui::Ui,
    form: &mut AppSandboxImportForm,
    candidate: &AppSandboxVmCandidate,
    existing_vms: &[VmSummary],
) -> Option<AppSandboxImportAction> {
    let mut action = None;
    let importable = candidate.compatibility == AppSandboxCompatibility::Compatible;

    ui.add_space(6.0);
    if !importable {
        ui.colored_label(
            egui::Color32::LIGHT_RED,
            compatibility_reason(&candidate.compatibility),
        );
        return None;
    }

    egui::Grid::new("appsandbox-import-form")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label(t!("appsandbox_import.destination_name").to_string());
            ui.add_sized(
                [260.0, FIELD_HEIGHT],
                egui::TextEdit::singleline(&mut form.destination_name),
            );
            ui.end_row();
        });

    let conflict = name_conflict(&form.destination_name, existing_vms);
    if conflict {
        ui.colored_label(
            egui::Color32::LIGHT_RED,
            t!("appsandbox_import.name_taken").to_string(),
        );
    }
    ui.add_space(4.0);
    ui.small(
        t!(
            "appsandbox_import.capacity",
            disk = candidate.disk_gb,
            headroom = HEADROOM_GIB
        )
        .to_string(),
    );

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let submit = ui.add_enabled(
            !conflict && !form.destination_name.trim().is_empty(),
            egui::Button::new(t!("appsandbox_import.submit").to_string()),
        );
        if submit.clicked() {
            match form.request(candidate) {
                Ok(request) => action = Some(AppSandboxImportAction::Submit(request)),
                Err(error) => form.error = Some(error.to_string()),
            }
        }
        if ui.button(t!("common.cancel").to_string()).clicked() {
            action = Some(AppSandboxImportAction::Cancel);
        }
    });
    action
}

fn render_running(
    ui: &mut egui::Ui,
    running: &[(&str, AppSandboxImportProgress)],
) -> Option<AppSandboxImportAction> {
    let mut action = None;
    ui.strong(t!("appsandbox_import.in_flight").to_string());
    ui.add_space(4.0);
    egui::Grid::new("appsandbox-running-grid")
        .num_columns(4)
        .striped(true)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (name, progress) in running {
                ui.label(*name);
                ui.label(stage_label(progress.stage));
                // Blank at every stage but the copy, rather than a stale count
                // that would read as a copy still running.
                ui.label(copy_progress(*progress).unwrap_or_default());
                let stop = ui.button(t!("appsandbox_import.stop").to_string());
                // What stopping costs is not the same at every stage, and the
                // person pressing it is entitled to know which it is now.
                stop.clone()
                    .on_hover_text(t!(cancel_key(progress.stage)).to_string());
                if stop.clicked() {
                    action = Some(AppSandboxImportAction::StopImport((*name).to_owned()));
                }
                ui.end_row();
            }
        });
    action
}

fn render_incomplete(
    ui: &mut egui::Ui,
    incomplete: &[IncompleteAppSandboxImport],
) -> Option<AppSandboxImportAction> {
    let mut action = None;
    ui.strong(t!("appsandbox_import.unfinished").to_string());
    ui.small(t!("appsandbox_import.unfinished_note").to_string());
    ui.add_space(4.0);
    egui::Grid::new("appsandbox-incomplete-grid")
        .num_columns(3)
        .striped(true)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for import in incomplete {
                ui.label(&import.destination_name);
                ui.label(stage_label(import.stage));
                ui.horizontal(|ui| {
                    if ui
                        .button(t!("appsandbox_import.retry").to_string())
                        .clicked()
                    {
                        action = Some(AppSandboxImportAction::Retry(
                            import.destination_name.clone(),
                        ));
                    }
                    if ui
                        .button(t!("appsandbox_import.discard").to_string())
                        .clicked()
                    {
                        action = Some(AppSandboxImportAction::Discard(
                            import.destination_name.clone(),
                        ));
                    }
                });
                ui.end_row();
            }
        });
    action
}

#[cfg(test)]
mod tests {
    use vmlord_core::{
        AppSandboxCompatibility, AppSandboxImportProgress, AppSandboxImportStage,
        AppSandboxIncompatibility, AppSandboxSourceId, AppSandboxVmCandidate, GpuMode, NetworkMode,
        SshAvailability, VmDisplayFacts, VmGpuFacts, VmState, VmSummary,
    };

    use super::{
        AppSandboxImportForm, cancel_key, compatibility_reason, copy_progress, name_conflict,
        stage_label,
    };

    fn candidate(name: &str, compatibility: AppSandboxCompatibility) -> AppSandboxVmCandidate {
        AppSandboxVmCandidate {
            source_id: AppSandboxSourceId::from_stable_hash(format!("source-{name}")).unwrap(),
            name: name.to_owned(),
            ram_mb: 4096,
            disk_gb: 80,
            cpu_cores: 4,
            network_mode: NetworkMode::Nat,
            gpu_mode: GpuMode::Default,
            ssh_user: "sandbox".into(),
            ssh_port: 22,
            compatibility,
        }
    }

    fn vm(name: &str) -> VmSummary {
        VmSummary {
            name: name.to_owned(),
            os_type: "Linux".into(),
            state: VmState::Stopped,
            ram_mb: 4096,
            disk_gb: 80,
            cpu_cores: 4,
            gpu_mode: GpuMode::None,
            gpu: VmGpuFacts::default(),
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display: VmDisplayFacts::default(),
            network_mode: NetworkMode::Nat,
            ip_address: None,
            ssh: SshAvailability::Disabled,
        }
    }

    #[test]
    fn selected_candidate_prefills_but_does_not_lock_the_name() {
        let candidate = candidate("ubuntu", AppSandboxCompatibility::Compatible);
        let mut form = AppSandboxImportForm::from_candidate(&candidate);
        assert_eq!(form.destination_name, "ubuntu");

        form.destination_name = "ubuntu-copy".into();

        assert_eq!(
            form.request(&candidate).unwrap().destination_name,
            "ubuntu-copy"
        );
    }

    #[test]
    fn a_name_that_is_only_spaces_makes_no_request() {
        let candidate = candidate("ubuntu", AppSandboxCompatibility::Compatible);
        let mut form = AppSandboxImportForm::from_candidate(&candidate);

        form.destination_name = "   ".into();

        assert!(form.request(&candidate).is_err());
    }

    #[test]
    fn a_name_something_else_holds_is_refused_before_the_button_is_pressed() {
        let existing = [vm("ubuntu-copy")];

        assert!(name_conflict("ubuntu-copy", &existing));
        assert!(
            name_conflict("UBUNTU-COPY", &existing),
            "Windows VM directories do not differ by case"
        );
        assert!(!name_conflict("ubuntu-copy-2", &existing));
        assert!(!name_conflict("  ", &existing));
    }

    #[test]
    fn an_incompatible_candidate_shows_every_reason_it_was_refused_for() {
        let reason = compatibility_reason(&AppSandboxCompatibility::Incompatible(vec![
            AppSandboxIncompatibility::Running,
            AppSandboxIncompatibility::SshDisabled,
        ]));

        assert!(reason.contains("running"), "{reason}");
        assert!(reason.to_ascii_lowercase().contains("ssh"), "{reason}");
    }

    #[test]
    fn a_compatible_candidate_says_so_rather_than_saying_nothing() {
        let reason = compatibility_reason(&AppSandboxCompatibility::Compatible);

        assert!(!reason.is_empty());
    }

    #[test]
    fn every_stage_has_a_label_of_its_own() {
        let labels: Vec<String> = [
            AppSandboxImportStage::Validating,
            AppSandboxImportStage::Copying,
            AppSandboxImportStage::Creating,
            AppSandboxImportStage::BootstrapStarting,
            AppSandboxImportStage::Converting,
            AppSandboxImportStage::Restarting,
            AppSandboxImportStage::Verifying,
            AppSandboxImportStage::NeedsAttention,
            AppSandboxImportStage::Complete,
        ]
        .into_iter()
        .map(stage_label)
        .collect();

        assert!(labels.iter().all(|label| !label.is_empty()));
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    #[test]
    fn bytes_are_shown_while_copying_and_at_no_other_stage() {
        let copying = copy_progress(AppSandboxImportProgress {
            stage: AppSandboxImportStage::Copying,
            copied_bytes: 10 * 1024 * 1024 * 1024,
            total_bytes: Some(80 * 1024 * 1024 * 1024),
        })
        .expect("a copy in progress has bytes to show");

        assert!(copying.contains("10.0"), "{copying}");
        assert!(copying.contains("80.0"), "{copying}");
        assert!(
            copy_progress(AppSandboxImportProgress {
                stage: AppSandboxImportStage::Converting,
                copied_bytes: 10,
                total_bytes: Some(80),
            })
            .is_none(),
            "a stale byte count beside a later stage reads as a copy still running"
        );
    }

    #[test]
    fn what_cancelling_costs_depends_on_whether_the_guest_was_changed() {
        assert_eq!(
            cancel_key(AppSandboxImportStage::Copying),
            "appsandbox_import.cancel_rolls_back"
        );
        assert_eq!(
            cancel_key(AppSandboxImportStage::Converting),
            "appsandbox_import.cancel_keeps_copy",
            "once the copied guest may have been changed, its copy is kept"
        );
    }
}
