// SPDX-License-Identifier: MPL-2.0
//! Dispatch of scoped-token requests (ADR-1523): the scope check, the project profile on top,
//! per-scope budgets, the caller seen by handlers, provenance names and deferred extension
//! methods. Requests go through in-process channels (no socket, no wall-clock waits).
#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::dispatch::{AgentInbox, RateLimit, process_requests, process_scoped_requests};
use crate::extension::{AgentExtension, DeferredReply};
use crate::protocol::{self, Request, Response, RpcError};
use crate::safety::{AgentConfig, AgentPermissions, AgentSession, AuditLog, ChangeLog};
use crate::schema::{MethodSchema, Permission};
use crate::scope::{
    AgentCaller, ScopedInbound, ScopedInbox, TOOL_SCOPE_DENIED, TOOL_TOKEN_REVOKED, ToolScope, ToolTokens,
    current_caller,
};
use crate::server::Inbound;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .register_type::<Name>()
        .register_type::<regia_scene::Transform>()
        .insert_resource(AgentPermissions {
            config: AgentConfig::default(),
            source: None,
        })
        .init_resource::<AuditLog>()
        .init_resource::<AgentSession>()
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>();
    app
}

fn request(method: &str, params: Value) -> Request {
    Request {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params,
    }
}

/// A token for tool `fixture` with these permissions and method patterns.
fn issue(tokens: &ToolTokens, permissions: &[Permission], methods: &[&str]) -> Arc<ToolScope> {
    let scope = ToolScope::new(
        "fixture",
        permissions.iter().copied(),
        methods.iter().map(|m| (*m).to_string()).collect(),
    );
    tokens.issue(scope).1
}

/// Drives one scoped request through `process_scoped_requests`.
fn scoped(
    app: &mut App,
    tokens: &ToolTokens,
    scope: &Arc<ToolScope>,
    method: &str,
    params: Value,
) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(ScopedInbound {
            request: request(method, params),
            reply: tx,
            scope: Arc::clone(scope),
        })
        .expect("queued");
    app.world_mut().insert_resource(ScopedInbox {
        inbox: inbox_rx,
        tokens: tokens.clone(),
    });
    process_scoped_requests(app.world_mut());
    rx.recv_timeout(Duration::from_secs(2)).expect("answered")
}

/// Drives one master-token request through `process_requests`.
fn master(app: &mut App, method: &str, params: Value) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: request(method, params),
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    rx.recv_timeout(Duration::from_secs(2)).expect("answered")
}

fn code(response: &Response) -> Option<String> {
    response
        .error
        .as_ref()
        .and_then(|e| e.data.as_ref())
        .and_then(|d| d["code"].as_str())
        .map(str::to_string)
}

/// A scoped token reaches the methods of its scope only, and only with its permissions; a
/// method outside the scope is refused the same way whether it exists or not.
#[test]
fn scoped_calls_stay_inside_their_scope() {
    let mut app = app();
    let tokens = ToolTokens::new();
    let narrow = issue(&tokens, &[Permission::Read], &["agent.*"]);
    let hello = scoped(&mut app, &tokens, &narrow, "agent.hello", json!({}));
    assert!(hello.error.is_none(), "{:?}", hello.error);
    let spawn = scoped(&mut app, &tokens, &narrow, "scene.spawn", json!({ "name": "x" }));
    assert_eq!(code(&spawn).as_deref(), Some(TOOL_SCOPE_DENIED));
    assert_eq!(
        spawn.error.as_ref().map(|e| e.code),
        Some(protocol::PERMISSION_DENIED)
    );
    let unknown = scoped(&mut app, &tokens, &narrow, "nope.missing", json!({}));
    assert_eq!(
        code(&unknown).as_deref(),
        Some(TOOL_SCOPE_DENIED),
        "no surface leak"
    );

    // The method is in scope but its permission is not.
    let read_scene = issue(&tokens, &[Permission::Read], &["scene.*"]);
    let spawn = scoped(
        &mut app,
        &tokens,
        &read_scene,
        "scene.spawn",
        json!({ "name": "x" }),
    );
    assert_eq!(code(&spawn).as_deref(), Some(TOOL_SCOPE_DENIED));
    assert!(
        spawn
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("edit_scene")),
        "{:?}",
        spawn.error
    );
    let query = scoped(&mut app, &tokens, &read_scene, "scene.query_entities", json!({}));
    assert_ne!(code(&query).as_deref(), Some(TOOL_SCOPE_DENIED));
}

/// The project's `.regia/agent.toml` still applies on top of the scope.
#[test]
fn the_project_profile_applies_to_scoped_calls() {
    let mut app = app();
    app.insert_resource(AgentPermissions {
        config: AgentConfig::fail_closed(),
        source: None,
    });
    let tokens = ToolTokens::new();
    let scope = issue(&tokens, &[Permission::Read, Permission::EditScene], &["scene.*"]);
    let spawn = scoped(&mut app, &tokens, &scope, "scene.spawn", json!({ "name": "x" }));
    assert_eq!(code(&spawn).as_deref(), Some("F0012"));
}

/// A request queued before its token was revoked is refused when it is drained.
#[test]
fn a_revoked_token_is_refused_at_dispatch() {
    let mut app = app();
    let tokens = ToolTokens::new();
    let scope = issue(&tokens, &[Permission::Read], &["*"]);
    tokens.revoke(scope.id);
    let response = scoped(&mut app, &tokens, &scope, "agent.hello", json!({}));
    assert_eq!(code(&response).as_deref(), Some(TOOL_TOKEN_REVOKED));
}

/// A tool's `agent.hello` does not rename the agent session, and its edits are stamped
/// `tool:<id>` in the change log.
#[test]
fn tool_edits_are_stamped_with_the_tool() {
    let mut app = app();
    let tokens = ToolTokens::new();
    let scope = issue(&tokens, &[Permission::Read, Permission::EditScene], &["*"]);
    let hello = scoped(
        &mut app,
        &tokens,
        &scope,
        "agent.hello",
        json!({ "client": "pretender", "model": "x" }),
    );
    assert!(hello.error.is_none());
    assert_eq!(app.world().resource::<AgentSession>().client, "unknown");
    let spawned = scoped(
        &mut app,
        &tokens,
        &scope,
        "scene.spawn",
        json!({ "name": "Tool Rock", "components": { "Transform": {
            "translation": [0.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0]
        } } }),
    );
    assert!(spawned.error.is_none(), "{:?}", spawned.error);
    let log = app.world().resource::<ChangeLog>();
    let record = log.records.last().expect("one record");
    assert_eq!(record.client, "tool:fixture");
    assert!(
        app.world().get_resource::<AgentCaller>().is_none(),
        "the caller lasts one call"
    );
}

/// A scoped token spends its own budget: a master budget that is used up does not refuse it.
#[test]
fn scoped_calls_have_their_own_budget() {
    let mut app = app();
    app.world_mut().resource_mut::<RateLimit>().fill_for_test(50);
    let refused = master(&mut app, "play.pause", json!({}));
    assert_eq!(refused.error.map(|e| e.code), Some(protocol::RATE_LIMITED));
    let tokens = ToolTokens::new();
    let scope = issue(&tokens, &[Permission::Read, Permission::Play], &["play.*"]);
    let paused = scoped(&mut app, &tokens, &scope, "play.pause", json!({}));
    assert!(paused.error.is_none(), "{:?}", paused.error);
}

fn whoami(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(json!({ "tool": current_caller(world).map(|scope| scope.tool.clone()) }))
}

#[derive(Resource, Default)]
struct Held(Vec<DeferredReply>);

fn later(world: &mut World, params: &Value, reply: DeferredReply) -> Result<(), RpcError> {
    if params.get("refuse").is_some() {
        return Err(RpcError::invalid_params("refused"));
    }
    world
        .get_resource_or_insert_with::<Held>(Default::default)
        .0
        .push(reply);
    Ok(())
}

fn schema(name: &'static str) -> MethodSchema {
    MethodSchema {
        name,
        tool: "",
        description: "A test method.",
        params: &[],
        result: "{}",
        mutating: false,
        permission: Permission::Read,
    }
}

/// Extension handlers see the scoped caller during the call, and no caller for the master
/// token.
#[test]
fn handlers_see_the_scoped_caller() {
    let mut app = app();
    let mut extension = AgentExtension::new();
    extension.register(schema("editor.whoami"), whoami);
    app.insert_resource(extension);
    let tokens = ToolTokens::new();
    let scope = issue(&tokens, &[Permission::Read], &["editor.whoami"]);
    let tool = scoped(&mut app, &tokens, &scope, "editor.whoami", json!({}));
    assert_eq!(tool.result.expect("answered")["tool"], "fixture");
    let agent = master(&mut app, "editor.whoami", json!({}));
    assert_eq!(agent.result.expect("answered")["tool"], Value::Null);
}

/// A deferred extension method answers from a later frame; an `Err` answers at once.
#[test]
fn deferred_extension_methods_answer_later() {
    let mut app = app();
    let mut extension = AgentExtension::new();
    extension.register_deferred(schema("editor.later"), later);
    app.insert_resource(extension);

    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: request("editor.later", json!({})),
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    assert!(rx.try_recv().is_err(), "not answered yet");
    let held = app
        .world_mut()
        .resource_mut::<Held>()
        .0
        .pop()
        .expect("reply kept");
    assert!(held.send(Ok(json!({ "done": true }))));
    let response = rx.try_recv().expect("answered later");
    assert_eq!(response.id, json!(1));
    assert_eq!(response.result, Some(json!({ "done": true })));

    let refused = master(&mut app, "editor.later", json!({ "refuse": true }));
    assert_eq!(code(&refused).as_deref(), Some("F0002"));
}

/// `docs/security/threat-model.md`: a tool's scope never holds the permissions that run code or
/// approve changes, whatever its manifest asks for.
#[test]
fn a_scope_never_grants_build_or_review() {
    let scope = ToolScope::new(
        "tool",
        [Permission::Read, Permission::Build, Permission::Review],
        Vec::new(),
    );
    assert!(scope.permissions.contains("read"));
    assert!(!scope.permissions.contains("build"));
    assert!(!scope.permissions.contains("review"));
}

/// A handler that runs `play.pause` in-process through `call_local` (as the editor's
/// `editor.console_run` does) and reports the inner answer and the caller it sees afterwards.
fn relay(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let inner = match crate::call_local(world, "play.pause", json!({}), None) {
        crate::LocalReply::Done(result) => result,
        crate::LocalReply::Pending(_) => Err(RpcError::invalid_params("deferred")),
    };
    Ok(json!({
        "inner_error": inner.err().and_then(|e| e.data).and_then(|d| d.get("code").cloned()),
        "caller_after": current_caller(world).map(|scope| scope.tool.clone()),
    }))
}

/// An in-process call made inside a scoped call keeps the scope (review w12-rev-early-k row 1): a
/// tool allowed only `editor.relay` cannot reach `play.pause` through it, and the caller is still
/// there for the outer call's audit line; the master token's relay runs it.
#[test]
fn an_in_process_call_keeps_the_scoped_caller() {
    let mut app = app();
    let mut extension = AgentExtension::new();
    extension.register(schema("editor.relay"), relay);
    app.insert_resource(extension);
    let tokens = ToolTokens::new();
    let scope = issue(&tokens, &[Permission::Read, Permission::Play], &["editor.relay"]);
    let tool = scoped(&mut app, &tokens, &scope, "editor.relay", json!({}));
    let tool = tool.result.expect("the relay answers");
    assert_eq!(tool["inner_error"], TOOL_SCOPE_DENIED, "{tool}");
    assert_eq!(tool["caller_after"], "fixture", "{tool}");
    assert!(
        app.world().get_resource::<AgentCaller>().is_none(),
        "the caller still lasts one call"
    );
    let agent = master(&mut app, "editor.relay", json!({}));
    assert_eq!(agent.result.expect("answered")["inner_error"], Value::Null);
}
