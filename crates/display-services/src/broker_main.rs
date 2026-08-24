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
    let secret = read_secret(&options.secret_path)?;

    let device = wait_for_device(options.device_deadline, || {
        Device::find(&options.driver, &options.sysfs_class, &options.dev_root)
    })?;

    let (uid, gid) = service_account(&options.user)?;
    if let Some(directory) = options.socket.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let listener = Listener::bind(&options.socket, gid)?;
    let control = vsock::Listener::bind(CONTROL_PORT)?;

    let shared: Shared = Arc::new((Mutex::new(BrokerState::default()), Condvar::new()));
    let output = Output::new(&options.mode);

    // Created once and held for the guest's lifetime: neither a rebound input
    // channel nor a whole new session makes the desktop see its keyboard
    // disconnect. A guest without them shows a read-only display.
    let devices = open_devices(&options.uinput);
    if devices.is_none() {
        let (lock, _) = &*shared;
        lock.lock().expect("the broker's lock is not poisoned").fault =
            Some("this guest has no input devices".to_owned());
    }

    thread::scope(|scope| {
        let ipc = {
            let shared = Arc::clone(&shared);
            let devices = devices.as_ref();
            scope.spawn(move || serve_peers(&listener, uid, &shared, devices))
        };
        let capture = {
            let shared = Arc::clone(&shared);
            scope.spawn(move || capture_frames(device, &shared))
        };
        serve_sessions(&control, &secret, &output, &shared);

        // Nothing above returns while the broker is healthy, so reaching here
        // means the control listener failed and the others are owed the news.
        stop(&shared);
        let _ = ipc.join();
        let _ = capture.join();
    });

    Ok(())
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
fn adopt_peer(
    shared: &Shared,
    connection: &Arc<Connection>,
    devices: Option<&(OwnedFd, OwnedFd)>,
) {
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
fn read_peer(
    connection: &Arc<Connection>,
    shared: &Shared,
    devices: Option<&(OwnedFd, OwnedFd)>,
) {
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
fn serve_sessions(
    listener: &vsock::Listener,
    secret: &Secret,
    output: &Output,
    shared: &Shared,
) {
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
            Outcome::Opened(parameters) => open_session(shared, parameters),
            Outcome::Relay(message) => send_to_peer(shared, &message),
            Outcome::Resize { width, height } => request_mode(output, control, stream, width, height),
            Outcome::Closed(reason) => return reason,
            Outcome::Nothing => {}
        }
    }
}

/// Records the session and hands its keys to the peer.
fn open_session(shared: &Shared, parameters: SessionParameters) {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    state.session = Some(parameters.clone());
    state.sent.clear();
    state.wants_frame = false;
    if let Some(peer) = state.peer.clone() {
        let _ = peer.send(&Message::SessionOpened(parameters), &[]);
    }
    signal.notify_all();
}

/// Ends the session and stops capture with it.
fn close_session(shared: &Shared, reason: &str) {
    let (lock, signal) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

    if state.session.take().is_some() {
        eprintln!("vmlord-display-broker: the session ended: {reason}");
    }
    state.wants_frame = false;
    state.sent.clear();
    if let Some(peer) = state.peer.clone() {
        let _ = peer.send(
            &Message::SessionClosed {
                reason: reason.to_owned(),
            },
            &[],
        );
    }
    signal.notify_all();
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
    loop {
        let Some(peer) = wait_for_request(shared) else {
            return;
        };

        if let Err(error) = device.wait_vblank() {
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            fault(shared, &format!("the output's clock failed: {error}"));

            return;
        }

        let planes = match device.snapshot() {
            Ok(planes) => planes,
            Err(error) => {
                fault(
                    shared,
                    &format!("reading the output's planes failed: {error}"),
                );

                return;
            }
        };

        // The primary plane's framebuffer is the output's real size, and the
        // only place it is read from: a mode that was asked for is not one
        // the compositor is obliged to have committed.
        //
        // The peer is told here rather than by the control thread, and ahead
        // of the snapshot that carries the first buffer of the new size: its
        // encoder is built on a geometry, and a frame of another shape is one
        // it has to drop.
        if let Some(primary) = planes.iter().find(|plane| plane.kind == PlaneKind::Primary)
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

        send_snapshot(&device, &peer, shared, &planes);
    }
}

/// Waits until a frame is wanted, and says who wants it.
///
/// `None` means the broker is stopping, which is the only way out.
fn wait_for_request(shared: &Shared) -> Option<Arc<Connection>> {
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

            return Some(peer);
        }

        state = signal
            .wait(state)
            .expect("the broker's lock is not poisoned");
    }
}

/// Sends one vblank's planes, with the descriptors this peer has not been sent.
fn send_snapshot(device: &Device, peer: &Arc<Connection>, shared: &Shared, planes: &[PlaneState]) {
    let (lock, _) = &**shared;
    let mut state = lock.lock().expect("the broker's lock is not poisoned");

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

        // A descriptor costs a syscall and a slot in the peer's table, so each
        // buffer crosses once and is named by its id thereafter.
        if state.sent.contains(&plane.fb_id) {
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
    if peer.send(&message, &descriptors).is_ok() {
        state.sent.extend(new_buffers.iter().map(|id| *id as u32));
    }
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
    use std::{cell::Cell, io, time::Duration};

    use super::{SOCKET_PATH, wait_for_device};

    /// The unit that ships in the payload, as the guest will read it.
    const BROKER_UNIT: &str =
        include_str!("../../../payloads/display/services/vmlord-display-broker.service");

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
