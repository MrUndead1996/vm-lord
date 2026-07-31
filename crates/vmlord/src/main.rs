#[cfg(not(windows))]
compile_error!("VMLord currently supports Windows only");

fn main() {
    let settings = vmlord_core::SettingsStore::for_current_user()
        .and_then(|store| store.load_or_create().map(|settings| (store, settings)));
    let repository = match &settings {
        Ok((_, settings)) => match vmlord_core::initialize_logging(settings) {
            Ok(()) => {
                log::info!(
                    "logging initialized at {} with {:?} level",
                    settings.log_file_path.display(),
                    settings.log_level
                );
                match vmlord_legacy_backend::AppSandboxBackend::load_from_executable_dir() {
                    Ok(backend) => Box::new(backend),
                    Err(error) => {
                        log::error!("failed to load the legacy backend: {error}");
                        vmlord_app::unavailable_repository(error.to_string())
                    }
                }
            }
            Err(error) => {
                eprintln!("failed to initialize logging: {error}");
                vmlord_app::unavailable_repository(format!("failed to initialize logging: {error}"))
            }
        },
        Err(error) => {
            eprintln!("failed to initialize settings: {error}");
            vmlord_app::unavailable_repository(format!("failed to initialize settings: {error}"))
        }
    };
    let mut application = vmlord_app::WorkspaceApp::new(repository)
        .with_image_picker(Box::new(vmlord_legacy_backend::WindowsImagePicker::new()))
        .with_settings_path_picker(Box::new(
            vmlord_legacy_backend::WindowsSettingsPathPicker::new(),
        ));
    if let Ok((store, settings)) = settings {
        application = application.with_settings(store, settings);
    }
    application.start();
    if let Err(error) = vmlord_ui::run(application) {
        panic!("failed to run VMLord UI: {error}");
    }
}
