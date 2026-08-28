#[cfg(not(windows))]
compile_error!("VMLord currently supports Windows only");

use std::path::PathBuf;

use vmlord_core::{AppSettings, BuildMonitor, BuildStep, VmRepository};

mod pickers;

fn main() {
    let settings = vmlord_core::SettingsStore::for_current_user().and_then(|store| {
        store
            .load_or_create_with_status()
            .map(|settings_load| (store, settings_load))
    });
    let mut diagnostics = None;
    let repository = match &settings {
        Ok((_, settings_load)) => {
            match vmlord_core::initialize_with_diagnostics(&settings_load.settings) {
                Ok(sink) => {
                    diagnostics = Some(sink);
                    tracing::info!(
                        "logging initialized at {} with {:?} level",
                        settings_load.settings.log_file_path.display(),
                        settings_load.settings.log_level
                    );
                    load_backend(&settings_load.settings)
                }
                Err(error) => {
                    eprintln!("failed to initialize logging: {error}");
                    vmlord_app::unavailable_repository(format!(
                        "failed to initialize logging: {error}"
                    ))
                }
            }
        }
        Err(error) => {
            eprintln!("failed to initialize settings: {error}");
            vmlord_app::unavailable_repository(format!("failed to initialize settings: {error}"))
        }
    };
    let distro_catalog = settings.as_ref().ok().and_then(|(store, settings_load)| {
        let bundle = match std::env::current_exe()
            .ok()
            .and_then(|executable| executable.parent().map(|parent| parent.join("distros")))
        {
            Some(bundle) => bundle,
            None => {
                eprintln!("failed to locate installed distribution profiles");
                vmlord_core::diagnostic!(
                    Error,
                    vmlord_core::Subsystem::App,
                    "Failed to locate installed distribution profiles"
                );
                return None;
            }
        };
        if let Err(error) = vmlord_core::sync_bundled_profiles(&bundle, store) {
            eprintln!("failed to synchronize distribution profiles: {error}");
            vmlord_core::diagnostic!(
                Error,
                vmlord_core::Subsystem::App,
                "Failed to synchronize distribution profiles: {error}"
            );
            return None;
        }
        match vmlord_core::DistroCatalog::load(store) {
            Ok(catalog) => {
                if let Err(error) = catalog.select(&settings_load.settings.default_distro) {
                    eprintln!("failed to load distribution profiles: {error}");
                    vmlord_core::diagnostic!(
                        Error,
                        vmlord_core::Subsystem::App,
                        "Failed to load distribution profiles: {error}"
                    );
                }
                Some(catalog)
            }
            Err(error) => {
                eprintln!("failed to load distribution profiles: {error}");
                vmlord_core::diagnostic!(
                    Error,
                    vmlord_core::Subsystem::App,
                    "Failed to load distribution profiles: {error}"
                );
                None
            }
        }
    });
    let mut application = vmlord_app::WorkspaceApp::new(repository)
        .with_guest_defaults(vmlord_platform::host_guest_defaults())
        .with_image_picker(Box::new(pickers::WindowsImagePicker::new()))
        .with_settings_path_picker(Box::new(pickers::WindowsSettingsPathPicker::new()));
    if let Some(catalog) = distro_catalog {
        application = application.with_distro_catalog(catalog);
    }
    // Without this the panel stays empty: the layer records, and nobody reads.
    if let Some(sink) = diagnostics {
        application = application.with_diagnostics(sink);
    }
    if let Ok((store, settings_load)) = settings {
        let _created = settings_load.created;
        application = application.with_settings(store, settings_load.settings);
    }
    application.start();
    if let Err(error) = vmlord_ui::run(application) {
        panic!("failed to run VMLord UI: {error}");
    }
}

fn load_backend(settings: &AppSettings) -> Box<dyn VmRepository> {
    tracing::info!(
        "using the native HCS backend with VM storage at {}",
        settings.vm_storage_path.display()
    );
    Box::new(
        vmlord_platform::HcsVmRepository::new(
            settings.vm_storage_path.clone(),
            cloud_disk_importer(settings.image_cache_path.clone()),
        )
        .with_readiness_timeouts(settings.guest_readiness)
        .with_file_clipboard_settings(settings.clipboard_files)
        .with_display_settings(settings.display),
    )
}

/// Joins the two halves of getting a cloud image onto a VM's disk: fetching it,
/// which knows nothing of Windows and lives in `vmlord-image`, and writing it
/// into a VHDX, which is `vmlord-platform`'s business.
///
/// The composition root is where they meet, which is what keeps the network out
/// of the Windows layer.
///
/// Both halves are long enough to report and to be cancelled, and both are
/// invisible from outside this closure, so the steps are reported here.
fn cloud_disk_importer(cache_directory: PathBuf) -> vmlord_platform::CloudDiskImporter {
    Box::new(
        move |image, disk_size_bytes, target, monitor: &BuildMonitor| {
            monitor.report(BuildStep::Downloading);
            let mut source = vmlord_image::open_cloud_image(
                &image.profile,
                &image.release,
                &cache_directory,
                disk_size_bytes,
                monitor.downloads(),
                monitor.cancel_flag(),
            )?;
            monitor.report(BuildStep::WritingDisk);
            vmlord_platform::import_image(
                &mut source,
                target,
                disk_size_bytes,
                monitor.cancel_flag(),
            )
            .map(|_summary| ())
        },
    )
}
