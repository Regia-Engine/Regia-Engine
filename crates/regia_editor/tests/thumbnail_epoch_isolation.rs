// SPDX-License-Identifier: MPL-2.0
//! Dogfood the existing thumbnail surface through regia_agent's real dispatcher, without a GPU.
//! Epoch delivery control remains private to thumb_cache's unit tests.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::thumb_cache::ThumbCache;
use serde_json::{Value, json};

fn call(app: &mut App, inbox: &Sender<Inbound>, id: u64, params: Value) -> Response {
    let (reply, answer) = crossbeam_channel::bounded(1);
    inbox
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(id)),
                method: "editor.asset_thumbnails".into(),
                params,
            },
            reply,
        })
        .expect("dispatcher inbox");
    app.update();
    answer.try_recv().expect("answered within one explicit frame")
}

#[test]
fn existing_thumbnail_protocol_reports_cpu_success_and_lowercase_errors() {
    use image::ImageEncoder as _;

    let root = regia_test_utils::unique_temp_dir("thumbnail_epoch_protocol");
    std::fs::create_dir_all(root.join("assets")).expect("fixture assets directory");
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &[0, 0, 255, 255].repeat(16),
            4,
            4,
            image::ExtendedColorType::Rgba8,
        )
        .expect("tiny CPU fixture");
    regia_test_utils::write_atomic(&root.join("assets/blue.png"), &bytes).expect("atomic fixture");
    regia_test_utils::write_atomic(&root.join("assets/broken.png"), b"invalid png")
        .expect("atomic broken fixture");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    let mut extension = regia_agent::extension::AgentExtension::new();
    for (schema, handler) in regia_editor::thumb_cache::methods() {
        extension.register(schema, handler);
    }
    app.insert_resource(extension);
    app.insert_resource(ThumbCache::new(16, 4));
    app.finish_plugins();
    app.update();
    let (inbox, requests) = crossbeam_channel::unbounded();
    app.insert_resource(regia_agent::AgentInbox(requests));
    let mut permissions = regia_agent::AgentPermissions::default();
    permissions.config.rate_limit_per_second = 10_000;
    app.insert_resource(permissions);

    let response = call(&mut app, &inbox, 1, json!({ "paths": ["assets/blue.png"] }));
    assert_eq!(
        response.error.expect("no project").data.expect("data")["thumb_error"],
        "thumb_no_project"
    );
    app.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    for (id, params, expected) in [
        (2, json!({ "paths": [] }), "thumb_bad_params"),
        (3, json!({ "paths": ["assets/../outside.png"] }), "thumb_bad_path"),
    ] {
        let response = call(&mut app, &inbox, id, params);
        assert_eq!(
            response.error.expect("refused").data.expect("data")["thumb_error"],
            expected
        );
    }
    let params = json!({ "paths": ["assets/blue.png", "assets/broken.png"] });
    let made = call(&mut app, &inbox, 4, params.clone()).result.expect("success");
    assert_eq!(made["thumbnails"][0]["state"], "ready");
    assert_eq!(made["thumbnails"][0]["origin"], "made");
    assert_eq!(made["thumbnails"][1]["state"], "failed");
    assert_eq!(made["stats"]["made"], 1);
    assert_eq!(made["stats"]["failed"], 1);
    assert_eq!(made["stats"]["in_flight"], 0);
    let memory = call(&mut app, &inbox, 5, params).result.expect("memory hit");
    assert_eq!(memory["stats"]["made"], 1);
    assert_eq!(memory["stats"]["failed"], 1);
    assert_eq!(memory["stats"]["memory_hits"], 1);
    let disk = call(
        &mut app,
        &inbox,
        6,
        json!({ "paths": ["assets/blue.png"], "clear_memory": true }),
    )
    .result
    .expect("disk hit");
    assert_eq!(disk["thumbnails"][0]["origin"], "disk");
    assert_eq!(disk["stats"]["disk_hits"], 1);
    let cache = app.world().resource::<ThumbCache>();
    let picture = cache.image("assets/blue.png").expect("CPU image");
    assert_eq!(picture.pixels[8 * 16 + 8], egui::Color32::BLUE);
    assert!(!cache.busy());
    drop(app);
    std::fs::remove_dir_all(root).expect("remove fixture");
}
