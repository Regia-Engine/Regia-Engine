// SPDX-License-Identifier: MPL-2.0
//! UI layout live reload (`docs/specs/12-assets.md` § 8): editing a `.ui.ron` on disk updates the
//! [`UiLibrary`] template and respawns every root spawned from it, keeping the root's settings,
//! within one second. A file that does not parse keeps the old layout and the live root.

use std::path::Path;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetReloadInbox, AssetServerSettings, AssetWatchStatus, SourceKind};
use regia_ui::node::NodeName;
use regia_ui::prelude::*;
use regia_ui::{UiLibrary, UiMigrators, UiTemplate, parse_ui_ron};

const HUD: &str = include_str!("../../../docs/formats/ui-ron.example.ui.ron");

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

fn write_atomically(path: &Path, text: &str) {
    let tmp = path.with_file_name(".regia-tmp-hud");
    std::fs::write(&tmp, text).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

fn names(app: &mut App) -> Vec<String> {
    let mut q = app.world_mut().query::<&NodeName>();
    let mut out: Vec<String> = q.iter(app.world()).map(|n| n.0.clone()).collect();
    out.sort();
    out
}

fn roots(app: &mut App) -> Vec<(Entity, UiRoot)> {
    let mut q = app.world_mut().query::<(Entity, &UiTemplate, &UiRoot)>();
    q.iter(app.world()).map(|(e, _, r)| (e, r.clone())).collect()
}

#[test]
fn editing_a_ui_layout_respawns_its_roots_within_one_second() {
    let root = std::env::temp_dir().join(format!("regia_ui_hot_{}", std::process::id()));
    let dir = root.join("assets").join("ui");
    std::fs::create_dir_all(&dir).expect("dirs");
    let hud_path = dir.join("hud.ui.ron");
    std::fs::write(&hud_path, HUD).expect("hud");

    let (tx, inbox) = AssetReloadInbox::channel();
    let mut app = ui_app();
    app.insert_resource(inbox);
    app.add_plugins(AssetPlugin {
        settings: AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        },
    });
    let file = parse_ui_ron(HUD, "hud.ui.ron", &UiMigrators::default()).expect("parse");
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("hud".into(), file);
    let spawned = regia_ui::authoring::spawn_ui(app.world_mut(), "hud").expect("spawn");
    app.world_mut().entity_mut(spawned).insert(UiRoot {
        sort_order: 7,
        ..UiRoot::default()
    });
    let _watch = regia_asset::hot_reload::spawn_source_forwarder(
        std::slice::from_ref(&dir),
        Duration::from_millis(80),
        |kind| kind == SourceKind::UiLayout,
        tx,
    )
    .expect("watch");
    app.update();
    assert!(names(&mut app).contains(&"score".to_string()));

    let start = Instant::now();
    write_atomically(&hud_path, &HUD.replace("name: \"score\"", "name: \"ammo\""));
    while start.elapsed() < Duration::from_secs(10) {
        app.update();
        if names(&mut app).contains(&"ammo".to_string()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let took = start.elapsed();
    assert_reload_budget(took, "ui reload");
    let names_now = names(&mut app);
    assert!(
        !names_now.contains(&"score".to_string()),
        "old tree despawned: {names_now:?}"
    );
    let live = roots(&mut app);
    assert_eq!(live.len(), 1, "one root, respawned");
    assert_ne!(live[0].0, spawned);
    assert_eq!(live[0].1.sort_order, 7, "root settings kept");

    // A broken layout: reported, the live root stays.
    let errors = app.world().resource::<AssetWatchStatus>().errors;
    write_atomically(&hud_path, "( version: 1, root: ( name: ");
    let start = Instant::now();
    while app.world().resource::<AssetWatchStatus>().errors == errors
        && start.elapsed() < Duration::from_secs(10)
    {
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(
        app.world().resource::<AssetWatchStatus>().errors,
        errors,
        "the broken layout is reported"
    );
    assert_reload_budget(start.elapsed(), "ui error report");
    assert!(names(&mut app).contains(&"ammo".to_string()), "layout kept");
    assert_eq!(roots(&mut app).len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

/// The spec's one-second reload budget, asserted in release builds only: a debug test binary on a
/// loaded machine measures the scheduler, not the reload (audit-v035-flaky H1). Debug runs print it.
fn assert_reload_budget(took: Duration, what: &str) {
    eprintln!("{what} took {took:?}");
    if !cfg!(debug_assertions) {
        assert!(took < Duration::from_secs(1), "{what} took {took:?}");
    }
}
