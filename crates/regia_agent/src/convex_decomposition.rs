// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of approximate convex decomposition (`book/agents/convex-decomposition.md`):
//! `physics.convex_decompose` splits an entity's triangle-mesh collider into convex hulls with
//! `regia_physics::convex_decomposition::decompose` and reports them, so an agent can pick the
//! parameters before game code builds a dynamic compound collider from the same call. Read-only:
//! the entity's collider is not changed.

use regia_app::prelude::*;
use regia_core::mesh_query::TriMesh;
use regia_physics::convex_decomposition::{DecompositionParams, decompose};
use regia_physics::{Collider, ColliderShape};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const METHOD: &str = "physics.convex_decompose";

fn bad(code: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::invalid_params(format!("{METHOD}: {message} [{code}]"))
}

fn uint_param(params: &Value, name: &str, default: u32, code: &str) -> Result<u32, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| bad(code, format!("`{name}` must be a non-negative integer"))),
    }
}

fn concavity_param(params: &Value, default: f32) -> Result<f32, RpcError> {
    match params.get("max_concavity") {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .ok_or_else(|| {
                bad(
                    "bad_max_concavity",
                    "`max_concavity` must be a number from 0 to 1",
                )
            }),
    }
}

/// `physics.convex_decompose { entity, max_hulls?, resolution?, max_concavity?, max_depth?,
/// max_hull_vertices?, points? }`: the convex hulls of the entity's `ColliderShape::TriMesh`, in the
/// collider's local space, largest first.
pub fn convex_decompose(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("{METHOD}: entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    let Some(collider) = world.get::<Collider>(entity) else {
        return Err(RpcError::not_found(
            format!("{METHOD}: entity {} has no Collider", entity.to_bits()),
            "decompose an entity whose Collider shape is a TriMesh",
        ));
    };
    let ColliderShape::TriMesh { vertices, indices } = &collider.shape else {
        return Err(bad("not_trimesh", "the entity's Collider shape is not a TriMesh"));
    };
    let defaults = DecompositionParams::default();
    let options = DecompositionParams {
        max_hulls: uint_param(params, "max_hulls", defaults.max_hulls, "bad_max_hulls")?,
        resolution: uint_param(params, "resolution", defaults.resolution, "bad_resolution")?,
        max_concavity: concavity_param(params, defaults.max_concavity)?,
        max_depth: uint_param(params, "max_depth", defaults.max_depth, "bad_max_depth")?,
        max_hull_vertices: uint_param(
            params,
            "max_hull_vertices",
            defaults.max_hull_vertices,
            "bad_max_hull_vertices",
        )?,
    };
    let points = match params.get("points") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(bad("bad_points", "`points` must be a boolean")),
    };
    let mesh = TriMesh::new(vertices.clone(), indices.clone()).map_err(|e| bad("bad_mesh", e))?;
    let hulls = decompose(&mesh, &options).map_err(|e| bad(e.code(), e))?;

    let total_volume: f32 = hulls.iter().map(|h| h.volume()).sum();
    let listed: Vec<Value> = hulls
        .iter()
        .map(|hull| {
            let c = hull.centroid();
            let mut entry = json!({
                "vertices": hull.vertices.len(),
                "triangles": hull.triangles.len(),
                "volume": hull.volume(),
                "centroid": [c.x, c.y, c.z],
            });
            if points {
                entry["points"] = hull.vertices.iter().map(|p| json!([p.x, p.y, p.z])).collect();
            }
            entry
        })
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "hull_count": listed.len(),
        "total_volume": total_volume,
        "hulls": listed,
        "params": {
            "max_hulls": options.max_hulls,
            "resolution": options.resolution,
            "max_concavity": options.max_concavity,
            "max_depth": options.max_depth,
            "max_hull_vertices": options.max_hull_vertices,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol;
    use regia_core::math::Vec3;

    /// An L-shaped prism (volume 3) as a static trimesh collider, plus a box collider entity.
    fn world_with_colliders() -> (World, Entity, Entity) {
        let outline = [
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
        ];
        let n = outline.len() as u32;
        let mut vertices = Vec::new();
        for z in [0.0, 1.0] {
            for [x, y] in outline {
                vertices.push(Vec3::new(x, y, z));
            }
        }
        let mut indices = Vec::new();
        for i in 1..n - 1 {
            indices.push([n, n + i, n + i + 1]);
            indices.push([0, i + 1, i]);
        }
        for i in 0..n {
            let j = (i + 1) % n;
            indices.push([i, j, n + j]);
            indices.push([i, n + j, n + i]);
        }
        let mut world = World::new();
        let l = world
            .spawn(Collider {
                shape: ColliderShape::TriMesh { vertices, indices },
                ..Collider::default()
            })
            .id();
        let cube = world.spawn(Collider::default()).id();
        (world, l, cube)
    }

    #[test]
    fn decomposes_a_trimesh_collider() {
        let (mut world, l, _) = world_with_colliders();
        let out = convex_decompose(&mut world, &json!({ "entity": l.to_bits() })).expect("call");
        assert_eq!(out["hull_count"], 2);
        assert_eq!(out["entity"], l.to_bits());
        let total = out["total_volume"].as_f64().expect("volume");
        assert!((total - 3.0).abs() <= 0.15, "total {total}");
        assert!(out["hulls"][0].get("points").is_none());
        assert_eq!(out["params"]["max_hulls"], 16);

        let one = convex_decompose(
            &mut world,
            &json!({ "entity": l.to_bits(), "max_hulls": 1, "max_hull_vertices": 0, "points": true }),
        )
        .expect("call");
        assert_eq!(one["hull_count"], 1);
        let points = one["hulls"][0]["points"].as_array().expect("points");
        assert_eq!(
            points.len(),
            one["hulls"][0]["vertices"].as_u64().expect("count") as usize
        );
        assert!((one["total_volume"].as_f64().expect("volume") - 3.5).abs() < 1e-4);
    }

    #[test]
    fn errors_carry_codes() {
        let (mut world, l, cube) = world_with_colliders();
        let cases = [
            (json!({ "entity": cube.to_bits() }), "[not_trimesh]"),
            (
                json!({ "entity": l.to_bits(), "max_hulls": 0 }),
                "[bad_max_hulls]",
            ),
            (
                json!({ "entity": l.to_bits(), "max_hulls": -1 }),
                "[bad_max_hulls]",
            ),
            (
                json!({ "entity": l.to_bits(), "resolution": 4 }),
                "[bad_resolution]",
            ),
            (
                json!({ "entity": l.to_bits(), "max_concavity": "high" }),
                "[bad_max_concavity]",
            ),
            (
                json!({ "entity": l.to_bits(), "max_concavity": 2.0 }),
                "[bad_max_concavity]",
            ),
            (
                json!({ "entity": l.to_bits(), "max_depth": 99 }),
                "[bad_max_depth]",
            ),
            (
                json!({ "entity": l.to_bits(), "max_hull_vertices": 2 }),
                "[bad_max_hull_vertices]",
            ),
            (json!({ "entity": l.to_bits(), "points": 1 }), "[bad_points]"),
        ];
        for (params, code) in cases {
            let error = convex_decompose(&mut world, &params).expect_err(code);
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{params}");
            assert!(error.message.contains(code), "{params}: {}", error.message);
        }

        let flat = world
            .spawn(Collider {
                shape: ColliderShape::TriMesh {
                    vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
                    indices: vec![[0, 1, 2]],
                },
                ..Collider::default()
            })
            .id();
        let error = convex_decompose(&mut world, &json!({ "entity": flat.to_bits() })).expect_err("flat");
        assert!(error.message.contains("[flat_mesh]"), "{}", error.message);
        let empty = world
            .spawn(Collider {
                shape: ColliderShape::TriMesh {
                    vertices: vec![Vec3::ZERO],
                    indices: Vec::new(),
                },
                ..Collider::default()
            })
            .id();
        let error = convex_decompose(&mut world, &json!({ "entity": empty.to_bits() })).expect_err("empty");
        assert!(error.message.contains("[empty_mesh]"), "{}", error.message);
        let broken = world
            .spawn(Collider {
                shape: ColliderShape::TriMesh {
                    vertices: vec![Vec3::ZERO],
                    indices: vec![[0, 1, 2]],
                },
                ..Collider::default()
            })
            .id();
        let error = convex_decompose(&mut world, &json!({ "entity": broken.to_bits() })).expect_err("broken");
        assert!(error.message.contains("[bad_mesh]"), "{}", error.message);
    }

    #[test]
    fn missing_entity_or_collider_is_not_found() {
        let (mut world, l, _) = world_with_colliders();
        let bare = world.spawn_empty().id();
        let error = convex_decompose(&mut world, &json!({ "entity": bare.to_bits() })).expect_err("bare");
        assert_eq!(error.code, protocol::NOT_FOUND);
        world.despawn(l);
        let error = convex_decompose(&mut world, &json!({ "entity": l.to_bits() })).expect_err("gone");
        assert_eq!(error.code, protocol::NOT_FOUND);
        let error = convex_decompose(&mut world, &json!({})).expect_err("no entity");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
    }
}
