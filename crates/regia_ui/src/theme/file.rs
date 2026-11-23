// SPDX-License-Identifier: MPL-2.0
//! The `.theme.ron` v2 file model, loader and `extends` chain (`docs/formats/theme-ron.md`).
//!
//! A [`ThemeFile`] is one theme as authored. A [`ThemeSet`] holds every loaded theme by name and
//! resolves one of them through its `extends` chain into a [`ResolvedTheme`]: tokens merge
//! root-first (a child key replaces the parent's), then expressions are resolved against the
//! *merged* table, so a child that only changes `color.accent` restyles every derived token it
//! inherits. Every failure carries the key path, and a rejected file leaves the set unchanged, so
//! the previous good theme stays active.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::color::UiColor;
use crate::theme_tokens::{Easing, Expr, ResolveError, TokenTable, Value};

/// Current `.theme.ron` format version (frozen-format candidate `regia.theme`). v2 (ADR-8105 (e),
/// ADR-8303) adds the `focus_visible` layer of [`TypeDef`] and the style [`ThemeFile::rules`]; a
/// v1 file reads unchanged.
pub const THEME_FORMAT_VERSION: u32 = 2;

/// Most distinct `inside` widget types the rules of one resolved theme may name (the style
/// cache keys a node by a 64-bit mask of them).
pub const MAX_RULE_INSIDE_TYPES: usize = 64;

/// The built-in theme sources, parent first.
pub const BUILTIN_THEME_SOURCES: [(&str, &str); 4] = [
    (
        "regia-dark",
        include_str!("../../assets/themes/regia-dark.theme.ron"),
    ),
    (
        "regia-light",
        include_str!("../../assets/themes/regia-light.theme.ron"),
    ),
    (
        "regia-high-contrast",
        include_str!("../../assets/themes/regia-high-contrast.theme.ron"),
    ),
    (
        "regia-light-high-contrast",
        include_str!("../../assets/themes/regia-light-high-contrast.theme.ron"),
    ),
];

/// Colour scheme of a theme (picks `prefers-color-scheme` style defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Dark surfaces, light text.
    #[default]
    Dark,
    /// Light surfaces, dark text.
    Light,
}

/// Size density: `comfortable` for games and handhelds, `compact` for tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// 44 UI px controls, 18 px body text.
    #[default]
    Comfortable,
    /// 28 UI px controls, 13 px body text.
    Compact,
}

impl Density {
    /// The key of this density in [`ThemeFile::densities`].
    pub fn key(self) -> &'static str {
        match self {
            Self::Comfortable => "comfortable",
            Self::Compact => "compact",
        }
    }
}

/// A token value as authored: a number (UI px) or a string in the grammar of
/// [`crate::theme_tokens::Expr`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenValue {
    /// A plain number.
    Number(f32),
    /// A colour, duration, easing, reference, operation or text.
    Text(String),
}

impl TokenValue {
    /// Parses the value into an expression.
    pub fn parse(&self) -> Result<Expr, String> {
        match self {
            Self::Number(n) if n.is_finite() => Ok(Expr::Literal(Value::Number(*n))),
            Self::Number(_) => Err("number is not finite".to_owned()),
            Self::Text(t) => Expr::parse(t),
        }
    }
}

impl From<&str> for TokenValue {
    fn from(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}

impl From<f32> for TokenValue {
    fn from(n: f32) -> Self {
        Self::Number(n)
    }
}

/// A widget property value: a number, a string (token expression or keyword) or a list of those
/// (`border: [1, "@color.border"]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropValue {
    /// A number.
    Number(f32),
    /// A string: an expression when it starts with `@` or `#` or is a call, otherwise a keyword.
    Text(String),
    /// A list of values.
    List(Vec<PropValue>),
}

impl PropValue {
    /// Calls `f` with every string in this value.
    fn for_each_text(&self, f: &mut dyn FnMut(&str)) {
        match self {
            Self::Text(t) => f(t),
            Self::List(l) => l.iter().for_each(|v| v.for_each_text(f)),
            Self::Number(_) => {}
        }
    }

    /// Whether any number in this value is NaN or infinite.
    fn has_non_finite(&self) -> bool {
        match self {
            Self::Number(n) => !n.is_finite(),
            Self::List(l) => l.iter().any(Self::has_non_finite),
            Self::Text(_) => false,
        }
    }
}

/// True when a property string is meant as a token expression rather than a keyword.
pub(crate) fn looks_like_expression(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('@') || t.starts_with('#') || (t.contains('(') && t.ends_with(')'))
}

/// Properties of one state or size layer.
pub type PropMap = BTreeMap<String, PropValue>;

/// A widget type or type variation: base properties plus state and size layers.
///
/// The property names are open (a map): the property set widens in lane D2 without a format
/// change, and unknown properties survive a load and save.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TypeDef {
    /// Variation base (`Button.primary` extends `Button`); `None` means the part before the dot.
    pub extends: Option<String>,
    /// Properties of the normal state.
    pub props: PropMap,
    /// Layer applied while hovered.
    pub hover: PropMap,
    /// Layer applied while pressed.
    pub pressed: PropMap,
    /// Layer applied while focused.
    pub focused: PropMap,
    /// Layer applied over `focused` only while the focus came from the keyboard or a gamepad
    /// (v2, ADR-8105 (e)): a clicked control does not show it.
    pub focus_visible: PropMap,
    /// Layer applied while disabled.
    pub disabled: PropMap,
    /// Layer applied while selected.
    pub selected: PropMap,
    /// Layer applied in the error state.
    pub error: PropMap,
    /// Layers per size name (`sm`, `lg`).
    pub sizes: BTreeMap<String, PropMap>,
}

impl TypeDef {
    /// Every property map with its path suffix (`props`, `hover`, `sizes.sm`, ...).
    fn layers(&self) -> Vec<(String, &PropMap)> {
        let mut out = vec![
            ("props".to_owned(), &self.props),
            ("hover".to_owned(), &self.hover),
            ("pressed".to_owned(), &self.pressed),
            ("focused".to_owned(), &self.focused),
            ("focus_visible".to_owned(), &self.focus_visible),
            ("disabled".to_owned(), &self.disabled),
            ("selected".to_owned(), &self.selected),
            ("error".to_owned(), &self.error),
        ];
        out.extend(self.sizes.iter().map(|(k, v)| (format!("sizes.{k}"), v)));
        out
    }

    /// Layers `child` over `self`: child properties replace the same keys, other keys stay.
    fn merge_child(&mut self, child: &TypeDef) {
        if child.extends.is_some() {
            self.extends.clone_from(&child.extends);
        }
        let merge = |into: &mut PropMap, from: &PropMap| {
            into.extend(from.iter().map(|(k, v)| (k.clone(), v.clone())));
        };
        merge(&mut self.props, &child.props);
        merge(&mut self.hover, &child.hover);
        merge(&mut self.pressed, &child.pressed);
        merge(&mut self.focused, &child.focused);
        merge(&mut self.focus_visible, &child.focus_visible);
        merge(&mut self.disabled, &child.disabled);
        merge(&mut self.selected, &child.selected);
        merge(&mut self.error, &child.error);
        for (size, props) in &child.sizes {
            merge(self.sizes.entry(size.clone()).or_default(), props);
        }
    }
}

/// A font family and the files that provide it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FontDef {
    /// Font files relative to the asset root.
    pub files: Vec<String>,
}

/// The state a [`StyleRule`] applies in (v2). The rule's properties join that state's layer, so
/// they compose in the fixed state order like a type's own layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleState {
    /// While hovered (skipped for touch input and while disabled).
    Hover,
    /// While pressed (skipped while disabled).
    Pressed,
    /// While focused (any focus).
    Focused,
    /// While focused from the keyboard or a gamepad only.
    FocusVisible,
    /// While disabled.
    Disabled,
    /// While selected, checked or open.
    Selected,
    /// In the error state.
    Error,
}

impl RuleState {
    /// The state's name in the file (`hover`, `focus_visible`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Pressed => "pressed",
            Self::Focused => "focused",
            Self::FocusVisible => "focus_visible",
            Self::Disabled => "disabled",
            Self::Selected => "selected",
            Self::Error => "error",
        }
    }
}

/// A style rule (v2, ADR-8105 (e)): a structured selector and the properties it sets. Every
/// condition that is set must hold; a rule needs at least one of `inside`, `widget` or `classes`.
///
/// Matching (ADR-8303): a node's widget types are the type and variation keys its classes pick,
/// each class's own key (the class itself when it names a type, such as `Toolbar`; else its v0
/// class-table home, `button` → `Button`), and each key's part before its first dot
/// (`Button.primary` gives `Button.primary` and `Button`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StyleRule {
    /// A widget type some ancestor (at any depth) has: a descendant selector.
    pub inside: Option<String>,
    /// The node's widget type (`Button`).
    pub widget: Option<String>,
    /// A variation of `widget` the node's variation chain holds (`primary` for `Button.primary`);
    /// needs `widget`.
    pub variation: Option<String>,
    /// Classes the node must carry, every one.
    pub classes: Vec<String>,
    /// The state the properties apply in; `None` is the normal state.
    pub state: Option<RuleState>,
    /// The properties, as in a type's layer.
    pub props: PropMap,
}

impl StyleRule {
    /// The rule's specificity: (variation and class conditions, widget and inside conditions).
    /// Rules apply in ascending specificity, then in file order; later wins per property.
    pub fn specificity(&self) -> (usize, usize) {
        (
            usize::from(self.variation.is_some()) + self.classes.len(),
            usize::from(self.widget.is_some()) + usize::from(self.inside.is_some()),
        )
    }

    /// The rule as display text, for inspectors and the agent (never parsed back):
    /// `Toolbar Button.primary [danger-zone] :hover`.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(inside) = &self.inside {
            parts.push(inside.clone());
        }
        match (&self.widget, &self.variation) {
            (Some(w), Some(v)) => parts.push(format!("{w}.{v}")),
            (Some(w), None) => parts.push(w.clone()),
            (None, Some(v)) => parts.push(format!("*.{v}")),
            (None, None) => {}
        }
        parts.extend(self.classes.iter().map(|c| format!("[{c}]")));
        if let Some(state) = self.state {
            parts.push(format!(":{}", state.name()));
        }
        parts.join(" ")
    }
}

/// One `.theme.ron` v2 document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeFile {
    /// Format version; this loader reads [`THEME_FORMAT_VERSION`] (older files are migrated).
    pub format_version: u32,
    /// Theme name, unique in a [`ThemeSet`].
    pub name: String,
    /// Parent theme name; `None` for a root theme.
    pub extends: Option<String>,
    /// Colour scheme; `None` inherits from the parent (root default: dark).
    pub scheme: Option<Scheme>,
    /// Default density; `None` inherits from the parent (root default: comfortable).
    pub density: Option<Density>,
    /// Tokens by key (`color.accent`, `radius.md`).
    pub tokens: BTreeMap<String, TokenValue>,
    /// Token overlays applied when a density is active: `densities["compact"]`.
    pub densities: BTreeMap<String, BTreeMap<String, TokenValue>>,
    /// Font families by name.
    pub fonts: BTreeMap<String, FontDef>,
    /// Widget types (and parts such as `Slider.track`).
    pub types: BTreeMap<String, TypeDef>,
    /// Type variations (`Button.primary`).
    pub variations: BTreeMap<String, TypeDef>,
    /// Theme audio items: UI cue name (`hover`, `press`, `focus`, `confirm`, `toggle`, `cancel`)
    /// to an `Audio` cue name; empty by default. A child theme overrides its parent per cue; an
    /// empty string silences the cue (ADR-2045, [`crate::ui_sounds::apply_theme_sounds`]).
    pub sounds: BTreeMap<String, String>,
    /// Style rules (v2): structured selectors over widget type, variation, classes, an ancestor
    /// type and a state. They layer over the types and variations and under a node's
    /// `ThemeOverride`; a child theme's rules follow its parent's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<StyleRule>,
}

impl Default for ThemeFile {
    fn default() -> Self {
        Self {
            format_version: THEME_FORMAT_VERSION,
            name: String::new(),
            extends: None,
            scheme: None,
            density: None,
            tokens: BTreeMap::new(),
            densities: BTreeMap::new(),
            fonts: BTreeMap::new(),
            types: BTreeMap::new(),
            variations: BTreeMap::new(),
            sounds: BTreeMap::new(),
            rules: Vec::new(),
        }
    }
}

/// Fingerprint of the v2 serde shape for `cargo xtask check-formats`.
pub fn theme_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ThemeFile>(),
        regia_core::schema::serde_field_names::<TypeDef>(),
        regia_core::schema::serde_field_names::<FontDef>(),
        regia_core::schema::serde_variant_names::<Scheme>(),
        regia_core::schema::serde_variant_names::<Density>(),
        regia_core::schema::serde_field_names::<StyleRule>(),
        regia_core::schema::serde_variant_names::<RuleState>(),
    ])
}

/// Migration steps registered for `.theme.ron`: the v0 (`UiTheme` serde shape) to v1 step, and
/// the number-only v1 to v2 step (v2 only adds fields).
pub fn theme_migration_sources() -> Vec<u32> {
    vec![0, 1]
}

/// Why a theme failed to load. Every variant names the theme and the key path.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ThemeError {
    /// The RON text did not parse.
    #[error("theme file {path}: {message}")]
    Parse {
        /// File the text came from.
        path: String,
        /// Parser message with line and column.
        message: String,
    },
    /// The file declares a format version newer than this engine reads.
    #[error("theme file {path}: format_version {found} is newer than the supported {supported}")]
    UnsupportedVersion {
        /// File the text came from.
        path: String,
        /// Version in the file.
        found: u32,
        /// Newest version this engine reads.
        supported: u32,
    },
    /// A value has bad syntax or the wrong kind.
    #[error("theme `{theme}`: {key}: {message}")]
    Value {
        /// Theme name.
        theme: String,
        /// Key path, e.g. `tokens.color.accent-hover`.
        key: String,
        /// What is wrong.
        message: String,
    },
    /// A reference names a token that does not exist in the theme chain.
    #[error("theme `{theme}`: {key}: unknown reference `@{reference}`")]
    UnknownReference {
        /// Theme name.
        theme: String,
        /// Key path of the value holding the reference.
        key: String,
        /// The missing token key.
        reference: String,
    },
    /// References form a loop.
    #[error("theme `{theme}`: reference cycle {chain}")]
    Cycle {
        /// Theme name.
        theme: String,
        /// `tokens.a -> tokens.b -> tokens.a`.
        chain: String,
    },
    /// The requested theme is not loaded.
    #[error("unknown theme `{theme}`")]
    UnknownTheme {
        /// The missing theme name.
        theme: String,
    },
    /// `extends` names a theme that is not loaded.
    #[error("theme `{theme}`: extends unknown theme `{parent}`")]
    UnknownParent {
        /// Theme name.
        theme: String,
        /// The missing parent.
        parent: String,
    },
    /// `extends` links form a loop.
    #[error("theme extends cycle {chain}")]
    ExtendsCycle {
        /// `a -> b -> a`.
        chain: String,
    },
    /// A variation's `extends` is missing or loops.
    #[error("theme `{theme}`: {key}: {message}")]
    Variation {
        /// Theme name.
        theme: String,
        /// Key path, e.g. `variations.Button.danger.extends`.
        key: String,
        /// What is wrong.
        message: String,
    },
}

fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

/// Parses a `.theme.ron` document of any supported version. A file with no `format_version` or
/// version 0 is the pre-v1 `UiTheme` shape and is migrated ([`crate::theme_migrate`]).
pub fn parse_theme_ron(source: &str, path: &str) -> Result<ThemeFile, ThemeError> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        format_version: Option<u32>,
    }
    let parse_err = |e: ron::error::SpannedError| ThemeError::Parse {
        path: path.to_owned(),
        message: e.to_string(),
    };
    let probe: Probe = ron_options().from_str(source).map_err(parse_err)?;
    match probe.format_version.unwrap_or(0) {
        0 => crate::theme_migrate::migrate_v0_ron(source, path),
        v if v > THEME_FORMAT_VERSION => Err(ThemeError::UnsupportedVersion {
            path: path.to_owned(),
            found: v,
            supported: THEME_FORMAT_VERSION,
        }),
        v => {
            let mut file: ThemeFile = ron_options().from_str(source).map_err(parse_err)?;
            if v < 2 && file.uses_v2_fields() {
                return Err(ThemeError::Value {
                    theme: file.name.clone(),
                    key: "format_version".to_owned(),
                    message: "`rules` and `focus_visible` layers need format_version 2".to_owned(),
                });
            }
            // v1 → v2 is number-only: a v1 file reads as v2 with no rules and no
            // `focus_visible` layers, and saves as v2.
            file.format_version = THEME_FORMAT_VERSION;
            file.check_syntax()?;
            Ok(file)
        }
    }
}

impl ThemeFile {
    /// Serialises the file as RON text (keys sorted).
    pub fn to_ron_string(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
    }

    /// Whether the file uses a field v2 added (style rules or a `focus_visible` layer).
    pub fn uses_v2_fields(&self) -> bool {
        !self.rules.is_empty()
            || self
                .types
                .values()
                .chain(self.variations.values())
                .any(|d| !d.focus_visible.is_empty())
    }

    fn value_err(&self, key: String, message: String) -> ThemeError {
        ThemeError::Value {
            theme: self.name.clone(),
            key,
            message,
        }
    }

    /// Checks the syntax of every token and property value, without resolving references.
    pub fn check_syntax(&self) -> Result<(), ThemeError> {
        if self.name.trim().is_empty() {
            return Err(self.value_err("name".to_owned(), "theme name is empty".to_owned()));
        }
        for (key, v) in &self.tokens {
            v.parse()
                .map_err(|m| self.value_err(format!("tokens.{key}"), m))?;
        }
        for (density, tokens) in &self.densities {
            if !matches!(density.as_str(), "comfortable" | "compact") {
                return Err(self.value_err(
                    format!("densities.{density}"),
                    "density must be `comfortable` or `compact`".to_owned(),
                ));
            }
            for (key, v) in tokens {
                v.parse()
                    .map_err(|m| self.value_err(format!("densities.{density}.{key}"), m))?;
            }
        }
        for (group, defs) in [("types", &self.types), ("variations", &self.variations)] {
            for (name, def) in defs {
                for (layer, props) in def.layers() {
                    for (prop, value) in props {
                        // Tokens already refused non-finite numbers; property numbers did not.
                        // Regression: tests/tqs_sweep.rs::check_syntax_rejects_non_finite_property_numbers
                        if value.has_non_finite() {
                            return Err(self.value_err(
                                format!("{group}.{name}.{layer}.{prop}"),
                                "number is not finite".to_owned(),
                            ));
                        }
                        let mut bad = None;
                        value.for_each_text(&mut |t| {
                            if bad.is_none() && looks_like_expression(t) {
                                bad = Expr::parse(t).err();
                            }
                        });
                        if let Some(m) = bad {
                            return Err(self.value_err(format!("{group}.{name}.{layer}.{prop}"), m));
                        }
                    }
                }
            }
        }
        for (i, rule) in self.rules.iter().enumerate() {
            self.check_rule(i, rule)?;
        }
        Ok(())
    }

    /// The shape and value syntax of one style rule.
    fn check_rule(&self, i: usize, rule: &StyleRule) -> Result<(), ThemeError> {
        let named = |field: &str, v: &Option<String>| match v {
            Some(s) if s.trim().is_empty() => {
                Err(self.value_err(format!("rules[{i}].{field}"), "name is empty".to_owned()))
            }
            _ => Ok(()),
        };
        named("inside", &rule.inside)?;
        named("widget", &rule.widget)?;
        named("variation", &rule.variation)?;
        if rule.inside.is_none() && rule.widget.is_none() && rule.classes.is_empty() {
            return Err(self.value_err(
                format!("rules[{i}]"),
                "a rule needs `inside`, `widget` or `classes` (a rule for every node is refused)".to_owned(),
            ));
        }
        if rule.variation.is_some() && rule.widget.is_none() {
            return Err(self.value_err(
                format!("rules[{i}].variation"),
                "`variation` needs `widget`".to_owned(),
            ));
        }
        if let Some(c) = rule.classes.iter().position(|c| c.trim().is_empty()) {
            return Err(self.value_err(
                format!("rules[{i}].classes[{c}]"),
                "class name is empty".to_owned(),
            ));
        }
        for (prop, value) in &rule.props {
            if value.has_non_finite() {
                return Err(self.value_err(
                    format!("rules[{i}].props.{prop}"),
                    "number is not finite".to_owned(),
                ));
            }
            let mut bad = None;
            value.for_each_text(&mut |t| {
                if bad.is_none() && looks_like_expression(t) {
                    bad = Expr::parse(t).err();
                }
            });
            if let Some(m) = bad {
                return Err(self.value_err(format!("rules[{i}].props.{prop}"), m));
            }
        }
        Ok(())
    }
}

/// A theme with its `extends` chain flattened and every expression resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTheme {
    /// Theme name.
    pub name: String,
    /// The chain, this theme first, root last.
    pub chain: Vec<String>,
    /// Effective scheme.
    pub scheme: Scheme,
    /// Effective density (the one the overlay was applied for).
    pub density: Density,
    /// Merged token expressions.
    pub table: TokenTable,
    /// Every token resolved.
    pub values: BTreeMap<String, Value>,
    /// Merged widget types.
    pub types: BTreeMap<String, TypeDef>,
    /// Merged type variations.
    pub variations: BTreeMap<String, TypeDef>,
    /// Merged font families.
    pub fonts: BTreeMap<String, FontDef>,
    /// Merged theme sounds, cue name to `Audio` cue (ADR-2045; children override parents).
    pub sounds: BTreeMap<String, String>,
    /// Every style rule of the chain in apply order: root theme first, file order within a
    /// theme (v2). Specificity sorts them when a node is resolved.
    pub rules: Vec<StyleRule>,
}

impl ResolvedTheme {
    /// A colour token.
    pub fn color(&self, key: &str) -> Option<UiColor> {
        self.values.get(key)?.as_color()
    }

    /// A number token.
    pub fn number(&self, key: &str) -> Option<f32> {
        self.values.get(key)?.as_number()
    }

    /// A duration token in seconds.
    pub fn duration(&self, key: &str) -> Option<f32> {
        self.values.get(key)?.as_duration()
    }

    /// An easing token.
    pub fn easing(&self, key: &str) -> Option<Easing> {
        self.values.get(key)?.as_easing()
    }

    /// A text token (font family names).
    pub fn text(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.as_text()
    }
}

/// Every loaded theme by name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThemeSet {
    files: BTreeMap<String, ThemeFile>,
}

impl ThemeSet {
    /// A set holding the three built-in themes. A built-in that fails to load is logged and
    /// skipped (the unit tests keep that from happening).
    pub fn with_builtins() -> Self {
        let mut set = Self::default();
        for (name, source) in BUILTIN_THEME_SOURCES {
            if let Err(e) = set.load_ron(source, name) {
                tracing::error!("built-in theme {name} failed to load: {e}");
            }
        }
        set
    }

    /// Theme names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// One loaded file. A built-in named with its prefix from before the rename to Regia Engine
    /// (`regia_core::brand::LEGACY_THEME_PREFIX`) finds the renamed built-in.
    pub fn get(&self, name: &str) -> Option<&ThemeFile> {
        self.files.get(&self.loaded_name(name)?)
    }

    /// The name `name` is loaded under: `name` itself, or the renamed built-in for a legacy
    /// built-in name (warned once per process, ADR-1429). `None` when neither is loaded.
    fn loaded_name(&self, name: &str) -> Option<String> {
        if self.files.contains_key(name) {
            return Some(name.to_owned());
        }
        let current = regia_core::brand::current_theme_name(name)?;
        if !self.files.contains_key(&current) {
            return None;
        }
        regia_core::brand::warn_legacy_once("theme name", name, &current);
        Some(current)
    }

    /// Parses (migrating v0 files) and inserts one `.theme.ron` text. Returns the theme name.
    pub fn load_ron(&mut self, source: &str, path: &str) -> Result<String, ThemeError> {
        let file = parse_theme_ron(source, path)?;
        let name = file.name.clone();
        self.insert(file)?;
        Ok(name)
    }

    /// Inserts or replaces a theme. The whole set (this theme and every theme that extends it)
    /// is re-validated first; on any error the set is left exactly as it was.
    pub fn insert(&mut self, file: ThemeFile) -> Result<(), ThemeError> {
        file.check_syntax()?;
        let mut next = self.clone();
        next.files.insert(file.name.clone(), file);
        for name in next.files.keys() {
            next.resolve(name, None)?;
            next.resolve(name, Some(Density::Compact))?;
        }
        *self = next;
        Ok(())
    }

    /// Names of every theme that extends `name`, directly or not (for hot reload).
    pub fn descendants_of(&self, name: &str) -> Vec<String> {
        self.files
            .keys()
            .filter(|n| {
                n.as_str() != name
                    && self
                        .chain(n)
                        .is_ok_and(|c| c.iter().skip(1).any(|f| f.name == name))
            })
            .cloned()
            .collect()
    }

    /// The `extends` chain of a theme, the theme first and the root last.
    pub fn chain(&self, name: &str) -> Result<Vec<&ThemeFile>, ThemeError> {
        let mut chain: Vec<&ThemeFile> = Vec::new();
        let Some(mut current) = self.loaded_name(name) else {
            return Err(ThemeError::UnknownTheme {
                theme: name.to_owned(),
            });
        };
        let mut owner = name.to_owned();
        loop {
            let unknown = || ThemeError::UnknownParent {
                theme: owner.clone(),
                parent: current.clone(),
            };
            // A parent named with a legacy built-in name is followed under its loaded name, so
            // the cycle check below compares loaded names only.
            let loaded = self.loaded_name(&current).ok_or_else(unknown)?;
            if let Some(pos) = chain.iter().position(|f| f.name == loaded) {
                let mut names: Vec<&str> = chain[pos..].iter().map(|f| f.name.as_str()).collect();
                names.push(&loaded);
                return Err(ThemeError::ExtendsCycle {
                    chain: names.join(" -> "),
                });
            }
            let file = self.files.get(&loaded).ok_or_else(unknown)?;
            chain.push(file);
            match &file.extends {
                Some(parent) => {
                    owner.clone_from(&file.name);
                    current.clone_from(parent);
                }
                None => return Ok(chain),
            }
        }
    }

    /// Flattens a theme and resolves every token and property reference. `density` overrides
    /// the density declared by the chain.
    pub fn resolve(&self, name: &str, density: Option<Density>) -> Result<ResolvedTheme, ThemeError> {
        let chain = self.chain(name)?;
        let declared = chain.iter().find_map(|f| f.density).unwrap_or_default();
        let density = density.unwrap_or(declared);
        let scheme = chain.iter().find_map(|f| f.scheme).unwrap_or_default();

        let mut table = TokenTable::default();
        let mut types: BTreeMap<String, TypeDef> = BTreeMap::new();
        let mut variations: BTreeMap<String, TypeDef> = BTreeMap::new();
        let mut fonts = BTreeMap::new();
        let mut sounds = BTreeMap::new();
        let mut rules: Vec<StyleRule> = Vec::new();
        let mut rule_paths: Vec<String> = Vec::new();
        let mut paths: BTreeMap<String, String> = BTreeMap::new();
        for file in chain.iter().rev() {
            let overlay = file.densities.get(density.key());
            let layers = std::iter::once(("tokens", &file.tokens)).chain(overlay.map(|o| ("densities", o)));
            for (group, tokens) in layers {
                for (key, value) in tokens {
                    let path = if group == "tokens" {
                        format!("tokens.{key}")
                    } else {
                        format!("densities.{}.{key}", density.key())
                    };
                    let expr = value.parse().map_err(|m| ThemeError::Value {
                        theme: file.name.clone(),
                        key: path.clone(),
                        message: m,
                    })?;
                    table.insert(key.clone(), expr);
                    paths.insert(key.clone(), path);
                }
            }
            for (dst, src) in [(&mut types, &file.types), (&mut variations, &file.variations)] {
                for (k, def) in src {
                    dst.entry(k.clone()).or_default().merge_child(def);
                }
            }
            fonts.extend(file.fonts.iter().map(|(k, v)| (k.clone(), v.clone())));
            sounds.extend(file.sounds.iter().map(|(k, v)| (k.clone(), v.clone())));
            for (i, rule) in file.rules.iter().enumerate() {
                rules.push(rule.clone());
                rule_paths.push(format!("{}: rules[{i}]", file.name));
            }
        }

        let values = table
            .resolve_all()
            .map_err(|e| map_resolve_error(name, &paths, e))?;
        check_variations(name, &types, &variations)?;
        for (group, defs) in [("types", &types), ("variations", &variations)] {
            for (def_name, def) in defs {
                for (layer, props) in def.layers() {
                    for (prop, value) in props {
                        let path = format!("{group}.{def_name}.{layer}.{prop}");
                        check_prop(name, &table, &path, value)?;
                    }
                }
            }
        }
        for (rule, at) in rules.iter().zip(&rule_paths) {
            for (prop, value) in &rule.props {
                check_prop(name, &table, &format!("{at}.props.{prop}"), value)?;
            }
        }
        let inside: BTreeSet<&str> = rules.iter().filter_map(|r| r.inside.as_deref()).collect();
        if inside.len() > MAX_RULE_INSIDE_TYPES {
            return Err(ThemeError::Value {
                theme: name.to_owned(),
                key: "rules".to_owned(),
                message: format!(
                    "{} distinct `inside` types; at most {MAX_RULE_INSIDE_TYPES} per resolved theme",
                    inside.len()
                ),
            });
        }
        Ok(ResolvedTheme {
            name: name.to_owned(),
            chain: chain.iter().map(|f| f.name.clone()).collect(),
            scheme,
            density,
            table,
            values,
            types,
            variations,
            fonts,
            sounds,
            rules,
        })
    }

    /// Token keys of `name` that neither the built-in `regia-dark` nor the named token slots
    /// ([`crate::theme_tokens::TOKEN_SLOTS`]) define. Unknown keys are kept and saved; the linter
    /// reports them as warnings.
    pub fn unknown_token_keys(&self, name: &str) -> Vec<String> {
        let mut known: BTreeSet<String> = self
            .resolve("regia-dark", None)
            .map(|t| t.values.into_keys().collect())
            .unwrap_or_default();
        known.extend(crate::theme_tokens::TOKEN_SLOTS.iter().map(|s| s.key.to_owned()));
        self.resolve(name, None)
            .map(|t| t.values.into_keys().filter(|k| !known.contains(k)).collect())
            .unwrap_or_default()
    }
}

fn map_resolve_error(theme: &str, paths: &BTreeMap<String, String>, e: ResolveError) -> ThemeError {
    let theme = theme.to_owned();
    let path = |k: &str| paths.get(k).cloned().unwrap_or_else(|| format!("tokens.{k}"));
    match e {
        ResolveError::Unknown { key, reference } => ThemeError::UnknownReference {
            theme,
            key: path(&key),
            reference,
        },
        ResolveError::Cycle { chain } => ThemeError::Cycle {
            theme,
            chain: chain.iter().map(|k| path(k)).collect::<Vec<_>>().join(" -> "),
        },
        ResolveError::Type { key, expected, found } => ThemeError::Value {
            theme,
            key: path(&key),
            message: format!("expected a {expected}, found a {found}"),
        },
    }
}

/// Resolves every expression string inside one property value.
fn check_prop(theme: &str, table: &TokenTable, path: &str, value: &PropValue) -> Result<(), ThemeError> {
    let mut texts = Vec::new();
    value.for_each_text(&mut |t| texts.push(t.to_owned()));
    for text in texts.iter().filter(|t| looks_like_expression(t)) {
        let expr = Expr::parse(text).map_err(|m| ThemeError::Value {
            theme: theme.to_owned(),
            key: path.to_owned(),
            message: m,
        })?;
        table.resolve_expr(path, &expr).map_err(|e| match e {
            ResolveError::Unknown { reference, .. } => ThemeError::UnknownReference {
                theme: theme.to_owned(),
                key: path.to_owned(),
                reference,
            },
            ResolveError::Cycle { chain } => ThemeError::Cycle {
                theme: theme.to_owned(),
                chain: format!(
                    "{path}: {}",
                    chain
                        .iter()
                        .map(|k| format!("tokens.{k}"))
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ),
            },
            ResolveError::Type { expected, found, .. } => ThemeError::Value {
                theme: theme.to_owned(),
                key: path.to_owned(),
                message: format!("expected a {expected}, found a {found}"),
            },
        })?;
    }
    Ok(())
}

/// Checks that variation bases that name another variation exist and do not loop. A bare widget
/// type name (no dot) is supplied by the widget code and is not checked here.
fn check_variations(
    theme: &str,
    types: &BTreeMap<String, TypeDef>,
    variations: &BTreeMap<String, TypeDef>,
) -> Result<(), ThemeError> {
    let err = |key: &str, message: String| ThemeError::Variation {
        theme: theme.to_owned(),
        key: format!("variations.{key}.extends"),
        message,
    };
    for (name, def) in variations {
        let mut seen = vec![name.as_str()];
        let mut base = def.extends.as_deref();
        while let Some(b) = base {
            if seen.contains(&b) {
                seen.push(b);
                return Err(err(name, format!("extends loop {}", seen.join(" -> "))));
            }
            seen.push(b);
            match variations.get(b) {
                Some(next) => base = next.extends.as_deref(),
                None if types.contains_key(b) || !b.contains('.') => break,
                None => return Err(err(name, format!("base `{b}` does not exist"))),
            }
        }
    }
    Ok(())
}
