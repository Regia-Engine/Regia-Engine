// SPDX-License-Identifier: MPL-2.0
//! Isolated group opacity and masks (ADR-2866, `ui-parity-roadmap` T11).
//!
//! A node with [`GroupOpacity`] draws its whole subtree (its own background, shadow, children)
//! into a pooled offscreen layer and composites that layer once at the group's opacity, so two
//! overlapping opaque children of a half-transparent group show one even tint instead of a darker
//! overlap. [`RoundedClip`] clips a node's children to its rounded rect with an antialiased edge,
//! and [`UiMask`] multiplies the node's subtree by an image's alpha. Both masks draw through the
//! same layers: the content is drawn into the layer, the mask is multiplied in with a
//! destination-in pass, and the layer is composited into its parent. Nested clips therefore
//! intersect: an inner layer is composited into its parent layer before the parent's own mask.
//!
//! Extract ([`crate::extract`]) wraps a group's quads between a `TextureRef::LayerBegin` marker
//! and a `TextureRef::LayerComposite` quad (plus a `TextureRef::LayerMask` quad for masks), moves
//! the group's quads into layer space (the layer's top-left at the origin), and flattens their
//! depth to the group's (a group is a stacking context). The draw ([`crate::render`]) keeps a
//! stack of pooled layer textures. A layer is skipped when it would change nothing: opacity 1
//! with no mask, or a single primitive (whose alpha takes the opacity). Above the layer memory
//! budget ([`UiLayerBudget`]) a group draws non-isolated, with the warning
//! `ui_group_layer_budget`.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::draw::{NO_CLIP, TextureRef, UiQuad, intersect};

/// Texture format of the offscreen layers (premultiplied linear colour, hardware sRGB encode).
pub const LAYER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Bytes per layer texel ([`LAYER_FORMAT`]).
pub const LAYER_BYTES_PER_PIXEL: u64 = 4;

/// Layer textures are allocated in steps of this many pixels per side, so groups of similar size
/// share pooled textures.
pub const LAYER_SIZE_STEP: u32 = 64;

/// Default [`UiLayerBudget::max_bytes`]: 64 MiB, about eight 1080p layers.
pub const DEFAULT_LAYER_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Pool slots unused for this many window draws are freed.
pub const LAYER_IDLE_DRAWS: u64 = 600;

/// Draws the node and its whole subtree as one isolated group at this opacity (0..=1): the
/// subtree is rendered into an offscreen layer and the layer is blended once. Unlike
/// [`crate::Opacity`], which multiplies every primitive's alpha, overlapping children do not show
/// through each other. A group is also a stacking context: `ZOffset` inside it orders only within
/// the group. Has no effect on a [`crate::UiRoot`] entity itself (put it on a child node).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct GroupOpacity(pub f32);

/// Clips the node's children to its rounded rect (the corner radii of its [`crate::Border`] or
/// theme), with an antialiased edge. Implies [`crate::ClipChildren`] for layout and hit testing;
/// pointer hits in the cut-away corners miss the children.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundedClip;

/// How a mask image is placed in the node's rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskFit {
    /// Stretched over the whole rect.
    #[default]
    Stretch,
    /// Scaled to fit inside the rect, aspect kept, centred; the rest of the rect is hidden.
    Contain,
    /// Scaled to cover the rect, aspect kept, centred; the overflow is cropped.
    Cover,
    /// Repeated from the rect's top-left at the image's pixel size times the UI scale.
    Tile,
}

impl MaskFit {
    /// Parses the `.ui.ron` / agent spelling (`stretch`, `contain`, `cover`, `tile`).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stretch" => Some(Self::Stretch),
            "contain" => Some(Self::Contain),
            "cover" => Some(Self::Cover),
            "tile" => Some(Self::Tile),
            _ => None,
        }
    }

    /// The spelling [`Self::parse`] accepts.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Stretch => "stretch",
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::Tile => "tile",
        }
    }
}

/// Multiplies the node and its subtree by the alpha of a registered image (`image` is the
/// renderer image id, as [`crate::UiImage`]); zero-alpha regions hide the content and are not hit
/// by the pointer. Outside the placed image (see [`MaskFit`]) everything is hidden.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiMask {
    /// Renderer image id whose alpha is the mask.
    pub image: u64,
    /// Sub-rect of the image (u0, v0, u1, v1).
    pub uv: [f32; 4],
    /// Placement in the node's rect.
    pub fit: MaskFit,
}

impl UiMask {
    /// The whole of `image`, stretched over the node.
    #[must_use]
    pub fn new(image: u64) -> Self {
        Self {
            image,
            uv: [0.0, 0.0, 1.0, 1.0],
            fit: MaskFit::Stretch,
        }
    }

    /// With a placement mode.
    #[must_use]
    pub fn with_fit(mut self, fit: MaskFit) -> Self {
        self.fit = fit;
        self
    }
}

/// Names a node's mask image by project asset path instead of a renderer id: `path` is the manifest
/// path of a cooked image or texture (`assets/ui/star.png`). The UI plugin's
/// [`crate::ui_image_assets::resolve_ui_image_assets`] loads it in the background (ADR-3148),
/// registers it with the renderer and inserts the [`UiMask`]; until then, and when the path cannot be
/// resolved (one warning, `ui_image_*` codes), the node draws unmasked. [`resolve_mask_assets`] is the
/// older frame-thread loader (cooked RGBA8 images only), kept for callers that schedule it.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UiMaskAsset {
    /// Manifest path of the image.
    pub path: String,
    /// Placement in the node's rect.
    pub fit: MaskFit,
}

impl UiMaskAsset {
    /// The image at `path`, stretched over the node.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            fit: MaskFit::Stretch,
        }
    }

    /// With a placement mode.
    #[must_use]
    pub fn with_fit(mut self, fit: MaskFit) -> Self {
        self.fit = fit;
        self
    }
}

/// Upper bound on offscreen layer memory per draw list. Groups past it draw non-isolated (their
/// opacity multiplies every primitive, masks fall back to rect clips) and the warning
/// `ui_group_layer_budget` is logged.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiLayerBudget {
    /// Bytes of layer texels the groups of one draw list may need together.
    pub max_bytes: u64,
}

impl Default for UiLayerBudget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_LAYER_BUDGET_BYTES,
        }
    }
}

/// What extract decided for the groups of the current draw list (counts across all roots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct LayerPlan {
    /// Groups drawn through a layer.
    pub layers: u32,
    /// Groups at opacity 1 with no mask: no layer needed.
    pub skipped_opaque: u32,
    /// Groups of a single primitive: its alpha took the opacity, no layer needed.
    pub skipped_single: u32,
    /// Groups that drew nothing (empty, fully clipped, or opacity 0).
    pub skipped_empty: u32,
    /// Groups over [`UiLayerBudget`], drawn non-isolated.
    pub over_budget: u32,
    /// Layer texels the plan needs, in bytes (before pool rounding).
    pub bytes: u64,
}

impl LayerPlan {
    /// Adds another plan's counts.
    pub fn add(&mut self, other: &LayerPlan) {
        self.layers += other.layers;
        self.skipped_opaque += other.skipped_opaque;
        self.skipped_single += other.skipped_single;
        self.skipped_empty += other.skipped_empty;
        self.over_budget += other.over_budget;
        self.bytes += other.bytes;
    }
}

/// What the layer pool did in the last window draw ([`crate::UiRenderer::layer_stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct LayerStats {
    /// Layers composited in the last window draw.
    pub drawn: u32,
    /// Deepest layer nesting in that draw.
    pub max_depth: u32,
    /// Mask passes in that draw.
    pub mask_passes: u32,
    /// Pooled layer textures alive.
    pub pool_slots: u32,
    /// Bytes of the pooled layer textures.
    pub pool_bytes: u64,
    /// Layer textures ever created (stays flat once the pool is warm).
    pub created_total: u64,
    /// Layer textures freed after going unused for `LAYER_IDLE_DRAWS` draws.
    pub freed_total: u64,
    /// Backdrop-blur quads inside layers (drawn without the blur).
    pub backdrops_in_layers: u32,
}

/// The mask a layer applies, in screen px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MaskSpec {
    /// The masked rect (x, y, w, h): the node's rect.
    pub rect: [f32; 4],
    /// Rounded-clip radii (tl, tr, br, bl); all zero = no rounded clip.
    pub radii: [f32; 4],
    /// Image mask: id, uv sub-rect, placed rect (x, y, w, h) and whether it tiles.
    pub image: Option<(u64, [f32; 4], [f32; 4], bool)>,
}

/// One group to wrap (screen px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GroupSpec {
    /// Composite opacity (the group's, times any non-isolated ancestor opacity).
    pub opacity: f32,
    /// Mask, if any.
    pub mask: Option<MaskSpec>,
    /// Where the layer may draw (x, y, w, h): the clip the composite is drawn under.
    pub limit: [f32; 4],
    /// The group's draw depth (every wrapped quad takes it).
    pub z: f32,
}

/// The decision [`wrap_group`] took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WrapOutcome {
    /// Drawn through a layer of this many bytes.
    Layer(u64),
    /// Opacity 1, no mask.
    Opaque,
    /// One primitive: opacity folded into its alpha.
    Single,
    /// Nothing visible: the quads were dropped.
    Empty,
    /// Over budget: drawn non-isolated.
    OverBudget,
}

impl WrapOutcome {
    /// Records this outcome in `plan`.
    pub fn record(self, plan: &mut LayerPlan) {
        match self {
            Self::Layer(bytes) => {
                plan.layers += 1;
                plan.bytes += bytes;
            }
            Self::Opaque => plan.skipped_opaque += 1,
            Self::Single => plan.skipped_single += 1,
            Self::Empty => plan.skipped_empty += 1,
            Self::OverBudget => plan.over_budget += 1,
        }
    }
}

/// Nesting depth change of a quad in a wrapped list: `+1` after a layer begin, `-1` at a
/// composite (the composite itself draws in the outer layer).
fn depth_step(q: &UiQuad) -> i32 {
    match q.texture {
        TextureRef::LayerBegin => 1,
        TextureRef::LayerComposite => -1,
        _ => 0,
    }
}

/// Calls `f` for every quad of `quads` that draws directly at this level (not inside a nested
/// layer): ordinary quads, nested begin markers and nested composites.
fn for_each_top_level(quads: &mut [UiQuad], mut f: impl FnMut(&mut UiQuad)) {
    let mut depth = 0i32;
    for q in quads {
        let step = depth_step(q);
        if step < 0 {
            depth += step;
        }
        if depth == 0 {
            f(q);
        }
        if step > 0 {
            depth += step;
        }
    }
}

/// Whether a quad is a layer marker or mask (not a visible primitive of its own).
fn is_marker(q: &UiQuad) -> bool {
    matches!(q.texture, TextureRef::LayerBegin | TextureRef::LayerMask { .. })
}

/// Wraps `out[start..]` (the group's quads, in paint order) as one group: sorts it by depth,
/// flattens the depth, and either leaves it as is (opacity 1, no mask), folds the opacity into a
/// single primitive, drops it (nothing visible), draws it non-isolated (over `budget_left`
/// bytes), or moves it into layer space between a `LayerBegin` marker and a `LayerComposite`
/// quad. Returns the decision; the caller charges [`WrapOutcome::Layer`] bytes to its budget.
pub(crate) fn wrap_group(
    out: &mut Vec<UiQuad>,
    start: usize,
    spec: &GroupSpec,
    budget_left: u64,
) -> WrapOutcome {
    if start >= out.len() {
        return WrapOutcome::Empty;
    }
    let opacity = if spec.opacity.is_finite() {
        spec.opacity.clamp(0.0, 1.0)
    } else {
        1.0
    };
    if opacity <= 0.0 {
        out.truncate(start);
        return WrapOutcome::Empty;
    }
    // A group is a stacking context: its own `ZOffset`s order only inside it.
    out[start..].sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));
    for q in &mut out[start..] {
        q.z = spec.z;
    }
    let mut primitives = 0usize;
    let mut union: Option<[f32; 4]> = None;
    for_each_top_level(&mut out[start..], |q| {
        if is_marker(q) {
            return;
        }
        primitives += 1;
        let visible = intersect(q.bounds(), q.clip);
        if visible[2] > 0.0 && visible[3] > 0.0 {
            union = Some(match union {
                None => visible,
                Some(u) => {
                    let x0 = u[0].min(visible[0]);
                    let y0 = u[1].min(visible[1]);
                    let x1 = (u[0] + u[2]).max(visible[0] + visible[2]);
                    let y1 = (u[1] + u[3]).max(visible[1] + visible[3]);
                    [x0, y0, x1 - x0, y1 - y0]
                }
            });
        }
    });
    let mut limit = spec.limit;
    if let Some(mask) = &spec.mask {
        limit = intersect(limit, mask.rect);
    }
    let Some(union) = union else {
        out.truncate(start);
        return WrapOutcome::Empty;
    };
    let b = intersect(union, limit);
    // Whole device pixels, so glyphs stay on their texel grid inside the layer.
    let (x0, y0) = (b[0].floor(), b[1].floor());
    let (x1, y1) = ((b[0] + b[2]).ceil(), (b[1] + b[3]).ceil());
    if b[2] <= 0.0 || b[3] <= 0.0 || x1 <= x0 || y1 <= y0 {
        out.truncate(start);
        return WrapOutcome::Empty;
    }
    if spec.mask.is_none() {
        if opacity >= 1.0 {
            return WrapOutcome::Opaque;
        }
        if primitives == 1 {
            for_each_top_level(&mut out[start..], |q| {
                if !is_marker(q) {
                    q.alpha *= opacity;
                }
            });
            return WrapOutcome::Single;
        }
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let bytes = (w as u64) * (h as u64) * LAYER_BYTES_PER_PIXEL;
    if bytes > budget_left {
        // Non-isolated: every primitive takes the opacity; masks degrade to their rect.
        let clip = spec.mask.map(|m| m.rect);
        for_each_top_level(&mut out[start..], |q| {
            if is_marker(q) {
                return;
            }
            q.alpha *= opacity;
            if let Some(c) = clip {
                // A quad wholly outside keeps its slot (a nested composite still closes its
                // layer) but draws nothing.
                match q.clipped(c) {
                    Some(inside) => *q = inside,
                    None => q.alpha = 0.0,
                }
            }
        });
        // Rebuilt lists repeat the decision every frame: log the 1st, 2nd, 4th, 8th... time.
        static OVER_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seen = OVER_BUDGET.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if seen.is_power_of_two() {
            tracing::warn!(
            target: "regia_ui",
            code = "ui_group_layer_budget",
            seen,
            "ui group layer of {w}x{h} px ({bytes} bytes) is over the layer budget ({budget_left} bytes left); drawn non-isolated"
            );
        }
        return WrapOutcome::OverBudget;
    }
    // Layer space: the layer's top-left is the origin.
    for_each_top_level(&mut out[start..], |q| {
        q.pos[0] -= x0;
        q.pos[1] -= y0;
        q.clip[0] -= x0;
        q.clip[1] -= y0;
    });
    let begin = UiQuad {
        texture: TextureRef::LayerBegin,
        z: spec.z,
        ..UiQuad::fill([x0, y0], [w, h], crate::color::UiColor::TRANSPARENT)
    };
    out.insert(start, begin);
    if let Some(mask) = &spec.mask {
        out.push(mask_quad(mask, [x0, y0], [w, h], spec.z));
    }
    out.push(UiQuad {
        texture: TextureRef::LayerComposite,
        alpha: opacity,
        z: spec.z,
        clip: [x0, y0, w, h],
        ..UiQuad::fill([x0, y0], [w, h], crate::color::UiColor::WHITE)
    });
    WrapOutcome::Layer(bytes)
}

/// The destination-in quad that multiplies a layer of `size` (whose top-left is `origin` on
/// screen) by `mask`: it covers the whole layer, so everything outside the mask goes to zero.
fn mask_quad(mask: &MaskSpec, origin: [f32; 2], size: [f32; 2], z: f32) -> UiQuad {
    let r = mask.rect;
    let rounded = mask.radii.iter().any(|r| *r > 0.0);
    let (image, uv, placed, tile) = match mask.image {
        Some((id, uv, placed, tile)) => (Some(id), uv, placed, tile),
        None => (None, [0.0, 0.0, 1.0, 1.0], r, false),
    };
    UiQuad {
        texture: TextureRef::LayerMask { image, tile },
        uv,
        corner_radius: mask.radii,
        // The masked rect (x0, y0, x1, y1), layer space; `rounded` selects the SDF.
        border_width: [
            r[0] - origin[0],
            r[1] - origin[1],
            r[0] + r[2] - origin[0],
            r[1] + r[3] - origin[1],
        ],
        // The placed image (x0, y0, x1, y1), layer space.
        color2: crate::color::UiColor([
            placed[0] - origin[0],
            placed[1] - origin[1],
            placed[0] + placed[2] - origin[0],
            placed[1] + placed[3] - origin[1],
        ]),
        // `blur` carries the rounded flag to the instance (see `instance_of`).
        blur: if rounded { 1.0 } else { 0.0 },
        z,
        clip: NO_CLIP,
        ..UiQuad::fill([0.0, 0.0], size, crate::color::UiColor::WHITE)
    }
}

/// Where a mask image of `image_px` pixels lands in `rect` (x, y, w, h) under `fit`; for
/// [`MaskFit::Tile`] the first tile. `scale` is the UI scale (tile size).
#[must_use]
pub fn mask_placement(
    fit: MaskFit,
    rect: [f32; 4],
    image_px: Option<[u32; 2]>,
    uv: [f32; 4],
    scale: f32,
) -> [f32; 4] {
    let px = image_px.unwrap_or([1, 1]);
    let iw = (px[0].max(1) as f32 * (uv[2] - uv[0]).abs()).max(1e-3);
    let ih = (px[1].max(1) as f32 * (uv[3] - uv[1]).abs()).max(1e-3);
    match fit {
        MaskFit::Stretch => rect,
        MaskFit::Contain | MaskFit::Cover => {
            let k = if fit == MaskFit::Contain {
                (rect[2] / iw).min(rect[3] / ih)
            } else {
                (rect[2] / iw).max(rect[3] / ih)
            };
            let (w, h) = (iw * k, ih * k);
            [rect[0] + (rect[2] - w) * 0.5, rect[1] + (rect[3] - h) * 0.5, w, h]
        }
        MaskFit::Tile => {
            let s = if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            };
            [rect[0], rect[1], iw * s, ih * s]
        }
    }
}

/// One mask shape a node's pointer hits must pass (screen px, as `ComputedNode::rect`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MaskShape {
    /// Inside a rounded rect (x, y, w, h) with radii (tl, tr, br, bl).
    Rounded {
        /// The rect.
        rect: [f32; 4],
        /// Corner radii in px.
        radii: [f32; 4],
    },
    /// Where an image mask's alpha is above zero.
    Image {
        /// Renderer image id.
        image: u64,
        /// The masked rect (outside it nothing is hit).
        rect: [f32; 4],
        /// Where the image lands ([`mask_placement`]).
        placed: [f32; 4],
        /// Image sub-rect.
        uv: [f32; 4],
        /// Repeats past `placed`.
        tile: bool,
    },
}

/// The masks above a node (and its own [`UiMask`]), written after layout for hit testing.
/// Absent when no mask applies.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiClipMasks(pub Vec<MaskShape>);

/// Signed distance from `p` to a rounded rect (negative inside); `radii` tl, tr, br, bl.
#[must_use]
pub fn rounded_rect_distance(p: [f32; 2], rect: [f32; 4], radii: [f32; 4]) -> f32 {
    let half = [rect[2] * 0.5, rect[3] * 0.5];
    let c = [rect[0] + half[0], rect[1] + half[1]];
    let q0 = [p[0] - c[0], p[1] - c[1]];
    let r_max = half[0].min(half[1]).max(0.0);
    let r = match (q0[0] < 0.0, q0[1] < 0.0) {
        (true, true) => radii[0],
        (false, true) => radii[1],
        (false, false) => radii[2],
        (true, false) => radii[3],
    }
    .clamp(0.0, r_max);
    let q = [q0[0].abs() - (half[0] - r), q0[1].abs() - (half[1] - r)];
    let outside = (q[0].max(0.0).powi(2) + q[1].max(0.0).powi(2)).sqrt();
    outside + q[0].max(q[1]).min(0.0) - r
}

impl UiClipMasks {
    /// Whether a pointer at `p` passes every mask. `alpha` reads an image's alpha (0..=1) at a
    /// uv; `None` (no CPU copy, e.g. a texture view) counts as opaque.
    pub fn admits(&self, p: [f32; 2], alpha: impl Fn(u64, [f32; 2]) -> Option<f32>) -> bool {
        self.0.iter().all(|m| match *m {
            MaskShape::Rounded { rect, radii } => rounded_rect_distance(p, rect, radii) <= 0.0,
            MaskShape::Image {
                image,
                rect,
                placed,
                uv,
                tile,
            } => {
                let inside =
                    |r: [f32; 4]| p[0] >= r[0] && p[1] >= r[1] && p[0] < r[0] + r[2] && p[1] < r[1] + r[3];
                if !inside(rect) {
                    return false;
                }
                let mut t = [
                    (p[0] - placed[0]) / placed[2].max(1e-3),
                    (p[1] - placed[1]) / placed[3].max(1e-3),
                ];
                if tile {
                    t = [t[0].rem_euclid(1.0), t[1].rem_euclid(1.0)];
                } else if !(0.0..1.0).contains(&t[0]) || !(0.0..1.0).contains(&t[1]) {
                    return false;
                }
                let at = [uv[0] + (uv[2] - uv[0]) * t[0], uv[1] + (uv[3] - uv[1]) * t[1]];
                alpha(image, at).is_none_or(|a| a > 0.0)
            }
        })
    }
}

/// A CPU copy of an image's alpha for mask hit testing, at most [`AlphaMap::MAX_SIDE`] texels a
/// side (nearest-sampled from the source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlphaMap {
    width: u32,
    height: u32,
    alpha: Vec<u8>,
}

impl AlphaMap {
    /// Largest side kept.
    pub const MAX_SIDE: u32 = 256;

    /// From tightly packed RGBA8 rows; `None` when `rgba` does not hold `width × height` pixels.
    #[must_use]
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Self> {
        let (w, h) = (width.max(1), height.max(1));
        let expected = u64::from(w).checked_mul(u64::from(h))?.checked_mul(4)?;
        if usize::try_from(expected).ok()? != rgba.len() {
            return None;
        }
        let (mw, mh) = (w.min(Self::MAX_SIDE), h.min(Self::MAX_SIDE));
        let mut alpha = Vec::with_capacity((mw * mh) as usize);
        for y in 0..mh {
            let sy = (u64::from(y) * u64::from(h) / u64::from(mh)) as usize;
            for x in 0..mw {
                let sx = (u64::from(x) * u64::from(w) / u64::from(mw)) as usize;
                alpha.push(rgba[(sy * w as usize + sx) * 4 + 3]);
            }
        }
        Some(Self {
            width: mw,
            height: mh,
            alpha,
        })
    }

    /// Alpha (0..=1) at `uv` (clamped to the image), nearest texel.
    #[must_use]
    pub fn sample(&self, uv: [f32; 2]) -> f32 {
        let x = ((uv[0].clamp(0.0, 1.0) * self.width as f32) as u32).min(self.width - 1);
        let y = ((uv[1].clamp(0.0, 1.0) * self.height as f32) as u32).min(self.height - 1);
        f32::from(self.alpha[(y * self.width + x) as usize]) / 255.0
    }
}

/// Run condition of [`update_clip_masks`]: only a moved node, a changed mask or radius, a
/// reparented node or a new image size can change what it writes. The walk touches every node and
/// allocates, so a settled UI must not pay for it each frame.
pub(crate) fn clip_masks_stale(
    changed: Query<
        (),
        Or<(
            Changed<crate::node::ComputedNode>,
            Changed<RoundedClip>,
            Changed<UiMask>,
            Changed<crate::node::Border>,
            Changed<crate::theme::Themed>,
            Changed<ChildOf>,
        )>,
    >,
    mut removed_clips: RemovedComponents<RoundedClip>,
    mut removed_masks: RemovedComponents<UiMask>,
    renderer: Option<Res<crate::render::SharedUiRenderer>>,
    mut images_seen: Local<u64>,
) -> bool {
    let removed = removed_clips.read().count() + removed_masks.read().count() > 0;
    let images = renderer.map_or(0, |r| r.lock().images_generation());
    let new_images = images != *images_seen;
    *images_seen = images;
    removed || new_images || !changed.is_empty()
}

/// `PostUpdate`, after layout: writes [`UiClipMasks`] on every node under a [`RoundedClip`] or
/// [`UiMask`], and removes it where none applies any more. A root's own clip and mask do not count:
/// they do not draw (see [`GroupOpacity`]), so they must not cut its children's hits either.
pub fn update_clip_masks(world: &mut World) {
    let _span = tracing::trace_span!("ui_update_clip_masks").entered();
    let any = world
        .query_filtered::<Entity, Or<(With<RoundedClip>, With<UiMask>, With<UiClipMasks>)>>()
        .iter(world)
        .next()
        .is_some();
    if !any {
        return;
    }
    let image_sizes: Option<crate::render::SharedUiRenderer> =
        world.get_resource::<crate::render::SharedUiRenderer>().cloned();
    let roots: Vec<Entity> = world
        .query_filtered::<Entity, With<crate::node::UiRoot>>()
        .iter(world)
        .collect();
    let mut writes: Vec<(Entity, Vec<MaskShape>)> = Vec::new();
    let mut stack: Vec<(Entity, Vec<MaskShape>)> = roots.into_iter().map(|r| (r, Vec::new())).collect();
    while let Some((entity, inherited)) = stack.pop() {
        let mut own = inherited.clone();
        let computed = world.get::<crate::node::ComputedNode>(entity).copied();
        let is_root = world.get::<crate::node::UiRoot>(entity).is_some();
        if let (Some(mask), Some(c), false) = (world.get::<UiMask>(entity), computed, is_root) {
            let size = image_sizes.as_ref().and_then(|r| r.lock().image_size(mask.image));
            own.push(MaskShape::Image {
                image: mask.image,
                rect: c.rect,
                placed: mask_placement(mask.fit, c.rect, size, mask.uv, c.scale),
                uv: mask.uv,
                tile: mask.fit == MaskFit::Tile,
            });
        }
        writes.push((entity, own.clone()));
        let mut for_children = own;
        if !is_root
            && world.get::<RoundedClip>(entity).is_some()
            && let Some(c) = computed
        {
            let radii = node_radii(world, entity, c.scale);
            for_children.push(MaskShape::Rounded { rect: c.rect, radii });
        }
        if let Some(children) = world.get::<Children>(entity) {
            for child in children.iter() {
                stack.push((child, for_children.clone()));
            }
        }
    }
    for (entity, shapes) in writes {
        let current = world.get::<UiClipMasks>(entity);
        match (shapes.is_empty(), current) {
            (true, None) => {}
            (true, Some(_)) => {
                world.entity_mut(entity).remove::<UiClipMasks>();
            }
            (false, Some(c)) if c.0 == shapes => {}
            (false, _) => {
                world.entity_mut(entity).insert(UiClipMasks(shapes));
            }
        }
    }
}

/// A node's corner radii in physical px: the theme's radius, else its [`crate::Border`]'s.
#[must_use]
pub fn node_radii(world: &World, entity: Entity, scale: f32) -> [f32; 4] {
    world
        .get::<crate::theme::Themed>(entity)
        .and_then(|t| t.radius)
        .map(|r| [r * scale; 4])
        .or_else(|| {
            world
                .get::<crate::node::Border>(entity)
                .map(|b| b.radius.map(|r| r * scale))
        })
        .unwrap_or([0.0; 4])
}

/// The renderer image id of a mask asset: derived from the asset id, with the top bit set to stay
/// out of the small ids games pick by hand.
fn mask_image_id(asset: regia_asset::AssetId) -> u64 {
    crate::ui_image_assets::asset_image_id(asset)
}

/// Why a mask asset could not be loaded: the agent error reason and the message.
pub(crate) type MaskAssetError = (&'static str, String);

/// Loads the cooked RGBA8 image at `path` and registers it with the renderer; returns its image id.
/// An image already registered is not read again. Runs on the frame thread: a mask is small, and
/// `.ui.ron` files load the same way.
pub(crate) fn load_mask_image(world: &mut World, path: &str) -> Result<u64, MaskAssetError> {
    let _span = tracing::debug_span!("ui_mask_asset").entered();
    let Some(renderer) = world.get_resource::<crate::render::SharedUiRenderer>().cloned() else {
        return Err((
            "ui_mask_asset_failed",
            "the UI renderer is not installed".to_owned(),
        ));
    };
    let Some(mut server) = world.get_resource_mut::<regia_asset::AssetServer>() else {
        return Err(("ui_mask_asset_failed", "no asset server is running".to_owned()));
    };
    let Some(id) = server.id_for_path(path) else {
        return Err((
            "ui_mask_asset_not_found",
            format!("no manifest entry for `{path}`"),
        ));
    };
    let image_id = mask_image_id(id);
    if renderer.lock().image_size(image_id).is_some() {
        return Ok(image_id);
    }
    let handle = server.load_image(id).map_err(|e| {
        (
            "ui_mask_asset_failed",
            format!("`{path}` is not a cooked RGBA8 image ({e}); cook it with `compress: Some(false)` in its .meta"),
        )
    })?;
    let Some(image) = server.images().get(&handle) else {
        return Err(("ui_mask_asset_failed", format!("`{path}` did not stay loaded")));
    };
    renderer
        .lock()
        .queue_image_rgba(image_id, image.width, image.height, image.rgba8.clone());
    Ok(image_id)
}

/// Turns each new or changed [`UiMaskAsset`] into a [`UiMask`], loading on the calling thread. The UI
/// plugin no longer schedules it (ADR-3148): the background loader resolves mask assets.
pub fn resolve_mask_assets(world: &mut World) {
    let pending: Vec<(Entity, UiMaskAsset)> = world
        .query_filtered::<(Entity, &UiMaskAsset), Changed<UiMaskAsset>>()
        .iter(world)
        .map(|(e, a)| (e, a.clone()))
        .collect();
    for (entity, asset) in pending {
        match load_mask_image(world, &asset.path) {
            Ok(image) => {
                world
                    .entity_mut(entity)
                    .insert(UiMask::new(image).with_fit(asset.fit));
            }
            Err((reason, message)) => {
                tracing::warn!(target: "regia_ui", code = "ui_mask_asset_failed", reason, "ui mask: {message}");
                world.entity_mut(entity).remove::<UiMask>();
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// GPU layer pool.
// ---------------------------------------------------------------------------------------------

/// One pooled layer texture with its sampling and drawing bind groups.
pub(crate) struct LayerSlot {
    /// Kept alive for the view.
    _texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
    /// Group 3 binding that samples this layer (the composite).
    pub(crate) sample_bg: wgpu::BindGroup,
    /// Kept alive for the bind group.
    _screen_buf: wgpu::Buffer,
    /// Group 0 binding for drawing into this layer (viewport = the texture size).
    pub(crate) screen_bg: wgpu::BindGroup,
    size: [u32; 2],
    busy: bool,
    last_used: u64,
}

/// Pooled offscreen layers, reused across groups and frames.
#[derive(Default)]
pub(crate) struct LayerPool {
    slots: Vec<LayerSlot>,
    created: u64,
    freed: u64,
    draws: u64,
}

/// Device objects a new slot needs.
pub(crate) struct SlotInputs {
    pub image_bgl: wgpu::BindGroupLayout,
    pub screen_bgl: wgpu::BindGroupLayout,
    pub sampler: wgpu::Sampler,
    pub paper_white: f32,
}

/// The pooled size for a layer of `need` px: rounded up to [`LAYER_SIZE_STEP`], at most `max`.
#[must_use]
pub fn pooled_size(need: [u32; 2], max: u32) -> [u32; 2] {
    need.map(|n| {
        n.max(1)
            .div_ceil(LAYER_SIZE_STEP)
            .saturating_mul(LAYER_SIZE_STEP)
            .min(max.max(1))
    })
}

impl LayerPool {
    /// Takes a free slot at least `need` px (smallest first), creating one when none fits.
    pub(crate) fn acquire(
        &mut self,
        device: &regia_render::RenderDevice,
        need: [u32; 2],
        inputs: &SlotInputs,
    ) -> usize {
        let max = device.device.limits().max_texture_dimension_2d;
        let need = need.map(|n| n.clamp(1, max.max(1)));
        let best = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.busy && s.size[0] >= need[0] && s.size[1] >= need[1])
            .min_by_key(|(_, s)| u64::from(s.size[0]) * u64::from(s.size[1]))
            .map(|(i, _)| i);
        let index = match best {
            Some(i) => i,
            None => {
                self.slots
                    .push(Self::create(device, pooled_size(need, max), inputs));
                self.created += 1;
                self.slots.len() - 1
            }
        };
        let draws = self.draws;
        let slot = &mut self.slots[index];
        slot.busy = true;
        slot.last_used = draws;
        index
    }

    /// Returns a slot to the pool.
    pub(crate) fn release(&mut self, index: usize) {
        if let Some(s) = self.slots.get_mut(index) {
            s.busy = false;
        }
    }

    /// The slot at `index`.
    pub(crate) fn slot(&self, index: usize) -> Option<&LayerSlot> {
        self.slots.get(index)
    }

    /// Ends a window draw: frees slots idle for [`LAYER_IDLE_DRAWS`] draws.
    pub(crate) fn end_draw(&mut self) {
        self.draws += 1;
        let draws = self.draws;
        let before = self.slots.len();
        self.slots
            .retain(|s| s.busy || draws.saturating_sub(s.last_used) < LAYER_IDLE_DRAWS);
        self.freed += (before - self.slots.len()) as u64;
    }

    /// Fills the pool fields of `stats`.
    pub(crate) fn fill_stats(&self, stats: &mut LayerStats) {
        stats.pool_slots = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
        stats.pool_bytes = self
            .slots
            .iter()
            .map(|s| u64::from(s.size[0]) * u64::from(s.size[1]) * LAYER_BYTES_PER_PIXEL)
            .sum();
        stats.created_total = self.created;
        stats.freed_total = self.freed;
    }

    fn create(device: &regia_render::RenderDevice, size: [u32; 2], inputs: &SlotInputs) -> LayerSlot {
        let _span = tracing::debug_span!("ui_layer_create", w = size[0], h = size[1]).entered();
        // Counted: the hitch accounting sees UI creates too.
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let texture = dev.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui group layer"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: LAYER_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sample_bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui group layer sample bg"),
            layout: &inputs.image_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&inputs.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
        });
        // Layers hold premultiplied linear colour (hardware sRGB, output mode 0) and blend in
        // plain linear light (blend gamma 1): the window's blend remap applies once, at the
        // composite.
        let mut uniforms = [0.0f32; 12];
        uniforms[..5].copy_from_slice(&[size[0] as f32, size[1] as f32, 0.0, inputs.paper_white, 1.0]);
        let screen_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui group layer screen uniforms"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device
            .queue
            .write_buffer(&screen_buf, 0, bytemuck::cast_slice(&uniforms));
        let screen_bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui group layer screen bg"),
            layout: &inputs.screen_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_buf.as_entire_binding(),
            }],
        });
        LayerSlot {
            _texture: texture,
            view,
            sample_bg,
            _screen_buf: screen_buf,
            screen_bg,
            size,
            busy: false,
            last_used: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent methods (book/agents/ui-group-layers.md).
// ---------------------------------------------------------------------------------------------

use crate::agent_bridge::UiBridgeError;
use serde_json::{Value, json};

/// The `.ui.ron` / agent spelling of a node's clip.
fn clip_name(world: &World, entity: Entity) -> &'static str {
    if world.get::<RoundedClip>(entity).is_some() {
        "rounded"
    } else if world.get::<crate::node::ClipChildren>(entity).is_some() {
        "rect"
    } else {
        "none"
    }
}

fn group_json(world: &World, entity: Entity) -> Value {
    json!({
        "node": entity.to_bits(),
        "group_opacity": world.get::<GroupOpacity>(entity).map(|g| g.0),
        "clip": clip_name(world, entity),
        "mask": world.get::<UiMask>(entity).map(|m| json!({
            "image": m.image,
            "fit": m.fit.name(),
            "asset": world.get::<UiMaskAsset>(entity).map(|a| a.path.clone()),
        })),
    })
}

/// `ui.set_group {node|name|text, group_opacity?, clip?, mask?}`: sets a node's isolated group
/// opacity (`null` removes it), its clip (`none`, `rect`, `rounded`) and its image mask
/// (`{image, fit?}` or `{asset, fit?}`, `null` removes it). Omitted fields are left alone. On a
/// root, which cannot be a group, the reply carries `warning: "ui_group_on_root"`.
pub(crate) fn agent_set_group(
    world: &mut World,
    params: &Value,
    resolve: fn(&mut World, &Value) -> Result<Entity, UiBridgeError>,
) -> Result<Value, UiBridgeError> {
    let entity = resolve(world, params)?;
    let opacity = match params.get("group_opacity") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => match v.as_f64().filter(|o| (0.0..=1.0).contains(o)) {
            Some(o) => Some(Some(o as f32)),
            None => {
                return Err(UiBridgeError::invalid(
                    "ui_group_opacity_out_of_range",
                    format!("group_opacity {v} is not a number in 0..=1"),
                    "use null to remove the group",
                ));
            }
        },
    };
    let clip = match params.get("clip") {
        None => None,
        Some(Value::String(s)) if matches!(s.as_str(), "none" | "rect" | "rounded") => Some(s.clone()),
        Some(v) => {
            return Err(UiBridgeError::invalid(
                "ui_clip_unknown",
                format!("clip {v} is not one of none, rect, rounded"),
                "rounded clips the children to the node's corner radii",
            ));
        }
    };
    let mask = match params.get("mask") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(m) => {
            let image = m.get("image").and_then(Value::as_u64);
            let asset = m.get("asset").and_then(Value::as_str);
            if image.is_some() == asset.is_some() {
                return Err(UiBridgeError::invalid(
                    "ui_mask_image_required",
                    "mask needs exactly one of `image`, a registered image id, and `asset`, a cooked image's manifest path",
                    "{\"image\": 7, \"fit\": \"contain\"} or {\"asset\": \"assets/ui/star.png\"}",
                ));
            }
            let fit = match m.get("fit") {
                None | Some(Value::Null) => MaskFit::Stretch,
                Some(f) => match f.as_str().and_then(MaskFit::parse) {
                    Some(fit) => fit,
                    None => {
                        return Err(UiBridgeError::invalid(
                            "ui_mask_fit_unknown",
                            format!("mask fit {f} is not one of stretch, contain, cover, tile"),
                            "stretch is the default",
                        ));
                    }
                },
            };
            let (image, asset) = match asset {
                Some(path) => {
                    let image = load_mask_image(world, path).map_err(|(reason, message)| {
                        if reason == "ui_mask_asset_not_found" {
                            UiBridgeError::not_found(reason, message, "assets.query lists the manifest paths")
                        } else {
                            UiBridgeError::invalid(
                                reason,
                                message,
                                "set `compress: Some(false)` in the image's .meta",
                            )
                        }
                    })?;
                    (image, Some(UiMaskAsset::new(path).with_fit(fit)))
                }
                None => (image.unwrap_or_default(), None),
            };
            Some(Some((UiMask::new(image).with_fit(fit), asset)))
        }
    };
    let before = group_json(world, entity);
    let on_root = world.get::<crate::node::UiRoot>(entity).is_some();
    let mut e = world.entity_mut(entity);
    match opacity {
        Some(Some(o)) => {
            e.insert(GroupOpacity(o));
        }
        Some(None) => {
            e.remove::<GroupOpacity>();
        }
        None => {}
    }
    match clip.as_deref() {
        Some("none") => {
            e.remove::<(RoundedClip, crate::node::ClipChildren)>();
        }
        Some("rect") => {
            e.remove::<RoundedClip>();
            e.insert(crate::node::ClipChildren);
        }
        Some(_) => {
            e.insert((RoundedClip, crate::node::ClipChildren));
        }
        None => {}
    }
    match mask {
        Some(Some((m, asset))) => {
            e.insert(m);
            match asset {
                Some(asset) => {
                    e.insert(asset);
                }
                None => {
                    e.remove::<UiMaskAsset>();
                }
            }
        }
        Some(None) => {
            e.remove::<(UiMask, UiMaskAsset)>();
        }
        None => {}
    }
    // Group components are not in the damage filters: force the rebuild.
    if let Some(mut damage) = world.get_resource_mut::<crate::extract::UiDamageSet>() {
        damage.full = true;
    }
    if let Some(mut damage) = world.get_resource_mut::<crate::extract::UiDamage>() {
        damage.mark();
    }
    let after = group_json(world, entity);
    let mut reply = json!({ "before": before, "after": after, "changed": before != after });
    let groups_something =
        !after["group_opacity"].is_null() || !after["mask"].is_null() || after["clip"] == "rounded";
    if on_root && groups_something {
        reply["warning"] = json!("ui_group_on_root");
    }
    Ok(reply)
}

/// `ui.layer_stats {}`: what extract planned for the groups (layers, skips, over-budget
/// fallbacks, bytes), what the last window draw did with the layer pool, the budget, and how
/// many nodes have a group, a rounded clip or a mask.
pub(crate) fn agent_layer_stats(world: &mut World) -> Value {
    let (plan, stats) = world
        .get_resource::<crate::render::SharedUiRenderer>()
        .map(|r| {
            let r = r.lock();
            (r.layer_plan(), r.layer_stats())
        })
        .unwrap_or_default();
    let budget = world.get_resource::<UiLayerBudget>().copied().unwrap_or_default();
    let groups = world.query::<&GroupOpacity>().iter(world).count();
    let rounded = world.query::<&RoundedClip>().iter(world).count();
    let masks = world.query::<&UiMask>().iter(world).count();
    json!({
        "nodes": { "groups": groups, "rounded_clips": rounded, "masks": masks },
        "plan": plan,
        "draw": stats,
        "budget_bytes": budget.max_bytes,
    })
}

/// `ui.set_scale_mode {root?, mode?, reference?, match_width?, dpi_curve?}`: sets the scale mode
/// of one root (`root`, an entity id) or of every root, and/or the project DPI curve
/// (`[[dpi, scale], ...]`, `null` removes it). Returns each root's mode and resolved scale.
pub(crate) fn agent_set_scale_mode(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let reference = match params.get("reference") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let pair = v.as_array().filter(|a| a.len() == 2).and_then(|a| {
                let w = a[0].as_f64().filter(|x| x.is_finite() && *x >= 1.0)?;
                let h = a[1].as_f64().filter(|x| x.is_finite() && *x >= 1.0)?;
                Some([w as f32, h as f32])
            });
            match pair {
                Some(p) => Some(p),
                None => {
                    return Err(UiBridgeError::invalid(
                        "ui_reference_invalid",
                        format!("reference {v} is not [width, height] of at least 1"),
                        "e.g. [320, 180] for a pixel-art UI",
                    ));
                }
            }
        }
    };
    let mode = match params.get("mode").and_then(Value::as_str) {
        None => None,
        Some("constant_pixel") => Some(crate::scale::ScaleMode::ConstantPixelSize),
        Some("constant_physical") => Some(crate::scale::ScaleMode::ConstantPhysicalSize),
        Some("scale_with_screen") => Some(crate::scale::ScaleMode::ScaleWithScreen {
            reference: reference.unwrap_or([1920.0, 1080.0]),
            match_width: params
                .get("match_width")
                .and_then(Value::as_f64)
                .map_or(0.5, |m| m.clamp(0.0, 1.0) as f32),
        }),
        Some("integer") => match reference {
            Some(reference) => Some(crate::scale::ScaleMode::IntegerScale { reference }),
            None => {
                return Err(UiBridgeError::invalid(
                    "ui_reference_required",
                    "mode integer needs `reference`, the UI's size in UI px",
                    "e.g. {\"mode\": \"integer\", \"reference\": [320, 180]}",
                ));
            }
        },
        Some(other) => {
            return Err(UiBridgeError::invalid(
                "ui_scale_mode_unknown",
                format!(
                    "mode `{other}` is not one of constant_pixel, constant_physical, scale_with_screen, integer"
                ),
                "integer is the pixel-art mode",
            ));
        }
    };
    let curve = match params.get("dpi_curve") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let points: Option<Vec<[f32; 2]>> = v.as_array().map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let p = p.as_array()?;
                        Some([p.first()?.as_f64()? as f32, p.get(1)?.as_f64()? as f32])
                    })
                    .collect()
            });
            let curve = crate::scale::DpiCurve {
                points: points.unwrap_or_default(),
            };
            let shape_ok = v.as_array().is_some_and(|a| a.len() == curve.points.len());
            if let Err(e) = curve.validate().and_then(|()| {
                if shape_ok {
                    Ok(())
                } else {
                    Err("every point must be [dpi, scale]".to_owned())
                }
            }) {
                return Err(UiBridgeError::invalid(
                    "ui_dpi_curve_invalid",
                    format!("dpi_curve: {e}"),
                    "e.g. [[96, 1.0], [144, 1.25], [192, 1.75]]",
                ));
            }
            Some(Some(curve))
        }
    };
    let roots: Vec<Entity> = match params.get("root").and_then(Value::as_u64) {
        Some(bits) => {
            let e = Entity::try_from_bits(bits).filter(|e| world.get::<crate::node::UiRoot>(*e).is_some());
            match e {
                Some(e) => vec![e],
                None => {
                    return Err(UiBridgeError::not_found(
                        "ui_root_not_found",
                        format!("{bits} is not a UI root"),
                        "omit `root` to set every root; ui.query lists node ids",
                    ));
                }
            }
        }
        None => world
            .query_filtered::<Entity, With<crate::node::UiRoot>>()
            .iter(world)
            .collect(),
    };
    if let Some(curve) = curve {
        world
            .get_resource_or_insert_with(crate::scale::UiScaleSettings::default)
            .dpi_curve = curve;
    }
    if let Some(mode) = mode {
        for &r in &roots {
            if let Some(mut root) = world.get_mut::<crate::node::UiRoot>(r) {
                root.scale_mode = mode;
            }
        }
    }
    if let Some(mut dirty) = world.get_resource_mut::<crate::layout::LayoutDirty>() {
        dirty.0 = true;
    }
    let scales = world
        .get_resource::<crate::scale::UiScales>()
        .copied()
        .unwrap_or_default();
    let settings = world
        .get_resource::<crate::scale::UiScaleSettings>()
        .cloned()
        .unwrap_or_default();
    let out: Vec<Value> = roots
        .iter()
        .filter_map(|&r| {
            let root = world.get::<crate::node::UiRoot>(r)?;
            let scale = settings.adjust(
                &root.scale_mode,
                scales.scale_factor,
                scales.scale_of(&root.scale_mode),
            );
            Some(json!({
                "root": r.to_bits(),
                "mode": format!("{:?}", root.scale_mode),
                "scale": scale,
                "letterbox": root.scale_mode.letterbox(scales.window_size, scale),
            }))
        })
        .collect();
    Ok(json!({
        "roots": out,
        "dpi_curve": settings.dpi_curve.map(|c| c.points),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::UiColor;

    fn red(pos: [f32; 2], size: [f32; 2]) -> UiQuad {
        UiQuad::fill(pos, size, UiColor([1.0, 0.0, 0.0, 1.0]))
    }

    fn spec(opacity: f32) -> GroupSpec {
        GroupSpec {
            opacity,
            mask: None,
            limit: NO_CLIP,
            z: 3.0,
        }
    }

    #[test]
    fn two_primitives_at_half_opacity_get_a_layer_in_layer_space() {
        let mut out = vec![red([10.4, 20.0], [30.0, 10.0]), red([25.0, 22.0], [30.0, 10.0])];
        let outcome = wrap_group(&mut out, 0, &spec(0.5), u64::MAX);
        // Bounds 10.4..55 x 20..32 → whole pixels 10..55 x 20..32 = 45 x 12.
        assert_eq!(outcome, WrapOutcome::Layer(45 * 12 * 4));
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].texture, TextureRef::LayerBegin);
        assert_eq!((out[0].pos, out[0].size), ([10.0, 20.0], [45.0, 12.0]));
        assert!((out[1].pos[0] - 0.4).abs() < 1e-5 && out[1].pos[1] == 0.0);
        assert_eq!(out[2].pos, [15.0, 2.0]);
        let composite = out[3];
        assert_eq!(composite.texture, TextureRef::LayerComposite);
        assert_eq!(
            (composite.pos, composite.size, composite.alpha),
            ([10.0, 20.0], [45.0, 12.0], 0.5)
        );
        assert!(out.iter().all(|q| q.z == 3.0));
        // The children keep their own alpha: the opacity is applied once, at the composite.
        assert_eq!((out[1].alpha, out[2].alpha), (1.0, 1.0));
    }

    #[test]
    fn opaque_single_and_empty_groups_need_no_layer() {
        let mut out = vec![red([0.0, 0.0], [5.0, 5.0]), red([2.0, 2.0], [5.0, 5.0])];
        assert_eq!(wrap_group(&mut out, 0, &spec(1.0), u64::MAX), WrapOutcome::Opaque);
        assert_eq!(out.len(), 2);
        let mut out = vec![red([0.0, 0.0], [5.0, 5.0])];
        assert_eq!(
            wrap_group(&mut out, 0, &spec(0.25), u64::MAX),
            WrapOutcome::Single
        );
        assert_eq!((out.len(), out[0].alpha), (1, 0.25));
        let mut out = vec![red([0.0, 0.0], [5.0, 5.0])];
        assert_eq!(wrap_group(&mut out, 0, &spec(0.0), u64::MAX), WrapOutcome::Empty);
        assert!(out.is_empty());
        let mut out = vec![red([0.0, 0.0], [5.0, 5.0]), red([1.0, 0.0], [5.0, 5.0])];
        let clipped = GroupSpec {
            limit: [100.0, 100.0, 10.0, 10.0],
            ..spec(0.5)
        };
        assert_eq!(wrap_group(&mut out, 0, &clipped, u64::MAX), WrapOutcome::Empty);
        let mut out: Vec<UiQuad> = Vec::new();
        assert_eq!(wrap_group(&mut out, 0, &spec(0.5), u64::MAX), WrapOutcome::Empty);
    }

    #[test]
    fn over_budget_draws_non_isolated_with_folded_opacity() {
        let mut out = vec![red([0.0, 0.0], [10.0, 10.0]), red([5.0, 0.0], [10.0, 10.0])];
        assert_eq!(wrap_group(&mut out, 0, &spec(0.5), 100), WrapOutcome::OverBudget);
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|q| q.alpha == 0.5 && q.texture == TextureRef::Rect)
        );
        assert_eq!(out[1].pos, [5.0, 0.0], "screen space, not layer space");
    }

    #[test]
    fn nested_layers_translate_once_and_keep_their_inner_space() {
        let mut out = vec![red([100.0, 100.0], [10.0, 10.0])];
        let inner_start = out.len();
        out.push(red([120.0, 100.0], [10.0, 10.0]));
        out.push(red([125.0, 105.0], [10.0, 10.0]));
        let inner = wrap_group(&mut out, inner_start, &spec(0.5), u64::MAX);
        assert!(matches!(inner, WrapOutcome::Layer(_)));
        // Inner content: layer space of the inner layer (origin 120, 100).
        assert_eq!(out[2].pos, [0.0, 0.0]);
        let outer = wrap_group(&mut out, 0, &spec(0.5), u64::MAX);
        assert!(matches!(outer, WrapOutcome::Layer(_)));
        // [outer begin, red, inner begin, inner a, inner b, inner composite, outer composite]
        assert_eq!(out.len(), 7);
        assert_eq!(out[1].pos, [0.0, 0.0], "outer origin 100, 100");
        assert_eq!(
            out[3].pos,
            [0.0, 0.0],
            "inner content untouched by the outer move"
        );
        assert_eq!(out[4].pos, [5.0, 5.0]);
        assert_eq!(out[5].texture, TextureRef::LayerComposite);
        assert_eq!(out[5].pos, [20.0, 0.0], "inner composite moved into outer space");
        assert_eq!(out[6].pos, [100.0, 100.0]);
    }

    #[test]
    fn a_masked_group_at_full_opacity_still_gets_a_layer_and_mask_quad() {
        let mut out = vec![red([0.0, 0.0], [40.0, 40.0])];
        let s = GroupSpec {
            mask: Some(MaskSpec {
                rect: [0.0, 0.0, 40.0, 40.0],
                radii: [10.0; 4],
                image: None,
            }),
            ..spec(1.0)
        };
        assert!(matches!(
            wrap_group(&mut out, 0, &s, u64::MAX),
            WrapOutcome::Layer(_)
        ));
        assert_eq!(out.len(), 4);
        assert!(matches!(
            out[2].texture,
            TextureRef::LayerMask {
                image: None,
                tile: false
            }
        ));
        assert_eq!(out[2].border_width, [0.0, 0.0, 40.0, 40.0]);
        assert_eq!(out[2].corner_radius, [10.0; 4]);
    }

    #[test]
    fn rounded_distance_cuts_the_corner() {
        let rect = [0.0, 0.0, 40.0, 40.0];
        assert!(
            rounded_rect_distance([1.0, 1.0], rect, [10.0; 4]) > 0.0,
            "cut corner"
        );
        assert!(
            rounded_rect_distance([20.0, 1.0], rect, [10.0; 4]) < 0.0,
            "edge middle"
        );
        assert!(
            rounded_rect_distance([1.0, 1.0], rect, [0.0; 4]) < 0.0,
            "square corner"
        );
        assert!(rounded_rect_distance([41.0, 20.0], rect, [0.0; 4]) > 0.0);
    }

    #[test]
    fn mask_placement_fits() {
        let rect = [0.0, 0.0, 100.0, 50.0];
        let full = [0.0, 0.0, 1.0, 1.0];
        assert_eq!(
            mask_placement(MaskFit::Stretch, rect, Some([10, 10]), full, 1.0),
            rect
        );
        assert_eq!(
            mask_placement(MaskFit::Contain, rect, Some([10, 10]), full, 1.0),
            [25.0, 0.0, 50.0, 50.0]
        );
        assert_eq!(
            mask_placement(MaskFit::Cover, rect, Some([10, 10]), full, 1.0),
            [0.0, -25.0, 100.0, 100.0]
        );
        assert_eq!(
            mask_placement(MaskFit::Tile, rect, Some([10, 10]), full, 2.0),
            [0.0, 0.0, 20.0, 20.0]
        );
    }

    #[test]
    fn clip_masks_admit_by_shape_and_alpha() {
        let masks = UiClipMasks(vec![MaskShape::Image {
            image: 1,
            rect: [0.0, 0.0, 20.0, 10.0],
            placed: [0.0, 0.0, 20.0, 10.0],
            uv: [0.0, 0.0, 1.0, 1.0],
            tile: false,
        }]);
        // Left half transparent, right half opaque.
        let map = AlphaMap::from_rgba(2, 1, &[0, 0, 0, 0, 0, 0, 0, 255]).expect("map");
        let alpha = |_: u64, uv: [f32; 2]| Some(map.sample(uv));
        assert!(!masks.admits([5.0, 5.0], alpha));
        assert!(masks.admits([15.0, 5.0], alpha));
        assert!(!masks.admits([25.0, 5.0], alpha), "outside the masked rect");
        assert!(
            masks.admits([5.0, 5.0], |_, _| None),
            "no CPU copy counts as opaque"
        );
        assert_eq!(pooled_size([1, 65], 100), [64, 100]);
    }
}
