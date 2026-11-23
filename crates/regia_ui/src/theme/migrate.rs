// SPDX-License-Identifier: MPL-2.0
//! The v0 to v1 migrator: today's `UiTheme` / `ThemeClasses` serde shape (format version 0) to a
//! `.theme.ron` v1 [`ThemeFile`] (`docs/specs/ui-default-design.md` § 6.1).
//!
//! Slots map as in § 2.1 (`background` to `color.bg`, `primary` to `color.accent`, `text_dim` to
//! `color.text-muted`, `overlay` to `color.scrim`, ...), `radius` to `radius.md`, `font_px` to
//! `type.body.size`. The migrated theme extends `regia-dark` or `regia-light` (chosen by the
//! background's lightness) so every token the v0 shape had no slot for keeps a sensible default.
//! Each class becomes a widget type or a variation of one ([`class_target`]).

use std::collections::BTreeMap;

use crate::color::UiColor;
use crate::theme::{StyleClass, ThemeClasses, UiTheme, VisualOverrides};
use crate::theme_color_ops::{self as color_ops, to_oklab};
use crate::theme_file::{PropMap, PropValue, ThemeError, ThemeFile, TokenValue, TypeDef};

/// Where a v0 class lands in the v1 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassTarget {
    /// Key in `types` (a widget type or part) or `variations`.
    pub key: String,
    /// True for a variation (goes in `variations`, extends the widget type).
    pub variation: bool,
}

/// Maps a v0 class name to its v1 home. The built-in class names style fixed widget types;
/// `primary`, `secondary`, `ghost`, `danger` and `icon` are button variations; any other name
/// is a variation of `Panel`.
pub fn class_target(name: &str) -> ClassTarget {
    let (key, variation) = match name {
        "button" => ("Button".to_owned(), false),
        "tab" => ("Tabs".to_owned(), false),
        "panel" => ("Panel".to_owned(), false),
        "toggle" => ("Toggle".to_owned(), false),
        "slider-track" => ("Slider.track".to_owned(), false),
        "nav" => ("Button.nav".to_owned(), true),
        "surface" => ("Panel.surface".to_owned(), true),
        "primary" | "secondary" | "ghost" | "danger" | "icon" => (format!("Button.{name}"), true),
        other => (format!("Panel.{other}"), true),
    };
    ClassTarget { key, variation }
}

fn hex_text(c: UiColor) -> TokenValue {
    TokenValue::Text(color_ops::to_hex(c))
}

/// `0.08` seconds as `80ms`; non-whole milliseconds stay in seconds.
fn format_duration(seconds: f32) -> String {
    let ms = seconds * 1000.0;
    if (ms - ms.round()).abs() < 1e-3 {
        format!("{}ms", ms.round())
    } else {
        format!("{seconds}s")
    }
}

/// Every [`VisualOverrides`] field as the v1 property the resolver reads back
/// (`theme::resolve::visuals`): nothing a v0 class sets is dropped (audit-v035-formats 1).
fn visuals_to_props(v: &VisualOverrides) -> PropMap {
    let mut out = PropMap::new();
    let colors = [
        ("background", v.background),
        ("border_color", v.border_color),
        ("text_color", v.text_color),
        ("focus_ring", v.focus_ring),
        ("icon_color", v.icon_color),
    ];
    for (key, c) in colors {
        if let Some(c) = c {
            out.insert(key.to_owned(), PropValue::Text(color_ops::to_hex(c)));
        }
    }
    let numbers = [
        ("border_width", v.border_width),
        ("radius", v.radius),
        ("focus_ring_width", v.focus_ring_width),
        ("focus_ring_offset", v.focus_ring_offset),
        ("padding_x", v.padding_x),
        ("height", v.height),
        ("min_width", v.min_width),
        ("min_height", v.min_height),
        ("max_width", v.max_width),
        ("max_height", v.max_height),
        ("gap", v.gap),
        ("text_px", v.text_px),
        ("elevation", v.elevation.map(f32::from)),
        ("opacity", v.opacity),
        ("scale", v.scale),
    ];
    for (key, n) in numbers {
        if let Some(n) = n {
            out.insert(key.to_owned(), PropValue::Number(n));
        }
    }
    // Stored left, top, right, bottom; written in the property's CSS order (top, right, bottom, left).
    if let Some([l, t, r, b]) = v.padding {
        out.insert(
            "padding".to_owned(),
            PropValue::List([t, r, b, l].into_iter().map(PropValue::Number).collect()),
        );
    }
    if let Some([x, y]) = v.translate {
        out.insert(
            "translate".to_owned(),
            PropValue::List(vec![PropValue::Number(x), PropValue::Number(y)]),
        );
    }
    out
}

fn class_to_type(class: &StyleClass, extends: Option<String>) -> TypeDef {
    let mut sizes = BTreeMap::new();
    for (name, layer) in [("sm", &class.sm), ("lg", &class.lg)] {
        if layer.any() {
            sizes.insert(name.to_owned(), visuals_to_props(layer));
        }
    }
    let mut def = TypeDef {
        extends,
        props: visuals_to_props(&class.base),
        hover: visuals_to_props(&class.hover),
        pressed: visuals_to_props(&class.pressed),
        focused: visuals_to_props(&class.focused),
        focus_visible: visuals_to_props(&class.focus_visible),
        disabled: visuals_to_props(&class.disabled),
        selected: visuals_to_props(&class.selected),
        error: visuals_to_props(&class.error),
        sizes,
    };
    if class.transition > 0.0 {
        def.props.insert(
            "transition".to_owned(),
            PropValue::List(vec![
                PropValue::Text(format_duration(class.transition)),
                PropValue::Text("@ease.standard".to_owned()),
            ]),
        );
    }
    def
}

/// A class's parts as widget part types (`Slider.thumb`: the class's v1 key, a dot, the part
/// name), nested parts flattened the same way.
fn insert_parts(file: &mut ThemeFile, key: &str, class: &StyleClass) {
    for (part, part_class) in &class.parts {
        let part_key = format!("{key}.{part}");
        file.types
            .insert(part_key.clone(), class_to_type(part_class, None));
        insert_parts(file, &part_key, part_class);
    }
}

/// Migrates a v0 theme and its class list to a v1 file.
pub fn migrate_v0(theme: &UiTheme, classes: &ThemeClasses) -> ThemeFile {
    let c = &theme.colors;
    let light = to_oklab(c.background).l >= 0.5;
    let slots: [(&str, UiColor); 10] = [
        ("color.bg", c.background),
        ("color.surface", c.surface),
        ("color.surface-raised", c.surface_hi),
        ("color.accent", c.primary),
        ("color.on-accent", c.on_primary),
        ("color.text", c.text),
        ("color.text-muted", c.text_dim),
        ("color.border", c.border),
        ("color.scrim", c.overlay),
        ("color.danger", c.danger),
    ];
    let mut tokens: BTreeMap<String, TokenValue> = slots
        .iter()
        .map(|(k, v)| ((*k).to_owned(), hex_text(*v)))
        .collect();
    tokens.insert("radius.md".to_owned(), TokenValue::Number(theme.radius));
    tokens.insert("type.body.size".to_owned(), TokenValue::Number(theme.font_px));

    let mut file = ThemeFile {
        name: theme.name.clone(),
        extends: Some(if light { "regia-light" } else { "regia-dark" }.to_owned()),
        scheme: None,
        tokens,
        ..ThemeFile::default()
    };
    for (name, class) in &classes.0 {
        let target = class_target(name);
        insert_parts(&mut file, &target.key, class);
        if target.variation {
            let base = target.key.split('.').next().map(str::to_owned);
            file.variations.insert(target.key, class_to_type(class, base));
        } else {
            file.types.insert(target.key, class_to_type(class, None));
        }
    }
    file
}

/// Parses a v0 `UiTheme` RON document and migrates it. v0 files carry no class list, so the
/// result has no types or variations.
pub fn migrate_v0_ron(source: &str, path: &str) -> Result<ThemeFile, ThemeError> {
    let theme: UiTheme = ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .from_str(source)
        .map_err(|e| ThemeError::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
    let file = migrate_v0(&theme, &ThemeClasses::default());
    file.check_syntax()?;
    Ok(file)
}
