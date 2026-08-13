# Host GPU-PV Capability Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Report whether this host can assign a GPU partition to a VM and whether the Linux userspace payload a guest needs is staged, from SetupAPI and the Configuration Manager alone.

**Architecture:** Result types live in `vmlord-core` (`crates/core/src/gpu.rs`) because `app` and `ui` both read them. The enumeration lives in `vmlord-platform`, split into `gpu_enumerate.rs` (the SetupAPI walk, the only `unsafe`) and `gpu_discovery.rs` (verdict assembly, the payload and service checks, the public entry point). `app` reaches it through a new defaulted `VmRepository::host_gpu_capabilities` method and never calls `platform` directly.

**Tech Stack:** Rust 2024, `windows` 0.61.3 (`Win32_Devices_DeviceAndDriverInstallation`, `Win32_Devices_Properties`, `Win32_System_SystemInformation`), existing `HcsGetServiceProperties` wrapper in `crates/platform/src/hcs.rs`.

**Spec:** `docs/superpowers/specs/2026-08-13-gpu-host-capabilities-design.md`

## Global Constraints

* **No WMI and no external processes.** Task #85 says so explicitly, and `AGENTS.md` prefers native Windows APIs over PowerShell, WMI or spawned programs. Every fact here comes from SetupAPI, the Configuration Manager, `GetSystemDirectoryW` or HCS.
* **`unsafe` only inside `crates/platform`.** Within it, the SetupAPI walk is confined to `gpu_enumerate.rs`; `gpu_discovery.rs` has none.
* **The UI holds no business logic and calls no Windows API.** Nothing in this plan touches `crates/ui`.
* **Nothing here turns GPU-PV on.** `crates/platform/src/hcs_config.rs` and `crates/platform/src/repository.rs` keep rejecting `gpu_mode != GpuMode::None`. Applying a mode is #98; exporting the DriverStore over Plan9 is #88.
* **Commit subjects are prefixed `TASK-85: `**, one branch for the task: `task-85-gpu-host-capabilities` (already created, spec already committed).
* **Verification commands:** `cargo check-windows` and `cargo test-windows`. Never prefix them with `timeout`.
* **Doc comments explain why, in the voice of the surrounding code.** `crates/core/src/gpu.rs` is the nearest model.

---

### Task 1: Capability types in the domain crate

**Files:**
- Modify: `crates/core/src/gpu.rs` (append types; extend `GpuStatusCode`)
- Modify: `crates/core/src/lib.rs:11-14` (re-exports)
- Test: `crates/core/src/gpu.rs` (new `#[cfg(test)] mod tests` at the end of the file)

**Interfaces:**
- Consumes: `GpuFailure`, `GpuStatusCode` — already in this file.
- Produces: `HostGpuCapabilities { assignment: GpuAvailability, linux_payload: GpuAvailability, adapters: Vec<HostGpuAdapter> }`; `GpuAvailability::{Available, Unavailable(GpuFailure)}` with `is_available(&self) -> bool` and `failure(&self) -> Option<&GpuFailure>`; `HostGpuAdapter { name: String, instance_id: String, interface_path: String, driver_store: Option<PathBuf>, service: Option<String> }`; `GpuStatusCode::{HostNoAdapter, HostServiceUnavailable, HostDriverStoreMissing, HostLinuxPayloadMissing}`.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/gpu.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{GpuAvailability, GpuFailure, GpuStatusCode, HostGpuCapabilities};

    #[test]
    fn host_status_codes_have_stable_strings() {
        assert_eq!(GpuStatusCode::HostNoAdapter.as_str(), "gpu-host-no-adapter");
        assert_eq!(
            GpuStatusCode::HostServiceUnavailable.as_str(),
            "gpu-host-service-unavailable"
        );
        assert_eq!(
            GpuStatusCode::HostDriverStoreMissing.as_str(),
            "gpu-host-driver-store-missing"
        );
        assert_eq!(
            GpuStatusCode::HostLinuxPayloadMissing.as_str(),
            "gpu-host-linux-payload-missing"
        );
    }

    #[test]
    fn an_unavailable_axis_keeps_the_reason_readable() {
        let capabilities = HostGpuCapabilities {
            assignment: GpuAvailability::Available,
            linux_payload: GpuAvailability::Unavailable(GpuFailure::new(
                GpuStatusCode::HostLinuxPayloadMissing,
                "no WSL payload",
            )),
            adapters: Vec::new(),
        };

        assert!(capabilities.assignment.is_available());
        assert!(!capabilities.linux_payload.is_available());
        assert_eq!(
            capabilities.linux_payload.failure().map(|failure| failure.code),
            Some(GpuStatusCode::HostLinuxPayloadMissing)
        );
        assert!(capabilities.assignment.failure().is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-core gpu::tests`
Expected: FAIL — `cannot find type HostGpuCapabilities`, `no variant HostNoAdapter`.

- [ ] **Step 3: Write the implementation**

In `crates/core/src/gpu.rs`, add `use std::path::PathBuf;` beside the existing `use std::time::SystemTime;`, add the four variants to `GpuStatusCode` (after `GuestFailed`) with their arms in `as_str`:

```rust
    /// The host has no GPU partition adapter.
    HostNoAdapter,
    /// The Host Compute Service is not answering, so nothing can be assigned
    /// to anything.
    HostServiceUnavailable,
    /// The host has adapters, but no driver package could be located for any
    /// of them.
    HostDriverStoreMissing,
    /// The Linux userspace a guest needs is not staged on this host.
    HostLinuxPayloadMissing,
```

```rust
            Self::HostNoAdapter => "gpu-host-no-adapter",
            Self::HostServiceUnavailable => "gpu-host-service-unavailable",
            Self::HostDriverStoreMissing => "gpu-host-driver-store-missing",
            Self::HostLinuxPayloadMissing => "gpu-host-linux-payload-missing",
```

Then append the types, before the test module:

```rust
/// What this host can do for GPU-PV, as far as anything can be told without
/// starting a VM.
///
/// Two axes rather than one verdict: a host with a partition adapter but no
/// WSL payload can assign a GPU that a Linux guest will not be able to render
/// on, which is a warning and not a refusal, and a single field could say only
/// one of the two things.
///
/// An adapter, a resolved driver package and a live Host Compute Service are a
/// precondition, never a guarantee: assignment is proven by assigning, which
/// needs a running compute system, and this type is read before there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostGpuCapabilities {
    /// Whether a GPU partition can be offered to a VM at all.
    pub assignment: GpuAvailability,
    /// Whether the Linux userspace a guest needs is staged on the host.
    pub linux_payload: GpuAvailability,
    /// The adapters behind the verdict, as the host reports them.
    ///
    /// Facts from device enumeration and nothing else -- no share names, no
    /// guest paths. What is exported to a guest is decided where the export is
    /// built, not here.
    pub adapters: Vec<HostGpuAdapter>,
}

/// One axis of [`HostGpuCapabilities`]: usable, or not and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuAvailability {
    Available,
    Unavailable(GpuFailure),
}

impl GpuAvailability {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Why this axis is unavailable, when it is.
    #[must_use]
    pub const fn failure(&self) -> Option<&GpuFailure> {
        match self {
            Self::Available => None,
            Self::Unavailable(failure) => Some(failure),
        }
    }
}

/// A GPU partition adapter the host presents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostGpuAdapter {
    /// What Windows calls the device.
    pub name: String,
    /// The device instance id, which identifies this adapter across reboots.
    pub instance_id: String,
    /// The device interface path a compute system would name.
    pub interface_path: String,
    /// The driver package directory in the DriverStore, when it resolved.
    ///
    /// `None` is an adapter whose INF could not be located: still a real
    /// adapter, but one with nothing to hand a guest.
    pub driver_store: Option<PathBuf>,
    /// The kernel service driving the adapter, for diagnostics.
    pub service: Option<String>,
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vmlord-core gpu::tests`
Expected: PASS.

- [ ] **Step 5: Export the new types**

In `crates/core/src/lib.rs`, extend the `pub use gpu::{...}` list so it reads:

```rust
pub use gpu::{
    GpuAssignment, GpuAvailability, GpuFailure, GpuMode, GpuStage, GpuState, GpuStatusCode,
    GuestGpuDetail, GuestGpuReport, HostGpuAdapter, HostGpuCapabilities, NativeGpuDetail,
    VmGpuFacts, VmGpuStatus,
};
```

- [ ] **Step 6: Verify the workspace still builds**

Run: `cargo check-windows`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/gpu.rs crates/core/src/lib.rs
git commit -m "TASK-85: Add host GPU capability types

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Pure helpers for the device walk

**Files:**
- Create: `crates/platform/src/gpu_enumerate.rs`
- Modify: `crates/platform/src/lib.rs` (add `mod gpu_enumerate;` in the alphabetical module list, between `mod force_stop;` and `mod guest_ready;`)
- Test: `crates/platform/src/gpu_enumerate.rs` (inline `#[cfg(test)] mod tests`)

This task adds no `unsafe` and no Windows call: it lands the two string-shaped pieces the walk needs, with tests that can run without a GPU.

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub(crate) fn driver_store_directory(inf_location: &str) -> Option<PathBuf>`; `pub(crate) fn decode_wide_property(bytes: &[u8]) -> Option<String>`.

- [ ] **Step 1: Write the failing test**

Create `crates/platform/src/gpu_enumerate.rs` containing only the doc comment and the tests:

```rust
//! Enumerating the host's GPU partition adapters through SetupAPI.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{decode_wide_property, driver_store_directory};

    fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn a_driver_store_location_becomes_its_package_directory() {
        assert_eq!(
            driver_store_directory(
                r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_1234\nv_dispi.inf"
            ),
            Some(PathBuf::from(
                r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_1234"
            ))
        );
    }

    #[test]
    fn a_location_without_a_directory_resolves_to_nothing() {
        assert_eq!(driver_store_directory("nv_dispi.inf"), None);
        assert_eq!(driver_store_directory(""), None);
    }

    #[test]
    fn a_wide_property_is_read_up_to_its_terminator() {
        let mut bytes = wide("Microsoft Virtual Render Driver");
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        assert_eq!(
            decode_wide_property(&bytes).as_deref(),
            Some("Microsoft Virtual Render Driver")
        );
    }

    #[test]
    fn an_unterminated_wide_property_is_still_read() {
        let bytes: Vec<u8> = "nvlddmkm".encode_utf16().flat_map(u16::to_le_bytes).collect();

        assert_eq!(decode_wide_property(&bytes).as_deref(), Some("nvlddmkm"));
    }

    #[test]
    fn an_empty_or_odd_length_property_is_nothing() {
        assert_eq!(decode_wide_property(&[]), None);
        assert_eq!(decode_wide_property(&wide("")), None);
        assert_eq!(decode_wide_property(&[0x41]), None);
    }
}
```

Add `mod gpu_enumerate;` to `crates/platform/src/lib.rs`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test-windows -p vmlord-platform gpu_enumerate`
Expected: FAIL — `unresolved import super::decode_wide_property`.

- [ ] **Step 3: Write the implementation**

Insert above the test module in `crates/platform/src/gpu_enumerate.rs`:

```rust
use std::path::{Path, PathBuf};

/// The driver package directory holding an INF that SetupAPI located.
///
/// `SetupGetInfDriverStoreLocationW` answers with the INF file itself; what a
/// guest is given is the directory around it, since a driver package is every
/// file in that folder and not the INF alone.
pub(crate) fn driver_store_directory(inf_location: &str) -> Option<PathBuf> {
    let parent = Path::new(inf_location).parent()?;
    (!parent.as_os_str().is_empty()).then(|| parent.to_path_buf())
}

/// Reads a `DEVPROP_TYPE_STRING` property buffer.
///
/// The buffer is UTF-16 and usually null-terminated; the terminator is not
/// guaranteed by the API contract, so text runs either to the first null or to
/// the end of the buffer. An empty string is `None`: a property that is
/// present and blank tells a reader nothing an absent one does not.
pub(crate) fn decode_wide_property(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();

    let text = String::from_utf16_lossy(&units);
    (!text.is_empty()).then_some(text)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test-windows -p vmlord-platform gpu_enumerate`
Expected: PASS, five tests.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/gpu_enumerate.rs crates/platform/src/lib.rs
git commit -m "TASK-85: Add driver package and property decoding helpers

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Verdict assembly

**Files:**
- Create: `crates/platform/src/gpu_discovery.rs`
- Modify: `crates/platform/src/lib.rs` (add `mod gpu_discovery;` between `mod force_stop;` and `mod gpu_enumerate;`)
- Test: `crates/platform/src/gpu_discovery.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `HostGpuAdapter`, `HostGpuCapabilities`, `GpuAvailability`, `GpuFailure`, `GpuStatusCode` from Task 1.
- Produces: `fn assemble(adapters: Vec<HostGpuAdapter>, service: Result<(), RepositoryError>, payload_present: bool) -> HostGpuCapabilities`.

- [ ] **Step 1: Write the failing test**

Create `crates/platform/src/gpu_discovery.rs` with the doc comment and tests:

```rust
//! Whether this host can give a VM a GPU, and whether a Linux guest could use
//! it.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use vmlord_core::{GpuStatusCode, HostGpuAdapter, RepositoryError};

    use super::assemble;

    fn adapter(driver_store: Option<&str>) -> HostGpuAdapter {
        HostGpuAdapter {
            name: "Microsoft Virtual Render Driver".to_owned(),
            instance_id: r"PCI\VEN_10DE&DEV_1234\3&11583659&0&08".to_owned(),
            interface_path: r"\\?\pci#ven_10de".to_owned(),
            driver_store: driver_store.map(PathBuf::from),
            service: Some("nvlddmkm".to_owned()),
        }
    }

    #[test]
    fn an_adapter_with_a_package_and_a_payload_is_fully_available() {
        let capabilities = assemble(vec![adapter(Some(r"C:\pkg"))], Ok(()), true);

        assert!(capabilities.assignment.is_available());
        assert!(capabilities.linux_payload.is_available());
        assert_eq!(capabilities.adapters.len(), 1);
    }

    #[test]
    fn no_adapters_makes_assignment_unavailable() {
        let capabilities = assemble(Vec::new(), Ok(()), true);

        assert_eq!(
            capabilities.assignment.failure().map(|failure| failure.code),
            Some(GpuStatusCode::HostNoAdapter)
        );
        assert!(capabilities.linux_payload.is_available());
    }

    #[test]
    fn a_dead_service_outranks_the_adapter_question() {
        let capabilities = assemble(
            Vec::new(),
            Err(RepositoryError::new("HCS is not answering")),
            true,
        );

        let failure = capabilities.assignment.failure().expect("unavailable");
        assert_eq!(failure.code, GpuStatusCode::HostServiceUnavailable);
        assert!(
            failure.message.contains("HCS is not answering"),
            "the service's own words have to survive: {}",
            failure.message
        );
    }

    #[test]
    fn adapters_without_any_package_make_assignment_unavailable() {
        let capabilities = assemble(vec![adapter(None), adapter(None)], Ok(()), true);

        assert_eq!(
            capabilities.assignment.failure().map(|failure| failure.code),
            Some(GpuStatusCode::HostDriverStoreMissing)
        );
        assert_eq!(
            capabilities.adapters.len(),
            2,
            "an unresolved adapter is still reported"
        );
    }

    #[test]
    fn one_resolved_package_is_enough_for_assignment() {
        let capabilities = assemble(vec![adapter(None), adapter(Some(r"C:\pkg"))], Ok(()), true);

        assert!(capabilities.assignment.is_available());
    }

    #[test]
    fn a_missing_payload_does_not_touch_assignment() {
        let capabilities = assemble(vec![adapter(Some(r"C:\pkg"))], Ok(()), false);

        assert!(capabilities.assignment.is_available());
        assert_eq!(
            capabilities.linux_payload.failure().map(|failure| failure.code),
            Some(GpuStatusCode::HostLinuxPayloadMissing)
        );
    }
}
```

Add `mod gpu_discovery;` to `crates/platform/src/lib.rs`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test-windows -p vmlord-platform gpu_discovery`
Expected: FAIL — `unresolved import super::assemble`.

- [ ] **Step 3: Write the implementation**

Insert above the test module:

```rust
use vmlord_core::{
    GpuAvailability, GpuFailure, GpuStatusCode, HostGpuAdapter, HostGpuCapabilities,
    RepositoryError,
};

/// Turns what was observed into the two verdicts.
///
/// Kept free of any Windows call so that every case below is a test rather
/// than a host someone has to find.
fn assemble(
    adapters: Vec<HostGpuAdapter>,
    service: Result<(), RepositoryError>,
    payload_present: bool,
) -> HostGpuCapabilities {
    let assignment = if let Err(error) = service {
        // A service that is not answering makes the adapter question moot:
        // reporting "no adapters" here would blame the wrong thing.
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostServiceUnavailable,
            format!("the Host Compute Service is not available: {error}"),
        ))
    } else if adapters.is_empty() {
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostNoAdapter,
            "this host presents no GPU partition adapter",
        ))
    } else if adapters
        .iter()
        .all(|adapter| adapter.driver_store.is_none())
    {
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostDriverStoreMissing,
            "no driver package could be located for any GPU partition adapter",
        ))
    } else {
        GpuAvailability::Available
    };

    let linux_payload = if payload_present {
        GpuAvailability::Available
    } else {
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostLinuxPayloadMissing,
            "the Linux GPU userspace is not staged on this host; install WSL",
        ))
    };

    HostGpuCapabilities {
        assignment,
        linux_payload,
        adapters,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test-windows -p vmlord-platform gpu_discovery`
Expected: PASS, six tests. `assemble` is not called outside tests yet, so expect a `dead_code` warning; Task 4 removes it.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/gpu_discovery.rs crates/platform/src/lib.rs
git commit -m "TASK-85: Assemble host GPU verdicts from observed facts

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: The device walk and the entry point

**Files:**
- Modify: `crates/platform/Cargo.toml` (three `windows` features)
- Modify: `crates/platform/src/gpu_enumerate.rs` (the SetupAPI walk)
- Modify: `crates/platform/src/gpu_discovery.rs` (`discover`, the payload check)
- Modify: `crates/platform/src/hcs.rs` (add `service_available`)
- Modify: `crates/platform/src/lib.rs` (`pub use gpu_discovery::discover_host_gpu;`)
- Test: `crates/platform/tests/gpu_discovery.rs` (new, `#[ignore]`d)

**Interfaces:**
- Consumes: `driver_store_directory`, `decode_wide_property` (Task 2); `assemble` (Task 3).
- Produces: `pub(crate) fn gpu_enumerate::partition_adapters() -> Result<Vec<HostGpuAdapter>, RepositoryError>`; `pub(crate) fn hcs::service_available() -> Result<(), RepositoryError>`; `pub fn discover_host_gpu() -> HostGpuCapabilities`.

- [ ] **Step 1: Add the Windows features**

In `crates/platform/Cargo.toml`, add to the `windows` feature list, keeping it alphabetical:

```toml
    "Win32_Devices_DeviceAndDriverInstallation",
    "Win32_Devices_Properties",
```

and, after `"Win32_System_Pipes"`:

```toml
    "Win32_System_SystemInformation",
```

`Win32_Devices_DeviceAndDriverInstallation` brings SetupAPI and the Configuration Manager, `Win32_Devices_Properties` brings the `DEVPKEY` constants, and `Win32_System_SystemInformation` brings `GetSystemDirectoryW`.

- [ ] **Step 2: Add the HCS service check**

In `crates/platform/src/hcs.rs`, beside `query_hcs_service_properties`:

```rust
/// Whether the Host Compute Service is answering right now.
///
/// The query and its parse belong together: "the service replied" and "the
/// reply was not an error" are one question, and a caller outside this module
/// has no business holding half of it.
pub(crate) fn service_available() -> Result<(), RepositoryError> {
    parse_service_result(&query_hcs_service_properties()?)
}
```

- [ ] **Step 3: Write the SetupAPI walk**

In `crates/platform/src/gpu_enumerate.rs`, above the test module, add the imports and the walk:

```rust
use windows::{
    Win32::{
        Devices::{
            DeviceAndDriverInstallation::{
                CM_Get_Device_IDW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
                MAX_DEVICE_ID_LEN, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
                SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
                SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDevicePropertyW,
                SetupGetInfDriverStoreLocationW,
            },
            Properties::{
                DEVPKEY_Device_DeviceDesc, DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_Service,
                DEVPROPTYPE,
            },
        },
        Foundation::{DEVPROPKEY, ERROR_NO_MORE_ITEMS},
    },
    core::{GUID, PCWSTR},
};

use vmlord_core::{HostGpuAdapter, RepositoryError};

use crate::error::windows_error;

/// The GPU Partition Adapter device interface class.
///
/// Not published in any SDK header: it is what Hyper-V presents partitionable
/// adapters under, and it is the same constant the AppSandbox backend used.
const GUID_GPU_PARTITION_ADAPTER: GUID = GUID::from_u128(0x064092b3_625e_43bf_9eb5_dc845897dd59);

/// Owns an `HDEVINFO` so that no error path can leak it.
struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid set returned by `SetupDiGetClassDevsW`
        // and owned solely by this wrapper, so it is destroyed exactly once.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Every GPU partition adapter present on this host.
///
/// An adapter whose driver package cannot be located is still returned: it is
/// a real device, and saying it does not exist would be a different and false
/// answer.
pub(crate) fn partition_adapters() -> Result<Vec<HostGpuAdapter>, RepositoryError> {
    // SAFETY: The GUID is a valid interface class; a null enumerator and a
    // null parent window request every present device of that class.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_GPU_PARTITION_ADAPTER),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }
    .map_err(|error| windows_error("enumerate GPU partition adapters", None, error))?;
    let set = DeviceInfoSet(set);

    let mut adapters = Vec::new();
    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).unwrap_or_default(),
            ..Default::default()
        };

        // SAFETY: `set.0` is live, and `interface` is a correctly sized
        // structure this call fills in.
        let enumerated = unsafe {
            SetupDiEnumDeviceInterfaces(
                set.0,
                None,
                &GUID_GPU_PARTITION_ADAPTER,
                index,
                &raw mut interface,
            )
        };
        match enumerated {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() => break,
            Err(error) => {
                return Err(windows_error(
                    "enumerate a GPU partition adapter interface",
                    None,
                    error,
                ));
            }
        }

        let mut device = SP_DEVINFO_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).unwrap_or_default(),
            ..Default::default()
        };
        let Some(interface_path) = interface_detail(&set, &interface, &mut device)? else {
            continue;
        };

        adapters.push(HostGpuAdapter {
            name: device_property(&set, &device, &DEVPKEY_Device_DeviceDesc)
                .unwrap_or_else(|| "GPU partition adapter".to_owned()),
            instance_id: device_instance_id(&device)?,
            interface_path,
            driver_store: device_property(&set, &device, &DEVPKEY_Device_DriverInfPath)
                .and_then(|inf| driver_store_location(&inf))
                .as_deref()
                .and_then(driver_store_directory),
            service: device_property(&set, &device, &DEVPKEY_Device_Service),
        });
    }

    Ok(adapters)
}

/// The device path of one interface, and the device behind it.
///
/// The detail structure is variable length -- a fixed header followed by as
/// many characters as the path needs -- so the buffer is sized by the API
/// itself rather than guessed at.
fn interface_detail(
    set: &DeviceInfoSet,
    interface: &SP_DEVICE_INTERFACE_DATA,
    device: &mut SP_DEVINFO_DATA,
) -> Result<Option<String>, RepositoryError> {
    let mut required = 0_u32;
    // SAFETY: Passing no buffer asks only for the size; the call is expected
    // to fail with ERROR_INSUFFICIENT_BUFFER and to fill `required`.
    let _ = unsafe {
        SetupDiGetDeviceInterfaceDetailW(set.0, interface, None, 0, Some(&raw mut required), None)
    };
    if required == 0 {
        return Ok(None);
    }

    let mut buffer = vec![0_u8; required as usize];
    let detail = buffer
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    // SAFETY: `buffer` is at least `required` bytes, which is what the call
    // above asked for. `cbSize` is the size of the fixed header, not of the
    // buffer -- SetupAPI demands exactly that.
    unsafe {
        (*detail).cbSize = u32::try_from(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>())
            .unwrap_or_default();
        SetupDiGetDeviceInterfaceDetailW(
            set.0,
            interface,
            Some(detail),
            required,
            None,
            Some(&raw mut *device),
        )
    }
    .map_err(|error| windows_error("read a GPU adapter interface path", None, error))?;

    // SAFETY: `DevicePath` is a null-terminated UTF-16 string that runs to the
    // end of the buffer the call just filled.
    let path = unsafe { PCWSTR::from_raw((&raw const (*detail).DevicePath).cast()).to_string() }
        .map_err(|error| {
            RepositoryError::new(format!("a GPU adapter interface path was not UTF-16: {error}"))
        })?;

    Ok(Some(path))
}

/// One string property of a device, or `None` when it is absent.
///
/// A missing property is not an error: these are diagnostics, and an adapter
/// that does not name its service is still an adapter.
fn device_property(
    set: &DeviceInfoSet,
    device: &SP_DEVINFO_DATA,
    key: &DEVPROPKEY,
) -> Option<String> {
    let mut property_type = DEVPROPTYPE::default();
    let mut required = 0_u32;
    // SAFETY: Passing no buffer asks only for the size.
    let _ = unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            device,
            key,
            &raw mut property_type,
            None,
            Some(&raw mut required),
            0,
        )
    };
    if required == 0 {
        return None;
    }

    let mut buffer = vec![0_u8; required as usize];
    // SAFETY: `buffer` is exactly the size the call above asked for.
    unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            device,
            key,
            &raw mut property_type,
            Some(&mut buffer),
            None,
            0,
        )
    }
    .ok()?;

    decode_wide_property(&buffer)
}

/// The device instance id, which is what names an adapter across reboots.
fn device_instance_id(device: &SP_DEVINFO_DATA) -> Result<String, RepositoryError> {
    let mut buffer = [0_u16; MAX_DEVICE_ID_LEN as usize + 1];
    // SAFETY: `buffer` is at least `MAX_DEVICE_ID_LEN` characters, which is
    // the documented maximum for a device instance id.
    let result = unsafe { CM_Get_Device_IDW(device.DevInst, &mut buffer, 0) };
    if result != CR_SUCCESS {
        return Err(RepositoryError::new(format!(
            "reading a GPU adapter instance id failed (CONFIGRET {})",
            result.0
        )));
    }

    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

/// Where the DriverStore keeps the INF a device was installed from.
fn driver_store_location(inf_name: &str) -> Option<String> {
    let name: Vec<u16> = inf_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0_u16; 512];
    // SAFETY: `name` is null-terminated, and `buffer` is passed as a sized
    // slice the call fills in.
    unsafe {
        SetupGetInfDriverStoreLocationW(
            PCWSTR::from_raw(name.as_ptr()),
            None,
            PCWSTR::null(),
            &mut buffer,
            None,
        )
    }
    .ok()?;

    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
    (end > 0).then(|| String::from_utf16_lossy(&buffer[..end]))
}
```

- [ ] **Step 4: Write the entry point**

In `crates/platform/src/gpu_discovery.rs`, add above `assemble`:

```rust
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::{gpu_enumerate::partition_adapters, hcs};
```

and below it:

```rust
/// What this host can do for GPU-PV, right now.
///
/// Not a `Result`: "GPU-PV is unavailable here" is an answer rather than a
/// failure, and a Windows error on the way to it is one of the reasons an axis
/// can be unavailable. Nothing is cached -- the enumeration is cheap, and a
/// driver update or a WSL install changes the answer with nothing to
/// invalidate a cache on.
#[must_use]
pub fn discover_host_gpu() -> HostGpuCapabilities {
    let service = hcs::service_available();
    let adapters = match partition_adapters() {
        Ok(adapters) => adapters,
        Err(error) => {
            log::warn!("enumerating GPU partition adapters failed: {error}");
            Vec::new()
        }
    };

    assemble(adapters, service, linux_payload_present())
}

/// Whether the Linux GPU userspace WSL stages is on this host.
///
/// Only the verdict is reported. The path is what an export is built from, and
/// that is decided where the export is built.
fn linux_payload_present() -> bool {
    let mut buffer = [0_u16; 260];
    // SAFETY: `buffer` is passed as a sized slice; a zero return means the
    // call did not fill it.
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length > buffer.len() {
        return false;
    }

    let system32 = PathBuf::from(String::from_utf16_lossy(&buffer[..length]));
    system32.join("lxss").join("lib").is_dir()
}
```

Add `use std::path::PathBuf;` at the top of the file. `assemble` loses its `dead_code` warning here.

- [ ] **Step 5: Export the entry point**

In `crates/platform/src/lib.rs`, in the alphabetical `pub use` list, after the `pub use force_stop::VmForceStopPipeline;` line:

```rust
pub use gpu_discovery::discover_host_gpu;
```

- [ ] **Step 6: Write the real-host test**

Create `crates/platform/tests/gpu_discovery.rs`:

```rust
//! Host GPU-PV discovery against the real host.
//!
//! `#[ignore]`d: what it reports depends on the machine it runs on, so it can
//! assert that the walk is sound and self-consistent but never that GPU-PV
//! exists. A test that demanded an adapter would be permanently red on a host
//! without one, and one that demanded none would be red on a host with one.
//!
//! Run it with:
//!
//! ```text
//! cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu --test gpu_discovery -- --ignored --nocapture
//! ```

use vmlord_platform::discover_host_gpu;

#[test]
#[ignore = "reads the real host's devices"]
fn discovery_reports_a_self_consistent_picture() {
    let capabilities = discover_host_gpu();
    println!("{capabilities:#?}");

    for adapter in &capabilities.adapters {
        assert!(!adapter.name.is_empty(), "an adapter must be named");
        assert!(
            !adapter.instance_id.is_empty(),
            "an adapter must have an instance id"
        );
        assert!(
            !adapter.interface_path.is_empty(),
            "an adapter must have an interface path"
        );
        if let Some(driver_store) = &adapter.driver_store {
            assert!(
                driver_store.is_dir(),
                "a resolved driver package must be a directory: {}",
                driver_store.display()
            );
        }
    }

    if capabilities.assignment.is_available() {
        assert!(
            capabilities
                .adapters
                .iter()
                .any(|adapter| adapter.driver_store.is_some()),
            "assignment cannot be available with no resolved driver package"
        );
    }
}
```

- [ ] **Step 7: Verify**

Run: `cargo check-windows`
Expected: no errors, no warnings about unused imports or dead code.

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS; the new integration test reports as ignored.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/Cargo.toml crates/platform/src/gpu_enumerate.rs \
  crates/platform/src/gpu_discovery.rs crates/platform/src/hcs.rs \
  crates/platform/src/lib.rs crates/platform/tests/gpu_discovery.rs
git commit -m "TASK-85: Discover GPU partition adapters through SetupAPI

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The repository method and the architecture note

**Files:**
- Modify: `crates/core/src/lib.rs` (the `VmRepository` trait, beside `open_console`)
- Modify: `crates/platform/src/repository.rs` (inside `impl VmRepository for HcsVmRepository`)
- Modify: `ARCHITECTURE.md`
- Test: `crates/core/src/lib.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `HostGpuCapabilities` (Task 1); `discover_host_gpu` (Task 4).
- Produces: `VmRepository::host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError>`, defaulted to an error.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` at the end of `crates/core/src/lib.rs`:

```rust
    #[test]
    fn a_backend_that_cannot_inspect_the_host_says_so() {
        struct SilentBackend;

        impl VmRepository for SilentBackend {
            fn initialize(&mut self) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn create_vm(&mut self, _request: VmCreateRequest) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn update_vm(&mut self, _request: VmUpdateRequest) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn start_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn force_stop_vm(&mut self, _name: &str) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn delete_vm(&mut self, _request: VmDeleteRequest) -> Result<(), RepositoryError> {
                Ok(())
            }
            fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
                Ok(Vec::new())
            }
            fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
                Vec::new()
            }
        }

        let error = SilentBackend
            .host_gpu_capabilities()
            .expect_err("the default must not claim to know the host");

        assert!(
            error.to_string().contains("not supported by this backend"),
            "a backend that cannot answer has to say so rather than report an empty host: {error}"
        );
    }
```

Extend that module's `use super::{...}` line with `Diagnostic`, `RepositoryError`, `VmDeleteRequest`, `VmRepository`, `VmSummary`, `VmUpdateRequest` -- whichever of them it does not already import.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-core a_backend_that_cannot_inspect_the_host_says_so`
Expected: FAIL — `no method named host_gpu_capabilities`.

- [ ] **Step 3: Add the trait method**

In `crates/core/src/lib.rs`, after `open_console` and before `list_vms`:

```rust
    /// What the host can do for GPU-PV, as far as it can be told without
    /// starting a VM.
    ///
    /// Defaulted rather than required, and an error rather than an empty
    /// report: a backend that cannot inspect the host does not thereby know
    /// the host has nothing. "This backend cannot tell you" and "this host
    /// cannot do it" are different answers and a reader has to be able to tell
    /// them apart.
    fn host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError> {
        Err(RepositoryError::new(
            "host GPU capabilities are not supported by this backend",
        ))
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vmlord-core a_backend_that_cannot_inspect_the_host_says_so`
Expected: PASS.

- [ ] **Step 5: Implement it on the native backend**

In `crates/platform/src/repository.rs`, inside `impl VmRepository for HcsVmRepository`, beside the other read-only methods:

```rust
    /// Reads the host afresh on every call: see [`discover_host_gpu`].
    fn host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError> {
        Ok(crate::gpu_discovery::discover_host_gpu())
    }
```

Add `HostGpuCapabilities` to the file's `use vmlord_core::{...}` import list.

- [ ] **Step 6: Verify the workspace**

Run: `cargo check-windows`
Expected: no errors.

Run: `cargo test-windows`
Expected: PASS.

- [ ] **Step 7: Update ARCHITECTURE.md**

In the section describing `crates/platform`, add a paragraph in the document's own voice covering: `gpu_enumerate` walks the GPU Partition Adapter interface class through SetupAPI and the Configuration Manager -- no WMI, no spawned process -- and resolves each adapter's driver package with `SetupGetInfDriverStoreLocationW`; `gpu_discovery` turns that, plus a `System32\lxss\lib` check and an HCS service query, into the two independent verdicts of `HostGpuCapabilities`; and that an adapter, a resolved driver package and a live HCS service are a precondition for GPU-PV rather than a guarantee of it, since assignment is only proven by a start. Mention that `app` reads this through `VmRepository::host_gpu_capabilities` and that the legacy backend inherits the default error.

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/lib.rs crates/platform/src/repository.rs ARCHITECTURE.md
git commit -m "TASK-85: Expose host GPU capabilities through VmRepository

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Done when

* `cargo check-windows` and `cargo test-windows` both pass.
* `crates/ui` is untouched, and no `unsafe` was added outside `crates/platform`.
* `hcs_config.rs` and `repository.rs` still reject `gpu_mode != GpuMode::None` — this task discovers, it does not enable.
* No MR is opened. `AGENTS.md` requires explicit approval first.
