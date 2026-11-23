// SPDX-License-Identifier: MPL-2.0
//! Playlists (specs/50 § 2.6): ordered entries with `loop_list` and a seeded shuffle that never repeats
//! an entry before every entry has played. Entries that fail to open are skipped by the caller.

use regia_core::math::Rng;

/// Most entries in a list.
pub const MAX_ENTRIES: usize = 4096;

/// Walks a list's entries in play order.
#[derive(Debug, Clone)]
pub struct ListCursor {
    len: usize,
    loop_list: bool,
    rng: Option<Rng>,
    order: Vec<usize>,
    pos: usize,
    failed_in_row: usize,
}

impl ListCursor {
    /// A cursor over `len` entries (capped at [`MAX_ENTRIES`]).
    pub fn new(len: usize, loop_list: bool, shuffle: Option<u64>) -> Self {
        let len = len.min(MAX_ENTRIES);
        let mut cursor = Self {
            len,
            loop_list,
            rng: shuffle.map(Rng::new),
            order: Vec::new(),
            pos: 0,
            failed_in_row: 0,
        };
        cursor.deal();
        cursor
    }

    fn deal(&mut self) {
        self.order = (0..self.len).collect();
        if let Some(rng) = &mut self.rng {
            rng.shuffle(&mut self.order);
        }
        self.pos = 0;
    }

    /// The next entry to open, or `None` when the list ended (`ListEnded`).
    pub fn next_entry(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        if self.pos >= self.order.len() {
            if !self.loop_list {
                return None;
            }
            self.deal();
        }
        let e = self.order[self.pos];
        self.pos += 1;
        Some(e)
    }

    /// Records that the entry just returned opened.
    pub fn opened(&mut self) {
        self.failed_in_row = 0;
    }

    /// Records that the entry just returned failed; `true` when every entry has failed in a row (the
    /// list is `Failed`).
    pub fn failed(&mut self) -> bool {
        self.failed_in_row += 1;
        self.failed_in_row >= self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(c: &mut ListCursor, n: usize) -> Vec<usize> {
        (0..n).filter_map(|_| c.next_entry()).collect()
    }

    #[test]
    fn order_and_end() {
        let mut c = ListCursor::new(3, false, None);
        assert_eq!(drain(&mut c, 5), vec![0, 1, 2]);
    }

    #[test]
    fn loop_list_returns_to_zero() {
        let mut c = ListCursor::new(2, true, None);
        assert_eq!(drain(&mut c, 5), vec![0, 1, 0, 1, 0]);
    }

    #[test]
    fn shuffle_is_seeded_and_never_repeats_before_exhaustion() {
        let a = drain(&mut ListCursor::new(8, true, Some(42)), 16);
        let b = drain(&mut ListCursor::new(8, true, Some(42)), 16);
        assert_eq!(a, b);
        for round in a.chunks(8) {
            let mut sorted = round.to_vec();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..8).collect::<Vec<_>>());
        }
        assert_ne!(a[..8], a[8..], "a new deal per round");
    }

    #[test]
    fn all_failing_is_detected() {
        let mut c = ListCursor::new(2, true, None);
        c.next_entry();
        assert!(!c.failed());
        c.next_entry();
        assert!(c.failed());
    }
}
