// SPDX-License-Identifier: MPL-2.0
//! Edge cases of the reflected (de)serializer found by the test-quality audit: float specials,
//! entity-keyed containers, bad binary variant indices, serialization-skipped fields, empty
//! values, and fixed-seed corruption sweeps that must never panic (`panic = abort` in shipping).

use std::collections::{HashMap, HashSet};

use regia_ecs::prelude::*;
use regia_ecs::serialize::{
    EntityIdMap, ReflectCodec, SerializeError, from_postcard, from_ron, from_ron_as, to_postcard, to_ron,
};

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Floats {
    a: f32,
    b: f64,
    list: Vec<f32>,
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
enum Shape {
    #[default]
    Empty,
    One(u16),
    Two(u8, String),
    Named {
        x: i32,
        y: i32,
    },
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Holder {
    shape: Shape,
    shapes: Vec<Shape>,
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Unit;

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Empties {
    text: String,
    list: Vec<u8>,
    map: HashMap<String, u8>,
    set: HashSet<u8>,
    opt: Option<Option<u8>>,
    unit: Unit,
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct WithSkip {
    kept: u32,
    #[reflect(skip_serializing)]
    runtime: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct EntityKeyed {
    map: HashMap<Entity, u8>,
    set: HashSet<Entity>,
    maybe: Option<Entity>,
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<Floats>();
    registry.register::<Holder>();
    registry.register::<Empties>();
    registry.register::<WithSkip>();
    registry.register::<EntityKeyed>();
    registry
}

#[test]
fn float_specials_round_trip_bit_exactly_in_ron_and_postcard() -> Result<(), SerializeError> {
    let registry = registry();
    let value = Floats {
        a: f32::INFINITY,
        b: f64::NEG_INFINITY,
        list: vec![f32::NAN, -0.0, f32::MIN_POSITIVE, f32::MAX, 5e-45],
    };
    let bits = |v: &Floats| -> Vec<u64> {
        let mut out = vec![u64::from(v.a.to_bits()), v.b.to_bits()];
        out.extend(v.list.iter().map(|f| u64::from(f.to_bits())));
        out
    };
    let text = to_ron(&value, &registry)?;
    let back: Floats = from_ron_as(&text, &registry)?;
    assert_eq!(bits(&back), bits(&value), "RON:\n{text}");
    // Re-serializing is byte identical even with NaN in the data.
    assert_eq!(to_ron(&back, &registry)?, text);

    let bytes = to_postcard(&value, &registry)?;
    let back = from_postcard(&bytes, &registry)?;
    let back = back
        .downcast_ref::<Floats>()
        .ok_or_else(|| SerializeError::NoTypeInfo("Floats".into()))?;
    assert_eq!(bits(back), bits(&value));
    assert_eq!(to_postcard(back, &registry)?, bytes, "postcard is deterministic");
    Ok(())
}

#[test]
fn every_enum_variant_shape_round_trips_in_both_formats() -> Result<(), SerializeError> {
    let registry = registry();
    let value = Holder {
        shape: Shape::Two(7, "x".into()),
        shapes: vec![
            Shape::Empty,
            Shape::One(65535),
            Shape::Two(0, String::new()),
            Shape::Named {
                x: i32::MIN,
                y: i32::MAX,
            },
        ],
    };
    let text = to_ron(&value, &registry)?;
    assert_eq!(from_ron_as::<Holder>(&text, &registry)?, value);
    let bytes = to_postcard(&value, &registry)?;
    let back = from_postcard(&bytes, &registry)?;
    assert_eq!(back.downcast_ref::<Holder>(), Some(&value));
    Ok(())
}

#[test]
fn enum_struct_variant_fills_missing_fields_from_the_field_default() -> Result<(), SerializeError> {
    let registry = registry();
    let text = r#"{"serialize_edges::Holder": (shape: Named(y: 4))}"#;
    let back: Holder = from_ron_as(text, &registry)?;
    assert_eq!(back.shape, Shape::Named { x: 0, y: 4 });
    Ok(())
}

#[test]
fn postcard_variant_index_out_of_range_is_an_error() -> Result<(), SerializeError> {
    let registry = registry();
    let value = Holder {
        shape: Shape::Empty,
        shapes: Vec::new(),
    };
    let mut bytes = to_postcard(&value, &registry)?;
    // Layout: type-path string, then `shape` (a varint variant index), then the empty list.
    let at = bytes.len() - 2;
    assert_eq!(bytes[at], 0, "variant index of Shape::Empty");
    for bad in [4u8, 5, 100] {
        bytes[at] = bad;
        assert!(from_postcard(&bytes, &registry).is_err(), "index {bad} accepted");
    }
    // A varint index beyond u32.
    let mut wide = bytes[..at].to_vec();
    wide.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x10, 0]);
    assert!(from_postcard(&wide, &registry).is_err());
    Ok(())
}

#[test]
fn unknown_and_misspelled_ron_variants_are_errors() {
    let registry = registry();
    for shape in [
        "Bogus",
        "empty",
        "One",
        "One(\"x\")",
        "Two(1)",
        "Named(x: \"no\")",
    ] {
        let text = format!("{{\"serialize_edges::Holder\": (shape: {shape})}}");
        assert!(from_ron(&text, &registry).is_err(), "accepted `{shape}`");
    }
}

#[test]
fn duplicate_field_in_ron_is_an_error() {
    let registry = registry();
    let text = r#"{"serialize_edges::WithSkip": (kept: 1, kept: 2)}"#;
    assert!(from_ron(text, &registry).is_err());
}

#[test]
fn empty_values_round_trip_in_both_formats() -> Result<(), SerializeError> {
    let registry = registry();
    let value = Empties {
        opt: Some(None),
        ..Empties::default()
    };
    let text = to_ron(&value, &registry)?;
    assert_eq!(from_ron_as::<Empties>(&text, &registry)?, value, "{text}");
    let bytes = to_postcard(&value, &registry)?;
    let back = from_postcard(&bytes, &registry)?;
    assert_eq!(back.downcast_ref::<Empties>(), Some(&value));

    // `Some(Some(_))` and `None` stay distinct from `Some(None)`.
    for opt in [None, Some(Some(3))] {
        let value = Empties {
            opt,
            ..Empties::default()
        };
        let text = to_ron(&value, &registry)?;
        assert_eq!(from_ron_as::<Empties>(&text, &registry)?, value, "{text}");
        let back = from_postcard(&to_postcard(&value, &registry)?, &registry)?;
        assert_eq!(back.downcast_ref::<Empties>(), Some(&value));
    }
    // An all-empty struct written with no fields at all loads as the default.
    let loaded: Empties = from_ron_as(r#"{"serialize_edges::Empties": ()}"#, &registry)?;
    assert_eq!(loaded, Empties::default());
    Ok(())
}

#[test]
fn serialization_skipped_fields_take_their_default_in_both_formats() -> Result<(), SerializeError> {
    let registry = registry();
    let value = WithSkip { kept: 5, runtime: 99 };
    let text = to_ron(&value, &registry)?;
    assert!(!text.contains("runtime"), "{text}");
    let back: WithSkip = from_ron_as(&text, &registry)?;
    assert_eq!(back, WithSkip { kept: 5, runtime: 0 });
    let bytes = to_postcard(&value, &registry)?;
    let back = from_postcard(&bytes, &registry)?;
    assert_eq!(
        back.downcast_ref::<WithSkip>(),
        Some(&WithSkip { kept: 5, runtime: 0 })
    );
    // The skipped field never reaches the bytes: type path, then one varint.
    let marker = WithSkip { kept: 5, runtime: 1 };
    assert_eq!(to_postcard(&marker, &registry)?, bytes);
    Ok(())
}

/// Entities spawned in a different order get the same stable ids: the text must not depend on the
/// raw entity bits, or two runs of the same scene would write different files.
#[test]
fn entity_keyed_containers_are_ordered_by_stable_id_not_raw_bits() -> Result<(), SerializeError> {
    let registry = registry();
    let ids: [u128; 4] = [0x40, 0x10, 0x30, 0x20];
    let build = |spawn_order: &[usize]| -> Result<String, SerializeError> {
        let mut world = World::new();
        let mut entities = [Entity::PLACEHOLDER; 4];
        for &i in spawn_order {
            entities[i] = world.spawn_empty().id();
        }
        let mut map = EntityIdMap::new();
        for (entity, id) in entities.iter().zip(ids) {
            map.insert(*entity, id);
        }
        let value = EntityKeyed {
            map: entities.iter().enumerate().map(|(i, e)| (*e, i as u8)).collect(),
            set: entities.iter().copied().collect(),
            maybe: Some(entities[2]),
        };
        let text = ReflectCodec::new(&registry)
            .entity_mapper(&mut map)
            .to_ron(&value)?;
        assert_eq!(map.misses(), 0);
        Ok(text)
    };
    let forward = build(&[0, 1, 2, 3])?;
    let backward = build(&[3, 2, 1, 0])?;
    assert_eq!(forward, backward, "text depends on entity allocation order");
    let positions: Vec<usize> = ["0010", "0020", "0030", "0040"]
        .iter()
        .map(|id| forward.find(&format!("{id}\": ")).unwrap_or(usize::MAX))
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "map keys not in id order: {positions:?}\n{forward}"
    );
    Ok(())
}

#[test]
fn malformed_entity_ids_are_errors_when_a_mapper_is_present() {
    let registry = registry();
    for bad in ["\"zz\"", "\"\"", "\"-1\"", "1.5", "\"1 2\"", "true"] {
        let text = format!("{{\"serialize_edges::EntityKeyed\": (map: {{}}, set: [], maybe: Some({bad}))}}");
        let mut map = EntityIdMap::new();
        let result = ReflectCodec::new(&registry)
            .entity_mapper(&mut map)
            .from_ron(&text);
        assert!(result.is_err(), "accepted entity id {bad}");
    }
    // 33 hex digits do not fit a u128.
    let text = format!(
        "{{\"serialize_edges::EntityKeyed\": (map: {{}}, set: [], maybe: Some(\"{}\"))}}",
        "f".repeat(33)
    );
    let mut map = EntityIdMap::new();
    assert!(
        ReflectCodec::new(&registry)
            .entity_mapper(&mut map)
            .from_ron(&text)
            .is_err()
    );
}

#[test]
fn short_and_unprefixed_entity_ids_parse_as_hex() -> Result<(), SerializeError> {
    let registry = registry();
    let mut world = World::new();
    let entity = world.spawn_empty().id();
    let mut map = EntityIdMap::new();
    map.insert(entity, 0xAB);
    let text = r#"{"serialize_edges::EntityKeyed": (map: {}, set: [], maybe: Some("ab"))}"#;
    let loaded = ReflectCodec::new(&registry)
        .entity_mapper(&mut map)
        .from_ron(text)?;
    let loaded = loaded
        .into_value()
        .ok_or_else(|| SerializeError::NoTypeInfo("EntityKeyed".into()))?;
    let loaded = loaded
        .downcast_ref::<EntityKeyed>()
        .ok_or_else(|| SerializeError::NoTypeInfo("EntityKeyed".into()))?;
    assert_eq!(loaded.maybe, Some(entity));
    assert_eq!(map.misses(), 0);
    Ok(())
}

/// Deterministic xorshift, so the sweeps below are identical on every run and platform.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn big_holder() -> Holder {
    Holder {
        shape: Shape::Named { x: -3, y: 9 },
        shapes: vec![Shape::One(3), Shape::Two(1, "hello".into()), Shape::Empty],
    }
}

#[test]
fn every_ron_prefix_and_fixed_seed_byte_flips_load_without_panicking() -> Result<(), SerializeError> {
    let registry = registry();
    let text = to_ron(&big_holder(), &registry)?;
    for cut in 0..text.len() {
        if text.is_char_boundary(cut) {
            // Only the complete text parses; a prefix that happens to parse is fine too.
            let _ = from_ron(&text[..cut], &registry);
        }
    }
    assert!(from_ron(&text, &registry).is_ok());
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..2000 {
        let mut bytes = text.clone().into_bytes();
        let at = (xorshift(&mut state) % bytes.len() as u64) as usize;
        bytes[at] = (xorshift(&mut state) & 0x7F) as u8;
        if let Ok(mutated) = String::from_utf8(bytes) {
            let _ = from_ron(&mutated, &registry);
        }
    }
    Ok(())
}

#[test]
fn every_postcard_prefix_and_fixed_seed_byte_flips_load_without_panicking() -> Result<(), SerializeError> {
    let registry = registry();
    let bytes = to_postcard(&big_holder(), &registry)?;
    for cut in 0..bytes.len() {
        assert!(
            from_postcard(&bytes[..cut], &registry).is_err(),
            "accepted a {cut}-byte prefix of {}",
            bytes.len()
        );
    }
    let mut state = 0xD1B5_4A32_D192_ED03u64;
    for _ in 0..4000 {
        let mut mutated = bytes.clone();
        for _ in 0..1 + xorshift(&mut state) % 3 {
            let at = (xorshift(&mut state) % mutated.len() as u64) as usize;
            mutated[at] = xorshift(&mut state) as u8;
        }
        let _ = from_postcard(&mutated, &registry);
    }
    Ok(())
}

#[test]
fn identical_values_serialize_identically_across_repeated_runs() -> Result<(), SerializeError> {
    let registry = registry();
    let value = Empties {
        map: (0..64u8).map(|i| (format!("key{i}"), i)).collect(),
        set: (0..64u8).collect(),
        ..Empties::default()
    };
    let first = to_ron(&value, &registry)?;
    let first_bin = to_postcard(&value, &registry)?;
    for _ in 0..20 {
        // A freshly built map has a fresh hasher seed, hence a fresh iteration order.
        let rebuilt = Empties {
            map: value.map.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            set: value.set.iter().copied().collect(),
            ..Empties::default()
        };
        assert_eq!(to_ron(&rebuilt, &registry)?, first);
        assert_eq!(to_postcard(&rebuilt, &registry)?, first_bin);
    }
    Ok(())
}

/// `Eq` and `Hash` on the raw bits, like a save-stable float wrapper: `+0.0` and `-0.0` are
/// distinct keys that hash apart, while the reflected ordering (IEEE `==`) calls them `Equal`.
#[derive(Reflect, Debug, Clone, Default)]
#[reflect(PartialEq)]
struct BitsKey {
    n: f32,
}

impl PartialEq for BitsKey {
    fn eq(&self, other: &Self) -> bool {
        self.n.to_bits() == other.n.to_bits()
    }
}

impl Eq for BitsKey {}

impl std::hash::Hash for BitsKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.n.to_bits().hash(state);
    }
}

/// `Eq` on the float `==`, so two NaN payloads are distinct keys (NaN != NaN) with no reflected
/// ordering and identical RON text (`NaN`).
#[derive(Reflect, Debug, Clone, Default)]
#[reflect(PartialEq)]
struct NanKey {
    n: f32,
}

impl PartialEq for NanKey {
    fn eq(&self, other: &Self) -> bool {
        self.n == other.n
    }
}

impl Eq for NanKey {}

impl std::hash::Hash for NanKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.n.to_bits().hash(state);
    }
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct BitsKeyed {
    by_key: HashMap<BitsKey, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct NanKeyed {
    by_key: HashMap<NanKey, u32>,
}

/// Map keys that tie under the reflected ordering (`Equal`, or no ordering like NaN) but are
/// distinct keys must not keep the `HashMap` iteration order: a rebuilt map has a fresh hasher
/// seed, so a tie that survives the sort flips the written order between two runs of the same
/// value (a second `snapshot_scene` or save would then differ from the first).
#[test]
fn map_key_ties_serialize_identically_across_rebuilt_maps() -> Result<(), SerializeError> {
    let mut registry = registry();
    registry.register::<BitsKeyed>();
    registry.register::<NanKeyed>();

    // `+0.0` vs `-0.0`: `reflect_partial_cmp` says Equal, the canonical RON text differs.
    let signed_zero = BitsKeyed {
        by_key: [(BitsKey { n: 0.0 }, 1), (BitsKey { n: -0.0 }, 2)]
            .into_iter()
            .collect(),
    };
    let first = to_ron(&signed_zero, &registry)?;
    let first_bin = to_postcard(&signed_zero, &registry)?;
    for _ in 0..20 {
        let rebuilt = BitsKeyed {
            by_key: signed_zero.by_key.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        };
        assert_eq!(
            to_ron(&rebuilt, &registry)?,
            first,
            "+0.0 vs -0.0 key order flipped"
        );
        assert_eq!(
            to_postcard(&rebuilt, &registry)?,
            first_bin,
            "+0.0 vs -0.0 key order flipped (postcard)"
        );
    }

    // Two NaN payloads: no reflected ordering and identical RON text `NaN` for both keys.
    let nan_payloads = NanKeyed {
        by_key: [
            (
                NanKey {
                    n: f32::from_bits(0x7fc0_0001),
                },
                1,
            ),
            (
                NanKey {
                    n: f32::from_bits(0x7fc0_0002),
                },
                2,
            ),
        ]
        .into_iter()
        .collect(),
    };
    let first = to_ron(&nan_payloads, &registry)?;
    let first_bin = to_postcard(&nan_payloads, &registry)?;
    for _ in 0..20 {
        let rebuilt = NanKeyed {
            by_key: nan_payloads.by_key.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        };
        assert_eq!(
            to_ron(&rebuilt, &registry)?,
            first,
            "NaN payload key order flipped"
        );
        assert_eq!(
            to_postcard(&rebuilt, &registry)?,
            first_bin,
            "NaN payload key order flipped (postcard)"
        );
    }
    Ok(())
}

/// `Eq`/`Hash` on the bits and no `PartialOrd`: the reflected ordering is the derived field-wise
/// one, numeric for finite values and `None` against a NaN (audit sprint-audit-g-ecs-correctness
/// finding 4).
#[derive(Reflect, Debug, Clone, Copy, Default)]
#[reflect(PartialEq)]
struct CycleKey {
    n: f32,
}

impl PartialEq for CycleKey {
    fn eq(&self, other: &Self) -> bool {
        self.n.to_bits() == other.n.to_bits()
    }
}

impl Eq for CycleKey {}

impl std::hash::Hash for CycleKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.n.to_bits().hash(state);
    }
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct CycleSet {
    items: HashSet<CycleKey>,
    by_key: HashMap<CycleKey, u32>,
}

/// `-inf < -1.0` by the reflected ordering, `-1.0 < -NaN` and `-NaN < -inf` by RON text: mixing
/// the two is a cycle, so the written order depended on `HashSet` iteration order (and a
/// comparator that is not a total order may make `sort_by` panic, an abort in shipping).
#[test]
fn keys_whose_reflected_ordering_is_partial_sort_canonically() -> Result<(), SerializeError> {
    let mut registry = registry();
    registry.register::<CycleSet>();
    let keys = [
        CycleKey { n: f32::NEG_INFINITY },
        CycleKey { n: -1.0 },
        CycleKey { n: -f32::NAN },
        CycleKey { n: 3.0 },
        CycleKey { n: f32::NAN },
        CycleKey { n: -0.0 },
        CycleKey { n: 0.0 },
        CycleKey { n: f32::INFINITY },
    ];
    let build = |order: &[CycleKey]| CycleSet {
        items: order.iter().copied().collect(),
        by_key: order.iter().map(|k| (*k, k.n.to_bits() % 7)).collect(),
    };
    // Fresh hasher seeds each round, so every round iterates the keys in a different order.
    for set in [&keys[..], &keys[..3]] {
        let first = to_ron(&build(set), &registry)?;
        let first_bin = to_postcard(&build(set), &registry)?;
        let set_rev: Vec<CycleKey> = set.iter().rev().copied().collect();
        for round in 0..40 {
            let order: &[CycleKey] = if round % 2 == 0 { &set_rev } else { set };
            let value = build(order);
            assert_eq!(
                to_ron(&value, &registry)?,
                first,
                "{} keys, round {round}",
                set.len()
            );
            assert_eq!(
                to_postcard(&value, &registry)?,
                first_bin,
                "{} keys, round {round} (postcard)",
                set.len()
            );
        }
    }
    Ok(())
}
