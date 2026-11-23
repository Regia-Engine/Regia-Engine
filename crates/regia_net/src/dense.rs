// SPDX-License-Identifier: MPL-2.0
//! Dense per-tick storage of the server send loop (spec 72 § 14, ADR-1950).
//!
//! Every replicated entity gets a small integer **slot** when the server first sees it. The slot
//! indexes plain arrays (the entity table, each connection's copies and relevance state, the
//! interest arrays), so one (connection, entity) step is an array read instead of an ordered-map
//! walk. Slots are reused after a despawn; the reuse order only depends on the spawn and despawn
//! sequence, so two runs of the same input use the same slots (spec 72 § 11).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::model::NetId;

/// "No slot" in `u32` slot arrays.
pub(crate) const NO_SLOT: u32 = u32::MAX;

pub(crate) use crate::fast_map::FastMap;

/// A growable bit set over slots.
#[derive(Debug, Clone, Default)]
pub(crate) struct SlotBits {
    words: Vec<u64>,
}

impl SlotBits {
    /// Becomes a copy of `other`, keeping this buffer.
    pub(crate) fn copy_from(&mut self, other: &SlotBits) {
        self.words.clear();
        self.words.extend_from_slice(&other.words);
    }

    /// Clears every bit and makes room for `bits` bits.
    pub(crate) fn reset(&mut self, bits: usize) {
        self.words.clear();
        self.words.resize(bits.div_ceil(64), 0);
    }

    /// Sets a bit (grows when needed).
    #[inline]
    pub(crate) fn set(&mut self, i: u32) {
        let word = i as usize / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        if let Some(w) = self.words.get_mut(word) {
            *w |= 1 << (i % 64);
        }
    }

    /// Clears a bit.
    #[inline]
    pub(crate) fn unset(&mut self, i: u32) {
        if let Some(w) = self.words.get_mut(i as usize / 64) {
            *w &= !(1 << (i % 64));
        }
    }

    /// Whether a bit is set.
    #[inline]
    pub(crate) fn get(&self, i: u32) -> bool {
        self.words
            .get(i as usize / 64)
            .is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    /// Clears every bit that is set in `other`.
    pub(crate) fn and_not(&mut self, other: &SlotBits) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= !*b;
        }
    }

    /// How many bits are set.
    pub(crate) fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Runs `f` for the set bits in ascending order.
    #[inline]
    pub(crate) fn for_each(&self, mut f: impl FnMut(u32)) {
        for (i, word) in self.words.iter().enumerate() {
            let mut w = *word;
            while w != 0 {
                f(i as u32 * 64 + w.trailing_zeros());
                w &= w - 1;
            }
        }
    }

    /// The set bits in ascending order.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(i, w)| {
            let mut w = *w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let bit = w.trailing_zeros();
                w &= w - 1;
                Some(i as u32 * 64 + bit)
            })
        })
    }
}

/// Values for the few slots one connection holds: a packed array behind an index by slot. A
/// connection's walk over the entities it holds touches its own packed values, whatever the total
/// entity count, which is what keeps per-connection work in cache at 64 connections.
#[derive(Debug, Clone)]
pub(crate) struct HeldSlots<T> {
    /// Slot to position in `values` (`NO_SLOT` = none).
    index: Vec<u32>,
    values: Vec<T>,
    /// The slot of each value.
    owners: Vec<u32>,
}

impl<T> Default for HeldSlots<T> {
    fn default() -> Self {
        Self {
            index: Vec::new(),
            values: Vec::new(),
            owners: Vec::new(),
        }
    }
}

impl<T> HeldSlots<T> {
    /// Values held.
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    /// The value of a slot.
    #[inline]
    pub(crate) fn get(&self, slot: u32) -> Option<&T> {
        match self.index.get(slot as usize) {
            Some(at) if *at != NO_SLOT => self.values.get(*at as usize),
            _ => None,
        }
    }

    /// The value of a slot, to change it.
    #[inline]
    pub(crate) fn get_mut(&mut self, slot: u32) -> Option<&mut T> {
        match self.index.get(slot as usize) {
            Some(at) if *at != NO_SLOT => self.values.get_mut(*at as usize),
            _ => None,
        }
    }

    /// Sets the value of a slot.
    pub(crate) fn insert(&mut self, slot: u32, value: T) {
        if slot as usize >= self.index.len() {
            self.index.resize(slot as usize + 1, NO_SLOT);
        }
        if let Some(old) = self.get_mut(slot) {
            *old = value;
            return;
        }
        let at = self.values.len() as u32;
        if let Some(i) = self.index.get_mut(slot as usize) {
            *i = at;
        }
        self.values.push(value);
        self.owners.push(slot);
    }

    /// The slots and their values, packed: a walk over every value without the index.
    pub(crate) fn parts_mut(&mut self) -> (&[u32], &mut [T]) {
        (&self.owners, &mut self.values)
    }

    /// Drops the value of a slot.
    pub(crate) fn remove(&mut self, slot: u32) {
        let Some(at) = self.index.get_mut(slot as usize) else {
            return;
        };
        let at = std::mem::replace(at, NO_SLOT);
        if at == NO_SLOT {
            return;
        }
        self.values.swap_remove(at as usize);
        self.owners.swap_remove(at as usize);
        if let Some(moved) = self.owners.get(at as usize).copied()
            && let Some(i) = self.index.get_mut(moved as usize)
        {
            *i = at;
        }
    }

    /// The value of a slot, created from `Default` when missing.
    pub(crate) fn entry(&mut self, slot: u32) -> Option<&mut T>
    where
        T: Default,
    {
        if self.get(slot).is_none() {
            self.insert(slot, T::default());
        }
        self.get_mut(slot)
    }
}

/// Values by [`NetId`] in dense slots.
///
/// `get` by id is a binary search over the id-ordered index; hot loops keep the slot and use
/// [`SlotTable::at`]. A removed entity's slot becomes free at the next [`SlotTable::reclaim`], so
/// per-connection arrays can drop their state for it first.
#[derive(Debug, Clone)]
pub(crate) struct SlotTable<T> {
    slots: Vec<Option<T>>,
    /// `(id, slot)` sorted by id.
    order: Vec<(NetId, u32)>,
    free: Vec<u32>,
    freed: Vec<u32>,
}

impl<T> Default for SlotTable<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            order: Vec::new(),
            free: Vec::new(),
            freed: Vec::new(),
        }
    }
}

impl<T> SlotTable<T> {
    /// Live values.
    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }

    /// The values by slot.
    pub(crate) fn slots(&self) -> &[Option<T>] {
        &self.slots
    }

    /// Inserts a value under a new id and returns its slot.
    pub(crate) fn insert(&mut self, id: NetId, value: T) -> u32 {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.slots.push(None);
                (self.slots.len() - 1) as u32
            }
        };
        if let Some(s) = self.slots.get_mut(slot as usize) {
            *s = Some(value);
        }
        // Ids only grow, so this is a push in practice.
        match self.order.last() {
            Some((last, _)) if *last > id => {
                let at = self.order.partition_point(|(i, _)| *i < id);
                self.order.insert(at, (id, slot));
            }
            _ => self.order.push((id, slot)),
        }
        slot
    }

    /// Removes a value; its slot stays taken until [`SlotTable::reclaim`].
    pub(crate) fn remove(&mut self, id: NetId) -> Option<(u32, T)> {
        let at = self.order.binary_search_by_key(&id, |(i, _)| *i).ok()?;
        #[cfg(test)]
        add_order_elems_moved((self.order.len() - at) as u64);
        let (_, slot) = self.order.remove(at);
        self.freed.push(slot);
        let value = self.slots.get_mut(slot as usize)?.take()?;
        Some((slot, value))
    }

    /// Removes every id in one pass over the index. Missing ids are skipped. The returned rows are
    /// in `NetId` order. Slots stay taken until [`SlotTable::reclaim`]. One id uses [`Self::remove`].
    pub(crate) fn remove_many(&mut self, mut ids: Vec<NetId>) -> Vec<(NetId, u32, T)> {
        if ids.is_empty() {
            return Vec::new();
        }
        ids.sort_unstable();
        ids.dedup();
        // A single despawn is the common case. The binary-search removal shifts one tail; the
        // batch path below copies the whole index once.
        if let [id] = ids[..] {
            return match self.remove(id) {
                Some((slot, value)) => vec![(id, slot, value)],
                None => Vec::new(),
            };
        }
        #[cfg(test)]
        add_order_elems_moved(self.order.len() as u64);
        let mut kept = Vec::with_capacity(self.order.len().saturating_sub(ids.len()));
        let mut out = Vec::with_capacity(ids.len());
        let mut next = 0usize;
        for (id, slot) in self.order.drain(..) {
            while next < ids.len() && ids[next] < id {
                next += 1;
            }
            if next < ids.len() && ids[next] == id {
                self.freed.push(slot);
                if let Some(value) = self.slots.get_mut(slot as usize).and_then(|s| s.take()) {
                    out.push((id, slot, value));
                }
                next += 1;
            } else {
                kept.push((id, slot));
            }
        }
        self.order = kept;
        out
    }

    /// Makes the slots of removed values available again.
    pub(crate) fn reclaim(&mut self) {
        // Lowest slots are reused first, whatever order the removals came in.
        self.free.append(&mut self.freed);
        self.free.sort_unstable_by(|a, b| b.cmp(a));
    }

    /// The slot of an id.
    pub(crate) fn slot_of(&self, id: NetId) -> Option<u32> {
        self.order
            .binary_search_by_key(&id, |(i, _)| *i)
            .ok()
            .and_then(|at| self.order.get(at))
            .map(|(_, s)| *s)
    }

    /// A value by id.
    pub(crate) fn get(&self, id: NetId) -> Option<&T> {
        self.at(self.slot_of(id)?)
    }

    /// A value by id, mutable.
    pub(crate) fn get_mut(&mut self, id: NetId) -> Option<&mut T> {
        let slot = self.slot_of(id)?;
        self.at_mut(slot)
    }

    /// A value by slot.
    #[inline]
    pub(crate) fn at(&self, slot: u32) -> Option<&T> {
        self.slots.get(slot as usize)?.as_ref()
    }

    /// A value by slot, mutable.
    #[inline]
    pub(crate) fn at_mut(&mut self, slot: u32) -> Option<&mut T> {
        self.slots.get_mut(slot as usize)?.as_mut()
    }

    /// `(id, slot)` in id order.
    pub(crate) fn ids(&self) -> &[(NetId, u32)] {
        &self.order
    }

    /// `(id, value)` in id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (NetId, &T)> + '_ {
        self.order
            .iter()
            .filter_map(|(id, slot)| self.at(*slot).map(|v| (*id, v)))
    }

    /// Mutable slots, for parallel change detection (in slot order; `None` = free).
    pub(crate) fn slots_mut(&mut self) -> &mut [Option<T>] {
        &mut self.slots
    }

    /// Every value, mutable, in slot order.
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> + '_ {
        self.slots.iter_mut().flatten()
    }
}

static SERIAL: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static ORDER_ELEMS_MOVED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn add_order_elems_moved(n: u64) {
    ORDER_ELEMS_MOVED.with(|c| c.set(c.get() + n));
}

#[cfg(test)]
fn reset_order_elems_moved() {
    ORDER_ELEMS_MOVED.with(|c| c.set(0));
}

#[cfg(test)]
fn order_elems_moved() -> u64 {
    ORDER_ELEMS_MOVED.with(|c| c.get())
}

/// `crate::testing::set_serial_gather`.
pub(crate) fn set_serial(on: bool) {
    SERIAL.store(on, Ordering::Relaxed);
}

const CHUNKS_PER_WORKER: usize = 4;

/// Items per chunk for `n` items: [`CHUNKS_PER_WORKER`] chunks for each of at most
/// [`MAX_RELEVANCE_THREADS`] workers, and a chunk of everything when the pool has one thread or
/// there are few items.
///
/// [`MAX_RELEVANCE_THREADS`]: crate::relevance::MAX_RELEVANCE_THREADS
pub(crate) fn worker_chunk(n: usize) -> usize {
    let workers = regia_core::tasks::ComputePool::get()
        .thread_num()
        .clamp(1, crate::relevance::MAX_RELEVANCE_THREADS);
    if n < 4 || workers == 1 || SERIAL.load(Ordering::Relaxed) {
        n.max(1)
    } else {
        // More chunks than workers: a worker that lost its core does not hold up the tick.
        n.div_ceil(workers * CHUNKS_PER_WORKER)
    }
}

/// Runs `f` over chunks of `items`, on the compute pool when there is more than one chunk (a
/// single chunk runs on the calling thread, with no task). Results come back in chunk order, so
/// the output does not depend on which worker ran what (spec 72 § 11).
pub(crate) fn par_mut<T, R, F>(items: &mut [T], chunk: usize, f: F) -> Vec<R>
where
    T: Send,
    R: Send + 'static,
    F: Fn(&mut [T]) -> R + Send + Sync,
{
    if items.len() <= chunk {
        return vec![f(items)];
    }
    regia_core::tasks::par_chunks_mut(items, chunk, |_, part| f(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_mut_keeps_chunk_order() {
        let mut items: Vec<u32> = (0..100).collect();
        let chunk = worker_chunk(items.len());
        let out = par_mut(&mut items, chunk, |part| {
            part.iter_mut().for_each(|v| *v += 1);
            part.first().copied()
        });
        assert_eq!(out.first().copied().flatten(), Some(1));
        assert!(out.windows(2).all(|w| w[0] < w[1]), "{out:?}");
        assert_eq!(items.iter().sum::<u32>(), (1..=100).sum::<u32>());
    }

    #[test]
    fn fast_map_finds_what_it_holds() {
        let mut m: FastMap<(i32, i32), u32> = FastMap::default();
        for x in -50..50 {
            for z in -50..50 {
                m.insert((x, z), (x * 1000 + z) as u32);
            }
        }
        assert_eq!(m.len(), 10_000);
        assert_eq!(m.get(&(-7, 31)).copied(), Some((-7000 + 31) as u32));
        assert_eq!(m.get(&(50, 0)), None);
        let mut names: FastMap<&'static str, u32> = FastMap::default();
        names.insert("transform", 1);
        names.insert("a longer component name than one word", 2);
        assert_eq!(
            names.get("a longer component name than one word").copied(),
            Some(2)
        );
    }

    #[test]
    fn held_slots_stay_packed() {
        let mut h = HeldSlots::<u32>::default();
        for s in [5, 900, 3] {
            h.insert(s, s * 10);
        }
        assert_eq!((h.len(), h.get(900).copied()), (3, Some(9000)));
        assert_eq!(h.get(4), None);
        h.insert(5, 7);
        assert_eq!((h.len(), h.get(5).copied()), (3, Some(7)));
        h.remove(5);
        h.remove(5);
        assert_eq!((h.len(), h.get(5)), (2, None));
        assert_eq!(h.get(3).copied(), Some(30), "the moved value is still found");
        assert_eq!(h.get(900).copied(), Some(9000));
        *h.entry(40).expect("new") += 1;
        *h.entry(40).expect("there") += 1;
        assert_eq!(h.get(40).copied(), Some(2));
        h.remove(100_000);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn bits_iterate_in_order() {
        let mut b = SlotBits::default();
        b.reset(130);
        for i in [129, 0, 64, 63, 5] {
            b.set(i);
        }
        assert!(b.get(64) && !b.get(65));
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![0, 5, 63, 64, 129]);
        let mut seen = Vec::new();
        b.for_each(|i| seen.push(i));
        assert_eq!(seen, vec![0, 5, 63, 64, 129]);
        let mut less = b.clone();
        let mut other = SlotBits::default();
        other.set(5);
        other.set(64);
        other.set(4000);
        less.and_not(&other);
        assert_eq!(less.iter().collect::<Vec<_>>(), vec![0, 63, 129]);
        b.unset(64);
        assert_eq!(b.iter().count(), 4);
        b.set(1000);
        assert!(b.get(1000), "set grows");
        b.reset(10);
        assert_eq!(b.iter().count(), 0);
    }

    #[test]
    fn slots_are_reused_after_reclaim_only() {
        let mut t: SlotTable<&str> = SlotTable::default();
        assert_eq!(t.insert(NetId(1), "a"), 0);
        assert_eq!(t.insert(NetId(2), "b"), 1);
        assert_eq!(t.insert(NetId(3), "c"), 2);
        assert_eq!(t.remove(NetId(2)), Some((1, "b")));
        assert_eq!(t.remove(NetId(1)), Some((0, "a")));
        assert_eq!(t.remove(NetId(1)), None);
        assert_eq!(t.insert(NetId(4), "d"), 3, "not reclaimed yet");
        t.reclaim();
        assert_eq!(t.insert(NetId(5), "e"), 0, "lowest slot first");
        assert_eq!(t.insert(NetId(6), "f"), 1);
        assert_eq!(t.get(NetId(6)), Some(&"f"));
        assert_eq!(t.slot_of(NetId(3)), Some(2));
        assert_eq!(
            t.iter().map(|(i, _)| i.0).collect::<Vec<_>>(),
            vec![3, 4, 5, 6],
            "id order, whatever the slots"
        );
        assert_eq!(t.len(), 4);
        assert_eq!(t.slots().len(), 4);
    }

    /// Removing every other id of a full table stays in `NetId` order and moves each index entry
    /// a constant number of times (one filter pass, not one tail shift per id).
    #[test]
    fn mass_remove_keeps_id_order() {
        const N: u32 = 4096;
        let mut t: SlotTable<u32> = SlotTable::default();
        for i in 0..N {
            assert_eq!(t.insert(NetId(i), i), i);
        }
        reset_order_elems_moved();
        let ids: Vec<NetId> = (0..N).filter(|i| i.is_multiple_of(2)).map(NetId).collect();
        let removed = t.remove_many(ids);
        let mut removed_slots = Vec::new();
        for (id, slot, value) in removed {
            assert_eq!(value, id.0);
            removed_slots.push(slot);
        }
        let moved = order_elems_moved();
        assert!(
            moved <= u64::from(N) * 2,
            "removing every other id moved {moved} index entries (bound {})",
            u64::from(N) * 2
        );
        assert_eq!(t.len(), (N / 2) as usize);
        let left: Vec<u32> = t
            .iter()
            .map(|(id, v)| {
                assert_eq!(id.0, *v);
                assert_eq!(t.slot_of(id), Some(id.0));
                id.0
            })
            .collect();
        assert_eq!(left, (0..N).filter(|i| !i.is_multiple_of(2)).collect::<Vec<_>>());
        for i in (0..N).filter(|i| i.is_multiple_of(2)) {
            assert_eq!(t.slot_of(NetId(i)), None);
        }
        t.reclaim();
        let reused = t.insert(NetId(N), 7);
        assert_eq!(reused, *removed_slots.iter().min().expect("a freed slot"));
        assert_eq!(t.get(NetId(N)), Some(&7));
    }
}
