// SPDX-License-Identifier: MPL-2.0
//! Biome nodes (`biome.*`, spec 65 § 5). The stages live in [`crate::biome::pipeline`].

use std::sync::{Arc, OnceLock};

use regia_core::math::Vec3;

use crate::biome::def::{BiomeDef, EntrySpawn};
use crate::biome::ecology::{CompPoint, Competition, Species, SpeciesCatalog, competition_pass, map_species};
use crate::biome::pipeline::{FilteredOut, LayerJob, layer_runs_no_subgraph, resolve, run_layer};
use crate::biome::{BiomeWorld, grass};
use crate::data::{AttrColumn, AttrValue, Bundle, DataItem, DataKind, MaskData, Payload, quat};
use crate::exec::{Collision, Instance, InstanceSet, NodeCtx, PrefabSpawn};
use crate::nodes::common::{Combine, Region, euler_deg, xz_bounds};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SPATIAL, SettingType as T, Settings, kind, num, pin_in,
    pin_multi, pin_out, pin_req, pins_points_inout, set,
};

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "biome.areas",
        "biome",
        "One claim item (a Mask of claim weights) per (area, biome) touching the bounds",
        &[num(
            "cell_m",
            T::Float,
            "1.0",
            0.05,
            1000.0,
            "Claim raster cell, meters (coarsened to at most 2048 cells per side)",
        )],
        |_| {
            PinSpec::new(
                vec![pin_in("bounds", SPATIAL)],
                vec![pin_out("out", kind(DataKind::Mask))],
            )
        },
        eval_areas,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "biome.layer",
        "biome",
        "Candidates, channels, entry picks, placement, filters, self spacing and children per claim and layer",
        &[set(
            "layers",
            T::NameList,
            "[]",
            "Only these layer names (empty = every layer)",
        )],
        |_| {
            PinSpec::new(
                vec![pin_req("claims", kind(DataKind::Mask)), pin_in("ground", SPATIAL)],
                vec![pin_out("out", POINTS), pin_out("filtered", POINTS)],
            )
        },
        eval_layer,
    )
    .uncached()
    .world(),
    BuiltinNode::in_place(
        "biome.resolve",
        "biome",
        "Cross-layer resolve: lower-ranked cores overlapping higher-ranked cores are removed",
        &[num(
            "max_core_m",
            T::Float,
            "4.0",
            0.0,
            1.0e3,
            "Largest `core_m` expected; bounds the declared reach (spec 66 § 2)",
        )],
        |_| PinSpec::new(vec![pin_multi("in", POINTS)], vec![pin_out("out", POINTS)]),
        eval_resolve,
    )
    .reaching(|s| 2.0 * s.f("max_core_m")),
    BuiltinNode::in_place(
        "biome.exclude",
        "biome",
        "Removes points inside `procgen_exclude` regions, road exclusions and `regions` items",
        &[set(
            "tagged",
            T::Bool,
            "true",
            "Use the captured exclusion regions (tagged entities, roads)",
        )],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_multi("regions", SPATIAL)],
                vec![pin_out("out", POINTS)],
            )
        },
        eval_exclude,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "biome.spawn",
        "biome",
        "Spawns each point's entry (mesh instances or prefabs) with the entry's render settings",
        &[set(
            "name",
            T::Name,
            "\"{entry}_{index}\"",
            "Prefab entity name pattern ({entry}, {index})",
        )],
        pins_points_inout,
        eval_spawn,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "biome.grass_mask",
        "biome",
        "Per grass class a multiplier Mask over the generator bounds; exported as `<export>/<class>`",
        &[
            num(
                "texel_m",
                T::Float,
                "1.0",
                0.05,
                1000.0,
                "Texel size, meters (at most 4096 per side)",
            ),
            set(
                "export",
                T::Name,
                "\"grass_multiplier\"",
                "Export name prefix (empty = no export)",
            ),
        ],
        |_| {
            PinSpec::new(
                vec![pin_in("claims", kind(DataKind::Mask))],
                vec![pin_out("out", kind(DataKind::Mask))],
            )
        },
        eval_grass,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "biome.stats",
        "biome",
        "Passes points through and counts them per entry into the run report",
        &[],
        pins_points_inout,
        eval_stats,
    )
    .uncached(),
    BuiltinNode::new(
        "biome.ecology",
        "biome",
        "Competition-aware thinning (ADR-2456): shade-needing understory only under tree crowns, light-demanding understory out of deep shade, canopy self-thinning by density",
        &[num(
            "reach_m",
            T::Float,
            "50.0",
            0.0,
            1.0e3,
            "Largest thinning cell or crown radius expected; bounds the declared reach (spec 66 § 2)",
        )],
        |_| PinSpec::new(vec![pin_multi("in", POINTS)], vec![pin_out("out", POINTS)]),
        eval_ecology,
    )
    .uncached()
    .world()
    .reaching(|s| s.f("reach_m")),
];

fn biome_world(ctx: &mut NodeCtx<'_>) -> Arc<BiomeWorld> {
    match ctx.world().biome() {
        Some(w) => w,
        None => {
            ctx.warn("biome_no_areas", "no biome areas were captured");
            Arc::new(BiomeWorld::default())
        }
    }
}

fn scope_int(item: &DataItem, key: &str) -> Option<i64> {
    match item.scope.get(key) {
        Some(AttrValue::Int(v)) => Some(*v),
        _ => None,
    }
}

fn scope_text<'a>(item: &'a DataItem, key: &str) -> Option<&'a str> {
    match item.scope.get(key) {
        Some(AttrValue::Name(v) | AttrValue::Asset(v)) => Some(v),
        _ => None,
    }
}

fn eval_areas(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("biome_areas").entered();
    let world = biome_world(ctx);
    let bounds = crate::nodes::input(inputs, 0);
    let rect = if bounds.0.is_empty() {
        ctx.world().self_info().map(|si| {
            let (lo, hi) = si.bounds.world_aabb();
            [lo[0], lo[2], hi[0], hi[2]]
        })
    } else {
        bounds
            .0
            .iter()
            .filter_map(xz_bounds)
            .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
    };
    let Some(rect) = rect else {
        return Ok(vec![Bundle::empty()]);
    };
    // One raster job per (area, biome). Jobs that share a rectangle and a resolution share one
    // `claims_at` per texel. A raster above 512² stays per area so the group does not hold every
    // biome's image at once while sampling; each texel is still resolved once for that area.
    struct AreaMask {
        ai: usize,
        bi: usize,
        path: String,
        name: String,
        guid: String,
        priority: i64,
        rect: [f32; 4],
        w: u32,
        h: u32,
        tw: f32,
        th: f32,
    }
    let mut jobs: Vec<AreaMask> = Vec::new();
    for (ai, area) in world.areas.iter().enumerate() {
        let ab = area.outline.xz_bounds();
        let r = [
            ab[0].max(rect[0]),
            ab[1].max(rect[1]),
            ab[2].min(rect[2]),
            ab[3].min(rect[3]),
        ];
        if !(r[0] < r[2] && r[1] < r[3]) {
            continue;
        }
        let cell = (s.f("cell_m") as f32).max((r[2] - r[0]).max(r[3] - r[1]) / 2048.0);
        let w = ((r[2] - r[0]) / cell).ceil().max(1.0) as u32;
        let h = ((r[3] - r[1]) / cell).ceil().max(1.0) as u32;
        let (tw, th) = ((r[2] - r[0]) / w as f32, (r[3] - r[1]) / h as f32);
        ctx.check_points((w as usize).saturating_mul(h as usize))?;
        for (bi, (path, def)) in area.biomes.iter().enumerate() {
            jobs.push(AreaMask {
                ai,
                bi,
                path: path.clone(),
                name: def.name.clone(),
                guid: area.guid.to_string(),
                priority: area.priority(bi),
                rect: r,
                w,
                h,
                tw,
                th,
            });
        }
    }
    let mut groups: std::collections::BTreeMap<([u32; 4], u32, u32), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, job) in jobs.iter().enumerate() {
        groups
            .entry((job.rect.map(f32::to_bits), job.w, job.h))
            .or_default()
            .push(i);
    }
    let mut rasters: Vec<Option<(Vec<f32>, Vec<u8>)>> = vec![None; jobs.len()];
    for group in groups.into_values() {
        let Some(&first) = group.first() else {
            continue;
        };
        let pixels = (jobs[first].w as usize).saturating_mul(jobs[first].h as usize);
        let mut chunks: Vec<Vec<usize>> = Vec::new();
        if pixels > 512 * 512 {
            let mut by_area: std::collections::BTreeMap<usize, Vec<usize>> =
                std::collections::BTreeMap::new();
            for i in group {
                by_area.entry(jobs[i].ai).or_default().push(i);
            }
            chunks.extend(by_area.into_values());
        } else {
            chunks.push(group);
        }
        for chunk in chunks {
            let Some(&sample_i) = chunk.first() else {
                continue;
            };
            let sample = &jobs[sample_i];
            let (w, h, tw, th, r) = (sample.w, sample.h, sample.tw, sample.th, sample.rect);
            ctx.check_cancel()?;
            // Texels in row-major order, split into element chunks that run on the pool and are
            // joined in chunk order (ADR-7252): the same rasters for any split.
            let texels = (w as usize).saturating_mul(h as usize);
            let fill = |range: std::ops::Range<usize>| {
                let mut values: Vec<Vec<f32>> =
                    chunk.iter().map(|_| Vec::with_capacity(range.len())).collect();
                let mut seconds: Vec<Vec<u8>> =
                    chunk.iter().map(|_| Vec::with_capacity(range.len())).collect();
                for t in range {
                    let (row, col) = (t / w as usize, t % w as usize);
                    let z = r[1] + (row as f32 + 0.5) * th;
                    let x = r[0] + (col as f32 + 0.5) * tw;
                    let claim = world.claims_at(x, z);
                    for (k, &ji) in chunk.iter().enumerate() {
                        let job = &jobs[ji];
                        values[k].push(claim.weight_of(job.ai, job.bi));
                        let second = claim
                            .second
                            .is_some_and(|s| s.area == job.ai && s.biome == job.bi);
                        seconds[k].push(u8::from(second));
                    }
                }
                (values, seconds)
            };
            let parts = ctx.map_chunks(texels, fill)?;
            let mut values: Vec<Vec<f32>> = chunk.iter().map(|_| Vec::with_capacity(texels)).collect();
            let mut seconds: Vec<Vec<u8>> = chunk.iter().map(|_| Vec::with_capacity(texels)).collect();
            for (part_values, part_seconds) in parts {
                for (k, (v, sec)) in part_values.into_iter().zip(part_seconds).enumerate() {
                    values[k].extend(v);
                    seconds[k].extend(sec);
                }
            }
            for (k, ji) in chunk.into_iter().enumerate() {
                rasters[ji] = Some((std::mem::take(&mut values[k]), std::mem::take(&mut seconds[k])));
            }
        }
    }
    let mut out = Bundle::empty();
    for (job, raster) in jobs.iter().zip(rasters) {
        let Some((values, second)) = raster else {
            continue;
        };
        if values.iter().all(|v| *v <= 0.0) {
            continue;
        }
        let mut mask = MaskData::new(job.rect, job.w, job.h, values, true)
            .map_err(|m| NodeError::new("biome_invalid", m))?;
        mask.second = second;
        let mut item = DataItem::new(Payload::Mask(Arc::new(mask)));
        item.tags.insert("biome".to_string());
        item.scope
            .insert("biome".into(), AttrValue::Name(job.name.clone()));
        item.scope
            .insert("biome_path".into(), AttrValue::Asset(job.path.clone()));
        item.scope
            .insert("area".into(), AttrValue::Name(job.guid.clone()));
        item.scope
            .insert("area_index".into(), AttrValue::Int(job.ai as i64));
        item.scope
            .insert("biome_index".into(), AttrValue::Int(job.bi as i64));
        item.scope.insert("priority".into(), AttrValue::Int(job.priority));
        out.0.push(item);
    }
    if world.unknown_map_texels > 0 {
        ctx.count("unknown_map_texels", world.unknown_map_texels);
    }
    Ok(vec![out])
}

/// A claim's `(area, biome)` indices (when known) and its definition.
type ClaimDef = (Option<(usize, usize)>, Arc<BiomeDef>);

/// The definition a claim item stands for.
fn claim_def(world: &BiomeWorld, item: &DataItem) -> Option<ClaimDef> {
    if let (Some(a), Some(b)) = (scope_int(item, "area_index"), scope_int(item, "biome_index"))
        && let Some((_, def)) = usize::try_from(a)
            .ok()
            .and_then(|a| world.areas.get(a))
            .and_then(|area| area.biomes.get(usize::try_from(b).ok()?))
    {
        return Some((Some((a as usize, b as usize)), def.clone()));
    }
    let path = scope_text(item, "biome_path")?;
    world.defs.get(path).map(|d| (None, d.clone()))
}

/// The asset path of the biome a claim item stands for.
fn claim_path(world: &BiomeWorld, item: &DataItem) -> Option<String> {
    if let (Some(a), Some(b)) = (scope_int(item, "area_index"), scope_int(item, "biome_index"))
        && let Some((path, _)) = usize::try_from(a)
            .ok()
            .and_then(|a| world.areas.get(a))
            .and_then(|area| area.biomes.get(usize::try_from(b).ok()?))
    {
        return Some(path.clone());
    }
    scope_text(item, "biome_path").map(str::to_string)
}

/// One `biome.layer` job: a claim item, its claimant and definition, and a layer index; `None` for a
/// claim item that names no captured biome (the job only warns, so warnings keep their order).
type LayerTask<'a> = Option<(&'a DataItem, Option<(usize, usize)>, Arc<BiomeDef>, usize)>;

fn eval_layer(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("biome_layer").entered();
    let world = biome_world(ctx);
    let only = s.names("layers");
    let ground = crate::nodes::input(inputs, 1)
        .0
        .iter()
        .find_map(|i| match &i.payload {
            Payload::Ground(g) => Some(g.clone()),
            _ => None,
        })
        .or_else(|| ctx.world().ground());
    let water = OnceLock::new();
    // Debug (preview) runs only: the candidates entry filters removed, for the overlay's filter
    // view (spec 65 § 7). Bake and runtime runs record nothing and output an empty bundle.
    let keep_debug = ctx.inputs.keep_debug;
    // One job per (claim, layer), in claim then layer order (ADR-7252).
    let claims = crate::nodes::input(inputs, 0);
    let mut jobs: Vec<LayerTask<'_>> = Vec::new();
    for claim in &claims.0 {
        let Some((claimant, def)) = claim_def(&world, claim) else {
            jobs.push(None);
            continue;
        };
        for (li, layer) in def.layers.iter().enumerate() {
            if !only.is_empty() && !only.contains(&layer.name) {
                continue;
            }
            jobs.push(Some((claim, claimant, def.clone(), li)));
        }
    }
    let run = |ctx: &mut NodeCtx<'_>, task: LayerTask<'_>| {
        let Some((claim, claimant, def, li)) = task else {
            ctx.warn("biome_unknown", "a claim item names no captured biome");
            return Ok(None);
        };
        let Some(layer) = def.layers.get(li) else {
            return Ok(None);
        };
        let mut filtered: Option<Vec<FilteredOut>> = keep_debug.then(Vec::new);
        let mut job = LayerJob {
            def: &def,
            layer,
            claim,
            claimant,
            world: Some(world.clone()),
            ground: ground.clone(),
            water: &water,
            filtered: filtered.as_mut(),
        };
        let item = run_layer(ctx, &mut job)?;
        Ok(Some((item, filtered.unwrap_or_default())))
    };
    // Jobs are independent unless a layer runs a subgraph (whose element cap reads the whole run
    // output): then they run one after another on this context, as before.
    let forked = jobs
        .iter()
        .flatten()
        .all(|(_, _, def, li)| def.layers.get(*li).is_none_or(layer_runs_no_subgraph));
    let results: Vec<Option<(DataItem, Vec<FilteredOut>)>> = if forked {
        ctx.fork_jobs(jobs, run)?
    } else {
        jobs.into_iter()
            .map(|task| run(ctx, task))
            .collect::<Result<_, NodeError>>()?
    };
    let mut out = Bundle::empty();
    let mut filtered: Option<Vec<FilteredOut>> = keep_debug.then(Vec::new);
    for (item, removed) in results.into_iter().flatten() {
        out.0.push(item);
        if let Some(list) = filtered.as_mut() {
            list.extend(removed);
        }
    }
    Ok(vec![out, filtered_bundle(filtered.unwrap_or_default())])
}

/// The `filtered` pin of `biome.layer`: one point set (attributes `entry`, `filter`), or nothing.
fn filtered_bundle(list: Vec<FilteredOut>) -> Bundle {
    if list.is_empty() {
        return Bundle::empty();
    }
    let mut set = crate::data::PointSet::new();
    let mut entries = Vec::with_capacity(list.len());
    let mut filters = Vec::with_capacity(list.len());
    for f in list {
        set.push(crate::data::Point {
            position: f.pos,
            ..crate::data::Point::default()
        });
        entries.push(f.entry);
        filters.push(f.filter);
    }
    set.attrs.insert("entry".into(), AttrColumn::Name(entries.into()));
    set.attrs
        .insert("filter".into(), AttrColumn::Name(filters.into()));
    Bundle::one(DataItem::points(set))
}

fn eval_resolve(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    _: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("biome_resolve").entered();
    let mut items = crate::nodes::take_input(inputs, 0).0;
    let (keep, removed) = resolve(&items);
    for (item, k) in items.iter_mut().zip(&keep) {
        if let Payload::Points(p) = &item.payload
            && k.len() == p.len()
        {
            let (kept, _) = p.partition(k);
            item.payload = Payload::Points(kept);
        }
    }
    for (name, n) in removed {
        ctx.count(&format!("{name}|resolved_out"), n);
    }
    Ok(vec![Bundle(items)])
}

fn entry_name(item: &DataItem, i: usize) -> String {
    match item.as_points().and_then(|p| p.attrs.get("entry")) {
        Some(AttrColumn::Name(v)) => v.get(i).cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

fn eval_exclude(
    ctx: &mut NodeCtx<'_>,
    inputs: &mut [Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("biome_exclude").entered();
    let world = ctx.world().biome();
    let regions_bundle = crate::nodes::input(inputs, 1);
    let regions = Region::new(&regions_bundle, Combine::Max)?;
    let has_regions = !regions_bundle.0.is_empty();
    let tagged = s.b("tagged");
    let index = if tagged {
        world.as_ref().map(|w| exclude_index(&w.exclusions))
    } else {
        None
    };
    let mut tallies: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut out = crate::nodes::take_input(inputs, 0);
    for item in &mut out.0 {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let mut keep = vec![true; p.len()];
        for (i, k) in keep.iter_mut().enumerate() {
            let pos = p.position[i];
            let hit = (tagged
                && index.as_ref().is_some_and(|index| {
                    world
                        .as_ref()
                        .is_some_and(|w| exclusion_hit(index, &w.exclusions, pos))
                }))
                || (has_regions && regions.value(pos) > 0.0);
            if hit {
                *k = false;
                *tallies.entry(entry_name(item, i)).or_default() += 1;
            }
        }
        if let Payload::Points(p) = &item.payload {
            let (kept, _) = p.partition(&keep);
            item.payload = Payload::Points(kept);
        }
    }
    for (name, n) in tallies {
        crate::probe::hit_count_flush();
        ctx.count(&format!("{name}|excluded"), n);
    }
    Ok(vec![out])
}

/// Cell size of the exclusion grid, meters.
const EXCLUDE_CELL_M: f32 = 32.0;
/// An exclusion covering more cells than this is tested by its box, not inserted into every cell.
const EXCLUDE_WIDE_CELLS: i64 = 64;

struct ExcludeIndex {
    cells: std::collections::BTreeMap<(i32, i32), Vec<usize>>,
    wide: Vec<usize>,
    bounds: Vec<[f32; 4]>,
}

fn exclude_cell(v: f32) -> i32 {
    (v / EXCLUDE_CELL_M).floor() as i32
}

fn xz_span(points: &[[f32; 2]], pad: f32) -> [f32; 4] {
    if points.is_empty() {
        return [0.0; 4];
    }
    let mut rect = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for p in points {
        rect[0] = rect[0].min(p[0] - pad);
        rect[1] = rect[1].min(p[1] - pad);
        rect[2] = rect[2].max(p[0] + pad);
        rect[3] = rect[3].max(p[1] + pad);
    }
    rect
}

fn exclusion_bounds(region: &crate::biome::ExclusionRegion) -> [f32; 4] {
    match region {
        crate::biome::ExclusionRegion::Shape(shape) => {
            crate::biome::area::AreaOutline::Shape(*shape).xz_bounds()
        }
        crate::biome::ExclusionRegion::Polygon(points) => xz_span(points, 0.0),
        crate::biome::ExclusionRegion::Corridor { points, half_width } => xz_span(points, *half_width),
    }
}

fn exclude_index(regions: &[crate::biome::ExclusionRegion]) -> ExcludeIndex {
    let mut index = ExcludeIndex {
        cells: std::collections::BTreeMap::new(),
        wide: Vec::new(),
        bounds: Vec::with_capacity(regions.len()),
    };
    for (i, region) in regions.iter().enumerate() {
        let bounds = exclusion_bounds(region);
        index.bounds.push(bounds);
        let (x0, z0) = (exclude_cell(bounds[0]), exclude_cell(bounds[1]));
        let (x1, z1) = (exclude_cell(bounds[2]), exclude_cell(bounds[3]));
        let across = i64::from(x1).saturating_sub(i64::from(x0)).saturating_add(1);
        let down = i64::from(z1).saturating_sub(i64::from(z0)).saturating_add(1);
        let cells = across.saturating_mul(down);
        if cells > EXCLUDE_WIDE_CELLS {
            index.wide.push(i);
            continue;
        }
        for z in z0..=z1 {
            for x in x0..=x1 {
                index.cells.entry((x, z)).or_default().push(i);
            }
        }
    }
    index
}

fn exclusion_hit(index: &ExcludeIndex, regions: &[crate::biome::ExclusionRegion], pos: [f32; 3]) -> bool {
    let inside = |i: usize| {
        let bounds = index.bounds[i];
        pos[0] >= bounds[0]
            && pos[0] <= bounds[2]
            && pos[2] >= bounds[1]
            && pos[2] <= bounds[3]
            && regions[i].contains(pos)
    };
    if index.wide.iter().copied().any(inside) {
        return true;
    }
    index
        .cells
        .get(&(exclude_cell(pos[0]), exclude_cell(pos[2])))
        .is_some_and(|list| list.iter().copied().any(inside))
}

fn eval_spawn(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("biome_spawn").entered();
    let world = biome_world(ctx);
    let pattern = s.text("name").to_string();
    let cap = ctx.limits().prefabs_per_generator;
    let out = crate::nodes::input(inputs, 0);
    let mut placed: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut slot_mesh: Option<String> = None;
    let mut slot_shadows = false;
    let mut slot_collision = Collision::None;
    let mut slot_index = 0usize;
    for item in &out.0 {
        let Some(p) = item.as_points() else {
            continue;
        };
        let def = claim_def(&world, item).map(|(_, d)| d);
        let layer_name = scope_text(item, "layer").unwrap_or("");
        let layer = def.as_ref().and_then(|d| d.layer(layer_name));
        let Some(layer) = layer else {
            if !p.is_empty() {
                ctx.warn("biome_unknown", "points name no captured biome layer");
            }
            continue;
        };
        let idx = match p.attrs.get("entry_index") {
            Some(AttrColumn::Int(v)) => v.clone(),
            _ => vec![-1; p.len()].into(),
        };
        for (i, &ix) in idx.iter().enumerate() {
            let Some(e) = usize::try_from(ix).ok().and_then(|k| layer.entries.get(k)) else {
                continue;
            };
            // Spacers claim space but place nothing, so they are not counted (ADR-1665 (5)).
            if !matches!(e.spawn, EntrySpawn::None) {
                *placed.entry(entry_name(item, i)).or_default() += 1;
            }
            let (pq, ps, pt) = (
                quat(p.rotation[i]),
                Vec3::from(p.scale[i]),
                Vec3::from(p.position[i]),
            );
            let (ot, oq, os) = match e.placement.offset {
                Some((t, r, sc)) => (
                    Vec3::new(t.0 as f32, t.1 as f32, t.2 as f32),
                    euler_deg([r.0, r.1, r.2]),
                    Vec3::new(sc.0 as f32, sc.1 as f32, sc.2 as f32),
                ),
                None => (Vec3::ZERO, euler_deg([0.0; 3]), Vec3::ONE),
            };
            let translation = (pt + pq * (ps * ot)).to_array();
            let rotation = (pq * oq).normalize().to_array();
            let scale = (ps * os).to_array();
            match &e.spawn {
                EntrySpawn::None => {}
                EntrySpawn::Mesh(mesh) => {
                    let collision = match e.render.collision {
                        crate::biome::def::EntryCollision::None => Collision::None,
                        crate::biome::def::EntryCollision::Simple => Collision::Simple,
                        crate::biome::def::EntryCollision::Mesh => Collision::Mesh,
                    };
                    let index = if slot_mesh.as_deref() == Some(mesh.as_str())
                        && slot_shadows == e.render.cast_shadows
                        && slot_collision == collision
                    {
                        slot_index
                    } else {
                        let proto = InstanceSet {
                            mesh: mesh.clone(),
                            material: None,
                            cast_shadows: e.render.cast_shadows,
                            collision,
                            draw_distance_m: e.render.draw_distance_m,
                            fade_m: e.render.fade_m,
                            cull_below_px: 2.0,
                            instances: Vec::new(),
                            layer: None,
                        };
                        let index = ctx.output().set_index_for(&proto);
                        slot_mesh = Some(mesh.clone());
                        slot_shadows = e.render.cast_shadows;
                        slot_collision = collision;
                        slot_index = index;
                        index
                    };
                    ctx.output().instance_sets[index].instances.push(Instance {
                        translation,
                        rotation,
                        scale,
                        color: [1.0; 4],
                        custom: [0.0; 4],
                    });
                }
                EntrySpawn::Prefab(prefab) => {
                    let index = ctx.output().prefabs.len();
                    if index >= cap {
                        for (name, n) in placed {
                            crate::probe::hit_count_flush();
                            ctx.count(&format!("{name}|placed"), n);
                        }
                        return Err(NodeError::new(
                            "limit_exceeded",
                            format!("prefabs_per_generator: more than {cap}"),
                        ));
                    }
                    let spawn = PrefabSpawn {
                        prefab: prefab.clone(),
                        name: pattern
                            .replace("{entry}", &e.name)
                            .replace("{index}", &index.to_string()),
                        translation,
                        rotation,
                        scale,
                        tags: Vec::new(),
                        overrides: Vec::new(),
                    };
                    ctx.output().prefabs.push(spawn);
                }
            }
        }
    }
    for (name, n) in placed {
        crate::probe::hit_count_flush();
        ctx.count(&format!("{name}|placed"), n);
    }
    Ok(vec![out])
}

fn eval_grass(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let world = biome_world(ctx);
    let rect = ctx
        .world()
        .self_info()
        .map(|si| {
            let (lo, hi) = si.bounds.world_aabb();
            [lo[0], lo[2], hi[0], hi[2]]
        })
        .or_else(|| {
            crate::nodes::input(inputs, 0)
                .0
                .iter()
                .filter_map(xz_bounds)
                .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
        });
    let Some(rect) = rect else {
        return Ok(vec![Bundle::empty()]);
    };
    let export = s.text("export").to_string();
    let mut out = Bundle::empty();
    let masks = grass::grass_masks_in(&world, rect, s.f("texel_m") as f32, |len, fill| {
        ctx.map_chunks(len, fill)
    })?;
    for (class, mask) in masks {
        if !export.is_empty() {
            ctx.output()
                .masks
                .insert(format!("{export}/{class}"), mask.clone());
        }
        let mut item = DataItem::new(Payload::Mask(Arc::new(mask)));
        item.tags.insert(class.clone());
        item.scope.insert("class".into(), AttrValue::Name(class));
        out.0.push(item);
    }
    Ok(vec![out])
}

fn eval_stats(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let out = crate::nodes::input(inputs, 0);
    let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for item in &out.0 {
        for i in 0..item.payload.len() {
            *counts.entry(entry_name(item, i)).or_default() += 1;
        }
    }
    for (name, n) in counts {
        ctx.count(&format!("{name}|points"), n);
    }
    Ok(vec![out])
}

/// One biome's species per `(layer, entry)` and its competition rules.
struct EcoBiome {
    species: std::collections::BTreeMap<(usize, usize), Species>,
    competition: Competition,
    group: u32,
}

fn eval_ecology(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!("biome_ecology").entered();
    let world = biome_world(ctx);
    let mut items = crate::nodes::input(inputs, 0).0;
    let mut biomes: std::collections::BTreeMap<String, Option<Arc<EcoBiome>>> =
        std::collections::BTreeMap::new();
    let mut points: Vec<CompPoint> = Vec::new();
    let mut origin: Vec<(usize, usize)> = Vec::new();
    for (ii, item) in items.iter().enumerate() {
        ctx.check_cancel()?;
        let Some(p) = item.as_points() else {
            continue;
        };
        let eco = match (claim_path(&world, item), claim_def(&world, item)) {
            (Some(path), Some((_, def))) => {
                let next = biomes.len() as u32;
                biomes
                    .entry(path.clone())
                    .or_insert_with(|| {
                        let profile = world.ecology.get(&path).map(|p| p.as_ref());
                        let catalog =
                            profile.map_or_else(|| SpeciesCatalog::builtin().clone(), |p| p.catalog());
                        let (mapped, _, _) = map_species(&def, profile, &catalog);
                        let species = mapped
                            .iter()
                            .filter_map(|m| catalog.get(&m.species).map(|s| ((m.layer, m.index), s.clone())))
                            .collect();
                        Some(Arc::new(EcoBiome {
                            species,
                            competition: profile.map(|p| p.competition).unwrap_or_default(),
                            group: next,
                        }))
                    })
                    .clone()
                    .map(|e| (e, def))
            }
            _ => None,
        };
        let layer_index = eco.as_ref().and_then(|(_, def)| {
            def.layers
                .iter()
                .position(|l| Some(l.name.as_str()) == scope_text(item, "layer"))
        });
        for i in 0..p.len() {
            let entry = match p.attrs.get("entry_index") {
                Some(AttrColumn::Int(v)) => v.get(i).and_then(|k| usize::try_from(*k).ok()),
                _ => None,
            };
            let species = match (&eco, layer_index, entry) {
                (Some((e, _)), Some(l), Some(k)) => e.species.get(&(l, k)).map(|s| (e, s)),
                _ => None,
            };
            let spread = match p.attrs.get("spread_m") {
                Some(AttrColumn::Float(v)) => v.get(i).copied().unwrap_or(0.0) as f32,
                _ => 0.0,
            };
            let sc = p.scale[i];
            // The geometric mean through detmath: platform f32::cbrt may differ in the last ulp,
            // which crosses the thinning slack and swaps the survival order (audit: procgen
            // determinism).
            let size = crate::detmath::cbrt(sc[0].abs() * sc[1].abs() * sc[2].abs());
            let pos = p.position[i];
            points.push(match species {
                Some((e, s)) => CompPoint {
                    x: pos[0],
                    z: pos[2],
                    spread_m: spread,
                    size,
                    seed: p.seed[i],
                    stratum: Some(s.stratum),
                    light: s.light,
                    casts_shade: s.casts_shade,
                    vernal: s.vernal,
                    shade_rules: e.competition.shade_rules,
                    thinning: e.competition.self_thinning.map(|t| (e.group, t)),
                },
                None => CompPoint {
                    x: pos[0],
                    z: pos[2],
                    spread_m: spread,
                    size,
                    seed: p.seed[i],
                    stratum: None,
                    light: 0.0,
                    casts_shade: 0.0,
                    vernal: false,
                    shade_rules: false,
                    thinning: None,
                },
            });
            origin.push((ii, i));
        }
    }
    let verdict = competition_pass(&points);
    let mut keep: Vec<Vec<bool>> = items
        .iter()
        .map(|it| vec![true; it.as_points().map_or(0, |p| p.len())])
        .collect();
    let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (k, v) in verdict.iter().enumerate() {
        if let Some(why) = v {
            let (ii, i) = origin[k];
            keep[ii][i] = false;
            *counts
                .entry(format!("{}|{}", entry_name(&items[ii], i), why.counter()))
                .or_default() += 1;
        }
    }
    for (item, k) in items.iter_mut().zip(&keep) {
        if let Payload::Points(p) = &item.payload
            && k.len() == p.len()
            && k.iter().any(|x| !x)
        {
            let (kept, _) = p.partition(k);
            item.payload = Payload::Points(kept);
        }
    }
    for (name, n) in counts {
        ctx.count(&name, n);
    }
    Ok(vec![Bundle(items)])
}
