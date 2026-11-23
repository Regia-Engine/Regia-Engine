// SPDX-License-Identifier: MPL-2.0
//! `anim.gait_validate` (gap G7) through the dispatcher: a `.gait.ron` in a project by path, the same
//! text inline, and a broken file whose errors carry the loader's codes. Fails on the base, which has
//! no such method.

use regia_agent::{AgentSettings, LocalReply, call_local};
use regia_app::prelude::App;
use serde_json::{Value, json};

const GOOD: &str = include_str!("fixtures/format-corpus/gait/v1.ron");

fn call(app: &mut App, params: Value) -> Result<Value, String> {
    match call_local(
        app.world_mut(),
        "anim.gait_validate",
        params,
        Some("gait-asset-test"),
    ) {
        LocalReply::Done(Ok(value)) => Ok(value),
        LocalReply::Done(Err(error)) => Err(format!("{error:?}")),
        reply => panic!("anim.gait_validate: {reply:?}"),
    }
}

fn write_atomic(path: &std::path::Path, text: &str) {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, text).expect("write");
    std::fs::rename(&temp, path).expect("rename");
}

#[test]
fn gait_files_are_validated_through_dispatcher() {
    let project = regia_test_utils::unique_temp_dir("gait-validate");
    std::fs::create_dir_all(project.join("assets/units")).expect("dir");
    write_atomic(&project.join("assets/units/walker.gait.ron"), GOOD);
    let bad = GOOD.replacen("clearance: 0.7", "clearance: -0.7", 1);
    write_atomic(&project.join("assets/units/broken.gait.ron"), &bad);
    let mut app = App::new();
    app.insert_resource(AgentSettings {
        project_dir: Some(project.clone()),
    });

    let ok = call(&mut app, json!({ "path": "assets/units/walker.gait.ron" })).expect("answers");
    assert_eq!(ok["path"], "assets/units/walker.gait.ron");
    assert_eq!(ok["ok"], true, "{ok}");
    assert_eq!(ok["format_version"], 1);
    assert_eq!(ok["legs"], 6);
    assert_eq!(ok["patterns"], 1);
    assert_eq!(ok["errors"], json!([]));

    let broken = call(&mut app, json!({ "path": "assets/units/broken.gait.ron" })).expect("answers");
    assert_eq!(broken["ok"], false);
    assert_eq!(broken["errors"][0]["code"], "gait_settings", "{broken}");

    // Inline text wins over a path and reports the same codes; a newer file is gait_version.
    let newer = GOOD.replacen("format_version: 1", "format_version: 2", 1);
    let inline = call(
        &mut app,
        json!({ "text": newer, "path": "assets/units/walker.gait.ron" }),
    )
    .expect("answers");
    assert_eq!(inline["path"], Value::Null);
    assert_eq!(inline["errors"][0]["code"], "gait_version", "{inline}");

    // A path outside the project, a missing file and no parameter are RPC errors.
    assert!(call(&mut app, json!({ "path": "../outside.gait.ron" })).is_err());
    assert!(call(&mut app, json!({ "path": "assets/units/none.gait.ron" })).is_err());
    assert!(call(&mut app, json!({})).is_err());
    let _ = std::fs::remove_dir_all(&project);
}
