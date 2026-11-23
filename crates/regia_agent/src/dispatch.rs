// SPDX-License-Identifier: MPL-2.0
//! Main-thread side: drains requests each frame, checks permissions and the mutating-call rate
//! limit (specs/23 § 5), routes to the area handlers, and records every call in the audit log
//! with a provenance entry for agent-authored changes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, PROTOCOL_VERSION, Request, Response, RpcError};
use crate::safety::{AgentSession, AuditEntry, AuditLog, ChangeLog, params_hash, unix_ms_now};
use crate::schema::{self, Permission};
use crate::seeing::Outcome;
use crate::server::Inbound;

/// Requests handled per frame at most (the rest wait for the next frame).
const MAX_PER_FRAME: usize = 64;

/// Incoming requests (the server's receiving end).
#[derive(Resource)]
pub struct AgentInbox(pub Receiver<Inbound>);

/// Per-second budgets for mutating calls and for heavy calls (the limits come from the
/// project's `.regia/agent.toml`: default 50 mutating and 60 heavy — specs/23 § 5, ADR-1034).
/// The master token has one; each scoped token has its own (ADR-1523), so a tool cannot spend
/// the agent's budget.
#[derive(Resource, Default)]
pub struct RateLimit {
    /// When each mutating call admitted in the last second was made (sliding window).
    admitted: VecDeque<Instant>,
    /// The same for heavy calls (the method contract's `cost`).
    heavy: VecDeque<Instant>,
}

impl RateLimit {
    /// Uses up `n` mutating-call admissions now (tests of other modules).
    #[cfg(test)]
    pub(crate) fn fill_for_test(&mut self, n: usize) {
        self.admitted = std::iter::repeat_n(Instant::now(), n).collect();
    }

    /// Checks every requested window and the final pre-handler guard before recording any
    /// admission. A failed guard or a full window leaves no new timestamp in either window.
    fn admit_at(
        &mut self,
        heavy: Option<u32>,
        mutating: Option<u32>,
        now: Instant,
        guard: impl FnOnce() -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        if let Some(limit) = heavy
            && !window_has_capacity(&mut self.heavy, limit, now)
        {
            return Err(rate_limit_error(LimitedBudget::Heavy, limit));
        }
        if let Some(limit) = mutating
            && !window_has_capacity(&mut self.admitted, limit, now)
        {
            return Err(rate_limit_error(LimitedBudget::Mutating, limit));
        }
        guard()?;
        if heavy.is_some() {
            self.heavy.push_back(now);
        }
        if mutating.is_some() {
            self.admitted.push_back(now);
        }
        Ok(())
    }
}

/// A call's exhausted sliding window; heavy takes precedence when both are full.
enum LimitedBudget {
    Heavy,
    Mutating,
}

/// The existing F0007 response for the exhausted window.
fn rate_limit_error(budget: LimitedBudget, limit: u32) -> RpcError {
    let (message, hint) = match budget {
        LimitedBudget::Heavy => (
            "too many heavy calls",
            format!(
                "wait a second; the limit is {limit} heavy calls per second (agent.schema marks them cost \"heavy\")"
            ),
        ),
        LimitedBudget::Mutating => (
            "too many mutating calls",
            format!("wait a second; the limit is {limit} mutating calls per second"),
        ),
    };
    RpcError::new(protocol::RATE_LIMITED, "F0007", message, hint)
}

/// Drops entries at least a second old and probes the remaining capacity without spending it.
fn window_has_capacity(window: &mut VecDeque<Instant>, per_second: u32, now: Instant) -> bool {
    while window
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= Duration::from_secs(1))
    {
        window.pop_front();
    }
    window.len() < per_second as usize
}

/// The per-scope budgets of scoped tokens (ADR-1523), keyed by scope id.
#[derive(Resource, Default)]
struct ScopedRateLimits(std::collections::HashMap<u64, RateLimit>);

// Hosted listeners have their own token table; their ids need not match tool ids.
#[derive(Resource, Default)]
struct HostedRateLimits(std::collections::HashMap<u64, RateLimit>);

/// Admits a call against all its windows, using the master's or the calling scope's budgets.
/// Refused calls leave neither window with a new timestamp.
fn admit_call(
    world: &mut World,
    caller: Option<&crate::scope::ToolScope>,
    heavy: Option<u32>,
    mutating: Option<u32>,
    guard: impl FnOnce() -> Result<(), RpcError>,
) -> Result<(), RpcError> {
    if heavy.is_none() && mutating.is_none() {
        return guard();
    }
    let now = Instant::now();
    match caller {
        None => {
            let mut rate = world.get_resource_or_insert_with::<RateLimit>(Default::default);
            rate.admit_at(heavy, mutating, now, guard)
        }
        Some(scope) => {
            let mut limits = world.get_resource_or_insert_with::<ScopedRateLimits>(Default::default);
            limits
                .0
                .entry(scope.id)
                .or_default()
                .admit_at(heavy, mutating, now, guard)
        }
    }
}

/// `PreUpdate` (exclusive): answers queued requests from scoped tokens (ADR-1523), through the
/// same path as [`process_requests`] plus the scope check. No-op without a
/// [`crate::scope::ScopedInbox`].
pub fn process_scoped_requests(world: &mut World) {
    if world.contains_resource::<regia_framework::demo_world::DemoCatchUp>() {
        return;
    }
    let Some((inbox, tokens)) = world
        .get_resource::<crate::scope::ScopedInbox>()
        .map(|i| (i.inbox.clone(), i.tokens.clone()))
    else {
        return;
    };
    // As in [`process_requests`]: connection-answered entries are skipped without counting
    // against MAX_PER_FRAME (ADR-4894).
    let mut handled = 0;
    while handled < MAX_PER_FRAME {
        let Ok(inbound) = inbox.try_recv() else {
            break;
        };
        let crate::scope::ScopedInbound {
            request,
            reply,
            scope,
        } = inbound;
        let id = request
            .id
            .clone()
            .filter(|id| matches!(id, Value::String(_) | Value::Number(_)))
            .unwrap_or(Value::Null);
        // The connection checked the token before queueing; it may have been revoked since.
        if !tokens.is_live(scope.id) {
            crate::server::take_connection(&reply);
            let _ = reply.send(Response::err(id, crate::scope::revoked_error(&scope.tool)));
            continue;
        }
        // Forget the budgets of revoked scopes (a relaunch gets a new scope id).
        if let Some(mut limits) = world.get_resource_mut::<ScopedRateLimits>() {
            limits.0.retain(|scope_id, _| tokens.is_live(*scope_id));
        }
        let started = Instant::now();
        world.insert_resource(crate::scope::AgentCaller(std::sync::Arc::clone(&scope)));
        if skip_abandoned(world, &request, &reply, started) {
            world.remove_resource::<crate::scope::AgentCaller>();
            continue;
        }
        handled += 1;
        let outcome = handle(
            world,
            &request,
            id.clone(),
            reply.clone(),
            Some(scope.as_ref()),
            None,
        );
        audit(world, &request, &outcome, started);
        world.remove_resource::<crate::scope::AgentCaller>();
        if let Outcome::Done(result) = outcome {
            let response = match result {
                Ok(value) => Response::ok(id, value),
                Err(error) => Response::err(id, error),
            };
            let _ = reply.send(response);
        }
    }
    if let Some(lifecycle) = world.get_resource::<crate::server::RequestLifecycle>().copied() {
        (lifecycle.idle)(world);
    }
}

/// Whether a drained request was already given up by its caller (its connection answered F0006
/// because the world did not drain it in time; audit-concurrency-1103 agent H1). Such a request
/// is not run: the caller may have retried, and a late mutating call would apply twice. It is
/// audited as `expired`.
fn skip_abandoned(world: &mut World, request: &Request, reply: &Sender<Response>, started: Instant) -> bool {
    if request.id.is_none() {
        return false;
    }
    let gone = crate::server::take_gone(reply);
    if gone.is_some() {
        crate::server::take_connection(reply);
    }
    match gone {
        None => false,
        Some(crate::server::Gone::TimedOut) => {
            tracing::warn!(
                target: "regia_agent",
                "{} not run: its caller already got F0006 (the world did not drain it in time)",
                truncate_on_char(&request.method, MAX_ECHOED_METHOD_BYTES)
            );
            record_audit(world, request, "expired", Some("F0006"), started);
            true
        }
        // The connection answered the poll from the progress board during a render-out step
        // (ADR-2582): the caller has its answer; the call is audited as answered by the board.
        Some(crate::server::Gone::AnsweredFromBoard) => {
            record_audit(world, request, "board", None, started);
            true
        }
    }
}

/// Longest method name echoed back in an error; longer names are cut (a request may be 32 MiB).
const MAX_ECHOED_METHOD_BYTES: usize = 128;

/// `PreUpdate` (exclusive): answers queued requests.
pub fn process_requests(world: &mut World) {
    if let Some(lifecycle) = world.get_resource::<crate::server::RequestLifecycle>().copied() {
        (lifecycle.idle)(world);
    }
    // A demo seek runs `PreUpdate` once per step: requests wait for the end of the catch-up.
    if world.contains_resource::<regia_framework::demo_world::DemoCatchUp>() {
        return;
    }
    let Some(inbox) = world.get_resource::<AgentInbox>().map(|i| i.0.clone()) else {
        return;
    };
    // The local transport (spec 23 § 2) has its own queue; both are drained within the same
    // per-frame budget, taking turns (`drain_inboxes`). An idle local transport adds one cheap
    // `try_recv` to frames that drain nothing.
    let local_inbox = world
        .get_resource::<crate::local::LocalServer>()
        .map(|local| local.inbox.clone());
    drain_inboxes(world, &inbox, local_inbox.as_ref());
    if let Some(lifecycle) = world.get_resource::<crate::server::RequestLifecycle>().copied() {
        (lifecycle.idle)(world);
    }
}

/// Drains the WebSocket inbox and the local transport's inbox within one frame's budget.
///
/// The two inboxes take turns, one handled request each (ADR-7454): a WebSocket client that
/// keeps a frame's worth of requests queued cannot stop the local transport, and a busy local
/// client cannot stop the WebSocket one. Each frame starts with the WebSocket inbox.
fn drain_inboxes(world: &mut World, inbox: &Receiver<Inbound>, local_inbox: Option<&Receiver<Inbound>>) {
    // Entries the connection already answered (F0006, or a poll answered from the render-out
    // progress board) are skipped, not handled: they cost no handler work, so they do not count
    // against MAX_PER_FRAME (ADR-4894). A stalled render-out fills the inbox with board-answered
    // polls, and a cancel that woke the app thread must dispatch in the first frame that drains
    // the inbox — behind more than MAX_PER_FRAME skips it never would be. A skip also keeps the
    // turn: only a handled request passes it to the other inbox, so local traffic cannot push a
    // cancel behind a WebSocket inbox full of skips out of the frame either.
    let mut handled = 0;
    let mut local_turn = false;
    while handled < MAX_PER_FRAME {
        let Some((inbound, from_local)) = next_inbound(inbox, local_inbox, local_turn) else {
            break;
        };
        let Inbound { request, reply } = inbound;
        // JSON-RPC ids are strings, numbers or null; any other id is refused in `handle` and
        // answered with a null id, so a client cannot make the server echo a large structure.
        let id = request
            .id
            .clone()
            .filter(|id| matches!(id, Value::String(_) | Value::Number(_)))
            .unwrap_or(Value::Null);
        let started = Instant::now();
        // A master-token call has no scoped caller (a handler that panicked mid-call cannot
        // leave one behind for the next request), nor an in-process client name.
        world.remove_resource::<crate::scope::AgentCaller>();
        world.remove_resource::<LocalClient>();
        if skip_abandoned(world, &request, &reply, started) {
            continue;
        }
        handled += 1;
        local_turn = !from_local;
        // Deferred handlers keep their own clone of `reply`; Done outcomes answer here.
        let outcome = handle(world, &request, id.clone(), reply.clone(), None, None);
        audit(world, &request, &outcome, started);
        if let Outcome::Done(result) = outcome {
            tracing::debug!(target: "regia_agent", "{} in {:?}", request.method, started.elapsed());
            let response = match result {
                Ok(value) => Response::ok(id, value),
                Err(error) => Response::err(id, error),
            };
            let _ = reply.send(response);
        }
    }
}

/// Dispatches one request from a trusted host's private scoped listener, with an additional
/// per-session guard. The host must check that the token is still live before calling this.
/// Unlike a tool, a hosted agent receives the project's normal permissions, intersected with
/// `permissions` (when supplied) and `guard`. The narrowed profile also applies to forwarded
/// handler calls. Contracts, audit, deferred replies and provenance use the regular path;
/// each hosted token has its own rate budget, separate from the master token.
/// The caller identity is isolated from the master session and restored before returning.
pub fn process_hosted_request(
    world: &mut World,
    inbound: crate::scope::ScopedInbound,
    permissions: Option<&[Permission]>,
    guard: impl Fn(&schema::MethodSchema) -> Result<(), RpcError>,
) {
    let crate::scope::ScopedInbound {
        request,
        reply,
        scope,
    } = inbound;
    let id = request
        .id
        .clone()
        .filter(|id| matches!(id, Value::String(_) | Value::Number(_)))
        .unwrap_or(Value::Null);
    let started = Instant::now();
    let previous = world.remove_resource::<crate::scope::AgentCaller>();
    let previous_session = world.remove_resource::<AgentSession>();
    world.insert_resource(AgentSession {
        client: scope.tool.clone(),
        model: String::new(),
        session: format!("hosted-{}", scope.id),
    });
    if skip_abandoned(world, &request, &reply, started) {
        world.remove_resource::<AgentSession>();
        if let Some(previous) = previous {
            world.insert_resource(previous);
        }
        if let Some(previous) = previous_session {
            world.insert_resource(previous);
        }
        return;
    }
    let previous_permissions = world.remove_resource::<crate::AgentPermissions>();
    let mut effective = previous_permissions.clone().unwrap_or_default();
    if let Some(permissions) = permissions {
        effective.config.permissions.retain(|name| {
            Permission::from_name(name).is_some_and(|permission| permissions.contains(&permission))
        });
    }
    world.insert_resource(effective);
    let previous_rate = world.remove_resource::<RateLimit>();
    let rate = world
        .get_resource_or_insert_with::<HostedRateLimits>(Default::default)
        .0
        .remove(&scope.id)
        .unwrap_or_default();
    world.insert_resource(rate);
    // A hosted agent is not a manifest tool. Its stable session identity owns proposals;
    // tool-only permission restrictions must not override an explicit project grant in Act.
    let outcome = handle(world, &request, id.clone(), reply.clone(), None, Some(&guard));
    audit(world, &request, &outcome, started);
    world.remove_resource::<crate::scope::AgentCaller>();
    world.remove_resource::<AgentSession>();
    world.remove_resource::<crate::AgentPermissions>();
    let rate = world.remove_resource::<RateLimit>().unwrap_or_default();
    world
        .get_resource_or_insert_with::<HostedRateLimits>(Default::default)
        .0
        .insert(scope.id, rate);
    if let Some(previous) = previous_rate {
        world.insert_resource(previous);
    }
    if let Some(previous) = previous_permissions {
        world.insert_resource(previous);
    }
    if let Some(previous) = previous {
        world.insert_resource(previous);
    }
    if let Some(previous) = previous_session {
        world.insert_resource(previous);
    }
    if let Outcome::Done(result) = outcome {
        let response = match result {
            Ok(value) => Response::ok(id, value),
            Err(error) => Response::err(id, error),
        };
        let _ = reply.send(response);
    }
}

/// The next queued request and whether it came from the local inbox: the inbox whose turn it
/// is first, the other one when that is empty.
fn next_inbound(
    inbox: &Receiver<Inbound>,
    local_inbox: Option<&Receiver<Inbound>>,
    local_turn: bool,
) -> Option<(Inbound, bool)> {
    let ws = || inbox.try_recv().ok().map(|inbound| (inbound, false));
    let local = || {
        local_inbox
            .and_then(|local| local.try_recv().ok())
            .map(|inbound| (inbound, true))
    };
    if local_turn {
        local().or_else(ws)
    } else {
        ws().or_else(local)
    }
}

type HostedGuard<'a> = dyn Fn(&schema::MethodSchema) -> Result<(), RpcError> + 'a;

/// Routes one request: scope check (scoped tokens), permission check, rate limit, handler,
/// provenance.
fn handle(
    world: &mut World,
    request: &Request,
    id: Value,
    reply: Sender<Response>,
    caller: Option<&crate::scope::ToolScope>,
    hosted_guard: Option<&HostedGuard<'_>>,
) -> Outcome {
    let outcome = handle_inner(world, request, id, reply.clone(), caller, hosted_guard);
    crate::server::take_connection(&reply);
    outcome
}

fn handle_inner(
    world: &mut World,
    request: &Request,
    id: Value,
    reply: Sender<Response>,
    caller: Option<&crate::scope::ToolScope>,
    hosted_guard: Option<&HostedGuard<'_>>,
) -> Outcome {
    if let Some(error) = envelope_error(request) {
        return Outcome::Done(Err(error));
    }
    // Built-in table first, then the host's extension (editor surface; specs/23 § 2). Both go
    // through the same permission/rate-limit/audit path below.
    let ext_schema = world
        .get_resource::<crate::extension::AgentExtension>()
        .and_then(|ext| ext.method_schema(&request.method).cloned());
    // Methods extensions registered (specs/57 § 5.2) follow the host's.
    let registered_schema = if schema::method(&request.method).is_none() && ext_schema.is_none() {
        crate::plugins::extension_schema(world, &request.method)
    } else {
        None
    };
    let Some(method) = schema::method(&request.method)
        .or(ext_schema.as_ref())
        .or(registered_schema.as_ref())
    else {
        // A scoped caller learns nothing about methods outside its scope.
        if let Some(scope) = caller
            && !scope.allows_method(&request.method)
        {
            return Outcome::Done(Err(scope.denied_error(&request.method, Permission::Read)));
        }
        // A disabled or failed extension's namespace is not an unknown method.
        if let Some(error) = crate::plugins::unavailable(world, &request.method) {
            return Outcome::Done(Err(error));
        }
        return Outcome::Done(Err(unknown_method(&request.method)));
    };

    // A scoped token (ADR-1523) calls only the methods and permissions of its scope.
    if let Some(scope) = caller
        && (!scope.allows_method(method.name) || !scope.allows_permission(method.permission))
    {
        return Outcome::Done(Err(scope.denied_error(method.name, method.permission)));
    }

    // Permissions (specs/23 § 5): the profile from .regia/agent.toml, default-granted when
    // the plugin had no project directory to load from.
    let permissions = world
        .get_resource_or_insert_with::<crate::safety::AgentPermissions>(Default::default)
        .clone();
    if !permissions.allows(method.permission) {
        return Outcome::Done(Err(permissions.denied_error(method.permission)));
    }

    if let Some(guard) = hosted_guard
        && let Err(error) = guard(method)
    {
        return Outcome::Done(Err(error));
    }

    // The method contract's parameter gate: an object, every limit, the default size caps
    // (ADR-1034). It runs before the rate limits so a refused call costs no budget.
    let contract = crate::contract::contract(&request.method);
    if let Err(error) = crate::contract::check_params(contract, &request.params) {
        return Outcome::Done(Err(error));
    }

    // A tool's hello does not rename the agent session (its records say `tool:<id>`).
    if request.method == "agent.hello"
        && caller.is_none()
        && let Err(error) = note_session(world, &request.params)
    {
        return Outcome::Done(Err(error));
    }

    // Heavy calls have their own budget, read-only or not. Probe both windows and the golden
    // path guard before recording either admission. Budget errors retain their precedence.
    let heavy = contract.is_some_and(|c| c.cost == crate::contract::Cost::Heavy);
    if let Err(error) = admit_call(
        world,
        caller,
        heavy.then_some(permissions.config.heavy_rate_limit_per_second),
        method
            .mutating
            .then_some(permissions.config.rate_limit_per_second),
        || crate::golden_tools::guard_params(&request.method, &request.params),
    ) {
        return Outcome::Done(Err(error));
    }

    let connection = crate::server::take_connection(&reply);
    if let Some(connection) = connection {
        world.insert_resource(connection);
    }
    let guard = world
        .get_resource::<crate::server::RequestLifecycle>()
        .copied()
        .map_or(Ok(()), |lifecycle| {
            (lifecycle.before)(world, &request.method, &request.params, method.mutating)
        });
    if let Some(observer) = world.get_resource::<crate::safety::AuditObserver>().copied() {
        (observer.begin)(world);
    }
    let outcome = match guard {
        Ok(()) => route(world, &request.method, &request.params, id, reply),
        Err(error) => Outcome::Done(Err(error)),
    };
    world.remove_resource::<crate::server::ConnectionLease>();

    // Provenance: successful authored edits (scene content and settings) are stamped into the
    // session's change log, readable through agent.changes (specs/23 § 5). `dry_run` skips the
    // record only for a method whose schema lists it; any other method ignores the flag and
    // applies the edit, so the edit is recorded (specs/23 § 3, ADR-5628).
    let previewed = method.params.iter().any(|param| param.name == "dry_run")
        && request
            .params
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if let Outcome::Done(Ok(value)) = &outcome
        && method.permission == Permission::EditScene
        && !previewed
        && value.get("applied").and_then(Value::as_bool) != Some(false)
    {
        record_change(world, &request.method, &request.params, value);
    }
    outcome
}

/// Runs a status or cancel method a job provider names (`crate::jobs::PROVIDERS`) and answers
/// at once; a handler that defers its answer is refused (no provider method does). The forwarded
/// method's own permission, the caller's tool scope and its parameter contract apply, so a job
/// id reaches nothing its caller could not call directly.
fn route_now(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
    let Some(schema) = schema::method(method) else {
        return Err(unknown_method(method));
    };
    if let Some(scope) = crate::scope::current_caller(world)
        && (!scope.allows_method(schema.name) || !scope.allows_permission(schema.permission))
    {
        return Err(scope.denied_error(schema.name, schema.permission));
    }
    let permissions = world
        .get_resource_or_insert_with::<crate::safety::AgentPermissions>(Default::default)
        .clone();
    if !permissions.allows(schema.permission) {
        return Err(permissions.denied_error(schema.permission));
    }
    crate::contract::check_params(crate::contract::contract(method), params)?;
    let (reply, _answer) = crossbeam_channel::bounded(1);
    match route(world, method, params, Value::Null, reply) {
        Outcome::Done(result) => result,
        Outcome::Deferred => Err(RpcError::internal(format!(
            "{method} answers later; call it directly"
        ))),
    }
}

/// Runs the handler of one method.
fn route(world: &mut World, method: &str, params: &Value, id: Value, reply: Sender<Response>) -> Outcome {
    // Extension methods (editor surface): same permission/rate/audit path, handler from the
    // host's registered AgentExtension resource.
    let handler = world
        .get_resource::<crate::extension::AgentExtension>()
        .and_then(|ext| ext.method(method).map(|(_, handler)| handler));
    if let Some(handler) = handler {
        return Outcome::Done(handler(world, params));
    }
    // Deferred extension methods (ADR-1523) keep the reply and answer from a later frame.
    let deferred = world
        .get_resource::<crate::extension::AgentExtension>()
        .and_then(|ext| ext.deferred_method(method));
    if let Some(handler) = deferred {
        return match handler(world, params, crate::extension::DeferredReply::new(id, reply)) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        };
    }
    // Methods extensions registered (specs/57 § 5.2); built-in names never reach them.
    if schema::method(method).is_none()
        && let Some(result) = crate::plugins::route_extension(world, method, params)
    {
        return Outcome::Done(result);
    }

    match method {
        "editor.asset_library" => Outcome::Done(Err(RpcError::new(
            crate::protocol::UNSUPPORTED,
            "editor_not_running",
            "The user asset library is provided by the editor",
            "Connect to a running editor",
        ))),
        "editor.viewport_overlays" => Outcome::Done(Err(RpcError::new(
            protocol::UNSUPPORTED,
            "editor_not_running",
            "Viewport overlays require a running editor",
            "connect to a running editor; see book/agents/viewport-tools.md",
        ))),
        // Discovery (§ 3.1).
        "agent.hello" => Outcome::Done(Ok(hello(world))),
        "agent.guide" => Outcome::Done(crate::discovery::guide(params)),
        "agent.schema" => Outcome::Done(
            schema::schema_call(
                params,
                world.get_resource::<crate::extension::AgentExtension>(),
                crate::plugins::schema_rows(world),
            )
            .map(|mut table| {
                if let Some(object) = table.as_object_mut() {
                    object.insert(
                        "capability_markers".into(),
                        crate::capabilities::markers_json(world),
                    );
                }
                table
            }),
        ),
        "agent.changes" => Outcome::Done(crate::discovery::changes(world, params)),
        // Jobs, tests, review, open in editor (specs/23 § 3.6, § 4, § 5; ADR-2346).
        "agent.job_status" => Outcome::Done(crate::jobs::job_status(world, params, &mut route_now)),
        "agent.job_cancel" => Outcome::Done(crate::jobs::job_cancel(world, params, &mut route_now)),
        "agent.run_tests" => Outcome::Done(crate::run_tests::run_tests(world, params)),
        "agent.build_game" => Outcome::Done(crate::build_game::build_game(world, params)),
        "agent.hot_reload" => Outcome::Done(crate::hot_reload::hot_reload(world, params)),
        "agent.cook" => Outcome::Done(crate::cook_package::start(
            world,
            crate::cook_package::PipelineMethod::Cook,
            params,
        )),
        "agent.package" => Outcome::Done(crate::cook_package::start(
            world,
            crate::cook_package::PipelineMethod::Package,
            params,
        )),
        "agent.open_in_editor" => Outcome::Done(crate::open_in_editor::open_in_editor(world, params)),
        "review.propose" => Outcome::Done(crate::review::propose(world, params)),
        "review.list" => Outcome::Done(crate::review::list(world, params)),
        "review.diff" => Outcome::Done(crate::review::diff(world, params)),
        "review.accept" => Outcome::Done(crate::review::accept(world, params)),
        "review.reject" => Outcome::Done(crate::review::reject(world, params)),
        // Golden tools (specs/23 § 3.7, ADR-2821): neither writes a golden.
        "agent.golden_test" => Outcome::Done(crate::golden_tools::golden_test(world, params)),
        "agent.bless_golden" => Outcome::Done(crate::golden_tools::bless_golden(world, params)),
        "project.info" => Outcome::Done(Ok(crate::discovery::project_info(world))),
        "types.list" => Outcome::Done(crate::discovery::types_list(world, params)),
        "types.describe" => Outcome::Done(crate::discovery::types_describe(world, params)),
        "resources.list" => Outcome::Done(Ok(crate::discovery::resources_list(world))),
        "errors.explain" => Outcome::Done(crate::discovery::errors_explain(params)),

        // Scene (§ 3.2).
        "scene.query_entities" => Outcome::Done(crate::scene::query_entities(world, params)),
        "scene.get_entity" => Outcome::Done(crate::scene::get_entity(world, params)),
        "scene.set_property" => Outcome::Done(crate::scene::set_property(world, params)),
        "scene.spawn" => Outcome::Done(crate::scene::spawn(world, params)),
        "scene.spawn_primitive" => Outcome::Done(crate::scene::edit(
            world,
            method,
            params,
            crate::primitive::spawn_primitive,
        )),
        "scene.despawn" => Outcome::Done(crate::scene::despawn(world, params)),
        "scene.duplicate" => Outcome::Done(crate::scene::duplicate(world, params)),
        "scene.reparent" => Outcome::Done(crate::scene::reparent(world, params)),
        "scene.add_component" => Outcome::Done(crate::scene::add_component(world, params)),
        "scene.remove_component" => Outcome::Done(crate::scene::remove_component(world, params)),
        "scene.apply_edits" => Outcome::Done(crate::scene::apply_edits(world, params)),
        "scene.serialize" => Outcome::Done(crate::scene::serialize(world)),

        // Assets (§ 3.3).
        "assets.access_trace" => Outcome::Done(crate::assets::access_trace(params)),
        "package.parity" => Outcome::Done(crate::package_parity::call(params)),
        "assets.list" => Outcome::Done(crate::assets::list(world, params)),
        "assets.describe" => Outcome::Done(crate::assets::describe(world, params)),
        "assets.load" => Outcome::Done(crate::assets::load(world, params)),
        "assets.reload" => Outcome::Done(crate::assets::reload(world, params)),
        "assets.skin_check" => Outcome::Done(crate::assets::skin_check(world, params)),
        "assets.status" => Outcome::Done(crate::assets::status(world, params)),
        "assets.watch_status" => Outcome::Done(crate::assets::watch_status(world, params)),
        "assets.load_progress" => Outcome::Done(crate::loading::load_progress(world, params)),
        "assets.handles" => Outcome::Done(crate::loading::asset_handles(world, params)),
        "assets.texture_memory" => Outcome::Done(crate::look::texture_memory(world, params)),
        "assets.cooked_pack" => Outcome::Done(crate::assets::cooked_pack(world, params)),
        "assets.pak_diff" => Outcome::Done(crate::pak_diff::pak_diff(world, params)),
        "assets.ids_export" => Outcome::Done(crate::asset_ids::ids_export(world, params)),
        "assets.ids_import" => Outcome::Done(crate::asset_ids::ids_import(world, params)),
        "assets.provenance_import" => {
            Outcome::Done(crate::provenance_import::provenance_import(world, params))
        }
        "assets.collision_report" => Outcome::Done(crate::collision_report::collision_report(world, params)),
        "assets.check_contract" => Outcome::Done(crate::check_contract::check_contract(world, params)),
        "assets.voxel_far" => Outcome::Done(crate::voxel_far::voxel_far(world, params)),
        "assets.font_subset" => Outcome::Done(crate::assets::font_subset(world, params)),
        "assets.import_rules" => Outcome::Done(crate::import_rules::import_rules(world, params)),
        "assets.texture_sets" => Outcome::Done(crate::assets_texture_sets::texture_sets(world, params)),
        "assets.terrain_tiles" => Outcome::Done(crate::terrain_tiles::terrain_tiles(world, params)),
        "assets.texture_coverage" => Outcome::Done(crate::texture_coverage::texture_coverage(world, params)),
        "assets.texpack_dry_run" => Outcome::Done(crate::texture_coverage::texpack_dry_run(world, params)),
        "assets.diet_report" => Outcome::Done(crate::diet_report::diet_report(world, params)),
        "render.voxel_far_stats" => Outcome::Done(crate::voxel_far::stats(world)),
        // Cluster residency of the visibility buffer (book/agents/visbuffer-residency.md, ADR-3118).
        "render.get_visbuffer_residency" => Outcome::Done(crate::visbuffer_residency::get(world)),
        "render.set_visbuffer_residency" => Outcome::Done(crate::visbuffer_residency::set(world, params)),
        // The MaterialX importer's report (book/agents/materialx-import.md, ADR-3341).
        "import.materialx_report" => Outcome::Done(crate::materialx_report::materialx_report(world, params)),
        "registry.query" => Outcome::Done(crate::registry::query(world, params)),
        "registry.describe" => Outcome::Done(crate::registry::describe(world, params)),
        "registry.dependencies" => Outcome::Done(crate::registry::dependencies(world, params)),
        "registry.referencers" => Outcome::Done(crate::registry::referencers(world, params)),
        "registry.tags" => Outcome::Done(crate::registry::tags(world, params)),

        // Seeing (§ 3.4).
        "seeing.screenshot" => crate::seeing::screenshot(world, params, id, reply),
        "seeing.capture_sequence" => crate::seeing::capture_sequence(world, params, id, reply),
        "seeing.contact_sheet" => crate::seeing::contact_sheet(world, params, id, reply),
        "seeing.compare_images" => Outcome::Done(crate::seeing::compare_images(params)),
        "seeing.reference_trace" => Outcome::Done(crate::reference::reference_trace(world, params)),
        "seeing.compare" => Outcome::Done(crate::reference::compare(world, params)),
        "seeing.pick_entity" => Outcome::Done(crate::seeing::pick_entity(params)),
        "seeing.memory_report" => Outcome::Done(crate::seeing::memory_report(world, params)),
        "seeing.list_resources" => Outcome::Done(crate::inspector::list(params)),
        "seeing.dump_resource" => Outcome::Done(crate::inspector::dump(world, params)),

        // Loading and boot (`specs/12` § 5b, `specs/13` § 2b–2c).
        "render.boot_timeline" => Outcome::Done(crate::loading::boot_timeline()),
        "render.pipeline_warm" => Outcome::Done(crate::loading::pipeline_warm(world, params)),
        "render.scene_ready" => Outcome::Done(crate::loading::scene_ready(world)),
        "render.memory_budget" => Outcome::Done(crate::loading::memory_budget(world, params)),
        // The host memory budget override (ADR-7891, `book/agents/load-memory.md`).
        "render.set_host_budget" => Outcome::Done(crate::loading::set_host_budget(params)),
        // CPU heap and GPU memory per subsystem (ADR-539, `book/agents/memory.md`).
        "memory.report" => Outcome::Done(crate::memory::report(world, params)),
        "memory.mark" => Outcome::Done(crate::memory::mark(world)),
        // Start-up work and the idle ledger (ADR-2987, `book/agents/startup-idle.md`).
        "app.startup_work" => Outcome::Done(crate::startup_idle::startup_work(world, params)),
        "memory.idle_ledger" => match crate::startup_idle::idle_ledger_start(world, params, id, reply) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        },

        // Playing and testing (§ 3.5).
        "play.pause" => Outcome::Done(crate::playtest::set_paused(world, true)),
        "play.resume" => Outcome::Done(crate::playtest::set_paused(world, false)),
        "play.step" => match crate::playtest::step(world, params, id, reply) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        },
        "play.set_time_scale" => Outcome::Done(crate::playtest::set_time_scale(world, params)),
        "play.inject_input" => Outcome::Done(crate::playtest::inject_input(world, params)),
        // The editor's Game tabs: frames and mute of a game in another process (ADR-7831).
        "play.frame" => crate::play_view::frame(world, params, id, reply),
        "play.mute" => Outcome::Done(crate::play_view::mute(world, params)),
        // Lockstep projectiles and the aim solver, stateless (ADR-8509).
        "detsim.projectile_simulate" => Outcome::Done(crate::det_projectiles::projectile_simulate(params)),
        "detsim.aim_solve" => Outcome::Done(crate::det_projectiles::aim_solve(params)),
        // The lockstep hover (book/agents/det-hover.md, plan row mp-hover-ecs-agent).
        "detsim.hover_step" => Outcome::Done(crate::det_hover::hover_step(world, params)),
        "detsim.hover_probe" => Outcome::Done(crate::det_hover::hover_probe(world, params)),
        "detsim.hover_list" => Outcome::Done(crate::det_hover::hover_list(world, params)),
        "detsim.hover_get" => Outcome::Done(crate::det_hover::hover_get(world, params)),
        "detsim.hover_set_input" => Outcome::Done(crate::det_hover::hover_set_input(world, params)),
        // SpaceMouse (specs/29 § 4).
        "input.spacemouse_status" => Outcome::Done(crate::spacemouse::status(world)),
        "input.spacemouse_inject" => Outcome::Done(crate::spacemouse::inject(world, params)),
        "input.spacemouse_nav_defaults" => Outcome::Done(Ok(crate::spacemouse::nav_defaults())),
        // Input rebinding (ADR-1801, book/agents/input-rebind.md).
        "input.rebind_state" => Outcome::Done(crate::input_rebind::state(world, params)),
        "input.rebind" => Outcome::Done(crate::input_rebind::start(world, params)),
        "input.rebind_cancel" => Outcome::Done(crate::input_rebind::cancel(world)),
        "input.rebind_reset" => Outcome::Done(crate::input_rebind::reset(world, params)),
        "input.rebind_axis" => Outcome::Done(crate::input_rebind::axis(world, params)),
        "input.rebind_save" => Outcome::Done(crate::input_rebind::save(world, params)),
        "input.rebind_load" => Outcome::Done(crate::input_rebind::load(world, params)),
        "input.rebind_profiles" => Outcome::Done(crate::input_rebind::profiles(world)),
        "play.wait_until" => match crate::playtest::wait_until(world, params, id, reply) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        },
        "play.expect" => Outcome::Done(crate::playtest::expect(world, params)),
        "play.get_diagnostics" => Outcome::Done(Ok(crate::playtest::get_diagnostics(world))),
        "play.hitch_report" => match crate::hitch::start(world, params, id, reply) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        },

        // Logs and events (§ 3.7).
        "logs.read" => Outcome::Done(read_logs(params)),
        "logs.perf_gate" => Outcome::Done(crate::perf_gate::perf_gate(params)),
        "logs.triage" => Outcome::Done(crate::log_triage::triage(params)),
        "events.list" => Outcome::Done(Ok(crate::events::list())),
        "events.read" => Outcome::Done(crate::events::read(world, params)),
        "diagnostics.crash_reports" => Outcome::Done(crate::crash::crash_reports(params)),
        "diagnostics.report_sources" => Outcome::Done(crate::report_sources::report_sources(world, params)),
        "report.outbox_status" => Outcome::Done(crate::report_outbox::outbox_status(world, params)),
        // The fuzz-target registry (ADR-1917, book/agents/fuzzing.md).
        "fuzz.targets" => Outcome::Done(crate::fuzz::targets(world)),
        "fuzz.run" => Outcome::Done(crate::fuzz::run(world, params)),
        "feedback.capture" => match crate::feedback::capture(world, params, id, reply) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        },
        "feedback.list" => Outcome::Done(crate::feedback::list(world, params)),
        "telemetry.get_last_summary" => Outcome::Done(crate::telemetry::get_last_summary(world, params)),
        "telemetry.live" => Outcome::Done(crate::telemetry::live(world)),
        // UI focus graph: groups, wrap/stop rules, Tab order (book/agents/ui-navigation.md).
        "ui.nav_graph" => Outcome::Done(crate::ui_nav::nav_graph(world, params)),
        "ui.nav_move" => Outcome::Done(crate::ui_nav::nav_move_method(world, params)),
        "ui.nav_lint" => Outcome::Done(crate::ui_nav::nav_lint(world, params)),

        // Framework slice (specs/14 M9, specs/25 § 3).
        "scripts.list" => Outcome::Done(crate::framework::scripts_list(world)),
        "scripts.get" => Outcome::Done(crate::framework::scripts_get(world, params)),
        "camera.get" => Outcome::Done(crate::framework::camera_get(world, params)),
        "camera.set_mode" => Outcome::Done(crate::framework::camera_set_mode(world, params)),
        "camera.blend_to" => Outcome::Done(crate::framework::camera_blend_to(world, params)),
        "settings.get" => Outcome::Done(crate::framework::settings_get(world, params)),
        "settings.set" => Outcome::Done(crate::framework::settings_set(world, params)),
        // Runtime gameplay (specs/14 § A3/A4/B3/B6 agent surface,
        // book/agents/runtime-gameplay.md).
        "scene.spawn_prefab" => Outcome::Done(crate::scene::edit(
            world,
            method,
            params,
            crate::runtime::spawn_prefab,
        )),
        "level.open" => Outcome::Done(crate::runtime::level_open(world, params)),
        "level.unload" => Outcome::Done(crate::runtime::level_unload(world, params)),
        "level.status" => Outcome::Done(crate::runtime::level_status(world, params)),
        "rules.get" => Outcome::Done(crate::runtime::rules_get(world)),
        // Match rules core (ADR-8102, book/agents/match-rules.md).
        "rules.match_get" => Outcome::Done(crate::match_rules::match_get(world, params)),
        "rules.match_setup" => Outcome::Done(crate::match_rules::match_setup(world, params)),
        "rules.match_input" => Outcome::Done(crate::match_rules::match_input(world, params)),
        "rules.match_objective" => Outcome::Done(crate::match_rules::match_objective(world, params)),
        "rules.match_step" => Outcome::Done(crate::match_rules::match_step(world, params)),
        "rules.end_stats" => Outcome::Done(crate::match_rules::end_stats(world, params)),
        "rules.series" => Outcome::Done(crate::match_rules::series(world, params)),
        "rules.vote_cast" => Outcome::Done(crate::match_rules::vote_cast(world, params)),
        "rules.vote_status" => Outcome::Done(crate::match_rules::vote_status(world, params)),
        "rules.match_assign" => Outcome::Done(crate::match_rules::match_assign(world, params)),
        "rules.spawn_preview" => Outcome::Done(crate::match_rules::spawn_preview(world, params)),
        "rules.ruleset_list" => Outcome::Done(crate::ruleset::ruleset_list(world, params)),
        "rules.ruleset_get" => Outcome::Done(crate::ruleset::ruleset_get(world, params)),
        "rules.ruleset_validate" => Outcome::Done(crate::ruleset::ruleset_validate(world, params)),
        "rules.match_report" => Outcome::Done(crate::match_rules::match_report(world, params)),
        "rules.match_results" => Outcome::Done(crate::match_results::match_results(world, params)),
        "rules.match_result" => Outcome::Done(crate::match_results::match_result(world, params)),
        "timers.list" => Outcome::Done(crate::runtime::timers_list(world)),
        "tweens.list" => Outcome::Done(crate::runtime::tweens_list(world)),
        "render.get_calibration" => Outcome::Done(crate::calibration::get(world)),
        "render.set_calibration" => Outcome::Done(crate::calibration::set(world, params)),
        "render.get_clouds" => Outcome::Done(crate::clouds::get(world)),
        "render.set_clouds" => Outcome::Done(crate::clouds::set(world, params)),
        "render.get_valley_mist" => Outcome::Done(crate::mist::get(world)),
        "render.set_valley_mist" => Outcome::Done(crate::mist::set(world, params)),
        "render.get_weather" => Outcome::Done(crate::weather::get(world)),
        "render.set_weather" => Outcome::Done(crate::weather::set(world, params)),
        "render.scene_wind" => Outcome::Done(crate::weather_effects::scene_wind(world)),
        "render.set_scene_wind" => Outcome::Done(crate::weather_effects::set_scene_wind(world, params)),
        "render.precipitation" => Outcome::Done(crate::weather_effects::precipitation(world)),
        "render.set_precipitation" => Outcome::Done(crate::weather_effects::set_precipitation(world, params)),
        "render.ambient_life" => Outcome::Done(crate::ambient_life::ambient_life(world)),
        "render.set_ambient_life" => Outcome::Done(crate::ambient_life::set_ambient_life(world, params)),
        "render.set_ambient_birds" => Outcome::Done(crate::ambient_life::set_ambient_birds(world, params)),
        "render.surface_weather" => Outcome::Done(crate::weather_effects::surface_weather(world)),
        "render.set_surface_weather" => {
            Outcome::Done(crate::weather_effects::set_surface_weather(world, params))
        }
        "render.surface_exposure" => Outcome::Done(crate::weather_effects::surface_exposure(world, params)),
        "render.swapchain" => Outcome::Done(crate::swapchain::swapchain(world)),
        "platform.paths" => Outcome::Done(crate::paths::paths(world)),
        "perf_overlay.get" => Outcome::Done(crate::perf_overlay::get(world)),
        "perf_overlay.toggle" => Outcome::Done(crate::perf_overlay::toggle(world, params)),
        "render.get_profile" => Outcome::Done(crate::render_profile::get(world)),
        "renderer.minspec_explain" => Outcome::Done(crate::minspec_explain::explain(params)),
        "geometry.body_probe" => Outcome::Done(crate::geometry_probe::body_probe_method(params)),
        "render.set_profile" => Outcome::Done(crate::render_profile::set(world, params)),
        "render.mesh_lod" => Outcome::Done(crate::render_profile::mesh_lod(world, params)),
        "render.get_sssr_cutoff" => Outcome::Done(crate::render_profile::get_sssr_cutoff()),
        "render.set_sssr_cutoff" => Outcome::Done(crate::render_profile::set_sssr_cutoff(params)),
        "render.get_exposure_settings" => Outcome::Done(crate::exposure_settings::get(world)),
        "render.set_exposure_settings" => Outcome::Done(crate::exposure_settings::set(world, params)),
        // First-launch quality detection (book/agents/auto-quality.md).
        "quality.get_detected" => Outcome::Done(crate::quality::get_detected(world)),
        "quality.detect" => Outcome::Done(crate::quality::detect_method(world, params)),
        "quality.get_terrain_vt_policy" => Outcome::Done(crate::quality::get_terrain_vt_policy(world)),
        "quality.set_terrain_vt_policy" => {
            Outcome::Done(crate::quality::set_terrain_vt_policy(world, params))
        }
        // UI input layers (book/agents/ui-input.md).
        "ui.layers" => Outcome::Done(crate::ui_layers::layers(world, params)),
        // Reusable .ui.ron components (ADR-3147, book/agents/ui-components.md).
        "ui.components" => Outcome::Done(crate::ui_components::components(world, params)),
        // UI shortcut registry (book/agents/ui-shortcuts.md).
        "ui.shortcuts" => Outcome::Done(crate::ui_shortcuts::list(world, params)),
        "ui.shortcut_trigger" => Outcome::Done(crate::ui_shortcuts::trigger(world, params)),
        "ui.shortcut_contexts" => Outcome::Done(crate::ui_shortcuts::contexts(world, params)),
        // Accessibility settings as the UI sees them (book/agents/ui-accessibility.md).
        "ui.accessibility" => Outcome::Done(crate::ui_accessibility::accessibility(world, params)),
        // Fuzzy search, anchor presets and touch gestures (book/agents/ui-fuzzy.md, ui-anchors.md, ui-touch.md).
        "ui.fuzzy" => Outcome::Done(crate::ui_parity::fuzzy(world, params)),
        "ui.set_anchor_preset" => Outcome::Done(crate::ui_parity::set_anchor_preset(world, params)),
        "ui.set_disabled" => Outcome::Done(crate::ui_parity::set_disabled(world, params)),
        "ui.debug_overlay" => Outcome::Done(crate::ui_parity::debug_overlay(world, params)),
        "ui.play_anim" => Outcome::Done(crate::ui_parity::play_anim(world, params)),
        "ui.sounds" => Outcome::Done(crate::ui_parity::sounds(world, params)),
        "ui.tree" => Outcome::Done(crate::ui_parity::tree(world, params)),
        "ui.color_picker" => Outcome::Done(crate::ui_parity::color_picker(world, params)),
        "ui.soft_cursor" => Outcome::Done(crate::ui_parity::soft_cursor(world, params)),
        "ui.keyboard" => Outcome::Done(crate::ui_parity::keyboard(world, params)),
        // Text effects and UI materials (book/agents/ui-text-style.md, ui-material.md; ADR-2045).
        "ui.text_style" => Outcome::Done(crate::ui_parity2::text_style(world, params)),
        "ui.material" => Outcome::Done(crate::ui_parity2::material(world, params)),
        "ui.data_grid" => Outcome::Done(crate::ui_parity2::data_grid(world, params)),
        "ui.touch_live" => Outcome::Done(crate::ui_parity::touch_live(world, params)),
        "ui.search_box" => Outcome::Done(crate::ui_parity2::search_box(world, params)),
        "ui.progress_dialog" => Outcome::Done(crate::ui_parity2::progress_dialog(world, params)),
        "ui.touch" => Outcome::Done(crate::ui_parity::touch(world, params)),
        // Game widgets (book/agents/ui-game-widgets.md, ADR-2958).
        "ui.lobby_list" => Outcome::Done(crate::ui_game_widgets::lobby_list(world, params)),
        "ui.scoreboard" => Outcome::Done(crate::ui_game_widgets::scoreboard(world, params)),
        "ui.radar" => Outcome::Done(crate::ui_game_widgets::radar(world, params)),
        "ui.world_markers" => Outcome::Done(crate::ui_game_widgets::world_markers(world, params)),
        "ui.target_bracket" => Outcome::Done(crate::ui_game_widgets::target_bracket(world, params)),
        // UI property tweens and the visual transform (book/agents/ui-animation.md).
        "ui.tween" => Outcome::Done(crate::ui_tween::tween(world, params)),
        // Virtualised lists and scroll views (book/agents/ui-list.md).
        "ui.list" => Outcome::Done(crate::ui_list::list(world, params)),
        "ui.scroll_to" => Outcome::Done(crate::ui_list::scroll_to(world, params)),
        // How a text node or string shapes: lines, ellipsis, missing glyphs (book/agents/ui-text.md).
        "ui.set_default_font" => Outcome::Done(crate::ui_text_shape::set_default_font(world, params)),
        "ui.text_shape" => Outcome::Done(crate::ui_text_shape::text_shape(world, params)),
        // The runtime UI inspector (book/agents/ui-inspector.md, ADR-2703).
        "ui.inspect" => Outcome::Done(crate::ui_inspect::inspect(world, params)),
        "ui.inspect_events" => Outcome::Done(crate::ui_inspect::inspect_events(world, params)),
        "ui.set_live_style" => Outcome::Done(crate::ui_inspect::set_live_style(world, params)),
        "ui.pick_at" => Outcome::Done(crate::ui_inspect::pick_at(world, params)),
        "ui.highlight" => Outcome::Done(crate::ui_inspect::highlight(world, params)),
        // The accessibility tree and screen-reader speech (book/agents/ui-a11y.md).
        "ui.a11y_tree" => Outcome::Done(crate::ui_a11y::a11y_tree(world, params)),
        "ui.a11y_announce" => Outcome::Done(crate::ui_a11y::a11y_announce(world, params)),
        "ui.a11y_act" => Outcome::Done(crate::ui_a11y::a11y_act(world, params)),
        // The screen-reader OS bridge (ADR-1998, book/agents/ui-a11y-platform.md).
        "ui.a11y_platform_tree" => Outcome::Done(crate::ui_a11y_platform::platform_tree(world, params)),
        "ui.a11y_platform_connect" => Outcome::Done(crate::ui_a11y_platform::platform_connect(world, params)),
        "ui.a11y_platform_action" => Outcome::Done(crate::ui_a11y_platform::platform_action(world, params)),
        // Accessibility defects on the live screen (ADR-3025, book/agents/ui-a11y-audit.md).
        "ui.a11y_audit" => Outcome::Done(crate::ui_a11y_audit::a11y_audit(world, params)),
        // Runtime theme switch: scheme preference, high contrast, cross-fade (book/agents/ui-theme-switch.md).
        "ui.theme_mode" => Outcome::Done(crate::ui_theme_mode::theme_mode(world, params)),
        // Icon atlas and icon nodes (book/agents/ui-icons.md).
        "ui.icons" => Outcome::Done(crate::ui_icons::icons(world, params)),
        "ui.glyph_atlas" => Outcome::Done(crate::ui_glyph_atlas::glyph_atlas(world, params)),
        // UI motion presets (book/agents/ui-motion.md).
        "ui.motion" => Outcome::Done(crate::ui_motion::motion(world, params)),
        // Scroll containers: state, nested wheel, fling, styles (book/agents/ui-scroll.md).
        "ui.scroll" => Outcome::Done(crate::ui_scroll::scroll(world, params)),
        // Text fields: keys, caret and selection (book/agents/ui-text-input.md).
        "ui.key" => Outcome::Done(crate::ui_text::key(world, params)),
        "ui.text_state" => Outcome::Done(crate::ui_text::text_state(world, params)),
        "ui.text_select" => Outcome::Done(crate::ui_text::text_select(world, params)),
        // SA-script (Thai/Lao/Khmer/Myanmar) dictionary line-break points (book/agents/ui-localization.md).
        "ui.text_breaks" => Outcome::Done(crate::ui_text::text_breaks(world, params)),
        "render.camera_cut" => Outcome::Done(crate::camera_cut::camera_cut(world, params)),
        // Offline fixed-dt render-out (book/agents/render-out.md).
        "render.out_start" => Outcome::Done(crate::render_out::start(world, params)),
        "render.out_progress" => Outcome::Done(crate::render_out::progress(world, params)),
        "render.out_cancel" => Outcome::Done(crate::render_out::cancel(world, params)),
        // Sequence-driven render-out from a job file (book/agents/render-jobs.md, ADR-1616).
        "render.out_job" => Outcome::Done(crate::render_out::out_job(world, params)),
        // Camera paths and shot sequences (book/agents/camera-paths.md).
        "render.sequence" => Outcome::Done(crate::shots::render_sequence(world, params)),
        "camera.set_path" => Outcome::Done(crate::shots::camera_set_path(world, params)),
        "camera.raycast" => Outcome::Done(crate::camera_raycast::camera_raycast(world, params)),
        // Runtime UI: see and drive regia_ui nodes (book/agents/ui.md).
        "ui.query" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.state" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.click" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.focus" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.type" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.set_value" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.set_locale" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.resolve" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.set_backdrop_blur" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.backdrop_stats" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.drag_drop" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.view_models" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.collection_state" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        // Group opacity, masks and scale modes (ADR-2866, book/agents/ui-group-layers.md).
        "ui.set_group" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.layer_stats" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.set_scale_mode" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.image_assets" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        // Camera-target and world-space UI (ADR-2406, book/agents/ui.md).
        "ui.world_click" => Outcome::Done(crate::ui::world_click(world, params)),
        "ui.set_target" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.targets" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        // Popups and menus (book/agents/ui-menus.md).
        "ui.open_popup" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.close_popup" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.menu" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        "ui.menu_activate" => Outcome::Done(crate::ui::dispatch(world, method, params)),
        // Runtime UI themes (book/agents/ui-theme.md).
        "ui.theme_list" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "ui.theme_get" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "ui.theme_set" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "ui.theme_select" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "ui.theme_preview" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "ui.theme_lint" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "ui.theme_reload" => Outcome::Done(crate::ui::dispatch_theme(world, method, params)),
        "level.get_look" => Outcome::Done(crate::look::get_look(world)),
        "level.set_look" => Outcome::Done(crate::look::set_look(world, params)),
        // The level's baked GI volume (book/agents/baked-gi-maps.md).
        "level.get_baked_gi" => Outcome::Done(crate::level_baked_gi::get_baked_gi(world)),
        // The level's bake input (book/agents/baked-gi-export.md).
        "level.export_bake" => Outcome::Done(crate::bake_export::export_bake(world, params)),
        // Several baked GI volumes and their seams (book/agents/baked-gi-seams.md).
        "level.get_baked_gi_stream" => Outcome::Done(crate::level_baked_gi_stream::get_stream(world)),
        "level.set_baked_gi_stream" => Outcome::Done(crate::level_baked_gi_stream::set_stream(world, params)),
        "level.sample_baked_gi" => Outcome::Done(crate::level_baked_gi_stream::sample(world, params)),
        // Level directories, data layers and placed levels (book/agents/levels-layers-instances.md).
        "level.storage" => Outcome::Done(crate::level_tools::storage(world, params)),
        "level.convert" => Outcome::Done(crate::level_tools::convert(world, params)),
        "level.layers" => Outcome::Done(crate::level_tools::layers(world, params)),
        "level.set_layer_state" => Outcome::Done(crate::level_tools::set_layer_state(world, params)),
        "level.set_membership" => Outcome::Done(crate::level_tools::set_membership(world, params)),
        "level.placed" => Outcome::Done(crate::level_tools::placed_list(world)),
        "level.place_level" => Outcome::Done(crate::level_tools::place_level(world, params)),
        "level.break_placed" => Outcome::Done(crate::level_tools::break_placed(world, params)),
        // The level switch with cancel (ADR-8027, `book/agents/level-switch.md`).
        "level.switch" => Outcome::Done(crate::level_tools::switch(world, params)),
        "level.switch_status" => Outcome::Done(crate::level_tools::switch_status(world)),
        // Diffuse GI source (book/agents/gi-probes.md).
        "render.get_forward_lite" => Outcome::Done(crate::forward_lite::get_forward_lite(world)),
        "render.set_forward_lite" => Outcome::Done(crate::forward_lite::set_forward_lite(world, params)),
        // The 8-bit output dither (book/agents/output-dither.md, ADR-2486).
        "render.get_output_dither" => Outcome::Done(crate::output_dither::get_output_dither(world)),
        "render.set_output_dither" => Outcome::Done(crate::output_dither::set_output_dither(world, params)),
        "render.get_star_flux" => Outcome::Done(crate::output_dither::get_star_flux(world)),
        "render.set_star_flux" => Outcome::Done(crate::output_dither::set_star_flux(world, params)),
        "render.get_sun_shadow_range" => Outcome::Done(crate::sun_shadow_range::get(world)),
        "render.set_sun_shadow_range" => Outcome::Done(crate::sun_shadow_range::set(world, params)),
        "render.get_gi_mode" => Outcome::Done(crate::gi_mode::get_gi_mode(world)),
        "render.set_gi_mode" => Outcome::Done(crate::gi_mode::set_gi_mode(world, params)),
        // Plates and the reference overlay (book/agents/plates.md).
        "seeing.capture_plate" => Outcome::Done(crate::plates::capture_plate(world, params)),
        "render.set_reference_overlay" => {
            Outcome::Done(crate::reference_overlay::set_reference_overlay(world, params))
        }
        "render.set_gi_occluders" => Outcome::Done(crate::gi_mode::set_gi_occluders(world, params)),
        "render.sample_baked_gi" => Outcome::Done(crate::gi_mode::sample_baked_gi(world, params)),
        // Baked GI debugging views (book/agents/gi-debug-views.md).
        "render.get_gi_probes" => Outcome::Done(crate::gi_debug::get_gi_probes(world, params)),
        "render.explain_gi" => Outcome::Done(crate::gi_debug::explain_gi(world, params)),
        "render.set_gi_debug_overlay" => Outcome::Done(crate::gi_debug::set_gi_debug_overlay(world, params)),
        "render.set_baked_gi_ao" => Outcome::Done(crate::gi_mode::set_baked_gi_ao(world, params)),
        // ZH3 or L1 evaluation of the baked SH (book/agents/gi-sampling.md, ADR-1377).
        "render.set_gi_sampling" => Outcome::Done(crate::gi_sampling::set_gi_sampling(world, params)),
        // Leak control of the baked GI lookup (book/agents/baked-gi-visibility.md).
        "render.get_baked_gi_visibility" => Outcome::Done(crate::baked_gi_visibility::get_visibility(world)),
        "render.set_baked_gi_visibility" => {
            Outcome::Done(crate::baked_gi_visibility::set_visibility(world, params))
        }
        "render.problems" => Outcome::Done(crate::look::render_problems(world)),
        "render.memory_pressure" => Outcome::Done(crate::look::memory_pressure(world)),
        "render.mesh_occluders" => Outcome::Done(crate::look::render_mesh_occluders(world)),
        "render.emissive_lights" => Outcome::Done(crate::look::render_emissive_lights(world)),
        // Hardware ray tracing state (book/agents/hwrt.md, ADR-515).
        "render.hwrt" => Outcome::Done(crate::hwrt::render_hwrt(world, params)),
        "render.blue_noise" => Outcome::Done(crate::blue_noise::render_blue_noise(world, params)),
        "render.set_blue_noise" => Outcome::Done(crate::blue_noise::set_blue_noise(world, params)),
        "render.set_sun_shadow_trace" => Outcome::Done(crate::hwrt::set_sun_shadow_trace(world, params)),
        // The terrain's caster proxy for the traced sun (book/agents/hwrt.md, ADR-2310).
        "terrain.trace_proxy" => Outcome::Done(crate::terrain_trace_proxy::get(world)),
        "terrain.set_trace_proxy" => Outcome::Done(crate::terrain_trace_proxy::set(world, params)),
        // CPU frame pipelining (book/agents/frame-pipelining.md, ADR-2074).
        "render.frame_pipelining" => Outcome::Done(crate::frame_pipelining::frame_pipelining(world)),
        "render.set_frame_pipelining" => {
            Outcome::Done(crate::frame_pipelining::set_frame_pipelining(world, params))
        }
        "sim.counters" => Outcome::Done(crate::framework::sim_counters(world)),
        "sim.check_framerate_independence" => {
            Outcome::Done(crate::framework::sim_check_framerate_independence(world, params))
        }
        // Per-viewer fog of war (book/agents/fog-of-war.md, ADR-8154).
        "fog.status" => Outcome::Done(crate::fog::status(world)),
        "fog.set_viewer" => Outcome::Done(crate::fog::set_viewer(world, params)),
        // The threaded sim host (ADR-8083, book/agents/framework.md).
        "sim.host_status" => Outcome::Done(crate::framework::sim_host_status(world)),
        "sim.host_hold" => Outcome::Done(crate::framework::sim_host_hold(world, params)),
        "sim.host_step" => Outcome::Done(crate::framework::sim_host_step(world, params)),
        "sim.tick_profile" => Outcome::Done(crate::framework::sim_tick_profile(world, params)),
        "sim.host_snapshot" => Outcome::Done(crate::framework::sim_host_snapshot(world, params)),
        "sim.host_restore" => Outcome::Done(crate::framework::sim_host_restore(world, params)),
        "sim.bridge_settings" => Outcome::Done(crate::framework::sim_bridge_settings(world)),
        "sim.set_bridge_settings" => Outcome::Done(crate::framework::sim_set_bridge_settings(world, params)),
        "fog.texture_stats" => Outcome::Done(crate::fog::texture_stats(world)),
        "fog.set_look" => Outcome::Done(crate::fog::set_look(world, params)),
        "sim.det_debug_draw" => Outcome::Done(crate::framework::sim_det_debug_draw(world, params)),
        "sim.grid_flow" => Outcome::Done(crate::sim_grid_flow::grid_flow(world, params)),

        // Animation (specs/17, specs/23 § 3).
        "anim.list" => Outcome::Done(crate::anim::list(world)),
        "anim.get_state" => Outcome::Done(crate::anim::get_state(world, params)),
        "anim.set_param" => Outcome::Done(crate::anim::set_param(world, params)),
        "anim.ragdoll_set" => Outcome::Done(crate::anim::ragdoll_set(world, params)),
        "anim.ragdoll_hit" => Outcome::Done(crate::anim::ragdoll_hit(world, params)),
        "anim.spring_list" => Outcome::Done(crate::anim::spring_list(world)),
        "anim.spring_set" => Outcome::Done(crate::anim::spring_set(world, params)),
        "anim.spring_reset" => Outcome::Done(crate::anim::spring_reset(world, params)),
        "anim.rig_attach" => Outcome::Done(crate::rig::rig_attach(world, params)),
        "anim.rig_state" => Outcome::Done(crate::rig::rig_state(world, params)),
        "anim.profile_check" => Outcome::Done(crate::rig::profile_check(world, params)),
        // `.gait.ron` gait settings, checked by the loader (book/agents/procedural-gait.md, gap G7).
        "anim.gait_validate" => Outcome::Done(crate::gait_asset::gait_validate(world, params)),
        // Morph weights end to end (book/agents/morph-weights.md).
        "anim.morph_weights" => Outcome::Done(crate::morph::get(world, params)),
        "anim.set_morph_weights" => Outcome::Done(crate::morph::set(world, params)),

        "anim.gaze_get" => Outcome::Done(crate::anim::gaze_get(world, params)),
        "anim.gaze_set" => Outcome::Done(crate::anim::gaze_set(world, params)),
        "anim.bind_pose_get" => Outcome::Done(crate::anim::bind_pose_get(world, params)),
        "anim.bind_pose_set" => Outcome::Done(crate::anim::bind_pose_set(world, params)),
        "anim.clip_report" => Outcome::Done(crate::anim::clip_report(world, params)),
        "anim.motion_match" => Outcome::Done(crate::motion_matching::get(world, params)),
        "anim.motion_match_set" => Outcome::Done(crate::motion_matching::set(world, params)),
        "anim.motion_match_database" => Outcome::Done(crate::motion_matching::database(world, params)),
        "anim.motion_match_databases" => Outcome::Done(crate::motion_matching::databases(world, params)),
        "anim.mm_predict" => Outcome::Done(crate::motion_matching::predict(world, params)),
        // Locomotion quality (ADR-8708, book/agents/locomotion-quality.md).
        "anim.locomotion_state" => Outcome::Done(crate::locomotion::locomotion_state(world, params)),
        "anim.gait_simulate" => Outcome::Done(crate::locomotion::gait_simulate(world, params)),
        "anim.foot_plant_simulate" => Outcome::Done(crate::locomotion::foot_plant_simulate(world, params)),
        "anim.ik_report" => Outcome::Done(crate::locomotion::ik_report(world, params)),

        // Physics scene queries (specs/15 § 3, specs/23 § 3).
        "physics.raycast" => Outcome::Done(crate::physics::raycast(world, params)),
        "physics.overlap" => Outcome::Done(crate::physics::overlap(world, params)),
        "physics.closest_point" => Outcome::Done(crate::physics::closest_point(world, params)),
        "physics.stats" => Outcome::Done(crate::physics::stats(world, params)),
        // Gameplay force fields (ADR-3348, book/agents/physics-fields.md).
        "physics.fields" => Outcome::Done(crate::physics_fields::fields(world, params)),
        "physics.field_set" => Outcome::Done(crate::physics_fields::field_set(world, params)),
        "physics.field_probe" => Outcome::Done(crate::physics_fields::field_probe(world, params)),
        // Angular CCD sweep between two shapes or entities (ADR-2423).
        "physics.time_of_impact" => Outcome::Done(crate::physics_toi::time_of_impact(world, params)),
        // Convex decomposition of a trimesh collider (book/agents/convex-decomposition.md).
        "physics.convex_decompose" => {
            Outcome::Done(crate::convex_decomposition::convex_decompose(world, params))
        }
        // Ray-cast vehicles (book/agents/vehicles.md).
        "physics.vehicles" => Outcome::Done(crate::vehicles::vehicles(world, params)),
        "physics.vehicle_input" => Outcome::Done(crate::vehicles::vehicle_input(world, params)),
        "physics.vehicle_reset" => Outcome::Done(crate::vehicles::vehicle_reset(world, params)),
        "physics.vehicle_telemetry" => Outcome::Done(crate::vehicles::vehicle_telemetry(world, params)),
        "physics.vehicle_shift" => Outcome::Done(crate::vehicles::vehicle_shift(world, params)),
        "physics.vehicle_tire_curve" => Outcome::Done(crate::vehicles::vehicle_tire_curve(world, params)),
        "physics.hovercraft" => Outcome::Done(crate::hovercraft::hovercraft(world, params)),
        "physics.vehicle_config_check" => {
            Outcome::Done(crate::hovercraft::vehicle_config_check(world, params))
        }
        // Physics debug draw and named materials (book/agents/physics-spec15.md, ADR-2424).
        "physics.debug_draw" => Outcome::Done(crate::physics::debug_draw(world, params)),
        "physics.parented_bodies" => Outcome::Done(crate::physics::parented_bodies(world, params)),
        "physics.material_check" => Outcome::Done(crate::physics::material_check(world, params)),
        "physics.material_register" => Outcome::Done(crate::physics::material_register(world, params)),
        // The explicit teleport: velocity kept or cleared, no swept contacts (ADR-7233).
        "physics.teleport" => Outcome::Done(crate::physics::teleport(world, params)),
        // Active ragdolls: PD motors, balance, get-up (book/agents/ragdoll-active.md, ADR-2498).
        "ragdoll.state" => Outcome::Done(crate::ragdoll_active::state(world, params)),
        "ragdoll.active_set" => Outcome::Done(crate::ragdoll_active::active_set(world, params)),

        // Render-side raycast over terrain and mesh layers (book/agents/world-pick.md).
        "world.pick" => Outcome::Done(crate::pick::pick(world, params)),

        // Terrain scatter (specs/26 § 1).
        "terrain.scatter" => Outcome::Done(crate::terrain::scatter(world)),
        "terrain.set_scatter" => Outcome::Done(crate::terrain::set_scatter(world, params)),
        "terrain.stroke" => Outcome::Done(crate::terrain::stroke(world, params)),
        // Non-destructive edit layers (ADR-7825).
        "terrain.edit_layers" => Outcome::Done(crate::terrain_edit_layers::list(world, params)),
        "terrain.edit_layer_create" => Outcome::Done(crate::terrain_edit_layers::create(world, params)),
        "terrain.edit_layer_set" => Outcome::Done(crate::terrain_edit_layers::set(world, params)),
        "terrain.edit_layer_delete" => Outcome::Done(crate::terrain_edit_layers::delete(world, params)),
        "terrain.edit_layer_reorder" => Outcome::Done(crate::terrain_edit_layers::reorder(world, params)),
        "terrain.edit_layer_flatten" => Outcome::Done(crate::terrain_edit_layers::flatten(world, params)),
        "terrain.edit_layer_weights" => Outcome::Done(crate::terrain_edit_layers::weights(world, params)),
        "terrain.edit_layer_paint_weights" => {
            Outcome::Done(crate::terrain_edit_layers::paint_weights(world, params))
        }
        "terrain.edit_layer_procgen" => Outcome::Done(crate::terrain_edit_layers::procgen(world, params)),
        "terrain.holes" => Outcome::Done(crate::terrain::holes(world)),
        "terrain.set_holes" => Outcome::Done(crate::terrain::set_holes(world, params)),
        "terrain.load_holes" => Outcome::Done(crate::terrain::load_holes(world, params)),
        "terrain.raycast" => Outcome::Done(crate::terrain::raycast(world, params)),
        "terrain.set_hole" => Outcome::Done(crate::terrain_runtime::set_hole(world, params)),
        "terrain.seam_report" => Outcome::Done(crate::terrain_seam::seam_report(world, params)),
        "terrain.save_holes" => Outcome::Done(crate::terrain_runtime::save_holes(world, params)),
        "terrain.save_paint" => Outcome::Done(crate::terrain_heights::save_paint(world, params)),
        "terrain.load_paint" => Outcome::Done(crate::terrain_heights::load_paint(world, params)),
        "terrain.patch_heights" => Outcome::Done(crate::terrain_runtime::patch_heights(world, params)),
        // Heightfield operators over the first terrain (book/agents/terrain-ops.md, ADR-8465).
        "terrain.apply_operator" => Outcome::Done(crate::terrain_ops::apply_operator(world, params)),
        "terrain.get" => Outcome::Done(crate::terrain_runtime::get(world, params)),
        // Covered overview flags (ADR-8333). The caller supplies underground; this does not query a mesh.
        "terrain.unit_view" => Outcome::Done(crate::unit_view::unit_view(world, params)),
        "terrain.backdrop_status" => Outcome::Done(crate::terrain_runtime::backdrop_status(world, params)),
        // The DCC scene feed (specs/60 § 5, book/agents/dcc-feed.md, ADR-1891).
        "dcc_feed.status" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.items" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.accept" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.disconnect" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.record" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.replay" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.listen" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.rotate_token" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.bake" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        "dcc_feed.bake_status" => Outcome::Done(crate::dcc_feed_surface::call(world, method, params)),
        // Procedural generators and graphs (specs/62 § 9, book/agents/procgen.md, ADR-1640).
        "procgen.list" => Outcome::Done(crate::procgen::list(world, params)),
        "procgen.get" => Outcome::Done(crate::procgen::get(world, params)),
        "procgen.run" => Outcome::Done(crate::procgen::run(world, params)),
        "procgen.cancel" => Outcome::Done(crate::procgen::cancel(world, params)),
        "procgen.clear" => Outcome::Done(crate::procgen::clear(world, params)),
        "procgen.report" => Outcome::Done(crate::procgen::report(world, params)),
        "procgen.inspect" => Outcome::Done(crate::procgen::inspect(world, params)),
        "procgen.graph_get" => Outcome::Done(crate::procgen::graph_get(world, params)),
        "procgen.graph_edit" => Outcome::Done(crate::procgen::graph_edit(world, params)),
        "procgen.node_types" => Outcome::Done(crate::procgen::node_types(world, params)),
        "procgen.bake" => Outcome::Done(crate::procgen::bake(world, params)),
        "procgen.verify" => Outcome::Done(crate::procgen::verify(world, params)),
        "procgen.set_debug" => Outcome::Done(crate::procgen_tiles::set_debug(world, params)),
        "procgen.detach" => Outcome::Done(crate::procgen::detach(world, params)),
        // Content chunks (book/agents/content-chunks.md, spec 40).
        "content.chunks" => Outcome::Done(crate::content::chunks(world, params)),
        "content.chunk" => Outcome::Done(crate::content::chunk(world, params)),
        "content.why" => Outcome::Done(crate::content::why(world, params)),
        "content.request" => Outcome::Done(crate::content::request(world, params)),
        "content.progress" => Outcome::Done(crate::content::progress(world, params)),
        "content.pause" => Outcome::Done(crate::content::pause(world, params)),
        "content.resume" => Outcome::Done(crate::content::resume(world, params)),
        "content.cancel" => Outcome::Done(crate::content::cancel(world, params)),
        "content.approve_metered" => Outcome::Done(crate::content::approve_metered(world, params)),
        "content.mount" => Outcome::Done(crate::content::mount(world, params)),
        "content.unmount" => Outcome::Done(crate::content::unmount(world, params)),
        "content.release" => Outcome::Done(crate::content::release(world, params)),
        "content.set_source" => Outcome::Done(crate::content::set_source(world, params)),
        "content.check_host" => Outcome::Done(crate::content::check_host(world, params)),
        "content.inject" => Outcome::Done(crate::content::inject(world, params)),
        "content.network" => Outcome::Done(crate::content::network(world, params)),
        // Spline components (specs/64 § 2.3, book/agents/splines.md).
        "spline.get" => Outcome::Done(crate::splines::get(world, params)),
        "spline.set" => Outcome::Done(crate::splines::set(world, params)),
        "spline.edit" => Outcome::Done(crate::splines::edit(world, params)),
        "spline.sample" => Outcome::Done(crate::splines::sample(world, params)),
        "spline.closest" => Outcome::Done(crate::splines::closest(world, params)),
        // Biomes (specs/65 § 10, book/agents/biomes.md, ADR-1665).
        "biome.areas" => Outcome::Done(crate::biome::areas(world, params)),
        "biome.claim" => Outcome::Done(crate::biome::claim(world, params)),
        "biome.stats" => Outcome::Done(crate::biome::stats(world, params)),
        "biome.def_get" => Outcome::Done(crate::biome::def_get(world, params)),
        "biome.def_edit" => Outcome::Done(crate::biome::def_edit(world, params)),
        "biome.preview" => Outcome::Done(crate::biome::preview(world, params)),
        "biome.validate_ecology" => Outcome::Done(crate::biome::validate_ecology(world, params)),
        // Tiled and anchored generation (specs/66 § 6, book/agents/procgen.md "Tiles and anchors").
        "procgen.tiles" => Outcome::Done(crate::procgen_tiles::tiles(world, params)),
        "procgen.anchors" => Outcome::Done(crate::procgen_tiles::anchors(world, params)),
        "procgen.tiles_set" => Outcome::Done(crate::procgen_tiles::tiles_set(world, params)),
        "procgen.tiles_rebuild" => Outcome::Done(crate::procgen_tiles::tiles_rebuild(world, params)),
        "procgen.tiles_wait" => match crate::procgen_tiles::tiles_wait(world, params, id, reply) {
            Ok(()) => Outcome::Deferred,
            Err(error) => Outcome::Done(Err(error)),
        },
        // Plant graphs (specs/68 § 8).
        "procgen.plant_stats" => Outcome::Done(crate::procgen::plant_stats(world, params)),
        "procgen.plant_build" => Outcome::Done(crate::procgen::plant_build(world, params)),
        // Wave Function Collapse (regia_procgen::wfc, book/agents/procgen.md).
        "procgen.wfc_solve" => Outcome::Done(crate::procgen_wfc::wfc_solve(world, params)),
        // Schedule profiling (spec 62 § 5.1, ADR-2724, book/agents/procgen.md).
        "procgen.profile" => Outcome::Done(crate::procgen_profile::profile(world, params)),
        // Video playback (specs/50, ADR-1670, book/agents/video-playback.md).
        "video.probe" => Outcome::Done(crate::video::probe(world, params)),
        "video.list" => Outcome::Done(crate::video::list(world)),
        "video.status" => Outcome::Done(crate::video::status(world, params)),
        "video.frame_hash" => Outcome::Done(crate::video::frame_hash(world, params)),
        "video.open" => Outcome::Done(crate::video::open(world, params)),
        "video.play" => Outcome::Done(crate::video::play(world, params)),
        "video.pause" => Outcome::Done(crate::video::pause(world, params)),
        "video.seek" => Outcome::Done(crate::video::seek(world, params)),
        "video.step" => Outcome::Done(crate::video::step(world, params)),
        "video.set_rate" => Outcome::Done(crate::video::set_rate(world, params)),
        "video.set_loop" => Outcome::Done(crate::video::set_loop(world, params)),
        "video.set_volume" => Outcome::Done(crate::video::set_volume(world, params)),
        "video.select_track" => Outcome::Done(crate::video::select_track(world, params)),
        "video.skip" => Outcome::Done(crate::video::skip(world, params)),
        "video.close" => Outcome::Done(crate::video::close(world, params)),
        "video.create_player" => Outcome::Done(crate::video::create_player(world, params)),
        // Dynamic GI tiers (book/agents/dynamic-gi.md, ADR-1679).
        "render.get_dynamic_gi" => Outcome::Done(crate::dynamic_gi::get_dynamic_gi(world)),
        "render.set_dynamic_gi" => Outcome::Done(crate::dynamic_gi::set_dynamic_gi(world, params)),
        "video.codecs" => Outcome::Done(crate::video::codecs()),
        // Video renderer half (book/agents/media-render.md, ADR-2044).
        "media.state" => Outcome::Done(crate::media::state(world, params)),
        "media.play" => Outcome::Done(crate::media::play(world, params)),
        "media.pause" => Outcome::Done(crate::media::pause(world, params)),
        "media.seek" => Outcome::Done(crate::media::seek(world, params)),
        "media.set_loop" => Outcome::Done(crate::media::set_loop(world, params)),
        // Boot movies, cues, visibility, decode pool, validation (ADR-2410, book/agents/video-boot-and-cues.md).
        "media.boot_state" => Outcome::Done(crate::media_boot::boot_state(world)),
        "media.boot_skip" => Outcome::Done(crate::media_boot::boot_skip(world)),
        "media.set_visible" => Outcome::Done(crate::media_boot::set_visible(world, params)),
        "media.cues" => Outcome::Done(crate::media_boot::cues(world, params)),
        "media.decoders" => Outcome::Done(crate::media_boot::decoders(world)),
        "media.validate" => Outcome::Done(crate::media_boot::validate(world, params)),
        // Heightmap export/import and the shared wind (ADR-1853).
        "terrain.export_heights" => Outcome::Done(crate::terrain_heights::export(world, params)),
        "terrain.import_heights" => Outcome::Done(crate::terrain_heights::import(world, params)),
        "terrain.wind" => Outcome::Done(crate::terrain_heights::wind(world)),
        "terrain.set_wind" => Outcome::Done(crate::terrain_heights::set_wind(world, params)),
        // Two-phase prepass occlusion (book/agents/occlusion.md, ADR-1887).
        "render.get_occlusion" => Outcome::Done(crate::occlusion::get_occlusion(world)),
        "render.set_occlusion" => Outcome::Done(crate::occlusion::set_occlusion(world, params)),
        "render.set_shadow_occlusion" => Outcome::Done(crate::occlusion::set_shadow_occlusion(world, params)),
        "render.get_radiance_cache" => Outcome::Done(crate::radiance_cache::get_radiance_cache(world)),
        "render.set_radiance_cache" => {
            Outcome::Done(crate::radiance_cache::set_radiance_cache(world, params))
        }
        // Painted mesh instances (ADR-1936).
        "terrain.scatter_stroke" => Outcome::Done(crate::terrain_scatter_paint::stroke(world, params)),
        "terrain.scatter_paint" => Outcome::Done(crate::terrain_scatter_paint::report(world, params)),
        "terrain.save_scatter_paint" => Outcome::Done(crate::terrain_scatter_paint::save(world, params)),
        "terrain.load_scatter_paint" => Outcome::Done(crate::terrain_scatter_paint::load(world, params)),
        // Renderer toggles (book/agents/contact-shadows.md, reflection-occlusion.md).
        "render.set_contact_shadows" => Outcome::Done(crate::render::set_contact_shadows(world, params)),
        "render.set_translucent_shadows" => {
            Outcome::Done(crate::render::set_translucent_shadows(world, params))
        }
        "render.set_reflection_occlusion" => {
            Outcome::Done(crate::render::set_reflection_occlusion(world, params))
        }
        // Bent normals (book/agents/bent-normals.md, ADR-1375).
        "render.set_bent_normals" => Outcome::Done(crate::render::set_bent_normals(world, params)),
        "render.set_near_sun_shadow" => Outcome::Done(crate::render::set_near_sun_shadow(world, params)),
        // Studio gradient / HDR equirect environment (book/agents/studio-environment.md, ADR-1653).
        "render.set_studio_environment" => Outcome::Done(crate::studio_environment::set(world, params)),
        // The studio frame (book/agents/studio-environment.md, book/agents/local-capture.md,
        // ADR-2000).
        "render.get_studio_environment" => Outcome::Done(crate::studio_lighting::get(world)),
        "render.set_environment_ground" => Outcome::Done(crate::studio_lighting::set_ground(world, params)),
        "render.set_studio_background" => {
            Outcome::Done(crate::studio_lighting::set_background(world, params))
        }
        "render.get_debug_view" => Outcome::Done(crate::studio_lighting::get_debug_view(world)),
        "render.set_debug_view" => Outcome::Done(crate::studio_lighting::set_debug_view(world, params)),
        // Directional lights and ambient volumes (book/agents/render-lights.md, ADR-7211).
        "render.lights" => Outcome::Done(crate::render_lights::lights(world)),
        "render.place_local_capture" => Outcome::Done(crate::studio_lighting::place_capture(world, params)),
        "render.bake_local_capture" => Outcome::Done(crate::studio_lighting::bake_capture(world, params)),
        "render.list_local_captures" => Outcome::Done(crate::studio_lighting::list_captures(world)),
        "render.remove_local_capture" => Outcome::Done(crate::studio_lighting::remove_capture(world, params)),
        // Character polish and the ragdoll budget (book/agents/character-physics.md, ADR-1693).
        "physics.characters" => Outcome::Done(crate::character_physics::characters(world, params)),
        "anim.ragdoll_settings" => Outcome::Done(crate::character_physics::ragdoll_settings(world, params)),
        // Traversal moves (book/agents/character-traversal.md, ADR-2971).
        "physics.character_traversal" => {
            Outcome::Done(crate::character_traversal::character_traversal(world, params))
        }
        "physics.character_traversal_probe" => Outcome::Done(
            crate::character_traversal::character_traversal_probe(world, params),
        ),
        // Sun cascade stability (book/agents/shadow-stability.md, ADR-1236).
        "render.shadow_stability" => Outcome::Done(crate::shadow_stability::shadow_stability(world, params)),
        "terrain.vt" => Outcome::Done(crate::terrain::vt(world)),
        "terrain.set_vt" => Outcome::Done(crate::terrain::set_vt(world, params)),
        "terrain.water" => Outcome::Done(crate::terrain::water(world, params)),
        "terrain.set_water" => Outcome::Done(crate::terrain::set_water(world, params)),
        "terrain.grass" => Outcome::Done(crate::terrain_grass::grass(world)),
        "terrain.set_grass" => Outcome::Done(crate::terrain_grass::set_grass(world, params)),
        "terrain.parity" => Outcome::Done(crate::terrain_parity::parity(world, params)),
        // Material graphs (specs/22 § T2.4, book/agents/material-graph.md).
        "material.graph_get" => Outcome::Done(crate::material::graph_get(world, params)),
        "material.graph_set" => Outcome::Done(crate::material::graph_set(world, params)),
        "material.compile" => Outcome::Done(crate::material::compile(world, params)),
        // Character shading models: skin, eye, fabric sheen (book/agents/shading-models.md).
        "material.shading" => Outcome::Done(crate::material_shading::get(world, params)),
        "material.set_shading" => Outcome::Done(crate::material_shading::set(world, params)),
        // Per-vertex ambient occlusion (book/agents/vertex-ao.md, ADR-1512).
        "material.vertex_ao" => Outcome::Done(crate::material_vertex_ao::get(world, params)),
        "material.set_vertex_ao" => Outcome::Done(crate::material_vertex_ao::set(world, params)),
        // The thin film (book/agents/thin-film.md, ADR-1977).
        "material.thin_film" => Outcome::Done(crate::material_thin_film::get(world, params)),
        "material.set_thin_film" => Outcome::Done(crate::material_thin_film::set(world, params)),
        "material.subsurface" => Outcome::Done(crate::subsurface::get_material(world, params)),
        "material.set_subsurface" => Outcome::Done(crate::subsurface::set_material(world, params)),
        "render.subsurface" => Outcome::Done(Ok(crate::subsurface::report(world))),
        "render.set_subsurface" => Outcome::Done(crate::subsurface::set_enabled(world, params)),
        "material.rough_transmission" => {
            Outcome::Done(crate::material_rough_transmission::get(world, params))
        }
        "material.set_rough_transmission" => {
            Outcome::Done(crate::material_rough_transmission::set(world, params))
        }
        "render.oit" => Outcome::Done(Ok(crate::oit::report(world))),
        "render.set_oit" => Outcome::Done(crate::oit::set_enabled(world, params)),
        // Navigation (specs/22 § T2.2, specs/23 § 3).
        "nav.get_mesh" => Outcome::Done(crate::nav::get_mesh(world, params)),
        "nav.find_nearest" => Outcome::Done(crate::nav::find_nearest(world, params)),
        "nav.find_path" => Outcome::Done(crate::nav::find_path(world, params)),
        "nav.plan" => Outcome::Done(crate::nav::plan(world, params)),
        "nav.add_obstacle" => Outcome::Done(crate::nav::add_obstacle(world, params)),
        "nav.remove_obstacle" => Outcome::Done(crate::nav::remove_obstacle(world, params)),
        "nav.check_stair" => Outcome::Done(crate::nav::check_stair(world, params)),
        // Smart objects (specs/31, book/agents/smart-objects.md).
        "functest.list" => Outcome::Done(crate::functest::list(world, params)),
        "functest.run" => Outcome::Done(crate::functest::run(world, params)),
        "functest.status" => Outcome::Done(crate::functest::status(world, params)),
        "functest.results" => Outcome::Done(crate::functest::results(world, params)),
        "functest.abort" => Outcome::Done(crate::functest::abort(world, params)),
        "functest.create" => Outcome::Done(crate::functest::create(world, params)),
        "functest.bless" => Outcome::Done(crate::functest::bless(world, params)),
        "smartobject.list" => Outcome::Done(crate::smart_object::list(world, params)),
        "smartobject.claim" => Outcome::Done(crate::smart_object::claim(world, params)),
        "smartobject.release" => Outcome::Done(crate::smart_object::release(world, params)),
        // Smart objects v2 (specs/39).
        "smartobject.search" => Outcome::Done(crate::smart_object_v2::search(world, params)),
        "smartobject.begin_use" => Outcome::Done(crate::smart_object_v2::begin_use_method(world, params)),
        "smartobject.set_enabled" => Outcome::Done(crate::smart_object_v2::set_enabled_method(world, params)),
        "smartobject.tags" => Outcome::Done(crate::smart_object_v2::tags(world, params)),
        "smartobject.set_definition" => Outcome::Done(crate::smart_object_v2::set_definition(world, params)),
        "smartobject.messages" => Outcome::Done(crate::smart_object_v2::messages(world, params)),
        // State trees (specs/38, book/agents/state-tree.md).
        "statetree.compile" => Outcome::Done(crate::state_tree::compile(world, params)),
        "statetree.control" => Outcome::Done(crate::state_tree::control(world, params)),
        "statetree.reload" => Outcome::Done(crate::state_tree::reload(world, params)),
        "statetree.debug" => Outcome::Done(crate::state_tree::debug(world, params)),
        // Logic graphs (specs/102 phase 1, book/agents/logic-graph.md).
        "logic.compile" => Outcome::Done(crate::logic::compile(world, params)),
        "logic.control" => Outcome::Done(crate::logic::control(world, params)),
        "logic.emit" => Outcome::Done(crate::logic::emit(world, params)),
        "logic.reload" => Outcome::Done(crate::logic::reload(world, params)),
        "logic.status" => Outcome::Done(crate::logic::status(world, params)),
        "logic.debug" => Outcome::Done(crate::logic::debug(world, params)),
        "logic.world" => Outcome::Done(crate::logic::world(world, params)),
        "logic.world_set" => Outcome::Done(crate::logic::world_set(world, params)),
        // Spatial queries for AI (specs/33, book/agents/spatial-queries.md).
        "ai.query_list" => Outcome::Done(crate::spatial_query::list(world, params)),
        "ai.query_run" => Outcome::Done(crate::spatial_query::run(world, params)),
        "ai.query_submit" => Outcome::Done(crate::spatial_query::submit(world, params)),
        "ai.query_poll" => Outcome::Done(crate::spatial_query::poll(world, params)),
        "ai.query_abort" => Outcome::Done(crate::spatial_query::abort(world, params)),
        "ai.query_stats" => Outcome::Done(crate::spatial_query::stats(world, params)),
        "ai.query_probe" => Outcome::Done(crate::spatial_query::probe(world, params)),
        // Behavior tree editor (ADR-2408, book/agents/behavior-tree-editor.md).
        "ai.bt_get" => Outcome::Done(crate::ai_bt::get(world, params)),
        "ai.bt_validate" => Outcome::Done(crate::ai_bt::validate(world, params)),
        "ai.bt_edit" => Outcome::Done(crate::ai_bt::edit(world, params)),
        "ai.bt_live" => Outcome::Done(crate::ai_bt::live(world, params)),
        // Perception, utility reasoners and GOAP agents (ADR-2565, book/agents/ai-perception.md).
        "ai.perception_get" => Outcome::Done(crate::ai_reasoners::perception_get(world, params)),
        "ai.team_knowledge" => Outcome::Done(crate::ai_reasoners::team_knowledge(world, params)),
        "ai.senses" => Outcome::Done(crate::ai_reasoners::senses(world, params)),
        "ai.attitude_set" => Outcome::Done(crate::ai_reasoners::attitude_set(world, params)),
        "ai.utility_list" => Outcome::Done(crate::ai_reasoners::utility_list(world, params)),
        "ai.utility_explain" => Outcome::Done(crate::ai_reasoners::utility_explain(world, params)),
        "ai.goap_plan" => Outcome::Done(crate::ai_reasoners::goap_plan(world, params)),
        // Squads, attack tokens, formations, the tick budget, custom query rows (ADR-2709, book/agents/ai-squads.md).
        "ai.squads" => Outcome::Done(crate::ai_squads::squads(world, params)),
        "ai.squad_set" => Outcome::Done(crate::ai_squads::squad_set(world, params)),
        "ai.tick_budget_get" => Outcome::Done(crate::ai_squads::tick_budget_get(world, params)),
        "ai.tick_budget_set" => Outcome::Done(crate::ai_squads::tick_budget_set(world, params)),
        "ai.query_impls" => Outcome::Done(crate::ai_squads::query_impls(world, params)),
        "ai.formation_get" => Outcome::Done(crate::ai_squads::formation_get(world, params)),
        "ai.formation_set" => Outcome::Done(crate::ai_squads::formation_set(world, params)),
        // State-tree files: validate and edit (ADR-3342, book/agents/state-tree-editor.md).
        "ai.statetree_validate" => Outcome::Done(crate::state_tree::statetree_validate(world, params)),
        "ai.statetree_edit" => Outcome::Done(crate::state_tree::statetree_edit(world, params)),
        // Navmesh tile cache and crowd steering (ADR-1484, book/agents/navigation-crowds.md).
        "nav.region_status" => Outcome::Done(crate::nav_world::region_status(world, params)),
        "nav.area_table" => Outcome::Done(crate::nav_lanes::area_table(world, params)),
        "nav.set_area_table" => Outcome::Done(crate::nav_lanes::set_area_table(world, params)),
        "nav.region_rebuild" => Outcome::Done(crate::nav_world::region_rebuild(world, params)),
        "nav.stream_status" => Outcome::Done(crate::nav_world::stream_status(world, params)),
        "nav.stream_begin" => Outcome::Done(crate::nav_world::stream_begin(world, params)),
        "nav.stream_cook_cell" => Outcome::Done(crate::nav_world::stream_cook_cell(world, params)),
        "nav.stream_load_cell" => Outcome::Done(crate::nav_world::stream_load_cell(world, params)),
        "nav.stream_unload_cell" => Outcome::Done(crate::nav_world::stream_unload_cell(world, params)),
        "nav.stream_find_path" => Outcome::Done(crate::nav_world::stream_find_path(world, params)),
        // The host's deterministic agents report (plan row S-03, ADR-8405, book/agents/det-grid.md).
        "nav.det_agents" => Outcome::Done(crate::nav_det_agents::det_agents(world, params)),
        "nav.area_status" => Outcome::Done(crate::nav_lanes::area_status(world, params)),
        "nav.set_area_cost" => Outcome::Done(crate::nav_lanes::set_area_cost(world, params)),
        "nav.add_area_volume" => Outcome::Done(crate::nav_lanes::add_area_volume(world, params)),
        "nav.remove_area_volume" => Outcome::Done(crate::nav_lanes::remove_area_volume(world, params)),
        "nav.crowd_set_filter" => Outcome::Done(crate::nav::crowd_set_filter(world, params)),
        "nav.lanes_set" => Outcome::Done(crate::nav_lanes::lanes_set(world, params)),
        "nav.lanes_status" => Outcome::Done(crate::nav_lanes::lanes_status(world, params)),
        "nav.lane_route" => Outcome::Done(crate::nav_lanes::lane_route(world, params)),
        "nav.lane_nearest" => Outcome::Done(crate::nav_lanes::lane_nearest(world, params)),
        "nav.lane_check" => Outcome::Done(crate::nav_lanes::lane_check(world, params)),
        "nav.lane_set_enabled" => Outcome::Done(crate::nav_lanes::lane_set_enabled(world, params)),
        "nav.lane_add_agent" => Outcome::Done(crate::nav_lanes::lane_add_agent(world, params)),
        "nav.lane_remove_agent" => Outcome::Done(crate::nav_lanes::lane_remove_agent(world, params)),
        "nav.lane_set_destination" => Outcome::Done(crate::nav_lanes::lane_set_destination(world, params)),
        "nav.lane_step" => Outcome::Done(crate::nav_lanes::lane_step(world, params)),
        "nav.lane_agents" => Outcome::Done(crate::nav_lanes::lane_agents(world, params)),
        "nav.tile_cache_status" => Outcome::Done(crate::nav::tile_cache_status(world, params)),
        "nav.tile_cache_add_obstacle" => Outcome::Done(crate::nav::tile_cache_add_obstacle(world, params)),
        "nav.tile_cache_remove_obstacle" => {
            Outcome::Done(crate::nav::tile_cache_remove_obstacle(world, params))
        }
        "nav.crowd_status" => Outcome::Done(crate::nav::crowd_status(world, params)),
        "nav.crowd_add_agent" => Outcome::Done(crate::nav::crowd_add_agent(world, params)),
        "nav.crowd_set_target" => Outcome::Done(crate::nav::crowd_set_target(world, params)),
        "nav.crowd_step" => Outcome::Done(crate::nav::crowd_step(world, params)),
        // Off-mesh links and moving obstacles (ADR-1821, book/agents/navigation-links.md).
        "nav.links" => Outcome::Done(crate::nav::links(world, params)),
        "nav.set_links" => Outcome::Done(crate::nav::set_links(world, params)),
        "nav.set_link_enabled" => Outcome::Done(crate::nav::set_link_enabled(world, params)),
        "nav.generate_links" => Outcome::Done(crate::nav::generate_links(world, params)),
        "nav.tile_cache_move_obstacle" => Outcome::Done(crate::nav::tile_cache_move_obstacle(world, params)),
        "nav.crowd_set_link_queue" => {
            Outcome::Done(crate::nav_authoring::crowd_set_link_queue(world, params))
        }
        "nav.crowd_link_queues" => Outcome::Done(crate::nav_authoring::crowd_link_queues(world, params)),
        "nav.lanes_from_splines" => Outcome::Done(crate::nav_authoring::lanes_from_splines(world, params)),
        "nav.cook_triangle_areas" => Outcome::Done(crate::nav_authoring::cook_triangle_areas(world, params)),
        "nav.set_link_files" => Outcome::Done(crate::nav_authoring::set_link_files(world, params)),
        "nav.authored_links_status" => {
            Outcome::Done(crate::nav_authoring::authored_links_status(world, params))
        }
        // Jump arcs, streamed links and grid growth (ADR-2589, book/agents/navigation-links.md).
        "nav.link_traversals" => Outcome::Done(crate::nav_links_stream::link_traversals(world, params)),
        "nav.crowd_set_link_arcs" => {
            Outcome::Done(crate::nav_links_stream::crowd_set_link_arcs(world, params))
        }
        "nav.tile_links" => Outcome::Done(crate::nav_links_stream::tile_links(world, params)),
        "nav.tile_cache_grow" => Outcome::Done(crate::nav_links_stream::tile_cache_grow(world, params)),
        "nav.raycast" => Outcome::Done(crate::nav_query::raycast(world, params)),
        "nav.random_point" => Outcome::Done(crate::nav_query::random_point(world, params)),
        "nav.crowd_set_stuck" => Outcome::Done(crate::nav_query::crowd_set_stuck(world, params)),
        // Entity audio (specs/16 § 3).
        "audio.list_emitters" => Outcome::Done(crate::audio_emitters::list_emitters(world)),
        "audio.set_emitter" => Outcome::Done(crate::audio_emitters::set_emitter(world, params)),
        "audio.list_zones" => Outcome::Done(crate::audio_zones::list_zones(world)),
        "audio.set_zone" => Outcome::Done(crate::audio_zones::set_zone(world, params)),
        "audio.probe_zones" => Outcome::Done(crate::audio_zones::probe_zones(world, params)),
        // 2D toolkit (specs/22 § T2.10, book/agents/2d.md).
        "sprite.list" => Outcome::Done(crate::sprite2d::list_sprites(world, params)),
        "sprite.get" => Outcome::Done(crate::sprite2d::get_sprite(world, params)),
        "tilemap.list" => Outcome::Done(crate::sprite2d::list_tilemaps(world, params)),
        "tilemap.get_tile" => Outcome::Done(crate::sprite2d::get_tile(world, params)),
        "view2d.get" => Outcome::Done(crate::sprite2d::get_view(world, params)),
        // Tilemap writes and auto-tiling (docs/specs/86-2d-toolkit.md § 5, ADR-1889).
        "tilemap.set_tile" => Outcome::Done(crate::tilemap_edit::set_tile(world, params)),
        "tilemap.autotile_register" => Outcome::Done(crate::tilemap_edit::autotile_register(world, params)),
        "tilemap.autotile_list" => Outcome::Done(crate::tilemap_edit::autotile_list(world, params)),
        "tilemap.paint" => Outcome::Done(crate::tilemap_edit::paint(world, params)),
        "tilemap.autotile" => Outcome::Done(crate::tilemap_edit::autotile(world, params)),
        "tilemap.stamp_copy" => Outcome::Done(crate::tilemap_edit::stamp_copy(world, params)),
        "tilemap.stamp_paste" => Outcome::Done(crate::tilemap_edit::stamp_paste(world, params)),
        // Import reports of the cook's Aseprite/LDtk importers (book/agents/import-report.md, ADR-3358).
        "import.report" => Outcome::Done(crate::import_report::import_report(world, params)),
        // Gizmo debug overlays (book/agents/gizmos.md).
        "debug.gizmos" => Outcome::Done(crate::gizmos::get(world)),
        "debug.set_gizmos" => Outcome::Done(crate::gizmos::set(world, params)),
        // Quick draw layer (book/agents/quick-draw.md).
        "quick.draw_list" => Outcome::Done(crate::quick::draw_list(world)),
        "quick.draw2d_stats" => Outcome::Done(crate::quick::draw2d_stats(world)),
        // Projected decals (specs/26 § 3).
        "vfx.spawn_decal" => Outcome::Done(crate::vfx::spawn_decal(world, params)),
        "vfx.place_rut" => Outcome::Done(crate::vfx::place_rut(world, params)),
        "vfx.decal_budget" => Outcome::Done(crate::vfx::decal_budget(world, params)),
        "vfx.damage_decal_atlas" => Outcome::Done(crate::vfx::damage_decal_atlas(world, params)),
        "vfx.decal_surface_at" => Outcome::Done(crate::vfx::decal_surface_at(world, params)),
        "vfx.get_look" => Outcome::Done(crate::vfx::get_look(world, params)),
        "vfx.set_look" => Outcome::Done(crate::vfx::set_look(world, params)),
        "vfx.medium" => Outcome::Done(crate::vfx::medium(world, params)),
        "vfx.hover_washes" => Outcome::Done(crate::vfx::hover_washes(world, params)),
        "vfx.set_hover_wash" => Outcome::Done(crate::vfx::set_hover_wash(world, params)),
        "vfx.hit" => Outcome::Done(crate::vfx::hit(world, params)),
        // Wheel smoke, dust, spray and marks (book/agents/vehicle-fx.md, ADR-3274).
        "vfx.vehicle_fx" => Outcome::Done(crate::vehicle_fx::read(world, params)),
        "vfx.vehicle_fx_set" => Outcome::Done(crate::vehicle_fx::set(world, params)),
        "vfx.weapons" => Outcome::Done(crate::vfx::weapons(world, params)),
        "vfx.plumes" => Outcome::Done(crate::vfx::plumes(world, params)),
        // Mesh debris and impact sites (book/agents/debris-impact.md).
        "vfx.debris_burst" => Outcome::Done(crate::debris::debris_burst(world, params)),
        "vfx.debris_state" => Outcome::Done(crate::debris::debris_state(world, params)),
        "vfx.debris_clock" => Outcome::Done(crate::debris::debris_clock(world, params)),
        "vfx.impact_site" => Outcome::Done(crate::debris::impact_site(world, params)),
        "vfx.set_plume" => Outcome::Done(crate::vfx::set_plume(world, params)),
        "vfx.stats" => Outcome::Done(crate::vfx::stats(world, params)),
        "vfx.systems" => Outcome::Done(crate::vfx_systems::systems(world, params)),
        "vfx.describe" => Outcome::Done(crate::vfx_systems::describe(world, params)),
        "vfx.spawn" => Outcome::Done(crate::vfx_systems::spawn(world, params)),
        "vfx.set_param" => Outcome::Done(crate::vfx_systems::set_param(world, params)),
        "vfx.spawn_probe" => Outcome::Done(crate::vfx_systems::spawn_probe(world, params)),
        "vfx.fire_lights" => Outcome::Done(crate::fire_shock::fire_lights(world, params)),
        "vfx.spawn_crash_fires" => Outcome::Done(crate::fire_shock::spawn_crash_fires(world, params)),
        "gameplay.explode" => Outcome::Done(crate::explosion::gameplay_explode(world, params)),
        "gameplay.explosion_stats" => {
            Outcome::Done(crate::explosion::gameplay_explosion_stats(world, params))
        }
        "camera.shock_shake" => Outcome::Done(crate::fire_shock::shock_shake(world, params)),
        "camera.schedule_shock_shake" => {
            Outcome::Done(crate::fire_shock::schedule_shock_shake(world, params))
        }
        // Per-camera shake sensors (book/agents/camera-shake.md, ADR-1619).
        "camera.shake" => Outcome::Done(crate::camera_shake::camera_shake(world, params)),
        "camera.shake_sensor" => Outcome::Done(crate::camera_shake::camera_shake_sensor(world, params)),
        "camera.set_shake_sensor" => {
            Outcome::Done(crate::camera_shake::camera_set_shake_sensor(world, params))
        }
        // The rail camera and the crane (book/agents/camera-rail-crane.md, ADR-3231).
        "camera.rail_state" => Outcome::Done(crate::camera_rail::camera_rail_state(world, params)),
        "camera.interior_state" => {
            Outcome::Done(crate::camera_interior::camera_interior_state(world, params))
        }
        "camera.crane_state" => Outcome::Done(crate::camera_rail::camera_crane_state(world, params)),
        "camera.set_rail" => Outcome::Done(crate::camera_rail::camera_set_rail(world, params)),
        "camera.set_crane" => Outcome::Done(crate::camera_rail::camera_set_crane(world, params)),
        // The first-person walk bob (book/agents/walk-bob.md, ADR-2486).
        "camera.walk_bob" => Outcome::Done(crate::walk_bob::camera_walk_bob(world, params)),
        "camera.set_walk_bob" => Outcome::Done(crate::walk_bob::camera_set_walk_bob(world, params)),
        // Showcase camera and look controls (ADR-2449).
        "camera.handheld" => Outcome::Done(crate::showcase_camera::camera_handheld(world, params)),
        "camera.photographic_walk" => {
            Outcome::Done(crate::showcase_camera::camera_photographic_walk(world, params))
        }
        "scene.season" => Outcome::Done(crate::scene::edit(
            world,
            method,
            params,
            crate::showcase_camera::scene_season,
        )),
        "render.foliage_shadow" => {
            Outcome::Done(crate::showcase_camera::render_foliage_shadow(world, params))
        }
        "render.capture_plan" => Outcome::Done(crate::render_out::capture::plan(params)),
        // The data-driven camera framework (book/agents/camera-framework.md, spec 35).
        "camera.assets" => Outcome::Done(crate::camera_framework::camera_assets(world, params)),
        "camera.validate" => Outcome::Done(crate::camera_framework::camera_validate(world, params)),
        "camera.inspect" => Outcome::Done(crate::camera_framework::camera_inspect(world, params)),
        "camera.set_var" => Outcome::Done(crate::camera_framework::camera_set_var(world, params)),
        "camera.set_param" => Outcome::Done(crate::camera_framework::camera_set_param(world, params)),
        "camera.set_input" => Outcome::Done(crate::camera_framework::camera_set_input(world, params)),
        "camera.reset_input" => Outcome::Done(crate::camera_framework::camera_reset_input(world, params)),
        "camera.push_overlay" => Outcome::Done(crate::camera_framework::camera_push_overlay(world, params)),
        "camera.add_additive" => Outcome::Done(crate::camera_framework::camera_add_additive(world, params)),
        "camera.release_layer" => Outcome::Done(crate::camera_framework::camera_release_layer(world, params)),
        "camera.hold_overlay" => Outcome::Done(crate::camera_framework::camera_hold_overlay(world, params)),
        "camera.set_driver" => Outcome::Done(crate::camera_framework::camera_set_driver(world, params)),
        "camera.load" => Outcome::Done(crate::camera_framework::camera_load(world, params)),
        // Crash effects: a scheduled crash scaled by yield, and its layers (book/agents/crash-fx.md).
        "vfx.crash" => Outcome::Done(crate::vfx_crash::crash(world, params)),
        "vfx.crashes" => Outcome::Done(crate::vfx_crash::crashes(world, params)),
        "vfx.crash_profile" => Outcome::Done(crate::vfx_crash::crash_profile(world, params)),
        "vfx.crash_fireball" => Outcome::Done(crate::vfx_crash_fireball::crash_fireball_view(world, params)),
        "vfx.blast_gust" => Outcome::Done(crate::vfx_crash::blast_gust(world, params)),
        "vfx.entry_state" => Outcome::Done(crate::vfx::entry_state(world, params)),
        // Per-instance glass: own-canopy view mode and damage (book/agents/glass.md).
        "render.glass_view" => Outcome::Done(crate::glass::get_view(world, params)),
        "render.set_glass_view" => Outcome::Done(crate::glass::set_view(world, params)),
        "render.canopy_glass" => Outcome::Done(crate::glass::get_canopy(world)),
        "render.set_canopy_glass" => Outcome::Done(crate::glass::set_canopy(params)),
        "render.glass_damage" => Outcome::Done(crate::glass::damage(world, params)),
        // Material mask recolor and decal slots (book/agents/mask-decals.md).
        "render.mask_decals" => Outcome::Done(crate::mask_decals::get(world, params)),
        "render.set_mask_decals" => Outcome::Done(crate::mask_decals::set(world, params)),
        // Hair card shading (book/agents/hair-cards.md).
        "render.hair_get" => Outcome::Done(crate::hair::get(world, params)),
        "render.hair_set" => Outcome::Done(crate::hair::set(world, params)),
        // Compute skinning, dual quaternions, eight influences (book/agents/skinning.md).
        "render.skinning" => Outcome::Done(crate::skinning::get(world)),
        "render.set_skinning" => Outcome::Done(crate::skinning::set(world, params)),
        // Render-world entry integrity and skinned bounds (book/agents/draw-integrity.md).
        "render.draw_integrity" => Outcome::Done(crate::draw_integrity::get(world)),
        "render.set_skinned_bounds" => {
            Outcome::Done(crate::draw_integrity::set_skinned_bounds(world, params))
        }
        // Bindless step-2 texture-set census (book/agents/texture-sets.md, ADR-356).
        "render.texture_sets" => Outcome::Done(crate::texture_sets::get(world)),
        "render.set_texture_set_census" => Outcome::Done(crate::texture_sets::set_census(world, params)),
        // Exposure and exposure zones (book/agents/exposure-zones.md).
        "render.get_exposure" => Outcome::Done(crate::exposure::get_exposure(world)),
        "render.exposure_log" => Outcome::Done(crate::exposure::exposure_log(world, params)),
        // The clock wind and other animated material offsets read (book/agents/material-clock.md).
        "render.material_clock" => Outcome::Done(crate::material_clock::get(world)),
        // Scene wind and the dynamic-caster shadow class (book/agents/foliage-wind.md, ADR-2028).
        "render.foliage_wind" => Outcome::Done(crate::foliage_wind::get(world)),
        "render.set_foliage_wind" => Outcome::Done(crate::foliage_wind::set_wind(world, params)),
        "render.set_dynamic_caster_shadows" => {
            Outcome::Done(crate::foliage_wind::set_dynamic_casters(world, params))
        }
        // Per-material foliage sway (book/agents/foliage-wind.md, ADR-2304).
        "material.sway" => Outcome::Done(crate::material_sway::get(world, params)),
        "material.set_sway" => Outcome::Done(crate::material_sway::set(world, params)),
        // Draw counters and skinned instancing (book/agents/skinned-instancing.md, ADR-1368).
        "render.stats" => Outcome::Done(crate::skinned_stats::get(world)),
        "render.set_skinned_instancing" => {
            Outcome::Done(crate::skinned_stats::set_skinned_instancing(world, params))
        }
        // Water bodies in the runtime frame (book/agents/water.md, ADR-1399).
        "water.state" => Outcome::Done(crate::water::state(world)),
        "water.probe" => Outcome::Done(crate::water::probe(world, params)),
        "water.set" => Outcome::Done(crate::water::set(world, params)),
        "water.surface" => Outcome::Done(crate::water::surface(world, params)),
        "water.raycast" => Outcome::Done(crate::water::raycast(world, params)),
        "water.floaters" => Outcome::Done(crate::water::floaters(world, params)),
        "water.ocean" => Outcome::Done(crate::water::ocean_report(world, params)),
        "water.ocean_check" => Outcome::Done(crate::water::ocean_check(world)),
        "water.set_mode" => Outcome::Done(crate::water::set_mode(world, params)),
        "water.ocean_cost" => Outcome::Done(crate::water::ocean_cost(world, params)),
        "water.set_caustics" => Outcome::Done(crate::water::set_caustics(world, params)),
        "water.caustics_stats" => Outcome::Done(crate::water::caustics_stats(world, params)),
        // Terrain ground variation and late-winter surfaces (book/agents/terrain-late-winter.md).
        "terrain.get_ground_look" => Outcome::Done(crate::ground_look::get(world)),
        "terrain.set_ground_look" => Outcome::Done(crate::ground_look::set(world, params)),
        "render.get_aerosol" => Outcome::Done(crate::aerosol::get(world)),
        "render.set_aerosol" => Outcome::Done(crate::aerosol::set(world, params)),
        // Daylight grey-card calibration (book/agents/daylight-calibration.md).
        "render.grey_card" => Outcome::Done(crate::daylight::grey_card(params)),
        // Terrain debug views on a running frame (book/agents/terrain-debug-views.md).
        "terrain.get_debug_view" => Outcome::Done(crate::terrain_debug::get(world)),
        "terrain.set_debug_view" => Outcome::Done(crate::terrain_debug::set(world, params)),
        // Post volumes and the physical camera (book/agents/post-volumes.md, ADR-1464).
        "render.post_volumes" => Outcome::Done(crate::post_volumes::post_volumes(world, params)),
        "render.set_post_volume" => Outcome::Done(crate::post_volumes::set_post_volume(world, params)),
        "render.get_physical_camera" => {
            Outcome::Done(crate::post_volumes::get_physical_camera(world, params))
        }
        "render.set_physical_camera" => {
            Outcome::Done(crate::post_volumes::set_physical_camera(world, params))
        }
        // Post-volume format v3 (book/agents/post-volumes.md, ADR-2323).
        "render.set_post_volume_luts" => {
            Outcome::Done(crate::post_volume_extras::set_post_volume_luts(world, params))
        }
        "render.post_volume_gizmos" => {
            Outcome::Done(crate::post_volume_extras::post_volume_gizmos(world, params))
        }
        // Runtime lens effects (book/agents/lens-effects.md, ADR-1863).
        "render.lens_effects" => Outcome::Done(crate::lens_effects::get(world, params)),
        "render.set_lens_effects" => Outcome::Done(crate::lens_effects::set(world, params)),
        // Montages, sync groups and motion warping (book/agents/montages.md).
        "anim.montage_state" => Outcome::Done(crate::anim_montage::montage_state(world, params)),
        "anim.montage_play" => Outcome::Done(crate::anim_montage::montage_play(world, params)),
        "anim.montage_stop" => Outcome::Done(crate::anim_montage::montage_stop(world, params)),
        "anim.montage_jump" => Outcome::Done(crate::anim_montage::montage_jump(world, params)),
        "anim.montage_set_warp_target" => {
            Outcome::Done(crate::anim_montage::montage_set_warp_target(world, params))
        }
        "anim.sync_state" => Outcome::Done(crate::anim_montage::sync_state(world, params)),
        "anim.warp_state" => Outcome::Done(crate::anim_warps::warp_state(world, params)),
        "anim.montage_describe" => Outcome::Done(crate::anim_montage_edit::montage_describe(world, params)),
        "anim.montage_edit" => Outcome::Done(crate::anim_montage_edit::montage_edit(world, params)),
        "anim.montage_slots" => Outcome::Done(crate::anim_montage_edit::montage_slots(world, params)),
        "anim.montage_slot_edit" => Outcome::Done(crate::anim_montage_edit::montage_slot_edit(world, params)),
        "anim.montage_control" => Outcome::Done(crate::anim_montage_edit::montage_control(world, params)),
        "anim.montage_commands" => Outcome::Done(crate::anim_montage::montage_commands(world, params)),
        "anim.scripted_warp" => Outcome::Done(crate::anim_warps::scripted_warp(world, params)),
        "anim.root_motion_route" => Outcome::Done(crate::anim_warps::root_motion_route(world, params)),
        // Data tables (book/agents/data-tables.md, ADR-1466).
        "tables.list" => Outcome::Done(crate::tables::list(world, params)),
        "tables.describe" => Outcome::Done(crate::tables::describe(world, params)),
        "tables.get" => Outcome::Done(crate::tables::get(world, params)),
        "tables.set" => Outcome::Done(crate::tables::set(world, params)),
        "tables.validate" => Outcome::Done(crate::tables::validate(world, params)),
        "tables.export" => Outcome::Done(crate::tables::export(world, params)),
        // Extensions (book/agents/plugins.md, specs/57). `plugin.reload` is the editor's when it
        // hosts a loader: its AgentExtension row is consulted above.
        "plugin.list" => Outcome::Done(crate::plugins::list(world, params)),
        "plugin.info" => Outcome::Done(crate::plugins::info(world, params)),
        "plugin.check" => Outcome::Done(crate::plugins::check(world, params)),
        "plugin.enable" => Outcome::Done(crate::plugins::enable(world, params)),
        "plugin.disable" => Outcome::Done(crate::plugins::disable(world, params)),
        "plugin.reload" => Outcome::Done(crate::plugins::reload(world, params)),
        // Answers from its own thread when the child ends, so a slow tool never holds a frame.
        "plugin.tool_run" => {
            let reply = crate::extension::DeferredReply::new(id, reply);
            match crate::plugins::tool_run(world, params, reply) {
                Ok(()) => Outcome::Deferred,
                Err(error) => Outcome::Done(Err(error)),
            }
        }

        // Localization (book/agents/localization.md, ADR-1466).
        "localization.status" => Outcome::Done(crate::localization::status(world, params)),
        "localization.set_locale" => Outcome::Done(crate::localization::set_locale(world, params)),
        "localization.resolve" => Outcome::Done(crate::localization::resolve(world, params)),
        "localization.set_pseudo" => Outcome::Done(crate::localization::set_pseudo(world, params)),
        "localization.validate" => Outcome::Done(crate::localization::validate(world, params)),
        "localization.plural" => Outcome::Done(crate::localization::plural_method(world, params)),
        "localization.set_key_display" => Outcome::Done(crate::localization::set_key_display(world, params)),
        "localization.coverage" => Outcome::Done(crate::localization::coverage(world, params)),
        "localization.glyph_coverage" => Outcome::Done(crate::localization::glyph_coverage(world, params)),
        "loc.status" => Outcome::Done(crate::loc_tools::status(world, params)),
        "loc.check_fit" => Outcome::Done(crate::loc_tools::check_fit(world, params)),
        "localization.set_audio_locale" => {
            Outcome::Done(crate::localization::set_audio_locale(world, params))
        }
        "render.graph" => Outcome::Done(crate::render_graph::get(world)),
        // Sequence players and the cinematic camera takeover (book/agents/sequence-player.md).
        "sequence.player" => Outcome::Done(crate::sequence_player::sequence_player(world, params)),
        "audio.medium" => Outcome::Done(crate::audio::medium(world, params)),
        "audio.propagation" => Outcome::Done(crate::audio::propagation(world, params)),
        // Procedural loop generator (book/agents/audio.md).
        "audio.loopgen_render" => Outcome::Done(crate::audio::loopgen_render(world, params)),
        // Visibility buffer and cooked meshlets (book/agents/visbuffer.md).
        "render.get_visbuffer" => Outcome::Done(crate::visbuffer::get(world)),
        // TAAU temporal-quality metrics on synthetic sequences (book/agents/temporal-quality.md).
        "render.temporal_quality" => Outcome::Done(crate::temporal_quality::temporal_quality(params)),
        // Probe specular: indirect specular from the probe light (book/agents/probe-specular.md).
        "render.get_probe_specular" => Outcome::Done(crate::probe_specular::get_probe_specular(world)),
        "render.set_probe_specular" => {
            Outcome::Done(crate::probe_specular::set_probe_specular(world, params))
        }
        "render.probe_specular_lobe" => {
            Outcome::Done(crate::probe_specular::probe_specular_lobe(world, params))
        }
        "render.set_visbuffer" => Outcome::Done(crate::visbuffer::set(world, params)),
        "render.vis_lod_hysteresis" => Outcome::Done(crate::visbuffer::lod_hysteresis(world, params)),
        // Lightmaps (book/agents/lightmaps.md, ADR-860).
        "render.get_lightmaps" => Outcome::Done(crate::lightmaps::get_lightmaps(world)),
        "render.set_lightmaps" => Outcome::Done(crate::lightmaps::set_lightmaps(world, params)),
        "render.lightmap_encode_report" => {
            Outcome::Done(crate::lightmaps::lightmap_encode_report(world, params))
        }
        // Mixer, master limiter, offline render check (book/agents/audio-mix.md).
        "audio.mix" => Outcome::Done(crate::audio_mix::mix(world, params)),
        "audio.bus_meters" => Outcome::Done(crate::audio_meters::bus_meters(world, params)),
        "audio.program_loudness" => {
            Outcome::Done(crate::audio_program_loudness::program_loudness(world, params))
        }
        "audio.set_bus" => Outcome::Done(crate::audio_mix::set_bus(world, params)),
        "audio.set_limiter" => Outcome::Done(crate::audio_mix::set_limiter(world, params)),
        "audio.render_check" => Outcome::Done(crate::audio_mix::render_check(world, params)),
        // Loudness at import (book/agents/loudness-import.md, ADR-1855).
        "audio.loudness" => Outcome::Done(crate::audio_loudness::loudness(world, params)),
        "audio.set_loudness_normalization" => {
            Outcome::Done(crate::audio_loudness::set_normalization(world, params))
        }
        // Voice capture and loopback (book/agents/voice.md, specs/16 § 3b).
        // Music on the audio clock and the listener (book/agents/audio-music.md, ADR-2517).
        "audio.music_state" => Outcome::Done(crate::audio_music::music_state(world)),
        "audio.music_transition" => Outcome::Done(crate::audio_music::music_transition(world, params)),
        "audio.music_stinger" => Outcome::Done(crate::audio_music::music_stinger(world, params)),
        "audio.listener_info" => Outcome::Done(crate::audio_music::listener_info(world)),
        // Parametric patches (book/agents/audio-patch.md, ADR-7226).
        "audio.patch_state" => Outcome::Done(crate::audio_patch::patch_state(world)),
        "audio.patch_trigger" => Outcome::Done(crate::audio_patch::patch_trigger(world, params)),
        // Convolution, time stretch and ambience files (book/agents/audio-convolution.md, ADR-2721).
        "audio.bus_effects" => Outcome::Done(crate::audio_convolution::bus_effects(world, params)),
        "audio.bus_effect_set" => Outcome::Done(crate::audio_convolution::bus_effect_set(world, params)),
        "audio.convolution_stats" => Outcome::Done(crate::audio_convolution::convolution_stats(world)),
        "audio.ambience_play" => Outcome::Done(crate::audio_convolution::ambience_play(world, params)),
        "audio.ambience_stop" => Outcome::Done(crate::audio_convolution::ambience_stop(world, params)),
        "audio.ambience_list" => Outcome::Done(crate::audio_convolution::ambience_list(world)),
        "audio.time_stretch_probe" => {
            Outcome::Done(crate::audio_convolution::time_stretch_probe(world, params))
        }
        "audio.voice" => Outcome::Done(crate::voice::voice(world, params)),
        "audio.capture_start" => Outcome::Done(crate::voice::capture_start(world, params)),
        "audio.capture_stop" => Outcome::Done(crate::voice::capture_stop(world, params)),
        "audio.set_talking" => Outcome::Done(crate::voice::set_talking(world, params)),
        "audio.voice_loopback_start" => Outcome::Done(crate::voice::voice_loopback_start(world, params)),
        "audio.voice_loopback_stop" => Outcome::Done(crate::voice::voice_loopback_stop(world, params)),
        "audio.voice_place" => Outcome::Done(crate::voice::voice_place(world, params)),
        // Voice chat: codec, channels, moderation, loss simulator (book/agents/voice-chat.md, specs/59).
        "voice_chat.status" => Outcome::Done(crate::voice_chat::status(world, params)),
        "voice_chat.set_channel" => Outcome::Done(crate::voice_chat::set_channel(world, params)),
        "voice_chat.mute" => Outcome::Done(crate::voice_chat::mute(world, params)),
        "voice_chat.loopback" => Outcome::Done(crate::voice_chat::loopback(world, params)),
        "voice_chat.stats" => Outcome::Done(crate::voice_chat::stats(world, params)),
        "voice_chat.test_source" => Outcome::Done(crate::voice_chat::test_source(world, params)),
        "voice_chat.simulate_network" => Outcome::Done(crate::voice_chat::simulate_network(world, params)),
        "voice_chat.mic" => Outcome::Done(crate::voice_chat::mic(world, params)),
        "voice_chat.settings" => Outcome::Done(crate::voice_chat::settings(world, params)),
        "voice_chat.talk" => Outcome::Done(crate::voice_chat::talk(world, params)),
        "voice_chat.relay" => Outcome::Done(crate::voice_chat::relay(world, params)),
        "voice_chat.processing" => Outcome::Done(crate::voice_chat::processing(world, params)),
        // Froxel fog local lights (book/agents/volumetric-fog.md, ADR-1365).
        "render.fog_lights" => Outcome::Done(crate::fog_lights::get(world)),
        // The froxel fog's sun-shadow lookup (book/agents/volumetric-fog.md, ADR-3122).
        "render.fog_shadow" => Outcome::Done(crate::fog_shadow::get(world, params)),
        // What the last frame drew on screen (book/agents/visibility.md, ADR-7200).
        "render.visible_entities" => Outcome::Done(crate::visible_entities::get(world, params)),
        "render.viscells" => Outcome::Done(crate::viscells::get(world, params)),
        "render.set_viscells" => Outcome::Done(crate::viscells::set(world, params)),
        // The host's keyed draws without entities (book/agents/draw-list.md, ADR-8087).
        "render.draw_list" => Outcome::Done(crate::draw_list::get(world, params)),
        // The aerial perspective's interior volumes (book/agents/aerial-interiors.md, ADR-4757).
        "render.aerial_interiors" => Outcome::Done(crate::aerial_interiors::get(world, params)),
        "render.set_aerial_interiors" => Outcome::Done(crate::aerial_interiors::set(world, params)),
        // The per-frame load budget (book/agents/instant-load.md, ADR-7884).
        "render.load_budget" => Outcome::Done(crate::load_budget::get(world, params)),
        "render.set_load_budget" => Outcome::Done(crate::load_budget::set(world, params)),

        // Gameplay: players, controllers, saves (specs/14 B3/B7, M20).
        "players.list" => Outcome::Done(crate::framework::players_list(world)),
        "players.get" => Outcome::Done(crate::framework::players_get(world, params)),
        "scene.possess" => Outcome::Done(crate::scene::edit(
            world,
            method,
            params,
            crate::framework::scene_possess,
        )),
        "save.save" => Outcome::Done(crate::framework::save_save(world, params)),
        "save.load" => Outcome::Done(crate::framework::save_load(world, params)),
        "save.list_slots" => Outcome::Done(crate::framework::save_list_slots(world)),
        "save.quick_save" => Outcome::Done(crate::save_flow::quick_save(world)),
        "save.quick_load" => Outcome::Done(crate::save_flow::quick_load(world)),
        "save.delete" => Outcome::Done(crate::save_flow::delete(world, params)),
        "save.status" => Outcome::Done(crate::save_flow::status(world)),
        "character.move_to" => Outcome::Done(crate::save_flow::character_move_to(world, params)),
        "save.inspect" => Outcome::Done(crate::save_refs::inspect(world, params)),

        // Frame loop, quit and coroutines (lane gameplay-coroutines).
        "loop.get" => Outcome::Done(crate::runloop::loop_get(world)),
        "loop.set" => Outcome::Done(crate::runloop::loop_set(world, params)),
        "quit.request" => Outcome::Done(crate::runloop::quit_request(world, params)),
        "coroutines.list" => Outcome::Done(crate::runloop::coroutines_list(world)),

        // Mobile lifecycle drill, thermal override and touch controls (ADR-527).
        "lifecycle.get" => Outcome::Done(crate::mobile::lifecycle_get(world)),
        "lifecycle.drill" => Outcome::Done(crate::mobile::lifecycle_drill(world, params)),
        "thermal.get" => Outcome::Done(crate::mobile::thermal_get(world)),
        "thermal.set_override" => Outcome::Done(crate::mobile::thermal_set_override(world, params)),
        "input.touch_controls" => Outcome::Done(crate::mobile::touch_controls(world)),
        // Frame pacing (N12, ADR-1942).
        "pacing.get" => Outcome::Done(crate::pacing::pacing_get(world)),
        "pacing.set" => Outcome::Done(crate::pacing::pacing_set(world, params)),
        "pacing.simulate" => Outcome::Done(crate::pacing::pacing_simulate(world, params)),
        "pacing.trace" => Outcome::Done(crate::pacing::pacing_trace(world, params)),
        // Schedule executor (ADR-4933).
        "executor.get" => Outcome::Done(crate::executor::executor_get(world)),
        "executor.set" => Outcome::Done(crate::executor::executor_set(world, params)),
        // OpenXR seam (ADR-1571).
        "xr.get" => Outcome::Done(crate::xr::xr_get(world)),
        "xr.drive" => Outcome::Done(crate::xr::xr_drive(world, params)),
        // Destruction level 2 (ADR-1597).
        "physics.destruction_define" => Outcome::Done(crate::destruction::destruction_define(world, params)),
        "physics.destruction_damage" => Outcome::Done(crate::destruction::destruction_damage(world, params)),
        "physics.destruction_state" => Outcome::Done(crate::destruction::destruction_state(world, params)),
        "physics.destruction_budget" => Outcome::Done(crate::destruction::destruction_budget(world, params)),
        "physics.destruction_net" => Outcome::Done(crate::destruction::destruction_net(world, params)),
        "physics.destruction_draw" => Outcome::Done(crate::destruction_draw::destruction_draw(world, params)),
        "physics.destruction_playback" => {
            Outcome::Done(crate::destruction::destruction_playback(world, params))
        }
        "physics.destruction_load_report" => {
            Outcome::Done(crate::destruction::destruction_load_report(world, params))
        }
        "physics.destruction_fx_table" => {
            Outcome::Done(crate::destruction::destruction_fx_table(world, params))
        }
        "physics.destruction_fx_stats" => {
            Outcome::Done(crate::destruction::destruction_fx_stats(world, params))
        }
        "physics.destruction_impact_damage" => {
            Outcome::Done(crate::destruction::destruction_impact_damage(world, params))
        }
        "physics.destruction_impact_damage_set" => {
            Outcome::Done(crate::destruction::destruction_impact_damage_set(world, params))
        }
        // OpenXR (spec 58, ADR-1676).
        "xr.set_option" => Outcome::Done(crate::xr::xr_set_option(world, params)),
        "xr.recenter" => Outcome::Done(crate::xr::xr_recenter(world)),
        "xr.capture_eye" => Outcome::Done(crate::xr::xr_capture_eye(world, params)),
        "xr.layers" => Outcome::Done(crate::xr::xr_layers(world)),
        "xr.set_quad" => Outcome::Done(crate::xr::xr_set_quad(world, params)),
        // XR walk mode (ADR-2457).
        "xr.walk_start" => Outcome::Done(crate::xr_walk::xr_walk_start(world, params)),
        "xr.walk_set" => Outcome::Done(crate::xr_walk::xr_walk_set(world, params)),

        // DO27 replay playback and Moments (book/agents/replay.md).
        "replay.load" => Outcome::Done(crate::replay::replay_load(world, params)),
        "replay.seek" => Outcome::Done(crate::replay::replay_seek(world, params)),
        "replay.step" => Outcome::Done(crate::replay::replay_step(world, params)),
        "replay.play" => Outcome::Done(crate::replay::replay_play(world, params)),
        "replay.pause" => Outcome::Done(crate::replay::replay_pause(world, params)),
        "moments.list" => Outcome::Done(crate::replay::moments_list(world, params)),
        "moments.get" => Outcome::Done(crate::replay::moments_get(world, params)),

        // Demo recording and the spectator camera (book/agents/demo.md, ADR-1569).
        "demo.inspect" => Outcome::Done(crate::demo::demo_inspect(world, params)),
        "demo.seek_plan" => Outcome::Done(crate::demo::demo_seek_plan(world, params)),
        "spectator.get" => Outcome::Done(crate::demo::spectator_get(world, params)),
        "spectator.set" => Outcome::Done(crate::demo::spectator_set(world, params)),
        "spectator.step" => Outcome::Done(crate::demo::spectator_step(world, params)),
        // The live demo driver and the scrub bar (regia_framework::demo_world, ADR-1878).
        "demo.status" => Outcome::Done(crate::demo::demo_status_method(world, params)),
        "demo.record_start" => Outcome::Done(crate::demo::demo_record_start(world, params)),
        "demo.record_stop" => Outcome::Done(crate::demo::demo_record_stop(world, params)),
        "demo.play" => Outcome::Done(crate::demo::demo_play(world, params)),
        "demo.pause" => Outcome::Done(crate::demo::demo_pause(world, params)),
        "demo.seek" => Outcome::Done(crate::demo::demo_seek(world, params)),
        "demo.scrub_bar" => Outcome::Done(crate::demo::demo_scrub_bar(world, params)),
        "spectator.bind" => Outcome::Done(crate::demo::spectator_bind(world, params)),

        // Leashed photo mode (book/agents/photo-mode.md, ADR-8415).
        "photo.enter" => Outcome::Done(crate::photo::photo_enter(world, params)),
        "photo.set" => Outcome::Done(crate::photo::photo_set(world, params)),
        "photo.capture" => Outcome::Done(crate::photo::photo_capture(world, params)),
        "photo.status" => Outcome::Done(crate::photo::photo_status(world, params)),

        // Game states (regia_ecs::state, book/agents/states.md).
        "state.list" => Outcome::Done(crate::states::state_list(world)),
        "state.get" => Outcome::Done(crate::states::state_get(world, params)),
        "state.set" => Outcome::Done(crate::states::state_set(world, params)),
        // Draw counters and the bindless tier (book/agents/renderer-excellence.md, ADR-316).
        "render.get_frame_stats" => Outcome::Done(crate::render_stats::get_frame_stats(world)),
        "render.get_capabilities" => Outcome::Done(crate::render_stats::get_capabilities(world)),
        "render.bindless_report" => Outcome::Done(crate::bindless_report::get(world)),

        // Temporal upscaler and quality mode (book/agents/upscalers.md, ADR-324).
        "render.get_upscalers" => Outcome::Done(crate::upscalers::get(world)),
        "render.set_upscaler" => Outcome::Done(crate::upscalers::set(world, params)),
        // Software reference upscaler check (book/agents/upscalers.md, ADR-510).
        "render.reference_upscale" => Outcome::Done(crate::upscalers::reference_upscale(params)),
        // Vendor-ready jitter sequence (book/agents/upscalers.md, ADR-1150).
        "render.upscaler_jitter" => Outcome::Done(crate::upscalers::upscaler_jitter(params)),
        // Vendor upscaler selection with the frame's fallback (book/agents/upscalers.md, ADR-2002).
        "render.set_vendor_upscaler" => Outcome::Done(crate::upscalers::set_vendor(world, params)),
        // FSR 3 frame generation preparation (book/agents/frame-gen.md, ADR-514).
        "render.frame_gen" => Outcome::Done(crate::frame_gen::get(world)),
        "render.set_frame_gen" => Outcome::Done(crate::frame_gen::set(world, params)),
        // GI denoise scores on the CPU reference (book/agents/gi-denoise.md, ADR-864).
        "seeing.gi_denoise_check" => Outcome::Done(crate::gi_denoise::gi_denoise_check(world, params)),
        // Spec 27 § 2.3 chart through a temporal upscaler (book/agents/temporal-chart.md, ADR-2638).
        "seeing.temporal_chart" => Outcome::Done(crate::temporal_chart::temporal_chart(world, params)),
        // Texture table growth and GPU residency feedback (book/agents/texture-residency.md, ADR-1110).
        "render.texture_residency" => Outcome::Done(crate::texture_residency::get(world, params)),

        // Determinism state hash (book/agents/determinism.md, ADR-537).
        "sim.state_hash" => Outcome::Done(crate::state_hash::sim_state_hash(world, params)),
        "sim.state_diff" => Outcome::Done(crate::state_hash::sim_state_diff(world, params)),
        // Lockstep desync rows of ECS games (book/agents/desync-hash-ecs.md, ADR-8080).
        "sim.desync_rows" => Outcome::Done(crate::desync_rows::sim_desync_rows(world, params)),

        // Gameplay tags (book/agents/gameplay-tags.md, ADR-1463).
        "tags.list" => Outcome::Done(crate::tags::list(world, params)),
        "tags.get" => Outcome::Done(crate::tags::get(world, params)),
        "tags.set" => Outcome::Done(crate::tags::set(world, params)),
        "tags.query" => Outcome::Done(crate::tags::query(world, params)),
        "tags.validate" => Outcome::Done(crate::tags::validate(world, params)),
        // UI screen stack (book/agents/ui-screens.md, ADR-1563 l).
        "ui.screens" => Outcome::Done(crate::ui_screens::screens(world, params)),
        // World streaming and HLOD data model (book/agents/world-streaming.md, ADR-1566).
        "scene.stream_plan" => Outcome::Done(crate::world_streaming::stream_plan(params)),
        "scene.hlod_select" => Outcome::Done(crate::world_streaming::hlod_select(params)),
        "scene.large_position" => Outcome::Done(crate::world_streaming::large_position(params)),
        // Baked HLOD proxy atlases of a cooked level (ADR-2483).
        "world.hlod_proxy" => Outcome::Done(crate::world_streaming::hlod_proxy(world, params)),
        "stream.open" => Outcome::Done(crate::stream::open(world, params)),
        "stream.status" => Outcome::Done(crate::stream::status(world, params)),
        "stream.cells" => Outcome::Done(crate::stream::cells(world, params)),
        "stream.sources" => Outcome::Done(crate::stream::sources(world, params)),
        "stream.pin" => Outcome::Done(crate::stream::pin(world, params)),
        "stream.unpin" => Outcome::Done(crate::stream::unpin(world, params)),
        "stream.teleport" => Outcome::Done(crate::stream::teleport(world, params)),
        "stream.ready" => Outcome::Done(crate::stream::ready(world, params)),
        "stream.budget_set" => Outcome::Done(crate::stream::budget_set(world, params)),
        "stream.timing_set" => Outcome::Done(crate::stream::timing_set(world, params)),
        "stream.retry" => Outcome::Done(crate::stream::retry(world, params)),
        "stream.hlod" => Outcome::Done(crate::stream::hlod(world, params)),
        "stream.origin" => Outcome::Done(crate::stream::origin(world, params)),
        "stream.rebase" => Outcome::Done(crate::stream::rebase(world, params)),
        "stream.trace" => Outcome::Done(crate::stream::trace(world, params)),
        "stream.overlay" => Outcome::Done(crate::stream::overlay(world, params)),
        "stream.bounds_set" => Outcome::Done(crate::stream::bounds_set(world, params)),
        "stream.collision_tiles" => Outcome::Done(crate::stream::collision_tiles(world, params)),
        // Localization pipeline report/pseudo preview and the subtitle queue (ADR-1570,
        // book/agents/ui-localization.md).
        "ui.localization" => Outcome::Done(crate::ui_localization::localization(world, params)),
        "ui.subtitles" => Outcome::Done(crate::ui_localization::subtitles(world, params)),
        // Culture formatting preview (ADR-2698, book/agents/ui-localization.md).
        "ui.format_preview" => Outcome::Done(crate::ui_format::format_preview(world, params)),
        // Conversations and the caption presenter (book/agents/conversation.md, ADR-1697).
        "conversation.play" => Outcome::Done(crate::conversation::play(world, params)),
        "conversation.say" => Outcome::Done(crate::conversation::say(world, params)),
        "conversation.choose" => Outcome::Done(crate::conversation::choose(world, params)),
        "conversation.stop" => Outcome::Done(crate::conversation::stop(world, params)),
        "conversation.validate" => Outcome::Done(crate::conversation::validate_method(world, params)),
        "captions.state" => Outcome::Done(crate::conversation::state(world, params)),
        "captions.set_options" => Outcome::Done(crate::conversation::set_options(world, params)),
        // Choice widgets, karaoke and anchored captions (book/agents/dialogue-ui.md, ADR-3234).
        "ui.choice_state" => Outcome::Done(crate::dialogue_ui::choice_state(world, params)),
        "ui.choice_pick" => Outcome::Done(crate::dialogue_ui::choice_pick(world, params)),
        "captions.anchor_state" => Outcome::Done(crate::dialogue_ui::anchor_state(world, params)),
        "captions.set_dialogue_style" => Outcome::Done(crate::dialogue_ui::set_dialogue_style(world, params)),
        // Dialogue lines (book/agents/dialogue-subtitles.md, ADR-1882).
        "dialogue.list" => Outcome::Done(crate::dialogue::list(world, params)),
        "dialogue.resolve" => Outcome::Done(crate::dialogue::resolve(world, params)),
        "dialogue.play" => Outcome::Done(crate::dialogue::play(world, params)),
        "dialogue.stop" => Outcome::Done(crate::dialogue::stop(world, params)),
        // Cue sets on the caption board (book/agents/dialogue-subtitles.md, ADR-1882).
        "subtitle_sets.status" => Outcome::Done(crate::subtitle_sets::status(world)),
        "subtitle_sets.queue" => Outcome::Done(crate::subtitle_sets::queue(world, params)),
        "subtitle_sets.stop" => Outcome::Done(crate::subtitle_sets::stop(world, params)),
        "subtitle_sets.set_enabled" => Outcome::Done(crate::subtitle_sets::set_enabled(world, params)),
        "subtitle_sets.sync" => Outcome::Done(crate::subtitle_sets::sync(world, params)),
        // Per-object shadow LOD hints (book/agents/shadow-lod-hint.md, ADR-1622).
        "render.shadow_lod_hints" => Outcome::Done(crate::shadow_lod_hint::get(world)),
        "render.set_shadow_lod_hint" => Outcome::Done(crate::shadow_lod_hint::set(world, params)),
        // The shader-permutation policy (book/agents/permutation-policy.md, ADR-2026).
        "render.permutations" => Outcome::Done(crate::render_permutations::get(world, params)),
        "render.set_permutation_policy" => Outcome::Done(crate::render_permutations::set(world, params)),
        // Retarget quality report and crowd animation (book/agents/retarget-crowds.md, ADR-1857).
        "anim.retarget_check" => Outcome::Done(crate::anim_crowd::retarget_check(world, params)),
        "anim.crowd_status" => Outcome::Done(crate::anim_crowd::crowd_status(world, params)),

        // Networking: endpoint status, peers, network conditioner (book/agents/net.md, ADR-1600).
        "net.status" => Outcome::Done(crate::net::status(world, params)),
        "net.peers" => Outcome::Done(crate::net::peers(world, params)),
        "net.crypto" => Outcome::Done(crate::net::crypto(world, params)),
        "net.lockstep" => Outcome::Done(crate::net::lockstep(world, params)),
        // Warm successors: the host journal and its shadows (book/agents/net-migration.md, ADR-8853).
        "net.migration" => Outcome::Done(crate::net::migration(world, params)),
        "net.desync" => Outcome::Done(crate::net_desync::desync(world, params)),
        "net.desync_inject" => Outcome::Done(crate::net_desync::desync_inject(world, params)),
        "net.replays" => Outcome::Done(crate::net_lsreplay::replays(world, params)),
        "net.replay_audit" => Outcome::Done(crate::net_lsreplay::replay_audit(world, params)),
        // Published external replication stats (book/agents/net-external-replication.md, ADR-8892).
        "net.external_stats" => Outcome::Done(crate::net_external_stats::external_stats(world, params)),
        // Connection quality grades and signals (book/agents/net-quality.md, ADR-8182).
        "net.quality" => Outcome::Done(crate::net::quality(world, params)),
        "net.conditioner_set" => Outcome::Done(crate::net::conditioner_set(world, params)),
        // Replication-game resume (book/agents/net-link.md, spec 120 § 10.3, ADR-8298).
        "net.resume" => Outcome::Done(crate::net::resume(world, params)),
        "net.rejections" => Outcome::Done(crate::net::rejections(world, params)),
        "net.rollback" => Outcome::Done(crate::net::rollback(world, params)),
        // Net model and replication (book/agents/net-model.md, net-replication.md, ADR-1643).
        "net.entity" => Outcome::Done(crate::net_model::entity(world, params)),
        "net.messages" => Outcome::Done(crate::net_model::messages(world, params)),
        "net.peer_flood" => Outcome::Done(crate::net_model::peer_flood(world, params)),
        "net.set_owner" => Outcome::Done(crate::net_model::set_owner(world, params)),
        "net.replicated" => Outcome::Done(crate::net_model::replicated_list(world, params)),
        "net.entity_state" => Outcome::Done(crate::net_model::entity_state(world, params)),
        "net.wake" => Outcome::Done(crate::net_model::wake(world, params)),
        "net.set_asleep" => Outcome::Done(crate::net_model::set_asleep(world, params)),
        "net.resend" => Outcome::Done(crate::net_model::resend(world, params)),
        "net.pending_refs" => Outcome::Done(crate::net_model::pending_refs(world, params)),
        "net.assets" => Outcome::Done(crate::net_model::assets(world, params)),
        // Net relevance, priority and budgets (book/agents/net-relevance.md, ADR-1662).
        "net.state_channel" => Outcome::Done(crate::net_state::state_channel(world, params)),
        // The entropy stage's counters (book/agents/net-entropy.md, spec 72 § 18, ADR-3315).
        "net.entropy_stats" => Outcome::Done(crate::net_entropy::entropy_stats(world, params)),
        "net.relevance" => Outcome::Done(crate::net_relevance::relevance(world, params)),
        "net.why_not_relevant" => Outcome::Done(crate::net_relevance::why_not_relevant(world, params)),
        "net.bandwidth" => Outcome::Done(crate::net_relevance::bandwidth(world, params)),
        "net.set_client_rate" => Outcome::Done(crate::net_relevance::set_client_rate(world, params)),
        "net.profiles" => Outcome::Done(crate::net_relevance::profiles(world, params)),
        "net.grant_relevance" => Outcome::Done(crate::net_relevance::grant_relevance(world, params)),
        // Net proxy interpolation (book/agents/net-interpolation.md, ADR-1662).
        "net.interpolation" => Outcome::Done(crate::net_interp::interpolation(world, params)),
        "net.proxy_timeline" => Outcome::Done(crate::net_interp::timeline(world, params)),
        "net.set_interp_delay" => Outcome::Done(crate::net_interp::set_delay(world, params)),
        "net.anim_state" => Outcome::Done(crate::net_interp::anim_state(world, params)),
        // Session lifecycle (book/agents/net-session.md, ADR-1663).
        "session.state" => Outcome::Done(crate::net_session::state(world, params)),
        "session.host" => Outcome::Done(crate::net_session::host(world, params)),
        "session.connect" => Outcome::Done(crate::net_session::connect(world, params)),
        "session.disconnect" => Outcome::Done(crate::net_session::disconnect(world, params)),
        "session.travel" => Outcome::Done(crate::net_session::travel(world, params)),
        "session.manifest" => Outcome::Done(crate::net_session::manifest(world, params)),
        "net.conditioner_get" => Outcome::Done(crate::net_session::conditioner_get(world, params)),
        "net.conditioner_apply" => Outcome::Done(crate::net_session::conditioner_apply(world, params)),
        "net.sim_clients" => Outcome::Done(crate::net_session::sim_clients(world, params)),
        "net.stats" => Outcome::Done(crate::net_session::stats(world, params)),
        "net.traffic" => Outcome::Done(crate::net_session::traffic(world, params)),
        // Dedicated-server host (book/agents/net-dedicated-server.md, ADR-1663).
        "server.status" => Outcome::Done(crate::net_server::status(world, params)),
        "server.kick" => Outcome::Done(crate::net_server::kick(world, params)),
        "server.ban" => Outcome::Done(crate::net_server::ban(world, params)),
        "server.unban" => Outcome::Done(crate::net_server::unban(world, params)),
        "server.bans" => Outcome::Done(crate::net_server::bans(world, params)),
        "server.change_level" => Outcome::Done(crate::net_server::change_level(world, params)),
        "server.reload_config" => Outcome::Done(crate::net_server::reload_config(world, params)),
        "server.shutdown" => Outcome::Done(crate::net_server::shutdown(world, params)),
        "server.rotation_get" => Outcome::Done(crate::net_server::rotation_get(world, params)),
        "server.rotation_skip" => Outcome::Done(crate::net_server::rotation_skip(world, params)),
        // A dedicated server's kept world (book/agents/net-server-persistence.md, ADR-3354).
        "server.save_now" => Outcome::Done(crate::net_server_persistence::save_now(world, params)),
        "server.persistence_status" => Outcome::Done(crate::net_server_persistence::status(world, params)),
        "server.restore" => Outcome::Done(crate::net_server_persistence::restore(world, params)),
        // A lockstep server's match rows (book/agents/net-lockstep-server.md, ADR-8558).
        "server.lockstep_status" => Outcome::Done(crate::net_server::lockstep_status(world, params)),
        // Net client prediction (book/agents/net-prediction.md, ADR-1793).
        "net.prediction" => Outcome::Done(crate::net_predict::prediction(world, params)),
        "net.input_buffer" => Outcome::Done(crate::net_predict::input_buffer(world, params)),
        "net.prediction_history" => Outcome::Done(crate::net_predict::history(world, params)),
        "net.force_correction" => Outcome::Done(crate::net_predict::force(world, params)),
        // Net lag compensation (book/agents/net-lag-compensation.md, ADR-1752).
        "lagcomp.stats" => Outcome::Done(crate::net_lagcomp::stats(world, params)),
        "lagcomp.history" => Outcome::Done(crate::net_lagcomp::history(world, params)),
        "lagcomp.raycast" => Outcome::Done(crate::net_lagcomp::raycast(world, params)),
        "lagcomp.last_queries" => Outcome::Done(crate::net_lagcomp::last_queries(world, params)),
        "lagcomp.echoes" => Outcome::Done(crate::net_lagcomp::echoes(world, params)),
        "lagcomp.rewound_shapes" => Outcome::Done(crate::net_lagcomp::rewound_shapes(world, params)),
        // Lobbies and matchmaking (book/agents/net-lobbies.md, ADR-1877).
        "lobby.status" => Outcome::Done(crate::net_lobby::status(world, params)),
        "lobby.list" => Outcome::Done(crate::net_lobby::list(world, params)),
        "lobby.create" => Outcome::Done(crate::net_lobby::create(world, params)),
        "lobby.join" => Outcome::Done(crate::net_lobby::join(world, params)),
        "lobby.leave" => Outcome::Done(crate::net_lobby::leave(world, params)),
        "lobby.set_data" => Outcome::Done(crate::net_lobby::set_data(world, params)),
        "lobby.ready" => Outcome::Done(crate::net_lobby::ready(world, params)),
        "lobby.chat" => Outcome::Done(crate::net_lobby::chat(world, params)),
        "lobby.launch" => Outcome::Done(crate::net_lobby::launch(world, params)),
        "matchmaking.start" => Outcome::Done(crate::net_lobby::matchmaking_start(world, params)),
        "matchmaking.cancel" => Outcome::Done(crate::net_lobby::matchmaking_cancel(world, params)),
        "matchmaking.status" => Outcome::Done(crate::net_lobby::matchmaking_status(world, params)),
        // Networked characters (book/agents/net-character.md, ADR-1905).
        "net.characters" => Outcome::Done(crate::net_character::characters(world, params)),
        "net.vehicles" => Outcome::Done(crate::net_vehicle::vehicles(world, params)),
        // Networked hover hulls (book/agents/net-hover.md, ADR-8819).
        "net.hovers" => Outcome::Done(crate::net_hover::hovers(world, params)),
        // The forward shader's studio axis (book/agents/forward-studio-axis.md, ADR-2308).
        "render.forward_studio" => Outcome::Done(crate::render_forward_studio::get(world)),
        "render.set_forward_studio" => Outcome::Done(crate::render_forward_studio::set(world, params)),
        // The frame profiler (book/agents/profiler.md, ADR-2323).
        "profiler.set_capture" => Outcome::Done(crate::profiler::set_capture(world, params)),
        "profiler.frame" => Outcome::Done(crate::profiler::frame(world, params)),
        "profiler.capture" => Outcome::Done(crate::profiler::capture(world, params)),
        "profiler.compare" => Outcome::Done(crate::profiler::compare(world, params)),
        "profiler.timeline" => Outcome::Done(crate::profiler::timeline(world, params)),
        // IES light profiles (book/agents/light-profiles.md, ADR-2592).
        "render.set_light_profile" => Outcome::Done(crate::light_profiles::set_light_profile(world, params)),
        // The particle liquid (book/agents/fluid.md, ADR-2441).
        "fluid.create" => Outcome::Done(crate::fluid::create(world, params)),
        "fluid.add_emitter" => Outcome::Done(crate::fluid::add_emitter(world, params)),
        "fluid.step" => Outcome::Done(crate::fluid::step(world, params)),
        "fluid.stats" => Outcome::Done(crate::fluid::stats(world, params)),
        "fluid.bake" => Outcome::Done(crate::fluid::bake(world, params)),
        "fluid.play" => Outcome::Done(crate::fluid::play(world, params)),
        "fluid.destroy" => Outcome::Done(crate::fluid::destroy(world, params)),
        // The trust-on-first-use store of server keys (book/agents/net-trust.md, ADR-2416).
        "net.known_hosts" => Outcome::Done(crate::net_trust::known_hosts(world, params)),
        "net.trust" => Outcome::Done(crate::net_trust::trust(world, params)),
        "net.forget" => Outcome::Done(crate::net_trust::forget(world, params)),
        // Session players: ping/loss, the stats overlay, hand-offs (book/agents/net-session-players.md, ADR-2413).
        "net.player_stats" => Outcome::Done(crate::net_session::player_stats(world, params)),
        "net.stats_overlay" => Outcome::Done(crate::net_session::stats_overlay(world, params)),
        "net.handoffs" => Outcome::Done(crate::net_session::handoffs(world, params)),
        // Connectionless server bots (book/agents/net-session-players.md, ADR-3346).
        "net.bots" => Outcome::Done(crate::net_bots::bots(world, params)),
        // The terrain editing remainder (book/agents/level-terrain.md, ADR-2411).
        "terrain.level_terrain" => Outcome::Done(crate::terrain_level::level_terrain(world, params)),
        "terrain.set_level_terrain" => Outcome::Done(crate::terrain_level::set_level_terrain(world, params)),
        "terrain.load_level_terrain" => {
            Outcome::Done(crate::terrain_level::load_level_terrain(world, params))
        }
        "terrain.grass_density_stroke" => {
            Outcome::Done(crate::terrain_level::grass_density_stroke(world, params))
        }
        "terrain.undo_grass_density" => Outcome::Done(crate::terrain_level::undo_grass_density(world)),
        "terrain.redo_grass_density" => Outcome::Done(crate::terrain_level::redo_grass_density(world)),
        "terrain.grass_density" => Outcome::Done(crate::terrain_level::grass_density(world, params)),
        "terrain.save_grass_density" => {
            Outcome::Done(crate::terrain_level::save_grass_density(world, params))
        }
        "terrain.load_grass_density" => {
            Outcome::Done(crate::terrain_level::load_grass_density(world, params))
        }
        "terrain.scatter_rules" => Outcome::Done(crate::terrain_level::scatter_rules(world, params)),
        "terrain.set_scatter_rule" => Outcome::Done(crate::terrain_level::set_scatter_rule(world, params)),
        "procgen.load_report" => Outcome::Done(crate::procgen::load_report(world, params)),
        // The `mods://` file system (book/agents/mods.md, ADR-2418).
        "mods.list" => Outcome::Done(crate::mods::list(world, params)),
        "mods.mount" => Outcome::Done(crate::mods::mount(world, params)),
        "mods.unmount" => Outcome::Done(crate::mods::unmount(world, params)),
        "mods.order" => Outcome::Done(crate::mods::order(world, params)),
        "mods.hash" => Outcome::Done(crate::mods::hash(world, params)),
        "mods.read" => Outcome::Done(crate::mods::read(world, params)),
        // Entity kinds and the project map (book/agents/entity-kinds.md, ADR-2488).
        "kinds.list" => Outcome::Done(crate::kinds::list(world, params)),
        "kinds.of" => Outcome::Done(crate::kinds::of(world, params)),
        "kinds.lint" => Outcome::Done(crate::kinds::lint(world, params)),
        "project.describe" => Outcome::Done(crate::kinds::describe(world, params)),
        // Sequencer v3 (book/agents/sequence-player.md, ADR-2405).
        "sequencer.spatial_voices" => Outcome::Done(crate::sequence_player::spatial_voices(world, params)),
        "sequencer.sub_loops" => Outcome::Done(crate::sequence_player::sub_loops(world, params)),
        // Sequence subtitles (book/agents/sequence-player.md, ADR-3334).
        "sequencer.subtitles" => Outcome::Done(crate::sequence_player::subtitles(world, params)),
        "sequencer.subtitle_settings" => {
            Outcome::Done(crate::sequence_player::subtitle_settings(world, params))
        }
        "sequencer.rename_binding" => {
            Outcome::Done(crate::sequence_player::rename_binding_method(world, params))
        }
        // Product viewer contracts (book/agents/configurator-contracts.md, ADR-2480).
        "configurator.contract_check" => Outcome::Done(crate::configurator::contract_check(world, params)),
        "configurator.contract_schema" => Outcome::Done(crate::configurator::contract_schema(world, params)),
        // Product configurator runtime (book/agents/configurator.md, ADR-2534).
        "configurator.open_product" => {
            Outcome::Done(crate::configurator_runtime::open_product(world, params))
        }
        "configurator.options" => Outcome::Done(crate::configurator_runtime::options(world, params)),
        "configurator.set_pick" => Outcome::Done(crate::configurator_runtime::set_pick(world, params)),
        "configurator.build" => Outcome::Done(crate::configurator_runtime::build(world, params)),
        "configurator.resolve" => Outcome::Done(crate::configurator_runtime::resolve(world, params)),
        // Product viewer drag view and packages (book/agents/viewer-view.md, viewer-pack.md, ADR-2540).
        "viewer.view_simulate" => Outcome::Done(crate::viewer_view_pack::view_simulate(world, params)),
        "viewer.pack_verify" => Outcome::Done(crate::viewer_view_pack::pack_verify(world, params)),
        // The product viewer's renderer (book/agents/viewer-renderer.md, ADR-2539).
        "viewer.capabilities" => Outcome::Done(crate::viewer::capabilities(world, params)),
        "viewer.render_still" => Outcome::Done(crate::viewer::render_still(world, params)),
        "viewer.env_info" => Outcome::Done(crate::viewer::env_info(world, params)),
        // The product viewer's cheap looks (book/agents/product-looks.md, ADR-2538).
        "looks.list" => Outcome::Done(crate::looks::list(world, params)),
        "looks.material" => Outcome::Done(crate::looks::material(world, params)),
        "looks.cut" => Outcome::Done(crate::looks::cut(world, params)),
        "looks.render_board" => Outcome::Done(crate::looks::render_board(world, params)),
        // Ambience that matches the picture (book/agents/audio-ambience.md, ADR-2442).
        "audio.ambience" => Outcome::Done(crate::audio_ambience::get(world)),
        "audio.set_ambience" => Outcome::Done(crate::audio_ambience::set(world, params)),
        "audio.ambient_spots" => Outcome::Done(crate::audio_ambience::list_spots(world)),
        "audio.set_ambient_spot" => Outcome::Done(crate::audio_ambience::set_spot(world, params)),
        "audio.probe_ambience" => Outcome::Done(crate::audio_ambience::probe(world, params)),
        "audio.footsteps" => Outcome::Done(crate::audio_ambience::footsteps(world)),
        "audio.set_footsteps" => Outcome::Done(crate::audio_ambience::set_footsteps(world, params)),
        "audio.footstep" => Outcome::Done(crate::audio_ambience::footstep(world, params)),
        // Stochastic many-light sampling (book/agents/many-light-sampling.md, ADR-2470).
        "render.set_many_light_sampling" => Outcome::Done(crate::many_light::set(world, params)),
        "render.many_light_stats" => Outcome::Done(crate::many_light::stats(world, params)),
        // The stochastic SSR cost probe and bound (book/agents/ssr-cost.md, ADR-2557).
        "render.ssr_stats" => Outcome::Done(crate::ssr_cost::ssr_stats(world)),
        "render.set_ssr_cost" => Outcome::Done(crate::ssr_cost::set_ssr_cost(params)),
        // Moving solids and rigid-body coupling (ADR-2577).
        "fluid.set_solids" => Outcome::Done(crate::fluid::set_solids(world, params)),
        "fluid.solids" => Outcome::Done(crate::fluid::solids(world, params)),
        // Device-independent frame work (book/agents/frame-work.md, ADR-2711).
        "render.frame_work" => Outcome::Done(crate::frame_work::frame_work(world)),
        // Cells of a speed grid that are still awake (book/agents/fluid.md, ADR-3949).
        "fluid.sleep_mask" => Outcome::Done(crate::fluid_sleep::sleep_mask(world, params)),

        // The net_relevance gizmo group (book/agents/net-relevance.md, ADR-2665).
        "net.relevance_gizmos" => Outcome::Done(crate::net_relevance::relevance_gizmos(world, params)),
        // The shared cook cache (book/agents/cook-cache.md, ADR-2612).
        "cook.cache_stats" => Outcome::Done(crate::cook_cache::cache_stats(world, params)),
        // Overlay asset roots (book/agents/cook-overlay-roots.md, ADR-8155).
        "cook.overlay_roots" => Outcome::Done(crate::cook_overlay::overlay_roots(world, params)),
        // Player accessibility assists (book/agents/input-assists.md, audio-mono.md, color-vision.md, ADR-2646).
        "input.assists" => Outcome::Done(crate::input_assists::assists(world, params)),
        "audio.mono" => Outcome::Done(crate::audio_mono::mono(world, params)),
        "input.haptics" => Outcome::Done(crate::input_haptics::haptics(world, params)),
        "render.color_vision" => Outcome::Done(crate::render_color_vision::color_vision(world, params)),
        // Split baked GI volumes: the brick-loop probe (book/agents/baked-gi-split.md, ADR-2618).
        "render.set_baked_gi_brick_probe" => {
            Outcome::Done(crate::baked_gi_volumes::set_brick_probe(world, params))
        }
        // Signed content manifests (book/agents/content-trust.md, ADR-2710).
        "content.update" => Outcome::Done(crate::content::update(world, params)),
        // Networked rigid bodies (book/agents/net-bodies.md, ADR-2902).
        "net.bodies" => Outcome::Done(crate::net_body::bodies(world, params)),

        // Trailers: couple, uncouple, articulation readout, .trailer.ron (book/agents/trailers.md, ADR-3281).
        "physics.trailers" => Outcome::Done(crate::trailers::trailers(world, params)),
        "physics.trailer_couple" => Outcome::Done(crate::trailers::trailer_couple(world, params)),
        "physics.trailer_uncouple" => Outcome::Done(crate::trailers::trailer_uncouple(world, params)),
        "physics.trailer_config_check" => Outcome::Done(crate::trailers::trailer_config_check(world, params)),
        // Contact-scale calibration opt-ins (book/agents/contact-calibration.md, ADR-3202).
        "render.get_contact_calibration" => Outcome::Done(crate::contact_calibration::get(world)),
        "render.set_contact_calibration" => Outcome::Done(crate::contact_calibration::set(world, params)),
        // Fixed-wing aircraft (book/agents/aircraft.md, ADR-3374).
        "physics.aircraft" => Outcome::Done(crate::aircraft::aircraft(world, params)),
        "physics.aircraft_input" => Outcome::Done(crate::aircraft::aircraft_input(world, params)),
        "physics.aircraft_trim" => Outcome::Done(crate::aircraft::aircraft_trim(world, params)),
        "physics.aircraft_config_check" => {
            Outcome::Done(crate::aircraft::aircraft_config_check(world, params))
        }
        // AI drivers on lane graphs (book/agents/vehicle-ai.md, ADR-3117).
        "vehicle_ai.drive_to" => Outcome::Done(crate::vehicle_ai::drive_to(world, params)),
        "vehicle_ai.status" => Outcome::Done(crate::vehicle_ai::status(world, params)),
        "vehicle_ai.set_params" => Outcome::Done(crate::vehicle_ai::set_params(world, params)),
        "vehicle_ai.stop" => Outcome::Done(crate::vehicle_ai::stop(world, params)),
        // Active-ragdoll push recovery (book/agents/ragdoll-active.md, ADR-2963).
        "ragdoll.push" => Outcome::Done(crate::ragdoll_active::push(world, params)),
        // Local reflection captures as level content (book/agents/reflection-captures.md, ADR-2860).
        "render.save_local_captures" => {
            Outcome::Done(crate::reflection_captures::save_captures(world, params))
        }
        "render.load_local_captures" => {
            Outcome::Done(crate::reflection_captures::load_captures(world, params))
        }
        "render.get_capture_bake" => Outcome::Done(crate::reflection_captures::get_capture_bake(world)),
        // ReSTIR GI over hardware ray queries (book/agents/restir-gi.md, ADR-3349).
        "render.restir_gi" => Outcome::Done(crate::restir_gi::report(world)),
        "render.set_restir_gi" => Outcome::Done(crate::restir_gi::set(world, params)),
        // Procedural evaluation digest (book/agents/procgen.md, ADR-4593).
        "procgen.fingerprint" => Outcome::Done(crate::procgen_fingerprint::fingerprint(world, params)),

        // Secondary rigid debris spawn cap (book/agents/destruction-debris.md, ADR-3929).
        "physics.debris_budget" => Outcome::Done(crate::destruction_debris::debris_budget(world, params)),

        // Vehicle audio (book/agents/vehicle-audio.md, ADR-3271).
        "audio.vehicle_audio" => Outcome::Done(crate::vehicle_audio::state(world)),
        "audio.vehicle_audio_set" => Outcome::Done(crate::vehicle_audio::set(world, params)),
        "audio.vehicle_audio_probe" => Outcome::Done(crate::vehicle_audio::probe(params)),
        // Photosensitivity check (book/agents/seeing.md, ADR-3804).
        "seeing.flash_check" => crate::seeing_flash::flash_check(world, params, id, reply),

        // Two-wheel balance, separate from the car solver (book/agents/bike.md, ADR-4017).
        "physics.bike_step" => Outcome::Done(crate::bike::bike_step(world, params)),
        "physics.bike_state" => Outcome::Done(crate::bike::bike_state(world, params)),

        // Host battery and AC supply (book/agents/platform-power.md, ADR-3997).
        "platform.power_state" => Outcome::Done(crate::platform_power::power_state(world, params)),

        // Primary window and virtual file system (book/agents/window-and-vfs.md, ADR-7232).
        "window.get" => Outcome::Done(crate::window_vfs::window_get(world)),
        "window.set" => Outcome::Done(crate::window_vfs::window_set(world, params)),
        "vfs.mounts" => Outcome::Done(crate::window_vfs::mounts(world)),
        "vfs.exists" => Outcome::Done(crate::window_vfs::exists(world, params)),
        "vfs.list" => Outcome::Done(crate::window_vfs::list(world, params)),
        "vfs.read" => Outcome::Done(crate::window_vfs::read(world, params)),
        "vfs.mount_pak" => Outcome::Done(crate::window_vfs::mount_pak(world, params)),

        // QUIC datagram counters (book/agents/net-quic.md, ADR-3970).
        "net.quic_status" => Outcome::Done(crate::net_quic::status(params)),
        // Relay allocations, caps and refusals (book/agents/net-relay.md, ADR-8183).
        "net.relay_status" => Outcome::Done(crate::net_relay::relay_status(world, params)),
        // Port mapping, port-forward self-check and capacity (book/agents/net-host-check.md, ADR-8370).
        "net.host_check" => Outcome::Done(crate::net_host_check::host_check(world, params)),
        // The self-hosted lobby hub (book/agents/net-lobby-hub.md, ADR-8559).
        "lobby.hub_status" => Outcome::Done(crate::net_lobby_hub::hub_status(world, params)),
        // Join brokering and player identity on the hub (book/agents/net-lobby-hub-join.md, ADR-8607).
        "lobby.hub_join_status" => Outcome::Done(crate::net_lobby_hub::hub_join_status(world, params)),
        // WAV cue points stored in the file (book/agents/audio.md, ADR-4632).
        "audio.cues" => Outcome::Done(crate::audio_cues::cues(world, params)),
        // Marker sheet CSV (book/agents/sequence-player.md, ADR-4597). `.seq.ron` version unchanged.
        "sequencer.marker_sheet" => Outcome::Done(crate::marker_sheet::marker_sheet(world, params)),
        // The direct BRDF's energy model (book/agents/brdf-energy.md, ADR-2632).
        "render.brdf_energy" => Outcome::Done(crate::render_brdf_energy::get(world)),
        "render.set_brdf_energy" => Outcome::Done(crate::render_brdf_energy::set(world, params)),
        // The frame's shading normal and roughness AOVs (book/agents/frame-aov.md, ADR-2951).
        "seeing.frame_aov" => Outcome::Done(crate::frame_aov::frame_aov(world, params)),

        // Contact sounds from physics (book/agents/physics-audio.md, ADR-3270).
        "audio.impacts" => Outcome::Done(crate::physics_audio::impacts(world)),
        "audio.impact_probe" => Outcome::Done(crate::physics_audio::impact_probe(world, params)),
        "audio.impacts_set" => Outcome::Done(crate::physics_audio::impacts_set(world, params)),
        // Sight acquire and lose cones (book/agents/perception-hysteresis.md, ADR-4586).
        "ai.perception_hysteresis" => {
            Outcome::Done(crate::perception_hysteresis::perception_hysteresis(world, params))
        }
        // Take recorder (book/agents/sequence-take-recorder.md, ADR-2637).
        "sequence.record_start" => Outcome::Done(crate::sequence_player::record_start(world, params)),
        "sequence.record_stop" => Outcome::Done(crate::sequence_player::record_stop(world, params)),
        "sequence.record_state" => Outcome::Done(crate::sequence_player::record_state(world, params)),
        // Video sections (book/agents/sequence-player.md, ADR-2862).
        "sequencer.video_sections" => Outcome::Done(crate::sequence_player::video_sections(world, params)),
        // The cutscene flow (book/agents/cutscene.md, ADR-2948).
        "cutscene.start" => Outcome::Done(crate::cutscene::start(world, params)),
        "cutscene.skip" => Outcome::Done(crate::cutscene::skip(world, params)),
        "cutscene.stop" => Outcome::Done(crate::cutscene::stop(world, params)),
        "cutscene.state" => Outcome::Done(crate::cutscene::state(world, params)),
        // The network recorder and replay player (book/agents/net-replay.md, ADR-3021).
        "net.replay_record_start" => Outcome::Done(crate::net_replay::record_start(world, params)),
        "net.replay_record_stop" => Outcome::Done(crate::net_replay::record_stop(world, params)),
        "net.replay_ring_export" => Outcome::Done(crate::net_replay::ring_export(world, params)),
        "net.replay_open" => Outcome::Done(crate::net_replay::open(world, params)),
        "net.replay_seek" => Outcome::Done(crate::net_replay::seek(world, params)),
        "net.replay_play" => Outcome::Done(crate::net_replay::play(world, params)),
        "net.replay_close" => Outcome::Done(crate::net_replay::close(world, params)),
        "net.replay_state" => Outcome::Done(crate::net_replay::state(world, params)),
        // Client capture on a NetLink (book/agents/net-capture.md, ADR-8890).
        "net.capture_start" => Outcome::Done(crate::net_capture::capture_start(world, params)),
        "net.capture_save" => Outcome::Done(crate::net_capture::capture_save(world, params)),
        "camera_body.get" => Outcome::Done(crate::camera_body::get(world, params)),
        "camera_body.set" => Outcome::Done(crate::camera_body::set(world, params)),
        "camera_body.wake" => Outcome::Done(crate::camera_body::wake(world, params)),
        "camera_body.proximity" => Outcome::Done(crate::camera_body::proximity_call(world, params)),
        "camera_body.clearance" => Outcome::Done(crate::camera_body::clearance(world, params)),
        "camera_body.lift_path" => Outcome::Done(crate::camera_body::lift_path(world, params)),
        "bird.spawn" => Outcome::Done(crate::bird::spawn(world, params)),
        "bird.list" => Outcome::Done(crate::bird::list(world, params)),
        "bird.remove" => Outcome::Done(crate::bird::remove(world, params)),
        "bird.trigger" => Outcome::Done(crate::bird::trigger(world, params)),
        "bird.place_on_path" => Outcome::Done(crate::bird::place(world, params)),
        // The sun's specular as a disc (book/agents/sun-specular-disc.md, ADR-3190).
        "render.sun_specular_disc" => Outcome::Done(crate::render_sun_specular_disc::get(world)),
        "render.set_sun_specular_disc" => Outcome::Done(crate::render_sun_specular_disc::set(world, params)),

        // Forest flythrough (book/agents/forest-flythrough.md, ADR-1503).
        "terrain.forest_query" => Outcome::Done(crate::forest::query(world, params)),
        "terrain.forest_corridor" => Outcome::Done(crate::forest::corridor(world, params)),
        "terrain.forest_carve" => Outcome::Done(crate::forest::carve(world, params)),
        "terrain.forest_flight" => Outcome::Done(crate::forest::get(world, params)),
        "terrain.set_forest_flight" => Outcome::Done(crate::forest::set(world, params)),
        // Quarter-resolution screen-space GI gather (book/agents/dynamic-gi.md, ADR-3964).
        "render.get_ssgi_gather" => Outcome::Done(crate::dynamic_gi::get_ssgi_gather(world)),
        "render.set_ssgi_gather" => Outcome::Done(crate::dynamic_gi::set_ssgi_gather(world, params)),
        "render.area_light_fallback" => Outcome::Done(crate::light_profiles::area_light_fallback(world)),
        "render.set_area_light_fallback" => {
            Outcome::Done(crate::light_profiles::set_area_light_fallback(world, params))
        }
        "render.sized_light_ltc" => Outcome::Done(crate::light_profiles::sized_light_ltc(world)),
        "render.set_sized_light_ltc" => {
            Outcome::Done(crate::light_profiles::set_sized_light_ltc(world, params))
        }
        // Loaded cells, the one-cell margin and last-snapshot drops (book/agents/stream-cell-relevance.md, ADR-4011).
        "stream.cell_relevance" => Outcome::Done(crate::stream_cell_relevance::cell_relevance(world, params)),
        // Skid-steer tracks (book/agents/vehicle-track.md, ADR-4629).
        "physics.track_step" => Outcome::Done(crate::vehicle_track::track_step(world, params)),
        "physics.track_state" => Outcome::Done(crate::vehicle_track::track_state(world, params)),
        // Lockstep ground vehicles (book/agents/det-ground-vehicles.md, ADR-8551).
        "detsim.ground_vehicle_simulate" => Outcome::Done(crate::det_ground_vehicle::simulate(world, params)),
        // Scene-wide VFX significance (book/agents/vfx.md, ADR-3275).
        "vfx.significance" => Outcome::Done(crate::vfx_systems::significance(world, params)),
        "vfx.significance_set" => Outcome::Done(crate::vfx_systems::significance_set(world, params)),
        // The button-prompt glyph of an action on a device family (book/agents/input.md, ADR-7070).
        "input.glyph" => Outcome::Done(crate::input_glyph::glyph(world, params)),
        "input.keyboard_layout" => Outcome::Done(crate::keyboard_layout::preset(params)),
        // Procedural mesh recipes (book/agents/gen-mesh.md, ADR-7240).
        "gen.mesh" => Outcome::Done(crate::gen_mesh::gen_mesh(params)),
        // Marching-squares contours (book/agents/gen-mesh.md, ADR-8044).
        "gen.contour" => Outcome::Done(crate::gen_contour::gen_contour(params)),
        // Procedural textures and heightfields (book/agents/gen-textures.md, ADR-7280).
        "gen.texture" => Outcome::Done(crate::gen_texture::gen_texture(params)),
        "gen.heightfield" => Outcome::Done(crate::gen_texture::gen_heightfield(params)),
        // Assets the editor pushes into a launched game (book/agents/asset-push.md, ADR-7836).
        "assets.push" => Outcome::Done(crate::assets_push::push(world, params)),
        // The paused-scene hold (ADR-7890).
        "render.scene_hold" => Outcome::Done(crate::scene_hold::get(world)),
        "render.set_scene_hold" => Outcome::Done(crate::scene_hold::set(world, params)),
        // What the gameplay pause holds (book/agents/game-pause-freeze.md, ADR-10105).
        "play.freeze_state" => Outcome::Done(crate::pause_freeze::freeze_state(world)),
        // Frame caps per app state and the dynamic-resolution hold (book/agents/frame-pacing.md, ADR-7889).
        "app.pacing" => Outcome::Done(crate::app_pacing::app_pacing(world)),
        "app.set_pacing_state" => Outcome::Done(crate::app_pacing::app_set_pacing_state(world, params)),
        "render.dynres" => Outcome::Done(crate::app_pacing::render_dynres(world)),
        // UDP socket-buffer read-back and DSCP EF (book/agents/net-socket-buffers.md, ADR-7998).
        "net.socket_buffers" => Outcome::Done(crate::net_socket_buffers::socket_buffers(params)),
        // Scatter gameplay colliders (book/agents/terrain-scatter-colliders.md, ADR-8029).
        "terrain.scatter_colliders" => {
            Outcome::Done(crate::terrain_scatter_colliders::scatter_colliders(world, params))
        }
        // UI pointer claims (book/agents/ui-input.md, ADR-7976).
        "input.pointer_claims" => Outcome::Done(crate::input_pointer_claims::pointer_claims(world, params)),
        // Grounded placement and install-tier caps (book/agents/content-smalls.md, ADR-8036).
        "content.check_placement" => Outcome::Done(crate::content_smalls::check_placement(params)),
        "content.install_tier" => Outcome::Done(crate::content_smalls::install_tier(params)),
        // Cooked world geometry (book/agents/terrain-mesh-layers.md, ADR-8386).
        "terrain.surface_at" => Outcome::Done(crate::terrain_mesh_layers::surface_at(world, params)),
        "terrain.mesh_layers" => Outcome::Done(crate::terrain_mesh_layers::mesh_layers(world, params)),
        "terrain.bake_report" => Outcome::Done(crate::terrain_mesh_layers::bake_report(world, params)),
        "terrain.blockers" => Outcome::Done(crate::terrain_mesh_layers::blockers(world, params)),
        "terrain.inspect_source" => Outcome::Done(crate::terrain_source::inspect(world, params)),
        "terrain.bake_overview" => Outcome::Done(crate::terrain_source::bake(world, params)),
        // Presentation physics counters (book/agents/presentation-physics.md, ADR-8356).
        "sim_bridge.presentation_stats" => {
            Outcome::Done(crate::presentation_physics::presentation_stats(world))
        }
        // Plain-node hinges, spins and slews (book/agents/node-drivers.md, ADR-8410).
        "anim.node_drivers" => Outcome::Done(crate::node_drivers::node_drivers(world, params)),
        // Vertex-cluster far LOD (book/agents/cluster-lod.md, ADR-8464).
        "render.cluster_lod" => Outcome::Done(crate::cluster_lod::cluster_lod(world, params)),
        // A hover hull file, checked on the host (book/agents/det-hover.md, ADR-8512).
        "detsim.hover_hull_check" => Outcome::Done(crate::det_hover::hover_hull_check(world, params)),

        // In-world AI debug layers (book/agents/ai.md, ADR-8463).
        "ai.debug_layers" => Outcome::Done(crate::debug_layers::debug_layers(world, params)),
        "ai.debug_layers_set" => Outcome::Done(crate::debug_layers::debug_layers_set(world, params)),
        // The game pause key and its screen (book/agents/game-pause.md, ADR-10104).
        "game.pause" => Outcome::Done(crate::game_pause::pause(world)),
        "game.resume" => Outcome::Done(crate::game_pause::resume(world)),
        "game.pause_state" => Outcome::Done(crate::game_pause::pause_state(world)),
        other => Outcome::Done(Err(unknown_method(other))),
    }
}

/// One call through the per-request path of [`process_requests`] (envelope, permission, parameter gate, rate limits,
/// handler, provenance) without an audit record or a reply: the `agent.params.*` fuzz targets (ADR-2084). `None` when
/// the handler answers from a later frame.
pub(crate) fn call_in_process(
    world: &mut World,
    method: &str,
    params: Value,
) -> Option<Result<Value, RpcError>> {
    let request = Request {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params,
    };
    let (reply, _replies) = crossbeam_channel::unbounded();
    match handle(world, &request, json!(1), reply, None, None) {
        Outcome::Done(result) => Some(result),
        Outcome::Deferred => None,
    }
}

/// What [`call_local`] answers.
#[derive(Debug)]
pub enum LocalReply {
    /// The handler answered in this call.
    Done(Result<Value, RpcError>),
    /// The handler answers from a later frame, on this receiver.
    Pending(Receiver<Response>),
}

/// The client a master-token [`call_local`] names in its audit line and provenance record, for
/// the length of the call (crate-private: dispatch inserts and removes it).
#[derive(Resource, Debug, Clone)]
struct LocalClient(String);

/// One call made inside the process (the editor's console line, ADR-7754): the same route a
/// client request takes (envelope, permission, parameter gate, rate limits, handler, provenance)
/// and its audit line. A deferred handler answers on the [`LocalReply::Pending`] receiver.
///
/// The call keeps the caller it runs under: inside a scoped tool's call (an extension handler
/// such as `editor.console_run`) it is that tool's, so the scope's method list and permissions
/// apply and the caller stays in place for the outer call's audit line (ADR-1523). Outside any
/// scoped call it is a master-token call; `client` then names who made it in the audit line and
/// the provenance record (`editor:console` for a line the person typed), and `None` keeps the
/// agent session's name (a call an agent asked for).
pub fn call_local(world: &mut World, method: &str, params: Value, client: Option<&str>) -> LocalReply {
    let request = Request {
        jsonrpc: "2.0".into(),
        id: Some(json!(0)),
        method: method.into(),
        params,
    };
    let started = Instant::now();
    let caller = crate::scope::current_caller(world);
    // Only a master-token call takes `client`; a call nested in a named one keeps that name.
    let renamed = client.filter(|_| caller.is_none());
    let previous = renamed.and_then(|_| world.remove_resource::<LocalClient>());
    if let Some(client) = renamed {
        world.insert_resource(LocalClient(client.to_string()));
    }
    let (reply, replies) = crossbeam_channel::unbounded();
    let outcome = handle(world, &request, json!(0), reply, caller.as_deref(), None);
    if world.contains_resource::<Time>() {
        audit(world, &request, &outcome, started);
    }
    if renamed.is_some() {
        world.remove_resource::<LocalClient>();
        if let Some(previous) = previous {
            world.insert_resource(previous);
        }
    }
    match outcome {
        Outcome::Done(result) => LocalReply::Done(result),
        Outcome::Deferred => LocalReply::Pending(replies),
    }
}

/// The stable unknown-method error; the schema table is the source of truth for the list. The
/// hint leads with the closest known name when one is near (a typo), and the echoed name is cut
/// to `MAX_ECHOED_METHOD_BYTES`.
fn unknown_method(method: &str) -> RpcError {
    let shown = truncate_on_char(method, MAX_ECHOED_METHOD_BYTES);
    let names = schema::method_names();
    let list = names.join(", ");
    let close = closest_method(method, &names);
    let hint = match close {
        Some(close) => format!("did you mean {close}? protocol {PROTOCOL_VERSION} methods: {list}"),
        None => format!("protocol {PROTOCOL_VERSION} methods: {list}"),
    };
    let mut error = RpcError::new(
        protocol::METHOD_NOT_FOUND,
        "F0008",
        format!("unknown method {shown}"),
        hint,
    );
    if let (Some(close), Some(data)) = (close, error.data.as_mut().and_then(Value::as_object_mut)) {
        data.insert("suggestion".into(), Value::String(close.to_string()));
    }
    error
}

/// The longest prefix of `text` of at most `max` bytes that ends on a character boundary.
fn truncate_on_char(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The known method nearest to `method` by edit distance, when it is close enough to be a typo:
/// at most a third of the name's length, and at most 4 edits. Names longer than
/// `MAX_ECHOED_METHOD_BYTES` are never compared (no quadratic work on a hostile name).
fn closest_method(method: &str, names: &[&'static str]) -> Option<&'static str> {
    if method.is_empty() || method.len() > MAX_ECHOED_METHOD_BYTES {
        return None;
    }
    let budget = (method.chars().count() / 3).clamp(1, 4);
    names
        .iter()
        .map(|name| (edit_distance(method, name), *name))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, name)| (*distance, *name))
        .map(|(_, name)| name)
}

/// Levenshtein distance over characters (Wagner–Fischer, one row of memory).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if ca == *cb {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

/// Envelope checks every request passes before routing: `jsonrpc` is `"2.0"` and the id, when
/// present, is a string, a number or null (JSON-RPC 2.0 § 4). Both answer `F0004`.
fn envelope_error(request: &Request) -> Option<RpcError> {
    if request.jsonrpc != "2.0" {
        return Some(RpcError::new(
            protocol::INVALID_REQUEST,
            "F0004",
            format!(
                "`jsonrpc` must be \"2.0\", not {:?}",
                truncate_on_char(&request.jsonrpc, MAX_ECHOED_METHOD_BYTES)
            ),
            "send {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":...,\"params\":{...}}",
        ));
    }
    if request
        .id
        .as_ref()
        .is_some_and(|id| !matches!(id, Value::Null | Value::String(_) | Value::Number(_)))
    {
        return Some(RpcError::new(
            protocol::INVALID_REQUEST,
            "F0004",
            "the request id must be a string, a number or null",
            "use a counter or a short string as the id",
        ));
    }
    None
}

/// Longest `agent.hello` `client` or `model` kept on the session (it is copied into every audit
/// line and change record, so a 1 MiB name made every later line 1 MiB; audit-agent-sec Low).
const MAX_SESSION_NAME_BYTES: usize = 256;

/// `agent.hello`'s client/model parameters become this session's identity (audit + provenance),
/// each cut to `MAX_SESSION_NAME_BYTES` on a character boundary.
fn note_session(world: &mut World, params: &Value) -> Result<(), RpcError> {
    // Read both before the session changes: a wrong-typed `client` or `model` is refused (ADR-2622).
    let client = crate::params::opt_str(params, "client")?;
    let model = crate::params::opt_str(params, "model")?;
    let mut session = world
        .get_resource_or_insert_with::<AgentSession>(Default::default)
        .clone();
    if let Some(client) = client {
        session.client = truncate_on_char(client, MAX_SESSION_NAME_BYTES).to_string();
    }
    if let Some(model) = model {
        session.model = truncate_on_char(model, MAX_SESSION_NAME_BYTES).to_string();
    }
    world.insert_resource(session);
    Ok(())
}

fn hello(world: &World) -> Value {
    let time = world.resource::<Time>();
    let mut capabilities = vec!["reflection", "events", "permissions", "audit"];
    if world.get_resource::<regia_asset::AssetServer>().is_some() {
        capabilities.push("assets");
    }
    if world.get_resource::<regia_render::ScreenshotRequests>().is_some() {
        capabilities.push("render");
    }
    if world.get_resource::<regia_input::InputInjection>().is_some() {
        capabilities.push("input_injection");
    }
    if world
        .get_resource::<regia_framework::sim_bridge::SimCounters>()
        .is_some()
    {
        capabilities.push("sim_bridge");
    }
    if world.get_resource::<regia_physics::PhysicsWorld>().is_some() {
        capabilities.push("physics");
    }
    // Named markers (ADR-1559): compiled-in features, the audio pair, and the host's.
    capabilities.extend(
        crate::capabilities::markers(world)
            .into_iter()
            .map(|(name, _)| name),
    );
    // Extension methods (the editor surface) join the advertised list (specs/23 § 2).
    let registered = crate::plugins::method_names(world);
    let mut methods: Vec<&str> = protocol::methods();
    methods.extend(
        world
            .get_resource::<crate::extension::AgentExtension>()
            .map(|ext| ext.method_names())
            .unwrap_or_default(),
    );
    methods.extend(registered.iter().map(String::as_str));
    json!({
        "protocol": PROTOCOL_VERSION,
        "engine": env!("CARGO_PKG_VERSION"),
        "app": regia_platform::paths::exe_stem(),
        "pid": std::process::id(),
        "os": std::env::consts::OS,
        "methods": methods,
        "capabilities": capabilities,
        "shots_ron_version": crate::capabilities::SHOTS_RON_VERSION,
        "frame": time.frame_count(),
    })
}

/// One audit entry per call (specs/23 § 5): method, params hash, outcome, duration, client.
fn audit(world: &mut World, request: &Request, outcome: &Outcome, started: Instant) {
    let (call_outcome, error_code) = match outcome {
        Outcome::Done(Ok(_)) => ("ok", None),
        Outcome::Done(Err(error)) => {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str);
            ("error", code)
        }
        Outcome::Deferred => ("deferred", None),
    };
    record_audit(world, request, call_outcome, error_code, started);
}

/// Writes one audit line with the given outcome (`ok`, `error`, `deferred`, `expired`).
fn record_audit(
    world: &mut World,
    request: &Request,
    call_outcome: &str,
    error_code: Option<&str>,
    started: Instant,
) {
    let client = match crate::scope::current_caller(world) {
        Some(scope) => scope.client_name(),
        None => match world.get_resource::<LocalClient>() {
            Some(local) => local.0.clone(),
            None => world
                .get_resource::<AgentSession>()
                .map(|session| session.client.clone())
                .unwrap_or_default(),
        },
    };
    let entry = AuditEntry {
        frame: world.resource::<Time>().frame_count(),
        unix_ms: unix_ms_now(),
        // The method is bounded only by the frame cap; an unknown 30 MiB name must not become a
        // 30 MiB audit line (audit-agent-sec Low). Real method names are far shorter.
        method: truncate_on_char(&request.method, MAX_ECHOED_METHOD_BYTES),
        params_hash: params_hash(&request.params),
        outcome: call_outcome,
        error_code,
        duration_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        client: &client,
    };
    if let Some(mut log) = world.get_resource_mut::<AuditLog>() {
        log.record(&entry);
    }
    if let Some(observer) = world.get_resource::<crate::safety::AuditObserver>().copied() {
        (observer.recorded)(world, &entry, &request.params);
    }
}

/// Appends one provenance record for a successful authored change (`agent.changes`).
fn record_change(world: &mut World, method: &str, params: &Value, result: &Value) {
    let mut session = world.get_resource::<AgentSession>().cloned().unwrap_or_default();
    // A tool's edits are stamped with the tool, not the agent session (ADR-1523).
    if let Some(scope) = crate::scope::current_caller(world) {
        session.client = scope.client_name();
        session.model = String::new();
    } else if let Some(local) = world.get_resource::<LocalClient>() {
        // A line the person typed (ADR-7754): theirs, not the connected agent's.
        session.client = local.0.clone();
        session.model = String::new();
    }
    let frame = world.resource::<Time>().frame_count();
    world
        .get_resource_or_insert_with::<ChangeLog>(Default::default)
        .push(
            frame,
            method,
            change_target(method, params),
            result.clone(),
            &session,
        );
}

/// A short human-readable target for the provenance record.
fn change_target(method: &str, params: &Value) -> String {
    let entity = || params.get("entity").and_then(Value::as_u64).unwrap_or(0);
    let component = || params.get("component").and_then(Value::as_str).unwrap_or("");
    match method {
        "scene.set_property" => format!(
            "entity {} {}{}",
            entity(),
            component(),
            params
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(|path| format!(".{path}"))
                .unwrap_or_default()
        ),
        "scene.add_component" | "scene.remove_component" => {
            format!("entity {} {}", entity(), component())
        }
        "scene.spawn" => format!(
            "spawn {}",
            params.get("name").and_then(Value::as_str).unwrap_or("unnamed")
        ),
        "scene.spawn_primitive" => format!(
            "spawn {} {}",
            params.get("shape").and_then(Value::as_str).unwrap_or("?"),
            params.get("name").and_then(Value::as_str).unwrap_or("unnamed")
        ),
        "scene.despawn" => format!("entity {}", entity()),
        "scene.duplicate" => format!("entity {}", entity()),
        "scene.reparent" => format!(
            "entity {} parent {}",
            entity(),
            params.get("parent").and_then(Value::as_u64).unwrap_or(0)
        ),
        "scene.apply_edits" => format!(
            "transaction of {} edits",
            params
                .get("edits")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        ),
        "settings.set" => format!(
            "resource GameSettings.{}",
            params.get("path").and_then(Value::as_str).unwrap_or("")
        ),
        "state.set" => format!(
            "state {} -> {}",
            params.get("state").and_then(Value::as_str).unwrap_or(""),
            params.get("value").map(Value::to_string).unwrap_or_default()
        ),
        other => other.to_string(),
    }
}

fn read_logs(params: &Value) -> Result<Value, RpcError> {
    use regia_core::log::ring::{LogQuery, recent};
    let min_level = match crate::params::opt_str(params, "level")? {
        None => None,
        Some(l) => Some(
            l.parse::<tracing::Level>()
                .map_err(|_| RpcError::invalid_params(format!("unknown level {l}")))?,
        ),
    };
    let query = LogQuery {
        since: crate::params::opt_u64(params, "since")?,
        min_level,
        target: crate::params::opt_str(params, "target")?.map(String::from),
        contains: crate::params::opt_str(params, "contains")?.map(String::from),
        limit: None,
    };
    let limit =
        crate::params::opt_u64(params, "limit")?.map_or(200, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let (records, next) = recent(&query);
    let (records, next) = page_logs(records, next, query.since, limit);
    Ok(json!({ "records": records, "next": next }))
}

/// Applies `limit` to matching log records (oldest first). Without a `since` cursor the newest
/// `limit` are returned (a tail); with one, the oldest `limit` after it are returned and `next`
/// resumes right after the last one returned, so a small `limit` never skips records.
fn page_logs<T: LogSeq>(mut records: Vec<T>, next: u64, since: Option<u64>, limit: usize) -> (Vec<T>, u64) {
    if records.len() <= limit {
        return (records, next);
    }
    if let Some(since) = since {
        if limit == 0 {
            // Nothing returned: the cursor stays where it was, so the next poll sees the records.
            return (Vec::new(), since);
        }
        records.truncate(limit);
        let next = records.last().map_or(next, |r| r.seq() + 1);
        (records, next)
    } else {
        records.drain(..records.len() - limit);
        (records, next)
    }
}

/// A record with a sequence number (lets `page_logs` be tested without the global log ring).
trait LogSeq {
    fn seq(&self) -> u64;
}

impl LogSeq for regia_core::log::ring::LogRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{AgentConfig, AgentPermissions};

    impl LogSeq for u64 {
        fn seq(&self) -> u64 {
            *self
        }
    }

    #[test]
    fn logs_read_limit_returns_the_oldest_unread_page() {
        let all: Vec<u64> = (10..15).collect();
        let mut got = Vec::new();
        let mut since = 10;
        loop {
            let remaining: Vec<u64> = all.iter().copied().filter(|s| *s >= since).collect();
            if remaining.is_empty() {
                break;
            }
            let (page, next) = page_logs(remaining, 15, Some(since), 2);
            got.extend(page);
            since = next;
        }
        assert_eq!(got, all, "no gaps, no duplicates, in order");
        // Without a cursor the newest records are returned.
        let (tail, next) = page_logs(all.clone(), 15, None, 2);
        assert_eq!(tail, vec![13, 14]);
        assert_eq!(next, 15);
    }

    #[test]
    fn logs_read_limit_zero_keeps_the_cursor() {
        let (page, next) = page_logs(vec![5_u64, 6, 7], 8, Some(5), 0);
        assert!(page.is_empty());
        assert_eq!(
            next, 5,
            "an empty page must not move the cursor past unread records"
        );
    }

    /// A minimal app with the dispatch resources and one named entity.
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
        app.world_mut().spawn(Name::new("Rock"));
        app
    }

    /// Drives one request through `process_requests` and returns the response.
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
        // Satisfy queued predicates with one deterministic poll, without sleeping.
        if method == "play.wait_until" {
            crate::playtest::advance_waiters(app.world_mut());
        }
        drop(inbox_tx);
        rx.recv_timeout(Duration::from_secs(2)).expect("answered")
    }

    #[test]
    fn ui_live_inspection_dispatch_validates_and_returns_the_live_patch() {
        let mut app = app();
        app.insert_resource(regia_ui::text::TextSystem::embedded_only());
        app.add_plugins(regia_ui::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let node = app
            .world_mut()
            .spawn((regia_ui::UiNode, regia_ui::NodeStyle::default()))
            .id();
        let events = call(&mut app, "ui.inspect_events", json!({"node":node.to_bits()}))
            .result
            .expect("events route");
        assert!(events["events"].is_array());
        // One real UI event, recorded by the UI plugin's recorder, comes back through the route.
        assert!(events["events"].as_array().expect("events").is_empty());
        app.world_mut()
            .write_message(regia_ui::UiEvent::new(node, regia_ui::UiEventKind::Click));
        app.update();
        let events = call(&mut app, "ui.inspect_events", json!({"node":node.to_bits()}))
            .result
            .expect("events route");
        let recorded = events["events"].as_array().expect("events");
        assert_eq!(recorded.len(), 1, "{events}");
        assert_eq!(recorded[0]["kind"], "click");
        assert_eq!(recorded[0]["node"], node.to_bits());
        let missing = call(&mut app, "ui.inspect_events", json!({}))
            .error
            .expect("missing node");
        assert_eq!(missing.data.as_ref().expect("data")["code"], "ui_node_required");
        crate::contract::assert_listed("ui.inspect_events", "ui_node_required");
        let edit = call(
            &mut app,
            "ui.set_live_style",
            json!({"node":node.to_bits(),"field":"style.flex_grow","value":2.0}),
        )
        .result
        .expect("live route");
        assert_eq!(edit["live_edits"]["style.flex_grow"], 2.0);
        assert_eq!(
            app.world()
                .get::<regia_ui::NodeStyle>(node)
                .expect("style")
                .0
                .flex_grow,
            2.0
        );
        let rejected = call(
            &mut app,
            "ui.set_live_style",
            json!({"node":node.to_bits(),"field":"style.flex_grow","value":"invalid"}),
        )
        .error
        .expect("invalid live value");
        assert_eq!(
            rejected.data.as_ref().expect("error data")["code"],
            "ui_live_value"
        );
        crate::contract::assert_listed("ui.set_live_style", "ui_live_value");
        assert_eq!(
            app.world()
                .get::<regia_ui::NodeStyle>(node)
                .expect("style")
                .0
                .flex_grow,
            2.0
        );
    }

    /// ADR-8083 through the dispatcher: `sim.host_status`, `sim.host_hold` and `sim.host_step` read,
    /// hold and step a threaded sim host, and answer their documented codes.
    #[test]
    fn sim_host_methods_dispatch_and_answer_their_codes() {
        use regia_framework::sim_bridge::BridgeInput;
        use regia_framework::sim_host::{FixedStepSim, HostPacing, SimHostSettings, ThreadedSimHost};

        struct Counter {
            tick: u64,
            panic_at: Option<u64>,
        }
        impl FixedStepSim for Counter {
            type Frame = u64;
            type Event = ();
            fn step(&mut self, _input: &BridgeInput) {
                self.tick += 1;
                assert_ne!(Some(self.tick), self.panic_at, "the test sim fails on purpose");
            }
            fn tick(&self) -> u64 {
                self.tick
            }
            fn write_frame(&self, out: &mut u64) {
                *out = self.tick;
            }
        }
        let manual = || SimHostSettings {
            pacing: HostPacing::Manual,
            ..SimHostSettings::default()
        };
        let code = |response: Response| -> Value {
            response.error.expect("an error").data.expect("data")["code"].clone()
        };

        let mut app = app();
        assert_eq!(code(call(&mut app, "sim.host_status", json!({}))), json!("F0003"));
        let host = ThreadedSimHost::start(
            Counter {
                tick: 0,
                panic_at: None,
            },
            manual(),
        )
        .expect("start");
        app.insert_resource(host.handle());
        let status = call(&mut app, "sim.host_status", json!({}))
            .result
            .expect("status");
        assert_eq!(
            (status["running"].clone(), status["manual"].clone()),
            (json!(true), json!(true))
        );
        crate::contract::assert_result_named("sim.host_status", &status);

        let bad = code(call(&mut app, "sim.host_step", json!({ "ticks": 0 })));
        assert_eq!(bad, json!("sim_host_bad_ticks"));
        crate::contract::assert_listed("sim.host_step", "sim_host_bad_ticks");
        let step = call(&mut app, "sim.host_step", json!({ "ticks": 3 }))
            .result
            .expect("step");
        assert_eq!(step["granted"], json!(3));
        crate::contract::assert_result_named("sim.host_step", &step);
        let mut pair = host.published();
        while pair.curr_tick < 3 {
            pair = host.wait_published(pair.sequence).expect("steps");
        }
        let status = call(&mut app, "sim.host_status", json!({}))
            .result
            .expect("status");
        assert_eq!(
            (status["ticks"].clone(), status["curr_tick"].clone()),
            (json!(3), json!(3))
        );

        let hold = call(&mut app, "sim.host_hold", json!({ "held": true }))
            .result
            .expect("hold");
        assert_eq!(
            (hold["held"].clone(), hold["was_held"].clone()),
            (json!(true), json!(false))
        );
        crate::contract::assert_result_named("sim.host_hold", &hold);
        assert_eq!(code(call(&mut app, "sim.host_hold", json!({}))), json!("F0002"));

        let realtime = ThreadedSimHost::start(
            Counter {
                tick: 0,
                panic_at: None,
            },
            SimHostSettings {
                start_held: true,
                ..SimHostSettings::default()
            },
        )
        .expect("start");
        app.insert_resource(realtime.handle());
        let not_manual = code(call(&mut app, "sim.host_step", json!({ "ticks": 1 })));
        assert_eq!(not_manual, json!("sim_host_not_manual"));
        crate::contract::assert_listed("sim.host_step", "sim_host_not_manual");

        app.insert_resource(host.handle());
        host.stop().expect("stop");
        let stopped = code(call(&mut app, "sim.host_hold", json!({ "held": false })));
        assert_eq!(stopped, json!("sim_host_stopped"));
        crate::contract::assert_listed("sim.host_hold", "sim_host_stopped");

        let failing = ThreadedSimHost::start(
            Counter {
                tick: 0,
                panic_at: Some(1),
            },
            manual(),
        )
        .expect("start");
        app.insert_resource(failing.handle());
        failing.handle().grant_ticks(1).expect("grant");
        assert!(failing.wait_published(0).is_err(), "the sim panicked");
        let panicked = code(call(&mut app, "sim.host_step", json!({ "ticks": 1 })));
        assert_eq!(panicked, json!("sim_host_panicked"));
        crate::contract::assert_listed("sim.host_step", "sim_host_panicked");
        crate::contract::assert_listed("sim.host_hold", "sim_host_panicked");
    }

    /// ADR-8498 through the dispatcher: `sim.tick_profile` returns the host report, a resource
    /// report, and the documented `no_tick_profile` / `bad_count` codes. The clock is a
    /// [`regia_detsim::profile::ManualClock`], so the rows are injected sample values.
    #[test]
    fn sim_tick_profile_agent_method_round_trip() {
        use regia_detsim::hash::Systems;
        use regia_detsim::profile::{ClockKind, ManualClock, ProfileSettings, TickProfiler};
        use regia_framework::sim_bridge::BridgeInput;
        use regia_framework::sim_host::{
            DetTickProfile, FixedStepSim, HostPacing, SimHostSettings, ThreadedSimHost,
        };

        struct Profiled {
            tick: u64,
            profiler: TickProfiler<2, ManualClock>,
        }
        impl FixedStepSim for Profiled {
            type Frame = u64;
            type Event = ();
            fn step(&mut self, _input: &BridgeInput) {
                self.tick += 1;
                let ai_ns = match self.tick {
                    2 => 5_000_000,
                    3 => 9_000_000,
                    _ => 1_000_000,
                };
                self.profiler.begin_tick();
                self.profiler.begin("move").expect("move");
                self.profiler.clock_mut().advance(1_000_000);
                self.profiler.end("move").expect("move");
                self.profiler.begin("ai").expect("ai");
                self.profiler.clock_mut().advance(ai_ns);
                self.profiler.end("ai").expect("ai");
                self.profiler.end_tick(self.tick);
            }
            fn tick(&self) -> u64 {
                self.tick
            }
            fn write_frame(&self, out: &mut u64) {
                *out = self.tick;
            }
            fn tick_profile(&self) -> Option<regia_framework::sim_host::ProfileReport> {
                Some(self.profiler.report())
            }
        }

        let code = |response: Response| -> Value {
            response.error.expect("an error").data.expect("data")["code"].clone()
        };
        let message = |response: Response| response.error.expect("an error").message;

        let mut app = app();
        let missing = call(&mut app, "sim.tick_profile", json!({}));
        assert_eq!(code(missing), json!("no_tick_profile"));
        assert!(message(call(&mut app, "sim.tick_profile", json!({}))).contains("[no_tick_profile]"));
        crate::contract::assert_listed("sim.tick_profile", "no_tick_profile");
        let bad = call(&mut app, "sim.tick_profile", json!({ "count": 0 }));
        assert_eq!(code(bad), json!("bad_count"));
        assert!(message(call(&mut app, "sim.tick_profile", json!({ "count": 0 }))).contains("[bad_count]"));
        crate::contract::assert_listed("sim.tick_profile", "bad_count");
        assert_eq!(
            code(call(&mut app, "sim.tick_profile", json!({ "count": 3601 }))),
            json!("bad_count")
        );
        assert_eq!(
            code(call(&mut app, "sim.tick_profile", json!({ "count": -1 }))),
            json!("bad_count")
        );

        let mut resource_profiler = TickProfiler::new(
            Systems::new(["move", "ai"]),
            ManualClock::new(ClockKind::Wall),
            ProfileSettings {
                ring_ticks: 4,
                ..ProfileSettings::default()
            },
        )
        .expect("resource profiler");
        resource_profiler.begin_tick();
        resource_profiler.begin("move").expect("move");
        resource_profiler.clock_mut().advance(10);
        resource_profiler.end("move").expect("move");
        resource_profiler.end_tick(1);
        app.insert_resource(DetTickProfile {
            report: resource_profiler.report(),
        });
        let resource_only = call(&mut app, "sim.tick_profile", json!({}))
            .result
            .expect("resource");
        assert_eq!(resource_only["samples"], json!(1));
        assert_eq!(resource_only["clock"], json!("wall"));
        crate::contract::assert_result_named("sim.tick_profile", &resource_only);

        let host = ThreadedSimHost::start(
            Profiled {
                tick: 0,
                profiler: TickProfiler::new(
                    Systems::new(["move", "ai"]),
                    ManualClock::new(ClockKind::Wall),
                    ProfileSettings::default(),
                )
                .expect("host profiler"),
            },
            SimHostSettings {
                pacing: HostPacing::Manual,
                ..SimHostSettings::default()
            },
        )
        .expect("start");
        app.insert_resource(host.handle());
        call(&mut app, "sim.host_step", json!({ "ticks": 3 }))
            .result
            .expect("grant");
        let mut pair = host.published();
        while pair.curr_tick < 3 {
            pair = host.wait_published(pair.sequence).expect("steps");
        }
        let answer = call(&mut app, "sim.tick_profile", json!({}))
            .result
            .expect("profile");
        assert_eq!(answer["samples"], json!(3));
        assert_eq!(answer["clock"], json!("wall"));
        assert_eq!(answer["tick_budget_ns"], json!(4_000_000));
        assert_eq!(answer["systems"][0]["system"], json!("move"));
        assert_eq!(answer["systems"][0]["p99_ns"], json!(1_000_000));
        assert_eq!(answer["systems"][1]["system"], json!("ai"));
        assert_eq!(answer["systems"][1]["p50_ns"], json!(5_000_000));
        assert_eq!(answer["systems"][1]["p99_ns"], json!(9_000_000));
        assert_eq!(answer["tick"]["p50_ns"], json!(6_000_000));
        assert_eq!(answer["tick"]["max_ns"], json!(10_000_000));
        assert_eq!(answer["over_budget"][0]["tick"], json!(2));
        assert_eq!(answer["over_budget"][1]["tick"], json!(3));
        crate::contract::assert_result_named("sim.tick_profile", &answer);

        let newest = call(&mut app, "sim.tick_profile", json!({ "count": 1 }))
            .result
            .expect("newest");
        assert_eq!(newest["over_budget"].as_array().expect("rows").len(), 1);
        assert_eq!(newest["over_budget"][0]["tick"], json!(3));
        assert_eq!(newest["over_budget"][0]["systems"][1]["ns"], json!(9_000_000));

        app.world_mut().remove_resource::<DetTickProfile>();
        // A sim that leaves the default `tick_profile` answers `no_tick_profile` after it steps.
        struct Quiet {
            tick: u64,
        }
        impl FixedStepSim for Quiet {
            type Frame = u64;
            type Event = ();
            fn step(&mut self, _input: &BridgeInput) {
                self.tick += 1;
            }
            fn tick(&self) -> u64 {
                self.tick
            }
            fn write_frame(&self, out: &mut u64) {
                *out = self.tick;
            }
        }
        let quiet = ThreadedSimHost::start(
            Quiet { tick: 0 },
            SimHostSettings {
                pacing: HostPacing::Manual,
                ..SimHostSettings::default()
            },
        )
        .expect("quiet");
        app.insert_resource(quiet.handle());
        call(&mut app, "sim.host_step", json!({ "ticks": 1 }))
            .result
            .expect("grant");
        let mut pair = quiet.published();
        while pair.curr_tick < 1 {
            pair = quiet.wait_published(pair.sequence).expect("step");
        }
        assert_eq!(
            code(call(&mut app, "sim.tick_profile", json!({}))),
            json!("no_tick_profile")
        );
    }

    /// KI-GP-005 (ADR-10062): `sim.host_snapshot { tick }` for a tick still ahead answers `pending`
    /// and keeps the request; once the sim has stepped past that tick the same call returns the
    /// snapshot taken at it (the same header hash as a snapshot taken at that tick directly); a
    /// tick the sim has passed answers `sim_host_snapshot_tick_passed`.
    #[test]
    fn sim_host_snapshot_takes_a_future_tick() {
        use regia_detsim::state::{DetReader, DetState, DetStateError, DetWriter};
        use regia_framework::sim_bridge::BridgeInput;
        use regia_framework::sim_host::{FixedStepSim, HostPacing, SimHostSettings, ThreadedSimHost};

        struct Counter {
            tick: u64,
            sum: u64,
        }
        impl FixedStepSim for Counter {
            type Frame = u64;
            type Event = ();
            fn step(&mut self, _input: &BridgeInput) {
                self.tick += 1;
                self.sum = self.sum.wrapping_mul(31).wrapping_add(self.tick);
            }
            fn tick(&self) -> u64 {
                self.tick
            }
            fn write_frame(&self, out: &mut u64) {
                *out = self.sum;
            }
        }
        impl DetState for Counter {
            const STATE_TAG: [u8; 8] = *b"counter\0";
            const STATE_VERSION: u32 = 1;
            fn save(&self, w: &mut DetWriter) {
                w.u64(self.tick);
                w.u64(self.sum);
            }
            fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
                Ok(Self {
                    tick: r.u64()?,
                    sum: r.u64()?,
                })
            }
        }
        let manual = || SimHostSettings {
            pacing: HostPacing::Manual,
            ..SimHostSettings::default()
        };
        // A second app for the direct snapshot below (made first: `app` is shadowed next).
        let mut other = app();
        let mut app = app();
        let host =
            ThreadedSimHost::start_with_snapshots(Counter { tick: 0, sum: 0 }, manual()).expect("start");
        let handle = host.handle();
        app.insert_resource(handle.clone());

        let ahead = call(&mut app, "sim.host_snapshot", json!({ "tick": 3 }))
            .result
            .expect("pending");
        crate::contract::assert_result_named("sim.host_snapshot", &ahead);
        assert_eq!(ahead, json!({ "tick": 3, "pending": true, "kept": false }));
        let still = call(&mut app, "sim.host_snapshot", json!({ "tick": 3 }))
            .result
            .expect("still pending");
        assert_eq!(still["pending"], json!(true));

        // Step past tick 3: the host thread took the snapshot between steps 3 and 4.
        handle.grant_ticks(4).expect("grant");
        handle.wait_status(|s| s.curr_tick == 4).expect("four ticks");
        let taken = call(&mut app, "sim.host_snapshot", json!({ "tick": 3 }))
            .result
            .expect("collected");
        assert_eq!(taken["tick"], json!(3));
        assert_eq!(taken["kept"], json!(true));
        assert!(taken.get("pending").is_none());

        // The same state taken directly at tick 3 has the same header hash.
        let direct =
            ThreadedSimHost::start_with_snapshots(Counter { tick: 0, sum: 0 }, manual()).expect("start");
        direct.handle().grant_ticks(3).expect("grant");
        direct
            .handle()
            .wait_status(|s| s.curr_tick == 3)
            .expect("three ticks");
        other.insert_resource(direct.handle());
        let at_three = call(&mut other, "sim.host_snapshot", json!({ "tick": 3 }))
            .result
            .expect("at the tick");
        assert_eq!(at_three["tick"], json!(3));
        assert_eq!(at_three["header_hash"], taken["header_hash"]);

        let passed = call(&mut app, "sim.host_snapshot", json!({ "tick": 1 }))
            .error
            .expect("passed")
            .data
            .expect("data")["code"]
            .clone();
        assert_eq!(passed, json!("sim_host_snapshot_tick_passed"));
        crate::contract::assert_listed("sim.host_snapshot", "sim_host_snapshot_tick_passed");
    }

    /// ADR-8526 through the dispatcher: `sim.host_snapshot` takes a framed snapshot on the host
    /// thread and keeps it, `sim.host_restore` loads it back (debug builds), `sim.host_status`
    /// reports the new counters, and each method answers its documented codes. Debug builds (the
    /// bytes and the restore are debug-only; the release gates are checked through `_with`).
    #[cfg(debug_assertions)]
    #[test]
    fn sim_host_snapshot_and_restore_dispatch_and_answer_their_codes() {
        use regia_detsim::state::{DetReader, DetState, DetStateError, DetWriter};
        use regia_framework::sim_bridge::BridgeInput;
        use regia_framework::sim_host::{FixedStepSim, HostPacing, SimHostSettings, ThreadedSimHost};

        struct Counter {
            tick: u64,
            sum: u64,
        }
        impl FixedStepSim for Counter {
            type Frame = u64;
            type Event = ();
            fn step(&mut self, _input: &BridgeInput) {
                self.tick += 1;
                self.sum = self.sum.wrapping_mul(31).wrapping_add(self.tick);
            }
            fn tick(&self) -> u64 {
                self.tick
            }
            fn write_frame(&self, out: &mut u64) {
                *out = self.sum;
            }
        }
        impl DetState for Counter {
            const STATE_TAG: [u8; 8] = *b"counter\0";
            const STATE_VERSION: u32 = 1;
            fn save(&self, w: &mut DetWriter) {
                w.u64(self.tick);
                w.u64(self.sum);
            }
            fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
                Ok(Self {
                    tick: r.u64()?,
                    sum: r.u64()?,
                })
            }
        }
        let manual = || SimHostSettings {
            pacing: HostPacing::Manual,
            ..SimHostSettings::default()
        };
        let code = |response: Response| -> Value {
            response.error.expect("an error").data.expect("data")["code"].clone()
        };

        let mut app = app();
        assert_eq!(
            code(call(&mut app, "sim.host_snapshot", json!({}))),
            json!("F0003")
        );
        // A host started without snapshots.
        let plain = ThreadedSimHost::start(Counter { tick: 0, sum: 0 }, manual()).expect("start");
        app.insert_resource(plain.handle());
        let none = code(call(&mut app, "sim.host_snapshot", json!({})));
        assert_eq!(none, json!("sim_host_no_snapshots"));
        crate::contract::assert_listed("sim.host_snapshot", "sim_host_no_snapshots");
        let empty = code(call(&mut app, "sim.host_restore", json!({})));
        assert_eq!(empty, json!("host_snapshot_none"));
        crate::contract::assert_listed("sim.host_restore", "host_snapshot_none");

        let host =
            ThreadedSimHost::start_with_snapshots(Counter { tick: 0, sum: 0 }, manual()).expect("start");
        let handle = host.handle();
        app.insert_resource(handle.clone());
        handle.grant_ticks(5).expect("grant");
        handle.wait_status(|s| s.curr_tick == 5).expect("five ticks");
        let snap = call(&mut app, "sim.host_snapshot", json!({ "include_bytes": true }))
            .result
            .expect("snapshot");
        crate::contract::assert_result_named("sim.host_snapshot", &snap);
        assert_eq!(snap["tick"], json!(5));
        assert_eq!(snap["tag"], json!("counter"));
        assert_eq!(snap["state_version"], json!(1));
        assert_eq!(snap["kept"], json!(true));
        let hex = snap["bytes_hex"]
            .as_str()
            .expect("bytes in a debug build")
            .to_string();
        assert_eq!(hex.len() as u64, 2 * snap["size"].as_u64().expect("size"));
        let lean = call(&mut app, "sim.host_snapshot", json!({}))
            .result
            .expect("snapshot");
        assert!(lean.get("bytes_hex").is_none(), "never the bytes unless asked");
        assert_eq!(lean["header_hash"], snap["header_hash"], "same state, same hash");

        handle.grant_ticks(4).expect("grant");
        handle.wait_status(|s| s.curr_tick == 9).expect("nine ticks");
        let restored = call(&mut app, "sim.host_restore", json!({}))
            .result
            .expect("restore the kept snapshot");
        crate::contract::assert_result_named("sim.host_restore", &restored);
        assert_eq!(restored["tick"], json!(5));
        assert_eq!(restored["status"]["curr_tick"], json!(5));
        assert_eq!(restored["status"]["restores"], json!(1));
        let status = call(&mut app, "sim.host_status", json!({}))
            .result
            .expect("status");
        crate::contract::assert_result_named("sim.host_status", &status);
        assert_eq!(status["snapshots"], json!(2));
        assert_eq!(
            status["fp_checks"],
            json!(9),
            "debug builds check before every step"
        );
        assert_eq!(status["lockstep"], json!(false));
        assert_eq!(*host.published().curr, {
            let mut sum = 0u64;
            for tick in 1..=5u64 {
                sum = sum.wrapping_mul(31).wrapping_add(tick);
            }
            sum
        });

        // The hex bytes restore too; a flipped digit is refused and the sim stays where it is.
        let back = call(&mut app, "sim.host_restore", json!({ "bytes_hex": hex.clone() }))
            .result
            .expect("restore from hex");
        assert_eq!(back["tick"], json!(5));
        let mut bad = hex.into_bytes();
        if let Some(last) = bad.last_mut() {
            *last = if *last == b'0' { b'1' } else { b'0' };
        }
        let bad = String::from_utf8(bad).expect("ascii");
        let refused = code(call(&mut app, "sim.host_restore", json!({ "bytes_hex": bad })));
        assert_eq!(refused, json!("sim_host_restore_failed"));
        crate::contract::assert_listed("sim.host_restore", "sim_host_restore_failed");
        assert_eq!(
            code(call(&mut app, "sim.host_restore", json!({ "bytes_hex": "abc" }))),
            json!("F0002")
        );

        // The release-build gates, exercised through the same handlers.
        let mut world_app = app;
        let world = world_app.world_mut();
        let disabled = crate::framework::sim_host_restore_with(world, &json!({}), false)
            .expect_err("release builds refuse");
        assert_eq!(
            disabled.data.expect("data")["code"],
            json!("host_restore_disabled")
        );
        crate::contract::assert_listed("sim.host_restore", "host_restore_disabled");
        let no_bytes =
            crate::framework::sim_host_snapshot_with(world, &json!({ "include_bytes": true }), false)
                .expect_err("release builds never return the bytes");
        assert_eq!(
            no_bytes.data.expect("data")["code"],
            json!("host_snapshot_bytes_disabled")
        );
        crate::contract::assert_listed("sim.host_snapshot", "host_snapshot_bytes_disabled");
        let lean =
            crate::framework::sim_host_snapshot_with(world, &json!({}), false).expect("a release snapshot");
        assert!(lean.get("bytes_hex").is_none());

        host.stop().expect("stop");
        let stopped = code(call(&mut world_app, "sim.host_snapshot", json!({})));
        assert_eq!(stopped, json!("sim_host_stopped"));
        crate::contract::assert_listed("sim.host_snapshot", "sim_host_stopped");
    }

    /// In a release-profile build the dispatcher itself refuses `sim.host_restore` and the bytes of
    /// `sim.host_snapshot` (`cargo test --release -p regia_agent --lib host_restore_disabled`).
    #[cfg(not(debug_assertions))]
    #[test]
    fn host_restore_disabled_in_release_builds() {
        let mut app = app();
        let response = call(&mut app, "sim.host_restore", json!({}));
        let error = response.error.expect("an error");
        assert_eq!(error.data.expect("data")["code"], json!("host_restore_disabled"));
        let response = call(&mut app, "sim.host_snapshot", json!({ "include_bytes": true }));
        let error = response.error.expect("an error");
        assert_eq!(
            error.data.expect("data")["code"],
            json!("host_snapshot_bytes_disabled")
        );
    }

    /// ADR-8456 through the dispatcher: `sim.bridge_settings` reads the handedness, teleport
    /// distance, delay, per-kind blend policies and counters; `sim.set_bridge_settings` changes them
    /// (dry run first) and the next frame poses the sim's entities by the new settings.
    #[test]
    fn sim_bridge_settings_dispatch_and_change_the_next_frame() {
        use regia_core::math::{Quat, Vec3};
        use regia_framework::sim_blend::SnapshotPair;
        use regia_framework::sim_bridge::{
            BridgeInput, ExternalSim, KindRegistry, SimBridgePlugin, SimEntityMap, SimEntityView,
            SimSnapshot, SnapshotTick,
        };

        #[derive(Default)]
        struct Snap(Vec3);
        impl SimSnapshot for Snap {
            fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
                f(SimEntityView {
                    id: 1,
                    kind: "unit",
                    team: 0,
                    translation: self.0,
                    rotation: Quat::IDENTITY,
                });
            }
        }
        /// Always halfway between x = 2 (prev) and x = 6 (curr).
        #[derive(Resource)]
        struct Halfway;
        impl ExternalSim for Halfway {
            type Snapshot = Snap;
            type Event = ();
            fn submit_input(&mut self, _input: &BridgeInput) {}
            fn read_snapshot(&mut self, _frame_time: std::time::Duration, out: &mut Snap) -> SnapshotTick {
                out.0 = Vec3::new(6.0, 0.0, 0.0);
                SnapshotTick(2)
            }
            fn read_pair(
                &mut self,
                _frame_time: std::time::Duration,
                _delay: std::time::Duration,
                prev: &mut Snap,
                curr: &mut Snap,
            ) -> Option<SnapshotPair> {
                prev.0 = Vec3::new(2.0, 0.0, 0.0);
                curr.0 = Vec3::new(6.0, 0.0, 0.0);
                Some(SnapshotPair {
                    prev_tick: SnapshotTick(1),
                    curr_tick: SnapshotTick(2),
                    alpha: 0.5,
                })
            }
            fn drain_events(&mut self, _out: &mut Vec<()>) {}
        }
        let code = |response: Response| -> Value {
            response.error.expect("an error").data.expect("data")["code"].clone()
        };
        let x_of_unit = |app: &mut App| -> f32 {
            let entity = app
                .world()
                .resource::<SimEntityMap>()
                .entity(1)
                .expect("the unit spawned");
            app.world()
                .get::<regia_scene::Transform>(entity)
                .expect("transform")
                .translation
                .x
        };

        let mut app = app();
        assert_eq!(
            code(call(&mut app, "sim.bridge_settings", json!({}))),
            json!("F0003")
        );
        assert_eq!(
            code(call(
                &mut app,
                "sim.set_bridge_settings",
                json!({ "handedness": "right_handed" })
            )),
            json!("F0003")
        );
        let kinds =
            KindRegistry::from_ron(r#"(kinds: { "unit": (prefab: None, presentation_scripts: []) })"#)
                .expect("kinds");
        app.add_plugins(SimBridgePlugin::new(Halfway).kinds(kinds));
        app.update();
        assert_eq!(x_of_unit(&mut app), 4.0, "lerped halfway by default");

        let read = call(&mut app, "sim.bridge_settings", json!({}))
            .result
            .expect("settings");
        crate::contract::assert_result_named("sim.bridge_settings", &read);
        assert_eq!(read["handedness"], json!("right_handed"));
        assert_eq!(read["teleport_distance"], Value::Null);
        assert_eq!(read["kinds"]["unit"], json!("lerp"));
        assert_eq!(read["default_blend"], json!("lerp"));
        assert_eq!(read["stats"]["lerped"], json!(1));
        assert_eq!(read["stats"]["pair_reads"], json!(1));

        // A dry run reports the change and leaves everything as it was.
        let dry = call(
            &mut app,
            "sim.set_bridge_settings",
            json!({ "kind": "unit", "blend": "hold", "handedness": "left_handed_reflect_x", "dry_run": true }),
        )
        .result
        .expect("dry run");
        crate::contract::assert_result_named("sim.set_bridge_settings", &dry);
        assert_eq!(dry["applied"], json!(false));
        assert_eq!(dry["after"]["kinds"]["unit"], json!("hold"));
        assert_eq!(dry["after"]["handedness"], json!("left_handed_reflect_x"));
        app.update();
        assert_eq!(x_of_unit(&mut app), 4.0, "a dry run changes nothing");

        // Hold draws `prev`; the reflection negates X once.
        let set = call(
            &mut app,
            "sim.set_bridge_settings",
            json!({ "kind": "unit", "blend": "hold", "handedness": "left_handed_reflect_x" }),
        )
        .result
        .expect("set");
        assert_eq!(set["applied"], json!(true));
        assert_eq!(set["before"]["kinds"]["unit"], json!("lerp"));
        app.update();
        assert_eq!(x_of_unit(&mut app), -2.0);
        // A teleport distance below the 4 m jump snaps a lerped kind to `curr`.
        call(
            &mut app,
            "sim.set_bridge_settings",
            json!({ "kind": "unit", "blend": "lerp", "teleport_distance": 1.0, "presentation_delay_ms": 25.0 }),
        )
        .result
        .expect("set");
        app.update();
        assert_eq!(x_of_unit(&mut app), -6.0);
        let read = call(&mut app, "sim.bridge_settings", json!({}))
            .result
            .expect("settings");
        assert_eq!(read["stats"]["teleports"], json!(1));
        assert_eq!(read["presentation_delay_ms"], json!(25.0));
        assert_eq!(read["teleport_distance"], json!(1.0));

        for (params, expect) in [
            (json!({ "handedness": "left" }), "sim_bad_handedness"),
            (json!({ "teleport_distance": -1.0 }), "sim_bad_teleport_distance"),
            (json!({ "presentation_delay_ms": 1.0e9 }), "sim_bad_delay"),
            (json!({ "kind": "unit", "blend": "slerp" }), "sim_bad_blend"),
            (json!({ "kind": "nobody", "blend": "snap" }), "sim_kind_unknown"),
        ] {
            assert_eq!(
                code(call(&mut app, "sim.set_bridge_settings", params)),
                json!(expect)
            );
            crate::contract::assert_listed("sim.set_bridge_settings", expect);
        }
        assert_eq!(
            code(call(
                &mut app,
                "sim.set_bridge_settings",
                json!({ "kind": "unit" })
            )),
            json!("F0002")
        );
    }

    #[test]
    fn overlay_font_loading_controls_and_inspection_through_dispatch() {
        use regia_framework::loading_widget::{apply_pipeline_warm_progress, spawn_pipeline_loading_widget};
        use regia_framework::perf_overlay::{PerformanceOverlay, PerformanceOverlayMode};
        let mut app = app();
        app.insert_resource(regia_ui::text::TextSystem::embedded_only());
        app.insert_resource(regia_ui::Localization::default());
        app.insert_resource(PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact));
        let changed = call(&mut app, "ui.set_default_font", json!({"family":"Noto Sans SC"}))
            .result
            .expect("font");
        assert_eq!(changed["family"], "Noto Sans SC");
        assert_eq!(changed["changed"], true);
        let shaped = call(&mut app, "ui.text_shape", json!({"text":"Hello 你好"}))
            .result
            .expect("shape");
        assert_eq!(shaped["default_family"], "Noto Sans SC");
        assert_eq!(shaped["missing_glyphs"], 0);
        let rejected = call(&mut app, "ui.set_default_font", json!({"family":"missing font"}))
            .error
            .expect("invalid font");
        assert_eq!(rejected.data.expect("error data")["code"], "bad_family");
        crate::contract::assert_listed("ui.set_default_font", "bad_family");
        assert_eq!(
            app.world()
                .resource::<regia_ui::text::TextSystem>()
                .default_family(),
            "Noto Sans SC"
        );
        let reset = call(&mut app, "ui.set_default_font", json!({"family":null}))
            .result
            .expect("reset");
        assert_eq!(reset["family"], "Inter");
        let placement = call(
            &mut app,
            "perf_overlay.toggle",
            json!({"placement":{"corner":"bottom_right","offset":[24,32]}}),
        )
        .result
        .expect("placement");
        assert_eq!(placement["after"]["mode"], "Compact");
        assert_eq!(placement["after"]["source"], "host_default");
        let got = call(&mut app, "perf_overlay.get", json!({}))
            .result
            .expect("overlay");
        assert_eq!(got["placement"]["corner"], "bottom_right");
        assert_eq!(got["placement"]["offset"], json!([24.0, 32.0]));
        assert!(
            call(
                &mut app,
                "perf_overlay.toggle",
                json!({"mode":"off","placement":{"corner":"bottom_right","offset":[-1,0]}})
            )
            .error
            .is_some()
        );
        assert_eq!(
            call(&mut app, "perf_overlay.get", json!({}))
                .result
                .expect("unchanged")["mode"],
            "Compact"
        );
        let root = app
            .world_mut()
            .spawn((regia_ui::UiRoot::default(), regia_ui::UiNode))
            .id();
        let widget = spawn_pipeline_loading_widget(app.world_mut(), root);
        let mut progress = regia_render::PipelineWarmProgress {
            total: 8,
            done: 2,
            started: true,
            ..Default::default()
        };
        apply_pipeline_warm_progress(app.world_mut(), &progress, 0);
        apply_pipeline_warm_progress(app.world_mut(), &progress, 5_001);
        let inspect = call(&mut app, "ui.inspect", json!({"node":widget.to_bits()}))
            .result
            .expect("inspect");
        assert_eq!(inspect["pipeline_loading"]["state"]["fraction"], 0.25);
        assert_eq!(inspect["pipeline_loading"]["notice"]["visible"], true);
        progress.done = 8;
        apply_pipeline_warm_progress(app.world_mut(), &progress, 5_002);
        let ready = call(&mut app, "ui.inspect", json!({"node":widget.to_bits()}))
            .result
            .expect("ready");
        assert_eq!(ready["pipeline_loading"]["state"]["busy"], false);
        assert_eq!(ready["pipeline_loading"]["notice"]["visible"], false);
    }

    #[test]
    fn overlay_font_loading_screenshot_and_expect_through_dispatch() {
        use regia_framework::loading_widget::{apply_pipeline_warm_progress, spawn_pipeline_loading_widget};
        use regia_ui::style::{Dimension, Edges, Style};
        let mut app = app();
        app.insert_resource(regia_ui::text::TextSystem::embedded_only());
        app.init_resource::<regia_render::WindowOverlay>();
        app.init_resource::<regia_render::RuntimeRenderer>();
        app.add_plugins(regia_ui::UiPlugin::code_only());
        app.insert_resource(crate::UiAgentHost::new(regia_ui::agent_bridge::host_call));
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let root = app
            .world_mut()
            .spawn((regia_ui::UiRoot::default(), regia_ui::UiNode))
            .id();
        let panel = spawn_pipeline_loading_widget(app.world_mut(), root);
        app.world_mut()
            .entity_mut(panel)
            .insert(regia_ui::NodeStyle::new(Style {
                size: [Dimension::Points(400.0), Dimension::Auto],
                padding: Edges::px(20.0),
                direction: regia_ui::style::FlexDirection::Column,
                ..Default::default()
            }));
        let mut progress = regia_render::PipelineWarmProgress {
            total: 8,
            done: 2,
            started: true,
            ..Default::default()
        };
        apply_pipeline_warm_progress(app.world_mut(), &progress, 0);
        app.update();
        app.update();
        let waited = call(
            &mut app,
            "play.wait_until",
            json!({
                "condition":{"predicate":"ui.node_exists","node":panel.to_bits()}
            }),
        )
        .result
        .expect("wait result");
        assert_eq!(waited["satisfied"], true, "{waited}");
        let expected = call(&mut app, "play.expect", json!({"assertions":[{
            "name":"loading panel exists", "condition":{"predicate":"ui.node_exists","node":panel.to_bits()}
        }]})).result.expect("expect result");
        assert_eq!(expected["pass"], true, "{expected}");
        let capture = |app: &mut App| {
            let shot = call(
                app,
                "seeing.screenshot",
                json!({"target":"ui","width":1280,"height":720,"calibration":"neutral"}),
            );
            let shot = shot
                .result
                .unwrap_or_else(|| panic!("screenshot: {:?}", shot.error));
            let bytes =
                crate::image::base64_decode(shot["png_base64"].as_str().expect("png")).expect("base64");
            let image = crate::image::decode_png_rgba_within(&bytes, 2048).expect("image");
            (
                bytes,
                image
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|pixel| pixel[3] != 0)
                    .count(),
            )
        };
        let (busy_png, busy_pixels) = capture(&mut app);
        assert!(busy_pixels > 100, "loading text and bar must draw: {busy_pixels}");
        progress.done = 8;
        apply_pipeline_warm_progress(app.world_mut(), &progress, 1);
        app.update();
        let (_, ready_pixels) = capture(&mut app);
        assert_eq!(ready_pixels, 0, "ready panel leaves the UI layer transparent");
        if let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") {
            let dir = std::path::Path::new(&dir);
            std::fs::create_dir_all(dir).expect("evidence directory");
            let tmp = dir.join(format!("loading-busy-{}.tmp", std::process::id()));
            let path = dir.join("loading-busy.png");
            std::fs::write(&tmp, busy_png).expect("evidence");
            std::fs::rename(tmp, &path).expect("publish evidence");
            eprintln!(
                "loading screenshot: {}; busy pixels: {busy_pixels}",
                path.display()
            );
        }
    }

    /// ADR-8562 through the dispatcher: `sim.det_debug_draw` sets what a host's sim draws, counts
    /// the frame (one contact line per contact of the last pass), reads a `DetDebugDraw` resource,
    /// and answers the documented `no_det_debug` / `bad_what` codes.
    #[test]
    fn sim_det_debug_draw_agent_method_round_trip() {
        use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};
        use regia_framework::det_debug::{DebugRecorder, DebugWhat, DetDebugDraw};
        use regia_framework::sim_bridge::BridgeInput;
        use regia_framework::sim_host::{FixedStepSim, HostPacing, SimHostSettings, ThreadedSimHost};

        struct Crowd {
            tick: u64,
            bodies: Vec<BroadBody>,
            grid: DetBroadphase,
            contacts: u64,
        }
        impl FixedStepSim for Crowd {
            type Frame = u64;
            type Event = ();
            fn step(&mut self, _input: &BridgeInput) {
                self.tick += 1;
                self.grid.for_each_contact(&mut self.bodies, |_, _| {});
                self.contacts = self.grid.stats().contacts;
            }
            fn tick(&self) -> u64 {
                self.tick
            }
            fn write_frame(&self, out: &mut u64) {
                *out = self.contacts;
            }
            fn det_debug(&mut self, what: DebugWhat, out: &mut DebugRecorder) -> bool {
                self.grid
                    .set_debug_capture((!what.is_empty()).then(Default::default));
                self.grid.debug_emit(out, what);
                true
            }
        }

        let code = |response: Response| -> Value {
            response.error.expect("an error").data.expect("data")["code"].clone()
        };
        let message = |response: Response| response.error.expect("an error").message;

        let mut app = app();
        assert_eq!(
            code(call(&mut app, "sim.det_debug_draw", json!({}))),
            json!("no_det_debug")
        );
        assert!(message(call(&mut app, "sim.det_debug_draw", json!({}))).contains("[no_det_debug]"));
        crate::contract::assert_listed("sim.det_debug_draw", "no_det_debug");

        // A resource-only game.
        app.insert_resource(DetDebugDraw::default());
        assert_eq!(
            code(call(
                &mut app,
                "sim.det_debug_draw",
                json!({ "what": ["grid", "bogus"] })
            )),
            json!("bad_what")
        );
        assert!(message(call(&mut app, "sim.det_debug_draw", json!({ "what": 3 }))).contains("[bad_what]"));
        crate::contract::assert_listed("sim.det_debug_draw", "bad_what");
        let set = call(
            &mut app,
            "sim.det_debug_draw",
            json!({ "what": ["grid", "contacts"] }),
        )
        .result
        .expect("set");
        assert_eq!(set["source"], json!("resource"));
        assert_eq!(set["what"], json!(["grid", "contacts"]));
        assert_eq!(set["tick"], Value::Null);
        crate::contract::assert_result_named("sim.det_debug_draw", &set);
        assert_eq!(
            app.world().resource::<DetDebugDraw>().what,
            DebugWhat::GRID.union(DebugWhat::CONTACTS)
        );
        let mut grid = DetBroadphase::new(BroadphaseConfig::default());
        let mut bodies = vec![
            BroadBody::new([0.0, 0.0, 0.0], 1.0, [-1.0, 2.0]),
            BroadBody::new([1.5, 0.0, 0.0], 1.0, [-1.0, 2.0]),
            BroadBody::new([2.5, 0.0, 0.0], 1.0, [-1.0, 2.0]),
        ];
        {
            let mut draw = app.world_mut().resource_mut::<DetDebugDraw>();
            draw.record(0, &mut [&mut grid]);
            grid.for_each_contact(&mut bodies, |_, _| {});
            draw.record(1, &mut [&mut grid]);
        }
        let read = call(&mut app, "sim.det_debug_draw", json!({}))
            .result
            .expect("read");
        assert_eq!(read["tick"], json!(1));
        assert_eq!(read["kinds"]["contact"], json!(grid.stats().contacts));
        assert_eq!(read["kinds"]["contact"], json!(2), "(0, 1) and (1, 2) touch");

        // A sim host whose sim draws wins over the resource.
        let host = ThreadedSimHost::start(
            Crowd {
                tick: 0,
                bodies: (0..12)
                    .map(|i| BroadBody::new([i as f32 * 1.2, 0.0, 0.0], 1.0, [-1.0, 2.0]))
                    .collect(),
                grid: DetBroadphase::new(BroadphaseConfig::default()),
                contacts: 0,
            },
            SimHostSettings {
                pacing: HostPacing::Manual,
                ..SimHostSettings::default()
            },
        )
        .expect("start");
        assert!(host.handle().det_debug_supported());
        app.insert_resource(host.handle());
        let on = call(&mut app, "sim.det_debug_draw", json!({ "what": "all" }))
            .result
            .expect("on");
        assert_eq!(on["source"], json!("host"));
        assert_eq!(
            on["what"],
            json!(["grid", "occupancy", "candidates", "contacts", "queries", "flow"])
        );
        // The first step after the switch turns capture on; the second draws its contacts.
        call(&mut app, "sim.host_step", json!({ "ticks": 2 }))
            .result
            .expect("grant");
        let mut pair = host.published();
        while pair.curr_tick < 2 {
            pair = host.wait_published(pair.sequence).expect("steps");
        }
        let contacts = *pair.curr;
        assert_eq!(contacts, 11, "a row of touching discs");
        let drawn = call(&mut app, "sim.det_debug_draw", json!({}))
            .result
            .expect("drawn");
        assert_eq!(drawn["tick"], json!(2));
        assert_eq!(drawn["kinds"]["contact"], json!(contacts));
        assert!(drawn["kinds"]["grid"].as_u64().unwrap_or(0) > 0);
        assert!(drawn["kinds"]["occupancy"].as_u64().unwrap_or(0) > 0);
        crate::contract::assert_result_named("sim.det_debug_draw", &drawn);
        assert_eq!(
            app.world().resource::<DetDebugDraw>().what,
            DebugWhat::ALL,
            "the resource follows the host switch"
        );

        let off = call(&mut app, "sim.det_debug_draw", json!({ "what": "none" }))
            .result
            .expect("off");
        assert_eq!(off["what"], json!([]));
        call(&mut app, "sim.host_step", json!({ "ticks": 1 }))
            .result
            .expect("grant");
        while pair.curr_tick < 3 {
            pair = host.wait_published(pair.sequence).expect("step");
        }
        let after = call(&mut app, "sim.det_debug_draw", json!({}))
            .result
            .expect("after");
        assert_eq!(after["tick"], Value::Null, "switching off drops the frame");
    }

    /// ADR-7210 through the dispatcher: `sim.counters` reports the router's applied reactions and
    /// `vfx.describe` a mesh particle's render mesh (`missing_mesh` when nothing is registered).
    #[test]
    fn sim_counters_reactions_and_describe_render_mesh_dispatch() {
        let mut app = app();
        if !app.world().contains_resource::<Time>() {
            app.init_resource::<Time>();
        }
        app.init_resource::<regia_framework::sim_bridge::SimCounters>();
        app.insert_resource(regia_framework::sim_bridge::RoutedReactionStats {
            vfx: 3,
            decals: 2,
            unplaced: 1,
            ..Default::default()
        });
        let counters = call(&mut app, "sim.counters", json!({}))
            .result
            .expect("counters");
        assert_eq!(counters["reactions"]["vfx"], json!(3));
        assert_eq!(counters["reactions"]["decals"], json!(2));
        assert_eq!(counters["reactions"]["unplaced"], json!(1));

        let def = regia_vfx::VfxSystemDef::from_ron(&format!(
            r#"(version: {}, name: "shards", emitters: [(
    name: "e", spawn: (mode: Burst(1), lifetime: (min: 1.0, max: 1.0)),
    init: (size: (min: 0.1, max: 0.1)),
    render: (mode: Mesh, lit: true, mesh: Some("rock")),
)])"#,
            regia_vfx::VFX_FORMAT_VERSION
        ))
        .expect("v8 effect");
        let mut library = regia_vfx::scene::VfxLibrary::new();
        library.add(def).expect("valid");
        app.insert_resource(regia_vfx::VfxScene::new(library));
        let described = call(&mut app, "vfx.describe", json!({ "system": "shards" }))
            .result
            .expect("described");
        let render = &described["emitters"][0]["render"];
        assert_eq!(render["mode"], json!("mesh"));
        assert_eq!(render["mesh"], json!("rock"));
        assert_eq!(render["mesh_status"], json!("missing_mesh"));
        assert_eq!(render["lit"], json!(true));
        assert_eq!(described["mesh_draws"], Value::Null, "no GPU context in this app");
    }

    #[test]
    fn unknown_methods_list_the_surface() {
        let mut app = app();
        let response = call(&mut app, "scene.warp", json!({}));
        let error = response.error.expect("rejected");
        assert_eq!(error.code, protocol::METHOD_NOT_FOUND);
        let hint = error
            .data
            .as_ref()
            .and_then(|d| d["hint"].as_str())
            .unwrap_or_default()
            .to_string();
        assert!(hint.contains("scene.spawn"), "the hint lists methods: {hint}");
    }

    #[test]
    fn denied_permissions_answer_f0012_and_audit() {
        let mut app = app();
        let dir = std::env::temp_dir().join(format!("regia-agent-audit-{}-denied", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        app.insert_resource(AuditLog::open(dir.join("audit.jsonl")));

        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                read_only: true,
                ..AgentConfig::default()
            },
            source: None,
        });
        let response = call(&mut app, "scene.spawn", json!({ "name": "X" }));
        let error = response.error.expect("denied");
        assert_eq!(error.code, protocol::PERMISSION_DENIED);
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .map(String::from),
            Some("F0012".into())
        );

        drop(app.world_mut().remove_resource::<AuditLog>().expect("log"));
        let text = std::fs::read_to_string(dir.join("audit.jsonl")).expect("audit");
        assert!(text.contains("scene.spawn"), "{text}");
        assert!(text.contains("F0012"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_assets_does_not_grant_assets_load() {
        let mut app = app();
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                permissions: ["read", "edit_scene", "edit_assets", "build", "play"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                ..AgentConfig::default()
            },
            source: None,
        });
        let response = call(&mut app, "assets.load", json!({ "asset": "a.png" }));
        let error = response.error.expect("denied");
        assert_eq!(error.code, protocol::PERMISSION_DENIED);
        let data = error.data.expect("data");
        assert_eq!(data["code"], "F0012");
        let hint = data["hint"].as_str().unwrap_or("");
        assert!(hint.contains("\"assets\""), "{hint}");
    }

    /// audit-agent-sec (Low): a huge `agent.hello` identity or method name was copied whole into
    /// every audit line.
    #[test]
    fn audit_lines_cap_the_method_and_the_session_identity() {
        let mut app = app();
        let dir = regia_test_utils::unique_temp_dir("agent-audit-caps");
        app.insert_resource(AuditLog::open(dir.join("audit.jsonl")));
        // A multi-byte character straddles the cut: the cut lands on a boundary.
        let long_client = format!(
            "{}é{}",
            "c".repeat(MAX_SESSION_NAME_BYTES - 1),
            "x".repeat(10_000)
        );
        let hello = call(
            &mut app,
            "agent.hello",
            json!({ "client": long_client, "model": "m".repeat(10_000) }),
        );
        assert!(hello.error.is_none(), "{:?}", hello.error);
        let session = app.world().resource::<AgentSession>();
        assert_eq!(session.client, "c".repeat(MAX_SESSION_NAME_BYTES - 1));
        assert_eq!(session.model.len(), MAX_SESSION_NAME_BYTES);
        let unknown = call(&mut app, &"a".repeat(100_000), json!({}));
        assert_eq!(unknown.error.expect("unknown").code, protocol::METHOD_NOT_FOUND);
        drop(app.world_mut().remove_resource::<AuditLog>().expect("log"));
        let text = std::fs::read_to_string(dir.join("audit.jsonl")).expect("audit");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        for line in &lines {
            assert!(line.len() < 1024, "{} bytes", line.len());
        }
        let last: Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(last["method"], "a".repeat(MAX_ECHOED_METHOD_BYTES));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mutating_calls_are_rate_limited() {
        let mut app = app();
        app.world_mut().resource_mut::<RateLimit>().admitted =
            std::iter::repeat_n(Instant::now(), 50).collect();
        let response = call(&mut app, "play.pause", json!({}));
        let error = response.error.expect("limited");
        assert_eq!(error.code, protocol::RATE_LIMITED);
        // Read-only calls are never limited.
        app.world_mut().resource_mut::<RateLimit>().admitted =
            std::iter::repeat_n(Instant::now(), 50).collect();
        let response = call(&mut app, "agent.hello", json!({}));
        assert!(response.error.is_none());
    }

    #[test]
    fn refused_mutating_calls_do_not_spend_the_heavy_budget() {
        let mut app = app();
        let config = AgentConfig::default();
        // Keep the fixture full independently of how long the test runner is descheduled.
        let stamp = Instant::now() + Duration::from_secs(3600);
        app.world_mut().resource_mut::<RateLimit>().admitted =
            std::iter::repeat_n(stamp, config.rate_limit_per_second as usize).collect();
        for _ in 0..=config.heavy_rate_limit_per_second {
            let error = call(&mut app, "nav.crowd_step", json!({ "steps": 1 }))
                .error
                .expect("mutating budget is full");
            assert_eq!(error.code, protocol::RATE_LIMITED);
            assert_eq!(error.message, "too many mutating calls");
            assert!(
                app.world().resource::<RateLimit>().heavy.is_empty(),
                "a refused mutating call must not spend the heavy budget"
            );
        }
        let error = call(&mut app, "nav.get_mesh", json!({}))
            .error
            .expect("no mesh installed");
        assert_eq!(error.code, protocol::NOT_FOUND, "the read reached its handler");
        assert_eq!(app.world().resource::<RateLimit>().heavy.len(), 1);
    }

    #[test]
    fn refused_scoped_calls_do_not_spend_the_heavy_budget() {
        let mut app = app();
        let (_, scope) = crate::scope::ToolTokens::new().issue(crate::scope::ToolScope::new(
            "budget-test",
            [Permission::Read, Permission::Play],
            vec!["nav.*".into()],
        ));
        let rate = RateLimit {
            admitted: std::iter::repeat_n(
                Instant::now() + Duration::from_secs(3600),
                AgentConfig::default().rate_limit_per_second as usize,
            )
            .collect(),
            ..Default::default()
        };
        app.insert_resource(ScopedRateLimits([(scope.id, rate)].into()));
        let mut request = request("2.0", Some(json!(1)), "nav.crowd_step");
        request.params = json!({ "steps": 1 });
        let (reply, _answer) = crossbeam_channel::bounded(1);
        let Outcome::Done(Err(error)) =
            handle(app.world_mut(), &request, json!(1), reply, Some(&scope), None)
        else {
            panic!("the scoped mutation must be refused");
        };
        assert_eq!(error.message, "too many mutating calls");
        let limits = app.world().resource::<ScopedRateLimits>();
        assert!(
            limits.0[&scope.id].heavy.is_empty(),
            "no partial admission for a scope"
        );
        assert!(
            app.world().resource::<RateLimit>().heavy.is_empty(),
            "the master budget is separate"
        );
    }

    #[test]
    fn golden_path_refusals_do_not_spend_either_budget() {
        let mut app = app();
        let error = call(
            &mut app,
            "nav.stream_load_cell",
            json!({ "blob": "fixtures/goldens/cell.bin" }),
        )
        .error
        .expect("golden path refused");
        assert_eq!(error.data.expect("reason")["reason"], "golden_path_denied");
        let rate = app.world().resource::<RateLimit>();
        assert!(rate.heavy.is_empty(), "golden refusal spent the heavy budget");
        assert!(
            rate.admitted.is_empty(),
            "golden refusal spent the mutating budget"
        );
    }

    #[test]
    fn authored_edits_are_stamped_into_the_change_log() {
        let mut app = app();
        let spawned = call(
            &mut app,
            "scene.spawn",
            json!({ "name": "Agent Rock", "components": { "Transform": {
                "translation": [0.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0]
            } } }),
        );
        let entity = spawned.result.expect("spawned")["entity"].as_u64().expect("id");
        let log = app.world().get_resource::<ChangeLog>().expect("change log");
        let record = log.records.first().expect("one record");
        assert_eq!(record.method, "scene.spawn");
        assert_eq!(record.target, "spawn Agent Rock");
        assert_eq!(record.summary["entity"], entity);
        assert_eq!(record.client, "unknown", "session not yet announced");

        // hello names the session; the next edit carries the client and model.
        let hello = call(
            &mut app,
            "agent.hello",
            json!({ "client": "claude", "model": "sonnet" }),
        );
        assert!(hello.error.is_none(), "hello failed: {:?}", hello.error);
        let edit = call(
            &mut app,
            "scene.set_property",
            json!({ "entity": entity, "component": "Transform", "path": "translation.y", "value": 2.0 }),
        );
        assert!(edit.error.is_none(), "set_property failed: {:?}", edit.error);
        let log = app.world().get_resource::<ChangeLog>().expect("change log");
        assert_eq!(log.records.len(), 2, "spawn + set_property recorded");
        let record = log.records.last().expect("two records");
        assert_eq!(record.client, "claude");
        assert_eq!(record.model, "sonnet");
        assert!(
            record.target.contains("Transform.translation.y"),
            "{}",
            record.target
        );
    }

    /// Spec 23 § 3 (ADR-5628): `dry_run` only skips the change log for a method whose schema
    /// lists it. `scene.spawn` has no `dry_run`, ignores the flag and spawns, so the applied edit
    /// must be recorded; a real dry run (`scene.set_property { dry_run: true }`) is not.
    #[test]
    fn dry_run_skips_the_change_log_only_where_the_schema_lists_it() {
        let spawn = schema::method("scene.spawn").expect("scene.spawn");
        assert!(
            spawn.params.iter().all(|p| p.name != "dry_run"),
            "spawn has no dry_run"
        );
        let set = schema::method("scene.set_property").expect("scene.set_property");
        assert!(
            set.params.iter().any(|p| p.name == "dry_run"),
            "set_property has dry_run"
        );

        let mut app = app();
        let spawned = call(
            &mut app,
            "scene.spawn",
            json!({ "name": "Flagged Rock", "dry_run": true, "components": { "Transform": {
                "translation": [0.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0]
            } } }),
        );
        let entity = spawned
            .result
            .expect("the flag is ignored and the entity spawned")["entity"]
            .as_u64()
            .expect("id");
        let records = |app: &App| {
            app.world()
                .get_resource::<ChangeLog>()
                .map(|log| log.records.iter().map(|r| r.method.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(
            records(&app),
            vec!["scene.spawn".to_string()],
            "the applied spawn is recorded"
        );

        let preview = call(
            &mut app,
            "scene.set_property",
            json!({ "entity": entity, "component": "Transform", "path": "translation.y",
                    "value": 2.0, "dry_run": true }),
        );
        assert!(preview.error.is_none(), "dry run failed: {:?}", preview.error);
        assert_eq!(
            records(&app),
            vec!["scene.spawn".to_string()],
            "the dry run is not recorded"
        );
    }

    /// Editor-surface methods ride the same permission/audit path through the extension hook.
    #[test]
    fn extension_methods_dispatch_through_the_same_gates() {
        let mut app = app();
        let mut extension = crate::extension::AgentExtension::new();
        extension.register(
            crate::schema::MethodSchema {
                name: "editor.poke",
                tool: "poke",
                description: "Test extension method.",
                params: &[],
                result: "{ ok }",
                mutating: true,
                permission: Permission::EditScene,
            },
            |_world, _params| Ok(json!({ "ok": true })),
        );
        app.insert_resource(extension);

        let response = call(&mut app, "editor.poke", json!({}));
        assert!(response.result.is_some(), "extension method answered");

        // A read-only profile denies it through the standard F0012 path.
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                read_only: true,
                ..AgentConfig::default()
            },
            source: None,
        });
        let denied = call(&mut app, "editor.poke", json!({}));
        let error = denied.error.expect("denied");
        assert_eq!(error.code, protocol::PERMISSION_DENIED);
    }

    /// `camera.set_path`, `render.sequence` and `camera.raycast` through the dispatcher: a path
    /// is authored, a seek moves the camera on the next frame, the raycast starts at the new pose,
    /// and a read-only profile still allows the raycast but not the sequence.
    #[test]
    fn camera_paths_dispatch_and_drive_the_camera() {
        let mut app = app();
        regia_framework::shots::player::install(&mut app);
        let camera = app
            .world_mut()
            .spawn((regia_render::Camera::default(), regia_scene::Transform::default()))
            .id();

        let set = call(
            &mut app,
            "camera.set_path",
            json!({
                "path": {
                    "interp": "Linear",
                    "keys": [
                        { "time": 0.0, "position": [0, 0, 0] },
                        { "time": 2.0, "position": [4, 0, 0] }
                    ],
                    "look_at": { "Point": [0, 0, -5] }
                },
                "preview": 2
            }),
        );
        let result = set
            .result
            .unwrap_or_else(|| panic!("set_path failed: {:?}", set.error));
        assert_eq!(result["created"], true);
        assert_eq!(result["preview"][1]["eye"], json!([4.0, 0.0, 0.0]));

        let seek = call(
            &mut app,
            "render.sequence",
            json!({ "action": "seek", "time": 1.0 }),
        );
        let result = seek
            .result
            .unwrap_or_else(|| panic!("seek failed: {:?}", seek.error));
        assert_eq!(result["state"]["sample"]["eye"], json!([2.0, 0.0, 0.0]));

        app.update();
        let pose = app
            .world()
            .get::<regia_scene::Transform>(camera)
            .expect("camera transform");
        assert_eq!(pose.translation, regia_core::math::Vec3::new(2.0, 0.0, 0.0));

        let ray = call(
            &mut app,
            "camera.raycast",
            json!({ "u": 0.5, "v": 0.5, "ray_only": true }),
        );
        let result = ray
            .result
            .unwrap_or_else(|| panic!("raycast failed: {:?}", ray.error));
        assert_eq!(result["camera"], camera.to_bits());
        assert_eq!(result["origin"], json!([2.0, 0.0, 0.0]));
        assert!(
            result["direction"][0].as_f64().is_some_and(|x| x < 0.0),
            "looks toward x = 0"
        );

        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                read_only: true,
                ..AgentConfig::default()
            },
            source: None,
        });
        let denied = call(&mut app, "render.sequence", json!({ "action": "stop" }));
        assert_eq!(
            denied.error.expect("denied").code,
            protocol::PERMISSION_DENIED,
            "moving the camera needs edit_scene"
        );
        let denied = call(
            &mut app,
            "camera.set_path",
            json!({ "path": [{ "t": 0.0, "pos": [0.0, 0.0, 0.0] }], "dry_run": true }),
        );
        assert_eq!(
            denied.error.expect("denied").code,
            protocol::PERMISSION_DENIED,
            "authoring a path needs edit_scene even as a dry run"
        );
        let allowed = call(&mut app, "camera.raycast", json!({ "u": 0.5, "v": 0.5 }));
        assert!(
            allowed.error.is_none(),
            "raycast is read-only: {:?}",
            allowed.error
        );
    }

    /// `replay.*`, `moments.*` and `render.out_*` through the dispatcher: error classes before
    /// anything is loaded, the permission each method needs, and a headless replay session.
    #[test]
    fn replay_moments_and_render_out_dispatch_with_their_error_classes() {
        use regia_framework::replay_sim::{PilotInput, ReplayHeader, SyntheticReplay};

        static NEXT_NEVER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let never = std::env::temp_dir()
            .join(format!(
                "regia-agent-dispatch-never-{}-{}",
                std::process::id(),
                NEXT_NEVER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ))
            .to_string_lossy()
            .into_owned();
        let mut app = app();
        // Before a replay is loaded every read answers NOT_FOUND, never a panic.
        for (method, params) in [
            ("replay.seek", json!({ "tick": 1 })),
            ("replay.step", json!({})),
            ("replay.play", json!({ "speed": 0.5 })),
            ("replay.pause", json!({})),
            ("moments.list", json!({})),
            ("moments.get", json!({ "index": 0 })),
            ("render.out_progress", json!({})),
            ("render.out_cancel", json!({})),
        ] {
            let error = call(&mut app, method, params).error.expect(method);
            assert_eq!(error.code, protocol::NOT_FOUND, "{method}");
        }
        // Bad params are INVALID_PARAMS whatever the state.
        for (method, params) in [
            ("replay.load", json!({})),
            ("replay.play", json!({ "speed": 100 })),
            ("render.out_start", json!({ "frames": 3 })),
            (
                "render.out_start",
                json!({ "out": never.as_str(), "frames": 3, "time_map": [{ "frame": 0, "scale": -1 }] }),
            ),
            ("render.out_start", json!({ "out": never.as_str(), "frames": 0 })),
        ] {
            let error = call(&mut app, method, params).error.expect(method);
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{method}");
        }
        // A missing file is a structured replay error, and creates no player.
        let missing_dir = regia_test_utils::unique_temp_dir("agent-dispatch-replay-missing");
        let missing = call(
            &mut app,
            "replay.load",
            json!({ "path": missing_dir.join("none.do27replay").to_string_lossy() }),
        );
        assert!(missing.error.expect("missing").message.contains("[io]"));
        // A path outside every allowed root is refused before any read (audit-agent-sec Medium).
        for outside in ["/nonexistent/none.do27replay", "/etc/passwd", "../x.do27replay"] {
            let refused = call(&mut app, "replay.load", json!({ "path": outside }));
            assert_eq!(
                refused.error.expect(outside).code,
                protocol::INVALID_PARAMS,
                "{outside}"
            );
        }
        let _ = std::fs::remove_dir_all(&missing_dir);

        // A real (tiny) replay, loaded and walked through the dispatcher.
        let header = ReplayHeader {
            version: 3,
            tick_rate: 60,
            ..ReplayHeader::default()
        };
        let mut writer = SyntheticReplay::new(&header);
        for tick in 0..=5u32 {
            writer = writer.inputs(tick, &[(1, PilotInput::default())]);
        }
        let dir = std::env::temp_dir().join(format!("regia-agent-dispatch-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tiny.do27replay");
        std::fs::write(&path, writer.finish()).expect("write");
        let loaded = call(&mut app, "replay.load", json!({ "path": path.to_string_lossy() }));
        let load = loaded.result.expect("replay.load");
        assert_eq!(load["end_tick"], 5);
        let again = call(&mut app, "replay.load", json!({ "path": path.to_string_lossy() }));
        assert_eq!(again.result.expect("reload"), load, "loading twice is idempotent");
        let stepped = call(&mut app, "replay.step", json!({ "ticks": 3 }))
            .result
            .expect("step");
        assert_eq!(stepped["advanced"], 3);
        let sought = call(&mut app, "replay.seek", json!({ "tick": 99 }))
            .result
            .expect("seek");
        assert_eq!(sought["position"]["at_end"], true);
        let listed = call(&mut app, "moments.list", json!({})).result.expect("list");
        assert_eq!(listed["total"], 0);
        let get = call(&mut app, "moments.get", json!({ "index": 0 }))
            .error
            .expect("no moments");
        assert_eq!(get.code, protocol::NOT_FOUND);

        // Permissions: the reads stay open under a read-only profile, the mutations do not.
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                read_only: true,
                ..AgentConfig::default()
            },
            source: None,
        });
        assert!(call(&mut app, "moments.list", json!({})).error.is_none());
        assert!(
            call(&mut app, "moments.get", json!({ "index": 0 }))
                .error
                .is_some_and(|e| e.code == protocol::NOT_FOUND)
        );
        for (method, params) in [
            ("replay.load", json!({ "path": path.to_string_lossy() })),
            ("replay.seek", json!({ "tick": 1 })),
            ("replay.step", json!({})),
            ("replay.play", json!({})),
            ("replay.pause", json!({})),
            ("render.out_start", json!({ "out": never.as_str(), "frames": 3 })),
            ("render.out_cancel", json!({})),
        ] {
            let error = call(&mut app, method, params).error.expect(method);
            assert_eq!(error.code, protocol::PERMISSION_DENIED, "{method}");
        }
        let progress = call(&mut app, "render.out_progress", json!({}))
            .error
            .expect("no job");
        assert_eq!(
            progress.code,
            protocol::NOT_FOUND,
            "render.out_progress is read-only"
        );
        assert!(!std::path::Path::new(never.as_str()).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_rate_windows_commit_together_at_a_fixed_instant() {
        let start = Instant::now();
        let mut rate = RateLimit::default();
        assert!(rate.admit_at(Some(1), Some(1), start, || Ok(())).is_ok());
        assert_eq!(rate.heavy, VecDeque::from([start]));
        assert_eq!(rate.admitted, VecDeque::from([start]));
        let guard_called = std::cell::Cell::new(false);
        let error = rate
            .admit_at(Some(1), Some(1), start, || {
                guard_called.set(true);
                Ok(())
            })
            .expect_err("heavy budget is full");
        assert_eq!(error.message, "too many heavy calls");
        assert!(
            !guard_called.get(),
            "full windows are checked before the final guard"
        );
        rate.heavy.clear();
        let error = rate
            .admit_at(Some(1), Some(1), start, || Ok(()))
            .expect_err("mutating budget is full");
        assert_eq!(error.message, "too many mutating calls");
        assert!(rate.heavy.is_empty(), "a partial admission must not be committed");
        rate.admitted.clear();
        let error = rate
            .admit_at(Some(1), Some(1), start, || {
                Err(RpcError::invalid_params("guard refused"))
            })
            .expect_err("guard refuses");
        assert_eq!(error.message, "guard refused");
        assert!(rate.heavy.is_empty() && rate.admitted.is_empty());
        assert!(rate.admit_at(Some(1), Some(1), start, || Ok(())).is_ok());
        let next = start + Duration::from_secs(1);
        assert!(rate.admit_at(Some(1), Some(1), next, || Ok(())).is_ok());
        assert_eq!(rate.heavy, VecDeque::from([next]));
        assert_eq!(rate.admitted, VecDeque::from([next]));
    }

    #[test]
    fn rate_limit_window_resets() {
        let start = Instant::now();
        let mut limit = RateLimit::default();
        for _ in 0..50 {
            assert!(limit.admit_at(None, Some(50), start, || Ok(())).is_ok());
        }
        assert!(
            !limit.admit_at(None, Some(50), start, || Ok(())).is_ok(),
            "the 51st in one second is refused"
        );
        // A burst straddling a fixed-window boundary no longer doubles the rate.
        assert!(
            !limit
                .admit_at(None, Some(50), start + Duration::from_millis(900), || Ok(()))
                .is_ok(),
            "still inside the sliding second"
        );
        assert!(
            limit
                .admit_at(None, Some(50), start + Duration::from_millis(1000), || Ok(()))
                .is_ok(),
            "the window slid"
        );
    }

    /// Handlers must tolerate `params` that are not an object (a string, an array, a number, or
    /// `null`): they answer with a result or an error, never panic the game (M18 robustness).
    #[test]
    fn non_object_params_never_panic() {
        let mut app = app();
        for method in [
            "agent.hello",
            "scene.get_entity",
            "scene.set_property",
            "scene.spawn",
            "assets.load",
            "logs.read",
            "save.load",
        ] {
            for bad in [
                json!("not-an-object"),
                json!([1, 2, 3]),
                json!(42),
                json!(true),
                Value::Null,
            ] {
                let response = call(&mut app, method, bad.clone());
                assert!(
                    response.result.is_some() || response.error.is_some(),
                    "{method} with params {bad} answered neither result nor error"
                );
            }
        }
    }

    #[test]
    fn hello_reports_the_full_surface_and_capabilities() {
        let mut app = app();
        app.update();
        let response = call(&mut app, "agent.hello", json!({ "client": "claude" }));
        let hello = response.result.expect("hello");
        assert_eq!(hello["protocol"], PROTOCOL_VERSION);
        let methods = hello["methods"].as_array().expect("methods");
        assert_eq!(methods.len(), schema::METHODS.len(), "every schema method listed");
        assert!(
            hello["capabilities"].as_array().is_some_and(|c| !c.is_empty()),
            "capabilities present"
        );
        let capabilities: Vec<&str> = hello["capabilities"]
            .as_array()
            .expect("capabilities")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for marker in [
            "shot_fade_layer",
            "render_out_aov_id",
            "render_out_aov_depth",
            "render_out_aov_motion",
            "render_out_aov_material",
            "shot_rotation_keys",
        ] {
            assert!(capabilities.contains(&marker), "{marker} in {capabilities:?}");
        }
        assert!(!capabilities.contains(&"render_out_audio"), "no Audio resource");
        assert_eq!(hello["shots_ron_version"], json!(3));
        let result = schema::method("agent.hello").expect("schema").result;
        for key in hello.as_object().expect("object").keys() {
            assert!(result.contains(key.as_str()), "{key} missing from {result}");
        }
    }

    #[test]
    fn schema_lists_the_capability_markers() {
        let mut app = app();
        app.update();
        for params in [json!({}), json!({ "method": "render.out_start" })] {
            let table = call(&mut app, "agent.schema", params).result.expect("schema");
            let markers = table["capability_markers"]
                .as_array()
                .expect("capability_markers");
            assert!(
                markers
                    .iter()
                    .any(|m| m["name"] == "render_out_aov_id" && m["description"].is_string()),
                "{markers:?}"
            );
        }
    }

    /// The M20 gameplay methods (players, possess, saves) dispatch through the schema table:
    /// permission check, route and audit all come from `schema::METHODS` — no side channels.
    #[test]
    fn gameplay_methods_dispatch_through_the_schema_table() {
        let mut app = app();
        // The save subsystem with a temp directory (app() has no save plugin).
        let dir = std::env::temp_dir().join(format!("regia-agent-save-{}-schema", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        app.add_plugins(regia_framework::save_game::SavePlugin::default())
            .insert_resource(regia_framework::save_game::SaveDirectory(dir.clone()));

        // save.save answers and the slot appears in save.list_slots.
        let saved = call(&mut app, "save.save", json!({ "slot": "Schema" }));
        let result = saved.result.expect("save.save works");
        assert_eq!(result["slot"], "Schema");
        assert!(
            result["bytes_written"].as_u64().unwrap_or(0) > 12,
            "file size reported"
        );

        let slots = call(&mut app, "save.list_slots", json!({}));
        let list = slots.result.expect("save.list_slots works");
        assert_eq!(list["count"], 1, "the slot is listed: {list}");

        // save.load restores the slot (empty world: zero entities).
        let loaded = call(&mut app, "save.load", json!({ "slot": "Schema" }));
        let result = loaded.result.expect("save.load works");
        assert_eq!(result["entity_count"], 0);

        // players.list without the player plugin answers F0003 with the fix hint.
        let players = call(&mut app, "players.list", json!({}));
        let error = players.error.expect("no PlayerManager in this app");
        assert_eq!(error.code, protocol::NOT_FOUND);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0003")
        );
        assert!(
            error
                .data
                .as_ref()
                .and_then(|d| d["hint"].as_str())
                .is_some_and(|hint| hint.contains("PlayerPlugin")),
            "the hint names the missing plugin"
        );

        // scene.possess validates its parameters (F0002).
        let possess = call(&mut app, "scene.possess", json!({ "entity": 1 }));
        let error = possess.error.expect("possess needs the controller param");
        assert_eq!(error.code, protocol::INVALID_PARAMS);

        // Every gameplay method is declared in the schema table with its MCP tool.
        for (method, tool) in [
            ("players.list", "players_list"),
            ("players.get", "players_get"),
            ("scene.possess", "possess"),
            ("save.save", "save_game"),
            ("save.load", "load_game"),
            ("save.list_slots", "list_save_slots"),
            ("save.quick_save", "quick_save"),
            ("save.quick_load", "quick_load"),
            ("save.delete", "delete_save"),
            ("save.status", "save_status"),
            ("character.move_to", "character_move_to"),
            ("save.inspect", "inspect_save"),
        ] {
            assert_eq!(
                schema::method(method).map(|m| m.tool),
                Some(tool),
                "{method} is declared in the schema table"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_wait_for_the_end_of_a_demo_catch_up() {
        let mut app = app();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: request("2.0", Some(json!(1)), "agent.hello"),
                reply: tx,
            })
            .expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        app.world_mut()
            .insert_resource(regia_framework::demo_world::DemoCatchUp);
        process_requests(app.world_mut());
        assert!(rx.try_recv().is_err(), "not answered during a catch-up step");
        app.world_mut()
            .remove_resource::<regia_framework::demo_world::DemoCatchUp>();
        process_requests(app.world_mut());
        assert!(rx.try_recv().is_ok(), "answered right after it");
    }

    /// Like `call`, with the whole request under the test's control (envelope cases).
    fn send(app: &mut App, request: Request) -> Response {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx.send(Inbound { request, reply: tx }).expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        process_requests(app.world_mut());
        drop(inbox_tx);
        rx.recv_timeout(Duration::from_secs(2)).expect("answered")
    }

    fn request(jsonrpc: &str, id: Option<Value>, method: &str) -> Request {
        Request {
            jsonrpc: jsonrpc.into(),
            id,
            method: method.into(),
            params: Value::Null,
        }
    }

    fn regia_code(error: &RpcError) -> Option<&str> {
        error.data.as_ref().and_then(|d| d["code"].as_str())
    }

    #[test]
    fn bad_envelopes_answer_f0004_before_routing() {
        let mut app = app();
        let version = send(&mut app, request("1.0", Some(json!(3)), "agent.hello"));
        let error = version.error.expect("refused");
        assert_eq!(error.code, protocol::INVALID_REQUEST);
        assert_eq!(regia_code(&error), Some("F0004"));
        assert_eq!(version.id, json!(3), "a good id is still echoed");
        for id in [json!({ "a": 1 }), json!([1]), json!(true)] {
            let response = send(&mut app, request("2.0", Some(id.clone()), "agent.hello"));
            assert_eq!(response.id, Value::Null, "{id} is not echoed");
            let error = response.error.expect("refused");
            assert_eq!(regia_code(&error), Some("F0004"), "{id}");
        }
        let ok = send(&mut app, request("2.0", Some(json!("a-1")), "agent.hello"));
        assert_eq!(ok.id, json!("a-1"));
        assert!(ok.result.is_some(), "{ok:?}");
    }

    #[test]
    fn unknown_methods_suggest_the_closest_name_and_cut_long_names() {
        let mut app = app();
        let typo = call(&mut app, "scene.spwan", json!({})).error.expect("unknown");
        let data = typo.data.clone().unwrap_or_default();
        assert_eq!(data["suggestion"], "scene.spawn");
        assert!(
            data["hint"]
                .as_str()
                .is_some_and(|h| h.starts_with("did you mean scene.spawn?")),
            "{data}"
        );
        let far = call(&mut app, "zzzz.qqqq", json!({})).error.expect("unknown");
        assert!(far.data.unwrap_or_default().get("suggestion").is_none());
        let long = "é".repeat(10_000);
        let cut = call(&mut app, &long, json!({})).error.expect("unknown");
        assert!(
            cut.message.len() <= "unknown method ".len() + MAX_ECHOED_METHOD_BYTES,
            "echoed {} bytes",
            cut.message.len()
        );
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
        assert_eq!(truncate_on_char("éé", 3), "é", "never splits a character");
    }

    #[test]
    fn heavy_calls_have_their_own_budget() {
        let mut app = app();
        let budget = AgentConfig::default().heavy_rate_limit_per_second as usize;
        app.world_mut().resource_mut::<RateLimit>().heavy =
            std::iter::repeat_n(Instant::now(), budget).collect();
        let error = call(&mut app, "scene.serialize", json!({}))
            .error
            .expect("limited");
        assert_eq!(error.code, protocol::RATE_LIMITED);
        assert_eq!(regia_code(&error), Some("F0007"));
        assert!(error.message.contains("heavy"), "{}", error.message);
        assert!(
            call(&mut app, "agent.hello", json!({})).error.is_none(),
            "light reads are never limited"
        );

        let start = Instant::now();
        let mut limit = RateLimit::default();
        for _ in 0..20 {
            assert!(limit.admit_at(Some(20), None, start, || Ok(())).is_ok());
        }
        assert!(
            !limit.admit_at(Some(20), None, start, || Ok(())).is_ok(),
            "the 21st heavy call in one second is refused"
        );
        assert!(
            limit.admit_at(None, Some(50), start, || Ok(())).is_ok(),
            "the mutating window is separate"
        );
        assert!(
            limit
                .admit_at(Some(20), None, start + Duration::from_secs(1), || Ok(()))
                .is_ok(),
            "the window slid"
        );
    }

    #[test]
    fn the_params_gate_runs_before_any_handler() {
        let mut app = app();
        for bad in [json!([1, 2]), json!("x"), json!(3), json!(false)] {
            let error = call(&mut app, "scene.spawn", bad.clone()).error.expect("refused");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let error = call(&mut app, "play.step", json!({ "frames": 0 }))
            .error
            .expect("refused");
        assert_eq!(error.data.unwrap_or_default()["limit"]["param"], "frames");
        assert!(
            app.world()
                .resource::<crate::playtest::AgentTimeControl>()
                .stepping
                .is_none(),
            "the refused step never started"
        );
        let before = call(&mut app, "loop.get", json!({})).result.expect("loop");
        let error = call(&mut app, "loop.set", json!({ "max_fps": 1e-20 }))
            .error
            .expect("refused");
        assert_eq!(
            error.code,
            protocol::INVALID_PARAMS,
            "a frame time past Duration's range"
        );
        let error = call(&mut app, "loop.set", json!({ "max_substeps": 65 }))
            .error
            .expect("refused");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert_eq!(error.data.unwrap_or_default()["limit"]["param"], "max_substeps");
        let error = call(&mut app, "loop.set", json!({ "throttled_fps": 0.5 }))
            .error
            .expect("refused");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        let error = call(&mut app, "loop.set", json!({ "fixed_hz": 1.0e9 + 1.0 }))
            .error
            .expect("refused");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert_eq!(
            call(&mut app, "loop.get", json!({})).result.expect("unchanged"),
            before,
            "a refused loop.set leaves the previous settings"
        );
        let stored = call(
            &mut app,
            "loop.set",
            json!({ "max_substeps": 64, "max_fps": 10000, "throttled_fps": 1000, "fixed_hz": 20000 }),
        )
        .result
        .expect("endpoints stored");
        assert_eq!(stored["after"]["max_substeps"], 64);
        assert_eq!(stored["after"]["max_fps"], 10000.0);
        assert_eq!(stored["after"]["throttled_fps"], 1000.0);
        assert_eq!(stored["after"]["fixed_hz"], 20000.0);
        let error = call(&mut app, "world.pick", json!({ "origin": [0.0, 1e8, 0.0] }))
            .error
            .expect("refused");
        assert_eq!(
            error.data.unwrap_or_default()["limit"]["param"],
            "origin",
            "nested numbers are checked"
        );
    }

    /// A request whose connection already answered F0006 (the world did not drain it in time) is
    /// not run when the world drains it later: the caller may have retried, and a late
    /// `scene.spawn` would spawn twice (audit-concurrency-1103 agent H1). The audit log records it
    /// as `expired`.
    #[test]
    fn a_request_answered_f0006_is_not_run_when_drained_later() {
        let dir = regia_test_utils::unique_temp_dir("agent-expired-request");
        let audit_path = dir.join("audit.jsonl");
        let mut app = app();
        app.world_mut()
            .insert_resource(AuditLog::open(audit_path.clone()));
        let server = crate::server::start("secret".into()).expect("server starts");
        app.world_mut().insert_resource(AgentInbox(server.inbox.clone()));
        let url = format!("ws://{}", server.addr);
        let mut client = crate::client::Client::connect(&url, "secret").expect("connects");
        // The world does not pump: the connection answers F0006 after `timeout_ms` + the grace.
        let error = client
            .call("agent.hello", json!({ "timeout_ms": 0 }))
            .expect_err("the connection timed out");
        match error {
            crate::client::ClientError::Rpc(error) => {
                assert_eq!(regia_code(&error), Some("F0006"), "{error:?}");
            }
            other => panic!("expected F0006, got {other:?}"),
        }
        // Now the world drains the queued request.
        process_requests(app.world_mut());
        let log = std::fs::read_to_string(&audit_path).unwrap_or_default();
        let lines: Vec<Value> = log
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|line: &Value| line["method"] == "agent.hello")
            .collect();
        assert_eq!(lines.len(), 1, "the drained request is audited once: {log}");
        assert_eq!(
            lines[0]["outcome"], "expired",
            "a request its caller already gave up on is not run: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `agent.changes` through the dispatcher: `limit` pages the log and `next` resumes it.
    #[test]
    fn agent_changes_pages_through_the_dispatcher() {
        let mut app = app();
        let mut log = ChangeLog::default();
        let session = AgentSession::default();
        for i in 0..5 {
            log.push(i, "scene.spawn", format!("entity {i}"), Value::Null, &session);
        }
        app.insert_resource(log);
        let page = call(&mut app, "agent.changes", json!({ "limit": 2 }))
            .result
            .expect("a page");
        let seqs: Vec<u64> = page["records"]
            .as_array()
            .expect("records")
            .iter()
            .filter_map(|r| r["seq"].as_u64())
            .collect();
        assert_eq!(seqs, vec![1, 2]);
        assert_eq!(page["next"], json!(2));
        let rest = call(&mut app, "agent.changes", json!({ "since": 2, "limit": 10 }))
            .result
            .expect("a page");
        assert_eq!(rest["records"].as_array().map(Vec::len), Some(3));
        assert_eq!(rest["next"], json!(5));
        let refused = call(&mut app, "agent.changes", json!({ "limit": -1 }));
        assert_eq!(
            refused.error.map(|e| e.code),
            Some(protocol::INVALID_PARAMS),
            "a negative limit is refused"
        );
    }

    /// A cheap read-only request (`errors.explain`) answering on `reply`.
    fn explain(id: u64, reply: &Sender<Response>) -> Inbound {
        Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(id)),
                method: "errors.explain".into(),
                params: json!({ "code": "F0012" }),
            },
            reply: reply.clone(),
        }
    }

    /// sprint-deep-zai-agent-protocol finding 1: a WebSocket client that keeps a full frame's
    /// worth of requests queued every frame must not stop the local transport. The local request
    /// is answered within two frames, and a frame still handles at most `MAX_PER_FRAME`.
    #[test]
    fn a_full_websocket_inbox_does_not_starve_the_local_transport() {
        let mut app = app();
        let (ws_tx, ws_rx) = crossbeam_channel::unbounded();
        let (local_tx, local_rx) = crossbeam_channel::unbounded();
        let (ws_reply, ws_answers) = crossbeam_channel::unbounded();
        let (local_reply, local_answers) = crossbeam_channel::unbounded();
        local_tx.send(explain(9_000, &local_reply)).expect("queued");
        let mut next_id = 0;
        let mut answered_in = None;
        for frame in 0..2 {
            // Top the WebSocket inbox up to more than one frame drains.
            while ws_rx.len() < MAX_PER_FRAME + 8 {
                ws_tx.send(explain(next_id, &ws_reply)).expect("queued");
                next_id += 1;
            }
            drain_inboxes(app.world_mut(), &ws_rx, Some(&local_rx));
            let ws_handled = ws_answers.try_iter().count();
            let local_handled = local_answers.try_iter().count();
            assert!(
                ws_handled + local_handled <= MAX_PER_FRAME,
                "frame {frame} handled {ws_handled} + {local_handled} requests"
            );
            assert!(ws_handled > 0, "frame {frame} still serves the WebSocket inbox");
            if local_handled > 0 && answered_in.is_none() {
                answered_in = Some(frame);
            }
        }
        assert!(
            answered_in.is_some(),
            "the local request was not answered within 2 frames while the WebSocket inbox stayed full"
        );
    }

    /// The ADR-4894 rule holds under local load: board-answered skips in the WebSocket inbox
    /// cost no budget and do not hand the turn to the local inbox, so the WebSocket request
    /// behind more than a frame's worth of them is answered in the first frame.
    #[test]
    fn local_load_does_not_push_a_request_behind_websocket_skips_out_of_the_frame() {
        let mut app = app();
        let (ws_tx, ws_rx) = crossbeam_channel::unbounded();
        let (local_tx, local_rx) = crossbeam_channel::unbounded();
        for i in 0..(4 * MAX_PER_FRAME) as u64 {
            let (reply, _rx) = crossbeam_channel::bounded(1);
            crate::server::abandon_board_for_test(reply.clone());
            ws_tx.send(explain(i, &reply)).expect("queued");
        }
        let (ws_reply, ws_answers) = crossbeam_channel::unbounded();
        ws_tx.send(explain(5_000, &ws_reply)).expect("queued");
        let (local_reply, local_answers) = crossbeam_channel::unbounded();
        for i in 0..(2 * MAX_PER_FRAME) as u64 {
            local_tx.send(explain(10_000 + i, &local_reply)).expect("queued");
        }
        drain_inboxes(app.world_mut(), &ws_rx, Some(&local_rx));
        assert_eq!(
            ws_answers.try_iter().count(),
            1,
            "the request behind the skips is answered"
        );
        let local_handled = local_answers.try_iter().count();
        assert!(
            (1..MAX_PER_FRAME).contains(&local_handled),
            "the local inbox shares the frame: {local_handled}"
        );
    }

    #[test]
    fn telemetry_live_schema_advertises_stall_diagnostics() {
        let schema = crate::schema::method("telemetry.live").expect("live method schema");
        for field in [
            "timing",
            "cpu_sample_ms",
            "gpu_sample_ms",
            "phases_frame",
            "event_wait_ms",
            "runner",
            "event_wait_share",
        ] {
            assert!(schema.result.contains(field), "agent schema omits {field}");
        }
    }

    /// ADR-7965: actual dispatch exposes renderer freshness and the runner's event waits.
    #[test]
    fn telemetry_live_reports_stale_reasons_and_runner_waits() {
        let mut app = app();
        let mut real = regia_app::RealTime::default();
        real.advance_by(Duration::from_millis(100));
        let runner = regia_app::RunnerState {
            stalled: true,
            surface_refused: true,
            throttle_ms: 100.0,
            ..Default::default()
        };
        let mut phases = regia_app::FramePhases {
            frame: 1,
            runner: Some(runner),
            ..Default::default()
        };
        phases.ms[regia_app::Phase::EventWait as usize] = 75.0;
        phases.ms[regia_app::Phase::Update as usize] = 25.0;
        let mut telemetry = regia_telemetry::Telemetry::new(Default::default());
        telemetry.request_summary("agent_stall_regression");
        app.insert_resource(telemetry)
            .insert_resource(real)
            .insert_resource(runner)
            .insert_resource(phases)
            .add_systems(regia_app::Last, regia_telemetry::telemetry_frame);
        app.run_schedule(regia_app::Last);
        let result = call(&mut app, "telemetry.live", json!({}))
            .result
            .expect("live response");
        assert_eq!(result["thresholds"]["event_wait_share"], 0.5);
        let live = &result["live"];
        assert_eq!(live["bound"], "unknown");
        assert_eq!(live["bound_reason"], "hidden");
        assert_eq!(live["timing"]["cpu_absent"], "hidden");
        assert_eq!(live["timing"]["gpu_absent"], "hidden");
        assert_eq!(live["cpu_sample_ms"], Value::Null);
        assert_eq!(live["gpu_sample_ms"], Value::Null);
        assert_eq!(live["event_wait_ms"], 75.0);
        assert_eq!(live["phases_frame"], 1);
        assert_eq!(
            live["runner"],
            json!({ "focused": false, "occluded": false, "stalled": true, "surface_refused": true, "throttle_ms": 100.0 })
        );
        let result = call(&mut app, "telemetry.get_last_summary", json!({}))
            .result
            .expect("summary response");
        let summary = &result["summary"];
        assert_eq!(summary["cpu_ms"]["samples"], 0, "{result}");
        assert_eq!(summary["frame_bound"]["class"], "unknown");
        assert_eq!(summary["frame_breakdown_ms"]["event_wait"], 75.0);

        // A visible tick with no extracted camera names the missing scene, not GPU load.
        app.insert_resource(regia_app::RunnerState {
            focused: true,
            ..Default::default()
        });
        app.insert_resource(regia_render::ExtractedFrame::default());
        app.world_mut()
            .resource_mut::<regia_app::RealTime>()
            .advance_by(Duration::from_millis(100));
        app.run_schedule(regia_app::Last);
        let result = call(&mut app, "telemetry.live", json!({}))
            .result
            .expect("no-scene response");
        assert_eq!(result["live"]["timing"]["cpu_absent"], "no_scene");
        assert_eq!(result["live"]["bound_reason"], "no_scene");
        assert_eq!(result["live"]["runner"]["focused"], true);
    }

    #[test]
    fn telemetry_live_reports_the_hitch_floor_and_bound_reason() {
        let mut app = app();
        app.insert_resource(regia_telemetry::Telemetry::new(
            regia_telemetry::TelemetryOptions::default(),
        ));
        let response = call(&mut app, "telemetry.live", json!({}));
        let result = response.result.expect("telemetry.live answered");
        assert_eq!(result["thresholds"]["hitch_floor_ms"], json!(8.0), "{result}");
        assert_eq!(result["thresholds"]["pacing_period_ms"], json!(0.0), "{result}");
        assert_eq!(result["thresholds"]["present_wait_share"], json!(0.5), "{result}");
        assert_eq!(result["live"]["bound"], "unknown", "{result}");
        assert!(result["live"].get("bound_reason").is_some(), "{result}");
        app.world_mut()
            .resource_mut::<regia_telemetry::Telemetry>()
            .set_pacing_period_ms(Some(20.0));
        let result = call(&mut app, "telemetry.live", json!({}))
            .result
            .expect("telemetry.live answered");
        assert_eq!(
            result["thresholds"]["hitch_floor_ms"],
            json!(30.0),
            "1.5 periods: {result}"
        );
        let counts = call(&mut app, "telemetry.get_last_summary", json!({}))
            .result
            .expect("telemetry.get_last_summary answered");
        assert_eq!(counts["counts"]["present_waits"], json!(0), "{counts}");
    }

    /// ADR-8417: a session at `TelemetryLevel::Ship` reports `level: "ship"` through
    /// `telemetry.get_last_summary` (no header yet: `session` is null).
    #[test]
    fn telemetry_get_last_summary_reports_the_ship_level() {
        let mut app = app();
        app.insert_resource(regia_telemetry::Telemetry::new(
            regia_telemetry::TelemetryOptions {
                level: regia_telemetry::TelemetryLevel::Ship,
                ..regia_telemetry::TelemetryOptions::default()
            },
        ));
        let result = call(&mut app, "telemetry.get_last_summary", json!({ "session": true }))
            .result
            .expect("telemetry.get_last_summary answered");
        assert_eq!(result["level"], "ship", "{result}");
        assert_eq!(result["session"], Value::Null, "{result}");
    }
}

#[cfg(test)]
mod hosted_dispatch_tests {
    use super::*;
    use crate::extension::AgentExtension;
    use crate::schema::MethodSchema;

    #[derive(Resource, Default)]
    struct Writes(u32);

    fn host() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<Writes>();
        app.insert_resource(AgentSession {
            client: "owner".into(),
            model: "test".into(),
            session: "master".into(),
        });
        let mut extension = AgentExtension::new();
        extension.register(
            MethodSchema {
                name: "test.hosted_write",
                tool: "",
                description: "Test mutation",
                params: &[],
                result: "{}",
                mutating: true,
                permission: Permission::EditScene,
            },
            |world, _| {
                world.resource_mut::<Writes>().0 += 1;
                Ok(json!({"applied":true}))
            },
        );
        app.insert_resource(extension);
        app
    }

    fn inbound() -> (crate::scope::ScopedInbound, crossbeam_channel::Receiver<Response>) {
        let (reply, answer) = crossbeam_channel::bounded(1);
        (
            crate::scope::ScopedInbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(7)),
                    method: "test.hosted_write".into(),
                    params: json!({}),
                },
                reply,
                scope: std::sync::Arc::new(crate::scope::ToolScope::new(
                    "hosted-test",
                    [Permission::Read],
                    vec!["*".into()],
                )),
            },
            answer,
        )
    }

    #[test]
    fn hosted_dispatch_guard_and_project_permissions_both_apply() {
        let mut app = host();
        let (request, answer) = inbound();
        process_hosted_request(app.world_mut(), request, None, |_| {
            Err(RpcError::new(
                protocol::PERMISSION_DENIED,
                "F0012",
                "Ask only",
                "Read class only",
            ))
        });
        assert_eq!(
            answer
                .try_recv()
                .expect("answer")
                .error
                .expect("denied")
                .data
                .expect("data")["code"],
            "F0012"
        );
        assert_eq!(app.world().resource::<Writes>().0, 0);
        assert_eq!(app.world().resource::<AgentSession>().session, "master");
        assert!(crate::scope::current_caller(app.world()).is_none());
        let mut permissions = crate::AgentPermissions::default();
        permissions.config.read_only = true;
        app.insert_resource(permissions);
        let (request, answer) = inbound();
        process_hosted_request(app.world_mut(), request, None, |_| Ok(()));
        assert_eq!(
            answer
                .try_recv()
                .expect("answer")
                .error
                .expect("project denies")
                .data
                .expect("data")["code"],
            "F0012"
        );
        assert_eq!(app.world().resource::<Writes>().0, 0);
        app.world_mut()
            .resource_mut::<crate::AgentPermissions>()
            .config
            .read_only = false;
        let (request, answer) = inbound();
        process_hosted_request(app.world_mut(), request, None, |_| Ok(()));
        assert!(answer.try_recv().expect("answer").result.is_some());
        assert_eq!(app.world().resource::<Writes>().0, 1);
        assert_eq!(app.world().resource::<AgentSession>().client, "owner");
        assert!(crate::scope::current_caller(app.world()).is_none());
    }

    #[test]
    fn hosted_dispatch_does_not_run_abandoned_requests() {
        let mut app = host();
        let (request, _) = inbound();
        crate::server::abandon(request.reply.clone());
        process_hosted_request(app.world_mut(), request, None, |_| Ok(()));
        assert_eq!(app.world().resource::<Writes>().0, 0);
        assert_eq!(app.world().resource::<AgentSession>().client, "owner");
    }

    #[test]
    fn hosted_dispatch_act_retains_explicit_project_review_permission() {
        let root = regia_test_utils::unique_temp_dir("hosted-review-permission");
        std::fs::create_dir_all(&root).expect("project");
        let mut app = host();
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let mut permissions = crate::AgentPermissions::default();
        permissions.config.permissions.insert("review".into());
        app.insert_resource(permissions);
        let proposal = crate::review::propose(
            app.world_mut(),
            &json!({"reason":"Owner's proposal", "changes":[{"path":"note.txt", "text":"review me"}]}),
        )
        .expect("proposal");
        let (mut request, answer) = inbound();
        request.request.method = "review.reject".into();
        request.request.params = json!({"set":proposal["set"]});
        process_hosted_request(app.world_mut(), request, None, |_| Ok(()));
        let response = answer.try_recv().expect("reply");
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response.result.expect("result")["decided"],
            1,
            "Act uses the project's explicit Review grant, without a tool-only restriction"
        );
        drop(app);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn hosted_dispatch_narrowing_and_rate_budget_do_not_change_the_owner() {
        let mut app = host();
        let permissions = crate::AgentPermissions::default();
        let limit = permissions.config.rate_limit_per_second;
        app.insert_resource(permissions);
        let mut master_rate = RateLimit::default();
        master_rate.fill_for_test(limit as usize);
        app.insert_resource(master_rate);
        let (request, answer) = inbound();
        process_hosted_request(app.world_mut(), request, Some(&[Permission::Read]), |_| Ok(()));
        assert!(answer.try_recv().expect("refused").error.is_some());
        assert!(
            app.world()
                .resource::<crate::AgentPermissions>()
                .allows(Permission::EditScene),
            "owner profile restored after Ask"
        );
        let (request, answer) = inbound();
        process_hosted_request(app.world_mut(), request, None, |_| Ok(()));
        assert!(
            answer
                .try_recv()
                .expect("hosted budget is separate")
                .result
                .is_some()
        );
        assert_eq!(app.world().resource::<RateLimit>().admitted.len(), limit as usize);
        assert_eq!(app.world().resource::<Writes>().0, 1);
    }
}

#[cfg(test)]
mod logs_perf_gate_dispatch_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn logs_perf_gate_p99_failure_is_a_result() {
        let mut frames = Vec::new();
        for _ in 0..290 {
            frames.push(json!({"frame_ms": 16.0}));
        }
        for _ in 0..10 {
            frames.push(json!({"frame_ms": 60.0}));
        }
        let log = serde_json::to_string(&json!({
            "target": "regia::summary",
            "phase": "play",
            "map": "map_a",
            "res_class": "1440p",
            "tier": "HIGH",
            "frames": frames,
        }))
        .expect("log");
        let mut app = App::new();
        let reply = call_local(
            app.world_mut(),
            "logs.perf_gate",
            json!({
                "logs": [log],
                "thresholds": "[classes.\"1440p\".HIGH.default]\nfps_min = 60.0\nframe_ms_p50_max = 16.7\nframe_ms_p95_max = 25.0\nframe_ms_p99_max = 33.0\n",
            }),
            None,
        );
        match reply {
            LocalReply::Done(Ok(value)) => {
                assert_eq!(value["overall"], "FAIL");
                let failed = value["results"]
                    .as_array()
                    .expect("results")
                    .iter()
                    .any(|row| row["metric"] == "frame p99" && row["pass"] == false);
                assert!(failed, "{value}");
            }
            other => panic!("unexpected perf-gate reply: {other:?}"),
        }
    }
}

#[cfg(test)]
mod logs_triage_dispatch_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn logs_triage_dispatches_without_filesystem_or_renderer() {
        let mut app = App::new();
        let reply = call_local(
            app.world_mut(),
            "logs.triage",
            json!({
                "bundles":[{"name":"local","files":{"test.jsonl":"{\"t_ms\":0,\"target\":\"app::start\"}\n"}}]
            }),
            None,
        );
        match reply {
            LocalReply::Done(Ok(value)) => assert_eq!(value["sessions"][0]["records"], 1),
            other => panic!("unexpected triage reply: {other:?}"),
        }
    }
}
