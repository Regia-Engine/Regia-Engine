// SPDX-License-Identifier: MPL-2.0
//! Editor package-hygiene policy, waivers and audit review (ADR-8399).
//!
//! Human edits call [`regia_editor::hygiene_policy::commit`]. Agent edits call
//! `editor.package_hygiene_set` through the real dispatcher. Both write with
//! `commit_loaded`. The audit tests use a channel-gated worker so cancellation
//! cannot race a late clean publish.

use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use serde_json::{Value, json};

const SECRET: &str = "sk-PLANTEDSECRETVALUE0001";

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next: u64,
}

impl Client {
    fn new(root: &Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        app.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
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

    fn allow_build(&mut self) {
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.permissions.insert("build".to_owned());
        self.app.world_mut().insert_resource(permissions);
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
        if answer.is_empty() {
            self.app.world_mut().run_schedule(Last);
        }
        let response = answer.try_recv().expect("reply in one frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            _ => panic!("missing response"),
        }
    }
}

fn write_atomic(path: &Path, text: &str) {
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text.as_bytes()).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

fn project(name: &str, body: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(name);
    write_atomic(&root.join("Regia.toml"), body);
    root
}

fn base_manifest() -> &'static str {
    "# keep this comment\n\
     [project]\n\
     name = \"Hygiene\"\n\
     id = \"com.example.hygiene\"\n\
     studio = \"north\"\n\
     \n\
     [steam]\n\
     app_id = 1\n\
     \n\
     [package_hygiene]\n\
     note = \"keep\"\n\
     forbidden_strings = [\"keep-me\"]\n"
}

fn reason(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn policy_patch() -> Value {
    json!({
        "rules": [{ "id": "dev_console", "kind": "string", "body": "keep-out" }],
        "allowances": [
            { "rule": "dev_console", "path": "notes/*.txt", "reason": "Reviewed sample" },
            { "rule": "secret_api_key", "path": "docs/*.md", "reason": "Public sample token" }
        ],
        "thresholds": { "max_entries": 1000 },
        "target": { "scan_names": true, "scan_secrets": true, "scan_emails": false },
        "save": true
    })
}

#[test]
fn human_and_agent_edits_write_the_same_policy() {
    let manifest = "\
[project]\n\
name = \"Hygiene\"\n\
id = \"com.example.hygiene\"\n\
studio = \"north\"\n\
\n\
[steam]\n\
app_id = 1\n";
    let human_root = project("hygiene-human", manifest);
    let agent_root = project("hygiene-agent", manifest);
    let patch = policy_patch();
    let mut world = World::new();
    let human = regia_editor::hygiene_policy::commit(&mut world, &human_root, &patch, true).expect("human");
    assert_eq!(human["saved"], true);
    assert_eq!(
        human["allowances"][0]["definition"],
        regia_editor::hygiene_policy::definition_digest("string", "keep-out")
    );
    assert_eq!(
        human["allowances"][1]["definition"],
        regia_editor::hygiene_policy::definition_digest("builtin", "secret_api_key")
    );
    assert_eq!(human["provenance"]["adr"], "ADR-8399");
    let mut agent = Client::new(&agent_root);
    let edited = agent.call("editor.package_hygiene_set", patch).expect("agent");
    assert_eq!(edited["saved"], true);
    let human_text = std::fs::read_to_string(human_root.join("Regia.toml")).expect("human file");
    let agent_text = std::fs::read_to_string(agent_root.join("Regia.toml")).expect("agent file");
    assert_eq!(human_text, agent_text);
    assert!(human_text.contains("dev_console"));
    assert!(human_text.contains("studio"));
    assert!(agent_text.contains("app_id"));
    let undone = agent.call("editor.undo", json!({})).expect("undo");
    assert_eq!(undone["undone"], true);
    assert_eq!(undone["description"], "Edit package hygiene policy");
    assert_eq!(
        std::fs::read_to_string(agent_root.join("Regia.toml")).expect("restored"),
        manifest
    );
    let preview_root = project("hygiene-preview", manifest);
    let mut preview_world = World::new();
    let mut preview_patch = policy_patch();
    preview_patch["save"] = json!(false);
    let preview =
        regia_editor::hygiene_policy::commit(&mut preview_world, &preview_root, &preview_patch, false)
            .expect("preview");
    assert_eq!(preview["saved"], false);
    assert_eq!(preview["rules"][0]["id"], "dev_console");
    assert_eq!(
        std::fs::read_to_string(preview_root.join("Regia.toml")).expect("preview file"),
        manifest
    );
    std::fs::remove_dir_all(human_root).expect("cleanup");
    std::fs::remove_dir_all(agent_root).expect("cleanup");
    std::fs::remove_dir_all(preview_root).expect("cleanup");
}

#[test]
fn invalid_rules_and_allowances_leave_the_manifest_unchanged() {
    let root = project("hygiene-invalid", base_manifest());
    let before = std::fs::read(root.join("Regia.toml")).expect("read");
    let mut client = Client::new(&root);
    let cases = [
        (
            json!({"rules": [{"id": "bad-id", "kind": "string", "body": "x"}]}),
            "hygiene_rule_id",
        ),
        (
            json!({"rules": [
                {"id": "same_rule", "kind": "string", "body": "one"},
                {"id": "same_rule", "kind": "term", "body": "two"}
            ]}),
            "hygiene_duplicate_rule",
        ),
        (
            json!({"rules": [{"id": "secret_api_key", "kind": "string", "body": "x"}]}),
            "hygiene_duplicate_rule",
        ),
        (
            json!({"rules": [{"id": "dev_console", "kind": "string", "body": "keep-out"}]}),
            "hygiene_rules",
        ),
        (
            json!({"allowances": [{"rule": "secret_api_key", "path": "docs/*.md"}]}),
            "hygiene_allowance",
        ),
        (
            json!({"allowances": [{"rule": "secret_api_key", "path": "docs/*.md", "reason": "  "}]}),
            "hygiene_allowance",
        ),
        (
            json!({"allowances": [{"rule": "secret_api_key", "path": "/etc/passwd", "reason": "Reviewed"}]}),
            "hygiene_allowance_path",
        ),
        (
            json!({"allowances": [{"rule": "secret_api_key", "path": "../secret.txt", "reason": "Reviewed"}]}),
            "hygiene_allowance_path",
        ),
        (
            json!({"thresholds": {"scan_window_bytes": 4096, "max_memory_bytes": 1024}}),
            "hygiene_threshold",
        ),
        (json!({"target": {"platform": "windows"}}), "hygiene_target"),
    ];
    for (mut patch, code) in cases {
        patch["save"] = json!(true);
        let error = client.call("editor.package_hygiene_set", patch).expect_err(code);
        assert_eq!(reason(&error), code, "{code}");
        assert_eq!(std::fs::read(root.join("Regia.toml")).expect("unchanged"), before);
    }
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn read_only_permission_rejects_policy_writes() {
    let root = project("hygiene-readonly", base_manifest());
    let before = std::fs::read(root.join("Regia.toml")).expect("read");
    let mut client = Client::new(&root);
    client
        .app
        .world_mut()
        .insert_resource(regia_agent::AgentPermissions {
            config: regia_agent::AgentConfig {
                read_only: true,
                ..Default::default()
            },
            source: None,
        });
    let view = client
        .call("editor.package_hygiene_get", json!({}))
        .expect("read");
    assert_eq!(view["legacy"]["forbidden_strings"][0], "keep-me");
    let error = client
        .call(
            "editor.package_hygiene_set",
            json!({"thresholds": {"max_entries": 10}}),
        )
        .expect_err("deny");
    assert_eq!(error.data.as_ref().expect("data")["code"], "F0012");
    assert_eq!(std::fs::read(root.join("Regia.toml")).expect("unchanged"), before);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn unrelated_settings_round_trip() {
    let root = project("hygiene-round-trip", base_manifest());
    let mut client = Client::new(&root);
    let edited = client
        .call(
            "editor.package_hygiene_set",
            json!({
                "thresholds": {"max_entries": 1000, "max_findings": 32},
                "target": {"scan_names": false}
            }),
        )
        .expect("set");
    assert_eq!(edited["saved"], true);
    assert_eq!(edited["legacy"]["forbidden_strings"][0], "keep-me");
    assert_eq!(edited["thresholds"]["max_entries"], 1000);
    assert_eq!(edited["target"]["scan_names"], false);
    let text = std::fs::read_to_string(root.join("Regia.toml")).expect("file");
    assert!(text.contains("keep-me"));
    assert!(text.contains("note"));
    assert!(text.contains("studio"));
    assert!(text.contains("app_id"));
    assert!(text.contains("max_entries"));
    assert!(
        !text.contains("# keep this comment"),
        "a valid save reformats; the keys above are the round trip"
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

fn stage(root: &Path) {
    std::fs::create_dir_all(root.join("stage")).expect("stage");
}

struct Gate {
    started: std::sync::mpsc::Receiver<()>,
    gate: std::sync::mpsc::Sender<()>,
    done: std::sync::mpsc::Receiver<()>,
}

fn arm(client: &mut Client, report: Value, open: bool) -> Gate {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    if open {
        gate_tx.send(()).expect("open");
    }
    client
        .app
        .world_mut()
        .insert_resource(regia_editor::hygiene_policy::HygieneAuditInjector::once(
            regia_editor::hygiene_policy::AuditScript {
                started: started_tx,
                gate: gate_rx,
                done: done_tx,
                report,
            },
        ));
    Gate {
        started: started_rx,
        gate: gate_tx,
        done: done_rx,
    }
}

#[test]
fn cancelling_an_audit_stays_responsive_and_drops_a_late_clean_result() {
    let root = project("hygiene-cancel", base_manifest());
    stage(&root);
    let mut client = Client::new(&root);
    client.allow_build();
    let refused = client
        .call("editor.package_hygiene_audit", json!({"audit_dir": "stage"}))
        .expect_err("confirm");
    assert_eq!(reason(&refused), "needs_confirmation");
    let first = arm(
        &mut client,
        json!({
            "ok": true,
            "hygiene": {"findings": [], "complete": true, "policy_digest": "b3:abc"},
            "receipt": {"artifact": "b3:deadbeef", "status": "bound"}
        }),
        true,
    );
    let started = client
        .call(
            "editor.package_hygiene_audit",
            json!({"audit_dir": "stage", "confirm": true, "platform": "linux"}),
        )
        .expect("start");
    assert_eq!(started["clean"], false);
    assert_eq!(started["receipt"]["status"], "absent");
    first.started.recv().expect("started");
    first.done.recv().expect("finished");
    let status = client
        .call("editor.package_hygiene_audit_status", json!({}))
        .expect("clean status");
    assert_eq!(status["state"], "done");
    assert_eq!(status["verdict"], "stage_clean");
    assert_eq!(status["clean"], true);
    assert_eq!(status["receipt"]["status"], "present");
    assert_eq!(status["receipt"]["receipt"]["artifact"], "b3:deadbeef");

    let second = arm(
        &mut client,
        json!({
            "ok": true,
            "hygiene": {"findings": [], "complete": true},
            "receipt": {"artifact": "b3:stale"}
        }),
        false,
    );
    let running = client
        .call(
            "editor.package_hygiene_audit",
            json!({"audit_dir": "stage", "confirm": true}),
        )
        .expect("second");
    assert_eq!(running["state"], "running");
    assert_eq!(running["clean"], false);
    second.started.recv().expect("blocked");
    let status = client
        .call("editor.package_hygiene_audit_status", json!({}))
        .expect("running status");
    assert_eq!(status["state"], "running");
    assert_eq!(status["clean"], false);
    assert_eq!(status["receipt"]["status"], "absent");
    assert!(status["findings"].as_array().expect("findings").is_empty());
    let settings = client
        .call("editor.project_settings_get", json!({}))
        .expect("responsive");
    assert_eq!(settings["project"]["name"], "Hygiene");
    let cancelled = client
        .call("editor.package_hygiene_audit_cancel", json!({}))
        .expect("cancel");
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["clean"], false);
    assert_eq!(cancelled["receipt"]["status"], "absent");
    second.gate.send(()).expect("release");
    second.done.recv().expect("worker finished");
    let status = client
        .call("editor.package_hygiene_audit_status", json!({}))
        .expect("after cancel");
    assert_eq!(status["state"], "cancelled");
    assert_eq!(status["verdict"], "cancelled");
    assert_eq!(status["clean"], false);
    assert_eq!(status["receipt"]["status"], "absent");
    assert!(status["findings"].as_array().expect("findings").is_empty());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn redacted_location_selection_never_copies_the_raw_secret_path() {
    let root = project("hygiene-redacted", base_manifest());
    stage(&root);
    let mut client = Client::new(&root);
    client.allow_build();
    let raw = format!("/tmp/{SECRET}");
    let gate = arm(
        &mut client,
        json!({
            "ok": false,
            "hygiene": {
                "complete": true,
                "findings": [{
                    "path": "[redacted]",
                    "location_id": "location-1",
                    "rule": "secret_api_key",
                    "source": "content",
                    "offset": 12,
                    "raw_path": raw
                }]
            }
        }),
        true,
    );
    client
        .call(
            "editor.package_hygiene_audit",
            json!({"audit_dir": "stage", "confirm": true}),
        )
        .expect("start");
    gate.started.recv().expect("started");
    gate.done.recv().expect("done");
    let status = client
        .call("editor.package_hygiene_audit_status", json!({}))
        .expect("status");
    let encoded = serde_json::to_string(&status).expect("json");
    assert!(!encoded.contains(SECRET));
    assert_eq!(status["clean"], false);
    assert_eq!(status["verdict"], "stage_rejected");
    assert_eq!(status["findings"][0]["path"], "[redacted]");
    assert_eq!(status["findings"][0]["location_id"], "location-1");
    assert_eq!(status["findings"][0]["rule"], "secret_api_key");
    assert!(status["findings"][0].get("raw_path").is_none());
    let selected = client
        .app
        .world()
        .resource::<regia_editor::asset_panels::AssetPanels>()
        .problems
        .selected_text()
        .expect("selection");
    assert!(!selected.contains(SECRET));
    assert!(selected.contains("location-1"));
    assert!(selected.contains("secret_api_key"));
    let copied = client
        .app
        .world()
        .resource::<regia_editor::asset_panels::AssetPanels>()
        .problems
        .copy_all_text();
    assert!(!copied.contains(SECRET));
    let condition = json!({"events_contain": {"kind": "hygiene_review", "contains": "location-1"}});
    let waited = client
        .call(
            "play.wait_until",
            json!({"condition": condition, "timeout_ms": 120000}),
        )
        .expect("wait");
    assert_eq!(waited["satisfied"], true);
    let expected = client
        .call(
            "play.expect",
            json!({"assertions": [
                {"name": "opaque id", "condition": condition},
                {"name": "no planted secret", "condition": {"not": {"events_contain": {"kind": "hygiene_review", "contains": SECRET}}}}
            ]}),
        )
        .expect("expect");
    assert_eq!(expected["pass"], true);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn audit_status_keeps_at_most_128_findings() {
    let root = project("hygiene-cap", base_manifest());
    stage(&root);
    let mut client = Client::new(&root);
    client.allow_build();
    let mut findings = Vec::new();
    for index in 0..200 {
        findings.push(json!({
            "path": "notes/item.txt",
            "location_id": format!("location-{index}"),
            "rule": "dev_console",
            "source": "content",
            "offset": index
        }));
    }
    let gate = arm(
        &mut client,
        json!({"ok": false, "hygiene": {"complete": true, "findings": findings}}),
        true,
    );
    client
        .call(
            "editor.package_hygiene_audit",
            json!({"audit_dir": "stage", "confirm": true}),
        )
        .expect("start");
    gate.done.recv().expect("done");
    let status = client
        .call("editor.package_hygiene_audit_status", json!({}))
        .expect("status");
    assert_eq!(
        status["findings"].as_array().expect("rows").len(),
        regia_editor::hygiene_policy::MAX_COPIED_FINDINGS
    );
    assert_eq!(status["truncated"], true);
    assert_eq!(status["clean"], false);
    assert_eq!(status["verdict"], "incomplete");
    std::fs::remove_dir_all(root).expect("cleanup");
}
