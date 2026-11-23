// SPDX-License-Identifier: MPL-2.0
//! `terrain.seam_report`: the render seam of open mesh-layer mouths (ADR-8291, spec 26 § 2a.11).
//!
//! The measurement is [`regia_terrain::seam_report`]. This module only chooses the eye, the pass
//! and which mouth, and maps a missing hole to a protocol code.

use regia_app::prelude::*;
use regia_terrain::{
    BandDrawCounts, LodSelection, Terrain, TerrainDrawPasses, TerrainMeshLayers, cascade_min_level,
    node_world_size, select_nodes, select_nodes_min_level,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `terrain.seam_report`: gap and lip of the open mouths against the drawn terrain.
///
/// `hole` limits the report to one filled mouth. `pass` is `main` or `cascade0`..`cascade3`.
/// `eye` is `[x, z]` in metres; omitted, the lowest `Camera`'s translation is
/// used, otherwise the origin. `texel_m` is the cascade texel (default: half of that cascade's
/// historical cell, so the cascade stops at that level).
pub fn seam_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let hole = match params.get("hole") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => return Err(RpcError::invalid_params("`hole` must be a string")),
    };
    let pass = match params.get("pass") {
        None | Some(Value::Null) => "main".to_owned(),
        Some(Value::String(pass)) => pass.clone(),
        Some(_) => return Err(RpcError::invalid_params("`pass` must be a string")),
    };
    if !is_pass(&pass) {
        return Err(RpcError::invalid_params(
            "`pass` must be main, cascade0, cascade1, cascade2 or cascade3",
        ));
    }
    let eye_param = eye_param(params)?;
    let texel_m = texel_param(params)?;

    let _ = world.register_component::<TerrainMeshLayers>();
    let _ = world.register_component::<Terrain>();
    let has_layers = {
        let mut query = world.query::<&TerrainMeshLayers>();
        query.iter(world).next().is_some()
    };
    let terrain_entity = first_terrain(world);

    let Some(entity) = terrain_entity else {
        if let Some(id) = hole.as_deref() {
            return Err(unknown_hole(id));
        }
        return Ok(empty_report(&pass, eye_param.unwrap_or([0.0, 0.0])));
    };
    if !has_layers {
        if let Some(id) = hole.as_deref() {
            return Err(missing_fill(world, entity, id));
        }
        return Ok(empty_report(&pass, eye_param.unwrap_or([0.0, 0.0])));
    }
    if let Some(id) = hole.as_deref()
        && !mouth_exists(world, id)
    {
        return Err(missing_fill(world, entity, id));
    }

    // The passes the draw ran last frame (ADR-8470): without an `eye` or `texel_m`, a pass the
    // draw ran is measured on the selection it drew, around its own morph centre.
    let drawn = world.get_resource::<TerrainDrawPasses>().cloned();
    let passes = drawn.as_ref().map(TerrainDrawPasses::names).unwrap_or_default();
    // Every terrain pass's band draws, main camera and texture views (ADR-8824).
    let band_draws = drawn.as_ref().and_then(TerrainDrawPasses::band_draws).map_or(
        Value::Null,
        |(main, views)| json!({ "main": band_counts(&main), "views": band_counts(&views) }),
    );
    let drawn_pass = drawn
        .filter(|_| eye_param.is_none() && texel_m.is_none())
        .and_then(|handle| handle.pass(&pass));
    let view = mouths_view(world, hole.as_deref());
    let _span = tracing::info_span!("terrain_seam_report_agent", pass = %pass).entered();
    // The band vertices the pass drew (ADR-8569); 0 for a pass not drawn or drawn without bands.
    let band_vertices = drawn_pass.as_ref().map_or(0, |d| d.band.len());
    let (selection, eye, source) = match drawn_pass {
        Some(drawn) => (drawn.selection, drawn.eye, "draw"),
        None => {
            let eye = eye_param.unwrap_or_else(|| camera_xz(world).unwrap_or([0.0, 0.0]));
            let terrain = world.get::<Terrain>(entity).ok_or_else(no_terrain)?;
            (selection_for(terrain, eye, &pass, texel_m)?, eye, "selected")
        }
    };
    let report = {
        let terrain = world.get::<Terrain>(entity).ok_or_else(no_terrain)?;
        regia_terrain::seam_report(terrain, &view, &selection, eye)
    };
    let mouths: Vec<Value> = view
        .mouths()
        .iter()
        .map(|mouth| {
            json!({
                "hole": mouth.hole,
                "open": mouth.open,
            })
        })
        .collect();
    Ok(json!({
        "pass": pass,
        "eye": eye,
        "mouths": mouths,
        "max_gap_m": report.max_gap_m,
        "max_lip_m": report.max_lip_m,
        "worst_at": report.worst_at,
        "ring_vertices": report.ring_vertices,
        "source": source,
        "passes": passes,
        "band_vertices": band_vertices,
        "band_draws": band_draws,
    }))
}

/// One camera's band draws per pass, as `terrain.seam_report` returns them (ADR-8824).
fn band_counts(c: &BandDrawCounts) -> Value {
    json!({
        "color": c.color,
        "depth": c.depth,
        "velocity": c.velocity,
        "receiver": c.receiver,
        "cascades": c.cascades,
        "cascade_regions": c.cascade_regions,
        "precip_tile": c.precip_tile,
    })
}

fn is_pass(pass: &str) -> bool {
    matches!(pass, "main" | "cascade0" | "cascade1" | "cascade2" | "cascade3")
}

fn eye_param(params: &Value) -> Result<Option<[f32; 2]>, RpcError> {
    match params.get("eye") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() == 2 => {
            let x = items[0]
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`eye` must be [x, z]"))?;
            let z = items[1]
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`eye` must be [x, z]"))?;
            let eye = [x as f32, z as f32];
            if !eye[0].is_finite() || !eye[1].is_finite() {
                return Err(RpcError::invalid_params("`eye` must be finite"));
            }
            Ok(Some(eye))
        }
        Some(_) => Err(RpcError::invalid_params("`eye` must be [x, z]")),
    }
}

fn texel_param(params: &Value) -> Result<Option<f32>, RpcError> {
    match params.get("texel_m") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let texel = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`texel_m` must be a number"))?
                as f32;
            if !texel.is_finite() || texel <= 0.0 {
                return Err(RpcError::invalid_params(
                    "`texel_m` must be a finite number greater than 0",
                ));
            }
            Ok(Some(texel))
        }
    }
}

fn empty_report(pass: &str, eye: [f32; 2]) -> Value {
    json!({
        "pass": pass,
        "eye": eye,
        "mouths": [],
        "max_gap_m": 0.0,
        "max_lip_m": 0.0,
        "worst_at": [0.0, 0.0, 0.0],
        "ring_vertices": 0,
        "band_vertices": 0,
        "band_draws": null,
    })
}

fn unknown_hole(id: &str) -> RpcError {
    RpcError::new(
        protocol::NOT_FOUND,
        "unknown_hole",
        format!("no hole {id:?}"),
        "terrain.get lists holes.named; omit hole to report every open mouth",
    )
}

fn hole_not_filled(id: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "hole_not_filled",
        format!("hole {id:?} has no mesh-layer mouth"),
        "load the cooked meshlayers for this map, or omit hole",
    )
}

fn no_terrain() -> RpcError {
    RpcError::not_found(
        "no Terrain in this world",
        "spawn an entity with regia_terrain::Terrain",
    )
}

fn missing_fill(world: &World, entity: Entity, id: &str) -> RpcError {
    let named = world
        .get::<Terrain>(entity)
        .and_then(|terrain| terrain.holes.as_ref())
        .is_some_and(|mask| mask.named_hole(id).is_some());
    if named {
        hole_not_filled(id)
    } else {
        unknown_hole(id)
    }
}

fn first_terrain(world: &mut World) -> Option<Entity> {
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    query.iter(world).min_by_key(|entity| entity.to_bits())
}

fn mouth_exists(world: &mut World, id: &str) -> bool {
    let mut query = world.query::<&TerrainMeshLayers>();
    query.iter(world).any(|layers| layers.mouth(id).is_some())
}

fn mouths_view(world: &mut World, hole: Option<&str>) -> TerrainMeshLayers {
    let mut view = TerrainMeshLayers::new();
    let mut query = world.query::<&TerrainMeshLayers>();
    for layers in query.iter(world) {
        for mouth in layers.mouths() {
            if hole.is_none_or(|id| mouth.hole == id) {
                view.push_mouth(mouth.clone());
            }
        }
    }
    view
}

fn camera_xz(world: &mut World) -> Option<[f32; 2]> {
    let _ = world.register_component::<regia_render::Camera>();
    let _ = world.register_component::<regia_scene::Transform>();
    let mut query = world.query::<(Entity, &regia_render::Camera, &regia_scene::Transform)>();
    let mut best: Option<(u64, [f32; 2])> = None;
    for (entity, _, transform) in query.iter(world) {
        let xz = [transform.translation.x, transform.translation.z];
        if !xz[0].is_finite() || !xz[1].is_finite() {
            continue;
        }
        let bits = entity.to_bits();
        if best.is_none_or(|(so_far, _)| bits < so_far) {
            best = Some((bits, xz));
        }
    }
    best.map(|(_, xz)| xz)
}

fn selection_for(
    terrain: &Terrain,
    eye: [f32; 2],
    pass: &str,
    texel_m: Option<f32>,
) -> Result<LodSelection, RpcError> {
    if pass == "main" {
        return Ok(select_nodes(
            &terrain.map,
            &terrain.minmax,
            &terrain.cdlod,
            terrain.root_level,
            eye,
            None,
        ));
    }
    let k = pass.as_bytes().last().copied().unwrap_or(b'0') - b'0';
    let k = u32::from(k).min(terrain.root_level);
    let grid = terrain.cdlod.grid_size.max(1);
    let world = terrain.map.world_width();
    let texel = texel_m.unwrap_or_else(|| {
        let cell = node_world_size(world, terrain.root_level, k) / grid as f32;
        cell / 2.0
    });
    let level = cascade_min_level(world, terrain.root_level, grid, texel);
    Ok(select_nodes_min_level(
        &terrain.map,
        &terrain.minmax,
        &terrain.cdlod,
        terrain.root_level,
        eye,
        None,
        level,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{BandMesh, HoleShape, LayerMouth};

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|data| data["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    fn world_with_terrain() -> (World, Entity) {
        let mut world = World::new();
        let n = 65u32;
        let hf = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: vec![32768; (n * n) as usize],
        };
        let terrain = Terrain::new(
            &hf,
            64.0,
            100.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            3,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let entity = world.spawn(terrain).id();
        (world, entity)
    }

    fn rect(id: &str, center_x: f32) -> Value {
        json!({
            "id": id,
            "shape": {
                "rect": {
                    "center": [center_x, 0.0],
                    "half_extents": [2.0, 2.0],
                    "rotation_radians": 0.0
                }
            }
        })
    }

    #[test]
    fn an_empty_world_reports_no_seam_and_a_bad_pass_is_refused() {
        let mut world = World::new();
        let report = seam_report(&mut world, &json!({})).expect("empty");
        assert_eq!(report["mouths"], json!([]));
        assert_eq!(report["max_gap_m"], json!(0.0));
        assert_eq!(report["ring_vertices"], json!(0));
        let err = seam_report(&mut world, &json!({ "pass": "shadow" })).expect_err("pass");
        assert_eq!(reason(&err), "F0002");
        let err = seam_report(&mut world, &json!({ "hole": "gate" })).expect_err("no terrain");
        assert_eq!(reason(&err), "unknown_hole");
    }

    /// A mesh layer fixes the footprint. Opening, closing and removing still succeed.
    #[test]
    fn filled_hole_shape_is_fixed() {
        let (mut world, entity) = world_with_terrain();
        crate::terrain_runtime::set_hole(&mut world, &rect("gate", 4.0)).expect("add");
        let mut layers = TerrainMeshLayers::new();
        layers.push_mouth(LayerMouth {
            hole: "gate".to_owned(),
            rings: vec![vec![[4.0, 0.0, 0.0], [5.0, 0.0, 0.0], [4.0, 0.0, 1.0]]],
            band: BandMesh::default(),
            collar: Vec::new(),
            throat: Vec::new(),
            body: Vec::new(),
            open: true,
        });
        world.entity_mut(entity).insert(layers);

        let err = crate::terrain_runtime::set_hole(&mut world, &rect("gate", 8.0)).expect_err("shape");
        assert_eq!(reason(&err), "hole_filled_by_layer");
        let shape = world
            .get::<Terrain>(entity)
            .and_then(|terrain| terrain.holes.as_ref())
            .and_then(|mask| mask.named_hole("gate"))
            .map(|hole| hole.shape.clone())
            .expect("hole kept");
        assert!(matches!(shape, HoleShape::Rect { center, .. } if center[0] == 4.0));

        crate::terrain_runtime::set_hole(&mut world, &json!({ "id": "gate", "open": false })).expect("close");
        assert!(
            world
                .get::<TerrainMeshLayers>(entity)
                .is_some_and(|layers| layers.mouth("gate").is_some_and(|mouth| !mouth.open))
        );
        let closed = seam_report(&mut world, &json!({ "hole": "gate", "eye": [0.0, 0.0] })).expect("closed");
        assert_eq!(closed["max_gap_m"], json!(0.0));
        assert_eq!(closed["ring_vertices"], json!(0));

        crate::terrain_runtime::set_hole(&mut world, &json!({ "id": "gate", "open": true })).expect("open");
        let open = seam_report(
            &mut world,
            &json!({ "hole": "gate", "pass": "cascade0", "eye": [1.0, 0.0] }),
        )
        .expect("open report");
        assert_eq!(open["pass"], json!("cascade0"));
        assert_eq!(open["mouths"][0]["open"], json!(true));

        crate::terrain_runtime::set_hole(&mut world, &rect("plain", 1.0)).expect("unfilled");
        let err = seam_report(&mut world, &json!({ "hole": "plain" })).expect_err("not filled");
        assert_eq!(reason(&err), "hole_not_filled");
        let err = seam_report(&mut world, &json!({ "hole": "missing" })).expect_err("unknown");
        assert_eq!(reason(&err), "unknown_hole");

        crate::terrain_runtime::set_hole(&mut world, &json!({ "id": "gate", "remove": true }))
            .expect("remove");
        assert_eq!(open["source"], json!("selected"));
        assert_eq!(open["passes"], json!([]));
        assert!(
            world
                .get::<TerrainMeshLayers>(entity)
                .is_some_and(|layers| layers.mouth("gate").is_none())
        );
    }

    /// Brief W-15 item 4 (ADR-8470): with the draw's pass handle linked, `pass` names a pass the
    /// draw ran last frame and the report measures the selection that pass drew, around its own
    /// morph centre (not a fresh selection); the reply lists the drawn passes. An explicit `eye`
    /// selects again.
    #[test]
    fn seam_report_reads_the_drawn_pass() {
        let (mut world, entity) = world_with_terrain();
        crate::terrain_runtime::set_hole(&mut world, &rect("gate", 4.0)).expect("add");
        let mut layers = TerrainMeshLayers::new();
        layers.push_mouth(LayerMouth {
            hole: "gate".to_owned(),
            rings: vec![vec![[4.0, 0.0, 0.0], [5.0, 0.0, 0.0], [4.0, 0.0, 1.0]]],
            band: BandMesh::default(),
            collar: Vec::new(),
            throat: Vec::new(),
            body: Vec::new(),
            open: true,
        });
        world.entity_mut(entity).insert(layers);
        // The draw drew cascade1 as the root node alone, around (9, 9); main around the origin.
        let (root, root_level) = {
            let terrain = world.get::<Terrain>(entity).expect("terrain");
            (
                regia_terrain::QuadNode::root(terrain.root_level),
                terrain.root_level,
            )
        };
        let coarse = [regia_terrain::NodeDraw {
            node: root,
            morph_start: 0.0,
            morph_end: 0.0,
        }];
        let main = {
            let terrain = world.get::<Terrain>(entity).expect("terrain");
            select_nodes(
                &terrain.map,
                &terrain.minmax,
                &terrain.cdlod,
                root_level,
                [0.0, 0.0],
                None,
            )
        };
        let handle = TerrainDrawPasses::linked();
        handle.publish([
            ("main", [0.0, 0.0], main.nodes.as_slice()),
            ("cascade1", [9.0, 9.0], coarse.as_slice()),
        ]);
        world.insert_resource(handle.clone());

        let report = seam_report(&mut world, &json!({ "hole": "gate", "pass": "cascade1" })).expect("drawn");
        assert_eq!(report["source"], json!("draw"));
        assert_eq!(report["eye"], json!([9.0, 9.0]));
        assert_eq!(report["passes"], json!(["main", "cascade1"]));
        assert_eq!(report["band_vertices"], json!(0), "no band published yet");
        handle.set_band("cascade1", &[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        let banded = seam_report(&mut world, &json!({ "hole": "gate", "pass": "cascade1" })).expect("banded");
        assert_eq!(banded["band_vertices"], json!(2), "the band the pass drew");
        // ADR-8824: an unlinked counter set answers null; a draw's counters answer every pass,
        // main camera and texture views (zero here: nothing drew).
        assert_eq!(banded["band_draws"], Value::Null, "no counters linked");
        handle.link_stats(std::sync::Arc::default());
        let counted =
            seam_report(&mut world, &json!({ "hole": "gate", "pass": "cascade1" })).expect("counted");
        let zero = json!({
            "color": 0, "depth": 0, "velocity": 0, "receiver": 0,
            "cascades": [0, 0, 0, 0], "cascade_regions": [0, 0, 0, 0], "precip_tile": 0,
        });
        assert_eq!(counted["band_draws"], json!({ "main": zero, "views": zero }));
        let view = mouths_view(&mut world, Some("gate"));
        let expected = {
            let terrain = world.get::<Terrain>(entity).expect("terrain");
            regia_terrain::seam_report(
                terrain,
                &view,
                &LodSelection {
                    nodes: coarse.to_vec(),
                },
                [9.0, 9.0],
            )
        };
        assert_eq!(report["ring_vertices"], json!(expected.ring_vertices));
        assert_eq!(report["max_gap_m"], json!(expected.max_gap_m));
        assert_eq!(report["max_lip_m"], json!(expected.max_lip_m));

        // A pass the draw did not run, or an explicit eye, selects again.
        let other =
            seam_report(&mut world, &json!({ "hole": "gate", "pass": "cascade2" })).expect("cascade2");
        assert_eq!(other["source"], json!("selected"));
        let explicit = seam_report(
            &mut world,
            &json!({ "hole": "gate", "pass": "cascade1", "eye": [9.0, 9.0] }),
        )
        .expect("eye");
        assert_eq!(explicit["source"], json!("selected"));
    }
}
