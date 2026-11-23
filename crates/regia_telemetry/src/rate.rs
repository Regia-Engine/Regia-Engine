// SPDX-License-Identifier: MPL-2.0
//! Repeat suppression with a fixed table: the same key within the window passes once, then is
//! counted, and the next pass after the window carries the count ("repeated N×").

use std::time::{Duration, Instant};

const SLOTS: usize = 64;

#[derive(Clone, Copy)]
struct Slot {
    key: u64,
    opened: Option<Instant>,
    suppressed: u32,
}

/// A rate limiter over hashed keys (no allocation; the oldest key is forgotten when full).
pub struct RateLimiter {
    window: Duration,
    slots: [Slot; SLOTS],
    suppressed_total: u64,
}

impl RateLimiter {
    /// A limiter that lets one event per key through every `window`.
    #[must_use]
    pub const fn new(window: Duration) -> Self {
        Self {
            window,
            slots: [Slot {
                key: 0,
                opened: None,
                suppressed: 0,
            }; SLOTS],
            suppressed_total: 0,
        }
    }

    /// `Some(repeats)` when an event with `key` may be emitted now (`repeats` = how many were
    /// suppressed since the last one), `None` when it is suppressed.
    pub fn check(&mut self, key: u64, now: Instant) -> Option<u32> {
        let window = self.window;
        if let Some(slot) = self.slots.iter_mut().find(|s| s.opened.is_some() && s.key == key) {
            let opened = slot.opened.unwrap_or(now);
            if now.saturating_duration_since(opened) < window {
                slot.suppressed = slot.suppressed.saturating_add(1);
                self.suppressed_total = self.suppressed_total.saturating_add(1);
                return None;
            }
            let repeats = slot.suppressed;
            slot.opened = Some(now);
            slot.suppressed = 0;
            return Some(repeats);
        }
        let slot = self
            .slots
            .iter_mut()
            .min_by_key(|s| s.opened)
            .unwrap_or_else(|| unreachable!("SLOTS > 0"));
        *slot = Slot {
            key,
            opened: Some(now),
            suppressed: 0,
        };
        Some(0)
    }

    /// Events suppressed since the limiter was created.
    #[must_use]
    pub fn suppressed_total(&self) -> u64 {
        self.suppressed_total
    }
}

/// FNV-1a over `parts` (stable across runs; keys only, not security).
#[must_use]
pub fn key(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in (part.len() as u64).to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for &b in *part {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// A token bucket: `burst` events at once, then one per `refill`.
pub struct TokenBucket {
    tokens: f32,
    burst: f32,
    per_s: f32,
    last: Option<Instant>,
    /// Events refused since creation.
    pub refused: u64,
}

impl TokenBucket {
    /// A full bucket of `burst` tokens refilling one per `refill`.
    #[must_use]
    pub fn new(burst: u32, refill: Duration) -> Self {
        Self {
            tokens: burst as f32,
            burst: burst as f32,
            per_s: 1.0 / refill.as_secs_f32().max(1e-3),
            last: None,
            refused: 0,
        }
    }

    /// Takes a token if one is left.
    pub fn take(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            let dt = now.saturating_duration_since(last).as_secs_f32();
            self.tokens = (self.tokens + dt * self.per_s).min(self.burst);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.refused = self.refused.saturating_add(1);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct RefRateSlot {
        key: u64,
        opened: Instant,
        suppressed: u32,
    }

    struct RefRateLimiter {
        slots: [Option<RefRateSlot>; SLOTS],
        suppressed_total: u64,
    }

    impl Default for RefRateLimiter {
        fn default() -> Self {
            Self {
                slots: [None; SLOTS],
                suppressed_total: 0,
            }
        }
    }

    impl RefRateLimiter {
        fn check(&mut self, key: u64, now: Instant, window: Duration) -> Option<u32> {
            if let Some(slot) = self.slots.iter_mut().flatten().find(|slot| slot.key == key) {
                if now.saturating_duration_since(slot.opened) < window {
                    slot.suppressed = slot.suppressed.saturating_add(1);
                    self.suppressed_total = self.suppressed_total.saturating_add(1);
                    return None;
                }
                let repeats = slot.suppressed;
                slot.opened = now;
                slot.suppressed = 0;
                return Some(repeats);
            }
            let index = self
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.as_ref().map(|slot| slot.opened))
                .map(|(index, _)| index)
                .expect("reference table has at least one slot");
            self.slots[index] = Some(RefRateSlot {
                key,
                opened: now,
                suppressed: 0,
            });
            Some(0)
        }
    }

    #[derive(Default)]
    struct RefBucket {
        tokens_numerator: u128,
        last: Option<Instant>,
        refused: u64,
        initialized: bool,
    }

    impl RefBucket {
        fn take(&mut self, now: Instant, burst: u32, refill: Duration) -> bool {
            let denominator = refill.as_nanos().max(1_000_000);
            if !self.initialized {
                self.tokens_numerator = u128::from(burst) * denominator;
                self.initialized = true;
            } else if let Some(last) = self.last {
                self.tokens_numerator = (self.tokens_numerator
                    + now.saturating_duration_since(last).as_nanos())
                .min(u128::from(burst) * denominator);
            }
            self.last = Some(now);
            if self.tokens_numerator >= denominator {
                self.tokens_numerator -= denominator;
                true
            } else {
                self.refused = self.refused.saturating_add(1);
                false
            }
        }
    }

    fn after(anchor: Instant, delta: Duration) -> Instant {
        anchor.checked_add(delta).expect("test instant offset fits")
    }

    #[test]
    fn repeats_are_counted_and_reported() {
        let mut r = RateLimiter::new(Duration::from_secs(5));
        let t0 = Instant::now();
        let k = key(&[b"target", b"msg"]);
        assert_eq!(r.check(k, t0), Some(0));
        for i in 1..=10 {
            assert_eq!(r.check(k, t0 + Duration::from_millis(i * 100)), None);
        }
        assert_eq!(r.check(key(&[b"other"]), t0), Some(0));
        assert_eq!(r.check(k, t0 + Duration::from_secs(6)), Some(10));
        assert_eq!(r.suppressed_total(), 10);
        // Many keys never panic or allocate; the oldest is forgotten.
        for i in 0..200u64 {
            let _ = r.check(i, t0 + Duration::from_secs(7));
        }
    }

    #[test]
    fn bucket_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(2, Duration::from_secs(1));
        assert!(b.take(t0) && b.take(t0));
        assert!(!b.take(t0));
        assert!(b.take(t0 + Duration::from_millis(1100)));
        assert_eq!(b.refused, 1);
    }

    #[test]
    fn limiter_matches_bounded_reference_across_capacity_and_timestamp_histories() {
        let start = Instant::now();
        let window = Duration::from_nanos(10);
        let mut actual = RateLimiter::new(window);
        let mut expected = RefRateLimiter::default();

        // Fill all slots at one instant, then insert the 65th key. Equal opening times make the
        // first slot the deterministic victim. Subsequent steps exercise boundary, quiet gap,
        // backward time, repeated suppression and eviction in one bounded history.
        let mut history = Vec::new();
        for key in 1..=65 {
            history.push((key, start));
        }
        history.extend([
            (2, start),
            (2, after(start, Duration::from_nanos(9))),
            (2, after(start, Duration::from_nanos(10))),
            (2, after(start, Duration::from_nanos(10))),
            (2, after(start, Duration::from_nanos(100_000))),
            (
                2,
                start
                    .checked_sub(Duration::from_nanos(1))
                    .expect("backward instant fits"),
            ),
            (65, after(start, Duration::from_nanos(100_001))),
            (66, after(start, Duration::from_nanos(100_002))),
            (3, after(start, Duration::from_nanos(100_003))),
        ]);

        for (key, now) in history {
            assert_eq!(
                actual.check(key, now),
                expected.check(key, now, window),
                "key={key} at {now:?}"
            );
            assert_eq!(actual.suppressed_total(), expected.suppressed_total);
        }
        // Key 1 was the oldest inserted entry and was evicted; asking for it starts a fresh slot.
        assert_eq!(actual.check(1, after(start, Duration::from_secs(1))), Some(0));
        assert_eq!(
            expected.check(1, after(start, Duration::from_secs(1)), window),
            Some(0)
        );
    }

    #[test]
    fn zero_window_admits_each_event_and_equal_instants_suppress_only_with_positive_window() {
        let start = Instant::now();
        let mut zero = RateLimiter::new(Duration::ZERO);
        assert_eq!(zero.check(7, start), Some(0));
        assert_eq!(zero.check(7, start), Some(0));
        assert_eq!(zero.suppressed_total(), 0);

        let mut positive = RateLimiter::new(Duration::from_nanos(1));
        assert_eq!(positive.check(7, start), Some(0));
        assert_eq!(positive.check(7, start), None);
        assert_eq!(positive.suppressed_total(), 1);
    }

    #[test]
    fn rate_key_preserves_part_boundaries() {
        assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
        assert_ne!(key(&[b"a"]), key(&[b"a", b""]));
        assert_ne!(key(&[b"a\xff", b"b"]), key(&[b"a", b"\xffb"]));
        assert_eq!(key(&[b"a", b"b"]), key(&[b"a", b"b"]));
    }

    #[test]
    fn equivalent_rate_event_partitions_report_the_same_repeat_count() {
        let start = Instant::now();
        let window = Duration::from_nanos(10);
        let partitions = [[0, 2, 6, 10], [0, 4, 8, 10]];
        for offsets in partitions {
            let mut limiter = RateLimiter::new(window);
            let outcomes: Vec<_> = offsets
                .into_iter()
                .map(|offset| limiter.check(8, after(start, Duration::from_nanos(offset))))
                .collect();
            assert_eq!(outcomes, [Some(0), None, None, Some(2)]);
            assert_eq!(limiter.suppressed_total(), 2);
        }
    }

    #[test]
    fn suppressed_repeat_counter_saturates_without_wrapping() {
        let start = Instant::now();
        let window = Duration::from_secs(1);
        let mut limiter = RateLimiter::new(window);
        assert_eq!(limiter.check(9, start), Some(0));
        let slot = limiter
            .slots
            .iter_mut()
            .find(|slot| slot.key == 9)
            .expect("key has an occupied slot");
        slot.suppressed = u32::MAX;
        assert_eq!(limiter.check(9, start), None);
        assert_eq!(limiter.check(9, after(start, window)), Some(u32::MAX));
        assert_eq!(limiter.suppressed_total(), 1);
    }

    #[test]
    fn cumulative_counters_saturate_instead_of_wrapping() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(Duration::from_secs(1));
        assert_eq!(limiter.check(10, start), Some(0));
        limiter.suppressed_total = u64::MAX;
        assert_eq!(limiter.check(10, start), None);
        assert_eq!(limiter.suppressed_total(), u64::MAX);

        let mut bucket = TokenBucket::new(0, Duration::from_secs(1));
        bucket.refused = u64::MAX;
        assert!(!bucket.take(start));
        assert_eq!(bucket.refused, u64::MAX);
    }

    #[test]
    fn equivalent_token_bucket_event_partitions_keep_the_same_admission_count() {
        let start = Instant::now();
        let refill = Duration::from_secs(1);
        for offsets in [[0, 400, 1000], [0, 200, 1000]] {
            let mut bucket = TokenBucket::new(1, refill);
            let outcomes: Vec<_> = offsets
                .into_iter()
                .map(|offset| bucket.take(after(start, Duration::from_millis(offset))))
                .collect();
            assert_eq!(outcomes, [true, false, true]);
            assert_eq!(bucket.refused, 1);
        }
    }

    #[test]
    fn token_bucket_matches_integer_reference_for_bursts_and_refill_boundaries() {
        let start = Instant::now();
        for burst in [0, 1, 10_000] {
            let refill = Duration::from_millis(250);
            let mut actual = TokenBucket::new(burst, refill);
            let mut expected = RefBucket::default();
            let mut now = start;
            for _ in 0..burst {
                assert!(actual.take(now));
                assert!(expected.take(now, burst, refill));
            }
            assert!(!actual.take(now));
            assert!(!expected.take(now, burst, refill));
            assert_eq!(actual.refused, expected.refused);
            let steps = [
                Duration::from_millis(249),
                Duration::from_millis(1),
                Duration::from_millis(250),
                Duration::from_secs(60),
                Duration::ZERO,
                Duration::from_millis(251),
            ];
            for elapsed in steps {
                now = after(now, elapsed);
                assert_eq!(
                    actual.take(now),
                    expected.take(now, burst, refill),
                    "burst={burst}, now={now:?}"
                );
                assert_eq!(actual.refused, expected.refused);
            }
            assert_eq!(actual.refused, expected.refused);
        }
    }

    #[test]
    fn token_bucket_backward_instants_do_not_refill() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(1, Duration::from_secs(1));
        assert!(bucket.take(start));
        assert!(!bucket.take(after(start, Duration::from_millis(500))));
        assert!(!bucket.take(after(start, Duration::from_millis(400))));
        assert!(bucket.take(after(start, Duration::from_secs(1))));
        assert_eq!(bucket.refused, 2);
    }

    #[test]
    fn token_bucket_zero_refill_uses_the_documented_minimum_interval() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(1, Duration::ZERO);
        assert!(bucket.take(start));
        assert!(!bucket.take(start));
        assert!(bucket.take(after(start, Duration::from_millis(1))));
        assert_eq!(bucket.refused, 1);
    }
}
