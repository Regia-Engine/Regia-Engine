// SPDX-License-Identifier: MPL-2.0
//! Error and warning badges on a graph node (P-186): a colored glyph whose color comes from the
//! theme tokens and whose accessible name (and hover text) is the message, so a screen reader
//! speaks what is wrong instead of "warning sign" (FU-7, ADR-8870).

/// How bad a badge's message is: picks the theme token it is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// An error (`ThemeTokens::error`).
    Error,
    /// A warning (`ThemeTokens::warn`).
    Warning,
}

impl Severity {
    /// This severity's color under `tokens`.
    #[must_use]
    pub fn color(self, tokens: &crate::theme::ThemeTokens) -> egui::Color32 {
        match self {
            Self::Error => tokens.error,
            Self::Warning => tokens.warn,
        }
    }
}

/// The "⚠" badge of one message, spoken as `message` (see `a11y_audit::warning_glyph`).
pub fn badge(ui: &mut egui::Ui, severity: Severity, message: &str) -> egui::Response {
    let color = severity.color(&crate::theme::ui_tokens(ui.ctx()));
    crate::a11y_audit::warning_glyph(ui, color, message)
}

/// The text of a count badge: the glyph and the count ("⚠3").
#[must_use]
pub fn count_text(count: usize) -> String {
    format!("\u{26a0}{count}")
}

/// A count badge ("⚠3": that many warnings or errors), spoken as `message` (which should name the
/// count and what it counts); draws nothing for a count of 0.
pub fn count_badge(
    ui: &mut egui::Ui,
    severity: Severity,
    count: usize,
    message: &str,
) -> Option<egui::Response> {
    if count == 0 {
        return None;
    }
    let color = severity.color(&crate::theme::ui_tokens(ui.ctx()));
    let response = ui.colored_label(color, count_text(count));
    Some(crate::a11y_audit::name(
        response,
        egui::WidgetType::Label,
        message,
    ))
}
