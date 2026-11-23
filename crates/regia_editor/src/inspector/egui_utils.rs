// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from bevy-inspector-egui 0.37.0
// (https://github.com/jakobhellermann/bevy-inspector-egui, commit ac67298,
// crates/bevy-inspector-egui/src/{egui_utils.rs,reflect_inspector/errors.rs}),
// Copyright (c) Jakob Hellermann and bevy-inspector-egui contributors,
// licensed MIT OR Apache-2.0; used here under the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Small egui helpers for the inspector: painted add/remove/up/down icon buttons and the
//! explanatory labels shown when a value cannot be edited.

use egui::FontId;

/// A label made of runs in different fonts (monospace type names inside prose).
#[must_use]
pub fn layout_job(text: &[(FontId, &str)]) -> egui::epaint::text::LayoutJob {
    let mut job = egui::epaint::text::LayoutJob::default();
    for (font_id, text) in text {
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font_id.clone(),
                ..Default::default()
            },
        );
    }
    job
}

/// A square button the size of a checkbox icon, painted with line strokes.
struct IconButton {
    rect: egui::Rect,
    response: egui::Response,
    painter: egui::Painter,
    stroke: egui::Stroke,
}

impl IconButton {
    fn new(ui: &mut egui::Ui) -> Self {
        let button_size = egui::Vec2::splat(ui.spacing().icon_width);
        let (response, painter) = ui.allocate_painter(button_size, egui::Sense::click());
        let visuals = if ui.is_enabled() {
            ui.style().interact(&response)
        } else {
            ui.style().noninteractive()
        };
        let rect = response.rect.shrink(2.0).expand(visuals.expansion);
        let stroke = visuals.fg_stroke;
        Self {
            rect,
            response,
            painter,
            stroke,
        }
    }

    fn line(&self, points: [egui::Pos2; 2]) {
        self.painter.line_segment(points, self.stroke);
    }

    fn add_button(self) -> egui::Response {
        self.line([self.rect.center_top(), self.rect.center_bottom()]);
        self.line([self.rect.left_center(), self.rect.right_center()]);
        self.response
    }

    fn remove_button(self) -> egui::Response {
        self.line([self.rect.left_center(), self.rect.right_center()]);
        self.response
    }

    fn up_button(self) -> egui::Response {
        self.line([self.rect.center_top(), self.rect.center_bottom()]);
        self.line([self.rect.center_top(), self.rect.left_center()]);
        self.line([self.rect.center_top(), self.rect.right_center()]);
        self.response
    }

    fn down_button(self) -> egui::Response {
        self.line([self.rect.center_top(), self.rect.center_bottom()]);
        self.line([self.rect.left_center(), self.rect.center_bottom()]);
        self.line([self.rect.right_center(), self.rect.center_bottom()]);
        self.response
    }
}

/// Names the painted button `spoken` (FU-6, `docs/evidence/editor-a11y-2026-11.md`): a
/// `allocate_painter` button has no text, so its AccessKit node gets the `Button` role and the
/// spoken name here (which is also its hover text).
fn named(response: egui::Response, spoken: &str) -> egui::Response {
    crate::a11y_audit::name(response, egui::WidgetType::Button, spoken)
}

/// A "+" button that adds an element to a collection.
pub fn add_button(ui: &mut egui::Ui, spoken: &str) -> egui::Response {
    named(IconButton::new(ui).add_button(), spoken)
}

/// A "−" button that removes an element from a collection.
pub fn remove_button(ui: &mut egui::Ui, spoken: &str) -> egui::Response {
    named(IconButton::new(ui).remove_button(), spoken)
}

/// An "↑" button that moves a collection element up.
pub fn up_button(ui: &mut egui::Ui, spoken: &str) -> egui::Response {
    named(IconButton::new(ui).up_button(), spoken)
}

/// A "↓" button that moves a collection element down.
pub fn down_button(ui: &mut egui::Ui, spoken: &str) -> egui::Response {
    named(IconButton::new(ui).down_button(), spoken)
}

/// The last `::` segment of each path inside a type name (`alloc::vec::Vec<game::Item>` →
/// `Vec<Item>`).
#[must_use]
pub fn pretty_type_name(type_path: &str) -> String {
    let mut out = String::with_capacity(type_path.len());
    let mut segment = String::new();
    for c in type_path.chars() {
        if c.is_alphanumeric() || c == '_' || c == ':' {
            segment.push(c);
        } else {
            out.push_str(segment.rsplit("::").next().unwrap_or(&segment));
            segment.clear();
            out.push(c);
        }
    }
    out.push_str(segment.rsplit("::").next().unwrap_or(&segment));
    out
}

/// Shown for an opaque value with no widget.
pub fn reflect_value_no_impl(ui: &mut egui::Ui, type_path: &str) {
    let name = pretty_type_name(type_path);
    let job = layout_job(&[
        (FontId::monospace(12.0), &name),
        (
            FontId::proportional(13.0),
            " is opaque to reflection and has no inspector widget",
        ),
    ]);
    ui.label(job).on_hover_text(
        "Register an `InspectorEguiImpl` for the type (regia_editor::inspector::impls) to edit it here.",
    );
}

/// Shown when a value must be constructed but its type has no `ReflectDefault`.
pub fn no_default_value(ui: &mut egui::Ui, type_path: &str) {
    let name = pretty_type_name(type_path);
    let job = layout_job(&[
        (FontId::monospace(12.0), &name),
        (FontId::proportional(13.0), " has no "),
        (FontId::monospace(12.0), "#[reflect(Default)]"),
        (FontId::proportional(13.0), ", so no new value of it can be made."),
    ]);
    ui.label(job);
}

/// Hover text for an enum variant whose fields cannot be defaulted.
pub fn unconstructable_variant(ui: &mut egui::Ui, type_path: &str, variant: &str, fields: &[String]) {
    let qualified = format!("{}::{variant}", pretty_type_name(type_path));
    let mut runs = vec![
        (FontId::monospace(12.0), qualified.as_str()),
        (FontId::proportional(13.0), " has fields without a default.\nAdd "),
        (FontId::monospace(12.0), "#[reflect(Default)]"),
        (FontId::proportional(13.0), " to\n"),
    ];
    let names: Vec<String> = fields.iter().map(|f| pretty_type_name(f)).collect();
    for name in &names {
        runs.push((FontId::proportional(13.0), "- "));
        runs.push((FontId::monospace(12.0), name.as_str()));
        runs.push((FontId::proportional(13.0), "\n"));
    }
    ui.label(layout_job(&runs));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names_are_shortened_per_segment() {
        assert_eq!(pretty_type_name("alloc::vec::Vec<game::inv::Item>"), "Vec<Item>");
        assert_eq!(
            pretty_type_name("core::option::Option<regia_core::math::Vec3>"),
            "Option<Vec3>"
        );
        assert_eq!(pretty_type_name("f32"), "f32");
        assert_eq!(pretty_type_name("[f32; 3]"), "[f32; 3]");
    }
}
