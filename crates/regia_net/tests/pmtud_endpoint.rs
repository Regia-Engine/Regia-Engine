// SPDX-License-Identifier: MPL-2.0
//! Path MTU discovery on the endpoints (spec 120 § 4.3, ADR-8300): a probing server and client
//! find the loopback path's 1,452 bytes and report it in `net.status` / `net.peers` (`link`
//! objects), sealed or plain; probing off reports the base and `disabled`; a probe with nonzero
//! padding is malformed.
use regia_net::PeerRow;
use regia_net::connection::{Connection, LinkConfig, LinkStats};
use regia_net::crypto::CryptoMode;
use regia_net::pmtud::{BASE_PLPMTU, MtuSearch};
use regia_net::prelude::*;
use std::net::SocketAddr;

#[derive(Debug)]
struct Game;
impl NetGame for Game {
    type Message = u32;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.pmtud";
    const PROTOCOL_VERSION: u32 = 1;
}

fn link(mtu_probing: bool) -> LinkConfig {
    LinkConfig {
        mtu_probing,
        ..LinkConfig::default()
    }
}

/// Runs a server and one client on loopback for three virtual seconds; returns the server's peer
/// row link and the client's link stats.
fn session(mtu_probing: bool, crypto: CryptoMode) -> (PeerRow, LinkStats) {
    let net = LoopbackNet::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let mut server: NetServer<Game, _> = NetServer::with_secret(
        net.bind(addr).expect("bind"),
        ServerConfig {
            crypto,
            link: link(mtu_probing),
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let mut client: NetClient<Game, _> = NetClient::connect(
        net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
        addr,
        ClientConfig {
            crypto,
            link: link(mtu_probing),
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    for now in 0..3_000 {
        client.update(now);
        server.update(now);
        if now.is_multiple_of(16) && client.is_connected() {
            let _ = client.send(Channel::GAME_RPC, &(now as u32));
        }
    }
    assert!(client.is_connected(), "joined");
    let row = server.peer_rows().into_iter().next().expect("one peer");
    let stats = client.status().link.expect("client link");
    (row, stats)
}

#[test]
fn probing_endpoints_report_the_loopback_path_mtu_sealed_and_plain() {
    for crypto in [CryptoMode::Off, CryptoMode::Required] {
        let (row, client) = session(true, crypto);
        println!(
            "{crypto:?}: server peer path_mtu {} {:?} probes {}/{} lost; client {} {:?}",
            row.link.path_mtu,
            row.link.mtu_search,
            row.link.mtu_probes_sent,
            row.link.mtu_probes_lost,
            client.path_mtu,
            client.mtu_search
        );
        assert_eq!(row.sealed, crypto == CryptoMode::Required);
        assert_eq!(
            row.link.path_mtu, 1452,
            "{crypto:?}: the server's sends reach 1,452"
        );
        assert_eq!(row.link.mtu_search, MtuSearch::Complete);
        assert_eq!((row.link.mtu_probes_sent, row.link.mtu_probes_lost), (3, 0));
        assert_eq!(client.path_mtu, 1452, "{crypto:?}: the client's sends too");
        // The agent protocol serializes the row as it is: the fields travel in its `link` object.
        let text = ron::to_string(&row).expect("serialize");
        assert!(text.contains("path_mtu:1452"), "{text}");
        assert!(text.contains("mtu_search:search_complete"), "{text}");
    }
}

#[test]
fn probing_off_reports_the_base_and_disabled() {
    let (row, client) = session(false, CryptoMode::Off);
    assert_eq!(row.link.path_mtu as usize, BASE_PLPMTU);
    assert_eq!(row.link.mtu_search, MtuSearch::Disabled);
    assert_eq!(row.link.mtu_probes_sent, 0);
    assert_eq!(
        (client.path_mtu as usize, client.mtu_search),
        (BASE_PLPMTU, MtuSearch::Disabled)
    );
    let text = ron::to_string(&row).expect("serialize");
    assert!(text.contains("mtu_search:disabled"), "{text}");
}

/// A probe is a ping plus zero padding; padding that is not all zeros is malformed and changes
/// nothing (the next valid packet is still accepted).
#[test]
fn a_probe_with_nonzero_padding_is_refused() {
    let mut a = Connection::new(link(true));
    let mut b = Connection::new(link(true));
    let mut probe = None;
    for now in 0..2_000u64 {
        for p in a.poll(now, &[]) {
            let is_probe = p.len() > BASE_PLPMTU && probe.is_none();
            if is_probe {
                probe = Some(p.clone());
            }
            b.receive(&p, now).expect("valid");
        }
        for p in b.poll(now, &[]) {
            a.receive(&p, now).expect("valid");
        }
        if probe.is_some() {
            break;
        }
    }
    let mut bad = probe.expect("a probe left");
    assert_eq!(bad.len(), 1280);
    *bad.last_mut().expect("padding") = 1;
    let before = b.stats().packets_received;
    assert!(b.receive(&bad, 5_000).is_err(), "nonzero padding is malformed");
    assert_eq!(b.stats().packets_received, before);
}
