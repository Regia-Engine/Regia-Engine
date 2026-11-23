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
//! A pan/zoom view between a value space (time × value, y up) and screen points (y down),
//! with "nice" grid steps — shared by the curve editor, the clip timeline and the blend-space
//! grid.
//!
//! Portions ported from Fyrox (<https://github.com/FyroxEngine/Fyrox>),
//! `fyrox-ui/src/curve/mod.rs` (`CurveTransform`, `StepIterator`, `standardize_step`,
//! zoom-to-fit), Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT
//! licence (notice in `docs/ported-sources.toml` / `THIRD_PARTY_NOTICES.md`). Adapted: plain
//! affine maths on egui types instead of nalgebra matrices, no interior mutability, and the
//! step table extended downwards for sub-0.1 s timeline ticks.

/// Grid lines are at least this many points apart.
pub const STANDARD_GRID_SIZE: f32 = 50.0;

/// Readable step sizes the grid rounds up to.
const STANDARD_STEP_SIZES: [f32; 14] = [
    0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0,
];

/// Rounds `step` up to the next standard step size (or returns it unchanged past the table).
#[must_use]
pub fn standardize_step(step: f32) -> f32 {
    STANDARD_STEP_SIZES
        .iter()
        .copied()
        .find(|x| step <= *x)
        .unwrap_or(step)
}

/// Iterates multiples of `step_size` covering `start..=end` (one past each end).
#[derive(Debug, Clone)]
pub struct StepIterator {
    /// The step.
    pub step_size: f32,
    index: i64,
    end: i64,
}

impl StepIterator {
    /// An iterator starting at or before `start` and ending at or after `end`.
    #[must_use]
    pub fn new(step: f32, start: f32, end: f32) -> Self {
        let step = step.max(1e-6);
        Self {
            step_size: step,
            index: (start / step).floor() as i64,
            end: ((end / step).ceil() as i64)
                .saturating_add(1)
                .min((start / step).floor() as i64 + 4096),
        }
    }
}

impl Iterator for StepIterator {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.index >= self.end {
            return None;
        }
        let value = self.index as f32 * self.step_size;
        self.index += 1;
        Some(value)
    }
}

/// The view: `position` (value-space point at the screen rect's centre) and `scale` (points
/// per value unit, per axis).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View2d {
    /// Value-space point shown at the centre of the rect.
    pub position: egui::Vec2,
    /// Points per value unit.
    pub scale: egui::Vec2,
    /// Screen rect the view maps into.
    pub rect: egui::Rect,
}

impl Default for View2d {
    fn default() -> Self {
        Self {
            position: egui::Vec2::ZERO,
            scale: egui::vec2(100.0, 100.0),
            rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0)),
        }
    }
}

impl View2d {
    /// Value space → screen (y flipped so values grow upwards).
    #[must_use]
    pub fn to_screen(&self, value: egui::Vec2) -> egui::Pos2 {
        let c = self.rect.center();
        egui::pos2(
            c.x + (value.x - self.position.x) * self.scale.x,
            c.y - (value.y - self.position.y) * self.scale.y,
        )
    }

    /// Screen → value space.
    #[must_use]
    pub fn to_value(&self, screen: egui::Pos2) -> egui::Vec2 {
        let c = self.rect.center();
        egui::vec2(
            self.position.x + (screen.x - c.x) / self.scale.x.max(1e-6),
            self.position.y - (screen.y - c.y) / self.scale.y.max(1e-6),
        )
    }

    /// Vertical grid positions (value-space x), at least `grid` points apart.
    #[must_use]
    pub fn x_steps(&self, grid: f32) -> StepIterator {
        let step = standardize_step(grid / self.scale.x.clamp(0.001, 100_000.0));
        let left = self.to_value(self.rect.left_top()).x;
        let right = self.to_value(self.rect.right_top()).x;
        StepIterator::new(step, left, right)
    }

    /// Horizontal grid positions (value-space y), at least `grid` points apart.
    #[must_use]
    pub fn y_steps(&self, grid: f32) -> StepIterator {
        let step = standardize_step(grid / self.scale.y.clamp(0.001, 100_000.0));
        let bottom = self.to_value(self.rect.left_bottom()).y;
        let top = self.to_value(self.rect.left_top()).y;
        StepIterator::new(step, bottom, top)
    }

    /// Zooms so value-space `bounds` (`[x0, y0, x1, y1]`) fill the rect with `margin` (fraction).
    pub fn zoom_to_fit(&mut self, bounds: [f32; 4], margin: f32) {
        let w = (bounds[2] - bounds[0]).max(0.001) * (1.0 + 2.0 * margin);
        let h = (bounds[3] - bounds[1]).max(0.001) * (1.0 + 2.0 * margin);
        self.scale = egui::vec2(self.rect.width() / w, self.rect.height() / h);
        self.position = egui::vec2((bounds[0] + bounds[2]) * 0.5, (bounds[1] + bounds[3]) * 0.5);
    }

    /// Pans by a screen-space drag.
    pub fn pan(&mut self, drag: egui::Vec2) {
        self.position.x -= drag.x / self.scale.x.max(1e-6);
        self.position.y += drag.y / self.scale.y.max(1e-6);
    }

    /// Zooms by `factor` per axis around the screen point `anchor` (which stays put).
    pub fn zoom_about(&mut self, anchor: egui::Pos2, factor: egui::Vec2) {
        let before = self.to_value(anchor);
        self.scale = egui::vec2(
            (self.scale.x * factor.x).clamp(0.01, 100_000.0),
            (self.scale.y * factor.y).clamp(0.01, 100_000.0),
        );
        let after = self.to_value(anchor);
        self.position += before - after;
    }

    /// Paints the grid lines and their labels into `painter`.
    pub fn paint_grid(&self, painter: &egui::Painter, label_x: impl Fn(f32) -> String, horizontal: bool) {
        let line = egui::Stroke::new(1.0, egui::Color32::from_gray(52));
        let text = egui::Color32::from_gray(150);
        let font = egui::FontId::monospace(10.0);
        for x in self.x_steps(STANDARD_GRID_SIZE) {
            let sx = self.to_screen(egui::vec2(x, 0.0)).x;
            painter.line_segment(
                [
                    egui::pos2(sx, self.rect.top()),
                    egui::pos2(sx, self.rect.bottom()),
                ],
                line,
            );
            painter.text(
                egui::pos2(sx + 2.0, self.rect.top() + 1.0),
                egui::Align2::LEFT_TOP,
                label_x(x),
                font.clone(),
                text,
            );
        }
        if horizontal {
            for y in self.y_steps(STANDARD_GRID_SIZE) {
                let sy = self.to_screen(egui::vec2(0.0, y)).y;
                painter.line_segment(
                    [
                        egui::pos2(self.rect.left(), sy),
                        egui::pos2(self.rect.right(), sy),
                    ],
                    line,
                );
                painter.text(
                    egui::pos2(self.rect.left() + 2.0, sy - 1.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{y:.2}"),
                    font.clone(),
                    text,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_round_up_and_cover_the_range() {
        assert_eq!(standardize_step(0.3), 0.5);
        assert_eq!(standardize_step(0.004), 0.01);
        assert_eq!(standardize_step(1000.0), 1000.0);
        let values: Vec<f32> = StepIterator::new(0.5, 0.2, 1.4).collect();
        assert_eq!(values, vec![0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn transforms_invert_and_zoom_keeps_the_anchor() {
        let mut view = View2d {
            rect: egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(400.0, 200.0)),
            ..View2d::default()
        };
        view.zoom_to_fit([0.0, -1.0, 2.0, 1.0], 0.0);
        let p = view.to_screen(egui::vec2(0.0, 1.0));
        assert!((p.x - 10.0).abs() < 1e-3 && (p.y - 20.0).abs() < 1e-3, "{p:?}");
        let v = view.to_value(egui::pos2(410.0, 220.0));
        assert!((v.x - 2.0).abs() < 1e-4 && (v.y + 1.0).abs() < 1e-4, "{v:?}");
        let anchor = egui::pos2(100.0, 50.0);
        let before = view.to_value(anchor);
        view.zoom_about(anchor, egui::vec2(2.0, 0.5));
        let after = view.to_value(anchor);
        assert!((before - after).length() < 1e-4);
        view.pan(egui::vec2(20.0, 0.0));
        assert!(
            view.to_value(anchor).x < after.x,
            "dragging right moves the view left"
        );
        // Grid lines at least 50 points apart.
        let xs: Vec<f32> = view.x_steps(STANDARD_GRID_SIZE).collect();
        assert!(xs.len() >= 2);
        let gap = view.to_screen(egui::vec2(xs[1], 0.0)).x - view.to_screen(egui::vec2(xs[0], 0.0)).x;
        assert!(gap >= STANDARD_GRID_SIZE - 1e-3, "{gap}");
    }
}
