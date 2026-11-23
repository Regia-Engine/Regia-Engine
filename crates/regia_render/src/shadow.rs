// SPDX-License-Identifier: MPL-2.0
//! Shadow-map cascade split calculation and texel snapping.
//!
//! ## Provenance
//!
//! - **Practical Split Scheme** (PSSM): Zhang et al., "Parallel-Split Shadow Maps for Large-Scale
//!   Virtual Environments" (2006), GPU Gems 3 Chapter 10. The blend between uniform and logarithmic
//!   splits given a parameter λ is equation (1) in that chapter.
//! - **Snap-to-texels**: Wronski, "Stable Cascaded Shadow Maps" (SIGGRAPH 2014 talk). The center
//!   of the shadow map is rounded to the nearest texel in world space to prevent shimmering when
//!   the camera rotates slowly.

/// Computes cascade split planes for a directional-light shadow map using the
/// Practical Split Scheme (PSSM).
///
/// # Parameters
///
/// - `near`: Camera near plane (positive, > 0).
/// - `far`: Camera far plane (positive, > near).
/// - `count`: Number of cascades (≥ 2).
/// - `lambda`: Blend between logarithmic (λ = 1.0) and uniform (λ = 0.0) split distribution.
///   Typical value: 0.75.
///
/// # Returns
///
/// A `Vec` of `f32` of length `count - 1` containing the split planes. For `count = 2` this returns
/// a single `f32` representing where cascade 0 ends and cascade 1 begins. The near and far planes
/// themselves are not included.
///
/// # Formula
///
/// Let N = count, i ∈ [1, N−1]:
///   `z_log[i] = near * (far / near)^(i / N)`
///   `z_uniform[i] = near + (far - near) * (i / N)`
///   `z[i] = λ * z_log[i] + (1 − λ) * z_uniform[i]`
pub fn cascade_splits(near: f32, far: f32, count: u32, lambda: f32) -> Vec<f32> {
    assert!(near > 0.0, "near must be > 0");
    assert!(far > near, "far must be > near");
    assert!(count >= 2, "count must be >= 2");
    assert!((0.0..=1.0).contains(&lambda), "lambda must be in [0, 1]");

    let n = count as f32;
    (1..count)
        .map(|i| {
            let i_f = i as f32;
            let z_log = near * (far / near).powf(i_f / n);
            let z_uni = near + (far - near) * (i_f / n);
            lambda * z_log + (1.0 - lambda) * z_uni
        })
        .collect()
}

/// Farthest the first sun cascade may end (metres; see [`csm_boundaries`]).
pub const CSM_FIRST_CASCADE_MAX: f32 = 16.0;
/// Nearest the last cascade may start (metres; see [`csm_boundaries`]) with three or more
/// cascades. 300 m (was 1 km): with four cascades the mid one then spans 69–300 m, the
/// ground an RTS camera ~100 m out sees (ADR-140, csm-far).
pub const CSM_LAST_SPLIT_MAX: f32 = 300.0;
/// Farthest the one split of a two-cascade layout may sit (metres): the geometric middle of
/// the 16 m – 1 km ladder it had before ADR-140 (csm-far). Kept, because the second of two
/// cascades runs to the shadow range and a nearer split would hand 69–126 m to it.
pub const CSM_SINGLE_SPLIT_MAX: f32 = 126.491_1;

/// Cascade boundaries for a sun CSM over `[near, far]`: `count + 1` view distances,
/// `near` first and `far` last.
///
/// The practical split scheme ([`cascade_splits`], λ = 0.75), with each split capped on a
/// geometric ladder from [`CSM_FIRST_CASCADE_MAX`] (the first split) to
/// [`CSM_LAST_SPLIT_MAX`] (the last). A short view range never reaches the caps, so its
/// layout is unchanged. A long one would stretch every cascade with it — on a 12 km vista
/// the first cascade ended 750 m out and a gate truss or a rover no longer covered a
/// texel. The renderer also caps the range at 4 km (`frame.rs` `csm_far`), so a 12 km
/// view splits at 16 m, 69 m and 300 m and the last cascade carries the rest. The first
/// cap is 16 m (was 40 m, ADR-061) so a character 1–5 m from the camera casts a shadow that
/// resolves its legs and starts at its feet (ADR-115, dyn-light). The last cap is 300 m
/// (was 1 km): the old 126–962 m cascade had ~1.1 m texels, so units 120–300 m from an RTS
/// camera lost their shadows on each other; 69–300 m has ~0.3 m (ADR-140, csm-far).
pub fn csm_boundaries(near: f32, far: f32, count: u32) -> Vec<f32> {
    let count = count.max(1);
    // `cascade_splits` needs a positive, finite near plane; a NaN, infinite or non-positive one
    // becomes the epsilon, and a far plane that is not finite sits just past it.
    let near = if near.is_finite() {
        near.max(1.0e-3)
    } else {
        1.0e-3
    };
    let far = if far.is_finite() {
        far.max(near + 0.1)
    } else {
        near + 0.1
    };
    let mut out = Vec::with_capacity(count as usize + 1);
    out.push(near);
    if count > 1 {
        let splits = count - 1;
        let ratio = CSM_LAST_SPLIT_MAX / CSM_FIRST_CASCADE_MAX;
        for (i, practical) in cascade_splits(near, far, count, 0.75).into_iter().enumerate() {
            let cap = if splits == 1 {
                CSM_SINGLE_SPLIT_MAX
            } else {
                CSM_FIRST_CASCADE_MAX * ratio.powf(i as f32 / (splits - 1) as f32)
            };
            let prev = out.last().copied().unwrap_or(near);
            out.push(practical.min(cap).max(prev + 1e-3).min(far));
        }
    }
    out.push(far);
    out
}

/// View distance (metres) the near sun tile covers ([`crate::FrameQuality::near_sun_tile`],
/// ADR-164). Mesh receivers nearer than this sample the tile instead of cascade 0; its last
/// fifth blends into the cascade. At 4 m a 2048² tile has ~5 mm texels for a 60° view, a
/// quarter of cascade 0's on its 16 m span: a hull flank at grazing sun 1–4 m from the camera
/// no longer shows the stretched texel steps ("garbled letters").
pub const NEAR_SUN_RANGE_M: f32 = 4.0;

/// Texels of margin inside the near sun tile: a receiver whose filter could reach outside the
/// tile (into a neighbouring local-light tile of the atlas) takes cascade 0 instead.
pub const NEAR_SUN_TILE_MARGIN_TEXELS: f32 = 12.0;

/// Bounding sphere `(centre, radius)` of the view-frustum slice `[near, far]` along `forward`
/// from `eye`: half its depth plus the far slice's corner extent, 5 % slack (the sun cascades'
/// fit, `frame.rs` `plan_shadows`).
#[must_use]
pub fn frustum_slice_sphere(
    eye: [f32; 3],
    forward: [f32; 3],
    near: f32,
    far: f32,
    tan_half_fov_y: f32,
    aspect: f32,
) -> ([f32; 3], f32) {
    let mid = (near + far) * 0.5;
    let lateral = far * tan_half_fov_y * aspect.mul_add(aspect, 1.0).sqrt();
    let radius = ((far - mid).mul_add(far - mid, lateral * lateral) * 1.05)
        .sqrt()
        .max(0.25);
    let centre = [
        forward[0].mul_add(mid, eye[0]),
        forward[1].mul_add(mid, eye[1]),
        forward[2].mul_add(mid, eye[2]),
    ];
    (centre, radius)
}

/// Tangent of the sun's angular radius behind the sun PCSS penumbra (`lab_forward.wgsl`
/// `SUN_ANGULAR_RADIUS_TAN`): the real sun's 0.27°, as the atmosphere's
/// `sun_angular_radius`, the path tracer's sun disc and Blender's default sun. It was 1°
/// (a hazy sun), which spread a thin close caster's shadow 3.7× wider than the tracers'.
pub const SUN_PCSS_ANGULAR_RADIUS_TAN: f32 = 0.004_675;

/// Smallest sun PCSS filter radius in shadow texels (`shadows.wgsl`
/// `SUN_PCSS_MIN_RADIUS_TEXELS`): the filter that hides the texel grid where the true
/// penumbra is narrower than a texel (every contact shadow). It was 1.5.
pub const SUN_PCSS_MIN_RADIUS_TEXELS: f32 = 1.0;

/// Largest sun PCSS filter radius in shadow texels (`shadow_pcss_sun` in `shadows.wgsl`).
pub const SUN_PCSS_MAX_RADIUS_TEXELS: f32 = 9.0;

/// The sun PCSS filter radius in shadow texels for a receiver `receiver_blocker_m` metres
/// behind its average blocker on a cascade of `texel_m`-metre texels: CPU mirror of
/// `shadow_pcss_sun` (`penumbra_per_depth` in `lab_forward.wgsl` times the blocker gap).
///
/// The penumbra of a disc light grows as gap × tan(angular radius) (Fernando, "Percentage-
/// Closer Soft Shadows", 2005), clamped to [`SUN_PCSS_MIN_RADIUS_TEXELS`],
/// [`SUN_PCSS_MAX_RADIUS_TEXELS`].
#[must_use]
pub fn sun_pcss_radius_texels(receiver_blocker_m: f32, texel_m: f32) -> f32 {
    let texel_m = texel_m.max(1e-9);
    (receiver_blocker_m.max(0.0) * SUN_PCSS_ANGULAR_RADIUS_TAN / texel_m)
        .clamp(SUN_PCSS_MIN_RADIUS_TEXELS, SUN_PCSS_MAX_RADIUS_TEXELS)
}

/// Rounds the center of a shadow-map frustum to the nearest texel in world space so the
/// shadow-map centre does not move between frames (preventing shimmer).
///
/// - `center`: The current centre of the shadow map in world-space units.
/// - `texel_world`: The size of one shadow-map texel in world-space units (world_units_per_texel).
///
/// Returns the snapped centre.
pub fn snap_to_texels(center: f32, texel_world: f32) -> f32 {
    (center / texel_world).round() * texel_world
}

/// Snaps an orthographic shadow-map centre onto the texel grid of `resolution`.
///
/// The view basis matches the renderer's sun cascades: look along `dir` (the
/// light's travel direction) with world +Y as up. `resolution == 0` leaves the
/// centre alone. A light parallel to +Y is the look-at singularity; the centre
/// is then returned unchanged.
///
/// A camera move smaller than one shadow texel keeps the same centre, so the
/// cascade matrix (and therefore cached pages) stays put. Light-space Z is
/// quantized with XY: the ortho matrix is rebuilt from this centre.
#[must_use]
pub fn snap_ortho_center(dir: [f32; 3], center: [f32; 3], radius: f32, resolution: u32) -> [f32; 3] {
    if resolution == 0 || !radius.is_finite() || radius <= 0.0 {
        return center;
    }
    let dir_v = regia_core::glam::Vec3::from_array(dir);
    let len = dir_v.length();
    if !len.is_finite() || len < 1.0e-6 {
        return center;
    }
    let dir_v = dir_v / len;
    if dir_v.dot(regia_core::glam::Vec3::Y).abs() > 0.99 {
        return center;
    }
    let center_v = regia_core::glam::Vec3::from_array(center);
    // The basis depends only on the light direction. Building the view on the
    // centre itself would put that centre at the origin, and rounding would
    // leave it unmoved.
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        -dir_v,
        regia_core::glam::Vec3::ZERO,
        regia_core::glam::Vec3::Y,
    );
    let light = view * center_v.extend(1.0);
    let texel = 2.0 * radius / resolution as f32;
    if !texel.is_finite() || texel <= 0.0 || !light.is_finite() {
        return center;
    }
    // Depth is quantized too. An orthographic cascade rebuilt from a centre that
    // still slides along the light would change every frame and invalidate pages.
    let snapped = regia_core::glam::Vec3::new(
        snap_to_texels(light.x, texel),
        snap_to_texels(light.y, texel),
        snap_to_texels(light.z, texel),
    );
    let world = view.inverse() * snapped.extend(1.0);
    let out = world.truncate().to_array();
    if out.iter().all(|c| c.is_finite()) {
        out
    } else {
        center
    }
}

/// Disc radius of a local light for PCSS, in metres.
///
/// A practical luminaire is not a point. 45 cm is a tunnel fixture, not the
/// light's influence radius (using that paints a room-sized blob). With the
/// atlas filter clamp at 18 texels, a few centimetres of blocker gap stays
/// near one texel and a few tens of centimetres opens a soft penumbra.
/// 8 cm hit the old 6-texel clamp at every contact, so the crate edge was a
/// hard line.
pub const LOCAL_SHADOW_SOURCE_RADIUS_M: f32 = 0.45;

/// One square tile in the local-light shadow atlas, in texels.
///
/// `x` and `y` are the top-left of the tile. The atlas origin matches the
/// shadow pass: +x right, +y down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasRect {
    /// Left edge, in atlas texels.
    pub x: u32,
    /// Top edge, in atlas texels.
    pub y: u32,
    /// Edge length. A light's cube faces share one size.
    pub size: u32,
}

/// Largest power of two that is `<= value`. `0` stays `0`.
fn floor_pow2(value: u32) -> u32 {
    if value < 2 {
        return value;
    }
    1u32 << (31 - value.leading_zeros())
}

/// True when `copies` squares of `edge` fit in `atlas` on their own.
fn tile_copies_fit(edge: u32, atlas: u32, copies: u32) -> bool {
    if edge == 0 || atlas == 0 || edge > atlas {
        return false;
    }
    let per_row = atlas / edge;
    if per_row == 0 {
        return false;
    }
    let rows = copies.div_ceil(per_row);
    rows.saturating_mul(edge) <= atlas
}

/// Edge of one shadow tile for a light of screen-importance rank.
///
/// Rank 0 (the light that covers the most screen) gets `atlas / 4`. Ranks
/// 1..=3 get `atlas / 8`. The rest get `atlas / 16`. When that class is at
/// least 64, the result is a power of two in `64..=1024` that can hold
/// `faces` copies of itself. A smaller atlas may return a smaller power of
/// two that still packs, or `1` when nothing packs. A point light
/// passes `faces = 6` so its cube is not given a size that cannot pack.
///
/// The rank is screen importance (candela times projected area; Persson,
/// SIGGRAPH 2013). It changes the tile size only. Callers keep the authored
/// light order.
#[must_use]
pub fn local_shadow_tile_size(rank: u32, atlas: u32, faces: u32) -> u32 {
    let atlas = atlas.max(1);
    let class = match rank {
        0 => atlas / 4,
        1..=3 => atlas / 8,
        _ => atlas / 16,
    };
    let mut edge = floor_pow2(class.max(1)).min(atlas.min(1024));
    edge = floor_pow2(edge.max(1));
    let faces = faces.max(1);
    while edge >= 64 {
        if tile_copies_fit(edge, atlas, faces) {
            return edge;
        }
        edge /= 2;
    }
    let mut small = floor_pow2(atlas.clamp(1, 64));
    while small >= 1 {
        if tile_copies_fit(small, atlas, faces) {
            return small;
        }
        if small == 1 {
            break;
        }
        small /= 2;
    }
    1
}

/// Next-fit shelf pack. `None` when a square sticks out of `atlas`.
///
/// Squares stay in the order they are given, so a light's first face keeps
/// its index. A row wraps when the next square does not fit beside the last.
#[must_use]
pub fn pack_atlas_rects(sizes: &[u32], atlas: u32) -> Option<Vec<AtlasRect>> {
    if atlas == 0 {
        return None;
    }
    let mut rects = Vec::with_capacity(sizes.len());
    let mut x = 0u32;
    let mut y = 0u32;
    let mut row_h = 0u32;
    for &size in sizes {
        if size == 0 || size > atlas {
            return None;
        }
        // In u64: next to an edge near `u32::MAX` the sums would overflow.
        if u64::from(x) + u64::from(size) > u64::from(atlas) {
            y = y.saturating_add(row_h);
            x = 0;
            row_h = 0;
        }
        if u64::from(y) + u64::from(size) > u64::from(atlas) {
            return None;
        }
        rects.push(AtlasRect { x, y, size });
        x += size;
        row_h = row_h.max(size);
    }
    Some(rects)
}

fn expand_face_sizes(edges: &[u32], faces: &[u32]) -> Vec<u32> {
    let mut sizes = Vec::new();
    for (&edge, &count) in edges.iter().zip(faces.iter()) {
        for _ in 0..count {
            sizes.push(edge);
        }
    }
    sizes
}

/// Shelf-packs one square per face, sized by screen importance.
///
/// `faces[i]` is 6 for a point light and 1 for a spot. `importance[i]` is the
/// screen weight; a higher weight gets a larger tile. Rects come back in
/// light order, every face of one light sharing an edge, so the shadow plan
/// can keep tile 0 as the first light's first face. When the preferred sizes
/// do not fit, the largest size class is halved until they do.
#[must_use]
pub fn layout_local_shadow_tiles(faces: &[u32], importance: &[f32], atlas: u32) -> Vec<AtlasRect> {
    let count = faces.len().min(importance.len());
    if count == 0 || atlas == 0 {
        return Vec::new();
    }
    let faces = &faces[..count];
    let importance = &importance[..count];
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&a, &b| importance[b].total_cmp(&importance[a]).then(a.cmp(&b)));
    let mut ranks = vec![0u32; count];
    for (rank, &index) in order.iter().enumerate() {
        ranks[index] = rank as u32;
    }
    let mut edges: Vec<u32> = (0..count)
        .map(|index| local_shadow_tile_size(ranks[index], atlas, faces[index].max(1)))
        .collect();
    for _ in 0..16 {
        let sizes = expand_face_sizes(&edges, faces);
        if let Some(rects) = pack_atlas_rects(&sizes, atlas) {
            return rects;
        }
        let max_edge = edges.iter().copied().max().unwrap_or(1);
        if max_edge <= 1 {
            break;
        }
        let next = (max_edge / 2).max(1);
        for edge in &mut edges {
            if *edge == max_edge {
                *edge = next;
            }
        }
    }
    let face_count: u32 = faces.iter().sum();
    let mut edge = floor_pow2(atlas);
    while edge >= 1 {
        if let Some(rects) = pack_atlas_rects(&vec![edge; face_count as usize], atlas) {
            return rects;
        }
        if edge == 1 {
            break;
        }
        edge /= 2;
    }
    expand_face_sizes(&edges, faces)
        .into_iter()
        .map(|size| AtlasRect {
            x: 0,
            y: 0,
            size: size.min(atlas).max(1),
        })
        .collect()
}

/// [`layout_local_shadow_tiles`] with one square of `reserved` texels kept for the near sun
/// tile (ADR-164): the reserved square first (top-left), then the local-light faces shelf-packed
/// after it, their largest size class halved until everything fits. `reserved == 0` is the plain
/// layout and `None` for the reserved rect.
#[must_use]
pub fn layout_shadow_tiles_reserved(
    reserved: u32,
    faces: &[u32],
    importance: &[f32],
    atlas: u32,
) -> (Option<AtlasRect>, Vec<AtlasRect>) {
    if reserved == 0 || reserved > atlas {
        return (None, layout_local_shadow_tiles(faces, importance, atlas));
    }
    let reserved_rect = AtlasRect {
        x: 0,
        y: 0,
        size: reserved,
    };
    if faces.is_empty() {
        return (Some(reserved_rect), Vec::new());
    }
    let mut edges: Vec<u32> = layout_local_shadow_tiles(faces, importance, atlas)
        .iter()
        .map(|rect| rect.size)
        .collect();
    for _ in 0..24 {
        let mut sizes = vec![reserved];
        sizes.extend_from_slice(&edges);
        if let Some(rects) = pack_atlas_rects(&sizes, atlas) {
            return (Some(rects[0]), rects[1..].to_vec());
        }
        let max_edge = edges.iter().copied().max().unwrap_or(1);
        if max_edge <= 1 {
            break;
        }
        let next = (max_edge / 2).max(1);
        for edge in &mut edges {
            if *edge == max_edge {
                *edge = next;
            }
        }
    }
    // Nothing fits beside the reserved square: the local lights keep the whole atlas and the
    // near tile is dropped.
    (None, layout_local_shadow_tiles(faces, importance, atlas))
}

#[cfg(test)]
mod tests {
    #[test]
    fn csm_boundaries_tolerates_non_positive_near() {
        for near in [0.0, -1.0, f32::NAN] {
            let b = csm_boundaries(near, 100.0, 4);
            assert_eq!(b.len(), 5);
            assert!(b.iter().all(|v| v.is_finite()));
            assert!(b.windows(2).all(|w| w[0] < w[1]));
        }
    }

    use super::*;

    #[test]
    fn csm_boundaries_survive_a_non_positive_near_plane() {
        for near in [0.0, -1.0, f32::NAN] {
            let b = csm_boundaries(near, 100.0, 4);
            assert_eq!(b.len(), 5);
            assert!(b[0] > 0.0);
            assert_eq!(b[4], 100.0);
            assert!(b.windows(2).all(|w| w[0] < w[1]), "strictly increasing: {b:?}");
        }
    }

    #[test]
    fn csm_boundaries_single_cascade_is_near_and_far() {
        assert_eq!(csm_boundaries(0.5, 80.0, 1), vec![0.5, 80.0]);
        assert_eq!(csm_boundaries(0.5, 80.0, 0), vec![0.5, 80.0]);
    }

    #[test]
    fn csm_boundaries_cap_first_split_on_a_long_range() {
        let b = csm_boundaries(0.1, 4000.0, 4);
        assert!(b[1] <= CSM_FIRST_CASCADE_MAX + 1e-3);
        assert!(b[3] <= CSM_LAST_SPLIT_MAX + 1e-3);
        assert_eq!(b.last().copied(), Some(4000.0));
    }

    #[test]
    #[should_panic(expected = "near must be > 0")]
    fn cascade_splits_rejects_zero_near() {
        let _ = cascade_splits(0.0, 10.0, 3, 0.5);
    }

    #[test]
    fn cascade_splits_lambda_extremes_are_uniform_and_logarithmic() {
        let uni = cascade_splits(1.0, 101.0, 4, 0.0);
        assert!((uni[0] - 26.0).abs() < 1e-3 && (uni[1] - 51.0).abs() < 1e-3 && (uni[2] - 76.0).abs() < 1e-3);
        let log = cascade_splits(1.0, 10000.0, 4, 1.0);
        assert!((log[0] - 10.0).abs() < 1e-2 && (log[1] - 100.0).abs() < 1e-1);
    }

    /// The constant `name` as written in a WGSL source (`const NAME: f32 = value;`).
    fn wgsl_const(source: &str, name: &str) -> f32 {
        let line = source
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("const {name}:")))
            .unwrap_or_else(|| panic!("{name} not in the shader"));
        let value = line.split('=').nth(1).unwrap_or("").trim().trim_end_matches(';');
        value.parse().unwrap_or_else(|_| panic!("{name}: {value}"))
    }

    #[test]
    fn the_near_tile_margin_matches_the_shader() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        assert_eq!(
            wgsl_const(forward, "NEAR_SUN_TILE_MARGIN_TEXELS"),
            NEAR_SUN_TILE_MARGIN_TEXELS
        );
    }

    #[test]
    fn the_near_tile_is_four_times_finer_than_cascade_0() {
        // A 60° 16:9 view: cascade 0 spans 0–16 m, the near tile 0–4 m, both at 2048².
        let tan = 30f32.to_radians().tan();
        let (_, c0) = frustum_slice_sphere(
            [0.0; 3],
            [0.0, 0.0, -1.0],
            0.1,
            CSM_FIRST_CASCADE_MAX,
            tan,
            16.0 / 9.0,
        );
        let (centre, near) =
            frustum_slice_sphere([0.0; 3], [0.0, 0.0, -1.0], 0.1, NEAR_SUN_RANGE_M, tan, 16.0 / 9.0);
        let (texel_c0, texel_near) = (2.0 * c0 / 2048.0, 2.0 * near / 2048.0);
        assert!(texel_near < 0.006, "near tile texel {texel_near} m");
        assert!(
            texel_c0 > 3.5 * texel_near,
            "cascade 0 {texel_c0} m vs near {texel_near} m"
        );
        assert!(
            (centre[2] + 2.05).abs() < 1e-4,
            "center halfway down the slice: {centre:?}"
        );
    }

    #[test]
    fn the_reserved_square_comes_first_and_locals_pack_around_it() {
        // No local lights: the atlas is the reserved square.
        let (near, locals) = layout_shadow_tiles_reserved(2048, &[], &[], 2048);
        assert_eq!(
            near,
            Some(AtlasRect {
                x: 0,
                y: 0,
                size: 2048
            })
        );
        assert!(locals.is_empty());
        // Desktop High's 8192 atlas with two points and a spot: the square stays 2048 at the
        // origin and no local face overlaps it.
        let (near, locals) = layout_shadow_tiles_reserved(2048, &[6, 6, 1], &[3.0, 2.0, 1.0], 8192);
        let near = near.expect("reserved square");
        assert_eq!((near.x, near.y, near.size), (0, 0, 2048));
        assert_eq!(locals.len(), 13);
        for r in &locals {
            let apart = r.x >= near.x + near.size || r.y >= near.y + near.size;
            assert!(apart, "{r:?} overlaps the near tile");
            assert!(
                r.x + r.size <= 8192 && r.y + r.size <= 8192,
                "{r:?} leaves the atlas"
            );
        }
        // Off: the plain layout.
        let (none, plain) = layout_shadow_tiles_reserved(0, &[1], &[1.0], 4096);
        assert!(none.is_none());
        assert_eq!(plain, layout_local_shadow_tiles(&[1], &[1.0], 4096));
    }

    #[test]
    fn the_sun_pcss_mirror_matches_the_shaders() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let shadows = include_str!("../shaders/shadows.wgsl");
        assert_eq!(
            wgsl_const(forward, "SUN_ANGULAR_RADIUS_TAN"),
            SUN_PCSS_ANGULAR_RADIUS_TAN
        );
        assert_eq!(
            wgsl_const(shadows, "SUN_PCSS_MIN_RADIUS_TEXELS"),
            SUN_PCSS_MIN_RADIUS_TEXELS
        );
        assert!(
            shadows.contains("SUN_PCSS_MIN_RADIUS_TEXELS, 9.0)"),
            "shadow_pcss_sun clamps to [min, {SUN_PCSS_MAX_RADIUS_TEXELS}]"
        );
        // The real sun: 0.2665° (the atmosphere's 0.004675 rad).
        assert!((SUN_PCSS_ANGULAR_RADIUS_TAN - 0.004_675_f32.tan()).abs() < 1e-6);
    }

    #[test]
    fn a_thin_close_caster_keeps_a_crisp_shadow_at_hero_distance() {
        // DO27's X-12 side shot: cascade 0 spans 0–16 m, ~1.08 cm texels at 2048²; the fin
        // stands 0.2–1 m off the cargo box. The tracers' penumbra half-width is gap × tan
        // (≤ 4.7 mm); the raster's filter radius must stay at one texel there, not the
        // 1.5–1.6 texels (1.6–1.9 cm) the 1° sun with a 1.5-texel floor gave.
        let texel = 0.0108;
        for gap in [0.2_f32, 0.5, 1.0, 2.0] {
            let r = sun_pcss_radius_texels(gap, texel);
            assert_eq!(r, SUN_PCSS_MIN_RADIUS_TEXELS, "gap {gap} m: {r} texels");
            let old = (gap * 0.0175 / texel).clamp(1.5, 9.0);
            assert!(
                r * texel <= 0.67 * old * texel,
                "gap {gap} m: {r} vs {old} texels"
            );
        }
        // A 5 m gap (a hull over the ground) widens with the sun: 2.2 texels, 2.4 cm.
        let r = sun_pcss_radius_texels(5.0, texel);
        assert!(
            (r - 5.0 * SUN_PCSS_ANGULAR_RADIUS_TAN / texel).abs() < 1e-4,
            "{r}"
        );
    }

    #[test]
    fn the_pcss_radius_is_clamped_to_the_texel_floor_and_ceiling() {
        // Far cascade (1 m texels): no gap reaches past the floor until ~214 m.
        assert_eq!(sun_pcss_radius_texels(100.0, 1.0), SUN_PCSS_MIN_RADIUS_TEXELS);
        assert!(sun_pcss_radius_texels(400.0, 1.0) > SUN_PCSS_MIN_RADIUS_TEXELS);
        // A mesa 2 km up-sun of fine texels stops at the ceiling.
        assert_eq!(sun_pcss_radius_texels(2000.0, 0.01), SUN_PCSS_MAX_RADIUS_TEXELS);
        // Degenerate inputs stay finite.
        assert_eq!(sun_pcss_radius_texels(-1.0, 0.01), SUN_PCSS_MIN_RADIUS_TEXELS);
        assert_eq!(sun_pcss_radius_texels(1.0, 0.0), SUN_PCSS_MAX_RADIUS_TEXELS);
    }

    #[test]
    fn csm_boundaries_keep_the_practical_layout_on_short_ranges() {
        for far in [30.0, 60.0, 200.0] {
            let b = csm_boundaries(0.1, far, 4);
            let mut want = vec![0.1];
            want.extend(cascade_splits(0.1, far, 4, 0.75));
            want.push(far);
            assert_eq!(b, want, "far {far}");
        }
    }

    /// Desktop full-forward (`sample_csm_cascades`) fades over the last 15 % of the last
    /// cascade interval. The lite shader fades over the last 15 % of the range (`hi * 0.15`).
    /// A one-cascade desktop frame starts the interval at 0, so the two agree.
    #[test]
    fn desktop_sun_shadow_fade_is_the_last_15_percent_of_the_last_cascade() {
        fn desktop_fade(bounds: &[f32]) -> (f32, f32) {
            let hi = bounds[bounds.len() - 1];
            let lo = if bounds.len() > 2 {
                bounds[bounds.len() - 2]
            } else {
                0.0
            };
            let width = ((hi - lo) * 0.15).max(1.0e-3);
            (hi - width, width)
        }
        let one = csm_boundaries(0.1, 150.0, 1);
        let (start, width) = desktop_fade(&one);
        assert!((width - 150.0 * 0.15).abs() < 1.0e-3, "{width}");
        assert!((start - 127.5).abs() < 1.0e-3, "{start}");
        for (far, cascades) in [(60.0, 2u32), (60.0, 4), (150.0, 2), (150.0, 4), (400.0, 4)] {
            let bounds = csm_boundaries(0.1, far, cascades);
            let (start, width) = desktop_fade(&bounds);
            let range_width = far * 0.15;
            assert!(
                start > far - range_width + 1.0,
                "far {far} x{cascades}: desktop fade {start} is not later than the range fade"
            );
            assert!(width < range_width, "far {far} x{cascades}: width {width}");
            let hi = bounds[bounds.len() - 1];
            let lo = bounds[bounds.len() - 2];
            assert!((width - (hi - lo) * 0.15).abs() < 1.0e-3, "{bounds:?}");
        }
    }

    #[test]
    fn a_long_view_range_keeps_the_near_cascades_tight() {
        // A 12 km vista: the uncapped layout ended cascade 0 about 750 m out.
        let b = csm_boundaries(0.1, 12_000.0, 4);
        assert_eq!(b.len(), 5);
        assert!((b[1] - 16.0).abs() < 1e-3, "{b:?}");
        assert!((b[2] - 69.28).abs() < 1e-2, "{b:?}");
        assert!((b[3] - 300.0).abs() < 1e-2, "{b:?}");
        assert_eq!(b[4], 12_000.0);
        assert!(b.windows(2).all(|w| w[0] < w[1]), "{b:?}");
        // Two cascades: the one split sits between the ladder ends.
        let two = csm_boundaries(0.1, 12_000.0, 2);
        assert!((two[1] - 126.49).abs() < 1e-2, "{two:?}");
        assert_eq!(csm_boundaries(0.1, 12_000.0, 1), vec![0.1, 12_000.0]);
    }

    #[test]
    fn the_first_cascade_ends_at_16_m_so_a_character_shadow_resolves() {
        // Desktop High's 400 m shadow range: the practical split put cascade 0 at 25.6 m
        // (3.0 cm texels at 2048² for a 55° view), the 40 m cap at 40 m (4.7 cm). A leg is
        // 16 cm across and the receiver bias is a few texels, so at either size a character's
        // shadow came out thin and detached from its feet (`lookdev dyn_light`). At 16 m the
        // cascade is about 1.8 cm per texel.
        let b = csm_boundaries(0.1, 400.0, 4);
        assert!((b[1] - CSM_FIRST_CASCADE_MAX).abs() < 1e-3, "{b:?}");
        assert!(cascade_splits(0.1, 400.0, 4, 0.75)[0] > CSM_FIRST_CASCADE_MAX);
        assert!(b.windows(2).all(|w| w[0] < w[1]), "{b:?}");
    }

    #[test]
    fn cascade_splits_count_2() {
        let splits = cascade_splits(0.1, 100.0, 2, 0.75);
        assert_eq!(splits.len(), 1);

        // Manual computation:
        // z_log = 0.1 * (100.0 / 0.1)^(1/2) = 0.1 * 1000^0.5 = 0.1 * 31.62277... ≈ 3.16228
        // z_uni = 0.1 + (100.0 - 0.1) * 0.5 = 0.1 + 99.9 * 0.5 = 50.05
        // z = 0.75 * z_log + 0.25 * z_uni = 0.75 * 3.16228 + 0.25 * 50.05 ≈ 2.3717 + 12.5125 = 14.884
        let expected = 0.75 * (0.1f32 * (100.0f32 / 0.1f32).powf(0.5)) + 0.25 * (0.1 + 99.9 * 0.5);
        assert!(
            (splits[0] - expected).abs() < 1e-4,
            "got {}, expected {}",
            splits[0],
            expected
        );
    }

    #[test]
    fn cascade_splits_lambda_0_is_uniform() {
        let splits = cascade_splits(1.0, 10.0, 3, 0.0);
        // lambda=0 means pure uniform
        // i=1: 1.0 + 9.0 * 1/3 = 4.0
        // i=2: 1.0 + 9.0 * 2/3 = 7.0
        assert!((splits[0] - 4.0).abs() < 1e-5);
        assert!((splits[1] - 7.0).abs() < 1e-5);
    }

    #[test]
    fn cascade_splits_lambda_1_is_log() {
        let splits = cascade_splits(1.0, 100.0, 2, 1.0);
        // lambda=1 means pure logarithmic
        // z_log = 1.0 * 100^(1/2) = 10.0
        assert!((splits[0] - 10.0).abs() < 1e-5);
    }

    #[test]
    fn snap_to_texels_known_value() {
        // Center at 3.7, texel size 0.5
        // 3.7 / 0.5 = 7.4 → round → 7.0 → 7.0 * 0.5 = 3.5
        let result = snap_to_texels(3.7, 0.5);
        assert!((result - 3.5).abs() < 1e-7);
    }

    #[test]
    fn snap_to_texels_already_aligned() {
        let result = snap_to_texels(4.0, 1.0);
        assert!((result - 4.0).abs() < 1e-7);
    }

    #[test]
    fn sub_texel_camera_motion_keeps_the_shadow_centre() {
        // radius 10, resolution 32 → texel 0.625. Nudge from a point already on the grid.
        let dir = [-0.4, -0.8, -0.3];
        let center = [2.0, 1.0, -3.0];
        let snapped = snap_ortho_center(dir, center, 10.0, 32);
        let again = snap_ortho_center(dir, snapped, 10.0, 32);
        let nudged = snap_ortho_center(dir, [snapped[0] + 0.01, snapped[1], snapped[2]], 10.0, 32);
        let shifted = snap_ortho_center(dir, [snapped[0] + 2.0, snapped[1], snapped[2]], 10.0, 32);
        for c in 0..3 {
            assert!((snapped[c] - again[c]).abs() < 1.0e-4, "snapping is idempotent");
        }
        for c in 0..3 {
            assert!(
                (snapped[c] - nudged[c]).abs() < 1.0e-4,
                "sub-texel move must snap together, snapped {snapped:?} nudged {nudged:?}"
            );
        }
        let moved = (0..3).any(|c| (snapped[c] - shifted[c]).abs() > 0.2);
        assert!(
            moved,
            "a two-meter move must reach another texel, {snapped:?} vs {shifted:?}"
        );
        assert_eq!(snap_ortho_center(dir, center, 10.0, 0), center);
    }

    fn overlaps(a: AtlasRect, b: AtlasRect) -> bool {
        a.x < b.x + b.size && b.x < a.x + a.size && a.y < b.y + b.size && b.y < a.y + a.size
    }

    fn inside(rect: AtlasRect, atlas: u32) -> bool {
        rect.size > 0 && rect.x + rect.size <= atlas && rect.y + rect.size <= atlas
    }

    #[test]
    fn higher_importance_gets_a_larger_or_equal_tile() {
        let rects = layout_local_shadow_tiles(&[1, 1, 1], &[0.2, 9.0, 0.05], 1024);
        assert_eq!(rects.len(), 3);
        assert!(rects[1].size >= rects[0].size);
        assert!(rects[0].size >= rects[2].size);
        assert!(rects[1].size > rects[2].size, "rank 0 must beat rank 2");
    }

    #[test]
    fn rank_zero_spot_is_larger_than_rank_five() {
        let importance = [1.0, 2.0, 3.0, 4.0, 5.0, 100.0];
        let rects = layout_local_shadow_tiles(&[1; 6], &importance, 1024);
        let top = rects.iter().max_by_key(|rect| rect.size).expect("tiles");
        let bottom = rects.iter().min_by_key(|rect| rect.size).expect("tiles");
        assert!(top.size > bottom.size);
        assert_eq!(local_shadow_tile_size(0, 1024, 1), 256);
        assert_eq!(local_shadow_tile_size(5, 1024, 1), 64);
        // Rank 0 on a 1024 atlas is 256, and six of those faces fit.
        assert_eq!(local_shadow_tile_size(0, 1024, 6), 256);
        // Below 64 the second loop may return a smaller power of two, or 1 when nothing packs.
        assert_eq!(local_shadow_tile_size(0, 128, 6), 32);
        assert_eq!(local_shadow_tile_size(0, 1, 6), 1);
    }

    #[test]
    fn a_point_lights_six_faces_share_one_tile_size() {
        let rects = layout_local_shadow_tiles(&[6], &[3.0], 1024);
        assert_eq!(rects.len(), 6);
        assert!(rects.iter().all(|rect| rect.size == rects[0].size));
        assert!(rects.iter().all(|rect| inside(*rect, 1024)));
        for (i, a) in rects.iter().enumerate() {
            for b in rects.iter().skip(i + 1) {
                assert!(!overlaps(*a, *b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn four_spots_pack_into_a_1024_atlas_without_overlap() {
        let rects = layout_local_shadow_tiles(&[1, 1, 1, 1], &[8.0, 4.0, 2.0, 1.0], 1024);
        assert_eq!(rects.len(), 4);
        assert!(rects[0].size > rects[3].size);
        for rect in &rects {
            assert!(inside(*rect, 1024), "{rect:?} left the atlas");
        }
        for (i, a) in rects.iter().enumerate() {
            for b in rects.iter().skip(i + 1) {
                assert!(!overlaps(*a, *b));
            }
        }
    }

    #[test]
    fn packed_tiles_shrink_until_a_full_budget_fits() {
        // 8 points are 48 faces. The rank-0 class (256) cannot hold them in
        // a 1024 atlas, so the packer steps the largest class down.
        let faces = [6u32; 8];
        let importance: Vec<f32> = (0..8).map(|i| 1.0 / (i as f32 + 1.0)).collect();
        let rects = layout_local_shadow_tiles(&faces, &importance, 1024);
        assert_eq!(rects.len(), 48);
        for (i, a) in rects.iter().enumerate() {
            assert!(inside(*a, 1024), "{a:?}");
            for b in rects.iter().skip(i + 1) {
                assert!(!overlaps(*a, *b));
            }
        }
        // Faces of light 0 are the first six rects and share a size.
        assert!(rects[..6].iter().all(|rect| rect.size == rects[0].size));
    }
}
