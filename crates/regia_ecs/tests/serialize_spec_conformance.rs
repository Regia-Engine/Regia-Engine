// SPDX-License-Identifier: MPL-2.0
//! Spec 10 § 2.3 conformance: "A tuple struct is not filled: a missing element is an error",
//! while a named struct with the same `Default` is filled. Also for a tuple enum variant, in RON
//! (positional postcard cannot omit an element at all).

use regia_ecs::prelude::*;
use regia_ecs::serialize::{SerializeError, from_ron_as};

/// A tuple struct with a `Default`: still not filled.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default, PartialEq)]
struct Pair(u32, u32);

impl Default for Pair {
    fn default() -> Self {
        Self(7, 9)
    }
}

/// The named twin of [`Pair`], filled from its `Default`.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default, PartialEq)]
struct Named {
    a: u32,
    b: u32,
}

impl Default for Named {
    fn default() -> Self {
        Self { a: 7, b: 9 }
    }
}

#[derive(Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Default, PartialEq)]
enum Shape {
    #[default]
    Empty,
    Two(u8, u8),
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<Pair>();
    registry.register::<Named>();
    registry.register::<Shape>();
    registry
}

#[test]
fn a_tuple_struct_missing_an_element_is_an_error_and_a_named_struct_is_filled() -> Result<(), SerializeError>
{
    let registry = registry();
    let whole: Pair = from_ron_as(r#"{"serialize_spec_conformance::Pair": (1, 2)}"#, &registry)?;
    assert_eq!(whole, Pair(1, 2));
    let short = from_ron_as::<Pair>(r#"{"serialize_spec_conformance::Pair": (1)}"#, &registry);
    assert!(
        short.is_err(),
        "a tuple struct with a missing element loaded: {short:?}"
    );
    let named: Named = from_ron_as(r#"{"serialize_spec_conformance::Named": (a: 1)}"#, &registry)?;
    assert_eq!(named, Named { a: 1, b: 9 });
    Ok(())
}

#[test]
fn a_tuple_variant_missing_an_element_is_an_error() -> Result<(), SerializeError> {
    let registry = registry();
    let whole: Shape = from_ron_as(r#"{"serialize_spec_conformance::Shape": Two(1, 2)}"#, &registry)?;
    assert_eq!(whole, Shape::Two(1, 2));
    let short = from_ron_as::<Shape>(r#"{"serialize_spec_conformance::Shape": Two(1)}"#, &registry);
    assert!(
        short.is_err(),
        "a tuple variant with a missing element loaded: {short:?}"
    );
    Ok(())
}
