// SPDX-License-Identifier: MPL-2.0
//! Viewport overlay acceptance, through the editor agent and headless keyboard input.
use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::editor_prefs::{EditorConfigPaths, load_prefs, persist_prefs};
use serde_json::{Value, json};

fn world(root: &std::path::Path) -> World {
    let mut world = World::new();
    world.insert_resource(EditorConfigPaths::at(root));
    world.insert_resource(regia_editor::viewport::GizmoState::default());
    world.insert_resource(regia_editor::command::UndoStack::new());
    load_prefs(&mut world);
    regia_editor::agent_ext::install(&mut world);
    world
}

fn call(world: &mut World, params: Value) -> Value {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method("editor.viewport_overlays")
        .expect("editor.viewport_overlays is registered");
    handler(world, &params).expect("overlay operation succeeds")
}

/// The name a screen reader speaks for `node`: a `Label`'s value, else its label (the OS
/// adapters' rule, which `a11y_bridge::name_node` follows since ADR-8587).
fn spoken(node: &egui::accesskit::Node) -> Option<String> {
    if node.role() == egui::accesskit::Role::Label {
        node.value().map(str::to_owned)
    } else {
        node.label().map(str::to_owned)
    }
}

#[test]
fn an_overlay_dragged_and_collapsed_survives_a_restart() {
    let root = regia_test_utils::unique_temp_dir("viewport-overlays-restart");
    let mut first = world(&root);
    let changed = call(
        &mut first,
        json!({"action":"set", "pane":"perspective", "group":"transform",
        "position":[0.35,0.6], "dock":"floating", "collapsed":true}),
    );
    assert!(persist_prefs(&mut first));
    let mut second = world(&root);
    assert_eq!(call(&mut second, json!({"pane":"perspective"})), changed);
    let other = call(&mut second, json!({"pane":"top"}));
    assert_eq!(other["groups"]["transform"]["collapsed"], false);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn presets_save_and_apply() {
    let root = regia_test_utils::unique_temp_dir("viewport-overlays-presets");
    let mut world = world(&root);
    let changed = call(
        &mut world,
        json!({"action":"set", "group":"camera", "dock":"left", "vertical":true}),
    );
    call(&mut world, json!({"action":"preset_save", "name":"Camera left"}));
    call(&mut world, json!({"action":"reset"}));
    let applied = call(&mut world, json!({"action":"preset_apply", "name":"Camera left"}));
    assert_eq!(applied["groups"], changed["groups"]);
    let deleted = call(
        &mut world,
        json!({"action":"preset_delete", "name":"Camera left"}),
    );
    assert_eq!(deleted["presets"], json!([]));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_select_overlay_has_a_box_lasso_route() {
    let root = regia_test_utils::unique_temp_dir("viewport-overlays-shape");
    let mut world = world(&root);
    assert_eq!(
        call(&mut world, json!({"action":"set", "selection":"lasso"}))["selection"],
        "lasso"
    );
    assert!(regia_editor::selection::SelectionShape::lasso_on(&world));
    assert_eq!(
        call(&mut world, json!({"action":"set", "selection":"box"}))["selection"],
        "box"
    );
    assert!(!regia_editor::selection::SelectionShape::lasso_on(&world));
    let _ = std::fs::remove_dir_all(root);
}

fn key(key: egui::Key) -> Vec<egui::Event> {
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
}

#[test]
fn the_overlay_menu_is_reachable_by_keyboard() {
    let root = regia_test_utils::unique_temp_dir("viewport-overlays-keyboard");
    let mut world = world(&root);
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    let mut frame = |events| {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 900.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                regia_editor::viewport::viewport_panel(ui, &mut world, None, &mut state);
            },
        );
        output.textures_delta.clear();
        output
            .platform_output
            .accesskit_update
            .and_then(|update| {
                update
                    .nodes
                    .iter()
                    .find(|(id, _)| *id == update.focus)
                    .and_then(|(_, n)| spoken(n))
            })
            .unwrap_or_default()
    };
    frame(vec![]);
    let mut seen = Vec::new();
    for _ in 0..90 {
        let label = frame(key(egui::Key::Tab));
        let reached = label == "Overlays";
        seen.push(label);
        if reached {
            break;
        }
    }
    assert_eq!(seen.last().map(String::as_str), Some("Overlays"), "{seen:?}");
    let _ = std::fs::remove_dir_all(root);
}

struct Panel {
    ctx: egui::Context,
    state: regia_editor::viewport::ViewportPanelState,
    width: f32,
    instance: Option<regia_editor::viewport_instance::ViewportInstance>,
    nodes: Vec<(egui::accesskit::NodeId, egui::accesskit::Node)>,
    two_passes: bool,
    backgrounds: Vec<egui::Rect>,
}

impl Panel {
    fn new(width: f32, instance: bool) -> Self {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Self {
            ctx,
            state: Default::default(),
            width,
            instance: instance.then(|| regia_editor::viewport_instance::ViewportInstance::new(91)),
            nodes: Vec::new(),
            two_passes: false,
            backgrounds: Vec::new(),
        }
    }

    fn frame(&mut self, world: &mut World, events: Vec<egui::Event>) -> String {
        let mut passes = 0;
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(self.width, 900.0),
                )),
                time: Some(self.ctx.input(|i| i.time) + 1.0 / 60.0),
                events,
                ..Default::default()
            },
            |ui| {
                if let Some(instance) = &mut self.instance {
                    regia_editor::viewport::viewport_panel_with(ui, world, None, &mut self.state, instance);
                } else {
                    regia_editor::viewport::viewport_panel(ui, world, None, &mut self.state);
                }
                passes += 1;
                if self.two_passes && passes == 1 {
                    ui.ctx().request_discard("overlay two-pass test");
                }
            },
        );
        self.backgrounds.clear();
        let fill = self.ctx.global_style().visuals.window_fill();
        let mut shapes: Vec<_> = output.shapes.iter().map(|shape| &shape.shape).collect();
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(children) => shapes.extend(children),
                egui::Shape::Rect(rect) if rect.fill == fill => self.backgrounds.push(rect.rect),
                _ => {}
            }
        }
        output.textures_delta.clear();
        let update = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree");
        let focus = update.focus;
        self.nodes = update.nodes;
        self.nodes
            .iter()
            .find(|(id, _)| *id == focus)
            .and_then(|(_, node)| spoken(node))
            .unwrap_or_default()
    }

    fn tab_to(&mut self, world: &mut World, label: &str) {
        let mut seen = Vec::new();
        for _ in 0..150 {
            let focus = self.frame(world, key(egui::Key::Tab));
            if focus.strip_suffix(" ⏵").unwrap_or(&focus) == label {
                return;
            }
            seen.push(focus);
        }
        panic!("Tab did not reach {label}: {seen:?}");
    }

    fn bounds(&self, label: &str) -> egui::accesskit::Rect {
        self.nodes
            .iter()
            .find(|(_, node)| spoken(node).as_deref() == Some(label))
            .and_then(|(_, node)| node.bounds())
            .unwrap_or_else(|| panic!("No bounds for {label}"))
    }

    fn settle(&mut self, world: &mut World) {
        for _ in 0..4 {
            self.frame(world, Vec::new());
        }
    }
}

#[test]
fn a_vertical_overlay_on_the_left_edge_keeps_every_control_reachable_by_keyboard() {
    use regia_editor::editor_prefs::{EditorPreferences, PrefValue, apply_prefs};
    for scale in [1.0, 1.5] {
        let root = regia_test_utils::unique_temp_dir("overlay-vertical-keys");
        let mut world = world(&root);
        world
            .resource_mut::<EditorPreferences>()
            .set("appearance.ui_scale", &PrefValue::Float(scale))
            .expect("scale");
        let mut panel = Panel::new(900.0, false);
        apply_prefs(Some(&panel.ctx), &mut world);
        call(
            &mut world,
            json!({"action":"set", "group":"snap", "dock":"left", "vertical":true, "collapsed":true, "position":[0,0.5]}),
        );
        panel.settle(&mut world);
        panel.tab_to(&mut world, "Snap overlay");
        panel.frame(&mut world, key(egui::Key::Enter));
        panel.tab_to(&mut world, "Grid snap step");
        panel.frame(&mut world, key(egui::Key::Enter));
        panel.tab_to(&mut world, "0.25 m");
        panel.frame(&mut world, key(egui::Key::Enter));
        panel.settle(&mut world);
        assert_eq!(
            world
                .resource::<regia_editor::viewport::GizmoState>()
                .snap_distance,
            0.25
        );
        // The same collapsed menu retains the other two step controls and the snap switch.
        panel.frame(&mut world, key(egui::Key::Escape));
        panel.tab_to(&mut world, "Snap overlay");
        panel.frame(&mut world, key(egui::Key::Enter));
        for name in ["Snap", "Angle snap step", "Scale snap step"] {
            panel.tab_to(&mut world, name);
        }
        for _ in 0..3 {
            panel.frame(&mut world, key(egui::Key::Escape));
        }
        call(
            &mut world,
            json!({"action":"set", "group":"snap", "collapsed":false}),
        );
        panel.settle(&mut world);
        let controls =
            ["Snap", "Grid snap step", "Angle snap step", "Scale snap step"].map(|name| panel.bounds(name));
        for pair in controls.windows(2) {
            assert!(
                pair[0].y1 <= pair[1].y0,
                "vertical controls are stacked: {controls:?}"
            );
        }
        panel.tab_to(&mut world, "Grid snap step");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn overlay_layout_never_moves_the_pane_in_main_or_instance_views() {
    for instance in [false, true] {
        for width in [360.0, 1600.0] {
            let root = regia_test_utils::unique_temp_dir("overlay-pane-bounds");
            let mut world = world(&root);
            let mut panel = Panel::new(width, instance);
            panel.settle(&mut world);
            let before = panel.bounds("Viewport pane 1");
            for dock in ["left", "right", "top", "bottom", "floating"] {
                for group in ["view_show", "transform", "snap", "camera", "tools", "select"] {
                    call(
                        &mut world,
                        json!({"action":"set", "group":group, "dock":dock, "position":[1,1], "collapsed":true, "vertical":true}),
                    );
                }
                panel.settle(&mut world);
                assert_eq!(
                    panel.bounds("Viewport pane 1"),
                    before,
                    "{instance} {width} {dock}"
                );
                panel.tab_to(&mut world, "Overlays");
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[test]
fn the_select_overlay_works_by_keyboard_in_a_two_pass_frame() {
    let root = regia_test_utils::unique_temp_dir("overlay-selection-keys");
    let mut world = world(&root);
    let mut panel = Panel::new(1600.0, false);
    panel.two_passes = true;
    panel.settle(&mut world);
    panel.tab_to(&mut world, "Lasso");
    panel.frame(&mut world, key(egui::Key::Enter));
    assert!(regia_editor::selection::SelectionShape::lasso_on(&world));
    panel.tab_to(&mut world, "Box");
    panel.frame(&mut world, key(egui::Key::Enter));
    assert!(!regia_editor::selection::SelectionShape::lasso_on(&world));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn invalid_overlay_edits_preserve_preferences_and_selection() {
    use regia_editor::editor_prefs::{EditorPreferences, PrefValue};
    let root = regia_test_utils::unique_temp_dir("overlay-errors");
    let mut world = world(&root);
    let before = call(&mut world, json!({}));
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension.method("editor.viewport_overlays").expect("method");
    for params in [
        json!({"action":"set","group":"snap","position":[1.1,0],"selection":"lasso"}),
        json!({"action":"set","group":"snap","vertical":"yes"}),
        json!({"action":"set","group":"unknown"}),
        json!({"action":"preset_save","name":" bad "}),
        json!({"action":"get","collapsed":true}),
        json!({"action":"reset","position":[0,0]}),
    ] {
        let error = handler(&mut world, &params).expect_err("invalid");
        assert_eq!(error.data.expect("code")["code"], "viewport_overlay_invalid");
        assert_eq!(call(&mut world, json!({})), before);
    }
    world
        .resource_mut::<EditorPreferences>()
        .set_extra("viewport.overlays", Some(PrefValue::Text("invalid".to_owned())));
    assert_eq!(
        handler(&mut world, &json!({}))
            .expect_err("corrupt")
            .data
            .expect("code")["code"],
        "viewport_overlay_saved_invalid"
    );
    assert_eq!(
        world.resource::<EditorPreferences>().extra("viewport.overlays"),
        Some(&PrefValue::Text("invalid".to_owned()))
    );
    assert_eq!(
        call(&mut world, json!({"action":"reset"}))["groups"],
        before["groups"]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn pointer_drag_and_keyboard_collapse_are_saved_once_per_frame() {
    let root = regia_test_utils::unique_temp_dir("overlay-real-drag");
    let mut world = world(&root);
    call(
        &mut world,
        json!({"action":"set", "group":"transform", "dock":"floating", "position":[0.3,0.5]}),
    );
    let mut panel = Panel::new(1600.0, false);
    panel.two_passes = true;
    panel.settle(&mut world);
    let before = call(&mut world, json!({}));
    let grip = panel.bounds("Drag Transform overlay");
    let from = egui::pos2(
        ((grip.x0 + grip.x1) * 0.5) as f32,
        ((grip.y0 + grip.y1) * 0.5) as f32,
    );
    let to = from + egui::vec2(70.0, 35.0);
    panel.frame(&mut world, vec![egui::Event::PointerMoved(from)]);
    panel.frame(
        &mut world,
        vec![egui::Event::PointerButton {
            pos: from,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    panel.frame(&mut world, vec![egui::Event::PointerMoved(to)]);
    panel.frame(
        &mut world,
        vec![egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    let dragged = call(&mut world, json!({}));
    let moved = panel.bounds("Drag Transform overlay");
    assert!(
        (moved.x0 - grip.x0 - 70.0).abs() <= 1.0,
        "one horizontal drag step: {grip:?} -> {moved:?}"
    );
    assert!(
        (moved.y0 - grip.y0 - 35.0).abs() <= 1.0,
        "one vertical drag step: {grip:?} -> {moved:?}"
    );
    assert_ne!(
        dragged["groups"]["transform"]["position"],
        before["groups"]["transform"]["position"]
    );
    panel.tab_to(&mut world, "Transform overlay");
    panel.frame(&mut world, key(egui::Key::Enter));
    panel.tab_to(&mut world, "Layout");
    panel.frame(&mut world, key(egui::Key::Enter));
    panel.tab_to(&mut world, "Collapse to icons");
    panel.frame(&mut world, key(egui::Key::Enter));
    let collapsed = call(&mut world, json!({}));
    assert_eq!(collapsed["groups"]["transform"]["collapsed"], true);
    assert_eq!(
        collapsed["groups"]["transform"]["position"],
        dragged["groups"]["transform"]["position"]
    );
    assert!(persist_prefs(&mut world));
    let mut restart = self::world(&root);
    assert_eq!(call(&mut restart, json!({})), collapsed);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn overlay_presets_are_keyboard_operable() {
    let root = regia_test_utils::unique_temp_dir("overlay-preset-keys");
    let mut world = world(&root);
    call(
        &mut world,
        json!({"action":"set", "group":"select", "dock":"bottom", "collapsed":true}),
    );
    let wanted = call(&mut world, json!({}))["groups"].clone();
    let mut panel = Panel::new(1200.0, false);
    panel.two_passes = true;
    panel.settle(&mut world);
    panel.tab_to(&mut world, "Overlays");
    panel.frame(&mut world, key(egui::Key::Enter));
    panel.tab_to(&mut world, "Overlay preset name");
    panel.frame(&mut world, vec![egui::Event::Text("Keyboard preset".to_owned())]);
    panel.tab_to(&mut world, "Save overlay preset");
    panel.frame(&mut world, key(egui::Key::Enter));
    assert_eq!(call(&mut world, json!({}))["presets"], json!(["Keyboard preset"]));
    panel.tab_to(&mut world, "Reset overlay layout");
    panel.frame(&mut world, key(egui::Key::Enter));
    assert_ne!(call(&mut world, json!({}))["groups"], wanted);
    panel.tab_to(&mut world, "Keyboard preset");
    panel.frame(&mut world, key(egui::Key::Enter));
    panel.tab_to(&mut world, "Apply overlay preset");
    panel.frame(&mut world, key(egui::Key::Enter));
    assert_eq!(call(&mut world, json!({}))["groups"], wanted);
    panel.tab_to(&mut world, "Delete overlay preset");
    panel.frame(&mut world, key(egui::Key::Enter));
    assert_eq!(call(&mut world, json!({}))["presets"], json!([]));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn expanded_overlay_backgrounds_contain_their_controls() {
    let root = regia_test_utils::unique_temp_dir("overlay-background-bounds");
    let mut world = world(&root);
    let mut panel = Panel::new(1200.0, false);
    panel.settle(&mut world);
    for (group, controls) in [
        ("Transform overlay", &["Gizmo pivot"][..]),
        (
            "Snap overlay",
            &["Snap", "Grid snap step", "Angle snap step", "Scale snap step"][..],
        ),
        ("Select overlay", &["Box", "Lasso"][..]),
    ] {
        let header = panel.bounds(group);
        let point = egui::pos2(
            ((header.x0 + header.x1) * 0.5) as f32,
            ((header.y0 + header.y1) * 0.5) as f32,
        );
        let background = panel
            .backgrounds
            .iter()
            .filter(|rect| rect.contains(point))
            .min_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("overlay background");
        for name in controls {
            let bounds = panel.bounds(name);
            let control = egui::Rect::from_min_max(
                egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
                egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
            );
            assert!(
                background.expand(1.0).contains_rect(control),
                "{group}: {name} {control:?} outside {background:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(root);
}
