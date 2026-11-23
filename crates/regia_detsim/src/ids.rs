// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/entity_ids.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (ADR-8048). Kept: ids `1..=cap`, id 0 reserved, a FIFO free list that is drained before
// fresh ids, `reserve` that never advances the fresh cursor and drops the id from the free list, so a
// host replaying DO27's call sequence gets the same ids. Named issues fixed: (1) the fresh cursor was
// a `u16` with `saturating_add`, so with `cap == u16::MAX` an `alloc` after the range was used up with
// an empty free list looped forever; the cursor is now a `u64`; (2) `live_count` walked the whole
// bitset, it is now a counter. Added: the id width as a type parameter (`u16` or `u32`), the cap as
// the constructor setting (no game default), a per-id generation bumped on free with `DetHandle`, and
// the live bitset grows with the highest id touched instead of being allocated for the whole cap.

//! Deterministic id allocation for lockstep simulations.
//!
//! [`DetIdAllocator`] hands out ids in `1..=cap` (id `0` is never issued). A freed id goes to the
//! back of a FIFO free list and is reused before any fresh id, so the id a spawn gets depends only
//! on the order of earlier `alloc`/`free`/`reserve` calls, never on a hash, a clock or an address.
//! Two peers that make the same calls get the same ids.
//!
//! Every id carries a generation that goes up by one each time the id is freed. A
//! [`DetHandle`] (id plus generation) taken at spawn time stops matching once the entity is
//! removed, even after the id is handed to a new entity, so a stale reference is caught instead of
//! silently pointing at the newcomer.
//!
//! ```
//! use regia_detsim::ids::DetIdAllocator;
//!
//! let mut ids = DetIdAllocator::<u16>::new(64); // the cap is the host's setting
//! let a = ids.alloc_handle().unwrap();
//! let b = ids.alloc().unwrap();
//! ids.free(a.id);
//! let c = ids.alloc_handle().unwrap();
//! assert_eq!(c.id, a.id); // FIFO reuse
//! assert!(!ids.is_current(a)); // the old handle is stale
//! assert!(ids.is_current(c));
//! assert!(ids.is_live(b));
//! ```

use std::collections::VecDeque;

use crate::state::{DetReader, DetState, DetStateError, DetWriter};

mod sealed {
    pub trait Sealed {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
}

/// An id width [`DetIdAllocator`] can issue: `u16` (a 16-bit wire id) or `u32`.
///
/// Sealed: only `u16` and `u32` implement it.
pub trait DetId: sealed::Sealed + Copy + Eq + Ord + core::hash::Hash + core::fmt::Debug {
    /// The largest id this width can carry.
    const MAX_ID: u32;
    /// The id 1 (the smallest cap).
    const ONE: Self;
    /// The [`crate::state::DetState::STATE_TAG`] of a [`DetIdAllocator`] of this width.
    const STATE_TAG: [u8; 8];
    /// The id as a `u32` (lossless).
    fn to_u32(self) -> u32;
    /// The id from a `u32`; `None` when it does not fit this width.
    fn from_u32(value: u32) -> Option<Self>;
}

impl DetId for u16 {
    const MAX_ID: u32 = u16::MAX as u32;
    const ONE: Self = 1;
    const STATE_TAG: [u8; 8] = *b"detids16";
    fn to_u32(self) -> u32 {
        u32::from(self)
    }
    fn from_u32(value: u32) -> Option<Self> {
        u16::try_from(value).ok()
    }
}

impl DetId for u32 {
    const MAX_ID: u32 = u32::MAX;
    const ONE: Self = 1;
    const STATE_TAG: [u8; 8] = *b"detids32";
    fn to_u32(self) -> u32 {
        self
    }
    fn from_u32(value: u32) -> Option<Self> {
        Some(value)
    }
}

/// An id together with the generation it had when it was issued.
///
/// Compare it with [`DetIdAllocator::is_current`]: it stops matching when the id is freed, and
/// stays stale after the id is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DetHandle<I: DetId> {
    /// The id (`1..=cap`).
    pub id: I,
    /// How many times the id had been freed before this issue (wraps at `u32::MAX`).
    pub generation: u32,
}

/// Allocates and recycles ids in `1..=cap` with FIFO reuse and per-id generations.
///
/// See the [module docs](self). All operations are deterministic. Memory is O(highest id touched),
/// not O(live ids): the live and generation tables are dense through the highest id ever issued or
/// reserved, and freeing ids does not shrink them. A large cap alone allocates no tables, but
/// reserving a distant `u32` id can exhaust memory. Hosts should bound imported ids to their budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetIdAllocator<I: DetId = u32> {
    cap: u32,
    cap_id: I,
    /// Next never-issued id. Advances until it passes `cap`; after that only the free list supplies
    /// ids. A `u64`, so it can pass `u32::MAX` (DO27 kept it in the id width and stalled there).
    next_fresh: u64,
    free: VecDeque<I>,
    /// `live[id]`: the id is issued or reserved. Grows on demand (index 0 is the reserved id 0).
    live: Vec<bool>,
    /// `generation[id]`: times the id has been freed. Same length as `live`.
    generation: Vec<u32>,
    live_count: usize,
}

impl<I: DetId> DetIdAllocator<I> {
    /// An allocator for ids `1..=cap`. The cap is the host's setting (a lobby's entity budget); it
    /// is clamped to at least 1. With `u16` the cap is at most `u16::MAX` by type.
    pub fn new(cap: I) -> Self {
        let cap_id = if cap.to_u32() == 0 { I::ONE } else { cap };
        Self {
            cap: cap_id.to_u32(),
            cap_id,
            next_fresh: 1,
            free: VecDeque::new(),
            live: vec![false],
            generation: vec![0],
            live_count: 0,
        }
    }

    /// The largest id this allocator issues.
    pub fn cap(&self) -> I {
        self.cap_id
    }

    /// Ids currently issued or reserved.
    pub fn live_count(&self) -> usize {
        self.live_count
    }

    /// Freed ids waiting for reuse.
    pub fn free_count(&self) -> usize {
        self.free.len()
    }

    /// Issue an id: the oldest freed id, else the lowest never-issued id that is not reserved.
    /// `None` when every id in `1..=cap` is live.
    pub fn alloc(&mut self) -> Option<I> {
        while let Some(id) = self.free.pop_front() {
            let slot = id.to_u32() as usize;
            if self.live[slot] {
                // Stale free-list entry; skip (kept from the source, unreachable through this API).
                continue;
            }
            self.set_live(slot, true);
            return Some(id);
        }
        // Walk fresh ids, skipping holes already claimed by `reserve`. `reserve` never advances the
        // cursor: sparse reserved high ids must leave the low ids available.
        while self.next_fresh <= u64::from(self.cap) {
            let raw = self.next_fresh as u32;
            self.next_fresh += 1;
            let slot = raw as usize;
            self.grow_to(slot);
            if self.live[slot] {
                continue;
            }
            self.set_live(slot, true);
            return I::from_u32(raw);
        }
        None
    }

    /// [`Self::alloc`], returning the id with its current generation.
    pub fn alloc_handle(&mut self) -> Option<DetHandle<I>> {
        let id = self.alloc()?;
        Some(DetHandle {
            id,
            generation: self.generation(id),
        })
    }

    /// Mark `id` free for reuse and bump its generation. No-op if the id is not live (0, past the
    /// cap, never issued, or already free).
    ///
    /// Call it only after the entity is gone from the sim, so it is not in the next snapshot.
    pub fn free(&mut self, id: I) {
        let raw = id.to_u32();
        if !self.is_live(id) {
            return;
        }
        let slot = raw as usize;
        self.set_live(slot, false);
        self.generation[slot] = self.generation[slot].wrapping_add(1);
        self.free.push_back(id);
    }

    /// Free the handle's id only if the handle is current. Returns whether it freed.
    pub fn free_handle(&mut self, handle: DetHandle<I>) -> bool {
        if !self.is_current(handle) {
            return false;
        }
        self.free(handle.id);
        true
    }

    /// True when `id` is issued or reserved.
    pub fn is_live(&self, id: I) -> bool {
        let raw = id.to_u32();
        if raw == 0 || raw > self.cap {
            return false;
        }
        self.live.get(raw as usize).copied().unwrap_or(false)
    }

    /// True when the handle's id is live and has not been freed since the handle was taken.
    pub fn is_current(&self, handle: DetHandle<I>) -> bool {
        self.is_live(handle.id) && self.generation(handle.id) == handle.generation
    }

    /// How many times `id` has been freed (0 for an id never freed or out of range).
    pub fn generation(&self, id: I) -> u32 {
        self.generation.get(id.to_u32() as usize).copied().unwrap_or(0)
    }

    /// The live handle for `id`, or `None` when the id is not live.
    pub fn handle(&self, id: I) -> Option<DetHandle<I>> {
        self.is_live(id).then(|| DetHandle {
            id,
            generation: self.generation(id),
        })
    }

    /// Claim a specific id (a seat's fixed slot, a level-placed entity). Returns `false` if it is 0,
    /// past the cap or already live. Never advances the fresh cursor; drops the id from the free
    /// list if it was there.
    pub fn reserve(&mut self, id: I) -> bool {
        let raw = id.to_u32();
        if raw == 0 || raw > self.cap || self.is_live(id) {
            return false;
        }
        let slot = raw as usize;
        self.grow_to(slot);
        self.set_live(slot, true);
        self.free.retain(|&free_id| free_id != id);
        true
    }

    /// Live ids in ascending order (a deterministic walk for hashing or snapshots).
    pub fn live_ids(&self) -> impl Iterator<Item = I> + '_ {
        let cap = self.cap as usize;
        self.live
            .iter()
            .enumerate()
            .skip(1)
            .take(cap)
            .filter(|(_, live)| **live)
            .filter_map(|(slot, _)| I::from_u32(slot as u32))
    }

    fn grow_to(&mut self, slot: usize) {
        if self.live.len() <= slot {
            self.live.resize(slot + 1, false);
            self.generation.resize(slot + 1, 0);
        }
    }

    fn set_live(&mut self, slot: usize, live: bool) {
        if self.live[slot] != live {
            self.live[slot] = live;
            if live {
                self.live_count += 1;
            } else {
                self.live_count -= 1;
            }
        }
    }
}

/// Saves the whole allocator: the cap, the fresh cursor, the free list in FIFO order, the live bits
/// and every generation (DC-3.7a). The live count is recomputed on load. `load` refuses a cap of 0 or
/// past the width, a fresh cursor outside `1..=cap + 1`, bit and generation tables of different
/// lengths or longer than `cap + 1`, a live id 0, and a free list entry that is 0, past the tables,
/// live, or listed twice: the invariants `alloc` and `free` index by.
impl<I: DetId> DetState for DetIdAllocator<I> {
    const STATE_TAG: [u8; 8] = I::STATE_TAG;
    const STATE_VERSION: u32 = 1;

    fn save(&self, w: &mut DetWriter) {
        w.u32(self.cap);
        w.u64(self.next_fresh);
        // The free list in FIFO order (a deque: one length over both of its halves).
        w.length(self.free.len());
        for id in &self.free {
            w.u32(id.to_u32());
        }
        w.bits(&self.live);
        w.seq(&self.generation, |w, g| w.u32(*g));
    }

    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        let cap = r.u32()?;
        let cap_id = match I::from_u32(cap) {
            Some(id) if cap != 0 => id,
            _ => return Err(DetStateError::bad_value(format_args!("id cap {cap}"))),
        };
        let next_fresh = r.u64()?;
        if next_fresh == 0 || next_fresh > u64::from(cap) + 1 {
            return Err(DetStateError::bad_value(format_args!(
                "fresh cursor {next_fresh} with cap {cap}"
            )));
        }
        let free_raw = r.seq(|r| r.u32())?;
        let live = r.bits()?;
        let generation = r.seq(|r| r.u32())?;
        if live.is_empty() || live.len() != generation.len() || live.len() - 1 > cap as usize {
            return Err(DetStateError::bad_value(format_args!(
                "id tables of {} live bits and {} generations with cap {cap}",
                live.len(),
                generation.len()
            )));
        }
        if live[0] {
            return Err(DetStateError::bad_value("id 0 is live"));
        }
        let mut listed = vec![false; live.len()];
        let mut free = VecDeque::with_capacity(free_raw.len());
        for raw in free_raw {
            let slot = raw as usize;
            if raw == 0 || slot >= live.len() || live[slot] || listed[slot] {
                return Err(DetStateError::bad_value(format_args!("free list entry {raw}")));
            }
            listed[slot] = true;
            let id =
                I::from_u32(raw).ok_or_else(|| DetStateError::bad_value(format_args!("free id {raw}")))?;
            free.push_back(id);
        }
        let live_count = live.iter().filter(|l| **l).count();
        Ok(DetIdAllocator {
            cap,
            cap_id,
            next_fresh,
            free,
            live,
            generation,
            live_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // DO27's three tests (ir-sim/src/entity_ids.rs lines 125-180), on a `u16` allocator (DO27
    // stored `u16` and returned `u32`); one `unwrap` became an `expect` for the workspace lints.

    #[test]
    fn spawn_remove_spawn_returns_freed_id() {
        let mut alloc = DetIdAllocator::<u16>::new(16);
        let a = alloc.alloc().expect("first");
        let b = alloc.alloc().expect("second");
        assert_ne!(a, b);
        assert!(alloc.is_live(a));
        assert!(alloc.is_live(b));

        alloc.free(a);
        assert!(!alloc.is_live(a));
        assert!(alloc.is_live(b));

        let reused = alloc.alloc().expect("reuse");
        assert_eq!(reused, a, "freed id must be reused before fresh ids");
        assert!(alloc.is_live(reused));
        assert!(alloc.is_live(b));
        assert_ne!(reused, b, "two live entities never share an id");
    }

    #[test]
    fn two_live_entities_never_share_an_id() {
        let mut alloc = DetIdAllocator::<u16>::new(8);
        let mut live = Vec::new();
        for _ in 0..8 {
            let id = alloc.alloc().expect("within capacity");
            assert!(!live.contains(&id), "duplicate live id {id}");
            live.push(id);
        }
        assert!(alloc.alloc().is_none(), "at capacity");
        alloc.free(live[3]);
        let again = alloc.alloc().expect("after free");
        assert_eq!(again, live[3]);
        for (i, &id) in live.iter().enumerate() {
            if i == 3 {
                continue;
            }
            assert_ne!(again, id);
        }
    }

    #[test]
    fn reserve_sparse_peer_slot_does_not_steal_low_fresh_ids() {
        let mut alloc = DetIdAllocator::<u16>::new(2000);
        assert!(alloc.reserve(1001));
        assert!(alloc.reserve(1003));
        let scrap = alloc.alloc().expect("low fresh");
        assert_eq!(scrap, 1, "fresh alloc must fill below reserved peer slots");
        assert!(alloc.alloc().expect("second low fresh") < 1001);
        assert!(alloc.is_live(1001));
        assert!(alloc.is_live(1003));
        assert!(!alloc.reserve(1001), "already live");
    }

    // Regia's tests.

    #[test]
    fn exhausting_a_full_width_u16_cap_returns_none() {
        // DO27's cursor saturated at u16::MAX and looped forever here.
        let mut alloc = DetIdAllocator::<u16>::new(u16::MAX);
        for expected in 1..=u16::MAX {
            assert_eq!(alloc.alloc(), Some(expected));
        }
        assert_eq!(alloc.alloc(), None);
        assert_eq!(alloc.alloc(), None, "a second call after exhaustion also returns");
        alloc.free(77);
        assert_eq!(alloc.alloc(), Some(77));
        assert_eq!(alloc.live_count(), usize::from(u16::MAX));
    }

    #[test]
    fn free_list_is_first_in_first_out() {
        let mut alloc = DetIdAllocator::<u32>::new(100);
        let ids: Vec<u32> = (0..10).filter_map(|_| alloc.alloc()).collect();
        for &id in &[ids[7], ids[2], ids[9], ids[0]] {
            alloc.free(id);
        }
        let reused: Vec<u32> = (0..5).filter_map(|_| alloc.alloc()).collect();
        assert_eq!(reused, vec![ids[7], ids[2], ids[9], ids[0], 11]);
    }

    #[test]
    fn a_freed_id_bumps_its_generation_and_stales_old_handles() {
        let mut alloc = DetIdAllocator::<u16>::new(4);
        let first = alloc.alloc_handle().expect("id");
        assert_eq!(first.generation, 0);
        assert!(alloc.is_current(first));
        assert!(alloc.free_handle(first));
        assert!(!alloc.free_handle(first), "a stale handle frees nothing");
        let second = alloc.alloc_handle().expect("reuse");
        assert_eq!(second.id, first.id);
        assert_eq!(second.generation, 1);
        assert!(!alloc.is_current(first));
        assert!(alloc.is_current(second));
        assert!(
            !alloc.free_handle(first),
            "the stale handle cannot free the newcomer"
        );
        assert!(alloc.is_live(second.id));
        assert_eq!(alloc.handle(second.id), Some(second));
    }

    #[test]
    fn reserve_takes_an_id_off_the_free_list() {
        let mut alloc = DetIdAllocator::<u32>::new(10);
        let a = alloc.alloc().expect("a");
        let b = alloc.alloc().expect("b");
        alloc.free(a);
        alloc.free(b);
        assert!(alloc.reserve(a));
        assert_eq!(alloc.free_count(), 1);
        assert_eq!(alloc.alloc(), Some(b));
        assert_eq!(alloc.alloc(), Some(3));
    }

    #[test]
    fn out_of_range_ids_are_ignored() {
        let mut alloc = DetIdAllocator::<u32>::new(5);
        assert!(!alloc.reserve(0));
        assert!(!alloc.reserve(6));
        alloc.free(0);
        alloc.free(6);
        alloc.free(3); // never issued
        assert_eq!(alloc.free_count(), 0);
        assert!(!alloc.is_live(0));
        assert_eq!(alloc.generation(99), 0);
        assert_eq!(alloc.cap(), 5);
        assert_eq!(DetIdAllocator::<u16>::new(0).cap(), 1, "cap clamps to 1");
    }

    #[test]
    fn a_u32_cap_far_above_the_ids_used_stays_small() {
        let mut alloc = DetIdAllocator::<u32>::new(u32::MAX);
        for expected in 1..=1000 {
            assert_eq!(alloc.alloc(), Some(expected));
        }
        assert!(alloc.live.len() <= 1001, "bitset grew to {}", alloc.live.len());
        assert!(alloc.reserve(u32::MAX));
        assert!(alloc.is_live(u32::MAX));
    }

    #[test]
    fn live_ids_walk_in_ascending_order() {
        let mut alloc = DetIdAllocator::<u16>::new(50);
        assert!(alloc.reserve(40));
        for _ in 0..5 {
            let _ = alloc.alloc();
        }
        alloc.free(2);
        let live: Vec<u16> = alloc.live_ids().collect();
        assert_eq!(live, vec![1, 3, 4, 5, 40]);
        assert_eq!(alloc.live_count(), 5);
    }

    /// A DO27-compatible call script: these exact ids, in this order, are what DO27's allocator
    /// returns for the same calls (worked by hand from its source; same FIFO and fresh-cursor rules).
    #[test]
    fn a_scripted_call_sequence_issues_the_source_allocators_ids() {
        let mut alloc = DetIdAllocator::<u16>::new(6);
        let mut got = Vec::new();
        assert!(alloc.reserve(3));
        got.extend((0..3).map(|_| alloc.alloc())); // 1, 2, 4
        alloc.free(2);
        alloc.free(1);
        got.push(alloc.alloc()); // 2 (oldest free)
        assert!(alloc.reserve(1)); // off the free list
        got.extend((0..4).map(|_| alloc.alloc())); // 5, 6, None, None
        alloc.free(4);
        alloc.free(3);
        got.extend((0..3).map(|_| alloc.alloc())); // 4, 3, None
        let expected = [
            Some(1),
            Some(2),
            Some(4),
            Some(2),
            Some(5),
            Some(6),
            None,
            None,
            Some(4),
            Some(3),
            None,
        ];
        assert_eq!(got, expected);
    }
}
