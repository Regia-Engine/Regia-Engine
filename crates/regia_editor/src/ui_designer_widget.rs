// SPDX-License-Identifier: MPL-2.0
//! Typed widget fields for the UI designer's inspector (ADR-9730, `docs/specs/81-ui-designer.md`
//! § 4.4): one row per field of each widget kind (a slider's min, max, step and value, a dropdown's
//! options, a radar's range, …) instead of the widget as one JSON value. Headless: a field is read
//! from and written to the widget's own JSON shape and decoded back by the format's serde types, so
//! a typed edit and the same edit typed as JSON produce the same widget and the same saved bytes.
//! The agent reaches the same fields through `set` with the key `widget.<field>`.

use regia_ui::authoring::WidgetDef;
use serde_json::Value;

use crate::ui_designer::DesignerError;

/// What a field holds, which picks its inspector row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// A checkbox.
    Bool,
    /// A number (`f32`).
    Number,
    /// A whole number `>= 0`.
    Integer,
    /// A whole number `>= 0`, or none.
    OptionalIndex,
    /// One line of text.
    Text,
    /// One line of text, or none: an empty row clears it (a foldable's accordion group, ADR-7827).
    OptionalText,
    /// A list of texts (one per line in the row).
    TextList,
    /// `[a, b, c, d]` numbers, or none.
    Rect4,
    /// One character, or none (a text input's mask, `.ui.ron` v11).
    Char,
    /// One of a fixed list of words (a text input's filter, `.ui.ron` v11).
    Choice(&'static [&'static str]),
    /// Any value as JSON (menu entries, scoreboard columns).
    Json,
}

/// Where a field sits in the widget's JSON value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The variant's whole payload (`toggle(true)`).
    Whole,
    /// One element of a tuple variant (`slider(value, min, max, step)`).
    Index(usize),
    /// One field of a struct variant (`radar((range: 50))`).
    Key(&'static str),
    /// One field of the optional struct at a tuple index (`text_input("", false, (max_chars: 4))`,
    /// v11): absent reads as the field's default, and an all-default struct is not written.
    IndexKey(usize, &'static str),
}

/// One typed field of a widget kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidgetField {
    /// The key in `widget.<key>` (agent `set`) and the row label.
    pub key: &'static str,
    /// What it holds.
    pub ty: FieldType,
    slot: Slot,
}

const fn f(key: &'static str, ty: FieldType, slot: Slot) -> WidgetField {
    WidgetField { key, ty, slot }
}

use FieldType::{
    Bool, Char, Choice, Integer, Json, Number, OptionalIndex, OptionalText, Rect4, Text, TextList,
};
use Slot::{Index, IndexKey, Key, Whole};

/// The `.ui.ron` names of a text input's filter (`regia_ui::authoring::TextFilterDef`).
pub const TEXT_FILTERS: &[&str] = &["any", "integer", "numeric"];

const TOGGLE: &[WidgetField] = &[f("checked", Bool, Whole)];
const SLIDER: &[WidgetField] = &[
    f("value", Number, Index(0)),
    f("min", Number, Index(1)),
    f("max", Number, Index(2)),
    f("step", Number, Index(3)),
];
const PROGRESS: &[WidgetField] = &[f("value", Number, Whole)];
const TEXT_INPUT: &[WidgetField] = &[
    f("placeholder", Text, Index(0)),
    f("multiline", Bool, Index(1)),
    f("max_chars", Integer, IndexKey(2, "max_chars")),
    f("secret", Char, IndexKey(2, "secret")),
    f("read_only", Bool, IndexKey(2, "read_only")),
    f("filter", Choice(TEXT_FILTERS), IndexKey(2, "filter")),
];
const DROPDOWN: &[WidgetField] = &[
    f("options", TextList, Index(0)),
    f("selected", OptionalIndex, Index(1)),
];
const TAB_BAR: &[WidgetField] = &[f("tabs", TextList, Index(0)), f("active", Integer, Index(1))];
const MODAL: &[WidgetField] = &[f("open", Bool, Index(0)), f("close_on_cancel", Bool, Index(1))];
const TOOLTIP: &[WidgetField] = &[f("text", Text, Index(0)), f("delay", Number, Index(1))];
const SCROLL: &[WidgetField] = &[f("inertia", Number, Index(0)), f("scrollbar", Bool, Index(1))];
const LIST: &[WidgetField] = &[f("items", Integer, Index(0)), f("row_height", Number, Index(1))];
const STICK: &[WidgetField] = &[f("action", Text, Index(0)), f("radius", Number, Index(1))];
const V_BUTTON: &[WidgetField] = &[f("action", Text, Whole)];
const MENU: &[WidgetField] = &[f("entries", Json, Whole)];
const LOBBY_LIST: &[WidgetField] = &[
    f("title", Text, Key("title")),
    f("refresh", Text, Key("refresh")),
    f("create", Text, Key("create")),
    f("empty", Text, Key("empty")),
    f("ready", Text, Key("ready")),
    f("leave", Text, Key("leave")),
    f("launch", Text, Key("launch")),
];
const SCOREBOARD: &[WidgetField] = &[
    f("title", Text, Key("title")),
    f("columns", Json, Key("columns")),
    f("sort_column", OptionalIndex, Key("sort_column")),
    f("descending", Bool, Key("descending")),
    f("group_by_team", Bool, Key("group_by_team")),
    f("hold_action", Text, Key("hold_action")),
    f("from_rules", Bool, Key("from_rules")),
];
const RADAR: &[WidgetField] = &[
    f("range", Number, Key("range")),
    f("world_rect", Rect4, Key("world_rect")),
    f("square", Bool, Key("square")),
    f("rotate", Bool, Key("rotate")),
    f("map_image", Integer, Key("map_image")),
    f("map_rect", Rect4, Key("map_rect")),
    f("layer_mask", Integer, Key("layer_mask")),
    f("max_blips", Integer, Key("max_blips")),
    f("show_north", Bool, Key("show_north")),
];
/// The `.ui.ron` names of a split's axis (`regia_ui::authoring::SplitAxisDef`, ADR-7827).
pub const SPLIT_AXES: &[&str] = &["horizontal", "vertical"];
const SPLIT: &[WidgetField] = &[
    f("axis", Choice(SPLIT_AXES), Key("axis")),
    f("ratio", Number, Key("ratio")),
    f("min_ratio", Number, Key("min_ratio")),
    f("max_ratio", Number, Key("max_ratio")),
];
const FOLDABLE: &[WidgetField] = &[
    f("title", Text, Key("title")),
    f("open", Bool, Key("open")),
    f("group", OptionalText, Key("group")),
];
const TABS: &[WidgetField] = &[
    f("tabs", TextList, Key("tabs")),
    f("active", Integer, Key("active")),
];
const MARKER_LAYER: &[WidgetField] = &[
    f("layer_mask", Integer, Key("layer_mask")),
    f("edge_margin", Number, Key("edge_margin")),
    f("max_markers", Integer, Key("max_markers")),
];

/// The widget's variant name in the format (`slider`, `lobby_list`, …).
pub fn variant_name(widget: &WidgetDef) -> &'static str {
    match widget {
        WidgetDef::Button => "button",
        WidgetDef::Toggle(_) => "toggle",
        WidgetDef::Slider(..) => "slider",
        WidgetDef::Progress(_) => "progress",
        WidgetDef::TextInput(..) => "text_input",
        WidgetDef::Dropdown(..) => "dropdown",
        WidgetDef::TabBar(..) => "tab_bar",
        WidgetDef::Modal(..) => "modal",
        WidgetDef::Tooltip(..) => "tooltip",
        WidgetDef::Scroll(..) => "scroll",
        WidgetDef::List(..) => "list",
        WidgetDef::Focusable => "focusable",
        WidgetDef::Stick(..) => "stick",
        WidgetDef::VButton(_) => "v_button",
        WidgetDef::PopupMenu(_) => "popup_menu",
        WidgetDef::ContextMenu(_) => "context_menu",
        WidgetDef::MenuBar(_) => "menu_bar",
        WidgetDef::LobbyList(_) => "lobby_list",
        WidgetDef::Scoreboard(_) => "scoreboard",
        WidgetDef::Radar(_) => "radar",
        WidgetDef::MarkerLayer(_) => "marker_layer",
        WidgetDef::Split(_) => "split",
        WidgetDef::Foldable(_) => "foldable",
        WidgetDef::Tabs(_) => "tabs",
    }
}

/// The typed fields of the widget's kind (none for `button` and `focusable`, which hold nothing).
pub fn widget_fields(widget: &WidgetDef) -> &'static [WidgetField] {
    match widget {
        WidgetDef::Button | WidgetDef::Focusable => &[],
        WidgetDef::Toggle(_) => TOGGLE,
        WidgetDef::Slider(..) => SLIDER,
        WidgetDef::Progress(_) => PROGRESS,
        WidgetDef::TextInput(..) => TEXT_INPUT,
        WidgetDef::Dropdown(..) => DROPDOWN,
        WidgetDef::TabBar(..) => TAB_BAR,
        WidgetDef::Modal(..) => MODAL,
        WidgetDef::Tooltip(..) => TOOLTIP,
        WidgetDef::Scroll(..) => SCROLL,
        WidgetDef::List(..) => LIST,
        WidgetDef::Stick(..) => STICK,
        WidgetDef::VButton(_) => V_BUTTON,
        WidgetDef::PopupMenu(_) | WidgetDef::ContextMenu(_) | WidgetDef::MenuBar(_) => MENU,
        WidgetDef::LobbyList(_) => LOBBY_LIST,
        WidgetDef::Scoreboard(_) => SCOREBOARD,
        WidgetDef::Radar(_) => RADAR,
        WidgetDef::MarkerLayer(_) => MARKER_LAYER,
        WidgetDef::Split(_) => SPLIT,
        WidgetDef::Foldable(_) => FOLDABLE,
        WidgetDef::Tabs(_) => TABS,
    }
}

/// The value a struct widget's field reads as while the file leaves it out (the writer drops
/// defaults): the v11 containers' defaults (ADR-7827). `None` for every other kind.
fn struct_default(widget: &WidgetDef, key: &str) -> Option<Value> {
    use regia_ui::authoring::{FoldableDef, SplitDef, TabsDef};
    let full = match widget {
        WidgetDef::Split(_) => {
            let d = SplitDef::default();
            serde_json::json!({ "axis": "horizontal", "ratio": d.ratio, "min_ratio": d.min_ratio, "max_ratio": d.max_ratio })
        }
        WidgetDef::Foldable(_) => {
            let d = FoldableDef::default();
            serde_json::json!({ "title": d.title, "open": d.open, "group": Value::Null })
        }
        WidgetDef::Tabs(_) => {
            let d = TabsDef::default();
            serde_json::json!({ "tabs": d.tabs, "active": d.active })
        }
        _ => return None,
    };
    full.get(key).cloned()
}

/// Whether the widget's kind has a typed field `key`.
pub fn has_field(widget: &WidgetDef, key: &str) -> bool {
    widget_fields(widget).iter().any(|f| f.key == key)
}

fn field(widget: &WidgetDef, key: &str) -> Result<WidgetField, DesignerError> {
    widget_fields(widget)
        .iter()
        .find(|f| f.key == key)
        .copied()
        .ok_or_else(|| {
            let keys: Vec<&str> = widget_fields(widget).iter().map(|f| f.key).collect();
            DesignerError::new(
                "bad_value",
                format!(
                    "a {} widget has no field {key:?}; its fields: {}",
                    variant_name(widget),
                    if keys.is_empty() {
                        "none".to_owned()
                    } else {
                        keys.join(", ")
                    }
                ),
            )
        })
}

/// The widget as the format's JSON (`{"slider": [0.5, 0.0, 1.0, 0.0]}`).
fn widget_json(widget: &WidgetDef) -> Value {
    serde_json::to_value(widget).unwrap_or(Value::Null)
}

/// The value of the typed field `key` as JSON.
pub fn widget_field(widget: &WidgetDef, key: &str) -> Result<Value, DesignerError> {
    let field = field(widget, key)?;
    let json = widget_json(widget);
    let payload = json.get(variant_name(widget)).cloned().unwrap_or(Value::Null);
    Ok(match field.slot {
        Whole => payload,
        Index(i) => payload.get(i).cloned().unwrap_or(Value::Null),
        Key(k) => payload
            .get(k)
            .cloned()
            .or_else(|| struct_default(widget, k))
            .unwrap_or(Value::Null),
        IndexKey(i, k) => payload
            .get(i)
            .and_then(|options| options.get(k))
            .cloned()
            .unwrap_or_else(|| default_of(field.ty)),
    })
}

/// What an absent optional field reads as.
fn default_of(ty: FieldType) -> Value {
    match ty {
        Bool => Value::Bool(false),
        Integer => Value::from(0),
        Choice(words) => words.first().map_or(Value::Null, |w| Value::from(*w)),
        Number => Value::from(0.0),
        Text => Value::from(""),
        TextList => Value::Array(Vec::new()),
        OptionalIndex | OptionalText | Rect4 | Char | Json => Value::Null,
    }
}

/// The widget with its typed field `key` set to `value`, decoded by the format's own types (a
/// value of the wrong shape is `bad_value` and changes nothing).
pub fn set_widget_field(widget: &WidgetDef, key: &str, value: Value) -> Result<WidgetDef, DesignerError> {
    let field = field(widget, key)?;
    let name = variant_name(widget);
    let mut json = widget_json(widget);
    let Some(payload) = json.get_mut(name) else {
        return Err(DesignerError::new(
            "bad_value",
            format!("a {name} widget holds no fields"),
        ));
    };
    match field.slot {
        Whole => *payload = value,
        Index(i) => match payload.get_mut(i) {
            Some(slot) => *slot = value,
            None => {
                return Err(DesignerError::new(
                    "bad_value",
                    format!("{name}.{key}: no slot {i}"),
                ));
            }
        },
        Key(k) => match payload.as_object_mut() {
            Some(map) => {
                map.insert(k.to_owned(), value);
            }
            None => {
                return Err(DesignerError::new(
                    "bad_value",
                    format!("{name}.{key}: not a struct"),
                ));
            }
        },
        IndexKey(i, k) => {
            let Some(items) = payload.as_array_mut() else {
                return Err(DesignerError::new(
                    "bad_value",
                    format!("{name}.{key}: not a tuple"),
                ));
            };
            if items.len() < i {
                return Err(DesignerError::new(
                    "bad_value",
                    format!("{name}.{key}: no slot {i}"),
                ));
            }
            if items.len() == i {
                items.push(Value::Object(Default::default()));
            }
            match items.get_mut(i).and_then(Value::as_object_mut) {
                // `null` resets the option to its default.
                Some(map) if value.is_null() => {
                    map.remove(k);
                }
                Some(map) => {
                    map.insert(k.to_owned(), value);
                }
                None => {
                    return Err(DesignerError::new(
                        "bad_value",
                        format!("{name}.{key}: slot {i} is not a struct"),
                    ));
                }
            }
        }
    }
    serde_json::from_value::<WidgetDef>(json)
        .map_err(|e| DesignerError::new("bad_value", format!("widget.{key}: {e}")))
}

/// Every widget kind the format has, with a sample of each (the typed rows' test matrix and the
/// agent guide's table).
pub fn sample_widgets() -> Vec<WidgetDef> {
    use regia_ui::authoring::{LobbyListDef, MarkerLayerDef, MenuEntryDef, RadarDef, ScoreboardDef};
    let entry = serde_json::from_value::<MenuEntryDef>(serde_json::json!({ "label": "Open" }));
    let entries: Vec<MenuEntryDef> = entry.into_iter().collect();
    vec![
        WidgetDef::Button,
        WidgetDef::Toggle(false),
        WidgetDef::Slider(0.5, 0.0, 1.0, 0.0),
        WidgetDef::Progress(0.5),
        WidgetDef::TextInput(String::new(), false, Default::default()),
        WidgetDef::Dropdown(vec!["One".into(), "Two".into()], Some(0)),
        WidgetDef::TabBar(vec!["A".into(), "B".into()], 0),
        WidgetDef::Modal(false, true),
        WidgetDef::Tooltip("Tip".into(), 0.5),
        WidgetDef::Scroll(0.3, true),
        WidgetDef::List(0, 32.0),
        WidgetDef::Focusable,
        WidgetDef::Stick("move".into(), 60.0),
        WidgetDef::VButton("jump".into()),
        WidgetDef::PopupMenu(entries.clone()),
        WidgetDef::ContextMenu(entries.clone()),
        WidgetDef::MenuBar(entries),
        WidgetDef::LobbyList(LobbyListDef::default()),
        WidgetDef::Scoreboard(ScoreboardDef {
            columns: vec![regia_ui::authoring::ScoreColumnDef {
                key: "score".to_owned(),
                title: "Score".to_owned(),
                width: 0.0,
            }],
            ..ScoreboardDef::default()
        }),
        WidgetDef::Radar(RadarDef::default()),
        WidgetDef::MarkerLayer(MarkerLayerDef::default()),
        WidgetDef::Split(regia_ui::authoring::SplitDef::default()),
        WidgetDef::Foldable(regia_ui::authoring::FoldableDef {
            title: "Section".to_owned(),
            ..Default::default()
        }),
        WidgetDef::Tabs(regia_ui::authoring::TabsDef {
            tabs: vec!["One".to_owned(), "Two".to_owned()],
            active: 0,
        }),
    ]
}

/// A JSON value of `ty` that differs from `current` (the test matrix's edit of each field).
pub fn changed_value(ty: FieldType, current: &Value) -> Value {
    use serde_json::json;
    match ty {
        Bool => json!(!current.as_bool().unwrap_or(false)),
        Number => json!(current.as_f64().unwrap_or(0.0) + 2.0),
        Integer => json!(
            current
                .as_u64()
                .unwrap_or(0)
                .saturating_add(3)
                .min(u64::from(u32::MAX - 1))
        ),
        OptionalIndex => match current {
            Value::Null => json!(1),
            _ => Value::Null,
        },
        Text => json!(format!("{}x", current.as_str().unwrap_or(""))),
        OptionalText => match current {
            Value::Null => json!("group"),
            _ => Value::Null,
        },
        TextList => json!(["Red", "Green", "Blue"]),
        Rect4 => match current {
            Value::Null => json!([-10.0, -20.0, 10.0, 20.0]),
            _ => Value::Null,
        },
        Json => match current {
            Value::Array(a) if !a.is_empty() => json!([]),
            _ => Value::Null,
        },
        Char => match current {
            Value::Null => json!("*"),
            _ => Value::Null,
        },
        Choice(words) => {
            let at = words
                .iter()
                .position(|w| current.as_str() == Some(*w))
                .unwrap_or(0);
            words
                .get((at + 1) % words.len().max(1))
                .map_or(Value::Null, |w| json!(*w))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::{DesignOp, UiDesigner};
    use serde_json::json;

    /// Every widget kind of the format is in the matrix and has a table entry.
    #[test]
    fn every_widget_kind_has_typed_fields() {
        let widgets = sample_widgets();
        assert_eq!(
            widgets.len(),
            24,
            "a new WidgetDef variant needs its typed fields"
        );
        for w in &widgets {
            for field in widget_fields(w) {
                widget_field(w, field.key)
                    .unwrap_or_else(|e| panic!("{}.{}: {e}", variant_name(w), field.key));
            }
        }
    }

    /// Spec 81 § 4.4 acceptance (ADR-9730): for every widget kind and every typed field, the typed
    /// path (`set widget.<field>`) and the JSON path (`set widget` with the whole widget written out
    /// by hand) save identical bytes.
    #[test]
    fn typed_and_json_paths_save_identical_bytes() {
        let mut checked = 0;
        for widget in sample_widgets() {
            for field in widget_fields(&widget) {
                let current = widget_field(&widget, field.key).expect("read");
                let value = changed_value(field.ty, &current);
                let save = |op: DesignOp| {
                    let mut designer = UiDesigner::default();
                    designer.new_document();
                    let doc = designer.doc_mut().expect("doc");
                    doc.apply(DesignOp::Add {
                        parent: Vec::new(),
                        index: None,
                        node: Box::new(regia_ui::authoring::UiNodeDef {
                            widget: Some(widget.clone()),
                            ..Default::default()
                        }),
                    })
                    .expect("add");
                    doc.apply(op)
                        .unwrap_or_else(|e| panic!("{} {}: {e}", variant_name(&widget), field.key));
                    doc.text().expect("text")
                };
                let typed = save(DesignOp::Set {
                    node: vec![0],
                    key: format!("widget.{}", field.key),
                    value: value.clone(),
                });
                // The JSON path: the whole widget as the inspector's Advanced row takes it.
                let mut json = serde_json::to_value(&widget).expect("json");
                let payload = &mut json[variant_name(&widget)];
                match field.slot {
                    Whole => *payload = value.clone(),
                    Index(i) => payload[i] = value.clone(),
                    Key(k) => payload[k] = value.clone(),
                    IndexKey(i, k) => {
                        let items = payload.as_array_mut().expect("a tuple");
                        if items.len() == i {
                            items.push(json!({}));
                        }
                        items[i][k] = value.clone();
                    }
                }
                let by_json = save(DesignOp::Set {
                    node: vec![0],
                    key: "widget".to_owned(),
                    value: json,
                });
                assert_eq!(typed, by_json, "{}.{}", variant_name(&widget), field.key);
                let base = save(DesignOp::Set {
                    node: vec![0],
                    key: "name".to_owned(),
                    value: Value::Null,
                });
                assert_ne!(
                    typed,
                    base,
                    "{}.{} changed nothing",
                    variant_name(&widget),
                    field.key
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 65, "every typed field of every kind");
    }

    /// A value of the wrong shape is refused and an unknown field names the kind's fields.
    #[test]
    fn bad_values_and_unknown_fields_are_refused() {
        let slider = WidgetDef::Slider(0.5, 0.0, 1.0, 0.0);
        let e = set_widget_field(&slider, "max", json!("high")).expect_err("not a number");
        assert_eq!(e.code, "bad_value");
        let e = set_widget_field(&slider, "radius", json!(1)).expect_err("no such field");
        assert!(e.message.contains("value, min, max, step"), "{}", e.message);
        let set = set_widget_field(&slider, "max", json!(2.0)).expect("set");
        assert_eq!(set, WidgetDef::Slider(0.5, 0.0, 2.0, 0.0));
    }

    /// ADR-9980 (`.ui.ron` v11): a text input's options read as their defaults while the file has
    /// none, a set writes the options element, and clearing every option drops it again.
    #[test]
    fn text_input_options_read_defaults_and_write_back_short() {
        let input = WidgetDef::TextInput("Name".into(), false, Default::default());
        assert_eq!(widget_field(&input, "max_chars").expect("read"), json!(0));
        assert_eq!(widget_field(&input, "secret").expect("read"), Value::Null);
        assert_eq!(widget_field(&input, "read_only").expect("read"), json!(false));
        assert_eq!(widget_field(&input, "filter").expect("read"), json!("any"));
        let pin = set_widget_field(&input, "max_chars", json!(4)).expect("set");
        let pin = set_widget_field(&pin, "secret", json!("*")).expect("set");
        let pin = set_widget_field(&pin, "filter", json!("integer")).expect("set");
        assert_eq!(
            serde_json::to_value(&pin).expect("json"),
            json!({ "text_input": ["Name", false, { "max_chars": 4, "secret": "*", "filter": "integer" }] })
        );
        assert_eq!(widget_field(&pin, "secret").expect("read"), json!("*"));
        let back = set_widget_field(&pin, "max_chars", json!(0)).expect("clear");
        let back = set_widget_field(&back, "secret", Value::Null).expect("clear");
        let back = set_widget_field(&back, "filter", json!("any")).expect("clear");
        assert_eq!(back, input, "every option default again");
        assert_eq!(
            serde_json::to_value(&back).expect("json"),
            json!({ "text_input": ["Name", false] })
        );
        for (key, bad) in [
            ("secret", json!("ab")),
            ("filter", json!("hex")),
            ("max_chars", json!(-1)),
        ] {
            let e = set_widget_field(&input, key, bad).expect_err("bad value");
            assert_eq!(e.code, "bad_value", "{key}");
        }
    }
}
