// SPDX-License-Identifier: MPL-2.0
//! Long lists that lay out only the rows in view (ADR-7306): a thin wrapper over
//! `egui::ScrollArea::show_rows` for panels and drop-down lists whose length follows the project
//! (clip names, pictures, ability assets, live agents).

/// Draws `count` one-line rows in a vertical scroll area at most `max_height` high; `add_row` is
/// called only for the rows in view, with each row's index. Rows should be one widget high
/// (`interact_size.y`); taller rows scroll a little unevenly but are never lost.
pub fn rows(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    count: usize,
    max_height: f32,
    mut add_row: impl FnMut(&mut egui::Ui, usize),
) {
    let row_height = ui.spacing().interact_size.y;
    egui::ScrollArea::vertical()
        .id_salt(egui::Id::new(id_salt))
        .max_height(max_height)
        .show_rows(ui, row_height, count, |ui, range| {
            for index in range {
                add_row(ui, index);
            }
        });
}

/// The height drop-down lists use for their rows ([`rows`]'s `max_height`).
pub const DROP_DOWN_HEIGHT: f32 = 320.0;

/// Row heights of a [`variable_rows`] list, kept between frames (ADR-7326): each row's estimate,
/// its measured height (spacing included) and the prefix sums of the heights.
#[derive(Clone, Debug, Default)]
struct RowHeights {
    /// The estimate each height was set from.
    estimates: Vec<f32>,
    /// Measured (or, until a row is drawn, estimated) height of each row, spacing included.
    heights: Vec<f32>,
    /// `offsets[i]`: the top of row `i`; `offsets[count]` is the total height.
    offsets: Vec<f32>,
    /// The offsets need summing again.
    dirty: bool,
}

impl RowHeights {
    /// Fits the cache to `count` rows: a row whose estimate changed (or a new row) starts from
    /// its estimate again; a row whose estimate is unchanged keeps its measured height.
    fn fit(&mut self, count: usize, spacing: f32, estimate: impl Fn(usize) -> f32) {
        if self.heights.len() != count {
            self.estimates.resize(count, f32::NAN);
            self.heights.resize(count, 0.0);
            self.dirty = true;
        }
        for (index, (known, height)) in self.estimates.iter_mut().zip(&mut self.heights).enumerate() {
            let guess = estimate(index).max(0.0);
            if guess.to_bits() != known.to_bits() {
                *known = guess;
                *height = guess + spacing;
                self.dirty = true;
            }
        }
        if self.dirty || self.offsets.len() != count + 1 {
            self.offsets.clear();
            self.offsets.reserve(count + 1);
            let mut top = 0.0;
            self.offsets.push(top);
            for height in &self.heights {
                top += height;
                self.offsets.push(top);
            }
            self.dirty = false;
        }
    }

    /// The row under the content height `y` (the last row past the end).
    fn row_at(&self, y: f32) -> usize {
        let count = self.heights.len();
        self.offsets
            .partition_point(|top| *top <= y)
            .saturating_sub(1)
            .min(count.saturating_sub(1))
    }

    /// Records the measured height of `row`.
    fn measured(&mut self, row: usize, height: f32) {
        if let Some(known) = self.heights.get_mut(row)
            && (*known - height).abs() > 0.5
        {
            *known = height;
            self.dirty = true;
        }
    }
}

/// Like [`rows`], for rows of different heights (ADR-7326): `estimate(i)` is row `i`'s height
/// before it is drawn (spacing excluded; called for every row each frame, so keep it cheap), the
/// height a drawn row really takes is kept until its estimate changes, and the scroll area lays
/// out only the rows in view, placed by prefix sums of the kept heights. Each row is drawn in its
/// own scope (top-down), so a row may hold several lines.
pub fn variable_rows(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    count: usize,
    max_height: f32,
    estimate: impl Fn(usize) -> f32,
    mut add_row: impl FnMut(&mut egui::Ui, usize),
) {
    let id = egui::Id::new(id_salt);
    let memory = id.with("row-heights");
    let spacing = ui.spacing().item_spacing.y;
    let mut heights: RowHeights =
        ui.data_mut(|data| std::mem::take(data.get_temp_mut_or_default::<RowHeights>(memory)));
    heights.fit(count, spacing, estimate);
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(max_height)
        .show_viewport(ui, |ui, viewport| {
            let total = heights.offsets.last().copied().unwrap_or(0.0);
            ui.set_height((total - spacing).max(0.0));
            if count == 0 {
                return;
            }
            let origin = ui.max_rect().min;
            let width = ui.max_rect().width();
            let mut row = heights.row_at(viewport.min.y);
            let mut top = heights.offsets[row];
            while row < count && top < viewport.max.y {
                let rect = egui::Rect::from_min_size(
                    origin + egui::vec2(0.0, top),
                    egui::vec2(width, f32::INFINITY),
                );
                // A scope, not a detached child: the content (and so the scroll area) grows to
                // the rows' width, as with `show_rows`.
                let drawn = ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt(("row", row))
                        .max_rect(rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |row_ui| add_row(row_ui, row),
                );
                let height = drawn.response.rect.height() + spacing;
                heights.measured(row, height);
                // Rows below follow what was drawn, not the old estimate, so none overlap.
                top += height;
                row += 1;
            }
        });
    ui.data_mut(|data| data.insert_temp(memory, heights));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_rows_in_view_are_drawn() {
        let ctx = egui::Context::default();
        let mut drawn = Vec::new();
        for _ in 0..2 {
            drawn.clear();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    rows(ui, "virtual-list-test", 50_000, DROP_DOWN_HEIGHT, |ui, i| {
                        drawn.push(i);
                        ui.label(format!("row {i}"));
                    });
                });
            });
            output.textures_delta.clear();
        }
        assert_eq!(drawn.first(), Some(&0));
        assert!(drawn.len() < 40, "a screenful of 50 000: {}", drawn.len());
    }

    /// Rows of one, two or three lines: a screenful is laid out, the rows do not overlap, and
    /// the heights the rows really took replace the estimates.
    #[test]
    fn variable_rows_lay_out_a_screenful_without_overlap() {
        let ctx = egui::Context::default();
        let mut drawn: Vec<(usize, egui::Rect)> = Vec::new();
        for _ in 0..3 {
            drawn.clear();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    // Every row is estimated one line high; the real rows are 1-3 lines.
                    variable_rows(
                        ui,
                        "variable-list-test",
                        20_000,
                        400.0,
                        |_| 18.0,
                        |ui, i| {
                            let response = ui.label(format!("row {i}"));
                            let mut rect = response.rect;
                            for line in 0..i % 3 {
                                rect = rect.union(ui.label(format!("line {line}")).rect);
                            }
                            drawn.push((i, rect));
                        },
                    );
                });
            });
            output.textures_delta.clear();
        }
        assert_eq!(drawn.first().map(|(i, _)| *i), Some(0));
        assert!(drawn.len() < 40, "a screenful of 20 000: {}", drawn.len());
        for pair in drawn.windows(2) {
            assert!(
                pair[1].1.top() >= pair[0].1.bottom() - 0.5,
                "row {} overlaps row {}",
                pair[1].0,
                pair[0].0
            );
        }
        let heights = ctx
            .data_mut(|d| d.get_temp::<RowHeights>(egui::Id::new("variable-list-test").with("row-heights")));
        let heights = heights.expect("kept");
        assert!(
            heights.heights[2] > heights.heights[0] + 10.0,
            "a three-line row is measured taller: {:?}",
            &heights.heights[..3]
        );
        assert_eq!(
            heights.heights[1_000],
            heights.estimates[1_000] + ctx.global_style().spacing.item_spacing.y
        );
    }

    #[test]
    fn row_heights_find_rows_by_prefix_sums_and_reset_changed_estimates() {
        let mut heights = RowHeights::default();
        heights.fit(4, 2.0, |i| [10.0, 20.0, 30.0, 40.0][i]);
        assert_eq!(heights.offsets, [0.0, 12.0, 34.0, 66.0, 108.0]);
        assert_eq!(heights.row_at(0.0), 0);
        assert_eq!(heights.row_at(11.9), 0);
        assert_eq!(heights.row_at(12.0), 1);
        assert_eq!(heights.row_at(500.0), 3);
        heights.measured(1, 50.0);
        heights.fit(4, 2.0, |i| [10.0, 20.0, 30.0, 40.0][i]);
        assert_eq!(heights.offsets[2], 62.0, "a measured height is kept");
        heights.fit(4, 2.0, |i| [10.0, 25.0, 30.0, 40.0][i]);
        assert_eq!(heights.offsets[2], 39.0, "a new estimate replaces it");
        heights.fit(2, 2.0, |i| [10.0, 25.0][i]);
        assert_eq!(heights.offsets, [0.0, 12.0, 39.0]);
    }
}
