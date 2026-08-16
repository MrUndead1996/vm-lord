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

use std::{
    error::Error,
    fs, process,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use vmlord_agent_protocol::{auth::Secret, backoff::Backoff};

mod command;
mod gpu_mountinfo;
mod gpu_mounts;
mod gpu_recipe;
mod gpu_targets;
mod session;
mod vsock;

/// Set by the signal handler when the guest is going down.
///
/// An `AtomicBool` and nothing else, because a signal handler may call almost
/// nothing: the unmounting it leads to happens on the main thread, once the
/// session that was open has ended.
static STOPPING: AtomicBool = AtomicBool::new(false);

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

    listen_for_shutdown();
    serve_host(&secret);

    // A guest that is going down takes its GPU mounts with it: the shares
    // behind them belong to a host that is about to stop serving this VM, and
    // a mount left behind would be a directory that answers `EIO` to whatever
    // reads it next.
    gpu_mounts::detach_all();
}

/// Asks to be told when the guest is shutting this agent down.
///
/// `SIGTERM` is what `systemctl stop` and a guest shutdown send, and `SIGINT`
/// is what a person running the agent by hand sends. Neither is a failure, so
/// both end the loop rather than the process: the unmounting afterwards is the
/// point of noticing them at all.
fn listen_for_shutdown() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let handler = stop as *const () as libc::sighandler_t;
        // SAFETY: `handler` is a plain function pointer with the C signature
        // `signal` expects, and all it does is store into a static, which is
        // the one thing a handler may safely do.
        let previous = unsafe { libc::signal(signal, handler) };
        if previous == libc::SIG_ERR {
            eprintln!(
                "vmlord-agent: signal {signal} could not be handled: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

extern "C" fn stop(_signal: libc::c_int) {
    STOPPING.store(true, Ordering::Relaxed);
}

fn read_secret() -> Result<Secret, Box<dyn Error>> {
    Ok(Secret::from_base64(&fs::read_to_string(
        vmlord_agent_protocol::auth::GUEST_SECRET_PATH,
    )?)?)
}

/// Keeps a session open with the host for as long as this guest runs.
///
/// Every way a connection can end is a connection to open again: the host may
/// not be listening yet, may have been closed, may be restarting, or may be a
/// VMLord too new to speak this agent's protocol -- and all of those are
/// answered by asking again later, so none of them is worth a different code
/// path. The one thing that ends the loop is the guest itself shutting the
/// agent down.
fn serve_host(secret: &Secret) {
    let mut backoff = Backoff::new();
    while !STOPPING.load(Ordering::Relaxed) {
        let authenticated = connect_to_host(secret);
        // A shutdown that arrived during the session must not be waited out:
        // systemd is holding the guest open for this process to exit.
        if STOPPING.load(Ordering::Relaxed) {
            break;
        }
        wait_before_connecting_again(backoff.after(authenticated));
    }
}

/// Waits out the backoff in slices short enough to shut down in.
///
/// `thread::sleep` resumes after a signal rather than returning, so a delay at
/// the cap would keep a guest that is shutting down waiting for half a minute
/// before its mounts are taken away.
fn wait_before_connecting_again(delay: Duration) {
    const SLICE: Duration = Duration::from_millis(250);

    let mut waited = Duration::ZERO;
    while waited < delay && !STOPPING.load(Ordering::Relaxed) {
        let slice = SLICE.min(delay - waited);
        thread::sleep(slice);
        waited += slice;
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
    match session::run(
        &mut stream,
        secret,
        AGENT_VERSION,
        &mut opened,
        gpu_mounts::attach,
    ) {
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
