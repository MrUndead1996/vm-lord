//! What a VM asks of GPU-PV, and what GPU-PV is actually doing for it.
//!
//! The two are deliberately different types. [`GpuMode`] is desired state: it
//! is what the user chose, it is stored with the VM, and it does not change
//! because a start failed. Everything else here is runtime state: facts a
//! backend observed ([`VmGpuFacts`]) and the reading the application layer
//! derives from them ([`VmGpuStatus`]). A single field could only ever answer
//! one of "what was asked for" and "what happened", and the UI needs both.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// What a VM asks of the host's GPU.
///
/// Desired state, chosen in the create or edit form. Serializable because the
/// mode a VM was created with outlives the process that applied it, and a start
/// has to know what to attach. The variant names are therefore an on-disk
/// format: renaming one changes what already-stored VMs read back as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuMode {
    /// No GPU is attached; the guest renders in software.
    #[default]
    None,
    /// The host's preferred adapter is attached.
    Default,
    /// Every GPU-PV capable adapter the host has is attached.
    Mirror,
    /// A mode read back from storage or from the legacy backend that this
    /// build does not know how to apply.
    Unknown(i32),
}

/// What a backend observed about a VM's GPU, without deciding what it means.
///
/// Facts only: each field is either something that was seen or `None` for "not
/// observed yet". Turning them into a state a person can read is
/// `vmlord_app`'s job, so that two backends observing the same thing cannot
/// disagree about what to call it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmGpuFacts {
    /// What the host side did the last time it tried to attach adapters.
    pub assignment: Option<GpuAssignment>,
    /// What the guest agent last reported about the GPU it was given.
    pub guest: Option<GuestGpuReport>,
    /// When the newest of the facts above was observed.
    ///
    /// `None` while there are none: a VM that has never been started has
    /// nothing to timestamp, and inventing a time would date an observation
    /// that was never made.
    pub observed_at: Option<SystemTime>,
}

/// What the host side of GPU-PV did for a VM, once it has done anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuAssignment {
    /// Everything the mode asked for was attached.
    Complete(NativeGpuDetail),
    /// Some of what the mode asked for was attached, and why the rest was not.
    ///
    /// Its own variant rather than a `Complete` with a warning beside it: GPU
    /// is applied best effort and never blocks a start, so "the VM runs with
    /// less GPU than it asked for" is an ordinary outcome that has to survive
    /// all the way to the UI.
    Partial {
        detail: NativeGpuDetail,
        reason: GpuFailure,
    },
    /// Nothing was attached, and why.
    Failed(GpuFailure),
}

/// What the guest agent last reported about the GPU it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestGpuReport {
    /// The guest kernel sees the device; nothing renders on it yet.
    DevicePresent(GuestGpuDetail),
    /// The guest renders on the GPU.
    Ready(GuestGpuDetail),
    /// The guest cannot use the device it was given, and why.
    Failed(GpuFailure),
}

/// What the host has to say about the adapters it attached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeGpuDetail {
    /// The adapter the mode picked, when there is a single one to name.
    pub adapter: Option<String>,
    /// How many adapters were attached.
    pub adapters: u32,
}

/// What the guest has to say about the GPU it was given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuestGpuDetail {
    /// What the guest kernel driver calls itself, when it says.
    pub driver: Option<String>,
    /// The render node the guest found, such as `/dev/dri/renderD128`.
    pub render_node: Option<String>,
}

/// Why a step of GPU-PV did not do what was asked of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuFailure {
    pub code: GpuStatusCode,
    pub message: String,
}

impl GpuFailure {
    #[must_use]
    pub fn new(code: GpuStatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// How well the GPU a VM asked for is working right now.
///
/// The coarse reading -- what colour the UI paints and what a person takes in
/// at a glance. [`GpuStage`] says where in the pipeline that reading comes
/// from and [`GpuStatusCode`] says exactly why, so this enum does not have to
/// grow a variant per reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuState {
    /// GPU-PV is not in play: the VM does not ask for a GPU, or it is not
    /// running so nothing is attached to it.
    #[default]
    Disabled,
    /// The GPU is on its way: the host side is attaching adapters or has
    /// finished, and the guest has not confirmed anything yet.
    WaitingForGuest,
    /// The guest sees the device, but nothing renders on it yet.
    Assigned,
    /// The guest renders on the GPU. This is the working state.
    GuestReady,
    /// The GPU works, but with less than the mode asked for.
    Degraded,
    /// The GPU does not work.
    Failed,
}

/// Which step of GPU-PV the status was read from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuStage {
    /// Nothing is under way.
    #[default]
    Idle,
    /// The host is choosing adapters, attaching them and exporting their
    /// drivers.
    Assignment,
    /// The guest is bringing the GPU up.
    Guest,
}

/// Exactly why a VM's GPU is in the state it is in.
///
/// Stable: these are what logs, tests and future automation match on, so a
/// variant's meaning does not change once it exists. The message beside a code
/// carries the host-specific detail and is free to be reworded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuStatusCode {
    /// The VM does not ask for a GPU.
    ModeDisabled,
    /// The VM asks for a GPU, but it is not running, so nothing is attached.
    VmNotRunning,
    /// The stored mode is not one this build knows how to apply.
    ModeUnsupported,
    /// The host side has not reported yet.
    AssignmentPending,
    /// The host attached the GPU; the guest agent has not reported yet.
    GuestPending,
    /// The host could not attach the GPU.
    AssignmentFailed,
    /// Fewer adapters were attached than the mode asked for.
    AssignmentPartial,
    /// The guest sees the device but nothing renders on it yet.
    GuestDevicePresent,
    /// The guest renders on the GPU.
    GuestReady,
    /// The guest cannot use the GPU it was given.
    GuestFailed,
}

impl GpuStatusCode {
    /// The code as it is written in logs and matched on from outside Rust.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModeDisabled => "gpu-mode-disabled",
            Self::VmNotRunning => "gpu-vm-not-running",
            Self::ModeUnsupported => "gpu-mode-unsupported",
            Self::AssignmentPending => "gpu-assignment-pending",
            Self::GuestPending => "gpu-guest-pending",
            Self::AssignmentFailed => "gpu-assignment-failed",
            Self::AssignmentPartial => "gpu-assignment-partial",
            Self::GuestDevicePresent => "gpu-guest-device-present",
            Self::GuestReady => "gpu-guest-ready",
            Self::GuestFailed => "gpu-guest-failed",
        }
    }
}

impl std::fmt::Display for GpuStatusCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What GPU-PV is doing for one VM, as the application layer reads it.
///
/// Derived from [`VmGpuFacts`], never stored: it describes a moment, and the
/// next refresh describes the next one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmGpuStatus {
    pub state: GpuState,
    pub stage: GpuStage,
    pub code: GpuStatusCode,
    /// What to show a person, with whatever host-specific detail there is.
    pub message: String,
    /// What the host attached, when it has attached anything.
    pub native: Option<NativeGpuDetail>,
    /// What the guest reported, when it has reported anything.
    pub guest: Option<GuestGpuDetail>,
    /// When the facts behind this status were observed; the time the status
    /// was derived when there are no facts yet.
    pub observed_at: SystemTime,
}

impl VmGpuStatus {
    /// Whether the GPU is doing anything for the guest right now.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.state,
            GpuState::Assigned | GpuState::GuestReady | GpuState::Degraded
        )
    }
}
