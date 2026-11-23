// SPDX-License-Identifier: MPL-2.0
//! World-tool acceptance through the registered agent surface and accessible panel.

use regia_app::prelude::*;
use regia_editor::agent_ext;

#[test]
fn terrain_region_is_registered() {
    assert!(
        agent_ext::all_methods()
            .iter()
            .any(|(s, _)| s.name == "editor.terrain_region")
    );
}

#[test]
fn kit_paint_is_registered() {
    assert!(
        agent_ext::all_methods()
            .iter()
            .any(|(s, _)| s.name == "editor.kit_paint")
    );
}

#[test]
fn terrain_panel_exposes_named_brush_sliders() {
    let mut world = World::new();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            regia_editor::terrain_panel::terrain_panel(ui, &mut world);
        });
    });
    out.textures_delta.clear();
    let update = out.platform_output.accesskit_update.expect("accessibility tree");
    let names = regia_editor::a11y_audit::widget_names(&update);
    for label in ["Size", "Strength", "Falloff", "Falloff curve"] {
        assert!(
            names.iter().any(|(_, name)| name == label),
            "missing {label}: {names:?}"
        );
    }
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

fn frame(
    ctx: &egui::Context,
    world: &mut World,
    events: Vec<egui::Event>,
    panel: fn(&mut egui::Ui, &mut World),
) {
    let mut out = ctx.run_ui(
        egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 1000.0),
            )),
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
        },
    );
    out.textures_delta.clear();
}

#[test]
fn terrain_panel_falloff_slider_has_a_keyboard_path() {
    let mut world = World::new();
    let ctx = egui::Context::default();
    frame(
        &ctx,
        &mut world,
        vec![],
        regia_editor::terrain_panel::brush_controls,
    );
    let before = world
        .resource::<regia_editor::terrain_tool::TerrainTool>()
        .brush
        .hardness;
    for _ in 0..24 {
        frame(
            &ctx,
            &mut world,
            key(egui::Key::Tab),
            regia_editor::terrain_panel::brush_controls,
        );
        frame(
            &ctx,
            &mut world,
            key(egui::Key::ArrowRight),
            regia_editor::terrain_panel::brush_controls,
        );
        if world
            .resource::<regia_editor::terrain_tool::TerrainTool>()
            .brush
            .hardness
            != before
        {
            return;
        }
    }
    panic!("Tab then Right must reach and change the Falloff slider");
}

/// Full agent dispatch, including permissions and the editor's normal undo surface.
struct Agent {
    app: App,
    inbox: crossbeam_channel::Sender<regia_agent::server::Inbound>,
    serial: u64,
}

impl Agent {
    fn new(root: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<regia_editor::command::UndoStack>();
        app.init_resource::<regia_editor::selection::Selection>();
        let open = regia_editor::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.world_mut().insert_resource(agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.heavy_rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            serial: 0,
        }
    }

    fn dispatch(&mut self, method: &str, params: serde_json::Value) -> regia_agent::protocol::Response {
        self.serial += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.inbox
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(serde_json::json!(self.serial)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("inbox");
        self.app.update();
        answer.try_recv().expect("dispatch answers in one frame")
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let response = self.dispatch(method, params);
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    }
}

#[test]
fn terrain_region_and_kit_paint_dispatch_edits_and_undo() {
    use serde_json::json;
    let root = regia_test_utils::unique_temp_dir("world_tools_agent");
    std::fs::create_dir_all(root.join("assets/kit")).expect("folder");
    std::fs::write(
        root.join("assets/kit/block.gltf"),
        r#"{"asset":{"version":"2.0"}}"#,
    )
    .expect("piece");
    let mut agent = Agent::new(&root);
    for (method, code) in [
        ("editor.terrain_region", "terrain_region_invalid"),
        ("editor.kit_paint", "kit_paint_invalid"),
    ] {
        let schema = agent.call("agent.schema", json!({"method":method}));
        let entry = &schema["methods"][0];
        assert_eq!(entry["cost"], "heavy");
        assert_eq!(entry["errors"], json!([code]));
        assert!(entry["example"].is_object());
    }
    let hf = regia_asset::Heightfield {
        width: 9,
        height: 9,
        samples: (0..81).map(|i| i * 123).collect(),
    };
    let terrain = regia_terrain::Terrain::new(
        &hf,
        8.0,
        100.0,
        0.0,
        regia_terrain::CdlodConfig::default(),
        2,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain");
    let entity = agent.app.world_mut().spawn(terrain).id();
    let before = agent
        .app
        .world()
        .get::<regia_terrain::Terrain>(entity)
        .expect("terrain")
        .map
        .heights()
        .to_vec();
    let copy = agent.call(
        "editor.terrain_region",
        json!({"op":"copy","entity":entity.to_bits(),"rect":[0,0,3,2]}),
    );
    assert_eq!(copy["width"], 3);
    let paste = agent.call(
        "editor.terrain_region",
        json!({"op":"paste","entity":entity.to_bits(),"at":[4,4],"rotation":90,"height_offset":2.0}),
    );
    assert_eq!(paste["changed"], 6);
    let depth = agent
        .app
        .world()
        .resource::<regia_editor::command::UndoStack>()
        .undo_depth();
    let error = agent
        .dispatch(
            "editor.terrain_region",
            json!({"op":"paste", "entity":entity.to_bits(), "rotation":45}),
        )
        .error
        .expect("invalid rotation");
    assert_eq!(error.data.expect("code")["code"], "terrain_region_invalid");
    assert_eq!(
        agent
            .app
            .world()
            .resource::<regia_editor::command::UndoStack>()
            .undo_depth(),
        depth
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(
        agent
            .app
            .world()
            .get::<regia_terrain::Terrain>(entity)
            .expect("terrain")
            .map
            .heights(),
        before
    );
    let exported = agent.call(
        "editor.terrain_region",
        json!({"op":"export","entity":entity.to_bits(),"rect":[0,0,3,2],"path":"assets/region.png"}),
    );
    assert_eq!(exported["width"], 3);
    let imported = agent.call(
        "editor.terrain_region",
        json!({"op":"import","entity":entity.to_bits(),"rect":[4,4,3,2],"path":"assets/region.png"}),
    );
    assert_eq!(imported["changed"], 6);
    agent.call("editor.undo", json!({}));
    assert_eq!(
        agent
            .app
            .world()
            .get::<regia_terrain::Terrain>(entity)
            .expect("terrain")
            .map
            .heights(),
        before
    );
    let paint=agent.call("editor.kit_paint",json!({"folder":"assets/kit","piece":"assets/kit/block.gltf","cells":[[0,0],[1,0],[2,0]],"floor":2,"grid":3.0,"rotation":90}));
    assert_eq!(paint["changed"], 3);
    let error = agent
        .dispatch("editor.kit_paint", json!({"cells":[[4,0]],"rotation":45}))
        .error
        .expect("invalid rotation");
    assert_eq!(error.data.expect("code")["code"], "kit_paint_invalid");
    let meshes = |world: &World| {
        world
            .iter_entities()
            .filter(|e| e.contains::<regia_render::MeshSource>())
            .count()
    };
    assert_eq!(meshes(agent.app.world()), 3);
    agent.call("editor.undo", json!({}));
    assert_eq!(meshes(agent.app.world()), 0);
    agent.call("editor.redo", json!({}));
    assert_eq!(meshes(agent.app.world()), 3);
    agent.call("editor.kit_paint", json!({"op":"erase","cells":[[1,0]]}));
    assert_eq!(meshes(agent.app.world()), 2);
    let stroke = agent.call(
        "terrain.stroke",
        json!({"entity":entity.to_bits(),"mode":"raise","radius":3.0,"center":[0.0,0.0],"falloff":"linear"}),
    );
    assert!(stroke["samples"].as_u64().is_some_and(|v| v > 0), "{stroke}");
    drop(agent);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn terrain_panel_strength_tracks_each_existing_hotkey_mode() {
    use regia_editor::terrain_tool::TerrainTool;
    let mut world = World::new();
    world.init_resource::<TerrainTool>();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    for mode in [1, 0, 9, 3] {
        let expected = {
            let mut tool = world.resource_mut::<TerrainTool>();
            tool.select_mode(mode);
            tool.scale_strength(1.25);
            if tool.paint_density {
                tool.density_target
            } else if tool.paint_scatter {
                tool.scatter_density
            } else if let regia_terrain::BrushMode::Raise { amount } = tool.brush.mode {
                amount
            } else {
                tool.brush.alpha
            }
        };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                regia_editor::terrain_panel::brush_controls(ui, &mut world)
            });
        });
        out.textures_delta.clear();
        let tree = out.platform_output.accesskit_update.expect("tree");
        let slider = tree
            .nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.role() == egui::accesskit::Role::Slider && node.label() == Some("Strength"))
            .expect("named Strength slider");
        assert_eq!(slider.numeric_value(), Some(f64::from(expected)), "mode {mode}");
    }
}

#[test]
fn terrain_panel_does_not_clamp_existing_hotkey_sizes_when_drawn() {
    use regia_editor::terrain_tool::TerrainTool;
    use regia_terrain::BrushShape;
    let ctx = egui::Context::default();
    let mut world = World::new();
    world.init_resource::<TerrainTool>();
    for shape in [
        BrushShape::Circle { radius: 5000.0 },
        BrushShape::Circle { radius: 0.02 },
        BrushShape::Rectangle {
            width: 6000.0,
            length: 0.02,
        },
    ] {
        world.resource_mut::<TerrainTool>().brush.shape = shape;
        frame(
            &ctx,
            &mut world,
            vec![],
            regia_editor::terrain_panel::brush_controls,
        );
        assert_eq!(world.resource::<TerrainTool>().brush.shape, shape);
    }
}
