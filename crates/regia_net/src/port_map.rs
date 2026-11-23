// SPDX-License-Identifier: MPL-2.0
//! Automatic port mapping on the home router: PCP (RFC 6887) first, NAT-PMP (RFC 6886) second.
//!
//! A host behind a cooperating router asks it to forward a public UDP port to its game socket, so
//! clients can reach it directly without the relay. Both protocols are small fixed-layout UDP
//! exchanges with the default gateway on port 5351, in network byte order; the codecs here follow
//! the RFC layouts directly. A refused or absent mapping is normal (many routers disable both);
//! callers fall back to a relay. Calls block for at most their timeout and are meant for session
//! setup, never for a frame.
//!
//! UPnP IGD is the third fallback (`crate::upnp`).

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use thiserror::Error;

mod platform;
mod route_table;

pub use route_table::{parse_bsd_route_dump, parse_ipv4_forward_table, parse_netlink_route_dump};

/// Gateway port for both PCP and NAT-PMP.
pub const PORT_MAP_SERVER_PORT: u16 = 5351;
/// PCP protocol version.
pub const PCP_VERSION: u8 = 2;
/// PCP `MAP` opcode.
pub const PCP_OPCODE_MAP: u8 = 1;
/// IANA protocol number of UDP.
pub const IPPROTO_UDP: u8 = 17;
/// PCP request or response header length.
pub const PCP_HEADER_BYTES: usize = 24;
/// PCP `MAP` request or response length (header plus opcode data).
pub const PCP_MAP_BYTES: usize = PCP_HEADER_BYTES + 36;
/// NAT-PMP opcode: map a UDP port.
pub const NAT_PMP_OP_MAP_UDP: u8 = 1;

/// A granted mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMapping {
    /// The public address clients should use.
    pub external: SocketAddr,
    /// The local port it forwards to.
    pub internal_port: u16,
    /// Seconds until it lapses (renew before then); 0 for a permanent mapping (a UPnP IGD v1
    /// router that grants only permanent leases), which [`PortMapLease`] never renews.
    pub lifetime_secs: u32,
    /// Which protocol granted it.
    pub protocol: MapProtocol,
}

/// The protocol that granted a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapProtocol {
    /// Port Control Protocol.
    Pcp,
    /// NAT Port Mapping Protocol.
    NatPmp,
    /// UPnP Internet Gateway Device (`crate::upnp`).
    Upnp,
    /// The in-memory [`FakeRouter`].
    Fake,
}

/// Why a mapping failed.
#[derive(Debug, Error)]
pub enum PortMapError {
    /// Socket error.
    #[error("port mapping I/O: {0}")]
    Io(#[from] io::Error),
    /// No answer before the timeout.
    #[error("the gateway did not answer")]
    Timeout,
    /// The gateway answered with a non-zero result code.
    #[error("the gateway refused the mapping (result code {0})")]
    Refused(u16),
    /// The answer did not parse.
    #[error("malformed gateway answer: {0}")]
    Malformed(&'static str),
    /// Not available on this platform or network (no IPv4 gateway found, IPv6-only, ...).
    #[error("port mapping unsupported: {0}")]
    Unsupported(&'static str),
}

/// Something that can open, renew and release a UDP mapping.
pub trait PortMapper {
    /// Maps `internal_port` for `lifetime_secs`.
    fn map(&mut self, internal_port: u16, lifetime_secs: u32) -> Result<PortMapping, PortMapError>;
    /// A stable lowercase name for logs and the agent protocol (`pcp`, `nat_pmp`, `upnp`, ...).
    fn name(&self) -> &'static str {
        "port_mapper"
    }
    /// Extends a mapping.
    fn renew(&mut self, mapping: &PortMapping, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
        self.map(mapping.internal_port, lifetime_secs)
    }
    /// Removes a mapping (both protocols: a request with lifetime 0).
    fn release(&mut self, mapping: &PortMapping) -> Result<(), PortMapError> {
        self.map(mapping.internal_port, 0).map(|_| ())
    }
}

/// IPv4 address as the IPv4-mapped IPv6 form PCP carries (RFC 6887 § 5).
pub fn ipv4_to_mapped(ip: Ipv4Addr) -> [u8; 16] {
    ip.to_ipv6_mapped().octets()
}

/// The IPv4 address inside an IPv4-mapped IPv6 address.
pub fn mapped_to_ipv4(bytes: &[u8; 16]) -> Option<Ipv4Addr> {
    Ipv6Addr::from(*bytes).to_ipv4_mapped()
}

/// A PCP `MAP` request (RFC 6887 § 7.1 and § 11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcpMapRequest {
    /// The client's own IPv4 address on the gateway's network.
    pub client_ip: Ipv4Addr,
    /// Random nonce tying the response to this request.
    pub nonce: [u8; 12],
    /// Local port to forward to.
    pub internal_port: u16,
    /// Preferred public port (0 = any).
    pub suggested_external_port: u16,
    /// Requested lifetime; 0 deletes.
    pub lifetime_secs: u32,
}

impl PcpMapRequest {
    /// The 60-byte request.
    pub fn encode(&self) -> [u8; PCP_MAP_BYTES] {
        let mut out = [0u8; PCP_MAP_BYTES];
        out[0] = PCP_VERSION;
        out[1] = PCP_OPCODE_MAP; // R bit clear: request
        out[4..8].copy_from_slice(&self.lifetime_secs.to_be_bytes());
        out[8..24].copy_from_slice(&ipv4_to_mapped(self.client_ip));
        out[24..36].copy_from_slice(&self.nonce);
        out[36] = IPPROTO_UDP;
        out[40..42].copy_from_slice(&self.internal_port.to_be_bytes());
        out[42..44].copy_from_slice(&self.suggested_external_port.to_be_bytes());
        out[44..60].copy_from_slice(&ipv4_to_mapped(Ipv4Addr::UNSPECIFIED));
        out
    }
}

/// A PCP `MAP` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcpMapResponse {
    /// Result code (0 = success).
    pub result: u8,
    /// Granted lifetime.
    pub lifetime_secs: u32,
    /// Echoed nonce.
    pub nonce: [u8; 12],
    /// Echoed internal port.
    pub internal_port: u16,
    /// Assigned public port.
    pub external_port: u16,
    /// Assigned public address.
    pub external_ip: Ipv4Addr,
}

impl PcpMapResponse {
    /// Parses a response datagram.
    pub fn decode(buf: &[u8]) -> Result<Self, PortMapError> {
        if buf.len() < PCP_MAP_BYTES {
            return Err(PortMapError::Malformed("short PCP response"));
        }
        if buf[0] != PCP_VERSION || buf[1] != (0x80 | PCP_OPCODE_MAP) {
            return Err(PortMapError::Malformed("not a PCP MAP response"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&buf[24..36]);
        let mut ip = [0u8; 16];
        ip.copy_from_slice(&buf[44..60]);
        Ok(Self {
            result: buf[3],
            lifetime_secs: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            nonce,
            internal_port: u16::from_be_bytes([buf[40], buf[41]]),
            external_port: u16::from_be_bytes([buf[42], buf[43]]),
            // An error reply may zero the address field (RFC 6887 § 7.2): keep the result code.
            external_ip: match mapped_to_ipv4(&ip) {
                Some(v4) => v4,
                None if buf[3] != 0 => Ipv4Addr::UNSPECIFIED,
                None => return Err(PortMapError::Malformed("PCP address is not IPv4")),
            },
        })
    }

    /// Encodes a response (for fake gateways in tests).
    pub fn encode(&self) -> [u8; PCP_MAP_BYTES] {
        let mut out = [0u8; PCP_MAP_BYTES];
        out[0] = PCP_VERSION;
        out[1] = 0x80 | PCP_OPCODE_MAP;
        out[3] = self.result;
        out[4..8].copy_from_slice(&self.lifetime_secs.to_be_bytes());
        out[24..36].copy_from_slice(&self.nonce);
        out[36] = IPPROTO_UDP;
        out[40..42].copy_from_slice(&self.internal_port.to_be_bytes());
        out[42..44].copy_from_slice(&self.external_port.to_be_bytes());
        out[44..60].copy_from_slice(&ipv4_to_mapped(self.external_ip));
        out
    }
}

/// NAT-PMP public-address request (RFC 6886 § 3.2).
pub fn nat_pmp_address_request() -> [u8; 2] {
    [0, 0]
}

/// Parses a NAT-PMP public-address response: the router's public IPv4 address.
pub fn nat_pmp_decode_address(buf: &[u8]) -> Result<Ipv4Addr, PortMapError> {
    if buf.len() < 12 || buf[0] != 0 || buf[1] != 128 {
        return Err(PortMapError::Malformed("not a NAT-PMP address response"));
    }
    let result = u16::from_be_bytes([buf[2], buf[3]]);
    if result != 0 {
        return Err(PortMapError::Refused(result));
    }
    Ok(Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11]))
}

/// Encodes a NAT-PMP public-address response (for fake gateways in tests).
pub fn nat_pmp_encode_address(public: Ipv4Addr, epoch: u32) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[1] = 128;
    out[4..8].copy_from_slice(&epoch.to_be_bytes());
    out[8..12].copy_from_slice(&public.octets());
    out
}

/// NAT-PMP UDP mapping request (RFC 6886 § 3.3).
pub fn nat_pmp_map_request(internal_port: u16, suggested_external_port: u16, lifetime_secs: u32) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[1] = NAT_PMP_OP_MAP_UDP;
    out[4..6].copy_from_slice(&internal_port.to_be_bytes());
    out[6..8].copy_from_slice(&suggested_external_port.to_be_bytes());
    out[8..12].copy_from_slice(&lifetime_secs.to_be_bytes());
    out
}

/// A parsed NAT-PMP mapping response: `(internal port, external port, lifetime)`.
pub fn nat_pmp_decode_map(buf: &[u8]) -> Result<(u16, u16, u32), PortMapError> {
    if buf.len() < 16 || buf[0] != 0 || buf[1] != 128 + NAT_PMP_OP_MAP_UDP {
        return Err(PortMapError::Malformed("not a NAT-PMP UDP map response"));
    }
    let result = u16::from_be_bytes([buf[2], buf[3]]);
    if result != 0 {
        return Err(PortMapError::Refused(result));
    }
    Ok((
        u16::from_be_bytes([buf[8], buf[9]]),
        u16::from_be_bytes([buf[10], buf[11]]),
        u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
    ))
}

/// Encodes a NAT-PMP mapping response (for fake gateways in tests).
pub fn nat_pmp_encode_map(
    result: u16,
    internal_port: u16,
    external_port: u16,
    lifetime_secs: u32,
) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[1] = 128 + NAT_PMP_OP_MAP_UDP;
    out[2..4].copy_from_slice(&result.to_be_bytes());
    out[8..10].copy_from_slice(&internal_port.to_be_bytes());
    out[10..12].copy_from_slice(&external_port.to_be_bytes());
    out[12..16].copy_from_slice(&lifetime_secs.to_be_bytes());
    out
}

/// Sends `request` to `gateway` and waits up to `timeout` for a reply that `accept` takes.
/// Resends with doubling intervals from 250 ms (RFC 6886 § 3.1), within the timeout.
fn exchange<T>(
    gateway: SocketAddr,
    request: &[u8],
    timeout: Duration,
    accept: impl FnMut(&[u8]) -> Option<Result<T, PortMapError>>,
) -> Result<T, PortMapError> {
    exchange_until(gateway, request, Instant::now() + timeout, accept)
}

/// [`exchange`] against a deadline that several requests can share.
fn exchange_until<T>(
    gateway: SocketAddr,
    request: &[u8],
    deadline: Instant,
    mut accept: impl FnMut(&[u8]) -> Option<Result<T, PortMapError>>,
) -> Result<T, PortMapError> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))?;
    let mut interval = Duration::from_millis(250);
    let mut buf = [0u8; 1100];
    loop {
        socket.send_to(request, gateway)?;
        let resend_at = Instant::now() + interval;
        loop {
            let now = Instant::now();
            let wait = resend_at.min(deadline).saturating_duration_since(now);
            if wait.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(wait))?;
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from == gateway => {
                    if let Some(result) = accept(&buf[..n]) {
                        return result;
                    }
                }
                Ok(_) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) => return Err(e.into()),
            }
        }
        if Instant::now() >= deadline {
            return Err(PortMapError::Timeout);
        }
        interval = interval.saturating_mul(2);
    }
}

/// Mapping nonces a [`PcpClient`] keeps at most (one per live mapping).
const PCP_MAX_NONCES: usize = 16;

/// PCP client for one gateway.
///
/// Each mapping gets its own random nonce (RFC 6887 § 11.1), and renewing or deleting a mapping
/// reuses the nonce that created it (§ 11.2.1, § 15): a gateway answers a request whose nonce
/// differs from the mapping's with `NOT_AUTHORIZED`, and a nonce shared by several mappings would
/// let one mapping's answers match another's request.
#[derive(Debug, Clone)]
pub struct PcpClient {
    /// Gateway address (port 5351).
    pub gateway: SocketAddr,
    /// Our address on the gateway's network.
    pub client_ip: Ipv4Addr,
    /// Per-attempt timeout.
    pub timeout: Duration,
    /// `(internal port, nonce)` of each mapping this client made and has not deleted.
    nonces: Vec<(u16, [u8; 12])>,
}

impl PcpClient {
    /// A client for `gateway`.
    pub fn new(gateway: SocketAddr, client_ip: Ipv4Addr, timeout: Duration) -> Self {
        Self {
            gateway,
            client_ip,
            timeout,
            nonces: Vec::new(),
        }
    }

    /// The nonce of the mapping of `internal_port`: the one that created it, or a fresh one.
    fn nonce_for(&mut self, internal_port: u16) -> [u8; 12] {
        if let Some((_, n)) = self.nonces.iter().find(|(p, _)| *p == internal_port) {
            return *n;
        }
        let secret = crate::handshake::fresh_secret();
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&secret[..12]);
        if self.nonces.len() >= PCP_MAX_NONCES {
            self.nonces.remove(0);
        }
        self.nonces.push((internal_port, nonce));
        nonce
    }

    /// One `MAP` exchange; `suggested_external_port` is the port asked for (RFC 6887 § 11.1).
    fn map_with(
        &mut self,
        internal_port: u16,
        suggested_external_port: u16,
        lifetime_secs: u32,
    ) -> Result<PortMapping, PortMapError> {
        let nonce = self.nonce_for(internal_port);
        let request = PcpMapRequest {
            client_ip: self.client_ip,
            nonce,
            internal_port,
            suggested_external_port,
            lifetime_secs,
        }
        .encode();
        let resp = exchange(self.gateway, &request, self.timeout, |buf| {
            let resp = PcpMapResponse::decode(buf).ok()?;
            (resp.nonce == nonce && resp.internal_port == internal_port).then_some(Ok(resp))
        });
        if lifetime_secs == 0 {
            // A delete ends the mapping, answered or not: the next mapping gets a new nonce.
            self.nonces.retain(|(p, _)| *p != internal_port);
        }
        let resp = resp?;
        if resp.result != 0 {
            return Err(PortMapError::Refused(u16::from(resp.result)));
        }
        Ok(PortMapping {
            external: SocketAddr::V4(SocketAddrV4::new(resp.external_ip, resp.external_port)),
            internal_port,
            lifetime_secs: resp.lifetime_secs,
            protocol: MapProtocol::Pcp,
        })
    }
}

impl PortMapper for PcpClient {
    fn map(&mut self, internal_port: u16, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
        self.map_with(internal_port, internal_port, lifetime_secs)
    }

    /// Renews with the mapping's nonce, asking for the external port it already has (RFC 6887
    /// § 11.2.1: the suggested fields of a renewal carry the assigned ones).
    fn renew(&mut self, mapping: &PortMapping, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
        self.map_with(mapping.internal_port, mapping.external.port(), lifetime_secs)
    }

    fn release(&mut self, mapping: &PortMapping) -> Result<(), PortMapError> {
        self.map_with(mapping.internal_port, mapping.external.port(), 0)
            .map(|_| ())
    }

    fn name(&self) -> &'static str {
        "pcp"
    }
}

/// NAT-PMP client for one gateway.
#[derive(Debug, Clone)]
pub struct NatPmpClient {
    /// Gateway address (port 5351).
    pub gateway: SocketAddr,
    /// Per-attempt timeout.
    pub timeout: Duration,
}

impl NatPmpClient {
    /// A client for `gateway`.
    pub fn new(gateway: SocketAddr, timeout: Duration) -> Self {
        Self { gateway, timeout }
    }

    /// The router's public address.
    pub fn public_address(&self) -> Result<Ipv4Addr, PortMapError> {
        exchange(self.gateway, &nat_pmp_address_request(), self.timeout, |buf| {
            (buf.len() >= 2 && buf[1] == 128).then(|| nat_pmp_decode_address(buf))
        })
    }
}

impl PortMapper for NatPmpClient {
    fn map(&mut self, internal_port: u16, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
        // The address query and the MAP request share one timeout (spec: one call blocks for
        // at most the client's timeout).
        let deadline = Instant::now() + self.timeout;
        let public = exchange_until(self.gateway, &nat_pmp_address_request(), deadline, |buf| {
            (buf.len() >= 2 && buf[1] == 128).then(|| nat_pmp_decode_address(buf))
        })?;
        let request = nat_pmp_map_request(internal_port, internal_port, lifetime_secs);
        let (_, external_port, lifetime) = exchange_until(self.gateway, &request, deadline, |buf| {
            (buf.len() >= 2 && buf[1] == 128 + NAT_PMP_OP_MAP_UDP)
                .then(|| nat_pmp_decode_map(buf))
                // NAT-PMP has no nonce: a reply echoing another internal port is not ours.
                .filter(|r| r.as_ref().map_or(true, |(port, _, _)| *port == internal_port))
        })?;
        Ok(PortMapping {
            external: SocketAddr::V4(SocketAddrV4::new(public, external_port)),
            internal_port,
            lifetime_secs: lifetime,
            protocol: MapProtocol::NatPmp,
        })
    }

    fn name(&self) -> &'static str {
        "nat_pmp"
    }
}

/// Tries PCP, then NAT-PMP against `gateway`, then UPnP IGD on the LAN; returns the first grant,
/// or the last error.
pub fn try_port_mapping(
    gateway: Ipv4Addr,
    client_ip: Ipv4Addr,
    internal_port: u16,
    lifetime_secs: u32,
    timeout: Duration,
) -> Result<PortMapping, PortMapError> {
    let addr = SocketAddr::V4(SocketAddrV4::new(gateway, PORT_MAP_SERVER_PORT));
    let pcp = match PcpClient::new(addr, client_ip, timeout).map(internal_port, lifetime_secs) {
        Ok(m) => return Ok(m),
        Err(e) => e,
    };
    tracing::debug!(error = %pcp, "PCP mapping failed, trying NAT-PMP");
    let nat_pmp = match NatPmpClient::new(addr, timeout).map(internal_port, lifetime_secs) {
        Ok(m) => return Ok(m),
        Err(e) => e,
    };
    tracing::debug!(error = %nat_pmp, "NAT-PMP mapping failed, trying UPnP");
    crate::upnp::UpnpClient::new(client_ip, timeout).map(internal_port, lifetime_secs)
}

/// A port mapper that can run on its own thread (each racer of [`race_port_mappers`] does).
pub type BoxedPortMapper = Box<dyn PortMapper + Send>;

/// The three standard mappers for `gateway`: PCP, NAT-PMP and UPnP IGD (spec 120 § 10.7), for
/// [`race_port_mappers`].
pub fn standard_mappers(gateway: Ipv4Addr, client_ip: Ipv4Addr, timeout: Duration) -> Vec<BoxedPortMapper> {
    let addr = SocketAddr::V4(SocketAddrV4::new(gateway, PORT_MAP_SERVER_PORT));
    vec![
        Box::new(PcpClient::new(addr, client_ip, timeout)),
        Box::new(NatPmpClient::new(addr, timeout)),
        Box::new(crate::upnp::UpnpClient::new(client_ip, timeout)),
    ]
}

/// Every racer of [`race_port_mappers`] failed: each one's name and error.
#[derive(Debug, Error)]
#[error("no port mapping: {}", self.summary())]
pub struct PortMapRaceError {
    /// `(mapper name, error)` of each racer, in the order they failed.
    pub failures: Vec<(&'static str, PortMapError)>,
}

impl PortMapRaceError {
    fn summary(&self) -> String {
        if self.failures.is_empty() {
            return "no mapper to race".to_owned();
        }
        self.failures
            .iter()
            .map(|(name, e)| format!("{name}: {e}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Stable lowercase code (agent protocol): `port_map_refused` when a gateway answered and
    /// refused, `port_map_unavailable` otherwise (no gateway answered, nothing to race).
    pub fn code(&self) -> &'static str {
        if self
            .failures
            .iter()
            .any(|(_, e)| matches!(e, PortMapError::Refused(_)))
        {
            "port_map_refused"
        } else {
            "port_map_unavailable"
        }
    }
}

/// What a racer reports back.
enum RaceOutcome {
    Won(PortMapping, BoxedPortMapper),
    Failed(&'static str, PortMapError),
}

/// Races `mappers` for a mapping of `internal_port` (spec 120 § 10.7, the DO27 hosting flow): each
/// asks its gateway at once on its own thread, the first grant wins and becomes the lease, and a
/// racer that is granted a mapping after another won deletes it again, so a router that speaks two
/// protocols is not left with a stray mapping. Blocks until a racer wins or every racer failed
/// (at most the slowest mapper's timeout).
///
/// # Errors
///
/// Every racer failed (or there were none).
pub fn race_port_mappers(
    mappers: Vec<BoxedPortMapper>,
    internal_port: u16,
    lifetime_secs: u32,
    now_ms: u64,
) -> Result<PortMapLease, PortMapRaceError> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let won = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<RaceOutcome>();
    let mut failures = Vec::new();
    for mut mapper in mappers {
        let name = mapper.name();
        let won = Arc::clone(&won);
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("regia-port-map-{name}"))
            .spawn(move || {
                let outcome = match mapper.map(internal_port, lifetime_secs) {
                    Ok(mapping) => {
                        if won
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            RaceOutcome::Won(mapping, mapper)
                        } else {
                            // A slower grant: give it back so only the winner's mapping exists.
                            if let Err(e) = mapper.release(&mapping) {
                                tracing::debug!(error = %e, mapper = name, "port map: a losing mapping was not deleted");
                            }
                            return;
                        }
                    }
                    Err(e) => RaceOutcome::Failed(name, e),
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = spawned {
            failures.push((name, PortMapError::Io(e)));
        }
    }
    drop(tx);
    while let Ok(outcome) = rx.recv() {
        match outcome {
            RaceOutcome::Won(mapping, mapper) => {
                tracing::info!(
                    mapper = mapper.name(),
                    external = %mapping.external,
                    lifetime_s = mapping.lifetime_secs,
                    "port map: mapped"
                );
                return Ok(PortMapLease::new(mapper, mapping, lifetime_secs, now_ms));
            }
            RaceOutcome::Failed(name, e) => {
                tracing::debug!(error = %e, mapper = name, "port map: racer failed");
                failures.push((name, e));
            }
        }
    }
    Err(PortMapRaceError { failures })
}

/// Least time between two renewal attempts after one failed, milliseconds.
pub const PORT_MAP_RENEW_RETRY_MIN_MS: u64 = 1_000;
/// Most time between two renewal attempts after one failed, milliseconds.
pub const PORT_MAP_RENEW_RETRY_MAX_MS: u64 = 30_000;

/// What one [`PortMapLease::update`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaseEvent {
    /// Nothing was due.
    Idle,
    /// The mapping was renewed; it may have a new external address or lifetime.
    Renewed(PortMapping),
    /// A renewal failed; another attempt follows before the mapping lapses.
    RenewFailed(String),
    /// The mapping's lifetime passed without a renewal: it is gone (drop the mapped candidate).
    Lapsed,
}

/// A granted mapping and the mapper that keeps it: renewed at half its lifetime (RFC 6886 § 3.3;
/// RFC 6887 § 11.2.1 allows 1/2 to 5/8), deleted by [`PortMapLease::release`] or on drop.
/// Sans-clock: the caller passes its milliseconds.
pub struct PortMapLease {
    mapper: BoxedPortMapper,
    mapping: PortMapping,
    lifetime_secs: u32,
    granted_ms: u64,
    next_renew_ms: u64,
    renewals: u64,
    renew_failures: u64,
    lapsed: bool,
    released: bool,
}

impl std::fmt::Debug for PortMapLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortMapLease")
            .field("mapper", &self.mapper.name())
            .field("mapping", &self.mapping)
            .field("next_renew_ms", &self.next_renew_ms)
            .finish_non_exhaustive()
    }
}

impl PortMapLease {
    /// A lease on `mapping`, granted at `now_ms` by `mapper`, renewed for `lifetime_secs` each time.
    pub fn new(mapper: BoxedPortMapper, mapping: PortMapping, lifetime_secs: u32, now_ms: u64) -> Self {
        let mut lease = Self {
            mapper,
            mapping,
            lifetime_secs,
            granted_ms: now_ms,
            next_renew_ms: u64::MAX,
            renewals: 0,
            renew_failures: 0,
            lapsed: false,
            released: false,
        };
        lease.next_renew_ms = lease.half_life_ms(now_ms);
        lease
    }

    /// When the current grant ends; `u64::MAX` for a permanent one (lifetime 0).
    pub fn expires_ms(&self) -> u64 {
        match self.mapping.lifetime_secs {
            0 => u64::MAX,
            s => self.granted_ms.saturating_add(u64::from(s) * 1000),
        }
    }

    fn half_life_ms(&self, granted_ms: u64) -> u64 {
        match self.mapping.lifetime_secs {
            0 => u64::MAX,
            s => granted_ms.saturating_add(u64::from(s) * 500),
        }
    }

    /// The current mapping.
    pub fn mapping(&self) -> &PortMapping {
        &self.mapping
    }

    /// The mapper that granted it (`pcp`, `nat_pmp`, `upnp`, ...).
    pub fn mapper_name(&self) -> &'static str {
        self.mapper.name()
    }

    /// When the next renewal is due.
    pub fn next_renew_ms(&self) -> u64 {
        self.next_renew_ms
    }

    /// Renewals that succeeded.
    pub fn renewals(&self) -> u64 {
        self.renewals
    }

    /// Renewal attempts that failed.
    pub fn renew_failures(&self) -> u64 {
        self.renew_failures
    }

    /// The mapping lapsed (its lifetime passed without a renewal).
    pub fn lapsed(&self) -> bool {
        self.lapsed
    }

    /// Renews when due (blocks for at most the mapper's timeout then). After a failed renewal the
    /// next attempt comes after a quarter of the time left (1 s to 30 s); once the grant's lifetime
    /// has passed the lease reports [`LeaseEvent::Lapsed`] once, and keeps trying to map again.
    pub fn update(&mut self, now_ms: u64) -> LeaseEvent {
        if self.released || now_ms < self.next_renew_ms {
            return LeaseEvent::Idle;
        }
        match self.mapper.renew(&self.mapping, self.lifetime_secs) {
            Ok(mapping) => {
                self.mapping = mapping;
                self.granted_ms = now_ms;
                self.next_renew_ms = self.half_life_ms(now_ms);
                self.renewals += 1;
                self.lapsed = false;
                LeaseEvent::Renewed(mapping)
            }
            Err(e) => {
                self.renew_failures += 1;
                let left = self.expires_ms().saturating_sub(now_ms);
                self.next_renew_ms = now_ms.saturating_add(
                    (left / 4).clamp(PORT_MAP_RENEW_RETRY_MIN_MS, PORT_MAP_RENEW_RETRY_MAX_MS),
                );
                if !self.lapsed && now_ms >= self.expires_ms() {
                    self.lapsed = true;
                    return LeaseEvent::Lapsed;
                }
                LeaseEvent::RenewFailed(e.to_string())
            }
        }
    }

    /// Deletes the mapping at the gateway (a request with lifetime 0). Called on drop too.
    ///
    /// # Errors
    ///
    /// The gateway did not confirm the delete (the mapping then lapses on its own).
    pub fn release(mut self) -> Result<(), PortMapError> {
        self.release_now()
    }

    fn release_now(&mut self) -> Result<(), PortMapError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        self.mapper.release(&self.mapping)
    }
}

impl Drop for PortMapLease {
    fn drop(&mut self) {
        if let Err(e) = self.release_now() {
            tracing::debug!(error = %e, "port map: the mapping was not deleted at shutdown");
        }
    }
}

/// The default IPv4 gateway, read from the operating system's routing table (ADR-8893):
/// `/proc/net/route` and then a netlink route dump on Linux and Android (all routing tables, so
/// Android's per-network tables count; no root), IP Helper's `GetIpForwardTable` on Windows and
/// the `PF_ROUTE` sysctl on macOS. [`PortMapError::Unsupported`] when there is no IPv4 default
/// route, when the platform denies the table (some Android builds), or on a platform without a
/// reader; the hosting flow then reports `port_map_unavailable`. Pass the gateway explicitly there.
pub fn default_gateway_v4() -> Result<Ipv4Addr, PortMapError> {
    platform::default_gateway_v4()
}

/// Lifetime a hosting flow asks its port mapping for, seconds (RFC 6886 § 3.3 recommends two
/// hours); the lease renews it at half.
pub const HOST_PORT_MAP_LIFETIME_S: u32 = 7200;
/// Per-mapper timeout of the hosting flow's race.
pub const HOST_PORT_MAP_TIMEOUT: Duration = Duration::from_secs(3);

/// What a [`PortMapWorker`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortMapEvent {
    /// A mapping was granted (the race) or renewed (perhaps with a new address).
    Mapped {
        /// The mapper that holds it: `pcp`, `nat_pmp` or `upnp`.
        mapper: &'static str,
        /// The mapping.
        mapping: PortMapping,
        /// Renewals so far.
        renewals: u64,
    },
    /// No mapping: `port_map_refused` or `port_map_unavailable` (the race failed, or there is no
    /// gateway to ask).
    Failed(&'static str),
    /// The mapping's lifetime passed without a renewal; the worker keeps trying.
    Lapsed,
}

/// The hosting flow's port mapping on its own thread (spec 120 § 10.7): it races `mappers` once,
/// renews the winner at half its lifetime and deletes it when the worker stops or drops. The
/// host's loop reads [`PortMapWorker::poll`] each tick and never blocks on a gateway.
#[derive(Debug)]
pub struct PortMapWorker {
    stop: Option<std::sync::mpsc::Sender<()>>,
    events: std::sync::mpsc::Receiver<PortMapEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PortMapWorker {
    /// Maps the game socket's `internal_port` on the default gateway (PCP, NAT-PMP and UPnP IGD
    /// raced), with `client_ip` as this machine's address toward it.
    pub fn start(internal_port: u16, client_ip: Ipv4Addr) -> Self {
        match default_gateway_v4() {
            Ok(gateway) => Self::with_mappers(
                standard_mappers(gateway, client_ip, HOST_PORT_MAP_TIMEOUT),
                internal_port,
                HOST_PORT_MAP_LIFETIME_S,
            ),
            Err(e) => {
                tracing::info!(error = %e, "port map: no gateway to ask");
                Self::failed("port_map_unavailable")
            }
        }
    }

    /// A worker that only reports `code` (nothing to race).
    pub fn failed(code: &'static str) -> Self {
        let (tx, events) = std::sync::mpsc::channel();
        let _ = tx.send(PortMapEvent::Failed(code));
        Self {
            stop: None,
            events,
            thread: None,
        }
    }

    /// Races `mappers` for `internal_port` on a new thread and keeps the winner for
    /// `lifetime_secs` at a time.
    pub fn with_mappers(mappers: Vec<BoxedPortMapper>, internal_port: u16, lifetime_secs: u32) -> Self {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (tx, events) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("regia-port-map".to_owned())
            .spawn(move || {
                let start = Instant::now();
                let now_ms = || start.elapsed().as_millis() as u64;
                let mut lease = match race_port_mappers(mappers, internal_port, lifetime_secs, now_ms()) {
                    Ok(lease) => lease,
                    Err(e) => {
                        tracing::info!(error = %e, "port map: no mapping; clients reach this host through the relay or a punch");
                        let _ = tx.send(PortMapEvent::Failed(e.code()));
                        return;
                    }
                };
                let _ = tx.send(PortMapEvent::Mapped {
                    mapper: lease.mapper_name(),
                    mapping: *lease.mapping(),
                    renewals: 0,
                });
                loop {
                    let wait = lease.next_renew_ms().saturating_sub(now_ms()).min(60_000);
                    match stop_rx.recv_timeout(Duration::from_millis(wait)) {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        // Stopped, or the worker was dropped: delete the mapping.
                        _ => break,
                    }
                    let event = match lease.update(now_ms()) {
                        LeaseEvent::Renewed(mapping) => PortMapEvent::Mapped {
                            mapper: lease.mapper_name(),
                            mapping,
                            renewals: lease.renewals(),
                        },
                        LeaseEvent::Lapsed => PortMapEvent::Lapsed,
                        LeaseEvent::RenewFailed(e) => {
                            tracing::debug!(error = %e, "port map: renewal failed, retrying");
                            continue;
                        }
                        LeaseEvent::Idle => continue,
                    };
                    let _ = tx.send(event);
                }
                if let Err(e) = lease.release() {
                    tracing::debug!(error = %e, "port map: the mapping was not deleted at shutdown");
                }
            });
        match spawned {
            Ok(thread) => Self {
                stop: Some(stop_tx),
                events,
                thread: Some(thread),
            },
            Err(e) => {
                tracing::warn!(error = %e, "port map: no thread");
                Self::failed("port_map_unavailable")
            }
        }
    }

    /// Events since the last call.
    pub fn poll(&self) -> Vec<PortMapEvent> {
        self.events.try_iter().collect()
    }

    /// Waits up to `timeout` for the next event (tools and tests; a host loop uses
    /// [`Self::poll`]).
    pub fn wait_event(&self, timeout: Duration) -> Option<PortMapEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Stops the worker: the mapping is deleted (blocks for at most the mapper's timeout, or the
    /// race's when it is still running).
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PortMapWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Parses `/proc/net/route` text: the gateway of the first default route.
pub fn parse_proc_route(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 || cols[1] != "00000000" {
            return None;
        }
        let gw = u32::from_str_radix(cols[2], 16).ok()?;
        // The kernel prints the network-order address bytes read as a host-order integer.
        (gw != 0).then(|| Ipv4Addr::from(gw.to_ne_bytes()))
    })
}

/// An in-memory router for tests and offline play.
#[derive(Debug, Clone)]
pub struct FakeRouter {
    /// Public address it hands out.
    pub public_ip: Ipv4Addr,
    /// Refuse every request.
    pub refuse: bool,
    /// Live mappings: internal port → (external port, lifetime).
    pub mappings: Vec<(u16, u16, u32)>,
}

impl FakeRouter {
    /// A cooperative router.
    pub fn new(public_ip: Ipv4Addr) -> Self {
        Self {
            public_ip,
            refuse: false,
            mappings: Vec::new(),
        }
    }
}

impl PortMapper for FakeRouter {
    fn map(&mut self, internal_port: u16, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
        if self.refuse {
            return Err(PortMapError::Refused(2));
        }
        self.mappings.retain(|(i, _, _)| *i != internal_port);
        if lifetime_secs > 0 {
            self.mappings.push((internal_port, internal_port, lifetime_secs));
        }
        Ok(PortMapping {
            external: SocketAddr::V4(SocketAddrV4::new(self.public_ip, internal_port)),
            internal_port,
            lifetime_secs,
            protocol: MapProtocol::Fake,
        })
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcp_codec_round_trip_and_layout() {
        let req = PcpMapRequest {
            client_ip: Ipv4Addr::new(192, 168, 1, 20),
            nonce: [7; 12],
            internal_port: 7777,
            suggested_external_port: 7777,
            lifetime_secs: 3600,
        }
        .encode();
        assert_eq!(req.len(), 60);
        assert_eq!(req[0], 2);
        assert_eq!(
            &req[8..24],
            &Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped().octets()
        );
        assert_eq!(u16::from_be_bytes([req[40], req[41]]), 7777);
        let resp = PcpMapResponse {
            result: 0,
            lifetime_secs: 1800,
            nonce: [7; 12],
            internal_port: 7777,
            external_port: 40_001,
            external_ip: Ipv4Addr::new(203, 0, 113, 5),
        };
        assert_eq!(PcpMapResponse::decode(&resp.encode()).expect("valid"), resp);
        assert!(
            PcpMapResponse::decode(&req).is_err(),
            "a request is not a response"
        );
        assert!(PcpMapResponse::decode(&[2, 0x81]).is_err());
    }

    #[test]
    fn a_pcp_error_reply_with_a_zeroed_address_keeps_its_result_code() {
        let mut resp = PcpMapResponse {
            result: 2,
            lifetime_secs: 0,
            nonce: [7; 12],
            internal_port: 7777,
            external_port: 0,
            external_ip: Ipv4Addr::UNSPECIFIED,
        }
        .encode();
        resp[44..60].fill(0);
        let back = PcpMapResponse::decode(&resp).expect("an error reply decodes");
        assert_eq!((back.result, back.nonce), (2, [7; 12]));
        // A success with a non-IPv4 address stays malformed.
        resp[3] = 0;
        assert!(PcpMapResponse::decode(&resp).is_err());
    }

    #[test]
    fn nat_pmp_codec_round_trip() {
        let addr = nat_pmp_encode_address(Ipv4Addr::new(203, 0, 113, 9), 5);
        assert_eq!(
            nat_pmp_decode_address(&addr).expect("valid"),
            Ipv4Addr::new(203, 0, 113, 9)
        );
        let map = nat_pmp_encode_map(0, 7777, 50_000, 7200);
        assert_eq!(nat_pmp_decode_map(&map).expect("valid"), (7777, 50_000, 7200));
        assert!(matches!(
            nat_pmp_decode_map(&nat_pmp_encode_map(3, 1, 1, 1)),
            Err(PortMapError::Refused(3))
        ));
        let req = nat_pmp_map_request(7777, 7777, 60);
        assert_eq!(req[1], 1);
        assert_eq!(u32::from_be_bytes([req[8], req[9], req[10], req[11]]), 60);
    }

    #[test]
    fn proc_route_parsing() {
        let table = "Iface\tDestination\tGateway \tFlags\n\
                     eth0\t0001A8C0\t00000000\t0001\n\
                     eth0\t00000000\t0101A8C0\t0003\n";
        assert_eq!(parse_proc_route(table), Some(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(parse_proc_route("Iface\tDestination\tGateway\n"), None);
    }

    #[test]
    fn fake_router_grants_renews_and_releases() {
        let mut r = FakeRouter::new(Ipv4Addr::new(203, 0, 113, 1));
        let m = r.map(7777, 60).expect("granted");
        assert_eq!(m.external.port(), 7777);
        let m = r.renew(&m, 120).expect("renewed");
        assert_eq!(r.mappings, vec![(7777, 7777, 120)]);
        r.release(&m).expect("released");
        assert!(r.mappings.is_empty());
        r.refuse = true;
        assert!(matches!(r.map(1, 1), Err(PortMapError::Refused(_))));
    }

    #[test]
    fn pcp_client_against_a_loopback_gateway() {
        let Ok(gateway) = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            return; // no loopback in this sandbox
        };
        let gw_addr = gateway.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let mut buf = [0u8; 128];
            let _ = gateway.set_read_timeout(Some(Duration::from_secs(10)));
            let Ok((n, from)) = gateway.recv_from(&mut buf) else {
                return;
            };
            if n < PCP_MAP_BYTES {
                return;
            }
            let mut nonce = [0u8; 12];
            nonce.copy_from_slice(&buf[24..36]);
            let resp = PcpMapResponse {
                result: 0,
                lifetime_secs: 600,
                nonce,
                internal_port: u16::from_be_bytes([buf[40], buf[41]]),
                external_port: 41_000,
                external_ip: Ipv4Addr::new(203, 0, 113, 77),
            };
            let _ = gateway.send_to(&resp.encode(), from);
        });
        let mut client = PcpClient::new(gw_addr, Ipv4Addr::LOCALHOST, Duration::from_secs(10));
        let m = client.map(7777, 600).expect("mapped");
        assert_eq!(m.external, SocketAddr::from(([203, 0, 113, 77], 41_000)));
        assert_eq!(m.protocol, MapProtocol::Pcp);
        server.join().expect("fake gateway thread");
    }

    /// The address query and the MAP request share one timeout. A gateway that answers the
    /// address query on its resend, then drops MAP, produces one MAP packet and `Timeout`.
    #[test]
    fn nat_pmp_map_shares_one_timeout() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let Ok(gateway) = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            return;
        };
        let gw_addr = gateway.local_addr().expect("addr");
        let maps = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&maps);
        let server = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let _ = gateway.set_read_timeout(Some(Duration::from_secs(2)));
            let mut address_seen = 0u32;
            while let Ok((n, from)) = gateway.recv_from(&mut buf) {
                if n < 2 {
                    continue;
                }
                if buf[1] == 0 {
                    address_seen += 1;
                    // Reply on the resend (250 ms into a 400 ms budget) so MAP has under 250 ms left.
                    if address_seen >= 2 {
                        let resp = nat_pmp_encode_address(Ipv4Addr::new(203, 0, 113, 9), 0);
                        let _ = gateway.send_to(&resp, from);
                    }
                } else if buf[1] == NAT_PMP_OP_MAP_UDP {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let mut client = NatPmpClient::new(gw_addr, Duration::from_millis(400));
        let err = client.map(7777, 60).expect_err("MAP is dropped");
        assert!(matches!(err, PortMapError::Timeout), "{err:?}");
        assert_eq!(
            maps.load(Ordering::SeqCst),
            1,
            "the shared deadline leaves no room for a MAP resend"
        );
        server.join().expect("fake gateway thread");
    }

    /// Plan N229 (the known port-map edge): each PCP mapping gets its own nonce, and a renewal
    /// or delete reuses the nonce of the mapping it names and asks for the external port that
    /// mapping already has (RFC 6887 § 11.1, § 11.2.1). On the base one nonce served every mapping
    /// of a client and a renewal asked for the internal port again.
    #[test]
    fn pcp_nonces_are_per_mapping_and_kept_for_renew_and_delete() {
        let Ok(gateway) = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            return; // no loopback in this sandbox
        };
        let gw_addr = gateway.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 128];
            let _ = gateway.set_read_timeout(Some(Duration::from_secs(10)));
            while seen.len() < 4 {
                let Ok((n, from)) = gateway.recv_from(&mut buf) else {
                    break;
                };
                if n < PCP_MAP_BYTES {
                    continue;
                }
                let mut nonce = [0u8; 12];
                nonce.copy_from_slice(&buf[24..36]);
                let internal_port = u16::from_be_bytes([buf[40], buf[41]]);
                let suggested = u16::from_be_bytes([buf[42], buf[43]]);
                let lifetime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                seen.push((nonce, internal_port, suggested, lifetime));
                let resp = PcpMapResponse {
                    result: 0,
                    lifetime_secs: lifetime,
                    nonce,
                    internal_port,
                    // The gateway assigns another port than the one suggested.
                    external_port: internal_port + 30_000,
                    external_ip: Ipv4Addr::new(203, 0, 113, 77),
                };
                let _ = gateway.send_to(&resp.encode(), from);
            }
            seen
        });
        let mut client = PcpClient::new(gw_addr, Ipv4Addr::LOCALHOST, Duration::from_secs(10));
        let first = client.map(7777, 600).expect("first mapping");
        let _second = client.map(7778, 600).expect("second mapping");
        let renewed = client.renew(&first, 600).expect("renewed");
        assert_eq!(renewed.external, first.external);
        client.release(&renewed).expect("deleted");
        let seen = server.join().expect("fake gateway thread");
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert_ne!(seen[0].0, seen[1].0, "two mappings share a PCP nonce");
        assert_eq!(seen[2].0, seen[0].0, "a renewal must carry its mapping's nonce");
        assert_eq!(seen[3].0, seen[0].0, "a delete must carry its mapping's nonce");
        assert_eq!(
            seen[2].2, 37_777,
            "a renewal suggests the external port the mapping has"
        );
        assert_eq!((seen[3].1, seen[3].3), (7777, 0), "the delete names the mapping");
    }

    /// A scripted mapper: grants or refuses by script, and logs every call.
    struct Scripted {
        log: std::sync::Arc<std::sync::Mutex<Vec<(u16, u32)>>>,
        fail_renewals: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl PortMapper for Scripted {
        fn map(&mut self, internal_port: u16, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
            self.log.lock().expect("log").push((internal_port, lifetime_secs));
            let calls = self.log.lock().expect("log").len();
            if calls > 1 && lifetime_secs > 0 && self.fail_renewals.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(PortMapError::Timeout);
            }
            Ok(PortMapping {
                external: SocketAddr::from(([203, 0, 113, 1], internal_port)),
                internal_port,
                lifetime_secs,
                protocol: MapProtocol::Fake,
            })
        }
    }

    /// The lease renews at half the lifetime, retries a failed renewal before the end, reports a
    /// lapse once when the lifetime passes, and deletes the mapping when dropped.
    #[test]
    fn a_lease_renews_at_half_life_retries_lapses_and_deletes_on_drop() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let log = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let mut mapper = Scripted {
            log: Arc::clone(&log),
            fail_renewals: Arc::clone(&fail),
        };
        let mapping = mapper.map(7777, 120).expect("granted");
        let mut lease = PortMapLease::new(Box::new(mapper), mapping, 120, 1_000);
        assert_eq!(lease.next_renew_ms(), 61_000);
        assert_eq!(lease.update(60_999), LeaseEvent::Idle);
        assert!(matches!(lease.update(61_000), LeaseEvent::Renewed(_)));
        assert_eq!(lease.renewals(), 1);
        assert_eq!(lease.expires_ms(), 181_000);
        fail.store(true, Ordering::SeqCst);
        assert!(matches!(lease.update(121_000), LeaseEvent::RenewFailed(_)));
        // A quarter of the 60 s left (between 1 s and 30 s).
        assert_eq!(lease.next_renew_ms(), 136_000);
        let mut events = Vec::new();
        let mut now = 136_000;
        while now <= 200_000 {
            let e = lease.update(now);
            if e != LeaseEvent::Idle {
                events.push((now, e));
            }
            now += 500;
        }
        let lapses = events.iter().filter(|(_, e)| *e == LeaseEvent::Lapsed).count();
        assert_eq!(lapses, 1, "{events:?}");
        assert!(
            events
                .iter()
                .any(|(t, e)| *e == LeaseEvent::Lapsed && *t >= 181_000)
        );
        drop(lease);
        let calls = log.lock().expect("log").clone();
        assert_eq!(
            calls.last(),
            Some(&(7777, 0)),
            "dropping the lease deletes the mapping"
        );
    }

    /// The worker reports the race's grant, and stopping it deletes the mapping.
    #[test]
    fn the_worker_reports_the_grant_and_deletes_the_mapping_when_stopped() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};
        let log = Arc::new(Mutex::new(Vec::new()));
        let mapper = Scripted {
            log: Arc::clone(&log),
            fail_renewals: Arc::new(AtomicBool::new(false)),
        };
        let mut worker = PortMapWorker::with_mappers(vec![Box::new(mapper)], 7777, 7200);
        let event = worker.wait_event(Duration::from_secs(10)).expect("an event");
        assert!(
            matches!(event, PortMapEvent::Mapped { mapping, renewals: 0, .. } if mapping.internal_port == 7777),
            "{event:?}"
        );
        worker.stop();
        assert_eq!(
            log.lock().expect("log").as_slice(),
            &[(7777, 7200), (7777, 0)],
            "granted once, deleted at stop"
        );
        let failed = PortMapWorker::failed("port_map_unavailable");
        assert_eq!(failed.poll(), vec![PortMapEvent::Failed("port_map_unavailable")]);
    }
}
