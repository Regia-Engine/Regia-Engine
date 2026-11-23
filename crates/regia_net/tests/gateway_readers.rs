// SPDX-License-Identifier: MPL-2.0
//! The per-OS default-gateway readers of port mapping (ADR-8893, spec 120 § 10.7).
//!
//! The parsers run on every host against routing-table bytes captured from a real Linux netlink
//! dump and a real macOS `PF_ROUTE` sysctl (addresses rewritten to documentation ranges), and a
//! Windows `MIB_IPFORWARDTABLE` built to the documented layout. `default_gateway_v4` itself is
//! checked live on the host the test runs on.

use std::net::Ipv4Addr;

use regia_net::port_map::{
    default_gateway_v4, parse_bsd_route_dump, parse_ipv4_forward_table, parse_netlink_route_dump,
};

fn hex(text: &str) -> Vec<u8> {
    let text = text.trim();
    assert!(text.len().is_multiple_of(2), "odd hex fixture");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex fixture"))
        .collect()
}

const NETLINK_DUMP: &str = include_str!("fixtures/gateway/linux-netlink-route-dump.hex");
const MACOS_DUMP: &str = include_str!("fixtures/gateway/macos-route-sysctl-dump.hex");

/// Netlink messages of the fixture: (offset, length).
fn netlink_messages(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 16 <= bytes.len() {
        let len = u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("len")) as usize;
        out.push((at, len));
        at += (len + 3) & !3;
    }
    out
}

#[test]
fn netlink_dump_finds_the_main_table_default_with_the_lowest_metric() {
    // Captured: a policy-routing table (52) host route, two main-table defaults through
    // 192.0.2.1 (metrics 100 and 101), link routes, a local route and NLMSG_DONE.
    let dump = hex(NETLINK_DUMP);
    assert_eq!(parse_netlink_route_dump(&dump), Some(Ipv4Addr::new(192, 0, 2, 1)));
}

#[test]
fn netlink_dump_prefers_a_lower_metric_and_falls_back_to_other_tables() {
    let dump = hex(NETLINK_DUMP);
    let messages = netlink_messages(&dump);
    // Message 1 and 2 are the defaults; make the second one's gateway distinct and its metric
    // the lowest: it must win.
    let mut lower = dump.clone();
    let (at, len) = messages[2];
    let msg = &mut lower[at..at + len];
    let gw = find_attr(msg, 5).expect("RTA_GATEWAY");
    msg[gw..gw + 4].copy_from_slice(&[192, 0, 2, 254]);
    let prio = find_attr(msg, 6).expect("RTA_PRIORITY");
    msg[prio..prio + 4].copy_from_slice(&50_u32.to_ne_bytes());
    assert_eq!(
        parse_netlink_route_dump(&lower),
        Some(Ipv4Addr::new(192, 0, 2, 254))
    );

    // Android: no default in the main table, one in a per-network table (1003).
    let mut android = dump.clone();
    for &(at, len) in &messages[1..3] {
        let msg = &mut android[at..at + len];
        let table = find_attr(msg, 15).expect("RTA_TABLE");
        msg[table..table + 4].copy_from_slice(&1003_u32.to_ne_bytes());
    }
    assert_eq!(
        parse_netlink_route_dump(&android),
        Some(Ipv4Addr::new(192, 0, 2, 1))
    );

    // Without the default routes there is no gateway; a cut dump keeps only whole messages.
    let (first_default, _) = messages[1];
    let (after_defaults, _) = messages[3];
    let mut none = dump[..first_default].to_vec();
    none.extend_from_slice(&dump[after_defaults..]);
    assert_eq!(parse_netlink_route_dump(&none), None);
    assert_eq!(parse_netlink_route_dump(&dump[..first_default + 30]), None);
}

/// Offset of the data of the first `rtattr` of `kind` in one route message.
fn find_attr(msg: &[u8], kind: u16) -> Option<usize> {
    let mut at = 16 + 12;
    while at + 4 <= msg.len() {
        let len = u16::from_ne_bytes(msg[at..at + 2].try_into().ok()?) as usize;
        let ty = u16::from_ne_bytes(msg[at + 2..at + 4].try_into().ok()?);
        if ty == kind {
            return Some(at + 4);
        }
        if len < 4 {
            return None;
        }
        at += (len + 3) & !3;
    }
    None
}

#[test]
fn macos_route_dump_finds_the_default_gateway() {
    // Captured on macOS 26 (arm64): the default route (netmask with sa_len 0) through 192.0.2.1,
    // then two cloned host routes through the same gateway.
    let dump = hex(MACOS_DUMP);
    assert_eq!(parse_bsd_route_dump(&dump), Some(Ipv4Addr::new(192, 0, 2, 1)));
    // Host routes alone (no default) give nothing.
    let first_len = usize::from(u16::from_ne_bytes([dump[0], dump[1]]));
    assert_eq!(parse_bsd_route_dump(&dump[first_len..]), None);
    // A cut message is not read past its end.
    assert_eq!(parse_bsd_route_dump(&dump[..first_len - 1]), None);
}

#[test]
fn macos_route_dump_prefers_an_unscoped_default() {
    let dump = hex(MACOS_DUMP);
    let first_len = usize::from(u16::from_ne_bytes([dump[0], dump[1]]));
    let default = &dump[..first_len];
    // A scoped default (RTF_IFSCOPE) through 192.0.2.77, then the unscoped one.
    let mut scoped = default.to_vec();
    let flags = i32::from_ne_bytes(scoped[8..12].try_into().expect("flags")) | 0x0100_0000;
    scoped[8..12].copy_from_slice(&flags.to_ne_bytes());
    // The gateway sockaddr follows the 92-byte header and the 16-byte destination.
    scoped[92 + 16 + 4..92 + 16 + 8].copy_from_slice(&[192, 0, 2, 77]);
    let mut both = scoped.clone();
    both.extend_from_slice(default);
    assert_eq!(parse_bsd_route_dump(&both), Some(Ipv4Addr::new(192, 0, 2, 1)));
    // Only the scoped one: it is used.
    assert_eq!(parse_bsd_route_dump(&scoped), Some(Ipv4Addr::new(192, 0, 2, 77)));
    // Another rtm_version is skipped.
    let mut other_version = default.to_vec();
    other_version[2] = 4;
    assert_eq!(parse_bsd_route_dump(&other_version), None);
}

/// One `MIB_IPFORWARDROW` (Win32 IP Helper reference): fourteen little-endian DWORDs, addresses
/// in network byte order.
fn forward_row(dest: [u8; 4], mask: [u8; 4], next_hop: [u8; 4], metric: u32) -> Vec<u8> {
    let mut row = Vec::with_capacity(56);
    row.extend_from_slice(&dest);
    row.extend_from_slice(&mask);
    row.extend_from_slice(&0_u32.to_le_bytes()); // dwForwardPolicy
    row.extend_from_slice(&next_hop);
    row.extend_from_slice(&11_u32.to_le_bytes()); // dwForwardIfIndex
    row.extend_from_slice(&4_u32.to_le_bytes()); // dwForwardType: MIB_IPROUTE_TYPE_INDIRECT
    row.extend_from_slice(&3_u32.to_le_bytes()); // dwForwardProto: MIB_IPPROTO_NETMGMT
    row.extend_from_slice(&1234_u32.to_le_bytes()); // dwForwardAge
    row.extend_from_slice(&0_u32.to_le_bytes()); // dwForwardNextHopAS
    row.extend_from_slice(&metric.to_le_bytes()); // dwForwardMetric1
    for _ in 0..4 {
        row.extend_from_slice(&u32::MAX.to_le_bytes()); // dwForwardMetric2..5 unused
    }
    assert_eq!(row.len(), 56);
    row
}

fn forward_table(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut table = (rows.len() as u32).to_le_bytes().to_vec();
    for row in rows {
        table.extend_from_slice(row);
    }
    table
}

#[test]
fn windows_forward_table_finds_the_lowest_metric_default() {
    let lan = forward_row([192, 0, 2, 0], [255, 255, 255, 0], [192, 0, 2, 57], 281);
    let loopback = forward_row([127, 0, 0, 0], [255, 0, 0, 0], [127, 0, 0, 1], 331);
    let wifi_default = forward_row([0; 4], [0; 4], [192, 0, 2, 1], 35);
    let vpn_default = forward_row([0; 4], [0; 4], [198, 51, 100, 1], 25);
    let on_link_default = forward_row([0; 4], [0; 4], [0; 4], 1);
    let table = forward_table(&[lan.clone(), loopback.clone(), wifi_default.clone()]);
    assert_eq!(
        parse_ipv4_forward_table(&table),
        Some(Ipv4Addr::new(192, 0, 2, 1))
    );
    let table = forward_table(&[on_link_default, wifi_default.clone(), vpn_default, lan.clone()]);
    assert_eq!(
        parse_ipv4_forward_table(&table),
        Some(Ipv4Addr::new(198, 51, 100, 1))
    );
    // No default row, a count larger than the rows present, and an empty table.
    assert_eq!(parse_ipv4_forward_table(&forward_table(&[lan, loopback])), None);
    let mut short = forward_table(&[wifi_default]);
    short[..4].copy_from_slice(&9_u32.to_le_bytes());
    assert_eq!(
        parse_ipv4_forward_table(&short),
        Some(Ipv4Addr::new(192, 0, 2, 1))
    );
    assert_eq!(parse_ipv4_forward_table(&0_u32.to_le_bytes()), None);
}

/// The host's own default route, read without this crate: `/proc/net/route` on Linux, `route -n
/// get default` on macOS, `route print` on Windows. `Some(None)`: the host has none. `None`: this
/// host has no independent reading (Android, where `/proc/net/route` is denied or main-table only).
fn host_default_gateway() -> Option<Option<Ipv4Addr>> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/net/route").ok()?;
        Some(regia_net::port_map::parse_proc_route(&table))
    }
    #[cfg(target_vendor = "apple")]
    {
        let out = std::process::Command::new("/sbin/route")
            .args(["-n", "get", "default"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        Some(
            text.lines()
                .find_map(|l| l.trim().strip_prefix("gateway:"))
                .and_then(|g| g.trim().parse().ok()),
        )
    }
    #[cfg(windows)]
    {
        // `route print -4 0.0.0.0`: "0.0.0.0  0.0.0.0  <gateway>  <interface>  <metric>" rows.
        let out = std::process::Command::new("route")
            .args(["print", "-4", "0.0.0.0"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        Some(text.lines().find_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() == 5 && cols[0] == "0.0.0.0" && cols[1] == "0.0.0.0" {
                cols[2].parse::<Ipv4Addr>().ok()
            } else {
                None
            }
        }))
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    {
        None
    }
}

#[test]
fn default_gateway_v4_reads_this_hosts_routing_table() {
    let got = default_gateway_v4();
    match host_default_gateway() {
        Some(Some(expected)) => {
            let got = got
                .unwrap_or_else(|e| panic!("default_gateway_v4 failed on a host with a default route: {e}"));
            // Windows picks by route metric alone; with several defaults the system tool may
            // list another first, so require only that the reader found a gateway.
            if cfg!(windows) {
                assert!(!got.is_unspecified());
            } else {
                assert_eq!(got, expected);
            }
            eprintln!("default_gateway_v4 = {got} (host tool: {expected})");
        }
        Some(None) => {
            let error = got.expect_err("no default route on this host, yet the reader found one");
            assert!(
                matches!(error, regia_net::port_map::PortMapError::Unsupported(_)),
                "{error}"
            );
        }
        None => eprintln!("default_gateway_v4 = {got:?} (no independent reading on this host)"),
    }
}
