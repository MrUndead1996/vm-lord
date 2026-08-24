# Native display integration implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pressing Connect on a running VM with a working desktop opens
`vmlord-display.exe` on that VM's native display, through UI → app → core →
platform, with no Windows API in the UI and no AppSandbox IDD involved.

**Architecture:** Three HvSocket services are listed in the compute system's
configuration so the partition has them. The repository's `open_display`
performs a preflight over what it already knows, then starts one viewer process
per Connect and keeps a thread on its launch pipes: VMLord holds the VM's
secret and drives the control handshake through the viewer's relay, hands over
one session's channel keys, and answers a later request for a fresh session.
Two facts the display model has always declared and nobody ever recorded --
whether the guest offers its display, and whether the desktop finished
installing -- are filled in from the report the agent already delivers.

**Tech Stack:** Rust 2024, `vmlord-display-protocol` (session machine and
records), `vmlord-display-viewer` (the launch contract's schema), HCS via the
`windows` crate, egui for the UI.

**Spec:** `docs/superpowers/specs/2026-08-24-native-display-integration-design.md`

## Global Constraints

* Commit subjects are `TASK-121: <comment>`; work stays on the current branch
  `task-121-native-display`, and no merge request is opened without the user
  asking for one.
* Build and test with the workspace aliases: `cargo check-windows` to
  compile-check, `cargo test-windows` to run the tests. Never prefix a command
  with `timeout`.
* The UI contains no business logic and calls no Windows API.
* `unsafe` stays inside the existing platform modules; nothing this plan adds
  needs any.
* No secret, token, channel key or pixel is ever formatted into a log line or
  an error message.
* No compatibility migration for VMs created before this task: they are
  recreated, not migrated.
* Every new module carries the crate's documentation style -- a `//!` header
  saying why the module exists, and doc comments that give the reason for a
  decision rather than restating the code.

---

### Task 1: The guest's readiness reaches the display facts

`VmDisplayFacts::guest` has never been written by any backend, so
`DisplayState` can never leave `WaitingForGuest`. The agent already delivers
the fact: `ApplyDisplayRecipeResponse` ends with the `SERVICES_START` stage,
which the guest marks `Ok` only once both units are active and the broker
socket exists.

**Files:**
- Modify: `crates/platform/src/agent_session.rs` (the
  `GuestDisplayPayloadReport` struct near line 107, and `report_display_recipe`
  near line 677)
- Modify: `crates/platform/src/display_runs.rs` (a new recorder)
- Modify: `crates/platform/src/agent.rs` (the display sink near line 241)

**Interfaces:**
- Produces: `GuestDisplayPayloadReport::guest: Option<GuestDisplayReport>`;
  `DisplayRuns::record_guest_display(&self, vm_id: Uuid, report: GuestDisplayReport)`.
- Consumes: nothing from other tasks.

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/agent_session.rs`, inside its `mod tests`:

```rust
#[test]
fn a_guest_whose_services_are_running_is_a_guest_that_offers_its_display() {
    let report = ApplyDisplayRecipeResponse {
        stages: vec![DisplayRecipeStage {
            step: DisplayRecipeStep::ServicesStart as i32,
            state: DisplayRecipeStageState::Ok as i32,
            message: "the display broker and session are running".to_owned(),
        }],
        versions: None,
    };
    let seen = std::cell::RefCell::new(None);

    super::report_display_recipe(&report, "dev", &|report| {
        *seen.borrow_mut() = report.guest;
    });

    assert!(matches!(
        seen.into_inner(),
        Some(GuestDisplayReport::Ready(_))
    ));
}

#[test]
fn a_payload_that_carries_no_services_is_a_display_that_will_never_arrive() {
    let report = ApplyDisplayRecipeResponse {
        stages: vec![DisplayRecipeStage {
            step: DisplayRecipeStep::ServicesStart as i32,
            state: DisplayRecipeStageState::Skipped as i32,
            message: "this payload carries no display services".to_owned(),
        }],
        versions: None,
    };
    let seen = std::cell::RefCell::new(None);

    super::report_display_recipe(&report, "dev", &|report| {
        *seen.borrow_mut() = report.guest;
    });

    let Some(GuestDisplayReport::Failed(failure)) = seen.into_inner() else {
        panic!("a payload with no services cannot offer a display");
    };
    assert_eq!(failure.code, DisplayStatusCode::PayloadInvalid);
}

#[test]
fn a_recipe_that_stopped_reports_the_guest_as_failed_for_the_same_reason() {
    let report = ApplyDisplayRecipeResponse {
        stages: vec![DisplayRecipeStage {
            step: DisplayRecipeStep::ModuleBuild as i32,
            state: DisplayRecipeStageState::Failed as i32,
            message: "the module did not build".to_owned(),
        }],
        versions: None,
    };
    let seen = std::cell::RefCell::new(None);

    super::report_display_recipe(&report, "dev", &|report| {
        *seen.borrow_mut() = report.guest;
    });

    let Some(GuestDisplayReport::Failed(failure)) = seen.into_inner() else {
        panic!("a recipe that stopped is a guest that offers nothing");
    };
    assert_eq!(failure.code, DisplayStatusCode::PayloadBuildFailed);
}
```

Add whatever of `GuestDisplayReport`, `DisplayStatusCode`,
`DisplayRecipeStage`, `DisplayRecipeStep` and `DisplayRecipeStageState` the
test module does not already import.

In `crates/platform/src/display_runs.rs`, inside its `mod tests`:

```rust
#[test]
fn what_the_guest_says_about_its_services_survives_a_payload_report() {
    let runs = DisplayRuns::default();
    let vm = Uuid::from_u128(1);

    runs.record_guest_display(vm, GuestDisplayReport::Ready(GuestDisplayDetail::default()));
    runs.record_guest_payload(vm, Some("0.2.0".into()), None, Some("0.2.0".into()), None);

    assert!(
        matches!(runs.snapshot(vm).guest, Some(GuestDisplayReport::Ready(_))),
        "a version report is not a reason to forget that the guest is offering a display"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform display`
Expected: FAIL -- no field `guest`, no method `record_guest_display`.

- [ ] **Step 3: Add the field and the recorder**

In `crates/platform/src/agent_session.rs`, extend the report:

```rust
pub(crate) struct GuestDisplayPayloadReport {
    pub(crate) installed: Option<String>,
    pub(crate) previous: Option<String>,
    pub(crate) loaded: Option<String>,
    pub(crate) failure: Option<DisplayFailure>,
    /// What the guest's own display services are doing, when this report says.
    ///
    /// `None` is a report that has nothing to say about them -- a mount that
    /// failed, or an update, which changes versions and not readiness -- and
    /// leaves whatever was last observed standing.
    pub(crate) guest: Option<GuestDisplayReport>,
}
```

At the end of `report_display_recipe`, beside the failure it already derives:

```rust
    // The last stage is the readiness Connect waits for: the guest marks it
    // `Ok` only once both units are active and the socket between them exists.
    let services = report
        .stages
        .iter()
        .find(|stage| stage.step() == DisplayRecipeStep::ServicesStart);
    let guest = match (services.map(DisplayRecipeStage::state), &failure) {
        (Some(DisplayRecipeStageState::Ok), _) => {
            Some(GuestDisplayReport::Ready(GuestDisplayDetail::default()))
        }
        (Some(DisplayRecipeStageState::Skipped), _) => {
            Some(GuestDisplayReport::Failed(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadInvalid,
                "this display payload carries no display services",
            )))
        }
        (_, Some(failure)) => Some(GuestDisplayReport::Failed(failure.clone())),
        // A report with neither is a guest that answered a recipe it had
        // nothing to run; saying nothing is what leaves the UI waiting rather
        // than claiming a desktop that is not there.
        _ => None,
    };

    sink(GuestDisplayPayloadReport {
        installed: some_version(&versions.installed),
        previous: some_version(&versions.previous),
        loaded: some_version(&versions.loaded),
        failure,
        guest,
    });
```

`DisplayRecipeStage::state` is prost's generated accessor, so
`stage.state()` is the enum; use whichever spelling the surrounding code
already uses. Add `..GuestDisplayPayloadReport::default()` where the struct is
built elsewhere in the file -- the mount-failure path near line 637 and the
update path in `agent.rs` near line 255 already use it.

In `crates/platform/src/display_runs.rs`:

```rust
    /// Records what the guest said about the display services inside it.
    ///
    /// Separate from the payload versions because the two are observed
    /// separately: an update changes what is installed and says nothing about
    /// whether anything is listening.
    pub(crate) fn record_guest_display(&self, vm_id: Uuid, report: GuestDisplayReport) {
        let mut runs = self.lock();
        let entry = runs.entry(vm_id).or_default();
        entry.facts.guest = Some(report);
        entry.facts.observed_at = Some(SystemTime::now());
    }
```

In `crates/platform/src/agent.rs`, in the display sink closure:

```rust
                        &|report| {
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

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform display`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/agent_session.rs crates/platform/src/display_runs.rs crates/platform/src/agent.rs
git commit -m "TASK-121: Record what the guest says about its display services"
```

---

### Task 2: A desktop that is running is a desktop that installed

`display_provisioning` is written once when a VM is created -- `Pending` for a
GNOME profile -- and nothing ever moves it. The application refuses to look
past a provisioning that is not `Ready`, so Connect would stay disabled on a
guest that is offering its desktop right now. The proof that the desktop
installed is the guest running the display services on top of it, which Task 1
now records.

**Files:**
- Modify: `crates/platform/src/repository.rs` (`take_diagnostics`, near line
  1430, and a new private method beside it)

**Interfaces:**
- Consumes: `DisplayRuns::snapshot(vm_id).guest` from Task 1.
- Produces: nothing other tasks call; the effect is a stored
  `DisplayProvisioning::Ready`.

- [ ] **Step 1: Write the failing test**

In `crates/platform/src/repository.rs`'s `mod tests`:

```rust
#[test]
fn a_guest_that_offers_its_desktop_records_the_desktop_as_installed() {
    let (mut repository, _root) = repository_with_store();
    let mapping = VmComputeSystemMapping {
        desktop_profile: DesktopProfile::Gnome,
        display_provisioning: DisplayProvisioning::Pending,
        ..mapping_for("dev")
    };
    repository.store.insert(mapping.clone()).expect("a mapping");
    repository.display_runs.record_guest_display(
        mapping.vm_id,
        GuestDisplayReport::Ready(GuestDisplayDetail::default()),
    );

    repository.record_installed_desktops();

    assert_eq!(
        repository
            .store
            .find_by_vm_id(mapping.vm_id)
            .expect("the store answers")
            .expect("the mapping is still there")
            .display_provisioning,
        DisplayProvisioning::Ready,
    );
}

#[test]
fn a_desktop_that_has_not_reported_is_left_as_it_was() {
    let (mut repository, _root) = repository_with_store();
    let mapping = VmComputeSystemMapping {
        desktop_profile: DesktopProfile::Gnome,
        display_provisioning: DisplayProvisioning::Pending,
        ..mapping_for("dev")
    };
    repository.store.insert(mapping.clone()).expect("a mapping");

    repository.record_installed_desktops();

    assert_eq!(
        repository
            .store
            .find_by_vm_id(mapping.vm_id)
            .expect("the store answers")
            .expect("the mapping is still there")
            .display_provisioning,
        DisplayProvisioning::Pending,
        "nothing observed is not evidence of an installed desktop"
    );
}
```

Reuse whichever helpers the module's tests already have for building a
repository over a temporary directory and a mapping; the two named here
(`repository_with_store`, `mapping_for`) stand for those -- read the
neighbouring tests and use their actual names rather than adding new ones.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform desktop`
Expected: FAIL -- no method `record_installed_desktops`.

- [ ] **Step 3: Implement the promotion**

In `impl HcsVmRepository`:

```rust
    /// Writes down that a VM's desktop is installed, once its guest offers it.
    ///
    /// The build cannot answer this: cloud-init installs the desktop on the
    /// first boot, long after the creation pipeline has finished, and nothing
    /// in the guest reports the package set. What does answer it is the guest
    /// running its display services on top of that desktop, which is the same
    /// fact Connect waits for -- so the first time it is observed, the stored
    /// provisioning stops saying "still installing".
    ///
    /// Stored rather than derived per refresh so that a stopped VM reads as a
    /// VM with a desktop that is not running, rather than as one whose desktop
    /// never arrived.
    fn record_installed_desktops(&self) {
        let Ok(mappings) = self.store.list() else {
            return;
        };
        for mapping in mappings {
            if !mapping.desktop_profile.wants_desktop()
                || mapping.display_provisioning == DisplayProvisioning::Ready
            {
                continue;
            }
            if !matches!(
                self.display_runs.snapshot(mapping.vm_id).guest,
                Some(GuestDisplayReport::Ready(_))
            ) {
                continue;
            }

            log::info!(
                "VM \"{}\" offers its desktop, so its desktop is installed",
                mapping.vm_name
            );
            let updated = VmComputeSystemMapping {
                display_provisioning: DisplayProvisioning::Ready,
                ..mapping
            };
            if let Err(error) = self.store.insert(updated) {
                log::warn!("the installed desktop could not be recorded: {error}");
            }
        }
    }
```

Call it from `take_diagnostics`, beside the other per-refresh reconciliations:

```rust
        // The same call for the same reason: a guest that has started offering
        // its desktop since the last refresh has an installation to record.
        self.record_installed_desktops();
```

Import `DisplayProvisioning` and `GuestDisplayReport` from `vmlord_core`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform desktop`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/repository.rs
git commit -m "TASK-121: Record a desktop as installed once its guest offers it"
```

---

### Task 3: The three display services in the compute system

**Files:**
- Modify: `crates/platform/src/hvsocket.rs` (beside `AGENT_VSOCK_PORT`)
- Modify: `crates/platform/src/hcs_config.rs` (the service table and its tests)

**Interfaces:**
- Produces: `hvsocket::DISPLAY_CONTROL_VSOCK_PORT`,
  `DISPLAY_FRAME_VSOCK_PORT`, `DISPLAY_INPUT_VSOCK_PORT` (all `pub(crate) const
  u32`), and `hvsocket::display_service_ids() -> [GUID; 3]`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/hvsocket.rs`'s `mod tests`:

```rust
#[test]
fn the_display_ports_are_the_ones_both_ends_of_the_protocol_spell() {
    // `VMLD`, `VMLF` and `VMLI` as ASCII, the way `VMLA` is the agent's. The
    // guest listens on these and the viewer connects to them, so a change
    // here is a change to two other crates.
    assert_eq!(DISPLAY_CONTROL_VSOCK_PORT, 0x564D_4C44);
    assert_eq!(DISPLAY_FRAME_VSOCK_PORT, 0x564D_4C46);
    assert_eq!(DISPLAY_INPUT_VSOCK_PORT, 0x564D_4C49);
}

#[test]
fn no_display_service_collides_with_the_agent_s() {
    let mut ids: Vec<String> = display_service_ids()
        .iter()
        .chain(std::iter::once(&agent_service_id()))
        .map(|id| format!("{id:?}"))
        .collect();
    ids.sort();
    ids.dedup();

    assert_eq!(ids.len(), 4, "four services, four distinct GUIDs");
}
```

In `crates/platform/src/hcs_config.rs`'s `mod tests`, extend the service-table
test:

```rust
    const DISPLAY_CONTROL_SERVICE_KEY: &str = "564D4C44-FACB-11E6-BD58-64006A7986D3";
    const DISPLAY_FRAME_SERVICE_KEY: &str = "564D4C46-FACB-11E6-BD58-64006A7986D3";
    const DISPLAY_INPUT_SERVICE_KEY: &str = "564D4C49-FACB-11E6-BD58-64006A7986D3";

    #[test]
    fn every_vm_lists_the_three_display_services() {
        // Listed for every VM, desktop or not: the entry is the partition's
        // permission for the service to exist, and a headless guest simply
        // never binds the ports. A VM created without them cannot be given
        // them, because a start rebuilds the compute system from this
        // document.
        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(
                &cloud_request(),
                Path::new(r"C:\vms\test-vm\disks\system.vhdx"),
                Path::new(r"C:\vms\test-vm\seed.iso"),
                None,
                &state_paths(),
                VM_ID,
            )
            .unwrap(),
        )
        .unwrap();

        let table = json
            .pointer("/VirtualMachine/Devices/HvSocket/HvSocketConfig/ServiceTable")
            .and_then(Value::as_object)
            .expect("the VM should have a service table");

        assert_eq!(table.len(), 4);
        for key in [
            DISPLAY_CONTROL_SERVICE_KEY,
            DISPLAY_FRAME_SERVICE_KEY,
            DISPLAY_INPUT_SERVICE_KEY,
        ] {
            assert_eq!(
                table[key].pointer("/BindSecurityDescriptor"),
                Some(&json!("D:P(A;;FA;;;SY)(A;;FA;;;BA)")),
                "a display service is as narrow as the agent's"
            );
        }
    }
```

The existing `assert_eq!(table.len(), 1)` in the agent's test becomes `4`, and
`builds_the_minimal_configuration`'s literal service table gains the three
keys with the same descriptor.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform hvsocket hcs_config`
Expected: FAIL -- the constants do not exist and the table holds one entry.

- [ ] **Step 3: Add the ports and the entries**

In `crates/platform/src/hvsocket.rs`, beside `AGENT_VSOCK_PORT`:

```rust
/// The vsock port a guest's display control service listens on.
///
/// `VMLD` as ASCII. Unlike the agent's port the host *connects* to this one:
/// a display session begins when a person presses Connect, so the socket's
/// lifetime is the session's. The value is the protocol's and is spelled the
/// same in `vmlord-display-services` and `vmlord-display-viewer`.
pub(crate) const DISPLAY_CONTROL_VSOCK_PORT: u32 = 0x564D_4C44;

/// The vsock port a guest's display frame service listens on -- `VMLF`.
pub(crate) const DISPLAY_FRAME_VSOCK_PORT: u32 = 0x564D_4C46;

/// The vsock port a guest's display input service listens on -- `VMLI`.
pub(crate) const DISPLAY_INPUT_VSOCK_PORT: u32 = 0x564D_4C49;

/// The three services a display session runs over, in channel order.
#[must_use]
pub(crate) fn display_service_ids() -> [GUID; 3] {
    [
        vsock_service_id(DISPLAY_CONTROL_VSOCK_PORT),
        vsock_service_id(DISPLAY_FRAME_VSOCK_PORT),
        vsock_service_id(DISPLAY_INPUT_VSOCK_PORT),
    ]
}
```

In `crates/platform/src/hcs_config.rs`, replace the single-entry table with:

```rust
                    hv_socket: HvSocket {
                        config: HvSocketConfig {
                            service_table: service_table(),
                        },
                    },
```

and add, beside `agent_service_key`:

```rust
/// Every HvSocket service a VM is given, keyed by service GUID.
///
/// The agent's, which the guest connects out on, and the display's three,
/// which the guest listens on. All four are listed for every VM: an entry is
/// the permission for a service to exist on this partition, not a claim that
/// anything inside the guest is using it.
fn service_table() -> BTreeMap<String, HvSocketService> {
    let mut table = BTreeMap::from([(agent_service_key(), HvSocketService::agent())]);
    for id in crate::hvsocket::display_service_ids() {
        table.insert(format!("{id:?}"), HvSocketService::agent());
    }
    table
}
```

Rename `HvSocketService::agent` to `HvSocketService::vmlord` -- one
constructor for one descriptor, since the display services are exactly as
narrow -- and update the doc comment on the constructor and on
`bind_security_descriptor` to say "the agent connects to, and the display
listens on".

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform hvsocket hcs_config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/hvsocket.rs crates/platform/src/hcs_config.rs
git commit -m "TASK-121: List the three display services in the VM configuration"
```

---

### Task 4: The host half of the handshake

The viewer holds the socket, VMLord holds the secret. This task is VMLord's
half as a state machine over launch-pipe messages, so that every rule in it is
tested without a process, a partition or a window.

**Files:**
- Create: `crates/platform/src/display_session.rs`
- Modify: `crates/platform/src/lib.rs` (declare the module)
- Modify: `crates/platform/Cargo.toml` (depend on `vmlord-display-viewer`)

**Interfaces:**
- Consumes: `hvsocket::DISPLAY_*_VSOCK_PORT` from Task 3.
- Produces:
  * `pub(crate) struct Driver`
  * `Driver::open(vm_name: &str, secret: Secret, runtime_id: Uuid, mode: Option<DisplayMode>) -> (Driver, LaunchParameters)`
  * `Driver::handle(&mut self, message: Message) -> Answer`
  * `pub(crate) struct Answer { to_viewer: Vec<Message>, diagnostics: Vec<Diagnostic> }`

- [ ] **Step 1: Add the dependency**

In `crates/platform/Cargo.toml`, under `[dependencies]`:

```toml
# The launch contract's private schema, which the viewer defines and this side
# speaks. Linked rather than copied: two encoders of one wire format are two
# things to keep in step.
vmlord-display-viewer = { path = "../display-viewer" }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/platform/src/display_session.rs` with only the test module for
now, so the compiler names what is missing:

```rust
#[cfg(test)]
mod tests {
    use uuid::Uuid;
    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Limits},
        session::{Event, Session, Support},
        v1::{Capability, Mode},
    };
    use vmlord_display_viewer::launch::Message;

    use super::Driver;

    fn support() -> Support {
        Support {
            capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        }
    }

    /// The bytes of one record, as the viewer relays them.
    fn framed(record: &vmlord_display_protocol::record::Record) -> Vec<u8> {
        let mut bytes = record.header.encode().to_vec();
        bytes.extend_from_slice(&record.payload);
        bytes
    }

    /// Runs a whole handshake between a driver and a guest session.
    ///
    /// Returns the hand-over the driver produced, which is the only thing the
    /// viewer needs out of VMLord.
    fn handshake(driver: &mut Driver, hello: Vec<u8>, secret: &Secret) -> Message {
        let mut guest = Session::guest(secret, support());
        let limits = Limits::new(0, 0);
        let mut to_guest = vec![hello];

        for _ in 0..8 {
            let mut from_guest = Vec::new();
            for bytes in to_guest.drain(..) {
                let mut cursor = bytes.as_slice();
                let mut payload = Vec::new();
                let header = record::read(&mut cursor, &limits, &mut payload)
                    .expect("VMLord frames whole records");
                let outcome = guest.handle(&header, &payload).expect("a valid record");
                if let Some(reply) = outcome.reply {
                    from_guest.push(framed(&reply));
                }
                if let Some(auth) = guest.pending_auth() {
                    from_guest.push(framed(&auth));
                }
            }

            for bytes in from_guest {
                let answer = driver.handle(Message::RelayFromViewer(bytes));
                for message in answer.to_viewer {
                    match message {
                        Message::RelayToViewer(bytes) => to_guest.push(bytes),
                        handover @ Message::Handover(_) => return handover,
                        other => panic!("VMLord said {other:?} during a handshake"),
                    }
                }
            }
        }

        panic!("the handshake did not finish");
    }

    #[test]
    fn a_handshake_ends_in_a_hand_over_a_viewer_can_use() {
        let secret = Secret::generate();
        let guest_secret = Secret::from_base64(&secret.to_base64()).expect("the same secret");
        let (mut driver, parameters) =
            Driver::open("dev", secret, Uuid::from_u128(7), None);

        let Message::Handover(handover) =
            handshake(&mut driver, parameters.client_hello.clone(), &guest_secret)
        else {
            panic!("a handshake ends in a hand-over");
        };

        assert_eq!(handover.session_id.len(), 16);
        assert_eq!(handover.frame_key.len(), 32);
        assert_eq!(handover.input_key.len(), 32);
        assert_eq!(handover.width, 1920);
        assert_eq!(handover.height, 1080);
        assert_eq!(handover.mode, i32::from(Mode::Desktop));
    }

    #[test]
    fn the_launch_parameters_name_the_partition_and_the_three_ports() {
        let (_driver, parameters) =
            Driver::open("dev", Secret::generate(), Uuid::from_u128(7), None);

        assert_eq!(parameters.runtime_id, *Uuid::from_u128(7).as_bytes());
        assert_eq!(parameters.control_port, 0x564D_4C44);
        assert_eq!(parameters.frame_port, 0x564D_4C46);
        assert_eq!(parameters.input_port, 0x564D_4C49);
        assert_eq!(parameters.token.len(), 32);
        assert_eq!(parameters.vm_name, "dev");
    }

    #[test]
    fn a_stored_mode_is_what_the_window_is_offered_before_the_handshake() {
        let (_driver, parameters) = Driver::open(
            "dev",
            Secret::generate(),
            Uuid::from_u128(7),
            vmlord_core::DisplayMode::new(2560, 1440),
        );

        assert_eq!((parameters.width, parameters.height), (2560, 1440));
    }

    #[test]
    fn a_request_carrying_the_right_token_opens_another_session() {
        let secret = Secret::generate();
        let (mut driver, parameters) =
            Driver::open("dev", secret, Uuid::from_u128(7), None);

        let answer = driver.handle(Message::RequestRelay {
            token: parameters.token.clone(),
        });

        let [Message::RelayToViewer(hello)] = answer.to_viewer.as_slice() else {
            panic!("a request with the right token is answered with a hello");
        };
        assert_ne!(
            *hello, parameters.client_hello,
            "a second session draws its own nonce, so its hello differs"
        );
    }

    #[test]
    fn a_request_carrying_the_wrong_token_is_refused_and_reported() {
        let (mut driver, _parameters) =
            Driver::open("dev", Secret::generate(), Uuid::from_u128(7), None);

        let answer = driver.handle(Message::RequestRelay {
            token: vec![0; 32],
        });

        assert!(answer.to_viewer.is_empty());
        assert_eq!(answer.diagnostics.len(), 1);
        assert!(
            !answer.diagnostics[0].message.contains("token"),
            "the diagnostic must not invite anybody to compare bytes"
        );
    }

    #[test]
    fn a_record_the_session_refuses_ends_the_attempt_with_a_diagnostic() {
        let (mut driver, _parameters) =
            Driver::open("dev", Secret::generate(), Uuid::from_u128(7), None);

        // A control record whose type is not what the host is waiting for.
        let answer = driver.handle(Message::RelayFromViewer(vec![0; 24]));

        assert!(answer.to_viewer.is_empty());
        assert_eq!(answer.diagnostics.len(), 1);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform display_session`
Expected: FAIL -- `Driver` does not exist.

- [ ] **Step 4: Write the driver**

Above the test module in `crates/platform/src/display_session.rs`:

```rust
//! VMLord's half of one display session.
//!
//! The viewer owns the three sockets and VMLord owns the VM's secret, so
//! neither can run the control handshake alone: the viewer frames records off
//! the wire and passes the bytes up a pipe without reading them, and what is
//! here drives the protocol's `Session` over those bytes and hands back the
//! one-shot credential the viewer needs.
//!
//! A state machine and not a thread: everything that decides anything is
//! reachable from a test with no process, no partition and no window.
//! `display_launches` is what puts a process and two pipes around it.
//!
//! Nothing here formats a secret, a token or a channel key. The master secret
//! never leaves this side, and what does cross the pipe is good for one
//! session.

use uuid::Uuid;
use vmlord_core::{Diagnostic, DiagnosticLevel, DisplayMode};
use vmlord_display_protocol::{
    keys::Secret,
    record::{self, Channel, Limits},
    session::{Event, Offer, Session},
    v1::{Capability, Mode},
};
use vmlord_display_viewer::launch::{Handover, LaunchParameters, Message};

use crate::hvsocket::{
    DISPLAY_CONTROL_VSOCK_PORT, DISPLAY_FRAME_VSOCK_PORT, DISPLAY_INPUT_VSOCK_PORT,
};

/// The size a VM with no stored mode is offered.
///
/// Only what the window opens at before the handshake settles: the viewer
/// prefers whatever it remembered for this VM, and #120's resize path replaces
/// both within a second of the desktop appearing.
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;

/// The tile size the host asks for. One of the three the guest's encoder
/// builds, and the one its benchmarks were taken at.
const TILE_SIZE: u32 = 32;

/// How many bytes prove the right to ask for another session on these pipes.
const TOKEN_LEN: usize = 32;

/// What VMLord answers one launch-pipe message with.
pub(crate) struct Answer {
    /// What to write back down the pipe, in order.
    pub(crate) to_viewer: Vec<Message>,
    /// What the rest of VMLord should be told, if anything.
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl Answer {
    fn nothing() -> Self {
        Self {
            to_viewer: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn reported(level: DiagnosticLevel, message: String) -> Self {
        Self {
            to_viewer: Vec::new(),
            diagnostics: vec![Diagnostic { level, message }],
        }
    }
}

/// The host end of one viewer's launch pipes.
pub(crate) struct Driver {
    vm_name: String,
    secret: Secret,
    offer: Offer,
    token: Vec<u8>,
    /// The handshake in progress. `None` once a hand-over has been sent: from
    /// then on the viewer owns the session, and the next thing this side may
    /// be asked for is a new one.
    session: Option<Session>,
}

impl Driver {
    /// Opens a session and returns what the viewer is to be started with.
    pub(crate) fn open(
        vm_name: &str,
        secret: Secret,
        runtime_id: Uuid,
        mode: Option<DisplayMode>,
    ) -> (Self, LaunchParameters) {
        let offer = Offer {
            // What the guest announces and what this viewer implements.
            capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
            // A host-side policy that resolves to `Desktop` until a motion
            // codec exists; the guest is the one that resolves it.
            mode: Mode::Auto,
            width: mode.map_or(DEFAULT_WIDTH, DisplayMode::width),
            height: mode.map_or(DEFAULT_HEIGHT, DisplayMode::height),
            tile_size: TILE_SIZE,
        };
        let token = vmlord_display_protocol::keys::random_bytes()[..TOKEN_LEN].to_vec();
        let (session, hello) = Session::host(&secret, offer.clone());

        let parameters = LaunchParameters {
            vm_name: vm_name.to_owned(),
            runtime_id: *runtime_id.as_bytes(),
            control_port: DISPLAY_CONTROL_VSOCK_PORT,
            frame_port: DISPLAY_FRAME_VSOCK_PORT,
            input_port: DISPLAY_INPUT_VSOCK_PORT,
            width: offer.width,
            height: offer.height,
            tile_size: offer.tile_size,
            token: token.clone(),
            client_hello: framed(&hello),
        };

        (
            Self {
                vm_name: vm_name.to_owned(),
                secret,
                offer,
                token,
                session: Some(session),
            },
            parameters,
        )
    }

    /// Answers one message from the viewer.
    pub(crate) fn handle(&mut self, message: Message) -> Answer {
        match message {
            Message::RelayFromViewer(bytes) => self.relay(&bytes),
            Message::RequestRelay { token } => self.new_session(&token),
            other => {
                // A viewer that sends what only VMLord sends is a build that
                // disagrees with this one. Logged and ignored: the revision
                // check in the launch contract catches the ordinary form.
                log::warn!(
                    "the viewer of VM \"{}\" sent {other:?}, which VMLord does not answer",
                    self.vm_name
                );
                Answer::nothing()
            }
        }
    }

    /// Feeds one relayed record into the handshake.
    fn relay(&mut self, bytes: &[u8]) -> Answer {
        let Some(session) = self.session.as_mut() else {
            log::debug!(
                "a record arrived for VM \"{}\" after its session was handed over",
                self.vm_name
            );
            return Answer::nothing();
        };

        let limits = Limits::new(0, 0);
        let mut cursor = bytes;
        let mut payload = Vec::new();
        let header = match record::read(&mut cursor, &limits, &mut payload) {
            Ok(header) => header,
            Err(error) => {
                self.session = None;
                return Answer::reported(
                    DiagnosticLevel::Error,
                    format!(
                        "The display of VM \"{}\" could not be opened: {error}",
                        self.vm_name
                    ),
                );
            }
        };

        let outcome = match session.handle(&header, &payload) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.session = None;
                return Answer::reported(
                    DiagnosticLevel::Error,
                    format!(
                        "The display of VM \"{}\" could not be opened: {error}",
                        self.vm_name
                    ),
                );
            }
        };

        let mut answer = Answer::nothing();
        if let Some(reply) = outcome.reply {
            answer.to_viewer.push(Message::RelayToViewer(framed(&reply)));
        }
        if outcome.event == Event::ControlEstablished {
            match self.hand_over() {
                Ok(handover) => {
                    let negotiated = handover.clone();
                    answer.to_viewer.push(Message::Handover(handover));
                    answer.diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Info,
                        message: format!(
                            "Display of VM \"{}\" opened at {}x{}",
                            self.vm_name, negotiated.width, negotiated.height
                        ),
                    });
                }
                Err(message) => {
                    answer.diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Error,
                        message,
                    });
                }
            }
            self.session = None;
        }

        answer
    }

    /// Builds the hand-over an established session hands the viewer.
    fn hand_over(&mut self) -> Result<Handover, String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| format!("VM \"{}\" has no display session", self.vm_name))?;
        let negotiated = session
            .negotiated()
            .ok_or_else(|| {
                format!(
                    "The display session of VM \"{}\" settled on nothing",
                    self.vm_name
                )
            })?
            .clone();
        let frame = session.derive_channel_key(Channel::Frame).ok_or_else(|| {
            format!(
                "The display session of VM \"{}\" has no frame key",
                self.vm_name
            )
        })?;
        let input = session.derive_channel_key(Channel::Input).ok_or_else(|| {
            format!(
                "The display session of VM \"{}\" has no input key",
                self.vm_name
            )
        })?;

        Ok(Handover {
            session_id: session.session_id().to_vec(),
            frame_key: frame.to_bytes().to_vec(),
            input_key: input.to_bytes().to_vec(),
            version_major: negotiated.version.major,
            version_minor: negotiated.version.minor,
            capabilities: negotiated
                .capabilities
                .iter()
                .map(|capability| i32::from(*capability))
                .collect(),
            mode: i32::from(negotiated.mode),
            width: negotiated.width,
            height: negotiated.height,
            tile_size: negotiated.tile_size,
            control_sequence: session.control_sequence(),
        })
    }

    /// Answers a viewer that lost control and wants another session.
    fn new_session(&mut self, token: &[u8]) -> Answer {
        if !constant_time_eq(token, &self.token) {
            return Answer::reported(
                DiagnosticLevel::Error,
                format!(
                    "Something asked VMLord for a display session of VM \"{}\" without the \
                     right to; it was refused",
                    self.vm_name
                ),
            );
        }

        let (session, hello) = Session::host(&self.secret, self.offer.clone());
        self.session = Some(session);
        log::info!(
            "the viewer of VM \"{}\" lost control and asked for another session",
            self.vm_name
        );

        Answer {
            to_viewer: vec![Message::RelayToViewer(framed(&hello))],
            diagnostics: Vec::new(),
        }
    }
}

/// The bytes of one record, header first.
fn framed(record: &record::Record) -> Vec<u8> {
    let mut bytes = record.header.encode().to_vec();
    bytes.extend_from_slice(&record.payload);
    bytes
}

/// Compares two byte strings without telling anybody where they differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}
```

Two details to settle against the crates rather than guessing:

* `vmlord_display_protocol::keys::random_bytes()` returns a `[u8; 32]`; if its
  signature differs, draw the token with whatever that module exposes rather
  than adding an RNG dependency to this crate.
* `DisplayMode::width`/`height` are accessors on `vmlord_core::DisplayMode`;
  use their real names.

Declare the module in `crates/platform/src/lib.rs` beside `display_runs`:

```rust
mod display_session;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform display_session`
Expected: PASS, all seven.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/Cargo.toml crates/platform/src/lib.rs crates/platform/src/display_session.rs Cargo.lock
git commit -m "TASK-121: Drive the display handshake from VMLord's side"
```

---

### Task 5: Starting a viewer and serving its pipes

**Files:**
- Create: `crates/platform/src/display_launches.rs`
- Modify: `crates/platform/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: `display_session::{Answer, Driver}` from Task 4.
- Produces:
  * `pub(crate) struct DisplayLaunches` (with `Default`)
  * `DisplayLaunches::start(&self, request: LaunchRequest<'_>) -> Result<(), RepositoryError>`
  * `pub(crate) struct LaunchRequest<'a> { vm_name: &'a str, secret: Secret, runtime_id: Uuid, mode: Option<DisplayMode>, viewer: PathBuf, diagnostics: Arc<Mutex<Vec<Diagnostic>>> }`
  * `pub(crate) fn viewer_path() -> Result<PathBuf, RepositoryError>`

- [ ] **Step 1: Write the failing tests**

Create `crates/platform/src/display_launches.rs` with its test module:

```rust
#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use uuid::Uuid;
    use vmlord_display_protocol::keys::Secret;

    use super::{DisplayLaunches, LaunchRequest};

    #[test]
    fn a_viewer_that_is_not_beside_the_application_is_refused_by_name() {
        let launches = DisplayLaunches::default();
        let diagnostics = Arc::new(Mutex::new(Vec::new()));

        let error = launches
            .start(LaunchRequest {
                vm_name: "dev",
                secret: Secret::generate(),
                runtime_id: Uuid::from_u128(7),
                mode: None,
                viewer: PathBuf::from(r"C:\nowhere\vmlord-display.exe"),
                diagnostics: Arc::clone(&diagnostics),
            })
            .expect_err("there is no viewer at that path");

        assert!(
            error.to_string().contains("vmlord-display.exe"),
            "the message must name the file that is missing: {error}"
        );
    }

    #[test]
    fn the_viewer_is_looked_for_beside_the_running_application() {
        let path = super::viewer_path().expect("this process has a path");

        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("vmlord-display.exe")
        );
        assert_eq!(
            path.parent(),
            std::env::current_exe()
                .expect("this process has a path")
                .parent(),
            "the viewer ships beside the application, as `cargo dist` puts it"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform display_launches`
Expected: FAIL -- the module has no `DisplayLaunches`.

- [ ] **Step 3: Write the launcher**

```rust
//! The viewer processes VMLord has started, and the pipes it serves them on.
//!
//! One process per Connect, and no map of open windows: the viewer answers
//! "is one open?" itself with a named mutex, and a second launch on the same
//! partition asks the first to come forward and exits. What is kept here is
//! only the threads that hold the launch pipes.
//!
//! Those threads are never joined at shutdown. A display session outliving the
//! application is the property the separate process was built for: closing
//! VMLord closes the pipes, which costs the viewer the right to ask for a
//! fresh session and nothing else, and leaves the desktop on screen.

use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use uuid::Uuid;
use vmlord_core::{Diagnostic, DiagnosticLevel, DisplayMode, RepositoryError};
use vmlord_display_protocol::keys::Secret;
use vmlord_display_viewer::launch::{Link, Message};

use crate::display_session::Driver;

/// The viewer binary, which ships beside the application.
const VIEWER: &str = "vmlord-display.exe";

/// Everything one launch needs.
pub(crate) struct LaunchRequest<'a> {
    pub(crate) vm_name: &'a str,
    pub(crate) secret: Secret,
    pub(crate) runtime_id: Uuid,
    pub(crate) mode: Option<DisplayMode>,
    pub(crate) viewer: PathBuf,
    pub(crate) diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
}

/// Where the viewer is, given where the application is.
pub(crate) fn viewer_path() -> Result<PathBuf, RepositoryError> {
    let executable = std::env::current_exe().map_err(|error| {
        RepositoryError::new(format!("VMLord cannot tell where it is running from: {error}"))
    })?;
    let directory = executable.parent().ok_or_else(|| {
        RepositoryError::new("VMLord is running from a path with no directory")
    })?;

    Ok(directory.join(VIEWER))
}

/// One viewer's thread.
struct Worker {
    vm_name: String,
    finished: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// The viewers this process has started.
#[derive(Default)]
pub(crate) struct DisplayLaunches {
    workers: Mutex<Vec<Worker>>,
}

impl DisplayLaunches {
    /// Starts a viewer and the thread that serves its pipes.
    ///
    /// Returns once the process is running and its launch parameters are
    /// written: everything after that -- the handshake, the hand-over, a
    /// session that had to be opened again -- happens on the thread and is
    /// reported through the diagnostics buffer.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] when the viewer is not where it should be, cannot
    /// be started, or its pipes cannot be written. All three are the launch
    /// failing, which is a different thing from a session that failed later.
    pub(crate) fn start(&self, request: LaunchRequest<'_>) -> Result<(), RepositoryError> {
        let mut workers = self.lock();
        join_finished(&mut workers);

        if !request.viewer.is_file() {
            return Err(RepositoryError::new(format!(
                "{} is not beside VMLord, so no display window can be opened",
                request.viewer.display()
            )));
        }

        let (mut driver, parameters) = Driver::open(
            request.vm_name,
            request.secret,
            request.runtime_id,
            request.mode,
        );
        let mut child = spawn(&request.viewer, request.vm_name)?;
        let (reader, writer) = pipes(&mut child, request.vm_name)?;

        let mut out = Link::new(std::io::empty(), BufWriter::new(writer));
        out.write(&Message::Launch(parameters)).map_err(|error| {
            RepositoryError::new(format!(
                "the display window of VM \"{}\" could not be told what to open: {error}",
                request.vm_name
            ))
        })?;

        let finished = Arc::new(AtomicBool::new(false));
        let vm_name = request.vm_name.to_owned();
        let diagnostics = Arc::clone(&request.diagnostics);
        let handle = std::thread::Builder::new()
            .name(format!("vmlord-display-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                let vm_name = vm_name.clone();
                move || {
                    let _finish = Finish(finished);
                    serve(&mut driver, reader, out, &vm_name, &diagnostics);
                    wait_for(child, &vm_name, &diagnostics);
                }
            })
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the thread serving the display window of VM \"{vm_name}\" could not be \
                     started: {error}"
                ))
            })?;

        report(
            &request.diagnostics,
            DiagnosticLevel::Info,
            format!("Opening the display of VM \"{vm_name}\""),
        );
        workers.push(Worker {
            vm_name,
            finished,
            handle: Some(handle),
        });
        Ok(())
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    fn lock(&self) -> MutexGuard<'_, Vec<Worker>> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Reads the viewer's messages and writes back what the driver answers.
fn serve<R, W>(
    driver: &mut Driver,
    reader: R,
    mut out: Link<std::io::Empty, W>,
    vm_name: &str,
    diagnostics: &Arc<Mutex<Vec<Diagnostic>>>,
) where
    R: std::io::Read,
    W: Write,
{
    let mut incoming = Link::new(reader, std::io::sink());
    loop {
        let message = match incoming.read() {
            Ok(message) => message,
            Err(error) => {
                log::info!("the launch pipe of VM \"{vm_name}\" ended: {error}");
                return;
            }
        };

        let answer = driver.handle(message);
        for diagnostic in answer.diagnostics {
            push(diagnostics, diagnostic);
        }
        for message in answer.to_viewer {
            if let Err(error) = out.write(&message) {
                log::info!("the launch pipe of VM \"{vm_name}\" could not be written: {error}");
                return;
            }
        }
    }
}

/// Waits for the viewer and reports an exit nobody asked for.
fn wait_for(mut child: Child, vm_name: &str, diagnostics: &Arc<Mutex<Vec<Diagnostic>>>) {
    match child.wait() {
        Ok(status) if status.success() => {
            log::info!("the display window of VM \"{vm_name}\" was closed");
        }
        Ok(status) => report(
            diagnostics,
            DiagnosticLevel::Error,
            format!("The display window of VM \"{vm_name}\" stopped unexpectedly ({status})"),
        ),
        Err(error) => log::warn!(
            "VMLord lost track of the display window of VM \"{vm_name}\": {error}"
        ),
    }
}

fn spawn(viewer: &Path, vm_name: &str) -> Result<Child, RepositoryError> {
    Command::new(viewer)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "the display window of VM \"{vm_name}\" could not be started: {error}"
            ));
            log::error!("{error}");
            error
        })
}

fn pipes(
    child: &mut Child,
    vm_name: &str,
) -> Result<(std::process::ChildStdout, std::process::ChildStdin), RepositoryError> {
    let reader = child.stdout.take().ok_or_else(|| {
        RepositoryError::new(format!(
            "the display window of VM \"{vm_name}\" was started without a pipe to read"
        ))
    })?;
    let writer = child.stdin.take().ok_or_else(|| {
        RepositoryError::new(format!(
            "the display window of VM \"{vm_name}\" was started without a pipe to write"
        ))
    })?;

    Ok((reader, writer))
}

fn report(diagnostics: &Arc<Mutex<Vec<Diagnostic>>>, level: DiagnosticLevel, message: String) {
    push(diagnostics, Diagnostic { level, message });
}

fn push(diagnostics: &Arc<Mutex<Vec<Diagnostic>>>, diagnostic: Diagnostic) {
    match diagnostic.level {
        DiagnosticLevel::Error => log::error!("{}", diagnostic.message),
        _ => log::info!("{}", diagnostic.message),
    }
    diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(diagnostic);
}

/// Joins and drops every worker whose viewer has gone.
fn join_finished(workers: &mut Vec<Worker>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].finished.load(Ordering::Relaxed) {
            let mut worker = workers.remove(index);
            if let Some(handle) = worker.handle.take()
                && handle.join().is_err()
            {
                log::error!(
                    "the thread serving the display window of VM \"{}\" panicked",
                    worker.vm_name
                );
            }
        } else {
            index += 1;
        }
    }
}

/// Marks a worker finished however its thread leaves.
struct Finish(Arc<AtomicBool>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
```

`Link::new` wants a reader and a writer; the two directions are two `Link`s
here because the reading half blocks while the writing half must stay usable.
If the type's generics make that awkward, keep the writer as a plain
`BufWriter` and call `launch::encode` with a four-byte length prefix directly
-- the framing is five lines and is already spelled in `launch::Link::write`.

Declare the module in `crates/platform/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform display_launches`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/display_launches.rs crates/platform/src/lib.rs
git commit -m "TASK-121: Start the display viewer and serve its launch pipes"
```

---

### Task 6: Connect in the repository

**Files:**
- Modify: `crates/platform/src/repository.rs` (a `DisplayLaunches` field, the
  `open_display` implementation and a private preflight)

**Interfaces:**
- Consumes: `DisplayLaunches`, `LaunchRequest`, `viewer_path` from Task 5;
  `DisplayRuns::snapshot` from Task 1; `HcsVmRepository::runtime_id`,
  `mapping`, `reported_state`, `require_initialized` as they already are.
- Produces: `VmRepository::open_display` on the native backend.

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/repository.rs`'s `mod tests` (follow the neighbouring
tests for how a repository over a temporary root is built):

```rust
#[test]
fn a_stopped_vm_has_no_display_to_open() {
    let (mut repository, _root) = repository_with_store();
    repository
        .store
        .insert(mapping_for("dev"))
        .expect("a mapping");

    let error = repository
        .open_display("dev")
        .expect_err("a stopped VM displays nothing");

    assert!(
        error.to_string().contains("running"),
        "the message must say what is missing: {error}"
    );
}

#[test]
fn a_headless_vm_says_it_was_created_without_a_desktop() {
    let (mut repository, _root) = repository_with_store();
    repository
        .store
        .insert(VmComputeSystemMapping {
            desktop_profile: DesktopProfile::Headless,
            ..mapping_for("dev")
        })
        .expect("a mapping");

    let error = repository
        .open_display("dev")
        .expect_err("a headless VM has no desktop");

    assert!(
        error.to_string().contains("desktop"),
        "the message must name the desktop: {error}"
    );
}

#[test]
fn a_guest_that_has_not_reported_is_not_a_guest_that_failed() {
    let (mut repository, _root) = repository_with_store();
    let mapping = VmComputeSystemMapping {
        desktop_profile: DesktopProfile::Gnome,
        display_provisioning: DisplayProvisioning::Ready,
        ..mapping_for("dev")
    };
    repository.store.insert(mapping).expect("a mapping");

    let error = repository
        .open_display("dev")
        .expect_err("nothing is running");

    assert!(
        !error.to_string().contains("failed"),
        "a guest that has said nothing has not failed: {error}"
    );
}
```

These three exercise the preflight in the order it runs. A test that reaches
the launch itself needs a partition and belongs to #128, not here.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform open_display`
Expected: FAIL -- the trait's default answers "not supported by this backend".

- [ ] **Step 3: Implement Connect**

Add the field to `HcsVmRepository`, beside `ssh_launches`:

```rust
    /// The display windows this process has opened, each with a thread on its
    /// launch pipes. Not joined at shutdown: a session outlives VMLord.
    display_launches: DisplayLaunches,
```

initialise it with `DisplayLaunches::default()` in `new`, and implement the
trait method beside `open_ssh`:

```rust
    /// Opens the native display of a running VM.
    ///
    /// Everything that can refuse does so here, where the facts are, and each
    /// reason is its own sentence: a person told "the VM is not running" and a
    /// person told "the guest has not offered its desktop yet" have different
    /// things to do next. The UI keeps the button disabled unless the display
    /// is connectable, so these answer a click that raced a refresh.
    ///
    /// What happens after the launch -- the handshake, the hand-over, a window
    /// that closed -- is the launch thread's, and is reported into the same
    /// diagnostics as everything else.
    fn open_display(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;

        let mapping = self.mapping(name)?;
        if !mapping.desktop_profile.wants_desktop() {
            return Err(refused(format!(
                "VM \"{name}\" was created without a desktop, so it has no display to open"
            )));
        }
        match &mapping.display_provisioning {
            DisplayProvisioning::Degraded(failure) => {
                return Err(refused(format!(
                    "the desktop of VM \"{name}\" is not installed: {}",
                    failure.message
                )));
            }
            DisplayProvisioning::Ready => {}
            // `Pending` and `NotRequested` beside a desktop profile both mean
            // the installation has not been seen through.
            _ => {
                return Err(refused(format!(
                    "the desktop of VM \"{name}\" has not finished installing"
                )));
            }
        }

        if !matches!(self.reported_state(&mapping)?, Some(HcsSystemState::Running)) {
            return Err(refused(format!(
                "VM \"{name}\" has to be running before its display can be opened"
            )));
        }

        let facts = self.display_runs.snapshot(mapping.vm_id);
        if let Some(failure) = &facts.failure {
            return Err(refused(format!(
                "the display of VM \"{name}\" is not working: {}",
                failure.message
            )));
        }
        match &facts.guest {
            Some(GuestDisplayReport::Ready(_)) => {}
            Some(GuestDisplayReport::Failed(failure)) => {
                return Err(refused(format!(
                    "the guest of VM \"{name}\" cannot offer its display: {}",
                    failure.message
                )));
            }
            _ => {
                return Err(refused(format!(
                    "the guest of VM \"{name}\" has not offered its display yet"
                )));
            }
        }

        let Some(runtime_id) = self.runtime_id(&mapping) else {
            return Err(refused(format!(
                "VMLord cannot tell which partition VM \"{name}\" is running as"
            )));
        };
        let vm_directory = layout::vm_directory(&self.storage_root, &mapping.vm_name)?;
        let secret = read_display_secret(&layout::agent_secret_path(&vm_directory), name)?;

        self.display_launches.start(LaunchRequest {
            vm_name: &mapping.vm_name,
            secret,
            runtime_id,
            mode: mapping.display_mode,
            viewer: display_launches::viewer_path()?,
            diagnostics: Arc::clone(&self.diagnostics),
        })
    }
```

Add the two free functions at the bottom of the file, beside the other
helpers:

```rust
/// A refusal, logged where it was decided.
fn refused(message: String) -> RepositoryError {
    let error = RepositoryError::new(message);
    log::warn!("{error}");
    error
}

/// Reads the VM's secret in the form the display protocol takes it.
///
/// The same 32 bytes the agent protocol minted and the same file: a display
/// session's keys are derived from the VM's identity, and nothing new is
/// minted or delivered for one. The text is held in `Zeroizing` on the way
/// through, and no error quotes it.
fn read_display_secret(
    path: &Path,
    vm_name: &str,
) -> Result<vmlord_display_protocol::keys::Secret, RepositoryError> {
    let text = zeroize::Zeroizing::new(std::fs::read_to_string(path).map_err(|error| {
        refused(format!(
            "the secret of VM \"{vm_name}\" could not be read from {}: {error}",
            path.display()
        ))
    })?);

    vmlord_display_protocol::keys::Secret::from_base64(&text)
        .map_err(|error| refused(format!("the secret of VM \"{vm_name}\" is unusable: {error}")))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform open_display`
Expected: PASS.

- [ ] **Step 5: Check the whole workspace still builds**

Run: `cargo test-windows`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/src/repository.rs
git commit -m "TASK-121: Open the native display from the repository"
```

---

### Task 7: Connect follows the display status

**Files:**
- Modify: `crates/ui/src/lib.rs` (`render_selected_vm`, near line 1660)

**Interfaces:**
- Consumes: `VmDisplayStatus::is_connectable` and `VmDisplayStatus::message`,
  both of which already reach `render_selected_vm`.
- Produces: nothing.

- [ ] **Step 1: Write the failing test**

In `crates/ui/src/lib.rs`'s `mod tests`:

```rust
#[test]
fn connect_is_offered_only_when_the_display_can_be_connected_to() {
    let ready = display_status(DisplayState::Ready, "The guest offers its desktop.");
    let waiting = display_status(
        DisplayState::WaitingForGuest,
        "The desktop is installed; waiting for the guest to offer it.",
    );

    assert_eq!(super::connect_offer(Some(&ready)), (true, None));
    assert_eq!(
        super::connect_offer(Some(&waiting)),
        (
            false,
            Some("The desktop is installed; waiting for the guest to offer it.")
        )
    );
    assert_eq!(
        super::connect_offer(None),
        (false, Some("The display of this VM has not been reported yet")),
        "a VM with no status yet is not a VM to offer a window on"
    );
}
```

Write the `display_status` helper in the test module if the module has none:

```rust
fn display_status(state: DisplayState, message: &str) -> VmDisplayStatus {
    VmDisplayStatus {
        state,
        stage: DisplayStage::Guest,
        code: DisplayStatusCode::GuestReady,
        running_version: None,
        available_version: None,
        message: message.to_owned(),
        guest: None,
        can_retry: false,
        observed_at: std::time::SystemTime::UNIX_EPOCH,
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test-windows -p vmlord-ui connect`
Expected: FAIL -- no function `connect_offer`.

- [ ] **Step 3: Implement the predicate and use it**

Beside the other small helpers in `crates/ui/src/lib.rs`:

```rust
/// Whether Connect is offered, and what to say when it is not.
///
/// The display's own status rather than "the VM is running": a running VM
/// whose desktop is still installing has nothing to open a window on, and the
/// sentence explaining that is the application layer's, not this one's.
fn connect_offer(status: Option<&VmDisplayStatus>) -> (bool, Option<&str>) {
    match status {
        Some(status) if status.is_connectable() => (true, None),
        Some(status) => (false, Some(status.message.as_str())),
        None => (
            false,
            Some("The display of this VM has not been reported yet"),
        ),
    }
}
```

In `render_selected_vm`, replace the Connect group:

```rust
        let (can_connect, waiting_for) = connect_offer(display_status);
        if let Some(clicked_action) = render_action_group(
            ui,
            &[(VmAction::Connect, "Connect")],
            can_connect,
            waiting_for,
        ) {
            action = Some(clicked_action);
        }
```

`is_running` stays where the other buttons use it.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test-windows -p vmlord-ui connect`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/lib.rs
git commit -m "TASK-121: Offer Connect when the display can be connected to"
```

---

### Task 8: Say what it now does

**Files:**
- Modify: `ARCHITECTURE.md` (the display protocol section near line 2850, the
  viewer section near line 2955, and the layer overview if it names the IDD
  path)

**Interfaces:** none.

- [ ] **Step 1: Replace the paragraphs that say it is not wired**

In "The display protocol", the sentence beginning "None of this is wired into
a running VM yet: Connect still opens the AppSandbox IDD window" is now false.
Replace it with what happens instead:

```markdown
Connect on the native backend opens this stack. The compute system lists all
three services beside the agent's, the repository refuses the session before
it starts one -- a VM that is not running, a desktop still installing, a guest
that has not offered its display -- and what gets past that is one viewer
process per Connect. The legacy backend still opens the AppSandbox IDD window,
and will until #129 removes it.
```

In "The native display viewer", replace "What is deliberately not there yet:
the HCS service entries and the Connect path that launches the binary (#121)."
with a paragraph describing the host half:

```markdown
VMLord's half of that crossing is `platform::display_session`: a state machine
over launch-pipe messages, holding the VM's secret and the protocol `Session`,
which answers a relayed record with the record to send back and answers an
established handshake with the hand-over. It is driven by
`platform::display_launches`, which starts the process, writes its launch
parameters and keeps a thread on its pipes -- a thread that is never joined,
because a display session outlives the application that opened it. A repeated
Connect starts a second process that finds the named mutex taken, asks the
window that is already open to come forward, and exits.
```

Add a sentence to whichever section describes the display's status model,
saying where readiness now comes from:

```markdown
Readiness is the display recipe's last stage. `SERVICES_START` is marked `Ok`
by the guest only once both units are active and the socket between them
exists, which is the same fact a viewer needs, so the host reads it out of the
report the agent already delivers rather than asking a second question. The
first time a VM's guest reports it, the stored provisioning stops saying
"still installing": nothing else on the host can observe that cloud-init
finished installing a desktop.
```

- [ ] **Step 2: Check the document reads correctly**

Run: `grep -n "not wired into a running VM\|deliberately not there yet" ARCHITECTURE.md`
Expected: no matches.

- [ ] **Step 3: Run the whole test suite**

Run: `cargo test-windows`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "TASK-121: Document the native display path"
```

---

## Self-review notes

* Spec coverage: service entries → Task 3; host handshake → Task 4; process
  launch, diagnostics and the never-joined thread → Task 5; preflight and the
  removal of the "not supported" default → Task 6; readiness → Task 1; the UI
  predicate → Task 7; documentation → Task 8. The provisioning gap is not in
  the spec's Components table and is Task 2; it is the same class of omission
  as the readiness gap the spec does cover, and Connect cannot work without it.
* Not in this plan, by the spec's own out-of-scope list: the Update-display
  button, the available-version line, removing `asb_vm_open_display`, and the
  E2E matrix.
