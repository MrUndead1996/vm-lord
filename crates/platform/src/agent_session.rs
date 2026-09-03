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
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

use vmlord_agent_protocol::{
    auth::{self, Nonce, Secret, Tag},
    frame::{self, FrameError},
    handshake::{self, CURRENT_VERSION, VersionMismatch},
    v1::{
        ApplyDisplayRecipeRequest, ApplyDisplayRecipeResponse, ApplyGpuRecipeRequest,
        ApplyGpuRecipeResponse, AttachDisplayPayloadRequest, AttachDisplayPayloadResponse,
        AttachGpuSharesRequest, AttachGpuSharesResponse, AuthenticateRequest, Capability,
        DisplayMountState, DisplayRecipeStage, DisplayRecipeStageState, DisplayRecipeStep,
        DisplayShare as WireDisplayShare, DisplayUpdateOutcome, Envelope, ErrorCode, GpuMountState,
        GpuProbeCheckState, GpuProbeVerdict, GpuRecipeStageState, GpuShareRole, HeartbeatRequest,
        HeartbeatResponse, HelloResponse, ProbeGpuRequest, ProbeGpuResponse, ProtocolVersion,
        UpdateDisplayPayloadRequest, UpdateDisplayPayloadResponse, envelope, request, response,
    },
};
use vmlord_core::{
    DisplayFailure, DisplayMode, DisplayShare, DisplayStage, DisplayStatusCode, GpuFailure,
    GpuShareManifest, GpuShareRole as CoreShareRole, GpuStatusCode, GuestDesktop,
    GuestDisplayDetail, GuestDisplayReport, GuestGpuDetail, GuestGpuReport,
};

use crate::agent::{DisplayUpdate, DisplayUpdateAnswer};

/// What this build of the host implements beyond the base protocol.
///
/// `Capability::Gpu` is what lets a session carry a share manifest, and
/// `Capability::Display` what lets it carry a display payload. Both are
/// announced whether or not the VM on this connection has either: a capability
/// says what the two builds can do, and a VM with no GPU or no desktop is
/// simply a session that is sent nothing about one.
const HOST_CAPABILITIES: &[Capability] = &[Capability::Gpu, Capability::Display];

/// A booting guest may be scheduled late, but opening cannot hold the only
/// listener forever.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// A candidate is checked while an authenticated session is still active, so
/// it gets enough time for scheduling jitter without monopolising that session.
const REPLACEMENT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The id the host asks a guest to probe its GPU with.
///
/// One probe per session, after the recipe of the same session: the probe asks
/// about a userspace the recipe has just installed.
const PROBE_REQUEST_ID: u32 = APPLY_REQUEST_ID + 1;

/// The display's two requests, numbered after the GPU's.
///
/// Fixed ids like the GPU's, and for the same reason: this side asks each
/// question once per session, so an id is a question rather than a counter.
const DISPLAY_ATTACH_REQUEST_ID: u32 = PROBE_REQUEST_ID + 1;
const DISPLAY_APPLY_REQUEST_ID: u32 = DISPLAY_ATTACH_REQUEST_ID + 1;

/// The id an update is asked under.
///
/// One id and not a counter, like the questions above it, because a session
/// carries at most one update at a time: a second while the first is building
/// would be a second question on a socket still waiting for an answer.
const DISPLAY_UPDATE_REQUEST_ID: u32 = DISPLAY_APPLY_REQUEST_ID + 1;

/// The id of the host's proof that an otherwise silent guest is still there.
const LIVENESS_REQUEST_ID: u32 = DISPLAY_UPDATE_REQUEST_ID + 1;

#[derive(Clone, Copy)]
struct SessionTiming {
    idle_before_probe: Duration,
    probe_timeout: Duration,
}

impl SessionTiming {
    const NORMAL: Self = Self {
        // The guest sends a heartbeat after 30 seconds without a frame. Give
        // it another poll window before asking independently.
        idle_before_probe: Duration::from_secs(31),
        probe_timeout: Duration::from_secs(5),
    };

    #[cfg(test)]
    const IMMEDIATE: Self = Self {
        idle_before_probe: Duration::ZERO,
        probe_timeout: Duration::ZERO,
    };
}

/// Where a session hands what the guest said about its GPU.
///
/// A callback rather than a channel: `serve` is tested against a peer made of
/// bytes, and a sink that collects into a vector is the whole test harness.
/// One report per session is the usual number -- the guest is asked once --
/// but nothing here limits it, because a session that ends up saying two
/// things has said two things.
pub(crate) type GuestGpuSink<'a> = &'a dyn Fn(GuestGpuReport);

/// What a guest said about the display payload it has.
///
/// Versions as the guest reported them, and the failure -- if any -- that its
/// recipe stopped at. Empty strings on the wire become `None` here, which is
/// what "the guest has no such version" is in the host's own model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuestDisplayPayloadReport {
    pub(crate) installed: Option<String>,
    pub(crate) previous: Option<String>,
    pub(crate) loaded: Option<String>,
    pub(crate) failure: Option<DisplayFailure>,
    /// What the guest's own display services are doing, when this report says.
    ///
    /// `None` is a report with nothing to say about them -- a share that did
    /// not mount, or an update, which changes versions and not readiness --
    /// and leaves whatever was last observed standing.
    pub(crate) guest: Option<GuestDisplayReport>,
    /// The DER of the certificate the guest signs its modules with, when it
    /// has one. The private half is never asked for and never sent.
    pub(crate) signing_certificate: Option<Vec<u8>>,
    /// The desktop the guest found in itself, when the recipe reported one.
    ///
    /// `None` is a report that looked and found nothing, and a report that
    /// never looked -- an update, or a mount that failed. Either way it leaves
    /// whatever the guest last said standing, because a guest does not stop
    /// having a desktop between two questions about its payload.
    pub(crate) desktop: Option<GuestDesktop>,
}

/// Where a display report goes, for the same reason the GPU's has a sink.
pub(crate) type GuestDisplaySink<'a> = &'a dyn Fn(GuestDisplayPayloadReport);

/// What one session is to do for a VM, and where its answers go.
///
/// A struct rather than four more parameters: the GPU half and the display
/// half are decided separately and neither is a variation of the other, and a
/// six-argument `serve` is a call nobody can read.
pub(crate) struct SessionWork<'a> {
    /// The GPU shares this VM's guest is to mount, if any.
    pub(crate) gpu_shares: Option<&'a GpuShareManifest>,
    /// The display payload share this VM's guest is to mount, if any.
    pub(crate) display_share: Option<&'a DisplayShare>,
    /// The mode this VM's output is to come up at, if one is stored with it.
    ///
    /// It belongs to the run for the reason the share does: a module parameter
    /// is read once, when the module loads, so every session of a run carries
    /// the same mode and a changed one reaches the output through a reload.
    pub(crate) display_mode: Option<DisplayMode>,
    pub(crate) gpu: GuestGpuSink<'a>,
    pub(crate) display: GuestDisplaySink<'a>,
    /// Where a display payload update arrives from, when this session is one
    /// somebody can ask things of.
    ///
    /// Read between frames rather than written to the socket from another
    /// thread: a session is one conversation, and two writers would interleave
    /// halfway through a frame.
    pub(crate) updates: Option<&'a Receiver<DisplayUpdate>>,
}

/// What a session agreed on when it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSession {
    /// The revision both peers speak, which is the lower of the two minors.
    pub(crate) version: ProtocolVersion,
    /// The capabilities both peers have, which is the only set either may use.
    pub(crate) capabilities: Vec<Capability>,
    /// The agent build that is speaking, for logs only.
    ///
    /// Kept because an agent is installed once, at a VM's first boot, and then
    /// outlives any number of host rebuilds. Which build is answering is
    /// otherwise invisible, and guessing at it costs a real-host round trip
    /// every time.
    pub(crate) build: String,
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
    open_with_timeout(stream, secret, vm_name, OPEN_TIMEOUT)
}

pub(crate) fn open_replacement<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    vm_name: &str,
) -> Result<AgentSession, SessionError> {
    open_with_timeout(stream, secret, vm_name, REPLACEMENT_OPEN_TIMEOUT)
}

fn open_with_timeout<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    vm_name: &str,
    timeout: Duration,
) -> Result<AgentSession, SessionError> {
    let mut buffer = Vec::new();
    let deadline = Instant::now() + timeout;
    let session = greet(stream, vm_name, &mut buffer, deadline)?;
    authenticate(stream, secret, vm_name, &mut buffer, deadline)?;

    tracing::info!(
        "the agent of VM \"{vm_name}\" is build \"{}\" and opened a session on protocol \
         {}.{} with {} agreed capability(ies)",
        session.build,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionExit {
    Closed,
    Replaced,
}

#[cfg(test)]
pub(crate) fn serve<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    work: SessionWork<'_>,
    vm_name: &str,
) -> Result<(), SessionError> {
    serve_with_timing(stream, session, work, vm_name, SessionTiming::NORMAL, None).map(|_| ())
}

/// Serves until the peer closes or the transport owner has authenticated a
/// newer connection for the same VM.
///
/// `replacement_ready` is called only between frames. It must return `true`
/// only after the candidate has completed [`open`], so an unauthenticated peer
/// cannot evict the session that already proved its secret.
pub(crate) fn serve_with_replacement<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    work: SessionWork<'_>,
    vm_name: &str,
    replacement_ready: &mut dyn FnMut() -> bool,
) -> Result<SessionExit, SessionError> {
    serve_with_timing(
        stream,
        session,
        work,
        vm_name,
        SessionTiming::NORMAL,
        Some(replacement_ready),
    )
}

fn serve_with_timing<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    work: SessionWork<'_>,
    vm_name: &str,
    timing: SessionTiming,
    mut replacement_ready: Option<&mut dyn FnMut() -> bool>,
) -> Result<SessionExit, SessionError> {
    let mut buffer = Vec::new();
    tracing::debug!(
        "serving the agent of VM \"{vm_name}\" on protocol {}.{}",
        session.version.major,
        session.version.minor
    );

    let sink = work.gpu;
    let mut pending_manifest =
        attach_shares(stream, session, work.gpu_shares, vm_name, &mut buffer)?;
    let mut pending_recipe = None;
    let mut pending_probe = None;
    // Sent up front and answered in turn: the guest serves one request at a
    // time, so the GPU's attach and recipe still go first, and the display's
    // long stage waits behind them without the host having to sequence it.
    let mut pending_display_attach =
        attach_display(stream, session, work.display_share, vm_name, &mut buffer)?;
    let mut pending_display_recipe = None;
    // At most one update at a time, and its answer channel with it: a second
    // request while one is in flight would be a second question on a socket
    // that is still waiting for the first answer.
    let mut pending_update: Option<Sender<DisplayUpdateAnswer>> = None;
    let mut update_after_probe: Option<DisplayUpdate> = None;
    let mut liveness_deadline = None;
    let mut last_received = Instant::now();

    loop {
        let envelope = match frame::read(stream, &mut buffer) {
            Ok(envelope) => {
                last_received = Instant::now();
                envelope
            }
            Err(FrameError::Closed) => {
                tracing::info!("the agent of VM \"{vm_name}\" closed its session");
                return Ok(SessionExit::Closed);
            }
            // `Idle` is reported only before a peer starts another frame, so
            // retrying cannot abandon a partial prefix or body. This is the
            // normal result of the bounded socket reads that let VM shutdown
            // interrupt a silent agent session.
            // The one place another thread's request can get onto this socket:
            // between frames, where nothing is half-written.
            Err(FrameError::Idle) => {
                if replacement_ready
                    .as_mut()
                    .is_some_and(|replacement_ready| replacement_ready())
                {
                    return Ok(SessionExit::Replaced);
                }
                let now = Instant::now();
                if liveness_deadline.is_some_and(|deadline| now >= deadline) {
                    return Err(SessionError::Unresponsive);
                }

                let has_pending_work = pending_manifest.is_some()
                    || pending_recipe.is_some()
                    || pending_probe.is_some()
                    || pending_display_attach.is_some()
                    || pending_display_recipe.is_some()
                    || pending_update.is_some();
                if liveness_deadline.is_none() && !has_pending_work {
                    if let Some(update) = work.updates.and_then(|updates| updates.try_recv().ok()) {
                        if session.capabilities.contains(&Capability::Display) {
                            send_liveness_probe(stream, &mut buffer)?;
                            update_after_probe = Some(update);
                            liveness_deadline = Some(now + timing.probe_timeout);
                        } else {
                            answer_unsupported_update(update);
                        }
                    } else if now.duration_since(last_received) >= timing.idle_before_probe {
                        send_liveness_probe(stream, &mut buffer)?;
                        liveness_deadline = Some(now + timing.probe_timeout);
                    }
                }
                continue;
            }
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
                // A recipe that did not finish ends the guest's GPU here:
                // nothing renders on a module that was not built, and a probe
                // would only be a second way of saying so.
                if report_recipe(&report, vm_name, sink) {
                    pending_probe = probe_gpu(stream, vm_name, &mut buffer)?;
                }
            }
            Body::Response(response::Kind::ProbeGpu(report))
                if pending_probe == Some(request_id) =>
            {
                pending_probe = None;
                report_probe(&report, vm_name, sink);
            }
            Body::Response(response::Kind::AttachDisplayPayload(report))
                if pending_display_attach == Some(request_id) =>
            {
                pending_display_attach = None;
                if report_display_mount(&report, vm_name, work.display) {
                    pending_display_recipe =
                        apply_display_recipe(stream, work.display_mode, vm_name, &mut buffer)?;
                }
            }
            Body::Response(response::Kind::ApplyDisplayRecipe(report))
                if pending_display_recipe == Some(request_id) =>
            {
                pending_display_recipe = None;
                report_display_recipe(&report, vm_name, work.display);
            }
            Body::Response(response::Kind::UpdateDisplayPayload(report))
                if request_id == DISPLAY_UPDATE_REQUEST_ID =>
            {
                let answer = report_display_update(&report, vm_name, work.display);
                if let Some(waiting) = pending_update.take() {
                    // A caller that gave up while the guest was building is
                    // not an error here: the update happened either way, and
                    // the facts it produced have already been recorded.
                    let _ = waiting.send(answer);
                }
            }
            Body::Response(response::Kind::Heartbeat(_))
                if request_id == LIVENESS_REQUEST_ID && liveness_deadline.take().is_some() =>
            {
                if let Some(update) = update_after_probe.take() {
                    pending_update = start_update(stream, session, &update, vm_name, &mut buffer)?;
                }
            }
            // A response to a request this side did not send, or one it has
            // already had an answer to. Worth a line and nothing more: there is
            // no id left to fail, and the session is otherwise intact.
            Body::Response(_) => tracing::warn!(
                "the agent of VM \"{vm_name}\" answered request {request_id}, which VMLord \
                 never sent"
            ),
        }
    }
}

fn send_liveness_probe<S: Write>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<(), SessionError> {
    let heartbeat = Envelope::request(
        LIVENESS_REQUEST_ID,
        request::Kind::Heartbeat(HeartbeatRequest {}),
    );
    frame::write(stream, &heartbeat, buffer).map_err(SessionError::Frame)
}

fn answer_unsupported_update(update: DisplayUpdate) {
    let _ = update.answer.send(DisplayUpdateAnswer {
        outcome: DisplayUpdateOutcome::Failed,
        report: GuestDisplayPayloadReport {
            failure: Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadUpdateFailed,
                "this guest's agent does not speak the display capability",
            )),
            ..GuestDisplayPayloadReport::default()
        },
    });
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
        tracing::warn!(
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
    tracing::debug!(
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
    tracing::debug!("VMLord asked the agent of VM \"{vm_name}\" to apply its GPU recipe");

    Ok(Some(APPLY_REQUEST_ID))
}

/// Asks the guest whether anything renders, and says which id asked.
///
/// After the recipe of the same session, because what it looks at is what the
/// recipe has just installed. Once per session, for the same reason the recipe
/// is asked for once: the answer describes a moment, and the next session asks
/// again.
fn probe_gpu<S: Read + Write>(
    stream: &mut S,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let request = Envelope::request(
        PROBE_REQUEST_ID,
        request::Kind::ProbeGpu(ProbeGpuRequest {}),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    tracing::debug!("VMLord asked the agent of VM \"{vm_name}\" to probe its GPU");

    Ok(Some(PROBE_REQUEST_ID))
}

/// One share in the form the wire carries it.
///
/// The roles are the same two facts on both sides, so the mapping is total
/// rather than fallible: a role `vmlord_core` has and this does not would fail
/// to compile here, which is where it should fail.
fn wire_share(share: &vmlord_core::GpuShare) -> vmlord_agent_protocol::v1::GpuShare {
    let (role, package) = match &share.role {
        CoreShareRole::WslLib => (GpuShareRole::WslLib, String::new()),
        CoreShareRole::WslD3d12 => (GpuShareRole::WslD3d12, String::new()),
        CoreShareRole::GpuPayload => (GpuShareRole::GpuPayload, String::new()),
        CoreShareRole::DriverPackage { package } => (GpuShareRole::DriverPackage, package.clone()),
    };

    vmlord_agent_protocol::v1::GpuShare {
        name: share.name.clone(),
        role: i32::from(role),
        package,
    }
}

/// Offers the guest its display payload share, and says which id asked.
///
/// `None` comes back when there is nothing to offer or nobody to offer it to:
/// a headless VM, a VM this release carries no payload for, or an agent too
/// old to have the display capability. All three are sessions that simply
/// never wait for a mount report.
fn attach_display<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    share: Option<&DisplayShare>,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let Some(share) = share else {
        return Ok(None);
    };
    if !session.capabilities.contains(&Capability::Display) {
        tracing::warn!(
            "the agent of VM \"{vm_name}\" does not speak the display capability, so its \
             display payload is exported but not mounted"
        );
        return Ok(None);
    }

    let request = Envelope::request(
        DISPLAY_ATTACH_REQUEST_ID,
        request::Kind::AttachDisplayPayload(AttachDisplayPayloadRequest {
            share: Some(WireDisplayShare {
                name: share.name.clone(),
            }),
        }),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    tracing::debug!("VMLord offered the agent of VM \"{vm_name}\" its display payload share");

    Ok(Some(DISPLAY_ATTACH_REQUEST_ID))
}

/// Asks the guest to apply its display recipe, and says which id asked.
///
/// After the mount of the same session, because the module is built out of the
/// payload the guest has just mounted. Once per session, for the reason the
/// GPU recipe is asked for once: the guest reconciles rather than rebuilds,
/// and a retry loop around a kernel build is how a guest ends up compiling
/// continuously.
fn apply_display_recipe<S: Read + Write>(
    stream: &mut S,
    mode: Option<DisplayMode>,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let request = Envelope::request(
        DISPLAY_APPLY_REQUEST_ID,
        request::Kind::ApplyDisplayRecipe(ApplyDisplayRecipeRequest {
            initial_mode: mode.map(|mode| vmlord_agent_protocol::v1::DisplayMode {
                width: mode.width(),
                height: mode.height(),
            }),
        }),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    tracing::debug!(
        "VMLord asked the agent of VM \"{vm_name}\" to apply its display recipe{}",
        match mode {
            Some(mode) => format!(" at {mode}"),
            None => String::new(),
        }
    );

    Ok(Some(DISPLAY_APPLY_REQUEST_ID))
}

/// Asks the guest to move to a version, and answers with where to send the
/// answer.
///
/// `None` is an agent that never agreed the display capability, which is a
/// guest nothing can be asked of.
fn start_update<S: Read + Write>(
    stream: &mut S,
    session: &AgentSession,
    update: &DisplayUpdate,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<Sender<DisplayUpdateAnswer>>, SessionError> {
    if !session.capabilities.contains(&Capability::Display) {
        return Ok(None);
    }

    let request = Envelope::request(
        DISPLAY_UPDATE_REQUEST_ID,
        request::Kind::UpdateDisplayPayload(UpdateDisplayPayloadRequest {
            target_version: update.target_version.clone(),
        }),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    tracing::info!(
        "VMLord asked the agent of VM \"{vm_name}\" to move its display payload to {}",
        update.target_version
    );

    Ok(Some(update.answer.clone()))
}

/// Why an update did not end up where it was asked to go.
///
/// The stage that broke says it, when the guest got far enough to record one.
/// When it did not -- a guest that is shutting down, or one that could not
/// read its own facts -- the reason is written onto every stage that never
/// ran, so the last stage of the report carries it just the same. Only a
/// report with nothing in it leaves the host with nothing to say.
fn update_reason(stages: &[DisplayRecipeStage]) -> String {
    stages
        .iter()
        .find(|stage| stage.state() == DisplayRecipeStageState::Failed)
        .or_else(|| {
            stages
                .iter()
                .rev()
                .find(|stage| stage.state() == DisplayRecipeStageState::Skipped)
        })
        .map(|stage| stage.message.clone())
        .unwrap_or_else(|| "the guest did not say which stage failed".to_owned())
}

/// Says what an update came to, records the facts it produced, and hands the
/// caller the outcome.
///
/// A rollback is reported as what it is: the display works, on the version that
/// was working before, and the failure that goes with it says so rather than
/// calling a working desktop broken.
fn report_display_update(
    report: &UpdateDisplayPayloadResponse,
    vm_name: &str,
    sink: GuestDisplaySink<'_>,
) -> DisplayUpdateAnswer {
    for stage in &report.stages {
        match stage.state() {
            DisplayRecipeStageState::Failed => tracing::warn!(
                "the agent of VM \"{vm_name}\" did not finish update stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            state => tracing::debug!(
                "the agent of VM \"{vm_name}\" update stage {:?} ({state:?}): {}",
                stage.step(),
                stage.message
            ),
        }
    }

    let versions = report.versions.clone().unwrap_or_default();
    let outcome = report.outcome();
    let reason = update_reason(&report.stages);
    let failure = match outcome {
        DisplayUpdateOutcome::Updated => None,
        DisplayUpdateOutcome::RolledBack => Some(DisplayFailure::new(
            DisplayStage::Payload,
            DisplayStatusCode::PayloadUpdateRolledBack,
            reason,
        )),
        DisplayUpdateOutcome::RebootRequired => Some(DisplayFailure::new(
            DisplayStage::Payload,
            DisplayStatusCode::PayloadUpdateRebootRequired,
            reason,
        )),
        _ => Some(DisplayFailure::new(
            DisplayStage::Payload,
            DisplayStatusCode::PayloadUpdateFailed,
            reason,
        )),
    };

    let payload = GuestDisplayPayloadReport {
        installed: some_text(&versions.installed),
        previous: some_text(&versions.previous),
        loaded: some_text(&versions.loaded),
        failure,
        // An update carries no certificate. The key is the VM's and does not
        // change between a start and an update, and the next start's recipe
        // reports it.
        signing_certificate: None,
        // An update moves versions. Whether anything is listening is what the
        // recipe reported when the session opened, and is not this answer's
        // to change.
        guest: None,
        // Nor does an update look at the desktop: the guest was asked to
        // change a module version, and the answer to a different question is
        // still the one the recipe gave.
        desktop: None,
    };
    sink(payload.clone());

    match outcome {
        DisplayUpdateOutcome::Updated => tracing::info!(
            "the agent of VM \"{vm_name}\" is running display payload {}",
            versions.loaded
        ),
        DisplayUpdateOutcome::RolledBack => tracing::warn!(
            "the display payload update of VM \"{vm_name}\" did not verify; {} is running again",
            versions.loaded
        ),
        DisplayUpdateOutcome::RebootRequired => tracing::warn!(
            "display payload {} is installed for VM \"{vm_name}\" and will load after reboot",
            versions.installed
        ),
        _ => tracing::error!("the display payload update of VM \"{vm_name}\" left nothing running"),
    }

    DisplayUpdateAnswer {
        outcome,
        report: payload,
    }
}

/// Says what the guest made of the display share, and whether the recipe is
/// worth asking for.
///
/// A share that was refused or would not mount is a display that cannot be
/// installed, and asking for the recipe anyway would only be a second way of
/// saying so.
fn report_display_mount(
    report: &AttachDisplayPayloadResponse,
    vm_name: &str,
    sink: GuestDisplaySink<'_>,
) -> bool {
    let Some(mount) = report.mount.as_ref() else {
        sink(GuestDisplayPayloadReport {
            failure: Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadInvalid,
                "the guest answered the display share with no mount report",
            )),
            ..GuestDisplayPayloadReport::default()
        });
        return false;
    };

    match mount.state() {
        DisplayMountState::Mounted | DisplayMountState::AlreadyMounted => {
            tracing::info!(
                "the agent of VM \"{vm_name}\" has its display payload at {}",
                mount.mount_point
            );
            true
        }
        state => {
            tracing::warn!(
                "the agent of VM \"{vm_name}\" did not mount its display payload ({state:?}): {}",
                mount.message
            );
            sink(GuestDisplayPayloadReport {
                failure: Some(DisplayFailure::new(
                    DisplayStage::Payload,
                    DisplayStatusCode::PayloadInvalid,
                    format!(
                        "the guest did not mount its display payload: {}",
                        mount.message
                    ),
                )),
                ..GuestDisplayPayloadReport::default()
            });
            false
        }
    }
}

/// Says what the guest's display recipe did, at the volume each stage earns,
/// and turns the whole of it into one report.
fn report_display_recipe(
    report: &ApplyDisplayRecipeResponse,
    vm_name: &str,
    sink: GuestDisplaySink<'_>,
) {
    for stage in &report.stages {
        match stage.state() {
            DisplayRecipeStageState::Ok => tracing::debug!(
                "the agent of VM \"{vm_name}\" finished display recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            DisplayRecipeStageState::Skipped => tracing::info!(
                "the agent of VM \"{vm_name}\" skipped display recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            state => tracing::warn!(
                "the agent of VM \"{vm_name}\" did not finish display recipe stage {:?} \
                 ({state:?}): {}",
                stage.step(),
                stage.message
            ),
        }
    }

    let versions = report.versions.clone().unwrap_or_default();
    let failure = report
        .stages
        .iter()
        .find(|stage| {
            stage.state() == DisplayRecipeStageState::Failed
                && !matches!(
                    stage.step(),
                    // Neither can degrade a display: Secure Boot is off, an
                    // unsigned module loads, and a desktop that works is not
                    // failed over a signature nothing checks yet.
                    DisplayRecipeStep::SigningKey | DisplayRecipeStep::ModuleSignature
                )
        })
        .map(|broken| {
            let message = format!(
                "the guest's display recipe stopped at {:?}: {}",
                broken.step(),
                broken.message
            );
            DisplayFailure::new(
                DisplayStage::Payload,
                code_for(broken.step(), &message),
                message,
            )
        });

    // The last stage is the readiness a viewer waits for: the guest marks it
    // `Ok` only once both units are active and the socket between them exists,
    // which is what proves the two halves met. Reading it here is why no
    // second question has to be asked over this channel.
    let services = report
        .stages
        .iter()
        .find(|stage| stage.step() == DisplayRecipeStep::ServicesStart)
        .map(|stage| stage.state());
    // What the guest says it found, which is a different question from what
    // the VM was created asking for: the host holds the profile itself and
    // never reads it back off this channel.
    let desktop = report.desktop.as_ref().map(|desktop| GuestDesktop {
        session: some_text(&desktop.session),
        session_type: some_text(&desktop.session_type),
        display_manager: some_text(&desktop.display_manager),
    });
    // Readiness is a separate answer and stays one: what a guest found is
    // carried beside the report and not folded into it, so there is one place
    // the found desktop is read from rather than two that can disagree.
    let guest = match (services, &failure) {
        (Some(DisplayRecipeStageState::Ok), _) => {
            Some(GuestDisplayReport::Ready(GuestDisplayDetail::default()))
        }
        // A payload built before the services existed carries none, and a
        // guest that has nothing to start will never offer a display. Saying
        // so beats a wait that cannot end.
        (Some(DisplayRecipeStageState::Skipped), _) => {
            Some(GuestDisplayReport::Failed(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadInvalid,
                "this display payload carries no display services",
            )))
        }
        (_, Some(failure)) => Some(GuestDisplayReport::Failed(failure.clone())),
        // A recipe that reached neither says nothing about the guest's
        // services, and inventing an answer would either promise a desktop
        // that is not there or condemn one that is.
        _ => None,
    };

    sink(GuestDisplayPayloadReport {
        installed: some_text(&versions.installed),
        previous: some_text(&versions.previous),
        loaded: some_text(&versions.loaded),
        failure,
        guest,
        signing_certificate: report
            .signing_certificate
            .as_ref()
            .map(|certificate| certificate.certificate.clone()),
        desktop,
    });
}

/// Which cause a failed stage is.
///
/// One code per stage rather than one for all of them: "the headers would not
/// install" and "the module built and no device appeared" are one word apart
/// in a summary and are different problems.
fn code_for(
    step: vmlord_agent_protocol::v1::DisplayRecipeStep,
    message: &str,
) -> DisplayStatusCode {
    use vmlord_agent_protocol::v1::DisplayRecipeStep as Step;

    match step {
        Step::BuildDependencies => DisplayStatusCode::PayloadDependenciesFailed,
        Step::ModuleBuild | Step::ModuleSource => DisplayStatusCode::PayloadBuildFailed,
        // A module the kernel refused over its signature is not a module that
        // would not load: the fix is an enrollment, and no retry performs one.
        Step::Initramfs | Step::ModuleLoad
            if vmlord_core::display::was_rejected_for_its_signature(message) =>
        {
            DisplayStatusCode::PayloadModuleSignatureRejected
        }
        Step::Initramfs | Step::ModuleLoad => DisplayStatusCode::PayloadModuleNotLoaded,
        Step::Device => DisplayStatusCode::PayloadNoDevice,
        // The module is loaded and the desktop is black: a compositor that
        // was left on the payload's Mesa never finishes its modeset, which
        // from the outside is the same nothing as a module that never loaded.
        Step::CompositorIsolation => DisplayStatusCode::PayloadModuleNotLoaded,
        Step::Services | Step::ServicesStart => DisplayStatusCode::GuestServicesFailed,
        // Filtered out before they reach here, and matched so that a step
        // added later cannot be swallowed by a catch-all.
        Step::SigningKey | Step::ModuleSignature => DisplayStatusCode::PayloadInvalid,
        Step::Distribution | Step::Payload | Step::Unspecified => DisplayStatusCode::PayloadInvalid,
    }
}

/// An empty string on the wire is "not present" here.
///
/// proto3 scalars have no absence, so every optional string the guest sends --
/// a payload version, the name of a desktop -- arrives empty when the guest
/// has none, and this is where that becomes the host's own `None`.
fn some_text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
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
            GpuMountState::Mounted => tracing::debug!(
                "the agent of VM \"{vm_name}\" mounted {} at {}",
                mount.share,
                mount.path
            ),
            state => tracing::warn!(
                "the agent of VM \"{vm_name}\" did not mount {} ({state:?}): {}",
                mount.share,
                mount.message
            ),
        }
    }

    if !report.libraries_refreshed && !report.mounts.is_empty() {
        tracing::warn!(
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
fn report_recipe(report: &ApplyGpuRecipeResponse, vm_name: &str, sink: GuestGpuSink<'_>) -> bool {
    for stage in &report.stages {
        match stage.state() {
            GpuRecipeStageState::Ok => tracing::debug!(
                "the agent of VM \"{vm_name}\" finished GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            // Info, unlike a stage that finished: a step that did not run is
            // how a recipe reports "there was nothing for me to do here", and
            // a guest that ends up with no device after a recipe that failed
            // nowhere is explained by exactly these lines.
            GpuRecipeStageState::Skipped => tracing::info!(
                "the agent of VM \"{vm_name}\" skipped GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            state => tracing::warn!(
                "the agent of VM \"{vm_name}\" did not finish GPU recipe stage {:?} ({state:?}): \
                 {}",
                stage.step(),
                stage.message
            ),
        }
    }

    let Some(broken) = report.stages.iter().find(|stage| {
        !matches!(
            stage.state(),
            GpuRecipeStageState::Ok | GpuRecipeStageState::Skipped
        )
    }) else {
        return true;
    };

    sink(GuestGpuReport::Failed(GpuFailure::new(
        GpuStatusCode::GuestFailed,
        format!(
            "the guest's GPU recipe stopped at {:?}: {}",
            broken.step(),
            broken.message
        ),
    )));
    false
}

/// Says what the guest found, at the volume each check earns.
///
/// Nothing is kept: the next session probes again, and turning a verdict into
/// a `VmGpuFacts` is the application layer's work.
fn report_probe(report: &ProbeGpuResponse, vm_name: &str, sink: GuestGpuSink<'_>) {
    match report.verdict() {
        GpuProbeVerdict::Renders => tracing::info!(
            "the agent of VM \"{vm_name}\" renders on {}",
            report.renderer
        ),
        verdict => {
            tracing::warn!("the agent of VM \"{vm_name}\" does not render on its GPU ({verdict:?})")
        }
    }

    for check in &report.checks {
        match check.state() {
            GpuProbeCheckState::Ok | GpuProbeCheckState::Skipped => tracing::debug!(
                "the agent of VM \"{vm_name}\" GPU check {:?} ({:?}): {}",
                check.step(),
                check.state(),
                check.message
            ),
            state => tracing::warn!(
                "the agent of VM \"{vm_name}\" failed GPU check {:?} ({state:?}): {}",
                check.step(),
                check.message
            ),
        }
    }

    // The verdict is the guest's to give: it is the only side that saw the
    // output of the programs it ran, and a host that re-derived one from the
    // checks could disagree with the peer that produced them.
    let detail = GuestGpuDetail {
        driver: (!report.driver.is_empty()).then(|| report.driver.clone()),
        render_node: (!report.render_node.is_empty()).then(|| report.render_node.clone()),
    };
    sink(match report.verdict() {
        GpuProbeVerdict::Renders => GuestGpuReport::Ready(detail),
        GpuProbeVerdict::DeviceOnly => GuestGpuReport::DevicePresent(detail),
        // `Unspecified` is an agent answering with a verdict this build does
        // not know, which is not a working GPU either.
        verdict => GuestGpuReport::Failed(GpuFailure::new(
            GpuStatusCode::GuestFailed,
            format!("the guest reports no usable GPU ({verdict:?})"),
        )),
    });
}

/// Settles the protocol revision and the capabilities of a new session.
fn greet<S: Read + Write>(
    stream: &mut S,
    vm_name: &str,
    buffer: &mut Vec<u8>,
    deadline: Instant,
) -> Result<AgentSession, SessionError> {
    let envelope = read_opening_frame(stream, buffer, deadline)?;
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

    tracing::debug!(
        "the agent of VM \"{vm_name}\" speaks protocol {}.{}",
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
        build: hello.agent_version,
    })
}

/// Challenges the guest and waits for a tag that verifies.
fn authenticate<S: Read + Write>(
    stream: &mut S,
    secret: &Secret,
    vm_name: &str,
    buffer: &mut Vec<u8>,
    deadline: Instant,
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
        let envelope = read_opening_frame(stream, buffer, deadline)?;
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
        tracing::warn!(
            "the agent of VM \"{vm_name}\" sent request {request_id} before answering its \
             challenge; it was refused"
        );
        frame::write(stream, &refusal, buffer).map_err(SessionError::Frame)?;
    }
}

fn read_opening_frame<S: Read>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    deadline: Instant,
) -> Result<Envelope, SessionError> {
    loop {
        match frame::read(stream, buffer) {
            Ok(envelope) => return Ok(envelope),
            Err(FrameError::Idle) if Instant::now() < deadline => {}
            Err(FrameError::Idle) => return Err(SessionError::OpeningTimedOut),
            Err(error) => return Err(SessionError::Frame(error)),
        }
    }
}

/// The answer to a request numbered `request_id`, as this build serves it.
fn answer(request_id: u32, kind: &request::Kind, vm_name: &str) -> Envelope {
    match kind {
        request::Kind::Heartbeat(_) => {
            tracing::trace!("the agent of VM \"{vm_name}\" is alive");
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
        // Likewise: the probe is the guest's to run and the host's to ask for,
        // and there is no GPU to probe on this side of the socket.
        request::Kind::ProbeGpu(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a GPU probe is the host's to ask for",
        ),
        // The display's three requests are the host's to ask, for the reason
        // the GPU's are: a guest that asked the host to mount, apply or update
        // something would be a guest with the conversation the wrong way round.
        request::Kind::AttachDisplayPayload(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a display payload share is the host's to offer",
        ),
        request::Kind::ApplyDisplayRecipe(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a display recipe is the host's to ask for",
        ),
        request::Kind::UpdateDisplayPayload(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a display payload update is the host's to ask for",
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
                tracing::warn!(
                    "the agent of VM \"{vm_name}\" sent request {request_id} with no kind this \
                 build knows"
                );
                SessionError::Malformed("a request with no kind this build knows".to_owned())
            })
        }
        Some(envelope::Body::Response(response)) => {
            response.kind.map(Body::Response).ok_or_else(|| {
                tracing::warn!(
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
    /// The connection accepted a liveness probe but never answered it.
    Unresponsive,
    /// Hello and authentication did not finish inside their shared budget.
    OpeningTimedOut,
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
            Self::Unresponsive => formatter.write_str("the agent stopped answering heartbeats"),
            Self::OpeningTimedOut => {
                formatter.write_str("the agent did not open its session in time")
            }
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

    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use vmlord_agent_protocol::{
        auth::{Nonce, Secret, tag},
        frame::{self, LENGTH_PREFIX_LEN},
        v1::{
            ApplyDisplayRecipeResponse, ApplyGpuRecipeResponse, AttachDisplayPayloadResponse,
            AttachGpuSharesResponse, AuthenticateResponse, Capability, DisplayMountState,
            DisplayPayloadVersions, DisplayRecipeStage, DisplayRecipeStageState, DisplayRecipeStep,
            DisplaySigningCertificate, DisplayUpdateOutcome, Envelope, ErrorCode, GpuMount,
            GpuMountState, GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict,
            GpuRecipeStage, GpuRecipeStageState, GpuRecipeStep, GpuShareRole, HeartbeatRequest,
            HeartbeatResponse, HelloRequest, ProbeGpuResponse, ProtocolVersion,
            UpdateDisplayPayloadResponse, envelope, request, response,
        },
    };

    use vmlord_core::{
        DisplayShare, DisplayStage, DisplayStatusCode, GpuShareManifest, GpuStatusCode,
        GuestDisplayReport, GuestGpuDetail, GuestGpuReport,
    };

    use crate::agent::DisplayUpdate;

    use super::{
        AgentSession, GuestDisplaySink, GuestGpuSink, SessionError, SessionExit, SessionTiming,
        SessionWork, open, report_display_recipe, report_display_update, serve,
        serve_with_replacement, serve_with_timing,
    };

    /// The readiness a recipe with these stages reports, if any.
    fn readiness(stages: Vec<DisplayRecipeStage>) -> Option<GuestDisplayReport> {
        let report = ApplyDisplayRecipeResponse {
            stages,
            versions: None,
            signing_certificate: None,
            desktop: None,
        };
        let seen = Mutex::new(None);

        report_display_recipe(&report, "dev", &|report| {
            *seen.lock().expect("an uncontended lock") = report.guest;
        });

        seen.into_inner().expect("an uncontended lock")
    }

    /// One recipe stage, as the guest reports it.
    fn stage(step: DisplayRecipeStep, state: DisplayRecipeStageState) -> DisplayRecipeStage {
        DisplayRecipeStage {
            step: step as i32,
            state: state as i32,
            message: "what the guest said".to_owned(),
        }
    }

    #[test]
    fn a_recipe_that_stopped_still_says_what_desktop_the_guest_has() {
        // The answer is worth most exactly here: a VM that asked for a desktop
        // and did not get a display is one somebody has to go looking in, and
        // what the guest reports is where to start.
        let report = ApplyDisplayRecipeResponse {
            stages: vec![stage(
                DisplayRecipeStep::ModuleBuild,
                DisplayRecipeStageState::Failed,
            )],
            versions: None,
            signing_certificate: None,
            desktop: Some(vmlord_agent_protocol::v1::GuestDesktop {
                session: "gnome".to_owned(),
                session_type: "wayland".to_owned(),
                display_manager: "gdm.service".to_owned(),
            }),
        };
        let seen = Mutex::new(None);

        report_display_recipe(&report, "dev", &|report| {
            *seen.lock().expect("an uncontended lock") = report.desktop;
        });

        let desktop = seen
            .into_inner()
            .expect("an uncontended lock")
            .expect("the guest reported a desktop");
        assert_eq!(desktop.session.as_deref(), Some("gnome"));
        assert_eq!(desktop.display_manager.as_deref(), Some("gdm.service"));
    }

    #[test]
    fn a_desktop_the_guest_did_not_find_arrives_as_absence_and_not_as_empty_names() {
        // Every guest while its VM is being built: the recipe runs as root
        // with nobody logged in. Three empty strings would read as a desktop
        // whose every name is blank.
        let report = ApplyDisplayRecipeResponse {
            stages: Vec::new(),
            versions: None,
            signing_certificate: None,
            desktop: None,
        };
        let seen = Mutex::new(None);

        report_display_recipe(&report, "dev", &|report| {
            *seen.lock().expect("an uncontended lock") = report.desktop;
        });

        assert_eq!(seen.into_inner().expect("an uncontended lock"), None);
    }

    #[test]
    fn a_guest_whose_services_are_running_offers_its_display() {
        assert!(matches!(
            readiness(vec![stage(
                DisplayRecipeStep::ServicesStart,
                DisplayRecipeStageState::Ok,
            )]),
            Some(GuestDisplayReport::Ready(_))
        ));
    }

    #[test]
    fn a_payload_that_carries_no_services_is_a_display_that_will_never_arrive() {
        let Some(GuestDisplayReport::Failed(failure)) = readiness(vec![stage(
            DisplayRecipeStep::ServicesStart,
            DisplayRecipeStageState::Skipped,
        )]) else {
            panic!("a payload with no services cannot offer a display");
        };

        assert_eq!(failure.code, DisplayStatusCode::PayloadInvalid);
    }

    #[test]
    fn a_recipe_that_stopped_reports_the_guest_as_failed_for_the_same_reason() {
        let Some(GuestDisplayReport::Failed(failure)) = readiness(vec![stage(
            DisplayRecipeStep::ModuleBuild,
            DisplayRecipeStageState::Failed,
        )]) else {
            panic!("a recipe that stopped is a guest that offers nothing");
        };

        assert_eq!(failure.code, DisplayStatusCode::PayloadBuildFailed);
    }

    #[test]
    fn a_recipe_that_reached_neither_says_nothing_about_the_guest() {
        assert_eq!(
            readiness(vec![stage(
                DisplayRecipeStep::Distribution,
                DisplayRecipeStageState::Ok,
            )]),
            None,
            "a guest that has not got there yet has not failed either"
        );
    }

    #[test]
    fn an_update_that_recorded_no_broken_stage_still_reports_the_guest_s_reason() {
        let report = UpdateDisplayPayloadResponse {
            stages: vec![
                DisplayRecipeStage {
                    step: DisplayRecipeStep::Distribution as i32,
                    state: DisplayRecipeStageState::Ok as i32,
                    message: "ubuntu 24.04 x86_64".to_owned(),
                },
                DisplayRecipeStage {
                    step: DisplayRecipeStep::ModuleBuild as i32,
                    state: DisplayRecipeStageState::Skipped as i32,
                    message: "the guest is shutting down".to_owned(),
                },
                DisplayRecipeStage {
                    step: DisplayRecipeStep::ServicesStart as i32,
                    state: DisplayRecipeStageState::Skipped as i32,
                    message: "the guest is shutting down".to_owned(),
                },
            ],
            versions: None,
            outcome: DisplayUpdateOutcome::Failed as i32,
        };
        let seen = Mutex::new(None);

        report_display_update(&report, "dev", &|report| {
            *seen.lock().expect("an uncontended lock") = report.failure;
        });

        assert_eq!(
            seen.into_inner()
                .expect("an uncontended lock")
                .expect("an update that failed is actionable")
                .message,
            "the guest is shutting down",
            "the guest said why; the host must not print a placeholder over it"
        );
    }

    #[test]
    fn an_update_waiting_for_reboot_is_not_reported_as_failed() {
        let report = UpdateDisplayPayloadResponse {
            stages: vec![DisplayRecipeStage {
                step: DisplayRecipeStep::ModuleLoad as i32,
                state: DisplayRecipeStageState::Failed as i32,
                message: "vmlord_drm is busy; reboot the guest".to_owned(),
            }],
            versions: Some(DisplayPayloadVersions {
                installed: "0.2.0".to_owned(),
                previous: "0.1.0".to_owned(),
                loaded: "0.1.0".to_owned(),
            }),
            outcome: DisplayUpdateOutcome::RebootRequired as i32,
        };
        let seen = Mutex::new(None);

        let answer = report_display_update(&report, "dev", &|report| {
            *seen.lock().expect("an uncontended lock") = report.failure;
        });

        assert_eq!(answer.outcome, DisplayUpdateOutcome::RebootRequired);
        assert_eq!(
            seen.into_inner()
                .expect("an uncontended lock")
                .expect("a reboot is actionable")
                .code,
            DisplayStatusCode::PayloadUpdateRebootRequired
        );
    }

    const VM: &str = "dev-linux";

    /// What a test session is to do: the GPU half as the test asked for it,
    /// and no display payload at all -- every test here is about the order of
    /// the GPU messages, and the display tests build their own work.
    fn work<'a>(
        gpu_shares: Option<&'a GpuShareManifest>,
        gpu: GuestGpuSink<'a>,
    ) -> SessionWork<'a> {
        SessionWork {
            gpu_shares,
            display_share: None,
            display_mode: None,
            gpu,
            display: &|_| {},
            updates: None,
        }
    }

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

        /// Whether the host ever sent a request of this shape.
        ///
        /// The negative is what the display tests need: a session that must
        /// not send something has no request to look up by id.
        fn was_asked(&self, matches: impl Fn(&request::Kind) -> bool) -> bool {
            self.received.iter().any(|envelope| {
                matches!(&envelope.body, Some(envelope::Body::Request(request))
                    if request.kind.as_ref().is_some_and(&matches))
            })
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

    struct IdleBeforeRead<S> {
        inner: S,
        idle_at: usize,
        reads: usize,
        idled: bool,
    }

    impl<S: Read> Read for IdleBeforeRead<S> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.reads == self.idle_at && !self.idled {
                self.idled = true;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "delayed guest"));
            }
            self.reads += 1;
            self.inner.read(buffer)
        }
    }

    impl<S: Write> Write for IdleBeforeRead<S> {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.inner.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
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

    struct IdleForever;

    impl Read for IdleForever {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "idle guest"))
        }
    }

    impl Write for IdleForever {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct ProbeGuest {
        outbox: Vec<u8>,
        read: usize,
        received: Vec<Envelope>,
        close: bool,
        close_after_update: bool,
        update_sent: Arc<AtomicBool>,
    }

    impl Read for ProbeGuest {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = &self.outbox[self.read..];
            if !available.is_empty() {
                let taken = available.len().min(buffer.len());
                buffer[..taken].copy_from_slice(&available[..taken]);
                self.read += taken;
                return Ok(taken);
            }
            if self.close {
                Ok(0)
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "idle guest"))
            }
        }
    }

    impl Write for ProbeGuest {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let envelope = frame::decode(&buffer[LENGTH_PREFIX_LEN..])
                .expect("the host writes one complete frame");
            match &envelope.body {
                Some(envelope::Body::Request(request))
                    if matches!(request.kind, Some(request::Kind::Heartbeat(_))) =>
                {
                    let answer = Envelope::response(
                        envelope.request_id,
                        response::Kind::Heartbeat(HeartbeatResponse {}),
                    );
                    let mut encoded = Vec::new();
                    frame::encode(&answer, &mut encoded).expect("a heartbeat response fits");
                    self.outbox.extend_from_slice(&encoded);
                }
                Some(envelope::Body::Request(request))
                    if matches!(request.kind, Some(request::Kind::UpdateDisplayPayload(_))) =>
                {
                    self.update_sent.store(true, Ordering::Relaxed);
                    self.close = self.close_after_update;
                }
                _ => {}
            }
            self.received.push(envelope);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A stamp of the shape a real agent sends: version plus the revision it
    /// was built from, which is the part that tells two builds apart.
    const AGENT_BUILD: &str = "0.1.0+e02e08e129b8";

    fn hello(version: ProtocolVersion, capabilities: &[Capability]) -> Envelope {
        Envelope::request(
            7,
            request::Kind::Hello(HelloRequest {
                version: Some(version),
                capabilities: capabilities.iter().copied().map(i32::from).collect(),
                agent_version: AGENT_BUILD.to_owned(),
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
        assert_eq!(
            session.build, AGENT_BUILD,
            "the build the guest named is what the session log reports, and it \
             is the only way to tell which agent a VM is running"
        );
        let Some(envelope::Body::Response(ref response)) = guest.answer_to(7).body else {
            panic!("the hello should have been answered");
        };
        assert!(matches!(response.kind, Some(response::Kind::Hello(_))));
    }

    #[test]
    fn an_idle_boundary_before_hello_does_not_reject_the_guest() {
        let secret = Secret::generate();
        let guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"));
        let mut guest = IdleBeforeRead {
            inner: guest,
            idle_at: 0,
            reads: 0,
            idled: false,
        };

        open(&mut guest, &secret, VM).expect("a delayed hello stays inside the opening budget");
    }

    #[test]
    fn an_idle_boundary_before_authentication_does_not_reject_the_guest() {
        let secret = Secret::generate();
        let guest = Guest::new(Secret::from_base64(&secret.to_base64()).expect("the secret"));
        let mut guest = IdleBeforeRead {
            inner: guest,
            // Reading the hello takes its prefix and body; pause before the
            // authentication response starts.
            idle_at: 2,
            reads: 0,
            idled: false,
        };

        open(&mut guest, &secret, VM)
            .expect("a delayed authentication stays inside the opening budget");
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

        serve(&mut guest, &session, work(None, &|_| {}), VM).expect("a session the agent closed");

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

        serve(&mut guest, &session, work(None, &|_| {}), VM).expect("a session the agent closed");

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

        serve(&mut guest, &session, work(None, &|_| {}), VM)
            .expect("a clean close is not a failure");
    }

    /// The work a display test does: no GPU, one display share, a stored mode
    /// or none, and a sink that keeps what came back.
    fn display_work<'a>(
        share: Option<&'a DisplayShare>,
        mode: Option<vmlord_core::DisplayMode>,
        display: GuestDisplaySink<'a>,
    ) -> SessionWork<'a> {
        SessionWork {
            gpu_shares: None,
            display_share: share,
            display_mode: mode,
            gpu: &|_| {},
            display,
            updates: None,
        }
    }

    fn display_share() -> DisplayShare {
        DisplayShare {
            name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
        }
    }

    #[test]
    fn a_desktop_vm_is_offered_its_display_payload_and_asked_to_apply_it() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
                    mount_point: "/opt/vmlord/display-payload".to_owned(),
                    state: i32::from(DisplayMountState::Mounted),
                    message: "mounted".to_owned(),
                }),
            }),
        ));
        guest.say(&Envelope::response(
            super::DISPLAY_APPLY_REQUEST_ID,
            response::Kind::ApplyDisplayRecipe(ApplyDisplayRecipeResponse {
                stages: vec![DisplayRecipeStage {
                    step: i32::from(vmlord_agent_protocol::v1::DisplayRecipeStep::Device),
                    state: i32::from(DisplayRecipeStageState::Ok),
                    message: "a vmlord_drm display device is present".to_owned(),
                }],
                versions: Some(vmlord_agent_protocol::v1::DisplayPayloadVersions {
                    installed: "0.1.0".to_owned(),
                    previous: String::new(),
                    loaded: "0.1.0".to_owned(),
                }),
                signing_certificate: None,
                desktop: None,
            }),
        ));

        let reports = Mutex::new(Vec::new());
        let share = display_share();
        serve(
            &mut guest,
            &session,
            display_work(Some(&share), None, &|report| {
                reports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(report);
            }),
            VM,
        )
        .expect("a session the agent closed");

        let offered = guest.answer_to(super::DISPLAY_ATTACH_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = offered.body else {
            panic!("the share should have been sent as a request");
        };
        let Some(request::Kind::AttachDisplayPayload(ref attach)) = request.kind else {
            panic!("the share should have been an attach request");
        };
        assert_eq!(
            attach.share.as_ref().expect("a share").name,
            "vmlord.display.payload"
        );

        let reports = reports
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].installed.as_deref(), Some("0.1.0"));
        assert_eq!(reports[0].loaded.as_deref(), Some("0.1.0"));
        assert_eq!(
            reports[0].previous, None,
            "an empty string on the wire is not a version"
        );
        assert_eq!(reports[0].failure, None);
    }

    #[test]
    fn a_vm_with_a_stored_mode_asks_the_guest_for_it() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
                    mount_point: "/opt/vmlord/display-payload".to_owned(),
                    state: i32::from(DisplayMountState::Mounted),
                    message: "mounted".to_owned(),
                }),
            }),
        ));

        let share = display_share();
        let mode = vmlord_core::DisplayMode::new(2560, 1440);
        let _ = serve(
            &mut guest,
            &session,
            display_work(Some(&share), mode, &|_| {}),
            VM,
        );

        let asked = guest.answer_to(super::DISPLAY_APPLY_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the recipe should have been sent as a request");
        };
        let Some(request::Kind::ApplyDisplayRecipe(ref apply)) = request.kind else {
            panic!("the recipe should have been an apply request");
        };
        let sent = apply.initial_mode.as_ref().expect("the stored mode");
        assert_eq!((sent.width, sent.height), (2560, 1440));
    }

    #[test]
    fn a_vm_with_no_stored_mode_asks_the_guest_for_nothing() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
                    mount_point: "/opt/vmlord/display-payload".to_owned(),
                    state: i32::from(DisplayMountState::Mounted),
                    message: "mounted".to_owned(),
                }),
            }),
        ));

        let share = display_share();
        let _ = serve(
            &mut guest,
            &session,
            display_work(Some(&share), None, &|_| {}),
            VM,
        );

        let asked = guest.answer_to(super::DISPLAY_APPLY_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the recipe should have been sent as a request");
        };
        let Some(request::Kind::ApplyDisplayRecipe(ref apply)) = request.kind else {
            panic!("the recipe should have been an apply request");
        };
        assert_eq!(
            apply.initial_mode, None,
            "absence is what the guest answers with its own fallback"
        );
    }

    #[test]
    fn a_headless_vm_is_asked_nothing_about_a_display() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, display_work(None, None, &|_| {}), VM)
            .expect("a session the agent closed");

        assert!(
            !guest.was_asked(|kind| matches!(kind, request::Kind::AttachDisplayPayload(_))),
            "a VM with no display payload is a session that says nothing about one"
        );
    }

    #[test]
    fn an_agent_without_the_display_capability_is_sent_no_display_requests() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let share = display_share();

        serve(
            &mut guest,
            &session,
            display_work(Some(&share), None, &|_| {}),
            VM,
        )
        .expect("a session the agent closed");

        assert!(
            !guest.was_asked(|kind| matches!(kind, request::Kind::AttachDisplayPayload(_))),
            "an agent that never agreed to the capability must not be sent its messages"
        );
    }

    #[test]
    fn a_display_recipe_that_failed_is_reported_with_the_cause_of_the_stage() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.to_owned(),
                    mount_point: "/opt/vmlord/display-payload".to_owned(),
                    state: i32::from(DisplayMountState::AlreadyMounted),
                    message: "already mounted".to_owned(),
                }),
            }),
        ));
        guest.say(&Envelope::response(
            super::DISPLAY_APPLY_REQUEST_ID,
            response::Kind::ApplyDisplayRecipe(ApplyDisplayRecipeResponse {
                stages: vec![DisplayRecipeStage {
                    step: i32::from(vmlord_agent_protocol::v1::DisplayRecipeStep::ModuleBuild),
                    state: i32::from(DisplayRecipeStageState::Failed),
                    message: "dkms build failed for kernel 6.8.0-137-generic".to_owned(),
                }],
                versions: None,
                signing_certificate: None,
                desktop: None,
            }),
        ));

        let reports = Mutex::new(Vec::new());
        let share = display_share();
        serve(
            &mut guest,
            &session,
            display_work(Some(&share), None, &|report| {
                reports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(report);
            }),
            VM,
        )
        .expect("a display that will not build ends nothing");

        let reports = reports
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let failure = reports[0]
            .failure
            .as_ref()
            .expect("a failed stage is a failure");
        assert_eq!(failure.code, DisplayStatusCode::PayloadBuildFailed);
        assert_eq!(failure.stage, DisplayStage::Payload);
        assert!(failure.message.contains("6.8.0-137-generic"));
    }

    #[test]
    fn a_share_the_guest_refused_is_not_followed_by_a_recipe() {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(
                ProtocolVersion::current(),
                &[Capability::Gpu, Capability::Display],
            ),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        guest.say(&Envelope::response(
            super::DISPLAY_ATTACH_REQUEST_ID,
            response::Kind::AttachDisplayPayload(AttachDisplayPayloadResponse {
                mount: Some(vmlord_agent_protocol::v1::DisplayMount {
                    name: "vmlord.display.payload".to_owned(),
                    mount_point: String::new(),
                    state: i32::from(DisplayMountState::Failed),
                    message: "9p mount failed".to_owned(),
                }),
            }),
        ));

        let reports = Mutex::new(Vec::new());
        let share = display_share();
        serve(
            &mut guest,
            &session,
            display_work(Some(&share), None, &|report| {
                reports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(report);
            }),
            VM,
        )
        .expect("a session the agent closed");

        assert!(
            !guest.was_asked(|kind| matches!(kind, request::Kind::ApplyDisplayRecipe(_))),
            "there is nothing to build out of a payload that was never mounted"
        );
        let reports = reports
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            reports[0]
                .failure
                .as_ref()
                .expect("a mount that failed is a failure")
                .code,
            DisplayStatusCode::PayloadInvalid
        );
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

        serve(&mut guest, &session, work(Some(&manifest), &|_| {}), VM)
            .expect("a session the agent closed");

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

        serve(&mut guest, &session, work(Some(&manifest), &|_| {}), VM)
            .expect("a session the agent closed");

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

    /// A guest that mounted its shares, applied its recipe and answered the
    /// probe with `verdict`, and what the session made of it.
    ///
    /// The whole conversation rather than the probe alone: the probe is asked
    /// for only after a recipe that finished, so a fixture that skipped the
    /// earlier answers would test a request the host never sends.
    fn reports_of_a_guest(
        recipe: ApplyGpuRecipeResponse,
        probe: Option<ProbeGpuResponse>,
    ) -> Vec<GuestGpuReport> {
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![vmlord_core::GpuShare::payload()],
        };
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
            response::Kind::ApplyGpuRecipe(recipe),
        ));
        if let Some(probe) = probe {
            guest.say(&Envelope::response(
                super::PROBE_REQUEST_ID,
                response::Kind::ProbeGpu(probe),
            ));
        }

        let reports = Mutex::new(Vec::new());
        serve(
            &mut guest,
            &session,
            work(Some(&manifest), &|report| {
                reports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(report);
            }),
            VM,
        )
        .expect("a session the agent closed");

        reports
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn a_recipe_that_finished() -> ApplyGpuRecipeResponse {
        ApplyGpuRecipeResponse {
            stages: vec![GpuRecipeStage {
                step: i32::from(GpuRecipeStep::Device),
                state: i32::from(GpuRecipeStageState::Ok),
                message: "/dev/dxg is a usable device".to_owned(),
            }],
        }
    }

    fn a_probe_with(verdict: GpuProbeVerdict, driver: &str, render_node: &str) -> ProbeGpuResponse {
        ProbeGpuResponse {
            verdict: i32::from(verdict),
            checks: Vec::new(),
            renderer: String::new(),
            driver: driver.to_owned(),
            render_node: render_node.to_owned(),
        }
    }

    #[test]
    fn a_guest_that_renders_is_reported_as_ready() {
        let reports = reports_of_a_guest(
            a_recipe_that_finished(),
            Some(a_probe_with(
                GpuProbeVerdict::Renders,
                "dxgkrnl",
                "/dev/dri/renderD128",
            )),
        );

        assert_eq!(
            reports,
            vec![GuestGpuReport::Ready(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            })]
        );
    }

    #[test]
    fn a_guest_that_only_opened_the_device_is_present_rather_than_ready() {
        let reports = reports_of_a_guest(
            a_recipe_that_finished(),
            Some(a_probe_with(GpuProbeVerdict::DeviceOnly, "dxgkrnl", "")),
        );

        let [GuestGpuReport::DevicePresent(detail)] = &reports[..] else {
            panic!("a device that renders nothing is not a ready GPU: {reports:?}");
        };
        assert_eq!(detail.driver.as_deref(), Some("dxgkrnl"));
        assert_eq!(
            detail.render_node, None,
            "an empty field on the wire is an absent fact, not an empty name"
        );
    }

    #[test]
    fn a_guest_without_a_device_has_failed() {
        let reports = reports_of_a_guest(
            a_recipe_that_finished(),
            Some(a_probe_with(GpuProbeVerdict::NoDevice, "", "")),
        );

        let [GuestGpuReport::Failed(failure)] = &reports[..] else {
            panic!("a guest with no device has no GPU: {reports:?}");
        };
        assert_eq!(failure.code, GpuStatusCode::GuestFailed);
    }

    #[test]
    fn a_recipe_that_broke_reports_the_stage_it_broke_at_and_is_never_probed() {
        let reports = reports_of_a_guest(
            ApplyGpuRecipeResponse {
                stages: vec![
                    GpuRecipeStage {
                        step: i32::from(GpuRecipeStep::Payload),
                        state: i32::from(GpuRecipeStageState::Ok),
                        message: "the payload applies to this guest".to_owned(),
                    },
                    GpuRecipeStage {
                        step: i32::from(GpuRecipeStep::ModuleBuild),
                        state: i32::from(GpuRecipeStageState::Failed),
                        message: "dkms build returned 1".to_owned(),
                    },
                ],
            },
            None,
        );

        let [GuestGpuReport::Failed(failure)] = &reports[..] else {
            panic!("a recipe that did not finish is a failure: {reports:?}");
        };
        assert!(
            failure.message.contains("dkms build returned 1"),
            "the guest's own words carry the detail: {}",
            failure.message
        );
    }

    #[test]
    fn a_session_probes_once_the_recipe_has_answered() {
        // The probe follows the recipe and never precedes it: it asks about a
        // userspace the recipe has just installed.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![vmlord_core::GpuShare::payload()],
        };
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
        guest.say(&Envelope::response(
            super::PROBE_REQUEST_ID,
            response::Kind::ProbeGpu(ProbeGpuResponse {
                verdict: i32::from(GpuProbeVerdict::Renders),
                checks: vec![GpuProbeCheck {
                    step: i32::from(GpuProbeStep::Opengl),
                    state: i32::from(GpuProbeCheckState::Ok),
                    message: "GL renders on D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                }],
                renderer: "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                driver: "dxgkrnl".to_owned(),
                render_node: String::new(),
            }),
        ));

        serve(&mut guest, &session, work(Some(&manifest), &|_| {}), VM)
            .expect("a session the agent closed");

        let asked = guest.answer_to(super::PROBE_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the probe should have been asked for as a request");
        };
        assert!(matches!(request.kind, Some(request::Kind::ProbeGpu(_))));
        assert_eq!(
            guest
                .received
                .iter()
                .filter(|envelope| matches!(
                    envelope.body,
                    Some(envelope::Body::Request(ref request))
                        if matches!(request.kind, Some(request::Kind::ProbeGpu(_)))
                ))
                .count(),
            1,
            "one probe per session"
        );
    }

    #[test]
    fn a_session_that_never_applied_a_recipe_never_probes() {
        // A guest with no shares has no payload, no recipe and nothing to
        // render with; asking it to probe would install two packages on a VM
        // that was never given a GPU.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, work(None, &|_| {}), VM).expect("a session the agent closed");

        assert!(
            !guest.received.iter().any(|envelope| matches!(
                envelope.body,
                Some(envelope::Body::Request(ref request))
                    if matches!(request.kind, Some(request::Kind::ProbeGpu(_)))
            )),
            "a VM with no manifest is asked for no probe"
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

        serve(&mut guest, &session, work(None, &|_| {}), VM).expect("a session the agent closed");

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

        serve(&mut guest, &session, work(Some(&manifest), &|_| {}), VM)
            .expect("a session the agent closed");

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
            build: String::new(),
        };

        serve(&mut stream, &session, work(None, &|_| {}), VM)
            .expect("an idle boundary is not a failed session");
    }

    #[test]
    fn a_guest_that_answers_no_liveness_probe_ends_its_session() {
        let mut stream = IdleForever;
        let session = AgentSession {
            version: ProtocolVersion::current(),
            capabilities: Vec::new(),
            build: String::new(),
        };

        let error = serve_with_timing(
            &mut stream,
            &session,
            work(None, &|_| {}),
            VM,
            SessionTiming::IMMEDIATE,
            None,
        )
        .expect_err("a peer that never answers a liveness probe is gone");

        assert!(matches!(error, SessionError::Unresponsive));
    }

    #[test]
    fn an_authenticated_replacement_ends_the_old_session() {
        let mut stream = IdleForever;
        let session = AgentSession {
            version: ProtocolVersion::current(),
            capabilities: Vec::new(),
            build: String::new(),
        };

        let exit =
            serve_with_replacement(&mut stream, &session, work(None, &|_| {}), VM, &mut || true)
                .expect("a replacement is an orderly session transition");

        assert_eq!(exit, SessionExit::Replaced);
    }

    #[test]
    fn an_update_is_sent_only_after_the_session_proves_it_is_alive() {
        let mut stream = ProbeGuest {
            close_after_update: true,
            ..ProbeGuest::default()
        };
        let session = AgentSession {
            version: ProtocolVersion::current(),
            capabilities: vec![Capability::Display],
            build: String::new(),
        };
        let (updates, pending_updates) = mpsc::channel();
        let (answer, answered) = mpsc::channel();
        updates
            .send(DisplayUpdate {
                target_version: "0.2.0".to_owned(),
                answer,
            })
            .expect("the session queue is open");
        let work = SessionWork {
            updates: Some(&pending_updates),
            ..work(None, &|_| {})
        };

        serve(&mut stream, &session, work, VM).expect("the guest closes after the update");

        assert!(matches!(
            stream.received.as_slice(),
            [
                Envelope {
                    body: Some(envelope::Body::Request(
                        vmlord_agent_protocol::v1::Request {
                            kind: Some(request::Kind::Heartbeat(_)),
                        }
                    )),
                    ..
                },
                Envelope {
                    body: Some(envelope::Body::Request(
                        vmlord_agent_protocol::v1::Request {
                            kind: Some(request::Kind::UpdateDisplayPayload(_)),
                        }
                    )),
                    ..
                }
            ]
        ));
        assert!(matches!(answered.recv(), Err(mpsc::RecvError)));
    }

    #[test]
    fn a_replacement_ends_a_session_with_an_update_in_flight() {
        let mut stream = ProbeGuest::default();
        let update_sent = Arc::clone(&stream.update_sent);
        let session = AgentSession {
            version: ProtocolVersion::current(),
            capabilities: vec![Capability::Display],
            build: String::new(),
        };
        let (updates, pending_updates) = mpsc::channel();
        let (answer, answered) = mpsc::channel();
        updates
            .send(DisplayUpdate {
                target_version: "0.2.0".to_owned(),
                answer,
            })
            .expect("the session queue is open");
        let work = SessionWork {
            updates: Some(&pending_updates),
            ..work(None, &|_| {})
        };

        let exit = serve_with_replacement(&mut stream, &session, work, VM, &mut || {
            update_sent.load(Ordering::Relaxed)
        })
        .expect("the replacement preempts the in-flight update");

        assert_eq!(exit, SessionExit::Replaced);
        assert!(matches!(answered.recv(), Err(mpsc::RecvError)));
    }

    #[test]
    fn a_signature_the_kernel_refused_is_not_the_failure_a_broken_build_is() {
        let report = ApplyDisplayRecipeResponse {
            stages: vec![DisplayRecipeStage {
                step: i32::from(DisplayRecipeStep::ModuleLoad),
                state: i32::from(DisplayRecipeStageState::Failed),
                message: "modprobe vmlord_drm exited with 1: modprobe: ERROR: could not \
                          insert 'vmlord_drm': Key was rejected by service -- Secure Boot \
                          is on and enroll /var/lib/shim-signed/mok/MOK.der (key id 0a1b) \
                          as a MOK"
                    .to_owned(),
            }],
            versions: None,
            signing_certificate: None,
            desktop: None,
        };
        let seen = Mutex::new(None);

        report_display_recipe(&report, "dev", &|report| {
            *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
        });

        let seen = seen
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let failure = seen
            .expect("a failed recipe reports")
            .failure
            .expect("a failed stage is a failure");
        assert_eq!(
            failure.code,
            DisplayStatusCode::PayloadModuleSignatureRejected
        );
    }

    #[test]
    fn a_signature_nobody_checks_yet_does_not_take_the_display_down() {
        for step in [
            DisplayRecipeStep::SigningKey,
            DisplayRecipeStep::ModuleSignature,
        ] {
            let report = ApplyDisplayRecipeResponse {
                stages: vec![
                    DisplayRecipeStage {
                        step: i32::from(step),
                        state: i32::from(DisplayRecipeStageState::Failed),
                        message: "vmlord_drm carries no signature".to_owned(),
                    },
                    stage(
                        DisplayRecipeStep::ServicesStart,
                        DisplayRecipeStageState::Ok,
                    ),
                ],
                versions: None,
                signing_certificate: None,
                desktop: None,
            };
            let seen = Mutex::new(None);

            report_display_recipe(&report, "dev", &|report| {
                *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
            });

            let seen = seen
                .into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .expect("a recipe reports");
            assert!(
                seen.failure.is_none(),
                "{step:?} must not degrade a display whose signature nothing checks"
            );
        }
    }

    #[test]
    fn the_certificate_the_guest_signs_with_reaches_the_host() {
        let report = ApplyDisplayRecipeResponse {
            stages: vec![stage(
                DisplayRecipeStep::ServicesStart,
                DisplayRecipeStageState::Ok,
            )],
            versions: None,
            signing_certificate: Some(DisplaySigningCertificate {
                certificate: vec![0x30, 0x82, 0x01],
                sha256: "ab".repeat(32),
                subject_key_identifier: "0a1b".to_owned(),
            }),
            desktop: None,
        };
        let seen = Mutex::new(None);

        report_display_recipe(&report, "dev", &|report| {
            *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
        });

        let seen = seen
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .expect("a recipe reports");
        assert_eq!(seen.signing_certificate, Some(vec![0x30, 0x82, 0x01]));
    }
}
