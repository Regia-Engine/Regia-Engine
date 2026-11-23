// SPDX-License-Identifier: MPL-2.0
//! Blockout acceptance through the real agent dispatcher (ADR-7809).
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    let (inbox, requests) = crossbeam_channel::unbounded();
    let (reply, answer) = crossbeam_channel::bounded(1);
    app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
    inbox
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply,
        })
        .expect("inbox");
    app.update();
    let response = answer.try_recv().expect("reply within frame");
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.expect("result")
}

#[test]
fn blockout_dispatcher_add_bake_and_undo_restore_brushes() {
    let root = regia_test_utils::unique_temp_dir("blockout_dispatcher");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    regia_editor::agent_ext::install(app.world_mut());
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.insert_resource(open);
    app.finish_plugins();
    app.update();
    let added = call(
        &mut app,
        "editor.blockout_add",
        json!({"shape":"box", "op":"add", "group":"acceptance", "translation":[0,0,0]}),
    );
    assert!(added["entity"].as_u64().is_some());
    let cutter = call(
        &mut app,
        "editor.blockout_add",
        json!({"shape":"cylinder","op":"subtract","group":"acceptance","size":[1,3,1],"translation":[0,0,0]}),
    );
    call(
        &mut app,
        "editor.blockout_edit",
        json!({"entity":cutter["entity"],"material":7}),
    );
    call(
        &mut app,
        "editor.set_viewport",
        json!({"grid":false,"labels":false,"icons":false,"bounds":false,"outline":false,"proxies":false}),
    );
    // Frame this two-meter fixture closely; the default top pane frames a much larger level.
    app.world_mut()
        .resource_mut::<regia_editor::views::ViewportLayout>()
        .panes[1]
        .camera
        .distance = 3.0;
    let capture_params = json!({"width":128,"height":128,"view":"top","warm_frames":1,"game_view":true});
    let control = call(&mut app, "editor.capture_viewport", capture_params.clone());
    for _ in 0..3 {
        regia_editor::blockout::tick(app.world_mut());
    }
    regia_editor::blockout::wait_for_worker(app.world_mut());
    regia_editor::blockout::tick(app.world_mut());
    let preview = call(&mut app, "editor.capture_viewport", capture_params.clone());
    assert!(preview["info"]["meshes"].as_u64().expect("preview mesh count") > 0);
    let baked = call(
        &mut app,
        "editor.blockout_bake",
        json!({"group":"acceptance","collider":true}),
    );
    assert!(baked["job"].as_str().is_some());
    regia_editor::blockout::wait_for_worker(app.world_mut());
    regia_editor::blockout::tick(app.world_mut());
    let status = call(&mut app, "agent.job_status", json!({"id":baked["job"]}));
    assert_eq!(status["state"], "done", "{status}");
    let path = status["result"]["out"].as_str().expect("asset path");
    let bytes = std::fs::read(root.join(path)).expect("baked file");
    let loaded = regia_editor::mesh_source::load_gltf_model(
        &root.join(path),
        &std::sync::Arc::new(regia_render::CpuMaterial::default()),
    )
    .expect("loads as a renderable model");
    assert!(loaded.triangles() > 0);
    let gltf = gltf::Gltf::from_slice(&bytes).expect("GLB");
    assert_eq!(gltf.materials().count(), 2);
    let container = gltf::binary::Glb::from_slice(&bytes).expect("container");
    let document: Value = serde_json::from_slice(&container.json).expect("GLB JSON");
    assert_eq!(document["materials"][0]["extras"]["blockout_material_id"], 0);
    assert_eq!(document["materials"][1]["extras"]["blockout_material_id"], 7);
    let world = app.world_mut();
    let collider = world
        .query::<&regia_physics::Collider>()
        .iter(world)
        .next()
        .expect("baked collider");
    let regia_physics::ColliderShape::TriMesh { indices, .. } = &collider.shape else {
        panic!("bake must preserve the cavity with triangle mesh collision");
    };
    assert_eq!(indices.len(), loaded.triangles());
    assert_eq!(
        world
            .query::<&regia_editor::blockout::BlockoutBrush>()
            .iter(world)
            .count(),
        0
    );
    regia_editor::mesh_source::resolve_mesh_sources(world);
    assert!(
        world.query::<&regia_render::MeshRenderer>().iter(world).count() > 0,
        "placed mesh reaches renderer"
    );
    let rendered = call(&mut app, "editor.capture_viewport", capture_params);
    assert!(rendered["info"]["meshes"].as_u64().expect("mesh count") > 0);
    let pixels = |v: &Value| {
        let bytes =
            regia_editor::reference::base64_decode(v["png_base64"].as_str().expect("PNG")).expect("base64");
        image::load_from_memory(&bytes)
            .expect("image")
            .to_rgba8()
            .into_raw()
    };
    let changed = pixels(&control)
        .as_chunks::<4>()
        .0
        .iter()
        .zip(pixels(&rendered).as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    let preview_changed = pixels(&control)
        .as_chunks::<4>()
        .0
        .iter()
        .zip(pixels(&preview).as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        preview_changed > 1000,
        "live preview must change the control render"
    );
    assert!(
        changed > 1000,
        "loaded baked mesh must change the control render: {changed} pixels"
    );
    for capture in [&preview, &rendered] {
        let image = pixels(capture);
        let sample = |x: usize, y: usize| &image[(y * 128 + x) * 4..(y * 128 + x) * 4 + 3];
        let background = sample(8, 8);
        let opening = sample(64, 64);
        let solid = sample(44, 44);
        assert!(
            opening.iter().zip(background).all(|(a, b)| a.abs_diff(*b) <= 3),
            "the cylindrical opening must show the background"
        );
        assert!(
            solid.iter().zip(background).any(|(a, b)| a.abs_diff(*b) > 16),
            "the solid corner must differ from the background"
        );
    }
    println!(
        "blockout loaded GLB: {} triangles, {} materials, {changed} changed pixels; preview {preview_changed} changed pixels",
        loaded.triangles(),
        gltf.materials().count()
    );
    if let Some(directory) = std::env::var_os("REGIA_BLOCKOUT_EVIDENCE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("evidence directory");
        for (name, capture) in [("control", &control), ("preview", &preview), ("baked", &rendered)] {
            let png = regia_editor::reference::base64_decode(capture["png_base64"].as_str().expect("PNG"))
                .expect("base64");
            regia_test_utils::write_atomic(&directory.join(format!("{name}.png")), &png).expect("evidence");
        }
        regia_test_utils::write_atomic(&directory.join("baked.glb"), &bytes).expect("mesh evidence");
        regia_test_utils::write_atomic(
            &directory.join("job.json"),
            &serde_json::to_vec_pretty(&status).expect("job JSON"),
        )
        .expect("job evidence");
    }
    call(&mut app, "editor.undo", json!({}));
    let world = app.world_mut();
    assert_eq!(
        world
            .query::<&regia_editor::blockout::BlockoutBrush>()
            .iter(world)
            .count(),
        2
    );
    call(&mut app, "editor.redo", json!({}));
    let world = app.world_mut();
    assert_eq!(
        world
            .query::<&regia_editor::blockout::BlockoutBrush>()
            .iter(world)
            .count(),
        0
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn blockout_brushes_reopen_before_agent_install() {
    use regia_editor::blockout::{BlockoutBrush, BrushShape};
    let root = regia_test_utils::unique_temp_dir("blockout_reopen");
    let path = root.join("saved.level.ron");
    let mut source = App::new();
    source.add_plugins(regia_scene::ScenePlugin);
    regia_editor::agent_ext::install(source.world_mut());
    let open = regia_editor::level_io::new_level(source.world_mut());
    let expected = BlockoutBrush {
        shape: BrushShape::Stairs,
        group: "Saved blockout".into(),
        steps: 8,
        material: 7,
        ..Default::default()
    };
    regia_editor::blockout::add(source.world_mut(), expected.clone(), glam::Vec3::X).expect("brush");
    regia_editor::level_io::save_level_file(source.world_mut(), &open, &path).expect("save");
    // GUI startup opens the project before installing agent extensions (main.rs).
    let mut reopened = App::new();
    reopened.add_plugins(regia_scene::ScenePlugin);
    regia_editor::level_io::open_level_file(reopened.world_mut(), &path).expect("startup open");
    let world = reopened.world_mut();
    let brushes: Vec<_> = world
        .query::<(&BlockoutBrush, &regia_scene::Transform)>()
        .iter(world)
        .collect();
    assert_eq!(brushes.len(), 1);
    assert_eq!(brushes[0].0, &expected);
    assert_eq!(brushes[0].1.translation, glam::Vec3::X);
    std::fs::remove_dir_all(root).expect("cleanup");
}
