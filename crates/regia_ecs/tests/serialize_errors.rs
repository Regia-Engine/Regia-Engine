// SPDX-License-Identifier: MPL-2.0
//! Error paths and API edges of the reflected (de)serializer, through the public surface only.

use std::any::TypeId;

use regia_ecs::prelude::*;
use regia_ecs::reflect::structs::DynamicStruct;
use regia_ecs::serialize::{
    DeserializedValue, EntityIdMap, EntityIdMapper, OpaqueValue, ReflectCodec, SerializeError,
    UnknownTypePolicy, from_postcard, from_ron, from_ron_as, to_postcard, to_ron,
};

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Probe {
    label: String,
    count: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct Other {
    flag: bool,
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
enum Kind {
    #[default]
    A,
    B(u8),
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
struct HasKind {
    kind: Kind,
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<Probe>();
    registry.register::<Other>();
    registry.register::<HasKind>();
    registry
}

const PROBE: &str = "serialize_errors::Probe";

fn probe() -> Probe {
    Probe {
        label: "p".into(),
        count: 9,
    }
}

#[test]
fn malformed_ron_is_a_parse_error() {
    let registry = registry();
    for text in ["", "{", "{\"x\": ", "not ron at all", "[1, 2]"] {
        assert!(from_ron(text, &registry).is_err(), "accepted {text:?}");
    }
    assert!(matches!(
        from_ron("{", &registry),
        Err(SerializeError::RonParse(_))
    ));
}

#[test]
fn garbage_after_the_value_is_rejected() -> Result<(), SerializeError> {
    let registry = registry();
    let text = to_ron(&probe(), &registry)?;
    assert!(from_ron(&format!("{text} trailing"), &registry).is_err());
    Ok(())
}

#[test]
fn two_type_path_entries_are_rejected() {
    let registry = registry();
    let text = format!("{{\"{PROBE}\": (label: \"a\", count: 1), \"{PROBE}\": (label: \"b\", count: 2)}}");
    assert!(from_ron(&text, &registry).is_err());
}

#[test]
fn empty_top_level_map_is_rejected() {
    let registry = registry();
    assert!(from_ron("{}", &registry).is_err());
}

#[test]
fn wrong_field_type_and_unknown_variant_are_errors() {
    let registry = registry();
    let bad_field = format!("{{\"{PROBE}\": (label: 5, count: 1)}}");
    assert!(from_ron(&bad_field, &registry).is_err());
    let negative = format!("{{\"{PROBE}\": (label: \"a\", count: -1)}}");
    assert!(from_ron(&negative, &registry).is_err(), "negative into u32");
    let bad_variant = "{\"serialize_errors::HasKind\": (kind: Nope)}";
    assert!(from_ron(bad_variant, &registry).is_err());
}

#[test]
fn from_ron_as_reports_the_wrong_type() -> Result<(), SerializeError> {
    let registry = registry();
    let text = to_ron(&probe(), &registry)?;
    match from_ron_as::<Other>(&text, &registry) {
        Err(SerializeError::WrongType { expected, found }) => {
            assert!(expected.ends_with("::Other"), "{expected}");
            assert!(found.ends_with("::Probe"), "{found}");
        }
        other => panic!("expected WrongType, got {other:?}"),
    }
    Ok(())
}

#[test]
fn from_ron_as_on_a_skipped_type_is_wrong_type() -> Result<(), SerializeError> {
    let full = registry();
    let text = to_ron(&probe(), &full)?;
    let empty = TypeRegistry::default();
    assert!(matches!(
        from_ron_as::<Probe>(&text, &empty),
        Err(SerializeError::WrongType { .. })
    ));
    Ok(())
}

#[test]
fn dynamic_value_without_type_info_cannot_be_written() {
    let registry = registry();
    let mut dynamic = DynamicStruct::default();
    dynamic.insert("count", 1u32);
    assert!(matches!(
        to_ron(&dynamic, &registry),
        Err(SerializeError::NoTypeInfo(_))
    ));
    assert!(matches!(
        to_postcard(&dynamic, &registry),
        Err(SerializeError::NoTypeInfo(_))
    ));
}

#[test]
fn truncated_postcard_is_an_error() -> Result<(), SerializeError> {
    let registry = registry();
    let bytes = to_postcard(&probe(), &registry)?;
    assert!(bytes.len() > 2);
    for cut in [0, 1, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            from_postcard(&bytes[..cut], &registry).is_err(),
            "accepted a {cut}-byte prefix of {} bytes",
            bytes.len()
        );
    }
    Ok(())
}

#[test]
fn typed_postcard_rejects_trailing_bytes() -> Result<(), SerializeError> {
    let registry = registry();
    let registration = registry
        .get(TypeId::of::<Probe>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Probe".into()))?;
    let mut codec = ReflectCodec::new(&registry);
    let mut bytes = codec.to_postcard_typed(&probe())?;
    bytes.extend_from_slice(&[1, 2, 3]);
    assert!(matches!(
        codec.from_postcard_typed(&bytes, registration),
        Err(SerializeError::TrailingBytes(3))
    ));
    Ok(())
}

#[test]
fn typed_ron_rejects_trailing_garbage() -> Result<(), SerializeError> {
    let registry = registry();
    let registration = registry
        .get(TypeId::of::<Probe>())
        .ok_or_else(|| SerializeError::NoTypeInfo("Probe".into()))?;
    let mut codec = ReflectCodec::new(&registry);
    let text = codec.to_ron_typed(&probe())?;
    assert!(codec.from_ron_typed(&format!("{text} 5"), registration).is_err());
    Ok(())
}

mod game_a {
    use regia_ecs::prelude::*;

    #[derive(Reflect, Default, Debug, PartialEq)]
    #[reflect(Default)]
    pub struct Door {
        pub open: bool,
    }
}

mod game_b {
    use regia_ecs::prelude::*;

    #[derive(Reflect, Default, Debug, PartialEq)]
    #[reflect(Default)]
    pub struct Door {
        pub open: bool,
    }
}

#[test]
fn an_ambiguous_short_type_path_is_not_called_unregistered() -> Result<(), SerializeError> {
    let mut registry = TypeRegistry::default();
    registry.register::<game_a::Door>();
    registry.register::<game_b::Door>();
    assert!(registry.is_ambiguous("Door"));
    let full = registry
        .get(TypeId::of::<game_a::Door>())
        .expect("registered")
        .type_info()
        .type_path();
    let mut known = ReflectCodec::new(&registry);
    let loaded = known.from_ron(&format!("{{\"{full}\": (open: true)}}"))?;
    assert!(matches!(loaded, DeserializedValue::Value(_)), "{loaded:?}");
    assert!(known.warnings().is_empty(), "{:?}", known.warnings());

    let mut skipped = ReflectCodec::new(&registry);
    let dropped = skipped.from_ron("{\"Door\": (open: true)}")?;
    assert!(
        matches!(dropped, DeserializedValue::Skipped { .. }),
        "{dropped:?}"
    );
    let warnings = skipped.warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("ambiguous short type path `Door`")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().all(|w| !w.contains("not registered")),
        "{warnings:?}"
    );

    let mut kept = ReflectCodec::new(&registry).unknown_types(UnknownTypePolicy::Preserve);
    let preserved = kept.from_ron("{\"Door\": (open: true)}")?;
    assert!(matches!(preserved, DeserializedValue::Opaque(_)), "{preserved:?}");
    assert!(
        kept.warnings()
            .iter()
            .any(|w| w.contains("ambiguous short type path `Door`; value preserved")),
        "{:?}",
        kept.warnings()
    );
    Ok(())
}

#[test]
fn short_type_path_is_accepted_at_the_top_level() -> Result<(), SerializeError> {
    let registry = registry();
    let value: Probe = from_ron_as("{\"Probe\": (label: \"s\", count: 2)}", &registry)?;
    assert_eq!(
        value,
        Probe {
            label: "s".into(),
            count: 2
        }
    );
    Ok(())
}

#[test]
fn canonical_ron_layout_is_stable() -> Result<(), SerializeError> {
    let registry = registry();
    let text = to_ron(&probe(), &registry)?;
    assert_eq!(
        text,
        format!("{{\n    \"{PROBE}\": (\n        label: \"p\",\n        count: 9,\n    ),\n}}\n")
    );
    Ok(())
}

#[test]
fn warnings_accumulate_and_drain() -> Result<(), SerializeError> {
    let registry = registry();
    let mut codec = ReflectCodec::new(&registry);
    let text = format!("{{\"{PROBE}\": (label: \"a\", count: 1, extra: 1)}}");
    codec.from_ron(&text)?;
    assert_eq!(codec.warnings().len(), 1);
    codec.from_ron("{\"nowhere::Missing\": (a: 1)}")?;
    assert_eq!(
        codec.warnings().len(),
        2,
        "unknown fields and unknown types both warn"
    );
    let taken = codec.take_warnings();
    assert_eq!(taken.len(), 2);
    assert!(codec.warnings().is_empty());
    Ok(())
}

#[test]
fn deserialized_value_into_value_only_for_known_types() -> Result<(), SerializeError> {
    let registry = registry();
    let text = to_ron(&probe(), &registry)?;
    let known = from_ron(&text, &registry)?;
    assert!(known.into_value().is_some());
    let skipped = from_ron("{\"nowhere::Missing\": (a: 1)}", &registry)?;
    assert!(matches!(&skipped, DeserializedValue::Skipped { type_path } if type_path == "nowhere::Missing"));
    assert!(skipped.into_value().is_none());
    let mut keep = ReflectCodec::new(&registry).unknown_types(UnknownTypePolicy::Preserve);
    let opaque = keep.from_ron("{\"nowhere::Missing\": (a: 1)}")?;
    assert!(opaque.into_value().is_none());
    Ok(())
}

#[test]
fn preserved_opaque_value_round_trips_verbatim() -> Result<(), SerializeError> {
    let registry = registry();
    let mut keep = ReflectCodec::new(&registry).unknown_types(UnknownTypePolicy::Preserve);
    let DeserializedValue::Opaque(opaque) = keep.from_ron("{\"nowhere::Missing\": (a: 1, b: [2, 3])}")?
    else {
        panic!("expected an opaque value");
    };
    assert_eq!(opaque.type_path, "nowhere::Missing");
    assert_eq!(keep.warnings().len(), 1);
    let again = ReflectCodec::new(&registry)
        .unknown_types(UnknownTypePolicy::Preserve)
        .from_ron(&opaque.to_ron()?)?;
    let DeserializedValue::Opaque(second) = again else {
        panic!("expected an opaque value");
    };
    assert_eq!(second, opaque);
    Ok(())
}

#[test]
fn opaque_value_with_invalid_ron_fails_to_write() {
    let opaque = OpaqueValue {
        type_path: "nowhere::Missing".into(),
        ron: "(unclosed".into(),
    };
    assert!(opaque.to_ron().is_err());
}

#[test]
fn postcard_unknown_type_is_a_construct_error() -> Result<(), SerializeError> {
    let full = registry();
    let bytes = to_postcard(&probe(), &full)?;
    let mut only_other = TypeRegistry::default();
    only_other.register::<Other>();
    assert!(from_postcard(&bytes, &only_other).is_err());
    Ok(())
}

#[test]
fn entity_id_map_counts_misses() {
    let mut world = World::new();
    let known = world.spawn_empty().id();
    let unknown = world.spawn_empty().id();
    let mut map = EntityIdMap::new();
    map.insert(known, 77);
    assert_eq!(map.misses(), 0);
    assert_eq!(map.entity_to_id(known), 77);
    assert_eq!(map.id_to_entity(77), known);
    assert_eq!(map.misses(), 0, "hits do not count");
    assert_eq!(map.entity_to_id(unknown), 0);
    assert_eq!(map.id_to_entity(12345), Entity::PLACEHOLDER);
    assert_eq!(map.misses(), 2);
}

#[test]
fn unmapped_entities_in_data_count_as_misses_on_load() -> Result<(), SerializeError> {
    #[derive(Reflect, Debug, Clone, PartialEq)]
    #[reflect(PartialEq)]
    struct Link {
        target: Entity,
    }
    let mut registry = TypeRegistry::default();
    registry.register::<Link>();
    let mut world = World::new();
    let target = world.spawn_empty().id();
    let mut save = EntityIdMap::new();
    save.insert(target, 0x1234);
    let text = ReflectCodec::new(&registry)
        .entity_mapper(&mut save)
        .to_ron(&Link { target })?;
    let mut empty = EntityIdMap::new();
    let back: Link = ReflectCodec::new(&registry)
        .entity_mapper(&mut empty)
        .from_ron_as(&text)?;
    assert_eq!(back.target, Entity::PLACEHOLDER);
    assert_eq!(empty.misses(), 1);
    assert_eq!(save.misses(), 0);
    Ok(())
}
