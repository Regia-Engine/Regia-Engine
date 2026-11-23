// SPDX-License-Identifier: MPL-2.0
//! The stages of `biome.layer` (spec 65 § 6.1-6.3) and the cross-layer resolve (§ 6.4).
//!
//! Each stage draws from its own stream `mix(node_seed, stage)` (spec 65 § 11): 1 candidate source,
//! 2 channel split, 3 entry pick, 4 placement, 5 filters (no draws), 6 realize, 7 children (child
//! rule `r` uses `mix(mix(node_seed, 7), r)`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

use regia_core::math::Vec3;
use regia_core::math::hash::mix;

use crate::biome::def::{
    BiomeDef, BiomeEntry, BiomeLayer, CandidateSource, ChannelRule, EntryFilter, EntrySpawn, KeepClear,
    ScaleRule, Sharing, Yaw, window_factor,
};
use crate::biome::water::WaterField;
use crate::biome::{BiomeWorld, entry_path};
use crate::data::{
    AttrColumn, AttrValue, Bundle, DataItem, GroundData, MaskData, Payload, Point, PointSet, quat,
};
use crate::detmath;
use crate::exec::{CHUNK, NodeCtx};
use crate::nodes::common::{read_num, rotation_between};
use crate::nodes::{NodeError, Settings};
use crate::rot::Rot as Quat;
use crate::seeds;
use crate::value::SettingValue;

/// Stage numbers (spec 65 § 11).
pub(crate) const STAGE_SOURCE: u32 = 1;
pub(crate) const STAGE_CHANNEL: u32 = 2;
pub(crate) const STAGE_PICK: u32 = 3;
pub(crate) const STAGE_PLACE: u32 = 4;
pub(crate) const STAGE_REALIZE: u32 = 6;
pub(crate) const STAGE_CHILDREN: u32 = 7;

/// The golden angle, radians (137.508°).
const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;

/// Fallback footprint when the mesh bounds are unknown.
const DEFAULT_SPREAD_M: f32 = 1.0;

/// One working point of a layer.
#[derive(Clone, Debug)]
struct Cand {
    pos: [f32; 3],
    rot: [f32; 4],
    scale: [f32; 3],
    density: f32,
    seed: u32,
    entry: usize,
    depth: u32,
    /// Placed by the second claim at its spot: realize keeps `1 - u < d` (a crossfade).
    second: bool,
    core: f32,
    spread: f32,
    /// Index into the source points (attribute reads); `None` for children.
    src: Option<usize>,
    /// Parent's xz and radii (children only).
    parent: Option<([f32; 2], f32)>,
}

/// What a layer run needs besides the context.
pub(crate) struct LayerJob<'a> {
    /// The biome.
    pub def: &'a BiomeDef,
    /// The layer.
    pub layer: &'a BiomeLayer,
    /// The claim item (a mask; its scope names the area and biome).
    pub claim: &'a DataItem,
    /// `(area index, biome index)` into the captured biome world, when known.
    pub claimant: Option<(usize, usize)>,
    /// The biome world.
    pub world: Option<Arc<BiomeWorld>>,
    /// The ground.
    pub ground: Option<Arc<GroundData>>,
    /// The water field (built on first use; shared by the node's layer jobs).
    pub water: &'a OnceLock<Option<WaterField>>,
    /// Debug runs (`keep_debug`): every candidate an entry filter removed, for the biome overlay's
    /// filter view (spec 65 § 7). `None` records nothing.
    pub filtered: Option<&'a mut Vec<FilteredOut>>,
}

/// A candidate an entry filter removed (debug runs only; spec 65 § 7 filter view).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FilteredOut {
    /// Where the candidate stood (after placement).
    pub pos: [f32; 3],
    /// The entry name.
    pub entry: String,
    /// `<index>:<kind>` of the filter that removed it (the run report's counter suffix).
    pub filter: String,
}

fn stage_seed(ctx: &NodeCtx<'_>, stage: u32) -> u32 {
    mix(ctx.node_seed, stage)
}

fn draw(seed: u32, stream: u32, slot: u32) -> f64 {
    seeds::draw(seed, stream, slot)
}

/// `(weight, is second)` of the claim at `(x, z)`.
///
/// A mask is the raster `biome.areas` already stored, including its second-claim bit. `claims_at`
/// runs only for a claim item that is not a mask.
fn claim_weight(job: &LayerJob<'_>, x: f32, z: f32) -> (f32, bool) {
    if let Payload::Mask(m) = &job.claim.payload {
        return (m.sample(x, z), m.claim_second_at(x, z));
    }
    if let (Some(w), Some((a, b))) = (&job.world, job.claimant) {
        let c = w.claims_at(x, z);
        let owner = c.owner.is_some_and(|o| o.area == a && o.biome == b);
        return (c.weight_of(a, b), !owner);
    }
    (1.0, false)
}

fn claim_rect(job: &LayerJob<'_>) -> Option<[f32; 4]> {
    let b = crate::nodes::common::xz_bounds(job.claim)?;
    match &job.ground {
        Some(g) => {
            let r = [
                b[0].max(g.rect[0]),
                b[1].max(g.rect[1]),
                b[2].min(g.rect[2]),
                b[3].min(g.rect[3]),
            ];
            (r[0] <= r[2] && r[1] <= r[3]).then_some(r)
        }
        None => Some(b),
    }
}

/// The weight of terrain layer `index` at a candidate: procedural (slope from the ground normal
/// through `detmath`, altitude = the point's height) blended with any painted weight there.
fn layer_weight(job: &LayerJob<'_>, index: usize, x: f32, y: f32, z: f32) -> Option<f32> {
    let n = job.ground.as_ref().map_or([0.0, 1.0, 0.0], |g| g.normal(x, z));
    let slope = detmath::acos(f64::from(n[1]).clamp(-1.0, 1.0)) as f32;
    job.world
        .as_ref()?
        .layer_weight_indexed(index, slope, y, Some([x, z]))
}

fn slope_deg(n: [f32; 3]) -> f64 {
    detmath::acos(f64::from(n[1]).clamp(-1.0, 1.0)).to_degrees()
}

/// Stage 1: root candidates inside the claim, density × claim value (zero dropped).
fn candidates(ctx: &mut NodeCtx<'_>, job: &LayerJob<'_>) -> Result<(PointSet, Vec<bool>), NodeError> {
    let s1 = stage_seed(ctx, STAGE_SOURCE);
    let mut src = PointSet::new();
    let Some(rect) = claim_rect(job) else {
        return Ok((src, Vec::new()));
    };
    match &job.layer.source {
        CandidateSource::Ground {
            spacing_m,
            jitter,
            noise,
        } => {
            let Some(g) = job.ground.clone() else {
                ctx.warn("no_terrain", "biome.layer: no ground under the claim");
                return Ok((src, Vec::new()));
            };
            let s = *spacing_m;
            let x0 = (f64::from(rect[0]) / s).floor() as i64;
            let x1 = (f64::from(rect[2]) / s).ceil() as i64 - 1;
            let z0 = (f64::from(rect[1]) / s).floor() as i64;
            let z1 = (f64::from(rect[3]) / s).ceil() as i64 - 1;
            let count = ((x1 - x0 + 1).max(0) as u128) * ((z1 - z0 + 1).max(0) as u128);
            ctx.check_points(usize::try_from(count).unwrap_or(usize::MAX))?;
            // Cells in row-major order (z, then x), in element chunks on the pool joined in chunk
            // order (ADR-7252): the same points for any split.
            let width = usize::try_from((x1 - x0 + 1).max(0)).unwrap_or(0);
            let cells = usize::try_from(count).unwrap_or(usize::MAX);
            let (tier_seed, jitter, noise) = (ctx.tier_seed, *jitter, noise.as_ref());
            let parts = ctx.map_chunks(cells, |range| {
                let mut part: Vec<([f32; 3], f32, u32)> = Vec::with_capacity(range.len());
                for t in range {
                    let iz = z0 + (t / width) as i64;
                    let ix = x0 + (t % width) as i64;
                    let cx = ((ix as f64 + 0.5) * s) as f32;
                    let cz = ((iz as f64 + 0.5) * s) as f32;
                    let cell = seeds::point_seed([cx, 0.0, cz], tier_seed);
                    let x = cx + ((draw(cell, s1, 0) - 0.5) * jitter * s) as f32;
                    let z = cz + ((draw(cell, s1, 1) - 0.5) * jitter * s) as f32;
                    if x < rect[0] || x >= rect[2] || z < rect[1] || z >= rect[3] {
                        continue;
                    }
                    let mut d = 1.0f32;
                    if let Some(nz) = noise {
                        let v = crate::nodes::density::fbm(
                            f64::from(x) / nz.scale_m.max(1e-6),
                            f64::from(z) / nz.scale_m.max(1e-6),
                            i64::from(nz.octaves),
                            0.5,
                            2.0,
                            s1,
                        );
                        d = (nz.min + (nz.max - nz.min) * v).clamp(0.0, 1.0) as f32;
                    }
                    if d <= 0.0 {
                        continue;
                    }
                    let position = [x, g.height(x, z), z];
                    part.push((position, d, seeds::point_seed(position, tier_seed)));
                }
                part
            })?;
            for (position, density, seed) in parts.into_iter().flatten() {
                src.push(Point {
                    position,
                    density,
                    seed,
                    ..Point::default()
                });
            }
        }
        CandidateSource::Poisson { min_dist_m } => {
            let mut settings = BTreeMap::new();
            settings.insert("min_dist_m".to_string(), SettingValue::Float(*min_dist_m));
            settings.insert("attempts".to_string(), SettingValue::Int(4));
            settings.insert(
                "max_points".to_string(),
                SettingValue::Int(i64::try_from(ctx.limits().points_per_item).unwrap_or(i64::MAX)),
            );
            let kind = ctx.graph.registry.get("sample.poisson").cloned();
            let Some(kind) = kind else {
                return Ok((src, Vec::new()));
            };
            let saved = ctx.node_seed;
            ctx.node_seed = s1;
            let region = DataItem::new(Payload::Mask(Arc::new(rect_mask(rect))));
            let r = kind.eval(ctx, &[Bundle::one(region)], &Settings(settings));
            ctx.node_seed = saved;
            if let Some(p) = r?.first().and_then(|b| b.0.first()).and_then(DataItem::as_points) {
                src = p.clone();
            }
            src.density.iter_mut().for_each(|d| *d = 1.0);
        }
        CandidateSource::ScatterClass { set, class } => {
            let list = job
                .world
                .as_ref()
                .and_then(|w| w.scatter.get(&(set.clone(), class.clone())).cloned());
            let Some(list) = list else {
                ctx.warn(
                    "biome_source_unsupported",
                    format!(
                        "scatter class `{class}` of `{set}` was not captured (no terrain, or a density map)"
                    ),
                );
                return Ok((src, Vec::new()));
            };
            ctx.check_points(list.len())?;
            for c in list.iter() {
                let [x, _, z] = c.position;
                if x < rect[0] || x > rect[2] || z < rect[1] || z > rect[3] {
                    continue;
                }
                let tilt = rotation_between(Vec3::Y, Vec3::from(c.up));
                let (sy, cy) = detmath::sin_cos(f64::from(c.yaw) * 0.5);
                let rot = (tilt * Quat::from_xyzw(0.0, sy as f32, 0.0, cy as f32)).normalize();
                src.push(Point {
                    position: c.position,
                    rotation: rot.to_array(),
                    scale: [c.scale; 3],
                    density: 1.0,
                    seed: ctx.point_seed(c.position),
                    ..Point::default()
                });
            }
        }
        CandidateSource::Graph { graph, params } => {
            let mut overrides = BTreeMap::new();
            for (k, v) in params {
                let v = SettingValue::parse(v)
                    .map_err(|e| NodeError::new("biome_invalid", format!("graph param `{k}`: {e:?}")))?;
                overrides.insert(k.clone(), v);
            }
            let mut inputs = BTreeMap::new();
            inputs.insert("region".to_string(), Bundle::one(job.claim.clone()));
            let r = ctx.run_subgraph(graph, &overrides, inputs, None)?;
            let out = r.output.graph_outputs.get("out").cloned().unwrap_or_default();
            for item in &out.0 {
                match item.as_points() {
                    Some(p) => src
                        .append(p)
                        .map_err(|m| NodeError::new("attr_type_mismatch", m))?,
                    None => {
                        return Err(NodeError::new(
                            "biome_invalid",
                            format!("graph `{graph}` must output points on `out`"),
                        ));
                    }
                }
            }
        }
    }
    // Density × claim value; drop zero.
    let mut keep = Vec::with_capacity(src.len());
    let mut second = Vec::with_capacity(src.len());
    let density = src.density.make_mut();
    for (i, d) in density.iter_mut().enumerate() {
        let [x, _, z] = src.position[i];
        let (w, sec) = claim_weight(job, x, z);
        *d *= w;
        keep.push(*d > 0.0);
        second.push(sec);
    }
    let (kept, _) = src.partition(&keep);
    let second: Vec<bool> = second
        .into_iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(s, _)| s)
        .collect();
    Ok((kept, second))
}

/// A mask of 1 over a rectangle (a region for `sample.poisson`).
fn rect_mask(rect: [f32; 4]) -> MaskData {
    MaskData {
        rect,
        width: 1,
        height: 1,
        values: vec![1.0],
        bilinear: false,
        second: Vec::new(),
    }
}

/// A mask texture channel mapped over the generator bounds (`Mask` rules and filters).
fn mask_data(ctx: &NodeCtx<'_>, asset: &str, channel: &str) -> Option<MaskData> {
    let img = ctx.world().mask_image(asset)?;
    let bounds = ctx.world().self_info()?.bounds;
    let (lo, hi) = bounds.world_aabb();
    let ch = |p: &[f32; 4]| match channel {
        "g" => p[1],
        "b" => p[2],
        "a" => p[3],
        "luma" => 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2],
        _ => p[0],
    };
    MaskData::new(
        [lo[0], lo[2], hi[0], hi[2]],
        img.width,
        img.height,
        img.rgba.iter().map(ch).collect(),
        true,
    )
    .ok()
}

/// Lazily built per-run lookups.
#[derive(Default)]
struct Lookups {
    masks: BTreeMap<(String, String), Option<MaskData>>,
    attrs: BTreeMap<String, Option<crate::nodes::common::NumColumn>>,
    /// Terrain layer name → index. `None` means the captured stack has no such layer.
    layer_index: BTreeMap<String, Option<usize>>,
    /// Missing layer names already warned about this run.
    layer_warned: BTreeSet<String>,
}

impl Lookups {
    fn mask(&mut self, ctx: &NodeCtx<'_>, asset: &str, channel: &str, x: f32, z: f32) -> Option<f32> {
        if let Some((_, cached)) = self
            .masks
            .iter()
            .find(|(key, _)| key.0 == asset && key.1 == channel)
        {
            return cached.as_ref().map(|m| m.sample(x, z));
        }
        crate::probe::hit_mask_key();
        let data = mask_data(ctx, asset, channel);
        let sample = data.as_ref().map(|m| m.sample(x, z));
        self.masks.insert((asset.to_string(), channel.to_string()), data);
        sample
    }

    /// The layer index, resolved once per name for this run.
    fn layer_index(&mut self, world: Option<&BiomeWorld>, name: &str) -> Option<usize> {
        if let Some(hit) = self.layer_index.get(name) {
            return *hit;
        }
        let index = world.and_then(|w| w.terrain_layers.as_ref()).and_then(|layers| {
            (0..layers.count()).find(|&i| layers.layer(i).is_some_and(|l| l.name == name))
        });
        self.layer_index.insert(name.to_string(), index);
        index
    }

    /// Warns the first time `name` is missing. Later candidates pay no `format!`.
    fn warn_missing_layer(&mut self, ctx: &mut NodeCtx<'_>, name: &str, detail: &str) {
        if self.layer_warned.insert(name.to_string()) {
            crate::probe::hit_layer_warn();
            ctx.warn(
                "biome_terrain_layer_unavailable",
                format!("no terrain layer `{name}` was captured; {detail}"),
            );
        }
    }

    fn attr(&mut self, src: &DataItem, sel: &str, i: Option<usize>) -> Option<f64> {
        let col = self.attrs.entry(sel.to_string()).or_insert_with(|| {
            crate::data::Selector::parse(sel)
                .ok()
                .and_then(|s| read_num(src, &s).ok())
        });
        let i = i?;
        let c = col.as_ref()?;
        (i < c.len()).then(|| c.scalar(i))
    }
}

/// The channel index of each candidate (`None` = no channel took it).
fn split_channels(
    ctx: &mut NodeCtx<'_>,
    job: &LayerJob<'_>,
    src_item: &DataItem,
    src: &PointSet,
    look: &mut Lookups,
) -> Vec<Option<usize>> {
    let layer = job.layer;
    if layer.channels.is_empty() {
        return vec![Some(0); src.len()];
    }
    let s2 = stage_seed(ctx, STAGE_CHANNEL);
    let mut out = Vec::with_capacity(src.len());
    for i in 0..src.len() {
        let [x, y, z] = src.position[i];
        let normal = job.ground.as_ref().map_or([0.0, 1.0, 0.0], |g| g.normal(x, z));
        let mut hit = None;
        for (c, ch) in layer.channels.iter().enumerate() {
            let ok = match &ch.rule {
                ChannelRule::Slope { min_deg, max_deg } => {
                    let s = slope_deg(normal);
                    s >= *min_deg && s <= *max_deg
                }
                ChannelRule::Altitude { min, max } => f64::from(y) >= *min && f64::from(y) <= *max,
                ChannelRule::TerrainLayer { name, min_weight } => {
                    match look
                        .layer_index(job.world.as_deref(), name)
                        .and_then(|i| layer_weight(job, i, x, y, z))
                    {
                        Some(w) => f64::from(w) >= *min_weight,
                        None => {
                            look.warn_missing_layer(ctx, name, "the rule never matches");
                            false
                        }
                    }
                }
                ChannelRule::Mask { asset, channel, min } => look
                    .mask(ctx, asset, channel, x, z)
                    .is_some_and(|v| f64::from(v) >= *min),
                ChannelRule::Attr { sel, min, max } => look
                    .attr(src_item, sel, Some(i))
                    .is_some_and(|v| v >= *min && v <= *max),
                ChannelRule::Random { fraction } => draw(src.seed[i], s2, 0) < *fraction,
                ChannelRule::Rest => true,
            };
            if ok {
                hit = Some(c);
                break;
            }
        }
        out.push(hit);
    }
    out
}

/// Stage 3: the entry of each candidate (roulette by weight, slot 0).
fn pick_entries(
    ctx: &NodeCtx<'_>,
    layer: &BiomeLayer,
    seeds_: &[u32],
    channels: &[Option<usize>],
) -> Vec<Option<usize>> {
    let s3 = stage_seed(ctx, STAGE_PICK);
    let channel_name = |c: usize| -> &str { layer.channels.get(c).map_or("all", |ch| ch.name.as_str()) };
    // Pools per channel.
    let n_channels = layer.channels.len().max(1);
    let pools: Vec<Vec<usize>> = (0..n_channels)
        .map(|c| {
            let name = channel_name(c);
            (0..layer.entries.len())
                .filter(|&k| {
                    let e = &layer.entries[k];
                    e.enabled
                        && e.weight > 0.0
                        && (e.channels.is_empty() || e.channels.iter().any(|n| n == name))
                })
                .collect()
        })
        .collect();
    seeds_
        .iter()
        .zip(channels)
        .map(|(&seed, ch)| {
            let pool = pools.get((*ch)?)?;
            let total: f64 = pool.iter().map(|&k| layer.entries[k].weight).sum();
            if total <= 0.0 {
                return None;
            }
            let u = draw(seed, s3, 0) * total;
            let mut acc = 0.0;
            for &k in pool {
                acc += layer.entries[k].weight;
                if u < acc {
                    return Some(k);
                }
            }
            pool.last().copied()
        })
        .collect()
}

/// The footprint `(core, spread)` of an entry at unit scale.
fn footprint(ctx: &mut NodeCtx<'_>, e: &BiomeEntry) -> (f32, f32) {
    let from_mesh = match &e.spawn {
        EntrySpawn::Mesh(p) => ctx.world().mesh(p).map(|m| {
            let (lo, hi) = m.bounds();
            0.5 * (hi[0] - lo[0]).max(hi[2] - lo[2])
        }),
        _ => None,
    };
    let spread = match (e.footprint.spread_m, from_mesh) {
        (Some(s), _) => s,
        (None, Some(s)) => s,
        (None, None) => {
            ctx.warn(
                "biome_footprint_default",
                format!(
                    "entry `{}`: no mesh bounds; spread_m defaults to {DEFAULT_SPREAD_M} m",
                    e.name
                ),
            );
            DEFAULT_SPREAD_M
        }
    };
    let core = e.footprint.core_m.unwrap_or(0.25 * spread);
    (core.max(0.0), spread.max(0.0))
}

/// Stage 4: rotation, scale and sink of a point (spec 65 § 3.4).
fn place(c: &mut Cand, e: &BiomeEntry, normal: [f32; 3], s4: u32) {
    let pl = &e.placement;
    // Tilt: normal alignment blended toward world up by `upright` (normalized lerp).
    let qn = rotation_between(Vec3::Y, Vec3::from(normal)).to_array();
    let u = pl.upright.clamp(0.0, 1.0) as f32;
    let mut q = [
        qn[0] * (1.0 - u),
        qn[1] * (1.0 - u),
        qn[2] * (1.0 - u),
        qn[3] * (1.0 - u) + u,
    ];
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if len > 1e-12 {
        q = q.map(|v| v / len);
    } else {
        q = [0.0, 0.0, 0.0, 1.0];
    }
    let yaw = match pl.yaw {
        Yaw::Random => draw(c.seed, s4, 0) * std::f64::consts::TAU,
        Yaw::Fixed(deg) => deg.to_radians(),
        Yaw::AlongSlope => {
            if normal[0].abs() + normal[2].abs() > 1e-6 {
                detmath::atan2(f64::from(normal[0]), f64::from(normal[2]))
            } else {
                0.0
            }
        }
    };
    let (sy, cy) = detmath::sin_cos(yaw * 0.5);
    let qy = Quat::from_xyzw(0.0, sy as f32, 0.0, cy as f32);
    let rot = (Quat(q) * qy).normalize();
    c.rot = rot.to_array();
    let draw_scale = |lo: f64, hi: f64| (lo + (hi - lo) * draw(c.seed, s4, 3)) as f32;
    let s = match pl.scale {
        ScaleRule::Keep => None,
        ScaleRule::Range(lo, hi) => Some(draw_scale(lo, hi)),
        ScaleRule::Fixed(v) => Some(v as f32),
    };
    if let Some(s) = s {
        c.scale = if pl.scale_from_source {
            c.scale.map(|v| v * s)
        } else {
            [s; 3]
        };
    }
    if pl.sink_m != 0.0 {
        let up = rot * Vec3::Y;
        let d = pl.sink_m as f32;
        c.pos = [c.pos[0] - up.x * d, c.pos[1] - up.y * d, c.pos[2] - up.z * d];
    }
}

fn uniform_scale(s: [f32; 3]) -> f32 {
    s[0].abs().max(s[2].abs())
}

/// What a layer run reports per entry.
#[derive(Default)]
struct EntryCounts {
    assigned: u64,
    filtered: Vec<u64>,
    realized_out: u64,
    spaced_out: u64,
    cleared_by_parent: u64,
    children: u64,
}

/// Runs stages 3-6 (pick done) on `cands` of one entry set: placement, filters, realize. Returns
/// the survivors.
fn place_filter_realize(
    ctx: &mut NodeCtx<'_>,
    job: &mut LayerJob<'_>,
    src_item: &DataItem,
    cands: Vec<Cand>,
    look: &mut Lookups,
    counts: &mut [EntryCounts],
    feet: &[(f32, f32)],
) -> Result<Vec<Cand>, NodeError> {
    let s4 = stage_seed(ctx, STAGE_PLACE);
    let s6 = stage_seed(ctx, STAGE_REALIZE);
    let layer = job.layer;
    let mut alive: Vec<Cand> = Vec::with_capacity(cands.len());
    // Group per entry for Graph filters, which run on point sets.
    let mut by_entry: BTreeMap<usize, Vec<Cand>> = BTreeMap::new();
    for c in cands {
        by_entry.entry(c.entry).or_default().push(c);
    }
    for (k, mut list) in by_entry {
        ctx.check_cancel()?;
        let e = &layer.entries[k];
        // Stage 4 per candidate, in element chunks on the pool (ADR-7252).
        let (ground, foot) = (job.ground.as_deref(), feet[k]);
        ctx.for_each_job(list.chunks_mut(CHUNK).collect(), |part: &mut [Cand]| {
            for c in part {
                let normal = ground.map_or([0.0, 1.0, 0.0], |g| g.normal(c.pos[0], c.pos[2]));
                place(c, e, normal, s4);
                let us = uniform_scale(c.scale);
                c.core = foot.0 * us;
                c.spread = foot.1 * us;
            }
        })?;
        if !e.filters_off {
            for (fi, f) in e.filters.iter().enumerate() {
                let (lo, hi, feather) = f.window();
                let values: Vec<f64> = match f {
                    EntryFilter::Graph { graph, attr, .. } => graph_filter_values(ctx, graph, attr, &list)?,
                    _ => list
                        .iter()
                        .map(|c| filter_value(ctx, job, src_item, look, f, c))
                        .collect(),
                };
                let mut removed = 0u64;
                let mut kept = Vec::with_capacity(list.len());
                for (mut c, v) in list.into_iter().zip(values) {
                    let factor = window_factor(v, lo, hi, feather);
                    if factor <= 0.0 {
                        removed += 1;
                        if let Some(out) = job.filtered.as_deref_mut() {
                            out.push(FilteredOut {
                                pos: c.pos,
                                entry: e.name.clone(),
                                filter: format!("{fi}:{}", f.kind_name()),
                            });
                        }
                        continue;
                    }
                    c.density *= factor as f32;
                    kept.push(c);
                }
                if let Some(slot) = counts[k].filtered.get_mut(fi) {
                    *slot += removed;
                }
                list = kept;
            }
        }
        // Stage 6: realize. The second claim at a spot keeps the complement of the owner's draw.
        for c in list {
            let u = draw(c.seed, s6, 0);
            let u = if c.second { 1.0 - u } else { u };
            if u < f64::from(c.density) {
                alive.push(Cand { density: 1.0, ..c });
            } else {
                counts[k].realized_out += 1;
            }
        }
    }
    Ok(alive)
}

fn filter_value(
    ctx: &mut NodeCtx<'_>,
    job: &mut LayerJob<'_>,
    src_item: &DataItem,
    look: &mut Lookups,
    f: &EntryFilter,
    c: &Cand,
) -> f64 {
    let [x, y, z] = c.pos;
    match f {
        EntryFilter::Altitude { .. } => f64::from(y),
        EntryFilter::Slope { .. } => {
            slope_deg(job.ground.as_ref().map_or([0.0, 1.0, 0.0], |g| g.normal(x, z)))
        }
        EntryFilter::Density { .. } => f64::from(c.density),
        EntryFilter::WaterDistance { .. } => {
            let water = job.water.get_or_init(|| match (&job.ground, &job.world) {
                (Some(g), Some(w)) => WaterField::build(g, &w.water),
                _ => None,
            });
            match water {
                Some(w) => f64::from(w.at(x, z)),
                None => f64::INFINITY,
            }
        }
        EntryFilter::TerrainLayer { name, min, .. } => {
            match look
                .layer_index(job.world.as_deref(), name)
                .and_then(|i| layer_weight(job, i, x, y, z))
            {
                Some(w) => f64::from(w),
                None => {
                    look.warn_missing_layer(ctx, name, "the filter passes");
                    *min
                }
            }
        }
        EntryFilter::Mask { asset, channel, .. } => {
            look.mask(ctx, asset, channel, x, z).map_or(0.0, f64::from)
        }
        // Children have no source attributes; they pass attribute filters.
        EntryFilter::Attr { sel, min, .. } => match c.src {
            Some(i) => look.attr(src_item, sel, Some(i)).unwrap_or(f64::NAN),
            None => *min,
        },
        EntryFilter::Graph { .. } => f64::NAN,
    }
}

/// Runs a filter graph on a point set (input pin `in`) and reads `attr` from its `out` points.
fn graph_filter_values(
    ctx: &mut NodeCtx<'_>,
    graph: &str,
    attr: &str,
    list: &[Cand],
) -> Result<Vec<f64>, NodeError> {
    let set = to_points(list);
    let mut inputs = BTreeMap::new();
    inputs.insert("in".to_string(), Bundle::one(DataItem::points(set)));
    let r = ctx.run_subgraph(graph, &BTreeMap::new(), inputs, None)?;
    let item = r
        .output
        .graph_outputs
        .get("out")
        .and_then(|b| b.0.first().cloned())
        .ok_or_else(|| {
            NodeError::new(
                "biome_invalid",
                format!("filter graph `{graph}` has no `out` points"),
            )
        })?;
    if item.payload.len() != list.len() {
        return Err(NodeError::new(
            "biome_invalid",
            format!(
                "filter graph `{graph}` must keep its points ({} in, {} out)",
                list.len(),
                item.payload.len()
            ),
        ));
    }
    let sel = crate::data::Selector::parse(attr).map_err(|m| NodeError::new("setting_type", m))?;
    let col = read_num(&item, &sel)?;
    Ok((0..list.len()).map(|i| col.scalar(i)).collect())
}

fn to_points(list: &[Cand]) -> PointSet {
    let mut set = PointSet::new();
    for c in list {
        set.push(Point {
            position: c.pos,
            rotation: c.rot,
            scale: c.scale,
            density: c.density,
            seed: c.seed,
            ..Point::default()
        });
    }
    set
}

/// A multiply-rotate hasher for integer cell keys (the FxHash scheme). `CircleHash` only probes
/// its map and never iterates it, so the hasher cannot change any output; it replaces the default
/// SipHash, whose per-key cost dominated the resolve pass (ADR-7252).
#[derive(Default)]
struct CellHasher(u64);

impl std::hash::Hasher for CellHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    fn write_i64(&mut self, v: i64) {
        self.write_u64(v as u64);
    }
}

/// A spatial hash of accepted circles (cell = 2 × the largest radius).
struct CircleHash {
    cell: f32,
    cells: HashMap<(i64, i64), Vec<usize>, std::hash::BuildHasherDefault<CellHasher>>,
}

impl CircleHash {
    fn new(max_r: f32) -> Self {
        Self {
            cell: (2.0 * max_r).max(1e-3),
            cells: HashMap::default(),
        }
    }
    fn key(&self, x: f32, z: f32) -> (i64, i64) {
        ((x / self.cell).floor() as i64, (z / self.cell).floor() as i64)
    }
    fn insert(&mut self, x: f32, z: f32, id: usize) {
        let k = self.key(x, z);
        self.cells.entry(k).or_default().push(id);
    }
    fn near(&self, x: f32, z: f32, mut f: impl FnMut(usize) -> bool) -> bool {
        let (kx, kz) = self.key(x, z);
        for dz in -1..=1 {
            for dx in -1..=1 {
                if let Some(v) = self.cells.get(&(kx + dx, kz + dz))
                    && v.iter().any(|&i| f(i))
                {
                    return true;
                }
            }
        }
        false
    }
}

/// Self spacing (spec 65 § 3.5): `new` points of one entry against `existing` points of the same
/// entry and each other; no two closer than the sum of their spreads. Larger spread first, ties by
/// seed, then by index. Returns the kept new points in their original order.
fn self_space(existing: &[Cand], new: Vec<Cand>) -> (Vec<Cand>, u64) {
    let max_r = existing
        .iter()
        .chain(&new)
        .map(|c| c.spread)
        .fold(0.0f32, f32::max);
    if max_r <= 0.0 {
        return (new, 0);
    }
    let mut hash = CircleHash::new(max_r);
    let mut accepted: Vec<([f32; 2], f32)> = Vec::new();
    for c in existing {
        hash.insert(c.pos[0], c.pos[2], accepted.len());
        accepted.push(([c.pos[0], c.pos[2]], c.spread));
    }
    let mut order: Vec<usize> = (0..new.len()).collect();
    order.sort_by(|&a, &b| {
        new[b]
            .spread
            .total_cmp(&new[a].spread)
            .then(new[a].seed.cmp(&new[b].seed))
            .then(a.cmp(&b))
    });
    let mut keep = vec![false; new.len()];
    for i in order {
        let c = &new[i];
        let (x, z) = (c.pos[0], c.pos[2]);
        let blocked = hash.near(x, z, |j| {
            let (p, r) = accepted[j];
            let (dx, dz) = (p[0] - x, p[1] - z);
            let s = r + c.spread;
            dx * dx + dz * dz < s * s
        });
        if !blocked {
            keep[i] = true;
            hash.insert(x, z, accepted.len());
            accepted.push(([x, z], c.spread));
        }
    }
    let mut removed = 0u64;
    let kept = new
        .into_iter()
        .zip(keep)
        .filter_map(|(c, k)| {
            if !k {
                removed += 1;
            }
            k.then_some(c)
        })
        .collect();
    (kept, removed)
}

/// One entry's self-spacing job: the entry, its new points, then the kept points and the removed
/// count.
type SpaceJob = (usize, Vec<Cand>, (Vec<Cand>, u64));

/// Spaces each entry's new points against that entry's existing points. Entries are independent, so
/// they run as jobs on the pool and are joined in entry order (ADR-7252).
fn space_by_entry(
    ctx: &NodeCtx<'_>,
    existing: &[Cand],
    new: Vec<Cand>,
    counts: &mut [EntryCounts],
) -> Result<Vec<Cand>, NodeError> {
    let mut by_entry: BTreeMap<usize, Vec<Cand>> = BTreeMap::new();
    for c in new {
        by_entry.entry(c.entry).or_default().push(c);
    }
    let mut jobs: Vec<SpaceJob> = by_entry
        .into_iter()
        .map(|(k, list)| (k, list, (Vec::new(), 0)))
        .collect();
    ctx.for_each_job(jobs.iter_mut().collect(), |job: &mut SpaceJob| {
        let k = job.0;
        let same: Vec<Cand> = existing.iter().filter(|c| c.entry == k).cloned().collect();
        job.2 = self_space(&same, std::mem::take(&mut job.1));
    })?;
    let mut out = Vec::new();
    for (k, _, (kept, removed)) in jobs {
        counts[k].spaced_out += removed;
        out.extend(kept);
    }
    Ok(out)
}

/// Stage 7 for one depth: children of `parents`. `kept` is how many points the layer already
/// holds: each parent's child count is checked against the per-item point budget together with
/// them before its positions are allocated, so an authored count cannot allocate past the budget.
fn make_children(
    ctx: &mut NodeCtx<'_>,
    job: &LayerJob<'_>,
    parents: &[Cand],
    depth: u32,
    kept: usize,
) -> Result<Vec<Cand>, NodeError> {
    let layer = job.layer;
    let base = stage_seed(ctx, STAGE_CHILDREN);
    let mut out = Vec::new();
    for (k, p) in parents.iter().enumerate() {
        if k.is_multiple_of(CHUNK) {
            ctx.check_cancel()?;
        }
        let e = &layer.entries[p.entry];
        for (r, rule) in e.children.iter().enumerate() {
            let Some(ck) = layer.entries.iter().position(|x| x.name == rule.entry) else {
                continue;
            };
            if !layer.entries[ck].enabled {
                continue;
            }
            let s7 = mix(base, r as u32);
            if draw(p.seed, s7, 0) >= rule.rate {
                continue;
            }
            let ps = if rule.scale_with_parent {
                uniform_scale(p.scale)
            } else {
                1.0
            };
            let positions: Vec<[f32; 3]> = match &rule.graph {
                Some(path) => {
                    let mut one = PointSet::new();
                    one.push(Point {
                        position: p.pos,
                        rotation: p.rot,
                        scale: p.scale,
                        seed: p.seed,
                        ..Point::default()
                    });
                    let mut inputs = BTreeMap::new();
                    inputs.insert("parent".to_string(), Bundle::one(DataItem::points(one)));
                    let res = ctx.run_subgraph(path, &BTreeMap::new(), inputs, None)?;
                    let pq = quat(p.rot);
                    let pscale = Vec3::from(p.scale);
                    res.output
                        .graph_outputs
                        .get("out")
                        .map(|b| {
                            b.0.iter()
                                .filter_map(DataItem::as_points)
                                .flat_map(|s| s.position.clone())
                                .map(|l| (Vec3::from(p.pos) + pq * (pscale * Vec3::from(l))).to_array())
                                .collect()
                        })
                        .unwrap_or_default()
                }
                None => {
                    let (lo, hi) = rule.count;
                    // `hi - lo + 1` in f64: a `(0, u32::MAX)` count must not overflow.
                    let n = (f64::from(lo)
                        + (draw(p.seed, s7, 1) * (f64::from(hi) - f64::from(lo) + 1.0)).floor())
                        as u32;
                    let n = n.min(hi);
                    ctx.check_points(kept.saturating_add(out.len()).saturating_add(n as usize))?;
                    let theta0 = draw(p.seed, s7, 2) * std::f64::consts::TAU;
                    let (r0, r1) = (rule.radius_m.0 * ps, rule.radius_m.1 * ps);
                    (0..n)
                        .map(|k| {
                            let (s, c) = detmath::sin_cos(theta0 + f64::from(k) * GOLDEN_ANGLE);
                            let rad = f64::from(r0)
                                + f64::from(r1 - r0) * ((f64::from(k) + 0.5) / f64::from(n)).sqrt();
                            [p.pos[0] + (rad * c) as f32, p.pos[1], p.pos[2] + (rad * s) as f32]
                        })
                        .collect()
                }
            };
            ctx.check_points(kept.saturating_add(out.len()).saturating_add(positions.len()))?;
            for mut pos in positions {
                if let Some(g) = &job.ground {
                    pos[1] = g.height(pos[0], pos[2]);
                }
                let (clear_r, keep) = match rule.keep_clear_of_parent {
                    KeepClear::Core => (p.core, true),
                    KeepClear::Spread => (p.spread, true),
                    KeepClear::None => (0.0, false),
                };
                out.push(Cand {
                    pos,
                    rot: [0.0, 0.0, 0.0, 1.0],
                    scale: [ps; 3],
                    density: 1.0,
                    seed: ctx.point_seed(pos),
                    entry: ck,
                    depth,
                    second: false,
                    core: 0.0,
                    spread: 0.0,
                    src: None,
                    parent: keep.then_some(([p.pos[0], p.pos[2]], clear_r)),
                });
            }
        }
    }
    Ok(out)
}

/// Whether a layer runs no subgraph (no `Graph` candidate source, filter or child rule), so its
/// `biome.layer` jobs may run on forked contexts (`NodeCtx::fork_jobs`, ADR-7252).
pub(crate) fn layer_runs_no_subgraph(layer: &BiomeLayer) -> bool {
    !matches!(layer.source, CandidateSource::Graph { .. })
        && layer.entries.iter().all(|e| {
            !e.filters.iter().any(|f| matches!(f, EntryFilter::Graph { .. }))
                && e.children.iter().all(|c| c.graph.is_none())
        })
}

/// Runs `biome.layer` for one claim and one layer; returns the item (points with the § 5
/// attributes) and records counters.
pub(crate) fn run_layer(ctx: &mut NodeCtx<'_>, job: &mut LayerJob<'_>) -> Result<DataItem, NodeError> {
    let layer = job.layer;
    let def = job.def;
    let lpath = format!("{}/{}", def.name, layer.name);
    let (src, second) = candidates(ctx, job)?;
    let src_item = DataItem::points(src.clone());
    ctx.count(&format!("{lpath}|candidates"), src.len() as u64);
    let mut look = Lookups::default();
    let channels = split_channels(ctx, job, &src_item, &src, &mut look);
    let picks = pick_entries(ctx, layer, &src.seed, &channels);
    let feet: Vec<(f32, f32)> = layer.entries.iter().map(|e| footprint(ctx, e)).collect();
    let mut counts: Vec<EntryCounts> = layer
        .entries
        .iter()
        .map(|e| EntryCounts {
            filtered: vec![0; e.filters.len()],
            ..EntryCounts::default()
        })
        .collect();
    let mut cands = Vec::new();
    let mut unassigned = 0u64;
    for (i, pick) in picks.iter().enumerate() {
        let Some(k) = *pick else {
            unassigned += 1;
            continue;
        };
        counts[k].assigned += 1;
        cands.push(Cand {
            pos: src.position[i],
            rot: src.rotation[i],
            scale: src.scale[i],
            density: src.density[i],
            seed: src.seed[i],
            entry: k,
            depth: 0,
            second: second.get(i).copied().unwrap_or(false),
            core: 0.0,
            spread: 0.0,
            src: Some(i),
            parent: None,
        });
    }
    ctx.count(&format!("{lpath}|unassigned"), unassigned);
    let alive = place_filter_realize(ctx, job, &src_item, cands, &mut look, &mut counts, &feet)?;
    let mut all = space_by_entry(ctx, &[], alive, &mut counts)?;
    // Children, depth by depth.
    let mut frontier: Vec<Cand> = all.clone();
    for depth in 1..=layer.max_child_depth {
        if frontier.is_empty() {
            break;
        }
        ctx.check_cancel()?;
        let kids = make_children(ctx, job, &frontier, depth, all.len())?;
        if kids.is_empty() {
            break;
        }
        let kids = place_filter_realize(ctx, job, &src_item, kids, &mut look, &mut counts, &feet)?;
        // Keep clear of the parent.
        let kids: Vec<Cand> = kids
            .into_iter()
            .filter(|c| match c.parent {
                Some((pp, r)) => {
                    let (dx, dz) = (c.pos[0] - pp[0], c.pos[2] - pp[1]);
                    let clear = dx * dx + dz * dz >= r * r;
                    if !clear {
                        counts[c.entry].cleared_by_parent += 1;
                    }
                    clear
                }
                None => true,
            })
            .collect();
        let kids = space_by_entry(ctx, &all, kids, &mut counts)?;
        for c in &kids {
            counts[c.entry].children += 1;
        }
        all.extend(kids.iter().cloned());
        ctx.check_points(all.len())?;
        frontier = kids;
    }
    for (k, e) in layer.entries.iter().enumerate() {
        let p = entry_path(&def.name, &layer.name, &e.name);
        let c = &counts[k];
        ctx.count(&format!("{p}|assigned"), c.assigned);
        for (fi, f) in e.filters.iter().enumerate() {
            ctx.count(&format!("{p}|filter:{fi}:{}", f.kind_name()), c.filtered[fi]);
        }
        ctx.count(&format!("{p}|realized_out"), c.realized_out);
        ctx.count(&format!("{p}|spaced_out"), c.spaced_out + c.cleared_by_parent);
        ctx.count(&format!("{p}|children"), c.children);
    }
    // Output points in a stable order: depth, then creation order.
    let entry_names: Vec<String> = layer
        .entries
        .iter()
        .map(|e| entry_path(&def.name, &layer.name, &e.name))
        .collect();
    let sharing_name = match layer.sharing {
        Sharing::Exclusive => "Exclusive",
        Sharing::Shared => "Shared",
    };
    let mut set = PointSet::new();
    let mut names = Vec::with_capacity(all.len());
    let mut idx = Vec::with_capacity(all.len());
    let mut ranks = Vec::with_capacity(all.len());
    let mut sharing = Vec::with_capacity(all.len());
    let mut cores = Vec::with_capacity(all.len());
    let mut spreads = Vec::with_capacity(all.len());
    let mut depths = Vec::with_capacity(all.len());
    for c in &all {
        set.push(Point {
            position: c.pos,
            rotation: c.rot,
            scale: c.scale,
            density: 1.0,
            seed: c.seed,
            ..Point::default()
        });
        names.push(entry_names[c.entry].clone());
        idx.push(c.entry as i64);
        ranks.push(i64::from(layer.rank));
        sharing.push(sharing_name.to_string());
        cores.push(f64::from(c.core));
        spreads.push(f64::from(c.spread));
        depths.push(i64::from(c.depth));
    }
    set.attrs.insert("entry".into(), AttrColumn::Name(names.into()));
    set.attrs
        .insert("entry_index".into(), AttrColumn::Int(idx.into()));
    set.attrs
        .insert("layer_rank".into(), AttrColumn::Int(ranks.into()));
    set.attrs
        .insert("sharing".into(), AttrColumn::Name(sharing.into()));
    set.attrs.insert("core_m".into(), AttrColumn::Float(cores.into()));
    set.attrs
        .insert("spread_m".into(), AttrColumn::Float(spreads.into()));
    set.attrs.insert("depth".into(), AttrColumn::Int(depths.into()));
    let mut item = job.claim.with_payload(Payload::Points(set));
    item.scope
        .insert("layer".into(), AttrValue::Name(layer.name.clone()));
    item.last_written = Some("entry".into());
    Ok(item)
}

/// The resolve sort key: owner priority desc, layer rank desc, depth, entry index, seed, point index.
type ResolveKey = (
    std::cmp::Reverse<i64>,
    std::cmp::Reverse<i64>,
    i64,
    i64,
    u32,
    usize,
);

/// One point in the resolve.
struct ResolvePoint {
    item: usize,
    index: usize,
    x: f32,
    z: f32,
    core: f32,
    rank: (i64, i64),
    shared: bool,
}

/// An integer attribute column of a point set (0 where it is missing or not an integer column).
struct IntAttr<'a>(Option<&'a AttrColumn>);

impl<'a> IntAttr<'a> {
    fn of(p: &'a PointSet, name: &str) -> Self {
        Self(p.attrs.get(name))
    }

    fn at(&self, i: usize) -> i64 {
        match self.0 {
            Some(AttrColumn::Int(v)) => v.get(i).copied().unwrap_or(0),
            _ => 0,
        }
    }
}

/// A number attribute value (integers widened; 0 where missing or not numeric).
fn float_at(col: Option<&AttrColumn>, i: usize) -> f64 {
    match col {
        Some(AttrColumn::Float(v)) => v.get(i).copied().unwrap_or(0.0),
        Some(AttrColumn::Int(v)) => v.get(i).map_or(0.0, |x| *x as f64),
        _ => 0.0,
    }
}

/// The cross-layer resolve (spec 65 § 6.4) over every item's points; returns per item the kept mask
/// and the removed count per entry name.
pub(crate) fn resolve(items: &[DataItem]) -> (Vec<Vec<bool>>, BTreeMap<String, u64>) {
    let mut pts: Vec<ResolvePoint> = Vec::new();
    let mut keys: Vec<ResolveKey> = Vec::new();
    let mut keep: Vec<Vec<bool>> = Vec::with_capacity(items.len());
    for (ii, item) in items.iter().enumerate() {
        let Some(p) = item.as_points() else {
            keep.push(Vec::new());
            continue;
        };
        keep.push(vec![true; p.len()]);
        let priority = match item.scope.get("priority") {
            Some(AttrValue::Int(v)) => *v,
            _ => 0,
        };
        // Each column looked up once per item, not per point.
        let (ranks, depths, entries) = (
            IntAttr::of(p, "layer_rank"),
            IntAttr::of(p, "depth"),
            IntAttr::of(p, "entry_index"),
        );
        let cores = p.attrs.get("core_m");
        let sharing = match p.attrs.get("sharing") {
            Some(AttrColumn::Name(v)) => Some(v),
            _ => None,
        };
        keys.reserve(p.len());
        pts.reserve(p.len());
        for i in 0..p.len() {
            let rank = (priority, ranks.at(i));
            let shared = sharing.is_some_and(|v| v.get(i).is_some_and(|s| s == "Shared"));
            keys.push((
                std::cmp::Reverse(rank.0),
                std::cmp::Reverse(rank.1),
                depths.at(i),
                entries.at(i),
                p.seed[i],
                pts.len(),
            ));
            pts.push(ResolvePoint {
                item: ii,
                index: i,
                x: p.position[i][0],
                z: p.position[i][2],
                core: float_at(cores, i) as f32,
                rank,
                shared,
            });
        }
    }
    // Keys are unique (the last field is the point's index), so an unstable sort gives the same order.
    keys.sort_unstable();
    let max_core = pts.iter().map(|p| p.core).fold(0.0f32, f32::max);
    let mut hash = CircleHash::new(max_core);
    let mut removed: BTreeMap<String, u64> = BTreeMap::new();
    for k in keys {
        let p = &pts[k.5];
        if !p.shared && p.core > 0.0 {
            let blocked = hash.near(p.x, p.z, |j| {
                let q = &pts[j];
                let (dx, dz) = (q.x - p.x, q.z - p.z);
                let s = q.core + p.core;
                q.rank > p.rank && dx * dx + dz * dz < s * s
            });
            if blocked {
                keep[p.item][p.index] = false;
                let name = items[p.item]
                    .as_points()
                    .and_then(|s| match s.attrs.get("entry") {
                        Some(AttrColumn::Name(v)) => v.get(p.index).cloned(),
                        _ => None,
                    })
                    .unwrap_or_default();
                *removed.entry(name).or_default() += 1;
                continue;
            }
        }
        if !p.shared && p.core > 0.0 {
            hash.insert(p.x, p.z, k.5);
        }
    }
    (keep, removed)
}
