// SPDX-License-Identifier: MPL-2.0
//! Agent methods `lobby.hub_status` (`book/agents/net-lobby-hub.md`, ADR-8559) and
//! `lobby.hub_join_status` (`book/agents/net-lobby-hub-join.md`, ADR-8607).
//!
//! The handler reads the hub reports a process publishes into `regia_net::NetAgentState`
//! (`HubHost::publish`, `NetAgentState::publish_hub`): sessions with their presence, listed lobbies,
//! open invites, the directory and presence generations, the core's state bytes, bytes sent by kind
//! and refusals by code. It opens no socket. Without the resource or a report the answer is an
//! empty list. Session addresses are never in a report.

use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::lobby::hub::HubReport;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn bad_params(message: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "hub_bad_params",
        message.to_owned(),
        "pass {} for every hub, or { \"hub\": \"<label>\" }",
    )
}

/// Reads `{}` or `{hub: label}` for `method`; the label must be published.
fn label<'a>(world: &'a World, method: &str, params: &'a Value) -> Result<Option<&'a str>, RpcError> {
    let label = match params {
        Value::Null => None,
        Value::Object(map) => {
            if let Some(key) = map.keys().find(|k| k.as_str() != "hub") {
                return Err(bad_params(&format!("{method}: unknown parameter `{key}`")));
            }
            match map.get("hub") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
                Some(_) => return Err(bad_params(&format!("{method}: `hub` must be a non-empty string"))),
            }
        }
        _ => return Err(bad_params(&format!("{method} takes an object"))),
    };
    let hubs = world.get_resource::<NetAgentState>().map(NetAgentState::hubs);
    if let Some(label) = label
        && !hubs.is_some_and(|h| h.contains_key(label))
    {
        let known: Vec<&str> = hubs.map_or_else(Vec::new, |h| h.keys().map(String::as_str).collect());
        return Err(RpcError::new(
            NOT_FOUND,
            "hub_unknown",
            format!("no hub report named `{label}`"),
            format!("published hubs: {known:?} ({method} with {{}} lists them)"),
        ));
    }
    Ok(label)
}

/// Every published report (or the labelled one) through `f`.
fn each(world: &World, label: Option<&str>, f: impl Fn(&str, &HubReport) -> Value) -> Vec<Value> {
    world
        .get_resource::<NetAgentState>()
        .map(|state| {
            state
                .hubs()
                .iter()
                .filter(|(l, _)| label.is_none_or(|t| t == l.as_str()))
                .map(|(l, report)| f(l, report))
                .collect()
        })
        .unwrap_or_default()
}

/// `lobby.hub_status`: every published hub report, or the one named by `hub`.
pub fn hub_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = label(world, "lobby.hub_status", params)?;
    Ok(json!({ "hubs": each(world, label, report_json) }))
}

/// `lobby.hub_join_status`: per published hub, every brokered join (the path planned and chosen per
/// member, punch attempts, relay use) and the identity refusals.
pub fn hub_join_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = label(world, "lobby.hub_join_status", params)?;
    Ok(json!({ "hubs": each(world, label, join_json) }))
}

/// One hub's joins as JSON. Member and lobby ids are decimal strings.
pub fn join_json(label: &str, r: &HubReport) -> Value {
    let joins: Vec<Value> = r
        .joins
        .iter()
        .map(|j| {
            json!({
                "lobby": j.lobby.0.to_string(),
                "member": j.member.0.to_string(),
                "handle": j.handle,
                "identity": j.identity,
                "platform": j.platform,
                "nat": j.nat,
                "state": j.state,
                "planned": j.planned.map(|p| p.as_str()),
                "path": j.chosen.map(|p| p.as_str()),
                "punch_attempts": j.punch_attempts,
                "relayed": j.relayed,
            })
        })
        .collect();
    let count = |kind: &str| {
        r.joins
            .iter()
            .filter(|j| j.chosen.map(|p| p.as_str()) == Some(kind))
            .count()
    };
    json!({
        "label": label,
        "joins": joins,
        "paths": { "mapped": count("mapped"), "punch": count("punch"), "relay": count("relay") },
        "identity_mismatches": r.refused.get("hub_identity_mismatch").copied().unwrap_or(0),
        "identity_in_use": r.refused.get("hub_identity_in_use").copied().unwrap_or(0),
    })
}

/// One hub report as JSON. Member and lobby ids are decimal strings (64-bit, as `lobby.*`).
pub fn report_json(label: &str, r: &HubReport) -> Value {
    let players: Vec<Value> = r
        .players
        .iter()
        .map(|p| {
            json!({
                "member": p.member.0.to_string(),
                "handle": p.handle,
                "presence": p.presence.name(),
                "lobby": p.lobby.map(|l| l.0.to_string()),
            })
        })
        .collect();
    let games: Vec<Value> = r
        .games
        .iter()
        .map(|g| {
            json!({
                "id": g.id.0.to_string(),
                "owner": g.owner.0.to_string(),
                "owner_handle": g.owner_handle,
                "visibility": g.visibility.name(),
                "max_members": g.max_members,
                "member_count": g.member_count,
                "joinable": g.joinable,
                "data": g.data,
            })
        })
        .collect();
    json!({
        "label": label,
        "sessions": r.sessions,
        "lobbies": r.lobbies,
        "listed": r.listed,
        "invites": r.invites,
        "generations": {
            "directory": r.directory_generation,
            "presence": r.presence_generation,
        },
        "state_bytes": r.state_bytes,
        "bytes_sent": {
            "directory": r.directory_bytes,
            "chat": r.chat_bytes,
            "other": r.other_bytes,
        },
        "expired": r.expired,
        "refused": r.refused,
        "players": players,
        "games": games,
    })
}
