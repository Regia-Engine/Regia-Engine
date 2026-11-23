// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// editor/src/plugins/absm/blendspace.rs. Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors,
// MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! The blend-space point editor: samples as draggable points on a 1D line or 2D field, the
//! Delaunay triangulation the runtime blends over, and a crosshair at the preview's current
//! parameter value with each sample's live weight.
//!
//! Portions ported from Fyrox (<https://github.com/FyroxEngine/Fyrox>),
//! `editor/src/plugins/absm/blendspace.rs` (`BlendSpaceField`: grid by snap step, triangle
//! edges, the sampling crosshair, `blend_to_local` mapping, click-to-add / drag-to-move
//! points), Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT
//! licence (notice in `docs/ported-sources.toml`). Adapted to an egui painter and to
//! `regia_animation`'s `BlendSpace1D` / `BlendSpace2D` (weights come from the runtime).

use regia_animation::blendspace::{BlendSpace1D, BlendSpace2D};

/// What the user did on the field this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldAction {
    /// Sample `index` was dragged to a new position.
    Move(usize, [f32; 2]),
    /// A click on empty space: add a sample here.
    Add([f32; 2]),
    /// Sample `index` was right-clicked: remove it.
    Remove(usize),
    /// The field was clicked/dragged without a point: move the preview input there.
    SetInput([f32; 2]),
}

/// Value range shown on each axis: the samples' bounds plus margin (at least `0..1`).
#[must_use]
pub fn field_range(points: &[[f32; 2]], input: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let mut lo = [0.0f32, 0.0];
    let mut hi = [1.0f32, 1.0];
    for p in points.iter().chain(std::iter::once(&input)) {
        for axis in 0..2 {
            lo[axis] = lo[axis].min(p[axis]);
            hi[axis] = hi[axis].max(p[axis]);
        }
    }
    for axis in 0..2 {
        let pad = (hi[axis] - lo[axis]) * 0.08;
        lo[axis] -= pad;
        hi[axis] += pad;
    }
    (lo, hi)
}

/// Maps a blend value into the rect (Fyrox's `blend_to_local`, y up).
#[must_use]
pub fn blend_to_local(value: [f32; 2], lo: [f32; 2], hi: [f32; 2], rect: egui::Rect) -> egui::Pos2 {
    let nx = (value[0] - lo[0]) / (hi[0] - lo[0]).max(1e-6);
    let ny = (value[1] - lo[1]) / (hi[1] - lo[1]).max(1e-6);
    egui::pos2(
        rect.left() + nx * rect.width(),
        rect.bottom() - ny * rect.height(),
    )
}

/// The inverse of [`blend_to_local`].
#[must_use]
pub fn local_to_blend(p: egui::Pos2, lo: [f32; 2], hi: [f32; 2], rect: egui::Rect) -> [f32; 2] {
    let nx = (p.x - rect.left()) / rect.width().max(1e-6);
    let ny = (rect.bottom() - p.y) / rect.height().max(1e-6);
    [lo[0] + nx * (hi[0] - lo[0]), lo[1] + ny * (hi[1] - lo[1])]
}

/// Snaps `v` to multiples of `step` (no snapping when `step` ≤ 0).
#[must_use]
pub fn snap(v: f32, step: f32) -> f32 {
    if step > 0.0 { (v / step).round() * step } else { v }
}

/// Draws the field and reports one action.
///
/// `points` are the sample positions (1D spaces pass `[value, 0]` and `one_d = true`),
/// `labels` their clip names, `triangles` the triangulation (2D), `weights` the live weight
/// per sample index, `input` the preview parameter value.
#[allow(clippy::too_many_arguments)] // one widget call with every input it draws
pub fn blend_field(
    ui: &mut egui::Ui,
    id: egui::Id,
    one_d: bool,
    points: &[[f32; 2]],
    labels: &[String],
    triangles: &[[usize; 3]],
    weights: &[(usize, f32)],
    input: [f32; 2],
    snap_step: f32,
) -> Option<FieldAction> {
    let height = if one_d {
        70.0
    } else {
        ui.available_width().min(260.0)
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(26));
    // Keep the range stable while dragging so points do not run away from the cursor.
    let range_id = id.with("range");
    let dragging_id = id.with("dragging");
    let dragging: Option<usize> = ui.data(|d| d.get_temp::<Option<usize>>(dragging_id)).flatten();
    let (lo, hi) = match (
        dragging,
        ui.data(|d| d.get_temp::<([f32; 2], [f32; 2])>(range_id)),
    ) {
        (Some(_), Some(range)) => range,
        _ => field_range(points, input),
    };
    ui.data_mut(|d| d.insert_temp(range_id, (lo, hi)));
    let lo = if one_d { [lo[0], -1.0] } else { lo };
    let hi = if one_d { [hi[0], 1.0] } else { hi };
    let map = |v: [f32; 2]| blend_to_local(if one_d { [v[0], 0.0] } else { v }, lo, hi, rect);

    // Grid at the snap step (Fyrox draws one line per step, capped at 256).
    let grid = egui::Stroke::new(1.0, egui::Color32::from_gray(44));
    let step = if snap_step > 0.0 {
        snap_step
    } else {
        (hi[0] - lo[0]) / 8.0
    };
    let nx = (((hi[0] - lo[0]) / step.max(1e-6)) as usize).min(64);
    let x0 = (lo[0] / step.max(1e-6)).ceil() * step;
    for i in 0..=nx {
        let x = map([x0 + i as f32 * step, 0.0]).x;
        if x <= rect.right() {
            painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], grid);
        }
    }
    if one_d {
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.center().y),
                egui::pos2(rect.right(), rect.center().y),
            ],
            egui::Stroke::new(2.0, egui::Color32::from_gray(90)),
        );
    } else {
        let ystep = if snap_step > 0.0 {
            snap_step
        } else {
            (hi[1] - lo[1]) / 8.0
        };
        let ny = (((hi[1] - lo[1]) / ystep.max(1e-6)) as usize).min(64);
        let y0 = (lo[1] / ystep.max(1e-6)).ceil() * ystep;
        for i in 0..=ny {
            let y = map([0.0, y0 + i as f32 * ystep]).y;
            if y >= rect.top() {
                painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], grid);
            }
        }
        for tri in triangles {
            let (Some(a), Some(b), Some(c)) = (points.get(tri[0]), points.get(tri[1]), points.get(tri[2]))
            else {
                continue;
            };
            let (a, b, c) = (map(*a), map(*b), map(*c));
            let edge = egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 110, 140));
            painter.line_segment([a, b], edge);
            painter.line_segment([b, c], edge);
            painter.line_segment([c, a], edge);
        }
    }

    // Sample points with labels and live weights.
    let pointer = response.hover_pos();
    let mut hovered = None;
    for (i, p) in points.iter().enumerate() {
        let s = map(*p);
        let weight = weights.iter().find(|w| w.0 == i).map_or(0.0, |w| w.1);
        let near = pointer.is_some_and(|q| q.distance(s) < 8.0);
        if near {
            hovered = Some(i);
        }
        let fill = egui::Color32::from_rgb(
            (80.0 + 175.0 * weight) as u8,
            (120.0 + 30.0 * weight) as u8,
            (220.0 - 180.0 * weight) as u8,
        );
        painter.circle(
            s,
            if near { 7.0 } else { 6.0 },
            fill,
            egui::Stroke::new(1.0, egui::Color32::WHITE),
        );
        let label = labels.get(i).map_or("", String::as_str);
        let text = if weight > 0.0 {
            format!("{label} {:.0}%", weight * 100.0)
        } else {
            label.to_string()
        };
        // Labels near the right edge go on the point's left so the field does not clip them.
        let (offset, align) = if s.x > rect.right() - 70.0 {
            (egui::vec2(-8.0, -8.0), egui::Align2::RIGHT_BOTTOM)
        } else {
            (egui::vec2(8.0, -8.0), egui::Align2::LEFT_BOTTOM)
        };
        painter.text(
            s + offset,
            align,
            text,
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(220),
        );
    }
    // The sample list's keyboard focus (FU-5).
    let keys_id = id.with("keys");
    let mut keys: crate::canvas_keys::ListKeys = ui.data(|d| d.get_temp(keys_id)).unwrap_or_default();
    if let Some(p) = keys.focused().and_then(|i| points.get(i)) {
        crate::canvas_keys::focus_ring(
            &painter,
            egui::Rect::from_center_size(map(*p), egui::vec2(12.0, 12.0)),
        );
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!(
            "Blend space {} canvas, {} samples",
            if one_d { "1D" } else { "2D" },
            points.len()
        ),
    );
    // Sampling crosshair (Fyrox draws a 14-point cross at the sampling point).
    let c = map(input);
    let cross = egui::Stroke::new(2.0, crate::theme::ui_tokens(painter.ctx()).accents.active);
    painter.line_segment([c - egui::vec2(7.0, 0.0), c + egui::vec2(7.0, 0.0)], cross);
    painter.line_segment([c - egui::vec2(0.0, 7.0), c + egui::vec2(0.0, 7.0)], cross);

    let to_value = |p: egui::Pos2| {
        let v = local_to_blend(p, lo, hi, rect);
        if one_d {
            [snap(v[0], snap_step), 0.0]
        } else {
            [snap(v[0], snap_step), snap(v[1], snap_step)]
        }
    };
    // The keyboard route: one row per sample (FU-5, the convention of `canvas_keys`).
    let nudge = if snap_step > 0.0 {
        [snap_step, snap_step]
    } else {
        [(hi[0] - lo[0]) / 20.0, (hi[1] - lo[1]) / 20.0]
    };
    let keyed = sample_keys(ui, &mut keys, (points, labels), one_d, nudge, snap_step);
    ui.data_mut(|d| d.insert_temp(keys_id, keys));
    if keyed.is_some() {
        return keyed;
    }
    if response.drag_started() {
        ui.data_mut(|d| d.insert_temp(dragging_id, hovered));
    }
    if response.drag_stopped() {
        ui.data_mut(|d| d.remove::<Option<usize>>(dragging_id));
    }
    if response.dragged()
        && let Some(p) = response.interact_pointer_pos()
    {
        return Some(match dragging {
            Some(i) => FieldAction::Move(i, to_value(p)),
            None => FieldAction::SetInput(local_to_blend(p, lo, hi, rect)),
        });
    }
    if response.secondary_clicked()
        && let Some(i) = hovered
    {
        return Some(FieldAction::Remove(i));
    }
    if response.double_clicked()
        && hovered.is_none()
        && let Some(p) = response.interact_pointer_pos()
    {
        return Some(FieldAction::Add(to_value(p)));
    }
    if response.clicked()
        && hovered.is_none()
        && let Some(p) = response.interact_pointer_pos()
    {
        return Some(FieldAction::SetInput(local_to_blend(p, lo, hi, rect)));
    }
    None
}

/// The sample list under a blend field (FU-5, the convention of `canvas_keys`): Up / Down move
/// the focus; Enter puts the preview input on the sample (a click's `SetInput`); Alt+arrow moves
/// it one `nudge` step (Shift: four; a 1D space also takes plain Left / Right), snapped as a drag
/// is (`Move`); Delete removes it (the right-click's `Remove`); Ctrl+Shift+A adds a sample one
/// step to its right (the double-click's `Add`).
fn sample_keys(
    ui: &mut egui::Ui,
    keys: &mut crate::canvas_keys::ListKeys,
    (points, labels): (&[[f32; 2]], &[String]),
    one_d: bool,
    nudge: [f32; 2],
    snap_step: f32,
) -> Option<FieldAction> {
    use crate::canvas_keys::{RowKey, list_rows};
    let name = |i: usize| {
        let label = labels.get(i).map_or("", String::as_str);
        match points.get(i) {
            Some(p) if one_d => format!("Sample {}: {label} at {:.2}", i + 1, p[0]),
            Some(p) => format!("Sample {}: {label} at {:.2}, {:.2}", i + 1, p[0], p[1]),
            None => String::new(),
        }
    };
    let height =
        (ui.spacing().interact_size.y + ui.spacing().item_spacing.y) * points.len().clamp(1, 5) as f32;
    let snapped = |v: [f32; 2]| {
        if one_d {
            [snap(v[0], snap_step), 0.0]
        } else {
            [snap(v[0], snap_step), snap(v[1], snap_step)]
        }
    };
    let mut out = None;
    for event in list_rows(
        ui,
        ("blend-field-samples", one_d),
        keys,
        points.len(),
        height,
        None,
        &name,
    ) {
        out = match event {
            RowKey::Activate(i) => points.get(i).map(|p| FieldAction::SetInput(*p)),
            RowKey::Delete(i) => Some(FieldAction::Remove(i)),
            RowKey::Palette(i) => points
                .get(i)
                .map(|p| FieldAction::Add(snapped([p[0] + nudge[0], p[1]]))),
            RowKey::Arrow {
                row,
                dx,
                dy,
                alt,
                coarse,
            } if alt || one_d => {
                let k = if coarse { 4.0 } else { 1.0 };
                // Screen up is +y in the blend space.
                let dy = if one_d { 0.0 } else { -(dy as f32) };
                points.get(row).map(|p| {
                    FieldAction::Move(
                        row,
                        snapped([p[0] + dx as f32 * nudge[0] * k, p[1] + dy * nudge[1] * k]),
                    )
                })
            }
            RowKey::Arrow { .. } | RowKey::Connect(_) => None,
        }
        .or(out);
    }
    out
}

/// Points of a 1D space (`[value, 0]`).
#[must_use]
pub fn points_1d(space: &BlendSpace1D) -> Vec<[f32; 2]> {
    space.samples.iter().map(|s| [s.value, 0.0]).collect()
}

/// Points of a 2D space.
#[must_use]
pub fn points_2d(space: &BlendSpace2D) -> Vec<[f32; 2]> {
    space.samples.iter().map(|s| s.point).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_round_trips_and_ranges_cover_points() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(200.0, 100.0));
        let (lo, hi) = field_range(&[[-2.0, 0.0], [6.0, 3.0]], [1.0, 1.0]);
        assert!(lo[0] < -2.0 && hi[0] > 6.0 && hi[1] > 3.0);
        let p = blend_to_local([1.0, 2.0], lo, hi, rect);
        let back = local_to_blend(p, lo, hi, rect);
        assert!((back[0] - 1.0).abs() < 1e-4 && (back[1] - 2.0).abs() < 1e-4);
        assert_eq!(snap(1.26, 0.5), 1.5);
        assert_eq!(snap(1.26, 0.0), 1.26);
    }

    /// FU-5: the sample list selects a sample (Enter: the click's `SetInput` at it), moves it
    /// (Alt+Right: the drag's `Move`, snapped) and removes another (Delete: the right-click's
    /// `Remove`) with keys only; each is the action the mouse produces, which the graph inspector
    /// applies through the same edit.
    #[test]
    fn the_sample_list_gives_the_mouse_actions_by_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        let points = [[0.0, 0.0], [1.0, 1.0]];
        let labels = ["Idle".to_string(), "Run".to_string()];
        let ctx = egui::Context::default();
        let mut got = None;
        let draw = |events: Vec<egui::Event>, got: &mut Option<FieldAction>| {
            frame(&ctx, [500.0, 700.0], events, |ui| {
                *got = blend_field(
                    ui,
                    egui::Id::new("bs"),
                    false,
                    &points,
                    &labels,
                    &[],
                    &[],
                    [0.5, 0.5],
                    0.25,
                );
            })
        };
        let first = draw(vec![], &mut got);
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas)
                .contains(&"Blend space 2D canvas, 2 samples".to_string())
        );
        tab_to(
            &ctx,
            [500.0, 700.0],
            "Sample 2: Run at 1.00, 1.00",
            false,
            10,
            |ui| {
                blend_field(
                    ui,
                    egui::Id::new("bs"),
                    false,
                    &points,
                    &labels,
                    &[],
                    &[],
                    [0.5, 0.5],
                    0.25,
                );
            },
        );
        draw(vec![key(Key::Enter, Modifiers::NONE)], &mut got);
        assert_eq!(
            got,
            Some(FieldAction::SetInput([1.0, 1.0])),
            "Enter: the click's SetInput"
        );
        draw(vec![key(Key::ArrowRight, Modifiers::ALT)], &mut got);
        assert_eq!(
            got,
            Some(FieldAction::Move(1, [1.25, 1.0])),
            "one snap step right"
        );
        draw(
            vec![key(Key::ArrowUp, Modifiers::ALT | Modifiers::SHIFT)],
            &mut got,
        );
        assert_eq!(
            got,
            Some(FieldAction::Move(1, [1.0, 2.0])),
            "Shift: four steps up"
        );
        draw(vec![key(Key::ArrowUp, Modifiers::NONE)], &mut got);
        draw(vec![], &mut got);
        draw(vec![key(Key::Delete, Modifiers::NONE)], &mut got);
        assert_eq!(got, Some(FieldAction::Remove(0)), "the right-click's Remove");
    }
}
