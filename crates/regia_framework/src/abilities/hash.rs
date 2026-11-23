// SPDX-License-Identifier: MPL-2.0
//! The cross-platform state hash of spec 53 § 7: counts fold exactly and attributes fold quantized
//! to [`ATTRIBUTE_QUANTUM`], so two machines whose floats differ only in the last places of a libm
//! `Function` amount agree on the hash. `state_hash::snapshot_hash` stays the bit-exact one-platform
//! check.

use regia_app::prelude::*;
use regia_scene::components::EntityGuid;

use super::{AbilityBook, AbilityCounters, AppliedEffects, Attributes, TagCounts, sorted_abilities_entities};

/// The attribute grid of [`abilities_state_hash`] (`1e-4`, § 7). Two values in one grid cell hash
/// the same. Two values on either side of a cell edge do not, however close: quantizing narrows
/// the tolerance problem, it cannot remove it, so a mismatch names a step to compare by value.
pub const ATTRIBUTE_QUANTUM: f64 = 1e-4;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over little-endian words, so the hash does not depend on the host's byte order.
struct Fold(u64);

impl Fold {
    fn u64(&mut self, v: u64) {
        for byte in v.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    fn quantized(&mut self, v: f32) {
        self.u64(quantize(v) as u64);
    }
}

/// `v` on the [`ATTRIBUTE_QUANTUM`] grid. Worked in `f64`, which holds any `f32` exactly, so the
/// result does not depend on the platform's `f32` rounding; `-0.0` and `0.0` land on one cell, and
/// a NaN (never stored, the module refuses them) on the cell of 0.
#[must_use]
pub fn quantize(v: f32) -> i64 {
    // `as` saturates: an infinity or a value past the `i64` range lands on the end of the range.
    (f64::from(v) / ATTRIBUTE_QUANTUM).round() as i64
}

/// A hash of the abilities state of `world` that two platforms running the same inputs agree on:
/// [`AbilityCounters`] and, per entity in the § 7 order, its id (the `EntityGuid`), then attributes
/// (quantized `base` and `current`), live effects (id, stacks, level, remaining ticks, next pulse,
/// suspended), grants (id, level, state) and tag counts. Nothing about presentation is in it.
/// Entities without an `EntityGuid` are ordered and keyed by their bits, which differ between runs
/// unless they spawn in one order.
#[must_use]
pub fn abilities_state_hash(world: &mut World) -> u64 {
    abilities_state_hash_keyed(world, Entity::to_bits)
}

/// [`abilities_state_hash`] with the key of each entity without an `EntityGuid` given by
/// `key_of` instead of its bits (ADR-5960): a host that knows such entities by a stable number of
/// its own (a spawn index, a save slot) gets a hash that does not move when an unrelated spawn, or
/// a resource stored as an entity, shifts the entity ids. They are still ordered by their bits, so
/// the order only stays put when every id shifts alike. `abilities_state_hash(w)` is
/// `abilities_state_hash_keyed(w, Entity::to_bits)`.
#[must_use]
pub fn abilities_state_hash_keyed(world: &mut World, mut key_of: impl FnMut(Entity) -> u64) -> u64 {
    let _span = tracing::debug_span!(target: "regia_framework", "abilities_state_hash").entered();
    let mut fold = Fold(FNV_OFFSET);
    if let Some(counters) = world.get_resource::<AbilityCounters>() {
        fold.u64(counters.tick);
        fold.u64(counters.last_effect_id);
        for &(key, asset, position) in &counters.rolls {
            fold.u64(key);
            fold.u64(asset);
            fold.u64(position);
        }
    }
    for e in sorted_abilities_entities(world) {
        match world.get::<EntityGuid>(e) {
            Some(guid) => {
                fold.u64(guid.0.0 as u64);
                fold.u64((guid.0.0 >> 64) as u64);
            }
            None => fold.u64(key_of(e)),
        }
        if let Some(attributes) = world.get::<Attributes>(e) {
            fold.u64(attributes.len() as u64);
            for a in attributes.list() {
                fold.u64(a.id.0);
                fold.quantized(a.base);
                fold.quantized(a.current);
            }
        }
        if let Some(effects) = world.get::<AppliedEffects>(e) {
            fold.u64(effects.list.len() as u64);
            for x in &effects.list {
                fold.u64(x.id.0);
                fold.u64(u64::from(x.stacks));
                fold.u64(u64::from(x.level));
                fold.u64(u64::from(x.remaining_ticks));
                fold.u64(x.next_pulse_tick);
                fold.u64(u64::from(x.suspended));
            }
        }
        if let Some(book) = world.get::<AbilityBook>(e) {
            fold.u64(book.grants.len() as u64);
            for grant in &book.grants {
                fold.u64(u64::from(grant.id.0));
                fold.u64(u64::from(grant.level));
                fold.u64(grant.state as u64);
            }
        }
        if let Some(counts) = world.get::<TagCounts>(e) {
            for &(tag, n) in counts.iter() {
                if n > 0 {
                    fold.u64(tag.id());
                    fold.u64(u64::from(n));
                }
            }
        }
    }
    fold.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_quantize_to_a_fixed_grid() {
        assert_eq!(quantize(0.0), 0);
        assert_eq!(quantize(-0.0), 0);
        assert_eq!(quantize(1.0), 10_000);
        assert_eq!(quantize(-1.0), -10_000);
        assert_eq!(quantize(0.00014), 1);
        assert_eq!(quantize(0.00016), 2);
        assert_eq!(quantize(f32::NAN), 0);
        assert_eq!(quantize(f32::INFINITY), i64::MAX);
        assert_eq!(quantize(f32::NEG_INFINITY), i64::MIN);
    }

    /// § 7's cross-platform tolerance (`1e-5` relative) stays inside one cell for values under 1,
    /// away from a cell edge (`0.12345` is on one: the edge is the limit `ATTRIBUTE_QUANTUM` names).
    #[test]
    fn a_value_inside_the_tolerance_lands_on_its_cell() {
        let v = 0.12341_f32;
        assert_eq!(quantize(v), quantize(v * (1.0 + 1e-5)));
        assert_eq!(quantize(v), quantize(v * (1.0 - 1e-5)));
    }
}
