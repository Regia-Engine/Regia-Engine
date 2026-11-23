// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface (`docs/specs/23-agent-native.md`): `steam.status` reports the shared
//! Steam connection, `steam.input` the Steam Input bridge and `steam.input_manifest` the generated
//! actions file. [`SteamPlugin`](crate::SteamPlugin) installs it on the world's
//! [`AgentExtension`](regia_agent::extension::AgentExtension).

use serde_json::{Value, json};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;

use crate::{Steam, SteamInputSettings, SteamInputStatus, SteamStatus};

/// The `steam.status` schema row.
fn status_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.status",
        tool: "steam_status",
        description: "The shared Steam connection: running, offline, user id, app id, overlay state, Deck flag.",
        params: &[],
        result: "{ plugin, state, running, offline, user_id, app_id, overlay_open, steam_deck }",
        mutating: false,
        permission: Permission::Read,
    }
}

/// The `steam.input` schema row.
fn input_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.input",
        tool: "steam_input",
        description: "The Steam Input bridge: enabled, active, controllers, chosen action set, held actions, UI actions down.",
        params: &[],
        result: "{ plugin, enabled, active, controllers, action_set, driven_actions, ui_down }",
        mutating: false,
        permission: Permission::Read,
    }
}

/// The `steam.input_manifest` schema row.
fn manifest_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.input_manifest",
        tool: "steam_input_manifest",
        description: "The Steam Input in-game actions file generated from the live InputConfig.",
        params: &[],
        result: "{ plugin, ok, manifest?, error? }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_input(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(settings) = world.get_resource::<SteamInputSettings>() else {
        return Ok(json!({ "plugin": false }));
    };
    let enabled = settings.enabled;
    let status = world
        .get_resource::<SteamInputStatus>()
        .cloned()
        .unwrap_or_default();
    Ok(json!({
        "plugin": true,
        "enabled": enabled,
        "active": status.active,
        "controllers": status.controllers,
        "action_set": status.action_set,
        "driven_actions": status.driven_actions,
        "ui_down": status.ui_down,
    }))
}

fn steam_input_manifest(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<Steam>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    let Some(config) = world.get_resource::<regia_input::InputConfig>() else {
        return Ok(json!({ "plugin": true, "ok": false, "error": "the app has no InputConfig" }));
    };
    Ok(match crate::input::action_manifest(config) {
        Ok(manifest) => json!({ "plugin": true, "ok": true, "manifest": manifest }),
        Err(err) => json!({ "plugin": true, "ok": false, "error": err.to_string() }),
    })
}

fn steam_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(steam) = world.get_resource::<Steam>() else {
        return Ok(json!({ "plugin": false }));
    };
    let status = world.get_resource::<SteamStatus>().cloned().unwrap_or_default();
    Ok(json!({
        "plugin": true,
        "state": format!("{:?}", steam.state()),
        "running": status.running,
        "offline": status.offline,
        "user_id": status.user_id.map(|id| id.0.to_string()),
        "app_id": status.app_id.map(|id| id.0),
        "overlay_open": status.overlay_open,
        "steam_deck": steam.is_steam_deck(),
    }))
}

/// The `steam.deck` schema row.
fn deck_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.deck",
        tool: "steam_deck",
        description: "Deck readiness: detection, frame cap, power policy, controller glyph family, keyboard state.",
        params: &[],
        result: "{ plugin, detected, cap_fps, power_cap_fps, power_reason, render_scale_factor, controller, glyph_family, keyboard_open, keyboard_failures }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_deck(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    use crate::deck::{DeckFrameCapSetting, SteamDeck};
    use crate::deck_prep::{ActiveGlyphs, DeckPowerPolicy, KeyboardState};
    if world.get_resource::<Steam>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    let deck = world.get_resource::<SteamDeck>().copied().unwrap_or_default();
    let setting = world
        .get_resource::<DeckFrameCapSetting>()
        .copied()
        .unwrap_or_default();
    let policy = world
        .get_resource::<DeckPowerPolicy>()
        .copied()
        .unwrap_or_default();
    let glyphs = world.get_resource::<ActiveGlyphs>().copied().unwrap_or_default();
    let keyboard = world.get_resource::<KeyboardState>().copied().unwrap_or_default();
    Ok(json!({
        "plugin": true,
        "detected": deck.detected,
        "cap_fps": setting.cap.fps(),
        "power_cap_fps": policy.cap.fps(),
        "power_reason": policy.reason.name(),
        "render_scale_factor": policy.render_scale_factor,
        "controller": format!("{:?}", glyphs.kind),
        "glyph_family": glyphs.family().dir(),
        "keyboard_open": keyboard.open,
        "keyboard_failures": keyboard.failures,
    }))
}

/// The `steam.glyphs` schema row.
fn glyphs_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.glyphs",
        tool: "steam_glyphs",
        description: "Controller prompt glyphs: every button's label and texture key for a family, and the prompts of an action.",
        params: &[
            ParamSchema {
                name: "family",
                kind: "string",
                required: false,
                description: "xbox, playstation, switch or deck; default the family the UI shows now",
            },
            ParamSchema {
                name: "action",
                kind: "string",
                required: false,
                description: "an input action; its gamepad bindings become `prompts`",
            },
            ParamSchema {
                name: "context",
                kind: "string",
                required: false,
                description: "the input context of `action`; default the player's top enabled context, else Gameplay",
            },
        ],
        result: "{ plugin, family, controller, buttons: [{ button, label, texture_key }], context?, action?, prompts? }",
        mutating: false,
        permission: Permission::Read,
    }
}

/// A string parameter. Missing and JSON null are omitted. Any other JSON type is `F0002`.
fn optional_string<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(RpcError::wrong_type(name, "a string")),
    }
}

fn steam_glyphs(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::deck_glyphs::{ALL_BUTTONS, PromptControl, action_prompts, family_from_dir, prompt};
    use crate::deck_prep::ActiveGlyphs;
    if world.get_resource::<Steam>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    let family_name = optional_string(params, "family")?;
    let action = optional_string(params, "action")?;
    let context_param = optional_string(params, "context")?;
    let active = world.get_resource::<ActiveGlyphs>().copied().unwrap_or_default();
    let family = match family_name {
        Some(name) => family_from_dir(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown glyph family `{name}`: use xbox, playstation, switch or deck"
            ))
        })?,
        None => active.family(),
    };
    let buttons: Vec<Value> = ALL_BUTTONS
        .iter()
        .map(|&button| {
            let p = prompt(family, PromptControl::Button(button));
            json!({ "button": format!("{button:?}"), "label": p.label, "texture_key": p.texture_key })
        })
        .collect();
    let mut result = json!({
        "plugin": true,
        "family": family.dir(),
        "controller": format!("{:?}", active.kind),
        "buttons": buttons,
    });
    if let Some(action) = action {
        let player = world
            .get_resource::<SteamInputSettings>()
            .map_or(0, |settings| settings.player);
        let config = world.get_resource::<regia_input::InputConfig>();
        let context = match context_param {
            Some(context) => context.to_owned(),
            None => config
                .zip(world.get_resource::<regia_input::InputContexts>())
                .and_then(|(config, contexts)| crate::input::active_context(config, contexts, player))
                .unwrap_or("Gameplay")
                .to_owned(),
        };
        let prompts: Vec<Value> = config
            .map(|config| action_prompts(config, &context, action, family))
            .unwrap_or_default()
            .into_iter()
            .map(|p| json!({ "label": p.label, "texture_key": p.texture_key }))
            .collect();
        result["context"] = json!(context);
        result["action"] = json!(action);
        result["prompts"] = json!(prompts);
    }
    Ok(result)
}

/// The `steam.deck_perf` schema row.
fn deck_perf_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.deck_perf",
        tool: "steam_deck_perf",
        description: "Deck performance preset in force: frame cap, render budget, power hint, gamescope (FSR 1) scaling.",
        params: &[],
        result: "{ plugin, detected, preset, presets, panel, frame_cap_fps, render_scale, upscaler, csm_cascade_count, csm_resolution, ssao, contact_shadows, bloom_mip_count, local_shadow_max_lights, max_visible_lights, tdp_hint_watts, game_resolution, gamescope, output, compositor_upscales, output_16_10 }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_deck_perf(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    use crate::deck::SteamDeck;
    use crate::deck_perf::{DeckDisplay, DeckPerfPreset, DeckPerfSettings, DeckPerfState};
    if world.get_resource::<Steam>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    let deck = world.get_resource::<SteamDeck>().copied().unwrap_or_default();
    let settings = world
        .get_resource::<DeckPerfSettings>()
        .copied()
        .unwrap_or_else(|| DeckPerfSettings::from_env(|_| None));
    let state = world.get_resource::<DeckPerfState>().copied().unwrap_or_default();
    let display = world
        .get_resource::<DeckDisplay>()
        .copied()
        .unwrap_or_else(|| DeckDisplay::from_env(|_| None));
    let knobs = state.knobs;
    Ok(json!({
        "plugin": true,
        "detected": deck.detected,
        "preset": settings.preset.name(),
        "presets": DeckPerfPreset::ALL.iter().map(|p| p.name()).collect::<Vec<_>>(),
        "panel": format!("{:?}", settings.panel),
        "frame_cap_fps": knobs.frame_cap.fps(),
        "render_scale": knobs.render_scale,
        "upscaler": knobs.upscaler.name(),
        "csm_cascade_count": knobs.csm_cascade_count,
        "csm_resolution": knobs.csm_resolution,
        "ssao": knobs.ssao,
        "contact_shadows": knobs.contact_shadows,
        "bloom_mip_count": knobs.bloom_mip_count,
        "local_shadow_max_lights": knobs.local_shadow_max_lights,
        "max_visible_lights": knobs.max_visible_lights,
        "tdp_hint_watts": knobs.tdp_hint_watts,
        "game_resolution": settings.preset.game_resolution(),
        "gamescope": display.gamescope,
        "output": display.output,
        "compositor_upscales": state.compositor_upscales,
        "output_16_10": display.is_16_10(),
    }))
}

/// The `steam.set_deck_preset` schema row.
fn set_deck_preset_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.set_deck_preset",
        tool: "steam_set_deck_preset",
        description: "Switch the Deck performance preset (battery, balanced, performance, fsr1); on a Deck the budget applies next frame, off a Deck only the stored preset changes.",
        params: &[ParamSchema {
            name: "preset",
            kind: "string",
            required: true,
            description: "battery, balanced, performance or fsr1",
        }],
        result: "{ plugin, preset, previous }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_set_deck_preset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::deck_perf::{DeckPerfPreset, DeckPerfSettings};
    if world.get_resource::<Steam>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    let name = params
        .get("preset")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`preset` (string) is required"))?;
    let preset = DeckPerfPreset::from_name(name).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "unknown preset `{name}`: use battery, balanced, performance or fsr1"
        ))
    })?;
    let mut settings = world.get_resource_or_insert_with(|| DeckPerfSettings::from_env(|_| None));
    let previous = settings.preset;
    if previous != preset {
        settings.preset = preset;
    }
    Ok(json!({ "plugin": true, "preset": preset.name(), "previous": previous.name() }))
}

/// The `steam.layout` schema row.
fn layout_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.layout",
        tool: "steam_layout",
        description: "Controller layout: the controls screen of each input context for a glyph family, and the controller-only check (no_gamepad_binding, reserved_control, control_conflict).",
        params: &[
            ParamSchema {
                name: "family",
                kind: "string",
                required: false,
                description: "xbox, playstation, switch or deck; default the family the UI shows now",
            },
            ParamSchema {
                name: "context",
                kind: "string",
                required: false,
                description: "one input context; default every context, highest priority first",
            },
            ParamSchema {
                name: "keyboard_only_ok",
                kind: "array",
                required: false,
                description: "action names allowed to have no gamepad binding (a debug console key)",
            },
        ],
        result: "{ plugin, ok, family, sheets: [{ context, rows: [{ control, label, texture_key, actions }], keyboard_only }], issues: [{ severity, code, context, action, control, message }], errors, error? }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_layout(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::deck_glyphs::family_from_dir;
    use crate::deck_layout::{LayoutCheck, check_layout, control_id, layout_sheet, layout_sheets};
    use crate::deck_prep::ActiveGlyphs;
    if world.get_resource::<Steam>().is_none() {
        return Ok(json!({ "plugin": false }));
    }
    // Wrong JSON types are `F0002`, as `steam.glyphs` answers (rc1-rev-net-k 9); null is omitted.
    let family_name = optional_string(params, "family")?;
    let context_param = optional_string(params, "context")?;
    let family = match family_name {
        Some(name) => family_from_dir(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown glyph family `{name}`: use xbox, playstation, switch or deck"
            ))
        })?,
        None => world
            .get_resource::<ActiveGlyphs>()
            .copied()
            .unwrap_or_default()
            .family(),
    };
    let keyboard_only_ok: Vec<String> = match params.get("keyboard_only_ok") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params("`keyboard_only_ok` is a list of action names"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`keyboard_only_ok` is a list of action names",
            ));
        }
    };
    let Some(config) = world.get_resource::<regia_input::InputConfig>() else {
        return Ok(
            json!({ "plugin": true, "ok": false, "family": family.dir(), "error": "the app has no InputConfig" }),
        );
    };
    let sheets = match context_param {
        Some(context) => vec![
            layout_sheet(config, context, family)
                .ok_or_else(|| RpcError::invalid_params(format!("unknown input context `{context}`")))?,
        ],
        None => layout_sheets(config, family),
    };
    let check = LayoutCheck {
        keyboard_only_ok,
        glyph_root: None,
    };
    let issues = check_layout(config, &check);
    let sheets: Vec<Value> = sheets
        .iter()
        .map(|sheet| {
            let rows: Vec<Value> = sheet
                .rows
                .iter()
                .map(|row| {
                    json!({
                        "control": control_id(row.prompt.control),
                        "label": row.prompt.label,
                        "texture_key": row.prompt.texture_key,
                        "actions": row.actions,
                    })
                })
                .collect();
            json!({ "context": sheet.context, "rows": rows, "keyboard_only": sheet.keyboard_only })
        })
        .collect();
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == crate::deck_layout::LayoutSeverity::Error)
        .count();
    let issues: Vec<Value> = issues
        .iter()
        .map(|issue| {
            json!({
                "severity": issue.severity.name(),
                "code": issue.code,
                "context": issue.context,
                "action": issue.action,
                "control": issue.control,
                "message": issue.message,
            })
        })
        .collect();
    Ok(json!({
        "plugin": true,
        "ok": true,
        "family": family.dir(),
        "sheets": sheets,
        "issues": issues,
        "errors": errors,
    }))
}

/// Registers `steam.status` on the world's `AgentExtension` (creating it when absent).
pub(crate) fn install(world: &mut World) {
    let mut extension = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(status_schema(), steam_status);
    extension.register(input_schema(), steam_input);
    extension.register(manifest_schema(), steam_input_manifest);
    extension.register(deck_schema(), steam_deck);
    extension.register(glyphs_schema(), steam_glyphs);
    extension.register(deck_perf_schema(), steam_deck_perf);
    extension.register(set_deck_preset_schema(), steam_set_deck_preset);
    // Leaderboards, ownership and timeline (ADR-2340).
    crate::agent_services::register(&mut extension);
    extension.register(layout_schema(), steam_layout);
    world.insert_resource(extension);
}
