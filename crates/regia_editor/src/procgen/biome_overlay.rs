// SPDX-License-Identifier: MPL-2.0
//! The biome debug overlay views (spec 65 § 7, spec 67 § 3), next to the area outlines that
//! [`super::view::biome_lines`] draws:
//!
//! - **tint**: the claim raster, each cell colored by the biome that owns it (its `color`, alpha by
//!   the owner's weight);
//! - **bands**: the cells inside a blend band (a second claim shares the spot) outlined;
//! - **footprints**: each placed point's core and spread circles;
//! - **filter view**: the candidates each entry filter removed, dim, with the filter name for hover
//!   (`editor.procgen_biome_overlay` with `at`).
//!
//! Everything that decides what to draw is a pure function of the captured biome world or the
//! preview run's debug outputs, so the tests run headless; [`overlay_lines`] turns the result into
//! debug lines. Tint and bands drape on the terrain under each cell (post05w4: the cell's center
//! and corners take the ground height, plus a small lift); where no terrain lies under a cell it
//! is drawn at the owning area's height (a shape area's center height, else 0).

use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_procgen::biome::ecs::BiomeArea;
use regia_procgen::biome::{AreaOutline, BiomeWorld};
use regia_procgen::data::{AttrColumn, DataItem};
use regia_procgen::world::DebugOutputs;
use regia_scene::Transform;
use regia_scene::shape::Shape;
use regia_scene::spline::Spline;

use super::view::Line;

/// Most claim cells the tint and band views sample (the cell grows until the areas fit).
pub const MAX_CLAIM_CELLS: usize = 16_384;
/// Most footprints drawn; more are drawn as a uniform subsample.
pub const MAX_FOOTPRINTS: usize = 4_096;
/// Most filtered-out candidates drawn; more are drawn as a uniform subsample (hover still finds
/// every one).
pub const MAX_FILTERED: usize = 50_000;
/// Blend-band cell outlines.
pub const BAND_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.9];
/// Footprint core circles.
pub const CORE_COLOR: [f32; 4] = [1.0, 0.55, 0.1, 1.0];
/// Footprint spread circles.
pub const SPREAD_COLOR: [f32; 4] = [1.0, 0.85, 0.4, 0.45];
/// Candidates a filter removed (dim).
pub const FILTERED_COLOR: [f32; 4] = [0.45, 0.45, 0.5, 0.6];
/// Lift above the reference height so the overlay does not z-fight the ground, meters.
const LIFT_M: f32 = 0.05;
/// Segments of a footprint circle.
const FOOTPRINT_SEGMENTS: usize = 16;

/// Which overlay views draw (each off by default; the area outlines are
/// `ProcPreview::biome_overlay`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BiomeViews {
    /// Claims tinted by biome color.
    pub tint: bool,
    /// Blend bands (cells a second claim shares).
    pub bands: bool,
    /// Entry footprints (core and spread circles) of the placed points.
    pub footprints: bool,
    /// Candidates removed by each entry filter.
    pub filtered: bool,
}

impl BiomeViews {
    /// Whether any view draws.
    #[must_use]
    pub fn any(&self) -> bool {
        self.tint || self.bands || self.footprints || self.filtered
    }
}

/// One cell of the claim raster.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimCell {
    /// Cell center (y: the ground under it, else the owning area's height; lifted).
    pub center: [f32; 3],
    /// Heights of the corners `(-x, -z)`, `(+x, -z)`, `(+x, +z)`, `(-x, +z)` (lifted), so the drawn
    /// cell follows the ground ([`drape`]); the center's height before draping.
    pub corner_y: [f32; 4],
    /// The center lies on a terrain (draped); false: drawn at the area height.
    pub draped: bool,
    /// Half the cell side, meters.
    pub half_m: f32,
    /// The owning biome's name.
    pub biome: String,
    /// The owning biome's color, alpha by the owner's weight.
    pub color: [f32; 4],
    /// A second claim shares the cell (it lies in a blend band).
    pub band: bool,
}

/// One placed point's footprint.
#[derive(Clone, Debug, PartialEq)]
pub struct Footprint {
    /// Point position.
    pub center: [f32; 3],
    /// Core radius, meters.
    pub core_m: f32,
    /// Spread radius, meters.
    pub spread_m: f32,
    /// The entry name.
    pub entry: String,
}

/// One candidate an entry filter removed.
#[derive(Clone, Debug, PartialEq)]
pub struct FilteredMark {
    /// Where it stood.
    pub position: [f32; 3],
    /// The entry name.
    pub entry: String,
    /// The filter, `<index>:<kind>` (the run report's counter suffix).
    pub filter: String,
}

/// The reference height of an area outline: a shape's center height, else 0.
fn outline_height(outline: &AreaOutline) -> f32 {
    match outline {
        AreaOutline::Shape(s) => s.position[1],
        _ => 0.0,
    }
}

/// The claim raster over the union of the areas' bounds: square cells (at most
/// [`MAX_CLAIM_CELLS`]), each resolved at its center with the generator's own claim rule
/// (`BiomeWorld::claims_at`). Cells nobody claims are left out.
#[must_use]
pub fn claim_cells(biomes: &BiomeWorld) -> Vec<ClaimCell> {
    let Some(rect) = biomes
        .areas
        .iter()
        .map(|a| a.outline.xz_bounds())
        .filter(|b| b.iter().all(|v| v.is_finite()) && b[0] <= b[2] && b[1] <= b[3])
        .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
    else {
        return Vec::new();
    };
    let (w_m, h_m) = ((rect[2] - rect[0]).max(1e-3), (rect[3] - rect[1]).max(1e-3));
    // The smallest cell with at most MAX_CLAIM_CELLS cells over the rectangle.
    let mut cell = (w_m * h_m / MAX_CLAIM_CELLS as f32).sqrt().max(0.25);
    let dims = |cell: f32| {
        (
            (w_m / cell).ceil().max(1.0) as usize,
            (h_m / cell).ceil().max(1.0) as usize,
        )
    };
    while dims(cell).0.saturating_mul(dims(cell).1) > MAX_CLAIM_CELLS {
        cell *= 1.05;
    }
    let (nx, nz) = dims(cell);
    let mut out = Vec::new();
    for j in 0..nz {
        let z = rect[1] + (j as f32 + 0.5) * cell;
        for i in 0..nx {
            let x = rect[0] + (i as f32 + 0.5) * cell;
            let claim = biomes.claims_at(x, z);
            let Some(owner) = claim.owner else {
                continue;
            };
            let Some(area) = biomes.areas.get(owner.area) else {
                continue;
            };
            let Some((_, def)) = area.biomes.get(owner.biome) else {
                continue;
            };
            let (r, g, b) = def.color;
            let y = outline_height(&area.outline) + LIFT_M;
            out.push(ClaimCell {
                center: [x, y, z],
                corner_y: [y; 4],
                draped: false,
                half_m: cell * 0.5,
                biome: def.name.clone(),
                color: [r, g, b, owner.weight.clamp(0.0, 1.0) * 0.8],
                band: claim.second.is_some(),
            });
        }
    }
    out
}

/// Drapes `cells` on the ground: each cell's center and corners take `ground(x, z)` plus the lift
/// where it answers (a terrain under the point); the others keep the area height.
pub fn drape(cells: &mut [ClaimCell], ground: impl Fn(f32, f32) -> Option<f32>) {
    for c in cells {
        let [x, _, z] = c.center;
        let h = c.half_m * 0.9;
        if let Some(g) = ground(x, z).filter(|g| g.is_finite()) {
            c.center[1] = g + LIFT_M;
            c.draped = true;
        }
        let corners = [[x - h, z - h], [x + h, z - h], [x + h, z + h], [x - h, z + h]];
        for (y, [cx, cz]) in c.corner_y.iter_mut().zip(corners) {
            if let Some(g) = ground(cx, cz).filter(|g| g.is_finite()) {
                *y = g + LIFT_M;
            }
        }
    }
}

/// The ground height at world `(x, z)` over the world's terrains: the highest terrain whose square
/// covers the point (the heightfield's bilinear surface, as the terrain pick marches it, holes
/// ignored); `None` off every terrain.
fn terrain_ground(world: &mut World) -> impl Fn(f32, f32) -> Option<f32> + '_ {
    let offset = world
        .get_resource::<regia_scene::streaming::OriginOffset>()
        .map_or([0.0f32; 3], |o| o.0.map(|axis| axis as f32));
    let mut query = world.query::<&regia_terrain::Terrain>();
    let maps: Vec<&regia_terrain::TerrainMap> = query.iter(world).map(|t| &t.map).collect();
    move |x, z| {
        let (wx, wz) = (x + offset[0], z + offset[2]);
        maps.iter()
            .filter(|m| {
                let half = m.world_width() * 0.5;
                wx.abs() <= half && wz.abs() <= half
            })
            .map(|m| m.sample_bilinear(wx, wz) - offset[1])
            .reduce(f32::max)
    }
}

/// A float column as `f32` per point (absent: `None`).
fn floats(item: &DataItem, name: &str) -> Option<Vec<f32>> {
    match item.as_points()?.attrs.get(name)? {
        AttrColumn::Float(v) => Some(v.iter().map(|x| *x as f32).collect()),
        _ => None,
    }
}

/// A name column per point (absent: empty names).
fn names(item: &DataItem, name: &str, len: usize) -> Vec<String> {
    match item.as_points().and_then(|p| p.attrs.get(name)) {
        Some(AttrColumn::Name(v)) => v.to_vec(),
        _ => vec![String::new(); len],
    }
}

/// Footprints of the placed points: the first output of the last node in `order` (the run
/// report's evaluation order, `(id, type)`) whose points carry `core_m` and `spread_m` (the end
/// of the biome chain: `biome.layer`, `biome.resolve`, `biome.exclude`, ...). Returns the drawn
/// footprints (a uniform subsample over [`MAX_FOOTPRINTS`]) and the total.
#[must_use]
pub fn footprints(debug: &DebugOutputs, order: &[(u32, String)]) -> (Vec<Footprint>, usize) {
    let carries = |id: &u32| {
        debug.get(id).and_then(|pins| pins.first()).is_some_and(|b| {
            b.0.iter()
                .any(|i| floats(i, "core_m").is_some() && floats(i, "spread_m").is_some())
        })
    };
    let Some(bundle) = order
        .iter()
        .rev()
        .map(|(id, _)| id)
        .find(|id| carries(id))
        .and_then(|id| debug.get(id))
        .and_then(|pins| pins.first())
    else {
        return (Vec::new(), 0);
    };
    let mut all = Vec::new();
    for item in &bundle.0 {
        let (Some(p), Some(core), Some(spread)) =
            (item.as_points(), floats(item, "core_m"), floats(item, "spread_m"))
        else {
            continue;
        };
        let entries = names(item, "entry", p.len());
        for (i, pos) in p.position.iter().enumerate() {
            all.push(Footprint {
                center: *pos,
                core_m: core.get(i).copied().unwrap_or(0.0),
                spread_m: spread.get(i).copied().unwrap_or(0.0),
                entry: entries.get(i).cloned().unwrap_or_default(),
            });
        }
    }
    let total = all.len();
    (subsample(all, MAX_FOOTPRINTS), total)
}

/// The candidates entry filters removed: the `filtered` output (second pin) of every
/// `biome.layer` node in `order`, in order. Every mark (hover reads them all).
#[must_use]
pub fn filtered_marks(debug: &DebugOutputs, order: &[(u32, String)]) -> Vec<FilteredMark> {
    let mut out = Vec::new();
    for (id, ty) in order {
        if ty != "biome.layer" {
            continue;
        }
        let Some(bundle) = debug.get(id).and_then(|pins| pins.get(1)) else {
            continue;
        };
        for item in &bundle.0 {
            let Some(p) = item.as_points() else {
                continue;
            };
            let entries = names(item, "entry", p.len());
            let filters = names(item, "filter", p.len());
            for (i, pos) in p.position.iter().enumerate() {
                out.push(FilteredMark {
                    position: *pos,
                    entry: entries.get(i).cloned().unwrap_or_default(),
                    filter: filters.get(i).cloned().unwrap_or_default(),
                });
            }
        }
    }
    out
}

/// The hover answer of the filter view: the mark nearest to `(x, z)` in XZ within `radius_m`
/// (ties: the earlier mark).
#[must_use]
pub fn filtered_at(marks: &[FilteredMark], x: f32, z: f32, radius_m: f32) -> Option<&FilteredMark> {
    let mut best: Option<(f32, &FilteredMark)> = None;
    for m in marks {
        let (dx, dz) = (m.position[0] - x, m.position[2] - z);
        let d2 = dx * dx + dz * dz;
        if d2 <= radius_m * radius_m && best.is_none_or(|(b, _)| d2 < b) {
            best = Some((d2, m));
        }
    }
    best.map(|(_, m)| m)
}

/// The mark nearest the cursor ray within `radius_m` (ties: the earlier mark) — the filter view's
/// cursor tooltip (spec 65 § 7), the pointer-side twin of [`filtered_at`].
#[must_use]
pub fn filtered_on_ray<'a>(
    marks: &'a [FilteredMark],
    origin: [f32; 3],
    dir: [f32; 3],
    radius_m: f32,
) -> Option<&'a FilteredMark> {
    let mut best: Option<(f32, &'a FilteredMark)> = None;
    for m in marks {
        let d = super::view::ray_distance(
            regia_core::math::Vec3::from_array(origin),
            regia_core::math::Vec3::from_array(dir),
            regia_core::math::Vec3::from_array(m.position),
        );
        if d <= radius_m && best.as_ref().is_none_or(|(b, _)| d < *b) {
            best = Some((d, m));
        }
    }
    best.map(|(_, m)| m)
}

/// The cursor tooltip text of a filtered mark: `<entry> · removed by <filter>`.
#[must_use]
pub fn filter_tooltip_text(mark: &FilteredMark) -> String {
    format!("{} · removed by {}", mark.entry, mark.filter)
}

/// Every `step`-th element so at most `cap` remain (a uniform subsample, first element kept).
fn subsample<T>(all: Vec<T>, cap: usize) -> Vec<T> {
    if all.len() <= cap || cap == 0 {
        return all;
    }
    let step = all.len().div_ceil(cap);
    all.into_iter().step_by(step).collect()
}

/// A flat circle around `c`.
fn ring(c: [f32; 3], r: f32, color: [f32; 4], out: &mut Vec<Line>) {
    if !r.is_finite() || r <= 0.0 || !c.iter().all(|v| v.is_finite()) {
        return;
    }
    let at = |k: usize| {
        let a = std::f32::consts::TAU * k as f32 / FOOTPRINT_SEGMENTS as f32;
        let (s, co) = a.sin_cos();
        [c[0] + co * r, c[1] + LIFT_M, c[2] + s * r]
    };
    for k in 0..FOOTPRINT_SEGMENTS {
        out.push((at(k), at(k + 1), color));
    }
}

/// The debug lines of the views that are on: tint as a cross per cell in the biome color, bands
/// as a cell outline, footprints as core and spread circles, filtered candidates as small dim
/// crosses.
#[must_use]
pub fn overlay_lines(
    views: BiomeViews,
    cells: &[ClaimCell],
    feet: &[Footprint],
    marks: &[FilteredMark],
) -> Vec<Line> {
    let mut out = Vec::new();
    for c in cells {
        let [x, _, z] = c.center;
        let h = c.half_m * 0.9;
        let [y0, y1, y2, y3] = c.corner_y;
        if views.tint {
            out.push(([x - h, y0, z - h], [x + h, y2, z + h], c.color));
            out.push(([x - h, y3, z + h], [x + h, y1, z - h], c.color));
        }
        if views.bands && c.band {
            let k = [
                [x - h, y0, z - h],
                [x + h, y1, z - h],
                [x + h, y2, z + h],
                [x - h, y3, z + h],
            ];
            for i in 0..4 {
                out.push((k[i], k[(i + 1) % 4], BAND_COLOR));
            }
        }
    }
    if views.footprints {
        for f in feet {
            ring(f.center, f.core_m, CORE_COLOR, &mut out);
            ring(f.center, f.spread_m, SPREAD_COLOR, &mut out);
        }
    }
    if views.filtered {
        let h = 0.1;
        for m in subsample(marks.iter().collect(), MAX_FILTERED) {
            let [x, y, z] = m.position;
            if ![x, y, z].iter().all(|v| v.is_finite()) {
                continue;
            }
            let y = y + LIFT_M;
            out.push(([x - h, y, z], [x + h, y, z], FILTERED_COLOR));
            out.push(([x, y, z - h], [x, y, z + h], FILTERED_COLOR));
        }
    }
    out
}

/// One biome area as the overlay cache keys it: the entity, the area, its transform, shape and
/// spline.
type AreaKey = (Entity, BiomeArea, Transform, Option<Shape>, Option<Spline>);

/// The overlay's world-side cache: the claim raster is rebuilt only when an area, its transform,
/// shape or spline, or a terrain (its heights, by change tick), changes (or the views are switched
/// on), not every tick.
#[derive(Resource, Default)]
pub struct BiomeOverlayCache {
    key: Vec<AreaKey>,
    terrain_key: Vec<(Entity, u32)>,
    cells: Arc<Vec<ClaimCell>>,
    built: bool,
    /// Times the claim raster was rebuilt (tests: a quiet tick does not rebuild).
    pub rebuilds: u64,
}

/// The claim raster of the level's biome areas, from the cache when nothing changed.
pub fn cached_claim_cells(world: &mut World) -> Arc<Vec<ClaimCell>> {
    let mut key: Vec<AreaKey> = world
        .query::<(
            Entity,
            &BiomeArea,
            Option<&Transform>,
            Option<&Shape>,
            Option<&Spline>,
        )>()
        .iter(world)
        .map(|(e, a, t, s, sp)| {
            (
                e,
                a.clone(),
                t.copied().unwrap_or_default(),
                s.cloned(),
                sp.cloned(),
            )
        })
        .collect();
    key.sort_by_key(|k| k.0);
    let mut terrain_key: Vec<(Entity, u32)> = world
        .query::<(Entity, Ref<regia_terrain::Terrain>)>()
        .iter(world)
        .map(|(e, t)| (e, t.last_changed().get()))
        .collect();
    terrain_key.sort_by_key(|k| k.0);
    if let Some(cache) = world.get_resource::<BiomeOverlayCache>()
        && cache.built
        && cache.key == key
        && cache.terrain_key == terrain_key
    {
        return cache.cells.clone();
    }
    let mut deps = Vec::new();
    let (biomes, _) = regia_procgen::biome::ecs::capture_biomes_with(
        world,
        [f32::MIN, f32::MIN, f32::MAX, f32::MAX],
        &mut deps,
        false,
    );
    let mut cells = claim_cells(&biomes);
    drape(&mut cells, terrain_ground(world));
    let cells = Arc::new(cells);
    let mut cache = world.get_resource_or_insert_with(BiomeOverlayCache::default);
    cache.key = key;
    cache.terrain_key = terrain_key;
    cache.cells = cells.clone();
    cache.built = true;
    cache.rebuilds += 1;
    cells
}

/// The preview run's debug outputs and its node order `(id, type)` (empty without a preview).
#[must_use]
pub fn preview_debug(world: &World) -> (Option<Arc<DebugOutputs>>, Vec<(u32, String)>) {
    let debug = super::debug_view::debug_outputs(world);
    let order = world
        .get_resource::<super::debug_view::ProcPreview>()
        .and_then(|p| p.target)
        .and_then(|t| world.get::<regia_procgen::world::ProcGenState>(t))
        .and_then(|s| s.report.as_ref())
        .map(|r| r.nodes.iter().map(|n| (n.id, n.ty.clone())).collect())
        .unwrap_or_default();
    (debug, order)
}

/// Everything the views that are on show this tick: claim cells (tint or bands on), footprints
/// and filtered marks (from the preview run), with their totals before subsampling.
#[derive(Clone, Debug, Default)]
pub struct OverlayState {
    /// Claim cells (empty unless tint or bands is on).
    pub cells: Arc<Vec<ClaimCell>>,
    /// Footprints drawn.
    pub footprints: Vec<Footprint>,
    /// Footprints before subsampling.
    pub footprints_total: usize,
    /// Every filtered mark (the draw subsamples over [`MAX_FILTERED`]).
    pub filtered: Vec<FilteredMark>,
}

/// Gathers what `views` need.
pub fn gather(world: &mut World, views: BiomeViews) -> OverlayState {
    let mut state = OverlayState::default();
    if views.tint || views.bands {
        state.cells = cached_claim_cells(world);
    }
    if views.footprints || views.filtered {
        let (debug, order) = preview_debug(world);
        if let Some(debug) = debug {
            if views.footprints {
                (state.footprints, state.footprints_total) = footprints(&debug, &order);
            }
            if views.filtered {
                state.filtered = filtered_marks(&debug, &order);
            }
        }
    }
    state
}

/// The overlay lines for the viewport tick (empty while every view is off).
#[must_use]
pub fn biome_view_lines(world: &mut World) -> Vec<Line> {
    let views = world
        .get_resource::<super::debug_view::ProcPreview>()
        .map(|p| p.biome_views)
        .unwrap_or_default();
    if !views.any() {
        return Vec::new();
    }
    let state = gather(world, views);
    overlay_lines(views, &state.cells, &state.footprints, &state.filtered)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use regia_core::id::Guid;
    use regia_procgen::biome::BiomeAreaSnap;
    use regia_procgen::biome::def::BiomeDef;
    use regia_procgen::data::{PointSet, ShapeData, ShapeForm};
    use regia_procgen::{Bundle, DataItem};

    use super::*;

    fn def(name: &str, priority: i32, color: (f32, f32, f32)) -> BiomeDef {
        BiomeDef {
            name: name.to_string(),
            priority,
            color,
            ..BiomeDef::default()
        }
    }

    /// A box area `[x0, x1] × [-20, 20]` with a blend band of `blend` meters.
    fn area(guid: u128, x0: f32, x1: f32, blend: f32, d: BiomeDef) -> BiomeAreaSnap {
        BiomeAreaSnap {
            guid: Guid(guid),
            name: format!("area{guid}"),
            outline: AreaOutline::Shape(ShapeData {
                form: ShapeForm::Box {
                    half_extents: [(x1 - x0) * 0.5, 50.0, 20.0],
                },
                position: [(x0 + x1) * 0.5, 2.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                edge_m: 0.0,
                clip_xz: None,
            }),
            blend_m: blend,
            blend_noise: None,
            priority_offset: 0,
            preview_alone: false,
            biomes: vec![(format!("{}.biome.ron", d.name), Arc::new(d))],
        }
    }

    /// Forest `[0, 100]` (priority 20, blend 10 m) over meadow `[-100, 100]` (priority 10).
    fn two_biomes() -> BiomeWorld {
        BiomeWorld {
            areas: vec![
                area(1, 0.0, 100.0, 10.0, def("Forest", 20, (0.1, 0.6, 0.1))),
                area(2, -100.0, 100.0, 0.0, def("Meadow", 10, (0.9, 0.8, 0.2))),
            ],
            ..BiomeWorld::default()
        }
    }

    /// Tint view: every claimed cell carries its owner's color; meadow owns the west half, forest
    /// the east core; the draw is two lines per cell in that color.
    #[test]
    fn tint_colors_each_claim_cell_by_its_owning_biome() {
        let cells = claim_cells(&two_biomes());
        assert!(
            !cells.is_empty() && cells.len() <= MAX_CLAIM_CELLS,
            "{}",
            cells.len()
        );
        for c in &cells {
            let want = if c.center[0] < 0.0 {
                "Meadow"
            } else if c.center[0] > 10.0 {
                "Forest"
            } else {
                continue;
            };
            assert_eq!(c.biome, want, "cell at {:?}", c.center);
            let rgb = if want == "Forest" {
                [0.1, 0.6, 0.1]
            } else {
                [0.9, 0.8, 0.2]
            };
            assert_eq!(c.color[..3], rgb);
            assert!(c.color[3] > 0.0);
            assert!((c.center[1] - 2.05).abs() < 1e-4, "drawn at the area height");
        }
        let views = BiomeViews {
            tint: true,
            ..BiomeViews::default()
        };
        let lines = overlay_lines(views, &cells, &[], &[]);
        assert_eq!(lines.len(), 2 * cells.len());
        assert!(overlay_lines(BiomeViews::default(), &cells, &[], &[]).is_empty());
    }

    /// Bands view: only cells inside the forest's 10 m blend band (x in (0, 10)) share a second
    /// claim; each draws a four-line outline.
    #[test]
    fn bands_mark_only_the_cells_inside_a_blend_band() {
        let cells = claim_cells(&two_biomes());
        let band: Vec<&ClaimCell> = cells.iter().filter(|c| c.band).collect();
        assert!(!band.is_empty(), "the forest edge blends into the meadow");
        for c in &band {
            // Within the forest's 10 m band along its outline (plus half a cell of sampling).
            let [x, _, z] = c.center;
            let h = c.half_m;
            let near_edge = x < 10.0 + h || x > 90.0 - h || z.abs() > 10.0 - h;
            assert!(
                x > -h && near_edge,
                "band cell outside the 10 m band: {:?}",
                c.center
            );
            assert_eq!(c.biome, "Forest", "the higher priority owns the band");
        }
        assert!(
            cells
                .iter()
                .any(|c| !c.band && c.center[0] > 20.0 && c.biome == "Forest"),
            "the forest core is not a band"
        );
        let views = BiomeViews {
            bands: true,
            ..BiomeViews::default()
        };
        let lines = overlay_lines(views, &cells, &[], &[]);
        assert_eq!(lines.len(), 4 * band.len());
        assert!(lines.iter().all(|l| l.2 == BAND_COLOR));
        assert!(claim_cells(&BiomeWorld::default()).is_empty());
    }

    fn points(n: usize, attrs: &[(&str, AttrColumn)]) -> DataItem {
        let mut set = PointSet::new();
        for i in 0..n {
            set.push(regia_procgen::data::Point {
                position: [i as f32, 1.0, 0.0],
                ..regia_procgen::data::Point::default()
            });
        }
        for (k, v) in attrs {
            set.attrs.insert((*k).to_string(), v.clone());
        }
        DataItem::points(set)
    }

    /// Footprints view: the last biome node whose points carry `core_m` and `spread_m` (here the
    /// exclude after the layer) gives one core and one spread circle per point.
    #[test]
    fn footprints_draw_core_and_spread_circles_from_the_last_biome_node() {
        let feet = |n: usize, core: f64| {
            points(
                n,
                &[
                    ("core_m", AttrColumn::Float(vec![core; n].into())),
                    ("spread_m", AttrColumn::Float(vec![core * 3.0; n].into())),
                    ("entry", AttrColumn::Name(vec!["pine".to_string(); n].into())),
                ],
            )
        };
        let mut debug: DebugOutputs = BTreeMap::new();
        debug.insert(4, vec![Bundle::one(feet(5, 0.5)), Bundle::empty()]);
        debug.insert(6, vec![Bundle::one(feet(3, 1.0))]);
        debug.insert(7, vec![Bundle::one(points(2, &[]))]);
        let order = vec![
            (4, "biome.layer".to_string()),
            (6, "biome.exclude".to_string()),
            (7, "biome.spawn".to_string()),
        ];
        let (drawn, total) = footprints(&debug, &order);
        assert_eq!(total, 3, "the exclude's survivors, not the layer's candidates");
        assert_eq!(drawn.len(), 3);
        assert!(
            drawn
                .iter()
                .all(|f| f.core_m == 1.0 && f.spread_m == 3.0 && f.entry == "pine")
        );
        let views = BiomeViews {
            footprints: true,
            ..BiomeViews::default()
        };
        let lines = overlay_lines(views, &[], &drawn, &[]);
        assert_eq!(lines.len(), 3 * 2 * FOOTPRINT_SEGMENTS);
        let core = lines.iter().filter(|l| l.2 == CORE_COLOR).count();
        assert_eq!(core, 3 * FOOTPRINT_SEGMENTS);
        // Each spread circle lies at its radius from its own footprint's center. The rings are
        // emitted per footprint, core then spread; the points sit 1 m apart, so measuring every
        // spread line from the first center alone would read 2 m + 3 m (ADR-7346).
        let spread: Vec<_> = lines.iter().filter(|l| l.2 == SPREAD_COLOR).collect();
        assert_eq!(spread.len(), 3 * FOOTPRINT_SEGMENTS);
        for (k, l) in spread.iter().enumerate() {
            let f = &drawn[k / FOOTPRINT_SEGMENTS];
            for end in [l.0, l.1] {
                let r = (end[0] - f.center[0]).hypot(end[2] - f.center[2]);
                assert!((r - 3.0).abs() < 1e-3, "{r} from {:?}", f.center);
            }
        }
        // Over the cap: a uniform subsample, the total stays.
        let n = MAX_FOOTPRINTS * 2 + 1;
        debug.insert(6, vec![Bundle::one(feet(n, 1.0))]);
        let (drawn, total) = footprints(&debug, &order);
        assert_eq!(total, n);
        assert!(drawn.len() <= MAX_FOOTPRINTS);
    }

    /// Filter view: every `biome.layer` node's `filtered` pin becomes dim crosses, and the hover
    /// lookup names the filter of the nearest mark.
    #[test]
    fn filter_view_draws_removed_candidates_and_hover_names_the_filter() {
        let marks = |filter: &str, n: usize| {
            points(
                n,
                &[
                    ("entry", AttrColumn::Name(vec!["reed".to_string(); n].into())),
                    ("filter", AttrColumn::Name(vec![filter.to_string(); n].into())),
                ],
            )
        };
        let mut debug: DebugOutputs = BTreeMap::new();
        debug.insert(
            4,
            vec![
                Bundle::empty(),
                Bundle(vec![marks("0:Altitude", 2), marks("1:Slope", 1)]),
            ],
        );
        // Not a layer node: its second pin is not read.
        debug.insert(5, vec![Bundle::empty(), Bundle::one(marks("9:Other", 4))]);
        let order = vec![(4, "biome.layer".to_string()), (5, "biome.resolve".to_string())];
        let all = filtered_marks(&debug, &order);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].filter, "0:Altitude");
        assert_eq!(all[2].filter, "1:Slope");
        let views = BiomeViews {
            filtered: true,
            ..BiomeViews::default()
        };
        let lines = overlay_lines(views, &[], &[], &all);
        assert_eq!(lines.len(), 2 * all.len());
        assert!(lines.iter().all(|l| l.2 == FILTERED_COLOR));
        // Marks sit at x = 0, 1 (Altitude) and x = 0 (Slope): x 0.9 is nearest the second mark.
        let hover = filtered_at(&all, 0.9, 0.0, 0.5).expect("a mark within 0.5 m");
        assert_eq!(
            (hover.entry.as_str(), hover.filter.as_str()),
            ("reed", "0:Altitude")
        );
        assert_eq!(hover.position[0], 1.0);
        // Ties go to the earlier mark; nothing in range answers none.
        assert_eq!(
            filtered_at(&all, 0.0, 0.0, 0.5).map(|m| m.filter.as_str()),
            Some("0:Altitude")
        );
        assert!(filtered_at(&all, 50.0, 0.0, 1.0).is_none());
    }

    /// The cursor tooltip (spec 65 § 7): the mark nearest the cursor ray names its entry and
    /// filter, and a ray that passes no mark answers none.
    #[test]
    fn the_cursor_tooltip_names_the_mark_under_the_pointer() {
        let marks = vec![
            FilteredMark {
                position: [0.0, 1.0, 0.0],
                entry: "reed".to_string(),
                filter: "0:Altitude".to_string(),
            },
            FilteredMark {
                position: [10.0, 1.0, 0.0],
                entry: "pine".to_string(),
                filter: "1:Slope".to_string(),
            },
        ];
        let on_ray =
            |from: [f32; 3], to: [f32; 3]| filtered_on_ray(&marks, from, to, 1.0).map(|m| m.entry.clone());
        assert_eq!(
            on_ray([-5.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
            Some("reed".to_string()),
            "a ray through the first mark"
        );
        assert_eq!(
            on_ray([5.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
            Some("pine".to_string()),
            "a ray past the first mark hits the second"
        );
        assert_eq!(
            on_ray([5.0, 50.0, 0.0], [0.0, -1.0, 0.0]),
            None,
            "a ray 5 m east of both marks hits neither"
        );
        assert_eq!(
            filter_tooltip_text(&marks[1]),
            "pine · removed by 1:Slope".to_string()
        );
    }

    /// A quiet tick reuses the claim raster; moving an area rebuilds it.
    #[test]
    fn claim_cells_are_cached_until_an_area_changes() {
        let mut world = World::new();
        let e = world
            .spawn((BiomeArea::new("x.biome.ron"), Transform::default()))
            .id();
        let _ = cached_claim_cells(&mut world);
        let _ = cached_claim_cells(&mut world);
        assert_eq!(world.resource::<BiomeOverlayCache>().rebuilds, 1);
        if let Some(mut tf) = world.get_mut::<Transform>(e) {
            tf.translation.x += 3.0;
        }
        let _ = cached_claim_cells(&mut world);
        assert_eq!(world.resource::<BiomeOverlayCache>().rebuilds, 2);
    }
}
