// SPDX-License-Identifier: MPL-2.0
//! The draw model (`specs/18` § 3): quads extracted from the widget tree, clipped on the CPU,
//! sorted by z, batched by texture.

use bytemuck::{Pod, Zeroable};

use crate::color::UiColor;

/// Which texture a quad samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureRef {
    /// No texture: SDF rect (fill/border/shadow/gradient).
    Rect,
    /// Monochrome glyph atlas, layer (page).
    MonoAtlas(u32),
    /// Colour (emoji) atlas, layer (page).
    ColorAtlas(u32),
    /// An image registered with the renderer.
    Image(u64),
    /// A frosted-glass region: the already drawn frame under the quad, blurred with a Gaussian
    /// of σ = [`UiQuad::blur`] px and composited through the quad's rounded rect (ADR-1836).
    /// Draw a tint quad right after it for the glass colour. Without a readable target the
    /// quad draws nothing, so the tint alone remains.
    Backdrop,
    /// A text effect layer (outline or shadow, ADR-2045) sampling the monochrome glyph atlas
    /// page: the quad's `corner_radius` holds the glyph's own uv rect and `border_width[0..2]`
    /// the outline width and blur in px ([`crate::text_effects::effect_quad`]).
    GlyphEffect(u32),
    /// A procedural UI material fill (ADR-2045, [`crate::ui_material`]): `color`/`color2` are
    /// its two colors, `border_width` its four parameters, `angle` its time in seconds and
    /// `blur` the material kind.
    Material,
    /// A distance-field glyph (ADR-2045, [`crate::sdf_text`]) on a monochrome atlas page:
    /// `border_width[0]` is screen px per reference px, `border_width[1]` the field's spread.
    SdfGlyph(u32),
    /// Start of an isolated group layer (ADR-2866, [`crate::group_layer`]): the quads up to the
    /// matching [`TextureRef::LayerComposite`] draw into a pooled offscreen layer of this quad's
    /// size, in layer space. Draws nothing itself.
    LayerBegin,
    /// Multiplies the open layer by a mask (destination-in): `border_width` is the masked rect
    /// (x0, y0, x1, y1) and `corner_radius` its radii (rounded when `blur > 0`), `color2` the
    /// placed image rect (x0, y0, x1, y1) and `uv` its sub-rect, all in layer space.
    LayerMask {
        /// Image whose alpha masks the layer (`None` = rounded rect only).
        image: Option<u64>,
        /// The image repeats past its placed rect.
        tile: bool,
    },
    /// Closes the open layer and blends it into the parent at this quad's rect, with `alpha`
    /// as the group opacity.
    LayerComposite,
}

/// Largest backdrop-blur σ in physical px (`TextureRef::Backdrop`); larger values clamp.
pub const MAX_BACKDROP_SIGMA: f32 = 64.0;

/// Clip rect that clips nothing (x, y, w, h).
pub const NO_CLIP: [f32; 4] = [-1.0e9, -1.0e9, 2.0e9, 2.0e9];

/// One draw quad: position/size in physical pixels (y down), uv rect, colours and SDF
/// decoration parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiQuad {
    /// Top-left position. For shadow quads: the caster rect, already offset.
    pub pos: [f32; 2],
    /// Size.
    pub size: [f32; 2],
    /// UV rect (u0, v0, u1, v1) over the full quad; ignored for `TextureRef::Rect`.
    pub uv: [f32; 4],
    /// Fill colour (linear, straight alpha); for glyph quads this is the text colour.
    pub color: UiColor,
    /// Secondary colour: border colour (rects) or gradient end colour.
    pub color2: UiColor,
    /// Corner radii (tl, tr, br, bl) in px.
    pub corner_radius: [f32; 4],
    /// Border widths (l, t, r, b) in px.
    pub border_width: [f32; 4],
    /// Blur radius for shadow quads (Gaussian σ = blur / 2, like CSS `box-shadow`); for
    /// [`TextureRef::Backdrop`] quads the Gaussian σ itself in px; 0 for other quads.
    pub blur: f32,
    /// Gradient angle in degrees for gradient quads: 0 = top→bottom, 90 = left→right.
    pub angle: f32,
    /// Shadow quads: offset of the shadow from its caster (px). The shadow is not drawn under
    /// the caster (CSS `box-shadow` semantics).
    pub shadow_offset: [f32; 2],
    /// Texture selection.
    pub texture: TextureRef,
    /// True only for `BackgroundKind::Gradient` quads: `color`→`color2` is a linear gradient.
    /// Border/ring quads also set `color2 != color` and must NOT be gradients, so the kind is
    /// explicit instead of inferred from the colours (see [`instance_of`]).
    pub gradient: bool,
    /// Extra multiplier (opacity), folded into alpha by the extractor.
    pub alpha: f32,
    /// Draw depth (extract sorts by this).
    pub z: f32,
    /// Clip rect (x, y, w, h) in physical px; the GPU draws only the part of the quad inside it
    /// while the quad's SDF frame and uv mapping stay those of the full quad.
    pub clip: [f32; 4],
}

impl UiQuad {
    /// A plain coloured rect.
    pub fn fill(pos: [f32; 2], size: [f32; 2], color: UiColor) -> Self {
        Self {
            pos,
            size,
            uv: [0.0, 0.0, 1.0, 1.0],
            color,
            color2: color,
            corner_radius: [0.0; 4],
            border_width: [0.0; 4],
            blur: 0.0,
            angle: 0.0,
            shadow_offset: [0.0; 2],
            texture: TextureRef::Rect,
            gradient: false,
            alpha: 1.0,
            z: 0.0,
            clip: NO_CLIP,
        }
    }

    /// The rect this quad actually touches on screen (shadows extend past their caster).
    pub fn bounds(&self) -> [f32; 4] {
        let m = self.shadow_margin();
        [
            self.pos[0] - m,
            self.pos[1] - m,
            self.size[0] + 2.0 * m,
            self.size[1] + 2.0 * m,
        ]
    }

    /// How far a shadow quad's geometry extends past its caster rect (3σ).
    pub fn shadow_margin(&self) -> f32 {
        if self.texture == TextureRef::Rect && self.blur > 0.0 {
            (self.blur * 1.5).ceil()
        } else {
            0.0
        }
    }

    /// Intersects this quad's clip rect with `clip` (x, y, w, h). Returns `None` when the quad
    /// is entirely outside. Geometry, uv and decoration are kept: clipping happens on the GPU,
    /// so a scrolled rounded panel keeps the corners that remain visible.
    pub fn clipped(mut self, clip: [f32; 4]) -> Option<Self> {
        let c = intersect(self.clip, clip);
        let b = intersect(self.bounds(), c);
        if b[2] <= 0.0 || b[3] <= 0.0 {
            return None;
        }
        self.clip = c;
        Some(self)
    }
}

/// Intersection of two rects (x, y, w, h); empty results have zero size.
pub fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

/// Splits an image fill into up to nine quads (`specs/18` § 1 "image (9-slice)"): corners keep
/// their source size × `scale`, edges stretch along one axis, the centre stretches both ways.
///
/// `base` supplies the destination rect and the shared quad state (tint, alpha, z, clip);
/// `uv` is the image sub-rect, `insets` (l, t, r, b) are in source pixels of an image of
/// `image_px`. Every slice edge is snapped to a whole pixel and adjacent slices share the exact
/// same edge coordinate, so the joins have neither gaps nor double-blended seams.
pub fn nine_slice(
    base: UiQuad,
    image: u64,
    uv: [f32; 4],
    insets: [f32; 4],
    image_px: [u32; 2],
    scale: f32,
) -> Vec<UiQuad> {
    let (iw, ih) = (image_px[0].max(1) as f32, image_px[1].max(1) as f32);
    // NaN, infinite or negative insets slice nothing, and an infinite scale would give `inf * 0`
    // (NaN) positions: both count as no slicing.
    let insets = insets.map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 });
    let scale = if scale.is_finite() { scale } else { 0.0 };
    let x0 = base.pos[0].round();
    let y0 = base.pos[1].round();
    let x1 = (base.pos[0] + base.size[0]).round();
    let y1 = (base.pos[1] + base.size[1]).round();
    // Destination insets in whole pixels, shrunk proportionally when the rect is too small.
    let fit = |a: f32, b: f32, span: f32| -> (f32, f32) {
        let (a, b) = ((a * scale).max(0.0), (b * scale).max(0.0));
        let k = if a + b > span && a + b > 0.0 {
            span / (a + b)
        } else {
            1.0
        };
        let a = (a * k).round();
        (a, ((b * k).round()).min(span - a).max(0.0))
    };
    let (dl, dr) = fit(insets[0], insets[2], x1 - x0);
    let (dt, db) = fit(insets[1], insets[3], y1 - y0);
    let xs = [x0, x0 + dl, x1 - dr, x1];
    let ys = [y0, y0 + dt, y1 - db, y1];
    // Source insets (UV units) are shrunk to the UV span the same way, so the centre slice never
    // reverses and the corners never sample outside the rect.
    let fit_uv = |a: f32, b: f32, span: f32| -> (f32, f32) {
        let span = span.max(0.0);
        let k = if a + b > span && a + b > 0.0 {
            span / (a + b)
        } else {
            1.0
        };
        (a * k, b * k)
    };
    let (ul, ur) = fit_uv(insets[0] / iw, insets[2] / iw, uv[2] - uv[0]);
    let (vt, vb) = fit_uv(insets[1] / ih, insets[3] / ih, uv[3] - uv[1]);
    let us = [uv[0], uv[0] + ul, uv[2] - ur, uv[2]];
    let vs = [uv[1], uv[1] + vt, uv[3] - vb, uv[3]];
    let mut out = Vec::with_capacity(9);
    for j in 0..3 {
        for i in 0..3 {
            let (w, h) = (xs[i + 1] - xs[i], ys[j + 1] - ys[j]);
            if w <= 0.0 || h <= 0.0 {
                continue;
            }
            out.push(UiQuad {
                pos: [xs[i], ys[j]],
                size: [w, h],
                uv: [us[i], vs[j], us[i + 1], vs[j + 1]],
                texture: TextureRef::Image(image),
                corner_radius: [0.0; 4],
                border_width: [0.0; 4],
                ..base
            });
        }
    }
    out
}

/// GPU instance payload. `kind`: 0 rect, 1 mono glyph, 2 colour glyph, 3 image, 4 gradient,
/// 5 shadow, 6 backdrop blur (`kind_layer.w` = σ px), 7 glyph effect layer, 8 UI material
/// (`kind_layer.z` = time s, `kind_layer.w` = material kind), 9 distance-field glyph, 10 layer
/// mask (`kind_layer.y` = flags: 1 image, 2 tile, 4 rounded), 11 layer begin, 12 layer composite.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct QuadInstance {
    /// Top-left position in physical px (shadows: expanded by the blur margin).
    pub pos: [f32; 2],
    /// Size in px.
    pub size: [f32; 2],
    /// UV rect; shadows: xy = shadow offset from the caster.
    pub uv: [f32; 4],
    /// Fill colour (linear, straight alpha, opacity folded in).
    pub color: [f32; 4],
    /// Secondary colour (border / gradient end).
    pub color2: [f32; 4],
    /// Corner radii tl, tr, br, bl.
    pub corner_radius: [f32; 4],
    /// Border widths l, t, r, b.
    pub border_width: [f32; 4],
    /// x: kind, y: atlas layer (shadows: margin px), z: gradient angle (degrees),
    /// w: shadow σ (px).
    pub kind_layer: [f32; 4],
    /// Clip rect x0, y0, x1, y1 in physical px.
    pub clip: [f32; 4],
}

impl QuadInstance {
    /// Bytes per instance (static assert target).
    pub const SIZE: u32 = std::mem::size_of::<Self>() as u32;
}

/// Converts a quad into its GPU instance form.
pub fn instance_of(q: &UiQuad) -> QuadInstance {
    let margin = q.shadow_margin();
    let (kind, layer) = match q.texture {
        TextureRef::Rect => {
            if q.blur > 0.0 {
                (5.0, margin)
            } else if q.gradient {
                (4.0, 0.0)
            } else {
                (0.0, 0.0)
            }
        }
        TextureRef::MonoAtlas(l) => (1.0, l as f32),
        TextureRef::ColorAtlas(l) => (2.0, l as f32),
        TextureRef::Image(_) => (3.0, 0.0),
        TextureRef::Backdrop => (6.0, 0.0),
        TextureRef::GlyphEffect(l) => (7.0, l as f32),
        TextureRef::Material => (8.0, 0.0),
        TextureRef::SdfGlyph(l) => (9.0, l as f32),
        TextureRef::LayerMask { image, tile } => (
            10.0,
            f32::from(u8::from(image.is_some()) + 2 * u8::from(tile) + 4 * u8::from(q.blur > 0.0)),
        ),
        TextureRef::LayerBegin => (11.0, 0.0),
        TextureRef::LayerComposite => (12.0, 0.0),
    };
    let shadow = kind == 5.0;
    QuadInstance {
        pos: [q.pos[0] - margin, q.pos[1] - margin],
        size: [q.size[0] + 2.0 * margin, q.size[1] + 2.0 * margin],
        uv: if shadow {
            [q.shadow_offset[0], q.shadow_offset[1], 0.0, 0.0]
        } else {
            q.uv
        },
        color: [q.color.0[0], q.color.0[1], q.color.0[2], q.color.0[3] * q.alpha],
        color2: [
            q.color2.0[0],
            q.color2.0[1],
            q.color2.0[2],
            q.color2.0[3] * q.alpha,
        ],
        corner_radius: q.corner_radius,
        border_width: q.border_width,
        kind_layer: [
            kind,
            layer,
            q.angle,
            if shadow {
                q.blur * 0.5
            } else if kind == 6.0 || kind == 8.0 {
                q.blur
            } else {
                0.0
            },
        ],
        clip: [q.clip[0], q.clip[1], q.clip[0] + q.clip[2], q.clip[1] + q.clip[3]],
    }
}

/// A run of consecutive instances drawn with one image binding (one draw call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRange {
    /// The image bound for this run (`None` = atlas / SDF quads only).
    pub image: Option<u64>,
    /// First instance.
    pub first: u32,
    /// Instance count.
    pub count: u32,
}

/// Fills `instances` (in draw order) and `ranges` (one per image change) from sorted quads,
/// reusing both allocations. Quads that sample no image (SDF rects, glyphs) join whatever run is
/// open, so an icon between panels costs no extra draw call unless the image actually changes.
pub fn batch_ranges(quads: &[UiQuad], instances: &mut Vec<QuadInstance>, ranges: &mut Vec<BatchRange>) {
    instances.clear();
    ranges.clear();
    for q in quads {
        let img = match q.texture {
            TextureRef::Image(id) => Some(id),
            _ => None,
        };
        let index = u32::try_from(instances.len()).unwrap_or(u32::MAX);
        match ranges.last_mut() {
            // Image-free quads ride along with any binding; a run without an image yet adopts
            // the first image that arrives.
            Some(r) if img.is_none() || r.image == img => r.count += 1,
            Some(r) if r.image.is_none() => {
                r.image = img;
                r.count += 1;
            }
            _ => ranges.push(BatchRange {
                image: img,
                first: index,
                count: 1,
            }),
        }
        instances.push(instance_of(q));
    }
}

/// One batch of instances sharing a texture binding.
#[derive(Debug, Default)]
pub struct QuadBatch {
    /// Instances in draw order.
    pub instances: Vec<QuadInstance>,
    /// The image bound for this batch (`None` for atlas/rect quads).
    pub image: Option<u64>,
}

/// Splits sorted quads into batches: a new batch starts whenever the bound image changes
/// (atlas pages live in always-bound texture arrays).
pub fn batch(quads: &[UiQuad]) -> Vec<QuadBatch> {
    let mut batches: Vec<QuadBatch> = Vec::new();
    let mut current_image: Option<u64> = None;
    for q in quads {
        let img = match q.texture {
            TextureRef::Image(id) => Some(id),
            _ => None,
        };
        if batches.is_empty() || img != current_image {
            batches.push(QuadBatch {
                instances: Vec::new(),
                image: img,
            });
            current_image = img;
        }
        if let Some(b) = batches.last_mut() {
            b.instances.push(instance_of(q))
        }
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nine_slice_uvs_shrink_with_oversized_insets() {
        let base = UiQuad::fill([0.0, 0.0], [200.0, 40.0], UiColor::WHITE);
        let q = nine_slice(
            base,
            1,
            [0.0, 0.0, 1.0, 1.0],
            [100.0, 0.0, 100.0, 0.0],
            [64, 64],
            1.0,
        );
        for quad in &q {
            let u = quad.uv;
            assert!(u[0] >= 0.0 && u[2] <= 1.0 && u[0] <= u[2], "uv {u:?}");
        }
    }

    #[test]
    fn a_backdrop_quad_becomes_kind_six_with_sigma_and_alpha() {
        let mut q = UiQuad::fill([4.0, 5.0], [30.0, 20.0], UiColor::WHITE);
        q.texture = TextureRef::Backdrop;
        q.blur = 7.0;
        q.alpha = 0.5;
        q.corner_radius = [3.0; 4];
        let i = instance_of(&q);
        assert_eq!(i.kind_layer[0], 6.0);
        assert_eq!(i.kind_layer[3], 7.0, "sigma, not halved like a shadow");
        // No shadow margin: the geometry is the node rect.
        assert_eq!((i.pos, i.size), ([4.0, 5.0], [30.0, 20.0]));
        assert_eq!(i.color[3], 0.5);
        assert_eq!(q.bounds(), [4.0, 5.0, 30.0, 20.0]);
    }

    #[test]
    fn clip_records_rect_and_keeps_uv_frame() {
        // GPU clipping: the quad keeps its full geometry and uv mapping (the vertex shader
        // draws only the clipped part and interpolates uv over the full quad), and the clip is
        // the intersection of every clip applied.
        let mut q = UiQuad::fill([10.0, 10.0], [20.0, 20.0], UiColor::WHITE);
        q.uv = [0.0, 0.0, 1.0, 1.0];
        let c = q.clipped([0.0, 0.0, 20.0, 100.0]).unwrap();
        assert_eq!(c.pos, [10.0, 10.0]);
        assert_eq!(c.size, [20.0, 20.0]);
        assert_eq!(c.uv, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(c.clip, [0.0, 0.0, 20.0, 100.0]);
        let c = c.clipped([15.0, 5.0, 100.0, 100.0]).unwrap();
        assert_eq!(c.clip, [15.0, 5.0, 5.0, 95.0]);
        let inst = instance_of(&c);
        assert_eq!(inst.clip, [15.0, 5.0, 20.0, 100.0], "x0, y0, x1, y1");
    }

    #[test]
    fn clip_removes_outside() {
        let q = UiQuad::fill([50.0, 50.0], [10.0, 10.0], UiColor::WHITE);
        assert!(q.clipped([0.0, 0.0, 10.0, 10.0]).is_none());
    }

    #[test]
    fn clip_keeps_decoration() {
        // Borders and radii are relative to the full rect, which the GPU still knows, so a
        // cropping clip keeps them (the visible corners stay rounded while scrolling).
        let mut q = UiQuad::fill([10.0, 10.0], [20.0, 10.0], UiColor::WHITE);
        q.border_width = [2.0; 4];
        q.corner_radius = [4.0; 4];
        let c = q.clipped(NO_CLIP).unwrap();
        assert_eq!(c.border_width, [2.0; 4], "uncropped quad keeps borders");
        assert_eq!(c.corner_radius, [4.0; 4], "uncropped quad keeps radii");
        let c = q.clipped([0.0, 0.0, 20.0, 100.0]).expect("partially inside");
        assert_eq!(c.border_width, [2.0; 4]);
        assert_eq!(c.corner_radius, [4.0; 4]);
    }

    #[test]
    fn nine_slice_edges_are_whole_pixels_and_shared() {
        let base = UiQuad::fill([10.3, 20.6], [100.45, 61.2], UiColor::WHITE);
        for scale in [0.7037f32, 1.0, 1.3333, 2.0] {
            let quads = nine_slice(base, 7, [0.0, 0.0, 1.0, 1.0], [20.0; 4], [64, 64], scale);
            // At 2× the 40 px top + bottom insets exceed the 61 px height: no middle row.
            let expected = if 40.0 * scale <= 61.0 { 9 } else { 6 };
            assert_eq!(quads.len(), expected, "scale {scale}");
            for q in &quads {
                for v in [q.pos[0], q.pos[1], q.size[0], q.size[1]] {
                    assert_eq!(v, v.round(), "slice edge {v} not on a pixel at scale {scale}");
                }
            }
            // Row-major: each slice starts exactly where its left / upper neighbour ends.
            for j in 0..quads.len() / 3 {
                for i in 1..3 {
                    let (a, b) = (quads[j * 3 + i - 1], quads[j * 3 + i]);
                    assert_eq!(a.pos[0] + a.size[0], b.pos[0]);
                    assert_eq!(a.uv[2], b.uv[0]);
                }
            }
            // Total area equals the snapped rect: no gaps, no overlaps.
            let area: f32 = quads.iter().map(|q| q.size[0] * q.size[1]).sum();
            assert_eq!(
                area,
                (111.0 - 10.0) * (82.0 - 21.0),
                "10.3..110.75 x 20.6..81.8 snapped"
            );
        }
        // A rect smaller than the insets shrinks them instead of overlapping slices.
        let tiny = UiQuad::fill([0.0, 0.0], [30.0, 30.0], UiColor::WHITE);
        let quads = nine_slice(tiny, 7, [0.0, 0.0, 1.0, 1.0], [20.0; 4], [64, 64], 1.0);
        let area: f32 = quads.iter().map(|q| q.size[0] * q.size[1]).sum();
        assert_eq!(area, 900.0);
    }

    #[test]
    fn shadow_geometry_covers_three_sigma() {
        let mut q = UiQuad::fill([100.0, 100.0], [50.0, 20.0], UiColor::BLACK);
        q.blur = 8.0;
        q.shadow_offset = [0.0, 4.0];
        let inst = instance_of(&q);
        assert_eq!(inst.kind_layer[0], 5.0);
        assert_eq!(inst.kind_layer[3], 4.0, "sigma = blur / 2");
        assert_eq!(inst.kind_layer[1], 12.0, "margin = 3 sigma");
        assert_eq!(inst.pos, [88.0, 88.0]);
        assert_eq!(inst.size, [74.0, 44.0]);
        assert_eq!(&inst.uv[..2], &[0.0, 4.0]);
        // A clip just outside the caster still keeps the shadow (its blur reaches in).
        assert!(q.clipped([152.0, 100.0, 50.0, 50.0]).is_some());
        assert!(q.clipped([170.0, 100.0, 50.0, 50.0]).is_none());
    }

    #[test]
    fn border_quads_are_not_gradients() {
        // Regression: `color2 != color` used to classify border quads as linear gradients,
        // painting the border colour as a translucent wash over the whole rect.
        let mut border = UiQuad::fill([0.0, 0.0], [64.0, 32.0], UiColor::TRANSPARENT);
        border.color2 = UiColor::WHITE;
        border.border_width = [2.0; 4];
        let inst = instance_of(&border);
        assert_eq!(inst.kind_layer[0], 0.0, "border quad renders as an SDF rect");
        let mut grad = UiQuad::fill([0.0, 0.0], [64.0, 32.0], UiColor::BLACK);
        grad.color2 = UiColor::WHITE;
        grad.gradient = true;
        assert_eq!(instance_of(&grad).kind_layer[0], 4.0, "explicit gradient");
    }

    #[test]
    fn batch_ranges_split_only_on_image_changes() {
        let img = |id| UiQuad {
            texture: TextureRef::Image(id),
            ..UiQuad::fill([0.0, 0.0], [1.0, 1.0], UiColor::WHITE)
        };
        let rect = || UiQuad::fill([0.0, 0.0], [1.0, 1.0], UiColor::WHITE);
        // panel, icon A, text, panel, icon A, panel, icon B, glyph
        let quads = vec![rect(), img(1), rect(), rect(), img(1), rect(), img(2), rect()];
        let (mut instances, mut ranges) = (Vec::new(), Vec::new());
        batch_ranges(&quads, &mut instances, &mut ranges);
        assert_eq!(instances.len(), 8);
        assert_eq!(
            ranges,
            vec![
                BatchRange {
                    image: Some(1),
                    first: 0,
                    count: 6
                },
                BatchRange {
                    image: Some(2),
                    first: 6,
                    count: 2
                },
            ]
        );
    }

    #[test]
    fn batching_splits_on_images() {
        let mut quads = vec![
            UiQuad::fill([0.0, 0.0], [1.0, 1.0], UiColor::WHITE),
            UiQuad {
                texture: TextureRef::Image(3),
                ..UiQuad::fill([0.0, 0.0], [1.0, 1.0], UiColor::WHITE)
            },
            UiQuad {
                texture: TextureRef::Image(3),
                ..UiQuad::fill([0.0, 0.0], [1.0, 1.0], UiColor::WHITE)
            },
            UiQuad {
                texture: TextureRef::MonoAtlas(1),
                ..UiQuad::fill([0.0, 0.0], [1.0, 1.0], UiColor::WHITE)
            },
        ];
        quads[3].uv = [0.0, 0.0, 0.5, 0.5];
        let batches = batch(&quads);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1].image, Some(3));
        assert_eq!(batches[1].instances.len(), 2);
        assert_eq!(batches[2].image, None);
    }
}
