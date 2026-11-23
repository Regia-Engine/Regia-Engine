// SPDX-License-Identifier: MPL-2.0
//! A collision-free flight corridor through scatter (ADR-1503): given a camera path, report the
//! clearance per segment to every trunk, crown and the terrain, and carve a corridor where the
//! path comes too close.
//!
//! - **Report** ([`corridor_report`]): the path is split into pieces no longer than
//!   [`CorridorSpec::max_piece_m`]; every scatter instance within [`CorridorSpec::search_m`] of a
//!   piece is measured (trunk capsule, crown spheroid, [`super::shape`]); the terrain is sampled
//!   every [`CorridorSpec::terrain_step_m`]. Each input segment gets its smallest clearance, the
//!   nearest obstacle, how many instances are closer than [`CorridorSpec::clearance_m`], the
//!   height above ground, the canopy cover overhead (gap finding) and the lift that would clear
//!   the ground.
//! - **Carve** ([`carve_corridor`]): per class, consecutive violating pieces become one
//!   [`ScatterExclusion`] along the path (simplified, Douglas & Peucker 1973) whose radius
//!   reaches the farthest violator's ground point. Exclusions are a field of the frozen
//!   `.scatter.ron` v2 format, and placement only drops the candidates an exclusion covers, so a
//!   carve never moves any other instance and the carved set loads in every existing build.
//!   Terrain violations cannot be carved; the report says how far to lift the path instead.
//!
//! Everything is deterministic: the same set, terrain and path give the same report and carve.

use std::collections::{HashMap, HashSet};

use super::super::set::{ScatterExclusion, ScatterSet};
use super::query::{ScatterHit, ScatterQuery, segment_clearance};
use super::shape::ObstaclePart;

/// Added to every carve radius, metres: the exclusion test is `<=` in `f32`, so a violator
/// exactly on the radius is still removed.
pub const CARVE_MARGIN_M: f32 = 0.05;

/// How a corridor is measured and carved.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CorridorSpec {
    /// Clearance the camera needs from any trunk or crown surface, metres.
    pub clearance_m: f32,
    /// Height the camera needs above the terrain, metres.
    pub ground_clearance_m: f32,
    /// Obstacles are measured out to this distance from the path (the nearest is reported even
    /// when it is not a violation), metres. At least the clearance.
    pub search_m: f32,
    /// The path is measured in pieces at most this long, metres.
    pub max_piece_m: f32,
    /// Terrain and canopy samples along the path, metres apart.
    pub terrain_step_m: f32,
    /// Classes to measure by name (empty = every class).
    pub classes: Vec<String>,
    /// Carved exclusion polylines are simplified to this tolerance, metres (added to the radius).
    pub simplify_m: f32,
}

impl Default for CorridorSpec {
    fn default() -> Self {
        Self {
            clearance_m: 2.0,
            ground_clearance_m: 2.0,
            search_m: 12.0,
            max_piece_m: 8.0,
            terrain_step_m: 2.0,
            classes: Vec::new(),
            simplify_m: 0.25,
        }
    }
}

impl CorridorSpec {
    /// Finite values in range (`search_m >= clearance_m`, pieces and steps at least 0.25 m).
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let pos = |v: f32, d: f32| if v.is_finite() && v >= 0.0 { v } else { d };
        let d = Self::default();
        self.clearance_m = pos(self.clearance_m, d.clearance_m);
        self.ground_clearance_m = pos(self.ground_clearance_m, d.ground_clearance_m);
        self.search_m = pos(self.search_m, d.search_m).max(self.clearance_m);
        self.max_piece_m = pos(self.max_piece_m, d.max_piece_m).max(0.25);
        self.terrain_step_m = pos(self.terrain_step_m, d.terrain_step_m).max(0.25);
        self.simplify_m = pos(self.simplify_m, d.simplify_m);
        self
    }
}

/// The nearest obstacle of a segment.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct NearestObstacle {
    /// Class name.
    pub class: String,
    /// Stable instance id ([`ScatterHit::id`]).
    pub id: u64,
    /// `trunk` or `crown`.
    pub part: ObstaclePart,
    /// Ground contact of the instance.
    pub position: [f32; 3],
    /// Height of the instance, metres.
    pub height: f32,
    /// Clearance from the path, metres (negative = the path passes through it).
    pub distance: f32,
}

/// Clearance data of one segment of the input path.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SegmentClearance {
    /// Index of the segment (points `index` → `index + 1`).
    pub index: u32,
    /// Start point, world metres.
    pub start: [f32; 3],
    /// End point, world metres.
    pub end: [f32; 3],
    /// Length, metres.
    pub length_m: f32,
    /// Speed over the segment when the path has times, metres per second.
    pub speed_m_s: Option<f32>,
    /// Smallest clearance to any measured obstacle surface, metres (`None`: nothing within
    /// [`CorridorSpec::search_m`]).
    pub clearance_m: Option<f32>,
    /// The obstacle at that clearance.
    pub nearest: Option<NearestObstacle>,
    /// Instances closer than [`CorridorSpec::clearance_m`].
    pub violations: u32,
    /// Smallest height above the terrain, metres (negative = below ground).
    pub ground_clearance_m: f32,
    /// The path comes closer to the ground than [`CorridorSpec::ground_clearance_m`].
    pub terrain_violation: bool,
    /// Share of the samples with a crown overhead (0 = open sky, 1 = under canopy all the way).
    pub canopy_cover: f32,
    /// Lift (metres) that would give the segment its ground clearance (0 when it has it).
    pub suggested_lift_m: f32,
    /// No obstacle and no terrain violation.
    pub ok: bool,
}

/// What [`corridor_report`] found along a path.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct CorridorReport {
    /// One entry per path segment.
    pub segments: Vec<SegmentClearance>,
    /// Segments with an obstacle or terrain violation.
    pub violating_segments: u32,
    /// Distinct instances closer than the clearance anywhere on the path.
    pub obstacle_violations: u32,
    /// Segments too close to the ground.
    pub terrain_violations: u32,
    /// Smallest obstacle clearance on the whole path (`None`: nothing within the search).
    pub min_clearance_m: Option<f32>,
    /// Smallest height above ground on the whole path.
    pub min_ground_clearance_m: f32,
    /// Path length, metres.
    pub length_m: f32,
    /// Mean canopy cover over the path (per metre).
    pub canopy_cover: f32,
    /// Instances measured (each counted once).
    pub instances_measured: u32,
}

/// A camera path: points in order, and optionally the time (seconds) at each.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CameraPath {
    /// World points, metres.
    pub points: Vec<[f32; 3]>,
    /// Seconds at each point (same length as `points`), when known.
    #[serde(default)]
    pub times: Option<Vec<f32>>,
}

/// One piece of the path (a segment or part of one).
#[derive(Clone, Copy)]
struct Piece {
    a: [f32; 3],
    b: [f32; 3],
    segment: u32,
}

/// Per class: the violators of each piece, by piece index.
type Violators = HashMap<u16, Vec<(usize, ScatterHit)>>;

/// Placement cells already measured, per class (each cell placed once per report).
struct CellCache<'q> {
    query: &'q ScatterQuery,
    cells: HashMap<(u16, i32, i32), Vec<ScatterHit>>,
    measured: HashSet<u64>,
}

impl<'q> CellCache<'q> {
    fn new(query: &'q ScatterQuery) -> Self {
        Self {
            query,
            cells: HashMap::new(),
            measured: HashSet::new(),
        }
    }

    fn visit(
        &mut self,
        class: u16,
        min: [f32; 2],
        max: [f32; 2],
        margin: f32,
        mut f: impl FnMut(&ScatterHit),
    ) {
        let Some((lo, hi)) = self.query.cell_range(class, min, max, margin) else {
            return;
        };
        for cz in lo[1]..=hi[1] {
            for cx in lo[0]..=hi[0] {
                let query = self.query;
                let measured = &mut self.measured;
                let hits = self.cells.entry((class, cx, cz)).or_insert_with(|| {
                    let mut v = Vec::new();
                    query.cell_hits(class, cx, cz, &mut v);
                    measured.extend(v.iter().map(|h| h.id));
                    v
                });
                for h in hits.iter() {
                    f(h);
                }
            }
        }
    }
}

fn length(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn pieces(path: &CameraPath, max_piece: f32) -> Vec<Piece> {
    let mut out = Vec::new();
    for (i, w) in path.points.windows(2).enumerate() {
        let n = (length(w[0], w[1]) / max_piece).ceil().max(1.0) as u32;
        for k in 0..n {
            out.push(Piece {
                a: lerp(w[0], w[1], k as f32 / n as f32),
                b: lerp(w[0], w[1], (k + 1) as f32 / n as f32),
                segment: i as u32,
            });
        }
    }
    out
}

fn class_filter(query: &ScatterQuery, spec: &CorridorSpec) -> Vec<u16> {
    (0..query.set().classes.len() as u16)
        .filter(|&c| spec.classes.is_empty() || spec.classes.iter().any(|n| n == query.class_name(c)))
        .collect()
}

/// Everything one pass over the path measures (the report and the carve share it).
struct Measured {
    report: CorridorReport,
    pieces: Vec<Piece>,
    violators: Violators,
}

fn measure(query: &ScatterQuery, path: &CameraPath, spec: &CorridorSpec) -> Measured {
    let spec = spec.clone().sanitized();
    let pieces = pieces(path, spec.max_piece_m);
    let classes = class_filter(query, &spec);
    let map = &query.source().map;
    let mut cache = CellCache::new(query);
    let segment_count = path.points.len().saturating_sub(1);
    let mut segments: Vec<SegmentClearance> = path
        .points
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            let speed = path.times.as_ref().and_then(|t| {
                let dt = t.get(i + 1)? - t.get(i)?;
                (dt > 0.0).then(|| length(w[0], w[1]) / dt)
            });
            SegmentClearance {
                index: i as u32,
                start: w[0],
                end: w[1],
                length_m: length(w[0], w[1]),
                speed_m_s: speed,
                clearance_m: None,
                nearest: None,
                violations: 0,
                ground_clearance_m: f32::INFINITY,
                terrain_violation: false,
                canopy_cover: 0.0,
                suggested_lift_m: 0.0,
                ok: true,
            }
        })
        .collect();
    let mut seg_violators: Vec<HashSet<u64>> = vec![HashSet::new(); segment_count];
    let mut all_violators: HashSet<u64> = HashSet::new();
    let mut violators: Violators = HashMap::new();
    let mut cover_samples = vec![(0u32, 0u32); segment_count];
    let mut covered_m = 0.0f32;
    let mut total_m = 0.0f32;
    for (pi, piece) in pieces.iter().enumerate() {
        let seg = &mut segments[piece.segment as usize];
        let min = [piece.a[0].min(piece.b[0]), piece.a[2].min(piece.b[2])];
        let max = [piece.a[0].max(piece.b[0]), piece.a[2].max(piece.b[2])];
        let len = length(piece.a, piece.b);
        let samples = (len / spec.terrain_step_m).ceil().max(1.0) as u32;
        let points: Vec<[f32; 3]> = (0..=samples)
            .map(|k| lerp(piece.a, piece.b, k as f32 / samples as f32))
            .collect();
        let mut covered = vec![false; points.len()];
        for &class in &classes {
            cache.visit(class, min, max, spec.search_m, |hit| {
                let (d, part, _) = segment_clearance(&hit.obstacle, piece.a, piece.b);
                if d <= spec.search_m && seg.clearance_m.is_none_or(|c| d < c) {
                    seg.clearance_m = Some(d);
                    seg.nearest = Some(NearestObstacle {
                        class: query.class_name(hit.class).to_string(),
                        id: hit.id,
                        part,
                        position: hit.position,
                        height: hit.height,
                        distance: d,
                    });
                }
                if d < spec.clearance_m {
                    if seg_violators[piece.segment as usize].insert(hit.id) {
                        seg.violations += 1;
                    }
                    all_violators.insert(hit.id);
                    violators.entry(class).or_default().push((pi, *hit));
                }
                for (c, p) in covered.iter_mut().zip(&points) {
                    *c |= hit.obstacle.covers(*p);
                }
            });
        }
        for (k, p) in points.iter().enumerate() {
            let h = p[1] - map.sample_bilinear(p[0], p[2]);
            seg.ground_clearance_m = seg.ground_clearance_m.min(h);
            // The first sample of a piece is the last of the one before: count it once.
            if k > 0 || pi == 0 || pieces[pi - 1].segment != piece.segment {
                let entry = &mut cover_samples[piece.segment as usize];
                entry.0 += u32::from(covered[k]);
                entry.1 += 1;
            }
        }
        let cover = covered.iter().filter(|c| **c).count() as f32 / covered.len() as f32;
        covered_m += cover * len;
        total_m += len;
    }
    let mut report = CorridorReport {
        min_ground_clearance_m: f32::INFINITY,
        ..CorridorReport::default()
    };
    for (seg, (covered, n)) in segments.iter_mut().zip(&cover_samples) {
        seg.canopy_cover = if *n > 0 { *covered as f32 / *n as f32 } else { 0.0 };
        seg.terrain_violation = seg.ground_clearance_m < spec.ground_clearance_m;
        seg.suggested_lift_m = (spec.ground_clearance_m - seg.ground_clearance_m).max(0.0);
        seg.ok = seg.violations == 0 && !seg.terrain_violation;
        report.violating_segments += u32::from(!seg.ok);
        report.terrain_violations += u32::from(seg.terrain_violation);
        if let Some(c) = seg.clearance_m {
            report.min_clearance_m = Some(report.min_clearance_m.map_or(c, |m: f32| m.min(c)));
        }
        report.min_ground_clearance_m = report.min_ground_clearance_m.min(seg.ground_clearance_m);
        report.length_m += seg.length_m;
    }
    if segments.is_empty() {
        report.min_ground_clearance_m = 0.0;
    }
    report.obstacle_violations = all_violators.len() as u32;
    report.canopy_cover = if total_m > 0.0 { covered_m / total_m } else { 0.0 };
    report.instances_measured = cache.measured.len() as u32;
    report.segments = segments;
    Measured {
        report,
        pieces,
        violators,
    }
}

/// Measures the clearance of `path` through `query`'s scatter and terrain, per segment.
#[must_use]
pub fn corridor_report(query: &ScatterQuery, path: &CameraPath, spec: &CorridorSpec) -> CorridorReport {
    let _span = tracing::info_span!(target: "regia_terrain", "forest_corridor_report").entered();
    measure(query, path, spec).report
}

/// The exclusions of one class that open the corridor.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ClassCarve {
    /// Class name.
    pub class: String,
    /// Exclusion polylines to append to the class's `exclusions`.
    pub exclusions: Vec<ScatterExclusion>,
    /// Instances closer than the clearance (all removed by the exclusions).
    pub violators: u32,
    /// Instances the exclusions remove in total (violators plus the ones that share their
    /// stretch of corridor).
    pub removed: u32,
}

/// A carve: per class, the exclusions that open the corridor, and the report before it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CorridorCarve {
    /// One entry per class that needs a carve.
    pub classes: Vec<ClassCarve>,
    /// The report the carve was computed from.
    pub before: CorridorReport,
}

impl CorridorCarve {
    /// Appends the exclusions to the classes of `set` by name; returns how many were added.
    pub fn apply(&self, set: &mut ScatterSet) -> usize {
        let mut added = 0;
        for carve in &self.classes {
            if let Some(class) = set.classes.iter_mut().find(|c| c.name == carve.class) {
                class.exclusions.extend(carve.exclusions.iter().cloned());
                added += carve.exclusions.len();
            }
        }
        added
    }

    /// Instances the carve removes over every class.
    #[must_use]
    pub fn removed(&self) -> u32 {
        self.classes.iter().map(|c| c.removed).sum()
    }
}

/// Computes the exclusions that clear every obstacle violation of `path` (see the module docs).
#[must_use]
pub fn carve_corridor(query: &ScatterQuery, path: &CameraPath, spec: &CorridorSpec) -> CorridorCarve {
    let _span = tracing::info_span!(target: "regia_terrain", "forest_corridor_carve").entered();
    let spec = spec.clone().sanitized();
    let measured = measure(query, path, &spec);
    let mut classes: Vec<u16> = measured.violators.keys().copied().collect();
    classes.sort_unstable();
    let mut out = Vec::new();
    for class in classes {
        let Some(list) = measured.violators.get(&class) else {
            continue;
        };
        let mut by_piece: Vec<Vec<&ScatterHit>> = vec![Vec::new(); measured.pieces.len()];
        for (pi, hit) in list {
            by_piece[*pi].push(hit);
        }
        let mut exclusions = Vec::new();
        let mut unique: HashSet<u64> = HashSet::new();
        let mut pi = 0;
        while pi < by_piece.len() {
            if by_piece[pi].is_empty() {
                pi += 1;
                continue;
            }
            let start = pi;
            while pi < by_piece.len() && !by_piece[pi].is_empty() {
                pi += 1;
            }
            // One piece of margin on each side: the violator's ground point may lie past the
            // piece its crown overhangs.
            let lo = start.saturating_sub(1);
            let hi = pi.min(by_piece.len() - 1);
            let mut line: Vec<[f32; 2]> = vec![[measured.pieces[lo].a[0], measured.pieces[lo].a[2]]];
            line.extend(measured.pieces[lo..=hi].iter().map(|p| [p.b[0], p.b[2]]));
            let line = simplify(&line, spec.simplify_m);
            let mut radius = 0.0f32;
            for hits in &by_piece[start..pi] {
                for hit in hits {
                    unique.insert(hit.id);
                    radius = radius.max(polyline_distance(&line, [hit.position[0], hit.position[2]]));
                }
            }
            exclusions.push(ScatterExclusion {
                points: line,
                radius: radius + spec.simplify_m + CARVE_MARGIN_M,
            });
        }
        let removed = count_removed(query, class, &exclusions);
        out.push(ClassCarve {
            class: query.class_name(class).to_string(),
            exclusions,
            violators: unique.len() as u32,
            removed,
        });
    }
    CorridorCarve {
        classes: out,
        before: measured.report,
    }
}

/// Instances of `class` inside any of `zones` (placed without the zones).
fn count_removed(query: &ScatterQuery, class: u16, zones: &[ScatterExclusion]) -> u32 {
    let mut seen: HashSet<u64> = HashSet::new();
    for zone in zones {
        let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
        for p in &zone.points {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let Some((c0, c1)) = query.cell_range(class, lo, hi, zone.radius) else {
            continue;
        };
        let mut hits = Vec::new();
        for cz in c0[1]..=c1[1] {
            for cx in c0[0]..=c1[0] {
                hits.clear();
                query.cell_hits(class, cx, cz, &mut hits);
                for h in &hits {
                    if zone.contains(h.position[0], h.position[2]) {
                        seen.insert(h.id);
                    }
                }
            }
        }
    }
    seen.len() as u32
}

fn polyline_distance(line: &[[f32; 2]], p: [f32; 2]) -> f32 {
    match line {
        [] => f32::INFINITY,
        [a] => ((p[0] - a[0]).powi(2) + (p[1] - a[1]).powi(2)).sqrt(),
        _ => line
            .windows(2)
            .map(|w| segment_distance_2d(p, w[0], w[1]))
            .fold(f32::INFINITY, f32::min),
    }
}

fn segment_distance_2d(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ap = [p[0] - a[0], p[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 > 0.0 {
        ((ap[0] * ab[0] + ap[1] * ab[1]) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let d = [ap[0] - ab[0] * t, ap[1] - ab[1] * t];
    (d[0] * d[0] + d[1] * d[1]).sqrt()
}

/// Douglas–Peucker simplification (Douglas & Peucker, "Algorithms for the reduction of the
/// number of points required to represent a digitized line or its caricature", 1973): keeps the
/// end points and every point farther than `tolerance` from the simplified line. Iterative.
#[must_use]
pub fn simplify(line: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    if line.len() <= 2 || tolerance <= 0.0 {
        return line.to_vec();
    }
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    let mut stack = vec![(0usize, line.len() - 1)];
    while let Some((s, e)) = stack.pop() {
        let mut far = (0.0f32, 0usize);
        for (i, p) in line.iter().enumerate().take(e).skip(s + 1) {
            let d = segment_distance_2d(*p, line[s], line[e]);
            if d > far.0 {
                far = (d, i);
            }
        }
        if far.0 > tolerance {
            keep[far.1] = true;
            stack.push((s, far.1));
            stack.push((far.1, e));
        }
    }
    line.iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(p, _)| *p)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_keeps_corners_and_drops_collinear_points() {
        let line: Vec<[f32; 2]> = (0..=10)
            .map(|i| [i as f32, 0.0])
            .chain((1..=10).map(|i| [10.0, i as f32]))
            .collect();
        let s = simplify(&line, 0.1);
        assert_eq!(s, vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]]);
        // Every dropped point stays within the tolerance of the result.
        for p in &line {
            assert!(polyline_distance(&s, *p) <= 0.1 + 1e-6);
        }
    }

    #[test]
    fn pieces_cover_the_path_in_order() {
        let path = CameraPath {
            points: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 3.0]],
            times: None,
        };
        let p = pieces(&path, 8.0);
        assert_eq!(p.len(), 3 + 1);
        assert_eq!(p[0].a, [0.0, 0.0, 0.0]);
        assert_eq!(p[2].b, [20.0, 0.0, 0.0]);
        assert_eq!(p[3].segment, 1);
        assert_eq!(p[3].b, [20.0, 0.0, 3.0]);
    }
}
