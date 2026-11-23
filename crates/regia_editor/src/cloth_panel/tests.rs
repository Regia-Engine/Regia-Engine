// SPDX-License-Identifier: MPL-2.0
//! Cloth panel tests (spec 55 § 6, ADR-2884): one-field saves, hand-edit equivalence, paint equal to
//! `physics.cloth_edit_map`, undo to the original bytes, the `cloth` gizmo group, soft bodies, the agent
//! methods and headless drawing.

use std::path::PathBuf;

use regia::physics::cloth::asset::{ClothSource, SimMeshInput};
use regia_framework::cloth::{ClothSim, SoftSimSource};
use serde_json::json;

use super::*;
use crate::data_panels::test_ui::draw_frames;

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

const REL: &str = "cloth/flag.cloth.ron";
const SOFT_REL: &str = "cloth/ball.soft.ron";

/// A 5 x 5 sheet in the XY plane, spacing 0.1 (vertex `r * 5 + c` at `(0.1 c, -0.1 r, 0)`).
fn sheet() -> SimMeshInput {
    let mut mesh = SimMeshInput::default();
    for r in 0..5u32 {
        for c in 0..5u32 {
            mesh.positions
                .push(Vec3::new(c as f32 * 0.1, -(r as f32) * 0.1, 0.0));
        }
    }
    for r in 0..4u32 {
        for c in 0..4u32 {
            let i = r * 5 + c;
            mesh.triangles.push([i, i + 5, i + 1]);
            mesh.triangles.push([i + 1, i + 5, i + 6]);
        }
    }
    mesh
}

fn source() -> ClothSource {
    ClothSource::from_ron_str(
        r#"(format_version: 1, name: "flag", sim_mesh: (source: "flag.glb", node: "Flag"),
            params: (substeps: 4, damping: 0.5),
            maps: { "reach": Sparse(default: 0.3, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0)]),
                    "standoff": Sparse(default: 0.02, entries: []) })"#,
    )
    .expect("parses")
}

/// A project with the canonical `flag.cloth.ron`, a `ClothSim` using its cooked asset and a
/// `ClothSimSource` naming the file on the same entity. Returns the world, the file and its text.
fn fixture(text: Option<&str>) -> (World, PathBuf, String, Entity) {
    let root = regia_test_utils::unique_temp_dir("cloth-panel");
    let text = text.map_or_else(|| source().to_ron_string().expect("ron"), str::to_owned);
    let file = root.join(REL);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, &text).expect("write");
    let mut world = World::new();
    crate::mesh_source::register_level_types(&mut world);
    world.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    world.insert_resource(UndoStack::new());
    let asset = Arc::new(ClothAsset::cook(&source(), &sheet(), None, &[]).expect("cooks"));
    let entity = world
        .spawn((
            ClothSim::new(asset),
            ClothSimSource {
                asset: REL.into(),
                ..ClothSimSource::default()
            },
            GlobalTransform::default(),
        ))
        .id();
    (world, file, text, entity)
}

fn doc(world: &World) -> &ClothDoc {
    world.resource::<ClothPanel>().doc().expect("open")
}

fn diff_lines(a: &str, b: &str) -> Vec<(String, String)> {
    assert_eq!(
        a.lines().count(),
        b.lines().count(),
        "same line count:\n{a}\n---\n{b}"
    );
    a.lines()
        .zip(b.lines())
        .filter(|(x, y)| x != y)
        .map(|(x, y)| (x.to_owned(), y.to_owned()))
        .collect()
}

/// Acceptance: editing one parameter writes a file that differs from the original only in that
/// field's line, and it loads to the same `ClothTuning` (and parameters) as the same change made by
/// hand in the text.
#[test]
fn one_param_edit_writes_only_that_field_and_matches_a_hand_edit() {
    let (mut world, file, original, _) = fixture(None);
    open_file(&mut world, REL).expect("open");
    assert_eq!(doc(&world).sim_from, "instance");
    assert!(edit_file(&mut world, &ClothChange::Param("damping".into(), 1.25)).expect("edit"));
    assert!(save_file(&mut world).expect("save") > 0);
    let written = std::fs::read_to_string(&file).expect("read");
    let changed = diff_lines(&original, &written);
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].0.trim_start().starts_with("damping:"), "{changed:?}");
    assert!(changed[0].1.contains("1.25"), "{changed:?}");
    let hand = original.replace(changed[0].0.as_str(), &changed[0].0.replace("0.5", "1.25"));
    assert_ne!(hand, original, "the hand edit changed the text");
    let from_panel = ClothSource::from_ron_str(&written).expect("panel file loads");
    let from_hand = ClothSource::from_ron_str(&hand).expect("hand file loads");
    assert_eq!(from_panel.params, from_hand.params);
    assert_eq!(from_panel.params.tuning(), from_hand.params.tuning());
    assert_eq!(from_panel, from_hand);
}

/// Acceptance: undo restores the file bytes, even for a hand-written (non-canonical) file; an
/// unedited file is never rewritten.
#[test]
fn undo_restores_the_original_bytes() {
    let hand_written = "(format_version: 1, name: \"flag\",\n  // a comment the canonical form drops\n  \
        sim_mesh: (source: \"flag.glb\", node: \"Flag\"), params: (substeps: 4, damping: 0.5))\n";
    let (mut world, file, original, _) = fixture(Some(hand_written));
    open_file(&mut world, REL).expect("open");
    assert_eq!(
        save_file(&mut world).expect("save"),
        0,
        "unedited: nothing written"
    );
    edit_file(&mut world, &ClothChange::Param("substeps".into(), 6.0)).expect("edit");
    edit_file(&mut world, &ClothChange::Preset(Some("silk".into()))).expect("preset");
    save_file(&mut world).expect("save");
    assert_ne!(std::fs::read_to_string(&file).expect("read"), original);
    assert!(crate::command::undo_world(&mut world));
    assert!(crate::command::undo_world(&mut world));
    assert!(doc(&world).dirty());
    save_file(&mut world).expect("save");
    assert_eq!(std::fs::read(&file).expect("read"), original.as_bytes());
    // Redo brings the edit back.
    assert!(crate::command::redo_world(&mut world));
    let PanelSource::Cloth(c) = &doc(&world).source else {
        panic!("cloth");
    };
    assert_eq!(c.params.substeps, 6);
}

/// A slider scrub on one field is one undo step; bad values are refused and change nothing.
#[test]
fn scrubs_coalesce_and_bad_values_are_refused() {
    let (mut world, _, _, _) = fixture(None);
    open_file(&mut world, REL).expect("open");
    for v in [0.6, 0.7, 0.8] {
        edit_file(&mut world, &ClothChange::Param("damping".into(), v)).expect("scrub");
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    let cases = [
        (
            ClothChange::Param("damping".into(), 50.0),
            "cloth_panel_bad_value",
        ),
        (
            ClothChange::Param("damping".into(), f32::NAN),
            "cloth_panel_bad_value",
        ),
        (
            ClothChange::Param("substeps".into(), 2.5),
            "cloth_panel_bad_value",
        ),
        (
            ClothChange::Param("wind_scale".into(), 11.0),
            "cloth_panel_bad_value",
        ),
        (
            ClothChange::Param("nope".into(), 1.0),
            "cloth_panel_unknown_param",
        ),
        (
            ClothChange::Preset(Some("cardboard".into())),
            "cloth_panel_unknown_preset",
        ),
    ];
    let before = doc(&world).source.clone();
    for (change, code) in cases {
        let e = edit_file(&mut world, &change).expect_err("refused");
        assert_eq!(e.code, code, "{change:?}");
    }
    assert_eq!(doc(&world).source, before);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    // Self-collision, wind model and wind scale round-trip through the file.
    edit_file(
        &mut world,
        &ClothChange::SelfCollision(SelfCollisionSetting::Spheres { radius: 0.02 }),
    )
    .expect("self");
    edit_file(
        &mut world,
        &ClothChange::WindModel(WindSetting::Push { gain: 0.5 }),
    )
    .expect("wind");
    edit_file(&mut world, &ClothChange::Param("wind_scale".into(), 0.5)).expect("scale");
    save_file(&mut world).expect("save");
    let PanelSource::Cloth(c) = &doc(&world).source else {
        panic!("cloth");
    };
    let text = c.to_ron_string().expect("ron");
    let back = ClothSource::from_ron_str(&text).expect("loads");
    assert_eq!(
        back.params.self_collision,
        SelfCollisionSetting::Spheres { radius: 0.02 }
    );
    assert_eq!(back.params.wind_model, WindSetting::Push { gain: 0.5 });
    assert_eq!(back.params.wind_scale, 0.5);
}

/// Acceptance: a paint stroke gives exactly the map `physics.cloth_edit_map` gives for the same
/// inputs (its dabs as sphere edits), on the instance and in the saved file; undo restores the
/// instance's asset and the file.
#[test]
fn a_paint_stroke_equals_cloth_edit_map_and_undoes() {
    let brush = ClothBrush {
        map: "reach".into(),
        value: 0.0,
        radius: 0.12,
        falloff: Falloff::Linear,
        strength: 0.75,
        spacing: None,
    };
    let points = [[0.05, -0.2, 0.0], [0.35, -0.25, 0.0]];
    // Panel side.
    let (mut world, file, original, entity) = fixture(None);
    open_file(&mut world, REL).expect("open");
    let result = paint_stroke(&mut world, &points, &brush).expect("stroke");
    assert!(result.dabs.len() > 2, "spaced dabs between the points");
    assert_eq!(result.dabs.first(), Some(&points[0]));
    assert_eq!(result.dabs.last(), Some(&points[1]));
    assert!(result.changed > 0);
    assert_eq!(result.instances, 1);
    let painted = world.get::<ClothSim>(entity).expect("sim").asset.clone();
    // Agent side: the same dabs through physics.cloth_edit_map on a fresh instance.
    let (mut agent_world, _, _, agent_entity) = fixture(None);
    regia_agent::cloth::install_cloth_methods(&mut agent_world);
    agent_world.init_resource::<ClothSet>();
    for d in &result.dabs {
        let edit = serde_json::to_value(brush.dab(*d)).expect("json");
        let handler = agent_world
            .resource::<regia_agent::extension::AgentExtension>()
            .method("physics.cloth_edit_map")
            .map(|(_, h)| h)
            .expect("registered");
        handler(
            &mut agent_world,
            &json!({ "asset": "flag", "map": "reach", "edit": edit }),
        )
        .expect("edit_map");
    }
    let edited = agent_world
        .get::<ClothSim>(agent_entity)
        .expect("sim")
        .asset
        .clone();
    let bits = |a: &ClothAsset| -> Vec<u32> {
        a.lod0()
            .maps
            .get("reach")
            .expect("reach")
            .iter()
            .map(|v| v.to_bits())
            .collect()
    };
    assert_eq!(bits(&painted), bits(&edited));
    assert_eq!(painted.lods, edited.lods);
    // The file holds the painted map after a save.
    save_file(&mut world).expect("save");
    let saved = ClothSource::from_ron_str(&std::fs::read_to_string(&file).expect("read")).expect("loads");
    let cooked = ClothAsset::cook(&saved, &sheet(), None, &[]).expect("cooks");
    assert_eq!(bits(&cooked), bits(&edited));
    // One stroke = one undo step; undo puts the instance's asset and the file back.
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    assert!(crate::command::undo_world(&mut world));
    let back = world.get::<ClothSim>(entity).expect("sim").asset.clone();
    assert_eq!(
        back.lod0().maps,
        ClothAsset::cook(&source(), &sheet(), None, &[])
            .expect("cooks")
            .lod0()
            .maps
    );
    save_file(&mut world).expect("save");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);
}

/// Stroke dabs: spaced evenly, deterministic, the points themselves with spacing 0, limits refused.
#[test]
fn stroke_dabs_are_spaced_and_bounded() {
    let d = stroke_dabs(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]], 0.25).expect("dabs");
    assert_eq!(d.len(), 5);
    assert_eq!(d[2], [0.5, 0.0, 0.0]);
    assert_eq!(
        stroke_dabs(&[[0.0; 3], [1.0, 0.0, 0.0]], 0.0)
            .expect("dabs")
            .len(),
        2
    );
    assert_eq!(
        stroke_dabs(&[], 0.1).expect_err("empty").code,
        "cloth_panel_bad_value"
    );
    assert_eq!(
        stroke_dabs(&[[0.0; 3], [f32::NAN, 0.0, 0.0]], 0.1)
            .expect_err("nan")
            .code,
        "cloth_panel_bad_value"
    );
    assert_eq!(
        stroke_dabs(&[[0.0; 3], [1000.0, 0.0, 0.0]], 0.01)
            .expect_err("too many")
            .code,
        "cloth_panel_bad_value"
    );
}

/// Acceptance: the `cloth` gizmo group is off by default and draws a `reach` sphere and a
/// `standoff` plane per selected vertex (N shapes for N vertices) when on, plus every proxy.
#[test]
fn the_cloth_gizmo_group_is_off_by_default_and_draws_per_selected_vertex() {
    let (mut world, _, _, _) = fixture(None);
    open_file(&mut world, REL).expect("open");
    world.spawn((
        ClothBodyProxy {
            name: "spine".into(),
            shape: ProxyShape::Sphere {
                center: Vec3::ZERO,
                radius: 0.2,
            },
            friction: 0.3,
        },
        GlobalTransform::default(),
    ));
    panel_mut(&mut world).selected = vec![0, 7, 12];
    assert!(!ClothPanel::default().gizmos);
    assert!(cloth_gizmo_shapes(&world).is_empty(), "off by default");
    assert!(cloth_gizmo_segments(&world).is_empty());
    for n in [3usize, 1, 0] {
        let selected: Vec<u32> = [0u32, 7, 12].into_iter().take(n).collect();
        panel_mut(&mut world).selected = selected;
        panel_mut(&mut world).gizmos = true;
        let shapes = cloth_gizmo_shapes(&world);
        let count = |k: &str| shapes.iter().filter(|s| s.kind == k).count();
        assert_eq!(count("reach"), n);
        assert_eq!(count("standoff"), n);
        assert_eq!(count("proxy"), 1);
        assert!(shapes.iter().all(|s| !s.segments.is_empty()));
    }
    // The reach sphere's radius is the vertex's reach (vertex 7: 0.3 m), the standoff plane sits
    // 0.02 m behind the vertex along its normal.
    panel_mut(&mut world).selected = vec![7];
    let shapes = cloth_gizmo_shapes(&world);
    let reach = shapes.iter().find(|s| s.kind == "reach").expect("reach");
    let p = Vec3::new(0.2, -0.1, 0.0);
    assert!(
        reach
            .segments
            .iter()
            .all(|(a, _, _)| (a.distance(p) - 0.3).abs() < 1e-4)
    );
    let plane = shapes.iter().find(|s| s.kind == "standoff").expect("standoff");
    let centre = plane
        .segments
        .iter()
        .map(|(a, _, _)| *a)
        .fold(Vec3::ZERO, |s, a| s + a)
        / plane.segments.len() as f32;
    assert!((centre.z.abs() - 0.02).abs() < 1e-3, "{centre:?}");
}

/// Blend and LOD distances are instance fields: they go through `SetProperty` on the shared undo
/// stack, not the file.
#[test]
fn blend_and_lod_distances_edit_the_instance_with_undo() {
    let (mut world, file, original, entity) = fixture(None);
    open_file(&mut world, REL).expect("open");
    assert_eq!(world.resource::<ClothPanel>().target, Some(entity));
    assert!(edit_instance(&mut world, &InstanceChange::Blend(0.25)).expect("blend"));
    assert!(edit_instance(&mut world, &InstanceChange::LodDistances([5.0, 10.0, 20.0])).expect("lod"));
    let saved = world.get::<ClothSimSource>(entity).expect("source");
    assert_eq!(saved.blend, 0.25);
    assert_eq!(saved.lod_distances, [5.0, 10.0, 20.0]);
    assert_eq!(
        edit_instance(&mut world, &InstanceChange::LodDistances([5.0, 2.0, 20.0]))
            .expect_err("descending")
            .code,
        "cloth_panel_bad_value"
    );
    assert_eq!(
        edit_instance(&mut world, &InstanceChange::Blend(1.5))
            .expect_err("range")
            .code,
        "cloth_panel_bad_value"
    );
    crate::command::undo_world(&mut world);
    crate::command::undo_world(&mut world);
    assert_eq!(world.get::<ClothSimSource>(entity).expect("source").blend, 1.0);
    assert!(!doc(&world).dirty(), "the file is untouched");
    assert_eq!(save_file(&mut world).expect("save"), 0);
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);
    panel_mut(&mut world).target = None;
    assert_eq!(
        edit_instance(&mut world, &InstanceChange::Blend(0.5))
            .expect_err("no target")
            .code,
        "cloth_panel_no_target"
    );
}

/// The simulate preview steps a private solver: deterministic, the level's instance untouched,
/// and it follows a parameter edit.
#[test]
fn the_simulate_preview_is_private_and_deterministic() {
    let (mut world, _, _, entity) = fixture(None);
    open_file(&mut world, REL).expect("open");
    let before = world.get::<ClothSim>(entity).expect("sim").asset.clone();
    assert_eq!(step_preview(&mut world, 60).expect("steps"), 60);
    let a = world
        .resource::<ClothPanel>()
        .preview()
        .expect("preview")
        .solver
        .state_digest();
    panel_mut(&mut world).preview = None;
    step_preview(&mut world, 60).expect("steps");
    let b = world
        .resource::<ClothPanel>()
        .preview()
        .expect("preview")
        .solver
        .state_digest();
    assert_eq!(a, b);
    // The pinned top row (reach 0) stays at rest; the others stay within their reach (0.3 m).
    let preview = world.resource::<ClothPanel>().preview().expect("preview");
    assert!(
        preview.solver.particles()[2]
            .position
            .distance(Vec3::new(0.2, 0.0, 0.0))
            < 1e-4
    );
    assert!(
        preview.solver.particles()[22]
            .position
            .distance(Vec3::new(0.2, -0.4, 0.0))
            <= 0.3 + 1e-4
    );
    assert_eq!(preview.steps, 60);
    assert!(Arc::ptr_eq(
        &before,
        &world.get::<ClothSim>(entity).expect("sim").asset
    ));
    edit_file(&mut world, &ClothChange::Param("gravity_scale".into(), 0.0)).expect("edit");
    assert!(
        world.resource::<ClothPanel>().preview().is_none(),
        "an edit drops the stale preview"
    );
}

/// A soft body gets the inspector's parameter subset (and its model's numbers); cloth-only
/// settings and paint are refused.
#[test]
fn a_soft_body_gets_the_parameter_subset() {
    let (mut world, _, _, _) = fixture(None);
    let root = crate::mesh_source::project_root(&world).expect("root");
    let text = "(format_version: 1, name: \"ball\", model: Pressure(pressure: 1.0, volume_compliance: 0.0), \
        surface: (source: \"ball.glb\", node: \"Ball\"), params: (substeps: 8))";
    std::fs::write(root.join(SOFT_REL), text).expect("write");
    let soft = world
        .spawn(SoftSimSource {
            asset: SOFT_REL.into(),
            ..SoftSimSource::default()
        })
        .id();
    panel_mut(&mut world).target = Some(soft);
    open_file(&mut world, SOFT_REL).expect("open");
    for (name, v) in [
        ("damping", 0.3),
        ("substeps", 4.0),
        ("pressure", 1.4),
        ("gravity_scale", 0.5),
    ] {
        edit_file(&mut world, &ClothChange::Param(name.into(), v)).expect(name);
    }
    for (name, code) in [
        ("stiffness", "cloth_panel_unknown_param"),
        ("wind_scale", "cloth_panel_unknown_param"),
    ] {
        assert_eq!(
            edit_file(&mut world, &ClothChange::Param(name.into(), 0.5))
                .expect_err(name)
                .code,
            code
        );
    }
    assert_eq!(
        edit_file(&mut world, &ClothChange::Param("pressure".into(), -1.0))
            .expect_err("neg")
            .code,
        "cloth_panel_bad_value"
    );
    assert_eq!(
        edit_file(&mut world, &ClothChange::Preset(Some("silk".into())))
            .expect_err("cloth only")
            .code,
        "cloth_panel_unknown_param"
    );
    assert_eq!(
        paint_stroke(&mut world, &[[0.0; 3]], &ClothBrush::default())
            .expect_err("paint")
            .code,
        "cloth_panel_no_sim_mesh"
    );
    edit_instance(&mut world, &InstanceChange::LodDistances([4.0, 8.0, 16.0])).expect("lod");
    assert_eq!(
        world.get::<SoftSimSource>(soft).expect("soft").lod_distances,
        [4.0, 8.0, 16.0]
    );
    assert_eq!(
        edit_instance(&mut world, &InstanceChange::Blend(0.5))
            .expect_err("cloth only")
            .code,
        "cloth_panel_no_target"
    );
    save_file(&mut world).expect("save");
    let back = SoftSource::from_ron_str(&std::fs::read_to_string(root.join(SOFT_REL)).expect("read"))
        .expect("loads");
    assert_eq!(back.params.substeps, 4);
    assert_eq!(back.params.damping, 0.3);
    assert_eq!(
        back.model,
        SoftModelSource::Pressure {
            pressure: 1.4,
            volume_compliance: 0.0
        }
    );
}

/// Without a `ClothSim` and without the glTF the file names, the inspector works and paint says why
/// it cannot run; bad paths and missing files have their codes.
#[test]
fn no_sim_mesh_and_path_errors_have_codes() {
    let (mut world, _, _, entity) = fixture(None);
    world.entity_mut(entity).remove::<ClothSim>();
    open_file(&mut world, REL).expect("open");
    assert!(doc(&world).asset.is_none());
    assert!(doc(&world).sim_error.is_some());
    edit_file(&mut world, &ClothChange::Param("damping".into(), 1.0)).expect("inspector works");
    assert_eq!(
        paint_stroke(&mut world, &[[0.0; 3]], &ClothBrush::default())
            .expect_err("no mesh")
            .code,
        "cloth_panel_no_sim_mesh"
    );
    assert_eq!(
        open_file(&mut world, "../x.cloth.ron").expect_err("path").code,
        "cloth_panel_bad_path"
    );
    assert_eq!(
        open_file(&mut world, "x.mesh.ron").expect_err("suffix").code,
        "cloth_panel_bad_path"
    );
    assert_eq!(
        open_file(&mut world, "cloth/none.cloth.ron")
            .expect_err("io")
            .code,
        "cloth_panel_io_failed"
    );
    let root = crate::mesh_source::project_root(&world).expect("root");
    std::fs::write(
        root.join("cloth/bad.cloth.ron"),
        "(format_version: 9, name: \"x\")",
    )
    .expect("write");
    assert_eq!(
        open_file(&mut world, "cloth/bad.cloth.ron")
            .expect_err("parse")
            .code,
        "cloth_panel_parse_failed"
    );
    let mut empty = World::new();
    assert_eq!(
        open_file(&mut empty, REL).expect_err("project").code,
        "cloth_panel_no_project"
    );
    assert_eq!(
        save_file(&mut empty).expect_err("project").code,
        "cloth_panel_no_project"
    );
}

/// The agent methods drive the same model: state, edit (params, gizmos, selection, paint brush,
/// preview, undo, save) and a paint stroke with its values; errors carry the documented reasons.
#[test]
fn the_agent_methods_drive_the_panel() {
    let (mut world, file, original, entity) = fixture(None);
    let state = method("editor.cloth_panel_state")(&mut world, &json!({})).expect("state");
    assert_eq!(state["open"], false);
    assert_eq!(state["gizmos"], false);
    let opened = method("editor.cloth_panel_edit")(
        &mut world,
        &json!({ "path": REL, "params": { "damping": 0.9, "wind_scale": 0.5 }, "gizmos": true, "selected": [1, 2] }),
    )
    .expect("edit");
    assert_eq!(opened["source"]["params"]["damping"], json!(0.9f32));
    assert_eq!(opened["source"]["params"]["wind_scale"], json!(0.5));
    assert_eq!(opened["gizmo_shapes"]["reach"], 2);
    assert_eq!(opened["gizmo_shapes"]["standoff"], 2);
    assert_eq!(opened["instance"]["entity"], json!(entity.to_bits()));
    assert_eq!(opened["sim_mesh"]["vertices"], 25);
    // All or nothing: one bad name refuses the whole call.
    let bad =
        method("editor.cloth_panel_edit")(&mut world, &json!({ "params": { "damping": 0.1, "nope": 1 } }))
            .expect_err("unknown");
    assert_eq!(
        bad.data.as_ref().expect("data")["reason"],
        "cloth_panel_unknown_param"
    );
    let state = method("editor.cloth_panel_state")(&mut world, &json!({})).expect("state");
    assert_eq!(state["source"]["params"]["damping"], json!(0.9f32));
    let stroke = method("editor.cloth_paint_stroke")(
        &mut world,
        &json!({ "points": [[0.1, -0.1, 0.0]], "map": "standoff", "value": 0.0, "radius": 0.15, "values": true }),
    )
    .expect("stroke");
    assert_eq!(stroke["dabs"].as_array().map(Vec::len), Some(1));
    assert_eq!(stroke["values"].as_array().map(Vec::len), Some(25));
    let preview =
        method("editor.cloth_panel_edit")(&mut world, &json!({ "simulate": true, "preview_steps": 10 }))
            .expect("preview");
    assert_eq!(preview["preview"]["steps"], 10);
    let refused =
        method("editor.cloth_panel_edit")(&mut world, &json!({ "preview_steps": 601 })).expect_err("limit");
    assert_eq!(
        refused.data.as_ref().expect("data")["reason"],
        "cloth_panel_bad_value"
    );
    let saved = method("editor.cloth_panel_edit")(&mut world, &json!({ "save": true })).expect("save");
    assert!(saved["written"].as_u64().is_some_and(|n| n > 0));
    // Undo the stroke and the two parameters, save: the original bytes.
    for _ in 0..3 {
        method("editor.cloth_panel_edit")(&mut world, &json!({ "undo": true })).expect("undo");
    }
    method("editor.cloth_panel_edit")(&mut world, &json!({ "save": true })).expect("save");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);
    let missing = method("editor.cloth_panel_edit")(&mut world, &json!({ "path": "cloth/none.cloth.ron" }))
        .expect_err("missing");
    assert_eq!(
        missing.data.as_ref().expect("data")["reason"],
        "cloth_panel_io_failed"
    );
    let no_entity = method("editor.cloth_panel_state")(&mut world, &json!({ "entity": 999_999_999u64 }))
        .expect_err("no entity");
    assert_eq!(
        no_entity.data.as_ref().expect("data")["reason"],
        "cloth_panel_no_target"
    );
    let names: Vec<&str> = methods().iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        [
            "editor.cloth_panel_state",
            "editor.cloth_panel_edit",
            "editor.cloth_paint_stroke"
        ]
    );
}

/// Picking finds the vertex under a ray on the target-placed rest mesh.
#[test]
fn pick_finds_the_vertex_under_the_ray() {
    let (mut world, _, _, _) = fixture(None);
    open_file(&mut world, REL).expect("open");
    let hit = pick_sim_vertex(&world, Vec3::new(0.21, -0.11, 1.0), Vec3::NEG_Z).expect("hit");
    assert_eq!(hit.0, 7);
    assert!((hit.1.z).abs() < 1e-6);
    assert!(pick_sim_vertex(&world, Vec3::new(5.0, 5.0, 1.0), Vec3::NEG_Z).is_none());
}

/// The ramp runs blue → green → red over `[low, high]` and clamps outside it.
#[test]
fn the_ramp_is_monotone_in_red_and_clamps() {
    let ramp = [0.0, 0.2];
    assert_eq!(ramp_color(-1.0, ramp), ramp_color(0.0, ramp));
    assert_eq!(ramp_color(5.0, ramp), ramp_color(0.2, ramp));
    let reds: Vec<u8> = (0..=10).map(|k| ramp_color(k as f32 * 0.02, ramp).r()).collect();
    assert!(reds.windows(2).all(|w| w[0] <= w[1]), "{reds:?}");
    assert_ne!(ramp_color(0.0, ramp), ramp_color(0.2, ramp));
}

/// The panel draws headless with and without a file, with paint, gizmos and the preview on, and
/// drawing makes no edit.
#[test]
fn the_panel_draws_headless() {
    let (mut world, _, _, _) = fixture(None);
    assert!(!draw_frames(&mut world, 2, cloth_panel).is_empty());
    open_file(&mut world, REL).expect("open");
    {
        let mut panel = panel_mut(&mut world);
        panel.painting = true;
        panel.gizmos = true;
        panel.simulate = true;
        panel.selected = vec![3];
    }
    assert!(!draw_frames(&mut world, 3, cloth_panel).is_empty());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    let preview = world.resource::<ClothPanel>().preview().expect("simulating");
    assert_eq!(preview.steps, 3, "one preview step per drawn frame");
    let segments = cloth_gizmo_segments(&world);
    assert!(segments.len() >= 25 * 2, "ramp crosses for every vertex");
}

/// Review rc1-rev-editor-k finding 4: opening `cloth/flag.cloth.ron` bound any instance whose asset
/// path merely ended with it (`armor/cloth/flag.cloth.ron`, `loincloth/flag.cloth.ron`), the lowest
/// entity first, and instance edits then went to that wrong entity. Only the same file binds.
#[test]
fn opening_a_file_binds_only_an_instance_of_that_same_file() {
    let (mut world, _, _, other) = fixture(None);
    world.get_mut::<ClothSimSource>(other).expect("source").asset = "armor/cloth/flag.cloth.ron".into();
    world.spawn(ClothSimSource {
        asset: "loincloth/flag.cloth.ron".into(),
        ..ClothSimSource::default()
    });
    // Only other files' instances: nothing binds (not even the `ClothSim` named `flag`, whose source
    // names the armor file), and an instance edit is refused rather than landing on one of them.
    open_file(&mut world, REL).expect("open");
    assert_eq!(world.resource::<ClothPanel>().target, None);
    assert_eq!(
        edit_instance(&mut world, &InstanceChange::Blend(0.25))
            .expect_err("no target")
            .code,
        "cloth_panel_no_target"
    );
    assert_eq!(world.get::<ClothSimSource>(other).expect("source").blend, 1.0);
    // The instance of this file binds, written with `./`.
    let mine = world
        .spawn(ClothSimSource {
            asset: "./cloth/flag.cloth.ron".into(),
            ..ClothSimSource::default()
        })
        .id();
    open_file(&mut world, REL).expect("reopen");
    assert_eq!(world.resource::<ClothPanel>().target, Some(mine));
}

/// Finding 4, the ambiguity half: when two instances name the file, the panel binds neither (an
/// edit answers `cloth_panel_no_target`) until `entity` picks one.
#[test]
fn two_instances_of_the_file_bind_neither() {
    let (mut world, _, _, first) = fixture(None);
    let second = world
        .spawn(ClothSimSource {
            asset: format!("assets/{REL}"),
            ..ClothSimSource::default()
        })
        .id();
    open_file(&mut world, REL).expect("open");
    assert_eq!(world.resource::<ClothPanel>().target, None);
    assert_eq!(
        edit_instance(&mut world, &InstanceChange::Blend(0.25))
            .expect_err("ambiguous")
            .code,
        "cloth_panel_no_target"
    );
    panel_mut(&mut world).target = Some(second);
    edit_instance(&mut world, &InstanceChange::Blend(0.25)).expect("picked");
    assert_eq!(world.get::<ClothSimSource>(second).expect("second").blend, 0.25);
    assert_eq!(world.get::<ClothSimSource>(first).expect("first").blend, 1.0);
}

#[test]
fn same_project_file_compares_whole_components() {
    for (a, b) in [
        ("cloth/flag.cloth.ron", "cloth/flag.cloth.ron"),
        ("./cloth/flag.cloth.ron", "cloth/flag.cloth.ron"),
        ("cloth\\flag.cloth.ron", "cloth/./flag.cloth.ron"),
        ("assets/cloth/flag.cloth.ron", "cloth/flag.cloth.ron"),
        ("cloth/flag.cloth.ron", "assets/cloth/flag.cloth.ron"),
    ] {
        assert!(same_project_file(a, b), "{a} = {b}");
    }
    for (a, b) in [
        ("armor/cloth/flag.cloth.ron", "cloth/flag.cloth.ron"),
        ("loincloth/flag.cloth.ron", "cloth/flag.cloth.ron"),
        ("cloth/flag.cloth.ron", "flag.cloth.ron"),
        ("assets/armor/flag.cloth.ron", "flag.cloth.ron"),
        ("cloth/bigflag.cloth.ron", "cloth/flag.cloth.ron"),
    ] {
        assert!(!same_project_file(a, b), "{a} != {b}");
    }
}

/// Review rc1-rev-editor-k finding 14: a drag stops growing the stroke at the cap and says so at
/// once; the points kept still make a stroke `stroke_dabs` accepts.
#[test]
fn a_long_drag_stops_at_the_stroke_cap() {
    let (mut world, _, _, _) = fixture(None);
    open_file(&mut world, REL).expect("open");
    let mut panel = panel_mut(&mut world);
    for i in 0..MAX_STROKE_POINTS + 10 {
        push_stroke_point(&mut panel, [i as f32 * 1e-4, 0.0, 0.0]);
    }
    assert_eq!(panel.stroke.len(), MAX_STROKE_POINTS);
    assert_eq!(
        panel.error.as_ref().map(|e| e.code.as_str()),
        Some("cloth_panel_bad_value")
    );
    assert!(stroke_dabs(&panel.stroke, 0.01).is_ok());
}

/// A closed 0.4 m cube surface (8 vertices, 12 outward triangles) for cooking a soft body.
fn cube() -> SimMeshInput {
    let mut mesh = SimMeshInput::default();
    for i in 0..8u32 {
        mesh.positions.push(Vec3::new(
            if i & 1 == 0 { 0.0 } else { 0.4 },
            if i & 2 == 0 { 0.0 } else { 0.4 },
            if i & 4 == 0 { 0.0 } else { 0.4 },
        ));
    }
    mesh.triangles = vec![
        [0, 2, 1],
        [1, 2, 3],
        [4, 5, 6],
        [5, 7, 6],
        [0, 1, 4],
        [1, 5, 4],
        [2, 6, 3],
        [3, 6, 7],
        [0, 4, 2],
        [2, 4, 6],
        [1, 3, 5],
        [3, 7, 5],
    ];
    mesh
}

/// w11 forms-undo-autosave item 3: Content ▸ New ▸ Cloth and Soft body write `.cloth.ron` / `.soft.ron`
/// files at the current versions that parse with the runtime source readers, cook into the runtime
/// assets with a sim mesh (a sheet; a closed cube), and open clean in the cloth panel.
#[test]
fn new_cloth_and_soft_body_assets_load_in_the_runtime_and_open() {
    use crate::content::{NewAssetKind, create_asset};
    use regia::physics::cloth::asset::{CLOTH_FORMAT_VERSION, SOFT_FORMAT_VERSION, SoftAsset, SoftSource};
    let root = regia_test_utils::unique_temp_dir("cloth-new-assets");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    let mut world = World::new();
    crate::mesh_source::register_level_types(&mut world);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    world.insert_resource(UndoStack::new());
    for (name, label, path) in [
        ("cloth", "Cloth", "assets/cloth/cape.cloth.ron"),
        ("soft_body", "Soft body", "assets/cloth/blob.soft.ron"),
    ] {
        let kind = NewAssetKind::parse(name).expect("kind");
        assert!(NewAssetKind::ALL.contains(&kind), "{name} is in the New menu");
        assert_eq!(kind.as_str(), name);
        assert_eq!(kind.label(), label);
        assert_eq!(create_asset(&root, kind, path).expect("created"), path);
        assert!(
            create_asset(&root, kind, path).is_err(),
            "{name}: a second create refuses"
        );
        assert!(
            create_asset(&root, kind, "assets/x.ron").is_err(),
            "{name}: the suffix"
        );
        let text = std::fs::read_to_string(root.join(path)).expect("file");
        if kind == NewAssetKind::Cloth {
            let source = ClothSource::from_ron_str(&text).expect("the runtime reader parses it");
            assert_eq!(source.format_version, CLOTH_FORMAT_VERSION);
            assert_eq!(source.name, "cape");
            assert_eq!(
                (source.sim_mesh.source.as_str(), source.sim_mesh.node.as_str()),
                ("cape.glb", "cape")
            );
            ClothAsset::cook(&source, &sheet(), None, &[]).expect("cooks with a sim mesh");
        } else {
            let source = SoftSource::from_ron_str(&text).expect("the runtime reader parses it");
            assert_eq!(source.format_version, SOFT_FORMAT_VERSION);
            assert_eq!(source.name, "blob");
            SoftAsset::cook(&source, &cube(), None).expect("cooks with a closed surface");
        }
        open_file(&mut world, path).expect("opens in the cloth panel");
        let doc = doc(&world);
        assert!(!doc.dirty(), "{name}: a new file opens clean");
        assert_eq!(doc.path, path);
    }
    let _ = std::fs::remove_dir_all(&root);
}
