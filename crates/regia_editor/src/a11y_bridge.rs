// SPDX-License-Identifier: MPL-2.0
//! The editor window's screen-reader bridge (ADR-7746; FU-1 of
//! `docs/evidence/editor-a11y-2026-11.md`, `book/agents/editor-a11y.md`).
//!
//! egui builds an AccessKit tree of the editor's widgets every frame while AccessKit is on
//! (`PlatformOutput::accesskit_update`) and reads screen-reader requests from its input
//! (`egui::Event::AccessKitActionRequest`). The editor's `accesskit` cargo feature (on by default)
//! connects both ends to the runtime's OS bridge (`regia_platform::accessibility`, ADR-1998)
//! instead of a second adapter crate:
//!
//! - [`attach_window`] creates the same OS adapter the runner gives a game's window (AT-SPI on
//!   Linux, UI Automation on Windows, NSAccessibility on macOS) for the editor's window, over an
//!   [`EditorA11y`] bridge kept in the edit world. A reader's request wakes the idle editor
//!   loop through the editor's waker.
//! - [`before_frame`] turns egui's tree on while assistive technology is connected (or an agent
//!   asked, `editor.a11y_keep_tree { on: true }`) and moves the queued action requests (click, focus,
//!   set value, increment, scroll into view, ...) into the frame's raw input, where egui's widgets
//!   act on them as on a pointer or key.
//! - [`after_frame`] takes the frame's tree out of egui's output, names its root window, pushes it
//!   through the bridge (which checks it as the adapter would and keeps the copy the agent and
//!   tests read) and drives the OS adapter.
//!
//! [`region`] gives a toolbar or a dock pane a named container node, so a reader announces where
//! focus is ("Inspector, pane") and a test can walk one area's subtree; [`unspoken_in`] lists the
//! operable widgets of an area whose name has no letter or digit (a glyph alone, such as an eye
//! or a lock, is not a name a reader can speak).

use egui::accesskit::{Node, NodeId, Role, TreeUpdate};

/// The spoken name of the editor window's root node.
pub const WINDOW_NAME: &str = "Regia Engine Editor";
/// The play toolbar's region name.
pub const PLAY_TOOLBAR: &str = "Play toolbar";
/// The viewport toolbar's region name.
pub const VIEWPORT_TOOLBAR: &str = "Viewport toolbar";

/// Makes `ui` a container node with `role` and the spoken `name` (a toolbar, a dock pane). Call it
/// first in the `ui`, before any widget: widgets drawn in `ui` and its children attach to the
/// first ancestor that already has a node. Does nothing while AccessKit is off.
pub fn region(ui: &egui::Ui, role: Role, name: &str) {
    let rect = ui.max_rect();
    ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
        node.set_role(role);
        node.set_label(name);
        if rect.is_finite() {
            node.set_bounds(egui::accesskit::Rect {
                x0: f64::from(rect.min.x),
                y0: f64::from(rect.min.y),
                x1: f64::from(rect.max.x),
                y1: f64::from(rect.max.y),
            });
        }
    });
}

/// Sets the spoken name of `response`'s widget, keeping its role and actions (egui filled them
/// when it drew the widget): for a field with only hint text, or a combo box that shows only its
/// value. Unlike `on_hover_text`, this is what a reader speaks: the AccessKit label, or the value
/// of a `Label` node, which the OS adapters (AT-SPI, UI Automation, NSAccessibility) name by its
/// value and never by a label (a draggable `::` grip is a `Label`; ADR-8587).
pub fn name_node(response: &egui::Response, name: &str) {
    response.ctx.accesskit_node_builder(response.id, |node| {
        if node.role() == Role::Label {
            node.set_value(name);
        } else {
            node.set_label(name);
        }
    });
}

/// Whether `name` is something a reader can speak: it has a letter or a digit (a glyph such as
/// `👁`, `🔒` or `–` alone is not).
#[must_use]
pub fn spoken(name: &str) -> bool {
    name.chars().any(char::is_alphanumeric)
}

/// Whether a person operates nodes of `role` (they must have a spoken name).
fn operable(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::CheckBox
            | Role::RadioButton
            | Role::TextInput
            | Role::MultilineTextInput
            | Role::SpinButton
            | Role::Slider
            | Role::ComboBox
            | Role::Link
            | Role::Tab
            | Role::MenuItem
            | Role::Switch
    )
}

/// The node `id` of `update`.
#[must_use]
pub fn node(update: &TreeUpdate, id: NodeId) -> Option<&Node> {
    update.nodes.iter().find(|(n, _)| *n == id).map(|(_, node)| node)
}

/// The first container node of `update` with `role` and the spoken `name` (a [`region`]).
#[must_use]
pub fn find_region(update: &TreeUpdate, role: Role, name: &str) -> Option<NodeId> {
    update
        .nodes
        .iter()
        .find(|(_, node)| node.role() == role && node.label() == Some(name))
        .map(|(id, _)| *id)
}

/// The nodes under `root` (itself excluded), depth first.
#[must_use]
pub fn subtree(update: &TreeUpdate, root: NodeId) -> Vec<(NodeId, &Node)> {
    let by_id: std::collections::HashMap<NodeId, &Node> =
        update.nodes.iter().map(|(id, node)| (*id, node)).collect();
    let mut out = Vec::new();
    let mut stack: Vec<NodeId> = by_id
        .get(&root)
        .map(|n| n.children().iter().rev().copied().collect())
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(node) = by_id.get(&id) {
            out.push((id, *node));
            stack.extend(node.children().iter().rev().copied());
        }
    }
    out
}

/// The name a reader speaks for `node`, by the rule the OS adapters share (ADR-8587): a `Label` is
/// named by its value (egui keeps a label's text there; a label set on it is never read); another
/// node by its label, else by the nodes it is labelled by (a field tied to a separate "Search"
/// label), a `Label` among them giving its value. Empty when it has none.
#[must_use]
pub fn spoken_name(update: &TreeUpdate, node: &Node) -> String {
    os_name(node, |id| self::node(update, id))
}

/// [`spoken_name`] over any node lookup (a tree update or the bridge's platform tree).
fn os_name<'a>(node: &Node, lookup: impl Fn(NodeId) -> Option<&'a Node>) -> String {
    if node.role() == Role::Label {
        return node.value().unwrap_or_default().to_owned();
    }
    if let Some(label) = node.label() {
        return label.to_owned();
    }
    node.labelled_by()
        .iter()
        .filter_map(|id| lookup(*id))
        .filter_map(|n| {
            if n.role() == Role::Label {
                n.value()
            } else {
                n.label()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The operable nodes under `root` as `(role, spoken name)`, depth first (name empty when
/// unnamed).
#[must_use]
pub fn widgets_in(update: &TreeUpdate, root: NodeId) -> Vec<(String, String)> {
    subtree(update, root)
        .into_iter()
        .filter(|(_, node)| operable(node.role()))
        .map(|(_, node)| (format!("{:?}", node.role()), spoken_name(update, node)))
        .collect()
}

/// The operable nodes under `root` with no spoken name ([`spoken`]), as `Role "name"`.
#[must_use]
pub fn unspoken_in(update: &TreeUpdate, root: NodeId) -> Vec<String> {
    widgets_in(update, root)
        .into_iter()
        .filter(|(_, name)| !spoken(name))
        .map(|(role, name)| format!("{role} {name:?}"))
        .collect()
}

#[cfg(feature = "accesskit")]
pub use bridge::{EditorA11y, after_frame, attach_window, before_frame, create_window_hidden, methods};

/// Without the `accesskit` feature the editor window has no OS adapter: the run loop's calls do
/// nothing.
#[cfg(not(feature = "accesskit"))]
mod off {
    use regia_ecs::prelude::World;

    /// Whether the window must be created hidden (never, without the feature).
    #[must_use]
    pub fn create_window_hidden() -> bool {
        false
    }

    /// Nothing to attach without the feature.
    pub fn attach_window(_world: &mut World, _window: &winit::window::Window) {}

    /// Nothing to inject without the feature.
    pub fn before_frame(_ctx: &egui::Context, _world: &mut World, _raw: &mut egui::RawInput) {}

    /// Drops the frame's tree (egui builds none while AccessKit is off).
    pub fn after_frame(_ctx: &egui::Context, _world: &mut World, output: &mut egui::PlatformOutput) {
        output.accesskit_update = None;
    }
}

#[cfg(not(feature = "accesskit"))]
pub use off::{after_frame, attach_window, before_frame, create_window_hidden};

#[cfg(feature = "accesskit")]
mod bridge {
    use egui::accesskit::{self, ActionData, ActionRequest, NodeId, TreeId, TreeUpdate};
    use regia_agent::protocol::{INVALID_PARAMS, RpcError};
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    use regia_ecs::prelude::*;
    use regia_platform::accessibility::{AccessKitBridge, PlatformTree, WindowAdapter};
    use serde_json::{Value, json};

    use super::WINDOW_NAME;

    /// An editor agent handler.
    type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

    /// The editor window's bridge, a resource of the edit world: the runtime's [`AccessKitBridge`]
    /// (tree out, action requests in, the platform tree copy) and whether egui builds its tree.
    #[derive(Resource, Debug, Clone, Default)]
    pub struct EditorA11y {
        bridge: AccessKitBridge,
        keep_on: bool,
        tree_on: bool,
        frames_sent: u64,
        actions_injected: u64,
    }

    impl EditorA11y {
        /// The bridge (tests push action requests into it the way the OS adapter does).
        #[must_use]
        pub fn bridge(&self) -> &AccessKitBridge {
            &self.bridge
        }

        /// Keeps egui's tree on without a connected reader (an agent or a test reads it).
        pub fn keep_tree_on(&mut self, on: bool) {
            self.keep_on = on;
        }

        /// Whether egui builds its tree this frame.
        #[must_use]
        pub fn tree_on(&self) -> bool {
            self.tree_on
        }

        /// Frames whose tree went through the bridge.
        #[must_use]
        pub fn frames_sent(&self) -> u64 {
            self.frames_sent
        }

        /// Action requests moved into egui's input.
        #[must_use]
        pub fn actions_injected(&self) -> u64 {
            self.actions_injected
        }
    }

    /// Whether the run loop must create the editor window hidden and show it after
    /// [`attach_window`] (the Windows and macOS adapters subclass the window before it is shown).
    #[must_use]
    pub fn create_window_hidden() -> bool {
        WindowAdapter::must_create_hidden()
    }

    /// Creates the OS adapter for the editor's `window` (an [`EditorA11y`] is inserted when the
    /// world has none). The editor's waker (`SpaceMouseWaker`) wakes the loop for a reader's
    /// request. A refused adapter is logged; the bridge and its tree copy still work.
    pub fn attach_window(world: &mut World, window: &winit::window::Window) {
        let state = world.get_resource_or_insert_with(EditorA11y::default).clone();
        if let Some(waker) = world.get_resource::<crate::spacemouse::SpaceMouseWaker>() {
            let wake = std::sync::Arc::clone(&waker.0);
            state.bridge.set_waker(move || wake());
        }
        match WindowAdapter::attach(window, &state.bridge) {
            Ok(adapter) => {
                tracing::info!(
                    target: "regia_editor",
                    "screen-reader bridge: {} adapter attached to the editor window",
                    WindowAdapter::name()
                );
                world.insert_non_send(adapter);
            }
            Err(error) => {
                tracing::info!(target: "regia_editor", "screen-reader bridge: no OS adapter: {error}");
            }
        }
    }

    /// Before the frame's UI: turns egui's tree on or off and moves the queued action requests into
    /// `raw` (oldest first).
    pub fn before_frame(ctx: &egui::Context, world: &mut World, raw: &mut egui::RawInput) {
        let mut state = world.get_resource_or_insert_with(EditorA11y::default);
        let want = state.bridge.is_active() || state.keep_on;
        if want != state.tree_on {
            if want {
                ctx.enable_accesskit();
            } else {
                ctx.disable_accesskit();
            }
            state.tree_on = want;
        }
        let requests = state.bridge.take_actions();
        state.actions_injected += requests.len() as u64;
        raw.events
            .extend(requests.into_iter().map(egui::Event::AccessKitActionRequest));
    }

    /// After the frame's UI: takes the tree out of `output`, names the root window, pushes it through
    /// the bridge and drives the OS adapter with the window's focus.
    pub fn after_frame(ctx: &egui::Context, world: &mut World, output: &mut egui::PlatformOutput) {
        let _span = tracing::trace_span!("editor_a11y_frame").entered();
        let Some(mut state) = world.get_resource_mut::<EditorA11y>() else {
            output.accesskit_update = None;
            return;
        };
        if let Some(mut update) = output.accesskit_update.take() {
            name_root(&mut update);
            state.frames_sent += 1;
            if let Err(error) = state.bridge.push_update(update) {
                tracing::warn!(target: "regia_editor", "screen-reader bridge: tree refused: {error}");
            }
        }
        let bridge = state.bridge.clone();
        let focused = ctx.input(|i| i.focused);
        if let Some(mut adapter) = world.get_non_send_mut::<WindowAdapter>() {
            adapter.drive(&bridge, focused);
        }
    }

    /// Gives egui's root window node the editor's name when it has none.
    fn name_root(update: &mut TreeUpdate) {
        let Some(root) = update.tree.as_ref().map(|tree| tree.root) else {
            return;
        };
        if let Some((_, node)) = update.nodes.iter_mut().find(|(id, _)| *id == root)
            && node.label().is_none()
        {
            node.set_label(WINDOW_NAME);
        }
    }

    /// The name the OS adapter exposes for `node` of the platform tree (ADR-8587): what a reader
    /// speaks, so what the agent answers and matches by `name`.
    fn platform_name(tree: &PlatformTree, node: &accesskit::Node) -> String {
        super::os_name(node, |id| tree.get(id))
    }

    /// One node of the platform tree, as the agent answers it.
    fn node_json(tree: &PlatformTree, id: NodeId, parent: Option<NodeId>) -> Option<Value> {
        let node = tree.get(id)?;
        let actions: Vec<&str> = PlatformTree::actions(node)
            .into_iter()
            .map(regia_platform::accessibility::action_name)
            .collect();
        Some(json!({
            "id": id.0,
            "parent": parent.map(|p| p.0),
            "role": format!("{:?}", node.role()),
            "name": platform_name(tree, node),
            "value": node.value(),
            "numeric_value": node.numeric_value(),
            "toggled": node.toggled().map(|t| format!("{t:?}").to_lowercase()),
            "disabled": node.is_disabled(),
            "focused": tree.focus == Some(id),
            "actions": actions,
        }))
    }

    fn error(code: &'static str, message: impl Into<String>, hint: &'static str) -> RpcError {
        RpcError::new(INVALID_PARAMS, code, message, hint)
    }

    /// `editor.a11y_tree` (read only: turning the tree on is `editor.a11y_keep_tree`).
    fn editor_a11y_tree(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        if params.get("keep_on").is_some() {
            return Err(error(
                "keep_on_moved",
                "editor.a11y_tree only reads the tree; keep_on changes editor state",
                "call editor.a11y_keep_tree { on: true }, wait a frame, then editor.a11y_tree",
            ));
        }
        let state = world.get_resource_or_insert_with(EditorA11y::default);
        let role = params.get("role").and_then(Value::as_str);
        let name = params.get("name").and_then(Value::as_str);
        let bridge = state.bridge.clone();
        let stats = bridge.stats();
        let nodes: Vec<Value> = bridge.with_platform_tree(|tree| {
            tree.depth_first()
                .into_iter()
                .filter(|(id, _)| {
                    tree.get(*id).is_some_and(|node| {
                        role.is_none_or(|r| format!("{:?}", node.role()) == r)
                            && name.is_none_or(|n| platform_name(tree, node) == n)
                    })
                })
                .filter_map(|(id, parent)| node_json(tree, id, parent))
                .collect()
        });
        Ok(json!({
            "adapter": bridge.adapter_name(),
            "adapter_error": bridge.adapter_error(),
            "active": bridge.is_active(),
            "keep_on": state.keep_on,
            "tree_on": state.tree_on,
            "frames_sent": state.frames_sent,
            "actions_injected": state.actions_injected,
            "actions_queued": stats.actions,
            "invalid_updates": stats.invalid_updates,
            "nodes": nodes,
        }))
    }

    /// `editor.a11y_keep_tree`: whether egui builds its tree every frame without a connected reader
    /// (the agent's switch; a mutating call, so a read-only client cannot turn the cost on).
    fn editor_a11y_keep_tree(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        let Some(on) = params.get("on").and_then(Value::as_bool) else {
            return Err(error(
                "invalid_value",
                "on must be true or false",
                "editor.a11y_keep_tree { on: true } keeps the tree on; false builds it only for a reader",
            ));
        };
        let mut state = world.get_resource_or_insert_with(EditorA11y::default);
        state.keep_tree_on(on);
        Ok(json!({ "keep_on": state.keep_on, "tree_on": state.tree_on }))
    }

    /// `editor.a11y_action` presses any editor widget by name, the Agent Changes panel's Accept and
    /// the package, launch and plugin controls included: on top of the schema's `review` it needs
    /// `build` (review #1, ADR-7795). Neither is in the default profile, and a tool's scope never
    /// holds `review`, so only a project that grants both in `.regia/agent.toml` lets an agent drive
    /// the editor's UI this way. A person's screen reader goes through the OS adapter, not here.
    fn check_ui_driver_permission(world: &World) -> Result<(), RpcError> {
        let profile = world
            .get_resource::<regia_agent::AgentPermissions>()
            .cloned()
            .unwrap_or_default();
        for permission in [Permission::Review, Permission::Build] {
            if !profile.allows(permission) {
                return Err(profile.denied_error(permission));
            }
        }
        Ok(())
    }

    /// The node an `editor.a11y_action` call names: `node`, or the one node with `name` (and
    /// `role`, when given).
    fn target(tree: &PlatformTree, params: &Value) -> Result<NodeId, RpcError> {
        if let Some(id) = params.get("node").and_then(Value::as_u64) {
            return if tree.get(NodeId(id)).is_some() {
                Ok(NodeId(id))
            } else {
                Err(error(
                    "node_not_found",
                    format!("no node {id} in the editor's tree"),
                    "list the nodes with editor.a11y_tree",
                ))
            };
        }
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(error(
                "missing_target",
                "give node or name",
                "editor.a11y_tree lists each node's id and name",
            ));
        };
        let role = params.get("role").and_then(Value::as_str);
        let hits: Vec<NodeId> = tree
            .depth_first()
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| {
                tree.get(*id).is_some_and(|node| {
                    platform_name(tree, node) == name
                        && role.is_none_or(|r| format!("{:?}", node.role()) == r)
                })
            })
            .collect();
        match hits.as_slice() {
            [one] => Ok(*one),
            [] => Err(error(
                "node_not_found",
                format!("no node named {name:?} in the editor's tree"),
                "call editor.a11y_keep_tree { on: true }, wait a frame, then list the names with editor.a11y_tree",
            )),
            _ => Err(error(
                "ambiguous_name",
                format!("{} nodes are named {name:?}", hits.len()),
                "add role, or give the node id from editor.a11y_tree",
            )),
        }
    }

    /// `editor.a11y_action`.
    fn editor_a11y_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        check_ui_driver_permission(world)?;
        let Some(action_name) = params.get("action").and_then(Value::as_str) else {
            return Err(error(
                "unknown_action",
                "action is required",
                "click, focus, set_value, increment, decrement, scroll_into_view, ...",
            ));
        };
        let Some(action) = regia_platform::accessibility::parse_action(action_name) else {
            return Err(error(
                "unknown_action",
                format!("unknown action {action_name:?}"),
                "click, focus, set_value, increment, decrement, scroll_into_view, ...",
            ));
        };
        let data = match params.get("value") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n.as_f64().map(ActionData::NumericValue),
            Some(Value::String(s)) => Some(ActionData::Value(s.as_str().into())),
            Some(_) => {
                return Err(error(
                    "invalid_value",
                    "value must be a number or a string",
                    "a slider or drag value takes a number",
                ));
            }
        };
        let state = world.get_resource_or_insert_with(EditorA11y::default);
        let bridge = state.bridge.clone();
        let node = bridge.with_platform_tree(|tree| {
            if tree.root.is_none() {
                return Err(error(
                    "no_tree",
                    "the editor has sent no accessibility tree yet",
                    "call editor.a11y_keep_tree { on: true } and wait a frame",
                ));
            }
            target(tree, params)
        })?;
        bridge.push_action(ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: node,
            data,
        });
        Ok(json!({ "queued": true, "node": node.0, "action": action_name }))
    }

    /// The screen-reader bridge methods.
    #[must_use]
    pub fn methods() -> Vec<(MethodSchema, Handler)> {
        vec![
            (
                MethodSchema {
                    name: "editor.a11y_tree",
                    tool: "a11y_tree",
                    description: "The editor window's accessibility tree as the OS screen-reader adapter has it (ADR-7746): every node's id, parent, role, name, value, toggled state and actions, depth first. egui builds the tree only while a reader is connected, or after editor.a11y_keep_tree { on: true } (the next frame has it). Read only. Filter with role and name.",
                    params: &[
                        ParamSchema {
                            name: "role",
                            kind: "string",
                            required: false,
                            description: "Only nodes of this AccessKit role (Button, CheckBox, TextInput, SpinButton, Toolbar, Pane, ...).",
                        },
                        ParamSchema {
                            name: "name",
                            kind: "string",
                            required: false,
                            description: "Only nodes with exactly this spoken name.",
                        },
                    ],
                    result: "{ adapter, adapter_error, active, keep_on, tree_on, frames_sent, actions_injected, actions_queued, invalid_updates, nodes: [{ id, parent, role, name, value, numeric_value, toggled, disabled, focused, actions }] }",
                    mutating: false,
                    permission: Permission::Read,
                },
                editor_a11y_tree as Handler,
            ),
            (
                MethodSchema {
                    name: "editor.a11y_keep_tree",
                    tool: "a11y_keep_tree",
                    description: "Keeps the editor window's accessibility tree on without a connected screen reader, so editor.a11y_tree has one (ADR-7746, ADR-7795): egui then builds it every frame, which costs UI time, until on: false.",
                    params: &[ParamSchema {
                        name: "on",
                        kind: "boolean",
                        required: true,
                        description: "true: build the tree every frame; false: only while a reader is connected.",
                    }],
                    result: "{ keep_on, tree_on }",
                    mutating: true,
                    permission: Permission::EditScene,
                },
                editor_a11y_keep_tree as Handler,
            ),
            (
                MethodSchema {
                    name: "editor.a11y_action",
                    tool: "a11y_action",
                    description: "Queues a screen-reader action request for a node of the editor's accessibility tree, exactly as the OS adapter does when a reader asks (ADR-7746): click (the default action), focus, set_value, increment, decrement, scroll_into_view, ... The next frame's widgets act on it. Name the node by id or by its spoken name (plus role when two share it). It can press any editor control (Agent Changes Accept, package, launch, plugins), so it needs the review and build permissions, neither in the default profile (ADR-7795).",
                    params: &[
                        ParamSchema {
                            name: "action",
                            kind: "string",
                            required: true,
                            description: "click | focus | blur | set_value | increment | decrement | scroll_into_view | expand | collapse | ... (snake_case AccessKit actions).",
                        },
                        ParamSchema {
                            name: "node",
                            kind: "integer",
                            required: false,
                            description: "The node id from editor.a11y_tree.",
                        },
                        ParamSchema {
                            name: "name",
                            kind: "string",
                            required: false,
                            description: "The node's spoken name, when node is absent.",
                        },
                        ParamSchema {
                            name: "role",
                            kind: "string",
                            required: false,
                            description: "With name: only a node of this role.",
                        },
                        ParamSchema {
                            name: "value",
                            kind: "any",
                            required: false,
                            description: "set_value's value: a number for a slider or drag value.",
                        },
                    ],
                    result: "{ queued: true, node, action }",
                    mutating: true,
                    permission: Permission::Review,
                },
                editor_a11y_action as Handler,
            ),
        ]
    }

    // `accesskit` is egui's re-export of the runtime bridge's crate: one version (0.24.1) in the lock.
    const _: fn(accesskit::ActionRequest) -> regia_platform::accessibility::accesskit::ActionRequest = |r| r;
}

#[cfg(test)]
mod tests;
