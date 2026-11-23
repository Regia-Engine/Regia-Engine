// SPDX-License-Identifier: MPL-2.0
//! `terrain.scatter_colliders`: bakes the gameplay colliders of a `.scatter.ron` over the world's
//! terrain, edge- or world-anchored (book/agents/terrain-scatter-colliders.md, Q-08, ADR-8029).
//!
//! Read-only: it places the set on the CPU exactly as the drawn field would with the same
//! origin, and reports the colliders, their digest and whether the anchor sits on the map's
//! sample lattice (the condition for rocks that do not move when the map grows).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_terrain::{
    MeshExtent, PlacementInputs, PlacementOrigin, ScatterError, SceneryCollider, SceneryColliderSettings,
    Terrain, WorldAnchor, bake_scenery_colliders,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};
use crate::terrain_heights::target_terrain;

/// Most colliders one reply lists (the digest and count always cover all of them).
pub(crate) const MAX_LISTED: u64 = 4096;
/// Colliders listed when `limit` is absent.
const DEFAULT_LISTED: u64 = 64;

const HINT: &str = "see book/agents/terrain-scatter-colliders.md";

fn coded(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, reason, message, HINT)
}

fn pair(params: &Value, key: &str) -> Result<Option<[f32; 2]>, RpcError> {
    let Some(v) = params.get(key) else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params(format!("`{key}` must be [x, z], two finite numbers"));
    let a = v.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
    let n = |i: usize| {
        a[i].as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .ok_or_else(bad)
    };
    Ok(Some([n(0)?, n(1)?]))
}

fn number(params: &Value, key: &str, default: f32, min: f32, max: f32) -> Result<f32, RpcError> {
    match params.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite() && (min..=max).contains(f))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number in {min}..={max}"))),
    }
}

/// `{ footprint: [dx, dz], height }`, every value finite and not negative.
fn extent(v: &Value, what: &str) -> Result<MeshExtent, RpcError> {
    let bad = || {
        RpcError::invalid_params(format!(
            "{what} must be {{ footprint: [dx, dz], height }} with finite values >= 0"
        ))
    };
    let footprint = pair(v, "footprint")?.ok_or_else(bad)?;
    let height = v
        .get("height")
        .and_then(Value::as_f64)
        .map(|f| f as f32)
        .ok_or_else(bad)?;
    if !height.is_finite() || height < 0.0 || footprint.iter().any(|f| *f < 0.0) {
        return Err(bad());
    }
    Ok(MeshExtent { footprint, height })
}

/// FNV-1a 64 over every collider's four millimetre values, little-endian.
fn digest(colliders: &[SceneryCollider]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in colliders {
        for v in [c.x_mm, c.z_mm, c.y_mm, c.r_mm] {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}

/// `terrain.scatter_colliders`.
pub fn scatter_colliders(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params.get("path").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`path` (a .scatter.ron under the project root) is required")
    })?;
    // Every parameter is checked before the terrain is touched.
    let anchor = pair(params, "anchor")?;
    let pin_altitude = match params.get("pin_altitude") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`pin_altitude` must be a boolean"))?,
    };
    if pin_altitude && anchor.is_none() {
        return Err(RpcError::invalid_params("`pin_altitude` needs `anchor`"));
    }
    let defaults = SceneryColliderSettings::default();
    let settings = SceneryColliderSettings {
        threshold_m: number(params, "threshold_m", defaults.threshold_m, 0.0, 1000.0)?,
        radius_per_scale: number(params, "radius_per_scale", defaults.radius_per_scale, 0.0, 100.0)?,
        clip_half_extent: pair(params, "clip_half_extent")?,
        mirror_x: match params.get("mirror_x") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`mirror_x` must be a boolean"))?,
        },
    };
    if settings
        .clip_half_extent
        .is_some_and(|c| c.iter().any(|v| *v < 0.0))
    {
        return Err(RpcError::invalid_params(
            "`clip_half_extent` must not be negative",
        ));
    }
    let mut extents = BTreeMap::new();
    if let Some(v) = params.get("extents") {
        let map = v
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("`extents` must be an object of mesh path -> extent"))?;
        for (mesh, e) in map {
            extents.insert(mesh.clone(), extent(e, &format!("`extents.{mesh}`"))?);
        }
    }
    let default_extent = params
        .get("default_extent")
        .map(|v| extent(v, "`default_extent`"))
        .transpose()?;
    let limit = match params.get("limit") {
        None => DEFAULT_LISTED,
        Some(v) => v.as_u64().filter(|n| *n <= MAX_LISTED).ok_or_else(|| {
            RpcError::invalid_params(format!("`limit` must be an integer 0..={MAX_LISTED}"))
        })?,
    };
    let set = crate::terrain_level::read_scatter(world, path)?;
    let entity = target_terrain(world, params)?;
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| RpcError::internal("terrain vanished"))?;
    let map = &terrain.map;
    let origin = match anchor {
        None => PlacementOrigin::MapEdge,
        Some(origin) if pin_altitude => PlacementOrigin::World(WorldAnchor::pinned_to(origin, map)),
        Some(origin) => PlacementOrigin::World(WorldAnchor::at(origin)),
    };
    let inputs = PlacementInputs {
        map,
        holes: terrain.holes.as_ref(),
        layers: Some(&terrain.layers),
        density_maps: None,
    };
    let colliders = bake_scenery_colliders(
        &set,
        inputs,
        &origin,
        |mesh: &str| extents.get(mesh).copied().or(default_extent),
        &settings,
    )
    .map_err(|e| match e {
        ScatterError::Asset { path: mesh, reason } if reason.starts_with("no mesh extent") => coded(
            protocol::INVALID_PARAMS,
            "scatter_colliders_extent",
            format!("{mesh}: {reason}; list it in `extents` or pass `default_extent`"),
        ),
        other => coded(
            protocol::INVALID_PARAMS,
            "scatter_colliders_place",
            format!("{path}: {other}"),
        ),
    })?;
    let (origin_name, aligned, altitude_range) = match origin {
        PlacementOrigin::MapEdge => ("map_edge", Value::Null, Value::Null),
        PlacementOrigin::World(a) => (
            "world",
            a.lattice_aligned(map).map_or(Value::Null, |k| json!(k)),
            a.altitude_range.map_or(Value::Null, |r| json!(r)),
        ),
    };
    let listed: Vec<[i64; 4]> = colliders
        .iter()
        .take(limit as usize)
        .map(|c| [c.x_mm, c.z_mm, c.y_mm, c.r_mm])
        .collect();
    Ok(json!({
        "path": path,
        "entity": entity.to_bits(),
        "origin": origin_name,
        "anchor": anchor,
        "aligned": aligned,
        "altitude_range": altitude_range,
        "threshold_m": settings.threshold_m,
        "count": colliders.len(),
        "digest": format!("{:#018x}", digest(&colliders)),
        "colliders": listed,
        "truncated": colliders.len() > listed.len(),
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap};

    use super::*;

    fn reason(e: &RpcError) -> Option<&str> {
        e.data.as_ref().and_then(|d| d["code"].as_str())
    }

    const SET: &str = r#"(
        version: 2,
        seed: 2591,
        classes: [
            (
                name: "boulders",
                meshes: [(mesh: "rocks/boulder.glb", weight: 1.0, min_scale: 0.6, max_scale: 2.0)],
                cell_size: 4.0,
                density: 1.5,
            ),
            (
                name: "pebbles",
                meshes: [(mesh: "rocks/pebble.glb", weight: 1.0, min_scale: 0.2, max_scale: 0.5)],
                cell_size: 2.0,
                density: 1.0,
            ),
        ],
    )"#;

    /// A world with an asset server on `root` (holding the set) and a 64 m terrain of `n`
    /// samples, `offset` samples of which lie before the shared 64 m interior.
    fn world_with(root: &Path, n: u32, width: f32, offset: i64) -> World {
        let mut world = World::new();
        std::fs::create_dir_all(root.join("maps")).expect("root");
        std::fs::write(root.join("maps/arena.scatter.ron"), SET).expect("set");
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        let mut heights = Vec::new();
        for row in 0..n {
            for col in 0..n {
                let (c, r) = (i64::from(col) - offset, i64::from(row) - offset);
                heights.push(0.25 + (c * 3 + r * 5).rem_euclid(11) as f32 * 0.01);
            }
        }
        let map = TerrainMap::from_heights(n, n, heights, width, 10.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            width,
            10.0,
            0.0,
            CdlodConfig::default(),
            1,
            TerrainLayers::default(),
        )
        .expect("terrain");
        world.spawn(terrain);
        world
    }

    fn params(extra: Value) -> Value {
        let mut p = json!({
            "path": "maps/arena.scatter.ron",
            "extents": {
                "rocks/boulder.glb": { "footprint": [1.6, 1.2], "height": 1.4 },
                "rocks/pebble.glb": { "footprint": [0.4, 0.4], "height": 0.3 },
            },
            "limit": 4096,
        });
        if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                p.insert(k.clone(), v.clone());
            }
        }
        p
    }

    #[test]
    fn world_anchored_colliders_do_not_move_when_the_terrain_grows() {
        let root = regia_test_utils::unique_temp_dir("scatter_colliders_grow");
        // 64 m inner terrain at 1 m spacing, and the same samples inside a 96 m terrain.
        let mut inner = world_with(&root.join("a"), 65, 64.0, 0);
        let mut grown = world_with(&root.join("b"), 97, 96.0, 16);
        let call = |world: &mut World, extra: Value| scatter_colliders(world, &params(extra)).expect("bake");
        let world_anchor = json!({ "anchor": [0.0, 0.0], "clip_half_extent": [31.0, 31.0] });
        let a = call(&mut inner, world_anchor.clone());
        let b = call(&mut grown, world_anchor);
        assert_eq!(a["origin"], "world");
        assert_eq!(a["aligned"], json!([32, 32]));
        assert_eq!(b["aligned"], json!([48, 48]));
        assert!(a["count"].as_u64().unwrap_or(0) > 20, "{a}");
        assert_eq!(a["digest"], b["digest"]);
        assert_eq!(a["colliders"], b["colliders"]);
        // Edge-anchored, the grown terrain renumbers the cells.
        let edge = json!({ "clip_half_extent": [31.0, 31.0] });
        let (c, d) = (call(&mut inner, edge.clone()), call(&mut grown, edge));
        assert_eq!(c["origin"], "map_edge");
        assert_eq!(c["aligned"], Value::Null);
        assert_ne!(c["digest"], d["digest"]);
        // Pebbles never reach the 0.7 m threshold: every collider is a boulder (r >= 0.4 · 0.6 · 1.39).
        assert!(
            a["colliders"]
                .as_array()
                .is_some_and(|l| l.iter().all(|c| c[3].as_i64().unwrap_or(0) >= 330))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_truncates_the_list_not_the_digest() {
        let root = regia_test_utils::unique_temp_dir("scatter_colliders_limit");
        let mut world = world_with(&root, 65, 64.0, 0);
        let all = scatter_colliders(&mut world, &params(json!({ "anchor": [0.0, 0.0] }))).expect("bake");
        let few = scatter_colliders(&mut world, &params(json!({ "anchor": [0.0, 0.0], "limit": 3 })))
            .expect("bake");
        assert_eq!(few["colliders"].as_array().map(Vec::len), Some(3));
        assert_eq!(few["truncated"], true);
        assert_eq!(all["truncated"], false);
        assert_eq!(few["digest"], all["digest"]);
        assert_eq!(few["count"], all["count"]);
        let pinned = scatter_colliders(
            &mut world,
            &params(json!({ "anchor": [0.0, 0.0], "pin_altitude": true })),
        )
        .expect("bake");
        assert!(pinned["altitude_range"].is_array());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refusals_have_codes_and_change_nothing() {
        let root = regia_test_utils::unique_temp_dir("scatter_colliders_refuse");
        let mut world = world_with(&root, 65, 64.0, 0);
        let mut no_extents = params(json!({}));
        if let Some(p) = no_extents.as_object_mut() {
            p.remove("extents");
        }
        let e = scatter_colliders(&mut world, &no_extents).expect_err("no extents");
        assert_eq!(reason(&e), Some("scatter_colliders_extent"), "{e:?}");
        // A default extent covers every mesh.
        let mut with_default = no_extents.clone();
        if let Some(p) = with_default.as_object_mut() {
            p.insert(
                "default_extent".into(),
                json!({ "footprint": [1.0, 1.0], "height": 2.0 }),
            );
        }
        assert!(scatter_colliders(&mut world, &with_default).is_ok());
        for bad in [
            json!({ "anchor": [0.0] }),
            json!({ "anchor": [0.0, "x"] }),
            json!({ "pin_altitude": true }),
            json!({ "threshold_m": -1.0 }),
            json!({ "clip_half_extent": [-1.0, 4.0] }),
            json!({ "mirror_x": 1 }),
            json!({ "limit": 5000 }),
            json!({ "extents": { "rocks/boulder.glb": { "footprint": [1.0, 1.0], "height": -2.0 } } }),
        ] {
            let e = scatter_colliders(&mut world, &params(bad.clone())).expect_err("refused");
            assert_eq!(e.code, protocol::INVALID_PARAMS, "{bad}: {e:?}");
        }
        let missing = scatter_colliders(&mut world, &params(json!({ "path": "maps/none.scatter.ron" })))
            .expect_err("missing file");
        assert_eq!(reason(&missing), Some("scatter_rules_read"), "{missing:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
