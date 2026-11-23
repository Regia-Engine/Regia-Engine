// SPDX-License-Identifier: MPL-2.0
//! [`HudLayer`]: screen anchors and the usual HUD element kinds as [`Draw2dList`] calls (P-06).
//!
//! The element set was derived from Direct Order 27's CPU HUD raster
//! (`shell-regia/game/src/hud/raster.rs`, commit c9513623f, read as requirements only): panels,
//! edge-fading scrims and veils, halos, strips, bars, lines, corner brackets, rings and discs,
//! outlines, crosshairs, plus marks, pointers, text left / centre / right of its anchor, labelled
//! blocks and image cells. Each one is a few instances of the instanced draw list, so a full HUD
//! is one draw call and, with dirty regions on, only the tiles of the elements that changed are
//! redrawn. Filled triangles (triangle and diamond marks, light cones) have no instance kind in
//! `Draw2d` yet; a pointer is drawn as a chevron of two segments.

use regia_core::math::{Color, Vec2};
use regia_ui::text::{ShapedText, TextSystem};

use super::{Draw2dImage, Draw2dList, PxRect};

/// Where on the screen an element sits: one of nine anchor points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HudAnchor {
    /// Top-left corner.
    #[default]
    TopLeft,
    /// Middle of the top edge.
    Top,
    /// Top-right corner.
    TopRight,
    /// Middle of the left edge.
    Left,
    /// Screen centre.
    Center,
    /// Middle of the right edge.
    Right,
    /// Bottom-left corner.
    BottomLeft,
    /// Middle of the bottom edge.
    Bottom,
    /// Bottom-right corner.
    BottomRight,
}

impl HudAnchor {
    /// The anchor as fractions of the screen: `(0, 0)` top-left, `(1, 1)` bottom-right.
    #[must_use]
    pub fn fraction(self) -> Vec2 {
        let (x, y) = match self {
            Self::TopLeft => (0.0, 0.0),
            Self::Top => (0.5, 0.0),
            Self::TopRight => (1.0, 0.0),
            Self::Left => (0.0, 0.5),
            Self::Center => (0.5, 0.5),
            Self::Right => (1.0, 0.5),
            Self::BottomLeft => (0.0, 1.0),
            Self::Bottom => (0.5, 1.0),
            Self::BottomRight => (1.0, 1.0),
        };
        Vec2::new(x, y)
    }

    /// The anchor point of a `screen`-sized target.
    #[must_use]
    pub fn point(self, screen: Vec2) -> Vec2 {
        self.fraction() * screen
    }
}

/// How a text run sits on its `x`: starting at it, centred on it or ending at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextAlign {
    /// The run starts at `x`.
    #[default]
    Left,
    /// The run is centred on `x`.
    Center,
    /// The run ends at `x`.
    Right,
}

impl TextAlign {
    /// The left edge of a run `width` pixels wide aligned on `x`.
    #[must_use]
    pub fn left_edge(self, x: f32, width: f32) -> f32 {
        match self {
            Self::Left => x,
            Self::Center => x - width * 0.5,
            Self::Right => x - width,
        }
    }
}

/// HUD drawing over a [`Draw2dList`] for one frame: anchors, a UI scale and the HUD element kinds.
/// Positions are physical pixels (origin top-left, +Y down); [`Self::place`] scales sizes and
/// margins by the UI scale.
#[derive(Debug)]
pub struct HudLayer<'a> {
    list: &'a mut Draw2dList,
    screen: Vec2,
    scale: f32,
}

impl<'a> HudLayer<'a> {
    /// HUD drawing into `list` for a `width` × `height` pixel target, UI scale 1.
    pub fn new(list: &'a mut Draw2dList, width: u32, height: u32) -> Self {
        Self {
            list,
            screen: Vec2::new(width as f32, height as f32),
            scale: 1.0,
        }
    }

    /// The same, with UI scale `scale` (sizes and margins given to [`Self::place`] are
    /// multiplied by it; a non-positive or non-finite scale counts as 1).
    #[must_use]
    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self
    }

    /// The target size in pixels.
    #[must_use]
    pub fn screen(&self) -> Vec2 {
        self.screen
    }

    /// The UI scale.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The draw list (for calls this helper does not wrap).
    pub fn list(&mut self) -> &mut Draw2dList {
        self.list
    }

    /// A `size` box at `anchor`, `margin` in from the anchor's edges (a right anchor's margin
    /// moves it left, a bottom anchor's up; a centre anchor ignores the margin on that axis
    /// except as a plain offset). Size and margin are in UI units (times the UI scale).
    #[must_use]
    pub fn place(&self, anchor: HudAnchor, margin: Vec2, size: Vec2) -> PxRect {
        let f = anchor.fraction();
        let size = size * self.scale;
        let margin = margin * self.scale;
        let inward = |f: f32| if f > 0.75 { -1.0 } else { 1.0 };
        let x = f.x * (self.screen.x - size.x) + margin.x * inward(f.x);
        let y = f.y * (self.screen.y - size.y) + margin.y * inward(f.y);
        PxRect::new(x, y, size.x, size.y)
    }

    /// A uniform translucent fill with corners rounded by `radius`.
    pub fn panel(&mut self, rect: PxRect, fill: Color, radius: f32) {
        self.list.push_box(rect, fill, radius);
    }

    /// A fill whose edge fades out over `feather` pixels (a scrim behind a block of text).
    pub fn scrim(&mut self, rect: PxRect, color: Color, feather: f32) {
        self.list.push_soft_box(rect, color, feather * 0.5, feather);
    }

    /// A soft ellipse-like veil: strongest inside, fading over `soft` (0-1) of its shorter
    /// half-size toward the edge.
    pub fn veil(&mut self, rect: PxRect, color: Color, soft: f32) {
        let half = rect.w.min(rect.h) * 0.5;
        let feather = (soft.clamp(0.0, 1.0) * half * 2.0).max(1.0);
        self.list.push_soft_box(rect, color, half, feather);
    }

    /// A soft dark halo centred on `center` with radii `rx`, `ry`: full colour in the middle,
    /// nothing at the rim (keeps a short readout legible over bright ground).
    pub fn halo(&mut self, center: Vec2, rx: f32, ry: f32, color: Color) {
        let rect = PxRect::new(center.x - rx, center.y - ry, rx * 2.0, ry * 2.0);
        let r = rx.min(ry);
        self.list.push_soft_box(rect, color, r, r);
    }

    /// A faint horizontal band: `color` in the middle, transparent at the left and right edges.
    pub fn strip(&mut self, rect: PxRect, color: Color) {
        let half = rect.w * 0.5;
        let clear = Color::new(color.r, color.g, color.b, 0.0);
        self.list
            .push_gradient_box(PxRect::new(rect.x, rect.y, half, rect.h), clear, color, 0.0, true);
        self.list.push_gradient_box(
            PxRect::new(rect.x + half, rect.y, rect.w - half, rect.h),
            color,
            clear,
            0.0,
            true,
        );
    }

    /// A bar: `back` over the whole rect, `fill` over the left `frac` (0-1) of it, inset by one
    /// pixel. A zero fraction draws only the back.
    pub fn bar(&mut self, rect: PxRect, frac: f32, fill: Color, back: Color) {
        let radius = rect.h * 0.25;
        self.list.push_box(rect, back, radius);
        let frac = if frac.is_finite() {
            frac.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let w = (rect.w - 2.0) * frac;
        if w > 0.0 {
            self.list.push_box(
                PxRect::new(rect.x + 1.0, rect.y + 1.0, w, rect.h - 2.0),
                fill,
                (radius - 1.0).max(0.0),
            );
        }
    }

    /// A line from `a` to `b`, `width` pixels wide.
    pub fn line(&mut self, a: Vec2, b: Vec2, width: f32, color: Color) {
        self.list.push_segment(a, b, width, color);
    }

    /// Four corner brackets around `center`: each corner of a square of half-size `half` gets two
    /// legs `leg` pixels long (at most `half`).
    pub fn bracket(&mut self, center: Vec2, half: f32, leg: f32, width: f32, color: Color) {
        let leg = leg.min(half);
        for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let corner = center + Vec2::new(sx * half, sy * half);
            self.list
                .push_segment(corner, corner - Vec2::new(sx * leg, 0.0), width, color);
            self.list
                .push_segment(corner, corner - Vec2::new(0.0, sy * leg), width, color);
        }
    }

    /// A ring of radius `r` (outer edge) and thickness `width` around `center`.
    pub fn ring(&mut self, center: Vec2, r: f32, width: f32, color: Color) {
        let rect = PxRect::new(center.x - r, center.y - r, r * 2.0, r * 2.0);
        self.list.push_outline(rect, color, r, width);
    }

    /// A filled disc of radius `r`.
    pub fn disc(&mut self, center: Vec2, r: f32, color: Color) {
        let rect = PxRect::new(center.x - r, center.y - r, r * 2.0, r * 2.0);
        self.list.push_box(rect, color, r);
    }

    /// The outline of `rect`, `width` pixels thick, corners rounded by `radius`.
    pub fn outline(&mut self, rect: PxRect, width: f32, color: Color, radius: f32) {
        self.list.push_outline(rect, color, radius, width);
    }

    /// A crosshair: four arms from `gap` to `gap + len` pixels out of `center`, plus a centre dot
    /// when `dot` is above zero (its radius).
    pub fn crosshair(&mut self, center: Vec2, gap: f32, len: f32, width: f32, dot: f32, color: Color) {
        for d in [Vec2::X, -Vec2::X, Vec2::Y, -Vec2::Y] {
            self.list
                .push_segment(center + d * gap, center + d * (gap + len), width, color);
        }
        if dot > 0.0 {
            self.disc(center, dot, color);
        }
    }

    /// A plus mark of half-size `s`.
    pub fn plus(&mut self, center: Vec2, s: f32, width: f32, color: Color) {
        self.list.push_segment(
            center - Vec2::new(s, 0.0),
            center + Vec2::new(s, 0.0),
            width,
            color,
        );
        self.list.push_segment(
            center - Vec2::new(0.0, s),
            center + Vec2::new(0.0, s),
            width,
            color,
        );
    }

    /// A pointer: a chevron whose tip is `len` pixels from `at` along `dir` (normalized here; a
    /// zero direction points up).
    pub fn pointer(&mut self, at: Vec2, dir: Vec2, len: f32, width: f32, color: Color) {
        let d = dir.try_normalize().unwrap_or(-Vec2::Y);
        let n = Vec2::new(-d.y, d.x);
        let tip = at + d * len;
        self.list
            .push_round_segment(tip, at + n * len * 0.35, width, color);
        self.list
            .push_round_segment(tip, at - n * len * 0.35, width, color);
    }

    /// A shaped run with its top at `at.y`, aligned on `at.x`.
    pub fn text(
        &mut self,
        fonts: &mut TextSystem,
        run: &ShapedText,
        at: Vec2,
        align: TextAlign,
        color: Color,
    ) {
        let x = align.left_edge(at.x, run.size[0]);
        self.list.push_text(fonts, run, Vec2::new(x, at.y), color);
    }

    /// A labelled block: a filled box with the run centred in it.
    pub fn block(
        &mut self,
        rect: PxRect,
        fill: Color,
        fonts: &mut TextSystem,
        run: &ShapedText,
        color: Color,
    ) {
        self.list.push_box(rect, fill, 2.0);
        let at = Vec2::new(rect.x + rect.w * 0.5, rect.y + (rect.h - run.size[1]) * 0.5);
        self.text(fonts, run, at, TextAlign::Center, color);
    }

    /// The `uv` cell of image slot `image` stretched over `rect`, tinted (an icon or sprite).
    pub fn image(&mut self, image: Draw2dImage, rect: PxRect, uv: [f32; 4], tint: Color) {
        self.list.push_image(image, rect, uv, tint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_place_boxes_inside_the_screen() {
        let mut list = Draw2dList::new();
        let hud = HudLayer::new(&mut list, 1000, 500).with_scale(2.0);
        let size = Vec2::new(50.0, 20.0);
        let m = Vec2::new(10.0, 5.0);
        assert_eq!(
            hud.place(HudAnchor::TopLeft, m, size),
            PxRect::new(20.0, 10.0, 100.0, 40.0)
        );
        assert_eq!(
            hud.place(HudAnchor::BottomRight, m, size),
            PxRect::new(880.0, 450.0, 100.0, 40.0)
        );
        assert_eq!(
            hud.place(HudAnchor::Center, Vec2::ZERO, size),
            PxRect::new(450.0, 230.0, 100.0, 40.0)
        );
        assert_eq!(
            hud.place(HudAnchor::Top, m, size),
            PxRect::new(470.0, 10.0, 100.0, 40.0)
        );
        assert_eq!(HudAnchor::Right.point(hud.screen()), Vec2::new(1000.0, 250.0));
        assert_eq!(
            HudLayer::new(&mut Draw2dList::new(), 1, 1)
                .with_scale(f32::NAN)
                .scale(),
            1.0
        );
    }

    #[test]
    fn element_kinds_are_draw2d_instances() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        let mut hud = HudLayer::new(&mut list, 640, 360);
        let c = Vec2::new(320.0, 180.0);
        hud.crosshair(c, 4.0, 8.0, 2.0, 1.5, Color::WHITE);
        hud.bracket(c, 30.0, 8.0, 2.0, Color::WHITE);
        hud.bar(
            PxRect::new(10.0, 10.0, 100.0, 10.0),
            0.5,
            Color::GREEN,
            Color::BLACK,
        );
        hud.bar(
            PxRect::new(10.0, 30.0, 100.0, 10.0),
            0.0,
            Color::GREEN,
            Color::BLACK,
        );
        hud.strip(PxRect::new(0.0, 100.0, 200.0, 20.0), Color::BLACK);
        hud.ring(c, 40.0, 2.0, Color::WHITE);
        hud.pointer(c, Vec2::ZERO, 10.0, 2.0, Color::WHITE);
        hud.plus(c, 5.0, 1.0, Color::WHITE);
        list.end_frame();
        let s = list.stats();
        assert_eq!(s.segments, 4 + 8 + 2 + 2, "{s:?}");
        assert_eq!(s.boxes, 1 + 2 + 1 + 2, "dot, bar, empty bar, strip: {s:?}");
        assert_eq!(s.outlines, 1, "{s:?}");
    }

    #[test]
    fn text_aligns_on_its_anchor() {
        assert_eq!(TextAlign::Left.left_edge(100.0, 40.0), 100.0);
        assert_eq!(TextAlign::Center.left_edge(100.0, 40.0), 80.0);
        assert_eq!(TextAlign::Right.left_edge(100.0, 40.0), 60.0);
    }
}
