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
use vmlord_display_protocol::record::Channel;
use vmlord_display_viewer::{
    launch::{self, Command, LaunchParameters, Link, Message},
    live::{Live, Signal},
    log as viewer_log,
    relay::{Relay, RelayError},
    status::{self, Button, Event, Progress, Status},
    windows::{
        d3d::Renderer,
        hvsocket::{CONNECT_TIMEOUT, ConnectError, HvSocket},
        ipc::{self, CommandServer, SingleInstance},
        window::{Shared, UiEvent, WM_SIGNAL, Window, report},
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

    let mut link = Link::new(io::stdin(), io::sink());
    let parameters = match launch::first_parameters(&mut link) {
        Ok(parameters) => parameters,
        Err(message) => {
            log::error!("{message}");
            report(&message);
            return ExitCode::FAILURE;
        }
    };

    match SingleInstance::take(&parameters.runtime_id) {
        Ok(Some(claim)) => run(parameters, claim),
        Ok(None) => focus_the_window_that_is_already_open(&parameters),
        Err(error) => {
            let message = format!("VMLord Display could not start: {error}");
            log::error!("{message}");
            report(&message);
            ExitCode::FAILURE
        }
    }
}

/// A second Connect on a VM that already has a window.
fn focus_the_window_that_is_already_open(parameters: &LaunchParameters) -> ExitCode {
    log::info!(
        "a viewer for {} is already running; asking it to come forward",
        parameters.vm_name
    );

    match ipc::send_command(&parameters.runtime_id, Command::Focus) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The mutex was held but nobody answered: a viewer that is on its
            // way out. Saying so beats opening a second window over it.
            log::warn!("the running viewer could not be reached: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one viewer, from its first socket to its last message.
fn run(parameters: LaunchParameters, claim: SingleInstance) -> ExitCode {
    let (ui_events, ui) = mpsc::channel();
    let shared = Arc::new(Shared::new(ui_events));
    let title = format!("{} - VMLord Display", parameters.vm_name);

    let window = match Window::open(
        &title,
        i32::try_from(parameters.width).unwrap_or(1280),
        i32::try_from(parameters.height).unwrap_or(720),
        Arc::clone(&shared),
    ) {
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
    let (signals_out, signals) = mpsc::channel();
    let (orders_out, orders) = mpsc::channel();

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
    });

    let exit = pump(Loop {
        window: &window,
        renderer: &mut renderer,
        shared: &shared,
        vm_name: &parameters.vm_name,
        frame: &frame,
        signals: &signals,
        orders: &orders_out,
        ui: &ui,
        commands: &commands_in,
    });

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
    log::info!("the display viewer for {} is closing", parameters.vm_name);

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
}

/// Everything the main loop borrows.
struct Loop<'a> {
    window: &'a Window,
    renderer: &'a mut Renderer,
    shared: &'a Arc<Shared>,
    vm_name: &'a str,
    frame: &'a Arc<Mutex<SharedFrame>>,
    signals: &'a Receiver<Signal>,
    orders: &'a Sender<Order>,
    ui: &'a Receiver<UiEvent>,
    commands: &'a Receiver<Command>,
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
                    log::info!("the launch pipe ended: {error}");
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
                log::info!("the launch pipe could not be written to: {error}");
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
                log::warn!("the handshake did not finish: {error}");
                return Attempt::Restart;
            }
        }
    };

    let runtime_id = session.parameters.runtime_id;
    let frame_port = session.parameters.frame_port;
    let input_port = session.parameters.input_port;
    let connect = move |channel: Channel| {
        let port = match channel {
            Channel::Frame => frame_port,
            Channel::Input => input_port,
            Channel::Control => return Err("control is not rebound".to_owned()),
        };

        HvSocket::connect(&runtime_id, port, CONNECT_TIMEOUT).map_err(|error| error.to_string())
    };

    let mut live = match Live::new(handover, control, connect, Instant::now()) {
        Ok(live) => live,
        Err(error) => {
            log::error!("the hand-over could not be used: {error}");
            return Attempt::Restart;
        }
    };

    drive(session, &mut live)
}

/// Pumps one established session until it ends.
fn drive<S, C>(session: &Session, live: &mut Live<S, C>) -> Attempt
where
    S: io::Read + io::Write,
    C: FnMut(Channel) -> Result<S, String>,
{
    let mut signals = Vec::new();

    loop {
        match session.orders.try_recv() {
            Ok(Order::End) => {
                live.end();
                return Attempt::Stop;
            }
            Ok(Order::Keyframe) => live.request_keyframe(),
            Ok(Order::Retry) => return Attempt::Restart,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Attempt::Stop,
        }

        signals.clear();
        live.pump(Instant::now(), &mut signals);

        let mut ended = false;
        for signal in signals.drain(..) {
            if let Signal::Ended(reason) = &signal {
                log::warn!("the session ended: {reason}");
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
                log::info!("the VM is not running; the viewer is closing");
                let _ = session.signals.send(Signal::Status(Event::PartitionGone));
                session.poster.post(WM_SIGNAL);
                return None;
            }
            Err(error) => {
                log::debug!("the control socket is not up yet: {error}");
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
fn pump(mut context: Loop<'_>) -> ExitCode {
    let mut progress = Progress::new(Instant::now());
    let mut closing = false;

    while context.window.pump() {
        let mut worked = false;

        while let Ok(signal) = context.signals.try_recv() {
            worked = true;
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
                UiEvent::Resized(width, height) => {
                    if let Err(error) = context
                        .renderer
                        .resize_swapchain(width.max(0) as u32, height.max(0) as u32)
                    {
                        log::warn!("the swapchain could not follow the window: {error}");
                    }
                }
            }
        }

        while let Ok(command) = context.commands.try_recv() {
            worked = true;
            match command {
                Command::Focus => context.window.focus(),
                Command::Close => closing = true,
            }
        }

        progress.tick(Instant::now());
        context.shared.failed.store(
            matches!(progress.status(), Status::Failed(_)),
            Ordering::Relaxed,
        );

        // A stopped VM closes the window rather than showing a failure.
        if closing || matches!(progress.status(), Status::Gone) {
            return ExitCode::SUCCESS;
        }

        upload(&mut context);
        draw(&mut context, &progress);

        if !worked {
            thread::sleep(IDLE);
        }
    }

    ExitCode::SUCCESS
}

/// Moves one signal into the status machine and the renderer.
fn apply(context: &mut Loop<'_>, progress: &mut Progress, signal: Signal) {
    match signal {
        Signal::Configured(geometry) => {
            if let Err(error) = context.renderer.configure(geometry) {
                log::error!("the stream could not be shown: {error}");
            }
        }
        Signal::Cursor(image) => {
            if let Err(error) = context.renderer.set_cursor(&image) {
                log::warn!("the guest's cursor could not be shown: {error}");
            }
        }
        // Where the guest thinks its pointer is. The host's own cursor is what
        // the user sees until #119 wires input up.
        Signal::Moved(_) | Signal::Damage(_) => {}
        Signal::Status(event) => progress.on(event, Instant::now()),
        Signal::Ended(reason) => log::info!("the session is over: {reason}"),
    }
}

/// Uploads whatever the session thread has copied in since the last frame.
fn upload(context: &mut Loop<'_>) {
    let Ok(mut frame) = context.frame.lock() else {
        return;
    };
    if frame.damage.is_empty() {
        return;
    }

    let damage = std::mem::take(&mut frame.damage);
    if let Err(error) = context.renderer.upload(&frame.pixels, &damage) {
        log::warn!("the frame could not be uploaded: {error}");
    }
}

/// Presents, and rebuilds the device if it was lost.
fn draw(context: &mut Loop<'_>, progress: &Progress) {
    let Err(error) = context.renderer.present(progress, context.vm_name) else {
        return;
    };

    log::warn!("the frame could not be presented: {error}");
    match context.renderer.recover() {
        // The device that was lost held the only copy of what was on screen.
        Ok(true) => {
            let _ = context.orders.send(Order::Keyframe);
        }
        Ok(false) => log::error!("the graphics device cannot be recovered again"),
        Err(error) => log::error!("the graphics device could not be rebuilt: {error}"),
    }
}
