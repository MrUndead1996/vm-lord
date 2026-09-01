//! The process a terminal hosts for an interactive SSH session.
//!
//! Nothing decides anything here: the process exists so that a terminal window
//! has something to host, and everything it does lives in `vmlord-platform`.

#[cfg(not(windows))]
compile_error!("vmlord-ssh currently supports Windows only");

fn main() {
    if let Err(error) = run() {
        eprintln!("VMLord SSH session failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Settings are loaded for the application log alone: what the session says
    // belongs to the person in this window, and what the helper did belongs in
    // the application log beside everything else VMLord did.
    let settings = vmlord_core::SettingsStore::for_current_user()?.load_or_create()?;
    vmlord_core::initialize_logging(&settings)?;
    let options = vmlord_platform::parse_ssh_helper_args(std::env::args_os().skip(1))?;
    vmlord_platform::run_ssh_helper(options)?;
    Ok(())
}
