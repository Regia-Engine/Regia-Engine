// SPDX-License-Identifier: MPL-2.0
//! Debug overlays for screenshots (`docs/specs/23-agent-native.md` § 3.4, ADR-7217): collider
//! outlines, light radii, mesh bounds and a ground grid, as world-space line segments, plus a CPU
//! rasterizer that projects them through a view and draws them over a captured RGBA8 image.
//!
//! The runtime `seeing.screenshot` draws them after the frame is read back (and after the
//! display calibration, so the overlay colors are exact); the editor's viewport capture pushes
//! the same segments through its own overlay painter (`editor.set_viewport`
//! `colliders` / `light_radii`). Overlays are drawn on top of the picture without a depth test
//! (an x-ray view: a collider inside a wall still shows).
//!
//! Algorithms (docs/provenance.md): the segment is clipped to the view's near plane in view
//! space, projected with a right-handed perspective, clipped to the image rectangle with the
//! Liang–Barsky parametric test, and stepped with Bresenham's integer line algorithm. Both are
//! textbook public algorithms (Liang and Barsky 1984; Bresenham 1965).

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Mat4, Quat, Vec3, Vec4};
use regia_physics::{Collider, ColliderShape};
use regia_scene::GlobalTransform;

/// One world-space overlay line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlaySegment {
    /// Start, world space.
    pub a: [f32; 3],
    /// End, world space.
    pub b: [f32; 3],
    /// Straight RGBA8 color.
    pub color: [u8; 4],
}

/// Collider outline color (solid colliders).
pub const COLLIDER_COLOR: [u8; 4] = [64, 255, 96, 255];
/// Sensor (trigger) collider outline color.
pub const SENSOR_COLOR: [u8; 4] = [64, 200, 255, 255];
/// Light radius color.
pub const LIGHT_COLOR: [u8; 4] = [255, 214, 64, 255];
/// Mesh bounds color.
pub const BOUNDS_COLOR: [u8; 4] = [255, 128, 32, 255];
/// Ground grid color.
pub const GRID_COLOR: [u8; 4] = [150, 150, 150, 255];

/// Most segments one overlay call produces; the rest are dropped (a huge trimesh or heightfield
/// would otherwise make one screenshot unbounded).
pub const MAX_SEGMENTS: usize = 65_536;

/// Chords per circle.
const CIRCLE_CHORDS: usize = 32;

/// Heightfield lines per side at most (rows and columns are strided to this).
const HEIGHTFIELD_LINES: usize = 48;

/// Half extent of the ground grid, meters (1 m spacing on the Y = 0 plane).
const GRID_HALF_EXTENT: i32 = 10;

/// The overlay names the runtime `seeing.screenshot` draws.
pub const RUNTIME_OVERLAYS: &[&str] = &["colliders", "light_radii", "bounds", "grid"];

/// Overlay names that need editor state; the runtime answers `F0013` with reason
/// `overlay_needs_editor` for them.
pub const EDITOR_ONLY_OVERLAYS: &[&str] = &["selection", "labels"];

/// A segment sink that stops at [`MAX_SEGMENTS`].
struct Sink<'a> {
    out: &'a mut Vec<OverlaySegment>,
    color: [u8; 4],
    to_world: Affine3A,
}

impl Sink<'_> {
    fn line(&mut self, a: Vec3, b: Vec3) {
        if self.out.len() >= MAX_SEGMENTS {
            return;
        }
        let (a, b) = (
            self.to_world.transform_point3(a),
            self.to_world.transform_point3(b),
        );
        if a.is_finite() && b.is_finite() {
            self.out.push(OverlaySegment {
                a: a.to_array(),
                b: b.to_array(),
                color: self.color,
            });
        }
    }

    /// A circle of `radius` around `center` in the plane of the unit vectors `u`, `v`.
    fn circle(&mut self, center: Vec3, u: Vec3, v: Vec3, radius: f32) {
        let point = |i: usize| {
            let angle = std::f32::consts::TAU * i as f32 / CIRCLE_CHORDS as f32;
            center + (u * angle.cos() + v * angle.sin()) * radius
        };
        for i in 0..CIRCLE_CHORDS {
            self.line(point(i), point(i + 1));
        }
    }

    /// Half a circle from `u` through `v` to `-u`.
    fn arc(&mut self, center: Vec3, u: Vec3, v: Vec3, radius: f32) {
        let half = CIRCLE_CHORDS / 2;
        let point = |i: usize| {
            let angle = std::f32::consts::PI * i as f32 / half as f32;
            center + (u * angle.cos() + v * angle.sin()) * radius
        };
        for i in 0..half {
            self.line(point(i), point(i + 1));
        }
    }

    /// The 12 edges of the box `min..max`.
    fn cuboid(&mut self, min: Vec3, max: Vec3) {
        let corner = |i: usize| {
            Vec3::new(
                if i & 1 == 0 { min.x } else { max.x },
                if i & 2 == 0 { min.y } else { max.y },
                if i & 4 == 0 { min.z } else { max.z },
            )
        };
        for i in 0..8usize {
            for bit in [1usize, 2, 4] {
                if i & bit == 0 {
                    self.line(corner(i), corner(i | bit));
                }
            }
        }
    }
}

/// Appends the outline of `shape` (in the local space `to_world` maps to the world).
fn shape_segments(shape: &ColliderShape, to_world: Affine3A, color: [u8; 4], out: &mut Vec<OverlaySegment>) {
    let mut sink = Sink { out, color, to_world };
    match shape {
        ColliderShape::Box { half_extents } => sink.cuboid(-*half_extents, *half_extents),
        ColliderShape::Sphere { radius } => {
            let r = radius.abs();
            sink.circle(Vec3::ZERO, Vec3::X, Vec3::Y, r);
            sink.circle(Vec3::ZERO, Vec3::X, Vec3::Z, r);
            sink.circle(Vec3::ZERO, Vec3::Y, Vec3::Z, r);
        }
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => {
            // Built along +Y, then turned onto the axis.
            sink.to_world = to_world * Affine3A::from_quat(axis.from_y_rotation());
            let (h, r) = (half_height.abs(), radius.abs());
            let (top, bottom) = (Vec3::Y * h, -Vec3::Y * h);
            sink.circle(top, Vec3::X, Vec3::Z, r);
            sink.circle(bottom, Vec3::X, Vec3::Z, r);
            for side in [Vec3::X, -Vec3::X, Vec3::Z, -Vec3::Z] {
                sink.line(top + side * r, bottom + side * r);
            }
            for side in [Vec3::X, Vec3::Z] {
                sink.arc(top, side, Vec3::Y, r);
                sink.arc(bottom, side, -Vec3::Y, r);
            }
        }
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        } => {
            sink.to_world = to_world * Affine3A::from_quat(axis.from_y_rotation());
            let (h, r) = (half_height.abs(), radius.abs());
            let (top, bottom) = (Vec3::Y * h, -Vec3::Y * h);
            sink.circle(top, Vec3::X, Vec3::Z, r);
            sink.circle(bottom, Vec3::X, Vec3::Z, r);
            for side in [Vec3::X, -Vec3::X, Vec3::Z, -Vec3::Z] {
                sink.line(top + side * r, bottom + side * r);
            }
        }
        ColliderShape::Cone {
            half_height,
            radius,
            axis,
        } => {
            sink.to_world = to_world * Affine3A::from_quat(axis.from_y_rotation());
            let (h, r) = (half_height.abs(), radius.abs());
            let (apex, base) = (Vec3::Y * h, -Vec3::Y * h);
            sink.circle(base, Vec3::X, Vec3::Z, r);
            for side in [Vec3::X, -Vec3::X, Vec3::Z, -Vec3::Z] {
                sink.line(apex, base + side * r);
            }
        }
        ColliderShape::ConvexHull { points } => {
            // The hull's bounding box (computing the hull's faces is the physics backend's job).
            if let Some((min, max)) = point_bounds(points.iter().copied()) {
                sink.cuboid(min, max);
            }
        }
        ColliderShape::TriMesh { vertices, indices } => {
            for tri in indices {
                let corner = |i: u32| vertices.get(i as usize).copied();
                if let (Some(a), Some(b), Some(c)) = (corner(tri[0]), corner(tri[1]), corner(tri[2])) {
                    sink.line(a, b);
                    sink.line(b, c);
                    sink.line(c, a);
                }
                if sink.out.len() >= MAX_SEGMENTS {
                    break;
                }
            }
        }
        ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } => heightfield_segments(&mut sink, *nx, *nz, heights, *scale),
        ColliderShape::Compound { shapes } => {
            for (local, child) in shapes {
                shape_segments(child, to_world * local.compute_affine(), color, sink.out);
            }
        }
    }
}

/// Strided row and column polylines of a heightfield (`ColliderShape::HeightField` layout:
/// sample `(i, j)`, `i` along Z, `j` along X, is `heights[i + j * nx]`; the field spans
/// `[-0.5, 0.5] × scale` in X and Z). A NaN sample (a hole) breaks the polyline.
fn heightfield_segments(sink: &mut Sink<'_>, nx: usize, nz: usize, heights: &[f32], scale: Vec3) {
    if nx < 2 || nz < 2 || heights.len() < nx.saturating_mul(nz) {
        return;
    }
    let point = |i: usize, j: usize| {
        let h = heights[i + j * nx];
        h.is_finite().then(|| {
            Vec3::new(
                (j as f32 / (nz - 1) as f32 - 0.5) * scale.x,
                h * scale.y,
                (i as f32 / (nx - 1) as f32 - 0.5) * scale.z,
            )
        })
    };
    let stride_i = nx.div_ceil(HEIGHTFIELD_LINES).max(1);
    let stride_j = nz.div_ceil(HEIGHTFIELD_LINES).max(1);
    // Lines along X (fixed i), then along Z (fixed j); the last row and column always draw.
    let rows = (0..nx).step_by(stride_i).chain(std::iter::once(nx - 1));
    for i in rows {
        for j in 0..nz - 1 {
            if let (Some(a), Some(b)) = (point(i, j), point(i, j + 1)) {
                sink.line(a, b);
            }
        }
    }
    let cols = (0..nz).step_by(stride_j).chain(std::iter::once(nz - 1));
    for j in cols {
        for i in 0..nx - 1 {
            if let (Some(a), Some(b)) = (point(i, j), point(i + 1, j)) {
                sink.line(a, b);
            }
        }
    }
}

/// The bounds of a point set (`None` when empty or not finite).
fn point_bounds(points: impl Iterator<Item = Vec3>) -> Option<(Vec3, Vec3)> {
    let mut bounds: Option<(Vec3, Vec3)> = None;
    for p in points.filter(|p| p.is_finite()) {
        bounds = Some(match bounds {
            None => (p, p),
            Some((min, max)) => (min.min(p), max.max(p)),
        });
    }
    bounds
}

/// The outline of every collider in the world: a [`Collider`] at its entity's
/// [`GlobalTransform`] composed with its `offset`, and a bare [`ColliderShape`] component at its
/// entity's transform. Sensors draw in [`SENSOR_COLOR`], solid colliders in [`COLLIDER_COLOR`].
/// Entities are visited in spawn order, so the result is deterministic.
pub fn collider_segments(world: &World) -> Vec<OverlaySegment> {
    collider_segments_colored(
        world,
        |_, sensor| if sensor { SENSOR_COLOR } else { COLLIDER_COLOR },
    )
}

/// Collider outlines with a host-selected color per entity and sensor state.
/// Geometry, ordering and the segment cap are identical to [`collider_segments`].
pub fn collider_segments_colored(
    world: &World,
    color: impl Fn(Entity, bool) -> [u8; 4],
) -> Vec<OverlaySegment> {
    let _span = tracing::trace_span!("collider_overlay_segments").entered();
    let mut out = Vec::new();
    let mut found: Vec<(Entity, Affine3A, ColliderShape, bool)> = Vec::new();
    if let Some(mut colliders) = world.try_query::<(Entity, &GlobalTransform, &Collider)>() {
        found.extend(colliders.iter(world).map(|(entity, global, collider)| {
            (
                entity,
                global.0 * collider.offset.compute_affine(),
                collider.shape.clone(),
                collider.sensor,
            )
        }));
    }
    let with_collider: std::collections::HashSet<Entity> = found.iter().map(|(index, ..)| *index).collect();
    if let Some(mut shapes) = world.try_query::<(Entity, &GlobalTransform, &ColliderShape)>() {
        found.extend(
            shapes
                .iter(world)
                .filter(|(entity, ..)| !with_collider.contains(entity))
                .map(|(entity, global, shape)| (entity, global.0, shape.clone(), false)),
        );
    }
    found.sort_by_key(|(entity, ..)| entity.index_u32());
    for (entity, to_world, shape, sensor) in &found {
        let color = color(*entity, *sensor);
        shape_segments(shape, *to_world, color, &mut out);
        if out.len() >= MAX_SEGMENTS {
            break;
        }
    }
    out
}

/// The outlines of the sensor colliders only (trigger volumes), in [`SENSOR_COLOR`], for entities
/// `keep` accepts. Only sensor shapes are cloned, so a level's solid trimeshes and heightfields cost
/// nothing here and cannot spend the [`MAX_SEGMENTS`] cap before a sensor spawned later is reached
/// (the editor's trigger-volume helper, ADR-7495). Entities are visited in spawn order.
pub fn sensor_segments(world: &World, keep: impl Fn(Entity) -> bool) -> Vec<OverlaySegment> {
    let mut out = Vec::new();
    let mut found: Vec<(u32, Affine3A, ColliderShape)> = Vec::new();
    if let Some(mut colliders) = world.try_query::<(Entity, &GlobalTransform, &Collider)>() {
        found.extend(
            colliders
                .iter(world)
                .filter(|(entity, _, collider)| collider.sensor && keep(*entity))
                .map(|(entity, global, collider)| {
                    (
                        entity.index_u32(),
                        global.0 * collider.offset.compute_affine(),
                        collider.shape.clone(),
                    )
                }),
        );
    }
    found.sort_by_key(|(index, ..)| *index);
    for (_, to_world, shape) in &found {
        shape_segments(shape, *to_world, SENSOR_COLOR, &mut out);
        if out.len() >= MAX_SEGMENTS {
            break;
        }
    }
    out
}

/// The reach of every point and spot light: three circles of the point light's `radius` around
/// its position, and the spot light's cone (the circle at `range` with four lines to the apex).
/// Positions and directions come from the entity's [`GlobalTransform`] (spot lights point down
/// their local −Z, as the renderer's extraction does).
pub fn light_radius_segments(world: &World) -> Vec<OverlaySegment> {
    let mut out = Vec::new();
    let mut point_lights: Vec<(u32, Vec3, f32)> = world
        .try_query::<(Entity, &GlobalTransform, &regia_render::PointLight)>()
        .map(|mut points| {
            points
                .iter(world)
                .map(|(entity, global, light)| {
                    (entity.index_u32(), Vec3::from(global.0.translation), light.radius)
                })
                .collect()
        })
        .unwrap_or_default();
    point_lights.sort_by_key(|(index, ..)| *index);
    let mut sink = Sink {
        out: &mut out,
        color: LIGHT_COLOR,
        to_world: Affine3A::IDENTITY,
    };
    for (_, center, radius) in point_lights {
        let r = radius.abs().max(0.01);
        sink.circle(center, Vec3::X, Vec3::Z, r);
        sink.circle(center, Vec3::X, Vec3::Y, r);
        sink.circle(center, Vec3::Z, Vec3::Y, r);
    }
    let mut spot_lights: Vec<(u32, Affine3A, f32, f32)> = world
        .try_query::<(Entity, &GlobalTransform, &regia_render::SpotLight)>()
        .map(|mut spots| {
            spots
                .iter(world)
                .map(|(entity, global, light)| (entity.index_u32(), global.0, light.range, light.outer_angle))
                .collect()
        })
        .unwrap_or_default();
    spot_lights.sort_by_key(|(index, ..)| *index);
    for (_, global, range, outer) in spot_lights {
        let (_, rotation, apex): (Vec3, Quat, Vec3) = global.to_scale_rotation_translation();
        let forward = rotation * Vec3::NEG_Z;
        let (right, up) = (rotation * Vec3::X, rotation * Vec3::Y);
        let range = range.abs().max(0.01);
        let radius = range * outer.clamp(0.0, 1.5).tan();
        let base = apex + forward * range;
        sink.circle(base, right, up, radius);
        for dir in [right, -right, up, -up] {
            sink.line(apex, base + dir * radius);
        }
    }
    out
}

/// The oriented bounds of every [`regia_render::MeshRenderer`]: its mesh's local box through the
/// entity's [`GlobalTransform`].
pub fn bounds_segments(world: &World) -> Vec<OverlaySegment> {
    let mut out = Vec::new();
    let mut found: Vec<(u32, Affine3A, Vec3, Vec3)> = world
        .try_query::<(Entity, &GlobalTransform, &regia_render::MeshRenderer)>()
        .map(|mut meshes| {
            meshes
                .iter(world)
                .filter_map(|(entity, global, renderer)| {
                    let (min, max) = point_bounds(
                        renderer
                            .mesh
                            .vertices
                            .iter()
                            .map(|v| Vec3::from_array(v.position)),
                    )?;
                    Some((entity.index_u32(), global.0, min, max))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|(index, ..)| *index);
    for (_, to_world, min, max) in found {
        let mut sink = Sink {
            out: &mut out,
            color: BOUNDS_COLOR,
            to_world,
        };
        sink.cuboid(min, max);
    }
    out
}

/// A 1 m ground grid on the Y = 0 plane, ±10 m around the origin.
#[must_use]
pub fn grid_segments() -> Vec<OverlaySegment> {
    let mut out = Vec::new();
    let mut sink = Sink {
        out: &mut out,
        color: GRID_COLOR,
        to_world: Affine3A::IDENTITY,
    };
    let e = GRID_HALF_EXTENT as f32;
    for i in -GRID_HALF_EXTENT..=GRID_HALF_EXTENT {
        let t = i as f32;
        sink.line(Vec3::new(t, 0.0, -e), Vec3::new(t, 0.0, e));
        sink.line(Vec3::new(-e, 0.0, t), Vec3::new(e, 0.0, t));
    }
    out
}

/// The segments of the named runtime overlays ([`RUNTIME_OVERLAYS`]), in the order given.
/// Unknown names are the caller's to refuse first.
pub fn segments_for(world: &World, names: &[&str]) -> Vec<OverlaySegment> {
    let mut out = Vec::new();
    for name in names {
        let more = match *name {
            "colliders" => collider_segments(world),
            "light_radii" => light_radius_segments(world),
            "bounds" => bounds_segments(world),
            "grid" => grid_segments(),
            _ => Vec::new(),
        };
        out.extend(more.into_iter().take(MAX_SEGMENTS.saturating_sub(out.len())));
    }
    out
}

/// The view a captured image was rendered from, for projecting overlays onto it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayView {
    /// Eye position, world space.
    pub eye: [f32; 3],
    /// A point the camera looks at.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane distance, meters.
    pub near: f32,
    /// Far plane distance, meters.
    pub far: f32,
}

impl OverlayView {
    /// The view of an extracted camera.
    #[must_use]
    pub fn of(camera: &regia_render::runtime::ExtractedCamera) -> Self {
        Self {
            eye: camera.eye,
            target: camera.target,
            fov_y: camera.fov_y,
            near: camera.near,
            far: camera.far,
        }
    }

    /// World → clip matrix for a `width`×`height` image (the renderer's guarded look-at and
    /// projection inputs, `regia_render::view_math`).
    fn clip_from_world(&self, width: u32, height: u32) -> (Mat4, Mat4, f32) {
        let eye = Vec3::from_array(self.eye);
        let target = Vec3::from_array(self.target);
        let view = regia_render::view_math::look_at_view(eye, target, Vec3::Y);
        let (fov_y, near, far) = regia_render::view_math::usable_projection(self.fov_y, self.near, self.far);
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        let projection = regia_core::glam::camera::rh::proj::directx::perspective(fov_y, aspect, near, far);
        (view, projection, near)
    }
}

/// Clips the view-space segment `a..b` to the near plane (`z <= -near`); `None` when it lies
/// wholly in front of it.
fn clip_near(a: Vec3, b: Vec3, near: f32) -> Option<(Vec3, Vec3)> {
    let plane = -near;
    let (a_in, b_in) = (a.z <= plane, b.z <= plane);
    match (a_in, b_in) {
        (true, true) => Some((a, b)),
        (false, false) => None,
        _ => {
            let t = (plane - a.z) / (b.z - a.z);
            let cut = a + (b - a) * t;
            Some(if a_in { (a, cut) } else { (cut, b) })
        }
    }
}

/// Liang–Barsky: clips the screen segment `p0..p1` to `[0, w) × [0, h)`.
fn clip_rect(p0: [f32; 2], p1: [f32; 2], w: f32, h: f32) -> Option<([f32; 2], [f32; 2])> {
    let (dx, dy) = (p1[0] - p0[0], p1[1] - p0[1]);
    let mut t0 = 0.0_f32;
    let mut t1 = 1.0_f32;
    let max_x = w - 1.0;
    let max_y = h - 1.0;
    for (p, q) in [
        (-dx, p0[0]),
        (dx, max_x - p0[0]),
        (-dy, p0[1]),
        (dy, max_y - p0[1]),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let r = q / p;
        if p < 0.0 {
            if r > t1 {
                return None;
            }
            t0 = t0.max(r);
        } else {
            if r < t0 {
                return None;
            }
            t1 = t1.min(r);
        }
    }
    Some((
        [p0[0] + t0 * dx, p0[1] + t0 * dy],
        [p0[0] + t1 * dx, p0[1] + t1 * dy],
    ))
}

/// Bresenham's line between two in-bounds pixels.
fn bresenham(rgba: &mut [u8], width: usize, a: (i64, i64), b: (i64, i64), color: [u8; 4]) -> u64 {
    let (mut x, mut y) = a;
    let dx = (b.0 - a.0).abs();
    let dy = -(b.1 - a.1).abs();
    let sx = if a.0 < b.0 { 1 } else { -1 };
    let sy = if a.1 < b.1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut drawn = 0;
    loop {
        let index = (y as usize * width + x as usize) * 4;
        if let Some(pixel) = rgba.get_mut(index..index + 4) {
            pixel.copy_from_slice(&color);
            drawn += 1;
        }
        if x == b.0 && y == b.1 {
            return drawn;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Draws `segments` over the tightly packed RGBA8 image `rgba` (`width`×`height`) as seen from
/// `view`; answers how many pixels were written. Segments behind the camera or outside the image
/// draw nothing; no depth test.
pub fn draw_segments(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    view: &OverlayView,
    segments: &[OverlaySegment],
) -> u64 {
    let _span = tracing::info_span!("seeing_overlays_draw", segments = segments.len()).entered();
    if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
        return 0;
    }
    let (view_m, projection, near) = view.clip_from_world(width, height);
    let (w, h) = (width as f32, height as f32);
    let to_screen = |p: Vec3| -> Option<[f32; 2]> {
        let clip: Vec4 = projection * p.extend(1.0);
        (clip.w > 0.0).then(|| {
            let ndc = clip.truncate() / clip.w;
            [(ndc.x * 0.5 + 0.5) * w, (0.5 - ndc.y * 0.5) * h]
        })
    };
    let mut drawn = 0;
    for segment in segments {
        let a = view_m.transform_point3(Vec3::from_array(segment.a));
        let b = view_m.transform_point3(Vec3::from_array(segment.b));
        let Some((a, b)) = clip_near(a, b, near) else {
            continue;
        };
        let (Some(pa), Some(pb)) = (to_screen(a), to_screen(b)) else {
            continue;
        };
        if !(pa.iter().chain(pb.iter()).all(|v| v.is_finite())) {
            continue;
        }
        let Some((pa, pb)) = clip_rect(pa, pb, w, h) else {
            continue;
        };
        let pixel = |p: [f32; 2]| {
            (
                (p[0].floor() as i64).clamp(0, i64::from(width) - 1),
                (p[1].floor() as i64).clamp(0, i64::from(height) - 1),
            )
        };
        drawn += bresenham(rgba, width as usize, pixel(pa), pixel(pb), segment.color);
    }
    drawn
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::Transform;

    fn front_view() -> OverlayView {
        OverlayView {
            eye: [0.0, 0.0, 5.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 100.0,
        }
    }

    fn world_with(shape: ColliderShape, at: Vec3) -> World {
        let mut world = World::new();
        let transform = Transform::from_translation(at);
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            Collider {
                shape,
                ..Collider::default()
            },
        ));
        world
    }

    #[test]
    fn a_box_collider_outlines_twelve_edges_and_draws_its_silhouette() {
        let world = world_with(
            ColliderShape::Box {
                half_extents: Vec3::splat(0.5),
            },
            Vec3::ZERO,
        );
        let segments = collider_segments(&world);
        assert_eq!(segments.len(), 12);
        assert!(segments.iter().all(|s| s.color == COLLIDER_COLOR));
        let (w, h) = (64u32, 48u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let drawn = draw_segments(&mut rgba, w, h, &front_view(), &segments);
        assert!(drawn > 40, "{drawn} pixels");
        // The front face's top edge crosses the image's vertical center line above the middle.
        let column: Vec<usize> = (0..h as usize)
            .filter(|y| rgba[(y * w as usize + w as usize / 2) * 4 + 1] == COLLIDER_COLOR[1])
            .collect();
        assert!(column.iter().any(|y| *y < h as usize / 2), "{column:?}");
        assert!(column.iter().any(|y| *y > h as usize / 2), "{column:?}");
        // The center (inside the box) stays untouched: an outline, not a fill.
        let center = ((h as usize / 2) * w as usize + w as usize / 2) * 4;
        assert_eq!(&rgba[center..center + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn every_shape_outlines_and_offsets_move_it() {
        let shapes = [
            ColliderShape::Sphere { radius: 1.0 },
            ColliderShape::Capsule {
                half_height: 0.5,
                radius: 0.3,
                axis: regia_physics::ShapeAxis::X,
            },
            ColliderShape::Cylinder {
                half_height: 0.5,
                radius: 0.3,
                axis: regia_physics::ShapeAxis::Y,
            },
            ColliderShape::Cone {
                half_height: 0.5,
                radius: 0.3,
                axis: regia_physics::ShapeAxis::Z,
            },
            ColliderShape::ConvexHull {
                points: vec![Vec3::ZERO, Vec3::ONE, Vec3::new(1.0, 0.0, 0.0)],
            },
            ColliderShape::TriMesh {
                vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
                indices: vec![[0, 1, 2], [0, 1, 9]],
            },
            ColliderShape::HeightField {
                nx: 3,
                nz: 3,
                heights: vec![0.0, 0.1, f32::NAN, 0.0, 0.2, 0.0, 0.0, 0.0, 0.0],
                scale: Vec3::new(4.0, 1.0, 4.0),
            },
            ColliderShape::Compound {
                shapes: vec![(
                    Transform::from_translation(Vec3::X),
                    ColliderShape::Sphere { radius: 0.2 },
                )],
            },
        ];
        for shape in shapes {
            let world = world_with(shape.clone(), Vec3::ZERO);
            let segments = collider_segments(&world);
            assert!(!segments.is_empty(), "{shape:?}");
            assert!(
                segments
                    .iter()
                    .all(|s| s.a.iter().chain(&s.b).all(|v| v.is_finite()))
            );
        }
        // The trimesh's bad index is skipped: one triangle, three edges.
        let world = world_with(
            ColliderShape::TriMesh {
                vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
                indices: vec![[0, 1, 2], [0, 1, 9]],
            },
            Vec3::ZERO,
        );
        assert_eq!(collider_segments(&world).len(), 3);
        // The collider offset composes with the entity transform.
        let mut world = World::new();
        let transform = Transform::from_translation(Vec3::new(10.0, 0.0, 0.0));
        world.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.0 },
                offset: Transform::from_translation(Vec3::new(0.0, 3.0, 0.0)),
                sensor: true,
                ..Collider::default()
            },
        ));
        let segments = collider_segments(&world);
        assert!(
            segments
                .iter()
                .all(|s| s.a == [10.0, 3.0, 0.0] && s.color == SENSOR_COLOR)
        );
    }

    #[test]
    fn light_radii_circle_point_lights_and_cone_spot_lights() {
        let mut world = World::new();
        let at = Transform::from_translation(Vec3::new(1.0, 2.0, 3.0));
        world.spawn((
            at,
            GlobalTransform(at.compute_affine()),
            regia_render::PointLight {
                radius: 2.0,
                ..regia_render::PointLight::default()
            },
        ));
        let segments = light_radius_segments(&world);
        assert_eq!(segments.len(), 3 * CIRCLE_CHORDS);
        for s in &segments {
            let d = Vec3::from_array(s.a) - Vec3::new(1.0, 2.0, 3.0);
            assert!((d.length() - 2.0).abs() < 1e-4, "{d}");
        }
        let spot = Transform::default();
        world.spawn((
            spot,
            GlobalTransform(spot.compute_affine()),
            regia_render::SpotLight {
                range: 4.0,
                outer_angle: 0.5,
                ..regia_render::SpotLight::default()
            },
        ));
        let segments = light_radius_segments(&world);
        assert_eq!(segments.len(), 3 * CIRCLE_CHORDS + CIRCLE_CHORDS + 4);
        // The cone's base circle lies at range 4 down −Z.
        let base = &segments[3 * CIRCLE_CHORDS];
        assert!((base.a[2] + 4.0).abs() < 1e-4, "{base:?}");
    }

    #[test]
    fn segments_behind_the_camera_or_off_image_draw_nothing() {
        let (w, h) = (32u32, 32u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let behind = OverlaySegment {
            a: [0.0, 0.0, 6.0],
            b: [1.0, 0.0, 7.0],
            color: [255; 4],
        };
        let far_left = OverlaySegment {
            a: [-500.0, 0.0, 0.0],
            b: [-400.0, 1.0, 0.0],
            color: [255; 4],
        };
        assert_eq!(
            draw_segments(&mut rgba, w, h, &front_view(), &[behind, far_left]),
            0
        );
        assert!(rgba.iter().all(|b| *b == 0));
        // A segment crossing the near plane is cut, not dropped or mirrored.
        let crossing = OverlaySegment {
            a: [0.0, -0.5, 0.0],
            b: [0.0, -0.5, 10.0],
            color: [255; 4],
        };
        assert!(draw_segments(&mut rgba, w, h, &front_view(), &[crossing]) > 0);
    }

    #[test]
    fn huge_trimeshes_are_capped() {
        let n = 30_000u32;
        let vertices: Vec<Vec3> = (0..3).map(|i| Vec3::new(i as f32, 0.0, 0.0)).collect();
        let world = world_with(
            ColliderShape::TriMesh {
                vertices,
                indices: (0..n).map(|_| [0, 1, 2]).collect(),
            },
            Vec3::ZERO,
        );
        assert_eq!(collider_segments(&world).len(), MAX_SEGMENTS);
    }

    /// The integration test `tests/seeing_camera_ui_overlays.rs` inlines these colors.
    #[test]
    fn overlay_colors_match_the_library() {
        assert_eq!(COLLIDER_COLOR, [64, 255, 96, 255]);
        assert_eq!(LIGHT_COLOR, [255, 214, 64, 255]);
    }

    #[test]
    fn grid_and_bounds_segments() {
        assert_eq!(grid_segments().len(), 2 * (2 * GRID_HALF_EXTENT as usize + 1));
        let mut world = World::new();
        let at = Transform::default();
        world.spawn((
            at,
            GlobalTransform(at.compute_affine()),
            regia_render::MeshRenderer::new(
                std::sync::Arc::new(regia_render::CpuMesh {
                    vertices: vec![
                        regia_render::LabVertex::from_pos_normal_uv(
                            [0.0, 0.0, 0.0],
                            [0.0, 0.0, 1.0],
                            [0.0, 0.0],
                        ),
                        regia_render::LabVertex::from_pos_normal_uv(
                            [1.0, 1.0, 0.0],
                            [0.0, 0.0, 1.0],
                            [1.0, 1.0],
                        ),
                        regia_render::LabVertex::from_pos_normal_uv(
                            [1.0, 0.0, 1.0],
                            [0.0, 0.0, 1.0],
                            [1.0, 0.0],
                        ),
                    ],
                    indices: vec![0, 1, 2],
                }),
                std::sync::Arc::new(regia_render::CpuMaterial::default()),
            ),
        ));
        let bounds = bounds_segments(&world);
        assert_eq!(bounds.len(), 12);
        assert_eq!(
            segments_for(&world, &["bounds", "grid", "nope"]).len(),
            12 + grid_segments().len()
        );
    }
}
