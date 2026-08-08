# TASK-47: встроенный DHCP-сервер — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Linux-гость на NAT-сети VMLord получает по DHCP ровно тот адрес, который HNS уже назначил его endpoint'у.

**Architecture:** Крейт `arcbox-dhcp` принимает решения по протоколу (`handle_packet`: байты на входе, байты на выходе), VMLord владеет сокетом, потоком и картой резерваций. Пакет с MAC, которого нет в резервациях, отбрасывается до `handle_packet`, поэтому пул `arcbox` никогда не расходуется мимо HNS IPAM и `reserve_ip` не может упасть в панику. Сервер поднимается один раз, при первом старте NAT VM, и живёт до конца процесса.

**Tech Stack:** Rust 2024, `arcbox-dhcp` 0.6, `std::net::UdpSocket`, `windows-rs` (`GetAdaptersAddresses`), крейт `vmlord-platform`.

**Спека:** `docs/superpowers/specs/2026-08-08-hns-dhcp-server-design.md`

## Global Constraints

- Весь новый код — в `crates/platform`, единственном месте, где разрешены вызовы Windows API.
- `unsafe` только внутри `crates/platform`; там `unsafe_code = "allow"`, в остальном workspace — `deny`.
- Каждый блок `unsafe` сопровождается комментарием `// SAFETY:`, как в `subnet.rs` и `hcn_endpoint.rs`.
- Логирование через крейт `log`, уровни DEBUG..ERROR. `tracing` из `arcbox-dhcp` подписчика не получает и молчит — это ожидаемо.
- Ошибки на границе — `vmlord_core::RepositoryError`; перед возвратом ошибка пишется в `log::error!`.
- Комментарии, doc-комментарии и сообщения об ошибках — на английском, как весь код репозитория.
- Сборка и тесты: `cargo build --target=x86_64-pc-windows-gnu`, `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`. Из WSL это работает: .exe запускается через interop.
- Коммиты: `TASK-47: <comment>`, автор задаётся переменными окружения `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local`.
- Ветка: `task-47-hns-dhcp-server`.
- MSRV `arcbox-dhcp` — 1.96; тулчейн проекта — 1.97.1.

## File Structure

| Файл | Ответственность |
|---|---|
| `crates/platform/Cargo.toml` | зависимость `arcbox-dhcp = "0.6"` |
| `crates/platform/src/subnet.rs` | + аксессор `Ipv4Subnet::prefix_length` |
| `crates/platform/src/dhcp.rs` | новый: конфигурация, карта резерваций, разбор датаграмм, сокет и поток |
| `crates/platform/src/host_dns.rs` | новый: DNS-серверы хоста через `GetAdaptersAddresses` и их отбор |
| `crates/platform/src/lib.rs` | `mod dhcp;`, `mod host_dns;` |
| `crates/platform/src/start.rs` | адрес endpoint'а в `VmNetworkAdapter`, шов `DhcpRegistrar`, продакшн-регистрация |
| `crates/platform/tests/hyperv.rs` | `#[ignore]`-тест на живом Hyper-V |
| `ARCHITECTURE.md` | раздел про DHCP |

---

### Task 1: Конфигурация DHCP из адреса endpoint'а

**Files:**
- Modify: `crates/platform/Cargo.toml`
- Modify: `crates/platform/src/subnet.rs` (добавить аксессор рядом с `gateway`)
- Create: `crates/platform/src/dhcp.rs`
- Modify: `crates/platform/src/lib.rs`

**Interfaces:**
- Consumes: `crate::subnet::Ipv4Subnet`, `crate::hcn_endpoint::EndpointAddress`.
- Produces:
  - `Ipv4Subnet::prefix_length(self) -> u8`
  - `dhcp::netmask(prefix_length: u8) -> Ipv4Addr`
  - `dhcp::parse_mac(text: &str) -> Option<[u8; 6]>`
  - `dhcp::endpoint_subnet(address: &EndpointAddress) -> Result<Ipv4Subnet, RepositoryError>`
  - `dhcp::dhcp_config(subnet: Ipv4Subnet, dns_servers: Vec<Ipv4Addr>) -> DhcpConfig`

- [ ] **Step 1: Добавить зависимость**

В `crates/platform/Cargo.toml`, в секцию `[dependencies]` после `vmlord-core`:

```toml
arcbox-dhcp = "0.6"
```

- [ ] **Step 2: Проверить, что зависимость встаёт**

Run: `cargo build -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: сборка проходит, в дерево приезжают только `arcbox-dhcp`, `thiserror`, `tracing`.

- [ ] **Step 3: Написать падающие тесты**

Создать `crates/platform/src/dhcp.rs` с одним лишь тестовым модулем:

```rust
//! VMLord's DHCP server for the shared NAT network.

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
```

В `crates/platform/src/lib.rs` добавить `mod dhcp;` в алфавитном порядке — между `mod delete;` и `mod enumerate;`.

- [ ] **Step 4: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: FAIL — компиляция не проходит, `dhcp_config`, `endpoint_subnet`, `netmask`, `parse_mac` и `Ipv4Subnet::prefix_length` не существуют.

- [ ] **Step 5: Добавить аксессор длины префикса**

В `crates/platform/src/subnet.rs`, сразу после метода `gateway`:

```rust
    /// The number of bits the subnet's prefix fixes.
    #[must_use]
    pub fn prefix_length(self) -> u8 {
        self.prefix_length
    }
```

- [ ] **Step 6: Реализовать помощники**

В начало `crates/platform/src/dhcp.rs`, перед тестовым модулем:

```rust
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
```

- [ ] **Step 7: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: PASS, 6 тестов.

- [ ] **Step 8: Проверить clippy**

Run: `cargo clippy -p vmlord-platform --target=x86_64-pc-windows-gnu --all-targets`
Expected: без предупреждений.

- [ ] **Step 9: Коммит**

```bash
git add crates/platform/Cargo.toml Cargo.lock crates/platform/src/dhcp.rs crates/platform/src/lib.rs crates/platform/src/subnet.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Derive the DHCP configuration from the address HNS assigned"
```

---

### Task 2: Резервации, которые не могут уронить процесс

**Files:**
- Modify: `crates/platform/src/dhcp.rs`

**Interfaces:**
- Consumes: `dhcp_config` из Task 1.
- Produces:
  - `struct State { server: DhcpServer, reserved: HashMap<[u8; 6], Ipv4Addr> }`
  - `State::new(config: DhcpConfig) -> State`
  - `State::reserve(&mut self, mac: [u8; 6], ip: Ipv4Addr) -> Result<(), RepositoryError>`

- [ ] **Step 1: Написать падающие тесты**

Добавить в тестовый модуль `dhcp.rs`:

```rust
    use std::collections::HashMap;

    use super::State;

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
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: FAIL — `State` не существует.

- [ ] **Step 3: Реализовать `State::reserve`**

Добавить в `dhcp.rs` после `dhcp_config`; в импорты добавить `std::collections::HashMap` и `arcbox_dhcp::DhcpServer`:

```rust
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
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: PASS, 11 тестов.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/src/dhcp.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Reserve an address for the MAC HNS gave it to"
```

---

### Task 3: Ответ только своим, и только адресом HNS

**Files:**
- Modify: `crates/platform/src/dhcp.rs`

**Interfaces:**
- Consumes: `State` из Task 2.
- Produces:
  - `State::handle(&mut self, datagram: &[u8]) -> Option<(Vec<u8>, SocketAddrV4)>`
  - `dhcp::reply_target(packet: &DhcpPacket) -> SocketAddrV4`

- [ ] **Step 1: Написать падающие тесты**

Добавить в тестовый модуль `dhcp.rs`:

```rust
    use arcbox_dhcp::{DhcpMessageType, DhcpPacket};

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

        assert_eq!(
            super::reply_target(&packet),
            SocketAddrV4::new(ip(5), 68)
        );
    }
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: FAIL — `State::handle` и `reply_target` не существуют.

- [ ] **Step 3: Реализовать разбор датаграммы**

В импорты `dhcp.rs` добавить `arcbox_dhcp::{DHCP_CLIENT_PORT, DhcpPacket}` и `std::net::SocketAddrV4`. Добавить метод в `impl State`, после `reserve`:

```rust
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
```

И свободную функцию после `impl State`:

```rust
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
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: PASS, 19 тестов.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/src/dhcp.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Answer the guests of the VMLord network and no one else"
```

---

### Task 4: DNS-серверы хоста

**Files:**
- Create: `crates/platform/src/host_dns.rs`
- Modify: `crates/platform/src/lib.rs`

**Interfaces:**
- Consumes: `crate::subnet::Ipv4Subnet`.
- Produces: `host_dns::dns_servers(subnet: Ipv4Subnet) -> Vec<Ipv4Addr>`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/platform/src/host_dns.rs` с тестовым модулем:

```rust
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
        let configured = vec![
            Ipv4Addr::new(172, 22, 42, 1),
            Ipv4Addr::new(192, 168, 1, 1),
        ];

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

        assert_eq!(usable(configured, vmlord()), vec![Ipv4Addr::new(9, 9, 9, 9)]);
    }

    #[test]
    fn a_resolver_listed_twice_is_offered_once() {
        let configured = vec![
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(192, 168, 1, 1),
        ];

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

    #[test]
    fn the_fallback_is_the_two_documented_public_resolvers() {
        assert_eq!(
            FALLBACK_DNS,
            [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
    }
}
```

В `crates/platform/src/lib.rs` добавить `mod host_dns;` между `mod hcs_config;` и `mod layout;`.

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu host_dns::`
Expected: FAIL — `usable` и `FALLBACK_DNS` не существуют.

- [ ] **Step 3: Реализовать отбор и запрос к Windows**

В начало `crates/platform/src/host_dns.rs`, перед тестовым модулем:

```rust
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

use crate::subnet::Ipv4Subnet;

/// What a guest is given when the host has no resolver it can use.
///
/// Reached on a host whose only resolvers are loopback ones -- a local DNS
/// proxy the guest cannot route to. Working name resolution through a public
/// resolver beats none at all.
const FALLBACK_DNS: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];

/// The resolvers a guest of `subnet` is offered.
pub(crate) fn dns_servers(subnet: Ipv4Subnet) -> Vec<Ipv4Addr> {
    let configured = match host_dns_servers() {
        Ok(servers) => servers,
        Err(error) => {
            log::warn!(
                "the host's DNS servers could not be read ({error}); \
                 guests are offered the public resolvers instead"
            );
            Vec::new()
        }
    };

    let servers = usable(configured, subnet);
    log::debug!(
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
            log::debug!("the host resolver {server} cannot serve a guest of the VMLord network");
            continue;
        }
        if !servers.contains(&server) {
            servers.push(server);
        }
    }

    if servers.is_empty() {
        log::warn!(
            "the host has no DNS server a guest could reach; \
             guests are offered the public resolvers instead"
        );
        return FALLBACK_DNS.to_vec();
    }
    servers
}

/// Every IPv4 DNS server the host's adapters are configured with.
fn host_dns_servers() -> Result<Vec<Ipv4Addr>, RepositoryError> {
    let buffer = AdapterAddresses::query()?;
    Ok(buffer.ipv4_dns_servers())
}

/// An owned `GetAdaptersAddresses` buffer.
struct AdapterAddresses(Vec<u8>);

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
            let mut buffer = vec![0u8; size as usize];
            // SAFETY: `buffer` is `size` bytes long and stays alive for the
            // call, and `size` is valid for reading and writing. Windows fills
            // the buffer with a linked list whose pointers are into it.
            let status = WIN32_ERROR(unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC.0.into(),
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

        let error = crate::error::hresult_to_repository_error(
            "read the host adapter addresses",
            None,
            last.to_hresult().0,
        );
        log::error!("{error}");
        Err(error)
    }

    /// Walks the adapters and collects their IPv4 DNS servers.
    fn ipv4_dns_servers(&self) -> Vec<Ipv4Addr> {
        let mut servers = Vec::new();
        let mut adapter = self.0.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();

        while !adapter.is_null() {
            // SAFETY: `adapter` is either the head of the list Windows wrote
            // into the owned buffer or a `Next` pointer from it.
            let current = unsafe { &*adapter };
            let mut entry = current.FirstDnsServerAddress;
            while !entry.is_null() {
                // SAFETY: `entry` comes from the same buffer and is non-null.
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
                        // always readable as the address in network order.
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
```

Ничего другого в файл добавлять не нужно: `dns_servers` — единственная точка входа, всё остальное приватно.

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu host_dns::`
Expected: PASS, 6 тестов.

- [ ] **Step 5: Проверить на живом хосте, что список не пустой**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu host_dns:: -- --nocapture`
Expected: PASS. Если реализация `query` расходится с ожиданиями Windows, это всплывёт в Task 7 на живом Hyper-V; отдельного unit-теста на FFI нет намеренно — он проверял бы Windows, а не VMLord.

- [ ] **Step 6: Коммит**

```bash
git add crates/platform/src/host_dns.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Offer the guest the resolvers the host itself uses"
```

---

### Task 5: Сокет, поток и диагностика занятого порта

**Files:**
- Modify: `crates/platform/src/dhcp.rs`

**Interfaces:**
- Consumes: `State` (Task 2–3), `host_dns::dns_servers` (Task 4), `endpoint_subnet`/`dhcp_config` (Task 1).
- Produces:
  - `dhcp::DhcpService`
  - `DhcpService::start(address: &EndpointAddress) -> Result<DhcpService, RepositoryError>`
  - `DhcpService::reserve(&self, mac: &str, ip: &EndpointAddress) -> Result<(), RepositoryError>`

- [ ] **Step 1: Написать падающий тест на диагностику**

Добавить в тестовый модуль `dhcp.rs`:

```rust
    use std::io;

    use super::bind_error;

    #[test]
    fn an_occupied_port_names_what_could_be_holding_it() {
        let error = bind_error(&io::Error::from(io::ErrorKind::AddrInUse));

        let message = error.to_string();
        assert!(message.contains("67"), "{message}");
        assert!(message.contains("Internet Connection Sharing"), "{message}");
    }

    #[test]
    fn a_refused_bind_names_privileges_and_the_firewall() {
        let error = bind_error(&io::Error::from(io::ErrorKind::PermissionDenied));

        let message = error.to_string();
        assert!(message.contains("firewall"), "{message}");
        assert!(message.contains("elevated"), "{message}");
    }

    #[test]
    fn any_other_failure_is_reported_as_it_came() {
        let error = bind_error(&io::Error::other("something else entirely"));

        assert!(
            error.to_string().contains("something else entirely"),
            "{error}"
        );
    }
```

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: FAIL — `bind_error` не существует.

- [ ] **Step 3: Реализовать сервис**

В импорты `dhcp.rs` добавить:

```rust
use std::{
    io,
    net::UdpSocket,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use arcbox_dhcp::DHCP_SERVER_PORT;

use crate::host_dns;
```

И после `reply_target`:

```rust
/// How long the worker waits for a packet before it re-checks whether it should
/// still be running.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The largest datagram the server reads; a DHCP packet is far smaller.
const RECEIVE_BUFFER: usize = 1500;

/// VMLord's DHCP server: a socket, a worker thread and what they serve.
///
/// One per process, started with the first NAT VM and stopped when the process
/// ends. A guest therefore keeps its address after VMLord is closed but has
/// nothing to renew against; moving the server into a Windows service or a tray
/// application is deliberately left out of this change.
pub(crate) struct DhcpService {
    state: Arc<Mutex<State>>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DhcpService {
    /// Binds the server to the subnet `address` belongs to and starts serving.
    pub(crate) fn start(address: &EndpointAddress) -> Result<Self, RepositoryError> {
        let subnet = endpoint_subnet(address)?;
        let socket = bind()?;
        let state = Arc::new(Mutex::new(State::new(dhcp_config(
            subnet,
            host_dns::dns_servers(subnet),
        ))));

        let running = Arc::new(AtomicBool::new(true));
        let worker = thread::Builder::new()
            .name("vmlord-dhcp".to_owned())
            .spawn({
                let state = Arc::clone(&state);
                let running = Arc::clone(&running);
                move || serve(&socket, &state, &running)
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the DHCP server thread could not be started: {error}"
                ));
                log::error!("{error}");
                error
            })?;

        log::info!(
            "the VMLord DHCP server is serving {subnet} from {}",
            subnet.gateway()
        );
        Ok(Self {
            state,
            running,
            worker: Some(worker),
        })
    }

    /// Serves `ip` to the guest at `mac`.
    pub(crate) fn reserve(&self, mac: &str, ip: &EndpointAddress) -> Result<(), RepositoryError> {
        let parsed = parse_mac(mac).ok_or_else(|| {
            let error = RepositoryError::new(format!(
                "HNS reported \"{mac}\" as an endpoint's MAC address, which cannot be parsed"
            ));
            log::error!("{error}");
            error
        })?;
        let address: Ipv4Addr = ip.ip_address.parse().map_err(|_| {
            let error = RepositoryError::new(format!(
                "HNS reported \"{}\" as an endpoint address, which is not an IPv4 address",
                ip.ip_address
            ));
            log::error!("{error}");
            error
        })?;

        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reserve(parsed, address)
    }
}

impl Drop for DhcpService {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        log::info!("the VMLord DHCP server stopped");
    }
}

/// Binds the DHCP port.
///
/// `SO_REUSEADDR` is deliberately not set: on Windows it lets a second server
/// take over a port that is already served, and two DHCP servers answering the
/// same guests is worse than a start that fails with a diagnosis.
fn bind() -> Result<UdpSocket, RepositoryError> {
    let socket =
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DHCP_SERVER_PORT)).map_err(|error| {
            let error = bind_error(&error);
            log::error!("{error}");
            error
        })?;
    socket.set_broadcast(true).map_err(|error| {
        let error =
            RepositoryError::new(format!("the DHCP socket could not be made broadcast: {error}"));
        log::error!("{error}");
        error
    })?;
    socket
        .set_read_timeout(Some(POLL_INTERVAL))
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "the DHCP socket could not be given a read timeout: {error}"
            ));
            log::error!("{error}");
            error
        })?;
    Ok(socket)
}

/// Turns a failure to bind UDP 67 into something a user can act on.
fn bind_error(error: &io::Error) -> RepositoryError {
    match error.kind() {
        io::ErrorKind::AddrInUse => RepositoryError::new(format!(
            "UDP port 67 is already served on this host, so VMLord cannot answer its guests: \
             {error}. Internet Connection Sharing, the Hyper-V Default Switch or a third-party \
             DHCP server typically holds it"
        )),
        io::ErrorKind::PermissionDenied => RepositoryError::new(format!(
            "VMLord was not allowed to serve DHCP on UDP port 67: {error}. \
             Run VMLord elevated and allow it through the firewall"
        )),
        _ => RepositoryError::new(format!("the DHCP server could not bind UDP port 67: {error}")),
    }
}

/// Serves packets until the service is dropped.
fn serve(socket: &UdpSocket, state: &Mutex<State>, running: &AtomicBool) {
    let mut buffer = [0u8; RECEIVE_BUFFER];

    while running.load(Ordering::Relaxed) {
        let received = match socket.recv_from(&mut buffer) {
            Ok((length, _)) => length,
            // The read timeout is what lets the loop notice it should stop;
            // Windows reports it as `TimedOut` rather than `WouldBlock`.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => {
                log::warn!("the DHCP server could not read from its socket: {error}");
                continue;
            }
        };

        let reply = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handle(&buffer[..received]);

        if let Some((reply, target)) = reply
            && let Err(error) = socket.send_to(&reply, target)
        {
            log::warn!("a DHCP reply to {target} could not be sent: {error}");
        }
    }
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: PASS, 22 теста. Ни один из них не открывает сокет: бинд UDP 67 зависит от того, что делает хост, и его проверяет `#[ignore]`-тест из Task 7.

- [ ] **Step 5: Проверить clippy**

Run: `cargo clippy -p vmlord-platform --target=x86_64-pc-windows-gnu --all-targets`
Expected: без предупреждений.

- [ ] **Step 6: Коммит**

```bash
git add crates/platform/src/dhcp.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Serve DHCP from a thread of the VMLord process"
```

---

### Task 6: Старт VM поднимает сервер и регистрирует гостя

**Files:**
- Modify: `crates/platform/src/start.rs`
- Modify: `crates/platform/src/dhcp.rs`

**Interfaces:**
- Consumes: `DhcpService` из Task 5, `MetadataStore::list`, `HcnEndpoint::open_if_present`, `HcnEndpoint::mac_address`, `HcnEndpoint::address`.
- Produces:
  - `VmNetworkAdapter { endpoint_id: Uuid, mac_address: String, address: Option<EndpointAddress> }`
  - `type DhcpRegistrar = Box<dyn Fn(&MetadataStore, &str, &EndpointAddress) -> Result<(), RepositoryError>>`
  - `dhcp::register(service: &Mutex<Option<DhcpService>>, store: &MetadataStore, mac: &str, address: &EndpointAddress) -> Result<(), RepositoryError>`

- [ ] **Step 1: Написать падающие тесты**

В тестовый модуль `crates/platform/src/start.rs`:

добавить в `struct Calls` поле

```rust
        dhcp: Arc<Mutex<Vec<(String, EndpointAddress)>>>,
```

добавить в `struct Behavior` поле

```rust
        fail_dhcp: bool,
```

добавить рядом с `MAC_ADDRESS` константу

```rust
    /// The address the test endpoint provider reports for its endpoint.
    fn endpoint_address() -> EndpointAddress {
        EndpointAddress {
            ip_address: "172.22.42.5".to_owned(),
            prefix_length: 24,
        }
    }
```

в `fn pipeline` вернуть из провайдера endpoint'а адрес

```rust
                    Ok(VmNetworkAdapter {
                        endpoint_id,
                        mac_address: MAC_ADDRESS.to_owned(),
                        address: Some(endpoint_address()),
                    })
```

и передать четвёртым аргументом в `VmStartPipeline::for_test`

```rust
            {
                let calls = calls.clone();
                move |_store: &MetadataStore, mac: &str, address: &EndpointAddress| {
                    calls.steps.lock().unwrap().push("dhcp");
                    calls
                        .dhcp
                        .lock()
                        .unwrap()
                        .push((mac.to_owned(), address.clone()));
                    if behavior.fail_dhcp {
                        return Err(RepositoryError::new("injected DHCP failure"));
                    }
                    Ok(())
                }
            },
```

и добавить тесты:

```rust
    #[test]
    fn a_nat_vm_is_registered_with_dhcp_before_it_starts() {
        let fixture = fixture_with("dhcp-registers", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert_eq!(
            *calls.dhcp.lock().unwrap(),
            vec![(MAC_ADDRESS.to_owned(), endpoint_address())]
        );
        let steps = calls.steps.lock().unwrap().clone();
        let dhcp = steps.iter().position(|step| *step == "dhcp").unwrap();
        let start = steps.iter().position(|step| *step == "start").unwrap();
        assert!(
            dhcp < start,
            "the guest must be able to get its address the moment it boots: {steps:?}"
        );
    }

    #[test]
    fn a_vm_without_a_network_is_not_registered_with_dhcp() {
        let fixture = fixture("dhcp-none");
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert!(calls.dhcp.lock().unwrap().is_empty());
    }

    #[test]
    fn a_dhcp_failure_fails_the_start() {
        // A VM that asked for a network and cannot be told its address would
        // come up with an adapter and no configuration at all.
        let fixture = fixture_with("dhcp-fails", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_dhcp: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a start that cannot serve the guest its address must fail");

        assert!(error.to_string().contains("injected DHCP failure"), "{error}");
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn an_endpoint_without_an_address_fails_the_start() {
        let fixture = fixture_with("dhcp-no-address", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let pipeline = VmStartPipeline::for_test(
            |_id: &str, _path: &Path| Ok(()),
            |_id: &str, _configuration: &str| Ok(()),
            |_vm_name: &str, _recorded: Option<Uuid>, _policy: EndpointPolicy| {
                Ok(VmNetworkAdapter {
                    endpoint_id: NEW_ENDPOINT_ID,
                    mac_address: MAC_ADDRESS.to_owned(),
                    address: None,
                })
            },
            {
                let calls = calls.clone();
                move |_store: &MetadataStore, mac: &str, address: &EndpointAddress| {
                    calls
                        .dhcp
                        .lock()
                        .unwrap()
                        .push((mac.to_owned(), address.clone()));
                    Ok(())
                }
            },
        );

        let error = pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("an endpoint HNS reports no address for must fail the start");

        assert!(error.to_string().contains(&NEW_ENDPOINT_ID.to_string()), "{error}");
        assert!(calls.dhcp.lock().unwrap().is_empty());
    }
```

`EndpointAddress` должен выводить `Clone` и `PartialEq` — он уже выводит оба.

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu start::`
Expected: FAIL — у `VmNetworkAdapter` нет поля `address`, а у `for_test` — четвёртого аргумента.

- [ ] **Step 3: Расширить `VmNetworkAdapter` и шов**

В `crates/platform/src/start.rs`:

```rust
/// What a VM needs written into its configuration to reach the network.
pub(crate) struct VmNetworkAdapter {
    pub(crate) endpoint_id: Uuid,
    pub(crate) mac_address: String,
    /// The address HNS assigned to the endpoint, which the DHCP server is what
    /// actually delivers to the guest.
    pub(crate) address: Option<EndpointAddress>,
}
```

Рядом с остальными псевдонимами типов:

```rust
type DhcpRegistrar =
    Box<dyn Fn(&MetadataStore, &str, &EndpointAddress) -> Result<(), RepositoryError>>;
```

В `struct VmStartPipeline` добавить поле `dhcp_registrar: DhcpRegistrar;` в `production()` — `dhcp_registrar: dhcp::registrar(),`; в `for_test` — четвёртый параметр

```rust
        dhcp_registrar: impl Fn(&MetadataStore, &str, &EndpointAddress) -> Result<(), RepositoryError>
        + 'static,
```

и `dhcp_registrar: Box::new(dhcp_registrar),` в конструкторе.

В `ensure_endpoint`, в возвращаемое значение:

```rust
    Ok(VmNetworkAdapter {
        endpoint_id,
        mac_address: endpoint.mac_address()?,
        address: endpoint.address()?,
    })
```

- [ ] **Step 4: Вызвать регистрацию в `attach_network`**

В `attach_network`, между записью конфигурации и финальным `log::info!`:

```rust
        let Some(address) = adapter.address.as_ref() else {
            let error = RepositoryError::new(format!(
                "HNS reports no address for endpoint {} of VM \"{}\", so the guest cannot be \
                 told one over DHCP",
                adapter.endpoint_id, mapping.vm_name
            ));
            log::error!("{error}");
            return Err(error);
        };
        (self.dhcp_registrar)(store, &adapter.mac_address, address)?;
```

- [ ] **Step 5: Реализовать продакшн-регистрацию**

В `crates/platform/src/dhcp.rs`, после `impl Drop for DhcpService`:

```rust
/// The production registrar: it starts the server on the first NAT VM and
/// reserves the guest's address on every start after that.
///
/// The service is held by the closure rather than by a global: one
/// [`crate::VmStartPipeline`] exists per process, which is exactly the lifetime
/// the server is meant to have.
pub(crate) fn registrar()
-> Box<dyn Fn(&MetadataStore, &str, &EndpointAddress) -> Result<(), RepositoryError>> {
    let service: Arc<Mutex<Option<DhcpService>>> = Arc::new(Mutex::new(None));
    Box::new(move |store, mac, address| register(&service, store, mac, address))
}

fn register(
    service: &Mutex<Option<DhcpService>>,
    store: &MetadataStore,
    mac: &str,
    address: &EndpointAddress,
) -> Result<(), RepositoryError> {
    let mut service = service
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if service.is_none() {
        let started = DhcpService::start(address)?;
        // A VMLord that was restarted while its VMs kept running has to know
        // their addresses too: a guest renewing its lease is answered only if
        // its MAC is reserved, and it never asks again if it is not.
        seed(&started, store);
        *service = Some(started);
    }

    service
        .as_ref()
        .expect("the service was started above")
        .reserve(mac, address)
}

/// Reserves the address of every endpoint VMLord has recorded.
///
/// Best effort by design: an endpoint HNS no longer has, or one it reports
/// nothing usable for, costs that VM its lease renewal -- it must not cost the
/// VM being started its start.
fn seed(service: &DhcpService, store: &MetadataStore) {
    let mappings = match store.list() {
        Ok(mappings) => mappings,
        Err(error) => {
            log::warn!(
                "the recorded VMs could not be read, so only the VM being started is served \
                 by DHCP: {error}"
            );
            return;
        }
    };

    for mapping in mappings {
        let Some(endpoint_id) = mapping.endpoint_id else {
            continue;
        };
        if let Err(error) = seed_one(service, endpoint_id) {
            log::warn!(
                "the endpoint of VM \"{}\" could not be served by DHCP: {error}",
                mapping.vm_name
            );
        }
    }
}

fn seed_one(service: &DhcpService, endpoint_id: Uuid) -> Result<(), RepositoryError> {
    let Some(endpoint) = HcnEndpoint::open_if_present(endpoint_id)? else {
        return Ok(());
    };
    let Some(address) = endpoint.address()? else {
        return Ok(());
    };
    service.reserve(&endpoint.mac_address()?, &address)
}
```

В импорты `dhcp.rs` добавить `uuid::Uuid`, `crate::hcn_endpoint::HcnEndpoint` и `crate::metadata::MetadataStore`.

- [ ] **Step 6: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS — все тесты крейта, включая существующие тесты `start::`, которые пришлось дополнить четвёртым аргументом.

- [ ] **Step 7: Проверить сборку и clippy**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo clippy -p vmlord-platform --target=x86_64-pc-windows-gnu --all-targets`
Expected: без ошибок и предупреждений.

- [ ] **Step 8: Коммит**

```bash
git add crates/platform/src/start.rs crates/platform/src/dhcp.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Serve a starting VM its address before it boots"
```

---

### Task 7: Проверка на живом хосте и документация

**Files:**
- Modify: `crates/platform/src/dhcp.rs`
- Modify: `crates/platform/tests/hyperv.rs`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `DhcpService` (Task 5), `VmStartPipeline::production` (Task 6), уже существующие помощники `hyperv.rs`: `recorded_endpoint(store, vm_name) -> Result<Uuid, String>` и `endpoint_address(endpoint) -> Result<Option<EndpointAddress>, String>`.

- [ ] **Step 1: Написать `#[ignore]`-тест реального обмена на UDP 67**

Этот тест не требует Hyper-V — только свободные UDP 67 и 68. Он проверяет то, чего не видит ни один из юнит-тестов: что сокет биндится, поток отвечает и ответ действительно уходит броадкастом. Добавить в тестовый модуль `crates/platform/src/dhcp.rs`:

```rust
    use std::{net::UdpSocket as TestSocket, time::Duration};

    use super::DhcpService;

    /// Run elevated, with nothing else serving DHCP on this host:
    /// `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu -- --ignored --exact dhcp::tests::the_server_answers_a_reserved_guest_over_a_real_socket --nocapture`
    #[test]
    #[ignore = "binds UDP 67 and 68 on the host"]
    fn the_server_answers_a_reserved_guest_over_a_real_socket() {
        let assigned = EndpointAddress {
            ip_address: "172.22.42.5".to_owned(),
            prefix_length: 24,
        };

        // The client half must be listening before the request goes out: the
        // reply is a broadcast to port 68, not an answer to the sender's port.
        let client = TestSocket::bind((Ipv4Addr::UNSPECIFIED, 68))
            .expect("UDP port 68 should be free on the test host");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the client socket should take a read timeout");
        client
            .set_broadcast(true)
            .expect("the client socket should take the broadcast option");

        let service = DhcpService::start(&assigned).expect("the DHCP server should start");
        service
            .reserve("00-15-5D-01-02-03", &assigned)
            .expect("the guest should be reservable");

        client
            .send_to(
                &datagram(
                    DhcpMessageType::Discover,
                    GUEST_MAC,
                    None,
                    Ipv4Addr::UNSPECIFIED,
                ),
                (Ipv4Addr::LOCALHOST, 67),
            )
            .expect("the Discover should be sent");

        let mut buffer = [0u8; 1500];
        let (length, _) = client
            .recv_from(&mut buffer)
            .expect("the server should answer a reserved guest within five seconds");
        let offer = DhcpPacket::parse(&buffer[..length]).expect("the reply should be a DHCP packet");

        assert_eq!(offer.message_type, Some(DhcpMessageType::Offer));
        assert_eq!(offer.yiaddr, Ipv4Addr::new(172, 22, 42, 5));
    }
```

- [ ] **Step 2: Проверить, что тест компилируется и по умолчанию пропускается**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu dhcp::`
Expected: PASS, 22 пройденных и 1 `ignored`.

- [ ] **Step 3: Написать `#[ignore]`-тест старта NAT VM с живым DHCP**

Добавить в конец `crates/platform/tests/hyperv.rs`. Он повторяет структуру `starts_a_nat_vm_on_its_endpoint`, включая закрытие ошибок в `Result` ради гарантированной уборки:

```rust
/// Exercises TASK-47 against a real host: starting a NAT VM must bring the
/// DHCP server up, and the address the server was given must be the one HNS
/// assigned to the VM's endpoint.
///
/// What this test cannot assert is the guest's own view -- whether its
/// interface actually came up with that address. That is the manual parity
/// check, run with a VHDX carrying an installed Linux; installer media is
/// enough for everything asserted here.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_started_nat_vm_is_served_the_address_hns_assigned --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS, HNS, a free UDP 67 and VMLORD_TEST_IMAGE_PATH set"]
fn a_started_nat_vm_is_served_the_address_hns_assigned() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-dhcp-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-dhcp-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
        username: "admin".into(),
        password: "not used by start".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let outcome = (|| -> Result<(), String> {
        // The start fails rather than coming up silently without a network if
        // the DHCP server cannot bind UDP 67, so this is also the check that
        // nothing else on the host is already serving it.
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| format!("a NAT VM must start with its DHCP server running: {error}"))?;

        let endpoint = recorded_endpoint(&store, &mapping.vm_name)?;
        let address = endpoint_address(endpoint)?
            .ok_or("HNS must assign the endpoint an address for the guest to be served")?;

        if address.prefix_length == 0 {
            return Err(format!(
                "HNS reported a prefix of 0 for {}, which describes no subnet",
                address.ip_address
            ));
        }
        address
            .ip_address
            .parse::<std::net::Ipv4Addr>()
            .map_err(|error| format!("HNS reported \"{}\": {error}", address.ip_address))?;

        // A second start must not fail on the reservation the first one made.
        if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
            let _ = system
                .terminate()
                .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
        }
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| {
                format!("a second start must not trip over the existing reservation: {error}")
            })?;

        Ok(())
    })();

    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    if let Ok(endpoint) = recorded_endpoint(&store, &mapping.vm_name) {
        let _ = HcnEndpoint::delete(endpoint);
    }
    let _ = fs::remove_dir_all(&root);

    outcome.expect("a NAT VM must start with the DHCP server serving the address HNS assigned");
}
```

- [ ] **Step 4: Проверить, что тест компилируется и по умолчанию пропускается**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu --test hyperv`
Expected: PASS, новый тест в списке `ignored`.

- [ ] **Step 5: Обновить ARCHITECTURE.md**

Вставить после абзаца, заканчивающегося словами «VMLord keeps its endpoints instead, so it has to release them explicitly», новый текст:

```markdown
An endpoint alone does not give a Linux guest an address. HNS NAT does not
answer the guest's DHCP Discover, and this network's `EnableDhcpServer` is
rejected as unsupported, so `platform::dhcp` answers instead: a UDP socket on
`0.0.0.0:67` and a worker thread, started with the first NAT VM and stopped with
the process. The protocol itself is `arcbox-dhcp`'s -- it takes a datagram and
returns the reply -- while VMLord owns the socket, the thread and the
reservations.

VMLord is not an allocator here either. Every address it offers is one HNS
assigned to an endpoint and reserved to that endpoint's MAC, and a packet from a
MAC that has no reservation is dropped before the server sees it. That check is
also what keeps the host's own LAN out: the socket has to be bound to `0.0.0.0`,
because a socket bound to the vNIC's unicast address receives no broadcast
Discover, so DHCP broadcasts from every host interface arrive at it. A stranger
is sent nothing at all -- not even a NAK, which would break its configuration --
and the server's pool never holds an address HNS did not hand out, which is what
keeps `reserve_ip` from panicking on an address already taken.

A start reserves the address of the VM it is starting, and the first start of
the process also reserves the address of every endpoint already recorded: a
VMLord that was restarted while its VMs kept running would otherwise drop their
renewals, and a guest that is not answered does not ask again.

The subnet, gateway and mask come from the endpoint's own address rather than
from a second query to HNS. The DNS servers do not: WinNAT runs no DNS proxy on
the gateway, so a guest pointed at it would resolve nothing. `platform::host_dns`
offers the host's own IPv4 resolvers instead, minus loopback, link-local and
anything inside the VMLord subnet, and falls back to 1.1.1.1 and 8.8.8.8 when
nothing usable is left.

The lease is a day long, and it outlives VMLord: the server stops with the
process, so a guest keeps its address after the application is closed but has
nothing to renew against. Moving the server into a Windows service, or keeping
VMLord in the tray, is left for later.

UDP 67 being served already fails the start with a diagnosis naming Internet
Connection Sharing, the Hyper-V Default Switch and third-party DHCP servers.
`SO_REUSEADDR` is deliberately not set: on Windows it would let VMLord take over
a port another server is answering on, and two servers answering the same guests
is worse than a start that says why it failed.
```

- [ ] **Step 6: Проверить весь workspace**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --target=x86_64-pc-windows-gnu && cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: сборка, тесты и clippy проходят по всему workspace.

- [ ] **Step 7: Коммит**

```bash
git add crates/platform/src/dhcp.rs crates/platform/tests/hyperv.rs ARCHITECTURE.md
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-47: Document the DHCP server and cover it on a real host"
```

---

## Что остаётся владельцу проекта

- Прогон обоих `#[ignore]`-тестов на живом хосте: только они проверяют FFI `GetAdaptersAddresses`, бинд UDP 67 и реальную отправку ответа.
- Ручная parity-проверка с VHDX, где установлен Linux: гость должен поднять интерфейс с тем же адресом, что HNS назначил endpoint'у, и разрешать имена через выданные DNS. Ни один автотест не видит этого изнутри гостя, пока не сделана #37.
- Открытие merge request после явного одобрения, с назначением на `mrundead` и запросом ревью у него.
