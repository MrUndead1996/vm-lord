//! VMLord's half of one display session.
//!
//! The viewer owns the three sockets and VMLord owns the VM's secret, so
//! neither can run the control handshake alone: the viewer frames records off
//! the wire and passes the bytes up a pipe without reading into them, and what
//! is here drives the protocol's `Session` over those bytes and hands back the
//! one-shot credential the viewer needs to bind its other two.
//!
//! A state machine and not a thread. Everything that decides anything is
//! reachable from a test with no process, no partition and no window;
//! `display_launches` is what puts a process and two pipes around it.
//!
//! Nothing here formats a secret, a token or a channel key. The VM's secret
//! never leaves this side, and what does cross the pipe is good for one
//! session and no longer.

use uuid::Uuid;
use vmlord_core::{DiagnosticLevel, DisplayMode};
use vmlord_display_protocol::{
    keys::{self, Secret},
    record::{self, Channel, Limits, Record},
    session::{Event, Offer, Session},
    v1::{Capability, Mode},
};
use vmlord_display_viewer::launch::{Handover, LaunchParameters, Message};

use crate::hvsocket::{
    DISPLAY_CLIPBOARD_VSOCK_PORT, DISPLAY_CONTROL_VSOCK_PORT, DISPLAY_FRAME_VSOCK_PORT,
    DISPLAY_INPUT_VSOCK_PORT,
};

/// The width a VM with no stored mode is offered.
///
/// Only what the window opens at before the handshake settles: the viewer
/// prefers whatever it remembered for this VM, and the resize path replaces
/// both within a second of the desktop appearing.
const DEFAULT_WIDTH: u32 = 1920;

/// The height a VM with no stored mode is offered.
const DEFAULT_HEIGHT: u32 = 1080;

/// The tile size the host asks for.
///
/// One of the three the guest's encoder builds, and the one its benchmarks
/// were taken at.
const TILE_SIZE: u32 = 32;

/// How many bytes prove the right to ask for another session on these pipes.
const TOKEN_LEN: usize = 32;

/// Control records only, so the frame caps have no geometry to depend on.
fn control_limits() -> Limits {
    Limits::new(0, 0)
}

/// What VMLord answers one launch-pipe message with.
pub(crate) struct Answer {
    /// What to write back down the pipe, in order.
    pub(crate) to_viewer: Vec<Message>,
    /// What the rest of VMLord is to be told, if anything: a level and a
    /// sentence.
    ///
    /// Not a whole record: which subsystem this is and which VM it is about
    /// are known by the launcher that reports it, and a driver that had to
    /// name them would be repeating what its caller already knows.
    pub(crate) diagnostics: Vec<(DiagnosticLevel, String)>,
}

impl Answer {
    /// An answer that says nothing, which is most of them.
    fn nothing() -> Self {
        Self {
            to_viewer: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// An answer that only reports.
    fn reported(level: DiagnosticLevel, message: String) -> Self {
        Self {
            to_viewer: Vec::new(),
            diagnostics: vec![(level, message)],
        }
    }
}

/// The host end of one viewer's launch pipes.
pub(crate) struct Driver {
    vm_name: String,
    secret: Secret,
    offer: Offer,
    /// The right to ask for another session, as it was handed to the viewer.
    token: Vec<u8>,
    /// The handshake in progress.
    ///
    /// `None` once a hand-over has been sent: from then on the viewer owns the
    /// session, and the only thing this side may be asked for is a new one.
    session: Option<Session>,
}

impl Driver {
    /// Opens a session and returns what the viewer is to be started with.
    pub(crate) fn open(
        vm_name: &str,
        secret: Secret,
        runtime_id: Uuid,
        mode: Option<DisplayMode>,
    ) -> (Self, LaunchParameters) {
        let offer = Offer {
            // What the guest announces and what this viewer implements.
            capabilities: vec![
                Capability::CursorStream,
                Capability::DynamicResolution,
                Capability::Clipboard,
            ],
            // A host-side policy that resolves to `Desktop` until a motion
            // codec exists. The guest is what resolves it.
            mode: Mode::Auto,
            width: mode.map_or(DEFAULT_WIDTH, |mode| mode.width()),
            height: mode.map_or(DEFAULT_HEIGHT, |mode| mode.height()),
            tile_size: TILE_SIZE,
        };
        let token = keys::random_bytes::<TOKEN_LEN>().to_vec();
        let (session, hello) = Session::host(&secret, offer.clone());

        let parameters = LaunchParameters {
            vm_name: vm_name.to_owned(),
            // The bytes an HvSocket address names the partition by. The viewer
            // reads them big-endian, which is the order `Uuid` writes.
            runtime_id: *runtime_id.as_bytes(),
            control_port: DISPLAY_CONTROL_VSOCK_PORT,
            frame_port: DISPLAY_FRAME_VSOCK_PORT,
            input_port: DISPLAY_INPUT_VSOCK_PORT,
            clipboard_port: DISPLAY_CLIPBOARD_VSOCK_PORT,
            width: offer.width,
            height: offer.height,
            tile_size: offer.tile_size,
            token: token.clone(),
            client_hello: framed(&hello),
        };

        (
            Self {
                vm_name: vm_name.to_owned(),
                secret,
                offer,
                token,
                session: Some(session),
            },
            parameters,
        )
    }

    /// Answers one message from the viewer.
    pub(crate) fn handle(&mut self, message: Message) -> Answer {
        match message {
            Message::RelayFromViewer(bytes) => self.relay(&bytes),
            Message::RequestRelay { token } => self.open_another_session(&token),
            other => {
                // A viewer that sends what only VMLord sends is a build that
                // disagrees with this one, and the launch contract's revision
                // check catches the ordinary form of that.
                tracing::warn!(
                    "the display window of VM \"{}\" sent a {} VMLord does not answer",
                    self.vm_name,
                    name_of(&other)
                );
                Answer::nothing()
            }
        }
    }

    /// Feeds one relayed record into the handshake.
    fn relay(&mut self, bytes: &[u8]) -> Answer {
        if self.session.is_none() {
            // The session is the viewer's from the hand-over on, so a record
            // arriving here is one the far side no longer needs answered.
            tracing::debug!(
                "a record arrived for VM \"{}\" after its session was handed over",
                self.vm_name
            );
            return Answer::nothing();
        }

        let mut payload = Vec::new();
        let outcome = {
            let session = self
                .session
                .as_mut()
                .expect("the session was there a line ago");
            let mut cursor = bytes;
            record::read(&mut cursor, &control_limits(), &mut payload)
                .map_err(|error| error.to_string())
                .and_then(|header| {
                    session
                        .handle(&header, &payload)
                        .map_err(|error| error.to_string())
                })
        };

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(reason) => {
                // Every one of these ends the session: a record out of turn, a
                // version with nothing to negotiate down to, a proof that did
                // not check out. The viewer notices the silence and starts
                // again with a session of its own asking.
                self.session = None;
                return Answer::reported(
                    DiagnosticLevel::Error,
                    format!(
                        "The display of VM \"{}\" could not be opened: {reason}",
                        self.vm_name
                    ),
                );
            }
        };

        let mut answer = Answer::nothing();
        if let Some(reply) = outcome.reply {
            answer
                .to_viewer
                .push(Message::RelayToViewer(framed(&reply)));
        }
        if outcome.event == Event::ControlEstablished {
            match self.hand_over() {
                Ok(handover) => {
                    answer.diagnostics.push((
                        DiagnosticLevel::Info,
                        format!(
                            "Display of VM \"{}\" opened at {}x{}",
                            self.vm_name, handover.width, handover.height
                        ),
                    ));
                    answer.to_viewer.push(Message::Handover(handover));
                }
                Err(reason) => answer.diagnostics.push((DiagnosticLevel::Error, reason)),
            }
            self.session = None;
        }

        answer
    }

    /// Builds what an established session hands the viewer.
    fn hand_over(&mut self) -> Result<Handover, String> {
        let vm_name = self.vm_name.clone();
        let missing = |what: &str| format!("The display session of VM \"{vm_name}\" has no {what}");
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| missing("handshake to hand over"))?;

        let negotiated = session
            .negotiated()
            .ok_or_else(|| missing("agreed geometry"))?
            .clone();
        let frame = session
            .derive_channel_key(Channel::Frame)
            .ok_or_else(|| missing("frame key"))?;
        let input = session
            .derive_channel_key(Channel::Input)
            .ok_or_else(|| missing("input key"))?;
        let clipboard = session
            .derive_channel_key(Channel::Clipboard)
            .ok_or_else(|| missing("clipboard key"))?;

        Ok(Handover {
            session_id: session.session_id().to_vec(),
            frame_key: frame.to_bytes().to_vec(),
            input_key: input.to_bytes().to_vec(),
            clipboard_key: clipboard.to_bytes().to_vec(),
            version_major: negotiated.version.major,
            version_minor: negotiated.version.minor,
            capabilities: negotiated
                .capabilities
                .iter()
                .map(|capability| i32::from(*capability))
                .collect(),
            mode: i32::from(negotiated.mode),
            width: negotiated.width,
            height: negotiated.height,
            tile_size: negotiated.tile_size,
            control_sequence: session.control_sequence(),
        })
    }

    /// Answers a viewer that lost control and wants another session.
    fn open_another_session(&mut self, token: &[u8]) -> Answer {
        if !same_bytes(token, &self.token) {
            return Answer::reported(
                DiagnosticLevel::Error,
                format!(
                    "Something without the right to it asked VMLord for a display session of \
                     VM \"{}\"; it was refused",
                    self.vm_name
                ),
            );
        }

        let (session, hello) = Session::host(&self.secret, self.offer.clone());
        self.session = Some(session);
        tracing::info!(
            "the display window of VM \"{}\" lost control and asked for another session",
            self.vm_name
        );

        Answer {
            to_viewer: vec![Message::RelayToViewer(framed(&hello))],
            diagnostics: Vec::new(),
        }
    }
}

/// The bytes of one record, header first.
fn framed(record: &Record) -> Vec<u8> {
    let mut bytes = record.header.encode().to_vec();
    bytes.extend_from_slice(&record.payload);
    bytes
}

/// Compares two byte strings without saying where they differ.
///
/// A token is compared and never logged, and a comparison that stops at the
/// first difference is one that can be walked a byte at a time.
fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

/// What a message is called, for a log line that must not carry its contents.
fn name_of(message: &Message) -> &'static str {
    match message {
        Message::Launch(_) => "launch",
        Message::RelayToViewer(_) => "relay",
        Message::RelayFromViewer(_) => "relayed record",
        Message::Handover(_) => "hand-over",
        Message::RequestRelay { .. } => "session request",
        Message::Command(_) => "window command",
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;
    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Record},
        session::{Session, Support},
        v1::{Capability, Mode},
    };
    use vmlord_display_viewer::launch::Message;

    use super::{Driver, control_limits, framed};

    /// What the MVP guest announces.
    fn support() -> Support {
        Support {
            capabilities: vec![
                Capability::CursorStream,
                Capability::DynamicResolution,
                Capability::Clipboard,
            ],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        }
    }

    /// A driver and the parameters its viewer would be started with.
    fn driver(mode: Option<vmlord_core::DisplayMode>) -> (Driver, Secret, Vec<u8>, Vec<u8>) {
        let secret = Secret::generate();
        let guest_secret = Secret::from_base64(&secret.to_base64()).expect("the same secret");
        let (driver, parameters) = Driver::open("dev", secret, Uuid::from_u128(7), mode);

        (
            driver,
            guest_secret,
            parameters.client_hello,
            parameters.token,
        )
    }

    /// Runs a whole handshake between a driver and a guest that answers it.
    fn handshake(driver: &mut Driver, hello: Vec<u8>, secret: &Secret) -> Message {
        let mut guest = Session::guest(secret, support());
        let mut to_guest = vec![hello];

        for _ in 0..8 {
            let mut from_guest = Vec::new();
            for bytes in to_guest.drain(..) {
                let mut cursor = bytes.as_slice();
                let mut payload = Vec::new();
                let header = record::read(&mut cursor, &control_limits(), &mut payload)
                    .expect("VMLord frames whole records");
                let outcome = guest.handle(&header, &payload).expect("a valid record");
                if let Some(reply) = outcome.reply {
                    from_guest.push(framed(&reply));
                }
                if let Some(auth) = guest.pending_auth() {
                    from_guest.push(framed(&auth));
                }
            }
            assert!(!from_guest.is_empty(), "the handshake stalled");

            for bytes in from_guest {
                let answer = driver.handle(Message::RelayFromViewer(bytes));
                for message in answer.to_viewer {
                    match message {
                        Message::RelayToViewer(bytes) => to_guest.push(bytes),
                        handover @ Message::Handover(_) => return handover,
                        other => panic!("VMLord said {other:?} during a handshake"),
                    }
                }
            }
        }

        panic!("the handshake did not finish");
    }

    #[test]
    fn a_handshake_ends_in_a_hand_over_a_viewer_can_use() {
        let (mut driver, guest_secret, hello, _token) = driver(None);

        let Message::Handover(handover) = handshake(&mut driver, hello, &guest_secret) else {
            panic!("a handshake ends in a hand-over");
        };

        assert_eq!(handover.session_id.len(), 16);
        assert_eq!(handover.frame_key.len(), 32);
        assert_eq!(handover.input_key.len(), 32);
        assert_eq!(handover.clipboard_key.len(), 32);
        assert_ne!(
            handover.clipboard_key, handover.frame_key,
            "one channel's key never opens another's socket"
        );
        assert_eq!((handover.width, handover.height), (1920, 1080));
        assert_eq!(
            handover.mode,
            i32::from(Mode::Desktop),
            "the guest resolves the host's Auto, and has one mode to resolve it to"
        );
    }

    #[test]
    fn the_launch_parameters_name_the_partition_and_the_three_ports() {
        let (_driver, parameters) =
            Driver::open("dev", Secret::generate(), Uuid::from_u128(7), None);

        assert_eq!(parameters.vm_name, "dev");
        assert_eq!(parameters.runtime_id, *Uuid::from_u128(7).as_bytes());
        assert_eq!(parameters.control_port, 0x564D_4C44);
        assert_eq!(parameters.frame_port, 0x564D_4C46);
        assert_eq!(parameters.input_port, 0x564D_4C49);
        assert_eq!(parameters.clipboard_port, 0x564D_4C43);
        assert_eq!(parameters.token.len(), 32);
    }

    #[test]
    fn a_stored_mode_is_what_the_window_is_offered_before_the_handshake() {
        let (_driver, parameters) = Driver::open(
            "dev",
            Secret::generate(),
            Uuid::from_u128(7),
            vmlord_core::DisplayMode::new(2560, 1440),
        );

        assert_eq!((parameters.width, parameters.height), (2560, 1440));
    }

    #[test]
    fn a_vm_with_no_stored_mode_is_offered_the_size_every_vm_has_come_up_at() {
        let (_driver, parameters) =
            Driver::open("dev", Secret::generate(), Uuid::from_u128(7), None);

        assert_eq!((parameters.width, parameters.height), (1920, 1080));
    }

    #[test]
    fn a_request_carrying_the_right_token_opens_another_session() {
        let (mut driver, _guest_secret, hello, token) = driver(None);

        let answer = driver.handle(Message::RequestRelay { token });

        let [Message::RelayToViewer(fresh)] = answer.to_viewer.as_slice() else {
            panic!("a request with the right token is answered with a hello");
        };
        assert_ne!(
            *fresh, hello,
            "a second session draws its own nonce, so its hello differs"
        );
        assert!(answer.diagnostics.is_empty());
    }

    #[test]
    fn a_request_carrying_the_wrong_token_is_refused_and_reported() {
        let (mut driver, _guest_secret, _hello, _token) = driver(None);

        let answer = driver.handle(Message::RequestRelay { token: vec![0; 32] });

        assert!(answer.to_viewer.is_empty());
        assert_eq!(answer.diagnostics.len(), 1);
    }

    #[test]
    fn a_record_the_session_refuses_ends_the_attempt_with_one_diagnostic() {
        let (mut driver, _guest_secret, _hello, _token) = driver(None);
        // A `ClientHello` back from the guest: a control record of a type the
        // host is not waiting for.
        let out_of_turn = Record::new(
            vmlord_display_protocol::record::Channel::Control,
            vmlord_display_protocol::v1::ControlRecord::ClientHello as u16,
            1,
            0,
            0,
            Vec::new(),
        );

        let answer = driver.handle(Message::RelayFromViewer(framed(&out_of_turn)));

        assert!(answer.to_viewer.is_empty());
        assert_eq!(answer.diagnostics.len(), 1);

        let after = driver.handle(Message::RelayFromViewer(framed(&out_of_turn)));
        assert!(
            after.diagnostics.is_empty(),
            "a session that ended is not a session that keeps reporting"
        );
    }

    #[test]
    fn bytes_that_are_not_a_record_are_reported_rather_than_parsed() {
        let (mut driver, _guest_secret, _hello, _token) = driver(None);

        let answer = driver.handle(Message::RelayFromViewer(vec![0; 24]));

        assert!(answer.to_viewer.is_empty());
        assert_eq!(answer.diagnostics.len(), 1);
    }
}
