// SPDX-License-Identifier: MPL-2.0
//! Descriptor growth policy of the material texture table (bindless step 5, ADR-1110).
//!
//! The table's layout declares `capacity` views (`texture_table::table_capacity`), and the
//! shaders are composed with that size, so the declared size never changes while the process
//! runs: growing it would recompile every table pipeline. What can change is how many of those
//! elements each rebuilt table group *binds*:
//!
//! - **Partially bound devices** (`PARTIALLY_BOUND_BINDING_ARRAY`: Vulkan and DX12, requested
//!   when the adapter has it, [`optional_table_features`]) bind a prefix: the *bound length*.
//!   It starts at [`MIN_BOUND`], grows to the next power of two above the high-water mark when a
//!   new index passes it, and shrinks (to twice the next power of two of the high-water mark)
//!   only when the high-water mark has fallen to a quarter of it. Both happen only on a rebuild
//!   that a changed element forced anyway, so the policy never adds a rebuild. A rebuild walks
//!   the bound prefix, not the whole table, so its CPU cost follows the scene's texture count.
//! - **Every other device** (Metal has no partially bound arrays) binds every element, as
//!   ADR-863 does: the bound length is the capacity.
//!
//! Indices are handed out lowest first (`texture_table::TableSlots`), and freeing the top index
//! lowers the high-water mark, so a scene that loads, unloads and reloads stays in a compact
//! prefix.
//!
//! No index a record names is ever past the bound length: every live index is below the
//! high-water mark, and the bound length is never below it.

/// The smallest bound length on a partially bound device (or the capacity, if smaller).
pub const MIN_BOUND: u32 = 256;

/// A partially bound table shrinks once the high-water mark's power of two is at most the bound
/// length over this.
pub const SHRINK_DIVISOR: u32 = 4;

/// The optional device feature the growth policy uses: requested with the table features when
/// the adapter has them all (`device.rs`). Without it every element is bound.
pub const PARTIAL_FEATURE: wgpu::Features = wgpu::Features::PARTIALLY_BOUND_BINDING_ARRAY;

/// Optional features a device created at bindless `level` asks for beside
/// [`crate::texture_table::device_features_for_level`]: [`PARTIAL_FEATURE`] when the level asks
/// for the table and the adapter has the table features and partially bound arrays, else none.
#[must_use]
pub fn optional_table_features(level: u32, adapter_features: wgpu::Features) -> wgpu::Features {
    let table = crate::texture_table::device_features_for_level(level, adapter_features);
    if !table.is_empty() && adapter_features.contains(PARTIAL_FEATURE) {
        PARTIAL_FEATURE
    } else {
        wgpu::Features::empty()
    }
}

/// The power-of-two prefix a partially bound table binds for `high_water` (one past the highest
/// live index), at least [`MIN_BOUND`], at most `capacity`.
#[must_use]
pub fn grow_target(high_water: u32, capacity: u32) -> u32 {
    high_water
        .max(MIN_BOUND)
        .checked_next_power_of_two()
        .unwrap_or(u32::MAX)
        .min(capacity)
}

/// The bound length a rebuild uses: `current` is the last rebuild's (0 before the first),
/// `high_water` one past the highest live index, `capacity` the declared size, and `partial`
/// whether the device may bind a prefix. Pure: the same inputs give the same length.
///
/// - Not `partial`: `capacity`.
/// - Grows to [`grow_target`] when `high_water` passes `current` (or on the first rebuild).
/// - Shrinks to twice [`grow_target`] (at most `current`) when [`grow_target`] times
///   [`SHRINK_DIVISOR`] is at most `current`; between the two it keeps `current`, so a scene
///   that oscillates around a power of two does not change length on every rebuild.
#[must_use]
pub fn bound_len(current: u32, high_water: u32, capacity: u32, partial: bool) -> u32 {
    if !partial {
        return capacity;
    }
    let target = grow_target(high_water, capacity);
    if current == 0 || target > current {
        return target;
    }
    if target.saturating_mul(SHRINK_DIVISOR) <= current {
        return target.saturating_mul(2).min(current);
    }
    current.min(capacity)
}

/// The declared size to ask for next launch ([`crate::texture_table::TABLE_VIEWS_ENV`]) after a
/// table of `capacity` views overflowed with `live` textures held and `overflows` slots that did
/// not fit: the next power of two that holds them all and is larger than `capacity`, at most
/// [`crate::texture_table::TEXTURE_TABLE_VIEWS_LIMIT`]. `None` when the table is already at the
/// limit. The device's binding-array budget may still cap what it gets.
#[must_use]
pub fn suggested_views(capacity: u32, live: u64, overflows: u64) -> Option<u32> {
    let limit = crate::texture_table::TEXTURE_TABLE_VIEWS_LIMIT;
    if capacity >= limit {
        return None;
    }
    let needed = live
        .saturating_add(overflows)
        .saturating_add(u64::from(crate::texture_table::RESERVED_INDICES))
        .max(u64::from(capacity) + 1)
        .min(u64::from(limit));
    let needed = u32::try_from(needed).unwrap_or(limit);
    Some(needed.checked_next_power_of_two().unwrap_or(limit).min(limit))
}

/// What a table rebuild bound ([`crate::TextureTableStats`] carries the totals).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HeapState {
    /// Elements the last rebuild bound.
    pub(crate) bound: u32,
    /// Times the bound length changed (the first rebuild not counted).
    pub(crate) changes: u64,
    /// Elements bound over every rebuild (the rebuild cost, in descriptor writes).
    pub(crate) written: u64,
}

impl HeapState {
    /// Picks the bound length of a rebuild ([`bound_len`]) and counts it.
    pub(crate) fn rebuild(&mut self, high_water: u32, capacity: u32, partial: bool) -> u32 {
        let next = bound_len(self.bound, high_water, capacity, partial);
        if self.bound != 0 && next != self.bound {
            self.changes += 1;
        }
        self.bound = next;
        self.written += u64::from(next);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u32 = 4032;

    #[test]
    fn fully_bound_devices_bind_the_whole_table() {
        for high_water in [4, 300, CAP] {
            assert_eq!(bound_len(0, high_water, CAP, false), CAP);
            assert_eq!(bound_len(CAP, high_water, CAP, false), CAP);
        }
    }

    #[test]
    fn the_first_rebuild_binds_the_smallest_prefix_that_holds_the_scene() {
        assert_eq!(bound_len(0, 4, CAP, true), MIN_BOUND, "reserved indices only");
        assert_eq!(bound_len(0, MIN_BOUND, CAP, true), MIN_BOUND);
        assert_eq!(bound_len(0, MIN_BOUND + 1, CAP, true), 512);
        assert_eq!(bound_len(0, 3000, CAP, true), CAP, "capped at the declared size");
        // A table smaller than the minimum binds all of it.
        assert_eq!(bound_len(0, 4, 100, true), 100);
    }

    #[test]
    fn growth_doubles_past_the_high_water_mark() {
        assert_eq!(bound_len(256, 257, CAP, true), 512);
        assert_eq!(bound_len(512, 1500, CAP, true), 2048);
        assert_eq!(bound_len(2048, 2049, CAP, true), CAP);
        assert_eq!(bound_len(512, 512, CAP, true), 512, "exactly full stays");
    }

    /// The hysteresis band: no change until the scene fell to a quarter, then one halving step
    /// that still leaves room to grow back without a change.
    #[test]
    fn shrinking_waits_for_a_quarter_and_keeps_headroom() {
        assert_eq!(bound_len(2048, 1025, CAP, true), 2048);
        assert_eq!(bound_len(2048, 600, CAP, true), 2048, "1024 × 4 > 2048: keep");
        assert_eq!(bound_len(2048, 512, CAP, true), 1024, "512 × 4 ≤ 2048: halve");
        assert_eq!(
            bound_len(2048, 4, CAP, true),
            512,
            "never below twice the minimum target"
        );
        assert_eq!(bound_len(256, 4, CAP, true), 256, "never below the minimum");
    }

    /// Whatever the history, the bound length always holds every live index and never passes
    /// the declared size.
    #[test]
    fn the_bound_always_covers_the_high_water_mark() {
        let mut state = HeapState::default();
        let mut seed = 0x2545_f491u32;
        for _ in 0..10_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let high_water = 4 + seed % (CAP - 3);
            let bound = state.rebuild(high_water, CAP, true);
            assert!(bound >= high_water, "{bound} < {high_water}");
            assert!(bound <= CAP);
        }
        assert!(state.changes > 0);
    }

    #[test]
    fn heap_state_counts_changes_and_descriptor_writes() {
        let mut state = HeapState::default();
        assert_eq!(state.rebuild(10, CAP, true), 256);
        assert_eq!(state.changes, 0, "the first rebuild is not a change");
        assert_eq!(state.rebuild(10, CAP, true), 256);
        assert_eq!(state.rebuild(300, CAP, true), 512);
        assert_eq!(state.rebuild(60, CAP, true), 512, "inside the band");
        assert_eq!(state.changes, 1);
        assert_eq!(state.written, 256 + 256 + 512 + 512);
        let mut full = HeapState::default();
        full.rebuild(10, CAP, false);
        full.rebuild(300, CAP, false);
        assert_eq!(full.changes, 0);
        assert_eq!(full.written, 2 * u64::from(CAP));
    }

    /// Indices go lowest first, and freeing the top lowers the high-water mark past every free
    /// index under it, so a scene that unloads and reloads stays in a compact prefix.
    #[test]
    fn indices_go_lowest_first_and_the_high_water_mark_falls() {
        use crate::texture_table::{RESERVED_INDICES, TableSlots};
        let mut slots: TableSlots<u32> = TableSlots::new(64);
        assert_eq!(slots.high_water(), RESERVED_INDICES);
        let held: Vec<u32> = (0..8).map(|k| slots.acquire(&k).expect("room").0).collect();
        assert_eq!(held, (4..12).collect::<Vec<_>>());
        assert_eq!(slots.high_water(), 12);
        // Free 5 and 9: the lowest free index goes first.
        assert!(slots.release(9));
        assert!(slots.release(5));
        assert_eq!(slots.high_water(), 12, "11 is still held");
        assert_eq!(slots.acquire(&100), Some((5, true)));
        assert_eq!(slots.acquire(&101), Some((9, true)));
        assert_eq!(slots.acquire(&102), Some((12, true)), "then past the mark");
        // Free the top three and one under them: the mark falls to the highest held index.
        for index in [12, 11, 10] {
            assert!(slots.release(index));
        }
        assert_eq!(slots.high_water(), 10, "9 holds 101");
        assert!(slots.release(9));
        assert_eq!(slots.high_water(), 9);
        // Everything gone: back to the reserved indices.
        for index in [4, 5, 6, 7, 8] {
            assert!(slots.release(index));
        }
        assert_eq!(slots.high_water(), RESERVED_INDICES);
        assert_eq!(slots.live(), 0);
        assert_eq!(slots.acquire(&7), Some((RESERVED_INDICES, true)));
    }

    /// Churn with a sliding window: the high-water mark never passes the reserved indices plus
    /// the peak live count.
    #[test]
    fn churn_keeps_the_high_water_mark_at_the_peak() {
        use crate::texture_table::{RESERVED_INDICES, TableSlots};
        let mut slots: TableSlots<u32> = TableSlots::new(4096);
        let mut held = std::collections::VecDeque::new();
        let mut seed = 0x9e37_79b9u32;
        for key in 0..20_000u32 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            // Between 1 and 64 live; release from the front or a random holder.
            if held.len() > (seed % 64) as usize
                && let Some(index) = held.remove((seed as usize / 64) % held.len())
            {
                assert!(slots.release(index));
            }
            let (index, fresh) = slots.acquire(&key).expect("room");
            assert!(fresh);
            held.push_back(index);
            assert!(
                slots.high_water() <= RESERVED_INDICES + 64,
                "mark {}",
                slots.high_water()
            );
            assert!(held.iter().all(|&i| i < slots.high_water()));
        }
    }

    /// In-place residency swaps: a texture only one holder uses moves to the new key at the same
    /// index; a shared index, a free or reserved one, or a key already in the table refuses.
    #[test]
    fn rekeying_keeps_the_index_for_a_single_holder_only() {
        use crate::texture_table::TableSlots;
        let mut slots: TableSlots<u32> = TableSlots::new(64);
        let (a, _) = slots.acquire(&1).expect("room");
        assert!(slots.rekey(a, &2), "one holder: in place");
        assert!(!slots.holds(&1));
        assert!(slots.holds(&2));
        assert_eq!(
            slots.acquire(&2),
            Some((a, false)),
            "the index now names the new key"
        );
        assert!(!slots.rekey(a, &3), "two holders share it");
        assert!(!slots.release(a));
        let (b, _) = slots.acquire(&4).expect("room");
        assert!(!slots.rekey(a, &4), "the new key already holds an index");
        assert!(!slots.rekey(0, &5), "reserved");
        assert!(!slots.rekey(40, &5), "never handed out");
        assert!(slots.release(b));
        assert!(!slots.rekey(b, &5), "free");
        assert_eq!(slots.live(), 1);
        assert!(slots.release(a), "the rekeyed index frees like any other");
        assert_eq!(slots.live(), 0);
    }

    #[test]
    fn an_overflowing_table_suggests_the_next_power_of_two_that_fits() {
        assert_eq!(suggested_views(4096, 4092, 3), Some(8192));
        assert_eq!(
            suggested_views(4032, 4028, 1),
            Some(4096),
            "past the capacity, a power of two"
        );
        assert_eq!(
            suggested_views(4096, 4092, 5000),
            Some(16_384),
            "every overflow fits"
        );
        assert_eq!(
            suggested_views(16_384, 20, 1_000_000),
            Some(65_536),
            "capped at the 16-bit limit"
        );
        assert_eq!(suggested_views(65_536, 65_532, 9), None, "already at the limit");
    }

    #[test]
    fn partial_binding_is_requested_only_with_the_table() {
        let table = crate::texture_table::TABLE_FEATURES;
        assert_eq!(
            optional_table_features(3, table | PARTIAL_FEATURE),
            PARTIAL_FEATURE
        );
        assert_eq!(
            optional_table_features(4, table | PARTIAL_FEATURE),
            PARTIAL_FEATURE
        );
        assert!(
            optional_table_features(1, table | PARTIAL_FEATURE).is_empty(),
            "below the table level"
        );
        assert!(
            optional_table_features(3, table).is_empty(),
            "adapter without it (Metal)"
        );
        assert!(
            optional_table_features(3, PARTIAL_FEATURE).is_empty(),
            "no table features: nothing to bind partially"
        );
    }
}
