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
pub const STEPS: [DisplayRecipeStep; 12] = [
    DisplayRecipeStep::Distribution,
    DisplayRecipeStep::Payload,
    DisplayRecipeStep::BuildDependencies,
    DisplayRecipeStep::SigningKey,
    DisplayRecipeStep::ModuleSource,
    DisplayRecipeStep::ModuleBuild,
    DisplayRecipeStep::ModuleSignature,
    DisplayRecipeStep::Initramfs,
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

/// Where the guest's own module-signing MOK lives.
///
/// Not a path VMLord chose: it is what `dkms` on 22.04, 24.04 and 26.04 all
/// sign with by default, which is why VMLord configures no signing of its own
/// and writes neither `framework.conf` nor a `framework.conf.d` file.
pub const SIGNING_KEY: &str = "/var/lib/shim-signed/mok/MOK.priv";

/// The certificate half of that pair, and the only half that ever leaves.
pub const SIGNING_CERTIFICATE: &str = "/var/lib/shim-signed/mok/MOK.der";

/// What the guest has of a signing pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigningKeyState {
    /// Both halves are there, and are what the modules are signed with.
    Complete,
    /// One half is there. A certificate cannot be derived from a private key,
    /// so this is a broken pair rather than half a good one: both halves are
    /// replaced, and the enrollment has to be performed again.
    HalfPresent,
    /// Neither half is there, which is every guest before its first build.
    Absent,
}

#[must_use]
pub fn signing_key_state(private_key_exists: bool, certificate_exists: bool) -> SigningKeyState {
    match (private_key_exists, certificate_exists) {
        (true, true) => SigningKeyState::Complete,
        (false, false) => SigningKeyState::Absent,
        _ => SigningKeyState::HalfPresent,
    }
}

/// The subject key identifier out of `openssl x509 -noout -text` output.
///
/// Lower-case and without separators, which is the form [`signature_matches`]
/// compares in. `None` is a certificate carrying no subject key identifier at
/// all -- one generated without `/usr/lib/shim/mok/openssl.cnf` -- and means
/// there is nothing a signature can be matched against.
#[must_use]
pub fn parse_subject_key_identifier(text: &str) -> Option<String> {
    let mut lines = text
        .lines()
        .skip_while(|line| !line.contains("Subject Key Identifier"));
    lines.next()?;
    let identifier = hex_only(lines.next()?);
    (!identifier.is_empty()).then_some(identifier)
}

/// The key `modinfo` says signed this module, in the same form.
///
/// `sign-file` writes the certificate's subject key identifier when it has
/// one, which is why the certificate is generated with
/// `/usr/lib/shim/mok/openssl.cnf` and its `subjectKeyIdentifier = hash`.
#[must_use]
pub fn parse_module_signature_key(modinfo: &str) -> Option<String> {
    let key = hex_only(
        modinfo
            .lines()
            .find_map(|line| line.strip_prefix("sig_key:"))?,
    );
    (!key.is_empty()).then_some(key)
}

/// Whether this module is signed by the certificate the guest holds.
///
/// An empty identifier never matches: a certificate with no subject key
/// identifier gives nothing to compare, and reading that as agreement would
/// report every module as signed by it.
#[must_use]
pub fn signature_matches(modinfo: &str, subject_key_identifier: &str) -> bool {
    !subject_key_identifier.is_empty()
        && parse_module_signature_key(modinfo).as_deref() == Some(subject_key_identifier)
}

/// What the kernel says when it refuses a module over its signature.
///
/// `EKEYREJECTED` is a module signed by a key the kernel does not trust;
/// `ENOKEY` is a module with no signature at all under a kernel that demands
/// one. Both mean one thing to a user: the certificate is not enrolled.
///
/// Written out a second time in `vmlord_core::display`, which reads these same
/// phrases back out of the message this crate wrote. Two copies because
/// `vmlord-agent` deliberately depends on no host crate -- the same trade as
/// the mode bounds below. Change one and change the other.
pub const SIGNATURE_REJECTION_PHRASES: [&str; 2] =
    ["Key was rejected by service", "Required key not available"];

#[must_use]
pub fn was_rejected_for_its_signature(output: &str) -> bool {
    SIGNATURE_REJECTION_PHRASES
        .iter()
        .any(|phrase| output.contains(phrase))
}

/// Whether Secure Boot is on, as `mokutil --sb-state` reports it.
///
/// `None` is a firmware with no Secure Boot to report on, which is every
/// VMLord VM today and is not a failure.
#[must_use]
pub fn parse_secure_boot_state(mokutil: &str) -> Option<bool> {
    if mokutil.contains("SecureBoot enabled") {
        Some(true)
    } else if mokutil.contains("SecureBoot disabled") {
        Some(false)
    } else {
        None
    }
}

/// A key identifier as both `openssl` and `modinfo` mean it, whichever
/// separators and case they happened to print it with.
fn hex_only(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_hexdigit)
        .flat_map(char::to_lowercase)
        .collect()
}

/// What the output comes up at when the host has not said, or has said
/// something this module will not drive.
pub const FALLBACK_MODE: (u32, u32) = (1920, 1080);

/// The bounds `vmlord_drm`'s `mode_config` carries.
///
/// Written out a second time rather than taken from `vmlord-core`: this crate
/// depends on `libc`, `serde_json`, `sha2` and the protocol crate and on
/// nothing else, because it cross-compiles to static musl, and a dependency
/// for four numbers would be the wrong trade. Change these and
/// `vmlord_core::MIN_DISPLAY_WIDTH` and `VMLORD_MIN_WIDTH` in `vmlord_drm.c`
/// together.
const MIN_WIDTH: u32 = 640;
const MIN_HEIGHT: u32 = 480;
const MAX_WIDTH: u32 = 2560;
const MAX_HEIGHT: u32 = 1440;

/// The mode to bring the output up at, given what the host asked for.
///
/// The host sends only modes it validated, and this checks again anyway: a
/// module parameter outside what the module drives is a device that exists and
/// shows nothing, and the fallback is a working desktop.
#[must_use]
pub fn wanted_mode(asked: Option<(u32, u32)>) -> (u32, u32) {
    match asked {
        Some((width, height))
            if (MIN_WIDTH..=MAX_WIDTH).contains(&width)
                && (MIN_HEIGHT..=MAX_HEIGHT).contains(&height) =>
        {
            (width, height)
        }
        _ => FALLBACK_MODE,
    }
}

/// What `/etc/modprobe.d/vmlord-display.conf` says, for a mode.
///
/// Written by the guest from what the host asked for, rather than copied out
/// of the payload: the size belongs to one VM and a payload is shared by all
/// of them.
#[must_use]
pub fn modprobe_options(width: u32, height: u32) -> String {
    format!(
        "# Written by vmlord-agent from the mode this VM has stored.\n\
         # The output comes up at this size; changing it needs the module reloaded.\n\
         options {MODULE} width={width} height={height}\n"
    )
}

/// The size the loaded module was given, out of its `parameters` directory.
///
/// `None` is a module that does not say: absent files, or text that is not a
/// number. Deliberately not a guess -- see [`needs_reload`].
#[must_use]
pub fn parse_module_parameters(width: &str, height: &str) -> Option<(u32, u32)> {
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// Whether a module that is already up has to be reloaded to reach `wanted`.
///
/// A module parameter is read once, when the module loads, so a stored mode
/// that changed under a running module reaches the output no other way. A
/// module that does not say what it was loaded with is left alone: a reload on
/// a guess is a desktop dropped for nothing.
#[must_use]
pub fn needs_reload(loaded: Option<(u32, u32)>, wanted: (u32, u32)) -> bool {
    loaded.is_some_and(|loaded| loaded != wanted)
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{DisplayRecipeStageState, DisplayRecipeStep};

    use super::{
        DKMS_PACKAGE, FALLBACK_MODE, InstalledVersions, Report, STEPS, SigningKeyState, applies_to,
        dkms_reports_installed, dkms_versions, has_recipe, modprobe_options, module_is_loaded,
        needs_build, needs_reload, parse_module_parameters, parse_module_signature_key,
        parse_secure_boot_state, parse_subject_key_identifier, read_payload_facts,
        signature_matches, signing_key_state, wanted_mode, was_rejected_for_its_signature,
    };

    #[test]
    fn a_mode_the_module_will_not_drive_falls_back() {
        assert_eq!(wanted_mode(Some((2560, 1440))), (2560, 1440));
        assert_eq!(wanted_mode(Some((640, 480))), (640, 480));
        assert_eq!(wanted_mode(None), FALLBACK_MODE);
        assert_eq!(
            wanted_mode(Some((3840, 2160))),
            FALLBACK_MODE,
            "the host sends only what it validated, and the guest checks anyway"
        );
        assert_eq!(wanted_mode(Some((0, 0))), FALLBACK_MODE);
    }

    #[test]
    fn the_modprobe_options_name_the_module_and_the_mode() {
        let options = modprobe_options(1600, 900);

        assert!(options.ends_with("options vmlord_drm width=1600 height=900\n"));
        assert!(
            options.starts_with('#'),
            "a file VMLord wrote should say so to whoever finds it"
        );
    }

    #[test]
    fn the_loaded_mode_is_what_the_module_says_it_is() {
        assert_eq!(
            parse_module_parameters("1920\n", "1080\n"),
            Some((1920, 1080))
        );
        assert_eq!(parse_module_parameters("", ""), None);
        assert_eq!(parse_module_parameters("wide", "1080"), None);
    }

    #[test]
    fn only_a_mode_that_is_known_to_differ_costs_a_reload() {
        assert!(needs_reload(Some((1920, 1080)), (2560, 1440)));
        assert!(!needs_reload(Some((1920, 1080)), (1920, 1080)));
        assert!(
            !needs_reload(None, (2560, 1440)),
            "a module that does not say must not be dropped on a guess"
        );
    }

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

    #[test]
    fn a_key_without_its_certificate_is_a_broken_pair_and_not_half_a_good_one() {
        assert_eq!(signing_key_state(true, true), SigningKeyState::Complete);
        assert_eq!(signing_key_state(false, false), SigningKeyState::Absent);
        assert_eq!(signing_key_state(true, false), SigningKeyState::HalfPresent);
        assert_eq!(signing_key_state(false, true), SigningKeyState::HalfPresent);
    }

    #[test]
    fn the_subject_key_identifier_is_read_out_of_what_openssl_prints() {
        let printed = "X509v3 Subject Key Identifier: \n    \
                       0A:1B:2C:3D:4E:5F:60:71:82:93:A4:B5:C6:D7:E8:F9:00:11:22:33\n";

        assert_eq!(
            parse_subject_key_identifier(printed).as_deref(),
            Some("0a1b2c3d4e5f60718293a4b5c6d7e8f900112233")
        );
    }

    #[test]
    fn a_certificate_with_no_subject_key_identifier_yields_nothing_to_match_on() {
        assert_eq!(parse_subject_key_identifier(""), None);
        assert_eq!(
            parse_subject_key_identifier("X509v3 Subject Key Identifier: \n"),
            None
        );
        assert_eq!(
            parse_subject_key_identifier("X509v3 Basic Constraints: critical\n    CA:FALSE\n"),
            None
        );
    }

    #[test]
    fn a_signed_module_names_the_key_that_signed_it() {
        let modinfo = "filename:       /lib/modules/6.8.0-79-generic/updates/dkms/vmlord_drm.ko\n\
                       version:        0.1.0\n\
                       sig_id:         PKCS#7\n\
                       signer:         DKMS module signing key\n\
                       sig_key:        0A:1B:2C:3D:4E:5F\n\
                       sig_hashalgo:   sha512\n";

        assert_eq!(
            parse_module_signature_key(modinfo).as_deref(),
            Some("0a1b2c3d4e5f")
        );
        assert!(signature_matches(modinfo, "0a1b2c3d4e5f"));
    }

    #[test]
    fn an_unsigned_module_matches_nothing() {
        let modinfo = "filename:       /lib/modules/6.8.0-79-generic/updates/dkms/vmlord_drm.ko\n\
                       version:        0.1.0\n";

        assert_eq!(parse_module_signature_key(modinfo), None);
        assert!(!signature_matches(modinfo, "0a1b2c3d4e5f"));
    }

    #[test]
    fn a_module_signed_by_some_other_key_is_not_one_we_can_vouch_for() {
        let modinfo = "sig_key:        FF:EE:DD\n";

        assert!(!signature_matches(modinfo, "0a1b2c3d4e5f"));
        assert!(
            !signature_matches(modinfo, ""),
            "an empty identifier matches nothing, or every module would pass"
        );
    }

    #[test]
    fn the_kernel_refusing_a_signature_reads_differently_from_every_other_refusal() {
        assert!(was_rejected_for_its_signature(
            "modprobe: ERROR: could not insert 'vmlord_drm': Key was rejected by service"
        ));
        assert!(was_rejected_for_its_signature(
            "modprobe: ERROR: could not insert 'vmlord_drm': Required key not available"
        ));
    }

    #[test]
    fn every_other_way_a_module_fails_to_load_is_not_a_signature_problem() {
        assert!(!was_rejected_for_its_signature(
            "modprobe: ERROR: could not insert 'vmlord_drm': Invalid argument"
        ));
        assert!(!was_rejected_for_its_signature(
            "modprobe: FATAL: Module vmlord_drm not found in directory /lib/modules/6.8.0-79-generic"
        ));
        assert!(!was_rejected_for_its_signature(""));
    }

    #[test]
    fn secure_boot_is_read_out_of_mokutil_and_absent_when_it_says_nothing() {
        assert_eq!(parse_secure_boot_state("SecureBoot enabled\n"), Some(true));
        assert_eq!(parse_secure_boot_state("SecureBoot disabled\n"), Some(false));
        assert_eq!(
            parse_secure_boot_state("This system doesn't support Secure Boot\n"),
            None
        );
        assert_eq!(parse_secure_boot_state(""), None);
    }

}
