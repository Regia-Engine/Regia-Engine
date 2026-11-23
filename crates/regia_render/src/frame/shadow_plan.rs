// SPDX-License-Identifier: MPL-2.0
//! Shadow planning on the CPU: sun cascades, the local-light atlas tiles, caster lists and
//! their caches (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

pub(super) fn shadow_model(model: [[f32; 4]; 4], shadow: ShadowCast) -> [[f32; 4]; 4] {
    match shadow {
        ShadowCast::Local {
            shadow_scale,
            shadow_lift,
            ..
        } => {
            let mut m = model;
            // A factor that is not finite and above 0 draws the shadow as drawn (a negative one
            // mirrored the caster; the visibility path uses the same rule).
            if shadow_scale != 1.0 && shadow_scale.is_finite() && shadow_scale > 0.0 {
                for column in m.iter_mut().take(3) {
                    for v in column.iter_mut().take(3) {
                        *v *= shadow_scale;
                    }
                }
            }
            if shadow_lift.is_finite() {
                m[3][1] += shadow_lift;
            }
            m
        }
        _ => model,
    }
}

/// Paged-shadow identity of a caster: its object and the mesh it draws.
///
/// The page cache compares a caster's bounds with last frame's under the same id. A mesh
/// swapped under the same object key that fits the old bounds (an entity's mesh replaced,
/// a reloaded asset, a LOD switch) would keep the old shadow; with the mesh in the id it is
/// a new caster, and its old and new pages are redrawn. A [`ShadowCast::Local`] caster with
/// `lod_switch_keeps_pages` keeps the object alone (scatter LOD chains: see that field).
pub(super) fn shadow_caster_id(object: u64, mesh: u64, shadow: ShadowCast) -> u64 {
    match shadow {
        ShadowCast::Local {
            lod_switch_keeps_pages: true,
            ..
        } => object,
        _ => object ^ mesh.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29),
    }
}

/// [`shadow_caster_id`] of a caster whose mesh bytes were rewritten in place `content_rev` times
/// under the same key (`crate::note_mesh_content_changed`, ADR-5594). Revision 0 (never
/// rewritten: every mesh but an in-place edit) keeps `id`. Any other revision is a new caster,
/// so the page cache redraws the old shape's pages and the new one's, as for a mesh swap.
pub(super) fn caster_content_id(id: u64, content_rev: u64) -> u64 {
    if content_rev == 0 {
        id
    } else {
        id ^ content_rev.wrapping_mul(0xD6E8_FEB8_6659_FD93).rotate_left(17)
    }
}

/// Sun-cascade mask of a caster at `distance` (bounding-sphere centre to eye) with `radius`.
/// Bit `i` = cascade `i`. `splits[i]` is cascade `i`'s far distance; receivers in the last
/// 15 % of a cascade blend into the next one (`sample_csm`), so that band counts for both.
pub(super) fn cascade_mask(shadow: ShadowCast, distance: f32, radius: f32, splits: &[f32]) -> u8 {
    let all = if splits.len() >= 8 {
        u8::MAX
    } else {
        (1u8 << splits.len()) - 1
    };
    let (reach, max) = match shadow {
        ShadowCast::All => return all,
        ShadowCast::Off => return 0,
        ShadowCast::Local {
            reach, max_cascades, ..
        } => (reach.max(0.0), max_cascades),
    };
    let lower = distance - radius - reach;
    let upper = distance + radius + reach;
    let mut first = None;
    let mut last = 0usize;
    let mut home = None;
    for i in 0..splits.len() {
        let near = if i == 0 { 0.0 } else { splits[i - 1] };
        let prev_near = if i < 2 { 0.0 } else { splits[i - 2] };
        let lo = if i == 0 {
            0.0
        } else {
            near - 0.15 * (near - prev_near)
        };
        let hi = splits[i];
        if upper >= lo && lower <= hi {
            first.get_or_insert(i);
            last = i;
        }
        if home.is_none() && distance <= hi {
            home = Some(i);
        }
    }
    let Some(first) = first else { return 0 };
    let home = home.unwrap_or(last).clamp(first, last);
    // Window of `max` cascades from the caster's own cascade outward (finer receivers first
    // when the window has to shrink toward the camera).
    let max = usize::from(max);
    if max == 0 {
        return 0;
    }
    let start = home.min(last.saturating_sub(max - 1)).max(first);
    let end = (start + max - 1).min(last);
    let mut mask = 0u8;
    for i in start..=end {
        mask |= 1 << i;
    }
    mask
}

/// What a light face's caster list was built for ([`CasterListCache::faces`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FaceStamp {
    /// The face's view-projection.
    pub(super) view_proj: [[f32; 4]; 4],
    /// The light's position.
    pub(super) center: [f32; 3],
    /// The light's radius.
    pub(super) radius: f32,
}

/// One shadowed light's atlas tiles and influence sphere ([`ShadowPlan::light_faces`]).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LightFaces {
    /// Its tiles in [`ShadowPlan::tiles`].
    pub(super) tiles: std::ops::Range<usize>,
    /// Light position, world metres.
    pub(super) center: [f32; 3],
    /// Point radius or spot range: no light reaches past it.
    pub(super) radius: f32,
}

/// How far past a light's radius a caster still enters its faces' lists: the receivers inside the
/// radius sample a filter kernel and a normal offset beyond their own texel (ADR-7986).
pub(super) const FACE_CULL_MARGIN: f32 = 0.1;

/// `true` when a caster with world box `aabb` can draw into the atlas face `view_proj` of the light
/// at `center` with influence `radius`: the box reaches the light's sphere (with
/// [`FACE_CULL_MARGIN`]) and is not wholly outside one of the face's four side planes. Outside a
/// side plane the rasteriser clips every triangle of the caster, so leaving it out of the face's
/// list changes no texel. The near and far planes are not tested (a depth-clamped pipeline still
/// draws what lies before the near plane). An unbounded caster (`None`) is always drawn.
pub(super) fn face_sees_caster(
    aabb: Option<crate::paged_shadow::Aabb3>,
    center: [f32; 3],
    radius: f32,
    view_proj: &[[f32; 4]; 4],
) -> bool {
    let Some(aabb) = aabb else {
        return true;
    };
    let reach = radius * (1.0 + FACE_CULL_MARGIN) + 0.25;
    let near = aabb.clamp_point(center);
    let d2: f32 = (0..3)
        .map(|k| (near[k] - center[k]) * (near[k] - center[k]))
        .sum();
    if d2.is_nan() || d2 > reach * reach {
        return false;
    }
    // Rows of the column-major matrix: clip = M · p.
    let row = |i: usize| [view_proj[0][i], view_proj[1][i], view_proj[2][i], view_proj[3][i]];
    let (x, y, w) = (row(0), row(1), row(3));
    // Inside means w ± x >= 0 and w ± y >= 0 (wgpu clip space).
    for plane in [
        [w[0] - x[0], w[1] - x[1], w[2] - x[2], w[3] - x[3]],
        [w[0] + x[0], w[1] + x[1], w[2] + x[2], w[3] + x[3]],
        [w[0] - y[0], w[1] - y[1], w[2] - y[2], w[3] - y[3]],
        [w[0] + y[0], w[1] + y[1], w[2] + y[2], w[3] + y[3]],
    ] {
        // The box corner furthest along the plane normal: if even it is outside, all are.
        let best = plane[3]
            + plane[..3]
                .iter()
                .zip(aabb.min.iter().zip(&aabb.max))
                .map(|(&n, (&lo, &hi))| n * if n >= 0.0 { hi } else { lo })
                .sum::<f32>();
        let scale = plane[0].abs() + plane[1].abs() + plane[2].abs() + plane[3].abs();
        if best < -1.0e-5 * scale.max(1.0) {
            return false;
        }
    }
    true
}

pub(super) struct PlannedTile {
    pub(super) gpu: ShadowTileGpu,
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) size: u32,
}

/// One shadowed light waiting for an atlas rect. Faces share a tile size.
pub(super) struct PendingShadow {
    pub(super) index: usize,
    pub(super) views: Vec<[[f32; 4]; 4]>,
    pub(super) importance: f32,
    /// `2·tan(fov/2)`. Divided by the packed tile edge for the world texel.
    pub(super) texel_slope: f32,
    pub(super) inv_depth: f32,
}

pub(super) struct ShadowJob {
    pub(super) slot: usize,
    pub(super) kind: u8,
    pub(super) rect: [u32; 4],
    /// Sun page `(x, y)` of a partial cascade redraw; `None` = the whole view.
    pub(super) page: Option<(u32, u32)>,
    /// Clear this rectangle to far depth before redrawing casters into it.
    pub(super) page_clear: bool,
}

/// `(cascade, page)` key of a cached caster list. `None` page = the whole cascade.
pub(super) type ShadowPageKey = (u32, Option<(u32, u32)>);

/// CPU cache of the per-shadow-job caster lists.
///
/// Building one list projects every caster into a cascade and filters the result against a page,
/// so on a frame with thousands of scattered rocks it is a measurable per-frame CPU term
/// (ADR-047). The lists are cached per shadow page and reused until the caster set (draw order,
/// bounds and cascade masks), which casters draw as clusters instead (the lists leave those out,
/// ADR-3915), or a cascade matrix changes; a held camera reuses every list.
#[derive(Default)]
pub(super) struct CasterListCache {
    /// Draw indices of the shadow casters, in the order the lists were built for.
    pub(super) shadow_ix: Vec<usize>,
    /// Per-caster `(world aabb, cascade mask)` parallel to [`Self::shadow_ix`].
    pub(super) caster_info: Vec<(Option<crate::paged_shadow::Aabb3>, u8)>,
    /// Per caster, parallel to [`Self::shadow_ix`]: whether it casts as visibility-buffer clusters,
    /// so the lists leave it out. The path switching off must bring these casters back.
    pub(super) cluster_casters: Vec<bool>,
    /// Light view-projection per cascade the lists were built for.
    pub(super) cascade_vp: Vec<[[f32; 4]; 4]>,
    /// `(cascade, page)` → draw indices of that page's casters. `None` page = whole cascade.
    /// Shared with the frame's job lists, so a cache hit copies nothing (ADR-385).
    pub(super) lists: std::collections::HashMap<ShadowPageKey, std::sync::Arc<[usize]>>,
    /// Object-space centroid of a skinned (unbounded) mesh, keyed by mesh key.
    ///
    /// [`Self::reset`] drops the caster lists and leaves this map: a moving camera still
    /// sorts the same mesh without walking its vertices. Rigid meshes use [`MeshBounds`]
    /// and never enter the map.
    centroids: std::collections::HashMap<u64, CachedCentroid>,
    /// Cascade-0 rectangle the near-tile caster list was built for.
    ///
    /// The list itself lives in [`Self::lists`] under the near-tile key. A different
    /// rectangle drops that entry; [`Self::reset`] drops every list and this stamp stays,
    /// so the next frame builds the list once for the new casters.
    pub(super) near_rect: Option<([f32; 2], [f32; 2])>,
    /// Precipitation view the precip-tile caster list was built for.
    pub(super) precip_plan: Option<super::weather::PrecipTilePlan>,
    /// Atlas tile → the face its list was built for and the list (ADR-7986). [`Self::reset`]
    /// drops them with the other lists.
    pub(super) faces: std::collections::HashMap<usize, (FaceStamp, std::sync::Arc<[usize]>)>,
    /// Vertex walks [`blended_sort_depth`] took because a mesh had no cached centre.
    pub(super) centroid_walks: u64,
}

/// Object-space centroid remembered for one mesh key.
struct CachedCentroid {
    count: usize,
    first: [f32; 3],
    last: [f32; 3],
    center: [f32; 3],
}

impl CasterListCache {
    /// `true` when the cached lists no longer describe `shadow_ix` / `caster_info` / `vps`, or
    /// `clustered` (by draw index) says another caster draws as clusters now.
    pub(super) fn stale(
        &self,
        shadow_ix: &[usize],
        caster_info: &[(Option<crate::paged_shadow::Aabb3>, u8)],
        vps: &[[[f32; 4]; 4]],
        clustered: impl Fn(usize) -> bool,
    ) -> bool {
        self.shadow_ix.as_slice() != shadow_ix
            || self.caster_info.as_slice() != caster_info
            || self
                .cluster_casters
                .iter()
                .zip(shadow_ix)
                .any(|(&was, &index)| was != clustered(index))
            || self.cascade_vp.len() != vps.len()
            || self
                .cascade_vp
                .iter()
                .zip(vps)
                .any(|(a, b)| !crate::paged_shadow::matrices_match(*a, *b))
    }

    /// Drops every cached list and re-seeds the cache with the current inputs.
    pub(super) fn reset(
        &mut self,
        shadow_ix: &[usize],
        caster_info: &[(Option<crate::paged_shadow::Aabb3>, u8)],
        vps: &[[[f32; 4]; 4]],
        clustered: impl Fn(usize) -> bool,
    ) {
        // A moving camera resets every frame: refill the buffers in place (ADR-385).
        self.lists.clear();
        self.faces.clear();
        self.shadow_ix.clear();
        self.shadow_ix.extend_from_slice(shadow_ix);
        self.caster_info.clear();
        self.caster_info.extend_from_slice(caster_info);
        self.cluster_casters.clear();
        self.cluster_casters
            .extend(shadow_ix.iter().map(|&index| clustered(index)));
        self.cascade_vp.clear();
        self.cascade_vp.extend_from_slice(vps);
        // `centroids` stays: the sort key does not depend on the cascade the lists were built for.
    }
}

pub(super) fn world_caster_aabb(bounds: MeshBounds, model: [[f32; 4]; 4]) -> crate::paged_shadow::Aabb3 {
    let m = regia_core::glam::Mat4::from_cols_array_2d(&model);
    let center = m.transform_point3(regia_core::glam::Vec3::from_array(bounds.center));
    let scale = m.x_axis.length().max(m.y_axis.length()).max(m.z_axis.length());
    crate::paged_shadow::Aabb3::from_center_half(center.to_array(), [bounds.radius * scale; 3])
}

pub(super) struct ShadowPlan {
    pub(super) tiles: Vec<PlannedTile>,
    pub(super) light_tile: Vec<i32>,
    /// Indices into the uploaded light list, in atlas-assignment order.
    /// One entry per shadowed light, not per cube face. The atlas cache uses
    /// this list so a dirty light index still names `tiles[index]` the way the
    /// paged-shadow test expects (a dirty point redraws tile 0 only).
    pub(super) shadowed: Vec<usize>,
    /// Parallel to [`Self::shadowed`]: each shadowed light's tiles (a point light's six cube
    /// faces, a spot's one) and its influence sphere. A dirty light redraws only the faces its
    /// moved casters reach, and each face draws only the casters it can see (ADR-7986). Empty
    /// when the plan does not name its lights' tiles (then a dirty index is a tile index and
    /// every tile draws every caster, as before).
    pub(super) light_faces: Vec<LightFaces>,
    pub(super) atlas_size: u32,
    pub(super) csm: crate::ShadowUniforms,
    /// Index in [`Self::tiles`] of the near sun tile (ADR-164), when the profile has one and
    /// the atlas could hold it. The frame drops it again when no mesh is in its range.
    pub(super) near_tile: Option<usize>,
    /// The near sun tile's bounding sphere (centre, radius) in world metres.
    pub(super) near_sphere: Option<([f32; 3], f32)>,
    /// Index in [`Self::tiles`] of the precipitation occlusion tile (spec 99 § 5.6): after the
    /// local lights' tiles, before the near sun tile.
    pub(super) precip_tile: Option<usize>,
    /// Where that tile looks.
    pub(super) precip_plan: Option<super::weather::PrecipTilePlan>,
    /// Graph clock `(time, previous time, wind x, wind z)`. The frame fills this after planning.
    pub(super) clock: [f32; 4],
    /// Graphs read [`Self::clock`] instead of material slot 0.
    pub(super) clock_live: bool,
}

/// Each fog light slot's spot shadow tile (ADR-1365 phase 2). A fog spot whose uploaded light
/// (same position, same direction) casts into the local atlas gets that light's tile; points
/// (cube tiles) and lights without a tile stay unshadowed in the fog.
pub(super) fn fog_light_shadows(
    fog_lights: &[crate::FogLight],
    uploaded: &[crate::LabLocalLight],
    plan: &ShadowPlan,
) -> [crate::outdoor::FogLightShadow; 4] {
    let mut rows = [[[0.0; 4]; 6]; 4];
    for (slot, fog) in fog_lights.iter().take(crate::FOG_LIGHT_SLOTS).enumerate() {
        let Some(cone) = fog.cone else {
            continue;
        };
        let axis = regia_core::glam::Vec3::from_array(cone.direction);
        let found = uploaded.iter().position(|light| match light {
            crate::LabLocalLight::Spot(spot) => {
                spot.cast_shadows
                    && spot.position == fog.position
                    && regia_core::glam::Vec3::from_array(spot.direction)
                        .normalize_or_zero()
                        .dot(axis)
                        > 0.9999
            }
            crate::LabLocalLight::Point(_) => false,
        });
        let tile = found
            .and_then(|index| plan.light_tile.get(index))
            .and_then(|&tile| usize::try_from(tile).ok())
            .and_then(|tile| plan.tiles.get(tile));
        if let Some(tile) = tile {
            let g = &tile.gpu;
            rows[slot] = [
                g.view_proj[0],
                g.view_proj[1],
                g.view_proj[2],
                g.view_proj[3],
                g.uv,
                [g.bias[0], g.bias[1], g.bias[2], 1.0],
            ];
        }
    }
    rows
}

pub(super) fn light_casts(light: &crate::LabLocalLight) -> bool {
    match light {
        crate::LabLocalLight::Point(light) => light.cast_shadows,
        crate::LabLocalLight::Spot(light) => light.cast_shadows,
    }
}

pub(super) fn look_proj(eye: [f32; 3], dir: [f32; 3], up: [f32; 3], fov_y: f32, far: f32) -> [[f32; 4]; 4] {
    let eye_v = regia_core::glam::Vec3::from_array(eye);
    let view = look_at_view(
        eye_v,
        eye_v + regia_core::glam::Vec3::from_array(dir),
        regia_core::glam::Vec3::from_array(up),
    );
    let proj = perspective(fov_y, 1.0, 0.05, far.max(0.2));
    (proj * view).to_cols_array_2d()
}

/// Far end of the sun cascades for a view's far plane: capped at [`CSM_MAX_DISTANCE`],
/// and [`CSM_MAX_FAR`] for an infinite far plane.
pub(crate) fn csm_far(view_far: f32) -> f32 {
    if view_far.is_finite() {
        view_far.min(CSM_MAX_DISTANCE)
    } else {
        CSM_MAX_FAR
    }
}

/// View distance the cascade volumetric fog samples should reach (metres): fog reads the
/// first cascade whose split is at least this far ([`fog_cascade_index`]).
/// Widest spot half-angle (radians) one shadow tile covers: the tile's field of view is twice this
/// (about 149 degrees). A shadowed spot's lit cone is clamped to it so no lit region is unshadowed.
pub(super) const SPOT_SHADOW_MAX_HALF_ANGLE: f32 = 1.3;

pub(super) const FOG_SHADOW_RANGE: f32 = 150.0;

/// The cascade volumetric fog samples: the first whose split reaches [`FOG_SHADOW_RANGE`],
/// else the last one drawn.
pub(super) fn fog_cascade_index(csm: &crate::ShadowUniforms, views: usize) -> usize {
    let count = (csm.count[0] as usize)
        .clamp(1, csm.cascades.len())
        .min(views.max(1));
    csm.cascades[..count]
        .iter()
        .position(|c| c.split_distance >= FOG_SHADOW_RANGE)
        .unwrap_or(count - 1)
}

/// How far up-sun a sun cascade reaches for casters at most (metres). A cascade's light
/// eye sits `max(radius, reach)` up-sun of its centre, `reach` being this capped at the
/// view's shadow range: a 40 m near cascade still takes the shadow of a mesa 1 km toward
/// a low sun, which an eye one radius out would clip.
pub(super) const CSM_CASTER_REACH: f32 = 2000.0;

/// Radius factor of the near sun tile over its frustum slice's sphere with paged shadows: the
/// slack its kept centre may drift by before the tile moves and redraws (ADR-164).
pub(super) const NEAR_TILE_SLACK: f32 = 1.25;

/// Up-sun distance from a cascade's centre to its light eye (see [`CSM_CASTER_REACH`]).
pub(crate) fn cascade_back(radius: f32, shadow_range: f32) -> f32 {
    radius.max(shadow_range.min(CSM_CASTER_REACH))
}

/// Orthographic sun cascade: `radius` square around `center`, the light eye `back` up-sun
/// of it, depth `back + 2 radius`.
pub(crate) fn ortho_light_vp(
    dir: [f32; 3],
    center: [f32; 3],
    radius: f32,
    back: f32,
    snap_texels: u32,
) -> [[f32; 4]; 4] {
    let dir_v = regia_core::glam::Vec3::from_array(dir).normalize_or_zero();
    let dir_v = if dir_v.length_squared() > 0.0 {
        dir_v
    } else {
        regia_core::glam::Vec3::NEG_Y
    };
    let center = if snap_texels > 0 {
        crate::snap_ortho_center(dir_v.to_array(), center, radius, snap_texels)
    } else {
        center
    };
    let center_v = regia_core::glam::Vec3::from_array(center);
    // The shadow eye sits up-sun (opposite the light's travel direction), looking
    // along the travel direction at the centre — the side the light comes from.
    let back = back.max(radius);
    // The look-at from `center - dir * back`, built at the center and pushed `back` along its
    // own view axis: the same matrix in exact arithmetic, but the lateral translation now comes
    // from the center alone. From the eye 2 km up-sun its coordinates canceled to a few meters
    // and left ~0.1 mm of rounding that changed with every placement, a hundredth of a 1 cm
    // texel of lattice jitter (ADR-1236). The rotation comes from `dir` itself, not from
    // `(center + dir) - center`: a far cascade's center is kilometers out, where that difference
    // keeps only ~1e-4 of the direction, and the basis turned by that much between placements
    // (a 0.03-texel slide across a 2.6 km cascade; ADR-1387).
    let view = regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(0.0, 0.0, -back))
        * look_at_view(regia_core::glam::Vec3::ZERO, dir_v, regia_core::glam::Vec3::Y)
        * regia_core::glam::Mat4::from_translation(-center_v);
    let proj = orthographic(-radius, radius, -radius, radius, 0.001, back + radius * 2.0);
    (proj * view).to_cols_array_2d()
}

pub(super) fn face_vp(pos: [f32; 3], face: u32, far: f32) -> [[f32; 4]; 4] {
    let (dir, up) = crate::reflections::cube_face_axes(face);
    look_proj(pos, dir, up, 90.0_f32.to_radians(), far)
}

/// Sun cascade placement inputs that persist between frames (see [`crate::cascade_anchor`]).
pub(super) struct CascadeSticky<'s> {
    /// Last placement per cascade.
    pub(super) anchors: &'s mut [Option<crate::cascade_anchor::CascadeAnchor>; 4],
    /// Bit `i` set: cascade `i` may keep its placement (paged sun shadows only).
    pub(super) mask: u8,
    /// Inverse of the unjittered camera view-projection.
    pub(super) inv_view_proj: regia_core::glam::Mat4,
    /// The near sun tile's kept centre (ADR-164), paged sun shadows only.
    pub(super) near_anchor: &'s mut Option<[f32; 3]>,
    /// The longest reach of the sun cascades (`forward_lite::sun_range_m`).
    pub(super) sun_range_m: f32,
    /// The precipitation occlusion view this frame wants (spec 99 § 5.6).
    pub(super) precip: Option<super::weather::PrecipTilePlan>,
}

pub(super) fn plan_shadows(
    scene: &FrameScene<'_>,
    local_lights: &[crate::LabLocalLight],
    view: &crate::light_budget::LightView,
    sun_travel: [f32; 3],
    sticky: CascadeSticky<'_>,
    warnings: &crate::light_budget::LightDropWarnings,
) -> ShadowPlan {
    let quality = scene.quality;
    let mut light_tile = vec![-1; local_lights.len()];
    let mut tiles = Vec::new();
    // No atlas without a shadowed light to put in it: casting lights under a zero light cap
    // (DO27's Low) kept a 4096² depth atlas (64 MiB) with no tile in it (ADR-223).
    let want_atlas = local_lights.iter().any(light_casts)
        && quality.local_shadow_atlas_size > 0
        && quality.local_shadow_max_lights > 0;
    // The near sun tile (ADR-164) takes one atlas square of its own, the first one; without
    // shadowed local lights the atlas is just that square.
    let near_edge = if scene.directional_shadows && quality.near_sun_tile > 0 {
        quality.near_sun_tile.clamp(256, 8192)
    } else {
        0
    };
    let layout_edge = if want_atlas {
        quality.local_shadow_atlas_size.max(256).max(near_edge)
    } else if near_edge > 0 {
        near_edge
    } else {
        4
    };
    let precip_edge = sticky.precip.map_or(0, |plan| plan.size);
    // The occlusion tile never takes a light's room: the atlas grows by its edge, the lights keep
    // their texels, and it sits in the strip at the right. Alone, the atlas is just the tile.
    let tile_alone = precip_edge > 0 && !want_atlas && near_edge == 0;
    let atlas_size = if tile_alone {
        precip_edge
    } else {
        layout_edge + precip_edge
    };
    let local_tile_cap = MAX_SHADOW_TILES - usize::from(near_edge > 0) - usize::from(precip_edge > 0);
    let candidates = crate::light_budget::order_shadow_casters(
        local_lights,
        view,
        quality.local_shadow_max_lights,
        local_tile_cap,
    );
    let mut assigned_lights = 0u32;
    let mut shadowed = Vec::new();
    let mut pending: Vec<PendingShadow> = Vec::new();
    for index in candidates.indices {
        if assigned_lights >= quality.local_shadow_max_lights {
            break;
        }
        let Some(light) = local_lights.get(index) else {
            continue;
        };
        let (views, texel_slope, inv_depth) = match *light {
            crate::LabLocalLight::Point(point) => {
                let views: Vec<[[f32; 4]; 4]> = (0..6)
                    .map(|face| face_vp(point.position, face, point.radius))
                    .collect();
                (views, 2.0, 1.0 / 0.05 - 1.0 / point.radius.max(0.2))
            }
            crate::LabLocalLight::Spot(spot) => {
                let dir = regia_core::glam::Vec3::from_array(spot.direction);
                let up = if dir.normalize().dot(regia_core::glam::Vec3::Y).abs() > 0.95 {
                    [0.0, 0.0, 1.0]
                } else {
                    [0.0, 1.0, 0.0]
                };
                let fov = (spot.outer_angle * 2.0).clamp(0.15, 2.0 * SPOT_SHADOW_MAX_HALF_ANGLE);
                let views = vec![look_proj(spot.position, spot.direction, up, fov, spot.range)];
                (
                    views,
                    2.0 * (fov * 0.5).tan(),
                    1.0 / 0.05 - 1.0 / spot.range.max(0.2),
                )
            }
        };
        let used: usize = pending.iter().map(|item| item.views.len()).sum();
        if used + views.len() > local_tile_cap {
            // Under the cap the old loop stopped. Over the cap, a later spot
            // may still fit in the tiles a point could not take.
            if candidates.all_fit {
                break;
            }
            continue;
        }
        assigned_lights += 1;
        shadowed.push(index);
        pending.push(PendingShadow {
            index,
            views,
            importance: crate::light_budget::screen_importance(light, view),
            texel_slope,
            inv_depth,
        });
    }
    // Size follows screen importance. Order stays the assignment order so
    // tile 0 is still the first shadowed light's first face (the paged test).
    let face_counts: Vec<u32> = pending.iter().map(|item| item.views.len() as u32).collect();
    let importance: Vec<f32> = pending.iter().map(|item| item.importance).collect();
    let (near_rect, rects) =
        crate::shadow::layout_shadow_tiles_reserved(near_edge, &face_counts, &importance, layout_edge);
    let mut rect_cursor = 0usize;
    let mut light_faces = Vec::with_capacity(pending.len());
    for item in &pending {
        light_tile[item.index] = tiles.len() as i32;
        let first_tile = tiles.len();
        for view_proj in &item.views {
            let rect = rects
                .get(rect_cursor)
                .copied()
                .unwrap_or(crate::shadow::AtlasRect { x: 0, y: 0, size: 1 });
            rect_cursor += 1;
            let tile_px = rect.size.max(1);
            let scale = tile_px as f32 / atlas_size.max(1) as f32;
            tiles.push(PlannedTile {
                gpu: ShadowTileGpu {
                    view_proj: *view_proj,
                    uv: [
                        scale,
                        scale,
                        rect.x as f32 / atlas_size.max(1) as f32,
                        rect.y as f32 / atlas_size.max(1) as f32,
                    ],
                    bias: [
                        item.texel_slope / tile_px as f32,
                        item.inv_depth,
                        1.0,
                        // The light's `LightSourceSize`, else the 45 cm default (ADR-1367).
                        crate::light_source::shadow_source_radius(
                            local_lights
                                .get(item.index)
                                .and_then(crate::light_source::light_source_radius),
                        ),
                    ],
                },
                x: rect.x,
                y: rect.y,
                size: tile_px,
            });
        }
        let (center, radius) = match local_lights.get(item.index) {
            Some(crate::LabLocalLight::Point(point)) => (point.position, point.radius),
            Some(crate::LabLocalLight::Spot(spot)) => (spot.position, spot.range),
            None => ([0.0; 3], f32::INFINITY),
        };
        light_faces.push(LightFaces {
            tiles: first_tile..tiles.len(),
            center,
            radius,
        });
    }
    let caster_count = local_lights.iter().filter(|light| light_casts(light)).count() as u32;
    if shadowed.len() < caster_count as usize {
        warnings.warn_shadow_cap(
            caster_count,
            shadowed.len() as u32,
            quality.local_shadow_max_lights,
        );
    }
    let precip_tile = sticky.precip.map(|plan| {
        let (view_proj, bias) = super::weather::precip_tile_matrix(&plan);
        let scale = plan.size as f32 / atlas_size.max(1) as f32;
        let x = if tile_alone { 0 } else { layout_edge };
        tiles.push(PlannedTile {
            gpu: ShadowTileGpu {
                view_proj,
                uv: [scale, scale, x as f32 / atlas_size.max(1) as f32, 0.0],
                bias,
            },
            x,
            y: 0,
            size: plan.size,
        });
        tiles.len() - 1
    });
    // CSM: practical split scheme over [near, far], each cascade an ortho sphere fit
    // around the camera's frustum slice, snapped to its texel lattice
    // (`crate::sun_cascades`; specs/13 § 5 step 5, ADR-1236).
    let near = scene.near.max(0.05);
    // The lite Low tier's one cascade spans less than the view (`forward_lite::sun_range_m`).
    let cascade_far = scene.far.min(sticky.sun_range_m);
    let shadow_far = csm_far(cascade_far);
    let eye = regia_core::glam::Vec3::from_array(scene.eye);
    let forward = (regia_core::glam::Vec3::from_array(scene.target) - eye).normalize_or_zero();
    let aspect = scene.width.max(1) as f32 / scene.height.max(1) as f32;
    // The projection's field of view (horizontal-plus widens it below 16:9), not the authored
    // one: a cascade fit to the narrower frustum leaves the top and bottom of a 4:3 or 16:10
    // picture outside every cascade.
    let tan_half = (view.fov_y * 0.5).tan();
    let csm = crate::sun_cascades::place_sun_cascades(
        &crate::sun_cascades::SunCascadeView {
            eye: scene.eye,
            forward: forward.to_array(),
            fov_y: view.fov_y,
            aspect,
            near: scene.near,
            far: cascade_far,
        },
        sun_travel,
        &quality,
        Some(crate::sun_cascades::CascadeAnchors {
            anchors: &mut *sticky.anchors,
            mask: sticky.mask,
            inv_view_proj: sticky.inv_view_proj,
        }),
    );
    // The near sun tile (ADR-164): an orthographic sun view around the first
    // `NEAR_SUN_RANGE_M` of the view frustum, the last atlas tile, snapped to its own texel
    // grid (a camera move under a texel keeps its matrix). Its light eye reaches up-sun as far
    // as the cascades' (`cascade_back`), so every mesh caster over the near receivers is in it.
    // `bias` = (texel metres, 1 / depth extent, 0 = orthographic, range metres).
    let mut near_sphere = None;
    let near_tile = near_rect
        .filter(|_| crate::NEAR_SUN_RANGE_M > near * 2.0)
        .map(|rect| {
            let (centre, radius) = crate::frustum_slice_sphere(
                eye.to_array(),
                forward.to_array(),
                near,
                crate::NEAR_SUN_RANGE_M,
                tan_half,
                aspect,
            );
            // On the radius ladder like stable cascades, so a drifting FOV keeps its lattice.
            let radius = if quality.stable_sun_cascades {
                crate::sun_cascades::quantize_cascade_radius(radius)
            } else {
                radius
            };
            // With paged shadows the tile covers a sphere a quarter wider than the slice's and
            // keeps its centre while the slice's sphere stays inside it, so a camera that
            // strafes or orbits redraws it every few metres instead of every frame (the matrix
            // is the page cache's key). Texels grow by that quarter.
            let (centre, radius) = if quality.paged_sun_shadows {
                let wide = radius * NEAR_TILE_SLACK;
                let need = regia_core::glam::Vec3::from_array(centre);
                let keep = sticky
                    .near_anchor
                    .filter(|kept| regia_core::glam::Vec3::from_array(*kept).distance(need) + radius <= wide);
                let c = keep.unwrap_or(centre);
                *sticky.near_anchor = Some(c);
                (c, wide)
            } else {
                *sticky.near_anchor = None;
                (centre, radius)
            };
            near_sphere = Some((centre, radius));
            let back = cascade_back(radius, shadow_far).max(radius);
            let size = rect.size.max(1);
            let scale = size as f32 / atlas_size.max(1) as f32;
            tiles.push(PlannedTile {
                gpu: ShadowTileGpu {
                    view_proj: ortho_light_vp(sun_travel, centre, radius, back, size),
                    uv: [
                        scale,
                        scale,
                        rect.x as f32 / atlas_size.max(1) as f32,
                        rect.y as f32 / atlas_size.max(1) as f32,
                    ],
                    bias: [
                        2.0 * radius / size as f32,
                        1.0 / (back + radius * 2.0),
                        0.0,
                        crate::NEAR_SUN_RANGE_M,
                    ],
                },
                x: rect.x,
                y: rect.y,
                size,
            });
            tiles.len() - 1
        });
    ShadowPlan {
        tiles,
        light_tile,
        shadowed,
        light_faces,
        atlas_size,
        csm,
        near_tile,
        near_sphere,
        precip_tile,
        precip_plan: sticky.precip,
        clock: [0.0; 4],
        clock_live: false,
    }
}

#[cfg(test)]
pub(super) fn centroid_view_depth(
    vertices: &[LabVertex],
    model: [[f32; 4]; 4],
    view: regia_core::glam::Mat4,
) -> f32 {
    view_depth(vertex_centroid(vertices), model, view)
}

/// Sort key of one blended draw.
///
/// A rigid mesh passes the centre already stored on its mesh-cache entry ([`MeshBounds`]):
/// that point is transformed and the vertex buffer is not read. A mesh without a bound
/// (a skin, or a key that has not been uploaded) uses the mean of its positions, remembered
/// on `cache` by mesh key, vertex count and the first and last positions. Key `0` is not
/// remembered: several ephemeral meshes would share it.
pub(super) fn blended_sort_depth(
    cache: &mut CasterListCache,
    mesh_key: u64,
    vertices: &[LabVertex],
    bounds: Option<MeshBounds>,
    model: [[f32; 4]; 4],
    view: regia_core::glam::Mat4,
) -> f32 {
    let center = if let Some(bounds) = bounds {
        bounds.center
    } else {
        cached_centroid(cache, mesh_key, vertices)
    };
    view_depth(center, model, view)
}

fn view_depth(center: [f32; 3], model: [[f32; 4]; 4], view: regia_core::glam::Mat4) -> f32 {
    let model = regia_core::glam::Mat4::from_cols_array_2d(&model);
    let view_pos = view * model * regia_core::glam::Vec3::from_array(center).extend(1.0);
    -view_pos.z
}

fn vertex_centroid(vertices: &[LabVertex]) -> [f32; 3] {
    if vertices.is_empty() {
        return [0.0; 3];
    }
    let mut acc = regia_core::glam::Vec3::ZERO;
    for vertex in vertices {
        acc += regia_core::glam::Vec3::from_array(vertex.position);
    }
    (acc / vertices.len() as f32).to_array()
}

fn cached_centroid(cache: &mut CasterListCache, mesh_key: u64, vertices: &[LabVertex]) -> [f32; 3] {
    let (count, first, last) = match (vertices.first(), vertices.last()) {
        (Some(first), Some(last)) => (vertices.len(), first.position, last.position),
        _ => return [0.0; 3],
    };
    if mesh_key != 0
        && let Some(hit) = cache.centroids.get(&mesh_key)
        && hit.count == count
        && hit.first == first
        && hit.last == last
    {
        return hit.center;
    }
    cache.centroid_walks += 1;
    let center = vertex_centroid(vertices);
    if mesh_key != 0 {
        if cache.centroids.len() >= 4096 {
            cache.centroids.clear();
        }
        cache.centroids.insert(
            mesh_key,
            CachedCentroid {
                count,
                first,
                last,
                center,
            },
        );
    }
    center
}

#[cfg(test)]
mod tests {
    use super::{CasterListCache, ShadowCast, blended_sort_depth, centroid_view_depth, shadow_model};
    use crate::LabVertex;
    use crate::frame::mesh_bounds;
    use regia_core::glam::Mat4;

    /// A negative or zero shadow scale draws the shadow as drawn instead of mirroring or
    /// collapsing the caster, the rule the visibility path's shadow fields share
    /// (corr-low-render, corr-render-visbuffer finding K).
    #[test]
    fn a_shadow_scale_that_is_not_positive_draws_the_caster_as_drawn() {
        let model = [
            [2.0, 0.0, 0.0, 0.0],
            [0.0, 2.0, 0.0, 0.0],
            [0.0, 0.0, 2.0, 0.0],
            [1.0, 2.0, 3.0, 1.0],
        ];
        for shadow_scale in [-2.0, 0.0, f32::NAN] {
            let cast = ShadowCast::Local {
                reach: 1.0,
                max_cascades: 2,
                shadow_scale,
                shadow_lift: 0.0,
                anchor: None,
                lod_switch_keeps_pages: false,
            };
            assert_eq!(shadow_model(model, cast), model, "scale {shadow_scale}");
        }
    }

    fn vertex_at(position: [f32; 3]) -> LabVertex {
        LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0])
    }

    /// ADR-5594: a caster whose mesh bytes were rewritten in place is a new paged-shadow caster
    /// (its old and new pages are redrawn); a mesh never rewritten keeps its id.
    #[test]
    fn a_mesh_rewritten_in_place_is_a_new_shadow_caster() {
        let id = super::shadow_caster_id(7, 11, crate::ShadowCast::All);
        assert_eq!(super::caster_content_id(id, 0), id);
        let first = super::caster_content_id(id, 1);
        let second = super::caster_content_id(id, 2);
        assert_ne!(first, id);
        assert_ne!(second, id);
        assert_ne!(first, second);
        // Another caster's ids stay apart from this one's.
        let other = super::shadow_caster_id(8, 11, crate::ShadowCast::All);
        assert_ne!(super::caster_content_id(other, 1), first);
    }

    /// A rigid box sorts at its cached centre. A longer buffer inside the same box must not
    /// move that depth, and must not walk the vertices. The vertex average of that longer
    /// buffer does move: that is the scan this replaces.
    #[test]
    fn blended_sort_depth_uses_the_cached_center() {
        let corners = [
            [-1.0, -1.0, -1.0],
            [3.0, -1.0, -1.0],
            [-1.0, 5.0, -1.0],
            [3.0, 5.0, -1.0],
            [-1.0, -1.0, 1.0],
            [3.0, -1.0, 1.0],
            [-1.0, 5.0, 1.0],
            [3.0, 5.0, 1.0],
        ];
        let mut vertices: Vec<LabVertex> = corners.into_iter().map(vertex_at).collect();
        let Some(bounds) = mesh_bounds(&vertices) else {
            panic!("the box has vertices");
        };
        assert_eq!(bounds.center, [1.0, 2.0, 0.0]);
        let model = Mat4::from_translation(regia_core::glam::Vec3::new(0.0, 0.0, 4.0)).to_cols_array_2d();
        let view = Mat4::IDENTITY;
        let mut cache = CasterListCache::default();
        let depth = blended_sort_depth(&mut cache, 7, &vertices, Some(bounds), model, view);
        assert_eq!(depth.to_bits(), (-4.0f32).to_bits());
        assert_eq!(cache.centroid_walks, 0, "a cached centre is not a vertex walk");
        let average = centroid_view_depth(&vertices, model, view);
        vertices.push(vertex_at([-1.0, -1.0, -1.0]));
        let longer_average = centroid_view_depth(&vertices, model, view);
        assert_ne!(
            average.to_bits(),
            longer_average.to_bits(),
            "the vertex average of a longer buffer inside the same box moves"
        );
        let Some(same_box) = mesh_bounds(&vertices) else {
            panic!("the longer buffer has vertices");
        };
        assert_eq!(same_box.center, bounds.center);
        let depth_longer = blended_sort_depth(&mut cache, 7, &vertices, Some(same_box), model, view);
        assert_eq!(depth_longer.to_bits(), depth.to_bits());
        assert_eq!(cache.centroid_walks, 0);
        let mut skinned = CasterListCache::default();
        let once = blended_sort_depth(&mut skinned, 9, &vertices, None, model, view);
        assert_eq!(skinned.centroid_walks, 1);
        let twice = blended_sort_depth(&mut skinned, 9, &vertices, None, model, view);
        assert_eq!(
            skinned.centroid_walks, 1,
            "the same skin must not be walked again"
        );
        assert_eq!(once.to_bits(), twice.to_bits());
        vertices[0].position[0] += 0.25;
        let _moved = blended_sort_depth(&mut skinned, 9, &vertices, None, model, view);
        assert_eq!(skinned.centroid_walks, 2, "a changed vertex refits the centroid");
    }
}

/// The per-face cull of a light's atlas tiles (ADR-7986).
#[cfg(test)]
mod face_cull_tests {
    use super::{face_sees_caster, face_vp};
    use crate::paged_shadow::Aabb3;

    const LIGHT: [f32; 3] = [0.0, 2.0, 0.0];

    fn cube(center: [f32; 3], half: f32) -> Option<Aabb3> {
        Some(Aabb3::from_center_half(center, [half; 3]))
    }

    /// Faces 0..6 are +X, -X, +Y, -Y, +Z, -Z (`reflections::cube_face_axes`).
    fn faces_seeing(aabb: Option<Aabb3>, radius: f32) -> Vec<u32> {
        (0..6)
            .filter(|&face| face_sees_caster(aabb, LIGHT, radius, &face_vp(LIGHT, face, radius)))
            .collect()
    }

    #[test]
    fn a_box_below_the_light_is_in_the_down_face_only() {
        assert_eq!(faces_seeing(cube([0.0, 0.5, 0.0], 0.3), 6.0), vec![3]);
    }

    #[test]
    fn a_box_across_a_face_seam_is_in_both_faces() {
        // Centred on the diagonal between +X and -Y, straddling the seam.
        assert_eq!(faces_seeing(cube([1.0, 1.0, 0.0], 0.3), 6.0), vec![0, 3]);
    }

    #[test]
    fn a_box_round_the_light_is_in_every_face() {
        assert_eq!(faces_seeing(cube(LIGHT, 0.5), 6.0), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_box_past_the_radius_and_its_margin_is_in_no_face() {
        // 8 m below a 6 m light: past 6 x 1.1 + 0.25 = 6.85 m.
        assert!(faces_seeing(cube([0.0, -6.3, 0.0], 0.3), 6.0).is_empty());
        // Its top 6.5 m below is past the radius but inside the margin: the down face keeps it.
        assert_eq!(faces_seeing(cube([0.0, -4.8, 0.0], 0.3), 6.0), vec![3]);
    }

    #[test]
    fn an_unbounded_caster_is_in_every_face() {
        assert_eq!(faces_seeing(None, 6.0), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_spot_keeps_only_what_is_inside_its_cone() {
        let down = super::look_proj(LIGHT, [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], 0.6, 10.0);
        assert!(face_sees_caster(cube([0.0, 0.0, 0.0], 0.2), LIGHT, 10.0, &down));
        // 3 m to the side at 2 m below: 56 degrees off a 17-degree half-angle cone.
        assert!(!face_sees_caster(cube([3.0, 0.0, 0.0], 0.2), LIGHT, 10.0, &down));
        // Above the light (behind the spot): outside its side planes too.
        assert!(!face_sees_caster(cube([0.0, 4.0, 0.0], 0.2), LIGHT, 10.0, &down));
    }
}
