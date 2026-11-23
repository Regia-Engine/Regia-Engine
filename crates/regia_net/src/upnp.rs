// SPDX-License-Identifier: MPL-2.0
//! UPnP Internet Gateway Device port mapping, the fallback after PCP and NAT-PMP.
//!
//! Three steps, from the public UPnP Device Architecture 1.1 and the IGD `WANIPConnection` service
//! specifications: an SSDP `M-SEARCH` (HTTP over UDP multicast to 239.255.255.250:1900) finds the
//! gateway's description URL; an HTTP `GET` of that XML names the `WANIPConnection` (or
//! `WANPPPConnection`) service and its control URL; SOAP `POST`s to the control URL call
//! `GetExternalIPAddress`, `AddPortMapping` and `DeletePortMapping`.
//!
//! Only what a game host needs is parsed: no general XML parser, tags are matched as text and
//! bodies are bounded. Each step (the search, the description fetch, each SOAP call) blocks at most
//! the client's timeout as a whole, however slowly a peer answers; use them at session setup, never
//! per frame. Discovery takes only an answer to its own search (`ST` equal to the gateway target).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use crate::port_map::{MapProtocol, PortMapError, PortMapper, PortMapping};

/// The SSDP multicast group and port.
pub const SSDP_MULTICAST: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
/// Search target for an Internet Gateway Device.
pub const IGD_SEARCH_TARGET: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";
/// Service types that can map ports, most capable first.
pub const WAN_SERVICE_TYPES: &[&str] = &[
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];
/// Largest HTTP response read.
const MAX_HTTP_BYTES: usize = 64 * 1024;
/// The IGD error `OnlyPermanentLeasesSupported` (IGD v1 `WANIPConnection`, error 725).
pub const UPNP_ONLY_PERMANENT_LEASES: u16 = 725;

/// The `M-SEARCH` request.
pub fn ssdp_search_request() -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_MULTICAST}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {IGD_SEARCH_TARGET}\r\n\r\n"
    )
}

/// The `LOCATION` header of an SSDP response (header names are case-insensitive).
pub fn parse_ssdp_location(response: &str) -> Option<String> {
    ssdp_header(response, "location")
}

/// The value of header `name` (case-insensitive) in an SSDP datagram.
fn ssdp_header(datagram: &str, name: &str) -> Option<String> {
    datagram.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// The description URL of an answer to [`ssdp_search_request`]: a `200` search response whose `ST`
/// is [`IGD_SEARCH_TARGET`]. A `NOTIFY` announcement or an answer for another search target is
/// `None`, so the first chatty device on the LAN cannot steer discovery.
pub fn parse_ssdp_answer(datagram: &str) -> Option<String> {
    let status = datagram.lines().next()?;
    let mut words = status.split_whitespace();
    let version = words.next()?;
    if !version.to_ascii_uppercase().starts_with("HTTP/1.") || words.next() != Some("200") {
        return None;
    }
    let target = ssdp_header(datagram, "st")?;
    if !target.eq_ignore_ascii_case(IGD_SEARCH_TARGET) {
        return None;
    }
    parse_ssdp_location(datagram)
}

/// Splits `http://a.b.c.d[:port]/path` into the address and the path. Gateways advertise IPv4
/// literals; host names are refused (no DNS at session setup).
pub fn parse_http_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, 80),
    };
    let ip: Ipv4Addr = host.parse().ok()?;
    // The path goes into a request line: no way out of it.
    if !path.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    Some((SocketAddr::V4(SocketAddrV4::new(ip, port)), path.to_owned()))
}

/// Whether a discovered service can be used: it lives on the host that answered the search, and its
/// type is fit for a header. Any LAN device can answer an SSDP search; this keeps the answer from
/// steering this host's HTTP at a third machine.
fn service_is_on(service: &UpnpService, responder: IpAddr) -> bool {
    parse_http_url(&service.control_url).is_some_and(|(addr, _)| addr.ip() == responder)
        && service
            .service_type
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"')
}

/// The text between `<tag>` and `</tag>` (first occurrence, namespace prefixes allowed).
pub fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut search = 0;
    while let Some(rel) = xml[search..].find('<') {
        let start = search + rel + 1;
        let end = start + xml[start..].find('>')?;
        let name = &xml[start..end];
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tag {
            let body_start = end + 1;
            let close = xml[body_start..].find("</")? + body_start;
            return Some(xml[body_start..close].trim());
        }
        search = end;
    }
    None
}

/// A mapping-capable service found in a device description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpnpService {
    /// Its service type URN.
    pub service_type: String,
    /// Absolute control URL.
    pub control_url: String,
}

/// Finds the best WAN service in a device description fetched from `location`.
pub fn find_wan_service(description: &str, location: &str) -> Option<UpnpService> {
    let found: Vec<UpnpService> = description
        .split("<service>")
        .skip(1)
        .filter_map(|block| {
            let block = block.split("</service>").next().unwrap_or(block);
            let kind = xml_text(block, "serviceType")?;
            let control = xml_text(block, "controlURL")?;
            WAN_SERVICE_TYPES.contains(&kind).then(|| UpnpService {
                service_type: kind.to_owned(),
                control_url: absolute_url(location, control),
            })
        })
        .collect();
    WAN_SERVICE_TYPES
        .iter()
        .find_map(|kind| found.iter().find(|s| s.service_type == *kind).cloned())
}

fn absolute_url(location: &str, control: &str) -> String {
    if control.starts_with("http://") {
        return control.to_owned();
    }
    let base_end = location
        .strip_prefix("http://")
        .and_then(|r| r.find('/'))
        .map_or(location.len(), |i| i + "http://".len());
    let slash = if control.starts_with('/') { "" } else { "/" };
    format!("{}{slash}{control}", &location[..base_end])
}

/// A SOAP request body for `action` on `service_type`.
pub fn soap_envelope(service_type: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut body = String::new();
    for (name, value) in args {
        body.push_str(&format!("<{name}>{value}</{name}>"));
    }
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} \
         xmlns:u=\"{service_type}\">{body}</u:{action}></s:Body></s:Envelope>\r\n"
    )
}

/// Reads until EOF, `MAX_HTTP_BYTES` or `deadline`, whichever comes first: the socket timeout
/// alone restarts with every byte, so a peer that trickles its answer could hold the call for
/// `MAX_HTTP_BYTES` timeouts.
fn read_until(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>, PortMapError> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    while raw.len() < MAX_HTTP_BYTES {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(PortMapError::Timeout);
        }
        stream.set_read_timeout(Some(left))?;
        let want = buf.len().min(MAX_HTTP_BYTES - raw.len());
        match stream.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(PortMapError::Timeout);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(raw)
}

/// One HTTP/1.1 exchange; returns the body of a `200` response. The whole exchange (connect,
/// send, answer) takes at most `timeout`.
fn http(addr: SocketAddr, request: &str, timeout: Duration) -> Result<String, PortMapError> {
    let deadline = Instant::now() + timeout;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(PortMapError::Timeout);
    }
    stream.set_write_timeout(Some(left))?;
    stream.write_all(request.as_bytes())?;
    let raw = read_until(&mut stream, deadline)?;
    let text = String::from_utf8_lossy(&raw);
    let status = text.lines().next().unwrap_or("");
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    if code != 200 {
        let upnp_code = xml_text(&body, "errorCode")
            .and_then(|c| c.parse().ok())
            .unwrap_or(code);
        return Err(PortMapError::Refused(upnp_code));
    }
    Ok(body)
}

/// UPnP IGD client.
#[derive(Debug, Clone)]
pub struct UpnpClient {
    /// Where the `M-SEARCH` goes (the multicast group, or a test responder).
    pub ssdp: SocketAddr,
    /// Our LAN address (the mapping's internal client).
    pub client_ip: Ipv4Addr,
    /// Per-step timeout.
    pub timeout: Duration,
    service: Option<UpnpService>,
}

impl UpnpClient {
    /// A client that searches the standard multicast group.
    pub fn new(client_ip: Ipv4Addr, timeout: Duration) -> Self {
        Self::with_ssdp(SocketAddr::V4(SSDP_MULTICAST), client_ip, timeout)
    }

    /// A client that sends its search to `ssdp`.
    pub fn with_ssdp(ssdp: SocketAddr, client_ip: Ipv4Addr, timeout: Duration) -> Self {
        Self {
            ssdp,
            client_ip,
            timeout,
            service: None,
        }
    }

    /// Waits for an answer to our own `M-SEARCH` until `deadline`. `recv` performs one receive
    /// (with the given remaining wait); it is injectable so a socket error can be scripted in
    /// tests. Returns the `LOCATION` and the responder.
    fn wait_for_ssdp_answer(
        mut recv: impl FnMut(Duration, &mut [u8]) -> std::io::Result<(usize, SocketAddr)>,
        deadline: Instant,
        buf: &mut [u8],
    ) -> Result<(String, IpAddr), PortMapError> {
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return Err(PortMapError::Timeout);
            }
            match recv(wait, buf) {
                Ok((n, from)) => {
                    if let Some(loc) = parse_ssdp_answer(&String::from_utf8_lossy(&buf[..n])) {
                        return Ok((loc, from.ip()));
                    }
                }
                // Windows reports an ICMP port-unreachable for an earlier send as a receive
                // error on UDP sockets; it says nothing about this datagram, so skip it —
                // as `UdpTransport::recv_from` and `port_map::exchange` already do.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                // A signal during the wait must not abort the search; retry the receive.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(PortMapError::Timeout);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Finds (once) the gateway's mapping service.
    pub fn discover(&mut self) -> Result<UpnpService, PortMapError> {
        if let Some(s) = &self.service {
            return Ok(s.clone());
        }
        let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))?;
        socket.send_to(ssdp_search_request().as_bytes(), self.ssdp)?;
        let deadline = Instant::now() + self.timeout;
        let mut buf = [0u8; 2048];
        let (location, responder) = Self::wait_for_ssdp_answer(
            |wait, buf| {
                socket.set_read_timeout(Some(wait))?;
                socket.recv_from(buf)
            },
            deadline,
            &mut buf,
        )?;
        let (addr, path) = parse_http_url(&location).ok_or(PortMapError::Malformed("description URL"))?;
        if addr.ip() != responder {
            return Err(PortMapError::Malformed("description URL names another host"));
        }
        let request = format!("GET {path} HTTP/1.1\r\nHOST: {addr}\r\nConnection: close\r\n\r\n");
        let description = http(addr, &request, self.timeout)?;
        let service =
            find_wan_service(&description, &location).ok_or(PortMapError::Unsupported("no WAN service"))?;
        if !service_is_on(&service, responder) {
            return Err(PortMapError::Malformed("control URL names another host"));
        }
        self.service = Some(service.clone());
        Ok(service)
    }

    fn call(&mut self, action: &str, args: &[(&str, String)]) -> Result<String, PortMapError> {
        let service = self.discover()?;
        let (addr, path) =
            parse_http_url(&service.control_url).ok_or(PortMapError::Malformed("control URL"))?;
        let body = soap_envelope(&service.service_type, action, args);
        let request = format!(
            "POST {path} HTTP/1.1\r\nHOST: {addr}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
             SOAPACTION: \"{}#{action}\"\r\nCONTENT-LENGTH: {}\r\nConnection: close\r\n\r\n{body}",
            service.service_type,
            body.len()
        );
        http(addr, &request, self.timeout)
    }

    /// The gateway's public address.
    pub fn external_ip(&mut self) -> Result<Ipv4Addr, PortMapError> {
        let body = self.call("GetExternalIPAddress", &[])?;
        xml_text(&body, "NewExternalIPAddress")
            .and_then(|ip| ip.parse().ok())
            .ok_or(PortMapError::Malformed("external address"))
    }
}

impl PortMapper for UpnpClient {
    fn map(&mut self, internal_port: u16, lifetime_secs: u32) -> Result<PortMapping, PortMapError> {
        if lifetime_secs == 0 {
            self.call(
                "DeletePortMapping",
                &[
                    ("NewRemoteHost", String::new()),
                    ("NewExternalPort", internal_port.to_string()),
                    ("NewProtocol", "UDP".into()),
                ],
            )?;
            return Ok(PortMapping {
                external: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, internal_port)),
                internal_port,
                lifetime_secs: 0,
                protocol: MapProtocol::Upnp,
            });
        }
        let public = self.external_ip()?;
        let lifetime_secs = match self.add_port_mapping(internal_port, lifetime_secs) {
            // IGD v1 routers may grant only permanent leases (`OnlyPermanentLeasesSupported`):
            // ask for one; the lease deletes it at shutdown.
            Err(PortMapError::Refused(UPNP_ONLY_PERMANENT_LEASES)) => {
                self.add_port_mapping(internal_port, 0)?;
                0
            }
            other => {
                other?;
                lifetime_secs
            }
        };
        Ok(PortMapping {
            external: SocketAddr::V4(SocketAddrV4::new(public, internal_port)),
            internal_port,
            lifetime_secs,
            protocol: MapProtocol::Upnp,
        })
    }

    fn name(&self) -> &'static str {
        "upnp"
    }
}

impl UpnpClient {
    fn add_port_mapping(&mut self, internal_port: u16, lease_secs: u32) -> Result<String, PortMapError> {
        self.call(
            "AddPortMapping",
            &[
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", internal_port.to_string()),
                ("NewProtocol", "UDP".into()),
                ("NewInternalPort", internal_port.to_string()),
                ("NewInternalClient", self.client_ip.to_string()),
                ("NewEnabled", "1".into()),
                ("NewPortMappingDescription", "regia_net".into()),
                ("NewLeaseDuration", lease_secs.to_string()),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit M7 (grok): nothing from an SSDP answer gets out of the request line.
    #[test]
    fn urls_with_control_bytes_are_refused() {
        assert!(parse_http_url("http://192.168.1.1/ok/path.xml").is_some());
        assert!(parse_http_url("http://192.168.1.1/\r\nX: y").is_none());
        assert!(parse_http_url("http://192.168.1.1/a b").is_none());
        assert!(parse_http_url("http://192.168.1.1/\u{e9}").is_none());
    }

    /// Audit L2 (muse2), L5 (grok): a LAN device that answers a search cannot point this host's
    /// SOAP at another machine.
    #[test]
    fn a_service_must_live_on_the_responder() {
        let gateway = IpAddr::from([192, 168, 1, 1]);
        let service = |url: &str, kind: &str| UpnpService {
            service_type: kind.to_owned(),
            control_url: url.to_owned(),
        };
        let kind = WAN_SERVICE_TYPES[0];
        assert!(service_is_on(
            &service("http://192.168.1.1:5000/ctl", kind),
            gateway
        ));
        assert!(!service_is_on(&service("http://127.0.0.1:22/x", kind), gateway));
        assert!(!service_is_on(&service("http://10.0.0.1/ctl", kind), gateway));
        assert!(!service_is_on(
            &service("http://192.168.1.1/ctl", "urn:x\"\r\nX: y"),
            gateway
        ));
    }
    use std::net::TcpListener;

    const DESCRIPTION: &str = "<?xml version=\"1.0\"?><root><device><serviceList>\
        <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
        <controlURL>/l3f</controlURL></service>\
        <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
        <controlURL>/ctl/IPConn</controlURL></service>\
        </serviceList></device></root>";

    #[test]
    fn parsing_helpers() {
        assert_eq!(
            parse_ssdp_location(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nLocation: http://192.168.1.1:5000/desc.xml\r\n"
            ),
            Some("http://192.168.1.1:5000/desc.xml".into())
        );
        assert_eq!(
            parse_http_url("http://192.168.1.1:5000/desc.xml"),
            Some((SocketAddr::from(([192, 168, 1, 1], 5000)), "/desc.xml".into()))
        );
        assert_eq!(
            parse_http_url("http://10.0.0.1"),
            Some((SocketAddr::from(([10, 0, 0, 1], 80)), "/".into()))
        );
        assert!(parse_http_url("http://router.local/x").is_none(), "no DNS names");
        assert!(parse_http_url("https://10.0.0.1/").is_none());
        let svc = find_wan_service(DESCRIPTION, "http://192.168.1.1:5000/desc.xml").expect("service");
        assert_eq!(svc.service_type, WAN_SERVICE_TYPES[1]);
        assert_eq!(svc.control_url, "http://192.168.1.1:5000/ctl/IPConn");
        assert_eq!(
            xml_text(
                "<s:Body><u:R><NewExternalIPAddress>1.2.3.4</NewExternalIPAddress></u:R>",
                "NewExternalIPAddress"
            ),
            Some("1.2.3.4")
        );
        assert!(find_wan_service("<root/>", "http://1.1.1.1/").is_none());
        let env = soap_envelope(
            WAN_SERVICE_TYPES[1],
            "AddPortMapping",
            &[("NewExternalPort", "7777".into())],
        );
        assert!(
            env.contains("<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">")
        );
        assert!(env.contains("<NewExternalPort>7777</NewExternalPort>"));
    }

    fn respond(listener: &TcpListener, body: &str) -> String {
        let Ok((mut stream, _)) = listener.accept() else {
            return String::new();
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut buf = vec![0u8; 8192];
        let mut got = Vec::new();
        // Read until the headers (and any announced body) are in.
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

    /// Audit v034 net M3: a peer that trickles its answer one byte at a time cannot hold a call
    /// past its timeout. The gateway below needs 4 s for its 400 bytes; each byte comes well within
    /// the 1 s per-read timeout, so only a deadline over the whole exchange stops it.
    #[test]
    fn a_trickled_answer_times_out_as_one_call() {
        let Ok(listener) = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            return; // no loopback in this sandbox
        };
        let addr = listener.local_addr().expect("addr");
        let gateway = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            for _ in 0..400 {
                // Pacing, not synchronization: the client must give up long before the end.
                std::thread::sleep(Duration::from_millis(10));
                if stream.write_all(b"x").is_err() {
                    return;
                }
            }
        });
        let result = http(addr, "GET / HTTP/1.1\r\n\r\n", Duration::from_secs(1));
        assert!(
            matches!(result, Err(PortMapError::Timeout)),
            "a call that outlived its timeout returned {result:?}"
        );
        let _ = gateway.join();
    }

    fn ssdp_200(location: &str) -> String {
        format!("HTTP/1.1 200 OK\r\nST: {IGD_SEARCH_TARGET}\r\nLOCATION: {location}\r\n\r\n")
    }

    /// Feeds `wait_for_ssdp_answer` `script` errors, then copies `answer` as one datagram.
    fn scripted_recv(
        mut script: Vec<std::io::Error>,
        answer: &str,
    ) -> impl FnMut(Duration, &mut [u8]) -> std::io::Result<(usize, SocketAddr)> + '_ {
        move |_, buf| match script.pop() {
            Some(e) => Err(e),
            None => {
                let bytes = answer.as_bytes();
                buf[..bytes.len()].copy_from_slice(bytes);
                Ok((bytes.len(), SocketAddr::from(([192, 168, 1, 1], 1900))))
            }
        }
    }

    /// Sprint audit G (net2-platform) M1: on Windows an ICMP port-unreachable for an earlier
    /// send surfaces as `ConnectionReset` on the next UDP receive; it says nothing about this
    /// datagram, so the search must keep receiving until the deadline.
    #[test]
    fn a_connreset_during_ssdp_does_not_end_the_search() {
        let answer = ssdp_200("http://192.168.1.1:5000/desc.xml");
        let result = UpnpClient::wait_for_ssdp_answer(
            scripted_recv(
                vec![std::io::Error::from(std::io::ErrorKind::ConnectionReset)],
                &answer,
            ),
            Instant::now() + Duration::from_secs(5),
            &mut [0u8; 2048],
        );
        assert_eq!(
            result.expect("the search continues past the reset"),
            (
                "http://192.168.1.1:5000/desc.xml".to_string(),
                IpAddr::from([192, 168, 1, 1]),
            )
        );
    }

    /// Sprint audit G (net2-platform) M1: a signal during the wait (`Interrupted`) is retried,
    /// as `read_until` already does, not fatal.
    #[test]
    fn an_interrupted_ssdp_receive_is_retried() {
        let answer = ssdp_200("http://10.0.0.2:8080/rootDesc.xml");
        let result = UpnpClient::wait_for_ssdp_answer(
            scripted_recv(
                vec![std::io::Error::from(std::io::ErrorKind::Interrupted)],
                &answer,
            ),
            Instant::now() + Duration::from_secs(5),
            &mut [0u8; 2048],
        );
        assert_eq!(
            result.expect("the search continues past the interrupt"),
            (
                "http://10.0.0.2:8080/rootDesc.xml".to_string(),
                IpAddr::from([192, 168, 1, 1]),
            )
        );
    }

    /// Errors that mean "keep waiting" must not outlive the deadline.
    #[test]
    fn ssdp_resets_until_the_deadline_time_out() {
        let result = UpnpClient::wait_for_ssdp_answer(
            |_, _| Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
            Instant::now() + Duration::from_millis(50),
            &mut [0u8; 2048],
        );
        assert!(
            matches!(result, Err(PortMapError::Timeout)),
            "resets past the deadline returned {result:?}"
        );
    }

    /// Audit v034 net L8: only an answer to the gateway search counts. A `NOTIFY` (or an answer for
    /// another search target) that arrives first does not steer the description fetch.
    #[test]
    fn discovery_waits_for_an_answer_to_its_own_search() {
        let (Ok(ssdp), Ok(http_listener)) = (
            UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
        ) else {
            return; // no loopback in this sandbox
        };
        let ssdp_addr = ssdp.local_addr().expect("addr");
        let http_port = http_listener.local_addr().expect("addr").port();
        let gateway = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let _ = ssdp.set_read_timeout(Some(Duration::from_secs(10)));
            let Ok((_, from)) = ssdp.recv_from(&mut buf) else {
                return String::new();
            };
            // Port 9 on loopback: nothing listens there, so following either of these fails.
            let notify = "NOTIFY * HTTP/1.1\r\nNT: upnp:rootdevice\r\nNTS: ssdp:alive\r\nLOCATION: http://127.0.0.1:9/notify.xml\r\n\r\n";
            let other = "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:MediaServer:1\r\nLOCATION: http://127.0.0.1:9/media.xml\r\n\r\n";
            let real = format!(
                "HTTP/1.1 200 OK\r\nST: {IGD_SEARCH_TARGET}\r\nLOCATION: http://127.0.0.1:{http_port}/desc.xml\r\n\r\n"
            );
            for reply in [notify, other, real.as_str()] {
                let _ = ssdp.send_to(reply.as_bytes(), from);
            }
            respond(&http_listener, DESCRIPTION)
        });
        let mut client =
            UpnpClient::with_ssdp(ssdp_addr, Ipv4Addr::new(192, 168, 1, 20), Duration::from_secs(10));
        let service = client.discover();
        assert!(
            service.is_ok(),
            "discovery followed the wrong answer: {service:?}"
        );
        let request = gateway.join().expect("gateway thread");
        assert!(request.starts_with("GET /desc.xml"), "{request}");
    }

    #[test]
    fn maps_through_a_loopback_gateway() {
        let (Ok(ssdp), Ok(http)) = (
            UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
        ) else {
            return; // no loopback in this sandbox
        };
        let ssdp_addr = ssdp.local_addr().expect("addr");
        let http_port = http.local_addr().expect("addr").port();
        let gateway = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let _ = ssdp.set_read_timeout(Some(Duration::from_secs(10)));
            let Ok((_, from)) = ssdp.recv_from(&mut buf) else {
                return Vec::new();
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nST: {IGD_SEARCH_TARGET}\r\nLOCATION: http://127.0.0.1:{http_port}/desc.xml\r\n\r\n"
            );
            let _ = ssdp.send_to(reply.as_bytes(), from);
            let mut requests = vec![respond(&http, DESCRIPTION)];
            requests.push(respond(
                &http,
                "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.8</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>",
            ));
            requests.push(respond(
                &http,
                "<s:Envelope><s:Body><u:AddPortMappingResponse/></s:Body></s:Envelope>",
            ));
            requests
        });
        let mut client =
            UpnpClient::with_ssdp(ssdp_addr, Ipv4Addr::new(192, 168, 1, 20), Duration::from_secs(10));
        let m = client.map(7777, 3600).expect("mapped");
        assert_eq!(m.external, SocketAddr::from(([203, 0, 113, 8], 7777)));
        assert_eq!(m.protocol, MapProtocol::Upnp);
        let requests = gateway.join().expect("gateway thread");
        assert!(requests[0].starts_with("GET /desc.xml"));
        assert!(
            requests[2]
                .contains("SOAPACTION: \"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\"")
        );
        assert!(requests[2].contains("<NewInternalClient>192.168.1.20</NewInternalClient>"));
        assert!(requests[2].contains("<NewLeaseDuration>3600</NewLeaseDuration>"));
    }

    /// Plan N229: an IGD v1 router that grants only permanent leases (error 725) gets a permanent
    /// mapping request; the mapping reports lifetime 0 (permanent), which the lease never renews
    /// and deletes at shutdown.
    #[test]
    fn a_router_with_only_permanent_leases_gets_a_permanent_mapping() {
        let (Ok(ssdp), Ok(http)) = (
            UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))),
        ) else {
            return; // no loopback in this sandbox
        };
        let ssdp_addr = ssdp.local_addr().expect("addr");
        let http_port = http.local_addr().expect("addr").port();
        let gateway = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let _ = ssdp.set_read_timeout(Some(Duration::from_secs(10)));
            let Ok((_, from)) = ssdp.recv_from(&mut buf) else {
                return Vec::new();
            };
            let _ = ssdp.send_to(
                ssdp_200(&format!("http://127.0.0.1:{http_port}/desc.xml")).as_bytes(),
                from,
            );
            let mut requests = vec![respond(&http, DESCRIPTION)];
            requests.push(respond(
                &http,
                "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.8</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>",
            ));
            // The timed lease is refused with 725.
            if let Ok((mut stream, _)) = http.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut got = vec![0u8; 8192];
                let n = stream.read(&mut got).unwrap_or(0);
                requests.push(String::from_utf8_lossy(&got[..n]).into_owned());
                let body = "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>725</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
                let reply = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
            requests.push(respond(&http, "<s:Envelope><s:Body/></s:Envelope>"));
            requests
        });
        let mut client =
            UpnpClient::with_ssdp(ssdp_addr, Ipv4Addr::new(192, 168, 1, 20), Duration::from_secs(10));
        let m = client.map(7777, 3600).expect("a permanent mapping");
        assert_eq!(m.lifetime_secs, 0, "permanent");
        let requests = gateway.join().expect("gateway thread");
        assert!(
            requests[3].contains("<NewLeaseDuration>0</NewLeaseDuration>"),
            "{requests:?}"
        );
    }
}
