# Installer, Updates, and Releases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish VMLord from GitHub as an Inno Setup package with current-user/all-users installation, first-run configuration, user-confirmed verified updates, and a Forgejo mirror.

**Architecture:** Pure release/profile rules live in `core`; network transfer stays in `image`; Windows installer launch stays in `platform`; `app` owns the update state machine and exposes state/actions to `ui`. `xtask`, Inno Setup, and pinned GitHub Actions turn an exact version tag into a draft release and mirror accepted refs to Forgejo.

**Tech Stack:** Rust 2024, serde/serde_json, semver, sha2, ureq, Windows API, egui/eframe, Inno Setup 6.6, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-08-28-installer-releases-design.md`

## Global Constraints

- All application code is Rust; Inno Setup remains declarative packaging only.
- New UI text uses `t!` and exists in both `crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml`.
- UI calls only application-layer APIs; Windows APIs remain isolated in `crates/platform`.
- User-facing failures use `vmlord_core::diagnostic!`; ordinary details use `tracing`.
- No Authenticode certificate exists; SHA-256 is an integrity check, not publisher authentication.
- Installation and updates never delete `%LOCALAPPDATA%\VMLord`.
- No dependency may force the Linux agent to link a system C library.
- Every commit subject uses `TASK-15: comment`.

---

### Task 1: Seed Bundled Distribution Profiles for Every User

**Files:**
- Modify: `crates/core/src/distro.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/vmlord/src/main.rs`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Produces: `sync_bundled_profiles(bundle: &Path, store: &SettingsStore) -> Result<(), DistroCatalogError>`.
- Produces: `SettingsLoad { settings: AppSettings, created: bool }` from `SettingsStore::load_or_create_with_status()`.
- Consumes: installed profile directory derived from `current_exe().parent().join("distros")`.

- [ ] **Step 1: Write failing profile synchronization tests**

Add tests in `crates/core/src/distro.rs` proving a missing bundled JSON is copied, changed bundled content replaces the recorded bundled copy, and an unrecorded user JSON with the same stem is preserved. The ownership file is `%LOCALAPPDATA%\VMLord\distros\.bundled-profiles.json` and contains hashes keyed by filename.

```rust
#[test]
fn a_user_profile_is_never_replaced_by_a_bundled_profile() {
    let fixture = ProfileFixture::new();
    fixture.write_bundle("ubuntu.json", "new bundle");
    fixture.write_user("ubuntu.json", "user copy");

    sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();

    assert_eq!(fixture.read_user("ubuntu.json"), "user copy");
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run: `cargo test -p vmlord-core distro::tests::a_user_profile_is_never_replaced_by_a_bundled_profile`

Expected: compile failure because `sync_bundled_profiles` does not exist.

- [ ] **Step 3: Implement profile provenance and synchronization**

Use SHA-256 of bytes, validate filenames as a single `.json` component, write copied content through a temporary file plus rename, and rewrite the ownership document only after successful copies. Do not delete stale profiles.

```rust
pub fn sync_bundled_profiles(
    bundle: &Path,
    store: &SettingsStore,
) -> Result<(), DistroCatalogError>;
```

- [ ] **Step 4: Add RED/GREEN tests for first-run status**

Add `SettingsStore::load_or_create_with_status()` returning:

```rust
pub struct SettingsLoad {
    pub settings: AppSettings,
    pub created: bool,
}
```

Keep `load_or_create()` as a compatibility wrapper returning only `.settings`. Test absent file gives `created == true`, existing valid file gives `false`.

- [ ] **Step 5: Wire startup and update architecture documentation**

In `crates/vmlord/src/main.rs`, derive the installed profile path, synchronize before `DistroCatalog::load`, and retain `created` for Task 5. Update `ARCHITECTURE.md` to replace the statement that the installer writes profiles beside settings.

- [ ] **Step 6: Verify and commit**

Run: `cargo test -p vmlord-core && cargo check-windows`

Commit: `TASK-15: Seed bundled distribution profiles per user`

---

### Task 2: Define and Validate the Release Manifest

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/core/Cargo.toml`
- Create: `crates/core/src/update.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces: `ReleaseManifest`, `InstallerAsset`, `ValidatedUpdate`, `UpdateManifestError`.
- Produces: `ReleaseManifest::validate(current: &Version) -> Result<Option<ValidatedUpdate>, UpdateManifestError>`.
- Consumes: `semver::Version`, `serde`, and the fixed repository path `MrUndead1996/vm-lord`.

- [ ] **Step 1: Add the workspace `semver` dependency and failing manifest tests**

Use `semver = { version = "1", features = ["serde"] }`. Tests cover schema 1, newer/equal/older versions, 64 lowercase hexadecimal characters, nonzero bounded size, HTTPS, exact GitHub host, and a release path whose version segment equals the parsed manifest version.

```rust
#[test]
fn another_repository_cannot_supply_an_installer() {
    let manifest = manifest_with_url(
        "https://github.com/attacker/vm-lord/releases/download/v0.2.0/setup.exe",
    );
    assert!(matches!(
        manifest.validate(&Version::new(0, 1, 0)),
        Err(UpdateManifestError::InstallerUrl)
    ));
}
```

- [ ] **Step 2: Run tests and confirm RED**

Run: `cargo test -p vmlord-core update::tests`

Expected: compile failure because `update` is not defined.

- [ ] **Step 3: Implement minimal manifest types and validation**

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub version: Version,
    pub installer: InstallerAsset,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstallerAsset {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedUpdate {
    pub version: Version,
    pub installer: InstallerAsset,
}
```

Keep GitHub release notes out of serialized `ReleaseManifest`; Task 5 combines
`ValidatedUpdate` with the release notes into application state.

- [ ] **Step 4: Verify and commit**

Run: `cargo test -p vmlord-core update::tests && cargo check-windows`

Commit: `TASK-15: Validate release update manifests`

---

### Task 3: Download Release Metadata and Verified Installers

**Files:**
- Create: `crates/image/src/update.rs`
- Modify: `crates/image/src/http.rs`
- Modify: `crates/image/src/lib.rs`
- Modify: `crates/image/src/error.rs`
- Modify: `crates/image/Cargo.toml`

**Interfaces:**
- Produces: `fetch_latest_release() -> Result<GitHubRelease, UpdateDownloadError>`.
- Produces: `fetch_update_installer(update: &ValidatedUpdate, directory: &Path, progress: &ProgressPublisher<DownloadPhase>, cancel: &AtomicBool) -> Result<PathBuf, UpdateDownloadError>`.
- Consumes: `ReleaseManifest`, `InstallerAsset`, shared HTTPS agent configuration.

- [ ] **Step 1: Write failing pure response and size-validation tests**

Factor response parsing away from sockets. A GitHub release fixture must reject drafts/prereleases, missing manifest assets, oversized JSON, non-200 status, and installer bodies that exceed the manifest size.

```rust
#[test]
fn bytes_past_the_declared_installer_size_are_refused() {
    let error = validate_downloaded_size(101, 100).unwrap_err();
    assert!(matches!(error, UpdateDownloadError::SizeMismatch { .. }));
}
```

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test -p vmlord-image update::tests`

Expected: compile failure because the update download module does not exist.

- [ ] **Step 3: Implement GitHub metadata fetch and verified download**

Reuse `http::build_agent`, platform TLS roots, explicit timeouts, 64 KiB reads, cancellation, and progress throttling. Build `User-Agent` with `concat!("VMLord/", env!("CARGO_PKG_VERSION"))` and set `Accept: application/vnd.github+json`. Write to a unique `.part` file, verify byte count and SHA-256, then rename with `format!("VMLord-{}-x86_64-setup.exe", update.version)`. Delete invalid partials.

- [ ] **Step 4: Add local TCP fixture integration tests**

Serve fixed HTTP responses from `TcpListener` for manifest/release parsing, interrupted body, cancellation, and hash mismatch. Tests must not call GitHub.

- [ ] **Step 5: Verify and commit**

Run: `cargo test -p vmlord-image && cargo check-windows`

Commit: `TASK-15: Download and verify application updates`

---

### Task 4: Launch a Verified Installer Through the Windows Layer

**Files:**
- Create: `crates/platform/src/installer.rs`
- Modify: `crates/platform/src/lib.rs`
- Modify: `crates/platform/Cargo.toml`

**Interfaces:**
- Produces: `launch_installer(request: &InstallerLaunch) -> Result<(), RepositoryError>`.
- Consumes: a canonical absolute installer path and install-scope-preserving Inno Setup arguments.

- [ ] **Step 1: Write failing argument-construction tests**

```rust
#[test]
fn an_update_waits_for_vmlord_to_exit_without_becoming_silent() {
    let request = InstallerLaunch::new(PathBuf::from(r"C:\Temp\VMLord-0.2.0-setup.exe"));
    assert_eq!(request.arguments(), ["/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS"]);
}
```

- [ ] **Step 2: Run focused test and confirm RED**

Run: `cargo test-windows -p vmlord-platform installer::tests`

Expected: compile failure because `InstallerLaunch` is absent.

- [ ] **Step 3: Implement the Windows launch boundary**

Validate that the path is absolute, is a regular `.exe`, and has no alternate data stream spelling. Use `ShellExecuteExW` with the `runas` verb only when the existing installation requires elevation; do not invoke PowerShell or `cmd.exe`. Return once process creation succeeds.

```rust
pub struct InstallerLaunch {
    pub path: PathBuf,
    pub elevated: bool,
}

pub fn launch_installer(request: &InstallerLaunch) -> Result<(), RepositoryError>;
```

- [ ] **Step 4: Verify and commit**

Run: `cargo test-windows -p vmlord-platform installer::tests && cargo check-windows`

Commit: `TASK-15: Launch verified installers through Windows`

---

### Task 5: Add the Application Update State Machine and First-Run Signal

**Files:**
- Create: `crates/app/src/update.rs`
- Modify: `crates/app/src/lib.rs`
- Modify: `crates/app/Cargo.toml`
- Modify: `crates/core/src/settings.rs`
- Modify: `crates/vmlord/src/main.rs`
- Modify: `crates/vmlord/Cargo.toml`

**Interfaces:**
- Produces: `AvailableUpdate { validated: ValidatedUpdate, release_notes: String }` and `UpdateState::{Idle, Checking, Available, Downloading, Ready, Failed}`.
- Produces: `WorkspaceApp::{check_for_updates, download_update, cancel_update, install_update, poll_update, update_state, first_run}`.
- Consumes: injected `UpdateRuntime` implemented in `vmlord` by composing `image` and `platform`.

- [ ] **Step 1: Write failing state-transition tests**

Use a deterministic fake runtime and assert real application state: newer release becomes `Available`, refusal stays `Available`, corrupt download becomes `Failed`, cancellation returns to `Available`, and successful launch sets `installing` before requesting app exit.

```rust
pub trait UpdateRuntime: Send + Sync {
    fn check(&self) -> Result<Option<AvailableUpdate>, String>;
    fn download(
        &self,
        update: &AvailableUpdate,
        progress: ProgressPublisher<DownloadPhase>,
        cancel: Arc<AtomicBool>,
    ) -> Result<PathBuf, String>;
    fn launch(&self, installer: &Path) -> Result<(), String>;
}
```

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test -p vmlord-app update::tests`

Expected: compile failure because `UpdateRuntime` and update state are absent.

- [ ] **Step 3: Implement worker ownership and polling**

Keep blocking network work on a worker thread with one `mpsc::Receiver<UpdateEvent>`. `poll_update()` drains events and emits diagnostics on terminal failures. Refuse a second concurrent operation. Store the last automatic check timestamp in settings as an optional RFC3339 value and throttle it to 24 hours; manual checks ignore the throttle.

- [ ] **Step 4: Carry first-run status to the UI**

Add `WorkspaceApp::with_first_run(bool)` and `first_run()`. In `vmlord/main.rs`, use Task 1's `SettingsLoad.created`, construct the concrete runtime, and inject both values without putting network or Windows logic in UI.

- [ ] **Step 5: Verify and commit**

Run: `cargo test -p vmlord-app && cargo check-windows`

Commit: `TASK-15: Orchestrate confirmed application updates`

---

### Task 6: Present Updates and First-Run Settings in the UI

**Files:**
- Modify: `crates/ui/src/lib.rs`
- Modify: `crates/ui/locales/en-US.toml`
- Modify: `crates/ui/locales/ru-RU.toml`

**Interfaces:**
- Consumes: only `WorkspaceApp` update actions/state and `first_run()`.
- Produces: translated settings/update panel and confirmation dialogs.

- [ ] **Step 1: Write failing pure UI mapping tests**

Extract label/detail/button decisions into pure functions over `UpdateState`. Test that `Available` offers download, `Ready` offers install, `Downloading` offers cancellation/progress, and `Failed` offers retry without embedding business decisions in egui code.

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test-windows -p vmlord-ui update`

Expected: compile failure because update rendering helpers do not exist.

- [ ] **Step 3: Add first-run settings and update presentation**

Initialize `settings_form` from current settings when `application.first_run()` is true. Add an Updates section to the existing settings window with manual check, release notes, download progress, cancellation, and a separate install confirmation. Call `application.poll_update()` during the normal redraw path.

- [ ] **Step 4: Add both locale catalogues atomically**

Add matching `updates.*` and `settings.first_run_*` keys to English and Russian. Extend the existing locale-key parity test so either catalogue missing a key fails.

- [ ] **Step 5: Verify and commit**

Run: `cargo test-windows -p vmlord-ui && cargo check-windows`

Commit: `TASK-15: Add update and first-run settings UI`

---

### Task 7: Build the Distribution and Inno Setup Package

**Files:**
- Modify: `crates/xtask/src/main.rs`
- Create: `crates/xtask/src/release.rs`
- Modify: `crates/xtask/Cargo.toml`
- Modify: `.cargo/config.toml`
- Create: `about.toml`
- Create: `installer/third-party-licenses.hbs`
- Create: `installer/vmlord.iss`
- Create: `installer/check.ps1`
- Modify: `README.md`

**Interfaces:**
- Produces: `cargo release-manifest --tag v0.1.0 --installer target/installer/VMLord-0.1.0-x86_64-setup.exe --output target/installer/release-manifest.json`.
- Produces: `target/dist/LICENSE`, `target/dist/THIRD-PARTY-LICENSES.txt`, and compiled setup executable.
- Consumes: workspace version, completed `cargo dist` tree, canonical GPL file, dependency metadata.

- [ ] **Step 1: Write failing xtask tests for tag/version and manifest generation**

```rust
#[test]
fn a_release_tag_must_equal_the_workspace_version() {
    assert_eq!(tag_version("v0.1.0", "0.1.0").unwrap(), "0.1.0");
    assert!(tag_version("v0.2.0", "0.1.0").is_err());
}
```

Test installer size/hash from fixture bytes and deterministic JSON output.

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test -p xtask release::tests`

Expected: compile failure because `release` is absent.

- [ ] **Step 3: Extend dist staging and implement release manifest generation**

Copy `LICENSE`; use a pinned `cargo-about` release plus `about.toml` and the
repository template to generate `THIRD-PARTY-LICENSES.html` with dependency
copyright and licence texts. Keep the Task 22 accepted-licence set explicit and
fail on an unknown or missing licence. Add the `release-manifest` xtask command
and Cargo alias. Do not download dependencies or invoke a shell from Rust.

- [ ] **Step 4: Write the declarative Inno Setup script**

Use one stable `AppId`, `PrivilegesRequired=lowest`,
`PrivilegesRequiredOverridesAllowed=dialog`, `DefaultDirName={autopf}\VMLord`,
64-bit install mode, recursive payload copying, Start Menu/desktop tasks, and
post-install `vmlord.exe`. Exclude `%LOCALAPPDATA%\VMLord` from uninstall.

- [ ] **Step 5: Add installer structure validation**

`installer/check.ps1` fails if required binaries, `LICENSE`, notices, or
`distros/ubuntu.json` or generated third-party notices are absent and checks both auto install modes are enabled
in the script. Run it before invoking `ISCC.exe`.

- [ ] **Step 6: Verify and commit**

Run: `cargo test -p xtask && cargo check-windows && powershell.exe -File installer/check.ps1 target/dist`

On Windows also run: `iscc installer/vmlord.iss`

Commit: `TASK-15: Package VMLord with Inno Setup`

---

### Task 8: Publish Draft Releases and Mirror GitHub to Forgejo

**Files:**
- Create: `.github/workflows/release.yml`
- Create: `.github/workflows/mirror-forgejo.yml`
- Create: `.github/dependabot.yml`
- Modify: `README.md`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: GitHub tag `vX.Y.Z`, `GITHUB_TOKEN`, and secret `FORGEJO_SSH_PRIVATE_KEY`.
- Produces: draft GitHub Release assets and fast-forward-only Forgejo `main`/tag refs.

- [ ] **Step 1: Add a failing static workflow validation test**

Add xtask test/validator that parses both YAML files as data and asserts: release
trigger is `v*`; permissions default to read; release job alone gets
`contents: write`; actions use full commit SHAs; mirror uses no `--force`; and
no pull-request ref is mirrored.

- [ ] **Step 2: Run validator and confirm RED**

Run: `cargo run -p xtask -- workflow-check`

Expected: failure because workflows do not exist.

- [ ] **Step 3: Implement the release workflow**

Order steps as spec: checkout exact tag, verify reachability/version, install
Rust targets, run `cargo test-windows`, prepare selected payloads, run
`cargo dist`, validate staging, install Inno Setup 6.6, compile installer,
generate/recheck manifest, and create a draft release. Upload installer,
manifest, checksum, notices, and source archive.

- [ ] **Step 4: Implement the mirror workflow**

Trigger only on `main` and `v*`. Install the deploy key with masked output,
pin `git.mrundead.org` host key from a repository variable, add the Forgejo SSH
remote, and push the exact current branch/tag without force.

- [ ] **Step 5: Document operator setup and trust limits**

Document the initial full-history push to
`git@github.com:MrUndead1996/vm-lord.git`, required secret/variable names,
tagging commands, draft publication, SmartScreen warning, manual update path,
and recovery from mirror divergence. Update `ARCHITECTURE.md` with component
boundaries and the unsigned-release trust model.

- [ ] **Step 6: Verify locally and commit**

Run: `cargo run -p xtask -- workflow-check && cargo test-windows && cargo check-windows`

Commit: `TASK-15: Publish GitHub releases and mirror Forgejo`

---

### Task 9: Bootstrap GitHub and Prove the First Draft Release

**Files:**
- No source changes expected; any correction follows its owning task and gets a `TASK-15` commit.

**Interfaces:**
- Consumes: user-authorized GitHub repository, Forgejo deploy key secret, and release version `0.1.0`.
- Produces: public GitHub history, passing Actions, synchronized Forgejo `main`, and one inspected draft release.

- [ ] **Step 1: Verify the final branch**

Run: `cargo test-windows && cargo agent && cargo display-services && cargo check-windows && cargo run -p xtask -- workflow-check`

Expected: every command exits 0. Linker warnings already documented by the project are allowed; test failures are not.

- [ ] **Step 2: Bootstrap the GitHub upstream**

Add remote `github` with URL `git@github.com:MrUndead1996/vm-lord.git`, fetch to
confirm it is empty, then push the current Forgejo `main` and all existing tags
without force. Verify GitHub's `main` SHA equals local `main` before changing
the repository's collaboration URL or creating a PR.

- [ ] **Step 3: Push the feature branch and merge through review**

Push `task-15-releases` to GitHub, open a PR against `main`, assign
`MrUndead1996`, and request review from `MrUndead1996`. Do not merge without
explicit user approval.

- [ ] **Step 4: Configure mirror credentials after merge**

Add `FORGEJO_SSH_PRIVATE_KEY` as an Actions secret and the verified Forgejo host
key as `FORGEJO_SSH_HOST_KEY`. Trigger the mirror workflow manually or with a
new `main` commit and confirm Forgejo reaches the same `main` SHA.

- [ ] **Step 5: Create and inspect the first version tag**

After explicit user approval, create `v0.1.0`, push it to GitHub, wait for
Actions, and inspect every draft asset. Confirm the installer manifest hash
matches the downloaded setup executable. Publish the draft only after explicit
user approval; then verify the public latest-release API exposes it.

- [ ] **Step 6: Close the task only after release proof**

Mark Vikunja task 15 complete only when the published release exists, its
installer was manually installed in both scope modes on Windows, first-run
settings open, and update discovery can read that release without installing it
automatically.
