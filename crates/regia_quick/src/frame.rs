// SPDX-License-Identifier: MPL-2.0
//! Turns a [`Draw`]'s commands into an [`ImmediateBatch`]: tessellation (lyon, with fast paths
//! for plain shapes), glyph quads for text, and the per-frame [`QuickDrawStats`].

use std::collections::BTreeMap;
use std::sync::Arc;

use lyon::tessellation::{FillTessellator, StrokeTessellator};
use regia_core::math::geom;
use regia_render::{ImmediateBatch, ImmediateImage, ImmediateImageFormat, ImmediateTexture, ImmediateVertex};
use regia_ui::Localization;
use regia_ui::atlas::{GlyphAtlases, PAGE_SIZE};
use regia_ui::text::TextSystem;

use crate::draw::mesh::Mesh;
use crate::draw::primitive::Primitive;
use crate::draw::primitive::text::{GlyphQuad, TextStamp};
use crate::draw::render::{RenderContext, RenderPrimitive};
use crate::draw::{Draw, DrawCommand, DrawContext, read};
use crate::prelude_internal::*;

/// Image ids at and above this are the glyph atlas pages (`GLYPH_IMAGE_BASE | kind << 8 | page`).
pub const GLYPH_IMAGE_BASE: u64 = 0xF1F0_0000_0000_0000;

/// The image id of glyph atlas page `page` of `kind` (0 mono, 1 colour).
#[must_use]
pub fn glyph_image_id(kind: u8, page: u32) -> u64 {
    GLYPH_IMAGE_BASE | (u64::from(kind) << 8) | u64::from(page)
}

/// Size and pixel density of the surface a frame is drawn for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameParams {
    /// Window size in logical units.
    pub size: Vec2,
    /// Physical pixels per logical unit.
    pub scale_factor: f32,
}

impl FrameParams {
    /// Clip space from logical units (origin at the centre, +Y up).
    #[must_use]
    pub fn clip_from_local(&self) -> Mat4 {
        let size = self.size.max(Vec2::splat(1.0));
        Mat4::from_scale(Vec3::new(2.0 / size.x, 2.0 / size.y, 1.0))
    }

    /// Converts a logical rectangle to a physical scissor `[x, y, w, h]` (y down).
    fn scissor(&self, rect: geom::Rect) -> [u32; 4] {
        let s = self.scale_factor;
        let x0 = ((rect.left() + self.size.x * 0.5) * s).floor().max(0.0);
        let y0 = ((self.size.y * 0.5 - rect.top()) * s).floor().max(0.0);
        let x1 = ((rect.right() + self.size.x * 0.5) * s).ceil().max(x0);
        let y1 = ((self.size.y * 0.5 - rect.bottom()) * s).ceil().max(y0);
        [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32]
    }
}

/// What the quick layer drew in the last frame: what agents read through `quick.draw_list` and
/// the `QuickDraw` predicate resource.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct QuickDrawStats {
    /// Frames extracted so far.
    pub frame: u64,
    /// Drawings in the last frame (2D).
    pub primitives: u32,
    /// Drawings per kind (`rect`, `ellipse`, `line`, `text`, …).
    pub by_kind: BTreeMap<String, u32>,
    /// The strings of the text drawings, in drawing order (at most 64).
    pub texts: Vec<String>,
    /// Background colour set this frame (linear RGBA).
    pub background: Option<[f32; 4]>,
    /// Bounding box of every 2D vertex in logical units: `[min_x, min_y, max_x, max_y]`.
    pub bounds: Option<[f32; 4]>,
    /// 2D vertices submitted.
    pub vertices: u32,
    /// 2D triangles submitted.
    pub triangles: u32,
    /// GPU draw calls after batching.
    pub draw_calls: u32,
    /// 3D primitives drawn (lit meshes).
    pub primitives_3d: u32,
    /// 3D line segments drawn.
    pub lines_3d: u32,
}

/// Maximum text strings kept in [`QuickDrawStats::texts`].
const MAX_TEXTS: usize = 64;

/// Shaped texts kept on the renderer. A larger cache is dropped and rebuilt.
const TEXT_CACHE_LIMIT: usize = 32;

struct TextCacheEntry {
    stamp: TextStamp,
    quads: Vec<GlyphQuad>,
}

/// Inputs outside a [`TextStamp`] that change every cached text's quads. When any of them
/// differs from the values the cache was filled under, the whole cache is dropped.
#[derive(PartialEq)]
struct TextCacheContext {
    /// [`GlyphAtlases::wipes`]: a recycled page invalidates the atlas rects the quads hold.
    atlas_wipes: u64,
    /// [`TextSystem::text_scale`] bits (the player's text size).
    text_scale: u32,
    /// [`TextSystem::min_font_px`] bits.
    min_font_px: u32,
    /// [`TextSystem::locale`]: font fallback (Han unification) follows it.
    locale: String,
    /// [`Localization::generation`]: bumped on every language switch.
    loc_generation: u64,
}

impl TextCacheContext {
    fn matches(&self, atlas: &GlyphAtlases, fonts: &TextSystem, localization: &Localization) -> bool {
        self.atlas_wipes == atlas.wipes()
            && self.text_scale == fonts.text_scale.to_bits()
            && self.min_font_px == fonts.min_font_px.to_bits()
            && self.loc_generation == localization.generation.0
            && self.locale == fonts.locale()
    }

    fn of(atlas: &GlyphAtlases, fonts: &TextSystem, localization: &Localization) -> Self {
        Self {
            atlas_wipes: atlas.wipes(),
            text_scale: fonts.text_scale.to_bits(),
            min_font_px: fonts.min_font_px.to_bits(),
            locale: fonts.locale().to_owned(),
            loc_generation: localization.generation.0,
        }
    }
}

/// CPU state of the quick layer's frame building: tessellators, scratch buffers and the glyph
/// atlas.
#[derive(Default)]
pub struct QuickRenderer {
    fill: FillTessellator,
    stroke: StrokeTessellator,
    mesh: Mesh,
    glyphs: Vec<GlyphQuad>,
    atlas: GlyphAtlases,
    /// Glyph pages changed since the last [`Self::take_images`].
    pending: Vec<(u64, ImmediateImage)>,
    /// Command-vector allocations during the last [`Self::build`].
    last_command_allocs: u64,
    /// Text shapes performed by [`Self::build`] since this renderer was created.
    text_shapes: u64,
    /// Theme-map clones during the last [`Self::build`].
    theme_clones: u64,
    /// New `String` allocations for [`QuickDrawStats::texts`] since this renderer was created.
    text_string_allocs: u64,
    /// Command list reused across frames.
    commands: Vec<DrawCommand>,
    /// Shaped glyph quads, keyed by `TextStamp`.
    text_cache: Vec<TextCacheEntry>,
    /// String buffers reused for [`QuickDrawStats::texts`].
    text_spare: Vec<String>,
    /// Kind names kept so a repeated kind does not allocate a new `String`.
    kind_stats: BTreeMap<String, u32>,
    /// What [`Self::text_cache`] was filled under; `None` before the first text.
    text_cache_context: Option<TextCacheContext>,
}

impl QuickRenderer {
    /// A renderer with empty caches.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Command-vector allocations during the last [`Self::build`].
    ///
    /// Zero on a later frame means both the draw's command list and this renderer's command
    /// buffer were reused.
    #[must_use]
    pub fn last_command_allocs(&self) -> u64 {
        self.last_command_allocs
    }

    /// Text shapes performed by [`Self::build`] since this renderer was created.
    ///
    /// A repeated string with the same font, wrap, scale and placement does not increment this.
    #[must_use]
    pub fn text_shape_count(&self) -> u64 {
        self.text_shapes
    }

    /// Theme-map clones during the last [`Self::build`].
    #[must_use]
    pub fn last_theme_clones(&self) -> u64 {
        self.theme_clones
    }

    /// New `String` allocations for [`QuickDrawStats::texts`] since this renderer was created.
    ///
    /// A later frame reuses the previous frame's string buffers and does not increment this.
    #[must_use]
    pub fn text_string_allocs(&self) -> u64 {
        self.text_string_allocs
    }

    /// Glyph atlas pages recycled so far. A recycle drops every cached text shape, because the
    /// cached quads point at atlas rects that now hold other glyphs.
    #[must_use]
    pub fn glyph_atlas_wipes(&self) -> u64 {
        self.atlas.wipes()
    }

    /// Glyphs not drawn so far because the frame that needed them had filled every atlas page
    /// with glyphs it already drew ([`GlyphAtlases::thrash`], ADR-8832).
    #[must_use]
    pub fn glyph_atlas_thrash(&self) -> u64 {
        self.atlas.thrash()
    }

    /// Drops the text cache when an input outside the per-text stamp changed (an atlas page was
    /// recycled, the text size or locale changed, or the language switched).
    ///
    /// Takes the fields it needs, not `self`: [`Self::build`] calls it while it drains
    /// `self.commands`.
    fn sync_text_cache(
        cache: &mut Vec<TextCacheEntry>,
        cache_context: &mut Option<TextCacheContext>,
        atlas: &GlyphAtlases,
        fonts: &TextSystem,
        localization: &Localization,
    ) {
        let current = cache_context
            .as_ref()
            .is_some_and(|context| context.matches(atlas, fonts, localization));
        if !current {
            cache.clear();
            *cache_context = Some(TextCacheContext::of(atlas, fonts, localization));
        }
    }

    /// Vertices left in the scratch mesh after the last [`Self::build`].
    ///
    /// Shapes written straight into the batch leave this at 0.
    #[must_use]
    pub fn scratch_vertices(&self) -> usize {
        self.mesh.vertices.len()
    }

    /// Drains `draw`'s commands into `batch` (cleared first) and returns what was drawn. Text is
    /// skipped when `text` is `None`.
    #[tracing::instrument(skip_all, name = "quick_draw_build")]
    pub fn build(
        &mut self,
        draw: &Draw,
        params: FrameParams,
        mut text: Option<(&mut TextSystem, &Localization)>,
        batch: &mut ImmediateBatch,
    ) -> QuickDrawStats {
        batch.clear();
        // Pages that served this frame's glyphs are pinned until the frame ends, so a later text
        // never recycles a page an earlier quad of the frame samples (ADR-8832, ADR-8930).
        self.atlas.begin_frame();
        let mut stats = QuickDrawStats::default();
        let allocs_before = draw.command_allocs();
        draw.drain_commands_into(&mut self.commands);
        self.last_command_allocs = draw.command_allocs().saturating_sub(allocs_before);
        let state = read(&draw.state);
        let intermediary = read(&state.intermediary_state);
        let theme = &state.theme;
        self.theme_clones = 0;
        let mut context = DrawContext::default();
        for count in self.kind_stats.values_mut() {
            *count = 0;
        }
        for command in self.commands.drain(..) {
            match command {
                DrawCommand::Context(c) => context = c,
                DrawCommand::BackgroundColor(color) => {
                    stats.background = Some(color.to_array());
                    batch.set_state(regia_render::ImmediateState::default());
                    let (hw, hh) = (params.size.x * 0.5, params.size.y * 0.5);
                    let c = color.to_array();
                    batch.triangles(
                        &[
                            ImmediateVertex::new([-hw, -hh, 0.0], c),
                            ImmediateVertex::new([hw, -hh, 0.0], c),
                            ImmediateVertex::new([hw, hh, 0.0], c),
                            ImmediateVertex::new([-hw, hh, 0.0], c),
                        ],
                        &[0, 1, 2, 0, 2, 3],
                    );
                }
                DrawCommand::Primitive(primitive, image) => {
                    stats.primitives += 1;
                    let kind = primitive.kind_name();
                    match self.kind_stats.get_mut(kind) {
                        Some(n) => *n += 1,
                        None => {
                            self.kind_stats.insert(kind.to_string(), 1);
                        }
                    }
                    let scissor = context.scissor.map(|r| params.scissor(r));
                    if let Primitive::Text(t) = primitive {
                        let Some((fonts, localization)) = text.as_mut() else {
                            continue;
                        };
                        if stats.texts.len() < MAX_TEXTS {
                            let i = stats.texts.len();
                            let s = t.string(&intermediary.text_buffer);
                            if self.text_spare.len() == i {
                                self.text_string_allocs = self.text_string_allocs.saturating_add(1);
                                self.text_spare.push(s.to_string());
                            } else {
                                self.text_spare[i].clear();
                                self.text_spare[i].push_str(s);
                            }
                            stats.texts.push(self.text_spare[i].clone());
                        }
                        self.glyphs.clear();
                        Self::sync_text_cache(
                            &mut self.text_cache,
                            &mut self.text_cache_context,
                            &self.atlas,
                            fonts,
                            localization,
                        );
                        let hit = self.text_cache.iter().position(|entry| {
                            t.matches_stamp(
                                &intermediary.text_buffer,
                                &entry.stamp,
                                theme,
                                &context.transform,
                                params.size,
                                params.scale_factor,
                            )
                        });
                        if let Some(hit) = hit {
                            let quads = std::mem::take(&mut self.text_cache[hit].quads);
                            // Replayed quads sample their pages as much as fresh ones.
                            for g in &quads {
                                self.atlas.use_page(g.kind, g.page);
                            }
                            self.glyphs.extend_from_slice(&quads);
                            self.text_cache[hit].quads = quads;
                        } else {
                            self.text_shapes = self.text_shapes.saturating_add(1);
                            let stamp = t.to_stamp(
                                &intermediary.text_buffer,
                                theme,
                                &context.transform,
                                params.size,
                                params.scale_factor,
                            );
                            let _span =
                                tracing::info_span!(target: "regia_quick", "quick_text_shape").entered();
                            let refused_before = self.atlas.thrash();
                            t.render_glyphs(
                                &intermediary.text_buffer,
                                theme,
                                &context.transform,
                                params.size,
                                params.scale_factor,
                                fonts,
                                localization,
                                &mut self.atlas,
                                &mut self.glyphs,
                            );
                            // Shaping may have recycled an atlas page that older entries use.
                            Self::sync_text_cache(
                                &mut self.text_cache,
                                &mut self.text_cache_context,
                                &self.atlas,
                                fonts,
                                localization,
                            );
                            // A text that lost glyphs to a full atlas is shaped again next frame.
                            if self.atlas.thrash() == refused_before {
                                if self.text_cache.len() >= TEXT_CACHE_LIMIT {
                                    self.text_cache.clear();
                                }
                                self.text_cache.push(TextCacheEntry {
                                    stamp,
                                    quads: self.glyphs.clone(),
                                });
                            }
                        }
                        for g in &self.glyphs {
                            let id = glyph_image_id(g.kind, g.page);
                            batch.set_state(regia_render::ImmediateState {
                                texture: if g.kind == 1 {
                                    ImmediateTexture::Image(id)
                                } else {
                                    ImmediateTexture::Mask(id)
                                },
                                blend: context.blend,
                                scissor,
                            });
                            let c = g.color.to_array();
                            let [u0, v0, u1, v1] = g.uv;
                            let p = g.corners.map(|v| v.to_array());
                            batch.triangles(
                                &[
                                    ImmediateVertex::textured(p[0], [u0, v0], c),
                                    ImmediateVertex::textured(p[1], [u1, v0], c),
                                    ImmediateVertex::textured(p[2], [u1, v1], c),
                                    ImmediateVertex::textured(p[3], [u0, v1], c),
                                ],
                                &[0, 1, 2, 0, 2, 3],
                            );
                        }
                        continue;
                    }
                    let ctxt = RenderContext {
                        transform: &context.transform,
                        intermediary_mesh: &intermediary.intermediary_mesh,
                        path_event_buffer: &intermediary.path_event_buffer,
                        path_points_vertex_buffer: &intermediary.path_points_vertex_buffer,
                        text_buffer: &intermediary.text_buffer,
                        theme,
                        fill_tessellator: &mut self.fill,
                        stroke_tessellator: &mut self.stroke,
                        output_attachment_size: params.size,
                        output_attachment_scale_factor: params.scale_factor,
                    };
                    batch.set_state(regia_render::ImmediateState {
                        texture: image.map_or(ImmediateTexture::White, |i| ImmediateTexture::Image(i.0)),
                        blend: context.blend,
                        scissor,
                    });
                    // The batch streams become the mesh for this primitive, so vertices are not
                    // copied out of a scratch buffer. The model matrix on this path is identity.
                    self.mesh.clear();
                    {
                        let (verts, indices) = batch.streams_mut();
                        std::mem::swap(&mut self.mesh.vertices, verts);
                        std::mem::swap(&mut self.mesh.indices, indices);
                    }
                    let v0 = self.mesh.vertices.len();
                    let i0 = self.mesh.indices.len();
                    primitive.render_primitive(ctxt, &mut self.mesh);
                    if self.mesh.indices.len() == i0 {
                        self.mesh.vertices.truncate(v0);
                        self.mesh.indices.truncate(i0);
                    }
                    {
                        let (verts, indices) = batch.streams_mut();
                        std::mem::swap(&mut self.mesh.vertices, verts);
                        std::mem::swap(&mut self.mesh.indices, indices);
                    }
                }
            }
        }
        drop(intermediary);
        drop(state);
        self.atlas.end_frame();
        self.kind_stats.retain(|_, n| *n != 0);
        stats.by_kind.clone_from(&self.kind_stats);
        batch.flush();
        let s = batch.stats();
        stats.vertices = s.vertices;
        stats.triangles = s.triangles;
        stats.draw_calls = s.draw_calls;
        stats.bounds = bounds(batch.vertices());
        self.collect_glyph_pages();
        stats
    }

    fn collect_glyph_pages(&mut self) {
        for (kind, page) in self.atlas.take_dirty_pages() {
            let Some((data, bytes_per_pixel)) = self.atlas.page_data(kind, page) else {
                continue;
            };
            let format = if bytes_per_pixel == 4 {
                ImmediateImageFormat::Rgba8SrgbPremultiplied
            } else {
                ImmediateImageFormat::R8
            };
            self.pending.retain(|(id, _)| *id != glyph_image_id(kind, page));
            self.pending.push((
                glyph_image_id(kind, page),
                ImmediateImage {
                    width: PAGE_SIZE,
                    height: PAGE_SIZE,
                    format,
                    data: Arc::from(data),
                },
            ));
        }
    }

    /// Glyph atlas pages that changed since the last call (register them with the GPU
    /// renderer under their ids).
    pub fn take_images(&mut self) -> Vec<(u64, ImmediateImage)> {
        std::mem::take(&mut self.pending)
    }
}

fn bounds(vertices: &[ImmediateVertex]) -> Option<[f32; 4]> {
    let first = vertices.first()?;
    let mut b = [
        first.position[0],
        first.position[1],
        first.position[0],
        first.position[1],
    ];
    for v in vertices {
        b[0] = b[0].min(v.position[0]);
        b[1] = b[1].min(v.position[1]);
        b[2] = b[2].max(v.position[0]);
        b[3] = b[3].max(v.position[1]);
    }
    Some(b)
}
