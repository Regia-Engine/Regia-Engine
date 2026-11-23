// SPDX-License-Identifier: MPL-2.0
//! The blackboard (`docs/specs/22-tier2.md` § T2.2): a per-agent reflected key/value store that
//! behaviour-tree nodes and leaf tasks read and write.
//!
//! Values are a closed set of primitive types ([`BbValue`]) so the blackboard derives `Reflect`
//! (and so serializes and is inspectable through the editor and the agent protocol) without a
//! bespoke dynamic-map type.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;

/// Whether two values are the same for change detection.
///
/// Floats, including vector components, compare by their bits, so an unchanged NaN is not a change.
pub(crate) fn bb_value_same(left: &BbValue, right: &BbValue) -> bool {
    match (left, right) {
        (BbValue::Float(a), BbValue::Float(b)) => a.to_bits() == b.to_bits(),
        (BbValue::Vec3(a), BbValue::Vec3(b)) => {
            a.to_array().map(f32::to_bits) == b.to_array().map(f32::to_bits)
        }
        (a, b) => a == b,
    }
}

/// One value a blackboard can hold: booleans (for conditions), integers, floats (timers, health),
/// strings, 3D vectors (targets) and entity ids (targets, stored as bits).
#[derive(Reflect, Debug, Clone, PartialEq)]
pub enum BbValue {
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// A 32-bit float.
    Float(f32),
    /// A string.
    String(String),
    /// A 3D vector.
    Vec3(Vec3),
    /// An entity id, stored as its raw bits.
    Entity(u64),
}

/// Makes `slot` equal to `value`, reusing `slot`'s string buffer when both are strings (no
/// allocation unless the new text is longer than the old buffer).
pub(crate) fn assign_value(slot: &mut BbValue, value: &BbValue) {
    match (slot, value) {
        (BbValue::String(old), BbValue::String(new)) => old.clone_from(new),
        (slot, value) => *slot = value.clone(),
    }
}

impl From<bool> for BbValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<i64> for BbValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<i32> for BbValue {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<f32> for BbValue {
    fn from(v: f32) -> Self {
        Self::Float(v)
    }
}
impl From<String> for BbValue {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}
impl From<&str> for BbValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_string())
    }
}
impl From<Vec3> for BbValue {
    fn from(v: Vec3) -> Self {
        Self::Vec3(v)
    }
}
impl From<Entity> for BbValue {
    fn from(v: Entity) -> Self {
        Self::Entity(v.to_bits())
    }
}

/// A reflected key/value store shared by a behaviour tree's nodes and leaf tasks.
///
/// Attach to an agent entity alongside a [`crate::BehaviorTree`]. Both the tree and any script can
/// read and write it; keys are plain strings. Deterministic iteration order (sorted keys).
///
/// [`Blackboard::insert`], [`Blackboard::assign`], [`Blackboard::append_str`] and
/// [`Blackboard::remove`] record the key when the stored value actually changes
/// ([`Blackboard::drain_changes`]). Writing [`Blackboard::values`] directly does not. Equality
/// compares the entries only, not that change set.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct Blackboard {
    /// The key/value entries.
    pub values: BTreeMap<String, BbValue>,
    /// Keys changed through [`Blackboard::insert`], [`Blackboard::assign`],
    /// [`Blackboard::append_str`] or [`Blackboard::remove`] since the last drain.
    #[reflect(ignore)]
    changes: BTreeSet<String>,
    /// Per-key write counters bumped by [`Blackboard::insert`], [`Blackboard::assign`],
    /// [`Blackboard::append_str`] and [`Blackboard::remove`], read by
    /// logic-graph `BlackboardChanged` watches (spec 102 § 4.1: a same-value write counts). Not
    /// reflected or saved, and never part of equality. Writes straight into `values` are not
    /// counted (a watch still sees them when the value changes).
    #[reflect(ignore)]
    pub writes: BlackboardWrites,
}

impl PartialEq for Blackboard {
    fn eq(&self, other: &Self) -> bool {
        // Floats match by bits (`bb_value_same`), so two NaNs with the same bits are equal.
        // The change set and the write counters are not part of equality.
        self.values.len() == other.values.len()
            && self.values.iter().all(|(key, value)| {
                other
                    .values
                    .get(key)
                    .is_some_and(|stored| bb_value_same(value, stored))
            })
    }
}

/// The write counters of a [`Blackboard`]: runtime bookkeeping, equal to every other instance.
#[derive(Debug, Clone, Default)]
pub struct BlackboardWrites {
    counts: BTreeMap<String, u64>,
}

impl PartialEq for BlackboardWrites {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl BlackboardWrites {
    fn bump(&mut self, key: &str) {
        match self.counts.get_mut(key) {
            Some(count) => *count = count.wrapping_add(1),
            None => {
                self.counts.insert(key.to_string(), 1);
            }
        }
    }
}

impl Blackboard {
    /// An empty blackboard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a value, returning the previous value for `key` (if any).
    ///
    /// Records `key` for [`Blackboard::drain_changes`] when it was absent or the new value differs.
    /// Writing the same value again, including the same NaN bits, does not record a change, but it
    /// still counts as a write of `key` ([`Blackboard::write_count`]).
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<BbValue>) -> Option<BbValue> {
        let key = key.into();
        let value = value.into();
        self.writes.bump(&key);
        let prev = self.values.get(&key).cloned();
        let changed = match &prev {
            Some(old) => !bb_value_same(old, &value),
            None => true,
        };
        self.values.insert(key.clone(), value);
        if changed {
            self.changes.insert(key);
        }
        prev
    }

    /// Writes a copy of `value` under `key`, counted like [`Blackboard::insert`] (a write of `key`
    /// even when the value is the same), but with both borrowed: when `key` is present it allocates
    /// nothing (a string value reuses the old string's buffer when it fits), so a steady writer
    /// such as a logic-graph `logic::SetBlackboard` stays off the heap (spec 102 § 13 P4.3).
    ///
    /// Records `key` for [`Blackboard::drain_changes`] like [`Blackboard::insert`] does: when it was
    /// absent or the value differs (a key already recorded since the last drain is not copied again).
    pub fn assign(&mut self, key: &str, value: &BbValue) {
        self.writes.bump(key);
        let changed = match self.values.get_mut(key) {
            Some(slot) => {
                let changed = !bb_value_same(slot, value);
                if changed {
                    assign_value(slot, value);
                }
                changed
            }
            None => {
                self.values.insert(key.to_string(), value.clone());
                true
            }
        };
        if changed {
            self.record_change(key);
        }
    }

    fn record_change(&mut self, key: &str) {
        if !self.changes.contains(key) {
            self.changes.insert(key.to_string());
        }
    }

    /// Appends `text` to the string under `key` in place, counted as a write of `key` (and recorded
    /// for [`Blackboard::drain_changes`] when `text` is not empty). Returns `false`, writing
    /// nothing, when `key` holds no string.
    pub fn append_str(&mut self, key: &str, text: &str) -> bool {
        let Some(BbValue::String(value)) = self.values.get_mut(key) else {
            return false;
        };
        value.push_str(text);
        self.writes.bump(key);
        if !text.is_empty() {
            self.record_change(key);
        }
        true
    }

    /// How many times `key` was written through [`Blackboard::insert`], [`Blackboard::assign`],
    /// [`Blackboard::append_str`] or [`Blackboard::remove`] (0 for a key never written that way).
    pub fn write_count(&self, key: &str) -> u64 {
        self.writes.counts.get(key).copied().unwrap_or(0)
    }

    /// Reads a value by key.
    pub fn get(&self, key: &str) -> Option<&BbValue> {
        self.values.get(key)
    }

    /// Removes and returns a value by key.
    ///
    /// Records `key` when it was present.
    pub fn remove(&mut self, key: &str) -> Option<BbValue> {
        let prev = self.values.remove(key);
        if prev.is_some() {
            self.changes.insert(key.to_string());
            self.writes.bump(key);
        }
        prev
    }

    /// Takes the keys [`Blackboard::insert`], [`Blackboard::assign`], [`Blackboard::append_str`] and
    /// [`Blackboard::remove`] have changed since the last call.
    pub fn drain_changes(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.changes)
    }

    /// Whether `key` is present.
    pub fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// Reads a boolean, defaulting to `false`.
    pub fn get_bool(&self, key: &str) -> bool {
        matches!(self.get(key), Some(BbValue::Bool(true)))
    }

    /// Reads a float, defaulting to `0.0`.
    pub fn get_float(&self, key: &str) -> f32 {
        match self.get(key) {
            Some(BbValue::Float(v)) => *v,
            Some(BbValue::Int(v)) => *v as f32,
            _ => 0.0,
        }
    }

    /// Reads an integer, defaulting to `0`. A float is truncated toward zero and saturates at `i64::MIN`/`i64::MAX`; a non-finite float
    /// reads as the default `0` (use [`Blackboard::try_get_int`] to tell the cases apart).
    pub fn get_int(&self, key: &str) -> i64 {
        match self.get(key) {
            Some(BbValue::Int(v)) => *v,
            Some(BbValue::Float(v)) if v.is_finite() => *v as i64,
            _ => 0,
        }
    }

    /// Reads a float without loss: `None` when the key is absent, not numeric, a non-finite
    /// float, or an integer that `f32` cannot represent exactly.
    pub fn try_get_float(&self, key: &str) -> Option<f32> {
        match self.get(key)? {
            BbValue::Float(v) if v.is_finite() => Some(*v),
            BbValue::Int(v) => {
                let f = *v as f32;
                (f.is_finite() && f as i64 == *v && (f as i64) != i64::MAX).then_some(f)
            }
            _ => None,
        }
    }

    /// Reads an integer without loss: `None` when the key is absent, not numeric, or a float
    /// that is non-finite, fractional or outside the `i64` range.
    pub fn try_get_int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            BbValue::Int(v) => Some(*v),
            BbValue::Float(v) if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e18 => Some(*v as i64),
            _ => None,
        }
    }

    /// Reads a vector, defaulting to the origin.
    pub fn get_vec3(&self, key: &str) -> Vec3 {
        match self.get(key) {
            Some(BbValue::Vec3(v)) => *v,
            _ => Vec3::ZERO,
        }
    }

    /// Reads an entity id (bits), defaulting to [`Entity::PLACEHOLDER`] when the key is missing, not an
    /// entity, or holds bits that are not a valid entity encoding (e.g. loaded from a corrupt file).
    pub fn get_entity(&self, key: &str) -> Entity {
        match self.get(key) {
            // Bits from a hand-edited or stale save may not name a valid entity.
            Some(BbValue::Entity(bits)) => Entity::try_from_bits(*bits).unwrap_or(Entity::PLACEHOLDER),
            _ => Entity::PLACEHOLDER,
        }
    }

    /// Reads a string, defaulting to `""`.
    pub fn get_string(&self, key: &str) -> &str {
        match self.get(key) {
            Some(BbValue::String(v)) => v,
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_int_saturates_a_huge_float() {
        let mut bb = Blackboard::default();
        bb.insert("big".to_string(), 1.0e20_f32);
        assert_eq!(bb.get_int("big"), i64::MAX);
        assert_eq!(bb.try_get_int("big"), None);
    }

    #[test]
    fn typed_round_trips() {
        let mut bb = Blackboard::new();
        bb.insert("alive", true);
        bb.insert("health", 75.5f32);
        bb.insert("score", 12i32);
        bb.insert("name", "guard");
        bb.insert("home", Vec3::new(1.0, 0.0, 2.0));
        let e = Entity::try_from_bits(42).expect("valid entity");
        bb.insert("target", e);

        assert!(bb.get_bool("alive"));
        assert_eq!(bb.get_float("health"), 75.5);
        assert_eq!(bb.get_int("score"), 12);
        assert_eq!(bb.get_string("name"), "guard");
        assert_eq!(bb.get_vec3("home"), Vec3::new(1.0, 0.0, 2.0));
        assert_eq!(bb.get_entity("target"), e);
        // A stored 0 (a reflected u64 default) is not a valid entity: it reads as PLACEHOLDER.
        bb.values.insert("zero".to_string(), BbValue::Entity(0));
        assert_eq!(bb.get_entity("zero"), Entity::PLACEHOLDER);
        assert!(!bb.get_bool("missing"));
        assert_eq!(bb.get_float("missing"), 0.0);
    }

    #[test]
    fn invalid_entity_bits_read_as_placeholder_instead_of_panicking() {
        let mut bb = Blackboard::new();
        bb.insert("zero", BbValue::Entity(0));
        // Bits that do not decode to an entity (a zero index field). `u64::MAX` is a valid
        // encoding in bevy_ecs 0.20, so it is not used here.
        bb.insert("junk", BbValue::Entity(0xFFFF_FFFF_0000_0000));
        assert_eq!(bb.get_entity("zero"), Entity::PLACEHOLDER);
        assert_eq!(bb.get_entity("junk"), Entity::PLACEHOLDER);
    }

    #[test]
    fn an_invalid_entity_value_reads_as_the_placeholder() {
        let mut bb = Blackboard::new();
        bb.insert("target", BbValue::Entity(0));
        assert_eq!(bb.get_entity("target"), Entity::PLACEHOLDER);
    }

    #[test]
    fn replace_and_remove() {
        let mut bb = Blackboard::new();
        assert!(bb.insert("x", 1i32).is_none());
        assert!(bb.insert("x", 2i32).is_some());
        assert_eq!(bb.get_int("x"), 2);
        assert_eq!(bb.remove("x"), Some(BbValue::Int(2)));
        assert!(bb.remove("x").is_none());
    }

    #[test]
    fn changes_record_a_real_write_and_equality_ignores_them() {
        let mut bb = Blackboard::new();
        bb.insert("health", 1.0_f32);
        assert_eq!(
            bb.drain_changes().into_iter().collect::<Vec<_>>(),
            vec!["health".to_string()]
        );
        bb.insert("health", 1.0_f32);
        assert!(bb.drain_changes().is_empty(), "the same bits are not a change");
        bb.insert("health", f32::from_bits(0x7fc0_0001));
        bb.insert("health", f32::from_bits(0x7fc0_0001));
        let nan_changes = bb.drain_changes();
        assert_eq!(
            nan_changes.len(),
            1,
            "the first NaN is a change and the same bits are not"
        );
        bb.insert("gone", 1_i64);
        bb.remove("gone");
        assert_eq!(
            bb.drain_changes().into_iter().collect::<Vec<_>>(),
            vec!["gone".to_string()],
            "removing a key records it once"
        );
        let mut other = Blackboard::new();
        other.insert("health", f32::from_bits(0x7fc0_0001));
        let _ = other.drain_changes();
        assert_eq!(bb, other);
        bb.values.insert("sneak".into(), BbValue::Bool(true));
        assert!(
            bb.drain_changes().is_empty(),
            "a direct map write is not a notification"
        );
    }

    /// integ-v06h (ADR-7762): the logic graph's borrowed writers (post05w4-logic-graph-p4-alloc)
    /// feed the same change set the behavior tree observers drain (v0.6 line), so a BT and a logic
    /// graph sharing one blackboard see each other's writes.
    #[test]
    fn assign_and_append_record_changes_like_insert() {
        let mut bb = Blackboard::new();
        bb.assign("hp", &BbValue::Int(3));
        assert_eq!(
            bb.drain_changes().into_iter().collect::<Vec<_>>(),
            vec!["hp".to_string()],
            "a new key is a change"
        );
        bb.assign("hp", &BbValue::Int(3));
        assert!(bb.drain_changes().is_empty(), "the same value is not a change");
        assert_eq!(bb.write_count("hp"), 2, "but both are writes");
        bb.assign("hp", &BbValue::Int(4));
        bb.assign("hp", &BbValue::Int(5));
        assert_eq!(bb.drain_changes().len(), 1, "a changed key is recorded once");
        assert_eq!(bb.get_int("hp"), 5);
        bb.insert("log", "a");
        let _ = bb.drain_changes();
        assert!(bb.append_str("log", ""));
        assert!(bb.drain_changes().is_empty(), "an empty append changes nothing");
        assert!(bb.append_str("log", "b"));
        assert_eq!(
            bb.drain_changes().into_iter().collect::<Vec<_>>(),
            vec!["log".to_string()]
        );
        assert!(!bb.append_str("hp", "x"), "not a string: nothing written");
        assert!(bb.drain_changes().is_empty());
    }

    #[test]
    fn values_are_reflected_and_iterate_sorted() {
        let mut bb = Blackboard::new();
        bb.insert("b", 1i32);
        bb.insert("a", 2i32);
        let keys: Vec<String> = bb.values.keys().cloned().collect();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn non_finite_floats_read_as_the_default_int() {
        let mut bb = Blackboard::new();
        bb.insert("inf", f32::INFINITY);
        bb.insert("nan", f32::NAN);
        assert_eq!(bb.get_int("inf"), 0);
        assert_eq!(bb.get_int("nan"), 0);
        assert_eq!(bb.try_get_int("inf"), None);
        assert_eq!(bb.try_get_int("nan"), None);
    }

    #[test]
    fn checked_reads_reject_lossy_conversions() {
        let mut bb = Blackboard::new();
        bb.insert("exact", 1_000_000i64);
        bb.insert("lossy", 16_777_217i64);
        bb.insert("max", i64::MAX);
        bb.insert("half", 0.5f32);
        bb.insert("whole", 3.0f32);
        bb.insert("huge", 1.0e30f32);
        assert_eq!(bb.try_get_float("exact"), Some(1_000_000.0));
        assert_eq!(bb.try_get_float("lossy"), None);
        assert_eq!(bb.try_get_float("max"), None);
        assert_eq!(bb.try_get_float("missing"), None);
        assert_eq!(bb.try_get_int("half"), None);
        assert_eq!(bb.try_get_int("whole"), Some(3));
        assert_eq!(bb.try_get_int("huge"), None);
    }
}
