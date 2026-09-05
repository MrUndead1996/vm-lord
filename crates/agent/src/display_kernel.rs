//! Making the guest's display module exist, and moving it between versions.
//!
//! What is here runs programs and touches files, which is why the decisions it
//! makes live next door in `display_recipe`: everything that can be decided
//! from text is decided there, and everything that needs an Ubuntu guest with a
//! kernel is decided here.
//!
//! Two entry points, and the difference between them is the whole shape of the
//! task. [`apply`] reconciles -- it installs what is missing, rebuilds what a
//! kernel upgrade broke, and does nothing at all to a guest that is already
//! running the mounted version. [`update`] moves a guest from one version to
//! another, verifies what it installed, and steps back one version when that
//! verification fails.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use vmlord_agent_protocol::v1::{
    DisplayPayloadVersions, DisplayRecipeStage, DisplayRecipeStep, DisplaySigningCertificate,
    DisplayUpdateOutcome, GuestDesktop,
};

use crate::{
    command,
    display_recipe::{
        DKMS_PACKAGE, InstalledVersions, KeyCreation, MODULE, PayloadFacts, Report,
        SigningKeyState, SigningPair, compositor_isolation, dkms_reports_installed, dkms_versions,
        kernel_signs_modules, modprobe_options, module_is_loaded, needs_build, needs_reload,
        parse_module_parameters, parse_module_signature_key, parse_module_version,
        parse_secure_boot_state, parse_subject_key_identifier, read_payload_facts, serves,
        signature_matches, signing_key_state, signing_pair, wanted_mode, was_built_for,
        was_rejected_for_its_signature,
    },
    gpu_kernel,
    gpu_targets::PAYLOAD as GPU_PAYLOAD,
    guest_files::{copy_tree, failure, read, write_if_different},
    guest_packages::{self, Package},
    guest_platform::{
        self, CompositorLaunch, DesktopFacts, GuestFacts, InitramfsBuilder, LibraryLayout,
        guest_facts, program_on_path,
    },
};

/// Where the guest mounts the display payload share.
pub const PAYLOAD_MOUNT: &str = "/opt/vmlord/display-payload";

const DKMS_TREE: &str = "/var/lib/dkms";
const MODULES_LOAD: &str = "/etc/modules-load.d/vmlord-display.conf";
const MODPROBE_OPTIONS: &str = "/etc/modprobe.d/vmlord-display.conf";
const UNBIND_UNIT: &str = "/etc/systemd/system/vmlord-display-unbind-simpledrm.service";
/// The drop-in that keeps a compositor off the payload's Mesa, by the name it
/// is installed under.
///
/// Which unit's drop-in directory it goes in is not written here, because it
/// is not a constant: it is the unit the compositor of this guest turned out
/// to be started by.
const COMPOSITOR_DROP_IN: &str = "vmlord-display-compositor-mesa.conf";
/// Where the rule that keeps the desktop on this output goes. The number puts
/// it after mutter's own `61-mutter.rules`, whose tag it adds to.
const UDEV_RULES: &str = "/etc/udev/rules.d/62-vmlord-display.rules";
const DRM_DEVICES: &str = "/sys/class/drm";

/// Where the two guest programs are installed. Beside the module's own unit,
/// and named by the units this task ships.
const SERVICES_INSTALL: &str = "/usr/local/lib/vmlord";
/// Where their units go.
const SYSTEMD_UNITS: &str = "/etc/systemd/system";
/// Where a user unit goes, for whichever session starts next.
const SYSTEMD_USER_UNITS: &str = "/etc/systemd/user";
/// The account the unprivileged half runs as.
const SERVICE_USER: &str = "vmlord-display";
/// The five programs, in the order they are started.
const SERVICE_BINARIES: [&str; 5] = [
    "vmlord-display-broker",
    "vmlord-display-session",
    "vmlord-display-clipboard",
    "vmlord-display-audio",
    "vmlord-display-tray",
];
/// The units systemd starts at boot, which are those binaries' names with a
/// suffix.
const SYSTEM_UNITS: [&str; 3] = [
    "vmlord-display-broker.service",
    "vmlord-display-session.service",
    "vmlord-display-audio.service",
];
/// The units that start inside a user's graphical session instead, because
/// what they serve does: a selection, and a tray icon, exist only there.
const USER_UNITS: [&str; 2] = [
    "vmlord-display-clipboard.service",
    "vmlord-display-tray.service",
];
/// The socket the two halves meet on, which is how "started" is confirmed.
const BROKER_SOCKET: &str = "/run/vmlord/display-broker.sock";

/// Where GNOME Shell's packaged extensions are installed.
const SHELL_EXTENSIONS: &str = "/usr/share/gnome-shell/extensions";
/// The distro package that ships a StatusNotifierItem extension. Under this
/// name Ubuntu's own fork on every supported release; Debian ships the same
/// source under the upstream UUID.
/// The packages a DKMS build needs, whatever this guest calls them.
const BUILD_DEPENDENCIES: &[Package] =
    &[Package::Dkms, Package::BuildTools, Package::KernelHeaders];

/// The GNOME extension the tray icon shows through.
const APPINDICATOR: &[Package] = &[Package::AppIndicator];
/// The extension UUIDs that package ships, Ubuntu's own first.
const APPINDICATOR_UUIDS: [&str; 2] = [
    "ubuntu-appindicators@ubuntu.com",
    "appindicatorsupport@rgcjonas.gmail.com",
];

/// The kernel module the audio daemon reads the desktop through.
const LOOPBACK_MODULE: &str = "snd-aloop";
/// What asks for that module on every boot, and where it is shipped.
const LOOPBACK_MODULES_LOAD: &str = "/etc/modules-load.d/vmlord-audio.conf";
/// What gives it one cable instead of the default two, so that GNOME shows one
/// output rather than a duplicate that plays into nothing.
const LOOPBACK_MODPROBE: &str = "/etc/modprobe.d/vmlord-audio.conf";
/// Where PipeWire reads its own drop-ins.
const PIPEWIRE_DROP_IN: &str = "/etc/pipewire/pipewire.conf.d/51-vmlord-audio.conf";
/// The sink that binds the desktop's output straight to the loopback.
const AUDIO_SINK: &str = "51-vmlord-audio.conf";
/// The rule that hides the loopback's capture side, for WirePlumber 0.5.
const LOOPBACK_RULE_JSON: &str = "51-vmlord-loopback.conf";
/// The same rule for WirePlumber 0.4, which reads Lua instead.
const LOOPBACK_RULE_LUA: &str = "51-vmlord-loopback.lua";
/// Where the guest's own WirePlumber says which of the two it reads.
const WIREPLUMBER_SHARE: &str = "/usr/share/wireplumber";
const MODULE_VERSION: &str = "/sys/module/vmlord_drm/version";
const MODULE_PARAM_WIDTH: &str = "/sys/module/vmlord_drm/parameters/width";
const MODULE_PARAM_HEIGHT: &str = "/sys/module/vmlord_drm/parameters/height";

/// The budgets the recipe's programs get, as the GPU recipe's do: a build
/// compiles a kernel module and everything else is a few syscalls. What an
/// install gets is `guest_packages`' own budget, because that is where the
/// installing happens.
const BUILD_BUDGET: Duration = Duration::from_secs(900);
const SHORT_BUDGET: Duration = Duration::from_secs(30);

const KEPT_LOG_LINES: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReloadDisposition {
    Reload,
    RebootRequired,
}

fn reload_disposition(unload_succeeded: bool, module_is_still_loaded: bool) -> ReloadDisposition {
    if !unload_succeeded && module_is_still_loaded {
        ReloadDisposition::RebootRequired
    } else {
        ReloadDisposition::Reload
    }
}

fn reported_update_versions(
    target_version: &str,
    outcome: DisplayUpdateOutcome,
    mut versions: DisplayPayloadVersions,
) -> DisplayPayloadVersions {
    if outcome == DisplayUpdateOutcome::RebootRequired {
        versions.previous.clone_from(&versions.loaded);
        versions.installed = target_version.to_owned();
    }
    versions
}

enum UpdateAttemptFailure {
    Failed(String),
    RebootRequired(String),
}

fn rollback_outcome(runtime_restored: bool) -> DisplayUpdateOutcome {
    if runtime_restored {
        DisplayUpdateOutcome::RolledBack
    } else {
        DisplayUpdateOutcome::Failed
    }
}

/// What one run of the recipe found out and what it installed.
///
/// A struct rather than a tuple because the last two members are answers the
/// recipe carries out of a run that failed: a certificate and a desktop are
/// worth most exactly when the display did not come up, and positional
/// members that far along stop saying what they are.
pub struct RecipeOutcome {
    pub stages: Vec<DisplayRecipeStage>,
    pub versions: DisplayPayloadVersions,
    /// The certificate this guest signs its modules with, when it has one.
    pub certificate: Option<DisplaySigningCertificate>,
    /// The desktop this guest turned out to have, when it has one.
    ///
    /// What the recipe found, never what the VM was created asking for: the
    /// host holds that half itself and does not need it read back to it.
    pub desktop: Option<GuestDesktop>,
}

/// Installs what the mounted payload says the guest should have.
///
/// Idempotent by fact: a guest already running the mounted version passes
/// through in a handful of checks and needs no network. Every failure is a
/// stage that says so, and the VM keeps running regardless.
pub fn apply(stopping: &AtomicBool, mode: Option<(u32, u32)>) -> RecipeOutcome {
    let mut report = Report::new();
    // Out-parameters rather than the `Ok` half: a recipe that failed at
    // `ModuleLoad` is exactly when the host most needs the certificate and the
    // desktop, and a `Result` would drop them on the way out.
    let mut signing = None;
    let mut desktop = None;
    let reason = match run_stages(&mut report, stopping, mode, &mut signing, &mut desktop) {
        Ok(()) => "the recipe did not need this stage".to_owned(),
        Err(reason) => reason,
    };
    RecipeOutcome {
        stages: report.finish(&reason),
        versions: versions(),
        certificate: signing.map(|signing| signing.certificate),
        desktop,
    }
}

/// The desktop the guest was found to have, in the shape the host reads.
///
/// `None` where nothing was found, which is what a guest with no desktop and
/// a guest nobody has logged into yet both are. Absence travels as absence
/// rather than as three empty strings, so the host is never left deciding
/// whether a message full of nothing means a desktop was looked for.
fn reported_desktop(facts: &DesktopFacts) -> Option<GuestDesktop> {
    facts.found().then(|| GuestDesktop {
        session: facts.session.clone().unwrap_or_default(),
        session_type: facts.session_type.clone().unwrap_or_default(),
        display_manager: facts.display_manager.clone().unwrap_or_default(),
    })
}

/// Moves the guest to `target_version`, verifies it, and rolls back if it did
/// not verify.
pub fn update(
    target_version: &str,
    stopping: &AtomicBool,
) -> (
    Vec<DisplayRecipeStage>,
    DisplayPayloadVersions,
    DisplayUpdateOutcome,
) {
    let mut report = Report::new();
    let (reason, outcome) = match run_update(&mut report, target_version, stopping) {
        Ok(()) => (
            "the update did not need this stage".to_owned(),
            DisplayUpdateOutcome::Updated,
        ),
        Err(UpdateFailure { reason, outcome }) => (reason, outcome),
    };
    let versions = reported_update_versions(target_version, outcome, versions());
    (report.finish(&reason), versions, outcome)
}

/// What the guest has of the display payload right now.
pub fn versions() -> DisplayPayloadVersions {
    let installed = installed_versions();
    let loaded = installed.loaded.clone().unwrap_or_default();
    let current = installed
        .loaded
        .clone()
        .or_else(|| installed.versions.first().cloned())
        .unwrap_or_default();
    DisplayPayloadVersions {
        previous: installed.previous(&current).unwrap_or_default(),
        installed: current,
        loaded,
    }
}

struct UpdateFailure {
    reason: String,
    outcome: DisplayUpdateOutcome,
}

/// The stages, in order, stopping at the first one that ends the recipe.
///
/// `Err` carries what the stages that never ran are reported with.
fn run_stages(
    report: &mut Report,
    stopping: &AtomicBool,
    mode: Option<(u32, u32)>,
    signing: &mut Option<GuestSigning>,
    desktop: &mut Option<GuestDesktop>,
) -> Result<(), String> {
    let guest = guest_facts()?;
    // Before the first stage can end the run: what a guest has for a desktop
    // is the answer somebody needs most when the display did not come up.
    *desktop = reported_desktop(&guest.desktop);
    // No list of distributions this build knows: every later stage asks the
    // guest for what it needs -- a package manager, an initramfs builder, a
    // compositor -- and says so by name when the guest has none. A recipe that
    // stopped here instead would be telling a guest it is the wrong
    // distribution when what it actually lacks is one program.
    report.ok(
        DisplayRecipeStep::Distribution,
        format!(
            "{} {} {} on kernel {}; {}",
            guest.distribution,
            guest.release,
            guest.architecture,
            guest.kernel_release,
            guest.platform()
        ),
    );

    let payload = payload_stage(
        report,
        &guest.distribution,
        &guest.release,
        &guest.architecture,
        None,
    )?;
    halted(stopping)?;

    let installed = installed_versions();
    let built = needs_build(&installed, &payload.version, device_is_present());
    if built {
        dependencies_stage(report, &guest)?;
        halted(stopping)?;
    } else {
        let already = format!(
            "{DKMS_PACKAGE} {} is installed, loaded and answering",
            payload.version
        );
        for step in [
            DisplayRecipeStep::BuildDependencies,
            DisplayRecipeStep::ModuleSource,
            DisplayRecipeStep::ModuleBuild,
        ] {
            report.skipped(step, already.clone());
        }
    }

    // After `BuildDependencies` and before anything builds, which is where
    // `STEPS` puts it: whether this kernel signs modules is written in its
    // build tree, which is what that stage installs, and the pair has to exist
    // before the build that signs with it.
    *signing = signing_key_stage(report, &guest);
    halted(stopping)?;

    if built {
        source_stage(report, &payload)?;
        halted(stopping)?;
        build_stage(report, &payload.version, &guest.kernel_release)?;
        halted(stopping)?;
    }

    module_signature_stage(report, signing.as_ref(), &guest.kernel_release);
    halted(stopping)?;

    load_stage(
        report,
        mode,
        built.then_some((guest.initramfs_builder, guest.kernel_release.as_str())),
        signing.as_ref(),
    )?;
    compositor_isolation_stage(report, &guest)?;
    device_stage(report)?;
    services_stages(
        report,
        &guest,
        &payload_services(),
        Path::new(SERVICES_INSTALL),
    )?;
    Ok(())
}

fn run_update(
    report: &mut Report,
    target_version: &str,
    stopping: &AtomicBool,
) -> Result<(), UpdateFailure> {
    let failed = |reason: String| UpdateFailure {
        reason,
        outcome: DisplayUpdateOutcome::Failed,
    };

    let guest = guest_facts().map_err(failed)?;
    report.ok(
        DisplayRecipeStep::Distribution,
        format!(
            "{} {} {}",
            guest.distribution, guest.release, guest.architecture
        ),
    );

    let payload = payload_stage(
        report,
        &guest.distribution,
        &guest.release,
        &guest.architecture,
        Some(target_version),
    )
    .map_err(failed)?;

    // What a rollback returns to, read before anything changes.
    let before = installed_versions();
    halted(stopping).map_err(failed)?;

    let attempt = (|| -> Result<(), UpdateAttemptFailure> {
        dependencies_stage(report, &guest).map_err(UpdateAttemptFailure::Failed)?;
        // After the headers are there: the kernel configuration that says
        // whether modules are signed at all lives in the build tree.
        let signing = signing_key_stage(report, &guest);
        source_stage(report, &payload).map_err(UpdateAttemptFailure::Failed)?;
        build_stage(report, &payload.version, &guest.kernel_release)
            .map_err(UpdateAttemptFailure::Failed)?;
        module_signature_stage(report, signing.as_ref(), &guest.kernel_release);
        update_initramfs_stage(report, guest.initramfs_builder, &guest.kernel_release)
            .map_err(UpdateAttemptFailure::Failed)?;
        reload_module_for_update(report, &payload.version, signing.as_ref())?;
        verify(report, &payload.version).map_err(UpdateAttemptFailure::Failed)
    })();

    match attempt {
        Ok(()) => {
            services_stages(
                report,
                &guest,
                &payload_services(),
                Path::new(SERVICES_INSTALL),
            )
            .map_err(failed)?;
            Ok(())
        }
        Err(UpdateAttemptFailure::RebootRequired(reason)) => Err(UpdateFailure {
            reason,
            outcome: DisplayUpdateOutcome::RebootRequired,
        }),
        Err(UpdateAttemptFailure::Failed(reason)) => {
            Err(roll_back(&before, &payload.version, &guest, reason))
        }
    }
}

/// Puts the previous version back, and says what that came to.
///
/// The previous `/usr/src` tree was never removed and DKMS still holds its
/// build, so this is a `modprobe` and a `dkms remove` rather than a download.
fn roll_back(
    before: &InstalledVersions,
    attempted: &str,
    guest: &GuestFacts,
    reason: String,
) -> UpdateFailure {
    let Some(previous) = before.loaded.clone().or_else(|| before.previous(attempted)) else {
        return UpdateFailure {
            reason: format!("{reason}; there is no previous version to return to"),
            outcome: DisplayUpdateOutcome::Failed,
        };
    };

    let _ = command::run("modprobe", &["-r", MODULE], &[], SHORT_BUDGET);
    let _ = command::run(
        "dkms",
        &["remove", "-m", DKMS_PACKAGE, "-v", attempted, "--all"],
        &[],
        SHORT_BUDGET,
    );
    let _ = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    let initramfs = guest
        .initramfs_builder
        .map(|builder| (builder, update_initramfs(builder, &guest.kernel_release)));

    let runtime_restored = module_is_loaded(&read(Path::new("/proc/modules")))
        && loaded_version().as_deref() == Some(previous.as_str())
        && device_is_present();

    if rollback_outcome(runtime_restored) == DisplayUpdateOutcome::RolledBack {
        let boot = match &initramfs {
            Some((builder, outcome)) if !outcome.succeeded() => format!(
                "; {previous} is running, but the rollback could not refresh initramfs: {}",
                failure(&builder.command(&guest.kernel_release), outcome)
            ),
            _ => String::new(),
        };
        UpdateFailure {
            reason: format!("{reason}; {previous} is running again{boot}"),
            outcome: DisplayUpdateOutcome::RolledBack,
        }
    } else {
        let boot = match &initramfs {
            Some((builder, outcome)) if !outcome.succeeded() => format!(
                "; rollback also could not refresh initramfs: {}",
                failure(&builder.command(&guest.kernel_release), outcome)
            ),
            _ => String::new(),
        };
        UpdateFailure {
            reason: format!("{reason}; {previous} could not be brought back either{boot}"),
            outcome: DisplayUpdateOutcome::Failed,
        }
    }
}

/// Reads the mounted payload and checks every file it declares.
///
/// Before anything is copied, and independently of whatever the host verified:
/// a 9p mount is a filesystem the host can rewrite between its own check and
/// this one.
fn payload_stage(
    report: &mut Report,
    distribution: &str,
    release: &str,
    architecture: &str,
    expected_version: Option<&str>,
) -> Result<PayloadFacts, String> {
    let root = Path::new(PAYLOAD_MOUNT);
    let manifest = match fs::read(root.join("payload.json")) {
        Ok(bytes) => bytes,
        Err(error) => {
            let reason = format!("{PAYLOAD_MOUNT}/payload.json could not be read: {error}");
            report.failed(DisplayRecipeStep::Payload, reason.clone());
            return Err(reason);
        }
    };
    let facts = match read_payload_facts(&manifest) {
        Ok(facts) => facts,
        Err(reason) => {
            report.failed(DisplayRecipeStep::Payload, reason.clone());
            return Err(reason);
        }
    };
    if !serves(&facts, architecture) {
        let reason = format!(
            "the mounted payload is built for {} and this guest is {architecture}",
            facts.architecture
        );
        report.failed(DisplayRecipeStep::Payload, reason.clone());
        return Err(reason);
    }
    if let Some(wanted) = expected_version
        && facts.version != wanted
    {
        let reason = format!(
            "an update to {wanted} was asked for and the mounted payload carries {}",
            facts.version
        );
        report.failed(DisplayRecipeStep::Payload, reason.clone());
        return Err(reason);
    }
    if let Err(reason) = verify_declared_files(&manifest, root) {
        report.failed(DisplayRecipeStep::Payload, reason.clone());
        return Err(reason);
    }

    // Provenance is said out loud when it is not this guest's. The payload
    // applies either way, but a person reading the recipe should not have to
    // decode a payload ID to find out that the build was proven elsewhere.
    let provenance = if was_built_for(&facts, distribution, release, architecture) {
        String::new()
    } else {
        format!(
            ", built for {} {} and serving {distribution} {release}",
            facts.distribution, facts.release
        )
    };
    report.ok(
        DisplayRecipeStep::Payload,
        format!(
            "{} verified at {PAYLOAD_MOUNT}{provenance}",
            facts.payload_id
        ),
    );
    Ok(facts)
}

/// Hashes every file `payload.json` declares against what it claims.
fn verify_declared_files(manifest: &[u8], root: &Path) -> Result<(), String> {
    let document: serde_json::Value =
        serde_json::from_slice(manifest).map_err(|error| format!("payload.json: {error}"))?;
    let files = document
        .pointer("/files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "payload.json declares no files".to_owned())?;

    for file in files {
        let path = file
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "payload.json declares a file with no path".to_owned())?;
        let expected = file
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("payload.json declares {path} with no digest"))?;
        let bytes = fs::read(root.join(path))
            .map_err(|error| format!("{path} could not be read from the payload: {error}"))?;
        let actual = sha256_hex(&bytes);
        if actual != expected {
            return Err(format!(
                "{path} hashes to {actual}; the payload says {expected}"
            ));
        }
    }
    Ok(())
}

/// SHA-256 of `bytes`, as lowercase hex.
///
/// The agent hashes nothing else, so this is `sha2` and no wrapper: the payload
/// crates' `Sha256Digest` lives on the host side of the boundary and dragging
/// it into the guest binary would be a dependency for one function.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The pair this guest signs with, once the stage has made sure it exists.
///
/// The certificate travels to the host as bytes; the path stays here, because
/// what an enrollment is performed on is a file on this guest and the host's
/// own copy is a copy.
struct GuestSigning {
    /// Where the certificate lives on this guest.
    certificate_path: String,
    /// What it is, in the two identities the host and the report need.
    certificate: DisplaySigningCertificate,
}

/// Makes sure the guest has a MOK to sign with, and says which one it is.
///
/// Never signs and never fails the recipe. Signing happens inside `dkms
/// build`, which is what carries it through the rebuild an unattended kernel
/// upgrade triggers with no host connected; and with Secure Boot off an
/// unsigned module loads, so a guest that cannot produce a key still gets its
/// desktop.
fn signing_key_stage(report: &mut Report, guest: &GuestFacts) -> Option<GuestSigning> {
    match kernel_module_signing(&guest.kernel_release) {
        Ok(true) => {}
        Ok(false) => {
            report.skipped(
                DisplayRecipeStep::SigningKey,
                format!(
                    "kernel {} is built without module signing",
                    guest.kernel_release
                ),
            );
            return None;
        }
        // Not the same answer as "this kernel does not sign", and reported as
        // the different thing it is: a guest whose headers are elsewhere would
        // otherwise be recorded as one whose kernel refuses signatures, and
        // the day Secure Boot is on that reading is the wrong one.
        Err(reason) => {
            report.skipped(DisplayRecipeStep::SigningKey, reason);
            return None;
        }
    }

    let pair = signing_pair(&dkms_framework_configuration(), &guest.distribution);
    let state = signing_key_state(
        Path::new(&pair.key).exists(),
        Path::new(&pair.certificate).exists(),
    );
    let replaced = state != SigningKeyState::Complete;
    let half = state == SigningKeyState::HalfPresent;
    if replaced {
        // A certificate cannot be derived from a private key, so half a pair
        // is replaced whole rather than completed.
        let _ = fs::remove_file(&pair.key);
        let _ = fs::remove_file(&pair.certificate);
        if let Err(reason) = create_signing_key(&pair) {
            report.failed(DisplayRecipeStep::SigningKey, reason);
            return None;
        }
    }

    let der = match fs::read(&pair.certificate) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => {
            report.failed(
                DisplayRecipeStep::SigningKey,
                format!("{} is empty", pair.certificate),
            );
            return None;
        }
        Err(error) => {
            report.failed(
                DisplayRecipeStep::SigningKey,
                format!("{} could not be read: {error}", pair.certificate),
            );
            return None;
        }
    };
    restrict_to_root(Path::new(&pair.key));

    let printed = command::run(
        "openssl",
        &[
            "x509",
            "-inform",
            "DER",
            "-in",
            &pair.certificate,
            "-noout",
            "-text",
        ],
        &[],
        SHORT_BUDGET,
    );
    let Some(identifier) = parse_subject_key_identifier(&printed.output) else {
        report.failed(
            DisplayRecipeStep::SigningKey,
            format!("{} carries no subject key identifier", pair.certificate),
        );
        return None;
    };
    let sha256 = sha256_hex(&der);

    if replaced {
        // Every version DKMS holds was signed by the key that is now gone, so
        // a rollback would land on a module Secure Boot refuses. Re-signing
        // them is what makes the rollback path survive a replaced key.
        resign_installed_versions(report, &guest.kernel_release);
    }
    report.ok(
        DisplayRecipeStep::SigningKey,
        format!(
            "modules are signed with {} (sha256 {sha256}, key id {identifier}){}",
            pair.certificate,
            if half {
                ", replaced because it was half a pair -- its enrollment has to be performed again"
            } else {
                ""
            }
        ),
    );

    Some(GuestSigning {
        certificate_path: pair.certificate,
        certificate: DisplaySigningCertificate {
            certificate: der,
            sha256,
            subject_key_identifier: identifier,
        },
    })
}

/// Whether this kernel signs modules, asked of the file dkms will ask.
///
/// Not `/boot/config-<release>`: that is where one packaging puts a copy, and
/// a guest that keeps its configuration elsewhere would answer "this kernel
/// does not sign modules" by having no such file. dkms reads the configuration
/// out of the kernel's own build tree -- `include/config/auto.conf` where the
/// build left one, `.config` otherwise -- and reading the same file is what
/// makes this stage's answer and the build's behaviour the same answer.
///
/// `Err` is a guest where neither file is there, which is a question that was
/// not answered rather than an answer of no. The tree is what
/// `BuildDependencies` installs, and that stage runs before this one on every
/// guest that needs a build.
fn kernel_module_signing(kernel_release: &str) -> Result<bool, String> {
    let build = PathBuf::from(format!("/lib/modules/{kernel_release}/build"));
    for name in ["include/config/auto.conf", ".config"] {
        let path = build.join(name);
        if path.exists() {
            return Ok(kernel_signs_modules(&read(&path)));
        }
    }
    Err(format!(
        "kernel {kernel_release} has no configuration under {} to say whether it signs modules",
        build.display()
    ))
}

/// What dkms's own configuration files say, as one text.
///
/// Concatenated rather than merged, because the only thing read out of them is
/// the last assignment of a name -- and `framework.conf.d` is read after
/// `framework.conf` for exactly that reason.
fn dkms_framework_configuration() -> String {
    let mut text = read(Path::new("/etc/dkms/framework.conf"));
    let Ok(entries) = fs::read_dir("/etc/dkms/framework.conf.d") else {
        return text;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "conf")
        })
        .collect();
    // The shell expands the glob in order and the last assignment wins, so the
    // order two drop-ins are read in is part of the answer.
    files.sort();
    for path in files {
        text.push('\n');
        text.push_str(&read(&path));
    }
    text
}

/// Creates the guest's own pair, the way this guest's dkms would create it.
fn create_signing_key(pair: &SigningPair) -> Result<(), String> {
    if let Some(directory) = Path::new(&pair.key).parent() {
        let _ = fs::create_dir_all(directory);
    }
    if let Some(directory) = Path::new(&pair.certificate).parent() {
        let _ = fs::create_dir_all(directory);
    }

    if pair.creation == KeyCreation::SecureBootPolicy {
        let policy = command::run(
            "update-secureboot-policy",
            &["--new-key"],
            &[
                ("SHIM_NOTRIGGER", "y"),
                ("DEBIAN_FRONTEND", "noninteractive"),
            ],
            SHORT_BUDGET,
        );
        if policy.succeeded() && Path::new(&pair.certificate).exists() {
            return Ok(());
        }
        // `shim-signed` is not installed after all. openssl is what dkms
        // itself falls back to, and it is what this falls back to.
    }

    // The extensions are named in the command rather than taken from a
    // configuration file. `shim-signed` ships one at
    // `/usr/lib/shim/mok/openssl.cnf` and a guest without that package has no
    // such file; what the file was needed for is `subjectKeyIdentifier`, which
    // is what `sign-file` records into the module and what `ModuleSignature`
    // matches on, and asking for it directly asks for it on every guest.
    let openssl = command::run(
        "openssl",
        &[
            "req",
            "-new",
            "-x509",
            "-nodes",
            "-utf8",
            "-batch",
            "-days",
            "36500",
            "-newkey",
            "rsa:2048",
            "-subj",
            "/CN=VMLord display module signing key/",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "keyUsage=digitalSignature",
            "-addext",
            "extendedKeyUsage=codeSigning,1.3.6.1.4.1.2312.16.1.2",
            "-addext",
            "subjectKeyIdentifier=hash",
            "-keyout",
            &pair.key,
            "-outform",
            "DER",
            "-out",
            &pair.certificate,
        ],
        &[],
        SHORT_BUDGET,
    );
    if !openssl.succeeded() {
        return Err(failure("openssl req", &openssl));
    }
    Ok(())
}

/// A private key readable by anything but root is a key that has left.
fn restrict_to_root(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

/// Rebuilds and reinstalls every version DKMS holds, so that all of them carry
/// the key that exists now.
fn resign_installed_versions(report: &mut Report, kernel_release: &str) {
    let status = command::run("dkms", &["status"], &[], SHORT_BUDGET);
    for version in dkms_versions(&status.output, DKMS_PACKAGE) {
        let module = format!("{DKMS_PACKAGE}/{version}");
        let built = command::run(
            "dkms",
            &["build", "--force", "-m", &module, "-k", kernel_release],
            &[],
            BUILD_BUDGET,
        );
        if !built.succeeded() {
            report.failed(
                DisplayRecipeStep::SigningKey,
                failure(&format!("dkms build --force {module}"), &built),
            );
            return;
        }
        let _ = command::run(
            "dkms",
            &["install", "--force", "-m", &module, "-k", kernel_release],
            &[],
            BUILD_BUDGET,
        );
    }
}

/// Says whether the module the build installed carries our signature.
///
/// Never fails the recipe: with Secure Boot off an unsigned module loads, and
/// failing a working desktop over a signature nothing checks yet would be a
/// regression. What it buys is that the day Secure Boot is on, the report
/// already says whether this guest was producing signed modules.
fn module_signature_stage(
    report: &mut Report,
    signing: Option<&GuestSigning>,
    kernel_release: &str,
) {
    let Some(certificate) = signing.map(|signing| &signing.certificate) else {
        report.skipped(
            DisplayRecipeStep::ModuleSignature,
            "this guest has no signing key, so there is no signature to check",
        );
        return;
    };

    let path = format!("/lib/modules/{kernel_release}/updates/dkms/{MODULE}.ko");
    let modinfo = command::run("modinfo", &[&path], &[], SHORT_BUDGET);
    if signature_matches(&modinfo.output, &certificate.subject_key_identifier) {
        report.ok(
            DisplayRecipeStep::ModuleSignature,
            format!(
                "{MODULE} is signed with key id {}",
                certificate.subject_key_identifier
            ),
        );
        return;
    }

    report.failed(
        DisplayRecipeStep::ModuleSignature,
        match parse_module_signature_key(&modinfo.output) {
            Some(other) => format!(
                "{MODULE} is signed with key id {other}, and not with the guest's own {}",
                certificate.subject_key_identifier
            ),
            None => format!("{MODULE} carries no signature"),
        },
    );
}

/// The text a failed `modprobe` is reported as.
///
/// A refusal over a signature keeps the kernel's own phrase -- the host reads
/// it back to choose a status code -- and gains the two facts a person needs
/// in order to act: whether Secure Boot is on, and which certificate has to be
/// enrolled.
fn load_failure_message(
    reason: &str,
    secure_boot: Option<bool>,
    certificate: Option<(&str, &str)>,
) -> String {
    if !was_rejected_for_its_signature(reason) {
        return reason.to_owned();
    }
    let state = match secure_boot {
        Some(true) => "Secure Boot is on",
        Some(false) => "Secure Boot is off",
        None => "the Secure Boot state is unknown",
    };
    let certificate = match certificate {
        Some((path, identifier)) => {
            format!("enroll {path} (key id {identifier}) as a MOK")
        }
        None => "this guest has no certificate to enroll".to_owned(),
    };
    format!("{reason} -- {state} and {certificate}")
}

/// What a failed `modprobe` of our module is reported as, asked once.
fn load_failure(outcome: &command::Outcome, signing: Option<&GuestSigning>) -> String {
    let reason = failure(&format!("modprobe {MODULE}"), outcome);
    if !was_rejected_for_its_signature(&reason) {
        return reason;
    }
    let secure_boot = parse_secure_boot_state(
        &command::run("mokutil", &["--sb-state"], &[], SHORT_BUDGET).output,
    );
    load_failure_message(
        &reason,
        secure_boot,
        signing.map(|signing| {
            (
                signing.certificate_path.as_str(),
                signing.certificate.subject_key_identifier.as_str(),
            )
        }),
    )
}

fn dependencies_stage(report: &mut Report, guest: &GuestFacts) -> Result<(), String> {
    if dependencies_are_present(&guest.kernel_release) {
        report.skipped(
            DisplayRecipeStep::BuildDependencies,
            "dkms, a compiler and the running kernel's headers are installed",
        );
        return Ok(());
    }

    let Some(manager) = guest.package_manager else {
        let reason = guest_packages::no_manager(BUILD_DEPENDENCIES);
        report.failed(DisplayRecipeStep::BuildDependencies, reason.clone());
        return Err(reason);
    };

    let (outcome, installed) =
        guest_packages::install(manager, BUILD_DEPENDENCIES, &guest.kernel_release);
    if !outcome.succeeded() {
        // A guest with no network is a guest whose display is degraded and
        // whose VM is fine: cloud-init provisioned it over the same NAT, so
        // this is a failure worth naming rather than one worth hiding.
        let reason = failure(&guest_packages::install_command(manager), &outcome);
        report.failed(DisplayRecipeStep::BuildDependencies, reason.clone());
        return Err(reason);
    }

    report.ok(
        DisplayRecipeStep::BuildDependencies,
        format!("installed {installed}"),
    );
    Ok(())
}

/// Whether the guest can already build a module for its own kernel.
///
/// `dkms` is looked for on `PATH` rather than at `/usr/sbin/dkms`: that is
/// Debian's placing of it, Arch's is `/usr/bin`, and the written-out path
/// finds nothing on the second one. The headers are still a path, because that
/// is where a kernel's build tree is on every distribution that ships one.
fn dependencies_are_present(kernel_release: &str) -> bool {
    program_on_path("dkms") && Path::new(&format!("/lib/modules/{kernel_release}/build")).exists()
}

/// Copies the payload's sources where DKMS can write beside them.
fn source_stage(report: &mut Report, payload: &PayloadFacts) -> Result<(), String> {
    let source = Path::new(PAYLOAD_MOUNT).join("content").join("drm");
    let destination = PathBuf::from(payload.source_directory());

    match copy_tree(&source, &destination) {
        Ok(true) => {
            report.ok(
                DisplayRecipeStep::ModuleSource,
                format!("staged {DKMS_PACKAGE} sources at {}", destination.display()),
            );
            Ok(())
        }
        Ok(false) => {
            report.skipped(
                DisplayRecipeStep::ModuleSource,
                format!("{} already holds these sources", destination.display()),
            );
            Ok(())
        }
        Err(error) => {
            let reason = format!("{} could not be staged: {error}", destination.display());
            report.failed(DisplayRecipeStep::ModuleSource, reason.clone());
            Err(reason)
        }
    }
}

/// Builds and installs the module for the running kernel.
fn build_stage(report: &mut Report, version: &str, kernel_release: &str) -> Result<(), String> {
    let status = command::run("dkms", &["status"], &[], SHORT_BUDGET);
    if dkms_reports_installed(&status.output, DKMS_PACKAGE, version, kernel_release) {
        report.skipped(
            DisplayRecipeStep::ModuleBuild,
            format!("{DKMS_PACKAGE} {version} is already installed for kernel {kernel_release}"),
        );
        return Ok(());
    }

    // `dkms add` fails when the package is already registered, which is the
    // ordinary state of a guest whose sources were staged by an earlier
    // session. The build is what decides, so its failure is the only one
    // reported.
    let _ = command::run(
        "dkms",
        &["add", "-m", DKMS_PACKAGE, "-v", version],
        &[],
        SHORT_BUDGET,
    );

    for (step, budget) in [("build", BUILD_BUDGET), ("install", SHORT_BUDGET)] {
        let outcome = command::run(
            "dkms",
            &[
                step,
                "-m",
                DKMS_PACKAGE,
                "-v",
                version,
                "-k",
                kernel_release,
            ],
            &[],
            budget,
        );
        if !outcome.succeeded() {
            let mut reason = failure(&format!("dkms {step}"), &outcome);
            if let Some(log) = make_log(version) {
                reason.push_str("\nmake.log:\n");
                reason.push_str(&log);
            }
            report.failed(DisplayRecipeStep::ModuleBuild, reason.clone());
            return Err(reason);
        }
    }

    report.ok(
        DisplayRecipeStep::ModuleBuild,
        format!("built and installed {DKMS_PACKAGE} {version} for kernel {kernel_release}"),
    );
    Ok(())
}

/// The tail of the log a failed DKMS build leaves behind.
fn make_log(version: &str) -> Option<String> {
    let log = Path::new(DKMS_TREE)
        .join(DKMS_PACKAGE)
        .join(version)
        .join("build/make.log");
    let text = fs::read_to_string(log).ok()?;
    Some(
        text.lines()
            .rev()
            .take(KEPT_LOG_LINES)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Refreshes the boot image that may carry this module and its load policy.
fn update_initramfs_stage(
    report: &mut Report,
    builder: Option<InitramfsBuilder>,
    kernel_release: &str,
) -> Result<(), String> {
    // A guest with no builder this build knows is a stage that skips rather
    // than one that ends the recipe: the module is loaded by `modprobe` in the
    // stage after this one either way, and what an unrefreshed image costs is
    // a first frame that waits for `modules-load` on the next boot.
    let Some(builder) = builder else {
        report.skipped(
            DisplayRecipeStep::Initramfs,
            "no initramfs builder this build knows is installed, \
             so the boot image keeps whatever it had",
        );
        return Ok(());
    };

    let outcome = update_initramfs(builder, kernel_release);
    if !outcome.succeeded() {
        let reason = failure(&builder.command(kernel_release), &outcome);
        report.failed(DisplayRecipeStep::Initramfs, reason.clone());
        return Err(reason);
    }
    report.ok(
        DisplayRecipeStep::Initramfs,
        format!("rebuilt initramfs for kernel {kernel_release} with {builder}"),
    );
    Ok(())
}

/// The compositor's drop-in as it is installed: what the payload ships, plus
/// the one line only the guest can answer.
///
/// The library path is not a shipped constant because it is not one: a
/// multiarch guest keeps the distribution's Mesa in `/usr/lib/<triplet>` and a
/// guest without a multiarch directory keeps it in `/usr/lib`, and a drop-in
/// naming the wrong one leaves the loader on the payload's Mesa -- which is
/// the failure this file exists to prevent, arriving silently.
///
/// Appended rather than substituted, so that an older payload still carrying
/// the line installs correctly: systemd reads a repeated `Environment=` for
/// the same variable last-wins, and the last one is this one.
fn compositor_drop_in(shipped: &str, layout: &LibraryLayout) -> String {
    let mut content = shipped.to_owned();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "Environment=LD_LIBRARY_PATH={}\n",
        layout.directory()
    ));
    content
}

/// Keeps the compositor of this guest off the payload's Mesa.
///
/// The drop-in itself is old; what is decided here is where it goes. A
/// compositor systemd started has a unit, and a drop-in on that unit's
/// template reaches it -- the greeter's instance and every logged-in user's
/// alike. A compositor a login shell started has no unit, and there is nothing
/// for a drop-in to attach to; that guest is told so rather than having a file
/// written at a path nothing reads.
///
/// Written rather than applied: a drop-in is read when the unit next starts,
/// which is the start after this one.
///
/// The way that suits a session with no unit is a wrapper, and it belongs to
/// whoever brings that desktop up (#166): the greeter launches a compositor
/// through the `Exec=` of an entry in `/usr/share/wayland-sessions`, and an
/// entry of the same name under `/usr/local/share/wayland-sessions` -- earlier
/// in `XDG_DATA_DIRS` on every guest this runs on -- can exec that command
/// through a script that sets the same two things this drop-in does. What it
/// must not do is set them for the session rather than for the compositor:
/// everything launched from the session is meant to keep the whole GPU
/// environment, which is where the acceleration was asked for.
fn compositor_isolation_stage(report: &mut Report, guest: &GuestFacts) -> Result<(), String> {
    let step = DisplayRecipeStep::CompositorIsolation;
    let user_units = Path::new(SYSTEMD_USER_UNITS);

    let Some(held_back) = compositor_is_held_back() else {
        return compositor_on_the_payloads_mesa(report, step, user_units);
    };

    let shipped = Path::new(PAYLOAD_MOUNT)
        .join("content")
        .join("drm")
        .join(COMPOSITOR_DROP_IN);
    if !shipped.exists() {
        report.skipped(step, "this display payload carries no compositor drop-in");
        return Ok(());
    }
    let content = fs::read_to_string(&shipped)
        .map(|shipped| compositor_drop_in(&shipped, &guest.library_layout))
        .map_err(|error| {
            let reason = format!("{} could not be read: {error}", shipped.display());
            report.failed(step, reason.clone());
            reason
        })?;
    let write = |report: &mut Report, path: &Path| -> Result<(), String> {
        write_if_different(path, &content).map_err(|error| {
            let reason = format!("{} could not be written: {error}", path.display());
            report.failed(step, reason.clone());
            reason
        })
    };

    let installed = installed_drop_ins(user_units);
    // Only when nothing is installed yet, because that is the only run whose
    // answer cannot wait: on every later boot the drop-in is already on disk
    // and correct before the greeter reads it.
    let launch = match installed.is_empty() {
        true => compositor_when_one_starts(&guest.desktop),
        false => guest.desktop.compositor.clone(),
    };

    match launch
        .as_ref()
        .map(|launch| (launch, launch.drop_in_directory()))
    {
        Some((launch, Some(directory))) => {
            let path = user_units.join(directory).join(COMPOSITOR_DROP_IN);
            write(report, &path)?;
            report.ok(
                step,
                format!(
                    "{held_back}, so {} keeps it on this guest's own Mesa; the compositor is {}",
                    path.display(),
                    launch.describe()
                ),
            );
        }
        Some((launch, None)) => report.skipped(
            step,
            format!(
                "{held_back}, but the compositor is {}, so there is no unit for a drop-in \
                 to attach to; this guest's compositor is isolated by whatever starts it, \
                 not from here",
                launch.describe()
            ),
        ),
        // Nothing on the screen and a drop-in already there: the guest that
        // boots this recipe ahead of its own greeter, which is the ordinary
        // case after the first. What is there is refreshed, because a payload
        // may have changed what it ships.
        None if !installed.is_empty() => {
            for path in &installed {
                write(report, path)?;
            }
            report.ok(
                step,
                format!(
                    "{held_back}; no compositor is running to be asked how it starts, so \
                     refreshed the drop-in already installed at {}",
                    installed
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        None => report.skipped(
            step,
            format!(
                "{held_back}, but no compositor has started on this guest yet, so how one \
                 is started is not known and nothing was written; the next run that finds \
                 one installs it"
            ),
        ),
    }

    Ok(())
}

/// Why this guest's compositor is kept off the payload's Mesa, or nothing when
/// it belongs on it.
///
/// The adapter is asked for here rather than carried in `GuestFacts` because the
/// answer expires: the recipe ran minutes ago, and an adapter that has since
/// gone is exactly the case this is deciding for. What the two answers mean is
/// decided in `display_recipe`, out of the text this hands it.
fn compositor_is_held_back() -> Option<String> {
    compositor_isolation(
        gpu_kernel::device_is_usable(),
        &read(&Path::new(GPU_PAYLOAD).join("sources.json")),
    )
}

/// Leaves this guest's compositor on the payload's Mesa, undoing an isolation an
/// earlier run installed.
///
/// Removal rather than rewriting: the drop-in exists to name a different Mesa,
/// and a guest that wants no such naming wants no file. What is left behind is
/// the empty `.d` directory, which systemd reads as nothing and which is not
/// ours to remove -- another drop-in may live in it.
///
/// The compositor reads a drop-in when it next starts, so this reaches the
/// screen on the start after this one, exactly as installing it does.
fn compositor_on_the_payloads_mesa(
    report: &mut Report,
    step: DisplayRecipeStep,
    user_units: &Path,
) -> Result<(), String> {
    let installed = installed_drop_ins(user_units);
    if installed.is_empty() {
        report.ok(
            step,
            "this guest has an adapter and its GPU payload declares compositor-scanout, \
             so its compositor draws on the payload's Mesa; no drop-in holds it back",
        );
        return Ok(());
    }
    for path in &installed {
        fs::remove_file(path).map_err(|error| {
            let reason = format!("{} could not be removed: {error}", path.display());
            report.failed(step, reason.clone());
            reason
        })?;
    }
    report.ok(
        step,
        format!(
            "this guest has an adapter and its GPU payload declares compositor-scanout, \
             so its compositor draws on the payload's Mesa; removed the drop-in that held \
             it back at {}",
            installed
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    Ok(())
}

/// The drop-ins of ours that are already installed under the user units.
///
/// Our own file name in somebody's `.d` directory, which is the only record
/// there is of what an earlier run decided: nothing else remembers which unit
/// this guest's compositor turned out to be.
fn installed_drop_ins(user_units: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(user_units) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join(COMPOSITOR_DROP_IN))
        .filter(|path| path.is_file())
        .collect();
    found.sort();
    found
}

/// How the compositor is started, waiting for one where one is coming.
///
/// A recipe that runs before the greeter has nothing to look at, and a greeter
/// that a display manager is about to start is worth the wait: the alternative
/// is a first session that comes up on the payload's Mesa and stays black
/// until it is started again. Bounded, and only ever waited for on a guest
/// with a display manager already running.
fn compositor_when_one_starts(facts: &DesktopFacts) -> Option<CompositorLaunch> {
    if let Some(launch) = facts.compositor.clone() {
        return Some(launch);
    }
    let manager = facts.display_manager.as_deref()?;
    if !unit_is_active(manager) {
        return None;
    }

    let deadline = std::time::Instant::now() + SHORT_BUDGET;
    loop {
        // A second between looks rather than the quarter the services wait:
        // what is being waited for is a greeter starting, and each look walks
        // every process on the guest.
        std::thread::sleep(Duration::from_secs(1));
        if let Some(launch) = guest_platform::desktop_now().compositor {
            return Some(launch);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
    }
}

fn update_initramfs(builder: InitramfsBuilder, kernel_release: &str) -> command::Outcome {
    let arguments = builder.arguments(kernel_release);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    command::run(builder.program(), &arguments, &[], BUILD_BUDGET)
}

/// Loads the module now, and arranges for it on every boot after this one.
///
/// The unit that unbinds `simple-framebuffer` is installed here too: simpledrm
/// is builtin, so it cannot be blacklisted, and until it lets go of the console
/// a compositor has two devices to choose between.
///
/// Keeping the compositor off the payload's Mesa is the other half of the same
/// job -- a device a compositor binds and then cannot allocate a buffer on is a
/// device it will not light -- and it is its own stage, because how it is
/// delivered depends on something this one never asks: how the compositor of
/// this guest is started.
fn load_stage(
    report: &mut Report,
    mode: Option<(u32, u32)>,
    refresh_initramfs_for: Option<(Option<InitramfsBuilder>, &str)>,
    signing: Option<&GuestSigning>,
) -> Result<(), String> {
    let wanted = wanted_mode(mode);
    let payload_drm = Path::new(PAYLOAD_MOUNT).join("content").join("drm");
    let install = |from: &str, to: &str, finish: &dyn Fn(String) -> String| -> Result<(), String> {
        let source = payload_drm.join(from);
        if !source.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&source)
            .map_err(|error| format!("{} could not be read: {error}", source.display()))?;
        write_if_different(Path::new(to), &finish(content))
            .map_err(|error| format!("{to} could not be written: {error}"))
    };
    let copy = |from: &str, to: &str| install(from, to, &|content| content);

    let prepared = write_if_different(Path::new(MODULES_LOAD), &format!("{MODULE}\n"))
        .map_err(|error| format!("{MODULES_LOAD} could not be written: {error}"))
        .and_then(|()| {
            write_if_different(
                Path::new(MODPROBE_OPTIONS),
                &modprobe_options(wanted.0, wanted.1),
            )
            .map_err(|error| format!("{MODPROBE_OPTIONS} could not be written: {error}"))
        })
        .and_then(|()| copy("vmlord-display-unbind-simpledrm.service", UNBIND_UNIT))
        .and_then(|()| copy("62-vmlord-display.rules", UDEV_RULES));
    if let Err(reason) = prepared {
        report.failed(DisplayRecipeStep::ModuleLoad, reason.clone());
        return Err(reason);
    }
    let _ = command::run(
        "systemctl",
        &["enable", "--now", "vmlord-display-unbind-simpledrm.service"],
        &[],
        SHORT_BUDGET,
    );

    if let Some((builder, kernel_release)) = refresh_initramfs_for {
        update_initramfs_stage(report, builder, kernel_release)?;
    }

    if module_is_loaded(&read(Path::new("/proc/modules"))) {
        let loaded = parse_module_parameters(
            &read(Path::new(MODULE_PARAM_WIDTH)),
            &read(Path::new(MODULE_PARAM_HEIGHT)),
        );
        if needs_reload(loaded, wanted) {
            // The stored mode changed under a module that is already up, and a
            // module parameter is read once. A reload that fails is a failed
            // stage and a degraded display -- and a VM that keeps running.
            return reload_module(report, signing);
        }
        report.skipped(
            DisplayRecipeStep::ModuleLoad,
            format!("{MODULE} is loaded at {}x{}", wanted.0, wanted.1),
        );
        return Ok(());
    }

    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if !outcome.succeeded() {
        let reason = load_failure(&outcome, signing);
        report.failed(DisplayRecipeStep::ModuleLoad, reason.clone());
        return Err(reason);
    }

    report.ok(
        DisplayRecipeStep::ModuleLoad,
        format!(
            "loaded {MODULE} at {}x{} and asked for it on every boot",
            wanted.0, wanted.1
        ),
    );
    Ok(())
}

/// Unloads whatever is running and loads what was just installed.
fn reload_module(report: &mut Report, signing: Option<&GuestSigning>) -> Result<(), String> {
    let _ = command::run("modprobe", &["-r", MODULE], &[], SHORT_BUDGET);
    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if !outcome.succeeded() {
        let reason = load_failure(&outcome, signing);
        report.failed(DisplayRecipeStep::ModuleLoad, reason.clone());
        return Err(reason);
    }
    report.ok(DisplayRecipeStep::ModuleLoad, format!("reloaded {MODULE}"));
    Ok(())
}

/// Reloads an updated module, or preserves it on disk for the next boot when
/// the compositor still owns the currently loaded version.
fn reload_module_for_update(
    report: &mut Report,
    target_version: &str,
    signing: Option<&GuestSigning>,
) -> Result<(), UpdateAttemptFailure> {
    let unloaded = command::run("modprobe", &["-r", MODULE], &[], SHORT_BUDGET);
    let still_loaded = module_is_loaded(&read(Path::new("/proc/modules")));
    if reload_disposition(unloaded.succeeded(), still_loaded) == ReloadDisposition::RebootRequired {
        let reason = format!(
            "{MODULE} is still in use; display payload {target_version} is installed and the guest must reboot to load it"
        );
        report.failed(DisplayRecipeStep::ModuleLoad, reason.clone());
        return Err(UpdateAttemptFailure::RebootRequired(reason));
    }

    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if !outcome.succeeded() {
        let reason = load_failure(&outcome, signing);
        report.failed(DisplayRecipeStep::ModuleLoad, reason.clone());
        return Err(UpdateAttemptFailure::Failed(reason));
    }
    report.ok(DisplayRecipeStep::ModuleLoad, format!("reloaded {MODULE}"));
    Ok(())
}

fn device_stage(report: &mut Report) -> Result<(), String> {
    if !device_is_present() {
        let reason =
            format!("{MODULE} is loaded and no display device appeared under {DRM_DEVICES}");
        report.failed(DisplayRecipeStep::Device, reason.clone());
        return Err(reason);
    }
    keep_the_desktop_on_this_output();
    report.ok(
        DisplayRecipeStep::Device,
        format!("a {MODULE} display device is present"),
    );
    Ok(())
}

/// Asks udev to look at the display cards again, now that this one is here.
///
/// The rule that hides the Hyper-V card is written for a guest where this
/// module is loaded, and it says so with a `TEST` on this device. At boot the
/// synthetic card is there long before the module is, so the rule ran and
/// found nothing; this is what makes it run again while the answer is yes.
/// Before any compositor starts, because a tag is read when a card is
/// enumerated and not after.
///
/// Nothing here fails a stage: a guest whose udev refused is a guest with a
/// second monitor nobody can see, which is worse than one monitor and better
/// than no display.
fn keep_the_desktop_on_this_output() {
    let _ = command::run("udevadm", &["control", "--reload"], &[], SHORT_BUDGET);
    let _ = command::run(
        "udevadm",
        &["trigger", "--subsystem-match=drm", "--action=change"],
        &[],
        SHORT_BUDGET,
    );
}

/// Checks the update did what it said: the target version loaded, on a device
/// that exists.
fn verify(report: &mut Report, target_version: &str) -> Result<(), String> {
    device_stage(report)?;
    match loaded_version() {
        Some(loaded) if loaded == target_version => {}
        Some(loaded) => {
            return Err(format!(
                "{MODULE} {loaded} is loaded, and the update installed {target_version}"
            ));
        }
        None => return Err(format!("{MODULE} does not say which version is loaded")),
    }

    verify_services()
}

/// The services half of a verification.
///
/// A failed verification is what makes an update roll back, so a payload whose
/// services do not come up rolls back rather than being declared installed. A
/// payload that carries none is not a failure: it is every payload built before
/// this task.
fn verify_services() -> Result<(), String> {
    let services = payload_services();
    let carried = fs::read_dir(&services)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if !carried {
        return Ok(());
    }

    if services_need_install(&services, Path::new(SERVICES_INSTALL)) {
        return Err(format!(
            "the display services in {SERVICES_INSTALL} are not the ones this payload carries"
        ));
    }
    for unit in SYSTEM_UNITS {
        if !unit_is_active(unit) {
            return Err(format!("{unit} is not running after the update"));
        }
    }

    Ok(())
}

/// Installs the two guest programs and their units, and starts them.
///
/// A payload that carries no services is skipped and never failed, with the
/// reason said out loud: every payload built before this task is one of those,
/// and reporting it as a failure would make every such guest degraded.
///
/// # Errors
///
/// The reason the stage failed, which ends the recipe and leaves the display
/// degraded while the VM keeps running.
fn services_stages(
    report: &mut Report,
    guest: &GuestFacts,
    services: &Path,
    installed: &Path,
) -> Result<(), String> {
    let carried: Vec<String> = fs::read_dir(services)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if carried.is_empty() {
        let reason = "this payload carries no display services";
        report.skipped(DisplayRecipeStep::Services, reason);
        report.skipped(DisplayRecipeStep::ServicesStart, reason);

        return Ok(());
    }

    if let Err(reason) = install_services(report, guest, services, installed) {
        report.failed(DisplayRecipeStep::Services, reason.clone());

        return Err(reason);
    }

    if let Err(reason) = start_services(report) {
        report.failed(DisplayRecipeStep::ServicesStart, reason.clone());

        return Err(reason);
    }

    Ok(())
}

/// Puts the binaries and the units where systemd will find them.
fn install_services(
    report: &mut Report,
    guest: &GuestFacts,
    services: &Path,
    installed: &Path,
) -> Result<(), String> {
    ensure_service_user()?;

    if !services_need_install(services, installed) {
        report.skipped(
            DisplayRecipeStep::Services,
            format!(
                "the display services in {} are what this payload carries",
                installed.display()
            ),
        );

        return Ok(());
    }

    fs::create_dir_all(installed)
        .map_err(|error| format!("{} could not be created: {error}", installed.display()))?;
    for binary in SERVICE_BINARIES {
        install_file(&services.join(binary), &installed.join(binary), 0o755)?;
    }
    for unit in SYSTEM_UNITS {
        install_file(
            &services.join(unit),
            &Path::new(SYSTEMD_UNITS).join(unit),
            0o644,
        )?;
    }
    for unit in USER_UNITS {
        install_file(
            &services.join(unit),
            &Path::new(SYSTEMD_USER_UNITS).join(unit),
            0o644,
        )?;
    }

    let reloaded = command::run("systemctl", &["daemon-reload"], &[], SHORT_BUDGET);
    if !reloaded.succeeded() {
        return Err(failure("systemctl daemon-reload", &reloaded));
    }
    for unit in SYSTEM_UNITS {
        let enabled = command::run("systemctl", &["enable", unit], &[], SHORT_BUDGET);
        if !enabled.succeeded() {
            return Err(failure(&format!("systemctl enable {unit}"), &enabled));
        }
    }
    // `--global` rather than `--user`: this recipe runs as root and outside any
    // session, and the unit has to be wanted by whichever session starts next.
    // Enabling it per user would mean knowing a name that is not decided until
    // somebody logs in.
    for unit in USER_UNITS {
        let enabled = command::run(
            "systemctl",
            &["--global", "enable", unit],
            &[],
            SHORT_BUDGET,
        );
        if !enabled.succeeded() {
            return Err(failure(
                &format!("systemctl --global enable {unit}"),
                &enabled,
            ));
        }
    }

    install_audio(&payload_audio())?;
    let appindicator = install_appindicator_extension(guest);

    report.ok(
        DisplayRecipeStep::Services,
        format!(
            "installed the display services in {} and asked for them on every boot{}",
            installed.display(),
            appindicator.map_or_else(String::new, |note| format!("; {note}")),
        ),
    );

    Ok(())
}

/// Puts the loopback's configuration where the kernel and WirePlumber read it.
///
/// Part of the services step rather than a step of its own: what it installs is
/// what makes one of those services able to do anything, and a guest that has
/// the audio daemon without the loopback is not in a different state worth
/// reporting separately -- the daemon says so itself, on the channel.
///
/// A payload that carries no audio directory is an older one, and an older
/// payload is allowed to carry fewer things than this build knows about.
fn install_audio(audio: &Path) -> Result<(), String> {
    if !audio.is_dir() {
        return Ok(());
    }

    install_file(
        &audio.join("vmlord-audio-modules.conf"),
        Path::new(LOOPBACK_MODULES_LOAD),
        0o644,
    )?;
    install_file(
        &audio.join("vmlord-audio-modprobe.conf"),
        Path::new(LOOPBACK_MODPROBE),
        0o644,
    )?;
    // Without this the desktop has no output at all while the daemon holds the
    // loopback: PipeWire's ALSA monitor drops a card whose every PCM device is
    // not free, and the daemon holds one for the life of a session.
    install_file(&audio.join(AUDIO_SINK), Path::new(PIPEWIRE_DROP_IN), 0o644)?;
    if let Some((rule, destination)) = wireplumber_rule(Path::new(WIREPLUMBER_SHARE)) {
        install_file(&audio.join(rule), &destination, 0o644)?;
    }

    // Loaded now as well as asked for on every boot: the first session after
    // provisioning should have sound without the guest being rebooted for it.
    // A failure here is not fatal -- `modules-load.d` will do it next boot --
    // so it is not reported as one.
    let _ = command::run("modprobe", &[LOOPBACK_MODULE], &[], SHORT_BUDGET);

    Ok(())
}

/// Which form of the WirePlumber rule this guest reads, and where it goes.
///
/// WirePlumber 0.5 reads SPA-JSON drop-ins and 0.4 reads Lua, and the two
/// releases in the compatibility matrix straddle that change. Deciding by the
/// directory the guest's own WirePlumber ships means the answer comes from the
/// guest rather than from a version string this recipe would have to parse.
/// A guest with neither directory has no WirePlumber, and gets no file: a rule
/// nothing reads is a file somebody will one day have to explain.
fn wireplumber_rule(share: &Path) -> Option<(&'static str, PathBuf)> {
    if share.join("wireplumber.conf.d").is_dir() {
        return Some((
            LOOPBACK_RULE_JSON,
            Path::new("/etc/wireplumber/wireplumber.conf.d").join(LOOPBACK_RULE_JSON),
        ));
    }
    if share.join("main.lua.d").is_dir() {
        return Some((
            LOOPBACK_RULE_LUA,
            Path::new("/etc/wireplumber/main.lua.d").join(LOOPBACK_RULE_LUA),
        ));
    }

    None
}

/// Where the mounted payload keeps the loopback's configuration.
fn payload_audio() -> PathBuf {
    Path::new(PAYLOAD_MOUNT).join("content").join("audio")
}

/// Installs a StatusNotifierItem extension for the tray icon to show
/// through, as a distro package.
///
/// Best effort and never a failed stage: what a guest is missing without it
/// is the tray icon, not the desktop. Skipped where there is no GNOME shell
/// to extend, and where an extension -- Ubuntu's or the upstream one -- is
/// already on disk, which is every supported guest with its desktop
/// installed; the apt call exists for the guest whose desktop install
/// predates the tray. Enabling the extension is left to the session: the
/// tray, which lives there, asks the running shell when it starts, which is
/// the one way to add a UUID to `org.gnome.shell enabled-extensions` without
/// a root-side write clobbering the desktop's defaults or the user's own.
///
/// What comes back is the note the Services stage's line carries when the
/// install failed -- a report, not a crash, because the stage that follows
/// does not depend on the answer.
fn install_appindicator_extension(guest: &GuestFacts) -> Option<String> {
    if !appindicator_is_needed(&guest.desktop, Path::new(SHELL_EXTENSIONS)) {
        return None;
    }

    let Some(manager) = guest.package_manager else {
        return Some(guest_packages::no_manager(APPINDICATOR));
    };

    let (outcome, package) = guest_packages::install(manager, APPINDICATOR, &guest.kernel_release);
    (!outcome.succeeded()).then(|| {
        format!(
            "the {package} package could not be installed, and the guest's tray \
             icon has nothing to show through: {}",
            failure(&guest_packages::install_command(manager), &outcome)
        )
    })
}

/// Whether the desktop found here needs an AppIndicator extension it has not
/// got.
///
/// The desktop that was *found*, never the one the VM was created asking for:
/// a session that shows StatusNotifierItems itself pays nothing for this
/// stage, and a guest whose desktop was replaced after creation is asked about
/// as it is now. A guest with no desktop at all -- headless, or one whose
/// desktop install has not landed yet -- is the same answer for the same
/// reason: there is nothing on screen for an extension to show through.
fn appindicator_is_needed(desktop: &DesktopFacts, extensions: &Path) -> bool {
    desktop.is_gnome()
        && !APPINDICATOR_UUIDS
            .iter()
            .any(|uuid| extensions.join(uuid).is_dir())
}

/// Restarts both units and waits until they are actually up.
fn start_services(report: &mut Report) -> Result<(), String> {
    // The user unit is not restarted and not waited for: it starts with a
    // graphical session, and this recipe runs as root while there may be no
    // session at all. A guest with nobody logged in has no clipboard yet, which
    // is not a failure of the installation.
    for unit in SYSTEM_UNITS {
        let restarted = command::run("systemctl", &["restart", unit], &[], SHORT_BUDGET);
        if !restarted.succeeded() {
            return Err(failure(&format!("systemctl restart {unit}"), &restarted));
        }
    }

    // `restart` returns when systemd has started the process, not when the two
    // halves have met. What proves they have is the socket between them.
    let deadline = std::time::Instant::now() + SHORT_BUDGET;
    loop {
        if SYSTEM_UNITS.iter().all(|unit| unit_is_active(unit)) && Path::new(BROKER_SOCKET).exists()
        {
            report.ok(
                DisplayRecipeStep::ServicesStart,
                "the display broker and session are running".to_owned(),
            );

            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the display services did not come up within {} seconds",
                SHORT_BUDGET.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Whether a unit is running right now.
fn unit_is_active(unit: &str) -> bool {
    command::run(
        "systemctl",
        &["is-active", "--quiet", unit],
        &[],
        SHORT_BUDGET,
    )
    .succeeded()
}

/// Makes sure the account the unprivileged half runs as exists.
///
/// A `useradd` that fails because the account is already there is not a
/// failure; a `getent` that then still cannot find it is.
fn ensure_service_user() -> Result<(), String> {
    if command::run("getent", &["passwd", SERVICE_USER], &[], SHORT_BUDGET).succeeded() {
        return Ok(());
    }

    let _ = command::run(
        "useradd",
        &[
            "--system",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
            SERVICE_USER,
        ],
        &[],
        SHORT_BUDGET,
    );
    if command::run("getent", &["passwd", SERVICE_USER], &[], SHORT_BUDGET).succeeded() {
        return Ok(());
    }

    Err(format!(
        "the {SERVICE_USER} account could not be created, and the display session will not run as root"
    ))
}

/// Whether what the payload carries differs from what is installed.
///
/// By content rather than by timestamp: a payload is unpacked fresh every time,
/// so every file's mtime is new and nothing would ever be skipped. A file the
/// payload does not carry is not a reason to reinstall -- an older payload is
/// allowed to carry fewer things than this build knows about.
fn services_need_install(services: &Path, installed: &Path) -> bool {
    let Ok(entries) = fs::read_dir(services) else {
        return false;
    };

    entries.filter_map(Result::ok).any(|entry| {
        let Ok(carried) = fs::read(entry.path()) else {
            return false;
        };
        match fs::read(installed.join(entry.file_name())) {
            Ok(there) => sha256_hex(&there) != sha256_hex(&carried),
            // Nothing installed under that name is something to install.
            Err(_) => true,
        }
    })
}

/// Copies one file and gives it the mode it is meant to have.
fn install_file(from: &Path, to: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let bytes =
        fs::read(from).map_err(|error| format!("{} could not be read: {error}", from.display()))?;
    if let Some(directory) = to.parent() {
        fs::create_dir_all(directory)
            .map_err(|error| format!("{} could not be created: {error}", directory.display()))?;
    }
    // Removed first: a running binary cannot be written over, but it can be
    // replaced, and systemd will start the new one on the next restart.
    let _ = fs::remove_file(to);
    fs::write(to, &bytes)
        .map_err(|error| format!("{} could not be written: {error}", to.display()))?;
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("{} could not be given mode {mode:o}: {error}", to.display()))?;

    Ok(())
}

/// Where the mounted payload keeps the guest services.
fn payload_services() -> PathBuf {
    Path::new(PAYLOAD_MOUNT).join("content").join("services")
}

/// Whether a DRM device belonging to this module is there.
///
/// By driver name rather than by device number: a guest that also has
/// `hyperv_drm` has a `/dev/dri/card0` that is not ours.
fn device_is_present() -> bool {
    let Ok(entries) = fs::read_dir(DRM_DEVICES) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let driver = entry.path().join("device").join("driver");
        fs::read_link(&driver)
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .is_some_and(|name| name == MODULE)
    })
}

fn loaded_version() -> Option<String> {
    parse_module_version(&read(Path::new(MODULE_VERSION)))
}

fn installed_versions() -> InstalledVersions {
    let status = command::run("dkms", &["status"], &[], SHORT_BUDGET);
    InstalledVersions {
        versions: dkms_versions(&status.output, DKMS_PACKAGE),
        loaded: module_is_loaded(&read(Path::new("/proc/modules")))
            .then(loaded_version)
            .flatten(),
    }
}

/// Stops the recipe when the guest is going down.
///
/// A kernel build is minutes long, and systemd is holding the guest open for
/// this process to exit.
fn halted(stopping: &AtomicBool) -> Result<(), String> {
    if stopping.load(Ordering::Relaxed) {
        return Err("the guest is shutting down".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use vmlord_agent_protocol::v1::{DisplayRecipeStageState, DisplayRecipeStep};

    use super::{
        COMPOSITOR_DROP_IN, SYSTEMD_USER_UNITS, apply, compositor_drop_in,
        compositor_on_the_payloads_mesa, installed_drop_ins, load_failure_message, sha256_hex,
        update, verify_declared_files,
    };
    use crate::display_recipe::{PayloadFacts, Report, STEPS};
    use crate::guest_platform::{
        CompositorLaunch, DesktopFacts, GuestFacts, InitramfsBuilder, LibraryLayout, PackageManager,
    };

    use super::reported_desktop;

    /// An ordinary Ubuntu guest, for the stages that now ask what it is.
    fn guest() -> GuestFacts {
        GuestFacts {
            distribution: "ubuntu".to_owned(),
            release: "26.04".to_owned(),
            architecture: "amd64".to_owned(),
            kernel_release: "7.0.0-14-generic".to_owned(),
            package_manager: Some(PackageManager::Apt),
            initramfs_builder: Some(InitramfsBuilder::UpdateInitramfs),
            library_layout: LibraryLayout::Multiarch("x86_64-linux-gnu".to_owned()),
            desktop: DesktopFacts::default(),
        }
    }

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    fn temporary(label: &str) -> PathBuf {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vmlord-display-kernel-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn the_recipe_reports_the_desktop_it_found_and_not_one_it_was_told_about() {
        // Nothing in the agent knows what the VM was created asking for, and
        // this is the whole of what it answers with: names read out of the
        // guest, on their way to a host that holds the other half itself.
        let found = reported_desktop(&DesktopFacts {
            session: Some("gnome".to_owned()),
            session_type: Some("wayland".to_owned()),
            display_manager: Some("gdm.service".to_owned()),
            ..DesktopFacts::default()
        })
        .expect("a guest with a desktop reports one");

        assert_eq!(found.session, "gnome");
        assert_eq!(found.session_type, "wayland");
        assert_eq!(found.display_manager, "gdm.service");
    }

    #[test]
    fn a_guest_with_nothing_on_screen_reports_no_desktop_at_all() {
        // What a headless guest is, and what every guest is while its VM is
        // being built: the recipe runs before anybody has logged in.
        assert!(reported_desktop(&DesktopFacts::default()).is_none());
    }

    #[test]
    fn a_desktop_found_only_by_its_login_screen_is_still_a_desktop() {
        // The shape at provisioning time: the display manager is linked, and
        // there is no session on the screen yet.
        let found = reported_desktop(&DesktopFacts {
            display_manager: Some("gdm.service".to_owned()),
            ..DesktopFacts::default()
        })
        .expect("a linked display manager is a desktop");

        assert!(found.session.is_empty());
        assert_eq!(found.display_manager, "gdm.service");
    }

    #[test]
    fn the_compositor_drop_in_undoes_both_paths_to_the_payloads_mesa() {
        // Two paths, and a drop-in that undid only one of them was measured on
        // a live guest to change nothing: the linker cache still resolved
        // libgbm to the payload's Mesa, the compositor still failed to lock a
        // front buffer, and the connector stayed disabled. So this file has to
        // keep saying both things -- the overrides off, and a library path that
        // outranks the cache.
        let drop_in =
            include_str!("../../../payloads/display/module/vmlord-display-compositor-mesa.conf");

        for variable in [
            "GALLIUM_DRIVER",
            "MESA_LOADER_DRIVER_OVERRIDE",
            "__GLX_VENDOR_LIBRARY_NAME",
            "VK_DRIVER_FILES",
        ] {
            assert!(
                drop_in
                    .lines()
                    .any(|line| line.starts_with("UnsetEnvironment=") && line.contains(variable)),
                "{variable} is one of the overrides the compositor must not inherit"
            );
        }
        assert!(
            !drop_in.lines().any(|line| line.starts_with("Environment=")),
            "the path is the guest's answer, not the payload's: it is appended on install"
        );

        let installed = compositor_drop_in(drop_in, &guest().library_layout);

        assert!(
            installed
                .lines()
                .any(|line| line == "Environment=LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu"),
            "unsetting LD_LIBRARY_PATH is not enough: the cache has to be outranked\n{installed}"
        );
        assert!(
            installed
                .lines()
                .any(|line| line.starts_with("UnsetEnvironment=")),
            "the shipped half has to survive the append\n{installed}"
        );
    }

    #[test]
    fn the_compositor_drop_in_names_the_library_directory_this_guest_turned_out_to_have() {
        // Arch has no multiarch directory, and a drop-in naming one would put
        // the loader on a path that does not exist -- which is the payload's
        // Mesa winning again, silently, on the guest this file is meant to
        // protect.
        let drop_in =
            include_str!("../../../payloads/display/module/vmlord-display-compositor-mesa.conf");
        let installed = compositor_drop_in(drop_in, &LibraryLayout::Flat);

        assert!(
            installed
                .lines()
                .any(|line| line == "Environment=LD_LIBRARY_PATH=/usr/lib"),
            "{installed}"
        );
    }

    #[test]
    fn the_drop_in_goes_where_this_guests_compositor_would_read_it() {
        // The path this used to be a constant for, arrived at from what the
        // guest turned out to be running rather than from the name GNOME.
        let gnome = CompositorLaunch::Unit("org.gnome.Shell@wayland.service".to_owned());
        let directory = gnome
            .drop_in_directory()
            .expect("a unit has a drop-in directory");

        assert_eq!(
            Path::new(SYSTEMD_USER_UNITS)
                .join(directory)
                .join(COMPOSITOR_DROP_IN),
            Path::new(
                "/etc/systemd/user/org.gnome.Shell@.service.d/\
                 vmlord-display-compositor-mesa.conf"
            ),
            "GNOME's guests keep the file they already have, at the path they already have it"
        );
    }

    #[test]
    fn what_an_earlier_run_decided_is_read_back_off_the_disk() {
        // Nothing stores which unit this guest's compositor turned out to be.
        // The drop-in itself is the record, which is what lets a recipe that
        // runs before the greeter refresh the right file without a session to
        // look at.
        let directory =
            std::env::temp_dir().join(format!("vmlord-display-drop-ins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let installed = directory
            .join("org.gnome.Shell@.service.d")
            .join(COMPOSITOR_DROP_IN);
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, "[Service]\n").unwrap();
        // A drop-in directory of somebody else's, with nothing of ours in it.
        fs::create_dir_all(directory.join("pipewire.service.d")).unwrap();
        fs::write(directory.join("pipewire.service.d/99-other.conf"), "").unwrap();

        let found = installed_drop_ins(&directory);

        let _ = fs::remove_dir_all(&directory);
        assert_eq!(found, vec![installed]);
        assert!(
            installed_drop_ins(Path::new("/nonexistent-user-units")).is_empty(),
            "a guest with no user unit directory has no drop-in of ours, and no error either"
        );
    }

    #[test]
    fn a_guest_with_an_adapter_has_the_drop_in_taken_off_it() {
        // The drop-in's whole job is to name a Mesa other than the payload's.
        // Once the payload's Mesa can present to `vmlord_drm`, a guest with an
        // adapter wants no such naming -- so the file goes, and somebody else's
        // drop-in in the same directory stays.
        let directory = std::env::temp_dir().join(format!(
            "vmlord-display-adapter-drop-ins-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let ours = directory
            .join("org.gnome.Shell@.service.d")
            .join(COMPOSITOR_DROP_IN);
        fs::create_dir_all(ours.parent().unwrap()).unwrap();
        fs::write(&ours, "[Service]\n").unwrap();
        let theirs = directory.join("org.gnome.Shell@.service.d/99-other.conf");
        fs::write(&theirs, "").unwrap();
        let mut report = Report::new();

        compositor_on_the_payloads_mesa(
            &mut report,
            DisplayRecipeStep::CompositorIsolation,
            &directory,
        )
        .unwrap();

        let ours_is_gone = !ours.exists();
        let theirs_remains = theirs.exists();
        let _ = fs::remove_dir_all(&directory);
        assert!(
            ours_is_gone,
            "the drop-in that held the compositor back is removed"
        );
        assert!(
            theirs_remains,
            "a drop-in that is not ours is not ours to remove"
        );
        assert!(
            installed_drop_ins(Path::new("/nonexistent-user-units")).is_empty(),
            "a guest that never had one is not an error either"
        );
    }

    #[test]
    fn the_udev_rule_hides_the_synthetic_display_only_where_this_one_exists() {
        // A Hyper-V guest has a display of its own, and a compositor that finds
        // two cards lights both. The second monitor is drawn on the Hyper-V
        // console, where the viewer cannot see it, and it stretches the desktop
        // an absolute pointer is mapped across -- so the guest's cursor lands
        // well to the right of where the user pointed. Task #121 measured that.
        let rule = include_str!("../../../payloads/display/module/62-vmlord-display.rules");
        let matched = rule
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<String>();

        assert!(
            matched.contains("TAG+=\"mutter-device-ignore\""),
            "the tag is what mutter reads; nothing else here hides a card"
        );
        assert!(
            matched.contains("DRIVERS==\"hyperv_drm\""),
            "the card to hide is the synthetic one, named by its driver"
        );
        assert!(
            matched.contains("TEST==\"/sys/devices/platform/vmlord_drm.0/drm\""),
            "without this a guest whose module never built loses its only display"
        );
        assert!(
            super::UDEV_RULES.contains("/62-"),
            "the file has to sort after 61-mutter.rules, whose tag it adds to"
        );
    }

    #[test]
    fn the_payload_carries_five_programs_and_two_of_them_are_user_units() {
        assert_eq!(super::SERVICE_BINARIES.len(), 5);
        assert!(
            super::SERVICE_BINARIES.contains(&"vmlord-display-clipboard"),
            "the clipboard daemon ships with the others"
        );
        assert!(
            super::SERVICE_BINARIES.contains(&"vmlord-display-tray"),
            "the tray ships with the others"
        );
        assert_eq!(super::SYSTEM_UNITS.len(), 3);
        assert_eq!(
            super::USER_UNITS,
            [
                "vmlord-display-clipboard.service",
                "vmlord-display-tray.service",
            ]
        );
        assert_eq!(super::SYSTEMD_USER_UNITS, "/etc/systemd/user");
    }

    #[test]
    fn the_tray_unit_starts_and_dies_with_the_session() {
        // The tray is enabled the way the clipboard is -- for whichever
        // session starts next -- and its unit says why it is a user unit.
        let unit = include_str!("../../../payloads/display/services/vmlord-display-tray.service");

        assert!(
            unit.contains("WantedBy=graphical-session.target"),
            "the tray is wanted by the session, not by the boot"
        );
        assert!(unit.contains("/usr/local/lib/vmlord/vmlord-display-tray"));
    }

    #[test]
    fn an_appindicator_extension_is_asked_for_only_where_one_is_missing() {
        let installed = temporary("appindicator-guest");
        fs::create_dir_all(installed.join("ubuntu-appindicators@ubuntu.com")).unwrap();
        let missing = temporary("appindicator-none");
        let gnome = DesktopFacts {
            session: Some("ubuntu".to_owned()),
            session_type: Some("wayland".to_owned()),
            display_manager: Some("gdm3.service".to_owned()),
            ..DesktopFacts::default()
        };

        assert!(
            !super::appindicator_is_needed(&gnome, &installed),
            "Ubuntu's own fork on disk is every supported desktop guest"
        );
        assert!(
            super::appindicator_is_needed(&gnome, &missing),
            "a GNOME desktop with no extension on disk needs the package"
        );
        assert!(
            !super::appindicator_is_needed(
                &DesktopFacts {
                    session: Some("Hyprland".to_owned()),
                    session_type: Some("wayland".to_owned()),
                    display_manager: None,
                    ..DesktopFacts::default()
                },
                &missing
            ),
            "a session that shows StatusNotifierItems itself pays nothing for \
             GNOME's extension"
        );
        assert!(
            !super::appindicator_is_needed(&DesktopFacts::default(), &missing),
            "there is nothing to extend on a guest with no desktop"
        );
    }

    #[test]
    fn the_audio_daemon_is_a_system_unit_and_not_a_user_one() {
        assert!(super::SERVICE_BINARIES.contains(&"vmlord-display-audio"));
        assert!(super::SYSTEM_UNITS.contains(&"vmlord-display-audio.service"));
        // Sound belongs to the machine rather than to whoever is at the
        // screen, and the stream has to exist before anybody logs in.
        assert!(!super::USER_UNITS.contains(&"vmlord-display-audio.service"));
    }

    #[test]
    fn the_desktops_output_is_a_static_node_rather_than_a_monitored_card() {
        // The one that must not be forgotten: PipeWire's ALSA monitor refuses
        // a card while any of its PCM devices is busy, and the audio daemon
        // holds one for the life of a session. A guest without this file has
        // a Dummy Output and no way to play anything at all.
        assert_eq!(super::AUDIO_SINK, "51-vmlord-audio.conf");
        assert!(super::PIPEWIRE_DROP_IN.starts_with("/etc/pipewire/pipewire.conf.d/"));
    }

    #[test]
    fn the_wireplumber_rule_is_installed_in_whichever_form_the_guest_reads() {
        // 0.5 reads SPA-JSON drop-ins and 0.4 reads Lua. The payload carries
        // both, and which one is installed is decided by which directory the
        // guest's own WirePlumber ships.
        let guest = temporary("wireplumber-guest");
        fs::create_dir_all(guest.join("wireplumber.conf.d")).unwrap();

        let chosen = super::wireplumber_rule(&guest);

        assert_eq!(
            chosen,
            Some((
                super::LOOPBACK_RULE_JSON,
                std::path::PathBuf::from("/etc/wireplumber/wireplumber.conf.d")
                    .join(super::LOOPBACK_RULE_JSON)
            ))
        );
    }

    #[test]
    fn an_older_wireplumber_gets_the_lua_rule_instead() {
        let guest = temporary("wireplumber-old-guest");
        fs::create_dir_all(guest.join("main.lua.d")).unwrap();

        let chosen = super::wireplumber_rule(&guest);

        assert_eq!(
            chosen,
            Some((
                super::LOOPBACK_RULE_LUA,
                std::path::PathBuf::from("/etc/wireplumber/main.lua.d")
                    .join(super::LOOPBACK_RULE_LUA)
            ))
        );
    }

    #[test]
    fn a_guest_without_wireplumber_gets_no_rule_rather_than_a_stray_file() {
        let guest = temporary("wireplumber-absent-guest");

        assert_eq!(super::wireplumber_rule(&guest), None);
    }

    #[test]
    fn services_that_are_already_installed_are_not_copied_again() {
        let payload = temporary("services-payload");
        let installed = temporary("services-installed");
        fs::write(payload.join("vmlord-display-broker"), b"binary").unwrap();
        fs::write(installed.join("vmlord-display-broker"), b"binary").unwrap();

        assert!(
            !super::services_need_install(&payload, &installed),
            "a guest already running what the payload carries needs no copy"
        );
    }

    #[test]
    fn a_service_whose_bytes_differ_is_reinstalled() {
        let payload = temporary("services-changed-payload");
        let installed = temporary("services-changed-installed");
        fs::write(payload.join("vmlord-display-broker"), b"new").unwrap();
        fs::write(installed.join("vmlord-display-broker"), b"old").unwrap();

        assert!(super::services_need_install(&payload, &installed));
    }

    #[test]
    fn a_service_that_is_not_installed_at_all_is_installed() {
        let payload = temporary("services-absent-payload");
        let installed = temporary("services-absent-installed");
        fs::write(payload.join("vmlord-display-broker"), b"new").unwrap();

        assert!(super::services_need_install(&payload, &installed));
    }

    #[test]
    fn a_payload_that_carries_no_services_still_skips_rather_than_fails() {
        // Every payload built before this task is one of these, and a failure
        // here would make every such guest degraded.
        let payload = temporary("services-empty-payload");
        let installed = temporary("services-empty-installed");
        let mut report = crate::display_recipe::Report::new();

        assert!(super::services_stages(&mut report, &guest(), &payload, &installed).is_ok());
        let stages = report.finish("the recipe did not need this stage");

        assert!(
            stages
                .iter()
                .filter(|stage| matches!(
                    stage.step(),
                    DisplayRecipeStep::Services | DisplayRecipeStep::ServicesStart
                ))
                .all(|stage| stage.state() == DisplayRecipeStageState::Skipped)
        );
    }

    #[test]
    fn the_source_tree_is_versioned_so_two_versions_can_coexist() {
        let facts = PayloadFacts {
            payload_id: "display-ubuntu-24.04-amd64-0.2.0".into(),
            version: "0.2.0".into(),
            distribution: "ubuntu".into(),
            release: "24.04".into(),
            architecture: "amd64".into(),
        };

        assert_eq!(facts.source_directory(), "/usr/src/vmlord-display-0.2.0");
    }

    #[test]
    fn a_declared_file_whose_digest_does_not_match_fails_verification() {
        let root = temporary("digests");
        fs::write(root.join("marker"), b"one").unwrap();
        let manifest = format!(
            r#"{{"files":[{{"path":"marker","size":3,"sha256":"{}"}}]}}"#,
            sha256_hex(b"one")
        );

        assert!(verify_declared_files(manifest.as_bytes(), &root).is_ok());

        fs::write(root.join("marker"), b"two").unwrap();
        let error = verify_declared_files(manifest.as_bytes(), &root)
            .expect_err("the mount was rewritten under us, which is the case this exists for");
        assert!(error.contains("marker"));
    }

    #[test]
    fn a_declared_file_that_is_not_there_fails_verification() {
        let root = temporary("absent");
        let manifest = r#"{"files":[{"path":"content/drm/Kbuild","size":1,"sha256":"00"}]}"#;

        assert!(verify_declared_files(manifest.as_bytes(), &root).is_err());
    }

    #[test]
    fn a_guest_that_is_going_down_gets_no_further_stages() {
        assert!(
            super::halted(&AtomicBool::new(true)).is_err(),
            "systemd is holding the guest open for this process to exit"
        );
        assert!(super::halted(&AtomicBool::new(false)).is_ok());
    }

    #[test]
    fn a_recipe_with_no_payload_mounted_builds_nothing_and_still_reports_every_step() {
        // This machine is not a VMLord guest: nothing is mounted at
        // /opt/vmlord/display-payload, which is the failure a guest whose share
        // never arrived would hit.
        let outcome = apply(&AtomicBool::new(false), None);
        let (stages, versions) = (outcome.stages, outcome.versions);

        assert_eq!(stages.len(), crate::display_recipe::STEPS.len());
        assert!(
            stages
                .iter()
                .any(|stage| stage.step() == DisplayRecipeStep::Payload
                    && stage.state() == DisplayRecipeStageState::Failed),
            "a payload that cannot be read is where the recipe stops"
        );
        assert!(
            stages
                .iter()
                .filter(|stage| matches!(
                    stage.step(),
                    DisplayRecipeStep::ModuleSource
                        | DisplayRecipeStep::ModuleBuild
                        | DisplayRecipeStep::ModuleLoad
                ))
                .all(|stage| stage.state() == DisplayRecipeStageState::Skipped),
            "nothing is built out of a payload that was never verified"
        );
        assert!(
            versions.installed.is_empty(),
            "a machine with no DKMS package installed says so"
        );
    }

    #[test]
    fn an_update_with_no_payload_mounted_fails_at_the_payload_stage() {
        let (stages, _, outcome) = update("9.9.9", &AtomicBool::new(false));

        assert_eq!(
            outcome,
            vmlord_agent_protocol::v1::DisplayUpdateOutcome::Failed
        );
        assert!(
            stages
                .iter()
                .any(|stage| stage.step() == DisplayRecipeStep::Payload
                    && stage.state() == DisplayRecipeStageState::Failed),
            "an update to a version the mount does not carry changes nothing"
        );
    }

    #[test]
    fn a_guest_with_no_initramfs_builder_skips_that_stage_rather_than_stopping() {
        let mut report = Report::new();

        let outcome = super::update_initramfs_stage(&mut report, None, "7.0.0-30-generic");

        assert!(outcome.is_ok(), "a missing builder does not end the recipe");
        let stages = report.finish("the recipe stopped before this stage");
        let initramfs = stages
            .iter()
            .find(|stage| stage.step() == DisplayRecipeStep::Initramfs)
            .expect("the report names every step");
        assert_eq!(initramfs.state(), DisplayRecipeStageState::Skipped);
        assert!(
            initramfs.message.contains("no initramfs builder"),
            "{}",
            initramfs.message
        );
    }

    #[test]
    fn a_module_that_remains_loaded_after_unload_failed_needs_a_reboot() {
        assert_eq!(
            super::reload_disposition(false, true),
            super::ReloadDisposition::RebootRequired
        );
        assert_eq!(
            super::reload_disposition(true, false),
            super::ReloadDisposition::Reload
        );
    }

    #[test]
    fn a_reboot_pending_report_distinguishes_installed_from_loaded() {
        let versions = vmlord_agent_protocol::v1::DisplayPayloadVersions {
            installed: "0.1.0".to_owned(),
            previous: "0.2.0".to_owned(),
            loaded: "0.1.0".to_owned(),
        };

        let reported = super::reported_update_versions(
            "0.2.0",
            vmlord_agent_protocol::v1::DisplayUpdateOutcome::RebootRequired,
            versions,
        );

        assert_eq!(reported.installed, "0.2.0");
        assert_eq!(reported.previous, "0.1.0");
        assert_eq!(reported.loaded, "0.1.0");
    }

    #[test]
    fn a_running_previous_module_is_a_rollback_even_if_boot_refresh_failed() {
        assert_eq!(
            super::rollback_outcome(true),
            vmlord_agent_protocol::v1::DisplayUpdateOutcome::RolledBack
        );
        assert_eq!(
            super::rollback_outcome(false),
            vmlord_agent_protocol::v1::DisplayUpdateOutcome::Failed
        );
    }

    #[test]
    fn the_recipe_prepares_a_key_before_it_builds_and_checks_the_signature_after() {
        let position = |wanted: DisplayRecipeStep| {
            STEPS
                .iter()
                .position(|step| *step == wanted)
                .expect("every step is in STEPS")
        };

        assert_eq!(STEPS.len(), 13);
        assert!(
            position(DisplayRecipeStep::BuildDependencies)
                < position(DisplayRecipeStep::SigningKey)
        );
        assert!(
            position(DisplayRecipeStep::SigningKey) < position(DisplayRecipeStep::ModuleSource)
        );
        assert!(
            position(DisplayRecipeStep::ModuleBuild) < position(DisplayRecipeStep::ModuleSignature)
        );
        assert!(
            position(DisplayRecipeStep::ModuleSignature) < position(DisplayRecipeStep::Initramfs)
        );
        assert!(
            position(DisplayRecipeStep::ModuleLoad)
                < position(DisplayRecipeStep::CompositorIsolation),
            "the compositor is kept off the payload's Mesa for the device the module \
             just made, so the module comes first"
        );
        assert!(
            position(DisplayRecipeStep::CompositorIsolation) < position(DisplayRecipeStep::Device)
        );
    }

    #[test]
    fn a_modprobe_refusal_over_a_signature_says_what_is_missing() {
        let message = load_failure_message(
            "modprobe vmlord_drm exited with 1: modprobe: ERROR: could not insert \
             'vmlord_drm': Key was rejected by service",
            Some(true),
            Some(("/var/lib/dkms/mok.pub", "0a1b2c")),
        );

        assert!(
            message.contains("Key was rejected by service"),
            "the host matches on the kernel's own phrase: {message}"
        );
        assert!(message.contains("Secure Boot is on"), "{message}");
        assert!(message.contains("0a1b2c"), "{message}");
    }

    #[test]
    fn a_modprobe_refusal_that_is_not_about_a_signature_is_left_as_it_was() {
        let reason = "modprobe vmlord_drm exited with 1: modprobe: ERROR: could not \
                      insert 'vmlord_drm': Invalid argument";

        assert_eq!(
            load_failure_message(
                reason,
                Some(false),
                Some(("/var/lib/dkms/mok.pub", "0a1b2c"))
            ),
            reason
        );
    }

    #[test]
    fn a_guest_with_no_certificate_still_says_why_its_module_was_refused() {
        let message = load_failure_message(
            "modprobe vmlord_drm exited with 1: Required key not available",
            None,
            None,
        );

        assert!(message.contains("Required key not available"), "{message}");
        assert!(message.contains("no certificate to enroll"), "{message}");
    }
}
