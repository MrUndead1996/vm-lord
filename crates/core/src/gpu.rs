//! What a VM asks of GPU-PV, and what GPU-PV is actually doing for it.
//!
//! The two are deliberately different types. [`GpuMode`] is desired state: it
//! is what the user chose, it is stored with the VM, and it does not change
//! because a start failed. Everything else here is runtime state: facts a
//! backend observed ([`VmGpuFacts`]) and the reading the application layer
//! derives from them ([`VmGpuStatus`]). A single field could only ever answer
//! one of "what was asked for" and "what happened", and the UI needs both.

use std::{path::PathBuf, time::SystemTime};

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
    /// This VMLord process did not start this VM, so what is attached to it
    /// was never observed.
    ///
    /// Its own variant rather than the absence of a fact: "nothing has been
    /// attached yet" and "something was attached and nobody here saw it" are
    /// different sentences to a reader, and only the second one is true of a
    /// VM reclaimed from a previous run.
    Unknown,
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
    /// The VM was started before this VMLord process, so what is attached to
    /// it is not known.
    AssignmentUnknown,
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
    /// The host has no GPU partition adapter.
    HostNoAdapter,
    /// The Host Compute Service is not answering, so nothing can be assigned
    /// to anything.
    HostServiceUnavailable,
    /// The host has adapters, but no driver package could be located for any
    /// of them.
    HostDriverStoreMissing,
    /// The Linux userspace a guest needs is not staged on this host.
    HostLinuxPayloadMissing,
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
            Self::AssignmentUnknown => "gpu-assignment-unknown",
            Self::GuestPending => "gpu-guest-pending",
            Self::AssignmentFailed => "gpu-assignment-failed",
            Self::AssignmentPartial => "gpu-assignment-partial",
            Self::GuestDevicePresent => "gpu-guest-device-present",
            Self::GuestReady => "gpu-guest-ready",
            Self::GuestFailed => "gpu-guest-failed",
            Self::HostNoAdapter => "gpu-host-no-adapter",
            Self::HostServiceUnavailable => "gpu-host-service-unavailable",
            Self::HostDriverStoreMissing => "gpu-host-driver-store-missing",
            Self::HostLinuxPayloadMissing => "gpu-host-linux-payload-missing",
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

/// What this host can do for GPU-PV, as far as anything can be told without
/// starting a VM.
///
/// Two axes rather than one verdict: a host with a partition adapter but no
/// WSL payload can assign a GPU that a Linux guest will not be able to render
/// on, which is a warning and not a refusal, and a single field could say only
/// one of the two things.
///
/// An adapter, a resolved driver package and a live Host Compute Service are a
/// precondition, never a guarantee: assignment is proven by assigning, which
/// needs a running compute system, and this type is read before there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostGpuCapabilities {
    /// Whether a GPU partition can be offered to a VM at all.
    pub assignment: GpuAvailability,
    /// Whether the Linux userspace a guest needs is staged on the host.
    pub linux_payload: GpuAvailability,
    /// The adapters behind the verdict, as the host reports them.
    ///
    /// Facts from device enumeration and nothing else -- no share names, no
    /// guest paths. What is exported to a guest is decided where the export is
    /// built, not here.
    pub adapters: Vec<HostGpuAdapter>,
}

/// One axis of [`HostGpuCapabilities`]: usable, or not and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuAvailability {
    Available,
    Unavailable(GpuFailure),
}

impl GpuAvailability {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Why this axis is unavailable, when it is.
    #[must_use]
    pub const fn failure(&self) -> Option<&GpuFailure> {
        match self {
            Self::Available => None,
            Self::Unavailable(failure) => Some(failure),
        }
    }
}

/// A GPU partition adapter the host presents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostGpuAdapter {
    /// What Windows calls the device.
    pub name: String,
    /// The device instance id, which identifies this adapter across reboots.
    pub instance_id: String,
    /// The device interface path a compute system would name.
    pub interface_path: String,
    /// The driver package directory in the DriverStore, when it resolved.
    ///
    /// `None` is an adapter whose INF could not be located: still a real
    /// adapter, but one with nothing to hand a guest.
    pub driver_store: Option<PathBuf>,
    /// The kernel service driving the adapter, for diagnostics.
    pub service: Option<String>,
}

/// The Plan9 shares a guest is offered, as the guest is told about them.
///
/// Roles, never host paths. Where a share is mounted is the guest's decision,
/// taken from its own allowlist: a host path would be useless to it and would
/// put the host's topology on the wire and into guest logs. Not serializable
/// -- it has no on-disk format, and the task that starts sending it converts
/// it to protobuf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuShareManifest {
    pub shares: Vec<GpuShare>,
}

/// One share: what it is called on the wire, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuShare {
    /// The share's name, which is also `aname=` when the guest mounts it.
    pub name: String,
    pub role: GpuShareRole,
}

/// What a guest is meant to make of a share.
///
/// The guest never parses the name to find this out: an agent that derived
/// meaning from a string would have to be updated in step with whatever
/// produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuShareRole {
    /// One exact, per-VM prepared GPU payload generation directory.
    GpuPayload,
    /// The host's WSL Linux userspace.
    WslLib,
    /// One driver package, named by its DriverStore folder.
    DriverPackage { package: String },
    /// The Microsoft D3D12 userspace from the WSL package.
    WslD3d12,
}

/// The share name the host's WSL Linux userspace is offered under.
pub const WSL_LIB_SHARE: &str = "vmlord.gpu.wsl-lib";
/// The share name for the immutable prepared GPU payload staging directory.
pub const GPU_PAYLOAD_SHARE: &str = "vmlord.gpu.payload";
/// The share name the Microsoft D3D12 userspace is offered under.
pub const WSL_D3D12_SHARE: &str = "vmlord.gpu.wsl-d3d12";

/// What every driver package share's name starts with.
const DRIVER_PACKAGE_SHARE_PREFIX: &str = "vmlord.gpu.drv.";

/// The longest package folder name that may become part of a share name.
///
/// A share name travels in a comma-separated `mount` option string, so it is
/// bounded for the guest's sake rather than by anything Windows imposes.
const MAX_PACKAGE_NAME: usize = 96;

impl GpuShare {
    /// The per-VM immutable GPU payload generation share.
    #[must_use]
    pub fn payload() -> Self {
        Self { name: GPU_PAYLOAD_SHARE.to_owned(), role: GpuShareRole::GpuPayload }
    }
    /// The share for the host's WSL Linux userspace.
    #[must_use]
    pub fn wsl_lib() -> Self {
        Self {
            name: WSL_LIB_SHARE.to_owned(),
            role: GpuShareRole::WslLib,
        }
    }

    /// The share for the Microsoft D3D12 userspace.
    ///
    /// Separate from [`Self::wsl_lib`] because the two are separate
    /// directories on every host that installs WSL from the Store: one holds
    /// the vendor's libraries and the other the Microsoft ones, and a guest
    /// needs both to render.
    #[must_use]
    pub fn wsl_d3d12() -> Self {
        Self {
            name: WSL_D3D12_SHARE.to_owned(),
            role: GpuShareRole::WslD3d12,
        }
    }

    /// The share for one driver package, or `None` when the folder's name
    /// cannot safely become a share name.
    ///
    /// The name ends up in an HCS JSON document and in a `mount` option
    /// string, so a separator, a quote, a space or a traversal component in it
    /// would be read by something downstream as structure rather than as text.
    #[must_use]
    pub fn driver_package(package: &str) -> Option<Self> {
        if package.is_empty() || package.len() > MAX_PACKAGE_NAME {
            return None;
        }
        if package == "." || package == ".." {
            return None;
        }
        if !package.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        }) {
            return None;
        }

        Some(Self {
            name: format!("{DRIVER_PACKAGE_SHARE_PREFIX}{package}"),
            role: GpuShareRole::DriverPackage {
                package: package.to_owned(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GpuAvailability, GpuFailure, GpuShare, GpuShareRole, GpuStatusCode, HostGpuCapabilities,
        WSL_D3D12_SHARE, WSL_LIB_SHARE,
    };

    #[test]
    fn host_status_codes_have_stable_strings() {
        assert_eq!(GpuStatusCode::HostNoAdapter.as_str(), "gpu-host-no-adapter");
        assert_eq!(
            GpuStatusCode::HostServiceUnavailable.as_str(),
            "gpu-host-service-unavailable"
        );
        assert_eq!(
            GpuStatusCode::HostDriverStoreMissing.as_str(),
            "gpu-host-driver-store-missing"
        );
        assert_eq!(
            GpuStatusCode::HostLinuxPayloadMissing.as_str(),
            "gpu-host-linux-payload-missing"
        );
    }

    #[test]
    fn an_unavailable_axis_keeps_the_reason_readable() {
        let capabilities = HostGpuCapabilities {
            assignment: GpuAvailability::Available,
            linux_payload: GpuAvailability::Unavailable(GpuFailure::new(
                GpuStatusCode::HostLinuxPayloadMissing,
                "no WSL payload",
            )),
            adapters: Vec::new(),
        };

        assert!(capabilities.assignment.is_available());
        assert!(!capabilities.linux_payload.is_available());
        assert_eq!(
            capabilities
                .linux_payload
                .failure()
                .map(|failure| failure.code),
            Some(GpuStatusCode::HostLinuxPayloadMissing)
        );
        assert!(capabilities.assignment.failure().is_none());
    }

    #[test]
    fn the_wsl_lib_share_is_named_once_and_carries_its_role() {
        let share = GpuShare::wsl_lib();

        assert_eq!(share.name, WSL_LIB_SHARE);
        assert_eq!(share.role, GpuShareRole::WslLib);
    }

    #[test]
    fn the_d3d12_share_is_its_own_role_under_its_own_name() {
        let share = GpuShare::wsl_d3d12();

        assert_eq!(share.role, GpuShareRole::WslD3d12);
        assert_eq!(share.name, WSL_D3D12_SHARE);
        assert_ne!(
            share.name,
            GpuShare::wsl_lib().name,
            "two sources of one userspace are two shares, and a guest tells them \
             apart by name"
        );
    }

    #[test]
    fn a_driver_package_share_is_named_after_its_folder() {
        let share = GpuShare::driver_package("nvltsi.inf_amd64_5b0e0dc41b0dbf1e")
            .expect("an ordinary DriverStore folder name is admissible");

        assert_eq!(share.name, "vmlord.gpu.drv.nvltsi.inf_amd64_5b0e0dc41b0dbf1e");
        assert_eq!(
            share.role,
            GpuShareRole::DriverPackage {
                package: "nvltsi.inf_amd64_5b0e0dc41b0dbf1e".to_owned()
            }
        );
    }

    #[test]
    fn a_package_name_that_could_break_a_mount_option_is_refused() {
        // A share name becomes `aname=` in a comma-separated mount option
        // string and a JSON string in the HCS document; a separator or a
        // traversal component in it would be read by something downstream.
        for refused in [
            "",
            ".",
            "..",
            "pkg,other",
            "pkg other",
            r"pkg\other",
            "pkg/other",
            "pkg\"other",
            "пакет",
            &"a".repeat(97),
        ] {
            assert!(
                GpuShare::driver_package(refused).is_none(),
                "{refused:?} must not become a share name"
            );
        }
    }

    #[test]
    fn a_package_name_at_the_limit_is_still_admissible() {
        assert!(GpuShare::driver_package(&"a".repeat(96)).is_some());
    }
}
