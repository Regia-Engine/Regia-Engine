// SPDX-License-Identifier: MPL-2.0
//! The typed form of the Abilities inspector's asset editor (spec 53 § 6, ADR-7472).
//!
//! The three ability formats (`.attrs.ron`, `.effect.ron`, `.ability.ron`) are serde types, so the
//! form edits their serde form: the text is parsed into the typed asset, shown as a JSON value tree
//! (numbers, booleans, strings, lists, records; an enum variant with data is a one-key record), and
//! a field edit sets one value of that tree, parses the tree back into the typed asset (which checks
//! the value's shape) and writes the canonical text (`regia_framework::abilities::to_ron`). The
//! asset editor then validates that text exactly as it validates typed text, so both paths share one
//! validator and one Save. A typed edit of a canonical file writes the bytes a text edit of the same
//! value writes; a hand-formatted file is rewritten canonically by its first typed edit, so its RON
//! comments are lost: the form says so before that edit ([`COMMENTS_WARNING`], [`has_comments`]) and
//! the agent reply carries the same warning (`comments_warning`, `comments_dropped`).
//!
//! Lists grow and shrink and an enum switches variant through the same [`set_field`] of the whole
//! list or enum value ([`list_with_entry`], [`list_without_entry`], [`variant_value`]); a new entry
//! or variant is default-filled from the format's serde types ([`tree::filled`]).
//!
//! A non-finite number written by hand (`NaN`, `inf`, `-inf`) is kept: the tree shows it as
//! `{ "non_finite": spelling }`, other typed edits write it back as it was, and a number typed over
//! it corrects it ([`tree`]). `format_version` is shown but never edited by a typed edit
//! (`abilities_panel_read_only`): the format's version is the file's, not a value to tune.
//!
//! Validation messages carry no field path, so [`diagnostic_field`] places each beside the
//! top-level field its message names (`chance must be in 0..=1` → `chance`), with a short table for
//! the codes whose message names none; the rest are shown for the asset as a whole.

use regia_framework::abilities::{self as ab, AbilityAsset, AttributesAsset, EffectAsset};
use serde_json::Value;

use crate::data_panels::PanelError;

pub mod tree;

/// The top-level field a typed edit never sets (it is shown read-only).
pub const READ_ONLY_FIELD: &str = "format_version";

/// Which of the three formats a form edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormKind {
    /// `.attrs.ron`.
    Attributes,
    /// `.effect.ron`.
    Effect,
    /// `.ability.ron`.
    Ability,
}

impl FormKind {
    /// The kind a path names by its suffix.
    #[must_use]
    pub fn of(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".attrs.ron") {
            Some(Self::Attributes)
        } else if lower.ends_with(".effect.ron") {
            Some(Self::Effect)
        } else if lower.ends_with(".ability.ron") {
            Some(Self::Ability)
        } else {
            None
        }
    }
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("abilities_panel_{code}"), message)
}

/// The asset in `text` as its serde value tree.
///
/// # Errors
///
/// The parse diagnostic (`parse_error`, `unknown_field`, `format_too_new`) when the text is not a
/// valid asset of `kind`: the form then has nothing to show and the raw text is edited instead.
pub fn form_value(kind: FormKind, text: &str) -> Result<Value, ab::Diagnostic> {
    fn tree<T: serde::Serialize>(value: &T) -> Result<Value, ab::Diagnostic> {
        tree::to_tree(value).map_err(|e| ab::Diagnostic {
            code: "parse_error",
            severity: ab::Severity::Error,
            message: e.to_string(),
            span: None,
        })
    }
    match kind {
        FormKind::Attributes => tree(&ab::parse_ron::<AttributesAsset>(
            text,
            ab::ATTRIBUTES_FORMAT_VERSION,
        )?),
        FormKind::Effect => tree(&ab::parse_ron::<EffectAsset>(text, ab::EFFECT_FORMAT_VERSION)?),
        FormKind::Ability => tree(&ab::parse_ron::<AbilityAsset>(text, ab::ABILITY_FORMAT_VERSION)?),
    }
}

/// The canonical text of a value tree of `kind`.
///
/// # Errors
///
/// `abilities_panel_bad_value` when the tree does not fit the format (`detail` names the field).
pub fn canonical_text(kind: FormKind, value: &Value, detail: &str) -> Result<String, PanelError> {
    fn text<T: serde::de::DeserializeOwned + serde::Serialize>(
        value: &Value,
        detail: &str,
    ) -> Result<String, PanelError> {
        let typed: T =
            tree::from_tree(value.clone()).map_err(|e| err("bad_value", format!("{detail}: {e}")))?;
        ab::to_ron(&typed).map_err(|e| err("bad_value", format!("{detail}: {e}")))
    }
    match kind {
        FormKind::Attributes => text::<AttributesAsset>(value, detail),
        FormKind::Effect => text::<EffectAsset>(value, detail),
        FormKind::Ability => text::<AbilityAsset>(value, detail),
    }
}

/// One step of a form field path.
#[derive(Debug, PartialEq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// Splits `live_mods[0].amount` into steps; `None` for a malformed path.
fn steps(path: &str) -> Option<Vec<Step<'_>>> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let (key, mut rest) = match part.find('[') {
            Some(at) => (&part[..at], &part[at..]),
            None => (part, ""),
        };
        if key.is_empty() {
            return None;
        }
        out.push(Step::Key(key));
        while let Some(inner) = rest.strip_prefix('[') {
            let close = inner.find(']')?;
            out.push(Step::Index(inner[..close].parse().ok()?));
            rest = &inner[close + 1..];
        }
        if !rest.is_empty() {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Sets `field` (a dotted path with `[i]` indices: `chance`, `lifetime.Timed.Const`,
/// `live_mods[0].amount.Const`) of the asset in `text` to `value` and returns the canonical text.
///
/// # Errors
///
/// `abilities_panel_read_only` for `format_version` (or a path under it), `abilities_panel_parse`
/// when `text` does not parse (edit the raw text), `abilities_panel_no_field` when the asset has no
/// such field, `abilities_panel_bad_value` when the value does not fit it.
pub fn set_field(kind: FormKind, text: &str, field: &str, value: &Value) -> Result<String, PanelError> {
    read_only_check(field)?;
    let mut tree = parsed_tree(kind, text)?;
    *at_field(&mut tree, field)? = value.clone();
    canonical_text(kind, &tree, &format!("`{field}` does not take {value}"))
}

/// The value at `field` of a form tree.
fn at_field<'a>(tree: &'a mut Value, field: &str) -> Result<&'a mut Value, PanelError> {
    let missing = || err("no_field", format!("the asset has no field `{field}`"));
    let mut at = tree;
    for step in steps(field).ok_or_else(missing)? {
        at = match step {
            Step::Key(key) => at.as_object_mut().and_then(|o| o.get_mut(key)),
            Step::Index(i) => at.as_array_mut().and_then(|a| a.get_mut(i)),
        }
        .ok_or_else(missing)?;
    }
    Ok(at)
}

/// The form tree of `text`, or `abilities_panel_parse`.
fn parsed_tree(kind: FormKind, text: &str) -> Result<Value, PanelError> {
    form_value(kind, text).map_err(|d| {
        err(
            "parse",
            format!(
                "the text does not parse ({}: {}); edit it in Advanced",
                d.code, d.message
            ),
        )
    })
}

/// `tree` with its fill records (`tree::fill_record`) replaced by default-filled values of the
/// format's types there.
fn fill_in(kind: FormKind, tree: &Value) -> Result<Value, serde_json::Error> {
    match kind {
        FormKind::Attributes => tree::filled::<AttributesAsset>(tree),
        FormKind::Effect => tree::filled::<EffectAsset>(tree),
        FormKind::Ability => tree::filled::<AbilityAsset>(tree),
    }
}

/// The list at `field` with one more entry, for a [`set_field`] of `field` (the edit the form's **Add
/// entry** makes, and an agent makes by setting the whole list): the entry is a default-filled value of
/// the list's element type (serde defaults, else zero values, an enum's first variant); when the type
/// has no such value (a tag cannot be empty) the last entry is repeated.
///
/// # Errors
///
/// `abilities_panel_parse`, `abilities_panel_no_field`, `abilities_panel_read_only`,
/// `abilities_panel_bad_value` (`field` is not a list, or an empty list whose entry cannot be made:
/// type the first one in Advanced).
pub fn list_with_entry(kind: FormKind, text: &str, field: &str) -> Result<Value, PanelError> {
    read_only_check(field)?;
    let mut tree = parsed_tree(kind, text)?;
    let list = at_field(&mut tree, field)?;
    let Some(items) = list.as_array_mut() else {
        return Err(err("bad_value", format!("`{field}` is not a list")));
    };
    let last = items.last().cloned();
    let at = items.len();
    items.push(tree::fill_record(None));
    match fill_in(kind, &tree) {
        Ok(mut filled) => at_field(&mut filled, field).map(|v| v.clone()),
        Err(e) => match last {
            Some(last) => {
                let mut original = parsed_tree(kind, text)?;
                let mut list = at_field(&mut original, field)?.clone();
                if let Some(items) = list.as_array_mut() {
                    items.push(last);
                }
                Ok(list)
            }
            None => Err(err(
                "bad_value",
                format!("`{field}[{at}]` cannot be filled in ({e}); type the first entry in Advanced"),
            )),
        },
    }
}

/// The list at `field` without entry `index`, for a [`set_field`] of `field` (the form's **Remove
/// entry**).
///
/// # Errors
///
/// `abilities_panel_parse`, `abilities_panel_no_field` (also for an index past the end),
/// `abilities_panel_read_only`, `abilities_panel_bad_value` (`field` is not a list).
pub fn list_without_entry(
    kind: FormKind,
    text: &str,
    field: &str,
    index: usize,
) -> Result<Value, PanelError> {
    read_only_check(field)?;
    let mut tree = parsed_tree(kind, text)?;
    let mut list = at_field(&mut tree, field)?.clone();
    let Some(items) = list.as_array_mut() else {
        return Err(err("bad_value", format!("`{field}` is not a list")));
    };
    if index >= items.len() {
        return Err(err(
            "no_field",
            format!("the asset has no field `{field}[{index}]`"),
        ));
    }
    items.remove(index);
    Ok(list)
}

/// The value at `field` switched to enum variant `variant`, for a [`set_field`] of `field` (the form's
/// variant picker): the variant's fields are default-filled (serde defaults, else zero values) and the
/// previous variant's values are dropped.
///
/// # Errors
///
/// `abilities_panel_parse`, `abilities_panel_no_field`, `abilities_panel_read_only`,
/// `abilities_panel_bad_value` (`field` is not an enum, or it has no such variant).
pub fn variant_value(kind: FormKind, text: &str, field: &str, variant: &str) -> Result<Value, PanelError> {
    read_only_check(field)?;
    let mut tree = parsed_tree(kind, text)?;
    let enums = enum_paths(kind, text);
    match enums.get(field) {
        Some(variants) if variants.contains(&variant) => {}
        Some(variants) => {
            return Err(err(
                "bad_value",
                format!("`{field}` has no variant `{variant}` (one of {variants:?})"),
            ));
        }
        None => return Err(err("bad_value", format!("`{field}` is not an enum"))),
    }
    *at_field(&mut tree, field)? = tree::fill_record(Some(variant));
    let mut filled = fill_in(kind, &tree).map_err(|e| {
        err(
            "bad_value",
            format!("`{field}` as `{variant}` cannot be filled in: {e}"),
        )
    })?;
    at_field(&mut filled, field).map(|v| v.clone())
}

/// Every form path of the asset in `text` that holds an enum value, with the enum's variants (empty
/// while the text does not parse).
#[must_use]
pub fn enum_paths(kind: FormKind, text: &str) -> std::collections::BTreeMap<String, &'static [&'static str]> {
    let Ok(tree) = form_value(kind, text) else {
        return std::collections::BTreeMap::new();
    };
    match kind {
        FormKind::Attributes => tree::enum_variants::<AttributesAsset>(&tree),
        FormKind::Effect => tree::enum_variants::<EffectAsset>(&tree),
        FormKind::Ability => tree::enum_variants::<AbilityAsset>(&tree),
    }
    .unwrap_or_default()
}

fn read_only_check(field: &str) -> Result<(), PanelError> {
    if steps(field).and_then(|s| s.into_iter().next()) == Some(Step::Key(READ_ONLY_FIELD)) {
        return Err(err(
            "read_only",
            format!("`{READ_ONLY_FIELD}` is the file's format version and is not edited in the form"),
        ));
    }
    Ok(())
}

/// What the form tells the user (and the agent reply carries as `comments_warning`) before the first
/// typed edit of a file that has comments.
pub const COMMENTS_WARNING: &str = "This file has comments: a typed edit rewrites it in its canonical form and \
    drops them (edit in Advanced to keep them).";

/// Whether RON `text` has a `//` or `/* */` comment outside strings (a typed edit writes the canonical
/// text, which has none).
#[must_use]
pub fn has_comments(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' if matches!(b.get(i + 1), Some(b'/' | b'*')) => return true,
            _ => i += 1,
        }
    }
    false
}

/// Whether `word` occurs in `text` as a whole word (`[a-z0-9_]` boundaries).
fn names_word(text: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// The top-level field a diagnostic of `kind` is shown beside, or `None` for the asset as a whole:
/// the first of `fields` (the form's top-level names, in order) its message names as a whole word,
/// else the field its code always concerns.
#[must_use]
pub fn diagnostic_field<'a>(
    kind: FormKind,
    diagnostic: &ab::Diagnostic,
    fields: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    // A parse error has a line and column instead: the raw text shows it.
    if diagnostic.span.is_some() {
        return None;
    }
    if let Some(field) = fields
        .into_iter()
        .find(|field| names_word(&diagnostic.message, field))
    {
        return Some(field.to_owned());
    }
    let field = match (kind, diagnostic.code) {
        (FormKind::Effect, "effect_bad_lifetime") => "lifetime",
        (FormKind::Effect, "curve_unsorted") => "curves",
        (FormKind::Attributes, _) if diagnostic.code.starts_with("attribute") => "attributes",
        _ => return None,
    };
    Some(field.to_owned())
}

/// The top-level field names of a form value, in order.
#[must_use]
pub fn top_fields(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use regia_framework::abilities::{AttributeDecl, Lifetime, Magnitude};

    use super::*;

    fn effect() -> EffectAsset {
        let mut e = EffectAsset::new(Lifetime::Timed(Magnitude::Const(0.25)));
        e.name = "Slow".into();
        e.chance = 0.75;
        e
    }

    /// The value tree round-trips every format losslessly: the canonical text of an unedited
    /// tree is the canonical text of the asset.
    #[test]
    fn unedited_trees_write_the_canonical_text() {
        let effect = ab::to_ron(&effect()).expect("effect");
        let mut ability = AbilityAsset::new("dash");
        ability.max_run_ticks = Some(12);
        ability.input_code = Some(3);
        let ability = ab::to_ron(&ability).expect("ability");
        let attributes = ab::to_ron(&AttributesAsset {
            format_version: ab::ATTRIBUTES_FORMAT_VERSION,
            description: "hero".into(),
            attributes: vec![AttributeDecl {
                name: "health".into(),
                base: 90.5,
                min: 0.0,
                max: 100.0,
                max_from: None,
                derived: None,
            }],
        })
        .expect("attributes");
        for (kind, text) in [
            (FormKind::Effect, effect),
            (FormKind::Ability, ability),
            (FormKind::Attributes, attributes),
        ] {
            let tree = form_value(kind, &text).expect("parses");
            assert_eq!(canonical_text(kind, &tree, "").expect("writes"), text, "{kind:?}");
        }
    }

    #[test]
    fn paths_split_into_keys_and_indices() {
        assert_eq!(
            steps("live_mods[0].amount.Const"),
            Some(vec![
                Step::Key("live_mods"),
                Step::Index(0),
                Step::Key("amount"),
                Step::Key("Const")
            ])
        );
        assert_eq!(
            steps("a[1][2]"),
            Some(vec![Step::Key("a"), Step::Index(1), Step::Index(2)])
        );
        for bad in ["", ".a", "a.", "a[x]", "a[1", "a[1]b", "[0]"] {
            assert_eq!(steps(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn diagnostics_go_beside_the_field_their_message_or_code_names() {
        let tree = form_value(FormKind::Effect, &ab::to_ron(&effect()).expect("text")).expect("tree");
        let fields = top_fields(&tree);
        let at = |code: &'static str, message: &str| {
            let d = ab::Diagnostic {
                code,
                severity: ab::Severity::Error,
                message: message.to_owned(),
                span: None,
            };
            diagnostic_field(FormKind::Effect, &d, fields.iter().copied())
        };
        assert_eq!(
            at("effect_bad_lifetime", "chance must be in 0..=1").as_deref(),
            Some("chance")
        );
        assert_eq!(
            at("effect_bad_lifetime", "pulse.every must be above 0 seconds").as_deref(),
            Some("pulse")
        );
        assert_eq!(
            at(
                "effect_bad_lifetime",
                "a Timed length must be a finite number of seconds above 0"
            )
            .as_deref(),
            Some("lifetime")
        );
        assert_eq!(
            at("stack_cap_zero", "stack.cap must be at least 1").as_deref(),
            Some("stack")
        );
        assert_eq!(
            at(
                "effect_instant_live_mods",
                "an Instant effect cannot have live_mods"
            )
            .as_deref(),
            Some("live_mods")
        );
        assert_eq!(at("undeclared_tag", "tag `Status.Burn` is not declared"), None);
        // `chances` is not `chance`.
        assert_eq!(at("x", "chances are"), None);
        let spanned = ab::Diagnostic {
            code: "parse_error",
            severity: ab::Severity::Error,
            message: "chance".into(),
            span: Some((3, 4)),
        };
        assert_eq!(
            diagnostic_field(FormKind::Effect, &spanned, fields.iter().copied()),
            None
        );
    }
}
