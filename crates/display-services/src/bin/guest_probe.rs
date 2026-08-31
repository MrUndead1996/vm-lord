//! What a real dma-buf's coherency call costs, measured against the desktop
//! that is on screen. A guest program: there is no dma-buf anywhere else.

fn main() -> std::process::ExitCode {
    match vmlord_display_services::guest_probe::run(std::env::args().skip(1)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vmlord-display-guest-probe: {error}");

            std::process::ExitCode::FAILURE
        }
    }
}
