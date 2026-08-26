//! The window VMLord opens on a VM's display.
//!
//! Nothing is decided here. The threads are chosen, wired to each other, and
//! the message pump is run; every rule the viewer follows lives in the library
//! beside this file.
//!
//! What each thread owns:
//!
//! * the **main** thread owns the window, the renderer and the status machine,
//!   and never blocks -- which is what keeps the buttons on a `Failed` screen
//!   alive;
//! * the **reader** thread owns standard input, turning launch messages into
//!   channel sends and waking the window;
//! * the **writer** thread owns standard output, so that nothing that writes to
//!   VMLord can be held up by something that reads from it;
//! * the **session** thread owns the three sockets, the protocol machine and
//!   the decoder, and wakes the window when it has something to draw.
//!
//! The reader and the writer are two threads rather than one because a read of
//! standard input blocks: a single thread doing both would hold a hand-over
//! back behind a message that had not arrived yet.
//!
//! The decoded frame crosses from the session thread to the main thread through
//! [`SharedFrame`]: the session thread copies the rectangles that changed into
//! it, and the renderer uploads exactly those. A whole frame is never copied,
//! and a pixel is never logged.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("vmlord-display supports Windows only");

use std::{
    io,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::Ordering,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use vmlord_display_codec::{Geometry, Rect};
use vmlord_display_protocol::{
    record::Channel,
    v1::{Capability, Mode},
};
use vmlord_display_viewer::{
    display_modes::{self, DisplayMode, select_mode},
    fps_gap::{self, FpsGap},
    input::{self, Report},
    launch::{self, Command, DiagnosticLevel, Handover, LaunchParameters, Link, Message},
    live::{Live, Signal},
    log as viewer_log,
    placement::place,
    relay::{Relay, RelayError},
    resize::Resize,
    state::{Quality, Store, WindowState},
    status::{self, Button, Event, Progress, Status},
    windows::{
        clipboard::{self, Focus},
        d3d::Renderer,
        display_modes::{MonitorWatch, snapshot_for_window},
        hook::Hook,
        hvsocket::{CONNECT_TIMEOUT, ConnectError, HvSocket},
        ipc::{self, CommandServer, SingleInstance},
        window::{Shared, UiEvent, WM_SIGNAL, Window, become_dpi_aware, report},
    },
};

/// How long the main loop rests when there was nothing to do.
///
/// The pump is not vsync-locked when the overlay is up, and a spin would cost a
/// core to draw one word.
const IDLE: Duration = Duration::from_millis(8);

/// How long the session thread rests between attempts at the control socket.
const CONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// How long a fresh `ClientHello` is waited for after one is asked for.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long shutdown waits for the session thread to say goodbye.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    viewer_log::initialize();
    // Before any window: a client rectangle in scaled units would put a small
    // desktop on a big panel and blur it back up.
    become_dpi_aware();

    let mut link = Link::new(io::stdin(), io::sink());
    let parameters = match launch::first_parameters(&mut link) {
        Ok(parameters) => parameters,
        Err(message) => {
            tracing::error!("{message}");
            report(&message);
            return ExitCode::FAILURE;
        }
    };

    match SingleInstance::take(&parameters.runtime_id) {
        Ok(Some(claim)) => run(parameters, claim),
        Ok(None) => replace_the_window_left_by_the_previous_parent(parameters),
        Err(error) => {
            let message = format!("VMLord Display could not start: {error}");
            tracing::error!("{message}");
            report(&message);
            ExitCode::FAILURE
        }
    }
}

/// A second Connect on a VM that already has a window.
fn replace_the_window_left_by_the_previous_parent(parameters: LaunchParameters) -> ExitCode {
    tracing::info!(
        "a viewer for {} is already running; replacing its stale VMLord session",
        parameters.vm_name
    );

    match ipc::replace_instance(&parameters.runtime_id) {
        Ok(claim) => run(parameters, claim),
        Err(error) => {
            tracing::warn!("the running viewer could not be replaced: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one viewer, from its first socket to its last message.
fn run(parameters: LaunchParameters, claim: SingleInstance) -> ExitCode {
    let (ui_events, ui) = mpsc::channel();
    let shared = Arc::new(Shared::new(ui_events));
    let title = format!("{} - VMLord Display", parameters.vm_name);

    // Where this VM's window was left. What VMLord offered is the fallback,
    // because it is what the guest's module was configured with, and 1920x1080
    // is the fallback to that.
    let store = Store::for_vm(&parameters.vm_name);
    let mut state = store.as_ref().map(Store::load).unwrap_or_else(|| {
        let mut state = WindowState::default();
        if parameters.width > 0 && parameters.height > 0 {
            state.size = (parameters.width, parameters.height);
        }

        state
    });

    let mut window = match Window::open(&title, &state, Arc::clone(&shared)) {
        Ok(window) => window,
        Err(error) => {
            report(&format!("VMLord Display could not open a window: {error}"));
            return ExitCode::FAILURE;
        }
    };

    let mut renderer = match Renderer::open(window.handle()) {
        Ok(renderer) => renderer,
        Err(error) => {
            report(&format!(
                "VMLord Display could not open a graphics device: {error}"
            ));
            return ExitCode::FAILURE;
        }
    };

    let (command_sender, commands_in) = mpsc::channel();
    let commands = match CommandServer::start(&parameters.runtime_id, command_sender) {
        Ok(server) => server,
        Err(error) => {
            report(&format!(
                "VMLord Display could not listen for VMLord: {error}"
            ));
            return ExitCode::FAILURE;
        }
    };

    let frame = Arc::new(Mutex::new(SharedFrame::default()));
    let (to_session, from_pipe) = mpsc::channel();
    let (to_parent, outgoing) = mpsc::channel();
    // The main thread reports too: what it measures about the picture is a
    // diagnostic, and the session thread has no part in it.
    let to_parent_from_window = to_parent.clone();
    let (signals_out, signals) = mpsc::channel();
    let (orders_out, orders) = mpsc::channel();

    let clipboard: Arc<Mutex<Option<Sender<Focus>>>> = Arc::default();
    let reader = spawn_reader(to_session, window.poster());
    let writer = spawn_writer(outgoing);
    let session = spawn_session(Session {
        parameters: parameters.clone(),
        inbox: from_pipe,
        outbox: to_parent,
        signals: signals_out,
        orders,
        frame: Arc::clone(&frame),
        poster: window.poster(),
        clipboard: Arc::clone(&clipboard),
    });

    let exit = pump(
        Loop {
            window: &mut window,
            renderer: &mut renderer,
            shared: &shared,
            vm_name: &parameters.vm_name,
            frame: &frame,
            signals: &signals,
            orders: &orders_out,
            ui: &ui,
            commands: &commands_in,
            clipboard: &clipboard,
            outbox: &to_parent_from_window,
        },
        &mut state,
        parameters.fps_gap_threshold_percent,
    );

    // Best effort, and last: a window position is not worth delaying a
    // shutdown for, and losing it costs the next session nothing but a place.
    if let Some(store) = store.as_ref()
        && let Err(error) = store.save(&state)
    {
        tracing::warn!(
            "this window's place could not be remembered in {}: {error}",
            store.path().display()
        );
    }

    // Best effort, and bounded: a guest that has already gone is not worth
    // waiting on, and the VM is unaffected either way.
    let _ = orders_out.send(Order::End);
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while !session.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    drop(commands);
    drop(claim);
    // The reader is parked on standard input and the writer on its channel;
    // both end with the process, which is what closing the pipes does to them.
    drop(reader);
    drop(writer);
    tracing::info!("the display viewer for {} is closing", parameters.vm_name);

    exit
}

/// The decoded frame, as it crosses from the session thread to the renderer.
///
/// The session thread copies the rectangles that changed into `pixels` and adds
/// them to `damage`; the main thread uploads them and clears the list. Only the
/// damage is ever copied.
#[derive(Default)]
struct SharedFrame {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    damage: Vec<Rect>,
}

impl SharedFrame {
    /// Sizes the buffer to a new stream, dropping whatever was in it.
    fn configure(&mut self, geometry: Geometry) {
        self.pixels = vec![0; geometry.frame_bytes()];
        self.width = geometry.width();
        self.height = geometry.height();
        self.damage.clear();
    }

    /// Copies the rectangles that changed out of the decoder's frame.
    fn absorb(&mut self, source: &[u8], damage: &[Rect]) {
        let stride = self.width as usize * 4;
        if self.pixels.len() != source.len() {
            return;
        }

        for rect in damage {
            for row in rect.y..rect.y + rect.height {
                let start = row as usize * stride + rect.x as usize * 4;
                let end = start + rect.width as usize * 4;
                if end > source.len() {
                    continue;
                }
                self.pixels[start..end].copy_from_slice(&source[start..end]);
            }
            self.damage.push(*rect);
        }
    }
}

/// What the main thread asks the session thread for.
enum Order {
    /// The user pressed Retry: start the whole cycle again.
    Retry,
    /// The renderer lost its device and has nothing to draw from.
    Keyframe,
    /// The window is closing.
    End,
    /// One input event for the guest.
    Input(input::Event),
    /// The window settled at a size, and the guest's output should follow.
    Resolution {
        /// Physical pixels of client area, never logical ones.
        width: u32,
        /// The same.
        height: u32,
    },
    /// The user picked an encoding mode.
    Mode(Mode),
    /// The monitor the window is on, and the mode to prefer on it.
    AvailableModes {
        /// Every mode the host monitor drives, normalized.
        modes: Vec<DisplayMode>,
        /// The one the viewer chose, by policy or by the user.
        preferred: Option<DisplayMode>,
    },
    /// The user picked a resolution from the system menu.
    DisplayMode(DisplayMode),
}

/// Everything the session thread owns.
struct Session {
    parameters: LaunchParameters,
    inbox: Receiver<Message>,
    outbox: Sender<Message>,
    signals: Sender<Signal>,
    orders: Receiver<Order>,
    frame: Arc<Mutex<SharedFrame>>,
    poster: vmlord_display_viewer::windows::window::Poster,
    /// Where the window's focus reports are sent, once a clipboard thread
    /// exists to hear them.
    ///
    /// Shared rather than owned by either side: the window knows about focus
    /// and the session knows when a clipboard channel is worth having, and the
    /// thread that carries selections is made and dropped once per session.
    clipboard: Arc<Mutex<Option<Sender<Focus>>>>,
}

/// Everything the main loop borrows.
struct Loop<'a> {
    window: &'a mut Window,
    renderer: &'a mut Renderer,
    shared: &'a Arc<Shared>,
    vm_name: &'a str,
    frame: &'a Arc<Mutex<SharedFrame>>,
    signals: &'a Receiver<Signal>,
    orders: &'a Sender<Order>,
    ui: &'a Receiver<UiEvent>,
    commands: &'a Receiver<Command>,
    clipboard: &'a Arc<Mutex<Option<Sender<Focus>>>>,
    /// Straight to VMLord, for what the window itself finds.
    outbox: &'a Sender<Message>,
}

/// Reads standard input until VMLord closes it.
fn spawn_reader(
    to_session: Sender<Message>,
    poster: vmlord_display_viewer::windows::window::Poster,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut link = Link::new(io::stdin(), io::sink());
        loop {
            match link.read() {
                Ok(message) => {
                    if to_session.send(message).is_err() {
                        return;
                    }
                    poster.post(WM_SIGNAL);
                }
                Err(error) => {
                    tracing::info!("the launch pipe ended: {error}");
                    return;
                }
            }
        }
    })
}

/// Writes whatever the session thread has for VMLord.
fn spawn_writer(outgoing: Receiver<Message>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut link = Link::new(io::empty(), io::stdout());
        while let Ok(message) = outgoing.recv() {
            if let Err(error) = link.write(&message) {
                tracing::info!("the launch pipe could not be written to: {error}");
                return;
            }
        }
    })
}

/// Runs sessions until the window closes or the VM does.
fn spawn_session(session: Session) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut hello = session.parameters.client_hello.clone();

        loop {
            match attempt(&session, &hello) {
                Attempt::Restart => {
                    let _ = session.signals.send(Signal::Status(Event::Retry));
                    session.poster.post(WM_SIGNAL);
                    match refresh(&session) {
                        Some(fresh) => hello = fresh,
                        None => {
                            let _ = session.signals.send(Signal::Status(Event::NoParent));
                            session.poster.post(WM_SIGNAL);
                            return;
                        }
                    }
                }
                Attempt::Stop => return,
            }
        }
    })
}

/// How one attempt at a whole session ended.
enum Attempt {
    /// Worth another session, with a fresh `ClientHello`.
    Restart,
    /// Nothing more to do: the VM is gone, or the window is closing.
    Stop,
}

/// Opens the control socket, relays a handshake, and runs what comes of it.
fn attempt(session: &Session, hello: &[u8]) -> Attempt {
    let control = match connect_control(session) {
        Some(socket) => socket,
        None => return Attempt::Stop,
    };
    let _ = session.signals.send(Signal::Status(Event::Connected));
    session.poster.post(WM_SIGNAL);

    let mut control = control;
    let handover = {
        let mut relay = Relay::new(&mut control, &session.inbox, &session.outbox);
        match relay.run(hello, Instant::now() + status::RETRY_BUDGET) {
            Ok(handover) => handover,
            Err(RelayError::Cancelled) => return Attempt::Stop,
            Err(RelayError::NoParent) => {
                let _ = session.signals.send(Signal::Status(Event::NoParent));
                session.poster.post(WM_SIGNAL);
                return Attempt::Stop;
            }
            Err(error) => {
                tracing::warn!("the handshake did not finish: {error}");
                return Attempt::Restart;
            }
        }
    };

    // Its own thread and its own socket, so that a selection being read out of
    // the Windows clipboard cannot delay a frame. It ends when the sender the
    // holder keeps is dropped, which is at the end of this session.
    let clipboard = start_clipboard(session, &handover);

    let runtime_id = session.parameters.runtime_id;
    let frame_port = session.parameters.frame_port;
    let input_port = session.parameters.input_port;
    let connect = move |channel: Channel| {
        let port = match channel {
            Channel::Frame => frame_port,
            Channel::Input => input_port,
            // Control established the session and is not rebound; the
            // clipboard is connected by the thread that owns that socket.
            Channel::Control | Channel::Clipboard => {
                return Err(format!(
                    "the {channel} channel is not this session's to open"
                ));
            }
        };

        HvSocket::connect(&runtime_id, port, CONNECT_TIMEOUT).map_err(|error| error.to_string())
    };

    let mut live = match Live::new(handover, control, connect, Instant::now()) {
        Ok(live) => live,
        Err(error) => {
            tracing::error!("the hand-over could not be used: {error}");
            return Attempt::Restart;
        }
    };

    let outcome = drive(session, &mut live);
    stop_clipboard(session, clipboard);

    outcome
}

/// Starts this session's clipboard thread, if the session has a clipboard.
///
/// A session without the capability gets no thread and no socket: the guest
/// either ships the daemon or it does not, and there is nothing to retry.
fn start_clipboard(session: &Session, handover: &Handover) -> Option<JoinHandle<()>> {
    if !handover
        .capabilities
        .contains(&i32::from(Capability::Clipboard))
    {
        tracing::info!("this guest has no clipboard");

        return None;
    }

    let (handle, sender) = clipboard::spawn(clipboard::Parameters {
        runtime_id: session.parameters.runtime_id,
        port: session.parameters.clipboard_port,
        handover: handover.clone(),
        file_policy: session.parameters.file_policy,
    });
    *session
        .clipboard
        .lock()
        .expect("the clipboard sender is not poisoned") = Some(sender);

    Some(handle)
}

/// Ends this session's clipboard thread by taking the sender it listens on.
fn stop_clipboard(session: &Session, handle: Option<JoinHandle<()>>) {
    *session
        .clipboard
        .lock()
        .expect("the clipboard sender is not poisoned") = None;

    // Not joined: the thread ends on its own once its sender is gone, and a
    // session's end is not worth waiting on a socket for.
    drop(handle);
}

/// Tells the clipboard thread about focus, if there is one to tell.
fn tell_clipboard(holder: &Arc<Mutex<Option<Sender<Focus>>>>, focus: Focus) {
    let sender = holder
        .lock()
        .expect("the clipboard sender is not poisoned")
        .clone();

    if let Some(sender) = sender {
        let _ = sender.send(focus);
    }
}

/// Pumps one established session until it ends.
fn drive<S, C>(session: &Session, live: &mut Live<S, C>) -> Attempt
where
    S: io::Read + io::Write,
    C: FnMut(Channel) -> Result<S, String>,
{
    let mut signals = Vec::new();

    loop {
        // The whole queue, not one order a pass: with a 2 ms sleep in this
        // loop, one at a time would cap pointer motion at 500 events a second
        // and add latency under exactly the load that matters.
        loop {
            match session.orders.try_recv() {
                Ok(Order::End) => {
                    live.end();
                    return Attempt::Stop;
                }
                Ok(Order::Keyframe) => live.request_keyframe(),
                Ok(Order::Retry) => return Attempt::Restart,
                Ok(Order::Input(event)) => live.send_input(event),
                Ok(Order::Resolution { width, height }) => live.set_resolution(width, height),
                Ok(Order::Mode(mode)) => live.set_mode(mode),
                // Dropped rather than queued for a guest that never asked for
                // them: an older payload resizes its output from the window
                // and has nothing to do with a monitor's mode list.
                Ok(Order::AvailableModes { modes, preferred }) => {
                    if live.host_modes() {
                        live.set_available_modes(&modes, preferred);
                    }
                }
                Ok(Order::DisplayMode(mode)) => {
                    if live.host_modes() {
                        live.set_display_mode(mode);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Attempt::Stop,
            }
        }

        signals.clear();
        live.pump(Instant::now(), &mut signals);

        let mut ended = false;
        for signal in signals.drain(..) {
            if let Signal::Ended(reason) = &signal {
                tracing::warn!("the session ended: {reason}");
                ended = true;
            }
            publish(session, live, signal);
        }
        if ended {
            return Attempt::Restart;
        }

        thread::sleep(Duration::from_millis(2));
    }
}

/// Hands one signal to the main thread, copying pixels where there are any.
fn publish<S, C>(session: &Session, live: &Live<S, C>, signal: Signal)
where
    S: io::Read + io::Write,
    C: FnMut(Channel) -> Result<S, String>,
{
    match &signal {
        Signal::Configured(geometry) => {
            if let Ok(mut frame) = session.frame.lock() {
                frame.configure(*geometry);
            }
        }
        Signal::Damage(damage) => {
            if let (Ok(mut frame), Some(pixels)) = (session.frame.lock(), live.video().frame()) {
                frame.absorb(pixels, damage);
            }
        }
        _ => {}
    }

    let _ = session.signals.send(signal);
    session.poster.post(WM_SIGNAL);
}

/// Opens the control socket, retrying while the guest's services come up.
fn connect_control(session: &Session) -> Option<HvSocket> {
    loop {
        match session.orders.try_recv() {
            Ok(Order::End) | Err(TryRecvError::Disconnected) => return None,
            _ => {}
        }

        // The port comes from the launch parameters rather than from this
        // build's constant, the way the other two channels' do: VMLord names
        // the three ports once, and the viewer uses what it was told.
        match HvSocket::connect(
            &session.parameters.runtime_id,
            session.parameters.control_port,
            CONNECT_TIMEOUT,
        ) {
            Ok(socket) => return Some(socket),
            Err(ConnectError::PartitionGone) => {
                tracing::info!("the VM is not running; the viewer is closing");
                let _ = session.signals.send(Signal::Status(Event::PartitionGone));
                session.poster.post(WM_SIGNAL);
                return None;
            }
            Err(error) => {
                tracing::debug!("the control socket is not up yet: {error}");
                thread::sleep(CONNECT_BACKOFF);
            }
        }
    }
}

/// Asks VMLord for a new session, and waits for the hello it answers with.
///
/// The token is what makes this answerable only by the VMLord instance that
/// spawned this viewer.
fn refresh(session: &Session) -> Option<Vec<u8>> {
    session
        .outbox
        .send(Message::RequestRelay {
            token: session.parameters.token.clone(),
        })
        .ok()?;

    let deadline = Instant::now() + REFRESH_TIMEOUT;
    while Instant::now() < deadline {
        match session.inbox.recv_timeout(REFRESH_TIMEOUT) {
            Ok(Message::RelayToViewer(bytes)) => return Some(bytes),
            Ok(Message::Command(Command::Close)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return None;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return None,
        }
    }

    None
}

/// The message pump, and everything that happens between messages.
///
/// `state` is this VM's window as it will be remembered: the loop keeps it up
/// to date rather than reading the window back at the end, because a window
/// that is closing has already stopped being where it was.
fn pump(mut context: Loop<'_>, state: &mut WindowState, fps_gap_threshold_percent: u8) -> ExitCode {
    let mut progress = Progress::new(Instant::now());
    let mut closing = false;
    let mut policy = input::Policy::new();
    let mut stream: Option<Geometry> = None;
    let mut resize = Resize::new();
    // The monitor the window is on, enumerated once a rearrangement settles,
    // and what its last enumeration said.
    let mut monitors = MonitorWatch::new();
    let mut offered: Vec<DisplayMode> = Vec::new();
    let mut native: Option<DisplayMode> = None;
    // What the guest confirmed it came up on, which is what a delivered rate
    // is measured against -- not what the viewer asked for.
    let mut committed: Option<DisplayMode> = None;
    let mut fps_gap = FpsGap::new(fps_gap_threshold_percent);
    let mut presented = 0u64;
    // While this lives the keyboard is the guest's. It is taken on focus and
    // given back the moment the window loses it -- or the user asks.
    let mut hook: Option<Hook> = None;

    // The mode the window was left on, asked for once the session is up.
    let mut mode_owed = true;

    while context.window.pump() {
        let mut worked = false;

        while let Ok(signal) = context.signals.try_recv() {
            worked = true;
            match &signal {
                Signal::Configured(geometry) => {
                    stream = Some(*geometry);
                    reposition(&mut policy, stream, context.window);
                }
                Signal::Committed(mode) => {
                    committed = *mode;
                    // A new mode is a new rate to measure: the frames that
                    // arrived at the mode before are not this one's.
                    fps_gap.reset();
                }
                Signal::Ended(_) => {
                    policy.report(Report::ChannelLost);
                    // The guest that was told the window's size is not the
                    // guest that will be listening next, so the window is
                    // offered again rather than assumed to have been taken.
                    // Held until the next session is running, because that is
                    // when the request is drained.
                    resize.forget();
                    let (width, height) = context.window.client_size();
                    resize.observe(width.max(0) as u32, height.max(0) as u32, Instant::now());
                    mode_owed = true;
                    // And the same for the monitor: the list and the selection
                    // are sent to the guest that is listening now, in that
                    // order, rather than assumed to have survived the socket.
                    monitors.forget();
                    monitors.observe(Instant::now());
                    committed = None;
                    fps_gap.reset();
                }
                _ => {}
            }
            apply(&mut context, &mut progress, signal);
        }

        while let Ok(event) = context.ui.try_recv() {
            worked = true;
            match event {
                UiEvent::Pressed(Button::Retry) => {
                    progress.on(Event::Retry, Instant::now());
                    let _ = context.orders.send(Order::Retry);
                }
                UiEvent::Pressed(Button::Cancel) | UiEvent::Closing => closing = true,
                UiEvent::Input(report) => {
                    match report {
                        Report::FocusGained => {
                            tell_clipboard(context.clipboard, Focus::Gained);
                            match Hook::install(context.shared) {
                                Ok(installed) => hook = Some(installed),
                                // A viewer without the hook still types; it only
                                // loses the keys the shell takes first.
                                Err(error) => tracing::warn!("{error}"),
                            }
                        }
                        Report::FocusLost | Report::ReleaseKeyboard => {
                            // The clipboard follows the keyboard: a VM in the
                            // background neither reads what its user copies
                            // elsewhere nor replaces what is on their clipboard.
                            tell_clipboard(context.clipboard, Focus::Lost);
                            hook = None;
                        }
                        _ => {}
                    }
                    policy.report(report);
                }
                UiEvent::Resized(width, height) => {
                    let (width, height) = (width.max(0) as u32, height.max(0) as u32);
                    if let Err(error) = context.renderer.resize_swapchain(width, height) {
                        tracing::warn!("the swapchain could not follow the window: {error}");
                    }
                    reposition(&mut policy, stream, context.window);
                    // Held rather than sent: a drag is hundreds of these, and
                    // each one taken at face value is a mode set in the guest.
                    resize.observe(width, height, Instant::now());
                    // What is remembered is the window, not the monitor a
                    // full-screen one is covering.
                    if !context.window.is_fullscreen() && width > 0 && height > 0 {
                        state.size = (width, height);
                    }
                }
                UiEvent::MonitorChanged => monitors.observe(Instant::now()),
                UiEvent::DisplayMode(mode) => {
                    // Remembered because the user said so: a mode chosen by
                    // policy is one the next monitor may not have, and a mode
                    // chosen from the menu is what this VM opens at.
                    state.display_mode = Some(mode);
                    context.window.set_modes(&offered, Some(mode));
                    let _ = context.orders.send(Order::DisplayMode(mode));
                }
                UiEvent::Moved(x, y) => {
                    // Only a restored window reports this, so what is kept is
                    // the place the user left the window rather than the
                    // monitor a full-screen one is covering.
                    state.position = Some((x, y));
                }
                UiEvent::ToggleFullscreen => {
                    let wanted = !context.window.is_fullscreen();
                    context.window.set_fullscreen(wanted);
                    state.fullscreen = context.window.is_fullscreen();
                    // A window filling a monitor wants that monitor's own
                    // timing, which is not always the one the desktop is on.
                    if wanted && let Some(mode) = native.filter(|mode| offered.contains(mode)) {
                        context.window.set_modes(&offered, Some(mode));
                        let _ = context.orders.send(Order::DisplayMode(mode));
                    }
                }
                UiEvent::Quality(quality) => {
                    state.quality = quality;
                    context.window.check_quality(quality);
                    let _ = context.orders.send(Order::Mode(mode_of(quality)));
                }
            }
        }

        // A settled window becomes one request, and only once the session is
        // running: a `SetResolution` on a control channel that is still coming
        // up is a record the guest has nowhere to put.
        if progress.is_running() {
            if mode_owed {
                mode_owed = false;
                let _ = context.orders.send(Order::Mode(mode_of(state.quality)));
            }
            if let Some((width, height)) = resize.due(Instant::now()) {
                worked = true;
                let _ = context.orders.send(Order::Resolution { width, height });
            }
        }
        if monitors.due(Instant::now()) {
            worked = true;
            if let Some(snapshot) = snapshot_for_window(context.window.handle())
                && monitors.accept(snapshot.clone())
            {
                tracing::debug!(
                    "the window is on {} with {} modes",
                    snapshot.identity,
                    snapshot.modes.len()
                );
                // The selection survives a monitor that still offers it and
                // falls back by policy when it does not, which is what a
                // laptop coming back without its dock looks like.
                let chosen = select_mode(state.display_mode, &snapshot.modes);
                // Cut to what the guest's connector holds before anything is
                // published: a longer list is one the module refuses whole.
                offered = display_modes::offered(&snapshot.modes, Some(chosen));
                native = snapshot.preferred;
                context.window.set_modes(&offered, Some(chosen));
                let _ = context.orders.send(Order::AvailableModes {
                    modes: offered.clone(),
                    preferred: Some(chosen),
                });
            }
        }

        while let Ok(command) = context.commands.try_recv() {
            worked = true;
            match command {
                Command::Focus => context.window.focus(),
                Command::Close => closing = true,
            }
        }

        for event in policy.drain() {
            worked = true;
            let _ = context.orders.send(Order::Input(event));
        }
        if policy.keyboard_release_requested() {
            hook = None;
        }

        progress.tick(Instant::now());
        context.shared.failed.store(
            matches!(progress.status(), Status::Failed(_)),
            Ordering::Relaxed,
        );

        // A stopped VM closes the window rather than showing a failure.
        if closing || matches!(progress.status(), Status::Gone) {
            // A hook left installed would swallow the user's keyboard with
            // nothing left to send it to.
            drop(hook);

            return ExitCode::SUCCESS;
        }

        let uploaded = upload(&mut context);
        if draw(&mut context, &progress) && uploaded {
            presented += 1;
        }

        // Paused rather than counted while there is no picture to deliver: a
        // minimised window and a session that is still coming up are both a
        // second of nothing, and neither is the guest failing to keep up.
        let measuring = progress.is_running() && !context.window.is_minimised();
        if !measuring {
            fps_gap.reset();
        } else if let Some(warning) = fps_gap.sample(Instant::now(), presented, committed) {
            let _ = context.outbox.send(Message::Diagnostic {
                level: DiagnosticLevel::Warning,
                detail: fps_gap::detail(context.vm_name, warning),
            });
        }

        if !worked {
            thread::sleep(IDLE);
        }
    }

    drop(hook);

    ExitCode::SUCCESS
}

/// The wire mode a menu choice means.
///
/// `Auto` is a host-side policy, and until task #123 there is one mode to
/// resolve it to. Sending `MODE_AUTO` rather than resolving it here is
/// deliberate: the guest is the one that knows what it can encode, and it
/// answers `Auto` with what it settled on.
fn mode_of(quality: Quality) -> Mode {
    match quality {
        Quality::Auto => Mode::Auto,
        Quality::Desktop => Mode::Desktop,
    }
}

/// Tells the policy where the picture is, from the stream and the window.
fn reposition(policy: &mut input::Policy, stream: Option<Geometry>, window: &Window) {
    let Some(geometry) = stream else {
        policy.set_placement(None);

        return;
    };

    let (width, height) = window.client_size();
    policy.set_placement(place(geometry.width(), geometry.height(), width, height));
}

/// Moves one signal into the status machine and the renderer.
fn apply(context: &mut Loop<'_>, progress: &mut Progress, signal: Signal) {
    match signal {
        Signal::Configured(geometry) => {
            if let Err(error) = context.renderer.configure(geometry) {
                tracing::error!("the stream could not be shown: {error}");
            }
        }
        Signal::Cursor(image) => {
            if let Err(error) = context.renderer.set_cursor(&image) {
                tracing::warn!("the guest's cursor could not be shown: {error}");
            }
        }
        // Where the guest thinks its pointer is. The host's own cursor is what
        // the user sees until #119 wires input up.
        // Both are the main loop's own business rather than the renderer's:
        // where the guest thinks its pointer is, and what mode it came up on.
        Signal::Moved(_) | Signal::Damage(_) | Signal::Committed(_) => {}
        Signal::Status(event) => progress.on(event, Instant::now()),
        Signal::Ended(reason) => tracing::info!("the session is over: {reason}"),
    }
}

/// Uploads whatever the session thread has copied in since the last frame.
///
/// Whether there was one, which is what an FPS is counted from: a loop that
/// spins with nothing new to draw is not a frame that was delivered.
fn upload(context: &mut Loop<'_>) -> bool {
    let Ok(mut frame) = context.frame.lock() else {
        return false;
    };
    if frame.damage.is_empty() {
        return false;
    }

    let damage = std::mem::take(&mut frame.damage);
    if let Err(error) = context.renderer.upload(&frame.pixels, &damage) {
        tracing::warn!("the frame could not be uploaded: {error}");

        return false;
    }

    true
}

/// Presents, and rebuilds the device if it was lost.
///
/// `true` when what was on screen is what the guest sent, which is the other
/// half of a frame having been delivered.
fn draw(context: &mut Loop<'_>, progress: &Progress) -> bool {
    let Err(error) = context.renderer.present(progress, context.vm_name) else {
        return true;
    };

    tracing::warn!("the frame could not be presented: {error}");
    match context.renderer.recover() {
        // The device that was lost held the only copy of what was on screen.
        Ok(true) => {
            let _ = context.orders.send(Order::Keyframe);
        }
        Ok(false) => tracing::error!("the graphics device cannot be recovered again"),
        Err(error) => tracing::error!("the graphics device could not be rebuilt: {error}"),
    }

    false
}
