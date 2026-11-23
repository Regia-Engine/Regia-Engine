// SPDX-License-Identifier: MPL-2.0
//! Hidden things stop drawing, and culling writes `ViewVisibility` (`docs/specs/14-scene-and-framework.md`
//! § A1, `docs/specs/37-per-entity-levels-layers-instances.md` § 13, ADR-7200).
//!
//! **The shared filter.** Every extraction path that turns an entity into draws (meshes, skinned
//! meshes, their shadow casters, the visibility-buffer clusters and mask decals that follow the
//! extracted meshes, glass, occlusion proxies, and the instance sets of other crates) reads
//! [`DrawVisibility`] and asks [`is_drawn`] — and so do the frame's directional lights and ambient
//! volumes: a hidden one (its own `Visibility::Hidden` or a hidden ancestor) lights nothing. Both
//! components require `InheritedVisibility`, so a light child of a hidden group is hidden without a
//! `Visibility` of its own. Point, spot and area lights are not filtered yet: a hidden one still
//! lights the frame.
//! An entity whose resolved [`InheritedVisibility`] is
//! false (its own `Visibility::Hidden`, or a hidden ancestor) is left out; an entity without the
//! component is not governed by visibility and draws. [`crate::MeshRenderer`] requires
//! `InheritedVisibility` (and `ViewVisibility`), so a mesh child of a hidden parent is hidden
//! without a `Visibility` of its own. Parked layer members (`LayerParked`) stay out as before.
//!
//! **`ViewVisibility`.** [`update_view_visibility`] runs in `Render` after the extraction chain.
//! An entity is visible when one of its extracted draws passes one view's layer mask and frustum,
//! and the GPU occlusion result does not hold it:
//!
//! - **Bounds.** A rigid draw tests a bounding sphere of its mesh, transformed by the draw's model
//!   matrix. A skinned draw tests its posed bound (ADR-7255): each joint's bind-space box of the
//!   vertices it weights, moved by that joint's palette matrix and unioned (the renderer's own
//!   colour-cull bound, ADR-248), so a limb posed on screen counts though its bind pose is off
//!   screen, and a mesh posed off screen does not count. A skin the bound cannot hold (negative
//!   weights, a weight sum off by more than 1 %, a palette shorter than the joints its vertices
//!   weight, dual-quaternion or eight-influence skinning) passes whole, as before. A morphed draw's
//!   bound grows by the morph's largest displacement.
//! - **Occlusion.** With an [`crate::OcclusionFeedback`] (the runtime's resource), each window
//!   view the renderer culls in two phases has its own result (ADR-7286): a draw in that view's
//!   frustum that the view's cull culled on the newest frame read back is not seen by that view
//!   (`crate::occlusion_feedback`). An entity is visible when any view sees one of its draws: in
//!   split screen a box behind a wall in one half but in plain sight in the other is visible. That
//!   result lags: one frame when the GPU keeps up, never newer. A view without the cull (a texture
//!   view, a frame without it) stays frustum-only for that view only. Draws the cull does not test
//!   (blended, visibility-buffer clusters) are never in a result, so they stay frustum-only.
//!   [`ViewVisibilityPerView`] holds each view's counts.
//!
//! Writes are change-checked, so a steady frame marks nothing changed, and the scratch lists and
//! the per-mesh bounds are reused, so a steady frame allocates nothing.
//!
//! A shadow-only draw (a mesh LOD's shadow draw, or a caster the indoor cell cull kept for the
//! shadow views, ADR-7586) is never on screen and is not tested.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_scene::components::{InheritedVisibility, ViewVisibility};

use crate::frame::Frustum;
use crate::frame::skin_bounds::SkinBindBounds;
use crate::mesh_lod::LodViewport;
use crate::runtime::{CpuMesh, ExtractedFrame, ExtractedMesh, ExtractedTarget, ExtractedView};
use crate::skin::{MAX_JOINTS_PER_SKIN, SkinningMethod};

/// The query item every extraction path reads to decide whether an entity draws. Pass it to
/// [`is_drawn`].
pub type DrawVisibility = Option<&'static InheritedVisibility>;

/// Whether an entity with this resolved visibility draws. No [`InheritedVisibility`] (an entity
/// outside the visibility hierarchy) draws; `InheritedVisibility(false)` does not.
///
/// # Example
///
/// ```
/// use regia_render::visibility::is_drawn;
/// use regia_scene::components::InheritedVisibility;
///
/// assert!(is_drawn(None));
/// assert!(is_drawn(Some(&InheritedVisibility(true))));
/// assert!(!is_drawn(Some(&InheritedVisibility(false))));
/// ```
#[inline]
#[must_use]
pub fn is_drawn(inherited: Option<&InheritedVisibility>) -> bool {
    inherited.is_none_or(|visible| visible.0)
}

/// Counters of the last [`update_view_visibility`] run (agent `render.visible_entities`, tests).
/// Deterministic: counts only, no timing.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ViewVisibilityStats {
    /// Views the frame extracted (each tested).
    pub views: usize,
    /// Extracted draws tested against the views.
    pub draws: usize,
    /// Entities with at least one draw on screen in some view.
    pub visible: usize,
    /// Entities carrying [`ViewVisibility`] (visible or not).
    pub entities: usize,
    /// `ViewVisibility` values that changed this run (0 on a steady frame).
    pub writes: usize,
    /// Entities with a draw in a view's frustum that the GPU occlusion result hid (every such
    /// draw occluded), so not visible. 0 without an [`crate::OcclusionFeedback`] result.
    pub occluded: usize,
    /// Runs since the app started.
    pub runs: u64,
}

/// One view's line of the last [`update_view_visibility`] run ([`ViewVisibilityPerView`], agent
/// `render.visible_entities`). Counts only, no timing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ViewVisibilityViewStats {
    /// The view's index in [`ExtractedFrame::views`].
    pub view: usize,
    /// The camera's texture asset id for a render-to-texture view; `None` for a window view.
    pub texture: Option<u64>,
    /// This view's GPU occlusion result was folded in (a window view the renderer culls in two
    /// phases, ADR-7286); `false` leaves the view frustum-only.
    pub occlusion: bool,
    /// Entities with a draw that passes this view's layer mask and frustum.
    pub in_view: usize,
    /// Of those, the entities whose every such draw this view's occlusion result holds.
    pub occluded: usize,
}

/// Per-view counts of the last [`update_view_visibility`] run, in [`ExtractedFrame::views`]
/// order (ADR-7286). Its list keeps its capacity: a steady frame allocates nothing.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct ViewVisibilityPerView {
    /// One line per view.
    pub views: Vec<ViewVisibilityViewStats>,
}

/// One cached mesh bound: the mesh (weak, so a freed allocation is noticed), its object-space
/// bounding sphere (centre, radius; `None` for an empty or non-finite mesh) and the run that last
/// used it.
type BoundsEntry = (Weak<CpuMesh>, Option<([f32; 3], f32)>, u64);

/// One cached skin bound: the mesh (weak), its per-joint bind-space boxes (`None`: the weights
/// allow no safe bound) and the run that last used it.
type SkinEntry = (Weak<CpuMesh>, Option<SkinBindBounds>, u64);

/// Per-view frustum and layer mask, the lists of shown and occluded entities, and the per-mesh
/// bounds, reused every frame.
#[derive(Default)]
pub struct ViewVisibilityScratch {
    frustums: Vec<(Frustum, u32)>,
    shown: Vec<Entity>,
    /// Entities with an on-screen draw the occlusion result hid.
    occluded_entities: Vec<Entity>,
    /// `(view, entity)`: a draw of the entity passes the view's mask and frustum and the view's
    /// occlusion result does not hold it.
    seen: Vec<(u32, Entity)>,
    /// `(view, entity)`: a draw in the view's frustum the view's occlusion result holds.
    hidden: Vec<(u32, Entity)>,
    /// Per view (by index; never shrinks, so the inner lists keep their capacity): the view's
    /// occlusion result (sorted object keys) and whether one was folded in.
    view_results: Vec<(Vec<u64>, bool)>,
    /// This run's per-view lines, compared with [`ViewVisibilityPerView`] before writing it.
    per_view: Vec<ViewVisibilityViewStats>,
    /// Object-space bounding sphere per mesh allocation (address), kept while the mesh lives and
    /// was drawn last run: computed once per mesh, never per frame.
    bounds: HashMap<usize, BoundsEntry>,
    /// Bind-space joint boxes per skinned mesh allocation (address), kept like `bounds`.
    skins: HashMap<usize, SkinEntry>,
    /// Sorted object keys whose skinning the palette bound does not hold (dual quaternion, eight
    /// influences).
    unboundable: Vec<u64>,
    /// Sorted `(object key, morph displacement)` of this frame's morphed draws.
    morphs: Vec<(u64, f32)>,
    run: u64,
}

/// Where one draw's bound lies this frame.
#[derive(Clone, Copy)]
enum DrawBound {
    /// A world-space sphere.
    Sphere([f32; 3], f32),
    /// No safe bound: passes every frustum.
    Whole,
    /// Draws nothing (empty or non-finite mesh).
    Nothing,
}

impl DrawBound {
    /// Whether the bound lies in `frustum` (layer mask aside).
    fn in_view(self, frustum: &Frustum) -> bool {
        match self {
            Self::Sphere(center, radius) => frustum.intersects_sphere(center, radius),
            Self::Whole => true,
            Self::Nothing => false,
        }
    }
}

/// The bound of `draw` from its cached object-space data: `rigid` is the mesh sphere (rigid
/// draws), `skin` the joint boxes (skinned draws; `None` passes whole), `inflate` the morph
/// displacement in object space.
fn draw_bound(
    draw: &ExtractedMesh,
    rigid: Option<([f32; 3], f32)>,
    skin: Option<&SkinBindBounds>,
    inflate: f32,
) -> DrawBound {
    if !inflate.is_finite() {
        return DrawBound::Whole;
    }
    if draw.joints.is_empty() {
        return match rigid {
            Some((center, radius)) => {
                let model = Mat4::from_cols_array_2d(&draw.model);
                let world = model.transform_point3(Vec3::from_array(center)).to_array();
                DrawBound::Sphere(world, (radius + inflate) * stretch(&model))
            }
            None => DrawBound::Nothing,
        };
    }
    let palette = &draw.joints[..draw.joints.len().min(MAX_JOINTS_PER_SKIN)];
    match skin.and_then(|bind| bind.object_box(palette, inflate)) {
        Some(object) => {
            let (center, radius) = crate::frame::skin_bounds::world_sphere(&draw.model, &object, 0.0);
            if center.iter().all(|v| v.is_finite()) && radius.is_finite() {
                DrawBound::Sphere(center, radius)
            } else {
                DrawBound::Whole
            }
        }
        None => DrawBound::Whole,
    }
}

/// The object key [`ExtractedFrame::frame_scene`] gives `draw` (the renderer's and the occlusion
/// result's identity).
fn object_key(draw: &ExtractedMesh) -> u64 {
    draw.entity.to_bits() ^ draw.object_key_xor
}

/// Whether the skinning `options` move vertices outside the palette bound (dual quaternions
/// blend rotations, eight influences read other joints).
fn unboundable_skin(options: &crate::SkinningOptions) -> bool {
    options.method != SkinningMethod::Linear || options.influences8.is_some()
}

/// Fills `unboundable` and `morphs` (both sorted) from this frame's skinning setup and morphs.
fn frame_side_tables(
    extracted: &ExtractedFrame,
    skinning: Option<&crate::ExtractedSkinning>,
    unboundable: &mut Vec<u64>,
    morphs: &mut Vec<(u64, f32)>,
) {
    unboundable.clear();
    if let Some(skinning) = skinning {
        unboundable.extend(
            skinning
                .keys
                .iter()
                .filter(|(_, options)| unboundable_skin(options))
                .map(|(key, _)| *key),
        );
    }
    unboundable.sort_unstable();
    morphs.clear();
    // The raw weights bound the effective ones (`MorphTargets::effective_weights` only zeroes);
    // a non-finite weight makes the displacement non-finite, which passes whole.
    morphs.extend(
        extracted
            .morphs
            .iter()
            .map(|m| (m.object_key, m.targets.max_displacement(&m.weights))),
    );
    morphs.sort_unstable_by_key(|(key, _)| *key);
}

/// The morph displacement of `key` in the sorted `morphs` (0 without one).
fn morph_inflate(morphs: &[(u64, f32)], key: u64) -> f32 {
    morphs
        .binary_search_by_key(&key, |(k, _)| *k)
        .map_or(0.0, |index| morphs[index].1)
}

/// Object-space bounding sphere: the AABB centre and the farthest vertex from it.
fn sphere_of(mesh: &CpuMesh) -> Option<([f32; 3], f32)> {
    let first = mesh.vertices.first()?;
    let mut lo = Vec3::from_array(first.position);
    let mut hi = lo;
    for vertex in &mesh.vertices {
        let p = Vec3::from_array(vertex.position);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let center = (lo + hi) * 0.5;
    let radius = mesh
        .vertices
        .iter()
        .map(|v| (Vec3::from_array(v.position) - center).length())
        .fold(0.0_f32, f32::max);
    (center.is_finite() && radius.is_finite()).then_some((center.to_array(), radius))
}

/// A bound on how far the model's linear part stretches a unit vector (the Frobenius norm, never
/// below the operator norm), so a transformed sphere always holds the transformed mesh.
fn stretch(model: &Mat4) -> f32 {
    let x = model.x_axis.truncate();
    let y = model.y_axis.truncate();
    let z = model.z_axis.truncate();
    (x.length_squared() + y.length_squared() + z.length_squared()).sqrt()
}

/// The frustum `view` draws with, at the window size (`window`) or the texture's own size. Below
/// 16:9 a horizontal-plus profile widens the drawn FOV, so the test takes that wider frustum
/// (conservative, as the mesh LOD's on-screen test does).
pub(crate) fn view_frustum(view: &ExtractedView, window: [u32; 2], texture: Option<[u32; 2]>) -> Frustum {
    let [width, height] = match view.target {
        ExtractedTarget::Window => [
            (window[0] as f32 * view.viewport[2]).max(1.0),
            (window[1] as f32 * view.viewport[3]).max(1.0),
        ],
        ExtractedTarget::Texture { .. } => {
            let size = texture.unwrap_or(window);
            [size[0].max(1) as f32, size[1].max(1) as f32]
        }
    };
    let aspect = (width / height).max(1.0e-3);
    let eye = Vec3::from_array(view.camera.eye);
    let target = Vec3::from_array(view.camera.target);
    let view_m = crate::view_math::look_at_view(eye, target, Vec3::Y);
    let fov_y = crate::output::horizontal_plus_fov_y(view.camera.fov_y, aspect);
    let (fov, near, far) = crate::view_math::usable_projection(fov_y, view.camera.near, view.camera.far);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov, aspect, near, far);
    Frustum::from_view_proj(&(proj * view_m))
}

/// The frame's skinning setup, the occlusion readback, the device it polls and the per-view
/// report (each optional).
type FrameInputs<'w> = (
    Option<Res<'w, crate::ExtractedSkinning>>,
    Option<Res<'w, crate::OcclusionFeedback>>,
    Option<Res<'w, crate::RenderDevice>>,
    Option<ResMut<'w, ViewVisibilityPerView>>,
);

/// Counts the entities in `pairs` (sorted, unique `(view, entity)`) not also in `minus` (same
/// order), per view, into `out[view]` via `add`.
fn per_view_counts(
    pairs: &[(u32, Entity)],
    minus: Option<&[(u32, Entity)]>,
    out: &mut [ViewVisibilityViewStats],
    add: fn(&mut ViewVisibilityViewStats),
) {
    for pair in pairs {
        if minus.is_some_and(|minus| minus.binary_search(pair).is_ok()) {
            continue;
        }
        if let Some(line) = out.get_mut(pair.0 as usize) {
            add(line);
        }
    }
}

/// `Render`, after the extraction chain: sets every [`ViewVisibility`] to whether one of the
/// entity's extracted draws passes one view's layer mask and frustum this frame, with skinned
/// draws at their posed bound, and is not held by that view's GPU occlusion result (module
/// docs). Writes [`ViewVisibilityStats`] and [`ViewVisibilityPerView`] when present.
/// Change-checked and allocation-free in steady state.
pub fn update_view_visibility(
    extracted: Option<Res<ExtractedFrame>>,
    viewport: Option<Res<LodViewport>>,
    mut entities: Query<(Entity, &mut ViewVisibility)>,
    mut scratch: Local<ViewVisibilityScratch>,
    stats: Option<ResMut<ViewVisibilityStats>>,
    (skinning, feedback, device, per_view): FrameInputs,
) {
    let _span = tracing::info_span!("view_visibility").entered();
    let scratch = &mut *scratch;
    scratch.run = scratch.run.wrapping_add(1);
    let run = scratch.run;
    scratch.frustums.clear();
    scratch.shown.clear();
    scratch.occluded_entities.clear();
    scratch.seen.clear();
    scratch.hidden.clear();
    scratch.per_view.clear();
    // The newest occlusion result whose map already completed: a non-blocking poll lets a
    // finished readback land, nothing waits for the GPU (ADR-7255).
    if let Some(feedback) = feedback.as_deref() {
        if feedback.in_flight()
            && let Some(device) = device.as_deref()
        {
            let _ = device.tracked().poll(wgpu::PollType::Poll);
        }
        // A view that went away takes its ring with it: its result never speaks for another
        // camera that takes its index later (ADR-7286).
        if let Some(extracted) = extracted.as_deref() {
            feedback.retain_views(extracted.views.len());
        }
        feedback.collect();
    }
    let window = viewport.as_deref().map_or([1920, 1080], |v| [v.width, v.height]);
    let mut draws = 0;
    if let Some(extracted) = extracted.as_deref() {
        if scratch.view_results.len() < extracted.views.len() {
            scratch
                .view_results
                .resize_with(extracted.views.len(), Default::default);
        }
        for (index, view) in extracted.views.iter().enumerate() {
            let texture = match view.target {
                ExtractedTarget::Texture { asset_id } => {
                    extracted.texture_view_settings.get(&asset_id).map(|s| s.size)
                }
                ExtractedTarget::Window => None,
            };
            scratch
                .frustums
                .push((view_frustum(view, window, texture), view.layer_mask));
            // Each window view the renderer culls in two phases speaks for itself; a texture
            // view never culls so, and stays frustum-only (ADR-7286).
            let (keys, folded) = &mut scratch.view_results[index];
            keys.clear();
            *folded = false;
            if let (ExtractedTarget::Window, Some(feedback), Ok(view_id)) =
                (view.target, feedback.as_deref(), u32::try_from(index))
                && feedback.has_view(view_id)
            {
                feedback.occluded_view_into(view_id, keys);
                *folded = true;
            }
            scratch.per_view.push(ViewVisibilityViewStats {
                view: index,
                texture: match view.target {
                    ExtractedTarget::Texture { asset_id } => Some(asset_id),
                    ExtractedTarget::Window => None,
                },
                occlusion: *folded,
                in_view: 0,
                occluded: 0,
            });
        }
        frame_side_tables(
            extracted,
            skinning.as_deref(),
            &mut scratch.unboundable,
            &mut scratch.morphs,
        );
        if !scratch.frustums.is_empty() {
            for draw in &extracted.meshes {
                draws += 1;
                // A shadow-only draw (a shadow LOD, or a caster the cell cull kept for the shadow
                // views, ADR-7586) is never on screen.
                if crate::frame::is_shadow_lod_key(draw.lod_key) {
                    continue;
                }
                let layer_bit = if draw.layer < 32 { 1u32 << draw.layer } else { 0 };
                if scratch.frustums.iter().all(|(_, mask)| mask & layer_bit == 0) {
                    continue;
                }
                let key = object_key(draw);
                let inflate = morph_inflate(&scratch.morphs, key);
                let address = Arc::as_ptr(&draw.mesh) as usize;
                let bound = if draw.joints.is_empty() {
                    let entry = scratch
                        .bounds
                        .entry(address)
                        .or_insert_with(|| (Arc::downgrade(&draw.mesh), sphere_of(&draw.mesh), run));
                    // A freed mesh whose address came back: measure the new one.
                    if !std::ptr::eq(entry.0.as_ptr(), Arc::as_ptr(&draw.mesh)) || entry.0.strong_count() == 0
                    {
                        *entry = (Arc::downgrade(&draw.mesh), sphere_of(&draw.mesh), run);
                    }
                    entry.2 = run;
                    draw_bound(draw, entry.1, None, inflate)
                } else if scratch.unboundable.binary_search(&key).is_ok() {
                    DrawBound::Whole
                } else {
                    let entry = scratch.skins.entry(address).or_insert_with(|| {
                        (
                            Arc::downgrade(&draw.mesh),
                            SkinBindBounds::from_vertices(&draw.mesh.vertices),
                            run,
                        )
                    });
                    if !std::ptr::eq(entry.0.as_ptr(), Arc::as_ptr(&draw.mesh)) || entry.0.strong_count() == 0
                    {
                        *entry = (
                            Arc::downgrade(&draw.mesh),
                            SkinBindBounds::from_vertices(&draw.mesh.vertices),
                            run,
                        );
                    }
                    entry.2 = run;
                    draw_bound(draw, None, entry.1.as_ref(), inflate)
                };
                for (index, ((frustum, mask), (occluded, _))) in
                    scratch.frustums.iter().zip(&scratch.view_results).enumerate()
                {
                    if mask & layer_bit == 0 || !bound.in_view(frustum) {
                        continue;
                    }
                    let pair = (index as u32, draw.entity);
                    if occluded.binary_search(&key).is_ok() {
                        scratch.hidden.push(pair);
                    } else {
                        scratch.seen.push(pair);
                    }
                }
            }
        }
    }
    scratch.seen.sort_unstable();
    scratch.seen.dedup();
    scratch.hidden.sort_unstable();
    scratch.hidden.dedup();
    // Visible: some view sees one of its draws. Occluded: in some view's frustum, seen by none.
    scratch
        .shown
        .extend(scratch.seen.iter().map(|(_, entity)| *entity));
    scratch.shown.sort_unstable();
    scratch.shown.dedup();
    scratch
        .occluded_entities
        .extend(scratch.hidden.iter().map(|(_, entity)| *entity));
    scratch.occluded_entities.sort_unstable();
    scratch.occluded_entities.dedup();
    per_view_counts(&scratch.seen, None, &mut scratch.per_view, |line| {
        line.in_view += 1
    });
    per_view_counts(
        &scratch.hidden,
        Some(&scratch.seen),
        &mut scratch.per_view,
        |line| {
            line.in_view += 1;
            line.occluded += 1;
        },
    );
    if let Some(mut per_view) = per_view
        && per_view.views != scratch.per_view
    {
        per_view.views.clone_from(&scratch.per_view);
    }
    let shown = &scratch.shown;
    let occluded = scratch
        .occluded_entities
        .iter()
        .filter(|entity| shown.binary_search(entity).is_err())
        .count();
    // Meshes no draw used this run, or that nothing holds any more, give their entry back.
    scratch
        .bounds
        .retain(|_, (mesh, _, seen)| *seen == run && mesh.strong_count() > 0);
    scratch
        .skins
        .retain(|_, (mesh, _, seen)| *seen == run && mesh.strong_count() > 0);
    let mut writes = 0;
    let mut count = 0;
    for (entity, mut view_visibility) in &mut entities {
        count += 1;
        let visible = scratch.shown.binary_search(&entity).is_ok();
        // Read through `Deref` first: only a real change marks the component changed.
        if view_visibility.0 != visible {
            view_visibility.0 = visible;
            writes += 1;
        }
    }
    if let Some(mut stats) = stats {
        let runs = stats.runs.wrapping_add(1);
        *stats = ViewVisibilityStats {
            views: scratch.frustums.len(),
            draws,
            visible: scratch.shown.len(),
            entities: count,
            writes,
            occluded,
            runs,
        };
    }
}

/// What one camera sees of the last extracted frame: how many entities have a draw that passes
/// its layer mask and frustum (skinned draws at their posed bound) and that the occlusion result
/// does not hold, and up to `limit` of them ascending by entity index (agent
/// `render.visible_entities {camera}`). The test is the one [`update_view_visibility`] runs per
/// view, at the [`LodViewport`] size (a texture camera at its texture's size). The occlusion
/// result of the frame's window view this camera drew (same placement, viewport and layers)
/// applies, as there (ADR-7286); a texture camera, or a camera the frame did not draw, is
/// frustum-only. `None` when `camera` has no `Camera` and `GlobalTransform`, or no forward
/// direction.
#[must_use]
pub fn visible_to_camera(world: &World, camera: Entity, limit: usize) -> Option<(usize, Vec<Entity>)> {
    let entity = world.get_entity(camera).ok()?;
    let (cam, transform) = (
        entity.get::<crate::Camera>()?,
        entity.get::<regia_scene::GlobalTransform>()?,
    );
    let target = match &cam.target {
        crate::RenderTarget::Window => ExtractedTarget::Window,
        crate::RenderTarget::Texture { asset_id } => ExtractedTarget::Texture { asset_id: *asset_id },
    };
    let view = crate::runtime::extract_view(cam, transform, target, crate::PostSettings::default())?;
    let window = world
        .get_resource::<LodViewport>()
        .map_or([1920, 1080], |v| [v.width, v.height]);
    let texture = entity.get::<crate::TextureViewSettings>().map(|s| s.size);
    let frustum = view_frustum(&view, window, texture);
    let mut shown: Vec<Entity> = Vec::new();
    if let Some(extracted) = world.get_resource::<ExtractedFrame>() {
        let (mut unboundable, mut morphs) = (Vec::new(), Vec::new());
        frame_side_tables(
            extracted,
            world.get_resource::<crate::ExtractedSkinning>(),
            &mut unboundable,
            &mut morphs,
        );
        // The extracted window view this camera drew, by placement, viewport and layers: its
        // index names its occlusion ring (ADR-7286).
        let view_id = extracted
            .views
            .iter()
            .position(|drawn| {
                matches!(drawn.target, ExtractedTarget::Window)
                    && drawn.target == view.target
                    && drawn.camera.eye == view.camera.eye
                    && drawn.camera.target == view.camera.target
                    && drawn.viewport == view.viewport
                    && drawn.layer_mask == view.layer_mask
            })
            .and_then(|index| u32::try_from(index).ok());
        let feedback = world.get_resource::<crate::OcclusionFeedback>().zip(view_id);
        for draw in &extracted.meshes {
            let layer_bit = if draw.layer < 32 { 1u32 << draw.layer } else { 0 };
            if view.layer_mask & layer_bit == 0 || crate::frame::is_shadow_lod_key(draw.lod_key) {
                continue;
            }
            let key = object_key(draw);
            let inflate = morph_inflate(&morphs, key);
            let bound = if draw.joints.is_empty() {
                draw_bound(draw, sphere_of(&draw.mesh), None, inflate)
            } else if unboundable.binary_search(&key).is_ok() {
                DrawBound::Whole
            } else {
                draw_bound(
                    draw,
                    None,
                    SkinBindBounds::from_vertices(&draw.mesh.vertices).as_ref(),
                    inflate,
                )
            };
            if bound.in_view(&frustum) && !feedback.is_some_and(|(f, view)| f.is_occluded_in(view, key)) {
                shown.push(draw.entity);
            }
        }
    }
    shown.sort_unstable_by_key(|entity| entity.index_u32());
    shown.dedup();
    let total = shown.len();
    shown.truncate(limit);
    Some((total, shown))
}

/// Up to `limit` entities whose [`ViewVisibility`] is true, ascending by entity index, plus how
/// many there are in all (agent `render.visible_entities`). `limit` bounds the list, not the count.
#[must_use]
pub fn visible_entities(world: &mut World, limit: usize) -> (usize, Vec<Entity>) {
    let mut query = world.query::<(Entity, &ViewVisibility)>();
    let mut total = 0;
    let mut listed = Vec::with_capacity(limit.min(1024));
    for (entity, view_visibility) in query.iter(world) {
        if !view_visibility.0 {
            continue;
        }
        total += 1;
        listed.push(entity);
    }
    listed.sort_unstable_by_key(|entity| entity.index_u32());
    listed.truncate(limit);
    (total, listed)
}
