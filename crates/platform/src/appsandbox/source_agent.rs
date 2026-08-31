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
//! speaks the smallest possible amount of it: two commands. Nothing here reads
//! the guest, writes a file, or keeps the connection past the answers.

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

/// What the agent says when it wrote the network configuration but the address
/// did not appear on the interface.
///
/// Its `set_ip` ends with `netplan apply` and then `systemctl restart
/// NetworkManager`, and a NetworkManager that restarts over an interface which
/// already carries an address *assumes* that address instead of applying the
/// profile it was just given. So the guest keeps the address it booted with,
/// the agent's own five-second poll correctly reports that the new one never
/// appeared, and this is what comes back.
///
/// It is not a failure to VMLord. The configuration is on disk, and a guest
/// that starts from cold reads it and takes the address -- which is why an
/// import answers this by restarting the guest rather than by giving up.
const NETPLAN_NOT_APPLIED: &str = "error:netplan_failed";

/// What a line from the agent said about the command that is outstanding.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// Something the agent says on its own -- `hello`, a heartbeat, a reply to
    /// nothing. Not an answer.
    Unrelated,
    /// The agent's reply, with the tag stripped.
    Reply(String),
}

/// What became of the address the guest was asked to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddressOutcome {
    /// The guest is on the address now.
    Applied,
    /// The configuration was written and the address did not take. A guest
    /// that boots from cold will read it -- see [`NETPLAN_NOT_APPLIED`].
    NeedsRestart,
}

impl AddressOutcome {
    /// Reads the agent's reply to `set_ip`.
    ///
    /// `None` is a reply this import has no answer for -- a malformed command,
    /// or something a later version of that agent says -- and becomes a
    /// failure rather than a restart.
    fn of(reply: &str) -> Option<Self> {
        match reply {
            "ok" => Some(Self::Applied),
            NETPLAN_NOT_APPLIED => Some(Self::NeedsRestart),
            _ => None,
        }
    }
}

/// Reads one line from the agent as an answer to the command tagged `tag`.
fn classify(tag: &str, line: &str) -> Answer {
    line.strip_prefix(tag)
        .and_then(|rest| rest.strip_prefix(':'))
        .map_or(Answer::Unrelated, |reply| Answer::Reply(reply.to_owned()))
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
    /// The tag the next command is sent under. Replies are matched by tag, so
    /// a second command must not reuse the first one's.
    next_tag: u32,
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
                        next_tag: 1,
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
    /// The agent rewrites the guest's own network configuration, so this is the
    /// first thing an import does that changes the copy. The source VM is
    /// untouched: this connection can only reach the partition the copy is
    /// running as.
    ///
    /// Whether the address takes immediately is the guest's business and not
    /// this import's -- see [`AddressOutcome`].
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if the command could not be sent, the agent refused
    /// it for a reason a restart cannot answer, or `deadline` passed first.
    pub(crate) fn move_onto(
        &mut self,
        address: Ipv4Addr,
        prefix_length: u8,
        gateway: Ipv4Addr,
        deadline: Instant,
    ) -> Result<AddressOutcome, RepositoryError> {
        tracing::info!(
            "asking the copied guest of VM \"{}\" to take {address}/{prefix_length} via {gateway}",
            self.vm_name
        );
        let reply = self.command(
            &format!("set_ip:{address}/{prefix_length}:{gateway}"),
            deadline,
        )?;
        match AddressOutcome::of(&reply) {
            Some(AddressOutcome::Applied) => Ok(AddressOutcome::Applied),
            Some(AddressOutcome::NeedsRestart) => {
                tracing::info!(
                    "the copied guest of VM \"{}\" wrote the address down but did not take it; \
                     it needs a restart to read it",
                    self.vm_name
                );
                Ok(AddressOutcome::NeedsRestart)
            }
            None => Err(self.failure(format!("refused it: {reply}"))),
        }
    }

    /// Asks the guest to reboot.
    ///
    /// The agent answers before it goes, so a reply here means the reboot was
    /// accepted rather than finished. What proves the guest came back is the
    /// wait that follows, not this.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if the command could not be sent, was refused, or
    /// `deadline` passed before the agent answered.
    pub(crate) fn restart(&mut self, deadline: Instant) -> Result<(), RepositoryError> {
        tracing::info!(
            "asking the copied guest of VM \"{}\" to restart so it reads its new address",
            self.vm_name
        );
        match self.command("restart", deadline)?.as_str() {
            "ok" => Ok(()),
            other => Err(self.failure(format!("refused to restart: {other}"))),
        }
    }

    /// Sends one command and returns the agent's reply to it.
    ///
    /// Each command gets its own tag. The protocol matches replies by tag, and
    /// it is also what separates a reply from the `hello` and the five-second
    /// heartbeats the agent emits on its own.
    fn command(&mut self, command: &str, deadline: Instant) -> Result<String, RepositoryError> {
        let tag = self.next_tag.to_string();
        self.next_tag += 1;
        self.stream
            .write_all(format!("{tag}:{command}\n").as_bytes())
            .map_err(|error| self.failure(format!("could not be sent a command: {error}")))?;
        self.await_reply(&tag, deadline)
    }

    /// Reads until the agent answers the command tagged `tag`.
    fn await_reply(&mut self, tag: &str, deadline: Instant) -> Result<String, RepositoryError> {
        let mut buffer = [0u8; 512];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    return Err(self.failure("closed the connection before it answered".to_owned()));
                }
                Ok(read) => {
                    let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    for line in self.lines.feed(&chunk) {
                        if let Answer::Reply(reply) = classify(tag, &line) {
                            return Ok(reply);
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
    use super::{AddressOutcome, Answer, Lines, classify};

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
        // belongs to somebody else's command -- which matters as soon as a
        // second command is sent down the same connection.
        assert_eq!(classify("1", "2:ok"), Answer::Unrelated);
    }

    #[test]
    fn a_tagged_line_is_the_answer_to_that_command() {
        assert_eq!(classify("1", "1:ok"), Answer::Reply("ok".to_owned()));
    }

    #[test]
    fn a_tagged_error_carries_the_agents_own_words() {
        // Kept verbatim: `error:netplan_failed` is answered with a restart and
        // `error:bad_format` is a failure, so the two must stay distinct all
        // the way up.
        assert_eq!(
            classify("2", "2:error:netplan_failed"),
            Answer::Reply("error:netplan_failed".to_owned())
        );
    }

    #[test]
    fn an_ok_means_the_guest_is_on_the_address() {
        assert_eq!(AddressOutcome::of("ok"), Some(AddressOutcome::Applied));
    }

    #[test]
    fn a_netplan_failure_is_answered_with_a_restart_and_not_with_a_failure() {
        // The agent finishes `set_ip` by restarting NetworkManager, and a
        // NetworkManager restarted over an interface that already carries an
        // address assumes that address instead of applying the new profile.
        // The file is written either way, so a guest that starts from cold
        // reads it -- which is why this is the one refusal that is not one.
        assert_eq!(
            AddressOutcome::of("error:netplan_failed"),
            Some(AddressOutcome::NeedsRestart)
        );
    }

    #[test]
    fn any_other_refusal_stays_a_failure() {
        // A restart cannot answer a command the agent could not parse, and
        // treating an unknown reply as one would hide it behind a reboot and a
        // five-minute wait for a port.
        assert_eq!(AddressOutcome::of("error:bad_format"), None);
        assert_eq!(AddressOutcome::of("error:unknown"), None);
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
