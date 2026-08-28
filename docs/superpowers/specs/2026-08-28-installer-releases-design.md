# Installer, Updates, and Releases Design

## Goal

Ship VMLord as a Windows installer, let the installed application offer safe
user-confirmed updates, and publish reproducible releases from GitHub Actions.
GitHub becomes the public source of truth; the existing Forgejo repository is a
read-only mirror.

This design implements Vikunja task 15. VMLord remains a Rust-native
application. The installer is declarative packaging, not an application
backend, and contains no settings validation or other business logic.

## Decisions

- The installer is built with Inno Setup 6.6.
- A tagged Git commit is the source of a release. Tags use `vX.Y.Z` and must
  match `[workspace.package].version` exactly.
- Releases are built with MSVC on GitHub-hosted `windows-latest` runners.
- Releases start as GitHub drafts and are published manually after inspection.
- Update installation always requires explicit confirmation in VMLord.
- The installer offers current-user and all-users installation modes.
- No Authenticode certificate is available. The release publishes a SHA-256
  digest, and VMLord verifies the downloaded installer before running it.
- The application owns settings creation and validation. The installer only
  offers to launch VMLord after setup.
- User data is never removed or replaced by installation, update, or uninstall.

## Repository Roles

`git@github.com:MrUndead1996/vm-lord.git` is the public upstream. Development
branches, pull requests, tags, Actions, and releases live there after the
initial migration. The complete existing Git history and tags are pushed to
GitHub before enabling release automation.

The Forgejo repository remains a backup mirror. A GitHub Actions workflow
pushes accepted `main` commits and tags to Forgejo using a dedicated SSH deploy
key stored as a GitHub Actions secret. It does not mirror feature branches or
pull-request refs. Pushes are fast-forward only; divergence fails visibly and
is resolved deliberately rather than overwritten.

Vikunja remains the task tracker. Moving source hosting does not migrate or
duplicate its tasks.

## Distribution Layout

`cargo dist` remains the single command that builds and stages release content
under `target/dist`. The staged tree contains:

```text
vmlord.exe
vmlord-com1.exe
vmlord-display.exe
vmlord-agent
LICENSE
THIRD-PARTY-LICENSES.txt
distros/*.json
gpu-payload/*                 when supplied to cargo dist
display-payload/*             when supplied to cargo dist
```

Inno Setup copies this tree below `{autopf}\VMLord`. In non-administrative mode
`{autopf}` resolves to the current user's Programs directory. In administrative
mode it resolves to Program Files. Companion binaries and payload directories
therefore stay beside `vmlord.exe`, matching the paths the application already
derives from its executable.

The setup program creates Start Menu and optional desktop shortcuts and
registers an uninstaller. Its final page offers “Launch VMLord and configure
it”. VMLord's own manifest requests elevation when the application starts; a
current-user installation itself does not require elevation.

## Per-User Distribution Profiles

VMLord currently loads distribution profiles from
`%LOCALAPPDATA%\VMLord\distros`, while an all-users installer cannot populate
every current and future user's Local AppData safely. The installed canonical
profiles therefore live in `{app}\distros` and the application synchronizes
them for the current user before loading the catalogue.

VMLord records which profile files came from the installed bundle. On startup
it copies a new bundled profile and replaces an older bundled copy when its
content changes. Files not recorded as bundle-owned are user files: VMLord
does not overwrite or remove them. A profile removed from a later bundle is not
deleted automatically, because deleting a file from a user-writable directory
would need stronger provenance than its name alone.

This synchronization belongs to `vmlord-core`: it is deterministic filesystem
policy and has no Windows API dependency. The composition root supplies the
installed `distros` path derived from `current_exe`; the catalogue continues to
read the per-user directory beside `settings.toml`.

## First-Run Settings

`SettingsStore::load_or_create` continues to create default settings under
`%LOCALAPPDATA%\VMLord`. It will report whether it created the file during this
launch. The application passes that fact to the UI, which opens the existing
settings form immediately for a new installation. The form uses the existing
application methods, validation, and translated text.

Cancelling the form leaves the valid defaults on disk and does not prevent
VMLord from starting. Later launches do not reopen the form automatically. The
installer neither writes nor parses `settings.toml`.

## Release Manifest

Each release contains `release-manifest.json` with this versioned schema:

```json
{
  "schema": 1,
  "version": "0.2.0",
  "installer": {
    "url": "https://github.com/MrUndead1996/vm-lord/releases/download/v0.2.0/VMLord-0.2.0-x86_64-setup.exe",
    "size": 12345678,
    "sha256": "64 lowercase hexadecimal characters"
  }
}
```

Release notes come from the GitHub Release rather than being duplicated in the
manifest. URLs must use HTTPS and the expected GitHub repository and release
path. Unknown schema versions, invalid semantic versions, invalid hashes,
unexpected hosts, and values exceeding configured download limits are refused.

`crates/xtask` creates the manifest from the completed installer, deriving its
size and SHA-256 from bytes rather than workflow input. It also rejects a tag
whose version does not equal the Cargo workspace version.

## Update Flow

Update responsibilities stay outside the UI:

1. The application layer requests the latest non-draft, non-prerelease GitHub
   Release and its manifest through the existing HTTP-capable image layer.
2. Pure core logic validates the release metadata and compares its semantic
   version with the running package version.
3. The UI receives `idle`, `checking`, `available`, `downloading`, `ready`, or
   `failed` state. It never performs network or Windows operations.
4. The user reviews the version and GitHub release notes and explicitly starts
   the download.
5. The installer is downloaded to a unique file below the user's temporary
   directory. Progress and cancellation use the same observable patterns as
   image downloads.
6. VMLord verifies the exact byte count and SHA-256. A mismatch deletes the
   temporary file and produces a user-facing diagnostic.
7. Only after a second explicit “Install” confirmation does the platform layer
   launch the verified installer. VMLord then exits cleanly so installed files
   are not in use.
8. Inno Setup detects the existing installation, preserves its selected scope,
   replaces application files, and optionally relaunches VMLord.

VMLord checks at startup at most once per 24 hours and also exposes a manual
“Check for updates” action. A failed automatic check stays unobtrusive in the
settings/update area; an explicitly requested check reports its error. Network
failure never prevents normal application startup.

SHA-256 protects against corruption and verifies that downloaded bytes match
the GitHub release metadata. Without Authenticode it does not provide Windows
publisher identity or eliminate SmartScreen warnings, and it does not protect
against compromise of the GitHub repository or release workflow. The user
documentation states these limitations plainly.

## Component Boundaries

- `installer/vmlord.iss`: declarative file placement, install-scope selection,
  shortcuts, uninstall, upgrade replacement, and optional post-install launch.
- `crates/xtask`: release-version checks, third-party notice generation,
  installer manifest creation, hashing, and release staging.
- `crates/core`: manifest types and validation, semantic version decision, and
  bundled-profile synchronization policy.
- `crates/image`: cancellable HTTPS retrieval of GitHub release metadata,
  manifest, and installer bytes. It remains platform-independent.
- `crates/platform`: Windows process launch for a verified installer. All new
  Windows API and `unsafe` code remains isolated here.
- `crates/app`: update state machine, scheduling, orchestration, and mapping
  failures to diagnostics.
- `crates/ui`: translated presentation and user commands only.

New user-facing text goes through `t!` and is added to both locale catalogues.
User-visible update failures use `vmlord_core::diagnostic!`; ordinary progress
details use `tracing`.

## GitHub Actions

The release workflow runs only for tags matching `v*` and has these ordered
jobs:

1. Check out the exact tag with a pinned action revision.
2. Verify that the tag is an annotated or lightweight tag on a commit reachable
   from `main`, and that its normalized version equals the Cargo version.
3. Install the required Rust targets and run `cargo test-windows`.
4. Build Linux guest artifacts and the MSVC Windows application through
   `cargo dist`, including prepared payloads selected by repository policy.
5. Compile `installer/vmlord.iss` with Inno Setup.
6. Generate and independently verify `release-manifest.json`, SHA-256 output,
   and third-party notices.
7. Create a draft GitHub Release for the tag and attach all artifacts.

The workflow uses the repository-provided `GITHUB_TOKEN` with only `contents:
write`; all other permissions are read-only or disabled. It does not publish a
release from a branch build.

The mirror workflow runs after a push to `main` or a version tag. It installs a
Forgejo SSH key from an Actions secret, pins the host key, and pushes the named
ref without `--force`. The private key is never printed or stored as an
artifact.

## Failure Handling and Recovery

- A failed build or test creates no release.
- A partially uploaded draft remains a draft and can be deleted or rerun
  without affecting installed clients.
- Invalid release metadata is ignored and diagnosed; it cannot trigger a
  process launch.
- Interrupted downloads leave no executable accepted as ready. Temporary files
  are cleaned on cancellation, validation failure, and later startup.
- Installer cancellation or UAC refusal leaves the old installation intact.
- An update never deletes `%LOCALAPPDATA%\VMLord`.
- Mirror divergence fails the workflow and preserves both histories.

## Testing

Core tests cover every manifest field, schema rejection, semantic version
ordering, release path restrictions, size limits, SHA-256 comparison, and
bundled-profile synchronization without deleting user files.

Application tests cover no update, available update, automatic-check throttling,
manual retry, cancellation, corrupt download, user rejection, and successful
handoff to the platform launcher. They assert application state and diagnostics,
not UI widgets or mocks alone.

Platform tests cover construction of the installer launch request without
running an installer. Windows integration checks exercise the actual process
creation boundary with an inert fixture executable.

Installer validation checks both install modes, required staged files,
shortcuts, upgrade identity, uninstall preservation of user data, and the
post-install launch option. CI runs `cargo check-windows`, `cargo test-windows`,
the xtask release validations, and an Inno Setup compile before release upload.

## Documentation Changes

Implementation updates `ARCHITECTURE.md` for distribution-profile seeding,
update component boundaries, install paths, and the release trust model.
`README.md` documents installation, SmartScreen expectations, manual updates,
release tagging, and the GitHub/Forgejo repository roles.

The project does not claim that an unsigned executable has a verified Windows
publisher. Obtaining an Authenticode certificate later can strengthen the same
pipeline without changing the manifest or user-confirmation design.
