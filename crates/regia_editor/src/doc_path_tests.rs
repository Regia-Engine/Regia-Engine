// SPDX-License-Identifier: MPL-2.0
//! Audit rc1-sec-paths-z (ADR-7388): every editor agent method that opens, reads or saves a file a
//! request names goes through one rule (`agent_ext::document_request_path`). Each escape class is
//! sent to each method: absolute paths outside the project and the temp dir, `..` after
//! normalization (both separators), Windows drive, drive-relative, UNC and verbatim forms, NUL, and a
//! symlink inside the project leading out. Windows-style strings run on every host.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// A module's method table.
type Methods = Vec<(regia_agent::schema::MethodSchema, Handler)>;

/// Escape forms every method must refuse, built from the name of a file the method accepts (`leaf`)
/// and the temp-dir parent of the project (`outside`, a sibling of the project, which `..` reaches).
fn escapes(leaf: &str) -> Vec<String> {
    vec![
        format!("../outside/{leaf}"),
        format!("a/../../outside/{leaf}"),
        format!("a\\..\\..\\outside\\{leaf}"),
        format!("C:\\Users\\Public\\{leaf}"),
        format!("C:{leaf}"),
        format!("\\\\server\\share\\{leaf}"),
        format!("//server/share/{leaf}"),
        format!("\\\\?\\C:\\{leaf}"),
        format!("\\Windows\\{leaf}"),
        format!("a\0/{leaf}"),
        format!("/etc/regia-doc-path-test/{leaf}"),
        format!("sys/etc/{leaf}"),
    ]
}

fn handler(methods: &[(regia_agent::schema::MethodSchema, Handler)], name: &str) -> Handler {
    methods
        .iter()
        .find(|(schema, _)| schema.name == name)
        .map(|(_, handler)| *handler)
        .unwrap_or_else(|| panic!("no method {name}"))
}

fn reason(error: &regia_agent::protocol::RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A project `<base>/project` (with a `sys` symlink to `/` on Unix) and its world.
fn setup(tag: &str) -> (PathBuf, PathBuf, World) {
    let base = regia_test_utils::unique_temp_dir(tag);
    let root = base.join("project");
    std::fs::create_dir_all(root.join("assets/ui")).expect("project");
    std::fs::create_dir_all(base.join("outside")).expect("outside");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/", root.join("sys")).expect("link");
    let mut world = World::new();
    world.insert_resource(crate::command::UndoStack::new());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    (base, root, world)
}

/// Calls `method` with `key` set to each escape form (plus `extra`), asserting a
/// `path_outside_project` refusal and that nothing appeared outside the project.
fn assert_refuses(
    world: &mut World,
    base: &Path,
    handler: Handler,
    method: &str,
    key: &str,
    leaf: &str,
    extra: &Value,
) {
    for bad in escapes(leaf) {
        // The symlink form needs a symlink.
        if bad.starts_with("sys/") && !cfg!(unix) {
            continue;
        }
        let mut params = extra.clone();
        params[key] = json!(bad);
        let result = handler(world, &params);
        let error = match result {
            Ok(out) => panic!("{method} {key}={bad:?} was accepted: {out}"),
            Err(error) => error,
        };
        assert_eq!(
            reason(&error),
            "path_outside_project",
            "{method} {key}={bad:?}: {} {:?}",
            error.message,
            error.data
        );
        assert!(
            !base.join("outside").join(leaf).exists(),
            "{method} {key}={bad:?} wrote outside the project"
        );
    }
}

#[test]
fn ui_designer_and_ui_anim_paths_stay_in_the_project() {
    let (base, root, mut world) = setup("doc-path-ui");
    let methods = crate::ui_designer_agent::methods();
    let open = handler(&methods, "editor.ui_designer_open");
    let save = handler(&methods, "editor.ui_designer_save");
    assert_refuses(
        &mut world,
        &base,
        open,
        "ui_designer_open",
        "path",
        "x.ui.ron",
        &json!({}),
    );
    assert_refuses(
        &mut world,
        &base,
        open,
        "ui_designer_open",
        "path",
        "x.ui.ron",
        &json!({ "new": true }),
    );
    open(&mut world, &json!({ "new": true })).expect("a new document");
    assert_refuses(
        &mut world,
        &base,
        save,
        "ui_designer_save",
        "path",
        "x.ui.ron",
        &json!({}),
    );
    // A save may not overwrite another kind of project file.
    let error = save(&mut world, &json!({ "path": "Regia.toml" })).expect_err("not a .ui.ron");
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str),
        Some("ui_designer_bad_value"),
        "{error:?}"
    );
    assert!(!root.join("Regia.toml").exists());
    // The project-relative form still works.
    let saved = save(&mut world, &json!({ "path": "assets/ui/menu.ui.ron" })).expect("saves");
    assert!(root.join("assets/ui/menu.ui.ron").is_file(), "{saved}");

    let methods = crate::ui_anim_agent::methods();
    let open = handler(&methods, "editor.ui_anim_open");
    let save = handler(&methods, "editor.ui_anim_save");
    assert_refuses(
        &mut world,
        &base,
        open,
        "ui_anim_open",
        "path",
        "x.uianim.ron",
        &json!({}),
    );
    assert_refuses(
        &mut world,
        &base,
        open,
        "ui_anim_open",
        "path",
        "x.uianim.ron",
        &json!({ "new": true }),
    );
    open(
        &mut world,
        &json!({ "new": true, "path": "assets/ui/hud.uianim.ron" }),
    )
    .expect("new");
    assert_refuses(
        &mut world,
        &base,
        save,
        "ui_anim_save",
        "path",
        "x.uianim.ron",
        &json!({}),
    );
    let error = save(&mut world, &json!({ "path": "assets/ui/menu.ui.ron" })).expect_err("not a .uianim.ron");
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str),
        Some("ui_anim_bad_value"),
        "{error:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn conversation_paths_stay_in_the_project() {
    let (base, root, mut world) = setup("doc-path-conv");
    let conv = root.join("assets/conversations");
    std::fs::create_dir_all(&conv).expect("dir");
    std::fs::write(
        conv.join("x.conv.ron"),
        "(format_version: 1, id: \"x\", lines: [], steps: [End])\n",
    )
    .expect("conv");
    std::fs::write(
        base.join("outside/x.conv.ron"),
        "(format_version: 1, id: \"secret\", lines: [], steps: [End])\n",
    )
    .expect("outside conv");
    let methods = crate::conversation_panel::agent::methods();
    let open = handler(&methods, "editor.conversation_open");
    let save = handler(&methods, "editor.conversation_save");
    for bad in escapes("x.conv.ron") {
        if bad.starts_with("sys/") && !cfg!(unix) {
            continue;
        }
        let error = open(&mut world, &json!({ "path": bad })).expect_err(&bad);
        assert_eq!(reason(&error), "path_outside_project", "open {bad:?}: {error:?}");
    }
    // `project` names a directory the editor reads speakers and locales from.
    let error = open(
        &mut world,
        &json!({ "path": "assets/conversations/x.conv.ron", "project": "../outside" }),
    )
    .expect_err("project outside");
    assert_eq!(reason(&error), "path_outside_project", "{error:?}");
    open(&mut world, &json!({ "path": "assets/conversations/x.conv.ron" })).expect("opens in the project");
    std::fs::remove_file(base.join("outside/x.conv.ron")).expect("rm");
    assert_refuses(
        &mut world,
        &base,
        save,
        "conversation_save",
        "path",
        "x.conv.ron",
        &json!({}),
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn mode2d_paths_stay_in_the_project() {
    let (base, _root, mut world) = setup("doc-path-2d");
    let methods = crate::mode2d::methods();
    let set = handler(&methods, "editor.mode2d_set");
    let save = handler(&methods, "editor.tilemap_save");
    assert_refuses(
        &mut world,
        &base,
        set,
        "mode2d_set",
        "map",
        "x.tilemap.ron",
        &json!({}),
    );
    assert_refuses(
        &mut world,
        &base,
        set,
        "mode2d_set",
        "rules",
        "x.autotile.ron",
        &json!({}),
    );
    assert_refuses(
        &mut world,
        &base,
        save,
        "tilemap_save",
        "path",
        "x.tilemap.ron",
        &json!({}),
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A level's `TileMap.map` is content: one that climbs out of the project names no map.
#[test]
fn level_tilemap_paths_from_content_stay_in_the_project() {
    let (base, root, mut world) = setup("doc-path-2d-level");
    std::fs::write(base.join("outside/x.tilemap.ron"), "()").expect("outside map");
    std::fs::create_dir_all(root.join("assets/maps")).expect("maps");
    std::fs::write(root.join("assets/maps/in.tilemap.ron"), "()").expect("inside map");
    let outside = base.join("outside/x.tilemap.ron").display().to_string();
    for map in [
        "../outside/x.tilemap.ron".to_string(),
        "../../outside/x.tilemap.ron".to_string(),
        outside,
        "maps/in.tilemap.ron".to_string(),
    ] {
        world.spawn(regia_2d::TileMap {
            map,
            ..Default::default()
        });
    }
    let found = crate::mode2d::level_maps(&mut world);
    assert_eq!(found, vec![root.join("assets").join("maps/in.tilemap.ron")]);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn importer_inspect_paths_stay_in_the_project() {
    let (base, _root, mut world) = setup("doc-path-import");
    let cases: Vec<(Methods, &str, &str)> = vec![
        (crate::usd_agent::methods(), "editor.usd_inspect", "x.usda"),
        (crate::mesh_image_agent::methods(), "editor.ply_inspect", "x.ply"),
        (crate::mesh_image_agent::methods(), "editor.stl_inspect", "x.stl"),
        (crate::mesh_image_agent::methods(), "editor.psd_inspect", "x.psd"),
        (crate::fbx_agent::methods(), "editor.fbx_inspect", "x.fbx"),
        (crate::fbx_agent::methods(), "editor.obj_inspect", "x.obj"),
        (crate::bvh_agent::methods(), "editor.bvh_inspect", "x.bvh"),
        (crate::svg_agent::methods(), "editor.svg_report", "x.svg"),
    ];
    for (methods, name, leaf) in cases {
        let call = handler(&methods, name);
        assert_refuses(&mut world, &base, call, name, "path", leaf, &json!({}));
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The working directory is not a document root (it was one through ADR-1816's output rule): an
/// editor started from the home folder would otherwise open and overwrite all of it. Unit tests run
/// in the crate directory, which lies outside the project and the temp dir.
#[test]
fn the_working_directory_is_not_a_document_root() {
    let (base, _root, mut world) = setup("doc-path-cwd");
    let cwd = std::env::current_dir().expect("cwd");
    if cwd.starts_with(std::env::temp_dir()) {
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    let manifest = cwd.join("Cargo.toml").display().to_string();
    let error =
        handler(&crate::usd_agent::methods(), "editor.usd_inspect")(&mut world, &json!({ "path": manifest }))
            .expect_err("cwd file");
    assert_eq!(reason(&error), "path_outside_project", "{error:?}");
    let bt = cwd.join("nope-doc-path.bt.ron").display().to_string();
    let error =
        handler(&crate::bt_editor::agent::methods(), "editor.bt_open")(&mut world, &json!({ "path": bt }))
            .expect_err("cwd bt");
    assert_eq!(reason(&error), "path_outside_project", "{error:?}");
    let _ = std::fs::remove_dir_all(&base);
}
