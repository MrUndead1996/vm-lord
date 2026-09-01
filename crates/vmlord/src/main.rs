#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("VMLord currently supports Windows only");

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use semver::Version;
use vmlord_core::{
    AppSettings, BuildMonitor, BuildStep, DownloadPhase, ProgressPublisher, VmRepository,
};

mod pickers;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("adopt-disk") {
        match adopt_disk(std::env::args().skip(2)) {
            Ok(document) => println!("{}", document.display()),
            Err(error) => {
                eprintln!("vmlord: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

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
        application = application
            .with_settings(store, settings_load.settings)
            .with_first_run(settings_load.created)
            .with_update_runtime(Arc::new(WindowsUpdateRuntime::new()));
    }
    application.start();
    if let Err(error) = vmlord_ui::run(application) {
        panic!("failed to run VMLord UI: {error}");
    }
}

/// The composition-root implementation that joins portable release retrieval
/// and the Windows-only verified-installer launcher.
struct WindowsUpdateRuntime {
    current_version: Version,
}

impl WindowsUpdateRuntime {
    fn new() -> Self {
        Self {
            current_version: Version::parse(env!("CARGO_PKG_VERSION"))
                .expect("the package version is valid semantic versioning"),
        }
    }
}

impl vmlord_app::UpdateRuntime for WindowsUpdateRuntime {
    fn check(&self) -> Result<Option<vmlord_app::AvailableUpdate>, String> {
        let release = vmlord_image::fetch_latest_release().map_err(|error| error.to_string())?;
        let validated = release
            .manifest
            .validate(&self.current_version)
            .map_err(|error| error.to_string())?;
        Ok(validated.map(|validated| vmlord_app::AvailableUpdate {
            validated,
            release_notes: release.release_notes,
        }))
    }

    fn download(
        &self,
        update: &vmlord_app::AvailableUpdate,
        progress: ProgressPublisher<DownloadPhase>,
        cancel: Arc<AtomicBool>,
    ) -> Result<PathBuf, String> {
        let directory = std::env::temp_dir().join("VMLord").join("updates");
        vmlord_image::fetch_update_installer(
            &update.validated,
            &directory,
            &progress,
            cancel.as_ref(),
        )
        .map_err(|error| error.to_string())
    }

    fn launch(&self, installer: &Path) -> Result<(), String> {
        vmlord_platform::launch_installer(&vmlord_platform::InstallerLaunch::new(
            installer.to_path_buf(),
        ))
        .map_err(|error| error.to_string())
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

/// Builds a VM around a disk that already holds a system, and reports where
/// the document the offline conversion consumes was written.
///
/// A subcommand rather than a screen: adopting a disk is the second step of an
/// import whose first is a copy made outside VMLord and whose third is the
/// offline conversion run under WSL. When the import ships as a feature it
/// gets a screen; until then this is the seam that does not pretend the flow
/// is finished.
///
/// A release build has no console of its own (`windows_subsystem = "windows"`),
/// so run this from a console that already exists, or read the document where
/// it always is: `<VM storage>\\<name>\\import-input.json`.
fn adopt_disk(arguments: impl Iterator<Item = String>) -> Result<PathBuf, String> {
    let arguments = vmlord_platform::AdoptArguments::parse(arguments)?;
    let store =
        vmlord_core::SettingsStore::for_current_user().map_err(|error| error.to_string())?;
    let settings = store
        .load_or_create_with_status()
        .map_err(|error| error.to_string())?
        .settings;
    let _ = vmlord_core::initialize_with_diagnostics(&settings);

    // The distribution profile comes from the installed catalog, which is
    // where every other VM's comes from: an adopted guest runs a release
    // VMLord ships a profile for, and that profile decides which drop-ins move
    // its SSH port and which payloads its display and GPU are built from.
    let bundle = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or("the VMLord executable has no directory")?
        .join("distros");
    vmlord_core::sync_bundled_profiles(&bundle, &store).map_err(|error| error.to_string())?;
    let profile = vmlord_core::DistroCatalog::load(&store)
        .map_err(|error| error.to_string())?
        .select(&settings.default_distro)
        .map_err(|error| error.to_string())?
        .clone();

    let mut repository = vmlord_platform::HcsVmRepository::new(
        settings.vm_storage_path.clone(),
        cloud_disk_importer(settings.image_cache_path.clone()),
    );
    repository.initialize().map_err(|error| error.to_string())?;
    repository
        .adopt_disk(arguments.request(profile))
        .map_err(|error| error.to_string())
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
