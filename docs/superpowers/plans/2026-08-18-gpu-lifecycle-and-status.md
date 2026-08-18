# GPU-PV Lifecycle and Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the GPU-PV parts that already exist into one working cycle — a VM stores its GPU mode, a start stages a payload, exports the host's drivers, attaches adapters and tells the guest what to mount, the guest's report travels back, and the UI shows desired mode and runtime status separately.

**Architecture:** The desired mode and the guest triple are persisted in `VmComputeSystemMapping`. Starting a VM moves onto a background thread (`StartRegistry`, modelled on the existing `BuildRegistry`) because payload staging unpacks and hashes hundreds of megabytes. That thread stages the payload, builds Plan9 exports, writes them into the stored HCS configuration, starts the system, applies HCS GPU assignment and hands the share manifest to the agent listener. Observations from the start thread and from the agent session thread land in a shared in-memory `GpuFacts` map, which `summary()` reads on every refresh; `vmlord_app::derive_status` turns them into the status the UI paints.

**Tech Stack:** Rust 2024, `windows` crate for Win32/HCS, `prost` for the agent protocol, `egui` for the UI, `serde_json` for HCS documents.

**Spec:** `docs/superpowers/specs/2026-08-18-gpu-lifecycle-and-status-design.md`

## Global Constraints

- All new application code is Rust. The legacy C backend is not modified; it keeps serving unmigrated functions.
- `vmlord-platform` is the only crate that calls Windows APIs. It depends on `vmlord-core` (and `vmlord-gpu-payload`) and never on `vmlord-app` or `vmlord-ui`.
- The UI contains no business logic and never calls Windows APIs; it talks only to `vmlord_app::Application`.
- No PowerShell, WMI or external processes. Native APIs only.
- `unsafe` stays inside platform-specific modules.
- GPU is best effort: nothing about GPU may fail a VM start or change VM lifecycle.
- Nothing is retried: not staging, not assignment, not a partial outcome.
- No back-compat migrations. New mapping fields are `#[serde(default)]`; old VMs read back as `GpuMode::None` with no guest target.
- Commit subjects are `TASK-98: <comment>`.
- Test commands: `cargo test-windows -p <crate> <filter>` for the Windows crates, plain `cargo test -p vmlord-gpu-payload` for the portable one. Never prefix commands with `timeout`.
- Compile check for the whole workspace: `cargo check-windows`.

---

### Task 1: `AssignmentUnknown` — a run this process did not start

A VM that was already running when VMLord started has no observed assignment, and reporting `AssignmentPending` ("the host has not attached the GPU yet") would lie about the stage. `GpuAssignment` gains an `Unknown` variant and `GpuStatusCode` a matching code.

**Files:**
- Modify: `crates/core/src/gpu.rs:56-73` (`GpuAssignment`), `crates/core/src/gpu.rs:163-218` (`GpuStatusCode`)
- Modify: `crates/app/src/gpu.rs:80-135` (`derive_status`), `crates/app/src/gpu.rs:137-146` (`native_detail`)
- Test: `crates/app/src/gpu.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `GpuAssignment::Unknown` (unit variant), `GpuStatusCode::AssignmentUnknown` with `as_str()` `"gpu-assignment-unknown"`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/app/src/gpu.rs` `mod tests`:

```rust
    #[test]
    fn a_run_this_process_did_not_start_says_so_rather_than_claiming_to_be_waiting() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Unknown),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Mirror, running(), &facts, NOW);

        assert_eq!(status.state, GpuState::WaitingForGuest);
        assert_eq!(status.stage, GpuStage::Guest);
        assert_eq!(status.code, GpuStatusCode::AssignmentUnknown);
        assert_eq!(
            status.native, None,
            "nothing was observed, so there is no adapter to report"
        );
    }

    #[test]
    fn an_unobserved_assignment_still_lets_the_guest_report_for_itself() {
        let facts = VmGpuFacts {
            assignment: Some(GpuAssignment::Unknown),
            guest: Some(GuestGpuReport::Ready(rendering())),
            ..VmGpuFacts::default()
        };

        let status = derive_status(GpuMode::Default, running(), &facts, NOW);

        assert_eq!(
            status.state,
            GpuState::GuestReady,
            "a guest that renders is ready whether or not this process attached the GPU"
        );
        assert_eq!(status.code, GpuStatusCode::GuestReady);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-app gpu::tests`
Expected: FAIL — `no variant named Unknown found for enum GpuAssignment`.

- [ ] **Step 3: Add the variant and the code**

In `crates/core/src/gpu.rs`, add to `GpuAssignment` as its first variant:

```rust
    /// This VMLord process did not start this VM, so what is attached to it
    /// was never observed.
    ///
    /// Its own variant rather than `None`: "nothing has been attached yet" and
    /// "something was attached and nobody here saw it" are different sentences
    /// to a reader, and only the second one is true after a restart.
    Unknown,
```

In `GpuStatusCode`, add after `AssignmentPending`:

```rust
    /// The VM was started before this VMLord process, so what is attached to
    /// it is not known.
    AssignmentUnknown,
```

and in `as_str`:

```rust
            Self::AssignmentUnknown => "gpu-assignment-unknown",
```

- [ ] **Step 4: Handle it in `derive_status`**

In `crates/app/src/gpu.rs`, replace the `partial_reason` match with:

```rust
    let partial_reason = match assignment {
        GpuAssignment::Failed(reason) => {
            return status(
                GpuState::Failed,
                GpuStage::Assignment,
                reason.code,
                reason.message.clone(),
            );
        }
        // Nothing was observed, so there is nothing to call partial or
        // complete; whatever the guest says stands on its own.
        GpuAssignment::Unknown if facts.guest.is_none() => {
            return status(
                GpuState::WaitingForGuest,
                GpuStage::Guest,
                GpuStatusCode::AssignmentUnknown,
                "This VM was started before VMLord, so what is attached to it is not \
                 known; waiting for the guest to report."
                    .into(),
            );
        }
        GpuAssignment::Unknown => None,
        GpuAssignment::Partial { reason, .. } => Some(reason),
        GpuAssignment::Complete(_) => None,
    };
```

and add `GpuAssignment::Unknown => None,` to the match in `native_detail`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-app gpu::tests`
Expected: PASS, all of them, including the ten that existed before.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/gpu.rs crates/app/src/gpu.rs
git commit -m "TASK-98: Say when an attached GPU was never observed"
```

---

### Task 2: Choosing a payload without knowing the guest's kernel

`PayloadCatalog::select` compares a whole `GuestTarget`, `kernel_release` included, which the host cannot know before the guest boots. The guest already treats the kernel as soft (`gpu_recipe.rs`: distribution, release and architecture are the hard gate, DKMS rebuilds for whatever kernel runs). Selection by triple follows.

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/error.rs`, `crates/gpu-payload/src/lib.rs` (re-export), `crates/platform/src/gpu_staging.rs:24-35,45-63`
- Test: `crates/gpu-payload/src/catalog.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct GuestSelector<'a> { pub distribution: &'a str, pub release: &'a str, pub architecture: &'a str }`
  - `PayloadCatalog::select_for_guest(&self, guest: &GuestSelector<'_>) -> Result<&CatalogEntry, PayloadError>`
  - `PayloadError::NoPayloadForGuest { distribution: String, release: String, architecture: String }`
  - `StageGpuPayloadRequest.guest: GuestSelector<'a>` replacing `target: &'a GuestTarget`

- [ ] **Step 1: Write the failing tests**

Add to `crates/gpu-payload/src/catalog.rs` `mod tests` (the existing helpers there build a catalog from JSON; follow the shape of `a_target_selects_its_entry`):

```rust
    #[test]
    fn a_guest_selects_an_entry_whatever_kernel_it_runs() {
        let catalog = catalog_with(&[target_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic")]);

        let entry = catalog
            .select_for_guest(&GuestSelector {
                distribution: "ubuntu",
                release: "26.04",
                architecture: "amd64",
            })
            .expect("the triple matches, so the kernel must not decide");

        assert_eq!(entry.target().kernel_release, "7.0.0-14-generic");
    }

    #[test]
    fn the_newest_proven_kernel_wins_when_a_triple_has_several_entries() {
        let catalog = catalog_with(&[
            target_json("ubuntu", "26.04", "amd64", "7.0.0-9-generic"),
            target_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic"),
        ]);

        let entry = catalog
            .select_for_guest(&GuestSelector {
                distribution: "ubuntu",
                release: "26.04",
                architecture: "amd64",
            })
            .expect("one of the two entries must be chosen");

        assert_eq!(
            entry.target().kernel_release,
            "7.0.0-14-generic",
            "14 is newer than 9, which sorting the text the other way round would get wrong"
        );
    }

    #[test]
    fn a_guest_with_no_entry_is_told_which_guest_had_none() {
        let catalog = catalog_with(&[target_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic")]);

        let error = catalog
            .select_for_guest(&GuestSelector {
                distribution: "ubuntu",
                release: "24.04",
                architecture: "amd64",
            })
            .expect_err("no entry matches this release");

        assert!(
            error.to_string().contains("24.04"),
            "the error has to name the guest it found nothing for: {error}"
        );
    }

    #[test]
    fn an_empty_catalog_has_nothing_for_anyone() {
        let catalog = catalog_with(&[]);

        assert!(
            catalog
                .select_for_guest(&GuestSelector {
                    distribution: "ubuntu",
                    release: "26.04",
                    architecture: "amd64",
                })
                .is_err(),
            "the shipped catalog is empty today, so this is the ordinary answer"
        );
    }

    #[test]
    fn kernel_order_reads_the_numbers_and_not_the_text() {
        assert!(kernel_order("7.0.0-14-generic") > kernel_order("7.0.0-9-generic"));
        assert!(kernel_order("7.1.0-1-generic") > kernel_order("7.0.0-99-generic"));
        assert_eq!(kernel_order("7.0.0-14-generic"), kernel_order("7.0.0-14-lowlatency"));
    }
```

Add the two helpers beside them if the module does not already have equivalents:

```rust
    fn target_json(distribution: &str, release: &str, architecture: &str, kernel: &str) -> String {
        format!(
            r#"{{"distribution":"{distribution}","release":"{release}",
               "architecture":"{architecture}","kernel_release":"{kernel}","payload_abi":1}}"#
        )
    }
```

`catalog_with` builds a `PayloadCatalog` from a `CatalogDocument` JSON with one entry per target; copy the JSON skeleton the existing tests in this module already use for a valid entry and substitute the `target` object.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-gpu-payload catalog`
Expected: FAIL — `no method named select_for_guest`.

- [ ] **Step 3: Implement selection and the error**

In `crates/gpu-payload/src/error.rs` add the variant:

```rust
    NoPayloadForGuest { distribution: String, release: String, architecture: String },
```

and its `Display` arm:

```rust
            Self::NoPayloadForGuest { distribution, release, architecture } => write!(f, "no GPU payload for {distribution} {release} {architecture}"),
```

In `crates/gpu-payload/src/catalog.rs`:

```rust
/// A guest as the host knows it before that guest has booted.
///
/// Three fields and not four: `kernel_release` is a property of a running
/// kernel, and the host chooses a payload before there is one. The guest
/// checks applicability itself and DKMS rebuilds the module for whatever
/// kernel it runs, so the catalog's kernel records what a payload was proven
/// on rather than what it requires.
#[derive(Clone, Copy, Debug)]
pub struct GuestSelector<'a> {
    pub distribution: &'a str,
    pub release: &'a str,
    pub architecture: &'a str,
}

impl PayloadCatalog {
    /// The entry for a guest, ignoring the kernel it runs.
    ///
    /// When a triple has several entries the newest proven kernel wins: it is
    /// the one built against the most recent headers, and an older one buys
    /// nothing.
    pub fn select_for_guest(
        &self,
        guest: &GuestSelector<'_>,
    ) -> Result<&CatalogEntry, PayloadError> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.target.distribution.eq_ignore_ascii_case(guest.distribution)
                    && entry.target.release == guest.release
                    && entry.target.architecture.eq_ignore_ascii_case(guest.architecture)
            })
            .max_by_key(|entry| kernel_order(&entry.target.kernel_release))
            .ok_or_else(|| PayloadError::NoPayloadForGuest {
                distribution: guest.distribution.to_owned(),
                release: guest.release.to_owned(),
                architecture: guest.architecture.to_owned(),
            })
    }
}

/// A kernel release as numbers, so that 14 sorts above 9.
///
/// Every run of digits in order, and nothing else: `7.0.0-14-generic` and
/// `7.0.0-14-lowlatency` are the same kernel with different flavours, and the
/// flavour must not decide which payload is newer.
fn kernel_order(release: &str) -> Vec<u64> {
    release
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}
```

Re-export `GuestSelector` from `crates/gpu-payload/src/lib.rs` beside `GuestTarget`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-gpu-payload catalog`
Expected: PASS.

- [ ] **Step 5: Point staging at it**

In `crates/platform/src/gpu_staging.rs`, replace the `target` field of `StageGpuPayloadRequest` with:

```rust
    /// The guest this payload is for, as the host knows it before boot.
    pub guest: GuestSelector<'a>,
```

and in `stage_for_vm` replace `catalog.select(request.target)?` with
`catalog.select_for_guest(&request.guest)?`. Update the import line to bring in
`GuestSelector` and drop `GuestTarget` if it becomes unused. Fix the existing
test in that module that builds a `StageGpuPayloadRequest`.

- [ ] **Step 6: Run the platform staging tests**

Run: `cargo test-windows -p vmlord-platform gpu_staging`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/gpu-payload/src crates/platform/src/gpu_staging.rs
git commit -m "TASK-98: Choose a GPU payload by distribution rather than by kernel"
```

---

### Task 3: Record the GPU mode and the guest triple with the VM

**Files:**
- Modify: `crates/platform/src/metadata.rs:33-78` (`VmComputeSystemMapping`)
- Modify: `crates/platform/src/create.rs:217-235` (mapping construction)
- Test: `crates/platform/src/metadata.rs` `mod tests`, `crates/platform/src/create.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `VmComputeSystemMapping.gpu_mode: GpuMode` (`#[serde(default)]`)
  - `VmComputeSystemMapping.guest_target: Option<GuestTargetKey>` (`#[serde(default)]`)
  - `pub(crate) struct GuestTargetKey { pub distribution: String, pub release: String, pub architecture: String }` with `pub(crate) fn selector(&self) -> GuestSelector<'_>`
  - `pub(crate) fn guest_target_key(source: &VmSource) -> Option<GuestTargetKey>`

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/metadata.rs` `mod tests`:

```rust
    #[test]
    fn a_mapping_written_before_gpu_existed_reads_back_without_one() {
        let document = r#"{"vm_id":"00000000-0000-0000-0000-000000000001",
            "vm_name":"dev","hcs_compute_system_id":"vmlord-dev"}"#;

        let mapping: VmComputeSystemMapping =
            serde_json::from_str(document).expect("an older mapping must still read");

        assert_eq!(mapping.gpu_mode, GpuMode::None);
        assert_eq!(mapping.guest_target, None);
    }

    #[test]
    fn a_recorded_gpu_mode_survives_a_round_trip() {
        let mapping = VmComputeSystemMapping {
            gpu_mode: GpuMode::Mirror,
            guest_target: Some(GuestTargetKey {
                distribution: "ubuntu".into(),
                release: "26.04".into(),
                architecture: "amd64".into(),
            }),
            ..sample_mapping()
        };

        let encoded = serde_json::to_string(&mapping).expect("a mapping must serialize");
        let decoded: VmComputeSystemMapping =
            serde_json::from_str(&encoded).expect("a mapping must deserialize");

        assert_eq!(decoded.gpu_mode, GpuMode::Mirror);
        assert_eq!(decoded.guest_target.expect("recorded").release, "26.04");
    }

    #[test]
    fn a_cloud_image_names_the_guest_it_provisions() {
        let key = guest_target_key(&VmSource::CloudImage {
            image: CloudImage {
                profile: vmlord_core::distro::ubuntu(),
                release: "26.04".into(),
            },
            provisioning: sample_provisioning(),
        })
        .expect("a cloud image knows what it boots");

        assert_eq!(key.distribution, "ubuntu", "the catalog spells it lowercase");
        assert_eq!(key.release, "26.04");
        assert_eq!(key.architecture, "amd64");
    }

    #[test]
    fn installation_media_names_no_guest() {
        assert_eq!(
            guest_target_key(&VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into()
            }),
            None,
            "VMLord does not know what system is inside installation media"
        );
    }
```

`sample_mapping` and `sample_provisioning` follow the fixtures already in that
test module; add them if absent.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform metadata::tests`
Expected: FAIL — `struct VmComputeSystemMapping has no field named gpu_mode`.

- [ ] **Step 3: Add the fields and the conversion**

In `crates/platform/src/metadata.rs`, add to `VmComputeSystemMapping`:

```rust
    /// What the VM asks of the host's GPU.
    ///
    /// Recorded because a start has to know what to attach and the stored HCS
    /// configuration cannot answer it: the configuration describes the shares a
    /// VM was last started with, not the mode it was created with.
    ///
    /// A mapping written before this field existed reads as [`GpuMode::None`],
    /// which is what every VM created so far has.
    #[serde(default)]
    pub gpu_mode: GpuMode,
    /// The guest a GPU payload would have to suit, as far as VMLord knows it.
    ///
    /// `None` is a VM built from installation media: VMLord promises nothing
    /// about the system inside it, so there is nothing to select a payload
    /// from. It is deliberately not a guess.
    #[serde(default)]
    pub guest_target: Option<GuestTargetKey>,
```

and below the struct:

```rust
/// The three facts that pick a GPU payload out of the catalog.
///
/// Not `GuestTarget`: that type carries the kernel a payload was proven on,
/// which is a property of a booted guest and not of a VM that has never run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestTargetKey {
    pub distribution: String,
    pub release: String,
    pub architecture: String,
}

impl GuestTargetKey {
    pub(crate) fn selector(&self) -> GuestSelector<'_> {
        GuestSelector {
            distribution: &self.distribution,
            release: &self.release,
            architecture: &self.architecture,
        }
    }
}

/// Every VM VMLord builds is amd64; the field exists because the catalog has
/// one and a hard-coded string in three places would be three places to fix.
const GUEST_ARCHITECTURE: &str = "amd64";

/// What a source says about the guest it will produce.
pub(crate) fn guest_target_key(source: &VmSource) -> Option<GuestTargetKey> {
    match source {
        VmSource::LocalMedia { .. } => None,
        VmSource::CloudImage { image, .. } => Some(GuestTargetKey {
            // The catalog spells distributions the way the guest's
            // `/etc/os-release` does, which is lowercase; the profile spells
            // the name the way a person reads it.
            distribution: image.profile.name.to_ascii_lowercase(),
            release: image.release.clone(),
            architecture: GUEST_ARCHITECTURE.to_owned(),
        }),
    }
}
```

- [ ] **Step 4: Fill them in at creation**

In `crates/platform/src/create.rs`, add to the `VmComputeSystemMapping` literal:

```rust
            gpu_mode: request.gpu_mode,
            guest_target: guest_target_key(&request.source),
```

Add a test to that module's `mod tests`, beside the existing mapping assertions:

```rust
    #[test]
    fn a_created_vm_records_the_gpu_mode_and_the_guest_it_was_built_from() {
        let (_root, store, storage_root) = temp_root("create-records-gpu");
        let pipeline = pipeline();

        let mapping = pipeline
            .create(&store, &cloud_image_request("dev", GpuMode::Mirror), &storage_root, &monitor())
            .expect("creation must succeed");

        assert_eq!(mapping.gpu_mode, GpuMode::Mirror);
        assert_eq!(
            mapping.guest_target.expect("a cloud image names its guest").distribution,
            "ubuntu"
        );
    }
```

Adapt `cloud_image_request` to the fixture helpers already in `create.rs`
(`request(name)` and friends), adding a `gpu_mode` parameter.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform metadata::tests create::tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/src/metadata.rs crates/platform/src/create.rs
git commit -m "TASK-98: Record the GPU mode and guest triple with the VM"
```

---

### Task 4: Accept GPU modes, and refuse changing one on a live VM

**Files:**
- Modify: `crates/platform/src/repository.rs:975-1000` (`update_vm`), `:531-570` (`summary`)
- Test: `crates/platform/src/repository.rs` `mod tests`

**Interfaces:**
- Consumes: `VmComputeSystemMapping.gpu_mode` (Task 3).
- Produces: `update_vm` records `gpu_mode`; `summary()` reports the stored mode.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_vm_can_now_be_created_with_a_gpu() {
        let mut repository = fixture();

        repository
            .create_vm(create_request_with_gpu("dev", GpuMode::Default))
            .expect("the native backend must no longer refuse a GPU mode");

        let listed = repository.list_vms().expect("listing must succeed");
        assert_eq!(listed[0].gpu_mode, GpuMode::Default);
    }

    #[test]
    fn a_stopped_vm_may_change_its_gpu_mode() {
        let mut repository = fixture_with_vm("dev", GpuMode::None, VmState::Stopped);

        repository
            .update_vm(update_request("dev", GpuMode::Mirror))
            .expect("a stopped VM may change its mode");

        assert_eq!(
            repository.mapping("dev").expect("mapping").gpu_mode,
            GpuMode::Mirror
        );
    }

    #[test]
    fn a_running_vm_may_not_change_its_gpu_mode() {
        let mut repository = fixture_with_vm("dev", GpuMode::None, running_state());

        let error = repository
            .update_vm(update_request("dev", GpuMode::Mirror))
            .expect_err("the mode is applied at start, so it may not change under a running VM");

        assert!(
            error.to_string().contains("stop"),
            "the refusal has to say what to do about it: {error}"
        );
    }

    #[test]
    fn a_running_vm_may_still_change_its_ram_while_keeping_its_gpu_mode() {
        let mut repository = fixture_with_vm("dev", GpuMode::Mirror, running_state());

        repository
            .update_vm(VmUpdateRequest {
                ram_mb: 4096,
                ..update_request("dev", GpuMode::Mirror)
            })
            .expect("only the GPU mode is frozen while a VM runs");
    }
```

Build `fixture_with_vm` on the fixtures already in this module: it inserts a
mapping with the given mode and makes `list_known_vms` report the given state.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform repository::tests::a_vm_can_now`
Expected: FAIL — the backend refuses the mode.

- [ ] **Step 3: Replace the refusal in `update_vm`**

Remove the `if request.gpu_mode != GpuMode::None { ... }` block and put in its
place, after the mapping is read:

```rust
        // The mode is applied when the compute system is built and started, so
        // a change under a running VM would be a stored mode that does not
        // describe the GPU the guest actually has. RAM and CPU are different:
        // they are read from the configuration on the next start and nothing
        // claims they are in effect before then.
        if request.gpu_mode != mapping.gpu_mode && self.is_live(&mapping) {
            let error = RepositoryError::new(format!(
                "stop VM \"{}\" before changing its GPU mode",
                mapping.vm_name
            ));
            log::error!("{error}");
            return Err(error);
        }
```

and after `record_network_mode`:

```rust
        record_gpu_mode(&self.store, &mapping, request.gpu_mode)?;
```

with, beside `record_network_mode`:

```rust
fn record_gpu_mode(
    store: &MetadataStore,
    mapping: &VmComputeSystemMapping,
    gpu_mode: GpuMode,
) -> Result<(), RepositoryError> {
    if mapping.gpu_mode == gpu_mode {
        return Ok(());
    }

    store.insert(VmComputeSystemMapping {
        gpu_mode,
        ..mapping.clone()
    })?;
    log::info!(
        "VM \"{}\" ({}) now asks for GPU mode {gpu_mode:?}; the change applies the next \
         time it starts",
        mapping.vm_name,
        mapping.vm_id
    );
    Ok(())
}
```

`is_live` is the predicate `refuse_if_live` already uses; extract it if it is
inline there so both call one thing.

- [ ] **Step 4: Report the stored mode**

In `summary()`, replace the `gpu_mode: GpuMode::None` line and its comment with
`gpu_mode: mapping.gpu_mode,`. Read it into a local before `mapping.vm_name` is
moved, the way `network_mode` is read.

- [ ] **Step 5: Drop the create-side refusal**

In `create_vm`, remove the block that refuses a non-`None` `gpu_mode`, if one
exists there separately from `update_vm`'s.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform repository::tests`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/platform/src/repository.rs
git commit -m "TASK-98: Accept a GPU mode and freeze it under a running VM"
```

---

### Task 5: The facts channel

**Files:**
- Create: `crates/platform/src/gpu_facts.rs`
- Modify: `crates/platform/src/lib.rs` (add `mod gpu_facts;`), `crates/platform/src/repository.rs` (field, `summary`, the five cleanup points, reconnect)
- Test: `crates/platform/src/gpu_facts.rs` `mod tests`, `crates/platform/src/repository.rs` `mod tests`

**Interfaces:**
- Consumes: `GpuAssignment::Unknown` (Task 1), `mapping.gpu_mode` (Task 3).
- Produces:
  - `pub(crate) struct GpuFacts` — `Clone` (an `Arc` inside), `Default`
  - `fn record_assignment(&self, vm_id: Uuid, assignment: GpuAssignment)`
  - `fn record_guest(&self, vm_id: Uuid, report: GuestGpuReport)`
  - `fn forget(&self, vm_id: Uuid)`
  - `fn forget_all(&self)`
  - `fn snapshot(&self, vm_id: Uuid) -> VmGpuFacts`

- [ ] **Step 1: Write the failing tests**

Create `crates/platform/src/gpu_facts.rs` with the module documentation and its
tests only:

```rust
//! What has been observed about each running VM's GPU, while it runs.
//!
//! Two threads write here -- the one that starts a VM and the one that serves
//! its agent -- and the refresh that lists VMs reads. Nothing is persisted:
//! `VmGpuStatus` describes a moment, and facts from a process that is gone are
//! confirmed by nothing. A reconnected VM re-observes them within seconds.

#[cfg(test)]
mod tests {
    use super::GpuFacts;
    use uuid::Uuid;
    use vmlord_core::{
        GpuAssignment, GpuFailure, GpuStatusCode, GuestGpuDetail, GuestGpuReport, NativeGpuDetail,
    };

    #[test]
    fn a_vm_nothing_was_observed_about_has_nothing_to_report() {
        let facts = GpuFacts::default();

        assert_eq!(facts.snapshot(Uuid::from_u128(1)).assignment, None);
        assert_eq!(
            facts.snapshot(Uuid::from_u128(1)).observed_at,
            None,
            "inventing a time would date an observation that was never made"
        );
    }

    #[test]
    fn what_each_side_observed_is_kept_side_by_side() {
        let facts = GpuFacts::default();
        let vm = Uuid::from_u128(1);

        facts.record_assignment(
            vm,
            GpuAssignment::Complete(NativeGpuDetail {
                adapter: Some("NVIDIA RTX 4070".into()),
                adapters: 1,
            }),
        );
        facts.record_guest(
            vm,
            GuestGpuReport::Ready(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            }),
        );

        let snapshot = facts.snapshot(vm);
        assert!(matches!(snapshot.assignment, Some(GpuAssignment::Complete(_))));
        assert!(matches!(snapshot.guest, Some(GuestGpuReport::Ready(_))));
        assert!(snapshot.observed_at.is_some());
    }

    #[test]
    fn an_observation_is_dated_when_it_is_written() {
        let facts = GpuFacts::default();
        let vm = Uuid::from_u128(1);

        facts.record_assignment(vm, GpuAssignment::Unknown);
        let first = facts.snapshot(vm).observed_at.expect("recorded");
        facts.record_guest(
            vm,
            GuestGpuReport::Failed(GpuFailure::new(GpuStatusCode::GuestFailed, "no dxgkrnl")),
        );
        let second = facts.snapshot(vm).observed_at.expect("recorded");

        assert!(second >= first, "the newest observation dates the facts");
    }

    #[test]
    fn a_vm_that_stops_leaves_nothing_behind() {
        let facts = GpuFacts::default();
        let vm = Uuid::from_u128(1);
        facts.record_guest(
            vm,
            GuestGpuReport::Ready(GuestGpuDetail::default()),
        );

        facts.forget(vm);

        assert_eq!(
            facts.snapshot(vm).guest,
            None,
            "a stopped VM must not show yesterday's report on its next start"
        );
    }

    #[test]
    fn forgetting_one_vm_leaves_the_others_alone() {
        let facts = GpuFacts::default();
        facts.record_assignment(Uuid::from_u128(1), GpuAssignment::Unknown);
        facts.record_assignment(Uuid::from_u128(2), GpuAssignment::Unknown);

        facts.forget(Uuid::from_u128(1));

        assert!(facts.snapshot(Uuid::from_u128(2)).assignment.is_some());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_facts`
Expected: FAIL — `cannot find type GpuFacts`.

- [ ] **Step 3: Implement the store**

Above the test module in the same file:

```rust
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use uuid::Uuid;
use vmlord_core::{GpuAssignment, GuestGpuReport, VmGpuFacts};

/// The GPU facts of every VM this process has observed anything about.
///
/// Cloned into the threads that write; the clone shares the map rather than
/// copying it.
#[derive(Clone, Default)]
pub(crate) struct GpuFacts(Arc<Mutex<BTreeMap<Uuid, VmGpuFacts>>>);

impl GpuFacts {
    pub(crate) fn record_assignment(&self, vm_id: Uuid, assignment: GpuAssignment) {
        let mut facts = self.lock();
        let entry = facts.entry(vm_id).or_default();
        entry.assignment = Some(assignment);
        entry.observed_at = Some(SystemTime::now());
    }

    pub(crate) fn record_guest(&self, vm_id: Uuid, report: GuestGpuReport) {
        let mut facts = self.lock();
        let entry = facts.entry(vm_id).or_default();
        entry.guest = Some(report);
        entry.observed_at = Some(SystemTime::now());
    }

    /// Drops everything observed about one VM, for a run that is over.
    pub(crate) fn forget(&self, vm_id: Uuid) {
        self.lock().remove(&vm_id);
    }

    /// Drops everything, for a VMLord that is going away.
    pub(crate) fn forget_all(&self) {
        self.lock().clear();
    }

    pub(crate) fn snapshot(&self, vm_id: Uuid) -> VmGpuFacts {
        self.lock().get(&vm_id).cloned().unwrap_or_default()
    }

    /// Recovers a poisoned lock rather than propagating the panic: a thread
    /// that died must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Uuid, VmGpuFacts>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
```

Add `mod gpu_facts;` to `crates/platform/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform gpu_facts`
Expected: PASS.

- [ ] **Step 5: Wire it into the repository**

Add the field `gpu_facts: GpuFacts,` to `HcsVmRepository` and
`gpu_facts: GpuFacts::default(),` to its constructor.

In `summary()`, replace `gpu: VmGpuFacts::default(),` with
`gpu: self.gpu_facts.snapshot(mapping.vm_id),` — reading the id into a local
before `mapping` is moved.

Add `self.gpu_facts.forget(<vm id>);` immediately after each of the five
existing `agent_sessions.cancel(...)` calls (`stop_vm`, `force_stop_vm`,
`delete_vm`, the released-event loop in `take_diagnostics`) and
`self.gpu_facts.forget_all();` beside `agent_sessions.cancel_all()` on
shutdown.

In `listen_for_agent`, before starting the connection, record that a run this
process did not start was never observed:

```rust
        // A VM this process is only now discovering was started by someone
        // else, so nothing here saw what was attached to it. Saying "not yet"
        // would be a different and false sentence.
        if mapping.gpu_mode != GpuMode::None && self.gpu_facts.snapshot(mapping.vm_id).assignment.is_none() {
            self.gpu_facts
                .record_assignment(mapping.vm_id, GpuAssignment::Unknown);
        }
```

- [ ] **Step 6: Write the repository tests**

```rust
    #[test]
    fn a_reclaimed_vm_reports_that_its_assignment_was_never_observed() {
        let mut repository = fixture_with_vm("dev", GpuMode::Default, running_state());

        repository.initialize().expect("initialization must succeed");

        let listed = repository.list_vms().expect("listing must succeed");
        assert!(matches!(
            listed[0].gpu.assignment,
            Some(GpuAssignment::Unknown)
        ));
    }

    #[test]
    fn stopping_a_vm_drops_what_was_observed_about_its_gpu() {
        let mut repository = fixture_with_vm("dev", GpuMode::Default, running_state());
        repository
            .gpu_facts
            .record_guest("dev-id".parse().expect("uuid"), GuestGpuReport::Ready(GuestGpuDetail::default()));

        repository.stop_vm("dev").expect("stopping must succeed");

        assert_eq!(
            repository.gpu_facts.snapshot("dev-id".parse().expect("uuid")).guest,
            None
        );
    }
```

Use the actual VM id the fixture inserts rather than the placeholder above.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform repository::tests gpu_facts`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/gpu_facts.rs crates/platform/src/lib.rs crates/platform/src/repository.rs
git commit -m "TASK-98: Keep what was observed about each running VM's GPU"
```

---

### Task 6: The guest's report reaches the facts

`agent_session::serve` logs the guest's three answers and tells nobody. It gains
a sink.

**Files:**
- Modify: `crates/platform/src/agent_session.rs:121-190` (`serve`), `:300-400` (the three `report_*` functions)
- Modify: `crates/platform/src/agent.rs:113-130` (`AgentConnection::start`), and its worker loop
- Modify: `crates/platform/src/repository.rs` (`listen_for_agent` passes the sink)
- Test: `crates/platform/src/agent_session.rs` `mod tests`

**Interfaces:**
- Consumes: `GpuFacts` (Task 5).
- Produces:
  - `pub(crate) type GuestGpuSink<'a> = &'a dyn Fn(GuestGpuReport);`
  - `serve(stream, session, shares, vm_name, sink: GuestGpuSink<'_>)`
  - `AgentConnection::start(mapping, runtime_id, secret_path, shares, facts: GpuFacts)`

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/agent_session.rs` `mod tests`, beside the existing
manifest tests (which already drive `serve` against a scripted peer):

```rust
    #[test]
    fn a_guest_that_renders_is_reported_as_ready() {
        let reports = collect_reports(scripted_guest(&[
            mounted_everything(),
            recipe_all_ok(),
            probe(GpuProbeVerdict::Renders, "d3d12", "dxgkrnl", "/dev/dri/renderD128"),
        ]));

        assert_eq!(
            reports,
            vec![GuestGpuReport::Ready(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            })]
        );
    }

    #[test]
    fn a_guest_that_only_opened_the_device_is_reported_as_present_and_not_ready() {
        let reports = collect_reports(scripted_guest(&[
            mounted_everything(),
            recipe_all_ok(),
            probe(GpuProbeVerdict::DeviceOnly, "", "dxgkrnl", ""),
        ]));

        assert!(matches!(reports[0], GuestGpuReport::DevicePresent(_)));
    }

    #[test]
    fn a_guest_without_a_device_has_failed_and_says_which_check_found_that_out() {
        let reports = collect_reports(scripted_guest(&[
            mounted_everything(),
            recipe_all_ok(),
            probe(GpuProbeVerdict::NoDevice, "", "", ""),
        ]));

        let GuestGpuReport::Failed(failure) = &reports[0] else {
            panic!("a guest with no device has not got a GPU: {:?}", reports[0]);
        };
        assert_eq!(failure.code, GpuStatusCode::GuestFailed);
    }

    #[test]
    fn a_recipe_that_breaks_reports_the_stage_it_broke_at_without_waiting_for_a_probe() {
        let reports = collect_reports(scripted_guest(&[
            mounted_everything(),
            recipe_failing_at(GpuRecipeStep::ModuleBuild, "dkms build returned 1"),
        ]));

        let GuestGpuReport::Failed(failure) = &reports[0] else {
            panic!("a recipe that did not finish is a failure: {:?}", reports[0]);
        };
        assert!(
            failure.message.contains("dkms build returned 1"),
            "the guest's own words carry the detail: {}",
            failure.message
        );
    }
```

`collect_reports` runs `serve` against the scripted peer with a sink that
pushes into a `Vec`; `scripted_guest`, `mounted_everything`, `recipe_all_ok`,
`recipe_failing_at` and `probe` build the response frames the same way the
existing tests in this module build theirs.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform agent_session::tests`
Expected: FAIL — `serve` takes four arguments.

- [ ] **Step 3: Add the sink and the conversions**

In `agent_session.rs`:

```rust
/// Where a session hands what the guest said about its GPU.
///
/// A callback rather than a channel: `serve` is tested against a peer made of
/// bytes, and a sink that collects into a vector is the whole test harness.
pub(crate) type GuestGpuSink<'a> = &'a dyn Fn(GuestGpuReport);
```

Add the parameter `sink: GuestGpuSink<'_>` to `serve` and pass it on to the
three reporters, each of which takes it as `sink` — their own response
parameter is already called `report`. In `report_recipe`, after the existing logging:

```rust
    // A recipe that did not finish is the end of the guest's GPU: nothing
    // renders on a module that was not built, and asking for a probe would
    // only produce a second way of saying the same thing.
    if let Some(broken) = report
        .stages
        .iter()
        .find(|stage| !matches!(stage.state(), GpuRecipeStageState::Ok | GpuRecipeStageState::Skipped))
    {
        sink(GuestGpuReport::Failed(GpuFailure::new(
            GpuStatusCode::GuestFailed,
            format!("the guest's GPU recipe stopped at {:?}: {}", broken.step(), broken.message),
        )));
        return false;
    }
    true
```

making `report_recipe` return `bool` — whether to go on to the probe — and
having the `serve` loop skip `probe_gpu` when it returns `false`.

In `report_probe`, after the existing logging:

```rust
    let detail = GuestGpuDetail {
        driver: (!report.driver.is_empty()).then(|| report.driver.clone()),
        render_node: (!report.render_node.is_empty()).then(|| report.render_node.clone()),
    };
    sink(match report.verdict() {
        GpuProbeVerdict::Renders => GuestGpuReport::Ready(detail),
        GpuProbeVerdict::DeviceOnly => GuestGpuReport::DevicePresent(detail),
        // `Unspecified` is an agent that answered with a verdict this build
        // does not know, which is not a working GPU either.
        verdict => GuestGpuReport::Failed(GpuFailure::new(
            GpuStatusCode::GuestFailed,
            format!("the guest reports no usable GPU ({verdict:?})"),
        )),
    });
```

`report_mounts` keeps logging only: a refused share is already covered by the
recipe stage that needs it, and two reports for one cause would race.

- [ ] **Step 4: Carry the sink from the connection**

In `agent.rs`, add `facts: GpuFacts` to `AgentConnection::start`, move a clone
into the worker thread, and call `serve` with
`&|report| facts.record_guest(vm_id, report)`.

In `repository.rs`, pass `self.gpu_facts.clone()` at the `AgentConnection::start`
call site.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform agent_session`
Expected: PASS, including every session test that existed before.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/src/agent_session.rs crates/platform/src/agent.rs crates/platform/src/repository.rs
git commit -m "TASK-98: Carry the guest's GPU report back from its session"
```

---

### Task 7: Preparing a VM's GPU before it starts

Staging, exports, the configuration edit, and the one honest source of
`Partial`: how much of what the host has could actually be handed over.

**Files:**
- Create: `crates/platform/src/gpu_prepare.rs`
- Modify: `crates/platform/src/lib.rs` (`mod gpu_prepare;`), `crates/platform/src/gpu_exports.rs` (drop `#![allow(dead_code)]`), `crates/platform/src/gpu_staging.rs` (drop `#![allow(dead_code)]`)
- Test: `crates/platform/src/gpu_prepare.rs` `mod tests`

**Interfaces:**
- Consumes: `GuestTargetKey::selector` (Task 3), `stage_for_vm` (Task 2), `GpuExports::build`, `hcs_config::apply_plan9_shares`.
- Produces:
  - `pub(crate) struct PreparedGpu { pub(crate) manifest: GpuShareManifest, pub(crate) assignment: GpuAssignment }`
  - `pub(crate) fn coverage(adapters: &[HostGpuAdapter], exports: &GpuExports, payload_staged: bool, mode: GpuMode) -> GpuAssignment`
  - `pub(crate) fn prepare(mapping: &VmComputeSystemMapping, vm_directory: &Path, executable_directory: &Path, cache_root: &Path, cancel: &AtomicBool) -> Option<PreparedGpu>`

- [ ] **Step 1: Write the failing tests for `coverage`**

```rust
#[cfg(test)]
mod tests {
    use super::coverage;
    use std::path::PathBuf;
    use vmlord_core::{GpuAssignment, GpuMode, GpuShare, GpuStatusCode, HostGpuAdapter};
    use crate::gpu_exports::GpuExports;

    fn adapter(name: &str, has_package: bool) -> HostGpuAdapter {
        HostGpuAdapter {
            name: name.into(),
            instance_id: format!("PCI\\{name}"),
            interface_path: format!("\\\\?\\{name}"),
            driver_store: has_package.then(|| PathBuf::from(format!("C:\\DriverStore\\{name}"))),
            service: None,
        }
    }

    fn exports_for(packages: &[&str], payload: bool) -> GpuExports {
        let mut shares = vec![(GpuShare::wsl_lib(), PathBuf::from("C:\\lxss\\lib"))];
        if payload {
            shares.push((GpuShare::payload(), PathBuf::from("C:\\vm\\gpu-payload")));
        }
        for package in packages {
            shares.push((
                GpuShare::driver_package(package).expect("a package name must become a share"),
                PathBuf::from(format!("C:\\DriverStore\\{package}")),
            ));
        }
        GpuExports::for_test(shares)
    }

    #[test]
    fn every_adapter_handed_over_with_its_payload_is_complete() {
        let assignment = coverage(
            &[adapter("nvidia", true)],
            &exports_for(&["nvidia"], true),
            true,
            GpuMode::Default,
        );

        let GpuAssignment::Complete(detail) = assignment else {
            panic!("nothing was missing: {assignment:?}");
        };
        assert_eq!(detail.adapters, 1);
        assert_eq!(detail.adapter.as_deref(), Some("nvidia"));
    }

    #[test]
    fn an_adapter_whose_driver_could_not_be_exported_is_partial() {
        let assignment = coverage(
            &[adapter("nvidia", true), adapter("intel", false)],
            &exports_for(&["nvidia"], true),
            true,
            GpuMode::Mirror,
        );

        let GpuAssignment::Partial { detail, reason } = assignment else {
            panic!("one of two adapters has no package: {assignment:?}");
        };
        assert_eq!(detail.adapters, 2);
        assert_eq!(reason.code, GpuStatusCode::AssignmentPartial);
        assert!(
            reason.message.contains("1 of 2"),
            "the reason has to say how much is missing: {}",
            reason.message
        );
    }

    #[test]
    fn a_missing_payload_is_partial_and_says_so_in_its_own_words() {
        let assignment = coverage(
            &[adapter("nvidia", true)],
            &exports_for(&["nvidia"], false),
            false,
            GpuMode::Default,
        );

        let GpuAssignment::Partial { reason, .. } = assignment else {
            panic!("the payload is what a guest renders with: {assignment:?}");
        };
        assert!(
            reason.message.contains("payload"),
            "the reason has to name what is missing: {}",
            reason.message
        );
    }

    #[test]
    fn a_host_with_no_adapter_at_all_has_failed_rather_than_partly_succeeded() {
        let assignment = coverage(&[], &exports_for(&[], false), false, GpuMode::Default);

        let GpuAssignment::Failed(reason) = assignment else {
            panic!("there is no GPU here to be partly attached: {assignment:?}");
        };
        assert_eq!(reason.code, GpuStatusCode::HostNoAdapter);
    }

    #[test]
    fn a_single_adapter_is_named_and_several_are_only_counted() {
        let assignment = coverage(
            &[adapter("nvidia", true), adapter("intel", true)],
            &exports_for(&["nvidia", "intel"], true),
            true,
            GpuMode::Mirror,
        );

        let GpuAssignment::Complete(detail) = assignment else {
            panic!("both adapters were handed over: {assignment:?}");
        };
        assert_eq!(detail.adapters, 2);
        assert_eq!(
            detail.adapter, None,
            "there is no single adapter to name under Mirror"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_prepare`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement `coverage`**

```rust
//! Everything a VM's GPU needs before its compute system is started.
//!
//! Staging, exports and the configuration edit belong together because they
//! are one decision seen three times: what this host can actually hand this
//! guest. The result is the manifest the agent will be given and the
//! assignment fact the status is read from.
//!
//! Nothing here fails a start. Every way this can go wrong produces a VM that
//! runs with less GPU than it asked for, which is an ordinary outcome the
//! status has words for.

/// How much of what the host has was actually handed over.
///
/// HCS reports nothing about partiality -- it either accepted the update or it
/// did not -- so coverage is the only honest source of it. An adapter whose
/// driver package could not be exported is attached to a guest that cannot
/// mount its driver, and a missing payload is a guest with no userspace to
/// render with.
pub(crate) fn coverage(
    adapters: &[HostGpuAdapter],
    exports: &GpuExports,
    payload_staged: bool,
    mode: GpuMode,
) -> GpuAssignment {
    if adapters.is_empty() {
        return GpuAssignment::Failed(GpuFailure::new(
            GpuStatusCode::HostNoAdapter,
            "this host presents no GPU partition adapter",
        ));
    }

    let packages = exports
        .iter()
        .filter(|export| matches!(export.share().role, GpuShareRole::DriverPackage { .. }))
        .count();
    let detail = NativeGpuDetail {
        // Under `Default` HCS picks the host's preferred adapter, and naming
        // the only one there is is the one case where a name is not a guess.
        adapter: (matches!(mode, GpuMode::Default) && adapters.len() == 1)
            .then(|| adapters[0].name.clone()),
        adapters: u32::try_from(adapters.len()).unwrap_or(u32::MAX),
    };

    let mut missing = Vec::new();
    if packages < adapters.len() {
        missing.push(format!(
            "a driver package was exported for {packages} of {} adapter(s)",
            adapters.len()
        ));
    }
    if !payload_staged {
        missing.push("the Linux GPU payload is not staged for this VM".to_owned());
    }

    if missing.is_empty() {
        return GpuAssignment::Complete(detail);
    }
    GpuAssignment::Partial {
        detail,
        reason: GpuFailure::new(GpuStatusCode::AssignmentPartial, missing.join("; ")),
    }
}
```

Add `pub(crate) fn share(&self) -> &GpuShare` to `GpuExport` in
`gpu_exports.rs` if it is not there, and remove the `#![allow(dead_code)]` from
`gpu_exports.rs` and `gpu_staging.rs` together with the comments that explain
them.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform gpu_prepare`
Expected: PASS.

- [ ] **Step 5: Implement `prepare`**

Below `coverage` in the same file:

```rust
/// What a VM's GPU needs, ready to be started with.
pub(crate) struct PreparedGpu {
    /// What the guest will be told to mount.
    pub(crate) manifest: GpuShareManifest,
    /// What the host managed to hand over, for the status to be read from.
    pub(crate) assignment: GpuAssignment,
}

/// Stages the payload, builds the exports and writes them into the stored
/// configuration.
///
/// `None` is a VM that asks for no GPU: there is nothing to prepare and
/// nothing to say about it. Everything else returns something, including a
/// host that could hand over nothing at all -- that is a `Failed` assignment
/// and a start that carries on.
pub(crate) fn prepare(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    executable_directory: &Path,
    cache_root: &Path,
    cancel: &AtomicBool,
) -> Option<PreparedGpu> {
    if mapping.gpu_mode == GpuMode::None {
        return None;
    }

    let payload_staged = stage_payload(mapping, vm_directory, executable_directory, cache_root, cancel);
    let adapters = partition_adapters().unwrap_or_else(|error| {
        log::warn!(
            "the GPU adapters of this host could not be enumerated for VM \"{}\": {error}",
            mapping.vm_name
        );
        Vec::new()
    });
    let exports = GpuExports::build(&adapters, vm_directory).unwrap_or_default();
    let assignment = coverage(&adapters, &exports, payload_staged, mapping.gpu_mode);
    let manifest = exports.manifest();

    if let Err(error) = write_shares(vm_directory, &exports) {
        log::warn!(
            "the GPU shares of VM \"{}\" could not be written into its configuration: {error}",
            mapping.vm_name
        );
        return Some(PreparedGpu {
            manifest: GpuShareManifest::default(),
            assignment: GpuAssignment::Failed(GpuFailure::new(
                GpuStatusCode::AssignmentFailed,
                format!("the GPU shares could not be written into the configuration: {error}"),
            )),
        });
    }

    Some(PreparedGpu { manifest, assignment })
}

/// Stages the payload, and answers whether there is one.
///
/// A failure is logged and nothing more. The catalog shipped with this build
/// may have no entry for this guest at all -- today it has none for anyone --
/// and a VM whose guest cannot render is still a VM that runs.
fn stage_payload(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    executable_directory: &Path,
    cache_root: &Path,
    cancel: &AtomicBool,
) -> bool {
    let Some(target) = &mapping.guest_target else {
        log::info!(
            "VM \"{}\" was not built from a cloud image, so VMLord has no GPU payload to \
             stage for it",
            mapping.vm_name
        );
        return false;
    };

    match stage_for_vm(StageGpuPayloadRequest {
        executable_directory,
        cache_root,
        vm_directory,
        guest: target.selector(),
        progress: &|_progress| {},
        cancel,
    }) {
        Ok(_staged) => true,
        Err(error) => {
            log::warn!(
                "no GPU payload was staged for VM \"{}\": {error}",
                mapping.vm_name
            );
            false
        }
    }
}

/// Rewrites the stored configuration with this run's shares.
fn write_shares(vm_directory: &Path, exports: &GpuExports) -> Result<(), RepositoryError> {
    let path = configuration_path(vm_directory);
    let document = fs::read_to_string(&path).map_err(|error| {
        RepositoryError::new(format!(
            "failed to read the HCS configuration at {}: {error}",
            path.display()
        ))
    })?;
    let updated = apply_plan9_shares(&document, exports)?;
    fs::write(&path, updated).map_err(|error| {
        RepositoryError::new(format!(
            "failed to write the HCS configuration at {}: {error}",
            path.display()
        ))
    })
}
```

Give `GpuExports` a `Default` (an empty export list) so `unwrap_or_default`
works, and make `manifest()` on an empty set return an empty manifest.

- [ ] **Step 6: Test `prepare` against a temporary VM directory**

```rust
    #[test]
    fn a_vm_without_a_gpu_has_nothing_prepared_for_it() {
        let (_root, directory) = temp_vm_directory("prepare-none");

        let prepared = prepare(
            &mapping_with(GpuMode::None, None),
            &directory,
            &directory,
            &directory,
            &AtomicBool::new(false),
        );

        assert!(prepared.is_none());
    }

    #[test]
    fn a_vm_from_installation_media_is_prepared_without_a_payload() {
        let (_root, directory) = temp_vm_directory("prepare-no-target");
        write_minimal_configuration(&directory);

        let prepared = prepare(
            &mapping_with(GpuMode::Default, None),
            &directory,
            &directory,
            &directory,
            &AtomicBool::new(false),
        )
        .expect("a VM that asks for a GPU always has something prepared");

        assert!(
            !matches!(prepared.assignment, GpuAssignment::Complete(_)),
            "a guest with no payload has less GPU than it asked for"
        );
    }
```

`temp_vm_directory` and `write_minimal_configuration` follow the temp-directory
fixtures in `start.rs` and the configuration skeleton in `hcs_config.rs` tests.
These two run on any host, with or without a GPU, because they assert on the
absence rather than on the presence of adapters.

- [ ] **Step 7: Run the tests and the workspace check**

Run: `cargo test-windows -p vmlord-platform gpu_prepare` then `cargo check-windows`
Expected: PASS, and no `dead_code` warnings from the two modules whose allow was removed.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/gpu_prepare.rs crates/platform/src/lib.rs crates/platform/src/gpu_exports.rs crates/platform/src/gpu_staging.rs
git commit -m "TASK-98: Prepare a VM's GPU shares before its system starts"
```

---

### Task 8: Starting a VM on a thread

Payload staging unpacks and hashes hundreds of megabytes. That cannot happen on
the thread that draws the window.

**Files:**
- Create: `crates/platform/src/start_registry.rs`
- Modify: `crates/platform/src/lib.rs`, `crates/platform/src/repository.rs` (`start_vm`, `vm_state`, `take_diagnostics`, `update_vm`, `delete_vm`)
- Test: `crates/platform/src/start_registry.rs` `mod tests`, `crates/platform/src/repository.rs` `mod tests`

**Interfaces:**
- Consumes: `StartedVm` from `build.rs`.
- Produces:
  - `pub(crate) struct StartRegistry` (`Default`)
  - `fn start<F>(&self, vm_name: &str, run: F) -> Result<(), RepositoryError> where F: FnOnce() -> Option<StartedVm> + Send + 'static`
  - `fn contains(&self, vm_name: &str) -> bool`
  - `fn refuse_if_starting(&self, vm_name: &str) -> Result<(), RepositoryError>`
  - `fn take_started(&self) -> Vec<StartedVm>`
  - `fn join_all(&self)`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::StartRegistry;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn a_vm_being_started_is_refused_a_second_start() {
        let registry = StartRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start("dev", move || {
                while !held.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
                None
            })
            .expect("the first start must be accepted");

        let error = registry
            .start("dev", || None)
            .expect_err("a VM must not be started twice at once");

        assert!(error.to_string().contains("already"), "{error}");
        release.store(true, Ordering::Relaxed);
        registry.join_all();
    }

    #[test]
    fn a_start_that_is_over_stops_being_listed() {
        let registry = StartRegistry::default();
        registry.start("dev", || None).expect("accepted");

        registry.join_all();

        assert!(
            !registry.contains("dev"),
            "a start that has ended holds neither a row nor its name"
        );
    }

    #[test]
    fn a_start_that_panicked_still_stops_being_listed() {
        let registry = StartRegistry::default();
        registry
            .start("dev", || panic!("the start thread died"))
            .expect("accepted");

        registry.join_all();

        assert!(!registry.contains("dev"), "a row nobody clears never goes away");
    }

    #[test]
    fn another_vm_is_not_refused_because_this_one_is_starting() {
        let registry = StartRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start("dev", move || {
                while !held.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
                None
            })
            .expect("accepted");

        assert!(registry.refuse_if_starting("other").is_ok());
        release.store(true, Ordering::Relaxed);
        registry.join_all();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform start_registry`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement the registry**

Follow `BuildRegistry` exactly: a `Mutex<HashMap<String, Start>>` where `Start`
holds `finished: Arc<AtomicBool>`, `outcome: Arc<Mutex<Option<StartedVm>>>` and
`worker: Option<JoinHandle<()>>`; a `started: Mutex<Vec<StartedVm>>` queue; a
`reap()` that every query runs first; a `Finish` guard set on the way out so a
panicking thread still clears its row; and a `lock()` that recovers a poisoned
mutex. The doc comments say why each of those is there — copy the reasoning
from `build.rs`, not the words.

```rust
//! The VMs being started right now, by name.
//!
//! A start became a thread when GPU-PV joined it: staging a payload unpacks an
//! archive on a cold cache and hashes the whole tree on every start, and
//! neither belongs on the thread that draws the window. What the thread
//! produces -- the console session and the compute-system handle -- is left
//! here for the main thread to take over, because both are owned by the
//! repository and neither may be dropped on the way.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform start_registry`
Expected: PASS.

- [ ] **Step 5: Move `start_vm` onto it**

In `repository.rs`, add the field `starts: StartRegistry,` and rewrite
`start_vm`:

```rust
    fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;
        self.starts.refuse_if_starting(name)?;

        // Read here rather than on the thread: a VM VMLord does not know, or
        // whose directory cannot be named, is the return value of the call that
        // asked for it instead of a diagnostic a moment later.
        let mapping = self.mapping(name)?;
        let vm_directory = layout::vm_directory(&self.storage_root, name)?;
        let store = self.store.clone();
        let start = self.start.clone();
        let diagnostics = Arc::clone(&self.diagnostics);

        self.starts.start(name, move || {
            match start.start(&store, &mapping.vm_name, &vm_directory) {
                Ok(session) => Some(StartedVm { mapping, session }),
                Err(error) => {
                    diagnostics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(Diagnostic {
                            level: DiagnosticLevel::Error,
                            message: format!(
                                "VM \"{}\" could not be started: {error}",
                                mapping.vm_name
                            ),
                        });
                    None
                }
            }
        })
    }
```

`MetadataStore` and `VmStartPipeline` must be `Clone + Send + 'static` for this;
wrap either in an `Arc` field if it is not already.

In `take_diagnostics`, beside the existing `self.builds.take_started()`:

```rust
        let started = self.starts.take_started();
        self.adopt_started(started);
```

In `vm_state`, add a `starting: bool` parameter and return `VmState::Starting`
when it is set and HCS does not yet report the system as running; pass
`self.starts.contains(&mapping.vm_name)` from `summary()`.

Add `self.starts.refuse_if_starting(&request.name)?;` to `update_vm` and
`delete_vm`, beside their `refuse_if_building` calls, and
`self.starts.join_all();` beside `self.builds.cancel_all_and_join()` on
shutdown.

- [ ] **Step 6: Write the repository tests**

```rust
    #[test]
    fn a_vm_whose_start_is_in_flight_is_listed_as_starting() {
        let mut repository = fixture_with_blocked_start("dev");

        let listed = repository.list_vms().expect("listing must succeed");

        assert_eq!(listed[0].state, VmState::Starting);
    }

    #[test]
    fn a_vm_being_started_may_not_be_deleted() {
        let mut repository = fixture_with_blocked_start("dev");

        let error = repository
            .delete_vm(VmDeleteRequest {
                name: "dev".into(),
                delete_disks: true,
            })
            .expect_err("a VM in the middle of starting is not a VM to remove");

        assert!(error.to_string().contains("starting"), "{error}");
    }

    #[test]
    fn a_start_that_failed_is_reported_as_a_diagnostic_rather_than_lost() {
        let mut repository = fixture_with_failing_start("dev");

        repository.start_vm("dev").expect("the request is accepted");
        repository.starts.join_all();
        let diagnostics = repository.take_diagnostics();

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("could not be started")),
            "a background failure has to reach the user: {diagnostics:?}"
        );
    }
```

`fixture_with_blocked_start` holds the start thread on an `AtomicBool` the test
releases at the end, the way the registry tests do.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform start_registry repository::tests`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/start_registry.rs crates/platform/src/lib.rs crates/platform/src/repository.rs
git commit -m "TASK-98: Start VMs on a thread of their own"
```

---

### Task 9: The GPU stages inside the start

**Files:**
- Modify: `crates/platform/src/repository.rs` (`start_vm` closure, `listen_for_agent`)
- Test: `crates/platform/src/repository.rs` `mod tests`

**Interfaces:**
- Consumes: `prepare` / `PreparedGpu` (Task 7), `GpuFacts` (Task 5), `StartRegistry` (Task 8), `GpuAssignmentService::assign`.
- Produces: a start that records an assignment fact and hands the manifest to `listen_for_agent`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_start_records_what_it_managed_to_attach() {
        let mut repository = fixture_with_vm("dev", GpuMode::Default, VmState::Stopped);

        repository.start_vm("dev").expect("the request is accepted");
        repository.starts.join_all();

        assert!(
            repository.gpu_facts.snapshot(vm_id("dev")).assignment.is_some(),
            "a start that ran has something to say about the GPU it was asked for"
        );
    }

    #[test]
    fn a_vm_without_a_gpu_has_nothing_recorded_about_one() {
        let mut repository = fixture_with_vm("dev", GpuMode::None, VmState::Stopped);

        repository.start_vm("dev").expect("the request is accepted");
        repository.starts.join_all();

        assert_eq!(
            repository.gpu_facts.snapshot(vm_id("dev")).assignment,
            None,
            "a VM that asks for no GPU is not a VM whose GPU failed"
        );
    }

    #[test]
    fn a_gpu_that_could_not_be_attached_does_not_fail_the_start() {
        let mut repository = fixture_with_vm("dev", GpuMode::Mirror, VmState::Stopped);
        repository.gpu_assignment = failing_assignment_service();

        repository.start_vm("dev").expect("the request is accepted");
        repository.starts.join_all();
        let diagnostics = repository.take_diagnostics();

        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("could not be started")),
            "GPU is best effort and never fails a start: {diagnostics:?}"
        );
        assert!(matches!(
            repository.gpu_facts.snapshot(vm_id("dev")).assignment,
            Some(GpuAssignment::Failed(_))
        ));
    }

    #[test]
    fn a_gpu_is_attached_exactly_once_and_never_retried() {
        let mut repository = fixture_with_vm("dev", GpuMode::Mirror, VmState::Stopped);
        let attempts = repository.count_assignment_attempts();

        repository.start_vm("dev").expect("the request is accepted");
        repository.starts.join_all();

        assert_eq!(attempts.taken(), 1, "a partial GPU is not retried");
    }
```

To make these testable, the assignment step becomes a field the fixture can
substitute: `gpu_assignment: Arc<dyn Fn(&HcsSystem, GpuMode) -> Result<(), GpuFailure> + Send + Sync>`,
defaulting to `GpuAssignmentService::assign`. Follow the substitution pattern
`start.rs` already uses for its own steps.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform repository::tests::a_start_records`
Expected: FAIL — nothing records an assignment.

- [ ] **Step 3: Put the stages in the start closure**

The closure inside `start_vm` becomes, in order:

```rust
            // 1-3: stage the payload, build the exports, write them into the
            // configuration. Everything the GPU needs before HCS is asked to
            // build anything, because a Plan9 section is immutable for the
            // lifetime of a boot.
            let prepared = gpu_prepare::prepare(
                &mapping,
                &vm_directory,
                &executable_directory,
                &cache_root,
                &cancel,
            );
            if let Some(prepared) = &prepared {
                facts.record_assignment(mapping.vm_id, prepared.assignment.clone());
            }

            // 4: the start itself, unchanged.
            let session = match start.start(&store, &mapping.vm_name, &vm_directory) { ... };

            // 5: assignment, best effort. A failure replaces the fact recorded
            // above and leaves the VM running.
            if let Some(prepared) = &prepared
                && !matches!(prepared.assignment, GpuAssignment::Failed(_))
                && let Some(system) = open_started_system(&mapping)
                && let Err(failure) = assign(&system, mapping.gpu_mode)
            {
                log::warn!(
                    "VM \"{}\" is running without the GPU it asked for: {}",
                    mapping.vm_name,
                    failure.message
                );
                facts.record_assignment(mapping.vm_id, GpuAssignment::Failed(failure));
            }

            Some(StartedVm {
                mapping,
                session,
                // 6: what the agent listener will offer the guest.
                shares: prepared.map(|prepared| prepared.manifest),
            })
```

Add `shares: Option<GpuShareManifest>` to `StartedVm`, defaulting to `None`
where builds construct it, and have `adopt_started` pass it into
`listen_for_agent`, which passes it to `AgentConnection::start` in place of the
`None` and the comment that stands there today.

`executable_directory` is `std::env::current_exe()`'s parent, read in
`start_vm` before the thread; `cache_root` is the shared payload cache under
the storage root; `cancel` is an `AtomicBool` owned by the closure — nothing
cancels a start today, and `prepare` needs one to pass down.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform repository::tests`
Expected: PASS.

- [ ] **Step 5: Check the whole workspace**

Run: `cargo check-windows`
Expected: no errors, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/src/repository.rs crates/platform/src/build.rs
git commit -m "TASK-98: Attach the GPU and offer its shares as a VM starts"
```

---

### Task 10: The application layer reads the host once

**Files:**
- Modify: `crates/app/src/lib.rs:98-130` (state), `:231-247` (`start`), and the accessors near `:549-566`
- Test: `crates/app/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `VmRepository::host_gpu_capabilities`.
- Produces: `pub fn host_gpu_capabilities(&self) -> Option<&HostGpuCapabilities>`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_host_is_read_once_when_the_application_starts() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut application = Application::new(Box::new(CountingBackend {
            reads: Arc::clone(&counter),
        }));

        application.start();
        application.refresh();
        application.refresh();

        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "SetupAPI and the filesystem are not walked once per refresh"
        );
        assert!(application.host_gpu_capabilities().is_some());
    }

    #[test]
    fn a_backend_that_cannot_answer_leaves_the_host_unknown() {
        let mut application = Application::new(Box::new(SilentGpuBackend));

        application.start();

        assert!(
            application.host_gpu_capabilities().is_none(),
            "\"this backend cannot tell you\" is not \"this host cannot do it\""
        );
    }
```

`CountingBackend` and `SilentGpuBackend` are the test doubles already used in
this module, with `host_gpu_capabilities` implemented to count and to fail.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-app`
Expected: FAIL — `no method named host_gpu_capabilities`.

- [ ] **Step 3: Implement it**

Add the field:

```rust
    /// What the host can do for GPU-PV, read once when the application starts.
    ///
    /// `None` is a backend that could not answer, which is not the same as a
    /// host that cannot do it, and the two must not read the same way. Not
    /// re-read on refresh: the read walks SetupAPI and the filesystem, a form
    /// redraws sixty times a second, and a host does not change between two
    /// openings of a dialog.
    host_gpu: Option<HostGpuCapabilities>,
```

In `start()`, after the backend is initialized:

```rust
        self.host_gpu = match self.repository.host_gpu_capabilities() {
            Ok(capabilities) => Some(capabilities),
            Err(error) => {
                log::info!("this backend does not report host GPU capabilities: {error}");
                None
            }
        };
```

and the accessor:

```rust
    /// What the host can do for GPU-PV, or `None` when the backend cannot say.
    #[must_use]
    pub fn host_gpu_capabilities(&self) -> Option<&HostGpuCapabilities> {
        self.host_gpu.as_ref()
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-app`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/lib.rs
git commit -m "TASK-98: Read what the host can do for GPU-PV once"
```

---

### Task 11: The forms warn, and the details say what is attached

**Files:**
- Modify: `crates/ui/src/lib.rs:655-680` (create form), `:955-990` (edit form), `:1197-1230` (status helpers), `:1540-1550` (detail rows), `:184-200` (`EditVmForm`)
- Test: `crates/ui/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `Application::host_gpu_capabilities` (Task 10), `VmGpuStatus` fields.
- Produces:
  - `fn gpu_capability_warnings(capabilities: Option<&HostGpuCapabilities>, mode: GpuMode) -> Vec<String>`
  - `fn gpu_mode_locked(state: &VmState) -> Option<&'static str>`
  - `fn gpu_status_detail(status: Option<&VmGpuStatus>) -> String` (extended)

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_vm_without_a_gpu_is_warned_about_nothing() {
        let warnings = gpu_capability_warnings(Some(&host_without_gpu()), GpuMode::None);

        assert!(
            warnings.is_empty(),
            "a VM with no GPU has no reason to read about the DriverStore"
        );
    }

    #[test]
    fn a_host_without_an_adapter_warns_and_does_not_refuse() {
        let warnings = gpu_capability_warnings(Some(&host_without_gpu()), GpuMode::Default);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("without a GPU"), "{}", warnings[0]);
    }

    #[test]
    fn a_host_without_the_linux_payload_warns_about_the_guest_and_not_the_host() {
        let warnings = gpu_capability_warnings(Some(&host_without_payload()), GpuMode::Mirror);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("render"), "{}", warnings[0]);
    }

    #[test]
    fn a_host_that_is_short_of_both_says_both() {
        let warnings = gpu_capability_warnings(Some(&host_without_anything()), GpuMode::Default);

        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn a_backend_that_could_not_be_asked_says_nothing_at_all() {
        let warnings = gpu_capability_warnings(None, GpuMode::Default);

        assert!(
            warnings.is_empty(),
            "claiming a GPU is unavailable where we could not ask is worse than silence"
        );
    }

    #[test]
    fn the_gpu_mode_is_locked_while_the_vm_is_not_stopped() {
        assert!(gpu_mode_locked(&VmState::Running { agent_status: AgentStatus::Online }).is_some());
        assert!(gpu_mode_locked(&VmState::Starting).is_some());
        assert!(gpu_mode_locked(&VmState::Stopped).is_none());
    }

    #[test]
    fn an_active_gpu_names_the_adapter_and_the_render_node() {
        let detail = gpu_status_detail(Some(&VmGpuStatus {
            state: GpuState::GuestReady,
            stage: GpuStage::Guest,
            code: GpuStatusCode::GuestReady,
            message: "The guest renders on the GPU.".into(),
            native: Some(NativeGpuDetail {
                adapter: Some("NVIDIA RTX 4070".into()),
                adapters: 1,
            }),
            guest: Some(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            }),
            observed_at: SystemTime::UNIX_EPOCH,
        }));

        assert!(detail.contains("NVIDIA RTX 4070"), "{detail}");
        assert!(detail.contains("/dev/dri/renderD128"), "{detail}");
    }

    #[test]
    fn a_failed_gpu_shows_the_code_the_log_uses() {
        let detail = gpu_status_detail(Some(&failed_status(GpuStatusCode::GuestFailed)));

        assert!(
            detail.contains("gpu-guest-failed"),
            "the screen and the log have to be matchable: {detail}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-ui`
Expected: FAIL — `cannot find function gpu_capability_warnings`.

- [ ] **Step 3: Implement the helpers**

```rust
/// What is worth saying about this host before a VM asks it for a GPU.
///
/// Warnings and never refusals: GPU is applied best effort, so a host that
/// cannot deliver produces a VM that starts and says why, not a form that
/// cannot be submitted. `None` capabilities say nothing at all -- a backend
/// that could not be asked has not reported an absence.
fn gpu_capability_warnings(
    capabilities: Option<&HostGpuCapabilities>,
    mode: GpuMode,
) -> Vec<String> {
    if matches!(mode, GpuMode::None) {
        return Vec::new();
    }
    let Some(capabilities) = capabilities else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    if !capabilities.assignment.is_available() {
        warnings.push(
            "This host presents no GPU partition adapter, so the VM will start without a GPU."
                .to_owned(),
        );
    }
    if !capabilities.linux_payload.is_available() {
        warnings.push(
            "The Linux GPU userspace is not installed on this host, so the guest will see \
             the device but will not render on it."
                .to_owned(),
        );
    }
    warnings
}

/// Why the GPU mode cannot be changed right now, when it cannot.
///
/// The mode is applied while the compute system is built and started, so a
/// change under a live VM would be a stored mode that does not describe the
/// GPU the guest has.
fn gpu_mode_locked(state: &VmState) -> Option<&'static str> {
    match state {
        VmState::Stopped => None,
        _ => Some("Stop the VM to change its GPU mode."),
    }
}
```

Extend `gpu_status_detail` to append, after the state and the message: the
adapter name when `native` names one, the render node when `guest` names one,
and the stable `code` in parentheses when the state is `Failed` or `Degraded`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS.

- [ ] **Step 5: Draw them**

In the create form, after the GPU combo box row, add a row that renders each
warning from `gpu_capability_warnings(self.application.host_gpu_capabilities(), form.gpu_mode)`
as `ui.small(...)` in the warning colour the rest of the file uses.

In the edit form, do the same, and wrap the combo box in
`ui.add_enabled_ui(gpu_mode_locked(&form.state).is_none(), |ui| { ... })` with
the returned reason as `.on_disabled_hover_text(...)`. Add `state: VmState` to
`EditVmForm` and fill it from the summary the form is built from.

Replace the sentence "RAM and CPU are editable. GPU and network are not wired
to the native backend yet." with "RAM, CPU and GPU are editable; the GPU mode
only while the VM is stopped. Network is not wired to the native backend yet."

- [ ] **Step 6: Run every test and check the workspace**

Run: `cargo test-windows` then `cargo check-windows`
Expected: PASS across the workspace, no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/lib.rs
git commit -m "TASK-98: Warn about the host's GPU and show what a VM got"
```

---

### Task 12: Record the wiring in ARCHITECTURE.md

**Files:**
- Modify: `ARCHITECTURE.md` (the GPU-PV sections)

- [ ] **Step 1: Describe the cycle**

Add to the GPU-PV part of `ARCHITECTURE.md`, in the voice of the surrounding
text:

- The GPU mode and the guest triple live in `VmComputeSystemMapping`; nothing
  else persists about GPU.
- A start runs on a thread because staging a payload unpacks and hashes it; the
  stages are staging, exports, configuration, start, assignment, manifest.
- `Partial` is derived from export coverage, because HCS reports no partiality.
- Facts are in memory only; a VM reclaimed from a previous process reports
  `gpu-assignment-unknown` until its guest answers.
- The mode is frozen while a VM is not stopped; deletion is stopped-only;
  nothing about GPU is retried.

- [ ] **Step 2: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "TASK-98: Record the GPU-PV lifecycle in the architecture"
```

---

## Done when

- `cargo test-windows` passes across the workspace.
- `cargo check-windows` is clean, with no `dead_code` allowances left in
  `gpu_exports.rs` or `gpu_staging.rs`.
- A VM created with `Default` or `Mirror` stores its mode, starts in the
  background, records an assignment fact, and shows a runtime GPU status
  distinct from its desired mode.
- Real-host verification is TASK-99 and is deliberately not attempted here.
