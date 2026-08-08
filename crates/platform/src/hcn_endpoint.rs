//! A VM's endpoint in VMLord's shared NAT network.
//!
//! The endpoint is the per-VM half of VMLord's networking: the network belongs
//! to the installation, the endpoint to one VM. It is created lazily, the first
//! time the VM is started, and lives until the VM is deleted -- across stops
//! and across VMLord restarts. Re-creating it per start would hand the guest a
//! new address every time and break everything that remembered the old one.
//!
//! Its identifier is therefore not derivable and has to be remembered:
//! [`crate::VmComputeSystemMapping::endpoint_id`] is where it is written.

use std::ptr;

use serde::Serialize;
use uuid::Uuid;
use vmlord_core::RepositoryError;
use windows::{
    Win32::System::HostComputeNetwork::{
        HcnCloseEndpoint, HcnCreateEndpoint, HcnDeleteEndpoint, HcnOpenEndpoint,
    },
    core::{GUID, HRESULT, HSTRING},
};

use crate::{
    error::windows_error,
    hcn::{HcnNetwork, SchemaVersion, VMLORD_NETWORK_ID, is_absent},
};

/// `HCN_E_ENDPOINT_NOT_FOUND` from `computenetwork.h` (facility 0x3B).
///
/// `windows-rs` does not surface the HCN error constants, so the value is
/// spelled out here.
const HCN_E_ENDPOINT_NOT_FOUND: HRESULT = HRESULT(0x803B_0002_u32 as i32);

/// Whether HNS is reporting that it does not have the endpoint.
fn is_endpoint_absent(error: &windows::core::Error) -> bool {
    is_absent(error, HCN_E_ENDPOINT_NOT_FOUND)
}

/// An owned HCN endpoint handle used by the Windows Host Network Service.
pub struct HcnEndpoint(*mut core::ffi::c_void);

impl HcnEndpoint {
    /// Creates the endpoint `id` for VM `vm_name` inside `network`.
    ///
    /// The caller allocates the identifier and is responsible for recording it
    /// before anything else can find the endpoint again: a VMLord that dies
    /// between this call and that write leaves an orphan behind, which is what
    /// the cleanup on `initialize` exists to collect.
    pub fn create(network: &HcnNetwork, id: Uuid, vm_name: &str) -> Result<Self, RepositoryError> {
        let settings = endpoint_settings(vm_name)?;
        log::debug!("creating HCN endpoint {id} for VM \"{vm_name}\"");

        let guid = GUID::from_u128(id.as_u128());
        let settings = HSTRING::from(settings);
        let mut endpoint = ptr::null_mut();
        // SAFETY: The network handle is owned by `network` and outlives the
        // call, as do `guid` and `settings`, and the output pointer is valid
        // for it. On success HCN transfers ownership of the returned handle to
        // this wrapper.
        unsafe { HcnCreateEndpoint(network.handle(), &guid, &settings, &mut endpoint, None) }
            .map_err(|error| {
                let error = windows_error("create HCN endpoint", Some(vm_name), error);
                log::error!("{error}");
                error
            })?;

        log::info!("created HCN endpoint {id} for VM \"{vm_name}\"");
        Ok(Self(endpoint))
    }

    /// Opens an existing HCN endpoint by its identifier.
    pub fn open(id: Uuid) -> Result<Self, RepositoryError> {
        Self::try_open(id).map_err(|error| {
            let error = windows_error("open HCN endpoint", None, error);
            log::error!("{error}");
            error
        })
    }

    /// Opens an HCN endpoint, reporting `Ok(None)` when HNS does not have it.
    ///
    /// This is what makes a recorded endpoint id safe to trust: one deleted
    /// outside VMLord, or lost to an HNS reset, answers "absent" here instead
    /// of failing the start that asked for it.
    pub fn open_if_present(id: Uuid) -> Result<Option<Self>, RepositoryError> {
        match Self::try_open(id) {
            Ok(endpoint) => Ok(Some(endpoint)),
            Err(error) if is_endpoint_absent(&error) => {
                log::debug!("HNS does not know endpoint {id}");
                Ok(None)
            }
            Err(error) => {
                let error = windows_error("open HCN endpoint", None, error);
                log::error!("{error}");
                Err(error)
            }
        }
    }

    fn try_open(id: Uuid) -> Result<Self, windows::core::Error> {
        let guid = GUID::from_u128(id.as_u128());
        let mut endpoint = ptr::null_mut();
        // SAFETY: `guid` and the output pointer are valid for the call. On
        // success HCN transfers ownership of the returned handle to this
        // wrapper.
        unsafe { HcnOpenEndpoint(&guid, &mut endpoint, None) }?;
        Ok(Self(endpoint))
    }

    /// Deletes an HCN endpoint, treating one HNS does not have as deleted.
    ///
    /// Deleting takes an identifier rather than an open handle, so this is an
    /// associated function; an endpoint that is already gone is the requested
    /// outcome, not a failure.
    pub fn delete(id: Uuid) -> Result<(), RepositoryError> {
        let guid = GUID::from_u128(id.as_u128());
        log::debug!("deleting HCN endpoint {id}");
        // SAFETY: `guid` is valid for the duration of the call.
        match unsafe { HcnDeleteEndpoint(&guid, None) } {
            Ok(()) => {
                log::info!("deleted HCN endpoint {id}");
                Ok(())
            }
            Err(error) if is_endpoint_absent(&error) => {
                log::debug!("HCN endpoint {id} was already gone");
                Ok(())
            }
            Err(error) => {
                let error = windows_error("delete HCN endpoint", None, error);
                log::error!("{error}");
                Err(error)
            }
        }
    }
}

impl Drop for HcnEndpoint {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns a handle returned by HCN.
        let _ = unsafe { HcnCloseEndpoint(self.0) };
    }
}

/// Builds the settings document for the endpoint of VM `vm_name`.
///
/// No address is asked for: the network's own IPAM assigns one out of the
/// subnet the network was created with, and that address -- not one VMLord
/// picked -- is what the guest is offered and what
/// `HcnQueryEndpointProperties` later reports.
fn endpoint_settings(vm_name: &str) -> Result<String, RepositoryError> {
    let settings = EndpointSettings {
        schema_version: SchemaVersion::V2,
        name: endpoint_name(vm_name),
        // The identifier goes to `HcnCreateEndpoint` as an argument, but the
        // network the endpoint joins is named only here.
        host_compute_network: GUID::from_u128(VMLORD_NETWORK_ID),
        flags: 0,
    };

    serde_json::to_string(&settings).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCN endpoint settings of VM \"{vm_name}\": {error}"
        ))
    })
}

/// The name HNS reports for a VM's endpoint, e.g. in `Get-HnsEndpoint`.
///
/// The VM's own name, prefixed: the endpoint is one row among every container
/// and VM on the host, and the prefix is what says which of them are VMLord's.
fn endpoint_name(vm_name: &str) -> String {
    format!("VMLord-{vm_name}")
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointSettings {
    schema_version: SchemaVersion,
    name: String,
    #[serde(serialize_with = "serialize_guid")]
    host_compute_network: GUID,
    flags: u32,
}

/// Writes a GUID the way HNS spells identifiers in a settings document.
fn serialize_guid<S: serde::Serializer>(guid: &GUID, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{guid:?}"))
}

#[cfg(test)]
mod tests {
    use super::endpoint_settings;

    fn settings(vm_name: &str) -> serde_json::Value {
        serde_json::from_str(&endpoint_settings(vm_name).unwrap())
            .expect("the settings document should be valid JSON")
    }

    #[test]
    fn the_settings_join_the_shared_vmlord_network() {
        let document = settings("dev-linux");

        assert_eq!(document["SchemaVersion"]["Major"], 2);
        assert_eq!(document["SchemaVersion"]["Minor"], 0);
        assert_eq!(
            document["HostComputeNetwork"],
            "1D6FAE4A-5C78-4C1B-9E2F-3A8B7D4C6E50"
        );
        assert_eq!(document["Flags"], 0);
    }

    #[test]
    fn the_endpoint_is_named_after_its_vm() {
        assert_eq!(settings("dev-linux")["Name"], "VMLord-dev-linux");
    }

    #[test]
    fn the_settings_ask_for_no_address_of_their_own() {
        // The address comes from the network's IPAM. Asking for one here would
        // mean VMLord picking guest addresses itself -- a second allocator
        // beside HNS's, and one with nothing to keep it from handing two VMs
        // the same address.
        let document = settings("dev-linux");

        assert!(document.get("IpConfigurations").is_none(), "{document}");
        assert!(document.get("Routes").is_none(), "{document}");
    }
}
