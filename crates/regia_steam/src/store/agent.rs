// SPDX-License-Identifier: MPL-2.0
//! Read-only store visibility; no implicit initialization or store mutation.

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, Permission};
use regia_app::prelude::*;
use serde_json::{Value, json};

use super::{PREFIX_STEAM, Store, StoreSelection, steam_ready};
use crate::Steam;

fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(selection) = world.get_resource::<StoreSelection>().copied() else {
        return Ok(json!({ "plugin": false }));
    };
    // A mutable adapter borrow would mark the resource changed merely by observing it.
    let steam = world
        .get_resource::<Steam>()
        .filter(|steam| selection.requested == Store::Steam && steam_ready(steam));
    let active = if steam.is_some() {
        Store::Steam
    } else {
        Store::None
    };
    let account_id = steam
        .and_then(Steam::steam_id)
        .filter(|id| !id.is_invalid())
        .map(|id| format!("{PREFIX_STEAM}{}", id.0));
    let description = if steam.is_some() {
        "steam (ready)"
    } else {
        "none (DRM-free)"
    };
    let reason = match selection.requested {
        Store::None => "none_requested",
        Store::Steam if active == Store::Steam => "steam_ready",
        Store::Steam => "steam_unavailable",
        Store::Gog => "gog_sdk_unavailable",
    };
    Ok(json!({
        "plugin": true,
        "requested": selection.requested,
        "active": active,
        "account_id": account_id,
        "description": description,
        "reason": reason,
    }))
}

pub(super) fn install(world: &mut World) {
    let mut extension = world.get_resource_or_insert_with(AgentExtension::default);
    extension.register(
        MethodSchema {
            name: "store.status",
            tool: "store_status",
            description: "The requested and active store, fallback reason, and optional store-prefixed account id.",
            params: &[],
            result: "{ plugin, requested, active, account_id, description, reason }",
            mutating: false,
            permission: Permission::Read,
        },
        status,
    );
}
