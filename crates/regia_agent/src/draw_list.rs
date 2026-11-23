// SPDX-License-Identifier: MPL-2.0
//! `render.draw_list {limit?}`: the host's keyed draws (`regia_render::DrawList`, ADR-8087,
//! `book/agents/draw-list.md`). Read only: the list's counts (items, hidden, skinned, what the
//! last extraction appended), items per render layer, and up to `limit` draws in key order (the
//! extraction order) with their key, object key, layer, visibility, shadow policy, skin palette
//! length and translation. Keys and object keys are 16-digit hex strings (they do not fit a JSON
//! number). Draws from the list are not entities: `render.visible_entities` and `scene.*` do
//! not list them. ADR-8457 adds the LOD and shadow rows: chained items, local casters and
//! shadowless items, the last LOD selection's counts per colour and shadow LOD and its
//! cross-fading draws (`lod`), and each listed draw's chain length (`lod_levels`).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_render::{DrawList, DrawLodStats, ShadowCast};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `limit` when the call names none.
pub(crate) const DEFAULT_LIMIT: usize = 64;
/// The most draws one reply lists.
pub(crate) const MAX_LIMIT: usize = 4096;

fn limit_param(params: &Value) -> Result<usize, RpcError> {
    match params.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_LIMIT),
        Some(value) => match value.as_u64() {
            Some(n) if n as usize <= MAX_LIMIT => Ok(n as usize),
            _ => Err(RpcError::invalid_params(format!(
                "limit must be an integer from 0 to {MAX_LIMIT}"
            ))),
        },
    }
}

fn shadow_name(shadow: ShadowCast) -> &'static str {
    match shadow {
        ShadowCast::All => "all",
        ShadowCast::Off => "off",
        ShadowCast::Local { .. } => "local",
    }
}

/// The last LOD selection's counts (ADR-8457).
fn lod_json(lod: &DrawLodStats) -> Value {
    json!({
        "selected": lod.selected,
        "color_lods": lod.color_lods,
        "shadow_lods": lod.shadow_lods,
        "crossfading": lod.crossfading,
        "selections": lod.selections,
    })
}

/// The handler (read only).
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = limit_param(params)?;
    let Some(list) = world.get_resource::<DrawList>() else {
        return Ok(json!({
            "present": false,
            "items": 0,
            "hidden": 0,
            "skinned": 0,
            "chained": 0,
            "local_casters": 0,
            "shadow_off": 0,
            "extracted": 0,
            "extractions": 0,
            "lod": lod_json(&DrawLodStats::default()),
            "layers": [],
            "listed": 0,
            "truncated": false,
            "limit": limit,
            "draws": [],
        }));
    };
    let stats = list.stats();
    let mut layers: BTreeMap<u32, usize> = BTreeMap::new();
    for (_, item) in list.iter() {
        *layers.entry(item.layer).or_default() += 1;
    }
    let draws: Vec<Value> = list
        .iter()
        .take(limit)
        .map(|(key, item)| {
            let t = item.model.w_axis;
            json!({
                "key": format!("{:016x}", key.0),
                "object_key": format!("{:016x}", key.object_key()),
                "layer": item.layer,
                "visible": item.visible,
                "shadow": shadow_name(item.shadow),
                "joints": item.joints.len(),
                "lod_levels": item.lod_chain.as_ref().map_or(0, |chain| chain.levels.len()),
                "translation": [t.x, t.y, t.z],
            })
        })
        .collect();
    Ok(json!({
        "present": true,
        "items": stats.items,
        "hidden": stats.hidden,
        "skinned": stats.skinned,
        "chained": stats.chained,
        "local_casters": stats.local_casters,
        "shadow_off": stats.shadow_off,
        "extracted": stats.extracted,
        "extractions": stats.extractions,
        "lod": lod_json(&list.lod_stats()),
        "layers": layers
            .into_iter()
            .map(|(layer, items)| json!({ "layer": layer, "items": items }))
            .collect::<Vec<_>>(),
        "listed": draws.len(),
        "truncated": draws.len() < stats.items,
        "limit": limit,
        "draws": draws,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_core::math::{Mat4, Vec3};
    use regia_render::{CpuMaterial, CpuMesh, DrawKey, KeyedDraw};

    use super::*;

    #[test]
    fn reports_counts_layers_and_draws_in_key_order() {
        let mut world = World::new();
        let none = get(&mut world, &json!({})).expect("answers without a list");
        assert_eq!(none["present"], false);
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let material = Arc::new(CpuMaterial::default());
        let mut list = DrawList::new();
        for (raw, layer, visible) in [(3u64, 0, true), (1, 2, false), (2, 2, true)] {
            list.insert(
                DrawKey(raw),
                KeyedDraw::new(
                    Arc::clone(&mesh),
                    Arc::clone(&material),
                    Mat4::from_translation(Vec3::new(raw as f32, 0.0, 0.0)),
                )
                .with_layer(layer)
                .with_visible(visible),
            );
        }
        world.insert_resource(list);
        let answer = get(&mut world, &json!({ "limit": 2 })).expect("answers");
        assert_eq!(answer["present"], true);
        assert_eq!(answer["items"], 3);
        assert_eq!(answer["hidden"], 1);
        assert_eq!(answer["listed"], 2);
        assert_eq!(answer["truncated"], true);
        assert_eq!(
            answer["layers"],
            json!([{ "layer": 0, "items": 1 }, { "layer": 2, "items": 2 }])
        );
        assert_eq!(answer["draws"][0]["key"], "0000000000000001");
        assert_eq!(
            answer["draws"][0]["object_key"],
            format!("{:016x}", DrawKey(1).object_key())
        );
        assert_eq!(answer["draws"][1]["translation"], json!([2.0, 0.0, 0.0]));
        assert_eq!(answer["draws"][0]["lod_levels"], 0);
        assert_eq!(
            (answer["chained"].clone(), answer["local_casters"].clone()),
            (json!(0), json!(0))
        );
        assert_eq!(answer["lod"]["selections"], 0);
        assert_eq!(none["lod"]["color_lods"], json!([]));
        assert!(get(&mut world, &json!({ "limit": 5000 })).is_err());
        assert!(get(&mut world, &json!({ "limit": "x" })).is_err());
    }
}
