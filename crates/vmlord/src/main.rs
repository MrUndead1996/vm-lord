#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("VMLord currently supports Windows only");

fn main() {
    let repository = match vmlord_legacy_backend::AppSandboxBackend::load_from_executable_dir() {
        Ok(backend) => Box::new(backend),
        Err(error) => vmlord_app::unavailable_repository(error.to_string()),
    };
    let mut application = vmlord_app::WorkspaceApp::new(repository)
        .with_image_picker(Box::new(vmlord_legacy_backend::WindowsImagePicker::new()));
    application.start();
    if let Err(error) = vmlord_ui::run(application) {
        panic!("failed to run VMLord UI: {error}");
    }
}
