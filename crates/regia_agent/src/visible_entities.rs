// SPDX-License-Identifier: MPL-2.0
//! `render.visible_entities {camera?, limit?}`: what the last frame drew on screen (spec 14 § A1,
//! ADR-7200, `book/agents/visibility.md`). Without `camera`: the entities whose `ViewVisibility`
//! is true (a draw passed some view's layer mask and frustum this frame, skinned draws at their
//! posed bound, and the GPU occlusion result did not hold it, ADR-7255). With `camera`: the
//! entities with a draw in that camera's frustum. Both report a count and up to `limit` entity ids
//! (default 64, at most 4096), ascending by entity index. `views` has one line per extracted view
//! (ADR-7286): its target, whether its occlusion result was folded in, how many entities it has
//! in view and how many of those its occlusion result hides, its own readback counters, and the
//! bytes of the Hi-Z pyramid the renderer keeps for its occlusion history (`pyramid_bytes`,
//! ADR-7329).
//! `play.wait_until` waits on one entity's `ViewVisibility` component directly.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `limit` when the call names none.
pub(crate) const DEFAULT_LIMIT: usize = 64;
/// The most ids one reply lists.
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

fn camera_param(params: &Value) -> Result<Option<Entity>, RpcError> {
    match params.get("camera") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("camera must be an entity id (an unsigned integer)")),
    }
}

/// The handler (read only).
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = limit_param(params)?;
    let camera = camera_param(params)?;
    let stats = world
        .get_resource::<regia_render::ViewVisibilityStats>()
        .copied()
        .unwrap_or_default();
    let (scope, total, ids) = match camera {
        None => {
            let (total, ids) = regia_render::visibility::visible_entities(world, limit);
            ("view_visibility", total, ids)
        }
        Some(camera) => {
            let Some((total, ids)) = regia_render::visibility::visible_to_camera(world, camera, limit) else {
                return Err(RpcError::not_found(
                    format!("entity {} is not a camera with a placement", camera.to_bits()),
                    "pass a camera entity from scene.query, or omit camera for every view",
                ));
            };
            ("camera", total, ids)
        }
    };
    let ids: Vec<u64> = ids.iter().map(|e| e.to_bits()).collect();
    let views = per_view(world);
    Ok(json!({
        "scope": scope,
        "camera": camera.map(Entity::to_bits),
        "visible": total,
        "listed": ids.len(),
        "truncated": ids.len() < total,
        "limit": limit,
        "entities": ids,
        "frame": {
            "runs": stats.runs,
            "views": stats.views,
            "draws": stats.draws,
            "visible": stats.visible,
            "entities": stats.entities,
            "writes": stats.writes,
            "occluded": stats.occluded,
        },
        "occlusion": world
            .get_resource::<regia_render::OcclusionFeedback>()
            .map(|feedback| occlusion_json(&feedback.stats())),
        "views": views,
    }))
}

/// One readback's counters as JSON.
fn occlusion_json(o: &regia_render::OcclusionFeedbackStats) -> Value {
    json!({
        "recorded": o.recorded,
        "dropped": o.dropped,
        "read": o.read,
        "stale": o.stale,
        "cleared": o.cleared,
        "published": o.published,
        "occluded": o.occluded,
        "result_seq": o.result_seq,
        "pyramid_bytes": o.pyramid_bytes,
    })
}

/// The last `ViewVisibility` run's line per view, each with its own readback counters (`null`
/// for a view without a ring: a texture view, or a window view not culled yet).
fn per_view(world: &World) -> Vec<Value> {
    let Some(lines) = world.get_resource::<regia_render::ViewVisibilityPerView>() else {
        return Vec::new();
    };
    let mut rings = Vec::new();
    let feedback = world.get_resource::<regia_render::OcclusionFeedback>();
    if let Some(feedback) = feedback {
        feedback.view_stats_into(&mut rings);
    }
    lines
        .views
        .iter()
        .map(|line| {
            let view = u32::try_from(line.view).ok();
            let ring = view.and_then(|view| rings.iter().find(|ring| ring.view == view));
            // The Hi-Z pyramid the renderer keeps for this view's occlusion history (ADR-7329).
            let pyramid_bytes = view
                .zip(feedback)
                .map_or(0, |(view, feedback)| feedback.view_pyramid_bytes(view));
            json!({
                "view": line.view,
                "target": if line.texture.is_some() { "texture" } else { "window" },
                "asset_id": line.texture,
                "occlusion_folded": line.occlusion,
                "in_view": line.in_view,
                "occluded": line.occluded,
                "pyramid_bytes": pyramid_bytes,
                "occlusion": ring.map(|ring| occlusion_json(&ring.stats)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_cameras_are_checked() {
        let mut world = World::new();
        assert!(get(&mut world, &json!({ "limit": -1 })).is_err());
        assert!(get(&mut world, &json!({ "limit": MAX_LIMIT + 1 })).is_err());
        assert!(get(&mut world, &json!({ "camera": "main" })).is_err());
        let empty = get(&mut world, &json!({})).expect("an empty world reports");
        assert_eq!(empty["visible"], 0);
        assert_eq!(empty["limit"], DEFAULT_LIMIT);
        let lone = world.spawn_empty().id();
        let err = get(&mut world, &json!({ "camera": lone.to_bits() })).expect_err("not a camera");
        let err = serde_json::to_value(&err).expect("error serializes");
        assert_eq!(err["data"]["code"], "F0003");
    }
}
