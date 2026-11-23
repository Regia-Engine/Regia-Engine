// SPDX-License-Identifier: MPL-2.0
//! Inspector property search and the **Modified only** filter (spec addendum § 7.1, P-110, P-116).
//!
//! The search box at the top of the Inspector (Ctrl+F while the pointer is over the Inspector,
//! Esc clears) filters field rows by a fuzzy match ([`regia_ui::fuzzy::fuzzy_match`], the command
//! palette's matcher) on the field's display name, its reflect path, or the component name followed
//! by the field name (`lgtclr` finds a light's `color`: `l`, `g`, `t` in the component, `c`, `l`,
//! `r` in the field; at least one query letter has to land in the field). Matching letters of the
//! name are highlighted with the theme's selection accent.
//!
//! [`RowFilter`] is what the reflection drawer asks per row ([`RowFilter::row_visible`]): a row is
//! shown when it is a hit, an ancestor of a hit (so a struct holding a matching field stays) or a
//! descendant of a hit (a matching struct shows all its fields). The search and the
//! modified-only sets are combined: a row has to pass both.

use std::collections::{BTreeSet, HashMap};

use regia_ecs::attrs::EditorAttrs;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{PartialReflect, ReflectRef, TypeInfo};
use regia_ui::fuzzy::fuzzy_match;

/// How deep the search walks into nested structs.
const MAX_DEPTH: usize = 8;

/// The Inspector's filter state (view state: never on the undo stack, never dirties a document).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorSearch {
    /// The search box's text.
    pub query: String,
    /// Hide fields equal to their baseline ([`super::defaults::baseline`]).
    pub modified_only: bool,
    /// The component filter's text (over the component list, entities with many components).
    pub component_filter: String,
}

impl InspectorSearch {
    /// Whether the search box holds a query (after trimming).
    #[must_use]
    pub fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Whether any field filter is on.
    #[must_use]
    pub fn filtering(&self) -> bool {
        self.searching() || self.modified_only
    }
}

/// The highlight positions (characters of `label`) when field `label` at reflect `path` of a
/// component called `component` matches `query`, or `None` when it does not. An empty query
/// matches with no highlight. See the module docs for the three candidates tried.
#[must_use]
pub fn field_match(component: &str, label: &str, path: &str, query: &str) -> Option<Vec<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return Some(Vec::new());
    }
    if let Some((_, positions)) = fuzzy_match(query, label) {
        return Some(positions);
    }
    if fuzzy_match(query, path).is_some() {
        return Some(Vec::new());
    }
    let offset = component.chars().count() + 1;
    let joined = format!("{component} {label}");
    let (_, positions) = fuzzy_match(query, &joined)?;
    let in_label: Vec<usize> = positions
        .iter()
        .filter(|at| **at >= offset)
        .map(|at| at - offset)
        .collect();
    (!in_label.is_empty()).then_some(in_label)
}

/// Reflect paths that pass a filter: a path is allowed when it is in the set, an ancestor of one
/// (`light` for `light.color`), or a descendant of one (`light.color` for `light`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathSet {
    hits: BTreeSet<String>,
}

impl PathSet {
    /// A set of hit paths.
    #[must_use]
    pub fn new(hits: BTreeSet<String>) -> Self {
        Self { hits }
    }

    /// Whether no path passes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Whether `path` passes (see the type docs).
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        if self.hits.contains(path) || self.hits.contains("") {
            return true;
        }
        self.hits
            .iter()
            .any(|hit| is_under(hit, path) || is_under(path, hit))
    }
}

/// Whether `child` is strictly below `parent` in reflect-path terms (`a.b` and `a[0]` under `a`).
fn is_under(child: &str, parent: &str) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && matches!(child.as_bytes().get(parent.len()), Some(b'.' | b'['))
}

/// The filter one component's drawer applies to its rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowFilter {
    /// Rows matching the search, when searching.
    pub search: Option<PathSet>,
    /// Rows differing from the baseline, when **Modified only** is on.
    pub modified: Option<PathSet>,
    /// Highlight positions of a matching row's label, by reflect path.
    pub highlights: HashMap<String, Vec<usize>>,
}

impl RowFilter {
    /// Whether the row at reflect `path` is drawn.
    #[must_use]
    pub fn row_visible(&self, path: &str) -> bool {
        self.search.as_ref().is_none_or(|set| set.allows(path))
            && self.modified.as_ref().is_none_or(|set| set.allows(path))
    }

    /// The label characters to highlight on the row at `path`.
    #[must_use]
    pub fn highlight(&self, path: &str) -> Option<&[usize]> {
        self.highlights.get(path).map(Vec::as_slice)
    }

    /// Whether the component has any row to show: both active sets are non-empty and some hit of
    /// one is allowed by the other.
    #[must_use]
    pub fn has_rows(&self) -> bool {
        match (&self.search, &self.modified) {
            (None, None) => true,
            (Some(set), None) | (None, Some(set)) => !set.is_empty(),
            (Some(search), Some(modified)) => {
                search.hits.iter().any(|hit| modified.allows(hit))
                    || modified.hits.iter().any(|hit| search.allows(hit))
            }
        }
    }
}

/// The search hits of a component value: every struct field (at any depth up to the walk's
/// bound) whose display name, path or component-qualified name matches `query`, with the label
/// highlight of each.
#[must_use]
pub fn search_hits(
    component: &str,
    value: &dyn PartialReflect,
    query: &str,
) -> (BTreeSet<String>, HashMap<String, Vec<usize>>) {
    let mut hits = BTreeSet::new();
    let mut highlights = HashMap::new();
    walk(component, value, "", 0, query, &mut hits, &mut highlights);
    (hits, highlights)
}

fn walk(
    component: &str,
    value: &dyn PartialReflect,
    path: &str,
    depth: usize,
    query: &str,
    hits: &mut BTreeSet<String>,
    highlights: &mut HashMap<String, Vec<usize>>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let ReflectRef::Struct(value) = value.reflect_ref() else {
        return;
    };
    let info = match value.get_represented_type_info() {
        Some(TypeInfo::Struct(info)) => Some(info),
        _ => None,
    };
    for index in 0..value.field_len() {
        let (Some(name), Some(field)) = (value.name_at(index), value.field_at(index)) else {
            continue;
        };
        let attrs = info
            .and_then(|info| info.field(name))
            .map(|field| EditorAttrs::from_custom(field.custom_attributes()))
            .unwrap_or_default();
        if attrs.hidden {
            continue;
        }
        let label = attrs.display_name.unwrap_or(name);
        let child = if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}.{name}")
        };
        if let Some(positions) = field_match(component, label, &child, query) {
            hits.insert(child.clone());
            highlights.insert(child.clone(), positions);
        }
        walk(component, field, &child, depth + 1, query, hits, highlights);
    }
}

/// The [`RowFilter`] for `entity`'s component (type path `component`, value `value`) under
/// `search`; `None` when no filter is on.
#[must_use]
#[tracing::instrument(target = "regia_editor", skip_all, name = "inspector_row_filter")]
pub fn row_filter(
    world: &World,
    entity: regia_ecs::ecs::entity::Entity,
    component: &str,
    value: &dyn PartialReflect,
    search: &InspectorSearch,
) -> Option<RowFilter> {
    if !search.filtering() {
        return None;
    }
    let short = component.rsplit("::").next().unwrap_or(component);
    let mut filter = RowFilter::default();
    if search.searching() {
        let (hits, highlights) = search_hits(short, value, &search.query);
        filter.search = Some(PathSet::new(hits));
        filter.highlights = highlights;
    }
    if search.modified_only {
        // No baseline: every field counts as changed (`defaults::differs`), so the component shows.
        let modified = super::defaults::baseline(world, entity, component).map_or_else(
            || super::defaults::all_leaf_paths(value),
            |(_, base)| super::defaults::modified_paths(value, base.as_ref()),
        );
        filter.modified = Some(PathSet::new(modified));
    }
    Some(filter)
}

/// `text` with the characters at `positions` drawn in the theme's selection accent and strong
/// (the search highlight). Plain text when `positions` is empty.
#[must_use]
pub fn highlighted(ui: &egui::Ui, text: &str, positions: &[usize]) -> egui::WidgetText {
    if positions.is_empty() {
        return text.into();
    }
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let style = ui.style();
    let plain = egui::TextFormat {
        font_id: egui::TextStyle::Body.resolve(style),
        color: style.visuals.text_color(),
        ..Default::default()
    };
    let lit = egui::TextFormat {
        color: tokens.accents.select,
        underline: egui::Stroke::new(1.0, tokens.accents.select),
        ..plain.clone()
    };
    let mut job = egui::text::LayoutJob::default();
    for (index, ch) in text.chars().enumerate() {
        let mut buf = [0u8; 4];
        let format = if positions.contains(&index) {
            lit.clone()
        } else {
            plain.clone()
        };
        job.append(ch.encode_utf8(&mut buf), 0.0, format);
    }
    job.into()
}

/// Filters one `editor.inspect_selection` component's `fields` (each `{ path, ... }`) by the
/// search `query` and the modified-only set, in place. Returns whether any field is left.
pub fn filter_inspected_fields(
    component: &str,
    fields: &mut Vec<serde_json::Value>,
    query: Option<&str>,
    modified: Option<&BTreeSet<String>>,
) -> bool {
    let query = query.map(str::trim).filter(|q| !q.is_empty());
    let modified = modified.map(|set| PathSet::new(set.clone()));
    fields.retain(|field| {
        let path = field
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let path = path.trim_start_matches('.');
        let searched = query.is_none_or(|query| {
            // A field is kept when it or one of its ancestors matches.
            let mut at = Some(path);
            while let Some(current) = at {
                let name = current.rsplit('.').next().unwrap_or(current);
                if field_match(component, name, current, query).is_some() {
                    return true;
                }
                at = current.rsplit_once('.').map(|(parent, _)| parent);
            }
            false
        });
        searched && modified.as_ref().is_none_or(|set| set.allows(path))
    });
    !fields.is_empty()
}

/// `editor.inspect_selection`'s `search` and `modified_only` parameters applied to its answer:
/// each component keeps the fields matching `search` (as the Inspector's search box) and, with
/// `modified_only`, differing from the baseline on at least one holder; a component left with no
/// field is dropped. Without either parameter the answer is unchanged.
///
/// # Errors
/// `invalid_params` when `search` is not a string or `modified_only` not a boolean.
pub fn filter_inspection(
    world: &World,
    mut inspected: serde_json::Value,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let query = match params.get("search") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => Some(text.trim().to_owned()).filter(|t| !t.is_empty()),
        Some(_) => return Err(RpcError::invalid_params("`search` must be a string")),
    };
    let modified_only = match params.get("modified_only") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(on)) => *on,
        Some(_) => return Err(RpcError::invalid_params("`modified_only` must be a boolean")),
    };
    if query.is_none() && !modified_only {
        return Ok(inspected);
    }
    let entities: Vec<regia_ecs::ecs::entity::Entity> = inspected
        .get("entities")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_u64)
                .filter_map(|bits| crate::command::resolve_entity(world, bits))
                .collect()
        })
        .unwrap_or_default();
    if let Some(components) = inspected
        .get_mut("components")
        .and_then(serde_json::Value::as_array_mut)
    {
        components.retain_mut(|component| {
            let short = component
                .get("component")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            let type_path = component
                .get("type_path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            let modified = modified_only.then(|| {
                let mut set = BTreeSet::new();
                for entity in &entities {
                    if let Some(paths) = super::defaults::modified_fields(world, *entity, &type_path) {
                        set.extend(paths);
                    }
                }
                set
            });
            let Some(fields) = component
                .get_mut("fields")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return false;
            };
            filter_inspected_fields(&short, fields, query.as_deref(), modified.as_ref())
        });
    }
    if let Some(object) = inspected.as_object_mut() {
        object.insert(
            "filter".into(),
            serde_json::json!({ "search": query, "modified_only": modified_only }),
        );
    }
    Ok(inspected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_of_the_name_matches_and_is_highlighted() {
        let positions = field_match("PointLight", "intensity", "intensity", "int").expect("match");
        assert_eq!(positions, [0, 1, 2]);
        assert!(
            field_match("PointLight", "color", "color", "int").is_none(),
            "letters only in the component name do not match a field"
        );
    }

    #[test]
    fn a_non_contiguous_query_spans_the_component_and_the_field() {
        let positions = field_match("PointLight", "color", "color", "lgtclr").expect("match");
        assert_eq!(positions, [0, 2, 4], "c, l, r of color");
        assert!(field_match("PointLight", "radius", "radius", "lgtclr").is_none());
    }

    #[test]
    fn path_sets_allow_ancestors_and_descendants() {
        let set = PathSet::new(["inner.b".to_string(), "list".to_string()].into());
        assert!(set.allows("inner"));
        assert!(set.allows("inner.b"));
        assert!(!set.allows("inner.a"));
        assert!(set.allows("list[3]"));
        assert!(!set.allows("in"));
        assert!(!set.allows("other"));
    }

    #[test]
    fn a_row_must_pass_both_filters() {
        let filter = RowFilter {
            search: Some(PathSet::new(["translation".to_string()].into())),
            modified: Some(PathSet::new(
                ["translation.x".to_string(), "scale.y".to_string()].into(),
            )),
            highlights: HashMap::new(),
        };
        assert!(filter.row_visible("translation"));
        assert!(filter.row_visible("translation.x"));
        assert!(!filter.row_visible("translation.y"));
        assert!(!filter.row_visible("scale"));
        assert!(filter.has_rows());
        let none = RowFilter {
            search: Some(PathSet::new(["rotation".to_string()].into())),
            modified: Some(PathSet::new(["scale.y".to_string()].into())),
            highlights: HashMap::new(),
        };
        assert!(!none.has_rows());
    }

    #[test]
    fn inspected_fields_filter_by_query_and_modified_set() {
        let mut fields = vec![
            serde_json::json!({"path": "color"}),
            serde_json::json!({"path": "intensity"}),
            serde_json::json!({"path": "radius"}),
        ];
        assert!(filter_inspected_fields(
            "PointLight",
            &mut fields,
            Some("int"),
            None
        ));
        assert_eq!(fields, [serde_json::json!({"path": "intensity"})]);
        let mut fields = vec![
            serde_json::json!({"path": "color"}),
            serde_json::json!({"path": "radius"}),
        ];
        let modified: BTreeSet<String> = ["radius".to_string()].into();
        assert!(filter_inspected_fields(
            "PointLight",
            &mut fields,
            None,
            Some(&modified)
        ));
        assert_eq!(fields, [serde_json::json!({"path": "radius"})]);
    }
}
