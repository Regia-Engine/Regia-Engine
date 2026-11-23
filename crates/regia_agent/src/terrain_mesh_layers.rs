// SPDX-License-Identifier: MPL-2.0
//! Terrain mesh-layer methods (spec 121 § 12, spec 26 § 2a.5, ADR-8386, ADR-8398, ADR-8627,
//! `book/agents/terrain-mesh-layers.md`).
//!
//! `terrain.surface_at`, `terrain.bake_report` and `terrain.blockers` read the cooked
//! [`TerrainMeshLayers`] resource the host inserts and answer `no_world_geom` without it. The
//! kernel takes the query height the caller passes; a body adds its own waist reach before the
//! call.
//!
//! `terrain.mesh_layers` is read-only and never answers `no_world_geom`: a world with neither that
//! resource nor a `regia_terrain::TerrainMeshLayers` component holding cooked layers answers
//! `loaded: false` and empty lists, because a map without `<map>.meshlayers` is the ordinary case
//! (ADR-8576 item 6). Its presentation keys (`layers`, `chunks`, `grid`, `mouths`, `generation`,
//! `stream`) come from the level's component and chunk stream; its geometry keys come from the
//! resource when it is present, otherwise from the component's geometry; the resource's rows sit
//! under `layer_reports`, `mouth_reports`, `section_rows`, `portal_rows` and `blocker_rows`.

use regia_app::prelude::*;
use regia_terrain::TerrainLayerStream;
use regia_terrain::TerrainMeshLayers as LevelMeshLayers;
use regia_world_geom::cook::CookReport;
use regia_world_geom::cooked::TriKind;
use regia_world_geom::{
    BlockerId, Generation, GeomState, GroundMode, SurfaceHit, SurfaceKind, TerrainBakeReport, WorldGeom,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

const HINT: &str = "see book/agents/terrain-mesh-layers.md";

/// Most points one `terrain.surface_at` call accepts (spec 121 § 12).
const MAX_POINTS: usize = 64;

/// Floor-triangle grade at or above this is a K1 facet. Grade is `sqrt(nx² + nz²) / |ny|`.
const K1_SLOPE_MIN: f32 = 1.5;

/// Cooked world geometry the terrain mesh-layer methods read (spec 121 § 12).
///
/// Not the level's `regia_terrain::TerrainMeshLayers` component (the cooked chunks a level load
/// spawns); `terrain.mesh_layers` reads both.
#[derive(Resource, Clone, Debug)]
pub struct TerrainMeshLayers {
    /// The loaded map.
    pub geom: WorldGeom,
    /// Blocker bits and the generation. `terrain.blockers` with `set` writes this.
    pub state: GeomState,
    /// The cook report kept beside the file: per-layer weld and trim counts, the ground mode and
    /// mouths. `terrain.bake_report` answers `null` for its counts and ground without it.
    pub report: Option<CookReport>,
    /// Every spec 121 § 10 check the cook ran (`CookOutput::bake_report`). `None` makes
    /// `terrain.bake_report` answer `no_bake_report`.
    pub bake: Option<TerrainBakeReport>,
    /// Map id compared with a call's `map` parameter.
    pub map: String,
    /// When true, `{ set, active }` answers `blocker_sim_owned` and changes nothing.
    pub sim_owned: bool,
}

/// Present while a cook is still loading. The four methods then answer `layers_loading`.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainMeshLayersLoading;

fn err(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, reason, message, HINT)
}

fn refuse_loading(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<TerrainMeshLayersLoading>().is_some() {
        Err(err(
            protocol::INVALID_PARAMS,
            "layers_loading",
            "world geometry is still loading",
        ))
    } else {
        Ok(())
    }
}

fn require_host(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<TerrainMeshLayers>().is_none() {
        Err(err(
            protocol::NOT_FOUND,
            "no_world_geom",
            "no world geometry is loaded",
        ))
    } else {
        Ok(())
    }
}

fn refuse_other_map(host: &TerrainMeshLayers, params: &Value) -> Result<(), RpcError> {
    let Some(asked) = crate::params::opt_str(params, "map")? else {
        return Ok(());
    };
    if asked == host.map {
        Ok(())
    } else {
        Err(RpcError::not_found(
            format!("no world geometry is loaded for map `{asked}`"),
            HINT,
        ))
    }
}

fn section_known(geom: &WorldGeom, id: u16) -> bool {
    geom.sections()
        .get(usize::from(id))
        .is_some_and(|section| section.id == id)
}

fn optional_section(geom: &WorldGeom, params: &Value) -> Result<Option<u16>, RpcError> {
    let Some(id) = crate::params::opt_u64(params, "section")? else {
        return Ok(None);
    };
    match u16::try_from(id) {
        Ok(id) if section_known(geom, id) => Ok(Some(id)),
        _ => Err(err(
            protocol::INVALID_PARAMS,
            "unknown_section",
            format!("section {id} is not in the cooked geometry"),
        )),
    }
}

fn non_finite_token(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "nan" | "inf" | "infinity" | "+inf" | "+infinity" | "-inf" | "-infinity"
    )
}

enum Coord {
    Finite(f64),
    NonFinite,
    Bad,
}

fn coord(value: &Value) -> Coord {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(value) if value.is_finite() => Coord::Finite(value),
            Some(_) => Coord::NonFinite,
            None => Coord::Bad,
        },
        Value::String(text) if non_finite_token(text) => Coord::NonFinite,
        _ => Coord::Bad,
    }
}

fn bad_point() -> RpcError {
    err(
        protocol::INVALID_PARAMS,
        "bad_point",
        "each point must be [x, z, near_y] of finite numbers",
    )
}

fn non_finite() -> RpcError {
    err(
        protocol::INVALID_PARAMS,
        "non_finite",
        "a coordinate is NaN or infinite",
    )
}

fn points_of(params: &Value) -> Result<Vec<[f64; 3]>, RpcError> {
    // Required: a missing or wrong-typed `points` is refused.
    let points = params
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(bad_point)?;
    if points.len() > MAX_POINTS {
        return Err(err(
            protocol::INVALID_PARAMS,
            "too_many_points",
            format!("`points` holds {} items; at most {MAX_POINTS}", points.len()),
        ));
    }
    let mut out = Vec::with_capacity(points.len());
    for point in points {
        let Some(items) = point.as_array() else {
            return Err(bad_point());
        };
        if items.len() != 3 {
            return Err(bad_point());
        }
        let mut xyz = [0.0; 3];
        for (slot, item) in items.iter().enumerate() {
            match coord(item) {
                Coord::Finite(value) => xyz[slot] = value,
                Coord::NonFinite => return Err(non_finite()),
                Coord::Bad => return Err(bad_point()),
            }
        }
        out.push(xyz);
    }
    Ok(out)
}

/// JSON numbers are `f64`. A value that is not a finite `f32` cannot be passed to the kernel or
/// returned: `serde_json` refuses a non-finite number.
fn finite_f32(value: f64) -> Result<f32, RpcError> {
    let narrow = value as f32;
    if narrow.is_finite() {
        Ok(narrow)
    } else {
        Err(non_finite())
    }
}

fn tolerance_m(params: &Value) -> Result<f32, RpcError> {
    match crate::params::opt_f64(params, "tolerance_m")? {
        None => Ok(0.0),
        Some(value) => finite_f32(value),
    }
}

fn outside_map(geom: &WorldGeom, x: f64, z: f64) -> bool {
    let lattice = &geom.cooked().lattice;
    let lo = lattice.vertex_mm(0);
    let hi = lattice.vertex_mm(lattice.cells());
    let x_mm = (x * 1000.0).round_ties_even() as i64;
    let z_mm = (z * 1000.0).round_ties_even() as i64;
    x_mm < lo || x_mm > hi || z_mm < lo || z_mm > hi
}

fn kind_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Ground => "ground",
        SurfaceKind::Floor => "floor",
        SurfaceKind::Roof => "roof",
        SurfaceKind::Wall => "wall",
        SurfaceKind::Blocker => "blocker",
    }
}

fn hit_json(hit: &SurfaceHit) -> Value {
    json!({
        "y": hit.y,
        "normal": hit.normal,
        "kind": kind_name(hit.kind),
        "material": hit.material,
        "section": hit.section.map(|id| id.0),
        "tri": hit.tri.map(|tri| json!({ "layer": tri.layer, "triangle": tri.triangle })),
    })
}

fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn mode_name(mode: GroundMode) -> String {
    match mode {
        GroundMode::Full => "full".to_string(),
        GroundMode::Nested { max_level } => format!("nested:{max_level}"),
    }
}

fn layers_json(report: Option<&CookReport>) -> Value {
    let Some(report) = report else {
        return json!([]);
    };
    Value::Array(
        report
            .layers
            .iter()
            .map(|layer| {
                json!({
                    "id": layer.id,
                    "triangles": layer.triangles,
                    "welded_vertices": layer.welded_vertices,
                    "trimmed_triangles": layer.trimmed_triangles,
                    "degenerate_triangles": layer.degenerate_triangles,
                    "render_triangles": layer.render_triangles,
                    "sections": layer.sections,
                    "plug_triangles": layer.plug_triangles,
                    "skylight_segments_opened": layer.skylight_segments_opened,
                })
            })
            .collect(),
    )
}

fn mouths_json(report: Option<&CookReport>) -> Value {
    let Some(report) = report else {
        return json!([]);
    };
    Value::Array(
        report
            .ground
            .mouths
            .iter()
            .map(|mouth| {
                json!({
                    "id": mouth.id,
                    "layer": mouth.layer,
                    "cut_triangles": mouth.cut_triangles,
                    "collar_triangles": mouth.collar_triangles,
                })
            })
            .collect(),
    )
}

fn sections_json(geom: &WorldGeom) -> Value {
    Value::Array(
        geom.sections()
            .iter()
            .map(|section| {
                let triangles: u64 = section
                    .triangles
                    .iter()
                    .map(|range| u64::from(range.end.saturating_sub(range.start)))
                    .sum();
                json!({
                    "id": section.id,
                    "source": section.source,
                    "covered": section.covered,
                    "blocker": section.blocker,
                    "portals": section.portals,
                    "triangles": triangles,
                })
            })
            .collect(),
    )
}

fn portals_json(geom: &WorldGeom) -> Value {
    Value::Array(
        geom.portals()
            .iter()
            .map(|portal| {
                json!({
                    "id": portal.id,
                    "sides": portal.sides,
                    "mouth": portal.mouth,
                    "nav_mm": portal.nav_mm,
                    "blocker": portal.blocker,
                })
            })
            .collect(),
    )
}

fn blockers_json(geom: &WorldGeom, state: &GeomState, section: Option<u16>) -> Value {
    Value::Array(
        state
            .blockers(geom)
            .filter(|row| section.is_none_or(|id| row.section.0 == id))
            .map(|row| {
                json!({
                    "id": row.id.0,
                    "section": row.section.0,
                    "active": row.active,
                })
            })
            .collect(),
    )
}

fn facet_of(v: [[f32; 3]; 3]) -> Option<(f32, f32, f32, f32)> {
    let ax = v[1][0] - v[0][0];
    let ay = v[1][1] - v[0][1];
    let az = v[1][2] - v[0][2];
    let bx = v[2][0] - v[0][0];
    let by = v[2][1] - v[0][1];
    let bz = v[2][2] - v[0][2];
    let nx = ay * bz - az * by;
    let ny = az * bx - ax * bz;
    let nz = ax * by - ay * bx;
    let horiz2 = nx * nx + nz * nz;
    if !horiz2.is_finite() || !ny.is_finite() {
        return None;
    }
    let vert = ny.abs();
    if vert < 1.0e-6 {
        return None;
    }
    let slope = horiz2.sqrt() / vert;
    if !slope.is_finite() || slope < K1_SLOPE_MIN {
        return None;
    }
    let x = (v[0][0] + v[1][0] + v[2][0]) / 3.0;
    let y = (v[0][1] + v[1][1] + v[2][1]) / 3.0;
    let z = (v[0][2] + v[1][2] + v[2][2]) / 3.0;
    if x.is_finite() && y.is_finite() && z.is_finite() {
        Some((x, y, z, slope))
    } else {
        None
    }
}

fn facets_json(geom: &WorldGeom) -> Value {
    let mut rows = Vec::new();
    for tri in geom.triangles() {
        if tri.kind != TriKind::Floor {
            continue;
        }
        let Some((x, y, z, slope)) = facet_of(tri.v) else {
            continue;
        };
        rows.push(json!({
            "x": x,
            "y": y,
            "z": z,
            "slope": slope,
            "section": tri.section,
        }));
    }
    Value::Array(rows)
}

fn conditioning(report: &CookReport) -> (u64, u64) {
    let welded = report
        .layers
        .iter()
        .map(|layer| u64::from(layer.welded_vertices))
        .sum();
    let trimmed = report
        .layers
        .iter()
        .map(|layer| u64::from(layer.trimmed_triangles))
        .sum();
    (welded, trimmed)
}

/// `terrain.surface_at`: support span and the surfaces around each point.
pub(crate) fn surface_at(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("terrain.surface_at", params, &[])?;
    refuse_loading(world)?;
    require_host(world)?;
    let tolerance = tolerance_m(params)?;
    let points = points_of(params)?;
    let Some(host) = world.get_resource::<TerrainMeshLayers>() else {
        return Err(err(
            protocol::NOT_FOUND,
            "no_world_geom",
            "no world geometry is loaded",
        ));
    };
    refuse_other_map(host, params)?;
    optional_section(&host.geom, params)?;
    let mut results = Vec::with_capacity(points.len());
    for point in points {
        if outside_map(&host.geom, point[0], point[1]) {
            return Err(err(
                protocol::INVALID_PARAMS,
                "out_of_map",
                "a point is outside the cooked ground",
            ));
        }
        let x = finite_f32(point[0])?;
        let z = finite_f32(point[1])?;
        let y = finite_f32(point[2])?;
        let support = host.geom.support(&host.state, x, z, y);
        let surface = host.geom.surface_at(&host.state, x, z, y, tolerance);
        results.push(json!({
            "at": [x, z, y],
            "section": support.floor.section.map(|id| id.0),
            "under": surface.under.as_ref().map(hit_json),
            "over": surface.over.as_ref().map(hit_json),
            "support": {
                "floor": hit_json(&support.floor),
                "ceiling": support.ceiling.as_ref().map(hit_json),
            },
        }));
    }
    let generation = host.state.generation().0;
    Ok(json!({
        "generation": generation,
        "results": results,
    }))
}

fn stream_json(world: &World) -> Value {
    world
        .get_resource::<TerrainLayerStream>()
        .map_or(Value::Null, |s| {
            let st = s.stats();
            json!({
                "resident": st.resident,
                "pending": st.pending,
                "bytes": st.bytes,
                "budget_bytes": st.budget_bytes,
                "coarsest_bytes": st.coarsest_bytes,
                "resident_bytes": st.resident_bytes,
                "coarsest_resident_bytes": st.coarsest_resident_bytes,
                "drawn": st.drawn,
                "wanted": st.wanted,
                "loads": st.loads,
                "evictions": st.evictions,
                "failed": st.failed,
                "visible_radius": s.visible_radius,
                "fade_frames": s.fade_frames,
                "fading": s.fading(),
            })
        })
}

fn presentation_layers_json(layers: &LevelMeshLayers) -> (Vec<Value>, usize, Value) {
    let Some(p) = layers.presentation() else {
        return (Vec::new(), 0, Value::Null);
    };
    let rows = p
        .layers
        .iter()
        .map(|l| {
            json!({
                "id": l.id,
                "chunks": l.chunks.len(),
                "lods": l.chunks.iter().map(|c| c.lods.len()).max().unwrap_or(0),
                "triangles": l
                    .chunks
                    .iter()
                    .map(|c| c.lods.first().map_or(0, |lod| u64::from(lod.triangles)))
                    .sum::<u64>(),
                "map_layer": l.map_layer,
                "cast_shadows": l.cast_shadows,
                "collider": l.collider,
                "material": l.material,
                "material_id": l.material_id.map(|id| id.to_string()),
            })
        })
        .collect();
    let grid = json!({
        "chunk_level": p.grid.chunk_level,
        "chunk_m": p.grid.node_size(p.grid.chunk_level),
        "root_level": p.grid.root_level,
        "span_m": p.grid.span_m,
        "range_factor": p.grid.range_factor,
    });
    (rows, p.chunk_count(), grid)
}

/// `terrain.mesh_layers`: the one mesh-layer overview (ADR-8576 item 6, ADR-8627).
///
/// Without geometry (no [`TerrainMeshLayers`] resource and no level component holding cooked
/// layers) it answers `loaded: false`, never `no_world_geom`. The level component gives layers,
/// chunks, grid, mouths, generation and the chunk stream's counters; the resource gives `map`,
/// `geom_generation` and the `*_rows` / `*_reports` arrays; the counts, `resident_bytes` and
/// `content_hash` come from the resource's geometry when it is present, otherwise from the
/// component's.
pub(crate) fn mesh_layers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("terrain.mesh_layers", params, &[])?;
    refuse_loading(world)?;
    let _ = world.register_component::<LevelMeshLayers>();
    let mut query = world.query::<&LevelMeshLayers>();
    let world: &World = world;
    let level = query
        .iter(world)
        .find(|l| l.geom().is_some() || l.presentation().is_some());
    let host = world.get_resource::<TerrainMeshLayers>();
    let stream = stream_json(world);
    if let Some(host) = host {
        refuse_other_map(host, params)?;
    }
    let geom = host
        .map(|h| &h.geom)
        .or_else(|| level.and_then(LevelMeshLayers::geom));
    let section = match geom {
        Some(geom) => optional_section(geom, params)?,
        // Nothing to check a section id against: the empty answer lists no rows anyway.
        None => None,
    };
    let loaded = host.is_some() || level.is_some();
    let (layers, chunks, grid) = level.map_or((Vec::new(), 0, Value::Null), presentation_layers_json);
    let (sections, portals, blockers, resident_bytes, content_hash) = match geom {
        Some(g) => (
            g.cooked().sections.len(),
            g.cooked().portals.len(),
            g.cooked().plugs.len(),
            g.resident_bytes(),
            Value::String(hex32(&g.content_hash())),
        ),
        None => (0, 0, 0, 0, Value::Null),
    };
    let (layer_reports, mouth_reports, section_rows, portal_rows, blocker_rows) = match host {
        Some(h) => (
            layers_json(h.report.as_ref()),
            mouths_json(h.report.as_ref()),
            sections_json(&h.geom),
            portals_json(&h.geom),
            blockers_json(&h.geom, &h.state, section),
        ),
        None => (json!([]), json!([]), json!([]), json!([]), json!([])),
    };
    Ok(json!({
        "loaded": loaded,
        "map": host.map(|h| h.map.clone()),
        "layers": layers,
        "chunks": chunks,
        "grid": grid,
        "mouths": level.map_or_else(Vec::new, LevelMeshLayers::filled_holes),
        "sections": sections,
        "portals": portals,
        "blockers": blockers,
        "resident_bytes": resident_bytes,
        "content_hash": content_hash,
        "generation": level.map_or(0, LevelMeshLayers::generation),
        "geom_generation": host.map(|h| h.state.generation().0),
        "stream": stream,
        "layer_reports": layer_reports,
        "mouth_reports": mouth_reports,
        "section_rows": section_rows,
        "portal_rows": portal_rows,
        "blocker_rows": blocker_rows,
    }))
}

/// `terrain.bake_report`: the cook's `TerrainBakeReport` (every spec 121 § 10 check, passed or
/// failed), the weld and trim counts, the ground memory and the K1 facets.
pub(crate) fn bake_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("terrain.bake_report", params, &[])?;
    refuse_loading(world)?;
    require_host(world)?;
    let Some(host) = world.get_resource::<TerrainMeshLayers>() else {
        return Err(err(
            protocol::NOT_FOUND,
            "no_world_geom",
            "no world geometry is loaded",
        ));
    };
    refuse_other_map(host, params)?;
    let Some(bake) = host.bake.as_ref() else {
        return Err(err(
            protocol::NOT_FOUND,
            "no_bake_report",
            "the loaded world geometry has no bake report",
        ));
    };
    // The same rows `regia terrain check --json` prints (`CheckFinding`'s serde shape).
    let checks = serde_json::to_value(&bake.checks).map_err(|e| {
        RpcError::internal(format!(
            "terrain.bake_report: the check rows did not serialize: {e}"
        ))
    })?;
    let (welded, trimmed, ground) = match host.report.as_ref() {
        Some(report) => {
            let (welded, trimmed) = conditioning(report);
            (
                json!(welded),
                json!(trimmed),
                json!({
                    "mode": mode_name(report.ground.mode),
                    "sim_bytes": report.ground.sim_bytes,
                    "budget_bytes": report.ground.budget_bytes,
                    "tiles_per_side": report.ground.tiles_per_side,
                }),
            )
        }
        None => (Value::Null, Value::Null, Value::Null),
    };
    Ok(json!({
        "map": host.map,
        "generation": host.state.generation().0,
        "content_hash": hex32(&host.geom.content_hash()),
        "resident_bytes": host.geom.resident_bytes(),
        "passed": bake.passed,
        "waived": bake.waived,
        "failure": (!bake.passed).then(|| bake.failure_code()),
        "ground_mode": bake.ground_mode,
        "memory_bytes": bake.memory_bytes,
        "report_content_hash": bake.content_hash,
        "welded_vertices": welded,
        "trimmed_triangles": trimmed,
        "ground": ground,
        "checks": checks,
        "facets": facets_json(&host.geom),
    }))
}

fn both_required() -> RpcError {
    RpcError::invalid_params("`set` and `active` are required together")
}

fn blockers_value(
    host: &TerrainMeshLayers,
    section: Option<u16>,
    set: Option<u16>,
    active: Option<bool>,
    applied: bool,
) -> Value {
    let mut value = json!({
        "generation": host.state.generation().0,
        "blockers": blockers_json(&host.geom, &host.state, section),
        "applied": applied,
    });
    if let (Some(set), Some(active), Some(object)) = (set, active, value.as_object_mut()) {
        object.insert("set".to_string(), json!(set));
        object.insert("active".to_string(), json!(active));
    }
    value
}

/// `terrain.blockers`: plugs with their active flags, and a dev/editor flip.
pub(crate) fn blockers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("terrain.blockers", params, &[])?;
    refuse_loading(world)?;
    require_host(world)?;
    let (section, sim_owned) = {
        let Some(host) = world.get_resource::<TerrainMeshLayers>() else {
            return Err(err(
                protocol::NOT_FOUND,
                "no_world_geom",
                "no world geometry is loaded",
            ));
        };
        refuse_other_map(host, params)?;
        (optional_section(&host.geom, params)?, host.sim_owned)
    };
    let set = crate::params::opt_u64(params, "set")?;
    let active = crate::params::opt_bool(params, "active")?;
    if set.is_some() != active.is_some() {
        return Err(both_required());
    }
    let Some(set) = set else {
        let Some(host) = world.get_resource::<TerrainMeshLayers>() else {
            return Err(err(
                protocol::NOT_FOUND,
                "no_world_geom",
                "no world geometry is loaded",
            ));
        };
        return Ok(blockers_value(host, section, None, None, false));
    };
    let Some(active) = active else {
        return Err(both_required());
    };
    if sim_owned {
        return Err(err(
            protocol::INVALID_PARAMS,
            "blocker_sim_owned",
            "the sim owns blocker flips during a match",
        ));
    }
    let id = u16::try_from(set).map_err(|_| {
        err(
            protocol::NOT_FOUND,
            "blocker_unknown",
            format!("no blocker plug {set}"),
        )
    })?;
    let Some(mut guard) = world.get_resource_mut::<TerrainMeshLayers>() else {
        return Err(err(
            protocol::NOT_FOUND,
            "no_world_geom",
            "no world geometry is loaded",
        ));
    };
    let host = &mut *guard;
    let before = host.state.generation().0;
    let generation: Generation = host
        .state
        .set_blocker(&host.geom, BlockerId(id), active)
        .map_err(|error| err(protocol::NOT_FOUND, error.code(), error.to_string()))?;
    let applied = generation.0 != before;
    Ok(blockers_value(host, section, Some(id), Some(active), applied))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetId;
    use regia_terrain::{
        CdlodConfig, CookedChunkLod, CookedLayer, CookedLayerChunk, LayerChunkGrid, TerrainMeshLayersCooked,
    };

    fn presentation() -> TerrainMeshLayersCooked {
        let c = CdlodConfig::default();
        TerrainMeshLayersCooked {
            version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
            chunk_importer: 0,
            grid: LayerChunkGrid::new(1024.0, 6, c.grid_size, c.range_factor, [0.0, 0.0], 64.0),
            layers: vec![CookedLayer {
                id: "tubes".into(),
                map_layer: 1,
                cast_shadows: false,
                collider: false,
                material: String::new(),
                material_id: None,
                chunks: (0..3)
                    .map(|x| CookedLayerChunk {
                        x,
                        z: 0,
                        bounds: [[0.0; 3], [1.0; 3]],
                        lods: (0..2u8)
                            .map(|lod| CookedChunkLod {
                                mesh: AssetId::sub_asset(AssetId::default(), &format!("{x}/{lod}")),
                                triangles: 10 >> lod,
                                bytes: 100 >> lod,
                                error_m: 0.0,
                            })
                            .collect(),
                    })
                    .collect(),
            }],
        }
    }

    fn done(reply: crate::LocalReply) -> Value {
        match reply {
            crate::LocalReply::Done(Ok(v)) => v,
            other => panic!("terrain.mesh_layers did not answer: {other:?}"),
        }
    }

    /// Spec 121 § 12 through the dispatcher: no layers answers `loaded: false`; loaded layers
    /// report their chunks and the stream's per-LOD residency, pending, bytes and budget.
    #[test]
    fn mesh_layers_reports_layers_and_stream_through_the_dispatcher() {
        let mut world = World::new();
        let empty = done(crate::call_local(
            &mut world,
            "terrain.mesh_layers",
            json!({}),
            None,
        ));
        assert_eq!(empty["loaded"], json!(false));
        assert_eq!(empty["stream"], Value::Null);

        let p = presentation();
        let mut stream = TerrainLayerStream::new(f32::INFINITY, 1000);
        let mut fetch = |_: AssetId| {
            Ok(std::sync::Arc::new(regia_render::CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            }))
        };
        stream.step(&p, &[[-500.0, -500.0]], &mut fetch);
        world.insert_resource(stream);
        world.spawn(LevelMeshLayers::new().with_presentation(p, None));
        let r = done(crate::call_local(
            &mut world,
            "terrain.mesh_layers",
            json!({}),
            None,
        ));
        assert_eq!(r["loaded"], json!(true));
        assert_eq!(r["chunks"], json!(3));
        assert_eq!(r["layers"][0]["id"], json!("tubes"));
        assert_eq!(r["layers"][0]["lods"], json!(2));
        assert_eq!(r["layers"][0]["cast_shadows"], json!(false));
        assert_eq!(r["grid"]["chunk_level"], json!(2));
        assert_eq!(r["stream"]["budget_bytes"], json!(1000));
        // ADR-8842: the cross-fade length and the chunks fading (none: no frame drove the stream).
        assert_eq!(
            r["stream"]["fade_frames"],
            json!(regia_terrain::DEFAULT_LAYER_FADE_FRAMES)
        );
        assert_eq!(r["stream"]["fading"], json!(0));
        // Every chunk keeps its coarsest LOD (LOD 1); the nearest ones also hold LOD 0.
        assert_eq!(r["stream"]["resident"][1], json!(3));
        assert!(r["stream"]["resident"][0].as_u64().is_some_and(|n| n >= 1));
        assert_eq!(r["stream"]["pending"], json!(0));
        assert!(r["stream"]["bytes"].as_u64().is_some_and(|b| b >= 150));

        let err = match crate::call_local(&mut world, "terrain.mesh_layers", json!({ "x": 1 }), None) {
            crate::LocalReply::Done(Err(e)) => e,
            other => panic!("expected an error: {other:?}"),
        };
        // The unknown key is refused by name (ADR-8627: `map` and `section` are the parameters now).
        assert!(err.message.contains("no parameter `x`"), "{}", err.message);
        assert_eq!(err.data.as_ref().map(|d| d["param"].clone()), Some(json!("x")));
    }
}
