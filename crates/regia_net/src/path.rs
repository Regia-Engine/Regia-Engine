// SPDX-License-Identifier: MPL-2.0
//! Path validation for address changes (ADR-8299; RFC 9000 § 8.1 anti-amplification, § 8.2 path
//! validation, § 9.3 migration), without a wire change.
//!
//! A record that authenticates from an address the link is not using makes that address a
//! *candidate*; it never moves the link by itself. The endpoint keeps sending to the validated
//! address and sends one acknowledgment-only packet (a *probe*,
//! [`crate::connection::Connection::path_probe`]) to the candidate alone. When a later record
//! from the candidate acknowledges the probe's sequence number
//! ([`crate::connection::packet_acknowledges`]), the far end has shown that it receives traffic
//! there, and the link moves. A copy of a fresh record raced from a third address (an on-path
//! observer, soak finding F1) therefore never moves the link: the probe goes to the third address,
//! and whoever is there cannot make the real far end acknowledge it from there.
//!
//! Until a candidate validates, the endpoint sends it at most [`ANTI_AMPLIFICATION_FACTOR`] times
//! the bytes it received from it (§ 8.1), so a forged source cannot turn the endpoint into an
//! amplifier. Trial decryptions from new addresses are budgeted per candidate address
//! (`CandidateLimiter`), so forgeries from elsewhere cannot spend the budget of the address the
//! far end really moved to (adversarial row 12).
//!
//! A NAT rebinding costs the far end at most about a round trip (ADR-8422):
//! - Every probe sent to a candidate stays answerable ([`MAX_OUTSTANDING_PROBES`]): a resend no
//!   longer forgets the earlier one, so a path whose round trip is longer than
//!   [`PATH_PROBE_RESEND_MS`] still validates on the first answer (RFC 9000 § 8.2.2: a response
//!   to any challenge sent on the path validates it).
//! - Once an address has authenticated a record it is a candidate, and the endpoint opens its
//!   records like those of the validated address: the per-address trial budget is for addresses
//!   that never authenticated, and must not refuse the genuine records that carry the probe's
//!   answer.
//! - While the validated path is silent and a candidate is the newest source of the far end's
//!   records (a NAT rebinding, not a copy raced beside live traffic), the endpoint also sends a
//!   copy of each data packet to that candidate, within the same three-times limit
//!   (`PathState::mirror_target`; RFC 9000 § 9.3: an endpoint may send to an unvalidated address
//!   within the § 8.1 limit). The link still moves only on the probe's answer, so a racer never
//!   moves it; what a racer can get is the bounded copy of traffic it already observes.

use std::net::SocketAddr;

/// Bytes an endpoint may send to an unvalidated address per byte it received from there (RFC 9000
/// § 8.1).
pub const ANTI_AMPLIFICATION_FACTOR: u64 = 3;
/// Addresses one link tracks as candidates at once. A new one replaces the oldest.
pub const MAX_PATH_CANDIDATES: usize = 4;
/// A probe unanswered this long may be sent again (a new sequence number), in milliseconds. A lost
/// probe costs at most this much extra time on a real move.
pub const PATH_PROBE_RESEND_MS: u64 = 100;
/// A candidate that sent nothing authentic for this long is forgotten, in milliseconds.
pub const PATH_CANDIDATE_IDLE_MS: u64 = 5_000;
/// How far behind the newest record a genuine duplicate from a new address may be and still make
/// that address a candidate (records; about two seconds of a 30 Hz client). A copy raced from
/// elsewhere delivers the record first, so the far end's own copy, from where its NAT really moved
/// it, arrives as a duplicate a moment later; older duplicates are plain replays.
pub const DUPLICATE_CANDIDATE_RECORDS: u64 = 64;
/// Probes to one candidate that stay answerable at once (ADR-8422): an acknowledgment of any of
/// them validates the path. The oldest is forgotten when another is sent.
pub const MAX_OUTSTANDING_PROBES: usize = 8;

/// Packets a plain link holds from one unvalidated candidate address (KI-NT-018, ADR-10077). A
/// plain packet carries no tag, so nothing shows it is the far end's until a packet from its
/// address answers the probe: until then it is held, not applied, and it is applied in arrival
/// order when the candidate validates. Past this many (or [`MAX_HELD_PLAIN_BYTES`]) a packet is
/// dropped; a reliable message in it is sent again by the far end. About 0.5 s of a 30 Hz client,
/// several round trips of an honest NAT move.
pub const MAX_HELD_PLAIN_PACKETS: usize = 16;
/// Bytes a plain link holds from one unvalidated candidate address (KI-NT-018, ADR-10077):
/// sixteen datagrams of a 1,200-byte path, rounded up.
pub const MAX_HELD_PLAIN_BYTES: usize = 24 * 1024;

/// One address a link might move to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathCandidate {
    /// The new address.
    pub addr: SocketAddr,
    /// The sequence numbers of the probes sent to `addr` that an acknowledgment from there still
    /// validates, oldest first, at most [`MAX_OUTSTANDING_PROBES`] (ADR-8422).
    pub probes: Vec<u64>,
    /// When the newest probe was sent.
    pub probe_sent_ms: Option<u64>,
    /// Bytes of authentic records received from `addr`.
    pub bytes_in: u64,
    /// Bytes sent to `addr` (probes and mirrored packets).
    pub bytes_out: u64,
    /// When the last authentic record from `addr` arrived.
    pub last_ms: u64,
    /// A plain link's packets from `addr`, held until it validates (KI-NT-018, ADR-10077), in
    /// arrival order; at most [`MAX_HELD_PLAIN_PACKETS`] and [`MAX_HELD_PLAIN_BYTES`].
    pub held: Vec<Vec<u8>>,
    /// Bytes in `held`.
    pub held_bytes: usize,
}

impl PathCandidate {
    /// True when `len` more bytes to the candidate stay within [`ANTI_AMPLIFICATION_FACTOR`]
    /// times what it sent (RFC 9000 § 8.1).
    pub fn within_budget(&self, len: usize) -> bool {
        self.bytes_out.saturating_add(len as u64) <= self.bytes_in.saturating_mul(ANTI_AMPLIFICATION_FACTOR)
    }

    /// True when a probe of `len` bytes may go to the candidate at `now_ms`: none sent yet (or
    /// the newest is older than [`PATH_PROBE_RESEND_MS`]), and the bytes stay within
    /// [`ANTI_AMPLIFICATION_FACTOR`] times what the candidate sent.
    pub fn may_probe(&self, len: usize, now_ms: u64) -> bool {
        let due = self
            .probe_sent_ms
            .is_none_or(|sent| now_ms.saturating_sub(sent) >= PATH_PROBE_RESEND_MS);
        due && self.within_budget(len)
    }
}

/// The candidates of one link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PathState {
    candidates: Vec<PathCandidate>,
    /// When the last authentic record arrived on the validated path (ADR-8422); `None` before
    /// the first.
    primary_ms: Option<u64>,
    /// Probes sent.
    pub probes: u64,
    /// Data packets also sent to a candidate while the validated path was silent (ADR-8422).
    pub mirrored: u64,
    /// Bytes sent to addresses that never validated (probes and mirrored packets), summed.
    pub unvalidated_bytes_out: u64,
}

impl PathState {
    /// Records `bytes` of an authentic record from `addr` at `now_ms` and returns its candidate
    /// (created, replacing the oldest when [`MAX_PATH_CANDIDATES`] are tracked). Forgets idle
    /// candidates first.
    pub fn note(&mut self, addr: SocketAddr, bytes: usize, now_ms: u64) -> &mut PathCandidate {
        self.candidates
            .retain(|c| c.addr == addr || now_ms.saturating_sub(c.last_ms) < PATH_CANDIDATE_IDLE_MS);
        let index = match self.candidates.iter().position(|c| c.addr == addr) {
            Some(i) => i,
            None => {
                if self.candidates.len() >= MAX_PATH_CANDIDATES
                    && let Some(oldest) = self
                        .candidates
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, c)| c.last_ms)
                        .map(|(i, _)| i)
                {
                    self.candidates.swap_remove(oldest);
                }
                self.candidates.push(PathCandidate {
                    addr,
                    probes: Vec::new(),
                    probe_sent_ms: None,
                    bytes_in: 0,
                    bytes_out: 0,
                    last_ms: now_ms,
                    held: Vec::new(),
                    held_bytes: 0,
                });
                self.candidates.len() - 1
            }
        };
        let c = &mut self.candidates[index];
        c.bytes_in = c.bytes_in.saturating_add(bytes as u64);
        c.last_ms = now_ms;
        c
    }

    /// True when the packet `body` from `addr` acknowledges any probe sent to `addr` that is
    /// still answerable (RFC 9000 § 8.2.2; ADR-8422).
    pub fn validates(&self, addr: SocketAddr, body: &[u8]) -> bool {
        self.candidates.iter().any(|c| {
            c.addr == addr
                && c.probes
                    .iter()
                    .any(|&seq| crate::connection::packet_acknowledges(body, seq))
        })
    }

    /// Holds a plain link's `packet` from candidate `addr` until the candidate validates
    /// (KI-NT-018, ADR-10077). `false` when the candidate is not tracked or already holds
    /// [`MAX_HELD_PLAIN_PACKETS`] packets or [`MAX_HELD_PLAIN_BYTES`] bytes: the packet is dropped,
    /// never applied.
    pub fn hold(&mut self, addr: SocketAddr, packet: &[u8]) -> bool {
        let Some(c) = self.candidates.iter_mut().find(|c| c.addr == addr) else {
            return false;
        };
        if c.held.len() >= MAX_HELD_PLAIN_PACKETS || c.held_bytes + packet.len() > MAX_HELD_PLAIN_BYTES {
            return false;
        }
        c.held_bytes += packet.len();
        c.held.push(packet.to_vec());
        true
    }

    /// Takes the plain packets held from candidate `addr`, in arrival order (KI-NT-018,
    /// ADR-10077): called when it validates, before [`Self::moved`] forgets it.
    pub fn take_held(&mut self, addr: SocketAddr) -> Vec<Vec<u8>> {
        self.candidates
            .iter_mut()
            .find(|c| c.addr == addr)
            .map(|c| {
                c.held_bytes = 0;
                std::mem::take(&mut c.held)
            })
            .unwrap_or_default()
    }

    /// Records a probe of `len` bytes with sequence number `seq` sent to `addr` at `now_ms`. The
    /// earlier probes stay answerable, up to [`MAX_OUTSTANDING_PROBES`].
    pub fn probed(&mut self, addr: SocketAddr, seq: u64, len: usize, now_ms: u64) {
        if let Some(c) = self.candidates.iter_mut().find(|c| c.addr == addr) {
            if c.probes.len() >= MAX_OUTSTANDING_PROBES {
                c.probes.remove(0);
            }
            c.probes.push(seq);
            c.probe_sent_ms = Some(now_ms);
            c.bytes_out = c.bytes_out.saturating_add(len as u64);
        }
        self.probes += 1;
        self.unvalidated_bytes_out = self.unvalidated_bytes_out.saturating_add(len as u64);
    }

    /// An authentic record arrived on the validated path at `now_ms` (ADR-8422).
    pub fn heard_primary(&mut self, now_ms: u64) {
        self.primary_ms = Some(self.primary_ms.map_or(now_ms, |t| t.max(now_ms)));
    }

    /// True when `addr` sent an authentic record recently enough to still be a candidate: its
    /// records are opened without the per-address trial budget (ADR-8422).
    pub fn is_candidate(&self, addr: SocketAddr, now_ms: u64) -> bool {
        self.candidates
            .iter()
            .any(|c| c.addr == addr && now_ms.saturating_sub(c.last_ms) < PATH_CANDIDATE_IDLE_MS)
    }

    /// Where a copy of a data packet of `len` bytes should also go now (ADR-8422): the candidate
    /// that sent the far end's most recent authentic record, when that record is newer than the
    /// last one on the validated path (the validated path went silent: a NAT rebinding, not a
    /// copy raced beside live traffic) and the copy stays within [`ANTI_AMPLIFICATION_FACTOR`]
    /// times what the candidate sent. `None` otherwise.
    pub fn mirror_target(&self, len: usize) -> Option<SocketAddr> {
        let c = self.candidates.iter().max_by_key(|c| c.last_ms)?;
        let newer = self.primary_ms.is_none_or(|p| c.last_ms > p);
        (newer && c.within_budget(len)).then_some(c.addr)
    }

    /// Records a copy of `len` bytes sent to candidate `addr` (ADR-8422).
    pub fn mirrored(&mut self, addr: SocketAddr, len: usize) {
        if let Some(c) = self.candidates.iter_mut().find(|c| c.addr == addr) {
            c.bytes_out = c.bytes_out.saturating_add(len as u64);
        }
        self.mirrored += 1;
        self.unvalidated_bytes_out = self.unvalidated_bytes_out.saturating_add(len as u64);
    }

    /// The candidate for `addr`, if tracked.
    pub fn get(&self, addr: SocketAddr) -> Option<&PathCandidate> {
        self.candidates.iter().find(|c| c.addr == addr)
    }

    /// The link moved to a validated address at `now_ms`: forgets every candidate, and the new
    /// path counts as heard from (the record that validated it arrived there).
    pub fn moved(&mut self, now_ms: u64) {
        self.candidates.clear();
        self.primary_ms = Some(now_ms);
    }

    /// The address of the newest candidate, for status rows.
    pub fn newest(&self) -> Option<SocketAddr> {
        self.candidates.iter().max_by_key(|c| c.last_ms).map(|c| c.addr)
    }
}

/// Trial decryptions per candidate address per second (adversarial row 12): a fixed table of
/// per-second counters indexed by a keyed hash of the link token and the address, so a forged
/// source can neither grow the table nor choose the counter of the address the far end really
/// moved to.
#[derive(Clone)]
pub(crate) struct CandidateLimiter {
    per_second: u32,
    key: [u8; 32],
    /// `(second, count)` per bucket.
    table: Vec<(u64, u32)>,
}

impl std::fmt::Debug for CandidateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandidateLimiter")
            .field("per_second", &self.per_second)
            .finish_non_exhaustive()
    }
}

/// Buckets of a [`CandidateLimiter`].
pub(crate) const CANDIDATE_LIMITER_SLOTS: usize = 4_096;

impl CandidateLimiter {
    /// A limiter admitting `per_second` trial opens per (token, address) per second, under the
    /// secret `key`. The table is allocated on the first request.
    pub fn new(per_second: u32, key: [u8; 32]) -> Self {
        Self {
            per_second,
            key,
            table: Vec::new(),
        }
    }

    /// Admits one trial open for link `token` from `addr` at `now_ms`, and counts it.
    pub fn admit(&mut self, token: u32, addr: SocketAddr, now_ms: u64) -> bool {
        if self.table.is_empty() {
            self.table.resize(CANDIDATE_LIMITER_SLOTS, (u64::MAX, 0));
        }
        let mut bytes = [0u8; 4 + 16 + 2];
        bytes[..4].copy_from_slice(&token.to_le_bytes());
        let ip = match addr.ip() {
            std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
            std::net::IpAddr::V6(v6) => v6.octets(),
        };
        bytes[4..20].copy_from_slice(&ip);
        bytes[20..].copy_from_slice(&addr.port().to_le_bytes());
        let hash = blake3::keyed_hash(&self.key, &bytes);
        let mut word = [0u8; 8];
        word.copy_from_slice(&hash.as_bytes()[..8]);
        let index = (u64::from_le_bytes(word) % self.table.len() as u64) as usize;
        let Some(slot) = self.table.get_mut(index) else {
            return false;
        };
        let second = now_ms / 1000;
        if slot.0 != second {
            *slot = (second, 0);
        }
        if slot.1 >= self.per_second {
            return false;
        }
        slot.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, 7], port))
    }

    #[test]
    fn probes_stay_within_three_times_the_bytes_received() {
        let mut s = PathState::default();
        let c = s.note(addr(1), 20, 0);
        assert!(c.may_probe(60, 0), "3 x 20 bytes");
        assert!(!c.may_probe(61, 0), "one byte over 3x");
        s.probed(addr(1), 7, 60, 0);
        let c = s.note(addr(1), 0, 500);
        assert!(!c.may_probe(1, 500), "the budget is spent until more arrives");
        let c = s.note(addr(1), 10, 600);
        assert!(c.may_probe(30, 600));
        assert_eq!(s.unvalidated_bytes_out, 60);
    }

    #[test]
    fn a_probe_waits_for_its_resend_interval() {
        let mut s = PathState::default();
        s.note(addr(1), 1_000, 0);
        s.probed(addr(1), 1, 30, 0);
        assert!(
            !s.note(addr(1), 10, PATH_PROBE_RESEND_MS - 1)
                .may_probe(30, PATH_PROBE_RESEND_MS - 1)
        );
        assert!(
            s.note(addr(1), 10, PATH_PROBE_RESEND_MS)
                .may_probe(30, PATH_PROBE_RESEND_MS)
        );
    }

    #[test]
    fn candidates_are_bounded_and_idle_ones_expire() {
        let mut s = PathState::default();
        for i in 0..(MAX_PATH_CANDIDATES as u16 + 3) {
            s.note(addr(i), 10, u64::from(i));
        }
        assert!(s.get(addr(0)).is_none(), "the oldest was replaced");
        assert!(s.get(addr(MAX_PATH_CANDIDATES as u16 + 2)).is_some());
        s.note(addr(99), 10, 100 + PATH_CANDIDATE_IDLE_MS);
        assert_eq!(s.newest(), Some(addr(99)));
        assert!(s.get(addr(4)).is_none(), "idle candidates are forgotten");
    }

    /// A transport v2 packet body (ADR-8184): packet number 77 in one byte, then an acknowledgment
    /// frame of `largest` and `ranges` (newest first) when `ranges` is not empty.
    fn packet(largest: u64, ranges: &[(u64, u64)]) -> Vec<u8> {
        let mut w = crate::wire::WireWriter::new();
        // Flags: one-byte packet number; bit 2 when an acknowledgment frame follows.
        w.u8(if ranges.is_empty() { 0 } else { 0x04 });
        w.u8(77);
        if !ranges.is_empty() {
            crate::ack::AckFrame {
                largest,
                delay_us: 0,
                arrival_ms: 0,
                ranges: ranges.to_vec(),
                timestamps: Vec::new(),
            }
            .encode(&mut w, false, usize::MAX);
        }
        w.finish()
    }

    /// The wire-4 version of this test checked the newest acknowledged number and the 32-bit field
    /// behind it; on transport v2 (ADR-8184) the same cases are the frame's ranges.
    #[test]
    fn a_packet_acknowledges_the_numbers_its_ranges_name() {
        use crate::connection::packet_acknowledges;
        assert!(packet_acknowledges(&packet(100, &[(100, 100)]), 100));
        assert!(!packet_acknowledges(&packet(100, &[(100, 100)]), 99));
        assert!(
            packet_acknowledges(&packet(100, &[(99, 100)]), 99),
            "the first range reaches back"
        );
        assert!(
            packet_acknowledges(&packet(100, &[(100, 100), (60, 68)]), 68),
            "an older range"
        );
        assert!(
            !packet_acknowledges(&packet(100, &[(100, 100), (60, 68)]), 80),
            "the gap between ranges"
        );
        assert!(
            !packet_acknowledges(&packet(100, &[(100, 100), (60, 68)]), 59),
            "below the oldest range"
        );
        assert!(
            !packet_acknowledges(&packet(100, &[(60, 100)]), 101),
            "ahead of the largest"
        );
        assert!(
            packet_acknowledges(&packet(70_000, &[(70_000, 70_000)]), 70_000),
            "62-bit numbers"
        );
        assert!(
            !packet_acknowledges(&packet(70_000, &[(70_000, 70_000)]), 70_000 - 65_536),
            "no 16-bit wrap"
        );
        assert!(!packet_acknowledges(&packet(100, &[]), 100), "no ack frame");
        let full = packet(100, &[(100, 100)]);
        assert!(!packet_acknowledges(&full[..3], 100), "truncated");
    }

    #[test]
    fn a_probe_is_acknowledged_by_the_far_end_and_never_counted_lost() {
        use crate::connection::{Connection, LinkConfig, packet_acknowledges};
        let mut near = Connection::new(LinkConfig::default());
        let mut far = Connection::new(LinkConfig::default());
        let (seq, probe) = near.path_probe(0, &[]);
        far.receive(&probe, 1).expect("probe decodes");
        let reply = far.poll(2, &[]).pop().expect("an ack is due");
        assert!(packet_acknowledges(&reply, seq));
        // Unanswered probes stay out of the loss count however long they wait.
        for i in 0..40u64 {
            let _ = near.path_probe(10 * i, &[]);
        }
        let _ = near.poll(60_000, &[]);
        assert_eq!(near.stats().packets_lost, 0);
    }

    /// ADR-8422: on a path whose round trip is longer than the resend interval, the answer to the
    /// first probe arrives after the second was sent. It still validates (base: only the newest
    /// probe counted, so the move waited for an answer to a probe that was always one behind).
    #[test]
    fn an_answer_to_an_earlier_probe_still_validates() {
        let mut s = PathState::default();
        s.note(addr(1), 1_000, 0);
        s.probed(addr(1), 40, 30, 0);
        s.note(addr(1), 100, PATH_PROBE_RESEND_MS);
        s.probed(addr(1), 52, 30, PATH_PROBE_RESEND_MS);
        assert!(
            s.validates(addr(1), &packet(60, &[(55, 60), (40, 41)])),
            "the first probe"
        );
        assert!(s.validates(addr(1), &packet(60, &[(52, 60)])), "the second probe");
        assert!(!s.validates(addr(1), &packet(51, &[(41, 51)])), "neither");
        assert!(!s.validates(addr(2), &packet(60, &[(40, 60)])), "another address");
        // Only the newest `MAX_OUTSTANDING_PROBES` stay answerable.
        for i in 0..MAX_OUTSTANDING_PROBES as u64 {
            s.probed(addr(1), 100 + i, 30, 1_000 + i * PATH_PROBE_RESEND_MS);
        }
        assert!(!s.validates(addr(1), &packet(52, &[(40, 52)])), "forgotten");
        assert!(s.validates(addr(1), &packet(100, &[(100, 100)])));
    }

    /// ADR-8422: a copy goes to the candidate only while it is the far end's newest source (the
    /// validated path went silent) and within three times what it sent.
    #[test]
    fn a_candidate_is_mirrored_only_while_the_validated_path_is_silent() {
        let mut s = PathState::default();
        s.heard_primary(1_000);
        assert_eq!(s.mirror_target(100), None, "no candidate");
        // A copy raced beside live traffic: the genuine record arrives on the validated path in
        // the same instant. No mirror.
        s.note(addr(1), 100, 1_010);
        s.heard_primary(1_010);
        assert_eq!(s.mirror_target(100), None, "the validated path is live");
        // The validated path goes silent (a NAT rebinding): mirror, within 3 x 100 bytes.
        s.note(addr(1), 0, 1_030);
        assert_eq!(s.mirror_target(300), Some(addr(1)));
        assert_eq!(s.mirror_target(301), None, "one byte over 3x");
        s.mirrored(addr(1), 250);
        assert_eq!(s.mirror_target(51), None, "the budget is spent");
        assert_eq!(s.mirror_target(50), Some(addr(1)));
        assert_eq!((s.mirrored, s.unvalidated_bytes_out), (1, 250));
        // A record on the validated path again: the mirror stops.
        s.heard_primary(1_040);
        assert_eq!(s.mirror_target(10), None);
        // Probes and copies share the budget.
        s.note(addr(1), 10, 1_050);
        s.probed(addr(1), 9, 60, 1_050);
        assert_eq!(s.get(addr(1)).map(|c| c.bytes_out), Some(310));
        assert_eq!(s.mirror_target(20), Some(addr(1)));
        assert_eq!(s.mirror_target(21), None);
        // A move forgets the candidates and counts the new path as heard.
        s.moved(1_060);
        assert_eq!(s.mirror_target(1), None);
        assert!(s.get(addr(1)).is_none());
    }

    /// ADR-8422: an address that authenticated a record stays a candidate (and past the trial
    /// budget) until it has been idle for `PATH_CANDIDATE_IDLE_MS`.
    #[test]
    fn a_candidate_is_known_until_it_goes_idle() {
        let mut s = PathState::default();
        assert!(!s.is_candidate(addr(1), 0), "never authenticated");
        s.note(addr(1), 10, 100);
        assert!(s.is_candidate(addr(1), 100 + PATH_CANDIDATE_IDLE_MS - 1));
        assert!(!s.is_candidate(addr(1), 100 + PATH_CANDIDATE_IDLE_MS));
        assert!(!s.is_candidate(addr(2), 100));
    }

    #[test]
    fn a_plain_candidate_holds_a_bounded_queue_until_it_is_taken() {
        let mut state = PathState::default();
        assert!(
            !state.hold(addr(1), b"untracked"),
            "only a tracked candidate holds"
        );
        state.note(addr(1), 100, 0);
        for i in 0..MAX_HELD_PLAIN_PACKETS {
            assert!(state.hold(addr(1), &[i as u8; 10]), "packet {i}");
        }
        assert!(!state.hold(addr(1), &[0xff; 10]), "the seventeenth is dropped");
        let held = state.take_held(addr(1));
        assert_eq!(held.len(), MAX_HELD_PLAIN_PACKETS);
        assert!(
            held.iter().enumerate().all(|(i, p)| p == &vec![i as u8; 10]),
            "arrival order"
        );
        assert!(state.take_held(addr(1)).is_empty());
        state.note(addr(2), 100, 0);
        assert!(state.hold(addr(2), &vec![0; MAX_HELD_PLAIN_BYTES]));
        assert!(!state.hold(addr(2), &[0]), "the byte bound");
        state.moved(10);
        assert!(
            state.take_held(addr(2)).is_empty(),
            "a move forgets every candidate's packets"
        );
    }

    #[test]
    fn the_limiter_counts_each_address_on_its_own() {
        let mut l = CandidateLimiter::new(8, [3; 32]);
        for _ in 0..8 {
            assert!(l.admit(5, addr(1), 10));
        }
        assert!(!l.admit(5, addr(1), 10), "the attacker's address is spent");
        assert!(l.admit(5, addr(2), 10), "another address keeps its own budget");
        assert!(l.admit(5, addr(1), 1_000), "a new second refills");
    }
}
