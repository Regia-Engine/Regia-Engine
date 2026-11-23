// SPDX-License-Identifier: MPL-2.0
//! Serialization side: canonical ordering of maps and sets, `Entity` → stable id.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;

use bevy_ecs::entity::Entity;
use bevy_reflect::serde::{ReflectSerializerProcessor, TypedReflectSerializer};
use bevy_reflect::{PartialReflect, ReflectRef, TypeRegistry};
use serde::ser::{self, SerializeMap, SerializeSeq, Serializer};

use super::EntityIdMapper;

/// Serializer processor: sorts map entries and set items, maps entities to stable ids.
pub(super) struct CanonicalSer<'m, 'a> {
    pub mapper: Option<RefCell<&'m mut (dyn EntityIdMapper + 'a)>>,
    /// True while a key or value is serialized only to decide order.
    pub ordering: Cell<bool>,
}

/// A map key or set item with the canonical forms used to break a reflected tie.
struct SortKey<'v, T> {
    value: &'v dyn PartialReflect,
    payload: T,
    /// Compact RON of the key.
    text: String,
    /// postcard of the key.
    bytes: Vec<u8>,
    /// Compact RON of the map value, computed only when another key ties on `text` and `bytes`.
    /// `None` for a set item and until a tie needs it.
    payload_text: Option<String>,
    /// postcard of the map value, computed only when another key ties on `text` and `bytes`.
    /// `None` for a set item and until a tie needs it.
    payload_bytes: Option<Vec<u8>>,
}

/// Map values contribute their canonical form to a tie. Set items have none.
trait PayloadOrder {
    fn reflect_for_order(&self) -> Option<&dyn PartialReflect>;
}

impl PayloadOrder for () {
    fn reflect_for_order(&self) -> Option<&dyn PartialReflect> {
        None
    }
}

impl PayloadOrder for &dyn PartialReflect {
    fn reflect_for_order(&self) -> Option<&dyn PartialReflect> {
        Some(*self)
    }
}

impl<T: PartialReflect> PayloadOrder for &T {
    fn reflect_for_order(&self) -> Option<&dyn PartialReflect> {
        Some(*self)
    }
}

/// Restores the ordering flag if a probe returns early or nests.
struct OrderProbe<'a> {
    flag: &'a Cell<bool>,
    previous: bool,
}

impl<'a> OrderProbe<'a> {
    fn enter(flag: &'a Cell<bool>) -> Self {
        let previous = flag.get();
        flag.set(true);
        Self { flag, previous }
    }
}

impl Drop for OrderProbe<'_> {
    fn drop(&mut self) {
        self.flag.set(self.previous);
    }
}

fn entry_order<T>(a: &SortKey<'_, T>, b: &SortKey<'_, T>) -> Ordering {
    a.text
        .cmp(&b.text)
        .then_with(|| a.bytes.cmp(&b.bytes))
        .then_with(|| payload_text(a).cmp(payload_text(b)))
        .then_with(|| payload_bytes(a).cmp(payload_bytes(b)))
}

/// Value forms take part in the order only once a key ties on its own forms; a set item (which
/// has no value) and a key whose forms were not needed compare as empty, like an empty value.
fn payload_text<'k, 'v, T>(key: &'k SortKey<'v, T>) -> &'k str {
    key.payload_text.as_deref().unwrap_or("")
}

/// Value forms take part in the order only once a key ties on its own forms; a set item (which
/// has no value) and a key whose forms were not needed compare as empty, like an empty value.
fn payload_bytes<'k, 'v, T>(key: &'k SortKey<'v, T>) -> &'k [u8] {
    key.payload_bytes.as_deref().unwrap_or(&[])
}

impl CanonicalSer<'_, '_> {
    fn sort_key<'v, T, E: ser::Error>(
        &self,
        value: &'v dyn PartialReflect,
        payload: T,
        registry: &TypeRegistry,
    ) -> Result<SortKey<'v, T>, E>
    where
        T: PayloadOrder,
    {
        note_sort_text();
        // Probes must not count entity misses. The flag stays set for nested maps, whose entry
        // writes are still only building the text this sort compares.
        let _probe = OrderProbe::enter(&self.ordering);
        let typed = TypedReflectSerializer::with_processor(value, registry, self);
        let text = ron::to_string(&typed).map_err(|e| E::custom(format_args!("map key or set item: {e}")))?;
        let bytes =
            postcard::to_stdvec(&typed).map_err(|e| E::custom(format_args!("map key or set item: {e}")))?;
        Ok(SortKey {
            value,
            payload,
            text,
            bytes,
            payload_text: None,
            payload_bytes: None,
        })
    }

    /// Serializes the map value's canonical forms into a key whose `text` and `bytes` tied with
    /// another key's, so the value is part of the order. Never called for a set item (which has
    /// no value) or when no tie needed the value forms.
    fn fill_payload_forms<T, E: ser::Error>(
        &self,
        key: &mut SortKey<'_, T>,
        registry: &TypeRegistry,
    ) -> Result<(), E>
    where
        T: PayloadOrder,
    {
        if let Some(payload_value) = key.payload.reflect_for_order() {
            let typed = TypedReflectSerializer::with_processor(payload_value, registry, self);
            let text = ron::to_string(&typed).map_err(|e| E::custom(format_args!("map value: {e}")))?;
            let bytes = postcard::to_stdvec(&typed).map_err(|e| E::custom(format_args!("map value: {e}")))?;
            key.payload_text = Some(text);
            key.payload_bytes = Some(bytes);
        }
        Ok(())
    }

    /// Puts map keys or set items in canonical order.
    ///
    /// The order is the reflected ordering of the keys when it says `Less`/`Greater`, then their
    /// RON text, then their postcard bytes, then the map value's RON text and postcard bytes (a
    /// set item has no value). When the reflected ordering is partial (a NaN field gives `None`),
    /// that mix can be cyclic. The entries are first put in text/bytes order, which depends only
    /// on the entries, never on `HashMap` iteration order; the value forms are then serialized
    /// only for the runs that tied on text/bytes (so ordering a map costs one key encoding per
    /// entry, not two value encodings more per entry, each of which would re-sort every map
    /// nested in the value); and the full text/bytes/value order is applied before a stable
    /// merge sort (`merge_sort_by`), which never panics on an inconsistent comparator, applies
    /// the mixed order. The merge output is a fixed function of its input; when the mixed order
    /// is total (the usual case), it is exactly that order.
    fn sort_keys<T, E: ser::Error>(
        &self,
        keys: &mut Vec<SortKey<'_, T>>,
        registry: &TypeRegistry,
    ) -> Result<(), E>
    where
        T: PayloadOrder,
    {
        keys.sort_by(|a, b| a.text.cmp(&b.text).then_with(|| a.bytes.cmp(&b.bytes)));
        // Value probes must not count entity misses or assign ids, like the key probes in
        // `sort_key`.
        let _probe = OrderProbe::enter(&self.ordering);
        // Serialize value forms only for the runs that tied on text/bytes: only there is the
        // value part of the order.
        let mut run_start = 0;
        while run_start < keys.len() {
            let mut run_end = run_start + 1;
            while run_end < keys.len()
                && keys[run_end].text == keys[run_start].text
                && keys[run_end].bytes == keys[run_start].bytes
            {
                run_end += 1;
            }
            if run_end - run_start > 1 {
                for key in &mut keys[run_start..run_end] {
                    self.fill_payload_forms(key, registry)?;
                }
            }
            run_start = run_end;
        }
        // Re-apply the full order now that the value forms it may need exist.
        keys.sort_by(entry_order);
        merge_sort_by(keys, |a, b| match a.value.reflect_partial_cmp(b.value) {
            Some(Ordering::Less) => Ordering::Less,
            Some(Ordering::Greater) => Ordering::Greater,
            Some(Ordering::Equal) | None => entry_order(a, b),
        });
        Ok(())
    }
}

/// Stable bottom-up merge sort. Unlike `slice::sort_by`, it never panics when `compare` is not a
/// total order, and for a given input it always produces the same output.
fn merge_sort_by<T, F: FnMut(&T, &T) -> Ordering>(items: &mut Vec<T>, mut compare: F) {
    let len = items.len();
    if len < 2 {
        return;
    }
    let mut src: Vec<Option<T>> = items.drain(..).map(Some).collect();
    let mut dst: Vec<Option<T>> = Vec::with_capacity(len);
    let mut width = 1;
    while width < len {
        let mut start = 0;
        while start < len {
            let mid = (start + width).min(len);
            let end = (start + 2 * width).min(len);
            let (mut i, mut j) = (start, mid);
            while i < mid && j < end {
                let take_right = match (&src[i], &src[j]) {
                    (Some(left), Some(right)) => compare(right, left) == Ordering::Less,
                    _ => false,
                };
                if take_right {
                    dst.push(src[j].take());
                    j += 1;
                } else {
                    dst.push(src[i].take());
                    i += 1;
                }
            }
            dst.extend(src[i..mid].iter_mut().map(Option::take));
            dst.extend(src[j..end].iter_mut().map(Option::take));
            start = end;
        }
        std::mem::swap(&mut src, &mut dst);
        dst.clear();
        width *= 2;
    }
    items.extend(src.into_iter().flatten());
}

impl ReflectSerializerProcessor for CanonicalSer<'_, '_> {
    fn try_serialize<S>(
        &self,
        value: &dyn PartialReflect,
        registry: &TypeRegistry,
        serializer: S,
    ) -> Result<Result<S::Ok, S>, S::Error>
    where
        S: Serializer,
    {
        if let Some(mapper) = &self.mapper
            && let Some(entity) = value.try_downcast_ref::<Entity>()
        {
            let id = {
                let mut mapper = mapper.borrow_mut();
                if self.ordering.get() {
                    mapper.entity_id_for_order(*entity)
                } else {
                    mapper.entity_to_id(*entity)
                }
            };
            return if serializer.is_human_readable() {
                serializer.serialize_str(&format!("{id:032x}")).map(Ok)
            } else {
                serializer.serialize_u128(id).map(Ok)
            };
        }
        match value.reflect_ref() {
            ReflectRef::Map(map) => {
                let mut entries = map
                    .iter()
                    .map(|(key, value)| self.sort_key::<_, S::Error>(key, value, registry))
                    .collect::<Result<Vec<_>, _>>()?;
                self.sort_keys::<_, S::Error>(&mut entries, registry)?;
                let mut out = serializer.serialize_map(Some(entries.len()))?;
                for entry in entries {
                    out.serialize_entry(
                        &TypedReflectSerializer::with_processor(entry.value, registry, self),
                        &TypedReflectSerializer::with_processor(entry.payload, registry, self),
                    )?;
                }
                out.end().map(Ok)
            }
            ReflectRef::Set(set) => {
                let mut items = set
                    .iter()
                    .map(|item| self.sort_key::<_, S::Error>(item, (), registry))
                    .collect::<Result<Vec<_>, _>>()?;
                self.sort_keys::<_, S::Error>(&mut items, registry)?;
                let mut out = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    out.serialize_element(&TypedReflectSerializer::with_processor(
                        item.value, registry, self,
                    ))?;
                }
                out.end().map(Ok)
            }
            _ => Ok(Err(serializer)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sort_matches_a_stable_sort_for_a_total_order() {
        // Pairs sorted by the first item only: equal keys must keep their input order.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut items: Vec<(u8, usize)> = (0..257)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state % 16) as u8, i)
            })
            .collect();
        let mut expected = items.clone();
        expected.sort_by_key(|(key, _)| *key);
        merge_sort_by(&mut items, |a, b| a.0.cmp(&b.0));
        assert_eq!(items, expected);
        for len in 0..4 {
            let mut short: Vec<u8> = (0..len).rev().collect();
            merge_sort_by(&mut short, u8::cmp);
            assert_eq!(short, (0..len).collect::<Vec<u8>>());
        }
    }

    #[test]
    fn merge_sort_is_deterministic_and_total_under_a_cyclic_comparator() {
        // Rock-paper-scissors: no total order exists. The sort must return every item, never
        // panic, and give the same output for the same input every time.
        let beats = |a: &u8, b: &u8| match (a % 3, b % 3) {
            (x, y) if x == y => a.cmp(b),
            (0, 2) | (1, 0) | (2, 1) => Ordering::Less,
            _ => Ordering::Greater,
        };
        let input: Vec<u8> = (0..64).map(|i| (i * 37 % 64) as u8).collect();
        let mut first = input.clone();
        merge_sort_by(&mut first, beats);
        for _ in 0..8 {
            let mut again = input.clone();
            merge_sort_by(&mut again, beats);
            assert_eq!(again, first);
        }
        let mut seen = first.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..64).collect::<Vec<u8>>());
    }

    /// Two map keys that serialize to the same bytes (they differ only in `#[reflect(ignore)]`
    /// state) must come out in value order, for either input order. Reflected `Less`/`Greater`
    /// still wins; this is the tie the text/bytes fallback claimed could not happen.
    #[test]
    fn ignored_state_does_not_let_map_order_follow_insertion() -> Result<(), ron::Error> {
        use bevy_reflect::Reflect;

        #[derive(Reflect, Debug, Clone)]
        #[reflect(PartialEq, Hash)]
        struct SaltKey {
            id: u32,
            #[reflect(ignore)]
            salt: u32,
        }
        impl PartialEq for SaltKey {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id && self.salt == other.salt
            }
        }
        impl Eq for SaltKey {}
        impl std::hash::Hash for SaltKey {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.id.hash(state);
                self.salt.hash(state);
            }
        }

        let mut registry = TypeRegistry::default();
        registry.register::<SaltKey>();
        registry.register::<String>();
        let ser = CanonicalSer {
            mapper: None,
            ordering: Cell::new(false),
        };
        let low = SaltKey { id: 1, salt: 10 };
        let high = SaltKey { id: 1, salt: 20 };
        let a = "a".to_string();
        let b = "b".to_string();
        let key_low = ser.sort_key::<_, ron::Error>(&low, &a, &registry)?;
        let key_high = ser.sort_key::<_, ron::Error>(&high, &b, &registry)?;
        assert_eq!(
            key_low.text, key_high.text,
            "ignored salt is part of the key text, so text order is already total"
        );
        assert_eq!(
            key_low.bytes, key_high.bytes,
            "ignored salt is part of the key bytes, so postcard order is already total"
        );
        let reflected = low.reflect_partial_cmp(&high);
        assert!(
            matches!(reflected, Some(Ordering::Equal) | None),
            "reflected order already separates these keys: {reflected:?}"
        );

        let mut forward = vec![
            ser.sort_key::<_, ron::Error>(&low, &a, &registry)?,
            ser.sort_key::<_, ron::Error>(&high, &b, &registry)?,
        ];
        let mut reverse = vec![
            ser.sort_key::<_, ron::Error>(&high, &b, &registry)?,
            ser.sort_key::<_, ron::Error>(&low, &a, &registry)?,
        ];
        ser.sort_keys::<_, ron::Error>(&mut forward, &registry)?;
        ser.sort_keys::<_, ron::Error>(&mut reverse, &registry)?;
        let forward: Vec<&str> = forward.iter().map(|key| key.payload.as_str()).collect();
        let reverse: Vec<&str> = reverse.iter().map(|key| key.payload.as_str()).collect();
        assert_eq!(
            forward, reverse,
            "same entries, opposite insertion order, different canonical order"
        );
        assert_eq!(forward, ["a", "b"]);
        Ok(())
    }

    /// A reflected `Less`/`Greater` still decides the order. Value text must not reorder integer keys.
    #[test]
    fn reflected_key_order_beats_value_text() -> Result<(), ron::Error> {
        let mut registry = TypeRegistry::default();
        registry.register::<u32>();
        registry.register::<String>();
        let ser = CanonicalSer {
            mapper: None,
            ordering: Cell::new(false),
        };
        // Key 2 carries the alphabetically first value, so a value-text tie-break would put it
        // first. Reflected numeric order must still put key 1 first.
        let on_one = "b".to_string();
        let on_two = "a".to_string();
        let mut keys = vec![
            ser.sort_key::<_, ron::Error>(&2_u32, &on_two, &registry)?,
            ser.sort_key::<_, ron::Error>(&1_u32, &on_one, &registry)?,
        ];
        ser.sort_keys::<_, ron::Error>(&mut keys, &registry)?;
        let order: Vec<&str> = keys.iter().map(|key| key.payload.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static SORT_TEXT_CALLS: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
fn note_sort_text() {
    SORT_TEXT_CALLS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(not(test))]
fn note_sort_text() {}

/// How many map keys or set items were encoded (RON text plus postcard bytes) for ordering since
/// the last take.
#[cfg(test)]
pub(super) fn take_sort_text_calls() -> u32 {
    SORT_TEXT_CALLS.with(|count| count.replace(0))
}
