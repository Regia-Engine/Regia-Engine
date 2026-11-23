// SPDX-License-Identifier: MPL-2.0
//! Custom handle gizmos and snap targets (Tincal engine request F6).
//!
//! The built-in transform gizmo only translates, rotates and scales. Joint pivots, axis and limit
//! dials, socket and hold placement and IK targets need handles of their own, and snapping to the
//! features of a mesh. This module has two halves:
//!
//! - [`handles`] draws a slice of [`Handle`]s (a [`Handle::Point`], [`Handle::Arrow`],
//!   [`Handle::Arc`] dial or [`Handle::Plane`]) over a viewport rect, handles hover and drag, and
//!   returns a [`HandleDrag`] with the world-space delta. The input handling is a plain state
//!   machine ([`HandleState::update`]) fed by [`PointerInput`], so tests (and the agent method
//!   `editor.handle_drag`) drive it without egui.
//! - [`snap_targets`] collects a [`SnapSet`] from an entity and its descendants: face centers
//!   (coplanar triangles merged into one face), edge midpoints (feature edges only), bounding-box
//!   centers and [`SnapConnector`] contact faces. [`SnapSet::nearest`] picks within a pixel radius.
//!
//! The math is free functions ([`ray_plane`], [`ray_line_param`], [`ray_arc_angle`],
//! [`segment_distance`]) so it is tested on its own. Specification: `book/user-guide/handles.md`,
//! `book/agents/handles.md`.

use std::collections::HashMap;

use regia_core::math::{Mat4, Vec2, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_render::{JointMatrix, MeshRenderer, SkinJoints};
use regia_scene::GlobalTransform;

use crate::views::ViewCamera;

/// Screen radius, pixels, within which a handle is hovered and a snap point is taken.
pub const DEFAULT_PICK_RADIUS: f32 = 10.0;
/// Snap points kept per [`snap_targets`] call before the set is marked truncated.
pub const DEFAULT_MAX_POINTS: usize = 8192;
/// Faces whose normals are within this angle (about one degree) are merged into one face.
const COPLANAR_COS: f32 = 0.999_85;
/// Segments an [`Handle::Arc`] is drawn and hit-tested with.
const ARC_SEGMENTS: usize = 48;

/// A custom handle. Positions are world space; `dir` of an [`Handle::Arrow`] is the arrow
/// (its length is the drawn length, its direction the drag axis).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Handle {
    /// A grab point, dragged in the plane through it facing the camera.
    Point {
        /// World position.
        pos: Vec3,
    },
    /// An arrow from `origin` along `dir`, dragged along that axis only.
    Arrow {
        /// Tail.
        origin: Vec3,
        /// Tip minus tail; zero-length arrows are inert.
        dir: Vec3,
    },
    /// A dial: the arc of radius `radius` about `axis` through `center`, from angle `from` to
    /// `to` (radians, counter-clockwise looking down `axis`, zero at [`arc_reference`]`(axis)`).
    /// Dragging turns the dial; the angle is clamped to `from..=to`.
    Arc {
        /// Centre of the circle.
        center: Vec3,
        /// Rotation axis (normalized internally).
        axis: Vec3,
        /// Start angle, radians.
        from: f32,
        /// End angle, radians.
        to: f32,
        /// Radius, metres.
        radius: f32,
    },
    /// A square of side `size` centered on `origin` in the plane with normal `normal`, dragged
    /// within that plane. Its in-plane axes are [`arc_reference`]`(normal)` and `normal × that`.
    Plane {
        /// Centre.
        origin: Vec3,
        /// Plane normal (normalized internally).
        normal: Vec3,
        /// Side length, metres.
        size: f32,
    },
}

impl Handle {
    /// The handle's anchor: the point a drag moves (the dial's tip for an arc is its `from` end
    /// until dragged).
    #[must_use]
    pub fn anchor(&self) -> Vec3 {
        match *self {
            Self::Point { pos } => pos,
            Self::Arrow { origin, dir } => origin + dir,
            Self::Arc {
                center,
                axis,
                from,
                radius,
                ..
            } => arc_point(center, axis, radius, from),
            Self::Plane { origin, .. } => origin,
        }
    }
}

/// One unit vector perpendicular to `axis`: zero angle of an [`Handle::Arc`] and first in-plane
/// axis of a [`Handle::Plane`]. Stable: the same axis always gives the same vector.
#[must_use]
pub fn arc_reference(axis: Vec3) -> Vec3 {
    let axis = axis.normalize_or_zero();
    let helper = if axis.y.abs() < 0.9 { Vec3::Y } else { Vec3::X };
    axis.cross(helper).cross(axis).normalize_or_zero()
}

/// The point at `angle` on the circle `center`, `axis`, `radius`.
#[must_use]
pub fn arc_point(center: Vec3, axis: Vec3, radius: f32, angle: f32) -> Vec3 {
    let axis = axis.normalize_or_zero();
    let a = arc_reference(axis);
    let b = axis.cross(a);
    center + (a * angle.cos() + b * angle.sin()) * radius
}

/// Ray parameter `t ≥ 0` where the ray meets the plane through `point` with normal `normal`;
/// `None` when parallel or the plane is behind the origin.
#[must_use]
pub fn ray_plane(origin: Vec3, dir: Vec3, point: Vec3, normal: Vec3) -> Option<f32> {
    let denom = dir.dot(normal);
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = (point - origin).dot(normal) / denom;
    (t >= 0.0 && t.is_finite()).then_some(t)
}

/// Parameter `s` (metres from `point` along the unit `axis`) of the point of the line closest to
/// the ray; `None` when the ray and the line are (nearly) parallel or the closest point on the
/// ray is behind its origin.
#[must_use]
pub fn ray_line_param(origin: Vec3, dir: Vec3, point: Vec3, axis: Vec3) -> Option<f32> {
    let w = origin - point;
    let b = dir.dot(axis);
    let denom = 1.0 - b * b; // both unit
    if denom < 1e-3 {
        return None;
    }
    let (d, e) = (dir.dot(w), axis.dot(w));
    // Ray parameter of the closest point: behind the origin means the axis is behind the eye.
    if (b * e - d) / denom < 0.0 {
        return None;
    }
    let s = (e - b * d) / denom;
    s.is_finite().then_some(s)
}

/// The angle of `point` about the circle `center`/`axis`, radians in `(-π, π]`, zero at
/// [`arc_reference`].
#[must_use]
pub fn angle_about(center: Vec3, axis: Vec3, point: Vec3) -> f32 {
    let axis = axis.normalize_or_zero();
    let a = arc_reference(axis);
    let b = axis.cross(a);
    let v = point - center;
    v.dot(b).atan2(v.dot(a))
}

/// The angle (radians, unclamped) where the ray meets the dial's plane; `None` when parallel.
#[must_use]
pub fn ray_arc_angle(origin: Vec3, dir: Vec3, center: Vec3, axis: Vec3) -> Option<f32> {
    let axis = axis.normalize_or_zero();
    let t = ray_plane(origin, dir, center, axis)?;
    Some(angle_about(center, axis, origin + dir * t))
}

/// Distance in pixels from `p` to the segment `a`–`b`.
#[must_use]
pub fn segment_distance(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 < 1e-12 {
        0.0
    } else {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    };
    (p - (a + ab * t)).length()
}

/// Wraps `angle` to within half a turn of `reference` (so a dial turned across ±π stays
/// continuous).
fn unwrap_near(angle: f32, reference: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let mut a = angle;
    while a - reference > std::f32::consts::PI {
        a -= tau;
    }
    while a - reference < -std::f32::consts::PI {
        a += tau;
    }
    a
}

// ---------------------------------------------------------------------------------------------
// Snapping
// ---------------------------------------------------------------------------------------------

/// What a snap point sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapKind {
    /// Centroid of a flat face (coplanar triangles merged).
    FaceCenter,
    /// Midpoint of a feature edge (boundary or crease).
    EdgeMidpoint,
    /// Center of a mesh's bounding box (and of the whole subtree's).
    BoundsCenter,
    /// Contact face of a [`SnapConnector`].
    Connector,
}

impl SnapKind {
    /// Stable name (`face_center`, `edge_midpoint`, `bounds_center`, `connector`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FaceCenter => "face_center",
            Self::EdgeMidpoint => "edge_midpoint",
            Self::BoundsCenter => "bounds_center",
            Self::Connector => "connector",
        }
    }

    /// Tie-break when two points are equally near: connectors first.
    fn rank(self) -> u8 {
        match self {
            Self::Connector => 0,
            Self::FaceCenter => 1,
            Self::EdgeMidpoint => 2,
            Self::BoundsCenter => 3,
        }
    }
}

/// A set of [`SnapKind`]s (flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapKinds(u8);

impl SnapKinds {
    /// No kinds.
    pub const NONE: Self = Self(0);
    /// [`SnapKind::FaceCenter`].
    pub const FACE_CENTERS: Self = Self(1);
    /// [`SnapKind::EdgeMidpoint`].
    pub const EDGE_MIDPOINTS: Self = Self(2);
    /// [`SnapKind::BoundsCenter`].
    pub const BOUNDS_CENTERS: Self = Self(4);
    /// [`SnapKind::Connector`].
    pub const CONNECTORS: Self = Self(8);
    /// Every kind.
    pub const ALL: Self = Self(15);

    /// The flag of one kind.
    #[must_use]
    pub fn of(kind: SnapKind) -> Self {
        match kind {
            SnapKind::FaceCenter => Self::FACE_CENTERS,
            SnapKind::EdgeMidpoint => Self::EDGE_MIDPOINTS,
            SnapKind::BoundsCenter => Self::BOUNDS_CENTERS,
            SnapKind::Connector => Self::CONNECTORS,
        }
    }

    /// Whether every flag of `other` is set.
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no kind is set.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Parses kind names (plus `all`); `Err` carries the unknown name.
    ///
    /// # Errors
    /// The first name that is not a [`SnapKind::name`] or `all`.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut kinds = Self::NONE;
        for name in names {
            kinds = kinds
                | match name {
                    "face_center" => Self::FACE_CENTERS,
                    "edge_midpoint" => Self::EDGE_MIDPOINTS,
                    "bounds_center" => Self::BOUNDS_CENTERS,
                    "connector" => Self::CONNECTORS,
                    "all" => Self::ALL,
                    other => return Err(other.to_string()),
                };
        }
        Ok(kinds)
    }
}

impl std::ops::BitOr for SnapKinds {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A connector contact face on an entity, in that entity's local space: a snap point at `center`
/// whose outward direction is `normal`. Attach it to a part (a hand, a socket, a plug); the
/// snap point follows the entity's `GlobalTransform`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct SnapConnector {
    /// Center of the contact face, local space.
    pub center: Vec3,
    /// Outward normal of the contact face, local space.
    pub normal: Vec3,
}

/// One snappable world-space point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapPoint {
    /// World position.
    pub position: Vec3,
    /// What it sits on.
    pub kind: SnapKind,
    /// The entity the feature belongs to (the mesh or connector entity).
    pub entity: Entity,
    /// Outward unit normal of the face or connector (`None` for edges and bounds centers).
    pub normal: Option<Vec3>,
}

/// The snap points of an entity subtree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapSet {
    /// Points, grouped by entity then kind.
    pub points: Vec<SnapPoint>,
    /// The set hit [`DEFAULT_MAX_POINTS`] (or the caller's limit) and dropped the rest.
    pub truncated: bool,
}

impl SnapSet {
    /// An empty set (nothing snaps).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The point nearest the pixel `pointer` of a `size` pane, within `radius_px` (points behind
    /// the eye are skipped; occlusion is not considered). Ties go to connectors, faces, edges,
    /// bounds in that order.
    #[must_use]
    pub fn nearest(
        &self,
        view: &ViewCamera,
        size: Vec2,
        pointer: Vec2,
        radius_px: f32,
    ) -> Option<&SnapPoint> {
        let mut best: Option<(&SnapPoint, f32)> = None;
        for point in &self.points {
            let Some(pixel) = view.project(point.position, size) else {
                continue;
            };
            let distance = (pixel - pointer).length();
            if distance > radius_px {
                continue;
            }
            let better = best.is_none_or(|(held, d)| {
                distance < d - 1e-3 || ((distance - d).abs() <= 1e-3 && point.kind.rank() < held.kind.rank())
            });
            if better {
                best = Some((point, distance));
            }
        }
        best.map(|(p, _)| p)
    }
}

/// World-space flat faces (center, unit normal) and feature-edge midpoints of an indexed
/// triangle mesh given as world-space `positions`. Vertices closer than 1e-4 m are welded;
/// triangles with an out-of-range index or no area are skipped. Faces are patches of triangles
/// that share an edge and whose normals agree within about a degree; edges inside a patch are
/// not reported.
#[must_use]
pub fn mesh_features(positions: &[Vec3], indices: &[u32]) -> (Vec<(Vec3, Vec3)>, Vec<Vec3>) {
    let key = |p: Vec3| {
        let q = |v: f32| (v * 1.0e4).round() as i64;
        (q(p.x), q(p.y), q(p.z))
    };
    let mut weld: HashMap<(i64, i64, i64), u32> = HashMap::new();
    let mut representative: Vec<Vec3> = Vec::new();
    let ids: Vec<u32> = positions
        .iter()
        .map(|p| {
            *weld.entry(key(*p)).or_insert_with(|| {
                representative.push(*p);
                (representative.len() - 1) as u32
            })
        })
        .collect();

    struct Tri {
        centroid: Vec3,
        normal: Vec3,
        area: f32,
        edges: [(u32, u32); 3],
    }
    let mut tris: Vec<Tri> = Vec::new();
    for chunk in indices.as_chunks::<3>().0 {
        let [a, b, c] = [chunk[0] as usize, chunk[1] as usize, chunk[2] as usize];
        let (Some(&pa), Some(&pb), Some(&pc)) = (positions.get(a), positions.get(b), positions.get(c)) else {
            continue;
        };
        let cross = (pb - pa).cross(pc - pa);
        let area = cross.length() * 0.5;
        if area < 1e-10 || !area.is_finite() {
            continue;
        }
        let e = |i: usize, j: usize| {
            let (x, y) = (ids[i], ids[j]);
            (x.min(y), x.max(y))
        };
        tris.push(Tri {
            centroid: (pa + pb + pc) / 3.0,
            normal: cross / (area * 2.0),
            area,
            edges: [e(a, b), e(b, c), e(c, a)],
        });
    }

    // Union triangles that share an edge across a flat join.
    let mut parent: Vec<usize> = (0..tris.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut edge_tris: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (i, tri) in tris.iter().enumerate() {
        for edge in tri.edges {
            edge_tris.entry(edge).or_default().push(i);
        }
    }
    for owners in edge_tris.values() {
        if let [a, b] = owners[..]
            && tris[a].normal.dot(tris[b].normal) >= COPLANAR_COS
        {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            // The lower index always becomes the root, so a patch's root (and so the
            // face order) does not depend on the hash map's iteration order.
            parent[ra.max(rb)] = ra.min(rb);
        }
    }

    // Area-weighted patch centroids and normals.
    let mut patches: HashMap<usize, (Vec3, Vec3, f32)> = HashMap::new();
    for (i, tri) in tris.iter().enumerate() {
        let root = find(&mut parent, i);
        let entry = patches.entry(root).or_insert((Vec3::ZERO, Vec3::ZERO, 0.0));
        entry.0 += tri.centroid * tri.area;
        entry.1 += tri.normal * tri.area;
        entry.2 += tri.area;
    }
    let mut roots: Vec<usize> = patches.keys().copied().collect();
    roots.sort_unstable();
    let faces = roots
        .into_iter()
        .map(|r| {
            let (c, n, a) = patches[&r];
            (c / a, n.normalize_or_zero())
        })
        .collect();

    // Feature edges: not shared by exactly two triangles of one patch.
    let mut edges: Vec<((u32, u32), Vec3)> = Vec::new();
    for (edge, owners) in &edge_tris {
        if owners.len() == 2 && find(&mut parent, owners[0]) == find(&mut parent, owners[1]) {
            continue;
        }
        let midpoint = (representative[edge.0 as usize] + representative[edge.1 as usize]) * 0.5;
        edges.push((*edge, midpoint));
    }
    edges.sort_by_key(|(edge, _)| *edge);
    (faces, edges.into_iter().map(|(_, m)| m).collect())
}

/// World position of one mesh vertex: skinned through `palette` when it has joints, then `model`.
pub(crate) fn vertex_world_position(
    v: &regia_render::LabVertex,
    model: Mat4,
    palette: &[JointMatrix],
) -> Vec3 {
    let local = if palette.is_empty() {
        v.position
    } else {
        let joint = |lane: usize| {
            palette
                .get(v.joints[lane] as usize)
                .copied()
                .unwrap_or(JointMatrix::identity())
        };
        let joints = [joint(0), joint(1), joint(2), joint(3)];
        regia_render::skin_position(v.position, &joints, v.weights)
    };
    model.transform_point3(Vec3::from_array(local))
}

fn mesh_world_positions(renderer: &MeshRenderer, model: Mat4, palette: &[JointMatrix]) -> Vec<Vec3> {
    renderer
        .mesh
        .vertices
        .iter()
        .map(|v| vertex_world_position(v, model, palette))
        .collect()
}

fn subtree(world: &World, root: Entity) -> Vec<Entity> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        if let Some(children) = world.get::<Children>(out[i]) {
            out.extend(children.iter());
        }
        i += 1;
    }
    out
}

/// Collects the snap points of `entity` and its descendants for the requested `kinds`
/// (see [`snap_targets_limited`] for the cap).
///
/// Rigid meshes use their `GlobalTransform`; skinned meshes use their current joint palette,
/// like picking does. Meshes are read from [`MeshRenderer`]; connectors from [`SnapConnector`].
#[must_use]
pub fn snap_targets(world: &World, entity: Entity, kinds: SnapKinds) -> SnapSet {
    snap_targets_limited(world, entity, kinds, DEFAULT_MAX_POINTS)
}

/// [`snap_targets`] keeping at most `max_points` points (the rest are dropped and
/// [`SnapSet::truncated`] is set).
#[must_use]
pub fn snap_targets_limited(world: &World, entity: Entity, kinds: SnapKinds, max_points: usize) -> SnapSet {
    let _span = tracing::info_span!(target: "regia_editor", "snap_targets").entered();
    let mut set = SnapSet::empty();
    let push = |set: &mut SnapSet, point: SnapPoint| {
        if set.points.len() >= max_points {
            set.truncated = true;
        } else {
            set.points.push(point);
        }
    };
    let (mut lo, mut hi, mut meshes) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN), 0_u32);
    for member in subtree(world, entity) {
        let model = world
            .get::<GlobalTransform>(member)
            .map_or(Mat4::IDENTITY, |g| Mat4::from(g.0));
        if let Some(renderer) = world.get::<MeshRenderer>(member) {
            let palette = world
                .get::<SkinJoints>(member)
                .map_or(&[][..], |s| s.palette.as_slice());
            let positions = mesh_world_positions(renderer, model, palette);
            let (mut mlo, mut mhi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for p in positions.iter().filter(|p| p.is_finite()) {
                mlo = mlo.min(*p);
                mhi = mhi.max(*p);
            }
            if mlo.x <= mhi.x {
                meshes += 1;
                lo = lo.min(mlo);
                hi = hi.max(mhi);
                if kinds.contains(SnapKinds::BOUNDS_CENTERS) {
                    // The center of the oriented box (object-space bounds through the model).
                    let (olo, ohi) = renderer.mesh.vertices.iter().fold(
                        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                        |(l, h), v| {
                            let p = Vec3::from_array(v.position);
                            (l.min(p), h.max(p))
                        },
                    );
                    let center = if palette.is_empty() {
                        model.transform_point3((olo + ohi) * 0.5)
                    } else {
                        (mlo + mhi) * 0.5
                    };
                    push(
                        &mut set,
                        SnapPoint {
                            position: center,
                            kind: SnapKind::BoundsCenter,
                            entity: member,
                            normal: None,
                        },
                    );
                }
            }
            if kinds.contains(SnapKinds::FACE_CENTERS) || kinds.contains(SnapKinds::EDGE_MIDPOINTS) {
                let (faces, edges) = mesh_features(&positions, &renderer.mesh.indices);
                if kinds.contains(SnapKinds::FACE_CENTERS) {
                    for (position, normal) in faces {
                        push(
                            &mut set,
                            SnapPoint {
                                position,
                                kind: SnapKind::FaceCenter,
                                entity: member,
                                normal: Some(normal),
                            },
                        );
                    }
                }
                if kinds.contains(SnapKinds::EDGE_MIDPOINTS) {
                    for position in edges {
                        push(
                            &mut set,
                            SnapPoint {
                                position,
                                kind: SnapKind::EdgeMidpoint,
                                entity: member,
                                normal: None,
                            },
                        );
                    }
                }
            }
        }
        if kinds.contains(SnapKinds::CONNECTORS)
            && let Some(connector) = world.get::<SnapConnector>(member)
        {
            push(
                &mut set,
                SnapPoint {
                    position: model.transform_point3(connector.center),
                    kind: SnapKind::Connector,
                    entity: member,
                    normal: Some(model.transform_vector3(connector.normal).normalize_or_zero()),
                },
            );
        }
    }
    if kinds.contains(SnapKinds::BOUNDS_CENTERS) && meshes > 1 {
        push(
            &mut set,
            SnapPoint {
                position: (lo + hi) * 0.5,
                kind: SnapKind::BoundsCenter,
                entity,
                normal: None,
            },
        );
    }
    set
}

// ---------------------------------------------------------------------------------------------
// Interaction
// ---------------------------------------------------------------------------------------------

/// The pointer for one update, in pixels of the handle rect (origin at its top-left corner).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PointerInput {
    /// Pointer position; `None` when it is outside the rect.
    pub pos: Option<Vec2>,
    /// The primary button went down this update.
    pub pressed: bool,
    /// The primary button is held.
    pub down: bool,
}

/// What a [`HandleDrag`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    /// The pointer is over the handle; no button.
    Hover,
    /// The button went down on the handle.
    Begin,
    /// The handle is being dragged.
    Drag,
    /// The button went up; `total` is the final displacement.
    End,
}

/// The result of one [`handles`] / [`HandleState::update`] call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HandleDrag {
    /// Index into the slice of handles.
    pub handle: usize,
    /// The phase.
    pub phase: DragPhase,
    /// World-space movement since the previous update (zero on `Hover` and `Begin`).
    pub delta: Vec3,
    /// World-space movement since the drag began.
    pub total: Vec3,
    /// Where the handle's anchor now is (its anchor plus `total`; for an arc, the dial point).
    pub position: Vec3,
    /// Arc handles: the dial angle, radians, clamped to the arc (`None` for other handles).
    pub angle: Option<f32>,
    /// Arc handles: angle movement since the previous update.
    pub angle_delta: f32,
    /// The snap point the drag is locked to, if one was within the pick radius.
    pub snapped: Option<SnapPoint>,
}

#[derive(Debug, Clone, Copy)]
struct Active {
    handle: usize,
    start: Vec3,
    last: Vec3,
    anchor: Vec3,
    angle: f32,
    start_angle: f32,
    /// An arc handle (its drags report the dial angle).
    arc: bool,
}

/// Hover and drag state of one [`handles`] widget (kept in egui memory under its id).
#[derive(Debug, Clone, Copy, Default)]
pub struct HandleState {
    active: Option<Active>,
    hovered: Option<usize>,
}

impl HandleState {
    /// The handle under the pointer (or being dragged).
    #[must_use]
    pub fn hovered(&self) -> Option<usize> {
        self.active.map(|a| a.handle).or(self.hovered)
    }

    /// Whether a drag is in progress.
    #[must_use]
    pub fn dragging(&self) -> bool {
        self.active.is_some()
    }

    /// Advances the state by one pointer sample and returns the event, if any: `Hover` while the
    /// pointer rests on a handle, `Begin`/`Drag`/`End` for a drag. `size` is the pane size in
    /// pixels (the projection's), `radius_px` the pick and snap radius.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        view: &ViewCamera,
        size: Vec2,
        input: PointerInput,
        handles: &[Handle],
        snap: &SnapSet,
        radius_px: f32,
    ) -> Option<HandleDrag> {
        if let Some(active) = self.active {
            let handle = handles.get(active.handle).copied();
            let Some(handle) = handle else {
                // The handle vanished mid-drag: end it, so a tool that commits on `End` finishes,
                // with the last dial angle an arc reached (not `None`, which reads as no turn).
                self.active = None;
                return Some(HandleDrag {
                    handle: active.handle,
                    phase: DragPhase::End,
                    delta: Vec3::ZERO,
                    total: active.last - active.start,
                    position: if active.arc {
                        active.last
                    } else {
                        active.anchor + (active.last - active.start)
                    },
                    angle: active.arc.then_some(active.angle),
                    angle_delta: 0.0,
                    snapped: None,
                });
            };
            let sample = input
                .pos
                .and_then(|pos| drag_target(&handle, view, size, pos, snap, radius_px, Some(active.angle)));
            let (position, angle, snapped) = sample.unwrap_or((active.last, Some(active.angle), None));
            let phase = if input.down {
                DragPhase::Drag
            } else {
                DragPhase::End
            };
            let drag = Self::report(&handle, active, position, angle, snapped, phase);
            if input.down {
                let mut next = active;
                next.last = position;
                next.angle = angle.unwrap_or(active.angle);
                self.active = Some(next);
            } else {
                self.active = None;
            }
            return Some(drag);
        }
        let Some(pos) = input.pos else {
            self.hovered = None;
            return None;
        };
        self.hovered = hit_test(handles, view, size, pos, radius_px);
        let index = self.hovered?;
        let handle = handles[index];
        if input.pressed && input.down {
            let (start, angle, _) = drag_target(&handle, view, size, pos, &SnapSet::empty(), radius_px, None)
                .unwrap_or((handle.anchor(), None, None));
            let angle = angle.unwrap_or(0.0);
            // A point is grabbed at its exact position, so a snap lands on the target.
            let start = if let Handle::Point { pos } = handle {
                pos
            } else {
                start
            };
            let active = Active {
                handle: index,
                start,
                last: start,
                anchor: handle.anchor(),
                angle,
                start_angle: angle,
                arc: matches!(handle, Handle::Arc { .. }),
            };
            self.active = Some(active);
            return Some(Self::report(
                &handle,
                active,
                start,
                matches!(handle, Handle::Arc { .. }).then_some(angle),
                None,
                DragPhase::Begin,
            ));
        }
        Some(HandleDrag {
            handle: index,
            phase: DragPhase::Hover,
            delta: Vec3::ZERO,
            total: Vec3::ZERO,
            position: handle.anchor(),
            angle: None,
            angle_delta: 0.0,
            snapped: None,
        })
    }

    fn report(
        handle: &Handle,
        active: Active,
        position: Vec3,
        angle: Option<f32>,
        snapped: Option<SnapPoint>,
        phase: DragPhase,
    ) -> HandleDrag {
        let is_arc = matches!(handle, Handle::Arc { .. });
        HandleDrag {
            handle: active.handle,
            phase,
            delta: if phase == DragPhase::Begin {
                Vec3::ZERO
            } else {
                position - active.last
            },
            total: position - active.start,
            position: if is_arc {
                position
            } else {
                active.anchor + (position - active.start)
            },
            angle: if is_arc { angle } else { None },
            angle_delta: if is_arc && phase != DragPhase::Begin {
                angle.map_or(0.0, |a| a - active.angle)
            } else {
                0.0
            },
            snapped,
        }
    }

    /// Net dial turn since the drag began (for tests and tools that show a readout).
    #[must_use]
    pub fn dial_turn(&self) -> Option<f32> {
        self.active.map(|a| a.angle - a.start_angle)
    }
}

/// Screen distance from `pos` to the handle (`None` when too far to pick).
fn handle_distance(handle: &Handle, view: &ViewCamera, size: Vec2, pos: Vec2, radius_px: f32) -> Option<f32> {
    let project = |p: Vec3| view.project(p, size);
    let distance = match *handle {
        Handle::Point { pos: p } => (project(p)? - pos).length(),
        Handle::Arrow { origin, dir } => {
            if dir.length_squared() < 1e-12 {
                return None;
            }
            segment_distance(pos, project(origin)?, project(origin + dir)?)
        }
        Handle::Arc {
            center,
            axis,
            from,
            to,
            radius,
        } => {
            let mut best = f32::MAX;
            let mut previous: Option<Vec2> = None;
            for i in 0..=ARC_SEGMENTS {
                let angle = from + (to - from) * i as f32 / ARC_SEGMENTS as f32;
                let here = project(arc_point(center, axis, radius, angle));
                if let (Some(a), Some(b)) = (previous, here) {
                    best = best.min(segment_distance(pos, a, b));
                }
                previous = here;
            }
            best
        }
        Handle::Plane {
            origin,
            normal,
            size: side,
        } => {
            let (o, d) = view.ray(pos, size);
            let normal = normal.normalize_or_zero();
            let t = ray_plane(o, d, origin, normal)?;
            let local = o + d * t - origin;
            let a = arc_reference(normal);
            let b = normal.cross(a);
            let half = side * 0.5;
            if local.dot(a).abs() > half || local.dot(b).abs() > half {
                return None;
            }
            // Lines and points win over the plane they sit on.
            radius_px * 0.9
        }
    };
    (distance <= radius_px).then_some(distance)
}

/// The index of the handle under `pos` (the nearest on screen; earlier wins ties).
#[must_use]
pub fn hit_test(
    handles: &[Handle],
    view: &ViewCamera,
    size: Vec2,
    pos: Vec2,
    radius_px: f32,
) -> Option<usize> {
    handles
        .iter()
        .enumerate()
        .filter_map(|(i, h)| handle_distance(h, view, size, pos, radius_px).map(|d| (i, d)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Where the pointer puts the dragged handle: the new anchor-space position, the arc angle, the
/// snap point used.
#[allow(clippy::type_complexity)]
fn drag_target(
    handle: &Handle,
    view: &ViewCamera,
    size: Vec2,
    pos: Vec2,
    snap: &SnapSet,
    radius_px: f32,
    previous_angle: Option<f32>,
) -> Option<(Vec3, Option<f32>, Option<SnapPoint>)> {
    let (o, d) = view.ray(pos, size);
    let snapped = snap.nearest(view, size, pos, radius_px).copied();
    match *handle {
        Handle::Point { pos: p } => {
            if let Some(s) = snapped {
                return Some((s.position, None, Some(s)));
            }
            let t = ray_plane(o, d, p, view.forward())?;
            Some((o + d * t, None, None))
        }
        Handle::Arrow { origin, dir } => {
            let axis = dir.normalize_or_zero();
            let on_axis = |target: Vec3| origin + axis * (target - origin).dot(axis);
            if let Some(s) = snapped {
                return Some((on_axis(s.position), None, Some(s)));
            }
            let s = ray_line_param(o, d, origin, axis)?;
            Some((origin + axis * s, None, None))
        }
        Handle::Plane { origin, normal, .. } => {
            let normal = normal.normalize_or_zero();
            let in_plane = |target: Vec3| target - normal * (target - origin).dot(normal);
            if let Some(s) = snapped {
                return Some((in_plane(s.position), None, Some(s)));
            }
            let t = ray_plane(o, d, origin, normal)?;
            Some((o + d * t, None, None))
        }
        Handle::Arc {
            center,
            axis,
            from,
            to,
            radius,
        } => {
            let (lo, hi) = (from.min(to), from.max(to));
            if let Some(s) = snapped {
                let raw = angle_about(center, axis, s.position);
                let angle =
                    unwrap_near(raw, previous_angle.unwrap_or_else(|| raw.clamp(lo, hi))).clamp(lo, hi);
                return Some((arc_point(center, axis, radius, angle), Some(angle), Some(s)));
            }
            let raw = ray_arc_angle(o, d, center, axis)?;
            let angle = unwrap_near(raw, previous_angle.unwrap_or_else(|| raw.clamp(lo, hi))).clamp(lo, hi);
            Some((arc_point(center, axis, radius, angle), Some(angle), None))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// egui widget
// ---------------------------------------------------------------------------------------------

/// Idle, hover and active colors of a handle.
const IDLE: egui::Color32 = egui::Color32::from_rgb(240, 200, 70);
const HOVER: egui::Color32 = egui::Color32::from_rgb(255, 255, 140);
const ACTIVE: egui::Color32 = egui::Color32::from_rgb(255, 140, 40);
const SNAP: egui::Color32 = egui::Color32::from_rgb(90, 230, 255);

/// Draws `handles` over `rect` (the viewport rect the `view` camera renders into), handles hover
/// and dragging, and returns the event for this frame: `Hover` while the pointer is over a handle,
/// then `Begin`, `Drag` and `End` with the world-space `delta` (see [`HandleDrag`]). `None` when
/// nothing is hovered or dragged. While dragging, the drag snaps to the nearest point of `snap`
/// within [`DEFAULT_PICK_RADIUS`] pixels and that point is drawn.
///
/// `id` keys the drag state, so give each handle group its own. Call it after the viewport image
/// and the built-in gizmo so a handle under the pointer takes the click: when it returns `Some`
/// the viewport should not also pick or start a gizmo drag.
pub fn handles(
    ui: &mut egui::Ui,
    view: &ViewCamera,
    rect: egui::Rect,
    id: egui::Id,
    handles: &[Handle],
    snap: &SnapSet,
) -> Option<HandleDrag> {
    let _span = tracing::info_span!(target: "regia_editor", "handles").entered();
    let mut state: HandleState = ui.ctx().data(|d| d.get_temp(id)).unwrap_or_default();
    let size = Vec2::new(rect.width(), rect.height());
    let (pointer, pressed, down) = ui.input(|i| {
        (
            i.pointer.hover_pos().or(i.pointer.latest_pos()),
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
        )
    });
    let inside = pointer.filter(|p| rect.contains(*p) || state.dragging());
    let input = PointerInput {
        pos: inside.map(|p| Vec2::new(p.x - rect.min.x, p.y - rect.min.y)),
        pressed: pressed && inside.is_some(),
        down,
    };
    let event = state.update(view, size, input, handles, snap, DEFAULT_PICK_RADIUS);
    ui.ctx().data_mut(|d| d.insert_temp(id, state));

    let painter = ui.painter_at(rect);
    let to_screen = |p: Vec3| {
        view.project(p, size)
            .map(|px| egui::pos2(rect.min.x + px.x, rect.min.y + px.y))
    };
    let focus = state.hovered();
    for (i, handle) in handles.iter().enumerate() {
        let color = if state.dragging() && focus == Some(i) {
            ACTIVE
        } else if focus == Some(i) {
            HOVER
        } else {
            IDLE
        };
        let stroke = egui::Stroke::new(if focus == Some(i) { 3.0 } else { 2.0 }, color);
        match *handle {
            Handle::Point { pos } => {
                if let Some(p) = to_screen(pos) {
                    painter.circle_filled(p, 5.0, color);
                }
            }
            Handle::Arrow { origin, dir } => {
                if let (Some(a), Some(b)) = (to_screen(origin), to_screen(origin + dir)) {
                    painter.line_segment([a, b], stroke);
                    painter.circle_filled(b, 4.5, color);
                }
            }
            Handle::Arc {
                center,
                axis,
                from,
                to,
                radius,
            } => {
                let points: Vec<egui::Pos2> = (0..=ARC_SEGMENTS)
                    .filter_map(|k| {
                        to_screen(arc_point(
                            center,
                            axis,
                            radius,
                            from + (to - from) * k as f32 / ARC_SEGMENTS as f32,
                        ))
                    })
                    .collect();
                painter.add(egui::Shape::line(points, stroke));
                if let (Some(c), Some(e)) = (
                    to_screen(center),
                    to_screen(
                        event
                            .and_then(|e| (e.handle == i).then_some(e.position))
                            .unwrap_or_else(|| handle.anchor()),
                    ),
                ) {
                    painter.line_segment([c, e], egui::Stroke::new(1.0, color));
                    painter.circle_filled(e, 4.0, color);
                }
            }
            Handle::Plane {
                origin,
                normal,
                size: side,
            } => {
                let n = normal.normalize_or_zero();
                let (a, b) = (
                    arc_reference(n) * side * 0.5,
                    n.cross(arc_reference(n)) * side * 0.5,
                );
                let corners =
                    [origin - a - b, origin + a - b, origin + a + b, origin - a + b].map(&to_screen);
                if let [Some(p0), Some(p1), Some(p2), Some(p3)] = corners {
                    painter.add(egui::Shape::convex_polygon(
                        vec![p0, p1, p2, p3],
                        color.gamma_multiply(if focus == Some(i) { 0.45 } else { 0.25 }),
                        stroke,
                    ));
                }
            }
        }
    }
    if let Some(snapped) = event.and_then(|e| e.snapped)
        && let Some(p) = to_screen(snapped.position)
    {
        painter.circle_stroke(p, 7.0, egui::Stroke::new(2.0, SNAP));
    }
    event
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use crate::views::{ViewKind, ViewportLayout};

pub(crate) fn vec3_param(value: &Value, name: &str) -> Result<Vec3, RpcError> {
    let array = value
        .get(name)
        .and_then(Value::as_array)
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, y, z]")))?;
    let mut out = [0.0_f32; 3];
    for (slot, v) in out.iter_mut().zip(array) {
        *slot = v
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be numbers")))?
            as f32;
    }
    Ok(Vec3::from_array(out))
}

fn number(value: &Value, name: &str) -> Result<f32, RpcError> {
    value
        .get(name)
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number")))
}

fn parse_handle(value: &Value) -> Result<Handle, RpcError> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    Ok(match kind {
        "point" => Handle::Point {
            pos: vec3_param(value, "pos")?,
        },
        "arrow" => Handle::Arrow {
            origin: vec3_param(value, "origin")?,
            dir: vec3_param(value, "dir")?,
        },
        "arc" => Handle::Arc {
            center: vec3_param(value, "center")?,
            axis: vec3_param(value, "axis")?,
            from: number(value, "from")?,
            to: number(value, "to")?,
            radius: number(value, "radius")?,
        },
        "plane" => Handle::Plane {
            origin: vec3_param(value, "origin")?,
            normal: vec3_param(value, "normal")?,
            size: number(value, "size")?,
        },
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown handle type `{other}` (point, arrow, arc, plane)"
            )));
        }
    })
}

pub(crate) fn pane_view(world: &World, params: &Value) -> Result<(ViewCamera, Vec2), RpcError> {
    let kind = match params.get("view").and_then(Value::as_str) {
        Some(name) => ViewKind::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!("unknown view `{name}` (perspective, top, front, side)"))
        })?,
        None => ViewKind::Perspective,
    };
    let layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let pane = layout.panes.iter().position(|p| p.kind == kind).unwrap_or(0);
    let (_, camera) = layout.pane_camera(world, pane);
    let dim = |name: &str, default: f64| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .unwrap_or(default)
            .clamp(64.0, 4096.0) as f32
    };
    Ok((
        ViewCamera::of(kind, &camera),
        Vec2::new(dim("width", 640.0), dim("height", 480.0)),
    ))
}

fn kinds_param(params: &Value) -> Result<SnapKinds, RpcError> {
    match params.get("kinds").and_then(Value::as_array) {
        None => Ok(SnapKinds::ALL),
        Some(names) => SnapKinds::from_names(names.iter().filter_map(Value::as_str)).map_err(|bad| {
            RpcError::invalid_params(format!(
                "unknown snap kind `{bad}` (face_center, edge_midpoint, bounds_center, connector, all)"
            ))
        }),
    }
}

fn snap_point_json(p: &SnapPoint) -> Value {
    json!({
        "kind": p.kind.name(),
        "position": p.position.to_array(),
        "entity": p.entity.to_bits(),
        "normal": p.normal.map(|n| n.to_array()),
    })
}

fn drag_json(d: &HandleDrag) -> Value {
    json!({
        "handle": d.handle,
        "phase": match d.phase {
            DragPhase::Hover => "hover",
            DragPhase::Begin => "begin",
            DragPhase::Drag => "drag",
            DragPhase::End => "end",
        },
        "delta": d.delta.to_array(),
        "total": d.total.to_array(),
        "position": d.position.to_array(),
        "angle": d.angle,
        "angle_delta": d.angle_delta,
        "snapped": d.snapped.as_ref().map(snap_point_json),
    })
}

fn entity_arg(params: &Value, name: &str) -> Result<Entity, RpcError> {
    let bits = params
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (entity id) is required")))?;
    crate::command::entity_from_bits(bits)
        .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

/// `editor.snap_targets`.
fn snap_targets_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_arg(params, "entity")?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    let kinds = kinds_param(params)?;
    let limit = params
        .get("max_points")
        .and_then(Value::as_u64)
        .map_or(1024, |v| (v as usize).min(DEFAULT_MAX_POINTS));
    let set = snap_targets_limited(world, entity, kinds, limit);
    let mut out = json!({
        "entity": entity.to_bits(),
        "count": set.points.len(),
        "truncated": set.truncated,
        "points": set.points.iter().map(snap_point_json).collect::<Vec<_>>(),
    });
    if let (Some(x), Some(y)) = (
        params.get("x").and_then(Value::as_f64),
        params.get("y").and_then(Value::as_f64),
    ) {
        let (view, size) = pane_view(world, params)?;
        let radius = params
            .get("radius")
            .and_then(Value::as_f64)
            .map_or(DEFAULT_PICK_RADIUS, |r| r as f32);
        out["nearest"] = set
            .nearest(&view, size, Vec2::new(x as f32, y as f32), radius)
            .map_or(Value::Null, snap_point_json);
    }
    Ok(out)
}

/// `editor.handle_drag`: replays a synthetic drag through the handle state machine.
fn handle_drag_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let handles: Vec<Handle> = params
        .get("handles")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`handles` (array) is required"))?
        .iter()
        .map(parse_handle)
        .collect::<Result<_, _>>()?;
    let pixel = |name: &str| -> Result<Vec2, RpcError> {
        let a = params
            .get(name)
            .and_then(Value::as_array)
            .filter(|a| a.len() == 2)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, y] pixels")))?;
        let n = |i: usize| {
            a[i].as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be numbers")))
        };
        Ok(Vec2::new(n(0)? as f32, n(1)? as f32))
    };
    let (start, end) = (pixel("from")?, pixel("to")?);
    let (view, size) = pane_view(world, params)?;
    let radius = params
        .get("radius")
        .and_then(Value::as_f64)
        .map_or(DEFAULT_PICK_RADIUS, |r| r as f32);
    let snap = match params.get("snap_entity") {
        None | Some(Value::Null) => SnapSet::empty(),
        Some(_) => snap_targets(world, entity_arg(params, "snap_entity")?, kinds_param(params)?),
    };
    let steps = params
        .get("steps")
        .and_then(Value::as_u64)
        .map_or(8, |s| s.clamp(1, 240)) as u32;
    let mut state = HandleState::default();
    let mut events = Vec::new();
    let run = |state: &mut HandleState, pos: Vec2, pressed: bool, down: bool| {
        let input = PointerInput {
            pos: Some(pos),
            pressed,
            down,
        };
        state.update(&view, size, input, &handles, &snap, radius)
    };
    let hover = run(&mut state, start, false, false);
    let begin = run(&mut state, start, true, true);
    if let Some(e) = begin.filter(|e| e.phase == DragPhase::Begin) {
        events.push(drag_json(&e));
        for k in 1..=steps {
            let pos = start.lerp(end, k as f32 / steps as f32);
            if let Some(e) = run(&mut state, pos, false, true) {
                events.push(drag_json(&e));
            }
        }
        if let Some(e) = run(&mut state, end, false, false) {
            events.push(drag_json(&e));
        }
    }
    let last = events.last().cloned().unwrap_or(Value::Null);
    Ok(json!({
        "hovered": hover.map(|h| h.handle),
        "grabbed": !events.is_empty(),
        "result": last,
        "events": events,
    }))
}

/// The F6 agent methods (`editor.snap_targets`, `editor.handle_drag`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    const P_VIEW: ParamSchema = ParamSchema {
        name: "view",
        kind: "string",
        required: false,
        description: "Pane camera: perspective (default, the editor camera), top, front or side.",
    };
    vec![
        (
            MethodSchema {
                name: "editor.snap_targets",
                tool: "snap_targets",
                description: "The snap points of an entity and its descendants: face centers (coplanar \
                    triangles merged), feature-edge midpoints, bounding-box centers and SnapConnector \
                    contact faces. With x and y (pixels of the width×height pane) also returns the \
                    nearest point within radius pixels.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity id of the subtree root.",
                    },
                    ParamSchema {
                        name: "kinds",
                        kind: "array",
                        required: false,
                        description: "Any of face_center, edge_midpoint, bounds_center, connector, all. Default all.",
                    },
                    ParamSchema {
                        name: "max_points",
                        kind: "number",
                        required: false,
                        description: "Points returned at most (default 1024, cap 8192); truncated says if cut.",
                    },
                    ParamSchema {
                        name: "x",
                        kind: "number",
                        required: false,
                        description: "Pointer pixel x: with y, also return the nearest point.",
                    },
                    ParamSchema {
                        name: "y",
                        kind: "number",
                        required: false,
                        description: "Pointer pixel y.",
                    },
                    ParamSchema {
                        name: "radius",
                        kind: "number",
                        required: false,
                        description: "Snap radius in pixels. Default 10.",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "Pane width in pixels for x/y. Default 640.",
                    },
                    ParamSchema {
                        name: "height",
                        kind: "number",
                        required: false,
                        description: "Pane height in pixels for x/y. Default 480.",
                    },
                    P_VIEW,
                ],
                result: "{ entity, count, truncated, points: [{ kind, position, entity, normal }], nearest? }",
                mutating: false,
                permission: Permission::Read,
            },
            snap_targets_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.handle_drag",
                tool: "handle_drag",
                description: "Replay a synthetic pointer drag through the custom-handle state machine \
                    (editor::handles) without a UI: press at `from` on a handle, move to `to`, release. \
                    Handles are {type: point, pos} | {type: arrow, origin, dir} | {type: arc, center, \
                    axis, from, to, radius} | {type: plane, origin, normal, size}, in world space. \
                    Returns the hovered handle, every begin/drag/end event (world delta, total, \
                    position, arc angle, snapped point) and the final result. Mutates nothing.",
                params: &[
                    ParamSchema {
                        name: "handles",
                        kind: "array",
                        required: true,
                        description: "Handle descriptions (see above).",
                    },
                    ParamSchema {
                        name: "from",
                        kind: "array",
                        required: true,
                        description: "[x, y] pixel where the button goes down.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "array",
                        required: true,
                        description: "[x, y] pixel where it is released.",
                    },
                    ParamSchema {
                        name: "steps",
                        kind: "number",
                        required: false,
                        description: "Interpolated pointer moves between from and to. Default 8.",
                    },
                    ParamSchema {
                        name: "snap_entity",
                        kind: "number",
                        required: false,
                        description: "Snap to the points of this entity's subtree while dragging.",
                    },
                    ParamSchema {
                        name: "kinds",
                        kind: "array",
                        required: false,
                        description: "Snap kinds (as editor.snap_targets). Default all.",
                    },
                    ParamSchema {
                        name: "radius",
                        kind: "number",
                        required: false,
                        description: "Pick and snap radius in pixels. Default 10.",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "Pane width in pixels. Default 640.",
                    },
                    ParamSchema {
                        name: "height",
                        kind: "number",
                        required: false,
                        description: "Pane height in pixels. Default 480.",
                    },
                    P_VIEW,
                ],
                result: "{ hovered, grabbed, result, events: [{ handle, phase, delta, total, position, angle, angle_delta, snapped }] }",
                mutating: false,
                permission: Permission::Read,
            },
            handle_drag_method as crate::viewport_agent::Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> ViewCamera {
        ViewCamera {
            eye: Vec3::new(0.0, 0.0, 10.0),
            target: Vec3::ZERO,
            fov_y: 0.8,
            near: 0.1,
            far: 100.0,
            ortho: false,
        }
    }

    const SIZE: Vec2 = Vec2::new(800.0, 600.0);

    fn px(p: Vec3) -> Vec2 {
        camera().project(p, SIZE).expect("in front of the eye")
    }

    /// Replays press at `a`, move to `b` and release; returns the final events.
    fn drag(handles: &[Handle], snap: &SnapSet, a: Vec2, b: Vec2) -> Vec<HandleDrag> {
        let mut state = HandleState::default();
        let view = camera();
        let mut out = Vec::new();
        let mut step = |input: PointerInput| {
            if let Some(e) = state.update(&view, SIZE, input, handles, snap, DEFAULT_PICK_RADIUS) {
                out.push(e);
            }
        };
        step(PointerInput {
            pos: Some(a),
            pressed: false,
            down: false,
        });
        step(PointerInput {
            pos: Some(a),
            pressed: true,
            down: true,
        });
        for k in 1..=4 {
            step(PointerInput {
                pos: Some(a.lerp(b, k as f32 / 4.0)),
                pressed: false,
                down: true,
            });
        }
        step(PointerInput {
            pos: Some(b),
            pressed: false,
            down: false,
        });
        out
    }

    fn close(a: Vec3, b: Vec3, eps: f32) -> bool {
        (a - b).length() < eps
    }

    #[test]
    fn ray_plane_and_line_math() {
        let t = ray_plane(Vec3::new(0.0, 0.0, 5.0), Vec3::NEG_Z, Vec3::ZERO, Vec3::Z).expect("hit");
        assert!((t - 5.0).abs() < 1e-5);
        assert!(
            ray_plane(Vec3::new(0.0, 0.0, 5.0), Vec3::X, Vec3::ZERO, Vec3::Z).is_none(),
            "parallel"
        );
        assert!(
            ray_plane(Vec3::new(0.0, 0.0, -5.0), Vec3::NEG_Z, Vec3::ZERO, Vec3::Z).is_none(),
            "behind"
        );
        // A ray along -Z at x=2 is closest to the X axis line at s=2.
        let s = ray_line_param(Vec3::new(2.0, 1.0, 5.0), Vec3::NEG_Z, Vec3::ZERO, Vec3::X).expect("skew");
        assert!((s - 2.0).abs() < 1e-5);
        assert!(ray_line_param(Vec3::new(0.0, 1.0, 0.0), Vec3::X, Vec3::ZERO, Vec3::X).is_none());
    }

    #[test]
    fn arc_angles_round_trip() {
        let (center, axis) = (Vec3::new(1.0, 2.0, 0.0), Vec3::Z);
        for angle in [-2.5_f32, -0.7, 0.0, 0.4, 1.9, 3.0] {
            let p = arc_point(center, axis, 2.0, angle);
            assert!((angle_about(center, axis, p) - angle).abs() < 1e-4, "{angle}");
            let hit = ray_arc_angle(p + Vec3::Z * 4.0, Vec3::NEG_Z, center, axis).expect("plane hit");
            assert!((hit - angle).abs() < 1e-4);
        }
        let reference = arc_reference(Vec3::new(0.0, 1.0, 0.0));
        assert!(reference.dot(Vec3::Y).abs() < 1e-6 && (reference.length() - 1.0).abs() < 1e-6);
        assert!((segment_distance(Vec2::new(5.0, 3.0), Vec2::ZERO, Vec2::new(10.0, 0.0)) - 3.0).abs() < 1e-6);
        assert!(
            (segment_distance(Vec2::new(-4.0, 3.0), Vec2::ZERO, Vec2::new(10.0, 0.0)) - 5.0).abs() < 1e-6
        );
    }

    #[test]
    fn point_handle_drags_in_the_camera_plane() {
        let handles = [Handle::Point {
            pos: Vec3::new(1.0, 0.0, 0.0),
        }];
        let events = drag(
            &handles,
            &SnapSet::empty(),
            px(Vec3::new(1.0, 0.0, 0.0)),
            px(Vec3::new(2.0, 1.0, 0.0)),
        );
        assert_eq!(events[0].phase, DragPhase::Hover);
        assert_eq!(events[1].phase, DragPhase::Begin);
        let end = events.last().expect("end");
        assert_eq!(end.phase, DragPhase::End);
        assert!(
            close(end.total, Vec3::new(1.0, 1.0, 0.0), 1e-3),
            "{:?}",
            end.total
        );
        assert!(close(end.position, Vec3::new(2.0, 1.0, 0.0), 1e-3));
        let summed: Vec3 = events.iter().map(|e| e.delta).sum();
        assert!(close(summed, end.total, 1e-4), "deltas add up to the total");
    }

    #[test]
    fn arrow_handle_moves_only_along_its_axis() {
        let handles = [Handle::Arrow {
            origin: Vec3::new(-1.0, 0.0, 0.0),
            dir: Vec3::new(2.0, 0.0, 0.0),
        }];
        // Grab the shaft midpoint, drag diagonally: only the X part counts.
        let events = drag(
            &handles,
            &SnapSet::empty(),
            px(Vec3::ZERO),
            px(Vec3::new(1.5, 2.0, 0.0)),
        );
        let end = events.last().expect("end");
        assert!(
            end.total.y.abs() < 1e-4 && end.total.z.abs() < 1e-4,
            "{:?}",
            end.total
        );
        // Perspective: the pointer ray's closest axis point is near, not exactly, the pointed x.
        assert!((end.total.x - 1.5).abs() < 0.1, "{:?}", end.total);
        // A zero-length arrow is inert.
        let inert = [Handle::Arrow {
            origin: Vec3::ZERO,
            dir: Vec3::ZERO,
        }];
        assert!(drag(&inert, &SnapSet::empty(), px(Vec3::ZERO), px(Vec3::X)).is_empty());
    }

    #[test]
    fn plane_handle_stays_in_its_plane() {
        let handles = [Handle::Plane {
            origin: Vec3::ZERO,
            normal: Vec3::Z,
            size: 2.0,
        }];
        let events = drag(
            &handles,
            &SnapSet::empty(),
            px(Vec3::new(0.2, 0.2, 0.0)),
            px(Vec3::new(1.2, -0.8, 0.0)),
        );
        let end = events.last().expect("end");
        assert!(
            close(end.total, Vec3::new(1.0, -1.0, 0.0), 1e-3),
            "{:?}",
            end.total
        );
        // Outside the square nothing is grabbed.
        assert!(
            drag(
                &handles,
                &SnapSet::empty(),
                px(Vec3::new(3.0, 0.0, 0.0)),
                px(Vec3::new(4.0, 0.0, 0.0))
            )
            .is_empty()
        );
    }

    #[test]
    fn arc_handle_turns_a_clamped_dial() {
        let arc = Handle::Arc {
            center: Vec3::ZERO,
            axis: Vec3::Z,
            from: 0.0,
            to: 1.5,
            radius: 2.0,
        };
        let start = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 0.2);
        let to = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 1.0);
        let events = drag(&[arc], &SnapSet::empty(), px(start), px(to));
        let end = events.last().expect("end");
        assert!((end.angle.expect("angle") - 1.0).abs() < 2e-2, "{:?}", end.angle);
        let turned: f32 = events.iter().map(|e| e.angle_delta).sum();
        assert!((turned - 0.8).abs() < 3e-2, "{turned}");
        // Dragging past the end of the arc clamps to the limit.
        let beyond = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 2.4);
        let events = drag(&[arc], &SnapSet::empty(), px(start), px(beyond));
        assert!((events.last().expect("end").angle.expect("angle") - 1.5).abs() < 1e-3);
        // Away from the circle nothing is grabbed.
        assert!(
            drag(&[arc], &SnapSet::empty(), px(Vec3::ZERO), px(to))
                .iter()
                .all(|e| e.phase == DragPhase::Hover)
        );
    }

    #[test]
    fn an_arrow_drag_rejects_a_near_parallel_or_behind_ray() {
        // About 1 degree off the axis: the closest point would be hundreds of metres away.
        let dir = Vec3::new(0.0, 0.0175, -1.0).normalize();
        assert!(ray_line_param(Vec3::new(0.0, 0.0, 5.0), dir, Vec3::ZERO, Vec3::Z).is_none());
        // The axis is behind the eye: the ray's closest point has t < 0.
        assert!(ray_line_param(Vec3::new(2.0, 1.0, 5.0), Vec3::Z, Vec3::ZERO, Vec3::X).is_none());
    }

    #[test]
    fn a_vanishing_handle_ends_the_drag() {
        let handles = [Handle::Point { pos: Vec3::ZERO }];
        let view = camera();
        let mut state = HandleState::default();
        let at = px(Vec3::ZERO);
        let down = |pos, pressed| PointerInput {
            pos: Some(pos),
            pressed,
            down: true,
        };
        let snap = SnapSet::empty();
        let begin = state.update(&view, SIZE, down(at, true), &handles, &snap, DEFAULT_PICK_RADIUS);
        assert_eq!(begin.map(|e| e.phase), Some(DragPhase::Begin));
        let end = state.update(&view, SIZE, down(at, false), &[], &snap, DEFAULT_PICK_RADIUS);
        assert_eq!(end.map(|e| e.phase), Some(DragPhase::End));
        assert!(!state.dragging());
    }

    /// Review w11 #10: an arc whose handle vanishes mid-drag ends with the dial angle it reached
    /// (a rotation tool committing on `End` must not read a missing angle as no turn).
    #[test]
    fn a_vanishing_arc_ends_with_its_last_angle() {
        let arc = Handle::Arc {
            center: Vec3::ZERO,
            axis: Vec3::Z,
            from: 0.0,
            to: 1.5,
            radius: 2.0,
        };
        let view = camera();
        let mut state = HandleState::default();
        let snap = SnapSet::empty();
        let input = |at: Vec3, pressed| PointerInput {
            pos: Some(px(at)),
            pressed,
            down: true,
        };
        let start = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 0.2);
        let to = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 1.0);
        let begin = state.update(
            &view,
            SIZE,
            input(start, true),
            &[arc],
            &snap,
            DEFAULT_PICK_RADIUS,
        );
        assert_eq!(begin.map(|e| e.phase), Some(DragPhase::Begin));
        let moved = state
            .update(&view, SIZE, input(to, false), &[arc], &snap, DEFAULT_PICK_RADIUS)
            .expect("drag");
        assert_eq!(moved.phase, DragPhase::Drag);
        let reached = moved.angle.expect("dial angle");
        let end = state
            .update(&view, SIZE, input(to, false), &[], &snap, DEFAULT_PICK_RADIUS)
            .expect("end");
        assert_eq!(end.phase, DragPhase::End);
        assert_eq!(end.angle, Some(reached));
        assert!((reached - 1.0).abs() < 2e-2, "{reached}");
        assert!((end.position - moved.position).length() < 1e-5, "{end:?}");
    }

    #[test]
    fn an_arc_drag_snaps_to_the_dial() {
        let arc = Handle::Arc {
            center: Vec3::ZERO,
            axis: Vec3::Z,
            from: 0.0,
            to: 1.5,
            radius: 2.0,
        };
        let on_dial = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 1.0);
        let snap = SnapSet {
            points: vec![SnapPoint {
                position: on_dial,
                kind: SnapKind::FaceCenter,
                entity: Entity::PLACEHOLDER,
                normal: None,
            }],
            truncated: false,
        };
        let start = arc_point(Vec3::ZERO, Vec3::Z, 2.0, 0.2);
        let near = px(on_dial) + Vec2::new(3.0, 2.0);
        let events = drag(&[arc], &snap, px(start), near);
        let end = events.last().expect("end");
        assert!((end.angle.expect("angle") - 1.0).abs() < 1e-3, "{:?}", end.angle);
        assert_eq!(end.snapped.map(|s| s.kind), Some(SnapKind::FaceCenter));
    }

    #[test]
    fn handles_prefer_the_nearer_one() {
        let handles = [
            Handle::Plane {
                origin: Vec3::ZERO,
                normal: Vec3::Z,
                size: 4.0,
            },
            Handle::Point {
                pos: Vec3::new(0.5, 0.0, 0.0),
            },
        ];
        let view = camera();
        assert_eq!(
            hit_test(&handles, &view, SIZE, px(Vec3::new(0.5, 0.0, 0.0)), 10.0),
            Some(1)
        );
        assert_eq!(
            hit_test(&handles, &view, SIZE, px(Vec3::new(-1.0, 1.0, 0.0)), 10.0),
            Some(0)
        );
        assert_eq!(
            hit_test(&handles, &view, SIZE, px(Vec3::new(5.0, 0.0, 0.0)), 10.0),
            None
        );
    }

    #[test]
    fn dragging_snaps_within_the_pixel_radius() {
        let target = Vec3::new(2.0, 1.0, 0.0);
        let snap = SnapSet {
            points: vec![SnapPoint {
                position: target,
                kind: SnapKind::FaceCenter,
                entity: Entity::PLACEHOLDER,
                normal: None,
            }],
            truncated: false,
        };
        let handles = [Handle::Point { pos: Vec3::ZERO }];
        let near = px(target) + Vec2::new(4.0, -3.0);
        let events = drag(&handles, &snap, px(Vec3::ZERO), near);
        let end = events.last().expect("end");
        assert!(close(end.position, target, 1e-4), "{:?}", end.position);
        assert_eq!(end.snapped.map(|s| s.kind), Some(SnapKind::FaceCenter));
        // 30 px away: no snap, the free position follows the pointer.
        let far = px(target) + Vec2::new(30.0, 0.0);
        let end = *drag(&handles, &snap, px(Vec3::ZERO), far).last().expect("end");
        assert!(end.snapped.is_none() && !close(end.position, target, 0.05));
        // An arrow takes only the snap point's projection onto its axis.
        let arrow = [Handle::Arrow {
            origin: Vec3::ZERO,
            dir: Vec3::X,
        }];
        let end = *drag(&arrow, &snap, px(Vec3::new(0.5, 0.0, 0.0)), px(target))
            .last()
            .expect("end");
        assert!(
            close(end.position, Vec3::new(2.5, 0.0, 0.0), 1e-4),
            "grabbed at 0.5, moved to 2: {:?}",
            end.position
        );
    }

    #[test]
    fn nearest_snap_breaks_ties_toward_connectors() {
        let point = |kind| SnapPoint {
            position: Vec3::ZERO,
            kind,
            entity: Entity::PLACEHOLDER,
            normal: None,
        };
        let set = SnapSet {
            points: vec![
                point(SnapKind::BoundsCenter),
                point(SnapKind::Connector),
                point(SnapKind::EdgeMidpoint),
            ],
            truncated: false,
        };
        let found = set
            .nearest(&camera(), SIZE, px(Vec3::ZERO), 5.0)
            .expect("within radius");
        assert_eq!(found.kind, SnapKind::Connector);
        assert!(
            set.nearest(&camera(), SIZE, px(Vec3::ZERO) + Vec2::new(20.0, 0.0), 5.0)
                .is_none()
        );
    }

    fn unit_box() -> (Vec<Vec3>, Vec<u32>) {
        let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
        (
            vertices.iter().map(|v| Vec3::from_array(v.position)).collect(),
            indices,
        )
    }

    #[test]
    fn a_box_has_six_faces_and_twelve_edges() {
        let (positions, indices) = unit_box();
        let (faces, edges) = mesh_features(&positions, &indices);
        assert_eq!(faces.len(), 6, "two triangles per side merge into one face");
        for (center, normal) in &faces {
            assert!((center.length() - 0.5).abs() < 1e-4, "{center:?}");
            assert!(
                close(*center, *normal * 0.5, 1e-4),
                "centered on the face: {center:?} {normal:?}"
            );
        }
        assert_eq!(edges.len(), 12, "diagonals inside a face are not features");
        for m in &edges {
            let on_axes = [m.x.abs(), m.y.abs(), m.z.abs()]
                .iter()
                .filter(|c| (**c - 0.5).abs() < 1e-4)
                .count();
            assert_eq!(on_axes, 2, "{m:?}");
        }
    }

    #[test]
    fn a_single_triangle_has_one_face_and_three_edges() {
        let positions = [Vec3::ZERO, Vec3::X, Vec3::Y];
        let (faces, edges) = mesh_features(&positions, &[0, 1, 2, 0, 1, 9]);
        assert_eq!(faces.len(), 1);
        assert!(close(faces[0].0, Vec3::new(1.0 / 3.0, 1.0 / 3.0, 0.0), 1e-5));
        assert_eq!(edges.len(), 3, "the out-of-range triangle is skipped");
    }

    #[test]
    fn snap_flags_parse_and_combine() {
        let kinds = SnapKinds::from_names(["face_center", "connector"]).expect("known");
        assert!(kinds.contains(SnapKinds::FACE_CENTERS | SnapKinds::CONNECTORS));
        assert!(!kinds.contains(SnapKinds::EDGE_MIDPOINTS));
        assert!(
            SnapKinds::from_names(["all"])
                .expect("all")
                .contains(SnapKinds::ALL)
        );
        assert_eq!(SnapKinds::from_names(["corner"]), Err("corner".to_string()));
        assert!(SnapKinds::NONE.is_empty());
    }
    #[test]
    fn mesh_features_face_order_is_stable_across_hash_seeds() {
        // Two disjoint flat quads (z = 0 and y = 5), two triangles each. Every call builds
        // fresh hash maps with fresh random seeds.
        let positions = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 5.0, 0.0),
            Vec3::new(1.0, 5.0, 0.0),
            Vec3::new(1.0, 5.0, 1.0),
            Vec3::new(0.0, 5.0, 1.0),
        ];
        let indices = [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7];
        let (first_faces, first_edges) = mesh_features(&positions, &indices);
        assert_eq!(first_faces.len(), 2);
        assert!(
            first_faces[0].0.y < 1.0,
            "faces follow the first triangle's index"
        );
        for _ in 0..32 {
            let (faces, edges) = mesh_features(&positions, &indices);
            assert_eq!(faces, first_faces);
            assert_eq!(edges, first_edges);
        }
    }
}
