//! The guest's tray icon, for whoever is at the guest's screen.
//!
//! It lives in the graphical session like the clipboard daemon does, because
//! a tray icon does too: StatusNotifierItem is answered on the session bus,
//! and GNOME shows one through its AppIndicator extension. Everything the
//! menu offers is a command the host viewer already has -- the kinds are the
//! wire's [`GuestCommandKind`] -- so the tray decides nothing. It forwards.
//!
//! The one exception is restarting services: the broker puts the system units
//! back, and the clipboard daemon -- a user unit on this very bus, which the
//! broker has no business on -- is put back from here.
//!
//! Everything short of a session ending is waited through rather than exited
//! over: no StatusNotifierWatcher yet, no broker yet, a broker that went
//! away. A tray that exited on any of those would spend its restart budget
//! on the ordinary shape of a session starting up.

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use ksni::{
    Icon, ToolTip,
    blocking::{Handle, TrayMethods},
    menu::{MenuItem, StandardItem, SubMenu},
};
use vmlord_display_protocol::v1::{DisplayTiming, GuestCommandKind};

use crate::{ipc::Message, systemd, tray_icon, unix::Connection};

/// Where the broker offers the tray socket.
const BROKER_SOCKET: &str = "/run/vmlord/display-tray.sock";

/// How long to wait before looking for the broker, doubled on every failed
/// attempt up to [`RETRY_MAX`]: a broker that is down stays down for minutes.
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// How long the mode list goes unrefreshed with nobody asking for it.
const REFRESH: Duration = Duration::from_secs(10);

/// How long an answer is watched for after a datagram was sent.
const REPLY_PATIENCE: Duration = Duration::from_millis(500);

/// How often the watcher for an answer wakes while it waits.
const REPLY_TICK: Duration = Duration::from_millis(25);

/// What the menu says while no modes are known.
const NO_MODES_LABEL: &str = "No modes offered yet";

/// The AppIndicator extensions the supported guests ship, Ubuntu's own fork
/// first: its UUID is the one the `gnome-shell-extension-appindicator`
/// package installs on every supported release, where Debian ships the same
/// source under the upstream UUID named second.
const APPINDICATOR_UUIDS: [&str; 2] = [
    "ubuntu-appindicators@ubuntu.com",
    "appindicatorsupport@rgcjonas.gmail.com",
];

/// What the tray was started with.
pub struct Options {
    /// The socket the broker offers the tray on.
    pub broker_socket: PathBuf,
}

impl Options {
    /// The defaults, with the environment allowed to override each one.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            broker_socket: std::env::var("VMLORD_DISPLAY_TRAY_SOCKET")
                .unwrap_or_else(|_| BROKER_SOCKET.to_owned())
                .into(),
        }
    }
}

/// Runs the tray until the session ends.
///
/// The menu runs on ksni's thread and never touches the broker; the socket
/// belongs to the link thread that [`serve`] starts here and never returns
/// from.
#[must_use]
pub fn run(options: Options) -> ExitCode {
    let (commands, link) = mpsc::channel();
    let handle = loop {
        let tray = GuestTray {
            commands: commands.clone(),
            modes: Vec::new(),
            selected: None,
        };
        match tray.assume_sni_available(true).spawn() {
            Ok(handle) => break handle,
            // Only a session with no bus at all lands here. The unit starts
            // with a graphical session, so one is on its way; saying so and
            // waiting costs less than a restart.
            Err(error) => {
                eprintln!("vmlord-display-tray: {error}");
                std::thread::sleep(RETRY_MAX);
            }
        }
    };

    serve(&link, &handle, &options.broker_socket);
    ExitCode::SUCCESS
}

/// The tray as ksni holds it: where clicks go, and what the broker last said.
struct GuestTray {
    /// What a click asks for. Sends never block: a menu that freezes is a
    /// menu the panel stops trusting.
    commands: Sender<Command>,
    /// Every mode the host has published, as of the last answer.
    modes: Vec<DisplayTiming>,
    /// The one the host chose, when it has chosen.
    selected: Option<DisplayTiming>,
}

impl GuestTray {
    /// Asks the broker what the host offers. Fire and forget: the answer
    /// comes back through the link thread, and a send that fails because the
    /// link is down is repaired by the link itself.
    fn request_modes(&self) {
        let _ = self
            .commands
            .send(Command::Broker(Message::DisplayModesRequested));
    }
}

impl ksni::Tray for GuestTray {
    fn id(&self) -> String {
        "vmlord-display-tray".to_owned()
    }

    fn title(&self) -> String {
        "VMLord".to_owned()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: "VMLord".to_owned(),
            description: "Control the viewer that shows this desktop".to_owned(),
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![tray_icon::monogram()]
    }

    fn menu_about_to_show(&mut self) {
        // The panel is about to draw the Resolution submenu, so this is the
        // moment for the freshest answer. The periodic refresh covers panels
        // that never call this.
        self.request_modes();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu(
            self.modes.as_slice(),
            self.selected.as_ref(),
            &self.commands,
        )
    }
}

/// What a click asks the link thread for.
enum Command {
    /// One datagram for the broker.
    Broker(Message),
    /// The clipboard daemon put back, on the session bus.
    RestartClipboard,
}

/// The menu, in the order the viewer's own shows them.
///
/// Stateless on purpose, v1: every label is fixed English, and the only thing
/// that changes is the Resolution submenu, which is the broker's answer.
fn menu(
    modes: &[DisplayTiming],
    selected: Option<&DisplayTiming>,
    commands: &Sender<Command>,
) -> Vec<MenuItem<GuestTray>> {
    vec![
        command_item("Full screen", GuestCommandKind::ToggleFullscreen, commands),
        command_item(
            "Send Ctrl+Alt+Del",
            GuestCommandKind::SendSecureAttention,
            commands,
        ),
        command_item(
            "Release keyboard",
            GuestCommandKind::ReleaseKeyboard,
            commands,
        ),
        MenuItem::Separator,
        resolution_menu(modes, selected, commands),
        command_item("Mute audio", GuestCommandKind::ToggleMute, commands),
        MenuItem::Separator,
        command_item("Quality: Auto", GuestCommandKind::QualityAuto, commands),
        command_item(
            "Quality: Desktop",
            GuestCommandKind::QualityDesktop,
            commands,
        ),
        MenuItem::Separator,
        restart_item(commands),
    ]
}

/// One item that raises one kind of user command, with no mode of its own.
fn command_item(
    label: &str,
    kind: GuestCommandKind,
    commands: &Sender<Command>,
) -> MenuItem<GuestTray> {
    let commands = commands.clone();
    StandardItem {
        label: label.to_owned(),
        activate: Box::new(move |_| {
            let _ = commands.send(Command::Broker(Message::UserCommand {
                kind,
                display_mode: None,
            }));
        }),
        ..Default::default()
    }
    .into()
}

/// The Resolution submenu: one plain entry per mode the host offered,
/// labelled the way the viewer writes them, and the chosen one on the header.
fn resolution_menu(
    modes: &[DisplayTiming],
    selected: Option<&DisplayTiming>,
    commands: &Sender<Command>,
) -> MenuItem<GuestTray> {
    let submenu = if modes.is_empty() {
        // No answer yet. A placeholder keeps the menu's shape steady, and an
        // item that cannot be clicked promises nothing.
        vec![
            StandardItem {
                label: NO_MODES_LABEL.to_owned(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ]
    } else {
        modes
            .iter()
            .map(|mode| {
                let commands = commands.clone();
                let timing = *mode;
                StandardItem {
                    label: mode_label(&timing),
                    activate: Box::new(move |_| {
                        let _ = commands.send(Command::Broker(Message::UserCommand {
                            kind: GuestCommandKind::SetDisplayMode,
                            display_mode: Some(timing),
                        }));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect()
    };

    SubMenu {
        label: selected.map_or_else(
            || "Resolution".to_owned(),
            |mode| format!("Resolution: {}", mode_label(mode)),
        ),
        submenu,
        ..Default::default()
    }
    .into()
}

/// A mode as the viewer writes it: 1920x1080@60.
fn mode_label(mode: &DisplayTiming) -> String {
    format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz)
}

/// The one item that is not a forwarded command.
///
/// The broker restarts the units it answers for; the clipboard daemon's unit
/// lives on the bus this process is already on, so the tray restarts it here,
/// in the same click.
fn restart_item(commands: &Sender<Command>) -> MenuItem<GuestTray> {
    let broker = commands.clone();
    let clipboard = commands.clone();
    StandardItem {
        label: "Restart services".to_owned(),
        activate: Box::new(move |_| {
            let _ = broker.send(Command::Broker(Message::RestartSession));
            let _ = clipboard.send(Command::RestartClipboard);
        }),
        ..Default::default()
    }
    .into()
}

/// Keeps one connection to the broker, for the life of the process.
///
/// Datagrams go out from here and answers come back to here, so the menu
/// thread never touches the socket and a failing send is this loop's problem
/// to repair. A send with nobody on the far side costs one round of the
/// backoff, and the periodic refresh bounds how stale the menu can get when
/// a datagram is lost with its connection.
fn serve(commands: &Receiver<Command>, handle: &Handle<GuestTray>, socket: &Path) {
    let mut backoff = RETRY_MIN;
    loop {
        let Ok(connection) = Connection::connect(socket) else {
            std::thread::sleep(backoff);
            backoff = RETRY_MAX.min(backoff * 2);

            continue;
        };
        let _ = connection.set_nonblocking();
        backoff = RETRY_MIN;
        eprintln!("vmlord-display-tray: attached to the display broker");

        // The reconnects this rides on are when a payload applied mid-session
        // has had the chance to install what the first try missed.
        ensure_appindicator_extension();
        let mut live = connection
            .send(&Message::DisplayModesRequested, &[])
            .is_ok();
        if live {
            live = drain(&connection, handle);
        }
        while live {
            match commands.recv_timeout(REFRESH) {
                Ok(Command::Broker(message)) => {
                    live = connection.send(&message, &[]).is_ok() && drain(&connection, handle);
                }
                Ok(Command::RestartClipboard) => restart_clipboard(&connection),
                Err(RecvTimeoutError::Timeout) => {
                    live = connection
                        .send(&Message::DisplayModesRequested, &[])
                        .is_ok()
                        && drain(&connection, handle);
                }
                // Nothing sends on this channel but the tray itself.
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        eprintln!("vmlord-display-tray: the display broker went away");
        std::thread::sleep(backoff);
        backoff = RETRY_MAX.min(backoff * 2);
    }
}

/// Reads every answer that arrives, for a short while.
///
/// `false` when the connection died underneath, so the caller reconnects. A
/// quiet socket inside the patience is an ending rather than a wait: the link
/// has a menu to get back to, and [`REFRESH`] bounds the staleness anyway.
fn drain(connection: &Connection, handle: &Handle<GuestTray>) -> bool {
    let deadline = Instant::now() + REPLY_PATIENCE;
    loop {
        match connection.receive() {
            Ok((Message::DisplayModes { modes, selected }, _)) => {
                // ksni tells the panel the layout changed, which is what
                // makes a menu redrawn since it last asked.
                let _ = handle.update(|tray| {
                    tray.modes = modes;
                    tray.selected = selected;
                });
            }
            // A menu action the broker could not carry out, written for the
            // guest's journal as the broker wrote it for this socket.
            Ok((Message::Report { detail }, _)) => eprintln!("vmlord-display-tray: {detail}"),
            // SessionClosed and everything else on this socket is the broker
            // and the session talking to each other; the tray has no part.
            Ok((_, _)) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return true;
                }
                std::thread::sleep(REPLY_TICK);
            }
            Err(_) => return false,
        }
    }
}

/// Puts the clipboard daemon back, on the bus this process lives in.
///
/// A session without a working bus is an ordinary error, logged and told to
/// the broker for the host's log -- never a crash.
fn restart_clipboard(connection: &Connection) {
    if let Err(reason) = systemd::restart_clipboard_service() {
        eprintln!("vmlord-display-tray: {reason}");
        let _ = connection.send(&Message::Report { detail: reason }, &[]);
    }
}

/// Asks the running shell to enable one of the AppIndicator extensions.
///
/// This is `gsettings set org.gnome.shell enabled-extensions ...` with the
/// merging left to the shell: `EnableExtension` adds one UUID and keeps the
/// rest of the user's list, which no process outside the session can do
/// without clobbering either the desktop's own defaults or the user's own
/// choices. Tried at startup and on every reconnect, so the icon recovers on
/// the next reconnect -- usually the next Restart services -- rather than at
/// the next login.
fn ensure_appindicator_extension() {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        eprintln!("vmlord-display-tray: no session bus to ask for the tray extension");

        return;
    };
    // Named for what the value is rather than for its shape: a scanner that
    // sees `uuid` written to a log reads a secret, and these two are
    // constants in this file.
    for extension in APPINDICATOR_UUIDS {
        match connection.call_method(
            Some("org.gnome.Shell.Extensions"),
            "/org/gnome/Shell",
            Some("org.gnome.Shell.Extensions"),
            "EnableExtension",
            &(extension,),
        ) {
            Ok(_) => {
                eprintln!("vmlord-display-tray: the shell shows tray items through {extension}");

                return;
            }
            // Not installed yet, or the shell is not up to answer: the next
            // attempt is the next reconnect, and the icon waits meanwhile.
            Err(error) => eprintln!("vmlord-display-tray: {extension} is not available: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use super::{Command, GuestTray, NO_MODES_LABEL, mode_label};
    use crate::ipc::{Message, decode, encode};
    use ksni::menu::MenuItem;
    use vmlord_display_protocol::v1::{DisplayTiming, GuestCommandKind};

    fn timing(width: u32, height: u32, refresh_hz: u32) -> DisplayTiming {
        DisplayTiming {
            width,
            height,
            refresh_hz,
        }
    }

    /// The menu as the panel would be handed it.
    fn menu_of(tray: &GuestTray) -> Vec<MenuItem<GuestTray>> {
        ksni::Tray::menu(tray)
    }

    /// A tray over a channel the test reads, holding the modes given.
    fn tray_with(
        modes: Vec<DisplayTiming>,
        selected: Option<DisplayTiming>,
    ) -> (GuestTray, Receiver<Command>) {
        let (commands, received) = mpsc::channel();
        (
            GuestTray {
                commands,
                modes,
                selected,
            },
            received,
        )
    }

    /// The labels of the menu's own items, separators and all.
    fn labels(items: &[MenuItem<GuestTray>]) -> Vec<Option<String>> {
        items
            .iter()
            .map(|item| match item {
                MenuItem::Standard(item) => Some(item.label.clone()),
                MenuItem::SubMenu(item) => Some(item.label.clone()),
                MenuItem::Separator => None,
                MenuItem::Checkmark(_) | MenuItem::RadioGroup(_) => unreachable!("none are built"),
            })
            .collect()
    }

    /// Clicks the one item of the menu named by its label, wherever it sits.
    fn click(items: &[MenuItem<GuestTray>], label: &str, tray: &mut GuestTray) {
        if !click_among(items, label, tray) {
            panic!("no menu item labelled {label}");
        }
    }

    /// The body of [`click`], which searches rather than panics, so a label
    /// one branch does not hold can still be found in another.
    fn click_among(items: &[MenuItem<GuestTray>], label: &str, tray: &mut GuestTray) -> bool {
        for item in items {
            match item {
                MenuItem::Standard(item) if item.label == label => {
                    (item.activate)(tray);

                    return true;
                }
                MenuItem::SubMenu(item) if click_among(&item.submenu, label, tray) => {
                    return true;
                }
                _ => {}
            }
        }

        false
    }

    /// What a click asked the link for, as the broker would read it.
    fn sent_datagram(received: &Receiver<Command>) -> Message {
        match received
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the click sent something")
        {
            Command::Broker(message) => message,
            Command::RestartClipboard => panic!("this click is not the restart"),
        }
    }

    #[test]
    fn the_menu_is_what_the_viewer_offers_in_the_viewers_order() {
        let (tray, received) = tray_with(vec![timing(1920, 1080, 60)], None);

        assert_eq!(
            labels(&menu_of(&tray)),
            [
                Some("Full screen".to_owned()),
                Some("Send Ctrl+Alt+Del".to_owned()),
                Some("Release keyboard".to_owned()),
                None,
                Some("Resolution".to_owned()),
                Some("Mute audio".to_owned()),
                None,
                Some("Quality: Auto".to_owned()),
                Some("Quality: Desktop".to_owned()),
                None,
                Some("Restart services".to_owned()),
            ],
            "v1 is fixed English labels in the viewer's order"
        );
        assert!(received.try_recv().is_err(), "building sends nothing");
    }

    #[test]
    fn every_plain_click_is_the_wire_command_it_names() {
        // Each label with the kind it must arrive at the host as: this is
        // the mapping the broker forwards without deciding anything.
        let expectations = [
            ("Full screen", GuestCommandKind::ToggleFullscreen),
            ("Send Ctrl+Alt+Del", GuestCommandKind::SendSecureAttention),
            ("Release keyboard", GuestCommandKind::ReleaseKeyboard),
            ("Mute audio", GuestCommandKind::ToggleMute),
            ("Quality: Auto", GuestCommandKind::QualityAuto),
            ("Quality: Desktop", GuestCommandKind::QualityDesktop),
        ];

        for (label, kind) in expectations {
            let (mut tray, received) = tray_with(Vec::new(), None);
            let items = menu_of(&tray);
            click(&items, label, &mut tray);

            assert_eq!(
                decode(&encode(&sent_datagram(&received))).expect("a message this build wrote"),
                Message::UserCommand {
                    kind,
                    display_mode: None,
                },
                "{label} forwards as {kind:?}"
            );
        }
    }

    #[test]
    fn a_resolution_entry_asks_for_the_mode_it_is_labelled_with() {
        let modes = vec![timing(1280, 720, 60), timing(1920, 1080, 144)];
        let (mut tray, received) = tray_with(modes, Some(timing(1920, 1080, 144)));
        let items = menu_of(&tray);
        click(&items, "1920x1080@144", &mut tray);

        assert_eq!(
            decode(&encode(&sent_datagram(&received))).expect("a message this build wrote"),
            Message::UserCommand {
                kind: GuestCommandKind::SetDisplayMode,
                display_mode: Some(timing(1920, 1080, 144)),
            }
        );
    }

    #[test]
    fn a_resolution_submenu_shows_the_choice_on_its_header() {
        let (tray, _) = tray_with(vec![timing(1280, 720, 60)], Some(timing(1280, 720, 60)));

        assert_eq!(
            labels(&menu_of(&tray))[4],
            Some("Resolution: 1280x720@60".to_owned())
        );
    }

    #[test]
    fn with_no_answer_yet_the_resolution_submenu_promises_nothing() {
        let (tray, received) = tray_with(Vec::new(), None);
        let items = menu_of(&tray);
        let MenuItem::SubMenu(resolution) = &items[4] else {
            panic!("the resolution submenu is where v1 puts it");
        };

        assert_eq!(resolution.submenu.len(), 1);
        let MenuItem::Standard(placeholder) = &resolution.submenu[0] else {
            panic!("the placeholder is a plain item");
        };
        assert_eq!(placeholder.label, NO_MODES_LABEL);
        assert!(
            !placeholder.enabled,
            "an unclickable item lies about nothing"
        );
        assert!(received.try_recv().is_err(), "a placeholder sends nothing");
    }

    #[test]
    fn a_mode_is_labelled_width_height_and_refresh() {
        assert_eq!(mode_label(&timing(2560, 1440, 165)), "2560x1440@165");
    }

    #[test]
    fn restarting_services_asks_the_broker_and_this_bus_alike() {
        let (mut tray, received) = tray_with(Vec::new(), None);
        let items = menu_of(&tray);
        click(&items, "Restart services", &mut tray);
        assert_eq!(
            decode(&encode(&sent_datagram(&received))).expect("a message this build wrote"),
            Message::RestartSession,
            "the broker puts its own units back"
        );
        // The clipboard daemon's unit is on the session bus, which is why
        // the second half of the click goes to the link thread instead.
        assert!(matches!(
            received.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(Command::RestartClipboard)
        ));
    }

    #[test]
    fn a_tray_asks_for_the_modes_when_the_menu_is_about_to_show() {
        let (tray, received) = tray_with(Vec::new(), None);

        tray.request_modes();

        assert_eq!(
            decode(&encode(&sent_datagram(&received))).expect("a message this build wrote"),
            Message::DisplayModesRequested
        );
    }

    /// The menu builds nothing but what the labels helper above promises to
    /// name, so a new item kind cannot arrive without this test asking what
    /// it should be called.
    #[test]
    fn the_menu_builds_nothing_but_plain_items_and_submenus() {
        let (tray, _) = tray_with(Vec::new(), None);
        for item in menu_of(&tray) {
            assert!(matches!(
                item,
                MenuItem::Standard(_) | MenuItem::SubMenu(_) | MenuItem::Separator
            ));
        }
    }
}
