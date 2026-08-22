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
    DisplayPayloadVersions, DisplayRecipeStage, DisplayRecipeStep, DisplayUpdateOutcome,
};

use crate::{
    command,
    display_recipe::{
        DKMS_PACKAGE, InstalledVersions, MODULE, PayloadFacts, Report, applies_to,
        dkms_reports_installed, dkms_versions, has_recipe, modprobe_options, module_is_loaded,
        needs_build, needs_reload, parse_module_parameters, parse_module_version,
        read_payload_facts, wanted_mode,
    },
    gpu_kernel::guest_facts,
    guest_files::{copy_tree, failure, read, write_if_different},
};

/// Where the guest mounts the display payload share.
pub const PAYLOAD_MOUNT: &str = "/opt/vmlord/display-payload";

const DKMS_TREE: &str = "/var/lib/dkms";
const MODULES_LOAD: &str = "/etc/modules-load.d/vmlord-display.conf";
const MODPROBE_OPTIONS: &str = "/etc/modprobe.d/vmlord-display.conf";
const UNBIND_UNIT: &str = "/etc/systemd/system/vmlord-display-unbind-simpledrm.service";
const DRM_DEVICES: &str = "/sys/class/drm";
const MODULE_VERSION: &str = "/sys/module/vmlord_drm/version";
const MODULE_PARAM_WIDTH: &str = "/sys/module/vmlord_drm/parameters/width";
const MODULE_PARAM_HEIGHT: &str = "/sys/module/vmlord_drm/parameters/height";

/// The budgets the recipe's three kinds of program get, as the GPU recipe's do:
/// apt talks to the network, a build compiles a kernel module, and everything
/// else is a few syscalls.
const APT_BUDGET: Duration = Duration::from_secs(300);
const BUILD_BUDGET: Duration = Duration::from_secs(900);
const SHORT_BUDGET: Duration = Duration::from_secs(30);

const KEPT_LOG_LINES: usize = 40;

/// Installs what the mounted payload says the guest should have.
///
/// Idempotent by fact: a guest already running the mounted version passes
/// through in a handful of checks and needs no network. Every failure is a
/// stage that says so, and the VM keeps running regardless.
pub fn apply(
    stopping: &AtomicBool,
    mode: Option<(u32, u32)>,
) -> (Vec<DisplayRecipeStage>, DisplayPayloadVersions) {
    let mut report = Report::new();
    let reason = match run_stages(&mut report, stopping, mode) {
        Ok(()) => "the recipe did not need this stage".to_owned(),
        Err(reason) => reason,
    };
    (report.finish(&reason), versions())
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
    (report.finish(&reason), versions(), outcome)
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
) -> Result<(), String> {
    let guest = guest_facts()?;
    if !has_recipe(&guest.distribution) {
        let reason = format!(
            "vmlord-agent has no display recipe for {} {}",
            guest.distribution, guest.release
        );
        report.skipped(DisplayRecipeStep::Distribution, reason.clone());
        return Err(reason);
    }
    report.ok(
        DisplayRecipeStep::Distribution,
        format!(
            "{} {} {} on kernel {}",
            guest.distribution, guest.release, guest.architecture, guest.kernel_release
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
    if needs_build(&installed, &payload.version, device_is_present()) {
        dependencies_stage(report, &guest.kernel_release)?;
        halted(stopping)?;
        source_stage(report, &payload)?;
        halted(stopping)?;
        build_stage(report, &payload.version, &guest.kernel_release)?;
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

    load_stage(report, mode)?;
    device_stage(report)?;
    services_stages(report);
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

    let attempt = (|| -> Result<(), String> {
        dependencies_stage(report, &guest.kernel_release)?;
        source_stage(report, &payload)?;
        build_stage(report, &payload.version, &guest.kernel_release)?;
        reload_module(report)?;
        verify(report, &payload.version)
    })();

    match attempt {
        Ok(()) => {
            services_stages(report);
            Ok(())
        }
        Err(reason) => Err(roll_back(&before, &payload.version, reason)),
    }
}

/// Puts the previous version back, and says what that came to.
///
/// The previous `/usr/src` tree was never removed and DKMS still holds its
/// build, so this is a `modprobe` and a `dkms remove` rather than a download.
fn roll_back(before: &InstalledVersions, attempted: &str, reason: String) -> UpdateFailure {
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

    let restored = module_is_loaded(&read(Path::new("/proc/modules")))
        && loaded_version().as_deref() == Some(previous.as_str())
        && device_is_present();

    if restored {
        UpdateFailure {
            reason: format!("{reason}; {previous} is running again"),
            outcome: DisplayUpdateOutcome::RolledBack,
        }
    } else {
        UpdateFailure {
            reason: format!("{reason}; {previous} could not be brought back either"),
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
    if !applies_to(&facts, distribution, release, architecture) {
        let reason = format!(
            "the mounted payload is for {} {} {}, and this guest is {distribution} {release} \
             {architecture}",
            facts.distribution, facts.release, facts.architecture
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

    report.ok(
        DisplayRecipeStep::Payload,
        format!("{} verified at {PAYLOAD_MOUNT}", facts.payload_id),
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

fn dependencies_stage(report: &mut Report, kernel_release: &str) -> Result<(), String> {
    if dependencies_are_present(kernel_release) {
        report.skipped(
            DisplayRecipeStep::BuildDependencies,
            "dkms, build-essential and the running kernel's headers are installed",
        );
        return Ok(());
    }

    let headers = format!("linux-headers-{kernel_release}");
    let outcome = command::run(
        "apt-get",
        &[
            "install",
            "-y",
            "--no-install-recommends",
            "dkms",
            "build-essential",
            &headers,
        ],
        &[("DEBIAN_FRONTEND", "noninteractive")],
        APT_BUDGET,
    );
    if !outcome.succeeded() {
        // A guest with no network is a guest whose display is degraded and
        // whose VM is fine: cloud-init provisioned it over the same NAT, so
        // this is a failure worth naming rather than one worth hiding.
        let reason = failure("apt-get install", &outcome);
        report.failed(DisplayRecipeStep::BuildDependencies, reason.clone());
        return Err(reason);
    }

    report.ok(
        DisplayRecipeStep::BuildDependencies,
        format!("installed dkms, build-essential and {headers}"),
    );
    Ok(())
}

fn dependencies_are_present(kernel_release: &str) -> bool {
    Path::new("/usr/sbin/dkms").exists()
        && Path::new(&format!("/lib/modules/{kernel_release}/build")).exists()
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

/// Loads the module now, and arranges for it on every boot after this one.
///
/// The unit that unbinds `simple-framebuffer` is installed here too: simpledrm
/// is builtin, so it cannot be blacklisted, and until it lets go of the console
/// a compositor has two devices to choose between.
fn load_stage(report: &mut Report, mode: Option<(u32, u32)>) -> Result<(), String> {
    let wanted = wanted_mode(mode);
    let payload_drm = Path::new(PAYLOAD_MOUNT).join("content").join("drm");
    let copy = |from: &str, to: &str| -> Result<(), String> {
        let source = payload_drm.join(from);
        if !source.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&source)
            .map_err(|error| format!("{} could not be read: {error}", source.display()))?;
        write_if_different(Path::new(to), &content)
            .map_err(|error| format!("{to} could not be written: {error}"))
    };

    let prepared = write_if_different(Path::new(MODULES_LOAD), &format!("{MODULE}\n"))
        .map_err(|error| format!("{MODULES_LOAD} could not be written: {error}"))
        .and_then(|()| {
            write_if_different(
                Path::new(MODPROBE_OPTIONS),
                &modprobe_options(wanted.0, wanted.1),
            )
            .map_err(|error| format!("{MODPROBE_OPTIONS} could not be written: {error}"))
        })
        .and_then(|()| copy("vmlord-display-unbind-simpledrm.service", UNBIND_UNIT));
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

    if module_is_loaded(&read(Path::new("/proc/modules"))) {
        let loaded = parse_module_parameters(
            &read(Path::new(MODULE_PARAM_WIDTH)),
            &read(Path::new(MODULE_PARAM_HEIGHT)),
        );
        if needs_reload(loaded, wanted) {
            // The stored mode changed under a module that is already up, and a
            // module parameter is read once. A reload that fails is a failed
            // stage and a degraded display -- and a VM that keeps running.
            return reload_module(report);
        }
        report.skipped(
            DisplayRecipeStep::ModuleLoad,
            format!("{MODULE} is loaded at {}x{}", wanted.0, wanted.1),
        );
        return Ok(());
    }

    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if !outcome.succeeded() {
        let reason = failure(&format!("modprobe {MODULE}"), &outcome);
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
fn reload_module(report: &mut Report) -> Result<(), String> {
    let _ = command::run("modprobe", &["-r", MODULE], &[], SHORT_BUDGET);
    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if !outcome.succeeded() {
        let reason = failure(&format!("modprobe {MODULE}"), &outcome);
        report.failed(DisplayRecipeStep::ModuleLoad, reason.clone());
        return Err(reason);
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
    report.ok(
        DisplayRecipeStep::Device,
        format!("a {MODULE} display device is present"),
    );
    Ok(())
}

/// Checks the update did what it said: the target version loaded, on a device
/// that exists.
fn verify(report: &mut Report, target_version: &str) -> Result<(), String> {
    device_stage(report)?;
    match loaded_version() {
        Some(loaded) if loaded == target_version => Ok(()),
        Some(loaded) => Err(format!(
            "{MODULE} {loaded} is loaded, and the update installed {target_version}"
        )),
        None => Err(format!("{MODULE} does not say which version is loaded")),
    }
}

/// The two stages that wait on task #115.
///
/// Skipped and never failed, with the reason said out loud: a payload that
/// carries no services is the ordinary state of every payload this task ships,
/// and reporting it as a failure would make every guest degraded.
fn services_stages(report: &mut Report) {
    let services = Path::new(PAYLOAD_MOUNT).join("content").join("services");
    let empty = fs::read_dir(&services).is_ok_and(|mut entries| entries.next().is_none())
        || !services.exists();
    let reason = if empty {
        "this payload carries no display services"
    } else {
        "installing display services is not implemented by this build"
    };
    report.skipped(DisplayRecipeStep::Services, reason);
    report.skipped(DisplayRecipeStep::ServicesStart, reason);
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
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use vmlord_agent_protocol::v1::{DisplayRecipeStageState, DisplayRecipeStep};

    use super::{apply, sha256_hex, update, verify_declared_files};
    use crate::display_recipe::PayloadFacts;

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
        let (stages, versions) = apply(&AtomicBool::new(false), None);

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
}
