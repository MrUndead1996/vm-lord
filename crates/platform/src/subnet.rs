//! Picks the IPv4 subnet for VMLord's shared NAT network.
//!
//! The subnet is chosen once, when the network is created, and is never
//! revisited: the guests' addresses come out of it, and re-picking it would
//! move every address that anything already remembers.

use std::{fmt, net::Ipv4Addr};

use vmlord_core::RepositoryError;
use windows::Win32::{
    Foundation::{ERROR_SUCCESS, WIN32_ERROR},
    NetworkManagement::IpHelper::{
        FreeMibTable, GetUnicastIpAddressTable, MIB_UNICASTIPADDRESS_TABLE,
    },
    Networking::WinSock::{AF_INET, AF_INET6},
};

use crate::error::hresult_to_repository_error;

/// The subnets VMLord's NAT network is allowed to use, in preference order.
///
/// Both live in the part of 172.16/12 that neither Docker Desktop, WSL nor
/// Hyper-V's own Default Switch hands out by default, which is what makes the
/// first candidate free on an ordinary host.
pub(crate) const CANDIDATE_SUBNETS: [Ipv4Subnet; 2] = [
    Ipv4Subnet::new(Ipv4Addr::new(172, 22, 42, 0), 24),
    Ipv4Subnet::new(Ipv4Addr::new(172, 22, 142, 0), 24),
];

/// An IPv4 network: an address together with its prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Subnet {
    address: Ipv4Addr,
    prefix_length: u8,
}

impl Ipv4Subnet {
    /// A subnet covering `address`, `prefix_length` bits wide.
    ///
    /// A prefix longer than an IPv4 address is clamped to 32: the operating
    /// system is the source of these values, and a nonsensical one must not
    /// turn into a shift overflow.
    #[must_use]
    pub const fn new(address: Ipv4Addr, prefix_length: u8) -> Self {
        Self {
            address,
            prefix_length: if prefix_length > 32 {
                32
            } else {
                prefix_length
            },
        }
    }

    /// The address of the network itself (the host bits cleared).
    #[must_use]
    pub fn network_address(self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.address) & mask(self.prefix_length))
    }

    /// The address VMLord gives the NAT gateway: the first one in the subnet.
    #[must_use]
    pub fn gateway(self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.network_address()) + 1)
    }

    /// The number of bits the subnet's prefix fixes.
    #[must_use]
    pub fn prefix_length(self) -> u8 {
        self.prefix_length
    }

    /// Whether the two subnets share any address.
    ///
    /// Two networks overlap exactly when the wider one contains the narrower,
    /// so comparing the addresses over the shorter of the two prefixes decides
    /// it in both directions.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        let shared = self.prefix_length.min(other.prefix_length);
        (u32::from(self.address) ^ u32::from(other.address)) & mask(shared) == 0
    }
}

impl fmt::Display for Ipv4Subnet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}",
            self.network_address(),
            self.prefix_length
        )
    }
}

/// The bits a prefix of `prefix_length` fixes.
fn mask(prefix_length: u8) -> u32 {
    match prefix_length {
        0 => 0,
        length => u32::MAX << (32 - u32::from(length.min(32))),
    }
}

/// Picks the first candidate subnet that no host adapter already uses.
///
/// When every candidate is taken the first one is used anyway, with a warning:
/// a VM without a network is worse than a VM on a contested subnet, and the
/// warning is the only thing that connects the resulting routing failure to
/// VMLord.
pub(crate) fn choose_subnet(occupied: &[Ipv4Subnet]) -> Ipv4Subnet {
    for candidate in CANDIDATE_SUBNETS {
        match occupied.iter().find(|used| candidate.overlaps(**used)) {
            None => {
                log::debug!("candidate subnet {candidate} does not overlap any host adapter");
                return candidate;
            }
            Some(used) => {
                log::debug!("candidate subnet {candidate} overlaps host subnet {used}");
            }
        }
    }

    let fallback = CANDIDATE_SUBNETS[0];
    log::warn!(
        "every candidate subnet for the VMLord NAT network overlaps a host adapter; \
         using {fallback} anyway. A host route -- a corporate VPN's, typically -- \
         shares this range, and traffic to it may reach the VMs instead of its \
         intended destination"
    );
    fallback
}

/// The IPv4 subnets the host's own adapters are on.
pub(crate) fn host_subnets() -> Result<Vec<Ipv4Subnet>, RepositoryError> {
    let table = UnicastAddressTable::query()?;
    let subnets: Vec<Ipv4Subnet> = table.ipv4_subnets();
    log::debug!(
        "the host occupies {} IPv4 subnet(s): {}",
        subnets.len(),
        subnets
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(subnets)
}

/// The index of the host interface that carries `address`.
///
/// The DHCP server needs it to name the adapter its replies leave through: a
/// socket bound to `0.0.0.0` sends the limited broadcast every DHCP offer uses
/// through whichever interface the routing table prefers, which on an ordinary
/// host is the physical one rather than VMLord's.
pub(crate) fn interface_index(address: Ipv4Addr) -> Result<u32, RepositoryError> {
    let table = UnicastAddressTable::query()?;
    table.interface_index(address).ok_or_else(|| {
        let error = RepositoryError::new(format!(
            "no host adapter carries {address}, so the interface serving the VMLord network \
             could not be identified"
        ));
        log::error!("{error}");
        error
    })
}

/// An owned unicast IP address table, freed with `FreeMibTable` on drop.
struct UnicastAddressTable(*mut MIB_UNICASTIPADDRESS_TABLE);

impl UnicastAddressTable {
    /// Reads the host's unicast IPv4 addresses.
    ///
    /// `AF_INET` rather than `AF_UNSPEC`: only IPv4 subnets can collide with an
    /// IPv4 NAT network, and asking for both families would mean skipping every
    /// IPv6 row afterwards.
    fn query() -> Result<Self, RepositoryError> {
        let mut table = std::ptr::null_mut();
        // SAFETY: The output pointer is valid for the call. On success the
        // table is transferred to this wrapper, which frees it in `Drop`.
        let status = unsafe { GetUnicastIpAddressTable(AF_INET, &mut table) };
        if status != ERROR_SUCCESS {
            let error = unicast_table_error(status);
            log::error!("{error}");
            return Err(error);
        }
        Ok(Self(table))
    }

    /// The interface index of the row holding `address`.
    fn interface_index(&self, address: Ipv4Addr) -> Option<u32> {
        // SAFETY: `self.0` is a table Windows filled in and this wrapper owns.
        let table = unsafe { &*self.0 };
        // SAFETY: `Table` is a flexible array of `NumEntries` rows; the
        // declared length of 1 is the C convention, not the real count.
        let rows =
            unsafe { std::slice::from_raw_parts(table.Table.as_ptr(), table.NumEntries as usize) };

        rows.iter().find_map(|row| {
            // SAFETY: `Address` is a `SOCKADDR_INET` union whose active member
            // the family field names; `Ipv4` is read only after `AF_INET`.
            let family = unsafe { row.Address.si_family };
            if family == AF_INET6 {
                return None;
            }
            // SAFETY: See above -- the family is `AF_INET` here.
            let raw = unsafe { row.Address.Ipv4.sin_addr.S_un.S_addr };
            // `S_addr` is in network byte order, which is what `Ipv4Addr::from`
            // a big-endian byte array expects.
            (Ipv4Addr::from(raw.to_ne_bytes()) == address).then_some(row.InterfaceIndex)
        })
    }

    fn ipv4_subnets(&self) -> Vec<Ipv4Subnet> {
        // SAFETY: `self.0` is a table Windows filled in and this wrapper owns.
        let table = unsafe { &*self.0 };
        // SAFETY: `Table` is a flexible array of `NumEntries` rows; the
        // declared length of 1 is the C convention, not the real count.
        let rows =
            unsafe { std::slice::from_raw_parts(table.Table.as_ptr(), table.NumEntries as usize) };

        rows.iter()
            .filter_map(|row| {
                // SAFETY: `Address` is a `SOCKADDR_INET` union whose active
                // member the family field names; `Ipv4` is read only after it
                // reports `AF_INET`.
                let family = unsafe { row.Address.si_family };
                if family == AF_INET6 {
                    return None;
                }
                // SAFETY: See above -- the family is `AF_INET` here.
                let address = unsafe { row.Address.Ipv4.sin_addr.S_un.S_addr };
                Some(Ipv4Subnet::new(
                    // `S_addr` is in network byte order, which is what
                    // `Ipv4Addr::from` a big-endian byte array expects.
                    Ipv4Addr::from(address.to_ne_bytes()),
                    row.OnLinkPrefixLength,
                ))
            })
            .collect()
    }
}

impl Drop for UnicastAddressTable {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns a table allocated by
        // `GetUnicastIpAddressTable` and frees it exactly once here.
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

/// Reports a failure to read the host's addresses.
///
/// `GetUnicastIpAddressTable` returns a Win32 error code rather than an
/// HRESULT, so it is widened into one the way `windows-rs` does.
fn unicast_table_error(status: WIN32_ERROR) -> RepositoryError {
    hresult_to_repository_error(
        "read the host unicast IP address table",
        None,
        status.to_hresult().0,
    )
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{CANDIDATE_SUBNETS, Ipv4Subnet, choose_subnet};

    fn subnet(address: [u8; 4], prefix_length: u8) -> Ipv4Subnet {
        Ipv4Subnet::new(Ipv4Addr::from(address), prefix_length)
    }

    #[test]
    fn a_subnet_is_displayed_as_its_network_address_and_prefix() {
        assert_eq!(subnet([172, 22, 42, 7], 24).to_string(), "172.22.42.0/24");
        assert_eq!(subnet([10, 0, 0, 0], 8).to_string(), "10.0.0.0/8");
        assert_eq!(subnet([192, 168, 1, 5], 32).to_string(), "192.168.1.5/32");
    }

    #[test]
    fn the_gateway_is_the_first_address_of_the_subnet() {
        assert_eq!(
            subnet([172, 22, 42, 0], 24).gateway(),
            Ipv4Addr::new(172, 22, 42, 1)
        );
    }

    #[test]
    fn a_wider_subnet_overlaps_the_narrower_one_it_contains() {
        let wide = subnet([172, 16, 0, 0], 12);
        let narrow = subnet([172, 22, 42, 0], 24);

        assert!(wide.overlaps(narrow));
        assert!(narrow.overlaps(wide));
    }

    #[test]
    fn adjacent_subnets_do_not_overlap() {
        assert!(!subnet([172, 22, 42, 0], 24).overlaps(subnet([172, 22, 43, 0], 24)));
        assert!(!subnet([172, 22, 42, 0], 24).overlaps(subnet([192, 168, 0, 0], 16)));
    }

    #[test]
    fn a_zero_length_prefix_overlaps_everything() {
        assert!(subnet([0, 0, 0, 0], 0).overlaps(subnet([172, 22, 42, 0], 24)));
    }

    #[test]
    fn a_prefix_longer_than_an_address_is_clamped() {
        let host_route = subnet([172, 22, 42, 9], 40);

        assert_eq!(host_route.to_string(), "172.22.42.9/32");
        assert!(host_route.overlaps(CANDIDATE_SUBNETS[0]));
    }

    #[test]
    fn the_first_candidate_is_used_when_the_host_occupies_nothing_nearby() {
        let occupied = [subnet([192, 168, 1, 0], 24), subnet([10, 0, 0, 0], 8)];

        assert_eq!(choose_subnet(&occupied), CANDIDATE_SUBNETS[0]);
    }

    #[test]
    fn an_occupied_first_candidate_moves_the_choice_to_the_second() {
        // A corporate VPN handing out 172.22.32.0/20 covers the first candidate
        // (172.22.32.0-172.22.47.255) but not the second.
        let occupied = [subnet([172, 22, 32, 0], 20)];

        assert_eq!(choose_subnet(&occupied), CANDIDATE_SUBNETS[1]);
    }

    #[test]
    fn a_single_host_address_inside_a_candidate_occupies_it() {
        let occupied = [subnet([172, 22, 42, 1], 24)];

        assert_eq!(choose_subnet(&occupied), CANDIDATE_SUBNETS[1]);
    }

    #[test]
    fn the_first_candidate_is_used_anyway_when_every_candidate_is_occupied() {
        let occupied = [subnet([172, 16, 0, 0], 12)];

        assert_eq!(choose_subnet(&occupied), CANDIDATE_SUBNETS[0]);
    }

    #[test]
    fn the_candidates_are_the_two_documented_subnets_in_order() {
        assert_eq!(
            CANDIDATE_SUBNETS.map(|candidate| candidate.to_string()),
            ["172.22.42.0/24", "172.22.142.0/24"]
        );
    }
}
