// SPDX-License-Identifier: MPL-2.0
//! Speaker-anchored caption placement (`docs/specs/56-dialogue-and-subtitles.md` § 12, ADR-3234).
//!
//! A caption can follow a world position (an entity's head). [`place_anchored`] is pure: it takes
//! each caption's projected anchor, its box size and the safe rectangle, and returns where the box
//! goes:
//! - on screen: centered above the anchor, clamped inside the safe rectangle;
//! - off screen (the anchor projects outside the safe rectangle): the box is clamped to the edge
//!   nearest the anchor and an [`EdgeArrow`] points toward it;
//! - behind the camera (or no room): the caption falls back to the bottom band, stacked upward.
//!
//! No two placed boxes overlap: later captions (by id) slide up or down by a box height until they
//! are clear, and fall back to the band when no spot is free inside the safe rectangle.

use regia_app::caption::CaptionId;
use regia_app::prelude::*;
use regia_core::glam::{Mat4, Vec3, Vec4};

use super::board::Rect;

/// Where captions go (a captions setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionPlacement {
    /// The classic bottom (or top) band.
    #[default]
    BottomBand,
    /// Next to the speaker, with the band as the fallback.
    Anchored,
}

impl CaptionPlacement {
    /// The setting name used by the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BottomBand => "bottom_band",
            Self::Anchored => "anchored",
        }
    }

    /// Parses [`Self::as_str`].
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "bottom_band" => Some(Self::BottomBand),
            "anchored" => Some(Self::Anchored),
            _ => None,
        }
    }
}

/// The dialogue-caption settings resource: karaoke highlighting and placement. Off / bottom band
/// by default. Kept apart from `CaptionOptions` so that struct stays as it was.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DialogueCaptionSettings {
    /// Highlight the words as they are spoken.
    pub karaoke: bool,
    /// Anchored or bottom band.
    pub placement: CaptionPlacement,
}

/// A screen projection of a world point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projected {
    /// Pixel in the viewport (y down); meaningless when `behind`.
    pub px: [f32; 2],
    /// True when the point is at or behind the camera plane.
    pub behind: bool,
}

/// Projects a world point with `view_proj` into a `viewport` of `(w, h)` pixels (y down).
pub fn project_point(view_proj: &Mat4, viewport: (f32, f32), world: Vec3) -> Projected {
    let c = *view_proj * Vec4::new(world.x, world.y, world.z, 1.0);
    if !c.w.is_finite() || c.w <= 1e-6 {
        return Projected {
            px: [0.0, 0.0],
            behind: true,
        };
    }
    let ndc_x = c.x / c.w;
    let ndc_y = c.y / c.w;
    Projected {
        px: [(ndc_x * 0.5 + 0.5) * viewport.0, (0.5 - ndc_y * 0.5) * viewport.1],
        behind: false,
    }
}

/// One caption to place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnchorInput {
    /// The caption.
    pub id: CaptionId,
    /// The projected anchor, `None` when the speaker is behind the camera or unknown.
    pub anchor: Option<[f32; 2]>,
    /// The box size in pixels.
    pub size: [f32; 2],
}

/// How a caption was placed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnchorMode {
    /// Above the on-screen speaker.
    OnScreen,
    /// Clamped to an edge with an arrow toward the off-screen speaker.
    EdgeArrow(EdgeArrow),
    /// In the bottom band (speaker behind the camera, unknown, or no free spot).
    BottomBand,
}

/// An arrow at the safe-rectangle edge pointing toward an off-screen speaker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeArrow {
    /// Arrow tip position on the safe-rectangle edge.
    pub pos: [f32; 2],
    /// Direction in radians clockwise from up.
    pub angle: f32,
}

/// The result for one caption.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnchorPlacement {
    /// The caption.
    pub id: CaptionId,
    /// Its box.
    pub rect: Rect,
    /// How it was placed.
    pub mode: AnchorMode,
}

/// Gap in pixels between the anchor and the box, and between stacked boxes.
pub const ANCHOR_GAP: f32 = 12.0;

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

fn clamp_into(r: Rect, safe: Rect) -> Rect {
    let w = r.w.min(safe.w);
    let h = r.h.min(safe.h);
    Rect {
        x: r.x.clamp(safe.x, (safe.x + safe.w - w).max(safe.x)),
        y: r.y.clamp(safe.y, (safe.y + safe.h - h).max(safe.y)),
        w,
        h,
    }
}

fn inside(safe: Rect, p: [f32; 2]) -> bool {
    p[0] >= safe.x && p[0] <= safe.x + safe.w && p[1] >= safe.y && p[1] <= safe.y + safe.h
}

/// Where the ray from the safe rectangle's center toward `p` leaves the rectangle.
fn edge_point(safe: Rect, p: [f32; 2]) -> [f32; 2] {
    let cx = safe.x + safe.w * 0.5;
    let cy = safe.y + safe.h * 0.5;
    let dx = p[0] - cx;
    let dy = p[1] - cy;
    let sx = if dx.abs() < 1e-6 {
        f32::INFINITY
    } else {
        (safe.w * 0.5) / dx.abs()
    };
    let sy = if dy.abs() < 1e-6 {
        f32::INFINITY
    } else {
        (safe.h * 0.5) / dy.abs()
    };
    let s = sx.min(sy);
    if !s.is_finite() {
        return [cx, cy];
    }
    [cx + dx * s, cy + dy * s]
}

/// Places the captions. `inputs` may come in any order; the result is in ascending id order. The
/// band holds the captions with no usable anchor, stacked upward from the safe rectangle's bottom.
pub fn place_anchored(inputs: &[AnchorInput], safe: Rect) -> Vec<AnchorPlacement> {
    let _span = tracing::trace_span!("caption_anchor_place").entered();
    let mut sorted: Vec<AnchorInput> = inputs.to_vec();
    sorted.sort_by_key(|i| i.id);
    let mut placed: Vec<AnchorPlacement> = Vec::with_capacity(sorted.len());
    let mut band_top = safe.y + safe.h;
    let mut band: Vec<AnchorPlacement> = Vec::new();
    let mut anchored: Vec<AnchorInput> = Vec::new();
    for i in &sorted {
        match i.anchor.filter(|p| p[0].is_finite() && p[1].is_finite()) {
            Some(_) => anchored.push(*i),
            None => band.push(place_in_band(*i, safe, &mut band_top)),
        }
    }
    let mut fixed: Vec<Rect> = band.iter().map(|b| b.rect).collect();
    let mut out_of_room: Vec<AnchorInput> = Vec::new();
    for i in anchored {
        let Some(a) = i.anchor else { continue };
        let on_screen = inside(safe, a);
        let w = i.size[0].min(safe.w);
        let h = i.size[1].min(safe.h);
        let want = if on_screen {
            Rect {
                x: a[0] - w * 0.5,
                y: a[1] - h - ANCHOR_GAP,
                w,
                h,
            }
        } else {
            let e = edge_point(safe, a);
            Rect {
                x: e[0] - w * 0.5,
                y: e[1] - h * 0.5,
                w,
                h,
            }
        };
        let base = clamp_into(want, safe);
        // Slide up or down by (height + gap) until clear; try nearer offsets first.
        let step = h + ANCHOR_GAP;
        let mut found = None;
        let max_steps = ((safe.h / step.max(1.0)).ceil() as i32).max(1);
        'search: for k in 0..=max_steps {
            for dir in [-1.0f32, 1.0] {
                if k == 0 && dir > 0.0 {
                    continue;
                }
                let cand = clamp_into(
                    Rect {
                        y: base.y + dir * k as f32 * step,
                        ..base
                    },
                    safe,
                );
                // A clamped candidate that moved less than asked is a duplicate of an earlier one.
                if !fixed.iter().any(|f| overlaps(cand, *f)) {
                    found = Some(cand);
                    break 'search;
                }
            }
        }
        match found {
            Some(rect) => {
                fixed.push(rect);
                let mode = if on_screen {
                    AnchorMode::OnScreen
                } else {
                    let c = [safe.x + safe.w * 0.5, safe.y + safe.h * 0.5];
                    let angle = (a[0] - c[0]).atan2(c[1] - a[1]).rem_euclid(std::f32::consts::TAU);
                    AnchorMode::EdgeArrow(EdgeArrow {
                        pos: edge_point(safe, a),
                        angle,
                    })
                };
                placed.push(AnchorPlacement { id: i.id, rect, mode });
            }
            None => out_of_room.push(i),
        }
    }
    for i in out_of_room {
        let mut top = fixed.iter().map(|f| f.y).fold(safe.y + safe.h, f32::min);
        let p = place_in_band(i, safe, &mut top);
        fixed.push(p.rect);
        band.push(p);
    }
    placed.extend(band);
    placed.sort_by_key(|p| p.id);
    placed
}

fn place_in_band(i: AnchorInput, safe: Rect, band_top: &mut f32) -> AnchorPlacement {
    let w = i.size[0].min(safe.w);
    let h = i.size[1].min(safe.h);
    let y = (*band_top - h).max(safe.y);
    *band_top = y - ANCHOR_GAP;
    AnchorPlacement {
        id: i.id,
        rect: Rect {
            x: safe.x + (safe.w - w) * 0.5,
            y,
            w,
            h,
        },
        mode: AnchorMode::BottomBand,
    }
}

/// What a caption follows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnchorTarget {
    /// A fixed world point.
    World(Vec3),
    /// An entity's world position plus `offset` (the head).
    Entity {
        /// The entity.
        entity: Entity,
        /// Added to the entity's translation.
        offset: Vec3,
    },
}

/// The captions that follow something, by id. A game (or the conversation runner's speaker sync)
/// fills it; captions without an entry use the band.
#[derive(Resource, Debug, Default, Clone)]
pub struct CaptionAnchors(pub std::collections::BTreeMap<CaptionId, AnchorTarget>);

/// The camera the anchors are projected with. The host writes it each frame from its main camera.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct CaptionAnchorView {
    /// Projection times view.
    pub view_proj: Mat4,
    /// Viewport in logical pixels.
    pub viewport: (f32, f32),
}

/// The last computed placements (`captions.anchor_state` reads this).
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct CaptionAnchorState {
    /// True when anchored placement ran this frame.
    pub active: bool,
    /// One per visible row.
    pub placements: Vec<AnchorPlacement>,
}

impl CaptionAnchorState {
    /// The placement of caption `id`.
    pub fn get(&self, id: CaptionId) -> Option<&AnchorPlacement> {
        self.placements.iter().find(|p| p.id == id)
    }
}

/// An arrow glyph for an [`EdgeArrow`] angle (clockwise from up), eight directions.
pub fn arrow_glyph(angle: f32) -> &'static str {
    const GLYPHS: [&str; 8] = [
        "\u{2191}", "\u{2197}", "\u{2192}", "\u{2198}", "\u{2193}", "\u{2199}", "\u{2190}", "\u{2196}",
    ];
    let sector =
        ((angle.rem_euclid(std::f32::consts::TAU) / std::f32::consts::FRAC_PI_4).round() as usize) % 8;
    GLYPHS[sector]
}

/// `PostUpdate`, after the board: projects the anchors of the visible rows and places them. Does
/// nothing (and clears the state) in bottom-band mode.
pub fn caption_anchor_system(world: &mut World) {
    let _span = tracing::trace_span!("caption_anchors").entered();
    let anchored = world
        .get_resource::<DialogueCaptionSettings>()
        .is_some_and(|s| s.placement == CaptionPlacement::Anchored);
    if !anchored {
        if let Some(mut st) = world.get_resource_mut::<CaptionAnchorState>()
            && (st.active || !st.placements.is_empty())
        {
            *st = CaptionAnchorState::default();
        }
        return;
    }
    let Some(opts) = world.get_resource::<super::CaptionOptions>().cloned() else {
        return;
    };
    let Some(board) = world.get_resource::<super::CaptionBoard>() else {
        return;
    };
    let rows = board.visible_rows(&opts);
    let safe = board.safe_area();
    let view = world.get_resource::<CaptionAnchorView>().copied();
    let targets = world
        .get_resource::<CaptionAnchors>()
        .cloned()
        .unwrap_or_default();
    let inputs: Vec<AnchorInput> = rows
        .iter()
        .map(|row| {
            let world_pos = targets.0.get(&row.id).and_then(|t| match *t {
                AnchorTarget::World(p) => Some(p),
                AnchorTarget::Entity { entity, offset } => world
                    .get::<regia_scene::GlobalTransform>(entity)
                    .map(|g| g.translation() + offset),
            });
            let anchor = match (view, world_pos) {
                (Some(v), Some(p)) => {
                    let pr = project_point(&v.view_proj, v.viewport, p);
                    (!pr.behind).then_some(pr.px)
                }
                _ => None,
            };
            AnchorInput {
                id: row.id,
                anchor,
                size: [row.rect.w, row.rect.h],
            }
        })
        .collect();
    let placements = place_anchored(&inputs, safe);
    if let Some(mut st) = world.get_resource_mut::<CaptionAnchorState>() {
        let next = CaptionAnchorState {
            active: true,
            placements,
        };
        if *st != next {
            *st = next;
        }
    }
}

#[cfg(test)]
mod tests;
