//! The unprivileged half of the guest display services.
//!
//! It captures, encodes and speaks the frame and input channels, and holds
//! nothing worth stealing: read-only mappings and one channel key per socket,
//! good for one session and no longer.

fn main() -> std::process::ExitCode {
    vmlord_display_services::session_main::run(
        vmlord_display_services::session_main::Options::from_env(),
    )
}
