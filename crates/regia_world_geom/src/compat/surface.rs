// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/surface.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-571 (368 kernel, 370-571 its nine tests), mark: as-is (Stage A: unchanged).
// Edits: module paths, doc comments (DO27 file names dropped, missing docs added), float max through
// `regia_detsim::float::det_max` (ADR-8166; the operands are non-negative lengths, so the bits are std's).

//! T3: the one "what is under me / over me" query.
//!
//! The world at one XZ is a [`Column`]: a short list of free [`Span`]s, bottom up. A span is the
//! surface a body stands on and the rock over it. The heightfield and every mesh layer (the
//! tunnels today; overhangs and stacked levels later) go into the same column, so hover physics,
//! bodies on foot, projectiles, the AI and the camera all read one answer.
//!
//! How the sheets combine (pure geometry, no history, no step-height band):
//!
//! * A mesh floor's span runs up to the lowest ceiling over it.
//! * The terrain is one more floor, except inside a mesh span. A mesh floor with no lid under the
//!   terrain cuts it (an open cut: the trench floor is the ground there). A lidded mesh span is
//!   carved space: a baked patch that dips into it is dropped for the grid's own ground (a
//!   skylight shaft patch leaves no ground in the column at all), and ground still between its
//!   floor and its lid is lifted onto the lid (the roof of the carved space is the ground over it).
//! * [`Column::span_at`] picks the span a point is in.
//!
//! Build-time conditioning of the mesh floors, once at load (`support_floors`,
//! `weld_to_terrain`): trim (a thin slab lying on another floor, such as a threshold sill)
//! carries nothing, and a floor vertex modeled a hair proud of the ground is put on it.
//!
//! Deterministic while lockstep lives: f32 add, subtract, multiply, divide and compare only (no
//! trigonometry; the one IEEE `sqrt`, in the shared plane code, normalizes the reported normal
//! and never reaches a height), candidates walked in ascending triangle order from one XZ
//! bucket, ties keep the first candidate, fixed-size arrays, no unordered container.

use regia_detsim::float::det_max;

use crate::compat::ground::outdoor_sample;
use crate::compat::mesh::{
    TriIndex, TunnelCollisionMesh, TunnelFaceKind, TunnelTriangle, intersect_floor_xz, plane_y_and_normal,
};
use crate::compat::sampler::{FloorSample, FloorSurface, TerrainSampler};

/// Most spans one column holds. The Cinder Steps plans five levels.
pub const MAX_SPANS: usize = 12;
/// Most ceilings read in one column.
const MAX_CEILINGS: usize = 16;

/// A body asks for its surface at its waist, this far over its origin (a hull's hover point, a
/// pilot's feet). A floor that rises over the waist in one tick is a wall. This is the query
/// point of a body, the same for every body; a shell or a camera asks at its own height.
pub const BODY_REACH_M: f32 = 0.9;

/// A support-floor vertex up to this far over the terrain is welded onto it (lips and caps are
/// modeled a hair proud of the ground so they draw without z-fight).
pub const WELD_M: f32 = 0.15;

/// A narrow floor strip that lies within this of another floor over its whole footprint is trim
/// (a sill slab, a plate), not a level.
pub const TRIM_M: f32 = 0.5;

/// Only a strip narrower than this in plan can be trim. A passage floor (5 m segments) never is,
/// however close another passage's floor runs under or over it.
pub const TRIM_WIDTH_M: f32 = 2.0;

/// Heights closer than this are one height.
const EPS_M: f32 = 1.0e-3;

/// A lid counts as over a floor when it is more than this above it (as
/// [`TunnelCollisionMesh::ceiling_above`]).
const LID_GAP_M: f32 = 0.05;

/// One free span of a column: the surface under a body in it and the rock over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    /// The surface a body in this span stands on.
    pub floor: FloorSample,
    /// The lowest lid over the floor; `None` under the sky.
    pub ceiling: Option<f32>,
}

impl Span {
    /// The floor height.
    pub fn floor_y(&self) -> f32 {
        self.floor.point[1]
    }

    /// True when rock is over this span.
    pub fn covered(&self) -> bool {
        self.ceiling.is_some()
    }

    /// Height of the free space; `None` under the sky.
    pub fn clearance(&self) -> Option<f32> {
        self.ceiling.map(|lid| lid - self.floor.point[1])
    }
}

/// Every free span at one XZ, bottom up. Never empty.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    len: usize,
    spans: [Span; MAX_SPANS],
}

impl Column {
    fn single(span: Span) -> Self {
        Column {
            len: 1,
            spans: [span; MAX_SPANS],
        }
    }

    /// Insert keeping the spans ascending by floor height; equal heights keep their order.
    fn insert(&mut self, span: Span) {
        if self.len >= MAX_SPANS {
            return;
        }
        let mut at = self.len;
        while at > 0 && self.spans[at - 1].floor.point[1] > span.floor.point[1] {
            self.spans[at] = self.spans[at - 1];
            at -= 1;
        }
        self.spans[at] = span;
        self.len += 1;
    }

    /// The spans, bottom up.
    pub fn spans(&self) -> &[Span] {
        &self.spans[..self.len]
    }

    /// The span a point at height `y` is in: the one with the highest floor at or under `y`.
    /// A point over that span's ceiling is in rock: the nearer of that ceiling and the next
    /// floor up decides (ties to the lower span). With no floor under `y`, the lowest one over
    /// it (the caller's wall and ledge rules see a rising floor).
    pub fn span_at(&self, y: f32) -> Span {
        let spans = self.spans();
        let mut best: Option<usize> = None;
        let mut above: Option<usize> = None;
        for (i, span) in spans.iter().enumerate() {
            let fy = span.floor.point[1];
            if fy <= y {
                // Ascending: a later span is at least as high. Of two floors at one height the
                // open ground stands (a welded lip and the terrain are one surface).
                let take = match best {
                    None => true,
                    Some(b) => {
                        fy > spans[b].floor.point[1] || span.floor.surface == FloorSurface::Heightfield
                    }
                };
                if take {
                    best = Some(i);
                }
            } else if above.is_none_or(|a| {
                fy == spans[a].floor.point[1] && span.floor.surface == FloorSurface::Heightfield
            }) {
                above = Some(i);
            }
        }
        match (best, above) {
            (Some(b), Some(a)) => match spans[b].ceiling {
                Some(lid) if y >= lid => {
                    if y - lid <= spans[a].floor.point[1] - y {
                        spans[b]
                    } else {
                        spans[a]
                    }
                }
                _ => spans[b],
            },
            (Some(b), None) => spans[b],
            (None, Some(a)) => spans[a],
            (None, None) => self.spans[0],
        }
    }

    /// The highest mesh span a body fits in (clearance at least `fit_m`, or open to the sky).
    pub fn highest_mesh_span(&self, fit_m: f32) -> Option<Span> {
        self.spans()
            .iter()
            .rev()
            .find(|s| {
                s.floor.surface == FloorSurface::TunnelFloor && s.clearance().is_none_or(|c| c >= fit_m)
            })
            .copied()
    }
}

/// The column at `(x, z)`: the terrain and every mesh layer.
pub fn column(
    terrain: Option<&TerrainSampler>,
    mesh: Option<&TunnelCollisionMesh>,
    x: f32,
    z: f32,
) -> Column {
    let outdoor = outdoor_sample(terrain, x, z);
    let open = Span {
        floor: outdoor,
        ceiling: None,
    };
    let Some(mesh) = mesh.filter(|m| !m.is_empty()) else {
        return Column::single(open);
    };
    // The lids over this XZ, in triangle order.
    let mut lids = [0.0f32; MAX_CEILINGS];
    let mut lid_count = 0usize;
    for tri in mesh.near(x, z) {
        if tri.kind != TunnelFaceKind::Ceiling || lid_count >= MAX_CEILINGS {
            continue;
        }
        if let Some((y, _)) = intersect_floor_xz(tri, x, z) {
            lids[lid_count] = y;
            lid_count += 1;
        }
    }
    let lid_over = |floor_y: f32| -> Option<f32> {
        let mut best: Option<f32> = None;
        for lid in &lids[..lid_count] {
            if *lid > floor_y + LID_GAP_M && best.is_none_or(|b| *lid < b) {
                best = Some(*lid);
            }
        }
        best
    };
    let ground = outdoor.point[1];
    let mut col = Column {
        len: 0,
        spans: [open; MAX_SPANS],
    };
    // An open mesh floor under the terrain cuts it: the trench floor is the ground there.
    let mut cut = false;
    for tri in mesh.support_near(x, z) {
        let Some((y, normal)) = intersect_floor_xz(tri, x, z) else {
            continue;
        };
        let ceiling = lid_over(y);
        if ceiling.is_none() && y < ground - EPS_M {
            cut = true;
        }
        // Two triangles that answer one height here are one floor (a shared edge, or two
        // passages sharing a junction floor): the first stands. One slot stays free for the terrain.
        let same = col.spans().iter().any(|s| (s.floor.point[1] - y).abs() <= EPS_M);
        if !same && col.len < MAX_SPANS - 1 {
            col.insert(Span {
                floor: FloorSample {
                    point: [x, y, z],
                    normal,
                    surface: FloorSurface::TunnelFloor,
                },
                ceiling,
            });
        }
    }
    if col.len == 0 {
        col.insert(Span {
            floor: outdoor,
            ceiling: lid_over(ground),
        });
        return col;
    }
    if !cut {
        // A lidded mesh span is carved space: the terrain is not inside it.
        let inside = |col: &Column, top: f32| {
            col.spans().iter().any(|s| {
                s.ceiling
                    .is_some_and(|lid| s.floor.point[1] <= top + EPS_M && lid > top)
            })
        };
        let mut top = ground;
        if inside(&col, top) {
            if terrain.is_some_and(|t| t.shaft_at(x, z)) {
                // A skylight shaft: drawn ground that drops through the rock into the carved
                // space. There is no ground in this column: a body on the plain over the drawn
                // pit falls in. The span keeps its lid for the bodies inside it.
                return col;
            }
            // Any other baked patch that dips into one (a drawn funnel under the sim lid) is
            // dropped for the grid's own ground.
            if let Some(grid) = terrain.map(|t| t.grid_height_at_world(x, z))
                && grid > top
            {
                top = grid;
            }
        }
        // Ground still inside a carved span is lifted onto its lid (the roof of the carved space
        // is the ground over it). The height only rises, over at most the lids here.
        for _ in 0..MAX_CEILINGS {
            let mut lifted: Option<f32> = None;
            for span in col.spans() {
                if let Some(lid) = span.ceiling
                    && span.floor.point[1] <= top + EPS_M
                    && lid > top
                    && lifted.is_none_or(|l| lid > l)
                {
                    lifted = Some(lid);
                }
            }
            match lifted {
                Some(lid) => top = lid,
                None => break,
            }
        }
        col.insert(Span {
            floor: FloorSample {
                point: [x, top, z],
                normal: outdoor.normal,
                surface: FloorSurface::Heightfield,
            },
            ceiling: lid_over(top),
        });
    }
    col
}

/// The surface under a point: the floor of the span it is in.
pub fn surface_under(
    terrain: Option<&TerrainSampler>,
    mesh: Option<&TunnelCollisionMesh>,
    pos: [f32; 3],
) -> FloorSample {
    column(terrain, mesh, pos[0], pos[2]).span_at(pos[1]).floor
}

/// The ceiling over a point: the lid of the span it is in; `None` under the sky.
pub fn ceiling_over(
    terrain: Option<&TerrainSampler>,
    mesh: Option<&TunnelCollisionMesh>,
    pos: [f32; 3],
) -> Option<f32> {
    column(terrain, mesh, pos[0], pos[2]).span_at(pos[1]).ceiling
}

/// The floor triangles of `triangles` that carry bodies, in triangle order: every floor that is
/// not trim. A floor triangle is trim when it is a narrow strip (under [`TRIM_WIDTH_M`] across in
/// plan) and, at its centre and at three points inside it, another floor that is not trim lies
/// within [`TRIM_M`] of it, and at one of them the two are not the same height. Triangles are
/// judged in order and a trimmed one no longer counts for the ones after it, so two strips cannot
/// trim each other away.
pub(crate) fn support_floors(triangles: &[TunnelTriangle], index: &TriIndex) -> Vec<TunnelTriangle> {
    let mut trimmed = vec![false; triangles.len()];
    let all: Vec<u32> = (0..triangles.len() as u32).collect();
    for (i, tri) in triangles.iter().enumerate() {
        if tri.kind != TunnelFaceKind::Floor {
            continue;
        }
        if !narrow_in_plan(tri) {
            continue;
        }
        let cx = (tri.a[0] + tri.b[0] + tri.c[0]) / 3.0;
        let cz = (tri.a[2] + tri.b[2] + tri.c[2]) / 3.0;
        let inner = |v: [f32; 3]| [(v[0] * 3.0 + cx) * 0.25, (v[2] * 3.0 + cz) * 0.25];
        let probes = [[cx, cz], inner(tri.a), inner(tri.b), inner(tri.c)];
        let mut every_probe_near = true;
        let mut differs = false;
        for [x, z] in probes {
            let Some((own, _)) = plane_y_and_normal(tri, x, z) else {
                every_probe_near = false;
                break;
            };
            let ids: &[u32] = index.at(x, z).unwrap_or(&all[..]);
            let mut near = false;
            for j in ids {
                let j = *j as usize;
                let other = &triangles[j];
                if j == i || other.kind != TunnelFaceKind::Floor || trimmed[j] {
                    continue;
                }
                let Some((y, _)) = intersect_floor_xz(other, x, z) else {
                    continue;
                };
                let gap = (own - y).abs();
                if gap <= TRIM_M {
                    near = true;
                    if gap > EPS_M {
                        differs = true;
                    }
                }
            }
            if !near {
                every_probe_near = false;
                break;
            }
        }
        trimmed[i] = every_probe_near && differs;
    }
    triangles
        .iter()
        .zip(&trimmed)
        .filter(|(tri, trim)| tri.kind == TunnelFaceKind::Floor && !**trim)
        .map(|(tri, _)| *tri)
        .collect()
}

/// True when the triangle is a strip under [`TRIM_WIDTH_M`] across in plan: its height over its
/// longest side (twice its area over that side's length), compared squared so no root is taken.
fn narrow_in_plan(tri: &TunnelTriangle) -> bool {
    let (ax, az) = (tri.a[0], tri.a[2]);
    let (bx, bz) = (tri.b[0], tri.b[2]);
    let (cx, cz) = (tri.c[0], tri.c[2]);
    let doubled_area = ((bx - ax) * (cz - az) - (cx - ax) * (bz - az)).abs();
    let side2 = |px: f32, pz: f32, qx: f32, qz: f32| (qx - px) * (qx - px) + (qz - pz) * (qz - pz);
    let longest2 = det_max(
        det_max(side2(ax, az, bx, bz), side2(bx, bz, cx, cz)),
        side2(cx, cz, ax, az),
    );
    doubled_area * doubled_area < TRIM_WIDTH_M * TRIM_WIDTH_M * longest2
}

/// Put every support-floor vertex that is up to [`WELD_M`] proud of `terrain` onto it.
pub(crate) fn weld_to_terrain(support: &mut [TunnelTriangle], terrain: &TerrainSampler) {
    let weld = |v: &mut [f32; 3]| {
        let ground = terrain.height_at_world(v[0], v[2]);
        if v[1] > ground && v[1] - ground <= WELD_M {
            v[1] = ground;
        }
    };
    for tri in support {
        weld(&mut tri.a);
        weld(&mut tri.b);
        weld(&mut tri.c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::mesh::{synthetic_four_decks, synthetic_gallery};
    use crate::compat::sampler::{HEIGHT_METERS_PER_COUNT, HeightGrid};

    /// Flat ground at y = 0.
    fn flat() -> TerrainSampler {
        let raw = (8.0 / HEIGHT_METERS_PER_COUNT) as u16;
        TerrainSampler::new(HeightGrid::square(4, vec![raw; 16]).expect("grid"), 80.0)
    }

    fn quad(a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3], kind: TunnelFaceKind) -> [TunnelTriangle; 2] {
        let tri = |a, b, c| TunnelTriangle {
            a,
            b,
            c,
            kind,
            portal: false,
            pilot_only: false,
        };
        [tri(a, b, c), tri(a, c, d)]
    }

    /// A flat rectangle at height `y`.
    fn slab(x0: f32, x1: f32, z0: f32, z1: f32, y: f32, kind: TunnelFaceKind) -> [TunnelTriangle; 2] {
        quad([x0, y, z0], [x1, y, z0], [x1, y, z1], [x0, y, z1], kind)
    }

    #[test]
    fn no_mesh_is_the_heightfield_and_nothing_else() {
        let terrain = flat();
        let col = column(Some(&terrain), None, 3.0, -2.0);
        assert_eq!(col.spans().len(), 1);
        assert_eq!(col.spans()[0].floor.surface, FloorSurface::Heightfield);
        assert_eq!(
            col.spans()[0].floor.point[1].to_bits(),
            terrain.height_at_world(3.0, -2.0).to_bits()
        );
        assert_eq!(col.spans()[0].ceiling, None);
        for y in [-50.0, 0.0, 50.0] {
            assert_eq!(col.span_at(y), col.spans()[0]);
        }
    }

    #[test]
    fn a_buried_gallery_and_the_ground_over_it_are_two_spans() {
        let terrain = flat();
        let mesh = synthetic_gallery(10.0, 10.0, -12.0, -6.0, false);
        let col = column(Some(&terrain), Some(&mesh), 0.0, 0.0);
        let spans = col.spans();
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[0].floor.surface, FloorSurface::TunnelFloor);
        assert!(
            (spans[0].floor_y() + 12.0).abs() < 1e-4 && spans[0].ceiling == Some(-6.0),
            "{spans:?}"
        );
        assert_eq!(spans[1].floor.surface, FloorSurface::Heightfield);
        assert!(
            spans[1].floor_y().abs() < 1e-4 && spans[1].ceiling.is_none(),
            "{spans:?}"
        );
        // A body in the gallery stays in it at any height under the lid; one on the ground over it stays there.
        for y in [-12.0, -11.0, -8.0, -6.01] {
            assert_eq!(col.span_at(y), spans[0], "y {y}");
        }
        for y in [0.0, 0.5, 30.0] {
            assert_eq!(col.span_at(y), spans[1], "y {y}");
        }
        // In the rock between them the nearer face decides; under everything, the lowest floor.
        assert_eq!(col.span_at(-5.5), spans[0], "poked through the lid");
        assert_eq!(col.span_at(-0.4), spans[1], "sunk into the ground");
        assert_eq!(col.span_at(-40.0), spans[0]);
        assert_eq!(
            surface_under(Some(&terrain), Some(&mesh), [0.0, -9.0, 0.0]).point[1],
            spans[0].floor_y()
        );
        assert_eq!(
            ceiling_over(Some(&terrain), Some(&mesh), [0.0, -9.0, 0.0]),
            Some(-6.0)
        );
        assert_eq!(ceiling_over(Some(&terrain), Some(&mesh), [0.0, 1.0, 0.0]), None);
    }

    #[test]
    fn an_open_floor_under_the_terrain_cuts_it() {
        let terrain = flat();
        // A trench floor 6 m down with no lid: the terrain over it does not exist.
        let mesh = TunnelCollisionMesh::new(slab(-5.0, 5.0, -5.0, 5.0, -6.0, TunnelFaceKind::Floor).to_vec());
        let col = column(Some(&terrain), Some(&mesh), 0.0, 0.0);
        assert_eq!(col.spans().len(), 1, "{:?}", col.spans());
        assert_eq!(col.spans()[0].floor.surface, FloorSurface::TunnelFloor);
        // A body over the trench at ground height is over the trench floor: it falls in.
        assert!((col.span_at(0.9).floor_y() + 6.0).abs() < 1e-4);
        // Beside the trench the terrain stands.
        let beside = column(Some(&terrain), Some(&mesh), 8.0, 0.0);
        assert_eq!(beside.span_at(0.9).floor.surface, FloorSurface::Heightfield);
    }

    #[test]
    fn a_deck_over_the_terrain_leaves_the_ground_under_it() {
        let terrain = flat();
        // A bridge deck 5 m up: two levels, picked by where the body is.
        let mesh = TunnelCollisionMesh::new(slab(-5.0, 5.0, -5.0, 5.0, 5.0, TunnelFaceKind::Floor).to_vec());
        let col = column(Some(&terrain), Some(&mesh), 0.0, 0.0);
        assert_eq!(col.spans().len(), 2, "{:?}", col.spans());
        assert_eq!(col.span_at(0.9).floor.surface, FloorSurface::Heightfield);
        assert_eq!(col.span_at(5.9).floor.surface, FloorSurface::TunnelFloor);
        // A body climbing a ramp reads the deck it is on, not the level under it, as long as
        // the deck has not risen over its waist.
        assert_eq!(
            col.span_at(5.0 - 0.3 + BODY_REACH_M).floor.surface,
            FloorSurface::TunnelFloor
        );
    }

    #[test]
    fn terrain_inside_a_carved_span_is_lifted_onto_its_lid() {
        let terrain = flat();
        // A lidded space through the ground line: floor 4 m down, lid 3 m up.
        let mesh = synthetic_gallery(6.0, 6.0, -4.0, 3.0, false);
        let col = column(Some(&terrain), Some(&mesh), 0.0, 0.0);
        let spans = col.spans();
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!((spans[0].floor_y() + 4.0).abs() < 1e-4 && spans[0].ceiling == Some(3.0));
        assert_eq!(spans[1].floor.surface, FloorSurface::Heightfield);
        assert!(
            (spans[1].floor_y() - 3.0).abs() < 1e-4 && spans[1].ceiling.is_none(),
            "{spans:?}"
        );
        // Inside, the drawn ground line is not a floor.
        assert_eq!(col.span_at(0.5), spans[0]);
        assert_eq!(col.span_at(2.0), spans[0]);
    }

    #[test]
    fn stacked_decks_are_one_span_each() {
        let mesh = synthetic_four_decks();
        let col = column(None, Some(&mesh), 0.0, 0.0);
        let floors: Vec<f32> = col.spans().iter().map(|s| s.floor_y()).collect();
        assert_eq!(col.spans().len(), 5, "four decks and the roof: {floors:?}");
        for (i, deck) in [-12.0f32, -8.0, -4.0, 0.0].iter().enumerate() {
            let span = col.spans()[i];
            assert!((span.floor_y() - deck).abs() < 1e-4, "{floors:?}");
            assert!(
                span.ceiling.is_some_and(|c| (c - (deck + 3.5)).abs() < 1e-4),
                "{span:?}"
            );
            for dy in [0.0, BODY_REACH_M, 3.4] {
                assert_eq!(col.span_at(deck + dy), span, "deck {deck} dy {dy}");
            }
        }
        assert_eq!(col.spans()[4].floor.surface, FloorSurface::Heightfield);
        assert!(
            (col.spans()[4].floor_y() - 3.5).abs() < 1e-4,
            "the roof: {floors:?}"
        );
        assert_eq!(col.highest_mesh_span(2.5).map(|s| s.floor_y()), Some(0.0));
    }

    #[test]
    fn a_sill_slab_on_a_floor_is_trim_and_coincident_floors_are_not() {
        let mut tris = slab(-10.0, 10.0, -10.0, 10.0, -6.0, TunnelFaceKind::Floor).to_vec();
        // A 0.3 m threshold slab lying on the floor.
        tris.extend(slab(-10.0, 10.0, 0.0, 1.0, -5.7, TunnelFaceKind::Floor));
        // A second passage sharing the same floor height.
        tris.extend(slab(-10.0, 0.0, -10.0, 0.0, -6.0, TunnelFaceKind::Floor));
        // A real level 3 m over the floor.
        tris.extend(slab(-10.0, 10.0, -8.0, -4.0, -3.0, TunnelFaceKind::Floor));
        // Another passage's floor crossing 0.2 m over this one: wide, so a floor, not trim.
        tris.extend(slab(2.0, 9.0, 3.0, 9.0, -5.8, TunnelFaceKind::Floor));
        let mesh = TunnelCollisionMesh::new(tris);
        assert_eq!(
            mesh.support_floors().len(),
            8,
            "only the sill's two triangles are trim"
        );
        assert!((surface_under(None, Some(&mesh), [5.0, -5.0, 6.0]).point[1] + 5.8).abs() < 1e-4);
        let over_sill = column(None, Some(&mesh), 3.0, 0.5);
        assert!(
            over_sill.spans().iter().all(|s| (s.floor_y() + 5.7).abs() > 0.01),
            "{:?}",
            over_sill.spans()
        );
        // Walking over the sill the surface does not move.
        let mut worst = 0.0f32;
        let mut last = -6.0f32;
        for k in 0..=40 {
            let z = -1.0 + k as f32 * 0.075;
            let y = surface_under(None, Some(&mesh), [3.0, last + BODY_REACH_M, z]).point[1];
            worst = det_max(worst, (y - last).abs());
            last = y;
        }
        assert!(worst < 1e-4, "a {worst} m step at the sill");
        // The level 3 m up is a level.
        assert!((surface_under(None, Some(&mesh), [3.0, -2.0, -6.0]).point[1] + 3.0).abs() < 1e-4);
    }

    #[test]
    fn a_proud_lip_is_welded_onto_the_ground() {
        let terrain = flat();
        // A trench (floor 6 m down) with a lip 0.10 m proud of the ground on one side.
        let mut tris = slab(-5.0, 5.0, -5.0, 5.0, -6.0, TunnelFaceKind::Floor).to_vec();
        tris.extend(slab(5.0, 8.0, -5.0, 5.0, 0.10, TunnelFaceKind::Floor));
        let raw = TunnelCollisionMesh::new(tris);
        let welded = raw.clone().welded(Some(&terrain));
        assert_eq!(raw, welded, "welding does not change the drawn triangles");
        // From the plain across the lip: unwelded steps 0.10 m, welded does not step.
        let walk = |mesh: &TunnelCollisionMesh| -> f32 {
            let mut worst = 0.0f32;
            let mut last = 0.0f32;
            for k in 0..=60 {
                let x = 9.5 - k as f32 * 0.07;
                let y = surface_under(Some(&terrain), Some(mesh), [x, last + BODY_REACH_M, 0.0]).point[1];
                worst = det_max(worst, (y - last).abs());
                last = y;
            }
            worst
        };
        assert!((walk(&raw) - 0.10).abs() < 1e-3, "unwelded: {}", walk(&raw));
        assert!(walk(&welded) < 1e-3, "welded: {}", walk(&welded));
    }

    #[test]
    fn two_builds_of_one_map_answer_bit_for_bit_the_same() {
        let terrain = flat();
        let sweep = || {
            let mesh = synthetic_four_decks().welded(Some(&terrain));
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for k in 0..2000 {
                let x = -5.0 + (k % 53) as f32 * 0.251;
                let z = -5.0 + (k % 41) as f32 * 0.243;
                let y = -13.0 + (k % 97) as f32 * 0.19;
                let span = column(Some(&terrain), Some(&mesh), x, z).span_at(y);
                for v in [span.floor.point[1], span.ceiling.unwrap_or(f32::MAX)] {
                    hash ^= u64::from(v.to_bits());
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            hash
        };
        assert_eq!(sweep(), sweep());
    }
}
