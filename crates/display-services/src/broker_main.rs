//! The privileged half, wired together.
//!
//! Three threads and one lock. The control thread owns the session, the IPC
//! thread owns the peer, and the capture thread owns the device; what they
//! share is the little that has to be shared, behind one [`Mutex`] and one
//! [`Condvar`]. The capture thread waits on the condvar rather than polling,
//! so a guest with no viewer costs a sleeping thread and nothing else.
//!
//! Every log line goes to stderr, which journald keeps -- the same choice the
//! guest agent makes.

use std::{
    collections::HashSet,
    env,
    io::{self, ErrorKind},
    os::fd::{AsFd, OwnedFd},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use vmlord_display_protocol::{keys::Secret, v1::ErrorCode};

use crate::{
    control::{Control, Outcome, support_from},
    drm::{DRM_CLASS, DRM_DEVICES, Device, PlaneState},
    ipc::{Message, PlaneKind, PlaneLayout, SessionParameters},
    output::Output,
    unix::{Connection, Listener},
    vsock::{self, CONTROL_PORT},
};

/// The driver name task #114's module registers under.
const DRIVER: &str = "vmlord_drm";

/// Where the socket between the two processes lives.
const SOCKET_PATH: &str = "/run/vmlord/display-broker.sock";

/// Where the clipboard daemon of whoever is logged in connects.
///
/// A socket of its own rather than a second peer on the broker's: the two
/// peers are different accounts, hold different keys and are authorised by
/// different rules.
const CLIPBOARD_SOCKET_PATH: &str = "/run/vmlord/display-clipboard.sock";

/// The unprivileged user the session process runs as.
const SERVICE_USER: &str = "vmlord-display";

/// How long a booting guest is given for its module to load.
const DEVICE_DEADLINE: Duration = Duration::from_secs(120);

/// How long the control socket waits before reporting an idle read.
///
/// Not a timeout on the session: an idle desktop is the ordinary state, and
/// what this buys is a control thread that comes up for air often enough to
/// notice a fault another thread found.
const CONTROL_IDLE: Duration = Duration::from_secs(1);

/// What the broker was told to do, from its environment.
///
/// Every one has a default that is right inside a guest; the overrides exist so
/// that a developer can point the broker at a stand-in tree without a VM.
#[derive(Clone, Debug)]
pub struct Options {
    /// The driver whose card to take, by name rather than by number.
    pub driver: String,
    /// Where the kernel lists DRM devices.
    pub sysfs_class: PathBuf,
    /// Where their device nodes are.
    pub dev_root: PathBuf,
    /// Where the guest's secret is.
    pub secret_path: PathBuf,
    /// The socket the unprivileged process connects to.
    pub socket: PathBuf,
    /// The socket the clipboard daemon connects to.
    pub clipboard_socket: PathBuf,
    /// Where the kernel's uinput device is.
    pub uinput: PathBuf,
    /// Where the module publishes the mode it drives.
    pub mode: PathBuf,
    /// The user that process runs as, which is the only one let in.
    pub user: String,
    /// How long to wait for the card.
    pub device_deadline: Duration,
}

impl Options {
    /// The defaults, with the environment allowed to override each one.
    #[must_use]
    pub fn from_env() -> Self {
        let text =
            |name: &str, fallback: &str| env::var(name).unwrap_or_else(|_| fallback.to_owned());

        Self {
            driver: text("VMLORD_DISPLAY_DRIVER", DRIVER),
            sysfs_class: text("VMLORD_DISPLAY_SYSFS", DRM_CLASS).into(),
            dev_root: text("VMLORD_DISPLAY_DEV", DRM_DEVICES).into(),
            secret_path: text(
                "VMLORD_DISPLAY_SECRET",
                vmlord_agent_protocol::auth::GUEST_SECRET_PATH,
            )
            .into(),
            socket: text("VMLORD_DISPLAY_SOCKET", SOCKET_PATH).into(),
            clipboard_socket: text("VMLORD_DISPLAY_CLIPBOARD_SOCKET", CLIPBOARD_SOCKET_PATH).into(),
            uinput: text("VMLORD_DISPLAY_UINPUT", crate::uinput::DEVICE_PATH).into(),
            mode: text("VMLORD_DISPLAY_MODE", crate::output::MODE_PARAMETER).into(),
            user: text("VMLORD_DISPLAY_USER", SERVICE_USER),
            device_deadline: DEVICE_DEADLINE,
        }
    }
}

/// Waits for the card, since the module loads after this unit starts.
///
/// The wait is not a courtesy: falling into a restart while the card has not
/// appeared would spend the crash-loop budget on the ordinary state of a
/// booting guest. The backoff starts at 250 ms and doubles to a five-second
/// ceiling, so a module that loads quickly is noticed quickly and one that
/// never loads costs almost nothing to keep waiting for.
///
/// Generic over the attempt so it can be driven without a device.
///
/// # Errors
///
/// [`ErrorKind::TimedOut`] if the deadline passes with nothing found, or
/// whatever the attempt itself failed with.
pub fn wait_for_device<T>(
    deadline: Duration,
    mut attempt: impl FnMut() -> io::Result<Option<T>>,
) -> io::Result<T> {
    /// Where the backoff starts.
    const FIRST: Duration = Duration::from_millis(250);
    /// And where it stops growing.
    const CEILING: Duration = Duration::from_secs(5);

    let started = Instant::now();
    let mut wait = FIRST;
    loop {
        if let Some(found) = attempt()? {
            return Ok(found);
        }
        if started.elapsed() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                format!(
                    "no DRM device appeared within {} seconds; the guest module did not load",
                    deadline.as_secs()
                ),
            ));
        }

        thread::sleep(wait.min(deadline.saturating_sub(started.elapsed())));
        wait = (wait * 2).min(CEILING);
    }
}

/// What the three threads share.
#[derive(Default)]
struct BrokerState {
    /// The unprivileged process, if one is connected. One at a time: a second
    /// connection replaces the first, because there is one capture process and
    /// a stale one holding the socket would be a display nobody can restart.
    peer: Option<Arc<Connection>>,
    /// The session that is open, if one is.
    session: Option<SessionParameters>,
    /// The clipboard daemon, if one is connected. One at a time, like the
    /// capture peer: there is one graphical session on the screen.
    clipboard_peer: Option<Arc<Connection>>,
    /// What that daemon needs of the session that is open: the session id and
    /// the clipboard key, and neither of the other two keys.
    clipboard: Option<(Vec<u8>, Vec<u8>)>,
    /// Changes whenever the host session opens or closes, even though the
    /// long-lived session process remains the same peer.
    session_epoch: u64,
    /// Whether the peer has asked for a frame and not yet been given one.
    wants_frame: bool,
    /// The framebuffers this peer has already been sent a descriptor for.
    /// Cleared when the peer is replaced, since a new peer has none of them.
    sent: HashSet<u32>,
    /// A fault another thread found, for the control thread to report.
    fault: Option<String>,
    /// The size of the framebuffer capture last saw, once it has seen one.
    /// The one answer to "what is the output actually on", because it is the
    /// buffer that gets encoded rather than the mode that was asked for.
    geometry: Option<(u32, u32)>,
    /// Whether that size has moved since the control thread last read it.
    geometry_changed: bool,
    /// Whether the broker is on its way out.
    stopping: bool,
}

/// The shared state and the signal that it changed.
type Shared = Arc<(Mutex<BrokerState>, Condvar)>;

/// Runs the broker until it cannot.
///
/// # Panics
///
/// If a thread it spawned panicked, which is a bug rather than a state to
/// recover from.
#[must_use]
pub fn run(options: Options) -> std::process::ExitCode {
    match serve(&options) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vmlord-display-broker: {error}");

            std::process::ExitCode::FAILURE
        }
    }
}

/// The body of [`run`], so that every failure is one `?` away from a log line.
fn serve(options: &Options) -> io::Result<()> {
    let secret = at("reading the VM's secret", read_secret(&options.secret_path))?;

    let device = at(
        "waiting for the display device",
        wait_for_device(options.device_deadline, || {
            Device::find(&options.driver, &options.sysfs_class, &options.dev_root)
        }),
    )?;

    let (uid, gid) = at(
        "looking up the service account",
        service_account(&options.user),
    )?;
    if let Some(directory) = options.socket.parent() {
        at(
            "creating the socket's directory",
            std::fs::create_dir_all(directory),
        )?;
    }
    let listener = at(
        "binding the socket to the session process",
        Listener::bind(&options.socket, gid),
    )?;
    let clipboard_listener = at(
        "binding the socket to the clipboard daemon",
        Listener::bind_open(&options.clipboard_socket),
    )?;
    let control = at(
        "binding the control service",
        vsock::Listener::bind(CONTROL_PORT),
    )?;

    let shared: Shared = Arc::new((Mutex::new(BrokerState::default()), Condvar::new()));
    let output = Output::new(&options.mode);

    // Created once and held for the guest's lifetime: neither a rebound input
    // channel nor a whole new session makes the desktop see its keyboard
    // disconnect. A guest without them shows a read-only display.
    let devices = open_devices(&options.uinput);
    if devices.is_none() {
        let (lock, _) = &*shared;
        lock.lock()
            .expect("the broker's lock is not poisoned")
            .fault = Some("this guest has no input devices".to_owned());
    }

    thread::scope(|scope| {
        let ipc = {
            let shared = Arc::clone(&shared);
            let devices = devices.as_ref();
            scope.spawn(move || serve_peers(&listener, uid, &shared, devices))
        };
        let clipboard = {
            let shared = Arc::clone(&shared);
            scope.spawn(move || serve_clipboard_peers(&clipboard_listener, &shared))
        };
        let capture = {
            let shared = Arc::clone(&shared);
            scope.spawn(move || capture_frames(device, &shared))
        };
        serve_sessions(&control, &secret, &output, &shared);

        // Nothing above returns while the broker is healthy, so reaching here
        // means a thread has stopped and the others are owed the news.
        stop(&shared);
        let _ = ipc.join();
        let _ = clipboard.join();
        let _ = capture.join();
    });

    // A fault is why this broker is no longer able to show anything, and
    // exiting with it is what asks systemd for a fresh one. Returning success
    // here would leave `Restart=on-failure` with nothing to act on.
    match take_fault(&shared) {
        Some(reason) => Err(io::Error::other(reason)),
        None => Ok(()),
    }
}

/// Names the step an error came from.
///
/// Every failure in `serve` is one `?` from the single line this unit writes
/// before it exits, and a bare `Operation not permitted` there names neither
/// the call that was refused nor the privilege it wanted. The step is what
/// turns that line into a diagnosis.
fn at<T>(step: &'static str, outcome: io::Result<T>) -> io::Result<T> {
    outcome.map_err(|error| io::Error::new(error.kind(), format!("{step}: {error}")))
}

/// The two input devices, or nothing at all.
///
/// A failure here degrades the display rather than breaking the VM, which is
/// the rule #114 set for the DRM side: a desktop with no keyboard is worth
/// more than a VM that refused to show one.
fn open_devices(path: &std::path::Path) -> Option<(OwnedFd, OwnedFd)> {
    match crate::uinput::create(path) {
        Ok(devices) => Some(devices),
        Err(error) => {
            eprintln!(
                "vmlord-display-broker: this guest has no input devices ({error}); the display will be read-only"
            );

            None
        }
    }
}

/// Reads the guest's secret, which is the one thing this process holds that the
/// other must never see.
fn read_secret(path: &std::path::Path) -> io::Result<Secret> {
    let text = std::fs::read_to_string(path)?;

    Secret::from_base64(text.trim())
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))
}

/// The uid and gid of the user the session process runs as.
fn service_account(user: &str) -> io::Result<(libc::uid_t, libc::gid_t)> {
    let name = std::ffi::CString::new(user)
        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
    // SAFETY: `name` is a NUL-terminated string that lives across the call, and
    // the returned pointer is only read before the next call to `getpwnam`.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!("there is no {user} account for the display session to run as"),
        ));
    }

    // SAFETY: `getpwnam` returned a live `passwd` this thread has not yet
    // invalidated.
    let entry = unsafe { &*entry };

    Ok((entry.pw_uid, entry.pw_gid))
}

/// Accepts the unprivileged process and reads what it asks for.
fn serve_peers(
    listener: &Listener,
    expected_uid: libc::uid_t,
    shared: &Shared,
    devices: Option<&(OwnedFd, OwnedFd)>,
) {
    loop {
        if stopping(shared) {
            return;
        }

        let connection = match listener.accept(expected_uid) {
            Ok(connection) => Arc::new(connection),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                eprintln!("vmlord-display-broker: refused a peer: {error}");
                continue;
            }
            Err(error) => {
                eprintln!("vmlord-display-broker: the broker socket failed: {error}");
                return;
            }
        };

        adopt_peer(shared, &connection, devices);
        read_peer(&connection, shared, devices);
    }
}

/// Makes a new connection the peer, and tells it what it missed.
fn adopt_peer(shared: &Shared, connection: &Arc<Connection>, devices: Option<&(OwnedFd, OwnedFd)>) {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    state.peer = Some(Arc::clone(connection));
    // A new peer holds none of the descriptors the last one was sent, and has
    // asked for nothing yet.
    state.sent.clear();
    state.wants_frame = false;

    // The devices before the session: a peer that learned its parameters first
    // would drop the input records it read before they arrived.
    send_devices(connection, devices);
    if let Some(parameters) = state.session.clone() {
        let _ = connection.send(&Message::SessionOpened(parameters), &[]);
    }
    signal.notify_all();
}

/// Reads one peer until it goes away.
fn read_peer(connection: &Arc<Connection>, shared: &Shared, devices: Option<&(OwnedFd, OwnedFd)>) {
    loop {
        let Ok((message, _)) = connection.receive() else {
            return;
        };

        let (lock, signal) = &**shared;
        let mut state = lock.lock().expect("the broker's lock is not poisoned");
        // A peer that has already been replaced is one whose requests are
        // answers to a session that is gone.
        if !state
            .peer
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection))
        {
            return;
        }

        match message {
            Message::Attach => {
                send_devices(connection, devices);
                if let Some(parameters) = state.session.clone() {
                    let _ = connection.send(&Message::SessionOpened(parameters), &[]);
                }
            }
            Message::NextFrame => {
                state.wants_frame = true;
                signal.notify_all();
            }
            Message::Report { detail } => {
                eprintln!("vmlord-display-session: {detail}");
                state.fault = Some(detail);
            }
            // Everything else on this socket is the broker's to send, not the
            // peer's, and a peer that sends one is confused rather than hostile.
            other => eprintln!("vmlord-display-broker: ignoring {other:?} from the peer"),
        }
    }
}

/// Hands the peer the two devices, if this guest has any.
fn send_devices(connection: &Connection, devices: Option<&(OwnedFd, OwnedFd)>) {
    if let Some((keyboard, pointer)) = devices {
        let _ = connection.send(&Message::InputDevices, &[keyboard.as_fd(), pointer.as_fd()]);
    }
}

/// Accepts one control connection at a time and runs its session.
fn serve_sessions(listener: &vsock::Listener, secret: &Secret, output: &Output, shared: &Shared) {
    loop {
        if stopping(shared) {
            return;
        }

        let mut stream = match listener.accept() {
            Ok(stream) => stream,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("vmlord-display-broker: the control listener failed: {error}");
                return;
            }
        };
        let _ = stream.set_read_timeout(CONTROL_IDLE);

        // What the module says it drives, rather than a constant: a guest
        // whose agent wrote a mode into modprobe.d comes up on it, and a
        // session opened after a resize starts where the last one left off.
        let (width, height) = geometry(output, shared);
        let mut control = Control::new(secret, support_from(width, height));
        let reason = run_session(&mut control, &mut stream, output, shared);

        close_session(shared, &reason);
    }
}

/// One session, from the first record to the reason it ended.
fn run_session(
    control: &mut Control,
    stream: &mut vsock::Stream,
    output: &Output,
    shared: &Shared,
) -> String {
    loop {
        if stopping(shared) {
            return "the broker is shutting down".to_owned();
        }

        // A mode the compositor has actually committed, seen as a framebuffer
        // of a new size. The capture process has already been told by the
        // thread that saw it; what is owed here is the host's own record, so
        // that a viewer knows the size it asked for is the size it has.
        if let Some((width, height)) = take_geometry_change(shared)
            && (width, height) != control.geometry()
        {
            eprintln!("vmlord-display-broker: the output came up at {width}x{height}");
            control.set_geometry(width, height);
            control.state(stream);
        }

        // A fault another thread found is reported on the one socket the host
        // is listening to, and ends the session: a display that cannot be
        // captured is not one to keep a viewer waiting on.
        if let Some(detail) = take_fault(shared) {
            control.report(stream, ErrorCode::CaptureFailed, &detail);

            return detail;
        }

        match control.pump(stream) {
            Outcome::Opened(parameters, clipboard_key) => {
                open_session(shared, parameters, clipboard_key);
            }
            Outcome::Relay(message) => send_to_peer(shared, &message),
            Outcome::Resize { width, height } => {
                request_mode(output, control, stream, width, height)
            }
            Outcome::Closed(reason) => return reason,
            Outcome::Nothing => {}
        }
    }
}

/// Records the session and hands its keys to the peer.
fn open_session(shared: &Shared, parameters: SessionParameters, clipboard_key: Vec<u8>) {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    state.session_epoch = state.session_epoch.wrapping_add(1);
    let session_id = parameters.session_id.clone();
    state.session = Some(parameters.clone());
    state.clipboard = Some((session_id.clone(), clipboard_key.clone()));
    state.sent.clear();
    state.wants_frame = false;
    if let Some(peer) = state.peer.clone() {
        let _ = peer.send(&Message::SessionOpened(parameters), &[]);
    }
    if let Some(peer) = state.clipboard_peer.clone() {
        let _ = peer.send(
            &Message::ClipboardOpened {
                session_id,
                clipboard_key,
            },
            &[],
        );
    }
    signal.notify_all();
}

/// Ends the session and stops capture with it.
fn close_session(shared: &Shared, reason: &str) {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    state.session_epoch = state.session_epoch.wrapping_add(1);
    if state.session.take().is_some() {
        eprintln!("vmlord-display-broker: the session ended: {reason}");
    }
    state.wants_frame = false;
    state.sent.clear();
    state.clipboard = None;
    for peer in [state.peer.clone(), state.clipboard_peer.clone()]
        .into_iter()
        .flatten()
    {
        let _ = peer.send(
            &Message::SessionClosed {
                reason: reason.to_owned(),
            },
            &[],
        );
    }
    signal.notify_all();
}

/// Accepts the clipboard daemon of the session that is on screen, and no other.
///
/// The uid is looked up at every accept rather than once at start-up: the
/// person at the screen is not decided when the broker starts, and a daemon
/// left running by a user who has since been switched away stops being
/// authorised without anything having to notice and evict it.
fn serve_clipboard_peers(listener: &Listener, shared: &Shared) {
    loop {
        if stopping(shared) {
            return;
        }

        let connection = match listener.accept_where(
            |uid| crate::seat::active_graphical_uid() == Some(uid),
            "the graphical session on seat0",
        ) {
            Ok(connection) => Arc::new(connection),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                eprintln!("vmlord-display-broker: refused a clipboard peer: {error}");
                continue;
            }
            Err(error) => {
                eprintln!("vmlord-display-broker: the clipboard socket failed: {error}");
                return;
            }
        };

        adopt_clipboard_peer(shared, &connection);
        read_clipboard_peer(&connection, shared);
    }
}

/// Makes a new connection the clipboard peer, and tells it what it missed.
fn adopt_clipboard_peer(shared: &Shared, connection: &Arc<Connection>) {
    let (lock, _) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    state.clipboard_peer = Some(Arc::clone(connection));
    send_clipboard_session(connection, state.clipboard.as_ref());
}

/// Reads the clipboard peer until it goes away.
fn read_clipboard_peer(connection: &Arc<Connection>, shared: &Shared) {
    loop {
        let Ok((message, _)) = connection.receive() else {
            return;
        };

        let (lock, _) = &**shared;
        let state = lock.lock().expect("the broker's lock is not poisoned");
        if !state
            .clipboard_peer
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection))
        {
            return;
        }

        match message {
            Message::Attach => send_clipboard_session(connection, state.clipboard.as_ref()),
            // Never the contents of a selection: the daemon reports what went
            // wrong, and what went wrong is a mime type and a reason.
            Message::Report { detail } => {
                eprintln!("vmlord-display-clipboard: {detail}");
            }
            other => eprintln!("vmlord-display-broker: ignoring {other:?} from the clipboard peer"),
        }
    }
}

/// Sends the session a clipboard daemon needs, if there is one open.
fn send_clipboard_session(connection: &Arc<Connection>, clipboard: Option<&(Vec<u8>, Vec<u8>)>) {
    if let Some((session_id, clipboard_key)) = clipboard {
        let _ = connection.send(
            &Message::ClipboardOpened {
                session_id: session_id.clone(),
                clipboard_key: clipboard_key.clone(),
            },
            &[],
        );
    }
}

/// Passes a message straight to the peer, if there is one.
fn send_to_peer(shared: &Shared, message: &Message) {
    let (lock, _) = &**shared;
    let peer = lock
        .lock()
        .expect("the broker's lock is not poisoned")
        .peer
        .clone();

    if let Some(peer) = peer {
        let _ = peer.send(message, &[]);
    }
}

/// Reads the planes at every vblank a frame was asked for.
fn capture_frames(mut device: Device, shared: &Shared) {
    // How many times in a row the clock has answered "this output is not
    // being driven". Kept to pace the retry and to say so once rather than
    // sixty times a second.
    let mut unlit: u32 = 0;
    let mut generations = SnapshotGenerations::default();
    let mut requested_by = None;
    let mut pacing = CapturePacing::default();

    loop {
        if requested_by.is_none() {
            requested_by = wait_for_request(shared);
            pacing.requested();
        }
        let Some(request) = requested_by.as_ref() else {
            return;
        };
        if !request_is_current(shared, request) {
            requested_by = None;
            generations = SnapshotGenerations::default();
            continue;
        }
        let peer = &request.peer;

        if pacing.wait_before_probe() {
            if let Err(error) = device.wait_vblank() {
                if error.kind() == ErrorKind::Interrupted {
                    continue;
                }
                // `EINVAL` is what the kernel answers while this output's vblank
                // is off: no compositor has lit it yet, or the desktop has
                // blanked. Neither is a fault -- there is simply nothing to
                // capture this instant -- and ending the session over it would
                // close a window that a moving mouse is about to fill.
                if error.raw_os_error() == Some(libc::EINVAL) {
                    idle_until_the_output_is_lit(&mut unlit);
                    continue;
                }
                fault(shared, &format!("the output's clock failed: {error}"));
                stop(shared);

                return;
            }
            pacing.vblank_seen();
            if unlit > 0 {
                eprintln!(
                    "vmlord-display-broker: the output is lit again after {unlit} attempts at its clock"
                );
                unlit = 0;
            }
        }

        let snapshot = match device.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                fault(
                    shared,
                    &format!("reading the output's planes failed: {error}"),
                );
                // And the whole broker with it. A capture thread that has
                // returned leaves a process that still answers on the control
                // port and can never fill a session again: every Connect after
                // it opens a window that stays black. Ending here hands the
                // restart to systemd, which is what `Restart=on-failure` and
                // the crash-loop budget beside it are for.
                stop(shared);

                return;
            }
        };

        if !snapshot
            .planes
            .iter()
            .any(|plane| plane.kind == PlaneKind::Primary)
            || !generations.advanced(&snapshot.planes, snapshot.generation_supported)
        {
            pacing.unchanged();
            continue;
        }

        // The primary plane's framebuffer is the output's real size, and the
        // only place it is read from: a mode that was asked for is not one
        // the compositor is obliged to have committed.
        //
        // The peer is told here rather than by the control thread, and ahead
        // of the snapshot that carries the first buffer of the new size: its
        // encoder is built on a geometry, and a frame of another shape is one
        // it has to drop.
        if let Some(primary) = snapshot
            .planes
            .iter()
            .find(|plane| plane.kind == PlaneKind::Primary)
            && observe_geometry(shared, primary.width, primary.height)
        {
            let _ = peer.send(
                &Message::Geometry {
                    width: primary.width,
                    height: primary.height,
                },
                &[],
            );
        }

        if send_snapshot(&device, request, shared, &snapshot.planes) {
            generations.observe(&snapshot.planes, snapshot.generation_supported);
        } else {
            generations = SnapshotGenerations::default();
        }
        requested_by = None;
    }
}

/// Avoids adding a whole refresh interval after the session finishes a frame.
///
/// A fresh request first probes the coherent DRM snapshot: a commit may have
/// arrived while the previous frame was encoded and sent. Only an unchanged
/// probe waits for another vblank before trying again.
#[derive(Default)]
struct CapturePacing {
    wait_before_probe: bool,
}

impl CapturePacing {
    fn requested(&mut self) {
        self.wait_before_probe = false;
    }

    fn unchanged(&mut self) {
        self.wait_before_probe = true;
    }

    fn vblank_seen(&mut self) {
        self.wait_before_probe = false;
    }

    fn wait_before_probe(&self) -> bool {
        self.wait_before_probe
    }
}

/// The last driver commits capture handed to the session process.
///
/// Missing cursor is state in its own right. A missing generation, on the
/// other hand, means an older module is loaded, so every vblank remains a
/// candidate exactly as it was before generation reporting existed.
#[derive(Default)]
struct SnapshotGenerations {
    initialized: bool,
    primary: Option<u64>,
    cursor: Option<u64>,
}

impl SnapshotGenerations {
    fn advanced(&self, planes: &[PlaneState], generation_supported: bool) -> bool {
        if !generation_supported {
            return true;
        }

        let primary = planes.iter().find(|plane| plane.kind == PlaneKind::Primary);
        let cursor = planes.iter().find(|plane| plane.kind == PlaneKind::Cursor);

        let current_primary = primary.and_then(|plane| plane.generation);
        let current_cursor = cursor.and_then(|plane| plane.generation);
        !self.initialized || self.primary != current_primary || self.cursor != current_cursor
    }

    fn observe(&mut self, planes: &[PlaneState], generation_supported: bool) {
        if !generation_supported {
            return;
        }

        self.initialized = true;
        self.primary = planes
            .iter()
            .find(|plane| plane.kind == PlaneKind::Primary)
            .and_then(|plane| plane.generation);
        self.cursor = planes
            .iter()
            .find(|plane| plane.kind == PlaneKind::Cursor)
            .and_then(|plane| plane.generation);
    }
}

/// Whether a pending request still belongs to the active session process.
fn request_is_current(shared: &Shared, request: &PendingRequest) -> bool {
    let (lock, _) = &**shared;
    let state = lock.lock().expect("the broker's lock is not poisoned");

    state.session.is_some()
        && state.session_epoch == request.session_epoch
        && state
            .peer
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &request.peer))
}

/// Waits out an output nothing is driving, and says so the first time.
///
/// A desktop that has not been lit yet and one that has blanked look the same
/// from here, and both end when something commits a mode -- which is an event
/// this process does not get told about, so it asks again. The pace is a
/// frame's worth of time: often enough that the picture returns immediately,
/// rarely enough that an unlit guest costs nothing.
fn idle_until_the_output_is_lit(attempts: &mut u32) {
    if *attempts == 0 {
        eprintln!(
            "vmlord-display-broker: this output has no clock yet -- nothing has lit it.              Waiting; the desktop's compositor is what turns it on."
        );
    }
    *attempts = attempts.saturating_add(1);
    thread::sleep(UNLIT_POLL);
}

/// How long the capture loop rests while nothing is driving the output.
const UNLIT_POLL: Duration = Duration::from_millis(100);

/// Waits until a frame is wanted, and says who wants it.
///
/// `None` means the broker is stopping, which is the only way out.
struct PendingRequest {
    peer: Arc<Connection>,
    session_epoch: u64,
}

fn wait_for_request(shared: &Shared) -> Option<PendingRequest> {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    loop {
        if state.stopping {
            return None;
        }
        if state.session.is_some()
            && state.wants_frame
            && let Some(peer) = state.peer.clone()
        {
            state.wants_frame = false;

            return Some(PendingRequest {
                peer,
                session_epoch: state.session_epoch,
            });
        }

        state = signal
            .wait(state)
            .expect("the broker's lock is not poisoned");
    }
}

/// Sends one vblank's planes, with the descriptors this peer has not been sent.
fn send_snapshot(
    device: &Device,
    request: &PendingRequest,
    shared: &Shared,
    planes: &[PlaneState],
) -> bool {
    let (lock, _) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");
    if state.session.is_none()
        || state.session_epoch != request.session_epoch
        || !state
            .peer
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &request.peer))
    {
        return false;
    }

    let mut layouts = Vec::with_capacity(planes.len());
    let mut new_buffers = Vec::new();
    let mut descriptors = Vec::new();
    for plane in planes {
        layouts.push(PlaneLayout {
            kind: plane.kind,
            buffer: u64::from(plane.fb_id),
            width: plane.width,
            height: plane.height,
            stride: plane.stride,
            format: plane.format,
            x: plane.x,
            y: plane.y,
        });

        if !owes_descriptor(plane, &state.sent) {
            continue;
        }
        let Some(buffer) = device.buffer(plane.fb_id) else {
            continue;
        };
        new_buffers.push(u64::from(plane.fb_id));
        descriptors.push(buffer);
    }

    let message = Message::Snapshot {
        sequence: 0,
        planes: layouts,
        new_buffers: new_buffers.clone(),
    };
    if request.peer.send(&message, &descriptors).is_ok() {
        state.sent.extend(new_buffers.iter().map(|id| *id as u32));
        true
    } else {
        false
    }
}

/// Whether this plane's buffer has to cross to the peer now.
///
/// A descriptor costs a syscall and a slot in the peer's table, so each buffer
/// crosses once and is named by its framebuffer id thereafter. The kernel
/// hands those ids out again, though: a framebuffer that is destroyed leaves
/// its number for the next one, and the peer would go on reading the buffer
/// that number used to mean. Task #121 watched a login do exactly that -- the
/// greeter's cursor gave its id to the new session's desktop, and the picture
/// froze on the last frame the greeter drew. So a plane whose buffer is not
/// the one that id named before is sent again, whatever the peer holds.
fn owes_descriptor(plane: &PlaneState, sent: &HashSet<u32>) -> bool {
    plane.fresh || !sent.contains(&plane.fb_id)
}

/// The geometry a session opening now is offered.
///
/// What capture has seen if it has seen anything, and what the module says it
/// drives otherwise -- which is the case for the first session of a guest,
/// where nothing has been captured yet.
fn geometry(output: &Output, shared: &Shared) -> (u32, u32) {
    let (lock, _) = &**shared;
    let seen = lock
        .lock()
        .expect("the broker's lock is not poisoned")
        .geometry;

    seen.unwrap_or_else(|| output.current())
}

/// Asks the module for a mode, and says so on the socket if it will not take it.
///
/// Nothing is reported to the host on success. A write that the module took is
/// a hotplug, not a mode: the compositor commits one, and what it committed
/// arrives as a framebuffer of a new size.
fn request_mode(
    output: &Output,
    control: &mut Control,
    stream: &mut vsock::Stream,
    width: u32,
    height: u32,
) {
    if let Err(error) = output.request(width, height) {
        eprintln!(
            "vmlord-display-broker: {} would not take {width}x{height}: {error}",
            output.path().display()
        );
        control.report(
            stream,
            ErrorCode::ResolutionRejected,
            "the output refused the mode",
        );
    }
}

/// Records the size capture last saw, and says whether it moved.
fn observe_geometry(shared: &Shared, width: u32, height: u32) -> bool {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    if state.geometry == Some((width, height)) {
        return false;
    }
    state.geometry = Some((width, height));
    // For the control thread, which owes the host a `DisplayState` for it.
    state.geometry_changed = true;
    signal.notify_all();

    true
}

/// The size capture saw, if it has moved since this was last called.
fn take_geometry_change(shared: &Shared) -> Option<(u32, u32)> {
    let (lock, _) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    if !state.geometry_changed {
        return None;
    }
    state.geometry_changed = false;

    state.geometry
}

/// Records a fault for the control thread to report, and stops capture.
fn fault(shared: &Shared, detail: &str) {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    eprintln!("vmlord-display-broker: {detail}");
    state.fault = Some(detail.to_owned());
    state.wants_frame = false;
    signal.notify_all();
}

/// Takes the fault another thread left, if there is one.
fn take_fault(shared: &Shared) -> Option<String> {
    let (lock, _) = &**shared;

    lock.lock()
        .expect("the broker's lock is not poisoned")
        .fault
        .take()
}

/// Whether the broker is on its way out.
fn stopping(shared: &Shared) -> bool {
    let (lock, _) = &**shared;

    lock.lock()
        .expect("the broker's lock is not poisoned")
        .stopping
}

/// Tells every thread to stop and wakes the one that is waiting.
fn stop(shared: &Shared) {
    let (lock, signal) = &**shared;
    lock.lock()
        .expect("the broker's lock is not poisoned")
        .stopping = true;
    signal.notify_all();
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        io,
        sync::{Arc, Condvar, Mutex},
        time::Duration,
    };

    use super::{SOCKET_PATH, wait_for_device};

    /// The unit that ships in the payload, as the guest will read it.
    const BROKER_UNIT: &str =
        include_str!("../../../payloads/display/services/vmlord-display-broker.service");

    /// A plane over a framebuffer id, with everything else out of the way.
    fn plane(fb_id: u32, fresh: bool) -> super::PlaneState {
        super::PlaneState {
            kind: crate::ipc::PlaneKind::Primary,
            fb_id,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            format: 0,
            x: 0,
            y: 0,
            fresh,
            generation: None,
        }
    }

    fn generated_plane(kind: crate::ipc::PlaneKind, generation: Option<u64>) -> super::PlaneState {
        super::PlaneState {
            kind,
            generation,
            ..plane(7, false)
        }
    }

    #[test]
    fn only_a_new_plane_generation_is_captured() {
        let mut seen = super::SnapshotGenerations::default();
        let first = [generated_plane(crate::ipc::PlaneKind::Primary, Some(10))];
        let same = [generated_plane(crate::ipc::PlaneKind::Primary, Some(10))];
        let next = [generated_plane(crate::ipc::PlaneKind::Primary, Some(11))];

        assert!(seen.advanced(&first, true));
        seen.observe(&first, true);
        assert!(!seen.advanced(&same, true));
        assert!(seen.advanced(&next, true));
    }

    #[test]
    fn a_cursor_disappearing_is_a_new_snapshot() {
        let mut seen = super::SnapshotGenerations::default();
        let with_cursor = [
            generated_plane(crate::ipc::PlaneKind::Primary, Some(10)),
            generated_plane(crate::ipc::PlaneKind::Cursor, Some(20)),
        ];
        let without_cursor = [generated_plane(crate::ipc::PlaneKind::Primary, Some(10))];

        assert!(seen.advanced(&with_cursor, true));
        seen.observe(&with_cursor, true);
        assert!(seen.advanced(&without_cursor, true));
    }

    #[test]
    fn a_module_without_generations_keeps_the_compatible_capture_path() {
        let mut seen = super::SnapshotGenerations::default();
        let legacy = [generated_plane(crate::ipc::PlaneKind::Primary, None)];

        assert!(seen.advanced(&legacy, false));
        seen.observe(&legacy, false);
        assert!(seen.advanced(&legacy, false));
        assert!(seen.advanced(&[], false));
    }

    #[test]
    fn a_generation_is_not_consumed_until_delivery_succeeds() {
        let seen = super::SnapshotGenerations::default();
        let frame = [generated_plane(crate::ipc::PlaneKind::Primary, Some(10))];

        assert!(seen.advanced(&frame, true));
        assert!(seen.advanced(&frame, true));
    }

    #[test]
    fn reopening_on_the_same_peer_changes_the_session_epoch() {
        let shared: super::Shared =
            Arc::new((Mutex::new(super::BrokerState::default()), Condvar::new()));
        let parameters = crate::ipc::SessionParameters {
            session_id: vec![1; 16],
            frame_key: vec![2; 32],
            input_key: vec![3; 32],
            width: 1920,
            height: 1080,
            tile_size: 32,
            cursor_stream: true,
        };

        super::open_session(&shared, parameters.clone(), vec![4; 32]);
        let first = shared.0.lock().unwrap().session_epoch;
        super::close_session(&shared, "test transition");
        super::open_session(&shared, parameters, vec![4; 32]);
        let second = shared.0.lock().unwrap().session_epoch;

        assert_ne!(first, second);
    }

    #[test]
    fn a_new_request_probes_before_waiting_for_another_vblank() {
        let mut pacing = super::CapturePacing::default();

        pacing.requested();
        assert!(!pacing.wait_before_probe());

        pacing.unchanged();
        assert!(pacing.wait_before_probe());

        pacing.vblank_seen();
        assert!(!pacing.wait_before_probe());
    }

    #[test]
    fn a_framebuffer_id_that_now_names_another_buffer_is_sent_again() {
        // The kernel hands framebuffer ids out again. A login does it: the
        // greeter's cursor is destroyed and the new session's desktop takes
        // its number. A peer told once and never again goes on reading the
        // cursor -- which is what froze the picture on the last frame the
        // greeter drew.
        let sent = std::collections::HashSet::from([7]);

        assert!(
            super::owes_descriptor(&plane(7, true), &sent),
            "the id is the same and the buffer is not, so the peer holds the wrong one"
        );
        assert!(
            !super::owes_descriptor(&plane(7, false), &sent),
            "the same buffer under the same id crosses once"
        );
        assert!(
            super::owes_descriptor(&plane(9, false), &sent),
            "a buffer the peer has never been sent has to cross"
        );
    }

    #[test]
    fn a_startup_failure_names_the_step_it_failed_at() {
        let refused = super::at(
            "binding the socket to the session process",
            Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied)),
        )
        .expect_err("what was passed in");

        assert!(
            refused.to_string().contains("binding the socket"),
            "a bare `Operation not permitted` names neither the call nor the privilege"
        );
        assert_eq!(
            refused.kind(),
            io::ErrorKind::PermissionDenied,
            "the kind survives, because a caller may still branch on it"
        );
    }

    #[test]
    fn the_unit_creates_the_directory_the_socket_lives_in() {
        // systemd sets up the mount namespace before ExecStart, so a directory
        // named there has to exist before this process could create it -- and
        // `bind` would fail on a missing parent even if the namespace were set
        // up. `RuntimeDirectory=` is what creates it, early enough for both.
        let directory = std::path::Path::new(SOCKET_PATH)
            .parent()
            .expect("the socket has a directory")
            .file_name()
            .expect("that directory has a name")
            .to_str()
            .expect("a name this repository wrote");

        assert!(
            BROKER_UNIT.contains(&format!("RuntimeDirectory={directory}")),
            "the unit must create /run/{directory} rather than assume it"
        );
        assert!(
            !BROKER_UNIT.contains(&format!("ReadWritePaths=/run/{directory}")),
            "a ReadWritePaths on a directory nothing creates is what fails at NAMESPACE;              RuntimeDirectory already makes it writable"
        );
    }

    #[test]
    fn the_unit_grants_the_capability_the_socket_is_handed_over_with() {
        // The socket is bound by root and read by the service user, so `bind`
        // gives it that user's group -- and changing a file's group to one the
        // caller does not belong to is `CAP_CHOWN`, which root does not have
        // once the bounding set has taken it away. Without it the broker exits
        // with EPERM before it has listened for anything.
        assert!(
            BROKER_UNIT
                .lines()
                .find(|line| line.starts_with("CapabilityBoundingSet="))
                .is_some_and(|line| line.contains("CAP_CHOWN")),
            "the broker chowns its socket, so the bounding set has to allow it"
        );
    }

    #[test]
    fn the_crash_loop_budget_is_where_systemd_reads_it() {
        // Under [Service] systemd answers these with `Unknown key ...,
        // ignoring`: a rate limit that is silently not one, and a unit that
        // restarts forever on a fault nobody is told about.
        const SESSION_UNIT: &str =
            include_str!("../../../payloads/display/services/vmlord-display-session.service");

        for unit in [BROKER_UNIT, SESSION_UNIT] {
            // Split on the section header itself: a comment may well mention
            // the name of the section it is warning about.
            let service = unit
                .split_once("\n[Service]\n")
                .expect("every unit has a service section")
                .1;

            assert!(unit.contains("StartLimitIntervalSec="));
            assert!(
                !service.contains("StartLimit"),
                "the crash-loop budget belongs to [Unit]"
            );
        }
    }

    #[test]
    fn a_broker_restart_does_not_take_the_directory_from_the_session() {
        // The capture process holds a namespace over the same directory and
        // reconnects through the same path. A runtime directory removed on
        // every restart would fail its namespace setup instead.
        assert!(BROKER_UNIT.contains("RuntimeDirectoryPreserve=yes"));
    }

    #[test]
    fn a_device_that_is_not_there_yet_is_waited_for_rather_than_failed_on() {
        // The module loads after this unit starts, every time. Falling into a
        // restart while the card has not appeared would spend the crash-loop
        // budget on the ordinary state of a booting guest.
        let attempts = Cell::new(0);
        let device = wait_for_device(Duration::from_secs(5), || {
            attempts.set(attempts.get() + 1);
            Ok((attempts.get() >= 3).then_some(()))
        });

        assert!(device.is_ok());
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn a_device_that_never_appears_ends_the_wait_with_the_reason() {
        let error = wait_for_device(Duration::from_millis(200), || {
            Ok::<Option<()>, io::Error>(None)
        })
        .expect_err("a guest whose module never loaded has no display");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn a_broker_with_no_uinput_carries_on_without_input() {
        // A guest whose kernel has no uinput still shows a desktop. What it
        // must not do is fail to start one.
        let devices = super::open_devices(std::path::Path::new("/nonexistent/uinput"));

        assert!(devices.is_none());
    }
}
