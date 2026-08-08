//! VMLord's DHCP server for the shared NAT network.
//!
//! HNS NAT does not answer a Linux guest's DHCP Discover, and this network's
//! `EnableDhcpServer` is rejected as unsupported, so the guest would come up
//! with an endpoint and no address at all. VMLord answers instead -- and only
//! ever repeats what HNS's IPAM already decided, so it does not become a second
//! allocator of guest addresses.

use std::{net::Ipv4Addr, time::Duration};

use arcbox_dhcp::DhcpConfig;
use vmlord_core::RepositoryError;

use crate::{hcn_endpoint::EndpointAddress, subnet::Ipv4Subnet};

/// How long a guest may keep its address.
///
/// A day rather than minutes: the server dies with the VMLord process, and a
/// long lease is what lets a guest outlive the application being closed. The
/// guest will not renew until half of it has passed.
const LEASE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// The mask a prefix of `prefix_length` bits stands for.
fn netmask(prefix_length: u8) -> Ipv4Addr {
    let bits = match prefix_length.min(32) {
        0 => 0,
        length => u32::MAX << (32 - u32::from(length)),
    };
    Ipv4Addr::from(bits)
}

/// The subnet the address HNS assigned to an endpoint belongs to.
fn endpoint_subnet(address: &EndpointAddress) -> Result<Ipv4Subnet, RepositoryError> {
    let ip: Ipv4Addr = address.ip_address.parse().map_err(|_| {
        let error = RepositoryError::new(format!(
            "HNS reported \"{}\" as an endpoint address, which is not an IPv4 address",
            address.ip_address
        ));
        log::error!("{error}");
        error
    })?;

    Ok(Ipv4Subnet::new(ip, address.prefix_length))
}

/// Parses a MAC address in either of the two shapes it is written in.
///
/// HNS reports `00-15-5D-01-02-03`; everything else prints the colon form.
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut octets = text.split(['-', ':']);
    for octet in &mut mac {
        *octet = u8::from_str_radix(octets.next()?, 16).ok()?;
    }
    if octets.next().is_some() {
        return None;
    }
    Some(mac)
}

/// The configuration the DHCP server serves `subnet` with.
///
/// The pool `arcbox-dhcp` derives from the gateway and mask covers the whole
/// host range of the subnet. That range is bookkeeping rather than an address
/// pool: every address handed out is one HNS assigned and reserved to its MAC,
/// and a packet from a MAC without a reservation never reaches the server.
fn dhcp_config(subnet: Ipv4Subnet, dns_servers: Vec<Ipv4Addr>) -> DhcpConfig {
    DhcpConfig::new(subnet.gateway(), netmask(subnet.prefix_length()))
        .with_gateway(subnet.gateway())
        .with_dns_servers(dns_servers)
        .with_lease_duration(LEASE_DURATION)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{dhcp_config, endpoint_subnet, netmask, parse_mac};
    use crate::{hcn_endpoint::EndpointAddress, subnet::Ipv4Subnet};

    fn address(ip: &str, prefix_length: u8) -> EndpointAddress {
        EndpointAddress {
            ip_address: ip.to_owned(),
            prefix_length,
        }
    }

    #[test]
    fn a_prefix_becomes_the_mask_it_stands_for() {
        assert_eq!(netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(netmask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(netmask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(netmask(0), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn the_subnet_comes_from_the_address_hns_assigned() {
        let subnet = endpoint_subnet(&address("172.22.42.5", 24))
            .expect("an address HNS reported should describe a subnet");

        assert_eq!(subnet.to_string(), "172.22.42.0/24");
        assert_eq!(subnet.gateway(), Ipv4Addr::new(172, 22, 42, 1));
    }

    #[test]
    fn an_address_that_is_not_ipv4_is_reported_rather_than_guessed() {
        let error = endpoint_subnet(&address("fe80::1", 64))
            .expect_err("an address that is not IPv4 cannot describe the NAT subnet");

        assert!(error.to_string().contains("fe80::1"), "{error}");
    }

    #[test]
    fn a_mac_is_parsed_in_the_shape_hns_reports_it() {
        // HNS reports dashes; the colon form is what every other tool prints.
        assert_eq!(
            parse_mac("00-15-5D-01-02-03"),
            Some([0x00, 0x15, 0x5d, 0x01, 0x02, 0x03])
        );
        assert_eq!(
            parse_mac("00:15:5d:0a:0b:0c"),
            Some([0x00, 0x15, 0x5d, 0x0a, 0x0b, 0x0c])
        );
    }

    #[test]
    fn a_mac_that_is_not_six_octets_is_rejected() {
        assert_eq!(parse_mac("00-15-5D-01-02"), None);
        assert_eq!(parse_mac("00-15-5D-01-02-03-04"), None);
        assert_eq!(parse_mac("not a mac"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn the_pool_covers_every_host_address_of_the_subnet_but_the_gateway() {
        let subnet = Ipv4Subnet::new(Ipv4Addr::new(172, 22, 42, 0), 24);

        let config = dhcp_config(subnet, vec![Ipv4Addr::new(1, 1, 1, 1)]);

        assert_eq!(config.server_ip, Ipv4Addr::new(172, 22, 42, 1));
        assert_eq!(config.gateway, Ipv4Addr::new(172, 22, 42, 1));
        assert_eq!(config.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(config.pool_start, Ipv4Addr::new(172, 22, 42, 2));
        assert_eq!(config.pool_end, Ipv4Addr::new(172, 22, 42, 254));
        assert_eq!(config.dns_servers, vec![Ipv4Addr::new(1, 1, 1, 1)]);
        assert_eq!(config.lease_duration.as_secs(), 24 * 60 * 60);
    }
}
