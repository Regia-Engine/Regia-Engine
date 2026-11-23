// SPDX-License-Identifier: MPL-2.0
//! What of the outdoors a view inside a cell can see (plan row W-22, ADR-8413).
//!
//! The terrain heightfield and the terrain scatter belong to the outdoor cell (cell 0) but are not
//! entities, so the stage 1 draw cull never sees them. An eye inside a sealed cell sees the
//! outdoors only through the portals its walk passes into cell 0, each narrowed to the clipped
//! portal ([`super::traverse()`]). [`OutdoorPortals`] keeps those narrowed frusta for one view, and
//! the terrain draw and the scatter field test their nodes and instance blocks against their union:
//! what lies in none of them is behind the cell's walls and is not drawn.
//!
//! The list is "unrestricted" (everything passes, the draws are exactly what they were without
//! cells) when the eye starts outdoors or in a doorway to it, when the walk hit its visit or depth
//! cap, when more than [`super::MAX_OUTDOOR_FRUSTA`] entries reached the outdoors, and when no
//! cells are loaded or culling is off. Restricted with no frustum means the view sees no outdoors
//! at all.
//!
//! The list also keeps a bounding sphere per mouth (a portal between a reached indoor cell and
//! cell 0): sunlight reaches the inside of a cell only through those, so a scatter caster whose
//! shadow can reach a mouth keeps casting even when the mouth itself is out of view.

use regia_core::math::Vec3;

use super::file::{OUTDOOR_CELL, VisCells};
use super::traverse::{PlaneSet, Traversal, TraversalScratch, traverse};

/// How far outside every portal frustum a terrain node's box may reach and still be drawn,
/// metres: covers the temporal jitter at the portal's edges and the vertex morph, which can move a
/// node's drawn surface off its height box by a few centimetres.
pub const OUTDOOR_NODE_MARGIN_M: f32 = 0.5;

/// The outdoor frusta of one view inside a cell (module docs). Its lists keep their capacity when
/// cleared, refilled or copied from another list, so a steady frame allocates nothing.
#[derive(Debug, Default, PartialEq)]
pub struct OutdoorPortals {
    restricted: bool,
    eye: [f32; 3],
    frusta: Vec<PlaneSet>,
    mouths: Vec<[f32; 4]>,
}

impl Clone for OutdoorPortals {
    fn clone(&self) -> Self {
        Self {
            restricted: self.restricted,
            eye: self.eye,
            frusta: self.frusta.clone(),
            mouths: self.mouths.clone(),
        }
    }

    /// Copies into the existing lists instead of replacing them, so a consumer that copies the
    /// list every frame (the terrain draw and the water layer, in their `set_outdoor_portals`)
    /// allocates nothing on a steady interior frame. A derived `Clone` would drop the buffers:
    /// its `clone_from` assigns a fresh `clone()`.
    fn clone_from(&mut self, source: &Self) {
        self.restricted = source.restricted;
        self.eye = source.eye;
        self.frusta.clone_from(&source.frusta);
        self.mouths.clone_from(&source.mouths);
    }
}

/// What a shared draw (the terrain) culled through the outdoor frusta on its last main-view
/// prepare ([`crate::SharedPassDraw::outdoor_cull_counts`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutdoorCullCounts {
    /// The prepare applied a restricted [`OutdoorPortals`] whose eye matched the frame's.
    pub restricted: bool,
    /// Nodes the camera passes draw (after the frustum test and the outdoor frusta).
    pub nodes_drawn: u32,
    /// Nodes in the view frustum that no outdoor frustum admits, so left out.
    pub nodes_culled: u32,
    /// Grass patches whose blades the camera passes may draw, after the outdoor frusta (ADR-8843).
    pub grass_patches_drawn: u32,
    /// Grass patches the outdoor frusta took out of the camera passes. A shadow patch among them
    /// keeps casting into the sun's cascades.
    pub grass_patches_culled: u32,
    /// Scatter impostor instances in tiles some outdoor frustum admits (the GPU cull still tests
    /// each one against the view).
    pub impostors_kept: u32,
    /// Scatter impostor instances in tiles no outdoor frustum admits: the GPU cull skips them.
    pub impostors_culled: u32,
}

impl OutdoorPortals {
    /// Unrestricted: everything outdoors may be seen. Keeps the lists' capacity.
    pub fn clear(&mut self) {
        self.restricted = false;
        self.eye = [0.0; 3];
        self.frusta.clear();
        self.mouths.clear();
    }

    /// Whether the outdoors is seen only through [`Self::frusta`].
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        self.restricted
    }

    /// The eye the list was made for, world metres.
    #[must_use]
    pub fn eye(&self) -> [f32; 3] {
        self.eye
    }

    /// Whether the list restricts a view whose eye is `eye`: restricted, and made for exactly that
    /// eye (bit for bit). Another view (a render-to-texture camera, a stale list) is not restricted.
    #[must_use]
    pub fn restricts(&self, eye: [f32; 3]) -> bool {
        self.restricted && self.eye.map(f32::to_bits) == eye.map(f32::to_bits)
    }

    /// The narrowed frusta into the outdoor cell (empty and restricted: no outdoors in view).
    #[must_use]
    pub fn frusta(&self) -> &[PlaneSet] {
        &self.frusta
    }

    /// Bounding spheres `[x, y, z, radius]` of the mouths of the cells the view reaches.
    #[must_use]
    pub fn mouths(&self) -> &[[f32; 4]] {
        &self.mouths
    }

    /// Whether the box `lo`..`hi` may be seen: unrestricted, or some frustum takes it within
    /// `margin` metres ([`PlaneSet::intersects_box`]).
    #[must_use]
    pub fn admits_box(&self, lo: [f32; 3], hi: [f32; 3], margin: f32) -> bool {
        !self.restricted || self.frusta.iter().any(|f| f.intersects_box(lo, hi, margin))
    }

    /// Whether a sphere may be seen: unrestricted, or some frustum takes it.
    #[must_use]
    pub fn admits_sphere(&self, centre: [f32; 3], radius: f32) -> bool {
        !self.restricted || self.frusta.iter().any(|f| f.intersects_sphere(centre, radius))
    }

    /// Whether a sphere at `centre` with `reach` (its radius plus its shadow's length) comes within
    /// a mouth's bounding sphere.
    #[must_use]
    pub fn near_mouth(&self, centre: [f32; 3], reach: f32) -> bool {
        let c = Vec3::from_array(centre);
        self.mouths
            .iter()
            .any(|m| c.distance(Vec3::new(m[0], m[1], m[2])) <= m[3] + reach)
    }

    /// The same list with every point moved by `offset` (frusta and mouths): from the frame's local
    /// space to a draw's world space. The eye is kept, so [`Self::restricts`] still matches the
    /// frame's eye.
    pub fn shift_into(&self, offset: [f32; 3], out: &mut Self) {
        out.restricted = self.restricted;
        out.eye = self.eye;
        out.frusta.clear();
        out.frusta.extend(self.frusta.iter().map(|f| f.shifted(offset)));
        out.mouths.clear();
        out.mouths.extend(
            self.mouths
                .iter()
                .map(|m| [m[0] + offset[0], m[1] + offset[1], m[2] + offset[2], m[3]]),
        );
    }

    /// Fills the list from the walk `walk` just made from `eye` (its `scratch`): restricted to the
    /// recorded outdoor frusta unless the walk started outdoors, hit a cap or overflowed the
    /// frustum list (then unrestricted).
    pub fn fill_from_walk(
        &mut self,
        cells: &VisCells,
        eye: [f32; 3],
        walk: &Traversal,
        scratch: &TraversalScratch,
    ) {
        self.clear();
        let Some(outdoor) = cells.index_of(OUTDOOR_CELL) else {
            return;
        };
        if walk.started_outdoors || walk.capped || walk.depth_capped || walk.outdoor_overflow {
            return;
        }
        self.restricted = true;
        self.eye = eye;
        self.frusta.extend_from_slice(scratch.outdoor_frusta());
        let reachable = scratch.reachable();
        for portal in &cells.portals {
            let inner = if portal.a as usize == outdoor {
                portal.b
            } else if portal.b as usize == outdoor {
                portal.a
            } else {
                continue;
            };
            if !reachable.get(inner as usize).copied().unwrap_or(false) {
                continue;
            }
            let radius = portal
                .quad
                .iter()
                .map(|q| q.distance(portal.center))
                .fold(0.0_f32, f32::max);
            self.mouths
                .push([portal.center.x, portal.center.y, portal.center.z, radius]);
        }
    }
}

/// One [`OutdoorPortals`] per window view of a frame (split-screen; plan row E, ADR-8843), filled by
/// [`crate::viscells::cull_unreachable_cells`] in [`crate::ExtractedFrame::outdoor_portal_views`].
/// A window view whose eye starts outdoors, or whose walk hit a cap, has no entry: it is
/// unrestricted. The per-view draws (the terrain, its grass and impostors, the water) take their
/// view's entry ([`Self::get`]); the frame's one draw list (the scatter) takes the union
/// ([`Self::union_into`]). Entries keep their capacity when cleared, so a steady frame allocates
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct OutdoorPortalViews {
    /// `(view index, list)`; only the first `len` are this frame's.
    lists: Vec<(usize, OutdoorPortals)>,
    len: usize,
}

impl OutdoorPortalViews {
    /// No entry (every view unrestricted). Keeps every list's capacity.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Entries this frame.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// No entry this frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The list of view `view` (its index in [`crate::ExtractedFrame::views`]), if it has one.
    #[must_use]
    pub fn get(&self, view: usize) -> Option<&OutdoorPortals> {
        self.lists[..self.len]
            .iter()
            .find(|(index, _)| *index == view)
            .map(|(_, list)| list)
    }

    /// This frame's entries, in the order they were added (window order).
    pub fn iter(&self) -> impl Iterator<Item = (usize, &OutdoorPortals)> {
        self.lists[..self.len].iter().map(|(index, list)| (*index, list))
    }

    /// A cleared list for view `view`, appended (a list left from an earlier frame is reused).
    pub fn push(&mut self, view: usize) -> &mut OutdoorPortals {
        if self.len == self.lists.len() {
            self.lists.push((view, OutdoorPortals::default()));
        }
        let slot = &mut self.lists[self.len];
        self.len += 1;
        slot.0 = view;
        slot.1.clear();
        &mut slot.1
    }

    /// The union of the entries into `out`, for a draw list every view shares: restricted only
    /// when every entry is (a view with no entry, or an unrestricted one, sees the whole outdoors,
    /// so the caller passes `views` = the number of window views), with the first entry's eye, all
    /// the frusta and all the mouths. `out` is left unrestricted otherwise.
    pub fn union_into(&self, views: usize, out: &mut OutdoorPortals) {
        out.clear();
        let entries = &self.lists[..self.len];
        if views == 0 || entries.len() < views || entries.iter().any(|(_, l)| !l.restricted) {
            return;
        }
        out.restricted = true;
        out.eye = entries[0].1.eye;
        for (_, list) in entries {
            out.frusta.extend_from_slice(&list.frusta);
            for mouth in &list.mouths {
                if !out.mouths.contains(mouth) {
                    out.mouths.push(*mouth);
                }
            }
        }
    }
}

/// How far past the screen's edge the outdoor walk looks, pixels: the terrain draws the nodes up to
/// two pixels outside the view (its TAA slack), so a portal clipped by the screen's edge narrows to
/// a frustum that takes them too.
pub const OUTDOOR_EDGE_SLACK_PX: f32 = 2.0;

/// `frustum` ([`crate::Frustum::planes`]: left, right, bottom, top, near, far) with its four side
/// planes turned outward about the eye so that their tangent from the view axis grows by
/// `1 + slack` (the near plane's normal is the axis): the frustum a view-projection whose clip `x`
/// and `y` bounds are widened to `±w (1 + slack)` has. Near and far are kept.
#[must_use]
pub fn widen_frustum(frustum: [[f32; 4]; 6], eye: [f32; 3], slack: f32) -> [[f32; 4]; 6] {
    let eye = Vec3::from_array(eye);
    let axis = Vec3::new(frustum[4][0], frustum[4][1], frustum[4][2]).normalize_or_zero();
    let mut out = frustum;
    if axis == Vec3::ZERO || !(slack.is_finite() && slack > 0.0) {
        return out;
    }
    for plane in out.iter_mut().take(4) {
        let n = Vec3::new(plane[0], plane[1], plane[2]).normalize_or_zero();
        let along = n.dot(axis);
        let across = n - axis * along;
        let len = across.length();
        if len <= 1.0e-6 {
            continue;
        }
        let tangent = along / len;
        let widened = (across / len + axis * (tangent * (1.0 + slack))).normalize_or_zero();
        if widened == Vec3::ZERO {
            continue;
        }
        *plane = [widened.x, widened.y, widened.z, -widened.dot(eye)];
    }
    out
}

/// Walks the portals from `eye` with the view's frustum `frustum` ([`crate::Frustum::planes`]),
/// widened by [`OUTDOOR_EDGE_SLACK_PX`] at a view of `size_px` pixels ([`widen_frustum`]), and
/// fills `out` (module docs): for a host that drives [`crate::FrameRenderer`] itself and hands
/// the list to [`crate::FrameRenderer::set_outdoor_portals`]. Allocation-free once `scratch` and
/// `out` have grown.
pub fn outdoor_portals(
    cells: &VisCells,
    eye: [f32; 3],
    frustum: [[f32; 4]; 6],
    size_px: [u32; 2],
    scratch: &mut TraversalScratch,
    out: &mut OutdoorPortals,
) -> Traversal {
    let slack = 2.0 * OUTDOOR_EDGE_SLACK_PX / size_px[0].min(size_px[1]).max(1) as f32;
    let walk = traverse(
        cells,
        Vec3::from_array(eye),
        widen_frustum(frustum, eye, slack),
        scratch,
    );
    out.fill_from_walk(cells, eye, &walk, scratch);
    walk
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frustum;

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

    /// A 4 m wide, 3 m tall tube (cell 1) along −Z from z 0 to z −20, with a 2 m by 2 m mouth to
    /// the outdoors at z 0 and the far end sealed.
    fn tube() -> VisCells {
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, -10.0), half_extents: (2.0, 1.5, 10.0))]),
             ],
             portals: [(a: 1, b: 0, quad: [(-1.0, 0.0, 0.0), (1.0, 0.0, 0.0), (1.0, 2.0, 0.0), (-1.0, 2.0, 0.0)])])",
        )
        .expect("valid")
    }

    #[test]
    fn inside_facing_the_sealed_end_sees_no_outdoors() {
        let cells = tube();
        let mut scratch = TraversalScratch::default();
        let mut out = OutdoorPortals::default();
        let eye = Vec3::new(0.0, 1.0, -10.0);
        outdoor_portals(
            &cells,
            eye.to_array(),
            frustum(eye, Vec3::new(0.0, 1.0, -30.0)),
            [256, 256],
            &mut scratch,
            &mut out,
        );
        assert!(out.is_restricted() && out.frusta().is_empty(), "{out:?}");
        assert!(out.restricts(eye.to_array()));
        assert!(!out.restricts([0.0, 1.0, -10.5]), "another eye is not restricted");
        assert!(!out.admits_box([-100.0, -1.0, -100.0], [100.0, 0.0, 100.0], 0.0));
        // The mouth is still listed: the sun reaches the inside through it.
        assert_eq!(out.mouths().len(), 1);
        assert!(out.near_mouth([0.0, 1.0, 5.0], 4.0) && !out.near_mouth([0.0, 1.0, 50.0], 4.0));
    }

    #[test]
    fn inside_facing_the_mouth_sees_the_outdoors_through_it_only() {
        let cells = tube();
        let mut scratch = TraversalScratch::default();
        let mut out = OutdoorPortals::default();
        let eye = Vec3::new(0.0, 1.0, -10.0);
        outdoor_portals(
            &cells,
            eye.to_array(),
            frustum(eye, Vec3::new(0.0, 1.0, 10.0)),
            [256, 256],
            &mut scratch,
            &mut out,
        );
        assert!(out.is_restricted());
        assert_eq!(out.frusta().len(), 1);
        // Straight out of the mouth, 50 m on: seen. Far to the side at the same distance, and
        // above the tube (behind the mouth's plane): not.
        assert!(out.admits_box([-1.0, 0.0, 49.0], [1.0, 2.0, 51.0], 0.0));
        assert!(!out.admits_box([40.0, 0.0, 49.0], [42.0, 2.0, 51.0], 0.0));
        assert!(!out.admits_box([-5.0, 3.5, -12.0], [5.0, 4.0, -8.0], 0.0));
        assert!(out.admits_sphere([0.0, 1.0, 30.0], 0.5));
        assert!(!out.admits_sphere([30.0, 1.0, 30.0], 0.5));
        // Moved into another space, the same box moves with it.
        let mut shifted = OutdoorPortals::default();
        out.shift_into([100.0, 0.0, 0.0], &mut shifted);
        assert!(shifted.admits_box([99.0, 0.0, 49.0], [101.0, 2.0, 51.0], 0.0));
        assert!(!shifted.admits_box([-1.0, 0.0, 49.0], [1.0, 2.0, 51.0], 0.0));
        assert!(shifted.restricts(eye.to_array()));
    }

    #[test]
    fn a_widened_frustum_takes_a_point_just_past_the_edge() {
        let eye = Vec3::new(0.0, 1.0, 0.0);
        let f = frustum(eye, Vec3::new(0.0, 1.0, -10.0));
        let wide = widen_frustum(f, eye.to_array(), 0.1);
        // 90 degrees: the right edge at 10 m in front is x = 10; 5 % past it is outside, inside widened.
        let point = [10.5_f32, 1.0, -10.0];
        let inside = |planes: &[[f32; 4]; 6]| {
            planes
                .iter()
                .all(|p| p[0] * point[0] + p[1] * point[1] + p[2] * point[2] + p[3] >= 0.0)
        };
        assert!(!inside(&f) && inside(&wide));
        assert_eq!(wide[4], f[4], "near kept");
        assert_eq!(wide[5], f[5], "far kept");
        assert!(
            !inside(&widen_frustum(f, eye.to_array(), 0.01)),
            "1 % does not reach 5 %"
        );
    }

    #[test]
    fn the_union_is_restricted_only_when_every_window_view_is() {
        let cells = tube();
        let mut scratch = TraversalScratch::default();
        let mut views = OutdoorPortalViews::default();
        let inside = Vec3::new(0.0, 1.0, -10.0);
        // Two players inside the tube: one facing the mouth, one facing the sealed end.
        for (view, target) in [(0, Vec3::new(0.0, 1.0, 10.0)), (1, Vec3::new(0.0, 1.0, -30.0))] {
            let slot = views.push(view);
            outdoor_portals(
                &cells,
                inside.to_array(),
                frustum(inside, target),
                [256, 256],
                &mut scratch,
                slot,
            );
        }
        assert_eq!(views.len(), 2);
        assert_eq!(views.get(0).map(|l| l.frusta().len()), Some(1));
        assert_eq!(views.get(1).map(|l| l.frusta().len()), Some(0));
        assert!(views.get(2).is_none());
        let mut union = OutdoorPortals::default();
        views.union_into(2, &mut union);
        assert!(union.restricts(inside.to_array()));
        assert_eq!(union.frusta().len(), 1);
        assert_eq!(union.mouths().len(), 1, "the shared mouth is listed once");
        // A third window view without an entry (outdoors, or a capped walk) sees everything.
        views.union_into(3, &mut union);
        assert!(!union.is_restricted());
        // Clearing keeps the lists for the next frame.
        views.clear();
        assert!(views.is_empty() && views.get(0).is_none());
        let again = views.push(4);
        assert!(!again.is_restricted() && again.frusta().is_empty());
        assert_eq!(views.get(4).map(OutdoorPortals::is_restricted), Some(false));
    }

    #[test]
    fn an_eye_outdoors_is_never_restricted() {
        let cells = tube();
        let mut scratch = TraversalScratch::default();
        let mut out = OutdoorPortals::default();
        let eye = Vec3::new(0.0, 1.0, 10.0);
        outdoor_portals(
            &cells,
            eye.to_array(),
            frustum(eye, Vec3::new(0.0, 1.0, -10.0)),
            [256, 256],
            &mut scratch,
            &mut out,
        );
        assert!(!out.is_restricted() && out.frusta().is_empty() && out.mouths().is_empty());
        assert!(out.admits_box([500.0, 0.0, 500.0], [501.0, 1.0, 501.0], 0.0));
        assert!(!out.restricts(eye.to_array()));
    }

    /// Straight out of the tube's mouth: the same box `inside_facing_the_mouth` keeps.
    fn sees_out(out: &OutdoorPortals) -> bool {
        out.admits_box([-1.0, 0.0, 49.0], [1.0, 2.0, 51.0], 0.0)
    }

    fn walk(eye: Vec3, target: Vec3) -> OutdoorPortals {
        let cells = tube();
        let mut scratch = TraversalScratch::default();
        let mut out = OutdoorPortals::default();
        outdoor_portals(
            &cells,
            eye.to_array(),
            frustum(eye, target),
            [256, 256],
            &mut scratch,
            &mut out,
        );
        out
    }

    /// The cell box includes the mouth face, so an eye standing in the opening is indoors.
    /// Looking straight out, the outdoors beyond the mouth must still be admitted (the same box
    /// `inside_facing_the_mouth` keeps).
    #[test]
    fn an_eye_on_the_portal_plane_still_sees_out_the_mouth() {
        let ahead = Vec3::new(0.0, 1.0, 30.0);
        // Just inside the face: the quad is in front of the eye and the walk keeps it.
        let inside = walk(Vec3::new(0.0, 1.0, -0.05), ahead);
        assert!(
            sees_out(&inside),
            "5 cm inside should see out (restricted {}, frusta {})",
            inside.is_restricted(),
            inside.frusta().len()
        );
        // Standing in the opening, on the quad, looking out. The cell box includes this face.
        for eye in [Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.4, 1.2, 0.0)] {
            let out = walk(eye, ahead);
            assert!(
                sees_out(&out),
                "eye {eye} on the mouth culled the outdoors (restricted {}, frusta {})",
                out.is_restricted(),
                out.frusta().len()
            );
        }
    }

    /// A box that crosses the jamb is partly in the opening, so the outdoor list must keep it.
    #[test]
    fn a_box_across_the_portal_edge_stays_visible() {
        let out = walk(Vec3::new(0.0, 1.0, -10.0), Vec3::new(0.0, 1.0, 10.0));
        assert!(out.is_restricted() && out.frusta().len() == 1, "{out:?}");
        assert!(
            out.admits_box([0.4, 0.2, 0.3], [1.3, 1.8, 1.2], OUTDOOR_NODE_MARGIN_M),
            "the jamb box is partly inside the 2 m mouth"
        );
        assert!(
            !out.admits_box([40.0, 0.0, 49.0], [42.0, 2.0, 51.0], OUTDOOR_NODE_MARGIN_M),
            "a box far beside the mouth must still go, margin included"
        );
    }

    /// Same eye and frustum twice: the list does not depend on a clock.
    #[test]
    fn the_same_eye_gives_the_same_list() {
        let eye = Vec3::new(0.0, 1.0, -10.0);
        let target = Vec3::new(0.0, 1.0, 10.0);
        assert_eq!(walk(eye, target), walk(eye, target));
    }

    /// `clone_from` is what the terrain draw and the water layer do with the list every frame
    /// (`TerrainSharedDraw::set_outdoor_portals`, `WaterLayer::set_outdoor_portals`). A derived
    /// `Clone` replaces the vecs, so a steady interior frame allocates.
    #[test]
    fn copying_a_list_keeps_the_buffers() {
        let src = walk(Vec3::new(0.0, 1.0, -10.0), Vec3::new(0.0, 1.0, 10.0));
        assert!(src.is_restricted() && !src.frusta.is_empty());
        let mut dst = OutdoorPortals::default();
        dst.frusta.reserve(32);
        dst.mouths.reserve(32);
        let (cap_f, cap_m) = (dst.frusta.capacity(), dst.mouths.capacity());
        assert!(src.frusta.capacity() < cap_f && src.mouths.capacity() < cap_m);
        dst.clone_from(&src);
        assert!(dst.is_restricted() && dst.frusta.len() == src.frusta.len());
        assert!(
            dst.frusta.capacity() >= cap_f && dst.mouths.capacity() >= cap_m,
            "clone_from dropped the buffers (frusta {} -> {}, mouths {} -> {})",
            cap_f,
            dst.frusta.capacity(),
            cap_m,
            dst.mouths.capacity()
        );
    }
}
