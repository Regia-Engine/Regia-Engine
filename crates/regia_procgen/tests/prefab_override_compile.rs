// SPDX-License-Identifier: MPL-2.0
//! `spawn.prefab` override paths checked against the reflected types when a generator's graph
//! compiles (spec 63 § 13.2, ADR-3027): an unknown component or field, or a field no attribute can
//! fill, is refused before the run with the node id and the path; good paths compile.

mod common;

use std::collections::BTreeMap;

use common::*;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistry};
use regia_procgen::prefab_overrides::check_compiled_overrides;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, compile_for,
};
use regia_procgen::{GraphError, ProcGraph};
use regia_scene::components::EntityGuid;
use regia_scene::{GlobalTransform, Transform};

/// A game component with fields of each kind the check sorts.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Lamp {
    /// A scalar.
    pub intensity: f32,
    /// Three numbers.
    pub offset: Vec3,
    /// A name.
    pub label: String,
    /// Nothing an attribute converts into.
    pub tags: Vec<String>,
    /// A nested struct.
    pub light: LampLight,
}

/// A nested field.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
pub struct LampLight {
    /// A scalar inside it.
    pub range: f64,
}

/// Not a component.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
pub struct Plain {
    /// A field.
    pub x: f32,
}

/// A component with fields addressed by index.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Panel {
    /// Three numbers by index.
    pub tint: [f32; 3],
    /// A one-field newtype.
    pub wrap: Wrap,
}

/// A one-field newtype (spec 62 § 3.5: only 2-4 component attributes write into structs).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq)]
pub struct Wrap {
    /// The single field.
    pub value: f32,
}

fn prefab_graph(rows: &str) -> ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(2.0, 1.0, 1.0)"), ("step_m", "(1.0, 1.0, 1.0)")],
                ),
            ),
            (
                7,
                node(
                    "spawn.prefab",
                    &[("prefab", "\"lamp.prefab.ron\""), ("overrides", rows)],
                ),
            ),
        ],
        &[(1, "out", 7, "in")],
    )
}

fn registry() -> TypeRegistry {
    let mut r = TypeRegistry::default();
    r.register::<Lamp>();
    r.register::<LampLight>();
    r.register::<Plain>();
    r.register::<Panel>();
    r.register::<Wrap>();
    r.register::<[f32; 3]>();
    r.register::<Vec3>();
    r.register::<String>();
    r.register::<Vec<String>>();
    r.register::<f32>();
    r.register::<f64>();
    r
}

fn errors(rows: &str) -> Vec<GraphError> {
    check_compiled_overrides(&compile(&prefab_graph(rows)), &registry())
}

fn codes(rows: &str) -> Vec<String> {
    errors(rows).into_iter().map(|e| e.code).collect()
}

#[test]
fn good_paths_pass_the_compile_check() {
    let rows = "[(attr: \"density\", path: \"Lamp.intensity\"), (attr: \"position\", path: \"Lamp.offset\"), \
        (attr: \"kind\", path: \"Lamp.label\"), (attr: \"position:y\", path: \"Lamp.light.range\"), \
        (attr: \"custom\", path: \"Lamp.intensity\")]";
    assert_eq!(errors(rows), Vec::<GraphError>::new());
    // The full type path names the same component.
    let full = format!(
        "[(attr: \"density\", path: \"{}.intensity\")]",
        std::any::type_name::<Lamp>()
    );
    assert_eq!(errors(&full), Vec::<GraphError>::new());
}

#[test]
fn array_elements_are_addressable_by_index() {
    // `reflect_path` (the apply side) resolves `tint[1]`; the compile check must accept the
    // same syntax, or an array-element override can never pass both checks.
    let rows = "[(attr: \"density\", path: \"Panel.tint[1]\")]";
    assert_eq!(errors(rows), Vec::<GraphError>::new());
    // A 1-field newtype takes no scalar (spec 62 § 3.5): the compile check already refused it;
    // pinned so the apply side cannot drift wider again.
    let rows = "[(attr: \"density\", path: \"Panel.wrap\")]";
    let e = errors(rows);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].code, "override_type_mismatch");
    assert_eq!(e[0].node, Some(7));
}

#[test]
fn bad_paths_are_refused_with_the_node_id_and_the_path() {
    let cases = [
        ("Nope.intensity", "density", "override_path_unknown"),
        ("Lamp.nothing", "density", "override_path_unknown"),
        ("Lamp.light.nothing", "density", "override_path_unknown"),
        ("Plain.x", "density", "override_path_unknown"),
        ("Lamp.tags", "kind", "override_type_mismatch"),
        ("Lamp.intensity", "position", "override_type_mismatch"),
        ("Lamp.offset", "position:xz", "override_type_mismatch"),
        ("Lamp.label", "rotation", "override_type_mismatch"),
    ];
    for (path, attr, code) in cases {
        let rows = format!("[(attr: \"{attr}\", path: \"{path}\")]");
        let e = errors(&rows);
        assert_eq!(e.len(), 1, "{path} from {attr}: {e:?}");
        assert_eq!(e[0].code, code, "{path} from {attr}");
        assert_eq!(e[0].node, Some(7), "{path}: the spawn node");
        assert!(e[0].message.contains(path), "{path}: {}", e[0].message);
    }
    // Every bad row is reported, the good one is not.
    let rows = "[(attr: \"density\", path: \"Nope.a\"), (attr: \"density\", path: \"Lamp.intensity\"), \
        (attr: \"density\", path: \"Lamp.tags\")]";
    assert_eq!(codes(rows), ["override_path_unknown", "override_type_mismatch"]);
}

#[test]
fn a_generator_with_a_bad_path_fails_to_compile_and_a_good_one_compiles() {
    let compile_with = |rows: &str| {
        let mut app = App::new();
        let files: BTreeMap<String, String> =
            [("g.pgraph.ron".to_string(), prefab_graph(rows).to_ron())].into();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.init_resource::<AppTypeRegistry>();
        app.register_type::<Lamp>();
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(10.0, 10.0, 10.0),
                    },
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                EntityGuid(Guid(0x2912)),
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id();
        compile_for(app.world(), e)
            .map(|_| ())
            .map_err(|(code, _, errors)| (code, errors))
    };
    assert_eq!(
        compile_with("[(attr: \"density\", path: \"Lamp.intensity\")]"),
        Ok(())
    );
    match compile_with("[(attr: \"density\", path: \"Lamp.missing\")]") {
        Err((code, errors)) => {
            assert_eq!(code, "procgen_graph_invalid");
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(errors[0].code, "override_path_unknown");
            assert_eq!(errors[0].node, Some(7));
            assert!(errors[0].message.contains("Lamp.missing"));
        }
        Ok(()) => panic!("a missing field compiled"),
    }
}
