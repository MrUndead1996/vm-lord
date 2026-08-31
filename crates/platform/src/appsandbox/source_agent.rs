//! The one exchange VMLord has with the guest agent of the source application.
//!
//! An AppSandbox guest is given a static address by its own agent, which also
//! deletes every other netplan file and turns cloud-init's network module off.
//! It never asks for one over DHCP. So the copy VMLord boots comes up on the
//! source application's subnet, on a network that no longer exists, and no
//! amount of waiting for an address will produce one.
//!
//! What does exist is the agent that put it there. It listens on an
//! `AF_VSOCK` port, is started by systemd `After=local-fs.target`, and needs no
//! network at all -- so it can be reached in exactly the situation where
//! nothing else can, and asked to move the guest onto VMLord's network.
//!
//! This is the only place that speaks another application's protocol, and it
//! speaks the smallest possible amount of it: one command, one reply. Nothing
//! here reads the guest, writes a file, or keeps the connection past the
//! answer.

use std::{
    io::{ErrorKind, Read, Write},
    net::Ipv4Addr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::hvsocket::{AgentStream, SOURCE_AGENT_VSOCK_PORT, connect_to_guest, vsock_service_id};

/// How long a connect is left alone before it is tried again.
///
/// The guest is booting for most of this wait, and each attempt already costs
/// its own connect timeout, so the pause between them only has to keep a
/// refusing guest from being asked in a tight loop.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// The tag every command here is sent under.
///
/// The protocol allows a sequence of commands over one connection and matches
/// replies by tag. VMLord sends one command and closes, so the sequence never
/// advances past its first number -- but the tag is still sent, because it is
/// what separates a reply from the `hello` and `heartbeat` lines the agent
/// emits on its own.
const TAG: &str = "1";

/// What a line from the agent said about the command that is outstanding.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// Something the agent says on its own -- `hello`, a heartbeat, a reply to
    /// nothing. Not an answer.
    Unrelated,
    /// The command succeeded.
    Accepted,
    /// The command failed, with the agent's own words for why.
    Refused(String),
}

/// Reads one line from the agent as an answer to the command tagged `tag`.
fn classify(tag: &str, line: &str) -> Answer {
    let Some(reply) = line
        .strip_prefix(tag)
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return Answer::Unrelated;
    };
    if reply == "ok" {
        Answer::Accepted
    } else {
        Answer::Refused(reply.to_owned())
    }
}

/// Splits what has arrived so far into whole lines, keeping the remainder.
///
/// The agent writes a line at a time but the transport does not preserve those
/// boundaries, and a heartbeat can arrive glued to the reply that follows it.
struct Lines {
    pending: String,
}

impl Lines {
    const fn new() -> Self {
        Self {
            pending: String::new(),
        }
    }

    /// Adds `chunk` and returns every complete line it finished.
    fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.pending.push_str(chunk);
        let mut lines = Vec::new();
        while let Some(end) = self.pending.find('\n') {
            let line = self.pending[..end].trim_end_matches('\r').to_owned();
            self.pending.drain(..=end);
            if !line.is_empty() {
                lines.push(line);
            }
        }
        lines
    }
}

/// A connection to the source application's guest agent, open for one command.
pub(crate) struct SourceAgent {
    stream: AgentStream,
    vm_name: String,
    lines: Lines,
    /// Held because [`AgentStream`] reads through it: while it is true an idle
    /// poll is a `WouldBlock` to poll again, which is what a deadline is made
    /// of.
    running: Arc<AtomicBool>,
}

impl SourceAgent {
    /// Knocks until the agent inside the copied guest answers, or `deadline`
    /// passes.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] naming the last refusal if the deadline passes with
    /// nothing listening -- which is what a guest that never booted, or one
    /// whose agent was removed, looks like from here.
    pub(crate) fn connect(
        vm_name: &str,
        runtime_id: Uuid,
        deadline: Instant,
    ) -> Result<Self, RepositoryError> {
        let running = Arc::new(AtomicBool::new(true));
        let service = vsock_service_id(SOURCE_AGENT_VSOCK_PORT);
        let mut last_error;
        loop {
            match connect_to_guest(vm_name, runtime_id, service, &running) {
                Ok(stream) => {
                    tracing::info!(
                        "the guest agent of the imported VM \"{vm_name}\" answered on \
                         vsock port {SOURCE_AGENT_VSOCK_PORT}"
                    );
                    return Ok(Self {
                        stream,
                        vm_name: vm_name.to_owned(),
                        lines: Lines::new(),
                        running,
                    });
                }
                Err(error) => last_error = error,
            }
            if Instant::now() >= deadline {
                let error = RepositoryError::new(format!(
                    "the copied guest of VM \"{vm_name}\" never answered on the source \
                     application's agent channel: {last_error}"
                ));
                tracing::error!("{error}");
                return Err(error);
            }
            std::thread::sleep(RETRY_INTERVAL);
        }
    }

    /// Moves the guest onto `address`, reachable through `gateway`.
    ///
    /// The agent rewrites the guest's own network configuration and applies it,
    /// so this is the first thing an import does that changes the copy. The
    /// source VM is untouched: this connection can only reach the partition
    /// the copy is running as.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if the command could not be sent, the agent refused
    /// it, or `deadline` passed before it answered.
    pub(crate) fn move_onto(
        &mut self,
        address: Ipv4Addr,
        prefix_length: u8,
        gateway: Ipv4Addr,
        deadline: Instant,
    ) -> Result<(), RepositoryError> {
        tracing::info!(
            "asking the copied guest of VM \"{}\" to take {address}/{prefix_length} via {gateway}",
            self.vm_name
        );
        self.send(&format!("{TAG}:set_ip:{address}/{prefix_length}:{gateway}"))?;
        self.await_answer(deadline)
    }

    fn send(&mut self, command: &str) -> Result<(), RepositoryError> {
        self.stream
            .write_all(format!("{command}\n").as_bytes())
            .map_err(|error| self.failure(format!("could not be sent a command: {error}")))
    }

    /// Reads until the agent answers the outstanding command.
    fn await_answer(&mut self, deadline: Instant) -> Result<(), RepositoryError> {
        let mut buffer = [0u8; 512];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    return Err(self.failure("closed the connection before it answered".to_owned()));
                }
                Ok(read) => {
                    let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    for line in self.lines.feed(&chunk) {
                        match classify(TAG, &line) {
                            Answer::Unrelated => {}
                            Answer::Accepted => return Ok(()),
                            Answer::Refused(reason) => {
                                return Err(self.failure(format!("refused it: {reason}")));
                            }
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    return Err(self.failure(format!("could not be read: {error}")));
                }
            }
            if Instant::now() >= deadline {
                return Err(self.failure("never answered".to_owned()));
            }
        }
    }

    fn failure(&self, what: String) -> RepositoryError {
        let error = RepositoryError::new(format!(
            "the guest agent of the imported VM \"{}\" {what}",
            self.vm_name
        ));
        tracing::error!("{error}");
        error
    }
}

impl Drop for SourceAgent {
    fn drop(&mut self) {
        // Ends the read poll of the stream's own `Drop` rather than leaving it
        // to expire: the connection is finished the moment the answer is in.
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::{Answer, Lines, classify};

    #[test]
    fn the_agents_own_chatter_answers_nothing() {
        // `hello` arrives before any command is sent and a heartbeat every
        // five seconds after it. Neither is a reply, and treating either as
        // one would report a set_ip that never happened as done.
        assert_eq!(classify("1", "hello"), Answer::Unrelated);
        assert_eq!(classify("1", "heartbeat"), Answer::Unrelated);
    }

    #[test]
    fn a_reply_to_another_command_answers_nothing() {
        // The tag is what separates them, so a reply carrying a different one
        // belongs to somebody else's command.
        assert_eq!(classify("1", "2:ok"), Answer::Unrelated);
    }

    #[test]
    fn a_tagged_ok_is_the_answer() {
        assert_eq!(classify("1", "1:ok"), Answer::Accepted);
    }

    #[test]
    fn a_tagged_error_carries_the_agents_own_words() {
        // Kept verbatim: `error:netplan_failed` and `error:bad_format` mean
        // very different things to whoever reads the failure.
        assert_eq!(
            classify("1", "1:error:netplan_failed"),
            Answer::Refused("error:netplan_failed".to_owned())
        );
    }

    #[test]
    fn lines_are_recovered_from_chunks_that_split_them() {
        // The transport does not preserve line boundaries, so a reply can
        // arrive in two reads with the newline in neither of the obvious
        // places.
        let mut lines = Lines::new();

        assert!(lines.feed("hel").is_empty());
        assert!(lines.feed("lo").is_empty());
        assert_eq!(lines.feed("\n1:"), vec!["hello".to_owned()]);
        assert_eq!(lines.feed("ok\n"), vec!["1:ok".to_owned()]);
    }

    #[test]
    fn lines_are_recovered_from_a_chunk_carrying_several() {
        // A heartbeat glued to the reply that followed it must not hide the
        // reply.
        let mut lines = Lines::new();

        assert_eq!(
            lines.feed("heartbeat\r\n1:ok\n"),
            vec!["heartbeat".to_owned(), "1:ok".to_owned()]
        );
    }
}
