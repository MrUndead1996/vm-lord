//! What a captured frame costs between the mapping and the socket.
//!
//! The codec has `cargo display-bench`; this is the layer above it, where the
//! copies a frame makes on its way to a record live. Neither gates anything:
//! both print a table of the machine they ran on.

fn main() -> std::process::ExitCode {
    match vmlord_display_services::pipeline_bench::run(std::env::args().skip(1)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vmlord-display-pipeline-bench: {error}");

            std::process::ExitCode::FAILURE
        }
    }
}
