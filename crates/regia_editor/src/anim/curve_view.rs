// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// fyrox-ui/src/curve/mod.rs. Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors,
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
//! The curve editor widget: a zoomable, pannable grid with the curve, its keys and (for cubic
//! keys) tangent handles; drag keys and handles, double-click to add, right-click for the key
//! menu, scroll to zoom (Shift/Ctrl = one axis), drag with the secondary button to pan.
//!
//! Portions ported from Fyrox (<https://github.com/FyroxEngine/Fyrox>),
//! `fyrox-ui/src/curve/mod.rs` (`CurveEditor`: key picking with a square hit box, tangent
//! handles at a fixed screen radius along the tangent angle, per-axis zoom with modifiers,
//! zoom-to-fit, cubic drawing by subdividing each segment), Copyright (c) 2019-present Dmitry
//! Stepanov and Fyrox Engine contributors, MIT licence (notice in `docs/ported-sources.toml`).
//! Adapted to egui and to `regia_animation::curve::Curve` (keys by index, slopes in value per
//! second).

use regia_animation::curve::{Curve, CurveKeyKind};

use super::view2d::View2d;

/// Key hit-box size, points (Fyrox's `key_size`).
pub const KEY_SIZE: f32 = 8.0;
/// Tangent handle distance from its key, points (Fyrox's `handle_radius`).
pub const HANDLE_RADIUS: f32 = 36.0;

/// An edit the user made on the curve this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum CurveAction {
    /// Key `index` moved to `(location, value)`.
    Move(usize, f32, f32),
    /// Add a key at `(location, value)`.
    Add(f32, f32),
    /// Remove key `index`.
    Remove(usize),
    /// Change key `index`'s interpolation.
    SetKind(usize, CurveKeyKind),
    /// Scrub the playhead to a location.
    Scrub(f32),
}

/// What a pointer position hits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    /// A key.
    Key(usize),
    /// A key's left tangent handle.
    LeftTangent(usize),
    /// A key's right tangent handle.
    RightTangent(usize),
}

/// The screen position of a tangent handle for `slope`, on the `side` (−1 left, +1 right).
#[must_use]
pub fn handle_position(view: &View2d, key: egui::Pos2, slope: f32, side: f32) -> egui::Pos2 {
    let dir = egui::vec2(view.scale.x, -slope * view.scale.y).normalized();
    key + dir * HANDLE_RADIUS * side
}

/// The slope a handle dragged to `pointer` encodes (value per second).
#[must_use]
pub fn slope_from_handle(view: &View2d, key: egui::Pos2, pointer: egui::Pos2) -> f32 {
    let d = pointer - key;
    if d.x.abs() < 1e-3 {
        return if d.y < 0.0 { 1e3 } else { -1e3 };
    }
    -(d.y / view.scale.y.max(1e-6)) / (d.x / view.scale.x.max(1e-6))
}

/// Finds what `pos` hits (keys first, then handles), like Fyrox's `pick`.
#[must_use]
pub fn pick(curve: &Curve, view: &View2d, pos: egui::Pos2) -> Option<Pick> {
    for (i, key) in curve.keys.iter().enumerate() {
        let s = view.to_screen(egui::vec2(key.location, key.value));
        if egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE))
            .expand(2.0)
            .contains(pos)
        {
            return Some(Pick::Key(i));
        }
        if let CurveKeyKind::Cubic {
            left_tangent,
            right_tangent,
        } = key.kind
        {
            if handle_position(view, s, left_tangent, -1.0).distance(pos) <= KEY_SIZE * 0.75 {
                return Some(Pick::LeftTangent(i));
            }
            if handle_position(view, s, right_tangent, 1.0).distance(pos) <= KEY_SIZE * 0.75 {
                return Some(Pick::RightTangent(i));
            }
        }
    }
    None
}

/// Draws the curve editor into `rect`-sized space and returns at most one action.
pub fn curve_editor(
    ui: &mut egui::Ui,
    id: egui::Id,
    curve: &Curve,
    view: &mut View2d,
    selected: &mut Option<usize>,
    playhead: Option<f32>,
) -> Option<CurveAction> {
    let size = ui.available_size().max(egui::vec2(120.0, 80.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let fresh = view.rect.width() < 2.0 || ui.data(|d| d.get_temp::<bool>(id.with("fitted"))).is_none();
    view.rect = rect;
    if fresh {
        let b = curve.bounds();
        view.zoom_to_fit([b[0], b[1], b[2].max(b[0] + 1.0), b[3].max(b[1] + 0.5)], 0.1);
        ui.data_mut(|d| d.insert_temp(id.with("fitted"), true));
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(22));
    view.paint_grid(&painter, |x| format!("{x:.2}s"), true);

    // The curve: sampled per pixel column (cubic segments subdivide naturally).
    let mut points = Vec::new();
    let steps = (rect.width() / 2.0).max(8.0) as usize;
    for i in 0..=steps {
        let sx = rect.left() + rect.width() * i as f32 / steps as f32;
        let t = view.to_value(egui::pos2(sx, 0.0)).x;
        points.push(view.to_screen(egui::vec2(t, curve.value_at(t))));
    }
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 200, 255)),
    ));

    for (i, key) in curve.keys.iter().enumerate() {
        let s = view.to_screen(egui::vec2(key.location, key.value));
        let is_selected = *selected == Some(i);
        if let CurveKeyKind::Cubic {
            left_tangent,
            right_tangent,
        } = key.kind
        {
            for (slope, side) in [(left_tangent, -1.0), (right_tangent, 1.0)] {
                let h = handle_position(view, s, slope, side);
                painter.line_segment([s, h], egui::Stroke::new(1.0, egui::Color32::from_gray(160)));
                painter.circle_filled(h, 3.5, egui::Color32::from_rgb(230, 200, 90));
            }
        }
        let fill = if is_selected {
            crate::theme::ui_tokens(painter.ctx()).accents.active
        } else {
            egui::Color32::WHITE
        };
        painter.rect_filled(
            egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE)),
            1.0,
            fill,
        );
    }
    // The key list's keyboard focus (FU-5).
    let keys_id = id.with("keys");
    let mut keys: crate::canvas_keys::ListKeys = ui.data(|d| d.get_temp(keys_id)).unwrap_or_default();
    if let Some(key) = keys.focused().and_then(|i| curve.keys.get(i)) {
        let s = view.to_screen(egui::vec2(key.location, key.value));
        crate::canvas_keys::focus_ring(
            &painter,
            egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE, KEY_SIZE)),
        );
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("Curve canvas, {} keys", curve.keys.len()),
    );
    if let Some(t) = playhead {
        let x = view.to_screen(egui::vec2(t, 0.0)).x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.5, crate::theme::ui_tokens(painter.ctx()).accents.alert),
        );
    }

    // The keyboard route, drawn every frame (a row that is not laid out loses the focus).
    let keyed = key_list(ui, &mut keys, curve, selected);
    ui.data_mut(|d| d.insert_temp(keys_id, keys));
    if keyed.is_some() {
        return keyed;
    }

    // Zoom (Fyrox: Shift = time only, Ctrl = value only) and pan.
    if response.hovered() {
        let (scroll, shift, ctrl) =
            ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.shift, i.modifiers.ctrl));
        if scroll != 0.0
            && let Some(anchor) = response.hover_pos()
        {
            let k = (1.0 + scroll * 0.002).clamp(0.5, 2.0);
            let factor = if shift {
                egui::vec2(k, 1.0)
            } else if ctrl {
                egui::vec2(1.0, k)
            } else {
                egui::vec2(k, k)
            };
            view.zoom_about(anchor, factor);
        }
    }
    if response.dragged_by(egui::PointerButton::Secondary) || response.dragged_by(egui::PointerButton::Middle)
    {
        view.pan(response.drag_delta());
        return None;
    }

    let drag_id = id.with("drag");
    if response.drag_started_by(egui::PointerButton::Primary) {
        let hit = response.interact_pointer_pos().and_then(|p| pick(curve, view, p));
        ui.data_mut(|d| d.insert_temp(drag_id, hit));
        if let Some(Pick::Key(i) | Pick::LeftTangent(i) | Pick::RightTangent(i)) = hit {
            *selected = Some(i);
        }
    }
    let dragging: Option<Pick> = ui.data(|d| d.get_temp::<Option<Pick>>(drag_id)).flatten();
    if response.drag_stopped() {
        ui.data_mut(|d| d.remove::<Option<Pick>>(drag_id));
    }
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some(p) = response.interact_pointer_pos()
    {
        let v = view.to_value(p);
        return match dragging {
            Some(Pick::Key(i)) => Some(CurveAction::Move(i, v.x.max(0.0), v.y)),
            Some(Pick::LeftTangent(i) | Pick::RightTangent(i)) => {
                let key = curve.keys.get(i)?;
                let s = view.to_screen(egui::vec2(key.location, key.value));
                let slope = slope_from_handle(view, s, p);
                let CurveKeyKind::Cubic {
                    left_tangent,
                    right_tangent,
                } = key.kind
                else {
                    return None;
                };
                // Dragging one handle keeps the tangent smooth (both sides follow) unless Alt.
                let broken = ui.input(|i| i.modifiers.alt);
                let (l, r) = match dragging {
                    Some(Pick::LeftTangent(_)) => (slope, if broken { right_tangent } else { slope }),
                    _ => (if broken { left_tangent } else { slope }, slope),
                };
                Some(CurveAction::SetKind(
                    i,
                    CurveKeyKind::Cubic {
                        left_tangent: l,
                        right_tangent: r,
                    },
                ))
            }
            None => Some(CurveAction::Scrub(v.x.max(0.0))),
        };
    }
    if response.double_clicked()
        && let Some(p) = response.interact_pointer_pos()
        && pick(curve, view, p).is_none()
    {
        let v = view.to_value(p);
        return Some(CurveAction::Add(v.x.max(0.0), v.y));
    }
    if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        *selected = match pick(curve, view, p) {
            Some(Pick::Key(i) | Pick::LeftTangent(i) | Pick::RightTangent(i)) => Some(i),
            None => None,
        };
    }
    let hovered_key = response
        .hover_pos()
        .and_then(|p| pick(curve, view, p))
        .map(|p| match p {
            Pick::Key(i) | Pick::LeftTangent(i) | Pick::RightTangent(i) => i,
        });
    let mut action = None;
    response.context_menu(|ui| {
        let Some(i) = hovered_key.or(*selected) else {
            if ui.button("Zoom to fit").clicked() {
                let b = curve.bounds();
                view.zoom_to_fit([b[0], b[1], b[2].max(b[0] + 1.0), b[3].max(b[1] + 0.5)], 0.1);
                ui.close();
            }
            return;
        };
        ui.label(format!("Key {i}"));
        for (label, kind) in [
            ("Constant", CurveKeyKind::Constant),
            ("Linear", CurveKeyKind::Linear),
            ("Cubic (flat)", CurveKeyKind::cubic_from_angles(0.0, 0.0)),
        ] {
            if ui.button(label).clicked() {
                action = Some(CurveAction::SetKind(i, kind));
                ui.close();
            }
        }
        if ui.button("Delete key").clicked() {
            action = Some(CurveAction::Remove(i));
            ui.close();
        }
    });
    action
}

/// How far Left / Right move a key, seconds (Shift: ten times).
pub const KEY_TIME_STEP: f32 = 1.0 / 30.0;

/// How far Alt+Up / Alt+Down move a key's value: a twentieth of the curve's value span (at least
/// 0.01; Shift: four times).
#[must_use]
pub fn key_value_step(curve: &Curve) -> f32 {
    let b = curve.bounds();
    ((b[3] - b[1]) / 20.0).max(0.01)
}

/// The key list under the curve (FU-5, the convention of `canvas_keys`): Up / Down move the
/// focus; Enter selects the key (a click); Left / Right move it in time, Alt+Up / Alt+Down in
/// value (Shift: coarser), each the drag's `Move`; Delete is the key menu's `Remove`;
/// Ctrl+Shift+A adds a key one step later on the curve (the double-click's `Add`). The key menu's
/// interpolation kinds stay on the right-click (agents: the clip's curve edit).
fn key_list(
    ui: &mut egui::Ui,
    keys: &mut crate::canvas_keys::ListKeys,
    curve: &Curve,
    selected: &mut Option<usize>,
) -> Option<CurveAction> {
    use crate::canvas_keys::{RowKey, list_rows};
    let n = curve.keys.len();
    let name = |i: usize| {
        curve.keys.get(i).map_or_else(String::new, |k| {
            let kind = match k.kind {
                CurveKeyKind::Constant => "constant",
                CurveKeyKind::Linear => "linear",
                CurveKeyKind::Cubic { .. } => "cubic",
            };
            format!(
                "Key {} of {n}: {:.2} s, value {:.2}, {kind}",
                i + 1,
                k.location,
                k.value
            )
        })
    };
    let height = (ui.spacing().interact_size.y + ui.spacing().item_spacing.y) * n.clamp(1, 5) as f32;
    let mut out = None;
    for event in list_rows(ui, "curve-keys", keys, n, height, *selected, &name) {
        out = match event {
            RowKey::Activate(i) => {
                *selected = Some(i);
                None
            }
            RowKey::Delete(i) => Some(CurveAction::Remove(i)),
            RowKey::Palette(i) => curve.keys.get(i).map(|k| {
                let t = k.location + KEY_TIME_STEP;
                CurveAction::Add(t, curve.value_at(t))
            }),
            RowKey::Arrow {
                row,
                dx,
                dy,
                alt,
                coarse,
            } if dx != 0 || alt => curve.keys.get(row).map(|k| {
                let c = if coarse { 10.0 } else { 1.0 };
                let v = if coarse { 4.0 } else { 1.0 };
                CurveAction::Move(
                    row,
                    (k.location + dx as f32 * KEY_TIME_STEP * c).max(0.0),
                    k.value - dy as f32 * key_value_step(curve) * v,
                )
            }),
            RowKey::Arrow { .. } | RowKey::Connect(_) => None,
        }
        .or(out);
    }
    out
}

/// Applies an action to a curve (the pending edit's body). Returns the moved key's new index.
pub fn apply_action(curve: &mut Curve, action: &CurveAction) -> Result<Option<usize>, String> {
    // A non-finite key (a NaN typed into a field, a drag that divided by zero) sorts
    // inconsistently and makes `value_at` sample NaN that bakes into the clip (audit-gameplay Low).
    if let CurveAction::Move(_, loc, val) | CurveAction::Add(loc, val) = action
        && !(loc.is_finite() && val.is_finite())
    {
        return Err(format!("a key needs a finite time and value, got ({loc}, {val})"));
    }
    match action {
        CurveAction::Move(i, loc, val) => curve
            .move_key(*i, *loc, *val)
            .map(Some)
            .ok_or_else(|| format!("no key {i}")),
        CurveAction::Add(loc, val) => Ok(Some(curve.add_key(regia_animation::curve::CurveKey::new(
            *loc,
            *val,
            CurveKeyKind::Linear,
        )))),
        CurveAction::Remove(i) => curve
            .remove_key(*i)
            .map(|_| None)
            .ok_or_else(|| format!("no key {i}")),
        CurveAction::SetKind(i, kind) => {
            let key = curve.keys.get_mut(*i).ok_or_else(|| format!("no key {i}"))?;
            key.kind = *kind;
            Ok(Some(*i))
        }
        CurveAction::Scrub(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::curve::CurveKey;

    #[test]
    fn handles_encode_slopes_and_picking_finds_keys() {
        let mut view = View2d {
            rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0)),
            ..View2d::default()
        };
        view.zoom_to_fit([0.0, 0.0, 2.0, 1.0], 0.0);
        let key = view.to_screen(egui::vec2(1.0, 0.5));
        for slope in [-2.0f32, 0.0, 0.7, 3.0] {
            let h = handle_position(&view, key, slope, 1.0);
            assert!((key.distance(h) - HANDLE_RADIUS).abs() < 1e-3);
            assert!(
                (slope_from_handle(&view, key, h) - slope).abs() < 1e-3,
                "slope {slope}"
            );
        }
        let curve = Curve::from(vec![
            CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
            CurveKey::new(1.0, 0.5, CurveKeyKind::cubic_from_angles(0.3, 0.3)),
        ]);
        assert_eq!(pick(&curve, &view, key), Some(Pick::Key(1)));
        let right = handle_position(&view, key, 0.3f32.tan(), 1.0);
        assert_eq!(pick(&curve, &view, right), Some(Pick::RightTangent(1)));
        assert_eq!(pick(&curve, &view, egui::pos2(390.0, 5.0)), None);

        let mut c = curve.clone();
        assert_eq!(
            apply_action(&mut c, &CurveAction::Add(0.5, 1.0)).expect("add"),
            Some(1)
        );
        assert_eq!(
            apply_action(&mut c, &CurveAction::Move(1, 1.5, 0.0)).expect("move"),
            Some(2)
        );
        assert!(apply_action(&mut c, &CurveAction::Remove(9)).is_err());
        let keys = c.keys.clone();
        for bad in [
            CurveAction::Add(f32::NAN, 1.0),
            CurveAction::Add(0.5, f32::INFINITY),
            CurveAction::Move(0, f32::NAN, 0.0),
        ] {
            assert!(apply_action(&mut c, &bad).is_err(), "{bad:?}");
        }
        assert_eq!(c.keys, keys, "a refused key changes nothing");
        apply_action(&mut c, &CurveAction::SetKind(0, CurveKeyKind::Constant)).expect("kind");
        assert_eq!(c.keys[0].kind, CurveKeyKind::Constant);
    }

    /// FU-5: the key list selects a key (Enter: the click's selection), moves it in time and
    /// value (Right, Alt+Up: the drag's `Move`) and removes another (Delete: the menu's `Remove`);
    /// the curve after `apply_action` equals the one the mouse's actions build.
    #[test]
    fn the_key_list_edits_by_keyboard_what_the_mouse_edits() {
        use crate::canvas_keys::test_input::{frame, key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [500.0, 500.0];
        let start = Curve::from(vec![
            CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
            CurveKey::new(1.0, 0.5, CurveKeyKind::Linear),
        ]);
        let step = key_value_step(&start);
        let mut mouse = start.clone();
        for action in [
            CurveAction::Move(1, 1.0 + KEY_TIME_STEP, 0.5),
            CurveAction::Move(1, 1.0 + KEY_TIME_STEP, 0.5 + step),
            CurveAction::Remove(0),
        ] {
            apply_action(&mut mouse, &action).expect("mouse action");
        }

        let mut curve = start.clone();
        let mut view = View2d::default();
        let mut selected = None;
        let ctx = egui::Context::default();
        let run =
            |events: Vec<egui::Event>, curve: &mut Curve, view: &mut View2d, selected: &mut Option<usize>| {
                let mut got = None;
                let update = frame(&ctx, SIZE, events, |ui| {
                    got = curve_editor(ui, egui::Id::new("curve"), curve, view, selected, None);
                });
                if let Some(action) = got.as_ref().filter(|a| !matches!(a, CurveAction::Scrub(_))) {
                    apply_action(curve, action).expect("keyboard action");
                }
                update
            };
        let first = run(vec![], &mut curve, &mut view, &mut selected);
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas).contains(&"Curve canvas, 2 keys".to_string())
        );
        tab_to(
            &ctx,
            SIZE,
            "Key 2 of 2: 1.00 s, value 0.50, linear",
            false,
            10,
            |ui| {
                let _ = curve_editor(ui, egui::Id::new("curve"), &curve, &mut view, &mut selected, None);
            },
        );
        run(
            vec![key(Key::Enter, Modifiers::NONE)],
            &mut curve,
            &mut view,
            &mut selected,
        );
        assert_eq!(selected, Some(1), "Enter selects");
        run(
            vec![key(Key::ArrowRight, Modifiers::NONE)],
            &mut curve,
            &mut view,
            &mut selected,
        );
        run(
            vec![key(Key::ArrowUp, Modifiers::ALT)],
            &mut curve,
            &mut view,
            &mut selected,
        );
        run(
            vec![key(Key::ArrowUp, Modifiers::NONE)],
            &mut curve,
            &mut view,
            &mut selected,
        );
        run(vec![], &mut curve, &mut view, &mut selected);
        run(
            vec![key(Key::Delete, Modifiers::NONE)],
            &mut curve,
            &mut view,
            &mut selected,
        );
        assert_eq!(curve.keys, mouse.keys);
    }
}

/// The multi-curve view's per-curve state (spec addendum § 11.3, P-208): which curves are hidden
/// and which are locked. Curves are keyed by a caller-chosen string (`curve:<name>`,
/// `property:<path>`, `track:<index>:<component>`). Every curve is visible and unlocked until the
/// user says otherwise. View state only: never saved, never on the undo stack.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MultiCurve {
    /// Hidden curves.
    pub hidden: std::collections::BTreeSet<String>,
    /// Locked curves: drawn, but their keys cannot be edited.
    pub locked: std::collections::BTreeSet<String>,
}

impl MultiCurve {
    /// Whether the curve `key` is drawn.
    #[must_use]
    pub fn visible(&self, key: &str) -> bool {
        !self.hidden.contains(key)
    }

    /// Whether the curve `key` is locked.
    #[must_use]
    pub fn is_locked(&self, key: &str) -> bool {
        self.locked.contains(key)
    }

    /// Shows or hides the curve `key`.
    pub fn set_visible(&mut self, key: &str, visible: bool) {
        if visible {
            self.hidden.remove(key);
        } else {
            self.hidden.insert(key.to_string());
        }
    }

    /// Locks or unlocks the curve `key`.
    pub fn set_locked(&mut self, key: &str, locked: bool) {
        if locked {
            self.locked.insert(key.to_string());
        } else {
            self.locked.remove(key);
        }
    }

    /// Drops an edit of a locked curve (scrubbing still works).
    #[must_use]
    pub fn filter(&self, key: &str, action: Option<CurveAction>) -> Option<CurveAction> {
        match action {
            Some(CurveAction::Scrub(t)) => Some(CurveAction::Scrub(t)),
            Some(_) if self.is_locked(key) => None,
            other => other,
        }
    }
}

/// The curve palette: an ordered list of theme tokens (the three axis colors, then the accents
/// and the data-kind colors), so curve colors follow the theme.
#[must_use]
pub fn curve_palette(tokens: &crate::theme::ThemeTokens) -> Vec<egui::Color32> {
    let mut out = vec![
        tokens.overlay.gizmo_x,
        tokens.overlay.gizmo_y,
        tokens.overlay.gizmo_z,
        tokens.accents.active,
        tokens.accents.select,
        tokens.accents.ok,
        tokens.warn,
        tokens.signal,
    ];
    out.extend(tokens.data_kinds);
    out
}

/// The palette color of the `index`-th curve (wrapping).
#[must_use]
pub fn curve_color(tokens: &crate::theme::ThemeTokens, index: usize) -> egui::Color32 {
    let palette = curve_palette(tokens);
    palette[index % palette.len()]
}

/// `[t0, v0, t1, v1]` bounds covering every point set (each `(time, value)`), padded so a flat
/// or empty set still frames a visible area. Feed it to `View2d::zoom_to_fit` ("frame all").
#[must_use]
pub fn frame_all<'a>(sets: impl IntoIterator<Item = &'a [(f32, f32)]>) -> [f32; 4] {
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for &(t, v) in sets.into_iter().flatten() {
        if t.is_finite() && v.is_finite() {
            b = [b[0].min(t), b[1].min(v), b[2].max(t), b[3].max(v)];
        }
    }
    if b[0] > b[2] {
        b = [0.0, 0.0, 1.0, 1.0];
    }
    [b[0], b[1], b[2].max(b[0] + 1.0), b[3].max(b[1] + 0.5)]
}

/// A curve's keys as `(time, value)` points.
#[must_use]
pub fn key_points(curve: &Curve) -> Vec<(f32, f32)> {
    curve.keys.iter().map(|k| (k.location, k.value)).collect()
}

/// Paints background curves (not the one being edited) in `view`: each sampled every two
/// points, with its keys as small squares, in its color; locked curves are drawn thinner.
/// `sample` returns the curve's value at a time, `None` where it has none.
pub fn paint_background_curve(
    painter: &egui::Painter,
    view: &View2d,
    sample: &dyn Fn(f32) -> Option<f32>,
    keys: &[(f32, f32)],
    color: egui::Color32,
    locked: bool,
) {
    let rect = view.rect;
    let steps = (rect.width() / 2.0).max(8.0) as usize;
    let line: Vec<egui::Pos2> = (0..=steps)
        .filter_map(|i| {
            let sx = rect.left() + rect.width() * i as f32 / steps as f32;
            let t = view.to_value(egui::pos2(sx, 0.0)).x;
            sample(t).map(|v| view.to_screen(egui::vec2(t, v)))
        })
        .collect();
    let width = if locked { 1.0 } else { 1.5 };
    painter.add(egui::Shape::line(line, egui::Stroke::new(width, color)));
    for &(t, v) in keys {
        let s = view.to_screen(egui::vec2(t, v));
        painter.rect_filled(
            egui::Rect::from_center_size(s, egui::vec2(KEY_SIZE * 0.6, KEY_SIZE * 0.6)),
            0.0,
            color,
        );
    }
}

#[cfg(test)]
mod multi_curve_tests {
    use super::*;
    use regia_animation::curve::CurveKey;

    #[test]
    fn visibility_locks_palette_and_frame_all() {
        let mut multi = MultiCurve::default();
        assert!(multi.visible("curve:a") && !multi.is_locked("curve:a"));
        multi.set_visible("curve:a", false);
        multi.set_locked("curve:b", true);
        assert!(!multi.visible("curve:a"));
        assert!(multi.is_locked("curve:b"));
        let edit = Some(CurveAction::Move(0, 1.0, 1.0));
        assert_eq!(
            multi.filter("curve:b", edit.clone()),
            None,
            "a locked curve is not edited"
        );
        assert_eq!(
            multi.filter("curve:b", Some(CurveAction::Scrub(0.5))),
            Some(CurveAction::Scrub(0.5)),
            "a locked curve still scrubs"
        );
        assert_eq!(multi.filter("curve:a", edit.clone()), edit);
        multi.set_visible("curve:a", true);
        multi.set_locked("curve:b", false);
        assert_eq!(multi, MultiCurve::default());

        let tokens = crate::theme::ThemeTokens::regia_default();
        let palette = curve_palette(&tokens);
        assert!(palette.len() >= 8);
        assert_eq!(curve_color(&tokens, palette.len()), palette[0], "wraps");
        assert_ne!(curve_color(&tokens, 0), curve_color(&tokens, 1));

        let a = Curve::from(vec![
            CurveKey::new(0.0, -1.0, CurveKeyKind::Linear),
            CurveKey::new(2.0, 0.0, CurveKeyKind::Linear),
        ]);
        let b = Curve::from(vec![CurveKey::new(3.0, 5.0, CurveKeyKind::Linear)]);
        let (pa, pb) = (key_points(&a), key_points(&b));
        assert_eq!(frame_all([pa.as_slice(), pb.as_slice()]), [0.0, -1.0, 3.0, 5.0]);
        assert_eq!(
            frame_all(std::iter::empty::<&[(f32, f32)]>()),
            [0.0, 0.0, 1.0, 1.0]
        );
    }
}
