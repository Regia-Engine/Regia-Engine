// SPDX-License-Identifier: MPL-2.0
//! `assets.collision_report { mesh }` (ADR-2341, `book/agents/collision-proxies.md`): the authored
//! collision a glTF cooked from its `collision_<kind>_*` nodes, read from the file's `collision`
//! sub-asset through the running [`AssetServer`]: counts per kind and every shape with its node,
//! pose and size. Read-only.

use regia_app::prelude::*;
use regia_asset::{
    AssetId, AssetServer, COLLISION_SET_SUB_PATH, CollisionProxyKind, CookedCollisionGeometry,
    CookedCollisionSet, load_collision_set_bytes,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const METHOD: &str = "assets.collision_report";

fn axis_name(axis: regia_asset::collision_set::CollisionAxis) -> &'static str {
    use regia_asset::collision_set::CollisionAxis as A;
    match axis {
        A::X => "x",
        A::Y => "y",
        A::Z => "z",
    }
}

/// One shape as JSON: node, kind, requested kind, pose and the kind's sizes.
fn shape_json(shape: &regia_asset::CookedCollisionShape) -> Value {
    let mut entry = json!({
        "node": shape.node,
        "kind": shape.geometry.kind().as_str(),
        "requested": shape.requested.as_str(),
        "translation": shape.translation,
        "rotation": shape.rotation,
    });
    match &shape.geometry {
        CookedCollisionGeometry::Box(b) => entry["half_extents"] = json!(b.half_extents),
        CookedCollisionGeometry::Sphere(s) => entry["radius"] = json!(s.radius),
        CookedCollisionGeometry::Capsule(c) => {
            entry["half_height"] = json!(c.half_height);
            entry["radius"] = json!(c.radius);
            entry["axis"] = json!(axis_name(c.axis));
        }
        CookedCollisionGeometry::Convex(c) => entry["points"] = json!(c.points.len()),
        CookedCollisionGeometry::TriMesh(m) => {
            entry["vertices"] = json!(m.vertices.len());
            entry["triangles"] = json!(m.indices.len());
        }
    }
    entry
}

/// The report of a set (or of no set: every count zero).
#[must_use]
pub fn report_json(mesh: &str, gltf: AssetId, set: Option<&CookedCollisionSet>) -> Value {
    let shapes: Vec<Value> = set
        .map(|s| s.shapes.iter().map(shape_json).collect())
        .unwrap_or_default();
    let mut counts = serde_json::Map::new();
    for kind in CollisionProxyKind::ALL {
        counts.insert(kind.as_str().into(), json!(set.map_or(0, |s| s.count(kind))));
    }
    let fallbacks = set.map_or(0, |s| {
        s.shapes
            .iter()
            .filter(|shape| shape.requested != shape.geometry.kind())
            .count()
    });
    json!({
        "mesh": mesh,
        "id": gltf.to_string(),
        "collision_id": AssetId::sub_asset(gltf, COLLISION_SET_SUB_PATH).to_string(),
        "has_collision": set.is_some_and(|s| !s.shapes.is_empty()),
        "shape_count": shapes.len(),
        "counts": counts,
        "fallbacks": fallbacks,
        "shapes": shapes,
    })
}

/// `assets.collision_report { mesh }`: the cooked collision proxies of the glTF `mesh` (a
/// project-relative path or the glTF's asset id). A glTF without proxies reports
/// `has_collision: false` and zero counts.
pub fn collision_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(mesh) = crate::params::opt_str(params, "mesh")?.filter(|s| !s.is_empty()) else {
        return Err(RpcError::invalid_params(format!(
            "{METHOD}: `mesh` (a glTF's project-relative path or asset id) is required [bad_mesh]"
        )));
    };
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return Err(crate::assets::no_asset_server());
    };
    let (path, gltf) = crate::assets::resolve_asset(&server, &json!({ "asset": mesh }))?;
    let id = AssetId::sub_asset(gltf, COLLISION_SET_SUB_PATH);
    let set = match server.read_cooked(id) {
        Ok((bytes, _)) => Some(
            load_collision_set_bytes(&bytes)
                .map(|(_, set)| set)
                .map_err(|e| {
                    RpcError::new(
                        crate::protocol::INVALID_PARAMS,
                        "F0013",
                        format!(
                            "{METHOD}: the collision set of {path} does not read: {e} [{}]",
                            e.code()
                        ),
                        "re-cook the glTF (regia cook)",
                    )
                })?,
        ),
        Err(regia_asset::AssetLoadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(RpcError::new(
                crate::protocol::INTERNAL_ERROR,
                "F0016",
                format!("{METHOD}: cannot read the collision set of {path}: {e} [collision_set_unreadable]"),
                "check assets.status for the glTF",
            ));
        }
    };
    Ok(report_json(&path, gltf, set.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::collision_set::{CollisionBox, CollisionConvex};

    #[test]
    fn the_report_counts_kinds_and_lists_shapes() {
        let set = CookedCollisionSet::new(vec![
            regia_asset::CookedCollisionShape {
                node: "collision_box_lid".into(),
                requested: CollisionProxyKind::Box,
                translation: [0.0, 1.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                geometry: CookedCollisionGeometry::Box(CollisionBox {
                    half_extents: [0.5, 0.25, 0.5],
                }),
            },
            regia_asset::CookedCollisionShape {
                node: "collision_convex_body".into(),
                requested: CollisionProxyKind::Convex,
                translation: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                geometry: CookedCollisionGeometry::Convex(CollisionConvex {
                    points: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                }),
            },
        ]);
        let gltf = AssetId::sub_asset(AssetId::default(), "crate");
        let report = report_json("assets/props/crate.glb", gltf, Some(&set));
        assert_eq!(report["has_collision"], true);
        assert_eq!(report["shape_count"], 2);
        assert_eq!(report["counts"]["box"], 1);
        assert_eq!(report["counts"]["convex"], 1);
        assert_eq!(report["counts"]["mesh"], 0);
        assert_eq!(report["fallbacks"], 0);
        assert_eq!(report["shapes"][0]["half_extents"], json!([0.5, 0.25, 0.5]));
        assert_eq!(report["shapes"][1]["points"], 4);
        let none = report_json("assets/props/plain.glb", gltf, None);
        assert_eq!(none["has_collision"], false);
        assert_eq!(none["shape_count"], 0);
        assert_eq!(none["counts"]["sphere"], 0);
    }

    #[test]
    fn a_missing_mesh_param_is_bad_mesh() {
        let mut world = World::new();
        let err = collision_report(&mut world, &json!({})).expect_err("no mesh");
        assert!(err.message.contains("[bad_mesh]"), "{}", err.message);
        let err = collision_report(&mut world, &json!({ "mesh": "assets/a.glb" })).expect_err("no server");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0016"));
    }
}
