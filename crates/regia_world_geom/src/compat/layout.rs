// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnel_layout.rs, commit d6ed37677), ADR-8012.
// Lines 1-200, 306-1423, mark: adapt (ADR-8166, below).
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes, visibility of helpers the
// sibling compat modules call.
// `for_map` and `switchbacks()` (lines 201-305) moved to `compat::fixtures`.
// ADR-8166 (review rev-b L1, L2): std trig and sqrt go through `regia_detsim::math` (libm 0.2.16) and
// std float min/max/clamp through `regia_detsim::float::det_*`; the source tag for those lines is
// DO27's DET_FLOAT change (pending regia-absorb-2).
// Re-cited at regia-absorb-2 (e6c84ba6d) by W-02 (ADR-8167): the file changed there only at 194
// (`MouthHole::contains`: the source's `math::sin_cos`) and 927 (the hole yaw: the source's `math::atan2`),
// DO27's DET_FLOAT; the copy equals the tag up to the math path. The hole yaw calls
// `compat::source_math::atan2` (the source's canonical signed-zero and NaN steps around libm `atan2f`).

//! Tunnel layouts: one spline description per map, the single source of truth.
//!
//! The sim builds its [`TunnelCollisionMesh`] (and from it the
//! `TunnelNavOverlay`) from this description, and the pathing layer reads the
//! same sampled centrelines. The renderer builds the visible geometry from the JSON
//! export ([`TunnelLayout::export_json`], `content/quarantine/maps/build/<map>/tunnels.json`),
//! which carries the description and the sampled rings the sim used, so the
//! two cannot drift. Format: `spec/maps/tunnels-switchbacks.md`.
//!
//! Every number is plain `+ - * /` and `sqrt` (correctly rounded on every
//! platform), so every peer builds the same mesh from the same description.

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;
use serde::{Deserialize, Serialize};

use crate::compat::mesh::{TunnelCollisionMesh, TunnelFaceKind, TunnelTriangle};
use crate::compat::sampler::TerrainSampler;

/// Bump when the description or the sampling rules change.
pub const LAYOUT_VERSION: u32 = 2;
/// Centreline sample spacing (m).
pub const SAMPLE_STEP_M: f32 = 5.0;
/// Rock above a ceiling needs this much cover, or the section is an open cut.
///
/// TUNNELS_FABLE_1013: 3.5 m, up from 1.0. The hero portals draw each ramp open to a headwall
/// about 6 m further in than a 1 m lid allowed, so the sim kept a thin lid (and flat ground
/// over it) across the first 5 m of every drawn trench: bodies on that ground stood in mid-air
/// over the pit. One more sampled segment of each ramp is now an open cut, which ends within
/// about 1 m of the drawn portal face on all four mouths.
pub const ROCK_COVER_M: f32 = 3.5;
/// The top this many metres of a ramp is the portal onto the surface.
pub const PORTAL_LENGTH_M: f32 = 6.0;
/// Two passage floors within this height of each other join flush (no riser between them).
pub const FLUSH_M: f32 = 0.75;
/// The header face over a lid's edge reaches this far over the rock cover.
pub const HEADER_EXTRA_M: f32 = 1.0;
/// TUNNELS_FIX: a side wall is clipped where its base stands this far inside another passage's
/// floor edge (m).
pub const WALL_CLIP_INSET_M: f32 = 0.05;
/// Sub-steps a side wall's 5 m base is classified in before each on/off crossing is bisected.
const WALL_CLIP_STEPS: usize = 16;
/// Format tag of the JSON export.
pub const EXPORT_FORMAT: &str = "do27-tunnels/1";
/// T2: the rim band where the tunnel mesh meets open ground (m). The lip runs
/// from each open-cut trench wall outward by this much, at the ground plus
/// [`LIP_PROUD_M`], so the mesh meets the terrain with no buried funnel.
pub const LIP_W_M: f32 = 3.0;
/// The lip runs this far past the hole edge over drawn ground, so no gap ring
/// shows between the lip and the terrain.
pub const LIP_OVERLAP_M: f32 = 0.5;
/// The lip's outer edge stands this far over the ground: no z-fight with the
/// terrain triangles it overlaps.
pub const LIP_PROUD_M: f32 = 0.10;
/// Hole rects extend this far past an open-cut run's top (mouth) end, so the
/// mouth cap fits; joints between open segments overlap by the same amount.
/// The downhill (rock) end is never extended: no hole over covered ground.
pub const HOLE_END_M: f32 = 1.0;
/// The engine hole mask takes at most this many shapes per map
/// (`spec/regia/engine-asks-2026-10-17.md`, ask 4: `HolesMask`, 64 shapes).
pub const MAX_HOLE_SHAPES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// What a passage is; it picks the look, not the collision.
pub enum PassageKind {
    /// The lava tube.
    Tube,
    /// The old mine gallery.
    Gallery,
    /// A ramp from a passage up to a surface opening (its last point is the top).
    Ramp,
}

/// A pilot-only crawlway: a narrow side passage hulls cannot enter.
///
/// Carried on the tunnel description beside the hull passages. It is not
/// sampled into the collision mesh (that mesh blocks every disc). The sim
/// reads this list and stops hulls at the mouth. Width and height limits
/// match `tools/terrain/crawlways.py` (0.90–2.00 m wide, 1.90–3.00 m high).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrawlwayVolume {
    /// Crawlway id, unique in the layout.
    pub id: String,
    /// Floor centreline, at least two points, metres (x east, y up, z north).
    pub points: Vec<[f32; 3]>,
    /// Floor width, metres.
    pub width_m: f32,
    /// Clear height over the floor, metres.
    pub height_m: f32,
    /// Reserved for wall detail. Collision and pathing ignore it.
    #[serde(default)]
    pub seed: u32,
}

/// One passage: a Catmull-Rom spline through `points` (x, floor y, z in sim
/// metres), a flat floor `width_m` wide, and a ceiling `ceiling_m` above it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Passage {
    /// Passage id, unique in the layout.
    pub id: String,
    /// Tube, gallery or ramp.
    pub kind: PassageKind,
    /// Floor width, metres.
    pub width_m: f32,
    /// Ceiling height over the floor, metres.
    pub ceiling_m: f32,
    /// Spline control points (x, floor y, z), metres.
    pub points: Vec<[f32; 3]>,
}

/// A surface opening: the top of a ramp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Opening {
    /// Opening id, unique in the layout.
    pub id: String,
    /// `skylight_rubble`, `mine_adit`, `fracture_ramp`, `skylight` (the look).
    pub kind: String,
    /// The ramp passage that comes up here.
    pub ramp: String,
    /// Centre of the ramp's top edge, on the surface (sim metres).
    pub top: [f32; 3],
}

/// Where passages meet underground.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Junction {
    /// Junction id, unique in the layout.
    pub id: String,
    /// Where the passages meet (sim metres).
    pub at: [f32; 3],
    /// The ids of the passages that meet here.
    pub passages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// One map's tunnel description: the single source the collision mesh, the nav overlay and the export are built from.
pub struct TunnelLayout {
    /// The map this layout belongs to.
    pub map_id: String,
    /// [`LAYOUT_VERSION`] the description was written for.
    pub version: u32,
    /// Centreline sample spacing (m); [`SAMPLE_STEP_M`].
    pub sample_step_m: f32,
    /// Every passage.
    pub passages: Vec<Passage>,
    /// Every surface opening.
    pub openings: Vec<Opening>,
    /// Every junction.
    pub junctions: Vec<Junction>,
    /// Pilot-only volumes. Empty on every shipped map: omitting the key in
    /// JSON (and when serialising an empty list) keeps `tunnels.json` stable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crawlways: Vec<CrawlwayVolume>,
}

/// One centreline sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Centre of the floor (sim metres).
    pub p: [f32; 3],
    /// Unit horizontal normal to the left of travel (x, z).
    pub left: [f32; 2],
    /// Arc length from the passage start (m).
    pub s: f32,
}

/// A passage sampled every [`SAMPLE_STEP_M`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampledPassage {
    /// The passage id.
    pub id: String,
    /// The passage kind.
    pub kind: PassageKind,
    /// Floor width, metres.
    pub width_m: f32,
    /// Ceiling height over the floor, metres.
    pub ceiling_m: f32,
    /// Centreline samples, in order along the passage.
    pub samples: Vec<Sample>,
    /// Per segment (between sample i and i + 1): rock over the ceiling. False is an open cut.
    pub covered: Vec<bool>,
    /// Per segment: the left and right walls exist (false where another passage opens into this one).
    pub wall_left: Vec<bool>,
    /// Per segment: the right wall exists.
    pub wall_right: Vec<bool>,
    /// The first and last sample end in a wall (a dead end).
    pub cap_start: bool,
    /// The last sample ends in a wall.
    pub cap_end: bool,
}

/// Every passage, sampled, with the junction and cover rules applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampledLayout {
    /// Every passage, in description order.
    pub passages: Vec<SampledPassage>,
}

/// T2: one terrain hole over an open-cut ramp stretch, baked from the same
/// description as the collision mesh. The renderer draws no terrain triangles
/// inside it; the tunnel mesh (trench walls plus the [`LIP_W_M`] lip) is the
/// ground there. One oriented rect per open-cut segment, so curved trenches
/// stay tight to the ramp and the whole map stays under [`MAX_HOLE_SHAPES`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MouthHole {
    /// The opening this hole belongs to.
    pub opening: String,
    /// World XZ of the rect centre.
    pub center: [f32; 2],
    /// Half length (along the ramp) and half width, in metres.
    pub half: [f32; 2],
    /// Direction of the ramp at this segment, `atan2(dz, dx)` radians.
    pub yaw: f32,
}

impl MouthHole {
    /// True when `(x, z)` is inside the rect (a hair of tolerance for edge samples).
    pub fn contains(&self, x: f32, z: f32) -> bool {
        let (dx, dz) = (x - self.center[0], z - self.center[1]);
        // Review rev-b L1 (ADR-8166): libm 0.2.16 through `regia_detsim::math`, not the platform libm,
        // so an x86-64 and an aarch64 peer answer `hole_at` alike at a hole edge.
        let (s, c) = (math::sin(self.yaw), math::cos(self.yaw));
        let u = dx * c + dz * s;
        let v = -dx * s + dz * c;
        u.abs() <= self.half[0] + 1e-3 && v.abs() <= self.half[1] + 1e-3
    }
}

fn catmull(p0: [f32; 3], p1: [f32; 3], p2: [f32; 3], p3: [f32; 3], t: f32) -> [f32; 3] {
    let t2 = t * t;
    let t3 = t2 * t;
    let mut out = [0.0f32; 3];
    for k in 0..3 {
        out[k] = 0.5
            * (2.0 * p1[k]
                + (-p0[k] + p2[k]) * t
                + (2.0 * p0[k] - 5.0 * p1[k] + 4.0 * p2[k] - p3[k]) * t2
                + (-p0[k] + 3.0 * p1[k] - 3.0 * p2[k] + p3[k]) * t3);
    }
    out
}

fn dist3(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    math::sqrt(dx * dx + dy * dy + dz * dz)
}

fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    math::sqrt(dx * dx + dz * dz)
}

/// Distance in XZ from `p` to the segment a-b, and the segment parameter.
fn seg_dist_xz(p: [f32; 3], a: [f32; 3], b: [f32; 3]) -> (f32, f32) {
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let len2 = abx * abx + abz * abz;
    let t = if len2 < 1e-9 {
        0.0
    } else {
        det_clamp(((p[0] - a[0]) * abx + (p[2] - a[2]) * abz) / len2, 0.0, 1.0)
    };
    let q = [a[0] + abx * t, 0.0, a[2] + abz * t];
    (dist_xz(p, q), t)
}

/// Dense polyline of the spline, then resampled to equal arc lengths.
fn centreline(points: &[[f32; 3]], step: f32) -> Vec<Sample> {
    let n = points.len();
    if n < 2 {
        return Vec::new();
    }
    let ghost_start = [
        2.0 * points[0][0] - points[1][0],
        2.0 * points[0][1] - points[1][1],
        2.0 * points[0][2] - points[1][2],
    ];
    let ghost_end = [
        2.0 * points[n - 1][0] - points[n - 2][0],
        2.0 * points[n - 1][1] - points[n - 2][1],
        2.0 * points[n - 1][2] - points[n - 2][2],
    ];
    let at = |i: isize| -> [f32; 3] {
        if i < 0 {
            ghost_start
        } else if i as usize >= n {
            ghost_end
        } else {
            points[i as usize]
        }
    };
    const SUB: usize = 64;
    let mut dense: Vec<[f32; 3]> = vec![points[0]];
    for i in 0..n - 1 {
        let (p0, p1, p2, p3) = (
            at(i as isize - 1),
            at(i as isize),
            at(i as isize + 1),
            at(i as isize + 2),
        );
        for k in 1..=SUB {
            let t = k as f32 / SUB as f32;
            dense.push(if k == SUB { p2 } else { catmull(p0, p1, p2, p3, t) });
        }
    }
    let mut cum = vec![0.0f32; dense.len()];
    for i in 1..dense.len() {
        cum[i] = cum[i - 1] + dist3(dense[i - 1], dense[i]);
    }
    let total = cum[dense.len() - 1];
    let segs = ((total / step).ceil() as usize).max(1);
    let mut pts: Vec<([f32; 3], f32)> = Vec::with_capacity(segs + 1);
    let mut j = 0usize;
    for k in 0..=segs {
        let s = if k == segs {
            total
        } else {
            total * k as f32 / segs as f32
        };
        while j + 1 < dense.len() - 1 && cum[j + 1] < s {
            j += 1;
        }
        let span = cum[j + 1] - cum[j];
        let f = if span > 1e-9 {
            det_clamp((s - cum[j]) / span, 0.0, 1.0)
        } else {
            0.0
        };
        let a = dense[j];
        let b = dense[j + 1];
        let p = if k == segs {
            dense[dense.len() - 1]
        } else {
            [
                a[0] + (b[0] - a[0]) * f,
                a[1] + (b[1] - a[1]) * f,
                a[2] + (b[2] - a[2]) * f,
            ]
        };
        pts.push((p, s));
    }
    let m = pts.len();
    (0..m)
        .map(|i| {
            let a = pts[i.saturating_sub(1)].0;
            let b = pts[(i + 1).min(m - 1)].0;
            let tx = b[0] - a[0];
            let tz = b[2] - a[2];
            let len = det_max(math::sqrt(tx * tx + tz * tz), 1e-6);
            Sample {
                p: pts[i].0,
                left: [-tz / len, tx / len],
                s: pts[i].1,
            }
        })
        .collect()
}

/// An upper bound on the samples [`TunnelLayout::sample`] makes from one passage's control points
/// (Regia addition, review rev-b L6: the baked-file bounds check). Each Catmull-Rom span between `p1` and
/// `p2` has speed `|C'(t)| <= (8|e0| + 13|e1| + 5|e2|) / 2` for its three control edges `e0 = p1 - p0`,
/// `e1 = p2 - p1`, `e2 = p3 - p2` (the end spans use the same ghost points as the sampler), so its length,
/// and the dense polyline's, is at most that; flattening a ramp only shortens the edges. Validation only:
/// it moves no sampled value.
pub(crate) fn centreline_sample_bound(points: &[[f32; 3]], step_m: f32) -> usize {
    let n = points.len();
    if n < 2 {
        return 0;
    }
    let p = |i: isize| -> [f64; 3] {
        let get = |k: usize| points[k].map(f64::from);
        if i < 0 {
            let (a, b) = (get(0), get(1));
            [2.0 * a[0] - b[0], 2.0 * a[1] - b[1], 2.0 * a[2] - b[2]]
        } else if i as usize >= n {
            let (a, b) = (get(n - 1), get(n - 2));
            [2.0 * a[0] - b[0], 2.0 * a[1] - b[1], 2.0 * a[2] - b[2]]
        } else {
            get(i as usize)
        }
    };
    let edge = |a: [f64; 3], b: [f64; 3]| {
        let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        regia_detsim::math::sqrt_f64(d[0] * d[0] + d[1] * d[1] + d[2] * d[2])
    };
    let mut length = 0.0f64;
    for i in 0..n as isize - 1 {
        let (p0, p1, p2, p3) = (p(i - 1), p(i), p(i + 1), p(i + 2));
        length += 0.5 * (8.0 * edge(p0, p1) + 13.0 * edge(p1, p2) + 5.0 * edge(p2, p3));
    }
    let step = f64::from(regia_detsim::float::det_max(step_m, 1.0));
    if !length.is_finite() {
        return usize::MAX;
    }
    // `as` saturates: a huge length gives `usize::MAX`, which the caller refuses.
    ((length / step).ceil() as usize).saturating_add(2)
}

/// The heightfield grid's own ground: the layout (ramp tops, the covered rule) is sampled from the
/// grid, never from the baked mouth patches (TUNNELS_FABLE_1013), which are themselves derived from
/// the drawn portals built on this layout. One direction only, so the export is stable.
fn ground(terrain: Option<&TerrainSampler>, x: f32, z: f32) -> f32 {
    terrain.map_or(0.0, |t| t.grid_height_at_world(x, z))
}

impl TunnelLayout {
    /// The passage with this id.
    pub fn passage(&self, id: &str) -> Option<&Passage> {
        self.passages.iter().find(|p| p.id == id)
    }

    /// Sample every passage and apply the cover, junction and cap rules.
    pub fn sample(&self, terrain: Option<&TerrainSampler>) -> SampledLayout {
        let step = det_max(self.sample_step_m, 1.0);
        // Ramps are sampled flat (arc length in plan) and given their climb below.
        let mut lines: Vec<Vec<Sample>> = self
            .passages
            .iter()
            .map(|p| {
                if p.kind == PassageKind::Ramp && !p.points.is_empty() {
                    let y0 = p.points[0][1];
                    let flat: Vec<[f32; 3]> = p.points.iter().map(|q| [q[0], y0, q[2]]).collect();
                    centreline(&flat, step)
                } else {
                    centreline(&p.points, step)
                }
            })
            .collect();
        let mut climbs: Vec<(usize, f32, f32, f32)> = Vec::new();
        for (i, spec) in self.passages.iter().enumerate() {
            if spec.kind != PassageKind::Ramp || spec.points.len() < 2 {
                continue;
            }
            let (y0, y1) = (spec.points[0][1], spec.points[spec.points.len() - 1][1]);
            let half_own = spec.width_m * 0.5;
            // The passages the ramp starts in (the tube, and a sibling ramp leaving the same spot): every
            // other passage whose footprint holds its first sample. It stays level until its whole width
            // is out of all of them, so where they overlap their floors are one floor.
            let Some(first) = lines[i].first().map(|q| q.p) else {
                continue;
            };
            let bases: Vec<(usize, f32)> = self
                .passages
                .iter()
                .enumerate()
                .filter(|(o, other)| {
                    *o != i
                        && lines[*o]
                            .windows(2)
                            .any(|w| seg_dist_xz(first, w[0].p, w[1].p).0 <= other.width_m * 0.5)
                })
                .map(|(o, other)| (o, other.width_m * 0.5))
                .collect();
            let inside = |q: [f32; 3]| {
                bases.iter().any(|(o, half)| {
                    lines[*o]
                        .windows(2)
                        .any(|w| seg_dist_xz(q, w[0].p, w[1].p).0 <= *half)
                })
            };
            let s_exit = lines[i]
                .iter()
                .find(|q| {
                    let l = [
                        q.p[0] + q.left[0] * half_own,
                        q.p[1],
                        q.p[2] + q.left[1] * half_own,
                    ];
                    let r = [
                        q.p[0] - q.left[0] * half_own,
                        q.p[1],
                        q.p[2] - q.left[1] * half_own,
                    ];
                    !inside(q.p) && !inside(l) && !inside(r)
                })
                .map_or(0.0, |q| q.s);
            climbs.push((i, s_exit, y0, y1));
        }
        // Every exit is measured on the level lines first, so mirror twins come out the same.
        for (i, s_exit, y0, y1) in climbs {
            let total = lines[i].last().map_or(0.0, |q| q.s);
            let run = det_max(total - s_exit, 1.0);
            for q in lines[i].iter_mut() {
                let f = det_clamp((q.s - s_exit) / run, 0.0, 1.0);
                q.p[1] = y0 + (y1 - y0) * f;
            }
        }
        let lines = lines;
        // A point is inside another passage's footprint when it is within that floor's half width
        // (less half a metre) of its centreline and near its floor height.
        let inside_other = |own: usize, q: [f32; 3]| -> bool {
            for (o, line) in lines.iter().enumerate() {
                if o == own {
                    continue;
                }
                let half = self.passages[o].width_m * 0.5 - 0.5;
                for w in line.windows(2) {
                    let (d, t) = seg_dist_xz(q, w[0].p, w[1].p);
                    let y = w[0].p[1] + (w[1].p[1] - w[0].p[1]) * t;
                    if d <= half && (q[1] - y).abs() < 4.0 {
                        return true;
                    }
                }
            }
            false
        };
        let tops: Vec<&str> = self.openings.iter().map(|o| o.ramp.as_str()).collect();
        let mut passages = Vec::with_capacity(self.passages.len());
        for (i, (spec, line)) in self.passages.iter().zip(&lines).enumerate() {
            let half = spec.width_m * 0.5;
            let nseg = line.len().saturating_sub(1);
            let mut covered = Vec::with_capacity(nseg);
            let mut wall_left = Vec::with_capacity(nseg);
            let mut wall_right = Vec::with_capacity(nseg);
            for k in 0..nseg {
                let a = line[k];
                let b = line[k + 1];
                let mid = [
                    (a.p[0] + b.p[0]) * 0.5,
                    (a.p[1] + b.p[1]) * 0.5,
                    (a.p[2] + b.p[2]) * 0.5,
                ];
                let lid = mid[1] + spec.ceiling_m;
                covered.push(ground(terrain, mid[0], mid[2]) > lid + ROCK_COVER_M);
                let side = |sign: f32| -> [f32; 3] {
                    let l = [(a.left[0] + b.left[0]) * 0.5, (a.left[1] + b.left[1]) * 0.5];
                    [
                        mid[0] + l[0] * half * sign,
                        mid[1] + 1.0,
                        mid[2] + l[1] * half * sign,
                    ]
                };
                wall_left.push(!inside_other(i, side(1.0)));
                wall_right.push(!inside_other(i, side(-1.0)));
            }
            let is_top = spec.kind == PassageKind::Ramp && tops.contains(&spec.id.as_str());
            let end_open = |s: &Sample| inside_other(i, [s.p[0], s.p[1] + 1.0, s.p[2]]);
            let cap_start = line.first().is_some_and(|s| !end_open(s));
            let cap_end = line.last().is_some_and(|s| !is_top && !end_open(s));
            passages.push(SampledPassage {
                id: spec.id.clone(),
                kind: spec.kind,
                width_m: spec.width_m,
                ceiling_m: spec.ceiling_m,
                samples: line.clone(),
                covered,
                wall_left,
                wall_right,
                cap_start,
                cap_end,
            });
        }
        SampledLayout { passages }
    }

    /// The sim's collision mesh: floors (the top of each ramp is a portal),
    /// ceilings where rock covers them, walls except where passages meet,
    /// caps on dead ends, and the T2 mouth lips where a ramp meets the sky.
    pub fn mesh(&self, terrain: Option<&TerrainSampler>) -> TunnelCollisionMesh {
        self.sample(terrain).mesh(&self.openings, terrain)
    }

    /// T2: the bake consistency check. Every output comes from the one
    /// description, and this fails the bake when any two disagree: portal
    /// floors at every opening top, a hole over every open-cut stretch and no
    /// hole over covered ground, a lip overlapping every hole edge, and the
    /// hole count inside the engine mask budget. Empty means the bake agrees.
    pub fn bake_check(&self, terrain: Option<&TerrainSampler>) -> Vec<String> {
        let mut errors = Vec::new();
        let sampled = self.sample(terrain);
        let holes = sampled.mouth_holes(&self.openings);
        let tris = sampled.triangles(&self.openings, terrain);
        for o in &self.openings {
            let Some(ramp) = sampled.passage(&o.ramp) else {
                errors.push(format!("opening {}: no ramp {}", o.id, o.ramp));
                continue;
            };
            let Some(top) = ramp.samples.last() else {
                errors.push(format!("opening {}: ramp {} has no samples", o.id, o.ramp));
                continue;
            };
            if dist_xz(top.p, o.top) > 0.01 || (top.p[1] - o.top[1]).abs() > 0.01 {
                errors.push(format!(
                    "opening {}: ramp top {:?} is not at {:?}",
                    o.id, top.p, o.top
                ));
            }
            let portal_near_top = tris.iter().any(|t| {
                t.kind == TunnelFaceKind::Floor
                    && t.portal
                    && [t.a, t.b, t.c]
                        .iter()
                        .any(|v| dist_xz(*v, o.top) < PORTAL_LENGTH_M + ramp.width_m * 0.5 + 1.0)
            });
            if !portal_near_top {
                errors.push(format!("opening {}: no portal floor at the top", o.id));
            }
        }
        for p in &sampled.passages {
            if p.kind != PassageKind::Ramp {
                continue;
            }
            let mouth = self
                .openings
                .iter()
                .find(|o| o.ramp == p.id)
                .map(|o| o.id.as_str())
                .unwrap_or("");
            let own: Vec<&MouthHole> = holes.iter().filter(|h| h.opening == mouth).collect();
            for k in 0..p.samples.len().saturating_sub(1) {
                let a = &p.samples[k];
                let b = &p.samples[k + 1];
                let mid = [
                    (a.p[0] + b.p[0]) * 0.5,
                    (a.p[1] + b.p[1]) * 0.5,
                    (a.p[2] + b.p[2]) * 0.5,
                ];
                let covered = p.covered.get(k).copied().unwrap_or(true);
                if covered {
                    if own.iter().any(|h| h.contains(mid[0], mid[2])) {
                        errors.push(format!("ramp {} segment {k}: covered ground inside a hole", p.id));
                    }
                    continue;
                }
                for q in [a.p, b.p, mid] {
                    if !own.iter().any(|h| h.contains(q[0], q[2])) {
                        errors.push(format!(
                            "ramp {} segment {k}: open cut at ({:.1}, {:.1}) outside every hole",
                            p.id, q[0], q[2]
                        ));
                    }
                }
            }
        }
        if holes.len() > MAX_HOLE_SHAPES {
            errors.push(format!(
                "{} holes over the {MAX_HOLE_SHAPES}-shape mask budget",
                holes.len()
            ));
        }
        // Every hole corner sits on a lip: a floor vertex near it at about the
        // ground. Trench floors are metres down, so only the lip qualifies.
        // Without terrain there is no ground to meet, so no lips are baked
        // and this check cannot run.
        if terrain.is_some() {
            let mut floor_verts: Vec<[f32; 3]> = Vec::new();
            for t in &tris {
                if t.kind == TunnelFaceKind::Floor {
                    floor_verts.extend_from_slice(&[t.a, t.b, t.c]);
                }
            }
            for h in &holes {
                let (s, c) = (math::sin(h.yaw), math::cos(h.yaw));
                for (eu, ev) in [(-1.0, -1.0), (-1.0, 1.0), (1.0, -1.0), (1.0, 1.0)] {
                    let (u, v) = (eu * h.half[0], ev * h.half[1]);
                    let (x, z) = (h.center[0] + u * c - v * s, h.center[1] + u * s + v * c);
                    let g = ground(terrain, x, z);
                    let on_lip = floor_verts.iter().any(|w| {
                        let (ex, ez) = (w[0] - x, w[2] - z);
                        let d = math::sqrt(ex * ex + ez * ez);
                        d < LIP_W_M * 0.5 + 0.25 && (w[1] - g).abs() < 1.0
                    });
                    if !on_lip {
                        errors.push(format!(
                            "hole {} corner ({x:.1}, {z:.1}): no lip under it",
                            h.opening
                        ));
                    }
                }
            }
        }
        errors
    }

    /// The JSON a renderer builds the visible tunnel from (see the spec for the format).
    pub fn export_json(&self, terrain: Option<&TerrainSampler>) -> String {
        #[derive(Serialize)]
        struct Export<'a> {
            format: &'static str,
            coordinates: &'static str,
            layout: &'a TunnelLayout,
            sampled: SampledLayout,
            holes: Vec<MouthHole>,
            triangle_count: usize,
        }
        let sampled = self.sample(terrain);
        let holes = sampled.mouth_holes(&self.openings);
        let triangle_count = sampled.triangles(&self.openings, terrain).len();
        let export = Export {
            format: EXPORT_FORMAT,
            coordinates: "sim metres: x east, y up, z north; a z-up centimetre renderer takes (x * 100, -z * 100, y * 100)",
            layout: self,
            sampled,
            holes,
            triangle_count,
        };
        let mut text = serde_json::to_string_pretty(&export).unwrap_or_default();
        text.push('\n');
        text
    }
}

impl SampledLayout {
    /// The sampled passage with this id.
    pub fn passage(&self, id: &str) -> Option<&SampledPassage> {
        self.passages.iter().find(|p| p.id == id)
    }

    /// The sim's collision mesh from these already-sampled centrelines (the
    /// baked file's `sampled` block) plus the openings and terrain. Byte-
    /// identical to [`TunnelLayout::mesh`] when the sampled block came from
    /// [`TunnelLayout::sample`] on the same terrain (the one-bake guarantee).
    pub fn mesh(&self, openings: &[Opening], terrain: Option<&TerrainSampler>) -> TunnelCollisionMesh {
        TunnelCollisionMesh::new(self.triangles(openings, terrain))
    }

    /// Floors of the other passages at `q`'s XZ (within their width plus `pad`).
    fn other_floors(&self, own: &str, q: [f32; 3], pad: f32) -> Vec<f32> {
        let mut out = Vec::new();
        for o in self.passages.iter().filter(|o| o.id != own) {
            let half = o.width_m * 0.5 + pad;
            let mut best: Option<(f32, f32)> = None;
            for w in o.samples.windows(2) {
                let (d, t) = seg_dist_xz(q, w[0].p, w[1].p);
                if d <= half && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, w[0].p[1] + (w[1].p[1] - w[0].p[1]) * t));
                }
            }
            if let Some((_, y)) = best {
                out.push(y);
            }
        }
        out
    }

    /// TUNNELS_FIX: true when `q`, a point on a floor edge, stands on one of `others`' floors: within
    /// that floor's half width (less [`WALL_CLIP_INSET_M`]) of its centreline, between its first and
    /// last sample (a floor has no round end), and within 4 m of its height (the junction rule of
    /// `sample`).
    fn on_other_floor(others: &[&SampledPassage], q: [f32; 3]) -> bool {
        others.iter().any(|o| {
            let half = o.width_m * 0.5 - WALL_CLIP_INSET_M;
            let last = o.samples.len().saturating_sub(2);
            o.samples.windows(2).enumerate().any(|(i, w)| {
                let (d, t) = seg_dist_xz(q, w[0].p, w[1].p);
                if (i == 0 && t <= 0.0) || (i == last && t >= 1.0) {
                    return false;
                }
                let y = w[0].p[1] + (w[1].p[1] - w[0].p[1]) * t;
                d <= half && (q[1] - y).abs() < 4.0
            })
        })
    }

    /// TUNNELS_FIX: the stretches `(t0, t1)` of a side wall's base `a -> b` that stand on no other
    /// passage's floor (the whole wall, `(0, 1)`, almost everywhere). Walls are kept or dropped per
    /// 5 m segment by the segment's midpoint (`sample`), so at a junction mouth the kept segment
    /// beside the opening stood up to 1.08 m into the neighbour's floor (the mid ramps' seg 5 in the
    /// tube, the tube's walls in the mid ramps, the gallery's in the tube): a wall stub no lining
    /// draws, which pushed a hull back. It is clipped at the neighbour's floor edge instead.
    /// Fixed sub-steps and a fixed bisection count, so every peer clips at the same point.
    fn wall_runs(&self, own: &str, a: [f32; 3], b: [f32; 3]) -> Vec<(f32, f32)> {
        // Only passages whose floor box reaches this wall can hold it.
        let (lo_x, hi_x) = (det_min(a[0], b[0]), det_max(a[0], b[0]));
        let (lo_z, hi_z) = (det_min(a[2], b[2]), det_max(a[2], b[2]));
        let others: Vec<&SampledPassage> = self
            .passages
            .iter()
            .filter(|o| o.id != own)
            .filter(|o| {
                // Samples are about SAMPLE_STEP_M apart, so a floor reaching the wall has one this close.
                let pad = o.width_m * 0.5 + SAMPLE_STEP_M + 1.0;
                o.samples.iter().any(|q| {
                    q.p[0] >= lo_x - pad
                        && q.p[0] <= hi_x + pad
                        && q.p[2] >= lo_z - pad
                        && q.p[2] <= hi_z + pad
                })
            })
            .collect();
        if others.is_empty() {
            return vec![(0.0, 1.0)];
        }
        let at = |t: f32| {
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ]
        };
        let on = |t: f32| Self::on_other_floor(&others, at(t));
        let flags: Vec<bool> = (0..=WALL_CLIP_STEPS)
            .map(|i| on(i as f32 / WALL_CLIP_STEPS as f32))
            .collect();
        if flags.iter().all(|f| !f) {
            return vec![(0.0, 1.0)];
        }
        // The crossing between `lo` (state `lo_on`) and `hi`.
        let cross = |mut lo: f32, mut hi: f32, lo_on: bool| -> f32 {
            for _ in 0..20 {
                let mid = (lo + hi) * 0.5;
                if on(mid) == lo_on {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            (lo + hi) * 0.5
        };
        let mut runs = Vec::new();
        let mut start = if flags[0] { None } else { Some(0.0) };
        for i in 1..=WALL_CLIP_STEPS {
            if flags[i] == flags[i - 1] {
                continue;
            }
            let (t0, t1) = (
                (i - 1) as f32 / WALL_CLIP_STEPS as f32,
                i as f32 / WALL_CLIP_STEPS as f32,
            );
            let t = cross(t0, t1, flags[i - 1]);
            if flags[i] {
                if let Some(s0) = start.take() {
                    runs.push((s0, t));
                }
            } else {
                start = Some(t);
            }
        }
        if let Some(s0) = start {
            runs.push((s0, 1.0));
        }
        runs.retain(|(t0, t1)| t1 - t0 > 1e-3);
        runs
    }

    /// Side wall of one segment, `a -> b` at the floor and `at -> bt` at its top, clipped by
    /// [`Self::wall_runs`]. A clipped stretch gets the riser an open side gets.
    #[allow(clippy::too_many_arguments)]
    fn side_wall(
        &self,
        own: &str,
        ceiling_m: f32,
        a: [f32; 3],
        b: [f32; 3],
        at: [f32; 3],
        bt: [f32; 3],
        left: bool,
        tris: &mut Vec<TunnelTriangle>,
    ) {
        let wall = |u: [f32; 3], v: [f32; 3], w: [f32; 3]| TunnelTriangle {
            a: u,
            b: v,
            c: w,
            kind: TunnelFaceKind::Wall,
            portal: false,
            pilot_only: false,
        };
        let quad = |a: [f32; 3], b: [f32; 3], at: [f32; 3], bt: [f32; 3], tris: &mut Vec<TunnelTriangle>| {
            if left {
                tris.push(wall(a, b, bt));
                tris.push(wall(a, bt, at));
            } else {
                tris.push(wall(a, bt, b));
                tris.push(wall(a, at, bt));
            }
        };
        let runs = self.wall_runs(own, a, b);
        if runs == [(0.0, 1.0)] {
            quad(a, b, at, bt, tris);
            return;
        }
        let lerp = |u: [f32; 3], v: [f32; 3], t: f32| -> [f32; 3] {
            [
                u[0] + (v[0] - u[0]) * t,
                u[1] + (v[1] - u[1]) * t,
                u[2] + (v[2] - u[2]) * t,
            ]
        };
        let mut open_from = 0.0f32;
        for &(t0, t1) in &runs {
            if t0 > open_from {
                self.riser(own, ceiling_m, lerp(a, b, open_from), lerp(a, b, t0), left, tris);
            }
            quad(
                lerp(a, b, t0),
                lerp(a, b, t1),
                lerp(at, bt, t0),
                lerp(at, bt, t1),
                tris,
            );
            open_from = t1;
        }
        if open_from < 1.0 {
            self.riser(own, ceiling_m, lerp(a, b, open_from), b, left, tris);
        }
    }

    /// Where a passage's side is open to a neighbour (no wall) but the neighbour's floor there is
    /// higher than a step, the open side gets a riser: a face from this floor up to the neighbour's.
    /// Without it a hull on the tube floor slid out sideways under a ramp's slab, off every floor.
    /// A flush join (within [`FLUSH_M`]) stays open; a neighbour lower down owns that riser.
    fn riser(
        &self,
        own: &str,
        ceiling_m: f32,
        a: [f32; 3],
        b: [f32; 3],
        left: bool,
        tris: &mut Vec<TunnelTriangle>,
    ) {
        let rise = |q: [f32; 3]| -> f32 {
            let floors = self.other_floors(own, q, 0.5);
            if floors.iter().any(|y| (y - q[1]).abs() <= FLUSH_M) {
                return 0.0;
            }
            floors
                .iter()
                .map(|y| y - q[1])
                .filter(|d| *d > FLUSH_M && *d < ceiling_m)
                .fold(0.0_f32, |m, d| if m == 0.0 { d } else { det_min(m, d) })
        };
        let (ha, hb) = (rise(a), rise(b));
        if ha <= 0.0 && hb <= 0.0 {
            return;
        }
        let at = [a[0], a[1] + ha, a[2]];
        let bt = [b[0], b[1] + hb, b[2]];
        let wall = |u: [f32; 3], v: [f32; 3], w: [f32; 3]| TunnelTriangle {
            a: u,
            b: v,
            c: w,
            kind: TunnelFaceKind::Wall,
            portal: false,
            pilot_only: false,
        };
        // Same winding as the full side walls, so the inward normal agrees.
        if left {
            tris.push(wall(a, b, bt));
            tris.push(wall(a, bt, at));
        } else {
            tris.push(wall(a, bt, b));
            tris.push(wall(a, at, bt));
        }
    }

    /// True when another passage's floor at `q`'s XZ (inside its width, plus a metre) stands more than
    /// a metre over `q` (a point on `own`'s floor) but under `own`'s lid `ceiling_m` over it: that
    /// passage climbs up through `own` here.
    fn climbs_through(&self, own: &str, q: [f32; 3], ceiling_m: f32) -> bool {
        self.passages.iter().filter(|o| o.id != own).any(|o| {
            let half = o.width_m * 0.5 + 1.0;
            o.samples.windows(2).any(|w| {
                let (d, t) = seg_dist_xz(q, w[0].p, w[1].p);
                let y = w[0].p[1] + (w[1].p[1] - w[0].p[1]) * t;
                d <= half && y > q[1] + 1.0 && y < q[1] + ceiling_m
            })
        })
    }

    /// T2: the terrain hole mask, one oriented rect per open-cut ramp segment.
    /// The rect hugs the trench at half width plus the lip minus its overlap
    /// onto drawn ground, and runs the segment plus [`HOLE_END_M`] past its
    /// top end (into the next open segment, or under the mouth cap). The
    /// downhill end is never extended: no hole over covered ground.
    pub fn mouth_holes(&self, openings: &[Opening]) -> Vec<MouthHole> {
        let mut out = Vec::new();
        for p in &self.passages {
            if p.kind != PassageKind::Ramp {
                continue;
            }
            let Some(o) = openings.iter().find(|o| o.ramp == p.id) else {
                continue;
            };
            let nseg = p.samples.len().saturating_sub(1);
            for k in 0..nseg {
                if p.covered.get(k).copied().unwrap_or(true) {
                    continue;
                }
                let (a, b) = (&p.samples[k], &p.samples[k + 1]);
                let (dx, dz) = (b.p[0] - a.p[0], b.p[2] - a.p[2]);
                let len = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
                // Past the top end only: into the next open segment, or under
                // the mouth cap on the last segment.
                let next_open = k + 1 < nseg && !p.covered.get(k + 1).copied().unwrap_or(true);
                let ext = if next_open || k + 1 == nseg {
                    HOLE_END_M
                } else {
                    0.0
                };
                let (ux, uz) = (dx / len, dz / len);
                out.push(MouthHole {
                    opening: o.id.clone(),
                    center: [
                        (a.p[0] + b.p[0]) * 0.5 + ux * ext * 0.5,
                        (a.p[2] + b.p[2]) * 0.5 + uz * ext * 0.5,
                    ],
                    half: [len * 0.5 + ext * 0.5, p.width_m * 0.5 + LIP_W_M - LIP_OVERLAP_M],
                    yaw: crate::compat::source_math::atan2(dz, dx),
                });
            }
        }
        out
    }

    /// Collision triangles for the whole layout. With terrain, open-cut ramp
    /// walls rise only to the ground (not a full ceiling height over the
    /// trench), and a lip band joins each wall top to the ground past the hole
    /// edge, with a sill at the rock end and a cap at the mouth: the mesh
    /// meets open ground with a modeled lip and no buried funnel. Without
    /// terrain there is no ground to meet, so walls keep ceiling height and no
    /// lips are baked.
    pub fn triangles(&self, openings: &[Opening], terrain: Option<&TerrainSampler>) -> Vec<TunnelTriangle> {
        let mut tris = Vec::new();
        let tri =
            |a: [f32; 3], b: [f32; 3], c: [f32; 3], kind: TunnelFaceKind, portal: bool| TunnelTriangle {
                a,
                b,
                c,
                kind,
                portal,
                pilot_only: false,
            };
        for p in &self.passages {
            let half = p.width_m * 0.5;
            let is_top = p.kind == PassageKind::Ramp && openings.iter().any(|o| o.ramp == p.id);
            let total = p.samples.last().map(|s| s.s).unwrap_or(0.0);
            let edge = |s: &Sample, sign: f32, up: f32| -> [f32; 3] {
                [
                    s.p[0] + s.left[0] * half * sign,
                    s.p[1] + up,
                    s.p[2] + s.left[1] * half * sign,
                ]
            };
            let nseg = p.samples.len().saturating_sub(1);
            for k in 0..nseg {
                let a = &p.samples[k];
                let b = &p.samples[k + 1];
                let (la, ra, lb, rb) = (
                    edge(a, 1.0, 0.0),
                    edge(a, -1.0, 0.0),
                    edge(b, 1.0, 0.0),
                    edge(b, -1.0, 0.0),
                );
                let portal = is_top && b.s >= total - PORTAL_LENGTH_M;
                tris.push(tri(la, ra, rb, TunnelFaceKind::Floor, portal));
                tris.push(tri(la, rb, lb, TunnelFaceKind::Floor, portal));
                // T2: an open-cut ramp stretch meets the sky, so its walls
                // rise only to the ground and a lip joins each wall top to the
                // ground past the hole edge. Covered stretches and the
                // terrain-free bake keep full-height walls and no lips.
                let open_cut = p.kind == PassageKind::Ramp && terrain.is_some() && !p.covered[k];
                let rim = |e: [f32; 3]| det_max(det_max(e[1], ground(terrain, e[0], e[2])), e[1] + 0.5);
                let h = p.ceiling_m;
                if p.covered[k] {
                    let (lat, rat, lbt, rbt) = (
                        edge(a, 1.0, h),
                        edge(a, -1.0, h),
                        edge(b, 1.0, h),
                        edge(b, -1.0, h),
                    );
                    // Where another passage climbs out through this one (a ramp leaving the tube at a shallow
                    // angle), its floor rises inside this passage's height and this lid would slice through
                    // its space: a hull riding the ramp down crossed the edge of the tube's lid and was
                    // clamped metres down in one tick. The lid is left out there, in a grid of pieces fine
                    // enough to follow the other passage's edge; that passage's own lid is the roof.
                    let lerp = |u: [f32; 3], v: [f32; 3], t: f32| -> [f32; 3] {
                        [
                            u[0] + (v[0] - u[0]) * t,
                            u[1] + (v[1] - u[1]) * t,
                            u[2] + (v[2] - u[2]) * t,
                        ]
                    };
                    let at = |i: f32, j: f32| -> [f32; 3] { lerp(lerp(lat, rat, i), lerp(lbt, rbt, i), j) };
                    let intruded = |i: f32, j: f32| -> bool {
                        let q = at(i, j);
                        self.climbs_through(&p.id, [q[0], q[1] - h, q[2]], h)
                    };
                    let any = (0..=4).any(|i| (0..=2).any(|j| intruded(i as f32 / 4.0, j as f32 / 2.0)));
                    if !any {
                        tris.push(tri(lat, rbt, rat, TunnelFaceKind::Ceiling, false));
                        tris.push(tri(lat, lbt, rbt, TunnelFaceKind::Ceiling, false));
                    } else {
                        const ACROSS: usize = 8;
                        const ALONG: usize = 2;
                        for i in 0..ACROSS {
                            for j in 0..ALONG {
                                let (i0, i1) = (i as f32 / ACROSS as f32, (i + 1) as f32 / ACROSS as f32);
                                let (j0, j1) = (j as f32 / ALONG as f32, (j + 1) as f32 / ALONG as f32);
                                if intruded((i0 + i1) * 0.5, (j0 + j1) * 0.5) {
                                    continue;
                                }
                                let (l0, r0, l1, r1) = (at(i0, j0), at(i1, j0), at(i0, j1), at(i1, j1));
                                tris.push(tri(l0, r1, r0, TunnelFaceKind::Ceiling, false));
                                tris.push(tri(l0, l1, r1, TunnelFaceKind::Ceiling, false));
                            }
                        }
                    }
                }
                // Wall tops: the rim at the ground in an open cut, the lid
                // line where rock covers the passage.
                let rim_at = |e: [f32; 3]| -> [f32; 3] {
                    if open_cut {
                        [e[0], rim(e), e[2]]
                    } else {
                        [e[0], e[1] + h, e[2]]
                    }
                };
                if p.wall_left[k] {
                    let (lat, lbt) = (rim_at(edge(a, 1.0, 0.0)), rim_at(edge(b, 1.0, 0.0)));
                    self.side_wall(&p.id, h, la, lb, lat, lbt, true, &mut tris);
                } else {
                    self.riser(&p.id, h, la, lb, true, &mut tris);
                }
                if p.wall_right[k] {
                    let (rat, rbt) = (rim_at(edge(a, -1.0, 0.0)), rim_at(edge(b, -1.0, 0.0)));
                    self.side_wall(&p.id, h, ra, rb, rat, rbt, false, &mut tris);
                } else {
                    self.riser(&p.id, h, ra, rb, false, &mut tris);
                }
                if open_cut {
                    // The lip: from each wall top outward past the hole edge,
                    // at the ground plus a hair, so the mesh meets drawn
                    // ground with no seam, no z-fight and no gap. Same
                    // winding pattern as the floors, so the faces read up.
                    let outer = |e: [f32; 3], l: [f32; 2], sign: f32| -> [f32; 3] {
                        let (x, z) = (e[0] + l[0] * LIP_W_M * sign, e[2] + l[1] * LIP_W_M * sign);
                        [x, ground(terrain, x, z) + LIP_PROUD_M, z]
                    };
                    let (ila, ilb) = ([la[0], rim(la), la[2]], [lb[0], rim(lb), lb[2]]);
                    let (ola, olb) = (outer(la, a.left, 1.0), outer(lb, b.left, 1.0));
                    tris.push(tri(ola, ila, ilb, TunnelFaceKind::Floor, false));
                    tris.push(tri(ola, ilb, olb, TunnelFaceKind::Floor, false));
                    let (ira, irb) = ([ra[0], rim(ra), ra[2]], [rb[0], rim(rb), rb[2]]);
                    let (ora, orb) = (outer(ra, a.left, -1.0), outer(rb, b.left, -1.0));
                    tris.push(tri(ira, ora, orb, TunnelFaceKind::Floor, false));
                    tris.push(tri(ira, orb, irb, TunnelFaceKind::Floor, false));
                    let run_start = k == 0 || p.covered[k - 1];
                    let run_end = k + 1 == nseg || p.covered[k + 1];
                    if run_start {
                        // A threshold sill where the ramp leaves the rock: on
                        // the floor, a step over it, meeting both wall bases.
                        let y0 = a.p[1] + 0.3;
                        let (dx, dz) = (b.p[0] - a.p[0], b.p[2] - a.p[2]);
                        let dl = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
                        let (sla, sra) = ([la[0], y0, la[2]], [ra[0], y0, ra[2]]);
                        let (slb, srb) = (
                            [la[0] + dx / dl, y0, la[2] + dz / dl],
                            [ra[0] + dx / dl, y0, ra[2] + dz / dl],
                        );
                        tris.push(tri(sla, sra, srb, TunnelFaceKind::Floor, false));
                        tris.push(tri(sla, srb, slb, TunnelFaceKind::Floor, false));
                    }
                    if run_end {
                        // The mouth cap past the top edge: full lip width, at
                        // the ground plus a hair, reaching past the hole end
                        // over drawn ground.
                        let reach = HOLE_END_M + LIP_OVERLAP_M;
                        let (dx, dz) = (b.p[0] - a.p[0], b.p[2] - a.p[2]);
                        let dl = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
                        let cap = |e: [f32; 3], l: [f32; 2], sign: f32, along: f32| -> [f32; 3] {
                            let (x, z) = (
                                e[0] + l[0] * (half + LIP_W_M) * sign + dx / dl * along,
                                e[2] + l[1] * (half + LIP_W_M) * sign + dz / dl * along,
                            );
                            [x, ground(terrain, x, z) + LIP_PROUD_M, z]
                        };
                        let (cla, cra) = (cap(lb, b.left, 1.0, 0.0), cap(rb, b.left, -1.0, 0.0));
                        let (clb, crb) = (cap(lb, b.left, 1.0, reach), cap(rb, b.left, -1.0, reach));
                        tris.push(tri(cla, cra, crb, TunnelFaceKind::Floor, false));
                        tris.push(tri(cla, crb, clb, TunnelFaceKind::Floor, false));
                    }
                }
                // Header: where the rock lid starts (or ends) over a passage, the rock face over the lid's
                // edge. A hull flying high in an open cut meets it instead of being pressed down under the
                // lid in one tick.
                if k > 0 && p.covered[k] != p.covered[k - 1] {
                    let (lt, rt) = (edge(a, 1.0, h), edge(a, -1.0, h));
                    let up = h + ROCK_COVER_M + HEADER_EXTRA_M;
                    let (lu, ru) = (edge(a, 1.0, up), edge(a, -1.0, up));
                    tris.push(tri(lt, rt, ru, TunnelFaceKind::Wall, false));
                    tris.push(tri(lt, ru, lu, TunnelFaceKind::Wall, false));
                }
            }
            let mut cap = |s: &Sample| {
                let (l, r, lt, rt) = (
                    edge(s, 1.0, 0.0),
                    edge(s, -1.0, 0.0),
                    edge(s, 1.0, p.ceiling_m),
                    edge(s, -1.0, p.ceiling_m),
                );
                tris.push(tri(l, r, rt, TunnelFaceKind::Wall, false));
                tris.push(tri(l, rt, lt, TunnelFaceKind::Wall, false));
            };
            if p.cap_start
                && let Some(s) = p.samples.first()
            {
                cap(s);
            }
            if p.cap_end
                && let Some(s) = p.samples.last()
            {
                cap(s);
            }
        }
        tris
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::compat::fixtures::{for_map, switchbacks};
    use crate::compat::sampler::HeightGrid;

    fn len(p: &SampledPassage) -> f32 {
        p.samples.last().map(|s| s.s).unwrap_or(0.0)
    }

    /// Flat -256 m ground, like the Switchbacks canyon floor the mouths open onto.
    fn canyon_terrain() -> TerrainSampler {
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0)
            .with_zero_counts(16384)
    }

    /// Arming (T1): every map the runtime arms has a layout that samples and
    /// meshes, and `for_map` returns it. A layout that panics or refuses to
    /// build would take its map down at load.
    #[test]
    fn every_armed_map_samples_and_meshes() {
        for id in ["switchbacks", "atacama_rail", "cinder_steps"] {
            let layout = for_map(id).unwrap_or_else(|| panic!("{id} has no layout"));
            assert_eq!(layout.map_id, id);
            let sampled = layout.sample(None);
            assert!(!sampled.passages.is_empty(), "{id}: no passages");
            let mesh = layout.mesh(None);
            assert!(
                mesh.triangles.len() > 100,
                "{id}: {} triangles",
                mesh.triangles.len()
            );
            assert!(mesh.triangles.iter().any(|t| t.portal), "{id}: no portal floors");
        }
    }

    /// No side wall stands in another passage's lane. A 4.5 m disc (the
    /// largest hull that routes underground) is moved along both lane edges of every passage of every
    /// armed map, every 0.5 m, held 0.05 m clear of the passage's own straight wall segments (a
    /// bend's chords included): no other wall may push it. The 5 m keep-or-drop rule left the segment
    /// beside each mouth up to 1.08 m into the neighbour's lane (Switchbacks: the mid ramps' seg 5 in
    /// the tube). Terrain-free; the sweep proofs (plan row W-05) repeat it on the real terrain for every
    /// hull class and the pilot.
    #[test]
    fn no_side_wall_stands_in_another_passages_lane() {
        const R: f32 = 4.5;
        let mut worst = Vec::new();
        for id in ["switchbacks", "atacama_rail", "cinder_steps"] {
            let layout = for_map(id).unwrap();
            let sampled = layout.sample(None);
            let mesh = sampled.mesh(&layout.openings, None);
            for p in &sampled.passages {
                let total = p.samples.last().map_or(0.0, |q| q.s);
                let half = p.width_m * 0.5;
                let lane = half - R - 0.05;
                let nseg = p.samples.len() - 1;
                let edge = |i: usize, sign: f32| -> [f32; 3] {
                    let q = &p.samples[i];
                    [
                        q.p[0] + q.left[0] * half * sign,
                        0.0,
                        q.p[2] + q.left[1] * half * sign,
                    ]
                };
                for (k, w) in p.samples.windows(2).enumerate() {
                    // The passage's own walls on this segment and its neighbours.
                    let mut own: Vec<([f32; 3], [f32; 3])> = Vec::new();
                    for j in k.saturating_sub(1)..=(k + 1).min(nseg - 1) {
                        if p.wall_left[j] {
                            own.push((edge(j, 1.0), edge(j + 1, 1.0)));
                        }
                        if p.wall_right[j] {
                            own.push((edge(j, -1.0), edge(j + 1, -1.0)));
                        }
                    }
                    let n = det_max(((w[1].s - w[0].s) / 0.5).ceil(), 1.0) as usize;
                    for j in 0..n {
                        let f = j as f32 / n as f32;
                        let s = w[0].s + (w[1].s - w[0].s) * f;
                        if (p.cap_start && s < R + 0.6) || (p.cap_end && s > total - R - 0.6) {
                            continue;
                        }
                        let c = [0usize, 1, 2].map(|k| w[0].p[k] + (w[1].p[k] - w[0].p[k]) * f);
                        let l = [
                            w[0].left[0] + (w[1].left[0] - w[0].left[0]) * f,
                            w[0].left[1] + (w[1].left[1] - w[0].left[1]) * f,
                        ];
                        let ln = math::sqrt(l[0] * l[0] + l[1] * l[1]);
                        for side in [-1.0f32, 1.0] {
                            // Step in from the floor edge until the disc clears its own walls.
                            let mut o = side * lane;
                            let at = |o: f32| [c[0] + l[0] / ln * o, 0.0, c[2] + l[1] / ln * o];
                            while o * side > 0.0
                                && own.iter().any(|(a, b)| seg_dist_xz(at(o), *a, *b).0 < R + 0.05)
                            {
                                o -= side * 0.05;
                            }
                            if o * side <= 0.0 {
                                continue;
                            }
                            let (x, z) = (at(o)[0], at(o)[2]);
                            let q = crate::compat::mesh::separate_disc_from_walls_band(
                                &mesh,
                                x,
                                z,
                                R,
                                c[1] + 0.5,
                                c[1] + 3.0,
                            );
                            let (ex, ez) = (q[0] - x, q[1] - z);
                            let push = math::sqrt(ex * ex + ez * ez);
                            if push > 0.02 {
                                worst.push(format!(
                                    "{id}/{} s {s:.1} offset {o:+.2} at ({x:.1}, {z:.1}): pushed {push:.2} m",
                                    p.id
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            worst.is_empty(),
            "{} lane-edge pushes:\n{}",
            worst.len(),
            worst.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn switchbacks_meets_the_spec() {
        let layout = switchbacks();
        let sampled = layout.sample(None);
        let tube = sampled.passage("tube").unwrap();
        assert!((1400.0..=1800.0).contains(&len(tube)), "tube {} m", len(tube));
        assert!((18.0..=24.0).contains(&tube.width_m));
        assert!((10.0..=14.0).contains(&tube.ceiling_m));
        assert_eq!(layout.openings.len(), 4);
        assert_eq!(layout.junctions.len(), 2);
        for opening in &layout.openings {
            let ramp = sampled.passage(&opening.ramp).unwrap();
            let l = len(ramp);
            assert!((40.0..=90.0).contains(&l), "{} ramp {l} m", opening.id);
            for w in ramp.samples.windows(2) {
                let rise = (w[1].p[1] - w[0].p[1]).abs();
                let run = dist_xz(w[0].p, w[1].p);
                // Level until clear of the tube, then a climb a hull takes at speed (hover step limit 4.0).
                // SMOKE_005 item 17: every ramp stays under the hover pathing `steep` limit (0.53,
                // pathing/grid.rs), so hover AI units price no ramp as a cliff. The mid ramps were 0.69.
                assert!(rise <= 0.53 * run, "{} ramp slope {}", opening.id, rise / run);
            }
            let top = ramp.samples.last().unwrap().p;
            assert!(dist_xz(top, opening.top) < 0.01 && (top[1] - opening.top[1]).abs() < 0.01);
        }
        // Mirror-fair: every passage has its twin at (-x, z) of equal length.
        // The bases are the per-map starts (the host's team starts):
        // The Switchbacks starts on the canyon floor at (±550, -126).
        let base_w = [-550.0, 0.0, -126.0];
        let base_e = [550.0, 0.0, -126.0];
        let west = layout.openings.iter().find(|o| o.id == "west").unwrap().top;
        let east = layout.openings.iter().find(|o| o.id == "east").unwrap().top;
        assert!((dist_xz(west, base_w) - dist_xz(east, base_e)).abs() < 0.01);
        assert!(
            (440.0..=480.0).contains(&dist_xz(west, base_w)),
            "{}",
            dist_xz(west, base_w)
        );
        assert!(
            (len(sampled.passage("ramp_west").unwrap()) - len(sampled.passage("ramp_east").unwrap())).abs()
                < 0.01
        );
        let t = &tube.samples;
        let first = t.first().unwrap().p;
        let last = t.last().unwrap().p;
        assert!((first[0] + last[0]).abs() < 0.01 && (first[2] - last[2]).abs() < 0.01);
    }

    /// T1 "one bake": the baked description (the one file the bake writes)
    /// round-trips through JSON unchanged and rebuilds the exact same
    /// collision mesh, so loading the file instead of the hardcoded splines
    /// cannot change the sim.
    #[test]
    fn the_baked_description_round_trips_to_the_same_mesh() {
        let terrain = canyon_terrain();
        let layout = switchbacks();
        let baked = serde_json::to_string(&layout).expect("serialize the description");
        let loaded: TunnelLayout = serde_json::from_str(&baked).expect("parse the baked description");
        assert_eq!(loaded, layout, "the description round-trips unchanged");
        assert_eq!(
            loaded.mesh(Some(&terrain)),
            layout.mesh(Some(&terrain)),
            "the loaded description rebuilds the same collision mesh"
        );
    }

    /// T1: Switchbacks is the only map with armed tunnels, so its sim must be
    /// byte-identical before and after the one-bake switch. The collision mesh
    /// (and the nav overlay baked from it) are the only tunnel inputs to the
    /// sim step; proving the baked-description mesh equals the spline mesh
    /// proves the replay checksums cannot move.
    #[test]
    fn switchbacks_mesh_is_byte_identical_from_the_baked_description() {
        let terrain = canyon_terrain();
        let from_spline = switchbacks().mesh(Some(&terrain));
        let baked: TunnelLayout =
            serde_json::from_str(&serde_json::to_string(&switchbacks()).unwrap()).unwrap();
        let from_baked = baked.mesh(Some(&terrain));
        assert_eq!(from_baked.triangles.len(), from_spline.triangles.len());
        assert_eq!(from_baked, from_spline, "byte-identical triangles, same order");
        assert_eq!(
            crate::compat::mesh::TunnelNavOverlay::bake(&from_baked),
            crate::compat::mesh::TunnelNavOverlay::bake(&from_spline),
            "byte-identical nav overlay"
        );
    }

    #[test]
    fn the_mesh_has_portals_ceilings_walls_and_open_junctions() {
        let layout = switchbacks();
        let mesh = layout.mesh(None);
        let count = |k: TunnelFaceKind| mesh.triangles.iter().filter(|t| t.kind == k).count();
        assert!(count(TunnelFaceKind::Floor) > 500);
        assert!(count(TunnelFaceKind::Wall) > 500);
        assert!(count(TunnelFaceKind::Ceiling) > 0);
        assert_eq!(
            layout
                .openings
                .iter()
                .filter(|o| mesh.is_portal_at(o.top[0], o.top[2] + 0.0) || {
                    let r = layout.sample(None);
                    let s = r.passage(&o.ramp).unwrap().samples.clone();
                    let n = s.len();
                    let q = s[n - 2].p;
                    mesh.is_portal_at((q[0] + s[n - 1].p[0]) * 0.5, (q[2] + s[n - 1].p[2]) * 0.5)
                })
                .count(),
            4
        );
        // At a junction both floors are there and no wall stands across the gallery mouth.
        let j = layout.junctions[0].at;
        assert!(!mesh.floors_at(j[0], j[2]).is_empty());
        let sampled = layout.sample(None);
        let gallery = sampled.passage("gallery").unwrap();
        assert!(
            !gallery.cap_start && !gallery.cap_end,
            "the gallery opens into the tube at both ends"
        );
        let tube = sampled.passage("tube").unwrap();
        assert!(tube.cap_start && tube.cap_end, "the tube ends are dead ends");
        assert!(
            tube.wall_left.iter().chain(&tube.wall_right).any(|w| !w),
            "walls open where passages join"
        );
        // Same description, same mesh.
        assert_eq!(layout.mesh(None), switchbacks().mesh(None));
    }

    #[test]
    fn mouth_hole_contains_respects_yaw() {
        let h = MouthHole {
            opening: "west".into(),
            center: [0.0, 0.0],
            half: [4.0, 2.0],
            yaw: 0.0,
        };
        assert!(h.contains(3.9, 0.0) && h.contains(0.0, 1.9));
        assert!(!h.contains(4.5, 0.0) && !h.contains(0.0, 2.5) && !h.contains(3.0, 2.5));
        // Turned 90 degrees: length and width swap roles.
        let turned = MouthHole {
            yaw: std::f32::consts::FRAC_PI_2,
            ..h.clone()
        };
        assert!(turned.contains(0.0, 3.9) && !turned.contains(3.9, 0.0));
        // 45 degrees: the diagonal reaches half[0] * sqrt(2) along x.
        let diag = MouthHole {
            yaw: std::f32::consts::FRAC_PI_4,
            ..h.clone()
        };
        assert!(diag.contains(2.8, 2.8) && !diag.contains(3.5, 3.5));
    }

    /// T2 acceptance (bake half): one hole rect per open-cut segment, every
    /// open-cut point under a hole, no hole over covered ground, and the whole
    /// mask inside the engine's shape budget.
    #[test]
    fn t2_holes_cover_every_open_cut_and_none_of_the_rock() {
        let terrain = canyon_terrain();
        let layout = switchbacks();
        let sampled = layout.sample(Some(&terrain));
        let holes = sampled.mouth_holes(&layout.openings);
        assert!(!holes.is_empty(), "the mouths cut holes");
        assert!(
            holes.len() <= MAX_HOLE_SHAPES,
            "{} holes over the {MAX_HOLE_SHAPES}-shape budget",
            holes.len()
        );
        for o in &layout.openings {
            let own: Vec<&MouthHole> = holes.iter().filter(|h| h.opening == o.id).collect();
            assert!(!own.is_empty(), "{} cuts no hole", o.id);
            let ramp = sampled.passage(&o.ramp).unwrap();
            for k in 0..ramp.samples.len().saturating_sub(1) {
                let (a, b) = (&ramp.samples[k], &ramp.samples[k + 1]);
                let mid = [
                    (a.p[0] + b.p[0]) * 0.5,
                    (a.p[1] + b.p[1]) * 0.5,
                    (a.p[2] + b.p[2]) * 0.5,
                ];
                if ramp.covered[k] {
                    assert!(
                        !own.iter().any(|h| h.contains(mid[0], mid[2])),
                        "{} segment {k}: covered ground inside a hole",
                        o.id
                    );
                } else {
                    for q in [a.p, b.p, mid] {
                        assert!(
                            own.iter().any(|h| h.contains(q[0], q[2])),
                            "{} segment {k}: open cut at ({:.1}, {:.1}) outside every hole",
                            o.id,
                            q[0],
                            q[2]
                        );
                    }
                }
            }
        }
        // Same description, same mask.
        assert_eq!(
            sampled.mouth_holes(&layout.openings),
            switchbacks().sample(Some(&terrain)).mouth_holes(&layout.openings)
        );
    }

    /// T2: the lips are real mesh. With ground the bake adds floor triangles
    /// (bands, sills, caps) that the terrain-free bake has no room for.
    #[test]
    fn t2_lips_join_the_trench_to_the_ground() {
        let terrain = canyon_terrain();
        let layout = switchbacks();
        let count = |kind: TunnelFaceKind, t: Option<&TerrainSampler>| {
            layout
                .mesh(t)
                .triangles
                .iter()
                .filter(|tri| tri.kind == kind)
                .count()
        };
        let floors = count(TunnelFaceKind::Floor, Some(&terrain));
        assert!(floors > count(TunnelFaceKind::Floor, None), "no lip floors baked");
        // Open-cut walls rise to the ground, not a full ceiling over the sky.
        for tri in &layout.mesh(Some(&terrain)).triangles {
            if tri.kind != TunnelFaceKind::Wall {
                continue;
            }
            let top = det_max(det_max(tri.a[1], tri.b[1]), tri.c[1]);
            assert!(
                top <= -256.0 + LIP_PROUD_M + 2.0,
                "a wall stands over the open ground: {tri:?}"
            );
        }
    }

    /// T2: the bake agrees with itself on the canyon floor, and the check
    /// catches a description whose opening no longer meets its ramp.
    #[test]
    fn t2_bake_check_passes_and_catches_a_broken_mouth() {
        let terrain = canyon_terrain();
        let layout = switchbacks();
        assert!(
            layout.bake_check(Some(&terrain)).is_empty(),
            "{:?}",
            layout.bake_check(Some(&terrain))
        );
        let mut bad = layout.clone();
        bad.openings[0].ramp = "a_ramp_that_is_not_there".into();
        assert!(
            !bad.bake_check(Some(&terrain)).is_empty(),
            "a broken mouth passes the bake"
        );
    }

    /// T2: the export carries the hole mask the renderer cuts, and the
    /// triangle count covers the lips.
    #[test]
    fn t2_export_carries_the_hole_mask() {
        let terrain = canyon_terrain();
        let layout = switchbacks();
        let text = layout.export_json(Some(&terrain));
        let value: serde_json::Value = serde_json::from_str(&text).expect("the export is JSON");
        let holes = value
            .get("holes")
            .and_then(|h| h.as_array())
            .expect("the export carries holes");
        assert_eq!(
            holes.len(),
            layout.sample(Some(&terrain)).mouth_holes(&layout.openings).len()
        );
        assert!(!holes.is_empty());
        let count = value
            .get("triangle_count")
            .and_then(|c| c.as_u64())
            .expect("the export counts triangles");
        assert_eq!(count as usize, layout.mesh(Some(&terrain)).triangles.len());
    }

    /// Review rev-b L6 (ADR-8166): `centreline_sample_bound` bounds what `sample` makes for every fixture passage,
    /// and every fixture sits inside the baked-file caps with room to spare.
    #[test]
    fn the_resample_bound_holds_and_the_fixtures_fit_the_caps() {
        use crate::compat::pack::{
            MAX_TUNNEL_CONTROL_POINTS, MAX_TUNNEL_ITEMS, MAX_TUNNEL_SAMPLES, MAX_TUNNEL_TRIANGLES,
        };
        for id in ["switchbacks", "atacama_rail", "cinder_steps"] {
            let layout = for_map(id).unwrap();
            let sampled = layout.sample(None);
            let mut bound_total = 0usize;
            for (spec, p) in layout.passages.iter().zip(&sampled.passages) {
                let bound = centreline_sample_bound(&spec.points, layout.sample_step_m);
                assert!(
                    p.samples.len() <= bound,
                    "{id}/{}: {} samples over the bound {bound}",
                    p.id,
                    p.samples.len()
                );
                bound_total += bound;
            }
            let samples: usize = sampled.passages.iter().map(|p| p.samples.len()).sum();
            let points: usize = layout.passages.iter().map(|p| p.points.len()).sum();
            let tris = sampled.triangles(&layout.openings, None).len() as u64;
            println!(
                "{id}: {samples} samples, bound {bound_total}, {points} control points, {tris} triangles"
            );
            assert!(
                bound_total * 4 <= MAX_TUNNEL_SAMPLES,
                "{id}: resample bound {bound_total}"
            );
            assert!(samples * 16 <= MAX_TUNNEL_SAMPLES, "{id}: {samples} samples");
            assert!(
                points * 16 <= MAX_TUNNEL_CONTROL_POINTS,
                "{id}: {points} control points"
            );
            assert!(layout.passages.len() + layout.openings.len() < MAX_TUNNEL_ITEMS / 16);
            assert!(tris * 16 <= MAX_TUNNEL_TRIANGLES, "{id}: {tris} triangles");
        }
        // Fewer than two points sample to nothing; a NaN length is refused (`usize::MAX`).
        assert_eq!(centreline_sample_bound(&[[0.0; 3]], 5.0), 0);
        assert_eq!(
            centreline_sample_bound(&[[0.0; 3], [f32::NAN, 0.0, 0.0]], 5.0),
            usize::MAX
        );
    }
}
