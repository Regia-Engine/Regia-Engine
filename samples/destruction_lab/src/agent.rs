// SPDX-License-Identifier: MIT-0
//! The lab's agent methods. The built-in methods see client 0; these reach all three processes:
//! `lab.fire` sets off a blast on the server and `lab.report` reads every process's playback, so an
//! agent can check that both clients show the same thing.

use regia::agent::extension::AgentExtension;
use regia::agent::protocol::RpcError;
use regia::agent::schema::{MethodSchema, ParamSchema, Permission};
use regia::core::math::Vec3;
use regia::framework::explosion::Explosion;
use regia::prelude::*;
use serde_json::{Value, json};

use crate::{LabSession, lab_report};

fn no_lab() -> RpcError {
    RpcError::not_found(
        "no lab in this app",
        "build the app with destruction_lab::lab_app",
    )
}

fn refuse(reason: &str, message: impl Into<String>) -> RpcError {
    let mut err = RpcError::invalid_params(message);
    if let Some(data) = err.data.as_mut() {
        data["reason"] = json!(reason);
    }
    err
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if (n as f32).is_finite() => Ok(Some(n as f32)),
            _ => Err(refuse(
                "lab_bad_param",
                format!("`{name}` must be a finite number"),
            )),
        },
    }
}

/// `lab.fire { center, preset?, radius?, impulse?, fx? }`: an explosion on the server, on the next
/// tick.
fn lab_fire(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let center = params
        .get("center")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            Some(Vec3::new(
                a[0].as_f64()? as f32,
                a[1].as_f64()? as f32,
                a[2].as_f64()? as f32,
            ))
        })
        .filter(|c| c.is_finite())
        .ok_or_else(|| refuse("bad_center", "`center` must be [x, y, z], three finite numbers"))?;
    let preset = params.get("preset").and_then(Value::as_str).unwrap_or("barrel");
    let mut explosion = Explosion::preset(preset, center).map_err(|e| refuse(e.code(), e.to_string()))?;
    if let Some(radius) = number(params, "radius")? {
        explosion.radius = radius;
    }
    if let Some(impulse) = number(params, "impulse")? {
        explosion.impulse = impulse;
    }
    match params.get("fx") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(fx)) => explosion.fx = *fx,
        Some(_) => return Err(refuse("lab_bad_param", "`fx` must be true or false")),
    }
    explosion
        .validate()
        .map_err(|e| refuse(e.code(), e.to_string()))?;
    let Some(mut lab) = world.get_non_send_mut::<LabSession>() else {
        return Err(no_lab());
    };
    lab.fire(explosion);
    Ok(json!({ "queued_for_tick": lab.ticks() + 1 }))
}

/// `lab.report`: what the three processes play.
fn lab_report_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let report = lab_report(world).ok_or_else(no_lab)?;
    serde_json::to_value(&report).map_err(|e| refuse("lab_report", e.to_string()))
}

/// Registers `lab.fire` and `lab.report` with the agent. Call it before the first frame.
pub fn install_lab_methods(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(
        MethodSchema {
            name: "lab.fire",
            tool: "lab_fire",
            description: "Set off an explosion on the lab's server on its next tick.",
            params: &[
                ParamSchema {
                    name: "center",
                    kind: "array",
                    required: true,
                    description: "[x, y, z] in meters.",
                },
                ParamSchema {
                    name: "preset",
                    kind: "string",
                    required: false,
                    description: "grenade, barrel (default), bomb or shell.",
                },
                ParamSchema {
                    name: "radius",
                    kind: "number",
                    required: false,
                    description: "Reach in meters, instead of the preset's.",
                },
                ParamSchema {
                    name: "impulse",
                    kind: "number",
                    required: false,
                    description: "Peak impulse in newton-seconds, instead of the preset's.",
                },
                ParamSchema {
                    name: "fx",
                    kind: "boolean",
                    required: false,
                    description: "Play the fire and smoke (default true).",
                },
            ],
            result: "{ queued_for_tick }",
            mutating: true,
            permission: Permission::Play,
        },
        lab_fire,
    );
    extension.register(
        MethodSchema {
            name: "lab.report",
            tool: "lab_report",
            description: "What the lab's server and two clients play: broken bonds, chunk pose digest, stream state.",
            params: &[],
            result: "{ ticks, server: <process>, clients: [<process>, <process>], clients_agree } where <process> is { playback, in_step, checks_failed, explosions, chunks_pushed, chunks_hit }",
            mutating: false,
            permission: Permission::Read,
        },
        lab_report_method,
    );
    world.insert_resource(extension);
}
