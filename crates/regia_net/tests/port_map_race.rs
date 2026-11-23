// SPDX-License-Identifier: MPL-2.0
//! Port mapping raced at host start (spec 120 § 10.7, plan N229, ADR-8370): PCP (RFC 6887),
//! NAT-PMP (RFC 6886) and UPnP IGD ask their gateways at once, the first grant wins, a later
//! grant is deleted again, and the winner's lease renews and deletes its mapping. The gateways
//! are in-process responders on loopback sockets.
//!
//! Ordering is forced with channels, never with sleeps: a responder that must answer late waits
//! until the race has returned. Every wait is bounded by a socket timeout.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

use regia_net::port_map::{
    BoxedPortMapper, LeaseEvent, MapProtocol, NAT_PMP_OP_MAP_UDP, NatPmpClient, PCP_MAP_BYTES, PcpClient,
    PcpMapResponse, nat_pmp_encode_address, nat_pmp_encode_map, race_port_mappers,
};
use regia_net::upnp::{IGD_SEARCH_TARGET, UpnpClient};

const PUBLIC: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 40);

fn loopback_udp() -> Option<UdpSocket> {
    UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).ok()
}

/// What a PCP responder saw: `(nonce, internal port, lifetime)` per request.
type PcpLog = Vec<([u8; 12], u16, u32)>;

/// A PCP gateway that answers each request after `gate` lets it (`None`: at once) with `result`,
/// until it has seen `requests` requests or its socket times out.
fn pcp_gateway(
    socket: UdpSocket,
    result: u8,
    gate: Option<mpsc::Receiver<()>>,
    requests: usize,
) -> std::thread::JoinHandle<PcpLog> {
    std::thread::spawn(move || {
        let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
        let mut seen = Vec::new();
        let mut buf = [0u8; 128];
        let mut gate = gate;
        while seen.len() < requests {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                break;
            };
            if n < PCP_MAP_BYTES {
                continue;
            }
            if let Some(g) = gate.take() {
                // Hold the first answer until the race is over; the client resends meanwhile.
                let _ = g.recv_timeout(Duration::from_secs(10));
            }
            let mut nonce = [0u8; 12];
            nonce.copy_from_slice(&buf[24..36]);
            let internal_port = u16::from_be_bytes([buf[40], buf[41]]);
            let lifetime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            // A resend of the request just answered is answered again, not logged again.
            if seen.last() != Some(&(nonce, internal_port, lifetime)) {
                seen.push((nonce, internal_port, lifetime));
            }
            let resp = PcpMapResponse {
                result,
                lifetime_secs: lifetime,
                nonce,
                internal_port,
                external_port: internal_port,
                external_ip: PUBLIC,
            };
            let _ = socket.send_to(&resp.encode(), from);
        }
        seen
    })
}

/// A NAT-PMP gateway: answers address queries, and logs and grants `maps` UDP map requests
/// `(internal port, lifetime)`.
fn nat_pmp_gateway(socket: UdpSocket, maps: usize) -> std::thread::JoinHandle<Vec<(u16, u32)>> {
    std::thread::spawn(move || {
        let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
        let mut seen = Vec::new();
        let mut buf = [0u8; 64];
        while seen.len() < maps {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                break;
            };
            if n == 2 && buf[1] == 0 {
                let _ = socket.send_to(&nat_pmp_encode_address(PUBLIC, 1), from);
            } else if n >= 12 && buf[1] == NAT_PMP_OP_MAP_UDP {
                let internal = u16::from_be_bytes([buf[4], buf[5]]);
                let lifetime = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                seen.push((internal, lifetime));
                let _ = socket.send_to(&nat_pmp_encode_map(0, internal, internal, lifetime), from);
            }
        }
        seen
    })
}

/// Reads one HTTP request (headers and the announced body) and answers `200` with `body`.
fn http_answer(listener: &TcpListener, body: &str) -> String {
    let Ok((mut stream, _)) = listener.accept() else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&got);
        if let Some((head, rest)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                })
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if rest.len() >= len {
                break;
            }
        }
    }
    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let _ = stream.write_all(reply.as_bytes());
    String::from_utf8_lossy(&got).into_owned()
}

const DESCRIPTION: &str = "<?xml version=\"1.0\"?><root><device><serviceList>\
    <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
    <controlURL>/ctl</controlURL></service></serviceList></device></root>";

/// A UPnP IGD: answers the search, the description, `GetExternalIPAddress` and then `calls` more
/// SOAP calls with an empty success. Returns every HTTP request.
fn upnp_gateway(ssdp: UdpSocket, http: TcpListener, calls: usize) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let _ = ssdp.set_read_timeout(Some(Duration::from_secs(10)));
        let mut buf = [0u8; 1024];
        let Ok((_, from)) = ssdp.recv_from(&mut buf) else {
            return Vec::new();
        };
        let port = http.local_addr().map(|a| a.port()).unwrap_or(0);
        let answer = format!(
            "HTTP/1.1 200 OK\r\nST: {IGD_SEARCH_TARGET}\r\nLOCATION: http://127.0.0.1:{port}/desc.xml\r\n\r\n"
        );
        let _ = ssdp.send_to(answer.as_bytes(), from);
        let mut requests = vec![http_answer(&http, DESCRIPTION)];
        requests.push(http_answer(
            &http,
            "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.40</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>",
        ));
        for _ in 0..calls {
            requests.push(http_answer(&http, "<s:Envelope><s:Body/></s:Envelope>"));
        }
        requests
    })
}

/// PCP refuses, NAT-PMP never answers, UPnP grants: UPnP wins without waiting for NAT-PMP's
/// timeout, and the lease deletes the UPnP mapping when released.
#[test]
fn the_race_takes_the_one_gateway_that_grants() {
    let (Some(pcp), Some(silent), Some(ssdp), Ok(http)) = (
        loopback_udp(),
        loopback_udp(),
        loopback_udp(),
        TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
    ) else {
        return; // no loopback in this sandbox
    };
    let pcp_addr = pcp.local_addr().expect("addr");
    let silent_addr = silent.local_addr().expect("addr");
    let ssdp_addr = ssdp.local_addr().expect("addr");
    let pcp_thread = pcp_gateway(pcp, 2, None, 1);
    // GetExternalIPAddress, AddPortMapping, then DeletePortMapping.
    let upnp_thread = upnp_gateway(ssdp, http, 2);
    let mappers: Vec<BoxedPortMapper> = vec![
        Box::new(PcpClient::new(
            pcp_addr,
            Ipv4Addr::LOCALHOST,
            Duration::from_secs(5),
        )),
        // Nobody answers here: this racer runs into its 5 s timeout after the race is decided.
        Box::new(NatPmpClient::new(silent_addr, Duration::from_secs(5))),
        Box::new(UpnpClient::with_ssdp(
            ssdp_addr,
            Ipv4Addr::LOCALHOST,
            Duration::from_secs(5),
        )),
    ];
    let lease = race_port_mappers(mappers, 7777, 600, 0).expect("UPnP grants");
    assert_eq!(lease.mapping().protocol, MapProtocol::Upnp);
    assert_eq!(lease.mapper_name(), "upnp");
    assert_eq!(lease.mapping().external, SocketAddr::from((PUBLIC, 7777)));
    lease.release().expect("deleted");
    let requests = upnp_thread.join().expect("upnp gateway");
    assert!(
        requests.iter().any(|r| r.contains("#AddPortMapping")),
        "{requests:?}"
    );
    assert!(
        requests.last().is_some_and(|r| r.contains("#DeletePortMapping")),
        "the released lease deletes the UPnP mapping: {requests:?}"
    );
    let pcp_seen = pcp_thread.join().expect("pcp gateway");
    assert_eq!(pcp_seen.len(), 1, "PCP was asked once and refused");
    drop(silent);
}

/// NAT-PMP grants at once; PCP grants only after the race returned. NAT-PMP wins, PCP's late
/// grant is deleted with the nonce that made it, and the NAT-PMP lease renews at half its
/// lifetime and deletes its mapping on release.
#[test]
fn the_first_grant_wins_a_late_grant_is_deleted_and_the_winner_renews() {
    let (Some(pcp), Some(pmp), Some(ssdp)) = (loopback_udp(), loopback_udp(), loopback_udp()) else {
        return; // no loopback in this sandbox
    };
    let pcp_addr = pcp.local_addr().expect("addr");
    let pmp_addr = pmp.local_addr().expect("addr");
    let ssdp_addr = ssdp.local_addr().expect("addr");
    let (go, gate) = mpsc::channel();
    // The late grant, then its delete.
    let pcp_thread = pcp_gateway(pcp, 0, Some(gate), 2);
    // The grant, one renewal, the delete.
    let pmp_thread = nat_pmp_gateway(pmp, 3);
    let mappers: Vec<BoxedPortMapper> = vec![
        Box::new(PcpClient::new(
            pcp_addr,
            Ipv4Addr::LOCALHOST,
            Duration::from_secs(10),
        )),
        Box::new(NatPmpClient::new(pmp_addr, Duration::from_secs(10))),
        // A UPnP search nobody answers: this racer fails on its own, after the race.
        Box::new(UpnpClient::with_ssdp(
            ssdp_addr,
            Ipv4Addr::LOCALHOST,
            Duration::from_secs(2),
        )),
    ];
    let mut lease = race_port_mappers(mappers, 7777, 600, 1_000).expect("NAT-PMP grants");
    let _ = go.send(());
    assert_eq!(lease.mapping().protocol, MapProtocol::NatPmp);
    assert_eq!(lease.mapping().external, SocketAddr::from((PUBLIC, 7777)));
    assert_eq!(lease.next_renew_ms(), 1_000 + 300_000);
    assert_eq!(lease.update(300_999), LeaseEvent::Idle);
    assert!(
        matches!(lease.update(301_000), LeaseEvent::Renewed(m) if m.protocol == MapProtocol::NatPmp),
        "the winner renews at half its lifetime"
    );
    assert_eq!(lease.renewals(), 1);
    lease.release().expect("deleted");
    let pmp_seen = pmp_thread.join().expect("nat-pmp gateway");
    assert_eq!(pmp_seen, vec![(7777, 600), (7777, 600), (7777, 0)]);
    let pcp_seen = pcp_thread.join().expect("pcp gateway");
    assert_eq!(pcp_seen.len(), 2, "the late PCP grant is deleted: {pcp_seen:?}");
    assert_eq!(pcp_seen[1].2, 0, "a delete has lifetime 0");
    assert_eq!(
        pcp_seen[1].0, pcp_seen[0].0,
        "the delete carries the late grant's nonce"
    );
    drop(ssdp);
}

/// Every racer failing is one error naming each of them.
#[test]
fn a_race_nobody_wins_names_every_racer() {
    let (Some(pcp), Some(pmp)) = (loopback_udp(), loopback_udp()) else {
        return;
    };
    let pcp_addr = pcp.local_addr().expect("addr");
    let pmp_addr = pmp.local_addr().expect("addr");
    let pcp_thread = pcp_gateway(pcp, 2, None, 1);
    let pmp_thread = std::thread::spawn(move || {
        let _ = pmp.set_read_timeout(Some(Duration::from_secs(10)));
        let mut buf = [0u8; 64];
        if let Ok((_, from)) = pmp.recv_from(&mut buf) {
            // Refused: result 2, not authorized.
            let mut refusal = nat_pmp_encode_address(PUBLIC, 0);
            refusal[3] = 2;
            let _ = pmp.send_to(&refusal, from);
        }
    });
    let mappers: Vec<BoxedPortMapper> = vec![
        Box::new(PcpClient::new(
            pcp_addr,
            Ipv4Addr::LOCALHOST,
            Duration::from_secs(5),
        )),
        Box::new(NatPmpClient::new(pmp_addr, Duration::from_secs(5))),
    ];
    let err = race_port_mappers(mappers, 7777, 600, 0).expect_err("both refuse");
    let mut names: Vec<&str> = err.failures.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["nat_pmp", "pcp"]);
    assert_eq!(err.code(), "port_map_refused");
    pcp_thread.join().expect("pcp gateway");
    pmp_thread.join().expect("nat-pmp gateway");
}
