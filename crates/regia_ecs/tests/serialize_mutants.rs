// SPDX-License-Identifier: MPL-2.0
//! Tests written from the mutation run of `regia_ecs::serialize` (ADR-3389): each one pins a behavior a mutant
//! broke while the older tests still passed.

use regia_ecs::prelude::*;
use regia_ecs::reflect::TypeRegistration;
use regia_ecs::serialize::ReflectCodec;

/// The registry's registration of `T` (with the type data registering adds: `FromReflect`, `Default`).
fn reg<T: 'static>(registry: &TypeRegistry) -> &TypeRegistration {
    registry.get(std::any::TypeId::of::<T>()).expect("registered")
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct Holder {
    label: String,
    maybe: Option<u32>,
}

#[test]
fn the_warning_cap_ends_with_one_summary_line() {
    let mut registry = TypeRegistry::default();
    registry.register::<Holder>();
    let mut text = String::from("(label: \"x\"");
    for i in 0..300 {
        text.push_str(&format!(", junk{i}: 1"));
    }
    text.push(')');
    let mut codec = ReflectCodec::new(&registry);
    codec
        .from_ron_typed(&text, reg::<Holder>(&registry))
        .expect("unknown fields are skipped");
    let warnings = codec.warnings();
    assert_eq!(warnings.len(), 257);
    assert_eq!(warnings[256], "further warnings suppressed after 256");
    assert!(
        warnings[..256].iter().all(|w| w.starts_with("unknown field")),
        "256 real warnings come first"
    );
    assert!(warnings[255].contains("junk255"));
    assert!(
        warnings[0].contains("Holder"),
        "the warning names the struct: {}",
        warnings[0]
    );
}

#[test]
fn an_option_field_reads_through_rons_option_rules() {
    // `implicit_some` is a RON option rule: a bare value is `Some(value)`. It only applies when the option
    // is decoded as an option, not as a generic two-variant enum.
    let mut registry = TypeRegistry::default();
    registry.register::<Holder>();
    let mut codec = ReflectCodec::new(&registry);
    let text = "#![enable(implicit_some)]\n(label: \"x\", maybe: 5)";
    let value = codec
        .from_ron_typed(text, reg::<Holder>(&registry))
        .expect("implicit_some decodes a bare value");
    assert_eq!(
        value.downcast_ref::<Holder>(),
        Some(&Holder {
            label: "x".into(),
            maybe: Some(5)
        })
    );
}

/// An enum that is only called `Option`: it is not `core::option::Option` and keeps the lenient rules.
mod lookalike {
    use regia_ecs::prelude::*;

    #[derive(Reflect, Debug, PartialEq)]
    pub enum Option {
        Shape { size: u32 },
        Nothing,
    }
}

#[test]
fn an_enum_named_option_elsewhere_keeps_lenient_struct_variants() {
    let mut registry = TypeRegistry::default();
    registry.register::<lookalike::Option>();
    let mut codec = ReflectCodec::new(&registry);
    let value = codec
        .from_ron_typed("Shape(size: 3, extra: 1)", reg::<lookalike::Option>(&registry))
        .expect("an unknown field in a struct variant is a warning, not an error");
    assert_eq!(
        value.downcast_ref::<lookalike::Option>(),
        Some(&lookalike::Option::Shape { size: 3 })
    );
    assert_eq!(codec.warnings().len(), 1);
    assert!(codec.warnings()[0].contains("extra"));
    assert!(
        codec.warnings()[0].contains("Shape"),
        "the warning names the variant: {}",
        codec.warnings()[0]
    );
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct Leaf {
    n: u32,
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
enum Mark {
    #[default]
    On,
    Off,
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct Siblings {
    leaves: Vec<Leaf>,
    marks: Vec<Mark>,
}

#[test]
fn sibling_structs_and_enums_do_not_add_up_to_the_depth_limit() {
    // 100 siblings: each one enters and leaves one level, so the depth never passes 2.
    let mut registry = TypeRegistry::default();
    registry.register::<Siblings>();
    let leaves = vec!["(n: 1)"; 100].join(", ");
    let marks = vec!["On"; 100].join(", ");
    let text = format!("(leaves: [{leaves}], marks: [{marks}])");
    let mut codec = ReflectCodec::new(&registry);
    let value = codec
        .from_ron_typed(&text, reg::<Siblings>(&registry))
        .expect("100 siblings are two levels deep");
    let value = value.downcast_ref::<Siblings>().expect("Siblings");
    assert_eq!((value.leaves.len(), value.marks.len()), (100, 100));
}

type V2<T> = Vec<Vec<T>>;
type V4<T> = V2<V2<T>>;
/// Eight nested lists around a `u32`.
type Deep8 = V4<V4<u32>>;
/// Nine nested lists: past the depth the old type-only walk could prove (ADR-4951 replaced it
/// with a check on each loaded item, which has no depth limit).
type Deep9 = Vec<Deep8>;

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct SetHolder8 {
    items: std::collections::HashSet<Deep8>,
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct SetHolder9 {
    items: std::collections::HashSet<Deep9>,
}

#[derive(Reflect, Debug, PartialEq, Eq, Hash)]
struct NoHash {
    id: u32,
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct StructSet {
    items: std::collections::HashSet<NoHash>,
}

/// `n` nested one-element lists around `1`, in RON.
fn nested_list(n: usize) -> String {
    format!("{}1{}", "[".repeat(n), "]".repeat(n))
}

#[test]
fn set_items_are_checked_for_a_reflected_hash() {
    let mut registry = TypeRegistry::default();
    registry.register::<SetHolder8>();
    registry.register::<SetHolder9>();
    registry.register::<StructSet>();
    let mut codec = ReflectCodec::new(&registry);
    // Nested lists of `u32` load as concrete lists, which hash, at any depth the format allows.
    let eight = codec
        .from_ron_typed(
            &format!("(items: [{}])", nested_list(8)),
            reg::<SetHolder8>(&registry),
        )
        .expect("eight nested lists of u32 hash");
    assert_eq!(eight.downcast_ref::<SetHolder8>().map(|s| s.items.len()), Some(1));
    let nine = codec
        .from_ron_typed(
            &format!("(items: [{}])", nested_list(9)),
            reg::<SetHolder9>(&registry),
        )
        .expect("nine nested lists of u32 hash");
    assert_eq!(nine.downcast_ref::<SetHolder9>().map(|s| s.items.len()), Some(1));
    // A struct item loads as a dynamic struct, which has no reflected hash: refuse it instead of panicking.
    let err = codec
        .from_ron_typed("(items: [(id: 1)])", reg::<StructSet>(&registry))
        .expect_err("a struct set item cannot be loaded");
    assert!(err.to_string().contains("set item"), "{err}");
    // An empty set holds no item to check.
    codec
        .from_ron_typed("(items: [])", reg::<StructSet>(&registry))
        .expect("an empty set of struct items loads");
}

/// A serde key type: loaded concrete, probed through its default.
#[derive(Reflect, Default, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[reflect(Default, Hash, PartialEq, Serialize, Deserialize)]
struct SerdeKey7(V4<V2<Vec<u32>>>);

#[derive(Reflect, Default, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[reflect(Default, Hash, PartialEq, Serialize, Deserialize)]
struct SerdeKey8(Deep8);

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct SerdeSets {
    seven: std::collections::HashSet<SerdeKey7>,
    eight: std::collections::HashSet<SerdeKey8>,
}

#[test]
fn serde_set_items_load_at_any_nesting_depth() {
    let mut registry = TypeRegistry::default();
    registry.register::<SerdeSets>();
    registry.register::<SerdeKey7>();
    registry.register::<SerdeKey8>();
    let mut codec = ReflectCodec::new(&registry);
    // The key check runs on each loaded item (ADR-4951), so a float-free serde key loads whatever
    // its depth; the old type-only walk refused `SerdeKey8` (its `u32` sits at depth 9).
    let text = format!("(seven: [({})], eight: [({})])", nested_list(7), nested_list(8));
    let value = codec
        .from_ron_typed(&text, reg::<SerdeSets>(&registry))
        .expect("float-free serde keys load at depth 8 and 9");
    let sets = value.downcast_ref::<SerdeSets>().expect("SerdeSets");
    assert_eq!((sets.seven.len(), sets.eight.len()), (1, 1));
}

/// Recursion through a tuple inside a list: nothing on the cycle counts depth.
#[derive(Reflect, Default, Debug, PartialEq)]
struct ViaTuple(Vec<(u8, ViaTuple)>);

/// Recursion through an array inside a list.
#[derive(Reflect, Default, Debug, PartialEq)]
struct ViaArray(Vec<[ViaArray; 1]>);

/// Recursion through a map value.
#[derive(Reflect, Default, Debug, PartialEq)]
struct ViaMap(std::collections::HashMap<u32, ViaMap>);

/// Recursion through a set item.
#[derive(Reflect, Default, Debug, PartialEq, Eq)]
struct ViaSet(std::collections::HashSet<ViaSet>);

impl std::hash::Hash for ViaSet {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
    }
}

#[test]
fn binary_decode_refuses_types_that_recurse_through_uncounted_containers() {
    let mut registry = TypeRegistry::default();
    registry.register::<ViaTuple>();
    registry.register::<ViaArray>();
    registry.register::<ViaMap>();
    registry.register::<ViaSet>();
    let mut codec = ReflectCodec::new(&registry);
    // An empty collection: postcard writes its length, 0.
    for (name, registration) in [
        ("tuple", reg::<ViaTuple>(&registry)),
        ("array", reg::<ViaArray>(&registry)),
        ("map", reg::<ViaMap>(&registry)),
        ("set", reg::<ViaSet>(&registry)),
    ] {
        let err = codec
            .from_postcard_typed(&[0], registration)
            .map(|_| ())
            .expect_err("a cycle the depth limit cannot count is refused");
        // postcard keeps no custom error text, so only the refusal itself is checked.
        let _ = (name, err);
    }
}

#[derive(Reflect, Debug, PartialEq)]
struct Link {
    target: Entity,
}

#[test]
fn a_numeric_stable_id_maps_to_its_entity() {
    use regia_ecs::serialize::EntityIdMap;
    let mut registry = TypeRegistry::default();
    registry.register::<Link>();
    let mut world = World::new();
    let entity = world.spawn_empty().id();
    let mut map = EntityIdMap::new();
    assert!(map.insert(entity, 7));
    let mut codec = ReflectCodec::new(&registry).entity_mapper(&mut map);
    let value = codec
        .from_ron_typed("(target: 7)", reg::<Link>(&registry))
        .expect("a number is a stable id");
    assert_eq!(value.downcast_ref::<Link>(), Some(&Link { target: entity }));
}

#[derive(Reflect, Default, Debug, PartialEq)]
#[reflect(Default)]
struct Inner {
    a: u32,
}

#[derive(Reflect, Debug, PartialEq)]
enum Wrap {
    One(Inner),
    Two(u32, u32),
}

#[test]
fn a_one_field_tuple_variant_reads_as_a_newtype() {
    // `unwrap_variant_newtypes` is a RON newtype-variant rule: `One(a: 1)` is `One((a: 1))`.
    let mut registry = TypeRegistry::default();
    registry.register::<Wrap>();
    let mut codec = ReflectCodec::new(&registry);
    let value = codec
        .from_ron_typed(
            "#![enable(unwrap_variant_newtypes)]\nOne(a: 1)",
            reg::<Wrap>(&registry),
        )
        .expect("a newtype variant reads its unwrapped struct");
    assert_eq!(value.downcast_ref::<Wrap>(), Some(&Wrap::One(Inner { a: 1 })));
    let two = codec
        .from_ron_typed("Two(1, 2)", reg::<Wrap>(&registry))
        .expect("a two-field variant");
    assert_eq!(two.downcast_ref::<Wrap>(), Some(&Wrap::Two(1, 2)));
}
