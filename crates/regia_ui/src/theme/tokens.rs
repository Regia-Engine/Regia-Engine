// SPDX-License-Identifier: MPL-2.0
//! Theme token values and the expression grammar (`docs/specs/ui-default-design.md` § 6.2).
//!
//! A token value is written as a plain number (UI px) or a string. A string is one of:
//!
//! - a colour: `#rgb`, `#rrggbb`, `#rrggbbaa`
//! - a duration: `80ms`, `0.08s`
//! - an easing: `cubic-bezier(x1, y1, x2, y2)` or `linear`
//! - a reference: `@color.accent`
//! - a colour operation: `lighten(c, x)`, `darken(c, x)`, `alpha(c, a)`, `mix(c1, c2, t)`, whose
//!   arguments are colours, numbers, references or nested operations
//! - anything else: text (font names)
//!
//! There are no other expressions, so themes stay data. [`Expr::parse`] checks the syntax when a
//! file loads; references are resolved and type-checked by [`TokenTable::resolve`].

use std::collections::{BTreeMap, BTreeSet};

use crate::color::UiColor;
use crate::theme_color_ops as color_ops;

/// A resolved token value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A colour.
    Color(UiColor),
    /// A number (UI px, ratio or weight).
    Number(f32),
    /// A duration in seconds.
    Duration(f32),
    /// A cubic easing curve.
    Easing(Easing),
    /// Plain text (font family names).
    Text(String),
}

impl Value {
    /// The kind name used in error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Color(_) => "color",
            Self::Number(_) => "number",
            Self::Duration(_) => "duration",
            Self::Easing(_) => "easing",
            Self::Text(_) => "text",
        }
    }

    /// The colour, if this is one.
    pub fn as_color(&self) -> Option<UiColor> {
        match self {
            Self::Color(c) => Some(*c),
            _ => None,
        }
    }

    /// The number, if this is one.
    pub fn as_number(&self) -> Option<f32> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The duration in seconds, if this is one.
    pub fn as_duration(&self) -> Option<f32> {
        match self {
            Self::Duration(d) => Some(*d),
            _ => None,
        }
    }

    /// The easing; the text `linear` also reads as [`Easing::Linear`].
    pub fn as_easing(&self) -> Option<Easing> {
        match self {
            Self::Easing(e) => Some(*e),
            Self::Text(t) if t == "linear" => Some(Easing::Linear),
            _ => None,
        }
    }

    /// The text, if this is text.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// An easing curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Easing {
    /// Constant speed.
    Linear,
    /// CSS-style cubic Bézier with control points `(x1, y1)` and `(x2, y2)`.
    CubicBezier([f32; 4]),
}

/// Parsed expression of one token value.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal value.
    Literal(Value),
    /// A reference to another token (`@color.accent`, stored without the `@`).
    Ref(String),
    /// A colour operation.
    Op(Op, Vec<Expr>),
}

/// A colour operation of the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `lighten(color, amount)`.
    Lighten,
    /// `darken(color, amount)`.
    Darken,
    /// `alpha(color, alpha)`.
    Alpha,
    /// `mix(color, color, t)`.
    Mix,
}

impl Op {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "lighten" => Some(Self::Lighten),
            "darken" => Some(Self::Darken),
            "alpha" => Some(Self::Alpha),
            "mix" => Some(Self::Mix),
            _ => None,
        }
    }

    fn arity(self) -> usize {
        match self {
            Self::Mix => 3,
            _ => 2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Lighten => "lighten",
            Self::Darken => "darken",
            Self::Alpha => "alpha",
            Self::Mix => "mix",
        }
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` into a colour.
pub fn parse_hex_color(s: &str) -> Option<UiColor> {
    let digits = s.strip_prefix('#')?;
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if digits.len() == 3 {
        let expanded: String = digits.chars().flat_map(|c| [c, c]).collect();
        return UiColor::from_hex(&format!("#{expanded}"));
    }
    UiColor::from_hex(s)
}

/// Parses `80ms` / `0.08s` into seconds. Returns `None` when `s` is not a duration.
pub fn parse_duration(s: &str) -> Option<f32> {
    let (num, per_second) = match s.strip_suffix("ms") {
        Some(n) => (n, 1000.0),
        None => (s.strip_suffix('s')?, 1.0),
    };
    let starts_numeric = num.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.');
    if !starts_numeric {
        return None;
    }
    let v: f32 = num.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v / per_second)
}

/// Splits `name(args)` into the name and the argument text.
fn split_call(s: &str) -> Option<(&str, &str)> {
    let open = s.find('(')?;
    let name = &s[..open];
    let inner = s[open + 1..].strip_suffix(')')?;
    let ident = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ident.then_some((name, inner))
}

/// Splits `s` at commas that are not inside parentheses.
fn split_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn parse_easing_args(args: &str) -> Result<Easing, String> {
    let parts = split_args(args);
    if parts.len() != 4 {
        return Err(format!("cubic-bezier takes 4 numbers, found {}", parts.len()));
    }
    let mut p = [0.0_f32; 4];
    for (slot, part) in p.iter_mut().zip(&parts) {
        *slot = part
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("`{}` is not a number", part.trim()))?;
    }
    if !(0.0..=1.0).contains(&p[0]) || !(0.0..=1.0).contains(&p[2]) {
        return Err("cubic-bezier x control points must be within 0..1".to_owned());
    }
    Ok(Easing::CubicBezier(p))
}

impl Expr {
    /// Parses a token string. Errors carry a message without the key path; the caller adds it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        if let Some(name) = t.strip_prefix('@') {
            let ok = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
            return if ok {
                Ok(Self::Ref(name.to_owned()))
            } else {
                Err(format!("malformed reference `{t}`"))
            };
        }
        if t.starts_with('#') {
            return parse_hex_color(t)
                .map(|c| Self::Literal(Value::Color(c)))
                .ok_or_else(|| format!("`{t}` is not a color (#rgb, #rrggbb or #rrggbbaa)"));
        }
        if let Some((name, inner)) = split_call(t) {
            if name == "cubic-bezier" {
                return parse_easing_args(inner).map(|e| Self::Literal(Value::Easing(e)));
            }
            let Some(op) = Op::from_name(name) else {
                return Err(format!(
                    "unknown function `{name}` (lighten, darken, alpha, mix, cubic-bezier)"
                ));
            };
            let args = split_args(inner);
            if args.len() != op.arity() {
                return Err(format!(
                    "{} takes {} arguments, found {}",
                    op.name(),
                    op.arity(),
                    args.len()
                ));
            }
            let args = args
                .iter()
                .map(|a| Self::parse_arg(a))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Self::Op(op, args));
        }
        if let Some(d) = parse_duration(t) {
            return Ok(Self::Literal(Value::Duration(d)));
        }
        Ok(Self::Literal(Value::Text(text.to_owned())))
    }

    /// An operation argument: a number literal or any expression.
    fn parse_arg(text: &str) -> Result<Self, String> {
        let t = text.trim();
        if let Ok(n) = t.parse::<f32>() {
            return if n.is_finite() {
                Ok(Self::Literal(Value::Number(n)))
            } else {
                Err(format!("`{t}` is not finite"))
            };
        }
        match Self::parse(t)? {
            Self::Literal(Value::Text(_)) => Err(format!("`{t}` is not a color, number or reference")),
            e => Ok(e),
        }
    }

    /// Collects every referenced token key.
    pub fn references(&self, out: &mut Vec<String>) {
        match self {
            Self::Ref(r) => out.push(r.clone()),
            Self::Op(_, args) => args.iter().for_each(|a| a.references(out)),
            Self::Literal(_) => {}
        }
    }
}

/// Why a token failed to resolve. The caller adds the theme name.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// The key (or a reference) is not in the table. `key` is the token being resolved.
    Unknown {
        /// Token whose expression holds the reference.
        key: String,
        /// The missing key.
        reference: String,
    },
    /// References form a loop; `chain` is `a -> b -> a`.
    Cycle {
        /// The loop, starting and ending at the same key.
        chain: Vec<String>,
    },
    /// An operation got the wrong kind of value.
    Type {
        /// Token being resolved.
        key: String,
        /// What the operation needed.
        expected: &'static str,
        /// What it got.
        found: &'static str,
    },
}

/// A table of parsed token expressions with lazy, cycle-checked resolution.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenTable {
    exprs: BTreeMap<String, Expr>,
}

impl TokenTable {
    /// Inserts or replaces one expression.
    pub fn insert(&mut self, key: impl Into<String>, expr: Expr) {
        self.exprs.insert(key.into(), expr);
    }

    /// The raw expression of a key.
    pub fn get(&self, key: &str) -> Option<&Expr> {
        self.exprs.get(key)
    }

    /// Every key in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.exprs.keys().map(String::as_str)
    }

    /// Number of tokens.
    pub fn len(&self) -> usize {
        self.exprs.len()
    }

    /// True when there are no tokens.
    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    /// Resolves one token to a value, following references.
    pub fn resolve(&self, key: &str) -> Result<Value, ResolveError> {
        let mut stack = Vec::new();
        self.resolve_inner(key, &mut stack)
    }

    /// Resolves every token, returning the first error in key order.
    pub fn resolve_all(&self) -> Result<BTreeMap<String, Value>, ResolveError> {
        self.exprs
            .keys()
            .map(|k| self.resolve(k).map(|v| (k.clone(), v)))
            .collect()
    }

    /// Resolves an expression that is not itself in the table (a widget property value).
    pub fn resolve_expr(&self, owner: &str, expr: &Expr) -> Result<Value, ResolveError> {
        let mut stack = Vec::new();
        self.eval(owner, expr, &mut stack)
    }

    fn resolve_inner(&self, key: &str, stack: &mut Vec<String>) -> Result<Value, ResolveError> {
        if let Some(pos) = stack.iter().position(|k| k == key) {
            let mut chain = stack[pos..].to_vec();
            chain.push(key.to_owned());
            return Err(ResolveError::Cycle { chain });
        }
        let expr = self.exprs.get(key).ok_or_else(|| ResolveError::Unknown {
            key: stack.last().cloned().unwrap_or_else(|| key.to_owned()),
            reference: key.to_owned(),
        })?;
        stack.push(key.to_owned());
        let out = self.eval(key, expr, stack);
        stack.pop();
        out
    }

    fn eval(&self, owner: &str, expr: &Expr, stack: &mut Vec<String>) -> Result<Value, ResolveError> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            // Outside any table token (`resolve_expr`), a missing reference belongs to `owner`.
            // Regression: tests/tqs_sweep.rs::resolve_expr_names_the_owner_of_a_missing_reference
            Expr::Ref(r) if stack.is_empty() && !self.exprs.contains_key(r) => Err(ResolveError::Unknown {
                key: owner.to_owned(),
                reference: r.clone(),
            }),
            Expr::Ref(r) => self.resolve_inner(r, stack),
            Expr::Op(op, args) => {
                if args.len() != op.arity() {
                    return Err(ResolveError::Type {
                        key: owner.to_owned(),
                        expected: "the operation's argument count",
                        found: "a different count",
                    });
                }
                let vals = args
                    .iter()
                    .map(|a| self.eval(owner, a, stack))
                    .collect::<Result<Vec<_>, _>>()?;
                let type_err = |expected: &'static str, found: &Value| ResolveError::Type {
                    key: owner.to_owned(),
                    expected,
                    found: found.kind(),
                };
                let color = |v: &Value| v.as_color().ok_or_else(|| type_err("color", v));
                let num = |v: &Value| v.as_number().ok_or_else(|| type_err("number", v));
                Ok(Value::Color(match op {
                    Op::Lighten => color_ops::lighten(color(&vals[0])?, num(&vals[1])?),
                    Op::Darken => color_ops::darken(color(&vals[0])?, num(&vals[1])?),
                    Op::Alpha => color_ops::with_alpha(color(&vals[0])?, num(&vals[1])?),
                    Op::Mix => color_ops::mix(color(&vals[0])?, color(&vals[1])?, num(&vals[2])?),
                }))
            }
        }
    }

    /// Keys referenced by any expression but absent from the table.
    pub fn dangling_references(&self) -> BTreeSet<String> {
        let mut refs = Vec::new();
        self.exprs.values().for_each(|e| e.references(&mut refs));
        refs.into_iter().filter(|r| !self.exprs.contains_key(r)).collect()
    }
}

/// One theme token slot a widget reads by name (ADR-7208): the scrollbar colors and the caption
/// `subtitle` style (`docs/specs/36-localization-exchange-dialogue-subtitles.md` § 8.3). A slot
/// a theme leaves unset falls back to the engine look described in [`TokenSlot::fallback`].
/// `ui.theme_list` lists them with the active theme's values (`book/agents/ui-theme.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSlot {
    /// Token key.
    pub key: &'static str,
    /// Value kind: `color`, `number` or `text`.
    pub kind: &'static str,
    /// What reads the slot.
    pub used_by: &'static str,
    /// What is drawn when the theme leaves the slot unset.
    pub fallback: &'static str,
}

/// The named token slots widgets read (ADR-7208, ADR-9340). The scrollbar slots are optional (no
/// built-in theme sets them, so the default look is unchanged); the `subtitle` slots are set by
/// `regia-dark` (and so every built-in theme) except `subtitle.font` (the UI font) and
/// `subtitle.plate-opacity`, which the high-contrast themes set to make the plate opaque.
pub const TOKEN_SLOTS: [TokenSlot; 11] = [
    TokenSlot {
        key: "color.scrollbar-track",
        kind: "color",
        used_by: "Scrollbars presets with a track (classic)",
        fallback: "color.text at the preset alpha (8 %)",
    },
    TokenSlot {
        key: "color.scrollbar-thumb",
        kind: "color",
        used_by: "the scroll view's built-in thumb and the Scrollbars preset thumb",
        fallback: "white at 35 % (built-in thumb); color.text at the preset alpha",
    },
    TokenSlot {
        key: "color.scrollbar-thumb-hover",
        kind: "color",
        used_by: "the Scrollbars preset thumb under the pointer",
        fallback: "color.text at the preset alpha",
    },
    TokenSlot {
        key: "color.scrollbar-thumb-active",
        kind: "color",
        used_by: "the Scrollbars preset thumb while pressed or dragged",
        fallback: "color.text at the preset alpha",
    },
    TokenSlot {
        key: "subtitle.text",
        kind: "color",
        used_by: "caption text while the player keeps the default text color (white)",
        fallback: "white",
    },
    TokenSlot {
        key: "subtitle.plate",
        kind: "color",
        used_by: "the caption box plate (its alpha multiplies the player's box opacity)",
        fallback: "black",
    },
    TokenSlot {
        key: "subtitle.plate-opacity",
        kind: "number",
        used_by: "the lowest caption box opacity (1 in the high-contrast themes)",
        fallback: "the player's box opacity",
    },
    TokenSlot {
        key: "subtitle.font",
        kind: "text",
        used_by: "the caption font family (rows are measured and drawn in it)",
        fallback: "the UI font",
    },
    TokenSlot {
        key: "subtitle.size",
        kind: "number",
        used_by: "multiplies the caption text size on top of the player's text scale (0.25 to 4)",
        fallback: "1",
    },
    TokenSlot {
        key: "subtitle.outline",
        kind: "color",
        used_by: "the stroke around caption glyphs with the Outline background",
        fallback: "black",
    },
    TokenSlot {
        key: "subtitle.outline-width",
        kind: "number",
        used_by: "the caption outline width in UI px (at most 8 physical px)",
        fallback: "2",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> TokenTable {
        let mut t = TokenTable::default();
        for (k, v) in pairs {
            t.insert(*k, Expr::parse(v).expect("test expression"));
        }
        t
    }

    #[test]
    fn classifies_every_value_form() {
        assert!(matches!(Expr::parse("#abc"), Ok(Expr::Literal(Value::Color(_)))));
        assert!(matches!(
            Expr::parse("#3d9df0"),
            Ok(Expr::Literal(Value::Color(_)))
        ));
        assert!(matches!(
            Expr::parse("#3d9df047"),
            Ok(Expr::Literal(Value::Color(_)))
        ));
        assert!(matches!(Expr::parse("@color.accent"), Ok(Expr::Ref(_))));
        assert!(matches!(Expr::parse("Inter"), Ok(Expr::Literal(Value::Text(_)))));
        assert!(matches!(
            Expr::parse("JetBrains Mono"),
            Ok(Expr::Literal(Value::Text(_)))
        ));
        assert!(matches!(Expr::parse("linear"), Ok(Expr::Literal(Value::Text(_)))));
        assert!(matches!(
            Expr::parse("cubic-bezier(0.2, 0, 0, 1)"),
            Ok(Expr::Literal(Value::Easing(Easing::CubicBezier(_))))
        ));
        assert!(matches!(
            Expr::parse("lighten(@a, 0.06)"),
            Ok(Expr::Op(Op::Lighten, _))
        ));
    }

    #[test]
    fn short_hex_expands_per_digit() {
        assert_eq!(parse_hex_color("#abc"), UiColor::from_hex("#aabbcc"));
    }

    #[test]
    fn durations_parse_to_seconds() {
        assert_eq!(parse_duration("80ms"), Some(0.08));
        assert_eq!(parse_duration("0.08s"), Some(0.08));
        assert_eq!(parse_duration("0ms"), Some(0.0));
        assert_eq!(parse_duration("1.5s"), Some(1.5));
        assert_eq!(parse_duration("fast"), None);
        assert_eq!(parse_duration("ms"), None);
        assert_eq!(parse_duration("-5ms"), None);
        // A font whose name ends in "s" is still text.
        assert!(matches!(
            Expr::parse("Noto Sans"),
            Ok(Expr::Literal(Value::Text(_)))
        ));
    }

    #[test]
    fn bad_syntax_is_rejected_with_a_message() {
        for bad in [
            "#12",
            "#gggggg",
            "@",
            "@bad key",
            "wobble(#fff, 1)",
            "lighten(#fff)",
            "lighten(#fff, 1, 2)",
            "mix(#fff, #000)",
            "cubic-bezier(0, 0, 1)",
            "cubic-bezier(2, 0, 0, 1)",
            "alpha(#fff, nope)",
        ] {
            assert!(Expr::parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn resolves_references_and_nested_operations() {
        let t = table(&[
            ("color.accent", "#3d9df0"),
            ("color.accent-hover", "lighten(@color.accent, 0.06)"),
            ("color.selection", "alpha(@color.accent, 0.28)"),
            ("color.edge", "mix(lighten(@color.accent, 0.1), #000000, 0.5)"),
            ("alias", "@color.accent-hover"),
            ("motion.fast", "80ms"),
        ]);
        let hover = t.resolve("color.accent-hover").unwrap().as_color().unwrap();
        assert_eq!(
            color_ops::to_hex(t.resolve("alias").unwrap().as_color().unwrap()),
            color_ops::to_hex(hover)
        );
        let sel = t.resolve("color.selection").unwrap().as_color().unwrap();
        assert_eq!(color_ops::to_hex(sel), "#3d9df047");
        assert!(t.resolve("color.edge").unwrap().as_color().is_some());
        assert_eq!(t.resolve("motion.fast").unwrap().as_duration(), Some(0.08));
        assert_eq!(t.resolve_all().unwrap().len(), 6);
    }

    #[test]
    fn changing_the_base_restyles_derived_tokens() {
        let a = table(&[("c", "#3d9df0"), ("h", "lighten(@c, 0.06)")]);
        let b = table(&[("c", "#e8793a"), ("h", "lighten(@c, 0.06)")]);
        assert_ne!(a.resolve("h").unwrap(), b.resolve("h").unwrap());
    }

    #[test]
    fn unknown_reference_names_the_token_and_the_missing_key() {
        let t = table(&[("color.hover", "lighten(@color.nope, 0.1)")]);
        assert_eq!(
            t.resolve("color.hover"),
            Err(ResolveError::Unknown {
                key: "color.hover".to_owned(),
                reference: "color.nope".to_owned()
            })
        );
        assert_eq!(
            t.dangling_references().into_iter().collect::<Vec<_>>(),
            ["color.nope"]
        );
    }

    #[test]
    fn cycles_are_reported_with_the_chain() {
        let t = table(&[("a", "@b"), ("b", "lighten(@c, 0.1)"), ("c", "@a")]);
        match t.resolve("a") {
            Err(ResolveError::Cycle { chain }) => assert_eq!(chain, ["a", "b", "c", "a"]),
            other => panic!("expected a cycle, got {other:?}"),
        }
        let selfref = table(&[("x", "@x")]);
        assert!(matches!(selfref.resolve("x"), Err(ResolveError::Cycle { .. })));
    }

    #[test]
    fn operations_type_check_their_arguments() {
        let t = table(&[
            ("n", "@motion"),
            ("motion", "80ms"),
            ("bad", "lighten(@motion, 0.1)"),
        ]);
        assert_eq!(
            t.resolve("bad"),
            Err(ResolveError::Type {
                key: "bad".to_owned(),
                expected: "color",
                found: "duration"
            })
        );
        assert!(t.resolve("n").is_ok());
    }

    #[test]
    fn easing_text_linear_reads_as_linear() {
        assert_eq!(Value::Text("linear".to_owned()).as_easing(), Some(Easing::Linear));
        assert_eq!(Value::Text("Inter".to_owned()).as_easing(), None);
    }
}
