// SPDX-License-Identifier: MPL-2.0
//! C-05 through the real editor agent dispatcher, including permission and path checks.
use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next: u64,
}
impl Client {
    fn new(root: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, receiver) = crossbeam_channel::unbounded();
        app.world_mut().insert_resource(regia_agent::AgentInbox(receiver));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self { app, inbox, next: 0 }
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(self.next)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("send");
        self.app.update();
        let response = answer.try_recv().expect("reply in one frame");
        match (response.result, response.error) {
            (_, Some(e)) => Err(e),
            (Some(v), None) => Ok(v),
            _ => panic!("missing response"),
        }
    }
}

fn fixture(root: &std::path::Path) -> Vec<u8> {
    std::fs::create_dir_all(root).expect("root");
    let bytes=serde_json::to_vec(&json!({"asset":{"version":"2.0"},"materials":[{
        "pbrMetallicRoughness":{"metallicRoughnessTexture":{"index":0},"metallicFactor":1.0,"roughnessFactor":0.2}}]})).expect("json");
    std::fs::write(root.join("mesh.gltf"), &bytes).expect("fixture");
    bytes
}

#[test]
fn agent_check_fix_and_recheck_reach_the_source_metadata() {
    let root = regia_test_utils::unique_temp_dir("material-agent");
    let before = fixture(&root);
    let mut c = Client::new(&root);
    let hello = c.call("agent.hello", json!({})).expect("discover");
    assert!(
        hello["capabilities"]
            .as_array()
            .expect("capabilities")
            .contains(&json!("gltf_material_lint"))
    );
    let settings = json!({"path":"mesh.gltf","settings":{"profile":"compatibility"}});
    let check = c
        .call("asset.gltf_material_lint", settings.clone())
        .expect("check");
    assert_eq!(check["profile"], "compatibility");
    assert_eq!(check["ok"], false);
    assert_eq!(check["changed"], false);
    assert_eq!(check["findings"][0]["code"], "orm_factors");
    assert_eq!(check["findings"][0]["effect"], "changes_appearance");
    assert_eq!(std::fs::read(root.join("mesh.gltf")).expect("read"), before);
    let fix = c.call("asset.gltf_material_fix", settings.clone()).expect("fix");
    assert_eq!(fix["ok"], true);
    assert_eq!(fix["changed"], true);
    let check = c
        .call("asset.gltf_material_lint", settings.clone())
        .expect("check");
    assert_eq!(check["findings"], json!([]));
    let fix = c.call("asset.gltf_material_fix", settings).expect("fix twice");
    assert_eq!(fix["changed"], false);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn read_only_client_cannot_fix_or_smuggle_fix_into_check() {
    let root = regia_test_utils::unique_temp_dir("material-agent-readonly");
    let before = fixture(&root);
    let mut c = Client::new(&root);
    c.app.world_mut().insert_resource(regia_agent::AgentPermissions {
        config: regia_agent::AgentConfig {
            read_only: true,
            ..Default::default()
        },
        source: None,
    });
    assert_eq!(
        c.call(
            "asset.gltf_material_lint",
            json!({"path":"mesh.gltf","settings":{"profile":"compatibility"}})
        )
        .expect("read-only inspection")["ok"],
        false
    );
    let e = c
        .call("asset.gltf_material_fix", json!({"path":"mesh.gltf"}))
        .expect_err("deny mutation");
    assert_eq!(e.data.as_ref().expect("data")["code"], "F0012");
    assert!(
        c.call("asset.gltf_material_lint", json!({"path":"mesh.gltf","fix":true}))
            .is_err()
    );
    assert_eq!(std::fs::read(root.join("mesh.gltf")).expect("read"), before);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn agent_rejects_escape_bad_policy_and_malformed_source_with_stable_reasons() {
    let root = regia_test_utils::unique_temp_dir("material-agent-errors");
    fixture(&root);
    let mut c = Client::new(&root);
    for (params, reason) in [
        (json!({"path":"../escape.gltf"}), "path_outside_project"),
        (json!({}), "gltf_material_params"),
        (
            json!({"path":"mesh.gltf","settings":{"unknown":true}}),
            "gltf_material_settings",
        ),
        (
            json!({"path":"mesh.gltf","settings":{"profile":"game"}}),
            "gltf_material_settings",
        ),
        (json!({"path":"missing.gltf"}), "gltf_material_io"),
    ] {
        let e = c.call("asset.gltf_material_lint", params).expect_err("refused");
        assert_eq!(e.data.as_ref().expect("data")["reason"], reason);
    }
    std::fs::write(root.join("mesh.gltf"), b"{broken").expect("invalid");
    let e = c
        .call("asset.gltf_material_fix", json!({"path":"mesh.gltf"}))
        .expect_err("invalid source");
    assert_eq!(e.data.as_ref().expect("data")["reason"], "gltf_material_invalid");
    assert_eq!(std::fs::read(root.join("mesh.gltf")).expect("read"), b"{broken");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn agent_default_profile_preserves_valid_textured_factors() {
    let root = regia_test_utils::unique_temp_dir("material-agent-preserve");
    std::fs::create_dir_all(&root).expect("root");
    let bytes = serde_json::to_vec(&json!({"asset":{"version":"2.0"},"materials":[{
        "pbrMetallicRoughness":{"metallicRoughnessTexture":{"index":0},"metallicFactor":0.35,"roughnessFactor":0.7}
    }]})).expect("json");
    std::fs::write(root.join("mesh.gltf"), &bytes).expect("fixture");
    let mut c = Client::new(&root);
    let check = c
        .call("asset.gltf_material_lint", json!({"path":"mesh.gltf"}))
        .expect("check");
    assert_eq!(check["ok"], true);
    assert_eq!(check["changed"], false);
    assert_eq!(check["profile"], "standard");
    assert_eq!(check["findings"], json!([]));
    assert_eq!(std::fs::read(root.join("mesh.gltf")).expect("read"), bytes);
    let fix = c
        .call("asset.gltf_material_fix", json!({"path":"mesh.gltf"}))
        .expect("fix");
    assert_eq!(fix["ok"], true);
    assert_eq!(fix["changed"], false);
    assert_eq!(fix["profile"], "standard");
    assert_eq!(std::fs::read(root.join("mesh.gltf")).expect("read"), bytes);
    let again = c
        .call("asset.gltf_material_fix", json!({"path":"mesh.gltf"}))
        .expect("fix twice");
    assert_eq!(again["changed"], false);
    std::fs::remove_dir_all(root).expect("cleanup");
}
