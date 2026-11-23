// SPDX-License-Identifier: MPL-2.0
//! The fuzzy node search every graph canvas shares (spec 80 § 12, ADR-9700; P-186): the
//! scorer, ranking with a dragged-pin filter, the search list a canvas menu shows (a field and
//! the best hits; Enter in the field takes the first) and the floating popup a canvas opens on
//! Space or Tab.

/// A fuzzy match score of `query` against `candidate` (case-insensitive subsequence; higher is
/// better): consecutive letters, a prefix and a word start (a capital) score extra, gaps cost.
/// `None` when `query` is not a subsequence. An empty query matches everything with 0.
#[must_use]
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = candidate.chars().collect();
    let mut score = 0i32;
    let mut at = 0usize;
    let mut last: Option<usize> = None;
    for q in &query {
        let q = q.to_ascii_lowercase();
        let found = (at..chars.len()).find(|&i| chars[i].to_ascii_lowercase() == q)?;
        score += 10;
        if found == 0 {
            score += 15;
        }
        if chars[found].is_ascii_uppercase() {
            score += 5;
        }
        match last {
            Some(prev) if found == prev + 1 => score += 8,
            Some(prev) => score -= (found - prev - 1).min(8) as i32,
            None => score -= found.min(8) as i32,
        }
        last = Some(found);
        at = found + 1;
    }
    if query.len() == chars.len() {
        score += 20;
    }
    // Shorter names win ties.
    Some(score * 4 - chars.len() as i32)
}

/// The `items` matching `query`, best first, ties in their given order. `accepts` is the
/// dragged-pin filter (an item that cannot take the wire is left out); `texts` lists the strings
/// an item matches on (its best one counts: a node's label, or "group label").
#[must_use]
pub fn rank<T>(
    items: impl IntoIterator<Item = T>,
    query: &str,
    accepts: impl Fn(&T) -> bool,
    texts: impl Fn(&T) -> Vec<String>,
) -> Vec<T> {
    let mut hits: Vec<(i32, usize, T)> = items
        .into_iter()
        .enumerate()
        .filter(|(_, item)| accepts(item))
        .filter_map(|(i, item)| {
            let score = texts(&item)
                .iter()
                .filter_map(|text| fuzzy_score(query, text))
                .max()?;
            Some((score, i, item))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, _, item)| item).collect()
}

/// The search list of a canvas menu or popup: a named field and the best hits as buttons.
#[derive(Clone, Copy, Debug)]
pub struct SearchList<'a> {
    /// The field's hint text.
    pub hint: &'a str,
    /// The field's accessible name (and hover text).
    pub spoken: &'a str,
    /// The most hits shown.
    pub max_hits: usize,
    /// The hit list's scroll height (points, before `ui_scale`).
    pub max_height: f32,
    /// The text shown when nothing matches (`None`: an empty list).
    pub empty: Option<&'a str>,
    /// The field's width (`None`: egui's default).
    pub width: Option<f32>,
}

impl SearchList<'_> {
    /// Draws the field and the hits of `query` (`hits` ranks them, `label` names a hit). The
    /// field takes the keyboard focus on the frame the list opens (not after, so Tab leaves it
    /// to the hits); Enter in the field picks the first hit, a click (or Space/Enter on a
    /// focused hit) picks that one. Returns the picked hit.
    pub fn show<T>(
        &self,
        ui: &mut egui::Ui,
        query: &mut String,
        hits: impl FnOnce(&str) -> Vec<T>,
        label: impl Fn(&T) -> String,
    ) -> Option<T> {
        let mut edit = egui::TextEdit::singleline(query).hint_text(self.hint);
        if let Some(width) = self.width {
            edit = edit.desired_width(width);
        }
        let field = ui.add(edit);
        let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, self.spoken);
        if opened_this_frame(ui.ctx(), field.id) {
            field.request_focus();
        }
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let mut hits = hits(query.as_str());
        hits.truncate(self.max_hits);
        let mut picked = None;
        egui::ScrollArea::vertical()
            .max_height(self.max_height)
            .show(ui, |ui| {
                for (index, hit) in hits.iter().enumerate() {
                    if ui.button(label(hit)).clicked() || (index == 0 && enter) {
                        picked = Some(index);
                        break;
                    }
                }
                if hits.is_empty()
                    && let Some(empty) = self.empty
                {
                    ui.weak(empty);
                }
            });
        let picked = picked.map(|index| hits.swap_remove(index));
        if picked.is_some() {
            query.clear();
        }
        picked
    }
}

/// Whether the widget `id` is shown this frame and was not shown the frame before (a menu or
/// popup that just opened). Both egui passes of one frame answer the same.
#[must_use]
pub fn opened_this_frame(ctx: &egui::Context, id: egui::Id) -> bool {
    let frame = ctx.cumulative_frame_nr();
    let key = id.with("graph-kit-shown");
    let (opened, record) = ctx.memory_mut(|m| {
        let last: Option<(u64, bool)> = m.data.get_temp(key);
        match last {
            // A second pass of the same frame keeps the first pass's answer.
            Some((at, opened)) if at == frame => (opened, false),
            Some((at, _)) if at + 1 == frame => (false, true),
            _ => (true, true),
        }
    });
    if record {
        ctx.memory_mut(|m| m.data.insert_temp(key, (frame, opened)));
    }
    opened
}

/// A floating search popup at `screen` (the canvas's Space or Tab search): a foreground area
/// with a popup frame, `id` its egui id. Returns `add`'s answer and whether Escape was pressed
/// (the caller closes the popup on it).
pub fn popup<R>(
    ctx: &egui::Context,
    id: egui::Id,
    screen: egui::Pos2,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, bool) {
    let shown = egui::Area::new(id)
        .fixed_pos(screen)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .show(ui, |ui| {
                    ui.set_min_width(200.0);
                    add(ui)
                })
                .inner
        });
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    (shown.inner, escape)
}
