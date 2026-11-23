// SPDX-License-Identifier: MPL-2.0
//! Viewport camera navigation (ADR-7755, `book/agents/viewport-tools.md` "Camera navigation"):
//! invert, the speed slider and the wheel while flying, orbit around the selection, the six
//! orthographic views, zoom toward the cursor, the navigation presets, camera to selection and back,
//! per-pane clip planes and the 2- and 3-pane layouts. The viewport panel is drawn through a
//! headless egui context with synthetic pointer, wheel and key input; no GPU.

use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_editor::command::{UndoStack, undo_world};
use regia_editor::level_io;
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::viewport::{EditorCamera, ViewportPanelState};
use regia_editor::views::{ViewCamera, ViewKind, ViewportLayout};
use regia_render::{CpuMesh, LabMaterial, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

/// Calls an editor agent method by name (the dispatcher's table); the error's `code` on failure.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

fn spawn_box(world: &mut World, transform: Transform) -> Entity {
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    world
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
        ))
        .id()
}

fn select(world: &mut World, entity: Entity) {
    let mut selection = Selection::default();
    selection.select(entity, SelectMode::Add);
    world.insert_resource(selection);
}

/// A camera looking along -Z at the origin from z = 12.
fn front_camera() -> EditorCamera {
    EditorCamera {
        target: Vec3::ZERO,
        distance: 12.0,
        yaw: 0.0,
        pitch: 0.0,
        ..EditorCamera::default()
    }
}

/// Draws the viewport panel (no toolbar, no GPU) with synthetic input.
struct Driver {
    ctx: egui::Context,
    rect: egui::Rect,
    mods: egui::Modifiers,
    state: ViewportPanelState,
}

impl Driver {
    fn new() -> Self {
        let mut state = ViewportPanelState::default();
        state.toolbar = false;
        Self {
            ctx: egui::Context::default(),
            rect: egui::Rect::NOTHING,
            mods: egui::Modifiers::NONE,
            state,
        }
    }

    fn frame(&mut self, world: &mut World, mut events: Vec<egui::Event>) {
        events.insert(0, egui::Event::ModifiersChanged(self.mods));
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(self.ctx.input(|i| i.time) + 1.0 / 60.0),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        };
        let mut rect = egui::Rect::NOTHING;
        let state = &mut self.state;
        let mut output = self.ctx.run_ui(raw, |ui| {
            rect = ui.available_rect_before_wrap();
            regia_editor::viewport::viewport_panel(ui, world, None, state);
        });
        output.textures_delta.clear();
        self.rect = rect;
    }

    fn button(&self, at: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: self.mods,
        }
    }

    /// Presses `button` at `from`, moves to `to` in `steps`, releases.
    fn drag(
        &mut self,
        world: &mut World,
        button: egui::PointerButton,
        from: egui::Pos2,
        to: egui::Pos2,
        steps: u32,
    ) {
        self.frame(world, vec![egui::Event::PointerMoved(from)]);
        let press = self.button(from, button, true);
        self.frame(world, vec![press]);
        for step in 1..=steps {
            let at = from + (to - from) * (step as f32 / steps as f32);
            self.frame(world, vec![egui::Event::PointerMoved(at)]);
        }
        let release = self.button(to, button, false);
        self.frame(world, vec![release]);
        self.frame(world, Vec::new());
    }

    fn wheel(&self, delta_y: f32) -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, delta_y),
            phase: egui::TouchPhase::Move,
            modifiers: self.mods,
        }
    }

    fn key(&self, key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: self.mods,
        }
    }

    /// Runs frames until no eased move is left (bounded).
    fn settle(&mut self, world: &mut World) {
        for _ in 0..60 {
            self.frame(world, Vec::new());
        }
    }
}

fn camera(world: &World) -> EditorCamera {
    world.resource::<EditorCamera>().clone()
}

/// The world point on the plane through the target facing the view, under pane point `at`
/// (base API only: the view's ray and a plane).
fn under(view: &ViewCamera, at: Vec2, size: Vec2) -> Vec3 {
    let (origin, direction) = view.ray(at, size);
    let normal = view.forward();
    let t = (view.target - origin).dot(normal) / direction.dot(normal);
    origin + direction * t
}

// ---------------------------------------------------------------------------------------------
// (1) Invert

#[test]
fn an_inverted_orbit_drag_turns_the_camera_exactly_the_other_way() {
    let run = |invert: bool| {
        let mut app = edit_app();
        let world = app.world_mut();
        world.insert_resource(front_camera());
        if invert {
            call(
                world,
                "editor.set_viewport",
                json!({ "invert": { "orbit_x": true, "orbit_y": true } }),
            )
            .expect("set");
        }
        let mut d = Driver::new();
        d.frame(world, Vec::new());
        d.mods = egui::Modifiers::ALT;
        let c = d.rect.center();
        d.drag(
            world,
            egui::PointerButton::Primary,
            c,
            c + egui::vec2(60.0, 30.0),
            6,
        );
        camera(world)
    };
    let plain = run(false);
    let inverted = run(true);
    assert!(
        plain.yaw < -0.1 && plain.pitch > 0.05,
        "plain orbit moved: {plain:?}"
    );
    assert!(
        (inverted.yaw + plain.yaw).abs() < 1e-6,
        "{} vs {}",
        inverted.yaw,
        plain.yaw
    );
    assert!(
        (inverted.pitch + plain.pitch).abs() < 1e-6,
        "{} vs {}",
        inverted.pitch,
        plain.pitch
    );
    assert_eq!(inverted.target, plain.target);
    assert_eq!(inverted.distance, plain.distance);
}

#[test]
fn an_inverted_pan_and_wheel_go_the_other_way() {
    let run = |invert: bool| {
        let mut app = edit_app();
        let world = app.world_mut();
        world.insert_resource(front_camera());
        call(world, "editor.set_viewport", json!({ "zoom_to_cursor": false, "invert": { "pan_x": invert, "pan_y": invert, "wheel": invert } }))
            .expect("set");
        let mut d = Driver::new();
        d.frame(world, Vec::new());
        let c = d.rect.center();
        d.drag(
            world,
            egui::PointerButton::Middle,
            c,
            c + egui::vec2(40.0, -20.0),
            4,
        );
        let panned = camera(world);
        d.frame(world, vec![egui::Event::PointerMoved(c)]);
        let wheel = d.wheel(40.0);
        d.frame(world, vec![wheel]);
        d.settle(world);
        (panned, camera(world))
    };
    let (pan, zoom) = run(false);
    let (pan_inv, zoom_inv) = run(true);
    assert!(pan.target.x < -0.1, "pan moved: {:?}", pan.target);
    assert!(((pan_inv.target - Vec3::ZERO) + (pan.target - Vec3::ZERO)).length() < 1e-5);
    assert!(zoom.distance < pan.distance, "wheel up zooms in");
    assert!(zoom_inv.distance > pan_inv.distance, "inverted wheel zooms out");
}

// ---------------------------------------------------------------------------------------------
// (2) Speed

#[test]
fn the_wheel_while_flying_moves_the_slider_step_and_each_step_doubles_the_fly_distance() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let step = |world: &mut World| {
        call(world, "editor.set_viewport", json!({})).expect("read")["speed_step"].clone()
    };
    assert_eq!(step(world), json!(4));
    let mut d = Driver::new();
    d.frame(world, Vec::new());
    let c = d.rect.center();
    // Hold the secondary button (fly) and roll the wheel up until the slider step changes.
    d.frame(world, vec![egui::Event::PointerMoved(c)]);
    let press = d.button(c, egui::PointerButton::Secondary, true);
    d.frame(world, vec![press]);
    d.frame(world, vec![egui::Event::PointerMoved(c + egui::vec2(8.0, 0.0))]);
    let mut rolls = 0;
    while step(world) == json!(4) && rolls < 40 {
        let wheel = d.wheel(40.0);
        d.frame(world, vec![wheel]);
        rolls += 1;
    }
    assert_eq!(
        step(world),
        json!(5),
        "the wheel while flying moved the slider to step 5"
    );
    assert!(camera(world).speed > 6.0);
    let release = d.button(c, egui::PointerButton::Secondary, false);
    d.frame(world, vec![release]);

    // Fly the same frames at step 3 and step 4: the distance doubles.
    let fly = |speed_step: u64| {
        let mut app = edit_app();
        let world = app.world_mut();
        world.insert_resource(front_camera());
        call(world, "editor.set_viewport", json!({ "speed_step": speed_step })).expect("set");
        let mut d = Driver::new();
        d.frame(world, Vec::new());
        let c = d.rect.center();
        d.frame(world, vec![egui::Event::PointerMoved(c)]);
        let press = d.button(c, egui::PointerButton::Secondary, true);
        d.frame(world, vec![press]);
        d.frame(world, vec![egui::Event::PointerMoved(c + egui::vec2(8.0, 0.0))]);
        let start = camera(world).target;
        let w = d.key(egui::Key::W, true);
        d.frame(world, vec![w]);
        for _ in 0..30 {
            d.frame(world, Vec::new());
        }
        (camera(world).target - start).length()
    };
    let (three, four) = (fly(3), fly(4));
    assert!(three > 0.1, "flew: {three}");
    assert!(
        (four / three - 2.0).abs() < 1e-3,
        "step 4 flies twice as far as step 3: {three} {four}"
    );
}

// ---------------------------------------------------------------------------------------------
// (3) Orbit around the selection

#[test]
fn orbit_around_the_selection_keeps_its_distance_and_screen_point() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let center = Vec3::new(3.0, 1.0, -2.0);
    let entity = spawn_box(world, Transform::from_translation(center));
    select(world, entity);
    call(world, "editor.set_viewport", json!({ "orbit_selection": true })).expect("set");
    let mut d = Driver::new();
    d.frame(world, Vec::new());
    let size = Vec2::new(d.rect.width(), d.rect.height());
    let view = |world: &World| ViewCamera::of(ViewKind::Perspective, &camera(world));
    let before_px = view(world).project(center, size).expect("visible");
    let before_d = (camera(world).eye() - center).length();
    d.mods = egui::Modifiers::ALT;
    let c = d.rect.center();
    d.drag(
        world,
        egui::PointerButton::Primary,
        c,
        c + egui::vec2(50.0, 25.0),
        5,
    );
    let after = camera(world);
    assert!(after.yaw.abs() > 0.1, "it orbited: {after:?}");
    assert_ne!(after.target, Vec3::ZERO, "the target turned about the selection");
    let after_px = view(world).project(center, size).expect("visible");
    assert!(((after.eye() - center).length() - before_d).abs() < 1e-3);
    assert!(
        (after_px - before_px).length() < 0.5,
        "{before_px:?} {after_px:?}"
    );

    // With nothing selected it orbits the target.
    world.insert_resource(Selection::default());
    world.insert_resource(front_camera());
    d.drag(
        world,
        egui::PointerButton::Primary,
        c,
        c + egui::vec2(50.0, 25.0),
        5,
    );
    assert_eq!(camera(world).target, Vec3::ZERO);
}

// ---------------------------------------------------------------------------------------------
// (4) Six views and zoom toward the cursor

#[test]
fn the_wheel_keeps_the_world_point_under_the_cursor_in_perspective_and_ortho_panes() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let mut d = Driver::new();
    d.frame(world, Vec::new());
    let rect = d.rect;
    let size = Vec2::new(rect.width(), rect.height());
    let at = Vec2::new(size.x * 0.8, size.y * 0.3);
    let point = under(&ViewCamera::of(ViewKind::Perspective, &camera(world)), at, size);
    d.frame(
        world,
        vec![egui::Event::PointerMoved(rect.min + egui::vec2(at.x, at.y))],
    );
    let wheel = d.wheel(40.0);
    d.frame(world, vec![wheel]);
    d.settle(world);
    assert!(camera(world).distance < 12.0, "zoomed in");
    let now = ViewCamera::of(ViewKind::Perspective, &camera(world))
        .project(point, size)
        .expect("visible");
    assert!((now - at).length() < 1.0, "perspective: {now:?} vs {at:?}");

    // The top pane of the quad layout (pane 1, top right).
    world.get_resource_or_insert_with(ViewportLayout::default).quad = true;
    d.frame(world, Vec::new());
    let half = rect.size() * 0.5 - egui::vec2(2.0, 2.0);
    let pane = egui::Rect::from_min_size(rect.min + egui::vec2(rect.width() * 0.5, 0.0), half);
    let pane_size = Vec2::new(pane.width(), pane.height());
    let at = Vec2::new(pane_size.x * 0.25, pane_size.y * 0.7);
    let top = world.resource::<ViewportLayout>().panes[1].camera.clone();
    let point = under(&ViewCamera::of(ViewKind::Top, &top), at, pane_size);
    d.frame(
        world,
        vec![egui::Event::PointerMoved(pane.min + egui::vec2(at.x, at.y))],
    );
    let wheel = d.wheel(-40.0);
    d.frame(world, vec![wheel]);
    d.settle(world);
    let top_after = world.resource::<ViewportLayout>().panes[1].camera.clone();
    assert!(top_after.distance > top.distance, "zoomed out");
    let now = ViewCamera::of(ViewKind::Top, &top_after)
        .project(point, pane_size)
        .expect("visible");
    assert!((now - at).length() < 1.0, "ortho: {now:?} vs {at:?}");
}

#[test]
fn alt_view_keys_reach_bottom_back_and_the_opposite_side() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let mut d = Driver::new();
    d.frame(world, Vec::new());
    d.frame(world, vec![egui::Event::PointerMoved(d.rect.center())]);
    d.mods = egui::Modifiers::ALT;
    for (key, preset) in [
        (egui::Key::Num2, "bottom"),
        (egui::Key::Num3, "back"),
        (egui::Key::Num4, "opposite_side"),
    ] {
        let (down, up) = (d.key(key, true), d.key(key, false));
        d.frame(world, vec![down, up]);
        d.settle(world);
        // The camera the key left, read before `editor.view_preset` (which moves it) answers the
        // pose the key should reach.
        let got = camera(world);
        let want = call(
            world,
            "editor.view_preset",
            json!({ "preset": preset, "instant": true }),
        )
        .expect("preset")["to"]
            .clone();
        assert!(
            (f64::from(got.yaw) - want["yaw"].as_f64().expect("yaw")).abs() < 1e-5,
            "{preset}"
        );
        assert!(
            (f64::from(got.pitch) - want["pitch"].as_f64().expect("pitch")).abs() < 1e-5,
            "{preset}"
        );
    }
    // The last key was Alt+4: the opposite side looks along +X.
    let opposite = camera(world);
    assert!(
        opposite.forward().x > 0.99,
        "the opposite side looks along +X: {:?}",
        opposite.forward()
    );

    // In an orthographic pane the keys turn the pane to that view.
    world.get_resource_or_insert_with(ViewportLayout::default).quad = true;
    d.frame(world, Vec::new());
    let top_pane = d.rect.min + egui::vec2(d.rect.width() * 0.75, d.rect.height() * 0.25);
    d.frame(world, vec![egui::Event::PointerMoved(top_pane)]);
    let (down, up) = (d.key(egui::Key::Num3, true), d.key(egui::Key::Num3, false));
    d.frame(world, vec![down, up]);
    assert_eq!(
        world.resource::<ViewportLayout>().panes[1].kind,
        ViewKind::from_name("back").expect("the back view exists")
    );
    d.mods = egui::Modifiers::NONE;
    let (down, up) = (d.key(egui::Key::Num2, true), d.key(egui::Key::Num2, false));
    d.frame(world, vec![down, up]);
    assert_eq!(world.resource::<ViewportLayout>().panes[1].kind, ViewKind::Top);
}

// ---------------------------------------------------------------------------------------------
// (5) Navigation presets

#[test]
fn under_three_button_emulation_alt_shift_left_pans_exactly_like_the_middle_button() {
    let run = |preset: &str, button: egui::PointerButton, mods: egui::Modifiers| {
        let mut app = edit_app();
        let world = app.world_mut();
        world.insert_resource(front_camera());
        call(
            world,
            "editor.set_viewport",
            json!({ "navigation_preset": preset }),
        )
        .expect("set");
        let mut d = Driver::new();
        d.frame(world, Vec::new());
        d.mods = mods;
        let c = d.rect.center();
        d.drag(world, button, c, c + egui::vec2(45.0, -25.0), 5);
        camera(world)
    };
    let alt_shift = egui::Modifiers::ALT | egui::Modifiers::SHIFT;
    let middle = run(
        "regia_default",
        egui::PointerButton::Middle,
        egui::Modifiers::NONE,
    );
    let emulated = run("three_button", egui::PointerButton::Primary, alt_shift);
    assert!(middle.target.length() > 0.1, "the middle drag panned");
    assert_eq!(emulated, middle, "the emulated chord gives the same pose");
    let default = run("regia_default", egui::PointerButton::Primary, alt_shift);
    assert_ne!(default, middle, "without emulation Alt+Shift+left orbits");
    // The original chord still works under emulation.
    assert_eq!(
        run("three_button", egui::PointerButton::Middle, egui::Modifiers::NONE),
        middle
    );
}

#[test]
fn numpad_emulation_puts_the_views_on_the_number_row_by_keypad_position() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    call(
        world,
        "editor.set_viewport",
        json!({ "navigation_preset": "numpad" }),
    )
    .expect("set");
    let mut d = Driver::new();
    d.frame(world, Vec::new());
    d.frame(world, vec![egui::Event::PointerMoved(d.rect.center())]);
    for (key, preset) in [
        (egui::Key::Num8, "top"),
        (egui::Key::Num2, "bottom"),
        (egui::Key::Num6, "side"),
        (egui::Key::Num4, "opposite_side"),
        (egui::Key::Num5, "front"),
        (egui::Key::Num0, "perspective"),
    ] {
        let (down, up) = (d.key(key, true), d.key(key, false));
        d.frame(world, vec![down, up]);
        d.settle(world);
        // The camera the key left, read before `editor.view_preset` (which moves it) answers the
        // pose the key should reach.
        let got = camera(world);
        let want = call(
            world,
            "editor.view_preset",
            json!({ "preset": preset, "instant": true }),
        )
        .expect("preset")["to"]
            .clone();
        assert!(
            (f64::from(got.yaw) - want["yaw"].as_f64().expect("yaw")).abs() < 1e-5,
            "{preset}"
        );
        assert!(
            (f64::from(got.pitch) - want["pitch"].as_f64().expect("pitch")).abs() < 1e-5,
            "{preset}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// (6) Camera and object

#[test]
fn camera_to_selection_looks_through_it_and_selection_to_camera_is_one_undo_step() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let pose = Transform {
        translation: Vec3::new(2.0, 3.0, 4.0),
        rotation: Quat::from_rotation_y(0.7) * Quat::from_rotation_x(-0.3),
        scale: Vec3::new(2.0, 1.0, 3.0),
    };
    let entity = spawn_box(world, pose);
    select(world, entity);
    // The palette (and the View menu) run it on the main camera.
    let message = regia_editor::command_palette::run(world, "view.camera_to_selection").expect("command");
    assert!(message.contains("Moved"), "{message}");
    let looked = camera(world);
    assert!(
        (looked.eye() - pose.translation).length() < 1e-4,
        "{:?}",
        looked.eye()
    );
    assert!((looked.forward() - pose.rotation * Vec3::NEG_Z).length() < 1e-4);
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        0,
        "a camera move is not an edit"
    );

    // Selection to camera from somewhere else.
    world.insert_resource(front_camera());
    regia_editor::command_palette::run(world, "view.selection_to_camera").expect("command");
    let moved = *world.get::<Transform>(entity).expect("transform");
    let cam = camera(world);
    assert!((moved.translation - cam.eye()).length() < 1e-4);
    assert!((moved.rotation * Vec3::NEG_Z - cam.forward()).length() < 1e-4);
    assert_eq!(moved.scale, pose.scale, "scale kept");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one undo step");
    assert!(undo_world(world));
    assert_eq!(*world.get::<Transform>(entity).expect("transform"), pose);

    // The agent: nothing selected is an error code.
    world.insert_resource(Selection::default());
    assert_eq!(
        call(world, "editor.camera_to_selection", json!({})),
        Err("nothing_selected".to_owned())
    );
    assert_eq!(
        call(world, "editor.selection_to_camera", json!({})),
        Err("nothing_selected".to_owned())
    );
}

#[test]
fn selection_to_camera_under_a_parent_lands_in_world_space() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let parent_pose = Transform {
        translation: Vec3::new(-1.0, 2.0, 0.5),
        rotation: Quat::from_rotation_y(1.1),
        scale: Vec3::splat(2.0),
    };
    let parent = world
        .spawn((parent_pose, GlobalTransform(parent_pose.compute_affine())))
        .id();
    let child = spawn_box(world, Transform::from_translation(Vec3::X));
    world.entity_mut(child).insert(ChildOf(parent));
    select(world, child);
    let out = call(world, "editor.selection_to_camera", json!({})).expect("moved");
    assert_eq!(out["undoable"], json!(true));
    regia_scene::propagate::propagate_transforms(world);
    let global = world.get::<GlobalTransform>(child).expect("global").0;
    let (_, rotation, translation) = global.to_scale_rotation_translation();
    let cam = camera(world);
    assert!(
        (translation - cam.eye()).length() < 1e-3,
        "{translation:?} vs {:?}",
        cam.eye()
    );
    assert!((rotation * Vec3::NEG_Z - cam.forward()).length() < 1e-3);
}

// ---------------------------------------------------------------------------------------------
// (7) Per-pane field of view and clip planes

#[test]
fn clip_planes_and_field_of_view_are_per_pane_and_checked() {
    let mut app = edit_app();
    let world = app.world_mut();
    let out = call(
        world,
        "editor.set_viewport",
        json!({ "near": 0.25, "far": 1500.0, "fov": 50.0 }),
    )
    .expect("set");
    assert_eq!(
        (out["near"].clone(), out["far"].clone(), out["fov"].clone()),
        (json!(0.25), json!(1500.0), json!(50.0))
    );
    let view = ViewCamera::of(ViewKind::Perspective, &camera(world));
    assert_eq!((view.near, view.far), (0.25, 1500.0));
    assert_eq!(
        call(world, "editor.set_viewport", json!({ "near": 200.0 })),
        Err("bad_clip".to_owned())
    );
    assert_eq!(
        call(world, "editor.set_viewport", json!({ "far": 2.0 })),
        Err("bad_clip".to_owned())
    );
    assert_eq!(camera(world).far, 1500.0, "a refused call changes nothing");
    // A viewport instance has its own planes.
    call(
        world,
        "editor.viewport_instance_set",
        json!({ "id": 7, "create": true, "near": 1.0, "far": 50.0 }),
    )
    .expect("instance");
    let instances = call(world, "editor.viewport_instances", json!({})).expect("list");
    let text = instances.to_string();
    assert!(
        text.contains("\"near\":1.0") && text.contains("\"far\":50.0"),
        "{text}"
    );
    assert_eq!(
        call(
            world,
            "editor.viewport_instance_set",
            json!({ "id": 7, "near": 10.0 })
        ),
        Err("bad_clip".to_owned())
    );
    assert_eq!(camera(world).near, 0.25, "the main camera kept its planes");
}

// ---------------------------------------------------------------------------------------------
// (8) Layouts

#[test]
fn two_and_three_pane_layouts_give_each_pane_its_own_camera() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let out = call(
        world,
        "editor.viewport_layout",
        json!({ "preset": "side_by_side", "kinds": [null, "back"] }),
    )
    .expect("layout");
    assert_eq!(
        out["panes"],
        json!([{ "index": 0, "kind": "perspective" }, { "index": 1, "kind": "back" }])
    );
    let mut d = Driver::new();
    d.frame(world, Vec::new());
    let right = d.rect.min + egui::vec2(d.rect.width() * 0.75, d.rect.height() * 0.5);
    d.frame(world, vec![egui::Event::PointerMoved(right)]);
    assert_eq!(
        world.resource::<ViewportLayout>().active,
        1,
        "the right half is pane 1"
    );
    let pane_before = world.resource::<ViewportLayout>().panes[1].camera.clone();
    let main_before = camera(world);
    let wheel = d.wheel(40.0);
    d.frame(world, vec![wheel]);
    d.settle(world);
    assert!(world.resource::<ViewportLayout>().panes[1].camera.distance < pane_before.distance);
    assert_eq!(
        camera(world),
        main_before,
        "the perspective pane's camera did not move"
    );

    // One large plus two: the bottom right is pane 2.
    call(world, "editor.set_viewport", json!({ "layout": "one_large_two" })).expect("layout");
    d.frame(world, Vec::new());
    let bottom_right = d.rect.min + egui::vec2(d.rect.width() * 0.9, d.rect.height() * 0.9);
    d.frame(world, vec![egui::Event::PointerMoved(bottom_right)]);
    assert_eq!(world.resource::<ViewportLayout>().active, 2);
    let left = d.rect.min + egui::vec2(d.rect.width() * 0.3, d.rect.height() * 0.9);
    d.frame(world, vec![egui::Event::PointerMoved(left)]);
    assert_eq!(
        world.resource::<ViewportLayout>().active,
        0,
        "the large pane spans the left two thirds"
    );
    assert_eq!(
        call(world, "editor.set_viewport", json!({ "layout": "two" })).map(|_| ()),
        Err("F0002".to_owned())
    );
}

// ---------------------------------------------------------------------------------------------
// Keyboard paths (every new toolbar control has a palette command)

#[test]
fn every_new_toolbar_control_has_a_keyboard_command() {
    let mut app = edit_app();
    let world = app.world_mut();
    let run = |world: &mut World, id: &str| {
        regia_editor::command_palette::run(world, id).unwrap_or_else(|| panic!("{id}"))
    };
    assert!(run(world, "view.toggle_orbit_selection").ends_with("on"));
    assert!(run(world, "view.toggle_zoom_to_cursor").ends_with("off"));
    let read = |world: &mut World| call(world, "editor.set_viewport", json!({})).expect("read");
    let state = read(world);
    assert_eq!(
        (state["orbit_selection"].clone(), state["zoom_to_cursor"].clone()),
        (json!(true), json!(false))
    );
    run(world, "camera.speed_up");
    assert_eq!(read(world)["speed_step"], json!(5));
    run(world, "camera.speed_down");
    run(world, "camera.speed_down");
    assert_eq!(read(world)["speed_step"], json!(3));
    let mut seen = Vec::new();
    for _ in 0..5 {
        run(world, "view.cycle_layout");
        seen.push(read(world)["layout"].as_str().unwrap_or_default().to_owned());
    }
    assert_eq!(
        seen,
        ["side_by_side", "stacked", "one_large_two", "quad", "single"]
    );
}

// ---------------------------------------------------------------------------------------------
// The toolbar's Camera menu (ADR-7797, review row 6 of w12-rev-early-k)

/// Draws the viewport panel WITH its toolbar and returns the AccessKit tree, so menu items are
/// found by label and clicked at their bounds.
struct ToolbarDriver {
    ctx: egui::Context,
    state: ViewportPanelState,
}

impl ToolbarDriver {
    fn new() -> Self {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Self {
            ctx,
            state: ViewportPanelState::default(),
        }
    }

    fn frame(&mut self, world: &mut World, events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
        let raw = egui::RawInput {
            // Wide enough that the whole toolbar row is on screen.
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(4000.0, 900.0),
            )),
            time: Some(self.ctx.input(|i| i.time) + 1.0 / 60.0),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        };
        let state = &mut self.state;
        let mut output = self.ctx.run_ui(raw, |ui| {
            regia_editor::viewport::viewport_panel(ui, world, None, state);
        });
        output.textures_delta.clear();
        output.platform_output.accesskit_update.expect("accesskit is on")
    }

    /// The center and the disabled flag of the widget labeled `label`.
    fn find(update: &egui::accesskit::TreeUpdate, label: &str) -> Option<(egui::Pos2, bool)> {
        update.nodes.iter().find_map(|(_, node)| {
            (node.label() == Some(label)).then(|| {
                let b = node.bounds().expect("bounds");
                (
                    egui::pos2(((b.x0 + b.x1) * 0.5) as f32, ((b.y0 + b.y1) * 0.5) as f32),
                    node.is_disabled(),
                )
            })
        })
    }

    /// Moves to `at`, presses and releases the primary button over three frames; returns the
    /// tree after the release.
    fn click(&mut self, world: &mut World, at: egui::Pos2) -> egui::accesskit::TreeUpdate {
        self.frame(world, vec![egui::Event::PointerMoved(at)]);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.frame(world, vec![button(true)]);
        self.frame(world, vec![button(false)])
    }

    /// Opens the toolbar's Camera menu; returns the tree with the menu open.
    fn open_camera_menu(&mut self, world: &mut World) -> egui::accesskit::TreeUpdate {
        let update = self.frame(world, Vec::new());
        let (at, _) = Self::find(&update, "Camera").expect("the toolbar's Camera menu");
        self.click(world, at);
        self.frame(world, Vec::new())
    }
}

#[test]
fn the_toolbar_camera_menu_refuses_ortho_panes_and_stops_the_running_ease() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(front_camera());
    let pose = Transform {
        translation: Vec3::new(2.0, 3.0, 4.0),
        rotation: Quat::from_rotation_y(0.7),
        scale: Vec3::ONE,
    };
    let entity = spawn_box(world, pose);
    select(world, entity);

    // Quad layout with the Top pane active: both items are disabled, and a click on them does
    // nothing (the pane's stored eye pose is never shown).
    call(world, "editor.set_viewport", json!({ "layout": "quad" })).expect("quad");
    world.resource_mut::<ViewportLayout>().active = 1;
    for label in ["Move camera to selection", "Move selection to camera"] {
        let mut d = ToolbarDriver::new();
        let menu = d.open_camera_menu(world);
        let (at, disabled) = ToolbarDriver::find(&menu, label).unwrap_or_else(|| panic!("{label} shown"));
        assert!(disabled, "{label} is disabled for the Top pane");
        assert_eq!(world.resource::<ViewportLayout>().active, 1, "still the Top pane");
        let top_before = world.resource::<ViewportLayout>().panes[1].camera.clone();
        d.click(world, at);
        assert_eq!(
            world.resource::<ViewportLayout>().active,
            1,
            "the menu kept the pane"
        );
        assert_eq!(
            *world.get::<Transform>(entity).expect("transform"),
            pose,
            "{label}: the entity did not move"
        );
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            0,
            "{label}: no undo step"
        );
        assert_eq!(
            world.resource::<ViewportLayout>().panes[1].camera,
            top_before,
            "{label}: the Top pane's camera did not move"
        );
    }

    // The perspective pane: Move camera to selection goes through the shared op, so a running
    // eased move of the main camera stops and does not overwrite the result.
    call(world, "editor.set_viewport", json!({ "layout": "single" })).expect("single");
    world.resource_mut::<ViewportLayout>().active = 0;
    let mut d = ToolbarDriver::new();
    let menu = d.open_camera_menu(world);
    let (at, disabled) = ToolbarDriver::find(&menu, "Move camera to selection").expect("shown");
    assert!(!disabled, "enabled for the perspective pane");
    d.frame(world, vec![egui::Event::PointerMoved(at)]);
    let press = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    d.frame(world, vec![press(true)]);
    // An ease toward a far pose starts just before the click lands.
    let far = EditorCamera {
        target: Vec3::new(-30.0, 0.0, 0.0),
        distance: 50.0,
        ..front_camera()
    };
    let from = camera(world);
    world
        .get_resource_or_insert_with(regia_editor::viewport_ops::CameraEases::default)
        .start(regia_editor::viewport_ops::MAIN_CAMERA, from, far);
    d.frame(world, vec![press(false)]);
    for _ in 0..30 {
        d.frame(world, Vec::new());
    }
    assert!(
        world
            .get_resource::<regia_editor::viewport_ops::CameraEases>()
            .is_none_or(|e| !e.moves.contains_key(&regia_editor::viewport_ops::MAIN_CAMERA)),
        "the click stopped the main camera's ease"
    );
    let looked = camera(world);
    assert!(
        (looked.eye() - pose.translation).length() < 1e-4,
        "the camera looks through the selection: {:?}",
        looked.eye()
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
}
