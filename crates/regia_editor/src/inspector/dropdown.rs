// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from bevy-inspector-egui 0.37.0
// (https://github.com/jakobhellermann/bevy-inspector-egui, commit ac67298,
// crates/bevy-inspector-egui/src/dropdown.rs),
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

//! A text box with a filtered drop-down list (the inspector's asset picker).

use egui::text::{CCursor, CCursorRange};
use egui::{Id, Popup, PopupCloseBehavior, Response, TextEdit, Ui, Widget, WidgetText};

/// A single-line text edit that opens a list of choices filtered by what is typed. Clicking a
/// choice writes it into the buffer and marks the response changed.
pub struct DropDownBox<'a, F: FnMut(&mut Ui, &str) -> Response, V: AsRef<str>, I: Iterator<Item = V>> {
    buf: &'a mut String,
    popup_id: Id,
    display: F,
    it: I,
    hint_text: WidgetText,
    filter_by_input: bool,
    select_on_focus: bool,
    desired_width: Option<f32>,
}

impl<'a, F: FnMut(&mut Ui, &str) -> Response, V: AsRef<str>, I: Iterator<Item = V>> DropDownBox<'a, F, V, I> {
    /// A drop-down over `it`, editing `buf`; `display` draws one choice and returns its response.
    pub fn from_iter(
        it: impl IntoIterator<IntoIter = I>,
        popup_id: Id,
        buf: &'a mut String,
        display: F,
    ) -> Self {
        Self {
            popup_id,
            it: it.into_iter(),
            display,
            buf,
            hint_text: WidgetText::default(),
            filter_by_input: true,
            select_on_focus: false,
            desired_width: None,
        }
    }

    /// Hint text shown in the empty text edit.
    #[must_use]
    pub fn hint_text(mut self, hint_text: impl Into<WidgetText>) -> Self {
        self.hint_text = hint_text.into();
        self
    }

    /// Whether the list only shows choices containing the typed text (default on).
    #[must_use]
    pub fn filter_by_input(mut self, filter_by_input: bool) -> Self {
        self.filter_by_input = filter_by_input;
        self
    }

    /// Whether focusing the text edit selects its text.
    #[must_use]
    pub fn select_on_focus(mut self, select_on_focus: bool) -> Self {
        self.select_on_focus = select_on_focus;
        self
    }

    /// The text edit's width.
    #[must_use]
    pub fn desired_width(mut self, desired_width: f32) -> Self {
        self.desired_width = Some(desired_width);
        self
    }
}

impl<F: FnMut(&mut Ui, &str) -> Response, V: AsRef<str>, I: Iterator<Item = V>> Widget
    for DropDownBox<'_, F, V, I>
{
    fn ui(self, ui: &mut Ui) -> Response {
        let Self {
            popup_id,
            buf,
            it,
            mut display,
            hint_text,
            filter_by_input,
            select_on_focus,
            desired_width,
        } = self;

        let mut edit = TextEdit::singleline(buf).hint_text(hint_text);
        if let Some(width) = desired_width {
            edit = edit.desired_width(width);
        }
        let mut edit_output = edit.show(ui);
        let mut response = edit_output.response.response;
        if response.gained_focus() {
            if select_on_focus {
                edit_output.state.cursor.set_char_range(Some(CCursorRange::two(
                    CCursor::new(0),
                    CCursor::new(buf.chars().count()),
                )));
                edit_output.state.store(ui.ctx(), response.id);
            }
            Popup::open_id(ui.ctx(), popup_id);
        }

        let mut changed = false;
        Popup::menu(&response)
            .id(popup_id)
            .close_behavior(PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                // Only the rows in view are laid out (ADR-7306): a project of 50 000 assets
                // filters its paths here, then draws a screenful.
                let needle = buf.to_lowercase();
                let mut counter = 0usize;
                let shown: Vec<V> = it
                    .filter(|choice| {
                        counter += 1;
                        !filter_by_input
                            || needle.is_empty()
                            || choice.as_ref().to_lowercase().contains(&needle)
                    })
                    .collect();
                if shown.is_empty() {
                    if buf.is_empty() {
                        ui.label("No items found");
                    } else {
                        ui.label(format!("No items out of {counter} match the filter"));
                    }
                    return;
                }
                let row_height = ui.spacing().interact_size.y;
                egui::ScrollArea::vertical().show_rows(ui, row_height, shown.len(), |ui, range| {
                    for choice in &shown[range] {
                        let text = choice.as_ref();
                        if display(ui, text).clicked() {
                            *buf = text.to_owned();
                            changed = true;
                        }
                    }
                });
            });

        if changed {
            Popup::close_id(ui.ctx(), popup_id);
            response.mark_changed();
        }
        response
    }
}
