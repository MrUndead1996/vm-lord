//! Builds the HCS JSON configuration document for a new compute system.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;
use vmlord_core::{GpuMode, NetworkMode, RepositoryError, VmCreateRequest};

/// Builds HCS compute-system configuration documents from a validated
/// [`VmCreateRequest`].
pub(crate) struct HcsVmConfigBuilder;

impl HcsVmConfigBuilder {
    /// Builds the JSON configuration for `request`, attaching
    /// `system_disk_path` as the VM's boot disk and `request.image_path` as
    /// its installer ISO.
    ///
    /// GPU and network configuration are not yet implemented (deferred to
    /// their own tasks); any mode other than `None` is rejected.
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
        if request.network_mode != NetworkMode::None {
            return Err(RepositoryError::new(format!(
                "HCS configuration does not support network mode: {:?}",
                request.network_mode
            )));
        }

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
                    path: PathBuf::from(&request.image_path),
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
    use vmlord_core::{GpuMode, NetworkMode, VmCreateRequest};

    use super::HcsVmConfigBuilder;

    fn request() -> VmCreateRequest {
        VmCreateRequest {
            name: "test-vm".into(),
            image_path: "C:\\images\\installer.iso".into(),
            ram_mb: 512,
            disk_gb: 1,
            cpu_cores: 1,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
            username: "admin".into(),
            password: "password".into(),
            ssh_enabled: true,
            ssh_deploy_key: false,
        }
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
        let request = VmCreateRequest {
            password: "secret \"password\"".into(),
            ..request()
        };
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");

        let document = HcsVmConfigBuilder::build(&request, &system_disk_path).unwrap();

        assert!(!document.contains("secret"));
        assert!(!document.contains("password"));
        assert!(!document.contains("username"));
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
    fn rejects_each_unsupported_network_mode() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        for mode in [
            NetworkMode::Nat,
            NetworkMode::External,
            NetworkMode::Internal,
            NetworkMode::Unknown(7),
        ] {
            let request = VmCreateRequest {
                network_mode: mode,
                ..request()
            };
            assert!(
                HcsVmConfigBuilder::build(&request, &system_disk_path)
                    .unwrap_err()
                    .to_string()
                    .contains("network mode")
            );
        }
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
}
