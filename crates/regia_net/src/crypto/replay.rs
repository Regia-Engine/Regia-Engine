// SPDX-License-Identifier: MPL-2.0
//! Anti-replay window over the 64-bit record counter.
//!
//! A bitmap ring as in RFC 6479 (the sliding window of RFC 4303 § 3.4.3 without shifting the whole
//! bitmap): the counter's high bits pick a word and its low bits a bit, and moving forward clears
//! only the words it passes. The receiver checks before it decrypts, so a replay flood costs no
//! cipher work, and commits only after the tag verified, so a forged counter cannot move the
//! window.

const WORD_BITS: u64 = 64;
const WORDS: usize = 64;
const RING_BITS: u64 = WORDS as u64 * WORD_BITS;

/// Counters behind the newest one that are still accepted. One ring word is partly in use for
/// the newest counters, so the usable span is one word short of the ring (RFC 6479 § 2).
pub const REPLAY_WINDOW_COUNTERS: u64 = RING_BITS - WORD_BITS;

/// Which record counters of one direction were already accepted.
#[derive(Debug, Clone)]
pub struct ReplayWindow {
    newest: Option<u64>,
    words: [u64; WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            newest: None,
            words: [0; WORDS],
        }
    }
}

fn slot(counter: u64) -> (usize, u64) {
    (
        ((counter / WORD_BITS) % WORDS as u64) as usize,
        1 << (counter % WORD_BITS),
    )
}

impl ReplayWindow {
    /// An empty window.
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest accepted counter.
    pub fn newest(&self) -> Option<u64> {
        self.newest
    }

    /// True when `counter` was not accepted yet and is still inside the window.
    pub fn is_fresh(&self, counter: u64) -> bool {
        let Some(newest) = self.newest else {
            return true;
        };
        if counter > newest {
            return true;
        }
        if newest - counter >= REPLAY_WINDOW_COUNTERS {
            return false;
        }
        let (word, bit) = slot(counter);
        self.words[word] & bit == 0
    }

    /// Records `counter`. Call after the record authenticated, and only when
    /// [`ReplayWindow::is_fresh`] said so.
    pub fn commit(&mut self, counter: u64) {
        let newest = match self.newest {
            None => {
                self.words = [0; WORDS];
                self.newest = Some(counter);
                counter
            }
            Some(newest) if counter > newest => {
                let (from, to) = (newest / WORD_BITS, counter / WORD_BITS);
                for step in 1..=(to - from).min(WORDS as u64) {
                    self.words[((from + step) % WORDS as u64) as usize] = 0;
                }
                self.newest = Some(counter);
                counter
            }
            Some(newest) => newest,
        };
        debug_assert!(newest >= counter);
        let (word, bit) = slot(counter);
        self.words[word] |= bit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(w: &mut ReplayWindow, counter: u64) -> bool {
        let fresh = w.is_fresh(counter);
        if fresh {
            w.commit(counter);
        }
        fresh
    }

    #[test]
    fn each_counter_passes_once_in_any_order() {
        let mut w = ReplayWindow::new();
        for c in [5, 3, 9, 4, 8, 0, 1, 7, 2, 6] {
            assert!(accept(&mut w, c), "{c}");
            assert!(!accept(&mut w, c), "{c} again");
        }
        assert_eq!(w.newest(), Some(9));
    }

    #[test]
    fn the_window_edge_is_exact() {
        let mut w = ReplayWindow::new();
        let top = 100_000;
        assert!(accept(&mut w, top));
        assert!(w.is_fresh(top - (REPLAY_WINDOW_COUNTERS - 1)), "last slot inside");
        assert!(!w.is_fresh(top - REPLAY_WINDOW_COUNTERS), "first slot outside");
        assert!(!w.is_fresh(0), "far behind");
    }

    #[test]
    fn checking_does_not_move_the_window() {
        let mut w = ReplayWindow::new();
        assert!(accept(&mut w, 10));
        assert!(w.is_fresh(u64::MAX - 1), "a forged jump is only checked");
        assert_eq!(w.newest(), Some(10));
        assert!(w.is_fresh(9), "and the old counters stay usable");
    }

    #[test]
    fn jumps_clear_old_bits_and_keep_recent_ones() {
        let mut w = ReplayWindow::new();
        for c in 0..2000 {
            assert!(accept(&mut w, c));
        }
        // Ring-sized jump: nothing of the old bitmap may read as seen for a new counter.
        let far = 2000 + RING_BITS * 3 + 17;
        assert!(accept(&mut w, far));
        for back in 1..REPLAY_WINDOW_COUNTERS {
            assert!(w.is_fresh(far - back), "{back} behind must be unseen");
        }
        // A jump inside the ring keeps what stayed in the window.
        let mut w = ReplayWindow::new();
        assert!(accept(&mut w, 1000));
        assert!(accept(&mut w, 1000 + REPLAY_WINDOW_COUNTERS / 2));
        assert!(!w.is_fresh(1000), "still inside, still seen");
        assert!(w.is_fresh(1001), "still inside, never seen");
    }

    #[test]
    fn matches_a_reference_set_over_a_long_scripted_run() {
        // Every decision against a full set of accepted counters, on a fixed pseudo-random walk.
        let mut w = ReplayWindow::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut newest = 0u64;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200_000 {
            state = state.wrapping_mul(0xD120_2F39_D5F0_C53D).wrapping_add(1);
            let step = (state >> 40) % 6000;
            let counter = if state & 1 == 0 {
                newest + step
            } else {
                newest.saturating_sub(step)
            };
            let in_window = counter >= newest
                || newest
                    .checked_sub(counter)
                    .is_some_and(|age| age < REPLAY_WINDOW_COUNTERS);
            let expect = !seen.contains(&counter) && (seen.is_empty() || in_window);
            assert_eq!(
                accept(&mut w, counter),
                expect,
                "counter {counter} newest {newest}"
            );
            if expect {
                seen.insert(counter);
                newest = newest.max(counter);
            }
        }
    }

    #[test]
    fn matches_a_set_oracle_at_counter_and_word_boundaries() {
        let mut w = ReplayWindow::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut newest = None;
        let mut check = |counter: u64| {
            let in_window = newest.is_none_or(|top: u64| {
                counter >= top
                    || top
                        .checked_sub(counter)
                        .is_some_and(|age| age < REPLAY_WINDOW_COUNTERS)
            });
            let expected = !seen.contains(&counter) && in_window;
            assert_eq!(w.is_fresh(counter), expected, "freshness at {counter}");
            assert_eq!(w.is_fresh(counter), expected, "repeat freshness at {counter}");
            if expected {
                w.commit(counter);
                seen.insert(counter);
                newest = Some(newest.map_or(counter, |top: u64| top.max(counter)));
            }
            assert!(!w.is_fresh(counter), "committed {counter} is seen");
            assert_eq!(w.newest(), newest);
        };

        check(1000);
        let jump = 1000 + RING_BITS * 3 + 17;
        check(jump);
        check(jump - 1);
        check(jump - 2);
        check(jump);
        for word in 0..=WORDS as u64 {
            let boundary = 100_000 + word * WORD_BITS;
            check(boundary - 1);
            check(boundary);
            check(boundary + 1);
        }

        let traces = [
            0,
            63,
            64,
            65,
            64 * 2,
            64 * 63,
            64 * 64,
            64 * 65,
            REPLAY_WINDOW_COUNTERS - 1,
            REPLAY_WINDOW_COUNTERS,
            REPLAY_WINDOW_COUNTERS + 1,
            u64::MAX - 2 * REPLAY_WINDOW_COUNTERS,
            u64::MAX - REPLAY_WINDOW_COUNTERS,
            u64::MAX - 1,
            u64::MAX,
            u64::MAX - 65,
            u64::MAX - REPLAY_WINDOW_COUNTERS + 1,
            u64::MAX - REPLAY_WINDOW_COUNTERS,
            u64::MAX,
        ];
        for counter in traces {
            check(counter);
        }
    }

    #[test]
    fn a_fixed_trace_begins_near_u64_max() {
        let mut w = ReplayWindow::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut newest = None;
        let start = u64::MAX - 2 * REPLAY_WINDOW_COUNTERS;
        for counter in [
            start,
            start + 1,
            start + 64,
            u64::MAX - REPLAY_WINDOW_COUNTERS + 1,
            u64::MAX - 1,
            u64::MAX,
            u64::MAX - 63,
            u64::MAX - REPLAY_WINDOW_COUNTERS,
            u64::MAX,
        ] {
            let in_window = newest.is_none_or(|top: u64| {
                counter >= top
                    || top
                        .checked_sub(counter)
                        .is_some_and(|age| age < REPLAY_WINDOW_COUNTERS)
            });
            let expected = !seen.contains(&counter) && in_window;
            assert_eq!(w.is_fresh(counter), expected, "counter {counter}");
            assert_eq!(w.is_fresh(counter), expected, "repeated check {counter}");
            if expected {
                w.commit(counter);
                seen.insert(counter);
                newest = Some(newest.map_or(counter, |top: u64| top.max(counter)));
            }
            assert_eq!(w.newest(), newest);
        }
    }

    #[test]
    fn window_edges_and_word_boundary_at_u64_max_are_exact() {
        let mut w = ReplayWindow::new();
        let newest = u64::MAX;
        let mut seen = std::collections::BTreeSet::from([newest]);
        w.commit(newest);

        let word_boundary = newest / WORD_BITS * WORD_BITS;
        for counter in [word_boundary - 1, word_boundary, newest - 1, newest] {
            let in_window = counter >= newest
                || newest
                    .checked_sub(counter)
                    .is_some_and(|age| age < REPLAY_WINDOW_COUNTERS);
            let expected = !seen.contains(&counter) && in_window;
            assert_eq!(w.is_fresh(counter), expected, "counter {counter}");
            assert_eq!(w.is_fresh(counter), expected, "repeated counter {counter}");
            if expected {
                w.commit(counter);
                seen.insert(counter);
            }
        }

        for (counter, expected) in [
            (newest - REPLAY_WINDOW_COUNTERS - 1, false),
            (newest - REPLAY_WINDOW_COUNTERS, false),
            (newest - REPLAY_WINDOW_COUNTERS + 1, true),
        ] {
            assert_eq!(w.is_fresh(counter), expected, "window edge at {counter}");
            assert_eq!(w.is_fresh(counter), expected, "repeat at {counter}");
        }
    }
}
