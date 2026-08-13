//! Builds the HCS JSON configuration document for a new compute system.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;
use uuid::Uuid;
use vmlord_core::{GpuMode, NetworkMode, RepositoryError, VmCreateRequest, VmSource};
use windows::core::GUID;

use crate::gpu_exports::GpuExports;

/// Builds HCS compute-system configuration documents from a validated
/// [`VmCreateRequest`].
pub(crate) struct HcsVmConfigBuilder;

impl HcsVmConfigBuilder {
    /// Builds the JSON configuration for `request`, attaching
    /// `system_disk_path` as the VM's boot disk and, as the second
    /// attachment, the installer ISO for a local-media source or `seed_path`
    /// for a cloud image. `tools_path`, when supplied for a cloud VM with an
    /// installed agent, becomes the third attachment.
    ///
    /// GPU configuration is not yet implemented; any mode other than `None` is
    /// rejected. Networking accepts `None` and `NetworkMode::Nat`; a NAT VM
    /// gets no adapter here, because `VmStartPipeline` writes the
    /// `NetworkAdapters` section once its endpoint exists.
    pub(crate) fn build(
        request: &VmCreateRequest,
        system_disk_path: &Path,
        seed_path: &Path,
        tools_path: Option<&Path>,
        vm_id: Uuid,
    ) -> Result<String, RepositoryError> {
        request.validate()?;

        if request.gpu_mode != GpuMode::None {
            return Err(RepositoryError::new(format!(
                "HCS configuration does not support GPU mode: {:?}",
                request.gpu_mode
            )));
        }
        ensure_supported_network_mode(request.network_mode)?;

        let mut attachments = BTreeMap::from([
            (
                "0".to_string(),
                Attachment {
                    attachment_type: "VirtualDisk",
                    path: system_disk_path.to_path_buf(),
                },
            ),
            (
                "1".to_string(),
                Attachment {
                    attachment_type: "Iso",
                    path: media_path(request, seed_path).to_path_buf(),
                },
            ),
        ]);
        if let Some(tools_path) = tools_path {
            attachments.insert(
                "2".to_string(),
                Attachment {
                    attachment_type: "Iso",
                    path: tools_path.to_path_buf(),
                },
            );
        }

        let configuration = HcsConfiguration {
            schema_version: SCHEMA_VERSION,
            owner: "VMLord",
            // `false`: VMLord opens a fresh HCS handle per operation and
            // closes it when done (see ARCHITECTURE.md); a compute system is
            // owned by its own `vmwp` worker process, not by the client that
            // started it. `true` would tear the system down -- even one that
            // was only just created and never started -- as soon as this
            // process's creating handle closes.
            should_terminate_on_last_handle_closed: false,
            virtual_machine: VirtualMachine {
                chipset: Chipset {
                    uefi: Uefi { console: "Default" },
                },
                compute_topology: ComputeTopology {
                    memory: Memory {
                        size_in_mb: request.ram_mb,
                        allow_overcommit: true,
                        enable_deferred_commit: true,
                        enable_cold_discard_hint: true,
                    },
                    processor: Processor {
                        count: request.cpu_cores,
                    },
                },
                devices: Devices {
                    scsi: Scsi {
                        primary: ScsiController { attachments },
                    },
                    com_ports: BTreeMap::from([(
                        "0".to_owned(),
                        ComPort {
                            named_pipe: com1_pipe_path(vm_id),
                        },
                    )]),
                    hv_socket: HvSocket {
                        config: HvSocketConfig {
                            service_table: BTreeMap::from([(
                                agent_service_key(),
                                HvSocketService::agent(),
                            )]),
                        },
                    },
                    keyboard: EmptyObject {},
                    mouse: EmptyObject {},
                },
                services: Services {
                    shutdown: EmptyObject {},
                    timesync: EmptyObject {},
                },
            },
        };

        serde_json::to_string(&configuration).map_err(|error| {
            RepositoryError::new(format!("failed to serialize HCS VM configuration: {error}"))
        })
    }
}

/// The key the agent's HvSocket service is listed under.
///
/// A service table is keyed by service GUID, and the agent's is derived from
/// the vsock port the guest connects to -- see `hvsocket::agent_service_id`.
/// Listing it here is what makes the service exist for this VM: without an
/// entry, the host cannot bind it and the guest's connect has nowhere to
/// arrive. That is also why a VM created before this existed cannot talk to
/// its agent until it is recreated -- `config.json` is what a start rebuilds
/// the compute system from.
fn agent_service_key() -> String {
    format!("{:?}", crate::hvsocket::agent_service_id())
}

/// Returns the named pipe the VM's first serial port is wired to.
///
/// Derived from the compute system's own identity: the endpoint has to stay the
/// same across renames and stay distinct between VMs, and the UUID is the only
/// thing about a VM that is both.
pub(crate) fn com1_pipe_path(vm_id: Uuid) -> String {
    format!(r"\\.\pipe\vmlord-{}.com1", vm_id.as_simple())
}

/// The ISO the VM boots with: the installer for local media, the seed for a
/// cloud image.
///
/// One place decides it, because two need it: the configuration document below
/// and the pipeline, which grants the VM access to the same file.
pub(crate) fn media_path<'a>(request: &'a VmCreateRequest, seed_path: &'a Path) -> &'a Path {
    match &request.source {
        VmSource::LocalMedia { path } => Path::new(path),
        VmSource::CloudImage { .. } => seed_path,
    }
}

/// Checks the network mode against what the native backend implements today.
///
/// Both entry points into the domain -- creation through
/// [`HcsVmConfigBuilder::build`] and editing through `HcsVmRepository::update_vm`
/// -- ask this, so a mode is refused in one place and with one message. The
/// message names the task that will lift the refusal: an HRESULT from HNS,
/// raised much deeper, tells the user nothing about why the mode is missing.
pub(crate) fn ensure_supported_network_mode(mode: NetworkMode) -> Result<(), RepositoryError> {
    match mode {
        NetworkMode::None | NetworkMode::Nat => Ok(()),
        other => {
            let error = RepositoryError::new(format!(
                "the HCS backend does not support network mode {other:?} yet; \
                 External and Internal networking arrive with #10"
            ));
            log::error!("{error}");
            Err(error)
        }
    }
}

/// The part of a stored configuration document VMLord lets users change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VmTopology {
    pub(crate) ram_mb: u32,
    pub(crate) cpu_cores: u32,
}

/// Reads the memory and processor topology out of a stored configuration.
pub(crate) fn read_topology(document: &str) -> Result<VmTopology, RepositoryError> {
    let configuration = parse(document)?;
    Ok(VmTopology {
        ram_mb: read_u32(&configuration, MEMORY_SIZE_POINTER)?,
        cpu_cores: read_u32(&configuration, PROCESSOR_COUNT_POINTER)?,
    })
}

/// Returns `document` with its memory and processor topology replaced.
///
/// The whole document is preserved apart from those two values: a VM's disks,
/// its attachments and its HCS identity must survive an edit unchanged, and
/// rebuilding the document from scratch would need state that only creation
/// had.
pub(crate) fn apply_topology(
    document: &str,
    topology: VmTopology,
) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    *write_target(&mut configuration, MEMORY_SIZE_POINTER)? = topology.ram_mb.into();
    *write_target(&mut configuration, PROCESSOR_COUNT_POINTER)? = topology.cpu_cores.into();

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the updated HCS VM configuration: {error}"
        ))
    })
}

/// The key HCS's `NetworkAdapters` section uses for a VM's adapter.
///
/// HCS keys each adapter by a device identifier of the caller's choosing. The
/// endpoint's own id serves: it is unique, it is stable across starts, and
/// using it means nothing further has to be remembered to find the adapter
/// again.
///
/// Both the section a start writes and the resource path a detach names are
/// built from here. A spelling that drifted between them would detach nothing
/// while HCS still reported success.
pub(crate) fn adapter_key(endpoint_id: Uuid) -> String {
    format!("{:?}", GUID::from_u128(endpoint_id.as_u128()))
}

/// Returns `document` with the VM attached to `endpoint_id` through `mac_address`.
///
/// The same point edit as [`apply_topology`], with one difference: creation
/// writes no `NetworkAdapters` section at all, so this inserts the key into
/// `Devices` instead of replacing a value that is already there. A document
/// without a `Devices` object is not one this can attach an adapter to.
///
/// The section is replaced whole rather than merged, which makes a second start
/// produce the same document as the first.
pub(crate) fn apply_network_adapter(
    document: &str,
    endpoint_id: Uuid,
    mac_address: &str,
) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let devices = write_target(&mut configuration, DEVICES_POINTER)?
        .as_object_mut()
        .ok_or_else(|| {
            let error = RepositoryError::new(format!(
                "the stored HCS configuration has no \"{DEVICES_POINTER}\" object to attach a \
                 network adapter to"
            ));
            log::error!("{error}");
            error
        })?;

    let id = adapter_key(endpoint_id);
    devices.insert(
        NETWORK_ADAPTERS_KEY.to_owned(),
        serde_json::json!({
            &id: { "EndpointId": &id, "MacAddress": mac_address }
        }),
    );

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration with its network adapter: {error}"
        ))
    })
}

/// Returns `document` without its `NetworkAdapters` section.
///
/// This is what a VM that no longer asks for a network needs: the stored
/// document describes the adapter a previous start gave it, and leaving the
/// section in place would bring the VM up on the network it just gave up.
///
/// A document that has no such section -- or no `Devices` object to hold one --
/// is returned byte for byte, so a start that changes nothing writes nothing.
pub(crate) fn remove_network_adapter(document: &str) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let removed = configuration
        .pointer_mut(DEVICES_POINTER)
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|devices| devices.remove(NETWORK_ADAPTERS_KEY));
    if removed.is_none() {
        return Ok(document.to_owned());
    }

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration without its network adapter: {error}"
        ))
    })
}

/// Returns `document` with `exports` written into its `Plan9` section.
///
/// The whole section is replaced rather than merged: the export set is
/// computed once per start and is what the VM boots with, so a leftover share
/// from a previous start is stale by definition.
/// Nothing calls this yet: a start cannot know a VM's GPU mode until the task
/// that applies HCS assignment records one, and that task is the caller. The
/// allow goes away with it.
#[allow(dead_code)]
pub(crate) fn apply_plan9_shares(
    document: &str,
    exports: &GpuExports,
) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let devices = write_target(&mut configuration, DEVICES_POINTER)?
        .as_object_mut()
        .ok_or_else(|| {
            let error = RepositoryError::new(format!(
                "the stored HCS configuration has no \"{DEVICES_POINTER}\" object to attach \
                 Plan9 shares to"
            ));
            log::error!("{error}");
            error
        })?;

    let shares: Vec<Plan9Share<'_>> = exports
        .iter()
        .map(|export| Plan9Share {
            name: export.name(),
            access_name: export.name(),
            path: export.host_path(),
            port: PLAN9_PORT,
            flags: PLAN9_FLAG_READ_ONLY,
        })
        .collect();
    let shares = serde_json::to_value(shares).map_err(|error| {
        RepositoryError::new(format!("failed to serialize the VM's Plan9 shares: {error}"))
    })?;
    devices.insert(PLAN9_KEY.to_owned(), serde_json::json!({ "Shares": shares }));

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration with its Plan9 shares: {error}"
        ))
    })
}

/// Returns `document` without its `Plan9` section.
///
/// A VM whose GPU was switched off still has the previous start's shares in
/// its stored configuration, and leaving them would hand the guest driver
/// directories it no longer asks for. A document that has no such section is
/// returned byte for byte, so a start that changes nothing writes nothing.
/// Nothing calls this yet: a start cannot know a VM's GPU mode until the task
/// that applies HCS assignment records one, and that task is the caller. The
/// allow goes away with it.
#[allow(dead_code)]
pub(crate) fn remove_plan9_shares(document: &str) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let removed = configuration
        .pointer_mut(DEVICES_POINTER)
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|devices| devices.remove(PLAN9_KEY));
    if removed.is_none() {
        return Ok(document.to_owned());
    }

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration without its Plan9 shares: {error}"
        ))
    })
}

/// One share as HCS reads it.
#[allow(dead_code)]
#[derive(Serialize)]
struct Plan9Share<'a> {
    #[serde(rename = "Name")]
    name: &'a str,
    /// What the guest passes as `aname=`; the same string as `Name`, because
    /// a second name would be one more thing for the two sides to disagree
    /// about.
    #[serde(rename = "AccessName")]
    access_name: &'a str,
    #[serde(rename = "Path")]
    path: &'a Path,
    #[serde(rename = "Port")]
    port: u32,
    #[serde(rename = "Flags")]
    flags: u32,
}

const MEMORY_SIZE_POINTER: &str = "/VirtualMachine/ComputeTopology/Memory/SizeInMB";
const PROCESSOR_COUNT_POINTER: &str = "/VirtualMachine/ComputeTopology/Processor/Count";
const DEVICES_POINTER: &str = "/VirtualMachine/Devices";
const NETWORK_ADAPTERS_KEY: &str = "NetworkAdapters";
const PLAN9_KEY: &str = "Plan9";
/// The HvSocket port the host's Plan9 server answers on, and the one the guest
/// agent connects to before it mounts.
const PLAN9_PORT: u32 = 50001;
/// Read-only. The flag values are not published in any SDK header; this is
/// what Hyper-V honours and what the AppSandbox backend passed, and read-only
/// is stated a second time by the guest's own `MS_RDONLY` mount.
const PLAN9_FLAG_READ_ONLY: u32 = 1;

fn parse(document: &str) -> Result<serde_json::Value, RepositoryError> {
    serde_json::from_str(document).map_err(|error| {
        let error = RepositoryError::new(format!(
            "the stored HCS configuration is not valid JSON: {error}"
        ));
        log::error!("{error}");
        error
    })
}

fn read_u32(configuration: &serde_json::Value, pointer: &str) -> Result<u32, RepositoryError> {
    configuration
        .pointer(pointer)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            let error = RepositoryError::new(format!(
                "the stored HCS configuration has no numeric \"{pointer}\""
            ));
            log::error!("{error}");
            error
        })
}

fn write_target<'a>(
    configuration: &'a mut serde_json::Value,
    pointer: &str,
) -> Result<&'a mut serde_json::Value, RepositoryError> {
    configuration.pointer_mut(pointer).ok_or_else(|| {
        let error = RepositoryError::new(format!(
            "the stored HCS configuration has no \"{pointer}\" to update"
        ));
        log::error!("{error}");
        error
    })
}

#[derive(Serialize)]
struct HcsConfiguration {
    #[serde(rename = "SchemaVersion")]
    schema_version: SchemaVersion,
    #[serde(rename = "Owner")]
    owner: &'static str,
    #[serde(rename = "ShouldTerminateOnLastHandleClosed")]
    should_terminate_on_last_handle_closed: bool,
    #[serde(rename = "VirtualMachine")]
    virtual_machine: VirtualMachine,
}

#[derive(Serialize)]
struct SchemaVersion {
    #[serde(rename = "Major")]
    major: u32,
    #[serde(rename = "Minor")]
    minor: u32,
}

/// The HCS schema a VMLord compute system is described in.
///
/// 2.5 rather than the 2.1 VMLord used through #63, because that is the version
/// that introduced `VirtualMachine.Services`: the integration components are
/// simply not part of the model a 2.1 document asks for, and a compute system
/// built from one is offered whatever HCS gives by default -- timesync, but no
/// shutdown channel for the guest's `hv_utils` to bind to. That is the whole of
/// bug #70.
const SCHEMA_VERSION: SchemaVersion = SchemaVersion { major: 2, minor: 5 };

#[derive(Serialize)]
struct VirtualMachine {
    #[serde(rename = "Chipset")]
    chipset: Chipset,
    #[serde(rename = "ComputeTopology")]
    compute_topology: ComputeTopology,
    #[serde(rename = "Devices")]
    devices: Devices,
    #[serde(rename = "Services")]
    services: Services,
}

/// The integration components the VM's guest is offered over VMBus.
///
/// `Shutdown` is what makes a graceful stop possible at all: the guest's
/// `hv_util` driver binds to the VMBus channel `0e0b6031-5213-4934-818b-38d90ced39db`
/// and answers `ICMSGTYPE_SHUTDOWN` with `orderly_poweroff`, but it can only
/// bind to a channel the host offers, and nothing in the guest can conjure one.
///
/// `Timesync` is named because naming any service replaces the default set, and
/// a VM that lost its clock synchronisation to gain a shutdown channel would be
/// a poor trade. Heartbeat and key-value exchange stay out until something
/// needs them: an offered channel is a guest-facing surface, not a free
/// courtesy.
#[derive(Serialize)]
struct Services {
    #[serde(rename = "Shutdown")]
    shutdown: EmptyObject,
    #[serde(rename = "Timesync")]
    timesync: EmptyObject,
}

#[derive(Serialize)]
struct Chipset {
    #[serde(rename = "Uefi")]
    uefi: Uefi,
}

#[derive(Serialize)]
struct Uefi {
    #[serde(rename = "Console")]
    console: &'static str,
}

#[derive(Serialize)]
struct ComputeTopology {
    #[serde(rename = "Memory")]
    memory: Memory,
    #[serde(rename = "Processor")]
    processor: Processor,
}

#[derive(Serialize)]
struct Memory {
    #[serde(rename = "SizeInMB")]
    size_in_mb: u32,
    #[serde(rename = "AllowOvercommit")]
    allow_overcommit: bool,
    #[serde(rename = "EnableDeferredCommit")]
    enable_deferred_commit: bool,
    #[serde(rename = "EnableColdDiscardHint")]
    enable_cold_discard_hint: bool,
}

#[derive(Serialize)]
struct Processor {
    #[serde(rename = "Count")]
    count: u32,
}

#[derive(Serialize)]
struct Devices {
    #[serde(rename = "Scsi")]
    scsi: Scsi,
    #[serde(rename = "ComPorts")]
    com_ports: BTreeMap<String, ComPort>,
    #[serde(rename = "HvSocket")]
    hv_socket: HvSocket,
    #[serde(rename = "Keyboard")]
    keyboard: EmptyObject,
    #[serde(rename = "Mouse")]
    mouse: EmptyObject,
}

#[derive(Serialize)]
struct Scsi {
    #[serde(rename = "Primary")]
    primary: ScsiController,
}

#[derive(Serialize)]
struct ScsiController {
    #[serde(rename = "Attachments")]
    attachments: BTreeMap<String, Attachment>,
}

#[derive(Serialize)]
struct Attachment {
    #[serde(rename = "Type")]
    attachment_type: &'static str,
    #[serde(rename = "Path")]
    path: PathBuf,
}

#[derive(Serialize)]
struct ComPort {
    #[serde(rename = "NamedPipe")]
    named_pipe: String,
}

#[derive(Serialize)]
struct HvSocket {
    #[serde(rename = "HvSocketConfig")]
    config: HvSocketConfig,
}

#[derive(Serialize)]
struct HvSocketConfig {
    #[serde(rename = "ServiceTable")]
    service_table: BTreeMap<String, HvSocketService>,
}

/// One HvSocket service a VM may talk on, and who may listen for it.
#[derive(Serialize)]
struct HvSocketService {
    /// Who on the host may bind this service for this VM.
    ///
    /// SDDL, and deliberately narrow: SYSTEM and the local administrators, who
    /// are the only accounts that can drive HCS in the first place. The
    /// alternative -- everyone -- would let any process on the host take the
    /// listening socket the agent is about to connect to, and while it could
    /// not answer the challenge, the guest's agent would have nowhere left to
    /// connect.
    #[serde(rename = "BindSecurityDescriptor")]
    bind_security_descriptor: &'static str,
}

impl HvSocketService {
    /// The service the guest's agent connects to.
    const fn agent() -> Self {
        Self {
            bind_security_descriptor: "D:P(A;;FA;;;SY)(A;;FA;;;BA)",
        }
    }
}

#[derive(Serialize)]
struct EmptyObject {}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};
    use uuid::Uuid;
    use vmlord_core::{
        CloudImage, GpuMode, GpuShare, NetworkMode, Password, Provisioning, SshAccess, SshPort,
        VmCreateRequest, VmSource, ubuntu,
    };

    use super::{
        HcsVmConfigBuilder, VmTopology, adapter_key, apply_network_adapter, apply_plan9_shares,
        apply_topology, com1_pipe_path, ensure_supported_network_mode, media_path, read_topology,
        remove_network_adapter, remove_plan9_shares,
    };
    use crate::gpu_exports::GpuExports;

    /// The identity a created compute system is given, fixed so that the pipe
    /// name derived from it can be written down.
    const VM_ID: Uuid = Uuid::from_u128(0x91cb_8e9a_f2a1_43b3_a682_5724_6fb8_f31d);

    fn request() -> VmCreateRequest {
        VmCreateRequest {
            name: "test-vm".into(),
            source: VmSource::LocalMedia {
                path: "C:\\images\\installer.iso".into(),
            },
            ram_mb: 512,
            disk_gb: 1,
            cpu_cores: 1,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    fn cloud_request() -> VmCreateRequest {
        VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "user".into(),
                    password: Some(Password::new("secret")),
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ..request()
        }
    }

    #[test]
    fn a_cloud_vm_with_tools_attaches_the_agent_iso_after_its_seed() {
        // The tools ISO is optional at runtime, but when it exists it belongs at
        // a stable third attachment after the system disk and seed.
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let tools_path = PathBuf::from("C:\\vms\\test-vm\\tools.iso");

        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(
                &cloud_request(),
                &system_disk_path,
                &seed_path,
                Some(&tools_path),
                VM_ID,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            json.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments"),
            Some(&json!({
                "0": { "Type": "VirtualDisk", "Path": system_disk_path },
                "1": { "Type": "Iso", "Path": seed_path },
                "2": { "Type": "Iso", "Path": tools_path }
            }))
        );
    }

    #[test]
    fn a_cloud_vms_document_carries_no_secret_of_its_provisioning() {
        let document = HcsVmConfigBuilder::build(
            &cloud_request(),
            &PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx"),
            &PathBuf::from("C:\\vms\\test-vm\\seed.iso"),
            None,
            VM_ID,
        )
        .unwrap();

        // The password travels to the guest inside the seed volume alone. Anyone
        // who can read the compute system's configuration must learn nothing.
        assert!(!document.contains("secret"), "got {document}");
        assert!(!document.contains("$6$"), "got {document}");
        assert!(!document.contains("user"), "got {document}");
    }

    #[test]
    fn the_media_a_vm_boots_is_its_installer_or_its_seed() {
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");

        assert_eq!(
            media_path(&request(), &seed_path),
            Path::new("C:\\images\\installer.iso")
        );
        assert_eq!(media_path(&cloud_request(), &seed_path), seed_path);
    }

    #[test]
    fn a_vm_exposes_com1_through_its_stable_named_pipe() {
        // The pipe name is derived from the VM's own identity rather than its
        // name: a rename must not move the endpoint a running reader is
        // attached to, and two VMs must never share one.
        let document = HcsVmConfigBuilder::build(
            &request(),
            Path::new(r"C:\vms\test-vm\disks\system.vhdx"),
            Path::new(r"C:\vms\test-vm\seed.iso"),
            None,
            VM_ID,
        )
        .unwrap();
        let json: Value = serde_json::from_str(&document).unwrap();

        assert_eq!(
            com1_pipe_path(VM_ID),
            r"\\.\pipe\vmlord-91cb8e9af2a143b3a68257246fb8f31d.com1"
        );
        assert_eq!(
            json.pointer("/VirtualMachine/Devices/ComPorts/0/NamedPipe"),
            Some(&json!(com1_pipe_path(VM_ID)))
        );
    }

    /// The agent's service GUID, spelled out rather than derived: this is the
    /// address a guest's vsock connect arrives at, and a test that computed it
    /// the way the code does would agree with any change to it.
    const AGENT_SERVICE_KEY: &str = "564D4C41-FACB-11E6-BD58-64006A7986D3";

    #[test]
    fn every_vm_is_given_the_service_its_agent_connects_to() {
        // Without the entry the host cannot bind the service and the guest's
        // agent has nowhere to connect, however well the rest of the VM works.
        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(
                &cloud_request(),
                Path::new(r"C:\vms\test-vm\disks\system.vhdx"),
                Path::new(r"C:\vms\test-vm\seed.iso"),
                None,
                VM_ID,
            )
            .unwrap(),
        )
        .unwrap();

        let table = json
            .pointer("/VirtualMachine/Devices/HvSocket/HvSocketConfig/ServiceTable")
            .and_then(Value::as_object)
            .expect("the VM should have a service table");
        assert_eq!(table.len(), 1);
        assert_eq!(
            table[AGENT_SERVICE_KEY].pointer("/BindSecurityDescriptor"),
            Some(&json!("D:P(A;;FA;;;SY)(A;;FA;;;BA)")),
            "only SYSTEM and the administrators may listen for a VM's agent"
        );
    }

    #[test]
    fn builds_the_minimal_configuration() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(&request(), &system_disk_path, &seed_path, None, VM_ID)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(
            json,
            json!({
                "SchemaVersion": { "Major": 2, "Minor": 5 },
                "Owner": "VMLord",
                "ShouldTerminateOnLastHandleClosed": false,
                "VirtualMachine": {
                    "Chipset": { "Uefi": { "Console": "Default" } },
                    "ComputeTopology": {
                        "Memory": {
                            "SizeInMB": 512,
                            "AllowOvercommit": true,
                            "EnableDeferredCommit": true,
                            "EnableColdDiscardHint": true
                        },
                        "Processor": { "Count": 1 }
                    },
                    "Devices": {
                        "Scsi": { "Primary": { "Attachments": {
                            "0": { "Type": "VirtualDisk", "Path": system_disk_path },
                            "1": { "Type": "Iso", "Path": "C:\\images\\installer.iso" }
                        }}},
                        "ComPorts": { "0": { "NamedPipe": com1_pipe_path(VM_ID) } },
                        "HvSocket": { "HvSocketConfig": { "ServiceTable": {
                            AGENT_SERVICE_KEY: {
                                "BindSecurityDescriptor": "D:P(A;;FA;;;SY)(A;;FA;;;BA)"
                            }
                        }}},
                        "Keyboard": {},
                        "Mouse": {}
                    },
                    "Services": { "Shutdown": {}, "Timesync": {} }
                }
            }),
        );
    }

    #[test]
    fn a_vm_is_offered_the_shutdown_channel_its_guest_waits_for() {
        // #70: a guest cannot ask for this. Linux' `hv_util` driver binds to the
        // VMBus channel the host offers and does nothing until it is offered
        // one, so a document that leaves `Services` out leaves the guest with
        // no way to be asked to power off -- which is what
        // `HcsShutDownComputeSystem` then reports as `ERROR_NOT_SUPPORTED`.
        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(
                &cloud_request(),
                Path::new(r"C:\vms\test-vm\disks\system.vhdx"),
                Path::new(r"C:\vms\test-vm\seed.iso"),
                None,
                VM_ID,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            json.pointer("/VirtualMachine/Services/Shutdown"),
            Some(&json!({}))
        );
        // The section exists only from schema 2.5 on; asking for an older
        // model is asking for one that has no integration components in it.
        assert_eq!(json.pointer("/SchemaVersion/Minor"), Some(&json!(5)));
    }

    #[test]
    fn naming_the_services_keeps_the_clock_the_default_set_gave() {
        // Naming any service replaces the set HCS offers by default, and
        // timesync is in that set today: a VM must not lose its clock
        // synchronisation as a side effect of gaining a shutdown channel.
        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(
                &request(),
                Path::new(r"C:\vms\test-vm\disks\system.vhdx"),
                Path::new(r"C:\vms\test-vm\seed.iso"),
                None,
                VM_ID,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            json.pointer("/VirtualMachine/Services/Timesync"),
            Some(&json!({}))
        );
    }

    #[test]
    fn serializes_ram_cpu_and_disk_path() {
        let request = VmCreateRequest {
            ram_mb: 65_536,
            cpu_cores: 64,
            ..request()
        };
        let system_disk_path = PathBuf::from("C:\\vms\\edge\\disks\\system.vhdx");

        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");

        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(&request, &system_disk_path, &seed_path, None, VM_ID)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(
            json.pointer("/VirtualMachine/ComputeTopology/Memory/SizeInMB"),
            Some(&json!(65_536))
        );
        assert_eq!(
            json.pointer("/VirtualMachine/ComputeTopology/Processor/Count"),
            Some(&json!(64))
        );
        assert_eq!(
            json.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments/0/Path"),
            Some(&json!(system_disk_path))
        );
    }

    #[test]
    fn omits_request_secrets() {
        let request = VmCreateRequest { ..request() };
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");

        let document =
            HcsVmConfigBuilder::build(&request, &system_disk_path, &seed_path, None, VM_ID)
                .unwrap();

        assert!(!document.contains("secret"));
        assert!(!document.contains("password"));
        assert!(!document.contains("username"));
        // Nor the hash the password becomes. It belongs in the seed volume,
        // which the guest reads and this document only points at; a `$6$`
        // entry here would be handed to anyone who can read the compute
        // system's configuration.
        assert!(!document.contains("$6$"));
    }

    #[test]
    fn rejects_each_unsupported_gpu_mode() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        for mode in [GpuMode::Default, GpuMode::Mirror, GpuMode::Unknown(42)] {
            let request = VmCreateRequest {
                gpu_mode: mode,
                ..request()
            };
            assert!(
                HcsVmConfigBuilder::build(&request, &system_disk_path, &seed_path, None, VM_ID)
                    .unwrap_err()
                    .to_string()
                    .contains("GPU mode")
            );
        }
    }

    #[test]
    fn accepts_nat_without_writing_a_network_adapter() {
        // Creation writes no adapter: the endpoint and its MAC only exist once
        // `VmStartPipeline` has run, so the section is the start's to write.
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let request = VmCreateRequest {
            network_mode: NetworkMode::Nat,
            ..request()
        };

        let document =
            HcsVmConfigBuilder::build(&request, &system_disk_path, &seed_path, None, VM_ID)
                .unwrap();

        let json: Value = serde_json::from_str(&document).unwrap();
        assert!(
            json.pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none()
        );
    }

    #[test]
    fn rejects_each_network_mode_that_waits_for_its_own_task() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        for mode in [
            NetworkMode::External,
            NetworkMode::Internal,
            NetworkMode::Unknown(7),
        ] {
            let request = VmCreateRequest {
                network_mode: mode,
                ..request()
            };

            let message =
                HcsVmConfigBuilder::build(&request, &system_disk_path, &seed_path, None, VM_ID)
                    .unwrap_err()
                    .to_string();

            assert!(message.contains("network mode"), "got: {message}");
            assert!(message.contains("#10"), "got: {message}");
        }
    }

    #[test]
    fn removes_the_network_adapter_section_and_nothing_else() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let created =
            HcsVmConfigBuilder::build(&request(), &system_disk_path, &seed_path, None, VM_ID)
                .unwrap();
        let attached = apply_network_adapter(
            &created,
            Uuid::from_u128(0x3f2b_0c11_5c78_4c1b_9e2f_3a8b_7d4c_6e50),
            "00-15-5D-01-02-03",
        )
        .unwrap();

        let removed = remove_network_adapter(&attached).unwrap();

        let before: Value = serde_json::from_str(&created).unwrap();
        let after: Value = serde_json::from_str(&removed).unwrap();
        assert!(
            after
                .pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none()
        );
        assert_eq!(after, before);
    }

    #[test]
    fn removing_an_absent_network_adapter_returns_the_document_unchanged() {
        // Byte-identical, not merely equivalent: `VmStartPipeline` decides
        // whether to rewrite `config.json` by comparing the two strings.
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let created =
            HcsVmConfigBuilder::build(&request(), &system_disk_path, &seed_path, None, VM_ID)
                .unwrap();

        let removed = remove_network_adapter(&created).unwrap();

        assert_eq!(removed, created);
    }

    #[test]
    fn removing_a_network_adapter_from_a_document_without_devices_changes_nothing() {
        let document = json!({ "VirtualMachine": {} }).to_string();

        let removed = remove_network_adapter(&document).unwrap();

        assert_eq!(removed, document);
    }

    #[test]
    fn removing_a_network_adapter_rejects_invalid_json() {
        let error = remove_network_adapter("not json").unwrap_err().to_string();

        assert!(error.contains("not valid JSON"), "got: {error}");
    }

    #[test]
    fn ensure_supported_network_mode_accepts_none_and_nat() {
        assert!(ensure_supported_network_mode(NetworkMode::None).is_ok());
        assert!(ensure_supported_network_mode(NetworkMode::Nat).is_ok());
    }

    #[test]
    fn rejects_an_invalid_request_before_serializing() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let request = VmCreateRequest {
            name: String::new(),
            ..request()
        };

        assert!(
            HcsVmConfigBuilder::build(&request, &system_disk_path, &seed_path, None, VM_ID)
                .is_err()
        );
    }

    #[test]
    fn reads_back_the_topology_it_built() {
        let request = VmCreateRequest {
            ram_mb: 4096,
            cpu_cores: 4,
            ..request()
        };
        let document = HcsVmConfigBuilder::build(
            &request,
            &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"),
            &PathBuf::from("C:\\vms\\test-vm\\seed.iso"),
            None,
            VM_ID,
        )
        .unwrap();

        assert_eq!(
            read_topology(&document).unwrap(),
            VmTopology {
                ram_mb: 4096,
                cpu_cores: 4
            }
        );
    }

    #[test]
    fn applying_a_topology_changes_only_memory_and_processors() {
        let system_disk_path = PathBuf::from("C:\\vms\\a\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let document =
            HcsVmConfigBuilder::build(&request(), &system_disk_path, &seed_path, None, VM_ID)
                .unwrap();

        let updated = apply_topology(
            &document,
            VmTopology {
                ram_mb: 8192,
                cpu_cores: 8,
            },
        )
        .unwrap();

        assert_eq!(
            read_topology(&updated).unwrap(),
            VmTopology {
                ram_mb: 8192,
                cpu_cores: 8
            }
        );
        let before: Value = serde_json::from_str(&document).unwrap();
        let mut after: Value = serde_json::from_str(&updated).unwrap();
        *after
            .pointer_mut("/VirtualMachine/ComputeTopology")
            .unwrap() = before
            .pointer("/VirtualMachine/ComputeTopology")
            .unwrap()
            .clone();
        assert_eq!(after, before, "nothing outside the topology may change");
        assert_eq!(
            after.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments/0/Path"),
            Some(&json!(system_disk_path))
        );
    }

    #[test]
    fn a_configuration_without_a_topology_is_rejected() {
        let document = r#"{"VirtualMachine":{}}"#;

        assert!(read_topology(document).is_err());
        assert!(
            apply_topology(
                document,
                VmTopology {
                    ram_mb: 512,
                    cpu_cores: 1
                }
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_json_is_rejected() {
        assert!(read_topology("not json").is_err());
    }

    const ENDPOINT_ID: Uuid = Uuid::from_u128(0x3f2b_0c11_5c78_4c1b_9e2f_3a8b_7d4c_6e50);
    const ENDPOINT_GUID: &str = "3F2B0C11-5C78-4C1B-9E2F-3A8B7D4C6E50";

    fn with_adapter(document: &str) -> String {
        apply_network_adapter(document, ENDPOINT_ID, "00-15-5D-01-02-03").unwrap()
    }

    #[test]
    fn attaching_an_adapter_names_the_endpoint_and_its_mac_address() {
        let document = HcsVmConfigBuilder::build(
            &request(),
            &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"),
            &PathBuf::from("C:\\vms\\test-vm\\seed.iso"),
            None,
            VM_ID,
        )
        .unwrap();

        let updated: Value = serde_json::from_str(&with_adapter(&document)).unwrap();

        assert_eq!(
            updated.pointer("/VirtualMachine/Devices/NetworkAdapters"),
            Some(&json!({
                ENDPOINT_GUID: {
                    "EndpointId": ENDPOINT_GUID,
                    "MacAddress": "00-15-5D-01-02-03"
                }
            }))
        );
    }

    #[test]
    fn attaching_an_adapter_changes_nothing_else() {
        let system_disk_path = PathBuf::from("C:\\vms\\a\\disks\\system.vhdx");
        let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");
        let document =
            HcsVmConfigBuilder::build(&request(), &system_disk_path, &seed_path, None, VM_ID)
                .unwrap();

        let before: Value = serde_json::from_str(&document).unwrap();
        let mut after: Value = serde_json::from_str(&with_adapter(&document)).unwrap();
        after
            .pointer_mut("/VirtualMachine/Devices")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("NetworkAdapters");

        assert_eq!(after, before, "nothing outside the adapter may change");
    }

    #[test]
    fn attaching_the_same_adapter_twice_yields_the_same_document() {
        // Every start rewrites the section, so a VM that has already been
        // started must not accumulate adapters or churn its configuration file.
        let document = HcsVmConfigBuilder::build(
            &request(),
            &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"),
            &PathBuf::from("C:\\vms\\test-vm\\seed.iso"),
            None,
            VM_ID,
        )
        .unwrap();

        let once = with_adapter(&document);

        assert_eq!(with_adapter(&once), once);
    }

    #[test]
    fn the_adapter_key_is_how_the_section_names_the_adapter() {
        // A detach names the adapter by this key in its resource path. A
        // spelling that drifts from the one the section uses detaches nothing
        // and still reports success, so both sides read it from here.
        let document = HcsVmConfigBuilder::build(
            &request(),
            &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"),
            &PathBuf::from("C:\\vms\\test-vm\\seed.iso"),
            None,
            VM_ID,
        )
        .unwrap();
        let updated: Value = serde_json::from_str(&with_adapter(&document)).unwrap();

        let key = adapter_key(ENDPOINT_ID);
        let adapters = updated
            .pointer("/VirtualMachine/Devices/NetworkAdapters")
            .and_then(Value::as_object)
            .unwrap();

        assert_eq!(key, ENDPOINT_GUID);
        assert_eq!(adapters.keys().collect::<Vec<_>>(), vec![&key]);
        assert_eq!(adapters[&key]["EndpointId"], key);
    }

    #[test]
    fn a_configuration_without_devices_cannot_take_an_adapter() {
        for document in [
            r#"{"VirtualMachine":{}}"#,
            r#"{"VirtualMachine":{"Devices":[]}}"#,
            "not json",
        ] {
            assert!(
                apply_network_adapter(document, ENDPOINT_ID, "00-15-5D-01-02-03").is_err(),
                "{document}"
            );
        }
    }

    fn document_with_devices() -> String {
        HcsVmConfigBuilder::build(
            &request(),
            &PathBuf::from(r"C:\vms\test-vm\disks\system.vhdx"),
            &PathBuf::from(r"C:\vms\test-vm\seed.iso"),
            None,
            VM_ID,
        )
        .expect("the configuration must build")
    }

    fn exports() -> GpuExports {
        GpuExports::for_test(vec![
            (
                GpuShare::wsl_lib(),
                PathBuf::from(r"C:\Windows\System32\lxss\lib"),
            ),
            (
                GpuShare::driver_package("nvltsi.inf_amd64_1").unwrap(),
                PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\nvltsi.inf_amd64_1"),
            ),
        ])
    }

    #[test]
    fn plan9_shares_are_written_read_only_on_the_agent_port() {
        let updated =
            apply_plan9_shares(&document_with_devices(), &exports()).expect("shares must apply");

        let value: Value = serde_json::from_str(&updated).expect("valid JSON");
        let shares = value
            .pointer("/VirtualMachine/Devices/Plan9/Shares")
            .and_then(Value::as_array)
            .expect("the Plan9 section must hold an array of shares");
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0]["Name"], "vmlord.gpu.wsl-lib");
        assert_eq!(shares[0]["AccessName"], "vmlord.gpu.wsl-lib");
        assert_eq!(shares[0]["Path"], r"C:\Windows\System32\lxss\lib");
        assert_eq!(shares[0]["Port"], 50001);
        assert_eq!(shares[0]["Flags"], 1, "1 is read-only");
        assert_eq!(shares[1]["Name"], "vmlord.gpu.drv.nvltsi.inf_amd64_1");
    }

    #[test]
    fn applying_shares_twice_replaces_rather_than_appends() {
        let document = document_with_devices();

        let once = apply_plan9_shares(&document, &exports()).expect("shares must apply");
        let twice = apply_plan9_shares(&once, &exports()).expect("shares must apply again");

        assert_eq!(once, twice, "a start that changes nothing writes nothing");
    }

    #[test]
    fn removing_shares_takes_the_whole_section() {
        let with_shares =
            apply_plan9_shares(&document_with_devices(), &exports()).expect("shares must apply");

        let without = remove_plan9_shares(&with_shares).expect("shares must be removable");

        let value: Value = serde_json::from_str(&without).expect("valid JSON");
        assert!(
            value.pointer("/VirtualMachine/Devices/Plan9").is_none(),
            "a VM whose GPU was switched off must not keep the previous start's shares"
        );
    }

    #[test]
    fn removing_shares_from_a_document_without_any_changes_nothing() {
        let document = document_with_devices();

        assert_eq!(
            remove_plan9_shares(&document).expect("nothing to remove"),
            document,
            "a document needing no change comes back byte for byte"
        );
    }
}
