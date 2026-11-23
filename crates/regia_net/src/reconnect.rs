// SPDX-License-Identifier: MPL-2.0
//! Automatic client reconnect after a timeout (spec 120 § 10.3, ADR-8352).
//!
//! A client whose server went silent past the session timeout does not give up: the server holds
//! its slot for the resume grace (dormant, spec 120 § 10.3), so the client starts fresh handshakes
//! with its resume ticket, spaced by an exponential backoff with jitter (1 s doubling to 8 s), for
//! the whole grace window, over the transport kind it connected with (UDP, Steam, QUIC). The first
//! handshake that completes is the same `PeerId`; when the window ends without one the client
//! gives up with one final event.
//!
//! [`Reconnector`] is the schedule only (no socket, no clock, no ECS): `NetLink` and `NetSession`
//! drive it with their own clocks and transports, so both layers share one tested timeline.
//!
//! An attempt is a whole handshake: it keeps resending its own handshake packets at the client's
//! `handshake_resend_ms` until it connects or the next attempt replaces it, so the client is never
//! idle inside the window and a link that comes back is used at the next resend, not the next
//! attempt. The backoff spaces the *fresh* attempts (a new nonce, and for UDP a new socket and so a
//! new NAT mapping), which is the costly part for a server many clients lost at once.

use serde::{Deserialize, Serialize};

/// Default [`ReconnectPolicy::first_ms`]: the first attempt starts 1 s after the timeout.
pub const DEFAULT_RECONNECT_FIRST_MS: u64 = 1_000;
/// Default [`ReconnectPolicy::max_ms`]: attempts are never more than 8 s apart.
pub const DEFAULT_RECONNECT_MAX_MS: u64 = 8_000;
/// Default [`ReconnectPolicy::window_ms`]: 60 s, the server's default resume grace.
pub const DEFAULT_RECONNECT_WINDOW_MS: u64 = 60_000;

/// How a client reconnects after a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectPolicy {
    /// Reconnect by itself after a timeout. Off: a timeout ends the client, as before.
    pub enabled: bool,
    /// Delay of the first attempt after the timeout, milliseconds; each later delay doubles.
    pub first_ms: u64,
    /// Longest delay between two attempts, milliseconds.
    pub max_ms: u64,
    /// How long after the timeout the client keeps trying, milliseconds: the server's resume
    /// grace (it holds the slot that long, so a later attempt could only be a fresh join).
    pub window_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            first_ms: DEFAULT_RECONNECT_FIRST_MS,
            max_ms: DEFAULT_RECONNECT_MAX_MS,
            window_ms: DEFAULT_RECONNECT_WINDOW_MS,
        }
    }
}

impl ReconnectPolicy {
    /// Never reconnect by itself.
    pub fn off() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// The delay before attempt `attempt` (1-based) is started, counted from the previous attempt
    /// (the timeout for the first): `first_ms` doubled per attempt up to `max_ms`, less a jitter of
    /// at most an eighth, drawn from `seed` (so clients that lost the same server spread out, and
    /// a replay draws the same delays). Never 0 and never above `max_ms`.
    pub fn delay_ms(&self, attempt: u32, seed: u64) -> u64 {
        let first = self.first_ms.max(1);
        let max = self.max_ms.max(first);
        let shift = attempt.saturating_sub(1).min(32);
        let base = first.saturating_mul(1u64 << shift).min(max);
        let jitter = mix(seed ^ u64::from(attempt)) % (base / 8 + 1);
        base.saturating_sub(jitter).max(1)
    }
}

/// SplitMix64's output step: a well-spread hash of a counter.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// What the caller does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectStep {
    /// Nothing: the current attempt keeps going (or the next one is not due).
    Wait,
    /// Start attempt `n` (1-based) now, replacing any attempt in flight.
    Attempt(u32),
    /// The window is over: give up (once).
    GiveUp,
}

/// The reconnect timeline of one outage (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconnector {
    policy: ReconnectPolicy,
    seed: u64,
    /// When the timeout ended the connection.
    since_ms: u64,
    /// Attempts started so far.
    attempt: u32,
    /// When the next attempt is due.
    next_at_ms: u64,
    gave_up: bool,
}

impl Reconnector {
    /// Starts the timeline of an outage whose timeout fired at `now_ms`.
    pub fn start(policy: ReconnectPolicy, now_ms: u64, seed: u64) -> Self {
        Self {
            policy,
            seed,
            since_ms: now_ms,
            attempt: 0,
            next_at_ms: now_ms.saturating_add(policy.delay_ms(1, seed)),
            gave_up: false,
        }
    }

    /// Attempts started so far.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// When the timeout fired.
    pub fn since_ms(&self) -> u64 {
        self.since_ms
    }

    /// When the next attempt is due.
    pub fn next_at_ms(&self) -> u64 {
        self.next_at_ms
    }

    /// When the window ends.
    pub fn deadline_ms(&self) -> u64 {
        self.since_ms.saturating_add(self.policy.window_ms)
    }

    /// True once [`ReconnectStep::GiveUp`] was returned.
    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

    /// The step at `now_ms`. An attempt that falls due inside the window starts even when the
    /// caller polls late (one per call, never a burst); at or past the deadline the timeline gives
    /// up exactly once and then waits.
    pub fn poll(&mut self, now_ms: u64) -> ReconnectStep {
        if self.gave_up {
            return ReconnectStep::Wait;
        }
        if now_ms >= self.deadline_ms() {
            self.gave_up = true;
            return ReconnectStep::GiveUp;
        }
        if now_ms < self.next_at_ms {
            return ReconnectStep::Wait;
        }
        self.attempt = self.attempt.saturating_add(1);
        let delay = self.policy.delay_ms(self.attempt.saturating_add(1), self.seed);
        // From now, not from the due time: a late poll must not start two attempts back to back.
        self.next_at_ms = now_ms.saturating_add(delay);
        ReconnectStep::Attempt(self.attempt)
    }

    /// Makes the next attempt due at `now_ms` (the caller knows the old path is dead, so waiting
    /// for the first delay gains nothing). Later delays keep the backoff.
    pub fn hurry(&mut self, now_ms: u64) {
        self.next_at_ms = self.next_at_ms.min(now_ms);
    }

    /// How long the attempt that just started may wait for its handshake: until the window ends
    /// (the next attempt replaces it earlier when one is due).
    pub fn attempt_timeout_ms(&self, now_ms: u64) -> u64 {
        self.deadline_ms().saturating_sub(now_ms).max(1)
    }
}

/// A seed for the jitter from bytes the client holds (its resume ticket): clients of one server
/// draw different delays, the same client the same delays.
pub fn seed_from(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_one_second_to_eight_with_bounded_jitter() {
        let p = ReconnectPolicy::default();
        for seed in 0..200u64 {
            for (attempt, base) in [
                (1u32, 1_000u64),
                (2, 2_000),
                (3, 4_000),
                (4, 8_000),
                (5, 8_000),
                (40, 8_000),
            ] {
                let d = p.delay_ms(attempt, seed);
                assert!(d <= base, "attempt {attempt}: {d} > {base}");
                assert!(d >= base - base / 8, "attempt {attempt}: {d} below 7/8 of {base}");
            }
        }
        // The jitter really varies between clients.
        let firsts: std::collections::BTreeSet<u64> = (0..64).map(|s| p.delay_ms(4, s)).collect();
        assert!(firsts.len() > 16, "{firsts:?}");
    }

    #[test]
    fn the_timeline_tries_for_the_whole_window_then_gives_up_once() {
        let p = ReconnectPolicy::default();
        let mut r = Reconnector::start(p, 20_000, 7);
        let mut starts = Vec::new();
        let mut gave_up = 0;
        for now in (20_000..90_000).step_by(10) {
            match r.poll(now) {
                ReconnectStep::Attempt(n) => starts.push((n, now)),
                ReconnectStep::GiveUp => gave_up += 1,
                ReconnectStep::Wait => {}
            }
        }
        assert_eq!(gave_up, 1);
        assert!(r.gave_up());
        // Numbered 1.. in order, all inside the window.
        for (i, (n, at)) in starts.iter().enumerate() {
            assert_eq!(*n as usize, i + 1);
            assert!(*at < 80_000);
        }
        // 1, 2, 4 s, then every 8 s: about nine attempts in 60 s.
        assert!((8..=11).contains(&starts.len()), "{starts:?}");
        assert!(starts[0].1 >= 20_875 && starts[0].1 <= 21_010, "{starts:?}");
        // The last gap before the deadline is never more than 8 s.
        for w in starts.windows(2) {
            assert!(w[1].1 - w[0].1 <= 8_010, "{starts:?}");
        }
        assert!(80_000 - starts.last().map_or(0, |s| s.1) <= 8_010);
    }

    #[test]
    fn a_late_poll_starts_one_attempt_not_a_burst() {
        let mut r = Reconnector::start(ReconnectPolicy::default(), 0, 1);
        assert_eq!(r.poll(30_000), ReconnectStep::Attempt(1));
        assert_eq!(r.poll(30_001), ReconnectStep::Wait);
        assert!(r.next_at_ms() > 31_000);
    }

    #[test]
    fn hurry_makes_the_next_attempt_due_and_keeps_the_backoff() {
        let p = ReconnectPolicy::default();
        let mut r = Reconnector::start(p, 5_000, 3);
        r.hurry(5_000);
        assert_eq!(r.poll(5_000), ReconnectStep::Attempt(1));
        let gap = r.next_at_ms() - 5_000;
        assert!(
            (1_750..=2_000).contains(&gap),
            "the second delay is still ~2 s: {gap}"
        );
    }

    #[test]
    fn an_off_policy_is_not_enabled() {
        assert!(!ReconnectPolicy::off().enabled);
        assert!(ReconnectPolicy::default().enabled);
    }
}
