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
    Win32::System::{
        Com::CoTaskMemFree,
        HostComputeNetwork::{
            HcnCloseEndpoint, HcnCreateEndpoint, HcnDeleteEndpoint, HcnOpenEndpoint,
            HcnQueryEndpointProperties,
        },
    },
    core::{GUID, HRESULT, HSTRING, PWSTR},
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

/// An address HNS assigned to an endpoint.
///
/// Read back rather than chosen: the network's IPAM allocates it, and VMLord
/// only ever repeats it when it has to recreate an endpoint that already had
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointAddress {
    pub ip_address: String,
    pub prefix_length: u8,
}

/// Whether HNS is reporting that it does not have the endpoint.
fn is_endpoint_absent(error: &windows::core::Error) -> bool {
    is_absent(error, HCN_E_ENDPOINT_NOT_FOUND)
}

/// Asks for an endpoint's properties in the schema every VMLord document uses.
///
/// HCN treats the query as a JSON document and rejects an empty string, so the
/// schema version has to be spelled out even when nothing is being filtered.
const PROPERTIES_QUERY: &str = r#"{"SchemaVersion":{"Major":2,"Minor":0}}"#;

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
        Self::create_with_address(network, id, vm_name, None)
    }

    /// Creates the endpoint `id`, asking HNS for `address` when one is given.
    ///
    /// Only the recovery path passes an address, and only one HNS itself
    /// assigned to the endpoint being replaced: repeating it is what keeps the
    /// guest reachable where it was. VMLord never invents an address, so it
    /// does not become a second allocator beside HNS's IPAM.
    pub fn create_with_address(
        network: &HcnNetwork,
        id: Uuid,
        vm_name: &str,
        address: Option<&EndpointAddress>,
    ) -> Result<Self, RepositoryError> {
        let settings = endpoint_settings(vm_name, address)?;
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

    /// The MAC address HNS assigned to this endpoint.
    ///
    /// HNS picks the address when the endpoint is created; VMLord reads it
    /// rather than choosing one, and writes it into the VM's `NetworkAdapters`
    /// section beside the endpoint id.
    pub fn mac_address(&self) -> Result<String, RepositoryError> {
        let properties = self.properties()?;
        let properties: serde_json::Value = serde_json::from_str(&properties).map_err(|error| {
            let error = RepositoryError::new(format!(
                "the properties HNS reported for the endpoint are not valid JSON: {error}"
            ));
            log::error!("{error}");
            error
        })?;

        mac_address(&properties).ok_or_else(|| {
            let error =
                RepositoryError::new("HNS reported no MAC address for the endpoint".to_owned());
            log::error!("{error}");
            error
        })
    }

    /// The address HNS assigned to this endpoint, if it reports one.
    ///
    /// `Ok(None)` rather than an error when there is none: the only caller is
    /// the recovery that recreates an occupied endpoint, and a missing address
    /// costs the guest its old one but must not cost it the start.
    pub fn address(&self) -> Result<Option<EndpointAddress>, RepositoryError> {
        let properties = self.properties()?;
        let properties: serde_json::Value = serde_json::from_str(&properties).map_err(|error| {
            let error = RepositoryError::new(format!(
                "the properties HNS reported for the endpoint are not valid JSON: {error}"
            ));
            log::error!("{error}");
            error
        })?;

        Ok(address(&properties))
    }

    /// Asks HNS for this endpoint's properties as a JSON document.
    fn properties(&self) -> Result<String, RepositoryError> {
        let query = HSTRING::from(PROPERTIES_QUERY);
        let mut properties = PWSTR::null();
        // SAFETY: The endpoint handle is owned by `self` and outlives the call,
        // as does `query`, and the output pointer is valid for it. On success
        // HCN transfers ownership of the returned string to this process.
        unsafe { HcnQueryEndpointProperties(self.0, &query, &mut properties, None) }.map_err(
            |error| {
                let error = windows_error("query HCN endpoint properties", None, error);
                log::error!("{error}");
                error
            },
        )?;

        HcnAllocatedString::new(properties)?.into_string()
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

/// Reads the MAC address out of the properties HNS reports for an endpoint.
fn mac_address(properties: &serde_json::Value) -> Option<String> {
    properties
        .get("MacAddress")
        .and_then(serde_json::Value::as_str)
        .filter(|address| !address.is_empty())
        .map(str::to_owned)
}

/// Reads the first address out of the properties HNS reports for an endpoint.
///
/// One address: VMLord's endpoints are created with none of their own, so HNS's
/// IPAM assigns exactly one out of the network's subnet.
fn address(properties: &serde_json::Value) -> Option<EndpointAddress> {
    let configuration = properties.get("IpConfigurations")?.as_array()?.first()?;
    let ip_address = configuration.get("IpAddress")?.as_str()?;
    if ip_address.is_empty() {
        return None;
    }
    let prefix_length = u8::try_from(configuration.get("PrefixLength")?.as_u64()?).ok()?;

    Some(EndpointAddress {
        ip_address: ip_address.to_owned(),
        prefix_length,
    })
}

/// An owned, HCN-allocated wide string, freed with `CoTaskMemFree` on drop.
///
/// Not the same allocator as the one behind HCS's result documents: the HCN
/// functions hand back COM task memory, so freeing one of these with
/// `LocalFree` -- what [`crate::hcs`] uses -- would corrupt the heap.
struct HcnAllocatedString(PWSTR);

impl HcnAllocatedString {
    fn new(raw: PWSTR) -> Result<Self, RepositoryError> {
        if raw.is_null() {
            return Err(RepositoryError::new("HCN returned a null result string"));
        }
        Ok(Self(raw))
    }

    fn into_string(self) -> Result<String, RepositoryError> {
        // SAFETY: `self.0` is a non-null pointer to a null-terminated UTF-16
        // buffer exclusively owned by this wrapper for the duration of the call.
        unsafe { self.0.to_string() }.map_err(|error| {
            RepositoryError::new(format!("the HCN result was not valid UTF-16: {error}"))
        })
    }
}

impl Drop for HcnAllocatedString {
    fn drop(&mut self) {
        // SAFETY: `self.0` originates from an HCN allocation exclusively owned
        // by this wrapper and is freed exactly once here.
        unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) };
    }
}

/// Builds the settings document for the endpoint of VM `vm_name`.
///
/// Without `address`, no address is asked for: the network's own IPAM assigns
/// one out of the subnet the network was created with, and that address -- not
/// one VMLord picked -- is what the guest is offered and what
/// `HcnQueryEndpointProperties` later reports. `address` is only ever an
/// address HNS assigned to an earlier endpoint of the same VM.
fn endpoint_settings(
    vm_name: &str,
    address: Option<&EndpointAddress>,
) -> Result<String, RepositoryError> {
    let settings = EndpointSettings {
        schema_version: SchemaVersion::V2,
        name: endpoint_name(vm_name),
        // The identifier goes to `HcnCreateEndpoint` as an argument, but the
        // network the endpoint joins is named only here.
        host_compute_network: GUID::from_u128(VMLORD_NETWORK_ID),
        flags: 0,
        ip_configurations: address
            .map(|address| IpConfiguration {
                ip_address: address.ip_address.clone(),
                prefix_length: address.prefix_length,
            })
            .into_iter()
            .collect(),
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
    /// Omitted entirely when empty: an `IpConfigurations: []` would be VMLord
    /// asking HNS for no address rather than leaving the choice to its IPAM.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ip_configurations: Vec<IpConfiguration>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct IpConfiguration {
    ip_address: String,
    prefix_length: u8,
}

/// Writes a GUID the way HNS spells identifiers in a settings document.
fn serialize_guid<S: serde::Serializer>(guid: &GUID, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{guid:?}"))
}

#[cfg(test)]
mod tests {
    use super::{EndpointAddress, address, endpoint_settings, mac_address};

    fn settings(vm_name: &str) -> serde_json::Value {
        serde_json::from_str(&endpoint_settings(vm_name, None).unwrap())
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

    #[test]
    fn the_mac_address_is_read_from_the_reported_properties() {
        let properties = serde_json::json!({
            "ID": "3F2B0C11-0000-0000-0000-000000000000",
            "MacAddress": "00-15-5D-01-02-03"
        });

        assert_eq!(
            mac_address(&properties).as_deref(),
            Some("00-15-5D-01-02-03")
        );
    }

    #[test]
    fn properties_without_a_usable_mac_address_report_none() {
        // An endpoint HNS has not finished setting up answers with the field
        // missing or empty; either way there is no address to configure the
        // adapter with, and a start must say so rather than write a blank one.
        for properties in [
            serde_json::json!({}),
            serde_json::json!({ "MacAddress": "" }),
            serde_json::json!({ "MacAddress": 42 }),
        ] {
            assert_eq!(mac_address(&properties), None, "{properties}");
        }
    }

    #[test]
    fn requested_settings_name_the_address_the_endpoint_must_take() {
        // The recovery path recreates an endpoint that HNS still considers
        // attached. Asking for the address the old one held is what keeps the
        // guest reachable at the same place afterwards.
        let requested = EndpointAddress {
            ip_address: "172.22.42.7".into(),
            prefix_length: 24,
        };

        let document: serde_json::Value =
            serde_json::from_str(&endpoint_settings("dev-linux", Some(&requested)).unwrap())
                .unwrap();

        assert_eq!(
            document["IpConfigurations"],
            serde_json::json!([{ "IpAddress": "172.22.42.7", "PrefixLength": 24 }])
        );
    }

    #[test]
    fn the_address_is_read_from_the_reported_properties() {
        let properties = serde_json::json!({
            "MacAddress": "00-15-5D-01-02-03",
            "IpConfigurations": [{ "IpAddress": "172.22.42.7", "PrefixLength": 24 }]
        });

        assert_eq!(
            address(&properties),
            Some(EndpointAddress {
                ip_address: "172.22.42.7".into(),
                prefix_length: 24,
            })
        );
    }

    #[test]
    fn properties_without_a_usable_address_report_none() {
        // An endpoint HNS has not finished setting up, or one whose properties
        // it no longer reports, leaves the recovery with no address to hold on
        // to; that is a warning, not a parse failure.
        for properties in [
            serde_json::json!({}),
            serde_json::json!({ "IpConfigurations": [] }),
            serde_json::json!({ "IpConfigurations": [{ "PrefixLength": 24 }] }),
            serde_json::json!({ "IpConfigurations": [{ "IpAddress": "", "PrefixLength": 24 }] }),
            serde_json::json!({ "IpConfigurations": [{ "IpAddress": "172.22.42.7" }] }),
        ] {
            assert_eq!(address(&properties), None, "{properties}");
        }
    }
}
