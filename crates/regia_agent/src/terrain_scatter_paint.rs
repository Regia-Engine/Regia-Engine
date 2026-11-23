// SPDX-License-Identifier: MPL-2.0
//! Painted mesh instances on the agent protocol (`docs/specs/82-terrain-editing.md` § 13,
//! ADR-1936, `book/agents/terrain-scatter-paint.md`): `terrain.scatter_stroke` paints or erases
//! with one brush stroke, `terrain.scatter_paint` reports what is painted, and
//! `terrain.save_scatter_paint` / `terrain.load_scatter_paint` write and read the
//! `<map>.scatterpaint.bin`. A stroke goes on the [`TerrainStrokeLog`] like a sculpt, so in the
//! editor it is one undo step.

use std::path::Path;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_terrain::{
    BrushShape, PaintedScatter, ScatterBrush, ScatterBrushMode, ScatterPaintError, ScatterSet, Terrain,
    TerrainEdits, TerrainScatterPaint, TerrainStrokeLog, apply_scatter_stroke, paintable_scatter_set,
    scatter_paint_path,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};
use crate::terrain_heights::{project_file, target_terrain};

const HINT: &str = "see book/agents/terrain-scatter-paint.md";

fn coded(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, reason, message, HINT)
}

fn paint_error(e: &ScatterPaintError) -> RpcError {
    let code = match e {
        ScatterPaintError::UnknownClass(_) | ScatterPaintError::Read(_) => protocol::NOT_FOUND,
        _ => protocol::INVALID_PARAMS,
    };
    coded(code, e.code(), e.to_string())
}

fn scatter_set(world: &World) -> Result<Arc<ScatterSet>, RpcError> {
    paintable_scatter_set(world).ok_or_else(|| {
        coded(
            protocol::NOT_FOUND,
            "scatter_paint_no_set",
            "no scatter set to paint from: insert a regia_terrain::RuntimeScatter, or a ScatterPaintSet for a host that draws its own field",
        )
    })
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number")))
        })
        .transpose()
}

/// `[min, max]` from `<name>_min` and `<name>_max`, each keeping `base` when absent.
fn range(params: &Value, name: &str, base: [f32; 2]) -> Result<[f32; 2], RpcError> {
    Ok([
        number(params, &format!("{name}_min"))?.unwrap_or(base[0]),
        number(params, &format!("{name}_max"))?.unwrap_or(base[1]),
    ])
}

fn point(value: &Value, name: &str) -> Result<Vec2, RpcError> {
    let pair: Option<[f32; 2]> = serde_json::from_value(value.clone()).ok();
    pair.map(Vec2::from)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, z] in meters")))
}

/// The path of a stroke call: `center`, then `to`, then `path`.
fn stroke_path(params: &Value) -> Result<Vec<Vec2>, RpcError> {
    let center = params
        .get("center")
        .ok_or_else(|| RpcError::invalid_params("`center` [x, z] is required"))?;
    let mut path = vec![point(center, "center")?];
    if let Some(to) = params.get("to") {
        path.push(point(to, "to")?);
    }
    if let Some(points) = params.get("path") {
        let points = points
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`path` must be a list of [x, z]"))?;
        for p in points {
            path.push(point(p, "path")?);
        }
    }
    Ok(path)
}

fn shape_of(params: &Value) -> Result<BrushShape, RpcError> {
    let radius = number(params, "radius")?;
    match crate::params::opt_str(params, "shape")?.unwrap_or("circle") {
        "circle" => Ok(BrushShape::Circle {
            radius: radius
                .ok_or_else(|| RpcError::invalid_params("`radius` (meters) is required for a circle"))?,
        }),
        "rectangle" | "rect" => {
            let side = |name: &str| -> Result<f32, RpcError> {
                number(params, name)?.or(radius.map(|r| r * 2.0)).ok_or_else(|| {
                    RpcError::invalid_params("a rectangle needs `width` and `length` (or `radius`)")
                })
            };
            Ok(BrushShape::Rectangle {
                width: side("width")?,
                length: side("length")?,
            })
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown shape `{other}`: circle | rectangle"
        ))),
    }
}

/// The brush a `terrain.scatter_stroke` call describes.
fn brush_of(params: &Value, set: &ScatterSet) -> Result<ScatterBrush, RpcError> {
    let class = crate::params::opt_str(params, "class")?;
    let mode = crate::params::opt_str(params, "mode")?.unwrap_or("paint");
    let mut brush = match mode {
        "paint" => {
            let name = class.ok_or_else(|| {
                RpcError::invalid_params("`class` (a class of the .scatter.ron) is required to paint")
            })?;
            let found = set
                .classes
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| paint_error(&ScatterPaintError::UnknownClass(name.to_owned())))?;
            ScatterBrush::for_class(found)
        }
        "erase" => ScatterBrush::eraser(class),
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown mode `{other}`: paint | erase"
            )));
        }
    };
    brush.shape = shape_of(params)?;
    brush.hardness = number(params, "hardness")?.unwrap_or(brush.hardness);
    brush.strength = number(params, "strength")?.unwrap_or(brush.strength);
    brush.density = number(params, "density")?.unwrap_or(brush.density);
    brush.scale = range(params, "scale", brush.scale)?;
    brush.slope = range(params, "slope", brush.slope)?;
    brush.height = range(params, "height", brush.height)?;
    brush.min_distance = number(params, "min_distance")?.unwrap_or(brush.min_distance);
    if let Some(seed) = params.get("seed") {
        let seed = seed
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`seed` must be a whole number"))?;
        brush.seed = seed as u32;
    }
    if let Some(wind) = params.get("wind") {
        brush.wind = wind
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`wind` must be a boolean"))?;
    }
    Ok(brush)
}

fn with_components(world: &mut World, entity: Entity) {
    let mut target = world.entity_mut(entity);
    if !target.contains::<TerrainEdits>() {
        target.insert(TerrainEdits::default());
    }
    if !target.contains::<TerrainScatterPaint>() {
        target.insert(TerrainScatterPaint::default());
    }
}

/// `terrain.scatter_stroke`: one brush stroke of painted instances on a terrain (`entity`, or the
/// first [`Terrain`]). A stamp at `center`, smeared through `to` / `path`.
pub fn stroke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let set = scatter_set(world)?;
    let brush = brush_of(params, &set)?;
    let path = stroke_path(params)?;
    let entity = target_terrain(world, params)?;
    with_components(world, entity);
    let mut query = world.query::<(&Terrain, &mut TerrainScatterPaint, &mut TerrainEdits)>();
    let Ok((terrain, mut paint, mut edits)) = query.get_mut(world, entity) else {
        return Err(RpcError::internal("terrain components vanished mid-call"));
    };
    let undo = apply_scatter_stroke(terrain, &mut paint, &mut edits, &set, &brush, &path)
        .map_err(|e| paint_error(&e))?;
    let instances = paint.0.len();
    let changed = undo.len();
    let d = undo.dirty();
    let cells: Vec<[u32; 2]> = undo.tiles().iter().map(|&(x, y)| [x, y]).collect();
    let mut log = world.get_resource_or_insert_with::<TerrainStrokeLog>(TerrainStrokeLog::default);
    let logged = changed > 0;
    if logged {
        // Under an editor every stroke becomes an undo step, however many arrive in one frame.
        log.push(entity, undo);
    }
    let (added, removed) = match brush.mode {
        ScatterBrushMode::Paint => (changed, 0),
        ScatterBrushMode::Erase => (0, changed),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "mode": brush.mode,
        "class": brush.class,
        "added": added,
        "removed": removed,
        "instances": instances,
        "undoable": logged,
        "dirty": { "col_start": d.col_start, "col_end": d.col_end,
                   "row_start": d.row_start, "row_end": d.row_end },
        "cells": cells,
    }))
}

/// `terrain.scatter_paint`: what a terrain has painted, per class, and whether the field draws it.
pub fn report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target_terrain(world, params)?;
    let set = paintable_scatter_set(world);
    let paint = world.get::<TerrainScatterPaint>(entity);
    let classes: Vec<Value> = paint.map_or_else(Vec::new, |paint| {
        paint
            .0
            .report()
            .into_iter()
            .map(|class| {
                let known = set
                    .as_ref()
                    .map(|set| set.classes.iter().any(|c| c.name == class.name));
                json!({
                    "name": class.name,
                    "instances": class.instances,
                    "tiles": class.tiles,
                    "in_set": known,
                })
            })
            .collect()
    });
    let instances = paint.map_or(0, |p| p.0.len());
    let unsaved = world
        .get::<TerrainEdits>(entity)
        .is_some_and(TerrainEdits::unsaved_scatter);
    let paintable: Vec<&str> = set
        .iter()
        .flat_map(|s| s.classes.iter().map(|c| c.name.as_str()))
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "instances": instances,
        "classes": classes,
        "unsaved": unsaved,
        "paintable": paintable,
    }))
}

/// The painted-scatter file of `path`: a name ending in `.scatterpaint.bin` as it is, a map or
/// level file's [`scatter_paint_path`].
fn file_name(params: &Value) -> Result<String, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (relative to the project root) is required"))?;
    Ok(if path.to_ascii_lowercase().ends_with(".scatterpaint.bin") {
        path.to_owned()
    } else {
        // Reply names use `/` on every host.
        scatter_paint_path(Path::new(path))
            .to_string_lossy()
            .replace('\\', "/")
    })
}

/// `terrain.save_scatter_paint`: writes the terrain's painted instances to the
/// `<map>.scatterpaint.bin` of `path` under the project root, atomically.
pub fn save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = file_name(params)?;
    let file = project_file(world, &name)?;
    let entity = target_terrain(world, params)?;
    let Some(paint) = world.get::<TerrainScatterPaint>(entity) else {
        return Err(RpcError::not_found(
            format!("entity {} has no painted instances", entity.to_bits()),
            "paint with terrain.scatter_stroke first",
        ));
    };
    let instances = paint.0.len();
    let write_failed = |e: String| coded(protocol::INTERNAL_ERROR, "scatter_paint_write", e);
    let bytes = paint.0.to_bytes().map_err(|e| write_failed(e.to_string()))?;
    crate::bake_export::write_atomic(&file, &bytes)
        .map_err(|e| write_failed(format!("{}: {e}", file.display())))?;
    if let Some(mut edits) = world.get_mut::<TerrainEdits>(entity) {
        edits.take_unsaved_scatter();
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "path": name,
        "instances": instances,
        "bytes": bytes.len(),
    }))
}

/// `terrain.load_scatter_paint`: replaces the terrain's painted instances with a
/// `<map>.scatterpaint.bin` under the project root and seats them on the terrain as it is now. A
/// load is not an undo step; strokes already in the journal still undo against the new store.
pub fn load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = file_name(params)?;
    let file = project_file(world, &name)?;
    let entity = target_terrain(world, params)?;
    let loaded = regia_terrain::load_scatter_paint(&file).map_err(|e| paint_error(&e))?;
    let Some(mut store): Option<PaintedScatter> = loaded else {
        return Err(coded(
            protocol::NOT_FOUND,
            "scatter_paint_read",
            format!("{name} does not exist"),
        ));
    };
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| RpcError::internal("terrain vanished mid-call"))?;
    let moved = store.reseat(&terrain.map, None);
    let instances = store.len();
    with_components(world, entity);
    world.entity_mut(entity).insert(TerrainScatterPaint(store));
    if let Some(mut edits) = world.get_mut::<TerrainEdits>(entity) {
        // What was just read is what the file holds, unless the terrain moved instances.
        edits.take_unsaved_scatter();
        if moved > 0 {
            edits.mark_scatter_unsaved();
        }
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "path": name,
        "instances": instances,
        "reseated": moved,
    }))
}

#[cfg(test)]
mod tests {
    use regia_terrain::{
        CdlodConfig, RockStyle, RuntimeScatter, ScatterClass, ScatterField, ScatterLibrary, ScatterMeshRef,
        ScatterPaintSet, ScatterSource, StrokeUndo, TerrainLayers, TerrainMap, procedural_rock,
    };

    use super::*;

    fn class() -> ScatterClass {
        ScatterClass {
            name: "trees".into(),
            density: 0.0,
            meshes: vec![ScatterMeshRef {
                mesh: "rock".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 2.0,
                scale_bias: 1.0,
            }],
            ..ScatterClass::default()
        }
    }

    fn set() -> ScatterSet {
        ScatterSet {
            classes: vec![class()],
            ..ScatterSet::default()
        }
    }

    fn terrain_at(offset: f32) -> Terrain {
        let n = 65u32;
        let heights = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: (0..n * n).map(|i| ((i % n) * 512) as u16).collect(),
        };
        Terrain::new(
            &heights,
            128.0,
            100.0,
            offset,
            CdlodConfig::default(),
            2,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    fn terrain() -> Terrain {
        terrain_at(0.0)
    }

    fn world_of(root: &Path, terrain: Terrain) -> (World, Entity) {
        let mut world = World::new();
        world.insert_resource(ScatterPaintSet(Arc::new(set())));
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        let entity = world.spawn(terrain).id();
        (world, entity)
    }

    fn world(root: &Path) -> (World, Entity) {
        world_of(root, terrain())
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn paint_args() -> Value {
        json!({ "class": "trees", "center": [0.0, 0.0], "radius": 30.0, "hardness": 1.0, "density": 0.5 })
    }

    #[test]
    fn a_stroke_paints_logs_and_reports() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-paint");
        let (mut world, entity) = world(&root);
        let out = stroke(&mut world, &paint_args()).expect("stroke");
        let added = out["added"].as_u64().expect("added");
        assert!(added > 400, "{out}");
        assert_eq!(out["mode"], json!("paint"));
        assert_eq!(out["instances"], json!(added));
        assert_eq!(out["undoable"], json!(true));
        let log = world.resource::<TerrainStrokeLog>();
        assert_eq!(log.strokes.len(), 1);
        assert!(matches!(log.strokes[0], (e, StrokeUndo::Scatter(_)) if e == entity));

        let report = report(&mut world, &json!({})).expect("report");
        assert_eq!(report["instances"], json!(added));
        assert_eq!(report["classes"][0]["name"], json!("trees"));
        assert_eq!(report["classes"][0]["in_set"], json!(true));
        assert_eq!(report["unsaved"], json!(true));
        assert_eq!(report["paintable"], json!(["trees"]));

        // Density is a target: the same stroke adds nothing and logs nothing.
        let again = stroke(&mut world, &paint_args()).expect("stroke");
        assert_eq!(again["added"], json!(0));
        assert_eq!(again["undoable"], json!(false));
        assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// deep-zai-terrain-stream #10: under an editor (`drained`) a scatter stroke drops no older entry.
    #[test]
    fn a_scatter_stroke_under_an_editor_drops_no_logged_stroke() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-paint-drained");
        let (mut world, _) = world(&root);
        stroke(&mut world, &paint_args()).expect("stroke");
        let entry = world.resource::<TerrainStrokeLog>().strokes[0].clone();
        {
            let mut log = world.resource_mut::<TerrainStrokeLog>();
            log.drained = true;
            log.strokes = vec![entry; 300];
        }
        let mut erase = paint_args();
        erase["mode"] = json!("erase");
        let out = stroke(&mut world, &erase).expect("erase");
        assert_eq!(out["undoable"], json!(true), "{out}");
        assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), 301);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_wind_flag_reaches_the_painted_instances() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-wind");
        let (mut world, entity) = world(&root);
        let mut args = paint_args();
        args["wind"] = json!(false);
        stroke(&mut world, &args).expect("calm stroke");
        let paint = world.get::<TerrainScatterPaint>(entity).expect("paint");
        assert!(!paint.0.is_empty());
        assert!(
            paint
                .0
                .instances("trees")
                .all(|i| i.flags == regia_terrain::PAINTED_FLAG_STILL)
        );
        let mut bad = paint_args();
        bad["wind"] = json!("yes");
        let err = stroke(&mut world, &bad).expect_err("not a boolean");
        assert_eq!(reason(&err), "F0002");
        // The default brush follows the wind (flags 0), in a corner the calm stroke did not reach.
        let mut windy = paint_args();
        windy["center"] = json!([-48.0, 48.0]);
        windy["radius"] = json!(10.0);
        stroke(&mut world, &windy).expect("windy stroke");
        let paint = world.get::<TerrainScatterPaint>(entity).expect("paint");
        assert!(paint.0.instances("trees").any(|i| i.flags == 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn filters_scale_and_erase_arrive() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-filters");
        let (mut world, entity) = world(&root);
        let mut args = paint_args();
        args["scale_min"] = json!(3.0);
        args["scale_max"] = json!(4.0);
        args["height_max"] = json!(40.0);
        args["seed"] = json!(5);
        stroke(&mut world, &args).expect("stroke");
        let paint = world.get::<TerrainScatterPaint>(entity).expect("paint");
        assert!(!paint.0.is_empty());
        for i in paint.0.instances("trees") {
            assert!(i.scale >= 3.0 && i.scale <= 8.0, "{}", i.scale);
            assert!(i.position[1] <= 40.0, "{}", i.position[1]);
        }
        let before = paint.0.len() as u64;
        let out = stroke(
            &mut world,
            &json!({ "mode": "erase", "center": [0.0, 0.0], "radius": 500.0, "hardness": 1.0 }),
        )
        .expect("erase");
        assert_eq!(out["removed"], json!(before));
        assert_eq!(out["instances"], json!(0));
        assert_eq!(out["class"], Value::Null);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refusals_carry_their_codes() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-refuse");
        let (mut world, _) = world(&root);
        let mut args = paint_args();
        args["class"] = json!("nope");
        assert_eq!(
            reason(&stroke(&mut world, &args).expect_err("class")),
            "scatter_paint_unknown_class"
        );
        let mut args = paint_args();
        args["density"] = json!(1e9);
        assert_eq!(
            reason(&stroke(&mut world, &args).expect_err("density")),
            "scatter_paint_bad_brush"
        );
        let mut args = paint_args();
        args["slope_min"] = json!(1.2);
        args["slope_max"] = json!(0.2);
        assert_eq!(
            reason(&stroke(&mut world, &args).expect_err("slope")),
            "scatter_paint_bad_brush"
        );
        let mut args = paint_args();
        args["radius"] = json!(3000.0);
        args["density"] = json!(64.0);
        let mut big = World::new();
        big.insert_resource(ScatterPaintSet(Arc::new(set())));
        let n = 65u32;
        let heights = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: vec![0; (n * n) as usize],
        };
        big.spawn(
            Terrain::new(
                &heights,
                8000.0,
                100.0,
                0.0,
                CdlodConfig::default(),
                2,
                TerrainLayers::default(),
            )
            .expect("terrain"),
        );
        assert_eq!(
            reason(&stroke(&mut big, &args).expect_err("size")),
            "scatter_paint_too_large"
        );
        for missing in [
            json!({ "radius": 5.0 }),
            json!({ "class": "trees", "center": [0.0, 0.0] }),
        ] {
            assert!(stroke(&mut world, &missing).is_err());
        }

        let mut bare = World::new();
        bare.spawn(terrain());
        assert_eq!(
            reason(&stroke(&mut bare, &paint_args()).expect_err("set")),
            "scatter_paint_no_set"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_field_of_a_runtime_scatter_is_a_paintable_set() {
        let map = Arc::new(TerrainMap::from_heights(9, 9, vec![0.0; 81], 128.0, 1.0, 0.0).expect("map"));
        let mut library = ScatterLibrary::new();
        library.insert("rock".into(), procedural_rock(1, RockStyle::angular([0.4; 3])));
        let field = ScatterField::new(&set(), ScatterSource::new(map), &library, 1).expect("field");
        let mut world = World::new();
        world.insert_resource(RuntimeScatter::new(field));
        world.spawn(terrain());
        assert!(stroke(&mut world, &paint_args()).expect("stroke")["added"].as_u64() > Some(0));
    }

    #[test]
    fn a_save_loads_back_and_seats_on_the_terrain_as_it_is() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-save");
        let (mut world, entity) = world(&root);
        std::fs::create_dir_all(root.join("maps")).expect("dir");
        assert!(
            save(&mut world, &json!({ "path": "maps/arena.level.ron" })).is_err(),
            "nothing to write"
        );
        stroke(&mut world, &paint_args()).expect("stroke");
        let out = save(&mut world, &json!({ "path": "maps/arena.level.ron" })).expect("save");
        assert_eq!(out["path"], json!("maps/arena.scatterpaint.bin"), "{out}");
        assert!(root.join("maps/arena.scatterpaint.bin").is_file());
        assert!(
            !world
                .get::<TerrainEdits>(entity)
                .expect("edits")
                .unsaved_scatter()
        );
        let saved = world.get::<TerrainScatterPaint>(entity).expect("paint").clone();

        let (mut fresh, other) = self::world(&root);
        let out = load(&mut fresh, &json!({ "path": "maps/arena.level.ron" })).expect("load");
        assert_eq!(out["instances"], json!(saved.0.len()));
        assert_eq!(out["reseated"], json!(0));
        assert_eq!(*fresh.get::<TerrainScatterPaint>(other).expect("paint"), saved);
        assert!(!fresh.get::<TerrainEdits>(other).expect("edits").unsaved_scatter());

        // A terrain that changed since the save: the instances land on it.
        let (mut raised, target) = world_of(&root, terrain_at(3.0));
        let out = load(&mut raised, &json!({ "path": "maps/arena.scatterpaint.bin" })).expect("load");
        assert_eq!(out["reseated"], json!(saved.0.len()));
        assert!(
            raised
                .get::<TerrainEdits>(target)
                .expect("edits")
                .unsaved_scatter()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_files_are_refused_and_nothing_changes() {
        let root = regia_test_utils::unique_temp_dir("agent-scatter-bad");
        let (mut world, entity) = world(&root);
        stroke(&mut world, &paint_args()).expect("stroke");
        let err = load(&mut world, &json!({ "path": "gone.scatterpaint.bin" })).expect_err("missing");
        assert_eq!(reason(&err), "scatter_paint_read");
        std::fs::write(root.join("junk.scatterpaint.bin"), b"junk").expect("write");
        let err = load(&mut world, &json!({ "path": "junk.scatterpaint.bin" })).expect_err("junk");
        assert_eq!(reason(&err), "scatter_paint_decode");
        assert!(
            !world
                .get::<TerrainScatterPaint>(entity)
                .expect("paint")
                .0
                .is_empty()
        );
        assert!(load(&mut world, &json!({ "path": "../out.scatterpaint.bin" })).is_err());
        assert!(save(&mut world, &json!({ "path": "/abs.scatterpaint.bin" })).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
