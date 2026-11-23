// SPDX-License-Identifier: MPL-2.0
//! `describe_type` over every type shape, and its lookup edges.

use std::collections::{HashMap, HashSet};

use regia_ecs::describe::{
    TypePathLookup, TypeShape, describe_info, describe_registration, describe_type, field_attrs, lookup_type,
    registered_type_paths,
};
use regia_ecs::prelude::*;

#[derive(Reflect, Default)]
#[reflect(Default)]
struct Unit;

#[derive(Reflect, Default)]
#[reflect(Default)]
struct Wrapper(u32, String);

#[derive(Reflect, Default)]
#[reflect(Default)]
struct Shapes {
    pair: (u8, bool),
    grid: [i32; 4],
    names: Vec<String>,
    scores: HashMap<String, u32>,
    ids: HashSet<u32>,
    maybe: Option<u32>,
    plain: f32,
}

// `Resource` implies `Component` in bevy_ecs 0.20, so the derive gives both impls.
#[derive(Resource, Reflect, Default)]
#[reflect(Component, Resource, Default)]
struct Both;

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<Shapes>();
    registry.register::<Wrapper>();
    registry.register::<Unit>();
    registry.register::<Both>();
    registry
}

#[test]
fn unknown_types_are_none() {
    let registry = registry();
    assert!(describe_type(&registry, "no::such::Type").is_none());
    assert!(describe_type(&registry, "").is_none());
    assert!(matches!(
        lookup_type(&registry, "no::such::Type"),
        Err(TypePathLookup::NotRegistered)
    ));
}

mod left_door {
    use regia_ecs::prelude::*;

    #[derive(Reflect, Default)]
    #[reflect(Default)]
    pub struct Door;
}

mod right_door {
    use regia_ecs::prelude::*;

    #[derive(Reflect, Default)]
    #[reflect(Default)]
    pub struct Door;
}

#[test]
fn an_ambiguous_short_path_is_not_a_missing_type() {
    let mut registry = TypeRegistry::default();
    registry.register::<left_door::Door>();
    registry.register::<right_door::Door>();
    assert!(describe_type(&registry, "Door").is_none());
    assert!(matches!(
        lookup_type(&registry, "Door"),
        Err(TypePathLookup::Ambiguous)
    ));
    let full = registry
        .get(std::any::TypeId::of::<left_door::Door>())
        .expect("registered")
        .type_info()
        .type_path();
    assert_eq!(describe_type(&registry, full).map(|d| d.type_path), Some(full));
}

#[test]
fn tuple_struct_fields_are_indexed() {
    let registry = registry();
    let Some(description) = describe_type(&registry, "Wrapper") else {
        panic!("registered");
    };
    let TypeShape::TupleStruct(fields) = description.shape else {
        panic!("tuple struct");
    };
    let names: Vec<_> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["0", "1"]);
    assert_eq!(fields[0].type_path, "u32");
    assert_eq!(fields[1].type_path, "alloc::string::String");
}

#[test]
fn unit_struct_has_no_fields() {
    let registry = registry();
    let Some(description) = describe_type(&registry, "Unit") else {
        panic!("registered");
    };
    assert!(matches!(&description.shape, TypeShape::Struct(fields) if fields.is_empty()));
}

#[test]
fn every_container_shape_is_described() {
    let registry = registry();
    let Some(shapes) = describe_type(&registry, "Shapes") else {
        panic!("registered");
    };
    let TypeShape::Struct(fields) = &shapes.shape else {
        panic!("struct");
    };
    let shape_of = |name: &str| {
        let field = fields.iter().find(|f| f.name == name);
        let path = field.map(|f| f.type_path).unwrap_or_default();
        describe_type(&registry, path).map(|d| d.shape)
    };
    let Some(TypeShape::Tuple(items)) = shape_of("pair") else {
        panic!("tuple");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].name, "1");
    assert!(matches!(
        shape_of("grid"),
        Some(TypeShape::Array { item: "i32", len: 4 })
    ));
    assert!(matches!(
        shape_of("names"),
        Some(TypeShape::List {
            item: "alloc::string::String"
        })
    ));
    assert!(matches!(
        shape_of("scores"),
        Some(TypeShape::Map {
            key: "alloc::string::String",
            value: "u32"
        })
    ));
    assert!(matches!(shape_of("ids"), Some(TypeShape::Set { value: "u32" })));
    let Some(TypeShape::Enum(variants)) = shape_of("maybe") else {
        panic!("Option is an enum");
    };
    let names: Vec<_> = variants.iter().map(|v| v.name).collect();
    assert_eq!(names, ["None", "Some"]);
    assert!(variants[0].fields.is_empty() && !variants[0].named);
    assert_eq!(variants[1].fields.len(), 1);
    assert!(matches!(shape_of("plain"), Some(TypeShape::Opaque)));
}

#[test]
fn data_flags_reflect_registered_capabilities() {
    let registry = registry();
    let Some(both) = describe_type(&registry, "Both") else {
        panic!("registered");
    };
    assert!(both.data.component && both.data.resource && both.data.default);
    let Some(wrapper) = describe_type(&registry, "Wrapper") else {
        panic!("registered");
    };
    assert!(!wrapper.data.component && !wrapper.data.resource);
    assert!(wrapper.data.default && wrapper.data.from_reflect);
    assert!(
        !wrapper.data.serde,
        "serde needs both Serialize and Deserialize data"
    );
}

#[test]
fn describe_info_leaves_data_empty_and_matches_registration_shape() {
    let registry = registry();
    let info = describe_info(Shapes::type_info());
    assert_eq!(info.data, Default::default());
    let Some(registration) = registry.get(std::any::TypeId::of::<Shapes>()) else {
        panic!("registered");
    };
    let described = describe_registration(registration);
    assert_eq!(info.type_path, described.type_path);
    assert_eq!(info.short_path, "Shapes");
    assert!(described.data.default);
}

#[test]
fn full_and_short_paths_find_the_same_type() {
    let registry = registry();
    let by_short = describe_type(&registry, "Wrapper").map(|d| d.type_path);
    let Some(full) = by_short else {
        panic!("short path lookup");
    };
    assert_eq!(describe_type(&registry, full).map(|d| d.type_path), Some(full));
}

#[test]
fn field_attrs_are_none_for_missing_fields_and_non_structs() {
    assert!(field_attrs::<Shapes>("plain").is_some_and(|a| a.is_empty()));
    assert!(field_attrs::<Shapes>("nope").is_none());
    assert!(
        field_attrs::<Wrapper>("0").is_none(),
        "tuple structs have no named fields"
    );
    assert!(field_attrs::<u32>("x").is_none());
}

#[test]
fn registered_type_paths_are_sorted_and_unique() {
    let registry = registry();
    let paths = registered_type_paths(&registry);
    assert!(paths.windows(2).all(|w| w[0] <= w[1]), "sorted");
    let mut deduped = paths.clone();
    deduped.dedup();
    assert_eq!(deduped, paths);
    assert!(paths.contains(&"u32"));
    assert!(paths.iter().any(|p| p.ends_with("::Shapes")));
    assert!(registered_type_paths(&TypeRegistry::empty()).is_empty());
}
