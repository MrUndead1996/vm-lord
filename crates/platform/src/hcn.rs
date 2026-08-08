//! VMLord's shared NAT network in the Windows Host Network Service.
//!
//! There is exactly one such network per installation. It has no owner among
//! the VMs -- the per-VM object is the endpoint -- so there is nothing to
//! record in the [`crate::MetadataStore`] either: a constant identifier makes
//! "open it, create it if it is missing" idempotent without reading a file.

use std::ptr;

use serde::Serialize;
use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::ERROR_NOT_FOUND,
        System::HostComputeNetwork::{
            HcnCloseNetwork, HcnCreateNetwork, HcnDeleteNetwork, HcnOpenNetwork,
        },
    },
    core::{GUID, HRESULT, HSTRING},
};

use crate::{
    error::windows_error,
    subnet::{Ipv4Subnet, choose_subnet, host_subnets},
};

/// The identifier of VMLord's shared NAT network.
///
/// Constant by design: the network belongs to the installation rather than to
/// any VM, so this value is the whole of what VMLord has to remember about it.
pub const VMLORD_NETWORK_ID: u128 = 0x1d6f_ae4a_5c78_4c1b_9e2f_3a8b_7d4c_6e50;

/// The name HNS reports for the network, e.g. in `Get-HnsNetwork`.
const VMLORD_NETWORK_NAME: &str = "VMLord";

/// `HCN_E_NETWORK_NOT_FOUND` from `computenetwork.h` (facility 0x3B).
///
/// `windows-rs` does not surface the HCN error constants, so the value is
/// spelled out here.
const HCN_E_NETWORK_NOT_FOUND: HRESULT = HRESULT(0x803B_0001_u32 as i32);

/// Whether HNS is reporting that it does not have the network.
///
/// Two codes, because HNS answers with a different one per call: a live host
/// fails `HcnOpenNetwork` with `HCN_E_NETWORK_NOT_FOUND` but
/// `HcnDeleteNetwork` with `ERROR_NOT_FOUND` (0x80070490, "Element not
/// found"). Both mean the same thing to a caller, and either one alone leaves
/// the other call reporting an absent network as a failure.
fn is_network_absent(error: &windows::core::Error) -> bool {
    let code = error.code();
    code == HCN_E_NETWORK_NOT_FOUND || code == ERROR_NOT_FOUND.to_hresult()
}

/// An owned HCN network handle used by the Windows Host Network Service.
pub struct HcnNetwork(*mut core::ffi::c_void);

impl HcnNetwork {
    /// Opens VMLord's shared NAT network, creating it if HNS does not have it.
    ///
    /// The subnet is picked only on the creating path: an existing network
    /// keeps the subnet it was created with, because the guests' addresses come
    /// out of it and re-picking it would move every address anything already
    /// remembers.
    pub fn ensure() -> Result<Self, RepositoryError> {
        if let Some(network) = Self::open_if_present(VMLORD_NETWORK_ID)? {
            log::debug!("the VMLord NAT network already exists");
            return Ok(network);
        }

        let subnet = choose_subnet(&host_subnets()?);
        let settings = nat_network_settings(subnet)?;
        log::info!("creating the VMLord NAT network on {subnet}");

        match Self::create(VMLORD_NETWORK_ID, &settings) {
            Ok(network) => {
                log::info!("created the VMLord NAT network on {subnet}");
                Ok(network)
            }
            // Another VMLord process may have created the network between the
            // open above and this create, in which case its network -- and its
            // subnet -- is the one to use.
            Err(error) => match Self::open_if_present(VMLORD_NETWORK_ID)? {
                Some(network) => {
                    log::debug!(
                        "the VMLord NAT network was created concurrently; using the existing one \
                         instead of reporting: {error}"
                    );
                    Ok(network)
                }
                None => Err(error),
            },
        }
    }

    /// Opens an existing HCN network by its canonical 128-bit identifier.
    pub fn open(id: u128) -> Result<Self, RepositoryError> {
        Self::try_open(id).map_err(|error| {
            let error = windows_error("open HCN network", None, error);
            log::error!("{error}");
            error
        })
    }

    /// Opens an HCN network, reporting `Ok(None)` when HNS does not have it.
    pub fn open_if_present(id: u128) -> Result<Option<Self>, RepositoryError> {
        match Self::try_open(id) {
            Ok(network) => Ok(Some(network)),
            Err(error) if is_network_absent(&error) => {
                log::debug!("HNS does not know network {:?}", GUID::from_u128(id));
                Ok(None)
            }
            Err(error) => {
                let error = windows_error("open HCN network", None, error);
                log::error!("{error}");
                Err(error)
            }
        }
    }

    fn try_open(id: u128) -> Result<Self, windows::core::Error> {
        let id = GUID::from_u128(id);
        let mut network = ptr::null_mut();
        // SAFETY: `id` and the output pointer are valid for the call. On success HCN
        // transfers ownership of the returned handle to this wrapper.
        unsafe { HcnOpenNetwork(&id, &mut network, None) }?;
        Ok(Self(network))
    }

    /// Creates an HCN network with `id` from a settings document.
    fn create(id: u128, settings: &str) -> Result<Self, RepositoryError> {
        let id = GUID::from_u128(id);
        let settings = HSTRING::from(settings);
        let mut network = ptr::null_mut();
        // SAFETY: `id` and `settings` outlive the call, and the output pointer
        // is valid for it. On success HCN transfers ownership of the returned
        // handle to this wrapper.
        unsafe { HcnCreateNetwork(&id, &settings, &mut network, None) }.map_err(|error| {
            let error = windows_error("create HCN network", None, error);
            log::error!("{error}");
            error
        })?;
        Ok(Self(network))
    }

    /// Deletes an HCN network, treating one HNS does not have as deleted.
    ///
    /// Deleting a network takes its identifier rather than an open handle, so
    /// this is an associated function; a network that is already gone is the
    /// requested outcome, not a failure.
    pub fn delete(id: u128) -> Result<(), RepositoryError> {
        let guid = GUID::from_u128(id);
        log::debug!("deleting HCN network {guid:?}");
        // SAFETY: `guid` is valid for the duration of the call.
        match unsafe { HcnDeleteNetwork(&guid, None) } {
            Ok(()) => {
                log::info!("deleted HCN network {guid:?}");
                Ok(())
            }
            Err(error) if is_network_absent(&error) => {
                log::debug!("HCN network {guid:?} was already gone");
                Ok(())
            }
            Err(error) => {
                let error = windows_error("delete HCN network", None, error);
                log::error!("{error}");
                Err(error)
            }
        }
    }
}

impl Drop for HcnNetwork {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns a handle returned by HCN.
        let _ = unsafe { HcnCloseNetwork(self.0) };
    }
}

/// Builds the settings document for VMLord's NAT network on `subnet`.
fn nat_network_settings(subnet: Ipv4Subnet) -> Result<String, RepositoryError> {
    let settings = NetworkSettings {
        schema_version: SchemaVersion { major: 2, minor: 0 },
        name: VMLORD_NETWORK_NAME,
        network_type: "NAT",
        ipams: vec![Ipam {
            ipam_type: "Static",
            subnets: vec![Subnet {
                ip_address_prefix: subnet.to_string(),
                // A NAT subnet carries exactly one route: the default one, out
                // through the gateway HNS puts on the host side of the network.
                // Without it the guest gets an address and reaches nothing.
                routes: vec![Route {
                    next_hop: subnet.gateway().to_string(),
                    destination_prefix: "0.0.0.0/0",
                }],
            }],
        }],
        flags: 0,
    };

    serde_json::to_string(&settings).map_err(|error| {
        RepositoryError::new(format!("failed to serialize HCN network settings: {error}"))
    })
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkSettings {
    schema_version: SchemaVersion,
    name: &'static str,
    #[serde(rename = "Type")]
    network_type: &'static str,
    ipams: Vec<Ipam>,
    flags: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SchemaVersion {
    major: u32,
    minor: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Ipam {
    #[serde(rename = "Type")]
    ipam_type: &'static str,
    subnets: Vec<Subnet>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Subnet {
    ip_address_prefix: String,
    routes: Vec<Route>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Route {
    next_hop: String,
    destination_prefix: &'static str,
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{VMLORD_NETWORK_ID, nat_network_settings};
    use crate::subnet::{CANDIDATE_SUBNETS, Ipv4Subnet};

    fn settings(subnet: Ipv4Subnet) -> serde_json::Value {
        serde_json::from_str(&nat_network_settings(subnet).unwrap())
            .expect("the settings document should be valid JSON")
    }

    #[test]
    fn the_settings_describe_a_nat_network_on_the_given_subnet() {
        let document = settings(CANDIDATE_SUBNETS[0]);

        assert_eq!(document["SchemaVersion"]["Major"], 2);
        assert_eq!(document["SchemaVersion"]["Minor"], 0);
        assert_eq!(document["Name"], "VMLord");
        assert_eq!(document["Type"], "NAT");
        assert_eq!(document["Flags"], 0);
        assert_eq!(document["Ipams"][0]["Type"], "Static");
        assert_eq!(
            document["Ipams"][0]["Subnets"][0]["IpAddressPrefix"],
            "172.22.42.0/24"
        );
    }

    #[test]
    fn the_subnet_carries_a_default_route_through_its_gateway() {
        let route = &settings(CANDIDATE_SUBNETS[1])["Ipams"][0]["Subnets"][0]["Routes"][0];

        assert_eq!(route["NextHop"], "172.22.142.1");
        assert_eq!(route["DestinationPrefix"], "0.0.0.0/0");
    }

    #[test]
    fn the_prefix_is_the_network_address_even_when_the_subnet_names_a_host() {
        let document = settings(Ipv4Subnet::new(Ipv4Addr::new(172, 22, 42, 17), 24));

        assert_eq!(
            document["Ipams"][0]["Subnets"][0]["IpAddressPrefix"],
            "172.22.42.0/24"
        );
    }

    #[test]
    fn the_network_identifier_is_constant() {
        // The identifier is the whole of what VMLord remembers about the
        // network: changing it strands the existing one and every endpoint in
        // it on every host that has already run VMLord.
        assert_eq!(
            format!("{:?}", windows::core::GUID::from_u128(VMLORD_NETWORK_ID)),
            "1D6FAE4A-5C78-4C1B-9E2F-3A8B7D4C6E50"
        );
    }
}
