// SPDX-License-Identifier: MPL-2.0
//! `ui.a11y_platform_tree`, `ui.a11y_platform_action` and `ui.a11y_platform_connect`: the
//! screen-reader OS bridge (ADR-1998, book `agents/ui-a11y-platform.md`). The tree is the one the OS
//! adapter holds (AT-SPI, UI Automation, NSAccessibility, TalkBack on Android, ADR-3454), as the
//! bridge sent it; an action is queued as if the adapter had received it from a screen reader, and
//! runs through the UI's request path in the next frame. Success results of the tree and connect
//! calls include `android` (`os`, `registered`, `attached`, `status`, `detail`). On a build without
//! the `accesskit` feature every call answers `platform_bridge_off`.

use regia_ecs::prelude::*;
use regia_ui::a11y_platform::{
    PlatformActionValue, PlatformBridgeError, platform_tree_json, queue_platform_action, set_connected,
};
use serde_json::Value;

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// Longest text value accepted (characters), as `ui.a11y_act`.
const MAX_TEXT_VALUE: usize = 4096;

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn bridge_error(error: PlatformBridgeError) -> RpcError {
    match error {
        PlatformBridgeError::Off(why) => RpcError::new(
            UNSUPPORTED,
            "platform_bridge_off",
            why,
            "build the game with the regia `accesskit` feature and the default screen-reader backend; ui.a11y_tree works without it",
        ),
        PlatformBridgeError::UnknownNode(id) => RpcError::new(
            NOT_FOUND,
            "unknown_node",
            format!("node {id} is not in the platform tree"),
            "take ids from ui.a11y_platform_tree; the tree fills once assistive technology is connected (ui.a11y_platform_connect)",
        ),
        PlatformBridgeError::BadAction(name) => invalid(
            "bad_action",
            format!("`{name}` is not an AccessKit action"),
            "use a name from the node's `actions` in ui.a11y_platform_tree (click, focus, increment, set_value, ...)",
        ),
        PlatformBridgeError::Unsupported => invalid(
            "unsupported_action",
            "the node does not offer that action",
            "a node lists what it offers under `actions` and `custom_actions` in ui.a11y_platform_tree",
        ),
        PlatformBridgeError::MissingValue => invalid(
            "a11y_value_missing",
            "the action needs `value`",
            "set_value takes text or a number, replace_selected_text text, custom_action the action id",
        ),
    }
}

/// `ui.a11y_platform_tree {}`.
pub(crate) fn platform_tree(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    platform_tree_json(world).map_err(bridge_error)
}

/// `ui.a11y_platform_connect { connected }`.
pub(crate) fn platform_connect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let connected = params.get("connected").and_then(Value::as_bool).ok_or_else(|| {
        invalid(
            "bad_connected",
            "`connected` (true or false) is required",
            "ui.a11y_platform_connect { connected: true }",
        )
    })?;
    set_connected(world, connected).map_err(bridge_error)
}

/// `ui.a11y_platform_action { node, action, value? }`.
pub(crate) fn platform_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let node = match params
        .get("node")
        .or_else(|| params.get("entity"))
        .filter(|v| !v.is_null())
    {
        None => {
            return Err(invalid(
                "a11y_node_missing",
                "`node` (a node id from ui.a11y_platform_tree) is required",
                "ui.a11y_platform_action { node: 123, action: \"click\" }",
            ));
        }
        Some(v) => v.as_u64().ok_or_else(|| {
            invalid(
                "bad_entity",
                "`node` must be a node id from ui.a11y_platform_tree",
                "take it from the `id` of a platform tree node",
            )
        })?,
    };
    let action = match params.get("action").filter(|v| !v.is_null()) {
        None => {
            return Err(invalid(
                "a11y_action_missing",
                "`action` is required",
                "an AccessKit action name from the node's `actions`: click, focus, increment, decrement, set_value, ...",
            ));
        }
        Some(v) => v.as_str().ok_or_else(|| {
            invalid(
                "bad_action",
                "`action` must be a string",
                "an AccessKit action name such as click",
            )
        })?,
    };
    let value = match params.get("value").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::String(text)) => {
            if text.chars().count() > MAX_TEXT_VALUE {
                return Err(invalid(
                    "text_too_long",
                    format!("`value` is longer than {MAX_TEXT_VALUE} characters"),
                    "set a shorter text",
                ));
            }
            Some(PlatformActionValue::Text(text.clone()))
        }
        Some(v) => match v.as_f64().filter(|n| n.is_finite()) {
            Some(n) => Some(PlatformActionValue::Number(n)),
            None => {
                return Err(invalid(
                    "bad_value",
                    "`value` must be a string or a finite number",
                    "set_value takes text or a number, custom_action the action id",
                ));
            }
        },
    };
    queue_platform_action(world, node, action, value).map_err(bridge_error)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn params_are_checked_before_the_bridge() {
        let mut world = World::new();
        for (params, expected) in [
            (json!({ "action": "click" }), "a11y_node_missing"),
            (json!({ "node": "x", "action": "click" }), "bad_entity"),
            (json!({ "node": 5 }), "a11y_action_missing"),
            (json!({ "node": 5, "action": 3 }), "bad_action"),
            (
                json!({ "node": 5, "action": "set_value", "value": [1] }),
                "bad_value",
            ),
        ] {
            let err = platform_action(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), expected, "{params}");
        }
        let err = platform_connect(&mut world, &json!({})).expect_err("refused");
        assert_eq!(code(&err), "bad_connected");
    }

    #[test]
    fn without_a_bridge_every_call_says_the_bridge_is_off() {
        // No bridge resource (and, on a build without the `accesskit` feature, no bridge at all).
        let mut world = World::new();
        let err = platform_tree(&mut world, &json!({})).expect_err("off");
        assert_eq!(code(&err), "platform_bridge_off");
        let err = platform_connect(&mut world, &json!({ "connected": true })).expect_err("off");
        assert_eq!(code(&err), "platform_bridge_off");
        let err = platform_action(&mut world, &json!({ "node": 5, "action": "click" })).expect_err("off");
        assert_eq!(code(&err), "platform_bridge_off");
    }
}
