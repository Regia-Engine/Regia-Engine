// SPDX-License-Identifier: MPL-2.0
//! `world.pick` (DO27 ask 12): the render-side ray over terrain and mesh layers
//! ([`regia_pick::world_pick`]), reporting the hit entity, its `MapLayer`, the point, the
//! normal and the distance. `book/agents/world-pick.md` is the guide.

use regia_app::prelude::*;
use regia_core::math::Ray;
use regia_pick::{FaceCull, LayerSet, PickFilter, PickSurface, WorldPick, world_pick, world_pick_layers};
use serde_json::{Value, json};

use crate::physics::vec3_param;
use crate::protocol::RpcError;

fn pick_json(world: &World, pick: &WorldPick) -> Value {
    let name = world
        .get::<regia_scene::prelude::Name>(pick.entity)
        .map(|n| n.0.to_string());
    json!({
        "entity": pick.entity.to_bits(),
        "name": name,
        "layer": pick.layer.0,
        "surface": match pick.surface {
            PickSurface::Terrain => "terrain",
            PickSurface::Mesh => "mesh",
        },
        "point": pick.point.to_array(),
        "normal": pick.normal.to_array(),
        "distance": pick.distance,
        "triangle": pick.triangle,
    })
}

fn bool_param(params: &Value, field: &str, default: bool) -> Result<bool, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a boolean"))),
    }
}

fn filter_from(params: &Value) -> Result<PickFilter, RpcError> {
    let mut filter = PickFilter::default();
    if let Some(v) = params.get("max_distance").filter(|v| !v.is_null()) {
        let d = v
            .as_f64()
            .filter(|d| d.is_finite() && *d > 0.0)
            .ok_or_else(|| RpcError::invalid_params("`max_distance` must be a positive number"))?;
        filter.max_distance = d as f32;
    }
    if let Some(v) = params.get("layers").filter(|v| !v.is_null()) {
        let bad = || RpcError::invalid_params("`layers` must be an array of integers 0..=255");
        let mut set = LayerSet::NONE;
        for item in v.as_array().ok_or_else(bad)? {
            let layer = item.as_u64().and_then(|l| u8::try_from(l).ok()).ok_or_else(bad)?;
            set.insert(layer);
        }
        filter.layers = set;
    }
    match params.get("cull") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) => {
            filter.cull = match name.as_str() {
                "none" => FaceCull::None,
                "back" => FaceCull::Back,
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "unknown cull `{other}` (none, back)"
                    )));
                }
            };
        }
        Some(_) => return Err(RpcError::invalid_params("`cull` must be a string (none, back)")),
    }
    filter.terrain = bool_param(params, "terrain", true)?;
    filter.meshes = bool_param(params, "meshes", true)?;
    filter.include_hidden = bool_param(params, "include_hidden", false)?;
    if let Some(v) = params.get("skip").filter(|v| !v.is_null()) {
        let bad = || RpcError::invalid_params("`skip` must be an array of entity ids");
        for item in v.as_array().ok_or_else(bad)? {
            let bits = item.as_u64().ok_or_else(bad)?;
            filter.skip.push(
                regia_ecs::ecs::entity::Entity::try_from_bits(bits)
                    .ok_or_else(|| RpcError::invalid_params(format!("`skip` entity {bits} is not valid")))?,
            );
        }
    }
    Ok(filter)
}

/// `world.pick`: the nearest surface along `{origin, direction}` (or, with `all_layers`, the
/// nearest hit of each layer), filtered by `layers`, `max_distance`, `cull`, `terrain`, `meshes`,
/// `include_hidden` and `skip`.
pub fn pick(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let origin = vec3_param(params, "origin")?;
    let direction = vec3_param(params, "direction")?;
    if direction.length_squared() < f32::MIN_POSITIVE {
        return Err(RpcError::invalid_params("`direction` must not be zero"));
    }
    cast(world, Ray::new(origin, direction), params)
}

/// Casts `ray` with the filter in `params` (`world.pick`'s optional fields) and reports
/// `{ hit, count }`, or `{ hits, count }` with `all_layers`. Shared with `camera.raycast`.
pub(crate) fn cast(world: &mut World, ray: Ray, params: &Value) -> Result<Value, RpcError> {
    let filter = filter_from(params)?;
    if bool_param(params, "all_layers", false)? {
        let hits: Vec<Value> = world_pick_layers(world, ray, &filter)
            .iter()
            .map(|h| pick_json(world, h))
            .collect();
        let count = hits.len();
        return Ok(json!({ "hits": hits, "count": count }));
    }
    let hit = world_pick(world, ray, &filter);
    Ok(json!({
        "hit": hit.as_ref().map(|h| pick_json(world, h)),
        "count": usize::from(hit.is_some()),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_core::glam::Affine3A;
    use regia_core::math::Vec3;
    use regia_render::{CpuMesh, LabMaterial, MeshRenderer, box_mesh};
    use regia_scene::GlobalTransform;
    use regia_scene::prelude::MapLayer;

    use super::*;

    fn deck(world: &mut World, top: f32, layer: u8) {
        let (vertices, indices) = box_mesh([20.0, 0.1, 20.0]);
        world.spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
            GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, top - 0.1, 0.0))),
            MapLayer(layer),
        ));
    }

    #[test]
    fn world_pick_reports_the_layer_and_honors_the_filter() {
        let mut world = World::new();
        deck(&mut world, 5.0, 1);
        deck(&mut world, 10.0, 2);
        let base = json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0] });
        let top = pick(&mut world, &base).expect("pick");
        assert_eq!(top["hit"]["layer"], 2);
        assert_eq!(top["hit"]["surface"], "mesh");
        assert!((top["hit"]["distance"].as_f64().expect("distance") - 40.0).abs() < 1e-3);

        let mut only_one = base.clone();
        only_one["layers"] = json!([1]);
        assert_eq!(pick(&mut world, &only_one).expect("pick")["hit"]["layer"], 1);

        let mut all = base.clone();
        all["all_layers"] = json!(true);
        let all = pick(&mut world, &all).expect("pick");
        assert_eq!(all["count"], 2);
        assert_eq!(all["hits"][0]["layer"], 2);

        let mut miss = base.clone();
        miss["layers"] = json!([9]);
        assert!(pick(&mut world, &miss).expect("pick")["hit"].is_null());
        miss["layers"] = json!([300]);
        assert!(pick(&mut world, &miss).is_err());
        assert!(
            pick(
                &mut world,
                &json!({ "origin": [0, 0, 0], "direction": [0, 0, 0] })
            )
            .is_err()
        );
    }

    #[test]
    fn filter_fields_are_type_checked_and_honored() {
        let mut world = World::new();
        deck(&mut world, 5.0, 1);
        let base = json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0] });
        let with = |k: &str, v: Value| {
            let mut p = base.clone();
            p[k] = v;
            p
        };
        // A wrong type is refused, never silently ignored.
        for (k, v) in [
            ("cull", json!(1)),
            ("cull", json!("front")),
            ("cull", json!(["back"])),
            ("max_distance", json!(0)),
            ("max_distance", json!(-3)),
            ("max_distance", json!("far")),
            ("layers", json!(1)),
            ("layers", json!([-1])),
            ("layers", json!([1.5])),
            ("layers", json!([256])),
            ("terrain", json!("yes")),
            ("meshes", json!(0)),
            ("include_hidden", json!([])),
            ("all_layers", json!("all")),
            ("skip", json!(7)),
            ("skip", json!(["a"])),
            ("skip", json!([0])),
        ] {
            let err = pick(&mut world, &with(k, v.clone())).expect_err(k);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{k}={v}");
        }
        // Nulls mean absent.
        let hit = pick(&mut world, &with("cull", Value::Null)).expect("null cull");
        assert_eq!(hit["count"], 1);
        // Boundary layer values 0 and 255 parse; layer 1 holds the deck.
        assert_eq!(
            pick(&mut world, &with("layers", json!([0, 255]))).expect("edge")["count"],
            0
        );
        assert_eq!(
            pick(&mut world, &with("layers", json!([]))).expect("empty set")["count"],
            0
        );
        // Switching the mesh test off leaves nothing to hit; terrain off alone does not matter.
        assert_eq!(
            pick(&mut world, &with("meshes", json!(false))).expect("no meshes")["count"],
            0
        );
        assert_eq!(
            pick(&mut world, &with("terrain", json!(false))).expect("no terrain")["count"],
            1
        );
        // A hit exactly at the limit is kept; a shorter limit misses.
        assert_eq!(
            pick(&mut world, &with("max_distance", json!(100.0))).expect("far")["count"],
            1
        );
        assert_eq!(
            pick(&mut world, &with("max_distance", json!(10.0))).expect("near")["count"],
            0
        );
        // Skipping the only mesh entity leaves nothing.
        let deck_entity = pick(&mut world, &base).expect("pick")["hit"]["entity"]
            .as_u64()
            .expect("entity");
        assert_eq!(
            pick(&mut world, &with("skip", json!([deck_entity]))).expect("skip")["count"],
            0
        );
    }

    #[test]
    fn origin_and_direction_are_validated_and_a_back_cull_skips_the_underside() {
        let mut world = World::new();
        deck(&mut world, 5.0, 0);
        for p in [
            json!({ "direction": [0, -1, 0] }),
            json!({ "origin": [0, 1] , "direction": [0, -1, 0] }),
            json!({ "origin": [0, 1, 0], "direction": [0, "d", 0] }),
            json!({ "origin": [1.0e300, 1, 0], "direction": [0, -1, 0] }),
            json!({ "origin": [0, 1, 0], "direction": [0, 0, 1.0e-300] }),
        ] {
            assert!(pick(&mut world, &p).is_err(), "{p}");
        }
        // Object form works and is the same as the array form.
        let a = pick(
            &mut world,
            &json!({ "origin": { "x": 0, "y": 50, "z": 0 }, "direction": [0, -1, 0] }),
        )
        .expect("obj");
        let b = pick(
            &mut world,
            &json!({ "origin": [0, 50, 0], "direction": [0, -1, 0] }),
        )
        .expect("arr");
        assert_eq!(a, b);
        // From below, the deck's underside faces the ray: a front face, kept by `cull: back`.
        let below = json!({ "origin": [0.0, -50.0, 0.0], "direction": [0.0, 1.0, 0.0] });
        let mut culled = below.clone();
        culled["cull"] = json!("back");
        assert_eq!(pick(&mut world, &below).expect("both")["count"], 1);
        assert_eq!(pick(&mut world, &culled).expect("front face")["count"], 1);
        // From inside the deck the ray meets the top's inner side, a back face.
        let inside = json!({ "origin": [0.0, 4.95, 0.0], "direction": [0.0, 1.0, 0.0] });
        let mut inside_culled = inside.clone();
        inside_culled["cull"] = json!("back");
        assert_eq!(pick(&mut world, &inside).expect("none")["count"], 1);
        assert_eq!(pick(&mut world, &inside_culled).expect("back")["count"], 0);
        // Repeated casts are identical.
        assert_eq!(
            pick(&mut world, &below).expect("again"),
            pick(&mut world, &below).expect("again")
        );
    }
}
