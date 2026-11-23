// SPDX-License-Identifier: MPL-2.0
//! Pre-proof answer budget shared fairly between source prefixes (spec 120 § 11.4, N218,
//! ADR-8718).
//!
//! A `ConnectRequest` (or an unusable-cookie refresh, or a version probe) has an unproved source,
//! so the server answers it without charging anything to that source's proved budgets. Before this
//! module the answers had one server-wide first-come cap: a forged flood from many prefixes spent
//! it in the first milliseconds of every second, and a real client's request was answered about
//! once in two hundred (the soak's `forged-handshake` profile: 100,000 forged requests a second
//! from 1,000 /24s, half the real clients never joined).
//!
//! Here each source prefix (IPv4 /24, IPv6 /56: `version_probe::probe_prefix`) has a token
//! bucket, one second deep, refilled at its fair share of the server-wide rate: a prefix asking
//! alone has all of it; several asking (in this or the last second) split 15/16 of it evenly, the
//! rest being headroom for a prefix that asks late in a second (max-min fairness over the active
//! prefixes: the per-flow fair queueing of Demers, Keshav and Shenker, SIGCOMM 1989, and RFC 7806
//! § 2). A forger spending its own prefixes' shares cannot touch another prefix's. The
//! server-wide cap stays the hard bound on what one second may send, and a request it sheds still
//! pays its prefix's token, so no bucket refills while the cap is closed.
//!
//! An answer whose cookie comes back valid (a `ChallengeResponse` or `KeyInit` from that source)
//! gives its token back to the prefix: the address is proved, the answer was not a reflection. A
//! blind forger never sees its answers, so its prefixes never earn a refund, while a prefix of real
//! clients (one office, one carrier-grade NAT, the soak's whole client range) gets an answer for
//! every client that completes the exchange, not one per share interval.
//!
//! The server-wide cap bounds the answers of a second that nobody proved (ADR-8764). A proof
//! matches one exact answer: the (address, client nonce) pair a `Challenge` went to in this or the
//! last second, and it matches once, so a cookie replayed for its whole validity proves nothing
//! more, an answer to a neighbour in the prefix is never proved by someone else's cookie, and a
//! refund with no admitted answer outstanding for the prefix returns nothing. Only a proof of an
//! answer of this second gives back a slot of this second's count. A plaintext `Deny` sent after a
//! proof takes the proved answer's slot, or passes the cap as a fresh answer.
//!
//! Memory is bounded: at most [`MAX_PREFIXES`] prefixes have their own bucket (56 bytes each,
//! about 7.5 MB with the map's table at the bound); past that, new prefixes share one overflow
//! bucket until idle ones are forgotten (two seconds without a request). The map is the standard
//! library's `HashMap`, whose SipHash keys are random per process, so a forger cannot aim
//! collisions; no decision depends on its iteration order, so a seeded run stays deterministic.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};

use crate::version_probe::probe_prefix;

/// Most source prefixes with their own bucket (spec 120 § 11.5 memory bounds): about 7.5 MB at
/// the bound with the map's table.
pub(crate) const MAX_PREFIXES: usize = 65_536;

/// A prefix that has not asked for this many whole seconds is forgotten.
const IDLE_SECONDS: u64 = 2;

/// Fractions of a token, so shares below one answer a second still refill.
const MILLI: u64 = 1_000;

/// Most answers remembered as provable (this second's and the last second's together, ADR-8764;
/// 40 bytes each, at most about 11 MB with both tables' capacity): a full table only makes further
/// answers unrefundable, never more of them.
const MAX_PROVABLE: usize = 65_536;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens, in thousandths of an answer.
    milli: u64,
    /// When `milli` was last refilled.
    refilled_ms: u64,
    /// The last whole second this prefix asked in.
    seen_second: u64,
    /// Answers to this prefix admitted in the current second and not yet proved (ADR-8764): a
    /// refund with none outstanding returns nothing.
    outstanding: u32,
}

/// Which second a proved answer was sent in (`AnswerShare::take_proved`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Proved {
    /// This second: its slot of the server-wide count may be given back or reused.
    ThisSecond,
    /// The last second: that second's count is closed, so only the prefix's token comes back.
    LastSecond,
}

/// Per-prefix fair shares of the pre-proof answer budget, under the server-wide hard cap.
#[derive(Debug, Clone)]
pub(crate) struct AnswerShare {
    per_second: u32,
    /// Answers sent in `second` and not proved, server-wide (the hard cap).
    used: u64,
    second: u64,
    active_last: u64,
    active_now: u64,
    buckets: HashMap<IpAddr, Bucket>,
    overflow: Option<Bucket>,
    /// Exact (address, client nonce) pairs answered in `second` and not proved yet (ADR-8764).
    provable_now: HashSet<(SocketAddr, u64)>,
    /// The same for the second before `second`.
    provable_last: HashSet<(SocketAddr, u64)>,
}

impl AnswerShare {
    /// A budget of `per_second` answers a second, server-wide (`0` = no limit).
    pub(crate) fn new(per_second: u32) -> Self {
        Self {
            per_second,
            used: 0,
            second: 0,
            active_last: 0,
            active_now: 0,
            buckets: HashMap::new(),
            overflow: None,
            provable_now: HashSet::new(),
            provable_last: HashSet::new(),
        }
    }

    /// Prefixes with their own bucket now.
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        self.buckets.len()
    }

    /// One prefix's refill rate in thousandths of an answer a second. A prefix asking alone has
    /// the whole rate. When several ask (in this or the last second), each has an even split of
    /// 15/16 of it: the rest is headroom, so a prefix whose request comes late in a second still
    /// finds the server-wide cap open. A share may be under one answer a second; the shares of
    /// competing prefixes never add up to the cap.
    fn share_milli(&self) -> u64 {
        let active = self.active_last.max(self.active_now);
        let rate = u64::from(self.per_second) * MILLI;
        if active <= 1 {
            rate
        } else {
            (rate * 15 / 16 / active).max(1)
        }
    }

    /// A bucket's depth: one second of share, and at least one whole answer.
    fn depth(share: u64) -> u64 {
        share.max(MILLI)
    }

    fn roll(&mut self, now_ms: u64) {
        let second = now_ms / 1_000;
        if second == self.second {
            return;
        }
        self.active_last = if second == self.second + 1 {
            self.active_now
        } else {
            0
        };
        if second == self.second + 1 {
            std::mem::swap(&mut self.provable_now, &mut self.provable_last);
        } else {
            self.provable_last.clear();
        }
        self.provable_now.clear();
        self.active_now = 0;
        self.used = 0;
        self.second = second;
        self.buckets.retain(|_, b| {
            b.outstanding = 0;
            b.seen_second + IDLE_SECONDS > second
        });
        if self
            .overflow
            .is_some_and(|b| b.seen_second + IDLE_SECONDS <= second)
        {
            self.overflow = None;
        }
        if let Some(b) = self.overflow.as_mut() {
            b.outstanding = 0;
        }
    }

    /// True when an answer to `ip` may be sent at `now_ms`; charges its prefix and the
    /// server-wide cap.
    pub(crate) fn try_admit(&mut self, ip: IpAddr, now_ms: u64) -> bool {
        if self.per_second == 0 {
            return true;
        }
        self.roll(now_ms);
        let share = self.share_milli();
        let second = self.second;
        let key = probe_prefix(ip);
        let full = self.buckets.len() >= MAX_PREFIXES;
        let bucket = match self.buckets.get_mut(&key) {
            Some(b) => b,
            None if !full => self.buckets.entry(key).or_insert(Bucket {
                milli: Self::depth(share),
                refilled_ms: now_ms,
                seen_second: u64::MAX,
                outstanding: 0,
            }),
            None => self.overflow.get_or_insert(Bucket {
                milli: Self::depth(share),
                refilled_ms: now_ms,
                seen_second: u64::MAX,
                outstanding: 0,
            }),
        };
        if bucket.seen_second != second {
            bucket.seen_second = second;
            self.active_now += 1;
        }
        let gained = now_ms.saturating_sub(bucket.refilled_ms).saturating_mul(share) / 1_000;
        if gained > 0 {
            bucket.milli = bucket.milli.saturating_add(gained).min(Self::depth(share));
            bucket.refilled_ms = now_ms;
        }
        if bucket.milli < MILLI {
            return false;
        }
        // The prefix pays even when the hard cap then sheds the request: a bucket that kept its
        // token while the cap was closed would refill to its depth, and every prefix would spend
        // a full bucket at the start of the next second, closing the cap again (a forged flood
        // would hold it closed for most of every second).
        bucket.milli -= MILLI;
        if self.used >= u64::from(self.per_second) {
            return false;
        }
        self.used += 1;
        bucket.outstanding = bucket.outstanding.saturating_add(1);
        true
    }

    /// [`AnswerShare::try_admit`] for an answer that carries a cookie for `client_nonce` (a
    /// `Challenge`): when admitted, the exact pair (`addr`, `client_nonce`) is remembered as
    /// provable for this second and the next, so a valid cookie from that pair can prove it once
    /// ([`AnswerShare::take_proved`]).
    pub(crate) fn try_admit_answer(&mut self, addr: SocketAddr, client_nonce: u64, now_ms: u64) -> bool {
        if !self.try_admit(addr.ip(), now_ms) {
            return false;
        }
        if self.per_second != 0 && self.provable_now.len() + self.provable_last.len() < MAX_PROVABLE {
            self.provable_now.insert((addr, client_nonce));
        }
        true
    }

    /// A valid cookie for (`addr`, `client_nonce`) came back at `now_ms`. If an answer to that
    /// exact pair was admitted in this second or the last one and not proved yet, it is proved
    /// now (once: a replay of the cookie finds nothing) and the second it was sent in is
    /// returned. `None`: nothing to prove, so whatever the server sends next is a fresh
    /// pre-proof answer. A pair the server never answered (another address of the prefix, a
    /// forged source) proves nothing.
    pub(crate) fn take_proved(&mut self, addr: SocketAddr, client_nonce: u64, now_ms: u64) -> Option<Proved> {
        if self.per_second == 0 {
            return None;
        }
        self.roll(now_ms);
        let pair = (addr, client_nonce);
        if self.provable_now.remove(&pair) {
            Some(Proved::ThisSecond)
        } else if self.provable_last.remove(&pair) {
            Some(Proved::LastSecond)
        } else {
            None
        }
    }

    /// Gives a proved answer's budget back (ADR-8718, ADR-8764): the token to `ip`'s prefix and,
    /// for an answer of this second, its slot of the server-wide count, so a prefix of real
    /// clients is answered as fast as they prove their cookies.
    pub(crate) fn refund_proved(&mut self, ip: IpAddr, proved: Proved, now_ms: u64) {
        match proved {
            Proved::ThisSecond => {
                self.refund(ip, now_ms);
            }
            Proved::LastSecond => {
                self.roll(now_ms);
                let depth = Self::depth(self.share_milli());
                if let Some(b) = self.buckets.get_mut(&probe_prefix(ip)) {
                    b.milli = b.milli.saturating_add(MILLI).min(depth);
                }
            }
        }
    }

    /// One answer to `ip`'s prefix admitted this second was proved at `now_ms`: the prefix gets
    /// its token back (never past the bucket's depth) and the server-wide count its slot. True
    /// when something was returned. With no answer outstanding for the prefix this second (every
    /// one already proved, or none sent) nothing is returned, so the count of unproved answers
    /// never drops below what was sent (ADR-8764). Prefixes without their own bucket earn
    /// nothing. The server calls this only for a pair [`AnswerShare::take_proved`] matched.
    pub(crate) fn refund(&mut self, ip: IpAddr, now_ms: u64) -> bool {
        if self.per_second == 0 {
            return false;
        }
        self.roll(now_ms);
        let depth = Self::depth(self.share_milli());
        let Some(b) = self.buckets.get_mut(&probe_prefix(ip)) else {
            return false;
        };
        if b.outstanding == 0 {
            return false;
        }
        b.outstanding -= 1;
        b.milli = b.milli.saturating_add(MILLI).min(depth);
        self.used = self.used.saturating_sub(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// A flood from many prefixes spends their shares, not a quiet prefix's: the real prefix keeps
    /// about its fair share of a second while 1,000 forged /24s ask 100 times a second each.
    #[test]
    fn a_flood_from_many_prefixes_leaves_a_quiet_prefix_its_share() {
        let mut s = AnswerShare::new(4_096);
        let mut real_answered = 0;
        let mut real_asked = 0;
        // Three seconds in 1 ms steps: 100 forged requests per ms, round robin over 1,000 /24s,
        // and, after the flood's first second, one real request every 250 ms.
        for ms in 0..3_000_u64 {
            for k in 0..100_u64 {
                let p = ((ms * 100 + k) % 1_000) as u16;
                s.try_admit(v4(20 + (p / 256) as u8, (p % 256) as u8, 7, 9), ms);
            }
            if ms.is_multiple_of(250) && ms >= 1_000 {
                real_asked += 1;
                if s.try_admit(v4(10, 1, 0, 5), ms) {
                    real_answered += 1;
                }
            }
        }
        assert_eq!(real_asked, 8);
        assert_eq!(
            real_answered, real_asked,
            "four requests a second fit the real prefix's share (about 3.8/s)"
        );
    }

    /// The old single server-wide cap, for contrast: the same flood leaves the real prefix
    /// nothing once the second's budget is gone.
    #[test]
    fn the_server_wide_cap_alone_is_spent_by_the_flood() {
        let mut g = crate::crypto::HandshakeGovernor::new(4_096);
        let mut real_answered = 0;
        for ms in 1_000..3_000_u64 {
            for _ in 0..100 {
                g.try_admit(ms);
            }
            if ms.is_multiple_of(250) && g.try_admit(ms) {
                real_answered += 1;
            }
        }
        assert!(
            real_answered <= 2,
            "only a request in the first milliseconds of a second finds it open: {real_answered} of 8"
        );
    }

    /// One prefix alone may use the whole budget, and never more than the hard cap in a second.
    #[test]
    fn a_lone_prefix_gets_the_whole_rate_but_not_past_the_cap() {
        let mut s = AnswerShare::new(64);
        let answered = (0..1_000).filter(|_| s.try_admit(v4(10, 0, 0, 1), 5_000)).count();
        assert_eq!(answered, 64, "the whole cap in its first second");
        let next = (0..1_000).filter(|_| s.try_admit(v4(10, 0, 0, 1), 6_000)).count();
        assert_eq!(next, 64, "one second of share refills, and the cap is the bound");
    }

    /// Every address of one /24 (and of one IPv6 /56) shares one bucket.
    #[test]
    fn a_prefix_is_one_bucket() {
        let mut s = AnswerShare::new(16);
        let a = (1..=200)
            .filter(|h| s.try_admit(v4(10, 0, 0, *h as u8), 0))
            .count();
        assert_eq!(a, 16, "one prefix, one share (the whole rate when alone)");
        let v6 = |x: u16| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, x, 1, 2, 3, 4));
        assert_eq!(s.tracked(), 1);
        s.try_admit(v6(0x00ff), 0);
        s.try_admit(v6(0x0001), 0);
        assert_eq!(s.tracked(), 2, "both addresses are one /56");
    }

    /// A proved answer gives its token back, so a prefix of real clients is answered as fast as
    /// they prove their cookies; an unproved prefix is not.
    #[test]
    fn a_proved_answer_refunds_its_prefix() {
        let mut s = AnswerShare::new(16);
        let ip = v4(10, 1, 0, 1);
        let spent = (0..100).filter(|_| s.try_admit(ip, 0)).count();
        assert_eq!(spent, 16);
        assert!(!s.try_admit(ip, 0), "empty");
        s.refund(ip, 0);
        assert!(
            s.try_admit(ip, 0),
            "the refund is spendable, past the spent server-wide cap"
        );
        assert!(!s.try_admit(ip, 0));
        s.refund(v4(10, 9, 0, 1), 0);
        assert_eq!(
            s.tracked(),
            1,
            "a refund to a prefix that never asked makes no bucket"
        );
    }

    /// Idle prefixes are forgotten, and the table never passes its bound.
    #[test]
    fn idle_prefixes_are_forgotten_and_the_table_is_bounded() {
        let mut s = AnswerShare::new(1_000_000);
        for p in 0..(MAX_PREFIXES as u32 + 10) {
            let [_, a, b, c] = p.to_be_bytes();
            s.try_admit(v4(30 + a, b, c, 1), 0);
        }
        assert_eq!(s.tracked(), MAX_PREFIXES);
        s.try_admit(v4(1, 1, 1, 1), 1_000);
        assert_eq!(s.tracked(), MAX_PREFIXES, "still seen within the idle window");
        s.try_admit(v4(1, 1, 1, 1), 2_000);
        assert_eq!(s.tracked(), 1, "the rest forgotten after two quiet seconds");
    }

    /// Zero is no limit.
    #[test]
    fn zero_is_unlimited() {
        let mut s = AnswerShare::new(0);
        assert!((0..10_000).all(|_| s.try_admit(v4(1, 2, 3, 4), 0)));
        assert_eq!(s.tracked(), 0);
    }

    /// Review (v06-net-soak-gate-reds): a refund that does not store a token must not shrink the
    /// server-wide count. Prefix A spends the cap and is proved back to a full bucket. Prefix B
    /// then spends the reopened cap. One more `refund` for A finds the bucket already over its
    /// (now smaller) depth, stores nothing, and still decrements `used`, so another answer goes
    /// out in a second that has already sent the cap.
    #[test]
    fn a_refund_with_nothing_to_return_does_not_reopen_the_cap() {
        let mut s = AnswerShare::new(4);
        let a = v4(10, 0, 0, 1);
        let b = v4(20, 0, 0, 1);
        assert_eq!((0..10).filter(|_| s.try_admit(a, 0)).count(), 4);
        for _ in 0..4 {
            s.refund(a, 0);
        }
        assert_eq!(
            (0..10).filter(|_| s.try_admit(b, 0)).count(),
            4,
            "B spends the cap A just gave back"
        );
        s.refund(a, 0);
        let extra =
            (0..10).filter(|_| s.try_admit(a, 0)).count() + (0..10).filter(|_| s.try_admit(b, 0)).count();
        assert_eq!(
            extra, 0,
            "a refund that stored no token reopened a spent cap ({extra} more answers)"
        );
    }

    /// Review: an honest prefix that already holds a slot is not dropped to make room for a
    /// forger who fills the table and keeps every other slot fresh. After the forger goes quiet
    /// the honest prefix is the one bucket left.
    #[test]
    fn a_tracked_prefix_is_not_evicted_when_the_table_stays_full() {
        let mut s = AnswerShare::new(4_096);
        let honest = v4(10, 1, 0, 5);
        assert!(s.try_admit(honest, 0));
        for p in 0..MAX_PREFIXES as u32 {
            let [_, a, b, c] = p.to_be_bytes();
            s.try_admit(v4(30 + a, b, c, 1), 0);
        }
        assert_eq!(s.tracked(), MAX_PREFIXES);
        // Honest asks first in the next second, then the forger refreshes every slot.
        assert!(
            s.try_admit(honest, 1_000),
            "a resident prefix still has its own bucket"
        );
        for p in 0..MAX_PREFIXES as u32 {
            let [_, a, b, c] = p.to_be_bytes();
            s.try_admit(v4(30 + a, b, c, 1), 1_000);
        }
        assert_eq!(
            s.tracked(),
            MAX_PREFIXES,
            "refreshing a full table does not grow it"
        );
        // seen in second 2, so the sweep at second 3 keeps this prefix and drops the forger.
        let _ = s.try_admit(honest, 2_000);
        let _ = s.try_admit(honest, 3_000);
        assert_eq!(
            s.tracked(),
            1,
            "the forger's idle prefixes left; the prefix that kept asking stayed"
        );
    }
}
