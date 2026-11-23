// SPDX-License-Identifier: MPL-2.0
//! `@EditCondition` (addendum § 7.5, P-119): a field greys out (or hides with
//! `@EditConditionHides`) while a condition over its sibling fields is false.
//!
//! Grammar (documented on `regia_ecs::attrs::EditCondition`): clauses joined by `&&`, each
//! `[!]path [op value]`, `op` one of `== != < <= > >=`, `value` a number, `true`/`false`, an enum
//! variant name or a `"string"`. A clause without `op` is truthy: a `true` bool, a `Some` option,
//! a non-zero number, a non-empty string. Conditions are evaluated against the struct that holds
//! the field, so `path` names a sibling (dots reach inside it).
//!
//! A condition that does not parse, or names a field that does not exist, evaluates to `None`:
//! the inspector then leaves the field editable (a typo never locks a field).

use regia_ecs::reflect::enums::Enum;
use regia_ecs::reflect::{PartialReflect, ReflectPath, ReflectRef};

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// The literal on the right of a comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A number (compared with any numeric field).
    Number(f64),
    /// `true` / `false`.
    Bool(bool),
    /// A bare word: an enum variant name (`Spot`, `None`, `Some`).
    Word(String),
    /// A quoted string.
    Text(String),
}

/// One clause: `[!]path [op literal]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    /// Leading `!`.
    pub negate: bool,
    /// Sibling field path (`cast_shadows`, `active_events.contact_force`).
    pub path: String,
    /// The comparison, if any (none = truthiness).
    pub compare: Option<(Op, Literal)>,
}

/// A parsed condition: every clause must hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// The clauses joined by `&&`.
    pub clauses: Vec<Clause>,
}

impl Condition {
    /// Parses `source`.
    ///
    /// # Errors
    /// A message naming what is wrong (empty clause, bad operator, bad literal).
    pub fn parse(source: &str) -> Result<Self, String> {
        let clauses = source
            .split("&&")
            .map(parse_clause)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { clauses })
    }

    /// Evaluates against `parent` (the struct holding the field). `None` when a path is missing
    /// or a comparison does not fit the field's type.
    #[must_use]
    pub fn eval(&self, parent: &dyn PartialReflect) -> Option<bool> {
        let mut all = true;
        for clause in &self.clauses {
            all &= eval_clause(clause, parent)?;
        }
        Some(all)
    }
}

/// Parses and evaluates `source` against `parent`; `None` (field stays editable) when it does not
/// parse or does not fit.
#[must_use]
pub fn evaluate(source: &str, parent: &dyn PartialReflect) -> Option<bool> {
    match Condition::parse(source) {
        Ok(condition) => condition.eval(parent),
        Err(err) => {
            tracing::debug!(target: "regia_editor", "@EditCondition({source:?}): {err}");
            None
        }
    }
}

fn parse_clause(text: &str) -> Result<Clause, String> {
    let text = text.trim();
    let (negate, text) = match text.strip_prefix('!') {
        Some(rest) => (true, rest.trim()),
        None => (false, text),
    };
    // Two-character operators first.
    const OPS: [(&str, Op); 6] = [
        ("==", Op::Eq),
        ("!=", Op::Ne),
        ("<=", Op::Le),
        (">=", Op::Ge),
        ("<", Op::Lt),
        (">", Op::Gt),
    ];
    let found = OPS
        .iter()
        .filter_map(|(token, op)| text.find(token).map(|at| (at, *token, *op)))
        .min_by_key(|(at, token, _)| (*at, usize::MAX - token.len()));
    let (path, compare) = match found {
        Some((at, token, op)) => {
            if negate {
                return Err("`!` only goes before a clause without a comparison".into());
            }
            let literal = parse_literal(text[at + token.len()..].trim())?;
            (text[..at].trim(), Some((op, literal)))
        }
        None => (text, None),
    };
    let valid = !path.is_empty()
        && path
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    if !valid {
        return Err(format!("`{path}` is not a field path"));
    }
    Ok(Clause {
        negate,
        path: path.to_owned(),
        compare,
    })
}

fn parse_literal(text: &str) -> Result<Literal, String> {
    if text.is_empty() {
        return Err("a comparison needs a value".into());
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        return Ok(Literal::Text(inner.to_owned()));
    }
    match text {
        "true" => return Ok(Literal::Bool(true)),
        "false" => return Ok(Literal::Bool(false)),
        _ => {}
    }
    if let Ok(number) = text.parse::<f64>() {
        return Ok(Literal::Number(number));
    }
    if text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Ok(Literal::Word(text.to_owned()));
    }
    Err(format!("`{text}` is not a value"))
}

/// A field's value as a number, when it is one.
fn number(value: &dyn PartialReflect) -> Option<f64> {
    if let Some(v) = value.try_downcast_ref::<f64>() {
        return Some(*v);
    }
    if let Some(v) = value.try_downcast_ref::<f32>() {
        return Some(f64::from(*v));
    }
    macro_rules! try_int {
        ($($t:ty),*) => {$(
            if let Some(v) = value.try_downcast_ref::<$t>() {
                // Precision past 2^53 does not matter for a condition.
                return Some(*v as f64);
            }
        )*};
    }
    try_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
    None
}

fn text(value: &dyn PartialReflect) -> Option<&str> {
    value.try_downcast_ref::<String>().map(String::as_str)
}

fn truthy(value: &dyn PartialReflect) -> Option<bool> {
    if let Some(on) = value.try_downcast_ref::<bool>() {
        return Some(*on);
    }
    if let Some(n) = number(value) {
        return Some(n != 0.0);
    }
    if let Some(s) = text(value) {
        return Some(!s.is_empty());
    }
    // Option: Some is true; other enums have no truth value.
    if let ReflectRef::Enum(e) = value.reflect_ref()
        && is_option(e)
    {
        return Some(e.variant_name() == "Some");
    }
    None
}

fn is_option(e: &dyn Enum) -> bool {
    e.reflect_type_path().starts_with("core::option::Option<")
}

fn eval_clause(clause: &Clause, parent: &dyn PartialReflect) -> Option<bool> {
    let value = clause.path.as_str().reflect_element(parent).ok()?;
    let result = match &clause.compare {
        None => truthy(value)?,
        Some((op, literal)) => compare(value, *op, literal)?,
    };
    Some(result != clause.negate)
}

fn compare(value: &dyn PartialReflect, op: Op, literal: &Literal) -> Option<bool> {
    let ordering = match literal {
        Literal::Number(rhs) => number(value)?.partial_cmp(rhs)?,
        Literal::Bool(rhs) => value.try_downcast_ref::<bool>()?.cmp(rhs),
        Literal::Text(rhs) => text(value)?.cmp(rhs.as_str()),
        Literal::Word(word) => {
            let ReflectRef::Enum(e) = value.reflect_ref() else {
                return None;
            };
            // Only equality makes sense for a variant name.
            let equal = e.variant_name() == word;
            return match op {
                Op::Eq => Some(equal),
                Op::Ne => Some(!equal),
                _ => None,
            };
        }
    };
    use std::cmp::Ordering::{Equal, Greater, Less};
    Some(match op {
        Op::Eq => ordering == Equal,
        Op::Ne => ordering != Equal,
        Op::Lt => ordering == Less,
        Op::Le => ordering != Greater,
        Op::Gt => ordering == Greater,
        Op::Ge => ordering != Less,
    })
}

/// How deep [`condition_states`] walks into nested structs.
const MAX_DEPTH: usize = 8;

/// One field's evaluated `@EditCondition` inside a component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldCondition {
    /// Reflect path of the field.
    pub path: String,
    /// The condition's text.
    pub source: &'static str,
    /// Whether it holds (a condition that does not parse holds).
    pub holds: bool,
    /// `@EditConditionHides`.
    pub hides: bool,
}

/// Every `@EditCondition` field of `value` (nested structs included), evaluated.
#[must_use]
pub fn condition_states(value: &dyn PartialReflect) -> Vec<FieldCondition> {
    let mut out = Vec::new();
    walk(value, "", 0, &mut |path, attrs, parent| {
        if let Some(source) = attrs.edit_condition {
            out.push(FieldCondition {
                path: path.to_owned(),
                source,
                holds: evaluate(source, parent).unwrap_or(true),
                hides: attrs.edit_condition_hides,
            });
        }
    });
    out
}

/// Reflect paths of the `@Hidden` fields of `value` (nested structs included).
#[must_use]
pub fn hidden_paths(value: &dyn PartialReflect) -> Vec<String> {
    let mut out = Vec::new();
    walk(value, "", 0, &mut |path, attrs, _| {
        if attrs.hidden {
            out.push(path.to_owned());
        }
    });
    out
}

/// Calls `visit(path, attrs, parent)` for every named struct field under `value`.
fn walk(
    value: &dyn PartialReflect,
    path: &str,
    depth: usize,
    visit: &mut dyn FnMut(&str, &regia_ecs::attrs::EditorAttrs, &dyn PartialReflect),
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let ReflectRef::Struct(fields) = value.reflect_ref() else {
        return;
    };
    let Some(regia_ecs::reflect::TypeInfo::Struct(info)) = value.get_represented_type_info() else {
        return;
    };
    for index in 0..fields.field_len() {
        let (Some(field_info), Some(field)) = (info.field_at(index), fields.field_at(index)) else {
            continue;
        };
        let attrs = regia_ecs::attrs::EditorAttrs::from_custom(field_info.custom_attributes());
        let child = if path.is_empty() {
            field_info.name().to_owned()
        } else {
            format!("{path}.{}", field_info.name())
        };
        visit(&child, &attrs, value);
        walk(field, &child, depth + 1, visit);
    }
}

/// Whether `path` is `parent` or a field inside it.
fn under(path: &str, parent: &str) -> bool {
    path == parent
        || (path.starts_with(parent) && matches!(path.as_bytes().get(parent.len()), Some(b'.' | b'[')))
}

/// Adds the conditional state to an `editor.inspect_selection` answer: a field whose
/// `@EditCondition` is false (on the primary entity, or the first holder) gets `enabled: false`
/// and `condition`; with `raw`, `@Hidden` fields get `hidden: true` and structural components
/// `structural: true` (the Inspector's raw view).
pub fn annotate_inspection(world: &regia_ecs::prelude::World, inspected: &mut serde_json::Value, raw: bool) {
    use serde_json::Value;
    let entities: Vec<regia_ecs::ecs::entity::Entity> = inspected
        .get("entities")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_u64)
                .filter_map(|bits| crate::command::resolve_entity(world, bits))
                .collect()
        })
        .unwrap_or_default();
    let primary = inspected
        .get("primary")
        .and_then(Value::as_u64)
        .and_then(|bits| crate::command::resolve_entity(world, bits));
    let Some(components) = inspected.get_mut("components").and_then(Value::as_array_mut) else {
        return;
    };
    for component in components {
        let Some(type_path) = component
            .get("type_path")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let value = primary
            .into_iter()
            .chain(entities.iter().copied())
            .find_map(|entity| crate::command::read_property(world, entity, &type_path, "").ok());
        let Some(value) = value else { continue };
        let blocked: Vec<FieldCondition> = condition_states(value.as_ref())
            .into_iter()
            .filter(|c| !c.holds)
            .collect();
        let hidden = if raw {
            hidden_paths(value.as_ref())
        } else {
            Vec::new()
        };
        if raw && super::is_structural(&type_path) {
            component["structural"] = Value::Bool(true);
        }
        let Some(fields) = component.get_mut("fields").and_then(Value::as_array_mut) else {
            continue;
        };
        for field in fields {
            let Some(path) = field.get("path").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            if let Some(condition) = blocked.iter().find(|c| under(&path, &c.path)) {
                field["enabled"] = Value::Bool(false);
                field["condition"] = Value::String(condition.source.to_owned());
            }
            if hidden.iter().any(|h| under(&path, h)) {
                field["hidden"] = Value::Bool(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::prelude::*;

    #[derive(Reflect, Default, Clone, PartialEq, Debug)]
    enum Kind {
        #[default]
        Point,
        Spot,
    }

    #[derive(Reflect, Default)]
    struct Events {
        contact_force: bool,
    }

    #[derive(Reflect, Default)]
    struct Light {
        kind: Kind,
        enabled: bool,
        weight: f32,
        lut: Option<String>,
        name: String,
        events: Events,
    }

    #[test]
    fn conditions_read_sibling_fields() {
        let mut light = Light {
            kind: Kind::Spot,
            enabled: true,
            weight: 0.5,
            ..Light::default()
        };
        let eval = |l: &Light, src: &str| evaluate(src, l);
        assert_eq!(eval(&light, "kind == Spot"), Some(true));
        assert_eq!(eval(&light, "kind != Spot"), Some(false));
        assert_eq!(eval(&light, "enabled"), Some(true));
        assert_eq!(eval(&light, "!enabled"), Some(false));
        assert_eq!(eval(&light, "enabled && weight > 0.25"), Some(true));
        assert_eq!(eval(&light, "weight >= 0.5 && weight <= 0.5"), Some(true));
        assert_eq!(eval(&light, "weight < 0.5"), Some(false));
        assert_eq!(eval(&light, "lut"), Some(false), "None is false");
        assert_eq!(eval(&light, "lut == None"), Some(true));
        assert_eq!(eval(&light, "name"), Some(false), "empty string is false");
        assert_eq!(eval(&light, "events.contact_force"), Some(false));
        light.events.contact_force = true;
        light.lut = Some("a.cube".into());
        light.name = "key".into();
        assert_eq!(eval(&light, "events.contact_force"), Some(true));
        assert_eq!(eval(&light, "lut"), Some(true));
        assert_eq!(eval(&light, "name == \"key\""), Some(true));
        assert_eq!(eval(&light, "enabled == false"), Some(false));
    }

    #[test]
    fn bad_conditions_never_grey_a_field() {
        let light = Light::default();
        for src in [
            "",
            "missing",
            "kind > Spot",
            "weight == Spot",
            "enabled ==",
            "a b",
            "!kind == Spot",
        ] {
            assert_eq!(evaluate(src, &light), None, "{src:?}");
        }
        assert!(Condition::parse("weight >= 1 && kind == Point").is_ok());
    }
}
