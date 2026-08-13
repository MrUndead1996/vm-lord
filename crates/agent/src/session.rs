//! The guest's half of a VMLord agent session.

use std::{
    error::Error,
    fmt,
    io::{Read, Write},
};

use vmlord_agent_protocol::{
    auth::{self, Nonce, Secret},
    frame::{self, FrameError},
    v1::{
        AuthenticateResponse, Envelope, ErrorCode, HeartbeatRequest, HeartbeatResponse,
        HelloRequest, ProtocolVersion, envelope, request, response,
    },
};

const HELLO_REQUEST_ID: u32 = 1;
const FIRST_HEARTBEAT_REQUEST_ID: u32 = HELLO_REQUEST_ID + 1;

/// Opens and serves the guest half of an agent session.
///
/// A clean host hang-up ends the session successfully. Other connection or
/// protocol failures end it with [`SessionError`].
pub fn run<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    version: &str,
) -> Result<(), SessionError> {
    let mut buffer = Vec::new();
    greet(stream, version, &mut buffer)?;
    authenticate(stream, secret, &mut buffer)?;
    serve(stream, &mut buffer)
}

fn greet<S: Read + Write>(
    stream: &mut S,
    version: &str,
    buffer: &mut Vec<u8>,
) -> Result<(), SessionError> {
    let hello = Envelope::request(
        HELLO_REQUEST_ID,
        request::Kind::Hello(HelloRequest {
            version: Some(ProtocolVersion::current()),
            capabilities: Vec::new(),
            agent_version: version.to_owned(),
        }),
    );
    frame::write(stream, &hello, buffer).map_err(SessionError::Frame)?;

    let envelope = frame::read(stream, buffer).map_err(SessionError::Frame)?;
    let request_id = envelope.request_id;
    match body(envelope)? {
        Body::Response(response::Kind::Hello(hello)) if request_id == HELLO_REQUEST_ID => {
            if hello.version != Some(ProtocolVersion::current()) || !hello.capabilities.is_empty() {
                return Err(SessionError::InvalidHello);
            }
            Ok(())
        }
        Body::Response(response::Kind::Error(error)) => Err(SessionError::Refused {
            code: error.code(),
            message: error.message,
        }),
        Body::Request(kind) => {
            refuse_unsupported(stream, request_id, buffer)?;
            Err(SessionError::OutOfOrder(kind_name(&kind)))
        }
        Body::Response(_) => Err(SessionError::OutOfOrder(
            "a response other than the hello reply",
        )),
    }
}

fn authenticate<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    buffer: &mut Vec<u8>,
) -> Result<(), SessionError> {
    let envelope = frame::read(stream, buffer).map_err(SessionError::Frame)?;
    let request_id = envelope.request_id;
    let nonce = match body(envelope)? {
        Body::Request(request::Kind::Authenticate(challenge)) => Nonce::from_wire(&challenge.nonce)
            .map_err(|error| SessionError::Malformed(error.to_string()))?,
        Body::Response(response::Kind::Error(error)) => {
            return Err(SessionError::Refused {
                code: error.code(),
                message: error.message,
            });
        }
        Body::Request(kind) => {
            refuse_unsupported(stream, request_id, buffer)?;
            return Err(SessionError::OutOfOrder(kind_name(&kind)));
        }
        Body::Response(_) => {
            return Err(SessionError::OutOfOrder(
                "a response instead of a challenge",
            ));
        }
    };

    let answer = Envelope::response(
        request_id,
        response::Kind::Authenticate(AuthenticateResponse {
            tag: auth::tag(secret, &nonce).as_bytes().to_vec(),
        }),
    );
    frame::write(stream, &answer, buffer).map_err(SessionError::Frame)
}

fn serve<S: Read + Write>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<(), SessionError> {
    let mut next_request_id = FIRST_HEARTBEAT_REQUEST_ID;
    let mut pending_heartbeat = None;

    loop {
        let envelope = match frame::read(stream, buffer) {
            Ok(envelope) => envelope,
            Err(FrameError::Closed) => return Ok(()),
            Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut => {
                if pending_heartbeat.is_some() {
                    return Err(SessionError::Frame(FrameError::Io(error)));
                }
                let request_id = next_request_id;
                next_request_id = next_request_id
                    .checked_add(1)
                    .ok_or(SessionError::RequestIdsExhausted)?;
                let heartbeat =
                    Envelope::request(request_id, request::Kind::Heartbeat(HeartbeatRequest {}));
                frame::write(stream, &heartbeat, buffer).map_err(SessionError::Frame)?;
                pending_heartbeat = Some(request_id);
                continue;
            }
            Err(error) => return Err(SessionError::Frame(error)),
        };

        let request_id = envelope.request_id;
        match body(envelope)? {
            Body::Request(request::Kind::Heartbeat(_)) => {
                let heartbeat =
                    Envelope::response(request_id, response::Kind::Heartbeat(HeartbeatResponse {}));
                frame::write(stream, &heartbeat, buffer).map_err(SessionError::Frame)?;
            }
            Body::Request(_) => refuse_unsupported(stream, request_id, buffer)?,
            Body::Response(response::Kind::Heartbeat(_))
                if pending_heartbeat == Some(request_id) =>
            {
                pending_heartbeat = None;
            }
            Body::Response(response::Kind::Error(error)) => {
                return Err(SessionError::Refused {
                    code: error.code(),
                    message: error.message,
                });
            }
            Body::Response(_) => return Err(SessionError::OutOfOrder("an unsolicited response")),
        }
    }
}

fn refuse_unsupported<S: Write>(
    stream: &mut S,
    request_id: u32,
    buffer: &mut Vec<u8>,
) -> Result<(), SessionError> {
    let refusal = Envelope::error(
        request_id,
        ErrorCode::UnsupportedRequest,
        "this build of vmlord-agent does not implement that host request",
    );
    frame::write(stream, &refusal, buffer).map_err(SessionError::Frame)
}

enum Body {
    Request(request::Kind),
    Response(response::Kind),
}

fn body(envelope: Envelope) -> Result<Body, SessionError> {
    match envelope.body {
        Some(envelope::Body::Request(request)) => {
            request.kind.map(Body::Request).ok_or_else(|| {
                SessionError::Malformed("a request with no kind this build knows".to_owned())
            })
        }
        Some(envelope::Body::Response(response)) => {
            response.kind.map(Body::Response).ok_or_else(|| {
                SessionError::Malformed("a response with no kind this build knows".to_owned())
            })
        }
        None => Err(SessionError::Malformed(
            "an envelope with no body".to_owned(),
        )),
    }
}

fn kind_name(kind: &request::Kind) -> &'static str {
    match kind {
        request::Kind::Hello(_) => "a hello request out of order",
        request::Kind::Authenticate(_) => "an authentication challenge out of order",
        request::Kind::Heartbeat(_) => "a heartbeat request out of order",
    }
}

/// Why the host connection ended without a clean hang-up.
#[derive(Debug)]
pub enum SessionError {
    Frame(FrameError),
    Refused { code: ErrorCode, message: String },
    InvalidHello,
    OutOfOrder(&'static str),
    Malformed(String),
    RequestIdsExhausted,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => write!(formatter, "{error}"),
            Self::Refused { code, message } => {
                write!(formatter, "the host refused ({code:?}): {message}")
            }
            Self::InvalidHello => {
                formatter.write_str("the host accepted a different agent protocol session")
            }
            Self::OutOfOrder(what) => write!(formatter, "the host sent {what}"),
            Self::Malformed(what) => write!(formatter, "the host sent {what}"),
            Self::RequestIdsExhausted => formatter.write_str("the agent exhausted its request ids"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::Refused { .. }
            | Self::InvalidHello
            | Self::OutOfOrder(_)
            | Self::Malformed(_)
            | Self::RequestIdsExhausted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::{self, Read, Write},
    };

    use vmlord_agent_protocol::{
        auth::{self, Nonce, Secret},
        frame,
        v1::{
            AuthenticateRequest, ErrorCode, HeartbeatResponse, HelloRequest, HelloResponse,
            ProtocolVersion, envelope, request, response,
        },
    };

    use super::run;

    /// A host peer that returns the exact bytes and timeout a session expects.
    struct ScriptedStream {
        reads: VecDeque<ReadStep>,
        written: Vec<u8>,
    }

    enum ReadStep {
        Bytes(Vec<u8>),
        Timeout,
    }

    impl ScriptedStream {
        fn new(steps: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                reads: steps.into_iter().collect(),
                written: Vec::new(),
            }
        }

        fn frame(envelope: vmlord_agent_protocol::v1::Envelope) -> ReadStep {
            let mut bytes = Vec::new();
            frame::encode(&envelope, &mut bytes).expect("a host frame that fits");
            ReadStep::Bytes(bytes)
        }

        fn written_frames(&self) -> Vec<vmlord_agent_protocol::v1::Envelope> {
            let mut frames = Vec::new();
            let mut unread = self.written.as_slice();
            let mut buffer = Vec::new();
            while !unread.is_empty() {
                frames.push(frame::read(&mut unread, &mut buffer).expect("a complete guest frame"));
            }
            frames
        }
    }

    impl Read for ScriptedStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self.reads.front_mut() {
                Some(ReadStep::Bytes(bytes)) => {
                    let count = bytes.len().min(buffer.len());
                    buffer[..count].copy_from_slice(&bytes[..count]);
                    bytes.drain(..count);
                    if bytes.is_empty() {
                        self.reads.pop_front();
                    }
                    Ok(count)
                }
                Some(ReadStep::Timeout) => {
                    self.reads.pop_front();
                    Err(io::Error::new(io::ErrorKind::TimedOut, "idle host"))
                }
                None => Ok(0),
            }
        }
    }

    impl Write for ScriptedStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn opens_authenticates_heartbeats_and_refuses_unsupported_host_requests() {
        // Removing the opening state machine, mishandling a receive timeout,
        // or accepting a host request this agent does not implement must fail
        // this test.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let version = ProtocolVersion::current();
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(version),
                    capabilities: vec![],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ReadStep::Timeout,
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                2,
                response::Kind::Heartbeat(HeartbeatResponse {}),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                8,
                request::Kind::Hello(HelloRequest::default()),
            )),
        ]);

        run(&mut stream, &secret, "test-agent").expect("a clean host hang-up");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 4);
        assert_eq!(
            frames[0],
            vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Hello(HelloRequest {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![],
                    agent_version: "test-agent".to_owned(),
                }),
            )
        );
        assert_eq!(frames[1].request_id, 1);
        let Some(envelope::Body::Response(authenticate)) = &frames[1].body else {
            panic!("the challenge needs an authentication response");
        };
        let Some(response::Kind::Authenticate(authenticate)) = &authenticate.kind else {
            panic!("the challenge needs an authentication tag");
        };
        assert_eq!(authenticate.tag, auth::tag(&secret, &nonce).as_bytes());
        assert_eq!(
            frames[2],
            vmlord_agent_protocol::v1::Envelope::request(
                2,
                request::Kind::Heartbeat(vmlord_agent_protocol::v1::HeartbeatRequest {}),
            )
        );
        assert_eq!(frames[3].request_id, 8);
        let Some(envelope::Body::Response(error)) = &frames[3].body else {
            panic!("the unsupported request needs an error response");
        };
        let Some(response::Kind::Error(error)) = &error.kind else {
            panic!("the unsupported request needs an error response");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }
}
