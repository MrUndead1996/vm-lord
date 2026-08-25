//! The DNS servers a guest of VMLord's NAT network is given.
//!
//! Not the NAT gateway: WinNAT runs no DNS proxy on the host side of the
//! network, so a guest pointed at it would resolve nothing. The host's own
//! resolvers are what let the guest see the same names the host does,
//! corporate zones included.

use std::net::Ipv4Addr;

use vmlord_core::RepositoryError;
use windows::Win32::{
    Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, WIN32_ERROR},
    NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
        GAA_FLAG_SKIP_UNICAST, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses,
        IP_ADAPTER_ADDRESSES_LH,
    },
    Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN},
};

use crate::{error::hresult_to_repository_error, subnet::Ipv4Subnet};

/// What a guest is given when the host has no resolver it can use.
///
/// Reached on a host whose only resolvers are loopback ones -- a local DNS
/// proxy the guest cannot route to. Working name resolution through a public
/// resolver beats none at all.
const FALLBACK_DNS: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];

/// The resolvers a guest of `subnet` is offered.
pub(crate) fn dns_servers(subnet: Ipv4Subnet) -> Vec<Ipv4Addr> {
    let configured = match AdapterAddresses::query() {
        Ok(adapters) => adapters.ipv4_dns_servers(),
        Err(error) => {
            tracing::warn!(
                "the host's DNS servers could not be read ({error}); \
                 guests are offered the public resolvers instead"
            );
            Vec::new()
        }
    };

    let servers = usable(configured, subnet);
    tracing::debug!(
        "guests of the VMLord network are offered DNS {}",
        servers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    servers
}

/// Keeps the resolvers a guest can actually reach, in the order the host lists
/// them, and falls back to the public ones when none is left.
fn usable(configured: Vec<Ipv4Addr>, subnet: Ipv4Subnet) -> Vec<Ipv4Addr> {
    let mut servers: Vec<Ipv4Addr> = Vec::new();
    for server in configured {
        // A loopback resolver answers only on the host itself; a link-local one
        // means the adapter never got a configuration; one inside VMLord's own
        // subnet is the gateway, which serves no DNS.
        if server.is_loopback()
            || server.is_link_local()
            || server.is_unspecified()
            || Ipv4Subnet::new(server, 32).overlaps(subnet)
        {
            tracing::debug!(
                "the host resolver {server} cannot serve a guest of the VMLord network"
            );
            continue;
        }
        if !servers.contains(&server) {
            servers.push(server);
        }
    }

    if servers.is_empty() {
        tracing::warn!(
            "the host has no DNS server a guest could reach; \
             guests are offered the public resolvers instead"
        );
        return FALLBACK_DNS.to_vec();
    }
    servers
}

/// An owned `GetAdaptersAddresses` buffer.
///
/// Held as `u64`s rather than bytes: Windows writes a linked list of structures
/// into it, and a `Vec<u8>` guarantees only byte alignment.
struct AdapterAddresses(Vec<u64>);

impl AdapterAddresses {
    /// Reads the adapter table, asking only for the DNS servers.
    ///
    /// The size the first call reports can be stale by the time the second one
    /// runs -- an adapter may appear in between -- so the call is retried a
    /// bounded number of times rather than once.
    fn query() -> Result<Self, RepositoryError> {
        const FLAGS: GET_ADAPTERS_ADDRESSES_FLAGS = GET_ADAPTERS_ADDRESSES_FLAGS(
            GAA_FLAG_SKIP_UNICAST.0
                | GAA_FLAG_SKIP_ANYCAST.0
                | GAA_FLAG_SKIP_MULTICAST.0
                | GAA_FLAG_SKIP_FRIENDLY_NAME.0,
        );
        const ATTEMPTS: usize = 3;

        let mut size: u32 = 16 * 1024;
        let mut last = ERROR_SUCCESS;
        for _ in 0..ATTEMPTS {
            let mut buffer = vec![0u64; size.div_ceil(8) as usize];
            // SAFETY: `buffer` is at least `size` bytes long and outlives the
            // call, and `size` is valid for reading and writing. On success
            // Windows fills the buffer with a linked list whose pointers point
            // into it.
            let status = WIN32_ERROR(unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC.0),
                    FLAGS,
                    None,
                    Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                    &mut size,
                )
            });

            if status == ERROR_SUCCESS {
                return Ok(Self(buffer));
            }
            last = status;
            if status != ERROR_BUFFER_OVERFLOW {
                break;
            }
        }

        let error = hresult_to_repository_error(
            "read the host adapter addresses",
            None,
            last.to_hresult().0,
        );
        tracing::error!("{error}");
        Err(error)
    }

    /// Walks the adapters and collects their IPv4 DNS servers.
    fn ipv4_dns_servers(&self) -> Vec<Ipv4Addr> {
        let mut servers = Vec::new();
        let mut adapter = self.0.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();

        while !adapter.is_null() {
            // SAFETY: `adapter` is either the head of the list Windows wrote
            // into the buffer this wrapper owns or a `Next` pointer from it.
            let current = unsafe { &*adapter };
            let mut entry = current.FirstDnsServerAddress;
            while !entry.is_null() {
                // SAFETY: `entry` points into the same buffer and is non-null.
                let dns = unsafe { &*entry };
                let socket_address = dns.Address.lpSockaddr;
                if !socket_address.is_null() {
                    // SAFETY: `lpSockaddr` points into the same buffer; only
                    // `sa_family` is read before the family is known.
                    let family = unsafe { (*socket_address).sa_family };
                    if family == AF_INET {
                        // SAFETY: The family reported `AF_INET`, so the address
                        // is a `SOCKADDR_IN`.
                        let address = unsafe { &*socket_address.cast::<SOCKADDR_IN>() };
                        // SAFETY: `S_un` is a union whose `S_addr` member is
                        // always readable, and holds the address in network
                        // byte order -- which is what `Ipv4Addr::from` a
                        // big-endian byte array expects.
                        let octets = unsafe { address.sin_addr.S_un.S_addr }.to_ne_bytes();
                        servers.push(Ipv4Addr::from(octets));
                    }
                }
                entry = dns.Next;
            }
            adapter = current.Next;
        }

        servers
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{FALLBACK_DNS, usable};
    use crate::subnet::Ipv4Subnet;

    fn vmlord() -> Ipv4Subnet {
        Ipv4Subnet::new(Ipv4Addr::new(172, 22, 42, 0), 24)
    }

    #[test]
    fn the_hosts_own_resolvers_are_what_the_guest_is_given() {
        let configured = vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(10, 0, 0, 53)];

        assert_eq!(usable(configured.clone(), vmlord()), configured);
    }

    #[test]
    fn a_resolver_on_the_vmlord_subnet_is_dropped() {
        // The NAT gateway is not a resolver: WinNAT runs no DNS proxy, and a
        // guest pointed at it would resolve nothing.
        let configured = vec![Ipv4Addr::new(172, 22, 42, 1), Ipv4Addr::new(192, 168, 1, 1)];

        assert_eq!(
            usable(configured, vmlord()),
            vec![Ipv4Addr::new(192, 168, 1, 1)]
        );
    }

    #[test]
    fn loopback_and_link_local_resolvers_are_dropped() {
        let configured = vec![
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(169, 254, 0, 1),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(9, 9, 9, 9),
        ];

        assert_eq!(
            usable(configured, vmlord()),
            vec![Ipv4Addr::new(9, 9, 9, 9)]
        );
    }

    #[test]
    fn a_resolver_listed_twice_is_offered_once() {
        let configured = vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 1)];

        assert_eq!(
            usable(configured, vmlord()),
            vec![Ipv4Addr::new(192, 168, 1, 1)]
        );
    }

    #[test]
    fn a_host_with_no_usable_resolver_falls_back_to_public_ones() {
        let configured = vec![Ipv4Addr::new(127, 0, 0, 1)];

        assert_eq!(usable(configured, vmlord()), FALLBACK_DNS.to_vec());
    }

    /// Run with:
    /// `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu -- --ignored --exact host_dns::tests::the_host_reports_its_own_resolvers --nocapture`
    #[test]
    #[ignore = "reads the DNS configuration of the host it runs on"]
    fn the_host_reports_its_own_resolvers() {
        let servers = super::AdapterAddresses::query()
            .expect("Windows should report the host's adapters")
            .ipv4_dns_servers();

        println!("the host is configured with DNS {servers:?}");
        assert!(
            !servers.is_empty(),
            "a host with a working network has at least one IPv4 resolver"
        );
    }

    #[test]
    fn the_fallback_is_the_two_documented_public_resolvers() {
        assert_eq!(
            FALLBACK_DNS,
            [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
    }
}
