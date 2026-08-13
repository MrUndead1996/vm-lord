# Discovering host GPU-PV capabilities (#85)

Before a VM can be offered a GPU, someone has to answer whether this host can
give it one. The answer has two independent halves: whether an adapter can be
assigned to a compute system at all, and whether the Linux userspace a guest
needs is staged on the host. This task answers both, from SetupAPI and the
Configuration Manager, with no WMI and no external processes.

Scope is discovery. Exporting the DriverStore over Plan9 is #88, applying a
mode to a running VM and showing per-VM status is #98, and both stay blocked
until this reports. `hcs_config` and the native repository continue to reject
any `gpu_mode` other than `None`; nothing here turns GPU-PV on.

## Decisions

Three questions were settled before design, and each closes an alternative:

* **The HCS half is a precondition check, not a proof.** Assignment can only be
  proven by assigning: `HcsModifyComputeSystem` needs a live compute-system
  handle, and the whole point of the epic is that GPU is applied after start.
  So discovery calls the existing `HcsGetServiceProperties` wrapper and asks
  only "is the service answering". Not skipped -- it is the one cheap signal
  that separates "no GPU-PV here" from "the virtualization stack is not up".
  Not replaced with a Windows build-number table either: such a table goes
  stale and then lies in both directions. Every type and doc comment says
  plainly that an adapter, a resolved DriverStore and a live HCS are a
  precondition, and that the guarantee comes from a start.
* **Verdict plus inventory.** Two independent availability fields answer the
  question the task asks, and a list of adapters carries the facts behind them:
  name, instance id, interface path, DriverStore directory, kernel service.
  These are SetupAPI facts and nothing more -- no Plan9 share names, no guest
  paths, no file filters. Those belong to #88's typed export descriptors, which
  canonicalize and allowlist what they expose; putting a share name here would
  make the diagnostic type the export contract by accident.
* **No caching.** Discovery runs on every request. The enumeration is cheap --
  the expensive path in AppSandbox was its WMI DriverStore resolver, which is
  exactly what is not being ported -- while host state changes under driver
  updates and WSL installs with nothing to invalidate a cache on. This mirrors
  `VmGpuStatus`, which is derived per refresh rather than stored.

## The types

`crates/core/src/gpu.rs` gains them, beside `GpuMode`, because both `app` and
`ui` read them and neither may reach into `platform`.

```rust
pub struct HostGpuCapabilities {
    pub assignment: GpuAvailability,
    pub linux_payload: GpuAvailability,
    pub adapters: Vec<HostGpuAdapter>,
}

pub enum GpuAvailability {
    Available,
    Unavailable(GpuFailure),
}

pub struct HostGpuAdapter {
    pub name: String,
    pub instance_id: String,
    pub interface_path: String,
    pub driver_store: Option<PathBuf>,
    pub service: Option<String>,
}
```

Two fields rather than one verdict: a host with a partition adapter but no WSL
is assignable and unusable by a Linux guest, which is a warning and not a
refusal, and one enum could not say both. `GpuFailure` is the pair from #84, so
a reason here reads the same way a per-VM failure does.

`GpuStatusCode` grows four variants -- `HostNoAdapter`,
`HostServiceUnavailable`, `HostDriverStoreMissing`, `HostLinuxPayloadMissing`,
with the kebab strings `gpu-host-no-adapter`, `gpu-host-service-unavailable`,
`gpu-host-driver-store-missing`, `gpu-host-linux-payload-missing`. A second
enum of reasons is not introduced: the existing code was declared stable for
exactly this use, and two parallel vocabularies drift apart.

`driver_store` is `Option` because an adapter whose INF cannot be resolved is
still a real adapter. One such adapter does not make the host unassignable --
`assignment` is `Unavailable(HostDriverStoreMissing)` only when adapters exist
and not one of them resolved. What it does mean is that #88 has nothing to
export for that adapter, which is why the field is per adapter and not a single
host-wide path.

`linux_payload` reports a verdict and not the path it checked; the path is
#88's to publish.

## Discovery

`crates/platform/src/gpu_discovery.rs`, one public function, with the SetupAPI
walk itself in `crates/platform/src/gpu_enumerate.rs` so that the `unsafe` sits
in one file and the verdicts in another:

```rust
pub fn discover_host_gpu() -> HostGpuCapabilities
```

Not a `Result`. "GPU-PV is unavailable here" is an answer, not a failure, and a
Windows API error on the way to it becomes an `Unavailable` carrying the text
`windows_error` already formats. The only outcome a caller has to handle is a
verdict.

The sequence, entirely SetupAPI and the Configuration Manager:

1. `SetupDiGetClassDevsW` for the GPU Partition Adapter interface class with
   `DIGCF_PRESENT | DIGCF_DEVICEINTERFACE`. The GUID
   `{064092b3-625e-43bf-9eb5-dc845897dd59}` is a module constant with a comment
   that it is not published in the SDK and comes from observed Hyper-V
   behaviour -- the same constant AppSandbox used. The handle is wrapped in a
   guard whose `Drop` calls `SetupDiDestroyDeviceInfoList`, so the error paths
   below cannot leak it.
2. `SetupDiEnumDeviceInterfaces` in a loop, each interface passed to
   `SetupDiGetDeviceInterfaceDetailW` twice: once for the required size, once
   for the data. A fixed stack buffer is what the C did and it is a truncation
   waiting for a long device path.
3. Per-device properties through `SetupDiGetDevicePropertyW`:
   `DEVPKEY_Device_DeviceDesc` for the name, `DEVPKEY_Device_Service` for the
   kernel service, `DEVPKEY_Device_DriverInfPath` for the INF. This replaces
   AppSandbox's `SetupDiOpenDevRegKey` plus `RegQueryValueExW("InfPath")`: same
   value, no registry key to open and close, and no `Win32_System_Registry`
   feature to add.
4. `CM_Get_Device_IDW` for the instance id.
5. `SetupGetInfDriverStoreLocationW` on the INF name, then the parent directory
   of the returned file -- the `FileRepository\<folder>` directory. This is the
   path AppSandbox's own enumeration used; its WMI branch was a slower second
   route to the same answer and is not ported.
6. `GetSystemDirectoryW` plus `\lxss\lib`, tested for existence as a directory,
   for `linux_payload`.
7. A new `pub(crate) fn hcs::service_available()` that queries the service
   properties and parses the result, so the two halves of "is HCS answering"
   stay together in the module that owns both and neither is exported alone.

Verdicts follow from the above: no adapters gives `HostNoAdapter`; an HCS query
that errors gives `HostServiceUnavailable` and takes precedence, since a dead
service makes the adapter question moot; adapters with no resolved DriverStore
give `HostDriverStoreMissing`; a missing `lxss\lib` gives
`HostLinuxPayloadMissing` on the payload axis alone.

`Win32_Devices_DeviceAndDriverInstallation`, `Win32_Devices_Properties` and
`Win32_System_SystemInformation` join the `windows` feature list in
`crates/platform/Cargo.toml` -- SetupAPI and the Configuration Manager, the
`DEVPKEY` constants, and `GetSystemDirectoryW` respectively. All three were
verified present in 0.61.3.

## Reaching it from the application

`VmRepository` gains a defaulted method, the way `cancel_create` and
`open_console` are defaulted:

```rust
fn host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError> {
    Err(RepositoryError::new(
        "host GPU capabilities are not supported by this backend",
    ))
}
```

The default errors rather than reporting everything unavailable. The legacy
backend does not know what the host has, and answering "no adapters" would be a
claim it cannot make; the UI has to be able to distinguish "this backend cannot
tell you" from "this host cannot do it". The native repository implements the
method as `Ok(gpu_discovery::discover_host_gpu())`.

`app` calls it through the trait, never `platform` directly, and `ui` only
displays what `app` hands it -- no Windows API above this crate, and no
business logic in the UI.

## Testing

The logic is separated from the `unsafe` walk so that most of it is ordinary
unit-testable code:

* Verdict assembly from a list of adapters plus the payload and service flags:
  no adapters; adapters with no resolved DriverStore; adapters with some
  resolved; a failing service check; a missing `lxss\lib` beside a healthy
  assignment. Each case asserts the `GpuStatusCode` on both axes.
* INF path to DriverStore directory, and decoding UTF-16 property buffers:
  empty value, missing terminator, unexpected property type.
* A single `#[ignore]`d test in `crates/platform/tests/`, following
  `tests/import.rs`: `discover_host_gpu()` runs on a real host without panicking, and
  when `assignment` is `Available` every adapter has a non-empty name and
  instance id. It cannot assert that GPU-PV exists -- on a host without one
  that test would be permanently failing or falsely green.

Verified with `cargo check-windows` and `cargo test-windows`.

`ARCHITECTURE.md` gains the discovery path and the statement that an adapter, a
resolved DriverStore and a live HCS service are a precondition for GPU-PV
rather than a guarantee of it.
