// SPDX-License-Identifier: MPL-2.0
//! GPU side of the UI overlay (`specs/18` § 3): pipelines per target format, glyph-atlas texture
//! arrays, registered image textures, the per-frame instance buffer, and the
//! [`regia_render::OverlayDraw`] implementation installed into
//! [`regia_render::WindowOverlay`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_render::RenderDevice;

use crate::atlas::{GlyphAtlases, MAX_PAGES, PAGE_SIZE};
use crate::draw::{BatchRange, QuadInstance, TextureRef, UiQuad, batch_ranges};

const SHADER: &str = include_str!("../shaders/ui-overlay.wgsl");

/// Overlay shader source (`ui-overlay.wgsl`), for `cargo xtask check-shaders`.
#[must_use]
pub fn overlay_shader_source() -> &'static str {
    SHADER
}

/// A shareable handle to the UI renderer (extract fills it; the overlay pass draws it).
pub type UiRendererHandle = Arc<Mutex<UiRenderer>>;

/// ECS resource holding the same handle the overlay draws from.
#[derive(Resource, Clone)]
pub struct SharedUiRenderer(pub UiRendererHandle);

impl SharedUiRenderer {
    /// Locks the renderer (a poisoned lock is a bug; draw an empty frame rather than panicking).
    pub fn lock(&self) -> std::sync::MutexGuard<'_, UiRenderer> {
        let mut guard = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.lock_acquires = guard.lock_acquires.saturating_add(1);
        guard
    }
}

/// How the overlay encodes its (display-referred, linear) colour for the target format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputEncoding {
    /// `*Srgb` formats: the hardware encodes on write and blends in linear light.
    HardwareSrgb,
    /// Other 8-bit UNORM formats: the shader sRGB-encodes (blending happens in sRGB space).
    ShaderSrgb,
    /// `Rgba16Float` (scRGB, 1.0 = 80 nits): SDR UI placed at paper white.
    ScRgb,
    /// `Rgb10a2Unorm` (HDR10, BT.2100 PQ): BT.2020 primaries, SDR UI at paper white.
    Pq,
}

impl OutputEncoding {
    /// The encoding a swapchain / target of `format` needs.
    pub fn for_format(format: wgpu::TextureFormat) -> Self {
        if format.is_srgb() {
            Self::HardwareSrgb
        } else {
            match format {
                wgpu::TextureFormat::Rgba16Float => Self::ScRgb,
                wgpu::TextureFormat::Rgb10a2Unorm => Self::Pq,
                _ => Self::ShaderSrgb,
            }
        }
    }

    /// The shader's encoding switch (`0` hardware sRGB, `1` shader sRGB, `2` scRGB, `3` PQ).
    pub(crate) fn mode(self) -> f32 {
        match self {
            Self::HardwareSrgb => 0.0,
            Self::ShaderSrgb => 1.0,
            Self::ScRgb => 2.0,
            Self::Pq => 3.0,
        }
    }
}

/// Look settings of the overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiRenderSettings {
    /// Blend gamma (`ui-overlay.wgsl::blend_alpha`). The overlay blends in linear light on
    /// sRGB targets; with `γ > 1` alpha is remapped so the result matches sRGB-space
    /// compositing — how CSS, design tools and native text rasterizers composite, and what
    /// authored `#rrggbbaa` colours expect. `1.0` = plain linear-light blending (dark
    /// translucent panels wash out over bright frames, dark text thins).
    pub blend_gamma: f32,
    /// SDR paper white in nits for HDR targets (ITU-R BT.2408 reference white: 203).
    pub paper_white_nits: f32,
}

impl Default for UiRenderSettings {
    fn default() -> Self {
        Self {
            blend_gamma: DEFAULT_BLEND_GAMMA,
            paper_white_nits: 203.0,
        }
    }
}

/// Default [`UiRenderSettings::blend_gamma`]: the sRGB curve's effective exponent.
pub const DEFAULT_BLEND_GAMMA: f32 = 2.2;

/// Bytes of the overlay's screen uniform block.
const SCREEN_UNIFORM_BYTES: u64 = 48;

/// One registered image texture.
struct GpuImage {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// One quad list and the GPU buffers it draws from: the window's list, or the list of one
/// camera or world-space target (ADR-2406). Each target has its own instance buffer and screen
/// uniforms, because queue writes all land before the frame's submit: a shared buffer would draw
/// every target with the last size written.
#[derive(Default)]
pub(crate) struct QuadBatch {
    pub(crate) quads: Vec<UiQuad>,
    /// True when `quads` changed since the instance buffer was last filled.
    quads_dirty: bool,
    /// Incremented every time a new quad list is installed.
    pub(crate) quads_generation: u64,
    /// Instances currently in `instance_buf` (draw order).
    instances: Vec<QuadInstance>,
    /// Scratch for the next instance list (diffed against `instances` before upload).
    next_instances: Vec<QuadInstance>,
    ranges: Vec<BatchRange>,
    /// Last screen uniforms written (skip the write when unchanged).
    screen_key: Option<[u32; 5]>,
    screen_bg: Option<wgpu::BindGroup>,
    screen_buf: Option<wgpu::Buffer>,
    instance_buf: Option<wgpu::Buffer>,
    instance_cap: u64,
    /// Scratch for the draw's span list (kept to avoid a per-frame allocation).
    spans: Vec<Span>,
}

/// One step of a draw: a run of ordinary instances, or one special instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Span {
    /// Instances `first..end` with one image binding.
    Draw {
        image: Option<u64>,
        first: u32,
        end: u32,
    },
    /// A backdrop-blur quad (kind 6).
    Backdrop(u32),
    /// A group layer begin marker (kind 11).
    Begin(u32),
    /// A group layer mask (kind 10).
    Mask(u32),
    /// A group layer composite (kind 12).
    Composite(u32),
}

/// The instance kinds that are not drawn in an ordinary run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Special {
    Backdrop,
    Mask,
    Begin,
    Composite,
}

fn special_kind(kind: f32) -> Option<Special> {
    match kind as u32 {
        6 => Some(Special::Backdrop),
        10 => Some(Special::Mask),
        11 => Some(Special::Begin),
        12 => Some(Special::Composite),
        _ => None,
    }
}

/// Destination-in: the layer keeps `src.a` of itself (the mask pass, ADR-2866).
const MASK_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::SrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::SrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

impl QuadBatch {
    /// Installs a new quad list.
    pub(crate) fn set_quads(&mut self, quads: Vec<UiQuad>) {
        self.quads = quads;
        self.quads_dirty = true;
        self.quads_generation += 1;
    }
}

/// All GPU state of the UI overlay. Created per app; device-dependent state initialises lazily on
/// the first [`UiRenderer::draw`].
#[derive(Default)]
pub struct UiRenderer {
    /// Look settings (blend gamma, HDR paper white).
    pub settings: UiRenderSettings,
    /// CPU-side glyph atlas (placements + page pixels); rasterized during extract, uploaded
    /// during draw.
    pub(crate) atlas: GlyphAtlases,
    /// The window's quad list and its GPU buffers.
    main: QuadBatch,
    /// Quad lists of camera-target and world-space roots (ADR-2406), keyed by target.
    pub(crate) targets:
        std::collections::BTreeMap<crate::world_target::UiTargetKey, crate::world_target::TargetSurface>,
    images: HashMap<u64, GpuImage>,
    /// Pixel size of every registered or queued image (9-slice insets are in source pixels).
    image_sizes: HashMap<u64, [u32; 2]>,
    /// Incremented on every image registration (extract re-splits 9-slices when sizes change).
    images_generation: u64,
    /// Images queued from the ECS side, uploaded on the next draw.
    pending_images: Vec<(u64, u32, u32, Vec<u8>)>,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    shader: Option<wgpu::ShaderModule>,
    layout: Option<wgpu::PipelineLayout>,
    image_bgl: Option<wgpu::BindGroupLayout>,
    mono_bg: Option<wgpu::BindGroup>,
    color_bg: Option<wgpu::BindGroup>,
    empty_image_bg: Option<wgpu::BindGroup>,
    sampler: Option<wgpu::Sampler>,
    screen_bgl: Option<wgpu::BindGroupLayout>,
    mono_texture: Option<wgpu::Texture>,
    color_texture: Option<wgpu::Texture>,
    /// Backdrop-blur passes (ADR-1836), created by the boot warm ([`Self::warm`], ADR-2585) or
    /// on the first frame that has a backdrop quad.
    backdrop: Option<crate::backdrop::BackdropGpu>,
    /// What the backdrop blur did in the last draw.
    backdrop_stats: crate::backdrop::BackdropStats,
    /// Pooled offscreen layers of isolated groups and masks (ADR-2866).
    layers: crate::group_layer::LayerPool,
    /// What the layers did in the last window draw.
    layer_stats: crate::group_layer::LayerStats,
    /// The destination-in pipeline of layer masks.
    mask_pipeline: Option<wgpu::RenderPipeline>,
    /// CPU alpha copies of registered images, for mask hit testing.
    mask_alpha: HashMap<u64, crate::group_layer::AlphaMap>,
    /// What extract decided for the groups of the current lists (set with the quads).
    pub(crate) layer_plan: crate::group_layer::LayerPlan,
    /// Ids on the dynamic (same-size rewrite) path.
    dynamic_ids: HashSet<u64>,
    /// Dynamic pictures waiting for their first GPU texture (one mip, no pyramid).
    pending_dynamic: Vec<(u64, u32, u32, Arc<[u8]>)>,
    /// Mip-0 rewrites of a dynamic image whose GPU texture already exists.
    pending_level0: Vec<(u64, u32, u32, Arc<[u8]>)>,
    /// Scratch for premultiplied mip 0 (not the caller's bytes).
    level0_scratch: Vec<u8>,
    /// GPU image textures created. Static [`Self::queue_image_rgba`] counts one per call;
    /// [`Self::queue_dynamic_image`] counts one per id until the size changes.
    image_texture_creates: u64,
    /// Mip-0 rewrites of an existing dynamic image.
    image_level0_writes: u64,
    /// Bytes handed to the static queue as an owned `Vec` (the copy a dynamic picture skips).
    image_byte_copies: u64,
    /// Times [`SharedUiRenderer::lock`] took the mutex.
    lock_acquires: u64,
    /// Top-level subtrees walked because the extract cache missed.
    subtree_walks: u64,
}

impl UiRenderer {
    /// A fresh renderer handle.
    pub fn new_handle() -> UiRendererHandle {
        Arc::new(Mutex::new(Self::default()))
    }

    /// Replaces the quad list (called by extract when the UI changed). The list persists and is
    /// redrawn every frame until replaced; the instance buffer is rebuilt only after a change.
    pub fn set_quads(&mut self, quads: Vec<UiQuad>) {
        self.main.set_quads(quads);
    }

    /// How many quad lists have been installed (damage-tracking diagnostics and tests).
    pub fn quads_generation(&self) -> u64 {
        self.main.quads_generation
    }

    /// Takes the current quad list's allocation for reuse (extract refills and hands it back via
    /// [`Self::set_quads`]).
    pub fn take_quads(&mut self) -> Vec<UiQuad> {
        self.main.quads_dirty = true;
        std::mem::take(&mut self.main.quads)
    }

    /// Draw calls the current quad list needs (after the next draw).
    pub fn batch_count(&self) -> usize {
        self.main.ranges.len()
    }

    /// The frame's quad list.
    pub fn quads(&self) -> &[UiQuad] {
        &self.main.quads
    }

    /// The CPU-side glyph atlas (for extract's rasterization step).
    pub fn atlas_mut(&mut self) -> &mut GlyphAtlases {
        &mut self.atlas
    }

    /// Queues an RGBA8 image (sRGB-encoded, straight alpha, tightly packed) under `id` for upload
    /// on the next draw — for code that has no [`RenderDevice`] at hand (ECS systems). Replaces
    /// any image already registered under `id`.
    pub fn queue_image_rgba(&mut self, id: u64, width: u32, height: u32, data: Vec<u8>) {
        self.image_texture_creates = self.image_texture_creates.saturating_add(1);
        self.image_byte_copies = self
            .image_byte_copies
            .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        self.dynamic_ids.remove(&id);
        self.pending_dynamic.retain(|p| p.0 != id);
        self.pending_level0.retain(|p| p.0 != id);
        self.record_mask_alpha(id, width, height, &data);
        self.image_sizes.insert(id, [width.max(1), height.max(1)]);
        self.images_generation += 1;
        self.pending_images.retain(|p| p.0 != id);
        self.pending_images.push((id, width, height, data));
    }

    /// Queues an RGBA8 picture that keeps one GPU texture while the size stays the same.
    ///
    /// The first picture for `id`, and any later picture whose size changed, creates one texture
    /// (mip 0 only, premultiplied, no mip pyramid) and bumps [`Self::images_generation`]. A later
    /// picture of the same size writes mip 0 of that texture and does not bump the generation, so
    /// a playing video does not rebuild the UI. `data` is straight-alpha sRGB, tightly packed.
    /// The bytes are not copied into a `Vec`. Mask alpha is recorded only when the texture is created.
    pub fn queue_dynamic_image(&mut self, id: u64, width: u32, height: u32, data: Arc<[u8]>) {
        let (w, h) = (width.max(1), height.max(1));
        let size = [w, h];
        let same = self.dynamic_ids.contains(&id) && self.image_sizes.get(&id) == Some(&size);
        if same {
            self.image_level0_writes = self.image_level0_writes.saturating_add(1);
            if let Some(slot) = self.pending_dynamic.iter_mut().find(|p| p.0 == id) {
                slot.1 = w;
                slot.2 = h;
                slot.3 = data;
                return;
            }
            self.pending_level0.retain(|p| p.0 != id);
            self.pending_level0.push((id, w, h, data));
            return;
        }
        self.image_texture_creates = self.image_texture_creates.saturating_add(1);
        if self.image_sizes.insert(id, size) != Some(size) {
            self.images_generation = self.images_generation.saturating_add(1);
        }
        self.record_mask_alpha(id, w, h, data.as_ref());
        self.dynamic_ids.insert(id);
        self.pending_images.retain(|p| p.0 != id);
        self.pending_dynamic.retain(|p| p.0 != id);
        self.pending_level0.retain(|p| p.0 != id);
        self.pending_dynamic.push((id, w, h, data));
    }

    /// Keeps a small CPU copy of an image's alpha for mask hit testing ([`crate::UiMask`]).
    fn record_mask_alpha(&mut self, id: u64, width: u32, height: u32, data: &[u8]) {
        match crate::group_layer::AlphaMap::from_rgba(width, height, data) {
            Some(map) => {
                self.mask_alpha.insert(id, map);
            }
            None => {
                self.mask_alpha.remove(&id);
            }
        }
    }

    /// Alpha (0..=1) of image `id` at `uv`, from its CPU copy; `None` for images registered as
    /// GPU views (no copy) or unknown ids.
    #[must_use]
    pub fn image_alpha(&self, id: u64, uv: [f32; 2]) -> Option<f32> {
        self.mask_alpha.get(&id).map(|m| m.sample(uv))
    }

    /// How many image registrations happened so far (draw-list damage for 9-slice splits).
    pub fn images_generation(&self) -> u64 {
        self.images_generation
    }

    /// GPU image textures created so far (one per static queue; one per dynamic id until the size changes).
    #[must_use]
    pub fn image_texture_creates(&self) -> u64 {
        self.image_texture_creates
    }

    /// Mip-0 rewrites of a dynamic image whose size did not change.
    #[must_use]
    pub fn image_level0_writes(&self) -> u64 {
        self.image_level0_writes
    }

    /// Bytes copied into an owned buffer on the static image queue.
    #[must_use]
    pub fn image_byte_copies(&self) -> u64 {
        self.image_byte_copies
    }

    /// Times [`SharedUiRenderer::lock`] took the mutex.
    #[must_use]
    pub fn lock_acquires(&self) -> u64 {
        self.lock_acquires
    }

    /// Top-level subtrees walked because the extract cache missed.
    #[must_use]
    pub fn subtree_walks(&self) -> u64 {
        self.subtree_walks
    }

    /// Counts one cache-miss walk of a top-level subtree.
    pub(crate) fn note_subtree_walk(&mut self) {
        self.subtree_walks = self.subtree_walks.saturating_add(1);
    }

    /// A CPU alpha copy for mask hit testing, or `None` when the image has no copy.
    pub(crate) fn copy_mask_alpha(&self, id: u64) -> Option<crate::group_layer::AlphaMap> {
        self.mask_alpha.get(&id).cloned()
    }

    /// Adds `dt` seconds to every animated material quad already in a draw list.
    /// Does not bump [`Self::quads_generation`] (the window list); a patched offscreen target's
    /// own generation moves, because a world panel only redraws its texture on a new one.
    pub(crate) fn patch_material_angles(&mut self, dt: f32) -> bool {
        if dt == 0.0 {
            return false;
        }
        let mut changed = Self::patch_material_quads(&mut self.main.quads, dt);
        if changed {
            self.main.quads_dirty = true;
        }
        for surface in self.targets.values_mut() {
            if Self::patch_material_quads(&mut surface.batch.quads, dt) {
                surface.batch.quads_dirty = true;
                surface.batch.quads_generation += 1;
                changed = true;
            }
        }
        changed
    }

    /// Adds `dt` to material quads in `quads` whose kind uses time (shine, scanlines, pulse).
    pub(crate) fn patch_material_quads(quads: &mut [UiQuad], dt: f32) -> bool {
        let wrap = crate::ui_material::MATERIAL_TIME_WRAP;
        let mut changed = false;
        for quad in quads.iter_mut() {
            if quad.texture != TextureRef::Material {
                continue;
            }
            let kind = quad.blur as u32;
            if !matches!(kind, 0 | 3 | 4) {
                continue;
            }
            let next = (quad.angle + dt) % wrap;
            if next != quad.angle {
                quad.angle = next;
                changed = true;
            }
        }
        changed
    }

    /// Replaces quads whose position and size match `old` with `fresh`, in every draw list.
    /// Does not bump [`Self::quads_generation`] (the window list); a patched offscreen target's
    /// own generation moves, so a world panel redraws its texture.
    pub(crate) fn replace_quads_matching(&mut self, old: &[([f32; 2], [f32; 2])], fresh: &[UiQuad]) -> bool {
        let mut changed = false;
        if replace_matching(&mut self.main.quads, old, fresh) {
            self.main.quads_dirty = true;
            changed = true;
        }
        for surface in self.targets.values_mut() {
            if replace_matching(&mut surface.batch.quads, old, fresh) {
                surface.batch.quads_dirty = true;
                surface.batch.quads_generation += 1;
                changed = true;
            }
        }
        changed
    }

    /// Replaces the last `tail` window quads with `overlay` when they differ.
    /// Does not bump [`Self::quads_generation`].
    pub(crate) fn replace_main_tail(&mut self, tail: usize, overlay: &[UiQuad]) -> bool {
        let quads = &mut self.main.quads;
        let keep = quads.len().saturating_sub(tail);
        if quads[keep..] == *overlay {
            return false;
        }
        quads.truncate(keep);
        quads.extend_from_slice(overlay);
        self.main.quads_dirty = true;
        true
    }

    /// Pixel size of a registered or queued image.
    pub fn image_size(&self, id: u64) -> Option<[u32; 2]> {
        self.image_sizes.get(&id).copied()
    }

    /// Shows an existing GPU texture under image `id`, with no copy: a render-to-texture
    /// camera's image ([`regia_render::FrameRenderer::texture_target_view`]) as a HUD
    /// screen or a map panel. The texture must be a filterable 2-D colour texture; its
    /// contents are read when the UI draws (after the views and the window are rendered, so
    /// the panel shows this frame's image). Register again if the texture is replaced (a view
    /// whose size changed).
    pub fn add_image_view(&mut self, device: &RenderDevice, id: u64, view: &wgpu::TextureView) {
        self.ensure_gpu(device);
        let size = view.texture().size();
        if self
            .image_sizes
            .insert(id, [size.width.max(1), size.height.max(1)])
            != Some([size.width.max(1), size.height.max(1)])
        {
            self.images_generation += 1;
        }
        self.pending_images.retain(|p| p.0 != id);
        // A GPU view has no CPU pixels: mask hits over it count as opaque.
        self.mask_alpha.remove(&id);
        let (Some(bgl), Some(sampler)) = (self.image_bgl.clone(), self.sampler.clone()) else {
            return;
        };
        // Counted: the hitch accounting sees UI creates too.
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let bind_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui image view bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
            ],
        });
        self.images.insert(
            id,
            GpuImage {
                texture: view.texture().clone(),
                bind_group,
            },
        );
    }

    /// Registers an RGBA8 image under `id` (call before the first frame that uses it). Row data
    /// is sRGB-encoded RGBA, tightly packed.
    pub fn add_image_rgba(&mut self, device: &RenderDevice, id: u64, width: u32, height: u32, data: &[u8]) {
        self.ensure_gpu(device);
        self.record_mask_alpha(id, width, height, data);
        if self.image_sizes.insert(id, [width.max(1), height.max(1)]) != Some([width.max(1), height.max(1)]) {
            self.images_generation += 1;
        }
        let (Some(bgl), Some(sampler)) = (self.image_bgl.clone(), self.sampler.clone()) else {
            return;
        };
        // Counted (pipelines, bind groups): the hitch accounting sees UI creates too.
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let (w, h) = (width.max(1), height.max(1));
        let Some(levels) = image_mip_chain(w, h, data) else {
            tracing::warn!(
                target: "regia_ui",
                "ui image {id}: {} bytes for {w}x{h} RGBA8; not registered",
                data.len()
            );
            return;
        };
        let texture = dev.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui image"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: u32::try_from(levels.len()).unwrap_or(1),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, (lw, lh, pixels)) in levels.iter().enumerate() {
            device.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: u32::try_from(level).unwrap_or(0),
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(lw * 4),
                    rows_per_image: Some(*lh),
                },
                wgpu::Extent3d {
                    width: *lw,
                    height: *lh,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui image bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        self.images.insert(id, GpuImage { texture, bind_group });
    }

    /// Creates one mip-0 texture for a dynamic image. The create was already counted at queue time,
    /// and the generation bump already happened there, so this does neither.
    fn add_dynamic_image(&mut self, device: &RenderDevice, id: u64, width: u32, height: u32, data: &[u8]) {
        self.ensure_gpu(device);
        let (Some(bgl), Some(sampler)) = (self.image_bgl.clone(), self.sampler.clone()) else {
            return;
        };
        let (w, h) = (width.max(1), height.max(1));
        if !premultiply_level0(w, h, data, &mut self.level0_scratch) {
            tracing::warn!(
                target: "regia_ui",
                "ui dynamic image {id}: {} bytes for {w}x{h} RGBA8; not registered",
                data.len()
            );
            return;
        }
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let texture = dev.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui dynamic image"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_level0(device, &texture, w, h, &self.level0_scratch);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui dynamic image bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        self.images.insert(id, GpuImage { texture, bind_group });
    }

    /// Writes mip 0 of an existing dynamic image. `false` when that texture does not exist yet or
    /// has another size (the caller then creates it), or the byte length does not match.
    fn write_dynamic_level0(
        &mut self,
        device: &RenderDevice,
        id: u64,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> bool {
        let Some(texture) = self.images.get(&id).map(|image| image.texture.clone()) else {
            return false;
        };
        let (w, h) = (width.max(1), height.max(1));
        // The queue classes a picture "same size" by the size it recorded, which moves at queue
        // time; a picture at a new size whose bytes were refused never replaced the texture. A
        // write larger than the texture is a wgpu validation error, which the engine makes fatal.
        if texture.width() != w || texture.height() != h || texture.mip_level_count() != 1 {
            return false;
        }
        if !premultiply_level0(w, h, data, &mut self.level0_scratch) {
            return false;
        }
        write_level0(device, &texture, w, h, &self.level0_scratch);
        true
    }

    fn ensure_gpu(&mut self, device: &RenderDevice) {
        if self.layout.is_some() {
            return;
        }
        // Counted (pipelines, bind groups): the hitch accounting sees UI creates too.
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui overlay"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let screen_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui screen bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let atlas_bgl = |label: &str| {
            dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            })
        };
        let mono_bgl = atlas_bgl("ui mono atlas bgl");
        let color_bgl = atlas_bgl("ui color atlas bgl");
        let image_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui image bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui overlay pl"),
            bind_group_layouts: &[
                Some(&screen_bgl),
                Some(&mono_bgl),
                Some(&color_bgl),
                Some(&image_bgl),
            ],
            immediate_size: 0,
        });

        let sampler = dev.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ui sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        // Placeholder 1×1 white image for batches with no image bound.
        let dummy = dev.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui dummy image"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &dummy,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let dummy_view = dummy.create_view(&wgpu::TextureViewDescriptor::default());
        self.empty_image_bg = Some(dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui dummy bg"),
            layout: &image_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dummy_view),
                },
            ],
        }));
        self.sampler = Some(sampler);

        // Atlas bind groups; the textures themselves are created on first atlas use.
        let mono_texture = self.ensure_atlas_texture(device, wgpu::TextureFormat::R8Unorm);
        let color_texture = self.ensure_atlas_texture(device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let make_bg =
            |bgl: &wgpu::BindGroupLayout, texture: &wgpu::Texture, sampler: &wgpu::Sampler, label: &str| {
                let view = texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some(label),
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    array_layer_count: Some(MAX_PAGES),
                    ..Default::default()
                });
                dev.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(label),
                    layout: bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                    ],
                })
            };
        if let Some(sampler) = &self.sampler {
            self.mono_bg = Some(make_bg(&mono_bgl, &mono_texture, sampler, "ui mono atlas bg"));
            self.color_bg = Some(make_bg(&color_bgl, &color_texture, sampler, "ui color atlas bg"));
        }
        self.mono_texture = Some(mono_texture);
        self.color_texture = Some(color_texture);

        self.layout = Some(layout);
        self.screen_bgl = Some(screen_bgl);
        self.image_bgl = Some(image_bgl);
        self.shader = Some(shader);
        self.pipelines.clear();
        // The window's uniforms exist from the warm on, as before targets had their own.
        let mut main = std::mem::take(&mut self.main);
        self.ensure_screen(device, &mut main);
        self.main = main;
    }

    fn ensure_atlas_texture(&self, device: &RenderDevice, format: wgpu::TextureFormat) -> wgpu::Texture {
        device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui glyph atlas"),
            size: wgpu::Extent3d {
                width: PAGE_SIZE,
                height: PAGE_SIZE,
                depth_or_array_layers: MAX_PAGES,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// Creates the overlay's GPU state and its pipeline for `format` now, so the first frame
    /// that shows UI (a HUD toggled on, the performance overlay) compiles nothing. Returns true
    /// when this call created the pipeline. [`Self::draw`] also warms every format it is asked to
    /// draw into, even with nothing to draw, so a window's format is warm from its first frame.
    /// The backdrop blur's pipelines for `format` are built too (ADR-2585), so the first blurred
    /// panel compiles nothing either, and so are the group layer pipelines (ADR-2866).
    pub fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) -> bool {
        self.ensure_gpu(device);
        self.upload_pending_images(device);
        let had = self.pipelines.contains_key(&format);
        let created = self.pipeline_for(device, format).is_some() && !had;
        self.ensure_backdrop(device);
        if let Some(backdrop) = self.backdrop.as_mut() {
            backdrop.warm(device, format);
        }
        let _ = self.layer_pipelines(device);
        created
    }

    /// The group layers' half of [`Self::warm_inputs`]: what compiling the missing layer pipelines
    /// needs, and which are missing (normal, mask). `None` when both exist.
    fn layer_warm_inputs(&self) -> Option<(wgpu::ShaderModule, wgpu::PipelineLayout, bool, bool)> {
        let normal = !self.pipelines.contains_key(&crate::group_layer::LAYER_FORMAT);
        let mask = self.mask_pipeline.is_none();
        if !normal && !mask {
            return None;
        }
        Some((self.shader.clone()?, self.layout.clone()?, normal, mask))
    }

    /// The backdrop blur's half of [`Self::warm_inputs`]: creates the blur state and returns what
    /// its missing pipelines for `format` need, or `None` when they exist.
    fn backdrop_warm_inputs(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
    ) -> Option<(wgpu::ShaderModule, wgpu::PipelineLayout, Vec<&'static str>)> {
        self.ensure_gpu(device);
        self.ensure_backdrop(device);
        self.backdrop.as_ref()?.warm_inputs(format)
    }

    /// First half of a warm that keeps the lock short: creates the GPU state and returns what the
    /// pipeline compile needs, or `None` when `format` is already warm. Pair with
    /// [`Self::install_pipeline`].
    fn warm_inputs(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
    ) -> Option<(wgpu::ShaderModule, wgpu::PipelineLayout)> {
        self.ensure_gpu(device);
        self.upload_pending_images(device);
        if self.pipelines.contains_key(&format) {
            return None;
        }
        Some((self.shader.clone()?, self.layout.clone()?))
    }

    /// Second half of a warm: stores a pipeline compiled by [`Self::build_pipeline`] unless a
    /// concurrent draw already created one for `format`.
    fn install_pipeline(&mut self, format: wgpu::TextureFormat, pipeline: wgpu::RenderPipeline) {
        self.pipelines.entry(format).or_insert(pipeline);
    }

    /// Whether the pipeline for `format` exists ([`Self::warm`]).
    #[must_use]
    pub fn is_warm(&self, format: wgpu::TextureFormat) -> bool {
        self.pipelines.contains_key(&format)
    }

    fn pipeline_for(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
    ) -> Option<wgpu::RenderPipeline> {
        if let Some(p) = self.pipelines.get(&format) {
            return Some(p.clone());
        }
        let shader = self.shader.clone()?;
        let layout = self.layout.clone()?;
        let pipeline = Self::build_pipeline(device, &shader, &layout, format);
        self.pipelines.insert(format, pipeline.clone());
        Some(pipeline)
    }

    /// Compiles the overlay pipeline for `format`. Touches no renderer state, so a caller can run
    /// it without holding the renderer's lock.
    fn build_pipeline(
        device: &RenderDevice,
        shader: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        // Premultiplied: correct filtering of translucent images and glyph edges, and
        // destination alpha stays 1 over an opaque frame.
        Self::build_pipeline_blend(
            device,
            shader,
            layout,
            format,
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            "ui overlay pipeline",
        )
    }

    /// [`Self::build_pipeline`] with an explicit blend state and label.
    fn build_pipeline_blend(
        device: &RenderDevice,
        shader: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
        blend: wgpu::BlendState,
        label: &str,
    ) -> wgpu::RenderPipeline {
        regia_render::TrackedDevice::observe(&device.device).create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: u64::from(QuadInstance::SIZE),
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: 8,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 16,
                                shader_location: 2,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 32,
                                shader_location: 3,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 48,
                                shader_location: 4,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 64,
                                shader_location: 5,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 80,
                                shader_location: 6,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 96,
                                shader_location: 7,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 112,
                                shader_location: 8,
                            },
                        ],
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&device.device).as_deref(),
            },
        )
    }

    /// Uploads dirty atlas pages into the texture arrays.
    fn upload_atlases(&mut self, device: &RenderDevice) {
        for (kind, page) in self.atlas.take_dirty_pages() {
            let Some((data, bpp)) = self.atlas.page_data(kind, page) else {
                continue;
            };
            let texture = if kind == 1 {
                &self.color_texture
            } else {
                &self.mono_texture
            };
            let Some(texture) = texture else { continue };
            let bytes_per_row = PAGE_SIZE * u32::try_from(bpp).unwrap_or(1);
            device.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: page },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(PAGE_SIZE),
                },
                wgpu::Extent3d {
                    width: PAGE_SIZE,
                    height: PAGE_SIZE,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Draws the extracted quad list over `view` (`format`, `width`×`height`). Backdrop-blur
    /// quads need the target texture ([`Self::draw_with_target`]); here they draw nothing.
    pub fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        self.draw_with_target(device, encoder, view, None, format, width, height);
    }

    /// [`Self::draw`] with the texture behind `view`, which backdrop-blur quads read (it needs
    /// `COPY_SRC` usage; without it the blur is skipped and only the quad's tint shows).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_with_target(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        target: Option<&wgpu::Texture>,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let _span = tracing::trace_span!("ui_draw").entered();
        // Before the window's batch is taken out: the first draw creates the window's screen
        // uniforms on the batch itself (an empty HUD at boot), so UI shown later creates no bind
        // group on the frame it appears.
        self.ensure_gpu(device);
        let mut batch = std::mem::take(&mut self.main);
        self.draw_batch(
            &mut batch, device, encoder, view, target, format, width, height, true,
        );
        self.main = batch;
        if let Some(backdrop) = self.backdrop.as_mut() {
            backdrop.end_frame();
        }
    }

    /// Draws `batch` over `view` with the shared pipelines, atlases and images. `main` = the
    /// window's list (only its draws publish [`Self::backdrop_stats`]).
    #[allow(clippy::too_many_arguments)] // ADR-2406: the draw's inputs plus the batch it draws
    pub(crate) fn draw_batch(
        &mut self,
        batch: &mut QuadBatch,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        target: Option<&wgpu::Texture>,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        main: bool,
    ) {
        self.ensure_gpu(device);
        self.upload_pending_images(device);
        self.upload_atlases(device);
        // Before the empty check: the first frame a target format is drawn into compiles the
        // pipeline (boot, inside the warm-up), so UI shown later (a HUD toggled on, the
        // performance overlay) creates nothing on the frame it appears.
        let pipeline = self.pipeline_for(device, format);
        if batch.quads.is_empty() || width == 0 || height == 0 {
            // An empty window list is a draw without backdrop quads too: without this the stats
            // kept the last frosted panel after it closed over a bare scene (ADR-5420).
            if main && batch.quads.is_empty() {
                self.backdrop_stats = crate::backdrop::BackdropStats::default();
            }
            return;
        }
        let Some(pipeline) = pipeline else {
            return;
        };
        let encoding = OutputEncoding::for_format(format);
        // Linear-light targets get the blend remap; shader-sRGB and PQ targets already blend in
        // encoded space.
        let blend_gamma = match encoding {
            OutputEncoding::HardwareSrgb | OutputEncoding::ScRgb => self.settings.blend_gamma,
            OutputEncoding::ShaderSrgb | OutputEncoding::Pq => 1.0,
        };
        let key = [
            width,
            height,
            encoding.mode().to_bits(),
            self.settings.paper_white_nits.to_bits(),
            blend_gamma.to_bits(),
        ];
        self.ensure_screen(device, batch);
        if batch.screen_key != Some(key)
            && let Some(buf) = &batch.screen_buf
        {
            let mut uniforms = [0.0f32; (SCREEN_UNIFORM_BYTES / 4) as usize];
            uniforms[..5].copy_from_slice(&[
                width as f32,
                height as f32,
                encoding.mode(),
                self.settings.paper_white_nits,
                blend_gamma,
            ]);
            device.queue.write_buffer(buf, 0, bytemuck::cast_slice(&uniforms));
            batch.screen_key = Some(key);
        }

        if batch.quads_dirty || batch.instance_buf.is_none() {
            let _span = tracing::trace_span!("ui_upload_instances").entered();
            batch_ranges(&batch.quads, &mut batch.next_instances, &mut batch.ranges);
            let bytes = (batch.next_instances.len().max(1) as u64) * u64::from(QuadInstance::SIZE);
            let mut full = false;
            if batch.instance_buf.is_none() || batch.instance_cap < bytes {
                // Grow geometrically so a slowly growing HUD does not reallocate every change.
                let cap = bytes
                    .max(batch.instance_cap * 2)
                    .max(64 * u64::from(QuadInstance::SIZE));
                batch.instance_buf = Some(device.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("ui instances"),
                    size: cap,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
                batch.instance_cap = cap;
                full = true;
            }
            // Upload only the span that differs from what the buffer already holds (a HUD whose
            // timer ticks or one bar moves rewrites a few instances, not the whole list).
            let (new, old) = (&batch.next_instances, &batch.instances);
            let span = if full {
                Some((0, new.len()))
            } else {
                let first = new.iter().zip(old.iter()).position(|(a, b)| a != b);
                let first = first.unwrap_or(new.len().min(old.len()));
                if first == new.len() {
                    None
                } else {
                    let tail = new
                        .iter()
                        .rev()
                        .zip(old.iter().rev())
                        .take(new.len() - first)
                        .take_while(|(a, b)| a == b)
                        .count();
                    // Equal tails only align when the lengths match.
                    let end = if new.len() == old.len() {
                        new.len() - tail
                    } else {
                        new.len()
                    };
                    Some((first, end))
                }
            };
            if let (Some((start, end)), Some(buf)) = (span, &batch.instance_buf)
                && end > start
            {
                device.queue.write_buffer(
                    buf,
                    start as u64 * u64::from(QuadInstance::SIZE),
                    bytemuck::cast_slice(&new[start..end]),
                );
            }
            std::mem::swap(&mut batch.instances, &mut batch.next_instances);
            batch.quads_dirty = false;
        }
        let Some(instance_buf) = batch.instance_buf.clone() else {
            return;
        };

        // Backdrop quads (kind 6) split the draw: the frame under them is blurred between two
        // passes. They need a readable target; without one they are skipped (the tint remains).
        // Group layers (kinds 10 to 12, ADR-2866) split it too: a begin marker opens a pass on a
        // pooled layer, the mask multiplies it, the composite blends it into the parent.
        let backdrop_quads = batch.instances.iter().filter(|i| i.kind_layer[0] == 6.0).count() as u32;
        let has_backdrop = backdrop_quads > 0;
        let has_layers = batch.instances.iter().any(|i| i.kind_layer[0] == 11.0);
        let mut stats = crate::backdrop::BackdropStats {
            quads: backdrop_quads,
            ..Default::default()
        };
        let mut layer_stats = crate::group_layer::LayerStats::default();
        let mut backdrop = if has_backdrop && target.is_some() {
            self.ensure_backdrop(device);
            let mut b = self.backdrop.take();
            if let Some(b) = b.as_mut() {
                b.begin_frame();
            }
            b
        } else {
            None
        };
        let layer_pipelines = if has_layers {
            self.layer_pipelines(device)
        } else {
            None
        };
        // Spans of instances in draw order: image runs and single special quads.
        let mut spans = std::mem::take(&mut batch.spans);
        spans.clear();
        for r in &batch.ranges {
            let end = r.first + r.count;
            let mut at = r.first;
            while at < end {
                let special_at = batch.instances[at as usize..end as usize]
                    .iter()
                    .position(|inst| special_kind(inst.kind_layer[0]).is_some())
                    .map(|p| at + p as u32);
                let run_end = special_at.unwrap_or(end);
                if run_end > at {
                    spans.push(Span::Draw {
                        image: r.image,
                        first: at,
                        end: run_end,
                    });
                }
                match special_at {
                    Some(b) => {
                        let kind = special_kind(batch.instances[b as usize].kind_layer[0]);
                        spans.push(match kind {
                            Some(Special::Begin) => Span::Begin(b),
                            Some(Special::Mask) => Span::Mask(b),
                            Some(Special::Composite) => Span::Composite(b),
                            _ => Span::Backdrop(b),
                        });
                        at = b + 1;
                    }
                    None => at = end,
                }
            }
        }
        // Open layers (pool slot indices), innermost last.
        let mut stack: Vec<usize> = Vec::new();
        let mut next = 0;
        let mut pending: Option<(wgpu::BindGroup, u32)> = None;
        let mut clear_next = false;
        while next < spans.len() || pending.is_some() {
            {
                let (pass_view, pass_screen, pass_pipeline) = match stack.last() {
                    Some(&slot) => match (self.layers.slot(slot), layer_pipelines.as_ref()) {
                        (Some(s), Some(lp)) => (&s.view, Some(&s.screen_bg), &lp.0),
                        _ => (view, batch.screen_bg.as_ref(), &pipeline),
                    },
                    None => (view, batch.screen_bg.as_ref(), &pipeline),
                };
                let mut pass = self.begin_pass(
                    encoder,
                    pass_view,
                    pass_pipeline,
                    &instance_buf,
                    pass_screen,
                    std::mem::take(&mut clear_next),
                );
                let mut bound: Option<Option<u64>> = None;
                if let Some((bg, at)) = pending.take() {
                    pass.set_bind_group(3, &bg, &[]);
                    pass.draw(0..4, at..at + 1);
                }
                while let Some(&span) = spans.get(next) {
                    match span {
                        Span::Draw { image, first, end } => {
                            if bound != Some(image) {
                                let image_bg = image
                                    .and_then(|id| self.images.get(&id))
                                    .map(|i| &i.bind_group)
                                    .or(self.empty_image_bg.as_ref());
                                if let Some(bg) = image_bg {
                                    pass.set_bind_group(3, bg, &[]);
                                }
                                bound = Some(image);
                            }
                            pass.draw(0..4, first..end);
                        }
                        Span::Mask(at) => {
                            // Destination-in into the open layer, then back to normal blending.
                            if let (Some(lp), false) = (layer_pipelines.as_ref(), stack.is_empty()) {
                                let image = match batch.quads.get(at as usize).map(|q| q.texture) {
                                    Some(crate::draw::TextureRef::LayerMask { image, .. }) => image,
                                    _ => None,
                                };
                                let image_bg = image
                                    .and_then(|id| self.images.get(&id))
                                    .map(|i| &i.bind_group)
                                    .or(self.empty_image_bg.as_ref());
                                if let Some(bg) = image_bg {
                                    pass.set_bind_group(3, bg, &[]);
                                }
                                bound = None;
                                pass.set_pipeline(&lp.1);
                                pass.draw(0..4, at..at + 1);
                                pass.set_pipeline(&lp.0);
                                layer_stats.mask_passes += 1;
                            }
                        }
                        Span::Backdrop(_) if !stack.is_empty() => {
                            // Inside a layer the frame under the group is not in the target:
                            // the blur is skipped there (the tint still draws).
                            layer_stats.backdrops_in_layers += 1;
                        }
                        Span::Backdrop(_) | Span::Begin(_) | Span::Composite(_) => break,
                    }
                    next += 1;
                }
            }
            match spans.get(next).copied() {
                Some(Span::Backdrop(at)) => {
                    next += 1;
                    if let (Some(b), Some(t)) = (backdrop.as_mut(), target) {
                        let inst = batch.instances[at as usize];
                        let rect = [inst.pos[0], inst.pos[1], inst.size[0], inst.size[1]];
                        if let Some(bg) = b.record(device, encoder, t, rect, inst.kind_layer[3], &mut stats) {
                            pending = Some((bg, at));
                        }
                    }
                }
                Some(Span::Begin(at)) => {
                    next += 1;
                    if let Some(inputs) = self.slot_inputs() {
                        let inst = batch.instances[at as usize];
                        let need = [
                            inst.size[0].max(1.0).ceil() as u32,
                            inst.size[1].max(1.0).ceil() as u32,
                        ];
                        let _span =
                            tracing::trace_span!("ui_group_layer", w = need[0], h = need[1]).entered();
                        let slot = self.layers.acquire(device, need, &inputs);
                        stack.push(slot);
                        layer_stats.max_depth = layer_stats.max_depth.max(stack.len() as u32);
                        clear_next = true;
                    }
                }
                Some(Span::Composite(at)) => {
                    next += 1;
                    if let Some(slot) = stack.pop() {
                        if let Some(s) = self.layers.slot(slot) {
                            pending = Some((s.sample_bg.clone(), at));
                            layer_stats.drawn += 1;
                        }
                        self.layers.release(slot);
                    }
                }
                _ => {}
            }
        }
        // A list cut short (no composite for a begin) must not leak busy slots.
        for slot in stack.drain(..) {
            self.layers.release(slot);
        }
        batch.spans = spans;
        if backdrop.is_some() {
            self.backdrop = backdrop;
        }
        stats.skipped = stats.quads - stats.blurred;
        if main {
            self.backdrop_stats = stats;
            self.layers.end_draw();
            self.layers.fill_stats(&mut layer_stats);
            self.layer_stats = layer_stats;
        }
    }

    /// What the group layers did in the last window draw (ADR-2866): layers composited, nesting
    /// depth, mask passes, and the pool's size and lifetime counters.
    pub fn layer_stats(&self) -> crate::group_layer::LayerStats {
        let mut stats = self.layer_stats;
        self.layers.fill_stats(&mut stats);
        stats
    }

    /// What extract decided for the groups of the current draw lists (ADR-2866): layers needed,
    /// groups that needed none (opaque, single primitive, empty) and groups over the budget.
    pub fn layer_plan(&self) -> crate::group_layer::LayerPlan {
        self.layer_plan
    }

    /// The device objects a new layer slot needs, once the base GPU state exists.
    fn slot_inputs(&self) -> Option<crate::group_layer::SlotInputs> {
        Some(crate::group_layer::SlotInputs {
            image_bgl: self.image_bgl.clone()?,
            screen_bgl: self.screen_bgl.clone()?,
            sampler: self.sampler.clone()?,
            paper_white: self.settings.paper_white_nits,
        })
    }

    /// The layer pipelines (normal blending and destination-in mask) for
    /// [`crate::group_layer::LAYER_FORMAT`], built on the first group.
    fn layer_pipelines(
        &mut self,
        device: &RenderDevice,
    ) -> Option<(wgpu::RenderPipeline, wgpu::RenderPipeline)> {
        let normal = self.pipeline_for(device, crate::group_layer::LAYER_FORMAT)?;
        if self.mask_pipeline.is_none() {
            let shader = self.shader.clone()?;
            let layout = self.layout.clone()?;
            self.mask_pipeline = Some(Self::build_mask_pipeline(device, &shader, &layout));
        }
        Some((normal, self.mask_pipeline.clone()?))
    }

    /// Compiles the destination-in pipeline of layer masks. Touches no renderer state.
    fn build_mask_pipeline(
        device: &RenderDevice,
        shader: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
    ) -> wgpu::RenderPipeline {
        Self::build_pipeline_blend(
            device,
            shader,
            layout,
            crate::group_layer::LAYER_FORMAT,
            MASK_BLEND,
            "ui group layer mask pipeline",
        )
    }

    /// Consecutive window draws without a successful backdrop blur before its scratch targets
    /// are released. Empty UI draws and skipped blurs count; a successful blur resets the count.
    pub const BACKDROP_IDLE_FRAMES: u32 = 120;

    /// Releases all pooled backdrop scratch targets and their bind groups immediately, for
    /// memory pressure or a paused renderer. Repeated calls are harmless. Compiled pipelines
    /// and uniform storage stay warm; the next blur allocates only its required targets.
    /// Submitted GPU work may retain its resources until completion.
    pub fn trim_backdrop(&mut self) {
        if let Some(backdrop) = self.backdrop.as_mut() {
            backdrop.trim();
        }
    }

    /// What the backdrop blur did in the last window draw: how many blurred, how many were
    /// skipped, and what the blurred ones cost (zeros after a draw with no backdrop quads, an
    /// empty list included).
    ///
    /// The live render-target fields (ADR-5420) are read now, from the blur's pool.
    pub fn backdrop_stats(&self) -> crate::backdrop::BackdropStats {
        let mut stats = self.backdrop_stats;
        if let Some(backdrop) = self.backdrop.as_ref() {
            backdrop.fill_target_stats(&mut stats);
        }
        stats
    }

    /// Opens an overlay pass over `view` (keeping what is there, or cleared to transparent for a
    /// fresh group layer) with the shared bindings set.
    fn begin_pass<'e>(
        &self,
        encoder: &'e mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        pipeline: &wgpu::RenderPipeline,
        instance_buf: &wgpu::Buffer,
        screen_bg: Option<&wgpu::BindGroup>,
        clear: bool,
    ) -> wgpu::RenderPass<'e> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ui overlay"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: if clear {
                        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                    } else {
                        wgpu::LoadOp::Load
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_vertex_buffer(0, instance_buf.slice(..));
        if let Some(bg) = screen_bg {
            pass.set_bind_group(0, bg, &[]);
        }
        if let Some(bg) = &self.mono_bg {
            pass.set_bind_group(1, bg, &[]);
        }
        if let Some(bg) = &self.color_bg {
            pass.set_bind_group(2, bg, &[]);
        }
        pass
    }

    /// Uploads images already queued by extraction, including the image-asset placeholders.
    /// Both boot-warm paths drain them so their bind groups exist before the first draw.
    fn upload_pending_images(&mut self, device: &RenderDevice) {
        for (id, w, h, data) in std::mem::take(&mut self.pending_images) {
            self.add_image_rgba(device, id, w, h, &data);
        }
        for (id, w, h, data) in std::mem::take(&mut self.pending_dynamic) {
            self.add_dynamic_image(device, id, w, h, &data);
        }
        for (id, w, h, data) in std::mem::take(&mut self.pending_level0) {
            if !self.write_dynamic_level0(device, id, w, h, &data) {
                self.add_dynamic_image(device, id, w, h, &data);
            }
        }
    }

    /// Creates `batch`'s screen uniform buffer and bind group once the base GPU state exists.
    fn ensure_screen(&self, device: &RenderDevice, batch: &mut QuadBatch) {
        if batch.screen_bg.is_some() {
            return;
        }
        let Some(bgl) = &self.screen_bgl else { return };
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let screen_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui screen uniforms"),
            size: SCREEN_UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        batch.screen_bg = Some(dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui screen bg"),
            layout: bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_buf.as_entire_binding(),
            }],
        }));
        batch.screen_buf = Some(screen_buf);
    }

    /// Creates the backdrop-blur state once the base GPU state exists.
    fn ensure_backdrop(&mut self, device: &RenderDevice) {
        if self.backdrop.is_some() {
            return;
        }
        if let (Some(shader), Some(bgl), Some(sampler)) = (&self.shader, &self.image_bgl, &self.sampler) {
            self.backdrop = Some(crate::backdrop::BackdropGpu::new(device, shader, bgl, sampler));
        }
    }
}

/// Builds the mip chain of a straight-alpha sRGB RGBA8 image as premultiplied sRGB RGBA8 levels
/// (`(width, height, pixels)`, level 0 first). Filtering and downsampling happen on linear
/// premultiplied values, so translucent edges neither darken nor halo at any scale. `None` when
/// `data` does not hold `width × height` pixels.
pub fn image_mip_chain(width: u32, height: u32, data: &[u8]) -> Option<Vec<(u32, u32, Vec<u8>)>> {
    let (w, h) = (width.max(1), height.max(1));
    // u64 product: on 64-bit, 2^31 x 2^31 x 4 wrapped a usize product to 0 (and overflows u64) and accepted an empty buffer.
    let expected = u64::from(w)
        .checked_mul(u64::from(h))
        .and_then(|px| px.checked_mul(4))
        .and_then(|n| usize::try_from(n).ok());
    if expected != Some(data.len()) {
        return None;
    }
    let lut = srgb_decode_lut();
    // Linear premultiplied level 0.
    let mut level: Vec<[f32; 4]> = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let a = f32::from(p[3]) / 255.0;
            [
                lut[p[0] as usize] * a,
                lut[p[1] as usize] * a,
                lut[p[2] as usize] * a,
                a,
            ]
        })
        .collect();
    let (mut lw, mut lh) = (w, h);
    let mut out = vec![(lw, lh, encode_premultiplied(&level))];
    while lw > 1 || lh > 1 {
        let (nw, nh) = ((lw / 2).max(1), (lh / 2).max(1));
        let mut next = vec![[0.0f32; 4]; (nw * nh) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let mut acc = [0.0f32; 4];
                let mut n = 0.0;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let sx = (x * 2 + dx).min(lw - 1);
                    let sy = (y * 2 + dy).min(lh - 1);
                    let p = level[(sy * lw + sx) as usize];
                    for c in 0..4 {
                        acc[c] += p[c];
                    }
                    n += 1.0;
                }
                next[(y * nw + x) as usize] = acc.map(|v| v / n);
            }
        }
        level = next;
        lw = nw;
        lh = nh;
        out.push((lw, lh, encode_premultiplied(&level)));
    }
    Some(out)
}

fn srgb_decode_lut() -> [f32; 256] {
    std::array::from_fn(|i| crate::color::srgb_to_linear(u8::try_from(i).unwrap_or(255)))
}

/// Premultiplies straight-alpha sRGB RGBA8 into `scratch` (mip 0 only, no pyramid).
/// `false` when `data` is not `width × height` pixels.
fn premultiply_level0(width: u32, height: u32, data: &[u8], scratch: &mut Vec<u8>) -> bool {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
        .and_then(|n| usize::try_from(n).ok());
    if expected != Some(data.len()) {
        return false;
    }
    let lut = srgb_decode_lut();
    use crate::color::linear_to_srgb8 as enc;
    scratch.clear();
    scratch.reserve(data.len());
    for p in data.as_chunks::<4>().0 {
        let a = f32::from(p[3]) / 255.0;
        let lin = [
            lut[p[0] as usize] * a,
            lut[p[1] as usize] * a,
            lut[p[2] as usize] * a,
            a,
        ];
        let a8 = (lin[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        scratch.extend_from_slice(&[enc(lin[0]), enc(lin[1]), enc(lin[2]), a8]);
    }
    true
}

/// Writes mip 0 of `texture`.
fn write_level0(device: &RenderDevice, texture: &wgpu::Texture, w: u32, h: u32, pixels: &[u8]) {
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

/// Replaces each `old` (position, size) with the matching `fresh` quad. `false` when a quad is missing,
/// and then `quads` is left unchanged.
fn replace_matching(quads: &mut [UiQuad], old: &[([f32; 2], [f32; 2])], fresh: &[UiQuad]) -> bool {
    if old.len() != fresh.len() {
        return false;
    }
    let mut used = vec![false; quads.len()];
    let mut idxs = Vec::with_capacity(old.len());
    for prev in old {
        let Some(idx) = quads
            .iter()
            .enumerate()
            .position(|(i, quad)| !used[i] && quad.pos == prev.0 && quad.size == prev.1)
        else {
            return false;
        };
        used[idx] = true;
        idxs.push(idx);
    }
    let mut changed = false;
    for (idx, next) in idxs.into_iter().zip(fresh.iter()) {
        if quads[idx] != *next {
            quads[idx] = *next;
            changed = true;
        }
    }
    changed
}

fn encode_premultiplied(level: &[[f32; 4]]) -> Vec<u8> {
    use crate::color::linear_to_srgb8 as enc;
    let mut out = Vec::with_capacity(level.len() * 4);
    for p in level {
        let a = (p[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        out.extend_from_slice(&[enc(p[0]), enc(p[1]), enc(p[2]), a]);
    }
    out
}

/// The `OverlayDraw` adapter installed into [`regia_render::WindowOverlay`].
pub struct OverlayShim {
    renderer: UiRendererHandle,
}

impl OverlayShim {
    /// Wraps the shared renderer handle.
    pub fn new(renderer: UiRendererHandle) -> Box<Self> {
        Box::new(Self { renderer })
    }
}

impl regia_render::OverlayDraw for OverlayShim {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let mut renderer = match self.renderer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        renderer.draw(device, encoder, view, format, width, height);
    }

    fn draw_with_target(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        target: &wgpu::Texture,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let mut renderer = match self.renderer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        renderer.draw_with_target(device, encoder, view, Some(target), format, width, height);
    }

    fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) {
        // The shader compile can take long: hold the renderer's lock only to read the inputs and
        // to store the result, so a frame that extracts or draws meanwhile does not wait for it.
        let (inputs, blur_inputs, layer_inputs) = {
            let mut renderer = match self.renderer.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            (
                renderer.warm_inputs(device, format),
                renderer.backdrop_warm_inputs(device, format),
                renderer.layer_warm_inputs(),
            )
        };
        if inputs.is_none() && blur_inputs.is_none() && layer_inputs.is_none() {
            return;
        }
        let pipeline =
            inputs.map(|(shader, layout)| UiRenderer::build_pipeline(device, &shader, &layout, format));
        // The backdrop blur's pipelines too (ADR-2585): the first blurred panel compiles nothing.
        let blur: Vec<(&'static str, wgpu::RenderPipeline)> = blur_inputs
            .map(|(module, layout, entries)| {
                entries
                    .into_iter()
                    .map(|entry| {
                        let built = crate::backdrop::BackdropGpu::build_pipeline(
                            device, &module, &layout, format, entry,
                        );
                        (entry, built)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The group layer pipelines too (ADR-2866): the first group compiles nothing. When the
        // target itself is in the layer format, `pipeline` above is the normal layer pipeline.
        let layer_format = crate::group_layer::LAYER_FORMAT;
        let (layer_normal, layer_mask) =
            layer_inputs.map_or((None, None), |(shader, layout, normal, mask)| {
                (
                    (normal && format != layer_format)
                        .then(|| UiRenderer::build_pipeline(device, &shader, &layout, layer_format)),
                    mask.then(|| UiRenderer::build_mask_pipeline(device, &shader, &layout)),
                )
            });
        let mut renderer = match self.renderer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(pipeline) = pipeline {
            renderer.install_pipeline(format, pipeline);
        }
        if let Some(pipeline) = layer_normal {
            renderer.install_pipeline(layer_format, pipeline);
        }
        if let Some(pipeline) = layer_mask {
            renderer.mask_pipeline.get_or_insert(pipeline);
        }
        if let Some(backdrop) = renderer.backdrop.as_mut() {
            for (entry, built) in blur {
                backdrop.install_pipeline(format, entry, built);
            }
        }
    }
}

#[cfg(test)]
mod mip_chain_tests {
    use super::image_mip_chain;

    #[test]
    fn wrapping_dimension_product_is_rejected() {
        // 2^31 x 2^31 x 4 wraps a usize product to 0, which used to accept an empty buffer.
        assert!(image_mip_chain(1 << 31, 1 << 31, &[]).is_none());
    }

    #[test]
    fn exact_size_builds_a_full_chain() {
        let levels = image_mip_chain(2, 2, &[255; 16]).expect("chain");
        assert_eq!(levels.len(), 2);
        assert!(image_mip_chain(2, 2, &[255; 15]).is_none());
    }
}

#[cfg(test)]
mod dynamic_image_tests {
    use std::sync::Arc;

    use regia_render::RenderDevice;

    use super::UiRenderer;

    /// Corr-ui-b: a refused picture at a new size left the old texture registered while the
    /// queue recorded the new size, so the next good picture at that size was written into the
    /// smaller texture as a "same size" update: a wgpu validation error, fatal in the engine.
    #[test]
    fn a_refused_picture_at_a_new_size_does_not_write_into_the_old_texture() {
        let device = RenderDevice::shared_headless()
            .unwrap_or_else(|err| panic!("no headless GPU (install Mesa lavapipe): {err}"));
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let handle = UiRenderer::new_handle();
        let mut ui = handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ui.queue_dynamic_image(7, 4, 4, Arc::from(vec![255u8; 4 * 4 * 4]));
        ui.warm(&device, format);
        assert_eq!(ui.images[&7].texture.width(), 4);
        // A host bug: the 8x8 picture arrives with a 4x2 byte count. It is refused.
        ui.queue_dynamic_image(7, 8, 8, Arc::from(vec![255u8; 16]));
        ui.warm(&device, format);
        // The next 8x8 picture is good: it gets an 8x8 texture.
        ui.queue_dynamic_image(7, 8, 8, Arc::from(vec![255u8; 8 * 8 * 4]));
        ui.warm(&device, format);
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
        assert_eq!(ui.images[&7].texture.width(), 8);
        assert_eq!(ui.images[&7].texture.height(), 8);
    }
}
