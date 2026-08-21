//! Which guests the GPU recipe applies to, and what the payload says it is.
//!
//! Everything here is a function of text: `/etc/os-release`, the payload's
//! `sources.json`, a `dkms.conf`, `/proc/modules`, the output of
//! `dkms status`. That is deliberate -- it is what makes the decisions of a
//! recipe testable on a machine that is neither Ubuntu nor a Hyper-V guest,
//! while `gpu_kernel` keeps the parts that need one.

use vmlord_agent_protocol::v1::{GpuRecipeStage, GpuRecipeStageState, GpuRecipeStep};

/// Every step of the recipe, in the order it is attempted.
///
/// The order is the report's order, and the report is what the host logs, so
/// it is written once here rather than implied by the sequence of calls in
/// `gpu_kernel`.
pub const STEPS: [GpuRecipeStep; 10] = [
    GpuRecipeStep::Distribution,
    GpuRecipeStep::Payload,
    GpuRecipeStep::BuildDependencies,
    GpuRecipeStep::ModuleSource,
    GpuRecipeStep::ModuleBuild,
    GpuRecipeStep::ModuleLoad,
    GpuRecipeStep::Device,
    GpuRecipeStep::Userspace,
    GpuRecipeStep::VulkanIcd,
    GpuRecipeStep::Environment,
];

/// What a recipe run has found out so far.
///
/// Collected rather than sent as it goes, because a stage list is one answer
/// to one request: the host asked what the recipe did, not to be narrated at.
#[derive(Default)]
pub struct Report {
    recorded: Vec<GpuRecipeStage>,
}

impl Report {
    pub fn new() -> Self {
        Self {
            recorded: Vec::with_capacity(STEPS.len()),
        }
    }

    pub fn ok(&mut self, step: GpuRecipeStep, message: impl Into<String>) {
        self.record(step, GpuRecipeStageState::Ok, message.into());
    }

    pub fn skipped(&mut self, step: GpuRecipeStep, message: impl Into<String>) {
        self.record(step, GpuRecipeStageState::Skipped, message.into());
    }

    pub fn failed(&mut self, step: GpuRecipeStep, message: impl Into<String>) {
        self.record(step, GpuRecipeStageState::Failed, message.into());
    }

    /// Keeps the first answer a step was given.
    ///
    /// Nothing should record a step twice; if something does, the report must
    /// not grow a second copy of a step the host reads once.
    fn record(&mut self, step: GpuRecipeStep, state: GpuRecipeStageState, message: String) {
        if self.recorded.iter().any(|stage| stage.step() == step) {
            return;
        }
        self.recorded.push(GpuRecipeStage {
            step: i32::from(step),
            state: i32::from(state),
            message,
        });
    }

    /// The whole report: what happened, and `reason` for what never ran.
    pub fn finish(mut self, reason: &str) -> Vec<GpuRecipeStage> {
        for step in STEPS {
            self.skipped(step, reason);
        }
        self.recorded
            .sort_by_key(|stage| STEPS.iter().position(|step| *step == stage.step()));
        self.recorded
    }
}

/// What the guest says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestFacts {
    /// `ID` from `/etc/os-release`, lowercase by that file's own convention.
    pub distribution: String,
    /// `VERSION_ID` from `/etc/os-release`.
    pub release: String,
    /// The Debian architecture name, not the machine name `uname` gives.
    pub architecture: String,
    /// `uname -r`: the kernel that is running now, which is the one DKMS
    /// builds against.
    pub kernel_release: String,
}

/// A distribution this build knows how to bring a GPU up on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuRecipe {
    Ubuntu,
}

/// The recipe for a distribution, or nothing for one with none.
///
/// The whole "an unsupported release degrades the GPU and does not stop the
/// VM" rule starts here: a guest with no recipe is a skipped first stage, not
/// an error.
pub fn recipe_for(distribution: &str) -> Option<GpuRecipe> {
    match distribution {
        "ubuntu" => Some(GpuRecipe::Ubuntu),
        _ => None,
    }
}

/// Reads `ID` and `VERSION_ID` out of an `/etc/os-release`.
pub fn parse_os_release(text: &str) -> Option<(String, String)> {
    let mut id = None;
    let mut version = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match name.trim() {
            "ID" => id = Some(value),
            "VERSION_ID" => version = Some(value),
            _ => {}
        }
    }

    Some((id?, version?))
}

/// What a payload says it was built for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadTarget {
    pub distribution: String,
    pub release: String,
    pub architecture: String,
    pub kernel_release: String,
}

/// Reads the target out of a payload's `sources.json`.
///
/// Only the target: the rest of that document is provenance the host has
/// already verified against the catalog, and re-deciding it here would be a
/// second validation boundary that could disagree with the first.
pub fn parse_payload_target(json: &str) -> Result<PayloadTarget, String> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("sources.json is unreadable: {error}"))?;
    let target = document
        .get("target")
        .ok_or_else(|| "sources.json names no target".to_owned())?;

    let field = |name: &str| -> Result<String, String> {
        target
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("the payload target has no {name}"))
    };

    Ok(PayloadTarget {
        distribution: field("distribution")?,
        release: field("release")?,
        architecture: field("architecture")?,
        kernel_release: field("kernel_release")?,
    })
}

/// Whether a payload's recipe applies to this guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applicability {
    /// It applies. `kernel` carries a note when the payload was proven on a
    /// different kernel than the one running.
    Applies { kernel: Option<String> },
    /// It does not, and this is why.
    NotApplicable(String),
}

/// Compares what the payload was built for with what the guest is.
///
/// Distribution, release and architecture are the hard gate. The kernel is
/// not: DKMS builds against the running kernel's headers, so the payload's
/// `kernel_release` records what the recipe was proven on rather than what it
/// requires -- and requiring it would mean the unattended kernel upgrade
/// Ubuntu performs on its own kills GPU-PV until a payload is repacked.
pub fn applicability(payload: &PayloadTarget, guest: &GuestFacts) -> Applicability {
    for (what, expected, actual) in [
        ("distribution", &payload.distribution, &guest.distribution),
        ("release", &payload.release, &guest.release),
        ("architecture", &payload.architecture, &guest.architecture),
    ] {
        if expected != actual {
            return Applicability::NotApplicable(format!(
                "the payload was built for {what} {expected} and this guest is {actual}"
            ));
        }
    }

    let kernel = (payload.kernel_release != guest.kernel_release).then(|| {
        format!(
            "the payload was proven on kernel {} and this guest runs {}",
            payload.kernel_release, guest.kernel_release
        )
    });
    Applicability::Applies { kernel }
}

/// The module package a `dkms.conf` describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DkmsPackage {
    pub name: String,
    pub version: String,
}

/// Reads `PACKAGE_NAME` and `PACKAGE_VERSION` out of a `dkms.conf`.
///
/// The payload names its own package and version rather than the agent
/// hard-coding them: a repacked payload with a newer module must not need a
/// new agent.
pub fn parse_dkms_conf(text: &str) -> Result<DkmsPackage, String> {
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let Some((field, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match field.trim() {
            "PACKAGE_NAME" => name = Some(value),
            "PACKAGE_VERSION" => version = Some(value),
            _ => {}
        }
    }

    match (name, version) {
        (Some(name), Some(version)) if !name.is_empty() && !version.is_empty() => {
            Ok(DkmsPackage { name, version })
        }
        _ => Err("dkms.conf names no package and version".to_owned()),
    }
}

/// Where a payload's userspace comes from.
///
/// The host has already checked this value against the catalog entry it
/// downloaded the payload for; the guest honours it rather than deciding
/// again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MesaPolicy {
    /// The distribution's own Mesa, installed from the guest's apt.
    Distro,
    /// The Mesa tree the payload carries.
    Bundled,
}

/// Reads `mesa_policy` out of a payload's `sources.json`.
///
/// Read here rather than folded into [`PayloadTarget`] on purpose: a policy
/// this build has never heard of must fail the userspace stage it belongs to,
/// not the payload stage after which a kernel module would have built and
/// `/dev/dxg` would have worked.
pub fn parse_mesa_policy(json: &str) -> Result<MesaPolicy, String> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("sources.json is unreadable: {error}"))?;
    match document
        .get("mesa_policy")
        .and_then(serde_json::Value::as_str)
    {
        Some("distro") => Ok(MesaPolicy::Distro),
        Some("bundled") => Ok(MesaPolicy::Bundled),
        Some(other) => Err(format!(
            "vmlord-agent has no recipe for the mesa policy {other}"
        )),
        None => Err("sources.json names no mesa policy".to_owned()),
    }
}

/// The multiarch directory a Debian architecture's libraries live under.
///
/// Derived from the guest rather than written as a constant: an agent that
/// hard-codes one architecture's library path is one that silently installs
/// nothing on the other.
pub fn library_triplet(architecture: &str) -> Option<&'static str> {
    match architecture {
        "amd64" => Some("x86_64-linux-gnu"),
        "arm64" => Some("aarch64-linux-gnu"),
        _ => None,
    }
}

/// The Vulkan ICD documents among the names of a directory's entries.
///
/// Names from the payload and never a constant: AppSandbox's own notes record
/// a README promising `microsoft_icd.x86_64.json` where Mesa shipped
/// `dzn_icd.x86_64.json`, and a hard-coded name is a stage that reports
/// success on a file it never found.
pub fn icd_documents(names: &[String]) -> Vec<String> {
    let mut documents: Vec<String> = names
        .iter()
        .filter(|name| name.ends_with(".json"))
        .cloned()
        .collect();
    documents.sort();
    documents
}

/// Which of the two files the environment is written into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// A systemd user-environment generator, which prints `NAME=VALUE`.
    Generator,
    /// A `profile.d` script, which is sourced and therefore exports.
    Profile,
}

/// The userspace a process in this guest should be pointed at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// Directories to put on `LD_LIBRARY_PATH`, in order. The first is also
    /// what the script checks for before setting anything.
    pub library_paths: Vec<String>,
    /// The ICD document to pin, when one was registered.
    pub icd: Option<String>,
}

/// The variables that do not depend on what the payload turned out to carry.
///
/// `GALLIUM_DRIVER` and `MESA_LOADER_DRIVER_OVERRIDE` are both there because
/// the first is direct gallium selection on the GLX path and the second is the
/// DRI loader EGL and Wayland clients go through: setting one gives an
/// accelerated GLX and llvmpipe on EGL.
const FIXED: [(&str, &str); 3] = [
    ("GALLIUM_DRIVER", "d3d12"),
    ("MESA_LOADER_DRIVER_OVERRIDE", "d3d12"),
    ("__GLX_VENDOR_LIBRARY_NAME", "mesa"),
];

/// The script that points a process at this userspace, when there is a GPU.
///
/// A script with the probe inside rather than a file of finished values: the
/// file outlives a reboot and `/dev/dxg` does not, and a VM restarted without
/// a GPU and a static `MESA_LOADER_DRIVER_OVERRIDE=d3d12` is a guest where GL
/// stops working entirely.
pub fn environment_document(form: Shell, environment: &Environment) -> String {
    let libraries = environment.library_paths.join(":");
    let guard = environment
        .library_paths
        .first()
        .cloned()
        .unwrap_or_default();

    let mut document = String::from("#!/bin/sh\n# Written by vmlord-agent. Do not edit.\n#\n");
    document.push_str("# The GPU is checked on every start: this file outlives a reboot and\n");
    document.push_str("# /dev/dxg does not.\n");
    document.push_str(&format!("if [ -e /dev/dxg ] && [ -d {guard} ]; then\n"));

    match form {
        Shell::Generator => {
            document.push_str(&format!("    echo \"LD_LIBRARY_PATH={libraries}\"\n"));
            for (name, value) in FIXED {
                document.push_str(&format!("    echo \"{name}={value}\"\n"));
            }
            if let Some(icd) = &environment.icd {
                document.push_str(&format!("    echo \"VK_DRIVER_FILES={icd}\"\n"));
            }
        }
        Shell::Profile => {
            document.push_str(&format!(
                "    LD_LIBRARY_PATH=\"{libraries}${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}\"\n"
            ));
            document.push_str("    export LD_LIBRARY_PATH\n");
            for (name, value) in FIXED {
                document.push_str(&format!("    export {name}={value}\n"));
            }
            if let Some(icd) = &environment.icd {
                document.push_str(&format!("    export VK_DRIVER_FILES={icd}\n"));
            }
        }
    }

    document.push_str("fi\n");
    document
}

/// Whether `/proc/modules` says the module is loaded.
pub fn module_is_loaded(proc_modules: &str, module: &str) -> bool {
    proc_modules
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|loaded| loaded == module)
}

/// Whether `dkms status` says this kernel already has the module installed.
///
/// Installed and not merely built: a built module that was never installed is
/// not in `/lib/modules`, and `modprobe` would not find it.
pub fn dkms_reports_installed(status: &str, package: &DkmsPackage, kernel: &str) -> bool {
    status.lines().any(|line| {
        line.contains(&format!("{}/{}", package.name, package.version))
            && line.contains(kernel)
            && line.contains("installed")
    })
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{GpuRecipeStageState, GpuRecipeStep};

    use super::{
        Applicability, DkmsPackage, Environment, GpuRecipe, GuestFacts, MesaPolicy, PayloadTarget,
        Report, STEPS, Shell, applicability, dkms_reports_installed, environment_document,
        icd_documents, library_triplet, module_is_loaded, parse_dkms_conf, parse_mesa_policy,
        parse_os_release, parse_payload_target, recipe_for,
    };

    fn ubuntu_guest() -> GuestFacts {
        GuestFacts {
            distribution: "ubuntu".to_owned(),
            release: "26.04".to_owned(),
            architecture: "amd64".to_owned(),
            kernel_release: "7.0.0-14-generic".to_owned(),
        }
    }

    fn payload_for(release: &str, architecture: &str, kernel: &str) -> PayloadTarget {
        PayloadTarget {
            distribution: "ubuntu".to_owned(),
            release: release.to_owned(),
            architecture: architecture.to_owned(),
            kernel_release: kernel.to_owned(),
        }
    }

    #[test]
    fn a_finished_report_has_every_step_exactly_once_and_in_order() {
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Distribution, "ubuntu 26.04 amd64");

        let stages = report.finish("the recipe stopped before this stage");

        assert_eq!(stages.len(), STEPS.len());
        for (stage, step) in stages.iter().zip(STEPS) {
            assert_eq!(stage.step(), step);
        }
        assert_eq!(stages[0].state(), GpuRecipeStageState::Ok);
        assert_eq!(stages[1].state(), GpuRecipeStageState::Skipped);
        assert_eq!(stages[1].message, "the recipe stopped before this stage");
    }

    #[test]
    fn the_steps_a_recipe_never_reached_carry_the_reason_it_stopped() {
        // A report that stopped at the failure would leave the host guessing
        // whether the rest was skipped or the agent hung up.
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Distribution, "ubuntu");
        report.ok(GpuRecipeStep::Payload, "dxgkrnl 2.0.3");
        report.failed(GpuRecipeStep::BuildDependencies, "apt-get exited with 100");

        let stages = report.finish("the build dependencies were not installed");

        assert_eq!(stages[2].state(), GpuRecipeStageState::Failed);
        assert!(stages[2].message.contains("100"));
        for stage in &stages[3..] {
            assert_eq!(stage.state(), GpuRecipeStageState::Skipped);
            assert_eq!(stage.message, "the build dependencies were not installed");
        }
    }

    #[test]
    fn a_stage_recorded_twice_keeps_the_first_answer() {
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Device, "/dev/dxg opens");
        report.failed(GpuRecipeStep::Device, "gone");

        let stages = report.finish("unreached");

        let device: Vec<_> = stages
            .iter()
            .filter(|stage| stage.step() == GpuRecipeStep::Device)
            .collect();
        assert_eq!(device.len(), 1);
        assert_eq!(device[0].state(), GpuRecipeStageState::Ok);
    }

    #[test]
    fn os_release_values_are_read_with_or_without_quotes() {
        let text = "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\nVERSION_ID=\"26.04\"\n";

        assert_eq!(
            parse_os_release(text),
            Some(("ubuntu".to_owned(), "26.04".to_owned()))
        );
    }

    #[test]
    fn an_os_release_without_an_id_names_nothing() {
        assert_eq!(parse_os_release("VERSION_ID=\"26.04\"\n"), None);
        assert_eq!(parse_os_release(""), None);
    }

    #[test]
    fn only_ubuntu_has_a_recipe_in_this_build() {
        assert!(matches!(recipe_for("ubuntu"), Some(GpuRecipe::Ubuntu)));
        assert!(recipe_for("debian").is_none());
        assert!(recipe_for("").is_none());
    }

    #[test]
    fn a_payload_built_for_this_guest_applies() {
        let applies = applicability(
            &payload_for("26.04", "amd64", "7.0.0-14-generic"),
            &ubuntu_guest(),
        );

        assert!(matches!(applies, Applicability::Applies { kernel: None }));
    }

    #[test]
    fn another_kernel_is_recorded_and_never_refuses() {
        // DKMS builds against the headers of the running kernel, so an exact
        // match is not needed to compile -- and requiring one would mean an
        // unattended kernel upgrade kills GPU-PV until a payload is repacked.
        let applies = applicability(
            &payload_for("26.04", "amd64", "7.0.0-11-generic"),
            &ubuntu_guest(),
        );

        let Applicability::Applies { kernel: Some(note) } = applies else {
            panic!("a different kernel must still apply");
        };
        assert!(note.contains("7.0.0-11-generic"), "{note}");
        assert!(note.contains("7.0.0-14-generic"), "{note}");
    }

    #[test]
    fn another_release_or_architecture_does_not_apply() {
        for payload in [
            payload_for("24.04", "amd64", "7.0.0-14-generic"),
            payload_for("26.04", "arm64", "7.0.0-14-generic"),
        ] {
            assert!(matches!(
                applicability(&payload, &ubuntu_guest()),
                Applicability::NotApplicable(_)
            ));
        }
    }

    #[test]
    fn a_payload_target_is_read_out_of_its_sources_document() {
        let document = r#"{
          "schema_version": 1,
          "target": {
            "distribution": "ubuntu",
            "release": "26.04",
            "architecture": "amd64",
            "kernel_release": "7.0.0-14-generic",
            "payload_abi": 1
          },
          "mesa_policy": "bundled"
        }"#;

        let target = parse_payload_target(document).expect("a readable target");

        assert_eq!(target.release, "26.04");
        assert_eq!(target.kernel_release, "7.0.0-14-generic");
    }

    #[test]
    fn a_sources_document_without_a_target_is_an_error() {
        for document in ["{}", "{\"target\": {}}", "not json"] {
            assert!(parse_payload_target(document).is_err(), "{document}");
        }
    }

    #[test]
    fn a_dkms_conf_names_its_package_and_version() {
        let text = "PACKAGE_NAME=\"dxgkrnl\"\nPACKAGE_VERSION=2.0.3\nAUTOINSTALL=\"yes\"\n";

        let package = parse_dkms_conf(text).expect("a readable dkms.conf");

        assert_eq!(package.name, "dxgkrnl");
        assert_eq!(package.version, "2.0.3");
    }

    #[test]
    fn a_dkms_conf_missing_a_field_is_an_error() {
        assert!(parse_dkms_conf("PACKAGE_NAME=dxgkrnl\n").is_err());
        assert!(parse_dkms_conf("").is_err());
    }

    #[test]
    fn a_loaded_module_is_recognised_by_name_alone() {
        let modules = "dxgkrnl 315392 0 - Live 0x0000000000000000\nvsock 45056 2 - Live 0x0\n";

        assert!(module_is_loaded(modules, "dxgkrnl"));
        assert!(!module_is_loaded(modules, "dxg"));
        assert!(!module_is_loaded("", "dxgkrnl"));
    }

    #[test]
    fn dkms_status_says_whether_this_kernel_already_has_the_module() {
        let package = DkmsPackage {
            name: "dxgkrnl".to_owned(),
            version: "2.0.3".to_owned(),
        };

        assert!(dkms_reports_installed(
            "dxgkrnl/2.0.3, 7.0.0-14-generic, x86_64: installed",
            &package,
            "7.0.0-14-generic"
        ));
        // Built for another kernel, or built and not installed: both are work
        // still to do for the kernel this guest is running.
        assert!(!dkms_reports_installed(
            "dxgkrnl/2.0.3, 7.0.0-11-generic, x86_64: installed",
            &package,
            "7.0.0-14-generic"
        ));
        assert!(!dkms_reports_installed(
            "dxgkrnl/2.0.3, 7.0.0-14-generic, x86_64: built",
            &package,
            "7.0.0-14-generic"
        ));
        assert!(!dkms_reports_installed("", &package, "7.0.0-14-generic"));
    }

    #[test]
    fn a_payload_names_the_mesa_policy_it_was_built_with() {
        assert_eq!(
            parse_mesa_policy(r#"{"mesa_policy": "bundled"}"#),
            Ok(MesaPolicy::Bundled)
        );
        assert_eq!(
            parse_mesa_policy(r#"{"mesa_policy":"distro","target":{}}"#),
            Ok(MesaPolicy::Distro)
        );
    }

    #[test]
    fn a_policy_this_build_does_not_know_is_an_error_and_not_a_guess() {
        // A payload built newer than this agent must fail the stage it belongs
        // to rather than be treated as one of the policies that exist today.
        for document in [r#"{"mesa_policy": "flatpak"}"#, "{}", "not json"] {
            assert!(parse_mesa_policy(document).is_err(), "{document}");
        }
    }

    #[test]
    fn every_architecture_with_a_recipe_has_a_library_path() {
        assert_eq!(library_triplet("amd64"), Some("x86_64-linux-gnu"));
        assert_eq!(library_triplet("arm64"), Some("aarch64-linux-gnu"));
        assert_eq!(library_triplet("riscv64"), None);
        assert_eq!(library_triplet(""), None);
    }

    #[test]
    fn the_icd_documents_of_a_directory_are_its_json_files_in_order() {
        // The names come from the payload rather than a constant: Mesa has
        // shipped this file under more than one name.
        let names = [
            "dzn_icd.x86_64.json".to_owned(),
            "README".to_owned(),
            "lvp_icd.x86_64.json".to_owned(),
            "notes.json.bak".to_owned(),
        ];

        assert_eq!(
            icd_documents(&names),
            vec![
                "dzn_icd.x86_64.json".to_owned(),
                "lvp_icd.x86_64.json".to_owned()
            ]
        );
        assert!(icd_documents(&[]).is_empty());
    }

    #[test]
    fn the_generator_prints_what_a_session_inherits() {
        let document = environment_document(
            Shell::Generator,
            &Environment {
                library_paths: vec![
                    "/opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu".to_owned(),
                    "/usr/lib/wsl/lib".to_owned(),
                ],
                icd: Some("/etc/vulkan/icd.d/dzn_icd.x86_64.json".to_owned()),
            },
        );

        assert!(document.starts_with("#!/bin/sh\n"), "{document}");
        // The probe runs on every start: this file outlives a reboot and
        // /dev/dxg does not.
        assert!(document.contains("[ -e /dev/dxg ]"), "{document}");
        assert!(
            document.contains("[ -d /opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu ]"),
            "{document}"
        );
        assert!(
            document.contains(
                "echo \"LD_LIBRARY_PATH=/opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu:/usr/lib/wsl/lib\""
            ),
            "{document}"
        );
        // Both, always: the first is gallium selection on the GLX path, the
        // second is the DRI loader EGL and Wayland clients use.
        assert!(
            document.contains("echo \"GALLIUM_DRIVER=d3d12\""),
            "{document}"
        );
        assert!(
            document.contains("echo \"MESA_LOADER_DRIVER_OVERRIDE=d3d12\""),
            "{document}"
        );
        assert!(
            document.contains("echo \"__GLX_VENDOR_LIBRARY_NAME=mesa\""),
            "{document}"
        );
        assert!(
            document.contains("echo \"VK_DRIVER_FILES=/etc/vulkan/icd.d/dzn_icd.x86_64.json\""),
            "{document}"
        );
    }

    #[test]
    fn the_profile_script_exports_and_never_exits_the_shell_it_is_sourced_by() {
        let document = environment_document(
            Shell::Profile,
            &Environment {
                library_paths: vec!["/usr/lib/wsl/lib".to_owned()],
                icd: None,
            },
        );

        // Sourced by /etc/profile: an `exit` here would end the login shell.
        assert!(!document.contains("exit"), "{document}");
        assert!(
            document.contains(
                "LD_LIBRARY_PATH=\"/usr/lib/wsl/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\""
            ),
            "{document}"
        );
        assert!(document.contains("export LD_LIBRARY_PATH"), "{document}");
        assert!(
            document.contains("export GALLIUM_DRIVER=d3d12"),
            "{document}"
        );
        // Nothing was registered, so nothing is pinned.
        assert!(!document.contains("VK_DRIVER_FILES"), "{document}");
    }

    #[test]
    fn the_same_environment_writes_the_same_document() {
        // What makes the second start of a VM report the stage as skipped.
        let environment = Environment {
            library_paths: vec!["/usr/lib/wsl/lib".to_owned()],
            icd: None,
        };

        assert_eq!(
            environment_document(Shell::Generator, &environment),
            environment_document(Shell::Generator, &environment)
        );
        assert_ne!(
            environment_document(Shell::Generator, &environment),
            environment_document(Shell::Profile, &environment)
        );
    }

    #[test]
    fn the_userspace_steps_a_failed_device_never_reached_carry_its_reason() {
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Distribution, "ubuntu");
        report.failed(GpuRecipeStep::Device, "/dev/dxg is missing");

        let stages = report.finish("/dev/dxg never appeared");

        assert_eq!(stages.len(), STEPS.len());
        for stage in &stages[STEPS.len() - 3..] {
            assert_eq!(stage.state(), GpuRecipeStageState::Skipped);
            assert_eq!(stage.message, "/dev/dxg never appeared");
        }
        assert_eq!(stages[STEPS.len() - 3].step(), GpuRecipeStep::Userspace);
        assert_eq!(stages[STEPS.len() - 1].step(), GpuRecipeStep::Environment);
    }
}
