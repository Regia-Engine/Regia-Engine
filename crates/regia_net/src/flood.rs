// SPDX-License-Identifier: MPL-2.0
//! Inbound flood guard (spec 70 § 6.6): one per connected peer, on the server.
//!
//! Per message type a token bucket gains `count / per_ticks` tokens each tick, capped at `count`.
//! Tokens are kept in integer units of `1 / per_ticks` so the bucket is exact and deterministic.
//! Per peer there is a cap on messages per tick and bytes per second. A peer that breaches becomes
//! `Slowed` (every per-type `count` halved); it is `Clear` again after `slowed_recover_ticks` quiet
//! ticks, and disconnected with `Flood` after `disconnect_after_slowed_ticks` of breaching while
//! `Slowed`. A frame after a server stall is not counted toward escalation.
//!
//! The guard runs at the processing tick, after a frame is stored. In front of it, the runtime sheds
//! frames that do not fit a peer's inbound queue ([`FloodGuard::note_overflow`], ADR-1892). That bound
//! holds for `ALWAYS_ADMIT` types too (they skip this guard, spec 70 § 6.6), a shed counts as a breach,
//! and two seconds of it in a row disconnect the peer with `Flood`.

use crate::model::NetTick;
use crate::settings::{MessageSettings, RateLimit};

/// Window over which the rate-limited share is measured, in ticks.
pub const FLOOD_WINDOW_TICKS: usize = 60;
/// Seconds of inbound queue overflow in a row after which the peer is disconnected.
pub const OVERFLOW_DISCONNECT_SECONDS: u32 = 2;

/// A peer's flood state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloodState {
    /// Normal limits.
    #[default]
    Clear,
    /// Every per-type rate halved after a breach.
    Slowed,
}

impl FloodState {
    /// Lowercase name for the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Slowed => "slowed",
        }
    }
}

/// What the guard decided for one inbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Deliver (validation still runs).
    Admitted,
    /// No token in the type's bucket: dropped, counted `rate_limited`.
    RateLimited,
    /// Over the per-peer cap this tick or this second: dropped, counted `rate_limited`.
    PeerCapped,
}

/// What a tick's end changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloodOutcome {
    /// Nothing changed.
    Steady,
    /// The peer became `Slowed`.
    BecameSlowed,
    /// The peer became `Clear` again.
    BecameClear,
    /// Disconnect the peer with `Flood`.
    Disconnect,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    units: u64,
    initialized: bool,
}

/// The per-peer flood guard.
#[derive(Debug, Clone)]
pub struct FloodGuard {
    buckets: Vec<Bucket>,
    state: FloodState,
    tick_counted: u32,
    tick_limited: u32,
    tick_capped: bool,
    second_bytes: u64,
    second_ticks: u32,
    ticks_per_second: u32,
    window: [(u32, u32); FLOOD_WINDOW_TICKS],
    window_pos: usize,
    quiet_ticks: u32,
    slowed_ticks: u32,
    forgive: bool,
    breach_ticks: Vec<u32>,
    last_breach: Option<NetTick>,
    /// Frames were shed since the last tick began.
    overflowed: bool,
    overflow_ticks: u32,
    shed: u64,
}

impl FloodGuard {
    /// A guard for a peer, with `ticks_per_second` used for the per-second byte cap.
    pub fn new(ticks_per_second: u32) -> Self {
        Self {
            buckets: Vec::new(),
            state: FloodState::Clear,
            tick_counted: 0,
            tick_limited: 0,
            tick_capped: false,
            second_bytes: 0,
            second_ticks: 0,
            ticks_per_second: ticks_per_second.max(1),
            window: [(0, 0); FLOOD_WINDOW_TICKS],
            window_pos: 0,
            quiet_ticks: 0,
            slowed_ticks: 0,
            forgive: false,
            breach_ticks: Vec::new(),
            last_breach: None,
            overflowed: false,
            overflow_ticks: 0,
            shed: 0,
        }
    }

    /// Sets the tick rate the per-second byte cap counts with (the fixed rate can change).
    pub fn set_ticks_per_second(&mut self, ticks_per_second: u32) {
        self.ticks_per_second = ticks_per_second.max(1);
    }

    /// The tick rate the per-second byte cap counts with.
    pub fn ticks_per_second(&self) -> u32 {
        self.ticks_per_second
    }

    /// The runtime shed `messages` inbound items of this peer before they were stored: a breach at
    /// the next tick.
    pub fn note_overflow(&mut self, messages: usize) {
        self.overflowed = true;
        self.shed = self.shed.saturating_add(messages as u64);
    }

    /// Inbound items shed for this peer so far.
    pub fn shed(&self) -> u64 {
        self.shed
    }

    /// The current state.
    pub fn state(&self) -> FloodState {
        self.state
    }

    /// The tick of the last breach.
    pub fn last_breach_tick(&self) -> Option<NetTick> {
        self.last_breach
    }

    /// Breaches within the last second of ticks before `now`.
    pub fn breaches_last_second(&self, now: NetTick) -> u32 {
        let from = now.0.saturating_sub(self.ticks_per_second);
        self.breach_ticks.iter().filter(|t| **t > from).count() as u32
    }

    /// Starts a tick. `forgive` is true when the previous server frame stalled past
    /// `stall_forgive_ms`: this tick's messages do not count toward escalation.
    pub fn begin_tick(&mut self, forgive: bool) {
        self.forgive = forgive;
        self.tick_counted = 0;
        self.tick_limited = 0;
        self.tick_capped = std::mem::take(&mut self.overflowed);
        if !self.tick_capped {
            self.overflow_ticks = 0;
        } else if !forgive {
            self.overflow_ticks += 1;
        }
        self.second_ticks += 1;
        if self.second_ticks > self.ticks_per_second {
            self.second_ticks = 1;
            self.second_bytes = 0;
        }
    }

    fn effective(&self, rate: RateLimit) -> (u64, u64) {
        let per = u64::from(rate.per_ticks.max(1));
        let count = u64::from(rate.count);
        // Slowed halves the rate exactly: an even count halves, an odd count keeps its count over
        // twice the ticks. A zero rate stays zero (a floor of 1 turned it into one message per
        // tick). Integer `count / 2` turned a count of 1 into 0: a peer sending its type's
        // legal one per tick was then 100 % limited, breached every tick and was disconnected
        // after `disconnect_after_slowed_ticks`, where an even count leaves it exactly half
        // limited, which is no breach (corr-net, ADR-5580).
        if self.state == FloodState::Slowed {
            if count.is_multiple_of(2) {
                (count / 2, per)
            } else {
                (count, per * 2)
            }
        } else {
            (count, per)
        }
    }

    /// Refills every bucket by one tick (call once per tick, before admitting).
    pub fn refill(&mut self, rates: &[RateLimit]) {
        if self.buckets.len() < rates.len() {
            self.buckets.resize(
                rates.len(),
                Bucket {
                    units: 0,
                    initialized: false,
                },
            );
        }
        for (i, rate) in rates.iter().enumerate() {
            let (count, per) = self.effective(*rate);
            let cap = count * per;
            let Some(b) = self.buckets.get_mut(i) else {
                continue;
            };
            if !b.initialized {
                b.units = cap;
                b.initialized = true;
            } else {
                b.units = (b.units + count).min(cap);
            }
        }
    }

    /// Admits or drops one inbound message of type `type_index`.
    pub fn admit(
        &mut self,
        type_index: usize,
        rate: RateLimit,
        always_admit: bool,
        bytes: usize,
        settings: &MessageSettings,
    ) -> Admit {
        if always_admit {
            return Admit::Admitted;
        }
        self.tick_counted += 1;
        self.second_bytes += bytes as u64;
        if self.tick_counted > settings.peer_cap_per_tick
            || self.second_bytes > u64::from(settings.peer_bytes_per_second)
        {
            self.tick_capped = true;
            self.tick_limited += 1;
            return Admit::PeerCapped;
        }
        let (count, per) = self.effective(rate);
        if self.buckets.len() <= type_index {
            self.buckets.resize(
                type_index + 1,
                Bucket {
                    units: 0,
                    initialized: false,
                },
            );
        }
        let Some(b) = self.buckets.get_mut(type_index) else {
            return Admit::RateLimited;
        };
        if !b.initialized {
            b.units = count * per;
            b.initialized = true;
        }
        if b.units >= per {
            b.units -= per;
            Admit::Admitted
        } else {
            self.tick_limited += 1;
            Admit::RateLimited
        }
    }

    /// Ends a tick: records the window and decides escalation.
    pub fn end_tick(&mut self, tick: NetTick, settings: &MessageSettings) -> FloodOutcome {
        if self.forgive {
            self.forgive = false;
            // A stalled frame's burst is not the peer's fault: record nothing for escalation.
            self.window[self.window_pos] = (0, 0);
            self.window_pos = (self.window_pos + 1) % FLOOD_WINDOW_TICKS;
            return self.quiet_tick(settings);
        }
        self.window[self.window_pos] = (self.tick_counted, self.tick_limited);
        self.window_pos = (self.window_pos + 1) % FLOOD_WINDOW_TICKS;
        let (total, limited) = self.window.iter().fold((0u64, 0u64), |(t, l), (c, x)| {
            (t + u64::from(*c), l + u64::from(*x))
        });
        let majority_limited = total > 0 && limited * 2 > total && self.tick_limited > 0;
        let breach = self.tick_capped || majority_limited;
        if !breach {
            return self.quiet_tick(settings);
        }
        self.last_breach = Some(tick);
        self.breach_ticks.push(tick.0);
        let keep_from = tick.0.saturating_sub(self.ticks_per_second);
        self.breach_ticks.retain(|t| *t > keep_from);
        self.quiet_ticks = 0;
        if self.overflow_ticks >= self.ticks_per_second.saturating_mul(OVERFLOW_DISCONNECT_SECONDS) {
            return FloodOutcome::Disconnect;
        }
        match self.state {
            FloodState::Clear => {
                self.state = FloodState::Slowed;
                self.slowed_ticks = 0;
                FloodOutcome::BecameSlowed
            }
            FloodState::Slowed => {
                self.slowed_ticks += 1;
                if self.slowed_ticks >= settings.disconnect_after_slowed_ticks {
                    FloodOutcome::Disconnect
                } else {
                    FloodOutcome::Steady
                }
            }
        }
    }

    /// A tick without a breach. While `Slowed` it counts toward recovery only: the disconnect
    /// counts breaching ticks (audit net3-correctness #3).
    fn quiet_tick(&mut self, settings: &MessageSettings) -> FloodOutcome {
        if self.state == FloodState::Slowed {
            // Recovery counts quiet ticks only. `slowed_ticks` is breaches while `Slowed`
            // (spec 70 § 6.6); a quiet tick must not move the peer toward `Flood`.
            self.quiet_ticks += 1;
            if self.quiet_ticks >= settings.slowed_recover_ticks {
                self.state = FloodState::Clear;
                self.quiet_ticks = 0;
                self.slowed_ticks = 0;
                return FloodOutcome::BecameClear;
            }
        }
        FloodOutcome::Steady
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_tick(
        g: &mut FloodGuard,
        tick: u32,
        sends: u32,
        rate: RateLimit,
        always: bool,
        forgive: bool,
        s: &MessageSettings,
    ) -> (u32, FloodOutcome) {
        g.begin_tick(forgive);
        g.refill(&[rate]);
        let mut admitted = 0;
        for _ in 0..sends {
            if g.admit(0, rate, always, 16, s) == Admit::Admitted {
                admitted += 1;
            }
        }
        (admitted, g.end_tick(NetTick(tick), s))
    }

    /// Spec 70 § 10 test 12.
    #[test]
    fn three_times_the_rate_slows_recovers_and_sustained_flood_disconnects() {
        let s = MessageSettings::default();
        let rate = s.default_rate; // 2 per tick
        let mut g = FloodGuard::new(60);
        let mut slowed_at = None;
        let mut dropped = 0;
        for t in 0..400 {
            let (admitted, out) = run_tick(&mut g, t, 6, rate, false, false, &s);
            dropped += 6 - admitted;
            if out == FloodOutcome::BecameSlowed {
                slowed_at = Some(t);
                break;
            }
        }
        assert!(dropped > 0, "3x the rate drops messages");
        let slowed_at = slowed_at.expect("becomes slowed");
        assert_eq!(g.state(), FloodState::Slowed);
        // Halved: at most one per tick gets through once the bucket is drained.
        let mut t = slowed_at + 1;
        for _ in 0..299 {
            assert_eq!(
                run_tick(&mut g, t, 0, rate, false, false, &s).1,
                FloodOutcome::Steady
            );
            t += 1;
        }
        assert_eq!(
            run_tick(&mut g, t, 0, rate, false, false, &s).1,
            FloodOutcome::BecameClear
        );
        t += 1;
        // Sustained flood: slowed again, then disconnected after 600 breaching ticks.
        let mut outcome = FloodOutcome::Steady;
        let mut ticks = 0;
        while outcome != FloodOutcome::Disconnect && ticks < 2000 {
            outcome = run_tick(&mut g, t, 6, rate, false, false, &s).1;
            t += 1;
            ticks += 1;
        }
        assert_eq!(outcome, FloodOutcome::Disconnect);
        assert!(g.breaches_last_second(NetTick(t)) > 0);
    }

    #[test]
    fn always_admit_is_never_blocked_and_never_escalates() {
        let s = MessageSettings::default();
        let mut g = FloodGuard::new(60);
        for t in 0..200 {
            let (admitted, out) = run_tick(&mut g, t, 50, s.default_rate, true, false, &s);
            assert_eq!(admitted, 50);
            assert_eq!(out, FloodOutcome::Steady);
        }
        assert_eq!(g.state(), FloodState::Clear);
    }

    /// Audit L3 (zai): the byte window follows the real tick rate, so a limit per second stays one.
    #[test]
    fn the_byte_cap_covers_one_second_at_any_tick_rate() {
        let s = MessageSettings {
            peer_bytes_per_second: 1000,
            ..MessageSettings::default()
        };
        let rate = RateLimit {
            count: 100_000,
            per_ticks: 1,
        };
        for hz in [30u32, 60, 120] {
            let mut g = FloodGuard::new(60);
            g.set_ticks_per_second(hz);
            assert_eq!(g.ticks_per_second(), hz);
            let mut admitted = 0u32;
            // Two seconds of 100 bytes per tick: 1000 bytes get in each second, not 2000 or 500.
            for _ in 0..hz * 2 {
                g.begin_tick(false);
                g.refill(&[rate]);
                if g.admit(0, rate, false, 100, &s) == Admit::Admitted {
                    admitted += 1;
                }
                g.end_tick(NetTick(0), &s);
            }
            assert_eq!(admitted, 20, "{hz} Hz");
        }
    }

    /// Audit H4: a shed frame is a breach, sustained overflow disconnects within two seconds, a
    /// stalled server does not.
    #[test]
    fn inbound_overflow_escalates_fast() {
        let s = MessageSettings::default();
        let mut g = FloodGuard::new(60);
        g.note_overflow(10);
        g.begin_tick(false);
        assert_eq!(g.end_tick(NetTick(0), &s), FloodOutcome::BecameSlowed);
        assert_eq!(g.shed(), 10);
        let mut ticks = 1;
        let outcome = loop {
            g.note_overflow(1);
            g.begin_tick(false);
            let out = g.end_tick(NetTick(ticks), &s);
            ticks += 1;
            if out != FloodOutcome::Steady {
                break out;
            }
            assert!(ticks < 200, "no disconnect");
        };
        assert_eq!(outcome, FloodOutcome::Disconnect);
        assert_eq!(ticks, 60 * OVERFLOW_DISCONNECT_SECONDS);

        let mut g = FloodGuard::new(60);
        for t in 0..400 {
            g.note_overflow(1);
            g.begin_tick(true);
            assert_eq!(g.end_tick(NetTick(t), &s), FloodOutcome::Steady, "forgiven");
        }
        assert_eq!(g.state(), FloodState::Clear);
    }

    /// Audit net3-correctness #3 (grok): only breaching ticks while `Slowed` count toward the
    /// disconnect, not the quiet ticks between them. A peer that breaches once every 300 ticks
    /// (never quiet long enough to recover) stays connected; 600 breaching ticks still disconnect.
    #[test]
    fn quiet_ticks_do_not_count_toward_the_flood_disconnect() {
        let s = MessageSettings::default();
        assert_eq!(
            (s.slowed_recover_ticks, s.disconnect_after_slowed_ticks),
            (300, 600)
        );
        let mut g = FloodGuard::new(60);
        let breach = |g: &mut FloodGuard, t: u32| {
            g.note_overflow(1);
            g.begin_tick(false);
            g.end_tick(NetTick(t), &s)
        };
        let quiet = |g: &mut FloodGuard, t: u32| {
            g.begin_tick(false);
            g.end_tick(NetTick(t), &s)
        };
        let mut t = 0;
        assert_eq!(breach(&mut g, t), FloodOutcome::BecameSlowed);
        // Breach, then one tick short of recovery, many times over.
        for round in 0..8 {
            for _ in 0..(s.slowed_recover_ticks - 1) {
                t += 1;
                assert_eq!(quiet(&mut g, t), FloodOutcome::Steady, "round {round}");
            }
            t += 1;
            assert_eq!(
                breach(&mut g, t),
                FloodOutcome::Steady,
                "round {round}: {} breaching ticks while slowed are not 600",
                round + 1
            );
        }
        assert_eq!(g.state(), FloodState::Slowed);
        // Breaching without pause: the disconnect comes after 600 breaching ticks in total
        // (8 so far). Shed frames past two seconds would disconnect first, so breach by rate.
        let rate = RateLimit {
            count: 1,
            per_ticks: 1,
        };
        let mut breaching = 8;
        let outcome = loop {
            t += 1;
            let (_, out) = run_tick(&mut g, t, 6, rate, false, false, &s);
            breaching += 1;
            if out != FloodOutcome::Steady {
                break out;
            }
            assert!(breaching < 2000, "no disconnect");
        };
        assert_eq!(outcome, FloodOutcome::Disconnect);
        assert_eq!(breaching, s.disconnect_after_slowed_ticks);
    }

    /// corr-net (ADR-5580): a peer that was slowed once and then sends a type limited to one
    /// per tick at exactly that rate is half limited, like an even count, so it recovers and
    /// stays connected. Integer halving made the count 0: every message limited, a breach every
    /// tick, a disconnect after 600 ticks.
    #[test]
    fn corr_net_a_slowed_peer_at_a_one_per_tick_rate_recovers() {
        let s = MessageSettings::default();
        let rate = RateLimit {
            count: 1,
            per_ticks: 1,
        };
        let mut g = FloodGuard::new(60);
        assert_eq!(
            run_tick(&mut g, 0, 600, rate, false, false, &s).1,
            FloodOutcome::BecameSlowed
        );
        let mut admitted = 0;
        for t in 1..=1000u32 {
            let (a, out) = run_tick(&mut g, t, 1, rate, false, false, &s);
            admitted += a;
            assert_ne!(
                out,
                FloodOutcome::Disconnect,
                "tick {t}: disconnected at its legal rate"
            );
        }
        assert_eq!(g.state(), FloodState::Clear, "the peer recovered");
        assert!(
            admitted >= 500,
            "about one in two while slowed, all once clear: {admitted}"
        );
        // A zero rate stays zero while slowed.
        let zero = RateLimit {
            count: 0,
            per_ticks: 1,
        };
        let mut g = FloodGuard::new(60);
        run_tick(&mut g, 0, 600, zero, false, false, &s);
        assert_eq!(g.state(), FloodState::Slowed);
        assert_eq!(run_tick(&mut g, 1, 4, zero, false, false, &s).0, 0);
    }

    #[test]
    fn per_peer_cap_breaches_and_a_stall_forgives_the_burst() {
        let s = MessageSettings::default();
        let rate = RateLimit {
            count: 100_000,
            per_ticks: 1,
        };
        let mut g = FloodGuard::new(60);
        // After a stall, a 600-message burst is dropped past the cap but does not escalate.
        let (admitted, out) = run_tick(&mut g, 0, 600, rate, false, true, &s);
        assert_eq!(admitted, 512);
        assert_eq!(out, FloodOutcome::Steady);
        assert_eq!(g.state(), FloodState::Clear);
        // The same burst in a normal frame is a breach.
        let (_, out) = run_tick(&mut g, 1, 600, rate, false, false, &s);
        assert_eq!(out, FloodOutcome::BecameSlowed);
    }

    /// Quiet ticks while `Slowed` recover the peer. They are not breaches, so two breaches
    /// separated by 299 quiet ticks do not disconnect. Disconnect is the 600th breach while
    /// `Slowed` (the tick that entered `Slowed` does not count).
    #[test]
    fn quiet_ticks_recover_and_the_600th_slowed_breach_disconnects() {
        let s = MessageSettings::default();
        let rate = RateLimit {
            count: 100_000,
            per_ticks: 1,
        };
        let mut g = FloodGuard::new(60);
        assert_eq!(
            run_tick(&mut g, 0, 600, rate, false, false, &s).1,
            FloodOutcome::BecameSlowed
        );
        let mut t = 1u32;
        for _ in 0..2 {
            for _ in 0..299 {
                assert_eq!(
                    run_tick(&mut g, t, 0, rate, false, false, &s).1,
                    FloodOutcome::Steady
                );
                t += 1;
            }
            let out = run_tick(&mut g, t, 600, rate, false, false, &s).1;
            assert_eq!(out, FloodOutcome::Steady, "a breach while slowed is not yet 600");
            t += 1;
        }
        assert_eq!(g.state(), FloodState::Slowed);
        // The two breaches above count. The tick that raises the count to 600 disconnects.
        let mut while_slowed = 2u32;
        while while_slowed + 1 < s.disconnect_after_slowed_ticks {
            let outcome = run_tick(&mut g, t, 600, rate, false, false, &s).1;
            assert_ne!(outcome, FloodOutcome::Disconnect);
            t += 1;
            while_slowed += 1;
        }
        assert_eq!(while_slowed + 1, s.disconnect_after_slowed_ticks);
        assert_eq!(
            run_tick(&mut g, t, 600, rate, false, false, &s).1,
            FloodOutcome::Disconnect
        );
    }

    /// Halving a rate never raises it: a zero-count type admits nothing while `Clear` and still
    /// nothing once an over-rate flood has moved the peer to `Slowed`.
    #[test]
    fn slowing_a_peer_does_not_raise_a_zero_rate() {
        let s = MessageSettings::default();
        let zero = RateLimit {
            count: 0,
            per_ticks: 1,
        };
        let rate = RateLimit {
            count: 2,
            per_ticks: 1,
        };
        let mut clear = FloodGuard::new(60);
        clear.begin_tick(false);
        clear.refill(&[zero]);
        assert_eq!(clear.admit(0, zero, false, 16, &s), Admit::RateLimited);
        let mut g = FloodGuard::new(60);
        let mut slowed = false;
        for t in 0..400 {
            let (_, out) = run_tick(&mut g, t, 6, rate, false, false, &s);
            if out == FloodOutcome::BecameSlowed {
                slowed = true;
                break;
            }
        }
        assert!(slowed, "an over-rate flood still slows the peer");
        assert_eq!(g.state(), FloodState::Slowed);
        g.begin_tick(false);
        g.refill(&[zero]);
        assert_eq!(
            g.admit(0, zero, false, 16, &s),
            Admit::RateLimited,
            "slowed does not turn a zero rate into one"
        );
    }

    /// Spec 70 § 11 memory: a peer's guard stays under 2 KiB plus 32 bytes per registered message
    /// type, counting its heap (bucket and breach lists at their capacity), through a sustained flood
    /// on every type at the usual tick rates.
    #[test]
    fn a_guard_stays_under_two_kib_plus_32_bytes_per_type() {
        let s = MessageSettings::default();
        // 3x the default rate on every type (spec 70 § 10 test 12): a majority is rate limited.
        let types = 64usize;
        let rates = vec![s.default_rate; types];
        let bytes = |g: &FloodGuard| {
            std::mem::size_of::<FloodGuard>()
                + g.buckets.capacity() * std::mem::size_of::<Bucket>()
                + g.breach_ticks.capacity() * std::mem::size_of::<u32>()
        };
        assert!(
            std::mem::size_of::<Bucket>() <= 32,
            "a bucket fits the per-type share"
        );
        for hz in [30u32, 60, 128] {
            let mut g = FloodGuard::new(hz);
            let mut most = 0;
            let mut breached = 0;
            for t in 0..2000u32 {
                g.begin_tick(false);
                g.refill(&rates);
                for ty in 0..types {
                    for _ in 0..6 {
                        let _ = g.admit(ty, s.default_rate, false, 16, &s);
                    }
                }
                if g.end_tick(NetTick(t), &s) != FloodOutcome::Steady || g.state() == FloodState::Slowed {
                    breached += 1;
                }
                most = most.max(bytes(&g));
            }
            assert!(
                breached > 1000,
                "{hz} Hz: the flood keeps breaching ({breached} ticks)"
            );
            assert!(
                g.breaches_last_second(NetTick(1999)) > 0,
                "{hz} Hz: the breach list is in use"
            );
            let budget = 2048 + 32 * types;
            assert!(most <= budget, "{hz} Hz: {most} bytes > {budget}");
        }
    }
}
