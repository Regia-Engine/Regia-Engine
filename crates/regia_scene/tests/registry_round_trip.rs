// SPDX-License-Identifier: MPL-2.0
//! Spec 10 § 2.3 acceptance: "round-trip every built-in component type through RON and postcard
//! (generated test over the registry)". The registry is the one `MinimalPlugins` and
//! `ScenePlugin` build; every engine (`regia_*`) type there with `ReflectComponent` or
//! `ReflectResource` and a `ReflectDefault` is written from its default and read back through
//! both encodings. Types the registry cannot construct (no `ReflectDefault`) are listed on stderr,
//! not silently counted as passing.
//!
//! Known defects are pinned in [`KNOWN_POSTCARD_FAILURES`] (ADR-5608): a type whose own serde
//! skips a field (`#[serde(skip_serializing_if = ...)]`) and that is reflected with
//! `#[reflect(Serialize, Deserialize)]` writes postcard it cannot read back, because postcard is
//! positional and the reader expects every field. The list must match exactly: a new failing type
//! fails this test, and so does fixing a listed one (remove it from the list then). The list is
//! empty since ADR-7232 (`LevelSettings` and `LevelLook` skip `None` fields only in text formats).
//!
//! `cargo test -p regia_scene --test registry_round_trip`

use regia_app::prelude::*;
use regia_ecs::reflect::std_traits::ReflectDefault;
use regia_ecs::reflect::{PartialReflect, Reflect, ReflectComponent, ReflectResource, TypeRegistry};
use regia_ecs::serialize::{from_postcard, from_ron, to_postcard, to_ron};

/// Types that fail only the postcard read today, with the field that causes it. Any other failure,
/// or a listed type that passes, fails the test.
const KNOWN_POSTCARD_FAILURES: [&str; 0] = [];

/// Same value: reflected equality where the type offers it, else the canonical RON text.
fn same(a: &dyn PartialReflect, b: &dyn PartialReflect, registry: &TypeRegistry) -> Result<(), String> {
    match a.reflect_partial_eq(b) {
        Some(true) => Ok(()),
        Some(false) => Err(format!("value changed: {a:?} became {b:?}")),
        None => {
            let (ta, tb) = (to_ron(a, registry), to_ron(b, registry));
            match (ta, tb) {
                (Ok(ta), Ok(tb)) if ta == tb => Ok(()),
                (ta, tb) => Err(format!("text changed: {ta:?} became {tb:?}")),
            }
        }
    }
}

fn round_trip(value: &dyn Reflect, registry: &TypeRegistry) -> Result<(), String> {
    let text = to_ron(value.as_partial_reflect(), registry).map_err(|e| format!("to_ron: {e}"))?;
    let back = from_ron(&text, registry)
        .map_err(|e| format!("from_ron: {e} in {text}"))?
        .into_value()
        .ok_or_else(|| format!("from_ron gave no value for {text}"))?;
    same(value.as_partial_reflect(), back.as_partial_reflect(), registry).map_err(|e| format!("ron: {e}"))?;
    let bytes = to_postcard(value.as_partial_reflect(), registry).map_err(|e| format!("to_postcard: {e}"))?;
    let back = from_postcard(&bytes, registry).map_err(|e| format!("from_postcard: {e}"))?;
    same(value.as_partial_reflect(), back.as_partial_reflect(), registry)
        .map_err(|e| format!("postcard: {e}"))?;
    let again =
        to_postcard(back.as_partial_reflect(), registry).map_err(|e| format!("to_postcard again: {e}"))?;
    if again != bytes {
        return Err("postcard: the bytes of the read value differ".to_owned());
    }
    Ok(())
}

#[test]
fn every_registered_engine_component_and_resource_round_trips_through_ron_and_postcard() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin));
    let registry = app.type_registry().read();
    let mut checked = Vec::new();
    let mut unconstructible = Vec::new();
    let mut failures = Vec::new();
    let mut known = Vec::new();
    for registration in registry.iter() {
        let path = registration.type_info().type_path();
        if !path.starts_with("regia_") {
            continue;
        }
        let component = registration.data::<ReflectComponent>().is_some();
        if !component && registration.data::<ReflectResource>().is_none() {
            continue;
        }
        let Some(default) = registration.data::<ReflectDefault>() else {
            unconstructible.push(path);
            continue;
        };
        let value = default.default();
        match round_trip(value.as_ref(), &registry) {
            Ok(()) if KNOWN_POSTCARD_FAILURES.contains(&path) => {
                failures.push(format!(
                    "{path}: now round-trips; remove it from KNOWN_POSTCARD_FAILURES"
                ));
            }
            Ok(()) => {}
            Err(e) if KNOWN_POSTCARD_FAILURES.contains(&path) && e.starts_with("from_postcard:") => {
                known.push(path);
            }
            Err(e) => failures.push(format!("{path}: {e}")),
        }
        checked.push((path, component));
    }
    checked.sort_unstable();
    unconstructible.sort_unstable();
    eprintln!("round-tripped {} types: {checked:?}", checked.len());
    eprintln!("no ReflectDefault, not constructed: {unconstructible:?}");
    eprintln!("known postcard failures (v0.5.x bug, ADR-5608): {known:?}");
    assert!(
        failures.is_empty(),
        "{} types failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(known, KNOWN_POSTCARD_FAILURES, "every pinned failure was seen");
    // The scene's core components must be among them, or the walk found nothing to check.
    for want in [
        "regia_scene::transform::Transform",
        "regia_scene::components::Name",
        "regia_scene::components::Tags",
    ] {
        assert!(
            checked.iter().any(|(p, c)| *p == want && *c),
            "{want} was not round-tripped: {checked:?}"
        );
    }
}
