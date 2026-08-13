//! The VMLord agent, which runs inside a Linux guest.
//!
//! It is the guest half of the protocol in `vmlord-agent-protocol`: it opens
//! the HvSocket connection to the host, authenticates the session, and does
//! the in-guest work the host asks for -- reporting what the GPU is doing and
//! mounting the shares GPU-PV needs.
//!
//! None of that exists yet. What is here is the crate the rest of it is built
//! in and the one thing it can already answer for: which protocol revision
//! this build speaks. That answer is worth having on its own, because
//! installing the agent (#91) has to be verifiable from inside a guest before
//! there is any host to connect to.

// The agent is built for the guest, never for the host. Saying so here turns
// an accidental `--workspace` build on Windows into a sentence instead of a
// link error about missing syscalls once the transport lands.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "vmlord-agent runs in a Linux guest; build it with \
     `cargo build -p vmlord-agent --target x86_64-unknown-linux-gnu`"
);

use vmlord_agent_protocol::v1::ProtocolVersion;

/// This build of the agent, as reported to the host and printed below.
const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let ProtocolVersion { major, minor } = ProtocolVersion::current();

    println!("vmlord-agent {AGENT_VERSION}, agent protocol {major}.{minor}");
}
