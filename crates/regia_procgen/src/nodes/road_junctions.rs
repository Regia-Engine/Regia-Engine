// SPDX-License-Identifier: MPL-2.0
//! `road.junctions` and `road.decals` (spec 64 § 7.2).
//!
//! Junctions: crossings (XZ) and T-ends between the input roads are clustered; each road is cut
//! back from a cluster by half the other road's total width (carriageway and shoulders) plus its
//! own `junction.margin_m`, and the pieces left go to `out_splines` for `road.ribbon`. The patch
//! is built from the end rows of those pieces, computed with the ribbon's own surface code so its
//! vertices are the ribbon's: a fan from the rows' centroid over all row vertices in angular order,
//! then the triangles that close the fan to its convex hull.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regia_core::math::Vec3;
use regia_scene::spline::SplineCurve;
use regia_terrain::RoadDecal;

use crate::data::{AttrColumn, Bundle, DataItem, GroundData, MeshData, Payload, SplineData, Table};
use crate::exec::{GeneratedMesh, NodeCtx};
use crate::nodes::roads::{
    GROUND, MESH, RoadSurface, check_smooth_rows, profile_for, road_key, vertex_normals,
};
use crate::nodes::spline::{SPLINE, cross_xz, dense};
use crate::nodes::{
    BuiltinNode, NodeError, PinSpec, SettingType as T, Settings, TABLE, input, num, pin_in, pin_out, pin_req,
    set,
};
use crate::road::RoadProfile;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "road.junctions",
        "road",
        "Finds crossings and T-ends, cuts the roads back from them (`out_splines`, for road.ribbon) and fills each junction with a patch (`out_meshes`)",
        &[
            set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)"),
            num("snap_m", T::Float, "0.5", 0.0, 100.0, "T-end detection slack beyond width/2 + margin_m"),
            num("min_arm_deg", T::Float, "20.0", 0.0, 90.0, "Arms closer than this warn `junction_sharp`"),
            num("smooth_rows", T::Int, "3", 1.0, 15.0, "Must match the ribbon's smoothing (odd)"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("splines", SPLINE), pin_in("ground", GROUND)],
                vec![pin_out("out_meshes", MESH), pin_out("out_splines", SPLINE)],
            )
        },
        eval_junctions,
    )
    .uncached()
    .world()
    .checked(check_smooth_rows),
    BuiltinNode::new(
        "road.decals",
        "road",
        "Road decal segments for hosts that draw roads in the terrain shader (run output `road_decals`, a table on `out`)",
        &[
            set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)"),
            num("max_error_m", T::Float, "0.25", 0.001, 100.0, "Polyline error"),
            set("color", T::Vec3, "(0.05, 0.05, 0.055)", "Linear RGB the decal writes"),
        ],
        |_| PinSpec::new(vec![pin_req("splines", SPLINE)], vec![pin_out("out", TABLE)]),
        eval_decals,
    )
    .uncached()
    .world(),
];

/// Half a road's total width: carriageway plus its wider shoulder.
fn half_width(prof: &RoadProfile) -> f32 {
    prof.width_m * 0.5 + prof.shoulders_m.0.max(prof.shoulders_m.1)
}

/// One road of the input.
struct Road<'a> {
    item: &'a DataItem,
    curve: SplineCurve,
    line: Vec<(Vec3, f64)>,
    prof: RoadProfile,
    key: String,
}

/// A place two roads meet: the distance along each and the center point. `gap_b` is what a T-end
/// falls short of road `b`'s center line (0 for a crossing).
#[derive(Clone, Copy, Debug)]
struct Meet {
    a: usize,
    b: usize,
    along_a: f64,
    along_b: f64,
    center: Vec3,
    gap: f64,
}

/// The XZ closest point of polyline `line` to `p`: `(distance, along)`.
fn closest_xz(line: &[(Vec3, f64)], p: Vec3) -> (f64, f64) {
    let mut best = (f64::INFINITY, 0.0);
    for w in line.windows(2) {
        let (a, b) = (w[0].0, w[1].0);
        let (dx, dz) = (f64::from(b.x - a.x), f64::from(b.z - a.z));
        let (px, pz) = (f64::from(p.x - a.x), f64::from(p.z - a.z));
        let l2 = dx * dx + dz * dz;
        let t = if l2 > 0.0 {
            ((px * dx + pz * dz) / l2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (ex, ez) = (px - dx * t, pz - dz * t);
        let d = (ex * ex + ez * ez).sqrt();
        if d < best.0 {
            best = (d, w[0].1 + (w[1].1 - w[0].1) * t);
        }
    }
    best
}

/// Every crossing between the roads, in `(a, b, along_a)` order.
fn crossings(ctx: &NodeCtx<'_>, roads: &[Road<'_>]) -> Result<Vec<Meet>, NodeError> {
    let mut out = Vec::new();
    for (a, ra) in roads.iter().enumerate() {
        for (b, rb) in roads.iter().enumerate().skip(a + 1) {
            ctx.check_cancel()?;
            for pa in ra.line.windows(2) {
                let (p0, p1) = (pa[0].0, pa[1].0);
                for pb in rb.line.windows(2) {
                    let (q0, q1) = (pb[0].0, pb[1].0);
                    if p0.x.max(p1.x) < q0.x.min(q1.x)
                        || q0.x.max(q1.x) < p0.x.min(p1.x)
                        || p0.z.max(p1.z) < q0.z.min(q1.z)
                        || q0.z.max(q1.z) < p0.z.min(p1.z)
                    {
                        continue;
                    }
                    if let Some((t, u)) = cross_xz(p0, p1, q0, q1) {
                        out.push(Meet {
                            a,
                            b,
                            along_a: pa[0].1 + (pa[1].1 - pa[0].1) * t,
                            along_b: pb[0].1 + (pb[1].1 - pb[0].1) * u,
                            center: p0.lerp(p1, t as f32),
                            gap: 0.0,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// A spline end of road `a` near road `b`, not already part of a crossing with it: `a` stops at the
/// other road (spec 64 § 7.2 T-ends).
fn t_ends(roads: &[Road<'_>], meets: &[Meet], snap_m: f64) -> Vec<Meet> {
    let mut out = Vec::new();
    for (a, ra) in roads.iter().enumerate() {
        let len = ra.curve.length_f64();
        for along_a in [0.0, len] {
            let end = ra.curve.at_distance_f64(along_a).position;
            for (b, rb) in roads.iter().enumerate() {
                if a == b {
                    continue;
                }
                let (dist, along_b) = closest_xz(&rb.line, end);
                let reach = f64::from(half_width(&rb.prof) + ra.prof.junction.margin_m) + snap_m;
                if dist > reach {
                    continue;
                }
                let crossing_near = meets.iter().any(|m| {
                    (m.a == a.min(b) && m.b == a.max(b))
                        && ((if m.a == a { m.along_a } else { m.along_b }) - along_a).abs() <= reach
                });
                if !crossing_near {
                    out.push(Meet {
                        a,
                        b,
                        along_a,
                        along_b,
                        center: rb.curve.at_distance_f64(along_b).position,
                        gap: dist,
                    });
                }
            }
        }
    }
    out
}

/// Clusters meets whose cut zones touch (union-find in meet order): one junction per cluster.
fn cluster(meets: &[Meet], roads: &[Road<'_>]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..meets.len()).collect();
    fn find(p: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while p[r] != r {
            p[r] = p[p[r]];
            r = p[r];
        }
        r
    }
    let reach = |m: &Meet| {
        f64::from(
            half_width(&roads[m.a].prof)
                + half_width(&roads[m.b].prof)
                + roads[m.a]
                    .prof
                    .junction
                    .margin_m
                    .max(roads[m.b].prof.junction.margin_m),
        )
    };
    // The cut half-length a meet takes out of road `r`, and its place along that road.
    let cut = |m: &Meet, r: usize| {
        let other = if m.a == r { m.b } else { m.a };
        f64::from(half_width(&roads[other].prof) + roads[r].prof.junction.margin_m)
    };
    let along = |m: &Meet, r: usize| if m.a == r { m.along_a } else { m.along_b };
    // Two meets on one road whose cuts overlap leave no piece between them, however far apart their
    // centers are: separate patches would leave a hole across the road.
    let cuts_touch = |x: &Meet, y: &Meet| {
        [x.a, x.b]
            .into_iter()
            .any(|r| (r == y.a || r == y.b) && (along(x, r) - along(y, r)).abs() <= cut(x, r) + cut(y, r))
    };
    for i in 0..meets.len() {
        for j in i + 1..meets.len() {
            let d = f64::from((meets[i].center - meets[j].center).length());
            if d <= reach(&meets[i]).max(reach(&meets[j])) || cuts_touch(&meets[i], &meets[j]) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[b.max(a)] = b.min(a);
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..meets.len() {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().collect()
}

/// The interval of road `r` a junction removes, from each meet on it: half the other road's width
/// plus this road's margin around the meet, less what a T-end falls short.
fn cut_interval(group: &[usize], meets: &[Meet], roads: &[Road<'_>], r: usize) -> Option<(f64, f64)> {
    let len = roads[r].curve.length_f64();
    let margin = roads[r].prof.junction.margin_m;
    let mut span: Option<(f64, f64)> = None;
    for &k in group {
        let m = &meets[k];
        let (along, other, stub) = if m.a == r {
            (m.along_a, m.b, m.gap > 0.0 || along_is_end(m.along_a, len))
        } else if m.b == r {
            (m.along_b, m.a, false)
        } else {
            continue;
        };
        let c = f64::from(half_width(&roads[other].prof) + margin);
        let (lo, hi) = if stub {
            (along - (c - m.gap).max(0.0), along + (c - m.gap).max(0.0))
        } else {
            (along - c, along + c)
        };
        span = Some(span.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
    }
    span.map(|(a, b)| (a.max(0.0), b.min(len)))
}

fn along_is_end(d: f64, len: f64) -> bool {
    d <= 1e-6 || (len - d).abs() <= 1e-6
}

/// A monotonic stand-in for `atan2(z, x)`: 0..4 around the circle, by the diamond of `|x| + |z|`.
fn pseudo_angle(x: f64, z: f64) -> f64 {
    let d = x.abs() + z.abs();
    if d <= 0.0 {
        return 0.0;
    }
    let t = x / d;
    if z >= 0.0 { 1.0 - t } else { 3.0 + t }
}

/// Fills the star of `verts` around their centroid, then closes it to the convex hull.
fn patch_triangles(verts: &[Vec3]) -> (Vec<[f32; 3]>, Vec<u32>) {
    let n = verts.len();
    let centroid = verts.iter().fold(Vec3::ZERO, |a, v| a + *v) / n.max(1) as f32;
    // A pseudo-angle: monotonic around the circle, no libm (spec 64 § 8).
    let angle = |v: &Vec3| pseudo_angle(f64::from(v.x - centroid.x), f64::from(v.z - centroid.z));
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| {
        angle(&verts[*a])
            .total_cmp(&angle(&verts[*b]))
            .then(verts[*a].x.total_cmp(&verts[*b].x))
            .then(verts[*a].z.total_cmp(&verts[*b].z))
    });
    let mut positions: Vec<[f32; 3]> = verts.iter().map(|v| v.to_array()).collect();
    positions.push(centroid.to_array());
    let hub = n as u32;
    let mut indices = Vec::with_capacity(3 * n + 6);
    for k in 0..n {
        indices.extend([hub, order[k] as u32, order[(k + 1) % n] as u32]);
    }
    // Graham pass over the ring: a reflex vertex gets a triangle to its neighbors and leaves the ring.
    let mut ring = order;
    let cross = |a: usize, b: usize, c: usize| {
        let (p, q, r) = (verts[a], verts[b], verts[c]);
        f64::from(q.x - p.x) * f64::from(r.z - p.z) - f64::from(q.z - p.z) * f64::from(r.x - p.x)
    };
    let mut k = 0;
    let mut stable = 0;
    while ring.len() > 3 && stable < ring.len() {
        let m = ring.len();
        let (a, b, c) = (ring[k % m], ring[(k + 1) % m], ring[(k + 2) % m]);
        if cross(a, b, c) < -1e-9 {
            indices.extend([a as u32, c as u32, b as u32]);
            ring.remove((k + 1) % m);
            stable = 0;
            k = k.saturating_sub(1);
        } else {
            k = (k + 1) % m;
            stable += 1;
        }
    }
    // Every triangle faces up.
    for t in indices.as_chunks_mut::<3>().0 {
        let [a, b, c] = t.map(|i| Vec3::from(positions[i as usize]));
        if (b - a).cross(c - a).y < 0.0 {
            t.swap(1, 2);
        }
    }
    (positions, indices)
}

fn eval_junctions(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.road_junctions").entered();
    let ground_b = input(inputs, 1);
    let ground: Option<Arc<GroundData>> = ground_b.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    });
    let smooth = (s.i("smooth_rows").clamp(1, 15) as usize) | 1;
    let tile = ctx.tile();
    let cell_m = tile.map(|t| t.cell_m);
    let splines = input(inputs, 0);
    let mut roads = Vec::new();
    for (index, item) in splines.0.iter().enumerate() {
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let curve = sd.curve();
        if !crate::nodes::common::has_length(f64::from(curve.length())) {
            continue;
        }
        let prof = profile_for(ctx, item, s)?;
        let line = dense(&curve);
        roads.push(Road {
            item,
            curve,
            line,
            prof,
            key: road_key(item, index),
        });
    }
    let mut meets = crossings(ctx, &roads)?;
    let ends = t_ends(&roads, &meets, s.f("snap_m"));
    meets.extend(ends);
    let groups = cluster(&meets, &roads);

    // Per road: the junction intervals, merged; `(start, end, junction)` in order.
    let mut cuts: Vec<Vec<(f64, f64, usize)>> = vec![Vec::new(); roads.len()];
    for (j, group) in groups.iter().enumerate() {
        let on: BTreeSet<usize> = group.iter().flat_map(|k| [meets[*k].a, meets[*k].b]).collect();
        for r in on {
            if let Some((a, b)) = cut_interval(group, &meets, &roads, r) {
                cuts[r].push((a, b, j));
            }
            ctx.road_report(&roads[r].key.clone()).junctions += 1;
        }
    }

    let mut out_splines = Bundle::empty();
    // Per junction: `(road, end row vertices, direction away from the junction)`.
    let mut rows: Vec<Vec<(usize, Vec<Vec3>, Vec3)>> = vec![Vec::new(); groups.len()];
    for (r, road) in roads.iter().enumerate() {
        ctx.check_cancel()?;
        let len = road.curve.length_f64();
        ctx.road_report(&road.key.clone()).length_m = road.curve.length();
        let mut list = cuts[r].clone();
        list.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut start = 0.0;
        let mut pieces: Vec<(f64, f64, Option<usize>, Option<usize>)> = Vec::new();
        let mut from: Option<usize> = None;
        for (a, b, j) in &list {
            if *a > start + 1e-3 {
                pieces.push((start, *a, from, Some(*j)));
            }
            start = start.max(*b);
            from = Some(*j);
        }
        if len > start + 1e-3 {
            pieces.push((start, len, from, None));
        }
        for (k, (a, b, first, last)) in pieces.into_iter().enumerate() {
            let piece = road.curve.range(a, b);
            let curve = piece.curve();
            let surf = RoadSurface::new(&curve, &road.prof, ground.as_deref(), smooth, cell_m);
            let row_at = |row: usize| -> Vec<Vec3> {
                surf.xz[row]
                    .iter()
                    .zip(&surf.offsets)
                    .map(|(p, o)| Vec3::new(p.x, surf.center[row] - surf.drop(*o), p.z))
                    .collect()
            };
            let centered = surf.offsets.iter().position(|o| *o == 0.0).unwrap_or(0);
            if let Some(j) = first {
                let r0 = row_at(0);
                let dir = surf.xz[1.min(surf.rows.len() - 1)][centered] - surf.xz[0][centered];
                rows[j].push((r, r0, dir));
            }
            if let Some(j) = last {
                let end = surf.rows.len() - 1;
                let dir = surf.xz[end.saturating_sub(1)][centered] - surf.xz[end][centered];
                rows[j].push((r, row_at(end), dir));
            }
            let mut item = road
                .item
                .with_payload(Payload::Spline(Arc::new(SplineData::new(piece))));
            item.scope
                .insert("piece".to_string(), crate::data::AttrValue::Int(k as i64));
            out_splines.0.push(item);
        }
    }

    let min_arm = s.f("min_arm_deg");
    let mut out_meshes = Bundle::empty();
    let mut built = 0;
    for arms in &rows {
        if arms.is_empty() {
            continue;
        }
        // Any two arms closer than `min_arm_deg`, by the cosine of the angle between them.
        let cos_min = crate::detmath::sin_cos_deg(min_arm).1;
        let sharp = arms.iter().enumerate().any(|(i, (_, _, a))| {
            arms.iter().skip(i + 1).any(|(_, _, b)| {
                let (a, b) = (a.normalize_or_zero(), b.normalize_or_zero());
                f64::from(a.dot(b)) > cos_min
            })
        });
        if sharp {
            ctx.warn(
                "junction_sharp",
                format!("two arms of a junction meet at less than {min_arm}°"),
            );
        }
        let verts: Vec<Vec3> = arms.iter().flat_map(|(_, r, _)| r.iter().copied()).collect();
        if verts.len() < 3 {
            continue;
        }
        let (positions, indices) = patch_triangles(&verts);
        let hub = Vec3::from(positions[verts.len()]);
        if tile.is_some_and(|t| !t.owns(f64::from(hub.x), f64::from(hub.z))) {
            continue;
        }
        let first_road = &roads[arms[0].0].prof;
        let tile_m = first_road.uv_m_per_tile.max(0.01);
        let mesh = GeneratedMesh {
            material: first_road
                .junction
                .material
                .clone()
                .unwrap_or_else(|| first_road.material.clone()),
            uvs: positions.iter().map(|p| [p[0] / tile_m, p[2] / tile_m]).collect(),
            normals: vertex_normals(&positions, &indices),
            positions,
            indices,
            ..GeneratedMesh::default()
        };
        out_meshes.0.push(DataItem::new(Payload::Mesh(Arc::new(MeshData {
            asset: String::new(),
            positions: mesh.positions.clone(),
            normals: mesh.normals.clone(),
            indices: mesh.indices.clone(),
        }))));
        GeneratedMesh::merge_into(&mut ctx.output().meshes, mesh);
        built += 1;
    }
    // Counted where the patch is owned, so a tiled run's cells add up to the junctions.
    ctx.count("junctions", built);
    Ok(vec![out_meshes, out_splines])
}

fn eval_decals(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let tile = ctx.tile();
    let color = s.v3("color").map(|c| c as f32);
    let max_error = s.f("max_error_m") as f32;
    let mut decals: Vec<RoadDecal> = Vec::new();
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let prof = profile_for(ctx, item, s)?;
        let line = sd.curve().to_polyline(max_error);
        for w in line.windows(2) {
            let mid = (w[0] + w[1]) * 0.5;
            if tile.is_some_and(|t| !t.owns(f64::from(mid.x), f64::from(mid.z))) {
                continue;
            }
            decals.push(RoadDecal::new(
                [w[0].x, w[0].z],
                [w[1].x, w[1].z],
                prof.width_m * 0.5,
                color,
            ));
        }
    }
    let col = |f: fn(&RoadDecal) -> f32| AttrColumn::Float(decals.iter().map(|d| f64::from(f(d))).collect());
    let table = Table {
        rows: decals.len(),
        columns: vec![
            ("a_x".to_string(), col(|d| d.a[0])),
            ("a_z".to_string(), col(|d| d.a[1])),
            ("b_x".to_string(), col(|d| d.b[0])),
            ("b_z".to_string(), col(|d| d.b[1])),
            ("half_width".to_string(), col(|d| d.half_width)),
        ],
    };
    ctx.output().road_decals.extend_from_slice(&decals);
    Ok(vec![Bundle::one(DataItem::new(Payload::Table(table)))])
}
