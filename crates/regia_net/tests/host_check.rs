// SPDX-License-Identifier: MPL-2.0
//! The hosting flow's check before hosting (spec 120 § 10.7, plan N229, ADR-8370): the mapped
//! candidate is announced, the relay (protocol 3) probes the mapped port from a port the host
//! never sent to and the host reports reachable or unreachable, and the uplink measurement on the
//! listen-uplink profile (spec 120 § 15.1: the host's 20 Mbit/s uplink, 128 KiB queue) gives a
//! client count within 20 % of the true one.
//!
//! The host sits behind a port-restricted NAT (`SimNat`, RFC 4787 address-and-port-dependent
//! filtering) with a port forward that the test turns on and off, on the in-memory network. Its
//! uplink is a drop-tail bottleneck in virtual time. Time is a step counter; nothing sleeps and no
//! assertion reads the wall clock.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_net::host_check::{HostCheckConfig, HostCheckReport, UPLINK_PROBE_MAX_BYTES};
use regia_net::nat::CandidateKind;
use regia_net::port_map::{MapProtocol, PortMapping};
use regia_net::relay::{NatFiltering, NatMapping, RelayConfig, RelayCore, RelayServer, SimNat};
use regia_net::server_host::{JoinTicket, RelayRegistrar};
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};

const RELAY_IP: [u8; 4] = [192, 0, 2, 1];
const HOST_PUBLIC: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 20);
const HOST_PRIVATE: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7777);
/// The port the router forwards to the game socket.
const MAPPED_PORT: u16 = 7777;

fn relay_addr() -> SocketAddr {
    SocketAddr::from((RELAY_IP, 7800))
}

/// A protocol 3 relay on the in-memory network, with a probe port unless `probe` is false.
fn relay(net: &LoopbackNet, probe: bool) -> RelayServer<LoopbackSocket> {
    let control = net.bind(relay_addr()).expect("bind relay");
    let pool = (0..4)
        .map(|i| {
            net.bind(SocketAddr::from((RELAY_IP, 50_000 + i)))
                .expect("bind allocation port")
        })
        .collect();
    let server = RelayServer::from_core(
        control,
        pool,
        RelayCore::with_config(5, [4; 32], RelayConfig::default()),
    );
    if probe {
        server.with_probe_socket(
            net.bind(SocketAddr::from((RELAY_IP, 51_000)))
                .expect("bind probe port"),
        )
    } else {
        server
    }
}

/// The host's uplink: a drop-tail queue drained at `bytes_per_ms`, then `delay_ms` to the far end.
struct Uplink {
    bytes_per_ms: f64,
    queue_bytes: f64,
    delay_ms: u64,
    /// When the queue is empty again, in fractional milliseconds.
    busy_until: f64,
    /// `(arrival ms, bytes, destination, public source port)`, in arrival order.
    in_flight: VecDeque<(u64, Vec<u8>, SocketAddr, u16)>,
    dropped: u64,
}

impl Uplink {
    /// listen-uplink (spec 120 § 15.1): 20 Mbit/s up, a 128 KiB queue; `delay_ms` one way.
    fn listen_uplink(delay_ms: u64) -> Self {
        Self {
            bytes_per_ms: 20_000_000.0 / 8.0 / 1000.0,
            queue_bytes: 128.0 * 1024.0,
            delay_ms,
            busy_until: 0.0,
            in_flight: VecDeque::new(),
            dropped: 0,
        }
    }

    /// A link with no rate limit worth the name (control traffic only).
    fn open(delay_ms: u64) -> Self {
        Self {
            bytes_per_ms: 1e12,
            ..Self::listen_uplink(delay_ms)
        }
    }

    fn enqueue(&mut self, now: u64, bytes: &[u8], dest: SocketAddr, port: u16) {
        let now_f = now as f64;
        let start = self.busy_until.max(now_f);
        let backlog = (start - now_f) * self.bytes_per_ms;
        if backlog + bytes.len() as f64 > self.queue_bytes {
            self.dropped += 1;
            return;
        }
        self.busy_until = start + bytes.len() as f64 / self.bytes_per_ms;
        let arrival = self.busy_until.ceil() as u64 + self.delay_ms;
        self.in_flight.push_back((arrival, bytes.to_vec(), dest, port));
    }
}

/// The host's game socket behind a port-restricted NAT with a switchable port forward.
struct HostSocket {
    net: LoopbackNet,
    nat: SimNat,
    public: HashMap<u16, LoopbackSocket>,
    forward: LoopbackSocket,
    forward_on: Arc<AtomicBool>,
    clock: Arc<AtomicU64>,
    uplink: Arc<Mutex<Uplink>>,
}

impl HostSocket {
    fn flush(&mut self, now: u64) -> io::Result<()> {
        let due: Vec<(u64, Vec<u8>, SocketAddr, u16)> = {
            let mut up = self.uplink.lock().expect("uplink");
            let mut out = Vec::new();
            while up.in_flight.front().is_some_and(|p| p.0 <= now) {
                if let Some(p) = up.in_flight.pop_front() {
                    out.push(p);
                }
            }
            out
        };
        for (_, bytes, dest, port) in due {
            if let Some(socket) = self.public.get_mut(&port) {
                socket.send_to(&bytes, dest)?;
            }
        }
        Ok(())
    }
}

impl DatagramSocket for HostSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let now = self.clock.load(Ordering::Relaxed);
        let public = self.nat.outbound(dest);
        if !self.public.contains_key(&public.port()) {
            let socket = self.net.bind(public)?;
            self.public.insert(public.port(), socket);
        }
        self.uplink
            .lock()
            .expect("uplink")
            .enqueue(now, bytes, dest, public.port());
        self.flush(now)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let now = self.clock.load(Ordering::Relaxed);
        self.flush(now)?;
        // The port forward: anything to the mapped port reaches the game socket while it is on.
        while let Some((n, from)) = self.forward.recv_from(buf)? {
            if self.forward_on.load(Ordering::Relaxed) {
                return Ok(Some((n, from)));
            }
        }
        let mut ports: Vec<u16> = self.public.keys().copied().collect();
        ports.sort_unstable();
        for port in ports {
            let Some(socket) = self.public.get_mut(&port) else {
                continue;
            };
            while let Some((n, from)) = socket.recv_from(buf)? {
                if self.nat.inbound(from, port) {
                    return Ok(Some((n, from)));
                }
            }
        }
        Ok(None)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(HOST_PRIVATE)
    }
}

struct Rig {
    relay: RelayServer<LoopbackSocket>,
    registrar: RelayRegistrar,
    clock: Arc<AtomicU64>,
    forward_on: Arc<AtomicBool>,
    uplink: Arc<Mutex<Uplink>>,
    now: u64,
}

impl Rig {
    fn new(probe_port: bool, uplink: Uplink) -> Self {
        let net = LoopbackNet::new();
        let relay = relay(&net, probe_port);
        let clock = Arc::new(AtomicU64::new(0));
        let forward_on = Arc::new(AtomicBool::new(true));
        let uplink = Arc::new(Mutex::new(uplink));
        let socket = HostSocket {
            net: net.clone(),
            nat: SimNat::new(
                NatMapping::EndpointIndependent,
                NatFiltering::AddressAndPortDependent,
                HOST_PUBLIC,
            ),
            public: HashMap::new(),
            forward: net
                .bind(SocketAddr::from((HOST_PUBLIC, MAPPED_PORT)))
                .expect("bind the forward"),
            forward_on: forward_on.clone(),
            clock: clock.clone(),
            uplink: uplink.clone(),
        };
        let registrar = RelayRegistrar::new(
            Box::new(socket),
            relay_addr(),
            &JoinTicket::from_game_socket(None),
        )
        .expect("registrar");
        Self {
            relay,
            registrar,
            clock,
            forward_on,
            uplink,
            now: 0,
        }
    }

    fn step(&mut self) {
        self.now += 1;
        self.clock.store(self.now, Ordering::Relaxed);
        self.relay.update(self.now);
        self.registrar.update(self.now);
    }

    /// Steps until the host has a join code (at most 5 s).
    fn register(&mut self) {
        for _ in 0..5_000 {
            if self.registrar.join_code().is_some() {
                return;
            }
            self.step();
        }
        panic!("the host never registered");
    }

    /// Steps until the host check is done (at most `limit_ms`); its report.
    fn until_done(&mut self, limit_ms: u64) -> HostCheckReport {
        let end = self.now + limit_ms;
        while self.now < end {
            self.step();
            if let Some(report) = self.registrar.host_check()
                && report.done
            {
                return report;
            }
        }
        panic!("the host check did not finish: {:?}", self.registrar.host_check());
    }
}

fn mapping() -> PortMapping {
    PortMapping {
        external: SocketAddr::from((HOST_PUBLIC, MAPPED_PORT)),
        internal_port: HOST_PRIVATE.port(),
        lifetime_secs: 600,
        protocol: MapProtocol::Pcp,
    }
}

fn no_measure() -> HostCheckConfig {
    HostCheckConfig {
        per_client_bytes_per_second: 262_144,
        measure_uplink: false,
    }
}

/// The mapping is announced as a mapped candidate; the relay's probe reaches the game socket
/// through the forward (reachable), and after the router dropped the forward the same check says
/// unreachable. On the base no hosting flow called the port mapper, there was no probe, and the
/// candidate source for mappings had no implementation.
#[test]
fn the_self_check_reports_reachable_and_then_unreachable_when_the_mapping_is_removed() {
    let mut rig = Rig::new(true, Uplink::open(10));
    rig.registrar.start_host_check(no_measure(), 0);
    rig.registrar.set_port_mapping("pcp", &mapping(), 0, 0);
    rig.register();
    let report = rig.until_done(5_000);
    assert_eq!(report.reachable, "reachable", "{report:?}");
    let view = report.mapping.expect("the mapping is reported");
    assert_eq!(
        (view.mapper.as_str(), view.external.as_str()),
        ("pcp", "198.51.100.20:7777")
    );
    assert!(
        rig.registrar
            .candidates()
            .iter()
            .any(|c| c.kind == CandidateKind::Mapped && c.addr == mapping().external),
        "the mapped candidate is announced: {:?}",
        rig.registrar.candidates()
    );
    assert_eq!(rig.relay.core().probes_sent, 1);

    // The router dropped the mapping: the check run again finds the port closed.
    rig.forward_on.store(false, Ordering::Relaxed);
    let now = rig.now;
    rig.registrar.start_host_check(no_measure(), now);
    let report = rig.until_done(5_000);
    assert_eq!(report.reachable, "unreachable", "{report:?}");
    // Four requests, four probes; none reached the game socket.
    assert_eq!(rig.relay.core().probes_sent, 1 + 4);

    // A lapsed mapping withdraws the candidate and the check has nothing to probe.
    let now = rig.now;
    rig.registrar.clear_port_mapping("port_map_unavailable", now);
    let report = rig.registrar.host_check().expect("running");
    assert_eq!(report.reachable, "no_mapping");
    assert_eq!(report.mapping_error.as_deref(), Some("port_map_unavailable"));
    rig.step();
    assert!(
        !rig.registrar
            .candidates()
            .iter()
            .any(|c| c.kind == CandidateKind::Mapped),
        "the mapped candidate is withdrawn"
    );
}

/// A relay without a probe port says it cannot check, rather than "unreachable".
#[test]
fn a_relay_without_a_probe_port_cannot_check() {
    let mut rig = Rig::new(false, Uplink::open(10));
    rig.registrar.start_host_check(no_measure(), 0);
    rig.registrar.set_port_mapping("nat_pmp", &mapping(), 0, 0);
    rig.register();
    let report = rig.until_done(5_000);
    assert_eq!(report.reachable, "relay_cannot_probe", "{report:?}");
}

/// listen-uplink: the measured uplink and the client count at three per-client budgets are within
/// 20 % of the truth (20 Mbit/s = 2,500,000 bytes/s). On the base nothing measured the uplink.
#[test]
fn the_capacity_estimate_on_listen_uplink_is_within_20_percent() {
    const TRUE_UPLINK: f64 = 2_500_000.0;
    let mut rig = Rig::new(true, Uplink::listen_uplink(15));
    rig.registrar.start_host_check(
        HostCheckConfig {
            per_client_bytes_per_second: 262_144,
            measure_uplink: true,
        },
        0,
    );
    rig.registrar.clear_port_mapping("port_map_unavailable", 0);
    rig.register();
    let report = rig.until_done(10_000);
    // The numbers for the record (`--nocapture`).
    eprintln!(
        "listen-uplink: {report:?}, {} ms, {} measurement packets acknowledged",
        rig.now,
        rig.relay.core().measure_acked
    );
    let uplink = report.uplink_bytes_per_second.expect("an estimate") as f64;
    assert!(
        (uplink - TRUE_UPLINK).abs() <= 0.2 * TRUE_UPLINK,
        "uplink {uplink} vs {TRUE_UPLINK}: {report:?}"
    );
    assert_eq!(report.uplink_end.as_deref(), Some("full_pipe"), "{report:?}");
    for budget in [262_144u32, 65_536, 16_384] {
        let truth = (TRUE_UPLINK / f64::from(budget)).floor();
        let estimate = regia_net::host_check::estimate_clients(uplink as u64, budget) as f64;
        assert!(
            (estimate - truth).abs() <= 0.2 * truth,
            "{budget} B/s per client: {estimate} clients estimated, {truth} true"
        );
    }
    let at_default = report.estimated_clients.expect("a count") as f64;
    let truth = (TRUE_UPLINK / 262_144.0).floor();
    assert!(
        (at_default - truth).abs() <= 0.2 * truth,
        "{at_default} vs {truth}"
    );
    let up = rig.uplink.lock().expect("uplink");
    assert!(
        up.dropped > 0,
        "the startup filled the queue (it found the ceiling)"
    );
    let sent = rig.relay.core().measure_acked * 1200;
    assert!(sent <= UPLINK_PROBE_MAX_BYTES, "{sent}");
}
