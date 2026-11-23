// SPDX-License-Identifier: MPL-2.0
//! ADR-7586: `render.viscells` and `render.set_viscells` end to end, through the agent protocol's
//! real dispatch path (permission check, rate limit, handler, reply). Installing a two-room
//! viscells.ron with the camera facing away from the doorway culls room B's mesh (counted, not in
//! `render.visible_entities`); `cull: false` brings it back; a bad text is refused with its code
//! and changes nothing; `debug_draw` flips the `viscells` gizmo group. No window, no GPU, no
//! socket, no wall clock: a channel stands in for the WebSocket inbox.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_render::gizmos::GizmoConfigStore;
use regia_render::viscells::{VisCell, VisCellCulled, VisCellGizmos, VisCellStats, cull_unreachable_cells};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, ExtractedFrame, GlassView, LabVertex, MeshRenderer, PointLight,
    ViewVisibilityStats, extract_frame, extract_glass, update_view_visibility,
};
use regia_scene::Transform;
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

const ROOMS: &str = "(version: 1, cells: [
    (id: 1, name: \"room_a\", bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
    (id: 2, name: \"room_b\", bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
], portals: [(a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)])])";

struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    project: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let project = unique_temp_dir("agent-viscells");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin))
            .insert_resource(AgentSettings {
                project_dir: Some(project.clone()),
            })
            .add_plugins(AgentPlugin)
            .init_resource::<ExtractedFrame>()
            .init_resource::<ViewVisibilityStats>()
            .init_resource::<VisCellStats>()
            .init_resource::<VisCellCulled>()
            .add_systems(
                Render,
                (
                    extract_frame,
                    cull_unreachable_cells,
                    regia_terrain::extract_runtime_scatter,
                    extract_glass,
                    update_view_visibility,
                )
                    .chain(),
            )
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.world_mut()
            .get_resource_or_init::<GizmoConfigStore>()
            .register::<VisCellGizmos>();
        app.finish_plugins();
        let (inbox, inbox_rx) = unbounded();
        app.insert_resource(AgentInbox(inbox_rx));
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: u32::MAX,
                ..AgentConfig::default()
            },
            source: None,
        });
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            replies,
            reply,
            project,
        }
    }

    /// Sends one request, runs two frames, and returns the one reply's result or error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        self.inbox
            .send(Inbound {
                request,
                reply: self.reply.clone(),
            })
            .expect("inbox open");
        for _ in 0..2 {
            self.app.update();
        }
        let response = self.replies.try_recv().expect("one reply per request");
        match (response.result, response.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(serde_json::to_value(error).expect("error serializes")),
            other => panic!("malformed response {other:?}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.5, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.5, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn listed(reply: &Value) -> Vec<u64> {
    reply["entities"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

fn code(error: &Value) -> &str {
    error["data"]["code"].as_str().unwrap_or_default()
}

#[test]
fn viscells_install_cull_read_and_switch_through_the_dispatcher() {
    let mut h = Harness::new();
    let world = h.app.world_mut();
    // In room A at (0, 1, 0), facing +Z: away from the doorway at z −4.
    world.spawn((
        Transform {
            translation: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::PI),
            ..Transform::default()
        },
        Camera::default(),
    ));
    let mesh = |world: &mut World, z: f32, cell: u16| {
        world
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, z)),
                MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
                VisCell(cell),
            ))
            .id()
    };
    let in_a = mesh(world, 3.0, 1);
    // Labelled room B but placed in plain view ahead of the camera: the frustum alone would draw
    // it, so only the cell (unreached) can cull it.
    let in_b = mesh(world, 3.5, 2);
    h.app.update();

    let before = h.call("render.viscells", json!({})).expect("reply");
    assert_eq!(before["loaded"], false, "{before}");

    let installed = h
        .call("render.set_viscells", json!({ "text": ROOMS }))
        .expect("installs");
    assert_eq!(installed["loaded"], true, "{installed}");
    assert_eq!(installed["cell_count"], 3);
    assert_eq!(installed["portal_count"], 1);
    h.app.update();

    let read = h.call("render.viscells", json!({})).expect("reply");
    assert_eq!(read["views"][0]["eye_cell"], 1, "{read}");
    assert_eq!(read["views"][0]["eye_cells"], json!([1]), "{read}");
    assert_eq!(read["views"][0]["depth_capped"], false, "{read}");
    assert!(
        read["portals"][0]["doorway_depth"]
            .as_f64()
            .is_some_and(|d| d > 0.0),
        "{read}"
    );
    assert_eq!(read["reachable"], json!([1]), "{read}");
    assert_eq!(read["frame"]["culled_draws"], 1, "{read}");
    assert_eq!(read["frame"]["shadow_only_draws"], 1);
    assert_eq!(read["portals"][0]["a"], 1);
    assert_eq!(read["cells"][2]["name"], "room_b");
    let visible = h.call("render.visible_entities", json!({})).expect("reply");
    let ids = listed(&visible);
    assert!(ids.contains(&in_a.to_bits()), "{visible}");
    assert!(!ids.contains(&in_b.to_bits()), "room B is culled: {visible}");

    let off = h
        .call("render.set_viscells", json!({ "cull": false }))
        .expect("switches");
    assert_eq!(off["cull"], false);
    h.app.update();
    let read = h
        .call("render.viscells", json!({ "geometry": false }))
        .expect("reply");
    assert_eq!(read["frame"]["active"], false, "{read}");
    assert_eq!(read["cells"], json!([]));
    let visible = h.call("render.visible_entities", json!({})).expect("reply");
    assert!(listed(&visible).contains(&in_b.to_bits()), "{visible}");

    // A bow-tie portal: refused with its code, the installed cells stay.
    let bad = ROOMS.replace(
        "(1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)",
        "(-1.0, 2.0, -4.0), (1.0, 2.0, -4.0)",
    );
    let err = h
        .call("render.set_viscells", json!({ "text": bad }))
        .expect_err("refused");
    assert_eq!(code(&err), "viscells_portal_not_convex", "{err}");
    let read = h
        .call("render.viscells", json!({ "geometry": false }))
        .expect("reply");
    assert_eq!(read["cell_count"], 3);

    let draw = h
        .call("render.set_viscells", json!({ "debug_draw": true }))
        .expect("draws");
    assert_eq!(draw["debug_draw"], true);
    assert!(
        h.app
            .world()
            .resource::<GizmoConfigStore>()
            .is_enabled::<VisCellGizmos>()
    );

    let cleared = h
        .call("render.set_viscells", json!({ "clear": true }))
        .expect("clears");
    assert_eq!(cleared["loaded"], false);
}

/// Stage 2 through the dispatcher: the parts of a cell-tagged root, its lamp and its glass pane
/// are counted (`cell_draws`, `culled_lights`, `culled_glass`) while no view reaches the room.
#[test]
fn viscells_counts_inherited_parts_lights_and_glass_through_the_dispatcher() {
    let mut h = Harness::new();
    let world = h.app.world_mut();
    world.spawn((
        Transform {
            translation: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::PI),
            ..Transform::default()
        },
        Camera::default(),
    ));
    let root_b = world
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, -8.0)),
            VisCell(2),
        ))
        .id();
    let part = world
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, 0.0)),
            MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
            ChildOf(root_b),
        ))
        .id();
    world.spawn((
        Transform::from_translation(regia_core::math::Vec3::new(1.0, 1.0, 0.0)),
        MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
        GlassView::viewer_inside(),
        ChildOf(root_b),
    ));
    world.spawn((
        Transform::from_translation(regia_core::math::Vec3::new(0.0, 2.0, -1.0)),
        PointLight {
            radius: 2.0,
            ..PointLight::default()
        },
        ChildOf(root_b),
    ));
    h.call("render.set_viscells", json!({ "text": ROOMS }))
        .expect("installs");
    h.app.update();
    let read = h
        .call("render.viscells", json!({ "geometry": false }))
        .expect("reply");
    let frame = &read["frame"];
    assert_eq!(read["reachable"], json!([1]), "{read}");
    assert_eq!(
        frame["cell_draws"], 2,
        "the part and the pane inherit cell 2: {read}"
    );
    assert_eq!(frame["culled_draws"], 2, "{read}");
    assert_eq!(frame["cell_lights"], 1, "{read}");
    assert_eq!(frame["culled_lights"], 1, "{read}");
    assert_eq!(frame["culled_glass"], 1, "{read}");
    let visible = h.call("render.visible_entities", json!({})).expect("reply");
    assert!(!listed(&visible).contains(&part.to_bits()), "{visible}");
}

/// Room A (cell 1) with a 2 m by 1.5 m window to the outdoors at z +4 and the door to room B at z −4.
const ROOMS_WITH_A_WINDOW: &str = "(version: 1, cells: [
    (id: 1, name: \"room_a\", bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
    (id: 2, name: \"room_b\", bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
], portals: [
    (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
    (a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)]),
])";

/// A rock about every 4 m on a flat map at height 0, out to 200 m.
fn rock_field() -> regia_terrain::RuntimeScatter {
    use regia_terrain::scatter::field::{ScatterField, ScatterLibrary, ScatterSource};
    use regia_terrain::scatter::set::{ScatterClass, ScatterMeshRef, ScatterSet};
    let n = 65u32;
    let heights = regia_asset::Heightfield {
        width: n,
        height: n,
        samples: vec![0; (n * n) as usize],
    };
    let map = Arc::new(regia_terrain::TerrainMap::new(&heights, 1024.0, 64.0, 0.0).expect("map"));
    let set = ScatterSet {
        classes: vec![ScatterClass {
            name: "boulders".into(),
            meshes: vec![ScatterMeshRef {
                mesh: "rock_a".into(),
                weight: 1.0,
                min_scale: 0.5,
                max_scale: 1.5,
                scale_bias: 1.0,
            }],
            cell_size: 4.0,
            density: 1.0,
            fade_start: 150.0,
            fade_end: 200.0,
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock_a".into(),
        regia_terrain::procedural_rock(3, regia_terrain::RockStyle::angular([0.5, 0.4, 0.35])),
    );
    let mut field = ScatterField::new(&set, ScatterSource::new(map), &library, 1).expect("field");
    field.set_background_streaming(false);
    regia_terrain::RuntimeScatter::new(field)
}

/// Plan row W-22 (ADR-8413) through the dispatcher: `render.stats` `interior` reports the outdoor
/// frusta of a view inside room A looking out of its window, and the scatter instances they left out;
/// with the cull off nothing is restricted and the scatter emits more.
#[test]
fn render_stats_reports_the_interior_cull_through_the_dispatcher() {
    let mut h = Harness::new();
    let world = h.app.world_mut();
    world.insert_resource(regia_render::RuntimeRenderer(regia_render::FrameRenderer::new()));
    world.insert_resource(rock_field());
    // In room A at (0, 1, 0), facing +Z: out of the window at z +4.
    world.spawn((
        Transform {
            translation: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::PI),
            ..Transform::default()
        },
        Camera::default(),
    ));
    h.app.update();
    let outdoors = h.call("render.stats", json!({})).expect("reply");
    assert_eq!(outdoors["interior"]["restricted"], false, "{outdoors}");
    assert!(
        outdoors["interior"]["terrain"].is_null(),
        "no terrain draw: {outdoors}"
    );
    assert_eq!(outdoors["interior"]["scatter"]["portal_culled"], 0, "{outdoors}");

    h.call("render.set_viscells", json!({ "text": ROOMS_WITH_A_WINDOW }))
        .expect("installs");
    h.app.update();
    let inside = h.call("render.stats", json!({})).expect("reply");
    let interior = &inside["interior"];
    assert_eq!(interior["restricted"], true, "{inside}");
    assert_eq!(interior["frusta"], 1, "the window: {inside}");
    assert_eq!(interior["mouths"], 1, "{inside}");
    let culled = interior["scatter"]["portal_culled"].as_u64().expect("count");
    let emitted_inside = interior["scatter"]["emitted"].as_u64().expect("count");
    assert!(
        culled > 0,
        "rocks outside the window's frustum are left out: {inside}"
    );

    h.call("render.set_viscells", json!({ "cull": false }))
        .expect("switches");
    h.app.update();
    let off = h.call("render.stats", json!({})).expect("reply");
    assert_eq!(off["interior"]["restricted"], false, "{off}");
    assert_eq!(off["interior"]["frusta"], 0);
    assert_eq!(off["interior"]["scatter"]["portal_culled"], 0, "{off}");
    let emitted_off = off["interior"]["scatter"]["emitted"].as_u64().expect("count");
    assert!(emitted_off > emitted_inside, "{emitted_off} vs {emitted_inside}");
}
