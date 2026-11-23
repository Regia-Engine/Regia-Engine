// SPDX-License-Identifier: MPL-2.0
//! Press routing of the viewport's pane tools while the transform gizmo is shown (lane
//! w10-gizmo-press-audit, ADR-7674; the ADR-8870 / ADR-7593 follow-up).
//!
//! transform-gizmo-egui's `GizmoExt::interact` registers a 1x1 click-and-drag widget under the
//! pointer every frame while the gizmo is drawn, and egui gives that widget the hover and every
//! press. Every pane tool that reads the pane's response (spline grips, the procedural path and
//! area tools, 2D mode, fracture clicks) then lost its press whenever an entity with a transform
//! was selected, in its own pane or, since every pane draws its gizmo with the same widget id, in
//! another pane of the quad layout. Each test here drives `viewport_panel` through a headless
//! egui context with real pointer events (no window, no GPU, no wall clock): a press on the
//! tool's handle must move only that handle, and a press beside it must reach the camera or the
//! selection as it does with nothing selected.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use regia::physics::{Collider, ColliderShape};
use regia_2d::tilemap::{MapLayer, ObjectLayer, Orientation, TileLayer, TileMapAsset, TilesetDef};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Vec2, Vec3};
use regia_editor::collider_handles::{self, ColliderHandleTool};
use regia_editor::command::UndoStack;
use regia_editor::mode2d::{Mode2d, TileDocument};
use regia_editor::procgen::presets::TOOL_ROAD_PATH;
use regia_editor::procgen::tools::{self, ToolKind};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::terrain_tool::TerrainTool;
use regia_editor::viewport::{EditorCamera, ViewportPanelState, viewport_panel};
use regia_editor::views::{ViewCamera, ViewKind, ViewportLayout};
use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource};
use regia_scene::GlobalTransform;
use regia_scene::components::Generated;
use regia_scene::prelude::Transform;
use regia_scene::spline::{Spline, SplinePoint};

const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);

/// The main viewport panel (toolbar off, so the panes fill the screen) over `world`.
struct Rig {
    ctx: egui::Context,
    world: World,
    state: ViewportPanelState,
    /// The panel's pane area, as of the last frame.
    full: egui::Rect,
    /// Force a repeated layout pass to exercise per-frame input guards.
    repeat_pass: bool,
}

impl Rig {
    fn new(mut world: World) -> Self {
        world.get_resource_or_insert_with(Selection::default);
        world.get_resource_or_insert_with(UndoStack::new);
        let mut state = ViewportPanelState::default();
        state.toolbar = false;
        let mut rig = Self {
            ctx: egui::Context::default(),
            world,
            state,
            full: egui::Rect::NOTHING,
            repeat_pass: false,
        };
        rig.frame(Vec::new());
        rig
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        let Self {
            ctx,
            world,
            state,
            full,
            repeat_pass,
            ..
        } = self;
        let raw = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            if *repeat_pass && ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("verify repeated viewport input");
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    *full = ui.available_rect_before_wrap();
                    viewport_panel(ui, world, None, state);
                });
        });
        output.textures_delta.clear();
    }

    fn layout(&self) -> ViewportLayout {
        self.world.resource::<ViewportLayout>().clone()
    }

    /// Pane `index`'s rectangle (the panel's split rule).
    fn pane_rect(&self, index: usize) -> egui::Rect {
        if !self.layout().quad {
            return self.full;
        }
        let half = self.full.size() * 0.5;
        let min = self.full.left_top() + egui::vec2((index % 2) as f32 * half.x, (index / 2) as f32 * half.y);
        egui::Rect::from_min_size(min, half - egui::vec2(2.0, 2.0))
    }

    /// Pane `index`'s camera.
    fn camera(&self, index: usize) -> EditorCamera {
        self.layout().pane_camera(&self.world, index).1
    }

    /// Where `point` shows in pane `index`.
    fn screen(&self, index: usize, point: Vec3) -> egui::Pos2 {
        let rect = self.pane_rect(index);
        let (kind, camera) = self.layout().pane_camera(&self.world, index);
        let at = ViewCamera::of(kind, &camera)
            .project(point, Vec2::new(rect.width(), rect.height()))
            .unwrap_or_else(|| panic!("{point} is in front of pane {index}'s camera"));
        let pos = rect.left_top() + egui::vec2(at.x, at.y);
        assert!(
            rect.contains(pos),
            "{point} projects into pane {index}: {pos:?} in {rect:?}"
        );
        pos
    }

    /// Rests the pointer at `pos` for two frames (the gizmo and the grips read the hover).
    fn hover(&mut self, pos: egui::Pos2) {
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        self.frame(Vec::new());
    }

    /// A pointer drag with `button` from `from` to `to`: hover, press, six moves, release.
    /// `mid` runs after the last move, before the release.
    fn drag(
        &mut self,
        button: egui::PointerButton,
        from: egui::Pos2,
        to: egui::Pos2,
        mid: impl FnOnce(&Self),
    ) {
        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.hover(from);
        self.frame(vec![press(from, true)]);
        let steps = 6;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            self.frame(vec![egui::Event::PointerMoved(from.lerp(to, t))]);
        }
        mid(self);
        self.frame(vec![press(to, false)]);
        self.frame(Vec::new());
    }

    /// A primary click at `pos`: hover, press, release.
    fn click(&mut self, pos: egui::Pos2) {
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.hover(pos);
        self.frame(vec![press(true)]);
        self.frame(vec![press(false)]);
        self.frame(Vec::new());
    }

    fn selected(&self) -> Vec<Entity> {
        self.world.resource::<Selection>().entities().to_vec()
    }

    fn translation(&self, entity: Entity) -> Vec3 {
        self.world
            .get::<Transform>(entity)
            .expect("transform")
            .translation
    }

    /// Whether pane `index`'s transform gizmo has a handle under the pointer (or is dragged).
    fn gizmo_focused(&self, index: usize) -> bool {
        self.state.gizmos[index].is_focused()
    }
}

/// Spawns an entity with a transform (the gizmo shows on it when it is the selection).
fn place(world: &mut World, at: Vec3) -> Entity {
    let transform = Transform::from_translation(at);
    world
        .spawn((transform, GlobalTransform(transform.compute_affine())))
        .id()
}

fn select(world: &mut World, entity: Entity) {
    let mut selection = Selection::default();
    selection.select(entity, SelectMode::Replace);
    world.insert_resource(selection);
}

/// A world whose selection (30 m along +X, away from the pane center) shows the gizmo.
fn world_with_gizmo() -> (World, Entity) {
    let mut world = World::new();
    let selected = place(&mut world, Vec3::new(30.0, 0.0, 0.0));
    select(&mut world, selected);
    (world, selected)
}

// ---------------------------------------------------------------------------------------------
// The transform gizmo itself (the control: its handle press must stay its own)
// ---------------------------------------------------------------------------------------------

/// A press on the gizmo's center handle drags the selection and nothing else: the camera stays,
/// the selection stays, no box select runs.
#[test]
fn a_press_on_the_gizmo_handle_moves_only_the_selection() {
    let mut world = World::new();
    let entity = place(&mut world, Vec3::new(0.0, 1.0, 0.0));
    select(&mut world, entity);
    let mut rig = Rig::new(world);
    let camera = rig.camera(0);
    let from = rig.screen(0, Vec3::new(0.0, 1.0, 0.0));
    rig.hover(from);
    assert!(
        rig.gizmo_focused(0),
        "the pointer rests on the gizmo's center handle"
    );
    rig.drag(
        egui::PointerButton::Primary,
        from,
        from + egui::vec2(40.0, 0.0),
        |_| {},
    );
    assert_ne!(
        rig.translation(entity),
        Vec3::new(0.0, 1.0, 0.0),
        "the handle drag moved the selection"
    );
    assert_eq!(rig.camera(0), camera, "the camera stayed");
    assert_eq!(rig.selected(), [entity], "no box select or pick ran");
}

/// A primary drag beside the gizmo box-selects (empty ground clears the selection), as with
/// nothing selected; the selection does not move.
#[test]
fn a_press_beside_the_gizmo_box_selects() {
    let (world, selected) = world_with_gizmo();
    let mut rig = Rig::new(world);
    let from = rig.pane_rect(0).center() + egui::vec2(-60.0, 40.0);
    rig.hover(from);
    assert!(!rig.gizmo_focused(0), "no gizmo handle under the pointer");
    rig.drag(
        egui::PointerButton::Primary,
        from,
        from + egui::vec2(60.0, 40.0),
        |_| {},
    );
    assert_eq!(
        rig.translation(selected),
        Vec3::new(30.0, 0.0, 0.0),
        "the selection did not move"
    );
    assert!(
        rig.selected().is_empty(),
        "the box over empty ground replaced the selection"
    );
}

/// A box select that starts beside the gizmo and sweeps across its center handle stays a box
/// select: the press was the pane's, so the handle it passes over never takes the drag.
#[test]
fn a_box_select_across_the_gizmo_handle_stays_a_box_select() {
    let mut world = World::new();
    let entity = place(&mut world, Vec3::new(0.0, 1.0, 0.0));
    select(&mut world, entity);
    let mut rig = Rig::new(world);
    let center = rig.screen(0, Vec3::new(0.0, 1.0, 0.0));
    let from = center + egui::vec2(-150.0, -110.0);
    rig.hover(from);
    assert!(!rig.gizmo_focused(0), "the press lands beside the gizmo");
    rig.drag(
        egui::PointerButton::Primary,
        from,
        center + egui::vec2(150.0, 110.0),
        |_| {},
    );
    assert_eq!(
        rig.translation(entity),
        Vec3::new(0.0, 1.0, 0.0),
        "the selection did not move"
    );
    assert!(
        rig.selected().is_empty(),
        "the box select ran and replaced the selection"
    );
}

/// A press that arrives in the same frame as the move onto the gizmo's center handle (a fast
/// flick-click, or a pen that reports hover only with the press) has one owner, the gizmo: it
/// moves the selection, and box select (which starts on egui's later drag start, when the gizmo
/// is already focused) and click picking leave it alone (integ-v06f review of ADR-7674).
#[test]
fn a_press_that_arrives_with_the_move_onto_a_handle_has_one_owner() {
    let mut world = World::new();
    let entity = place(&mut world, Vec3::new(0.0, 1.0, 0.0));
    select(&mut world, entity);
    let mut rig = Rig::new(world);
    let center = rig.screen(0, Vec3::new(0.0, 1.0, 0.0));
    let away = center + egui::vec2(-150.0, -110.0);
    rig.hover(away);
    assert!(!rig.gizmo_focused(0), "the pointer rests away from the gizmo");
    let press = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    // Move and press in one frame, then drag down-right and release.
    rig.frame(vec![egui::Event::PointerMoved(center), press(center, true)]);
    let to = center + egui::vec2(60.0, 40.0);
    for step in 1..=6 {
        rig.frame(vec![egui::Event::PointerMoved(
            center.lerp(to, step as f32 / 6.0),
        )]);
    }
    rig.frame(vec![press(to, false)]);
    rig.frame(Vec::new());
    assert_ne!(
        rig.translation(entity),
        Vec3::new(0.0, 1.0, 0.0),
        "the gizmo took the press and moved the selection"
    );
    assert_eq!(rig.selected(), [entity], "no box select or pick ran");
}

// ---------------------------------------------------------------------------------------------
// Collider edit handles (ADR-2696)
// ---------------------------------------------------------------------------------------------

/// A box collider on a selected entity with the handle tool on (the grips replace its gizmo).
fn collider_world() -> (World, Entity) {
    let mut world = World::new();
    let entity = place(&mut world, Vec3::new(0.0, 1.0, 0.0));
    world.entity_mut(entity).insert(Collider {
        shape: ColliderShape::Box {
            half_extents: Vec3::splat(1.0),
        },
        ..Collider::default()
    });
    select(&mut world, entity);
    let mut tool = ColliderHandleTool::default();
    tool.enabled = true;
    world.insert_resource(tool);
    (world, entity)
}

/// The first grip of `entity`'s collider that projects into pane 0 at least 40 px from the
/// shape's center, and its screen direction away from the center.
fn collider_grip(rig: &Rig, entity: Entity) -> (egui::Pos2, egui::Vec2) {
    let collider = rig.world.get::<Collider>(entity).expect("collider").clone();
    let body = collider_handles::body_pose(&rig.world, entity);
    let center = rig.screen(0, Vec3::new(0.0, 1.0, 0.0));
    collider_handles::handles_for(&collider, body)
        .iter()
        .map(|grip| rig.screen(0, grip.handle.anchor()))
        .find(|pos| (*pos - center).length() > 40.0)
        .map(|pos| (pos, (pos - center).normalized()))
        .expect("a grip away from the center")
}

#[test]
fn a_press_on_a_collider_grip_resizes_only_the_collider() {
    let (world, entity) = collider_world();
    let mut rig = Rig::new(world);
    let before = rig.world.get::<Collider>(entity).cloned();
    let camera = rig.camera(0);
    let (grip, out) = collider_grip(&rig, entity);
    rig.drag(egui::PointerButton::Primary, grip, grip + out * 30.0, |_| {});
    assert_ne!(
        rig.world.get::<Collider>(entity).cloned(),
        before,
        "the grip drag resized the collider"
    );
    assert_eq!(
        rig.translation(entity),
        Vec3::new(0.0, 1.0, 0.0),
        "the entity did not move"
    );
    assert_eq!(rig.camera(0), camera, "the camera stayed");
    assert_eq!(rig.selected(), [entity], "no box select or pick ran");
}

#[test]
fn a_press_beside_the_collider_grips_box_selects() {
    let (world, entity) = collider_world();
    let mut rig = Rig::new(world);
    let before = rig.world.get::<Collider>(entity).cloned();
    let from = rig.pane_rect(0).left_top() + egui::vec2(30.0, 30.0);
    rig.drag(
        egui::PointerButton::Primary,
        from,
        from + egui::vec2(60.0, 40.0),
        |_| {},
    );
    assert_eq!(
        rig.world.get::<Collider>(entity).cloned(),
        before,
        "the collider is unchanged"
    );
    assert!(
        rig.selected().is_empty(),
        "the box over empty ground replaced the selection"
    );
}

// ---------------------------------------------------------------------------------------------
// Spline control points (procedural path and area volumes, spec 67 § 5)
// ---------------------------------------------------------------------------------------------

const SPLINE_POINT: Vec3 = Vec3::new(2.5, 1.0, -2.5);

/// A spline entity with control points away from its origin. `selected`: it is the selection
/// (its gizmo shows at the origin); otherwise nothing is selected and it is a generated spline,
/// whose grips show without a selection (the no-gizmo control).
fn spline_world(selected: bool) -> (World, Entity) {
    let mut world = World::new();
    let entity = place(&mut world, Vec3::ZERO);
    let point = |position| SplinePoint {
        position,
        ..SplinePoint::default()
    };
    world.entity_mut(entity).insert(Spline {
        points: vec![point(Vec3::new(-3.0, 0.0, -3.0)), point(SPLINE_POINT)],
        ..Spline::default()
    });
    if selected {
        select(&mut world, entity);
    } else {
        world.entity_mut(entity).insert(Generated {
            generator: Guid(1),
            run_id: 1,
        });
    }
    (world, entity)
}

fn spline_points(rig: &Rig, entity: Entity) -> Vec<Vec3> {
    rig.world
        .get::<Spline>(entity)
        .expect("spline")
        .points
        .iter()
        .map(|p| p.position)
        .collect()
}

/// The control: the same press with no gizmo shown moves the point too.
#[test]
fn a_press_on_a_spline_point_moves_only_that_point() {
    for selected in [false, true] {
        let (world, entity) = spline_world(selected);
        let mut rig = Rig::new(world);
        let camera = rig.camera(0);
        let from = rig.screen(0, SPLINE_POINT);
        rig.hover(from);
        assert!(
            !rig.gizmo_focused(0),
            "the control point is off the gizmo's handles"
        );
        rig.drag(
            egui::PointerButton::Primary,
            from,
            from + egui::vec2(30.0, 20.0),
            |_| {},
        );
        let points = spline_points(&rig, entity);
        assert_eq!(points[0], Vec3::new(-3.0, 0.0, -3.0), "the other point stayed");
        assert_ne!(
            points[1], SPLINE_POINT,
            "the drag moved the pressed control point (gizmo shown: {selected})"
        );
        assert_eq!(rig.translation(entity), Vec3::ZERO, "the entity did not move");
        assert_eq!(rig.camera(0), camera, "the camera stayed");
        let expected: &[Entity] = if selected { &[entity] } else { &[] };
        assert_eq!(rig.selected(), expected, "no box select or pick ran");
    }
}

/// A control point under the gizmo's center handle: the handle shows hovered, so the press is
/// the gizmo's (decided at press time, ADR-7674). The entity moves and carries its spline; the
/// point keeps its local position.
#[test]
fn a_spline_point_under_a_gizmo_handle_leaves_the_press_to_the_gizmo() {
    let mut world = World::new();
    let origin = Vec3::new(0.0, 1.0, 0.0);
    let entity = place(&mut world, origin);
    let point = |position| SplinePoint {
        position,
        ..SplinePoint::default()
    };
    world.entity_mut(entity).insert(Spline {
        points: vec![point(Vec3::ZERO), point(Vec3::new(3.0, 0.0, -3.0))],
        ..Spline::default()
    });
    select(&mut world, entity);
    let mut rig = Rig::new(world);
    let from = rig.screen(0, origin);
    rig.hover(from);
    assert!(
        rig.gizmo_focused(0),
        "the pointer rests on the gizmo's center handle"
    );
    rig.drag(
        egui::PointerButton::Primary,
        from,
        from + egui::vec2(40.0, 0.0),
        |_| {},
    );
    assert_ne!(rig.translation(entity), origin, "the gizmo moved the entity");
    assert_eq!(
        spline_points(&rig, entity),
        [Vec3::ZERO, Vec3::new(3.0, 0.0, -3.0)],
        "the control point under the handle kept its place"
    );
    assert_eq!(rig.selected(), [entity], "no box select or pick ran");
}

#[test]
fn a_press_beside_the_spline_points_box_selects() {
    let (world, entity) = spline_world(true);
    let mut rig = Rig::new(world);
    let from = rig.pane_rect(0).left_top() + egui::vec2(30.0, 30.0);
    rig.drag(
        egui::PointerButton::Primary,
        from,
        from + egui::vec2(60.0, 40.0),
        |_| {},
    );
    assert_eq!(
        spline_points(&rig, entity),
        [Vec3::new(-3.0, 0.0, -3.0), SPLINE_POINT],
        "the spline is unchanged"
    );
    assert!(
        rig.selected().is_empty(),
        "the box over empty ground replaced the selection"
    );
}

// ---------------------------------------------------------------------------------------------
// The procedural Path tool (spec 67 § 5.2)
// ---------------------------------------------------------------------------------------------

/// A world with the procedural editor installed, a ground box, the Path tool armed and another
/// entity selected (its gizmo shows).
fn path_tool_world() -> (World, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>();
    regia_editor::procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    let mut world = std::mem::take(app.world_mut());
    let (vertices, indices) = regia_render::box_mesh([20.0, 0.5, 20.0]);
    let ground = Transform::from_translation(Vec3::new(0.0, -0.5, 0.0));
    world.spawn((
        ground,
        GlobalTransform(ground.compute_affine()),
        regia_render::MeshRenderer::new(
            Arc::new(regia_render::CpuMesh { vertices, indices }),
            Arc::new(regia_render::LabMaterial::default()),
        ),
    ));
    let selected = place(&mut world, Vec3::new(30.0, 0.0, 0.0));
    select(&mut world, selected);
    tools::arm(&mut world, ToolKind::Path, TOOL_ROAD_PATH);
    (world, selected)
}

/// The control points of every spline in the world.
fn all_spline_points(world: &mut World) -> usize {
    let mut query = world.query::<&Spline>();
    query.iter(world).map(|s| s.points.len()).sum()
}

#[test]
fn a_press_with_the_path_tool_armed_places_a_point() {
    let (world, selected) = path_tool_world();
    let mut rig = Rig::new(world);
    assert_eq!(all_spline_points(&mut rig.world), 0, "no path yet");
    let camera = rig.camera(0);
    let at = rig.pane_rect(0).center() + egui::vec2(-40.0, 60.0);
    rig.click(at);
    assert_eq!(
        all_spline_points(&mut rig.world),
        1,
        "the click placed the path's first point ({})",
        rig.world.resource::<tools::ProcViewportMode>().last_error
    );
    assert_eq!(
        rig.translation(selected),
        Vec3::new(30.0, 0.0, 0.0),
        "the selection did not move"
    );
    assert_eq!(rig.camera(0), camera, "the camera stayed");
}

/// A drag from a placed point moves it (the tool's grip), with another entity's gizmo shown.
#[test]
fn a_drag_from_a_path_point_moves_it() {
    let (world, selected) = path_tool_world();
    let mut rig = Rig::new(world);
    let at = rig.pane_rect(0).center() + egui::vec2(-40.0, 60.0);
    rig.click(at);
    let placed = path_points(&mut rig.world);
    assert_eq!(placed.len(), 1, "the click placed a point");
    rig.drag(
        egui::PointerButton::Primary,
        at,
        at + egui::vec2(60.0, 0.0),
        |_| {},
    );
    let moved = path_points(&mut rig.world);
    assert_eq!(moved.len(), 1, "the drag placed no second point: {moved:?}");
    assert!(
        moved[0].distance(placed[0]) > 0.5,
        "the drag moved the point: {placed:?} -> {moved:?}"
    );
    assert_eq!(
        rig.translation(selected),
        Vec3::new(30.0, 0.0, 0.0),
        "the selection did not move"
    );
}

/// The control points of every spline in the world, in world order.
fn path_points(world: &mut World) -> Vec<Vec3> {
    let mut query = world.query::<&Spline>();
    query
        .iter(world)
        .flat_map(|s| s.points.iter().map(|p| p.position))
        .collect()
}

/// The Path tool owns the primary button; the right button still orbits the camera.
#[test]
fn a_right_drag_with_the_path_tool_armed_orbits() {
    let (world, _) = path_tool_world();
    let mut rig = Rig::new(world);
    let camera = rig.camera(0);
    let from = rig.pane_rect(0).center() + egui::vec2(-40.0, 60.0);
    rig.drag(
        egui::PointerButton::Secondary,
        from,
        from + egui::vec2(50.0, 20.0),
        |_| {},
    );
    assert_ne!(rig.camera(0), camera, "the right drag orbited");
    assert_eq!(all_spline_points(&mut rig.world), 0, "no point placed");
}

// ---------------------------------------------------------------------------------------------
// Terrain sculpt brush
// ---------------------------------------------------------------------------------------------

/// A flat terrain under the camera, the sculpt tool on, and another entity selected.
fn terrain_world() -> (World, Entity) {
    let mut world = World::new();
    let n = 65u32;
    let map = regia_terrain::TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 64.0, 100.0, 0.0)
        .expect("map");
    let terrain = regia_terrain::Terrain::new(
        &map.to_heightfield(),
        64.0,
        100.0,
        0.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain");
    let paint = regia_terrain::TerrainPaint(regia_terrain::LayerWeightMap::new(n, n, 1).expect("paint"));
    world.spawn((terrain, paint));
    let selected = place(&mut world, Vec3::new(30.0, 0.0, 0.0));
    select(&mut world, selected);
    let mut tool = TerrainTool::default();
    tool.active = true;
    world.insert_resource(tool);
    (world, selected)
}

/// A pane point whose ray hits the terrain.
fn on_terrain(rig: &mut Rig) -> egui::Pos2 {
    let rect = rig.pane_rect(0);
    let camera = rig.camera(0);
    let size = Vec2::new(rect.width(), rect.height());
    let local = Vec2::new(rect.width() * 0.5 - 40.0, rect.height() * 0.5 + 60.0);
    let (origin, direction) = regia_editor::viewport::pick_ray(&camera, local, size);
    assert!(
        regia_editor::terrain_tool::pick_terrain(&mut rig.world, origin, direction).is_some(),
        "the pointer ray hits the terrain"
    );
    rect.left_top() + egui::vec2(local.x, local.y)
}

#[test]
fn a_press_on_the_terrain_starts_a_stroke() {
    let (world, selected) = terrain_world();
    let mut rig = Rig::new(world);
    let at = on_terrain(&mut rig);
    let camera = rig.camera(0);
    let mut stroking = false;
    rig.drag(
        egui::PointerButton::Primary,
        at,
        at + egui::vec2(30.0, 0.0),
        |rig| {
            stroking = rig.world.resource::<TerrainTool>().stroking();
        },
    );
    assert!(stroking, "the press started a sculpt stroke");
    assert_eq!(
        rig.translation(selected),
        Vec3::new(30.0, 0.0, 0.0),
        "the selection did not move"
    );
    assert_eq!(rig.selected(), [selected], "no box select or pick ran");
    assert_eq!(rig.camera(0), camera, "the camera stayed");
}

#[test]
fn a_right_drag_with_the_sculpt_tool_on_orbits() {
    let (world, _) = terrain_world();
    let mut rig = Rig::new(world);
    let at = on_terrain(&mut rig);
    let camera = rig.camera(0);
    let mut stroking = false;
    rig.drag(
        egui::PointerButton::Secondary,
        at,
        at + egui::vec2(50.0, 20.0),
        |rig| {
            stroking = rig.world.resource::<TerrainTool>().stroking();
        },
    );
    assert!(!stroking, "the right button does not sculpt");
    assert_ne!(rig.camera(0), camera, "the right drag orbited");
}

// ---------------------------------------------------------------------------------------------
// 2D mode in the quad layout (the front pane paints; the other panes still draw the gizmo)
// ---------------------------------------------------------------------------------------------

const FRONT: usize = 2;

/// A 6x4 map with one tile layer (layer 1).
fn tile_map() -> TileMapAsset {
    TileMapAsset {
        version: 1,
        orientation: Orientation::Orthogonal,
        width: 6,
        height: 4,
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![TilesetDef {
            name: "t".into(),
            first_gid: 1,
            image: "t.png".into(),
            image_width: 128,
            image_height: 64,
            tile_width: 16,
            tile_height: 16,
            columns: 8,
            tile_count: 32,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }],
        layers: vec![
            MapLayer::Objects(ObjectLayer {
                name: "spawns".into(),
                visible: true,
                objects: Vec::new(),
                properties: BTreeMap::new(),
            }),
            MapLayer::Tiles(TileLayer {
                name: "ground".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0; 24],
                properties: BTreeMap::new(),
            }),
        ],
        properties: BTreeMap::new(),
    }
}

/// The quad layout with the 2D mode on (its map framed in the front pane) and an entity with a
/// transform selected, so the perspective, top and side panes draw the gizmo.
fn mode2d_world() -> (World, Entity) {
    let mut world = World::new();
    let mut mode = Mode2d::default();
    mode.active = true;
    mode.layer = 1;
    mode.pending_frame = true;
    mode.doc = Some(TileDocument {
        path: PathBuf::from("m.tilemap.ron"),
        map: tile_map(),
        dirty: false,
        images: Vec::new(),
    });
    world.insert_resource(mode);
    world.insert_resource(ViewportLayout {
        quad: true,
        ..ViewportLayout::default()
    });
    let selected = place(&mut world, Vec3::new(0.0, 1.0, 0.0));
    select(&mut world, selected);
    (world, selected)
}

fn ground_tiles(rig: &Rig) -> Vec<u32> {
    rig.world
        .resource::<Mode2d>()
        .doc
        .as_ref()
        .and_then(|d| d.map.tile_layer(1).map(|t| t.data.clone()))
        .unwrap_or_default()
}

/// The screen position of cell `(x, y)`'s center in the front pane.
fn cell(rig: &Rig, x: u32, y: u32) -> egui::Pos2 {
    let mode = rig.world.resource::<Mode2d>();
    let (min, max) = mode.doc.as_ref().expect("map").map.tile_world_rect(x, y);
    let center = mode.origin
        + Vec3::new(
            ((min[0] + max[0]) * 0.5) as f32,
            ((min[1] + max[1]) * 0.5) as f32,
            0.0,
        );
    rig.screen(FRONT, center)
}

#[test]
fn a_press_in_the_2d_front_pane_paints_a_tile() {
    let (world, selected) = mode2d_world();
    let mut rig = Rig::new(world);
    assert_eq!(rig.layout().panes[FRONT].kind, ViewKind::Front);
    assert!(ground_tiles(&rig).iter().all(|t| *t == 0), "an empty map");
    let at = cell(&rig, 2, 1);
    rig.click(at);
    assert!(
        ground_tiles(&rig).iter().any(|t| *t != 0),
        "the click painted a tile: {:?}",
        ground_tiles(&rig)
    );
    assert_eq!(
        rig.translation(selected),
        Vec3::new(0.0, 1.0, 0.0),
        "the selection did not move"
    );
    assert_eq!(rig.selected(), [selected], "no box select or pick ran");
}

/// The 2D mode owns the front pane's primary button; the middle button still pans it.
#[test]
fn a_middle_drag_in_the_2d_front_pane_pans() {
    let (world, _) = mode2d_world();
    let mut rig = Rig::new(world);
    let camera = rig.camera(FRONT);
    let at = cell(&rig, 2, 1);
    rig.drag(
        egui::PointerButton::Middle,
        at,
        at + egui::vec2(40.0, 10.0),
        |_| {},
    );
    assert_ne!(rig.camera(FRONT), camera, "the middle drag panned the front pane");
    assert!(ground_tiles(&rig).iter().all(|t| *t == 0), "nothing painted");
}

// ---------------------------------------------------------------------------------------------
// Smart-object slot handles of the open definition (ADR-7736)
// ---------------------------------------------------------------------------------------------

/// One slot 1.5 m along the object's local -X; the object, at (0, 1, 0), is turned +90° about Y so
/// that local -X is world +Z (the handle's local and world deltas differ).
fn slot_world() -> (World, Entity) {
    use regia_ai::smart_object_def::SmartObjectDef;
    use regia_editor::smart_object_def_panel::{SmartObjectDefPanel, default_slot, open_def};
    let root = regia_test_utils::unique_temp_dir("pane-tool-slot-handles");
    let rel = "assets/bench.smartobject.ron";
    let mut def = SmartObjectDef::new();
    let mut seat = default_slot("seat");
    seat.offset.translation = (-1.5, 0.0, 0.0);
    def.slots.push(seat);
    std::fs::create_dir_all(root.join("assets")).expect("mkdir");
    std::fs::write(root.join(rel), def.to_ron().expect("ron")).expect("write");
    let mut world = World::new();
    world.insert_resource(regia_editor::agent_ext::EditorProject { root: Some(root) });
    open_def(&mut world, rel).expect("open");
    let transform = Transform {
        translation: Vec3::new(0.0, 1.0, 0.0),
        rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        ..Transform::IDENTITY
    };
    let entity = world
        .spawn((transform, GlobalTransform(transform.compute_affine())))
        .id();
    world.resource_mut::<SmartObjectDefPanel>().preview = Some(entity);
    select(&mut world, entity);
    (world, entity)
}

fn seat_offset(world: &World) -> Vec3 {
    let (x, y, z) = world
        .resource::<regia_editor::smart_object_def_panel::SmartObjectDefPanel>()
        .doc()
        .expect("open")
        .def
        .slots[0]
        .offset
        .translation;
    Vec3::new(x, y, z)
}

/// Acceptance (2): a pointer drag of the slot's handle moves the slot so it ends under the pointer,
/// its offset changes by the world delta taken into the object's local space, the drag is one undo
/// step, and the object, the camera and the selection stay.
#[test]
fn a_press_on_a_slot_handle_moves_only_that_slot_by_the_local_delta() {
    let (world, entity) = slot_world();
    let mut rig = Rig::new(world);
    let camera = rig.camera(0);
    let object = regia_editor::hierarchy_ops::world_affine(&rig.world, entity);
    let before_local = seat_offset(&rig.world);
    let before_world = object.transform_point3(before_local);
    assert!(
        (before_world - Vec3::new(0.0, 1.0, 1.5)).length() < 1e-4,
        "{before_world}"
    );
    let from = rig.screen(0, before_world);
    let to = from + egui::vec2(36.0, -18.0);
    rig.drag(egui::PointerButton::Primary, from, to, |_| {});
    let after_local = seat_offset(&rig.world);
    let after_world = object.transform_point3(after_local);
    assert!(
        (after_world - before_world).length() > 0.05,
        "the handle drag moved the slot ({before_world} -> {after_world})"
    );
    // The slot ends under the release point, and its local offset moved by the local delta.
    let landed = rig.screen(0, after_world);
    assert!((landed - to).length() < 1.5, "{landed:?} vs {to:?}");
    let local_delta = object.inverse().transform_vector3(after_world - before_world);
    assert!(((after_local - before_local) - local_delta).length() < 1e-4);
    assert_eq!(
        rig.world.resource::<UndoStack>().undo_depth(),
        1,
        "the whole drag is one undo step"
    );
    assert_eq!(
        rig.translation(entity),
        Vec3::new(0.0, 1.0, 0.0),
        "the object did not move"
    );
    assert_eq!(rig.camera(0), camera, "the camera stayed");
    assert_eq!(rig.selected(), [entity], "no box select or pick ran");
    regia_editor::command::undo_world(&mut rig.world);
    assert_eq!(seat_offset(&rig.world), before_local, "undo puts the slot back");
}

#[test]
fn a_press_beside_the_slot_handles_box_selects() {
    let (world, _) = slot_world();
    let mut rig = Rig::new(world);
    let before = seat_offset(&rig.world);
    let from = rig.pane_rect(0).left_top() + egui::vec2(30.0, 30.0);
    rig.drag(
        egui::PointerButton::Primary,
        from,
        from + egui::vec2(60.0, 40.0),
        |_| {},
    );
    assert_eq!(seat_offset(&rig.world), before, "the slot is unchanged");
    assert!(
        rig.selected().is_empty(),
        "the box over empty ground replaced the selection"
    );
}

// Terrain region and kit grid tools share the pane's press ownership (w12).
#[test]
fn a_region_drag_selects_samples_without_sculpting_or_moving_the_camera() {
    let (mut world, selected) = terrain_world();
    let mut region = regia_editor::terrain_region::TerrainRegion::default();
    region.active = true;
    world.insert_resource(region);
    let mut rig = Rig::new(world);
    let at = on_terrain(&mut rig);
    let camera = rig.camera(0);
    rig.drag(
        egui::PointerButton::Primary,
        at,
        at + egui::vec2(50.0, 20.0),
        |rig| {
            assert!(!rig.world.resource::<TerrainTool>().stroking());
            let region = rig
                .world
                .resource::<regia_editor::terrain_region::TerrainRegion>();
            assert!(region.rect[2] > 1 || region.rect[3] > 1, "drag spans samples");
        },
    );
    assert_eq!(rig.camera(0), camera);
    assert_eq!(rig.selected(), [selected]);
    assert_eq!(rig.world.resource::<UndoStack>().undo_depth(), 0);
    for e in rig.world.iter_entities() {
        if let Some(terrain) = e.get::<regia_terrain::Terrain>() {
            assert!(terrain.map.heights().iter().all(|h| *h == 0.0));
        }
    }
}

#[test]
fn kit_pointer_strokes_place_and_erase_continuous_cells_without_camera_motion() {
    use regia_editor::kit_paint::KitPaint;
    use regia_render::MeshSource;
    let root = regia_test_utils::unique_temp_dir("kit_pane_press");
    std::fs::create_dir_all(root.join("assets/kit")).expect("kit folder");
    std::fs::write(
        root.join("assets/kit/block.gltf"),
        r#"{"asset":{"version":"2.0"}}"#,
    )
    .expect("piece");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::agent_ext::install(app.world_mut());
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    let mut state = KitPaint::default();
    state.active = true;
    state.folder = "assets/kit".into();
    state.piece = "assets/kit/block.gltf".into();
    app.world_mut().insert_resource(state);
    let selected = place(app.world_mut(), Vec3::new(30.0, 0.0, 0.0));
    select(app.world_mut(), selected);
    let mut rig = Rig::new(std::mem::take(app.world_mut()));
    let camera = rig.camera(0);
    let from = rig.screen(0, Vec3::new(-2.0, 0.0, 0.0));
    let to = rig.screen(0, Vec3::new(3.0, 0.0, 0.0));
    rig.drag(egui::PointerButton::Primary, from, to, |_| {});
    let mut cells: Vec<_> = rig
        .world
        .iter_entities()
        .filter(|e| e.contains::<MeshSource>())
        .map(|e| e.get::<Transform>().expect("transform").translation.x.round() as i32)
        .collect();
    cells.sort();
    assert_eq!(cells, [-2, -1, 0, 1, 2, 3], "the drag fills every cell");
    assert_eq!(rig.world.resource::<UndoStack>().undo_depth(), 1);
    assert_eq!(rig.camera(0), camera);
    assert_eq!(rig.selected(), [selected]);
    rig.drag(egui::PointerButton::Secondary, from, to, |_| {});
    assert_eq!(
        rig.world
            .iter_entities()
            .filter(|e| e.contains::<MeshSource>())
            .count(),
        0
    );
    assert_eq!(rig.world.resource::<UndoStack>().undo_depth(), 2);
    assert_eq!(rig.camera(0), camera, "right-drag erases without orbiting");
    assert_eq!(rig.selected(), [selected]);
    let key = |key| {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .collect()
    };
    rig.repeat_pass = true;
    rig.frame(key(egui::Key::PageUp));
    rig.frame(key(egui::Key::R));
    assert_eq!(rig.world.resource::<KitPaint>().floor, 1);
    assert_eq!(rig.world.resource::<KitPaint>().rotation, 90);
    rig.frame(key(egui::Key::PageDown));
    assert_eq!(rig.world.resource::<KitPaint>().floor, 0);
    rig.frame(key(egui::Key::Enter));
    assert_eq!(
        rig.world
            .iter_entities()
            .filter(|e| e.contains::<MeshSource>())
            .count(),
        1
    );
    assert_eq!(rig.world.resource::<UndoStack>().undo_depth(), 3);
    assert_eq!(rig.camera(0), camera);
    rig.repeat_pass = false;
    {
        let mut layout = rig.world.resource_mut::<ViewportLayout>();
        layout.quad = true;
        layout.panes[1].kind = ViewKind::Perspective;
    }
    rig.frame(Vec::new());
    let focus = rig.screen(0, Vec3::ZERO);
    rig.click(focus);
    assert!(
        rig.ctx.memory(|m| m.focused().is_some()),
        "the first pane keeps keyboard focus"
    );
    let count = rig
        .world
        .iter_entities()
        .filter(|e| e.contains::<MeshSource>())
        .count();
    let depth = rig.world.resource::<UndoStack>().undo_depth();
    let from = rig.screen(1, Vec3::new(-5.0, 0.0, 0.0));
    let to = rig.screen(1, Vec3::new(-2.0, 0.0, 0.0));
    rig.drag(egui::PointerButton::Primary, from, to, |_| {});
    assert_eq!(
        rig.world
            .iter_entities()
            .filter(|e| e.contains::<MeshSource>())
            .count(),
        count + 4,
        "a hovered second perspective pane receives its press despite the first pane's focus"
    );
    assert_eq!(rig.world.resource::<UndoStack>().undo_depth(), depth + 1);
    assert_eq!(rig.camera(0), camera);
    std::fs::remove_dir_all(root).expect("cleanup");
}
