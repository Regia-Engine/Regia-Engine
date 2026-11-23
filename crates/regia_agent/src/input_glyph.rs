// SPDX-License-Identifier: MPL-2.0
//! `input.glyph {action, device?}`: the button-prompt glyph a device family shows for an action
//! (book `agents/input.md`, `docs/specs/11` § 7, ADR-7070): the project's `InputGlyph` table first,
//! then the built-in families of the UI hint actions, then the action's binding.

use regia_ecs::prelude::*;
use regia_input::InputConfig;
use regia_ui::HintDevice;
use regia_ui::input_glyphs::InputGlyphs;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    action: String,
    device: Option<String>,
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "input_glyph_bad_params",
        format!("bad `input.glyph` parameters: {detail}"),
        "pass { action: \"Jump\", device: \"xbox\" }; device is keyboard_mouse, xbox, playstation or steam_deck",
    )
}

/// `input.glyph`: the glyph and where it came from (`table`, `builtin`, `binding`).
pub(crate) fn glyph(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let params: Params = serde_json::from_value(params.clone()).map_err(bad_params)?;
    if params.action.is_empty() {
        return Err(bad_params("`action` is empty"));
    }
    let device = match params.device.as_deref() {
        Some(name) => {
            HintDevice::parse(name).ok_or_else(|| bad_params(format!("unknown device {name:?}")))?
        }
        None => world.get_resource::<HintDevice>().copied().unwrap_or_default(),
    };
    let empty = InputGlyphs::default();
    let glyphs = world.get_resource::<InputGlyphs>().unwrap_or(&empty);
    let table_rows = glyphs.0.glyphs.len();
    let hit = glyphs.lookup(&params.action, device, world.get_resource::<InputConfig>());
    let Some(hit) = hit else {
        return Err(RpcError::new(
            NOT_FOUND,
            "input_glyph_unknown",
            format!(
                "no glyph for {:?} on {}: not in the glyph table, not a hint action, and no binding the device can press",
                params.action,
                device.name()
            ),
            "add a row to assets/input/project.glyphs.ron or bind the action for that device; input.rebind_state lists bindings",
        ));
    };
    Ok(json!({
        "action": params.action,
        "device": device.name(),
        "glyph": hit.glyph,
        "source": hit.source.name(),
        "table_rows": table_rows,
    }))
}

#[cfg(test)]
mod tests {
    use regia_input::{ActionKind, Binding, InputSource};
    use regia_ui::input_glyphs::InputGlyphTable;

    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(InputGlyphs::new(
            InputGlyphTable::from_ron(r#"(version: 1, glyphs: { "Fire": { "xbox": "Ⓡ" } })"#).expect("table"),
        ));
        world.insert_resource(
            InputConfig::new()
                .action("Jump", ActionKind::Button)
                .binding(Binding::new("Jump", InputSource::key("Space"))),
        );
        world
    }

    #[test]
    fn table_builtin_and_binding_glyphs() {
        let mut world = world();
        let out = glyph(&mut world, &json!({ "action": "Fire", "device": "xbox" })).expect("table");
        assert_eq!(
            (out["glyph"].clone(), out["source"].clone()),
            (json!("Ⓡ"), json!("table"))
        );
        let out = glyph(&mut world, &json!({ "action": "back", "device": "playstation" })).expect("builtin");
        assert_eq!(
            (out["glyph"].clone(), out["source"].clone()),
            (json!("Circle"), json!("builtin"))
        );
        let out = glyph(&mut world, &json!({ "action": "Jump" })).expect("binding, default device");
        assert_eq!(
            (out["glyph"].clone(), out["device"].clone(), out["source"].clone()),
            (json!("Space"), json!("keyboard_mouse"), json!("binding"))
        );
    }

    #[test]
    fn input_glyph_answers_through_the_dispatcher() {
        let mut world = world();
        world.insert_resource(crate::safety::AgentPermissions {
            config: crate::safety::AgentConfig::fail_closed(),
            source: None,
        });
        let out = crate::dispatch::call_in_process(
            &mut world,
            "input.glyph",
            json!({ "action": "Fire", "device": "xbox" }),
        )
        .expect("synchronous method")
        .expect("glyph");
        assert_eq!(out["glyph"], json!("Ⓡ"));
        assert_eq!(out["source"], json!("table"));
        let err = crate::dispatch::call_in_process(&mut world, "input.glyph", json!({ "action": "Nope" }))
            .expect("synchronous method")
            .expect_err("unknown");
        assert_eq!(err.data.expect("data")["code"], json!("input_glyph_unknown"));
    }

    #[test]
    fn errors_name_their_codes() {
        let mut world = world();
        for (bad, want) in [
            (json!({}), "input_glyph_bad_params"),
            (json!({ "action": "" }), "input_glyph_bad_params"),
            (
                json!({ "action": "Jump", "device": "switch" }),
                "input_glyph_bad_params",
            ),
            (json!({ "action": "Jump", "extra": 1 }), "input_glyph_bad_params"),
            (
                json!({ "action": "Jump", "device": "xbox" }),
                "input_glyph_unknown",
            ),
        ] {
            let err = glyph(&mut world, &bad).expect_err("error");
            assert_eq!(err.data.expect("data")["code"], json!(want), "{bad}");
        }
    }
}
