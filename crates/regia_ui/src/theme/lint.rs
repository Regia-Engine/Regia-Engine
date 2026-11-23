// SPDX-License-Identifier: MPL-2.0
//! The theme linter (`docs/specs/ui-default-design.md` §§ 1, 2.1, 2.4, 6.6; lane D7, ADR-330).
//!
//! [`lint_file`] is a pure function over one parsed [`ThemeFile`] and the themes it can extend.
//! It never fails: a theme that would not load (an unknown reference, a cycle, a missing parent)
//! is reported as findings instead, so an agent can lint a draft before it applies it. It checks:
//!
//! - **contrast**: the text and boundary pairs of § 2.1 ([`contrast_pairs`]) with the WCAG 2.x
//!   relative-luminance contrast ratio (W3C, *Web Content Accessibility Guidelines 2.2*,
//!   definitions "relative luminance" and "contrast ratio"; success criteria 1.4.3 and 1.4.11),
//!   in the theme's own scheme;
//! - **references**: unknown references and reference cycles in tokens and widget properties,
//!   and references made unreachable by a missing `extends` parent or variation base;
//! - **unknown keys**: token keys and widget properties the engine does not define (kept on load
//!   and save, § 6.1, so only a warning);
//! - **touch targets**: control heights under `size.touch-min` in the `comfortable` density
//!   (`compact` is pointer-only, § 2.4).

use std::collections::{BTreeMap, BTreeSet};

use crate::color::UiColor;
use crate::theme_color_ops::contrast_ratio;
use crate::theme_file::{Density, PropValue, Scheme, ThemeFile, ThemeSet, TypeDef, looks_like_expression};
use crate::theme_tokens::{Expr, ResolveError, TokenTable, Value};

/// What a [`LintFinding`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LintKind {
    /// A text or boundary pair below its WCAG contrast ratio.
    Contrast,
    /// A reference names a token that no theme in the chain defines.
    UnknownReference,
    /// References form a loop.
    ReferenceCycle,
    /// A reference cannot be reached: the `extends` parent or a variation base is missing.
    UnreachableReference,
    /// A token key or widget property the engine does not define (kept in the file).
    UnknownKey,
    /// A control height under `size.touch-min`.
    TouchTarget,
}

impl LintKind {
    /// Stable snake_case name used by the agent protocol (`book/agents/ui-theme.md`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Contrast => "contrast",
            Self::UnknownReference => "unknown_reference",
            Self::ReferenceCycle => "reference_cycle",
            Self::UnreachableReference => "unreachable_reference",
            Self::UnknownKey => "unknown_key",
            Self::TouchTarget => "touch_target",
        }
    }
}

/// One lint result. `key` is the key path in the file (`tokens.color.text`,
/// `types.Button.props.height`, `extends`).
#[derive(Debug, Clone, PartialEq)]
pub struct LintFinding {
    /// What kind of problem this is.
    pub kind: LintKind,
    /// Key path of the value at fault.
    pub key: String,
    /// What is wrong, for people.
    pub message: String,
    /// Contrast findings: `[foreground, background]` token keys.
    pub pair: Option<[String; 2]>,
    /// Contrast findings: the measured ratio (`ratio : 1`).
    pub ratio: Option<f32>,
    /// Contrast findings: the ratio the pair needs; touch findings: the minimum size (UI px).
    pub required: Option<f32>,
    /// Touch findings: the measured size (UI px).
    pub value: Option<f32>,
}

impl LintFinding {
    fn new(kind: LintKind, key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
            message: message.into(),
            pair: None,
            ratio: None,
            required: None,
            value: None,
        }
    }
}

/// The lint of one theme.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ThemeLint {
    /// Theme name.
    pub theme: String,
    /// The scheme the contrast pairs were checked in.
    pub scheme: Scheme,
    /// The density the size tokens were read in.
    pub density: Density,
    /// Every finding, sorted by kind then key path.
    pub findings: Vec<LintFinding>,
}

impl ThemeLint {
    /// Findings of one kind.
    pub fn of_kind(&self, kind: LintKind) -> impl Iterator<Item = &LintFinding> {
        self.findings.iter().filter(move |f| f.kind == kind)
    }

    /// True when nothing was found.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// One contrast requirement: `foreground` drawn on `background` needs `required : 1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastPair {
    /// Token of the text or boundary colour.
    pub foreground: &'static str,
    /// Token of the surface it sits on.
    pub background: &'static str,
    /// Minimum WCAG contrast ratio: 4.5 for body text, 3 for large text and UI boundaries.
    pub required: f32,
    /// Where the pair shows up.
    pub role: &'static str,
}

const fn pair(
    foreground: &'static str,
    background: &'static str,
    required: f32,
    role: &'static str,
) -> ContrastPair {
    ContrastPair {
        foreground,
        background,
        required,
        role,
    }
}

/// WCAG 1.4.3 minimum for body text.
pub const TEXT_CONTRAST: f32 = 4.5;
/// WCAG 1.4.11 minimum for UI component boundaries and large text.
pub const BOUNDARY_CONTRAST: f32 = 3.0;

/// The pairs of the spec's § 2.1 contrast table for `scheme`. Muted text is checked on the
/// overlay surface in dark themes and on the sunken surface in light ones (where each is the
/// weakest surface muted text sits on). `text-disabled` is exempt (WCAG 1.4.3) and
/// `color.border` is decorative, so neither is listed.
pub fn contrast_pairs(scheme: Scheme) -> [ContrastPair; 11] {
    let muted_surface = match scheme {
        Scheme::Dark => "color.surface-overlay",
        Scheme::Light => "color.surface-sunken",
    };
    [
        pair("color.text", "color.surface", TEXT_CONTRAST, "body text"),
        pair(
            "color.text-muted",
            "color.surface",
            TEXT_CONTRAST,
            "secondary text",
        ),
        pair("color.text-muted", muted_surface, TEXT_CONTRAST, "secondary text"),
        pair(
            "color.on-accent",
            "color.accent",
            TEXT_CONTRAST,
            "text on accent fills",
        ),
        pair(
            "color.on-danger",
            "color.danger",
            TEXT_CONTRAST,
            "text on danger fills",
        ),
        pair(
            "color.accent",
            "color.surface",
            BOUNDARY_CONTRAST,
            "slider fill, tab indicator",
        ),
        pair("color.danger", "color.surface", TEXT_CONTRAST, "error text"),
        pair("color.warning", "color.surface", TEXT_CONTRAST, "warning text"),
        pair("color.success", "color.surface", TEXT_CONTRAST, "success text"),
        pair(
            "color.border-strong",
            "color.surface",
            BOUNDARY_CONTRAST,
            "control boundary",
        ),
        pair("color.focus-ring", "color.bg", BOUNDARY_CONTRAST, "focus ring"),
    ]
}

/// Token keys whose values are control heights a finger has to hit.
const TOUCH_TOKENS: [&str; 3] = ["size.control.md", "size.control.lg", "size.row"];

/// Widget types (and parts) whose base `height` is a touch target.
const TOUCH_TYPES: [&str; 13] = [
    "Button",
    "Toggle",
    "Checkbox",
    "Radio",
    "SpinBox",
    "TextInput",
    "Dropdown",
    "Tabs",
    "List.row",
    "Tree.row",
    "Menu.item",
    "Foldable",
    "Split",
];

/// Widget property names of § 6.2 (and the aliases the resolver reads).
const KNOWN_PROPS: [&str; 28] = [
    "background",
    "border",
    "border_color",
    "border_width",
    "radius",
    "padding",
    "padding_x",
    "height",
    "min_width",
    "min_height",
    "max_width",
    "max_height",
    "gap",
    "text",
    "font",
    "font_size",
    "text_px",
    "text_color",
    "icon_color",
    "shadow",
    "elevation",
    "opacity",
    "focus_ring",
    "focus_ring_width",
    "focus_ring_offset",
    "translate",
    "scale",
    "transition",
];

/// Widget property names the linter accepts besides [`KNOWN_PROPS`] (`cursor`, gap T08).
const EXTRA_PROPS: [&str; 1] = ["cursor"];

/// Token keys the engine defines: every key of the built-in `regia-dark`.
fn known_token_keys() -> &'static BTreeSet<String> {
    static KEYS: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    KEYS.get_or_init(|| {
        crate::theme_resolve::builtin_theme_set()
            .get(crate::theme_resolve::DEFAULT_THEME)
            .map(|f| {
                f.tokens
                    .keys()
                    .chain(f.densities.values().flat_map(|d| d.keys()))
                    .cloned()
                    .chain(crate::theme_tokens::TOKEN_SLOTS.iter().map(|s| s.key.to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Lints a theme that is loaded in `set`.
pub fn lint_theme(set: &ThemeSet, name: &str) -> Option<ThemeLint> {
    set.get(name).map(|file| lint_file(file, set))
}

/// Lints `file` as if it were loaded into `set` (replacing a theme of the same name). The file
/// need not be valid: load errors become findings. Pure: no IO, no globals but the built-in key
/// list.
pub fn lint_file(file: &ThemeFile, set: &ThemeSet) -> ThemeLint {
    let mut findings = Vec::new();

    // The extends chain, this file first. A missing parent ends the chain.
    let mut chain: Vec<&ThemeFile> = vec![file];
    let mut next = file.extends.clone();
    while let Some(parent) = next {
        if chain.iter().any(|f| f.name == parent) {
            findings.push(LintFinding::new(
                LintKind::UnreachableReference,
                "extends",
                format!("theme extends loop through `{parent}`: the chain is cut there"),
            ));
            break;
        }
        match set.get(&parent).filter(|p| p.name != file.name) {
            Some(p) => {
                next = p.extends.clone();
                chain.push(p);
            }
            None => {
                findings.push(LintFinding::new(
                    LintKind::UnreachableReference,
                    "extends",
                    format!(
                        "parent theme `{parent}` is not loaded: every token it would supply is unreachable"
                    ),
                ));
                break;
            }
        }
    }
    let scheme = chain.iter().find_map(|f| f.scheme).unwrap_or_default();
    let density = chain.iter().find_map(|f| f.density).unwrap_or_default();

    // Merge root first, as the loader does; remember which file and path defined each key.
    let mut table = TokenTable::default();
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    let mut types: BTreeMap<String, TypeDef> = BTreeMap::new();
    let mut variations: BTreeMap<String, TypeDef> = BTreeMap::new();
    let mut bad_syntax: BTreeSet<String> = BTreeSet::new();
    for f in chain.iter().rev() {
        let own = std::ptr::eq(*f, file);
        let overlay = f.densities.get(density.key());
        let layers = std::iter::once(("tokens".to_owned(), &f.tokens))
            .chain(overlay.map(|o| (format!("densities.{}", density.key()), o)));
        for (group, tokens) in layers {
            for (key, value) in tokens {
                match value.parse() {
                    Ok(expr) => {
                        table.insert(key.clone(), expr);
                    }
                    Err(_) => {
                        // A syntax error is a load error the loader reports; here the key just
                        // drops out, so references to it show as unknown.
                        bad_syntax.insert(key.clone());
                    }
                }
                if own {
                    paths.insert(key.clone(), format!("{group}.{key}"));
                }
            }
        }
        for (dst, src) in [(&mut types, &f.types), (&mut variations, &f.variations)] {
            for (k, def) in src {
                merge_type(dst.entry(k.clone()).or_default(), def);
            }
        }
    }

    // References in tokens.
    for key in table.keys() {
        let Some(expr) = table.get(key) else { continue };
        let mut refs = Vec::new();
        expr.references(&mut refs);
        for r in refs.iter().filter(|r| table.get(r).is_none()) {
            let path = paths.get(key).cloned().unwrap_or_else(|| format!("tokens.{key}"));
            findings.push(unknown_ref(path, key, r, &bad_syntax));
        }
    }
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let mut cycles: BTreeSet<String> = BTreeSet::new();
    for key in table.keys() {
        match table.resolve(key) {
            Ok(v) => {
                values.insert(key.to_owned(), v);
            }
            Err(ResolveError::Cycle { chain }) => {
                let mut members: Vec<String> = chain.clone();
                members.sort();
                members.dedup();
                let id = members.join(",");
                if cycles.insert(id) {
                    let first = chain.first().cloned().unwrap_or_else(|| key.to_owned());
                    let path = paths
                        .get(&first)
                        .cloned()
                        .unwrap_or_else(|| format!("tokens.{first}"));
                    findings.push(LintFinding::new(
                        LintKind::ReferenceCycle,
                        path,
                        format!("reference cycle {}", chain.join(" -> ")),
                    ));
                }
            }
            Err(_) => {}
        }
    }

    // References and property names in this file's widget types and variations.
    for (group, defs) in [("types", &file.types), ("variations", &file.variations)] {
        for (def_name, def) in defs {
            for (layer, props) in layers_of(def) {
                for (prop, value) in props {
                    let path = format!("{group}.{def_name}.{layer}.{prop}");
                    if !KNOWN_PROPS.contains(&prop.as_str()) && !EXTRA_PROPS.contains(&prop.as_str()) {
                        findings.push(LintFinding::new(
                            LintKind::UnknownKey,
                            path.clone(),
                            format!(
                                "`{prop}` is not a widget property (kept in the file, ignored by the engine)"
                            ),
                        ));
                    }
                    for text in texts(value).iter().filter(|t| looks_like_expression(t)) {
                        let Ok(expr) = Expr::parse(text) else { continue };
                        let mut refs = Vec::new();
                        expr.references(&mut refs);
                        for r in refs.iter().filter(|r| table.get(r).is_none()) {
                            findings.push(unknown_ref(path.clone(), prop, r, &bad_syntax));
                        }
                    }
                }
            }
        }
    }
    for (name, def) in &file.variations {
        let base = def
            .extends
            .clone()
            .or_else(|| name.rsplit_once('.').map(|(b, _)| b.to_owned()));
        if let Some(base) = base
            && base.contains('.')
            && !variations.contains_key(&base)
            && !types.contains_key(&base)
        {
            findings.push(LintFinding::new(
                LintKind::UnreachableReference,
                format!("variations.{name}.extends"),
                format!("base `{base}` does not exist: this variation never applies"),
            ));
        }
    }

    // Unknown token keys (this file's own keys only: inherited ones were linted with their file).
    let known = known_token_keys();
    for (key, path) in &paths {
        if !known.contains(key) {
            findings.push(LintFinding::new(
                LintKind::UnknownKey,
                path.clone(),
                format!("`{key}` is not a token the engine defines (kept in the file, unused)"),
            ));
        }
    }

    findings.extend(check_values(
        &values,
        scheme,
        density,
        &types,
        &variations,
        &paths,
    ));
    findings.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
    ThemeLint {
        theme: file.name.clone(),
        scheme,
        density,
        findings,
    }
}

fn unknown_ref(path: String, owner: &str, reference: &str, bad_syntax: &BTreeSet<String>) -> LintFinding {
    let why = if bad_syntax.contains(reference) {
        " (its value has a syntax error)"
    } else {
        ""
    };
    LintFinding::new(
        LintKind::UnknownReference,
        path,
        format!("`{owner}` references `@{reference}`, which no theme in the chain defines{why}"),
    )
}

fn merge_type(into: &mut TypeDef, child: &TypeDef) {
    if child.extends.is_some() {
        into.extends.clone_from(&child.extends);
    }
    for (dst, src) in [
        (&mut into.props, &child.props),
        (&mut into.hover, &child.hover),
        (&mut into.pressed, &child.pressed),
        (&mut into.focused, &child.focused),
        (&mut into.focus_visible, &child.focus_visible),
        (&mut into.disabled, &child.disabled),
        (&mut into.selected, &child.selected),
        (&mut into.error, &child.error),
    ] {
        dst.extend(src.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    for (size, props) in &child.sizes {
        into.sizes
            .entry(size.clone())
            .or_default()
            .extend(props.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
}

fn layers_of(def: &TypeDef) -> Vec<(String, &BTreeMap<String, PropValue>)> {
    let mut out = vec![
        ("props".to_owned(), &def.props),
        ("hover".to_owned(), &def.hover),
        ("pressed".to_owned(), &def.pressed),
        ("focused".to_owned(), &def.focused),
        ("focus_visible".to_owned(), &def.focus_visible),
        ("disabled".to_owned(), &def.disabled),
        ("selected".to_owned(), &def.selected),
        ("error".to_owned(), &def.error),
    ];
    out.extend(def.sizes.iter().map(|(k, v)| (format!("sizes.{k}"), v)));
    out
}

fn texts(value: &PropValue) -> Vec<&str> {
    match value {
        PropValue::Text(t) => vec![t.as_str()],
        PropValue::List(items) => items.iter().flat_map(texts).collect(),
        PropValue::Number(_) => Vec::new(),
    }
}

/// Composites `fg` over `bg` (linear light, straight alpha).
fn over(fg: UiColor, bg: UiColor) -> UiColor {
    let a = fg.0[3].clamp(0.0, 1.0);
    UiColor([
        fg.0[0] * a + bg.0[0] * (1.0 - a),
        fg.0[1] * a + bg.0[1] * (1.0 - a),
        fg.0[2] * a + bg.0[2] * (1.0 - a),
        1.0,
    ])
}

/// The contrast ratio of `fg` drawn on `bg`: translucent colours are composited first (the
/// background over `color.bg`, then over black).
pub fn pair_ratio(fg: UiColor, bg: UiColor, backdrop: Option<UiColor>) -> f32 {
    let base = backdrop.map_or(UiColor::BLACK, |b| over(b, UiColor::BLACK));
    let bg = over(bg, base);
    contrast_ratio(over(fg, bg), bg)
}

/// Contrast and touch checks over resolved token values.
fn check_values(
    values: &BTreeMap<String, Value>,
    scheme: Scheme,
    density: Density,
    types: &BTreeMap<String, TypeDef>,
    variations: &BTreeMap<String, TypeDef>,
    paths: &BTreeMap<String, String>,
) -> Vec<LintFinding> {
    let mut out = Vec::new();
    let color = |k: &str| values.get(k).and_then(Value::as_color);
    let backdrop = color("color.bg");
    for p in contrast_pairs(scheme) {
        let (Some(fg), Some(bg)) = (color(p.foreground), color(p.background)) else {
            continue;
        };
        let ratio = pair_ratio(fg, bg, backdrop);
        if ratio < p.required {
            let key = paths
                .get(p.foreground)
                .or_else(|| paths.get(p.background))
                .cloned()
                .unwrap_or_else(|| format!("tokens.{}", p.foreground));
            out.push(LintFinding {
                pair: Some([p.foreground.to_owned(), p.background.to_owned()]),
                ratio: Some(ratio),
                required: Some(p.required),
                ..LintFinding::new(
                    LintKind::Contrast,
                    key,
                    format!(
                        "`{}` on `{}` ({}) is {ratio:.2}:1; it needs {}:1",
                        p.foreground, p.background, p.role, p.required
                    ),
                )
            });
        }
    }

    if density == Density::Comfortable
        && let Some(min) = values.get("size.touch-min").and_then(Value::as_number)
    {
        let mut touch = |key: String, what: String, value: f32| {
            if value < min {
                out.push(LintFinding {
                    required: Some(min),
                    value: Some(value),
                    ..LintFinding::new(
                        LintKind::TouchTarget,
                        key,
                        format!("{what} is {value} UI px, under `size.touch-min` ({min} UI px)"),
                    )
                });
            }
        };
        for key in TOUCH_TOKENS {
            if let Some(v) = values.get(key).and_then(Value::as_number) {
                let path = paths.get(key).cloned().unwrap_or_else(|| format!("tokens.{key}"));
                touch(path, format!("`{key}`"), v);
            }
        }
        let table_value = |text: &str| -> Option<f32> {
            match Expr::parse(text).ok()? {
                Expr::Ref(r) => values.get(&r)?.as_number(),
                Expr::Literal(Value::Number(n)) => Some(n),
                _ => None,
            }
        };
        for (group, defs) in [("types", types), ("variations", variations)] {
            for (name, def) in defs {
                let root = if group == "types" {
                    name.as_str()
                } else {
                    name.split('.').next().unwrap_or(name)
                };
                if !TOUCH_TYPES.contains(&name.as_str()) && !TOUCH_TYPES.contains(&root) {
                    continue;
                }
                // The base and `lg` layers are touch sizes; `sm` is the dense, pointer-first size
                // (`size.control.sm` is 32 in comfortable by design).
                for (layer, props) in std::iter::once(("props".to_owned(), Some(&def.props)))
                    .chain(std::iter::once(("sizes.lg".to_owned(), def.sizes.get("lg"))))
                {
                    let height = match props.and_then(|p| p.get("height")) {
                        Some(PropValue::Number(n)) => Some(*n),
                        Some(PropValue::Text(t)) => table_value(t),
                        _ => None,
                    };
                    if let Some(h) = height {
                        touch(
                            format!("{group}.{name}.{layer}.height"),
                            format!("`{name}` height"),
                            h,
                        );
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme_file::parse_theme_ron;

    fn builtins() -> ThemeSet {
        crate::theme_resolve::builtin_theme_set()
    }

    fn child(tokens: &str) -> ThemeFile {
        let src = format!(
            "(format_version: 1, name: \"draft\", extends: Some(\"regia-dark\"), tokens: {{ {tokens} }})"
        );
        parse_theme_ron(&src, "draft.theme.ron").expect("draft parses")
    }

    #[test]
    fn the_shipped_themes_lint_clean() {
        let set = builtins();
        for name in ["regia-dark", "regia-light", "regia-high-contrast"] {
            let lint = lint_theme(&set, name).expect("loaded");
            assert!(lint.is_clean(), "{name}: {:#?}", lint.findings);
        }
        assert_eq!(
            lint_theme(&set, "regia-light").map(|l| l.scheme),
            Some(Scheme::Light)
        );
    }

    #[test]
    fn a_two_to_one_text_pair_is_flagged_with_ratio_and_requirement() {
        let lint = lint_file(&child("\"color.text\": \"#4d4d4d\""), &builtins());
        let found: Vec<_> = lint.of_kind(LintKind::Contrast).collect();
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = found[0];
        assert_eq!(
            f.pair,
            Some(["color.text".to_owned(), "color.surface".to_owned()])
        );
        let ratio = f.ratio.expect("ratio");
        assert!((1.9..2.1).contains(&ratio), "{ratio}");
        assert_eq!(f.required, Some(4.5));
        assert_eq!(f.key, "tokens.color.text");
    }

    #[test]
    fn wcag_ratio_extremes_and_translucent_foregrounds() {
        assert!((pair_ratio(UiColor::WHITE, UiColor::BLACK, None) - 21.0).abs() < 1e-3);
        assert!((pair_ratio(UiColor::BLACK, UiColor::BLACK, None) - 1.0).abs() < 1e-6);
        // A fully transparent foreground disappears into its background.
        let clear = UiColor::WHITE.with_alpha(0.0);
        assert!((pair_ratio(clear, UiColor::BLACK, None) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn unknown_unreachable_and_cyclic_references_are_reported() {
        let lint = lint_file(
            &child(
                "\"color.a\": \"lighten(@color.nope, 0.1)\", \"color.b\": \"@color.c\", \"color.c\": \"@color.b\"",
            ),
            &builtins(),
        );
        let unknown: Vec<_> = lint.of_kind(LintKind::UnknownReference).collect();
        assert_eq!(unknown.len(), 1, "{unknown:#?}");
        assert_eq!(unknown[0].key, "tokens.color.a");
        assert_eq!(lint.of_kind(LintKind::ReferenceCycle).count(), 1);
        // New keys are kept but reported.
        assert!(
            lint.of_kind(LintKind::UnknownKey)
                .any(|f| f.key == "tokens.color.a")
        );

        let orphan = parse_theme_ron(
            "(format_version: 1, name: \"orphan\", extends: Some(\"missing\"), tokens: { \"color.x\": \"@color.text\" })",
            "orphan.theme.ron",
        )
        .expect("parses");
        let lint = lint_file(&orphan, &builtins());
        assert!(
            lint.of_kind(LintKind::UnreachableReference)
                .any(|f| f.key == "extends")
        );
        assert!(
            lint.of_kind(LintKind::UnknownReference)
                .any(|f| f.key == "tokens.color.x")
        );
    }

    #[test]
    fn touch_targets_under_the_minimum_are_flagged_in_comfortable_only() {
        let lint = lint_file(&child("\"size.row\": 30"), &builtins());
        let touch: Vec<_> = lint.of_kind(LintKind::TouchTarget).collect();
        assert_eq!(touch.len(), 1, "{touch:#?}");
        assert_eq!(touch[0].key, "tokens.size.row");
        assert_eq!((touch[0].value, touch[0].required), (Some(30.0), Some(44.0)));

        let compact = parse_theme_ron(
            "(format_version: 1, name: \"tools\", extends: Some(\"regia-dark\"), density: compact)",
            "tools.theme.ron",
        )
        .expect("parses");
        let lint = lint_file(&compact, &builtins());
        assert_eq!(lint.density, Density::Compact);
        assert_eq!(
            lint.of_kind(LintKind::TouchTarget).count(),
            0,
            "compact is pointer-only"
        );
    }

    #[test]
    fn unknown_widget_properties_are_warnings() {
        let src = "(format_version: 1, name: \"draft\", extends: Some(\"regia-dark\"), \
                   types: { \"Button\": (props: { \"wobble\": 3, \"height\": 30 }) })";
        let file = parse_theme_ron(src, "draft.theme.ron").expect("parses");
        let lint = lint_file(&file, &builtins());
        assert!(
            lint.of_kind(LintKind::UnknownKey)
                .any(|f| f.key == "types.Button.props.wobble")
        );
        assert!(
            lint.of_kind(LintKind::TouchTarget)
                .any(|f| f.key == "types.Button.props.height")
        );
    }
}
