// SPDX-License-Identifier: MPL-2.0
//! The screen-reader OS bridge as the agent sees it (ADR-1998): a dump of the platform tree (the tree
//! the OS adapter holds) and a simulated platform action request. Compiled with or without the
//! `accesskit` feature, so `ui.a11y_platform_tree` and `ui.a11y_platform_action` exist on every
//! build and say why when the bridge is not built in.

use regia_app::prelude::*;
use serde_json::Value;

/// Whether this build has the bridge (`accesskit` feature of `regia_ui`).
pub const BRIDGE_BUILT: bool = cfg!(feature = "accesskit");

/// A value passed with a simulated action.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformActionValue {
    /// Text (`set_value`, `replace_selected_text`).
    Text(String),
    /// A number (`set_value` on a slider, the custom action id of `custom_action`).
    Number(f64),
}

/// Why a platform-bridge call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformBridgeError {
    /// The build has no bridge (`accesskit` feature off), or the app installed another screen-reader
    /// backend.
    Off(&'static str),
    /// The node is not in the platform tree.
    UnknownNode(u64),
    /// The action name is not an AccessKit action.
    BadAction(String),
    /// The node does not advertise the action.
    Unsupported,
    /// The action needs a value (`set_value`, `replace_selected_text`, `custom_action`).
    MissingValue,
}

/// The platform tree as JSON (`ui.a11y_platform_tree`).
pub fn platform_tree_json(world: &World) -> Result<Value, PlatformBridgeError> {
    imp::platform_tree_json(world)
}

/// Simulates assistive technology connecting (`true`: the next frame sends the whole tree) or going
/// away (`ui.a11y_platform_connect`); returns `{ active, was_active, adapter, note, android }`.
pub fn set_connected(world: &World, connected: bool) -> Result<Value, PlatformBridgeError> {
    imp::set_connected(world, connected)
}

/// Queues `action` on `node` as if the OS adapter had sent it (`ui.a11y_platform_action`); the UI
/// routes it in the next frame's `PreUpdate`.
pub fn queue_platform_action(
    world: &World,
    node: u64,
    action: &str,
    value: Option<PlatformActionValue>,
) -> Result<Value, PlatformBridgeError> {
    imp::queue_platform_action(world, node, action, value)
}

#[cfg(not(feature = "accesskit"))]
mod imp {
    use super::{PlatformActionValue, PlatformBridgeError, Value, World};

    const OFF: &str = "this build has no screen-reader OS bridge (build regia with the `accesskit` feature)";

    pub(super) fn platform_tree_json(_world: &World) -> Result<Value, PlatformBridgeError> {
        Err(PlatformBridgeError::Off(OFF))
    }

    pub(super) fn set_connected(_world: &World, _connected: bool) -> Result<Value, PlatformBridgeError> {
        Err(PlatformBridgeError::Off(OFF))
    }

    pub(super) fn queue_platform_action(
        _world: &World,
        _node: u64,
        _action: &str,
        _value: Option<PlatformActionValue>,
    ) -> Result<Value, PlatformBridgeError> {
        Err(PlatformBridgeError::Off(OFF))
    }
}

#[cfg(feature = "accesskit")]
mod imp {
    use accesskit::{Action, ActionData, ActionRequest, Live, Node, NodeId, TreeId};
    use regia_platform::accessibility::{
        AccessKitBridge, PlatformTree, action_name, android_bridge_status, parse_action,
    };
    use serde_json::json;

    use super::{PlatformActionValue, PlatformBridgeError, Value, World};
    use crate::accesskit_bridge::request_from_action;

    const NO_BRIDGE: &str = "the app installed another screen-reader backend, so no AccessKit bridge runs";

    fn bridge(world: &World) -> Result<&AccessKitBridge, PlatformBridgeError> {
        world
            .get_resource::<AccessKitBridge>()
            .ok_or(PlatformBridgeError::Off(NO_BRIDGE))
    }

    fn node_json(id: NodeId, parent: Option<NodeId>, node: &Node) -> Value {
        let toggled = node.toggled().map(|t| match t {
            accesskit::Toggled::True => "true",
            accesskit::Toggled::False => "false",
            accesskit::Toggled::Mixed => "mixed",
        });
        let live = node.live().map(|l| match l {
            Live::Off => "off",
            Live::Polite => "polite",
            Live::Assertive => "assertive",
        });
        json!({
            "id": id.0,
            "parent": parent.map(|p| p.0),
            "children": node.children().iter().map(|c| c.0).collect::<Vec<_>>(),
            "role": format!("{:?}", node.role()),
            "label": node.label(),
            "description": node.description(),
            "value": node.value(),
            "numeric_value": node.numeric_value(),
            "min": node.min_numeric_value(),
            "max": node.max_numeric_value(),
            "step": node.numeric_value_step(),
            "bounds": node.bounds().map(|r| [r.x0, r.y0, r.x1, r.y1]),
            "actions": PlatformTree::actions(node).into_iter().map(action_name).collect::<Vec<_>>(),
            "custom_actions": node
                .custom_actions()
                .iter()
                .map(|c| json!({ "id": c.id, "description": &*c.description }))
                .collect::<Vec<_>>(),
            "toggled": toggled,
            "expanded": node.is_expanded(),
            "selected": node.is_selected(),
            "disabled": node.is_disabled(),
            "read_only": node.is_read_only(),
            "modal": node.is_modal(),
            "invalid": node.invalid().is_some(),
            "live": live,
        })
    }

    fn android_field(bridge: &AccessKitBridge) -> Value {
        let error = bridge.adapter_error();
        let status = android_bridge_status(
            cfg!(target_os = "android"),
            bridge.adapter_name(),
            error.as_deref(),
        );
        json!({
            "os": status.os,
            "registered": status.registered,
            "attached": status.attached,
            "status": status.status,
            "detail": status.detail,
        })
    }

    pub(super) fn set_connected(world: &World, connected: bool) -> Result<Value, PlatformBridgeError> {
        let bridge = bridge(world)?;
        let was_active = bridge.is_active();
        if connected {
            bridge.activate();
        } else {
            bridge.deactivate();
        }
        Ok(json!({
            "active": bridge.is_active(),
            "was_active": was_active,
            "adapter": bridge.adapter_name(),
            "android": android_field(bridge),
            "note": if connected {
                "the next frame sends the whole tree; read it with ui.a11y_platform_tree"
            } else {
                "updates stop until assistive technology connects again"
            },
        }))
    }

    pub(super) fn platform_tree_json(world: &World) -> Result<Value, PlatformBridgeError> {
        let bridge = bridge(world)?;
        let stats = bridge.stats();
        let (root, focus, nodes) = bridge.with_platform_tree(|tree| {
            let nodes: Vec<Value> = tree
                .depth_first()
                .into_iter()
                .filter_map(|(id, parent)| tree.get(id).map(|n| node_json(id, parent, n)))
                .collect();
            (tree.root.map(|r| r.0), tree.focus.map(|f| f.0), nodes)
        });
        let adapter_error = bridge.adapter_error();
        Ok(json!({
            "enabled": true,
            "adapter": bridge.adapter_name(),
            "adapter_error": adapter_error,
            "android": android_field(bridge),
            "active": bridge.is_active(),
            "needs_full_tree": bridge.needs_full_tree(),
            "root": root,
            "focus": focus,
            "count": nodes.len(),
            "nodes": nodes,
            "last_error": bridge.last_error().map(|e| json!({ "reason": e.reason(), "message": e.to_string() })),
            "stats": {
                "updates": stats.updates,
                "full_updates": stats.full_updates,
                "nodes_sent": stats.nodes_sent,
                "delivered": stats.delivered,
                "superseded": stats.superseded,
                "invalid_updates": stats.invalid_updates,
                "resyncs": stats.resyncs,
                "actions": stats.actions,
                "actions_dropped": stats.actions_dropped,
                "activations": stats.activations,
                "deactivations": stats.deactivations,
            },
        }))
    }

    pub(super) fn queue_platform_action(
        world: &World,
        node: u64,
        action: &str,
        value: Option<PlatformActionValue>,
    ) -> Result<Value, PlatformBridgeError> {
        let bridge = bridge(world)?;
        let action = parse_action(action).ok_or_else(|| PlatformBridgeError::BadAction(action.to_owned()))?;
        let data = match (action, value) {
            (Action::SetValue, Some(PlatformActionValue::Text(text))) => Some(ActionData::Value(text.into())),
            (Action::SetValue, Some(PlatformActionValue::Number(n))) => Some(ActionData::NumericValue(n)),
            (Action::ReplaceSelectedText, Some(PlatformActionValue::Text(text))) => {
                Some(ActionData::Value(text.into()))
            }
            (Action::CustomAction, Some(PlatformActionValue::Number(n)))
                if n.fract() == 0.0 && n >= f64::from(i32::MIN) && n <= f64::from(i32::MAX) =>
            {
                // Integral and in range (checked above), so the cast is exact.
                Some(ActionData::CustomAction(n as i32))
            }
            (Action::SetValue | Action::ReplaceSelectedText | Action::CustomAction, _) => {
                return Err(PlatformBridgeError::MissingValue);
            }
            _ => None,
        };
        bridge.with_platform_tree(|tree| {
            let target = tree
                .get(NodeId(node))
                .ok_or(PlatformBridgeError::UnknownNode(node))?;
            let supported = match &data {
                Some(ActionData::CustomAction(id)) => target.custom_actions().iter().any(|c| c.id == *id),
                _ => target.supports_action(action),
            };
            if supported {
                Ok(())
            } else {
                Err(PlatformBridgeError::Unsupported)
            }
        })?;
        let request = ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: NodeId(node),
            data,
        };
        let maps_to = request_from_action(&request).map(|r| r.action.name());
        bridge.push_action(request);
        Ok(json!({
            "queued": true,
            "node": node,
            "action": action_name(action),
            "maps_to": maps_to,
            "note": "routed in the next frame's PreUpdate; ui.a11y_tree lists the result under `requests`",
        }))
    }
}
