// SPDX-License-Identifier: MPL-2.0
//! Post-v0.5 procedural editor surfaces through the real agent dispatcher: the viewport tool mode
//! "Procedural" (`editor.procgen_tool { mode, gesture }`: Box footprint and height drags, Paint
//! strokes stamped in the background), the preview target rule (`editor.procgen_preview`), the hit
//! filter through `editor.prefs_set`, and the plant preview with its six-seed variants strip
//! (`editor.plant_preview`, spec 68 § 8). Headless: fixed calls and editor ticks, no GPU, no
//! wall clock, no golden images.

use std::collections::BTreeMap;
use std::sync::Arc;

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::UndoStack;
use regia_editor::procgen::debug_view::{ProcEditorPrefs, ProcPreview};
use regia_editor::procgen::plant_preview::{self, PlantPreview, VARIANT_COUNT};
use regia_editor::procgen::tools::{self, HitFilter, PaintStrokeQueue};
use regia_procgen::painted::PaintedPoints;
use regia_procgen::world::{ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource};
use regia_procgen::{Edge, NodeEntry, ProcGraph};
use regia_scene::components::{EntityGuid, Name};
use regia_scene::prelude::Transform;
use regia_scene::shape::{Shape, ShapeKind};
use serde_json::{Value, json};

fn node(ty: &str, at: f64, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    n.at = (at, 0.0);
    for (k, v) in settings {
        n = n.with_ron(k, v);
    }
    n
}

fn grid_graph(tool: Option<&str>) -> String {
    let mut g = ProcGraph::default();
    g.flags.tool = tool.map(str::to_string);
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("sample.region", 200.0, &[("spacing_m", "2.0")]));
    g.nodes.insert(3, node("out.points", 400.0, &[]));
    g.edges = vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")];
    g.to_ron()
}

fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("g.pgraph.ron".to_string(), grid_graph(None)),
        ("box.pgraph.ron".to_string(), grid_graph(Some("box"))),
        ("paint.pgraph.ron".to_string(), grid_graph(Some("paint"))),
        (
            BUSH.to_string(),
            regia_procgen::plant::starter(BUSH)
                .expect("bush starter")
                .to_string(),
        ),
    ])
}

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
    dir: std::path::PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Agent {
    fn new() -> Self {
        let dir = regia_test_utils::unique_temp_dir("procgen-post05");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<regia_editor::selection::Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let open = regia_editor::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.heavy_rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
            dir,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        regia_editor::procgen::tick(self.app.world_mut());
        self.app.update();
        let response = answer.try_recv().expect("answered within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(v) => v,
            Err(e) => panic!("{method} {params}: {e:?}"),
        }
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            regia_editor::procgen::tick(self.app.world_mut());
            self.app.update();
        }
    }

    fn ground(&mut self) {
        let (vertices, indices) = regia_render::box_mesh([20.0, 0.5, 20.0]);
        let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
        let material = Arc::new(regia_render::LabMaterial::default());
        self.app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            regia_render::MeshRenderer::new(mesh, material),
            Name::new("ground"),
            EntityGuid(Guid(77)),
        ));
        self.app.update();
    }
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn entity(v: &Value) -> Entity {
    Entity::try_from_bits(v.as_u64().expect("entity bits")).expect("entity")
}

/// Spec 67 § 5.2 Box through the viewport mode: drag a footprint on the ground, then drag the
/// height; Enter applies one undo entry with a box `Shape`.
#[test]
fn box_gestures_through_the_agent_write_a_box_shape_in_one_undo_entry() {
    let mut a = Agent::new();
    a.ground();
    let armed = a.call(
        "editor.procgen_tool",
        json!({ "mode": { "tool": "box", "preset": "box.pgraph.ron" } }),
    );
    assert_eq!(armed["armed"]["tool"], "box");
    a.call(
        "editor.procgen_tool",
        json!({ "gesture": { "kind": "press", "at": [-2.0, 5.0, -1.0] } }),
    );
    let footprint = a.call(
        "editor.procgen_tool",
        json!({ "gesture": { "kind": "release", "at": [4.0, 5.0, 3.0] } }),
    );
    let e = entity(&footprint["outcome"]["entity"]);
    assert!(!footprint["session"].is_null(), "the footprint opens a session");
    // The height drag: a horizontal ray at y = 2.5 passing the footprint's center.
    a.call(
        "editor.procgen_tool",
        json!({ "gesture": { "kind": "press", "origin": [1.0, 2.5, -30.0], "direction": [0.0, 0.0, 1.0] } }),
    );
    a.call(
        "editor.procgen_tool",
        json!({ "gesture": { "kind": "release", "origin": [1.0, 2.5, -30.0], "direction": [0.0, 0.0, 1.0] } }),
    );
    let depth = a.app.world().resource::<UndoStack>().undo_depth();
    let applied = a.call("editor.procgen_tool", json!({ "gesture": { "kind": "enter" } }));
    assert_eq!(applied["outcome"]["applied"], true);
    assert_eq!(a.app.world().resource::<UndoStack>().undo_depth(), depth + 1);
    let shape = a.app.world().get::<Shape>(e).expect("box shape").kind.clone();
    match shape {
        ShapeKind::Box { half_extents } => {
            assert!(
                (half_extents - Vec3::new(3.0, 1.25, 2.0)).abs().max_element() < 1e-3,
                "{half_extents:?}"
            );
        }
        other => panic!("not a box: {other:?}"),
    }
    assert_eq!(
        a.app.world().get::<ProcGenerator>(e).map(|g| g.bounds),
        Some(ProcBounds::FromShape)
    );
    let off = a.call("editor.procgen_tool", json!({ "mode": "off" }));
    assert!(off["armed"].is_null());
    let bad = a
        .try_call("editor.procgen_tool", json!({ "gesture": { "kind": "wiggle" } }))
        .expect_err("unknown gesture");
    assert_eq!(reason(&bad), "procgen_tool_bad_gesture");
}

/// Paints one drag stroke through the viewport mode with `seed`, lets the background queue stamp
/// it, applies, and returns the painted yaws and the ticks the queue needed.
fn paint_stroke(seed: u32) -> (Vec<f32>, u64) {
    let mut a = Agent::new();
    a.ground();
    a.call(
        "editor.procgen_tool",
        json!({ "mode": { "tool": "paint", "preset": "paint.pgraph.ron",
                          "brush": { "radius_m": 4.0, "spacing_m": 0.25, "seed": seed } } }),
    );
    let pressed = a.call(
        "editor.procgen_tool",
        json!({ "gesture": { "kind": "press", "at": [-6.0, 5.0, 0.0] } }),
    );
    // The press queues a stroke point; nothing is stamped inside the gesture call.
    assert_eq!(pressed["pending_strokes"], 1, "{pressed}");
    assert!(pressed["session"].is_null(), "{pressed}");
    for x in [-4.0, -2.0, 0.0, 2.0, 4.0, 6.0] {
        a.call(
            "editor.procgen_tool",
            json!({ "gesture": { "kind": "move", "at": [x, 5.0, 0.0] } }),
        );
    }
    a.call(
        "editor.procgen_tool",
        json!({ "gesture": { "kind": "release", "at": [6.0, 5.0, 0.0] } }),
    );
    let mut ticks = 0;
    while tools::pending_strokes(a.app.world()) > 0 {
        a.frames(1);
        ticks += 1;
        assert!(ticks < 64, "the stroke queue drains");
    }
    let busy = a.app.world().resource::<PaintStrokeQueue>().busy_ticks;
    let applied = a.call("editor.procgen_tool", json!({ "gesture": { "kind": "enter" } }));
    assert_eq!(applied["outcome"]["applied"], true);
    let e = entity(&applied["outcome"]["entity"]);
    let painted = a.app.world().get::<PaintedPoints>(e).expect("painted");
    assert!(painted.points.len() > 20, "{} points", painted.points.len());
    (painted.points.iter().map(|p| p.yaw).collect(), busy)
}

/// Spec 67 § 5.2: strokes run in the background (more than one tick), and two strokes with the
/// same seed give the same yaws; another seed gives other yaws.
#[test]
fn paint_strokes_stamp_in_the_background_with_seeded_yaws() {
    let (one, busy) = paint_stroke(7);
    let (two, _) = paint_stroke(7);
    let (other, _) = paint_stroke(8);
    assert!(busy > 1, "a large stroke is spread over ticks ({busy})");
    assert_eq!(one, two, "same seed, same yaws");
    assert_ne!(one, other, "another seed, other yaws");
    assert!(one.iter().all(|y| (0.0..std::f32::consts::TAU).contains(y)));
}

/// Spec 67 § 3 through the agent: an explicit generator stays bound; `follow: true` returns to the
/// selection's generator.
#[test]
fn the_preview_follows_the_selection_until_a_generator_is_chosen() {
    let mut a = Agent::new();
    let spawn = |a: &mut Agent, guid: u128| {
        a.app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(4.0, 1.0, 4.0),
                    },
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                EntityGuid(Guid(guid)),
                Transform::IDENTITY,
            ))
            .id()
    };
    let g1 = spawn(&mut a, 11);
    let g2 = spawn(&mut a, 12);
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let select = |a: &mut Agent, e: Entity| {
        let mut sel = a
            .app
            .world_mut()
            .resource_mut::<regia_editor::selection::Selection>();
        sel.clear();
        sel.select(e, regia_editor::selection::SelectMode::Replace);
    };
    select(&mut a, g1);
    a.frames(1);
    assert_eq!(a.app.world().resource::<ProcPreview>().target, Some(g1));
    let chosen = a.call("editor.procgen_preview", json!({ "generator": g2.to_bits() }));
    assert_eq!(chosen["follow"], false);
    select(&mut a, g1);
    a.frames(2);
    assert_eq!(
        a.app.world().resource::<ProcPreview>().target,
        Some(g2),
        "an explicit choice is kept"
    );
    let follow = a.call("editor.procgen_preview", json!({ "follow": true }));
    assert_eq!(follow["follow"], true);
    assert_eq!(follow["generator"], json!(g1.to_bits()));
}

/// Spec 67 § 5.3: `editor.prefs_set` on a `procgen.hit.*` key reaches the tool's hit filter.
#[test]
fn hit_filter_preferences_set_through_the_agent_reach_the_tool() {
    let mut a = Agent::new();
    a.call(
        "editor.prefs_set",
        json!({ "values": { "procgen.hit.box.meshes": false, "procgen.hit.box.physics_mask": 3 } }),
    );
    a.frames(1);
    let got = a
        .app
        .world()
        .resource::<ProcEditorPrefs>()
        .hit_filters
        .get("box")
        .cloned();
    assert_eq!(
        got,
        Some(HitFilter {
            meshes: false,
            physics_mask: 3,
            ..HitFilter::default()
        })
    );
    assert_eq!(
        tools::hit_filter(a.app.world(), tools::ToolKind::Box).physics_mask,
        3
    );
}

const BUSH: &str = "starter/plant_bush.pgraph.ron";

/// Spec 68 § 8: the variants strip shows six seeds; choosing one sets the plant's seed and the
/// viewport shows it. Not a Plant graph: `procgen_wrong_domain`.
#[test]
fn the_plant_preview_strip_shows_six_seeds_and_a_click_sets_the_seed() {
    let mut a = Agent::new();
    let r = a.call("editor.plant_preview", json!({ "asset": BUSH, "seeds": 10 }));
    assert_eq!(r["seeds"], json!([10, 11, 12, 13, 14, 15]));
    let variants = r["variants"].as_array().expect("variants");
    assert_eq!(variants.len(), VARIANT_COUNT);
    for v in variants {
        assert!(v["error"].is_null(), "{v}");
        assert!(v["nodes"].as_u64().unwrap_or(0) > 1, "{v}");
        assert!(v["segments"].as_u64().unwrap_or(0) > 0, "{v}");
    }
    assert_eq!(r["seed"], 10, "the plant's seed starts at the strip's first seed");
    let heights: Vec<f64> = variants.iter().filter_map(|v| v["height_m"].as_f64()).collect();
    assert!(
        heights.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-6),
        "the seeds grow different plants: {heights:?}"
    );
    let builds = r["builds"].as_u64().expect("builds");
    // A click on the third variant: the plant's seed is 12 and no graph runs again.
    plant_preview::set_seed(a.app.world_mut(), 12);
    let clicked = a.call("editor.plant_preview", json!({ "asset": BUSH }));
    assert_eq!(clicked["seed"], 12);
    assert_eq!(clicked["shown"]["seed"], 12);
    assert_eq!(clicked["builds"].as_u64(), Some(builds), "the strip is reused");
    assert_eq!(a.app.world().resource::<PlantPreview>().seed, 12);
    let via_agent = a.call("editor.plant_preview", json!({ "asset": BUSH, "seed": 14 }));
    assert_eq!(via_agent["shown"]["seed"], 14);
    let wrong = a
        .try_call("editor.plant_preview", json!({ "asset": "g.pgraph.ron" }))
        .expect_err("not a plant graph");
    assert_eq!(reason(&wrong), "procgen_wrong_domain");
    let missing = a
        .try_call("editor.plant_preview", json!({ "asset": "nope.pgraph.ron" }))
        .expect_err("missing");
    assert_eq!(reason(&missing), "procgen_unknown_graph");
}

/// Spec 68 § 8: an open Plant graph previews itself in the panel; the editor tick builds at most
/// one variant per frame, and the strip fills over six frames.
#[test]
fn an_open_plant_graph_fills_the_strip_one_variant_per_tick() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": BUSH }));
    let built = |a: &Agent| {
        a.app
            .world()
            .get_resource::<PlantPreview>()
            .map_or(0, |p| p.variants.len())
    };
    let mut seen = Vec::new();
    for _ in 0..VARIANT_COUNT {
        a.frames(1);
        seen.push(built(&a));
    }
    assert_eq!(
        a.app.world().resource::<PlantPreview>().asset.as_deref(),
        Some(BUSH)
    );
    assert!(
        seen.windows(2).all(|w| w[1] <= w[0] + 1),
        "one run per tick: {seen:?}"
    );
    assert_eq!(built(&a), VARIANT_COUNT);
}

/// post05w4 item 3a (ADR-7215 (8): "a GPU-lit plant thumbnail is not built"): on a render device
/// the strip's mesh variants get lit thumbnails. Each covers part of its picture with lit (not
/// black) plant pixels, and different seeds give different pictures. Lavapipe is enough.
#[test]
fn the_plant_strip_draws_lit_mesh_thumbnails_on_a_render_device() {
    let mut a = Agent::new();
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    a.app.world_mut().insert_resource(device);
    let r = a.call(
        "editor.plant_preview",
        json!({ "asset": BUSH, "seeds": 10, "lit": true }),
    );
    let variants = r["variants"].as_array().expect("variants");
    assert_eq!(variants.len(), VARIANT_COUNT);
    let mut covers = Vec::new();
    for v in variants {
        assert!(v["triangles"].as_u64().unwrap_or(0) > 0, "a mesh variant: {v}");
        let lit = &v["lit"];
        assert_eq!(lit["width"], 192, "a lit thumbnail per mesh variant: {v}");
        let coverage = lit["coverage"].as_f64().unwrap_or(0.0);
        assert!(
            coverage > 0.02 && coverage < 0.95,
            "the plant covers part of the picture: {v}"
        );
        let rgb: Vec<f64> = lit["mean_rgb"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_f64)
            .collect();
        let luma = 0.2126 * rgb.first().copied().unwrap_or(0.0)
            + 0.7152 * rgb.get(1).copied().unwrap_or(0.0)
            + 0.0722 * rgb.get(2).copied().unwrap_or(0.0);
        assert!(luma > 10.0, "the plant is lit, not black: {v}");
        covers.push(coverage);
    }
    assert!(
        covers.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-4),
        "different seeds draw different pictures: {covers:?}"
    );
    assert!(
        r["lit_renders"].as_u64().unwrap_or(0) >= VARIANT_COUNT as u64,
        "{r}"
    );
    // A second call reuses the pictures (nothing is drawn again while the graph is unchanged).
    let again = a.call("editor.plant_preview", json!({ "asset": BUSH, "lit": true }));
    assert_eq!(again["lit_renders"], r["lit_renders"], "{again}");
}
