//! The console VMLord opens on a VM's first serial port.
//!
//! Nothing decides anything here: the process exists so that a terminal window
//! has something to host, and everything it does lives in `vmlord-platform`.

#[cfg(not(windows))]
compile_error!("vmlord-com1 currently supports Windows only");

fn main() {
    if let Err(error) = run() {
        eprintln!("VMLord COM1 reader failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Settings are loaded for one reason: the application log. Guest bytes
    // never travel through `log` -- they are written to `com1.log` and to this
    // window unchanged -- but what the reader itself does belongs in the
    // application log beside everything else VMLord did.
    let settings = vmlord_core::SettingsStore::for_current_user()?.load_or_create()?;
    vmlord_core::initialize_logging(&settings)?;
    let options = vmlord_platform::parse_com1_helper_args(std::env::args_os().skip(1))?;
    vmlord_platform::run_com1_helper(options)?;
    Ok(())
}
