// SPDX-License-Identifier: MPL-2.0
//! Post-v0.5 procedural editor tools (spec 67 § 3, § 5.2, § 5.3): the Area tool on a biome preset,
//! the Box tool's `Shape`, Paint's random yaw, the preview bar's default target and seed, and the
//! hit filter kept in the editor preferences file. Headless: fixed calls, editor ticks, no GPU, no
//! wall clock, no golden images.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_editor::command::{UndoStack, undo_world};
use regia_editor::editor_prefs::{self, EditorConfigPaths, EditorPreferences, PrefValue};
use regia_editor::procgen::debug_view::{ProcEditorPrefs, ProcPreview};
use regia_editor::procgen::panel::{self, PanelAction};
use regia_editor::procgen::tools::{self, Brush, HitFilter, ToolGesture, ToolKind};
use regia_editor::procgen::{ProcGraphDoc, doc};
use regia_procgen::biome::ecs::{AreaShape, BiomeArea};
use regia_procgen::painted::PaintedPoints;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph};
use regia_scene::components::{EntityGuid, Name};
use regia_scene::prelude::Transform;
use regia_scene::shape::{Shape, ShapeKind};
use regia_scene::spline::Spline;

const BIOME_PRESET: &str = "tool_biome_area.pgraph.ron";
const BOX_PRESET: &str = "tool_rubble_box.pgraph.ron";
const PAINT_PRESET: &str = "tool_painted_props.pgraph.ron";

fn node(ty: &str, at: f64, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    n.at = (at, 0.0);
    for (k, v) in settings {
        n = n.with_ron(k, v);
    }
    n
}

/// A jittered grid over the generator bounds (the output moves with the seed).
fn grid_graph() -> ProcGraph {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("sample.region", 200.0, &[("spacing_m", "2.0")]));
    g.nodes.insert(
        3,
        node(
            "points.jitter",
            400.0,
            &[("offset", "[(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]")],
        ),
    );
    g.nodes.insert(4, node("out.points", 600.0, &[]));
    g.edges = vec![
        Edge::new(1, "out", 2, "region"),
        Edge::new(2, "out", 3, "in"),
        Edge::new(3, "out", 4, "in"),
    ];
    g
}

/// An Area preset that is a biome graph (it reads `BiomeArea`s through `biome.areas`); its
/// `biome` parameter names the biome the area paints.
const BIOME_PRESET_RON: &str = r#"ProcGraph(
    format_version: 1,
    description: "biome area preset (test)",
    params: [
        (name: "biome", ty: Asset, default: "biomes/forest.biome.ron", min: None, max: None, doc: "The biome the area paints"),
    ],
    nodes: {
        1: (ty: "in.self", settings: {}, at: (0.0, 0.0)),
        2: (ty: "biome.areas", settings: {}, at: (220.0, 0.0)),
    },
    edges: [
        (from: (1, "out"), to: (2, "bounds")),
    ],
    flags: (starter: false, tool: Some("area")),
)"#;

fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("g.pgraph.ron".to_string(), grid_graph().to_ron()),
        (BIOME_PRESET.to_string(), BIOME_PRESET_RON.to_string()),
        (BOX_PRESET.to_string(), {
            let mut g = grid_graph();
            g.flags.tool = Some("box".into());
            g.to_ron()
        }),
        (PAINT_PRESET.to_string(), {
            let mut g = grid_graph();
            g.flags.tool = Some("paint".into());
            g.to_ron()
        }),
    ])
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>();
    regia_editor::procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    app
}

/// One editor frame: the procedural editor tick, then the app update.
fn frame(app: &mut App) {
    regia_editor::procgen::tick(app.world_mut());
    app.update();
}

fn ground(app: &mut App) -> Entity {
    let (vertices, indices) = regia_render::box_mesh([20.0, 0.5, 20.0]);
    let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
    let material = Arc::new(regia_render::LabMaterial::default());
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            regia_render::MeshRenderer::new(mesh, material),
            Name::new("ground"),
            EntityGuid(Guid(77)),
        ))
        .id()
}

fn biome_areas(app: &mut App) -> Vec<(Entity, BiomeArea)> {
    app.world_mut()
        .query::<(Entity, &BiomeArea)>()
        .iter(app.world())
        .map(|(e, a)| (e, a.clone()))
        .collect()
}

/// Spec 67 § 5.2: Area on a biome preset writes a closed spline and a `BiomeArea` (biome from the
/// preset, `FromSpline`) instead of a generator, as one undo step.
#[test]
fn an_area_gesture_on_a_biome_preset_creates_one_biome_area_and_one_undo_step() {
    let mut app = app();
    ground(&mut app);
    app.update();
    let depth = app.world().resource::<UndoStack>().undo_depth();
    let out = tools::run_tool(
        app.world_mut(),
        ToolKind::Area,
        BIOME_PRESET,
        None,
        &ToolGesture {
            points: vec![
                Vec3::new(-6.0, 5.0, -6.0),
                Vec3::new(6.0, 5.0, -6.0),
                Vec3::new(0.0, 5.0, 6.0),
            ],
            ..ToolGesture::default()
        },
        true,
    )
    .expect("area");
    assert!(out.created && out.applied);
    let areas = biome_areas(&mut app);
    assert_eq!(areas.len(), 1, "one BiomeArea: {areas:?}");
    let (e, area) = &areas[0];
    assert_eq!(*e, out.entity);
    assert_eq!(area.biome.as_deref(), Some("biomes/forest.biome.ron"));
    assert_eq!(area.shape, AreaShape::FromSpline);
    let spline = app.world().get::<Spline>(out.entity).expect("spline");
    assert!(spline.closed && spline.points.len() == 3, "{spline:?}");
    assert!(
        app.world().get::<ProcGenerator>(out.entity).is_none(),
        "a biome area feeds the biome generators; it carries no generator of its own"
    );
    assert_eq!(
        app.world().resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    assert!(undo_world(app.world_mut()));
    assert!(biome_areas(&mut app).is_empty(), "undo removes the area");
}

/// Spec 67 § 5.2 and ADR-1675 (10): Box writes a box `Shape` (half extents, entity transform) and
/// binds the generator with `ProcBounds::FromShape`, not the generator's own box bounds.
#[test]
fn the_box_tool_writes_a_box_shape() {
    let mut app = app();
    let out = tools::run_tool(
        app.world_mut(),
        ToolKind::Box,
        BOX_PRESET,
        None,
        &ToolGesture {
            box_extents: Some((Vec3::new(-2.0, 0.0, 1.0), Vec3::new(4.0, 3.0, 5.0))),
            ..ToolGesture::default()
        },
        true,
    )
    .expect("box");
    let shape = app.world().get::<Shape>(out.entity).expect("a box Shape");
    match shape.kind {
        ShapeKind::Box { half_extents } => {
            assert!((half_extents - Vec3::new(3.0, 1.5, 2.0)).abs().max_element() < 1e-5);
        }
        ref other => panic!("not a box: {other:?}"),
    }
    let t = app.world().get::<Transform>(out.entity).expect("transform");
    assert!((t.translation - Vec3::new(1.0, 1.5, 3.0)).abs().max_element() < 1e-5);
    assert_eq!(
        app.world().get::<ProcGenerator>(out.entity).map(|g| g.bounds),
        Some(ProcBounds::FromShape)
    );
    let bounds = regia_procgen::world::generator_bounds(app.world(), out.entity);
    let (lo, hi) = bounds.world_aabb();
    for (got, want) in lo.iter().chain(hi.iter()).zip([-2.0, 0.0, 1.0, 4.0, 3.0, 5.0]) {
        assert!((got - want).abs() < 1e-4, "box {lo:?} {hi:?}");
    }
    // A second Box session on the selected box edits the same Shape.
    app.world_mut()
        .resource_mut::<regia_editor::selection::Selection>()
        .select(out.entity, regia_editor::selection::SelectMode::Replace);
    let again = tools::run_tool(
        app.world_mut(),
        ToolKind::Box,
        BOX_PRESET,
        None,
        &ToolGesture {
            box_extents: Some((Vec3::ZERO, Vec3::splat(2.0))),
            ..ToolGesture::default()
        },
        true,
    )
    .expect("box again");
    assert_eq!((again.entity, again.created), (out.entity, false));
    assert_eq!(
        app.world().get::<Shape>(out.entity).map(|s| s.kind.clone()),
        Some(ShapeKind::Box {
            half_extents: Vec3::ONE
        })
    );
}

/// The yaw of each painted point, from the point set `in.self` reads (rotation about +Y on flat
/// ground, where every normal is +Y).
fn yaws(app: &App, e: Entity) -> Vec<f32> {
    let set = app
        .world()
        .get::<PaintedPoints>(e)
        .expect("painted")
        .to_point_set();
    set.rotation
        .iter()
        .map(|r| {
            let q = Quat::from_array(*r);
            let fwd = q * Vec3::Z;
            fwd.x.atan2(fwd.z)
        })
        .collect()
}

fn paint_once(app: &mut App, at: Vec3) -> Entity {
    tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        PAINT_PRESET,
        None,
        &ToolGesture {
            points: vec![at],
            brush: Some(Brush {
                radius_m: 2.0,
                spacing_m: 1.0,
                ..Brush::default()
            }),
            ..ToolGesture::default()
        },
        true,
    )
    .expect("paint")
    .entity
}

/// Spec 67 § 5.2: Paint stamps a random yaw per point, drawn from the brush seed, so two strokes
/// with the same seed give the same yaws.
#[test]
fn two_paint_strokes_with_the_same_seed_give_the_same_yaws() {
    let mut one = app();
    ground(&mut one);
    one.update();
    let mut two = app();
    ground(&mut two);
    two.update();
    let a = paint_once(&mut one, Vec3::new(1.0, 5.0, 2.0));
    let b = paint_once(&mut two, Vec3::new(1.0, 5.0, 2.0));
    let (ya, yb) = (yaws(&one, a), yaws(&two, b));
    assert!(ya.len() > 4, "{ya:?}");
    assert_eq!(ya, yb, "same seed, same stroke: same yaws");
    let spread =
        ya.iter().fold(f32::INFINITY, |m, y| m.min(*y))..ya.iter().fold(f32::NEG_INFINITY, |m, y| m.max(*y));
    assert!(
        spread.end - spread.start > 0.5,
        "the yaws are random, not all the same: {ya:?}"
    );
}

fn generator(app: &mut App, guid: u128, name: &str) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 2.0, 8.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(guid)),
            Name::new(name),
            Transform::IDENTITY,
        ))
        .id()
}

fn select(app: &mut App, e: Option<Entity>) {
    let mut sel = app
        .world_mut()
        .resource_mut::<regia_editor::selection::Selection>();
    sel.clear();
    if let Some(e) = e {
        sel.select(e, regia_editor::selection::SelectMode::Replace);
    }
}

fn target(app: &App) -> Option<Entity> {
    app.world().resource::<ProcPreview>().target
}

/// Spec 67 § 3: the panel previews on the selected entity's generator, else the last one used.
#[test]
fn the_default_preview_target_is_the_selections_generator_else_the_last_used() {
    let mut app = app();
    let ground = ground(&mut app);
    let a = generator(&mut app, 1, "a");
    let b = generator(&mut app, 2, "b");
    app.update();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    frame(&mut app);
    assert_eq!(target(&app), None, "nothing selected, nothing used yet");
    select(&mut app, Some(a));
    frame(&mut app);
    assert_eq!(target(&app), Some(a), "the selection's generator");
    select(&mut app, Some(b));
    frame(&mut app);
    assert_eq!(target(&app), Some(b));
    select(&mut app, Some(ground));
    frame(&mut app);
    assert_eq!(
        target(&app),
        Some(b),
        "a selection without a generator keeps the last used"
    );
    select(&mut app, None);
    frame(&mut app);
    assert_eq!(target(&app), Some(b), "an empty selection keeps the last used");
    assert_eq!(app.world().resource::<ProcPreview>().last_used, Some(b));
}

/// Spec 67 § 3: the preview bar's seed field pins a seed on the previewed generator and the
/// generator runs again with it.
#[test]
fn the_preview_bars_seed_change_re_runs_the_previewed_generator() {
    let mut app = app();
    let a = generator(&mut app, 3, "a");
    app.update();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    regia_editor::procgen::debug_view::set_preview(app.world_mut(), Some(a)).expect("preview");
    for _ in 0..16 {
        frame(&mut app);
    }
    let state = |app: &App| {
        let s = app.world().get::<ProcGenState>(a).expect("state");
        (s.output_hash, s.run_id)
    };
    let (h0, r0) = state(&app);
    assert!(h0.is_some(), "the preview ran");
    panel::apply_panel_action(
        app.world_mut(),
        PanelAction::Seed {
            entity: a,
            action: doc::SeedAction::Set(424_242),
        },
    )
    .expect("seed");
    assert_eq!(
        app.world().get::<ProcGenerator>(a).and_then(|g| g.seed),
        Some(424_242)
    );
    for _ in 0..16 {
        frame(&mut app);
    }
    let (h1, r1) = state(&app);
    assert!(r1 > r0, "the generator ran again ({r0} -> {r1})");
    assert_ne!(h0, h1, "the new seed moves the jittered points");
    assert!(app.world().resource::<ProcGraphDoc>().path.is_some());
}

fn paths(tag: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Spec 67 § 5.3: the per-tool hit filter is saved in the editor preferences file and read back by
/// the next editor session.
#[test]
fn the_hit_filter_survives_an_editor_restart() {
    let dir = paths("procgen_hit_prefs");
    let wanted = HitFilter {
        terrain: false,
        meshes: true,
        skip_generated: false,
        only_entity: None,
        physics_mask: 0b101,
    };
    {
        let mut first = app();
        first.world_mut().insert_resource(EditorConfigPaths::at(&dir));
        editor_prefs::load_prefs(first.world_mut());
        let mut prefs = first.world_mut().resource_mut::<EditorPreferences>();
        prefs
            .set("procgen.hit.paint.terrain", &PrefValue::Bool(false))
            .expect("terrain pref");
        prefs
            .set("procgen.hit.paint.skip_generated", &PrefValue::Bool(false))
            .expect("skip pref");
        prefs
            .set("procgen.hit.paint.physics_mask", &PrefValue::Int(0b101))
            .expect("mask pref");
        frame(&mut first);
        assert_eq!(
            first
                .world()
                .resource::<ProcEditorPrefs>()
                .hit_filters
                .get("paint"),
            Some(&wanted),
            "the preferences reach the tool"
        );
        assert!(editor_prefs::persist_prefs(first.world_mut()), "written");
    }
    // A new editor session on the same preferences directory.
    let mut second = app();
    second.world_mut().insert_resource(EditorConfigPaths::at(&dir));
    editor_prefs::load_prefs(second.world_mut());
    frame(&mut second);
    let prefs = second.world().resource::<ProcEditorPrefs>();
    assert_eq!(prefs.hit_filters.get("paint"), Some(&wanted));
    assert_eq!(
        prefs.hit_filters.get("box").cloned().unwrap_or_default(),
        HitFilter::default(),
        "other tools keep the defaults"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
