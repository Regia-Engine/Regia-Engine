// SPDX-License-Identifier: MPL-2.0
//! `regia bake-lightmaps --scene <level> --cells`: bakes each cell of a streamed level as its own
//! lightmap (ADR-3318, `book/agents/lightmaps.md` "Per-cell bakes").
//!
//! The level is read once ([`super::level::load`]) and its placed mesh parts are split into the
//! streaming grid's cells by the cook's partition rules (`docs/specs/78-world-streaming.md` § 5.3):
//! `StreamPlacement::AlwaysLoaded` and parts wider than `max_entity_extent_cells` cells go to the
//! always-loaded set, `StreamPlacement::Cell` forces a cell, every other entity goes to the cell
//! holding the XZ centre of its parts' world bounds.
//!
//! **One file per cell.** Cell `(x, z)` is baked from a scene holding its own parts (the
//! receivers), the parts of every cell within `--cell-ring` cells of it and the always-loaded set:
//! those cast shadows and bounce light into the cell and get no texels, so light does not leak
//! through a cell edge whose wall belongs to the neighbor. Its file is
//! `<out stem>.cells/<x>_<z>.flmap` (default `<project>/assets/maps/<level>.cells/`), written at
//! one exposure for every cell (`regia_pt::lightmap::cells::CELL_LIGHTMAP_EXPOSURE`). The
//! always-loaded set's receivers are baked into the primary file (`<out>`, the level's own
//! lightmap path) against the whole level; with none, the primary is an empty 4 × 4 atlas the level
//! can still name.
//!
//! **Seams.** After the bakes, every pair of edge-adjacent cells where one side was baked in this
//! run gets the seam pass (`stitch_cell_border`): texels on both sides of the border agree. A
//! neighbor that was not re-baked is read from disk and only the texels its border lookups read
//! change.
//!
//! **Incremental.** `cells.json` beside the files records each cell's input hash: its own parts'
//! meshes, transforms and materials, the sun, sky and lights, and the bake settings. A cell whose
//! hash and file are unchanged is `skipped`. A neighbor's edit does not re-bake a cell (its light
//! near the border can lag behind; `--cells-force` re-bakes every cell).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Args;
use regia_agent::bake_export::slash;
use regia_asset::lightmap::{Lightmap, LightmapBakeInfo, LightmapContents, LightmapLayout};
use regia_core::id::Guid;
use regia_pt::lightmap::cells::{
    BorderAxis, CELL_BAKE_INDEX_FILE, CELL_BAKE_INDEX_VERSION, CELL_LIGHTMAP_EXPOSURE, CellBakeIndex,
    CellBakeRecord, CellBorder, CellSeamReport, instance_world_bounds, scene_subset, set_exposure,
    stitch_cell_border,
};
use regia_pt::lightmap::{bake_lightmap, bake_lightmap_checkpointed};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{LevelStreaming, StreamPlacement};
use serde_json::{Value, json};

use super::{
    BakeLightmapsArgs, InstancePart, LightmapCommandError, TexelEncodingArg, bake_settings,
    check_bake_settings, check_gpu_args, lay_out, measure_seams, source_hashes,
};
use crate::commands::bake_checkpoint;

/// The neighbor ring's default width, cells. One ring holds every occluder within one cell side of
/// the border: at the default 128 m cells that is any wall, building or cliff a sun above about
/// 10° can shadow across the edge from, and the bounce a surface receives from more than a cell
/// away is below the bake's noise. Use 2 for low suns over tall geometry or for small cells.
pub const DEFAULT_CELL_RING: u32 = 1;

/// The widest neighbor ring, cells (a ring of 4 bakes each cell against 81 cells).
pub const MAX_CELL_RING: u32 = 4;

/// The code of a `--cells` bake of a level that is not streamed.
pub const CELLS_NOT_STREAMED: &str = "lightmap_cells_not_streamed";

/// The options of `--cells` (ADR-3318).
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CellArgs {
    /// Bake each cell of the streamed level (`--scene`, with `LevelStreaming` on) as its own
    /// lightmap, `<out stem>.cells/<x>_<z>.flmap`, with its neighbors as occluders, a seam pass
    /// between cells and `cells.json` to skip unchanged cells (ADR-3318).
    #[arg(long = "cells", requires = "level")]
    pub cells: bool,
    /// `--cells`: the neighbor ring, cells (0..=4): cells this close shadow and bounce light into
    /// a cell's bake without receiving texels.
    #[arg(long = "cell-ring", default_value_t = DEFAULT_CELL_RING)]
    pub ring: u32,
    /// `--cells`: bake every cell, also those whose inputs did not change.
    #[arg(long = "cells-force")]
    pub force: bool,
}

impl Default for CellArgs {
    fn default() -> Self {
        Self {
            cells: false,
            ring: DEFAULT_CELL_RING,
            force: false,
        }
    }
}

/// The directory of the cell files of a level whose lightmap is `primary`: `<stem>.cells` beside
/// it (`maps/world.flmap` → `maps/world.cells/`).
#[must_use]
pub fn cell_dir(primary: &Path) -> PathBuf {
    let stem = primary
        .file_name()
        .and_then(|n| n.to_str())
        .map_or("lightmap", |n| n.strip_suffix(".flmap").unwrap_or(n));
    primary.with_file_name(format!("{stem}.cells"))
}

/// What the partition put where: instances by cell, and the always-loaded ones.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellPartition {
    /// The instances of each cell, ascending.
    pub cells: BTreeMap<CellCoord, Vec<usize>>,
    /// Instances of the always-loaded set, ascending.
    pub always: Vec<usize>,
    /// Entities of the always-loaded set.
    pub always_entities: usize,
}

/// Splits the level's placed parts into cells (module docs). `placements` holds each entity's
/// `StreamPlacement` (absent: `Auto`); `roots` maps an entity to the root of its hierarchy (absent:
/// its own root). A hierarchy moves as one, as the cook moves it (spec 78 § 5.3 rule 2): the root's
/// placement decides, over the bounds of every part of the hierarchy.
///
/// # Errors
///
/// `lightmap_cells_not_streamed` when a part's position is outside the grid.
pub fn partition(
    scene: &regia_pt::PtScene,
    parts: &[InstancePart],
    settings: &LevelStreaming,
    placements: &HashMap<Guid, StreamPlacement>,
    roots: &HashMap<Guid, Guid>,
) -> Result<CellPartition, LightmapCommandError> {
    let grid = settings
        .grid()
        .map_err(|e| LightmapCommandError::new(CELLS_NOT_STREAMED, None, e.to_string()))?;
    let mut by_entity: BTreeMap<Guid, Vec<usize>> = BTreeMap::new();
    for part in parts {
        let root = roots.get(&part.key.entity).copied().unwrap_or(part.key.entity);
        by_entity.entry(root).or_default().push(part.instance);
    }
    let limit = settings.max_entity_extent_cells * settings.cell_size_m;
    let mut out = CellPartition::default();
    for (guid, instances) in &by_entity {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for &k in instances {
            if let Some((a, b)) = instance_world_bounds(scene, k) {
                for i in 0..3 {
                    lo[i] = lo[i].min(a[i]);
                    hi[i] = hi[i].max(b[i]);
                }
            }
        }
        let placement = placements.get(guid).copied().unwrap_or_default();
        let wide = lo[0].is_finite() && f64::from((hi[0] - lo[0]).max(hi[2] - lo[2])) > limit;
        let cell = match placement {
            StreamPlacement::AlwaysLoaded => None,
            _ if wide || !lo[0].is_finite() => None,
            StreamPlacement::Cell { x, z } => Some(CellCoord::new(x, z)),
            StreamPlacement::Auto => {
                let (cx, cz) = (0.5 * (lo[0] + hi[0]), 0.5 * (lo[2] + hi[2]));
                Some(grid.cell_of(f64::from(cx), f64::from(cz)).map_err(|e| {
                    LightmapCommandError::new(CELLS_NOT_STREAMED, Some(guid.to_string()), e.to_string())
                })?)
            }
        };
        match cell {
            Some(c) => out.cells.entry(c).or_default().extend(instances),
            None => {
                out.always.extend(instances);
                out.always_entities += 1;
            }
        }
    }
    for v in out.cells.values_mut() {
        v.sort_unstable();
    }
    out.always.sort_unstable();
    Ok(out)
}

/// The cells within `ring` cells of `c` (Chebyshev distance), `c` left out.
fn ring_of(c: CellCoord, ring: u32) -> impl Iterator<Item = CellCoord> {
    let r = i64::from(ring);
    (-r..=r).flat_map(move |dx| {
        (-r..=r)
            .filter(move |&dz| dx != 0 || dz != 0)
            .map(move |dz| CellCoord::new(c.x + dx, c.z + dz))
    })
}

/// The root of every entity of `entities` that has a parent (a missing parent or a cycle ends the
/// walk at the last entity found).
fn hierarchy_roots(entities: &[regia_scene::format::SceneEntity]) -> HashMap<Guid, Guid> {
    let parent: HashMap<Guid, Guid> = entities
        .iter()
        .filter_map(|e| e.parent.map(|p| (e.guid, p)))
        .collect();
    let known: std::collections::HashSet<Guid> = entities.iter().map(|e| e.guid).collect();
    parent
        .keys()
        .map(|&g| {
            let mut root = g;
            for _ in 0..entities.len() {
                match parent.get(&root) {
                    Some(p) if known.contains(p) && *p != g => root = *p,
                    _ => break,
                }
            }
            (g, root)
        })
        .collect()
}

/// What `read_streaming` reads of a streamed level: its settings, every entity's
/// `StreamPlacement` and every child's hierarchy root.
type StreamInfo = (
    LevelStreaming,
    HashMap<Guid, StreamPlacement>,
    HashMap<Guid, Guid>,
);

/// The level's streaming settings, every entity's `StreamPlacement` and its hierarchy roots.
fn read_streaming(path: &Path) -> Result<StreamInfo, LightmapCommandError> {
    use regia_ecs::reflect::TypeRegistry;
    use regia_ecs::serialize::ReflectCodec;
    let level = super::super::bake_gi::from_scene::read_level(path)?;
    let label = Some(slash(path));
    let mut registry = TypeRegistry::default();
    regia_asset_pipeline::stream_cook::register_cook_types(&mut registry);
    let settings = regia_asset_pipeline::stream_cook::level_streaming(&level.level.scene, &registry)
        .map_err(|e| LightmapCommandError::new(CELLS_NOT_STREAMED, label.clone(), e.to_string()))?
        .ok_or_else(|| {
            LightmapCommandError::new(
                CELLS_NOT_STREAMED,
                label.clone(),
                "--cells needs a streamed level (a `LevelStreaming` resource with `enabled: true`)",
            )
        })?;
    let mut placements = HashMap::new();
    let mut codec = ReflectCodec::new(&registry);
    let placement_path = <StreamPlacement as regia_ecs::reflect::TypePath>::type_path();
    let roots = hierarchy_roots(&level.level.scene.entities);
    let Some(registration) = registry.get_with_type_path(placement_path) else {
        return Ok((settings, placements, roots));
    };
    for entity in &level.level.scene.entities {
        let Some(raw) = entity.components.get(placement_path) else {
            continue;
        };
        let decoded = codec
            .from_ron_typed(raw.get_ron(), registration)
            .ok()
            .and_then(|v| v.downcast::<StreamPlacement>().ok());
        match decoded {
            Some(p) => {
                placements.insert(entity.guid, *p);
            }
            None => {
                return Err(LightmapCommandError::new(
                    CELLS_NOT_STREAMED,
                    Some(entity.guid.to_string()),
                    "the entity's StreamPlacement does not read",
                ));
            }
        }
    }
    Ok((settings, placements, roots))
}

/// The hash of what a cell's bake reads of its own (module docs): BLAKE3 hex.
fn input_hash(
    scene: &regia_pt::PtScene,
    parts: &[&InstancePart],
    args: &BakeLightmapsArgs,
    salt: &str,
) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"regia-lightmap-cell/1\0");
    h.update(salt.as_bytes());
    let bake = regia_pt::lightmap::LightmapBakeSettings {
        threads: 0,
        ..bake_settings(args)
    };
    h.update(
        format!(
            "{bake:?}|{}|{}|{}|{}|{}|{}|{:?}|{}|{}|{}",
            args.unwrap.name(),
            args.density,
            args.padding,
            args.max_atlas,
            args.chart_angle,
            args.min_area,
            args.crease,
            args.stitch == super::Switch::On,
            args.auto_density == super::Switch::On,
            args.meshlets,
        )
        .as_bytes(),
    );
    h.update(
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            scene.sun, scene.sky, scene.lights, scene.media
        )
        .as_bytes(),
    );
    for part in parts {
        h.update(format!("{:?}|{}|{}", part.key, part.density_scale, part.receive).as_bytes());
        let Some(inst) = scene.instances.get(part.instance) else {
            continue;
        };
        h.update(format!("{:?}", inst.transform).as_bytes());
        if let Some(m) = scene.materials.get(inst.material) {
            h.update(format!("{m:?}").as_bytes());
        }
        if let Some(mesh) = scene.meshes.get(inst.mesh) {
            for p in &mesh.positions {
                for v in p {
                    h.update(&v.to_le_bytes());
                }
            }
            for i in &mesh.indices {
                h.update(&i.to_le_bytes());
            }
        }
    }
    h.finalize().to_hex().to_string()
}

/// World positions of instance `k`'s vertices (the order a lightmap entry's unwrap indexes).
fn world_positions(scene: &regia_pt::PtScene, k: usize) -> Vec<[f32; 3]> {
    let Some(inst) = scene.instances.get(k) else {
        return Vec::new();
    };
    scene.meshes.get(inst.mesh).map_or_else(Vec::new, |m| {
        m.positions
            .iter()
            .map(|p| {
                inst.transform
                    .transform_point3(regia_core::glam::Vec3::from_array(*p))
                    .to_array()
            })
            .collect()
    })
}

/// A cell map held for the seam pass.
struct Held {
    map: Lightmap,
    /// World positions of each entry's source vertices.
    positions: Vec<Vec<[f32; 3]>>,
    /// Rewrite the file after the seams.
    dirty: bool,
}

impl Held {
    fn new(map: Lightmap, scene: &regia_pt::PtScene, by_key: &HashMap<(Guid, u32), usize>) -> Self {
        let positions = map
            .entries
            .iter()
            .map(|e| {
                e.key
                    .and_then(|k| by_key.get(&(k.entity, k.part)))
                    .map_or_else(Vec::new, |&i| world_positions(scene, i))
            })
            .collect();
        Self {
            map,
            positions,
            dirty: false,
        }
    }
}

/// What one cell's bake produced (the `cells` table of the JSON).
struct CellRow {
    record: CellBakeRecord,
    texels: u64,
    seconds: f64,
    skipped: bool,
    /// The GPU baker's stats (adapter, tiles, bytes) when `--gpu` baked the cell.
    gpu: Option<Value>,
}

/// The neighbor pairs the seam pass visits: every edge-adjacent pair with a baked side, sorted.
fn seam_pairs(
    baked: &BTreeSet<CellCoord>,
    present: &BTreeSet<CellCoord>,
) -> Vec<(CellCoord, CellCoord, BorderAxis)> {
    let mut pairs = BTreeSet::new();
    for &c in baked {
        for (n, axis) in [
            (CellCoord::new(c.x + 1, c.z), BorderAxis::X),
            (CellCoord::new(c.x - 1, c.z), BorderAxis::X),
            (CellCoord::new(c.x, c.z + 1), BorderAxis::Z),
            (CellCoord::new(c.x, c.z - 1), BorderAxis::Z),
        ] {
            if present.contains(&n) {
                let (a, b) = if c < n { (c, n) } else { (n, c) };
                pairs.insert((a, b, axis == BorderAxis::X));
            }
        }
    }
    pairs
        .into_iter()
        .map(|(a, b, x)| (a, b, if x { BorderAxis::X } else { BorderAxis::Z }))
        .collect()
}

/// An empty primary lightmap (4 × 4, no entry) for a level whose always-loaded set has no
/// receiver: the level can still name it, and the cells stack below it at run time.
fn empty_primary(contents: u32) -> Result<Lightmap, LightmapCommandError> {
    let mut map = Lightmap::from_irradiance(
        LightmapLayout {
            width: 4,
            height: 4,
            density: 1.0,
            padding: 0,
            entries: Vec::new(),
            unwraps: Vec::new(),
        },
        &[[0.0; 3]; 16],
        vec![regia_asset::lightmap::Coverage::EMPTY; 16],
        LightmapBakeInfo {
            contents,
            ..LightmapBakeInfo::default()
        },
    )
    .map_err(|e| LightmapCommandError::new(e.code(), None, e.to_string()))?;
    set_exposure(&mut map, CELL_LIGHTMAP_EXPOSURE);
    Ok(map)
}

/// Bakes `laid` on the CPU or the GPU (as the single-level bake does), checkpointed beside `out`
/// with `--checkpoint`.
fn bake_one(
    args: &BakeLightmapsArgs,
    laid: &super::LaidOut,
    out: &Path,
    label: &str,
) -> Result<(Lightmap, regia_pt::lightmap::LightmapBakeReport, Option<Value>), LightmapCommandError> {
    let bake = bake_settings(args);
    let plan = args
        .checkpoint
        .plan(out)
        .map_err(|(code, path, message)| LightmapCommandError::new(code, Some(path), message))?;
    if let Some(p) = &plan {
        let _ = bake_checkpoint::announce(p, "lightmap");
    }
    let progress = |done: usize, total: usize| {
        if done == total {
            eprintln!("  lightmap cell {label}: {done}/{total} texels");
        }
    };
    if args.gpu {
        match super::bake_on_gpu(laid, &bake, &args.gpu_opts, &progress, plan.as_ref()) {
            Ok((map, report, gpu, _)) => return Ok((map, report, gpu)),
            Err(e) if args.gpu_opts.gpu_fallback && super::GPU_FALLBACK_CODES.contains(&e.code) => {
                eprintln!(
                    "  lightmap cell {label}: the GPU bake failed ({}); baking on the CPU",
                    e.code
                );
            }
            Err(e) => return Err(e),
        }
    }
    let cpu_plan = plan
        .as_ref()
        .map(|p| bake_checkpoint::plan_for_kind(p, regia_pt::checkpoint::BakeCheckpointKind::LightmapCpu));
    let baked = match &cpu_plan {
        Some(p) => {
            bake_lightmap_checkpointed(&laid.scene, laid.layout.clone(), &laid.receivers, &bake, p, |e| {
                progress(e.done as usize, e.total as usize);
            })
            .map(|(m, r, _)| (m, r))
        }
        None => bake_lightmap(&laid.scene, laid.layout.clone(), &laid.receivers, &bake, progress),
    };
    let (map, report) = baked.map_err(|e| {
        bake_checkpoint::bake_error(&e, cpu_plan.as_ref(), LightmapCommandError::new, |e| {
            LightmapCommandError::new("lightmap_bake", Some(label.to_string()), e.to_string())
        })
    })?;
    Ok((map, report, None))
}

/// Lays out and bakes the receivers `own` (instances of `scene`) against the instances `keep`
/// (which holds `own`), and returns the finished map (seams measured, source hashes, clusters,
/// the cell exposure), its texel count and the GPU baker's stats (`--gpu`).
fn bake_receivers(
    args: &BakeLightmapsArgs,
    scene: &regia_pt::PtScene,
    parts: &[InstancePart],
    own: &[usize],
    keep: &[usize],
    out: &Path,
    label: &str,
) -> Result<(Lightmap, u64, Option<Value>), LightmapCommandError> {
    let sub = scene_subset(scene, keep);
    let index_in: HashMap<usize, usize> = keep.iter().enumerate().map(|(i, &k)| (k, i)).collect();
    let own_set: BTreeSet<usize> = own.iter().copied().collect();
    let sub_parts: Vec<InstancePart> = parts
        .iter()
        .filter(|p| own_set.contains(&p.instance))
        .filter_map(|p| {
            index_in.get(&p.instance).map(|&i| InstancePart {
                instance: i,
                ..p.clone()
            })
        })
        .collect();
    let laid = lay_out(&sub, args, Some(&sub_parts))?;
    let clusters = args
        .meshlets
        .then(|| super::meshlets::build(&sub, &laid.receivers, &laid.layout, args.threads));
    let (mut map, report, gpu) = bake_one(args, &laid, out, label)?;
    let _ = measure_seams(&mut map, &laid.scene, &laid.receivers);
    if let Some(c) = clusters {
        map.meshlets = c.blobs;
    }
    map.source_hashes = source_hashes(&sub, &laid.receivers);
    set_exposure(&mut map, CELL_LIGHTMAP_EXPOSURE);
    Ok((map, report.texels_baked as u64, gpu))
}

/// Runs `--cells` (module docs) and returns its JSON.
///
/// # Errors
///
/// `lightmap_cells_not_streamed`, `lightmap_bake_settings` (an option `--cells` does not take), the
/// level's and the bake's codes, `lightmap_cells_index` for an index that does not read, and
/// `lightmap_io`.
pub fn execute(args: &BakeLightmapsArgs) -> Result<Value, LightmapCommandError> {
    let started = Instant::now();
    let Some(level_path) = args.level.as_deref() else {
        return Err(LightmapCommandError::new(
            CELLS_NOT_STREAMED,
            None,
            "--cells needs --scene <level>",
        ));
    };
    let refuse = |field: &str, message: &str| {
        Err(LightmapCommandError::new(
            "lightmap_bake_settings",
            Some(field.into()),
            message.to_string(),
        ))
    };
    if args.encoding != TexelEncodingArg::Rgb9e5 {
        return refuse(
            "encoding",
            "--cells writes RGB9E5 cells (the seam pass rewrites neighbors' texels); the cook encodes per platform",
        );
    }
    if args.cells.ring > MAX_CELL_RING {
        return refuse("cell_ring", "--cell-ring is above 4");
    }
    if !args.instances.is_empty() || args.compare.is_some() || args.checkpoint.resume.is_some() {
        return refuse(
            "cells",
            "--cells does not take --instance, --compare or --resume (use --checkpoint: one checkpoint per cell)",
        );
    }
    let bake = bake_settings(args);
    check_bake_settings(&bake)?;
    check_gpu_args(&args.gpu_opts)?;
    let mut timings = serde_json::Map::new();
    let mut mark = Instant::now();
    let mut lap = |name: &str, timings: &mut serde_json::Map<String, Value>| {
        timings.insert(name.into(), json!(mark.elapsed().as_secs_f64()));
        mark = Instant::now();
    };
    let lb = super::level::load(level_path, args)?;
    let (streaming, placements, roots) = read_streaming(level_path)?;
    let part = partition(&lb.scene, &lb.parts, &streaming, &placements, &roots)?;
    lap("load", &mut timings);
    let primary_out = args.out.clone().unwrap_or_else(|| lb.default_out.clone());
    let dir = cell_dir(&primary_out);
    let label = slash(level_path);
    let cells_json = |rows: &[CellRow]| -> Vec<Value> {
        rows.iter()
            .map(|r| {
                let mut row = json!({
                    "key": r.record.key, "x": r.record.x, "z": r.record.z, "file": r.record.file,
                    "entries": r.record.entries, "texels": r.texels, "seconds": r.seconds,
                    "skipped": r.skipped, "input_hash": r.record.input_hash,
                });
                if let Some(g) = &r.gpu {
                    row["gpu"] = g.clone();
                }
                row
            })
            .collect()
    };
    let settings = json!({
        "ring": args.cells.ring,
        "cell_size_m": streaming.cell_size_m,
        "force": args.cells.force,
        "density": args.density,
        "samples": bake.samples,
        "bounces": bake.bounces,
        "seed": bake.seed,
        "backend": if args.gpu { "gpu" } else { "cpu" },
    });
    if args.dry_run {
        let cells: Vec<Value> = part
            .cells
            .iter()
            .map(|(c, inst)| json!({ "key": c.label(), "x": c.x, "z": c.z, "instances": inst.len() }))
            .collect();
        return Ok(json!({
            "ok": true, "dry_run": true, "cells_mode": true, "scene": label,
            "out": slash(&primary_out), "cell_dir": slash(&dir), "cells": cells,
            "always_loaded": { "instances": part.always.len(), "entities": part.always_entities },
            "settings": settings, "timings": timings, "seconds": started.elapsed().as_secs_f64(),
        }));
    }
    std::fs::create_dir_all(&dir).map_err(|e| {
        LightmapCommandError::new(
            "lightmap_io",
            Some(slash(&dir)),
            format!("cannot create {}: {e}", dir.display()),
        )
    })?;
    let index_path = dir.join(CELL_BAKE_INDEX_FILE);
    let old_index = match std::fs::read_to_string(&index_path) {
        Ok(text) => Some(
            CellBakeIndex::parse(&text)
                .map_err(|e| LightmapCommandError::new(e.code, Some(slash(&index_path)), e.message))?,
        ),
        Err(_) => None,
    };
    // An index of another grid or ring holds hashes of other bakes.
    let old_index = old_index.filter(|i| i.cell_size_m == streaming.cell_size_m && i.ring == args.cells.ring);
    let by_key: HashMap<(Guid, u32), usize> = lb
        .parts
        .iter()
        .map(|p| ((p.key.entity, p.key.part), p.instance))
        .collect();
    let parts_of = |instances: &[usize]| -> Vec<&InstancePart> {
        let set: BTreeSet<usize> = instances.iter().copied().collect();
        let mut v: Vec<&InstancePart> = lb.parts.iter().filter(|p| set.contains(&p.instance)).collect();
        v.sort_by_key(|p| (p.key.entity, p.key.part));
        v
    };
    let receivers_of = |instances: &[usize]| parts_of(instances).iter().any(|p| p.receive);

    // The primary: the always-loaded set's receivers against the whole level.
    let primary_started = Instant::now();
    let primary_hash = input_hash(&lb.scene, &parts_of(&part.always), args, "primary");
    let primary_skipped = !args.cells.force
        && primary_out.is_file()
        && old_index.as_ref().is_some_and(|i| i.primary_hash == primary_hash);
    if !primary_skipped {
        let primary = if receivers_of(&part.always) {
            let all: Vec<usize> = (0..lb.scene.instances.len()).collect();
            bake_receivers(
                args,
                &lb.scene,
                &lb.parts,
                &part.always,
                &all,
                &primary_out,
                "always",
            )?
            .0
        } else {
            let flat = regia_pt::lightmap::LightmapScene::from_pt_scene(&lb.scene)
                .map_err(|e| LightmapCommandError::new("lightmap_scene", None, e.to_string()))?;
            empty_primary(flat.contents(&bake) & LightmapContents::KNOWN)?
        };
        write_map(&primary, &primary_out)?;
    }
    let primary_json = json!({
        "out": slash(&primary_out),
        "entries": Lightmap::read(&primary_out).map(|m| m.entries.len()).unwrap_or(0),
        "skipped": primary_skipped,
        "seconds": primary_started.elapsed().as_secs_f64(),
    });
    lap("primary", &mut timings);

    // The cells.
    let mut rows: Vec<CellRow> = Vec::new();
    let mut held: BTreeMap<CellCoord, Held> = BTreeMap::new();
    let mut baked: BTreeSet<CellCoord> = BTreeSet::new();
    for (&c, own) in &part.cells {
        if !receivers_of(own) {
            continue;
        }
        let cell_started = Instant::now();
        let key = c.label();
        let file = format!("{key}.flmap");
        let out = dir.join(&file);
        let hash = input_hash(&lb.scene, &parts_of(own), args, "cell");
        let previous = old_index.as_ref().and_then(|i| i.cell(c.x, c.z));
        if !args.cells.force
            && out.is_file()
            && let Some(prev) = previous.filter(|p| p.input_hash == hash)
        {
            rows.push(CellRow {
                record: prev.clone(),
                texels: 0,
                seconds: cell_started.elapsed().as_secs_f64(),
                skipped: true,
                gpu: None,
            });
            continue;
        }
        let mut keep: BTreeSet<usize> = own.iter().copied().collect();
        for n in ring_of(c, args.cells.ring) {
            if let Some(v) = part.cells.get(&n) {
                keep.extend(v);
            }
        }
        keep.extend(&part.always);
        let keep: Vec<usize> = keep.into_iter().collect();
        let (map, texels, gpu) = bake_receivers(args, &lb.scene, &lb.parts, own, &keep, &out, &key)?;
        rows.push(CellRow {
            record: CellBakeRecord {
                key: key.clone(),
                x: c.x,
                z: c.z,
                file,
                input_hash: hash,
                entries: map.entries.len(),
                width: map.width,
                height: map.height,
            },
            texels,
            seconds: cell_started.elapsed().as_secs_f64(),
            skipped: false,
            gpu,
        });
        let mut h = Held::new(map, &lb.scene, &by_key);
        h.dirty = true;
        held.insert(c, h);
        baked.insert(c);
    }
    lap("bake", &mut timings);

    // The seam pass.
    let present: BTreeSet<CellCoord> = rows
        .iter()
        .map(|r| CellCoord::new(r.record.x, r.record.z))
        .collect();
    let mut seams = Vec::new();
    for (a, b, axis) in seam_pairs(&baked, &present) {
        for c in [a, b] {
            if let std::collections::btree_map::Entry::Vacant(slot) = held.entry(c) {
                let path = dir.join(format!("{}.flmap", c.label()));
                let map = Lightmap::read(&path)
                    .map_err(|e| LightmapCommandError::new(e.code(), Some(slash(&path)), e.to_string()))?;
                slot.insert(Held::new(map, &lb.scene, &by_key));
            }
        }
        let at = (match axis {
            BorderAxis::X => b.x as f64,
            BorderAxis::Z => b.z as f64,
        } * streaming.cell_size_m) as f32;
        let border = CellBorder { axis, at };
        let (Some(mut ha), Some(mut hb)) = (held.remove(&a), held.remove(&b)) else {
            continue;
        };
        let pa: Vec<&[[f32; 3]]> = ha.positions.iter().map(Vec::as_slice).collect();
        let pb: Vec<&[[f32; 3]]> = hb.positions.iter().map(Vec::as_slice).collect();
        let report: CellSeamReport = stitch_cell_border(&mut ha.map, &pa, &mut hb.map, &pb, border);
        ha.dirty |= !report.texels_a.is_empty() || !report.direction_texels_a.is_empty();
        hb.dirty |= !report.texels_b.is_empty() || !report.direction_texels_b.is_empty();
        seams.push(json!({
            "a": a.label(), "b": b.label(), "axis": axis, "at": at, "edges": report.edges,
            "before": report.before, "after": report.after,
            "texels_a": report.texels_a.len(), "texels_b": report.texels_b.len(),
            "direction_texels_a": report.direction_texels_a.len(),
            "direction_texels_b": report.direction_texels_b.len(),
            "direction_texels": report.direction_texels_a.len() + report.direction_texels_b.len(),
        }));
        held.insert(a, ha);
        held.insert(b, hb);
    }
    lap("seams", &mut timings);
    for (c, h) in &held {
        if h.dirty {
            write_map(&h.map, &dir.join(format!("{}.flmap", c.label())))?;
        }
    }
    let index = CellBakeIndex {
        format_version: CELL_BAKE_INDEX_VERSION,
        level: label.clone(),
        cell_size_m: streaming.cell_size_m,
        ring: args.cells.ring,
        exposure: CELL_LIGHTMAP_EXPOSURE,
        primary_hash,
        cells: rows.iter().map(|r| r.record.clone()).collect(),
    };
    regia_agent::bake_export::write_atomic(&index_path, index.to_text().as_bytes())
        .map_err(|e| LightmapCommandError::new("lightmap_io", Some(slash(&index_path)), e.to_string()))?;
    lap("write", &mut timings);
    let worst = |pick: fn(&Value) -> f64| seams.iter().map(pick).fold(0.0f64, f64::max);
    let direction_seam_texels: u64 = seams
        .iter()
        .map(|s| s["direction_texels"].as_u64().unwrap_or(0))
        .sum();
    let mut summary = json!({
        "ok": true,
        "cells_mode": true,
        "scene": label,
        "out": slash(&primary_out),
        "cell_dir": slash(&dir),
        "index": slash(&index_path),
        "primary": primary_json,
        "cells": cells_json(&rows),
        "baked": rows.iter().filter(|r| !r.skipped).count(),
        "skipped": rows.iter().filter(|r| r.skipped).count(),
        "seams": seams,
        "direction_seam_texels": direction_seam_texels,
        "border_step": {
            "before_p95": worst(|s| s["before"]["p95"].as_f64().unwrap_or(0.0)),
            "after_p95": worst(|s| s["after"]["p95"].as_f64().unwrap_or(0.0)),
            "after_max": worst(|s| s["after"]["max"].as_f64().unwrap_or(0.0)),
        },
        "always_loaded": { "instances": part.always.len(), "entities": part.always_entities },
        "settings": settings,
        "timings": timings,
        "seconds": started.elapsed().as_secs_f64(),
    });
    lb.describe(&mut summary);
    if args.write_level {
        summary["level_written"] = json!(super::level::write_level(level_path, &lb.project, &primary_out)?);
    }
    Ok(summary)
}

fn write_map(map: &Lightmap, path: &Path) -> Result<(), LightmapCommandError> {
    map.write(path)
        .map_err(|e| LightmapCommandError::new(e.code(), Some(slash(path)), e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use base64::Engine as _;
    use regia_asset::lightmap::Coverage;

    use super::super::execute as bake;
    use super::*;

    /// Cell side of the generated worlds, meters.
    const CELL: f32 = 16.0;

    /// A box entity: guid number, centre, size, a forced cell.
    struct Cube {
        id: u32,
        center: [f32; 3],
        size: [f32; 3],
        cell: Option<(i64, i64)>,
    }

    fn guid(id: u32) -> String {
        format!("00000000-0000-0000-0000-{id:012x}")
    }

    /// A unit cube (side 1, centred) as glTF with an embedded buffer: 24 vertices, outward normals.
    fn cube_gltf() -> String {
        let (mut pos, mut nrm, mut idx) = (Vec::<f32>::new(), Vec::<f32>::new(), Vec::<u16>::new());
        for a in 0..3 {
            for s in [1.0f32, -1.0] {
                let (u, v) = ((a + 1) % 3, (a + 2) % 3);
                let base = (pos.len() / 3) as u16;
                for (cu, cv) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
                    let mut p = [0.0f32; 3];
                    p[a] = 0.5 * s;
                    p[u] = cu;
                    p[v] = cv;
                    pos.extend(p);
                    let mut n = [0.0f32; 3];
                    n[a] = s;
                    nrm.extend(n);
                }
                let tris: [u16; 6] = if s > 0.0 {
                    [0, 1, 2, 0, 2, 3]
                } else {
                    [0, 2, 1, 0, 3, 2]
                };
                idx.extend(tris.map(|t| base + t));
            }
        }
        let mut bytes = Vec::new();
        for f in pos.iter().chain(&nrm) {
            bytes.extend(f.to_le_bytes());
        }
        for i in &idx {
            bytes.extend(i.to_le_bytes());
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2}}]}}],
"buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,{data}"}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":288}},{{"buffer":0,"byteOffset":288,"byteLength":288}},{{"buffer":0,"byteOffset":576,"byteLength":72}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":24,"type":"VEC3","min":[-0.5,-0.5,-0.5],"max":[0.5,0.5,0.5]}},
{{"bufferView":1,"componentType":5126,"count":24,"type":"VEC3"}},{{"bufferView":2,"componentType":5123,"count":36,"type":"SCALAR"}}]}}"#,
            bytes.len()
        )
    }

    fn level_text(cubes: &[Cube], streamed: bool) -> String {
        let mut out = String::from("SceneFile(format_version: 10, entities: [");
        for c in cubes {
            let forced = c.cell.map_or(String::new(), |(x, z)| {
                format!("\"regia_scene::streaming::StreamPlacement\": Cell(x: {x}, z: {z}),")
            });
            let _ = write!(
                out,
                "(guid: \"{}\", name: \"c{}\", components: {{ \"regia_scene::transform::Transform\": (translation: ({:?}, {:?}, {:?}), scale: ({:?}, {:?}, {:?})), \
                 \"regia_render::runtime::MeshSource\": (mesh: \"assets/meshes/cube.gltf\", material: \"\"), \
                 \"regia_scene::components::GiMobility\": Static, {forced} }}),",
                guid(c.id),
                c.id,
                c.center[0],
                c.center[1],
                c.center[2],
                c.size[0],
                c.size[1],
                c.size[2],
            );
        }
        let streaming = if streamed {
            format!(
                "\"regia_scene::streaming::LevelStreaming\": (enabled: true, cell_size_m: {CELL:?}, load_radius_m: 32.0, unload_radius_m: 48.0, hlod: None),"
            )
        } else {
            String::new()
        };
        let _ = write!(
            out,
            "], resources: {{ \"regia_scene::level::LevelSettings\": (atmosphere: (preset: Earth, time_of_day: Some((hour: 12.0, day_of_year: 172, latitude_deg: 40.0)))), {streaming} }})"
        );
        out
    }

    /// A project holding the level; returns the project and the level path.
    fn project(tag: &str, cubes: &[Cube]) -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let meshes = dir.join("assets/meshes");
        let levels = dir.join("assets/levels");
        for d in [&meshes, &levels] {
            std::fs::create_dir_all(d).expect("dirs");
        }
        regia_test_utils::write_atomic(&meshes.join("cube.gltf"), cube_gltf().as_bytes()).expect("mesh");
        let level = levels.join("world.level.ron");
        regia_test_utils::write_atomic(&level, level_text(cubes, true).as_bytes()).expect("level");
        regia_test_utils::write_atomic(
            &dir.join(regia_core::brand::MANIFEST_FILE),
            b"[project]\nname = \"t\"\nid = \"com.example.t\"\n",
        )
        .expect("manifest");
        (dir, level)
    }

    /// An `n_x × n_z` grid of floor tiles (one a cell, 0.5 m thick, top at y = 0) and a 2 m prop in
    /// each cell; with `wall` a 4 m wall crossing the border x = 16 at z = 8, one half in each of
    /// cells (0, 0) and (1, 0).
    fn grid_world(nx: i64, nz: i64, wall: bool) -> Vec<Cube> {
        let mut cubes = Vec::new();
        for x in 0..nx {
            for z in 0..nz {
                let (ox, oz) = (x as f32 * CELL, z as f32 * CELL);
                let k = (x * 64 + z) as u32;
                cubes.push(Cube {
                    id: 0x1000 + k,
                    center: [ox + 8.0, -0.25, oz + 8.0],
                    size: [CELL, 0.5, CELL],
                    cell: None,
                });
                cubes.push(Cube {
                    id: 0x2000 + k,
                    center: [ox + 4.0, 1.0, oz + 4.0 + (x + z) as f32 % 3.0],
                    size: [2.0, 2.0, 2.0],
                    cell: None,
                });
            }
        }
        if wall {
            for (id, cx) in [(0x3000, 12.0), (0x3001, 20.0)] {
                cubes.push(Cube {
                    id,
                    center: [cx, 2.0, 8.0],
                    size: [8.0, 4.0, 0.5],
                    cell: None,
                });
            }
        }
        cubes
    }

    fn args(level: &Path) -> BakeLightmapsArgs {
        use super::super::{DenoiseMode, GpuBakeArgs, Switch, UnwrapMode};
        BakeLightmapsArgs {
            scene: None,
            level: Some(level.to_path_buf()),
            project: None,
            sun: Some(regia_agent::bake_export::SunOverride {
                elevation_deg: 40.0,
                azimuth_deg: 30.0,
                lux: 50_000.0,
            }),
            unwrap: UnwrapMode::Atlas,
            seam_budget: None,
            out: None,
            density: 1.0,
            padding: 2,
            max_atlas: 1024,
            max_pages: super::super::DEFAULT_MAX_PAGES,
            chart_angle: 60.0,
            samples: 16,
            bounces: 1,
            seed: 5,
            direct_sun: false,
            gpu: false,
            gpu_opts: GpuBakeArgs::default(),
            compare: None,
            no_filter: false,
            denoise: DenoiseMode::On,
            denoise_iterations: Some(3),
            encoding: TexelEncodingArg::Rgb9e5,
            crease: Switch::On,
            stitch: Switch::On,
            auto_density: Switch::On,
            no_directional: false,
            max_samples: 0,
            noise_target: 0.0,
            threads: 2,
            min_area: 0.0,
            instances: Vec::new(),
            dry_run: false,
            inspect: false,
            meshlets: false,
            write_level: false,
            checkpoint: crate::commands::bake_checkpoint::CheckpointArgs::default(),
            cells: CellArgs {
                cells: true,
                ..CellArgs::default()
            },
        }
    }

    fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for e in std::fs::read_dir(dir).expect("cell dir").flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".flmap") {
                out.insert(name, std::fs::read(e.path()).expect("read"));
            }
        }
        out
    }

    fn row<'a>(v: &'a Value, key: &str) -> &'a Value {
        v["cells"]
            .as_array()
            .and_then(|c| c.iter().find(|r| r["key"] == key))
            .unwrap_or_else(|| panic!("no cell {key}: {v}"))
    }

    /// Mean irradiance (luminance) of an entry's baked texels.
    fn entry_mean(map: &Lightmap, name: &str) -> f32 {
        let Some(e) = map.entry(name).map(|i| &map.entries[i]) else {
            panic!("no entry {name}");
        };
        let [x0, y0, w, h] = e.rect;
        let (mut sum, mut n) = (0.0f32, 0u32);
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                if map.coverage[(y * map.width + x) as usize] == Coverage::BAKED {
                    let c = map.irradiance_at(x, y);
                    sum += 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                    n += 1;
                }
            }
        }
        sum / n.max(1) as f32
    }

    /// A 3 × 3 world bakes one file per cell plus an empty primary, the wall across x = 16 shows a
    /// border step the seam pass brings below 0.05 (p95, relative), and a second run skips every
    /// cell and changes no byte.
    #[test]
    fn a_3x3_world_bakes_per_cell_with_a_seam_pass_and_skips_unchanged_cells() {
        let (dir, level) = project("lightmap_cells_3x3", &grid_world(3, 3, true));
        let v = bake(&args(&level)).expect("cell bake");
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["baked"], 9, "{v}");
        assert_eq!(v["skipped"], 0);
        assert_eq!(v["primary"]["entries"], 0, "no always-loaded receiver: {v}");
        let direction_total: u64 = v["seams"]
            .as_array()
            .expect("seam reports")
            .iter()
            .map(|seam| {
                let a = seam["direction_texels_a"].as_u64().expect("direction count a");
                let b = seam["direction_texels_b"].as_u64().expect("direction count b");
                assert_eq!(seam["direction_texels"].as_u64(), Some(a + b), "{seam}");
                a + b
            })
            .sum();
        assert_eq!(v["direction_seam_texels"].as_u64(), Some(direction_total));
        let cell_dir = dir.join("assets/maps/world.cells");
        let first = files(&cell_dir);
        assert_eq!(first.len(), 9);
        for (name, bytes) in &first {
            let map = Lightmap::from_bytes(bytes).expect("a cell file reads");
            assert_eq!(map.exposure, CELL_LIGHTMAP_EXPOSURE, "{name}");
            assert!(map.entries.iter().all(|e| e.key.is_some()), "{name}");
        }
        // The cell (0, 0) holds its floor, its prop and its half of the wall.
        assert_eq!(row(&v, "0_0")["entries"], 3, "{v}");
        assert!(row(&v, "0_0")["texels"].as_u64() > Some(0));
        // A CPU bake carries no GPU stats.
        assert!(row(&v, "0_0").get("gpu").is_none(), "{v}");
        // The wall's border: measured before, and below the bound after.
        let seam = v["seams"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["a"] == "0_0" && s["b"] == "1_0"))
            .unwrap_or_else(|| panic!("no 0_0 | 1_0 seam: {v}"))
            .clone();
        let before = seam["before"]["p95"].as_f64().unwrap_or(1.0);
        let after = seam["after"]["p95"].as_f64().unwrap_or(1.0);
        eprintln!(
            "border 0_0 | 1_0: before {} after {}",
            seam["before"], seam["after"]
        );
        assert!(seam["edges"].as_u64() >= Some(2), "floor and wall edges: {seam}");
        assert!(after < 0.05 && after <= before, "{seam}");
        // The step on the written files is the reported one.
        let a = Lightmap::from_bytes(&first["0_0.flmap"]).expect("0_0");
        let b = Lightmap::from_bytes(&first["1_0.flmap"]).expect("1_0");
        assert!(a.width > 0 && b.width > 0);
        // Unchanged: every cell skipped, no byte moves, no seam runs.
        let again = bake(&args(&level)).expect("again");
        assert_eq!(
            (again["baked"].clone(), again["skipped"].clone()),
            (json!(0), json!(9)),
            "{again}"
        );
        assert!(again["primary"]["skipped"] == true);
        assert_eq!(again["seams"].as_array().map(Vec::len), Some(0));
        assert!(row(&again, "1_1")["skipped"] == true);
        assert_eq!(files(&cell_dir), first);
        let index = CellBakeIndex::parse(
            &std::fs::read_to_string(cell_dir.join(CELL_BAKE_INDEX_FILE)).expect("index"),
        )
        .expect("the index reads");
        assert_eq!(index.cells.len(), 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An irradiance-only cell bake keeps its format and reports no direction edits; an unchanged
    /// repeat still skips both cells and leaves the serialized files byte-identical.
    #[test]
    fn a_non_directional_cell_bake_keeps_files_and_reports_zero_direction_edits() {
        let (dir, level) = project("lightmap_cells_no_directions", &grid_world(2, 1, true));
        let mut settings = args(&level);
        settings.no_directional = true;
        let result = bake(&settings).expect("irradiance-only cell bake");
        assert_eq!(result["baked"], 2, "{result}");
        assert_eq!(result["direction_seam_texels"].as_u64(), Some(0));
        let seams = result["seams"].as_array().expect("seam reports");
        assert!(!seams.is_empty(), "the shared border must be exercised");
        for seam in seams {
            for field in ["direction_texels", "direction_texels_a", "direction_texels_b"] {
                assert_eq!(seam[field].as_u64(), Some(0), "{seam}");
            }
        }
        let cell_dir = dir.join("assets/maps/world.cells");
        let first = files(&cell_dir);
        assert_eq!(first.len(), 2);
        for bytes in first.values() {
            let map = Lightmap::from_bytes(bytes).expect("irradiance-only file reads");
            assert!(!map.is_directional());
            assert!(map.direction_blocks.is_none());
        }
        let again = bake(&settings).expect("unchanged irradiance-only bake");
        assert_eq!(
            (again["baked"].as_u64(), again["skipped"].as_u64()),
            (Some(0), Some(2))
        );
        assert_eq!(again["direction_seam_texels"].as_u64(), Some(0));
        assert_eq!(files(&cell_dir), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-baking one cell (its prop moved) rewrites that cell's file and, of its neighbors, only the
    /// texels the seam pass reports; the diagonal cells and the rest keep every byte.
    #[test]
    fn re_baking_one_cell_changes_only_its_file_and_its_neighbors_seam_texels() {
        let mut world = grid_world(3, 3, false);
        let (dir, level) = project("lightmap_cells_incremental", &world);
        bake(&args(&level)).expect("first bake");
        let cell_dir = dir.join("assets/maps/world.cells");
        let first = files(&cell_dir);
        let prop = 0x2000 + 64 + 1;
        if let Some(c) = world.iter_mut().find(|c| c.id == prop) {
            c.center[0] += 3.0;
        }
        regia_test_utils::write_atomic(&level, level_text(&world, true).as_bytes()).expect("edit");
        let v = bake(&args(&level)).expect("second bake");
        assert_eq!(
            (v["baked"].clone(), v["skipped"].clone()),
            (json!(1), json!(8)),
            "{v}"
        );
        assert!(row(&v, "1_1")["skipped"] == false);
        let second = files(&cell_dir);
        assert_ne!(first["1_1.flmap"], second["1_1.flmap"]);
        for corner in ["0_0", "2_0", "0_2", "2_2"] {
            assert_eq!(
                first[&format!("{corner}.flmap")],
                second[&format!("{corner}.flmap")],
                "{corner}"
            );
        }
        let seams = v["seams"].as_array().cloned().unwrap_or_default();
        assert_eq!(seams.len(), 4, "the four edge neighbors: {v}");
        for n in ["1_0", "0_1", "2_1", "1_2"] {
            let file = format!("{n}.flmap");
            let (old, new) = (
                Lightmap::from_bytes(&first[&file]).expect("old"),
                Lightmap::from_bytes(&second[&file]).expect("new"),
            );
            assert_eq!(
                (&old.entries, &old.coverage, old.exposure, old.directions.len()),
                (&new.entries, &new.coverage, new.exposure, new.directions.len()),
                "{n}: only texels change"
            );
            let changed = old.texels.iter().zip(&new.texels).filter(|(a, b)| a != b).count();
            let dir_changed = old
                .directions
                .iter()
                .zip(&new.directions)
                .filter(|(a, b)| a != b)
                .count();
            let seam = seams
                .iter()
                .find(|s| s["a"] == n || s["b"] == n)
                .unwrap_or_else(|| panic!("no seam with {n}"));
            let side = if seam["a"] == n { "texels_a" } else { "texels_b" };
            let dir_side = if seam["a"] == n {
                "direction_texels_a"
            } else {
                "direction_texels_b"
            };
            let reported = seam[side].as_u64().unwrap_or(0) as usize;
            let reported_dir = seam[dir_side].as_u64().unwrap_or(0) as usize;
            assert!(
                changed <= reported,
                "{n}: {changed} texels changed, the seam reports {reported}"
            );
            assert!(
                dir_changed <= reported_dir,
                "{n}: {dir_changed} direction texels changed, the seam reports {reported_dir}"
            );
            if reported_dir > 0 {
                assert!(
                    dir_changed > 0,
                    "{n}: the seam's direction texels were not written"
                );
            }
            assert!(
                changed * 10 < old.texels.len(),
                "{n}: a seam is a strip ({changed} of {})",
                old.texels.len()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A room whose floor is in cell (0, 0) and whose walls and roof are forced into cell (1, 0)
    /// leaks no more light onto the floor than the same room baked as one level; with no neighbor
    /// ring the walls are missing from the floor's bake and the floor is lit.
    #[test]
    fn a_room_sealed_by_a_neighbor_cell_leaks_no_more_than_one_level() {
        let mut world = grid_world(2, 1, false);
        let floor = 0x4000;
        world.push(Cube {
            id: floor,
            center: [15.0, 0.05, 8.0],
            size: [5.0, 0.1, 5.0],
            cell: None,
        });
        for (id, center, size) in [
            (0x4001, [12.25, 1.5, 8.0], [0.5, 3.2, 6.0]),
            (0x4002, [17.75, 1.5, 8.0], [0.5, 3.2, 6.0]),
            (0x4003, [15.0, 1.5, 10.75], [6.0, 3.2, 0.5]),
            (0x4004, [15.0, 1.5, 5.25], [6.0, 3.2, 0.5]),
            (0x4005, [15.0, 3.25, 8.0], [6.0, 0.5, 6.0]),
        ] {
            world.push(Cube {
                id,
                center,
                size,
                cell: Some((1, 0)),
            });
        }
        let (dir, level) = project("lightmap_cells_room", &world);
        let name = format!("entity_{}_0", guid(floor));
        let mut one = args(&level);
        one.cells.cells = false;
        one.out = Some(dir.join("one.flmap"));
        bake(&one).expect("one level");
        let one_level = entry_mean(&Lightmap::read(&dir.join("one.flmap")).expect("one"), &name);
        let cells = |ring: u32, out: &str| {
            let mut a = args(&level);
            a.cells.ring = ring;
            a.out = Some(dir.join(out));
            let v = bake(&a).expect("cells");
            assert_eq!(v["ok"], true, "{v}");
            let stem = out.trim_end_matches(".flmap");
            entry_mean(
                &Lightmap::read(&dir.join(format!("{stem}.cells/0_0.flmap"))).expect("cell 0_0"),
                &name,
            )
        };
        let ring1 = cells(1, "ring1.flmap");
        let ring0 = cells(0, "ring0.flmap");
        eprintln!("sealed floor: one level {one_level} lux, ring 1 {ring1} lux, ring 0 {ring0} lux");
        assert!(
            ring1 <= one_level * 1.05 + 0.5,
            "ring 1 {ring1} vs one level {one_level}"
        );
        assert!(
            ring0 > 10.0 * (one_level + 1.0),
            "without the ring the room is open: {ring0}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CPU bake of a cell is deterministic: two bakes, on 1 and 3 threads, write the same bytes
    /// for every cell, and a checkpointed bake writes them too (and leaves no checkpoint).
    #[test]
    fn cell_files_are_deterministic_per_cell_and_with_checkpoints() {
        let (dir, level) = project("lightmap_cells_determinism", &grid_world(2, 2, true));
        let run = |threads: usize, checkpoint: bool, out: &str| {
            let mut a = args(&level);
            a.threads = threads;
            a.checkpoint.checkpoint = checkpoint;
            a.out = Some(dir.join(out));
            bake(&a).expect("cells");
            files(&dir.join(format!("{}.cells", out.trim_end_matches(".flmap"))))
        };
        let a = run(1, false, "a.flmap");
        assert_eq!(a.len(), 4);
        assert_eq!(a, run(3, false, "b.flmap"));
        assert_eq!(a, run(2, true, "c.flmap"));
        let left: Vec<_> = std::fs::read_dir(dir.join("c.cells"))
            .expect("dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bakeck"))
            .collect();
        assert!(left.is_empty(), "checkpoints are removed when a cell finishes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A level without `LevelStreaming` is refused, as are the block encodings.
    #[test]
    fn cells_need_a_streamed_level_and_rgb9e5() {
        let world = grid_world(1, 1, false);
        let (dir, level) = project("lightmap_cells_refused", &world);
        let mut a = args(&level);
        a.encoding = TexelEncodingArg::Bc6h;
        assert_eq!(bake(&a).map_err(|e| e.code), Err("lightmap_bake_settings"));
        regia_test_utils::write_atomic(&level, level_text(&world, false).as_bytes()).expect("unstreamed");
        assert_eq!(bake(&args(&level)).map_err(|e| e.code), Err(CELLS_NOT_STREAMED));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_partition_follows_the_cook_rules_and_the_ring_is_chebyshev() {
        assert_eq!(ring_of(CellCoord::new(0, 0), 1).count(), 8);
        assert_eq!(ring_of(CellCoord::new(5, -2), 2).count(), 24);
        assert_eq!(ring_of(CellCoord::new(0, 0), 0).count(), 0);
        assert_eq!(cell_dir(Path::new("a/maps/w.flmap")), Path::new("a/maps/w.cells"));
        let baked: BTreeSet<CellCoord> = [CellCoord::new(1, 1)].into();
        let present: BTreeSet<CellCoord> = (0..3)
            .flat_map(|x| (0..3).map(move |z| CellCoord::new(x, z)))
            .collect();
        assert_eq!(seam_pairs(&baked, &present).len(), 4);
        assert_eq!(seam_pairs(&present, &present).len(), 12);
    }

    /// The partition follows the cook (spec 78 § 5.3): a child goes with its root even when it stands
    /// in another cell, a forced cell wins, `AlwaysLoaded` and an entity wider than the extent limit
    /// go to the always-loaded set, everything else to the cell of its centre.
    #[test]
    fn the_partition_keeps_hierarchies_together_and_follows_the_placements() {
        use regia_core::glam::{Mat4, Vec3};
        let qb = regia_pt::lightmap::quad_box_scene();
        let (mesh, material) = (
            qb.scene.instances[qb.cube].mesh,
            qb.scene.instances[qb.cube].material,
        );
        let mut scene = qb.scene;
        scene.instances.clear();
        let at = |x: f32, z: f32, s: f32| {
            Mat4::from_scale_rotation_translation(
                Vec3::splat(s),
                regia_core::glam::Quat::IDENTITY,
                Vec3::new(x, 0.0, z),
            )
        };
        // 0 root (cell 0_0), 1 its child standing in cell 1_0, 2 alone in 1_0, 3 forced to 5_5,
        // 4 always-loaded, 5 wider than two cells.
        for t in [
            at(4.0, 4.0, 1.0),
            at(20.0, 4.0, 1.0),
            at(20.0, 4.0, 1.0),
            at(4.0, 4.0, 1.0),
            at(4.0, 4.0, 1.0),
            at(8.0, 8.0, 100.0),
        ] {
            scene.add_instance(mesh, material, t);
        }
        let g = |n: u128| Guid::from_u128(n);
        let parts: Vec<InstancePart> = (0..6)
            .map(|k| InstancePart {
                instance: k,
                key: regia_asset::lightmap::LightmapEntryKey {
                    entity: g(k as u128 + 1),
                    part: 0,
                },
                label: String::new(),
                density_scale: 1.0,
                receive: true,
            })
            .collect();
        let settings = LevelStreaming {
            enabled: true,
            cell_size_m: f64::from(CELL),
            ..LevelStreaming::default()
        };
        let placements: HashMap<Guid, StreamPlacement> = [
            (g(4), StreamPlacement::Cell { x: 5, z: 5 }),
            (g(5), StreamPlacement::AlwaysLoaded),
        ]
        .into();
        let roots: HashMap<Guid, Guid> = [(g(2), g(1))].into();
        let p = partition(&scene, &parts, &settings, &placements, &roots).expect("partition");
        assert_eq!(p.cells.get(&CellCoord::new(0, 0)), Some(&vec![0, 1]), "{p:?}");
        assert_eq!(p.cells.get(&CellCoord::new(1, 0)), Some(&vec![2]));
        assert_eq!(p.cells.get(&CellCoord::new(5, 5)), Some(&vec![3]));
        assert_eq!((p.always.clone(), p.always_entities), (vec![4, 5], 2));
        // The roots of a scene's entities: a chain resolves to its top, a missing parent stops.
        let entity = |n: u128, parent: Option<u128>| regia_scene::format::SceneEntity {
            guid: g(n),
            parent: parent.map(g),
            ..regia_scene::format::SceneEntity::default()
        };
        let r = hierarchy_roots(&[
            entity(1, None),
            entity(2, Some(1)),
            entity(3, Some(2)),
            entity(4, Some(99)),
        ]);
        assert_eq!(
            (r.get(&g(2)), r.get(&g(3)), r.get(&g(4))),
            (Some(&g(1)), Some(&g(1)), Some(&g(4)))
        );
    }

    /// Per-cell bake times for the report (not a test of time): `cargo test -p regia_cli --bin regia
    /// cell_bake_times -- --ignored --nocapture` (add `--features lightmap-gpu` and
    /// `REGIA_CELL_TIMES_GPU=1` for the GPU baker). The 8 × 8 world bakes at 8 samples.
    #[test]
    #[ignore = "measurement for the lane report"]
    fn cell_bake_times() {
        for (n, samples) in [(3, 16), (8, 8)] {
            let (dir, level) = project("lightmap_cells_times", &grid_world(n, n, true));
            let mut a = args(&level);
            a.samples = samples;
            a.threads = 0;
            // `REGIA_CELL_TIMES_DENSITY=4`: texels per meter (default 1).
            if let Some(d) = std::env::var("REGIA_CELL_TIMES_DENSITY")
                .ok()
                .and_then(|d| d.parse().ok())
            {
                a.density = d;
            }
            // `REGIA_CELL_TIMES_GPU=1` with `--features lightmap-gpu`: the GPU baker (lavapipe on a CI host).
            a.gpu = std::env::var_os("REGIA_CELL_TIMES_GPU").is_some();
            let v = bake(&a).expect("cells");
            let rows: Vec<String> = v["cells"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .map(|r| {
                            format!(
                                "{} {:.3}s {}tx",
                                r["key"],
                                r["seconds"].as_f64().unwrap_or(0.0),
                                r["texels"]
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Which baker ran (the GPU stats name the adapter) and the bytes of cell 1_1, so a CPU and
            // a `--gpu` run can be told apart.
            let cell_dir = v["cell_dir"].as_str().map(PathBuf::from).unwrap_or_default();
            let bytes = std::fs::read(cell_dir.join("1_1.flmap")).unwrap_or_default();
            eprintln!(
                "{n}x{n} ({samples} spp, density {}, gpu {}, adapter {}): total {:.2}s timings {} border {} \
                 cell 1_1 blake3 {}\n  {}",
                a.density,
                a.gpu,
                v["cells"][0]["gpu"]["adapter"],
                v["seconds"].as_f64().unwrap_or(0.0),
                v["timings"],
                v["border_step"],
                blake3::hash(&bytes).to_hex(),
                rows.join("\n  ")
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
