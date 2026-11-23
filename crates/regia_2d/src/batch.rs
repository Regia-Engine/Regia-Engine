// SPDX-License-Identifier: MPL-2.0
//! CPU side of the sprite pass: sprites, 9-slices and tilemap layers become instanced quads,
//! sorted back to front and split into batches on texture changes.
//!
//! One instance is one textured parallelogram: an origin plus two edge vectors in world space,
//! a source rectangle in UV space and the three Tiled flip bits. Ordering is the painter's
//! algorithm on world z (stable, so equal z keeps submission order); there is no depth buffer.

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use regia_core::glam::{DAffine3, DVec2};
use regia_core::math::{Affine3A, Vec2, Vec3};

use crate::atlas::NineSlice;
use crate::camera::Rect2;
use crate::sprite::{GpuImage, LoadedAtlas, LoadedTileMap, Sprite, SpriteSource, TileMap};
use crate::tilemap::{MapLayer, TileFlags, split_gid};

/// Instance flag: mirror the source horizontally.
pub const FLAG_FLIP_X: u32 = 1;
/// Instance flag: mirror the source vertically.
pub const FLAG_FLIP_Y: u32 = 2;
/// Instance flag: transpose the source (Tiled's diagonal flip; applied after the mirrors when
/// sampling, i.e. first as an image operation).
pub const FLAG_DIAGONAL: u32 = 4;

/// One quad as the GPU reads it (per-instance vertex data, 60 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct SpriteInstance {
    /// World position of the quad's bottom-left corner.
    pub origin: [f32; 2],
    /// World vector along the quad's bottom edge.
    pub axis_x: [f32; 2],
    /// World vector along the quad's left edge.
    pub axis_y: [f32; 2],
    /// UV of the source rectangle's top-left.
    pub uv_min: [f32; 2],
    /// UV of the source rectangle's bottom-right.
    pub uv_max: [f32; 2],
    /// Tint, sRGB-encoded as authored (`Sprite::color`); `SpriteRenderer::draw` converts a copy to
    /// linear when the target format is sRGB, so this stored value is not target space.
    pub color: [f32; 4],
    /// `FLAG_*` bits.
    pub flags: u32,
}

/// A quad waiting to be sorted.
#[derive(Clone, Debug)]
struct Item {
    z: f32,
    seq: u32,
    /// Index into [`Frame2d::images`]. One tileset image is stored once, not once per tile.
    image: u32,
    instance: SpriteInstance,
}

/// The quads of one frame, in submission order.
#[derive(Default, Debug)]
pub struct Frame2d {
    items: Vec<Item>,
    /// Distinct images referenced by `items`, in first-use order.
    images: Vec<Arc<GpuImage>>,
    /// Time used for animated tiles, in milliseconds.
    pub time_ms: u64,
    /// World rectangle the camera sees (tiles outside are skipped); `None` draws everything.
    pub visible: Option<Rect2>,
}

thread_local! {
    /// How many animation-frame durations the per-cell scan in [`animated_tile`] has read
    /// on this thread. The tilemap path uses the loaded prefix and does not increment this.
    static ANIM_DURATION_ADDS: Cell<u64> = const { Cell::new(0) };
}

/// How many animation-frame durations [`animated_tile`] has read on this thread.
#[cfg(test)]
#[must_use]
pub(crate) fn anim_duration_adds() -> u64 {
    ANIM_DURATION_ADDS.with(Cell::get)
}

/// Zeroes [`anim_duration_adds`] on this thread.
#[cfg(test)]
pub(crate) fn reset_anim_duration_adds() {
    ANIM_DURATION_ADDS.with(|cell| cell.set(0));
}

fn note_anim_duration_add() {
    ANIM_DURATION_ADDS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

/// A sorted frame: instances plus `(image, instance range)` batches.
#[derive(Default, Debug, Clone)]
pub struct PreparedFrame {
    /// The instances, back to front.
    pub instances: Vec<SpriteInstance>,
    /// Consecutive instances sharing a texture.
    pub batches: Vec<(Arc<GpuImage>, Range<u32>)>,
    /// Identity of this extracted frame.
    ///
    /// `0` means the frame was built by [`Frame2d::prepare`] and not published by extraction.
    /// Extraction publishes `1`, `2`, `3`, … even when it reuses the instance buffer, so the
    /// overlay can tell a new frame from a recycled allocation.
    pub generation: u64,
}

/// Decomposed 2D placement: where local `(x, y)` lands in the world.
#[derive(Clone, Copy, Debug)]
struct Place {
    affine: Affine3A,
    precise: DAffine3,
}

impl Place {
    fn new(affine: &Affine3A) -> Self {
        Self {
            affine: *affine,
            precise: affine.as_daffine3(),
        }
    }
    fn point(&self, p: DVec2) -> [f32; 2] {
        // Apply a map's translation before rounding its far-away local coordinates for the GPU.
        let w = self.precise.transform_point3(p.extend(0.0));
        [w.x as f32, w.y as f32]
    }
    fn vector(&self, v: Vec2) -> [f32; 2] {
        let w = self.affine.transform_vector3(Vec3::new(v.x, v.y, 0.0));
        [w.x, w.y]
    }
    fn z(&self) -> f32 {
        self.affine.translation.z
    }
}

impl Frame2d {
    /// An empty frame.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of quads so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when no quads were added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drops the quads and keeps the item and image capacity for the next extract.
    pub fn clear(&mut self) {
        self.items.clear();
        self.images.clear();
        self.time_ms = 0;
        self.visible = None;
    }

    /// Index of `image` in `images`, appending it the first time this frame sees that key.
    fn image_index(&mut self, image: &Arc<GpuImage>) -> u32 {
        if let Some(last) = self.images.last()
            && last.key == image.key
        {
            return (self.images.len() - 1) as u32;
        }
        if let Some(index) = self.images.iter().position(|stored| stored.key == image.key) {
            return index as u32;
        }
        self.images.push(Arc::clone(image));
        (self.images.len() - 1) as u32
    }

    #[allow(clippy::too_many_arguments)] // one quad: placement, local rect, source rect, image, tint, flags
    fn push_quad(
        &mut self,
        place: &Place,
        z: f32,
        local_min: DVec2,
        local_size: Vec2,
        image: &Arc<GpuImage>,
        src: [u32; 4],
        color: [f32; 4],
        flags: u32,
    ) {
        let (iw, ih) = (image.image.width.max(1) as f32, image.image.height.max(1) as f32);
        let [sx, sy, sw, sh] = src.map(|v| v as f32);
        let seq = self.items.len() as u32;
        let image = self.image_index(image);
        self.items.push(Item {
            // `total_cmp` orders -0.0 before 0.0, but they are the same depth: keep submission order.
            z: if z == 0.0 { 0.0 } else { z },
            seq,
            image,
            instance: SpriteInstance {
                origin: place.point(local_min),
                axis_x: place.vector(Vec2::new(local_size.x, 0.0)),
                axis_y: place.vector(Vec2::new(0.0, local_size.y)),
                uv_min: [sx / iw, sy / ih],
                uv_max: [(sx + sw) / iw, (sy + sh) / ih],
                color,
                flags,
            },
        });
    }

    /// Adds a sprite at `transform` (its world matrix). Returns false when the source is not in
    /// the atlas.
    /// Finite positive tint alpha is capped at 1; other alpha values emit no quads.
    pub fn push_sprite(
        &mut self,
        atlas: &LoadedAtlas,
        sprite: &Sprite,
        transform: &Affine3A,
        tint: [f32; 4],
    ) -> bool {
        let (page, rect, source_size, trim_offset, pivot, nine) = match &sprite.source {
            SpriteSource::Named(name) => {
                let Some(s) = atlas.atlas.sprite(name) else {
                    return false;
                };
                (
                    s.page,
                    s.rect,
                    s.source_size,
                    s.trim_offset,
                    s.pivot,
                    s.nine_slice,
                )
            }
            SpriteSource::Rect { rect } => (0, *rect, [rect[2], rect[3]], [0, 0], [0.5, 0.5], None),
        };
        let Some(image) = atlas.pages.get(page as usize) else {
            return false;
        };
        let Some(tint) = drawable_tint(tint) else {
            return true;
        };
        let source = Vec2::new(source_size[0] as f32, source_size[1] as f32);
        let size = sprite.size.map_or(source, |[w, h]| Vec2::new(w, h));
        let pivot = Vec2::from(sprite.anchor.unwrap_or(pivot));
        let place = Place::new(transform);
        let z = place.z();
        let base = -pivot * size;
        let mut flags = 0;
        if sprite.flip_x {
            flags |= FLAG_FLIP_X;
        }
        if sprite.flip_y {
            flags |= FLAG_FLIP_Y;
        }

        if let (Some(nine), Some(_)) = (nine, sprite.size) {
            self.push_nine_slice(&place, z, base, size, image, rect, nine, tint, flags);
            return true;
        }

        // The trimmed rectangle inside the (scaled) source box, y up.
        let scale = size / source.max(Vec2::ONE);
        let [tx, ty] = trim_offset.map(|v| v as f32);
        let (w, h) = (rect[2] as f32, rect[3] as f32);
        let mut min = Vec2::new(tx, source.y - (ty + h)) * scale;
        let quad = Vec2::new(w, h) * scale;
        if sprite.flip_x {
            min.x = size.x - (min.x + quad.x);
        }
        if sprite.flip_y {
            min.y = size.y - (min.y + quad.y);
        }
        self.push_quad(&place, z, (base + min).as_dvec2(), quad, image, rect, tint, flags);
        true
    }

    /// Nine quads: fixed corners, edges stretched along one axis, the centre along both.
    #[allow(clippy::too_many_arguments)]
    fn push_nine_slice(
        &mut self,
        place: &Place,
        z: f32,
        base: Vec2,
        size: Vec2,
        image: &Arc<GpuImage>,
        rect: [u32; 4],
        nine: NineSlice,
        tint: [f32; 4],
        flags: u32,
    ) {
        let [rx, ry, rw, rh] = rect;
        let (l, r) = (nine.left.min(rw), nine.right.min(rw.saturating_sub(nine.left)));
        let (t, b) = (nine.top.min(rh), nine.bottom.min(rh.saturating_sub(nine.top)));
        // Source columns/rows (x left→right, y top→bottom) and destination columns/rows (x
        // left→right, y bottom→top). Borders shrink proportionally when the size is too small.
        let fit = |a: f32, c: f32, total: f32| {
            let sum = a + c;
            if sum > total && sum > 0.0 {
                (a * total / sum, c * total / sum)
            } else {
                (a, c)
            }
        };
        let (dl, dr) = fit(l as f32, r as f32, size.x);
        let (db, dt) = fit(b as f32, t as f32, size.y);
        let (Some(cx1), Some(cx2), Some(ry1), Some(ry2)) = (
            rx.checked_add(l),
            rx.checked_add(rw).and_then(|v| v.checked_sub(r)),
            ry.checked_add(t),
            ry.checked_add(rh).and_then(|v| v.checked_sub(b)),
        ) else {
            return;
        };
        let src_cols = [(rx, l), (cx1, rw.saturating_sub(l + r)), (cx2, r)];
        let src_rows = [(ry, t), (ry1, rh.saturating_sub(t + b)), (ry2, b)];
        let dst_cols = [(0.0, dl), (dl, size.x - dl - dr), (size.x - dr, dr)];
        // Destination rows bottom→top pair with source rows bottom→top.
        let dst_rows = [(size.y - dt, dt), (db, size.y - db - dt), (0.0, db)];
        for (row, &(sy, sh)) in src_rows.iter().enumerate() {
            for (col, &(sx, sw)) in src_cols.iter().enumerate() {
                let (dx, dw) = dst_cols[col];
                let (dy, dh) = dst_rows[row];
                if sw == 0 || sh == 0 || dw <= 0.0 || dh <= 0.0 {
                    continue;
                }
                let mut min = Vec2::new(dx, dy);
                if flags & FLAG_FLIP_X != 0 {
                    min.x = size.x - (dx + dw);
                }
                if flags & FLAG_FLIP_Y != 0 {
                    min.y = size.y - (dy + dh);
                }
                self.push_quad(
                    place,
                    z,
                    (base + min).as_dvec2(),
                    Vec2::new(dw, dh),
                    image,
                    [sx, sy, sw, sh],
                    tint,
                    flags,
                );
            }
        }
    }

    /// Adds every visible tile of every visible layer of `map` at `transform` (the map's
    /// bottom-left corner). Returns the number of tiles added.
    pub fn push_tilemap(
        &mut self,
        loaded: &LoadedTileMap,
        component: &TileMap,
        transform: &Affine3A,
        tint: [f32; 4],
    ) -> usize {
        let map = &loaded.map;
        let place = Place::new(transform);
        let (tw, th) = (f64::from(map.tile_width), f64::from(map.tile_height));
        let tile_scale = map
            .tilesets
            .iter()
            .map(|s| s.tile_width.max(s.tile_height))
            .max()
            .unwrap_or(0) as f64;
        // The camera's view brought into map space (plus a margin for tiles taller than the grid).
        let view_local = self.visible.map(|view| {
            let inverse = place.precise.inverse();
            let min = view.min.as_dvec2();
            let size = view.size.as_dvec2();
            let corners = [
                min,
                min + DVec2::new(size.x, 0.0),
                min + DVec2::new(0.0, size.y),
                min + size,
            ]
            .map(|c| inverse.transform_point3(c.extend(0.0)));
            let lo = corners
                .iter()
                .fold(DVec2::splat(f64::MAX), |a, c| a.min(c.truncate()));
            let hi = corners
                .iter()
                .fold(DVec2::splat(f64::MIN), |a, c| a.max(c.truncate()));
            (lo - DVec2::splat(tile_scale), hi + DVec2::splat(tile_scale))
        });
        // Cells to visit for a layer drawn `offset` away from its grid position, or the whole map.
        let cell_range = |offset: DVec2| {
            let Some((lo, hi)) = view_local else {
                return (0, map.width, 0, map.height);
            };
            // A tile at `p` is drawn at `p + offset`, so look for it at `view - offset`.
            let (lo, hi) = (lo - offset, hi - offset);
            let clamp = clamp_cells;
            let x0 = clamp((lo.x / tw).floor(), map.width);
            let x1 = clamp((hi.x / tw).ceil(), map.width);
            // Rows count from the top in the data, from the bottom in the world.
            let up0 = clamp((lo.y / th).floor(), map.height);
            let up1 = clamp((hi.y / th).ceil(), map.height);
            (x0, x1, map.height - up1, map.height - up0)
        };
        let prefixes: Vec<Arc<[crate::tilemap::TileAnimPrefix]>> = (0..map.tilesets.len())
            .map(|index| loaded.anim_prefix(index))
            .collect();
        let mut added = 0;
        for (index, layer) in map.layers.iter().enumerate() {
            let MapLayer::Tiles(layer) = layer else { continue };
            // Opacity is documented 0..=1; above 1 the blend's `1 - src_alpha` would go negative.
            if !layer.visible || layer.opacity.is_nan() || layer.opacity <= 0.0 {
                continue;
            }
            let opacity = layer.opacity.min(1.0);
            let z = place.z() + index as f32 * component.layer_z_step;
            let Some(color) = drawable_tint([tint[0], tint[1], tint[2], tint[3] * opacity]) else {
                continue;
            };
            let offset = DVec2::new(f64::from(layer.offset[0]), -f64::from(layer.offset[1]));
            if !map.orientation.is_orthogonal() {
                added +=
                    self.push_oriented_layer(loaded, layer, place, z, offset, color, view_local, &prefixes);
                continue;
            }
            let (x0, x1, y0, y1) = cell_range(offset);
            // Visit only cells the layer stores: an unvalidated map may claim far more cells than
            // its data holds (10^10 claimed cells hung extraction). Indices grow with x and then
            // y, so the first index past the data ends the layer.
            let len = layer.data.len() as u64;
            'rows: for y in y0..y1 {
                let row = u64::from(y) * u64::from(map.width);
                if row >= len {
                    break;
                }
                for x in x0..x1 {
                    let index = row + u64::from(x);
                    if index >= len {
                        if x == x0 {
                            break 'rows;
                        }
                        break;
                    }
                    let Some(&raw) = usize::try_from(index).ok().and_then(|i| layer.data.get(i)) else {
                        break 'rows;
                    };
                    let (gid, flags) = split_gid(raw);
                    let Some((set_index, local)) = map.tileset_for(gid) else {
                        continue;
                    };
                    let set = &map.tilesets[set_index];
                    let Some(image) = loaded.images.get(set_index) else {
                        continue;
                    };
                    let prefix = prefixes.get(set_index).map(Arc::as_ref).unwrap_or(&[]);
                    let shown = shown_tile(prefix, set, local, self.time_ms);
                    let Some(src) = set.tile_rect(shown) else { continue };
                    let ([left, bottom], _) = map.tile_world_rect(x, y);
                    let min = DVec2::new(left, bottom) + offset;
                    let size = Vec2::new(set.tile_width as f32, set.tile_height as f32);
                    self.push_quad(&place, z, min, size, image, src, color, flag_bits(flags));
                    added += 1;
                }
            }
        }
        added
    }

    /// One tile layer of an isometric, staggered or hexagonal map (`grid.rs`): the cells in view,
    /// in that orientation's back-to-front order. Returns the tiles added.
    #[allow(clippy::too_many_arguments)] // one layer pass: loaded map, layer, placement, z, offset, tint, view
    fn push_oriented_layer(
        &mut self,
        loaded: &LoadedTileMap,
        layer: &crate::tilemap::TileLayer,
        place: Place,
        z: f32,
        offset: DVec2,
        color: [f32; 4],
        view_local: Option<(DVec2, DVec2)>,
        prefixes: &[Arc<[crate::tilemap::TileAnimPrefix]>],
    ) -> usize {
        let map = &loaded.map;
        let grid = map.grid_size();
        let map_h = map.orientation.pixel_size(grid)[1];
        // World (+Y up) to map pixels (+Y down); a tile at `p` draws at `p + offset`.
        let (x0, x1, y0, y1) = match view_local {
            Some((lo, hi)) => {
                let (lo, hi) = (lo - offset, hi - offset);
                map.orientation
                    .visible_cells(grid, [lo.x, map_h - hi.y], [hi.x, map_h - lo.y])
            }
            None => (0, map.width, 0, map.height),
        };
        // Only rows the layer stores (an unvalidated map may claim far more cells than it holds).
        let stored_rows = (layer.data.len() as u64).div_ceil(u64::from(map.width.max(1)));
        let y1 = y1.min(u32::try_from(stored_rows).unwrap_or(u32::MAX));
        let mut added = 0;
        map.orientation.draw_order(grid, x0, x1, y0, y1, |x, y| {
            let index = u64::from(y) * u64::from(map.width) + u64::from(x);
            let Some(&raw) = usize::try_from(index).ok().and_then(|i| layer.data.get(i)) else {
                return true;
            };
            let (gid, flags) = split_gid(raw);
            let Some((set_index, local)) = map.tileset_for(gid) else {
                return true;
            };
            let set = &map.tilesets[set_index];
            let Some(image) = loaded.images.get(set_index) else {
                return true;
            };
            let prefix = prefixes.get(set_index).map(Arc::as_ref).unwrap_or(&[]);
            let shown = shown_tile(prefix, set, local, self.time_ms);
            let Some(src) = set.tile_rect(shown) else {
                return true;
            };
            let ([left, bottom], _) = map.tile_world_rect(x, y);
            let min = DVec2::new(left, bottom) + offset;
            let size = Vec2::new(set.tile_width as f32, set.tile_height as f32);
            self.push_quad(&place, z, min, size, image, src, color, flag_bits(flags));
            added += 1;
            true
        });
        added
    }

    /// Sorts back to front (stable) and splits into texture batches, writing into `out`.
    ///
    /// `out`'s instance buffer is cleared and reused. It grows only when `items` does not fit,
    /// which extraction uses to keep one buffer per in-flight frame.
    pub fn prepare_into(&mut self, out: &mut PreparedFrame) {
        self.items
            .sort_by(|a, b| a.z.total_cmp(&b.z).then(a.seq.cmp(&b.seq)));
        out.instances.clear();
        out.batches.clear();
        if out.instances.capacity() < self.items.len() {
            // `reserve` is relative to len. `clear` left len at 0, so ask for the whole frame.
            out.instances.reserve(self.items.len());
        }
        for item in &self.items {
            let Some(image) = self.images.get(item.image as usize) else {
                continue;
            };
            let index = out.instances.len() as u32;
            match out.batches.last_mut() {
                Some((previous, range)) if previous.key == image.key => range.end = index + 1,
                _ => out.batches.push((Arc::clone(image), index..index + 1)),
            }
            out.instances.push(item.instance);
        }
        self.items.clear();
        self.images.clear();
    }

    /// Sorts back to front (stable) and splits into texture batches.
    ///
    /// The returned frame's [`PreparedFrame::generation`] stays `0`. Extraction assigns the
    /// generation after it reuses a buffer through [`Self::prepare_into`].
    #[must_use]
    pub fn prepare(mut self) -> PreparedFrame {
        let mut out = PreparedFrame::default();
        self.prepare_into(&mut out);
        out
    }
}

/// The local tile id an animated tile shows at `time_ms` (the tile itself when not animated).
///
/// Tile extraction uses the prefix table on the loaded map. This scan remains the reference
/// for a tile that has no prefix (an empty animation, or a tileset edited between the cache
/// check and the lookup).
#[must_use]
pub fn animated_tile(set: &crate::tilemap::TilesetDef, local: u32, time_ms: u64) -> u32 {
    let Some(def) = set.tile(local) else { return local };
    let total: u64 = def.animation.iter().map(|f| u64::from(f.duration_ms)).sum();
    if total == 0 {
        return local;
    }
    let mut t = time_ms % total;
    for frame in &def.animation {
        note_anim_duration_add();
        if t < u64::from(frame.duration_ms) {
            return frame.tile;
        }
        t -= u64::from(frame.duration_ms);
    }
    local
}

/// The tile `prefixes` shows at `time_ms`, or the scan in [`animated_tile`] when `local` has
/// no prefix.
fn shown_tile(
    prefixes: &[crate::tilemap::TileAnimPrefix],
    set: &crate::tilemap::TilesetDef,
    local: u32,
    time_ms: u64,
) -> u32 {
    if let Ok(index) = prefixes.binary_search_by_key(&local, |prefix| prefix.id) {
        return prefixes[index].at(time_ms).unwrap_or(local);
    }
    animated_tile(set, local, time_ms)
}

fn flag_bits(flags: TileFlags) -> u32 {
    let mut bits = 0;
    if flags.flip_x {
        bits |= FLAG_FLIP_X;
    }
    if flags.flip_y {
        bits |= FLAG_FLIP_Y;
    }
    if flags.diagonal {
        bits |= FLAG_DIAGONAL;
    }
    bits
}

/// Clamps a double-precision cell coordinate to `0..=n`, keeping legal wide-map indices exact.
fn clamp_cells(v: f64, n: u32) -> u32 {
    (v.clamp(0.0, f64::from(n)) as u32).min(n)
}

/// A tint that can contribute to a straight-alpha blend without a non-finite factor.
fn drawable_tint(mut tint: [f32; 4]) -> Option<[f32; 4]> {
    if !tint[3].is_finite() || tint[3] <= 0.0 {
        return None;
    }
    tint[3] = tint[3].min(1.0);
    Some(tint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_clamp_never_exceeds_the_count_above_2_pow_24() {
        // 16_777_219 is not representable as f32 and rounds up to 16_777_220.
        assert_eq!(clamp_cells(f64::MAX, 16_777_219), 16_777_219);
        assert_eq!(clamp_cells(1e30, 7), 7);
        assert_eq!(clamp_cells(-5.0, 7), 0);
        assert_eq!(clamp_cells(3.0, 7), 3);
    }
    use crate::atlas::{AtlasSprite, RgbaImage, SpriteAtlas};
    use crate::tilemap::{
        CookedTileMap, FLIP_X_BIT, ObjectLayer, Orientation, TileDef, TileFrame, TileLayer, TileMapAsset,
        TilesetDef,
    };
    use regia_core::math::Quat;

    fn atlas() -> LoadedAtlas {
        let page = RgbaImage {
            width: 64,
            height: 32,
            rgba8: vec![255; 64 * 32 * 4],
        };
        let mut atlas = SpriteAtlas::single("full", page);
        atlas.sprites.push(AtlasSprite {
            name: "panel".into(),
            page: 0,
            rect: [0, 0, 12, 12],
            source_size: [12, 12],
            trim_offset: [0, 0],
            pivot: [0.0, 0.0],
            nine_slice: Some(NineSlice {
                left: 4,
                top: 4,
                right: 4,
                bottom: 4,
            }),
        });
        atlas.sprites.push(AtlasSprite {
            name: "trimmed".into(),
            page: 0,
            rect: [16, 8, 4, 2],
            source_size: [10, 10],
            trim_offset: [3, 1],
            pivot: [0.5, 0.0],
            nine_slice: None,
        });
        atlas.sprites.sort_by(|a, b| a.name.cmp(&b.name));
        LoadedAtlas::new(atlas)
    }

    #[test]
    fn clamp_cells_never_exceeds_the_count_on_huge_maps() {
        // 16_777_219 is not representable in f32 (it rounds up to ...220).
        let n = 16_777_219u32;
        assert_eq!(clamp_cells(1.0e9, n), n);
        assert_eq!(clamp_cells(f64::from(n as f32), n), n);
        assert_eq!(clamp_cells(-5.0, n), 0);
        assert_eq!(clamp_cells(f64::NAN, n), 0);
        assert_eq!(n - clamp_cells(f64::MAX, n), 0, "row window stays non-negative");
    }

    #[test]
    fn trimmed_sprite_lands_inside_its_source_box() {
        let atlas = atlas();
        let mut frame = Frame2d::new();
        let sprite = Sprite::named("a", "trimmed");
        let at = Affine3A::from_translation(Vec3::new(100.0, 50.0, 0.0));
        assert!(frame.push_sprite(&atlas, &sprite, &at, [1.0; 4]));
        let prepared = frame.prepare();
        let q = prepared.instances[0];
        // Source box 10×10 with pivot (0.5, 0): from (95, 50). Trim (3, 1) top-left, 4×2:
        // x = 95 + 3, y = 50 + (10 - (1 + 2)).
        assert_eq!(q.origin, [98.0, 57.0]);
        assert_eq!(q.axis_x, [4.0, 0.0]);
        assert_eq!(q.axis_y, [0.0, 2.0]);
        assert_eq!(q.uv_min, [16.0 / 64.0, 8.0 / 32.0]);
        assert_eq!(q.uv_max, [20.0 / 64.0, 10.0 / 32.0]);
        assert!(!frame_has_unknown(&atlas));
    }

    fn frame_has_unknown(atlas: &LoadedAtlas) -> bool {
        Frame2d::new().push_sprite(
            atlas,
            &Sprite::named("a", "missing"),
            &Affine3A::IDENTITY,
            [1.0; 4],
        )
    }

    #[test]
    fn rotation_and_scale_reach_the_edge_vectors() {
        let atlas = atlas();
        let mut frame = Frame2d::new();
        let sprite = Sprite::rect("a", [0, 0, 8, 4]);
        let at = Affine3A::from_scale_rotation_translation(
            Vec3::new(2.0, 2.0, 1.0),
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            Vec3::ZERO,
        );
        frame.push_sprite(&atlas, &sprite, &at, [1.0; 4]);
        let q = frame.prepare().instances[0];
        let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4;
        assert!(close(q.axis_x, [0.0, 16.0]), "{:?}", q.axis_x);
        assert!(close(q.axis_y, [-8.0, 0.0]), "{:?}", q.axis_y);
    }

    #[test]
    fn nine_slice_keeps_corners_and_stretches_the_rest() {
        let atlas = atlas();
        let mut frame = Frame2d::new();
        let mut sprite = Sprite::named("a", "panel");
        sprite.size = Some([40.0, 20.0]);
        frame.push_sprite(&atlas, &sprite, &Affine3A::IDENTITY, [1.0; 4]);
        let quads = frame.prepare().instances;
        assert_eq!(quads.len(), 9);
        // Bottom-left corner quad: 4×4 at the origin, sampling the bottom-left 4×4 source texels.
        let bl = quads.iter().find(|q| q.origin == [0.0, 0.0]).expect("corner");
        assert_eq!((bl.axis_x, bl.axis_y), ([4.0, 0.0], [0.0, 4.0]));
        assert_eq!(bl.uv_min, [0.0, 8.0 / 32.0]);
        // Centre: 32×12 from (4, 4), sampling the middle 4×4.
        let c = quads.iter().find(|q| q.origin == [4.0, 4.0]).expect("centre");
        assert_eq!((c.axis_x, c.axis_y), ([32.0, 0.0], [0.0, 12.0]));
        assert_eq!(c.uv_min, [4.0 / 64.0, 4.0 / 32.0]);
        let covered: f32 = quads.iter().map(|q| q.axis_x[0] * q.axis_y[1]).sum();
        assert!((covered - 800.0).abs() < 1e-3);
    }

    #[test]
    fn sorting_is_back_to_front_and_batches_split_on_texture() {
        let a = atlas();
        let b = atlas();
        let mut frame = Frame2d::new();
        let s = Sprite::rect("x", [0, 0, 1, 1]);
        let at = |z: f32| Affine3A::from_translation(Vec3::new(0.0, 0.0, z));
        frame.push_sprite(&a, &s, &at(2.0), [1.0; 4]);
        frame.push_sprite(&b, &s, &at(1.0), [0.5; 4]);
        frame.push_sprite(&a, &s, &at(1.0), [0.25; 4]);
        frame.push_sprite(&a, &s, &at(3.0), [0.75; 4]);
        let prepared = frame.prepare();
        let tints: Vec<f32> = prepared.instances.iter().map(|q| q.color[0]).collect();
        assert_eq!(tints, vec![0.5, 0.25, 1.0, 0.75]);
        let ranges: Vec<Range<u32>> = prepared.batches.iter().map(|(_, r)| r.clone()).collect();
        assert_eq!(ranges, vec![0..1, 1..4]);
    }

    fn tilemap() -> LoadedTileMap {
        let set = TilesetDef {
            name: "t".into(),
            first_gid: 1,
            image: "t.png".into(),
            image_width: 16,
            image_height: 16,
            tile_width: 8,
            tile_height: 8,
            columns: 2,
            tile_count: 4,
            spacing: 0,
            margin: 0,
            tiles: vec![TileDef {
                id: 0,
                animation: vec![
                    TileFrame {
                        tile: 0,
                        duration_ms: 100,
                    },
                    TileFrame {
                        tile: 3,
                        duration_ms: 50,
                    },
                ],
                ..TileDef::default()
            }],
        };
        let map = TileMapAsset {
            version: 1,
            orientation: Orientation::Orthogonal,
            width: 4,
            height: 3,
            tile_width: 8,
            tile_height: 8,
            background: None,
            tilesets: vec![set],
            layers: vec![MapLayer::Tiles(TileLayer {
                name: "l".into(),
                visible: true,
                opacity: 0.5,
                offset: [0.0, 0.0],
                data: vec![1, 0, 0, 2 | FLIP_X_BIT, 0, 0, 0, 0, 3, 0, 0, 4],
                properties: Default::default(),
            })],
            properties: Default::default(),
        };
        LoadedTileMap::new(CookedTileMap {
            version: 1,
            map,
            images: vec![RgbaImage {
                width: 16,
                height: 16,
                rgba8: vec![0; 16 * 16 * 4],
            }],
        })
    }

    #[test]
    fn tiles_place_bottom_up_with_flags_opacity_and_animation() {
        let map = tilemap();
        let mut frame = Frame2d::new();
        frame.time_ms = 120;
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            4
        );
        let q = frame.prepare().instances;
        // Row 0 (top) of a 3-row map sits at y = 16.
        assert_eq!(q[0].origin, [0.0, 16.0]);
        // The animated tile is on its second frame (tile 3 → x 8, y 8 in the image).
        assert_eq!(q[0].uv_min, [0.5, 0.5]);
        assert_eq!(q[0].color[3], 0.5);
        assert_eq!(q[1].origin, [24.0, 16.0]);
        assert_eq!(q[1].flags, FLAG_FLIP_X);
        assert_eq!(q[2].origin, [0.0, 0.0]);
        assert_eq!(q[3].origin, [24.0, 0.0]);
    }

    fn wide_tilemap() -> LoadedTileMap {
        let mut map = tilemap();
        map.map.width = 20_000_000;
        map.map.height = 1;
        map.map.tile_width = 16;
        map.map.tile_height = 16;
        let MapLayer::Tiles(layer) = &mut map.map.layers[0] else {
            panic!("tile layer");
        };
        layer.data = vec![0; map.map.width as usize];
        layer.data[16_777_216..=16_777_218].copy_from_slice(&[1, 2, 3]);
        assert!(map.map.validate().is_ok());
        map
    }

    #[test]
    fn rebased_wide_map_keeps_adjacent_tiles_distinct_and_visible() {
        let map = wide_tilemap();
        // The map's far columns are moved near the world origin before converting to GPU f32.
        let transform = Affine3A::from_translation(Vec3::new(-268_435_456.0, 0.0, 0.0));
        let mut frame = Frame2d::new();
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &transform, [1.0; 4]),
            3
        );
        let instances = frame.prepare().instances;
        assert_eq!(
            instances.iter().map(|q| q.origin[0]).collect::<Vec<_>>(),
            vec![0.0, 16.0, 32.0]
        );

        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(16.0, 0.0, 1.0, 8.0));
        assert!(frame.push_tilemap(&map, &TileMap::new("m"), &transform, [1.0; 4]) > 0);
        assert!(frame.prepare().instances.iter().any(|q| q.origin[0] == 16.0));
    }

    #[test]
    fn rebased_wide_map_culling_keeps_views_smaller_than_a_local_f32_step() {
        let map = wide_tilemap();
        let transform = Affine3A::from_translation(Vec3::new(-268_435_456.0, 0.0, 0.0));
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(1.0, 0.0, 1.0, 8.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &transform, [1.0; 4]),
            1
        );
        assert_eq!(frame.prepare().instances[0].origin, [0.0, 0.0]);
    }

    #[test]
    fn rebased_wide_isometric_map_keeps_half_tile_steps() {
        let mut map = wide_tilemap();
        map.map.orientation = Orientation::Isometric;
        assert!(map.map.validate().is_ok());
        let transform = Affine3A::from_translation(Vec3::new(-134_217_728.0, -25_782_256.0, 0.0));
        let mut frame = Frame2d::new();
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &transform, [1.0; 4]),
            3
        );
        assert_eq!(
            frame
                .prepare()
                .instances
                .iter()
                .map(|q| q.origin)
                .collect::<Vec<_>>(),
            vec![[0.0, 8.0], [8.0, 0.0], [16.0, -8.0]]
        );
    }

    #[test]
    fn tiles_outside_the_view_are_skipped() {
        let map = tilemap();
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(-100.0, -100.0, 90.0, 90.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            0
        );
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(-30.0, -30.0, 32.0, 32.0));
        // Only the bottom-left tile (and anything within one tile of the view) is visited.
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            1
        );
    }

    #[test]
    fn layer_offset_moves_the_visited_cells_with_the_tiles() {
        let mut map = tilemap();
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.offset = [64.0, 0.0];
        }
        // The tiles are drawn at x 64..96; a view over that range must still find them.
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(70.0, 0.0, 20.0, 24.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            4
        );
        // A view over the unshifted position sees nothing.
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(0.0, 0.0, 20.0, 24.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            0
        );
    }

    #[test]
    fn a_short_layer_is_skipped_instead_of_panicking() {
        let mut map = tilemap();
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.data.pop();
        }
        let mut frame = Frame2d::new();
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            3
        );
    }

    /// An unvalidated map (a hand-built `CookedTileMap`, never `validate`d) claiming 2^60 cells
    /// with one cell of data: extraction before a window has drawn has no view rect and visited
    /// every claimed cell, hanging the frame (concurrency audit 11-03 M1). Only cells the layer
    /// holds may be visited.
    #[test]
    fn a_huge_unvalidated_map_visits_only_the_cells_it_has() {
        let mut map = tilemap();
        map.map.width = 1 << 30;
        map.map.height = 1 << 30;
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.data = vec![1];
        }
        let mut frame = Frame2d::new();
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            1
        );
        // A view over the whole (claimed) map is bounded the same way.
        let mut frame = Frame2d::new();
        // 2^30 rows of 8 px: the top row (the stored cell) sits near y = 8.6e9.
        frame.visible = Some(Rect2::new(0.0, 0.0, 1.0e12, 1.0e12));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            1,
            "the view covers the whole map; only the one stored cell is drawn"
        );
    }

    #[test]
    fn nine_slice_shrinks_borders_that_do_not_fit() {
        let atlas = atlas();
        let mut sprite = Sprite::named("a", "panel");
        sprite.size = Some([6.0, 5.0]);
        let mut frame = Frame2d::new();
        assert!(frame.push_sprite(&atlas, &sprite, &Affine3A::IDENTITY, [1.0; 4]));
        let quads = frame.prepare().instances;
        // Borders are 4+4. On a 6×5 quad they scale to 3 and 2.5 and the centre span is empty.
        assert_eq!(quads.len(), 4);
        assert_eq!(quads[0].origin, [0.0, 2.5]);
        assert_eq!((quads[0].axis_x, quads[0].axis_y), ([3.0, 0.0], [0.0, 2.5]));
        assert_eq!(quads[0].uv_min, [0.0, 0.0]);
        assert_eq!(quads[0].uv_max, [4.0 / 64.0, 4.0 / 32.0]);
        assert_eq!(quads[1].origin, [3.0, 2.5]);
        assert_eq!(quads[1].uv_min, [8.0 / 64.0, 0.0]);
        assert_eq!(quads[1].uv_max, [12.0 / 64.0, 4.0 / 32.0]);
        assert_eq!(quads[2].origin, [0.0, 0.0]);
        assert_eq!(quads[2].uv_min, [0.0, 8.0 / 32.0]);
        assert_eq!(quads[2].uv_max, [4.0 / 64.0, 12.0 / 32.0]);
        assert_eq!(quads[3].origin, [3.0, 0.0]);
        assert_eq!(quads[3].uv_min, [8.0 / 64.0, 8.0 / 32.0]);
        assert_eq!(quads[3].uv_max, [12.0 / 64.0, 12.0 / 32.0]);
        let covered: f32 = quads.iter().map(|q| q.axis_x[0] * q.axis_y[1]).sum();
        assert_eq!(covered, 30.0);

        sprite.size = Some([20.0, 5.0]);
        let mut frame = Frame2d::new();
        assert!(frame.push_sprite(&atlas, &sprite, &Affine3A::IDENTITY, [1.0; 4]));
        let quads = frame.prepare().instances;
        assert_eq!(quads.len(), 6);
        let centre = quads.iter().find(|q| q.origin == [4.0, 2.5]).expect("top centre");
        assert_eq!((centre.axis_x, centre.axis_y), ([12.0, 0.0], [0.0, 2.5]));
        assert_eq!(centre.uv_min, [4.0 / 64.0, 0.0]);
        assert_eq!(centre.uv_max, [8.0 / 64.0, 4.0 / 32.0]);
        let covered: f32 = quads.iter().map(|q| q.axis_x[0] * q.axis_y[1]).sum();
        assert_eq!(covered, 100.0);
    }

    #[test]
    fn anchor_and_flip_place_the_trimmed_quad_and_a_missing_page_adds_nothing() {
        let atlas = atlas();
        let mut sprite = Sprite::named("a", "trimmed");
        sprite.anchor = Some([0.0, 1.0]);
        sprite.flip_x = true;
        sprite.flip_y = true;
        let mut frame = Frame2d::new();
        assert!(frame.push_sprite(&atlas, &sprite, &Affine3A::IDENTITY, [0.25, 0.5, 0.75, 1.0]));
        assert_eq!(frame.len(), 1);
        let prepared = frame.prepare();
        let q = &prepared.instances[0];
        assert_eq!(q.origin, [3.0, -9.0]);
        assert_eq!(q.axis_x, [4.0, 0.0]);
        assert_eq!(q.axis_y, [0.0, 2.0]);
        assert_eq!(q.uv_min, [16.0 / 64.0, 8.0 / 32.0]);
        assert_eq!(q.uv_max, [20.0 / 64.0, 10.0 / 32.0]);
        assert_eq!(q.flags, FLAG_FLIP_X | FLAG_FLIP_Y);
        assert_eq!(q.color, [0.25, 0.5, 0.75, 1.0]);

        let mut bare = atlas;
        bare.pages.clear();
        let mut frame = Frame2d::new();
        assert!(!frame.push_sprite(
            &bare,
            &Sprite::named("a", "trimmed"),
            &Affine3A::IDENTITY,
            [1.0; 4]
        ));
        assert!(!frame.push_sprite(
            &bare,
            &Sprite::rect("a", [0, 0, 2, 2]),
            &Affine3A::IDENTITY,
            [1.0; 4]
        ));
        assert!(frame.is_empty());
        assert_eq!(frame.len(), 0);
        let prepared = frame.prepare();
        assert!(prepared.instances.is_empty());
        assert!(prepared.batches.is_empty());
    }

    fn relocation_atlas(page_index: u32, page_size: [u32; 2], rects: [[u32; 4]; 3]) -> LoadedAtlas {
        use crate::atlas::RgbaImage;

        let mut pages = vec![
            RgbaImage {
                width: page_size[0],
                height: page_size[1],
                rgba8: vec![
                    if page_index == 0 { 0xA5 } else { 0x3C };
                    (page_size[0] * page_size[1] * 4) as usize
                ],
            };
            page_index as usize + 1
        ];
        let image = &mut pages[page_index as usize];
        for rect in rects {
            for y in 0..rect[3] {
                for x in 0..rect[2] {
                    let index = (((rect[1] + y) * page_size[0] + rect[0] + x) * 4) as usize;
                    image.rgba8[index..index + 4].copy_from_slice(&[
                        (x * 37 + y * 11) as u8,
                        (y * 53 + x * 7) as u8,
                        (x ^ y) as u8,
                        255,
                    ]);
                }
            }
        }
        let mut atlas = SpriteAtlas {
            version: crate::atlas::COOKED_ATLAS_VERSION,
            pages,
            sprites: vec![
                AtlasSprite {
                    name: "nine".into(),
                    page: page_index,
                    rect: rects[2],
                    source_size: [7, 6],
                    trim_offset: [0, 0],
                    pivot: [0.27, 0.68],
                    nine_slice: Some(NineSlice {
                        left: 2,
                        top: 1,
                        right: 1,
                        bottom: 2,
                    }),
                },
                AtlasSprite {
                    name: "one".into(),
                    page: page_index,
                    rect: rects[1],
                    source_size: [7, 6],
                    trim_offset: [4, 1],
                    pivot: [0.27, 0.68],
                    nine_slice: None,
                },
                AtlasSprite {
                    name: "trim".into(),
                    page: page_index,
                    rect: rects[0],
                    source_size: [11, 9],
                    trim_offset: [2, 3],
                    pivot: [0.27, 0.68],
                    nine_slice: None,
                },
            ],
        };
        atlas.sprites.sort_by(|a, b| a.name.cmp(&b.name));
        atlas.validate().expect("fixture atlas validates");
        LoadedAtlas::new(atlas)
    }

    type RelocationGeometry = ([f32; 2], [f32; 2], [f32; 2], u32);

    fn relocation_geometry(frame: &PreparedFrame) -> Vec<RelocationGeometry> {
        frame
            .instances
            .iter()
            .map(|q| (q.origin, q.axis_x, q.axis_y, q.flags))
            .collect()
    }

    fn source_texels(atlas: &LoadedAtlas, name: &str, frame: &PreparedFrame) -> Vec<[f32; 4]> {
        let sprite = atlas.atlas.sprite(name).expect("fixture sprite");
        let [rx, ry, _, _] = sprite.rect;
        let image = &atlas.pages[sprite.page as usize].image;
        frame
            .instances
            .iter()
            .map(|q| {
                [
                    q.uv_min[0] * image.width as f32 - rx as f32,
                    q.uv_min[1] * image.height as f32 - ry as f32,
                    q.uv_max[0] * image.width as f32 - rx as f32,
                    q.uv_max[1] * image.height as f32 - ry as f32,
                ]
            })
            .collect()
    }

    #[test]
    fn atlas_page_relocation_preserves_sprite_geometry_and_source_texels() {
        let a = relocation_atlas(0, [13, 15], [[2, 1, 3, 2], [7, 3, 1, 2], [1, 6, 7, 6]]);
        let b = relocation_atlas(1, [19, 9], [[15, 4, 3, 2], [3, 1, 1, 2], [6, 1, 7, 6]]);
        let transform = Affine3A::from_translation(Vec3::new(3.0, -2.0, 0.0));

        for (name, size, flips) in [
            (
                "trim",
                None,
                [(false, false), (true, false), (false, true), (true, true)],
            ),
            (
                "one",
                None,
                [(false, false), (true, false), (false, true), (true, true)],
            ),
            (
                "nine",
                Some([17.0, 13.0]),
                [(false, false), (true, false), (false, true), (true, true)],
            ),
        ] {
            for (flip_x, flip_y) in flips {
                let mut sprite_a = Sprite::named("relocated", name);
                sprite_a.size = size;
                sprite_a.flip_x = flip_x;
                sprite_a.flip_y = flip_y;
                let sprite_b = sprite_a.clone();
                let mut frame_a = Frame2d::new();
                let mut frame_b = Frame2d::new();
                assert!(frame_a.push_sprite(&a, &sprite_a, &transform, [1.0; 4]));
                assert!(frame_b.push_sprite(&b, &sprite_b, &transform, [1.0; 4]));
                let prepared_a = frame_a.prepare();
                let prepared_b = frame_b.prepare();
                assert_eq!(
                    relocation_geometry(&prepared_a),
                    relocation_geometry(&prepared_b),
                    "{name} flips {flip_x}/{flip_y}"
                );
                let texels_a = source_texels(&a, name, &prepared_a);
                let texels_b = source_texels(&b, name, &prepared_b);
                for (source_a, source_b) in texels_a.iter().zip(&texels_b) {
                    for (coord_a, coord_b) in source_a.iter().zip(source_b) {
                        assert!(
                            (coord_a - coord_b).abs() < 1.0e-5,
                            "{name} flips {flip_x}/{flip_y}: {source_a:?} != {source_b:?}"
                        );
                    }
                }
                let packed = a.atlas.sprite(name).expect("fixture sprite").rect;
                let expected_source = if name == "nine" {
                    let cols = [(0.0, 2.0), (2.0, 4.0), (6.0, 1.0)];
                    let rows = [(0.0, 1.0), (1.0, 3.0), (4.0, 2.0)];
                    (0..3)
                        .flat_map(|row| {
                            (0..3).map(move |col| {
                                [
                                    cols[col].0,
                                    rows[row].0,
                                    cols[col].0 + cols[col].1,
                                    rows[row].0 + rows[row].1,
                                ]
                            })
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![[0.0, 0.0, packed[2] as f32, packed[3] as f32]]
                };
                assert_eq!(texels_a.len(), expected_source.len());
                for (source, expected) in texels_a.iter().zip(expected_source) {
                    assert!(source[0] >= -1.0e-5 && source[1] >= -1.0e-5);
                    assert!(source[0] <= source[2] + 1.0e-5 && source[1] <= source[3] + 1.0e-5);
                    assert!(
                        source[2] <= packed[2] as f32 + 1.0e-5 && source[3] <= packed[3] as f32 + 1.0e-5,
                        "{name} UV maps outside its packed rectangle: {source:?}"
                    );
                    for (actual, expected) in source.iter().zip(expected) {
                        assert!(
                            (actual - expected).abs() < 1.0e-5,
                            "{name} source texels {source:?}, expected {expected:?}"
                        );
                    }
                }
                assert_eq!(prepared_a.instances.len(), prepared_b.instances.len());
                assert_eq!(prepared_a.instances.len(), if name == "nine" { 9 } else { 1 });
                assert_eq!(
                    prepared_a
                        .batches
                        .iter()
                        .map(|(_, range)| range.clone())
                        .collect::<Vec<_>>(),
                    prepared_b
                        .batches
                        .iter()
                        .map(|(_, range)| range.clone())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn missing_page_does_not_poison_the_next_valid_sprite_batch() {
        let valid = atlas();
        let mut invalid_data = valid.atlas.clone();
        invalid_data.sprites[0].page = 1;
        let invalid = LoadedAtlas::new(invalid_data);
        let mut frame = Frame2d::new();
        assert!(!frame.push_sprite(
            &invalid,
            &Sprite::named("bad", "panel"),
            &Affine3A::IDENTITY,
            [1.0; 4]
        ));
        assert!(frame.push_sprite(
            &valid,
            &Sprite::named("ok", "panel"),
            &Affine3A::IDENTITY,
            [1.0; 4]
        ));
        let prepared = frame.prepare();
        assert_eq!(prepared.instances.len(), 1);
        assert_eq!(prepared.batches.len(), 1);
        assert_eq!(prepared.batches[0].1, 0..1);
    }

    fn animated_set(frames: Vec<TileFrame>) -> TilesetDef {
        TilesetDef {
            name: "t".into(),
            first_gid: 1,
            image: "t.png".into(),
            image_width: 8,
            image_height: 8,
            tile_width: 8,
            tile_height: 8,
            columns: 1,
            tile_count: 4,
            spacing: 0,
            margin: 0,
            tiles: vec![TileDef {
                id: 1,
                animation: frames,
                ..TileDef::default()
            }],
        }
    }

    #[test]
    fn animated_tile_steps_on_duration_boundaries() {
        let set = animated_set(vec![
            TileFrame {
                tile: 7,
                duration_ms: 100,
            },
            TileFrame {
                tile: 8,
                duration_ms: 0,
            },
            TileFrame {
                tile: 9,
                duration_ms: 50,
            },
        ]);
        assert_eq!(animated_tile(&set, 0, 0), 0);
        assert_eq!(animated_tile(&set, 2, 1_000), 2);
        assert_eq!(animated_tile(&set, 1, 0), 7);
        assert_eq!(animated_tile(&set, 1, 99), 7);
        assert_eq!(animated_tile(&set, 1, 100), 9);
        assert_eq!(animated_tile(&set, 1, 149), 9);
        assert_eq!(animated_tile(&set, 1, 150), 7);
        assert_eq!(animated_tile(&set, 1, 250), 9);
        assert_eq!(animated_tile(&set, 1, 300), 7);

        let zero = animated_set(vec![TileFrame {
            tile: 4,
            duration_ms: 0,
        }]);
        assert_eq!(animated_tile(&zero, 1, 999), 1);
        assert_eq!(animated_tile(&animated_set(Vec::new()), 1, 5), 1);
    }

    /// The prefix table and the per-frame scan agree, including a zero-duration frame.
    /// Building the table reads each frame once. A later lookup does not.
    #[test]
    fn prefix_lookup_matches_the_duration_scan() {
        use crate::tilemap::{prefix_frame_reads, reset_prefix_frame_reads};
        let set = animated_set(vec![
            TileFrame {
                tile: 7,
                duration_ms: 100,
            },
            TileFrame {
                tile: 8,
                duration_ms: 0,
            },
            TileFrame {
                tile: 9,
                duration_ms: 50,
            },
        ]);
        reset_prefix_frame_reads();
        let prefixes = set.anim_prefixes();
        assert_eq!(
            prefix_frame_reads(),
            3,
            "three frames are read once while the prefix is built"
        );
        reset_anim_duration_adds();
        for time in [0u64, 99, 100, 149, 150, 250, 300, 10_000] {
            let scanned = animated_tile(&set, 1, time);
            let looked = prefixes
                .iter()
                .find(|prefix| prefix.id == 1)
                .and_then(|prefix| prefix.at(time))
                .unwrap_or(1);
            assert_eq!(looked, scanned, "time {time}");
        }
        // The scan still walks frames. The prefix lookup above did not.
        assert!(anim_duration_adds() > 0);
        reset_prefix_frame_reads();
        let _ = prefixes[0].at(100);
        assert_eq!(prefix_frame_reads(), 0);
    }

    /// Twelve cells of one animated tile used to add a duration per cell per frame (24 reads
    /// for a two-frame animation that always walks both frames). The prefix is built at load.
    #[test]
    fn many_cells_of_one_animation_do_not_rescan_frames() {
        use crate::tilemap::{prefix_frame_reads, reset_prefix_frame_reads};
        reset_prefix_frame_reads();
        let map = {
            let mut map = tilemap();
            if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
                layer.data = vec![1; 12];
            }
            map
        };
        // `LoadedTileMap::new` already built the prefixes (two frames). Filling the layer
        // does not. Reset and extract.
        reset_prefix_frame_reads();
        reset_anim_duration_adds();
        let mut frame = Frame2d::new();
        frame.time_ms = 120;
        let drawn = frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]);
        assert_eq!(drawn, 12);
        assert_eq!(
            anim_duration_adds(),
            0,
            "the old per-cell scan added a duration up to 12 cells * 2 frames = 24 times"
        );
        assert_eq!(
            prefix_frame_reads(),
            0,
            "prefixes are built at load, not once per cell"
        );
        let prepared = frame.prepare();
        assert!(prepared.instances.iter().all(|quad| quad.uv_min == [0.5, 0.5]));
    }

    /// An in-memory duration edit misses the fingerprint, so the next extract uses the new frame.
    #[test]
    fn an_edited_duration_rebuilds_the_prefix() {
        let mut map = tilemap();
        map.map.tilesets[0].tiles[0].animation[0].duration_ms = 200;
        let mut frame = Frame2d::new();
        frame.time_ms = 120;
        assert!(frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]) > 0);
        let quads = frame.prepare().instances;
        assert_eq!(
            quads[0].uv_min,
            [0.0, 0.0],
            "120 ms is inside the edited 200 ms first frame (tile 0), not tile 3"
        );
    }

    /// One tileset image is stored once for the whole frame, not once per quad.
    #[test]
    fn one_tileset_image_is_stored_once_per_frame() {
        let map = tilemap();
        let image = Arc::clone(&map.images[0]);
        let base = Arc::strong_count(&image);
        let mut frame = Frame2d::new();
        let drawn = frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]);
        assert!(drawn > 1, "the map draws more than one quad");
        assert_eq!(
            Arc::strong_count(&image),
            base + 1,
            "the old path cloned the image once per quad ({drawn} extra counts)"
        );
    }

    #[test]
    fn layer_opacity_is_clamped_to_one_and_nan_layers_are_skipped() {
        let mut map = tilemap();
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.opacity = 3.0;
        }
        let mut frame = Frame2d::new();
        let n = frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]);
        assert!(n > 0);
        assert!(frame.prepare().instances.iter().all(|q| q.color[3] == 1.0));
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.opacity = f32::NAN;
        }
        let mut frame = Frame2d::new();
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            0
        );
    }

    #[test]
    fn sprite_tint_alpha_is_clamped_and_invalid_alpha_is_skipped() {
        let atlas = atlas();
        for (name, size) in [("full", None), ("panel", Some([40.0, 20.0]))] {
            for (alpha, expected) in [
                (2.0, Some(1.0)),
                (0.25, Some(0.25)),
                (0.0, None),
                (-1.0, None),
                (f32::NAN, None),
                (f32::INFINITY, None),
                (f32::NEG_INFINITY, None),
            ] {
                let mut sprite = Sprite::named("a", name);
                sprite.size = size;
                sprite.color = [0.25, 0.5, 0.75, alpha];
                let mut frame = Frame2d::new();
                assert!(frame.push_sprite(&atlas, &sprite, &Affine3A::IDENTITY, sprite.color));
                let quads = frame.prepare().instances;
                if let Some(expected) = expected {
                    assert!(!quads.is_empty());
                    assert!(
                        quads.iter().all(|q| q.color == [0.25, 0.5, 0.75, expected]),
                        "{name}, alpha {alpha}"
                    );
                } else {
                    assert!(quads.is_empty(), "{name}, alpha {alpha}");
                }
            }
        }
    }

    #[test]
    fn tilemap_tint_alpha_is_clamped_after_layer_opacity_and_invalid_alpha_is_skipped() {
        for orientation in [Orientation::Orthogonal, Orientation::Isometric] {
            for (alpha, opacity, expected) in [
                (2.0, 1.0, Some(1.0)),
                (2.0, 0.25, Some(0.5)),
                (0.5, 0.5, Some(0.25)),
                (0.0, 1.0, None),
                (-1.0, 1.0, None),
                (f32::NAN, 1.0, None),
                (f32::INFINITY, 1.0, None),
                (f32::NEG_INFINITY, 1.0, None),
            ] {
                let mut map = oriented_map(orientation);
                if let MapLayer::Tiles(layer) = &mut map.map.layers[0] {
                    layer.opacity = opacity;
                }
                let mut component = TileMap::new("m");
                component.color = [0.25, 0.5, 0.75, alpha];
                let mut frame = Frame2d::new();
                let added = frame.push_tilemap(&map, &component, &Affine3A::IDENTITY, component.color);
                let quads = frame.prepare().instances;
                assert_eq!(added, quads.len());
                if let Some(expected) = expected {
                    assert_eq!(added, 12);
                    assert!(
                        quads.iter().all(|q| q.color == [0.25, 0.5, 0.75, expected]),
                        "{orientation:?}, alpha {alpha}, opacity {opacity}"
                    );
                } else {
                    assert_eq!(added, 0, "{orientation:?}, alpha {alpha}");
                }
            }
        }
    }

    #[test]
    fn a_positive_tiled_y_offset_moves_tiles_down_and_hidden_layers_are_skipped() {
        let mut map = tilemap();
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.offset = [0.0, 16.0];
        }
        map.map.layers.push(MapLayer::Tiles(TileLayer {
            name: "hidden".into(),
            visible: false,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![1; 12],
            properties: Default::default(),
        }));
        map.map.layers.push(MapLayer::Tiles(TileLayer {
            name: "clear".into(),
            visible: true,
            opacity: 0.0,
            offset: [0.0, 0.0],
            data: vec![1; 12],
            properties: Default::default(),
        }));
        map.map.layers.push(MapLayer::Objects(ObjectLayer {
            name: "spawns".into(),
            visible: true,
            objects: Vec::new(),
            properties: Default::default(),
        }));

        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(0.0, 0.0, 32.0, 8.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            2
        );
        let quads = frame.prepare().instances;
        assert_eq!(quads[0].origin, [0.0, 0.0]);
        assert_eq!(quads[0].flags, 0);
        assert_eq!(quads[1].origin, [24.0, 0.0]);
        assert_eq!(quads[1].flags, FLAG_FLIP_X);

        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(0.0, 16.0, 32.0, 8.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            0
        );
    }

    /// A 4x3 map of 8x8 tiles (tile 1 everywhere) in `orientation`.
    fn oriented_map(orientation: Orientation) -> LoadedTileMap {
        let mut map = tilemap();
        map.map.orientation = orientation;
        if let Some(MapLayer::Tiles(layer)) = map.map.layers.first_mut() {
            layer.data = vec![1; 12];
            layer.opacity = 1.0;
        }
        map
    }

    #[test]
    fn oriented_maps_place_tiles_at_their_cell_boxes_back_to_front() {
        use crate::tilemap::{StaggerAxis, StaggerIndex};
        let orientations = [
            Orientation::Isometric,
            Orientation::Staggered {
                axis: StaggerAxis::Y,
                index: StaggerIndex::Odd,
            },
            Orientation::Hexagonal {
                axis: StaggerAxis::X,
                index: StaggerIndex::Even,
                side_length: 4,
            },
        ];
        for orientation in orientations {
            let map = oriented_map(orientation);
            let mut frame = Frame2d::new();
            assert_eq!(
                frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
                12,
                "{orientation:?}"
            );
            let quads = frame.prepare().instances;
            // Insertion order is the draw order; every quad sits at its cell box's bottom-left.
            let mut order = Vec::new();
            orientation.draw_order(map.map.grid_size(), 0, 4, 0, 3, |x, y| {
                order.push((x, y));
                true
            });
            assert_eq!(quads.len(), order.len());
            for (quad, (x, y)) in quads.iter().zip(order) {
                let ([left, bottom], _) = map.map.tile_world_rect(x, y);
                assert_eq!(
                    quad.origin,
                    [left as f32, bottom as f32],
                    "{orientation:?} cell ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn isometric_tiles_cull_to_the_view_and_keep_the_cells_they_overlap() {
        let map = oriented_map(Orientation::Isometric);
        // The whole map is 28 x 28 px; a view over it sees every tile, a view far away none.
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(0.0, 0.0, 28.0, 28.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            12
        );
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(500.0, 500.0, 10.0, 10.0));
        assert_eq!(
            frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]),
            0
        );
        // A small view: the culled count never drops a tile whose box intersects it.
        let mut frame = Frame2d::new();
        frame.visible = Some(Rect2::new(10.0, 10.0, 3.0, 3.0));
        let n = frame.push_tilemap(&map, &TileMap::new("m"), &Affine3A::IDENTITY, [1.0; 4]);
        let mut expected = 0;
        for y in 0..3 {
            for x in 0..4 {
                let ([l, b], [r, t]) = map.map.tile_world_rect(x, y);
                if l < 13.0 && r > 10.0 && b < 13.0 && t > 10.0 {
                    expected += 1;
                }
            }
        }
        assert!(
            expected > 0 && n >= expected,
            "drew {n}, at least {expected} intersect"
        );
    }
}
