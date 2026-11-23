// SPDX-License-Identifier: MPL-2.0
//! `Draw2d`: an instanced, allocation-free 2D draw list for HUDs and menus that redraw every frame
//! (P-05).
//!
//! [`Draw2dList`] is a plain struct of instances: rounded boxes, box outlines, anti-aliased
//! segments, glyphs of shaped text runs and images. [`Draw2dLayer`] draws a list with one
//! instanced pipeline and one shader (`draw2d.wgsl`): every instance is one quad, coverage is
//! computed per pixel (signed distance for boxes and segments, the glyph atlas for text, the image
//! for images), so a frame of a few thousand instances is one draw call and one buffer write.
//! Glyphs live in a paged atlas ([`regia_ui::atlas::GlyphAtlases`]): a glyph is rasterized once
//! and its page uploaded once; later frames only look it up.
//!
//! Coordinates are **physical pixels of the target, origin top-left, +Y down**. Colours are
//! [`Color`] (linear, straight alpha); the list stores them premultiplied and the layer blends
//! premultiplied-over in the target's space (linear on sRGB and float targets; an 8-bit unorm
//! target receives sRGB-encoded values).
//!
//! Two ways to use it:
//! - a shell that owns its loop keeps a [`Draw2dList`] and a [`Draw2dLayer`]:
//!   `list.begin_frame()`, push, `list.end_frame()`, then
//!   [`Draw2dLayer::draw_list`] into the frame (after [`Draw2dLayer::warm`] at load);
//! - an ECS app adds [`Draw2dPlugin`] (after `RenderPlugin`) and draws through
//!   `ResMut<Draw2d>`: the list is cleared in `PreUpdate` and drawn as a window overlay layer
//!   above the quick layer and under the runtime UI.
//!
//! This is not a replacement for [`crate::Draw`] (lyon-tessellated paths, meshes, transforms):
//! it is the fast path for flat HUD shapes. Agents read [`Draw2dStats`] through
//! `quick.draw2d_stats` (`book/agents/quick-draw2d.md`).

mod gpu;
pub mod hud;
pub mod reference;
pub mod tiles;

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::*;
use regia_core::math::{Color, Vec2};
use regia_render::{ImmediateImage, WindowOverlay};
use regia_ui::atlas::{GlyphAtlases, MAX_PAGES, PAGE_SIZE};
use regia_ui::text::{ShapedText, TextSystem};

pub use gpu::{Draw2dGpuStats, Draw2dLayer, UI_FORMAT};
pub use hud::{HudAnchor, HudLayer, TextAlign};
pub use tiles::{DirtyRun, Draw2dTileStats, Draw2dTiles};

/// The WGSL source of the draw list's pipeline (`shaders/draw2d.wgsl`).
pub const SHADER: &str = include_str!("../../shaders/draw2d.wgsl");

/// Image slots a list can draw from in one frame ([`Draw2dImage`]).
pub const IMAGE_SLOTS: usize = 4;

/// A clip rectangle that clips nothing.
const NO_CLIP: [f32; 4] = [-1.0e9, -1.0e9, 1.0e9, 1.0e9];

/// Instance kinds (`params[0]` of a [`Draw2dInstance`]).
pub mod kind {
    /// A box with rounded corners: `params[1]` radius, `params[2]` edge feather in pixels (0 or
    /// 1: a one-pixel anti-aliased edge), `params[3]` gradient (0 none, 1 vertical, 2
    /// horizontal: `color` at the top or left edge, `uv` at the bottom or right edge).
    pub const BOX: f32 = 0.0;
    /// A segment: `geo` is `a.x a.y b.x b.y`, `params[1]` half width, `params[2]` 1 for round caps.
    pub const SEGMENT: f32 = 1.0;
    /// A monochrome glyph: `uv` in its atlas page, `params[2]` the page.
    pub const GLYPH: f32 = 2.0;
    /// A colour (emoji) glyph: `uv` in its colour atlas page, `params[2]` the page.
    pub const COLOR_GLYPH: f32 = 3.0;
    /// An image: `uv` in the image, `params[3]` the slot.
    pub const IMAGE: f32 = 4.0;
    /// A box outline: `params[1]` radius, `params[2]` thickness.
    pub const OUTLINE: f32 = 5.0;
}

/// A rectangle in physical pixels: top-left corner and size.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PxRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl PxRect {
    /// A rectangle from its top-left corner and size.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// `[x0, y0, x1, y1]`.
    #[must_use]
    pub fn corners(&self) -> [f32; 4] {
        [self.x, self.y, self.x + self.w, self.y + self.h]
    }
}

/// An image slot of the layer (`0..IMAGE_SLOTS`), filled with [`Draw2dList::set_image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Draw2dImage(pub u8);

/// One instance: one quad of the instanced pipeline (80 bytes, the vertex layout of
/// `draw2d.wgsl`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Draw2dInstance {
    /// A rectangle `x0 y0 x1 y1`, or a segment `ax ay bx by`.
    pub geo: [f32; 4],
    /// Texture rectangle `u0 v0 u1 v1` (glyphs and images).
    pub uv: [f32; 4],
    /// Premultiplied linear colour.
    pub color: [f32; 4],
    /// Clip rectangle `x0 y0 x1 y1` (pixels outside are discarded).
    pub clip: [f32; 4],
    /// [`kind`] and its parameters.
    pub params: [f32; 4],
}

/// Instances per kind in the current list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Draw2dKinds {
    /// Boxes.
    pub boxes: u32,
    /// Box outlines.
    pub outlines: u32,
    /// Segments.
    pub segments: u32,
    /// Glyphs (monochrome and colour).
    pub glyphs: u32,
    /// Images.
    pub images: u32,
}

/// What a list holds and what its glyph atlas did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Draw2dListStats {
    /// Instances in the list.
    pub instances: u32,
    /// Boxes.
    pub boxes: u32,
    /// Box outlines.
    pub outlines: u32,
    /// Segments.
    pub segments: u32,
    /// Glyph instances.
    pub glyphs: u32,
    /// Image instances.
    pub images: u32,
    /// Live glyph atlas pages (monochrome plus colour).
    pub atlas_pages: u32,
    /// Glyphs cached in the atlas.
    pub glyphs_cached: u32,
    /// Glyphs rasterized so far (cache misses).
    pub glyphs_rasterized: u64,
    /// Glyphs not drawn because the atlas was full (every page pinned by the frame).
    pub glyphs_refused: u64,
    /// Atlas pages copied into a hand-over so far (each one is uploaded once).
    pub pages_staged: u64,
}

/// An atlas page or image the layer must upload.
#[derive(Debug, Clone, Default)]
pub struct StagedPage {
    /// `0` monochrome (R8), `1` colour (RGBA8, premultiplied sRGB).
    pub kind: u8,
    /// Page index (the texture-array layer).
    pub page: u32,
    /// The page's pixels (`PAGE_SIZE`² × bytes per pixel).
    pub data: Vec<u8>,
}

/// One frame of a list, handed from the thread that built it to the layer that draws it
/// ([`Draw2dList::hand_over`]). Its buffers are reused: a steady frame allocates nothing.
#[derive(Debug, Default)]
pub struct Draw2dFrame {
    instances: Vec<Draw2dInstance>,
    pages: Vec<StagedPage>,
    pages_len: usize,
    images: Vec<(Draw2dImage, ImmediateImage)>,
    /// Pixels behind unchanged instances may have changed (an atlas page was recycled or an
    /// image slot refilled): the retained target redraws everything once.
    invalidate: bool,
}

impl Draw2dFrame {
    /// The frame's instances.
    #[must_use]
    pub fn instances(&self) -> &[Draw2dInstance] {
        &self.instances
    }
}

/// The 2D draw list: instances in draw order plus the CPU glyph atlas their glyphs sample.
pub struct Draw2dList {
    instances: Vec<Draw2dInstance>,
    clip: [f32; 4],
    kinds: Draw2dKinds,
    atlas: GlyphAtlases,
    /// Glyphs with no ink (spaces): never rasterized again.
    blank: HashSet<cosmic_text::CacheKey>,
    pages_staged: u64,
    /// Atlas page recycles already reported to a hand-over.
    wipes_seen: u64,
    rasterized: u64,
    dirty: Vec<(u8, u32)>,
    images: Vec<(Draw2dImage, ImmediateImage)>,
    open: bool,
}

impl Default for Draw2dList {
    fn default() -> Self {
        Self {
            instances: Vec::new(),
            clip: NO_CLIP,
            kinds: Draw2dKinds::default(),
            atlas: GlyphAtlases::default(),
            blank: HashSet::new(),
            pages_staged: 0,
            wipes_seen: 0,
            rasterized: 0,
            dirty: Vec::new(),
            images: Vec::new(),
            open: false,
        }
    }
}

impl std::fmt::Debug for Draw2dList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Draw2dList")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Premultiplies a linear straight-alpha colour.
fn premul(c: Color) -> [f32; 4] {
    let a = c.a.clamp(0.0, 1.0);
    [c.r * a, c.g * a, c.b * a, a]
}

impl Draw2dList {
    /// Bytes of one instance in the instance buffer.
    pub const INSTANCE_BYTES: u64 = std::mem::size_of::<Draw2dInstance>() as u64;

    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a frame: clears the instances and the clip, and opens the glyph atlas frame (a page
    /// that served this frame's glyphs is never recycled before [`Self::end_frame`]).
    pub fn begin_frame(&mut self) {
        self.instances.clear();
        self.clip = NO_CLIP;
        self.kinds = Draw2dKinds::default();
        self.atlas.begin_frame();
        self.open = true;
    }

    /// Ends the frame: closes the atlas frame and collects the pages its new glyphs changed.
    pub fn end_frame(&mut self) {
        if self.open {
            self.atlas.end_frame();
            self.open = false;
        }
        // `take_dirty_pages` hands back an empty, unallocated list when nothing changed.
        for page in self.atlas.take_dirty_pages() {
            if !self.dirty.contains(&page) {
                self.dirty.push(page);
            }
        }
    }

    /// Sets the clip rectangle of the instances pushed after it (`None`: no clip).
    pub fn set_clip(&mut self, clip: Option<PxRect>) {
        self.clip = clip.map_or(NO_CLIP, |r| r.corners());
    }

    /// A filled box with corners rounded by `radius` pixels (clamped to half the shorter side; a
    /// radius of half a square's side draws a disc). Edges are anti-aliased.
    pub fn push_box(&mut self, rect: PxRect, color: Color, radius: f32) {
        self.kinds.boxes += 1;
        self.instances.push(Draw2dInstance {
            geo: rect.corners(),
            uv: [0.0; 4],
            color: premul(color),
            clip: self.clip,
            params: [kind::BOX, radius.max(0.0), 0.0, 0.0],
        });
    }

    /// The outline of a rounded box: a band `thickness` pixels wide inside `rect`'s edge (a ring
    /// when the box is a disc).
    pub fn push_outline(&mut self, rect: PxRect, color: Color, radius: f32, thickness: f32) {
        self.kinds.outlines += 1;
        self.instances.push(Draw2dInstance {
            geo: rect.corners(),
            uv: [0.0; 4],
            color: premul(color),
            clip: self.clip,
            params: [kind::OUTLINE, radius.max(0.0), thickness.max(0.0), 0.0],
        });
    }

    /// A box whose edge fades over `feather` pixels (centred on the edge): a scrim, a veil, or a
    /// halo when the box is a disc.
    pub fn push_soft_box(&mut self, rect: PxRect, color: Color, radius: f32, feather: f32) {
        self.kinds.boxes += 1;
        self.instances.push(Draw2dInstance {
            geo: rect.corners(),
            uv: [0.0; 4],
            color: premul(color),
            clip: self.clip,
            params: [kind::BOX, radius.max(0.0), feather.max(0.0), 0.0],
        });
    }

    /// A box filled with a linear gradient from `from` to `to`: top to bottom, or left to right
    /// when `horizontal` (a strip or a bar fill). Colours blend premultiplied.
    pub fn push_gradient_box(&mut self, rect: PxRect, from: Color, to: Color, radius: f32, horizontal: bool) {
        self.kinds.boxes += 1;
        self.instances.push(Draw2dInstance {
            geo: rect.corners(),
            uv: premul(to),
            color: premul(from),
            clip: self.clip,
            params: [
                kind::BOX,
                radius.max(0.0),
                0.0,
                if horizontal { 2.0 } else { 1.0 },
            ],
        });
    }

    /// An anti-aliased segment from `a` to `b`, `width` pixels wide, with butt caps.
    pub fn push_segment(&mut self, a: Vec2, b: Vec2, width: f32, color: Color) {
        self.segment(a, b, width, color, false);
    }

    /// [`Self::push_segment`] with round caps.
    pub fn push_round_segment(&mut self, a: Vec2, b: Vec2, width: f32, color: Color) {
        self.segment(a, b, width, color, true);
    }

    fn segment(&mut self, a: Vec2, b: Vec2, width: f32, color: Color, round: bool) {
        self.kinds.segments += 1;
        self.instances.push(Draw2dInstance {
            geo: [a.x, a.y, b.x, b.y],
            uv: [0.0; 4],
            color: premul(color),
            clip: self.clip,
            params: [
                kind::SEGMENT,
                width.max(0.5) * 0.5,
                if round { 1.0 } else { 0.0 },
                0.0,
            ],
        });
    }

    /// The part `uv` (`u0 v0 u1 v1`, 0-1) of image `image` stretched over `rect`, multiplied by
    /// `tint` (white keeps the image's colours). An empty slot draws nothing.
    pub fn push_image(&mut self, image: Draw2dImage, rect: PxRect, uv: [f32; 4], tint: Color) {
        if usize::from(image.0) >= IMAGE_SLOTS {
            crate::warn_once!(
                "Draw2dImage({}) is outside the {IMAGE_SLOTS} image slots; not drawn",
                image.0
            );
            return;
        }
        self.kinds.images += 1;
        self.instances.push(Draw2dInstance {
            geo: rect.corners(),
            uv,
            color: premul(tint),
            clip: self.clip,
            params: [kind::IMAGE, 0.0, 0.0, f32::from(image.0)],
        });
    }

    /// The glyphs of a shaped run (from [`TextSystem::shape`], physical pixels) with the block's
    /// top-left at `origin` (rounded to whole pixels so glyphs stay sharp). Monochrome glyphs take
    /// `color`; colour glyphs keep theirs (times `color`'s alpha). A glyph seen before is looked
    /// up in the atlas; a new one is rasterized with `fonts` and its page uploaded once.
    pub fn push_text(&mut self, fonts: &mut TextSystem, run: &ShapedText, origin: Vec2, color: Color) {
        let ox = origin.x.round();
        let oy = origin.y.round();
        let page = PAGE_SIZE as f32;
        let tint = premul(color);
        for glyph in &run.glyphs {
            let key = glyph.cache_key;
            if self.blank.contains(&key) {
                continue;
            }
            let mut missed = false;
            let mut blank = false;
            let placed = self.atlas.get_or_insert(key, || {
                missed = true;
                let image = fonts.glyph_image(key);
                blank = image
                    .as_ref()
                    .is_none_or(|i| i.placement.width == 0 || i.placement.height == 0);
                image
            });
            if missed {
                self.rasterized += 1;
            }
            let Some((place, offset)) = placed else {
                if blank {
                    self.blank.insert(key);
                }
                continue;
            };
            let x0 = ox + (glyph.pen[0] + offset[0]) as f32;
            let y0 = oy + (glyph.pen[1] + offset[1]) as f32;
            let [px, py, pw, ph] = place.rect.map(|v| v as f32);
            let (k, c) = if place.kind == 1 {
                (kind::COLOR_GLYPH, [tint[3]; 4])
            } else {
                (kind::GLYPH, tint)
            };
            self.kinds.glyphs += 1;
            self.instances.push(Draw2dInstance {
                geo: [x0, y0, x0 + pw, y0 + ph],
                uv: [px / page, py / page, (px + pw) / page, (py + ph) / page],
                color: c,
                clip: self.clip,
                params: [k, 0.0, place.page as f32, 0.0],
            });
        }
    }

    /// Fills image slot `slot` (premultiplied sRGB RGBA8; an R8 image becomes a white mask). The
    /// layer uploads it before its next draw.
    pub fn set_image(&mut self, slot: Draw2dImage, image: ImmediateImage) {
        if usize::from(slot.0) >= IMAGE_SLOTS {
            crate::warn_once!("Draw2dImage({}) is outside the {IMAGE_SLOTS} image slots", slot.0);
            return;
        }
        self.images.retain(|(s, _)| *s != slot);
        self.images.push((slot, image));
    }

    /// The instances, in draw order.
    #[must_use]
    pub fn instances(&self) -> &[Draw2dInstance] {
        &self.instances
    }

    /// The glyph atlas the list's glyph instances sample.
    #[must_use]
    pub fn atlas(&self) -> &GlyphAtlases {
        &self.atlas
    }

    /// What the list holds.
    #[must_use]
    pub fn stats(&self) -> Draw2dListStats {
        Draw2dListStats {
            instances: u32::try_from(self.instances.len()).unwrap_or(u32::MAX),
            boxes: self.kinds.boxes,
            outlines: self.kinds.outlines,
            segments: self.kinds.segments,
            glyphs: self.kinds.glyphs,
            images: self.kinds.images,
            atlas_pages: self.atlas.page_count(0) + self.atlas.page_count(1),
            glyphs_cached: u32::try_from(self.atlas.len()).unwrap_or(u32::MAX),
            glyphs_rasterized: self.rasterized,
            glyphs_refused: self.atlas.thrash(),
            pages_staged: self.pages_staged,
        }
    }

    /// Copies the frame into `frame` for the layer: the instances, the atlas pages changed since
    /// the last hand-over and the images set since then (pages and images stay staged until the
    /// layer uploads them, so a frame the layer skips loses nothing). The buffers of `frame` are reused, so a
    /// steady frame (no new glyph, no new image) allocates nothing.
    pub fn hand_over(&mut self, frame: &mut Draw2dFrame) {
        frame.instances.clear();
        frame.instances.extend_from_slice(&self.instances);
        for &(kind, page) in &self.dirty {
            let Some((data, _)) = self.atlas.page_data(kind, page) else {
                continue;
            };
            // A page staged earlier and not drawn yet is replaced, not staged twice.
            let slot = frame.pages[..frame.pages_len]
                .iter()
                .position(|p| p.kind == kind && p.page == page)
                .unwrap_or(frame.pages_len);
            if slot == frame.pages_len {
                if frame.pages.len() == frame.pages_len {
                    frame.pages.push(StagedPage::default());
                }
                frame.pages_len += 1;
            }
            let staged = &mut frame.pages[slot];
            staged.kind = kind;
            staged.page = page;
            staged.data.clear();
            staged.data.extend_from_slice(data);
            self.pages_staged += 1;
        }
        self.dirty.clear();
        let wipes = self.atlas.wipes();
        if wipes != self.wipes_seen || !self.images.is_empty() {
            self.wipes_seen = wipes;
            frame.invalidate = true;
        }
        frame.images.append(&mut self.images);
    }
}

/// Atlas page count of each kind the layer allocates (`MAX_PAGES`).
pub const ATLAS_PAGES: u32 = MAX_PAGES;

// ---------------------------------------------------------------------------------------------
// ECS convenience.

/// The primary window's 2D draw list (an ECS resource). Systems in `Update` push into it through
/// `ResMut<Draw2d>` (it derefs to [`Draw2dList`]); [`Draw2dPlugin`] clears it in `PreUpdate`
/// and draws it as a window overlay layer.
#[derive(Resource, Default)]
pub struct Draw2d {
    list: Draw2dList,
}

impl Deref for Draw2d {
    type Target = Draw2dList;
    fn deref(&self) -> &Draw2dList {
        &self.list
    }
}

impl DerefMut for Draw2d {
    fn deref_mut(&mut self) -> &mut Draw2dList {
        &mut self.list
    }
}

/// What the draw list drew: the `quick.draw2d_stats` agent method and predicate resource.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct Draw2dStats {
    /// Frames extracted so far.
    pub frame: u64,
    /// The last extracted list.
    pub list: Draw2dListStats,
    /// The layer's GPU counters after its last draw (one frame behind the list).
    pub gpu: Draw2dGpuStats,
}

/// State shared by the extraction (main thread) and the overlay pass (render thread).
#[derive(Default)]
pub struct Draw2dShared {
    /// The staged frame.
    pub frame: Draw2dFrame,
    /// The GPU layer.
    pub layer: Draw2dLayer,
}

/// Shared handle to the overlay state (a resource; the overlay pass holds a clone).
#[derive(Resource, Clone, Default)]
pub struct Draw2dOverlay(pub Arc<Mutex<Draw2dShared>>);

impl Draw2dOverlay {
    /// Locks the state (a poisoned lock is recovered: it holds plain data and GPU handles).
    pub fn lock(&self) -> MutexGuard<'_, Draw2dShared> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The [`regia_render::OverlayDraw`] of [`Draw2dPlugin`].
struct Draw2dPass(Draw2dOverlay);

impl regia_render::OverlayDraw for Draw2dPass {
    fn draw(
        &mut self,
        device: &regia_render::RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let _span = tracing::info_span!(target: "regia_quick", "draw2d_overlay_record").entered();
        let mut shared = self.0.lock();
        let Draw2dShared { frame, layer } = &mut *shared;
        layer.draw_frame(device, encoder, view, format, width, height, frame);
    }

    fn warm(&mut self, device: &regia_render::RenderDevice, format: wgpu::TextureFormat) {
        self.0.lock().layer.warm(device, format);
    }
}

/// Order of the draw list among window overlays: above the quick layer, under the UI.
pub const DRAW2D_OVERLAY_ORDER: i32 = WindowOverlay::UI_ORDER - 5;

/// How [`Draw2dPlugin`]'s layer draws (a resource; insert your own before the plugin to change
/// it, it is read when the plugin finishes).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Draw2dSettings {
    /// Dirty regions: keep a retained UI target and redraw only the tiles whose instances
    /// changed ([`Draw2dTiles`]). On by default.
    pub dirty_regions: bool,
    /// Tile width in pixels ([`tiles::TILE_W`] by default).
    pub tile_w: u32,
    /// Tile height in pixels ([`tiles::TILE_H`] by default).
    pub tile_h: u32,
}

impl Default for Draw2dSettings {
    fn default() -> Self {
        Self {
            dirty_regions: true,
            tile_w: tiles::TILE_W,
            tile_h: tiles::TILE_H,
        }
    }
}

/// Installs [`Draw2d`], [`Draw2dStats`], [`Draw2dSettings`] and the overlay layer. Add after
/// `RenderPlugin` (without it the list is still built and counted, but nothing draws).
#[derive(Default)]
pub struct Draw2dPlugin;

impl Plugin for Draw2dPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Draw2d>()
            .init_resource::<Draw2dStats>()
            .init_resource::<Draw2dOverlay>()
            .init_resource::<Draw2dSettings>()
            .add_systems(PreUpdate, begin_draw2d)
            .add_systems(PostUpdate, extract_draw2d);
    }

    fn finish(&self, app: &mut App) {
        let handle = app.world().resource::<Draw2dOverlay>().clone();
        let settings = *app.world().resource::<Draw2dSettings>();
        {
            let mut shared = handle.lock();
            shared.layer.set_tile_size(settings.tile_w, settings.tile_h);
            shared.layer.set_dirty_regions(settings.dirty_regions);
        }
        if let Some(mut overlay) = app.world_mut().get_resource_mut::<WindowOverlay>() {
            overlay.insert(DRAW2D_OVERLAY_ORDER, "draw2d", Box::new(Draw2dPass(handle)));
        }
    }
}

fn begin_draw2d(mut draw: ResMut<Draw2d>) {
    draw.begin_frame();
}

fn extract_draw2d(mut draw: ResMut<Draw2d>, overlay: Res<Draw2dOverlay>, mut stats: ResMut<Draw2dStats>) {
    let _span = tracing::info_span!(target: "regia_quick", "draw2d_extract").entered();
    draw.end_frame();
    let mut shared = overlay.lock();
    draw.hand_over(&mut shared.frame);
    stats.frame += 1;
    stats.list = draw.stats();
    stats.gpu = shared.layer.gpu_stats();
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::Localization;
    use regia_ui::text::{Text, TextSpan, TextStyle};

    fn shaped(fonts: &mut TextSystem, s: &str, px: f32) -> ShapedText {
        let text = Text {
            spans: vec![TextSpan::lit(s).with_style(TextStyle {
                font_px: px,
                ..Default::default()
            })],
            ..Default::default()
        };
        fonts.shape(&text, 1.0, None, &Localization::default())
    }

    #[test]
    fn the_glyph_atlas_reuses_cached_glyphs() {
        let mut fonts = TextSystem::embedded_only();
        let run = shaped(&mut fonts, "FUEL 75", 18.0);
        let mut list = Draw2dList::new();
        let mut frame = Draw2dFrame::default();
        list.begin_frame();
        list.push_text(&mut fonts, &run, Vec2::new(10.0, 10.0), Color::WHITE);
        let n = list.instances().len();
        // The same run again, at another whole-pixel origin: the same cache keys (glyph, size,
        // sub-pixel bin), so the same atlas rects and no new rasterization.
        let after_first = list.stats();
        list.push_text(&mut fonts, &run, Vec2::new(10.0, 40.0), Color::WHITE);
        list.end_frame();
        list.hand_over(&mut frame);
        let first = list.stats();
        assert_eq!(n, 6, "six inked glyphs: {first:?}");
        assert_eq!(first.glyphs, 12, "{first:?}");
        assert_eq!(first.glyphs_cached, after_first.glyphs_cached, "{first:?}");
        assert_eq!(
            first.glyphs_rasterized, after_first.glyphs_rasterized,
            "{first:?}"
        );
        // Six inked glyphs (one atlas entry each, unless two share a key), plus the space when
        // the shaper emits one (rasterized once, then remembered as blank).
        assert!(first.glyphs_cached <= 6, "{first:?}");
        assert!(first.glyphs_rasterized <= 7, "{first:?}");
        let inst = list.instances();
        for (k, (a, b)) in inst[..n].iter().zip(&inst[n..]).enumerate() {
            assert_eq!(a.uv, b.uv, "glyph {k} samples the same atlas rect");
            assert_eq!(b.geo[1] - a.geo[1], 30.0);
        }
        assert_eq!(first.pages_staged, 1, "one page changed");
        assert_eq!(frame.pages_len, 1);

        for x in 0..3 {
            list.begin_frame();
            list.push_text(&mut fonts, &run, Vec2::new(20.0 + x as f32, 70.0), Color::WHITE);
            list.end_frame();
            list.hand_over(&mut frame);
            let again = list.stats();
            assert_eq!(again.glyphs, 6);
            assert_eq!(again.glyphs_cached, first.glyphs_cached, "frame {x}");
            assert_eq!(
                again.glyphs_rasterized, first.glyphs_rasterized,
                "nothing re-rasterized"
            );
            assert_eq!(again.pages_staged, 1, "no page is staged again");
            assert_eq!(frame.pages_len, 1, "the staged page waits for the layer");
        }
    }

    #[test]
    fn instances_carry_kind_clip_and_premultiplied_colour() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        list.set_clip(Some(PxRect::new(1.0, 2.0, 3.0, 4.0)));
        list.push_box(
            PxRect::new(0.0, 0.0, 10.0, 10.0),
            Color::new(1.0, 0.5, 0.0, 0.5),
            2.0,
        );
        list.set_clip(None);
        list.push_round_segment(Vec2::ZERO, Vec2::new(5.0, 0.0), 3.0, Color::WHITE);
        list.push_image(Draw2dImage(9), PxRect::default(), [0.0; 4], Color::WHITE);
        list.end_frame();
        let i = list.instances();
        assert_eq!(i.len(), 2, "an out-of-range image slot draws nothing");
        assert_eq!(i[0].color, [0.5, 0.25, 0.0, 0.5]);
        assert_eq!(i[0].clip, [1.0, 2.0, 4.0, 6.0]);
        assert_eq!(i[0].params, [kind::BOX, 2.0, 0.0, 0.0]);
        assert_eq!(i[1].clip, NO_CLIP);
        assert_eq!(i[1].params, [kind::SEGMENT, 1.5, 1.0, 0.0]);
        assert_eq!(
            std::mem::size_of::<Draw2dInstance>() as u64,
            Draw2dList::INSTANCE_BYTES
        );
        assert_eq!(Draw2dList::INSTANCE_BYTES, 80);
    }

    #[test]
    fn draw2d_shader_validates() {
        regia_render::validate_wgsl_modules(&[("draw2d.wgsl", SHADER)]).expect("draw2d.wgsl validates");
    }
}
