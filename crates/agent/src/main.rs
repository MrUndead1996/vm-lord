//! The VMLord agent, which runs inside a Linux guest.
//!
//! It is the guest half of the protocol in `vmlord-agent-protocol`: it opens
//! the HvSocket connection to the host, authenticates the session, and does
//! the in-guest work the host asks for -- reporting what the GPU is doing and
//! mounting the shares GPU-PV needs.
//!
//! The transport and opening handshake live in small modules, so the session
//! rules can be tested against bytes without a running Linux VM.

// The agent is built for the guest, never for the host. Saying so here turns
// an accidental `--workspace` build on Windows into a sentence instead of a
// link error about missing syscalls once the transport lands.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "vmlord-agent runs in a Linux guest; build it with \
     `cargo agent`"
);

use std::{error::Error, fs, process};

mod session;
mod vsock;

/// This build of the agent, as reported to the host during its hello.
const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(error) = connect_to_host() {
        eprintln!("vmlord-agent: {error}");
        process::exit(1);
    }
}

fn connect_to_host() -> Result<(), Box<dyn Error>> {
    let secret = vmlord_agent_protocol::auth::Secret::from_base64(&fs::read_to_string(
        vmlord_agent_protocol::auth::GUEST_SECRET_PATH,
    )?)?;
    let mut stream = vsock::connect(vsock::VMADDR_CID_HOST, vsock::AGENT_VSOCK_PORT)?;

    session::run(&mut stream, &secret, AGENT_VERSION, &mut None)?;
    Ok(())
}
