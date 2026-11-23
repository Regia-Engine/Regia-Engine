// SPDX-License-Identifier: MPL-2.0
//! Immediate 3D primitives: lit cubes, spheres, cylinders, cones and planes drawn through the
//! renderer's forward path with a default material, plus world-space lines and grids.
//!
//! The API follows macroquad's `draw_cube` / `draw_sphere` / `draw_cylinder` / `draw_plane` /
//! `draw_grid` / `draw_line_3d` family (meshes from [`regia_render::primitives::models`]),
//! with a builder per drawing like [`crate::Draw`]: `draw3d.cube(centre, size).color(c)`.
//! Every drawing lasts one frame, unless [`crate::DrawFrozen`] is set: extraction then
//! resubmits the last drained frame and leaves new drawings buffered until the next live
//! frame. Primitives cast and receive shadows and take the scene's lighting, exposure and
//! post-processing; lines draw as gizmo lines (group `quick`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use regia_render::primitives::models;
use regia_render::{ALPHA_BLEND, ALPHA_OPAQUE, CpuMaterial, CpuMesh, ExtractedMesh, ShadowCast};

use crate::prelude_internal::*;

/// The shape of a 3D drawing, in local units before its model matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape3d {
    /// A unit cube (`-0.5..0.5`).
    Cube,
    /// A unit-radius sphere.
    Sphere {
        /// Latitude bands between the poles.
        rings: u32,
        /// Meridians.
        slices: u32,
    },
    /// A cylinder of height 1 standing on the origin; the larger end has radius 1.
    Cylinder {
        /// Top radius over bottom radius (`0` is a cone).
        top: f32,
        /// Bottom radius over the larger radius.
        bottom: f32,
        /// Segments around.
        sides: u32,
    },
    /// A unit plane (`-1..1` in X and Z) facing +Y.
    Plane,
}

/// One 3D drawing.
#[derive(Clone, Debug, PartialEq)]
pub struct Item3d {
    /// Shape.
    pub shape: Shape3d,
    /// Local-to-world matrix.
    pub model: Mat4,
    /// Base colour (linear; alpha below 1 blends).
    pub color: Color,
    /// Perceptual roughness.
    pub roughness: f32,
    /// Metalness.
    pub metallic: f32,
    /// Emitted radiance multiplier of the base colour (0 = none).
    pub emissive: f32,
}

#[derive(Default)]
struct State3d {
    items: Vec<Item3d>,
    lines: Vec<(Vec3, Vec3, Color)>,
}

/// Immediate 3D drawing (a [`Resource`]): `fn view(draw3d: Res<Draw3d>) { draw3d.sphere(Vec3::Y, 0.5); }`.
#[derive(Resource, Clone, Default)]
pub struct Draw3d {
    state: Arc<Mutex<State3d>>,
    /// Times `lock` has run. Shared by clones.
    locks: Arc<AtomicU64>,
}

/// A 3D drawing in progress; it is recorded when dropped.
pub struct Drawing3d<'a> {
    draw: &'a Draw3d,
    item: Option<Item3d>,
}

impl Drawing3d<'_> {
    fn map(mut self, f: impl FnOnce(&mut Item3d)) -> Self {
        if let Some(item) = &mut self.item {
            f(item);
        }
        self
    }

    /// Base colour.
    pub fn color(self, color: impl Into<Color>) -> Self {
        let color = color.into();
        self.map(|i| i.color = color)
    }

    /// Rotates the drawing about its own centre.
    pub fn rotation(self, rotation: Quat) -> Self {
        self.map(|i| {
            let (scale, r, t) = i.model.to_scale_rotation_translation();
            // A cylinder mesh stands on its origin; pivot about its mid-height instead.
            let local_centre = match i.shape {
                Shape3d::Cylinder { .. } => Vec3::new(0.0, 0.5, 0.0),
                _ => Vec3::ZERO,
            };
            let centre = t + r * (scale * local_centre);
            let r = rotation * r;
            i.model = Mat4::from_scale_rotation_translation(scale, r, centre - r * (scale * local_centre));
        })
    }

    /// Applies `transform` after the drawing's own placement (e.g. a parent's matrix).
    pub fn transform(self, transform: Mat4) -> Self {
        self.map(|i| i.model = transform * i.model)
    }

    /// Perceptual roughness (`0..1`, default 0.5).
    pub fn roughness(self, roughness: f32) -> Self {
        self.map(|i| i.roughness = roughness.clamp(0.0, 1.0))
    }

    /// Metalness (`0..1`, default 0).
    pub fn metallic(self, metallic: f32) -> Self {
        self.map(|i| i.metallic = metallic.clamp(0.0, 1.0))
    }

    /// Makes the surface emit its base colour times `intensity`.
    pub fn emissive(self, intensity: f32) -> Self {
        self.map(|i| i.emissive = intensity.max(0.0))
    }
}

impl Drop for Drawing3d<'_> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            self.draw.lock().items.push(item);
        }
    }
}

/// Most cells across a `Draw3d::grid`; larger requests are capped so a typo cannot exhaust memory.
const MAX_GRID_SLICES: u32 = 1024;

impl Draw3d {
    fn lock(&self) -> std::sync::MutexGuard<'_, State3d> {
        self.locks.fetch_add(1, Ordering::Relaxed);
        self.lock_quiet()
    }

    fn lock_quiet(&self) -> std::sync::MutexGuard<'_, State3d> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Times [`Self::line`], [`Self::lines`], [`Self::grid`] and the other list mutators have
    /// locked the draw. Capacity queries do not count.
    #[must_use]
    pub fn lock_count(&self) -> u64 {
        self.locks.load(Ordering::Relaxed)
    }

    /// Capacity of the mesh-item list (kept across a retaining drain).
    #[must_use]
    pub fn items_capacity(&self) -> usize {
        self.lock_quiet().items.capacity()
    }

    /// Capacity of the line list.
    #[must_use]
    pub fn lines_capacity(&self) -> usize {
        self.lock_quiet().lines.capacity()
    }

    /// Appends every segment under one lock.
    pub fn lines(&self, segments: impl IntoIterator<Item = (Vec3, Vec3, Color)>) {
        let mut state = self.lock();
        for (start, end, color) in segments {
            state.lines.push((start, end, color));
        }
    }

    fn begin(&self, shape: Shape3d, model: Mat4) -> Drawing3d<'_> {
        Drawing3d {
            draw: self,
            item: Some(Item3d {
                shape,
                model,
                color: Color::from_srgb(0.8, 0.8, 0.8, 1.0),
                roughness: 0.5,
                metallic: 0.0,
                emissive: 0.0,
            }),
        }
    }

    /// A box of `size` (full extents) centred at `center`.
    pub fn cube(&self, center: Vec3, size: Vec3) -> Drawing3d<'_> {
        self.begin(
            Shape3d::Cube,
            Mat4::from_scale_rotation_translation(size, Quat::IDENTITY, center),
        )
    }

    /// A sphere (16 rings, 16 slices).
    pub fn sphere(&self, center: Vec3, radius: f32) -> Drawing3d<'_> {
        self.sphere_ex(center, radius, 16, 16)
    }

    /// A sphere with explicit tessellation. `rings` is clamped to `1..=256` and `slices` to
    /// `3..=256`; larger values are silently reduced to the limit.
    pub fn sphere_ex(&self, center: Vec3, radius: f32, rings: u32, slices: u32) -> Drawing3d<'_> {
        self.begin(
            Shape3d::Sphere {
                rings: rings.clamp(1, 256),
                slices: slices.clamp(3, 256),
            },
            Mat4::from_scale_rotation_translation(Vec3::splat(radius), Quat::IDENTITY, center),
        )
    }

    /// A cylinder (or cone, when a radius is 0) standing on `base` (16 sides).
    pub fn cylinder(&self, base: Vec3, radius_top: f32, radius_bottom: f32, height: f32) -> Drawing3d<'_> {
        self.cylinder_ex(base, radius_top, radius_bottom, height, 16)
    }

    /// A cylinder with explicit side count. `sides` is clamped to `3..=256`; larger values are
    /// silently reduced to the limit.
    pub fn cylinder_ex(
        &self,
        base: Vec3,
        radius_top: f32,
        radius_bottom: f32,
        height: f32,
        sides: u32,
    ) -> Drawing3d<'_> {
        let (top, bottom) = (radius_top.max(0.0), radius_bottom.max(0.0));
        let r = top.max(bottom).max(1e-6);
        // Ratios quantised so a mesh is shared by nearly equal shapes.
        // A positive radius keeps at least one step so a thin end stays a cylinder with a cap.
        let q = |v: f32| {
            let ratio = (v / r * 256.0).round() / 256.0;
            if v > 0.0 { ratio.max(1.0 / 256.0) } else { ratio }
        };
        // The unit mesh stands on the origin and winds outward. A negative height would scale
        // Y below zero and flip that winding, so the forward pass would cull the primitive.
        // Place the same mesh below `base` with a positive Y scale instead.
        let (y_scale, origin) = if height.is_sign_negative() {
            (-height, base + Vec3::Y * height)
        } else {
            (height, base)
        };
        self.begin(
            Shape3d::Cylinder {
                top: q(top),
                bottom: q(bottom),
                sides: sides.clamp(3, 256),
            },
            Mat4::from_scale_rotation_translation(Vec3::new(r, y_scale, r), Quat::IDENTITY, origin),
        )
    }

    /// A cone with its base on `base` and apex `height` above it.
    pub fn cone(&self, base: Vec3, radius: f32, height: f32) -> Drawing3d<'_> {
        self.cylinder(base, 0.0, radius, height)
    }

    /// A horizontal plane facing +Y, `half_size` (x, z) from `center`.
    pub fn plane(&self, center: Vec3, half_size: Vec2) -> Drawing3d<'_> {
        self.begin(
            Shape3d::Plane,
            Mat4::from_scale_rotation_translation(
                Vec3::new(half_size.x, 1.0, half_size.y),
                Quat::IDENTITY,
                center,
            ),
        )
    }

    /// A line segment in world space.
    pub fn line(&self, start: Vec3, end: Vec3, color: impl Into<Color>) {
        let color = color.into();
        self.lock().lines.push((start, end, color));
    }

    /// A grid in the XZ plane centred at `center` (`slices` cells across, `spacing` apart);
    /// the lines through the centre use `axes_color`. `slices` is capped at 1024 and a
    /// non-finite `spacing` draws nothing.
    pub fn grid(&self, center: Vec3, slices: u32, spacing: f32, axes_color: Color, other_color: Color) {
        if slices < 2 {
            return;
        }
        let slices = slices.min(MAX_GRID_SLICES);
        let spacing = if spacing.is_finite() { spacing } else { return };
        let mut state = self.lock();
        for (a, b, axis) in models::grid_lines(slices, spacing) {
            state.lines.push((
                a + center,
                b + center,
                if axis { axes_color } else { other_color },
            ));
        }
    }

    /// The 12 edges of a box of `size` centred at `center`.
    pub fn cube_wires(&self, center: Vec3, size: Vec3, color: impl Into<Color>) {
        let color = color.into();
        let mut state = self.lock();
        for (a, b) in models::cube_wire_lines(size) {
            state.lines.push((a + center, b + center, color));
        }
    }

    /// True when nothing was drawn this frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let state = self.lock();
        state.items.is_empty() && state.lines.is_empty()
    }

    /// Takes this frame's drawings and lines.
    pub fn drain(&self) -> (Vec<Item3d>, Vec<(Vec3, Vec3, Color)>) {
        let mut state = self.lock();
        // The replacement keeps the capacity the next frame will fill.
        let n_items = state.items.len();
        let n_lines = state.lines.len();
        let items = std::mem::replace(&mut state.items, Vec::with_capacity(n_items));
        let lines = std::mem::replace(&mut state.lines, Vec::with_capacity(n_lines));
        (items, lines)
    }

    /// A copy of this frame's drawings and lines. Does not consume them.
    #[must_use]
    pub fn snapshot(&self) -> (Vec<Item3d>, Vec<(Vec3, Vec3, Color)>) {
        let state = self.lock();
        (state.items.clone(), state.lines.clone())
    }
}

/// Bit pattern key of a shape (meshes are shared by equal keys). The last field is the
/// reversed-winding variant used when a model matrix mirrors.
fn shape_key(shape: Shape3d, reverse: bool) -> (u8, u32, u32, u32, u8) {
    let (tag, a, b, c) = match shape {
        Shape3d::Cube => (0, 0, 0, 0),
        Shape3d::Sphere { rings, slices } => (1, rings, slices, 0),
        Shape3d::Cylinder { top, bottom, sides } => (2, top.to_bits(), bottom.to_bits(), sides),
        Shape3d::Plane => (3, 0, 0, 0),
    };
    (tag, a, b, c, u8::from(reverse))
}

/// True when `model`'s linear part is a reflection. An odd number of negative scale axes
/// (or any other mirror) flips triangle winding, and the forward pass culls those faces.
fn reflects(model: Mat4) -> bool {
    let x = model.x_axis.truncate();
    let y = model.y_axis.truncate();
    let z = model.z_axis.truncate();
    x.dot(y.cross(z)) < 0.0
}

fn build_mesh(shape: Shape3d, reverse: bool) -> CpuMesh {
    let _span = tracing::info_span!(target: "regia_quick", "quick_3d_mesh").entered();
    let (vertices, mut indices) = match shape {
        Shape3d::Cube => models::cube_mesh(Vec3::ONE),
        Shape3d::Sphere { rings, slices } => models::sphere_mesh(1.0, rings, slices),
        Shape3d::Cylinder { top, bottom, sides } => models::cylinder_mesh(top, bottom, 1.0, sides),
        Shape3d::Plane => models::plane_mesh(Vec2::ONE),
    };
    if reverse {
        // Swap the last two indices of each triangle so the mirrored model still faces outward.
        // Vertex normals stay: the shader's inverse-transpose flips them with the mirror.
        for tri in indices.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
    }
    CpuMesh { vertices, indices }
}

/// Most cached meshes / materials before the caches are dropped and rebuilt.
const CACHE_LIMIT: usize = 512;

/// First entity index of the synthetic entities that key quick 3D drawings in the renderer's
/// object cache (counting down from the top of the index space).
const OBJECT_INDEX_TOP: u32 = u32::MAX - 1;

/// Mesh and material caches of the 3D quick layer.
#[derive(Resource, Default)]
pub struct Quick3dCache {
    meshes: HashMap<(u8, u32, u32, u32, u8), Arc<CpuMesh>>,
    materials: HashMap<[u32; 7], Arc<CpuMaterial>>,
    /// Last submitted items. A frozen frame reuses this allocation.
    held_items: Vec<Item3d>,
    /// Last submitted lines.
    held_lines: Vec<(Vec3, Vec3, Color)>,
    /// Allocation address of `held_items` after the last submit.
    held_ptr: usize,
    /// Times [`Self::extend_extracted`] grew its mesh-instance vector.
    extract_allocs: u64,
    /// Reused mesh-instance vector.
    extracted: Vec<ExtractedMesh>,
}

impl Quick3dCache {
    fn mesh(&mut self, shape: Shape3d, reverse: bool) -> Arc<CpuMesh> {
        if self.meshes.len() > CACHE_LIMIT {
            self.meshes.clear();
        }
        self.meshes
            .entry(shape_key(shape, reverse))
            .or_insert_with(|| Arc::new(build_mesh(shape, reverse)))
            .clone()
    }

    fn material(&mut self, item: &Item3d) -> Arc<CpuMaterial> {
        if self.materials.len() > CACHE_LIMIT {
            self.materials.clear();
        }
        let c = item.color;
        let key = [c.r, c.g, c.b, c.a, item.roughness, item.metallic, item.emissive].map(f32::to_bits);
        self.materials
            .entry(key)
            .or_insert_with(|| {
                Arc::new(CpuMaterial {
                    base_color: [c.r, c.g, c.b, c.a],
                    roughness: item.roughness,
                    metallic: item.metallic,
                    emissive: [c.r, c.g, c.b],
                    emissive_intensity: item.emissive,
                    alpha_mode: if c.a < 1.0 { ALPHA_BLEND } else { ALPHA_OPAQUE },
                    ..CpuMaterial::EMPTY
                })
            })
            .clone()
    }

    /// Moves this frame's drawings into the cache.
    ///
    /// A frozen frame keeps the lists already held and does not read or clear `draw`, so
    /// drawings made during the hold wait for the next live frame (the same rule as the 2D
    /// overlay). A live frame moves the draw's lists, so both the draw and the cache keep their
    /// capacity. Returns how many mesh items the frame contains.
    pub fn take_frame(&mut self, draw: &Draw3d, frozen: bool) -> usize {
        if frozen {
            self.held_ptr = self.held_items.as_ptr() as usize;
            return self.held_items.len();
        }
        let mut state = draw.lock_quiet();
        self.held_items.clear();
        self.held_items.append(&mut state.items);
        self.held_lines.clear();
        self.held_lines.append(&mut state.lines);
        self.held_ptr = self.held_items.as_ptr() as usize;
        self.held_items.len()
    }

    /// The items [`Self::take_frame`] kept.
    #[must_use]
    pub fn items(&self) -> &[Item3d] {
        &self.held_items
    }

    /// The lines [`Self::take_frame`] kept.
    #[must_use]
    pub fn lines(&self) -> &[(Vec3, Vec3, Color)] {
        &self.held_lines
    }

    /// Address of the retained item allocation. A frozen re-submit keeps this stable.
    #[must_use]
    pub fn retained_items_ptr(&self) -> usize {
        self.held_ptr
    }

    /// Times [`Self::extend_extracted`] allocated a mesh-instance vector.
    ///
    /// A second extract of the same number of items does not increment this.
    #[must_use]
    pub fn extract_allocs(&self) -> u64 {
        self.extract_allocs
    }

    /// Appends the retained items onto `out`.
    ///
    /// The instance vector is reused. The first call may allocate it; a later call of the same
    /// size does not.
    pub fn extend_extracted(&mut self, out: &mut Vec<ExtractedMesh>) {
        let before = self.extracted.capacity();
        self.fill_extracted();
        if self.extracted.capacity() > before {
            self.extract_allocs = self.extract_allocs.saturating_add(1);
        }
        out.append(&mut self.extracted);
    }

    fn fill_extracted(&mut self) {
        self.extracted.clear();
        let n = self.held_items.len();
        for i in 0..n {
            let Some(id) = OBJECT_INDEX_TOP.checked_sub(i as u32) else {
                continue;
            };
            let Some(entity) = Entity::from_raw_u32(id) else {
                continue;
            };
            let item = self.held_items[i].clone();
            // A mirroring model matrix draws the reversed-winding variant (as `extract` does).
            let mesh = self.mesh(item.shape, reflects(item.model));
            let material = self.material(&item);
            self.extracted.push(ExtractedMesh {
                is_static: false,
                entity,
                mesh,
                material,
                layer: 0,
                model: item.model.to_cols_array_2d(),
                joints: Vec::new(),
                decal_layer: 0,
                uv1: None,
                shadow: ShadowCast::All,
                draw_indices: None,
                lod_key: 0,
                object_key_xor: 0,
                gi_proxy: true,
                lod_dither: 0.0,
            });
        }
    }

    /// The extracted mesh instances of `items` (the renderer's forward path draws them like any
    /// `MeshRenderer`).
    pub fn extract(&mut self, items: &[Item3d]) -> Vec<ExtractedMesh> {
        items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let entity = Entity::from_raw_u32(OBJECT_INDEX_TOP.checked_sub(i as u32)?)?;
                let reverse = reflects(item.model);
                Some(ExtractedMesh {
                    is_static: false,
                    entity,
                    mesh: self.mesh(item.shape, reverse),
                    material: self.material(item),
                    layer: 0,
                    model: item.model.to_cols_array_2d(),
                    joints: Vec::new(),
                    decal_layer: 0,
                    uv1: None,
                    shadow: ShadowCast::All,
                    draw_indices: None,
                    lod_key: 0,
                    object_key_xor: 0,
                    gi_proxy: true,
                    lod_dither: 0.0,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degenerate_grids_draw_nothing_and_thin_cylinder_ends_keep_a_cap() {
        let draw = Draw3d::default();
        draw.grid(Vec3::ZERO, 0, 1.0, Color::RED, Color::GRAY);
        draw.grid(Vec3::ZERO, 1, 1.0, Color::RED, Color::GRAY);
        assert!(draw.is_empty());
        draw.cylinder_ex(Vec3::ZERO, 0.001, 1.0, 1.0, 8);
        let (items, _) = draw.drain();
        match items[0].shape {
            Shape3d::Cylinder { top, bottom, .. } => {
                assert!(top > 0.0, "a positive top radius stays positive");
                assert_eq!(bottom, 1.0);
            }
            other => panic!("expected a cylinder, got {other:?}"),
        }
    }

    #[test]
    fn drawings_record_on_drop_with_builder_settings() {
        let draw = Draw3d::default();
        draw.cube(Vec3::new(1.0, 2.0, 3.0), Vec3::splat(2.0))
            .color(Color::RED)
            .roughness(2.0)
            .metallic(1.0);
        draw.sphere(Vec3::ZERO, 0.5);
        draw.cone(Vec3::ZERO, 1.0, 2.0);
        draw.line(Vec3::ZERO, Vec3::X, Color::WHITE);
        draw.grid(Vec3::ZERO, 4, 1.0, Color::RED, Color::GRAY);
        assert!(!draw.is_empty());
        let (items, lines) = draw.drain();
        assert!(draw.is_empty());
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].color, Color::RED);
        assert_eq!(items[0].roughness, 1.0);
        assert_eq!(items[0].model.w_axis.truncate(), Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(
            items[2].shape,
            Shape3d::Cylinder {
                top: 0.0,
                bottom: 1.0,
                sides: 16
            }
        );
        assert_eq!(lines.len(), 1 + 10);
        // Builder settings land in the record, not just the defaults.
        assert_eq!(items[0].shape, Shape3d::Cube);
        assert_eq!(items[0].model.x_axis.x, 2.0, "cube size scales the model");
        assert_eq!(items[0].model.y_axis.y, 2.0);
        assert_eq!(items[0].metallic, 1.0);
        assert_eq!(
            items[1].shape,
            Shape3d::Sphere {
                rings: 16,
                slices: 16
            }
        );
        assert_eq!(items[1].model.x_axis.x, 0.5, "sphere radius scales the model");
        assert_eq!(items[1].metallic, 0.0, "defaults stay for untouched drawings");
        assert_eq!(lines[0], (Vec3::ZERO, Vec3::X, Color::WHITE));
        // The grid spacing scales its extent.
        let reach = |lines: &[(Vec3, Vec3, Color)]| {
            lines
                .iter()
                .flat_map(|l| [l.0.abs().max_element(), l.1.abs().max_element()])
                .fold(0.0_f32, f32::max)
        };
        let wide = Draw3d::default();
        wide.grid(Vec3::ZERO, 4, 2.0, Color::RED, Color::GRAY);
        let (_, wide_lines) = wide.drain();
        let grid_only = &lines[1..];
        assert!(reach(grid_only) > 0.0);
        assert!(
            (reach(&wide_lines) - 2.0 * reach(grid_only)).abs() < 1e-4,
            "spacing 2 reaches twice as far as spacing 1"
        );
    }

    #[test]
    fn rotation_pivots_a_cylinder_about_its_middle() {
        let draw = Draw3d::default();
        draw.cylinder(Vec3::ZERO, 1.0, 1.0, 2.0)
            .rotation(Quat::from_rotation_x(std::f32::consts::PI));
        draw.cube(Vec3::new(1.0, 2.0, 3.0), Vec3::ONE)
            .rotation(Quat::from_rotation_x(std::f32::consts::PI));
        let (items, _) = draw.drain();
        // Flipped about the middle (0, 1, 0), the base ends up at the old top.
        let base = items[0].model.transform_point3(Vec3::ZERO);
        assert!((base - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-5, "{base:?}");
        let cube = items[1].model.transform_point3(Vec3::ZERO);
        assert!((cube - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-5, "{cube:?}");
    }

    #[test]
    fn grid_caps_slices_and_ignores_non_finite_spacing() {
        let draw = Draw3d::default();
        draw.grid(Vec3::ZERO, u32::MAX, 1.0, Color::RED, Color::GRAY);
        let (_, lines) = draw.drain();
        assert_eq!(lines.len(), (MAX_GRID_SLICES as usize / 2 * 2 + 1) * 2);
        draw.grid(Vec3::ZERO, 4, f32::NAN, Color::RED, Color::GRAY);
        assert!(draw.is_empty());
    }

    #[test]
    fn extraction_shares_meshes_and_materials() {
        let draw = Draw3d::default();
        for i in 0..10 {
            draw.cube(Vec3::X * i as f32, Vec3::ONE).color(Color::BLUE);
        }
        draw.sphere(Vec3::ZERO, 1.0).color(Color::BLUE);
        let (items, _) = draw.drain();
        let mut cache = Quick3dCache::default();
        let meshes = cache.extract(&items);
        assert_eq!(meshes.len(), 11);
        assert!(Arc::ptr_eq(&meshes[0].mesh, &meshes[9].mesh));
        assert!(!Arc::ptr_eq(&meshes[0].mesh, &meshes[10].mesh));
        assert!(Arc::ptr_eq(&meshes[0].material, &meshes[10].material));
        assert_ne!(meshes[0].entity, meshes[1].entity);
        assert_eq!(meshes[3].model[3][0], 3.0);
    }

    #[test]
    fn materials_with_different_colours_are_not_shared() {
        let draw = Draw3d::default();
        draw.cube(Vec3::ZERO, Vec3::ONE).color(Color::RED);
        draw.cube(Vec3::X, Vec3::ONE).color(Color::BLUE);
        let (items, _) = draw.drain();
        let meshes = Quick3dCache::default().extract(&items);
        assert!(Arc::ptr_eq(&meshes[0].mesh, &meshes[1].mesh));
        assert!(!Arc::ptr_eq(&meshes[0].material, &meshes[1].material));
    }
}
