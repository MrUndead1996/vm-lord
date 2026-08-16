//! Applying the Ubuntu GPU recipe to the guest this agent runs in.
//!
//! What decides is in `gpu_recipe`; what is here is the part that needs an
//! Ubuntu guest with a payload mounted: reading the guest's own files, staging
//! the module sources somewhere DKMS can write beside them, running apt, DKMS
//! and `modprobe`, and looking at `/dev/dxg` afterwards.
//!
//! Nothing here fails as a whole. Every stage that does not succeed is a stage
//! in the report and a VM that keeps running with less GPU than it asked for.

use std::{
    fs, io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use vmlord_agent_protocol::v1::{GpuRecipeStage, GpuRecipeStep};

use crate::{
    command::{self, Outcome},
    gpu_recipe::{
        Applicability, DkmsPackage, GuestFacts, Report, applicability, dkms_reports_installed,
        module_is_loaded, parse_dkms_conf, parse_os_release, parse_payload_target, recipe_for,
    },
    gpu_targets::PAYLOAD,
};

/// The kernel module this recipe exists to deliver.
const MODULE: &str = "dxgkrnl";

/// The device node the module creates, and the point of the whole recipe.
const DEVICE: &str = "/dev/dxg";

/// Where the module is asked for on every boot.
///
/// A module loaded only by `modprobe` is gone after the next reboot, and
/// GPU-PV then breaks silently on a VM that was fine yesterday.
const MODULES_LOAD: &str = "/etc/modules-load.d/vmlord-dxgkrnl.conf";

/// Where DKMS expects to find the sources of a package.
const DKMS_SOURCES: &str = "/usr/src";

/// Where DKMS leaves the log of a build that failed.
const DKMS_TREE: &str = "/var/lib/dkms";

/// How many lines of a failed build's log reach the host.
const KEPT_LOG_LINES: usize = 20;

const APT_BUDGET: Duration = Duration::from_secs(300);
const BUILD_BUDGET: Duration = Duration::from_secs(900);
const SHORT_BUDGET: Duration = Duration::from_secs(30);

/// Applies this guest's GPU recipe and says what happened, stage by stage.
///
/// Called once per session, after the shares of the same session were mounted.
/// Most calls do almost nothing: a guest whose module is already built,
/// installed and loaded short-circuits before the first stage that would run a
/// program.
pub fn apply(stopping: &AtomicBool) -> Vec<GpuRecipeStage> {
    let mut report = Report::new();
    let reason = match run_stages(&mut report, stopping) {
        Ok(()) => "the recipe did not need this stage".to_owned(),
        Err(reason) => reason,
    };
    report.finish(&reason)
}

/// The stages, in order, stopping at the first one that ends the recipe.
///
/// `Err` carries what the stages that never ran are reported with.
fn run_stages(report: &mut Report, stopping: &AtomicBool) -> Result<(), String> {
    let guest = guest_facts()?;
    if recipe_for(&guest.distribution).is_none() {
        let reason = format!(
            "vmlord-agent has no GPU recipe for {} {}",
            guest.distribution, guest.release
        );
        report.skipped(GpuRecipeStep::Distribution, reason.clone());
        return Err(reason);
    }
    report.ok(
        GpuRecipeStep::Distribution,
        format!(
            "{} {} {} on kernel {}",
            guest.distribution, guest.release, guest.architecture, guest.kernel_release
        ),
    );

    let package = payload_stage(report, &guest)?;
    halted(stopping)?;

    if module_is_loaded(&read(Path::new("/proc/modules")), MODULE) && device_is_usable() {
        let already = format!("{MODULE} is already loaded and {DEVICE} answers");
        for step in [
            GpuRecipeStep::BuildDependencies,
            GpuRecipeStep::ModuleSource,
            GpuRecipeStep::ModuleBuild,
        ] {
            report.skipped(step, already.clone());
        }
    } else {
        dependencies_stage(report, &guest)?;
        halted(stopping)?;
        source_stage(report, &package)?;
        halted(stopping)?;
        build_stage(report, &package, &guest)?;
        halted(stopping)?;
    }

    load_stage(report)?;
    device_stage(report);
    Ok(())
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

/// What this guest is, from its own files.
fn guest_facts() -> Result<GuestFacts, String> {
    let (distribution, release) = parse_os_release(&read(Path::new("/etc/os-release")))
        .ok_or_else(|| "/etc/os-release names no distribution".to_owned())?;
    let (kernel_release, machine) = uname()?;

    Ok(GuestFacts {
        distribution,
        release,
        // Debian's name for the machine, because that is what a payload target
        // and an apt package name are written in.
        architecture: match machine.as_str() {
            "x86_64" => "amd64".to_owned(),
            "aarch64" => "arm64".to_owned(),
            other => other.to_owned(),
        },
        kernel_release,
    })
}

/// The running kernel's release and machine.
fn uname() -> Result<(String, String), String> {
    let mut information = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `uname` fills the `utsname` it is given and touches nothing
    // else; the pointer is to a live, correctly sized allocation.
    let result = unsafe { libc::uname(information.as_mut_ptr()) };
    if result != 0 {
        return Err(format!("uname failed: {}", io::Error::last_os_error()));
    }
    // SAFETY: `uname` returned success, so the structure is initialized.
    let information = unsafe { information.assume_init() };

    Ok((field(&information.release), field(&information.machine)))
}

/// One NUL-terminated C string out of a `utsname`.
fn field(bytes: &[libc::c_char]) -> String {
    bytes
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8 as char)
        .collect()
}

/// Checks the mounted payload and reads what module it carries.
fn payload_stage(report: &mut Report, guest: &GuestFacts) -> Result<DkmsPackage, String> {
    let root = Path::new(PAYLOAD);
    let sources = read(&root.join("sources.json"));
    if sources.is_empty() {
        let reason = format!("no GPU payload is mounted at {PAYLOAD}");
        report.skipped(GpuRecipeStep::Payload, reason.clone());
        return Err(reason);
    }

    let target = parse_payload_target(&sources).map_err(|error| {
        report.failed(GpuRecipeStep::Payload, error.clone());
        error
    })?;
    let note = match applicability(&target, guest) {
        Applicability::NotApplicable(reason) => {
            report.skipped(GpuRecipeStep::Payload, reason.clone());
            return Err(reason);
        }
        Applicability::Applies { kernel } => kernel,
    };

    let module = root.join("content").join(MODULE);
    let package = parse_dkms_conf(&read(&module.join("dkms.conf"))).map_err(|error| {
        let reason = format!("{PAYLOAD}/content/{MODULE}: {error}");
        report.failed(GpuRecipeStep::Payload, reason.clone());
        reason
    })?;

    let mut message = format!("{} {} from the payload", package.name, package.version);
    if let Some(note) = note {
        message.push_str("; ");
        message.push_str(&note);
    }
    report.ok(GpuRecipeStep::Payload, message);
    Ok(package)
}

/// Installs what the build needs, and only what is missing.
///
/// A guest that already has a compiler, DKMS and its own kernel's headers
/// never reaches apt, which is what makes the second start of a VM work with
/// no network at all.
fn dependencies_stage(report: &mut Report, guest: &GuestFacts) -> Result<(), String> {
    let headers = format!("linux-headers-{}", guest.kernel_release);
    if dependencies_are_present(&guest.kernel_release) {
        report.skipped(
            GpuRecipeStep::BuildDependencies,
            format!("dkms, a compiler and {headers} are already installed"),
        );
        return Ok(());
    }

    let mut outcome = apt_install(&headers);
    if !outcome.succeeded() {
        // A cloud image's package lists are as old as the image, and a stale
        // list is the ordinary reason an install of a kernel-specific package
        // fails on a VM's first boot.
        let _ = command::run(
            "apt-get",
            &["update"],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        );
        outcome = apt_install(&headers);
    }

    if outcome.succeeded() {
        report.ok(
            GpuRecipeStep::BuildDependencies,
            format!("installed dkms, build-essential and {headers}"),
        );
        Ok(())
    } else {
        let reason = failure("apt-get install", &outcome);
        report.failed(GpuRecipeStep::BuildDependencies, reason.clone());
        Err(reason)
    }
}

fn apt_install(headers: &str) -> Outcome {
    command::run(
        "apt-get",
        &["install", "-y", "dkms", "build-essential", headers],
        &[("DEBIAN_FRONTEND", "noninteractive")],
        APT_BUDGET,
    )
}

/// Whether the guest can already build a module for its own kernel.
fn dependencies_are_present(kernel_release: &str) -> bool {
    let headers = PathBuf::from(format!("/lib/modules/{kernel_release}/build"));
    headers.exists()
        && command::run("dkms", &["--version"], &[], SHORT_BUDGET).succeeded()
        && command::run("cc", &["--version"], &[], SHORT_BUDGET).succeeded()
}

/// Stages the module sources where DKMS can write beside them.
///
/// A copy rather than a symlink: the payload is mounted read-only over 9p, and
/// DKMS writes its build tree next to the sources it is given.
fn source_stage(report: &mut Report, package: &DkmsPackage) -> Result<(), String> {
    let source = Path::new(PAYLOAD).join("content").join(MODULE);
    let destination = Path::new(DKMS_SOURCES).join(format!("{}-{}", package.name, package.version));

    match copy_tree(&source, &destination) {
        Ok(true) => {
            report.ok(
                GpuRecipeStep::ModuleSource,
                format!(
                    "staged {} sources at {}",
                    package.name,
                    destination.display()
                ),
            );
            Ok(())
        }
        Ok(false) => {
            report.skipped(
                GpuRecipeStep::ModuleSource,
                format!("{} already holds these sources", destination.display()),
            );
            Ok(())
        }
        Err(error) => {
            let reason = format!("{} could not be staged: {error}", destination.display());
            report.failed(GpuRecipeStep::ModuleSource, reason.clone());
            Err(reason)
        }
    }
}

/// Builds and installs the module for the running kernel.
fn build_stage(report: &mut Report, package: &DkmsPackage, guest: &GuestFacts) -> Result<(), String> {
    let status = command::run("dkms", &["status"], &[], SHORT_BUDGET);
    if dkms_reports_installed(&status.output, package, &guest.kernel_release) {
        report.skipped(
            GpuRecipeStep::ModuleBuild,
            format!(
                "{} {} is already installed for kernel {}",
                package.name, package.version, guest.kernel_release
            ),
        );
        return Ok(());
    }

    // `dkms add` fails when the package is already registered, which is the
    // ordinary state of a guest whose sources were staged by an earlier
    // session. The build is what decides, so its failure is the only one
    // reported.
    let _ = command::run(
        "dkms",
        &["add", "-m", &package.name, "-v", &package.version],
        &[],
        SHORT_BUDGET,
    );

    for (step, budget) in [("build", BUILD_BUDGET), ("install", SHORT_BUDGET)] {
        let outcome = command::run(
            "dkms",
            &[
                step,
                "-m",
                &package.name,
                "-v",
                &package.version,
                "-k",
                &guest.kernel_release,
            ],
            &[],
            budget,
        );
        if !outcome.succeeded() {
            let mut reason = failure(&format!("dkms {step}"), &outcome);
            if let Some(log) = make_log(package) {
                reason.push_str("\nmake.log:\n");
                reason.push_str(&log);
            }
            report.failed(GpuRecipeStep::ModuleBuild, reason.clone());
            return Err(reason);
        }
    }

    report.ok(
        GpuRecipeStep::ModuleBuild,
        format!(
            "built and installed {} {} for kernel {}",
            package.name, package.version, guest.kernel_release
        ),
    );
    Ok(())
}

/// The tail of the log a failed DKMS build leaves behind.
///
/// An exit code from a compiler is not a diagnosis, and the host's log is
/// where this is read.
fn make_log(package: &DkmsPackage) -> Option<String> {
    let log = Path::new(DKMS_TREE)
        .join(&package.name)
        .join(&package.version)
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

/// Loads the module now, and asks for it on every boot after this one.
fn load_stage(report: &mut Report) -> Result<(), String> {
    if let Err(error) = write_if_different(Path::new(MODULES_LOAD), &format!("{MODULE}\n")) {
        let reason = format!("{MODULES_LOAD} could not be written: {error}");
        report.failed(GpuRecipeStep::ModuleLoad, reason.clone());
        return Err(reason);
    }

    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if outcome.succeeded() {
        report.ok(
            GpuRecipeStep::ModuleLoad,
            format!("{MODULE} is loaded and listed in {MODULES_LOAD}"),
        );
        Ok(())
    } else {
        let reason = failure("modprobe", &outcome);
        report.failed(GpuRecipeStep::ModuleLoad, reason.clone());
        Err(reason)
    }
}

/// Looks at the device node the module exists to create.
fn device_stage(report: &mut Report) {
    if device_is_usable() {
        report.ok(GpuRecipeStep::Device, format!("{DEVICE} is a usable device"));
    } else {
        report.failed(
            GpuRecipeStep::Device,
            format!("{DEVICE} is missing, is not a character device, or will not open"),
        );
    }
}

/// Whether `/dev/dxg` is there and answers.
///
/// Opened rather than merely stat'd: that is what separates a node the kernel
/// created from one left behind by a module that is no longer there.
fn device_is_usable() -> bool {
    let Ok(metadata) = fs::metadata(DEVICE) else {
        return false;
    };
    metadata.file_type().is_char_device() && fs::File::open(DEVICE).is_ok()
}

/// Copies `source` onto `destination`, and says whether anything changed.
///
/// Files that are already byte-for-byte identical are left alone, so a
/// reconnect does not rewrite the tree DKMS is registered against -- rewriting
/// it is what would make DKMS rebuild on every session.
pub fn copy_tree(source: &Path, destination: &Path) -> io::Result<bool> {
    fs::create_dir_all(destination)?;
    let mut changed = false;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            changed |= copy_tree(&from, &to)?;
            continue;
        }
        let wanted = fs::read(&from)?;
        if fs::read(&to).is_ok_and(|present| present == wanted) {
            continue;
        }
        fs::write(&to, &wanted)?;
        changed = true;
    }

    Ok(changed)
}

/// Writes `content` only when the file does not already hold it.
fn write_if_different(path: &Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|present| present == content) {
        return Ok(());
    }
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, content)
}

/// A file that may not be there, as the empty string.
///
/// Every caller treats "missing" and "empty" the same way -- as a fact that is
/// not there to be read -- and an `io::Error` here would be a second way of
/// saying the same stage did not apply.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// One line about a program that did not succeed.
fn failure(what: &str, outcome: &Outcome) -> String {
    let ending = match outcome.ending {
        command::Ending::Exited(code) => format!("exited with {code}"),
        command::Ending::TimedOut => "outran its time budget".to_owned(),
        command::Ending::NotStarted => "could not be started".to_owned(),
    };
    format!("{what} {ending}: {}", outcome.output)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::copy_tree;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temporary(label: &str) -> PathBuf {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vmlord-agent-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a temporary directory");
        path
    }

    #[test]
    fn a_tree_is_copied_whole() {
        let source = temporary("copy-source");
        let destination = temporary("copy-destination").join("staged");
        fs::create_dir(source.join("include")).unwrap();
        fs::write(source.join("dkms.conf"), b"PACKAGE_NAME=dxgkrnl\n").unwrap();
        fs::write(source.join("include/d3dkmthk.h"), b"header\n").unwrap();

        let changed = copy_tree(&source, &destination).expect("a copied tree");

        assert!(changed);
        assert_eq!(
            fs::read(destination.join("include/d3dkmthk.h")).unwrap(),
            b"header\n"
        );
    }

    #[test]
    fn copying_the_same_tree_again_changes_nothing() {
        // A reconnect must not rewrite the tree DKMS is registered against.
        let source = temporary("idempotent-source");
        let destination = temporary("idempotent-destination").join("staged");
        fs::write(source.join("dkms.conf"), b"PACKAGE_NAME=dxgkrnl\n").unwrap();

        assert!(copy_tree(&source, &destination).unwrap());
        assert!(!copy_tree(&source, &destination).unwrap());
    }

    #[test]
    fn a_changed_file_is_copied_over() {
        let source = temporary("changed-source");
        let destination = temporary("changed-destination").join("staged");
        fs::write(source.join("dkms.conf"), b"PACKAGE_VERSION=2.0.3\n").unwrap();
        copy_tree(&source, &destination).unwrap();

        fs::write(source.join("dkms.conf"), b"PACKAGE_VERSION=2.0.4\n").unwrap();

        assert!(copy_tree(&source, &destination).unwrap());
        assert_eq!(
            fs::read(destination.join("dkms.conf")).unwrap(),
            b"PACKAGE_VERSION=2.0.4\n"
        );
    }
}
