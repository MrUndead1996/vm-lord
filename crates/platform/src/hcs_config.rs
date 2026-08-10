//! Builds the HCS JSON configuration document for a new compute system.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;
use uuid::Uuid;
use vmlord_core::{GpuMode, NetworkMode, RepositoryError, VmCreateRequest, VmSource};
use windows::core::GUID;

/// Builds HCS compute-system configuration documents from a validated
/// [`VmCreateRequest`].
pub(crate) struct HcsVmConfigBuilder;

impl HcsVmConfigBuilder {
    /// Builds the JSON configuration for `request`, attaching
    /// `system_disk_path` as the VM's boot disk and the installer ISO of its
    /// source as the second attachment.
    ///
    /// GPU configuration is not yet implemented; any mode other than `None` is
    /// rejected. Networking accepts `None` and `NetworkMode::Nat`; a NAT VM
    /// gets no adapter here, because `VmStartPipeline` writes the
    /// `NetworkAdapters` section once its endpoint exists.
    pub(crate) fn build(
        request: &VmCreateRequest,
        system_disk_path: &Path,
    ) -> Result<String, RepositoryError> {
        request.validate()?;

        if request.gpu_mode != GpuMode::None {
            return Err(RepositoryError::new(format!(
                "HCS configuration does not support GPU mode: {:?}",
                request.gpu_mode
            )));
        }
        ensure_supported_network_mode(request.network_mode)?;
        let image_path = local_media_path(request)?;

        let attachments = BTreeMap::from([
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
                    path: PathBuf::from(image_path),
                },
            ),
        ]);

        let configuration = HcsConfiguration {
            schema_version: SchemaVersion { major: 2, minor: 1 },
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
                    hv_socket: HvSocket {
                        config: HvSocketConfig {
                            service_table: BTreeMap::new(),
                        },
                    },
                    keyboard: EmptyObject {},
                    mouse: EmptyObject {},
                },
            },
        };

        serde_json::to_string(&configuration).map_err(|error| {
            RepositoryError::new(format!("failed to serialize HCS VM configuration: {error}"))
        })
    }
}

/// The path of the installer ISO, or a refusal naming the task that will teach
/// the pipeline to build a VM from a cloud image.
///
/// Same shape as [`ensure_supported_network_mode`]: an unsupported request is
/// refused in one place, with a message that says which task lifts the refusal.
pub(crate) fn local_media_path(request: &VmCreateRequest) -> Result<&str, RepositoryError> {
    match &request.source {
        VmSource::LocalMedia { path } => Ok(path),
        VmSource::CloudImage { .. } => {
            let error = RepositoryError::new(
                "the HCS backend cannot build a VM from a cloud image yet; \
                 seed generation and provisioning arrive with #61",
            );
            log::error!("{error}");
            Err(error)
        }
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

const MEMORY_SIZE_POINTER: &str = "/VirtualMachine/ComputeTopology/Memory/SizeInMB";
const PROCESSOR_COUNT_POINTER: &str = "/VirtualMachine/ComputeTopology/Processor/Count";
const DEVICES_POINTER: &str = "/VirtualMachine/Devices";
const NETWORK_ADAPTERS_KEY: &str = "NetworkAdapters";

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

#[derive(Serialize)]
struct VirtualMachine {
    #[serde(rename = "Chipset")]
    chipset: Chipset,
    #[serde(rename = "ComputeTopology")]
    compute_topology: ComputeTopology,
    #[serde(rename = "Devices")]
    devices: Devices,
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
struct HvSocket {
    #[serde(rename = "HvSocketConfig")]
    config: HvSocketConfig,
}

#[derive(Serialize)]
struct HvSocketConfig {
    #[serde(rename = "ServiceTable")]
    service_table: BTreeMap<String, EmptyObject>,
}

#[derive(Serialize)]
struct EmptyObject {}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use uuid::Uuid;
    use vmlord_core::{
        CloudImage, GpuMode, NetworkMode, Password, Provisioning, SshAccess, VmCreateRequest,
        VmSource, ubuntu,
    };

    use super::{
        HcsVmConfigBuilder, VmTopology, adapter_key, apply_network_adapter, apply_topology,
        ensure_supported_network_mode, read_topology, remove_network_adapter,
    };

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

    #[test]
    fn a_cloud_image_is_refused_with_the_task_that_will_support_it() {
        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "user".into(),
                    password: Some(Password::new("secret")),
                    ssh: SshAccess::Enabled { deploy_key: true },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ..request()
        };

        let error = HcsVmConfigBuilder::build(
            &request,
            &PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx"),
        )
        .expect_err("the pipeline cannot provision a cloud image yet");

        assert!(error.to_string().contains("#61"), "got {error}");
    }

    #[test]
    fn builds_the_minimal_configuration() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let json: Value = serde_json::from_str(
            &HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap(),
        )
        .unwrap();

        assert_eq!(
            json,
            json!({
                "SchemaVersion": { "Major": 2, "Minor": 1 },
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
                        "HvSocket": { "HvSocketConfig": { "ServiceTable": {} } },
                        "Keyboard": {},
                        "Mouse": {}
                    }
                }
            }),
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

        let json: Value =
            serde_json::from_str(&HcsVmConfigBuilder::build(&request, &system_disk_path).unwrap())
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

        let document = HcsVmConfigBuilder::build(&request, &system_disk_path).unwrap();

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
        for mode in [GpuMode::Default, GpuMode::TryAll, GpuMode::Unknown(42)] {
            let request = VmCreateRequest {
                gpu_mode: mode,
                ..request()
            };
            assert!(
                HcsVmConfigBuilder::build(&request, &system_disk_path)
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
        let request = VmCreateRequest {
            network_mode: NetworkMode::Nat,
            ..request()
        };

        let document = HcsVmConfigBuilder::build(&request, &system_disk_path).unwrap();

        let json: Value = serde_json::from_str(&document).unwrap();
        assert!(
            json.pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none()
        );
    }

    #[test]
    fn rejects_each_network_mode_that_waits_for_its_own_task() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        for mode in [
            NetworkMode::External,
            NetworkMode::Internal,
            NetworkMode::Unknown(7),
        ] {
            let request = VmCreateRequest {
                network_mode: mode,
                ..request()
            };

            let message = HcsVmConfigBuilder::build(&request, &system_disk_path)
                .unwrap_err()
                .to_string();

            assert!(message.contains("network mode"), "got: {message}");
            assert!(message.contains("#10"), "got: {message}");
        }
    }

    #[test]
    fn removes_the_network_adapter_section_and_nothing_else() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let created = HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap();
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
        let created = HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap();

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
        let request = VmCreateRequest {
            name: String::new(),
            ..request()
        };

        assert!(HcsVmConfigBuilder::build(&request, &system_disk_path).is_err());
    }

    #[test]
    fn reads_back_the_topology_it_built() {
        let request = VmCreateRequest {
            ram_mb: 4096,
            cpu_cores: 4,
            ..request()
        };
        let document =
            HcsVmConfigBuilder::build(&request, &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"))
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
        let document = HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap();

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
        let document =
            HcsVmConfigBuilder::build(&request(), &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"))
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
        let document = HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap();

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
        let document =
            HcsVmConfigBuilder::build(&request(), &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"))
                .unwrap();

        let once = with_adapter(&document);

        assert_eq!(with_adapter(&once), once);
    }

    #[test]
    fn the_adapter_key_is_how_the_section_names_the_adapter() {
        // A detach names the adapter by this key in its resource path. A
        // spelling that drifts from the one the section uses detaches nothing
        // and still reports success, so both sides read it from here.
        let document =
            HcsVmConfigBuilder::build(&request(), &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"))
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
}
