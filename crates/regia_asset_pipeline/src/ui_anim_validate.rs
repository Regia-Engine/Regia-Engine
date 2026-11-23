// SPDX-License-Identifier: MPL-2.0
//! The check of a `.uianim.ron` library against the nodes of a `.ui.ron` document (spec 32, ADR-3092).
//!
//! The registry validators of spec 32 judge asset metadata and never read source (§ 4.6), but this check
//! needs the library's tracks and the document's nodes, so it is a plain function over both. The designer
//! panel, `editor.ui_anim_validate` and `regia assets validate` callers run it; each finding carries a
//! stable code and the same [`Finding`] shape the registry validators return.
//!
//! A track has no node field (the format is frozen at version 1), so the binding is the animation's name:
//! `"<node>::<label>"` animates the node named `<node>`; a name without `::` is generic and plays on any
//! node, so it is not checked against the document.
//!
//! | Code | Severity | Meaning |
//! |---|---|---|
//! | `ui_anim_missing_node` | error | the animation's target node is not in the document |
//! | `ui_anim_ambiguous_node` | warning | two or more nodes carry the target name; the first one is previewed |
//! | `ui_anim_property_unsupported` | error | a track animates a property the target node cannot show (`text_color` on a node without text) |
//! | `ui_anim_invalid` | error | the animation fails the format's own validation |

use regia_ui::anim_timeline::UiAnimLibrary;
use regia_ui::tween::TweenProperty;

use crate::validators::{Finding, Severity};

/// Separator between the target node name and the label in an animation name.
pub const TARGET_SEPARATOR: &str = "::";

/// What the check needs to know of one node of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiNodeFact {
    /// The node's `name`.
    pub name: String,
    /// The node shows text (it has a `text` block).
    pub has_text: bool,
}

/// The target node name of animation `name` (`"hud::pop"` is `"hud"`), `None` for a generic animation.
#[must_use]
pub fn target_of(name: &str) -> Option<&str> {
    name.split_once(TARGET_SEPARATOR)
        .map(|(node, _)| node)
        .filter(|node| !node.is_empty())
}

/// Findings for `lib` against `nodes`, ordered by animation name then track.
#[must_use]
pub fn validate_ui_anim(lib: &UiAnimLibrary, nodes: &[UiNodeFact]) -> Vec<Finding> {
    let _span = tracing::debug_span!(target: "regia_asset_pipeline", "ui_anim_validate").entered();
    let mut out = Vec::new();
    for (name, def) in &lib.animations {
        if let Err(e) = def.validate() {
            out.push(
                Finding::error("ui_anim_invalid", format!("animation `{name}`: {e}"))
                    .with_hint("fix the track or key the message names"),
            );
        }
        let Some(target) = target_of(name) else {
            continue;
        };
        let matches: Vec<&UiNodeFact> = nodes.iter().filter(|n| n.name == target).collect();
        let Some(node) = matches.first() else {
            out.push(
                Finding::error(
                    "ui_anim_missing_node",
                    format!("animation `{name}` targets node `{target}`, which the document does not have"),
                )
                .with_hint("rename the animation to name an existing node, or add the node"),
            );
            continue;
        };
        if matches.len() > 1 {
            out.push(Finding::new(
                "ui_anim_ambiguous_node",
                Severity::Warning,
                format!("animation `{name}`: {} nodes are named `{target}`", matches.len()),
            ));
        }
        for track in &def.tracks {
            if TweenProperty::parse(&track.property) == Some(TweenProperty::TextColor) && !node.has_text {
                out.push(
                    Finding::error(
                        "ui_anim_property_unsupported",
                        format!(
                            "animation `{name}`: node `{target}` has no text, so `text_color` cannot animate"
                        ),
                    )
                    .with_hint("animate `background` or `opacity`, or target a node with text"),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::anim_timeline::{AnimKey, AnimTrack, AnimValue, LoopMode, UiAnimDef};

    fn def(property: &str, value: AnimValue) -> UiAnimDef {
        UiAnimDef {
            tracks: vec![AnimTrack {
                property: property.into(),
                keys: vec![AnimKey {
                    time: 0.0,
                    value,
                    ease: None,
                }],
            }],
            mode: LoopMode::Once,
            loops: 1,
            speed: 1.0,
            events: Vec::new(),
        }
    }

    fn node(name: &str, has_text: bool) -> UiNodeFact {
        UiNodeFact {
            name: name.into(),
            has_text,
        }
    }

    #[test]
    fn target_is_the_text_before_the_separator() {
        assert_eq!(target_of("hud::pop"), Some("hud"));
        assert_eq!(target_of("pop"), None);
        assert_eq!(target_of("::pop"), None);
    }

    #[test]
    fn findings_carry_stable_codes() {
        let mut lib = UiAnimLibrary::default();
        lib.animations
            .insert("gone::pop".into(), def("opacity", AnimValue::Number(1.0)));
        lib.animations.insert(
            "panel::tint".into(),
            def("text_color", AnimValue::Color([1.0; 4])),
        );
        lib.animations.insert(
            "title::tint".into(),
            def("text_color", AnimValue::Color([1.0; 4])),
        );
        lib.animations
            .insert("generic".into(), def("opacity", AnimValue::Number(1.0)));
        let nodes = [node("panel", false), node("title", true), node("title", true)];
        let codes: Vec<(String, Severity)> = validate_ui_anim(&lib, &nodes)
            .into_iter()
            .map(|f| (f.code, f.severity))
            .collect();
        assert_eq!(
            codes,
            vec![
                ("ui_anim_missing_node".to_owned(), Severity::Error),
                ("ui_anim_property_unsupported".to_owned(), Severity::Error),
                ("ui_anim_ambiguous_node".to_owned(), Severity::Warning),
            ]
        );
    }

    #[test]
    fn a_clean_library_has_no_findings() {
        let mut lib = UiAnimLibrary::default();
        lib.animations
            .insert("title::pop".into(), def("opacity", AnimValue::Number(1.0)));
        assert!(validate_ui_anim(&lib, &[node("title", true)]).is_empty());
    }
}
