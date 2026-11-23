// SPDX-License-Identifier: MPL-2.0
//! `ui.icons`: the loaded icon atlas and how each icon node resolved (book `agents/ui-icons.md`,
//! ADR-865): which drawing was used at which size, whether it was scaled up (soft) or is missing.

use regia_ecs::prelude::*;
use regia_ui::icons::{Icon, IconAtlas, IconState};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn node_json(entity: Entity, icon: &Icon, state: Option<&IconState>) -> Value {
    json!({
        "entity": entity.to_bits(),
        "name": icon.name,
        "missing": state.is_some_and(|s| s.missing),
        "ui_px": state.map(|s| s.ui_px),
        "variant_px": state.map(|s| s.variant_px),
        "draw_px": state.map(|s| s.draw_px),
        "upscaled": state.is_some_and(|s| s.upscaled),
        "tint": state.map(|s| s.tint.0),
        "resolved": state.is_some(),
    })
}

/// `ui.icons { entity? }`.
pub(crate) fn icons(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<IconAtlas>() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "ui_not_installed",
            "this app has no icon atlas",
            "the app needs regia_ui::UiPlugin",
        ));
    }
    if let Some(v) = params
        .get("entity")
        .or_else(|| params.get("node"))
        .filter(|v| !v.is_null())
    {
        let entity = v.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "bad_entity",
                "`entity` must be a node id from ui.query",
                "omit it to list every icon",
            )
        })?;
        let Ok(node) = world.get_entity(entity) else {
            return Err(RpcError::new(
                NOT_FOUND,
                "unknown_entity",
                "the entity does not exist",
                "take entity ids from ui.query or ui.icons",
            ));
        };
        let Some(icon) = node.get::<Icon>() else {
            return Err(RpcError::new(
                NOT_FOUND,
                "not_an_icon",
                "the entity has no Icon",
                "ui.icons {} lists the icon nodes",
            ));
        };
        return Ok(node_json(entity, icon, node.get::<IconState>()));
    }
    let mut nodes: Vec<(Entity, Value)> = world
        .query::<(Entity, &Icon, Option<&IconState>)>()
        .iter(world)
        .map(|(e, icon, state)| (e, node_json(e, icon, state)))
        .collect();
    nodes.sort_by_key(|(e, _)| e.to_bits());
    let missing: Vec<&Value> = nodes
        .iter()
        .map(|(_, v)| v)
        .filter(|v| v["missing"] == json!(true))
        .collect();
    let missing_names: Vec<Value> = missing.iter().map(|v| v["name"].clone()).collect();
    let upscaled = nodes.iter().filter(|(_, v)| v["upscaled"] == json!(true)).count();
    let atlas = world.resource::<IconAtlas>();
    let loaded: Vec<Value> = atlas
        .names()
        .into_iter()
        .map(|name| {
            let sizes: Vec<u32> = atlas.variants(name).iter().map(|v| v.size_px).collect();
            json!({ "name": name, "sizes": sizes })
        })
        .collect();
    let pending = atlas.pending_pages().len();
    Ok(json!({
        "sheets": atlas.sheet_count(),
        "generation": atlas.generation,
        "pending_pages": pending,
        "icons": loaded,
        "nodes": nodes.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "missing": missing_names,
        "upscaled": upscaled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::icons::{IconPage, IconSheet};

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    #[test]
    fn lists_the_atlas_and_the_icon_nodes() {
        let mut world = World::new();
        world.init_resource::<IconAtlas>();
        let page = IconPage {
            width: 32,
            height: 16,
            rgba8: vec![255; 32 * 16 * 4],
        };
        let sheet = IconSheet::from_grid(page, [16, 16], &["gear@16", "close@16"]).expect("grid");
        world.resource_mut::<IconAtlas>().add_sheet(sheet).expect("sheet");
        let gear = world
            .spawn((
                Icon::new("gear"),
                IconState {
                    variant_px: 16,
                    draw_px: 24.0,
                    upscaled: true,
                    ui_px: 24.0,
                    ..IconState::default()
                },
            ))
            .id();
        world.spawn((
            Icon::new("nope"),
            IconState {
                missing: true,
                ..IconState::default()
            },
        ));
        let out = icons(&mut world, &json!({})).expect("list");
        assert_eq!(out["sheets"], json!(1));
        assert_eq!(out["icons"][0], json!({ "name": "close", "sizes": [16] }));
        assert_eq!(out["missing"], json!(["nope"]));
        assert_eq!(out["upscaled"], json!(1));
        assert_eq!(out["pending_pages"], json!(1));
        let one = icons(&mut world, &json!({ "entity": gear.to_bits() })).expect("node");
        assert_eq!(one["variant_px"], json!(16));
        assert_eq!(one["upscaled"], json!(true));
    }

    #[test]
    fn validates_the_entity() {
        let mut world = World::new();
        let err = icons(&mut world, &json!({})).expect_err("no atlas");
        assert_eq!(code(&err), "ui_not_installed");
        world.init_resource::<IconAtlas>();
        let plain = world.spawn_empty().id();
        let err = icons(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("plain");
        assert_eq!(code(&err), "not_an_icon");
        let err = icons(&mut world, &json!({ "entity": "x" })).expect_err("bad");
        assert_eq!(code(&err), "bad_entity");
        assert!(world.despawn(plain));
        let err = icons(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("gone");
        assert_eq!(code(&err), "unknown_entity");
    }
}
