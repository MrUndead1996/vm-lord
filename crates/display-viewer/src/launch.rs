//! The contract between VMLord and one viewer process.
//!
//! Two anonymous pipes, wired as this process's stdin and stdout by whoever
//! spawned it. Nothing structural and nothing sensitive is on the command line
//! or in the environment, which is what keeps a channel key out of a process
//! listing.
//!
//! Every message names the revision it was written against, so a VMLord and a
//! viewer that disagree fail at the first message rather than part-way through
//! a stream neither can parse.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
};

use prost::Message as _;

use crate::viewer::v1::{self as wire, envelope};

/// The revision of the launch contract this build speaks.
pub const REVISION: u32 = 1;

/// The largest message a launch pipe may carry.
///
/// A hand-over is a few hundred bytes and a relay is a control record, whose
/// own cap is 64 KiB. A megabyte is far above both and far below anything that
/// would matter if the pipe were ever fed nonsense.
pub const MAX_MESSAGE: u32 = 1024 * 1024;

/// What VMLord and a viewer say to each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Everything the viewer is told at startup. Once, first.
    Launch(LaunchParameters),
    /// Handshake bytes to write to the control socket, verbatim.
    RelayToViewer(Vec<u8>),
    /// Handshake bytes read off the control socket, verbatim.
    RelayFromViewer(Vec<u8>),
    /// The one-shot derived credential. Relay mode ends here.
    Handover(Handover),
    /// The viewer asking for a new session after control was lost.
    RequestRelay {
        /// The right to ask, carried since launch.
        token: Vec<u8>,
    },
    /// Something for the window rather than for the session.
    Command(Command),
}

/// What VMLord asks the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Bring the window to the front. What a repeated Connect means.
    Focus,
    /// Close the session and exit, the way the close button does.
    Close,
}

/// Everything the viewer is told at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchParameters {
    /// For the title bar and the log. Never parsed.
    pub vm_name: String,
    /// The compute system's runtime id, which is what an HvSocket address names.
    pub runtime_id: [u8; 16],
    /// The vsock port the control service listens on.
    pub control_port: u32,
    /// The vsock port the frame service listens on.
    pub frame_port: u32,
    /// The vsock port the input service listens on.
    pub input_port: u32,
    /// The width VMLord offered, for the window before the handshake settles.
    pub width: u32,
    /// The height VMLord offered.
    pub height: u32,
    /// The tile size VMLord offered.
    pub tile_size: u32,
    /// The right to ask for a new session over these pipes.
    pub token: Vec<u8>,
    /// The `ClientHello` record to write once the control socket connects.
    pub client_hello: Vec<u8>,
}

/// The one-shot derived credential, and what the handshake settled on.
///
/// Two channel keys, good for one session and no longer. The VM's secret is
/// not here and never crosses this pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handover {
    /// The 16 bytes that name the session across its three sockets.
    pub session_id: Vec<u8>,
    /// The key the frame socket proves itself with.
    pub frame_key: Vec<u8>,
    /// The key the input socket proves itself with.
    pub input_key: Vec<u8>,
    /// The major of the revision the session runs at.
    pub version_major: u32,
    /// The minor of the revision the session runs at.
    pub version_minor: u32,
    /// The capabilities both peers have, as `vmlord.display.v1.Capability`.
    pub capabilities: Vec<i32>,
    /// The mode the guest resolved to, as `vmlord.display.v1.Mode`.
    pub mode: i32,
    /// The width the session displays.
    pub width: u32,
    /// The height the session displays.
    pub height: u32,
    /// The tile size the frame stream uses for the life of the session.
    pub tile_size: u32,
    /// The sequence the host's control channel carries on from.
    pub control_sequence: u32,
}

/// Turns a message into the bytes an envelope carries, without the prefix.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    let kind = match message {
        Message::Launch(parameters) => envelope::Kind::Launch(wire::LaunchParameters {
            vm_name: parameters.vm_name.clone(),
            runtime_id: parameters.runtime_id.to_vec(),
            control_port: parameters.control_port,
            frame_port: parameters.frame_port,
            input_port: parameters.input_port,
            width: parameters.width,
            height: parameters.height,
            tile_size: parameters.tile_size,
            token: parameters.token.clone(),
            client_hello: parameters.client_hello.clone(),
        }),
        Message::RelayToViewer(bytes) => envelope::Kind::RelayToViewer(wire::Relay {
            bytes: bytes.clone(),
        }),
        Message::RelayFromViewer(bytes) => envelope::Kind::RelayFromViewer(wire::Relay {
            bytes: bytes.clone(),
        }),
        Message::Handover(handover) => envelope::Kind::Handover(wire::Handover {
            session_id: handover.session_id.clone(),
            frame_key: handover.frame_key.clone(),
            input_key: handover.input_key.clone(),
            version_major: handover.version_major,
            version_minor: handover.version_minor,
            capabilities: handover.capabilities.clone(),
            mode: handover.mode,
            width: handover.width,
            height: handover.height,
            tile_size: handover.tile_size,
            control_sequence: handover.control_sequence,
        }),
        Message::RequestRelay { token } => envelope::Kind::RequestRelay(wire::RequestRelay {
            token: token.clone(),
        }),
        Message::Command(command) => envelope::Kind::Command(wire::Command {
            kind: match command {
                Command::Focus => wire::command::Kind::Focus as i32,
                Command::Close => wire::command::Kind::Close as i32,
            },
        }),
    };

    wire::Envelope {
        revision: REVISION,
        kind: Some(kind),
    }
    .encode_to_vec()
}

/// Reads a message back out of an envelope's bytes.
///
/// # Errors
///
/// [`LaunchError::Decode`] for bytes that are not an envelope,
/// [`LaunchError::Revision`] for one written against another contract,
/// [`LaunchError::Empty`] for an envelope naming no message, and
/// [`LaunchError::Field`] for a fixed-width field that arrived at another
/// width.
pub fn decode(bytes: &[u8]) -> Result<Message, LaunchError> {
    let envelope = wire::Envelope::decode(bytes).map_err(LaunchError::Decode)?;
    if envelope.revision != REVISION {
        return Err(LaunchError::Revision {
            expected: REVISION,
            found: envelope.revision,
        });
    }

    let message = match envelope.kind.ok_or(LaunchError::Empty)? {
        envelope::Kind::Launch(parameters) => Message::Launch(LaunchParameters {
            vm_name: parameters.vm_name,
            runtime_id: parameters.runtime_id.as_slice().try_into().map_err(|_| {
                LaunchError::Field {
                    what: "runtime id",
                    len: parameters.runtime_id.len(),
                }
            })?,
            control_port: parameters.control_port,
            frame_port: parameters.frame_port,
            input_port: parameters.input_port,
            width: parameters.width,
            height: parameters.height,
            tile_size: parameters.tile_size,
            token: parameters.token,
            client_hello: parameters.client_hello,
        }),
        envelope::Kind::RelayToViewer(relay) => Message::RelayToViewer(relay.bytes),
        envelope::Kind::RelayFromViewer(relay) => Message::RelayFromViewer(relay.bytes),
        envelope::Kind::Handover(handover) => Message::Handover(Handover {
            session_id: handover.session_id,
            frame_key: handover.frame_key,
            input_key: handover.input_key,
            version_major: handover.version_major,
            version_minor: handover.version_minor,
            capabilities: handover.capabilities,
            mode: handover.mode,
            width: handover.width,
            height: handover.height,
            tile_size: handover.tile_size,
            control_sequence: handover.control_sequence,
        }),
        envelope::Kind::RequestRelay(request) => Message::RequestRelay {
            token: request.token,
        },
        envelope::Kind::Command(command) => {
            Message::Command(match wire::command::Kind::try_from(command.kind) {
                Ok(wire::command::Kind::Focus) => Command::Focus,
                Ok(wire::command::Kind::Close) => Command::Close,
                _ => return Err(LaunchError::Empty),
            })
        }
    };

    Ok(message)
}

/// Reads the one message a viewer must be started with.
///
/// A viewer launched with no usable standard input -- double-clicked from
/// Explorer, say -- has no VM to talk to and invents none. What comes back is
/// the message to put in the error window before exiting.
///
/// # Errors
///
/// The text to show the user, for a pipe with nothing on it or a first message
/// that is not [`Message::Launch`].
pub fn first_parameters<R: Read, W: Write>(
    link: &mut Link<R, W>,
) -> Result<LaunchParameters, String> {
    match link.read() {
        Ok(Message::Launch(parameters)) => Ok(parameters),
        Ok(other) => Err(format!(
            "VMLord Display was started with a {other:?} rather than its launch parameters. \
             It is opened from VMLord, through Connect on a VM's display."
        )),
        Err(error) => Err(format!(
            "VMLord Display cannot be started on its own ({error}). \
             It is opened from VMLord, through Connect on a VM's display."
        )),
    }
}

/// The pair of pipes, framed.
///
/// Generic over the two halves so that a test can put them in memory: what the
/// binary passes is standard input and standard output.
pub struct Link<R: Read, W: Write> {
    reader: R,
    writer: W,
    payload: Vec<u8>,
}

impl<R: Read, W: Write> Link<R, W> {
    /// A link over one reader and one writer.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            payload: Vec::new(),
        }
    }

    /// Waits for the next message.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Closed`] when the far end hung up at a message boundary,
    /// which is what a VMLord that exited looks like; [`LaunchError::TooLarge`]
    /// for a prefix above [`MAX_MESSAGE`], refused before anything is
    /// allocated; and whatever [`decode`] can return.
    pub fn read(&mut self) -> Result<Message, LaunchError> {
        let mut prefix = [0u8; 4];
        let mut filled = 0;
        while filled < prefix.len() {
            match self.reader.read(&mut prefix[filled..]) {
                Ok(0) if filled == 0 => return Err(LaunchError::Closed),
                Ok(0) => {
                    return Err(LaunchError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "a launch pipe ended part-way through a length prefix",
                    )));
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(LaunchError::Io(error)),
            }
        }

        let length = u32::from_le_bytes(prefix);
        if length > MAX_MESSAGE {
            return Err(LaunchError::TooLarge {
                length,
                cap: MAX_MESSAGE,
            });
        }

        self.payload.clear();
        self.payload.resize(length as usize, 0);
        self.reader
            .read_exact(&mut self.payload)
            .map_err(LaunchError::Io)?;

        decode(&self.payload)
    }

    /// Writes one message and flushes it.
    ///
    /// Flushing belongs here: a buffered pipe that holds a relay back is a
    /// handshake that appears to have stalled.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Io`] if the pipe failed.
    pub fn write(&mut self, message: &Message) -> Result<(), LaunchError> {
        let bytes = encode(message);
        let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        if length > MAX_MESSAGE {
            return Err(LaunchError::TooLarge {
                length,
                cap: MAX_MESSAGE,
            });
        }

        self.writer
            .write_all(&length.to_le_bytes())
            .map_err(LaunchError::Io)?;
        self.writer.write_all(&bytes).map_err(LaunchError::Io)?;
        self.writer.flush().map_err(LaunchError::Io)
    }
}

/// Why a launch pipe could not be used.
#[derive(Debug)]
pub enum LaunchError {
    /// The far end hung up at a message boundary.
    Closed,
    /// A message from another revision of this contract.
    Revision {
        /// What this build speaks.
        expected: u32,
        /// What arrived.
        found: u32,
    },
    /// An envelope that names no message, or a command this build has no name
    /// for.
    Empty,
    /// A fixed-width field arrived at another width.
    Field {
        /// Which field.
        what: &'static str,
        /// How long it was.
        len: usize,
    },
    /// A message longer than the pipe's cap.
    TooLarge {
        /// What the prefix announced.
        length: u32,
        /// What this build allows.
        cap: u32,
    },
    /// The bytes are not an envelope.
    Decode(prost::DecodeError),
    /// The pipe failed.
    Io(io::Error),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("the launch pipe was closed by VMLord"),
            Self::Revision { expected, found } => write!(
                formatter,
                "a launch message of revision {found} arrived where this build speaks {expected}"
            ),
            Self::Empty => formatter.write_str("a launch message names nothing this build knows"),
            Self::Field { what, len } => {
                write!(formatter, "a {what} of {len} bytes is the wrong width")
            }
            Self::TooLarge { length, cap } => write!(
                formatter,
                "a {length}-byte launch message exceeds the {cap}-byte limit"
            ),
            Self::Decode(error) => write!(formatter, "a launch message is unreadable: {error}"),
            Self::Io(error) => write!(formatter, "a launch pipe failed: {error}"),
        }
    }
}

impl Error for LaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{Command, Handover, LaunchError, LaunchParameters, Link, Message, decode, encode};

    fn parameters() -> LaunchParameters {
        LaunchParameters {
            vm_name: "ubuntu-24.04".to_owned(),
            runtime_id: [7; 16],
            control_port: 0x564D_4C44,
            frame_port: 0x564D_4C46,
            input_port: 0x564D_4C49,
            width: 1920,
            height: 1080,
            tile_size: 32,
            token: vec![9; 32],
            client_hello: vec![1, 2, 3, 4],
        }
    }

    fn handover() -> Handover {
        Handover {
            session_id: vec![3; 16],
            frame_key: vec![4; 32],
            input_key: vec![5; 32],
            version_major: 1,
            version_minor: 0,
            capabilities: vec![1],
            mode: 2,
            width: 1920,
            height: 1080,
            tile_size: 32,
            control_sequence: 2,
        }
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        let messages = [
            Message::Launch(parameters()),
            Message::RelayToViewer(vec![0xaa; 64]),
            Message::RelayFromViewer(vec![0xbb; 64]),
            Message::Handover(handover()),
            Message::RequestRelay { token: vec![9; 32] },
            Message::Command(Command::Focus),
            Message::Command(Command::Close),
        ];

        for message in messages {
            assert_eq!(
                decode(&encode(&message)).expect("a message this build wrote"),
                message
            );
        }
    }

    #[test]
    fn a_message_from_another_revision_is_refused() {
        let mut bytes = encode(&Message::Command(Command::Focus));
        // Field 1, varint: the revision is the first two bytes of the envelope.
        assert_eq!(bytes[0], 0x08);
        bytes[1] = 99;

        assert!(matches!(
            decode(&bytes),
            Err(LaunchError::Revision { found: 99, .. })
        ));
    }

    #[test]
    fn an_envelope_naming_no_message_is_refused() {
        // Revision 1 and nothing else.
        assert!(matches!(decode(&[0x08, 0x01]), Err(LaunchError::Empty)));
    }

    #[test]
    fn bytes_that_are_not_an_envelope_are_refused() {
        assert!(decode(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn a_link_carries_messages_both_ways() {
        let mut pipe = Vec::new();
        {
            let mut link = Link::new(io::empty(), &mut pipe);
            link.write(&Message::Command(Command::Close))
                .expect("an in-memory writer");
        }

        let mut link = Link::new(pipe.as_slice(), io::sink());
        assert_eq!(
            link.read().expect("what was just written"),
            Message::Command(Command::Close)
        );
    }

    #[test]
    fn a_link_whose_parent_is_gone_reports_a_closed_pipe() {
        let mut link = Link::new(io::empty(), io::sink());

        assert!(matches!(link.read(), Err(LaunchError::Closed)));
    }

    #[test]
    fn a_length_prefix_above_the_cap_is_refused_before_anything_is_allocated() {
        let prefix = (super::MAX_MESSAGE + 1).to_le_bytes();
        let mut link = Link::new(prefix.as_slice(), io::sink());

        assert!(matches!(link.read(), Err(LaunchError::TooLarge { .. })));
    }

    #[test]
    fn a_viewer_started_without_launch_parameters_says_how_it_is_started() {
        // What a double-click from Explorer produces: no parent, no pipe, no
        // first message.
        let mut link = Link::new(io::empty(), io::sink());
        let outcome = super::first_parameters(&mut link);

        let message = outcome.expect_err("there are no parameters on an empty pipe");
        assert!(
            message.contains("VMLord"),
            "the message must name the only supported way to start this program"
        );
    }

    #[test]
    fn a_first_message_that_is_not_launch_parameters_is_refused() {
        let mut pipe = Vec::new();
        {
            let mut link = Link::new(io::empty(), &mut pipe);
            link.write(&Message::Command(Command::Focus))
                .expect("an in-memory writer");
        }

        let mut link = Link::new(pipe.as_slice(), io::sink());

        assert!(super::first_parameters(&mut link).is_err());
    }
}
