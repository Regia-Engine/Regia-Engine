// SPDX-License-Identifier: MPL-2.0
//! Loading `.camera.ron` files and hot reload (`docs/specs/35-camera-framework.md` § 3, ADR-2030):
//! a file reaches a driver, a changed file reaches a running driver with its variables kept, a file
//! that does not parse changes nothing and says why, and a level names a camera by file. Files live
//! in a unique temp dir and written atomically; changes arrive as the messages the watcher sends, and
//! frames are counted, so nothing here waits on a clock.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, SourceAssetChanged, current_platform};
use regia_framework::camera::CameraPlugin;
use regia_framework::camera::asset::{CameraBuildStatus, ParamValue, VarValue};
use regia_framework::camera::driver::{CameraAssets, CameraDriver};
use regia_framework::camera::load::{
    BAD_PATH, CameraAssetSource, CameraAssetsPlugin, CameraFiles, FILE_NOT_FOUND, load_camera_file,
};
use regia_scene::Transform;
use regia_scene::components::Name;

fn asset(fov: f32) -> String {
    format!(
        r#"(format_version: 1, name: "player",
            params: {{ "len": Float(3.0) }},
            variables: [(name: "aiming", default: Bool(false))],
            rigs: [(name: "main", root: [Fov(degrees: Literal({fov:?})), Boom(length: Param("len"))])],
            director: Fixed("main"))"#
    )
}

const REL: &str = "camera/player.camera.ron";

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let file = root.join("assets").join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(&file, text.as_bytes()).expect("write");
    file
}

fn open(root: &Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open")
}

fn app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CameraPlugin));
    app.insert_resource(open(root));
    app
}

fn changed(app: &mut App, file: &Path) {
    app.world_mut().write_message(SourceAssetChanged::new(file));
    app.update();
}

fn active_fov(app: &App) -> Option<String> {
    let assets = app.world().resource::<CameraAssets>();
    assets
        .active("player")
        .map(|a| format!("{:?}", a.rigs[0].root[0]))
}

#[test]
fn a_file_loads_by_its_asset_path_under_its_written_name() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-path");
    write(&dir, REL, &asset(60.0));
    let mut app = app(&dir);
    let status = load_camera_file(app.world_mut(), &format!("assets/{REL}")).expect("loads");
    assert_eq!(status, CameraBuildStatus::Ok);
    let world = app.world();
    let assets = world.resource::<CameraAssets>();
    assert_eq!(assets.path("player"), Some(REL));
    assert_eq!(assets.revision("player"), 1);
    let file = world
        .resource::<CameraFiles>()
        .get(REL)
        .expect("recorded")
        .clone();
    assert_eq!(file.asset.as_deref(), Some("player"));
    assert_eq!((file.generation, file.cooked, file.error), (1, false, None));
    assert_eq!(world.resource::<CameraFiles>().path_of_asset("player"), Some(REL));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_cannot_load_says_why_and_is_on_record() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-errors");
    let mut app = app(&dir);
    let world = app.world_mut();
    let refused = |r: Result<CameraBuildStatus, _>| {
        r.map_err(|e: regia_framework::camera::load::CameraFileError| e.reason)
    };
    assert_eq!(refused(load_camera_file(world, "../a.camera.ron")), Err(BAD_PATH));
    assert_eq!(refused(load_camera_file(world, "a.ron")), Err(BAD_PATH));
    assert_eq!(
        refused(load_camera_file(world, "none.camera.ron")),
        Err(FILE_NOT_FOUND)
    );
    write(&dir, "bad.camera.ron", "(format_version: 1,\n  nope: 3)");
    let error = load_camera_file(world, "bad.camera.ron").expect_err("does not parse");
    assert_eq!(error.reason, "camera_asset_invalid");
    write(&dir, "new.camera.ron", "(format_version: 99, name: \"n\")");
    let error = load_camera_file(world, "new.camera.ron").expect_err("too new");
    assert_eq!(error.reason, "camera_asset_invalid");
    assert!(error.detail.contains("99"), "{}", error.detail);
    let files = world.resource::<CameraFiles>();
    assert_eq!(
        files
            .get("none.camera.ron")
            .and_then(|f| f.error.as_ref())
            .map(|e| e.reason),
        Some(FILE_NOT_FOUND)
    );
    assert!(
        files
            .get("bad.camera.ron")
            .is_some_and(|f| f.error.is_some() && f.asset.is_none())
    );
    assert!(
        files.get("../a.camera.ron").is_none(),
        "a refused path is no file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_changed_file_reaches_a_running_driver_and_keeps_its_state() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-reload");
    let file = write(&dir, REL, &asset(60.0));
    let mut app = app(&dir);
    load_camera_file(app.world_mut(), REL).expect("loads");
    let camera = app
        .world_mut()
        .spawn((Transform::default(), CameraDriver::new("player")))
        .id();
    app.update();
    let core = |app: &mut App| {
        app.world_mut()
            .get_mut::<CameraDriver>(camera)
            .expect("driver")
            .core()
            .map(|core| (core.vars.get("aiming"), core.param("len").cloned()))
    };
    assert!(core(&mut app).is_some(), "bound");
    {
        let world = app.world_mut();
        let mut driver = world.get_mut::<CameraDriver>(camera).expect("driver");
        let bound = driver.core_mut().expect("bound");
        bound.set_var("aiming", VarValue::Bool(true)).expect("var");
        bound.set_param("len", ParamValue::Float(9.0)).expect("param");
    }

    write(&dir, REL, &asset(75.0));
    changed(&mut app, &file);
    assert_eq!(app.world().resource::<CameraAssets>().revision("player"), 2);
    assert!(
        active_fov(&app).is_some_and(|f| f.contains("75")),
        "{:?}",
        active_fov(&app)
    );
    app.update();
    let (aiming, len) = core(&mut app).expect("rebound");
    assert_eq!(
        aiming,
        Some(VarValue::Bool(true)),
        "the variable survives the rebind"
    );
    assert_eq!(len, Some(ParamValue::Float(9.0)), "so does the override");
    let bound_asset_fov = {
        let driver = app.world().get::<CameraDriver>(camera).expect("driver");
        format!("{:?}", driver.core().expect("bound").asset().rigs[0].root[0])
    };
    assert!(
        bound_asset_fov.contains("75"),
        "the driver runs the new file: {bound_asset_fov}"
    );
    assert_eq!(
        app.world()
            .resource::<CameraFiles>()
            .get(REL)
            .map(|f| f.generation),
        Some(2)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_that_breaks_changes_nothing_until_it_is_fixed() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-broken");
    let file = write(&dir, REL, &asset(60.0));
    let mut app = app(&dir);
    load_camera_file(app.world_mut(), REL).expect("loads");

    write(&dir, REL, "(format_version: 1, name: \"player\",\n  oops");
    changed(&mut app, &file);
    {
        let world = app.world();
        let assets = world.resource::<CameraAssets>();
        assert_eq!(assets.revision("player"), 1);
        assert_eq!(assets.status("player"), Some(CameraBuildStatus::Ok));
        let record = world.resource::<CameraFiles>().get(REL).expect("record");
        let error = record.error.as_ref().expect("the failure is on record");
        assert_eq!(error.reason, "camera_asset_invalid");
        assert!(error.detail.contains('2'), "line and column: {}", error.detail);
    }
    assert!(active_fov(&app).is_some_and(|f| f.contains("60")));

    // It parses but does not validate: the working copy shows it, drivers keep the last good one.
    write(
        &dir,
        REL,
        r#"(format_version: 1, name: "player", rigs: [(name: "main", root: [])], director: Fixed("gone"))"#,
    );
    changed(&mut app, &file);
    {
        let assets = app.world().resource::<CameraAssets>();
        assert_eq!(assets.status("player"), Some(CameraBuildStatus::Errors));
        assert_eq!(assets.revision("player"), 1);
        assert!(assets.issues("player").is_some_and(|issues| !issues.is_empty()));
    }
    assert!(active_fov(&app).is_some_and(|f| f.contains("60")));
    assert_eq!(
        app.world()
            .resource::<CameraFiles>()
            .get(REL)
            .and_then(|f| f.error.clone()),
        None
    );

    // The file disappears: on record, nothing else moves.
    std::fs::remove_file(&file).expect("remove");
    changed(&mut app, &file);
    let world = app.world();
    assert_eq!(
        world
            .resource::<CameraFiles>()
            .get(REL)
            .and_then(|f| f.error.as_ref())
            .map(|e| e.reason),
        Some(FILE_NOT_FOUND)
    );
    assert_eq!(world.resource::<CameraAssets>().revision("player"), 1);

    write(&dir, REL, &asset(50.0));
    changed(&mut app, &file);
    assert_eq!(app.world().resource::<CameraAssets>().revision("player"), 2);
    assert!(active_fov(&app).is_some_and(|f| f.contains("50")));
    assert_eq!(
        app.world()
            .resource::<CameraFiles>()
            .get(REL)
            .and_then(|f| f.error.clone()),
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_change_goes_to_the_longest_matching_file_and_other_files_are_ignored() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-routing");
    let short = asset(10.0).replace("\"player\"", "\"short\"");
    let long = asset(20.0).replace("\"player\"", "\"long\"");
    write(&dir, "a.camera.ron", &short);
    let long_file = write(&dir, "cam/a.camera.ron", &long);
    let mut app = app(&dir);
    load_camera_file(app.world_mut(), "a.camera.ron").expect("short");
    load_camera_file(app.world_mut(), "cam/a.camera.ron").expect("long");
    changed(&mut app, &long_file);
    let assets = app.world().resource::<CameraAssets>();
    assert_eq!((assets.revision("long"), assets.revision("short")), (2, 1));
    // A file nothing has loaded and a file of another kind.
    let other = write(&dir, "b.camera.ron", &asset(30.0));
    changed(&mut app, &other);
    changed(&mut app, &dir.join("assets/a.vfx.ron"));
    let assets = app.world().resource::<CameraAssets>();
    assert_eq!(assets.names().count(), 2);
    assert_eq!(assets.revision("long"), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An asset a host parsed itself still follows its file, when the host gave the path.
#[test]
fn an_asset_a_host_stored_with_a_path_reloads_too() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-host");
    let file = write(&dir, REL, &asset(60.0));
    let mut app = app(&dir);
    app.world_mut()
        .resource_scope(|world, mut assets: Mut<CameraAssets>| {
            let hooks = world.resource::<regia_framework::camera::director::CameraHooks>();
            assets
                .load_ron(&asset(60.0), Some(REL.to_owned()), hooks)
                .expect("parses");
        });
    write(&dir, REL, &asset(61.0));
    changed(&mut app, &file);
    assert_eq!(app.world().resource::<CameraAssets>().revision("player"), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_assets_plugin_alone_lists_and_reloads_without_moving_anything() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-edit-world");
    let file = write(&dir, REL, &asset(60.0));
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CameraAssetsPlugin));
    app.insert_resource(open(&dir));
    load_camera_file(app.world_mut(), REL).expect("loads");
    let camera = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(1.0, 2.0, 3.0)),
            CameraAssetSource::new(REL),
        ))
        .id();
    write(&dir, REL, &asset(70.0));
    changed(&mut app, &file);
    assert_eq!(app.world().resource::<CameraAssets>().revision("player"), 2);
    assert!(
        app.world().get::<CameraDriver>(camera).is_none(),
        "the edit world attaches no driver"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_level_names_a_camera_by_file_and_the_driver_follows_the_file() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-source");
    let file = write(&dir, REL, &asset(60.0));
    let mut app = app(&dir);
    let hero = app
        .world_mut()
        .spawn((Name::new("Hero"), Transform::default()))
        .id();
    let camera = app
        .world_mut()
        .spawn((
            Transform::default(),
            CameraAssetSource {
                path: REL.to_owned(),
                target: "Hero".to_owned(),
                seed: 7,
            },
        ))
        .id();
    app.update();
    {
        let driver = app.world().get::<CameraDriver>(camera).expect("attached");
        assert_eq!(driver.asset, "player");
        assert_eq!(driver.target, Some(hero));
        assert_eq!(driver.seed, 7);
    }
    app.update();
    assert!(
        app.world()
            .get::<CameraDriver>(camera)
            .expect("driver")
            .core()
            .is_some(),
        "bound"
    );

    // The file now names another asset: the driver moves to it.
    write(&dir, REL, &asset(60.0).replace("\"player\"", "\"renamed\""));
    changed(&mut app, &file);
    app.update();
    let driver = app.world().get::<CameraDriver>(camera).expect("driver");
    assert_eq!(driver.asset, "renamed");
    assert_eq!(
        driver.core().map(|core| core.asset().name.as_str()),
        Some("renamed")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_with_a_broken_file_waits_and_is_not_read_every_frame() {
    let dir = regia_test_utils::unique_temp_dir("camera-load-source-broken");
    let file = write(&dir, REL, "(");
    let mut app = app(&dir);
    let camera = app
        .world_mut()
        .spawn((Transform::default(), CameraAssetSource::new(REL)))
        .id();
    app.update();
    app.update();
    assert!(app.world().get::<CameraDriver>(camera).is_none());
    let error = app
        .world()
        .resource::<CameraFiles>()
        .get(REL)
        .and_then(|f| f.error.clone());
    assert_eq!(error.map(|e| e.reason), Some("camera_asset_invalid"));
    // A later bad write of the file is not seen without a change message: no polling.
    write(&dir, REL, &asset(60.0));
    app.update();
    assert!(app.world().get::<CameraDriver>(camera).is_none());
    changed(&mut app, &file);
    app.update();
    assert_eq!(
        app.world()
            .get::<CameraDriver>(camera)
            .map(|d| d.asset.clone())
            .as_deref(),
        Some("player")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_source_component_round_trips_through_reflection() {
    use regia_ecs::reflect::AppTypeRegistry;
    let mut app = App::new();
    app.add_plugins(CameraAssetsPlugin);
    let registry = app.world().resource::<AppTypeRegistry>().read();
    assert!(
        registry
            .get(std::any::TypeId::of::<CameraAssetSource>())
            .is_some(),
        "levels and play snapshots need it registered"
    );
}
