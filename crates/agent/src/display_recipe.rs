//! Which guests the display recipe applies to, what the mounted payload says
//! it is, and what a guest already has of it.
//!
//! Everything here is a function of text: `/etc/os-release`, the payload's
//! `payload.json`, the output of `dkms status`, `/proc/modules`,
//! `/sys/class/drm`. That is deliberate -- it is what makes the recipe's
//! decisions testable on a machine that is neither Ubuntu nor a Hyper-V guest,
//! while `display_kernel` keeps the parts that need one.

use vmlord_agent_protocol::v1::{DisplayRecipeStage, DisplayRecipeStageState, DisplayRecipeStep};

/// The DKMS package a display payload installs.
pub const DKMS_PACKAGE: &str = "vmlord-display";

/// The kernel module that package builds.
pub const MODULE: &str = "vmlord_drm";

/// Every step of the recipe, in the order it is attempted.
///
/// The order is the report's order, and the report is what the host logs, so it
/// is written once here rather than implied by the sequence of calls in
/// `display_kernel`.
pub const STEPS: [DisplayRecipeStep; 9] = [
    DisplayRecipeStep::Distribution,
    DisplayRecipeStep::Payload,
    DisplayRecipeStep::BuildDependencies,
    DisplayRecipeStep::ModuleSource,
    DisplayRecipeStep::ModuleBuild,
    DisplayRecipeStep::ModuleLoad,
    DisplayRecipeStep::Device,
    DisplayRecipeStep::Services,
    DisplayRecipeStep::ServicesStart,
];

/// What a recipe run has found out so far.
///
/// Collected rather than sent as it goes, because a stage list is one answer to
/// one request: the host asked what the recipe did, not to be narrated at.
#[derive(Default)]
pub struct Report {
    recorded: Vec<DisplayRecipeStage>,
}

impl Report {
    #[must_use]
    pub fn new() -> Self {
        Self {
            recorded: Vec::with_capacity(STEPS.len()),
        }
    }

    pub fn ok(&mut self, step: DisplayRecipeStep, message: impl Into<String>) {
        self.record(step, DisplayRecipeStageState::Ok, message.into());
    }

    pub fn skipped(&mut self, step: DisplayRecipeStep, message: impl Into<String>) {
        self.record(step, DisplayRecipeStageState::Skipped, message.into());
    }

    pub fn failed(&mut self, step: DisplayRecipeStep, message: impl Into<String>) {
        self.record(step, DisplayRecipeStageState::Failed, message.into());
    }

    /// Keeps the first answer a step was given.
    ///
    /// Nothing should record a step twice; if something does, the report must
    /// not grow a second copy of a step the host reads once.
    fn record(&mut self, step: DisplayRecipeStep, state: DisplayRecipeStageState, message: String) {
        if self.recorded.iter().any(|stage| stage.step() == step) {
            return;
        }
        self.recorded.push(DisplayRecipeStage {
            step: i32::from(step),
            state: i32::from(state),
            message,
        });
    }

    /// The whole report: what happened, and `reason` for what never ran.
    #[must_use]
    pub fn finish(mut self, reason: &str) -> Vec<DisplayRecipeStage> {
        for step in STEPS {
            self.skipped(step, reason);
        }
        self.recorded
            .sort_by_key(|stage| STEPS.iter().position(|step| *step == stage.step()));
        self.recorded
    }
}

/// A distribution this build knows how to bring a display up on.
///
/// The whole "an unsupported release degrades the display and does not stop the
/// VM" rule starts here: a guest with no recipe is a skipped first stage, not
/// an error.
#[must_use]
pub fn has_recipe(distribution: &str) -> bool {
    distribution == "ubuntu"
}

/// What the mounted payload says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadFacts {
    pub payload_id: String,
    pub version: String,
    pub distribution: String,
    pub release: String,
    pub architecture: String,
}

impl PayloadFacts {
    /// The `/usr/src` tree this payload's sources are copied to.
    ///
    /// Named for the version, which is what lets two versions sit beside each
    /// other -- and a rollback be a `modprobe` rather than a download.
    #[must_use]
    pub fn source_directory(&self) -> String {
        format!("/usr/src/{DKMS_PACKAGE}-{}", self.version)
    }
}

/// Reads the mounted payload's `payload.json`.
///
/// # Errors
///
/// The reason the stage failed, in the words the host will log.
pub fn read_payload_facts(bytes: &[u8]) -> Result<PayloadFacts, String> {
    let document: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| format!("payload.json: {error}"))?;
    let string = |pointer: &str| {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("payload.json says nothing at {pointer}"))
    };

    Ok(PayloadFacts {
        payload_id: string("/payload_id")?,
        version: string("/version")?,
        distribution: string("/target/distribution")?,
        release: string("/target/release")?,
        architecture: string("/target/architecture")?,
    })
}

/// Whether a payload built for one guest applies to this one.
///
/// Distribution, release and architecture, and never the kernel: DKMS builds
/// against the headers of the running kernel, and the kernel a payload records
/// is what it was proven on.
#[must_use]
pub fn applies_to(
    payload: &PayloadFacts,
    distribution: &str,
    release: &str,
    architecture: &str,
) -> bool {
    payload.distribution.eq_ignore_ascii_case(distribution)
        && payload.release == release
        && payload.architecture.eq_ignore_ascii_case(architecture)
}

/// What a guest already has of the display payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstalledVersions {
    /// Every version DKMS has registered, newest last is not assumed: order is
    /// whatever `dkms status` printed.
    pub versions: Vec<String>,
    /// The version of the module that is loaded, when one is.
    pub loaded: Option<String>,
}

impl InstalledVersions {
    /// The version a rollback would land on, given the one that is current.
    ///
    /// One step and no further: keeping more would be a version history, and
    /// there is nothing in an MVP to build one from.
    #[must_use]
    pub fn previous(&self, current: &str) -> Option<String> {
        self.versions
            .iter()
            .find(|version| *version != current)
            .cloned()
    }
}

/// Every version of `package` that `dkms status` reports as installed.
#[must_use]
pub fn dkms_versions(status: &str, package: &str) -> Vec<String> {
    let prefix = format!("{package}/");
    let mut versions = Vec::new();
    for line in status.lines() {
        let Some(rest) = line.trim().strip_prefix(&prefix) else {
            continue;
        };
        let Some(version) = rest.split([',', ':', ' ']).next() else {
            continue;
        };
        if !version.is_empty() && !versions.iter().any(|seen| seen == version) {
            versions.push(version.to_owned());
        }
    }
    versions
}

/// Whether `dkms status` says this kernel already has the version installed.
///
/// Installed and not merely built: a built module that was never installed is
/// not in `/lib/modules`, and `modprobe` would not find it.
#[must_use]
pub fn dkms_reports_installed(status: &str, package: &str, version: &str, kernel: &str) -> bool {
    status.lines().any(|line| {
        line.contains(&format!("{package}/{version}"))
            && line.contains(kernel)
            && line.contains("installed")
    })
}

/// Whether the module is in `/proc/modules`.
#[must_use]
pub fn module_is_loaded(proc_modules: &str) -> bool {
    proc_modules
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|loaded| loaded == MODULE)
}

/// Whether anything has to be built for the guest to be running `wanted`.
///
/// By fact and not by a flag: the wanted version installed, the module loaded
/// and a device that answers is a guest that needs nothing, and every other
/// combination is a guest that does. A kernel upgrade DKMS did not carry shows
/// up here as a module that is not loaded.
#[must_use]
pub fn needs_build(installed: &InstalledVersions, wanted: &str, device_present: bool) -> bool {
    let running_wanted = installed.loaded.as_deref() == Some(wanted);
    let has_wanted = installed.versions.iter().any(|version| version == wanted);
    !(has_wanted && running_wanted && device_present)
}

/// The version of the loaded module, out of `/sys/module/vmlord_drm/version`.
#[must_use]
pub fn parse_module_version(text: &str) -> Option<String> {
    let version = text.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{DisplayRecipeStageState, DisplayRecipeStep};

    use super::{
        DKMS_PACKAGE, InstalledVersions, Report, STEPS, applies_to, dkms_reports_installed,
        dkms_versions, has_recipe, module_is_loaded, needs_build, read_payload_facts,
    };

    fn payload_json(version: &str, release: &str) -> Vec<u8> {
        format!(
            r#"{{"schema_version":1,"payload_id":"display-ubuntu-{release}-amd64-{version}",
             "version":"{version}","target":{{"distribution":"ubuntu","release":"{release}",
             "architecture":"amd64","payload_abi":1}},"files":[]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn a_payload_json_says_which_version_is_mounted() {
        let facts = read_payload_facts(&payload_json("0.1.0", "24.04")).expect("it parses");

        assert_eq!(facts.version, "0.1.0");
        assert_eq!(facts.payload_id, "display-ubuntu-24.04-amd64-0.1.0");
        assert_eq!(facts.source_directory(), "/usr/src/vmlord-display-0.1.0");
    }

    #[test]
    fn a_payload_that_says_nothing_useful_is_refused() {
        assert!(read_payload_facts(b"{not json").is_err());
        assert!(
            read_payload_facts(br#"{"payload_id":"p","target":{}}"#).is_err(),
            "a payload with no version is a payload nothing can be installed from"
        );
    }

    #[test]
    fn a_payload_applies_by_triple_and_never_by_kernel() {
        let facts = read_payload_facts(&payload_json("0.1.0", "24.04")).unwrap();

        assert!(applies_to(&facts, "ubuntu", "24.04", "amd64"));
        assert!(applies_to(&facts, "Ubuntu", "24.04", "AMD64"));
        assert!(!applies_to(&facts, "ubuntu", "22.04", "amd64"));
        assert!(!applies_to(&facts, "debian", "24.04", "amd64"));
    }

    #[test]
    fn dkms_status_yields_every_installed_version_of_our_package() {
        let status = "\
vmlord-display/0.1.0, 6.8.0-137-generic, x86_64: installed
vmlord-display/0.2.0, 6.8.0-137-generic, x86_64: installed
vmlord-display/0.2.0, 6.8.0-140-generic, x86_64: installed
other-module/1.0, 6.8.0-137-generic, x86_64: installed";

        assert_eq!(
            dkms_versions(status, DKMS_PACKAGE),
            vec!["0.1.0".to_owned(), "0.2.0".to_owned()],
            "one entry per version, however many kernels it is built for"
        );
        assert!(dkms_reports_installed(
            status,
            DKMS_PACKAGE,
            "0.2.0",
            "6.8.0-140-generic"
        ));
        assert!(
            !dkms_reports_installed(status, DKMS_PACKAGE, "0.1.0", "6.8.0-140-generic"),
            "a version built for another kernel is not installed for this one"
        );
    }

    #[test]
    fn a_rollback_lands_on_the_one_other_version_dkms_holds() {
        let installed = InstalledVersions {
            versions: vec!["0.1.0".into(), "0.2.0".into()],
            loaded: Some("0.2.0".into()),
        };

        assert_eq!(installed.previous("0.2.0").as_deref(), Some("0.1.0"));
        assert_eq!(
            InstalledVersions {
                versions: vec!["0.2.0".into()],
                loaded: Some("0.2.0".into()),
            }
            .previous("0.2.0"),
            None,
            "a guest that has only ever had one version has nothing to roll back to"
        );
    }

    #[test]
    fn a_guest_that_already_runs_the_payloads_version_needs_no_build() {
        let installed = InstalledVersions {
            versions: vec!["0.1.0".into()],
            loaded: Some("0.1.0".into()),
        };

        assert!(!needs_build(&installed, "0.1.0", true));
        assert!(
            needs_build(&installed, "0.2.0", true),
            "a newer payload is a build"
        );
        assert!(
            needs_build(&installed, "0.1.0", false),
            "a kernel upgrade that left the module unbuilt shows up as no device"
        );
        assert!(
            needs_build(
                &InstalledVersions {
                    versions: vec!["0.1.0".into()],
                    loaded: None,
                },
                "0.1.0",
                true
            ),
            "installed and not loaded is a guest that still needs the load stages"
        );
    }

    #[test]
    fn a_loaded_module_is_the_one_named_in_proc_modules() {
        assert!(module_is_loaded(
            "vmlord_drm 20480 0 - Live 0x0000\ndrm 1 0"
        ));
        assert!(!module_is_loaded("hyperv_drm 20480 0 - Live 0x0000"));
    }

    #[test]
    fn only_ubuntu_has_a_display_recipe_today() {
        assert!(has_recipe("ubuntu"));
        assert!(!has_recipe("fedora"));
    }

    #[test]
    fn a_report_names_every_step_even_the_ones_that_never_ran() {
        let mut report = Report::new();
        report.ok(DisplayRecipeStep::Distribution, "ubuntu 24.04 amd64");
        report.failed(DisplayRecipeStep::ModuleBuild, "dkms build failed");

        let stages = report.finish("the recipe stopped before this stage");

        assert_eq!(stages.len(), STEPS.len());
        assert_eq!(stages[0].step(), DisplayRecipeStep::Distribution);
        assert_eq!(stages[0].state(), DisplayRecipeStageState::Ok);
        assert!(stages.iter().all(|stage| !stage.message.is_empty()));
        assert_eq!(
            stages
                .iter()
                .filter(|stage| stage.state() == DisplayRecipeStageState::Failed)
                .count(),
            1
        );
    }

    #[test]
    fn a_step_answered_twice_keeps_its_first_answer() {
        let mut report = Report::new();
        report.failed(DisplayRecipeStep::Device, "no device");
        report.ok(DisplayRecipeStep::Device, "a device appeared after all");

        let stages = report.finish("skipped");

        let device = stages
            .iter()
            .find(|stage| stage.step() == DisplayRecipeStep::Device)
            .unwrap();
        assert_eq!(device.state(), DisplayRecipeStageState::Failed);
    }
}
