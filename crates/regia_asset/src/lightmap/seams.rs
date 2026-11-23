// SPDX-License-Identifier: MPL-2.0
//! The chart-seam metric (ADR-2024, `docs/formats/lightmap.md` § 2a).
//!
//! A chart seam is where one smooth surface is cut in UV2: the two sides of the cut read different
//! texels, so a bake whose charts disagree shows a line that no lighting explains. For every such
//! edge of an entry, [`seam_report`] reads the lightmap along both sides with the runtime's
//! bilinear lookup ([`Lightmap::sample_entry`]) and takes the relative luminance step,
//! `|La - Lb| / max((La + Lb) / 2, floor)`, where `floor` is 2 % of the entry's mean baked
//! luminance, so a step between two near-black texels does not read as a large one.
//!
//! What counts as a seam needs the source mesh ([`SeamGeometry`]), which the `.flmap` does not
//! hold: positions weld the two sides (a glTF splits a vertex at any normal or UV0 seam), and an
//! edge whose faces bend by more than [`DEFAULT_HARD_EDGE_DEG`] is a real crease, where a step in
//! the light is correct. So `regia bake-lightmaps` measures at bake time and stores the figures in
//! the entry ([`super::LightmapSeams`]), and the seam budget reads them from the header.
//!
//! A chart is a connected set of triangles over the unwrapped vertices (both unwrappers split a
//! vertex at every chart border); charts are numbered by their first triangle. A seam is any edge
//! whose two sides read different UV2, so it usually joins two charts but may join one chart to
//! itself (a tube unrolled along a slit). The result is deterministic: nothing here depends on hash
//! order.

use std::collections::HashMap;

use super::{Coverage, Lightmap, LightmapSeams};

/// A face bend above this many degrees is a crease, not a seam to measure.
pub const DEFAULT_HARD_EDGE_DEG: f32 = 30.0;
/// The default limit on [`LightmapSeams::p95`] (`[lightmaps] seam_budget` in `Regia.toml`).
pub const DEFAULT_SEAM_BUDGET: f32 = 0.25;
/// Largest number of sample points along one seam edge.
const MAX_EDGE_SAMPLES: usize = 16;
/// The luminance floor of the relative step, as a share of the entry's mean baked luminance.
const FLOOR_SHARE: f64 = 0.02;
/// UVs closer than this are one point ([`same_uv`]).
const UV_EPSILON: f32 = 1e-6;
/// Most worst seam segments kept per entry for the editor's overlay.
const MAX_SEGMENTS: usize = 256;

/// The source mesh of an entry, as the bake placed it.
#[derive(Clone, Copy, Debug)]
pub struct SeamGeometry<'a> {
    /// World positions of the source vertices (`xref` indexes them).
    pub positions: &'a [[f32; 3]],
}

/// One chart's seam figures.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct ChartSeam {
    /// Chart number: charts are numbered in the order of their first triangle.
    pub chart: u32,
    /// Seam edges on this chart's border.
    pub edges: u32,
    /// Length-weighted mean step.
    pub mean: f32,
    /// Largest step.
    pub max: f32,
}

/// One seam edge for an overlay, in atlas UV on the side of the lower-numbered chart.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct SeamSegment {
    /// Atlas UV of one end.
    pub a: [f32; 2],
    /// Atlas UV of the other end.
    pub b: [f32; 2],
    /// World position of one end (the source mesh as the bake placed it).
    pub world_a: [f32; 3],
    /// World position of the other end.
    pub world_b: [f32; 3],
    /// The step on this edge.
    pub error: f32,
}

/// What [`seam_report`] measured for one entry.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct EntrySeamReport {
    /// The figures stored in the entry; `None` when it has no seam to measure.
    pub seams: Option<LightmapSeams>,
    /// Every chart with a measured seam, by chart number.
    pub charts: Vec<ChartSeam>,
    /// Edges left out as creases (bend above the limit).
    pub hard_edges: u32,
    /// The worst seam edges, worst first, at most 256.
    pub worst: Vec<SeamSegment>,
}

/// One side of a welded edge: its triangle, chart and the two unwrapped vertices, in welded
/// endpoint order.
#[derive(Clone, Copy)]
struct Side {
    tri: u32,
    a: u32,
    b: u32,
}

fn weld_key(p: [f32; 3]) -> [u32; 3] {
    // +0.0 and -0.0 weld.
    p.map(|v| (v + 0.0).to_bits())
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f64; 3] {
    std::array::from_fn(|k| f64::from(a[k]) - f64::from(b[k]))
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn luminance(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

/// Chart number per triangle: connected components over unwrapped vertices, numbered by first
/// triangle.
fn charts_of(indices: &[u32], vertices: usize) -> Vec<u32> {
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let up = parent[parent[v as usize] as usize];
            parent[v as usize] = up;
            v = up;
        }
        v
    }
    let mut parent: Vec<u32> = (0..vertices as u32).collect();
    let tris = indices.as_chunks::<3>().0;
    for t in tris {
        let root = find(&mut parent, t[0]);
        for &v in &t[1..] {
            let other = find(&mut parent, v);
            parent[other as usize] = root;
        }
    }
    let mut numbers: HashMap<u32, u32> = HashMap::new();
    tris.iter()
        .map(|t| {
            let root = find(&mut parent, t[0]);
            let next = numbers.len() as u32;
            *numbers.entry(root).or_insert(next)
        })
        .collect()
}

/// The length-weighted `q` quantile of `(value, weight)` pairs.
fn weighted_quantile(mut pairs: Vec<(f32, f64)>, q: f64) -> f32 {
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = pairs.iter().map(|p| p.1).sum();
    let mut acc = 0.0;
    for (value, weight) in &pairs {
        acc += weight;
        if acc >= total * q {
            return *value;
        }
    }
    pairs.last().map_or(0.0, |p| p.0)
}

/// Whether two entry-local UVs are the same point: far under a texel at the largest atlas.
fn same_uv(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() < UV_EPSILON && (a[1] - b[1]).abs() < UV_EPSILON
}

/// The mean baked luminance inside `entry`'s rectangle.
fn entry_mean_luminance(map: &Lightmap, entry: usize) -> f64 {
    let Some(e) = map.entries.get(entry) else {
        return 0.0;
    };
    let [x0, y0, w, h] = e.rect;
    // Stacked rows: the entry's page starts `page × height` rows down (ADR-3300).
    let y0 = y0 + e.page * map.height;
    let (mut sum, mut n) = (0.0f64, 0u64);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            if map.coverage_at(x, y) == Coverage::BAKED {
                sum += luminance(map.irradiance_at(x, y));
                n += 1;
            }
        }
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// Measures the seams of `entry` against its source mesh, with `hard_edge_deg` as the crease
/// limit (module docs). An entry out of range, or one whose unwrap names vertices the geometry
/// does not have, reports nothing.
#[must_use]
pub fn seam_report(
    map: &Lightmap,
    entry: usize,
    geometry: SeamGeometry<'_>,
    hard_edge_deg: f32,
) -> EntrySeamReport {
    let _span = tracing::debug_span!("lightmap_seam_report", entry).entered();
    let (Some(_), Some(u)) = (map.entries.get(entry), map.unwraps.get(entry)) else {
        return EntrySeamReport::default();
    };
    let position = |v: u32| geometry.positions.get(u.xref[v as usize] as usize).copied();
    if u.xref.iter().any(|&s| s as usize >= geometry.positions.len()) {
        return EntrySeamReport::default();
    }
    // Welded vertex ids, in first-seen order.
    let mut welds: HashMap<[u32; 3], u32> = HashMap::new();
    let weld: Vec<u32> = (0..u.xref.len() as u32)
        .map(|v| {
            let p = position(v).unwrap_or([0.0; 3]);
            let next = welds.len() as u32;
            *welds.entry(weld_key(p)).or_insert(next)
        })
        .collect();
    measure_seams(map, entry, &weld, &position, hard_edge_deg)
}

/// The seams of `entry` found from the `.flmap` alone (ADR-3120): the two sides of an edge are welded
/// when they copy the same source vertices (`xref`), which is every cut the bake's unwrap made; a
/// cut the source mesh already had (a glTF vertex split at a normal or `TEXCOORD_0` seam) is not
/// seen, and no edge is set aside as a crease (the faces' bend needs the positions). So this is a
/// proxy for [`seam_report`], not the stored figure: the cook compares it on two lightmaps of the
/// same unwrap (a bake and a tier derived from it), where both measure the same edges.
#[must_use]
pub fn seam_report_by_source_vertex(map: &Lightmap, entry: usize) -> EntrySeamReport {
    let _span = tracing::debug_span!("lightmap_seam_report_by_source_vertex", entry).entered();
    let Some(u) = map.unwraps.get(entry) else {
        return EntrySeamReport::default();
    };
    let no_position = |_: u32| Some([0.0f32; 3]);
    measure_seams(map, entry, &u.xref, &no_position, 180.0)
}

/// The shared body of [`seam_report`] and [`seam_report_by_source_vertex`]: `weld` is the welded id of
/// every unwrapped vertex, `position` its world position (the crease test and the segments' world
/// ends).
fn measure_seams(
    map: &Lightmap,
    entry: usize,
    weld: &[u32],
    position: &dyn Fn(u32) -> Option<[f32; 3]>,
    hard_edge_deg: f32,
) -> EntrySeamReport {
    let mut report = EntrySeamReport::default();
    let (Some(e), Some(u)) = (map.entries.get(entry), map.unwraps.get(entry)) else {
        return report;
    };
    let tris = u.indices.as_chunks::<3>().0;
    let chart = charts_of(&u.indices, u.xref.len());
    let mut edges: Vec<((u32, u32), Side)> = Vec::with_capacity(tris.len() * 3);
    for (k, t) in tris.iter().enumerate() {
        for c in 0..3 {
            let (a, b) = (t[c], t[(c + 1) % 3]);
            let (wa, wb) = (weld[a as usize], weld[b as usize]);
            if wa == wb {
                continue;
            }
            let side = Side {
                tri: k as u32,
                a: if wa < wb { a } else { b },
                b: if wa < wb { b } else { a },
            };
            edges.push(((wa.min(wb), wa.max(wb)), side));
        }
    }
    edges.sort_by_key(|(key, side)| (*key, side.tri));
    // At 180 degrees no bend is a crease: the faces are not needed (and may have no positions).
    let no_creases = hard_edge_deg >= 180.0;
    let face = |tri: u32| -> Option<[f64; 3]> {
        if no_creases {
            return Some([0.0, 0.0, 1.0]);
        }
        let t = tris[tri as usize];
        let (p0, p1, p2) = (position(t[0])?, position(t[1])?, position(t[2])?);
        let n = cross(sub(p1, p0), sub(p2, p0));
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        (len > 1e-20).then(|| n.map(|v| v / len))
    };
    let crease_cos = f64::from(hard_edge_deg.to_radians().cos());
    let floor = (entry_mean_luminance(map, entry) * FLOOR_SHARE).max(1e-9);
    let (rw, rh) = (f64::from(e.rect[2]), f64::from(e.rect[3]));
    let mut measured: Vec<(u32, u32, f32, f64, SeamSegment)> = Vec::new();
    let mut at = 0;
    while at < edges.len() {
        let key = edges[at].0;
        let end = at + edges[at..].iter().take_while(|(k, _)| *k == key).count();
        let group = &edges[at..end];
        at = end;
        // Every pair of sides, not only the first against the rest: where closed blocks meet
        // (a wall built as a grid of blocks around an opening) four faces share an edge, two
        // coplanar faces and two inner ones, and the coplanar pair is the seam (ADR-4108).
        for (i, (_, first)) in group.iter().enumerate() {
            let first = *first;
            for (_, other) in &group[i + 1..] {
                // Sides that read the same UV2 are one surface, whatever their vertices. A cut needs no
                // second chart: a tube unrolled into one chart meets itself along its slit.
                if same_uv(u.uv[first.a as usize], u.uv[other.a as usize])
                    && same_uv(u.uv[first.b as usize], u.uv[other.b as usize])
                {
                    continue;
                }
                let (Some(n0), Some(n1)) = (face(first.tri), face(other.tri)) else {
                    continue;
                };
                if n0[0] * n1[0] + n0[1] * n1[1] + n0[2] * n1[2] < crease_cos {
                    report.hard_edges += 1;
                    continue;
                }
                let texels = |s: &Side| {
                    let (pa, pb) = (u.uv[s.a as usize], u.uv[s.b as usize]);
                    let (dx, dy) = (
                        (f64::from(pb[0]) - f64::from(pa[0])) * rw,
                        (f64::from(pb[1]) - f64::from(pa[1])) * rh,
                    );
                    (dx * dx + dy * dy).sqrt()
                };
                let length = texels(&first).max(texels(other));
                let samples = (length.ceil() as usize).clamp(2, MAX_EDGE_SAMPLES);
                let lookup = |s: &Side, t: f32| {
                    let (pa, pb) = (u.uv[s.a as usize], u.uv[s.b as usize]);
                    let local = [pa[0] + (pb[0] - pa[0]) * t, pa[1] + (pb[1] - pa[1]) * t];
                    luminance(map.sample_entry(entry, local))
                };
                let mut sum = 0.0f64;
                for k in 0..samples {
                    let t = (k as f32 + 0.5) / samples as f32;
                    let (la, lb) = (lookup(&first, t), lookup(other, t));
                    sum += (la - lb).abs() / (0.5 * (la + lb)).max(floor);
                }
                let error = (sum / samples as f64) as f32;
                let (ca, cb) = (chart[first.tri as usize], chart[other.tri as usize]);
                let lower = if ca <= cb { &first } else { other };
                let atlas = |v: u32| {
                    let p = u.uv[v as usize];
                    let s = e.scale_offset;
                    [p[0] * s[0] + s[2], p[1] * s[1] + s[3]]
                };
                measured.push((
                    ca,
                    cb,
                    error,
                    length.max(1e-6),
                    SeamSegment {
                        a: atlas(lower.a),
                        b: atlas(lower.b),
                        world_a: position(lower.a).unwrap_or([0.0; 3]),
                        world_b: position(lower.b).unwrap_or([0.0; 3]),
                        error,
                    },
                ));
            }
        }
    }
    if measured.is_empty() {
        return report;
    }
    let total: f64 = measured.iter().map(|m| m.3).sum();
    let mean = measured.iter().map(|m| f64::from(m.2) * m.3).sum::<f64>() / total;
    let p95 = weighted_quantile(measured.iter().map(|m| (m.2, m.3)).collect(), 0.95);
    let mut per_chart: std::collections::BTreeMap<u32, (u32, f64, f64, f32)> =
        std::collections::BTreeMap::new();
    for m in &measured {
        // A chart cut along itself has one seam edge, not two.
        let sides: &[u32] = if m.0 == m.1 { &[m.0] } else { &[m.0, m.1] };
        for &c in sides {
            let slot = per_chart.entry(c).or_insert((0, 0.0, 0.0, 0.0));
            slot.0 += 1;
            slot.1 += f64::from(m.2) * m.3;
            slot.2 += m.3;
            slot.3 = slot.3.max(m.2);
        }
    }
    report.charts = per_chart
        .into_iter()
        .map(|(chart, (edges, weighted, length, max))| ChartSeam {
            chart,
            edges,
            mean: (weighted / length) as f32,
            max,
        })
        .collect();
    let worst = measured
        .iter()
        .max_by(|x, y| x.2.total_cmp(&y.2))
        .map_or(0, |m| m.0.min(m.1));
    let max = measured.iter().map(|m| m.2).fold(0.0f32, f32::max);
    report.seams = Some(LightmapSeams {
        edges: measured.len() as u32,
        mean: mean as f32,
        p95,
        max,
        worst_chart: worst,
    });
    measured.sort_by(|x, y| y.2.total_cmp(&x.2));
    measured.truncate(MAX_SEGMENTS);
    report.worst = measured.into_iter().map(|m| m.4).collect();
    report
}

#[cfg(test)]
mod tests {
    use super::super::{LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap};
    use super::*;

    /// Two unit quads side by side along x, sharing the edge x = 1, each its own chart in a
    /// 12 × 4 atlas: chart A on texels 0..6, chart B on texels 6..12. `a` and `b` fill each side's
    /// irradiance.
    fn two_charts(a: f32, b: f32) -> (Lightmap, Vec<[f32; 3]>) {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [2.0, 0.0, 0.0],
            [2.0, 1.0, 0.0],
        ];
        let unwrap = MeshUnwrap {
            // Vertices 1 and 2 are split along the seam (source vertices shared).
            xref: vec![0, 1, 2, 3, 1, 4, 5, 2],
            uv: vec![
                [1.0 / 12.0, 0.25],
                [5.0 / 12.0, 0.25],
                [5.0 / 12.0, 0.75],
                [1.0 / 12.0, 0.75],
                [7.0 / 12.0, 0.25],
                [11.0 / 12.0, 0.25],
                [11.0 / 12.0, 0.75],
                [7.0 / 12.0, 0.75],
            ],
            indices: vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7],
        };
        let irr: Vec<[f32; 3]> = (0..4)
            .flat_map(|_| 0..12u32)
            .map(|x| if x < 6 { [a; 3] } else { [b; 3] })
            .collect();
        let map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 12,
                height: 4,
                density: 4.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 6,
                    rect: [0, 0, 12, 4],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 2,
                    area_m2: 2.0,
                    texels_per_metre: 4.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![unwrap],
            },
            &irr,
            vec![Coverage::BAKED; 48],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        (map, positions)
    }

    #[test]
    fn matching_sides_measure_zero_and_a_step_measures_its_size() {
        let (same, p) = two_charts(10.0, 10.0);
        let r = seam_report(&same, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        let s = r.seams.expect("one seam");
        assert_eq!(s.edges, 1, "the shared edge x = 1");
        assert!(s.max < 1e-5, "{s:?}");
        assert_eq!(r.charts.len(), 2);

        // 10 lux against 30 lux: |10 - 30| / 20 = 1.
        let (step, p) = two_charts(10.0, 30.0);
        let r = seam_report(&step, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        let s = r.seams.expect("one seam");
        assert!((s.max - 1.0).abs() < 1e-4, "{s:?}");
        assert!((s.mean - 1.0).abs() < 1e-4, "{s:?}");
        assert!((s.p95 - 1.0).abs() < 1e-4, "{s:?}");
        assert_eq!(s.worst_chart, 0);
        assert_eq!(r.worst.len(), 1);
        // The overlay segment lies on chart 0's side of the cut, in atlas UV.
        assert!((r.worst[0].a[0] - 5.0 / 12.0).abs() < 1e-5, "{:?}", r.worst[0]);
        // And the viewport's line lies on the shared edge x = 1 of the mesh.
        assert_eq!((r.worst[0].world_a[0], r.worst[0].world_b[0]), (1.0, 1.0));
        assert_eq!(
            r.charts.iter().map(|c| c.chart).collect::<Vec<_>>(),
            [0, 1],
            "both charts report the seam"
        );
    }

    /// A tube unrolled into one chart: 8 quads in a strip, whose two ends are the same edge of the
    /// mesh in different places of the atlas. `left` and `right` fill the two halves of the strip.
    fn tube(left: f32, right: f32) -> (Lightmap, Vec<[f32; 3]>) {
        const SIDES: u32 = 8;
        let positions: Vec<[f32; 3]> = [0.0f32, 1.0]
            .iter()
            .flat_map(|&z| {
                (0..SIDES).map(move |k| {
                    let a = k as f32 / SIDES as f32 * std::f32::consts::TAU;
                    [a.cos(), a.sin(), z]
                })
            })
            .collect();
        // 9 columns of 2 unwrapped vertices: the last column is the first one's mesh vertices.
        let (mut xref, mut uv, mut indices) = (Vec::new(), Vec::new(), Vec::new());
        for column in 0..=SIDES {
            let u = (1.5 + 2.0 * column as f32) / 20.0;
            xref.extend([column % SIDES, column % SIDES + SIDES]);
            uv.extend([[u, 0.25], [u, 0.75]]);
        }
        for q in 0..SIDES {
            let (a, b, c, d) = (2 * q, 2 * q + 2, 2 * q + 3, 2 * q + 1);
            indices.extend([a, b, c, a, c, d]);
        }
        let irr: Vec<[f32; 3]> = (0..4)
            .flat_map(|_| 0..20u32)
            .map(|x| if x < 10 { [left; 3] } else { [right; 3] })
            .collect();
        let map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 20,
                height: 4,
                density: 4.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "tube".into(),
                    source_vertices: 2 * SIDES,
                    rect: [0, 0, 20, 4],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 5.0,
                    texels_per_metre: 4.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap { xref, uv, indices }],
            },
            &irr,
            vec![Coverage::BAKED; 80],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        (map, positions)
    }

    /// One chart meeting itself along a slit is a seam like any other: it is measured, once, and
    /// the step in the middle of the strip, which no cut explains, is not one.
    #[test]
    fn a_chart_cut_along_itself_has_a_seam() {
        // The octagon bends 45 degrees per side, so the limit must admit the slit's bend.
        const LIMIT: f32 = 60.0;
        let (even, p) = tube(10.0, 10.0);
        let r = seam_report(&even, 0, SeamGeometry { positions: &p }, LIMIT);
        let s = r.seams.expect("the slit");
        assert_eq!(s.edges, 1, "only the slit: every other edge shares its UVs");
        assert!(s.max < 1e-5, "{s:?}");

        // The two ends of the strip read 10 and 30 lux: |10 - 30| / 20 = 1.
        let (step, p) = tube(10.0, 30.0);
        let r = seam_report(&step, 0, SeamGeometry { positions: &p }, LIMIT);
        let s = r.seams.expect("the slit");
        assert_eq!(s.edges, 1);
        assert!((s.max - 1.0).abs() < 1e-3, "{s:?}");
        assert_eq!(s.worst_chart, 0);
        assert_eq!(r.charts.len(), 1);
        assert_eq!(
            r.charts[0].edges, 1,
            "one edge, though both its sides are chart 0"
        );

        // At the default limit the same bend is a crease.
        let r = seam_report(&step, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(r.seams, None);
    }

    /// Two unwrapped vertices at one UV (a split the unwrap kept, with the sides reading the same
    /// texels) are no seam: counting them would dilute the percentile the budget reads.
    #[test]
    fn split_vertices_at_the_same_uv_are_no_seam() {
        let (mut map, p) = two_charts(10.0, 30.0);
        let u = &mut map.unwraps[0];
        u.uv[4] = u.uv[1];
        u.uv[7] = u.uv[2];
        let r = seam_report(&map, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(r.seams, None);
    }

    /// Bending the second quad past the crease limit makes the same edge a real crease: a step in
    /// the light there is correct, so nothing is measured.
    #[test]
    fn a_crease_is_not_a_seam() {
        let (step, mut p) = two_charts(10.0, 30.0);
        p[4] = [1.0, 0.0, 1.0];
        p[5] = [1.0, 1.0, 1.0];
        let r = seam_report(&step, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(r.seams, None);
        assert_eq!(r.hard_edges, 1);
        // A wider limit counts it.
        let r = seam_report(&step, 0, SeamGeometry { positions: &p }, 95.0);
        assert_eq!(r.seams.expect("measured").edges, 1);
    }

    /// Two closed blocks meeting in one plane share the seam edge with their two inner faces,
    /// which come first in triangle order: the coplanar pair is still measured (ADR-4108), and the
    /// inner faces count as hard edges.
    #[test]
    fn a_seam_shared_with_inner_faces_is_measured() {
        let (mut map, mut p) = two_charts(10.0, 30.0);
        p.push([1.0, 0.0, -1.0]);
        p.push([1.0, 1.0, -1.0]);
        map.entries[0].source_vertices = 8;
        let u = &mut map.unwraps[0];
        let first_new = u.xref.len() as u32;
        u.xref.extend_from_slice(&[1, 2, 6, 2, 1, 7]);
        u.uv.extend_from_slice(&[
            [0.5 / 12.0, 0.1],
            [0.5 / 12.0, 0.2],
            [1.5 / 12.0, 0.1],
            [2.5 / 12.0, 0.2],
            [2.5 / 12.0, 0.1],
            [3.5 / 12.0, 0.2],
        ]);
        let mut indices: Vec<u32> = (first_new..first_new + 6).collect();
        indices.extend_from_slice(&u.indices);
        u.indices = indices;
        let r = seam_report(&map, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        let seams = r.seams.expect("the coplanar pair is a seam");
        assert_eq!(seams.edges, 1, "{r:?}");
        // The step 10 | 30 is measured as before.
        let (plain, q) = two_charts(10.0, 30.0);
        let alone = seam_report(&plain, 0, SeamGeometry { positions: &q }, DEFAULT_HARD_EDGE_DEG);
        assert!(
            (seams.p95 - alone.seams.expect("measured").p95).abs() < 1e-6,
            "{r:?}"
        );
        // Inner against inner, and each inner face against each wall face.
        assert_eq!(r.hard_edges, 5, "{r:?}");
    }

    /// Split source vertices with the same positions (a glTF normal or UV0 seam) still weld, so the
    /// seam is found without a shared `xref`.
    #[test]
    fn welded_positions_find_a_seam_without_a_shared_vertex() {
        let (mut map, mut p) = two_charts(10.0, 30.0);
        p.push(p[1]);
        p.push(p[2]);
        map.entries[0].source_vertices = 8;
        map.unwraps[0].xref[4] = 6;
        map.unwraps[0].xref[7] = 7;
        let r = seam_report(&map, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(r.seams.expect("found").edges, 1);
    }

    /// A black entry has no step, and an entry whose geometry does not match reports nothing rather
    /// than reading out of bounds.
    #[test]
    fn edge_cases_report_nothing_or_zero() {
        let (black, p) = two_charts(0.0, 0.0);
        let r = seam_report(&black, 0, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        assert!(r.seams.expect("measured").max.abs() < 1e-6);
        let r = seam_report(
            &black,
            0,
            SeamGeometry { positions: &p[..3] },
            DEFAULT_HARD_EDGE_DEG,
        );
        assert_eq!(r, EntrySeamReport::default());
        let r = seam_report(&black, 9, SeamGeometry { positions: &p }, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(r, EntrySeamReport::default());
    }

    #[test]
    fn the_result_does_not_depend_on_run_order() {
        let (map, p) = two_charts(5.0, 9.0);
        let g = SeamGeometry { positions: &p };
        assert_eq!(
            seam_report(&map, 0, g, DEFAULT_HARD_EDGE_DEG),
            seam_report(&map, 0, g, DEFAULT_HARD_EDGE_DEG)
        );
    }

    /// The seam metric reads the entry's own page (ADR-3300): the same entry moved to page 1 of a
    /// two-page lightmap, with page 0 holding other light, measures the same seams and mean.
    #[test]
    fn an_entry_on_page_1_measures_its_own_page() {
        let (one, p) = two_charts(10.0, 30.0);
        let mut paged = one.clone();
        paged.entries[0].page = 1;
        // Page 0 now holds a bright constant, page 1 the entry's texels.
        let n = one.page_texels();
        let bright = crate::baked_gi::rgb9e5([1000.0 * one.exposure; 3]);
        paged.texels = std::iter::repeat_n(bright, n)
            .chain(one.texels.iter().copied())
            .collect();
        paged.coverage = std::iter::repeat_n(Coverage::BAKED, n)
            .chain(one.coverage.iter().copied())
            .collect();
        paged.validate().expect("two pages");
        let geometry = || SeamGeometry { positions: &p };
        let (a, b) = (
            seam_report(&one, 0, geometry(), DEFAULT_HARD_EDGE_DEG),
            seam_report(&paged, 0, geometry(), DEFAULT_HARD_EDGE_DEG),
        );
        assert_eq!(a.seams, b.seams);
        assert_eq!(entry_mean_luminance(&one, 0), entry_mean_luminance(&paged, 0));
    }
}
