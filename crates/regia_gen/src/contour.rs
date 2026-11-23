// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/holo.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (D-02 class P3). Regia reimplemented the behavior; see the docs/provenance.md row (ADR-8044).
//! Marching-squares contours of a scalar grid on the XZ plane (ADR-8044).
//!
//! Each cell is the bilinear patch of its four corners. An edge contributes a point when the
//! level sits strictly between the two samples (linear interpolation; `+ - * /` only). Two
//! crossings make one segment. Four crossings are a saddle: the center value is the average of
//! the corners, and the segments are paired with the asymptotic decider (Nielson, 1991) so the
//! center falls on the same side of the level as the matching corners. Segments are chained by
//! edge key in a [`BTreeMap`] (sorted, never a hash-map walk). Open chains start at degree-1
//! keys in key order; closed loops follow in segment order. Optional Chaikin corner cutting
//! (Chaikin, 1974) runs a caller-chosen number of passes. The strict circle test uses zero
//! passes: two passes shrink a polygonal circle, so a radius check belongs on the unsmoothed
//! line.

use std::collections::{BTreeMap, BTreeSet};

use crate::{GenError, MAX_VERTICES, finite};

/// Most cells along one side of a contour grid.
pub const MAX_CONTOUR_EDGE: u32 = 512;

/// Most contour levels in one call.
pub const MAX_CONTOUR_LEVELS: usize = 64;

/// Most Chaikin passes. Each pass about doubles the point count.
pub const MAX_SMOOTH_PASSES: u32 = 8;

/// A regular scalar grid on the XZ plane.
///
/// Column `i` is `x = origin[0] + i * cell`. Row `j` is `z = origin[1] + j * cell`. `samples`
/// is row-major and holds `(width + 1) * (height + 1)` values (the sample at the far corner is
/// included). Contour `y` is the level, so each contour is planar.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarGrid {
    /// World `x` and `z` of sample `(0, 0)`, metres.
    pub origin: [f32; 2],
    /// Distance between samples, metres. Must be `> 0`.
    pub cell: f32,
    /// Cells along X.
    pub width: u32,
    /// Cells along Z.
    pub height: u32,
    /// Row-major samples, length `(width + 1) * (height + 1)`.
    pub samples: Vec<f32>,
}

/// How [`contours`] turns raw segments into polylines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContourOptions {
    /// Chaikin passes. `0` keeps the marching-squares polyline. The library default is `0`
    /// (a radius check needs the unsmoothed line). An agent call defaults to `2`.
    pub smooth_passes: u32,
    /// Drop a polyline whose length is below this many metres. `0` keeps every polyline.
    pub min_length: f32,
}

impl Default for ContourOptions {
    /// No smoothing and no minimum length.
    fn default() -> Self {
        Self {
            smooth_passes: 0,
            min_length: 0.0,
        }
    }
}

/// One contour polyline at a single level.
#[derive(Clone, Debug, PartialEq)]
pub struct Contour {
    /// The scalar level this line was extracted at. Every point's `y` equals `level`.
    pub level: f32,
    /// `true` when the first point repeats at the end and the line has more than two points.
    pub closed: bool,
    /// Polyline points, metres. A closed line repeats its first point at the end.
    pub points: Vec<[f32; 3]>,
}

/// Extracts one polyline per chain at each level, in `levels` order.
///
/// # Errors
///
/// [`GenError::InvalidInput`] for a non-finite value, a non-positive cell, an empty level list,
/// a sample count that does not match the grid, or a negative `min_length`.
/// [`GenError::TooLarge`] when a side, the level count, the smooth passes or the point count
/// passes its limit.
pub fn contours(
    grid: &ScalarGrid,
    levels: &[f32],
    options: &ContourOptions,
) -> Result<Vec<Contour>, GenError> {
    let _span = tracing::trace_span!("regia_gen::contours").entered();
    check_grid(grid)?;
    if levels.is_empty() {
        return Err(GenError::InvalidInput("levels must not be empty".into()));
    }
    if levels.len() > MAX_CONTOUR_LEVELS {
        return Err(GenError::TooLarge {
            what: "contour levels",
            value: levels.len(),
            limit: MAX_CONTOUR_LEVELS,
        });
    }
    finite("contour levels", levels)?;
    if options.smooth_passes > MAX_SMOOTH_PASSES {
        return Err(GenError::TooLarge {
            what: "smooth passes",
            value: options.smooth_passes as usize,
            limit: MAX_SMOOTH_PASSES as usize,
        });
    }
    finite("min length", &[options.min_length])?;
    if options.min_length < 0.0 {
        return Err(GenError::InvalidInput("min length must be >= 0".into()));
    }
    let mut out = Vec::new();
    let mut points_used = 0usize;
    for &level in levels {
        let segments = march(grid, level);
        for (closed, points) in link(&segments) {
            let (closed, points) = smooth(&points, closed, options.smooth_passes);
            if polyline_length(&points) < options.min_length {
                continue;
            }
            let next = points_used.saturating_add(points.len());
            if next > MAX_VERTICES {
                return Err(GenError::TooLarge {
                    what: "contour points",
                    value: next,
                    limit: MAX_VERTICES,
                });
            }
            points_used = next;
            out.push(Contour {
                level,
                closed,
                points,
            });
        }
    }
    Ok(out)
}

fn check_grid(grid: &ScalarGrid) -> Result<(), GenError> {
    finite("grid origin", &grid.origin)?;
    finite("grid cell", &[grid.cell])?;
    if grid.cell <= 0.0 {
        return Err(GenError::InvalidInput("grid cell must be > 0".into()));
    }
    if grid.width == 0 || grid.height == 0 {
        return Err(GenError::InvalidInput(
            "grid width and height must be >= 1".into(),
        ));
    }
    if grid.width > MAX_CONTOUR_EDGE || grid.height > MAX_CONTOUR_EDGE {
        return Err(GenError::TooLarge {
            what: "contour grid side",
            value: grid.width.max(grid.height) as usize,
            limit: MAX_CONTOUR_EDGE as usize,
        });
    }
    let cols = (grid.width as usize).saturating_add(1);
    let rows = (grid.height as usize).saturating_add(1);
    let need = cols.saturating_mul(rows);
    if grid.samples.len() != need {
        return Err(GenError::InvalidInput(format!(
            "grid samples are {}, expected {need}",
            grid.samples.len()
        )));
    }
    finite("grid samples", &grid.samples)?;
    Ok(())
}

struct Seg {
    a: u64,
    b: u64,
}

fn sample_at(grid: &ScalarGrid, i: u32, j: u32) -> (f32, [f32; 3]) {
    let cols = grid.width + 1;
    let value = grid.samples[(j * cols + i) as usize];
    let p = [
        grid.origin[0] + i as f32 * grid.cell,
        0.0,
        grid.origin[1] + j as f32 * grid.cell,
    ];
    (value, p)
}

fn h_key(i: u32, j: u32, stride: u32) -> u64 {
    2 * (u64::from(j) * u64::from(stride) + u64::from(i))
}

fn v_key(i: u32, j: u32, stride: u32) -> u64 {
    h_key(i, j, stride) + 1
}

/// A crossing of `level` on the segment from `a` to `b`, or `None` when the level is not
/// strictly between them.
fn crossing(a: f32, b: f32, pa: [f32; 3], pb: [f32; 3], level: f32) -> Option<[f32; 3]> {
    let a_below = a < level;
    let b_below = b < level;
    if a_below == b_below {
        return None;
    }
    let t = (level - a) / (b - a);
    Some([pa[0] + (pb[0] - pa[0]) * t, level, pa[2] + (pb[2] - pa[2]) * t])
}

struct Hit {
    key: u64,
    point: [f32; 3],
}

fn march(grid: &ScalarGrid, level: f32) -> (Vec<Seg>, BTreeMap<u64, [f32; 3]>) {
    let mut segs = Vec::new();
    let mut points = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for j in 0..grid.height {
        for i in 0..grid.width {
            cell_segments(grid, i, j, level, &mut segs, &mut points, &mut seen);
        }
    }
    (segs, points)
}

fn cell_segments(
    grid: &ScalarGrid,
    i: u32,
    j: u32,
    level: f32,
    segs: &mut Vec<Seg>,
    points: &mut BTreeMap<u64, [f32; 3]>,
    seen: &mut BTreeSet<(u64, u64)>,
) {
    let stride = grid.width + 1;
    let (v00, p00) = sample_at(grid, i, j);
    let (v10, p10) = sample_at(grid, i + 1, j);
    let (v11, p11) = sample_at(grid, i + 1, j + 1);
    let (v01, p01) = sample_at(grid, i, j + 1);
    let mut hits = Vec::with_capacity(4);
    if let Some(point) = crossing(v00, v10, p00, p10, level) {
        hits.push(Hit {
            key: h_key(i, j, stride),
            point,
        });
    }
    if let Some(point) = crossing(v10, v11, p10, p11, level) {
        hits.push(Hit {
            key: v_key(i + 1, j, stride),
            point,
        });
    }
    if let Some(point) = crossing(v01, v11, p01, p11, level) {
        hits.push(Hit {
            key: h_key(i, j + 1, stride),
            point,
        });
    }
    if let Some(point) = crossing(v00, v01, p00, p01, level) {
        hits.push(Hit {
            key: v_key(i, j, stride),
            point,
        });
    }
    let pairs: Vec<(usize, usize)> = match hits.len() {
        2 => vec![(0, 1)],
        4 => saddle_pairs(v00, v10, v11, v01, level),
        _ => return,
    };
    for (ia, ib) in pairs {
        push_seg(&hits[ia], &hits[ib], segs, points, seen);
    }
}

/// Nielson's asymptotic decider. Corner 0 is the bottom-left sample. Hits are stored in edge
/// order (bottom, right, top, left), so a 4-hit cell's hits are edges 0, 1, 2, 3.
fn saddle_pairs(v00: f32, v10: f32, v11: f32, v01: f32, level: f32) -> Vec<(usize, usize)> {
    let center = (v00 + v10 + v11 + v01) * 0.25;
    if (center < level) == (v00 < level) {
        vec![(0, 3), (1, 2)]
    } else {
        vec![(0, 1), (2, 3)]
    }
}

fn push_seg(
    a: &Hit,
    b: &Hit,
    segs: &mut Vec<Seg>,
    points: &mut BTreeMap<u64, [f32; 3]>,
    seen: &mut BTreeSet<(u64, u64)>,
) {
    if a.key == b.key {
        return;
    }
    let pair = if a.key < b.key {
        (a.key, b.key)
    } else {
        (b.key, a.key)
    };
    if !seen.insert(pair) {
        return;
    }
    points.entry(a.key).or_insert(a.point);
    points.entry(b.key).or_insert(b.point);
    segs.push(Seg { a: a.key, b: b.key });
}

fn link(input: &(Vec<Seg>, BTreeMap<u64, [f32; 3]>)) -> Vec<(bool, Vec<[f32; 3]>)> {
    let (segs, points) = input;
    let mut adj: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, seg) in segs.iter().enumerate() {
        adj.entry(seg.a).or_default().push(index);
        adj.entry(seg.b).or_default().push(index);
    }
    let mut used = vec![false; segs.len()];
    let mut out = Vec::new();
    let starts: Vec<u64> = adj
        .iter()
        .filter(|(_, v)| v.len() == 1)
        .map(|(k, _)| *k)
        .collect();
    for start in starts {
        if let Some(poly) = walk(start, segs, points, &adj, &mut used) {
            out.push(poly);
        }
    }
    for index in 0..segs.len() {
        if used[index] {
            continue;
        }
        if let Some(poly) = walk(segs[index].a, segs, points, &adj, &mut used) {
            out.push(poly);
        }
    }
    out
}

fn walk(
    start: u64,
    segs: &[Seg],
    points: &BTreeMap<u64, [f32; 3]>,
    adj: &BTreeMap<u64, Vec<usize>>,
    used: &mut [bool],
) -> Option<(bool, Vec<[f32; 3]>)> {
    let mut keys = vec![start];
    let mut prev = start;
    for _ in 0..=segs.len() {
        let Some(next_seg) = adj
            .get(&prev)
            .and_then(|list| list.iter().copied().find(|index| !used[*index]))
        else {
            break;
        };
        used[next_seg] = true;
        let seg = &segs[next_seg];
        let next = if seg.a == prev { seg.b } else { seg.a };
        keys.push(next);
        if next == start {
            break;
        }
        prev = next;
    }
    if keys.len() < 2 {
        return None;
    }
    let closed = keys.len() > 2 && keys.first() == keys.last();
    let mut poly = Vec::with_capacity(keys.len());
    for key in &keys {
        poly.push(*points.get(key)?);
    }
    Some((closed, poly))
}

fn smooth(points: &[[f32; 3]], closed: bool, passes: u32) -> (bool, Vec<[f32; 3]>) {
    let mut closed = closed;
    let mut current = points.to_vec();
    for _ in 0..passes {
        let (next_closed, next) = chaikin(&current, closed);
        closed = next_closed;
        current = next;
    }
    (closed, current)
}

/// One Chaikin cut. A closed line repeats its first point; the cut runs on the unique points
/// and the result is closed the same way. An open line keeps its ends.
fn chaikin(points: &[[f32; 3]], closed: bool) -> (bool, Vec<[f32; 3]>) {
    if points.len() < 3 {
        return (closed, points.to_vec());
    }
    if closed {
        let n = points.len() - 1;
        if n < 3 {
            return (true, points.to_vec());
        }
        let mut out = Vec::with_capacity(n * 2 + 1);
        for i in 0..n {
            let a = points[i];
            let b = points[(i + 1) % n];
            out.push(lerp3(a, b, 0.25));
            out.push(lerp3(a, b, 0.75));
        }
        let first = out[0];
        out.push(first);
        (true, out)
    } else {
        let mut out = Vec::with_capacity(points.len() * 2);
        out.push(points[0]);
        for pair in points.windows(2) {
            out.push(lerp3(pair[0], pair[1], 0.25));
            out.push(lerp3(pair[0], pair[1], 0.75));
        }
        out.push(points[points.len() - 1]);
        (false, out)
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn polyline_length(points: &[[f32; 3]]) -> f32 {
    let mut length = 0.0f32;
    for pair in points.windows(2) {
        let d = [
            pair[1][0] - pair[0][0],
            pair[1][1] - pair[0][1],
            pair[1][2] - pair[0][2],
        ];
        length += (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    }
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cone_grid() -> ScalarGrid {
        let cells = 96u32;
        let cell = 0.125f32;
        let origin = [-6.0f32, -6.0];
        let mut samples = Vec::with_capacity(97 * 97);
        for j in 0..=cells {
            for i in 0..=cells {
                let x = origin[0] + i as f32 * cell;
                let z = origin[1] + j as f32 * cell;
                samples.push(8.0 - (x * x + z * z).sqrt());
            }
        }
        ScalarGrid {
            origin,
            cell,
            width: cells,
            height: cells,
            samples,
        }
    }

    #[test]
    fn contour_of_a_cone_is_circles() {
        let grid = cone_grid();
        let levels = [3.25f32, 4.25, 5.25];
        let found = contours(
            &grid,
            &levels,
            &ContourOptions {
                smooth_passes: 0,
                min_length: 0.0,
            },
        )
        .expect("cone");
        assert_eq!(found.len(), 3, "one closed circle at each level");
        for (contour, level) in found.iter().zip(levels) {
            assert!(contour.closed, "level {level} is open");
            assert_eq!(contour.level, level);
            let analytic = 8.0 - level;
            let unique = &contour.points[..contour.points.len() - 1];
            assert!(unique.len() >= 16, "level {level} has too few points");
            let mut sum = 0.0f32;
            for p in unique {
                assert!((p[1] - level).abs() <= 1.0e-5, "contour y is the level");
                let radius = (p[0] * p[0] + p[2] * p[2]).sqrt();
                assert!(
                    (radius - analytic).abs() < 0.02,
                    "level {level}: radius {radius} analytic {analytic}"
                );
                sum += radius;
            }
            let mean = sum / unique.len() as f32;
            assert!((mean - analytic).abs() < 0.02, "mean radius {mean}");
            let mut area = 0.0f32;
            for pair in contour.points.windows(2) {
                area += pair[0][0] * pair[1][2] - pair[1][0] * pair[0][2];
            }
            assert!(area.abs() > 1.0, "level {level} does not wind around the origin");
        }
    }

    #[test]
    fn contour_repeat_is_bit_identical() {
        let grid = cone_grid();
        let options = ContourOptions::default();
        let first = contours(&grid, &[4.25], &options).expect("first");
        let second = contours(&grid, &[4.25], &options).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn two_chaikin_passes_stay_closed_and_round() {
        let grid = cone_grid();
        let found = contours(
            &grid,
            &[4.25],
            &ContourOptions {
                smooth_passes: 2,
                min_length: 0.0,
            },
        )
        .expect("smoothed");
        assert_eq!(found.len(), 1);
        assert!(found[0].closed);
        let unique = &found[0].points[..found[0].points.len() - 1];
        let mut lo = f32::MAX;
        let mut hi = 0.0f32;
        for p in unique {
            let radius = (p[0] * p[0] + p[2] * p[2]).sqrt();
            lo = lo.min(radius);
            hi = hi.max(radius);
        }
        assert!(hi - lo < 0.05, "smoothed radius span {lo}..{hi}");
    }

    #[test]
    fn a_saddle_pairs_two_segments() {
        let grid = ScalarGrid {
            origin: [0.0, 0.0],
            cell: 1.0,
            width: 1,
            height: 1,
            samples: vec![0.0, 1.0, 1.0, 0.0],
        };
        let found = contours(&grid, &[0.5], &ContourOptions::default()).expect("saddle");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|c| !c.closed));
        let mut ends = Vec::new();
        for contour in &found {
            for p in &contour.points {
                ends.push((p[0], p[2]));
            }
        }
        assert!(ends.contains(&(0.5, 0.0)), "{ends:?}");
        assert!(ends.contains(&(1.0, 0.5)), "{ends:?}");
    }

    #[test]
    fn contour_rejects_non_finite_and_a_bad_grid() {
        let mut grid = ScalarGrid {
            origin: [0.0, 0.0],
            cell: 1.0,
            width: 1,
            height: 1,
            samples: vec![0.0, 0.0, 1.0, 1.0],
        };
        let err = contours(&grid, &[f32::NAN], &ContourOptions::default()).expect_err("nan level");
        assert_eq!(err.code(), "gen_invalid_input");
        grid.samples[0] = f32::INFINITY;
        let err = contours(&grid, &[0.5], &ContourOptions::default()).expect_err("inf sample");
        assert_eq!(err.code(), "gen_invalid_input");
        grid.samples[0] = 0.0;
        grid.cell = 0.0;
        let err = contours(&grid, &[0.5], &ContourOptions::default()).expect_err("zero cell");
        assert_eq!(err.code(), "gen_invalid_input");
        grid.cell = 1.0;
        grid.width = MAX_CONTOUR_EDGE + 1;
        grid.samples = vec![0.0; (grid.width as usize + 1) * 2];
        let err = contours(&grid, &[0.5], &ContourOptions::default()).expect_err("too wide");
        assert_eq!(err.code(), "gen_too_large");
    }
}
