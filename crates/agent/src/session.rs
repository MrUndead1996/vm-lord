//! The guest's half of a VMLord agent session.

use std::{
    error::Error,
    fmt,
    io::{Read, Write},
};

use vmlord_agent_protocol::{
    auth::{self, Nonce, Secret},
    frame::{self, FrameError},
    handshake::{self, CURRENT_VERSION},
    v1::{
        ApplyGpuRecipeResponse, AttachGpuSharesResponse, AuthenticateResponse, Capability,
        Envelope, ErrorCode, GpuMount, GpuRecipeStage, GpuShare, HeartbeatRequest,
        HeartbeatResponse, HelloRequest, ProbeGpuResponse, ProtocolVersion, envelope, request,
        response,
    },
};

const HELLO_REQUEST_ID: u32 = 1;
const FIRST_HEARTBEAT_REQUEST_ID: u32 = HELLO_REQUEST_ID + 1;

/// What this build of the agent implements beyond the base protocol.
///
/// `Capability::Gpu` is announced unconditionally, because it says what this
/// build can do rather than what its VM has: an agent on a VM with no GPU is
/// asked for no manifest, and one that was given shares can mount them.
const AGENT_CAPABILITIES: &[Capability] = &[Capability::Gpu];

/// What a session agreed on when it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The revision both peers speak, which is the lower of the two minors.
    pub version: ProtocolVersion,
    /// The capabilities both peers have, which is the only set either may use.
    pub capabilities: Vec<Capability>,
}

/// Opens and serves the guest half of an agent session.
///
/// `attach` is what a GPU share manifest is carried out with, `apply` is what
/// the guest's GPU recipe is run by, and `probe` is what looks at whether any
/// of it renders. They are parameters rather than calls into the modules that
/// implement them because the order of the messages around them has to be
/// testable against a peer made of bytes, and neither mounting a Hyper-V Plan9
/// share, building a kernel module nor holding a GL context can happen under a
/// `cargo test`.
///
/// The session that was agreed on is returned so the caller can tell a
/// connection that reached an authenticated session from one that did not:
/// that is the difference between a host that is there and a host that is not,
/// and it is what the reconnect backoff is decided on.
///
/// A clean host hang-up ends the session successfully. Other connection or
/// protocol failures end it with [`SessionError`].
///
/// # Errors
///
/// [`SessionError`] if the connection failed, the host answered the hello with
/// something this build never offered, or the host sent something that has no
/// place where it arrived.
pub fn run<S, A, R, P>(
    stream: &mut S,
    secret: &Secret,
    version: &str,
    opened: &mut Option<Session>,
    attach: A,
    apply: R,
    probe: P,
) -> Result<(), SessionError>
where
    S: Read + Write,
    A: FnMut(&[GpuShare]) -> (Vec<GpuMount>, bool),
    R: FnMut() -> Vec<GpuRecipeStage>,
    P: FnMut() -> ProbeGpuResponse,
{
    let mut buffer = Vec::new();
    let session = greet(stream, version, &mut buffer)?;
    authenticate(stream, secret, &mut buffer)?;
    let session = opened.insert(session);
    serve(stream, session, attach, apply, probe, &mut buffer)
}

fn greet<S: Read + Write>(
    stream: &mut S,
    version: &str,
    buffer: &mut Vec<u8>,
) -> Result<Session, SessionError> {
    let hello = Envelope::request(
        HELLO_REQUEST_ID,
        request::Kind::Hello(HelloRequest {
            version: Some(CURRENT_VERSION),
            capabilities: AGENT_CAPABILITIES.iter().copied().map(i32::from).collect(),
            agent_version: version.to_owned(),
        }),
    );
    frame::write(stream, &hello, buffer).map_err(SessionError::Frame)?;

    let envelope = frame::read(stream, buffer).map_err(SessionError::Frame)?;
    let request_id = envelope.request_id;
    match body(envelope)? {
        Body::Response(response::Kind::Hello(hello)) if request_id == HELLO_REQUEST_ID => {
            confirm(hello)
        }
        Body::Response(response::Kind::Error(error)) => Err(SessionError::Refused {
            code: error.code(),
            message: error.message,
        }),
        Body::Request(kind) => {
            refuse_unsupported(stream, request_id, buffer)?;
            Err(SessionError::OutOfOrder(kind_name(&kind)))
        }
        Body::UnknownRequest => {
            refuse_unsupported(stream, request_id, buffer)?;
            Err(SessionError::OutOfOrder(
                "an unsupported request out of order",
            ))
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
        Body::UnknownRequest => {
            refuse_unsupported(stream, request_id, buffer)?;
            return Err(SessionError::OutOfOrder(
                "an unsupported request out of order",
            ));
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

fn serve<S, A, R, P>(
    stream: &mut S,
    session: &Session,
    mut attach: A,
    mut apply: R,
    mut probe: P,
    buffer: &mut Vec<u8>,
) -> Result<(), SessionError>
where
    S: Read + Write,
    A: FnMut(&[GpuShare]) -> (Vec<GpuMount>, bool),
    R: FnMut() -> Vec<GpuRecipeStage>,
    P: FnMut() -> ProbeGpuResponse,
{
    let mut next_request_id = FIRST_HEARTBEAT_REQUEST_ID;
    let mut pending_heartbeat = None;

    loop {
        let envelope = match frame::read(stream, buffer) {
            Ok(envelope) => envelope,
            Err(FrameError::Closed) => return Ok(()),
            Err(FrameError::Idle) => {
                if pending_heartbeat.is_some() {
                    return Err(SessionError::HeartbeatUnanswered);
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
            // A manifest is the host's to send and the guest's to carry out,
            // and it is answered from the same place a heartbeat is: mounting
            // takes seconds, and a session that answered nothing meanwhile
            // would be a session with two conversations in it.
            Body::Request(request::Kind::AttachGpuShares(manifest))
                if session.capabilities.contains(&Capability::Gpu) =>
            {
                let (mounts, libraries_refreshed) = attach(&manifest.shares);
                let report = Envelope::response(
                    request_id,
                    response::Kind::AttachGpuShares(AttachGpuSharesResponse {
                        mounts,
                        libraries_refreshed,
                    }),
                );
                frame::write(stream, &report, buffer).map_err(SessionError::Frame)?;
            }
            // A recipe is minutes of work rather than seconds, and it is still
            // answered from here: the host sends nothing that needs an answer
            // meanwhile, and a second thread would be two conversations on one
            // socket for a report that was asked for.
            Body::Request(request::Kind::ApplyGpuRecipe(_))
                if session.capabilities.contains(&Capability::Gpu) =>
            {
                let stages = apply();
                let report = Envelope::response(
                    request_id,
                    response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse { stages }),
                );
                frame::write(stream, &report, buffer).map_err(SessionError::Frame)?;
            }
            // The probe follows the recipe and answers from the same place: it
            // runs two short programs rather than a build, and a thread of its
            // own would be two conversations on one socket.
            Body::Request(request::Kind::ProbeGpu(_))
                if session.capabilities.contains(&Capability::Gpu) =>
            {
                let report = Envelope::response(request_id, response::Kind::ProbeGpu(probe()));
                frame::write(stream, &report, buffer).map_err(SessionError::Frame)?;
            }
            Body::Request(_) | Body::UnknownRequest => {
                refuse_unsupported(stream, request_id, buffer)?;
            }
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

/// Reads the host's answer to the hello as the session it agreed to.
///
/// Both halves are checked against what this build announced rather than
/// merely recorded: a revision the agent never claimed to speak and a
/// capability it never offered are each a host expecting messages nothing here
/// has an arm for, and there is no round of this handshake left to settle that
/// in.
fn confirm(hello: vmlord_agent_protocol::v1::HelloResponse) -> Result<Session, SessionError> {
    let chosen = hello.version.unwrap_or_default();
    let version = handshake::confirm_version(CURRENT_VERSION, chosen)
        .map_err(|mismatch| SessionError::InvalidHello(mismatch.to_string()))?;
    let capabilities = handshake::confirm_capabilities(AGENT_CAPABILITIES, &hello.capabilities)
        .map_err(|unoffered| SessionError::InvalidHello(unoffered.to_string()))?;

    Ok(Session {
        version,
        capabilities,
    })
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
    UnknownRequest,
    Response(response::Kind),
}

fn body(envelope: Envelope) -> Result<Body, SessionError> {
    match envelope.body {
        Some(envelope::Body::Request(request)) => Ok(request
            .kind
            .map(Body::Request)
            .unwrap_or(Body::UnknownRequest)),
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
        request::Kind::AttachGpuShares(_) => "a GPU share manifest out of order",
        request::Kind::ApplyGpuRecipe(_) => "a GPU recipe request out of order",
        request::Kind::ProbeGpu(_) => "a GPU probe request out of order",
    }
}

/// Why the host connection ended without a clean hang-up.
#[derive(Debug)]
pub enum SessionError {
    Frame(FrameError),
    Refused { code: ErrorCode, message: String },
    InvalidHello(String),
    OutOfOrder(&'static str),
    Malformed(String),
    HeartbeatUnanswered,
    RequestIdsExhausted,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => write!(formatter, "{error}"),
            Self::Refused { code, message } => {
                write!(formatter, "the host refused ({code:?}): {message}")
            }
            Self::InvalidHello(why) => {
                write!(
                    formatter,
                    "the host opened a session this agent cannot serve: {why}"
                )
            }
            Self::OutOfOrder(what) => write!(formatter, "the host sent {what}"),
            Self::Malformed(what) => write!(formatter, "the host sent {what}"),
            Self::HeartbeatUnanswered => {
                formatter.write_str("the host did not answer the heartbeat")
            }
            Self::RequestIdsExhausted => formatter.write_str("the agent exhausted its request ids"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::Refused { .. }
            | Self::InvalidHello(_)
            | Self::OutOfOrder(_)
            | Self::Malformed(_)
            | Self::HeartbeatUnanswered
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
            ApplyGpuRecipeRequest, AttachGpuSharesRequest, AuthenticateRequest, Capability,
            ErrorCode, GpuMount, GpuMountState, GpuProbeCheck, GpuProbeCheckState, GpuProbeStep,
            GpuProbeVerdict, GpuRecipeStage, GpuRecipeStageState, GpuRecipeStep, GpuShare,
            GpuShareRole, HelloRequest, HelloResponse, ProbeGpuRequest, ProbeGpuResponse,
            ProtocolVersion, envelope, request, response,
        },
    };

    use super::run;

    /// An attach that mounts nothing, for the tests about message order.
    ///
    /// Every one of them would otherwise ask a machine with no Hyper-V under
    /// it to mount a Plan9 share.
    fn refuse_to_mount(_shares: &[GpuShare]) -> (Vec<GpuMount>, bool) {
        (Vec::new(), false)
    }

    /// A recipe that does nothing, for the tests about message order.
    ///
    /// Every one of them would otherwise ask a machine that is not an Ubuntu
    /// guest to build a kernel module.
    fn apply_nothing() -> Vec<GpuRecipeStage> {
        Vec::new()
    }

    /// A probe that looks at nothing, for the tests about message order.
    ///
    /// Every one of them would otherwise ask a machine that was given no GPU
    /// to install two packages and render with them.
    fn probe_nothing() -> ProbeGpuResponse {
        ProbeGpuResponse::default()
    }

    /// A host peer that returns the exact bytes and timeout a session expects.
    struct ScriptedStream {
        reads: VecDeque<ReadStep>,
        written: Vec<u8>,
    }

    enum ReadStep {
        Bytes(Vec<u8>),
        WouldBlock,
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

        fn partial_frame(
            envelope: vmlord_agent_protocol::v1::Envelope,
            byte_count: usize,
        ) -> ReadStep {
            let mut bytes = Vec::new();
            frame::encode(&envelope, &mut bytes).expect("a host frame that fits");
            ReadStep::Bytes(bytes[..byte_count].to_vec())
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
                Some(ReadStep::WouldBlock) => {
                    self.reads.pop_front();
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "idle host"))
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
    fn would_block_before_the_next_frame_sends_a_heartbeat() {
        // `FrameError::Idle` says no part of the next prefix was consumed, so
        // the heartbeat is not interleaved with a host frame.
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
            ReadStep::WouldBlock,
        ]);

        let mut opened = None;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut opened,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect("the host closes after the heartbeat");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Hello(HelloRequest {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![i32::from(Capability::Gpu)],
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
    }

    #[test]
    fn would_block_after_part_of_a_frame_does_not_send_a_heartbeat() {
        // A heartbeat at this point would be interleaved with a host frame
        // whose prefix has already been consumed, corrupting the session.
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
            ScriptedStream::partial_frame(
                vmlord_agent_protocol::v1::Envelope::request(
                    8,
                    request::Kind::Heartbeat(vmlord_agent_protocol::v1::HeartbeatRequest {}),
                ),
                1,
            ),
            ReadStep::WouldBlock,
        ]);

        let error = run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect_err("a timeout after a partial frame must end the session");
        assert!(matches!(
            error,
            super::SessionError::Frame(frame::FrameError::Io(error))
                if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert_eq!(stream.written_frames().len(), 2);
    }

    #[test]
    fn an_unknown_request_kind_is_refused_with_its_request_id() {
        // Turning an unrecognized request into a malformed-session error
        // leaves a conforming host waiting forever for the response it needs.
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
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope {
                request_id: 27,
                body: Some(envelope::Body::Request(
                    vmlord_agent_protocol::v1::Request::default(),
                )),
            }),
        ]);

        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect("the host closes after the refusal");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 27);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("the unknown request needs an error response");
        };
        let Some(response::Kind::Error(error)) = &response.kind else {
            panic!("the unknown request needs an error response");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }

    #[test]
    fn accepts_an_older_same_major_hello_response() {
        // Rejecting a host with an older compatible minor breaks the protocol
        // negotiation that deliberately lets new agents speak to old hosts.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let current = ProtocolVersion::current();
        let older = ProtocolVersion {
            major: current.major,
            minor: current.minor - 1,
        };
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(older),
                    capabilities: vec![],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
        ]);

        let mut opened = None;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut opened,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect("a compatible host hang-up");
        assert_eq!(opened.expect("a session that authenticated").version, older);

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].request_id, 1);
        let Some(envelope::Body::Response(authenticate)) = &frames[1].body else {
            panic!("the challenge needs an authentication response");
        };
        assert!(matches!(
            authenticate.kind,
            Some(response::Kind::Authenticate(_))
        ));
    }

    #[test]
    fn a_capability_the_agent_never_offered_ends_the_session() {
        // Serving a session that agreed on a capability this build has no arm
        // for leaves the host waiting for answers that never come.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let mut stream = ScriptedStream::new([ScriptedStream::frame(
            vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    // A number no build here can name, which is what a host
                    // one release ahead would answer with.
                    capabilities: vec![97],
                }),
            ),
        )]);

        let mut opened = None;
        let error = run(
            &mut stream,
            &secret,
            "test-agent",
            &mut opened,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect_err("a capability the agent did not announce must not be served");

        assert!(matches!(error, super::SessionError::InvalidHello(_)));
        assert_eq!(opened, None);
        // The hello and nothing else: no tag is handed to a host whose session
        // this agent has already refused.
        assert_eq!(stream.written_frames().len(), 1);
    }

    #[test]
    fn a_manifest_on_a_gpu_session_is_carried_out_and_reported_back() {
        // The host reads this answer to find out what the guest has mounted,
        // so it has to arrive as the response to the request that asked.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let manifest = vec![GpuShare {
            name: "vmlord.gpu.wsl-lib".to_owned(),
            role: i32::from(GpuShareRole::WslLib),
            package: String::new(),
        }];
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![i32::from(Capability::Gpu)],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                4,
                request::Kind::AttachGpuShares(AttachGpuSharesRequest {
                    shares: manifest.clone(),
                }),
            )),
        ]);

        let mut attached = Vec::new();
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            |shares: &[GpuShare]| {
                attached.extend_from_slice(shares);
                (
                    vec![GpuMount {
                        share: "vmlord.gpu.wsl-lib".to_owned(),
                        state: i32::from(GpuMountState::Mounted),
                        path: "/usr/lib/wsl/lib".to_owned(),
                        message: "mounted".to_owned(),
                    }],
                    true,
                )
            },
            apply_nothing,
            probe_nothing,
        )
        .expect("the host closes after its manifest was answered");

        assert_eq!(attached, manifest);
        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 4);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("the manifest needs a response");
        };
        let Some(response::Kind::AttachGpuShares(report)) = &response.kind else {
            panic!("the manifest needs a mount report");
        };
        assert!(report.libraries_refreshed);
        assert_eq!(report.mounts.len(), 1);
        assert_eq!(report.mounts[0].path, "/usr/lib/wsl/lib");
    }

    #[test]
    fn an_apply_on_a_gpu_session_is_carried_out_and_reported_back() {
        // The host reads this answer to find out what the guest's recipe did,
        // so it has to arrive as the response to the request that asked.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![i32::from(Capability::Gpu)],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                5,
                request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}),
            )),
        ]);

        let mut applied = 0;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            || {
                applied += 1;
                vec![GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Device),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "/dev/dxg is a usable device".to_owned(),
                }]
            },
            probe_nothing,
        )
        .expect("the host closes after its recipe was answered");

        assert_eq!(applied, 1);
        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 5);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("an apply needs a response");
        };
        let Some(response::Kind::ApplyGpuRecipe(report)) = &response.kind else {
            panic!("an apply needs a recipe report");
        };
        assert_eq!(report.stages.len(), 1);
        assert_eq!(report.stages[0].step(), GpuRecipeStep::Device);
    }

    #[test]
    fn a_probe_on_a_gpu_session_is_carried_out_and_reported_back() {
        // The host reads this answer to find out whether the guest renders, so
        // it has to arrive as the response to the request that asked.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![i32::from(Capability::Gpu)],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                6,
                request::Kind::ProbeGpu(ProbeGpuRequest {}),
            )),
        ]);

        let mut probed = 0;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            apply_nothing,
            || {
                probed += 1;
                ProbeGpuResponse {
                    verdict: i32::from(GpuProbeVerdict::Renders),
                    checks: vec![GpuProbeCheck {
                        step: i32::from(GpuProbeStep::Opengl),
                        state: i32::from(GpuProbeCheckState::Ok),
                        message: "GL renders on D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                    }],
                    renderer: "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                    driver: "dxgkrnl".to_owned(),
                    render_node: String::new(),
                }
            },
        )
        .expect("the host closes after its probe was answered");

        assert_eq!(probed, 1);
        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 6);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("a probe needs a response");
        };
        let Some(response::Kind::ProbeGpu(report)) = &response.kind else {
            panic!("a probe needs a probe report");
        };
        assert_eq!(report.verdict(), GpuProbeVerdict::Renders);
        assert_eq!(report.checks[0].step(), GpuProbeStep::Opengl);
    }

    #[test]
    fn a_probe_on_a_session_without_the_gpu_capability_is_refused() {
        // The capability is what says the two builds agreed this session may
        // carry a probe at all. Installing packages for a session that never
        // agreed on one would make the negotiation decorative.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                6,
                request::Kind::ProbeGpu(ProbeGpuRequest {}),
            )),
        ]);

        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            apply_nothing,
            || panic!("a probe that was never agreed on must not be run"),
        )
        .expect("the host closes after the refusal");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 6);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("the probe needs a response");
        };
        let Some(response::Kind::Error(error)) = &response.kind else {
            panic!("the probe needs an error response");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }

    #[test]
    fn an_apply_on_a_session_without_the_gpu_capability_is_refused() {
        // The capability is what says the two builds agreed this session may
        // carry a recipe at all. Building a kernel module for a session that
        // never agreed on one would make the negotiation decorative.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                5,
                request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}),
            )),
        ]);

        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            || panic!("a recipe that was never agreed on must not be applied"),
            probe_nothing,
        )
        .expect("the host closes after the refusal");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 5);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("the apply needs a response");
        };
        let Some(response::Kind::Error(error)) = &response.kind else {
            panic!("the apply needs an error response");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }

    #[test]
    fn a_manifest_on_a_session_without_the_gpu_capability_is_refused() {
        // The capability is what says the two builds agreed this session may
        // carry a manifest at all. Mounting one that was never agreed on would
        // make the negotiation decorative.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                4,
                request::Kind::AttachGpuShares(AttachGpuSharesRequest { shares: vec![] }),
            )),
        ]);

        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            |_shares: &[GpuShare]| {
                panic!("a manifest that was never agreed on must not be mounted")
            },
            apply_nothing,
            probe_nothing,
        )
        .expect("the host closes after the refusal");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 4);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("the manifest needs a response");
        };
        let Some(response::Kind::Error(error)) = &response.kind else {
            panic!("the manifest needs an error response");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }

    #[test]
    fn a_host_that_answers_with_a_newer_minor_ends_the_session() {
        // The host picks the revision from the two hellos, so a minor above
        // this build's is a host that answered with messages it never heard
        // this agent claim to speak.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let current = ProtocolVersion::current();
        let newer = ProtocolVersion {
            major: current.major,
            minor: current.minor + 1,
        };
        let mut stream = ScriptedStream::new([ScriptedStream::frame(
            vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(newer),
                    capabilities: vec![],
                }),
            ),
        )]);

        let mut opened = None;
        let error = run(
            &mut stream,
            &secret,
            "test-agent",
            &mut opened,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect_err("a revision this agent never claimed must not be served");

        assert!(matches!(error, super::SessionError::InvalidHello(_)));
        assert_eq!(opened, None);
    }

    #[test]
    fn a_connection_that_never_authenticates_reports_no_session() {
        // The reconnect loop resets its backoff on an authenticated session,
        // so a hang-up during the handshake must not look like one.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let mut stream = ScriptedStream::new([ScriptedStream::frame(
            vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![],
                }),
            ),
        )]);

        let mut opened = None;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut opened,
            refuse_to_mount,
            apply_nothing,
            probe_nothing,
        )
        .expect_err("a host that hangs up before its challenge ends the session");

        assert_eq!(opened, None);
    }
}
