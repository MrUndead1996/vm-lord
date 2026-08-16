//! The agent connections VMLord owns, one per running VM.
//!
//! A VM's agent connects when its guest is up and reconnects whenever the
//! connection is lost, so the host end is not a connection but a standing
//! offer: a listener bound to that VM for as long as the VM runs, and a thread
//! that serves whatever arrives on it. The registry below is where those live,
//! keyed by VM id the way every other per-VM resource in this crate is.
//!
//! Lifetime is the whole point of keeping them here. A listener bound to a VM
//! that has stopped is bound to a partition that no longer exists, and a
//! thread parked on it would keep a secret in memory for a guest that is gone.
//! Dropping the entry stops the thread and closes the socket, so `stop`,
//! `delete`, a VM that exited on its own and VMLord itself going away all end
//! the same way: the entry is removed.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use uuid::Uuid;
use vmlord_agent_protocol::{auth::Secret, backoff::Backoff};
use vmlord_core::{GpuShareManifest, RepositoryError};
use zeroize::Zeroizing;

use crate::{
    agent_session::{self, SessionError},
    hvsocket::{ACCEPT_POLL, AgentListener},
    metadata::VmComputeSystemMapping,
};

/// The agent connections VMLord currently owns.
#[derive(Default)]
pub(crate) struct AgentSessions(BTreeMap<Uuid, AgentConnection>);

impl AgentSessions {
    /// Takes ownership of `connection`, ending whatever this VM had before.
    ///
    /// A VM that is started twice -- or restarted while VMLord was still
    /// listening for its previous run -- must not end up with two listeners:
    /// the older one is bound to a runtime id that no longer exists, and only
    /// one of the two could ever accept.
    pub(crate) fn insert(&mut self, connection: AgentConnection) {
        self.0.insert(connection.vm_id, connection);
    }

    /// Stops listening for one VM's agent, if VMLord was.
    pub(crate) fn cancel(&mut self, vm_id: Uuid) {
        self.0.remove(&vm_id);
    }

    /// Stops listening for every VM, for a VMLord that is going away.
    pub(crate) fn cancel_all(&mut self) {
        self.0.clear();
    }

    /// Whether the agent of `vm_id` has a session open right now.
    ///
    /// `None` means VMLord is not listening for that VM at all, which is not
    /// the same as an agent that is offline: a VM created before the agent
    /// service existed has no listener, and reporting its agent as offline
    /// would read as one that failed to start.
    pub(crate) fn is_online(&self, vm_id: Uuid) -> Option<bool> {
        self.0
            .get(&vm_id)
            .map(|connection| connection.online.load(Ordering::Relaxed))
    }
}

/// The standing offer to one VM's agent: a bound listener and the thread that
/// serves it.
pub(crate) struct AgentConnection {
    vm_id: Uuid,
    vm_name: String,
    /// Whether a session is open and authenticated right now.
    ///
    /// Shared with the thread because it is the only thing about a session the
    /// rest of VMLord asks about between refreshes, and asking the thread would
    /// mean a channel for a single bit.
    online: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AgentConnection {
    /// Binds the agent service of a running VM and starts serving it.
    ///
    /// `runtime_id` is the partition the VM is running as, which is what an
    /// HvSocket address names; `secret_path` is the host's copy of the secret
    /// the guest will be challenged against. Both are read here rather than on
    /// the thread, so that a VM whose secret is missing or whose service cannot
    /// be bound says so to the caller instead of failing silently a second
    /// later.
    ///
    /// `shares` is what this VM's guest is told to mount, and it belongs to
    /// the run rather than to a connection: the Plan9 section of a compute
    /// system is written before it is started and is immutable for the
    /// lifetime of a boot, so every session of this run delivers the same
    /// manifest. `None` is a VM VMLord has nothing to say about GPU to, which
    /// is a session with no manifest rather than one with an empty manifest.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if the secret cannot be read, the listener cannot be
    /// bound, or the thread cannot be started. None of these stop the VM: it is
    /// running either way, and what is lost is the agent.
    pub(crate) fn start(
        mapping: &VmComputeSystemMapping,
        runtime_id: Uuid,
        secret_path: &Path,
        shares: Option<GpuShareManifest>,
    ) -> Result<Self, RepositoryError> {
        let vm_name = mapping.vm_name.clone();
        let secret = read_secret(secret_path, &vm_name)?;
        let listener = AgentListener::bind(&vm_name, runtime_id)?;

        let online = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let worker = thread::Builder::new()
            .name(format!("vmlord-agent-{}", mapping.vm_id.as_simple()))
            .spawn({
                let vm_name = vm_name.clone();
                let online = Arc::clone(&online);
                let running = Arc::clone(&running);
                move || {
                    serve(
                        &listener,
                        &secret,
                        shares.as_ref(),
                        &vm_name,
                        &online,
                        &running,
                    )
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the agent thread of VM \"{vm_name}\" could not be started: {error}"
                ));
                log::error!("{error}");
                error
            })?;

        Ok(Self {
            vm_id: mapping.vm_id,
            vm_name,
            online,
            running,
            worker: Some(worker),
        })
    }

    /// A connection with no thread and no socket behind it, for the tests of
    /// what the rest of VMLord reads off one.
    #[cfg(test)]
    pub(crate) fn for_test(vm_id: Uuid, online: bool) -> Self {
        Self {
            vm_id,
            vm_name: format!("vm-{}", vm_id.as_simple()),
            online: Arc::new(AtomicBool::new(online)),
            running: Arc::new(AtomicBool::new(true)),
            worker: None,
        }
    }
}

/// Stops the thread and closes the socket it owns.
///
/// The thread notices within one [`ACCEPT_POLL`] -- or one read timeout, if a
/// session is open -- because nothing else can interrupt a blocking socket. The
/// join is what makes the wait bounded rather than the socket outliving the VM
/// it was bound to.
impl Drop for AgentConnection {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        log::debug!(
            "VMLord stopped listening for the agent of VM \"{}\"",
            self.vm_name
        );
    }
}

/// Accepts one connection at a time and serves it, until VMLord says stop.
///
/// One at a time because a VM has one agent: a second connection while a
/// session is open would be a second thing claiming to speak for the guest, and
/// the one already proved it holds the secret. A reconnecting agent is served
/// by the next turn of this loop, which is what makes losing a connection
/// survivable without anything here knowing that it was lost.
///
/// Nothing on that turn touches Hyper-V. The listener stays bound to the
/// runtime id of the run it was created for, and a reconnect is a new session
/// on it rather than a VM that has to be modified: what a guest proved in the
/// session before is worth nothing to the next one anyway, since the challenge
/// it answered was drawn for that session alone.
///
/// A connection that never authenticates is the one thing this loop slows down
/// for. VMLord's own agent reconnects on a backoff of its own, so a peer that
/// connects and drops as fast as this thread can accept is a broken agent or
/// something else on the machine that found the service, and serving it at that
/// rate would cost a busy thread per VM.
fn serve(
    listener: &AgentListener,
    secret: &Secret,
    shares: Option<&GpuShareManifest>,
    vm_name: &str,
    online: &AtomicBool,
    running: &Arc<AtomicBool>,
) {
    let mut backoff = Backoff::new();

    while running.load(Ordering::Relaxed) {
        let mut stream = match listener.accept(ACCEPT_POLL, running) {
            Ok(Some(stream)) => stream,
            // Nobody connected in the last poll, which is what a guest that
            // has not finished booting looks like.
            Ok(None) => continue,
            // The listener is broken rather than idle: retrying it in a loop
            // would spin a thread on a socket that cannot recover.
            Err(_) => break,
        };

        let authenticated = match agent_session::open(&mut stream, secret, vm_name) {
            Ok(session) => {
                online.store(true, Ordering::Relaxed);
                let outcome = agent_session::serve(&mut stream, &session, shares, vm_name);
                online.store(false, Ordering::Relaxed);
                if let Err(error) = outcome {
                    report(vm_name, &error);
                }
                true
            }
            Err(error) => {
                report(vm_name, &error);
                false
            }
        };
        // Dropped before the wait: a guest reconnecting during it must not find
        // the socket of the session that just ended still open.
        drop(stream);
        wait_before_offering_again(backoff.after(authenticated), running);
    }
}

/// Waits out the backoff, in slices short enough to stop a VM in.
///
/// The wait is spent in [`ACCEPT_POLL`]-sized pieces with the running flag read
/// between them, because dropping an `AgentConnection` joins this thread and
/// must stay bounded by that poll rather than by the longest backoff.
fn wait_before_offering_again(delay: Duration, running: &AtomicBool) {
    let mut waited = Duration::ZERO;
    while waited < delay && running.load(Ordering::Relaxed) {
        let slice = ACCEPT_POLL.min(delay - waited);
        thread::sleep(slice);
        waited += slice;
    }
}

/// Says why a session ended, at the volume it deserves.
///
/// A connection VMLord itself closed is the ordinary end of a session -- the VM
/// was stopped, or VMLord is going away -- and is not something to warn about.
/// Everything else is: an agent that cannot authenticate, or one that sends
/// something unreadable, is a guest whose agent will not work until somebody
/// looks at it.
fn report(vm_name: &str, error: &SessionError) {
    if let SessionError::Frame(frame) = error
        && let vmlord_agent_protocol::frame::FrameError::Io(io) = frame
        && io.kind() == std::io::ErrorKind::ConnectionAborted
    {
        log::debug!("the agent session of VM \"{vm_name}\" ended with the VM");
        return;
    }
    log::warn!("the agent session of VM \"{vm_name}\" ended: {error}");
}

/// Reads the host's copy of a VM's agent secret.
///
/// The text is held in `Zeroizing` on the way through: it is the secret in the
/// form it is stored in, and a `String` dropped normally leaves it in the
/// allocator.
fn read_secret(path: &Path, vm_name: &str) -> Result<Secret, RepositoryError> {
    let text = Zeroizing::new(fs::read_to_string(path).map_err(|error| {
        let error = RepositoryError::new(format!(
            "the agent secret of VM \"{vm_name}\" could not be read from {}: {error}",
            path.display()
        ));
        log::error!("{error}");
        error
    })?);

    Secret::from_base64(&text).map_err(|error| {
        // The text is not quoted: it is the secret, and an error that carried
        // it would put it in the log.
        let error = RepositoryError::new(format!(
            "the agent secret of VM \"{vm_name}\" at {} is unusable: {error}",
            path.display()
        ));
        log::error!("{error}");
        error
    })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use uuid::Uuid;
    use vmlord_agent_protocol::auth::{Nonce, Secret, tag, verify};

    use super::{AgentConnection, AgentSessions, read_secret};

    fn temporary_file(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("vmlord-agent-secret-{name}"));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn a_stored_secret_is_read_back_as_the_one_that_was_written() {
        let path = temporary_file("round-trip");
        let secret = Secret::generate();
        // With the trailing newline `write_provisioning` ends the file with.
        fs::write(&path, format!("{}\n", secret.to_base64().as_str()))
            .expect("the secret should be writable");

        let read_back = read_secret(&path, "dev-linux").expect("the secret just written");

        let nonce = Nonce::generate();
        assert!(verify(&read_back, &nonce, &tag(&secret, &nonce)));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_vm_with_no_secret_cannot_be_listened_for() {
        // `Secret` has no `Debug` on purpose, so the error is taken out by
        // hand rather than through `expect_err`.
        let Err(error) = read_secret(&temporary_file("missing"), "dev-linux") else {
            panic!("a secret that is not there must not be read as one");
        };

        assert!(error.to_string().contains("dev-linux"));
    }

    #[test]
    fn a_secret_that_is_not_one_is_refused_without_quoting_it() {
        let path = temporary_file("not-a-secret");
        fs::write(&path, "hunter2").expect("the file should be writable");

        let Err(error) = read_secret(&path, "dev-linux") else {
            panic!("text that is not a secret must not be read as one");
        };

        assert!(!error.to_string().contains("hunter2"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_vm_listened_for_twice_keeps_only_the_newer_listener() {
        let vm_id = Uuid::from_u128(7);
        let mut sessions = AgentSessions::default();

        sessions.insert(AgentConnection::for_test(vm_id, false));
        sessions.insert(AgentConnection::for_test(vm_id, true));

        assert_eq!(sessions.is_online(vm_id), Some(true));
    }

    #[test]
    fn a_vm_nobody_listens_for_has_no_agent_status_at_all() {
        let mut sessions = AgentSessions::default();
        let vm_id = Uuid::from_u128(9);
        sessions.insert(AgentConnection::for_test(vm_id, true));

        sessions.cancel(vm_id);

        assert_eq!(sessions.is_online(vm_id), None);
    }

    #[test]
    fn every_listener_goes_when_vmlord_does() {
        let mut sessions = AgentSessions::default();
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        sessions.insert(AgentConnection::for_test(first, true));
        sessions.insert(AgentConnection::for_test(second, false));

        sessions.cancel_all();

        assert_eq!(sessions.is_online(first), None);
        assert_eq!(sessions.is_online(second), None);
    }

    #[test]
    fn a_backoff_wait_lasts_as_long_as_it_was_given() {
        let running = AtomicBool::new(true);
        let delay = Duration::from_millis(120);

        let started = Instant::now();
        super::wait_before_offering_again(delay, &running);

        assert!(started.elapsed() >= delay);
    }

    #[test]
    fn a_backoff_wait_ends_when_the_connection_is_told_to_stop() {
        // Stopping a VM joins this thread, so a wait that ran to the end would
        // freeze the caller for as long as the longest backoff.
        let running = AtomicBool::new(false);

        let started = Instant::now();
        super::wait_before_offering_again(Duration::from_secs(30), &running);

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn dropping_a_connection_tells_its_thread_to_stop() {
        let running = Arc::new(AtomicBool::new(true));
        let connection = AgentConnection {
            vm_id: Uuid::from_u128(3),
            vm_name: "dev-linux".to_owned(),
            online: Arc::new(AtomicBool::new(true)),
            running: Arc::clone(&running),
            worker: None,
        };

        drop(connection);

        assert!(!running.load(Ordering::Relaxed));
    }
}
