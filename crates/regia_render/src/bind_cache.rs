// SPDX-License-Identifier: MPL-2.0
//! Bind groups and uniform bytes reused across frames.
//!
//! A bind group references buffer and texture objects, not their contents.
//! `queue.write_buffer` on a cached buffer stays valid for every group that
//! binds it, so a group is rebuilt only when one of those objects is replaced.
//! The byte cache skips a `write_buffer` when the CPU contents did not change.
//!
//! A key holds clones of the views and buffers it binds, and a clone keeps its texture alive.
//! When a size-dependent target is replaced (a resize, a dynamic-resolution step), the old
//! entries are never hit again but would hold every old target until the cap. The renderer
//! therefore runs each frame inside an [`EpochScope`] and starts a new epoch when its targets
//! change size; a cache accessed under a different epoch than its last access drops its
//! entries first (ADR-184 "Texture views keep their own view state", review fixes).

use std::cell::Cell;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

/// Entries kept before the map is dropped. Post rotates 16 parameter slots
/// across many source views; a small cap would rebuild the same groups forever.
const BIND_CACHE_CAP: usize = 512;

/// Source of process-unique epochs (0 means "no scope").
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The epoch of the frame being recorded on this thread (0 outside any [`EpochScope`]).
    static CURRENT_EPOCH: Cell<u64> = const { Cell::new(0) };
}

/// A new epoch, different from every epoch handed out before in this process.
pub(crate) fn new_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Makes `epoch` the current one on this thread until dropped (then restores the previous,
/// so a nested renderer's frame does not leak its epoch into the outer one).
pub(crate) struct EpochScope {
    previous: u64,
}

impl EpochScope {
    /// Enters `epoch` (one renderer's current epoch, from [`new_epoch`]).
    pub(crate) fn enter(epoch: u64) -> Self {
        let previous = CURRENT_EPOCH.with(|current| current.replace(epoch));
        Self { previous }
    }
}

impl Drop for EpochScope {
    fn drop(&mut self) {
        CURRENT_EPOCH.with(|current| current.set(self.previous));
    }
}

/// How many frames a [`PerSize`] entry outlives its last use: long enough for a texture camera
/// that draws every few frames, short enough that a resize frees the old size soon.
const PER_SIZE_KEEP_FRAMES: u64 = 8;

/// Size-dependent resources of a frame pass, one entry per key (a size, a format). Views of
/// different sizes in one frame (uneven split screen, a texture camera) keep their own entry
/// instead of rebuilding a single one per view per frame; an entry no view used for
/// [`PER_SIZE_KEEP_FRAMES`] frames (a resize, a dynamic-resolution step, the effect turned off)
/// goes at the next [`Self::evict`].
pub(crate) struct PerSize<K, T> {
    /// Key, the frame of the last use, the resources.
    entries: Vec<(K, u64, T)>,
}

impl<K, T> Default for PerSize<K, T> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<K: PartialEq, T> PerSize<K, T> {
    /// The entry of `key`, built with `make` when there is none, marked as used in `frame`.
    pub(crate) fn get_or_insert_with(&mut self, key: K, frame: u64, make: impl FnOnce() -> T) -> &T {
        let index = match self.entries.iter().position(|(k, _, _)| *k == key) {
            Some(index) => {
                self.entries[index].1 = frame;
                index
            }
            None => {
                self.entries.push((key, frame, make()));
                self.entries.len() - 1
            }
        };
        &self.entries[index].2
    }

    /// The entry of `key`, if there is one.
    pub(crate) fn get(&self, key: &K) -> Option<&T> {
        self.entries.iter().find(|(k, _, _)| k == key).map(|(_, _, t)| t)
    }

    /// Drops the entries unused for more than [`PER_SIZE_KEEP_FRAMES`] frames before `frame`;
    /// returns whether any went.
    pub(crate) fn evict(&mut self, frame: u64) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|(_, used, _)| used.saturating_add(PER_SIZE_KEEP_FRAMES) >= frame);
        self.entries.len() != before
    }
}

/// Map from a resource key to a reusable value (a bind group in production).
pub(crate) struct BindCache<K, V = wgpu::BindGroup> {
    groups: HashMap<K, V>,
    /// The epoch of the last access under a scope.
    epoch: u64,
    /// Values built or inserted since creation ([`Self::built`]).
    built: u64,
}

impl<K, V> Default for BindCache<K, V> {
    fn default() -> Self {
        Self {
            groups: HashMap::new(),
            epoch: 0,
            built: 0,
        }
    }
}

impl<K, V> BindCache<K, V> {
    /// How many values were built ([`Self::get_or_insert_with`] misses) or inserted so far: a
    /// caller compares it across a frame to know whether that frame built anything.
    pub(crate) fn built(&self) -> u64 {
        self.built
    }

    /// Cached groups, for buffer-retirement audits.
    #[cfg(test)]
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.groups.values()
    }

    /// The keys held now (an audit of what the cache keeps alive, ADR-7877). Entries of an old
    /// epoch are still listed until the next access drops them.
    #[cfg(test)]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.groups.keys()
    }
}

impl<K: Eq + Hash, V: Clone> BindCache<K, V> {
    /// Returns the cached value for `key`, or inserts `make()`.
    ///
    /// `make` runs only on a miss. Under a new [`EpochScope`] epoch, and at [`BIND_CACHE_CAP`],
    /// the map is cleared first, so replaced targets are released instead of piling up.
    pub(crate) fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> V {
        self.follow_epoch();
        if let Some(existing) = self.groups.get(&key) {
            return existing.clone();
        }
        if self.groups.len() >= BIND_CACHE_CAP {
            self.groups.clear();
        }
        let value = make();
        self.built += 1;
        self.groups.insert(key, value.clone());
        value
    }

    /// The cached value for `key` (after the epoch check of [`Self::get_or_insert_with`]), for
    /// callers whose `make` cannot be a closure; pair it with [`Self::insert`].
    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        self.follow_epoch();
        self.groups.get(key).cloned()
    }

    /// Stores `value` for `key` (clearing the map first at [`BIND_CACHE_CAP`]).
    pub(crate) fn insert(&mut self, key: K, value: V) {
        self.follow_epoch();
        if self.groups.len() >= BIND_CACHE_CAP {
            self.groups.clear();
        }
        self.built += 1;
        self.groups.insert(key, value);
    }

    /// Drops every entry when the thread's current epoch is not the one of the last access
    /// under a scope.
    ///
    /// A cache never accessed under a scope (filled only by a prewarm before the first frame,
    /// epoch 0) adopts the first scope's epoch and keeps its entries: a key names the objects
    /// it binds, so an entry over a replaced object is never hit and goes at the next epoch
    /// change, and dropping the prewarmed groups would only rebuild them on the frame that
    /// first uses them (the first decal of a level, ADR-2303).
    fn follow_epoch(&mut self) {
        let current = CURRENT_EPOCH.with(Cell::get);
        if current != 0 && current != self.epoch {
            if self.epoch != 0 {
                self.groups.clear();
            }
            self.epoch = current;
        }
    }

    /// Drops every entry. Call when a keyed resource was destroyed.
    pub(crate) fn clear(&mut self) {
        self.groups.clear();
    }
}

/// `true` when `cache` already holds `bytes` (the skip test used by the staging belt).
#[must_use]
pub(crate) fn bytes_match(cache: &[u8], bytes: &[u8]) -> bool {
    cache == bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_builds_once() {
        let mut cache: BindCache<u32, u32> = BindCache::default();
        let mut builds = 0u32;
        let first = cache.get_or_insert_with(7, || {
            builds += 1;
            11
        });
        let second = cache.get_or_insert_with(7, || {
            builds += 1;
            99
        });
        let other = cache.get_or_insert_with(8, || {
            builds += 1;
            3
        });
        assert_eq!(first, 11);
        assert_eq!(second, 11);
        assert_eq!(other, 3);
        assert_eq!(builds, 2, "a repeated key must not call make");
    }

    #[test]
    fn clear_forces_a_rebuild() {
        let mut cache: BindCache<&str, u32> = BindCache::default();
        let _ = cache.get_or_insert_with("a", || 1);
        cache.clear();
        let mut builds = 0u32;
        let _ = cache.get_or_insert_with("a", || {
            builds += 1;
            2
        });
        assert_eq!(builds, 1);
    }

    #[test]
    fn a_new_epoch_drops_old_entries_and_no_scope_keeps_them() {
        fn get(cache: &mut BindCache<&'static str, u32>, builds: &mut u32) {
            cache.get_or_insert_with("a", || {
                *builds += 1;
                1
            });
        }
        let mut cache: BindCache<&str, u32> = BindCache::default();
        let first = new_epoch();
        let mut builds = 0u32;
        {
            let _scope = EpochScope::enter(first);
            get(&mut cache, &mut builds);
            get(&mut cache, &mut builds);
        }
        // Outside a scope (tools, captures) the entries stay.
        get(&mut cache, &mut builds);
        {
            let _scope = EpochScope::enter(first);
            get(&mut cache, &mut builds);
        }
        assert_eq!(builds, 1, "same epoch and no scope must reuse the entry");
        {
            let _outer = EpochScope::enter(new_epoch());
            get(&mut cache, &mut builds);
            {
                // A nested renderer's scope is restored on exit.
                let _inner = EpochScope::enter(new_epoch());
            }
            get(&mut cache, &mut builds);
        }
        assert_eq!(builds, 2, "a new epoch rebuilds once");
    }

    #[test]
    fn a_prewarmed_entry_survives_its_first_frame() {
        // Filled outside any scope (a level prewarm), first read inside a frame's scope: the
        // entry is kept (ADR-2303: the first decal of a level rebuilt its receiver group).
        let mut cache: BindCache<&str, u32> = BindCache::default();
        let mut builds = 0u32;
        let mut get = |cache: &mut BindCache<&str, u32>| {
            cache.get_or_insert_with("receiver", || {
                builds += 1;
                7
            })
        };
        assert_eq!(get(&mut cache), 7);
        {
            let _frame = EpochScope::enter(new_epoch());
            assert_eq!(get(&mut cache), 7);
        }
        {
            let _resized = EpochScope::enter(new_epoch());
            assert_eq!(get(&mut cache), 7);
        }
        assert_eq!(builds, 2, "kept into the first epoch, dropped at the next one");
    }

    #[test]
    fn per_size_keeps_every_size_in_use_and_drops_the_rest() {
        let mut cache: PerSize<u32, u32> = PerSize::default();
        let mut builds = 0u32;
        // Two sizes every frame, for far longer than the keep window: each builds once.
        for frame in 0..100 {
            for size in [64, 128] {
                let built = *cache.get_or_insert_with(size, frame, || {
                    builds += 1;
                    size * 2
                });
                assert_eq!(built, size * 2);
            }
            assert!(!cache.evict(frame), "nothing goes while it is used");
        }
        assert_eq!(builds, 2);
        // One size stops being used (a resize): it stays through the window, then goes.
        let last_used = 99;
        for frame in 100..=(last_used + PER_SIZE_KEEP_FRAMES) {
            let _ = cache.get_or_insert_with(128, frame, || 0);
            assert!(!cache.evict(frame), "frame {frame} is inside the window");
        }
        let frame = last_used + PER_SIZE_KEEP_FRAMES + 1;
        let _ = cache.get_or_insert_with(128, frame, || 0);
        assert!(cache.evict(frame));
        assert_eq!(cache.get(&64), None);
        assert_eq!(cache.get(&128), Some(&256));
    }

    #[test]
    fn bytes_match_is_exact() {
        assert!(bytes_match(b"abc", b"abc"));
        assert!(!bytes_match(b"abc", b"abd"));
        assert!(!bytes_match(b"", b"a"));
        assert!(bytes_match(b"", b""));
    }
}
