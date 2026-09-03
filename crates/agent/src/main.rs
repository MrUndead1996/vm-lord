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
    fs,
    os::fd::RawFd,
    process,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
    thread,
    time::Duration,
};

use vmlord_agent_protocol::{
    auth::Secret,
    backoff::Backoff,
    v1::{ApplyDisplayRecipeResponse, UpdateDisplayPayloadResponse},
};

mod command;
mod display_kernel;
mod display_mounts;
mod display_recipe;
mod gpu_kernel;
mod gpu_mountinfo;
mod gpu_mounts;
mod gpu_probe;
mod gpu_recipe;
mod gpu_render;
mod gpu_targets;
mod guest_files;
mod guest_packages;
mod guest_platform;
mod self_update;
mod session;
mod vsock;

/// Set by the signal handler when the guest is going down.
///
/// An `AtomicBool` and nothing else, because a signal handler may call almost
/// nothing: the unmounting it leads to happens on the main thread, once the
/// session that was open has ended.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// The connection to the host, for the signal handler to wake, or `-1`.
///
/// A flag on its own is not enough to stop this agent: between requests it
/// sits in a read on this connection, and a signal does not end that read --
/// the handler is installed with the BSD semantics `signal` gives it, so the
/// kernel restarts the call, and the socket's own timeout only turns the wait
/// into another wait. The read has to be given something to return, and
/// `shutdown` is what gives it one: it ends the connection under the read,
/// which comes back at once reporting a peer that is gone. Without it a
/// `systemctl stop` waits out the unit's stop timeout and the guest takes a
/// minute and a half to go down.
static WAKE_DESCRIPTOR: AtomicI32 = AtomicI32::new(NO_DESCRIPTOR);

/// What [`WAKE_DESCRIPTOR`] holds while no connection is open.
const NO_DESCRIPTOR: RawFd = -1;

/// This build of the agent, as reported to the host during its hello.
///
/// The version and the revision it was built from. The version alone is the
/// same string on every build there has ever been, so a host reading it could
/// not tell a fresh agent from the one a VM was created with -- and an agent
/// is installed once, at first boot, and then outlives any number of host
/// rebuilds.
const AGENT_VERSION: &str = env!("VMLORD_AGENT_BUILD");

fn main() {
    // Before anything else this process does, and before it needs anything to
    // be true of itself: what the host put on the tools volume is this
    // release's agent, and if it is not the one running, the one running gets
    // out of the way. Nothing below this line would be the new agent's work.
    if self_update::apply() == self_update::Replacement::Installed {
        process::exit(0);
    }

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
    STOPPING.store(true, Ordering::SeqCst);
    // `shutdown` is one of the few calls a handler may make, and it is the
    // one that turns the flag above into something a blocked read notices.
    let descriptor = WAKE_DESCRIPTOR.load(Ordering::SeqCst);
    if descriptor != NO_DESCRIPTOR {
        vsock::wake(descriptor);
    }
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
    while !STOPPING.load(Ordering::SeqCst) {
        let authenticated = connect_to_host(secret);
        // A shutdown that arrived during the session must not be waited out:
        // systemd is holding the guest open for this process to exit.
        if STOPPING.load(Ordering::SeqCst) {
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
    while waited < delay && !STOPPING.load(Ordering::SeqCst) {
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

    // From here until the stream is given up, a shutdown signal ends this
    // connection rather than only setting the flag.
    WAKE_DESCRIPTOR.store(stream.as_raw_fd(), Ordering::SeqCst);
    // A signal that arrived while the socket was being connected saw no
    // descriptor to wake, so the session it would have ended is not started.
    if STOPPING.load(Ordering::SeqCst) {
        WAKE_DESCRIPTOR.store(NO_DESCRIPTOR, Ordering::SeqCst);
        return false;
    }

    let mut opened = None;
    match session::run(
        &mut stream,
        secret,
        AGENT_VERSION,
        &mut opened,
        session::Handlers {
            attach_gpu: &mut gpu_mounts::attach,
            apply_gpu_recipe: &mut || gpu_kernel::apply(&STOPPING),
            probe_gpu: &mut || gpu_render::probe(&STOPPING),
            attach_display: &mut display_mounts::attach,
            apply_display_recipe: &mut |mode| {
                let outcome = display_kernel::apply(&STOPPING, mode);
                ApplyDisplayRecipeResponse {
                    stages: outcome.stages,
                    versions: Some(outcome.versions),
                    signing_certificate: outcome.certificate,
                    desktop: outcome.desktop,
                }
            },
            update_display: &mut |target_version| {
                let (stages, versions, outcome) = display_kernel::update(target_version, &STOPPING);
                UpdateDisplayPayloadResponse {
                    stages,
                    versions: Some(versions),
                    outcome: i32::from(outcome),
                }
            },
        },
    ) {
        Ok(()) => eprintln!("vmlord-agent: the host closed the session"),
        Err(error) => eprintln!("vmlord-agent: {error}"),
    }

    // Before the stream is dropped, so that the handler never shuts down a
    // descriptor the kernel has since handed to something else.
    WAKE_DESCRIPTOR.store(NO_DESCRIPTOR, Ordering::SeqCst);

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

#[cfg(test)]
mod tests {
    use super::{STOPPING, WAKE_DESCRIPTOR, listen_for_shutdown};
    use std::sync::atomic::Ordering;

    /// The bug this covers is a guest that took a minute and a half to stop:
    /// the flag was set, the session was blocked in a read that nothing woke,
    /// and systemd killed the agent when its stop timeout ran out.
    #[test]
    fn a_shutdown_signal_wakes_the_connection_it_arrives_on() {
        let mut pair = [0; 2];
        // SAFETY: `pair` is a two-element array of the type `socketpair`
        // fills, and the constants describe a local stream socket pair.
        let created = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM,
                0,
                pair.as_mut_ptr().cast(),
            )
        };
        assert_eq!(created, 0, "a socket pair for the test");

        // A read that is never woken is the bug itself, so it is given a
        // deadline: this test fails on a regression instead of hanging on it.
        let timeout = libc::timeval {
            tv_sec: 5,
            tv_usec: 0,
        };
        // SAFETY: `timeout` is an initialized `timeval` described by the
        // pointer and length given.
        let limited = unsafe {
            libc::setsockopt(
                pair[0],
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                std::mem::size_of_val(&timeout) as libc::socklen_t,
            )
        };
        assert_eq!(limited, 0, "a read deadline for the test");

        listen_for_shutdown();
        WAKE_DESCRIPTOR.store(pair[0], Ordering::SeqCst);

        // `raise` runs the handler on this thread, which is where the agent's
        // own reads block.
        // SAFETY: `raise` takes a signal number and no pointers.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);

        assert!(STOPPING.load(Ordering::SeqCst), "the flag is set");

        let mut byte = [0u8; 1];
        // SAFETY: `byte` is a valid mutable buffer of the length given.
        let read = unsafe { libc::read(pair[0], byte.as_mut_ptr().cast(), byte.len()) };
        assert_eq!(
            read, 0,
            "a read on the woken connection ends instead of blocking"
        );

        for descriptor in pair {
            // SAFETY: both descriptors are owned by this test and closed once.
            unsafe { libc::close(descriptor) };
        }
    }
}
