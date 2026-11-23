// SPDX-License-Identifier: MPL-2.0
//! `photo.enter`, `photo.set`, `photo.capture` and `photo.status` through the dispatcher.

use regia_agent::protocol::RpcError;
use regia_agent::{AgentConfig, AgentPermissions, AgentPlugin, AgentSettings, LocalReply, call_local};
use regia_app::prelude::*;
use regia_framework::photo_mode::{PhotoMode, PhotoModePlugin};
use serde_json::{Value, json};

fn code_of(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(app.world_mut(), method, params, None) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} deferred; photo methods answer immediately"),
    }
}

fn app() -> (App, std::path::PathBuf) {
    let project = regia_test_utils::unique_temp_dir("photo_mode_dispatch");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhotoModePlugin, AgentPlugin))
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        });
    enable_fixed_step(&mut app, 60.0);
    app.finish_plugins();
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            rate_limit_per_second: u32::MAX,
            heavy_rate_limit_per_second: u32::MAX,
            ..AgentConfig::default()
        },
        source: None,
    });
    {
        let mut mode = app.world_mut().resource_mut::<PhotoMode>();
        mode.pictures = Some(project.clone());
        mode.hires = false;
        mode.frame_w = 2;
        mode.frame_h = 2;
        mode.frame_dt = Some(0.0);
    }
    (app, project)
}

#[test]
fn photo_methods_round_trip_and_competitive_is_refused() {
    let (mut app, project) = app();
    let closed = call(&mut app, "photo.status", json!({})).expect("status");
    assert_eq!(closed["active"], false);
    assert_eq!(closed["pause_sim"], false);

    let denied = call(&mut app, "photo.enter", json!({ "competitive": true })).expect_err("competitive");
    assert_eq!(code_of(&denied), "photo_not_allowed");
    assert_eq!(
        call(&mut app, "photo.status", json!({})).expect("status")["active"],
        false
    );

    let inactive = call(&mut app, "photo.capture", json!({})).expect_err("closed");
    assert_eq!(code_of(&inactive), "photo_inactive");

    let opened = call(&mut app, "photo.enter", json!({ "anchor": [1.0, 2.0, 3.0] })).expect("enter");
    assert_eq!(opened["active"], true);
    assert_eq!(opened["roam"], "free");
    assert_eq!(opened["radius_m"], 30.0);
    assert_eq!(opened["pause_sim"], false);
    assert_eq!(opened["anchor"], json!([1.0, 2.0, 3.0]));

    let set = call(
        &mut app,
        "photo.set",
        json!({ "fov_deg": 1.0, "exposure_ev": 9.0, "tick": 123, "game": "ridge", "map": "north" }),
    )
    .expect("set");
    assert_eq!(set["fov_deg"], 20.0);
    assert_eq!(set["exposure_ev"], 3.0);
    assert_eq!(set["tick"], 123);
    assert_eq!(set["game"], "ridge");
    assert_eq!(set["map"], "north");

    let shot = call(
        &mut app,
        "photo.capture",
        json!({ "directory": project, "width": 2, "height": 2, "hires": false, "tick": 123 }),
    )
    .expect("capture");
    assert_eq!(shot["name"], "ridge-north-t000123-1.png");
    assert_eq!(shot["width"], 2);
    assert_eq!(shot["height"], 2);
    assert_eq!(shot["tick"], 123);
    assert!(project.join("ridge-north-t000123-1.png").is_file());

    let online = call(&mut app, "photo.enter", json!({ "networked": true })).expect("online");
    assert_eq!(online["roam"], "online");
    assert_eq!(online["radius_m"], 12.0);
    assert_eq!(online["pause_sim"], false);

    let tick = app.world().resource::<PhotoMode>().tick;
    app.update();
    app.update();
    assert!(app.world().resource::<PhotoMode>().tick > tick);
    assert!(!app.world().resource::<Time>().paused);
    std::fs::remove_dir_all(&project).expect("remove");
}

/// ADR-8795: a request `directory` follows the output-root rule. A relative folder joins the project
/// root (it used to join the process's working directory, so the dispatch fuzz test's `"Rock"` and
/// `"../../outside"` wrote stills into the source tree); `..` and an absolute folder outside the
/// allowed roots are refused before anything is written, and the refusal leaves the session tick alone.
#[test]
fn capture_directory_is_confined_to_the_allowed_roots() {
    let (mut app, project) = app();
    call(&mut app, "photo.enter", json!({})).expect("enter");
    let shot = call(
        &mut app,
        "photo.capture",
        json!({ "directory": "shots", "width": 2, "height": 2, "hires": false, "tick": 7 }),
    )
    .expect("a relative folder joins the project");
    let name = shot["name"].as_str().expect("name").to_string();
    assert!(project.join("shots").join(&name).is_file(), "{shot}");

    let crate_dir = env!("CARGO_MANIFEST_DIR");
    for directory in ["../../outside", "shots/../../outside", crate_dir] {
        let refused = call(
            &mut app,
            "photo.capture",
            json!({ "directory": directory, "width": 2, "height": 2, "hires": false, "tick": 8 }),
        )
        .expect_err(directory);
        assert_eq!(code_of(&refused), "F0002", "{directory}");
        let data = refused.data.as_ref().expect("data");
        assert_eq!(data["reason"], "path_outside_project", "{directory}: {data}");
    }
    assert_eq!(
        app.world().resource::<PhotoMode>().tick,
        7,
        "a refusal sets no tick"
    );
    let stray: Vec<String> = std::fs::read_dir(crate_dir)
        .expect("crate dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("-t000008-"))
        .collect();
    assert!(stray.is_empty(), "written into the crate: {stray:?}");
    assert!(
        !project
            .join("shots")
            .join(name.replace("t000007", "t000008"))
            .exists()
    );
    std::fs::remove_dir_all(&project).expect("remove");
}
