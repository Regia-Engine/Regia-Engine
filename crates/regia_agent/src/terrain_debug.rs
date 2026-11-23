// SPDX-License-Identifier: MPL-2.0
//! `terrain.get_debug_view` / `terrain.set_debug_view`: the terrain's debug views on a running
//! frame (ADR-1447, book `agents/terrain-debug-views.md`, guide topic `terrain_debug`).
//!
//! The host inserts [`TerrainDebugViewControl`] with its terrain draw's handle; the draw reads the
//! view at its next prepare. Without the resource both methods answer `F0003`; an unknown view is
//! `F0002`.

use regia_ecs::prelude::*;
use regia_terrain::{TERRAIN_DEBUG_VIEWS, TerrainDebugView, TerrainDebugViewControl};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no terrain debug view control in this app",
        "the host inserts TerrainDebugViewControl(draw.debug_view_handle()) with its terrain draw",
    )
}

fn entry(view: &TerrainDebugView) -> Value {
    json!({ "id": view.id, "name": view.name, "channels": view.channels })
}

fn current(id: u32) -> Value {
    TerrainDebugView::by_id(id).map_or_else(
        || json!({ "id": id, "name": null, "channels": null }),
        |view| entry(&view),
    )
}

/// `terrain.get_debug_view`: the view the terrain draw shows and every view it implements.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let control = world
        .get_resource::<TerrainDebugViewControl>()
        .ok_or_else(missing)?;
    let views: Vec<Value> = TERRAIN_DEBUG_VIEWS.iter().map(entry).collect();
    Ok(json!({ "view": current(control.0.get()), "views": views }))
}

/// `terrain.set_debug_view { view }`: a view number or name (`off` / 0 is the lit terrain),
/// applied at the draw's next prepare.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let handle = world
        .get_resource::<TerrainDebugViewControl>()
        .ok_or_else(missing)?
        .0
        .clone();
    let raw = params.get("view").unwrap_or(&Value::Null);
    let view = match raw {
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .and_then(TerrainDebugView::by_id),
        Value::String(name) => TerrainDebugView::find(name),
        Value::Null => {
            return Err(RpcError::invalid_params(
                "`view` is required: a view number or name (terrain.get_debug_view lists them)",
            ));
        }
        _ => return Err(RpcError::invalid_params("`view` must be a number or a name")),
    }
    .ok_or_else(|| {
        RpcError::invalid_params(format!(
            "unknown terrain debug view {raw}; terrain.get_debug_view lists them"
        ))
    })?;
    let before = handle.get();
    handle.set(view.id);
    Ok(json!({ "before": current(before), "after": entry(&view) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &RpcError) -> Option<Value> {
        err.data.as_ref().map(|d| d["code"].clone())
    }

    #[test]
    fn without_the_control_both_answer_not_found() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no control");
        assert_eq!(code(&err), Some(json!("F0003")));
        let err = set(&mut world, &json!({ "view": 8 })).expect_err("no control");
        assert_eq!(code(&err), Some(json!("F0003")));
    }

    #[test]
    fn views_are_set_by_number_or_name_and_reach_the_draw_handle() {
        let mut world = World::new();
        let control = TerrainDebugViewControl::default();
        let handle = control.0.clone();
        world.insert_resource(control);

        let listed = get(&mut world).expect("get");
        assert_eq!(listed["view"]["name"], "off");
        assert_eq!(
            listed["views"].as_array().map(Vec::len),
            Some(TERRAIN_DEBUG_VIEWS.len())
        );

        let reply = set(&mut world, &json!({ "view": "sun_split" })).expect("by name");
        assert_eq!(reply["before"]["id"], 0);
        assert_eq!(reply["after"]["id"], 8);
        assert_eq!(handle.get(), 8);

        let reply = set(&mut world, &json!({ "view": 15 })).expect("by number");
        assert_eq!(reply["before"]["name"], "sun_split");
        assert_eq!(reply["after"]["name"], "occlusion_texture");
        assert_eq!(handle.get(), 15);

        set(&mut world, &json!({ "view": "off" })).expect("off");
        assert_eq!(handle.get(), 0);
    }

    #[test]
    fn unknown_or_missing_views_are_refused_and_leave_the_view() {
        let mut world = World::new();
        let control = TerrainDebugViewControl::default();
        control.0.set(9);
        let handle = control.0.clone();
        world.insert_resource(control);
        for bad in [
            json!({ "view": 17 }),
            json!({ "view": -1 }),
            json!({ "view": 2.5 }),
            json!({ "view": "sky" }),
            json!({ "view": true }),
            json!({}),
        ] {
            let err = set(&mut world, &bad).expect_err("refused");
            assert_eq!(code(&err), Some(json!("F0002")), "{bad}");
        }
        assert_eq!(handle.get(), 9);
        // A view number the table does not name (a host's own) still reads back.
        handle.set(98);
        assert_eq!(get(&mut world).expect("get")["view"]["id"], 98);
    }
}
