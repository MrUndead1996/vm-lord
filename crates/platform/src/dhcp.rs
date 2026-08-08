//! VMLord's DHCP server for the shared NAT network.
//!
//! HNS NAT does not answer a Linux guest's DHCP Discover, and this network's
//! `EnableDhcpServer` is rejected as unsupported, so the guest would come up
//! with an endpoint and no address at all. VMLord answers instead -- and only
//! ever repeats what HNS's IPAM already decided, so it does not become a second
//! allocator of guest addresses.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use arcbox_dhcp::{DHCP_CLIENT_PORT, DhcpConfig, DhcpPacket, DhcpServer};
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

/// Everything the DHCP worker mutates while it serves a packet.
///
/// The reservation map is not a cache of `DhcpServer`'s own: that one is
/// private, and `reserve_ip` panics both on an address outside its pool and on
/// one already allocated. Knowing what has been reserved is what makes both
/// panics unreachable.
struct State {
    server: DhcpServer,
    reserved: HashMap<[u8; 6], Ipv4Addr>,
}

impl State {
    fn new(config: DhcpConfig) -> Self {
        Self {
            server: DhcpServer::new(config),
            reserved: HashMap::new(),
        }
    }

    /// Makes `ip` the address served to `mac`, and only to `mac`.
    ///
    /// Idempotent, and the only caller of `reserve_ip`: an address is released
    /// from whoever held it before it is handed to its new owner, whether that
    /// was the same MAC under a different address or another MAC entirely.
    fn reserve(&mut self, mac: [u8; 6], ip: Ipv4Addr) -> Result<(), RepositoryError> {
        let config = self.server.config();
        let pool = u32::from(config.pool_start)..=u32::from(config.pool_end);
        if !pool.contains(&u32::from(ip)) {
            let error = RepositoryError::new(format!(
                "HNS assigned {ip} to a guest, but the VMLord network serves {}-{}; \
                 the guest cannot be offered its address",
                config.pool_start, config.pool_end
            ));
            log::error!("{error}");
            return Err(error);
        }

        if self.reserved.get(&mac) == Some(&ip) {
            return Ok(());
        }

        if let Some(previous) = self.reserved.remove(&mac) {
            self.server.remove_reservation(&mac);
            log::debug!("the guest at {mac:02x?} moved from {previous} to {ip}");
        }

        let holder = self
            .reserved
            .iter()
            .find(|(_, held)| **held == ip)
            .map(|(holder, _)| *holder);
        if let Some(holder) = holder {
            self.reserved.remove(&holder);
            self.server.remove_reservation(&holder);
            log::info!("HNS moved {ip} from the guest at {holder:02x?} to the one at {mac:02x?}");
        }

        self.server.reserve_ip(mac, ip);
        self.reserved.insert(mac, ip);
        log::debug!("the guest at {mac:02x?} is served {ip}");
        Ok(())
    }

    /// The reply to `datagram`, and where it has to go.
    ///
    /// The MAC is checked before `handle_packet` rather than before the reply
    /// is sent. The socket is bound to `0.0.0.0:67` -- on Windows a socket
    /// bound to a vNIC's unicast address does not receive broadcast Discovers
    /// -- so DHCP broadcasts from every host interface arrive here, including
    /// the host's own LAN. Dropping them early keeps a stranger from being sent
    /// a NAK that would break its configuration, and keeps the server's pool
    /// and lease table free of addresses HNS never assigned.
    fn handle(&mut self, datagram: &[u8]) -> Option<(Vec<u8>, SocketAddrV4)> {
        let packet = match DhcpPacket::parse(datagram) {
            Ok(packet) => packet,
            Err(error) => {
                log::debug!("a datagram on the DHCP port was not a DHCP packet: {error}");
                return None;
            }
        };

        let mac = packet.client_mac();
        if !self.reserved.contains_key(&mac) {
            log::debug!(
                "a DHCP request from {mac:02x?} is not from a guest of the VMLord network; \
                 it is left unanswered"
            );
            return None;
        }

        match self.server.handle_packet(datagram) {
            Ok(Some(reply)) => Some((reply, reply_target(&packet))),
            Ok(None) => None,
            Err(error) => {
                log::warn!("the DHCP request from {mac:02x?} could not be answered: {error}");
                None
            }
        }
    }
}

/// Where the reply to `packet` goes.
///
/// A guest that already has an address is renewing and can be answered where it
/// asked from; one that has none can only be reached by broadcast.
fn reply_target(packet: &DhcpPacket) -> SocketAddrV4 {
    if packet.ciaddr.is_unspecified() {
        SocketAddrV4::new(Ipv4Addr::BROADCAST, DHCP_CLIENT_PORT)
    } else {
        SocketAddrV4::new(packet.ciaddr, DHCP_CLIENT_PORT)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::{Ipv4Addr, SocketAddrV4},
    };

    use arcbox_dhcp::{DhcpMessageType, DhcpPacket};

    use super::{State, dhcp_config, endpoint_subnet, netmask, parse_mac};
    use crate::{hcn_endpoint::EndpointAddress, subnet::Ipv4Subnet};

    /// A state serving 172.22.42.0/24, the first candidate subnet.
    fn state() -> State {
        State::new(dhcp_config(
            Ipv4Subnet::new(Ipv4Addr::new(172, 22, 42, 0), 24),
            vec![Ipv4Addr::new(1, 1, 1, 1)],
        ))
    }

    const GUEST_MAC: [u8; 6] = [0x00, 0x15, 0x5d, 0x01, 0x02, 0x03];
    const OTHER_MAC: [u8; 6] = [0x00, 0x15, 0x5d, 0x0a, 0x0b, 0x0c];

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(172, 22, 42, last)
    }

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

    #[test]
    fn reserving_the_same_pair_twice_changes_nothing() {
        let mut state = state();

        state.reserve(GUEST_MAC, ip(5)).expect("first reservation");
        state
            .reserve(GUEST_MAC, ip(5))
            .expect("a repeated reservation must not fail");

        assert_eq!(state.reserved, HashMap::from([(GUEST_MAC, ip(5))]));
    }

    #[test]
    fn a_guest_whose_address_changed_keeps_only_the_new_one() {
        let mut state = state();

        state.reserve(GUEST_MAC, ip(5)).expect("first reservation");
        state
            .reserve(GUEST_MAC, ip(9))
            .expect("a new address for the same MAC must replace the old one");

        assert_eq!(state.reserved, HashMap::from([(GUEST_MAC, ip(9))]));
    }

    #[test]
    fn an_address_that_moved_to_another_guest_follows_it() {
        // A VM is deleted and HNS gives its address to another VM's new
        // endpoint. Reserving it for the new MAC must not fail.
        let mut state = state();
        state.reserve(GUEST_MAC, ip(5)).expect("first reservation");

        state
            .reserve(OTHER_MAC, ip(5))
            .expect("an address HNS moved to another endpoint must be reservable");

        assert_eq!(state.reserved, HashMap::from([(OTHER_MAC, ip(5))]));
    }

    #[test]
    fn two_guests_keep_their_own_addresses() {
        let mut state = state();

        state.reserve(GUEST_MAC, ip(5)).expect("first reservation");
        state.reserve(OTHER_MAC, ip(6)).expect("second reservation");

        assert_eq!(
            state.reserved,
            HashMap::from([(GUEST_MAC, ip(5)), (OTHER_MAC, ip(6))])
        );
    }

    #[test]
    fn an_address_outside_the_subnet_is_refused_rather_than_reserved() {
        // `DhcpServer::reserve_ip` panics on an address outside its pool, and
        // its allocator cannot be asked beforehand, so the check lives here.
        let mut state = state();

        let error = state
            .reserve(GUEST_MAC, Ipv4Addr::new(10, 0, 0, 5))
            .expect_err("an address outside the subnet must be refused");

        assert!(error.to_string().contains("10.0.0.5"), "{error}");
        assert!(state.reserved.is_empty());
    }

    /// Builds a client datagram: `arcbox`'s own `serialize` writes server
    /// options and never option 50, so a Request has to be built by hand.
    fn datagram(
        message_type: DhcpMessageType,
        mac: [u8; 6],
        requested: Option<Ipv4Addr>,
        ciaddr: Ipv4Addr,
    ) -> Vec<u8> {
        let mut data = vec![0u8; 240];
        data[0] = 1; // BOOTREQUEST
        data[1] = 1; // Ethernet
        data[2] = 6; // MAC length
        data[4..8].copy_from_slice(&0x1234_5678_u32.to_be_bytes()); // xid
        data[12..16].copy_from_slice(&ciaddr.octets());
        data[28..34].copy_from_slice(&mac);
        data[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie
        data.extend_from_slice(&[53, 1, message_type as u8]);
        if let Some(requested) = requested {
            data.push(50);
            data.push(4);
            data.extend_from_slice(&requested.octets());
        }
        data.push(255);
        data
    }

    fn reply(state: &mut State, datagram: &[u8]) -> Option<DhcpPacket> {
        state
            .handle(datagram)
            .map(|(bytes, _)| DhcpPacket::parse(&bytes).expect("a reply should be a DHCP packet"))
    }

    #[test]
    fn a_discover_is_offered_the_address_hns_assigned() {
        let mut state = state();
        state.reserve(GUEST_MAC, ip(5)).expect("reservation");

        let offer = reply(
            &mut state,
            &datagram(
                DhcpMessageType::Discover,
                GUEST_MAC,
                None,
                Ipv4Addr::UNSPECIFIED,
            ),
        )
        .expect("a guest of the VMLord network should be answered");

        assert_eq!(offer.message_type, Some(DhcpMessageType::Offer));
        assert_eq!(offer.yiaddr, ip(5));
    }

    #[test]
    fn a_request_for_the_assigned_address_is_acknowledged() {
        let mut state = state();
        state.reserve(GUEST_MAC, ip(5)).expect("reservation");

        let ack = reply(
            &mut state,
            &datagram(
                DhcpMessageType::Request,
                GUEST_MAC,
                Some(ip(5)),
                Ipv4Addr::UNSPECIFIED,
            ),
        )
        .expect("a guest asking for its own address should be answered");

        assert_eq!(ack.message_type, Some(DhcpMessageType::Ack));
        assert_eq!(ack.yiaddr, ip(5));
    }

    #[test]
    fn a_request_for_a_stale_address_is_refused() {
        // The endpoint was replaced and HNS assigned a different address; the
        // guest still asks for the one it remembers.
        let mut state = state();
        state.reserve(GUEST_MAC, ip(9)).expect("reservation");

        let nak = reply(
            &mut state,
            &datagram(
                DhcpMessageType::Request,
                GUEST_MAC,
                Some(ip(5)),
                Ipv4Addr::UNSPECIFIED,
            ),
        )
        .expect("a guest asking for a stale address should be told to stop using it");

        assert_eq!(nak.message_type, Some(DhcpMessageType::Nak));
    }

    #[test]
    fn a_guest_of_another_network_is_not_answered_at_all() {
        // The socket is bound to 0.0.0.0:67 and sees DHCP broadcasts from every
        // host interface. Answering one -- even with a NAK -- would break a
        // machine that has nothing to do with VMLord.
        let mut state = state();
        state.reserve(GUEST_MAC, ip(5)).expect("reservation");

        assert!(
            state
                .handle(&datagram(
                    DhcpMessageType::Discover,
                    OTHER_MAC,
                    None,
                    Ipv4Addr::UNSPECIFIED,
                ))
                .is_none()
        );
        assert!(
            state
                .handle(&datagram(
                    DhcpMessageType::Request,
                    OTHER_MAC,
                    Some(ip(6)),
                    Ipv4Addr::UNSPECIFIED,
                ))
                .is_none()
        );
    }

    #[test]
    fn a_foreign_guest_never_takes_an_address_out_of_the_pool() {
        // The reason the check stands before `handle_packet`: an address the
        // server handed to a stranger would later make `reserve_ip` panic.
        let mut state = state();
        state.reserve(GUEST_MAC, ip(5)).expect("reservation");

        state.handle(&datagram(
            DhcpMessageType::Discover,
            OTHER_MAC,
            None,
            Ipv4Addr::UNSPECIFIED,
        ));

        assert_eq!(state.reserved, HashMap::from([(GUEST_MAC, ip(5))]));
        assert!(state.server.leases().is_empty());
    }

    #[test]
    fn a_malformed_datagram_is_dropped() {
        let mut state = state();
        state.reserve(GUEST_MAC, ip(5)).expect("reservation");

        assert!(state.handle(b"not a dhcp packet").is_none());
    }

    #[test]
    fn a_guest_without_an_address_is_answered_by_broadcast() {
        // Nothing can deliver a unicast reply to a guest that has no address:
        // no ARP entry for it exists and VMLord cannot create one.
        let packet = DhcpPacket::parse(&datagram(
            DhcpMessageType::Discover,
            GUEST_MAC,
            None,
            Ipv4Addr::UNSPECIFIED,
        ))
        .expect("the test datagram should parse");

        assert_eq!(
            super::reply_target(&packet),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 68)
        );
    }

    #[test]
    fn a_renewing_guest_is_answered_where_it_asked_from() {
        let packet = DhcpPacket::parse(&datagram(
            DhcpMessageType::Request,
            GUEST_MAC,
            None,
            ip(5),
        ))
        .expect("the test datagram should parse");

        assert_eq!(super::reply_target(&packet), SocketAddrV4::new(ip(5), 68));
    }
}
