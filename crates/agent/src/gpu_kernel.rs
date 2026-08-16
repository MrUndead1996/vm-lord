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
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use vmlord_agent_protocol::v1::{GpuRecipeStage, GpuRecipeStep};

use crate::{
    command::{self, Outcome},
    gpu_recipe::{
        Applicability, DkmsPackage, Environment, GuestFacts, MesaPolicy, Report, Shell,
        applicability, dkms_reports_installed, environment_document, icd_documents,
        library_triplet, module_is_loaded, parse_dkms_conf, parse_mesa_policy, parse_os_release,
        parse_payload_target, recipe_for,
    },
    gpu_targets::{PAYLOAD, WSL_LIB},
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

/// Where a bundled Mesa tree is staged, out of the payload that carries it.
///
/// A copy rather than the read-only 9p mount it came from: the mount lives as
/// long as the agent's session, and the linker cache, the `ld.so.conf.d` line
/// and the ICD symlink all outlive a reboot.
const MESA_PREFIX: &str = "/opt/vmlord/wsl-mesa";

/// Where the linker is told about a bundled Mesa.
///
/// Its own file, never the one `gpu_mounts` rewrites from the current set of
/// mounts: sharing one would mean that dropping a GPU share erases a line that
/// has nothing to do with shares.
const MESA_LD_CONF: &str = "/etc/ld.so.conf.d/vmlord-wsl-mesa.conf";

/// Where the Vulkan loader looks for the drivers of a system.
const VULKAN_ICD: &str = "/etc/vulkan/icd.d";

/// What a systemd user session and everything started from it inherits.
const GENERATOR: &str = "/etc/systemd/user-environment-generators/50-vmlord-gpu";

/// What a login shell picks up, which in an MVP guest means SSH.
const PROFILE: &str = "/etc/profile.d/vmlord-gpu.sh";

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
    device_stage(report)?;
    halted(stopping)?;

    let userspace = userspace_stage(report, &guest)?;
    halted(stopping)?;
    let icd = vulkan_stage(report, &userspace)?;
    halted(stopping)?;
    environment_stage(report, &userspace, icd)
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
///
/// The stage that decides whether the userspace half runs at all: a guest
/// whose device never appeared must not be configured for a driver that
/// cannot open it.
fn device_stage(report: &mut Report) -> Result<(), String> {
    if device_is_usable() {
        report.ok(GpuRecipeStep::Device, format!("{DEVICE} is a usable device"));
        return Ok(());
    }

    let reason = format!("{DEVICE} is missing, is not a character device, or will not open");
    report.failed(GpuRecipeStep::Device, reason.clone());
    Err(reason)
}

/// The userspace this guest ended up with, and where it lives.
struct Userspace {
    policy: MesaPolicy,
    /// Where a bundled Mesa was staged; nothing under `distro`.
    prefix: Option<PathBuf>,
    /// The directories a process has to load libraries from, in order.
    library_paths: Vec<String>,
}

/// Installs or stages the Mesa the payload's policy calls for.
fn userspace_stage(report: &mut Report, guest: &GuestFacts) -> Result<Userspace, String> {
    let policy =
        parse_mesa_policy(&read(&Path::new(PAYLOAD).join("sources.json"))).map_err(|error| {
            report.failed(GpuRecipeStep::Userspace, error.clone());
            error
        })?;
    let Some(triplet) = library_triplet(&guest.architecture) else {
        let reason = format!(
            "vmlord-agent has no library path for architecture {}",
            guest.architecture
        );
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        return Err(reason);
    };

    match policy {
        MesaPolicy::Distro => {
            distribution_mesa(report, triplet)?;
            Ok(Userspace {
                policy,
                prefix: None,
                library_paths: vec![WSL_LIB.to_owned()],
            })
        }
        MesaPolicy::Bundled => {
            let prefix = bundled_mesa(report, triplet)?;
            Ok(Userspace {
                policy,
                library_paths: vec![format!("{MESA_PREFIX}/lib/{triplet}"), WSL_LIB.to_owned()],
                prefix: Some(prefix),
            })
        }
    }
}

/// Installs the distribution's own Mesa, and only what is missing.
///
/// Ubuntu's Mesa carries the d3d12 gallium driver and is not built with
/// `microsoft-experimental`, so Vulkan under this policy is lavapipe. That is
/// a fact for the host's log and not a refusal: the payload's author chose the
/// policy, and whether GL alone is enough is the probe's question.
fn distribution_mesa(report: &mut Report, triplet: &str) -> Result<(), String> {
    let driver = PathBuf::from(format!("/usr/lib/{triplet}/dri/d3d12_dri.so"));
    let loader = PathBuf::from(format!("/usr/lib/{triplet}/libvulkan.so.1"));
    if driver.exists() && loader.exists() {
        report.skipped(
            GpuRecipeStep::Userspace,
            format!(
                "the distribution's Mesa is already installed; {} is present",
                driver.display()
            ),
        );
        return Ok(());
    }

    let mut outcome = apt_mesa();
    if !outcome.succeeded() {
        // A cloud image's package lists are as old as the image.
        let _ = command::run(
            "apt-get",
            &["update"],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        );
        outcome = apt_mesa();
    }

    if outcome.succeeded() {
        report.ok(
            GpuRecipeStep::Userspace,
            "installed the distribution's Mesa: d3d12 gallium for GL, and lavapipe for Vulkan, \
             because Ubuntu does not build the dzn driver"
                .to_owned(),
        );
        Ok(())
    } else {
        let reason = failure("apt-get install", &outcome);
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        Err(reason)
    }
}

fn apt_mesa() -> Outcome {
    command::run(
        "apt-get",
        &[
            "install",
            "-y",
            "libgl1-mesa-dri",
            "mesa-vulkan-drivers",
            "libvulkan1",
        ],
        &[("DEBIAN_FRONTEND", "noninteractive")],
        APT_BUDGET,
    )
}

/// Stages the Mesa tree the payload carries, and tells the linker about it.
fn bundled_mesa(report: &mut Report, triplet: &str) -> Result<PathBuf, String> {
    let source = Path::new(PAYLOAD).join("content").join("mesa");
    let prefix = PathBuf::from(MESA_PREFIX);
    if !source.is_dir() {
        let reason = format!(
            "the payload's policy is bundled and {} is not there",
            source.display()
        );
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        return Err(reason);
    }

    let changed = copy_tree(&source, &prefix).map_err(|error| {
        let reason = format!("{MESA_PREFIX} could not be staged: {error}");
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        reason
    })?;

    let libraries = format!("{MESA_PREFIX}/lib/{triplet}");
    let line = format!("# Written by vmlord-agent. Do not edit.\n{libraries}\n");
    if let Err(error) = write_if_different(Path::new(MESA_LD_CONF), &line) {
        let reason = format!("{MESA_LD_CONF} could not be written: {error}");
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        return Err(reason);
    }
    let _ = command::run("ldconfig", &[], &[], SHORT_BUDGET);

    if changed {
        report.ok(
            GpuRecipeStep::Userspace,
            format!(
                "staged the payload's Mesa at {MESA_PREFIX} and told the linker about {libraries}"
            ),
        );
    } else {
        report.skipped(
            GpuRecipeStep::Userspace,
            format!("{MESA_PREFIX} already holds this payload's Mesa"),
        );
    }
    Ok(prefix)
}

/// Registers the Vulkan driver a payload carries, when it carries one.
///
/// A payload with GL and no Vulkan is a legitimate payload, so nothing here
/// fails on an absent ICD: whether a guest has enough of a renderer is the
/// probe's judgement, not a stage's.
fn vulkan_stage(report: &mut Report, userspace: &Userspace) -> Result<Option<String>, String> {
    let Some(prefix) = &userspace.prefix else {
        report.skipped(
            GpuRecipeStep::VulkanIcd,
            "the distribution registers its own Vulkan drivers".to_owned(),
        );
        return Ok(None);
    };

    let directory = prefix.join("share/vulkan/icd.d");
    let names: Vec<String> = fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let documents = icd_documents(&names);
    if documents.is_empty() {
        report.skipped(
            GpuRecipeStep::VulkanIcd,
            format!(
                "the payload carries no Vulkan driver in {}",
                directory.display()
            ),
        );
        return Ok(None);
    }

    let mut registered = Vec::with_capacity(documents.len());
    for document in &documents {
        let link = Path::new(VULKAN_ICD).join(document);
        if let Err(error) = symlink_if_different(&directory.join(document), &link) {
            let reason = format!("{} could not be registered: {error}", link.display());
            report.failed(GpuRecipeStep::VulkanIcd, reason.clone());
            return Err(reason);
        }
        registered.push(link.to_string_lossy().into_owned());
    }

    report.ok(
        GpuRecipeStep::VulkanIcd,
        format!("registered {} in {VULKAN_ICD}", documents.join(", ")),
    );
    Ok(registered.into_iter().next())
}

/// Writes what makes a process in this guest pick that userspace.
fn environment_stage(
    report: &mut Report,
    userspace: &Userspace,
    icd: Option<String>,
) -> Result<(), String> {
    let environment = Environment {
        library_paths: userspace.library_paths.clone(),
        icd,
    };

    let mut changed = false;
    for (path, form) in [(GENERATOR, Shell::Generator), (PROFILE, Shell::Profile)] {
        match write_script_if_different(Path::new(path), &environment_document(form, &environment)) {
            Ok(written) => changed |= written,
            Err(error) => {
                let reason = format!("{path} could not be written: {error}");
                report.failed(GpuRecipeStep::Environment, reason.clone());
                return Err(reason);
            }
        }
    }

    let policy = match userspace.policy {
        MesaPolicy::Distro => "the distribution's Mesa",
        MesaPolicy::Bundled => "the payload's Mesa",
    };
    if changed {
        report.ok(
            GpuRecipeStep::Environment,
            format!("{GENERATOR} and {PROFILE} now point a process at {policy}"),
        );
    } else {
        report.skipped(
            GpuRecipeStep::Environment,
            format!("{GENERATOR} and {PROFILE} already point a process at {policy}"),
        );
    }
    Ok(())
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

/// Points `link` at `target`, and says whether anything changed.
fn symlink_if_different(target: &Path, link: &Path) -> io::Result<bool> {
    if fs::read_link(link).is_ok_and(|present| present == target) {
        return Ok(false);
    }
    if let Some(directory) = link.parent() {
        fs::create_dir_all(directory)?;
    }
    match fs::remove_file(link) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(true)
}

/// Writes an executable script only when the file does not already hold it.
fn write_script_if_different(path: &Path, content: &str) -> io::Result<bool> {
    if fs::read_to_string(path).is_ok_and(|present| present == content) {
        return Ok(false);
    }
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(true)
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

    use super::{copy_tree, symlink_if_different};

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

    #[test]
    fn an_icd_symlink_is_written_once_and_then_left_alone() {
        // The stage is skipped on a second session only if re-registering an
        // ICD that is already registered changes nothing.
        let directory = temporary("icd");
        let target = directory.join("dzn_icd.x86_64.json");
        let link = directory.join("registered.json");
        fs::write(&target, b"{}\n").unwrap();

        assert!(symlink_if_different(&target, &link).unwrap());
        assert!(!symlink_if_different(&target, &link).unwrap());
        assert_eq!(fs::read(&link).unwrap(), b"{}\n");
    }

    #[test]
    fn an_icd_symlink_pointing_elsewhere_is_replaced() {
        let directory = temporary("icd-replaced");
        let old = directory.join("old.json");
        let new = directory.join("new.json");
        let link = directory.join("registered.json");
        fs::write(&old, b"old\n").unwrap();
        fs::write(&new, b"new\n").unwrap();
        symlink_if_different(&old, &link).unwrap();

        assert!(symlink_if_different(&new, &link).unwrap());
        assert_eq!(fs::read(&link).unwrap(), b"new\n");
    }
}
