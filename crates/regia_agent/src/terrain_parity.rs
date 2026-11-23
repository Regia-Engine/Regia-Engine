// SPDX-License-Identifier: MPL-2.0
//! `terrain.parity` (ADR-873, `book/agents/terrain-parity.md`): one read-only call that checks
//! a terrain's three agreements an agent cannot see in a screenshot.
//!
//! - **LOD seams**: the CDLOD selection from an eye, every shared edge measured on the CPU
//!   mirror of the vertex shader ([`regia_terrain::lod_seam_report`]).
//! - **Collision**: the heightfield collider the map builds
//!   ([`regia_terrain::TerrainMap::collision_heightfield`], holes cut) and how far its
//!   triangles can be from the simulation's bilinear sampler.
//! - **Save**: how far the live (sculpted) heights are from the 16-bit heightfield they save to
//!   ([`regia_terrain::TerrainMap::save_drift`]), which is what a reload and the game's
//!   simulation read.

use regia_app::prelude::*;
use regia_terrain::{HeightMips, Terrain, TerrainView, lod_seam_report, select_nodes, terrain_holes};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Largest drawn-edge gap, metres, that still counts as crack-free (float rounding of two
/// evaluations of one vertex is far below it).
const SEAM_TOLERANCE_M: f32 = 1e-3;

/// The terrain a call names (`entity`) or the first one.
fn terrain_entity(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    if let Some(value) = params.get("entity") {
        let bits = value
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
        if world.get::<Terrain>(entity).is_none() {
            return Err(RpcError::not_found(
                format!("entity {bits} has no Terrain"),
                "omit `entity` to use the first terrain",
            ));
        }
        return Ok(entity);
    }
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    query.iter(world).next().ok_or_else(|| {
        RpcError::not_found(
            "no Terrain in this world",
            "spawn an entity with regia_terrain::Terrain (the level's heightfield)",
        )
    })
}

/// The eye the LOD selection is checked from: `eye` [x, z], else the [`TerrainView`]
/// camera, else the map centre.
fn eye_param(world: &World, params: &Value) -> Result<([f32; 2], &'static str), RpcError> {
    if let Some(value) = params.get("eye") {
        let xz = value
            .as_array()
            .filter(|a| a.len() == 2)
            .and_then(|a| Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32]))
            .filter(|p| p.iter().all(|v| v.is_finite()))
            .ok_or_else(|| RpcError::invalid_params("`eye` must be [x, z] in meters"))?;
        return Ok((xz, "param"));
    }
    if let Some(view) = world.get_resource::<TerrainView>() {
        return Ok(([view.position[0], view.position[2]], "terrain_view"));
    }
    Ok(([0.0, 0.0], "map_center"))
}

/// `terrain.parity`: LOD seams from an eye, collider-vs-sampler bound, and save drift for one
/// terrain (`entity`, or the first). `ok` is true when the selection is crack-free (gaps under
/// 1 mm, no level jump over one) and the live heights save exactly.
pub fn parity(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_agent", "terrain.parity").entered();
    let entity = terrain_entity(world, params)?;
    let (eye, eye_source) = eye_param(world, params)?;
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| RpcError::internal("the terrain vanished mid-call"))?;
    let map = &terrain.map;

    let selection = select_nodes(
        map,
        &terrain.minmax,
        &terrain.cdlod,
        terrain.root_level,
        eye,
        None,
    );
    let mips = HeightMips::build(map);
    let seams = lod_seam_report(map, &mips, &terrain.cdlod, terrain.root_level, &selection, eye);
    let crack_free = seams.is_crack_free(SEAM_TOLERANCE_M);

    let holes = terrain_holes(world, terrain);
    let collider = map.collision_heightfield(holes.as_deref());
    let save = map.save_drift();
    let round_trips = save.round_trips();

    let to_json = |v: Result<Value, serde_json::Error>| {
        v.map_err(|e| RpcError::internal(format!("parity report serialization failed: {e}")))
    };
    let mut lod = to_json(serde_json::to_value(seams))?;
    if let Value::Object(fields) = &mut lod {
        fields.insert("eye".into(), json!(eye));
        fields.insert("eye_source".into(), json!(eye_source));
        fields.insert("crack_free".into(), json!(crack_free));
        fields.insert(
            "morph_fraction".into(),
            json!({
                "configured": terrain.cdlod.morph_fraction,
                "effective": terrain.cdlod.effective_morph_fraction(),
            }),
        );
    }
    let mut save_json = to_json(serde_json::to_value(save))?;
    if let Value::Object(fields) = &mut save_json {
        fields.insert("round_trips".into(), json!(round_trips));
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "ok": crack_free && round_trips,
        "lod": lod,
        "collision": {
            "nx": collider.nx,
            "nz": collider.nz,
            "scale": collider.scale,
            "cut_samples": collider.cut_samples,
            "max_bilinear_delta_m": collider.max_bilinear_delta(),
        },
        "save": save_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec2;
    use regia_terrain::{CdlodConfig, HoleShape, HolesMask, TerrainLayers, TerrainMap};

    fn hills() -> Terrain {
        let n = 129u32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let (x, z) = (col as f32, row as f32);
                heights.push(20.0 + 6.0 * (x * 0.07).sin() * (z * 0.05).cos() + 0.05 * x);
            }
        }
        let map = TerrainMap::from_heights(n, n, heights, 128.0, 100.0, 0.0).expect("map");
        Terrain::new(
            &map.to_heightfield(),
            128.0,
            100.0,
            0.0,
            CdlodConfig::with_grid_size(16),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    #[test]
    fn parity_reports_seams_collision_and_save() {
        let mut world = World::new();
        assert!(parity(&mut world, &json!({})).is_err(), "no terrain");
        let mut terrain = hills();
        terrain.holes = Some(
            HolesMask::from_shapes(vec![HoleShape::segment([-10.0, 0.0], [10.0, 0.0], 6.0)]).expect("mask"),
        );
        let entity = world.spawn(terrain).id();
        let out = parity(&mut world, &json!({ "eye": [5.0, -12.0] })).expect("parity");
        assert_eq!(out["entity"], json!(entity.to_bits()));
        assert_eq!(out["lod"]["eye_source"], json!("param"));
        assert_eq!(out["lod"]["crack_free"], json!(true), "{out}");
        assert!(
            out["lod"]["level_change_edges"].as_u64().expect("edges") > 0,
            "{out}"
        );
        assert_eq!(out["collision"]["nx"], json!(129));
        assert!(
            out["collision"]["cut_samples"].as_u64().expect("cut") > 0,
            "{out}"
        );
        assert_eq!(out["save"]["round_trips"], json!(true), "{out}");
        assert_eq!(out["ok"], json!(true), "{out}");

        // A sculpt past the top of the height range no longer saves exactly.
        world.insert_resource(TerrainView {
            position: [0.0, 40.0, 0.0],
            target: [0.0, 0.0, 10.0],
        });
        world
            .entity_mut(entity)
            .insert(regia_terrain::TerrainEdits::default());
        let mut query = world.query::<(&mut Terrain, &mut regia_terrain::TerrainEdits)>();
        let (mut terrain, mut edits) = query.get_mut(&mut world, entity).expect("terrain");
        let brush = regia_terrain::Brush {
            shape: regia_terrain::BrushShape::Circle { radius: 4.0 },
            mode: regia_terrain::BrushMode::Raise { amount: 500.0 },
            hardness: 1.0,
            ..regia_terrain::Brush::default()
        };
        regia_terrain::apply_stroke(&mut terrain, None, &mut edits, &brush, &[Vec2::new(30.0, 30.0)])
            .expect("stroke");
        let out = parity(&mut world, &json!({})).expect("parity");
        assert_eq!(out["lod"]["eye_source"], json!("terrain_view"));
        assert!(
            out["save"]["clamped_samples"].as_u64().expect("clamped") > 0,
            "{out}"
        );
        assert_eq!(out["save"]["round_trips"], json!(false));
        assert_eq!(out["ok"], json!(false));
    }

    #[test]
    fn parity_validates_its_params() {
        let mut world = World::new();
        world.spawn(hills());
        assert!(parity(&mut world, &json!({ "eye": [1.0] })).is_err());
        assert!(parity(&mut world, &json!({ "eye": "here" })).is_err());
        assert!(parity(&mut world, &json!({ "entity": "x" })).is_err());
        let stray = world.spawn_empty().id();
        assert!(parity(&mut world, &json!({ "entity": stray.to_bits() })).is_err());
        let out = parity(&mut world, &json!({})).expect("parity");
        assert_eq!(out["lod"]["eye_source"], json!("map_center"));
    }
}
