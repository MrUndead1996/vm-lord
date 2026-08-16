//! The VMLord agent, which runs inside a Linux guest.
//!
//! It is the guest half of the protocol in `vmlord-agent-protocol`: it opens
//! the HvSocket connection to the host, authenticates the session, and does
//! the in-guest work the host asks for -- reporting what the GPU is doing and
//! mounting the shares GPU-PV needs.
//!
//! The transport and opening handshake live in small modules, so the session
//! rules can be tested against bytes without a running Linux VM.
//!
//! The agent outlives any one connection. A VM boots before VMLord is
//! listening, VMLord restarts, a host hangs up mid-session: each is a
//! connection to open again rather than a reason to stop, so this program
//! connects in a loop for as long as the VM runs. The unit's `Restart=always`
//! is what recovers from a crash; the loop below is what recovers from a host
//! that is not there, and it is the agent's rather than systemd's because a
//! fixed `RestartSec` cannot back off and a host that hung up is not a failure
//! worth reporting to `systemctl`.

// The agent is built for the guest, never for the host. Saying so here turns
// an accidental `--workspace` build on Windows into a sentence instead of a
// link error about missing syscalls once the transport lands.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "vmlord-agent runs in a Linux guest; build it with \
     `cargo agent`"
);

use std::{error::Error, fs, process, thread};

use vmlord_agent_protocol::{auth::Secret, backoff::Backoff};

mod session;
mod vsock;

/// This build of the agent, as reported to the host during its hello.
const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let secret = match read_secret() {
        Ok(secret) => secret,
        // The one failure that ends the agent: a VM's secret is minted when the
        // VM is created and never rotated, so nothing that happens while this
        // guest runs can turn an unreadable one into a usable one.
        Err(error) => {
            eprintln!("vmlord-agent: {error}");
            process::exit(1);
        }
    };

    serve_host(&secret);
}

fn read_secret() -> Result<Secret, Box<dyn Error>> {
    Ok(Secret::from_base64(&fs::read_to_string(
        vmlord_agent_protocol::auth::GUEST_SECRET_PATH,
    )?)?)
}

/// Keeps a session open with the host for as long as this guest runs.
///
/// Never returns. Every way a connection can end is a connection to open
/// again: the host may not be listening yet, may have been closed, may be
/// restarting, or may be a VMLord too new to speak this agent's protocol -- and
/// all of those are answered by asking again later, so none of them is worth a
/// different code path.
fn serve_host(secret: &Secret) -> ! {
    let mut backoff = Backoff::new();
    loop {
        let authenticated = connect_to_host(secret);
        thread::sleep(backoff.after(authenticated));
    }
}

/// Runs one connection, from the socket to the end of its session.
///
/// Returns whether the session got as far as answering the host's challenge,
/// which is what says the host is there and is the only thing the backoff
/// starts over for.
fn connect_to_host(secret: &Secret) -> bool {
    let mut stream = match vsock::connect(vsock::VMADDR_CID_HOST, vsock::AGENT_VSOCK_PORT) {
        Ok(stream) => stream,
        Err(error) => {
            // Ordinary while a VM boots ahead of VMLord, and ordinary while
            // VMLord is closed, so it is said once per attempt and the attempts
            // themselves are what the backoff thins out.
            eprintln!("vmlord-agent: the host is not accepting connections: {error}");
            return false;
        }
    };

    let mut opened = None;
    match session::run(&mut stream, secret, AGENT_VERSION, &mut opened) {
        Ok(()) => eprintln!("vmlord-agent: the host closed the session"),
        Err(error) => eprintln!("vmlord-agent: {error}"),
    }

    match &opened {
        Some(session) => {
            eprintln!(
                "vmlord-agent: that session ran on protocol {}.{} with {} agreed capability(ies)",
                session.version.major,
                session.version.minor,
                session.capabilities.len()
            );
            true
        }
        None => false,
    }
}
