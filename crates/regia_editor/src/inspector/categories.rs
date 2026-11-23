// SPDX-License-Identifier: MPL-2.0
//! Field categories and the add-component picker (spec addendum § 7.2, § 7.3; P-109, P-118).
//!
//! - Fields with `@Category("…")` draw under a collapsible sub-header inside their component, in
//!   the order the categories first appear; fields without one come first ([`group_fields`]).
//! - The add-component picker lists every reflected component type by category (a type-level
//!   `@Category`, else the type's crate: `regia_render::…` is **Render**), recent picks first,
//!   filtered by a fuzzy search. Types already on every selected entity, or without a reflected
//!   `Default` to add, stay listed but grayed with the reason ([`picker_entries`]).
//! - `editor.component_types { query?, entities? }` answers the same list.

use std::collections::BTreeSet;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::attrs::EditorAttrs;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::std_traits::ReflectDefault;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent, TypeInfo, TypeRegistration};
use regia_ui::fuzzy::fuzzy_match;
use serde_json::{Value, json};

/// Recent picks kept for the picker's "Recent" group.
pub const MAX_RECENT: usize = 8;

/// The egui id of the add-component picker's search box.
pub const PICKER_SEARCH_ID: &str = "inspector_add_component_search";

/// The reason a type already on every selected entity is grayed.
pub const REASON_PRESENT: &str = "already on this entity";
/// The reason a type without a reflected `Default` is grayed.
pub const REASON_NO_DEFAULT: &str = "has no default value to add";

/// Groups field indices by category: uncategorized fields first, then one group per category in
/// the order the categories first appear. Field order is kept inside a group.
#[must_use]
pub fn group_fields<'a>(categories: &[Option<&'a str>]) -> Vec<(Option<&'a str>, Vec<usize>)> {
    let mut groups: Vec<(Option<&'a str>, Vec<usize>)> = vec![(None, Vec::new())];
    for (index, category) in categories.iter().enumerate() {
        match groups.iter_mut().find(|(name, _)| name == category) {
            Some((_, indices)) => indices.push(index),
            None => groups.push((*category, vec![index])),
        }
    }
    if groups[0].1.is_empty() {
        groups.remove(0);
    }
    groups
}

/// Whether a struct's fields need category sub-headers (any field has a category).
#[must_use]
pub fn has_categories(categories: &[Option<&str>]) -> bool {
    categories.iter().any(Option::is_some)
}

/// The type-level `@Category` of a registered type (structs, tuple structs and enums).
fn type_category(registration: &TypeRegistration) -> Option<&'static str> {
    let attrs = match registration.type_info() {
        TypeInfo::Struct(info) => info.custom_attributes(),
        TypeInfo::TupleStruct(info) => info.custom_attributes(),
        TypeInfo::Enum(info) => info.custom_attributes(),
        _ => return None,
    };
    EditorAttrs::from_custom(attrs).category
}

/// The picker category of a component type: its type-level `@Category`, else its crate name
/// without the `regia_` prefix, words capitalized (`regia_render::lights::PointLight` is
/// `Render`, `my_game::door::Door` is `My game`).
#[must_use]
pub fn component_category(registration: &TypeRegistration) -> String {
    if let Some(category) = type_category(registration) {
        return category.to_owned();
    }
    module_category(registration.type_info().type_path())
}

/// [`component_category`]'s crate-name rule over a type path.
#[must_use]
pub fn module_category(type_path: &str) -> String {
    let krate = type_path.split("::").next().unwrap_or(type_path);
    let krate = krate.strip_prefix("regia_").unwrap_or(krate);
    let words = krate.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Other".to_owned(),
    }
}

/// One row of the add-component picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    /// The component's type path.
    pub type_path: &'static str,
    /// The short type name shown.
    pub short: String,
    /// Its category ([`component_category`]).
    pub category: String,
    /// Why it cannot be added, when it cannot ([`REASON_PRESENT`], [`REASON_NO_DEFAULT`]).
    pub unavailable: Option<&'static str>,
    /// Whether it is one of the recent picks.
    pub recent: bool,
}

/// The add-component picker's rows for an entity holding `present` (type paths on every
/// selected entity): every reflected component type matching `query` (fuzzy over the short name
/// and the type path), recent picks first in pick order, then by category, then (with a query)
/// best match first, then by name. Category before score keeps each category heading once
/// (review sprint-v06-rev-late-k #21: sorting by score first repeated the headings).
#[must_use]
#[tracing::instrument(target = "regia_editor", skip_all, name = "inspector_picker_entries")]
pub fn picker_entries(
    registry: &regia_ecs::reflect::TypeRegistry,
    present: &BTreeSet<String>,
    query: &str,
    recent: &[String],
) -> Vec<PickerEntry> {
    let query = query.trim();
    let mut rows: Vec<(usize, i32, PickerEntry)> = registry
        .iter()
        .filter(|registration| registration.data::<ReflectComponent>().is_some())
        .filter_map(|registration| {
            let type_path = registration.type_info().type_path();
            let short = registration.type_info().type_path_table().short_path().to_owned();
            let score = if query.is_empty() {
                0
            } else {
                fuzzy_match(query, &short)
                    .map(|(score, _)| score)
                    .or_else(|| fuzzy_match(query, type_path).map(|(score, _)| score / 2))?
            };
            let unavailable = if present.contains(type_path) {
                Some(REASON_PRESENT)
            } else if registration.data::<ReflectDefault>().is_none() {
                Some(REASON_NO_DEFAULT)
            } else {
                None
            };
            let rank = recent.iter().position(|r| r == type_path).unwrap_or(usize::MAX);
            Some((
                rank,
                score,
                PickerEntry {
                    type_path,
                    short,
                    category: component_category(registration),
                    unavailable,
                    recent: rank != usize::MAX,
                },
            ))
        })
        .collect();
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.2.category.cmp(&b.2.category))
            .then_with(|| {
                if query.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    b.1.cmp(&a.1)
                }
            })
            .then_with(|| a.2.short.cmp(&b.2.short))
            .then_with(|| a.2.type_path.cmp(b.2.type_path))
    });
    rows.into_iter().map(|(_, _, entry)| entry).collect()
}

/// The add-component picker's state (view state).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct AddComponentPicker {
    /// Whether the picker is open.
    pub open: bool,
    /// Its search text.
    pub query: String,
    /// Recent picks, newest first (at most [`MAX_RECENT`]).
    pub recent: Vec<String>,
    /// Set when the picker opened, so its search box takes keyboard focus once.
    pub focus: bool,
    /// The row Up/Down highlighted (a type path); `None`: the first row that can be added is the
    /// one Enter adds ([`enter_target`]). A new query clears it.
    pub highlight: Option<String>,
}

/// The row Enter adds in the picker (lead decision on review sprint-v06-rev-late-k #21): the
/// highlighted row when it is listed and can be added, else the first row that can be added.
#[must_use]
pub fn enter_target<'a>(entries: &'a [PickerEntry], highlight: Option<&str>) -> Option<&'a PickerEntry> {
    highlight
        .and_then(|path| {
            entries
                .iter()
                .find(|entry| entry.type_path == path && entry.unavailable.is_none())
        })
        .or_else(|| entries.iter().find(|entry| entry.unavailable.is_none()))
}

/// The highlight after one Down (`down`) or Up step from [`enter_target`]'s row, over the rows that
/// can be added; it stops at the first and the last.
#[must_use]
pub fn step_highlight(entries: &[PickerEntry], highlight: Option<&str>, down: bool) -> Option<&'static str> {
    let addable: Vec<&PickerEntry> = entries
        .iter()
        .filter(|entry| entry.unavailable.is_none())
        .collect();
    let current = enter_target(entries, highlight)?;
    let at = addable
        .iter()
        .position(|entry| entry.type_path == current.type_path)
        .unwrap_or(0);
    let next = if down {
        (at + 1).min(addable.len().saturating_sub(1))
    } else {
        at.saturating_sub(1)
    };
    addable.get(next).map(|entry| entry.type_path)
}

impl AddComponentPicker {
    /// Notes a pick for the "Recent" group.
    pub fn note_pick(&mut self, type_path: &str) {
        self.recent.retain(|r| r != type_path);
        self.recent.insert(0, type_path.to_owned());
        self.recent.truncate(MAX_RECENT);
    }
}

/// Type paths of the components on every one of `entities`.
fn present_on_all(world: &World, entities: &[Entity]) -> BTreeSet<String> {
    let mut out: Option<BTreeSet<String>> = None;
    for entity in entities {
        let Ok(infos) = world.inspect_entity(*entity) else {
            continue;
        };
        let here: BTreeSet<String> = infos.map(|(_, info)| info.name().to_string()).collect();
        out = Some(match out {
            Some(acc) => acc.intersection(&here).cloned().collect(),
            None => here,
        });
    }
    out.unwrap_or_default()
}

/// Draws the add-component picker for `entities` (one entity, or a multi-selection): an
/// **Add component…** button that opens a search box and the categorized list. Up/Down in the
/// search box move a highlight over the rows that can be added; Enter adds the highlighted row, or
/// the first row that can be added when none is highlighted ([`enter_target`]); Esc closes the
/// picker. Every add is undoable.
pub fn add_component_picker(ui: &mut egui::Ui, world: &mut World, entities: &[Entity]) {
    let mut state = world
        .get_resource_or_insert_with(AddComponentPicker::default)
        .clone();
    let before = state.clone();
    let opened = crate::agent_parity::action_button(ui, &crate::agent_parity::INSPECTOR_ADD_COMPONENT);
    if opened.clicked() {
        state.open = !state.open;
        state.focus = state.open;
    }
    let mut pick: Option<&'static str> = None;
    if state.open {
        let entries = {
            let registry = world.resource::<AppTypeRegistry>().clone();
            let registry = registry.read();
            picker_entries(
                &registry,
                &present_on_all(world, entities),
                &state.query,
                &state.recent,
            )
        };
        // Up/Down in the search box move the highlight (taken before the text box sees them).
        let search_id = egui::Id::new(PICKER_SEARCH_ID);
        let mut moved = false;
        if ui.memory(|m| m.has_focus(search_id)) {
            let (down, up) = ui.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                )
            });
            if down || up {
                state.highlight =
                    step_highlight(&entries, state.highlight.as_deref(), down).map(str::to_owned);
                moved = true;
            }
        }
        let typed = state.query.clone();
        let search = ui.add(
            egui::TextEdit::singleline(&mut state.query)
                .id(search_id)
                .hint_text("Search components")
                .desired_width(f32::INFINITY),
        );
        let search = crate::a11y_audit::name(search, egui::WidgetType::TextEdit, "Search components");
        if state.query != typed {
            // The list changed: Enter adds its first row again until Up/Down picks another.
            state.highlight = None;
        }
        if state.focus {
            search.request_focus();
            state.focus = false;
        }
        let target = enter_target(&entries, state.highlight.as_deref()).map(|entry| entry.type_path);
        let (enter, escape) =
            ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
        if search.lost_focus() && enter {
            pick = target;
        }
        if escape && (search.has_focus() || search.lost_focus()) {
            state.open = false;
        }
        let row_height = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt("add_component_picker")
            .max_height(row_height * 14.0)
            .show(ui, |ui| {
                let mut group: Option<String> = None;
                for entry in &entries {
                    let heading = if entry.recent {
                        "Recent".to_owned()
                    } else {
                        entry.category.clone()
                    };
                    if group.as_deref() != Some(heading.as_str()) {
                        ui.weak(&heading);
                        group = Some(heading);
                    }
                    let response = crate::agent_parity::action_enabled(
                        ui,
                        entry.unavailable.is_none(),
                        &crate::agent_parity::INSPECTOR_ADD_COMPONENT,
                        Some(entry.short.clone()),
                    );
                    // The row Enter adds is drawn highlighted (and scrolled to after Up/Down).
                    let response = if target == Some(entry.type_path) {
                        if moved {
                            response.scroll_to_me(None);
                        }
                        response.highlight()
                    } else {
                        response
                    };
                    let hover = match entry.unavailable {
                        Some(reason) => format!("{}: {reason}", entry.type_path),
                        None => entry.type_path.to_owned(),
                    };
                    if response.on_hover_text(hover).clicked() && entry.unavailable.is_none() {
                        pick = Some(entry.type_path);
                    }
                    if let Some(reason) = entry.unavailable {
                        ui.weak(format!("  {reason}"));
                    }
                }
                if entries.is_empty() {
                    ui.weak("No component matches");
                }
            });
    }
    // A pick is an input event, which egui delivers to one pass only: no discard guard (a guarded
    // pick in a discarded pass would be lost).
    if let Some(type_path) = pick {
        let result = if entities.len() == 1 {
            super::add_default_component(world, entities[0], type_path);
            Ok(())
        } else {
            super::add_component_to(world, entities, type_path).map(|_| ())
        };
        match result {
            Ok(()) => {
                state.note_pick(type_path);
                state.open = false;
                state.query.clear();
            }
            Err(err) => tracing::warn!(target: "regia_editor", "add component: {err}"),
        }
    }
    if state != before {
        world.insert_resource(state);
    }
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// The agent surface: `editor.component_types` (`book/agents/inspector.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.component_types",
            tool: "component_types",
            description: "The component types the Inspector's add-component picker offers: every \
                reflected component type matching `query` (fuzzy over the short name and the type \
                path), with its category (a type-level @Category, else its crate) and whether it \
                can be added to `entities` (default: the selection). A type already on every \
                entity, or one without a reflected Default, has `addable: false` and a `reason`. \
                Recent picks come first.",
            params: &[
                ParamSchema {
                    name: "query",
                    kind: "string",
                    required: false,
                    description: "Fuzzy search text; absent or empty lists every type.",
                },
                ParamSchema {
                    name: "entities",
                    kind: "array",
                    required: false,
                    description: "Entity ids whose components gray the list; absent uses the \
                        selection.",
                },
            ],
            result: "{ count, types: [{ type_path, name, category, addable, reason?, recent }] }",
            mutating: false,
            permission: Permission::Read,
        },
        editor_component_types as Handler,
    )]
}

/// `editor.component_types`.
fn editor_component_types(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = match params.get("query") {
        None | Some(Value::Null) => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => return Err(RpcError::invalid_params("`query` must be a string")),
    };
    let entities = super::pinned::entities_param(world, params)?;
    let recent = world
        .get_resource::<AddComponentPicker>()
        .map(|picker| picker.recent.clone())
        .unwrap_or_default();
    let present = present_on_all(world, &entities);
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let entries = picker_entries(&registry, &present, query, &recent);
    let types: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut row = json!({
                "type_path": entry.type_path,
                "name": entry.short,
                "category": entry.category,
                "addable": entry.unavailable.is_none(),
                "recent": entry.recent,
            });
            if let (Some(reason), Some(object)) = (entry.unavailable, row.as_object_mut()) {
                object.insert("reason".into(), Value::String(reason.to_owned()));
            }
            row
        })
        .collect();
    Ok(json!({ "count": types.len(), "types": types }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncategorized_fields_come_first_then_categories_in_first_seen_order() {
        let groups = group_fields(&[Some("Audio"), None, Some("Light"), Some("Audio"), None]);
        assert_eq!(
            groups,
            vec![
                (None, vec![1, 4]),
                (Some("Audio"), vec![0, 3]),
                (Some("Light"), vec![2]),
            ]
        );
        assert_eq!(group_fields(&[Some("A")]), vec![(Some("A"), vec![0])]);
        assert!(!has_categories(&[None, None]));
    }

    use regia_ecs::attrs::Category;

    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    #[reflect(@Category("Render"))]
    struct Glow;

    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    #[reflect(@Category("Physics"))]
    struct XxGlow;

    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    #[reflect(@Category("Render"))]
    struct XxxxxxxGlow;

    fn glow_registry() -> regia_ecs::reflect::TypeRegistry {
        let mut registry = regia_ecs::reflect::TypeRegistry::default();
        registry.register::<Glow>();
        registry.register::<XxGlow>();
        registry.register::<XxxxxxxGlow>();
        registry
    }

    fn entry(path: &'static str, unavailable: Option<&'static str>) -> PickerEntry {
        PickerEntry {
            type_path: path,
            short: path.to_owned(),
            category: "C".to_owned(),
            unavailable,
            recent: false,
        }
    }

    /// Review sprint-v06-rev-late-k #21: with a query the rows go by category, then best match,
    /// so each category heading shows once (score first gave Render, Physics, Render).
    #[test]
    fn a_query_keeps_each_category_heading_once() {
        let scores: Vec<i32> = ["Glow", "XxGlow", "XxxxxxxGlow"]
            .iter()
            .map(|short| fuzzy_match("glow", short).map_or(i32::MIN, |(score, _)| score))
            .collect();
        assert!(
            scores[0] > scores[1] && scores[1] > scores[2],
            "the scores interleave the categories (Render > Physics > Render): {scores:?}"
        );
        let registry = glow_registry();
        let rows = picker_entries(&registry, &BTreeSet::new(), "glow", &[]);
        let shown: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.category.as_str(), r.short.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("Physics", "XxGlow"),
                ("Render", "Glow"),
                ("Render", "XxxxxxxGlow")
            ],
            "category first, then best match"
        );
        // Recent picks still come first, in pick order.
        let recent = [std::any::type_name::<XxxxxxxGlow>().to_owned()];
        let rows = picker_entries(&registry, &BTreeSet::new(), "glow", &recent);
        assert_eq!(rows[0].short, "XxxxxxxGlow");
        assert!(rows[0].recent);
    }

    /// Lead decision on review #21: Enter adds the highlighted row, or the first row that can be
    /// added when nothing (or a row that is gone or grayed) is highlighted; Up/Down skip grayed rows
    /// and stop at the ends.
    #[test]
    fn enter_adds_the_highlighted_row_or_the_first_addable_one() {
        let rows = [
            entry("a", Some(REASON_PRESENT)),
            entry("b", None),
            entry("c", Some(REASON_NO_DEFAULT)),
            entry("d", None),
            entry("e", None),
        ];
        let path = |e: Option<&PickerEntry>| e.map(|e| e.type_path);
        assert_eq!(path(enter_target(&rows, None)), Some("b"), "first addable");
        assert_eq!(path(enter_target(&rows, Some("d"))), Some("d"), "the highlight");
        assert_eq!(
            path(enter_target(&rows, Some("a"))),
            Some("b"),
            "a grayed highlight falls back"
        );
        assert_eq!(path(enter_target(&rows, Some("gone"))), Some("b"));
        assert_eq!(
            step_highlight(&rows, None, true),
            Some("d"),
            "Down from the first skips c"
        );
        assert_eq!(step_highlight(&rows, Some("d"), true), Some("e"));
        assert_eq!(
            step_highlight(&rows, Some("e"), true),
            Some("e"),
            "stops at the last"
        );
        assert_eq!(step_highlight(&rows, Some("d"), false), Some("b"));
        assert_eq!(
            step_highlight(&rows, None, false),
            Some("b"),
            "stops at the first"
        );
        assert_eq!(
            step_highlight(&[entry("x", Some(REASON_PRESENT))], None, true),
            None
        );
    }

    #[test]
    fn the_crate_names_the_category() {
        assert_eq!(module_category("regia_render::lights::PointLight"), "Render");
        assert_eq!(module_category("my_game::door::Door"), "My game");
        assert_eq!(module_category("Plain"), "Plain");
    }
}
