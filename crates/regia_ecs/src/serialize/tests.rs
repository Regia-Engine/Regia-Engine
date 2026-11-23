// SPDX-License-Identifier: MPL-2.0
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use glam::{Quat, Vec3};

use super::*;
use crate::prelude::*;

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Inner {
    label: String,
    weight: f64,
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
enum Mode {
    #[default]
    Idle,
    Patrol(u32),
    Pair(i8, bool),
    Chase {
        speed: f32,
        target: Inner,
    },
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default, PartialEq)]
struct Everything {
    name: String,
    flag: bool,
    count: u64,
    small: i16,
    ratio: f32,
    position: Vec3,
    rotation: Quat,
    tags: Vec<String>,
    scores: HashMap<String, i32>,
    ids: HashSet<u32>,
    maybe: Option<u32>,
    nothing: Option<Inner>,
    inner: Inner,
    inners: Vec<Inner>,
    mode: Mode,
    modes: Vec<Mode>,
    pair: (u8, char),
    grid: [i32; 3],
}

impl Default for Everything {
    fn default() -> Self {
        Self {
            name: String::new(),
            flag: false,
            count: 0,
            small: 0,
            ratio: 0.5,
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            tags: Vec::new(),
            scores: HashMap::new(),
            ids: HashSet::new(),
            maybe: None,
            nothing: None,
            inner: Inner::default(),
            inners: Vec::new(),
            mode: Mode::Idle,
            modes: Vec::new(),
            pair: (0, 'a'),
            grid: [0; 3],
        }
    }
}

fn sample() -> Everything {
    Everything {
        name: "door \"north\"\n".into(),
        flag: true,
        count: u64::MAX,
        small: -12,
        ratio: 0.1,
        position: Vec3::new(1.5, -2.0, 1e-7),
        rotation: Quat::from_xyzw(0.0, 0.70710677, 0.0, 0.70710677),
        tags: vec!["a".into(), "b".into()],
        scores: (0..40).map(|i| (format!("k{i}"), i * 3 - 7)).collect(),
        ids: (0..30).map(|i| i * 7919 % 1000).collect(),
        maybe: Some(3),
        nothing: None,
        inner: Inner {
            label: "x".into(),
            weight: 1.0 / 3.0,
        },
        inners: vec![
            Inner::default(),
            Inner {
                label: "y".into(),
                weight: -0.0,
            },
        ],
        mode: Mode::Chase {
            speed: 3.25,
            target: Inner {
                label: "player".into(),
                weight: 80.0,
            },
        },
        modes: vec![Mode::Idle, Mode::Patrol(4), Mode::Pair(-1, true)],
        pair: (255, 'ß'),
        grid: [1, -2, 3],
    }
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<Everything>();
    registry
}

#[test]
fn ron_round_trip_representative_types() -> Result<(), SerializeError> {
    let registry = registry();
    let value = sample();
    let text = to_ron(&value, &registry)?;
    assert!(text.starts_with("{\n    \""), "type-path keyed: {text}");
    let back: Everything = from_ron_as(&text, &registry)?;
    assert_eq!(back, value);
    Ok(())
}

#[test]
fn postcard_round_trip_representative_types() -> Result<(), SerializeError> {
    let registry = registry();
    let value = sample();
    let bytes = to_postcard(&value, &registry)?;
    let back = from_postcard(&bytes, &registry)?;
    assert_eq!(back.downcast_ref::<Everything>(), Some(&value));
    let registration = registry
        .get(std::any::TypeId::of::<Everything>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Everything".into()))?;
    let mut codec = ReflectCodec::new(&registry);
    let bare = codec.to_postcard_typed(&value)?;
    let back = codec.from_postcard_typed(&bare, registration)?;
    assert_eq!(back.downcast_ref::<Everything>(), Some(&value));
    assert!(matches!(
        from_postcard(&[bytes.as_slice(), &[0]].concat(), &registry),
        Err(SerializeError::TrailingBytes(1))
    ));
    Ok(())
}

#[test]
fn canonical_ron_is_byte_identical_and_order_independent() -> Result<(), SerializeError> {
    let registry = registry();
    let value = sample();
    let first = to_ron(&value, &registry)?;
    let back: Everything = from_ron_as(&first, &registry)?;
    let second = to_ron(&back, &registry)?;
    assert_eq!(first, second);

    // Same map contents inserted in the opposite order → same text.
    let mut reordered = value.clone();
    let mut scores: Vec<_> = value.scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
    scores.sort();
    reordered.scores = HashMap::new();
    reordered.scores.extend(scores.into_iter().rev());
    let mut ids: Vec<_> = value.ids.iter().copied().collect();
    ids.sort_unstable();
    reordered.ids = HashSet::new();
    reordered.ids.extend(ids.into_iter().rev());
    assert_eq!(to_ron(&reordered, &registry)?, first);

    // Shortest round-trip floats; integer-valued floats keep a `.0`.
    assert!(first.contains("ratio: 0.1,"), "{first}");
    assert!(first.contains("weight: 80.0,"), "{first}");
    assert!(first.contains("(1.5, -2.0, 0.0000001)"), "{first}");
    // Map keys sorted.
    let k0 = first.find("\"k0\"").unwrap_or(usize::MAX);
    let k1 = first.find("\"k1\"").unwrap_or(usize::MAX);
    let k10 = first.find("\"k10\"").unwrap_or(usize::MAX);
    assert!(k0 < k1 && k1 < k10, "{first}");
    // Sets sorted numerically.
    let ids: Vec<u32> = {
        let mut ids: Vec<u32> = value.ids.iter().copied().collect();
        ids.sort_unstable();
        ids
    };
    let ids_text = ids
        .iter()
        .map(|id| format!("            {id},\n"))
        .collect::<String>();
    assert!(first.contains(&ids_text), "{first}");
    // Option fields that are None stay in the text. Canonical RON does not omit them.
    assert!(first.contains("nothing: None"), "{first}");
    Ok(())
}

#[test]
fn missing_fields_take_container_then_field_defaults() -> Result<(), SerializeError> {
    let registry = registry();
    let text = r#"{
        "regia_ecs::serialize::tests::Everything": (
            name: "partial",
            inner: (label: "only label"),
            mode: Chase(speed: 2.0),
        ),
    }"#;
    let mut codec = ReflectCodec::new(&registry);
    let value: Everything = codec.from_ron_as(text)?;
    assert_eq!(value.name, "partial");
    assert_eq!(value.ratio, 0.5, "container Default, not f32::default");
    assert_eq!(value.rotation, Quat::IDENTITY);
    assert_eq!(value.pair, (0, 'a'));
    assert_eq!(value.inner.label, "only label");
    assert_eq!(value.inner.weight, 0.0);
    assert_eq!(
        value.mode,
        Mode::Chase {
            speed: 2.0,
            target: Inner::default()
        }
    );
    assert!(codec.warnings().is_empty(), "{:?}", codec.warnings());
    Ok(())
}

#[derive(Reflect, Debug, PartialEq)]
struct NoDefault {
    a: u8,
    b: NoDefaultInner,
}

#[derive(Reflect, Debug, PartialEq)]
struct NoDefaultInner(u8);

#[test]
fn missing_field_without_any_default_is_an_error() {
    let mut registry = TypeRegistry::default();
    registry.register::<NoDefault>();
    let text = r#"{"regia_ecs::serialize::tests::NoDefault": (a: 1)}"#;
    let error = from_ron(text, &registry).err().map(|e| e.to_string());
    assert!(
        error.as_deref().is_some_and(|e| e.contains("missing field `b`")),
        "{error:?}"
    );
}

#[test]
fn unknown_fields_are_ignored_with_a_warning() -> Result<(), SerializeError> {
    let registry = registry();
    let text = r#"{
        "regia_ecs::serialize::tests::Everything": (
            name: "x",
            removed_field: [1, 2, (deep: Some("thing"))],
            inner: (label: "l", old: 3),
            mode: Chase(speed: 1.0, legacy: true),
        ),
    }"#;
    let mut codec = ReflectCodec::new(&registry);
    let value: Everything = codec.from_ron_as(text)?;
    assert_eq!(value.name, "x");
    assert_eq!(value.inner.label, "l");
    let warnings = codec.warnings().join("\n");
    assert!(warnings.contains("unknown field `removed_field`"), "{warnings}");
    assert!(warnings.contains("unknown field `old`"), "{warnings}");
    assert!(warnings.contains("unknown field `legacy`"), "{warnings}");
    Ok(())
}

#[test]
fn unknown_types_skip_or_preserve() -> Result<(), SerializeError> {
    let full = registry();
    let text = to_ron(&sample(), &full)?;
    let empty = TypeRegistry::default();

    let mut skip = ReflectCodec::new(&empty);
    let skipped = skip.from_ron(&text)?;
    assert!(
        matches!(&skipped, DeserializedValue::Skipped { type_path } if type_path.ends_with("Everything"))
    );
    assert_eq!(skip.warnings().len(), 1);

    let mut keep = ReflectCodec::new(&empty).unknown_types(UnknownTypePolicy::Preserve);
    let DeserializedValue::Opaque(opaque) = keep.from_ron(&text)? else {
        panic!("preserved");
    };
    assert_eq!(opaque.to_ron()?, text, "re-saved without data loss");
    // Once the type is known again, the preserved text loads.
    let back: Everything = from_ron_as(&opaque.to_ron()?, &full)?;
    assert_eq!(back, sample());

    let bytes = to_postcard(&sample(), &full)?;
    assert!(from_postcard(&bytes, &empty).is_err(), "binary cannot skip");
    Ok(())
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct Links {
    target: Entity,
    others: Vec<Entity>,
    maybe: Option<Entity>,
    by_name: HashMap<String, Entity>,
}

#[test]
fn entities_map_to_stable_ids_and_back() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<Links>();
    let mut world = World::new();
    let saved: Vec<Entity> = (0..3).map(|_| world.spawn_empty().id()).collect();
    let value = Links {
        target: saved[0],
        others: vec![saved[1], saved[2]],
        maybe: Some(saved[2]),
        by_name: HashMap::from([("boss".to_string(), saved[1])]),
    };
    let mut save_map = EntityIdMap::new();
    for (i, e) in saved.iter().enumerate() {
        save_map.insert(*e, 0xABCD_0000_0000_0000_0000_0000_0000_0000 + i as u128);
    }

    // A different world: the same ids now belong to different entities.
    let mut other = World::new();
    other.spawn_empty();
    let loaded: Vec<Entity> = (0..3).map(|_| other.spawn_empty().id()).collect();
    let mut load_map = EntityIdMap::new();
    for (i, e) in loaded.iter().enumerate() {
        load_map.insert(*e, 0xABCD_0000_0000_0000_0000_0000_0000_0000 + i as u128);
    }
    let expected = Links {
        target: loaded[0],
        others: vec![loaded[1], loaded[2]],
        maybe: Some(loaded[2]),
        by_name: HashMap::from([("boss".to_string(), loaded[1])]),
    };

    let text = ReflectCodec::new(&registry)
        .entity_mapper(&mut save_map)
        .to_ron(&value)?;
    assert!(
        text.contains("target: \"abcd0000000000000000000000000000\""),
        "{text}"
    );
    let back: Links = ReflectCodec::new(&registry)
        .entity_mapper(&mut load_map)
        .from_ron_as(&text)?;
    assert_eq!(back, expected);

    let bytes = ReflectCodec::new(&registry)
        .entity_mapper(&mut save_map)
        .to_postcard(&value)?;
    let back = ReflectCodec::new(&registry)
        .entity_mapper(&mut load_map)
        .from_postcard(&bytes)?;
    assert_eq!(back.downcast_ref::<Links>(), Some(&expected));
    assert_eq!(save_map.misses() + load_map.misses(), 0);

    // Without a mapper, entities keep their raw bits (in-process snapshots).
    let raw = to_ron(&value, &registry)?;
    let back: Links = from_ron_as(&raw, &registry)?;
    assert_eq!(back, value);
    assert!(
        raw.contains(&saved[0].to_bits().to_string()),
        "no-mapper RON writes Entity::to_bits(): {raw}"
    );
    assert!(
        !raw.contains("00000000000000000000000000000000"),
        "the EntityIdMap miss token is not written when no mapper is set: {raw}"
    );
    Ok(())
}

/// A type that serializes itself. Nested `Entity` fields are not walked.
#[derive(serde::Serialize, serde::Deserialize, Reflect, Debug, Clone, PartialEq)]
#[reflect(Serialize, Deserialize, PartialEq)]
struct SerdeLink {
    target: Entity,
}

#[test]
fn typed_ron_round_trip() -> Result<(), SerializeError> {
    let registry = registry();
    let registration = registry
        .get(std::any::TypeId::of::<Inner>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Inner".into()))?;
    let mut codec = ReflectCodec::new(&registry);
    let value = Inner {
        label: "bare".into(),
        weight: 2.5,
    };
    let text = codec.to_ron_typed(&value)?;
    assert_eq!(text, "(\n    label: \"bare\",\n    weight: 2.5,\n)\n");
    let back = codec.from_ron_typed(&text, registration)?;
    assert_eq!(back.downcast_ref::<Inner>(), Some(&value));
    Ok(())
}

#[test]
fn entity_id_map_stays_one_to_one_and_refuses_the_miss_id() {
    let mut world = World::new();
    let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
    let mut map = EntityIdMap::new();
    assert!(!map.insert(a, 0));
    assert!(map.insert(a, 10));
    assert!(map.insert(b, 10));
    // `a` lost id 10 to `b`: it is unmapped now, not silently pointing at `b`.
    assert_eq!(map.entity_to_id(b), 10);
    assert_eq!(map.entity_to_id(a), 0);
    assert_eq!(map.id_to_entity(10), b);
    // Re-pairing an entity frees its old id.
    assert!(map.insert(b, 11));
    assert_eq!(map.id_to_entity(10), Entity::PLACEHOLDER);
    assert_eq!(map.id_to_entity(11), b);
    // Id 0 never resolves.
    let before = map.misses();
    assert_eq!(map.id_to_entity(0), Entity::PLACEHOLDER);
    assert_eq!(map.misses(), before + 1);
}

#[derive(Reflect, Debug, Clone, PartialEq, Eq, Hash)]
#[reflect(PartialEq, Hash)]
struct StructKey {
    id: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByStruct {
    by_key: HashMap<StructKey, u32>,
}

/// `value` in the codec's top-level form: a one-entry map from `T`'s type path to the value.
fn typed_ron<T: TypePath>(value: &str) -> String {
    format!("{{\"{}\": {value}}}", T::type_path())
}

/// Whether `result` failed on the map-key/set-item hash check, not on some other error.
fn is_key_error<T>(result: &Result<T, SerializeError>) -> bool {
    matches!(result, Err(e) if e.to_string().contains("cannot be loaded"))
}

#[test]
fn struct_keyed_map_load_is_an_error_not_a_panic() {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedByStruct>();
    let result = ReflectCodec::new(&registry)
        .from_ron_as::<KeyedByStruct>(&typed_ron::<KeyedByStruct>("(by_key: {(id: 1): 2})"));
    assert!(is_key_error(&result), "{result:?}");
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default)]
struct Node {
    children: Vec<Node>,
}

fn nested_postcard(depth: usize) -> Vec<u8> {
    // Each level is a one-element list (length varint 1) holding the next level; the last is empty.
    let mut bytes = vec![1u8; depth];
    bytes.push(0);
    bytes
}

#[test]
fn postcard_nesting_is_bounded() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<Node>();
    let registration = registry
        .get(std::any::TypeId::of::<Node>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Node".into()))?;
    let mut codec = ReflectCodec::new(&registry);
    codec.from_postcard_typed(&nested_postcard(8), registration)?;
    let deep = nested_postcard(100_000);
    let err = codec.from_postcard_typed(&deep, registration).err();
    assert!(
        err.is_some(),
        "100000 nested levels must be rejected, not overflow the stack"
    );
    Ok(())
}

#[test]
fn unknown_field_warnings_are_capped() -> Result<(), SerializeError> {
    let registry = registry();
    let registration = registry
        .get(std::any::TypeId::of::<Inner>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Inner".into()))?;
    let mut text = String::from("(label: \"x\", weight: 1.0");
    for i in 0..1000 {
        text.push_str(&format!(", junk{i}: 1"));
    }
    text.push(')');
    let mut codec = ReflectCodec::new(&registry);
    codec.from_ron_typed(&text, registration)?;
    assert_eq!(codec.warnings().len(), de::MAX_WARNINGS + 1);
    Ok(())
}

#[test]
fn entity_id_map_rebind_drops_stale_entries() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let mut map = EntityIdMap::new();
    map.insert(a, 10);
    map.insert(a, 11);
    assert_eq!(
        map.id_to_entity(10),
        Entity::PLACEHOLDER,
        "old id of a rebound entity is gone"
    );
    assert_eq!(map.id_to_entity(11), a);
    map.insert(b, 11);
    assert_eq!(map.id_to_entity(11), b);
    assert_eq!(map.entity_to_id(a), 0, "entity that lost its id is a miss");
    assert_eq!(map.entity_to_id(b), 11);
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Wide {
    seed: u128,
    offset: i128,
}

#[test]
fn ron_round_trips_128_bit_integers() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<Wide>();
    let value = Wide {
        seed: u128::MAX,
        offset: i128::MIN,
    };
    let text = to_ron(&value, &registry)?;
    let back: Wide = from_ron_as(&text, &registry)?;
    assert_eq!(back, value);
    Ok(())
}

// Hashable in Rust (a `HashMap` key must be) but without a reflected `Hash`.
#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
#[reflect(Default, Deserialize)]
struct SerdeKeyNoHash {
    id: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedBySerdeNoHash {
    by_key: HashMap<SerdeKeyNoHash, u32>,
}

#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
#[reflect(Default, PartialEq, Hash, Deserialize)]
struct SerdeKeyHashed {
    id: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedBySerdeHashed {
    by_key: HashMap<SerdeKeyHashed, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq, Eq, Hash)]
#[reflect(PartialEq, Hash)]
enum EnumKey {
    Holds(StructKey),
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByEnum {
    by_key: HashMap<EnumKey, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByMap {
    by_key: HashMap<Vec<StructKey>, u32>,
}

#[test]
fn serde_key_without_reflected_hash_is_an_error_not_a_panic() {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedBySerdeNoHash>();
    let result =
        ReflectCodec::new(&registry)
            .from_ron_as::<KeyedBySerdeNoHash>(&typed_ron::<KeyedBySerdeNoHash>("(by_key: {(id: 1): 2})"));
    assert!(is_key_error(&result), "{result:?}");
}

#[test]
fn reflect_serialize_entity_fields_stay_raw_bits_with_a_mapper() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<SerdeLink>();
    let mut world = World::new();
    let entity = world.spawn_empty().id();
    let value = SerdeLink { target: entity };
    let mut save_map = EntityIdMap::new();
    assert!(save_map.insert(entity, 0xABCD));
    let text = ReflectCodec::new(&registry)
        .entity_mapper(&mut save_map)
        .to_ron(&value)?;
    assert!(
        text.contains(&entity.to_bits().to_string()),
        "custom serde writes the raw bits: {text}"
    );
    assert!(
        !text.contains("00000000000000000000000000000abcd"),
        "the mapper's stable id is not written inside #[reflect(Serialize)]: {text}"
    );
    let mut other = World::new();
    let different = other.spawn_empty().id();
    let mut load_map = EntityIdMap::new();
    assert!(load_map.insert(different, 0xABCD));
    let back: SerdeLink = ReflectCodec::new(&registry)
        .entity_mapper(&mut load_map)
        .from_ron_as(&text)?;
    assert_eq!(
        back.target, entity,
        "#[reflect(Deserialize)] does not consult the mapper"
    );
    Ok(())
}

#[test]
fn reflect_deserialize_skips_default_fill_and_unknown_field_warnings() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<SerdeKeyHashed>();
    let registration = registry
        .get(std::any::TypeId::of::<SerdeKeyHashed>())
        .ok_or_else(|| SerializeError::NoTypeInfo("SerdeKeyHashed".into()))?;
    let missing = ReflectCodec::new(&registry).from_ron_typed("()", registration);
    assert!(
        missing.is_err(),
        "own serde does not fill id from Default: {missing:?}"
    );
    let mut codec = ReflectCodec::new(&registry);
    let loaded = codec.from_ron_typed("(id: 1, extra: 2)", registration)?;
    assert_eq!(
        loaded.downcast_ref::<SerdeKeyHashed>(),
        Some(&SerdeKeyHashed { id: 1 })
    );
    assert!(
        codec.warnings().is_empty(),
        "own serde does not warn on an unknown field: {:?}",
        codec.warnings()
    );
    Ok(())
}

#[test]
fn serde_key_with_reflected_hash_still_loads() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedBySerdeHashed>();
    let loaded =
        ReflectCodec::new(&registry)
            .from_ron_as::<KeyedBySerdeHashed>(&typed_ron::<KeyedBySerdeHashed>("(by_key: {(id: 1): 2})"))?;
    assert_eq!(loaded.by_key.get(&SerdeKeyHashed { id: 1 }), Some(&2));
    Ok(())
}

#[test]
fn enum_and_list_keys_holding_structs_are_errors_not_panics() {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedByEnum>();
    registry.register::<KeyedByMap>();
    let enum_result = ReflectCodec::new(&registry)
        .from_ron_as::<KeyedByEnum>(&typed_ron::<KeyedByEnum>("(by_key: {Holds((id: 1)): 2})"));
    assert!(is_key_error(&enum_result), "{enum_result:?}");
    // A list key is rebuilt as the concrete `Vec<StructKey>` (through `FromReflect`), which
    // reflects `Hash` and equals itself, so it loads; the key check runs on the loaded value
    // (ADR-4951), where the old type-only check refused it.
    let list_result = ReflectCodec::new(&registry)
        .from_ron_as::<KeyedByMap>(&typed_ron::<KeyedByMap>("(by_key: {[(id: 1)]: 2})"));
    assert_eq!(
        list_result
            .ok()
            .and_then(|m| m.by_key.get(&vec![StructKey { id: 1 }]).copied()),
        Some(2)
    );
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default)]
struct TupleNode(Vec<TupleNode>);

#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq)]
#[reflect(Deserialize)]
struct SerdeNode {
    children: Vec<SerdeNode>,
}

#[test]
fn postcard_refuses_recursion_the_depth_counter_cannot_see() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<TupleNode>();
    registry.register::<SerdeNode>();
    let deep = nested_postcard(100_000);
    for id in [
        std::any::TypeId::of::<TupleNode>(),
        std::any::TypeId::of::<SerdeNode>(),
    ] {
        let registration = registry
            .get(id)
            .ok_or_else(|| SerializeError::NoTypeInfo("recursive".into()))?;
        let result = ReflectCodec::new(&registry).from_postcard_typed(&deep, registration);
        assert!(result.is_err(), "{id:?} must be rejected up front");
        let shallow = ReflectCodec::new(&registry).from_postcard_typed(&[0u8], registration);
        assert!(
            shallow.is_err(),
            "{id:?}: a recursive type is refused even for a short stream"
        );
    }
    Ok(())
}

#[test]
fn warning_cap_applies_per_decode_not_per_codec() -> Result<(), SerializeError> {
    let registry = registry();
    let registration = registry
        .get(std::any::TypeId::of::<Inner>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Inner".into()))?;
    let junk = |count: usize| {
        let mut text = String::from("(label: \"x\", weight: 1.0");
        for i in 0..count {
            text.push_str(&format!(", junk{i}: 1"));
        }
        text.push(')');
        text
    };
    let mut codec = ReflectCodec::new(&registry);
    codec.from_ron_typed(&junk(1000), registration)?;
    assert_eq!(codec.warnings().len(), de::MAX_WARNINGS + 1);
    codec.from_ron_typed(&junk(3), registration)?;
    assert_eq!(
        codec.warnings().len(),
        de::MAX_WARNINGS + 1 + 3,
        "the second decode keeps its own warnings"
    );
    Ok(())
}

#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
#[reflect(opaque, PartialEq, Default, Deserialize)]
struct OpaqueTok(u64);

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByOpaque {
    by_key: HashMap<OpaqueTok, u32>,
}

#[test]
fn opaque_key_without_reflected_hash_is_an_error_not_a_panic() {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedByOpaque>();
    let result = ReflectCodec::new(&registry)
        .from_ron_as::<KeyedByOpaque>(&typed_ron::<KeyedByOpaque>("(by_key: {(1): 2})"));
    assert!(is_key_error(&result), "{result:?}");
}

#[derive(Reflect, serde::Deserialize, Debug, Clone, Default)]
#[reflect(Default, PartialEq, Hash, Deserialize)]
struct FloatKey {
    n: f32,
}

impl PartialEq for FloatKey {
    fn eq(&self, other: &Self) -> bool {
        self.n == other.n
    }
}
impl Eq for FloatKey {}
impl std::hash::Hash for FloatKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.n.to_bits().hash(state);
    }
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByFloat {
    by_key: HashMap<FloatKey, u32>,
}

#[test]
fn float_holding_key_is_an_error_not_a_panic() {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedByFloat>();
    let result = ReflectCodec::new(&registry)
        .from_ron_as::<KeyedByFloat>(&typed_ron::<KeyedByFloat>("(by_key: {(n: NaN): 2})"));
    assert!(is_key_error(&result), "{result:?}");
}

// Audit sprint-audit-g-ecs-correctness finding 1: an opaque serde key with `PartialEq` and no
// `Default` (so the old up-front probe could not build a value) and no reflected `Hash`.
#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[reflect(opaque, PartialEq, Deserialize)]
struct OpaqueTokNoDefault(u64);

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByOpaqueNoDefault {
    by_key: HashMap<OpaqueTokNoDefault, u32>,
}

#[test]
fn opaque_key_without_default_or_reflected_hash_is_an_error_not_a_panic() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedByOpaqueNoDefault>();
    let result =
        ReflectCodec::new(&registry).from_ron_as::<KeyedByOpaqueNoDefault>(&typed_ron::<
            KeyedByOpaqueNoDefault,
        >("(by_key: {(1): 2})"));
    assert!(is_key_error(&result), "{result:?}");
    // The same map in postcard: map length 1, key 1, value 2.
    let registration = registry
        .get(std::any::TypeId::of::<KeyedByOpaqueNoDefault>())
        .ok_or_else(|| SerializeError::NoTypeInfo("KeyedByOpaqueNoDefault".into()))?;
    // postcard keeps no custom error text (`SerdeDeCustom`): an error, not a panic, is the check.
    let result = ReflectCodec::new(&registry).from_postcard_typed(&[1, 1, 2], registration);
    assert!(matches!(result, Err(SerializeError::Postcard(_))), "{result:?}");
    // An empty map holds no key to hash and loads.
    let empty = ReflectCodec::new(&registry)
        .from_ron_as::<KeyedByOpaqueNoDefault>(&typed_ron::<KeyedByOpaqueNoDefault>("(by_key: {})"))?;
    assert!(empty.by_key.is_empty());
    Ok(())
}

// Finding 2: floats the reflected shape cannot see. An opaque key around an `f32`, and a key
// whose only float is `#[reflect(ignore)]`; both reflect `Hash` (bits) and `PartialEq` (`==`).
#[derive(Reflect, serde::Deserialize, Debug, Clone, Default)]
#[reflect(opaque, Default, PartialEq, Hash, Deserialize)]
struct OpaqueMeters {
    m: f32,
}

impl PartialEq for OpaqueMeters {
    fn eq(&self, other: &Self) -> bool {
        self.m == other.m
    }
}
impl Eq for OpaqueMeters {}
impl std::hash::Hash for OpaqueMeters {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.m.to_bits().hash(state);
    }
}

#[derive(Reflect, serde::Deserialize, Debug, Clone, Default)]
#[reflect(Default, PartialEq, Hash, Deserialize)]
struct HiddenFloatKey {
    tag: u32,
    #[reflect(ignore)]
    n: f32,
}

impl PartialEq for HiddenFloatKey {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag && self.n == other.n
    }
}
impl Eq for HiddenFloatKey {}
impl std::hash::Hash for HiddenFloatKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tag.hash(state);
        self.n.to_bits().hash(state);
    }
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByOpaqueFloat {
    by_key: HashMap<OpaqueMeters, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct KeyedByHiddenFloat {
    by_key: HashMap<HiddenFloatKey, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct SetOfHiddenFloat {
    items: HashSet<HiddenFloatKey>,
}

#[test]
fn nan_in_a_float_reflection_cannot_see_is_an_error_not_a_panic() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<KeyedByOpaqueFloat>();
    registry.register::<KeyedByHiddenFloat>();
    registry.register::<SetOfHiddenFloat>();
    let opaque =
        ReflectCodec::new(&registry)
            .from_ron_as::<KeyedByOpaqueFloat>(&typed_ron::<KeyedByOpaqueFloat>("(by_key: {(m: NaN): 1})"));
    assert!(is_key_error(&opaque), "{opaque:?}");
    let hidden =
        ReflectCodec::new(&registry).from_ron_as::<KeyedByHiddenFloat>(&typed_ron::<KeyedByHiddenFloat>(
            "(by_key: {(tag: 1, n: NaN): 1})",
        ));
    assert!(is_key_error(&hidden), "{hidden:?}");
    let set = ReflectCodec::new(&registry)
        .from_ron_as::<SetOfHiddenFloat>(&typed_ron::<SetOfHiddenFloat>("(items: [(tag: 1, n: NaN)])"));
    assert!(is_key_error(&set), "{set:?}");
    // postcard: map length 1, tag 1, n = f32 NaN (little endian), value 1.
    let registration = registry
        .get(std::any::TypeId::of::<KeyedByHiddenFloat>())
        .ok_or_else(|| SerializeError::NoTypeInfo("KeyedByHiddenFloat".into()))?;
    let mut bytes = vec![1u8, 1];
    bytes.extend_from_slice(&f32::NAN.to_le_bytes());
    bytes.push(1);
    let binary = ReflectCodec::new(&registry).from_postcard_typed(&bytes, registration);
    assert!(matches!(binary, Err(SerializeError::Postcard(_))), "{binary:?}");
    // The same bytes with a finite float load, so the error above is the NaN key.
    let mut finite = vec![1u8, 1];
    finite.extend_from_slice(&2.5f32.to_le_bytes());
    finite.push(1);
    // (`FromReflect` rebuilds the key from `Default` plus its reflected fields, so the ignored
    // `n` reads back as 0.0: a `bevy_reflect` limit, listed in the lane report.)
    let loaded = ReflectCodec::new(&registry).from_postcard_typed(&finite, registration)?;
    let tags: Vec<(u32, u32)> = loaded
        .downcast_ref::<KeyedByHiddenFloat>()
        .map(|m| m.by_key.iter().map(|(k, v)| (k.tag, *v)).collect())
        .unwrap_or_default();
    assert_eq!(tags, [(1, 1)]);
    // The same keys with an ordinary float load: the check is on the value, not the type.
    let fine =
        ReflectCodec::new(&registry).from_ron_as::<KeyedByHiddenFloat>(&typed_ron::<KeyedByHiddenFloat>(
            "(by_key: {(tag: 1, n: 2.5): 7})",
        ))?;
    let tags: Vec<(u32, u32)> = fine.by_key.iter().map(|(k, v)| (k.tag, *v)).collect();
    assert_eq!(tags, [(1, 7)]);
    let fine =
        ReflectCodec::new(&registry)
            .from_ron_as::<KeyedByOpaqueFloat>(&typed_ron::<KeyedByOpaqueFloat>("(by_key: {(m: 1.5): 3})"))?;
    assert_eq!(fine.by_key.get(&OpaqueMeters { m: 1.5 }), Some(&3));
    Ok(())
}

// Finding 3: recursion through serde that the reflected shape hides (opaque, or an ignored field).
#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq)]
#[reflect(opaque, Deserialize)]
struct OpaqueTree {
    kids: Vec<OpaqueTree>,
}

#[derive(Reflect, serde::Deserialize, Debug, Clone, PartialEq, Default)]
#[reflect(Default, Deserialize)]
struct HiddenNode {
    #[reflect(ignore)]
    next: Option<Box<HiddenNode>>,
    n: u32,
}

#[test]
fn postcard_bounds_recursion_reflection_cannot_see() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<OpaqueTree>();
    registry.register::<HiddenNode>();
    let deep = nested_postcard(100_000);
    for (id, short) in [
        (std::any::TypeId::of::<OpaqueTree>(), &[0u8][..]),
        (std::any::TypeId::of::<HiddenNode>(), &[0u8, 0][..]),
    ] {
        let registration = registry
            .get(id)
            .ok_or_else(|| SerializeError::NoTypeInfo("hidden recursion".into()))?;
        let result = ReflectCodec::new(&registry).from_postcard_typed(&deep, registration);
        assert!(
            result.is_err(),
            "{id:?}: 100000 hidden levels must be an error, not a stack overflow"
        );
        // A shallow value still loads: the bound is on the data's depth, not the type.
        ReflectCodec::new(&registry).from_postcard_typed(short, registration)?;
        let mut moderate = vec![1u8; 20];
        moderate.push(0);
        if id == std::any::TypeId::of::<HiddenNode>() {
            moderate.extend(std::iter::repeat_n(0u8, 21));
        }
        ReflectCodec::new(&registry).from_postcard_typed(&moderate, registration)?;
    }
    Ok(())
}

// Finding 5: a missing field takes the container `Default`'s value even when that value holds
// state reflection ignores (so `reflect_clone` fails and a dynamic copy would drop it).
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default)]
struct HiddenStats {
    hp: u32,
    #[reflect(ignore)]
    secret: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
struct Npc {
    stats: HiddenStats,
    name: String,
}

impl Default for Npc {
    fn default() -> Self {
        Self {
            stats: HiddenStats { hp: 10, secret: 99 },
            name: "npc".into(),
        }
    }
}

#[derive(Reflect, Debug, Clone, PartialEq)]
struct Town {
    npc: Npc,
}

#[test]
fn missing_field_keeps_container_default_state_reflection_ignores() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<Npc>();
    registry.register::<Town>();
    let npc: Npc = ReflectCodec::new(&registry).from_ron_as(&typed_ron::<Npc>("(name: \"guard\")"))?;
    assert_eq!(npc.name, "guard");
    assert_eq!(npc.stats, HiddenStats { hp: 10, secret: 99 });
    // Nested: the parent rebuilds the `Npc` from its loaded fields.
    let town: Town = ReflectCodec::new(&registry).from_ron_as(&typed_ron::<Town>("(npc: ())"))?;
    assert_eq!(town.npc, Npc::default());
    Ok(())
}

fn typed_registration<'a>(
    registry: &'a TypeRegistry,
    id: std::any::TypeId,
    what: &str,
) -> Result<&'a super::TypeRegistration, SerializeError> {
    registry
        .get(id)
        .ok_or_else(|| SerializeError::NoTypeInfo(what.into()))
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default, PartialEq)]
struct PrimFields {
    n: f32,
    label: String,
}

impl Default for PrimFields {
    fn default() -> Self {
        Self {
            n: 0.0,
            label: String::new(),
        }
    }
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct ListField {
    values: Vec<f32>,
}

/// A complete postcard of primitive fields must not walk the type graph.
#[test]
fn postcard_decode_of_primitives_does_not_walk_the_type_graph_per_field() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<PrimFields>();
    let prims = typed_registration(&registry, std::any::TypeId::of::<PrimFields>(), "PrimFields")?;
    let prim = PrimFields {
        n: 1.5,
        label: "door".into(),
    };
    let mut codec = ReflectCodec::new(&registry);
    let prim_bytes = codec.to_postcard_typed(&prim)?;
    let _ = de::take_cycle_checks();
    let _ = de::take_cycle_walks();
    let back = codec.from_postcard_typed(&prim_bytes, prims)?;
    assert_eq!(back.downcast_ref::<PrimFields>(), Some(&prim));
    assert_eq!(
        de::take_cycle_checks(),
        0,
        "f32 and String are opaque; the walk cannot see a cycle through them"
    );
    assert_eq!(de::take_cycle_walks(), 0, "opaque leaves do not start a walk");
    Ok(())
}

/// A `Vec<f32>` walks once for the list itself rather than once per element. A second list
/// reuses that answer.
#[test]
fn postcard_decode_of_a_list_walks_its_type_once() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<ListField>();
    let lists = typed_registration(&registry, std::any::TypeId::of::<ListField>(), "ListField")?;
    let list = ListField {
        values: vec![1.0, 2.0, 3.0, 4.0],
    };
    let mut codec = ReflectCodec::new(&registry);
    let list_bytes = codec.to_postcard_typed(&list)?;
    let _ = de::take_cycle_checks();
    let _ = de::take_cycle_walks();
    let back = codec.from_postcard_typed(&list_bytes, lists)?;
    assert_eq!(back.downcast_ref::<ListField>(), Some(&list));
    assert_eq!(
        de::take_cycle_checks(),
        1,
        "one Vec value, not one walk per f32 element"
    );
    let walks = de::take_cycle_walks();
    assert!(
        walks <= 1,
        "the Vec's type graph is walked at most once, got {walks}"
    );
    let _ = de::take_cycle_checks();
    let back = codec.from_postcard_typed(&list_bytes, lists)?;
    assert_eq!(back.downcast_ref::<ListField>(), Some(&list));
    assert_eq!(de::take_cycle_checks(), 1, "the second Vec is still one check");
    assert_eq!(
        de::take_cycle_walks(),
        0,
        "the second Vec reuses the cached cycle answer"
    );
    Ok(())
}

thread_local! {
    static BOXED_DEFAULTS: Cell<u32> = const { Cell::new(0) };
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default, PartialEq)]
struct BoxedFields {
    n: f32,
    label: String,
}

impl Default for BoxedFields {
    fn default() -> Self {
        BOXED_DEFAULTS.with(|count| count.set(count.get().saturating_add(1)));
        Self {
            n: 0.0,
            label: String::new(),
        }
    }
}

/// A postcard struct with every field present must not construct an extra container `Default`.
///
/// `FromReflect` for a type that registers `Default` still constructs one (bevy uses it as the
/// base value), so a complete postcard is 1. That call is not `fill_missing`.
#[test]
fn postcard_struct_with_every_field_present_does_not_call_default() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<BoxedFields>();
    let registration = typed_registration(&registry, std::any::TypeId::of::<BoxedFields>(), "BoxedFields")?;
    let value = BoxedFields {
        n: 3.25,
        label: "kept".into(),
    };
    let mut codec = ReflectCodec::new(&registry);
    let bytes = codec.to_postcard_typed(&value)?;
    BOXED_DEFAULTS.with(|count| count.set(0));
    let back = codec.from_postcard_typed(&bytes, registration)?;
    assert_eq!(back.downcast_ref::<BoxedFields>(), Some(&value));
    let defaults = BOXED_DEFAULTS.with(Cell::get);
    assert_eq!(
        defaults, 1,
        "every postcard field was present, so only FromReflect builds Default"
    );
    Ok(())
}

/// Omitting a field still builds the container `Default` once inside `fill_missing`, plus the
/// `FromReflect` base value.
#[test]
fn omitted_ron_field_builds_the_container_default_once() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<BoxedFields>();
    let registration = typed_registration(&registry, std::any::TypeId::of::<BoxedFields>(), "BoxedFields")?;
    let mut codec = ReflectCodec::new(&registry);
    BOXED_DEFAULTS.with(|count| count.set(0));
    let partial = codec.from_ron_typed("(n: 1.5)", registration)?;
    let partial = partial
        .downcast_ref::<BoxedFields>()
        .ok_or_else(|| SerializeError::NoTypeInfo("BoxedFields".into()))?;
    assert_eq!(partial.n, 1.5);
    assert_eq!(partial.label, "");
    let defaults = BOXED_DEFAULTS.with(Cell::get);
    assert_eq!(
        defaults, 2,
        "one omitted field builds the container Default once, plus FromReflect"
    );
    Ok(())
}

#[derive(Reflect, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[reflect(Default, PartialEq, Hash, Deserialize)]
struct ProbeKey(u32);

thread_local! {
    static PROBE_KEY_DEFAULTS: Cell<u32> = const { Cell::new(0) };
}

/// Counts every `ProbeKey::default`, so the test sees a key check that builds a probe value.
impl Default for ProbeKey {
    fn default() -> Self {
        PROBE_KEY_DEFAULTS.with(|count| count.set(count.get().saturating_add(1)));
        Self(0)
    }
}

/// Two maps with the same key type never build that key's `Default` to check it (integ-v047a: the
/// check runs on each decoded key value, ecs-correctness finding 1), and both still round-trip.
/// The only `Default` calls are the concrete conversion's, one per key.
#[test]
fn map_key_hash_check_runs_once_per_key_type() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<HashMap<ProbeKey, u32>>();
    let registration = typed_registration(
        &registry,
        std::any::TypeId::of::<HashMap<ProbeKey, u32>>(),
        "HashMap<ProbeKey, u32>",
    )?;
    let first = HashMap::from([(ProbeKey(1), 10), (ProbeKey(2), 20)]);
    let second = HashMap::from([(ProbeKey(3), 30)]);
    let mut codec = ReflectCodec::new(&registry);
    let first_bytes = codec.to_postcard_typed(&first)?;
    let second_bytes = codec.to_postcard_typed(&second)?;
    PROBE_KEY_DEFAULTS.with(|count| count.set(0));
    let back = codec.from_postcard_typed(&first_bytes, registration)?;
    assert_eq!(back.downcast_ref::<HashMap<ProbeKey, u32>>(), Some(&first));
    let back = codec.from_postcard_typed(&second_bytes, registration)?;
    assert_eq!(back.downcast_ref::<HashMap<ProbeKey, u32>>(), Some(&second));
    // Converting the decoded map to the concrete `HashMap` runs the derived `FromReflect`, which
    // starts each `#[reflect(Default)]` key from `Default`: one call per decoded key (2 + 1). A
    // key check that probed a default key would add one per map (or per value) on top.
    assert_eq!(
        PROBE_KEY_DEFAULTS.with(Cell::get),
        3,
        "one Default per decoded key from FromReflect; the key check builds none"
    );
    Ok(())
}

/// Opaque on purpose: a derived struct compares its fields, so `reflect_partial_cmp` is `Some`
/// and the RON tie-break never runs. An opaque key without `PartialOrd` returns `None`.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Hash)]
#[reflect(opaque)]
#[reflect(PartialEq, Hash, Serialize, Deserialize)]
struct CountedMapKey {
    n: u32,
}

impl serde::Serialize for CountedMapKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.n)
    }
}

impl<'de> serde::Deserialize<'de> for CountedMapKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            n: <u32 as serde::Deserialize>::deserialize(deserializer)?,
        })
    }
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct CountedMap {
    by_key: HashMap<CountedMapKey, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct NestedCountedMap {
    level: HashMap<CountedMapKey, HashMap<CountedMapKey, u32>>,
}

/// Keys with no reflected ordering are turned into RON text once each, and the bytes stay stable.
#[test]
fn map_keys_without_partial_cmp_are_encoded_once_per_key() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<CountedMap>();
    let value = CountedMap {
        by_key: (0..32).map(|n| (CountedMapKey { n }, n * 3)).collect(),
    };
    let mut codec = ReflectCodec::new(&registry);
    let _ = ser::take_sort_text_calls();
    let first = codec.to_postcard_typed(&value)?;
    let encodes = ser::take_sort_text_calls();
    assert_eq!(
        encodes, 32,
        "each of the 32 keys is encoded once for ordering, not once per comparison"
    );
    let second = codec.to_postcard_typed(&value)?;
    assert_eq!(first, second, "canonical postcard bytes stay stable");
    assert_eq!(
        ser::take_sort_text_calls(),
        32,
        "the second encode is also once per key"
    );
    Ok(())
}

/// Nested maps: ordering the outer map must not serialize its values just to order the keys.
/// Each entry at every level is encoded once for ordering, so the count stays linear in the
/// total number of entries. Serializing a value twice more per entry (RON text plus postcard
/// bytes, as the unconditional tie-break did) re-sorts every nested map inside each probe:
/// 4 outer keys + 2*4 outer values * 8 inner keys + 4*8 inner keys on the write = 100, growing
/// by a factor of the branching factor per level.
#[test]
fn nested_map_values_are_not_serialized_for_ordering() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<NestedCountedMap>();
    let value = NestedCountedMap {
        level: (0..4)
            .map(|m| {
                let inner = (0..8).map(|n| (CountedMapKey { n }, n * 3)).collect();
                (CountedMapKey { n: 100 + m }, inner)
            })
            .collect(),
    };
    let mut codec = ReflectCodec::new(&registry);
    let _ = ser::take_sort_text_calls();
    let first = codec.to_postcard_typed(&value)?;
    let encodes = ser::take_sort_text_calls();
    assert_eq!(
        encodes, 36,
        "4 outer keys + 4*8 inner keys (write pass); the unfixed tie-break adds 2*4*8 = 100"
    );
    let second = codec.to_postcard_typed(&value)?;
    assert_eq!(first, second, "canonical postcard bytes stay stable");
    assert_eq!(
        ser::take_sort_text_calls(),
        36,
        "the second encode is also linear"
    );
    Ok(())
}

// A parent with no `#[reflect(Default)]` cannot omit the missing field: `FromReflect` requires it.
// Filling it with the concrete field-type `Default` makes the parent rebuild that value, and the
// rebuild drops `#[reflect(ignore)]` state nested inside the field type's own `Default`.
#[derive(Reflect, Debug, Clone, PartialEq)]
struct Square {
    npc: Npc,
    present: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
enum Watch {
    Post { npc: Npc, present: u32 },
}

#[test]
fn missing_field_on_parent_without_default_keeps_nested_ignored_state() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<Npc>();
    registry.register::<Town>();
    registry.register::<Square>();
    registry.register::<Watch>();
    // A map (an unknown field), not `()`: RON reads `()` as an empty sequence and errors before fill.
    let town: Town = ReflectCodec::new(&registry).from_ron_as(&typed_ron::<Town>("(not_a_field: 1)"))?;
    assert_eq!(
        town.npc,
        Npc::default(),
        "omitting Town.npc must keep Npc::default(), including the ignored secret"
    );
    let square: Square = ReflectCodec::new(&registry).from_ron_as(&typed_ron::<Square>("(present: 1)"))?;
    assert_eq!(square.present, 1);
    assert_eq!(
        square.npc,
        Npc::default(),
        "a sibling field must not cause the missing Npc to be rebuilt without its ignored state"
    );
    let watch: Watch = ReflectCodec::new(&registry).from_ron_as(&typed_ron::<Watch>("Post(present: 1)"))?;
    assert_eq!(
        watch,
        Watch::Post {
            npc: Npc::default(),
            present: 1
        }
    );
    Ok(())
}

fn tuned_seven() -> u32 {
    7
}

/// Field-level `#[reflect(default = path)]` differs from the container `Default`. A missing field
/// whose reflected parts all clone must stay the container `Default` (here `n == 3`), not `path()`.
#[derive(Reflect, Debug, PartialEq)]
#[reflect(Default)]
struct Tuned {
    #[reflect(default = "tuned_seven")]
    n: u32,
}

impl Default for Tuned {
    fn default() -> Self {
        Self { n: 3 }
    }
}

#[derive(Reflect, Debug, PartialEq)]
struct Holder {
    tuned: Tuned,
    flag: bool,
}

#[test]
fn missing_cloneable_field_keeps_field_type_default_not_field_attribute() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<Tuned>();
    registry.register::<Holder>();
    let holder: Holder = ReflectCodec::new(&registry).from_ron_as(&typed_ron::<Holder>("(flag: true)"))?;
    assert!(holder.flag);
    assert_eq!(holder.tuned, Tuned { n: 3 });
    Ok(())
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct ByEnt {
    entries: HashMap<Entity, u32>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct ByName {
    entries: HashMap<String, Entity>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct EntSet {
    items: HashSet<Entity>,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(PartialEq)]
struct TwoEnt {
    a: Entity,
    b: Entity,
}

/// Sorting a map key or a set item serializes it before the write. Those probes must not add
/// misses: one unmapped entity in the file is one miss. Two entity fields stay two misses.
#[test]
fn ordering_an_entity_key_counts_one_miss() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<ByEnt>();
    registry.register::<ByName>();
    registry.register::<EntSet>();
    registry.register::<TwoEnt>();
    let mut world = World::new();
    let key = world.spawn_empty().id();

    let mut unmapped = EntityIdMap::new();
    let text = ReflectCodec::new(&registry)
        .entity_mapper(&mut unmapped)
        .to_ron(&ByEnt {
            entries: HashMap::from([(key, 7)]),
        })?;
    assert!(text.contains("00000000000000000000000000000000"), "{text}");
    assert_eq!(unmapped.misses(), 1, "one unmapped entity map key");

    let mut mapped = EntityIdMap::new();
    assert!(mapped.insert(key, 0x11));
    ReflectCodec::new(&registry)
        .entity_mapper(&mut mapped)
        .to_ron(&ByEnt {
            entries: HashMap::from([(key, 7)]),
        })?;
    assert_eq!(mapped.misses(), 0, "a mapped entity key is a hit");

    let value_entity = world.spawn_empty().id();
    let mut value_map = EntityIdMap::new();
    ReflectCodec::new(&registry)
        .entity_mapper(&mut value_map)
        .to_ron(&ByName {
            entries: HashMap::from([("boss".to_string(), value_entity)]),
        })?;
    assert_eq!(value_map.misses(), 1, "one unmapped entity map value");

    let set_entity = world.spawn_empty().id();
    let mut set_map = EntityIdMap::new();
    ReflectCodec::new(&registry)
        .entity_mapper(&mut set_map)
        .to_ron(&EntSet {
            items: HashSet::from([set_entity]),
        })?;
    assert_eq!(set_map.misses(), 1, "one unmapped entity set item");

    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let mut fields = EntityIdMap::new();
    ReflectCodec::new(&registry)
        .entity_mapper(&mut fields)
        .to_ron(&TwoEnt { a, b })?;
    assert_eq!(fields.misses(), 2, "two unmapped entity fields");
    Ok(())
}
