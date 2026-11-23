// SPDX-License-Identifier: MPL-2.0
//! Sections, portals, zones and blocker plugs of a `Passages` layer (spec 121 § 5.6, § 5.7; plan row W-08,
//! ADR-8210). Cook-time only: the run time reads the results from the canonical section and never runs this.
//!
//! **The cut rule** (spec 121 § 5.6, original rules; DO27's requirements from its tunnel layout and shell):
//! a passage is cut into sections at sample boundaries, at
//! - every **cover change** (`SampledPassage::covered` differs between two segments), so a section is either covered
//!   (a lid over every floor) or an open cut;
//! - every **junction** the layout lists, at the member's sample nearest the junction point (an interior sample only;
//!   a passage that ends at a junction is joined by a portal, not cut);
//! - both ends of every **blocker run** (`BlockerSource`, snapped to the nearest samples): a run is one section, never
//!   split further, and a cover change or junction inside a run is refused;
//! - and, between those, by the **chord rule**: a section grows sample by sample while every sample between its ends
//!   lies within `section_chord_m` (default 4 m) of the straight chord between them (3D, `f64`, squared distances).
//!
//! **Portals**: a section cut inside a passage (the cross-section at the cut sample), an open passage end (a passage
//! that starts or ends inside another: the cross-section at that end, joined to the other passage's section nearest
//! it), and a sky portal per open-cut section (its top outline, joined to the outdoor cell; `mouth` names the opening
//! whose ramp it is). With every portal closed no section reaches another (W-21's VisCell input).
//!
//! **Skylight shafts (K8)**: a skylight opening (kind `skylight`, `skylight_*` or `shaft`) is a shaft open to the
//! sky: [`open_skylight_shafts`] marks its ramp's climb (every segment from where the ramp leaves its base passage to
//! the top) as an open cut before meshing, so the pinned mesher writes no lid there and raises the walls to the
//! ground, and the climb's footprint is a mouth (the terrain cut, W-10).
//!
//! The arithmetic is `f64` and cook-only (transcendentals through `regia_detsim::math`); the results are integer
//! millimetres and the bytes are host-independent.

use regia_detsim::float::{det_clamp_f64, det_max_f64, det_min_f64};
use std::collections::BTreeMap;

use crate::compat::layout::{Opening, PassageKind, Sample, SampledLayout, SampledPassage, TunnelLayout};
use crate::compat::mesh::{TunnelFaceKind, TunnelTriangle};
use crate::compat::source_math;
use crate::cook::WorldTriangleMm;
use crate::cooked::{
    CookedPlug, CookedPortal, CookedSection, CookedZone, CookedZoneShape, TriKind, TriRange,
};
use crate::meshlayers::{BlockerSource, SectionRule};
use crate::units::m_to_mm;

/// A section zone's covered exposure band (EV100 low, high) and blend distance, metres (spec 121 § 5.6 defaults).
pub const ZONE_EXPOSURE: [f32; 3] = [8.5, 10.5, 25.0];
/// How far a zone's box reaches past the section's free space on every side, metres.
pub const ZONE_PAD_M: f64 = 0.25;
/// Two open-end portals that join the same two sections with nav nodes this close (on every axis, millimetres) are
/// one portal (two passages that start at one point open into each other once).
pub const END_PORTAL_MERGE_MM: i64 = 2_000;
/// Reverb preset of an open-cut section (DO27's `AUDIO_PRESETS` "open").
pub const REVERB_OPEN: &str = "open";
/// Reverb preset of a covered tube or ramp section (DO27's `AUDIO_PRESETS` "tube").
pub const REVERB_TUBE: &str = "tube";
/// Reverb preset of a covered gallery section (DO27's `AUDIO_PRESETS` "gallery").
pub const REVERB_GALLERY: &str = "gallery";

/// Whether an opening kind is a skylight shaft (`skylight`, `skylight_*`, or the description format's `shaft`).
#[must_use]
pub fn is_skylight_kind(kind: &str) -> bool {
    kind == "skylight" || kind.starts_with("skylight_") || kind == "shaft"
}

/// K8: marks the climb of every skylight opening's ramp as an open cut (`covered = false`): every segment whose far
/// sample is off the ramp's level start (the ramp has left its base passage and climbs). The pinned mesher then
/// writes no lid over the shaft and raises its walls to the ground. Returns the number of segments opened (already
/// open segments are not counted).
pub fn open_skylight_shafts(sampled: &mut SampledLayout, openings: &[Opening]) -> u32 {
    let mut opened = 0u32;
    for o in openings.iter().filter(|o| is_skylight_kind(&o.kind)) {
        let Some(p) = sampled
            .passages
            .iter_mut()
            .find(|p| p.id == o.ramp && p.kind == PassageKind::Ramp)
        else {
            continue;
        };
        let Some(y0) = p.samples.first().map(|s| s.p[1]) else {
            continue;
        };
        for k in 0..p.covered.len() {
            let climbs = p.samples.get(k + 1).is_some_and(|s| s.p[1] != y0);
            if climbs && p.covered[k] {
                p.covered[k] = false;
                opened += 1;
            }
        }
    }
    opened
}

/// The segment of a passage a canonical triangle came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentOwner {
    /// The passage (index in the sampled layout).
    pub passage: u16,
    /// The segment (between samples `segment` and `segment + 1`).
    pub segment: u32,
}

/// The mesher's `edge` point, in its arithmetic order (`compat::layout::SampledLayout::triangles`).
fn edge(s: &Sample, half: f32, sign: f32, up: f32) -> [f32; 3] {
    [
        s.p[0] + s.left[0] * half * sign,
        s.p[1] + up,
        s.p[2] + s.left[1] * half * sign,
    ]
}

fn same_tri(t: &TunnelTriangle, kind: TunnelFaceKind, v: [[f32; 3]; 3]) -> bool {
    t.kind == kind && [t.a, t.b, t.c] == v
}

/// Which passage segment each triangle of the pinned mesher's output came from. The mesher writes passage by passage
/// and segment by segment, each segment starting with its floor triangle `(left a, right a, right b)`, and each
/// passage's end caps last; this finds those markers (bit-exact, same arithmetic) and gives every triangle between
/// two markers to the earlier segment, the start cap to segment 0. A passage with fewer than two samples owns no
/// segment; its triangles stay with the segment before (`None` when there is none).
///
/// # Errors
///
/// A marker is missing (the triangles are not this sampled layout's mesh).
pub fn trace_segments(
    sampled: &SampledLayout,
    raw: &[TunnelTriangle],
) -> Result<Vec<Option<SegmentOwner>>, String> {
    let mut owners = vec![None; raw.len()];
    let mut cursor = 0usize;
    let mut current: Option<SegmentOwner> = None;
    // (passage, its first triangle) for every passage with a segment.
    let mut firsts: Vec<(usize, usize)> = Vec::new();
    for (pi, p) in sampled.passages.iter().enumerate() {
        let half = p.width_m * 0.5;
        let nseg = p.samples.len().saturating_sub(1);
        for k in 0..nseg {
            let (a, b) = (&p.samples[k], &p.samples[k + 1]);
            let marker = [
                edge(a, half, 1.0, 0.0),
                edge(a, half, -1.0, 0.0),
                edge(b, half, -1.0, 0.0),
            ];
            let Some(m) = (cursor..raw.len()).find(|&i| same_tri(&raw[i], TunnelFaceKind::Floor, marker))
            else {
                return Err(format!(
                    "passage `{}` segment {k}: its floor is not in the mesh",
                    p.id
                ));
            };
            for o in &mut owners[cursor..m] {
                *o = current;
            }
            if k == 0 {
                firsts.push((pi, m));
            }
            cursor = m;
            current = Some(SegmentOwner {
                passage: pi as u16,
                segment: k as u32,
            });
        }
    }
    for o in &mut owners[cursor..] {
        *o = current;
    }
    // The start cap (two walls at the first sample, written after the last segment) belongs to segment 0.
    for (j, &(pi, f)) in firsts.iter().enumerate() {
        let end = firsts.get(j + 1).map_or(raw.len(), |n| n.1);
        let p = &sampled.passages[pi];
        if !p.cap_start {
            continue;
        }
        let Some(s) = p.samples.first() else {
            continue;
        };
        let half = p.width_m * 0.5;
        let (l, r, lt, rt) = (
            edge(s, half, 1.0, 0.0),
            edge(s, half, -1.0, 0.0),
            edge(s, half, 1.0, p.ceiling_m),
            edge(s, half, -1.0, p.ceiling_m),
        );
        let caps = [[l, r, rt], [l, rt, lt]];
        // Search from the passage's end backwards: the start cap is written just before the end cap.
        for cap in caps {
            if let Some(i) = (f..end)
                .rev()
                .find(|&i| same_tri(&raw[i], TunnelFaceKind::Wall, cap))
            {
                owners[i] = Some(SegmentOwner {
                    passage: pi as u16,
                    segment: 0,
                });
            }
        }
    }
    Ok(owners)
}

fn p64(p: [f32; 3]) -> [f64; 3] {
    p.map(f64::from)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Squared 3D distance from `q` to the segment `a b`.
fn seg_dist2(q: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let ab = sub(b, a);
    let l2 = dot(ab, ab);
    let t = if l2 > 0.0 {
        det_clamp_f64(dot(sub(q, a), ab) / l2, 0.0, 1.0)
    } else {
        0.0
    };
    let c = [a[0] + ab[0] * t, a[1] + ab[1] * t, a[2] + ab[2] * t];
    let d = sub(q, c);
    dot(d, d)
}

/// Whether every sample strictly between `a` and `b` is within `chord_m` of the chord `a b`.
fn chord_holds(samples: &[Sample], a: usize, b: usize, chord_m: f64) -> bool {
    let (pa, pb) = (p64(samples[a].p), p64(samples[b].p));
    let limit = chord_m * chord_m;
    samples[a + 1..b]
        .iter()
        .all(|s| seg_dist2(p64(s.p), pa, pb) <= limit)
}

/// The sample of `samples` nearest `s_m` by arc length (ties to the lower index).
fn nearest_by_s(samples: &[Sample], s_m: f32) -> usize {
    let mut best = (f64::INFINITY, 0usize);
    for (i, q) in samples.iter().enumerate() {
        let d = (f64::from(q.s) - f64::from(s_m)).abs();
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1
}

/// The sample of `samples` nearest the point (3D, ties to the lower index).
fn nearest_to(samples: &[Sample], at: [f32; 3]) -> usize {
    let mut best = (f64::INFINITY, 0usize);
    for (i, q) in samples.iter().enumerate() {
        let d = sub(p64(q.p), p64(at));
        let d2 = dot(d, d);
        if d2 < best.0 {
            best = (d2, i);
        }
    }
    best.1
}

/// One section of a passage before ids are global: samples `start..=end` (segments `start..end`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassageSection {
    /// The passage (index in the sampled layout).
    pub passage: usize,
    /// First sample.
    pub start: usize,
    /// Last sample (`> start`).
    pub end: usize,
    /// The blocker run (index in the layer's `blockers`) that makes it collapsible.
    pub blocker: Option<usize>,
}

/// Why a layer's sections could not be cut (`meshlayers_invalid`: the document asks for something the layout cannot
/// give, or `cook_internal`: the triangles are not this layout's mesh).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SectionsError {
    /// A blocker names an unknown passage, overlaps another, is shorter than one segment, or has a cover change or a
    /// junction inside it.
    #[error("{0}")]
    Blocker(String),
    /// The segment trace failed.
    #[error("section trace: {0}")]
    Trace(String),
    /// More than 65535 sections, portals or plugs.
    #[error("more than 65535 {0}")]
    TooMany(&'static str),
}

/// Cuts every passage of a sampled layout into sections (the rule in the module docs), in passage order, then sample
/// order.
///
/// # Errors
///
/// [`SectionsError::Blocker`].
pub fn cut_sections(
    sampled: &SampledLayout,
    layout: &TunnelLayout,
    rule: &SectionRule,
    blockers: &[BlockerSource],
) -> Result<Vec<PassageSection>, SectionsError> {
    for b in blockers {
        if !sampled.passages.iter().any(|p| p.id == b.passage) {
            return Err(SectionsError::Blocker(format!(
                "blocker on `{}`: no such passage in the layer",
                b.passage
            )));
        }
    }
    let chord_m = f64::from(rule.section_chord_m);
    let mut out = Vec::new();
    for (pi, p) in sampled.passages.iter().enumerate() {
        let n = p.samples.len();
        if n < 2 {
            continue;
        }
        let mut cuts: Vec<usize> = Vec::new();
        for k in 1..p.covered.len() {
            if p.covered[k] != p.covered[k - 1] {
                cuts.push(k);
            }
        }
        let mut junction_cuts = Vec::new();
        for j in layout.junctions.iter().filter(|j| j.passages.contains(&p.id)) {
            let i = nearest_to(&p.samples, j.at);
            if i > 0 && i + 1 < n {
                junction_cuts.push(i);
            }
        }
        cuts.extend_from_slice(&junction_cuts);
        let mut runs: Vec<(usize, usize, usize)> = Vec::new();
        for (bi, b) in blockers.iter().enumerate().filter(|(_, b)| b.passage == p.id) {
            let i0 = nearest_by_s(&p.samples, b.from_s_m);
            let mut i1 = nearest_by_s(&p.samples, b.to_s_m);
            if i1 <= i0 {
                i1 = i0 + 1;
            }
            if i1 >= n {
                return Err(SectionsError::Blocker(format!(
                    "blocker on `{}` from {} m to {} m is shorter than one segment at the passage end",
                    b.passage, b.from_s_m, b.to_s_m
                )));
            }
            runs.push((i0, i1, bi));
        }
        runs.sort_unstable();
        for w in runs.windows(2) {
            if w[1].0 < w[0].1 {
                return Err(SectionsError::Blocker(format!(
                    "two blocker runs overlap on `{}`",
                    p.id
                )));
            }
        }
        for &(i0, i1, _) in &runs {
            if let Some(c) = cuts.iter().find(|&&c| c > i0 && c < i1) {
                return Err(SectionsError::Blocker(format!(
                    "blocker run on `{}` (samples {i0}..{i1}) crosses a cover change or junction at sample {c}",
                    p.id
                )));
            }
            cuts.push(i0);
            cuts.push(i1);
        }
        cuts.retain(|&c| c > 0 && c + 1 < n);
        cuts.push(0);
        cuts.push(n - 1);
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (a0, b0) = (w[0], w[1]);
            if let Some(&(_, _, bi)) = runs.iter().find(|r| r.0 == a0 && r.1 == b0) {
                out.push(PassageSection {
                    passage: pi,
                    start: a0,
                    end: b0,
                    blocker: Some(bi),
                });
                continue;
            }
            let mut a = a0;
            while a < b0 {
                let mut b = a + 1;
                while b < b0 && chord_holds(&p.samples, a, b + 1, chord_m) {
                    b += 1;
                }
                out.push(PassageSection {
                    passage: pi,
                    start: a,
                    end: b,
                    blocker: None,
                });
                a = b;
            }
        }
    }
    Ok(out)
}

/// What the section step adds to one layer: ids are global (they start after the previous layers').
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerSections {
    /// The sections.
    pub sections: Vec<CookedSection>,
    /// The portals.
    pub portals: Vec<CookedPortal>,
    /// One zone per section, same order.
    pub zones: Vec<CookedZone>,
    /// The blocker plugs.
    pub plugs: Vec<CookedPlug>,
    /// The section of each canonical triangle of the layer (passage triangles, then plug triangles), by index.
    pub triangle_section: Vec<Option<u16>>,
    /// The plug triangles, appended to the layer after its passage triangles (spec 121 § 5.3 rule 6, § 5.7).
    pub plug_triangles: Vec<WorldTriangleMm>,
}

/// Where the ids of a layer start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdBase {
    /// First section id.
    pub section: usize,
    /// First portal id.
    pub portal: usize,
    /// First plug id.
    pub plug: usize,
}

/// The inputs of [`layer_sections`].
pub struct SectionInputs<'a> {
    /// The layer index.
    pub layer: u16,
    /// The layer id (each section's `source` is `<layer>/<passage>`).
    pub layer_id: &'a str,
    /// The layout as meshed (skylight shafts already opened).
    pub sampled: &'a SampledLayout,
    /// The layout (junctions and openings).
    pub layout: &'a TunnelLayout,
    /// The mesher's triangles, before quantization (for the segment trace).
    pub raw: &'a [TunnelTriangle],
    /// The canonical triangles (same order as `raw`).
    pub canonical: &'a [WorldTriangleMm],
    /// The section rule.
    pub rule: &'a SectionRule,
    /// The layer's blocker runs.
    pub blockers: &'a [BlockerSource],
    /// Where this layer's ids start.
    pub base: IdBase,
}

fn mm(p: [f64; 3]) -> [i32; 3] {
    p.map(|v| m_to_mm(v as f32))
}

/// The cross-section of a passage at a sample: floor left, floor right, lid right, lid left, world mm.
fn cross_section(p: &SampledPassage, s: &Sample) -> Vec<[i32; 3]> {
    let half = p.width_m * 0.5;
    vec![
        edge(s, half, 1.0, 0.0),
        edge(s, half, -1.0, 0.0),
        edge(s, half, -1.0, p.ceiling_m),
        edge(s, half, 1.0, p.ceiling_m),
    ]
    .into_iter()
    .map(|q| mm(p64(q)))
    .collect()
}

/// A sample's floor centre, world mm (a nav node).
fn floor_mm(s: &Sample) -> [i32; 3] {
    mm(p64(s.p))
}

/// The point and left normal at arc length `s_m` along a passage (linear between samples, the normal renormalized).
fn at_s(samples: &[Sample], s_m: f64) -> ([f64; 3], [f64; 2]) {
    let last = samples.len() - 1;
    let mut k = 0;
    while k + 1 < last && f64::from(samples[k + 1].s) < s_m {
        k += 1;
    }
    let (a, b) = (&samples[k], &samples[k + 1]);
    let span = f64::from(b.s) - f64::from(a.s);
    let t = if span > 0.0 {
        det_clamp_f64((s_m - f64::from(a.s)) / span, 0.0, 1.0)
    } else {
        0.0
    };
    let (pa, pb) = (p64(a.p), p64(b.p));
    let p = [
        pa[0] + (pb[0] - pa[0]) * t,
        pa[1] + (pb[1] - pa[1]) * t,
        pa[2] + (pb[2] - pa[2]) * t,
    ];
    let l = [
        f64::from(a.left[0]) + (f64::from(b.left[0]) - f64::from(a.left[0])) * t,
        f64::from(a.left[1]) + (f64::from(b.left[1]) - f64::from(a.left[1])) * t,
    ];
    let len = regia_detsim::math::sqrt_f64(l[0] * l[0] + l[1] * l[1]);
    let l = if len > 0.0 {
        [l[0] / len, l[1] / len]
    } else {
        [f64::from(a.left[0]), f64::from(a.left[1])]
    };
    (p, l)
}

/// The plug of a collapsible section: the passage cross-section over `length_m` centred on the section's middle (both
/// end faces and the top, kind `Blocker`), world mm.
fn plug_triangles(p: &SampledPassage, start: usize, end: usize, length_m: f64) -> Vec<WorldTriangleMm> {
    let (s0, s1) = (f64::from(p.samples[start].s), f64::from(p.samples[end].s));
    let mid = (s0 + s1) * 0.5;
    let half_len = det_min_f64(length_m * 0.5, (s1 - s0) * 0.5);
    let half_w = f64::from(p.width_m) * 0.5;
    let h = f64::from(p.ceiling_m);
    let corners = |s: f64| -> [[i32; 3]; 4] {
        let (c, l) = at_s(&p.samples, s);
        let side = |sign: f64, up: f64| {
            mm([
                c[0] + l[0] * half_w * sign,
                c[1] + up,
                c[2] + l[1] * half_w * sign,
            ])
        };
        [side(1.0, 0.0), side(-1.0, 0.0), side(-1.0, h), side(1.0, h)]
    };
    let [fl0, fr0, tr0, tl0] = corners(mid - half_len);
    let [fl1, fr1, tr1, tl1] = corners(mid + half_len);
    let tri = |a, b, c| WorldTriangleMm {
        v: [a, b, c],
        kind: TriKind::Blocker,
        flags: 0,
    };
    let mut top = [tri(tl0, tr0, tr1), tri(tl0, tr1, tl1)];
    for t in &mut top {
        // Up-facing: the y of (b - a) x (c - a) is positive, else swap b and c.
        let [a, b, c] = t.v.map(|q| q.map(i64::from));
        let ny = (c[0] - a[0]) * (b[2] - a[2]) - (b[0] - a[0]) * (c[2] - a[2]);
        if ny < 0 {
            t.v.swap(1, 2);
        }
    }
    vec![
        tri(fl0, fr0, tr0),
        tri(fl0, tr0, tl0),
        tri(fl1, tr1, fr1),
        tri(fl1, tl1, tr1),
        top[0],
        top[1],
    ]
}

/// The zone of a section: an oriented box along its chord around the passage's free space (floor and lid edges at
/// every sample), padded [`ZONE_PAD_M`].
fn zone_box(p: &SampledPassage, start: usize, end: usize) -> CookedZoneShape {
    let (a, b) = (p.samples[start].p, p.samples[end].p);
    // The lockstep atan2: signed-zero inputs as +0, so the stored yaw is never -0 or -π (ADR-8579).
    let yaw = source_math::atan2(b[2] - a[2], b[0] - a[0]);
    let (c, s) = (
        f64::from(regia_detsim::math::cos(yaw)),
        f64::from(regia_detsim::math::sin(yaw)),
    );
    let half = f64::from(p.width_m) * 0.5;
    let h = f64::from(p.ceiling_m);
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for q in &p.samples[start..=end] {
        for sign in [1.0, -1.0] {
            for up in [0.0, h] {
                let x = f64::from(q.p[0]) + f64::from(q.left[0]) * half * sign;
                let z = f64::from(q.p[2]) + f64::from(q.left[1]) * half * sign;
                let y = f64::from(q.p[1]) + up;
                let local = [x * c + z * s, y, -x * s + z * c];
                for i in 0..3 {
                    lo[i] = det_min_f64(lo[i], local[i]);
                    hi[i] = det_max_f64(hi[i], local[i]);
                }
            }
        }
    }
    let mid = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let center = [mid[0] * c - mid[2] * s, mid[1], mid[0] * s + mid[2] * c];
    // Metres cross to millimetres through `m_to_mm`, like every other cooked length (ADR-8579).
    let half_mm = [0, 1, 2].map(|i| m_to_mm(((hi[i] - lo[i]) * 0.5 + ZONE_PAD_M) as f32));
    CookedZoneShape::OrientedBox {
        center_mm: mm(center),
        half_mm,
        yaw,
    }
}

/// The reverb size of a passage's sections, metres: the mean of its width and ceiling height.
/// Stored with `-0` as `+0` (ADR-8579).
fn reverb_size_m(p: &SampledPassage) -> f32 {
    let v = (p.width_m + p.ceiling_m) * 0.5;
    if v == 0.0 { 0.0 } else { v }
}

/// The reverb preset of a section: `open` for an open cut, `gallery` for a gallery, `tube` otherwise.
#[must_use]
pub fn reverb_preset(kind: PassageKind, covered: bool) -> &'static str {
    match (covered, kind) {
        (false, _) => REVERB_OPEN,
        (true, PassageKind::Gallery) => REVERB_GALLERY,
        (true, _) => REVERB_TUBE,
    }
}

fn id16(i: usize, what: &'static str) -> Result<u16, SectionsError> {
    u16::try_from(i).map_err(|_| SectionsError::TooMany(what))
}

fn ranges(layer: u16, mut indices: Vec<u32>) -> Vec<TriRange> {
    indices.sort_unstable();
    let mut out: Vec<TriRange> = Vec::new();
    for i in indices {
        match out.last_mut() {
            Some(r) if r.end == i => r.end += 1,
            _ => out.push(TriRange {
                layer,
                start: i,
                end: i + 1,
            }),
        }
    }
    out
}

/// Cuts one `Passages` layer into sections and emits their portals, zones and plugs (module docs).
///
/// # Errors
///
/// [`SectionsError`].
pub fn layer_sections(inp: &SectionInputs<'_>) -> Result<LayerSections, SectionsError> {
    let owners = trace_segments(inp.sampled, inp.raw).map_err(SectionsError::Trace)?;
    let cut = cut_sections(inp.sampled, inp.layout, inp.rule, inp.blockers)?;
    // (passage, segment) -> local section index.
    let mut seg_section: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for (i, s) in cut.iter().enumerate() {
        for k in s.start..s.end {
            seg_section.insert((s.passage, k), i);
        }
    }
    let local_of = |o: Option<SegmentOwner>| {
        o.and_then(|o| {
            seg_section
                .get(&(usize::from(o.passage), o.segment as usize))
                .copied()
        })
    };
    let mut own: Vec<Vec<u32>> = vec![Vec::new(); cut.len()];
    for (i, o) in owners.iter().enumerate() {
        if let Some(l) = local_of(*o) {
            own[l].push(i as u32);
        }
    }
    let gid = |l: usize| id16(inp.base.section + l, "sections");
    let mut out = LayerSections {
        triangle_section: Vec::with_capacity(inp.canonical.len()),
        ..LayerSections::default()
    };
    for o in &owners {
        out.triangle_section.push(local_of(*o).map(gid).transpose()?);
    }
    // Plugs: appended after every passage triangle, in section order.
    let mut next_index = inp.canonical.len() as u32;
    let mut plug_of: Vec<Option<u16>> = vec![None; cut.len()];
    for (l, s) in cut.iter().enumerate() {
        let Some(bi) = s.blocker else {
            continue;
        };
        let p = &inp.sampled.passages[s.passage];
        let tris = plug_triangles(p, s.start, s.end, f64::from(inp.rule.plug_length_m));
        let id = id16(inp.base.plug + out.plugs.len(), "plugs")?;
        let start = next_index;
        next_index += tris.len() as u32;
        for t in tris {
            out.plug_triangles.push(t);
            out.triangle_section.push(Some(gid(l)?));
        }
        out.plugs.push(CookedPlug {
            id,
            section: gid(l)?,
            initially_active: inp.blockers[bi].initially_active,
            triangles: TriRange {
                layer: inp.layer,
                start,
                end: next_index,
            },
        });
        plug_of[l] = Some(id);
    }
    // Portals.
    let mut portals: Vec<CookedPortal> = Vec::new();
    let mut push_portal = |sides: [Option<u16>; 2],
                           polygon_mm: Vec<[i32; 3]>,
                           nav_mm: [i32; 3],
                           mouth: Option<String>,
                           blocker: Option<u16>| {
        portals.push(CookedPortal {
            id: 0,
            sides,
            polygon_mm,
            nav_mm,
            mouth,
            blocker,
        });
    };
    // 1. Cuts inside a passage.
    for (l, w) in cut.windows(2).enumerate() {
        let (a, b) = (&w[0], &w[1]);
        if a.passage != b.passage {
            continue;
        }
        let p = &inp.sampled.passages[a.passage];
        let s = &p.samples[a.end];
        push_portal(
            [Some(gid(l)?), Some(gid(l + 1)?)],
            cross_section(p, s),
            floor_mm(s),
            None,
            plug_of[l].or(plug_of[l + 1]),
        );
    }
    // 2. Open passage ends: joined to the other passage's section nearest the end (ties to the lower passage, then
    // the lower segment).
    let mut end_seen: Vec<([Option<u16>; 2], [i32; 3])> = Vec::new();
    let top_of: BTreeMap<&str, &Opening> = inp.layout.openings.iter().map(|o| (o.ramp.as_str(), o)).collect();
    for (pi, p) in inp.sampled.passages.iter().enumerate() {
        let n = p.samples.len();
        if n < 2 {
            continue;
        }
        let is_top = p.kind == PassageKind::Ramp && top_of.contains_key(p.id.as_str());
        let ends = [
            (!p.cap_start, 0usize, 0usize),
            (!p.cap_end && !is_top, n - 1, n - 2),
        ];
        for (open, sample, segment) in ends {
            if !open {
                continue;
            }
            let Some(&own_l) = seg_section.get(&(pi, segment)) else {
                continue;
            };
            let q = p64(p.samples[sample].p);
            let mut best: Option<(f64, usize)> = None;
            for (oi, o) in inp.sampled.passages.iter().enumerate() {
                if oi == pi {
                    continue;
                }
                for (k, w) in o.samples.windows(2).enumerate() {
                    let d = seg_dist2(q, p64(w[0].p), p64(w[1].p));
                    if best.is_none_or(|(bd, _)| d < bd)
                        && let Some(&l) = seg_section.get(&(oi, k))
                    {
                        best = Some((d, l));
                    }
                }
            }
            let Some((_, host)) = best else {
                continue;
            };
            let s = &p.samples[sample];
            let (lo, hi) = (own_l.min(host), own_l.max(host));
            let sides = [Some(gid(lo)?), Some(gid(hi)?)];
            let nav = floor_mm(s);
            // Two passages that start at one point (a ramp leaving a tube's end) open into each other: one portal.
            let near = |q: &[i32; 3]| {
                (0..3).all(|c| (i64::from(q[c]) - i64::from(nav[c])).abs() <= END_PORTAL_MERGE_MM)
            };
            if end_seen.iter().any(|(sd, q)| *sd == sides && near(q)) {
                continue;
            }
            end_seen.push((sides, nav));
            push_portal(
                sides,
                cross_section(p, s),
                nav,
                None,
                plug_of[own_l].or(plug_of[host]),
            );
        }
    }
    // 3. Sky portals: every open-cut section opens to the outdoor cell; on an opening's ramp it rings that mouth. A
    // covered top section of an opening's ramp gets the mouth portal at the top cross-section.
    for (l, s) in cut.iter().enumerate() {
        let p = &inp.sampled.passages[s.passage];
        let mouth = top_of
            .get(p.id.as_str())
            .filter(|_| p.kind == PassageKind::Ramp)
            .map(|o| o.id.clone());
        let open = !p.covered[s.start];
        let is_top_section = s.end + 1 == p.samples.len();
        if open {
            let half = p.width_m * 0.5;
            let run = &p.samples[s.start..=s.end];
            let mut polygon: Vec<[i32; 3]> = run
                .iter()
                .map(|q| mm(p64(edge(q, half, 1.0, p.ceiling_m))))
                .collect();
            polygon.extend(
                run.iter()
                    .rev()
                    .map(|q| mm(p64(edge(q, half, -1.0, p.ceiling_m)))),
            );
            let nav = if is_top_section && mouth.is_some() {
                floor_mm(&p.samples[s.end])
            } else {
                floor_mm(&p.samples[(s.start + s.end) / 2])
            };
            push_portal([Some(gid(l)?), None], polygon, nav, mouth, plug_of[l]);
        } else if is_top_section && mouth.is_some() {
            let q = &p.samples[s.end];
            push_portal(
                [Some(gid(l)?), None],
                cross_section(p, q),
                floor_mm(q),
                mouth,
                plug_of[l],
            );
        }
    }
    for (i, portal) in portals.iter_mut().enumerate() {
        portal.id = id16(inp.base.portal + i, "portals")?;
    }
    // Sections and zones.
    for (l, s) in cut.iter().enumerate() {
        let p = &inp.sampled.passages[s.passage];
        let id = gid(l)?;
        let covered = p.covered[s.start];
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for &i in &own[l] {
            for v in inp.canonical[i as usize].v {
                for c in 0..3 {
                    lo[c] = lo[c].min(v[c]);
                    hi[c] = hi[c].max(v[c]);
                }
            }
        }
        if own[l].is_empty() {
            for q in &p.samples[s.start..=s.end] {
                let v = floor_mm(q);
                for c in 0..3 {
                    lo[c] = lo[c].min(v[c]);
                    hi[c] = hi[c].max(v[c]);
                }
            }
        }
        let mut section_portals: Vec<u16> = portals
            .iter()
            .filter(|q| q.sides.contains(&Some(id)))
            .map(|q| q.id)
            .collect();
        section_portals.sort_unstable();
        out.sections.push(CookedSection {
            id,
            source: format!("{}/{}", inp.layer_id, p.id),
            covered,
            bounds_mm: [lo, hi],
            triangles: ranges(inp.layer, std::mem::take(&mut own[l])),
            portals: section_portals,
            blocker: plug_of[l],
        });
        out.zones.push(CookedZone {
            section: id,
            shape: zone_box(p, s.start, s.end),
            exposure: ZONE_EXPOSURE,
            reverb_preset: reverb_preset(p.kind, covered).to_string(),
            reverb_size_m: reverb_size_m(p),
        });
    }
    out.portals = portals;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight two-sample passage from `a` to `b` (floor centres, metres).
    fn two_sample(a: [f32; 3], b: [f32; 3]) -> SampledPassage {
        let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
        let len = regia_detsim::math::sqrt_f64(f64::from(dx * dx + dz * dz)) as f32;
        let left = [dz / len, -dx / len];
        SampledPassage {
            id: "p".to_string(),
            kind: PassageKind::Tube,
            width_m: 6.0,
            ceiling_m: 5.0,
            samples: vec![Sample { p: a, left, s: 0.0 }, Sample { p: b, left, s: len }],
            covered: vec![true],
            wall_left: vec![true],
            wall_right: vec![true],
            cap_start: false,
            cap_end: false,
        }
    }

    fn yaw_of(shape: &CookedZoneShape) -> f32 {
        match shape {
            CookedZoneShape::OrientedBox { yaw, .. } => *yaw,
            other => panic!("section zone is not an oriented box: {other:?}"),
        }
    }

    /// Format-freeze review B, hazard: the yaw a section zone stores is the lockstep `atan2` (signed-zero
    /// inputs as `+0`, ADR-8579), so a chord whose z runs from `+0` to `-0` hashes `+0` (`dx > 0`) and
    /// `+π` (`dx < 0`), never `-0` or `-π`.
    #[test]
    fn section_zone_yaw_uses_the_lockstep_atan2() {
        let yaw_pos = source_math::atan2(-0.0, 10.0);
        let yaw_neg = source_math::atan2(-0.0, -10.0);
        assert_eq!(yaw_pos.to_bits(), 0, "zone yaw bits {:08x}", yaw_pos.to_bits());
        assert_eq!(yaw_neg.to_bits(), core::f32::consts::PI.to_bits());
        for (a, b, want) in [
            ([0.0, 0.0, 0.0], [10.0, 0.0, -0.0], 0.0f32),
            ([10.0, 0.0, 0.0], [0.0, 0.0, -0.0], core::f32::consts::PI),
            ([3.0, 1.0, -0.0], [-4.0, 1.0, 0.0], core::f32::consts::PI),
            ([3.0, 1.0, -0.0], [7.0, 1.0, -0.0], 0.0),
        ] {
            let p = two_sample(a, b);
            let yaw = yaw_of(&zone_box(&p, 0, 1));
            let lockstep = source_math::atan2(b[2] - a[2], b[0] - a[0]);
            assert_eq!(
                yaw.to_bits(),
                lockstep.to_bits(),
                "chord {a:?} -> {b:?}: zone yaw {yaw} ({:08x}), lockstep atan2 {lockstep} ({:08x})",
                yaw.to_bits(),
                lockstep.to_bits()
            );
            assert_eq!(yaw.to_bits(), want.to_bits(), "chord {a:?} -> {b:?}");
        }
    }

    /// A general chord keeps the bare `atan2f` bits (the canonical steps only touch signed zeros and NaN).
    #[test]
    fn section_zone_yaw_matches_atan2f_off_the_zero_axis() {
        let (a, b) = ([1.5, 0.0, 2.0], [-7.25, 3.0, -11.0]);
        let yaw = yaw_of(&zone_box(&two_sample(a, b), 0, 1));
        assert_eq!(yaw.to_bits(), libm::atan2f(b[2] - a[2], b[0] - a[0]).to_bits());
    }

    /// Review B nit: the zone half extents cross metres to millimetres through [`m_to_mm`] like the rest of
    /// the cook, and still cover the free space plus [`ZONE_PAD_M`] to the millimetre.
    #[test]
    fn section_zone_half_extents_cross_through_m_to_mm() {
        let p = two_sample([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]);
        let CookedZoneShape::OrientedBox { half_mm, .. } = zone_box(&p, 0, 1) else {
            panic!("not an oriented box");
        };
        // Chord along +x: half length 5 m, half height 2.5 m, half width 3 m, each plus 0.25 m.
        assert_eq!(half_mm, [5_250, 2_750, 3_250]);
        let p = two_sample([0.0, 0.0, 0.0], [10.0003, 0.0, 0.0]);
        let CookedZoneShape::OrientedBox { half_mm, .. } = zone_box(&p, 0, 1) else {
            panic!("not an oriented box");
        };
        let want = m_to_mm(((10.0003f32 as f64) * 0.5 + ZONE_PAD_M) as f32);
        assert_eq!(half_mm[0], want);
    }

    /// Review B nit: a section's reverb size is stored with `-0` as `+0`.
    #[test]
    fn section_reverb_size_has_no_negative_zero() {
        let mut p = two_sample([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]);
        p.width_m = -0.0;
        p.ceiling_m = -0.0;
        assert_eq!(reverb_size_m(&p).to_bits(), 0);
        p.width_m = 6.0;
        p.ceiling_m = 5.0;
        assert_eq!(reverb_size_m(&p).to_bits(), 5.5f32.to_bits());
    }
}
