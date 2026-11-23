// SPDX-License-Identifier: MPL-2.0
//! Tiled culling of projected decals.
//!
//! Each decal is an oriented box. The box is tested against the view (behind the camera, or
//! its screen AABB misses the viewport) and the survivors are binned into screen tiles, the
//! same screen-space clustering used for lights (Olsson, Billeter, Assarsson, HPG 2012). The
//! GPU draws one screen quad per survivor, clipped to the union of the tiles it touches,
//! in priority order.

use std::cell::Cell;

use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;

use crate::decal::{Decal, DecalSurface, distance_fade};

/// Default tile edge, in pixels.
pub const DECAL_TILE_PX: u32 = 16;

/// Upper bound on the tile grid (16M tiles, far past any real display). Larger frames
/// get an empty bin instead of an allocation that wraps or exhausts memory.
pub const MAX_TILES: u64 = 1 << 24;

/// CPU-side budget snapshot the agent reads (`vfx.decal_budget`).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecalCullStats {
    /// Pool capacity.
    pub capacity: u32,
    /// Live decals before the frustum test.
    pub live: u32,
    /// Dropped by the frustum or the distance fade.
    pub culled: u32,
    /// Tiles that hold at least one decal.
    pub tiles_touched: u32,
}

/// One frame of tiled decal culling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecalBin {
    /// Tile edge in pixels.
    pub tile_px: u32,
    /// Tiles across.
    pub tiles_x: u32,
    /// Tiles down.
    pub tiles_y: u32,
    /// Inclusive tile rectangle of each entry of [`Self::visible`], in the same order:
    /// `[tx0, ty0, tx1, ty1]`.
    pub tile_rects: Vec<[u32; 4]>,
    /// Visible decal indices, lowest priority first (drawn first).
    pub visible: Vec<u16>,
    /// How many inputs were dropped.
    pub culled: usize,
    /// Probes recorded by [`bin_decals`] (one per survivor) and [`quads_from_tiles`]
    /// (one per chosen decal). Callers that only need the budget use [`DecalBin::tiles_touched`].
    visits: Cell<u64>,
}

impl DecalBin {
    /// How many tiles hold at least one decal.
    ///
    /// Overlapping rectangles count once. The count is the area of the union, not a walk of
    /// the frame's tile grid.
    pub fn tiles_touched(&self) -> usize {
        tile_union_area(&self.tile_rects)
    }

    /// Tile probes recorded for this bin. One per visible decal while binning, plus one per
    /// chosen decal when quads are built.
    pub fn tile_visits(&self) -> u64 {
        self.visits.get()
    }
}

/// Area of the union of inclusive integer rectangles `[x0, y0, x1, y1]`.
///
/// A sweep over the distinct y edges merges the active x intervals, so the cost follows the
/// rectangle count (a few hundred decals), not the tile grid.
fn tile_union_area(rects: &[[u32; 4]]) -> usize {
    let mut edges: Vec<(u32, u32, u32, bool)> = Vec::with_capacity(rects.len().saturating_mul(2));
    for rect in rects {
        let (x0, y0, x1, y1) = (rect[0], rect[1], rect[2], rect[3]);
        if x1 < x0 || y1 < y0 {
            continue;
        }
        edges.push((y0, x0, x1, true));
        let close = y1.saturating_add(1);
        if close > y1 {
            edges.push((close, x0, x1, false));
        }
    }
    edges.sort_by_key(|edge| edge.0);
    let mut active: Vec<(u32, u32)> = Vec::new();
    let mut covered = 0u64;
    let mut index = 0;
    while index < edges.len() {
        let y = edges[index].0;
        while index < edges.len() && edges[index].0 == y {
            let (_, x0, x1, open) = edges[index];
            if open {
                active.push((x0, x1));
            } else if let Some(pos) = active.iter().position(|&(a, b)| a == x0 && b == x1) {
                active.swap_remove(pos);
            }
            index += 1;
        }
        if active.is_empty() || index >= edges.len() {
            continue;
        }
        let span = u64::from(edges[index].0.saturating_sub(y));
        covered = covered.saturating_add(merged_tile_width(&active).saturating_mul(span));
    }
    usize::try_from(covered).unwrap_or(usize::MAX)
}

/// Width of the union of inclusive x intervals.
fn merged_tile_width(intervals: &[(u32, u32)]) -> u64 {
    if intervals.is_empty() {
        return 0;
    }
    let mut ordered = intervals.to_vec();
    ordered.sort_unstable();
    let mut covered = 0u64;
    let mut left = ordered[0].0;
    let mut right = ordered[0].1;
    for &(start, end) in ordered.iter().skip(1) {
        if start <= right.saturating_add(1) {
            right = right.max(end);
        } else {
            covered = covered.saturating_add(u64::from(right - left).saturating_add(1));
            left = start;
            right = end;
        }
    }
    covered.saturating_add(u64::from(right - left).saturating_add(1))
}

/// Bins `decals` into screen tiles. `surfaces` is parallel; a missing surface uses priority 0
/// and no distance fade.
///
/// `view_proj` is the same matrix the shader uses ([`crate::gpu::VfxCamera::matrices`]).
#[tracing::instrument(skip_all)]
pub fn bin_decals(
    decals: &[Decal],
    surfaces: &[DecalSurface],
    view_proj: Mat4,
    eye: Vec3,
    width: u32,
    height: u32,
    tile_px: u32,
) -> DecalBin {
    let width = width.max(1);
    let height = height.max(1);
    let tile_px = tile_px.max(1);
    let tiles_x = width.div_ceil(tile_px);
    let tiles_y = height.div_ceil(tile_px);
    // u64 product: a u32 multiply wraps for huge sizes and leaves an empty grid to index.
    let tile_count = u64::from(tiles_x) * u64::from(tiles_y);
    if tile_count > MAX_TILES {
        return DecalBin {
            tile_px,
            tiles_x: 0,
            tiles_y: 0,
            tile_rects: Vec::new(),
            visible: Vec::new(),
            culled: decals.len(),
            visits: Cell::new(0),
        };
    }
    let mut ranked: Vec<(i16, u16, [u32; 4])> = Vec::new();
    let mut culled = 0usize;

    for (index, decal) in decals.iter().enumerate() {
        let surface = surfaces.get(index);
        if let Some(surface) = surface {
            let end = surface.distance_fade[1];
            let start = surface.distance_fade[0];
            if end > start {
                let radius = decal.half_size.max_element();
                let dist = (decal.position - eye).length();
                if dist - radius > end || distance_fade(dist, start, end) <= 0.0 && dist > end {
                    culled += 1;
                    continue;
                }
            }
        }
        let Some((min_x, min_y, max_x, max_y)) = screen_aabb(decal, view_proj, width, height) else {
            culled += 1;
            continue;
        };
        let idx = u16::try_from(index).unwrap_or(u16::MAX);
        let priority = surface.map(|s| s.priority).unwrap_or(0);
        let tx0 = (min_x / tile_px).min(tiles_x.saturating_sub(1));
        let tx1 = (max_x / tile_px).min(tiles_x.saturating_sub(1));
        let ty0 = (min_y / tile_px).min(tiles_y.saturating_sub(1));
        let ty1 = (max_y / tile_px).min(tiles_y.saturating_sub(1));
        ranked.push((priority, idx, [tx0, ty0, tx1, ty1]));
    }

    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let visits = u64::try_from(ranked.len()).unwrap_or(u64::MAX);
    let mut tile_rects = Vec::with_capacity(ranked.len());
    let mut visible = Vec::with_capacity(ranked.len());
    for (_, index, rect) in ranked {
        visible.push(index);
        tile_rects.push(rect);
    }
    DecalBin {
        tile_px,
        tiles_x,
        tiles_y,
        tile_rects,
        visible,
        culled,
        visits: Cell::new(visits),
    }
}

/// Inclusive pixel AABB of the decal's projected box, or `None` when it is outside the view.
fn screen_aabb(decal: &Decal, view_proj: Mat4, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let right = decal.rotation * Vec3::X * decal.half_size.x;
    let up = decal.rotation * Vec3::Y * decal.half_size.y;
    let fwd = decal.rotation * Vec3::Z * decal.half_size.z;
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    let mut in_front = 0u32;
    for sx in [-1.0, 1.0] {
        for sy in [-1.0, 1.0] {
            for sz in [-1.0, 1.0] {
                let world = decal.position + right * sx + up * sy + fwd * sz;
                let clip = view_proj * world.extend(1.0);
                if clip.w <= 1e-4 {
                    continue;
                }
                in_front += 1;
                let ndc_x = clip.x / clip.w;
                let ndc_y = clip.y / clip.w;
                let sx = (ndc_x * 0.5 + 0.5) * width as f32;
                let sy = (1.0 - (ndc_y * 0.5 + 0.5)) * height as f32;
                min_x = min_x.min(sx);
                min_y = min_y.min(sy);
                max_x = max_x.max(sx);
                max_y = max_y.max(sy);
            }
        }
    }
    if in_front == 0 {
        return None;
    }
    // Corners behind the near plane do not project. Expand to the viewport so the tile
    // union still covers the clipped box (a tight AABB of the surviving corners is not).
    if in_front < 8 {
        min_x = 0.0;
        min_y = 0.0;
        max_x = width as f32 - 1.0;
        max_y = height as f32 - 1.0;
    }
    if max_x < 0.0 || max_y < 0.0 || min_x >= width as f32 || min_y >= height as f32 {
        return None;
    }
    let x0 = min_x.floor().max(0.0) as u32;
    let y0 = min_y.floor().max(0.0) as u32;
    let x1 = (max_x.ceil() as u32).saturating_sub(1).min(width - 1);
    let y1 = (max_y.ceil() as u32).saturating_sub(1).min(height - 1);
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

/// One decal's screen quad: the union of the 16 px tiles that contain it.
///
/// `rect` is `(x0, y0, x1, y1)` in pixels with `x1` and `y1` exclusive.
/// `index` is the decal's index in the slice passed to [`bin_decals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecalQuad {
    /// Inclusive-min, exclusive-max pixel rectangle.
    pub rect: [u32; 4],
    /// Index into the decal slice.
    pub index: u16,
}

/// One screen quad per `chosen` decal, from the tiles [`bin_decals`] recorded.
///
/// `chosen` is the budget-trimmed visible list (lowest priority first). A chosen index
/// that no tile lists is skipped.
#[must_use]
pub fn quads_from_tiles(bin: &DecalBin, chosen: &[u16], width: u32, height: u32) -> Vec<DecalQuad> {
    let width = width.max(1);
    let height = height.max(1);
    let tile = bin.tile_px.max(1);
    let mut lookup: Vec<Option<[u32; 4]>> = Vec::new();
    for (i, &index) in bin.visible.iter().enumerate() {
        let Some(rect) = bin.tile_rects.get(i).copied() else {
            continue;
        };
        let slot = index as usize;
        if lookup.len() <= slot {
            lookup.resize(slot + 1, None);
        }
        lookup[slot] = Some(rect);
    }
    let mut out = Vec::with_capacity(chosen.len());
    for &index in chosen {
        bin.visits.set(bin.visits.get().saturating_add(1));
        let Some(Some([tx0, ty0, tx1, ty1])) = lookup.get(index as usize).copied() else {
            continue;
        };
        let x0 = tx0.saturating_mul(tile).min(width);
        let y0 = ty0.saturating_mul(tile).min(height);
        let x1 = tx1.saturating_add(1).saturating_mul(tile).min(width);
        let y1 = ty1.saturating_add(1).saturating_mul(tile).min(height);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        out.push(DecalQuad {
            rect: [x0, y0, x1, y1],
            index,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decal::{DecalSurface, align_to_normal};
    use crate::gpu::VfxCamera;
    use regia_core::math::Quat;

    fn flat(position: Vec3) -> Decal {
        Decal {
            position,
            rotation: align_to_normal(Vec3::Y, 0.0),
            half_size: Vec3::new(0.4, 0.4, 0.25),
            color: [1.0, 1.0, 1.0, 1.0],
            age: 0.0,
            lifetime: None,
        }
    }

    #[test]
    fn tiles_split_left_and_right_and_drop_what_is_behind() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, 8.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(16.0 / 9.0);
        let decals = [
            flat(Vec3::new(-3.0, 0.0, 0.0)),
            flat(Vec3::new(3.0, 0.0, 0.0)),
            flat(camera.eye + (camera.eye - camera.target)),
        ];
        let surfaces = [DecalSurface::from_decal(&decals[0]); 3];
        let bin = bin_decals(&decals, &surfaces, view_proj, camera.eye, 320, 180, 16);
        assert!(bin.visible.contains(&0), "left decal visible: {:?}", bin.visible);
        assert!(bin.visible.contains(&1), "right decal visible");
        assert!(!bin.visible.contains(&2), "behind the camera is culled");
        assert!(bin.culled >= 1);
        let rect_of = |index: u16| {
            let pos = bin.visible.iter().position(|&i| i == index).expect("visible");
            bin.tile_rects[pos]
        };
        let left = rect_of(0);
        let right = rect_of(1);
        assert!(
            left[2] < right[0],
            "left tile rect {left:?} reaches the right rect {right:?}"
        );
    }

    #[test]
    fn higher_priority_is_drawn_last_and_far_decals_drop() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 4.0, 6.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(1.0);
        let mut low = flat(Vec3::ZERO);
        low.rotation = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
        let high = Decal {
            position: Vec3::new(0.2, 0.0, 0.0),
            ..low
        };
        let mut far = low;
        far.position = Vec3::new(0.0, 0.0, -80.0);
        let mut near_surface = DecalSurface::from_decal(&low);
        near_surface.priority = 1;
        let mut high_surface = near_surface;
        high_surface.priority = 5;
        let mut far_surface = near_surface;
        far_surface.distance_fade = [10.0, 20.0];
        let bin = bin_decals(
            &[low, high, far],
            &[near_surface, high_surface, far_surface],
            view_proj,
            camera.eye,
            128,
            128,
            32,
        );
        assert_eq!(bin.visible.last().copied(), Some(1), "priority 5 draws last");
        assert!(!bin.visible.contains(&2), "past the distance fade is culled");
    }

    #[test]
    fn left_decal_quad_does_not_cover_the_right_side() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, 8.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(16.0 / 9.0);
        let decals = [flat(Vec3::new(-3.0, 0.0, 0.0)), flat(Vec3::new(3.0, 0.0, 0.0))];
        let surfaces = [DecalSurface::from_decal(&decals[0]); 2];
        let width = 320u32;
        let height = 180u32;
        let bin = bin_decals(&decals, &surfaces, view_proj, camera.eye, width, height, 16);
        let quads = quads_from_tiles(&bin, &bin.visible, width, height);
        let left = quads.iter().find(|quad| quad.index == 0).expect("left quad");
        let right = quads.iter().find(|quad| quad.index == 1).expect("right quad");
        assert!(
            left.rect[2] <= width / 2,
            "left tile union {:?} reaches the right half",
            left.rect
        );
        assert!(
            right.rect[0] >= width / 2,
            "right tile union {:?} reaches the left half",
            right.rect
        );
        assert!(left.rect[2] <= right.rect[0], "quads overlap: {left:?} {right:?}");
    }

    #[test]
    fn near_clipped_box_quad_covers_the_viewport() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, 5.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(1.0);
        let decal = Decal {
            position: camera.eye,
            half_size: Vec3::splat(2.0),
            ..flat(Vec3::ZERO)
        };
        let surface = DecalSurface::from_decal(&decal);
        let width = 64u32;
        let height = 64u32;
        let bin = bin_decals(&[decal], &[surface], view_proj, camera.eye, width, height, 16);
        assert!(
            bin.visible.contains(&0),
            "a box around the eye is not fully behind"
        );
        let quads = quads_from_tiles(&bin, &bin.visible, width, height);
        let quad = quads.first().expect("quad");
        assert_eq!(quad.rect, [0, 0, width, height], "near-clipped box expands");
    }

    #[test]
    fn mesh_scorch_quad_contains_its_centre() {
        use crate::builtin_prefab;
        use crate::project_pixel;
        let camera = VfxCamera::looking_at(Vec3::new(8.0, 6.5, 10.0), Vec3::new(1.0, 0.4, 0.4));
        let (_, _, view_proj) = camera.matrices(640.0 / 360.0);
        let (decal, surface) = builtin_prefab("scorch")
            .expect("scorch")
            .instantiate(Vec3::new(2.2, 1.02, 0.0), Vec3::Y, 0.0, None)
            .expect("place");
        let bin = bin_decals(&[decal], &[surface], view_proj, camera.eye, 640, 360, 16);
        let quads = quads_from_tiles(&bin, &bin.visible, 640, 360);
        let px = project_pixel(&camera, 640, 360, Vec3::new(2.2, 1.0, 0.0)).expect("on screen");
        let quad = quads.first().expect("quad");
        assert!(
            px.0 >= quad.rect[0] && px.0 < quad.rect[2] && px.1 >= quad.rect[1] && px.1 < quad.rect[3],
            "pixel {px:?} outside {:?}",
            quad.rect
        );
    }

    #[test]
    fn equal_priority_draws_in_slice_order_and_priority_beats_slice_order() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 4.0, 6.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(1.0);
        let base = flat(Vec3::ZERO);
        let decals = [base, base, base, base];
        let mut surfaces = [DecalSurface::from_decal(&base); 4];
        surfaces[0].priority = 2;
        surfaces[3].priority = -1;
        let bin = bin_decals(&decals, &surfaces, view_proj, camera.eye, 128, 128, 32);
        assert_eq!(
            bin.visible,
            vec![3, 1, 2, 0],
            "lowest priority first; ties keep slice order (callers pass spawn order)"
        );
    }

    #[test]
    fn a_decal_off_to_the_side_is_culled_without_touching_tiles() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, 8.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(1.0);
        let side = flat(Vec3::new(200.0, 0.0, 0.0));
        let surface = DecalSurface::from_decal(&side);
        let bin = bin_decals(&[side], &[surface], view_proj, camera.eye, 64, 64, 16);
        assert!(bin.visible.is_empty());
        assert_eq!(bin.culled, 1);
        assert_eq!(bin.tiles_touched(), 0);
    }
    /// Pixel rect a tile-aligned screen AABB becomes: inclusive pixels snapped out to the
    /// tile grid, then exclusive edges clamped to the frame.
    fn tile_aligned_rect(aabb: (u32, u32, u32, u32), tile: u32, width: u32, height: u32) -> [u32; 4] {
        let tile = tile.max(1);
        let tiles_x = width.div_ceil(tile);
        let tiles_y = height.div_ceil(tile);
        let (x0, y0, x1, y1) = aabb;
        let tx0 = (x0 / tile).min(tiles_x.saturating_sub(1));
        let ty0 = (y0 / tile).min(tiles_y.saturating_sub(1));
        let tx1 = (x1 / tile).min(tiles_x.saturating_sub(1));
        let ty1 = (y1 / tile).min(tiles_y.saturating_sub(1));
        [
            tx0.saturating_mul(tile).min(width),
            ty0.saturating_mul(tile).min(height),
            (tx1.saturating_add(1).saturating_mul(tile)).min(width),
            (ty1.saturating_add(1).saturating_mul(tile)).min(height),
        ]
    }

    #[test]
    fn four_k_quads_match_the_tile_aabb_and_visits_scale_with_decals() {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, 8.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(16.0 / 9.0);
        let width = 3840u32;
        let height = 2160u32;
        let tile = 16u32;
        let mut decals = vec![
            flat(Vec3::new(-3.0, 0.0, 0.0)),
            flat(Vec3::new(3.0, 0.0, 0.0)),
            flat(Vec3::ZERO),
        ];
        // A box around the eye has corners behind the near plane and must still be one
        // full-viewport quad, not a per-tile stamp.
        decals.push(Decal {
            position: camera.eye,
            half_size: Vec3::splat(2.0),
            ..flat(Vec3::ZERO)
        });
        let surfaces = vec![DecalSurface::from_decal(&decals[0]); decals.len()];
        let bin = bin_decals(&decals, &surfaces, view_proj, camera.eye, width, height, tile);
        assert!(
            bin.visible.len() >= 4,
            "all four decals project: {:?}",
            bin.visible
        );
        let quads = quads_from_tiles(&bin, &bin.visible, width, height);
        assert_eq!(quads.len(), bin.visible.len());
        for quad in &quads {
            let decal = &decals[quad.index as usize];
            let aabb = screen_aabb(decal, view_proj, width, height).expect("visible");
            let expect = tile_aligned_rect(aabb, tile, width, height);
            assert_eq!(
                quad.rect, expect,
                "quad {quad:?} is not the tile-aligned AABB {expect:?}"
            );
        }
        let full = quads.iter().find(|quad| quad.index == 3).expect("near-clipped");
        assert_eq!(
            full.rect,
            [0, 0, width, height],
            "near-clipped box covers the viewport"
        );
        // 320x180 and 640x360 stay the small-frame cases the rest of the suite checks.
        let small = bin_decals(
            &decals[..2],
            &surfaces[..2],
            view_proj,
            camera.eye,
            320,
            180,
            tile,
        );
        let small_quads = quads_from_tiles(&small, &small.visible, 320, 180);
        for quad in &small_quads {
            let aabb = screen_aabb(&decals[quad.index as usize], view_proj, 320, 180).expect("visible");
            assert_eq!(quad.rect, tile_aligned_rect(aabb, tile, 320, 180));
        }
        let visits = bin.tile_visits();
        let n = bin.visible.len() as u64;
        // Binning records one probe per survivor and quad building one probe per chosen decal.
        assert!(
            visits <= n.saturating_mul(4),
            "tile visits {visits} are not proportional to {n} visible decals (4K grid is {} tiles)",
            (width.div_ceil(tile) as u64) * (height.div_ceil(tile) as u64)
        );
    }

    #[test]
    fn huge_frame_returns_empty_bin_instead_of_wrapping() {
        // 65536 x 65536 tiles: the u32 product wraps to 0.
        let d = flat(Vec3::ZERO);
        let bin = bin_decals(&[d], &[], Mat4::IDENTITY, Vec3::ZERO, 1_048_576, 1_048_576, 16);
        assert!(bin.tile_rects.is_empty());
        assert_eq!(bin.tiles_x, 0);
        assert!(bin.visible.is_empty());
        assert_eq!(bin.culled, 1);
    }
}
