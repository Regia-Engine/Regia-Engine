// SPDX-License-Identifier: MPL-2.0
//! CP_SIZE shared image identity through the existing registry and cooked-pack agent methods.

#[path = "../../regia_asset_pipeline/tests/support/gltf_shared_project.rs"]
mod fixture;

use crossbeam_channel::unbounded;
use fixture::{Project, image_meta};
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings};
use serde_json::{Value, json};

#[test]
fn registry_and_cooked_pack_report_one_shared_texture_with_both_referencers() {
    let project = Project::new();
    project.cook();
    let pak = project.pack(false);
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: project.0.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("asset server");
    server.mount_chunk_pak(&pak, "base", None).expect("mount");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.0.clone()),
        })
        .add_plugins(AgentPlugin)
        .insert_resource(server);
    app.finish_plugins();
    let (inbox, incoming) = unbounded();
    let (reply, replies) = unbounded();
    app.insert_resource(AgentInbox(incoming));
    let mut call = |method: &str, params: Value| {
        inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: reply.clone(),
            })
            .expect("send request");
        app.update();
        let response = replies.try_recv().expect("immediate read-only response");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    };
    let query = call("registry.query", json!({"type":"CookedTexture","refresh":true}));
    assert_eq!(query["total"], 1, "one texture record: {query}");
    let id = query["entries"][0]["id"].as_str().expect("texture id");
    assert_eq!(id, image_meta(&project.0).id.to_string());
    assert_eq!(project.texture_ids().len(), 1);
    assert_eq!(project.material_texture("a", false).to_string(), id);
    assert_eq!(project.material_texture("b", false).to_string(), id);
    let description = call("registry.describe", json!({"asset":id}));
    assert_eq!(
        description["referencers"].as_array().expect("referencers").len(),
        2,
        "{description}"
    );
    assert_eq!(description["missing_dependencies"], json!([]));
    let by_path = call("assets.texture_coverage", json!({"asset":"assets/shared.png"}));
    let by_id = call("assets.texture_coverage", json!({"asset":id}));
    assert_eq!(
        by_path["levels"], by_id["levels"],
        "shared path and current id inspect the same image"
    );
    assert!(!by_path["levels"].as_array().expect("mip coverage").is_empty());
    let pack = call("assets.cooked_pack", json!({"inspect":true}));
    assert_eq!(pack["paks"][0]["textures"], 1, "{pack}");
    assert_eq!(pack["errors"], 0, "{pack}");
}

#[test]
fn texture_coverage_schema_distinguishes_shared_external_and_private_embedded_images() {
    let schema = regia_agent::schema::method("assets.texture_coverage").expect("coverage method");
    let asset = schema
        .params
        .iter()
        .find(|p| p.name == "asset")
        .expect("asset parameter");
    assert!(
        asset.description.contains("external") && asset.description.contains("embedded"),
        "coverage help must distinguish shared external image paths from private embedded image paths: {}",
        asset.description
    );
}
