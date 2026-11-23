// SPDX-License-Identifier: MPL-2.0
//! Editor controls share these handlers with the agent dispatcher.

use regia_agent::extension::AgentHandler;
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use super::*;

fn error(code: &str, message: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        code,
        message,
        "Read book/agents/collaboration.md and refresh the participant list.",
    )
}

fn fields(params: &Value, allowed: &[&str]) -> Result<(), RpcError> {
    let Some(object) = params.as_object() else {
        return Err(error("collab_bad_params", "Expected an object."));
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(error("collab_bad_params", "Unknown collaboration parameter."));
    }
    Ok(())
}

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| error("collab_bad_params", &format!("{key} must be a string.")))
}

fn name(params: &Value) -> Result<String, RpcError> {
    let name = text(params, "name")?;
    if !valid_name(name) {
        return Err(error(
            "collab_bad_params",
            "Name needs 1–64 bytes and no control characters.",
        ));
    }
    Ok(name.to_owned())
}

fn address(params: &Value, joining: bool) -> Result<SocketAddr, RpcError> {
    let parsed = text(params, "address")?
        .parse::<SocketAddr>()
        .map_err(|_| error("collab_bad_address", "Use a numeric LAN IP address and port."))?;
    if !transport::lan_ip(parsed.ip()) || (joining && parsed.port() == 0) {
        return Err(error(
            "collab_bad_address",
            "Use a private, link-local or loopback address; join needs a nonzero port.",
        ));
    }
    Ok(parsed)
}

fn idle(world: &World) -> Result<(), RpcError> {
    if world
        .get_resource::<Session>()
        .is_some_and(|s| !matches!(s.role, Role::Off))
    {
        Err(error("collab_busy", "Leave the current session first."))
    } else {
        Ok(())
    }
}

fn participant(params: &Value) -> Result<u64, RpcError> {
    params
        .get("participant")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| error("collab_bad_params", "participant must be a positive integer."))
}

pub(super) fn host(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    fields(params, &["action", "address", "name", "participant"])?;
    let action = match params.get("action") {
        None => "start",
        Some(Value::String(action)) => action.as_str(),
        _ => return Err(error("collab_bad_params", "action must be a string.")),
    };
    match action {
        "start" => {
            if params.get("participant").is_some() {
                return Err(error("collab_bad_params", "start does not take participant."));
            }
            let name = name(params)?;
            let address = address(params, false)?;
            idle(world)?;
            let transport = Transport::bind(address).map_err(|e| error("collab_io", &e.to_string()))?;
            world.insert_resource(Session::host(transport, name));
        }
        "accept" | "reject" => {
            if params.get("name").is_some() || params.get("address").is_some() {
                return Err(error(
                    "collab_bad_params",
                    "accept/reject takes participant only.",
                ));
            }
            let id = participant(params)?;
            let mut session = world
                .get_resource_mut::<Session>()
                .ok_or_else(|| error("collab_not_host", "Only the host accepts or rejects joiners."))?;
            let Role::Host(host) = &mut session.role else {
                return Err(error(
                    "collab_not_host",
                    "Only the host accepts or rejects joiners.",
                ));
            };
            let used: Vec<_> = std::iter::once(host.own.color)
                .chain(
                    host.peers
                        .values()
                        .filter(|p| p.admission == Admission::Accepted)
                        .map(|p| p.participant.color),
                )
                .collect();
            let peer = host
                .peers
                .values_mut()
                .find(|p| p.participant.id == id && p.admission == Admission::Pending)
                .ok_or_else(|| error("collab_not_pending", "That participant is not pending."))?;
            if action == "accept" {
                let color = (0..MAX_PARTICIPANTS as u8)
                    .find(|slot| !used.contains(slot))
                    .ok_or_else(|| {
                        error(
                            "collab_full",
                            "The session already has eight accepted participants.",
                        )
                    })?;
                peer.participant.color = color;
                peer.admission = Admission::Accepted;
            } else {
                peer.admission = Admission::Rejected;
            }
        }
        _ => {
            return Err(error(
                "collab_bad_params",
                "action must be start, accept or reject.",
            ));
        }
    }
    participants(world, &json!({}))
}

pub(super) fn join(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    fields(params, &["address", "name"])?;
    let name = name(params)?;
    let remote = address(params, true)?;
    idle(world)?;
    let local = if remote.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let transport = Transport::bind(local).map_err(|e| error("collab_io", &e.to_string()))?;
    world.insert_resource(Session::join(transport, remote, name));
    participants(world, &json!({}))
}

pub(super) fn leave(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    fields(params, &[])?;
    if let Some(mut session) = world.get_resource_mut::<Session>() {
        session.leave();
    }
    participants(world, &json!({}))
}

pub(super) fn participants(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    fields(params, &[])?;
    Ok(status(world))
}

pub(super) fn status(world: &World) -> Value {
    let Some(session) = world.get_resource::<Session>() else {
        return json!({"applied":false,"state":"off","address":null,"self_id":null,"participants":[],"pending":[],"error":null,"sent":0,"received":0,"invalid":0});
    };
    let state = match &session.role {
        Role::Off => "off",
        Role::Host(_) => "hosting",
        Role::Client(c) if c.id.is_some() => "joined",
        Role::Client(_) => "joining",
    };
    json!({"applied":false,"state":state,"address":session.transport.as_ref().and_then(|t| t.address().ok()).map(|a| a.to_string()),
        "self_id":session.self_id(),"participants":session.participants(),"pending":session.pending(),
        "error":session.error,"sent":session.sent,"received":session.received,"invalid":session.invalid})
}

pub(super) fn jump(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    fields(params, &["participant"])?;
    let id = participant(params)?;
    let remote = world
        .get_resource::<Session>()
        .and_then(|s| s.remote_participants().into_iter().find(|p| p.id == id))
        .ok_or_else(|| {
            error(
                "collab_unknown_participant",
                "Jump requires a current accepted remote participant.",
            )
        })?;
    let mut layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let active = layout.active.min(3);
    let (_, old) = layout.pane_camera(world, active);
    let kind = ViewKind::from_name(&remote.presence.camera.kind)
        .ok_or_else(|| error("collab_bad_params", "Invalid remote camera kind."))?;
    layout.panes[active].kind = kind;
    layout.set_pane_camera(world, active, remote.presence.camera.camera(old.speed));
    world.insert_resource(layout);
    if let Some(mut eases) = world.get_resource_mut::<crate::viewport_ops::CameraEases>() {
        eases.moves.remove(&crate::viewport_ops::MAIN_CAMERA);
        eases.moves.remove(&(0, active));
    }
    Ok(json!({"applied":false,"participant":id,"camera":remote.presence.camera}))
}

const START_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "action",
        kind: "string",
        required: false,
        description: "start (default), accept or reject.",
    },
    ParamSchema {
        name: "address",
        kind: "string",
        required: false,
        description: "Numeric LAN IP:port; required for start.",
    },
    ParamSchema {
        name: "name",
        kind: "string",
        required: false,
        description: "Display name, 1–64 bytes; required for start.",
    },
    ParamSchema {
        name: "participant",
        kind: "integer",
        required: false,
        description: "Pending participant id; required for accept/reject.",
    },
];
const JOIN_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "address",
        kind: "string",
        required: true,
        description: "Numeric LAN host IP:port; nonzero port.",
    },
    ParamSchema {
        name: "name",
        kind: "string",
        required: true,
        description: "Display name, 1–64 bytes without control characters.",
    },
];
const JUMP_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "participant",
    kind: "integer",
    required: true,
    description: "Accepted remote participant id.",
}];

/// One collaboration method's schema (`name` is its protocol name). Session controls spend the
/// mutation budget but report no scene change.
fn schema(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    permission: Permission,
) -> MethodSchema {
    let result = if name == "editor.collab_jump" {
        "{ applied: false, participant, camera }"
    } else {
        "{ applied: false, state, address, self_id, participants, pending, error, sent, received, invalid }"
    };
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating: permission != Permission::Read,
        permission,
    }
}

/// Collaboration method schemas and handlers for the editor's ordinary agent dispatcher.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, AgentHandler)> {
    let entries: [(MethodSchema, AgentHandler); 5] = [
        (
            schema(
                "editor.collab_host",
                "collab_host",
                "Host LAN presence or accept/reject a pending participant.",
                START_PARAMS,
                Permission::EditScene,
            ),
            host,
        ),
        (
            schema(
                "editor.collab_join",
                "collab_join",
                "Request admission to a LAN presence session.",
                JOIN_PARAMS,
                Permission::EditScene,
            ),
            join,
        ),
        (
            schema(
                "editor.collab_leave",
                "collab_leave",
                "Leave presence and clear indicators. Idempotent.",
                &[],
                Permission::EditScene,
            ),
            leave,
        ),
        (
            schema(
                "editor.collab_participants",
                "collab_participants",
                "Read participants, pending joins and transport status without network I/O.",
                &[],
                Permission::Read,
            ),
            participants,
        ),
        (
            schema(
                "editor.collab_jump",
                "collab_jump",
                "Copy an accepted participant's view to the main active viewport.",
                JUMP_PARAMS,
                Permission::EditScene,
            ),
            jump,
        ),
    ];
    entries.into()
}
