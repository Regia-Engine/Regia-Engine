// SPDX-License-Identifier: MPL-2.0
//! Cells and portals the mesh-layer cook writes as `<map>.viscells.ron` (spec 121 § 5.6, spec 25
//! § 7.7, ADR-8371, plan row W-21).
//!
//! One cell per cooked section, plus cell 0 for the outdoors. A mouth ring and each junction or
//! section cut becomes one or more convex portal quads in VisCell's version-1 file (no new fields).
//! The cook refuses a section whose wall has a boundary gap of 0.5 m or more (`viscell_leak`).
//!
//! The arithmetic is `f64` and cook-only. The file's coordinates are metres (`f32`), the same
//! numbers the VisCell loader checks. No hash map: portal order follows the cooked portals, and
//! the leak clustering walks a `BTreeMap`.

use regia_detsim::float::{det_clamp_f64, det_max_f64, det_min_f64};
use std::collections::{BTreeMap, BTreeSet};

use crate::cooked::{CookedZoneShape, TriKind, WorldGeomCooked};

/// A wall gap at least this long is a leak, millimetres (the acceptance gap is 0.5 m).
pub const WALL_GAP_MM: i64 = 500;
/// Both ends of a boundary edge this close to a portal polygon are the portal's ring, not a hole.
const PORTAL_SEAL_MM: f64 = 20.0;
/// A boundary edge this close to another section's triangle is a joint (a lid hole around a ramp),
/// not a hole in an isolated wall.
const OTHER_SECTION_MM: f64 = 500.0;
/// A boundary edge whose midpoint lies this close to another edge of the same section is a
/// T-junction or a weld sliver, not a hole. The weld step may part a floor edge from the wall
/// foot by up to 0.15 m. A 0.5 m hole's own edges stay further apart than this.
const ON_EDGE_MM: f64 = 180.0;
/// More cells than VisCell version 1 accepts, outdoor cell included.
const MAX_CELLS: usize = 4096;
/// More portal quads than VisCell version 1 accepts.
const MAX_PORTALS: usize = 16384;

/// Why the bake cannot emit `<map>.viscells.ron`. [`crate::cook::CookError`] carries the agent code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisCellsFault {
    /// A wall gap of at least [`WALL_GAP_MM`], or two sections still touch with the portals closed.
    Leak {
        /// `viscell_leak: ...`, the section name included.
        detail: String,
    },
    /// A cooked portal did not become convex quads the loader would accept.
    BadPortal {
        /// The section the portal bounds.
        section: String,
        /// The cooked portal id.
        portal: u16,
    },
    /// A section has no zone, or the zone's half extents are not above 0.
    BadBox {
        /// The section source (`<layer>/<passage>`).
        section: String,
    },
    /// More cells or portal quads than the version-1 file allows.
    TooMany {
        /// `cells` or `portals`.
        what: &'static str,
    },
}

/// One triangle of a section shell the leak check reads (world millimetres).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GapTri {
    /// The three vertices, world millimetres.
    pub v: [[i32; 3]; 3],
    /// Floor, roof, wall or blocker. Blockers are ignored.
    pub kind: TriKind,
}

/// One section's shell, the input of [`wall_gap_of`] (what the cook runs on every section).
#[derive(Clone, Debug, PartialEq)]
pub struct GapSection {
    /// `<layer>/<passage>`, the name `viscell_leak` reports.
    pub source: String,
    /// The section's triangles.
    pub triangles: Vec<GapTri>,
    /// Portal polygons on its boundary, world millimetres (mouth rings, cuts, sky outlines).
    pub portals_mm: Vec<Vec<[i32; 3]>>,
    /// The section's zone. Edges outside it are lips and headers, not interior holes.
    pub zone: Option<CookedZoneShape>,
    /// Triangles of other sections, world millimetres (blockers left out).
    pub others: Vec<[[i32; 3]; 3]>,
}

/// A wall gap [`wall_gap_of`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WallGap {
    /// The section source.
    pub source: String,
    /// The gap's longest span, millimetres.
    pub span_mm: i32,
    /// The `viscell_leak` detail (the section name and the span).
    pub detail: String,
}

/// One cell in the emitted file.
#[derive(Clone, Debug, PartialEq)]
pub struct VisCellRow {
    /// Cell id. 0 is outdoors. An indoor cell is `section.id + 1`.
    pub id: u16,
    /// The section source, or `outdoor`.
    pub name: String,
    /// Set on cell 0 only.
    pub outdoor: bool,
    /// Box centre, metres. Zero for the outdoor cell.
    pub center_m: [f32; 3],
    /// Box half extents, metres. Zero for the outdoor cell.
    pub half_extents_m: [f32; 3],
    /// Yaw about +Y, radians. Zero for the outdoor cell.
    pub rotation_radians: f32,
}

/// One portal quad in the emitted file.
#[derive(Clone, Debug, PartialEq)]
pub struct VisPortalRow {
    /// One side's cell id.
    pub a: u16,
    /// The other side's cell id.
    pub b: u16,
    /// The cooked portal this quad was cut from.
    pub cooked: u16,
    /// The quad, world metres, in order around the opening.
    pub quad_m: [[f32; 3]; 4],
}

/// The `<map>.viscells.ron` text and the rows the cook report lists.
#[derive(Clone, Debug, PartialEq)]
pub struct VisCellsEmit {
    /// Pretty RON, version 1, LF line endings, one trailing newline.
    pub text: String,
    /// Cell 0, then one cell per section in id order.
    pub cells: Vec<VisCellRow>,
    /// Portal quads in cooked-portal order.
    pub portals: Vec<VisPortalRow>,
    /// Shared edges between two sections. Above 0, the sealed flood had something to separate.
    pub sealed_crossings: usize,
}

/// Emits `<map>.viscells.ron` from a cooked canonical section.
///
/// # Errors
///
/// [`VisCellsFault`]: a wall gap, a portal that is not a convex quad, a section box with a
/// non-positive half extent, or more cells or quads than version 1 allows. A flood that still
/// reaches another section with the quads closed is a leak.
pub fn emit(canonical: &WorldGeomCooked) -> Result<VisCellsEmit, VisCellsFault> {
    if canonical.sections.len() + 1 > MAX_CELLS {
        return Err(VisCellsFault::TooMany { what: "cells" });
    }
    let mut cells = Vec::with_capacity(canonical.sections.len() + 1);
    cells.push(VisCellRow {
        id: 0,
        name: "outdoor".to_string(),
        outdoor: true,
        center_m: [0.0; 3],
        half_extents_m: [0.0; 3],
        rotation_radians: 0.0,
    });
    for section in &canonical.sections {
        let id = section
            .id
            .checked_add(1)
            .ok_or(VisCellsFault::TooMany { what: "cells" })?;
        let zone = canonical.zones.iter().find(|z| z.section == section.id);
        let Some((center, half, yaw)) = zone.and_then(|z| box_of(&z.shape)) else {
            return Err(VisCellsFault::BadBox {
                section: section.source.clone(),
            });
        };
        cells.push(VisCellRow {
            id,
            name: section.source.clone(),
            outdoor: false,
            center_m: center,
            half_extents_m: half,
            rotation_radians: yaw,
        });
    }
    let mut portals = Vec::new();
    for portal in &canonical.portals {
        let a = cell_of(portal.sides[0])?;
        let b = cell_of(portal.sides[1])?;
        if a == b {
            return Err(VisCellsFault::BadPortal {
                section: section_name(canonical, portal.sides),
                portal: portal.id,
            });
        }
        let quads = match strip_quads(&portal.polygon_mm) {
            Some(quads) => quads,
            None => {
                return Err(VisCellsFault::BadPortal {
                    section: section_name(canonical, portal.sides),
                    portal: portal.id,
                });
            }
        };
        for quad in quads {
            let projected = project_quad(quad);
            if !convex_quad(projected) {
                return Err(VisCellsFault::BadPortal {
                    section: section_name(canonical, portal.sides),
                    portal: portal.id,
                });
            }
            let quad_m = projected.map(mm_to_m);
            if quad_m.iter().any(|p| p.iter().any(|c| !c.is_finite())) {
                return Err(VisCellsFault::BadPortal {
                    section: section_name(canonical, portal.sides),
                    portal: portal.id,
                });
            }
            portals.push(VisPortalRow {
                a,
                b,
                cooked: portal.id,
                quad_m,
            });
        }
    }
    if portals.len() > MAX_PORTALS {
        return Err(VisCellsFault::TooMany { what: "portals" });
    }
    if let Some(gap) = first_wall_gap(canonical) {
        return Err(VisCellsFault::Leak { detail: gap.detail });
    }
    let seal: Vec<(u16, u16, [[f32; 3]; 4])> = portals.iter().map(|p| (p.a, p.b, p.quad_m)).collect();
    let sealed_crossings = sealed_flood(canonical, &seal)?;
    let text = write_ron(&cells, &portals);
    Ok(VisCellsEmit {
        text,
        cells,
        portals,
        sealed_crossings,
    })
}

/// The first section wall gap in id order, if any cooked section has one.
#[must_use]
pub fn first_wall_gap(canonical: &WorldGeomCooked) -> Option<WallGap> {
    let world = world_triangles(canonical);
    for section in &canonical.sections {
        let triangles = world
            .iter()
            .filter(|t| t.section == Some(section.id))
            .map(|t| GapTri { v: t.v, kind: t.kind })
            .collect();
        let others = world
            .iter()
            .filter(|t| t.section != Some(section.id) && t.kind != TriKind::Blocker)
            .map(|t| t.v)
            .collect();
        let portals_mm = canonical
            .portals
            .iter()
            .filter(|p| p.sides.contains(&Some(section.id)))
            .map(|p| p.polygon_mm.clone())
            .collect();
        let zone = canonical
            .zones
            .iter()
            .find(|z| z.section == section.id)
            .map(|z| z.shape);
        let gap = wall_gap_of(&GapSection {
            source: section.source.clone(),
            triangles,
            portals_mm,
            zone,
            others,
        });
        if gap.is_some() {
            return gap;
        }
    }
    None
}

/// A 0.5 m wall gap in `section`, or `None` when the shell is closed.
///
/// Boundary edges of non-blocker triangles are the candidates. An edge is not a hole when it is
/// not a wall, when both ends sit on a portal polygon, when its midpoint is outside the section
/// zone (a lip or a header), when it lies on another edge of the same section (a T-junction), or
/// when it sits against another section's triangle (a lid opening around a ramp). What remains is
/// clustered by shared vertices. A closed loop spanning [`WALL_GAP_MM`] or more is a leak. An open
/// rim (a header top, the end of a wall) is not.
#[must_use]
pub fn wall_gap_of(section: &GapSection) -> Option<WallGap> {
    let mut uses: BTreeMap<([i32; 3], [i32; 3]), (u32, bool)> = BTreeMap::new();
    let mut segments: Vec<([i32; 3], [i32; 3])> = Vec::new();
    for tri in &section.triangles {
        if tri.kind == TriKind::Blocker {
            continue;
        }
        let [a, b, c] = tri.v;
        let wall = tri.kind == TriKind::Wall;
        for (p, q) in [(a, b), (b, c), (c, a)] {
            let key = ord_edge(p, q);
            let entry = uses.entry(key).or_insert((0, false));
            entry.0 += 1;
            entry.1 |= wall;
            segments.push(key);
        }
    }
    let mut portal_quads = Vec::new();
    for poly in &section.portals_mm {
        if let Some(quads) = strip_quads(poly) {
            portal_quads.extend(quads);
        }
    }
    let mut kept: Vec<([i32; 3], [i32; 3])> = Vec::new();
    for (key, (count, wall)) in &uses {
        if *count != 1 || !wall {
            continue;
        }
        let (a, b) = *key;
        let (pa, pb) = (v3(a), v3(b));
        if portal_quads.iter().any(|q| on_portal(pa, q) && on_portal(pb, q)) {
            continue;
        }
        let mid = midpoint(a, b);
        if section.zone.as_ref().is_some_and(|z| !zone_contains(z, mid)) {
            continue;
        }
        if segments
            .iter()
            .any(|&(c, d)| !same_edge(a, b, c, d) && seg_dist2(mid, v3(c), v3(d)) <= ON_EDGE_MM * ON_EDGE_MM)
        {
            continue;
        }
        if section
            .others
            .iter()
            .any(|t| dist2_point_tri(mid, *t) <= OTHER_SECTION_MM * OTHER_SECTION_MM)
        {
            continue;
        }
        kept.push((a, b));
    }
    let (span_sq, sample) = leaking_component(&kept)?;
    let span_mm = i32::try_from(isqrt(span_sq as u64)).unwrap_or(i32::MAX);
    let detail = format!(
        "section `{}` has a 0.5 m wall gap (spanning {span_mm} mm, near {sample:?} mm)",
        section.source
    );
    Some(WallGap {
        source: section.source.clone(),
        span_mm,
        detail,
    })
}

/// With every interior portal quad closed, a flood across shared triangle edges reaches no other
/// section. Returns how many edges two sections share (0 when the mesh has no such edge).
///
/// # Errors
///
/// [`VisCellsFault::Leak`] naming the section that reached another.
pub fn sealed_flood(
    canonical: &WorldGeomCooked,
    portals: &[(u16, u16, [[f32; 3]; 4])],
) -> Result<usize, VisCellsFault> {
    let tris = world_triangles(canonical);
    let seals: Vec<Seal> = portals
        .iter()
        .filter(|p| p.0 != 0 && p.1 != 0)
        .filter_map(|p| Seal::from_quad(p.2))
        .collect();
    let key = |a: [i32; 3], b: [i32; 3]| ord_edge(a, b);
    let mut edges: BTreeMap<([i32; 3], [i32; 3]), Vec<usize>> = BTreeMap::new();
    for (i, t) in tris.iter().enumerate() {
        let [a, b, c] = t.v;
        for (p, q) in [(a, b), (b, c), (c, a)] {
            edges.entry(key(p, q)).or_default().push(i);
        }
    }
    let mut crossings = 0usize;
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); tris.len()];
    for ((p, q), users) in &edges {
        let closed = seals.iter().any(|s| s.covers(*p) && s.covers(*q));
        for &a in users {
            for &b in users {
                if a == b {
                    continue;
                }
                if tris[a].section != tris[b].section {
                    crossings += 1;
                    if closed {
                        continue;
                    }
                }
                adj[a].push(b);
            }
        }
    }
    let mut seen = vec![false; tris.len()];
    for start in 0..tris.len() {
        if seen[start] {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let mut ids: BTreeSet<u16> = BTreeSet::new();
        while let Some(t) = stack.pop() {
            if let Some(id) = tris[t].section {
                ids.insert(id);
            }
            for &n in &adj[t] {
                if !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        if ids.len() > 1 {
            let mut it = ids.iter();
            let from = it.next().copied().unwrap_or(0);
            let into = it.next().copied().unwrap_or(from);
            return Err(VisCellsFault::Leak {
                detail: format!(
                    "section `{}` reaches `{}` with every portal closed",
                    name_of(canonical, from),
                    name_of(canonical, into)
                ),
            });
        }
    }
    Ok(crossings / 2)
}

fn cell_of(side: Option<u16>) -> Result<u16, VisCellsFault> {
    match side {
        None => Ok(0),
        Some(id) => id.checked_add(1).ok_or(VisCellsFault::TooMany { what: "cells" }),
    }
}

fn section_name(canonical: &WorldGeomCooked, sides: [Option<u16>; 2]) -> String {
    for side in sides.into_iter().flatten() {
        if let Some(name) = canonical
            .sections
            .iter()
            .find(|s| s.id == side)
            .map(|s| s.source.clone())
        {
            return name;
        }
    }
    "unknown".to_string()
}

fn name_of(canonical: &WorldGeomCooked, id: u16) -> String {
    canonical
        .sections
        .iter()
        .find(|s| s.id == id)
        .map_or_else(|| format!("section {id}"), |s| s.source.clone())
}

/// An oriented box around the zone, metres. `None` when a half extent is not above 0.
fn box_of(shape: &CookedZoneShape) -> Option<([f32; 3], [f32; 3], f32)> {
    match *shape {
        CookedZoneShape::OrientedBox {
            center_mm,
            half_mm,
            yaw,
        } => {
            if half_mm.iter().any(|h| *h <= 0) {
                return None;
            }
            let yaw = if yaw == 0.0 { 0.0 } else { yaw };
            let center = center_mm.map(mm_to_m_i);
            let half = half_mm.map(mm_to_m_i);
            let ok = yaw.is_finite()
                && center.iter().all(|c| c.is_finite())
                && half.iter().all(|h| h.is_finite() && *h > 0.0);
            ok.then_some((center, half, yaw))
        }
        CookedZoneShape::Capsule {
            a_mm,
            b_mm,
            radius_mm,
        } => {
            let r = i64::from(radius_mm);
            if r <= 0 {
                return None;
            }
            let mut center = [0.0f32; 3];
            let mut half = [0.0f32; 3];
            for i in 0..3 {
                let a = i64::from(a_mm[i]);
                let b = i64::from(b_mm[i]);
                let mid = a + (b - a) / 2;
                let span = (a - b).unsigned_abs();
                let h = (span / 2) as i64 + r;
                let (Ok(mid), Ok(h)) = (i32::try_from(mid), i32::try_from(h)) else {
                    return None;
                };
                if h <= 0 {
                    return None;
                }
                center[i] = mm_to_m_i(mid);
                half[i] = mm_to_m_i(h);
            }
            let ok = center.iter().all(|c| c.is_finite()) && half.iter().all(|h| h.is_finite() && *h > 0.0);
            ok.then_some((center, half, 0.0))
        }
    }
}

fn mm_to_m_i(mm: i32) -> f32 {
    let v = f64::from(mm) / 1000.0;
    let v = v as f32;
    if v == 0.0 { 0.0 } else { v }
}

fn mm_to_m(p: [f64; 3]) -> [f32; 3] {
    p.map(|c| {
        let v = (c / 1000.0) as f32;
        if v == 0.0 { 0.0 } else { v }
    })
}

/// Even-length rings become strip quads. A 4-point cross-section is the one quad, in order.
/// A sky outline (left samples forward, then right samples reversed) becomes one quad per step.
fn strip_quads(poly: &[[i32; 3]]) -> Option<Vec<[[f64; 3]; 4]>> {
    if poly.len() < 4 || !poly.len().is_multiple_of(2) {
        return None;
    }
    let n = poly.len() / 2;
    let mut out = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let quad = [
            v3(poly[i]),
            v3(poly[i + 1]),
            v3(poly[2 * n - 2 - i]),
            v3(poly[2 * n - 1 - i]),
        ];
        if quad_span_sq(quad) <= 1.0 {
            continue;
        }
        out.push(quad);
    }
    if out.is_empty() { None } else { Some(out) }
}

fn quad_span_sq(q: [[f64; 3]; 4]) -> f64 {
    let mut best: f64 = 0.0;
    for i in 0..4 {
        for j in (i + 1)..4 {
            best = det_max_f64(best, dist2(q[i], q[j]));
        }
    }
    best
}

fn project_quad(q: [[f64; 3]; 4]) -> [[f64; 3]; 4] {
    let n = cross(sub(q[1], q[0]), sub(q[2], q[0]));
    let n2 = dot(n, n);
    if n2 <= 1.0e-18 {
        return q;
    }
    let mut out = q;
    for p in &mut out {
        let t = dot(sub(*p, q[0]), n) / n2;
        *p = sub(*p, scale(n, t));
    }
    out
}

fn convex_quad(q: [[f64; 3]; 4]) -> bool {
    let n = newell(q);
    let n2 = dot(n, n);
    if n2 <= 0.0 {
        return false;
    }
    let diag2 = det_max_f64(dist2(q[0], q[2]), dist2(q[1], q[3]));
    if diag2 <= 0.0 || n2 <= 1.0e-12 * diag2 * diag2 {
        return false;
    }
    for i in 0..4 {
        let prev = q[(i + 3) % 4];
        let here = q[i];
        let next = q[(i + 1) % 4];
        let turn = dot(cross(sub(here, prev), sub(next, here)), n);
        if turn <= 0.0 || turn * turn <= 1.0e-12 * diag2 * diag2 * n2 {
            return false;
        }
    }
    true
}

fn on_portal(p: [f64; 3], quad: &[[f64; 3]; 4]) -> bool {
    dist2_point_quad(p, *quad) <= PORTAL_SEAL_MM * PORTAL_SEAL_MM
}

fn zone_contains(zone: &CookedZoneShape, p: [f64; 3]) -> bool {
    match *zone {
        CookedZoneShape::OrientedBox {
            center_mm,
            half_mm,
            yaw,
        } => {
            let yaw = if yaw == 0.0 { 0.0 } else { yaw };
            let neg = -yaw;
            let s = f64::from(regia_detsim::math::sin(neg));
            let c = f64::from(regia_detsim::math::cos(neg));
            let d = sub(p, v3(center_mm));
            let lx = c * d[0] + s * d[2];
            let lz = -s * d[0] + c * d[2];
            lx.abs() <= f64::from(half_mm[0])
                && d[1].abs() <= f64::from(half_mm[1])
                && lz.abs() <= f64::from(half_mm[2])
        }
        CookedZoneShape::Capsule {
            a_mm,
            b_mm,
            radius_mm,
        } => {
            let r = f64::from(radius_mm);
            r > 0.0 && seg_dist2(p, v3(a_mm), v3(b_mm)) <= r * r
        }
    }
}

fn leaking_component(edges: &[([i32; 3], [i32; 3])]) -> Option<(i64, [i32; 3])> {
    if edges.is_empty() {
        return None;
    }
    let mut adj: BTreeMap<[i32; 3], Vec<[i32; 3]>> = BTreeMap::new();
    for &(a, b) in edges {
        adj.entry(a).or_default().push(b);
        adj.entry(b).or_default().push(a);
    }
    let starts: Vec<[i32; 3]> = adj.keys().copied().collect();
    let mut seen: BTreeSet<[i32; 3]> = BTreeSet::new();
    let limit = WALL_GAP_MM * WALL_GAP_MM;
    for start in starts {
        if !seen.insert(start) {
            continue;
        }
        let mut stack = vec![start];
        let mut comp = vec![start];
        while let Some(v) = stack.pop() {
            if let Some(next) = adj.get(&v) {
                for &n in next {
                    if seen.insert(n) {
                        stack.push(n);
                        comp.push(n);
                    }
                }
            }
        }
        // An open rim has an end. A hole in a wall is a loop.
        let closed = comp.len() >= 3 && comp.iter().all(|v| adj.get(v).is_some_and(|n| n.len() == 2));
        if !closed {
            continue;
        }
        let mut worst = 0i64;
        for (i, a) in comp.iter().enumerate() {
            for b in comp.iter().skip(i + 1) {
                worst = worst.max(dist2_i(*a, *b));
            }
        }
        if worst >= limit || comp.len() > 400 {
            return Some((worst.max(limit), start));
        }
    }
    None
}

struct WorldTri {
    v: [[i32; 3]; 3],
    kind: TriKind,
    section: Option<u16>,
}

fn world_triangles(canonical: &WorldGeomCooked) -> Vec<WorldTri> {
    let mut all = Vec::new();
    for tile in &canonical.tiles {
        for t in &tile.triangles {
            let v =
                t.v.map(|p| [p[0] + tile.origin_mm[0], p[1], p[2] + tile.origin_mm[1]]);
            all.push(WorldTri {
                v,
                kind: t.kind,
                section: t.section,
            });
        }
    }
    all
}

struct Seal {
    origin: [f64; 3],
    normal: [f64; 3],
    n2: f64,
    lo_x: f64,
    hi_x: f64,
    lo_z: f64,
    hi_z: f64,
}

impl Seal {
    fn from_quad(quad: [[f32; 3]; 4]) -> Option<Self> {
        let q = quad.map(|p| p.map(f64::from));
        let normal = cross(sub(q[1], q[0]), sub(q[2], q[0]));
        let n2 = dot(normal, normal);
        if n2 <= 1.0e-18 {
            return None;
        }
        let mut lo_x = f64::MAX;
        let mut hi_x = f64::MIN;
        let mut lo_z = f64::MAX;
        let mut hi_z = f64::MIN;
        for p in &q {
            lo_x = det_min_f64(lo_x, p[0]);
            hi_x = det_max_f64(hi_x, p[0]);
            lo_z = det_min_f64(lo_z, p[2]);
            hi_z = det_max_f64(hi_z, p[2]);
        }
        Some(Self {
            origin: q[0],
            normal,
            n2,
            lo_x,
            hi_x,
            lo_z,
            hi_z,
        })
    }

    /// On the quad's plane within 0.2 m and inside its XZ box padded 4 m (the open-cut lips).
    fn covers(&self, p_mm: [i32; 3]) -> bool {
        let p = v3(p_mm).map(|c| c / 1000.0);
        let d = dot(sub(p, self.origin), self.normal);
        d * d <= 0.04 * self.n2
            && p[0] >= self.lo_x - 4.0
            && p[0] <= self.hi_x + 4.0
            && p[2] >= self.lo_z - 4.0
            && p[2] <= self.hi_z + 4.0
    }
}

fn write_ron(cells: &[VisCellRow], portals: &[VisPortalRow]) -> String {
    let mut out = String::from("(\n    version: 1,\n    cells: [\n");
    for cell in cells {
        out.push_str("        (\n            id: ");
        out.push_str(&cell.id.to_string());
        out.push_str(",\n            name: ");
        push_ron_str(&mut out, &cell.name);
        out.push_str(",\n            outdoor: ");
        out.push_str(if cell.outdoor { "true" } else { "false" });
        out.push_str(",\n            bounds: [");
        if !cell.outdoor {
            out.push_str("\n                (\n                    center: ");
            push_f32_3(&mut out, cell.center_m);
            out.push_str(",\n                    half_extents: ");
            push_f32_3(&mut out, cell.half_extents_m);
            out.push_str(",\n                    rotation_radians: ");
            push_f32(&mut out, cell.rotation_radians);
            out.push_str(",\n                ),\n            ");
        }
        out.push_str("],\n        ),\n");
    }
    out.push_str("    ],\n    portals: [\n");
    for portal in portals {
        out.push_str("        (\n            a: ");
        out.push_str(&portal.a.to_string());
        out.push_str(",\n            b: ");
        out.push_str(&portal.b.to_string());
        out.push_str(",\n            quad: [\n");
        for p in &portal.quad_m {
            out.push_str("                ");
            push_f32_3(&mut out, *p);
            out.push_str(",\n");
        }
        out.push_str("            ],\n        ),\n");
    }
    out.push_str("    ],\n)\n");
    out
}

fn push_ron_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_f32_3(out: &mut String, p: [f32; 3]) {
    out.push('(');
    push_f32(out, p[0]);
    out.push_str(", ");
    push_f32(out, p[1]);
    out.push_str(", ");
    push_f32(out, p[2]);
    out.push(')');
}

fn push_f32(out: &mut String, v: f32) {
    let v = if v == 0.0 { 0.0 } else { v };
    out.push_str(&format!("{v:?}"));
}

fn v3(p: [i32; 3]) -> [f64; 3] {
    p.map(f64::from)
}

fn midpoint(a: [i32; 3], b: [i32; 3]) -> [f64; 3] {
    [
        (f64::from(a[0]) + f64::from(b[0])) * 0.5,
        (f64::from(a[1]) + f64::from(b[1])) * 0.5,
        (f64::from(a[2]) + f64::from(b[2])) * 0.5,
    ]
}

fn ord_edge(a: [i32; 3], b: [i32; 3]) -> ([i32; 3], [i32; 3]) {
    if a <= b { (a, b) } else { (b, a) }
}

fn same_edge(a: [i32; 3], b: [i32; 3], c: [i32; 3], d: [i32; 3]) -> bool {
    (a == c && b == d) || (a == d && b == c)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn scale(a: [f64; 3], t: f64) -> [f64; 3] {
    [a[0] * t, a[1] * t, a[2] * t]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d = sub(a, b);
    dot(d, d)
}

fn dist2_i(a: [i32; 3], b: [i32; 3]) -> i64 {
    let d = |i: usize| i64::from(a[i]) - i64::from(b[i]);
    let (x, y, z) = (d(0), d(1), d(2));
    x * x + y * y + z * z
}

fn newell(q: [[f64; 3]; 4]) -> [f64; 3] {
    let mut n = [0.0; 3];
    for i in 0..4 {
        let (u, v) = (q[i], q[(i + 1) % 4]);
        n[0] += (u[1] - v[1]) * (u[2] + v[2]);
        n[1] += (u[2] - v[2]) * (u[0] + v[0]);
        n[2] += (u[0] - v[0]) * (u[1] + v[1]);
    }
    n
}

fn seg_dist2(q: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let ab = sub(b, a);
    let l2 = dot(ab, ab);
    let t = if l2 > 0.0 {
        det_clamp_f64(dot(sub(q, a), ab) / l2, 0.0, 1.0)
    } else {
        0.0
    };
    dist2(q, [a[0] + ab[0] * t, a[1] + ab[1] * t, a[2] + ab[2] * t])
}

/// Point to a quad: the plane distance when the projection falls inside, otherwise the nearest edge.
fn dist2_point_quad(p: [f64; 3], q: [[f64; 3]; 4]) -> f64 {
    let n = newell(q);
    let n2 = dot(n, n);
    let edge = (0..4)
        .map(|i| seg_dist2(p, q[i], q[(i + 1) % 4]))
        .fold(f64::MAX, det_min_f64);
    if n2 <= 1.0e-18 {
        return edge;
    }
    let t = dot(sub(p, q[0]), n) / n2;
    let proj = sub(p, scale(n, t));
    if (0..4).all(|i| dot(cross(sub(q[(i + 1) % 4], q[i]), sub(proj, q[i])), n) >= -1.0e-4) {
        let d = sub(p, proj);
        return det_min_f64(dot(d, d), edge);
    }
    edge
}

fn dist2_point_tri(p: [f64; 3], v: [[i32; 3]; 3]) -> f64 {
    let (a, b, c) = (v3(v[0]), v3(v[1]), v3(v[2]));
    let ab = sub(b, a);
    let ac = sub(c, a);
    let n = cross(ab, ac);
    let n2 = dot(n, n);
    let edge = det_min_f64(
        det_min_f64(seg_dist2(p, a, b), seg_dist2(p, b, c)),
        seg_dist2(p, c, a),
    );
    if n2 <= 1.0e-18 {
        return edge;
    }
    let t = dot(sub(p, a), n) / n2;
    let proj = sub(p, scale(n, t));
    let bp = sub(proj, a);
    let d00 = dot(ab, ab);
    let d01 = dot(ab, ac);
    let d11 = dot(ac, ac);
    let d20 = dot(bp, ab);
    let d21 = dot(bp, ac);
    let denom = d00 * d11 - d01 * d01;
    if denom.abs() <= 1.0e-18 {
        return edge;
    }
    let vv = (d11 * d20 - d01 * d21) / denom;
    let w = (d00 * d21 - d01 * d20) / denom;
    let u = 1.0 - vv - w;
    if u >= -1.0e-6 && vv >= -1.0e-6 && w >= -1.0e-6 {
        let d = sub(p, proj);
        return det_min_f64(dot(d, d), edge);
    }
    edge
}

fn isqrt(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = x.midpoint(n / x);
    }
    x
}
