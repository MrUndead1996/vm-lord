//! The host's half of a conversation with a guest agent.
//!
//! Split from the socket underneath it on purpose: a session is a sequence of
//! frames and the rules about their order, and none of that needs Hyper-V to
//! be true. What is here reads and writes any stream, which is how the order
//! can be tested against a peer made of bytes rather than against a VM.
//!
//! A session opens in two steps, because two things have to be agreed before
//! the host will act on anything. The guest says hello and the two settle on a
//! protocol revision and the capabilities they share; then the host challenges
//! the guest to prove it holds the VM's secret. Until that tag has been
//! verified, the only requests this side answers are the ones that get a
//! session to that point -- `auth::allowed_unauthenticated` is where that rule
//! is written, and it is deliberately not re-decided here.

use std::{
    error::Error,
    fmt,
    io::{Read, Write},
};

use vmlord_agent_protocol::{
    auth::{self, Nonce, Secret, Tag},
    frame::{self, FrameError},
    handshake::{self, CURRENT_VERSION, VersionMismatch},
    v1::{
        ApplyGpuRecipeRequest, ApplyGpuRecipeResponse, AttachGpuSharesRequest,
        AttachGpuSharesResponse, AuthenticateRequest, Capability, Envelope, ErrorCode,
        GpuMountState, GpuRecipeStageState, GpuShareRole, HeartbeatResponse, HelloResponse,
        ProtocolVersion, envelope, request, response,
    },
};
use vmlord_core::{GpuShareManifest, GpuShareRole as CoreShareRole};

/// What this build of the host implements beyond the base protocol.
///
/// `Capability::Gpu` is what lets a session carry a share manifest. It is
/// announced whether or not the VM on this connection has any shares: the
/// capability says what the two builds can do, and a VM with no GPU is simply
/// a session no manifest is sent on.
const HOST_CAPABILITIES: &[Capability] = &[Capability::Gpu];

/// The id the host numbers its challenge with.
///
/// Request ids are per originator, so the host's numbering is its own and
/// starts here.
const CHALLENGE_REQUEST_ID: u32 = 1;

/// The id the host sends a GPU share manifest with.
///
/// One manifest per session and one id for it: the host has nothing else to
/// ask a guest, and a counter would be a counter of one.
const ATTACH_REQUEST_ID: u32 = CHALLENGE_REQUEST_ID + 1;

/// The id the host asks for the guest's GPU recipe with.
///
/// One recipe per session, after the manifest of the same session: the module
/// is built out of the payload the guest has just been told to mount.
const APPLY_REQUEST_ID: u32 = ATTACH_REQUEST_ID + 1;

/// What a session agreed on when it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSession {
    /// The revision both peers speak, which is the lower of the two minors.
    pub(crate) version: ProtocolVersion,
    /// The capabilities both peers have, which is the only set either may use.
    pub(crate) capabilities: Vec<Capability>,
}

/// Runs the hello exchange and the challenge, in that order.
///
/// Returns once the guest has proved it holds `secret`. Everything the guest
/// says in the meantime that is not part of getting there is refused with an
/// error frame rather than silently dropped: an agent that is waiting for an
/// answer it will never get would sit there until its socket died.
///
/// # Errors
///
/// [`SessionError`] if the guest never gets there: a closed connection, a
/// protocol major this build cannot speak, a tag that does not verify, or a
/// message that has no place in an opening session. Every one of them leaves
/// the connection to be dropped -- there is no state from a half-opened
/// session worth keeping.
pub(crate) fn open<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    vm_name: &str,
) -> Result<AgentSession, SessionError> {
    let mut buffer = Vec::new();
    let session = greet(stream, vm_name, &mut buffer)?;
    authenticate(stream, secret, vm_name, &mut buffer)?;

    log::info!(
        "the agent of VM \"{vm_name}\" opened a session on protocol {}.{} with {} \
         agreed capability(ies)",
        session.version.major,
        session.version.minor,
        session.capabilities.len()
    );
    Ok(session)
}

/// Serves the requests of a session that is open, until the guest closes it.
///
/// Returns `Ok(())` when the agent hangs up at a frame boundary, which is how
/// a guest that is shutting down or restarting its agent ends a session and is
/// not a fault.
///
/// # Errors
///
/// [`SessionError`] if the connection failed or the guest sent something that
/// cannot be read as a frame. Both leave the stream at an unknown position, so
/// the connection is dropped rather than resynchronised.
pub(crate) fn serve<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    shares: Option<&GpuShareManifest>,
    vm_name: &str,
) -> Result<(), SessionError> {
    let mut buffer = Vec::new();
    log::debug!(
        "serving the agent of VM \"{vm_name}\" on protocol {}.{}",
        session.version.major,
        session.version.minor
    );

    let mut pending_manifest = attach_shares(stream, session, shares, vm_name, &mut buffer)?;
    let mut pending_recipe = None;

    loop {
        let envelope = match frame::read(stream, &mut buffer) {
            Ok(envelope) => envelope,
            Err(FrameError::Closed) => {
                log::info!("the agent of VM \"{vm_name}\" closed its session");
                return Ok(());
            }
            // `Idle` is reported only before a peer starts another frame, so
            // retrying cannot abandon a partial prefix or body. This is the
            // normal result of the bounded socket reads that let VM shutdown
            // interrupt a silent agent session.
            Err(FrameError::Idle) => continue,
            Err(error) => return Err(SessionError::Frame(error)),
        };

        let request_id = envelope.request_id;
        match body(envelope, vm_name)? {
            Body::Request(kind) => {
                let answer = answer(request_id, &kind, vm_name);
                frame::write(stream, &answer, &mut buffer).map_err(SessionError::Frame)?;
            }
            Body::Response(response::Kind::AttachGpuShares(report))
                if pending_manifest == Some(request_id) =>
            {
                pending_manifest = None;
                report_mounts(&report, vm_name);
                pending_recipe = apply_recipe(stream, vm_name, &mut buffer)?;
            }
            Body::Response(response::Kind::ApplyGpuRecipe(report))
                if pending_recipe == Some(request_id) =>
            {
                pending_recipe = None;
                report_recipe(&report, vm_name);
            }
            // A response to a request this side did not send, or one it has
            // already had an answer to. Worth a line and nothing more: there is
            // no id left to fail, and the session is otherwise intact.
            Body::Response(_) => log::warn!(
                "the agent of VM \"{vm_name}\" answered request {request_id}, which VMLord \
                 never sent"
            ),
        }
    }
}

/// Hands the guest the shares its VM was given, and says which id asked.
///
/// Once per session rather than once per VM: the host cannot tell an agent
/// that lost its socket from one whose VM rebooted, and the guest reconciles
/// against what it already has, so re-sending costs a message and saves this
/// side from having to know which happened. Nothing here touches HCS -- the
/// shares were written into the compute system's configuration before it was
/// started, and this is only the guest being told what they are for.
///
/// `None` comes back when there was nothing to send or nobody to send it to,
/// which is a session that simply never waits for a report.
fn attach_shares<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    shares: Option<&GpuShareManifest>,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let Some(manifest) = shares else {
        return Ok(None);
    };
    if !session.capabilities.contains(&Capability::Gpu) {
        // An agent too old to have the capability cannot mount anything, and
        // sending it a manifest would be a request it would refuse.
        log::warn!(
            "the agent of VM \"{vm_name}\" does not speak the GPU capability, so its {} \
             share(s) are exported but not mounted",
            manifest.shares.len()
        );
        return Ok(None);
    }

    let request = Envelope::request(
        ATTACH_REQUEST_ID,
        request::Kind::AttachGpuShares(AttachGpuSharesRequest {
            shares: manifest.shares.iter().map(wire_share).collect(),
        }),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    log::debug!(
        "VMLord offered the agent of VM \"{vm_name}\" {} GPU share(s)",
        manifest.shares.len()
    );

    Ok(Some(ATTACH_REQUEST_ID))
}

/// Asks the guest to apply its GPU recipe, and says which id asked.
///
/// After the mounts of the same session, because the module is built out of
/// the payload the guest has just mounted. Once per session, for the same
/// reason the manifest is sent once: the guest reconciles rather than
/// rebuilds, and a retry loop around a kernel build is how a guest ends up
/// compiling continuously.
fn apply_recipe<S: Read + Write>(
    stream: &mut S,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let request = Envelope::request(
        APPLY_REQUEST_ID,
        request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    log::debug!("VMLord asked the agent of VM \"{vm_name}\" to apply its GPU recipe");

    Ok(Some(APPLY_REQUEST_ID))
}

/// One share in the form the wire carries it.
///
/// The roles are the same two facts on both sides, so the mapping is total
/// rather than fallible: a role `vmlord_core` has and this does not would fail
/// to compile here, which is where it should fail.
fn wire_share(share: &vmlord_core::GpuShare) -> vmlord_agent_protocol::v1::GpuShare {
    let (role, package) = match &share.role {
        CoreShareRole::WslLib => (GpuShareRole::WslLib, String::new()),
        CoreShareRole::GpuPayload => (GpuShareRole::GpuPayload, String::new()),
        CoreShareRole::DriverPackage { package } => (GpuShareRole::DriverPackage, package.clone()),
    };

    vmlord_agent_protocol::v1::GpuShare {
        name: share.name.clone(),
        role: i32::from(role),
        package,
    }
}

/// Says what the guest made of the manifest, at the volume each answer earns.
///
/// A share the guest refused is the two builds disagreeing about what a share
/// is, and a share it could not mount is one that is exported and broken.
/// Neither ends the session: GPU is best effort, and a VM with half a GPU
/// userspace is still a running VM.
fn report_mounts(report: &AttachGpuSharesResponse, vm_name: &str) {
    for mount in &report.mounts {
        match mount.state() {
            GpuMountState::Mounted => log::debug!(
                "the agent of VM \"{vm_name}\" mounted {} at {}",
                mount.share,
                mount.path
            ),
            state => log::warn!(
                "the agent of VM \"{vm_name}\" did not mount {} ({state:?}): {}",
                mount.share,
                mount.message
            ),
        }
    }

    if !report.libraries_refreshed && !report.mounts.is_empty() {
        log::warn!(
            "the agent of VM \"{vm_name}\" could not tell the dynamic linker about its GPU \
             libraries"
        );
    }
}

/// Says what the guest's recipe did, at the volume each stage earns.
///
/// Nothing is kept and nothing is retried: the next session applies the recipe
/// again, and deriving a GPU status from these facts is the application
/// layer's work.
fn report_recipe(report: &ApplyGpuRecipeResponse, vm_name: &str) {
    for stage in &report.stages {
        match stage.state() {
            GpuRecipeStageState::Ok => log::debug!(
                "the agent of VM \"{vm_name}\" finished GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            GpuRecipeStageState::Skipped => log::debug!(
                "the agent of VM \"{vm_name}\" skipped GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            state => log::warn!(
                "the agent of VM \"{vm_name}\" did not finish GPU recipe stage {:?} ({state:?}): \
                 {}",
                stage.step(),
                stage.message
            ),
        }
    }
}

/// Settles the protocol revision and the capabilities of a new session.
fn greet<S: Read + Write>(
    stream: &mut S,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<AgentSession, SessionError> {
    let envelope = frame::read(stream, buffer).map_err(SessionError::Frame)?;
    let request_id = envelope.request_id;
    let Body::Request(request::Kind::Hello(hello)) = body(envelope, vm_name)? else {
        // The first frame of a session is the hello and nothing else: until
        // there is an agreed revision, this side does not know what any other
        // message means.
        let refusal = Envelope::error(
            request_id,
            ErrorCode::Unauthenticated,
            "this session has not said hello yet",
        );
        frame::write(stream, &refusal, buffer).map_err(SessionError::Frame)?;
        return Err(SessionError::OutOfOrder(
            "the agent sent something other than a hello to open its session",
        ));
    };

    let remote = hello.version.unwrap_or_default();
    let version = match handshake::negotiate_version(CURRENT_VERSION, remote) {
        Ok(version) => version,
        Err(mismatch) => {
            let refusal = Envelope::error(
                request_id,
                ErrorCode::UnsupportedVersion,
                mismatch.to_string(),
            );
            frame::write(stream, &refusal, buffer).map_err(SessionError::Frame)?;
            return Err(SessionError::Version(mismatch));
        }
    };
    let capabilities = handshake::agreed_capabilities(HOST_CAPABILITIES, &hello.capabilities);

    log::debug!(
        "the agent of VM \"{vm_name}\" is build \"{}\" and speaks protocol {}.{}",
        hello.agent_version,
        remote.major,
        remote.minor
    );
    let accepted = Envelope::response(
        request_id,
        response::Kind::Hello(HelloResponse {
            version: Some(version),
            capabilities: capabilities.iter().copied().map(i32::from).collect(),
        }),
    );
    frame::write(stream, &accepted, buffer).map_err(SessionError::Frame)?;

    Ok(AgentSession {
        version,
        capabilities,
    })
}

/// Challenges the guest and waits for a tag that verifies.
fn authenticate<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<(), SessionError> {
    let nonce = Nonce::generate();
    let challenge = Envelope::request(
        CHALLENGE_REQUEST_ID,
        request::Kind::Authenticate(AuthenticateRequest {
            nonce: nonce.as_bytes().to_vec(),
        }),
    );
    frame::write(stream, &challenge, buffer).map_err(SessionError::Frame)?;

    loop {
        let envelope = frame::read(stream, buffer).map_err(SessionError::Frame)?;
        let request_id = envelope.request_id;
        let kind = match body(envelope, vm_name)? {
            Body::Request(kind) => kind,
            Body::Response(_) if request_id != CHALLENGE_REQUEST_ID => {
                return Err(SessionError::OutOfOrder(
                    "the agent answered a request VMLord never sent",
                ));
            }
            Body::Response(response::Kind::Authenticate(answer)) => {
                let answer = Tag::from_wire(&answer.tag)
                    .map_err(|error| SessionError::Malformed(error.to_string()))?;
                if !auth::verify(secret, &nonce, &answer) {
                    return Err(SessionError::Unauthenticated);
                }
                return Ok(());
            }
            Body::Response(response::Kind::Error(error)) => {
                return Err(SessionError::Refused {
                    code: error.code(),
                    message: error.message,
                });
            }
            Body::Response(_) => {
                return Err(SessionError::OutOfOrder(
                    "the agent answered the challenge with something else",
                ));
            }
        };

        // Everything a guest may ask before it has authenticated has already
        // been asked, so what is left is either refused as out of order or
        // refused as unauthenticated. The rule about which is which belongs to
        // the protocol, not to this transport.
        let refusal = if auth::allowed_unauthenticated(&kind) {
            Envelope::error(
                request_id,
                ErrorCode::InvalidArgument,
                "this session is already open and waiting for its challenge to be answered",
            )
        } else {
            Envelope::error(
                request_id,
                ErrorCode::Unauthenticated,
                "this session has not answered its challenge yet",
            )
        };
        log::warn!(
            "the agent of VM \"{vm_name}\" sent request {request_id} before answering its \
             challenge; it was refused"
        );
        frame::write(stream, &refusal, buffer).map_err(SessionError::Frame)?;
    }
}

/// The answer to a request numbered `request_id`, as this build serves it.
fn answer(request_id: u32, kind: &request::Kind, vm_name: &str) -> Envelope {
    match kind {
        request::Kind::Heartbeat(_) => {
            log::trace!("the agent of VM \"{vm_name}\" is alive");
            Envelope::response(request_id, response::Kind::Heartbeat(HeartbeatResponse {}))
        }
        // A second hello would renegotiate a session that is already running,
        // and the guest has no way to know what this side would then still
        // believe about it. Reconnecting is how an agent starts over.
        request::Kind::Hello(_) => Envelope::error(
            request_id,
            ErrorCode::InvalidArgument,
            "this session is already open; reconnect to open another",
        ),
        // The protocol is symmetric, so an agent may challenge its host. This
        // build does not answer one: nothing in the guest acts on the reply
        // yet, and a tag sent to a peer that never asked for it is a tag given
        // away for nothing.
        request::Kind::Authenticate(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "this build of VMLord does not answer challenges from a guest",
        ),
        // The manifest travels the other way. A guest that sends one is asking
        // the host to mount host directories, which is not what the message
        // means and not something this side would do.
        request::Kind::AttachGpuShares(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a GPU share manifest is the host's to send",
        ),
        // Likewise: the recipe is the guest's to apply and the host's to ask
        // for, and there is no GPU recipe for a Windows host to run.
        request::Kind::ApplyGpuRecipe(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a GPU recipe is the host's to ask for",
        ),
    }
}

/// The two shapes a frame can carry, unwrapped to the arm inside.
enum Body {
    Request(request::Kind),
    Response(response::Kind),
}

/// Reads what an envelope carries, refusing one that carries nothing.
///
/// An envelope with no body -- or a request or response with no kind -- is what
/// a peer from a future minor sends when it uses an arm this build has never
/// heard of, and what a corrupt encoder sends. Neither can be answered, because
/// there is nothing to answer: `request_id` alone does not say what failed.
fn body(envelope: Envelope, vm_name: &str) -> Result<Body, SessionError> {
    let request_id = envelope.request_id;
    match envelope.body {
        Some(envelope::Body::Request(request)) => {
            request.kind.map(Body::Request).ok_or_else(|| {
                log::warn!(
                    "the agent of VM \"{vm_name}\" sent request {request_id} with no kind this \
                 build knows"
                );
                SessionError::Malformed("a request with no kind this build knows".to_owned())
            })
        }
        Some(envelope::Body::Response(response)) => {
            response.kind.map(Body::Response).ok_or_else(|| {
                log::warn!(
                    "the agent of VM \"{vm_name}\" answered request {request_id} with no kind \
                     this build knows"
                );
                SessionError::Malformed("a response with no kind this build knows".to_owned())
            })
        }
        None => Err(SessionError::Malformed(
            "an envelope with no body at all".to_owned(),
        )),
    }
}

/// Why a session ended before it should have.
#[derive(Debug)]
pub(crate) enum SessionError {
    /// The connection failed, or carried something that is not a frame.
    Frame(FrameError),
    /// The peers have nothing to talk about.
    Version(VersionMismatch),
    /// The tag did not verify: whatever is on the other end does not hold this
    /// VM's secret.
    Unauthenticated,
    /// The guest refused something the host asked of it.
    Refused { code: ErrorCode, message: String },
    /// A frame that is well-formed and has no place where it arrived.
    OutOfOrder(&'static str),
    /// A frame this build cannot make sense of.
    Malformed(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => write!(formatter, "{error}"),
            Self::Version(mismatch) => write!(formatter, "{mismatch}"),
            Self::Unauthenticated => formatter.write_str(
                "the peer did not prove it holds this VM's agent secret, so it is not this \
                 VM's agent",
            ),
            Self::Refused { code, message } => {
                write!(formatter, "the agent refused ({code:?}): {message}")
            }
            Self::OutOfOrder(what) => formatter.write_str(what),
            Self::Malformed(what) => write!(formatter, "the agent sent {what}"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::Version(mismatch) => Some(mismatch),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};

    use vmlord_agent_protocol::{
        auth::{Nonce, Secret, tag},
        frame::{self, LENGTH_PREFIX_LEN},
        v1::{
            ApplyGpuRecipeResponse, AttachGpuSharesResponse, AuthenticateResponse, Capability,
            Envelope, ErrorCode, GpuMount, GpuMountState, GpuRecipeStage, GpuRecipeStageState,
            GpuRecipeStep, GpuShareRole, HeartbeatRequest, HelloRequest, ProtocolVersion, envelope,
            request, response,
        },
    };
    use vmlord_core::GpuShareManifest;

    use super::{AgentSession, SessionError, open, serve};

    const VM: &str = "dev-linux";

    /// An agent made of bytes, which answers rather than replays.
    ///
    /// A recorded conversation cannot stand in for one: the host draws a fresh
    /// nonce for every session, so the only peer that can open one is a peer
    /// that reads the challenge it was actually sent. This one does, in the
    /// same place a real agent would -- when the frame arrives.
    struct Guest {
        /// What the guest answers challenges with, which is not always the
        /// secret the host is verifying against.
        secret: Secret,
        /// Frames the host has not read yet.
        outbox: Vec<u8>,
        read: usize,
        /// Everything the host has written, and how much of it has been read
        /// back out as frames.
        inbox: Vec<u8>,
        parsed: usize,
        /// What the host was sent, for the assertions.
        received: Vec<Envelope>,
        /// Sent just before the answer to the challenge.
        before_answer: Vec<Envelope>,
        /// Sent once the challenge has been answered.
        after_answer: Vec<Envelope>,
    }

    impl Guest {
        /// A guest that opens with a hello and answers with `secret`.
        fn new(secret: Secret) -> Self {
            Self::opening_with(secret, hello(ProtocolVersion::current(), &[]))
        }

        fn opening_with(secret: Secret, first: Envelope) -> Self {
            let mut guest = Self {
                secret,
                outbox: Vec::new(),
                read: 0,
                inbox: Vec::new(),
                parsed: 0,
                received: Vec::new(),
                before_answer: Vec::new(),
                after_answer: Vec::new(),
            };
            guest.say(&first);
            guest
        }

        fn before_answer(mut self, envelopes: &[Envelope]) -> Self {
            self.before_answer = envelopes.to_vec();
            self
        }

        fn after_answer(mut self, envelopes: &[Envelope]) -> Self {
            self.after_answer = envelopes.to_vec();
            self
        }

        fn say(&mut self, envelope: &Envelope) {
            let mut frame = Vec::new();
            frame::encode(envelope, &mut frame).expect("a frame that fits");
            self.outbox.extend_from_slice(&frame);
        }

        /// Reads whatever complete frames the host has written, answering a
        /// challenge the moment one arrives.
        fn take(&mut self) {
            while let Some(envelope) = self.next_frame() {
                if let Some(envelope::Body::Request(ref request)) = envelope.body
                    && let Some(request::Kind::Authenticate(ref challenge)) = request.kind
                {
                    let nonce =
                        Nonce::from_wire(&challenge.nonce).expect("a nonce of the right length");
                    let answer = Envelope::response(
                        envelope.request_id,
                        response::Kind::Authenticate(AuthenticateResponse {
                            tag: tag(&self.secret, &nonce).as_bytes().to_vec(),
                        }),
                    );
                    for envelope in std::mem::take(&mut self.before_answer) {
                        self.say(&envelope);
                    }
                    self.say(&answer);
                    for envelope in std::mem::take(&mut self.after_answer) {
                        self.say(&envelope);
                    }
                }
                self.received.push(envelope);
            }
        }

        fn next_frame(&mut self) -> Option<Envelope> {
            let rest = &self.inbox[self.parsed..];
            if rest.len() < LENGTH_PREFIX_LEN {
                return None;
            }
            let prefix: [u8; LENGTH_PREFIX_LEN] = rest[..LENGTH_PREFIX_LEN]
                .try_into()
                .expect("four bytes of prefix");
            let body_len = frame::body_len(prefix).expect("a body within the limit");
            let frame_len = LENGTH_PREFIX_LEN + body_len;
            if rest.len() < frame_len {
                return None;
            }

            let envelope = frame::decode(&rest[LENGTH_PREFIX_LEN..frame_len]).expect("an envelope");
            self.parsed += frame_len;
            Some(envelope)
        }

        /// The answer the host gave to the request numbered `request_id`.
        fn answer_to(&self, request_id: u32) -> &Envelope {
            self.received
                .iter()
                .find(|envelope| envelope.request_id == request_id)
                .expect("the host should have answered")
        }
    }

    impl Read for Guest {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = &self.outbox[self.read..];
            let taken = available.len().min(buffer.len());
            buffer[..taken].copy_from_slice(&available[..taken]);
            self.read += taken;
            // Nothing left to say is a guest that hung up, which is how every
            // session in these tests ends.
            Ok(taken)
        }
    }

    impl Write for Guest {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.inbox.extend_from_slice(buffer);
            self.take();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct IdleThenClosed {
        idle: bool,
    }

    impl Read for IdleThenClosed {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            if self.idle {
                self.idle = false;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "idle guest"));
            }
            Ok(0)
        }
    }

    impl Write for IdleThenClosed {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn hello(version: ProtocolVersion, capabilities: &[Capability]) -> Envelope {
        Envelope::request(
            7,
            request::Kind::Hello(HelloRequest {
                version: Some(version),
                capabilities: capabilities.iter().copied().map(i32::from).collect(),
                agent_version: "0.1.0".to_owned(),
            }),
        )
    }

    fn heartbeat(request_id: u32) -> Envelope {
        Envelope::request(request_id, request::Kind::Heartbeat(HeartbeatRequest {}))
    }

    /// The error code the host answered `request_id` with.
    fn refusal(guest: &Guest, request_id: u32) -> ErrorCode {
        let Some(envelope::Body::Response(ref response)) = guest.answer_to(request_id).body else {
            panic!("expected a response");
        };
        match response.kind {
            Some(response::Kind::Error(ref error)) => error.code(),
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn a_session_opens_on_a_hello_and_a_verified_tag() {
        let secret = Secret::generate();
        let mut guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"));

        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        assert_eq!(session.version, ProtocolVersion::current());
        assert!(session.capabilities.is_empty());
        let Some(envelope::Body::Response(ref response)) = guest.answer_to(7).body else {
            panic!("the hello should have been answered");
        };
        assert!(matches!(response.kind, Some(response::Kind::Hello(_))));
    }

    #[test]
    fn a_session_speaks_the_older_peers_minor() {
        let current = ProtocolVersion::current();
        let older = ProtocolVersion {
            major: current.major,
            minor: 0,
        };
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(older, &[]),
        );

        let session = open(&mut guest, &secret, VM).expect("a session with an older agent");

        assert_eq!(session.version, older);
    }

    #[test]
    fn a_differing_major_is_refused_with_the_reason() {
        let current = ProtocolVersion::current();
        let future = ProtocolVersion {
            major: current.major + 1,
            minor: 0,
        };
        let mut guest = Guest::opening_with(Secret::generate(), hello(future, &[]));

        let error = open(&mut guest, &Secret::generate(), VM).expect_err("an unspeakable major");

        assert!(matches!(error, SessionError::Version(_)), "{error}");
        assert_eq!(refusal(&guest, 7), ErrorCode::UnsupportedVersion);
    }

    #[test]
    fn a_capability_the_agent_does_not_have_is_not_agreed() {
        // The host announces the GPU capability on every session; an agent
        // installed before it existed announces nothing, and the intersection
        // of the two is what the session may carry.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[]),
        );

        let session = open(&mut guest, &secret, VM).expect("a session with an older agent");

        assert!(session.capabilities.is_empty());
    }

    #[test]
    fn an_agent_that_can_mount_shares_agrees_on_the_gpu_capability() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );

        let session = open(&mut guest, &secret, VM).expect("a session with a GPU-capable agent");

        assert_eq!(session.capabilities, vec![Capability::Gpu]);
    }

    #[test]
    fn a_tag_from_another_secret_does_not_open_a_session() {
        // What something that reached the socket without the secret would send.
        let mut guest = Guest::new(Secret::generate());

        let error = open(&mut guest, &Secret::generate(), VM).expect_err("a forged tag");

        assert!(matches!(error, SessionError::Unauthenticated), "{error}");
    }

    #[test]
    fn anything_but_a_hello_first_is_refused_as_unauthenticated() {
        let mut guest = Guest::opening_with(Secret::generate(), heartbeat(3));

        let error = open(&mut guest, &Secret::generate(), VM).expect_err("a session with no hello");

        assert!(matches!(error, SessionError::OutOfOrder(_)), "{error}");
        assert_eq!(refusal(&guest, 3), ErrorCode::Unauthenticated);
    }

    #[test]
    fn a_request_sent_before_the_challenge_is_answered_is_refused() {
        // The heartbeat arrives while the host is waiting for the tag, so it is
        // refused -- and the session still opens on the tag behind it.
        let secret = Secret::generate();
        let mut guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"))
            .before_answer(&[heartbeat(4)]);

        open(&mut guest, &secret, VM).expect("a session that authenticated after the refusal");

        assert_eq!(refusal(&guest, 4), ErrorCode::Unauthenticated);
    }

    #[test]
    fn an_open_session_answers_heartbeats() {
        let secret = Secret::generate();
        let mut guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"))
            .after_answer(&[heartbeat(11)]);
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, None, VM).expect("a session the agent closed");

        let Some(envelope::Body::Response(ref response)) = guest.answer_to(11).body else {
            panic!("the heartbeat should have been answered");
        };
        assert!(matches!(response.kind, Some(response::Kind::Heartbeat(_))));
    }

    #[test]
    fn a_second_hello_is_refused_rather_than_renegotiated() {
        let secret = Secret::generate();
        let mut guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"))
            .after_answer(&[hello(ProtocolVersion::current(), &[])]);
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, None, VM).expect("a session the agent closed");

        // The hello and its refusal share a request id, so the last answer to
        // it is the one `serve` gave.
        let refused = guest
            .received
            .iter()
            .rfind(|envelope| envelope.request_id == 7)
            .expect("the second hello should have been answered");
        let Some(envelope::Body::Response(ref response)) = refused.body else {
            panic!("expected a response");
        };
        assert!(matches!(
            response.kind,
            Some(response::Kind::Error(ref error))
                if error.code() == ErrorCode::InvalidArgument
        ));
    }

    #[test]
    fn an_agent_that_hangs_up_ends_its_session_without_a_fault() {
        let secret = Secret::generate();
        let mut guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"));
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, None, VM).expect("a clean close is not a failure");
    }

    #[test]
    fn a_session_hands_a_gpu_capable_agent_its_manifest() {
        // The shares are already in the compute system's configuration; this
        // message is the only way the guest learns what they are for.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![
                vmlord_core::GpuShare::wsl_lib(),
                vmlord_core::GpuShare::driver_package("nv_dispi.inf_amd64_1234")
                    .expect("a package name the host accepts"),
                vmlord_core::GpuShare::payload(),
            ],
        };
        // The answer a guest that mounted them all would send.
        guest.say(&Envelope::response(
            super::ATTACH_REQUEST_ID,
            response::Kind::AttachGpuShares(AttachGpuSharesResponse {
                mounts: vec![GpuMount {
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                    state: i32::from(GpuMountState::Mounted),
                    path: "/usr/lib/wsl/lib".to_owned(),
                    message: "mounted".to_owned(),
                }],
                libraries_refreshed: true,
            }),
        ));

        serve(&mut guest, &session, Some(&manifest), VM).expect("a session the agent closed");

        let offered = guest.answer_to(super::ATTACH_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = offered.body else {
            panic!("the manifest should have been sent as a request");
        };
        let Some(request::Kind::AttachGpuShares(ref attach)) = request.kind else {
            panic!("the manifest should have been an attach request");
        };
        assert_eq!(
            attach.shares,
            vec![
                vmlord_agent_protocol::v1::GpuShare {
                    name: "vmlord.gpu.wsl-lib".to_owned(),
                    role: i32::from(GpuShareRole::WslLib),
                    package: String::new(),
                },
                vmlord_agent_protocol::v1::GpuShare {
                    name: "vmlord.gpu.drv.nv_dispi.inf_amd64_1234".to_owned(),
                    role: i32::from(GpuShareRole::DriverPackage),
                    package: "nv_dispi.inf_amd64_1234".to_owned(),
                },
                vmlord_agent_protocol::v1::GpuShare {
                    name: "vmlord.gpu.payload".to_owned(),
                    role: i32::from(GpuShareRole::GpuPayload),
                    package: String::new(),
                },
            ]
        );
    }

    #[test]
    fn a_session_asks_for_the_recipe_once_the_shares_are_attached() {
        // The recipe follows the mounts and never precedes them: a module
        // built out of a payload that is not mounted yet would fail for a
        // reason that has nothing to do with the guest.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![vmlord_core::GpuShare::payload()],
        };
        // What a guest that mounted its payload and applied its recipe sends.
        guest.say(&Envelope::response(
            super::ATTACH_REQUEST_ID,
            response::Kind::AttachGpuShares(AttachGpuSharesResponse {
                mounts: vec![GpuMount {
                    share: "vmlord.gpu.payload".to_owned(),
                    state: i32::from(GpuMountState::Mounted),
                    path: "/opt/vmlord/gpu-payload".to_owned(),
                    message: "mounted".to_owned(),
                }],
                libraries_refreshed: true,
            }),
        ));
        guest.say(&Envelope::response(
            super::APPLY_REQUEST_ID,
            response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse {
                stages: vec![GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Device),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "/dev/dxg is a usable device".to_owned(),
                }],
            }),
        ));

        serve(&mut guest, &session, Some(&manifest), VM).expect("a session the agent closed");

        let asked = guest.answer_to(super::APPLY_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the recipe should have been asked for as a request");
        };
        assert!(matches!(
            request.kind,
            Some(request::Kind::ApplyGpuRecipe(_))
        ));
        assert_eq!(
            guest
                .received
                .iter()
                .filter(|envelope| matches!(
                    envelope.body,
                    Some(envelope::Body::Request(ref request))
                        if matches!(request.kind, Some(request::Kind::ApplyGpuRecipe(_)))
                ))
                .count(),
            1,
            "one recipe per session"
        );
    }

    #[test]
    fn a_session_with_no_shares_asks_for_no_recipe() {
        // A guest with no GPU shares has no payload to build a module from.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, None, VM).expect("a session the agent closed");

        assert!(
            !guest.received.iter().any(|envelope| matches!(
                envelope.body,
                Some(envelope::Body::Request(ref request))
                    if matches!(request.kind, Some(request::Kind::ApplyGpuRecipe(_)))
            )),
            "a VM with no manifest is asked for no recipe"
        );
    }

    #[test]
    fn an_agent_without_the_gpu_capability_is_sent_no_manifest() {
        // It has no arm for the message, so sending one would earn a refusal
        // and tell the host nothing it did not already know from the hello.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![vmlord_core::GpuShare::wsl_lib()],
        };

        serve(&mut guest, &session, Some(&manifest), VM).expect("a session the agent closed");

        assert!(
            !guest.received.iter().any(|envelope| matches!(
                envelope.body,
                Some(envelope::Body::Request(ref request))
                    if matches!(request.kind, Some(request::Kind::AttachGpuShares(_)))
            )),
            "an agent that cannot mount shares must not be sent a manifest"
        );
    }

    #[test]
    fn an_idle_stream_keeps_the_session_open_until_the_agent_hangs_up() {
        let mut stream = IdleThenClosed { idle: true };
        let session = AgentSession {
            version: ProtocolVersion::current(),
            capabilities: Vec::new(),
        };

        serve(&mut stream, &session, None, VM).expect("an idle boundary is not a failed session");
    }
}
