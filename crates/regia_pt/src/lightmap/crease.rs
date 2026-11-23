// SPDX-License-Identifier: MPL-2.0
//! Crease-aware texel sample placement (ADR-2404): a texel whose centre lies inside geometry (a
//! floor running under a wall, a wall's foot inside the slab it stands on) is sampled at the
//! nearest point of its own surface outside that geometry, instead of being dropped and refilled
//! by the gutter dilation.
//!
//! Without it such a texel takes its neighbours' mean, so the light at a crease is extrapolated
//! flat from a texel centre half a texel or more away, and the contact darkening at the joint
//! (a few centimetres wide in a room) is lost to the reconstruction filter. With it the texel
//! holds the light right at the joint, which is what bilinear and bicubic lookups between the
//! texel and its visible neighbour need there.
//!
//! Per texel, deterministic (fixed directions, no random numbers):
//!
//! 1. **Probe.** [`PROBE_RAYS`] cosine-distributed directions over the face (a stratified spiral)
//!    from the texel's ray origin; the share whose first hit is a back face is compared with the
//!    bake's `inside_threshold`, the same test the bake applies to its own hemisphere rays.
//! 2. **Exit.** For a texel inside, [`TANGENT_DIRECTIONS`] rays along the face plane look for the
//!    nearest back face within [`CREASE_REACH`] texels: the side of the solid the texel is in. The
//!    candidate point lies just past it ([`EXIT_MARGIN`] of a texel, at least 1 mm), on the plane.
//! 3. **Accept.** The candidate must lie on a triangle of the texel's own chart (so a texel never
//!    moves off its surface, across a chart border or out of a thin wall onto another surface) and
//!    pass the probe itself. Candidates are tried nearest first; a texel with none stays where it
//!    was, and the bake drops and refills it as before.
//!
//! The reach keeps a moved sample inside the texel's own footprint (its corners are 0.71 texels
//! from the centre), so the texel still stands for its own patch of surface. A moved texel keeps
//! its random stream (keyed by its place in the entry), so the bake stays deterministic and
//! independent of the thread count. The idea of moving a texel sample out of the geometry that
//! covers it, rather than discarding it, follows the conservative-rasterization practice of
//! taking a covered point of the texel footprint when the centre is not usable (Hasselgren,
//! Akenine-Möller and Ohlsson, *Conservative Rasterization*, GPU Gems 2 ch. 42, 2005); the probe
//! and the exit search are this engine's own.

#[cfg(test)]
use std::cell::Cell;

use regia_core::glam::Vec3;

use super::{LightmapScene, Task, basis, cosine_sample, offset_origin};

#[cfg(test)]
thread_local! {
    /// Probe rays `back_share` has cast on this thread.
    static PROBE_RAYS_CAST: Cell<u64> = const { Cell::new(0) };
    /// Chart triangles `locate` has tested on this thread.
    static LOCATE_TRI_TESTS: Cell<u64> = const { Cell::new(0) };
}

/// Probe rays `back_share` has cast on this thread.
#[cfg(test)]
#[must_use]
pub(super) fn crease_probe_rays_cast() -> u64 {
    PROBE_RAYS_CAST.with(Cell::get)
}

/// Chart triangles `locate` has tested on this thread.
#[cfg(test)]
#[must_use]
pub(super) fn crease_locate_tri_tests() -> u64 {
    LOCATE_TRI_TESTS.with(Cell::get)
}

/// Probe rays per texel.
pub(super) const PROBE_RAYS: u32 = 32;
/// In-plane directions searched for the exit.
pub(super) const TANGENT_DIRECTIONS: u32 = 16;
/// Farthest a sample moves, texels (the footprint's corner is 0.71 texels from its centre).
pub(super) const CREASE_REACH: f32 = 0.75;
/// How far past the exit the sample lands, texels (at least [`LEAST_MARGIN_M`]).
pub(super) const EXIT_MARGIN: f32 = 0.02;
/// Least margin past the exit, metres (the border inset of `inset_from_border`).
const LEAST_MARGIN_M: f32 = 1e-3;

/// A moved texel sample: its point, shading normal and face normal.
type Sample = (Vec3, Vec3, Vec3);

/// Where a texel task came from: what the exit test needs to keep a moved sample on its chart.
#[derive(Clone, Copy, Debug)]
pub(super) struct TaskSource {
    /// Entry of the layout (`charts[entry]`).
    pub entry: usize,
    /// Instance in the flattened scene.
    pub instance: usize,
    /// The instance's triangle the texel centre lies on.
    pub tri: u32,
    /// World size of a texel of the entry, metres.
    pub texel_size: f32,
}

/// The chart of each triangle of one instance, so a moved sample stays on its own chart.
pub(super) struct InstanceCharts {
    /// Chart of each triangle of the instance (`u32::MAX` for a degenerate one).
    pub tri_chart: Vec<u32>,
}

impl InstanceCharts {
    /// Keeps `tri_chart` from `denoise::triangle_charts`.
    pub fn new(tri_chart: Vec<u32>) -> Self {
        Self { tri_chart }
    }
}

/// What the placement did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CreaseStats {
    /// Texels the probe found inside geometry.
    pub inside: usize,
    /// Of those, texels moved to a point outside.
    pub relocated: usize,
}

/// `q` is within `tol` of the box. A triangle outside that cannot hold `q` on its plane.
fn point_near_box(min: [f32; 3], max: [f32; 3], q: Vec3, tol: f32) -> bool {
    (0..3).all(|a| q[a] >= min[a] - tol && q[a] <= max[a] + tol)
}

/// A direction of the probe's stratified spiral around `n` (`k` of [`PROBE_RAYS`]).
fn probe_direction(n: Vec3, k: u32) -> Vec3 {
    const GOLDEN: f32 = 0.618_034;
    let u1 = (k as f32 + 0.5) / PROBE_RAYS as f32;
    let u2 = (k as f32 * GOLDEN).fract();
    cosine_sample(n, u1, u2)
}

impl LightmapScene {
    /// Share of the probe's rays from `p` (face `face`) whose first hit is a back face.
    ///
    /// The denominator is [`PROBE_RAYS`], so the comparisons callers make (`share > threshold`
    /// is inside, `share <= threshold` is not) are decided as soon as the share cannot cross
    /// `threshold`. The returned value stays on the same side of `threshold` as the full share.
    fn back_share(&self, p: Vec3, face: Vec3, threshold: f32) -> f32 {
        let o = offset_origin(p, face);
        let mut back = 0u32;
        let total = PROBE_RAYS;
        for k in 0..total {
            let remaining = total - k;
            let share_now = back as f32 / total as f32;
            let share_max = (back + remaining) as f32 / total as f32;
            if share_now > threshold {
                return share_now;
            }
            if share_max <= threshold {
                return share_max;
            }
            #[cfg(test)]
            PROBE_RAYS_CAST.with(|c| c.set(c.get() + 1));
            let d = probe_direction(face, k);
            if d.dot(face) <= 0.0 {
                continue;
            }
            if let Some((ti, ..)) = self.intersect(o, d, f32::INFINITY, false)
                && self.tris[ti].face.dot(d) > 0.0
            {
                back += 1;
            }
        }
        back as f32 / total as f32
    }

    /// The triangle of chart `chart` (`tri_chart[local] == chart`, instance-local, triangles
    /// starting at `first_tri`) that holds `q` within `tol` metres of its plane. On a shared
    /// edge the smallest local index wins, which is the triangle the linear chart scan returned.
    fn locate(
        &self,
        first_tri: usize,
        chart: u32,
        tri_chart: &[u32],
        q: Vec3,
        tol: f32,
    ) -> Option<(usize, [f32; 3])> {
        if self.tris.is_empty() || self.bvh.nodes.is_empty() || tri_chart.is_empty() {
            return None;
        }
        let mut best_local: Option<u32> = None;
        let mut best_bary = [0.0f32; 3];
        let mut stack = super::NodeStack::default();
        stack.push(0);
        while let Some(ni) = stack.pop() {
            let Some(node) = self.bvh.nodes.get(ni as usize) else {
                continue;
            };
            if !point_near_box(node.min, node.max, q, tol) {
                continue;
            }
            if node.is_leaf() {
                let start = node.a;
                let end = start + node.count;
                for j in start..end {
                    let Some(&order) = self.bvh.order.get(j as usize) else {
                        continue;
                    };
                    let ti = order as usize;
                    if ti < first_tri {
                        continue;
                    }
                    let local = (ti - first_tri) as u32;
                    if best_local.is_some_and(|best| local >= best) {
                        continue;
                    }
                    if tri_chart.get(local as usize).copied() != Some(chart) {
                        continue;
                    }
                    #[cfg(test)]
                    LOCATE_TRI_TESTS.with(|c| c.set(c.get() + 1));
                    let Some(t) = self.tris.get(ti) else {
                        continue;
                    };
                    let (e1, e2) = (t.p[1] - t.p[0], t.p[2] - t.p[0]);
                    let n = e1.cross(e2);
                    let nn = n.length_squared();
                    if nn <= 1e-20 {
                        continue;
                    }
                    let rel = q - t.p[0];
                    if (rel.dot(n) / nn.sqrt()).abs() > tol {
                        continue;
                    }
                    let b1 = rel.cross(e2).dot(n) / nn;
                    let b2 = e1.cross(rel).dot(n) / nn;
                    let b0 = 1.0 - b1 - b2;
                    let eps = -1e-5;
                    if b0 >= eps && b1 >= eps && b2 >= eps {
                        best_local = Some(local);
                        best_bary = [b0, b1, b2];
                    }
                }
            } else {
                stack.push(node.a + 1);
                stack.push(node.a);
            }
        }
        best_local.map(|k| (first_tri + k as usize, best_bary))
    }

    /// The moved sample of a texel inside geometry: its point, shading normal and face, or `None`
    /// when no exit within the reach lands on its own chart outside the geometry.
    fn exit_point(
        &self,
        task: &Task,
        src: TaskSource,
        charts: &InstanceCharts,
        threshold: f32,
    ) -> Option<Sample> {
        let inst = self.instances.get(src.instance)?;
        let chart = *charts.tri_chart.get(src.tri as usize)?;
        let face = task.face;
        let (t, b) = basis(face);
        let reach = CREASE_REACH * src.texel_size;
        let margin = (EXIT_MARGIN * src.texel_size).max(LEAST_MARGIN_M);
        let o = offset_origin(task.p, face);
        let mut exits: Vec<(f32, u32, Vec3)> = Vec::new();
        for j in 0..TANGENT_DIRECTIONS {
            let a = std::f32::consts::TAU * j as f32 / TANGENT_DIRECTIONS as f32;
            let d = (t * a.cos() + b * a.sin()).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            if let Some((ti, dist, ..)) = self.intersect(o, d, reach, false)
                && self.tris[ti].face.dot(d) > 0.0
            {
                exits.push((dist, j, d));
            }
        }
        exits.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
        let tol = 1e-4 * src.texel_size.max(1e-3) + 1e-5;
        for (dist, _, d) in exits {
            let q = task.p + d * (dist + margin);
            let Some((ti, bary)) = self.locate(inst.first_tri, chart, &charts.tri_chart, q, tol) else {
                continue;
            };
            let tri = &self.tris[ti];
            let face = if tri.face == Vec3::ZERO {
                task.face
            } else {
                tri.face
            };
            // On the plane exactly, from the barycentrics.
            let q = tri.p[0] * bary[0] + tri.p[1] * bary[1] + tri.p[2] * bary[2];
            if self.back_share(q, face, threshold) > threshold {
                continue;
            }
            let sn = (tri.n[0] * bary[0] + tri.n[1] * bary[1] + tri.n[2] * bary[2]).normalize_or(face);
            let n = if sn.dot(face) < 0.0 { face } else { sn };
            return Some((q, n, face));
        }
        None
    }
}

/// Moves every texel the probe finds inside geometry to its exit (module docs), on `threads`
/// workers. `sources[i]` describes `tasks[i]`; `charts[entry]` the charts of each entry's
/// instance.
pub(super) fn relocate(
    scene: &LightmapScene,
    tasks: &mut [Task],
    sources: &[TaskSource],
    charts: &[InstanceCharts],
    threshold: f32,
    threads: usize,
) -> CreaseStats {
    let _span = tracing::info_span!("lightmap_crease_placement", texels = tasks.len()).entered();
    let total = tasks.len();
    if total == 0 {
        return CreaseStats::default();
    }
    let chunk = total.div_ceil(threads.max(1)).max(1);
    let moved: Vec<Vec<(usize, Option<Sample>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .chunks(chunk)
            .zip(sources.chunks(chunk))
            .enumerate()
            .map(|(c, (part, srcs))| {
                scope.spawn(move || {
                    let mut out = Vec::new();
                    for (i, (task, src)) in part.iter().zip(srcs).enumerate() {
                        if scene.back_share(task.p, task.face, threshold) <= threshold {
                            continue;
                        }
                        let found = charts
                            .get(src.entry)
                            .and_then(|ch| scene.exit_point(task, *src, ch, threshold));
                        out.push((c * chunk + i, found));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    let mut stats = CreaseStats::default();
    for (i, found) in moved.into_iter().flatten() {
        stats.inside += 1;
        if let Some((p, n, face)) = found {
            let task = &mut tasks[i];
            task.p = p;
            task.n = n;
            task.face = face;
            stats.relocated += 1;
        }
    }
    stats
}

/// Least and most the extrapolated light may be, as a share of the edge texel's own.
const EXTRAPOLATION_CLAMP: (f32, f32) = (0.5, 1.5);

/// The first ring of the gutter dilation, crease-aware (ADR-2404): an empty texel next to a chart
/// takes the light's gradient across the chart's edge, `2 · v_edge − v_inner` along each axis where
/// the edge texel `v_edge` and the one behind it `v_inner` are baked texels of one chart, averaged
/// over those axes, instead of its neighbours' mean. Both lookups read it: bilinear between the
/// edge texel and the ring only outside the surface, the bicubic B-spline (whose weights reach one
/// texel past the edge) then reproduces the edge texel's light at the edge instead of pulling it
/// toward the inner one, which is what a crease's contact darkening and a chart border need.
/// Irradiance (channels 0 to 2) is clamped to [`EXTRAPOLATION_CLAMP`] of the edge texel's, every
/// channel of a directional bake beyond it takes the edge texels' mean, and a texel with no such
/// axis is left to the plain dilation. Returns the texels filled (marked `Coverage::DILATED`).
pub(super) fn extrapolate_first_ring<const N: usize>(
    texels: &mut [[f32; N]],
    coverage: &mut [u8],
    chart_of: &[u32],
    w: usize,
    h: usize,
) -> usize {
    use regia_asset::lightmap::Coverage;
    let _span = tracing::info_span!("lightmap_gutter_extrapolation", w, h).entered();
    let baked = |i: usize| coverage[i] == Coverage::BAKED;
    let mut out: Vec<(usize, [f32; N])> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if coverage[i] != Coverage::EMPTY {
                continue;
            }
            let mut sum = [0.0f32; N];
            let mut n = 0u32;
            for (dx, dy) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                let (ex, ey) = (x as i64 + dx, y as i64 + dy);
                let (kx, ky) = (x as i64 + 2 * dx, y as i64 + 2 * dy);
                if kx < 0 || ky < 0 || kx >= w as i64 || ky >= h as i64 {
                    continue;
                }
                let (e, k) = (ey as usize * w + ex as usize, ky as usize * w + kx as usize);
                if !(baked(e) && baked(k)) || chart_of[e] != chart_of[k] || chart_of[e] == u32::MAX {
                    continue;
                }
                for c in 0..N {
                    let (ve, vk) = (texels[e][c], texels[k][c]);
                    sum[c] += if c < 3 {
                        (2.0 * ve - vk).clamp(EXTRAPOLATION_CLAMP.0 * ve, EXTRAPOLATION_CLAMP.1 * ve)
                    } else {
                        ve
                    };
                }
                n += 1;
            }
            if n > 0 {
                out.push((i, sum.map(|v| v / n as f32)));
            }
        }
    }
    let filled = out.len();
    for (i, v) in out {
        texels[i] = v;
        coverage[i] = Coverage::DILATED;
    }
    filled
}

#[cfg(test)]
mod tests {
    use regia_asset::lightmap::{Coverage, LightmapEntry, LightmapLayout, MeshUnwrap};
    use regia_core::glam::{Mat4, Vec3};

    use super::super::{LightmapBakeSettings, LightmapScene, bake_lightmap};
    use crate::{PtCamera, PtMaterial, PtMesh, PtScene, PtSky};

    /// Texels across the floor's interior (one gutter texel each side): 0.25 m texels on 4 m.
    const RES: u32 = 18;

    /// A closed axis-aligned box with outward faces.
    fn block(lo: [f32; 3], hi: [f32; 3]) -> PtMesh {
        let c = [
            (lo[0] + hi[0]) * 0.5,
            (lo[1] + hi[1]) * 0.5,
            (lo[2] + hi[2]) * 0.5,
        ];
        let e = [
            (hi[0] - lo[0]) * 0.5,
            (hi[1] - lo[1]) * 0.5,
            (hi[2] - lo[2]) * 0.5,
        ];
        // (normal axis, sign, u axis, v axis) with u × v = the outward normal.
        let faces = [
            (0, 1.0, 1, 2),
            (0, -1.0, 2, 1),
            (1, 1.0, 2, 0),
            (1, -1.0, 0, 2),
            (2, 1.0, 0, 1),
            (2, -1.0, 1, 0),
        ];
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut indices = Vec::new();
        for (axis, sign, u, v) in faces {
            let base = positions.len() as u32;
            for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let mut p = c;
                p[axis] += sign * e[axis];
                p[u] += su * e[u];
                p[v] += sv * e[v];
                positions.push(p);
                let mut n = [0.0; 3];
                n[axis] = sign;
                normals.push(n);
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        PtMesh {
            positions,
            normals,
            indices,
            ..PtMesh::default()
        }
    }

    /// A 4 m floor at y = 0 under a uniform sky, with a black block standing on it across its
    /// whole width: z from -1.0 to 0.05, 0.25 m high. Atlas row `y` has its centre at
    /// z = -2.125 + 0.25 y (row 0 is the gutter): rows 5 to 8 lie under the block, rows 5 and 8
    /// within the reach of its faces (0.125 m and 0.175 m), rows 6 and 7 deeper.
    fn scene() -> LightmapScene {
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 5.0, 5.0], [0.0; 3], 1.0, 4, 4));
        let floor = s.add_mesh(PtMesh {
            positions: vec![
                [-2.0, 0.0, -2.0],
                [2.0, 0.0, -2.0],
                [2.0, 0.0, 2.0],
                [-2.0, 0.0, 2.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            ..PtMesh::default()
        });
        let grey = s.add_material(PtMaterial::lambert([0.5; 3]));
        s.add_instance(floor, grey, Mat4::IDENTITY);
        let wall = s.add_mesh(block([-2.0, 0.0, -1.0], [2.0, 0.25, 0.05]));
        let black = s.add_material(PtMaterial::lambert([0.0; 3]));
        s.add_instance(wall, black, Mat4::IDENTITY);
        s.sky = PtSky::Uniform([1.0; 3]);
        LightmapScene::from_pt_scene(&s).expect("scene")
    }

    fn layout() -> LightmapLayout {
        let g = 1.0 / RES as f32;
        let (lo, hi) = (g, 1.0 - g);
        LightmapLayout {
            width: RES,
            height: RES,
            density: 4.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "floor".into(),
                source_vertices: 4,
                rect: [0, 0, RES, RES],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 16.0,
                texels_per_metre: 4.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[lo, lo], [hi, lo], [hi, hi], [lo, hi]],
                indices: vec![0, 2, 1, 0, 3, 2],
            }],
        }
    }

    fn settings(crease: bool, threads: usize) -> LightmapBakeSettings {
        LightmapBakeSettings {
            samples: 512,
            bounces: 1,
            seed: 5,
            filter: false,
            threads,
            directional: false,
            crease_placement: crease,
            ..LightmapBakeSettings::default()
        }
    }

    /// Mean red irradiance of atlas row `y` over the interior columns.
    fn row_mean(map: &regia_asset::lightmap::Lightmap, y: u32) -> f32 {
        (1..RES - 1).map(|x| map.irradiance_at(x, y)[0]).sum::<f32>() / (RES - 2) as f32
    }

    #[test]
    fn texels_under_a_wall_within_reach_sample_its_foot_and_deeper_ones_stay_dropped() {
        let scene = scene();
        let (old, old_report) =
            bake_lightmap(&scene, layout(), &[0], &settings(false, 2), |_, _| {}).expect("bake");
        let (new, report) =
            bake_lightmap(&scene, layout(), &[0], &settings(true, 2), |_, _| {}).expect("bake");
        let cols = (RES - 2) as usize;
        assert_eq!(old_report.texels_crease_inside, 0);
        assert_eq!(old_report.texels_inside, 4 * cols, "{old_report:?}");
        assert_eq!(report.texels_crease_inside, 4 * cols, "{report:?}");
        assert_eq!(report.texels_relocated, 2 * cols, "rows 5 and 8: {report:?}");
        assert_eq!(
            report.texels_inside,
            2 * cols,
            "rows 6 and 7 stay dropped: {report:?}"
        );
        for x in 1..RES - 1 {
            assert_eq!(old.coverage_at(x, 8), Coverage::DILATED);
            assert_eq!(
                new.coverage_at(x, 8),
                Coverage::BAKED,
                "row 8 is baked at the wall's foot"
            );
            assert_eq!(new.coverage_at(x, 5), Coverage::BAKED, "row 5 at the back foot");
            assert_eq!(new.coverage_at(x, 6), Coverage::DILATED);
            assert_eq!(new.coverage_at(x, 7), Coverage::DILATED);
        }
        // The relocated row holds the light at the foot (half the sky hidden by the 0.25 m
        // block), darker than the copy of row 9 (0.075 m out) the dilation gave it before.
        let (at_foot, copied) = (row_mean(&new, 8), row_mean(&old, 8));
        assert!(at_foot < 0.9 * copied, "foot {at_foot} vs dilated {copied}");
        let pi = std::f32::consts::PI;
        assert!(
            (at_foot / pi - 0.5).abs() < 0.05,
            "about half the sky at the foot: {}",
            at_foot / pi
        );
        // Texels outside the block bake exactly as before.
        for y in [1u32, 4, 9, 12, 16] {
            for x in 1..RES - 1 {
                assert_eq!(new.irradiance_at(x, y), old.irradiance_at(x, y), "row {y}");
            }
        }
    }

    /// The first gutter ring continues the light's gradient across a chart edge, clamped to half
    /// and one and a half times the edge texel, and never reads another chart.
    #[test]
    fn the_first_ring_continues_the_gradient_across_the_edge() {
        // One row of 8: empty, chart 0 at columns 1-4 with 4, 3, 2, 1 (a ramp falling to the right),
        // empty at 5, chart 1 at columns 6-7.
        let (w, h) = (8usize, 1usize);
        let mut texels: Vec<[f32; 3]> = vec![
            [0.0; 3], [4.0; 3], [3.0; 3], [2.0; 3], [1.0; 3], [0.0; 3], [10.0; 3], [10.0; 3],
        ];
        let mut cov = vec![
            Coverage::EMPTY,
            Coverage::BAKED,
            Coverage::BAKED,
            Coverage::BAKED,
            Coverage::BAKED,
            Coverage::EMPTY,
            Coverage::BAKED,
            Coverage::BAKED,
        ];
        let chart = vec![u32::MAX, 0, 0, 0, 0, u32::MAX, 1, 1];
        let filled = super::extrapolate_first_ring(&mut texels, &mut cov, &chart, w, h);
        assert_eq!(filled, 2);
        // Left of the ramp: 2·4 − 3 = 5 (within 1.5 × 4).
        assert_eq!(texels[0], [5.0; 3]);
        // Column 5 sees the ramp's end (2·1 − 2 = 0, clamped to 0.5) and chart 1 (10): the mean.
        assert_eq!(texels[5], [(0.5 + 10.0) / 2.0; 3]);
        assert!(cov.iter().all(|&c| c != Coverage::EMPTY));
        assert_eq!(cov[0], Coverage::DILATED);
    }

    #[test]
    fn the_placement_is_deterministic_across_thread_counts() {
        let scene = scene();
        let (a, ra) = bake_lightmap(&scene, layout(), &[0], &settings(true, 1), |_, _| {}).expect("bake");
        let (b, rb) = bake_lightmap(&scene, layout(), &[0], &settings(true, 5), |_, _| {}).expect("bake");
        assert_eq!(ra, rb);
        assert_eq!(a.texels, b.texels);
        assert_eq!(a.coverage, b.coverage);
    }

    #[test]
    fn a_texel_never_leaves_its_chart() {
        // The block's far face is at the floor's edge: rows past the floor's end do not exist, so
        // the exits found along +z and -z are the only ones, and an exit off the floor (past
        // x = ±2) is refused. Every relocated position stays on the floor.
        let scene = scene();
        let mut tasks = Vec::new();
        let p = super::super::prepare_tasks(&scene, &layout(), &[0], &settings(true, 2)).expect("prepare");
        for t in &p.tasks {
            assert!(t.p.y.abs() < 1e-6, "{:?}", t.p);
            assert!(t.p.x.abs() <= 2.0 && t.p.z.abs() <= 2.0, "{:?}", t.p);
            if (-1.0..0.05).contains(&t.p.z) {
                tasks.push(t.p);
            }
        }
        // The kept-inside texels (rows 6 and 7) still have their centres under the block; no
        // relocated one does.
        assert_eq!(tasks.len(), 2 * (RES - 2) as usize);
        assert_eq!(p.crease.relocated, 2 * (RES - 2) as usize);
    }

    /// A point inside a closed box is inside after fewer than [`super::PROBE_RAYS`] rays: the
    /// share's denominator is fixed, so the comparison is decided before the spiral ends.
    #[test]
    fn an_inside_point_stops_the_probe_before_the_last_ray() {
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 5.0, 5.0], [0.0; 3], 1.0, 4, 4));
        let mesh = s.add_mesh(block([-1.0; 3], [1.0; 3]));
        let material = s.add_material(PtMaterial::lambert([0.5; 3]));
        s.add_instance(mesh, material, Mat4::IDENTITY);
        s.sky = PtSky::Uniform([1.0; 3]);
        let scene = LightmapScene::from_pt_scene(&s).expect("box");
        let before = super::crease_probe_rays_cast();
        let share = scene.back_share(Vec3::ZERO, Vec3::Y, 0.5);
        let rays = super::crease_probe_rays_cast() - before;
        assert!(share > 0.5, "share {share}");
        assert!(rays < super::PROBE_RAYS as u64, "cast {rays}");
    }

    /// Locating a point on the last triangle of a large chart does not test every triangle.
    #[test]
    fn locate_finds_the_last_triangle_without_scanning_the_chart() {
        let n = 16u32;
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut indices = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                positions.push([x as f32, 0.0, z as f32]);
                normals.push([0.0, 1.0, 0.0]);
            }
        }
        let w = n + 1;
        for z in 0..n {
            for x in 0..n {
                let i = z * w + x;
                indices.extend_from_slice(&[i, i + w, i + 1, i + 1, i + w, i + w + 1]);
            }
        }
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 5.0, 5.0], [0.0; 3], 1.0, 4, 4));
        let mesh = s.add_mesh(PtMesh {
            positions,
            normals,
            indices,
            ..PtMesh::default()
        });
        let material = s.add_material(PtMaterial::lambert([0.5; 3]));
        s.add_instance(mesh, material, Mat4::IDENTITY);
        let scene = LightmapScene::from_pt_scene(&s).expect("grid");
        let last = scene.tris.len() - 1;
        let tri = &scene.tris[last];
        let q = (tri.p[0] + tri.p[1] + tri.p[2]) / 3.0;
        let charts = vec![0u32; scene.tris.len()];
        let before = super::crease_locate_tri_tests();
        let (found, _) = scene
            .locate(0, 0, &charts, q, 1e-3)
            .expect("on the last triangle");
        let tests = super::crease_locate_tri_tests() - before;
        assert_eq!(found, last);
        assert!(
            tests < scene.tris.len() as u64 / 4,
            "tested {tests} of {}",
            scene.tris.len()
        );
    }
}
