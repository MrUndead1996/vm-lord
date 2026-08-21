//! What a VM asks of its desktop, what provisioning that desktop made of it,
//! and what the guest is doing with it right now.
//!
//! Three separate things, deliberately three types. [`DesktopProfile`] is
//! desired state: it is chosen when the VM is created, stored with the VM, and
//! unchanged by whatever installing a desktop makes of it -- a GNOME VM whose
//! packages failed to download is still a GNOME VM. [`DisplayProvisioning`] is
//! the outcome of that installation, and it is stored too, because it outlives
//! the process that watched it: a VM whose desktop install failed must come
//! back after a restart still knowing that it failed, or nobody could be
//! offered a retry. Everything else here is runtime state: what the guest last
//! reported ([`VmDisplayFacts`]) and the reading the application layer derives
//! from that plus the two stored fields ([`VmDisplayStatus`]).
//!
//! Nothing in this module carries a credential. The guest password exists
//! while a VM is being created and is hashed into the cloud-init seed there;
//! a display session authenticates against the guest with its own per-session
//! secret (see `vmlord-display-protocol`), so no type here has a field a
//! plaintext password could be put in, and none is stored beside the profile.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// What a VM asks of its own desktop.
///
/// Desired state, chosen in the create form. Serializable because the profile
/// a VM was created with outlives the process that applied it: a start has to
/// know whether there is a desktop to connect to. The variant names are
/// therefore an on-disk format -- renaming one changes what already-stored VMs
/// read back as.
///
/// Two variants and no `Auto`: the MVP installs GNOME on GDM under Wayland or
/// installs nothing at all, and a third variant would be a promise no code
/// keeps. Changing a created VM from `Headless` to `Gnome` afterwards is its
/// own task (#127); until then the profile is a creation-time decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopProfile {
    /// No desktop is installed. The guest is reached over SSH and the serial
    /// console, and Connect has nothing to open.
    Headless,
    /// GNOME on GDM under Wayland, installed from the distribution's own apt
    /// repositories -- no third-party archive, no downloaded binary, nothing
    /// VMLord has to sign or update.
    #[default]
    Gnome,
}

impl DesktopProfile {
    /// The profile as it is written in logs and matched on from outside Rust.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::Gnome => "gnome",
        }
    }

    /// Whether this profile asks for a desktop to be installed at all.
    #[must_use]
    pub const fn wants_desktop(self) -> bool {
        matches!(self, Self::Gnome)
    }
}

impl std::fmt::Display for DesktopProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The fewest CPU cores a desktop guest is comfortable on.
pub const MIN_DESKTOP_CPU_CORES: u32 = 2;

/// The least memory a desktop guest is comfortable on, in MiB.
pub const MIN_DESKTOP_RAM_MB: u32 = 4096;

/// What to tell someone creating a desktop VM smaller than GNOME wants, or
/// `None` when there is nothing to say.
///
/// Advice and never a refusal: GNOME on one core boots, logs in and is
/// unpleasant, and "unpleasant" is a judgement its user is allowed to make.
/// Refusing here would also refuse the machine that is small on purpose --
/// a VM built to test exactly this -- so the create path calls this and shows
/// what comes back beside the form instead of rejecting it.
#[must_use]
pub fn desktop_resource_advice(
    profile: DesktopProfile,
    cpu_cores: u32,
    ram_mb: u32,
) -> Option<String> {
    if !profile.wants_desktop() {
        return None;
    }
    let short_cpu = cpu_cores < MIN_DESKTOP_CPU_CORES;
    let short_ram = ram_mb < MIN_DESKTOP_RAM_MB;
    let ram_gib = MIN_DESKTOP_RAM_MB / 1024;
    match (short_cpu, short_ram) {
        (false, false) => None,
        (true, false) => Some(format!(
            "A GNOME desktop is slow below {MIN_DESKTOP_CPU_CORES} CPU cores; this VM has \
             {cpu_cores}."
        )),
        (false, true) => Some(format!(
            "A GNOME desktop is slow below {ram_gib} GiB of RAM; this VM has {ram_mb} MiB."
        )),
        (true, true) => Some(format!(
            "A GNOME desktop is slow below {MIN_DESKTOP_CPU_CORES} CPU cores and {ram_gib} GiB of \
             RAM; this VM has {cpu_cores} cores and {ram_mb} MiB."
        )),
    }
}

/// How far the desktop a VM asked for got installed.
///
/// Stored with the VM, and the reason it is stored rather than derived: the
/// installation happens once, during the build, and its outcome has to be
/// readable by every later run of VMLord. A VM whose desktop packages could
/// not be downloaded is a working VM with a broken desktop -- it boots, SSH
/// answers, and the only thing that knows the desktop is missing is this
/// field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayProvisioning {
    /// The VM asks for no desktop, so none was installed.
    #[default]
    NotRequested,
    /// A desktop was asked for and installing it has not finished -- or was
    /// interrupted, which reads the same way and is why a retry exists.
    Pending,
    /// The desktop is installed.
    Ready,
    /// The desktop was asked for and is not installed, and why.
    ///
    /// Not `Failed`: the VM itself is fine. Naming the state after what was
    /// lost rather than after the VM keeps the UI from painting a usable
    /// machine as a broken one.
    Degraded(DisplayFailure),
}

impl DisplayProvisioning {
    /// What a VM's provisioning starts out as, given what it asked for.
    #[must_use]
    pub const fn requested(profile: DesktopProfile) -> Self {
        match profile {
            DesktopProfile::Headless => Self::NotRequested,
            DesktopProfile::Gnome => Self::Pending,
        }
    }

    /// Whether installing the desktop can be attempted again.
    ///
    /// Only a degraded provisioning with a retryable cause: a desktop that is
    /// installed has nothing to retry, and a cause the guest cannot get past
    /// on its own -- a release with no desktop packages -- would retry into
    /// the same failure forever.
    #[must_use]
    pub const fn can_retry(&self) -> bool {
        match self {
            Self::Degraded(failure) => failure.code.is_retryable(),
            Self::NotRequested | Self::Pending | Self::Ready => false,
        }
    }

    /// The provisioning a retry starts from, or `None` when there is nothing
    /// to retry.
    ///
    /// A retry moves back to `Pending` rather than clearing the failure to
    /// nothing: until the new attempt reports, what is known about the desktop
    /// is still that the last attempt did not finish.
    #[must_use]
    pub fn retried(&self) -> Option<Self> {
        self.can_retry().then_some(Self::Pending)
    }
}

/// Why a step of display provisioning did not do what was asked of it.
///
/// Structured rather than a sentence: the stage says where it happened, the
/// code says exactly what happened and is what logs and tests match on, and
/// the message carries the host- or guest-specific detail and is free to be
/// reworded. Stored inside [`DisplayProvisioning::Degraded`], so the stage and
/// the code are an on-disk format and the message is not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayFailure {
    pub stage: DisplayStage,
    pub code: DisplayStatusCode,
    pub message: String,
}

impl DisplayFailure {
    #[must_use]
    pub fn new(stage: DisplayStage, code: DisplayStatusCode, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            message: message.into(),
        }
    }
}

/// Which step of the display stack a status or a failure was read from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayStage {
    /// Nothing is under way.
    #[default]
    Idle,
    /// The desktop is being installed into the guest.
    Provisioning,
    /// The guest is bringing its display services up.
    Guest,
    /// The display payload is being verified, built or updated.
    ///
    /// Its own stage rather than part of `Provisioning`: installing a desktop
    /// and installing the module that desktop draws on fail differently, are
    /// fixed differently, and a person told only "provisioning" would go
    /// looking in the wrong half.
    Payload,
}

impl DisplayStage {
    /// The stage as it is written in logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Provisioning => "provisioning",
            Self::Guest => "guest",
            Self::Payload => "payload",
        }
    }
}

impl std::fmt::Display for DisplayStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Exactly why a VM's display is in the state it is in.
///
/// Stable: these are what logs, tests and the troubleshooting documentation
/// are indexed by, so a variant's meaning does not change once it exists. The
/// serialized form is the same string the logs carry, because these travel in
/// a stored [`DisplayFailure`] and a renamed variant would silently change
/// what an existing VM reads back as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayStatusCode {
    /// The VM asks for no desktop.
    #[serde(rename = "display-profile-headless")]
    ProfileHeadless,
    /// The VM has a desktop, but it is not running, so nothing is displayed.
    #[serde(rename = "display-vm-not-running")]
    VmNotRunning,
    /// The desktop is still being installed.
    #[serde(rename = "display-provisioning-pending")]
    ProvisioningPending,
    /// The desktop packages could not be downloaded from the distribution's
    /// repositories.
    #[serde(rename = "display-package-download-failed")]
    PackageDownloadFailed,
    /// The desktop packages were downloaded and did not install.
    #[serde(rename = "display-package-install-failed")]
    PackageInstallFailed,
    /// Installing the desktop did not finish inside the time allowed for it.
    #[serde(rename = "display-provisioning-timeout")]
    ProvisioningTimeout,
    /// This guest cannot have the desktop the profile asks for.
    #[serde(rename = "display-profile-unsupported")]
    ProfileUnsupported,
    /// The desktop is installed; the guest has not reported its display
    /// services yet.
    #[serde(rename = "display-guest-pending")]
    GuestPending,
    /// The guest's display services are up and a viewer may connect.
    #[serde(rename = "display-guest-ready")]
    GuestReady,
    /// The guest's display services are installed and not running.
    #[serde(rename = "display-guest-services-failed")]
    GuestServicesFailed,
    /// This build carries no display payload for this guest.
    #[serde(rename = "display-payload-missing")]
    PayloadMissing,
    /// A display payload is there and is not what it claims to be.
    #[serde(rename = "display-payload-invalid")]
    PayloadInvalid,
    /// The guest could not install what building the module needs.
    #[serde(rename = "display-payload-dependencies-failed")]
    PayloadDependenciesFailed,
    /// The module would not build for the guest's running kernel.
    #[serde(rename = "display-payload-build-failed")]
    PayloadBuildFailed,
    /// The module built and would not load.
    #[serde(rename = "display-payload-module-not-loaded")]
    PayloadModuleNotLoaded,
    /// The module loaded and no display device appeared.
    #[serde(rename = "display-payload-no-device")]
    PayloadNoDevice,
    /// An update did not verify, and the previous version is running again.
    #[serde(rename = "display-payload-update-rolled-back")]
    PayloadUpdateRolledBack,
    /// An update did not verify and the previous version did not come back.
    #[serde(rename = "display-payload-update-failed")]
    PayloadUpdateFailed,
}

impl DisplayStatusCode {
    /// The code as it is written in logs and matched on from outside Rust.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProfileHeadless => "display-profile-headless",
            Self::VmNotRunning => "display-vm-not-running",
            Self::ProvisioningPending => "display-provisioning-pending",
            Self::PackageDownloadFailed => "display-package-download-failed",
            Self::PackageInstallFailed => "display-package-install-failed",
            Self::ProvisioningTimeout => "display-provisioning-timeout",
            Self::ProfileUnsupported => "display-profile-unsupported",
            Self::GuestPending => "display-guest-pending",
            Self::GuestReady => "display-guest-ready",
            Self::GuestServicesFailed => "display-guest-services-failed",
            Self::PayloadMissing => "display-payload-missing",
            Self::PayloadInvalid => "display-payload-invalid",
            Self::PayloadDependenciesFailed => "display-payload-dependencies-failed",
            Self::PayloadBuildFailed => "display-payload-build-failed",
            Self::PayloadModuleNotLoaded => "display-payload-module-not-loaded",
            Self::PayloadNoDevice => "display-payload-no-device",
            Self::PayloadUpdateRolledBack => "display-payload-update-rolled-back",
            Self::PayloadUpdateFailed => "display-payload-update-failed",
        }
    }

    /// Whether attempting the same step again could get past this cause.
    ///
    /// A download that failed and an install that was interrupted are worth
    /// another attempt; a release that has no desktop packages is not, and a
    /// retry offered for it would only fail the same way. The same line runs
    /// through the payload's causes: a build that failed may build on the next
    /// start, and a release that carries no payload for this guest will carry
    /// none on the next start either.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::PackageDownloadFailed
                | Self::PackageInstallFailed
                | Self::ProvisioningTimeout
                | Self::GuestServicesFailed
                | Self::PayloadDependenciesFailed
                | Self::PayloadBuildFailed
                | Self::PayloadModuleNotLoaded
                | Self::PayloadNoDevice
                | Self::PayloadUpdateRolledBack
                | Self::PayloadUpdateFailed
        )
    }
}

impl std::fmt::Display for DisplayStatusCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The name the display payload share is offered and mounted under.
pub const DISPLAY_PAYLOAD_SHARE: &str = "vmlord.display.payload";

/// The one share a VM's display is offered.
///
/// Its own type and not a GPU share with another role: a GPU manifest that
/// failed to attach must not be able to take the display with it, and a role
/// added to the GPU enum would be exactly that coupling. It carries a name and
/// nothing else, because there is one display share and the guest knows what to
/// do with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayShare {
    /// The share's name, which is also `aname=` when the guest mounts it.
    pub name: String,
}

/// What a backend observed about a VM's display, without deciding what it
/// means.
///
/// Facts only, exactly as with the GPU: each field is either something that
/// was seen or `None` for "not observed yet", and turning them into a state a
/// person can read is `vmlord_app`'s job. Runtime state, so unlike the profile
/// and the provisioning beside it on a `VmSummary`, none of this is stored:
/// a display service that was running before the VM was stopped says nothing
/// about the one that is not running now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmDisplayFacts {
    /// What the guest agent last reported about its display services.
    pub guest: Option<GuestDisplayReport>,
    /// What is installed of the display payload, and what could be.
    pub payload: DisplayPayloadFacts,
    /// What the payload half last failed at, when it has failed.
    ///
    /// Separate from the guest report's own `Failed`, because that one is
    /// about services that are installed and not running, and this one is
    /// about a module that is not installed at all.
    pub failure: Option<DisplayFailure>,
    /// When that report was observed. `None` while there is none.
    pub observed_at: Option<SystemTime>,
}

/// Which versions of the display payload are in play for one VM.
///
/// Facts, not stored: an update needs a running VM to ask, so a stopped one has
/// no question to answer, and a version read before a stop says nothing about
/// what is installed after one. Versions are strings here because the host does
/// not order them -- the catalog does, and it does so with a type of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayPayloadFacts {
    /// The version DKMS has installed, when the guest has said.
    pub installed: Option<String>,
    /// The version DKMS still holds beside it, which is what a failed update
    /// would roll back to.
    pub previous: Option<String>,
    /// The version of the module that is actually loaded.
    pub loaded: Option<String>,
    /// The best version this release could offer this guest.
    pub available: Option<String>,
}

impl DisplayPayloadFacts {
    /// Whether a newer version could be installed than the one running.
    ///
    /// An offer and never an action: a start installs what is missing and
    /// nothing else, and moving to a newer version is something a person asks
    /// for.
    #[must_use]
    pub fn update_available(&self) -> bool {
        match (&self.installed, &self.available) {
            (Some(installed), Some(available)) => installed != available,
            _ => false,
        }
    }
}

/// What the guest agent last said about the display services inside it.
///
/// The guest reports readiness over the agent channel rather than over the
/// display protocol, because the display sockets exist only while a viewer is
/// connected: the host connects to them, and something has to say when
/// connecting is worth trying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestDisplayReport {
    /// The desktop is installed and its display services are still coming up.
    ServicesPending,
    /// The guest listens on its display services; a viewer may connect.
    Ready(GuestDisplayDetail),
    /// The guest has a desktop it cannot display, and why.
    Failed(DisplayFailure),
}

/// What the guest has to say about the display it is offering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuestDisplayDetail {
    /// What the guest's compositor calls itself, when it says.
    pub compositor: Option<String>,
    /// The DRM output the desktop is drawn on, such as `Virtual-1`.
    pub output: Option<String>,
}

/// How well the display a VM asked for is working right now.
///
/// The coarse reading -- what the UI paints and what a person takes in at a
/// glance. [`DisplayStage`] says where it came from and [`DisplayStatusCode`]
/// says exactly why, so this enum does not grow a variant per reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayState {
    /// There is no desktop in play: the VM asks for none, or it is not
    /// running so nothing is displayed.
    #[default]
    Disabled,
    /// The desktop is being installed.
    Provisioning,
    /// The desktop is installed and the guest has not reported it yet.
    WaitingForGuest,
    /// The guest offers a display. This is the working state.
    Ready,
    /// The VM runs, and the desktop it asked for does not.
    Degraded,
}

/// What the display stack is doing for one VM, as the application layer reads
/// it.
///
/// Derived per refresh from the stored profile, the stored provisioning, the
/// VM's state and whatever the guest last reported; never stored itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmDisplayStatus {
    pub state: DisplayState,
    pub stage: DisplayStage,
    pub code: DisplayStatusCode,
    /// The payload version the guest is running, when it has said.
    pub running_version: Option<String>,
    /// A newer version this release offers, when there is one.
    pub available_version: Option<String>,
    /// What to show a person, with whatever detail there is.
    pub message: String,
    /// What the guest reported, when it has reported anything.
    pub guest: Option<GuestDisplayDetail>,
    /// Whether installing the desktop can be attempted again from here.
    pub can_retry: bool,
    /// When the facts behind this status were observed; the time the status
    /// was derived when there are none yet.
    pub observed_at: SystemTime,
}

impl VmDisplayStatus {
    /// Whether a viewer could connect to this VM right now.
    #[must_use]
    pub const fn is_connectable(&self) -> bool {
        matches!(self.state, DisplayState::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_with_no_payload_for_this_guest_is_not_worth_retrying() {
        assert!(!DisplayStatusCode::PayloadMissing.is_retryable());
        assert!(!DisplayStatusCode::PayloadInvalid.is_retryable());
    }

    #[test]
    fn a_build_that_failed_is_worth_another_attempt() {
        for code in [
            DisplayStatusCode::PayloadDependenciesFailed,
            DisplayStatusCode::PayloadBuildFailed,
            DisplayStatusCode::PayloadModuleNotLoaded,
            DisplayStatusCode::PayloadNoDevice,
        ] {
            assert!(code.is_retryable(), "{code} should be retryable");
        }
    }

    #[test]
    fn every_payload_code_serializes_as_the_string_it_logs() {
        let code = DisplayStatusCode::PayloadUpdateRolledBack;

        assert_eq!(code.as_str(), "display-payload-update-rolled-back");
        assert_eq!(
            serde_json::to_string(&code).unwrap(),
            "\"display-payload-update-rolled-back\""
        );
    }

    #[test]
    fn the_payload_has_a_stage_of_its_own_because_it_is_not_a_provisioning() {
        assert_eq!(DisplayStage::Payload.as_str(), "payload");
        assert_ne!(DisplayStage::Payload, DisplayStage::Provisioning);
    }

    #[test]
    fn an_update_is_available_only_when_two_versions_differ() {
        let running = |installed: &str, available: &str| DisplayPayloadFacts {
            installed: Some(installed.to_owned()),
            available: Some(available.to_owned()),
            ..DisplayPayloadFacts::default()
        };

        assert!(running("0.1.0", "0.2.0").update_available());
        assert!(!running("0.2.0", "0.2.0").update_available());
        assert!(
            !DisplayPayloadFacts {
                available: Some("0.2.0".to_owned()),
                ..DisplayPayloadFacts::default()
            }
            .update_available(),
            "a guest that has said nothing is not a guest with an update waiting"
        );
    }

    #[test]
    fn a_new_vm_asks_for_a_desktop() {
        assert_eq!(DesktopProfile::default(), DesktopProfile::Gnome);
        assert!(DesktopProfile::default().wants_desktop());
        assert!(!DesktopProfile::Headless.wants_desktop());
    }

    #[test]
    fn a_headless_vm_provisions_no_desktop() {
        assert_eq!(
            DisplayProvisioning::requested(DesktopProfile::Headless),
            DisplayProvisioning::NotRequested
        );
        assert_eq!(
            DisplayProvisioning::requested(DesktopProfile::Gnome),
            DisplayProvisioning::Pending
        );
    }

    #[test]
    fn small_resources_are_advised_against_and_not_refused() {
        assert!(desktop_resource_advice(DesktopProfile::Gnome, 1, 8192).is_some());
        assert!(desktop_resource_advice(DesktopProfile::Gnome, 4, 2048).is_some());
        let both = desktop_resource_advice(DesktopProfile::Gnome, 1, 1024).expect("advice");
        assert!(both.contains('1') && both.contains("1024"));
        assert_eq!(
            desktop_resource_advice(
                DesktopProfile::Gnome,
                MIN_DESKTOP_CPU_CORES,
                MIN_DESKTOP_RAM_MB
            ),
            None
        );
    }

    #[test]
    fn a_headless_vm_is_never_advised_about_desktop_resources() {
        assert_eq!(
            desktop_resource_advice(DesktopProfile::Headless, 1, 512),
            None
        );
    }

    #[test]
    fn only_a_retryable_failure_offers_a_retry() {
        let network = DisplayProvisioning::Degraded(DisplayFailure::new(
            DisplayStage::Provisioning,
            DisplayStatusCode::PackageDownloadFailed,
            "the archive did not answer",
        ));
        assert!(network.can_retry());
        assert_eq!(network.retried(), Some(DisplayProvisioning::Pending));

        let unsupported = DisplayProvisioning::Degraded(DisplayFailure::new(
            DisplayStage::Provisioning,
            DisplayStatusCode::ProfileUnsupported,
            "this release has no GNOME packages",
        ));
        assert!(!unsupported.can_retry());
        assert_eq!(unsupported.retried(), None);

        assert!(!DisplayProvisioning::Ready.can_retry());
        assert!(!DisplayProvisioning::Pending.can_retry());
        assert!(!DisplayProvisioning::NotRequested.can_retry());
    }

    #[test]
    fn a_stored_failure_keeps_its_code_across_a_round_trip() {
        let provisioning = DisplayProvisioning::Degraded(DisplayFailure::new(
            DisplayStage::Provisioning,
            DisplayStatusCode::PackageInstallFailed,
            "dpkg exited with 1",
        ));
        let document = serde_json::to_string(&provisioning).expect("serialize");
        assert!(document.contains("display-package-install-failed"));
        let decoded: DisplayProvisioning = serde_json::from_str(&document).expect("deserialize");
        assert_eq!(decoded, provisioning);
    }

    #[test]
    fn every_code_spells_itself_the_way_it_is_stored() {
        for code in [
            DisplayStatusCode::ProfileHeadless,
            DisplayStatusCode::VmNotRunning,
            DisplayStatusCode::ProvisioningPending,
            DisplayStatusCode::PackageDownloadFailed,
            DisplayStatusCode::PackageInstallFailed,
            DisplayStatusCode::ProvisioningTimeout,
            DisplayStatusCode::ProfileUnsupported,
            DisplayStatusCode::GuestPending,
            DisplayStatusCode::GuestReady,
            DisplayStatusCode::GuestServicesFailed,
        ] {
            let stored = serde_json::to_string(&code).expect("serialize");
            assert_eq!(stored, format!("\"{}\"", code.as_str()));
        }
    }
}
