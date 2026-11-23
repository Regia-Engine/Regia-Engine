// SPDX-License-Identifier: MPL-2.0
//! The server send loop's priority and budgets (spec 72 §§ 6-7): per (connection, entity)
//! priority accumulators, the bandwidth budget with one tick of carry-over, the spawn budget,
//! the total outbound cap and the per-connection bandwidth counters.
//!
//! Active with the relevance pipeline (`relevance::install_net_interest`, feature `ecs`) and with
//! [`crate::external_replication`] (no ECS). The gather
//! encodes every candidate (spawn records and deltas), [`select`] picks what fits in priority
//! order, and only the picked records change what the server believes the connection holds; the
//! rest keep their accumulators and are encoded again next tick against the same baseline.

use std::collections::{HashMap, VecDeque};

use crate::fast_map::FastMap;
use crate::model::{NetId, NetTick};

/// Ticks per second the budgets assume (the fixed tick rate).
pub const DEFAULT_TICK_HZ: f64 = 60.0;
/// Lowest client rate `net.set_client_rate` accepts, in bytes per second.
pub const MIN_CLIENT_RATE: u32 = 4096;
/// Highest client rate `net.set_client_rate` accepts, in bytes per second.
pub const MAX_CLIENT_RATE: u32 = 10 * 1024 * 1024;
/// Entity state larger than this is not sent (`net.state_too_large`).
pub const MAX_ENTITY_STATE_BYTES: usize = 16 * 1024;
/// Bandwidth counters cover this many seconds.
pub const BANDWIDTH_WINDOW_S: usize = 5;
/// Resync requests a client may make per second.
pub const RESYNC_PER_SECOND: u32 = 10;

/// Per (connection, entity) send state, public view. See `PeerInterest::send_state`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EntitySendState {
    /// The priority accumulator.
    pub acc: f32,
    /// The tick the entity was last sent to this connection.
    pub last_sent: Option<NetTick>,
}

/// One entity's candidacy this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The entity.
    pub id: NetId,
    /// A spawn record (else a delta).
    pub spawn: bool,
    /// Its encoded size in bytes.
    pub bytes: usize,
    /// The accumulator after this tick's gain.
    pub acc: f32,
    /// `max_interval_ticks` reached: packed first.
    pub forced: bool,
}

/// Rolling byte counts in one-second buckets (5 s).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ByteWindow {
    second: u64,
    buckets: [u64; BANDWIDTH_WINDOW_S],
}

impl ByteWindow {
    fn roll(&mut self, second: u64) {
        if second > self.second {
            let steps = (second - self.second).min(BANDWIDTH_WINDOW_S as u64);
            for s in 1..=steps {
                let idx = ((self.second + s) % BANDWIDTH_WINDOW_S as u64) as usize;
                if let Some(b) = self.buckets.get_mut(idx) {
                    *b = 0;
                }
            }
            self.second = second;
        }
    }

    /// Adds bytes at a second.
    pub fn add(&mut self, second: u64, bytes: u64) {
        self.roll(second);
        let idx = (second % BANDWIDTH_WINDOW_S as u64) as usize;
        if let Some(b) = self.buckets.get_mut(idx) {
            *b += bytes;
        }
    }

    /// Bytes per second over the window ending at `second`.
    pub fn per_second(&self, second: u64) -> f64 {
        let mut w = self.clone();
        w.roll(second);
        w.buckets.iter().sum::<u64>() as f64 / BANDWIDTH_WINDOW_S as f64
    }
}

/// Bandwidth counters of one connection.
#[derive(Debug, Clone, Default)]
pub struct ConnBandwidth {
    /// Every byte of the connection's frames.
    pub total: ByteWindow,
    /// By registered component name.
    pub by_component: FastMap<&'static str, ByteWindow>,
    /// By entity (spawn records and deltas).
    pub by_entity: FastMap<NetId, ByteWindow>,
    /// Bytes per tick over the last 300 ticks (budget tests).
    pub recent: VecDeque<(NetTick, u32)>,
}

/// Per-connection counters (spec 72 § 10).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnSendCounters {
    /// Entities relevant (or lingering) at the last tick.
    pub relevant: u32,
    /// Spawn records waiting for the spawn budget.
    pub spawns_pending: u32,
    /// Bytes charged at the last tick.
    pub budget_used: u32,
    /// The last tick's budget in bytes.
    pub budget: u32,
    /// Entities whose accumulator exceeds 10× their priority.
    pub starved: u32,
    /// Deltas deferred by the budget at the last tick.
    pub deferred: u32,
    /// Deltas held back by the profile's distance rate tiers at the last tick (spec 120 § 8.3).
    pub tier_held: u32,
    /// Updates sent at the last tick, by distance tier: `[near, mid, far]` (entities whose profile
    /// has no tiers count as near).
    pub tier_sent: [u32; 3],
    /// Resync requests honored.
    pub resyncs: u64,
    /// Resync requests over the rate limit.
    pub resyncs_limited: u64,
}

/// The send state of one connection.
#[derive(Debug, Clone, Default)]
pub struct ConnSendState {
    /// Budget carried into the next tick (negative after an overspend).
    pub carry: f64,
    /// A rate set for this connection (`net.set_client_rate`), else the project's.
    pub rate_override: Option<u32>,
    /// The rate the client declared in its session `Hello` (`relevance::NetSessionPeers`):
    /// it lowers the project's rate for this connection (not below [`MIN_CLIENT_RATE`]), never
    /// raises it (spec 72 § 7, ADR-7213).
    pub session_rate: Option<u32>,
    /// Message bytes written after the last gather, charged at the next.
    pub message_debt: u32,
    /// Counters.
    pub counters: ConnSendCounters,
    /// Bandwidth.
    pub bandwidth: ConnBandwidth,
    /// Resync tokens (refilled each second).
    pub resync_tokens: u32,
    /// The second of the last refill.
    pub resync_second: Option<u64>,
    /// Recent resync ticks per entity (loop detection).
    pub resync_history: HashMap<NetId, VecDeque<NetTick>>,
}

impl ConnSendState {
    /// The client rate in bytes per second.
    pub fn rate(&self, project: u32) -> u32 {
        // A declared rate lowers the project's, down to `MIN_CLIENT_RATE` (a client cannot starve
        // its own copies with a tiny limit), never above the project's.
        self.rate_override.unwrap_or_else(|| {
            self.session_rate
                .map_or(project, |declared| declared.max(MIN_CLIENT_RATE).min(project))
        })
    }

    /// Whether a resync for an entity may be honored now (10 per second per client), and whether
    /// it makes a loop (three of the same entity within 5 s, logged `net.resync_loop`).
    pub fn admit_resync(&mut self, id: NetId, tick: NetTick, tick_hz: f64) -> (bool, bool) {
        let second = (f64::from(tick.0) / tick_hz) as u64;
        let window = (5.0 * tick_hz) as u32;
        if self.resync_second != Some(second) {
            self.resync_second = Some(second);
            self.resync_tokens = RESYNC_PER_SECOND;
            // Entities despawn: a history that left the window goes with them.
            self.resync_history
                .retain(|_, h| h.back().is_some_and(|t| tick.0.saturating_sub(t.0) < window));
        }
        if self.resync_tokens == 0 {
            self.counters.resyncs_limited += 1;
            return (false, false);
        }
        self.resync_tokens -= 1;
        self.counters.resyncs += 1;
        let h = self.resync_history.entry(id).or_default();
        h.push_back(tick);
        while h.front().is_some_and(|t| tick.0.saturating_sub(t.0) >= window) {
            h.pop_front();
        }
        let looped = h.len() >= 3;
        if looped {
            self.resync_history.remove(&id);
        }
        (true, looped)
    }
}

/// The priority gain of one entity this tick (spec 72 § 6): `priority × distance × view × owner`.
pub fn priority_gain(
    priority: f32,
    radius: Option<f32>,
    distance: Option<f32>,
    view_falloff: bool,
    in_view: bool,
    owned: bool,
    owner_boost: f32,
) -> f32 {
    let distance_factor = match (radius, distance) {
        (Some(r), Some(d)) if r > 0.0 => (1.0 - d / r).max(0.1),
        _ => 1.0,
    };
    let view_factor = if !view_falloff || in_view { 1.0 } else { 0.5 };
    let owner_factor = if owned { owner_boost } else { 1.0 };
    priority * distance_factor * view_factor * owner_factor
}

/// What one tick may send to one connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickBudget {
    /// Bytes this tick may use.
    pub bytes: f64,
    /// Spawn records this tick may carry.
    pub spawns: usize,
}

/// The per-tick bandwidth budget: `rate / tick_hz` scaled by the server cap, plus the carry
/// (at most one tick of unused budget; an overspend is paid back in full, spec 120 § 8.1).
pub fn tick_budget(rate: u32, tick_hz: f64, scale: f64, carry: f64, spawns: usize) -> TickBudget {
    let per_tick = f64::from(rate) / tick_hz * scale;
    TickBudget {
        bytes: per_tick + carry.min(per_tick),
        spawns,
    }
}

/// The send order (spec 72 § 6): forced entities first in `NetId` order, then by accumulator
/// (descending), ties by `NetId`.
pub fn candidate_order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.forced
        .cmp(&a.forced)
        .then_with(|| {
            if a.forced {
                a.id.cmp(&b.id)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| b.acc.total_cmp(&a.acc))
        .then_with(|| a.id.cmp(&b.id))
}

/// [`candidate_order`] as an integer, for ranking thousands of candidates per connection: ascending
/// keys are the send order. The high word is 0 when forced, else the accumulator in descending
/// total order; the low word is the id.
#[inline]
pub fn rank_key(forced: bool, acc: f32, id: NetId) -> u64 {
    let high = if forced {
        0
    } else {
        // `f32::total_cmp` as an unsigned integer, reversed.
        let bits = acc.to_bits() as i32;
        let ordered = (bits ^ (((bits >> 31) as u32) >> 1) as i32) as u32 ^ (1 << 31);
        (!ordered).max(1)
    };
    u64::from(high) << 32 | u64::from(id.0)
}

/// Packs one tick's records in send order (spec 72 § 7, spec 120 § 8.1). Every record obeys the
/// byte test, spawns and forced records too (adversarial row 4, ADR-8419): ordinary spawns also
/// stop at the spawn budget; a forced spawn (a joining connection's own or game-state entity,
/// spec 77 § 6.1) goes outside the spawn budget, and a forced delta (`max_interval_ticks`) goes
/// first, but both wait for bytes like any record. One record may go when nothing went yet and
/// budget remains (so a record larger than a tick's budget is not held forever); the overspend is
/// paid back from the next ticks' budgets. Once a record does not fit the tick is full: later
/// records are not offered, so they need not be encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Packer {
    budget: TickBudget,
    used: f64,
    spawns: usize,
    picked: usize,
    full: bool,
}

impl Packer {
    /// A packer for a budget, with bytes already charged this tick.
    pub fn new(budget: TickBudget, already_used: f64) -> Self {
        Self {
            budget,
            used: already_used,
            spawns: 0,
            picked: 0,
            full: false,
        }
    }

    /// Whether a record of this kind could still be picked (encode it only then).
    pub fn wants(&self, spawn: bool, forced: bool) -> bool {
        !self.full && (!spawn || forced || self.spawns < self.budget.spawns)
    }

    /// Offers a record of `bytes`; returns whether it is picked.
    pub fn offer(&mut self, spawn: bool, forced: bool, bytes: usize) -> bool {
        if !self.wants(spawn, forced) {
            return false;
        }
        let bytes = bytes as f64;
        let first = self.picked == 0 && self.used < self.budget.bytes;
        if self.used + bytes > self.budget.bytes && !first {
            self.full = true;
            return false;
        }
        if spawn && !forced {
            self.spawns += 1;
        }
        self.used += bytes;
        self.picked += 1;
        true
    }

    /// Bytes charged so far.
    pub fn used(&self) -> f64 {
        self.used
    }
}

/// Picks what this tick sends from encoded candidates ([`candidate_order`], then [`Packer`]).
/// Returns the picked ids and the bytes used.
pub fn select(cands: &mut [Candidate], budget: TickBudget, already_used: f64) -> (Vec<NetId>, f64) {
    cands.sort_by(candidate_order);
    let mut packer = Packer::new(budget, already_used);
    let picked = cands
        .iter()
        .filter(|c| packer.offer(c.spawn, c.forced, c.bytes))
        .map(|c| c.id)
        .collect();
    (picked, packer.used())
}

/// The starvation bound of spec 72 § 6: an entity of effective priority `p` among `n` competing
/// entities of effective priority at most `max_p`, with a budget fitting one entity per tick, is
/// sent within this many send ticks.
pub fn starvation_bound(n: usize, max_p: f32, p: f32) -> u32 {
    ((n as f32 * max_p / p).ceil() as u32).saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u32, bytes: usize, acc: f32) -> Candidate {
        Candidate {
            id: NetId(id),
            spawn: false,
            bytes,
            acc,
            forced: false,
        }
    }

    #[test]
    fn rank_key_sorts_like_the_candidate_order() {
        let accs = [
            -f32::INFINITY,
            -3.5,
            -0.0,
            0.0,
            1e-30,
            0.25,
            0.25,
            7.0,
            f32::MAX,
            f32::INFINITY,
        ];
        let mut list = Vec::new();
        for (i, acc) in accs.iter().enumerate() {
            for forced in [false, true] {
                let mut c = cand(40 - i as u32 * 3, 0, *acc);
                c.id = NetId(c.id.0 + u32::from(forced));
                c.forced = forced;
                list.push(c);
            }
        }
        let mut by_order = list.clone();
        by_order.sort_by(candidate_order);
        let mut by_key = list;
        by_key.sort_by_key(|c| rank_key(c.forced, c.acc, c.id));
        let ids = |l: &[Candidate]| l.iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(ids(&by_order), ids(&by_key));
    }

    #[test]
    fn gain_factors() {
        assert_eq!(priority_gain(2.0, None, None, true, false, false, 4.0), 1.0);
        assert_eq!(priority_gain(2.0, None, None, false, false, false, 4.0), 2.0);
        assert_eq!(
            priority_gain(1.0, Some(100.0), Some(50.0), true, true, false, 4.0),
            0.5
        );
        // Floor of 0.1 at and beyond the radius.
        assert!((priority_gain(1.0, Some(100.0), Some(150.0), true, true, false, 4.0) - 0.1).abs() < 1e-6);
        assert_eq!(priority_gain(1.0, None, None, true, true, true, 4.0), 4.0);
        // Only ratios matter: doubling every priority keeps the order.
        let a = priority_gain(1.0, Some(80.0), Some(20.0), true, true, false, 4.0);
        let b = priority_gain(3.0, Some(80.0), Some(60.0), true, false, false, 4.0);
        let a2 = priority_gain(2.0, Some(80.0), Some(20.0), true, true, false, 4.0);
        let b2 = priority_gain(6.0, Some(80.0), Some(60.0), true, false, false, 4.0);
        assert_eq!(a > b, a2 > b2);
    }

    #[test]
    fn select_orders_and_budgets() {
        let mut c = vec![
            cand(3, 40, 1.0),
            cand(1, 40, 5.0),
            cand(2, 40, 5.0),
            cand(4, 40, 9.0),
        ];
        let (picked, used) = select(
            &mut c,
            TickBudget {
                bytes: 100.0,
                spawns: 24,
            },
            0.0,
        );
        // 9.0 first, then the tie by NetId; the third does not fit.
        assert_eq!(picked, vec![NetId(4), NetId(1)]);
        assert_eq!(used, 80.0);
        // A budget smaller than one entity still sends one.
        let mut c = vec![cand(1, 500, 1.0), cand(2, 10, 0.5)];
        let (picked, _) = select(
            &mut c,
            TickBudget {
                bytes: 100.0,
                spawns: 24,
            },
            0.0,
        );
        assert_eq!(picked, vec![NetId(1)]);
        // Forced entities go first in NetId order, but within the bytes (spec 120 § 8.1,
        // adversarial row 4: the base sent both, 120 bytes on a 100-byte budget).
        let forced_three = || {
            vec![
                cand(5, 60, 9.0),
                Candidate {
                    forced: true,
                    ..cand(7, 60, 0.0)
                },
                Candidate {
                    forced: true,
                    ..cand(6, 60, 0.0)
                },
            ]
        };
        let (picked, used) = select(
            &mut forced_three(),
            TickBudget {
                bytes: 100.0,
                spawns: 24,
            },
            0.0,
        );
        assert_eq!(picked, vec![NetId(6)]);
        assert_eq!(used, 60.0);
        let (picked, _) = select(
            &mut forced_three(),
            TickBudget {
                bytes: 200.0,
                spawns: 24,
            },
            0.0,
        );
        assert_eq!(
            picked,
            vec![NetId(6), NetId(7), NetId(5)],
            "forced first, then by priority"
        );
        // Spawns stop at the spawn budget, and at the bytes (the base skipped the byte test for
        // spawns: all 24 went on a 50-byte budget).
        let thirty = || -> Vec<Candidate> {
            (0..30)
                .map(|i| Candidate {
                    spawn: true,
                    ..cand(i, 10, 1.0)
                })
                .collect()
        };
        let (picked, used) = select(
            &mut thirty(),
            TickBudget {
                bytes: 50.0,
                spawns: 24,
            },
            0.0,
        );
        assert_eq!(picked.len(), 5);
        assert_eq!(used, 50.0);
        let (picked, _) = select(
            &mut thirty(),
            TickBudget {
                bytes: 10_000.0,
                spawns: 24,
            },
            0.0,
        );
        assert_eq!(picked.len(), 24, "the spawn budget still holds");
        // A forced spawn goes outside the spawn budget, inside the bytes.
        let mut c = thirty();
        c.push(Candidate {
            spawn: true,
            forced: true,
            ..cand(99, 10, 0.0)
        });
        let (picked, _) = select(
            &mut c,
            TickBudget {
                bytes: 10_000.0,
                spawns: 24,
            },
            0.0,
        );
        assert_eq!(picked.len(), 25);
        assert_eq!(picked.first(), Some(&NetId(99)));
    }

    /// Spec 120 § 8.1: a record larger than a whole tick's budget still goes when it is the first
    /// of a tick with budget left (never held forever); with no budget left (debt), nothing goes,
    /// a forced record included.
    #[test]
    fn one_oversized_first_record_goes_and_debt_holds_everything() {
        let budget = |bytes: f64| TickBudget { bytes, spawns: 24 };
        let forced = Candidate {
            forced: true,
            ..cand(1, 5000, 0.0)
        };
        let (picked, used) = select(&mut [forced, cand(2, 10, 1.0)], budget(100.0), 0.0);
        assert_eq!(picked, vec![NetId(1)]);
        assert_eq!(used, 5000.0);
        let (picked, _) = select(&mut [forced, cand(2, 10, 1.0)], budget(-50.0), 0.0);
        assert!(picked.is_empty(), "debt holds even a forced record");
        let (picked, _) = select(&mut [forced], budget(100.0), 100.0);
        assert!(picked.is_empty(), "messages already spent the tick");
    }

    #[test]
    fn budget_carry_is_one_tick() {
        let b = tick_budget(6000, 60.0, 1.0, 1000.0, 24);
        assert_eq!(b.bytes, 200.0);
        let b = tick_budget(6000, 60.0, 1.0, -50.0, 24);
        assert_eq!(b.bytes, 50.0);
        let b = tick_budget(6000, 60.0, 0.5, 0.0, 24);
        assert_eq!(b.bytes, 50.0);
        // Defaults: 49_152 B/s and 30 send ticks/s (60 Hz / send_every_ticks 2).
        let b = tick_budget(49_152, 30.0, 1.0, 0.0, 24);
        assert_eq!(b.bytes, 1_638.4);
    }

    #[test]
    fn byte_window_rolls() {
        let mut w = ByteWindow::default();
        w.add(0, 500);
        w.add(1, 500);
        assert_eq!(w.per_second(1), 200.0);
        assert_eq!(w.per_second(5), 100.0);
        assert_eq!(w.per_second(6), 0.0);
        w.add(20, 50);
        assert_eq!(w.per_second(20), 10.0);
    }

    /// Audit L1 (muse2): the history does not outlive its five-second window.
    #[test]
    fn resync_history_is_pruned() {
        let mut s = ConnSendState::default();
        for i in 0..5 {
            s.admit_resync(NetId(i), NetTick(0), 60.0);
        }
        assert_eq!(s.resync_history.len(), 5);
        s.admit_resync(NetId(9), NetTick(60 * 6), 60.0);
        assert_eq!(s.resync_history.len(), 1, "only the new entry");
    }

    #[test]
    fn resync_rate_and_loop() {
        let mut s = ConnSendState::default();
        let mut admitted = 0;
        for i in 0..20u32 {
            if s.admit_resync(NetId(i), NetTick(0), 60.0).0 {
                admitted += 1;
            }
        }
        assert_eq!(admitted, RESYNC_PER_SECOND);
        let mut s = ConnSendState::default();
        assert_eq!(s.admit_resync(NetId(1), NetTick(0), 60.0), (true, false));
        assert_eq!(s.admit_resync(NetId(1), NetTick(60), 60.0), (true, false));
        assert_eq!(s.admit_resync(NetId(1), NetTick(120), 60.0), (true, true));
        // Spread over more than 5 s: no loop.
        let mut s = ConnSendState::default();
        for t in [0, 200, 400] {
            assert!(!s.admit_resync(NetId(1), NetTick(t), 60.0).1);
        }
    }

    #[test]
    fn starvation_bound_formula() {
        assert_eq!(starvation_bound(100, 3.0, 1.0), 301);
        assert_eq!(starvation_bound(10, 1.0, 1.0), 11);
    }
}
