# Display Module Signing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `vmlord_drm` a signed kernel module, signed by the guest's own MOK inside the DKMS build, and let VMLord say whether the signature is there, hand the certificate to the host, and give a kernel-rejected module a status code of its own.

**Architecture:** The guest's `/var/lib/shim-signed/mok/MOK.{priv,der}` is the signing key on all three supported Ubuntu releases, and dkms already signs with it by default. VMLord adds two recipe steps around the build -- `SigningKey` before it, `ModuleSignature` after it -- which prepare and verify but never sign, so that `AUTOINSTALL=yes` rebuilds after an unattended kernel upgrade stay signed with VMLord absent. Neither step can degrade the display, because Secure Boot is off and an unsigned module loads.

**Tech Stack:** Rust, prost/protobuf over the agent channel, `dkms`, `openssl`, `modinfo`, `mokutil`, `update-secureboot-policy` from `shim-signed`.

**Spec:** `docs/superpowers/specs/2026-08-31-display-module-signing-design.md`

## Global Constraints

- The signing key is always `/var/lib/shim-signed/mok/MOK.priv` and `/var/lib/shim-signed/mok/MOK.der`. VMLord writes no `/etc/dkms/framework.conf` and no `framework.conf.d` file, and never ships or holds a private key.
- The private key never leaves the guest. Only the DER certificate and its two identities do.
- `SigningKey` and `ModuleSignature` never fail the recipe and never produce a `DisplayFailure`. Their stage functions return `()`, not `Result`.
- The DKMS package is `vmlord-display` and the module is `vmlord_drm` (`display_recipe::DKMS_PACKAGE`, `display_recipe::MODULE`). Never spell either literally in new code.
- `crates/agent` does not depend on `vmlord-core` and must not start to. Where the guest and the host need the same knowledge, it is written out twice with a comment naming the other copy -- the established pattern at `crates/agent/src/display_recipe.rs:257` and `crates/agent/src/gpu_targets.rs:45`.
- Every external program runs through `command::run(program, arguments, environment, budget)`, never `std::process::Command` directly.
- New `DisplayStatusCode` variants are stable: the `serde(rename)` string, the `as_str` string and the variant name never change once merged.
- Run `cargo test` from the repository root. Do not prefix commands with `timeout`.

---

### Task 1: The two new steps and the certificate on the wire

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto:514-580`
- Test: `crates/agent-protocol/tests/recipe.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_agent_protocol::v1::DisplayRecipeStep::{SigningKey, ModuleSignature}`; `vmlord_agent_protocol::v1::DisplaySigningCertificate { certificate: Vec<u8>, sha256: String, subject_key_identifier: String }`; `ApplyDisplayRecipeResponse::signing_certificate: Option<DisplaySigningCertificate>`.

- [ ] **Step 1: Write the failing test**

Append to `crates/agent-protocol/tests/recipe.rs`:

```rust
#[test]
fn the_recipe_has_a_step_for_the_signing_key_and_one_for_the_signature() {
    use vmlord_agent_protocol::v1::DisplayRecipeStep;

    assert_eq!(i32::from(DisplayRecipeStep::SigningKey), 11);
    assert_eq!(i32::from(DisplayRecipeStep::ModuleSignature), 12);
}

#[test]
fn a_recipe_answer_can_carry_the_certificate_the_guest_signs_with() {
    use vmlord_agent_protocol::v1::{ApplyDisplayRecipeResponse, DisplaySigningCertificate};

    let answer = ApplyDisplayRecipeResponse {
        stages: Vec::new(),
        versions: None,
        signing_certificate: Some(DisplaySigningCertificate {
            certificate: vec![0x30, 0x82],
            sha256: "ab".repeat(32),
            subject_key_identifier: "0a1b2c".to_owned(),
        }),
    };

    let certificate = answer.signing_certificate.expect("the field exists");
    assert_eq!(certificate.certificate, vec![0x30, 0x82]);
    assert_eq!(certificate.subject_key_identifier, "0a1b2c");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-agent-protocol --test recipe`
Expected: FAIL — `no variant named SigningKey`, `struct has no field named signing_certificate`.

- [ ] **Step 3: Extend the proto**

In `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`, add to `enum DisplayRecipeStep`, after `DISPLAY_RECIPE_STEP_INITRAMFS = 10;`:

```proto
  // Ensuring the guest has a MOK to sign the module with. Never signs: the
  // signing itself happens inside `dkms build`, which is what carries it
  // through an unattended kernel upgrade with no host connected.
  DISPLAY_RECIPE_STEP_SIGNING_KEY = 11;
  // Confirming the built module carries a signature by that certificate.
  DISPLAY_RECIPE_STEP_MODULE_SIGNATURE = 12;
```

Add the message beside `DisplayPayloadVersions`:

```proto
// The certificate the guest's modules are signed with, so the host can hand
// it to whoever performs the MOK enrollment.
//
// The private half is never sent, never asked for and never logged. Both
// identities travel because two readers need different ones: `sha256` is what
// a person compares against the file on the host, and
// `subject_key_identifier` is what `sign-file` writes into the module and
// what the signature is matched on.
message DisplaySigningCertificate {
  bytes certificate = 1;
  string sha256 = 2;
  string subject_key_identifier = 3;
}
```

Add to `ApplyDisplayRecipeResponse`, keeping fields 1 and 2 as they are:

```proto
  // Absent when this guest has no signing key -- a kernel that cannot sign
  // modules, or a `SigningKey` step that failed.
  DisplaySigningCertificate signing_certificate = 3;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent-protocol`
Expected: PASS, including the existing `descriptor.rs` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agent-protocol/proto/vmlord/agent/v1/agent.proto crates/agent-protocol/tests/recipe.rs
git commit -m "TASK-126: Give the recipe a place to say what it signs with"
```

---

### Task 2: Reading a signing key pair and a certificate's identities

**Files:**
- Modify: `crates/agent/src/display_recipe.rs` (add below `parse_module_version`, around line 250)
- Test: `crates/agent/src/display_recipe.rs` (the `mod tests` at the bottom)

**Interfaces:**
- Consumes: nothing.
- Produces: `display_recipe::SigningKeyState` (`Complete`, `HalfPresent`, `Absent`), `display_recipe::signing_key_state(private_key_exists: bool, certificate_exists: bool) -> SigningKeyState`, `display_recipe::parse_subject_key_identifier(text: &str) -> Option<String>`, `display_recipe::certificate_sha256(der: &[u8]) -> String`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/agent/src/display_recipe.rs`:

```rust
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
fn a_certificates_fingerprint_is_the_sha256_of_its_der() {
    // The SHA-256 of the empty input, which is what an unreadable certificate
    // would hash to and why the caller checks the bytes before hashing them.
    assert_eq!(
        certificate_sha256(&[]),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(certificate_sha256(b"vmlord").len(), 64);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: FAIL — `cannot find function signing_key_state in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `crates/agent/src/display_recipe.rs`, below `parse_module_version`:

```rust
/// Where the guest's own module-signing MOK lives.
///
/// Not a path VMLord chose: it is what `dkms` on 22.04, 24.04 and 26.04 all
/// sign with by default, which is why VMLord configures no signing of its own.
pub const SIGNING_KEY: &str = "/var/lib/shim-signed/mok/MOK.priv";
pub const SIGNING_CERTIFICATE: &str = "/var/lib/shim-signed/mok/MOK.der";

/// What the guest has of a signing pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigningKeyState {
    /// Both halves are there and are the pair the modules are signed with.
    Complete,
    /// One half is there. A certificate cannot be derived from a private key,
    /// so this is a broken pair rather than half a good one: both halves are
    /// replaced, and the enrollment has to be repeated.
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
/// Lower-case and without separators, which is the form
/// [`signature_matches`] compares in. `None` is a certificate carrying no
/// subject key identifier at all -- one generated without
/// `/usr/lib/shim/mok/openssl.cnf` -- and it means there is nothing to match a
/// signature against.
#[must_use]
pub fn parse_subject_key_identifier(text: &str) -> Option<String> {
    let mut lines = text
        .lines()
        .skip_while(|line| !line.contains("Subject Key Identifier"));
    lines.next()?;
    let identifier: String = lines
        .next()?
        .chars()
        .filter(char::is_ascii_hexdigit)
        .flat_map(char::to_lowercase)
        .collect();
    (!identifier.is_empty()).then_some(identifier)
}

/// The SHA-256 of a certificate's DER, hex-encoded and lower-case.
///
/// The identity a person compares against the copy on the host. The one a
/// signature is matched on is the subject key identifier, not this.
#[must_use]
pub fn certificate_sha256(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(der);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        })
}
```

If `sha2` is not already a dependency of `crates/agent`, add it to `crates/agent/Cargo.toml` matching the version the workspace already pins (`display_kernel.rs` has a `sha256_hex`; use the same crate and version it uses).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/display_recipe.rs crates/agent/Cargo.toml
git commit -m "TASK-126: Read a signing pair and what identifies its certificate"
```

---

### Task 3: Telling a signed module from an unsigned one

**Files:**
- Modify: `crates/agent/src/display_recipe.rs`
- Test: `crates/agent/src/display_recipe.rs` (`mod tests`)

**Interfaces:**
- Consumes: `parse_subject_key_identifier` from Task 2.
- Produces: `display_recipe::parse_module_signature_key(modinfo: &str) -> Option<String>`, `display_recipe::signature_matches(modinfo: &str, subject_key_identifier: &str) -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
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
fn a_module_signed_by_some_other_key_is_not_a_module_we_can_vouch_for() {
    let modinfo = "sig_key:        FF:EE:DD\n";

    assert!(!signature_matches(modinfo, "0a1b2c3d4e5f"));
    assert!(
        !signature_matches(modinfo, ""),
        "an empty identifier matches nothing, or every module would pass"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: FAIL — `cannot find function parse_module_signature_key in this scope`.

- [ ] **Step 3: Write the implementation**

```rust
/// The key `modinfo` says signed this module, in the form
/// [`parse_subject_key_identifier`] returns.
///
/// `sign-file` writes the certificate's subject key identifier when it has
/// one, which is why the certificate is generated with
/// `/usr/lib/shim/mok/openssl.cnf` and its `subjectKeyIdentifier = hash`.
#[must_use]
pub fn parse_module_signature_key(modinfo: &str) -> Option<String> {
    let value = modinfo
        .lines()
        .find_map(|line| line.strip_prefix("sig_key:"))?;
    let key: String = value
        .chars()
        .filter(char::is_ascii_hexdigit)
        .flat_map(char::to_lowercase)
        .collect();
    (!key.is_empty()).then_some(key)
}

/// Whether this module is signed by the certificate the guest holds.
///
/// An empty identifier never matches: a certificate with no subject key
/// identifier gives nothing to compare, and treating that as agreement would
/// report every module as signed by it.
#[must_use]
pub fn signature_matches(modinfo: &str, subject_key_identifier: &str) -> bool {
    !subject_key_identifier.is_empty()
        && parse_module_signature_key(modinfo).as_deref() == Some(subject_key_identifier)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/display_recipe.rs
git commit -m "TASK-126: Tell a module we signed from one we did not"
```

---

### Task 4: Recognising a module the kernel rejected

**Files:**
- Modify: `crates/agent/src/display_recipe.rs`
- Test: `crates/agent/src/display_recipe.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `display_recipe::SIGNATURE_REJECTION_PHRASES: [&str; 2]`, `display_recipe::was_rejected_for_its_signature(output: &str) -> bool`, `display_recipe::parse_secure_boot_state(mokutil: &str) -> Option<bool>`.

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: FAIL — `cannot find function was_rejected_for_its_signature in this scope`.

- [ ] **Step 3: Write the implementation**

```rust
/// What the kernel says when it refuses a module over its signature.
///
/// `EKEYREJECTED` is a module signed by a key that is not trusted;
/// `ENOKEY` is a module with no signature at all under a kernel that requires
/// one. Both mean the same thing to a user: the certificate is not enrolled.
///
/// Written out a second time in `vmlord_core::display` for the host, which
/// reads these same phrases back out of the stage message. Two copies rather
/// than a shared crate because `vmlord-agent` deliberately does not depend on
/// `vmlord-core`; change one and change the other.
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
/// `None` is a firmware that has no Secure Boot to report on, which is every
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/display_recipe.rs
git commit -m "TASK-126: Recognise the kernel refusing a module's signature"
```

---

### Task 5: A status code for a rejected module

**Files:**
- Modify: `crates/core/src/display.rs:333-420` (the `DisplayStatusCode` enum, its `as_str`, its `is_retryable`)
- Test: `crates/core/src/display.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_core::display::DisplayStatusCode::PayloadModuleSignatureRejected`, `vmlord_core::display::was_rejected_for_its_signature(text: &str) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/core/src/display.rs`:

```rust
#[test]
fn a_module_the_kernel_refused_has_a_code_of_its_own() {
    let code = DisplayStatusCode::PayloadModuleSignatureRejected;

    assert_eq!(code.as_str(), "display-payload-module-signature-rejected");
    assert_eq!(
        serde_json::to_string(&code).unwrap(),
        "\"display-payload-module-signature-rejected\""
    );
    assert_ne!(code, DisplayStatusCode::PayloadModuleNotLoaded);
}

#[test]
fn no_number_of_retries_enrolls_a_certificate() {
    assert!(!DisplayStatusCode::PayloadModuleSignatureRejected.is_retryable());
}

#[test]
fn the_host_reads_the_same_refusal_the_guest_wrote_down() {
    assert!(was_rejected_for_its_signature(
        "the guest's display recipe stopped at ModuleLoad: modprobe vmlord_drm \
         exited with 1: modprobe: ERROR: could not insert 'vmlord_drm': \
         Key was rejected by service"
    ));
    assert!(was_rejected_for_its_signature("Required key not available"));
    assert!(!was_rejected_for_its_signature(
        "modprobe: ERROR: could not insert 'vmlord_drm': Invalid argument"
    ));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-core display`
Expected: FAIL — `no variant named PayloadModuleSignatureRejected`.

- [ ] **Step 3: Write the implementation**

Add the variant to `DisplayStatusCode`, after `PayloadModuleNotLoaded`:

```rust
    /// The module built and the kernel refused its signature.
    ///
    /// Distinct from [`Self::PayloadModuleNotLoaded`] because the fix is
    /// different and lives outside VMLord: the guest's certificate has to be
    /// enrolled as a MOK. A build that broke and an enrollment that was never
    /// done read identically otherwise.
    #[serde(rename = "display-payload-module-signature-rejected")]
    PayloadModuleSignatureRejected,
```

Add its arm to `as_str`:

```rust
            Self::PayloadModuleSignatureRejected => "display-payload-module-signature-rejected",
```

In `is_retryable`, place it with the codes that are not retryable — retrying `modprobe` cannot enroll a certificate. Read the existing body and add the variant to the arm that returns `false`.

Add beside the enum:

```rust
/// Whether a guest's failure text is the kernel refusing a module's signature.
///
/// Written out a second time from `vmlord_agent::display_recipe`, which
/// produces this text: `vmlord-agent` deliberately does not depend on
/// `vmlord-core`, so the two phrases exist in both crates. Change one and
/// change the other.
#[must_use]
pub fn was_rejected_for_its_signature(text: &str) -> bool {
    ["Key was rejected by service", "Required key not available"]
        .iter()
        .any(|phrase| text.contains(phrase))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-core display`
Expected: PASS. If `cargo build` reports a non-exhaustive `match` anywhere over `DisplayStatusCode`, add the new variant to it alongside `PayloadModuleNotLoaded`.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/display.rs
git commit -m "TASK-126: Give a refused signature a status of its own"
```

---

### Task 6: The two guest stages

**Files:**
- Modify: `crates/agent/src/display_kernel.rs` (`run_stages` at 200-257, `run_update` at 260, `load_stage` at 668, `reload_module` at 749, `reload_module_for_update` at 763, plus the new stage functions)
- Modify: `crates/agent/src/main.rs:227-233`
- Modify: `crates/agent/src/session.rs:53`
- Test: `crates/agent/src/display_kernel.rs` (`mod tests`)

**Interfaces:**
- Consumes: everything from Tasks 2-4, and `DisplayRecipeStep::{SigningKey, ModuleSignature}` and `DisplaySigningCertificate` from Task 1.
- Produces: `display_kernel::apply(&AtomicBool, Option<(u32, u32)>) -> (Vec<DisplayRecipeStage>, DisplayPayloadVersions, Option<DisplaySigningCertificate>)` — a third element, which `main.rs` puts on the response.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/agent/src/display_kernel.rs`:

```rust
#[test]
fn the_recipe_prepares_a_key_before_it_builds_and_checks_the_signature_after() {
    use vmlord_agent_protocol::v1::DisplayRecipeStep as Step;

    let steps = crate::display_recipe::STEPS;
    let position = |wanted: Step| {
        steps
            .iter()
            .position(|step| *step == wanted)
            .expect("every step is in STEPS")
    };

    assert_eq!(steps.len(), 12);
    assert!(position(Step::BuildDependencies) < position(Step::SigningKey));
    assert!(position(Step::SigningKey) < position(Step::ModuleSource));
    assert!(position(Step::ModuleBuild) < position(Step::ModuleSignature));
    assert!(position(Step::ModuleSignature) < position(Step::Initramfs));
}

#[test]
fn a_modprobe_refusal_over_a_signature_says_what_is_missing() {
    let message = load_failure_message(
        "modprobe vmlord_drm exited with 1: modprobe: ERROR: could not insert \
         'vmlord_drm': Key was rejected by service",
        Some(true),
        Some("0a1b2c"),
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

    assert_eq!(load_failure_message(reason, Some(false), Some("0a1b2c")), reason);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent display_kernel`
Expected: FAIL — `STEPS` has 10 entries and `load_failure_message` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/agent/src/display_recipe.rs`, extend `STEPS` to twelve in the order the test asserts:

```rust
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
```

In `crates/agent/src/display_kernel.rs`, add the stage functions and the message helper:

```rust
/// Makes sure the guest has a MOK to sign with, and says which one it is.
///
/// Never signs and never fails the recipe: signing happens inside
/// `dkms build`, and with Secure Boot off an unsigned module loads. A guest
/// that cannot produce a key still gets its desktop.
fn signing_key_stage(report: &mut Report, kernel_release: &str) -> Option<DisplaySigningCertificate> {
    if !kernel_can_sign_modules(kernel_release) {
        report.skipped(
            DisplayRecipeStep::SigningKey,
            format!("kernel {kernel_release} is built without module signing"),
        );
        return None;
    }

    let state = signing_key_state(
        Path::new(SIGNING_KEY).exists(),
        Path::new(SIGNING_CERTIFICATE).exists(),
    );
    let repeat_enrollment = state == SigningKeyState::HalfPresent;
    if state != SigningKeyState::Complete {
        // A certificate cannot be derived from a private key, so half a pair
        // is replaced whole rather than completed.
        let _ = fs::remove_file(SIGNING_KEY);
        let _ = fs::remove_file(SIGNING_CERTIFICATE);
        if let Err(reason) = create_signing_key() {
            report.failed(DisplayRecipeStep::SigningKey, reason);
            return None;
        }
    }

    let der = match fs::read(SIGNING_CERTIFICATE) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => {
            report.failed(
                DisplayRecipeStep::SigningKey,
                format!("{SIGNING_CERTIFICATE} is empty"),
            );
            return None;
        }
        Err(error) => {
            report.failed(
                DisplayRecipeStep::SigningKey,
                format!("{SIGNING_CERTIFICATE} could not be read: {error}"),
            );
            return None;
        }
    };
    restrict_to_root(Path::new(SIGNING_KEY));

    let printed = command::run(
        "openssl",
        &["x509", "-inform", "DER", "-in", SIGNING_CERTIFICATE, "-noout", "-text"],
        &[],
        SHORT_BUDGET,
    );
    let Some(identifier) = parse_subject_key_identifier(&printed.output) else {
        report.failed(
            DisplayRecipeStep::SigningKey,
            format!("{SIGNING_CERTIFICATE} carries no subject key identifier"),
        );
        return None;
    };
    let sha256 = certificate_sha256(&der);

    if repeat_enrollment || state == SigningKeyState::Absent {
        // Every version DKMS holds was signed by the key that is now gone, so
        // a rollback would land on a module Secure Boot refuses. Re-signing
        // them is what makes the rollback path survive a new key.
        resign_installed_versions(report, kernel_release);
    }
    report.ok(
        DisplayRecipeStep::SigningKey,
        format!(
            "modules are signed with {SIGNING_CERTIFICATE} (sha256 {sha256}, key id {identifier}){}",
            if repeat_enrollment {
                ", replaced because it was half a pair -- its enrollment has to be repeated"
            } else {
                ""
            }
        ),
    );

    Some(DisplaySigningCertificate {
        certificate: der,
        sha256,
        subject_key_identifier: identifier,
    })
}

/// Whether this kernel signs modules at all. `dkms` skips signing without it.
fn kernel_can_sign_modules(kernel_release: &str) -> bool {
    read(Path::new(&format!("/boot/config-{kernel_release}"))).contains("CONFIG_MODULE_SIG_HASH=")
}

fn create_signing_key() -> Result<(), String> {
    let policy = command::run(
        "update-secureboot-policy",
        &["--new-key"],
        &[
            ("SHIM_NOTRIGGER", "y"),
            ("DEBIAN_FRONTEND", "noninteractive"),
        ],
        SHORT_BUDGET,
    );
    if policy.succeeded() && Path::new(SIGNING_CERTIFICATE).exists() {
        return Ok(());
    }

    // No `shim-signed` on this guest. The configuration file is the one thing
    // that must not be substituted: without its `subjectKeyIdentifier = hash`
    // the certificate carries nothing a signature can be matched on.
    let openssl = command::run(
        "openssl",
        &[
            "req", "-config", "/usr/lib/shim/mok/openssl.cnf", "-new", "-x509", "-newkey",
            "rsa:2048", "-nodes", "-days", "36500", "-outform", "DER", "-keyout", SIGNING_KEY,
            "-out", SIGNING_CERTIFICATE, "-subj", "/CN=VMLord display module signing key/",
        ],
        &[],
        SHORT_BUDGET,
    );
    if !openssl.succeeded() {
        return Err(failure("openssl req", &openssl));
    }
    Ok(())
}

fn restrict_to_root(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

/// Rebuilds and reinstalls every version DKMS holds, so all of them carry the
/// key that exists now.
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
/// regression. What this buys is that the day Secure Boot is on, the report
/// already says whether this guest was producing signed modules.
fn module_signature_stage(
    report: &mut Report,
    certificate: Option<&DisplaySigningCertificate>,
    kernel_release: &str,
) {
    let Some(certificate) = certificate else {
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
                "{MODULE} is signed with key id {other}, not the guest's own {}",
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
/// to act: whether Secure Boot is on, and which certificate has to be
/// enrolled.
fn load_failure_message(
    reason: &str,
    secure_boot: Option<bool>,
    subject_key_identifier: Option<&str>,
) -> String {
    if !was_rejected_for_its_signature(reason) {
        return reason.to_owned();
    }
    let state = match secure_boot {
        Some(true) => "Secure Boot is on",
        Some(false) => "Secure Boot is off",
        None => "Secure Boot state is unknown",
    };
    let certificate = match subject_key_identifier {
        Some(identifier) => {
            format!("enroll {SIGNING_CERTIFICATE} (key id {identifier}) as a MOK")
        }
        None => "this guest has no certificate to enroll".to_owned(),
    };
    format!("{reason} -- {state} and {certificate}")
}

fn secure_boot_state() -> Option<bool> {
    parse_secure_boot_state(&command::run("mokutil", &["--sb-state"], &[], SHORT_BUDGET).output)
}
```

Wire the stages into `run_stages`. Replace the `if built { ... } else { ... }` block and the `load_stage` call so that they read:

```rust
    let certificate = signing_key_stage(report, &guest.kernel_release);
    halted(stopping)?;

    let installed = installed_versions();
    let built = needs_build(&installed, &payload.version, device_is_present());
    if built {
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
    module_signature_stage(report, certificate.as_ref(), &guest.kernel_release);
    halted(stopping)?;
```

`signing_key_stage` runs before `dependencies_stage` in the source order of `STEPS`, but `dependencies_stage` is what installs `dkms`; call `signing_key_stage` after the `if built` block's dependencies are in place by moving its call to immediately after `dependencies_stage` inside the `if built` branch, and in the `else` branch call it directly. Keep `STEPS` order as the report's order -- `Report::finish` sorts by `STEPS`, so the call order does not have to match.

Make `run_stages` return the certificate so `apply` can pass it on: change its signature to `-> Result<Option<DisplaySigningCertificate>, String>` and return `Ok(certificate)` at the end, and have `apply` return the triple.

In `load_stage`, `reload_module` and `reload_module_for_update`, wrap each `failure(&format!("modprobe {MODULE}"), &outcome)` in the new message. Each of those three call sites becomes:

```rust
        let reason = load_failure_message(
            &failure(&format!("modprobe {MODULE}"), &outcome),
            secure_boot_state(),
            certificate.map(|certificate| certificate.subject_key_identifier.as_str()),
        );
```

which means `load_stage`, `reload_module` and `reload_module_for_update` each take an extra `certificate: Option<&DisplaySigningCertificate>` parameter, passed down from `run_stages` and `run_update`.

In `run_update`, call `signing_key_stage` and `module_signature_stage` in the same relative positions, and pass the certificate to the load helpers. The update response does not carry the certificate: it is per VM and does not change between a start and an update, and the next start's recipe reports it.

In `crates/agent/src/session.rs:53`, change the handler type to return the response unchanged -- it already returns `ApplyDisplayRecipeResponse`, so only `main.rs` changes:

```rust
            apply_display_recipe: &mut |mode| {
                let (stages, versions, signing_certificate) = display_kernel::apply(&STOPPING, mode);
                ApplyDisplayRecipeResponse {
                    stages,
                    versions: Some(versions),
                    signing_certificate,
                }
            },
```

Add the imports the new code needs at the top of `display_kernel.rs`: `certificate_sha256`, `parse_module_signature_key`, `parse_secure_boot_state`, `parse_subject_key_identifier`, `signature_matches`, `signing_key_state`, `was_rejected_for_its_signature`, `SigningKeyState`, `SIGNING_CERTIFICATE`, `SIGNING_KEY` from `crate::display_recipe`, and `DisplaySigningCertificate` from `vmlord_agent_protocol::v1`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent`
Expected: PASS, including the existing `a_report_names_every_step_even_the_ones_that_never_ran` test, which now covers twelve steps.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/display_kernel.rs crates/agent/src/display_recipe.rs crates/agent/src/main.rs
git commit -m "TASK-126: Prepare a signing key and check what the build signed"
```

---

### Task 7: The host keeps the certificate and reads a refusal

**Files:**
- Modify: `crates/platform/src/layout.rs` (beside `display_payload_staging_directory`, around line 72)
- Modify: `crates/platform/src/agent_session.rs:839-940` (`report_display_recipe`, `code_for`, `GuestDisplayPayloadReport`)
- Modify: `crates/platform/src/agent.rs:232-300` (`AgentConnection::start` and its display sink)
- Modify: `crates/platform/src/repository.rs:825-835` (the one call site)
- Test: `crates/platform/src/agent_session.rs` (`mod tests`), `crates/platform/src/layout.rs` (`mod tests`)

**Interfaces:**
- Consumes: `DisplaySigningCertificate` (Task 1), `DisplayStatusCode::PayloadModuleSignatureRejected` and `vmlord_core::display::was_rejected_for_its_signature` (Task 5), the guest stages (Task 6).
- Produces: `layout::display_mok_certificate_path(vm_directory: &Path) -> PathBuf`; `GuestDisplayPayloadReport::signing_certificate: Option<Vec<u8>>`.

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/agent_session.rs`, `mod tests`:

```rust
#[test]
fn a_signature_the_kernel_refused_is_not_the_same_failure_as_a_module_that_would_not_build() {
    let report = ApplyDisplayRecipeResponse {
        stages: vec![DisplayRecipeStage {
            step: i32::from(DisplayRecipeStep::ModuleLoad),
            state: i32::from(DisplayRecipeStageState::Failed),
            message: "modprobe vmlord_drm exited with 1: modprobe: ERROR: could not \
                      insert 'vmlord_drm': Key was rejected by service -- Secure Boot \
                      is on and enroll /var/lib/shim-signed/mok/MOK.der (key id 0a1b) \
                      as a MOK"
                .to_owned(),
        }],
        versions: None,
        signing_certificate: None,
    };

    let mut seen = None;
    report_display_recipe(&report, "dev", &|report| seen = Some(report));

    let failure = seen.expect("a failed recipe reports").failure.expect("a failure");
    assert_eq!(
        failure.code,
        DisplayStatusCode::PayloadModuleSignatureRejected
    );
}

#[test]
fn a_signature_nobody_checks_yet_does_not_take_the_display_down() {
    for step in [
        DisplayRecipeStep::SigningKey,
        DisplayRecipeStep::ModuleSignature,
    ] {
        let report = ApplyDisplayRecipeResponse {
            stages: vec![
                DisplayRecipeStage {
                    step: i32::from(step),
                    state: i32::from(DisplayRecipeStageState::Failed),
                    message: "vmlord_drm carries no signature".to_owned(),
                },
                stage(
                    DisplayRecipeStep::ServicesStart,
                    DisplayRecipeStageState::Ok,
                ),
            ],
            versions: None,
            signing_certificate: None,
        };

        let mut seen = None;
        report_display_recipe(&report, "dev", &|report| seen = Some(report));

        let seen = seen.expect("a recipe reports");
        assert!(
            seen.failure.is_none(),
            "{step:?} must not degrade a display nothing checks the signature of"
        );
    }
}

#[test]
fn the_certificate_the_guest_signs_with_reaches_the_host() {
    let report = ApplyDisplayRecipeResponse {
        stages: vec![stage(
            DisplayRecipeStep::ServicesStart,
            DisplayRecipeStageState::Ok,
        )],
        versions: None,
        signing_certificate: Some(DisplaySigningCertificate {
            certificate: vec![0x30, 0x82, 0x01],
            sha256: "ab".repeat(32),
            subject_key_identifier: "0a1b".to_owned(),
        }),
    };

    let mut seen = None;
    report_display_recipe(&report, "dev", &|report| seen = Some(report));

    assert_eq!(
        seen.expect("a recipe reports").signing_certificate,
        Some(vec![0x30, 0x82, 0x01])
    );
}
```

In `crates/platform/src/layout.rs`, `mod tests`:

```rust
#[test]
fn a_vms_signing_certificate_sits_with_its_display_and_not_in_the_payload_it_mounts() {
    let vm = Path::new("C:\\vms\\dev");

    assert_eq!(
        display_mok_certificate_path(vm),
        vm.join("display").join("mok.der")
    );
    assert!(
        !display_mok_certificate_path(vm).starts_with(display_payload_staging_directory(vm)),
        "a payload cleanup must not take the certificate with it"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform display`
Expected: FAIL — `struct has no field named signing_certificate`, `cannot find function display_mok_certificate_path`.

- [ ] **Step 3: Write the implementation**

In `crates/platform/src/layout.rs`:

```rust
/// Where a VM's guest MOK certificate is kept for whoever enrolls it.
///
/// Under `display/` and not under `display-payload/`: the payload's staging
/// directory is emptied when a generation is replaced, and the certificate
/// outlives every payload version the VM ever runs.
pub(crate) fn display_mok_certificate_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("display").join("mok.der")
}
```

In `crates/platform/src/agent_session.rs`, add the field to `GuestDisplayPayloadReport`:

```rust
    /// The DER of the certificate the guest signs its modules with, when it
    /// has one. The private half is never asked for.
    pub(crate) signing_certificate: Option<Vec<u8>>,
```

In `report_display_recipe`, skip the two signing steps when choosing the failure:

```rust
    let failure = report
        .stages
        .iter()
        .find(|stage| {
            stage.state() == DisplayRecipeStageState::Failed
                && !matches!(
                    stage.step(),
                    // Neither can degrade a display: Secure Boot is off, an
                    // unsigned module loads, and a desktop that works is not
                    // failed over a signature nothing checks yet.
                    DisplayRecipeStep::SigningKey | DisplayRecipeStep::ModuleSignature
                )
        })
        .map(|broken| {
            let message = format!(
                "the guest's display recipe stopped at {:?}: {}",
                broken.step(),
                broken.message
            );
            DisplayFailure::new(
                DisplayStage::Payload,
                code_for(broken.step(), &message),
                message,
            )
        });
```

and pass the certificate to the sink:

```rust
        signing_certificate: report
            .signing_certificate
            .as_ref()
            .map(|certificate| certificate.certificate.clone()),
```

Extend `code_for`:

```rust
fn code_for(step: vmlord_agent_protocol::v1::DisplayRecipeStep, message: &str) -> DisplayStatusCode {
    use vmlord_agent_protocol::v1::DisplayRecipeStep as Step;

    match step {
        Step::BuildDependencies => DisplayStatusCode::PayloadDependenciesFailed,
        Step::ModuleBuild | Step::ModuleSource => DisplayStatusCode::PayloadBuildFailed,
        // A module the kernel refused over its signature is not a module that
        // would not load: the fix is an enrollment, and no retry performs one.
        Step::Initramfs | Step::ModuleLoad
            if vmlord_core::display::was_rejected_for_its_signature(message) =>
        {
            DisplayStatusCode::PayloadModuleSignatureRejected
        }
        Step::Initramfs | Step::ModuleLoad => DisplayStatusCode::PayloadModuleNotLoaded,
        Step::Device => DisplayStatusCode::PayloadNoDevice,
        Step::Services | Step::ServicesStart => DisplayStatusCode::GuestServicesFailed,
        Step::SigningKey | Step::ModuleSignature => DisplayStatusCode::PayloadInvalid,
        Step::Distribution | Step::Payload | Step::Unspecified => DisplayStatusCode::PayloadInvalid,
    }
}
```

The `SigningKey | ModuleSignature` arm is unreachable through `report_display_recipe`, which filters those steps out, and exists so the match stays exhaustive without a catch-all that would silently swallow a step added later.

In `crates/platform/src/agent.rs`, give `AgentConnection::start` a `mok_certificate_path: PathBuf` parameter, move it into the worker closure, and write the file in the display sink:

```rust
                        &|report| {
                            if let Some(certificate) = &report.signing_certificate {
                                write_mok_certificate(&mok_certificate_path, certificate, &vm_name);
                            }
                            if let Some(guest) = report.guest {
                                display_facts.record_guest_display(vm_id, guest);
                            }
                            display_facts.record_guest_payload(
                                vm_id,
                                report.installed,
                                report.previous,
                                report.loaded,
                                report.failure,
                            );
                        },
```

with, beside `read_secret`:

```rust
/// Keeps the guest's signing certificate where a person can find it.
///
/// Overwritten on every run rather than written once: it is a copy of what the
/// guest holds now, and a stale copy would send somebody to enroll a
/// certificate the guest has replaced.
fn write_mok_certificate(path: &Path, certificate: &[u8], vm_name: &str) {
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, certificate));
    if let Err(error) = written {
        // Never fatal: a certificate nobody can enroll yet is not a reason to
        // end a session that is otherwise bringing a desktop up.
        tracing::warn!(
            "the signing certificate of VM \"{vm_name}\" could not be written to {}: {error}",
            path.display()
        );
    }
}
```

In `crates/platform/src/repository.rs`, pass the path at the one call site:

```rust
            layout::display_mok_certificate_path(&vm_directory),
```

Fix the three existing `ApplyDisplayRecipeResponse { .. }` literals in `agent_session.rs` tests (lines 1422, 2097, 2307) by adding `signing_certificate: None`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/layout.rs crates/platform/src/agent_session.rs crates/platform/src/agent.rs crates/platform/src/repository.rs
git commit -m "TASK-126: Keep the guest's certificate and name a refused signature"
```

---

### Task 8: What the documentation now says

**Files:**
- Modify: `docs/display-troubleshooting.md:39` and its "Secure Boot and networking" section at line 104
- Modify: `docs/display-compatibility.md:49-50`
- Modify: `docs/display-drm-backend.md:107-109`

**Interfaces:**
- Consumes: the status code from Task 5 and the certificate path from Task 7.
- Produces: nothing code depends on.

- [ ] **Step 1: Add the code to the troubleshooting table**

In `docs/display-troubleshooting.md`, beside the `display-payload-module-not-loaded` row:

```markdown
| `display-payload-module-signature-rejected` | The module built and the kernel refused its signature. | Enroll the VM's certificate: it is written to `display/mok.der` beside the VM's state, and `mokutil --import` stages it for MokManager on the next boot. |
```

- [ ] **Step 2: Rewrite the Secure Boot section**

Replace the section at line 104 with:

```markdown
## Secure Boot

The module is signed. Each VM's guest generates its own MOK at
`/var/lib/shim-signed/mok/`, dkms signs every build with it -- including the
rebuilds an unattended kernel upgrade triggers, with VMLord closed -- and
VMLord copies the certificate to `display/mok.der` beside the VM's state.

What is not done is the enrollment. `MokList` is written by MokManager alone,
from the firmware console, and VMLord's VMs have none: they are created
straight through HCS and do not appear in Hyper-V Manager. Secure Boot must
therefore stay off for a VMLord VM. With it on and the certificate not
enrolled, the display is `Degraded` with
`display-payload-module-signature-rejected`, and the VM itself keeps running.
```

Move whatever the old section said about networking under its own heading, unchanged.

- [ ] **Step 3: Correct the compatibility statement**

In `docs/display-compatibility.md`, replace the two lines at 49-50 with:

```markdown
- Secure Boot must be disabled. The guest DRM module is signed with the guest's
  own MOK, but nothing can enroll that certificate: MokManager needs a firmware
  console, and VMLord's VMs have none.
```

- [ ] **Step 4: Point the backend note at the answer**

In `docs/display-drm-backend.md`, replace the sentence beginning "If Secure Boot is ever turned on for VMLord VMs" with:

```markdown
  The module is signed with the guest's own MOK -- see
  [the signing design](superpowers/specs/2026-08-31-display-module-signing-design.md)
  -- so what an enabled Secure Boot still needs is the enrollment of that
  certificate, which no VMLord VM can perform today.
```

- [ ] **Step 5: Commit**

```bash
git add docs/display-troubleshooting.md docs/display-compatibility.md docs/display-drm-backend.md
git commit -m "TASK-126: Say that the module is signed and the enrollment is not"
```

---

## Final verification

- [ ] Run `cargo test` from the repository root. Expected: PASS, whole workspace.
- [ ] Run `cargo clippy --all-targets -- -D warnings`. Expected: no warnings.
- [ ] Confirm by hand that no new file writes `/etc/dkms/framework.conf` or `framework.conf.d`: `grep -rn "framework.conf" crates/` returns nothing.
- [ ] Confirm the private key is never sent: `grep -rn "MOK.priv" crates/` appears only in `display_recipe.rs` as `SIGNING_KEY` and in `display_kernel.rs` where the file is created and its permissions repaired -- never in a proto message, a log line or a host crate.
