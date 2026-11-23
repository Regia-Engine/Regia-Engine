// SPDX-License-Identifier: MPL-2.0
//! `input.pointer_claims`: the mouse buttons the runtime UI has claimed from press to release,
//! and a check that a click on a UI node left the named gameplay actions untriggered (book
//! `agents/ui-input.md` "Pointer claims", ADR-7976, `docs/specs/18-runtime-ui.md` § 4).
//!
//! A watch counts, per named action, the frames on which gameplay saw it (any phase but idle, a
//! press edge, or held). The check passes when every named action stayed at zero since the watch
//! was armed. The counter runs in `Last`, once per frame, only while a watch is armed.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_input::{ActionPhase, Actions, InputConfig, PointerClaims, mouse_button_name};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// The armed watch: action name → frames gameplay saw it since the watch was armed.
#[derive(Resource, Debug, Default, Clone)]
pub(crate) struct PointerClaimWatch {
    triggered_frames: BTreeMap<String, u64>,
    frames: u64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Params {
    /// Turn claims on or off.
    #[serde(default)]
    enabled: Option<bool>,
    /// Arm a fresh watch on these actions (replaces any earlier watch).
    #[serde(default)]
    watch: Option<Vec<String>>,
    /// Check these watched actions stayed untriggered since the watch was armed.
    #[serde(default)]
    expect_untriggered: Option<Vec<String>>,
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "pointer_claims_bad_params",
        format!("bad `input.pointer_claims` parameters: {detail}"),
        "pass { enabled?: bool, watch?: [\"<action>\"], expect_untriggered?: [\"<action>\"] }",
    )
}

/// `Last`: one frame of every armed watch.
pub(crate) fn record_pointer_claim_watch(
    mut watch: ResMut<PointerClaimWatch>,
    actions: Option<Res<Actions>>,
) {
    let Some(actions) = actions else {
        return;
    };
    watch.frames += 1;
    for (name, frames) in &mut watch.triggered_frames {
        if actions.phase(name) != ActionPhase::Idle
            || actions.just_pressed_this_frame(name)
            || actions.pressed(name)
        {
            *frames += 1;
        }
    }
}

fn snapshot(world: &World) -> Value {
    let claims = world.get_resource::<PointerClaims>();
    let buttons: Vec<String> = claims
        .map(|c| c.claimed().iter().map(|b| mouse_button_name(*b)).collect())
        .unwrap_or_default();
    let watch = world.get_resource::<PointerClaimWatch>().map(|w| {
        let actions: serde_json::Map<String, Value> = w
            .triggered_frames
            .iter()
            .map(|(name, frames)| (name.clone(), json!({ "triggered_frames": frames })))
            .collect();
        json!({ "frames": w.frames, "actions": actions })
    });
    json!({
        "enabled": claims.is_some_and(PointerClaims::enabled),
        "ui_claimed_buttons": buttons,
        "claimed_presses": claims.map_or(0, PointerClaims::claimed_presses),
        "watch": watch,
    })
}

/// `input.pointer_claims { enabled?, watch?, expect_untriggered? }`: with no parameters it only
/// reports. `enabled` changes first, then `watch` arms, then `expect_untriggered` checks.
pub(crate) fn pointer_claims(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let params: Params = match params {
        Value::Null => Params::default(),
        Value::Object(_) => serde_json::from_value(params.clone()).map_err(bad_params)?,
        _ => return Err(bad_params("expected an object")),
    };
    if !world.contains_resource::<PointerClaims>() {
        return Err(RpcError::new(
            NOT_FOUND,
            "pointer_claims_not_installed",
            "no PointerClaims resource: the app has no InputPlugin",
            "add regia_input::InputPlugin (and UiPlugin, which writes the claims)",
        ));
    }
    // Validate everything before changing anything.
    if let Some(watch) = &params.watch {
        if watch.is_empty() {
            return Err(bad_params("`watch` names no action"));
        }
        if let Some(config) = world.get_resource::<InputConfig>()
            && let Some(unknown) = watch.iter().find(|a| !config.actions.contains_key(*a))
        {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "pointer_claims_unknown_action",
                format!("the input config declares no action `{unknown}`"),
                "use a declared action name; input.rebind_state lists them",
            ));
        }
    }
    if let Some(expect) = &params.expect_untriggered {
        if expect.is_empty() {
            return Err(bad_params("`expect_untriggered` names no action"));
        }
        let armed = |name: &String| {
            params.watch.as_ref().is_some_and(|w| w.contains(name))
                || world
                    .get_resource::<PointerClaimWatch>()
                    .is_some_and(|w| w.triggered_frames.contains_key(name))
        };
        if let Some(missing) = expect.iter().find(|name| !armed(name)) {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "pointer_claims_not_watched",
                format!("`{missing}` is not watched"),
                "arm a watch first: input.pointer_claims { watch: [\"<action>\"] }, then click, then check",
            ));
        }
    }
    if let Some(enabled) = params.enabled {
        world.resource_mut::<PointerClaims>().set_enabled(enabled);
    }
    if let Some(watch) = params.watch {
        world.insert_resource(PointerClaimWatch {
            triggered_frames: watch.into_iter().map(|a| (a, 0)).collect(),
            frames: 0,
        });
    }
    let mut out = snapshot(world);
    if let Some(expect) = params.expect_untriggered {
        let watch = world.get_resource::<PointerClaimWatch>();
        let triggered: serde_json::Map<String, Value> = expect
            .iter()
            .filter_map(|name| {
                let frames = watch
                    .and_then(|w| w.triggered_frames.get(name))
                    .copied()
                    .unwrap_or(0);
                (frames > 0).then(|| (name.clone(), json!(frames)))
            })
            .collect();
        out["check"] = json!({ "passed": triggered.is_empty(), "triggered": triggered });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_app::TimeUpdateStrategy;
    use regia_app::prelude::*;
    use regia_input::prelude::*;
    use regia_platform::MouseButton;
    use regia_ui::{ComputedNode, Interactable, UiPlugin};

    use super::*;
    use crate::ui::UiAgentHost;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )))
            .add_plugins(UiPlugin::code_only())
            .add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::ImeEvent>();
        app.add_plugins(InputPlugin::new(
            InputConfig::new()
                .action("Fire", ActionKind::Button)
                .binding(Binding::new("Fire", InputSource::mouse_button(MouseButton::Left))),
        ));
        // `build` only: `finish` would start the agent server.
        Plugin::build(&crate::AgentPlugin, &mut app);
        app.insert_resource(crate::safety::AgentPermissions {
            config: crate::safety::AgentConfig::default(),
            source: None,
        });
        app.insert_resource(UiAgentHost::new(regia_ui::agent_bridge::host_call));
        app.update();
        app
    }

    /// One request through the dispatcher in a real frame; the answer is read after the frame.
    fn call(app: &mut App, method: &str, params: Value) -> crate::protocol::Response {
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::bounded(1);
        inbox_tx
            .send(crate::server::Inbound {
                request: crate::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: tx,
            })
            .expect("queued");
        app.insert_resource(crate::AgentInbox(inbox_rx));
        app.update();
        rx.try_recv().expect("answered in the frame")
    }

    fn ok(app: &mut App, method: &str, params: Value) -> Value {
        let response = call(app, method, params);
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    }

    fn code(response: &crate::protocol::Response) -> String {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// A laid-out 100 x 100 px clickable tile at the window's top-left corner.
    fn widget(app: &mut App) -> Entity {
        use regia_ui::{Dimension, NodeStyle, Style, UiNode, UiRoot};
        let root = app
            .world_mut()
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    ..Default::default()
                }),
            ))
            .id();
        let tile = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(100.0), Dimension::Points(100.0)],
                    flex_shrink: 0.0,
                    ..Default::default()
                }),
                Interactable::default(),
            ))
            .id();
        app.world_mut().entity_mut(root).add_child(tile);
        for _ in 0..4 {
            app.update();
        }
        // The default scale mode shrinks the 100 UI px tile in the headless window; it stays at
        // the corner and well inside x < 300, where the world presses below land.
        let rect = app
            .world()
            .get::<ComputedNode>(tile)
            .map(|c| c.rect)
            .unwrap_or_default();
        assert_eq!([rect[0], rect[1]], [0.0, 0.0], "laid out at the corner: {rect:?}");
        assert!(rect[2] > 50.0 && rect[3] > 50.0 && rect[2] < 300.0, "{rect:?}");
        tile
    }

    #[test]
    fn a_ui_click_leaves_the_watched_action_untriggered() {
        let mut app = app();
        let tile = widget(&mut app);
        let report = ok(&mut app, "input.pointer_claims", json!({}));
        assert_eq!(report["enabled"], true, "on by default with UiPlugin");
        assert_eq!(report["ui_claimed_buttons"], json!([]));
        ok(&mut app, "input.pointer_claims", json!({ "watch": ["Fire"] }));
        // A real click: cursor, press, release through the platform messages.
        ok(&mut app, "ui.click", json!({ "node": tile.to_bits() }));
        for _ in 0..6 {
            app.update();
        }
        let report = ok(
            &mut app,
            "input.pointer_claims",
            json!({ "expect_untriggered": ["Fire"] }),
        );
        assert_eq!(report["check"]["passed"], true, "{report}");
        assert_eq!(report["claimed_presses"], 1, "{report}");
        assert_eq!(
            report["ui_claimed_buttons"],
            json!([]),
            "released after the click"
        );
    }

    #[test]
    fn a_held_claim_is_reported_and_a_world_press_fails_the_check() {
        let mut app = app();
        widget(&mut app);
        let cursor = |app: &mut App, x: f64| {
            app.world_mut().write_message(regia_platform::CursorMoved {
                window: Entity::PLACEHOLDER,
                position: (x, 50.0),
                timestamp: Default::default(),
            });
        };
        let button = |app: &mut App, state: regia_platform::ButtonState| {
            app.world_mut().write_message(regia_platform::MouseButtonInput {
                window: Entity::PLACEHOLDER,
                button: MouseButton::Left,
                state,
                timestamp: Default::default(),
            });
        };
        ok(&mut app, "input.pointer_claims", json!({ "watch": ["Fire"] }));
        cursor(&mut app, 50.0);
        app.update();
        button(&mut app, regia_platform::ButtonState::Pressed);
        app.update();
        // Still held on the next frame: the claim stays whatever order the request runs in.
        let held = ok(&mut app, "input.pointer_claims", json!({}));
        assert_eq!(held["ui_claimed_buttons"], json!(["left"]), "{held}");
        button(&mut app, regia_platform::ButtonState::Released);
        app.update();
        // Now a press on empty screen: gameplay fires and the check fails.
        cursor(&mut app, 300.0);
        app.update();
        button(&mut app, regia_platform::ButtonState::Pressed);
        app.update();
        let report = ok(
            &mut app,
            "input.pointer_claims",
            json!({ "expect_untriggered": ["Fire"] }),
        );
        assert_eq!(report["check"]["passed"], false, "{report}");
        assert!(
            report["check"]["triggered"]["Fire"]
                .as_u64()
                .is_some_and(|n| n > 0)
        );
    }

    #[test]
    fn disabling_claims_gives_the_mouse_back_to_gameplay() {
        let mut app = app();
        let tile = widget(&mut app);
        // Since integ-v06s-editor's merge of the claims lane (d20066201) action evaluation honors both
        // ADR-7070's `UiPointerGate` (a click the UI took is hidden from every context it does not
        // exempt) and the claims; `set_enabled(false)` restores the gate's reading (ADR-7976). With
        // Gameplay exempt from the gate, the claims are the only thing between the click and `Fire`.
        app.world_mut()
            .resource_mut::<regia_input::UiPointerGate>()
            .exempt_contexts
            .push(regia_input::GAMEPLAY_CONTEXT.to_owned());
        // Claims on: even the gate-exempt context does not see the claimed click.
        ok(&mut app, "input.pointer_claims", json!({ "watch": ["Fire"] }));
        ok(&mut app, "ui.click", json!({ "node": tile.to_bits() }));
        for _ in 0..6 {
            app.update();
        }
        let on = ok(
            &mut app,
            "input.pointer_claims",
            json!({ "expect_untriggered": ["Fire"] }),
        );
        assert_eq!(on["check"]["passed"], true, "claims on hide the click: {on}");
        assert_eq!(on["claimed_presses"], 1, "{on}");
        let off = ok(
            &mut app,
            "input.pointer_claims",
            json!({ "enabled": false, "watch": ["Fire"] }),
        );
        assert_eq!(off["enabled"], false);
        ok(&mut app, "ui.click", json!({ "node": tile.to_bits() }));
        for _ in 0..6 {
            app.update();
        }
        let report = ok(
            &mut app,
            "input.pointer_claims",
            json!({ "expect_untriggered": ["Fire"] }),
        );
        assert_eq!(
            report["check"]["passed"], false,
            "with claims off the click fires: {report}"
        );
        assert_eq!(report["claimed_presses"], 1, "no new claim while off: {report}");
    }

    #[test]
    fn bad_calls_name_their_error_codes() {
        let mut app = app();
        let response = call(&mut app, "input.pointer_claims", json!({ "watch": ["Jump"] }));
        assert_eq!(code(&response), "pointer_claims_unknown_action");
        let response = call(
            &mut app,
            "input.pointer_claims",
            json!({ "expect_untriggered": ["Fire"] }),
        );
        assert_eq!(code(&response), "pointer_claims_not_watched");
        let response = call(&mut app, "input.pointer_claims", json!({ "watch": [] }));
        assert_eq!(code(&response), "pointer_claims_bad_params");
        let response = call(&mut app, "input.pointer_claims", json!({ "bogus": 1 }));
        assert!(response.error.is_some(), "unknown parameters are refused");
        // A non-object is refused by the dispatcher before the handler (generic invalid params).
        let response = call(&mut app, "input.pointer_claims", json!([1]));
        assert_eq!(
            response.error.map(|e| e.code),
            Some(crate::protocol::INVALID_PARAMS)
        );

        let mut world = World::new();
        let error = pointer_claims(&mut world, &json!({})).expect_err("no input plugin");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("pointer_claims_not_installed")
        );
    }
}
