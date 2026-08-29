# AppSandbox Linux VM Import Design

**Task:** Vikunja #21  
**Scope:** Import completed Linux virtual machines from AppSandbox into VMLord. Hyper-V imports, Windows guests, AppSandbox templates, and VM export are out of scope.

## Goal

VMLord discovers compatible AppSandbox Linux VMs, copies a selected VM into VMLord-owned storage, converts the copied guest to VMLord's host and guest contracts, and verifies that it works as a normal VMLord VM. The AppSandbox source VM remains unchanged and usable.

An import is successful only after the copied guest boots, accepts VMLord's SSH key, connects through `vmlord-agent`, and passes its display and GPU checks. Merely copying a VHDX or producing a bootable guest is not success.

## Supported Source

The first version discovers VMs from `%ProgramData%\AppSandbox\vms.cfg`. A candidate is importable only when all of the following hold:

- it is a Linux VM, not a Windows VM or template;
- AppSandbox records installation as complete;
- its `disk.vhdx` exists and can be opened for the required validation and copy operations;
- the VM is stopped;
- SSH is enabled and the AppSandbox public key was deployed to the guest;
- the configured SSH user and port are valid;
- the destination name is valid and unused in VMLord;
- the destination volume has enough free space.

The discovery result includes incompatible entries with a precise reason instead of silently hiding them. This lets a user distinguish an unsupported VM from a parsing or storage failure.

The current live VM at `C:\ProgramData\AppSandbox\ubuntu` is the end-to-end validation source, not a special case in the implementation.

## User Experience

The VM list offers an **Import from AppSandbox** action. The import screen obtains candidates from the application layer and displays their compatibility, name, RAM, processor count, virtual disk size, network mode, GPU request, SSH user, and SSH port.

Selecting a compatible VM pre-fills its AppSandbox name. The user may change the destination name before starting. Name conflicts are reported before the disk copy begins; VMLord neither invents a suffix nor overwrites an existing VM.

Progress has explicit stages:

1. validating the source and destination;
2. copying the system disk;
3. creating the VMLord compute system;
4. starting the bootstrap environment;
5. converting the guest;
6. restarting with VMLord integration;
7. verifying SSH, agent, display, and GPU operation.

Copying supports progress reporting and cancellation. Cancellation or a normal failure removes VMLord-owned temporary data and compute-system state only. New user-facing text uses `t!` and is present in both locale catalogues.

## Architecture

The UI contains no import business logic and reads neither AppSandbox files nor Windows state directly. It sends discovery and import requests to the application layer and renders application-owned progress and results.

The application layer owns the import workflow, serializes conflicting VM operations, maps platform failures to diagnostics, and exposes recoverable incomplete imports. The core crate owns safe request, candidate, compatibility, stage, and result models.

The Windows platform layer owns:

- locating and parsing AppSandbox configuration;
- checking source state, file locks, disk properties, and free space;
- copying the VHDX through native Windows APIs with progress and cancellation;
- creating VMLord state files, HCS configuration, and metadata;
- the SSH bootstrap and guest-conversion orchestration;
- cleanup and recovery of partially created imports.

No AppSandbox C code or FFI is introduced. AppSandbox sources are a behavioural reference only. The implementation uses Rust and keeps Windows API calls and `unsafe` code inside platform-specific modules.

## Discovery and Parsing

The parser treats `vms.cfg` as untrusted persisted data. It supports multiple `[VM]` sections, preserves enough source location context for useful errors, rejects duplicate or contradictory fields, and does not infer required values from unrelated sections.

The importer maps these AppSandbox fields:

| AppSandbox field | VMLord meaning |
| --- | --- |
| `Name` | editable default destination name |
| `RamMB` | memory size |
| `CpuCores` | processor count |
| `HddGB` | expected virtual disk size, verified through Virtual Disk API |
| `NetworkMode=1` | NAT; unsupported values are reported explicitly |
| `GpuMode=1` | standard/default VMLord GPU request |
| `AdminUser` | SSH user |
| `SshEnabled` | bootstrap eligibility |
| `SshPort` | bootstrap SSH port |
| `SshDeployKey` | bootstrap key eligibility |
| `InstallComplete` | completed-install prerequisite |
| `VhdxPath` | source disk, constrained to the selected VM |

`ImagePath`, `TestMode`, display-window placement, AppSandbox runtime state files, and AppSandbox's published SSH endpoint are not copied into VMLord state. Guest distribution, release, architecture, and kernel are observed from the running copied guest instead of guessed from the original ISO filename.

## Storage and Registration

The source VHDX is copied; it is never moved, linked, adopted in place, or modified. AppSandbox's `vm.vmgs`, `vm.vmrs`, `vm_state.json`, `display_settings.json`, snapshots, and staging files are not imported.

The copy first lands in a VMLord-owned staging location carrying an import marker and a generated import ID. Only a complete, validated disk is promoted to the final VM directory. VMLord then creates a new VM UUID, fresh VMGS and VMRS files, its own `config.json`, SSH material, agent secret, and metadata.

The metadata model records an import lifecycle distinct from ordinary VM readiness. At minimum it distinguishes copying, bootstrap-ready, converting, verifying, needs-attention, and complete. An incomplete import is not presented as an ordinary healthy VM.

Startup recovery finds import markers and metadata left by an interrupted process. It offers a safe retry from the last confirmed idempotent stage or cleanup of VMLord-owned files. It never cleans an AppSandbox path.

## Two-Stage Guest Conversion

The first boot intentionally uses NAT and SSH without enabling VMLord GPU or display integration. The copied guest still contains AppSandbox's agent, services, `asb_drm` module, HvSocket expectations, and Plan9 share names; enabling both stacks at once would create conflicting and ambiguous guest state.

During the bootstrap boot, VMLord uses the existing AppSandbox private key directly from its protected source location. It does not copy that private key into VMLord storage. Over SSH, VMLord runs a versioned, idempotent conversion procedure that:

1. observes `/etc/os-release`, architecture, and kernel information;
2. selects compatible VMLord display and GPU payloads;
3. transfers a manifest-bound conversion bundle and verifies its hashes;
4. installs VMLord's SSH public key;
5. installs the VMLord agent secret and `vmlord-agent` service;
6. installs the selected VMLord display and GPU components;
7. stops and disables AppSandbox services;
8. removes obsolete AppSandbox files only after their replacements validate;
9. validates the installed systemd units and requests a normal shutdown.

The source VM is stopped throughout and is never contacted. All guest changes occur inside the copied VHDX.

After shutdown, VMLord persists the observed guest target, requested desktop profile, GPU mode, and other normal metadata. It rebuilds the compute system with VMLord's agent and display HvSocket services and the selected GPU/display configuration, then starts it a second time.

The old AppSandbox SSH key is no longer used after VMLord verifies access with its own per-VM key.

## Verification and Outcomes

Final verification checks:

- the guest accepts the per-VM VMLord SSH key;
- `vmlord-agent` authenticates with the generated secret;
- the agent reports the expected guest identity;
- requested display services become available;
- requested GPU preparation and probing complete successfully.

Failure before the copied disk is promoted rolls back automatically. A failure after guest conversion has begun records `needs-attention`, preserves the VMLord-owned copy for diagnosis or retry, and never reports a successful import. The user may explicitly delete that copy through the recovery flow.

Every external input is revalidated at the stage that consumes it. In particular, the importer rechecks source identity, stopped state, source disk properties, destination name, and available space immediately before copying, rather than trusting discovery-time observations.

Secrets and private key contents have no revealing `Display` or `Debug` implementation and never enter tracing or diagnostics. User-actionable failures use `vmlord_core::diagnostic!`; detailed ordinary events use `tracing`.

## Testing

Automated coverage includes:

- `vms.cfg` parsing with multiple VMs, missing and duplicate fields, malformed values, Windows guests, templates, incomplete installations, unsupported network/GPU values, and path inconsistencies;
- core validation and application workflow transitions;
- name-conflict detection before copying;
- source revalidation between discovery and import;
- virtual disk size validation and insufficient-space refusal;
- copy progress, cancellation, interrupted-copy recovery, and rollback;
- idempotent replay of each guest-conversion stage;
- preservation of `needs-attention` state after post-promotion failures;
- proof that cleanup is constrained to VMLord-owned paths;
- secret-redaction checks;
- UI state and both localization catalogues.

Repository verification uses `cargo check-windows` and `cargo test-windows`.

The manual end-to-end test imports `C:\ProgramData\AppSandbox\ubuntu`, verifies boot, SSH, agent, display, and GPU behaviour, and then compares the source configuration and VM files against their pre-test hashes, sizes, and timestamps. The test requires sufficient destination capacity for the approximately 155 GiB current VHDX plus working headroom and must not start or alter the AppSandbox VM.

## Documentation Impact

Implementation updates `ARCHITECTURE.md` to replace the statement that AppSandbox VMs are not migrated with the supported discovery, copy, conversion, ownership, recovery, and compatibility rules. User documentation explains prerequisites, space requirements, the two boot stages, unsupported source types, cancellation, recovery, and the fact that the source VM is retained.
