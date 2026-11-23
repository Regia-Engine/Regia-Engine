// SPDX-License-Identifier: MPL-2.0
//! `plugin.*` and extension-method routing through the dispatcher (`specs/57` § 8 test 10):
//! list, info, check, enable and disable round trips against a fixture project, the `build`
//! permission, `method_unavailable`, the schema rows. In-process channels; no socket, no GPU.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::extension::discover::MANIFEST_FILE;
use regia_framework::extension::prelude::*;
use regia_framework::extension::{ExtensionPlugin, ExtensionRegistry};
use serde_json::{Value, json};

use crate::dispatch::{AgentInbox, RateLimit, process_requests};
use crate::protocol::{self, Request, Response};
use crate::safety::{AgentConfig, AgentPermissions, AgentSession, AuditLog};
use crate::server::Inbound;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

fn manifest(project: &Path, id: &str, body: &str) {
    write(
        &project.join("extensions").join(id).join(MANIFEST_FILE),
        &format!("format_version = 1\nid = \"{id}\"\nversion = \"1.2.0\"\npublisher = \"Fixture\"\n{body}"),
    );
}

const CODE: &str = "[[code]]\ncrate = \"x\"\naudience = [\"game\"]\n";

#[derive(Default)]
struct Basic;

/// Counts how often `fixture_basic.hello`'s handler ran (proves a refused call never reached it).
#[derive(Resource, Default)]
struct HelloCalls(u32);

impl RegiaExtension for Basic {
    fn id(&self) -> &str {
        "fixture_basic"
    }

    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.register_agent_method(AgentMethodDesc {
            name: "fixture_basic.hello".into(),
            summary: "Says hello.".into(),
            access: MethodAccess::Read,
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "fail": { "type": "boolean" } }
            }),
            result_schema: json!({ "type": "object" }),
            handler: Arc::new(|world: &mut World, params: Value| {
                if let Some(mut calls) = world.get_resource_mut::<HelloCalls>() {
                    calls.0 += 1;
                }
                if params.get("fail").is_some() {
                    return Err(AgentFault {
                        code: "fixture_failed".into(),
                        data: json!({ "why": "asked" }),
                    });
                }
                Ok(json!({ "reply": "hello" }))
            }),
        })?;
        cx.register_agent_method(AgentMethodDesc {
            name: "fixture_basic.poke".into(),
            summary: "Mutates.".into(),
            access: MethodAccess::EditScene,
            params_schema: json!({ "type": "object" }),
            result_schema: json!({ "type": "object" }),
            handler: Arc::new(|_world: &mut World, _params: Value| Ok(json!({ "poked": true }))),
        })
    }
}

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn project(tag: &str) -> Project {
    let dir = regia_test_utils::unique_temp_dir(tag);
    write(
        &dir.join("Regia.toml"),
        "# keep me\n[project]\nname = \"fixture\"\n\n[extensions]\nenabled = [\"fixture_basic\"]\n\n[extensions.grants]\nfixture_basic = [\"agent_method\"]\n",
    );
    manifest(
        &dir,
        "fixture_basic",
        &format!("capabilities = [\"agent_method\"]\n{CODE}"),
    );
    manifest(
        &dir,
        "fixture_broken",
        &format!("capabilities = [\"agent_method\"]\n{CODE}"),
    );
    manifest(
        &dir,
        "fixture_review",
        &format!("capabilities = [\"agent_method\", \"process\", \"systems\"]\n{CODE}"),
    );
    manifest(&dir, "fixture_content", "[[content]]\nroot = \"content\"\n");
    write(&dir.join("extensions/fixture_content/content/a.txt"), "a");
    Project(dir)
}

fn app(project: &Path, permissions: &[&str]) -> App {
    app_with(project, permissions, vec![Arc::new(Basic)])
}

fn app_with(project: &Path, permissions: &[&str], linked: Vec<Arc<dyn RegiaExtension>>) -> App {
    let config = AgentConfig {
        permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        ..AgentConfig::default()
    };
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentPermissions { config, source: None })
        .init_resource::<AuditLog>()
        .init_resource::<AgentSession>()
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>();
    let mut plugin = ExtensionPlugin::for_project(project.to_path_buf(), linked);
    plugin.reserved_ids = Some(crate::plugins::reserved_namespaces());
    app.add_plugins(plugin);
    app.update();
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    drop(inbox_tx);
    rx.recv_timeout(Duration::from_secs(2)).expect("answered")
}

fn ok(app: &mut App, method: &str, params: Value) -> Value {
    let response = call(app, method, params);
    response
        .result
        .unwrap_or_else(|| panic!("{method}: {:?}", response.error))
}

fn data_code(response: &Response) -> String {
    let error = response.error.as_ref().expect("refused");
    error.data.as_ref().expect("data")["code"]
        .as_str()
        .expect("code")
        .to_owned()
}

const BUILD: &[&str] = &["read", "edit_scene", "play", "assets", "build"];

#[test]
fn list_and_info_read_under_the_read_profile() {
    let p = project("plugin-list");
    let mut app = app(&p.0, &["read"]);
    let list = ok(&mut app, "plugin.list", json!({}));
    let states: Vec<(String, String)> = list["extensions"]
        .as_array()
        .expect("list")
        .iter()
        .map(|e| {
            (
                e["id"].as_str().expect("id").to_owned(),
                e["state"].as_str().expect("state").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        states,
        [
            ("fixture_basic".to_owned(), "active".to_owned()),
            ("fixture_broken".to_owned(), "available".to_owned()),
            ("fixture_content".to_owned(), "available".to_owned()),
            ("fixture_review".to_owned(), "available".to_owned()),
        ]
    );
    assert!(!list["resolved_hash"].as_str().expect("hash").is_empty());
    let active = ok(&mut app, "plugin.list", json!({ "state": "active" }));
    assert_eq!(active["extensions"].as_array().expect("list").len(), 1);
    assert!(
        call(&mut app, "plugin.list", json!({ "state": "odd" }))
            .error
            .is_some()
    );

    let info = ok(&mut app, "plugin.info", json!({ "id": "fixture_basic" }));
    assert_eq!(info["publisher"], "Fixture");
    assert_eq!(info["requested"], json!(["agent_method"]));
    assert_eq!(info["granted"][0]["grant"], "explicit");
    assert_eq!(
        info["registrations"]["agent_method"],
        json!(["fixture_basic.hello", "fixture_basic.poke"])
    );
    assert_eq!(info["build"]["state"], "built");
    assert!(info["trust"].as_str().expect("trust").contains("not a sandbox"));

    let missing = call(&mut app, "plugin.info", json!({ "id": "nothing" }));
    assert_eq!(data_code(&missing), "not_found");
}

#[test]
fn check_is_a_dry_run() {
    let p = project("plugin-check");
    let before = std::fs::read_to_string(p.0.join("Regia.toml")).expect("read");
    let mut app = app(&p.0, &["read"]);
    let hash = ok(&mut app, "plugin.list", json!({}))["resolved_hash"].clone();
    let dry = ok(
        &mut app,
        "plugin.check",
        json!({ "enabled": ["fixture_basic", "fixture_review"], "platform": "android", "profile": "shipping" }),
    );
    let codes: Vec<&str> = dry["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"capability_not_granted"), "{dry}");
    assert_eq!(dry["platform"], "android");
    assert_eq!(dry["states"]["fixture_review"], "failed");
    assert_eq!(ok(&mut app, "plugin.list", json!({}))["resolved_hash"], hash);
    assert_eq!(
        std::fs::read_to_string(p.0.join("Regia.toml")).expect("read"),
        before
    );
    assert!(
        call(&mut app, "plugin.check", json!({ "platform": "dos" }))
            .error
            .is_some()
    );
}

#[test]
fn enable_needs_build_and_grants_and_keeps_the_file() {
    let p = project("plugin-enable");
    let mut reader = app(&p.0, &["read", "edit_scene"]);
    let denied = call(&mut reader, "plugin.enable", json!({ "id": "fixture_review" }));
    assert_eq!(
        denied.error.as_ref().expect("denied").code,
        protocol::PERMISSION_DENIED
    );
    assert_eq!(data_code(&denied), "F0012");

    let mut app = app(&p.0, BUILD);
    let ungranted = call(&mut app, "plugin.enable", json!({ "id": "fixture_review" }));
    assert_eq!(data_code(&ungranted), "capability_not_granted");
    assert_eq!(
        ungranted
            .error
            .as_ref()
            .expect("error")
            .data
            .as_ref()
            .expect("data")["missing"],
        json!(["agent_method", "process"]),
        "systems is implicit"
    );
    let bad = call(
        &mut app,
        "plugin.enable",
        json!({ "id": "fixture_review", "grant": ["root"] }),
    );
    assert_eq!(data_code(&bad), "unknown_capability");
    let absent = call(&mut app, "plugin.enable", json!({ "id": "nothing" }));
    assert_eq!(data_code(&absent), "not_found");

    let enabled = ok(
        &mut app,
        "plugin.enable",
        json!({ "id": "fixture_review", "grant": ["agent_method", "process"] }),
    );
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["restart_required"], true, "code needs a restart");
    assert_eq!(enabled["state_after_restart"], "active");
    let text = std::fs::read_to_string(p.0.join("Regia.toml")).expect("read");
    assert!(text.starts_with("# keep me\n"), "{text}");
    let config = regia_framework::extension::ExtensionsConfig::from_project_toml(&text).expect("parse");
    assert_eq!(config.enabled, ["fixture_basic", "fixture_review"]);
    assert_eq!(
        config.grant_map().get("fixture_review").expect("grant"),
        &["agent_method".to_owned(), "process".to_owned()]
    );
    let list = ok(&mut app, "plugin.list", json!({}));
    assert_eq!(list["restart_required"], json!(["fixture_review"]));

    let disabled = ok(&mut app, "plugin.disable", json!({ "id": "fixture_basic" }));
    assert_eq!(disabled["disabled"], true);
    assert_eq!(disabled["restart_required"], true);
    let text = std::fs::read_to_string(p.0.join("Regia.toml")).expect("read");
    let config = regia_framework::extension::ExtensionsConfig::from_project_toml(&text).expect("parse");
    assert!(config.disabled.contains(&"fixture_basic".to_owned()));
}

#[test]
fn content_extensions_mount_and_unmount_live() {
    let p = project("plugin-content");
    let mut app = app(&p.0, BUILD);
    let enabled = ok(&mut app, "plugin.enable", json!({ "id": "fixture_content" }));
    assert_eq!(enabled["restart_required"], false);
    assert_eq!(enabled["mounted"][0]["files"], 1);
    let hit = app
        .world()
        .resource::<ExtensionRegistry>()
        .mounts()
        .resolve("ext://fixture_content/a.txt", None)
        .expect("mounted");
    assert_eq!(hit.origin, "extension:fixture_content");

    let disabled = ok(&mut app, "plugin.disable", json!({ "id": "fixture_content" }));
    assert_eq!(disabled["restart_required"], false);
    assert_eq!(disabled["unmounted"], 1);
    assert!(
        app.world()
            .resource::<ExtensionRegistry>()
            .mounts()
            .resolve("ext://fixture_content/a.txt", None)
            .is_none()
    );
}

#[test]
fn extension_methods_route_with_their_provider() {
    let p = project("plugin-methods");
    let mut app = app(&p.0, &["read", "play"]);
    assert_eq!(ok(&mut app, "fixture_basic.hello", json!({}))["reply"], "hello");

    let fault = call(&mut app, "fixture_basic.hello", json!({ "fail": true }));
    assert_eq!(data_code(&fault), "fixture_failed");
    let data = fault.error.expect("error").data.expect("data");
    assert_eq!(data["provider"], "extension:fixture_basic");
    assert_eq!(data["detail"]["why"], "asked");

    // The access class gates it like any built-in method.
    let denied = call(&mut app, "fixture_basic.poke", json!({}));
    assert_eq!(data_code(&denied), "F0012");

    // Advertised in hello and the schema.
    let hello = ok(&mut app, "agent.hello", json!({}));
    let methods: Vec<&str> = hello["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(methods.contains(&"fixture_basic.hello"));
    let schema = ok(&mut app, "agent.schema", json!({}));
    let row = schema["extension_methods"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|m| m["name"] == "fixture_basic.hello")
        .expect("row");
    assert_eq!(row["provider"], "extension:fixture_basic");
    assert_eq!(row["permission"], "read");
    let one = ok(
        &mut app,
        "agent.schema",
        json!({ "method": "fixture_basic.poke" }),
    );
    assert_eq!(one["methods"][0]["provider"], "extension:fixture_basic");
}

#[test]
fn extension_params_are_checked_against_the_schema_before_the_handler() {
    let p = project("plugin-params");
    let mut app = app(&p.0, &["read"]);
    app.world_mut().init_resource::<HelloCalls>();
    let calls = |app: &App| app.world().resource::<HelloCalls>().0;

    let wrong_type = call(&mut app, "fixture_basic.hello", json!({ "fail": "yes" }));
    assert_eq!(data_code(&wrong_type), "invalid_params");
    let error = wrong_type.error.expect("error");
    assert_eq!(error.code, protocol::INVALID_PARAMS);
    let data = error.data.expect("data");
    assert_eq!(data["path"], "/fail");
    assert_eq!(data["rule"], "type");
    assert_eq!(data["provider"], "extension:fixture_basic");

    let unknown = call(&mut app, "fixture_basic.hello", json!({ "extra": 1 }));
    assert_eq!(data_code(&unknown), "invalid_params");
    assert_eq!(
        unknown.error.expect("error").data.expect("data")["rule"],
        "additionalProperties"
    );
    // Positional (array) params are refused by the dispatcher before routing (F0002).
    let not_object = call(&mut app, "fixture_basic.hello", json!([1, 2]));
    assert_eq!(not_object.error.expect("error").code, protocol::INVALID_PARAMS);
    assert_eq!(calls(&app), 0, "a refused call never reaches the handler");

    // Valid params, and absent params (null, read as `{}`), run it.
    assert_eq!(ok(&mut app, "fixture_basic.hello", json!({}))["reply"], "hello");
    assert_eq!(ok(&mut app, "fixture_basic.hello", Value::Null)["reply"], "hello");
    let fault = call(&mut app, "fixture_basic.hello", json!({ "fail": true }));
    assert_eq!(data_code(&fault), "fixture_failed");
    assert_eq!(calls(&app), 3);
}

#[test]
fn methods_of_an_inactive_provider_are_unavailable_not_unknown() {
    let p = project("plugin-unavailable");
    let mut app = app(&p.0, &["read"]);
    // `fixture_broken` is discovered and not enabled.
    let gone = call(&mut app, "fixture_broken.ping", json!({}));
    assert_eq!(data_code(&gone), "method_unavailable");
    let data = gone.error.expect("error").data.expect("data");
    assert_eq!(data["id"], "fixture_broken");
    assert_eq!(data["state"], "available");
    // No extension owns this namespace.
    let unknown = call(&mut app, "nobody.ping", json!({}));
    assert_eq!(unknown.error.expect("error").code, protocol::METHOD_NOT_FOUND);
}

#[test]
fn reload_without_an_editor_host_is_not_supported() {
    let p = project("plugin-reload");
    let mut app = app(&p.0, BUILD);
    let source = call(&mut app, "plugin.reload", json!({ "id": "fixture_basic" }));
    assert_eq!(data_code(&source), "not_supported");
    let message = source.error.expect("error").message;
    assert!(message.contains("source extension"), "{message}");
    let content = call(&mut app, "plugin.reload", json!({ "id": "fixture_content" }));
    assert_eq!(data_code(&content), "not_supported");
    let absent = call(&mut app, "plugin.reload", json!({ "id": "nothing" }));
    assert_eq!(data_code(&absent), "not_found");
}

#[test]
fn engine_namespaces_are_reserved_ids() {
    let reserved = crate::plugins::reserved_namespaces();
    for ns in ["scene", "plugin", "settings", "render", "localization", "agent"] {
        assert!(reserved.iter().any(|r| r == ns), "{ns}");
    }
    let p = project("plugin-reserved");
    manifest(&p.0, "scene", "");
    let mut app = app(&p.0, &["read"]);
    let dry = ok(&mut app, "plugin.check", json!({ "enabled": ["scene"] }));
    let codes: Vec<&str> = dry["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"reserved_id"), "{dry}");
}

#[test]
fn the_app_without_a_project_answers_quietly() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentPermissions {
            config: AgentConfig::default(),
            source: None,
        })
        .init_resource::<AuditLog>()
        .init_resource::<AgentSession>()
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>();
    let list = ok(&mut app, "plugin.list", json!({}));
    assert_eq!(list["available"], false);
    assert_eq!(ok(&mut app, "plugin.check", json!({}))["available"], false);
    assert_eq!(
        data_code(&call(&mut app, "plugin.info", json!({ "id": "x" }))),
        "not_found"
    );
}

/// Spec § 2.6: a filtered or failing content extension is recorded for the next start, not mounted.
#[test]
fn enable_mounts_only_what_would_activate() {
    let p = project("plugin-enable-filtered");
    let other = if cfg!(windows) { "linux" } else { "windows" };
    manifest(
        &p.0,
        "fixture_elsewhere",
        &format!("platforms = [\"{other}\"]\n\n[[content]]\nroot = \"content\"\n"),
    );
    write(&p.0.join("extensions/fixture_elsewhere/content/a.txt"), "a");
    let mut app = app(&p.0, BUILD);
    let answer = ok(&mut app, "plugin.enable", json!({ "id": "fixture_elsewhere" }));
    assert_eq!(answer["state_after_restart"], "filtered");
    assert_eq!(answer["mounted"], json!([]));
    let registry = app.world().resource::<ExtensionRegistry>();
    assert!(
        registry
            .mounts()
            .resolve("ext://fixture_elsewhere/a.txt", None)
            .is_none()
    );
}

/// A grant the manifest does not ask for is not stored for a later manifest to use.
#[test]
fn enable_stores_only_requested_grants() {
    let p = project("plugin-enable-grants");
    let mut app = app(&p.0, BUILD);
    ok(
        &mut app,
        "plugin.enable",
        json!({ "id": "fixture_content", "grant": ["process"] }),
    );
    let text = std::fs::read_to_string(p.0.join("Regia.toml")).expect("read");
    assert!(!text.contains("process"), "{text}");
}

/// Spec § 3.5 and the guide's task flow: an agent can wait on `ExtensionEvent` messages.
#[test]
fn extension_events_reach_wait_until() {
    let p = project("plugin-events");
    let mut app = app(&p.0, &["read"]);
    app.init_resource::<crate::events::AgentEventLog>()
        .add_systems(Last, crate::events::record_events);
    let holds = |app: &mut App, contains: &str| {
        let condition = json!({ "events_contain": { "kind": "extension", "contains": contains } });
        crate::playtest::evaluate(app.world_mut(), &condition).holds
    };
    app.update();
    app.update();
    assert!(holds(&mut app, "Built"), "the startup build is a record");
    assert!(holds(&mut app, "fixture_basic"));
    assert!(!holds(&mut app, "Reloaded"));
}

/// Spec 57 § 5.1 and § 3.5: `plugin.disable` lists the active extensions that lose a required
/// dependency in `dependents_affected`, and enable/disable post `ExtensionEvent::Enabled` and
/// `Disabled`, which an agent can wait on.
#[test]
fn disable_lists_dependents_and_toggles_post_events() {
    let p = project("plugin-dependents");
    write(
        &p.0.join("Regia.toml"),
        "[project]\nname = \"fixture\"\n\n[extensions]\nenabled = [\"fixture_basic\", \"fixture_dep\"]\n\n[extensions.grants]\nfixture_basic = [\"agent_method\"]\n",
    );
    manifest(
        &p.0,
        "fixture_dep",
        "[[dependency]]\nid = \"fixture_basic\"\n\n[[content]]\nroot = \"content\"\n",
    );
    write(&p.0.join("extensions/fixture_dep/content/b.txt"), "b");
    let mut app = app(&p.0, BUILD);
    app.init_resource::<crate::events::AgentEventLog>()
        .add_systems(Last, crate::events::record_events);
    let list = ok(&mut app, "plugin.list", json!({ "state": "active" }));
    let active: Vec<&str> = list["extensions"]
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(active, ["fixture_basic", "fixture_dep"], "{list}");

    let disabled = ok(&mut app, "plugin.disable", json!({ "id": "fixture_basic" }));
    assert_eq!(
        disabled["dependents_affected"],
        json!(["fixture_dep"]),
        "{disabled}"
    );
    let enabled = ok(&mut app, "plugin.enable", json!({ "id": "fixture_content" }));
    assert_eq!(enabled["enabled"], true);
    app.update();
    app.update();
    let holds = |app: &mut App, contains: &str| {
        let condition = json!({ "events_contain": { "kind": "extension", "contains": contains } });
        crate::playtest::evaluate(app.world_mut(), &condition).holds
    };
    assert!(holds(&mut app, "Disabled"), "plugin.disable posts Disabled");
    assert!(holds(&mut app, "Enabled"), "plugin.enable posts Enabled");
    assert!(holds(&mut app, "fixture_content"));
}

/// An extension's declared settings resource (spec 57 § 3.3.5).
#[derive(Resource, Reflect, Default, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
#[serde(default)]
struct TunedSettings {
    max_line_chars: u32,
    speaker_colors: bool,
}

struct Tuned;

impl RegiaExtension for Tuned {
    fn id(&self) -> &str {
        "fixture_tuned"
    }

    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.declare_settings::<TunedSettings>()
    }
}

/// Spec 57 § 3.3.5: `declare_settings` registers the resource under the extension id with the
/// existing `settings.get` / `settings.set` methods: `<id>` and `<id>.<field>` read and write
/// it (initialized from `[extensions.settings.<id>]`), `dry_run` writes nothing, and an unknown
/// field is `not_found`.
#[test]
fn extension_settings_route_through_settings_get_and_set() {
    let p = project("plugin-settings");
    write(
        &p.0.join("Regia.toml"),
        "[project]\nname = \"fixture\"\n\n[extensions]\nenabled = [\"fixture_basic\", \"fixture_tuned\"]\n\n[extensions.grants]\nfixture_basic = [\"agent_method\"]\n\n[extensions.settings.fixture_tuned]\nmax_line_chars = 240\n",
    );
    manifest(
        &p.0,
        "fixture_tuned",
        &format!("capabilities = [\"settings\"]\n{CODE}"),
    );
    let mut app = app_with(&p.0, BUILD, vec![Arc::new(Basic), Arc::new(Tuned)]);
    assert_eq!(
        app.world().resource::<TunedSettings>().max_line_chars,
        240,
        "initialized from the project table"
    );

    let one = ok(
        &mut app,
        "settings.get",
        json!({ "path": "fixture_tuned.max_line_chars" }),
    );
    assert_eq!(one["value"], 240);
    assert_eq!(one["provider"], "extension:fixture_tuned");
    let whole = ok(&mut app, "settings.get", json!({ "path": "fixture_tuned" }));
    assert_eq!(whole["value"]["max_line_chars"], 240, "{whole}");
    assert_eq!(whole["value"]["speaker_colors"], false, "{whole}");

    let dry = ok(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.max_line_chars", "value": 80, "dry_run": true }),
    );
    assert_eq!(
        (dry["before"].clone(), dry["after"].clone()),
        (json!(240), json!(80))
    );
    assert_eq!(
        app.world().resource::<TunedSettings>().max_line_chars,
        240,
        "dry_run writes nothing"
    );

    let set = ok(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.max_line_chars", "value": 120 }),
    );
    assert_eq!(
        (set["before"].clone(), set["after"].clone()),
        (json!(240), json!(120))
    );
    assert_eq!(set["provider"], "extension:fixture_tuned");
    assert_eq!(app.world().resource::<TunedSettings>().max_line_chars, 120);

    let unknown = call(&mut app, "settings.get", json!({ "path": "fixture_tuned.nope" }));
    assert!(unknown.error.is_some(), "an unknown field is refused");
    let bad = call(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.max_line_chars", "value": "many" }),
    );
    assert!(bad.error.is_some(), "a value of the wrong type is refused");
    assert_eq!(app.world().resource::<TunedSettings>().max_line_chars, 120);
}

/// How often an extension system gated on `resource_changed` / `resource_added` of its settings
/// resource ran (the "re-apply my settings" pattern spec 57 § 3.3.5 settings invite).
#[derive(Resource, Default, Debug, PartialEq)]
struct TunedReapplied {
    changed: u32,
    added: u32,
}

/// rev-kimi-14 finding 3 (ADR-5936): `settings.set { dry_run: true }`, a set naming an unknown
/// field and a set whose value has the wrong type leave the extension's settings resource
/// neither changed nor added, so a system gated on `resource_changed` / `resource_added` does
/// not run. A real write still reads as changed (control). Before the fix every call removed
/// and re-inserted the resource, marking it added and changed.
#[test]
fn extension_settings_dry_run_and_refused_sets_leave_change_detection_alone() {
    let p = project("plugin-settings-ticks");
    write(
        &p.0.join("Regia.toml"),
        "[project]\nname = \"fixture\"\n\n[extensions]\nenabled = [\"fixture_basic\", \"fixture_tuned\"]\n\n[extensions.grants]\nfixture_basic = [\"agent_method\"]\n",
    );
    manifest(
        &p.0,
        "fixture_tuned",
        &format!("capabilities = [\"settings\"]\n{CODE}"),
    );
    let mut app = app_with(&p.0, BUILD, vec![Arc::new(Basic), Arc::new(Tuned)]);
    app.init_resource::<TunedReapplied>().add_systems(
        Update,
        (
            (|mut n: ResMut<TunedReapplied>| n.changed += 1).run_if(resource_changed::<TunedSettings>),
            (|mut n: ResMut<TunedReapplied>| n.added += 1).run_if(resource_added::<TunedSettings>),
        ),
    );
    // Settle: the first run of the new systems sees the start-up insert.
    app.update();
    app.update();
    *app.world_mut().resource_mut::<TunedReapplied>() = TunedReapplied::default();
    app.update();
    assert_eq!(
        *app.world().resource::<TunedReapplied>(),
        TunedReapplied::default(),
        "settled: nothing touched the settings"
    );

    let dry = ok(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.max_line_chars", "value": 80, "dry_run": true }),
    );
    assert_eq!(
        (dry["before"].clone(), dry["after"].clone()),
        (json!(0), json!(80))
    );
    assert_eq!(dry["provider"], "extension:fixture_tuned");
    let dry_whole = ok(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned", "value": { "max_line_chars": 7, "speaker_colors": true }, "dry_run": true }),
    );
    assert_eq!(dry_whole["after"]["max_line_chars"], 7, "{dry_whole}");
    let unknown = call(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.nope", "value": 1 }),
    );
    assert_eq!(
        unknown.error.as_ref().map(|e| e.code),
        Some(protocol::NOT_FOUND),
        "an unknown field is refused"
    );
    let bad = call(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.max_line_chars", "value": "many" }),
    );
    assert_eq!(
        bad.error.as_ref().map(|e| e.code),
        Some(protocol::INVALID_PARAMS),
        "a value of the wrong type is refused"
    );
    app.update();
    assert_eq!(
        *app.world().resource::<TunedReapplied>(),
        TunedReapplied::default(),
        "dry_run and refused sets leave the resource neither changed nor added"
    );
    assert_eq!(app.world().resource::<TunedSettings>(), &TunedSettings::default());

    let set = ok(
        &mut app,
        "settings.set",
        json!({ "path": "fixture_tuned.max_line_chars", "value": 120 }),
    );
    assert_eq!(
        (set["before"].clone(), set["after"].clone()),
        (json!(0), json!(120))
    );
    app.update();
    assert_eq!(
        app.world().resource::<TunedReapplied>().changed,
        1,
        "a real write reads as changed"
    );
    assert_eq!(app.world().resource::<TunedSettings>().max_line_chars, 120);
}

#[cfg(unix)]
#[test]
fn tools_run_off_the_frame_and_are_bounded() {
    let p = project("plugin-tools");
    manifest(
        &p.0,
        "fixture_tool",
        "capabilities = [\"process\"]\n\n[[tool]]\nname = \"echo\"\ncommand = \"bin/echo\"\ntimeout_seconds = 30\n\n[[tool]]\nname = \"net\"\ncommand = \"bin/echo\"\ncapabilities = [\"network\"]\n\n[[tool]]\nname = \"hang\"\ncommand = \"bin/hang\"\ntimeout_seconds = 1\n",
    );
    let dir = p.0.join("extensions/fixture_tool/bin");
    // Written by child processes, so no sibling test's fork holds them open for writing when
    // they run (ETXTBSY, ADR-5959).
    std::fs::create_dir_all(&dir).expect("bin dir");
    for (name, text) in [
        ("echo", "#!/bin/sh\nread line\necho \"$1 $line\"\n"),
        ("hang", "#!/bin/sh\nexec sleep 300\n"),
    ] {
        regia_test_utils::process::write_executable_script(&dir.join(name), text).expect("tool");
    }
    let mut app = app(&p.0, BUILD);
    let params = json!({ "id": "fixture_tool", "name": "echo", "args": ["hello"], "stdin": "world\n" });
    assert_eq!(
        data_code(&call(&mut app, "plugin.tool_run", params.clone())),
        "capability_not_granted"
    );
    ok(
        &mut app,
        "plugin.enable",
        json!({ "id": "fixture_tool", "grant": ["process"] }),
    );
    // The grant is in the file; a host reads it at the next start.
    let mut app = self::app(&p.0, BUILD);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "plugin.tool_run".into(),
                params,
            },
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    let answer = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("answered")
        .result
        .expect("ran");
    assert_eq!(answer["status"], 0);
    assert_eq!(answer["stdout"], "hello world\n");

    let net = call(
        &mut app,
        "plugin.tool_run",
        json!({ "id": "fixture_tool", "name": "net" }),
    );
    assert_eq!(
        data_code(&net),
        "capability_not_granted",
        "network needs its own grant"
    );
    let absent = call(
        &mut app,
        "plugin.tool_run",
        json!({ "id": "fixture_tool", "name": "nope" }),
    );
    assert_eq!(data_code(&absent), "not_found");
}
