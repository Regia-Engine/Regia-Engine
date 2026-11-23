// SPDX-License-Identifier: MPL-2.0
//! Forward decals: projected decals blended into the surface before it is lit (ADR-3255).
//!
//! The projected decal layer (`regia_vfx`'s decal pass) composites after the forward pass, so a
//! decal's normal and roughness never reach the lighting: it relights the finished colour with an
//! N·L ratio and a Phong-shaped lobe. A forward decal instead changes the surface inputs of the
//! forward shader (`lab_forward.wgsl`, `shade_forward`): after the material is sampled and before
//! any light is evaluated, the pixel gathers the decals of its screen tile and blends their
//! albedo, normal, roughness and metallic into the surface, which is then lit once by the same
//! BRDF as everything else. The tracer (`regia_pt::PtDecal`) applies the same weights to the same
//! inputs.
//!
//! **Tiles.** [`pack_forward_decals`] bins the decals into screen tiles on the CPU (the screen
//! clustering of Olsson, Billeter and Assarsson, "Clustered Deferred and Forward Shading",
//! HPG 2012, with one 64-bit membership mask per tile instead of an index list, as in the public
//! talk "Improved Culling for Tiled and Clustered Rendering", Drobot, SIGGRAPH 2017). The mask's
//! bit order is the draw order, so a higher priority blends later. A view the tiles were not
//! built for (another view of the same frame, a probe face) tests every decal instead
//! ([`ForwardDecalHeader`]).
//!
//! **Data.** One storage buffer (group 0 binding [`DATA_BINDING`], no texture or sampler binding):
//! a 64-byte header, then [`FORWARD_DECAL_MAX`] decals of [`DECAL_BYTES`] each, then the tile masks
//! ([`FORWARD_DECAL_MAX_TILES`] × 8 bytes), then the atlas ([`ATLAS_OFFSET`]): packed RGBA8 texels,
//! [`FORWARD_DECAL_ATLAS_PAGES`] pages per decal layer, filtered bilinearly in the shader. Pages:
//! albedo + coverage (sRGB-encoded bytes, decoded in the shader), a tangent-space normal
//! (`xy * 0.5 + 0.5`), and roughness/metallic multipliers (`R/G × 2`, so 128 is ×1).
//!
//! **Switch.** The path is behind [`RenderProfile::forward_decals`](crate::RenderProfile) and the
//! process-wide [`process_forward_decals`] (read once, so the view layout and every forward
//! module agree). With it off the forward modules carry no decal code and no binding, so default
//! frames are unchanged. Android and devices without room for the binding keep the post path.
//!
//! **Fetches per pixel (desktop).** One 8-byte tile-mask read; then per decal in the mask a box
//! test from 4 × 16 bytes; per decal that covers the pixel 5 more 16-byte reads, the albedo page
//! when the decal has an atlas layer, the normal page when it writes the normal from the atlas,
//! and the roughness page when it writes roughness from the atlas (each page sample is four
//! 4-byte reads, so at most 12).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use regia_core::math::{Mat4, Vec3, Vec4};

/// Most decals one frame shades in the forward pass (one 64-bit tile mask).
pub const FORWARD_DECAL_MAX: usize = 64;
/// Smallest tile edge in pixels. Larger views grow the tile so the grid fits
/// [`FORWARD_DECAL_MAX_TILES`].
pub const FORWARD_DECAL_TILE_PX: u32 = 32;
/// Most tiles of one view's grid (the tile-mask region of the buffer).
pub const FORWARD_DECAL_MAX_TILES: usize = 8192;
/// Texels on one side of an atlas page.
pub const FORWARD_DECAL_ATLAS_SIZE: u32 = 64;
/// Pages per decal atlas layer: albedo + coverage, normal, roughness/metallic.
pub const FORWARD_DECAL_ATLAS_PAGES: u32 = 3;
/// Decal atlas layers the forward path holds.
pub const FORWARD_DECAL_ATLAS_LAYERS: u32 = 8;
/// Bytes of the buffer header.
pub const HEADER_BYTES: usize = 64;
/// Bytes of one packed decal (nine `vec4`).
pub const DECAL_BYTES: usize = 144;
/// Bytes of the whole data buffer.
pub const BUFFER_BYTES: usize = HEADER_BYTES + FORWARD_DECAL_MAX * DECAL_BYTES + FORWARD_DECAL_MAX_TILES * 8;
/// Byte offset of the atlas region (right after the tile masks).
pub const ATLAS_OFFSET: usize = BUFFER_BYTES;
/// Bytes of the atlas region: every page of every layer, one packed RGBA8 `u32` per texel.
pub const ATLAS_BYTES: usize = (FORWARD_DECAL_ATLAS_LAYERS
    * FORWARD_DECAL_ATLAS_PAGES
    * FORWARD_DECAL_ATLAS_SIZE
    * FORWARD_DECAL_ATLAS_SIZE
    * 4) as usize;
/// Bytes of the whole storage buffer (data, then the atlas).
pub const STORAGE_BYTES: usize = ATLAS_OFFSET + ATLAS_BYTES;
/// Group 0 binding of the storage buffer.
pub const DATA_BINDING: u32 = 39;
/// The forward shader's device-level define for the forward-decal bindings.
pub const FORWARD_DEF: &str = "FRAME_FORWARD_DECALS";
/// Storage buffers per fragment stage a device needs for the data buffer (the view group's
/// eight with it, the object group's two and a material record, with room).
pub const FORWARD_MIN_STORAGE_BUFFERS: u32 = 12;

/// Channel bit: albedo (modulate or replace). Same bits as `regia_vfx::CHANNEL_*`.
pub const CHANNEL_ALBEDO: u32 = 1;
/// Channel bit: normal.
pub const CHANNEL_NORMAL: u32 = 1 << 1;
/// Channel bit: roughness.
pub const CHANNEL_ROUGHNESS: u32 = 1 << 2;
/// Channel bit: emissive (never shaded forward; the post path adds it).
pub const CHANNEL_EMISSIVE: u32 = 1 << 3;
/// Channel bit: metallic.
pub const CHANNEL_METALLIC: u32 = 1 << 4;
/// The channels the forward path writes.
pub const FORWARD_CHANNELS: u32 = CHANNEL_ALBEDO | CHANNEL_NORMAL | CHANNEL_ROUGHNESS | CHANNEL_METALLIC;

/// Receiver layer the forward shader gives a rigid mesh (bit 1 of a receiver mask).
pub const LAYER_INDEX_STATIC: u32 = 1;
/// Receiver layer the forward shader gives a skinned mesh (bit 2 of a receiver mask).
pub const LAYER_INDEX_CHARACTER: u32 = 2;

/// One decal for the forward pass: an oriented box and the surface it writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForwardDecal {
    /// Centre of the box (world meters).
    pub center: Vec3,
    /// Unit right axis (local +X).
    pub right: Vec3,
    /// Unit up axis (local +Y).
    pub up: Vec3,
    /// Unit projection axis (local +Z, into the surface).
    pub forward: Vec3,
    /// Half extents `(right, up, depth)` in meters.
    pub half_size: Vec3,
    /// Linear albedo.
    pub albedo: [f32; 3],
    /// Opacity with the lifetime fade multiplied in.
    pub opacity: f32,
    /// `0` modulate the albedo, `1` replace it.
    pub blend: f32,
    /// Perceptual roughness written with [`CHANNEL_ROUGHNESS`].
    pub roughness: f32,
    /// Metallic written with [`CHANNEL_METALLIC`].
    pub metallic: f32,
    /// Tangent-space normal xy (z reconstructed). Zero is flat.
    pub normal_xy: [f32; 2],
    /// Weight of the decal's normal, `0..=1`.
    pub normal_strength: f32,
    /// `0` hard box edge, `1` the smoothstep feather.
    pub edge_softness: f32,
    /// `smoothstep` range on `|dot(surface normal, projection axis)|`; `end <= start` disables it.
    pub angle_fade: [f32; 2],
    /// Camera-distance fade range in meters; `end <= start` disables it.
    pub distance_fade: [f32; 2],
    /// [`CHANNEL_ALBEDO`] | [`CHANNEL_NORMAL`] | [`CHANNEL_ROUGHNESS`] | [`CHANNEL_METALLIC`].
    /// [`CHANNEL_EMISSIVE`] is ignored here.
    pub channels: u32,
    /// Receiver layer bits; `0` and `u32::MAX` accept every layer.
    pub receiver_mask: u32,
    /// Atlas layer, or `None` for a solid rectangle.
    pub atlas_layer: Option<u32>,
    /// Use the atlas layer's normal page for the normal (with [`CHANNEL_NORMAL`]).
    pub normal_page: bool,
    /// Use the atlas layer's roughness/metallic page as multipliers.
    pub material_page: bool,
    /// Draw order: higher blends later. Ties keep slice order.
    pub priority: i16,
}

impl ForwardDecal {
    /// A solid, modulating, albedo-only decal box (the defaults of `regia_vfx::DecalSurface`).
    #[must_use]
    pub fn from_box(
        center: Vec3,
        rotation: regia_core::math::Quat,
        half_size: Vec3,
        color: [f32; 4],
    ) -> Self {
        Self {
            center,
            right: rotation * Vec3::X,
            up: rotation * Vec3::Y,
            forward: rotation * Vec3::Z,
            half_size,
            albedo: [color[0], color[1], color[2]],
            opacity: color[3],
            blend: 0.0,
            roughness: 1.0,
            metallic: 0.0,
            normal_xy: [0.0, 0.0],
            normal_strength: 0.0,
            edge_softness: 1.0,
            angle_fade: [0.25, 0.6],
            distance_fade: [0.0, 0.0],
            channels: CHANNEL_ALBEDO,
            receiver_mask: u32::MAX,
            atlas_layer: None,
            normal_page: false,
            material_page: false,
            priority: 0,
        }
    }

    /// Whether every field is finite and the box has volume (a malformed decal is left out of the
    /// pack instead of reaching the shader as NaN).
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let v3 = |v: Vec3| v.is_finite();
        let fs = [
            self.opacity,
            self.blend,
            self.roughness,
            self.metallic,
            self.normal_xy[0],
            self.normal_xy[1],
            self.normal_strength,
            self.edge_softness,
            self.angle_fade[0],
            self.angle_fade[1],
            self.distance_fade[0],
            self.distance_fade[1],
            self.albedo[0],
            self.albedo[1],
            self.albedo[2],
        ];
        v3(self.center)
            && v3(self.right)
            && v3(self.up)
            && v3(self.forward)
            && v3(self.half_size)
            && fs.iter().all(|f| f.is_finite())
            && self.half_size.min_element() > 1e-5
            && self.right.length_squared() > 0.5
            && self.up.length_squared() > 0.5
            && self.forward.length_squared() > 0.5
    }

    /// The eight corners of the box.
    fn corners(&self) -> [Vec3; 8] {
        let r = self.right * self.half_size.x;
        let u = self.up * self.half_size.y;
        let f = self.forward * self.half_size.z;
        let mut out = [Vec3::ZERO; 8];
        for (i, c) in out.iter_mut().enumerate() {
            let sx = if i & 1 == 0 { -1.0 } else { 1.0 };
            let sy = if i & 2 == 0 { -1.0 } else { 1.0 };
            let sz = if i & 4 == 0 { -1.0 } else { 1.0 };
            *c = self.center + r * sx + u * sy + f * sz;
        }
        out
    }

    /// Whether a surface point lies inside the box (the shader's first test).
    #[must_use]
    pub fn contains(&self, p: Vec3) -> bool {
        let rel = p - self.center;
        rel.dot(self.right).abs() <= self.half_size.x
            && rel.dot(self.up).abs() <= self.half_size.y
            && rel.dot(self.forward).abs() <= self.half_size.z
    }
}

/// The view the tile masks are built for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForwardDecalView {
    /// The view-projection the forward pass draws with.
    pub view_proj: Mat4,
    /// The view matrix (its third row is the view axis the shader compares).
    pub view: Mat4,
    /// Camera position.
    pub eye: Vec3,
    /// View size in pixels (`LabFrame::screen.xy`).
    pub width: u32,
    /// View height in pixels.
    pub height: u32,
}

/// What one pack did: the counts `vfx.decal_budget` and `render.get_decal_shading` report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForwardDecalStats {
    /// Decals handed in.
    pub submitted: u32,
    /// Decals packed for the forward pass.
    pub packed: u32,
    /// Left out: malformed, no forward channel, or past [`FORWARD_DECAL_MAX`].
    pub dropped: u32,
    /// Left out of the tiles: behind the camera, off screen, or faded out by distance.
    pub culled: u32,
    /// Tile size in pixels.
    pub tile_px: u32,
    /// Tiles across and down.
    pub tiles_x: u32,
    /// Tiles down.
    pub tiles_y: u32,
    /// Tiles whose mask is not empty.
    pub tiles_touched: u32,
    /// Most decals in one tile.
    pub max_per_tile: u32,
    /// Sum of mask bits over every tile (decal-tile pairs).
    pub tile_entries: u32,
}

/// The header the shader reads (`ForwardDecalData::header`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForwardDecalHeader {
    /// Decals in the buffer.
    pub count: u32,
    /// Tiles across.
    pub tiles_x: u32,
    /// Tiles down.
    pub tiles_y: u32,
    /// Tile edge in pixels.
    pub tile_px: u32,
    /// Camera position the tiles were built for.
    pub eye: Vec3,
    /// View axis (third row of the view matrix) the tiles were built for.
    pub view_axis: Vec3,
    /// View size the tiles were built for.
    pub size: [f32; 2],
    /// `true` when the tile masks are valid for that view.
    pub tiles_valid: bool,
}

/// A packed frame of forward decals: the buffer bytes and what was packed.
#[derive(Clone, Debug, PartialEq)]
pub struct ForwardDecalPack {
    /// Exactly [`BUFFER_BYTES`] bytes (header, decals, tile masks).
    pub bytes: Vec<u8>,
    /// The packed header.
    pub header: ForwardDecalHeader,
    /// Counts.
    pub stats: ForwardDecalStats,
    /// Index into the input slice of each packed decal, in packed (draw) order.
    pub packed: Vec<usize>,
    /// Per packed decal, its 64-bit tile mask bit count (tiles it touches).
    pub tiles_per_decal: Vec<u32>,
}

/// Tiles of a `w × h` view at `px` per tile (u64: no wrap on any target).
fn grid_tiles(w: u32, h: u32, px: u32) -> u64 {
    u64::from(w.div_ceil(px)) * u64::from(h.div_ceil(px))
}

/// The tile edge for a view: [`FORWARD_DECAL_TILE_PX`], doubled until the grid fits.
#[must_use]
pub fn tile_px_for(width: u32, height: u32) -> u32 {
    let (w, h) = (width.max(1), height.max(1));
    let mut px = FORWARD_DECAL_TILE_PX;
    while grid_tiles(w, h, px) > FORWARD_DECAL_MAX_TILES as u64 && px < 1 << 15 {
        px *= 2;
    }
    px
}

/// Inclusive pixel rectangle of a decal box on the view, or `None` when it is off screen or
/// behind the eye. A box that crosses the near plane covers the whole view (its corners behind
/// the eye do not project). One pixel of margin covers the frame's sub-pixel jitter.
fn screen_rect(decal: &ForwardDecal, view_proj: Mat4, width: u32, height: u32) -> Option<[u32; 4]> {
    let (w, h) = (width as f32, height as f32);
    let mut min = [f32::MAX; 2];
    let mut max = [f32::MIN; 2];
    let mut in_front = 0;
    for c in decal.corners() {
        let clip = view_proj * Vec4::new(c.x, c.y, c.z, 1.0);
        if clip.w <= 1e-4 {
            continue;
        }
        in_front += 1;
        let sx = (clip.x / clip.w * 0.5 + 0.5) * w;
        let sy = (0.5 - clip.y / clip.w * 0.5) * h;
        min = [min[0].min(sx), min[1].min(sy)];
        max = [max[0].max(sx), max[1].max(sy)];
    }
    if in_front == 0 {
        return None;
    }
    if in_front < 8 {
        min = [0.0, 0.0];
        max = [w, h];
    }
    if max[0] < 0.0
        || max[1] < 0.0
        || min[0] >= w
        || min[1] >= h
        || !min[0].is_finite()
        || !max[0].is_finite()
    {
        return None;
    }
    let x0 = (min[0] - 1.0).floor().max(0.0) as u32;
    let y0 = (min[1] - 1.0).floor().max(0.0) as u32;
    let x1 = ((max[0] + 1.0).ceil() as u32).min(width.saturating_sub(1));
    let y1 = ((max[1] + 1.0).ceil() as u32).min(height.saturating_sub(1));
    (x1 >= x0 && y1 >= y0).then_some([x0, y0, x1, y1])
}

/// Hermite smoothstep (the shader's).
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Packs `decals` for `view`: the ones with a forward channel, in priority order (ties keep
/// slice order), at most [`FORWARD_DECAL_MAX`], with one tile mask per screen tile.
#[must_use]
#[tracing::instrument(skip_all, name = "forward_decal_pack")]
pub fn pack_forward_decals(decals: &[ForwardDecal], view: &ForwardDecalView) -> ForwardDecalPack {
    let width = view.width.max(1);
    let height = view.height.max(1);
    let tile_px = tile_px_for(width, height);
    // A view too large for the grid even at the largest tile keeps no tiles: the shader then tests
    // every decal (`tiles_valid` false).
    let tiles_fit = grid_tiles(width, height, tile_px) <= FORWARD_DECAL_MAX_TILES as u64;
    let (tiles_x, tiles_y) = if tiles_fit {
        (width.div_ceil(tile_px), height.div_ceil(tile_px))
    } else {
        (0, 0)
    };
    let mut stats = ForwardDecalStats {
        submitted: decals.len().min(u32::MAX as usize) as u32,
        tile_px,
        tiles_x,
        tiles_y,
        ..ForwardDecalStats::default()
    };
    let mut order: Vec<usize> = (0..decals.len())
        .filter(|&i| decals[i].is_well_formed() && decals[i].channels & FORWARD_CHANNELS != 0)
        .collect();
    order.sort_by_key(|&i| decals[i].priority);
    order.truncate(FORWARD_DECAL_MAX);
    stats.dropped = (decals.len() - order.len()) as u32;

    let mut bytes = vec![0u8; BUFFER_BYTES];
    let mut masks = vec![0u64; FORWARD_DECAL_MAX_TILES];
    let mut tiles_per_decal = Vec::with_capacity(order.len());
    for (slot, &i) in order.iter().enumerate() {
        let d = &decals[i];
        write_decal(&mut bytes[HEADER_BYTES + slot * DECAL_BYTES..][..DECAL_BYTES], d);
        // Distance fade: a box wholly past the fade end draws nothing.
        let [d0, d1] = d.distance_fade;
        let far = d1 > d0 && (d.center - view.eye).length() - d.half_size.length() > d1;
        let rect = if far {
            None
        } else {
            screen_rect(d, view.view_proj, width, height)
        };
        if !tiles_fit {
            tiles_per_decal.push(0);
            continue;
        }
        let Some([x0, y0, x1, y1]) = rect else {
            stats.culled += 1;
            tiles_per_decal.push(0);
            continue;
        };
        let bit = 1u64 << slot;
        let mut touched = 0;
        for ty in (y0 / tile_px)..=(y1 / tile_px).min(tiles_y - 1) {
            for tx in (x0 / tile_px)..=(x1 / tile_px).min(tiles_x - 1) {
                masks[(ty * tiles_x + tx) as usize] |= bit;
                touched += 1;
            }
        }
        tiles_per_decal.push(touched);
    }
    let mask_base = HEADER_BYTES + FORWARD_DECAL_MAX * DECAL_BYTES;
    for (t, m) in masks.iter().enumerate().take((tiles_x * tiles_y) as usize) {
        if *m != 0 {
            stats.tiles_touched += 1;
            stats.max_per_tile = stats.max_per_tile.max(m.count_ones());
            stats.tile_entries += m.count_ones();
        }
        let o = mask_base + t * 8;
        bytes[o..o + 4].copy_from_slice(&(*m as u32).to_le_bytes());
        bytes[o + 4..o + 8].copy_from_slice(&((*m >> 32) as u32).to_le_bytes());
    }
    stats.packed = order.len() as u32;
    let row = view.view.row(2);
    let header = ForwardDecalHeader {
        count: order.len() as u32,
        tiles_x,
        tiles_y,
        tile_px,
        eye: view.eye,
        view_axis: Vec3::new(row.x, row.y, row.z),
        size: [width as f32, height as f32],
        tiles_valid: tiles_fit,
    };
    write_header(&mut bytes[..HEADER_BYTES], &header);
    ForwardDecalPack {
        bytes,
        header,
        stats,
        packed: order,
        tiles_per_decal,
    }
}

/// An empty pack (count 0): the forward shader skips the gather.
#[must_use]
pub fn empty_pack() -> ForwardDecalPack {
    let mut bytes = vec![0u8; BUFFER_BYTES];
    let header = ForwardDecalHeader {
        count: 0,
        tiles_x: 0,
        tiles_y: 0,
        tile_px: FORWARD_DECAL_TILE_PX,
        eye: Vec3::ZERO,
        view_axis: Vec3::ZERO,
        size: [0.0, 0.0],
        tiles_valid: false,
    };
    write_header(&mut bytes[..HEADER_BYTES], &header);
    ForwardDecalPack {
        bytes,
        header,
        stats: ForwardDecalStats::default(),
        packed: Vec::new(),
        tiles_per_decal: Vec::new(),
    }
}

fn put_f32s(out: &mut [u8], at: usize, v: [f32; 4]) {
    for (k, f) in v.iter().enumerate() {
        out[at + k * 4..at + k * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
}

fn put_u32s(out: &mut [u8], at: usize, v: [u32; 4]) {
    for (k, u) in v.iter().enumerate() {
        out[at + k * 4..at + k * 4 + 4].copy_from_slice(&u.to_le_bytes());
    }
}

fn write_header(out: &mut [u8], h: &ForwardDecalHeader) {
    put_u32s(out, 0, [h.count, h.tiles_x, h.tiles_y, h.tile_px]);
    put_f32s(out, 16, [h.eye.x, h.eye.y, h.eye.z, h.size[0]]);
    put_f32s(out, 32, [h.view_axis.x, h.view_axis.y, h.view_axis.z, h.size[1]]);
    put_u32s(out, 48, [u32::from(h.tiles_valid), 2, 0, 0]);
}

/// Atlas layer value of a solid decal.
pub const NO_ATLAS: u32 = u32::MAX;

fn write_decal(out: &mut [u8], d: &ForwardDecal) {
    let inv = |h: f32| 1.0 / h.max(1e-5);
    let (r, u, f) = (d.right.normalize(), d.up.normalize(), d.forward.normalize());
    put_f32s(out, 0, [r.x, r.y, r.z, inv(d.half_size.x)]);
    put_f32s(out, 16, [u.x, u.y, u.z, inv(d.half_size.y)]);
    put_f32s(out, 32, [f.x, f.y, f.z, inv(d.half_size.z)]);
    put_f32s(
        out,
        48,
        [d.center.x, d.center.y, d.center.z, d.opacity.clamp(0.0, 1.0)],
    );
    put_f32s(out, 64, [d.albedo[0], d.albedo[1], d.albedo[2], d.blend]);
    put_f32s(
        out,
        80,
        [
            d.roughness.clamp(0.0, 1.0),
            d.metallic.clamp(0.0, 1.0),
            d.normal_strength.clamp(0.0, 1.0),
            d.edge_softness.clamp(0.0, 1.0),
        ],
    );
    put_f32s(
        out,
        96,
        [d.normal_xy[0], d.normal_xy[1], d.angle_fade[0], d.angle_fade[1]],
    );
    let layer = d
        .atlas_layer
        .filter(|l| *l < FORWARD_DECAL_ATLAS_LAYERS)
        .unwrap_or(NO_ATLAS);
    let pages = u32::from(d.normal_page) | (u32::from(d.material_page) << 1);
    let mask = if d.receiver_mask == u32::MAX {
        0
    } else {
        d.receiver_mask
    };
    put_u32s(out, 112, [d.channels & FORWARD_CHANNELS, mask, layer, pages]);
    put_f32s(out, 128, [d.distance_fade[0], d.distance_fade[1], 0.0, 0.0]);
}

/// The CPU mirror of the shader's blend for one surface point: what the forward pass shades
/// with after the decals (tests and the agent's probe use it as the oracle; no atlas pages).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecalSurfacePoint {
    /// Linear base colour.
    pub base: [f32; 3],
    /// Shading normal (unit).
    pub normal: Vec3,
    /// Perceptual roughness.
    pub roughness: f32,
    /// Metallic.
    pub metallic: f32,
}

/// Applies `decals` (in the order given, solid decals only: an atlas layer reads as full
/// coverage) to `surface` at world point `p` seen from `eye`, as the forward shader does for a
/// receiver of layer `layer`.
#[must_use]
pub fn blend_point(
    decals: &[ForwardDecal],
    surface: DecalSurfacePoint,
    p: Vec3,
    eye: Vec3,
    layer: u32,
) -> DecalSurfacePoint {
    let mut s = surface;
    let n_recv = surface.normal;
    for d in decals {
        if !d.is_well_formed() || !d.contains(p) {
            continue;
        }
        let mask = d.receiver_mask;
        if mask != 0 && mask != u32::MAX && mask & (1 << layer.min(31)) == 0 {
            continue;
        }
        let rel = p - d.center;
        let x = rel.dot(d.right) / d.half_size.x;
        let y = rel.dot(d.up) / d.half_size.y;
        let facing = n_recv.dot(d.forward).abs();
        let grazing = if d.angle_fade[1] > d.angle_fade[0] {
            smoothstep(d.angle_fade[0], d.angle_fade[1], facing)
        } else {
            1.0
        };
        let feather = (1.0 - smoothstep(0.7, 1.0, x.abs())) * (1.0 - smoothstep(0.7, 1.0, y.abs()));
        let edge = 1.0 + (feather - 1.0) * d.edge_softness.clamp(0.0, 1.0);
        let dist = (eye - d.center).length();
        let dist_fade = if d.distance_fade[1] > d.distance_fade[0] {
            1.0 - smoothstep(d.distance_fade[0], d.distance_fade[1], dist)
        } else {
            1.0
        };
        let w = d.opacity.clamp(0.0, 1.0) * grazing * edge * dist_fade;
        if w <= 1e-4 {
            continue;
        }
        let ch = d.channels;
        if ch & CHANNEL_ALBEDO != 0 {
            for (b, a) in s.base.iter_mut().zip(d.albedo) {
                *b = if d.blend < 0.5 {
                    *b * (1.0 + (a - 1.0) * w)
                } else {
                    *b + (a - *b) * w
                };
            }
        }
        if ch & CHANNEL_NORMAL != 0 {
            let n_shade = whiteout(n_recv, d.right, d.up, d.normal_xy);
            let strength = d.normal_strength.clamp(0.0, 1.0);
            let n_decal = s.normal.lerp(n_shade, strength).normalize_or(s.normal);
            s.normal = s.normal.lerp(n_decal, w).normalize_or(s.normal);
        }
        if ch & CHANNEL_ROUGHNESS != 0 {
            s.roughness += (d.roughness.clamp(0.0, 1.0) - s.roughness) * w;
        }
        if ch & CHANNEL_METALLIC != 0 {
            s.metallic += (d.metallic.clamp(0.0, 1.0) - s.metallic) * w;
        }
    }
    s
}

/// The decal's tangent-space normal `(nx, ny, √(1 − nx² − ny²))` in the frame t = the decal's
/// right axis on the receiver, b = n × t, n = the receiver normal (whiteout over a flat base:
/// Barré-Brisebois and Hill, "Blending in Detail", 2012).
#[must_use]
pub fn whiteout(n: Vec3, right: Vec3, up: Vec3, nxy: [f32; 2]) -> Vec3 {
    let (nx, ny) = (nxy[0], nxy[1]);
    if nx * nx + ny * ny <= 1e-6 {
        return n;
    }
    let nz = (1.0 - nx * nx - ny * ny).max(0.0).sqrt();
    let mut t = right - n * right.dot(n);
    if t.length_squared() < 1e-6 {
        t = up - n * up.dot(n);
    }
    let t = t.normalize_or(Vec3::X);
    let b = n.cross(t);
    let ts = Vec3::new(nx, ny, nz).normalize_or(Vec3::Z);
    (t * ts.x + b * ts.y + n * ts.z).normalize_or(n)
}

/// One decal atlas for the forward path: [`FORWARD_DECAL_ATLAS_PAGES`] RGBA8 pages per layer,
/// page-major inside a layer (albedo, normal, roughness/metallic), each
/// [`FORWARD_DECAL_ATLAS_SIZE`]² texels, row-major, top-left origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardDecalAtlas {
    /// Decal layers (at most [`FORWARD_DECAL_ATLAS_LAYERS`] are uploaded).
    pub layers: u32,
    /// The bytes, `layers × PAGES × SIZE² × 4`.
    pub rgba: Vec<u8>,
}

/// Why an atlas or a pack was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardDecalError {
    /// The atlas byte count is not `layers × pages × size² × 4`.
    AtlasSize {
        /// Bytes expected.
        expected: usize,
        /// Bytes given.
        got: usize,
    },
}

impl std::fmt::Display for ForwardDecalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtlasSize { expected, got } => {
                write!(f, "forward decal atlas has {got} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for ForwardDecalError {}

impl ForwardDecalAtlas {
    /// Bytes of one page.
    pub const PAGE_BYTES: usize = (FORWARD_DECAL_ATLAS_SIZE * FORWARD_DECAL_ATLAS_SIZE * 4) as usize;

    /// Checks the byte count.
    ///
    /// # Errors
    /// [`ForwardDecalError::AtlasSize`] when it does not match the layer count.
    pub fn validate(&self) -> Result<(), ForwardDecalError> {
        let expected = self.layers as usize * FORWARD_DECAL_ATLAS_PAGES as usize * Self::PAGE_BYTES;
        if self.rgba.len() == expected {
            Ok(())
        } else {
            Err(ForwardDecalError::AtlasSize {
                expected,
                got: self.rgba.len(),
            })
        }
    }
}

const UNSET: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;
static PROCESS: AtomicU8 = AtomicU8::new(UNSET);

/// The environment variable that turns the forward-decal path on (`1`) before the first frame.
pub const FORWARD_DECALS_ENV: &str = "REGIA_FORWARD_DECALS";

/// Whether this process shades decals in the forward pass. Latched on the first read (what
/// [`set_process_forward_decals`] set before, else [`FORWARD_DECALS_ENV`], else off), so the view
/// layout and every forward module of the process agree.
#[must_use]
pub fn process_forward_decals() -> bool {
    let cur = PROCESS.load(Ordering::Acquire);
    if cur != UNSET {
        return cur == ON;
    }
    let from_env = regia_core::brand::env_var(FORWARD_DECALS_ENV).is_ok_and(|v| v.trim() == "1");
    let want = if from_env { ON } else { OFF };
    match PROCESS.compare_exchange(UNSET, want, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => want == ON,
        Err(now) => now == ON,
    }
}

/// Sets the process-wide switch. Returns `false` (and changes nothing) once it is latched:
/// after the first [`process_forward_decals`] read, which the first renderer makes.
pub fn set_process_forward_decals(on: bool) -> bool {
    let want = if on { ON } else { OFF };
    match PROCESS.compare_exchange(UNSET, want, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => true,
        Err(now) => now == want,
    }
}

/// Startup: a render profile that opted in sets the process switch before the first frame composes
/// a forward module. A profile with it off leaves the switch alone (so it never undoes
/// [`FORWARD_DECALS_ENV`] or a host's [`set_process_forward_decals`]).
pub(crate) fn apply_profile_switch(settings: Option<regia_app::prelude::Res<crate::RenderSettings>>) {
    let Some(settings) = settings else {
        return;
    };
    if settings.profile.forward_decals && !set_process_forward_decals(true) {
        tracing::warn!(
            "forward decals: the profile asks for them, but the process switch was already latched off"
        );
    }
}

/// Whether a device's forward view layout carries the forward-decal binding: the switch is on,
/// not Android (the lite shader's tiers keep the post path), and the device has room.
#[must_use]
pub fn binding_supported(limits: &wgpu::Limits) -> bool {
    !cfg!(target_os = "android")
        && process_forward_decals()
        && limits.max_storage_buffers_per_shader_stage >= FORWARD_MIN_STORAGE_BUFFERS
        && limits.max_storage_buffer_binding_size >= STORAGE_BYTES as u64
}

/// Adds [`FORWARD_DEF`] to a forward module's defines on a device whose view layout has the
/// binding (every forward module of the device, so the prepass and the colour pass agree).
pub fn add_forward_defs(
    device: &wgpu::Device,
    defs: &mut std::collections::HashMap<String, naga_oil::compose::ShaderDefValue>,
) {
    if binding_supported(&device.limits()) {
        defs.insert(
            FORWARD_DEF.to_string(),
            naga_oil::compose::ShaderDefValue::Bool(true),
        );
    }
}

/// The view-layout entry of the storage buffer.
#[must_use]
pub fn layout_entry() -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding: DATA_BINDING,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// What the host hands the renderer between frames: this frame's decals and the atlas.
#[derive(Clone, Debug, Default)]
pub struct ForwardDecalHost {
    /// The decals the next frame shades.
    pub decals: Vec<ForwardDecal>,
    /// The atlas (staged once after each change).
    pub atlas: Option<ForwardDecalAtlas>,
    /// Bumped by every atlas change.
    pub atlas_generation: u64,
    /// The generation of the last layer feed taken ([`Self::take_feed`]).
    feed_generation: Option<u64>,
    /// The last layer atlas taken (compared by pointer).
    feed_atlas: Option<Arc<ForwardDecalAtlas>>,
}

/// A translucent layer's forward decals for the next frame
/// (`TranslucentLayer::forward_decal_feed`, ADR-3255). The VFX runtime publishes one per game
/// frame with the forward copies of its pool's decals.
#[derive(Clone, Debug, Default)]
pub struct ForwardDecalFeed {
    /// Bumped by the layer whenever [`Self::decals`] changes; the frame copies the list only when
    /// the generation moved.
    pub generation: u64,
    /// The decals the next frame shades in the forward pass.
    pub decals: Arc<[ForwardDecal]>,
    /// The layer's atlas; the frame stages it again only when the pointer changes.
    pub atlas: Option<Arc<ForwardDecalAtlas>>,
}

impl ForwardDecalHost {
    /// Takes a layer's feed: copies its decals when its generation moved (or on the first feed),
    /// as [`Self::set_decals`] does, and its atlas when the pointer changed. Returns whether the
    /// decals were copied.
    ///
    /// # Errors
    /// [`ForwardDecalError::AtlasSize`] when the feed's atlas is malformed (its decals are still
    /// taken, and the same atlas is not tried again).
    pub fn take_feed(&mut self, feed: &ForwardDecalFeed) -> Result<bool, ForwardDecalError> {
        let moved = self.feed_generation != Some(feed.generation);
        if moved {
            self.feed_generation = Some(feed.generation);
            self.set_decals(&feed.decals);
        }
        if let Some(atlas) = &feed.atlas
            && !self.feed_atlas.as_ref().is_some_and(|a| Arc::ptr_eq(a, atlas))
        {
            self.feed_atlas = Some(Arc::clone(atlas));
            self.set_atlas(atlas)?;
        }
        Ok(moved)
    }

    /// Replaces the decals. Returns `false` (and keeps none) while the process switch is off.
    pub fn set_decals(&mut self, decals: &[ForwardDecal]) -> bool {
        self.decals.clear();
        if !process_forward_decals() {
            return false;
        }
        self.decals.extend_from_slice(decals);
        true
    }

    /// Replaces the atlas.
    ///
    /// # Errors
    /// [`ForwardDecalError::AtlasSize`] when the bytes do not match the layer count.
    pub fn set_atlas(&mut self, atlas: &ForwardDecalAtlas) -> Result<(), ForwardDecalError> {
        atlas.validate()?;
        self.atlas = Some(atlas.clone());
        self.atlas_generation += 1;
        Ok(())
    }
}

/// The atlas region's bytes for `atlas`: its first [`FORWARD_DECAL_ATLAS_LAYERS`] layers, the
/// rest transparent.
#[must_use]
pub fn atlas_region(atlas: &ForwardDecalAtlas) -> Vec<u8> {
    let mut out = vec![0u8; ATLAS_BYTES];
    let n = (atlas.layers.min(FORWARD_DECAL_ATLAS_LAYERS) as usize
        * FORWARD_DECAL_ATLAS_PAGES as usize
        * ForwardDecalAtlas::PAGE_BYTES)
        .min(atlas.rgba.len());
    out[..n].copy_from_slice(&atlas.rgba[..n]);
    out
}

/// The GPU side: the storage buffer, created once per device with the view layout, and the last
/// pack's counts.
pub struct ForwardDecalBindings {
    buffer: wgpu::Buffer,
    stats: ForwardDecalStats,
    uploads: u64,
    last_bytes: Vec<u8>,
    atlas_generation: u64,
}

impl ForwardDecalBindings {
    /// Creates the buffer (zero-filled: an empty pack and a transparent atlas).
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("forward decals"),
            size: STORAGE_BYTES as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            // wgpu zero-fills a new buffer: decal count 0, so the shader skips the gather until
            // the first pack is staged.
            mapped_at_creation: false,
        });
        Self {
            buffer,
            stats: ForwardDecalStats::default(),
            uploads: 0,
            last_bytes: empty_pack().bytes,
            atlas_generation: 0,
        }
    }

    /// The view-group entry, for [`layout_entry`].
    #[must_use]
    pub fn bind_entry(&self) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry {
            binding: DATA_BINDING,
            resource: self.buffer.as_entire_binding(),
        }
    }

    /// Packs the host's decals for `view` and hands the writes to `write` (`buffer, offset,
    /// bytes`; the frame defers them on its staging belt, as it writes the frame uniform): the
    /// data region when its bytes changed since the last write, the atlas region after an atlas
    /// change. No decals writes the empty header, so the shader skips the gather. Returns whether
    /// anything was written.
    pub fn stage(
        &mut self,
        host: &ForwardDecalHost,
        view: &ForwardDecalView,
        mut write: impl FnMut(&wgpu::Buffer, u64, &[u8]),
    ) -> bool {
        let _span = tracing::trace_span!("forward_decal_stage").entered();
        let pack = if host.decals.is_empty() {
            empty_pack()
        } else {
            pack_forward_decals(&host.decals, view)
        };
        let mut wrote = false;
        if pack.bytes != self.last_bytes {
            write(&self.buffer, 0, &pack.bytes);
            self.last_bytes = pack.bytes;
            wrote = true;
        }
        if host.atlas_generation != self.atlas_generation {
            if let Some(atlas) = &host.atlas {
                write(&self.buffer, ATLAS_OFFSET as u64, &atlas_region(atlas));
                wrote = true;
            }
            self.atlas_generation = host.atlas_generation;
        }
        self.stats = pack.stats;
        self.uploads += 1;
        wrote
    }

    /// The last pack's counts.
    #[must_use]
    pub fn stats(&self) -> ForwardDecalStats {
        self.stats
    }

    /// How many packs were staged.
    #[must_use]
    pub fn uploads(&self) -> u64 {
        self.uploads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;

    fn view(w: u32, h: u32) -> ForwardDecalView {
        let eye = Vec3::new(0.0, 3.0, 3.0);
        let view = crate::view_math::look_at_view(eye, Vec3::ZERO, Vec3::Y);
        let proj =
            regia_core::glam::camera::rh::proj::directx::perspective(0.9, w as f32 / h as f32, 0.1, 100.0);
        ForwardDecalView {
            view_proj: proj * view,
            view,
            eye,
            width: w,
            height: h,
        }
    }

    fn floor_decal(x: f32) -> ForwardDecal {
        let mut d = ForwardDecal::from_box(
            Vec3::new(x, 0.0, 0.0),
            Quat::from_rotation_x(std::f32::consts::FRAC_PI_2),
            Vec3::new(0.3, 0.3, 0.2),
            [0.8, 0.1, 0.1, 1.0],
        );
        d.channels = CHANNEL_ALBEDO | CHANNEL_NORMAL | CHANNEL_ROUGHNESS;
        d
    }

    fn mask_of(pack: &ForwardDecalPack, tile: usize) -> u64 {
        let o = HEADER_BYTES + FORWARD_DECAL_MAX * DECAL_BYTES + tile * 8;
        let lo = u32::from_le_bytes(pack.bytes[o..o + 4].try_into().unwrap_or([0; 4]));
        let hi = u32::from_le_bytes(pack.bytes[o + 4..o + 8].try_into().unwrap_or([0; 4]));
        u64::from(lo) | (u64::from(hi) << 32)
    }

    #[test]
    fn the_buffer_layout_is_what_the_shader_declares() {
        assert_eq!(DECAL_BYTES, 9 * 16);
        assert_eq!(BUFFER_BYTES, 64 + 64 * 144 + 8192 * 8);
        assert!(BUFFER_BYTES.is_multiple_of(16));
    }

    #[test]
    fn a_left_decal_sets_left_tiles_only_and_a_decal_behind_is_culled() {
        let v = view(256, 192);
        let mut behind = floor_decal(0.0);
        behind.center = Vec3::new(0.0, 3.0, 8.0);
        let pack = pack_forward_decals(&[floor_decal(-1.2), behind], &v);
        assert_eq!(pack.stats.packed, 2);
        assert_eq!(pack.stats.culled, 1);
        assert!(pack.stats.tiles_touched > 0);
        let tx = pack.stats.tiles_x as usize;
        let mut left = 0;
        let mut right = 0;
        for t in 0..(tx * pack.stats.tiles_y as usize) {
            if mask_of(&pack, t) & 1 != 0 {
                if t % tx < tx / 2 { left += 1 } else { right += 1 }
            }
            assert_eq!(mask_of(&pack, t) & 2, 0, "the culled decal sets no tile");
        }
        assert!(left > 0);
        assert_eq!(right, 0, "the left decal reaches no right-half tile");
    }

    #[test]
    fn priority_orders_the_bits_and_ties_keep_slice_order() {
        let v = view(128, 128);
        let mut a = floor_decal(0.0);
        a.priority = 5;
        let b = floor_decal(0.1);
        let c = floor_decal(0.2);
        let pack = pack_forward_decals(&[a, b, c], &v);
        assert_eq!(pack.packed, vec![1, 2, 0]);
    }

    #[test]
    fn emissive_only_malformed_and_overflow_decals_are_dropped() {
        let v = view(128, 128);
        let mut glow = floor_decal(0.0);
        glow.channels = CHANNEL_EMISSIVE;
        let mut nan = floor_decal(0.0);
        nan.center.x = f32::NAN;
        let mut flat = floor_decal(0.0);
        flat.half_size.z = 0.0;
        let mut many = vec![glow, nan, flat];
        many.extend((0..70).map(|i| floor_decal(i as f32 * 0.01)));
        let pack = pack_forward_decals(&many, &v);
        assert_eq!(pack.stats.packed, FORWARD_DECAL_MAX as u32);
        assert_eq!(pack.stats.dropped, 3 + 6);
        assert!(pack.stats.max_per_tile <= 64);
    }

    #[test]
    fn a_huge_view_grows_the_tile_to_fit_the_grid() {
        let px = tile_px_for(7680, 4320);
        assert!((7680u32.div_ceil(px) * 4320u32.div_ceil(px)) as usize <= FORWARD_DECAL_MAX_TILES);
        assert_eq!(tile_px_for(1920, 1080), 32);
        let pack = pack_forward_decals(&[floor_decal(0.0)], &view(u32::MAX, u32::MAX));
        assert!(!pack.header.tiles_valid, "no grid fits: every decal is tested");
        assert_eq!(
            (pack.stats.tiles_x, pack.stats.tiles_y, pack.stats.tiles_touched),
            (0, 0, 0)
        );
        assert_eq!(pack.stats.packed, 1);
    }

    #[test]
    fn a_box_across_the_near_plane_covers_every_tile() {
        let v = view(64, 64);
        let mut d = floor_decal(0.0);
        d.center = v.eye;
        d.half_size = Vec3::splat(1.0);
        let pack = pack_forward_decals(&[d], &v);
        assert_eq!(pack.stats.tiles_touched, pack.stats.tiles_x * pack.stats.tiles_y);
    }

    #[test]
    fn an_empty_pack_has_count_zero() {
        let p = empty_pack();
        assert_eq!(p.bytes.len(), BUFFER_BYTES);
        assert_eq!(&p.bytes[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn the_cpu_blend_writes_the_decal_normal_roughness_and_paint() {
        let mut d = floor_decal(0.0);
        d.blend = 1.0;
        d.roughness = 0.25;
        d.normal_xy = [0.3, 0.0];
        d.normal_strength = 1.0;
        d.angle_fade = [0.0, 0.0];
        d.edge_softness = 0.0;
        d.receiver_mask = 0b110;
        let s = DecalSurfacePoint {
            base: [0.5, 0.5, 0.5],
            normal: Vec3::Y,
            roughness: 0.8,
            metallic: 0.0,
        };
        let out = blend_point(&[d], s, Vec3::ZERO, Vec3::new(0.0, 3.0, 3.0), LAYER_INDEX_STATIC);
        for (got, want) in out.base.iter().zip([0.8, 0.1, 0.1]) {
            assert!((got - want).abs() < 1e-6, "{:?}", out.base);
        }
        assert!((out.roughness - 0.25).abs() < 1e-6);
        let tilt = out.normal.dot(Vec3::Y).acos().to_degrees();
        assert!((tilt - 17.46).abs() < 0.1, "{tilt}");
        assert!(
            out.normal.dot(d.right) > 0.29,
            "tilts toward the decal's right axis"
        );
        // A character is outside the mask; a point outside the box is untouched.
        assert_eq!(blend_point(&[d], s, Vec3::ZERO, Vec3::Y, 0), s);
        assert_eq!(blend_point(&[d], s, Vec3::new(2.0, 0.0, 0.0), Vec3::Y, 1), s);
    }

    #[test]
    fn the_switch_latches() {
        // The first read latches the value; later sets cannot flip it.
        let first = process_forward_decals();
        assert!(set_process_forward_decals(first));
        assert!(!set_process_forward_decals(!first));
        assert_eq!(process_forward_decals(), first);
    }

    #[test]
    fn atlas_size_is_checked() {
        let bad = ForwardDecalAtlas {
            layers: 1,
            rgba: vec![0; 7],
        };
        assert!(matches!(bad.validate(), Err(ForwardDecalError::AtlasSize { .. })));
        let good = ForwardDecalAtlas {
            layers: 1,
            rgba: vec![0; 3 * ForwardDecalAtlas::PAGE_BYTES],
        };
        assert!(good.validate().is_ok());
    }

    #[test]
    fn a_layer_feed_is_copied_when_its_generation_moves_and_its_atlas_once() {
        let on = process_forward_decals();
        let mut host = ForwardDecalHost::default();
        let atlas = Arc::new(ForwardDecalAtlas {
            layers: 1,
            rgba: vec![0; 3 * ForwardDecalAtlas::PAGE_BYTES],
        });
        let mut feed = ForwardDecalFeed {
            generation: 0,
            decals: vec![floor_decal(0.0)].into(),
            atlas: Some(Arc::clone(&atlas)),
        };
        assert_eq!(host.take_feed(&feed), Ok(true), "the first feed is taken");
        assert_eq!(host.decals.len(), usize::from(on), "kept only with the switch on");
        assert_eq!(host.atlas_generation, 1);
        // The same generation and atlas: nothing copied, the host's own list stays.
        host.decals.clear();
        assert_eq!(host.take_feed(&feed), Ok(false));
        assert!(host.decals.is_empty());
        assert_eq!(host.atlas_generation, 1, "the same atlas is not staged again");
        // A new generation replaces the list; an equal but new atlas is staged again.
        feed.generation = 1;
        feed.decals = vec![floor_decal(0.0), floor_decal(1.0)].into();
        feed.atlas = Some(Arc::new((*atlas).clone()));
        assert_eq!(host.take_feed(&feed), Ok(true));
        assert_eq!(host.decals.len(), if on { 2 } else { 0 });
        assert_eq!(host.atlas_generation, 2);
        // A malformed atlas is refused once; the decals are still taken.
        feed.generation = 2;
        feed.atlas = Some(Arc::new(ForwardDecalAtlas {
            layers: 1,
            rgba: vec![0; 5],
        }));
        assert!(matches!(
            host.take_feed(&feed),
            Err(ForwardDecalError::AtlasSize { .. })
        ));
        assert_eq!(host.take_feed(&feed), Ok(false), "not tried again");
        assert_eq!(host.atlas_generation, 2);
    }
}
