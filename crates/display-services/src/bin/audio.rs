//! The guest's audio daemon: one loopback's sound, and no more.

fn main() -> std::process::ExitCode {
    vmlord_display_services::audio_main::run(
        vmlord_display_services::audio_main::Options::from_env(),
    )
}
