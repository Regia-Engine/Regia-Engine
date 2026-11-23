// SPDX-License-Identifier: MPL-2.0
//! `schema` fingerprint helpers on serde shapes the unit tests do not reach: field aliases, flatten,
//! transparent and `from` wrappers, untagged enums, unit structs, generics and the joined fingerprint.

use regia_core::schema::{
    join_field_lists, serde_field_names, serde_variant_names, try_serde_field_names, try_serde_variant_names,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[allow(dead_code)]
struct Aliased {
    #[serde(alias = "old_name")]
    name: String,
    other: u32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct NotAliased {
    name: String,
    other: u32,
}

#[test]
fn a_field_alias_changes_the_fingerprint() {
    // Dropping a documented alias breaks old files on disk, so it must trip `check-formats`.
    assert_ne!(serde_field_names::<Aliased>(), serde_field_names::<NotAliased>());
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Inner {
    a: u32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Flattening {
    first: u32,
    #[serde(flatten)]
    rest: Inner,
}

#[test]
fn a_flatten_struct_is_reported_as_unfingerprintable_or_lists_its_own_fields_only() {
    // Serde's derive reads a flattened struct through `deserialize_map`, so the capture never sees a
    // field list. It must say so (Err) instead of returning a list that hides `Inner::a`.
    assert!(try_serde_field_names::<Flattening>().is_err());
    assert!(serde_field_names::<Flattening>().is_empty());
}

#[derive(Deserialize)]
#[serde(transparent)]
#[allow(dead_code)]
struct Wrapper {
    inner: Inner,
}

#[test]
fn transparent_wrappers_fingerprint_as_the_inner_struct() {
    assert_eq!(serde_field_names::<Wrapper>(), serde_field_names::<Inner>());
}

#[derive(Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum Either {
    A(u32),
    B(String),
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Unit;

#[test]
fn untagged_enums_and_unit_structs_are_errors_not_empty_lists() {
    assert!(try_serde_variant_names::<Either>().is_err());
    assert!(try_serde_field_names::<Either>().is_err());
    assert!(try_serde_field_names::<Unit>().is_err());
    assert!(try_serde_variant_names::<Unit>().is_err());
    let message = try_serde_field_names::<Unit>().expect_err("a unit struct has no field list");
    assert!(message.contains("Unit"), "the error names the type: {message}");
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Strict {
    only: u32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Generic<T> {
    value: T,
    extra: u8,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Empty {}

#[test]
fn strict_generic_and_empty_structs() {
    assert_eq!(serde_field_names::<Strict>(), ["only"]);
    assert_eq!(serde_field_names::<Generic<u32>>(), ["value", "extra"]);
    assert_eq!(
        serde_field_names::<Generic<Inner>>(),
        serde_field_names::<Generic<u32>>()
    );
    assert_eq!(
        try_serde_field_names::<Empty>(),
        Ok(Vec::new()),
        "no fields is a valid shape, unlike a failure"
    );
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum Mixed {
    Unit,
    Newtype(u32),
    Tuple(u32, u32),
    Struct {
        x: u32,
    },
    #[serde(rename = "renamed")]
    Original,
    #[serde(skip_deserializing)]
    Skipped,
}

#[test]
fn enum_variant_names_cover_every_representation_and_skip_skipped() {
    assert_eq!(
        serde_variant_names::<Mixed>(),
        ["Unit", "Newtype", "Tuple", "Struct", "renamed"]
    );
}

#[test]
fn the_capture_is_repeatable() {
    for _ in 0..3 {
        assert_eq!(serde_field_names::<Aliased>(), serde_field_names::<Aliased>());
    }
}

#[test]
fn joined_fingerprints_keep_the_types_apart() {
    let a = vec!["x".to_owned(), "y".to_owned()];
    let b = vec!["z".to_owned()];
    assert_eq!(join_field_lists(&[a.clone(), b.clone()]), "x,y;z");
    assert_eq!(join_field_lists(&[]), "");
    assert_eq!(join_field_lists(std::slice::from_ref(&a)), "x,y");
    // Moving a field across the `;` boundary changes the string.
    assert_ne!(
        join_field_lists(&[vec!["x".into()], vec!["y".into(), "z".into()]]),
        join_field_lists(&[vec!["x".into(), "y".into()], vec!["z".into()]])
    );
    // An empty struct in the middle still shows up as its own slot.
    assert_eq!(join_field_lists(&[a.clone(), Vec::new(), b.clone()]), "x,y;;z");
    assert_ne!(
        join_field_lists(&[a.clone(), Vec::new(), b.clone()]),
        join_field_lists(&[a, b])
    );
    // Order matters (the root struct comes first).
    assert_ne!(
        join_field_lists(&[vec!["p".into()], vec!["q".into()]]),
        join_field_lists(&[vec!["q".into()], vec!["p".into()]])
    );
}
