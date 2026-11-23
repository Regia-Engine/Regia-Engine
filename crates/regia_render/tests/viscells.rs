// SPDX-License-Identifier: MPL-2.0
//! Indoor cells and portals, stage 1 (`docs/specs/25-sim-bridge-and-do27.md` § 7.7, ADR-7586),
//! on a synthetic two-room scene: room A (cell 1) around the origin, room B (cell 2) behind it at
//! z −8, a 2 m doorway portal at z −4 between them, and (in some tests) a window portal from A to
//! outdoors (cell 0) at z +4. No walls are modelled: only the cells say what is hidden.
//!
//! - Facing away from the door, B's mesh is counted culled, kept only as a shadow caster, and not
//!   view-visible.
//! - With a view that sees B's mesh but not the doorway, the lavapipe frame equals a control
//!   frame with B hidden (and differs from one without cells, where B shows).
//! - Looking through the doorway, B draws and the frame equals the control with no cells.
//! - A portal to cell 0 keeps the outside reachable; without it the outside is culled.
//! - Entities without `VisCell`, or with an unlisted cell, always draw; `cull: false` culls nothing.
//! - A dense, cyclic portal set ends at the visit cap and then draws everything.
//! - An eye in a doorway's wall gap (in no room box) draws both rooms (ADR-7639).
//!
//! The ECS tests are CPU only; the image tests need a headless device (lavapipe). No goldens.

use std::f32::consts::PI;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::viscells::{
    MAX_CELL_VISITS, VisCell, VisCellCulled, VisCellSettings, VisCellStats, VisCells, cull_unreachable_cells,
};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, ExtractedFrame, FrameRenderer, GlassView, LabLocalLight, LabVertex,
    LightSourceLtc, LodViewport, MeshRenderer, NotShadowCaster, PointLight, RenderDevice, SpotLight,
    ViewVisibilityStats, extract_frame, extract_glass, update_view_visibility,
};
use regia_scene::Transform;
use regia_scene::components::{ViewVisibility, Visibility};
use regia_scene::propagate::propagate_transforms;

const TWO_ROOMS: &str = "(
    version: 1,
    cells: [
        (id: 0, name: \"outdoor\", outdoor: true, bounds: []),
        (id: 1, name: \"room_a\", bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
        (id: 2, name: \"room_b\", bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
    ],
    portals: [
        (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
        WINDOW
    ],
)";

const WINDOW: &str =
    "(a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)]),";

fn cells(window: bool) -> VisCells {
    VisCells::from_ron(&TWO_ROOMS.replace("WINDOW", if window { WINDOW } else { "" }))
        .expect("the scene's cells load")
}

/// A unit cube (±0.5 m) around the origin.
fn cube() -> Arc<CpuMesh> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                0.5 * (n[0] + su * u[0] + sv * v[0]),
                0.5 * (n[1] + su * u[1] + sv * v[1]),
                0.5 * (n[2] + su * u[2] + sv * v[2]),
            ];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                n,
                [0.5 * (su + 1.0), 0.5 * (sv + 1.0)],
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

fn material(color: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: color,
        ..CpuMaterial::default()
    })
}

/// Where the camera stands and what it sees.
#[derive(Clone, Copy)]
struct View {
    eye: Vec3,
    /// Turn about +Y; 0 looks down −Z, π looks down +Z.
    yaw: f32,
    fov_y: f32,
}

/// In room A, facing away from the doorway (looking down +Z).
const FACING_AWAY: View = View {
    eye: Vec3::new(0.0, 1.0, 0.0),
    yaw: PI,
    fov_y: std::f32::consts::FRAC_PI_4,
};

/// In room A near its +X wall, looking down −Z with a narrow lens: room B's mesh is in view, the
/// doorway (x within ±1 m) is not.
const PAST_THE_DOOR: View = View {
    eye: Vec3::new(3.5, 1.0, 3.5),
    yaw: 0.0,
    fov_y: 0.3,
};

/// In room A, looking straight through the doorway into room B.
const THROUGH_THE_DOOR: View = View {
    eye: Vec3::new(0.0, 1.0, 3.0),
    yaw: 0.0,
    fov_y: std::f32::consts::FRAC_PI_4,
};

/// `THROUGH_THE_DOOR` with the narrow lens of `PAST_THE_DOOR`, for the image test: at π/4 room
/// B's cube, 11 m away, covers about 5 × 5 pixels of the 64 × 48 frame, too few to show it draws.
/// At 0.3 rad the doorway still fills the view's centre and B covers about 14 × 14 pixels.
const THROUGH_THE_DOOR_NARROW: View = View {
    fov_y: 0.3,
    ..THROUGH_THE_DOOR
};

/// An app with propagation in `PostUpdate` and extraction, the cell cull and the
/// `ViewVisibility` write in `Render`, one window camera at `view`, and a 64×48 viewport.
fn app(view: View) -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<VisCellStats>();
    app.init_resource::<VisCellCulled>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(
        Render,
        (
            extract_frame,
            cull_unreachable_cells,
            extract_glass,
            update_view_visibility,
        )
            .chain(),
    );
    let transform = Transform {
        translation: view.eye,
        rotation: Quat::from_rotation_y(view.yaw),
        ..Transform::default()
    };
    app.world_mut().spawn((
        transform,
        Camera {
            fov_y: view.fov_y,
            exposure_ev: Some(10.0),
            ..Camera::default()
        },
    ));
    app
}

fn mesh_at(app: &mut App, at: (f32, f32, f32), color: [f32; 4], cell: Option<u16>) -> Entity {
    let mut entity = app.world_mut().spawn((
        Transform::from_translation(Vec3::new(at.0, at.1, at.2)),
        MeshRenderer::new(cube(), material(color)),
    ));
    if let Some(cell) = cell {
        entity.insert(VisCell(cell));
    }
    entity.id()
}

/// The entity has a colour draw in the extracted frame (a shadow-only draw does not count).
fn color_drawn(app: &App, entity: Entity) -> bool {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .any(|m| m.entity == entity && m.lod_key >> 56 != 0xFF)
}

/// The entity has a shadow-only draw in the extracted frame.
fn shadow_only(app: &App, entity: Entity) -> bool {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .any(|m| m.entity == entity && m.lod_key >> 56 == 0xFF)
}

fn view_visible(app: &App, entity: Entity) -> bool {
    app.world().get::<ViewVisibility>(entity).is_some_and(|v| v.0)
}

fn stats(app: &App) -> VisCellStats {
    app.world().resource::<VisCellStats>().clone()
}

/// The two-room scene: A's mesh (cell 1), B's mesh (cell 2), and an outdoor mesh (cell 0)
/// beyond the window. Returns (a, b, outdoor).
fn scene(app: &mut App) -> (Entity, Entity, Entity) {
    let a = mesh_at(app, (4.3, 0.5, -1.0), [0.8, 0.8, 0.8, 1.0], Some(1));
    let b = mesh_at(app, (3.0, 1.5, -8.0), [0.9, 0.1, 0.1, 1.0], Some(2));
    let outdoor = mesh_at(app, (0.0, 1.0, 12.0), [0.1, 0.8, 0.2, 1.0], Some(0));
    (a, b, outdoor)
}

#[test]
fn facing_away_from_the_door_counts_room_b_culled() {
    let mut app = app(FACING_AWAY);
    let (a, b, _) = scene(&mut app);
    // A mesh in room B on the view's line, so the frustum alone would draw it if it were ahead.
    app.insert_resource(cells(false));
    app.update();
    let s = stats(&app);
    assert!(s.active, "{s:?}");
    assert_eq!(s.views.len(), 1);
    assert_eq!(s.views[0].eye_cell, 1, "the eye is in room A");
    assert_eq!(s.views[0].reachable, vec![1], "only room A is reached: {s:?}");
    assert_eq!(s.reachable, vec![1]);
    assert!(color_drawn(&app, a), "room A's own mesh draws");
    assert!(!color_drawn(&app, b), "room B's mesh is out of the colour pass");
    assert!(
        shadow_only(&app, b),
        "room B still casts: stage 1 does not cull shadow views"
    );
    // B and the outdoor mesh (cell 0, no window portal) are culled; both cast, so both stay as
    // shadow-only draws.
    assert_eq!(s.culled_draws, 2, "{s:?}");
    assert_eq!(s.shadow_only_draws, 2, "{s:?}");
    assert_eq!(s.cell_draws, 3);
    assert!(!view_visible(&app, b));
}

#[test]
fn a_culled_draw_that_casts_nothing_leaves_the_frame() {
    let mut app = app(FACING_AWAY);
    let (_, b, _) = scene(&mut app);
    app.world_mut().entity_mut(b).insert(NotShadowCaster);
    app.insert_resource(cells(false));
    app.update();
    assert!(!color_drawn(&app, b) && !shadow_only(&app, b));
    let s = stats(&app);
    assert_eq!(s.culled_draws, 2);
    assert_eq!(s.shadow_only_draws, 1, "only the outdoor mesh casts: {s:?}");
}

#[test]
fn looking_through_the_door_draws_room_b() {
    let mut app = app(THROUGH_THE_DOOR);
    let (_, b, _) = scene(&mut app);
    app.insert_resource(cells(false));
    app.update();
    let s = stats(&app);
    assert_eq!(s.views[0].reachable, vec![1, 2], "{s:?}");
    assert_eq!(s.views[0].portals_passed, 1);
    assert!(color_drawn(&app, b));
    assert!(view_visible(&app, b));
}

#[test]
fn a_portal_to_cell_0_keeps_the_outside_reachable() {
    let mut with_window = app(FACING_AWAY);
    let (_, _, outdoor) = scene(&mut with_window);
    with_window.insert_resource(cells(true));
    with_window.update();
    assert_eq!(stats(&with_window).reachable, vec![0, 1]);
    assert!(
        color_drawn(&with_window, outdoor),
        "seen through the window, the outside draws"
    );
    assert!(view_visible(&with_window, outdoor));

    let mut without = app(FACING_AWAY);
    let (_, _, outdoor) = scene(&mut without);
    without.insert_resource(cells(false));
    without.update();
    assert!(
        !color_drawn(&without, outdoor),
        "no portal chain reaches the outside"
    );
}

#[test]
fn entities_without_a_cell_or_with_an_unlisted_cell_always_draw() {
    let mut app = app(FACING_AWAY);
    scene(&mut app);
    let plain = mesh_at(&mut app, (0.0, 1.0, -8.0), [0.5; 4], None);
    let unlisted = mesh_at(&mut app, (1.0, 1.0, -8.0), [0.5; 4], Some(77));
    app.insert_resource(cells(false));
    app.update();
    assert!(color_drawn(&app, plain), "no VisCell: always drawn");
    assert!(color_drawn(&app, unlisted), "an unlisted cell: drawn");
    assert_eq!(stats(&app).unknown_cell_draws, 1);
}

#[test]
fn without_cells_or_with_cull_off_nothing_is_culled() {
    let mut app = app(FACING_AWAY);
    let (_, b, outdoor) = scene(&mut app);
    app.update();
    assert!(color_drawn(&app, b) && color_drawn(&app, outdoor));
    assert!(!stats(&app).active);

    app.insert_resource(cells(false));
    app.insert_resource(VisCellSettings { cull: false });
    app.update();
    assert!(color_drawn(&app, b) && color_drawn(&app, outdoor));
    assert!(!stats(&app).active);

    app.insert_resource(VisCellSettings { cull: true });
    app.update();
    assert!(!color_drawn(&app, b), "culling back on");
}

#[test]
fn a_hidden_entity_never_reaches_the_cull() {
    // The ADR-7200 filter runs first: a hidden mesh in a reachable cell stays out.
    let mut app = app(THROUGH_THE_DOOR);
    let (_, b, _) = scene(&mut app);
    app.world_mut().entity_mut(b).insert(Visibility::Hidden);
    app.insert_resource(cells(false));
    app.update();
    assert!(!color_drawn(&app, b) && !shadow_only(&app, b));
    assert_eq!(stats(&app).cell_draws, 2, "the hidden mesh is not a cell draw");
}

#[test]
fn a_dense_cyclic_portal_set_stops_at_the_visit_cap_and_draws_everything() {
    // 24 thin cells in a row, every pair (i < j) joined by a portal facing the camera at
    // z −(30 + j), larger the deeper it sits: every chain of increasing ids stays in view (2^23
    // of them), so the walk runs into the visit cap.
    let mut text = String::from("(cells: [");
    for i in 1..=24 {
        text.push_str(&format!(
            "(id: {i}, bounds: [(center: (0.0, 1.0, {:.1}), half_extents: (50.0, 50.0, 0.5))]),",
            -(i as f32)
        ));
    }
    text.push_str("], portals: [");
    for i in 1..=24 {
        for j in (i + 1)..=24 {
            let (z, h) = (-(30.0 + j as f32), 40.0 + 2.0 * j as f32);
            text.push_str(&format!(
                "(a: {i}, b: {j}, quad: [({:.1}, {:.1}, {z:.1}), ({h:.1}, {:.1}, {z:.1}), ({h:.1}, {:.1}, {z:.1}), ({:.1}, {:.1}, {z:.1})]),",
                -h,
                1.0 - h,
                1.0 - h,
                1.0 + h,
                -h,
                1.0 + h
            ));
        }
    }
    text.push_str("])");
    let dense = VisCells::from_ron(&text).expect("valid");
    let mut app = app(View {
        eye: Vec3::new(0.0, 1.0, -1.0),
        yaw: 0.0,
        fov_y: 1.0,
    });
    let far = mesh_at(&mut app, (0.0, 1.0, -20.0), [0.5; 4], Some(24));
    app.insert_resource(dense);
    app.update();
    let s = stats(&app);
    assert!(s.views[0].capped, "{:?}", s.views[0].visits);
    assert_eq!(s.views[0].visits, MAX_CELL_VISITS, "the counter bounds the walk");
    assert_eq!(s.visits, MAX_CELL_VISITS);
    assert_eq!(s.reachable.len(), 25, "at the cap every cell counts as reachable");
    assert!(color_drawn(&app, far));
}

/// The two rooms with boxes that stop at the faces of a 0.3 m wall (z −3.85 to −4.15), the
/// doorway portal in the wall's middle plane: the layout a bake that boxes each room's interior
/// produces.
const TWO_ROOMS_WITH_A_WALL: &str = "(
    version: 1,
    cells: [
        (id: 1, name: \"room_a\", bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 3.85))]),
        (id: 2, name: \"room_b\", bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 3.85))]),
    ],
    portals: [
        (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
    ],
)";

#[test]
fn an_eye_in_a_doorways_wall_gap_draws_both_rooms() {
    // Walking through the door: the eye is in the wall's thickness, in neither room's box. Both
    // rooms keep drawing, looking ahead into B and looking back into A.
    for (z, yaw) in [(-4.05, 0.0), (-3.9, 0.0), (-4.05, PI), (-3.9, PI)] {
        let mut app = app(View {
            eye: Vec3::new(0.0, 1.0, z),
            yaw,
            fov_y: std::f32::consts::FRAC_PI_4,
        });
        let a = mesh_at(&mut app, (0.0, 1.0, 2.0), [0.8, 0.8, 0.8, 1.0], Some(1));
        let b = mesh_at(&mut app, (0.0, 1.0, -8.0), [0.9, 0.1, 0.1, 1.0], Some(2));
        app.insert_resource(VisCells::from_ron(TWO_ROOMS_WITH_A_WALL).expect("loads"));
        app.update();
        let s = stats(&app);
        assert_eq!(s.views[0].reachable, vec![1, 2], "eye at z {z}, yaw {yaw}: {s:?}");
        assert_eq!(s.culled_draws, 0, "eye at z {z}, yaw {yaw}: {s:?}");
        assert!(
            color_drawn(&app, a) && color_drawn(&app, b),
            "eye at z {z}, yaw {yaw}"
        );
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The scene's first frame, rendered by a fresh renderer (frame number 1 for every world).
fn render_first_frame(device: &RenderDevice, app: &mut App) -> Vec<u8> {
    app.update();
    let extracted = app.world().resource::<ExtractedFrame>();
    let scene = extracted.frame_scene(0, 64, 48);
    FrameRenderer::new()
        .render_offscreen(device, &scene)
        .expect("frame")
        .pixels
}

fn differing(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).filter(|(x, y)| x != y).count()
}

/// A view that sees B's mesh but not the doorway: with cells the frame equals the control frame
/// where B is hidden, and differs from the frame without cells (where B shows), so the test sees B.
#[test]
fn past_the_door_the_frame_equals_the_control_with_room_b_hidden() {
    let device = device();
    let build = |with_cells: bool, hide_b: bool| {
        let mut app = app(PAST_THE_DOOR);
        let (_, b, _) = scene(&mut app);
        if hide_b {
            app.world_mut().entity_mut(b).insert(Visibility::Hidden);
        }
        if with_cells {
            app.insert_resource(cells(false));
        }
        (app, b)
    };
    let (mut culled_app, b) = build(true, false);
    let culled = render_first_frame(&device, &mut culled_app);
    let s = stats(&culled_app);
    assert_eq!(s.reachable, vec![1], "{s:?}");
    assert_eq!(s.culled_draws, 2, "B and the outdoor mesh: {s:?}");
    assert!(shadow_only(&culled_app, b), "B stays a shadow caster");
    assert!(
        !view_visible(&culled_app, b),
        "in the frustum but culled by its cell: not view-visible"
    );
    let (mut open_app, open_b) = build(false, false);
    let open = render_first_frame(&device, &mut open_app);
    assert!(view_visible(&open_app, open_b), "without cells B is on screen");
    let control = render_first_frame(&device, &mut build(false, true).0);
    assert_eq!(
        differing(&culled, &control),
        0,
        "the culled frame equals the control with room B hidden"
    );
    assert!(
        differing(&open, &control) > 64,
        "without cells B shows: the view really sees it"
    );
}

/// Looking through the doorway: the frame with cells equals the frame of the same world without
/// cells.
#[test]
fn through_the_door_the_frame_equals_the_control_without_cells() {
    let device = device();
    let build = |with_cells: bool| {
        let mut app = app(THROUGH_THE_DOOR_NARROW);
        let b = mesh_at(&mut app, (0.0, 1.0, -8.0), [0.9, 0.1, 0.1, 1.0], Some(2));
        mesh_at(&mut app, (2.0, 0.5, -1.0), [0.8, 0.8, 0.8, 1.0], Some(1));
        if with_cells {
            app.insert_resource(cells(false));
        }
        (app, b)
    };
    let (mut with_cells, b) = build(true);
    let frame = render_first_frame(&device, &mut with_cells);
    assert!(color_drawn(&with_cells, b));
    let s = stats(&with_cells);
    assert_eq!(
        s.views[0].reachable,
        vec![1, 2],
        "the walk passes the door: {s:?}"
    );
    assert_eq!(s.culled_draws, 0);
    let (mut control, _) = build(false);
    let control = render_first_frame(&device, &mut control);
    assert_eq!(
        differing(&frame, &control),
        0,
        "through the door the frame equals the control"
    );
    // And B really draws there: hidden, the frame changes.
    let (mut hidden, b) = build(true);
    hidden.world_mut().entity_mut(b).insert(Visibility::Hidden);
    let hidden = render_first_frame(&device, &mut hidden);
    let shown = differing(&hidden, &control);
    assert!(
        shown > 64,
        "hiding B changes only {shown} bytes: the view does not see it"
    );
}

/// A building root at `at` tagged with `cell` (no mesh of its own), the way DO27's bake places a
/// building: the cell on the root, the parts spawned as its children.
fn building_root(app: &mut App, at: (f32, f32, f32), cell: u16) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(at.0, at.1, at.2)),
            VisCell(cell),
        ))
        .id()
}

/// A mesh part under `parent` at `offset` from it, without a `VisCell` of its own.
fn part(app: &mut App, parent: Entity, offset: (f32, f32, f32)) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(offset.0, offset.1, offset.2)),
            MeshRenderer::new(cube(), material([0.6, 0.6, 0.6, 1.0])),
            ChildOf(parent),
        ))
        .id()
}

/// Stage 2, inherited cells: the parts of a cell-tagged root (and their children) take the root's
/// cell, so facing away from the door culls room B's parts, not only meshes tagged themselves. A
/// part with its own `VisCell` keeps it, and a hierarchy with no `VisCell` anywhere still draws.
#[test]
fn the_parts_of_a_cell_tagged_root_take_its_cell() {
    let mut app = app(FACING_AWAY);
    let root_a = building_root(&mut app, (2.0, 0.0, -1.0), 1);
    let root_b = building_root(&mut app, (2.0, 0.0, -8.0), 2);
    let a_part = part(&mut app, root_a, (0.0, 0.5, 0.0));
    let b_part = part(&mut app, root_b, (0.0, 0.5, 0.0));
    let b_grandchild = part(&mut app, b_part, (1.0, 0.0, 0.0));
    // A part of B's building tagged room A itself (a balcony seen from A): the nearest tag wins.
    let b_part_tagged_a = part(&mut app, root_b, (-1.0, 0.5, 0.0));
    app.world_mut().entity_mut(b_part_tagged_a).insert(VisCell(1));
    // Its child inherits room A from it, not room B from the root.
    let under_tagged_a = part(&mut app, b_part_tagged_a, (0.0, 1.0, 0.0));
    let plain_root = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(0.0, 0.0, -8.0)))
        .id();
    let plain_part = part(&mut app, plain_root, (0.0, 0.5, 0.0));
    app.insert_resource(cells(false));
    app.update();
    let s = stats(&app);
    assert_eq!(s.reachable, vec![1], "{s:?}");
    assert!(color_drawn(&app, a_part), "room A's part draws");
    assert!(
        !color_drawn(&app, b_part),
        "room B's part inherits cell 2 and is culled"
    );
    assert!(shadow_only(&app, b_part), "and still casts");
    assert!(
        !color_drawn(&app, b_grandchild),
        "a grandchild inherits through its parent"
    );
    assert!(
        color_drawn(&app, b_part_tagged_a),
        "its own tag wins over the root's"
    );
    assert!(
        color_drawn(&app, under_tagged_a),
        "the nearest ancestor's tag wins"
    );
    assert!(
        color_drawn(&app, plain_part),
        "no VisCell up the chain: always drawn"
    );
    // Four parts resolve to a cell (a_part, b_part, b_grandchild, under_tagged_a) plus the one
    // tagged itself; the plain part is not a cell draw.
    assert_eq!(s.cell_draws, 5, "{s:?}");
    assert_eq!(s.culled_draws, 2, "{s:?}");
    assert!(!view_visible(&app, b_part) && !view_visible(&app, b_grandchild));
}

/// The inherited cells follow edits: a part moved under room A's root draws, a root that loses
/// its tag stops culling its parts, and a root tagged later starts.
#[test]
fn inherited_cells_follow_reparenting_and_tag_edits() {
    let mut app = app(FACING_AWAY);
    let root_a = building_root(&mut app, (2.0, 0.0, -1.0), 1);
    let root_b = building_root(&mut app, (2.0, 0.0, -8.0), 2);
    let moved = part(&mut app, root_b, (0.0, 0.5, 0.0));
    let stays = part(&mut app, root_b, (1.0, 0.5, 0.0));
    app.insert_resource(cells(false));
    app.update();
    assert!(!color_drawn(&app, moved) && !color_drawn(&app, stays));

    app.world_mut().entity_mut(moved).insert(ChildOf(root_a));
    app.update();
    assert!(color_drawn(&app, moved), "reparented under room A: drawn");
    assert!(!color_drawn(&app, stays), "its sibling stays culled");

    app.world_mut().entity_mut(root_b).remove::<VisCell>();
    app.update();
    assert!(
        color_drawn(&app, stays),
        "the root's tag removed: no cell up the chain, drawn"
    );

    app.world_mut().entity_mut(root_b).insert(VisCell(2));
    app.update();
    assert!(!color_drawn(&app, stays), "tagged again: culled");

    // A part spawned later under the tagged root inherits on its first frame.
    let late = part(&mut app, root_b, (2.0, 0.5, 0.0));
    app.update();
    assert!(
        !color_drawn(&app, late),
        "a new part takes the root's cell at once"
    );
    assert!(shadow_only(&app, late));
}

/// Item 5 (verify-finish, open finding 3): a culled caster turns into a shadow-only draw, whose
/// mesh-cache key differs from its colour draw's. Walking out of and back into view of room B ten
/// times must not upload B's meshes again each time: after the first walk the mesh-cache upload
/// counter stays put. Lavapipe, one renderer for every frame.
#[test]
fn walking_in_and_out_of_a_room_ten_times_uploads_no_mesh_again() {
    let device = device();
    let mut app = app(FACING_AWAY);
    scene(&mut app);
    let root_b = building_root(&mut app, (-2.0, 0.0, -8.0), 2);
    part(&mut app, root_b, (0.0, 0.5, 0.0));
    app.world_mut().spawn(regia_render::DirectionalLight::default());
    app.insert_resource(cells(false));
    let mut renderer = FrameRenderer::new();
    let frame = |renderer: &mut FrameRenderer, app: &mut App, view: View| {
        let camera = {
            let world = app.world_mut();
            let mut cameras = world.query_filtered::<Entity, With<Camera>>();
            cameras.single(world).expect("one camera")
        };
        app.world_mut().entity_mut(camera).insert(Transform {
            translation: view.eye,
            rotation: Quat::from_rotation_y(view.yaw),
            ..Transform::default()
        });
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        renderer
            .render_offscreen(&device, &extracted.frame_scene(0, 64, 48))
            .expect("frame");
        stats(app)
    };
    // One walk in and out: both variants of every mesh have been drawn once.
    let out = frame(&mut renderer, &mut app, FACING_AWAY);
    eprintln!(
        "mesh uploads after the first frame (room B shadow-only): {}",
        renderer.cache_stats().mesh_uploads
    );
    assert_eq!(out.reachable, vec![1], "{out:?}");
    assert!(
        out.shadow_only_draws >= 2,
        "room B's casters turn shadow-only: {out:?}"
    );
    let seen = frame(&mut renderer, &mut app, THROUGH_THE_DOOR);
    assert_eq!(seen.reachable, vec![1, 2], "{seen:?}");
    let warm = renderer.cache_stats().mesh_uploads;
    eprintln!("mesh uploads after the first walk out and back: {warm}");
    for walk in 0..10 {
        let out = frame(&mut renderer, &mut app, FACING_AWAY);
        assert!(out.shadow_only_draws >= 2, "walk {walk}: {out:?}");
        let seen = frame(&mut renderer, &mut app, THROUGH_THE_DOOR);
        assert_eq!(
            seen.culled_draws, 1,
            "walk {walk}: only the outdoor mesh: {seen:?}"
        );
    }
    let after = renderer.cache_stats();
    assert_eq!(
        after.mesh_uploads - warm,
        0,
        "ten more walks upload no mesh again: {after:?}"
    );
}

/// A point light at `at` with influence `radius`, optionally tagged or under `parent`.
fn point_light(app: &mut App, at: (f32, f32, f32), radius: f32, parent: Option<Entity>) -> Entity {
    let mut light = app.world_mut().spawn((
        Transform::from_translation(Vec3::new(at.0, at.1, at.2)),
        PointLight {
            radius,
            intensity: 4000.0,
            ..PointLight::default()
        },
    ));
    if let Some(parent) = parent {
        light.insert(ChildOf(parent));
    }
    light.id()
}

/// The frame's point and spot lights, by the entity that made them.
fn lit(app: &App) -> Vec<Entity> {
    app.world()
        .resource::<ExtractedFrame>()
        .local_light_entities
        .clone()
}

/// Stage 2, local lights: a lamp of room B (a child of B's root) whose range stays inside room B
/// leaves the frame while no view reaches B, and comes back through the door. A lamp whose range
/// reaches into room A's box stays (it may light A's surfaces), as do lamps without a cell. The
/// side tables stay aligned: the LTC mark of the lamp after the culled one follows it.
#[test]
fn a_lamp_of_an_unreached_room_leaves_the_frame_unless_its_range_reaches_a_reached_room() {
    let mut app = app(FACING_AWAY);
    let root_b = building_root(&mut app, (0.0, 0.0, -8.0), 2);
    // B's box spans z −12 to −4; A's z −4 to 4. 3 m of range at z −9 stays 2 m short of A.
    let deep = point_light(&mut app, (0.0, 1.5, -1.0), 3.0, Some(root_b));
    // 6 m of range at z −7 crosses z −4 into A.
    let near_door = point_light(&mut app, (0.0, 1.5, 1.0), 6.0, Some(root_b));
    let untagged = point_light(&mut app, (2.0, 1.5, -9.0), 3.0, None);
    let spot = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(-2.0, 2.5, -10.0)),
            SpotLight {
                range: 2.0,
                ..SpotLight::default()
            },
            VisCell(2),
            LightSourceLtc,
        ))
        .id();
    app.insert_resource(cells(false));
    app.update();
    let s = stats(&app);
    assert_eq!(s.reachable, vec![1], "{s:?}");
    assert_eq!(s.cell_lights, 3, "{s:?}");
    assert_eq!(s.culled_lights, 2, "the deep lamp and the spot: {s:?}");
    let kept = lit(&app);
    assert!(!kept.contains(&deep) && !kept.contains(&spot), "{kept:?}");
    assert!(kept.contains(&near_door) && kept.contains(&untagged), "{kept:?}");
    let frame = app.world().resource::<ExtractedFrame>();
    assert_eq!(frame.local_lights.len(), 2);
    assert!(
        frame
            .local_lights
            .iter()
            .all(|l| matches!(l, LabLocalLight::Point(_))),
        "the spot left with its entity"
    );
    assert!(
        !frame.light_extras.ltc_spheres.iter().any(|on| *on),
        "the culled spot's LTC mark left with it: {:?}",
        frame.light_extras.ltc_spheres
    );

    // Through the door B is reached: every lamp is back, and the spot keeps its LTC mark.
    let camera = {
        let world = app.world_mut();
        let mut cameras = world.query_filtered::<Entity, With<Camera>>();
        cameras.single(world).expect("one camera")
    };
    app.world_mut().entity_mut(camera).insert(Transform {
        translation: THROUGH_THE_DOOR.eye,
        rotation: Quat::from_rotation_y(THROUGH_THE_DOOR.yaw),
        ..Transform::default()
    });
    app.update();
    let s = stats(&app);
    assert_eq!(s.culled_lights, 0, "{s:?}");
    assert_eq!(lit(&app).len(), 4);
    let frame = app.world().resource::<ExtractedFrame>();
    let spot_index = frame
        .local_light_entities
        .iter()
        .position(|e| *e == spot)
        .expect("the spot is back");
    assert_eq!(frame.light_extras.ltc_spheres.get(spot_index), Some(&true));
}

/// Stage 2, glass: a pane of room B (its colour draw culled) adds no glass instance, so the
/// renderer prepares nothing for it; through the door it does.
#[test]
fn a_glass_pane_of_an_unreached_room_adds_no_glass_instance() {
    let mut app = app(FACING_AWAY);
    let root_b = building_root(&mut app, (0.0, 0.0, -8.0), 2);
    let pane = part(&mut app, root_b, (0.0, 1.0, 0.0));
    app.world_mut()
        .entity_mut(pane)
        .insert(GlassView::viewer_inside());
    let open_pane = mesh_at(&mut app, (2.0, 1.0, 1.0), [0.5; 4], Some(1));
    app.world_mut()
        .entity_mut(open_pane)
        .insert(GlassView::viewer_inside());
    app.insert_resource(cells(false));
    app.update();
    let keys: Vec<u64> = app
        .world()
        .resource::<ExtractedFrame>()
        .glass
        .iter()
        .map(|g| g.object_key)
        .collect();
    assert_eq!(keys, vec![open_pane.to_bits()], "only room A's pane");
    let culled = app.world().resource::<VisCellCulled>();
    assert_eq!(culled.glass, 1);
    assert!(culled.contains(pane) && !culled.contains(open_pane));
    app.insert_resource(VisCellSettings { cull: false });
    app.update();
    assert_eq!(
        app.world().resource::<ExtractedFrame>().glass.len(),
        2,
        "cull off: both panes"
    );
    assert_eq!(app.world().resource::<VisCellCulled>().glass, 0);
}

/// The lamp and glass routes on lavapipe: past the door (B's mesh in the frustum, the door not),
/// with B's parts under a cell-tagged root, a glass pane and a lamp of room B, the frame equals
/// the control without B (no cells, B's root hidden, the lamp despawned); with culling off it
/// differs (B shows), so the view really sees B.
#[test]
fn past_the_door_a_room_with_parts_glass_and_a_lamp_renders_as_the_control_without_it() {
    let device = device();
    let build = |with_cells: bool, cull: bool, without_b: bool| {
        let mut app = app(PAST_THE_DOOR);
        mesh_at(&mut app, (4.3, 0.5, -1.0), [0.8, 0.8, 0.8, 1.0], Some(1));
        let root_b = building_root(&mut app, (3.0, 0.0, -8.0), 2);
        let body = part(&mut app, root_b, (0.0, 1.5, 0.0));
        app.world_mut()
            .entity_mut(body)
            .insert(MeshRenderer::new(cube(), material([0.9, 0.1, 0.1, 1.0])));
        let pane = part(&mut app, root_b, (-1.0, 1.0, 1.0));
        app.world_mut()
            .entity_mut(pane)
            .insert(GlassView::viewer_inside());
        if !without_b {
            // At z −7 with 2.5 m of range: 0.5 m short of room A's box (z −4).
            point_light(&mut app, (0.0, 2.0, 1.0), 2.5, Some(root_b));
        } else {
            app.world_mut().entity_mut(root_b).insert(Visibility::Hidden);
        }
        if with_cells {
            app.insert_resource(cells(false));
            app.insert_resource(VisCellSettings { cull });
        }
        app
    };
    let mut culled_app = build(true, true, false);
    let culled = render_first_frame(&device, &mut culled_app);
    let s = stats(&culled_app);
    assert_eq!(
        (s.culled_draws, s.culled_lights),
        (2, 1),
        "B's body and pane, B's lamp: {s:?}"
    );
    assert_eq!(culled_app.world().resource::<VisCellCulled>().glass, 1);
    let control = render_first_frame(&device, &mut build(false, true, true));
    assert_eq!(
        differing(&culled, &control),
        0,
        "the culled frame equals the control without room B"
    );
    let mut off_app = build(true, false, false);
    let off = render_first_frame(&device, &mut off_app);
    assert_eq!(stats(&off_app).culled_lights, 0);
    assert!(
        differing(&off, &control) > 64,
        "with culling off B shows: the view really sees it"
    );
}

/// Split-screen inside a cell (ADR-8843): every window view gets its own outdoor list, so the
/// terrain, its grass and impostors, and the water cull for each player, and the shared draw list
/// (the scatter) takes their union, restricted only while every player is inside.
#[test]
fn every_split_screen_view_gets_its_outdoor_list() {
    // Player 1 (left half): in room A looking out of the window. Player 2 (right half): in room
    // A facing the doorway, so no frustum reaches the outdoors.
    let mut app = app(View {
        eye: Vec3::new(0.0, 1.0, 0.0),
        yaw: PI,
        fov_y: std::f32::consts::FRAC_PI_4,
    });
    let first = {
        let mut q = app.world_mut().query::<(Entity, &Camera)>();
        q.iter(app.world())
            .map(|(e, _)| e)
            .next()
            .expect("the app's camera")
    };
    app.world_mut().get_mut::<Camera>(first).expect("camera").viewport = [0.0, 0.0, 0.5, 1.0];
    let second = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.5, 1.0, 0.0)),
            Camera {
                fov_y: std::f32::consts::FRAC_PI_4,
                exposure_ev: Some(10.0),
                viewport: [0.5, 0.0, 0.5, 1.0],
                order: 1,
                ..Camera::default()
            },
        ))
        .id();
    app.insert_resource(cells(true));
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    let windows = frame.window_views();
    assert_eq!(windows.len(), 2, "two window views");
    let (i1, i2) = (windows[0].0, windows[1].0);
    let l1 = frame.outdoor_portals_for(i1).expect("player 1 is inside: a list");
    let l2 = frame.outdoor_portals_for(i2).expect("player 2 is inside: a list");
    assert!(l1.is_restricted() && l2.is_restricted());
    assert_eq!(l1.frusta().len(), 1, "player 1 sees out of the window");
    assert!(l2.frusta().is_empty(), "player 2 sees no outdoors");
    assert!(l1.restricts(windows[0].1.camera.eye) && l2.restricts(windows[1].1.camera.eye));
    // The union keeps player 1's frustum and is made for the first window view's eye.
    assert!(frame.outdoor_portals.restricts(windows[0].1.camera.eye));
    assert_eq!(frame.outdoor_portals.frusta().len(), 1);

    // Player 2 steps outside: their view has no list, and the shared list is unrestricted.
    app.world_mut()
        .get_mut::<Transform>(second)
        .expect("transform")
        .translation = Vec3::new(0.0, 1.0, 10.0);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    let windows = frame.window_views();
    assert!(
        frame
            .outdoor_portals_for(windows[0].0)
            .is_some_and(|l| l.is_restricted())
    );
    assert!(
        frame.outdoor_portals_for(windows[1].0).is_none(),
        "outdoors: unrestricted"
    );
    assert!(!frame.outdoor_portals.is_restricted());
}
