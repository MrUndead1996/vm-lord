//! The guest's clipboard daemon: one graphical session's, and no more.

fn main() -> std::process::ExitCode {
    vmlord_display_services::clipboard_main::run(
        vmlord_display_services::clipboard_main::Options::from_env(),
    )
}
