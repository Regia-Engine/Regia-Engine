// SPDX-License-Identifier: MPL-2.0
//! Per-peer reliable message cap.
//!
//! A fixed one-second window counts reliable messages per peer and refuses the rest, so one chatty
//! system cannot flood the reliable channel (and its resend queue) for a peer. `0` disables it.

use std::collections::HashMap;
use std::net::IpAddr;

/// Counts and gates reliable sends for one peer.
#[derive(Debug, Clone, Default)]
pub struct ReliableRateLimiter {
    per_second: u32,
    window_start_ms: Option<u64>,
    accepted: u32,
    refused_total: u64,
}

impl ReliableRateLimiter {
    /// A limiter admitting `per_second` messages per second (`0` = unlimited).
    pub fn new(per_second: u32) -> Self {
        Self {
            per_second,
            ..Self::default()
        }
    }

    /// Admits one message at `now_ms`, or refuses it when the window's cap is used up.
    pub fn try_admit(&mut self, now_ms: u64) -> bool {
        if self.per_second == 0 {
            return true;
        }
        match self.window_start_ms {
            Some(start) if now_ms.saturating_sub(start) < 1_000 => {}
            _ => {
                self.window_start_ms = Some(now_ms);
                self.accepted = 0;
            }
        }
        if self.accepted >= self.per_second {
            self.refused_total += 1;
            return false;
        }
        self.accepted += 1;
        true
    }

    /// Messages refused since creation.
    pub fn refused_total(&self) -> u64 {
        self.refused_total
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SourceBudget {
    /// `None` until an IP lands here; a slot idle for two seconds counts as empty.
    last_ms: Option<u64>,
    tokens: u32,
}

/// A token bucket per source IP, for traffic that arrives before any session exists (handshakes,
/// relay control). The bucket holds two seconds of its rate.
///
/// The buckets are a fixed table indexed by a keyed hash of the IP, so forged sources cannot grow
/// it. Several addresses can land in one slot and share its tokens. A drained slot refuses a
/// colliding address until elapsed time refills the bucket. The key stops an attacker from
/// choosing the slot, and it does not cap how many addresses share one.
#[derive(Clone)]
pub struct SourceLimiter {
    per_second: u32,
    slots: usize,
    table: Vec<SourceBudget>,
    key: [u8; 32],
    refused_total: u64,
}

impl std::fmt::Debug for SourceLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceLimiter")
            .field("per_second", &self.per_second)
            .field("refused_total", &self.refused_total)
            .finish_non_exhaustive()
    }
}

impl SourceLimiter {
    /// A limiter admitting `per_second` requests per IP per second (`0` = unlimited), with
    /// `slots` buckets indexed under the secret `key`. The table is allocated on the first request.
    pub fn new(per_second: u32, slots: usize, key: [u8; 32]) -> Self {
        Self {
            per_second,
            slots: slots.max(1),
            table: Vec::new(),
            key,
            refused_total: 0,
        }
    }

    /// Buckets in the table.
    pub fn slots(&self) -> usize {
        self.table.len()
    }

    /// Requests refused since creation.
    pub fn refused_total(&self) -> u64 {
        self.refused_total
    }

    /// Admits one request from `ip` at `now_ms`. Every address of one IPv6 /64 (and both
    /// spellings of an IPv4 address) share a bucket ([`crate::addr_block`]).
    pub fn admit(&mut self, ip: IpAddr, now_ms: u64) -> bool {
        if self.per_second == 0 {
            return true;
        }
        if self.table.is_empty() {
            self.table.resize(self.slots, SourceBudget::default());
        }
        // One bucket per IPv4 address or IPv6 /64 (ADR-3283): a /64 rotating addresses is one source.
        let octets = crate::addr_block::AddrBlock::of(ip).key_bytes();
        let hash = blake3::keyed_hash(&self.key, &octets);
        let mut word = [0u8; 8];
        word.copy_from_slice(&hash.as_bytes()[..8]);
        let index = (u64::from_le_bytes(word) % self.table.len() as u64) as usize;
        let Some(budget) = self.table.get_mut(index) else {
            return true;
        };
        let burst = self.per_second.saturating_mul(2);
        match budget.last_ms {
            None => {
                *budget = SourceBudget {
                    last_ms: Some(now_ms),
                    tokens: burst,
                }
            }
            Some(last) => {
                // Saturating: a huge forward clock jump is a full refill, not an overflow.
                let gained = now_ms
                    .saturating_sub(last)
                    .saturating_mul(u64::from(self.per_second))
                    / 1000;
                if gained > 0 {
                    budget.tokens = u64::from(budget.tokens)
                        .saturating_add(gained)
                        .min(u64::from(burst)) as u32;
                    budget.last_ms = Some(now_ms);
                }
            }
        }
        if budget.tokens == 0 {
            self.refused_total += 1;
            return false;
        }
        budget.tokens -= 1;
        true
    }
}

/// One limiter per peer key.
#[derive(Debug, Clone, Default)]
pub struct ReliableSendGovernor<K: std::hash::Hash + Eq> {
    per_second: u32,
    peers: HashMap<K, ReliableRateLimiter>,
}

impl<K: std::hash::Hash + Eq> ReliableSendGovernor<K> {
    /// A governor admitting `per_second` messages per peer per second (`0` = unlimited).
    pub fn new(per_second: u32) -> Self {
        Self {
            per_second,
            peers: HashMap::new(),
        }
    }

    /// Admits one message for `peer` at `now_ms`.
    pub fn try_admit(&mut self, peer: K, now_ms: u64) -> bool {
        let per_second = self.per_second;
        self.peers
            .entry(peer)
            .or_insert_with(|| ReliableRateLimiter::new(per_second))
            .try_admit(now_ms)
    }

    /// Forgets a peer.
    pub fn remove(&mut self, peer: &K) {
        self.peers.remove(peer);
    }

    /// Messages refused for `peer` since it was first seen.
    pub fn refused(&self, peer: &K) -> u64 {
        self.peers.get(peer).map_or(0, ReliableRateLimiter::refused_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_per_window_and_resets() {
        let mut l = ReliableRateLimiter::new(3);
        assert!((0..3).all(|_| l.try_admit(0)));
        assert!(!l.try_admit(999));
        assert!(l.try_admit(1000), "a new window opens after one second");
        assert_eq!(l.refused_total(), 1);
        let mut unlimited = ReliableRateLimiter::new(0);
        assert!((0..10_000).all(|_| unlimited.try_admit(0)));
    }

    #[test]
    fn source_limiter_is_per_ip_and_refills() {
        let ip = |n: u8| IpAddr::from([10, 0, 0, n]);
        let mut l = SourceLimiter::new(10, 65_536, [1; 32]);
        assert_eq!(
            (0..30).filter(|_| l.admit(ip(1), 0)).count(),
            20,
            "two seconds of rate"
        );
        assert_eq!(l.refused_total(), 10);
        assert!(l.admit(ip(2), 0), "another IP");
        assert!(l.admit(ip(1), 100), "refilled by one");
        assert!(!l.admit(ip(1), 100));
        assert_eq!(
            (0..30).filter(|_| l.admit(ip(1), 2_100)).count(),
            20,
            "full again"
        );
        let mut open = SourceLimiter::new(0, 1, [1; 32]);
        assert!((0..1000).all(|i| open.admit(ip(i as u8), 0)));
        assert_eq!(open.slots(), 0, "never allocated");
    }

    /// rc1-rev-net-k 11: a huge forward clock jump refills a drained bucket instead of overflowing
    /// the refill product (a debug panic, a wrapped and possibly tiny refill in release).
    #[test]
    fn a_huge_clock_jump_refills_without_overflow() {
        let ip = IpAddr::from([10, 0, 0, 9]);
        let mut l = SourceLimiter::new(1_000, 16, [3; 32]);
        assert_eq!((0..2_000).filter(|_| l.admit(ip, 0)).count(), 2_000);
        assert!(!l.admit(ip, 0), "drained");
        assert!(l.admit(ip, u64::MAX / 2), "a jump refills it");
        let mut wide = SourceLimiter::new(u32::MAX, 16, [3; 32]);
        assert!(wide.admit(ip, 0));
        assert!(wide.admit(ip, u64::MAX));
    }

    /// One slot, drained by one address, refuses a second address at the same timestamp.
    #[test]
    fn a_drained_slot_refuses_a_colliding_address() {
        let mut l = SourceLimiter::new(10, 1, [1; 32]);
        let a = IpAddr::from([10, 0, 0, 1]);
        let b = IpAddr::from([10, 0, 0, 2]);
        assert_eq!(
            (0..20).filter(|_| l.admit(a, 0)).count(),
            20,
            "two seconds of rate"
        );
        assert!(!l.admit(b, 0), "the only slot is shared and empty");
    }

    /// Review: forged sources cannot grow the table. After two seconds every bucket is full again.
    #[test]
    fn forged_sources_neither_grow_the_table_nor_lock_anyone_out() {
        let mut l = SourceLimiter::new(10, 4096, [1; 32]);
        for i in 0..200_000u32 {
            l.admit(IpAddr::from(i.to_be_bytes()), 0);
        }
        assert_eq!(l.slots(), 4096);
        // Two seconds on every bucket is full again, whatever filled it.
        assert!(l.admit(IpAddr::from([203, 0, 113, 7]), 2_000));
    }

    #[test]
    fn governor_is_per_peer() {
        let mut g = ReliableSendGovernor::new(1);
        assert!(g.try_admit(1u16, 0));
        assert!(!g.try_admit(1u16, 0));
        assert!(g.try_admit(2u16, 0), "another peer has its own window");
        assert_eq!(g.refused(&1), 1);
        g.remove(&1);
        assert_eq!(g.refused(&1), 0);
    }
}
