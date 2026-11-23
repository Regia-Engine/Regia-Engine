// SPDX-License-Identifier: MPL-2.0
//! Editor live asset reload (`docs/specs/12-assets.md` § 8, `docs/specs/19-editor.md`): the open
//! level changed on disk reloads within one second and keeps the editor camera; with unsaved
//! edits it waits for the user's answer; a changed prefab re-instantiates its placed instances.

use std::path::Path;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetWatchStatus, ExternalReloadSource};
use regia_core::math::Vec3;
use regia_editor::asset_watch::{self, PendingLevelReload};
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::viewport::EditorCamera;
use regia_scene::prelude::Name;

fn level(name: &str) -> String {
    format!(
        r#"(
    format_version: 1,
    entities: [
        (guid: "8b6f2a11-0000-4000-8000-000000000001", name: "{name}",
         components: {{ "regia_scene::transform::Transform": () }}),
    ],
)
"#
    )
}

fn prefab(children: usize) -> String {
    let mut entities = String::from(
        r#"(guid: "44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0", name: "Crate",
            components: { "regia_scene::transform::Transform": () }),"#,
    );
    for i in 0..children {
        entities.push_str(&format!(
            r#"
        (guid: "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c{i:02}", name: "Part{i}",
            parent: Some("44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0"),
            components: {{ "regia_scene::transform::Transform": () }}),"#
        ));
    }
    format!("SceneFile(\n    format_version: 5,\n    entities: [\n        {entities}\n    ],\n)\n")
}

fn write_atomically(path: &Path, text: &str) {
    let tmp = path.with_file_name(format!(
        ".regia-tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    std::fs::write(&tmp, text).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

fn names(app: &mut App) -> Vec<String> {
    let mut q = app.world_mut().query::<&Name>();
    let mut out: Vec<String> = q.iter(app.world()).map(|n| n.as_str().to_string()).collect();
    out.sort();
    out
}

fn pump_until(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        asset_watch::pump_asset_watch(app.world_mut(), None);
        app.update();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("nothing happened within 3 s");
}

#[test]
fn level_and_prefab_edits_reload_in_the_edit_world() {
    let root = std::env::temp_dir().join(format!("regia_editor_reload_{}", std::process::id()));
    let levels = root.join("assets").join("levels");
    std::fs::create_dir_all(&levels).expect("dirs");
    let level_path = levels.join("intro.level.ron");
    std::fs::write(&level_path, level("Origin")).expect("level");
    let prefab_path = root.join("assets").join("crate.prefab.ron");
    std::fs::write(&prefab_path, prefab(1)).expect("prefab");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ExternalReloadSource);
    app.add_plugins(regia_asset::AssetPlugin {
        settings: regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: regia_asset::current_platform(),
            content_dir: None,
        },
    });
    app.init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>();
    let camera = EditorCamera {
        target: Vec3::new(3.0, 1.0, -2.0),
        distance: 17.5,
        ..EditorCamera::default()
    };
    app.insert_resource(camera.clone());
    let open = level_io::open_level_file(app.world_mut(), &level_path).expect("open");
    app.world_mut().insert_resource(open);
    app.update();
    asset_watch::start_asset_watch(app.world_mut(), &root).expect("watch");
    assert!(app.world().resource::<AssetWatchStatus>().watching);
    assert!(names(&mut app).contains(&"Origin".to_string()));

    // 1. A clean level: reloaded in place, camera kept.
    write_atomically(&level_path, &level("Beacon"));
    pump_until(&mut app, |app| names(app).contains(&"Beacon".to_string()));
    assert!(!names(&mut app).contains(&"Origin".to_string()));
    let kept = app.world().resource::<EditorCamera>();
    assert_eq!(
        (kept.target, kept.distance),
        (camera.target, camera.distance),
        "camera kept"
    );
    let line = app
        .world()
        .resource::<AssetWatchStatus>()
        .last_line()
        .unwrap_or_default();
    assert!(
        line.starts_with("reloaded assets/levels/intro.level.ron in "),
        "{line}"
    );

    // 2. Unsaved edits: the change waits for an answer.
    let moved = app.world_mut().spawn(regia_scene::Transform::default()).id();
    let edit = regia_editor::command::SetTransform::capture(
        app.world(),
        moved,
        regia_scene::Transform::from_translation(Vec3::X),
    );
    app.world_mut().resource_mut::<UndoStack>().push(Box::new(edit));
    assert!(app.world().resource::<UndoStack>().is_dirty());
    write_atomically(&level_path, &level("Gantry"));
    pump_until(&mut app, |app| {
        app.world().contains_resource::<PendingLevelReload>()
    });
    assert!(
        !names(&mut app).contains(&"Gantry".to_string()),
        "not reloaded behind the user's back"
    );
    asset_watch::resolve_level_reload(app.world_mut(), true);
    assert!(
        names(&mut app).contains(&"Gantry".to_string()),
        "reloaded on 'Reload from disk'"
    );
    assert!(!app.world().resource::<UndoStack>().is_dirty());

    // 3. A placed prefab: its instance is rebuilt from the edited file.
    regia_editor::content::spawn_prefab_asset(app.world_mut(), &prefab_path, Vec3::ZERO).expect("place");
    app.update();
    // Prefab-internal entities carry no `Name`; count transforms instead.
    let parts = |app: &mut App| {
        let mut q = app.world_mut().query::<&regia_scene::Transform>();
        q.iter(app.world()).count()
    };
    let before = parts(&mut app);
    write_atomically(&prefab_path, &prefab(3));
    pump_until(&mut app, |app| parts(app) == before + 2);

    let _ = std::fs::remove_dir_all(&root);
}
