// SPDX-License-Identifier: MPL-2.0
//! ADR-7871: request paths through the dispatcher, with an isolated cwd outside the project
//! and the child's temp root. Requests use dispatcher channels; no GPU, sleeps, or mutation of
//! the parent process's environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use crossbeam_channel::{Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use serde_json::{Value, json};

fn isolated(test: &str) -> Option<PathBuf> {
    const CHILD: &str = "REGIA_TEST_REQUEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        return Some(PathBuf::from(root));
    }
    let base = regia_test_utils::unique_temp_dir("agent-request-roots");
    for folder in ["project", "work", "temp"] {
        std::fs::create_dir_all(base.join(folder)).expect("fixture directory");
    }
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", test, "--nocapture"])
        .current_dir(base.join("work"))
        .env(CHILD, &base)
        .env("TMPDIR", base.join("temp"))
        .env("TMP", base.join("temp"))
        .env("TEMP", base.join("temp"))
        .output()
        .expect("child test");
    std::fs::remove_dir_all(&base).expect("cleanup");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

fn app(project: &Path) -> (App, Sender<Inbound>) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.to_path_buf()),
        })
        .add_plugins(AgentPlugin);
    app.finish_plugins();
    let (tx, rx) = unbounded();
    app.insert_resource(AgentInbox(rx));
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            rate_limit_per_second: u32::MAX,
            ..Default::default()
        },
        source: None,
    });
    (app, tx)
}

fn call(app: &mut App, inbox: &Sender<Inbound>, method: &str, params: Value) -> Response {
    let (reply, replies) = unbounded();
    inbox
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply,
        })
        .expect("inbox");
    for _ in 0..2 {
        app.update();
    }
    replies.try_recv().expect("response")
}

fn assert_outside(response: Response) {
    let error = response.error.expect("outside path refused");
    let data = error.data.expect("error data");
    assert_eq!(data["reason"], "path_outside_project");
    assert_eq!(data["path_reason"], "path_escapes");
    if let Some(roots) = data["allowed_roots"].as_array() {
        assert!(!roots.contains(&json!("working_dir")));
    }
}

#[test]
fn mods_mount_cannot_read_the_working_directory_without_an_explicit_root() {
    let Some(base) = isolated("mods_mount_cannot_read_the_working_directory_without_an_explicit_root") else {
        return;
    };
    let work = base.join("work");
    let project = base.join("project");
    std::fs::write(work.join("private.txt"), "outside the project").expect("fixture");
    std::fs::create_dir_all(project.join("local_mod")).expect("local mod");
    std::fs::write(project.join("local_mod/local.txt"), "project content").expect("fixture");
    let (mut app, tx) = app(&project);
    assert_outside(call(
        &mut app,
        &tx,
        "mods.mount",
        json!({"id":"outside", "path":work}),
    ));
    assert!(
        call(&mut app, &tx, "mods.read", json!({"path":"private.txt"}))
            .error
            .is_some()
    );
    assert!(
        call(
            &mut app,
            &tx,
            "mods.mount",
            json!({"id":"local", "path":"local_mod"})
        )
        .error
        .is_none()
    );
    assert_eq!(
        call(&mut app, &tx, "mods.read", json!({"path":"local.txt"}))
            .result
            .expect("read")["text"],
        "project content"
    );
    // An explicit replay root still grants read authority to mods.mount.
    app.world_mut()
        .resource_mut::<AgentPermissions>()
        .config
        .replay_roots
        .push(work.clone());
    assert!(
        call(&mut app, &tx, "mods.mount", json!({"id":"outside", "path":work}))
            .error
            .is_none()
    );
    assert_eq!(
        call(&mut app, &tx, "mods.read", json!({"path":"private.txt"}))
            .result
            .expect("explicit read")["text"],
        "outside the project"
    );
}

#[test]
fn render_out_refuses_the_implicit_working_directory_before_opening_a_gpu() {
    let Some(base) = isolated("render_out_refuses_the_implicit_working_directory_before_opening_a_gpu")
    else {
        return;
    };
    let (mut app, tx) = app(&base.join("project"));
    let out = base.join("work/renders");
    assert_outside(call(
        &mut app,
        &tx,
        "render.out_start",
        json!({"out":out, "frames":1}),
    ));
    assert!(!out.exists(), "a refused request creates nothing");
}

#[test]
fn req_path_requires_an_explicit_working_directory_root() {
    let Some(base) = isolated("req_path_requires_an_explicit_working_directory_root") else {
        return;
    };
    let root = base.join("project");
    let outside = base.join("work/private.txt");
    let mut world = World::new();
    let err = regia_agent::req_path::allowed_path_in(&world, &root, "path", outside.to_str().expect("path"))
        .expect_err("cwd is not allowed");
    let data = err.data.expect("error data");
    assert_eq!(data["reason"], "path_outside_project");
    assert_eq!(data["path_reason"], "path_escapes");
    world.insert_resource(AgentPermissions {
        config: AgentConfig {
            output_roots: vec![base.join("work")],
            ..Default::default()
        },
        source: None,
    });
    assert!(
        regia_agent::req_path::allowed_path_in(&world, &root, "path", outside.to_str().expect("path"))
            .is_ok()
    );
    assert!(regia_agent::req_path::allowed_path_in(&world, &root, "path", "inside.txt").is_ok());
    assert!(
        regia_agent::req_path::allowed_path_in(
            &world,
            &root,
            "path",
            base.join("temp/capture").to_str().expect("path")
        )
        .is_ok()
    );
}

#[test]
fn content_mount_reports_manifest_integrity_failure_through_the_protocol() {
    use regia_asset::content::{ChunkEntry, ContentManager, ContentManifest, Delivery};
    let root = regia_test_utils::TempDir::new("agent-chunk-integrity");
    let content = root.join("content");
    std::fs::create_dir_all(&content).expect("content");
    let replaced = b"replaced";
    std::fs::write(content.join("arena.fpak"), replaced).expect("pak");
    let mut manifest = ContentManifest::new("1.0-1-integrity", "linux");
    manifest.chunks.push(ChunkEntry {
        name: "arena".into(),
        file: "arena.fpak".into(),
        // The manifest's length: a wrong length is a torn install the scan already refuses
        // (`chunk_size_mismatch`, ADR-8810); a same-length file whose BLAKE3 differs is the
        // integrity failure this test reports through the protocol (ADR-7871, ADR-8714).
        size: replaced.len() as u64,
        hash: "0".repeat(64),
        delivery: Delivery::Install,
        requires: Vec::new(),
        overrides: false,
        mount_order: 0,
        assets: 0,
        blocks: Vec::new(),
    });
    let manager = ContentManager::from_manifest(manifest, &content, &root.join("install"));
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    let (mut app, tx) = app(&root);
    app.insert_resource(server).insert_resource(manager);
    let response = call(&mut app, &tx, "content.mount", json!({"chunk":"arena"}));
    let data = response.error.expect("refused").data.expect("error data");
    assert_eq!(data["reason"], "chunk_integrity_mismatch");
    let chunks = call(&mut app, &tx, "content.chunks", json!({}))
        .result
        .expect("chunks");
    assert_eq!(chunks["chunks"][0]["state"], "Failed(hash_mismatch)");
    assert!(
        app.world()
            .resource::<regia_asset::AssetServer>()
            .mounted_pak_names()
            .is_empty()
    );
}
