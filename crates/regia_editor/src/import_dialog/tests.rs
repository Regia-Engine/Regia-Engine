// SPDX-License-Identifier: MPL-2.0
//! Tests of the import dialog (P-150, P-151; ADR-7904).

use super::*;

/// GLB: one triangle mesh `body` with material `paint`, a two-joint skin, and two takes on the
/// bone, `a` and `b`, each one second long (30 frames at the clip rate).
fn two_take_glb() -> Vec<u8> {
    let f = |values: &[f32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
    let mut bin = Vec::new();
    bin.extend(f(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0])); // 0..36
    bin.extend(f(&[0.0, 1.0])); // 36..44
    bin.extend(f(&[0.0, 0.0, 0.0, 0.0, 2.0, 0.0])); // 44..68
    bin.extend(f(&[0.0, 1.0, 0.0, 0.0, 3.0, 0.0])); // 68..92
    for i in [0u32, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes()); // 92..104
    }
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,2]}],
        "nodes":[{"name":"root","children":[1]},{"name":"bone","translation":[0.0,0.0,0.0]},
            {"name":"body","mesh":0}],
        "meshes":[{"name":"body","primitives":[{"attributes":{"POSITION":0},"indices":4,"material":0}]}],
        "materials":[{"name":"paint","pbrMetallicRoughness":{"baseColorFactor":[0.5,0.25,1.0,1.0]}}],
        "skins":[{"joints":[0,1]}],
        "animations":[
            {"name":"a","channels":[{"sampler":0,"target":{"node":1,"path":"translation"}}],
              "samplers":[{"input":1,"output":2,"interpolation":"LINEAR"}]},
            {"name":"b","channels":[{"sampler":0,"target":{"node":1,"path":"translation"}}],
              "samplers":[{"input":1,"output":3,"interpolation":"LINEAR"}]}],
        "accessors":[
            {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]},
            {"bufferView":1,"componentType":5126,"count":2,"type":"SCALAR","min":[0.0],"max":[1.0]},
            {"bufferView":2,"componentType":5126,"count":2,"type":"VEC3"},
            {"bufferView":3,"componentType":5126,"count":2,"type":"VEC3"},
            {"bufferView":4,"componentType":5125,"count":3,"type":"SCALAR"}],
        "bufferViews":[
            {"buffer":0,"byteOffset":0,"byteLength":36},
            {"buffer":0,"byteOffset":36,"byteLength":8},
            {"buffer":0,"byteOffset":44,"byteLength":24},
            {"buffer":0,"byteOffset":68,"byteLength":24},
            {"buffer":0,"byteOffset":92,"byteLength":12}],
        "buffers":[{"byteLength":104}]}"#;
    let mut json = json.as_bytes().to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut glb = Vec::new();
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

fn tiny_png() -> Vec<u8> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    bytes
}

/// An empty project plus a folder of files to import from (outside the project).
fn project(label: &str) -> (World, PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let incoming = regia_test_utils::unique_temp_dir(&format!("{label}-incoming"));
    std::fs::create_dir_all(&incoming).expect("incoming");
    std::fs::write(incoming.join("hero.glb"), two_take_glb()).expect("glb");
    std::fs::write(incoming.join("a.png"), tiny_png()).expect("png");
    std::fs::write(incoming.join("b.png"), tiny_png()).expect("png");
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    (world, root, incoming)
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let mut extension = regia_agent::extension::AgentExtension::default();
    for (schema, handler) in methods()
        .into_iter()
        .chain(crate::import_presets::methods())
        .chain(crate::import_settings_panel::methods())
        .chain(crate::asset_ops_agent::methods())
    {
        extension.register(schema, handler);
    }
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn reason(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => code_of(&error),
    }
}

fn meta(root: &Path, rel: &str) -> regia_asset::MetaSidecar {
    let bytes = std::fs::read(root.join(format!("{rel}.meta"))).expect("meta");
    regia_asset::MetaSidecar::from_ron(&bytes).expect("parse meta")
}

fn sub_paths(meta: &regia_asset::MetaSidecar) -> Vec<String> {
    meta.sub_assets.iter().map(|s| s.path.clone()).collect()
}

#[test]
fn the_scene_tree_lists_meshes_materials_and_takes() {
    let scene = read_scene(&two_take_glb()).expect("scene");
    assert_eq!(
        scene.meshes,
        [MeshItem {
            name: "body".into(),
            primitives: 1,
            triangles: 1,
            key: "body".into(),
            import: true,
        }]
    );
    assert_eq!(scene.materials.len(), 1);
    assert_eq!(scene.materials[0].name, "paint");
    assert_eq!(scene.materials[0].base_color, [0.5, 0.25, 1.0, 1.0]);
    assert_eq!(scene.materials[0].key, "paint");
    assert_eq!(scene.materials[0].mode, MaterialImportMode::Embedded);
    let takes: Vec<(&str, f32)> = scene
        .clips
        .iter()
        .map(|c| (c.animation.as_str(), c.duration))
        .collect();
    assert_eq!(takes, [("a", 1.0), ("b", 1.0)]);
    assert!(scene.clips.iter().all(ClipItem::is_default));
}

/// P-151 acceptance: importing a model with clip options writes its `.meta` before the copy, so
/// the import's one cook already splits take `a` into two clips and skips `b`. Before ADR-7904
/// there was no import dialog or `editor.import_with`, and the cook ignored clip ranges.
#[test]
fn import_with_splits_a_take_and_skips_another_in_the_first_cook() {
    let (mut world, root, incoming) = project("import-dialog-split");
    let out = call(
        &mut world,
        "editor.import_with",
        json!({
            "paths": [incoming.join("hero.glb").to_string_lossy()],
            "folder": "models",
            "items": [
                { "kind": "clip", "animation": "a", "ranges": [
                    { "name": "rise", "start_frame": 0, "end_frame": 15 },
                    { "name": "top", "start_frame": 15, "end_frame": 30 }
                ] },
                { "kind": "clip", "animation": "b", "import": false }
            ]
        }),
    )
    .expect("import_with");
    assert_eq!(out["imported"], json!(["assets/models/hero.glb"]), "{out}");
    assert_eq!(out["errors"], json!([]), "{out}");
    let meta = meta(&root, "assets/models/hero.glb");
    let settings = meta.settings.clone().expect("settings");
    assert!(settings.contains("clips"), "{settings}");
    let subs = sub_paths(&meta);
    for wanted in ["clip/rise", "clip/top"] {
        assert!(subs.iter().any(|p| p == wanted), "{wanted} in {subs:?}");
    }
    for unwanted in ["clip/a", "clip/b"] {
        assert!(!subs.iter().any(|p| p == unwanted), "{unwanted} in {subs:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// P-150: a multi-file import shows the dialog once; Apply to all gives every file of the shown
/// file's importer its settings, and the model keeps its own defaults.
#[test]
fn apply_to_all_sets_every_file_of_the_importer() {
    let (mut world, root, incoming) = project("import-dialog-all");
    let paths: Vec<String> = ["a.png", "b.png", "hero.glb"]
        .iter()
        .map(|n| incoming.join(n).to_string_lossy().into_owned())
        .collect();
    let state = call(&mut world, "editor.import_dialog_open", json!({ "paths": paths })).expect("open");
    assert_eq!(state["mode"], "import");
    assert_eq!(state["files"].as_array().map(Vec::len), Some(3));
    assert_eq!(state["files"][2]["scene"]["clips"][0]["frames"], 30);
    world.resource_mut::<ImportDialog>().files[0]
        .form
        .edit("max_size", &json!(64))
        .expect("edit");
    let out = call(&mut world, "editor.import_with", json!({ "apply_to_all": true })).expect("import");
    assert_eq!(out["imported"].as_array().map(Vec::len), Some(3), "{out}");
    for rel in ["assets/a.png", "assets/b.png"] {
        assert_eq!(
            meta(&root, rel).settings.as_deref(),
            Some("(max_size: Some(64))"),
            "{rel}"
        );
    }
    assert_eq!(
        meta(&root, "assets/hero.glb").settings,
        None,
        "another importer keeps its defaults"
    );
    let state = call(&mut world, "editor.import_dialog_state", json!({})).expect("state");
    assert_eq!(state["open"], false, "the dialog closes after its import");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// Re-import with options: the dialog reads the `.meta`'s clip options back, and a rename writes
/// the `.meta` and recooks once.
#[test]
fn reimport_with_options_renames_a_clip() {
    let (mut world, root, incoming) = project("import-dialog-reimport");
    crate::content::import_files(&root, "", &[incoming.join("hero.glb")], Some("test")).expect("import");
    let out = call(
        &mut world,
        "editor.import_with",
        json!({ "reimport": "assets/hero.glb", "items": [{ "animation": "b", "name": "jump" }] }),
    )
    .expect("reimport");
    assert!(out["cooked"].as_u64().is_some_and(|c| c >= 1), "{out}");
    let subs = sub_paths(&meta(&root, "assets/hero.glb"));
    assert!(subs.iter().any(|p| p == "clip/jump"), "{subs:?}");
    assert!(subs.iter().any(|p| p == "clip/a"), "{subs:?}");
    let state = call(
        &mut world,
        "editor.import_dialog_open",
        json!({ "reimport": "assets/hero.glb" }),
    )
    .expect("open");
    assert_eq!(state["mode"], "reimport");
    assert_eq!(state["files"][0]["scene"]["clips"][1]["name"], "jump", "{state}");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

#[test]
fn refusals_carry_their_codes() {
    let (mut world, root, incoming) = project("import-dialog-refusals");
    let glb = incoming.join("hero.glb").to_string_lossy().into_owned();
    let with = |world: &mut World, items: Value| {
        reason(call(
            world,
            "editor.import_with",
            json!({ "paths": [glb], "items": items }),
        ))
    };
    // Mesh and material items take options since ADR-8376; another kind is still refused.
    assert_eq!(
        with(&mut world, json!([{ "kind": "light", "animation": "a" }])),
        "import_item_unsupported"
    );
    assert_eq!(
        with(
            &mut world,
            json!([{ "kind": "mesh", "mesh": "nope", "import": false }])
        ),
        "import_item_unknown"
    );
    assert_eq!(
        with(&mut world, json!([{ "animation": "nope" }])),
        "import_item_unknown"
    );
    assert_eq!(
        with(
            &mut world,
            json!([{ "animation": "a", "ranges": [{ "name": "r", "start": 0.5, "end": 0.25 }] }])
        ),
        "asset_settings_invalid"
    );
    assert_eq!(
        reason(call(&mut world, "editor.import_with", json!({}))),
        "import_dialog_not_open"
    );
    assert_eq!(
        reason(call(
            &mut world,
            "editor.import_with",
            json!({ "paths": [glb], "skip": ["hero.glb"] })
        )),
        "import_dialog_nothing"
    );
    // Nothing above wrote into the project.
    assert!(!root.join("assets/hero.glb").exists());
    assert!(!root.join("assets/hero.glb.meta").exists());
    call(&mut world, "editor.import_with", json!({ "paths": [glb] })).expect("first import");
    assert_eq!(
        reason(call(&mut world, "editor.import_with", json!({ "paths": [glb] }))),
        "import_destination_exists"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

#[test]
fn the_dialog_opens_for_several_files_or_a_model() {
    assert!(wants_dialog(&[PathBuf::from("a.FBX")]));
    assert!(wants_dialog(&[PathBuf::from("a.obj")]));
    assert!(wants_dialog(&[PathBuf::from("a.png"), PathBuf::from("b.png")]));
    assert!(!wants_dialog(&[PathBuf::from("a.png")]));
    assert!(!wants_dialog(&[]));
}

#[test]
fn clip_settings_text_is_compact_and_parses_back() {
    let clips = vec![
        ClipImportSettings {
            animation: "Take \"1\"".into(),
            skip: false,
            name: Some("run".into()),
            ranges: vec![ClipRangeSettings {
                name: "start".into(),
                start: 0.0,
                end: 0.5,
            }],
        },
        ClipImportSettings {
            animation: "idle".into(),
            skip: true,
            name: None,
            ranges: Vec::new(),
        },
    ];
    let text = clips_ron(&clips);
    assert_eq!(
        text,
        "[(animation: \"Take \\\"1\\\"\", name: Some(\"run\"), ranges: [(name: \"start\", start: 0.0, end: 0.5)]), (animation: \"idle\", skip: true)]"
    );
    let back: Vec<ClipImportSettings> = ron::from_str(&text).expect("parse");
    assert_eq!(back, clips);
}

/// The keyboard path (accessibility bar): Tab reaches the dialog's Import button (named by its
/// label) and Enter presses it (the import itself runs after drawing, in the window function).
#[test]
fn the_dialog_imports_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let (_world, root, incoming) = project("import-dialog-keys");
    let mut dialog = ImportDialog::default();
    dialog
        .open_files(&root, "", &[incoming.join("a.png"), incoming.join("b.png")])
        .expect("open");
    let library = crate::mesh_source::MeshLibrary::default();
    let ctx = egui::Context::default();
    let size = [900.0, 1400.0];
    {
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| import_dialog_ui(ui, &mut dialog, None, &library));
        };
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "Import", false, 80, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
    }
    assert_eq!(dialog.requested, Some(false), "Enter on Import asks to import");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// Dispatcher end to end (specs/23): `editor.import_dialog_open`, `editor.import_presets` and
/// `editor.import_with` answer through the agent inbox like any client call, and the import
/// lands with its clip split.
#[test]
fn the_dialog_methods_answer_through_the_dispatcher() {
    let (_world, root, incoming) = project("import-dialog-dispatch");
    let mut app = crate::engine::app::App::new();
    app.add_plugins(crate::engine::app::MinimalPlugins);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    crate::agent_ext::install(app.world_mut());
    app.world_mut().insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    app.finish_plugins();
    let (sender, inbox) = crossbeam_channel::unbounded();
    app.insert_resource(regia_agent::AgentInbox(inbox));
    let mut call = |method: &str, params: Value| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        app.update();
        let response = answer.try_recv().expect("dispatch answers in one frame");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    };
    let glb = incoming.join("hero.glb").to_string_lossy().into_owned();
    let state = call("editor.import_dialog_open", json!({ "paths": [glb] }));
    assert_eq!(state["files"][0]["scene"]["meshes"][0]["name"], "body");
    let presets = call(
        "editor.import_presets",
        json!({ "action": "save", "name": "Big", "importer": "gltf", "settings": "(scale: 2.0)" }),
    );
    assert_eq!(presets["saved"], "Big");
    let out = call(
        "editor.import_with",
        json!({ "preset": "Big", "items": [{ "animation": "a", "ranges": [{ "name": "half", "start": 0.0, "end": 0.5 }] }] }),
    );
    assert_eq!(out["imported"], json!(["assets/hero.glb"]), "{out}");
    let meta = meta(&root, "assets/hero.glb");
    let settings = meta.settings.clone().unwrap_or_default();
    assert!(settings.starts_with("(scale: 2.0"), "{settings}");
    assert!(
        sub_paths(&meta).iter().any(|p| p == "clip/half"),
        "{:?}",
        sub_paths(&meta)
    );
    let state = call("editor.import_dialog_state", json!({}));
    assert_eq!(state["open"], false);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// Review g18 row 6: a prewritten `.meta` whose destination never arrived (a failed or cancelled
/// copy, or a worker that never started) is removed; one whose file is there stays.
#[test]
fn orphan_prewritten_metas_are_removed_and_imported_ones_kept() {
    let dir = regia_test_utils::unique_temp_dir("import-dialog-orphan-metas");
    std::fs::create_dir_all(&dir).expect("dir");
    let arrived = dir.join("arrived.png");
    let missing = dir.join("missing.png");
    std::fs::write(&arrived, b"png").expect("asset");
    let pairs: Vec<(PathBuf, PathBuf)> = [&arrived, &missing]
        .into_iter()
        .map(|dest| (dest.clone(), regia_asset::meta_path_for(dest)))
        .collect();
    for (_, meta) in &pairs {
        std::fs::write(meta, b"(meta)").expect("meta");
    }
    remove_orphan_metas(&pairs);
    assert!(pairs[0].1.is_file(), "the arrived file keeps its meta");
    assert!(!pairs[1].1.exists(), "the missing file's meta is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review g18 row 5: egui runs a frame's UI twice when a pass asks for a discard. Space on Skip
/// file flips the file once across both passes (the review read the second pass as seeing the
/// same key; 3243-ff's tests found egui 0.36 gives events to the first pass, so the guard is
/// defensive and this test pins the result either way).
#[test]
fn a_second_pass_of_the_frame_toggles_skip_once() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let (_world, root, incoming) = project("import-dialog-second-pass");
    let mut dialog = ImportDialog::default();
    dialog
        .open_files(&root, "", &[incoming.join("a.png"), incoming.join("b.png")])
        .expect("open");
    let library = crate::mesh_source::MeshLibrary::default();
    let ctx = egui::Context::default();
    let size = [900.0, 1400.0];
    let mut passes = 0;
    {
        let discard_first = std::cell::Cell::new(false);
        let mut draw = |ui: &mut egui::Ui| {
            if discard_first.get() {
                passes += 1;
                if ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: a second pass of the frame");
                }
            }
            egui::CentralPanel::default().show(ui, |ui| import_dialog_ui(ui, &mut dialog, None, &library));
        };
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "Skip file", false, 80, &mut draw);
        discard_first.set(true);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            &mut draw,
        );
    }
    assert_eq!(passes, 2, "the key frame ran twice");
    assert!(dialog.files[dialog.current].skipped, "one Space, one flip");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

// ---------------------------------------------------------------------------------------------
// Per-mesh and per-material options (P-151, ADR-8376)
// ---------------------------------------------------------------------------------------------

/// GLB: two mesh nodes over one triangle, `body` (material `paint`, a red base color) and `hat`
/// (material `trim`).
fn two_mesh_glb() -> Vec<u8> {
    let mut bin: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    for i in [0u32, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,1]}],
        "nodes":[{"name":"body","mesh":0},{"name":"hat","mesh":1,"translation":[0.0,2.0,0.0]}],
        "meshes":[{"name":"m0","primitives":[{"attributes":{"POSITION":0},"indices":1,"material":0}]},
            {"name":"m1","primitives":[{"attributes":{"POSITION":0},"indices":1,"material":1}]}],
        "materials":[{"name":"paint","pbrMetallicRoughness":{"baseColorFactor":[1.0,0.0,0.0,1.0]}},{"name":"trim"}],
        "accessors":[
            {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]},
            {"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}],
        "bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12}],
        "buffers":[{"byteLength":48}]}"#;
    let mut json = json.as_bytes().to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut glb = Vec::new();
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

/// [`project`] plus `incoming/robot.glb` (two meshes, two materials).
fn parts_project(label: &str) -> (World, PathBuf, PathBuf) {
    let (world, root, incoming) = project(label);
    std::fs::write(incoming.join("robot.glb"), two_mesh_glb()).expect("glb");
    (world, root, incoming)
}

/// A project material asset at `rel` with a `.meta` (as an import of it leaves it). Answers its id.
fn material_asset(root: &Path, rel: &str) -> regia_asset::AssetId {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    let ron = regia_asset::MaterialAssetRon::default_standard()
        .to_ron_string()
        .expect("material ron");
    std::fs::write(&path, ron).expect("material");
    let id = regia_asset::AssetId::new_random();
    let meta = regia_asset::MetaSidecar::new("material_ron", id)
        .to_ron_string()
        .expect("meta");
    std::fs::write(regia_asset::meta_path_for(&path), meta).expect("meta");
    id
}

/// The cooked `#model` of `rel` (the material of each part, in part order) from the project cache.
fn cooked_model(root: &Path, rel: &str) -> regia_asset::CookedModel {
    let cache = regia_asset::cooked_cache_dir(root, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    let id = manifest.id_for_path(&format!("{rel}#model")).expect("model id");
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked");
    let payload = regia_asset::decode_cooked_file(&bytes)
        .expect("decodes")
        .1
        .to_vec();
    regia_asset::CookedModel::from_postcard(&payload).expect("model")
}

/// P-151 acceptance (ADR-8376): a mesh the dialog skips is not cooked: the model `.meta` lists no
/// sub-asset for it, the package manifest has no row, and the cooked model has only the other
/// mesh's part. Before ADR-8376 a mesh item was refused (`import_item_unsupported`) and every
/// node was cooked.
#[test]
fn a_skipped_mesh_is_not_cooked() {
    let (mut world, root, incoming) = parts_project("import-dialog-skip-mesh");
    let state = call(
        &mut world,
        "editor.import_dialog_open",
        json!({ "paths": [incoming.join("robot.glb").to_string_lossy()], "folder": "models" }),
    )
    .expect("open");
    let meshes = &state["files"][0]["scene"]["meshes"];
    assert_eq!(meshes[0]["key"], "body", "{state}");
    assert_eq!(meshes[1]["key"], "hat", "{state}");
    assert_eq!(state["files"][0]["part_options"], true);
    let out = call(
        &mut world,
        "editor.import_with",
        json!({ "items": [{ "kind": "mesh", "mesh": "body", "import": false }] }),
    )
    .expect("import");
    assert_eq!(out["errors"], json!([]), "{out}");
    let meta = meta(&root, "assets/models/robot.glb");
    assert!(
        meta.settings
            .as_deref()
            .is_some_and(|s| s.contains("meshes: [(mesh: \"body\", skip: true)]")),
        "{:?}",
        meta.settings
    );
    let subs = sub_paths(&meta);
    assert!(!subs.iter().any(|p| p.starts_with("mesh/body/")), "{subs:?}");
    assert!(subs.iter().any(|p| p == "mesh/hat/primitive_0"), "{subs:?}");
    let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    assert!(
        manifest
            .id_for_path("assets/models/robot.glb#mesh/body/primitive_0")
            .is_none()
    );
    assert!(
        manifest
            .id_for_path("assets/models/robot.glb#mesh/hat/primitive_0")
            .is_some()
    );
    assert_eq!(cooked_model(&root, "assets/models/robot.glb").parts.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// P-151 acceptance (ADR-8376): Extract writes the embedded material as a `.material.ron` (with a
/// `.meta` holding the id the settings name) next to the model, and the cooked model draws the
/// parts that used it with that asset; the other material stays embedded.
#[test]
fn an_extracted_material_is_written_and_referenced() {
    let (mut world, root, incoming) = parts_project("import-dialog-extract");
    let out = call(
        &mut world,
        "editor.import_with",
        json!({
            "paths": [incoming.join("robot.glb").to_string_lossy()],
            "folder": "models",
            "items": [{ "kind": "material", "material": "paint", "mode": "extract" }]
        }),
    )
    .expect("import");
    assert_eq!(out["errors"], json!([]), "{out}");
    let written = root.join("assets/models/robot_paint.material.ron");
    let text = std::fs::read_to_string(&written).expect("the extracted material is written");
    let material = regia_asset::MaterialAssetRon::from_ron(text.as_bytes()).expect("it loads");
    assert_eq!(material.base.base_color, [1.0, 0.0, 0.0, 1.0], "{text}");
    let extracted = meta(&root, "assets/models/robot_paint.material.ron");
    let model_meta = meta(&root, "assets/models/robot.glb");
    let settings = model_meta.settings.clone().expect("settings");
    assert!(settings.contains("Extracted("), "{settings}");
    assert!(settings.contains(&extracted.id.to_string()), "{settings}");
    let parts = cooked_model(&root, "assets/models/robot.glb").parts;
    assert_eq!(
        parts[0].material,
        Some(extracted.id),
        "body draws with the extracted asset"
    );
    let trim = model_meta
        .sub_assets
        .iter()
        .find(|s| s.path == "material/trim")
        .map(|s| s.id);
    assert_eq!(parts[1].material, trim, "hat keeps its embedded material");
    // The project cook cooked the extracted asset too, so the reference resolves.
    let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
    assert!(cache.join(regia_asset::cooked_file_name(extracted.id)).is_file());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// P-151 acceptance (ADR-8376): Replace binds the parts to the chosen project material; the
/// dialog lists the project's materials for the picker.
#[test]
fn a_replaced_material_points_at_the_chosen_asset() {
    let (mut world, root, incoming) = parts_project("import-dialog-replace");
    material_asset(&root, "assets/materials/green.material.ron");
    let blue = material_asset(&root, "assets/materials/blue.material.ron");
    let state = call(
        &mut world,
        "editor.import_dialog_open",
        json!({ "paths": [incoming.join("robot.glb").to_string_lossy()] }),
    )
    .expect("open");
    let listed: Vec<&str> = state["material_assets"]
        .as_array()
        .expect("material_assets")
        .iter()
        .filter_map(|m| m["path"].as_str())
        .collect();
    assert_eq!(
        listed,
        [
            "assets/materials/blue.material.ron",
            "assets/materials/green.material.ron"
        ]
    );
    let out = call(
        &mut world,
        "editor.import_with",
        json!({ "items": [{ "kind": "material", "material": "trim", "mode": "replace",
            "path": "assets/materials/blue.material.ron" }] }),
    )
    .expect("import");
    assert_eq!(out["errors"], json!([]), "{out}");
    let parts = cooked_model(&root, "assets/robot.glb").parts;
    assert_eq!(parts[1].material, Some(blue), "hat draws with the chosen asset");
    let model_meta = meta(&root, "assets/robot.glb");
    let paint = model_meta
        .sub_assets
        .iter()
        .find(|s| s.path == "material/paint")
        .map(|s| s.id);
    assert_eq!(parts[0].material, paint);
    assert!(
        model_meta.sub_assets.iter().any(|s| s.path == "material/trim"),
        "the embedded material is still cooked (its id stays valid)"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// P-151 acceptance (ADR-8376): Re-import with options shows the skip, the extraction and the
/// replacement the file was imported with, and a re-import that changes something else keeps them
/// (the mesh stays uncooked, the extracted file is not rewritten, the binding stays).
#[test]
fn choices_survive_a_reimport() {
    let (mut world, root, incoming) = parts_project("import-dialog-reimport-parts");
    let blue = material_asset(&root, "assets/materials/blue.material.ron");
    call(
        &mut world,
        "editor.import_with",
        json!({
            "paths": [incoming.join("robot.glb").to_string_lossy()],
            "items": [
                { "kind": "mesh", "mesh": "hat", "import": false },
                { "kind": "material", "material": "paint", "mode": "extract" },
                { "kind": "material", "material": "trim", "mode": "replace",
                  "path": "assets/materials/blue.material.ron" }
            ]
        }),
    )
    .expect("import");
    let extracted_path = root.join("assets/robot_paint.material.ron");
    // An edit made to the extracted material in the project survives the re-import.
    let edited = regia_asset::MaterialAssetRon::default_standard()
        .to_ron_string()
        .expect("ron");
    std::fs::write(&extracted_path, &edited).expect("edit");
    let extracted = meta(&root, "assets/robot_paint.material.ron").id;
    let state = call(
        &mut world,
        "editor.import_dialog_open",
        json!({ "reimport": "assets/robot.glb" }),
    )
    .expect("open");
    let scene = &state["files"][0]["scene"];
    assert_eq!(scene["meshes"][1]["import"], false, "{state}");
    assert_eq!(scene["materials"][0]["mode"], "extract", "{state}");
    assert_eq!(scene["materials"][0]["asset"], extracted.to_string(), "{state}");
    assert_eq!(scene["materials"][1]["mode"], "replace", "{state}");
    assert_eq!(
        scene["materials"][1]["path"],
        "assets/materials/blue.material.ron"
    );
    // Re-import with another change (a scale) through the open dialog.
    let out = call(
        &mut world,
        "editor.import_with",
        json!({ "settings": "(scale: 2.0)" }),
    )
    .expect("reimport");
    assert!(out["cooked"].as_u64().is_some_and(|c| c >= 1), "{out}");
    let model_meta = meta(&root, "assets/robot.glb");
    let settings = model_meta.settings.clone().expect("settings");
    assert!(settings.contains("scale: 2.0"), "{settings}");
    assert!(settings.contains("meshes:"), "{settings}");
    assert!(settings.contains("materials:"), "{settings}");
    assert!(
        !sub_paths(&model_meta).iter().any(|p| p.starts_with("mesh/hat/")),
        "{:?}",
        sub_paths(&model_meta)
    );
    assert_eq!(
        std::fs::read_to_string(&extracted_path).expect("extracted"),
        edited,
        "the extracted file is kept as edited"
    );
    let parts = cooked_model(&root, "assets/robot.glb").parts;
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].material, Some(extracted));
    let state = call(
        &mut world,
        "editor.import_dialog_open",
        json!({ "reimport": "assets/robot.glb" }),
    )
    .expect("open again");
    assert_eq!(
        state["files"][0]["scene"]["materials"][1]["asset"],
        blue.to_string()
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// ADR-8376 refusals: a replacement that is not a project material, an extract path outside
/// `assets/`, skipping every mesh, and mesh options on a file that does not take them.
#[test]
fn part_option_refusals_carry_their_codes() {
    let (mut world, root, incoming) = parts_project("import-dialog-part-refusals");
    let glb = incoming.join("robot.glb").to_string_lossy().into_owned();
    let with = |world: &mut World, items: Value| {
        reason(call(
            world,
            "editor.import_with",
            json!({ "paths": [glb], "items": items }),
        ))
    };
    assert_eq!(
        with(
            &mut world,
            json!([{ "kind": "material", "material": "paint", "mode": "replace", "path": "assets/none.material.ron" }])
        ),
        "import_material_not_found"
    );
    assert_eq!(
        with(
            &mut world,
            json!([{ "kind": "material", "material": "nope", "mode": "extract" }])
        ),
        "import_item_unknown"
    );
    assert_ne!(
        with(
            &mut world,
            json!([{ "kind": "material", "material": "paint", "mode": "extract", "path": "../x.material.ron" }])
        ),
        "",
        "an extract path outside assets/ is refused"
    );
    assert_eq!(
        with(
            &mut world,
            json!([{ "kind": "mesh", "mesh": "body", "import": false }, { "kind": "mesh", "mesh": "hat", "import": false }])
        ),
        "asset_settings_invalid"
    );
    let png = incoming.join("a.png").to_string_lossy().into_owned();
    assert_eq!(
        reason(call(
            &mut world,
            "editor.import_with",
            json!({ "paths": [png], "items": [{ "kind": "mesh", "mesh": "body" }] }),
        )),
        "import_item_unsupported"
    );
    assert!(!root.join("assets/robot.glb").exists(), "nothing was imported");
    assert!(!root.join("assets/robot_paint.material.ron").exists());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// The keyboard path of the new rows (accessibility bar): Tab reaches a mesh's Import toggle and
/// Space skips the mesh; Tab reaches Extract, then Replace and a project material, and Space picks
/// each.
#[test]
fn mesh_and_material_rows_work_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let (_world, root, incoming) = parts_project("import-dialog-parts-keys");
    material_asset(&root, "assets/materials/blue.material.ron");
    let mut dialog = ImportDialog::default();
    dialog
        .open_files(&root, "", &[incoming.join("robot.glb")])
        .expect("open");
    let library = crate::mesh_source::MeshLibrary::default();
    let ctx = egui::Context::default();
    let size = [900.0, 2400.0];
    let space = || vec![key(egui::Key::Space, egui::Modifiers::NONE)];
    {
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| import_dialog_ui(ui, &mut dialog, None, &library));
        };
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "Import mesh", false, 80, &mut draw);
        frame(&ctx, size, space(), &mut draw);
        tab_to(&ctx, size, "Extract", false, 80, &mut draw);
        frame(&ctx, size, space(), &mut draw);
        frame(&ctx, size, vec![], &mut draw);
    }
    let scene = &dialog.files[0].scene;
    assert!(!scene.meshes[0].import, "Space on Import mesh skips body");
    assert!(scene.meshes[1].import);
    assert!(
        matches!(scene.materials[0].mode, MaterialImportMode::Extracted { .. }),
        "{:?}",
        scene.materials[0].mode
    );
    {
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| import_dialog_ui(ui, &mut dialog, None, &library));
        };
        tab_to(&ctx, size, "Replace", false, 80, &mut draw);
        frame(&ctx, size, space(), &mut draw);
        frame(&ctx, size, vec![], &mut draw);
    }
    assert!(
        matches!(
            &dialog.files[0].scene.materials[0].mode,
            MaterialImportMode::Replaced { path, .. } if path == "assets/materials/blue.material.ron"
        ),
        "{:?}",
        dialog.files[0].scene.materials[0].mode
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&incoming);
}

/// ADR-8376: mesh and material options write compact RON the importer reads back.
#[test]
fn part_settings_text_parses_back() {
    let id = regia_asset::AssetId::new_random();
    let meshes = vec![MeshImportSettings {
        mesh: "body \"1\"".into(),
        skip: true,
    }];
    let materials = vec![
        MaterialImportSettings {
            material: "paint".into(),
            mode: MaterialImportMode::Extracted {
                asset: id,
                path: "assets/a.material.ron".into(),
            },
        },
        MaterialImportSettings {
            material: "trim".into(),
            mode: MaterialImportMode::Replaced {
                asset: id,
                path: "assets/b.material.ron".into(),
            },
        },
    ];
    let text = format!(
        "({MESHES_KEY}: {}, {MATERIALS_KEY}: {})",
        meshes_ron(&meshes),
        materials_ron(&materials)
    );
    let parsed = regia_asset_pipeline::GltfImporterSettings::from_meta_string(Some(&text));
    assert_eq!(parsed.meshes, meshes, "{text}");
    assert_eq!(parsed.materials, materials, "{text}");
    assert_eq!(
        default_extract_path("assets/models", "robot", "a b~1"),
        "assets/models/robot_a_b_1.material.ron"
    );
}
