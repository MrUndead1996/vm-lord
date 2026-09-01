//! The guest's tray icon: the logged-in session's controls for the viewer.

fn main() -> std::process::ExitCode {
    vmlord_display_services::tray_main::run(vmlord_display_services::tray_main::Options::from_env())
}
