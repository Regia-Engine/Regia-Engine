// SPDX-License-Identifier: MPL-2.0
//! OS file drop tests (spec addendum § 8.7): routing, sorting, the synthetic-drop path through egui's
//! `dropped_files`, viewport placement as one undo step, and `editor.import_files` through the
//! registered agent extension.

use super::*;
use regia_core::math::Vec3;
use regia_render::MeshSource;
use regia_scene::prelude::Transform;

/// A one-triangle binary glTF (GLB 2.0: JSON chunk + BIN chunk, both 4-byte aligned).
pub(crate) fn glb_triangle() -> Vec<u8> {
    let mut bin = Vec::new();
    for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend(value.to_le_bytes());
    }
    for index in [0u32, 1, 2] {
        bin.extend(index.to_le_bytes());
    }
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0},"indices":1}]}],"buffers":[{"byteLength":48}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}]}"#;
    let mut json = json.as_bytes().to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend(b"glTF");
    out.extend(2u32.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((json.len() as u32).to_le_bytes());
    out.extend(b"JSON");
    out.extend(&json);
    out.extend((bin.len() as u32).to_le_bytes());
    out.extend(b"BIN\0");
    out.extend(&bin);
    out
}

/// A 1×1 PNG.
pub(crate) fn tiny_png() -> Vec<u8> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[200u8, 40, 40, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("png");
    bytes
}

/// A project with `assets/` and an outside folder holding `crate.glb`, `wall.png` and `notes.txt`.
fn fixture(tag: &str) -> (PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let outside = regia_test_utils::unique_temp_dir(&format!("{tag}_src"));
    std::fs::write(outside.join("crate.glb"), glb_triangle()).expect("glb");
    std::fs::write(outside.join("wall.png"), tiny_png()).expect("png");
    std::fs::write(outside.join("notes.txt"), b"not an asset").expect("txt");
    (root, outside)
}

fn editor_world(root: &Path) -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    crate::mesh_source::register_level_types(app.world_mut());
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.world_mut().insert_resource(UndoStack::new());
    app
}

fn mesh_positions(world: &mut World, rel: &str) -> Vec<Vec3> {
    let mut query = world.query::<(&MeshSource, &Transform)>();
    query
        .iter(world)
        .filter(|(mesh, _)| mesh.mesh == rel)
        .map(|(_, transform)| transform.translation)
        .collect()
}

/// A file as egui-winit hands a native drop to egui (a path; the bytes read on demand).
#[derive(Debug)]
struct OsFile(PathBuf);

impl egui::DroppedFile for OsFile {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

/// Runs one egui frame whose raw input carries `files` as OS drops (what egui-winit makes of winit's
/// `DroppedFile`), with the pointer at `pointer`, then calls the shell's hook.
fn drop_frame(world: &mut World, root: &Path, files: &[PathBuf], pointer: egui::Pos2) -> Option<String> {
    let ctx = egui::Context::default();
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        ..egui::RawInput::default()
    };
    input.events.push(egui::Event::PointerMoved(pointer));
    input.dropped_files = files
        .iter()
        .map(|path| {
            std::sync::Arc::new(OsFile(path.clone())) as std::sync::Arc<dyn egui::DroppedFile + Send + Sync>
        })
        .collect();
    let mut output = ctx.run_ui(input, |_ui| {});
    output.textures_delta.clear();
    handle_os_drops(&ctx, world, Some(root), false)
}

/// Review sprint-w12-rev-g5 #12: a background drop aimed at the viewport whose import ends while
/// play runs is not placed into the edit world; it says so. With play stopped it is placed.
#[test]
fn a_drop_whose_import_ends_during_play_is_not_placed() {
    let root = regia_test_utils::unique_temp_dir("os-drop-finish-playing");
    let pending = |world: &mut World| PendingDrop {
        job: crate::editor_jobs::ImportJob::finished_for_test(world, crate::content::ImportReport::default()),
        place: Some(None),
        in_project: vec!["assets/readme.txt".to_string()],
        unsupported: Vec::new(),
        folder: "assets".to_string(),
    };
    let mut world = World::new();
    let drop = pending(&mut world);
    world.insert_resource(PendingDrops(vec![drop]));
    let toast = finish_pending(&mut world, Some(&root), true).expect("the finished drop answers");
    assert!(
        toast.contains("1 failed"),
        "the skipped placement is reported: {toast}"
    );
    assert!(
        !world.contains_resource::<UndoStack>(),
        "nothing was placed into the edit world"
    );
    let drop = pending(&mut world);
    world.insert_resource(PendingDrops(vec![drop]));
    finish_pending(&mut world, Some(&root), false).expect("answers");
    assert!(
        world.contains_resource::<UndoStack>(),
        "with play stopped the placement runs"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn routing_prefers_the_aimed_viewport_then_the_hovered_folder_then_the_shown_one() {
    let zone = BrowserDropZone {
        rect: Some(egui::Rect::from_min_max(
            egui::pos2(0.0, 400.0),
            egui::pos2(800.0, 600.0),
        )),
        hovered_folder: Some("assets/props".into()),
        current_folder: "assets/env".into(),
        pass: 1,
    };
    let inside = Some(egui::pos2(10.0, 500.0));
    let outside = Some(egui::pos2(10.0, 100.0));
    assert_eq!(
        route_drop(outside, true, &zone),
        DropTarget::Viewport("assets/env".into())
    );
    assert_eq!(
        route_drop(inside, false, &zone),
        DropTarget::Folder("assets/props".into())
    );
    assert_eq!(
        route_drop(outside, false, &zone),
        DropTarget::Folder("assets/env".into())
    );
    assert_eq!(
        route_drop(None, false, &zone),
        DropTarget::Folder("assets/env".into())
    );
    let no_row = BrowserDropZone {
        hovered_folder: None,
        ..zone
    };
    assert_eq!(
        route_drop(inside, false, &no_row),
        DropTarget::Folder("assets/env".into())
    );
}

#[test]
fn sorting_copies_outside_files_keeps_project_assets_and_lists_unsupported_ones() {
    let (root, outside) = fixture("os_drop_sort");
    std::fs::create_dir_all(root.join("assets/props")).expect("props");
    std::fs::write(root.join("assets/props/rock.glb"), glb_triangle()).expect("rock");
    let types = crate::content::importable_extensions()
        .iter()
        .map(|e| (*e).to_string())
        .collect::<Vec<_>>();
    let sorted = sort_drop(
        &root,
        &[
            outside.join("crate.glb"),
            outside.join("notes.txt"),
            outside.clone(),
            root.join("assets/props/rock.glb"),
        ],
        &types,
    );
    assert_eq!(sorted.import, vec![outside.join("crate.glb")]);
    assert_eq!(sorted.in_project, vec!["assets/props/rock.glb".to_string()]);
    assert_eq!(sorted.unsupported.len(), 2, "{:?}", sorted.unsupported);
    assert!(sorted.unsupported.contains(&"notes.txt".to_string()));
    let toast = unsupported_toast(&sorted.unsupported).expect("one toast");
    assert!(toast.contains("notes.txt"), "{toast}");
    assert_eq!(unsupported_toast(&[]), None);
}

#[test]
fn a_synthetic_drop_on_the_browser_imports_a_glb_and_a_png_into_the_hovered_folder() {
    let (root, outside) = fixture("os_drop_browser");
    let mut app = editor_world(&root);
    let world = app.world_mut();
    world.insert_resource(OsDropMode::Inline);
    let ctx_frame = 0;
    world.insert_resource(BrowserDropZone {
        rect: Some(egui::Rect::from_min_max(
            egui::pos2(0.0, 400.0),
            egui::pos2(800.0, 600.0),
        )),
        hovered_folder: Some("assets/drops".into()),
        current_folder: String::new(),
        pass: ctx_frame,
    });
    let files = [
        outside.join("crate.glb"),
        outside.join("wall.png"),
        outside.join("notes.txt"),
    ];
    let toast = drop_frame(world, &root, &files, egui::pos2(20.0, 500.0)).expect("a toast");
    assert!(root.join("assets/drops/crate.glb").is_file(), "{toast}");
    assert!(root.join("assets/drops/wall.png").is_file(), "{toast}");
    assert!(!root.join("assets/drops/notes.txt").exists());
    assert!(toast.contains("imported 2"), "{toast}");
    assert!(
        toast.contains("notes.txt"),
        "one toast lists the unsupported file: {toast}"
    );
    // Copied, never referenced in place: the sources are untouched and the copies are separate.
    assert!(outside.join("crate.glb").is_file());
    assert!(regia_asset::meta_path_for(&root.join("assets/drops/crate.glb")).is_file());
    // A browser drop places nothing.
    assert!(mesh_positions(world, "assets/drops/crate.glb").is_empty());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
}

#[test]
fn a_synthetic_drop_on_the_viewport_imports_and_places_at_the_aim_as_one_undo_step() {
    let (root, outside) = fixture("os_drop_viewport");
    let mut app = editor_world(&root);
    let world = app.world_mut();
    world.insert_resource(OsDropMode::Inline);
    world.insert_resource(ViewportDropAim {
        origin: Vec3::new(4.0, 0.0, 0.0),
        direction: Vec3::Y,
        target_depth: 2.5,
    });
    let files = [outside.join("crate.glb"), outside.join("wall.png")];
    let toast = drop_frame(world, &root, &files, egui::pos2(300.0, 200.0)).expect("a toast");
    assert!(toast.contains("placed 1"), "{toast}");
    let placed = mesh_positions(world, "assets/crate.glb");
    assert_eq!(placed.len(), 1, "{toast}");
    assert!(
        (placed[0] - Vec3::new(4.0, 2.5, 0.0)).length() < 1.0e-4,
        "{placed:?}"
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one undo step");
    crate::command::undo_world(world);
    assert!(
        mesh_positions(world, "assets/crate.glb").is_empty(),
        "undo removes the placement"
    );
    // The drop's aim does not linger past the drop.
    assert!(
        world.get_resource::<ViewportDropAim>().is_some(),
        "the frame's own aim is restored"
    );
}

#[test]
fn several_placed_files_are_one_undo_step() {
    let (root, outside) = fixture("os_drop_group");
    std::fs::write(outside.join("barrel.glb"), glb_triangle()).expect("glb");
    let mut app = editor_world(&root);
    let world = app.world_mut();
    let sorted = SortedDrop {
        import: vec![outside.join("crate.glb"), outside.join("barrel.glb")],
        ..SortedDrop::default()
    };
    let aim = ViewportDropAim {
        origin: Vec3::ZERO,
        direction: Vec3::X,
        target_depth: 3.0,
    };
    let outcome = import_and_place(world, &root, "props", &sorted, Some(Some(aim)), None).expect("drop");
    assert_eq!(outcome.placed.len(), 2, "{outcome:?}");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    crate::command::undo_world(world);
    assert!(mesh_positions(world, "assets/props/crate.glb").is_empty());
    assert!(mesh_positions(world, "assets/props/barrel.glb").is_empty());
    crate::command::redo_world(world);
    assert_eq!(mesh_positions(world, "assets/props/barrel.glb").len(), 1);
}

#[test]
fn import_files_runs_through_the_registered_agent_extension() {
    let (root, outside) = fixture("os_drop_agent");
    let mut app = editor_world(&root);
    let world = app.world_mut();
    crate::agent_ext::install(world);
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (schema, handler) = extension.method("editor.import_files").expect("registered");
    assert!(schema.mutating);
    let code = |err: RpcError| err.data.and_then(|d| d.get("drop_error").cloned());
    let err = handler(world, &json!({ "paths": [] })).expect_err("empty");
    assert_eq!(code(err), Some(json!("drop_bad_params")));
    let err = handler(
        world,
        &json!({ "paths": [outside.join("crate.glb")], "folder": "../escape" }),
    )
    .expect_err("escape");
    assert_eq!(code(err), Some(json!("drop_bad_folder")));
    let reply = handler(
        world,
        &json!({
            "paths": [outside.join("crate.glb"), outside.join("notes.txt")],
            "folder": "agent",
            "place": true,
        }),
    )
    .expect("import_files");
    assert_eq!(reply["imported"], json!(["assets/agent/crate.glb"]));
    assert_eq!(reply["placed"], json!(["assets/agent/crate.glb"]));
    assert_eq!(reply["unsupported"], json!(["notes.txt"]));
    assert_eq!(mesh_positions(world, "assets/agent/crate.glb").len(), 1);
}
