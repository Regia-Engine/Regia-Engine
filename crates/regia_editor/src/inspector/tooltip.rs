// SPDX-License-Identifier: MPL-2.0
//! Field tooltips (addendum § 7.4, P-032): every Inspector field row explains itself with its doc
//! text (`@Tooltip`, else the field's doc comment), its default value and the agent path that
//! writes it.
//!
//! The minimal builder for the Inspector's field rows. A shared builder for commands and other
//! panels belongs to the keymap/help work (3206); this one only formats, so swapping it for the
//! shared builder later is a call-site change.

/// What a field tooltip shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldTooltip {
    /// The help text: `@Tooltip`, else the field's doc comment (first paragraph).
    pub doc: Option<String>,
    /// The default (or the prefab's) value, as compact RON.
    pub default: Option<String>,
    /// Where the default came from: `"default"` or `"prefab"`.
    pub default_source: &'static str,
    /// The agent call that writes the field (`editor.set_property {component, path}`).
    pub agent_path: String,
    /// Why the field cannot be edited right now (an `@EditCondition` that is false).
    pub condition: Option<String>,
}

/// Longest default value shown before it is cut with an ellipsis.
pub const MAX_DEFAULT_CHARS: usize = 80;

/// The first paragraph of a doc comment, trimmed (`None` for an empty one).
#[must_use]
pub fn first_paragraph(doc: &str) -> Option<String> {
    let paragraph: Vec<&str> = doc
        .trim()
        .lines()
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect();
    let joined = paragraph.join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// `text` cut to [`MAX_DEFAULT_CHARS`] characters, with an ellipsis when cut.
#[must_use]
pub fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_DEFAULT_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(MAX_DEFAULT_CHARS).collect();
    out.push('\u{2026}');
    out
}

/// The agent path text for a field: `editor.set_property {"component": …, "path": …}`.
#[must_use]
pub fn agent_path(component: &str, path: &str) -> String {
    format!(
        "editor.set_property {}",
        serde_json::json!({ "component": component, "path": path })
    )
}

impl FieldTooltip {
    /// The tooltip's lines, in order (doc, default, condition, agent path).
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(doc) = &self.doc {
            lines.push(doc.clone());
        }
        if let Some(default) = &self.default {
            let label = if self.default_source == "prefab" {
                "Prefab value"
            } else {
                "Default"
            };
            lines.push(format!("{label}: {}", clip(default)));
        }
        if let Some(condition) = &self.condition {
            lines.push(format!("Editable when: {condition}"));
        }
        lines.push(format!("Agent: {}", self.agent_path));
        lines
    }

    /// Draws the tooltip (inside `on_hover_ui`).
    pub fn ui(&self, ui: &mut egui::Ui) {
        let lines = self.lines();
        for (i, line) in lines.iter().enumerate() {
            if i + 1 == lines.len() {
                ui.weak(line);
            } else {
                ui.label(line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_lines_cover_doc_default_and_agent_path() {
        let tip = FieldTooltip {
            doc: first_paragraph("  Outer cone half-angle in radians.\n\nMore detail.\n"),
            default: Some("0.5".into()),
            default_source: "default",
            agent_path: agent_path("SpotLight", "outer_angle"),
            condition: None,
        };
        assert_eq!(
            tip.lines(),
            vec![
                "Outer cone half-angle in radians.".to_owned(),
                "Default: 0.5".to_owned(),
                "Agent: editor.set_property {\"component\":\"SpotLight\",\"path\":\"outer_angle\"}"
                    .to_owned(),
            ]
        );
        assert_eq!(first_paragraph("\n \n"), None);
        let long = "x".repeat(200);
        assert_eq!(clip(&long).chars().count(), MAX_DEFAULT_CHARS + 1);
    }
}
