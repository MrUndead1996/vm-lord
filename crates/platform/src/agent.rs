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
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use uuid::Uuid;
use vmlord_agent_protocol::v1::DisplayUpdateOutcome;
use vmlord_agent_protocol::{auth::Secret, backoff::Backoff};
use vmlord_core::{DisplayMode, DisplayShare, GpuShareManifest, RepositoryError};
use zeroize::Zeroizing;

use crate::{
    agent_session::{
        self, GuestDisplayPayloadReport, GuestDisplaySink, GuestGpuSink, SessionError,
    },
    display_runs::DisplayRuns,
    gpu_runs::GpuRuns,
    hvsocket::{ACCEPT_POLL, AgentListener, AgentStream},
    layout,
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

    /// Hands out the way to ask the agent of `vm_id` for a payload update.
    ///
    /// A channel rather than the answer itself, because the asking happens on a
    /// thread of its own: an update takes as long as a DKMS build inside the
    /// guest, and the registry lives behind the repository's `&mut self`, which
    /// is the UI's thread. `None` is a VM VMLord is not listening for at all.
    pub(crate) fn display_update_channel(&self, vm_id: Uuid) -> Option<DisplayUpdateChannel> {
        Some(self.0.get(&vm_id)?.display_update_channel())
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
            .map(|connection| session_online(&connection.online))
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
    online: Arc<Mutex<bool>>,
    running: Arc<AtomicBool>,
    /// Where a display payload update is handed to the thread that owns the
    /// session.
    updates: Sender<DisplayUpdate>,
    worker: Option<JoinHandle<()>>,
}

/// One update, and where its answer goes.
pub(crate) struct DisplayUpdate {
    pub(crate) target_version: String,
    pub(crate) answer: Sender<DisplayUpdateAnswer>,
}

/// What the guest made of an update it was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayUpdateAnswer {
    pub(crate) outcome: DisplayUpdateOutcome,
    pub(crate) report: GuestDisplayPayloadReport,
}

/// How long a caller waits for a guest to finish an update.
///
/// Long, because what it waits on is a DKMS build against the guest's running
/// kernel, and the recipe's own budget for one is fifteen minutes. Shorter than
/// forever, because a guest that stopped answering must not hold the thread
/// that asked.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The way to ask one VM's guest for a display payload update, off the thread
/// that owns the session registry.
///
/// Carries only what the asking needs -- where to send the request, whether
/// there is a session to send it to, and the VM's name for what it has to say
/// -- so that an update can be handed to a worker without handing it the
/// registry. The sender is the connection's own: a session is one conversation
/// on one socket, and every request for this VM goes to the one thread serving
/// it, whichever thread asked.
pub(crate) struct DisplayUpdateChannel {
    vm_name: String,
    online: Arc<Mutex<bool>>,
    updates: Sender<DisplayUpdate>,
}

impl AgentConnection {
    /// Hands out this connection's update channel.
    fn display_update_channel(&self) -> DisplayUpdateChannel {
        DisplayUpdateChannel {
            vm_name: self.vm_name.clone(),
            online: Arc::clone(&self.online),
            updates: self.updates.clone(),
        }
    }
}

impl DisplayUpdateChannel {
    /// Asks this VM's guest to move its display payload to `target_version`.
    ///
    /// Blocks until the guest answers or the budget runs out, because what a
    /// caller wants from an update is whether it worked -- which is why the
    /// caller is a worker thread. A VM whose session is not open right now
    /// answers immediately: there is nobody to ask, and queueing the request
    /// would move a version at a moment nobody chose.
    pub(crate) fn ask(&self, target_version: &str) -> Result<DisplayUpdateAnswer, RepositoryError> {
        let (answer, answered) = mpsc::channel();
        let online = self
            .online
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*online {
            return Err(RepositoryError::new(format!(
                "the agent of VM \"{}\" has no open session, so its display payload cannot be \
                 updated right now",
                self.vm_name
            )));
        }
        self.updates
            .send(DisplayUpdate {
                target_version: target_version.to_owned(),
                answer,
            })
            .map_err(|_| {
                RepositoryError::new(format!(
                    "the agent thread of VM \"{}\" is gone",
                    self.vm_name
                ))
            })?;
        drop(online);

        answered.recv_timeout(UPDATE_TIMEOUT).map_err(|error| {
            let reason = match error {
                RecvTimeoutError::Timeout => "did not finish inside the time allowed for it",
                RecvTimeoutError::Disconnected => "ended before it answered",
            };
            RepositoryError::new(format!(
                "the display payload update of VM \"{}\" {reason}",
                self.vm_name
            ))
        })
    }
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
    /// `facts` is where what the guest says about its GPU is recorded. The
    /// registry is shared rather than owned because the list of VMs reads it on
    /// every refresh, and a report that stayed on this thread would be a report
    /// only the log ever saw.
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
        // `vm_directory` rather than one path per file: the agent's secret and
        // the guest's signing certificate are two names in the same directory,
        // and `layout` is what knows them.
        vm_directory: &Path,
        shares: Option<GpuShareManifest>,
        display_share: Option<DisplayShare>,
        facts: GpuRuns,
        display_facts: DisplayRuns,
    ) -> Result<Self, RepositoryError> {
        let vm_name = mapping.vm_name.clone();
        let vm_id = mapping.vm_id;
        let display_mode = mapping.display_mode;
        let secret = read_secret(&layout::agent_secret_path(vm_directory), &vm_name)?;
        let mok_certificate_path = layout::display_mok_certificate_path(vm_directory);
        let listener = AgentListener::bind(&vm_name, runtime_id)?;

        let online = Arc::new(Mutex::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let (updates, pending_updates) = mpsc::channel();
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
                        &pending_updates,
                        shares.as_ref(),
                        display_share.as_ref(),
                        display_mode,
                        &vm_name,
                        &online,
                        &running,
                        &|report| facts.record_guest(vm_id, report),
                        &|report| {
                            if let Some(certificate) = &report.signing_certificate {
                                write_mok_certificate(&mok_certificate_path, certificate, &vm_name);
                            }
                            if let Some(guest) = report.guest {
                                display_facts.record_guest_display(vm_id, guest);
                            }
                            if let Some(desktop) = report.desktop {
                                display_facts.record_guest_desktop(vm_id, desktop);
                            }
                            display_facts.record_guest_payload(
                                vm_id,
                                report.installed,
                                report.previous,
                                report.loaded,
                                report.failure,
                            );
                        },
                    )
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the agent thread of VM \"{vm_name}\" could not be started: {error}"
                ));
                tracing::error!("{error}");
                error
            })?;

        Ok(Self {
            vm_id: mapping.vm_id,
            vm_name,
            online,
            running,
            updates,
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
            online: Arc::new(Mutex::new(online)),
            running: Arc::new(AtomicBool::new(true)),
            // Nothing serves this one, so an update sent into it is never read
            // -- which is what a connection with no session is.
            updates: mpsc::channel().0,
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
        tracing::debug!(
            "VMLord stopped listening for the agent of VM \"{}\"",
            self.vm_name
        );
    }
}

/// Accepts one connection at a time and serves it, until VMLord says stop.
///
/// One active session at a time because a VM has one agent. A candidate may
/// arrive while the active socket still looks open, which is what an unclean
/// guest reboot does to HvSocket; it displaces the old session only after it
/// proves the same secret. A reconnecting agent is otherwise served by the
/// next turn of this loop, which makes losing a connection survivable without
/// anything here knowing why it was lost.
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
#[expect(
    clippy::too_many_arguments,
    reason = "one thread serves both stacks, and each needs what to offer and where to report"
)]
fn serve(
    listener: &AgentListener,
    secret: &Secret,
    updates: &Receiver<DisplayUpdate>,
    shares: Option<&GpuShareManifest>,
    display_share: Option<&DisplayShare>,
    display_mode: Option<DisplayMode>,
    vm_name: &str,
    online: &Mutex<bool>,
    running: &Arc<AtomicBool>,
    sink: GuestGpuSink<'_>,
    display_sink: GuestDisplaySink<'_>,
) {
    let mut backoff = Backoff::new();
    let mut candidate_backoff = Backoff::new();
    let mut candidate_after = Instant::now();
    let mut replacement: Option<(AgentStream, agent_session::AgentSession)> = None;

    while running.load(Ordering::Relaxed) {
        let (mut stream, session) = if let Some(replacement) = replacement.take() {
            replacement
        } else {
            let mut stream = match listener.accept(ACCEPT_POLL, running) {
                Ok(Some(stream)) => stream,
                // Nobody connected in the last poll, which is what a guest
                // that has not finished booting looks like.
                Ok(None) => continue,
                // The listener is broken rather than idle: retrying it in a
                // loop would spin a thread on a socket that cannot recover.
                Err(_) => break,
            };
            match agent_session::open(&mut stream, secret, vm_name) {
                Ok(session) => (stream, session),
                Err(error) => {
                    report(vm_name, &error);
                    drop(stream);
                    wait_before_offering_again(backoff.after(false), running);
                    continue;
                }
            }
        };

        *online
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        let mut next_session = None;
        let mut listener_failed = false;
        let outcome = agent_session::serve_with_replacement(
            &mut stream,
            &session,
            agent_session::SessionWork {
                gpu_shares: shares,
                display_share,
                display_mode,
                gpu: sink,
                display: display_sink,
                updates: Some(updates),
            },
            vm_name,
            &mut || {
                if Instant::now() < candidate_after {
                    return false;
                }
                match listener.accept(Duration::ZERO, running) {
                    Ok(Some(mut candidate)) => {
                        match agent_session::open_replacement(&mut candidate, secret, vm_name) {
                            Ok(session) => {
                                let _ = candidate_backoff.after(true);
                                next_session = Some((candidate, session));
                                true
                            }
                            Err(error) => {
                                report(vm_name, &error);
                                candidate_after = Instant::now() + candidate_backoff.after(false);
                                false
                            }
                        }
                    }
                    Ok(None) => false,
                    Err(_) => {
                        listener_failed = true;
                        true
                    }
                }
            },
        );
        set_offline_and_cancel_updates(online, updates);
        drop(stream);

        if listener_failed {
            break;
        }
        if matches!(outcome, Ok(agent_session::SessionExit::Replaced))
            && let Some(next_session) = next_session
        {
            replacement = Some(next_session);
            continue;
        }
        if let Err(error) = outcome {
            report(vm_name, &error);
        }
        wait_before_offering_again(backoff.after(true), running);
    }
}

fn session_online(online: &Mutex<bool>) -> bool {
    *online
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Closes the admission gate before discarding work that belonged to the
/// session which just ended. Holding the same gate as `ask` makes the state
/// change and queue drain one indivisible transition to callers.
fn set_offline_and_cancel_updates(online: &Mutex<bool>, updates: &Receiver<DisplayUpdate>) {
    let mut online = online
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *online = false;
    while let Ok(update) = updates.try_recv() {
        drop(update);
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
        tracing::debug!("the agent session of VM \"{vm_name}\" ended with the VM");
        return;
    }
    tracing::warn!("the agent session of VM \"{vm_name}\" ended: {error}");
}

/// Reads the host's copy of a VM's agent secret.
///
/// The text is held in `Zeroizing` on the way through: it is the secret in the
/// form it is stored in, and a `String` dropped normally leaves it in the
/// allocator.
/// Keeps the guest's signing certificate where a person who has to enroll it
/// can find it.
///
/// Overwritten on every run rather than written once: it is a copy of what the
/// guest holds now, and a stale copy would send somebody to enroll a
/// certificate the guest has since replaced.
///
/// Never fatal. A certificate nobody can enroll yet is not a reason to end a
/// session that is otherwise bringing a desktop up.
fn write_mok_certificate(path: &Path, certificate: &[u8], vm_name: &str) {
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(path, certificate));
    if let Err(error) = written {
        tracing::warn!(
            "the signing certificate of VM \"{vm_name}\" could not be written to {}: {error}",
            path.display()
        );
    }
}

fn read_secret(path: &Path, vm_name: &str) -> Result<Secret, RepositoryError> {
    let text = Zeroizing::new(fs::read_to_string(path).map_err(|error| {
        let error = RepositoryError::new(format!(
            "the agent secret of VM \"{vm_name}\" could not be read from {}: {error}",
            path.display()
        ));
        tracing::error!("{error}");
        error
    })?);

    Secret::from_base64(&text).map_err(|error| {
        // The text is not quoted: it is the secret, and an error that carried
        // it would put it in the log.
        let error = RepositoryError::new(format!(
            "the agent secret of VM \"{vm_name}\" at {} is unusable: {error}",
            path.display()
        ));
        tracing::error!("{error}");
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
            mpsc,
        },
        time::{Duration, Instant},
    };

    use uuid::Uuid;
    use vmlord_agent_protocol::auth::{Nonce, Secret, tag, verify};

    use super::{
        AgentConnection, AgentSessions, DisplayUpdate, read_secret, session_online,
        set_offline_and_cancel_updates,
    };

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
            online: Arc::new(std::sync::Mutex::new(true)),
            running: Arc::clone(&running),
            updates: mpsc::channel().0,
            worker: None,
        };

        drop(connection);

        assert!(!running.load(Ordering::Relaxed));
    }

    #[test]
    fn ending_a_session_rejects_an_update_still_in_its_queue() {
        let online = Arc::new(std::sync::Mutex::new(true));
        let (updates, pending_updates) = mpsc::channel();
        let (answer, answered) = mpsc::channel();
        updates
            .send(DisplayUpdate {
                target_version: "0.2.0".to_owned(),
                answer,
            })
            .expect("the session queue is open");

        set_offline_and_cancel_updates(&online, &pending_updates);

        assert!(!session_online(&online));
        assert!(matches!(answered.recv(), Err(mpsc::RecvError)));
    }
}
