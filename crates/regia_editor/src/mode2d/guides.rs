// SPDX-License-Identifier: MPL-2.0
//! 2D rulers, guides and smart guides (ADR-7853): rulers along the front pane's top and left
//! edges in level units, guide lines dragged out of them, snapping of moved objects to guides,
//! and smart guides while an object moves (edge and center alignment with nearby objects, and
//! equal-spacing hints). Alt held while dragging turns snapping off.
//!
//! Guides are editor view state: they are kept per map in [`Guides2d`] and never written into the
//! map or the level.
//!
//! Coordinates are level units relative to the map's bottom-left corner (+X right, +Y up), the
//! front view's world plane. `editor.mode2d_guides` is the agent route.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::{Mode2d, Mode2dError, mode2d_error, param};
use crate::viewport_agent::Handler;

/// Most guides one map keeps.
pub const MAX_GUIDES: usize = 256;
/// Snap distance in pane pixels (converted to level units with the pane's scale).
pub const SNAP_PX: f32 = 6.0;
/// Thickness of a ruler band, points (scaled by `appearance.ui_scale` through egui's points).
pub const RULER_PX: f32 = 18.0;

/// Which way a guide runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideAxis {
    /// A vertical line at an x.
    Vertical,
    /// A horizontal line at a y.
    Horizontal,
}

impl GuideAxis {
    /// The protocol name (`vertical`, `horizontal`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
        }
    }

    /// The axis of a protocol name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "vertical" => Some(Self::Vertical),
            "horizontal" => Some(Self::Horizontal),
            _ => None,
        }
    }
}

/// One guide line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    /// Its direction.
    pub axis: GuideAxis,
    /// Its x (vertical) or y (horizontal), level units.
    pub at: f32,
}

/// The rulers and guides of the 2D view.
#[derive(Debug, Clone, PartialEq)]
pub struct Guides2d {
    /// Draw the rulers.
    pub rulers: bool,
    /// Moved objects snap to guides and smart guides.
    pub snap: bool,
    /// The guides.
    pub guides: Vec<Guide>,
}

impl Default for Guides2d {
    fn default() -> Self {
        Self {
            rulers: true,
            snap: true,
            guides: Vec::new(),
        }
    }
}

/// An axis-aligned box in level units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Box2 {
    /// Minimum corner.
    pub min: [f32; 2],
    /// Maximum corner.
    pub max: [f32; 2],
}

impl Box2 {
    /// The box moved by `d`.
    #[must_use]
    pub fn offset(self, d: [f32; 2]) -> Self {
        Self {
            min: [self.min[0] + d[0], self.min[1] + d[1]],
            max: [self.max[0] + d[0], self.max[1] + d[1]],
        }
    }

    /// Low edge, center and high edge along `axis` (0 = x, 1 = y).
    fn stops(self, axis: usize) -> [f32; 3] {
        [
            self.min[axis],
            (self.min[axis] + self.max[axis]) * 0.5,
            self.max[axis],
        ]
    }

    /// Whether the boxes overlap along `axis`.
    fn overlaps(self, other: Self, axis: usize) -> bool {
        self.min[axis] <= other.max[axis] && other.min[axis] <= self.max[axis]
    }
}

/// A smart-guide line to draw: from `a` to `b`, level units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuideLine {
    /// Start.
    pub a: [f32; 2],
    /// End.
    pub b: [f32; 2],
}

/// The result of a snapped move.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapResult {
    /// The offset to add to the moved box's position.
    pub offset: [f32; 2],
    /// Alignment lines (edges or centers that line up).
    pub lines: Vec<GuideLine>,
    /// Equal-spacing hints: each gap that now matches, as a line across the gap.
    pub spacing: Vec<GuideLine>,
    /// Snapped to a user guide on x / y.
    pub to_guide: [bool; 2],
}

/// Snaps `moving` (already at its dragged position) to the guides, then aligns it with `others`
/// (edges and centers) and evens its gaps to its neighbors, within `tolerance` level units per
/// axis. A guide wins over a smart guide on the same axis.
#[must_use]
pub fn snap_move(moving: Box2, others: &[Box2], guides: &[Guide], tolerance: f32) -> SnapResult {
    let mut out = SnapResult::default();
    for axis in 0..2 {
        let wanted = if axis == 0 {
            GuideAxis::Vertical
        } else {
            GuideAxis::Horizontal
        };
        // 1. User guides: the closest stop of the box to a guide.
        let mut best: Option<f32> = None;
        for guide in guides.iter().filter(|g| g.axis == wanted) {
            for stop in moving.stops(axis) {
                let d = guide.at - stop;
                if d.abs() <= tolerance && best.is_none_or(|b| d.abs() < b.abs()) {
                    best = Some(d);
                }
            }
        }
        if let Some(d) = best {
            out.offset[axis] = d;
            out.to_guide[axis] = true;
            continue;
        }
        // 2. Alignment: a stop of the box to a stop of another box.
        let mut align: Option<f32> = None;
        for other in others {
            for stop in moving.stops(axis) {
                for target in other.stops(axis) {
                    let d = target - stop;
                    if d.abs() <= tolerance && align.is_none_or(|b| d.abs() < b.abs()) {
                        align = Some(d);
                    }
                }
            }
        }
        // 3. Equal spacing between the nearest neighbors on each side (boxes that overlap the
        // moved one across this axis).
        let spacing = equal_spacing(moving, others, axis, tolerance);
        let d = match (align, spacing) {
            (Some(a), Some(s)) if s.abs() < a.abs() => s,
            (Some(a), _) => a,
            (None, Some(s)) => s,
            (None, None) => continue,
        };
        out.offset[axis] = d;
    }
    // Lines and hints for the snapped position.
    let moved = moving.offset(out.offset);
    for axis in 0..2 {
        let other_axis = 1 - axis;
        for other in others {
            for stop in moved.stops(axis) {
                if other.stops(axis).iter().any(|t| (t - stop).abs() <= 1e-3) {
                    let lo = moved.min[other_axis].min(other.min[other_axis]);
                    let hi = moved.max[other_axis].max(other.max[other_axis]);
                    let line = if axis == 0 {
                        GuideLine {
                            a: [stop, lo],
                            b: [stop, hi],
                        }
                    } else {
                        GuideLine {
                            a: [lo, stop],
                            b: [hi, stop],
                        }
                    };
                    if !out.lines.contains(&line) {
                        out.lines.push(line);
                    }
                }
            }
        }
        if let Some((left, right)) = neighbors(moved, others, axis) {
            let gap_l = moved.min[axis] - left.max[axis];
            let gap_r = right.min[axis] - moved.max[axis];
            if (gap_l - gap_r).abs() <= 1e-3 && gap_l > 0.0 {
                let mid = (moved.min[other_axis] + moved.max[other_axis]) * 0.5;
                for (a, b) in [
                    (left.max[axis], moved.min[axis]),
                    (moved.max[axis], right.min[axis]),
                ] {
                    out.spacing.push(if axis == 0 {
                        GuideLine {
                            a: [a, mid],
                            b: [b, mid],
                        }
                    } else {
                        GuideLine {
                            a: [mid, a],
                            b: [mid, b],
                        }
                    });
                }
            }
        }
    }
    out
}

/// The nearest box on each side of `moving` along `axis` among the boxes that overlap it across
/// the axis.
fn neighbors(moving: Box2, others: &[Box2], axis: usize) -> Option<(Box2, Box2)> {
    let across = 1 - axis;
    let row: Vec<Box2> = others
        .iter()
        .copied()
        .filter(|o| o.overlaps(moving, across))
        .collect();
    let left = row
        .iter()
        .filter(|o| o.max[axis] <= moving.min[axis] + 1e-3)
        .max_by(|a, b| a.max[axis].total_cmp(&b.max[axis]))?;
    let right = row
        .iter()
        .filter(|o| o.min[axis] >= moving.max[axis] - 1e-3)
        .min_by(|a, b| a.min[axis].total_cmp(&b.min[axis]))?;
    Some((*left, *right))
}

/// The offset along `axis` that makes the gaps to both neighbors equal, when it is within
/// `tolerance`.
fn equal_spacing(moving: Box2, others: &[Box2], axis: usize, tolerance: f32) -> Option<f32> {
    let (left, right) = neighbors(moving, others, axis)?;
    let gap_l = moving.min[axis] - left.max[axis];
    let gap_r = right.min[axis] - moving.max[axis];
    let d = (gap_r - gap_l) * 0.5;
    (d.abs() <= tolerance && gap_l + d > 0.0).then_some(d)
}

/// Ruler tick spacing in level units for `units_per_px`: the 1-2-5 step whose ticks are at least
/// 8 px apart; every fifth or tenth tick is labeled by the caller.
#[must_use]
pub fn ruler_step(units_per_px: f32) -> f32 {
    let min = (units_per_px * 8.0).max(1e-6);
    let mut step = 10f32.powf(min.log10().floor());
    for factor in [1.0, 2.0, 5.0, 10.0] {
        if step * factor >= min {
            step *= factor;
            break;
        }
    }
    step
}

impl Guides2d {
    /// Adds a guide; refused past [`MAX_GUIDES`] or for a value that is not finite.
    pub fn add(&mut self, axis: GuideAxis, at: f32) -> Result<usize, Mode2dError> {
        if !at.is_finite() {
            return Err(Mode2dError::BadParams("`at` is a finite number".into()));
        }
        if self.guides.len() >= MAX_GUIDES {
            return Err(Mode2dError::BadParams(format!("at most {MAX_GUIDES} guides")));
        }
        self.guides.push(Guide { axis, at });
        Ok(self.guides.len() - 1)
    }
}

fn bad(message: impl Into<String>) -> Mode2dError {
    Mode2dError::BadParams(message.into())
}

fn guides_json(guides: &Guides2d) -> Value {
    json!({
        "rulers": guides.rulers,
        "snap": guides.snap,
        "guides": guides.guides.iter().enumerate().map(|(i, g)| json!({ "index": i, "axis": g.axis.name(), "at": g.at })).collect::<Vec<_>>(),
    })
}

fn box_param(value: Option<&Value>, what: &str) -> Result<Box2, Mode2dError> {
    let v: Vec<f32> = value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).map(|f| f as f32).collect())
        .unwrap_or_default();
    match v.as_slice() {
        [x0, y0, x1, y1] if v.iter().all(|f| f.is_finite()) => Ok(Box2 {
            min: [x0.min(*x1), y0.min(*y1)],
            max: [x0.max(*x1), y0.max(*y1)],
        }),
        _ => Err(bad(format!("`{what}` is [x0, y0, x1, y1] in level units"))),
    }
}

fn line_json(line: &GuideLine) -> Value {
    json!([line.a[0], line.a[1], line.b[0], line.b[1]])
}

fn run(mode: &mut Mode2d, params: &Value) -> Result<Value, Mode2dError> {
    let action = params.get("action").map_or(Some("list"), Value::as_str);
    let index = || {
        params
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| bad("`index` is a guide index"))
    };
    let at = || {
        params
            .get("at")
            .and_then(Value::as_f64)
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .ok_or_else(|| bad("`at` is a finite number"))
    };
    let guides = &mut mode.guides;
    match action {
        Some("list") => {}
        Some("add") => {
            let axis = params
                .get("axis")
                .and_then(Value::as_str)
                .and_then(GuideAxis::parse)
                .ok_or_else(|| bad("`axis` is vertical or horizontal"))?;
            guides.add(axis, at()?)?;
        }
        Some("move") => {
            let (i, at) = (index()?, at()?);
            let guide = guides
                .guides
                .get_mut(i)
                .ok_or_else(|| bad(format!("no guide {i}")))?;
            guide.at = at;
        }
        Some("remove") => {
            let i = index()?;
            if i >= guides.guides.len() {
                return Err(bad(format!("no guide {i}")));
            }
            guides.guides.remove(i);
        }
        Some("clear") => guides.guides.clear(),
        Some("set") => {
            for key in ["rulers", "snap"] {
                if let Some(v) = params.get(key) {
                    let v = v.as_bool().ok_or_else(|| bad(format!("`{key}` is a boolean")))?;
                    if key == "rulers" {
                        guides.rulers = v;
                    } else {
                        guides.snap = v;
                    }
                }
            }
        }
        Some("snap") => {
            // What a drag of `moving` among `others` snaps to (`alt: true`: no snapping).
            let moving = box_param(params.get("moving"), "moving")?;
            let others = params
                .get("others")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|o| box_param(Some(o), "others[]"))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let tolerance = params
                .get("tolerance")
                .and_then(Value::as_f64)
                .map_or(4.0, |f| f as f32);
            let alt = params.get("alt").and_then(Value::as_bool).unwrap_or(false);
            let result = if alt || !guides.snap {
                SnapResult::default()
            } else {
                snap_move(moving, &others, &guides.guides, tolerance.max(0.0))
            };
            let mut out = guides_json(guides);
            out["snap_result"] = json!({
                "offset": result.offset,
                "to_guide": result.to_guide,
                "lines": result.lines.iter().map(line_json).collect::<Vec<_>>(),
                "spacing": result.spacing.iter().map(line_json).collect::<Vec<_>>(),
            });
            return Ok(out);
        }
        _ => return Err(bad("`action` is list, add, move, remove, clear, set or snap")),
    }
    Ok(guides_json(guides))
}

/// The 2D guides as the editor view state stores them per level (`view_state`).
#[must_use]
pub fn saved_guides(world: &World) -> Vec<crate::view_state::Guide2dPose> {
    world.get_resource::<Mode2d>().map_or_else(Vec::new, |mode| {
        mode.guides
            .guides
            .iter()
            .map(|g| crate::view_state::Guide2dPose {
                vertical: g.axis == GuideAxis::Vertical,
                at: g.at,
            })
            .collect()
    })
}

/// Restores a level's saved 2D guides (non-finite values and any past [`MAX_GUIDES`] are dropped).
pub fn restore_guides(world: &mut World, saved: &[crate::view_state::Guide2dPose]) {
    let guides: Vec<Guide> = saved
        .iter()
        .filter(|g| g.at.is_finite())
        .take(MAX_GUIDES)
        .map(|g| Guide {
            axis: if g.vertical {
                GuideAxis::Vertical
            } else {
                GuideAxis::Horizontal
            },
            at: g.at,
        })
        .collect();
    if guides.is_empty() && !world.contains_resource::<Mode2d>() {
        return;
    }
    let mut mode = world.get_resource_or_insert_with(Mode2d::default);
    if mode.guides.guides != guides {
        mode.guides.guides = guides;
        mode.dragging_guide = None;
    }
}

/// Runs one `editor.mode2d_guides` action on `mode` (the 2D tab's guide controls use it too).
pub fn guide_action(mode: &mut Mode2d, params: &Value) -> Result<Value, Mode2dError> {
    run(mode, params)
}

/// `editor.mode2d_guides`.
fn mode2d_guides(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    let result = run(&mut mode, params);
    world.insert_resource(mode);
    result.map_err(|e| mode2d_error(&e))
}

const PARAMS: &[ParamSchema] = &[
    param(
        "action",
        "string",
        false,
        "list (default), add, move, remove, clear, set (rulers / snap) or snap (what a move would snap to).",
    ),
    param("axis", "string", false, "add: vertical or horizontal."),
    param(
        "at",
        "number",
        false,
        "add / move: the guide's x (vertical) or y (horizontal), level units.",
    ),
    param("index", "integer", false, "move / remove: the guide."),
    param("rulers", "boolean", false, "set: draw the rulers."),
    param(
        "snap",
        "boolean",
        false,
        "set: moved objects snap to guides and smart guides.",
    ),
    param(
        "moving",
        "array",
        false,
        "snap: the moved box [x0, y0, x1, y1], level units.",
    ),
    param(
        "others",
        "array",
        false,
        "snap: the other boxes, each [x0, y0, x1, y1].",
    ),
    param(
        "tolerance",
        "number",
        false,
        "snap: the snap distance in level units (default 4).",
    ),
    param("alt", "boolean", false, "snap: Alt held (snapping off)."),
];

/// The guides' agent method.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.mode2d_guides",
            tool: "mode2d_guides",
            description: "The 2D view's rulers and guides (editor view state, never saved in the map or level): list, \
                add, move, remove or clear guides, turn the rulers and snapping on or off, and ask what a moved box \
                snaps to (guides first, then edge and center alignment with the other boxes and equal spacing to its \
                neighbors; Alt turns snapping off). Errors in data.mode2d_error: mode2d_bad_params.",
            params: PARAMS,
            result: "{ rulers, snap, guides: [{ index, axis, at }], snap_result? { offset, to_guide, lines, spacing } }",
            mutating: true,
            permission: Permission::EditScene,
        },
        mode2d_guides as Handler,
    )]
}

/// Paints the rulers along the top and left edges of a front pane and the guides across it, in
/// theme colors. `to_pixel` maps level units to pane pixels; `units_per_px` is the pane's scale.
pub fn paint_rulers(
    painter: &egui::Painter,
    rect: egui::Rect,
    guides: &Guides2d,
    to_pixel: impl Fn([f32; 2]) -> Option<egui::Pos2>,
    from_pixel: impl Fn(egui::Pos2) -> Option<[f32; 2]>,
    units_per_px: f32,
    tokens: &crate::theme::ThemeTokens,
) {
    let line = egui::Stroke::new(1.0, tokens.overlay.selection_outline);
    for guide in &guides.guides {
        let (a, b) = match guide.axis {
            GuideAxis::Vertical => {
                let x = to_pixel([guide.at, 0.0]).map(|p| p.x);
                (
                    x.map(|x| egui::pos2(x, rect.top())),
                    x.map(|x| egui::pos2(x, rect.bottom())),
                )
            }
            GuideAxis::Horizontal => {
                let y = to_pixel([0.0, guide.at]).map(|p| p.y);
                (
                    y.map(|y| egui::pos2(rect.left(), y)),
                    y.map(|y| egui::pos2(rect.right(), y)),
                )
            }
        };
        if let (Some(a), Some(b)) = (a, b) {
            painter.line_segment([a, b], line);
        }
    }
    if !guides.rulers {
        return;
    }
    let top = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), RULER_PX));
    let left = egui::Rect::from_min_size(rect.min, egui::vec2(RULER_PX, rect.height()));
    painter.rect_filled(top, 0.0, tokens.ground);
    painter.rect_filled(left, 0.0, tokens.ground);
    let tick = egui::Stroke::new(1.0, tokens.overlay.grid_major);
    let step = ruler_step(units_per_px);
    let font = egui::FontId::proportional(9.0);
    let (Some(lo), Some(hi)) = (from_pixel(rect.left_bottom()), from_pixel(rect.right_top())) else {
        return;
    };
    for axis in 0..2 {
        let first = (lo[axis] / step).floor() as i64;
        let last = (hi[axis] / step).ceil() as i64;
        if last - first > 2048 {
            continue;
        }
        for i in first..=last {
            let value = i as f32 * step;
            let point = if axis == 0 { [value, lo[1]] } else { [lo[0], value] };
            let Some(p) = to_pixel(point) else { continue };
            let major = i.rem_euclid(5) == 0;
            let len = if major { RULER_PX * 0.6 } else { RULER_PX * 0.3 };
            if axis == 0 && p.x > left.right() {
                painter.line_segment(
                    [egui::pos2(p.x, top.bottom()), egui::pos2(p.x, top.bottom() - len)],
                    tick,
                );
                if major {
                    painter.text(
                        egui::pos2(p.x + 2.0, top.top()),
                        egui::Align2::LEFT_TOP,
                        format!("{value}"),
                        font.clone(),
                        tokens.overlay.grid_major,
                    );
                }
            } else if axis == 1 && p.y > top.bottom() {
                painter.line_segment(
                    [egui::pos2(left.right(), p.y), egui::pos2(left.right() - len, p.y)],
                    tick,
                );
                if major {
                    painter.text(
                        egui::pos2(left.left() + 1.0, p.y - 1.0),
                        egui::Align2::LEFT_BOTTOM,
                        format!("{value}"),
                        font.clone(),
                        tokens.overlay.grid_major,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(x0: f32, y0: f32, x1: f32, y1: f32) -> Box2 {
        Box2 {
            min: [x0, y0],
            max: [x1, y1],
        }
    }

    #[test]
    fn a_move_aligns_edges_and_centers_and_draws_the_line() {
        let other = b(0.0, 0.0, 10.0, 10.0);
        // The left edge is 2 units right of the other's right edge; every y stop is 1.5 above one.
        let r = snap_move(b(12.0, 1.5, 20.0, 11.5), &[other], &[], 4.0);
        assert_eq!(r.offset, [-2.0, -1.5]);
        assert!(
            r.lines.contains(&GuideLine {
                a: [10.0, 0.0],
                b: [10.0, 10.0]
            }),
            "{:?}",
            r.lines
        );
        // Centers: a box centered 1 unit off the other's center line snaps onto it.
        let r = snap_move(b(1.0, 30.0, 11.0, 40.0), &[other], &[], 4.0);
        assert_eq!(r.offset[0], -1.0);
        // Too far: nothing moves.
        let r = snap_move(b(30.0, 30.0, 40.0, 40.0), &[other], &[], 4.0);
        assert_eq!(r.offset, [0.0, 0.0]);
        assert!(r.lines.is_empty());
    }

    #[test]
    fn a_guide_wins_over_a_nearer_smart_guide() {
        let other = b(0.0, 0.0, 10.0, 10.0);
        let guides = [Guide {
            axis: GuideAxis::Vertical,
            at: 13.5,
        }];
        // The left edge is 1 from the other's right edge, the center 1.5 from the guide.
        let r = snap_move(b(11.0, 50.0, 19.0, 60.0), &[other], &guides, 4.0);
        assert_eq!(r.offset[0], -1.5, "the center goes to the guide");
        assert_eq!(r.to_guide, [true, false]);
    }

    #[test]
    fn equal_spacing_evens_the_gaps_and_hints_both() {
        let left = b(0.0, 0.0, 10.0, 10.0);
        let right = b(40.0, 0.0, 50.0, 10.0);
        // Gaps 9 and 11 → both 10 after a +1 move; y has an exact alignment already.
        let r = snap_move(b(19.0, 0.0, 29.0, 10.0), &[left, right], &[], 1.5);
        assert_eq!(r.offset, [1.0, 0.0]);
        assert_eq!(r.spacing.len(), 2);
        assert_eq!(
            r.spacing[0],
            GuideLine {
                a: [10.0, 5.0],
                b: [20.0, 5.0]
            }
        );
        assert_eq!(
            r.spacing[1],
            GuideLine {
                a: [30.0, 5.0],
                b: [40.0, 5.0]
            }
        );
    }

    #[test]
    fn guides_round_trip_through_the_view_state_and_bad_values_drop() {
        let mut world = World::new();
        restore_guides(&mut world, &[]);
        assert!(
            !world.contains_resource::<Mode2d>(),
            "nothing to restore adds nothing"
        );
        let saved = [
            crate::view_state::Guide2dPose {
                vertical: true,
                at: 12.0,
            },
            crate::view_state::Guide2dPose {
                vertical: false,
                at: f32::NAN,
            },
            crate::view_state::Guide2dPose {
                vertical: false,
                at: -3.5,
            },
        ];
        restore_guides(&mut world, &saved);
        assert_eq!(
            world.resource::<Mode2d>().guides.guides,
            vec![
                Guide {
                    axis: GuideAxis::Vertical,
                    at: 12.0
                },
                Guide {
                    axis: GuideAxis::Horizontal,
                    at: -3.5
                },
            ]
        );
        assert_eq!(saved_guides(&world), vec![saved[0], saved[2]]);
        // A level's view state carries them through the file.
        let mut doc = crate::view_state::ViewStateDoc::default();
        doc.levels.insert(
            "a.level.ron".into(),
            crate::view_state::LevelView {
                guides_2d: saved_guides(&world),
                ..Default::default()
            },
        );
        let back = crate::view_state::ViewStateDoc::from_text(&doc.to_text()).expect("parses");
        assert_eq!(back.levels["a.level.ron"].guides_2d, saved_guides(&world));
        // A level without guides writes no `guides_2d` key (older files stay byte-identical).
        let plain = crate::view_state::ViewStateDoc::default();
        assert!(!plain.to_text().contains("guides_2d"));
    }

    #[test]
    fn ruler_steps_follow_one_two_five() {
        assert_eq!(ruler_step(1.0), 10.0);
        assert_eq!(ruler_step(0.2), 2.0);
        assert_eq!(ruler_step(0.5), 5.0);
        assert_eq!(ruler_step(3.0), 50.0);
    }

    #[test]
    fn the_agent_method_edits_guides_and_reports_a_snap() {
        let mut world = World::new();
        world.insert_resource(Mode2d::default());
        let out = mode2d_guides(
            &mut world,
            &json!({ "action": "add", "axis": "vertical", "at": 32.0 }),
        )
        .expect("adds");
        assert_eq!(
            out["guides"],
            json!([{ "index": 0, "axis": "vertical", "at": 32.0 }])
        );
        mode2d_guides(&mut world, &json!({ "action": "move", "index": 0, "at": 48.0 })).expect("moves");
        let out = mode2d_guides(
            &mut world,
            &json!({ "action": "snap", "moving": [45.0, 0.0, 55.0, 10.0], "others": [] }),
        )
        .expect("snaps");
        assert_eq!(
            out["snap_result"]["offset"],
            json!([-2.0, 0.0]),
            "the center to the guide at 48"
        );
        let out = mode2d_guides(
            &mut world,
            &json!({ "action": "snap", "moving": [45.0, 0.0, 55.0, 10.0], "alt": true }),
        )
        .expect("alt");
        assert_eq!(
            out["snap_result"]["offset"],
            json!([0.0, 0.0]),
            "Alt turns snapping off"
        );
        let out = mode2d_guides(&mut world, &json!({ "action": "set", "rulers": false })).expect("sets");
        assert_eq!(out["rulers"], false);
        mode2d_guides(&mut world, &json!({ "action": "remove", "index": 0 })).expect("removes");
        for params in [
            json!({ "action": "add", "axis": "diagonal", "at": 1.0 }),
            json!({ "action": "add", "axis": "vertical" }),
            json!({ "action": "move", "index": 5, "at": 1.0 }),
            json!({ "action": "remove", "index": 0 }),
            json!({ "action": "snap", "moving": [1.0] }),
            json!({ "action": "spin" }),
        ] {
            let err = mode2d_guides(&mut world, &params).expect_err("refused");
            assert_eq!(
                err.data.as_ref().and_then(|d| d["mode2d_error"].as_str()),
                Some("mode2d_bad_params"),
                "{params}"
            );
        }
    }
}
