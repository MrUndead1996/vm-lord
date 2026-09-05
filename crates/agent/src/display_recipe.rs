//! Which guests the display recipe applies to, what the mounted payload says
//! it is, and what a guest already has of it.
//!
//! Everything here is a function of text: `/etc/os-release`, the payload's
//! `payload.json`, the output of `dkms status`, `/proc/modules`,
//! `/sys/class/drm`. That is deliberate -- it is what makes the recipe's
//! decisions testable on a machine that is neither Ubuntu nor a Hyper-V guest,
//! while `display_kernel` keeps the parts that need one.

use vmlord_agent_protocol::v1::{DisplayRecipeStage, DisplayRecipeStageState, DisplayRecipeStep};

use crate::gpu_recipe::{GuestCapability, payload_declares};

/// The DKMS package a display payload installs.
pub const DKMS_PACKAGE: &str = "vmlord-display";

/// The kernel module that package builds.
pub const MODULE: &str = "vmlord_drm";

/// Every step of the recipe, in the order it is attempted.
///
/// The order is the report's order, and the report is what the host logs, so it
/// is written once here rather than implied by the sequence of calls in
/// `display_kernel`.
pub const STEPS: [DisplayRecipeStep; 13] = [
    DisplayRecipeStep::Distribution,
    DisplayRecipeStep::Payload,
    DisplayRecipeStep::BuildDependencies,
    DisplayRecipeStep::SigningKey,
    DisplayRecipeStep::ModuleSource,
    DisplayRecipeStep::ModuleBuild,
    DisplayRecipeStep::ModuleSignature,
    DisplayRecipeStep::Initramfs,
    DisplayRecipeStep::ModuleLoad,
    DisplayRecipeStep::CompositorIsolation,
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

/// Whether a payload built for one guest can run in this one.
///
/// The architecture, and nothing else. A display payload carries DKMS sources
/// and static musl binaries, so the distribution and release it records are
/// provenance -- where its build was proven -- rather than who it may serve.
/// The host stopped keying its catalogue on them in task #169, and this is the
/// guest's half of the same claim: a stricter rule here would only move the
/// refusal from the host's selection to the mount, which is exactly what a
/// guest whose release nobody built for used to hit.
///
/// Never the kernel either: DKMS builds against the headers of the running
/// kernel, and the kernel a payload records is what it was proven on.
#[must_use]
pub fn serves(payload: &PayloadFacts, architecture: &str) -> bool {
    payload.architecture.eq_ignore_ascii_case(architecture)
}

/// Whether this is the guest the payload was built and proven on.
///
/// Never a condition. What it decides is what the `PAYLOAD` stage reports, so
/// that a guest running a payload proven somewhere else says so where a person
/// reading the recipe will see it.
#[must_use]
pub fn was_built_for(
    payload: &PayloadFacts,
    distribution: &str,
    release: &str,
    architecture: &str,
) -> bool {
    serves(payload, architecture)
        && payload.distribution.eq_ignore_ascii_case(distribution)
        && payload.release == release
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

/// The pair `shim-signed` owns, and the one dkms signs with on Ubuntu.
const SHIM_KEY: &str = "/var/lib/shim-signed/mok/MOK.priv";
const SHIM_CERTIFICATE: &str = "/var/lib/shim-signed/mok/MOK.der";

/// The pair dkms makes for itself where no distribution names another.
const DKMS_KEY: &str = "/var/lib/dkms/mok.key";
const DKMS_CERTIFICATE: &str = "/var/lib/dkms/mok.pub";

/// Where the guest's own module-signing MOK lives, and how it comes to exist.
///
/// Not a path VMLord chose and not one it can choose: signing happens inside
/// `dkms build`, so the pair that counts is the pair dkms will read, and every
/// other pair on the guest is a file nothing signs with. VMLord writes no
/// `framework.conf` and ships no key; it answers the same question dkms
/// answers, in the same order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningPair {
    /// The private key half, which never leaves the guest.
    pub key: String,
    /// The certificate half, and the only half that ever leaves.
    pub certificate: String,
    /// What creates the pair when it is not there.
    pub creation: KeyCreation,
}

/// How a guest that has no pair yet gets one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCreation {
    /// `update-secureboot-policy --new-key`, from Ubuntu's `shim-signed`.
    ///
    /// Only where the pair is `shim-signed`'s own: it writes that pair and no
    /// other, and it stages the enrollment request alongside, which is the
    /// half a plain `openssl` cannot do.
    SecureBootPolicy,
    /// `openssl req`, which every guest with openssl can do.
    OpenSsl,
}

/// Which pair dkms will sign with on this guest.
///
/// `framework` is `/etc/dkms/framework.conf` and every
/// `framework.conf.d/*.conf` beside it, concatenated; `distribution` is the
/// `ID` of `/etc/os-release`. The order is dkms's own `prepare_mok`: what the
/// configuration names wins, Ubuntu without one gets `shim-signed`'s pair, and
/// everyone else gets the pair dkms generates under its own tree.
///
/// A guest whose dkms resolves the pair some third way -- Gentoo reads it out
/// of `make.conf` and the kernel's `CONFIG_MODULE_SIG_KEY` -- is one this
/// answers wrongly, and one no profile builds. It costs a mismatch in the
/// report and no display: the module is signed either way, with a key the
/// report then names as not ours.
#[must_use]
pub fn signing_pair(framework: &str, distribution: &str) -> SigningPair {
    let key = framework_variable(framework, "mok_signing_key");
    let certificate = framework_variable(framework, "mok_certificate");

    // dkms asks about the key alone, so a configuration that names only the
    // certificate leaves the distribution's branch untaken -- and this has to
    // be wrong in the same way to be right about the same guest.
    if key.is_none() && distribution == "ubuntu" {
        return SigningPair {
            key: SHIM_KEY.to_owned(),
            certificate: SHIM_CERTIFICATE.to_owned(),
            creation: KeyCreation::SecureBootPolicy,
        };
    }

    SigningPair {
        key: key.unwrap_or_else(|| DKMS_KEY.to_owned()),
        certificate: certificate.unwrap_or_else(|| DKMS_CERTIFICATE.to_owned()),
        creation: KeyCreation::OpenSsl,
    }
}

/// One shell assignment out of a dkms framework configuration.
///
/// Read rather than sourced, which is the difference between this and dkms:
/// it runs the file as shell and this looks for `name=value` on a line of its
/// own, unquoted or in either quote. A value written as an expansion is
/// therefore not read -- and is answered with `None`, which sends the caller
/// to the same default the file was trying to override rather than to a path
/// spelled `$dkms_tree/mok.key`.
fn framework_variable(text: &str, name: &str) -> Option<String> {
    text.lines()
        .rev()
        .filter_map(|line| line.trim().strip_prefix(name)?.strip_prefix('='))
        .map(|value| {
            let value = value.trim();
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|value| value.strip_suffix('\''))
                })
                .unwrap_or(value)
        })
        .find(|value| !value.is_empty() && !value.contains('$'))
        .map(str::to_owned)
}

/// Whether this kernel signs modules at all, from the configuration dkms
/// reads.
///
/// The one line dkms greps for: without `CONFIG_MODULE_SIG_HASH` it prints
/// that the kernel is built without the module signing facility and signs
/// nothing, whatever pair the guest holds.
#[must_use]
pub fn kernel_signs_modules(kernel_config: &str) -> bool {
    kernel_config
        .lines()
        .any(|line| line.starts_with("CONFIG_MODULE_SIG_HASH="))
}

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
/// all -- one generated without asking for one -- and means there is nothing a
/// signature can be matched against.
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
/// one, which is why every certificate this guest generates is generated with
/// `subjectKeyIdentifier=hash` asked for explicitly.
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

/// Why this guest's compositor is kept off the payload's Mesa, or nothing when
/// it belongs on it.
///
/// Two things have to be true before a compositor may draw on the payload's
/// Mesa, and only one of them is about this guest.
///
/// `device_is_usable` is the adapter. A guest without one wants the isolation
/// drop-in more than it used to: the payload's Mesa is built `-Dllvm=disabled`,
/// so the renderer it falls back to with no adapter is softpipe where the
/// distribution's is llvmpipe.
///
/// `gpu_sources` is the GPU payload's `sources.json`, because presenting a frame
/// is the payload's ability and not the device's, and the two are versioned
/// apart -- a build carrying every commit since #180 can ship a payload packed
/// before any of them. A compositor moved onto such a Mesa draws on the GPU and
/// then cannot hand the frame to KMS: `No GPUs found`, a display manager that
/// gives up, and a screen that stays black through every later boot. Nothing the
/// guest can look at tells that Mesa from a patched one, so the payload says so
/// itself, and a payload that says nothing is believed to be the old one.
///
/// The string is the reason, and it exists because the two answers have to be
/// told apart by whoever reads the report: a guest holding its compositor back
/// on an old payload otherwise looks exactly like a guest with no adapter.
pub fn compositor_isolation(device_is_usable: bool, gpu_sources: &str) -> Option<String> {
    if !device_is_usable {
        return Some("this guest has no adapter".to_owned());
    }
    if !payload_declares(gpu_sources, GuestCapability::CompositorScanout) {
        return Some(
            "this guest has an adapter, but its GPU payload does not declare \
             compositor-scanout, so that Mesa cannot hand a finished frame to vmlord_drm"
                .to_owned(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{DisplayRecipeStageState, DisplayRecipeStep};

    use super::{
        DKMS_PACKAGE, FALLBACK_MODE, InstalledVersions, KeyCreation, Report, STEPS,
        SigningKeyState, compositor_isolation, dkms_reports_installed, dkms_versions, has_recipe,
        kernel_signs_modules, modprobe_options, module_is_loaded, needs_build, needs_reload,
        parse_module_parameters, parse_module_signature_key, parse_secure_boot_state,
        parse_subject_key_identifier, read_payload_facts, serves, signature_matches,
        signing_key_state, signing_pair, wanted_mode, was_built_for,
        was_rejected_for_its_signature,
    };

    const DECLARED: &str =
        r#"{"guest_capabilities":["compositor-scanout"],"mesa_policy":"bundled"}"#;
    const SILENT: &str = r#"{"schema_version":2,"mesa_policy":"bundled"}"#;

    #[test]
    fn ubuntu_without_a_framework_file_signs_with_the_pair_shim_signed_owns() {
        let pair = signing_pair("", "ubuntu");

        assert_eq!(pair.key, "/var/lib/shim-signed/mok/MOK.priv");
        assert_eq!(pair.certificate, "/var/lib/shim-signed/mok/MOK.der");
        assert_eq!(pair.creation, KeyCreation::SecureBootPolicy);
    }

    #[test]
    fn every_other_guest_signs_with_the_pair_dkms_makes_for_itself() {
        // The whole point of the stage on Arch: a key written to the Debian
        // path is a key dkms never reads, and a module signed with a key the
        // report does not know about reads as a module signed by a stranger.
        let pair = signing_pair("", "arch");

        assert_eq!(pair.key, "/var/lib/dkms/mok.key");
        assert_eq!(pair.certificate, "/var/lib/dkms/mok.pub");
        assert_eq!(pair.creation, KeyCreation::OpenSsl);
    }

    #[test]
    fn a_framework_file_that_names_a_pair_is_what_wins_on_any_distribution() {
        let framework = "# mok_signing_key=\"/nowhere\"\n\
                         mok_signing_key=\"/etc/keys/module.key\"\n\
                         mok_certificate='/etc/keys/module.der'\n";

        for distribution in ["ubuntu", "arch"] {
            let pair = signing_pair(framework, distribution);

            assert_eq!(pair.key, "/etc/keys/module.key", "{distribution}");
            assert_eq!(pair.certificate, "/etc/keys/module.der", "{distribution}");
            assert_eq!(pair.creation, KeyCreation::OpenSsl, "{distribution}");
        }
    }

    #[test]
    fn the_last_assignment_of_a_name_is_the_one_that_takes_effect() {
        // `framework.conf.d` is read after `framework.conf` and sourced, so a
        // drop-in overriding the file it sits beside is the ordinary case.
        let framework = "mok_signing_key=/var/lib/dkms/mok.key\n\
                         mok_signing_key=/etc/keys/module.key\n";

        assert_eq!(signing_pair(framework, "arch").key, "/etc/keys/module.key");
    }

    #[test]
    fn a_value_this_cannot_read_leaves_the_default_standing() {
        // An expansion is shell, and reading it literally would name a
        // directory spelled with a dollar sign.
        let pair = signing_pair("mok_signing_key=\"$dkms_tree/mok.key\"\n", "arch");

        assert_eq!(pair.key, "/var/lib/dkms/mok.key");
    }

    #[test]
    fn a_kernel_signs_modules_when_its_configuration_names_the_hash() {
        assert!(kernel_signs_modules(
            "CONFIG_MODULE_SIG=y\nCONFIG_MODULE_SIG_HASH=\"sha512\"\n"
        ));
        assert!(!kernel_signs_modules("CONFIG_MODULE_SIG_ALL=y\n"));
        // Commented out is how a configuration says a symbol is unset, and
        // a substring match would read it as set.
        assert!(!kernel_signs_modules(
            "# CONFIG_MODULE_SIG_HASH= is not set\n"
        ));
    }

    #[test]
    fn a_compositor_draws_on_the_payloads_mesa_only_when_both_halves_are_there() {
        assert_eq!(compositor_isolation(true, DECLARED), None);
    }

    #[test]
    fn a_payload_that_cannot_present_keeps_the_compositor_on_the_guests_own_mesa() {
        // The bug this exists for: an adapter opens, so the old rule moved the
        // compositor onto a Mesa that draws on the GPU and then cannot hand the
        // frame to KMS. Black screen, permanently. An undeclared capability is
        // an old payload, and an old payload keeps the drop-in.
        for sources in [SILENT, r#"{"guest_capabilities":[]}"#, "not json", ""] {
            let held_back = compositor_isolation(true, sources);
            assert!(held_back.is_some(), "{sources}");
            assert!(
                held_back.unwrap().contains("compositor-scanout"),
                "the reason must name what was missing, or a guest with an old \
                 payload reads as a guest with no adapter: {sources}"
            );
        }
    }

    #[test]
    fn a_guest_with_no_adapter_is_isolated_whatever_its_payload_declares() {
        // The declaration is about the payload, not about this guest: that Mesa
        // falls back to softpipe with no adapter under it, where the
        // distribution's falls back to llvmpipe.
        for sources in [DECLARED, SILENT] {
            assert_eq!(
                compositor_isolation(false, sources),
                Some("this guest has no adapter".to_owned()),
                "{sources}"
            );
        }
    }

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
    fn a_payload_applies_by_architecture_and_never_by_kernel() {
        let facts = read_payload_facts(&payload_json("0.1.0", "24.04")).unwrap();

        assert!(serves(&facts, "amd64"));
        assert!(serves(&facts, "AMD64"));
        assert!(
            !serves(&facts, "arm64"),
            "a binary built for amd64 does not run on anything else"
        );
    }

    #[test]
    fn a_release_nobody_built_for_is_provenance_and_not_a_refusal() {
        let facts = read_payload_facts(&payload_json("0.1.0", "24.04")).unwrap();

        // The case that came back from a real guest: the host selected the
        // 24.04 payload for a 26.04 guest, and the mount used to refuse it.
        assert!(serves(&facts, "amd64"));
        assert!(!was_built_for(&facts, "ubuntu", "26.04", "amd64"));
        assert!(!was_built_for(&facts, "arch", "rolling", "amd64"));
        assert!(was_built_for(&facts, "Ubuntu", "24.04", "AMD64"));
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
        assert_eq!(
            parse_secure_boot_state("SecureBoot disabled\n"),
            Some(false)
        );
        assert_eq!(
            parse_secure_boot_state("This system doesn't support Secure Boot\n"),
            None
        );
        assert_eq!(parse_secure_boot_state(""), None);
    }
}
