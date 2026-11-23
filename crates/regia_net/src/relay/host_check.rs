// SPDX-License-Identifier: MPL-2.0
//! The relay's half of the host check (relay protocol 3, spec 120 § 10.7, ADR-8370): the
//! port-forward probe and the uplink measurement echo. The host's half is `crate::host_check`.
//!
//! **Probe.** A registered host asks the relay to send one datagram to its mapped address. The
//! probe leaves from the relay's probe port, a port the host never sent to, so it reaches the game
//! socket only through the mapping (a NAT whose filter is address-and-port dependent, RFC 4787
//! § 5, drops it otherwise). A relay has one IP, so behind a NAT that filters by address alone the
//! probe also passes through the NAT's own state toward the relay: the check then says what a new
//! client from another port of the relay's address would see, which is the case RFC 5780 § 4.4
//! needs a second address for. The target must be on the host's own registered IP, the request is
//! padded to more than the acknowledgement and the probe together, and only a host whose
//! registration proved its address (the cookie round trip) is answered: the relay cannot be aimed
//! at a third party.
//!
//! **Measurement.** A registered host sends [`RELAY_MEASURE_BYTES`] packets as fast as its startup
//! allows; the relay acknowledges each with a 12-byte answer. Measurement packets skip the
//! per-source request limit (an uplink runs far above 32 requests a second) but share a byte
//! budget per host and window ([`RELAY_MEASURE_BYTES_PER_WINDOW`]); past it they go unanswered.

use std::net::SocketAddr;

use super::{
    RELAY_HOST_CHECK_MIN_VERSION, RELAY_MAX_QUEUED_PROBES, RELAY_MEASURE_BYTES,
    RELAY_MEASURE_BYTES_PER_WINDOW, RELAY_MEASURE_WINDOW_MS, RELAY_PROBE_NONCE_BYTES,
    RELAY_PROBE_REQUEST_BYTES, RelayCore, RelayError, RelayTag, control_v,
};
use crate::handshake::decode_addr;
use crate::wire::WireReader;

impl RelayCore {
    /// Lets this core queue port-forward probes ([`RelayCore::take_probes`]); a relay process
    /// with a probe port turns it on. Without it a probe request is acknowledged as not sent.
    pub fn set_probe_capable(&mut self, on: bool) {
        self.probe_capable = on;
        if !on {
            self.probes.clear();
        }
    }

    /// The probes to send from the probe port since the last call.
    pub fn take_probes(&mut self) -> Vec<(Vec<u8>, SocketAddr)> {
        std::mem::take(&mut self.probes)
    }

    /// A `ProbeRequest` from `from`, `len` bytes long (it is always versioned framing).
    pub(super) fn probe(
        &mut self,
        from: SocketAddr,
        r: &mut WireReader<'_>,
        len: usize,
        version: u8,
        now_ms: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        if len < RELAY_PROBE_REQUEST_BYTES {
            return Vec::new();
        }
        let Ok(nonce) = r.array::<RELAY_PROBE_NONCE_BYTES>() else {
            return Vec::new();
        };
        let Ok(target) = decode_addr(r) else {
            return Vec::new();
        };
        let error = |code: RelayError| {
            let mut w = control_v(RelayTag::Error, version);
            w.u8(code as u8);
            vec![(w.finish(), from)]
        };
        if self.lobby_of(from, now_ms).is_none() {
            return error(RelayError::NotRegistered);
        }
        if target.ip() != from.ip() || target.port() == 0 {
            return error(RelayError::ProbeTarget);
        }
        let sent = self.probe_capable && self.probes.len() < RELAY_MAX_QUEUED_PROBES;
        if sent {
            let mut hello = control_v(RelayTag::ProbeHello, version.max(RELAY_HOST_CHECK_MIN_VERSION));
            hello.raw(&nonce);
            self.probes.push((hello.finish(), target));
            self.probes_sent += 1;
        }
        let mut ack = control_v(RelayTag::ProbeSent, version);
        ack.raw(&nonce);
        ack.u8(u8::from(sent));
        vec![(ack.finish(), from)]
    }

    /// A `Measure` packet: acknowledged when `from` is a registered host within its budget.
    pub(super) fn measure(
        &mut self,
        from: SocketAddr,
        datagram: &[u8],
        body: usize,
        now_ms: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        if datagram.len() < RELAY_MEASURE_BYTES || self.lobby_of(from, now_ms).is_none() {
            self.measure_dropped += 1;
            return Vec::new();
        }
        let mut r = WireReader::new(&datagram[body..]);
        let Ok(seq) = r.u32() else {
            self.measure_dropped += 1;
            return Vec::new();
        };
        let entry = self.measure.entry(from).or_insert((now_ms, 0));
        if now_ms.saturating_sub(entry.0) >= RELAY_MEASURE_WINDOW_MS {
            *entry = (now_ms, 0);
        }
        if entry.1 >= RELAY_MEASURE_BYTES_PER_WINDOW {
            self.measure_dropped += 1;
            return Vec::new();
        }
        entry.1 = entry.1.saturating_add(datagram.len() as u64);
        let capped = entry.1 >= RELAY_MEASURE_BYTES_PER_WINDOW;
        self.measure_acked += 1;
        let mut ack = control_v(RelayTag::MeasureAck, RELAY_HOST_CHECK_MIN_VERSION);
        ack.u32(seq);
        ack.u8(u8::from(capped));
        vec![(ack.finish(), from)]
    }
}

#[cfg(test)]
mod tests {
    use super::super::{RelayReply, encode_measure, encode_probe_request, encode_register_with_cookie};
    use super::*;

    const HOST: [u8; 4] = [198, 51, 100, 7];

    /// A core with `host` registered (the cookie round trip done).
    fn registered(host: SocketAddr) -> RelayCore {
        let mut core = RelayCore::with_secret(1, [3; 32]);
        let first = super::super::encode_register(600, b"t").expect("register");
        let reply = core.handle(host, &first, 0);
        let Ok(Some(RelayReply::RegisterChallenge(cookie))) = RelayReply::decode(&reply[0].0) else {
            panic!("challenge expected: {reply:?}");
        };
        let again = encode_register_with_cookie(600, b"t", &cookie).expect("register");
        let reply = core.handle(host, &again, 1);
        assert!(matches!(
            RelayReply::decode(&reply[0].0),
            Ok(Some(RelayReply::JoinCode(_)))
        ));
        core
    }

    #[test]
    fn a_probe_goes_only_to_the_hosts_own_address_and_never_outweighs_its_request() {
        let host = SocketAddr::from((HOST, 40_000));
        let mut core = registered(host);
        core.set_probe_capable(true);
        let mapped = SocketAddr::from((HOST, 7777));
        let request = encode_probe_request(&[5; 8], mapped);
        let reply = core.handle(host, &request, 2);
        let probes = core.take_probes();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].1, mapped);
        assert_eq!(
            RelayReply::decode(&probes[0].0).expect("decodes"),
            Some(RelayReply::ProbeHello { nonce: [5; 8] })
        );
        assert_eq!(
            RelayReply::decode(&reply[0].0).expect("decodes"),
            Some(RelayReply::ProbeSent {
                nonce: [5; 8],
                sent: true
            })
        );
        assert!(
            reply[0].0.len() + probes[0].0.len() <= request.len(),
            "the relay never sends more than it was sent"
        );
        // Another IP is refused, and nothing is queued.
        let elsewhere = encode_probe_request(&[6; 8], SocketAddr::from(([203, 0, 113, 9], 7777)));
        let reply = core.handle(host, &elsewhere, 3);
        assert_eq!(
            RelayReply::decode(&reply[0].0).expect("decodes"),
            Some(RelayReply::Error(RelayError::ProbeTarget))
        );
        // An unregistered source gets a refusal, not a probe.
        let stranger = SocketAddr::from(([203, 0, 113, 10], 1000));
        let reply = core.handle(stranger, &encode_probe_request(&[7; 8], stranger), 4);
        assert_eq!(
            RelayReply::decode(&reply[0].0).expect("decodes"),
            Some(RelayReply::Error(RelayError::NotRegistered))
        );
        assert!(core.take_probes().is_empty());
        // Without a probe port the relay says so.
        core.set_probe_capable(false);
        let reply = core.handle(host, &request, 5);
        assert_eq!(
            RelayReply::decode(&reply[0].0).expect("decodes"),
            Some(RelayReply::ProbeSent {
                nonce: [5; 8],
                sent: false
            })
        );
        // A short request is dropped.
        assert!(core.handle(host, &request[..40], 6).is_empty());
    }

    #[test]
    fn measurement_skips_the_request_limit_but_not_the_byte_budget() {
        let host = SocketAddr::from((HOST, 40_000));
        let mut core = registered(host);
        let per_window = (RELAY_MEASURE_BYTES_PER_WINDOW / RELAY_MEASURE_BYTES as u64) as u32;
        let mut acked = 0u32;
        let mut capped_seen = false;
        // All in one millisecond: far above the 32 requests a second of control traffic.
        for seq in 0..per_window + 50 {
            let packet = encode_measure(seq);
            assert_eq!(packet.len(), RELAY_MEASURE_BYTES);
            let reply = core.handle(host, &packet, 10);
            if let Some((bytes, to)) = reply.first() {
                assert_eq!(*to, host);
                assert!(bytes.len() < 16, "a 12-byte answer to a 1,200-byte packet");
                let Ok(Some(RelayReply::MeasureAck { seq: s, capped })) = RelayReply::decode(bytes) else {
                    panic!("measure ack expected");
                };
                assert_eq!(s, seq);
                capped_seen |= capped;
                acked += 1;
            }
        }
        assert!(acked >= per_window - 1 && acked <= per_window + 1, "{acked}");
        assert!(capped_seen);
        assert_eq!(core.measure_acked, u64::from(acked));
        // The next window has a fresh budget.
        assert_eq!(
            core.handle(host, &encode_measure(1), 10 + RELAY_MEASURE_WINDOW_MS)
                .len(),
            1
        );
        // A source that is not a registered host is never answered.
        let stranger = SocketAddr::from(([203, 0, 113, 10], 1000));
        assert!(core.handle(stranger, &encode_measure(1), 20).is_empty());
    }

    /// A protocol 2 framed host-check request is told the protocol it needs.
    #[test]
    fn an_old_framing_is_told_the_version_it_needs() {
        let host = SocketAddr::from((HOST, 40_000));
        let mut core = registered(host);
        let mut request = encode_probe_request(&[5; 8], SocketAddr::from((HOST, 7777)));
        request[5] = 2;
        let reply = core.handle(host, &request, 2);
        assert_eq!(
            RelayReply::decode(&reply[0].0).expect("decodes"),
            Some(RelayReply::VersionTooOld {
                min: RELAY_HOST_CHECK_MIN_VERSION
            })
        );
    }
}
