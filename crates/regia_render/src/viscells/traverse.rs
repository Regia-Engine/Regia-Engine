// SPDX-License-Identifier: MPL-2.0
//! Per-view portal traversal (ADR-7586; Luebke and Georges 1995, "Portals and Mirrors: Simple,
//! Fast Evaluation of Potentially Visible Sets", I3D; see `docs/provenance.md`).
//!
//! Start in the view's cell with the view frustum less its near plane (a door nearer than the
//! camera's near distance still leads on). For each portal of the current cell (but the
//! one just come through), clip its quad against the current planes; if anything is left, the
//! cell behind it is reachable, and the walk continues there with the frustum narrowed to the
//! clipped quad: one plane through the eye and each clipped edge, plus the portal's own plane as
//! the new near plane, plus the view's far plane. A cell may be entered more than once (through
//! different portal chains, each with its own narrower frustum).
//!
//! The walk starts in every cell [`VisCells::locate_view_cells`] gives: the eye's cell, or, for an
//! eye in no cell box but in a doorway's wall gap, both cells of that doorway (ADR-7639).
//!
//! Bounded and allocation-free once warm: the walk keeps an explicit stack in reused scratch, and
//! both caps err towards drawing (never pop geometry). A chain that reaches [`MAX_PORTAL_DEPTH`]
//! portals and still sees a portal onward stops narrowing there: every cell connected to the one
//! beyond, through any portals, counts as reachable (`depth_capped`, ADR-7639). A view enters at
//! most [`MAX_CELL_VISITS`] cells; at that cap every cell counts as reachable (`capped`). Where
//! the narrowed frustum would need more planes than [`MAX_PLANES`], or the eye stands on a
//! portal's plane, the walk keeps the current, wider planes (it may draw more, never less).

use regia_core::math::Vec3;

use super::file::VisCells;

/// The most planes a narrowed frustum holds. More edges keep the current planes (conservative).
pub const MAX_PLANES: usize = 16;

/// The longest portal chain one walk narrows through. A chain that reaches it and still sees a
/// portal onward counts everything connected beyond that portal as reachable.
pub const MAX_PORTAL_DEPTH: u32 = 32;

/// The most cell entries one view's walk makes; at the cap every cell is reachable.
pub const MAX_CELL_VISITS: u32 = 4096;

/// Under this distance from a portal's plane the eye is on the portal: the walk passes through
/// without narrowing (metres).
pub const EYE_ON_PORTAL_M: f32 = 0.02;

/// The most narrowed frusta one walk records into the outdoor cell
/// ([`TraversalScratch::outdoor_frusta`], ADR-8413). Past it the walk counts the outdoors as seen
/// whole ([`Traversal::outdoor_overflow`]).
pub const MAX_OUTDOOR_FRUSTA: usize = 64;

/// Most vertices a clipped quad can have: 4 plus one per clipping plane.
const MAX_POLY: usize = 4 + MAX_PLANES;

/// A convex volume: inward planes `n·p + d >= 0` inside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaneSet {
    planes: [[f32; 4]; MAX_PLANES],
    len: u8,
}

impl PlaneSet {
    /// The six planes of a view frustum ([`crate::Frustum::planes`]).
    #[must_use]
    pub fn from_frustum(planes: [[f32; 4]; 6]) -> Self {
        let mut set = Self {
            planes: [[0.0; 4]; MAX_PLANES],
            len: 0,
        };
        for p in planes {
            set.push(p);
        }
        set
    }

    fn push(&mut self, plane: [f32; 4]) -> bool {
        let len = self.len as usize;
        if len >= MAX_PLANES {
            return false;
        }
        self.planes[len] = plane;
        self.len += 1;
        true
    }

    /// The planes in use.
    #[must_use]
    pub fn planes(&self) -> &[[f32; 4]] {
        &self.planes[..self.len as usize]
    }

    /// Whether the box `lo`..`hi` may lie in the volume: no plane has the whole box more than
    /// `margin` metres outside it (the box's farthest corner along each normal is tested, so the
    /// answer errs towards `true`).
    #[must_use]
    pub fn intersects_box(&self, lo: [f32; 3], hi: [f32; 3], margin: f32) -> bool {
        self.planes().iter().all(|p| {
            let far = [
                if p[0] >= 0.0 { hi[0] } else { lo[0] },
                if p[1] >= 0.0 { hi[1] } else { lo[1] },
                if p[2] >= 0.0 { hi[2] } else { lo[2] },
            ];
            p[0] * far[0] + p[1] * far[1] + p[2] * far[2] + p[3] >= -margin
        })
    }

    /// Whether a sphere at `centre` with `radius` may lie in the volume (no plane has it wholly
    /// outside).
    #[must_use]
    pub fn intersects_sphere(&self, centre: [f32; 3], radius: f32) -> bool {
        self.planes()
            .iter()
            .all(|p| p[0] * centre[0] + p[1] * centre[1] + p[2] * centre[2] + p[3] >= -radius)
    }

    /// The same volume with every point moved by `offset` (a plane `n·p + d` becomes
    /// `n·p + d − n·offset`): from the frame's local space to a draw's world space, for example.
    #[must_use]
    pub fn shifted(&self, offset: [f32; 3]) -> Self {
        let mut out = *self;
        for p in &mut out.planes[..self.len as usize] {
            p[3] -= p[0] * offset[0] + p[1] * offset[1] + p[2] * offset[2];
        }
        out
    }
}

fn dist(plane: &[f32; 4], p: Vec3) -> f32 {
    plane[0] * p.x + plane[1] * p.y + plane[2] * p.z + plane[3]
}

/// A small fixed polygon.
#[derive(Clone, Copy)]
struct Poly {
    points: [Vec3; MAX_POLY],
    len: usize,
}

impl Poly {
    fn from_quad(quad: &[Vec3; 4]) -> Self {
        let mut points = [Vec3::ZERO; MAX_POLY];
        points[..4].copy_from_slice(quad);
        Self { points, len: 4 }
    }

    /// Sutherland–Hodgman against one plane (keeps the inside, `dist >= 0`).
    fn clip(&self, plane: &[f32; 4]) -> Self {
        let mut out = Self {
            points: [Vec3::ZERO; MAX_POLY],
            len: 0,
        };
        let push = |out: &mut Self, p: Vec3| {
            if out.len < MAX_POLY {
                out.points[out.len] = p;
                out.len += 1;
            }
        };
        for i in 0..self.len {
            let a = self.points[i];
            let b = self.points[(i + 1) % self.len];
            let (da, db) = (dist(plane, a), dist(plane, b));
            if da >= 0.0 {
                push(&mut out, a);
            }
            if (da >= 0.0) != (db >= 0.0) {
                let t = da / (da - db);
                push(&mut out, a + (b - a) * t);
            }
        }
        out
    }
}

/// One pending cell entry.
#[derive(Clone, Copy)]
struct Entry {
    cell: u32,
    /// The portal this entry came through (`u32::MAX` for the start).
    came_through: u32,
    depth: u32,
    planes: PlaneSet,
}

/// The walk's reusable scratch and its last result.
#[derive(Default)]
pub struct TraversalScratch {
    stack: Vec<Entry>,
    /// Per cell index: reached by this walk.
    reachable: Vec<bool>,
    /// The cell indices the last walk started in, ascending.
    start: Vec<u32>,
    /// Per cell index: already flooded past the depth cap by this walk.
    flooded: Vec<bool>,
    /// The flood's pending cells.
    flood: Vec<u32>,
    /// The narrowed planes of every entry into the outdoor cell (ADR-8413).
    outdoor: Vec<PlaneSet>,
}

impl TraversalScratch {
    /// Per cell index of the last walk: whether the cell is reachable.
    #[must_use]
    pub fn reachable(&self) -> &[bool] {
        &self.reachable
    }

    /// The cell indices the last walk started in, ascending
    /// ([`VisCells::locate_view_cells`]): one, or each cell of the doorway the eye stood in.
    #[must_use]
    pub fn start_cells(&self) -> &[u32] {
        &self.start
    }

    /// The planes of each entry the last walk made into the outdoor cell (cell 0) through a
    /// portal, at most [`MAX_OUTDOOR_FRUSTA`] (ADR-8413): the outdoors this view sees lies inside
    /// their union. Empty when the walk started outdoors or never reached it.
    #[must_use]
    pub fn outdoor_frusta(&self) -> &[PlaneSet] {
        &self.outdoor
    }
}

/// Past the depth cap: marks reachable every cell connected to `from` through any portals
/// (frustum ignored). Cells flooded earlier in the same walk are not walked again.
fn flood_from(
    cells: &VisCells,
    from: u32,
    flooded: &mut [bool],
    pending: &mut Vec<u32>,
    reachable: &mut [bool],
) {
    let Some(mark) = flooded.get_mut(from as usize) else {
        return;
    };
    if *mark {
        return;
    }
    *mark = true;
    pending.clear();
    pending.push(from);
    while let Some(cell) = pending.pop() {
        reachable[cell as usize] = true;
        let Some(info) = cells.cells.get(cell as usize) else {
            continue;
        };
        for &portal_index in &info.portals {
            let Some(portal) = cells.portals.get(portal_index as usize) else {
                continue;
            };
            let next = portal.other(cell);
            if let Some(mark) = flooded.get_mut(next as usize)
                && !*mark
            {
                *mark = true;
                pending.push(next);
            }
        }
    }
}

/// One walk's result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Traversal {
    /// The cell index the eye is in: the first of [`TraversalScratch::start_cells`].
    pub eye_cell: usize,
    /// The eye was in no cell box but in a doorway, so the walk started in each of the doorway's
    /// cells ([`TraversalScratch::start_cells`], ADR-7639).
    pub doorway: bool,
    /// Cell entries made (the start included), at most [`MAX_CELL_VISITS`].
    pub visits: u32,
    /// Portals whose clipped quad was non-empty (each passage counted).
    pub portals_passed: u32,
    /// The visit cap was hit, so every cell counts as reachable.
    pub capped: bool,
    /// A chain reached [`MAX_PORTAL_DEPTH`] and still saw a portal onward, so every cell connected
    /// beyond it counts as reachable (ADR-7639).
    pub depth_capped: bool,
    /// The walk started in the outdoor cell (the eye is outdoors, or in a doorway to it).
    pub started_outdoors: bool,
    /// More than [`MAX_OUTDOOR_FRUSTA`] entries reached the outdoor cell; the recorded list is
    /// incomplete, so the outdoors counts as seen whole.
    pub outdoor_overflow: bool,
}

/// The narrowed planes through the clipped portal `poly` seen from `eye`; `None` when the
/// planes do not fit (the caller keeps the wider set).
fn narrow(
    eye: Vec3,
    poly: &Poly,
    portal_normal: Vec3,
    portal_center: Vec3,
    far: [f32; 4],
) -> Option<PlaneSet> {
    let mut set = PlaneSet {
        planes: [[0.0; 4]; MAX_PLANES],
        len: 0,
    };
    let mut centroid = Vec3::ZERO;
    for p in &poly.points[..poly.len] {
        centroid += *p;
    }
    centroid /= poly.len as f32;
    for i in 0..poly.len {
        let a = poly.points[i];
        let b = poly.points[(i + 1) % poly.len];
        let n = (a - eye).cross(b - eye);
        let len = n.length();
        // An edge in line with the eye (or a repeated vertex) bounds nothing: skip it.
        if len.is_nan() || len <= 1.0e-9 {
            continue;
        }
        let mut n = n / len;
        if n.dot(centroid - eye) < 0.0 {
            n = -n;
        }
        if !set.push([n.x, n.y, n.z, -n.dot(eye)]) {
            return None;
        }
    }
    // The portal's plane, facing away from the eye: the next cell lies beyond it.
    let mut pn = portal_normal;
    if pn.dot(eye - portal_center) > 0.0 {
        pn = -pn;
    }
    if !set.push([pn.x, pn.y, pn.z, -pn.dot(portal_center)]) {
        return None;
    }
    set.push(far).then_some(set)
}

/// The walk's start planes: the view frustum without its near plane (plane 4). The near plane only
/// bounds the camera's depth range; a portal nearer than it (the eye in a doorway, walking through)
/// must still lead on. The side planes meet at the eye, so nothing behind it gets in.
fn start_planes(frustum: [[f32; 4]; 6]) -> PlaneSet {
    let mut set = PlaneSet {
        planes: [[0.0; 4]; MAX_PLANES],
        len: 0,
    };
    for (i, plane) in frustum.into_iter().enumerate() {
        if i != 4 {
            set.push(plane);
        }
    }
    set
}

/// Walks the portals of `cells` from `eye` with the view's frustum `frustum` (inward planes,
/// [`crate::Frustum::planes`]; plane 4 is the near plane, which the walk leaves out, and plane 5
/// the far plane). Fills `scratch.reachable()` (by cell index) and `scratch.start_cells()`.
/// Allocation-free once `scratch` has grown.
pub fn traverse(
    cells: &VisCells,
    eye: Vec3,
    frustum: [[f32; 4]; 6],
    scratch: &mut TraversalScratch,
) -> Traversal {
    let count = cells.cells.len();
    let TraversalScratch {
        stack,
        reachable,
        start,
        flooded,
        flood,
        outdoor: outdoor_frusta,
    } = scratch;
    outdoor_frusta.clear();
    let outdoor = cells.index_of(super::file::OUTDOOR_CELL).map(|i| i as u32);
    reachable.clear();
    reachable.resize(count, false);
    flooded.clear();
    flooded.resize(count, false);
    stack.clear();
    let doorway = cells.locate_view_cells(eye, start);
    let far = frustum[5];
    let planes = start_planes(frustum);
    // Pushed in reverse, so the walk pops the first start cell first.
    for &cell in start.iter().rev() {
        stack.push(Entry {
            cell,
            came_through: u32::MAX,
            depth: 0,
            planes,
        });
    }
    let mut result = Traversal {
        eye_cell: start.first().map_or(0, |c| *c as usize),
        doorway,
        started_outdoors: outdoor.is_some_and(|o| start.contains(&o)),
        ..Traversal::default()
    };
    while let Some(entry) = stack.pop() {
        if result.visits >= MAX_CELL_VISITS {
            result.capped = true;
            reachable.fill(true);
            stack.clear();
            break;
        }
        result.visits += 1;
        let cell_index = entry.cell as usize;
        let Some(cell) = cells.cells.get(cell_index) else {
            continue;
        };
        reachable[cell_index] = true;
        let at_depth_cap = entry.depth >= MAX_PORTAL_DEPTH;
        for &portal_index in &cell.portals {
            if portal_index == entry.came_through {
                continue;
            }
            let Some(portal) = cells.portals.get(portal_index as usize) else {
                continue;
            };
            let mut poly = Poly::from_quad(&portal.quad);
            for plane in entry.planes.planes() {
                poly = poly.clip(plane);
                if poly.len < 3 {
                    break;
                }
            }
            if poly.len < 3 {
                continue;
            }
            result.portals_passed += 1;
            if at_depth_cap {
                // Too deep to narrow on: what lies beyond counts as reachable (never pop).
                result.depth_capped = true;
                flood_from(cells, portal.other(entry.cell), flooded, flood, reachable);
                continue;
            }
            let on_portal = (eye - portal.center).dot(portal.normal).abs() < EYE_ON_PORTAL_M;
            let planes = if on_portal {
                entry.planes
            } else {
                narrow(eye, &poly, portal.normal, portal.center, far).unwrap_or(entry.planes)
            };
            let next = portal.other(entry.cell);
            if Some(next) == outdoor {
                if outdoor_frusta.len() < MAX_OUTDOOR_FRUSTA {
                    outdoor_frusta.push(planes);
                } else {
                    result.outdoor_overflow = true;
                }
            }
            stack.push(Entry {
                cell: next,
                came_through: portal_index,
                depth: entry.depth + 1,
                planes,
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frustum;

    /// A frustum at `eye` looking at `target`, 90 degrees, square.
    fn frustum(eye: Vec3, target: Vec3) -> [[f32; 4]; 6] {
        let view = crate::view_math::look_at_view(eye, target, Vec3::Y);
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(
            std::f32::consts::FRAC_PI_2,
            1.0,
            0.1,
            500.0,
        );
        Frustum::from_view_proj(&(proj * view)).planes()
    }

    /// Room A (cell 1) around the origin, room B (cell 2) at z −8, a doorway at z −4.
    fn two_rooms(extra: &str) -> VisCells {
        VisCells::from_ron(&format!(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
                (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
             ],
             portals: [(a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]){extra}])"
        ))
        .expect("valid")
    }

    fn reached(cells: &VisCells, scratch: &TraversalScratch) -> Vec<u16> {
        cells
            .cells
            .iter()
            .zip(scratch.reachable())
            .filter(|(_, r)| **r)
            .map(|(c, _)| c.id)
            .collect()
    }

    #[test]
    fn facing_away_from_the_door_reaches_only_the_own_room() {
        let cells = two_rooms("");
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 0.0);
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, 5.0)), &mut s);
        assert_eq!(cells.cells[t.eye_cell].id, 1);
        assert_eq!(reached(&cells, &s), vec![1]);
        assert_eq!(t.visits, 1);
    }

    #[test]
    fn looking_through_the_door_reaches_the_next_room() {
        let cells = two_rooms("");
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 0.0);
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -5.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![1, 2]);
        assert_eq!(t.visits, 2);
        assert_eq!(t.portals_passed, 1);
    }

    #[test]
    fn a_portal_seen_past_its_edge_is_not_passed() {
        // Looking along the wall at x = 3.9: the doorway (x within ±1) is outside the 90-degree
        // frustum only when the view turns well away; aim sideways (+X).
        let cells = two_rooms("");
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 0.0);
        traverse(&cells, eye, frustum(eye, Vec3::new(5.0, 1.0, 0.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![1]);
    }

    #[test]
    fn a_portal_to_outdoors_keeps_the_outside_reachable() {
        // Room A also opens to outdoors through a window at z +4.
        let cells = two_rooms(
            ", (a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)])",
        );
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 0.0);
        traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, 5.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![0, 1]);
        // From outdoors, looking at the window, room A is reachable and B (behind A's door,
        // in line) too; looking away from the window, only outdoors.
        let eye = Vec3::new(0.0, 1.2, 10.0);
        traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.2, 0.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![0, 1, 2]);
        traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.2, 20.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![0]);
    }

    #[test]
    fn a_door_seen_through_a_window_off_axis_is_narrowed_away() {
        // From outdoors far to the side, the window is visible but the door of room A (at the
        // far wall, x within ±1) lies outside the window's narrowed frustum.
        let cells = two_rooms(
            ", (a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)])",
        );
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(12.0, 1.2, 6.0);
        traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.2, 4.0)), &mut s);
        assert_eq!(
            reached(&cells, &s),
            vec![0, 1],
            "the window is seen, the door is not"
        );
    }

    #[test]
    fn a_ring_of_rooms_stays_bounded() {
        // Eight cells in a line, each joined to the next by two portals (a loop at every step):
        // the walk ends within the visit cap and reaches the cells the narrowing allows.
        let mut text = String::from("(cells: [");
        for i in 1..=8 {
            text.push_str(&format!(
                "(id: {i}, bounds: [(center: (0.0, 1.5, {:.1}), half_extents: (4.0, 1.5, 4.0))]),",
                -8.0 * (i - 1) as f32
            ));
        }
        text.push_str("], portals: [");
        for i in 1..8 {
            let z = -8.0 * (i - 1) as f32 - 4.0;
            text.push_str(&format!(
                "(a: {i}, b: {}, quad: [(-3.0, 0.0, {z:.1}), (-1.0, 0.0, {z:.1}), (-1.0, 2.0, {z:.1}), (-3.0, 2.0, {z:.1})]),",
                i + 1
            ));
            text.push_str(&format!(
                "(a: {}, b: {i}, quad: [(1.0, 0.0, {z:.1}), (3.0, 0.0, {z:.1}), (3.0, 2.0, {z:.1}), (1.0, 2.0, {z:.1})]),",
                i + 1
            ));
        }
        text.push_str("])");
        let cells = VisCells::from_ron(&text).expect("valid");
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 2.0);
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -100.0)), &mut s);
        assert!(!t.capped);
        assert!(t.visits <= MAX_CELL_VISITS);
        assert!(t.visits < 200, "two portals a step never explode: {t:?}");
        assert!(s.reachable()[1] && s.reachable()[2], "the next room is seen");
    }

    #[test]
    fn standing_in_the_doorway_sees_both_rooms() {
        let cells = two_rooms("");
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, -4.0);
        // On the portal plane, looking along it (sideways): no narrowing, both rooms stay.
        traverse(&cells, eye, frustum(eye, Vec3::new(-5.0, 1.0, -4.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![1, 2]);
    }

    #[test]
    fn a_door_nearer_than_the_near_plane_still_leads_to_the_next_room() {
        // Walking through a doorway: the eye is closer to the door than the camera's near
        // distance (0.1 m in `frustum`), looking into room B. B stays reachable, both off the
        // portal (narrowed) and within `EYE_ON_PORTAL_M` of it (not narrowed).
        let cells = two_rooms("");
        let mut s = TraversalScratch::default();
        for z in [-3.92_f32, -3.95, -3.99, -3.999] {
            let eye = Vec3::new(0.0, 1.0, z);
            let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -10.0)), &mut s);
            assert_eq!(cells.cells[t.eye_cell].id, 1, "eye at z {z}");
            assert_eq!(reached(&cells, &s), vec![1, 2], "eye at z {z}");
        }
        // Facing away from the door at the same distance, B stays culled.
        let eye = Vec3::new(0.0, 1.0, -3.95);
        traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, 5.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![1]);
    }

    /// Room A (cell 1) and room B (cell 2) whose boxes stop at the faces of a 0.3 m wall between
    /// z −3.85 and z −4.15; the doorway portal sits in the wall's middle plane (z −4).
    fn two_rooms_with_a_wall() -> VisCells {
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 3.85))]),
                (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 3.85))]),
             ],
             portals: [(a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)])])",
        )
        .expect("valid")
    }

    #[test]
    fn an_eye_in_the_wall_gap_of_a_doorway_sees_both_rooms() {
        // Walking through the door, the eye passes the 0.3 m the room boxes leave open. It is in
        // neither box, but it is in the doorway: both rooms stay reachable whichever way it looks
        // (the eye is in both cells), never "outdoors" with both rooms culled.
        let cells = two_rooms_with_a_wall();
        let mut s = TraversalScratch::default();
        for z in [-3.86_f32, -3.95, -4.0, -4.05, -4.14] {
            let eye = Vec3::new(0.0, 1.0, z);
            assert_eq!(cells.locate(eye), 0, "the eye at z {z} is in no box");
            for target_z in [-10.0_f32, 10.0] {
                let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, target_z)), &mut s);
                assert_eq!(
                    reached(&cells, &s),
                    vec![1, 2],
                    "eye at z {z} looking toward z {target_z}"
                );
                assert!(t.doorway, "{t:?}");
                assert_eq!(s.start_cells(), &[1, 2], "both rooms are start cells");
                assert_eq!(t.eye_cell, 1, "the first start cell");
            }
        }
        // Off to the side of the doorway (inside the wall's thickness but not in the door's
        // opening), and far outside every box, the eye is outdoors as before.
        for eye in [Vec3::new(3.0, 1.0, -4.0), Vec3::new(50.0, 1.0, -4.0)] {
            let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -10.0)), &mut s);
            assert_eq!(reached(&cells, &s), vec![0], "eye at {eye}");
            assert!(!t.doorway && t.eye_cell == 0, "{t:?}");
        }
    }

    /// `count` rooms in a row down −Z, 8 m apart, each joined to the next by the same 2 m doorway.
    fn corridor(count: u32) -> VisCells {
        let mut text = String::from("(cells: [");
        for i in 1..=count {
            text.push_str(&format!(
                "(id: {i}, bounds: [(center: (0.0, 1.5, {:.1}), half_extents: (4.0, 1.5, 4.0))]),",
                -8.0 * (i - 1) as f32
            ));
        }
        text.push_str("], portals: [");
        for i in 1..count {
            let z = -8.0 * (i - 1) as f32 - 4.0;
            text.push_str(&format!(
                "(a: {i}, b: {}, quad: [(-1.0, 0.0, {z:.1}), (1.0, 0.0, {z:.1}), (1.0, 2.0, {z:.1}), (-1.0, 2.0, {z:.1})]),",
                i + 1
            ));
        }
        text.push_str("])");
        VisCells::from_ron(&text).expect("valid")
    }

    #[test]
    fn a_chain_deeper_than_the_depth_cap_still_reaches_its_far_end() {
        // 40 rooms in a line, every doorway in line with the eye: the room at the far end lies
        // 39 portals down one chain, deeper than MAX_PORTAL_DEPTH. Never pop geometry: the rooms
        // beyond the cap count as reachable.
        let rooms = 40;
        assert!(rooms - 1 > MAX_PORTAL_DEPTH);
        let cells = corridor(rooms);
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 2.0);
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -400.0)), &mut s);
        assert!(!t.capped, "{t:?}");
        assert_eq!(
            reached(&cells, &s),
            (1..=rooms as u16).collect::<Vec<_>>(),
            "every room down the corridor: {t:?}"
        );
        assert!(t.depth_capped, "the chain reached the depth cap: {t:?}");
        assert_eq!(t.visits, MAX_PORTAL_DEPTH + 1, "the walk narrows 32 portals deep");
        // Facing away, the corridor stays culled: the cap only widens a chain that reached it.
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, 10.0)), &mut s);
        assert_eq!(reached(&cells, &s), vec![1]);
        assert!(!t.depth_capped);
        // A corridor exactly as deep as the cap ends there without hitting it.
        let cells = corridor(MAX_PORTAL_DEPTH + 1);
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -400.0)), &mut s);
        assert!(!t.depth_capped, "{t:?}");
        assert_eq!(
            s.reachable().iter().filter(|r| **r).count(),
            MAX_PORTAL_DEPTH as usize + 1
        );
        // Beyond the cap the flood ignores the frustum but follows the portal graph: a room in
        // line with the corridor but joined to nothing stays culled.
        let mut doc = corridor(rooms).document().clone();
        doc.cells.push(crate::viscells::VisCellDef {
            id: 900,
            name: String::new(),
            outdoor: false,
            bounds: vec![crate::viscells::CellBox {
                center: [0.0, 1.5, -400.0],
                half_extents: [4.0, 1.5, 4.0],
                rotation_radians: 0.0,
            }],
        });
        let cells = VisCells::from_document(doc).expect("valid");
        let t = traverse(&cells, eye, frustum(eye, Vec3::new(0.0, 1.0, -400.0)), &mut s);
        assert!(t.depth_capped);
        let ids = reached(&cells, &s);
        assert!(ids.contains(&(rooms as u16)) && !ids.contains(&900), "{ids:?}");
    }

    #[test]
    fn the_walk_allocates_nothing_once_warm_and_is_deterministic() {
        let cells = two_rooms("");
        let mut s = TraversalScratch::default();
        let eye = Vec3::new(0.0, 1.0, 0.0);
        let f = frustum(eye, Vec3::new(0.0, 1.0, -5.0));
        let first = traverse(&cells, eye, f, &mut s);
        let cap = (s.stack.capacity(), s.reachable.capacity(), s.start.capacity());
        for _ in 0..10 {
            assert_eq!(traverse(&cells, eye, f, &mut s), first);
        }
        assert_eq!(
            (s.stack.capacity(), s.reachable.capacity(), s.start.capacity()),
            cap
        );
    }
}
