//! The privileged half of the guest display services.
//!
//! It owns the DRM device, the VM's secret and the control channel, and hands
//! the unprivileged half read-only dma-bufs and one channel key per socket.

fn main() -> std::process::ExitCode {
    vmlord_display_services::broker_main::run(
        vmlord_display_services::broker_main::Options::from_env(),
    )
}
