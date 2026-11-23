// SPDX-License-Identifier: MPL-2.0
//! Samplers and generators (`sample.*`, `gen.*`, spec 63 § 4).

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_core::math::hash::mix;

use crate::data::{Bundle, DataItem, GroundData, Payload, Point, PointSet};
use crate::exec::{CHUNK, NodeCtx};
use crate::nodes::common::{Combine, Region, rotation_between, world_bounds, xz_bounds};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SPATIAL, SettingType as T, Settings, num, pin_out, pin_req, set,
};
use crate::value::SettingValue;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "sample.ground",
        "sample",
        "A world-aligned grid of candidates on the ground, jittered, density = region value",
        &[
            num("spacing_m", T::Float, "4.0", 0.01, 1.0e6, "Cell size, meters"),
            num(
                "jitter",
                T::Float,
                "0.8",
                0.0,
                1.0,
                "Fraction of a cell each candidate may move",
            ),
            num(
                "bounds_m",
                T::Vec3,
                "(1.0, 1.0, 1.0)",
                0.0,
                1.0e6,
                "Point box size",
            ),
            set(
                "keep_zero",
                T::Bool,
                "false",
                "Keep candidates with region value 0",
            ),
            set(
                "align",
                T::Enum(&["up", "normal"]),
                "up",
                "Rotation: identity or the ground normal",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("region", SPATIAL)], vec![pin_out("out", POINTS)]),
        eval_ground,
    )
    .world()
    .reads_world_data(),
    BuiltinNode::new(
        "sample.region",
        "sample",
        "A world-aligned grid over the region without terrain; Y from the region or `y`",
        &[
            num("spacing_m", T::Float, "2.0", 0.01, 1.0e6, "Cell size, meters"),
            num(
                "jitter",
                T::Float,
                "0.0",
                0.0,
                1.0,
                "Fraction of a cell each candidate may move",
            ),
            set(
                "height",
                T::Enum(&["region", "value"]),
                "region",
                "Y from the region's center or `y`",
            ),
            set("y", T::Float, "0.0", "Y when `height: value`"),
        ],
        |_| PinSpec::new(vec![pin_req("region", SPATIAL)], vec![pin_out("out", POINTS)]),
        eval_region,
    ),
    BuiltinNode::new(
        "sample.poisson",
        "sample",
        "World-anchored blue noise: priority dart throwing on a fixed world grid",
        &[
            num(
                "min_dist_m",
                T::Float,
                "2.0",
                0.05,
                1.0e6,
                "Minimum distance between points",
            ),
            num("attempts", T::Int, "4", 1.0, 32.0, "Candidates per grid cell"),
            num(
                "max_points",
                T::Int,
                "100000",
                1.0,
                1.0e9,
                "Cap on the output count (per run: per cell on a tiled tier)",
            ),
        ],
        |_| PinSpec::new(vec![pin_req("region", SPATIAL)], vec![pin_out("out", POINTS)]),
        eval_poisson,
    )
    .reaching(|s| 2.0 * s.f("min_dist_m"))
    .reads_world_data(),
    BuiltinNode::new(
        "gen.grid",
        "gen",
        "A local grid of points around the origin (order y, z, x)",
        &[
            set("count", T::Vec3, "(4.0, 1.0, 4.0)", "Points per axis (integers)"),
            num(
                "step_m",
                T::Vec3,
                "(1.0, 1.0, 1.0)",
                0.0,
                1.0e6,
                "Spacing per axis",
            ),
            set("centered", T::Bool, "true", "Center the grid on the origin"),
        ],
        |_| PinSpec::new(vec![], vec![pin_out("out", POINTS)]),
        eval_grid,
    )
    .checked(check_grid),
    BuiltinNode::new(
        "gen.points",
        "gen",
        "Literal points: [(position, rotation_deg?, scale?)]",
        &[set(
            "points",
            T::Any,
            "[]",
            "List of (position: (x,y,z), rotation_deg: (x,y,z), scale: (x,y,z))",
        )],
        |_| PinSpec::new(vec![], vec![pin_out("out", POINTS)]),
        eval_points,
    )
    .checked(check_points),
    BuiltinNode::new(
        "gen.one",
        "gen",
        "One point at the origin or the generator's position",
        &[set("at", T::Enum(&["origin", "self"]), "origin", "Where")],
        |_| PinSpec::new(vec![], vec![pin_out("out", POINTS)]),
        eval_one,
    )
    .uncached(),
];

/// The ground to sample under a region item: the item itself, or the run's ground clipped to its
/// XZ bounds.
fn ground_for(ctx: &NodeCtx<'_>, item: &DataItem) -> Option<(Arc<GroundData>, [f32; 4])> {
    if let Payload::Ground(g) = &item.payload {
        return Some((g.clone(), g.rect));
    }
    let g = ctx.world().ground()?;
    let b = xz_bounds(item)?;
    let rect = [
        b[0].max(g.rect[0]),
        b[1].max(g.rect[1]),
        b[2].min(g.rect[2]),
        b[3].min(g.rect[3]),
    ];
    Some((g, rect))
}

/// Integer cell range `[lo, hi]` whose cells `[i·s, (i+1)·s)` overlap `[min, max)`.
fn cells(min: f32, max: f32, s: f64) -> (i64, i64) {
    let lo = (f64::from(min) / s).floor() as i64;
    let hi = ((f64::from(max) / s).ceil() as i64).saturating_sub(1);
    (lo, hi)
}

/// The number of cells in `[lo, hi]`. A bound past the `i64` range saturates in [`cells`], and the
/// span of two saturated bounds does not fit in `i64`, so it is measured wider.
pub(crate) fn cell_span(lo: i64, hi: i64) -> u128 {
    (i128::from(hi) - i128::from(lo) + 1).max(0) as u128
}

/// What a grid sampler lays out.
struct GridSpec {
    rect: [f32; 4],
    spacing: f64,
    jitter: f64,
    keep_zero: bool,
    bounds: [f32; 3],
}

fn grid_sample(
    ctx: &mut NodeCtx<'_>,
    spec: &GridSpec,
    mut place: impl FnMut(f32, f32) -> Option<(f32, [f32; 4])>,
    region: &Region<'_>,
) -> Result<PointSet, NodeError> {
    let GridSpec {
        rect,
        spacing,
        jitter,
        keep_zero,
        bounds,
    } = *spec;
    let (x0, x1) = cells(rect[0], rect[2], spacing);
    let (z0, z1) = cells(rect[1], rect[3], spacing);
    let count = cell_span(x0, x1).saturating_mul(cell_span(z0, z1));
    ctx.check_points(usize::try_from(count).unwrap_or(usize::MAX))?;
    let mut out = PointSet::new();
    let mut writer = out.writer();
    let mut n = 0usize;
    for iz in z0..=z1 {
        for ix in x0..=x1 {
            n += 1;
            if n.is_multiple_of(CHUNK) {
                ctx.check_cancel()?;
            }
            let cx = ((ix as f64 + 0.5) * spacing) as f32;
            let cz = ((iz as f64 + 0.5) * spacing) as f32;
            // Draws keyed by the cell center so they do not depend on the run or the cell.
            let cell_seed = ctx.point_seed([cx, 0.0, cz]);
            let jx = (ctx.draw(cell_seed, 0) - 0.5) * jitter * spacing;
            let jz = (ctx.draw(cell_seed, 1) - 0.5) * jitter * spacing;
            let x = cx + jx as f32;
            let z = cz + jz as f32;
            // Half-open like `cells` and tile ownership: a shared edge belongs to one side only.
            if x < rect[0] || x >= rect[2] || z < rect[1] || z >= rect[3] {
                continue;
            }
            let Some((y, rotation)) = place(x, z) else {
                continue;
            };
            let position = [x, y, z];
            let d = region.value(position);
            if d <= 0.0 && !keep_zero {
                continue;
            }
            writer.push(Point {
                position,
                rotation,
                density: d,
                seed: ctx.point_seed(position),
                bounds_min: bounds.map(|b| -b * 0.5),
                bounds_max: bounds.map(|b| b * 0.5),
                ..Point::default()
            });
        }
    }
    Ok(out)
}

fn eval_ground(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let spacing = s.f("spacing_m");
    let jitter = s.f("jitter");
    let bm = s.v3("bounds_m").map(|v| v as f32);
    let align = s.text("align") == "normal";
    let keep_zero = s.b("keep_zero");
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let Some((ground, rect)) = ground_for(ctx, item) else {
            ctx.warn("no_terrain", "no ground under the region");
            continue;
        };
        let one = Bundle::one(item.clone());
        let region = Region::new(&one, Combine::Max)?;
        let g = ground.clone();
        let place = move |x: f32, z: f32| {
            let y = g.height(x, z);
            let rot = if align {
                rotation_between(Vec3::Y, Vec3::from(g.normal(x, z))).to_array()
            } else {
                [0.0, 0.0, 0.0, 1.0]
            };
            Some((y, rot))
        };
        let spec = GridSpec {
            rect,
            spacing,
            jitter,
            keep_zero,
            bounds: bm,
        };
        let points = grid_sample(ctx, &spec, place, &region)?;
        out.0.push(item.with_payload(Payload::Points(points)));
    }
    Ok(vec![out])
}

fn eval_region(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let spacing = s.f("spacing_m");
    let jitter = s.f("jitter");
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let Some((lo, hi)) = world_bounds(item) else {
            out.0.push(item.with_payload(Payload::Points(PointSet::new())));
            continue;
        };
        let y = if s.text("height") == "value" {
            s.f("y") as f32
        } else {
            match &item.payload {
                Payload::Shape(sh) => sh.position[1],
                _ => (lo[1] + hi[1]) * 0.5,
            }
        };
        let one = Bundle::one(item.clone());
        let region = Region::new(&one, Combine::Max)?;
        let spec = GridSpec {
            rect: [lo[0], lo[2], hi[0], hi[2]],
            spacing,
            jitter,
            keep_zero: false,
            bounds: [1.0; 3],
        };
        let points = grid_sample(ctx, &spec, |_, _| Some((y, [0.0, 0.0, 0.0, 1.0])), &region)?;
        out.0.push(item.with_payload(Payload::Points(points)));
    }
    Ok(vec![out])
}

fn eval_poisson(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let r = s.f("min_dist_m");
    let attempts = s.i("attempts").clamp(1, 32) as u32;
    let max_points = usize::try_from(s.i("max_points")).unwrap_or(usize::MAX);
    let cell = r / std::f64::consts::SQRT_2;
    let mut out = Bundle::empty();
    for item in crate::nodes::input_items(inputs, 0) {
        let Some(b) = xz_bounds(item) else {
            out.0.push(item.with_payload(Payload::Points(PointSet::new())));
            continue;
        };
        let halo = 2.0 * r;
        let (x0, x1) = cells(
            (f64::from(b[0]) - halo) as f32,
            (f64::from(b[2]) + halo) as f32,
            cell,
        );
        let (z0, z1) = cells(
            (f64::from(b[1]) - halo) as f32,
            (f64::from(b[3]) + halo) as f32,
            cell,
        );
        let ncells = cell_span(x0, x1)
            .saturating_mul(cell_span(z0, z1))
            .saturating_mul(u128::from(attempts));
        ctx.check_points(usize::try_from(ncells).unwrap_or(usize::MAX))?;
        if ncells == 0 {
            // An inverted rectangle on either axis has no cells; sizing the grid below from a
            // negative width would wrap to a huge allocation.
            out.0.push(item.with_payload(Payload::Points(PointSet::new())));
            continue;
        }
        let w = (x1 - x0 + 1) as usize;
        let h = (z1 - z0 + 1) as usize;
        // Candidates from world cells only: (priority, cell z, cell x, k, x, z), cells in row-major
        // order (z, then x) in element chunks on the pool, joined in chunk order (ADR-7252).
        let node_seed = ctx.node_seed;
        let parts = ctx.map_chunks(w * h, |range| {
            let mut part: Vec<PoissonCand> = Vec::with_capacity(range.len() * attempts as usize);
            for t in range {
                let (iz, ix) = (z0 + (t / w) as i64, x0 + (t % w) as i64);
                let key = mix(mix(ix as u32, iz as u32), node_seed);
                for k in 0..attempts {
                    let mut rng = regia_core::math::Rng::with_stream(u64::from(key), u64::from(k));
                    let u = rng.f64();
                    let v = rng.f64();
                    let pr = rng.f64();
                    part.push((pr, iz, ix, k, (ix as f64 + u) * cell, (iz as f64 + v) * cell));
                }
            }
            part
        })?;
        let mut cands: Vec<PoissonCand> = parts.concat();
        ctx.check_cancel()?;
        // Descending priority, ties by cell then candidate index.
        sort_candidates(ctx, &mut cands)?;
        // Background grid of accepted points (one per cell at most, since cell = r/√2).
        let mut grid: Vec<Option<(f64, f64)>> = vec![None; w * h];
        let mut accepted: Vec<(f64, f64)> = Vec::new();
        for (n, c) in cands.iter().enumerate() {
            if n.is_multiple_of(CHUNK) {
                ctx.check_cancel()?;
            }
            let (gx, gz) = (c.2 - x0, c.1 - z0);
            let mut ok = true;
            'scan: for dz in -2..=2i64 {
                for dx in -2..=2i64 {
                    let (nx, nz) = (gx + dx, gz + dz);
                    if nx < 0 || nz < 0 || nx >= w as i64 || nz >= h as i64 {
                        continue;
                    }
                    if let Some((px, pz)) = grid[nz as usize * w + nx as usize] {
                        let (ddx, ddz) = (px - c.4, pz - c.5);
                        if ddx * ddx + ddz * ddz < r * r {
                            ok = false;
                            break 'scan;
                        }
                    }
                }
            }
            if ok && grid[gz as usize * w + gx as usize].is_none() {
                grid[gz as usize * w + gx as usize] = Some((c.4, c.5));
                accepted.push((c.4, c.5));
            }
        }
        // Keep points in the region, in a stable order (z, then x).
        accepted.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.total_cmp(&b.0)));
        let one = Bundle::one(item.clone());
        let region = Region::new(&one, Combine::Max)?;
        let ground = ctx.world().ground();
        let mut points = PointSet::new();
        let mut writer = points.writer();
        for (x, z) in accepted {
            let (x, z) = (x as f32, z as f32);
            if x < b[0] || x >= b[2] || z < b[1] || z >= b[3] {
                continue;
            }
            let y = ground.as_ref().map_or(0.0, |g| g.height(x, z));
            let mut position = [x, y, z];
            let mut d = region.value(position);
            if d <= 0.0 {
                // Regions with height (shapes) are tested at their own height too.
                if let Some((lo, hi)) = world_bounds(item) {
                    position[1] = position[1].clamp(lo[1], hi[1]);
                    d = region.value(position);
                    position[1] = y;
                }
            }
            if d <= 0.0 {
                continue;
            }
            if writer.position.len() >= max_points {
                ctx.warn("max_points", format!("capped at {max_points} points"));
                break;
            }
            writer.push(Point {
                position,
                density: d,
                seed: ctx.point_seed(position),
                ..Point::default()
            });
        }
        out.0.push(item.with_payload(Payload::Points(points)));
    }
    Ok(vec![out])
}

/// A Poisson candidate: (priority, cell z, cell x, attempt, x, z).
type PoissonCand = (f64, i64, i64, u32, f64, f64);

/// The candidate order as an integer key: descending priority (by `f64::total_cmp`, through the
/// integer transform its documentation gives), then cell z, cell x and attempt ascending. The cell
/// and attempt make every key unique, so this is a strict total order and every correct sort, stable
/// or not, split or not, gives the same sequence.
fn cand_key(c: &PoissonCand) -> (std::cmp::Reverse<i64>, i64, i64, u32) {
    let mut bits = c.0.to_bits() as i64;
    bits ^= (((bits >> 63) as u64) >> 1) as i64;
    (std::cmp::Reverse(bits), c.1, c.2, c.3)
}

/// Sorts candidates by [`cand_key`]: one run per worker sorted on the pool, then merged (ADR-7252).
fn sort_candidates(ctx: &NodeCtx<'_>, cands: &mut Vec<PoissonCand>) -> Result<(), NodeError> {
    let runs = ctx.threads().min(cands.len() / CHUNK).max(1);
    if runs == 1 {
        cands.sort_unstable_by_key(cand_key);
        return Ok(());
    }
    let run_len = cands.len().div_ceil(runs);
    ctx.for_each_job(
        cands.chunks_mut(run_len).collect(),
        |run: &mut [PoissonCand]| {
            run.sort_unstable_by_key(cand_key);
        },
    )?;
    *cands = merge_sorted_runs(cands, run_len);
    Ok(())
}

/// Merges consecutive runs of `run_len` candidates, each sorted by [`cand_key`], into one sorted list.
fn merge_sorted_runs(cands: &[PoissonCand], run_len: usize) -> Vec<PoissonCand> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut runs: Vec<&[PoissonCand]> = cands.chunks(run_len.max(1)).collect();
    let mut heap = BinaryHeap::with_capacity(runs.len());
    for (r, run) in runs.iter().enumerate() {
        if let Some(first) = run.first() {
            heap.push(Reverse((cand_key(first), r)));
        }
    }
    let mut merged = Vec::with_capacity(cands.len());
    while let Some(Reverse((_, r))) = heap.pop() {
        let Some((head, rest)) = runs[r].split_first() else {
            continue;
        };
        merged.push(*head);
        runs[r] = rest;
        if let Some(next) = rest.first() {
            heap.push(Reverse((cand_key(next), r)));
        }
    }
    merged
}

fn check_grid(s: &Settings) -> Vec<(String, String)> {
    let c = s.v3("count");
    if c.iter().any(|v| *v < 1.0 || v.fract() != 0.0 || *v > 1.0e6) {
        return vec![(
            "setting_out_of_range".into(),
            "`count` needs integers in 1..=1000000".into(),
        )];
    }
    Vec::new()
}

fn eval_grid(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let c = s.v3("count").map(|v| v as i64);
    let step = s.v3("step_m");
    let total = (c[0] as u128) * (c[1] as u128) * (c[2] as u128);
    ctx.check_points(usize::try_from(total).unwrap_or(usize::MAX))?;
    let center = s.b("centered");
    let off = |k: usize| {
        if center {
            (c[k] - 1) as f64 * step[k] * 0.5
        } else {
            0.0
        }
    };
    let mut p = PointSet::new();
    let mut writer = p.writer();
    for y in 0..c[1] {
        for z in 0..c[2] {
            for x in 0..c[0] {
                let pos = [
                    (x as f64 * step[0] - off(0)) as f32,
                    (y as f64 * step[1] - off(1)) as f32,
                    (z as f64 * step[2] - off(2)) as f32,
                ];
                writer.push(Point {
                    position: pos,
                    seed: ctx.point_seed(pos),
                    ..Point::default()
                });
            }
        }
    }
    Ok(vec![Bundle::one(DataItem::points(p))])
}

/// One literal point: position, rotation in degrees, scale.
type LiteralPoint = ([f64; 3], [f64; 3], [f64; 3]);

fn literal_points(v: &SettingValue) -> Option<Vec<LiteralPoint>> {
    let items = v.as_items()?;
    items
        .iter()
        .map(|p| {
            // Either a record (position: ..., rotation_deg: ..., scale: ...) or a tuple.
            let (pos, rot, scale) = match p {
                SettingValue::Record(_, _) => (
                    p.field("position")?.as_vec3()?,
                    p.field("rotation_deg")
                        .map_or(Some([0.0; 3]), SettingValue::as_vec3)?,
                    p.field("scale").map_or(Some([1.0; 3]), SettingValue::as_vec3)?,
                ),
                _ => {
                    let t = p.as_items()?;
                    if let Some(single) = p.as_vec3().filter(|_| t.iter().all(|x| x.as_f64().is_some())) {
                        (single, [0.0; 3], [1.0; 3])
                    } else {
                        (
                            t.first()?.as_vec3()?,
                            t.get(1).map_or(Some([0.0; 3]), SettingValue::as_vec3)?,
                            t.get(2).map_or(Some([1.0; 3]), SettingValue::as_vec3)?,
                        )
                    }
                }
            };
            Some((pos, rot, scale))
        })
        .collect()
}

fn check_points(s: &Settings) -> Vec<(String, String)> {
    match s.get("points").map(literal_points) {
        Some(Some(_)) | None => Vec::new(),
        Some(None) => vec![(
            "setting_type".into(),
            "`points` must be a list of (position: (x, y, z), rotation_deg?, scale?)".into(),
        )],
    }
}

fn eval_points(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let list = s.get("points").and_then(literal_points).unwrap_or_default();
    ctx.check_points(list.len())?;
    let mut p = PointSet::new();
    for (pos, rot, scale) in list {
        let position = pos.map(|v| v as f32);
        p.push(Point {
            position,
            rotation: crate::nodes::common::euler_deg(rot).to_array(),
            scale: scale.map(|v| v as f32),
            seed: ctx.point_seed(position),
            ..Point::default()
        });
    }
    Ok(vec![Bundle::one(DataItem::points(p))])
}

fn eval_one(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let position = if s.text("at") == "self" {
        ctx.world().self_info().map_or([0.0; 3], |i| i.bounds.position)
    } else {
        [0.0; 3]
    };
    let mut p = PointSet::new();
    p.push(Point {
        position,
        seed: ctx.point_seed(position),
        ..Point::default()
    });
    Ok(vec![Bundle::one(DataItem::points(p))])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The candidate order of the original comparator (kept verbatim here) equals a whole
    /// `cand_key` sort and a split sort merged by `merge_sorted_runs`, for every split; with ties in
    /// priority, NaN of both signs, signed zeros and infinities (ADR-7252).
    #[test]
    fn split_sorts_give_the_comparator_order() {
        let mut rng = regia_core::math::Rng::with_stream(7, 1);
        let specials = [
            0.0,
            -0.0,
            1.0,
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.5,
        ];
        // (i mod 13, i mod 101, i / 1313) is unique per i, like (cell z, cell x, attempt).
        let cands: Vec<PoissonCand> = (0..20_000usize)
            .map(|i| {
                let pr = if i.is_multiple_of(7) {
                    specials[(i / 7) % specials.len()]
                } else if i.is_multiple_of(3) {
                    0.25
                } else {
                    rng.f64()
                };
                (
                    pr,
                    (i % 13) as i64 - 6,
                    (i % 101) as i64 - 50,
                    (i / 1313) as u32,
                    0.0,
                    i as f64,
                )
            })
            .collect();
        let mut reference = cands.clone();
        reference.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.1, a.2, a.3).cmp(&(b.1, b.2, b.3))));
        let bits = |v: &[PoissonCand]| {
            v.iter()
                .map(|c| (c.0.to_bits(), c.1, c.2, c.3, c.5.to_bits()))
                .collect::<Vec<_>>()
        };
        let mut whole = cands.clone();
        whole.sort_unstable_by_key(cand_key);
        assert_eq!(bits(&whole), bits(&reference));
        for runs in [1, 2, 3, 7, 8, 32] {
            let run_len = cands.len().div_ceil(runs);
            let mut split = cands.clone();
            for run in split.chunks_mut(run_len) {
                run.sort_unstable_by_key(cand_key);
            }
            assert_eq!(
                bits(&merge_sorted_runs(&split, run_len)),
                bits(&reference),
                "{runs} runs"
            );
        }
    }
}
