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
pub const STEPS: [GpuRecipeStep; 7] = [
    GpuRecipeStep::Distribution,
    GpuRecipeStep::Payload,
    GpuRecipeStep::BuildDependencies,
    GpuRecipeStep::ModuleSource,
    GpuRecipeStep::ModuleBuild,
    GpuRecipeStep::ModuleLoad,
    GpuRecipeStep::Device,
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
        Applicability, DkmsPackage, GpuRecipe, GuestFacts, PayloadTarget, Report, STEPS,
        applicability, dkms_reports_installed, module_is_loaded, parse_dkms_conf, parse_os_release,
        parse_payload_target, recipe_for,
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
}
