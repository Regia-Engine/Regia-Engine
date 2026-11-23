// SPDX-License-Identifier: MPL-2.0
//! Scoped callers (ADR-1523): out-of-process tools reach the protocol with a token of their own,
//! bound to a [`ToolScope`] (which methods, which permissions), on a second loopback listener
//! ([`crate::server::start_scoped`]). The master token (ADR-014) is never handed to a tool.
//!
//! The host (the editor's tool launcher) mints a token per launch with [`ToolTokens::issue`] and
//! revokes it when the tool exits ([`ToolTokens::revoke`]). Dispatch checks every scoped request
//! against its scope before the usual permission profile, contract and rate limits, and exposes
//! the caller to handlers as the [`AgentCaller`] resource for the length of the call.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crossbeam_channel::{Receiver, Sender};
use regia_app::prelude::*;

use crate::protocol::{self, Request, Response, RpcError};
use crate::schema::Permission;

/// What one scoped token may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolScope {
    /// Unique per issued token (a relaunch of the same tool gets a new id).
    pub id: u64,
    /// The tool the token belongs to (its manifest id).
    pub tool: String,
    /// Granted permission names (`read`, `edit_scene`, `play`, `assets`); `tools` and `build`
    /// are never accepted. The project's `.regia/agent.toml` still applies on top.
    pub permissions: BTreeSet<String>,
    /// Method patterns the token may call: an exact name, a prefix ending in `*`
    /// (`scene.*`, `render.out_*`), or `*` for every method.
    pub methods: Vec<String>,
}

impl ToolScope {
    /// A scope for `tool` with these permissions and method patterns. The `tools` and `build`
    /// permissions are dropped: a tool can never start or drive other tools, or change which
    /// native code loads, through its own token.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        permissions: impl IntoIterator<Item = Permission>,
        methods: Vec<String>,
    ) -> Self {
        Self {
            id: 0,
            tool: tool.into(),
            permissions: permissions
                .into_iter()
                .filter(|permission| {
                    !matches!(
                        permission,
                        Permission::Tools | Permission::Build | Permission::Review
                    )
                })
                .map(|permission| permission.name().to_string())
                .collect(),
            methods,
        }
    }

    /// Whether `method` matches one of the scope's patterns.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.methods.iter().any(|pattern| method_matches(pattern, method))
    }

    /// Whether the scope grants `permission` (`tools`, `build` and `review` never).
    #[must_use]
    pub fn allows_permission(&self, permission: Permission) -> bool {
        !matches!(
            permission,
            Permission::Tools | Permission::Build | Permission::Review
        ) && self.permissions.contains(permission.name())
    }

    /// The client name audit and provenance records use: `tool:<id>`.
    #[must_use]
    pub fn client_name(&self) -> String {
        format!("tool:{}", self.tool)
    }

    /// The stable refusal for a method or permission outside the scope.
    #[must_use]
    pub fn denied_error(&self, method: &str, permission: Permission) -> RpcError {
        let why = if self.allows_method(method) {
            format!(
                "the tool's scope does not grant the \"{}\" permission",
                permission.name()
            )
        } else {
            format!("the tool's scope does not include {method}")
        };
        RpcError::new(
            protocol::PERMISSION_DENIED,
            TOOL_SCOPE_DENIED,
            format!("{why} (tool {})", self.tool),
            "widen [scope] permissions or methods in the tool's .tool.toml manifest and relaunch it",
        )
    }
}

/// The environment variable holding the scoped listener's URL (`ws://127.0.0.1:<port>`) in a
/// launched tool.
pub const ENDPOINT_ENV: &str = "REGIA_TOOL_ENDPOINT";

/// The environment variable holding a launched tool's scoped token (never written to disk).
pub const TOKEN_ENV: &str = "REGIA_TOOL_TOKEN";

/// The environment variable holding a launched tool's manifest id.
pub const ID_ENV: &str = "REGIA_TOOL_ID";

/// A method or permission outside the caller's scope.
pub const TOOL_SCOPE_DENIED: &str = "tool_scope_denied";

/// The caller's token was revoked (the tool was stopped or exited, or the editor closed).
pub const TOOL_TOKEN_REVOKED: &str = "tool_token_revoked";

/// The refusal for a request on a revoked token.
#[must_use]
pub fn revoked_error(tool: &str) -> RpcError {
    RpcError::new(
        protocol::PERMISSION_DENIED,
        TOOL_TOKEN_REVOKED,
        format!("the token of tool {tool} was revoked"),
        "the tool was stopped or exited; the editor issues a new token at the next launch",
    )
}

/// Whether `pattern` matches `method`: exact, a prefix ending in `*`, or `*`.
#[must_use]
pub fn method_matches(pattern: &str, method: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => method.starts_with(prefix),
        None => pattern == method,
    }
}

/// One live token and its scope.
type TokenEntry = (String, Arc<ToolScope>);

/// The live scoped tokens, shared by the scoped listener's connection threads and the host.
#[derive(Clone, Default)]
pub struct ToolTokens {
    inner: Arc<RwLock<Vec<TokenEntry>>>,
    next_id: Arc<AtomicU64>,
}

impl ToolTokens {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a fresh token for `scope` (its `id` is assigned here) and returns `(token, scope)`.
    pub fn issue(&self, mut scope: ToolScope) -> (String, Arc<ToolScope>) {
        scope.id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let scope = Arc::new(scope);
        let token = crate::endpoint::new_token();
        match self.inner.write() {
            Ok(mut table) => table.push((token.clone(), Arc::clone(&scope))),
            Err(poisoned) => poisoned.into_inner().push((token.clone(), Arc::clone(&scope))),
        }
        (token, scope)
    }

    /// Revokes the token of scope `id`. Returns whether it was live.
    pub fn revoke(&self, id: u64) -> bool {
        let mut table = match self.inner.write() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = table.len();
        table.retain(|(_, scope)| scope.id != id);
        table.len() != before
    }

    /// Revokes every token (the host is shutting down).
    pub fn revoke_all(&self) {
        match self.inner.write() {
            Ok(mut table) => table.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
    }

    /// The scope of `token`, when it is live. Every entry is compared in constant time, so the
    /// lookup's timing does not reveal how much of a guess matched.
    #[must_use]
    pub fn lookup(&self, token: &str) -> Option<Arc<ToolScope>> {
        let table = match self.inner.read() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut found = None;
        for (live, scope) in table.iter() {
            if crate::server::constant_time_eq(live.as_bytes(), token.as_bytes()) {
                found = Some(Arc::clone(scope));
            }
        }
        found
    }

    /// Whether the token of scope `id` is still live.
    #[must_use]
    pub fn is_live(&self, id: u64) -> bool {
        let table = match self.inner.read() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        table.iter().any(|(_, scope)| scope.id == id)
    }

    /// How many tokens are live.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.inner.read() {
            Ok(table) => table.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Whether no token is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A request from a scoped connection, with the caller's scope and the reply channel.
pub struct ScopedInbound {
    /// The request.
    pub request: Request,
    /// Where to send the response.
    pub reply: Sender<Response>,
    /// Who is calling.
    pub scope: Arc<ToolScope>,
}

/// The scoped listener's requests and its token table, as a world resource. Dispatch drains it
/// each frame next to [`crate::AgentInbox`] (no-op when absent).
#[derive(Resource)]
pub struct ScopedInbox {
    /// Requests from every scoped connection.
    pub inbox: Receiver<ScopedInbound>,
    /// The live tokens (requests on revoked tokens are refused).
    pub tokens: ToolTokens,
}

/// The scoped caller of the request being handled, inserted by dispatch for the length of one
/// call (absent for master-token calls). Extension handlers read it with [`current_caller`].
#[derive(Resource, Clone, Debug)]
pub struct AgentCaller(pub Arc<ToolScope>);

/// The scoped caller of the request being handled, or `None` for a master-token call.
#[must_use]
pub fn current_caller(world: &World) -> Option<Arc<ToolScope>> {
    world
        .get_resource::<AgentCaller>()
        .map(|caller| Arc::clone(&caller.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(methods: &[&str]) -> ToolScope {
        ToolScope::new(
            "red-rouge",
            [Permission::Read, Permission::Play, Permission::Tools],
            methods.iter().map(|m| (*m).to_string()).collect(),
        )
    }

    #[test]
    fn patterns_match_exact_prefix_and_all() {
        assert!(method_matches("scene.spawn", "scene.spawn"));
        assert!(!method_matches("scene.spawn", "scene.spawn_primitive"));
        assert!(method_matches("scene.*", "scene.spawn_primitive"));
        assert!(!method_matches("scene.*", "scenes.x"));
        assert!(method_matches("render.out_*", "render.out_start"));
        assert!(!method_matches("render.out_*", "render.png"));
        assert!(method_matches("*", "anything.at_all"));
    }

    /// `tools` is dropped at construction and refused even if it were present, so a tool's
    /// token never launches or drives tools.
    #[test]
    fn a_scope_never_grants_the_tools_permission() {
        let mut scope = scope(&["*"]);
        assert!(scope.allows_permission(Permission::Read));
        assert!(scope.allows_permission(Permission::Play));
        assert!(!scope.allows_permission(Permission::EditScene));
        assert!(!scope.allows_permission(Permission::Tools));
        scope.permissions.insert("tools".into());
        assert!(!scope.allows_permission(Permission::Tools));
    }

    #[test]
    fn denied_errors_name_the_reason_and_the_code() {
        let scope = scope(&["camera.*"]);
        let error = scope.denied_error("scene.spawn", Permission::EditScene);
        assert_eq!(error.code, protocol::PERMISSION_DENIED);
        let data = error.data.expect("data");
        assert_eq!(data["code"], TOOL_SCOPE_DENIED);
        assert!(
            error.message.contains("does not include scene.spawn"),
            "{}",
            error.message
        );
        let error = scope.denied_error("camera.set_path", Permission::EditScene);
        assert!(error.message.contains("\"edit_scene\""), "{}", error.message);
    }

    #[test]
    fn tokens_are_issued_looked_up_and_revoked() {
        let tokens = ToolTokens::new();
        let (a, scope_a) = tokens.issue(scope(&["*"]));
        let (b, scope_b) = tokens.issue(scope(&["*"]));
        assert_ne!(a, b, "a fresh token per launch");
        assert_ne!(scope_a.id, scope_b.id);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens.lookup(&a).map(|s| s.id), Some(scope_a.id));
        assert!(tokens.lookup("not-a-token").is_none());
        assert!(tokens.lookup("").is_none());
        assert!(tokens.revoke(scope_a.id));
        assert!(!tokens.revoke(scope_a.id), "already revoked");
        assert!(tokens.lookup(&a).is_none());
        assert!(!tokens.is_live(scope_a.id));
        assert!(tokens.is_live(scope_b.id));
        tokens.revoke_all();
        assert!(tokens.is_empty());
    }

    #[test]
    fn the_caller_resource_is_read_back() {
        let mut world = World::new();
        assert!(current_caller(&world).is_none());
        world.insert_resource(AgentCaller(Arc::new(scope(&["*"]))));
        let caller = current_caller(&world).expect("caller");
        assert_eq!(caller.client_name(), "tool:red-rouge");
    }
}
