// SPDX-License-Identifier: MPL-2.0
//! Read-only physical keyboard preset discovery (ADR-8034).

use regia_input::KeyboardLayout;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    layout: String,
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "keyboard_layout_bad_params",
        format!("invalid keyboard layout parameters: {detail}"),
        "pass {\"layout\":\"colemak\"}; supported layouts: qwerty, colemak, azerty",
    )
}

pub(crate) fn preset(params: &Value) -> Result<Value, RpcError> {
    let params: Params = serde_json::from_value(params.clone()).map_err(bad_params)?;
    let layout = KeyboardLayout::parse(&params.layout)
        .ok_or_else(|| bad_params("unknown layout; use qwerty, colemak or azerty"))?;
    let keys: Vec<Value> = layout
        .keys()
        .map(|(label, physical_key)| json!({ "label": label, "physical_key": physical_key }))
        .collect();
    Ok(json!({
        "layout": layout.name(),
        "layouts": KeyboardLayout::ALL.map(KeyboardLayout::name),
        "keys": keys,
        "movement": layout.movement_keys(),
    }))
}

#[cfg(test)]
mod tests {
    use regia_ecs::prelude::*;

    use super::*;

    #[test]
    fn keyboard_layout_presets_through_dispatcher() {
        let mut world = World::new();
        for (layout, label, key) in [
            ("colemak", "F", "KeyE"),
            ("azerty", "Z", "KeyW"),
            ("qwerty", "W", "KeyW"),
        ] {
            let out = crate::dispatch::call_in_process(
                &mut world,
                "input.keyboard_layout",
                json!({ "layout": layout }),
            )
            .expect("immediate")
            .expect("preset without an input plugin");
            assert_eq!(out["layout"], layout);
            let keys = out["keys"].as_array().expect("keys");
            assert_eq!(keys.len(), 30);
            assert!(
                keys.iter()
                    .any(|row| row["label"] == label && row["physical_key"] == key)
            );
            assert_eq!(out["movement"]["Keys2D"]["up"], "KeyW");
            assert_eq!(out["movement"]["Keys2D"]["down"], "KeyS");
            assert_eq!(out["movement"]["Keys2D"]["left"], "KeyA");
            assert_eq!(out["movement"]["Keys2D"]["right"], "KeyD");
        }
    }

    #[test]
    fn keyboard_layout_rejects_invalid_parameters() {
        for params in [
            json!({}),
            json!({ "layout": "unknown" }),
            json!({ "layout": 42 }),
            json!({ "layout": null }),
            json!({ "layout": "colemak", "apply": true }),
        ] {
            let error = preset(&params).expect_err("invalid parameters");
            assert_eq!(error.code, INVALID_PARAMS);
            assert_eq!(error.data.expect("data")["code"], "keyboard_layout_bad_params");
        }
        assert_eq!(
            preset(&json!({ "layout": "COLEMAK" })).expect("case")["layout"],
            "colemak"
        );
    }
}
