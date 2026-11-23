// SPDX-License-Identifier: MPL-2.0
//! Editor-side extension of the protocol's method table (`specs/23` § 2).
//!
//! The editor surface (selection, gizmos, undo/redo — `specs/19`) lives in `regia_editor`,
//! which cannot be a dependency of this runtime-safe crate. Instead, the editor registers an
//! [`AgentExtension`] resource with the app: schema rows in the same shape as
//! [`crate::schema::METHODS`] plus plain handler functions. Dispatch consults the extension
//! after the built-in table, so extension methods get the same permission checks, rate
//! limiting, audit log and provenance records as native ones — no side channels.

use crossbeam_channel::Sender;
use serde_json::Value;

use regia_app::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;

use crate::protocol::{Response, RpcError};
use crate::schema::MethodSchema;

/// One editor method: its schema row and its handler.
pub type AgentHandler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// A method that answers later (ADR-1523): it keeps the [`DeferredReply`] and sends the answer
/// from a later frame. An `Err` answers at once, like a synchronous handler's.
pub type AgentDeferredHandler = fn(&mut World, &Value, DeferredReply) -> Result<(), RpcError>;

/// The answer channel of one deferred call. Dropping it unanswered leaves the caller to its
/// timeout (`F0006`), so a handler answers every reply it keeps.
#[derive(Debug, Clone)]
pub struct DeferredReply {
    id: Value,
    reply: Sender<Response>,
}

impl DeferredReply {
    /// The reply for request `id` on `reply` (dispatch builds these; tests build them too).
    #[must_use]
    pub fn new(id: Value, reply: Sender<Response>) -> Self {
        Self { id, reply }
    }

    /// The request id this reply answers.
    #[must_use]
    pub fn id(&self) -> &Value {
        &self.id
    }

    /// Sends the answer. Returns `false` when the caller has gone (its connection closed or
    /// it timed out).
    pub fn send(self, result: Result<Value, RpcError>) -> bool {
        let response = match result {
            Ok(value) => Response::ok(self.id, value),
            Err(error) => Response::err(self.id, error),
        };
        self.reply.send(response).is_ok()
    }
}

/// Methods added by the host app (the editor). Inserted as a resource before the first frame.
#[derive(Resource, Clone, Default)]
pub struct AgentExtension {
    entries: Vec<(MethodSchema, AgentHandler)>,
    deferred: Vec<(MethodSchema, AgentDeferredHandler)>,
    capabilities: Vec<(&'static str, &'static str)>,
    param_shapes: Vec<(&'static str, &'static str, Value)>,
}

impl AgentExtension {
    /// An empty extension.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a method and its handler. Later registrations for the same name win.
    pub fn register(&mut self, method: MethodSchema, handler: AgentHandler) {
        self.deferred.retain(|(m, _)| m.name != method.name);
        self.entries.retain(|(m, _)| m.name != method.name);
        self.entries.push((method, handler));
    }

    /// Registers a method that answers later (ADR-1523). Later registrations for the same name
    /// win, synchronous or deferred.
    pub fn register_deferred(&mut self, method: MethodSchema, handler: AgentDeferredHandler) {
        self.entries.retain(|(m, _)| m.name != method.name);
        self.deferred.retain(|(m, _)| m.name != method.name);
        self.deferred.push((method, handler));
    }

    /// Registers the JSON Schema of one param of an extension method (ADR-1582), for params whose
    /// `ParamSchema::kind` cannot say enough (an array of tagged items, an enum). `agent.schema`
    /// shows `shape` in place of the bare `{ "type": kind }`. Registering the same
    /// method and param twice keeps the later shape.
    pub fn register_param_shape(&mut self, method: &'static str, param: &'static str, shape: Value) {
        self.param_shapes
            .retain(|(m, p, _)| !(*m == method && *p == param));
        self.param_shapes.push((method, param, shape));
    }

    /// The registered shape of `method`'s `param`, when there is one.
    #[must_use]
    pub fn param_shape(&self, method: &str, param: &str) -> Option<&Value> {
        self.param_shapes
            .iter()
            .find(|(m, p, _)| *m == method && *p == param)
            .map(|(_, _, shape)| shape)
    }

    /// Every extension method as `agent.schema` lists it: its schema row with the registered
    /// param shapes, synchronous methods first, then the deferred ones. A method that takes over a
    /// built-in row (`dcc_feed.bake`) is listed once, by that row.
    #[must_use]
    pub fn schema_entries(&self) -> Vec<Value> {
        self.entries
            .iter()
            .map(|(m, _)| m)
            .chain(self.deferred.iter().map(|(m, _)| m))
            .filter(|m| crate::schema::method(m.name).is_none())
            .map(|m| m.to_json_with(&|param| self.param_shape(m.name, param).cloned()))
            .collect()
    }

    /// Registers a capability marker (`name`, one-line description) that `agent.hello` lists in
    /// `capabilities` and `agent.schema` lists in `capability_markers` (ADR-1559). Registering a
    /// name twice keeps the later description.
    pub fn add_capability(&mut self, name: &'static str, description: &'static str) {
        self.capabilities.retain(|(n, _)| *n != name);
        self.capabilities.push((name, description));
    }

    /// The registered capability markers, `(name, description)`, in registration order.
    #[must_use]
    pub fn capabilities(&self) -> &[(&'static str, &'static str)] {
        &self.capabilities
    }

    /// The names of every registered method that has no built-in row: synchronous ones in
    /// registration order, then the deferred ones.
    #[must_use]
    pub fn method_names(&self) -> Vec<&'static str> {
        self.entries
            .iter()
            .map(|(m, _)| m.name)
            .chain(self.deferred.iter().map(|(m, _)| m.name))
            .filter(|name| crate::schema::method(name).is_none())
            .collect()
    }

    /// The schema row of `name`, synchronous or deferred.
    #[must_use]
    pub fn method_schema(&self, name: &str) -> Option<&MethodSchema> {
        self.entries
            .iter()
            .map(|(m, _)| m)
            .chain(self.deferred.iter().map(|(m, _)| m))
            .find(|m| m.name == name)
    }

    /// The handler of the deferred method `name`, when the extension provides it.
    #[must_use]
    pub fn deferred_method(&self, name: &str) -> Option<AgentDeferredHandler> {
        self.deferred
            .iter()
            .find(|(m, _)| m.name == name)
            .map(|(_, h)| *h)
    }

    /// The schema row and handler of `name`, when the extension provides it.
    #[must_use]
    pub fn method(&self, name: &str) -> Option<(&MethodSchema, AgentHandler)> {
        self.entries
            .iter()
            .find(|(m, _)| m.name == name)
            .map(|(m, h)| (m, *h))
    }

    /// Number of registered methods (test hook).
    #[cfg(test)]
    #[must_use]
    pub fn entries_len_for_test(&self) -> usize {
        self.entries.len()
    }
}

/// Reads the world's type registry (convenience for extension handlers that edit reflected
/// data — the same seam `scene.*` handlers use).
#[must_use]
pub fn type_registry(world: &World) -> AppTypeRegistry {
    world.resource::<AppTypeRegistry>().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(name: &'static str) -> MethodSchema {
        MethodSchema {
            name,
            tool: "extension_tool",
            description: "A test method.",
            params: &[],
            result: "{}",
            mutating: true,
            permission: crate::schema::Permission::EditScene,
        }
    }

    fn handler(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
        let count = world.entities().len();
        Ok(json!({ "entities": count }))
    }

    #[test]
    fn registers_and_look_up_methods() {
        let mut extension = AgentExtension::new();
        assert!(extension.method("editor.test").is_none());
        extension.register(schema("editor.test"), handler);
        let (method, handler) = extension.method("editor.test").expect("registered");
        assert_eq!(method.name, "editor.test");
        assert_eq!(method.permission.name(), "edit_scene");

        // The handler answers against a world (internal entities exist; just verify shape).
        let mut world = World::new();
        let result = handler(&mut world, &json!({})).expect("runs");
        assert!(result["entities"].as_u64().is_some());
    }

    fn deferred(_world: &mut World, params: &Value, reply: DeferredReply) -> Result<(), RpcError> {
        if params.get("fail").is_some() {
            return Err(RpcError::invalid_params("fail"));
        }
        let _ = reply.send(Ok(json!({ "later": true })));
        Ok(())
    }

    /// Deferred methods are listed with the synchronous ones, and one name is either kind,
    /// never both (the later registration wins).
    #[test]
    fn deferred_methods_are_listed_and_replace_by_name() {
        let mut extension = AgentExtension::new();
        extension.register(schema("editor.a"), handler);
        extension.register_deferred(schema("editor.b"), deferred);
        assert_eq!(extension.method_names(), vec!["editor.a", "editor.b"]);
        assert!(extension.method_schema("editor.b").is_some());
        assert!(extension.method("editor.b").is_none(), "not a synchronous method");
        assert!(extension.deferred_method("editor.b").is_some());
        extension.register_deferred(schema("editor.a"), deferred);
        assert!(extension.method("editor.a").is_none());
        assert!(extension.deferred_method("editor.a").is_some());
        extension.register(schema("editor.a"), handler);
        assert!(extension.deferred_method("editor.a").is_none());
        assert_eq!(extension.method_names().len(), 2);
    }

    #[test]
    fn a_deferred_reply_answers_its_request_id() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let reply = DeferredReply::new(json!(7), tx);
        assert_eq!(reply.id(), &json!(7));
        let mut world = World::new();
        deferred(&mut world, &json!({}), reply).expect("accepted");
        let response = rx.try_recv().expect("answered");
        assert_eq!(response.id, json!(7));
        assert_eq!(response.result, Some(json!({ "later": true })));
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(rx);
        assert!(
            !DeferredReply::new(json!(1), tx).send(Ok(json!({}))),
            "caller gone"
        );
    }

    #[test]
    fn later_registrations_replace_earlier_ones() {
        let mut extension = AgentExtension::new();
        extension.register(schema("editor.test"), handler);
        extension.register(schema("editor.test"), handler);
        assert_eq!(extension.entries_len_for_test(), 1);
    }
}
