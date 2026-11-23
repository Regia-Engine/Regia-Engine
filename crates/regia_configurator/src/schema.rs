// SPDX-License-Identifier: MPL-2.0
//! A JSON Schema 2020-12 validator for the keywords the contract schemas use (ADR-2480).
//!
//! Written from the public specification (JSON Schema Core and Validation, draft 2020-12,
//! json-schema.org). It covers the applicator keywords (`$ref` to the same document, `allOf`,
//! `anyOf`, `oneOf`, `not`, `if`/`then`/`else`, `properties`, `patternProperties`,
//! `additionalProperties`, `propertyNames`, `dependentSchemas`, `prefixItems`, `items`,
//! `contains`) and every validation keyword (`type`, `enum`, `const`, the numeric, string, array
//! and object bounds, `required`, `dependentRequired`). `format` is an annotation, as the default
//! 2020-12 vocabulary says.
//!
//! A schema that needs a keyword this validator does not evaluate (`$dynamicRef`,
//! `unevaluatedProperties`, a remote `$ref`, ...) is refused when it is compiled, never silently
//! accepted: a contract test that passes because a keyword was skipped would be worse than a red
//! one (ADR-278). Unknown keywords that are not in the 2020-12 vocabularies are ignored, as the
//! specification asks.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{Map, Value};

/// Deepest schema/instance nesting evaluated; deeper input is reported, never recursed into.
pub const MAX_DEPTH: usize = 64;

/// Schema evaluations one [`Schema::validate`] call may spend at least, whatever the instance.
pub const MIN_EVAL_STEPS: u64 = 1_000_000;

/// Schema evaluations one [`Schema::validate`] call may spend per value of the instance, on top
/// of [`MIN_EVAL_STEPS`]. The depth limit alone does not bound the work: a schema whose `allOf`
/// names the same `$ref` twice doubles it at every level, about 2^32 evaluations of one `null`
/// within 64 levels. Past the budget the validation stops and reports `maxSteps`.
pub const EVAL_STEPS_PER_VALUE: u64 = 256;

/// The evaluation budget of the [`Schema::validate`] call running on this thread.
#[derive(Debug, Clone, Copy, Default)]
struct Budget {
    /// Evaluations left.
    left: u64,
    /// An evaluation was refused because none were left.
    exhausted: bool,
}

thread_local! {
    static BUDGET: std::cell::Cell<Budget> = const {
        std::cell::Cell::new(Budget { left: u64::MAX, exhausted: false })
    };
}

/// Spends one evaluation; `false` when the budget is used up.
fn spend() -> bool {
    BUDGET.with(|b| {
        let mut now = b.get();
        if now.left == 0 {
            now.exhausted = true;
            b.set(now);
            return false;
        }
        now.left -= 1;
        b.set(now);
        true
    })
}

/// How many JSON values `v` holds (itself included).
fn count_values(v: &Value) -> u64 {
    match v {
        Value::Array(a) => a.iter().fold(1u64, |n, x| n.saturating_add(count_values(x))),
        Value::Object(o) => o.values().fold(1u64, |n, x| n.saturating_add(count_values(x))),
        _ => 1,
    }
}

/// Keywords of the 2020-12 vocabularies this validator does not evaluate. A schema using one is
/// refused at compile time.
const UNSUPPORTED: &[&str] = &[
    "$dynamicRef",
    "$dynamicAnchor",
    "$recursiveRef",
    "$recursiveAnchor",
    "$anchor",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
];

/// Why a schema document cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// A keyword this validator does not evaluate, at a schema location (JSON pointer).
    Unsupported {
        /// The keyword.
        keyword: String,
        /// Where in the schema document.
        at: String,
    },
    /// A `$ref` that is not a fragment of the same document, or points nowhere.
    BadRef {
        /// The reference.
        reference: String,
        /// Where in the schema document.
        at: String,
    },
    /// A `pattern` / `patternProperties` key the regex engine does not accept.
    BadPattern {
        /// The pattern.
        pattern: String,
        /// The regex engine's message.
        message: String,
    },
    /// A keyword with a value of the wrong type (a `required` that is not an array of strings, ...).
    Malformed {
        /// The keyword.
        keyword: String,
        /// Where in the schema document.
        at: String,
    },
    /// `$schema` names a dialect other than 2020-12.
    Dialect(String),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { keyword, at } => {
                write!(f, "unsupported keyword `{keyword}` at {at}")
            }
            Self::BadRef { reference, at } => write!(f, "unresolvable $ref `{reference}` at {at}"),
            Self::BadPattern { pattern, message } => write!(f, "bad pattern `{pattern}`: {message}"),
            Self::Malformed { keyword, at } => write!(f, "malformed `{keyword}` at {at}"),
            Self::Dialect(d) => write!(f, "unsupported $schema dialect `{d}` (needs 2020-12)"),
        }
    }
}

impl std::error::Error for SchemaError {}

/// One way an instance fails a schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// JSON pointer of the failing value in the instance (`""` is the root).
    pub instance_path: String,
    /// The keyword that failed.
    pub keyword: &'static str,
    /// What was wrong, for a person.
    pub message: String,
}

/// A compiled schema: the document plus its compiled patterns.
#[derive(Debug, Clone)]
pub struct Schema {
    root: Value,
    patterns: HashMap<String, Regex>,
}

const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

impl Schema {
    /// Checks `doc` for keywords this validator cannot evaluate, resolves every `$ref` and
    /// compiles every pattern.
    pub fn compile(doc: Value) -> Result<Self, SchemaError> {
        if let Some(d) = doc.get("$schema") {
            let d = d.as_str().unwrap_or_default();
            if d.trim_end_matches('#') != DIALECT {
                return Err(SchemaError::Dialect(d.to_string()));
            }
        }
        let mut patterns = HashMap::new();
        let mut visited = std::collections::HashSet::new();
        walk(&doc, &doc, &mut String::new(), &mut patterns, &mut visited, 0)?;
        Ok(Self { root: doc, patterns })
    }

    /// Parses and compiles a schema from JSON text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let doc: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Self::compile(doc).map_err(|e| e.to_string())
    }

    /// The schema document.
    pub fn document(&self) -> &Value {
        &self.root
    }

    /// Every violation of `instance` (empty when it is valid).
    pub fn validate(&self, instance: &Value) -> Vec<Violation> {
        let mut out = Vec::new();
        let mut path = String::new();
        let left = MIN_EVAL_STEPS.saturating_add(EVAL_STEPS_PER_VALUE.saturating_mul(count_values(instance)));
        let outer = BUDGET.with(|b| {
            b.replace(Budget {
                left,
                exhausted: false,
            })
        });
        self.eval(&self.root, instance, &mut path, &mut out, 0);
        let mine = BUDGET.with(|b| b.replace(outer));
        if mine.exhausted {
            // Results computed after the budget ran out are not trustworthy (a skipped `not` reads
            // as a match), so the instance is never reported valid.
            push(
                &mut out,
                "",
                "maxSteps",
                format!("the schema needs more than {left} evaluations for this value"),
            );
        }
        out
    }

    /// `true` when `instance` is valid.
    pub fn is_valid(&self, instance: &Value) -> bool {
        self.validate(instance).is_empty()
    }

    fn resolve(&self, reference: &str) -> Option<&Value> {
        let pointer = reference.strip_prefix('#')?;
        self.root.pointer(&unescape_fragment(pointer))
    }

    fn eval(&self, schema: &Value, inst: &Value, path: &mut String, out: &mut Vec<Violation>, depth: usize) {
        if depth > MAX_DEPTH {
            push(
                out,
                path,
                "maxDepth",
                format!("nested deeper than {MAX_DEPTH} levels"),
            );
            return;
        }
        if !spend() {
            return;
        }
        let obj = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                push(out, path, "false", "no value is allowed here".to_string());
                return;
            }
            Value::Object(o) => o,
            _ => return,
        };
        if let Some(r) = obj.get("$ref").and_then(Value::as_str)
            && let Some(target) = self.resolve(r)
        {
            self.eval(target, inst, path, out, depth + 1);
        }
        if let Some(t) = obj.get("type") {
            check_type(t, inst, path, out);
        }
        if let Some(e) = obj.get("enum").and_then(Value::as_array)
            && !e.iter().any(|v| json_eq(v, inst))
        {
            push(
                out,
                path,
                "enum",
                format!("{} is not one of {}", short(inst), short_list(e)),
            );
        }
        if let Some(c) = obj.get("const")
            && !json_eq(c, inst)
        {
            push(out, path, "const", format!("{} is not {}", short(inst), short(c)));
        }
        match inst {
            Value::Number(n) => self.eval_number(obj, n, path, out),
            Value::String(s) => self.eval_string(obj, s, path, out),
            Value::Array(a) => self.eval_array(obj, a, path, out, depth),
            Value::Object(o) => self.eval_object(obj, o, path, out, depth),
            _ => {}
        }
        self.eval_combinators(obj, inst, path, out, depth);
    }

    fn eval_combinators(
        &self,
        obj: &Map<String, Value>,
        inst: &Value,
        path: &mut String,
        out: &mut Vec<Violation>,
        depth: usize,
    ) {
        if let Some(all) = obj.get("allOf").and_then(Value::as_array) {
            for s in all {
                self.eval(s, inst, path, out, depth + 1);
            }
        }
        if let Some(any) = obj.get("anyOf").and_then(Value::as_array) {
            let results: Vec<Vec<Violation>> = any.iter().map(|s| self.sub(s, inst, path, depth)).collect();
            if !results.iter().any(Vec::is_empty) {
                out.extend(closest(results));
            }
        }
        if let Some(one) = obj.get("oneOf").and_then(Value::as_array) {
            let results: Vec<Vec<Violation>> = one.iter().map(|s| self.sub(s, inst, path, depth)).collect();
            let passing = results.iter().filter(|r| r.is_empty()).count();
            match passing {
                1 => {}
                0 => out.extend(closest(results)),
                n => push(
                    out,
                    path,
                    "oneOf",
                    format!("matches {n} alternatives, exactly one is allowed"),
                ),
            }
        }
        if let Some(not) = obj.get("not")
            && self.sub(not, inst, path, depth).is_empty()
        {
            push(
                out,
                path,
                "not",
                format!("{} matches a forbidden shape", short(inst)),
            );
        }
        if let Some(cond) = obj.get("if") {
            let branch = if self.sub(cond, inst, path, depth).is_empty() {
                obj.get("then")
            } else {
                obj.get("else")
            };
            if let Some(b) = branch {
                self.eval(b, inst, path, out, depth + 1);
            }
        }
    }

    fn sub(&self, schema: &Value, inst: &Value, path: &mut String, depth: usize) -> Vec<Violation> {
        let mut v = Vec::new();
        self.eval(schema, inst, path, &mut v, depth + 1);
        v
    }

    fn eval_number(
        &self,
        obj: &Map<String, Value>,
        n: &serde_json::Number,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let x = n.as_f64().unwrap_or(f64::NAN);
        // Each bound against the value: exact when both are integers (an f64 reading rounds past
        // 2^53, so `9007199254740993` passed `maximum: 9007199254740992`), else as f64.
        let bound = |k: &str| match obj.get(k) {
            Some(Value::Number(m)) => Some((m, num_cmp(n, m))),
            _ => None,
        };
        if let Some((m, Some(Less))) = bound("minimum") {
            push(out, path, "minimum", format!("{n} is below the minimum {m}"));
        }
        if let Some((m, Some(Greater))) = bound("maximum") {
            push(out, path, "maximum", format!("{n} is above the maximum {m}"));
        }
        if let Some((m, Some(Less | Equal))) = bound("exclusiveMinimum") {
            push(
                out,
                path,
                "exclusiveMinimum",
                format!("{n} must be greater than {m}"),
            );
        }
        if let Some((m, Some(Greater | Equal))) = bound("exclusiveMaximum") {
            push(
                out,
                path,
                "exclusiveMaximum",
                format!("{n} must be less than {m}"),
            );
        }
        let bound = |k: &str| obj.get(k).and_then(Value::as_f64);
        if let Some(m) = bound("multipleOf")
            && m > 0.0
            && !is_multiple(n, obj.get("multipleOf"), x, m)
        {
            push(out, path, "multipleOf", format!("{x} is not a multiple of {m}"));
        }
    }

    fn eval_string(&self, obj: &Map<String, Value>, s: &str, path: &str, out: &mut Vec<Violation>) {
        let chars = s.chars().count();
        if let Some(m) = obj.get("minLength").and_then(count)
            && (chars as u64) < m
        {
            push(out, path, "minLength", format!("shorter than {m} characters"));
        }
        if let Some(m) = obj.get("maxLength").and_then(count)
            && (chars as u64) > m
        {
            push(out, path, "maxLength", format!("longer than {m} characters"));
        }
        if let Some(p) = obj.get("pattern").and_then(Value::as_str)
            && let Some(re) = self.patterns.get(p)
            && !re.is_match(s)
        {
            push(
                out,
                path,
                "pattern",
                format!("{} does not match `{p}`", short_str(s)),
            );
        }
    }

    fn eval_array(
        &self,
        obj: &Map<String, Value>,
        a: &[Value],
        path: &mut String,
        out: &mut Vec<Violation>,
        depth: usize,
    ) {
        let len = a.len() as u64;
        if let Some(m) = obj.get("minItems").and_then(count)
            && len < m
        {
            push(out, path, "minItems", format!("{len} items, at least {m} needed"));
        }
        if let Some(m) = obj.get("maxItems").and_then(count)
            && len > m
        {
            push(out, path, "maxItems", format!("{len} items, at most {m} allowed"));
        }
        if obj.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            // One canonical key per item in a hash set: a pairwise scan was O(items²) and a
            // million-item array stalled the validation.
            let mut seen = std::collections::HashSet::with_capacity(a.len());
            if let Some(x) = a.iter().find(|x| !seen.insert(unique_key(x))) {
                push(out, path, "uniqueItems", format!("{} appears twice", short(x)));
            }
        }
        let prefix = obj.get("prefixItems").and_then(Value::as_array);
        let start = prefix.map_or(0, Vec::len);
        if let Some(prefix) = prefix {
            for (i, (s, v)) in prefix.iter().zip(a).enumerate() {
                let keep = enter(path, &i.to_string());
                self.eval(s, v, path, out, depth + 1);
                path.truncate(keep);
            }
        }
        if let Some(items) = obj.get("items") {
            for (i, v) in a.iter().enumerate().skip(start) {
                let keep = enter(path, &i.to_string());
                self.eval(items, v, path, out, depth + 1);
                path.truncate(keep);
            }
        }
        if let Some(c) = obj.get("contains") {
            let hits = a
                .iter()
                .filter(|v| self.sub(c, v, path, depth).is_empty())
                .count() as u64;
            let min = obj.get("minContains").and_then(count).unwrap_or(1);
            let max = obj.get("maxContains").and_then(count);
            if hits < min {
                push(
                    out,
                    path,
                    "contains",
                    format!("{hits} items match `contains`, at least {min} needed"),
                );
            }
            if let Some(max) = max
                && hits > max
            {
                push(
                    out,
                    path,
                    "maxContains",
                    format!("{hits} items match `contains`, at most {max} allowed"),
                );
            }
        }
    }

    fn eval_object(
        &self,
        obj: &Map<String, Value>,
        o: &Map<String, Value>,
        path: &mut String,
        out: &mut Vec<Violation>,
        depth: usize,
    ) {
        let n = o.len() as u64;
        if let Some(m) = obj.get("minProperties").and_then(count)
            && n < m
        {
            push(
                out,
                path,
                "minProperties",
                format!("{n} properties, at least {m} needed"),
            );
        }
        if let Some(m) = obj.get("maxProperties").and_then(count)
            && n > m
        {
            push(
                out,
                path,
                "maxProperties",
                format!("{n} properties, at most {m} allowed"),
            );
        }
        if let Some(req) = obj.get("required").and_then(Value::as_array) {
            for k in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(k) {
                    let keep = enter(path, k);
                    push(out, path, "required", format!("`{k}` is required"));
                    path.truncate(keep);
                }
            }
        }
        if let Some(dep) = obj.get("dependentRequired").and_then(Value::as_object) {
            for (k, needs) in dep {
                if o.contains_key(k) {
                    for need in needs.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        if !o.contains_key(need) {
                            let keep = enter(path, need);
                            push(
                                out,
                                path,
                                "dependentRequired",
                                format!("`{need}` is required with `{k}`"),
                            );
                            path.truncate(keep);
                        }
                    }
                }
            }
        }
        if let Some(dep) = obj.get("dependentSchemas").and_then(Value::as_object) {
            let inst = Value::Object(o.clone());
            for (k, s) in dep {
                if o.contains_key(k) {
                    self.eval(s, &inst, path, out, depth + 1);
                }
            }
        }
        let props = obj.get("properties").and_then(Value::as_object);
        let pattern_props = obj.get("patternProperties").and_then(Value::as_object);
        for (k, v) in o {
            if let Some(names) = obj.get("propertyNames") {
                let key = Value::String(k.clone());
                let keep = enter(path, k);
                let errs = self.sub(names, &key, path, depth);
                if !errs.is_empty() {
                    push(
                        out,
                        path,
                        "propertyNames",
                        format!("property name `{}` is not allowed", short_str(k)),
                    );
                }
                path.truncate(keep);
            }
            let mut matched = false;
            if let Some(s) = props.and_then(|p| p.get(k)) {
                matched = true;
                let keep = enter(path, k);
                self.eval(s, v, path, out, depth + 1);
                path.truncate(keep);
            }
            for (p, s) in pattern_props.into_iter().flatten() {
                if self.patterns.get(p).is_some_and(|re| re.is_match(k)) {
                    matched = true;
                    let keep = enter(path, k);
                    self.eval(s, v, path, out, depth + 1);
                    path.truncate(keep);
                }
            }
            if !matched && let Some(extra) = obj.get("additionalProperties") {
                let keep = enter(path, k);
                if extra == &Value::Bool(false) {
                    push(
                        out,
                        path,
                        "additionalProperties",
                        format!("`{}` is not a known field", short_str(k)),
                    );
                } else {
                    self.eval(extra, v, path, out, depth + 1);
                }
                path.truncate(keep);
            }
        }
    }
}

/// Walks a schema document: refuses unsupported keywords, checks `$ref`s and compiles patterns.
///
/// A `$ref` target is walked too (once per target): a `$ref` may point anywhere in the document,
/// and a target outside the keywords walked here would keep its pattern uncompiled (so it never
/// failed), its unsupported keywords unrefused and its own broken `$ref` silently ignored.
fn walk(
    root: &Value,
    node: &Value,
    at: &mut String,
    patterns: &mut HashMap<String, Regex>,
    visited: &mut std::collections::HashSet<String>,
    depth: usize,
) -> Result<(), SchemaError> {
    if depth > MAX_DEPTH * 2 {
        return Err(SchemaError::Malformed {
            keyword: "nesting".into(),
            at: at.clone(),
        });
    }
    let Value::Object(obj) = node else {
        return Ok(());
    };
    for (k, v) in obj {
        if UNSUPPORTED.contains(&k.as_str()) {
            return Err(SchemaError::Unsupported {
                keyword: k.clone(),
                at: at.clone(),
            });
        }
        let malformed = || SchemaError::Malformed {
            keyword: k.clone(),
            at: at.clone(),
        };
        match k.as_str() {
            // An embedded `$id` starts a resource whose fragment `$ref`s resolve against it; this
            // validator resolves every `$ref` against the root, so it would pick wrong targets.
            "$id" if !std::ptr::eq(node, root) => {
                return Err(SchemaError::Unsupported {
                    keyword: k.clone(),
                    at: at.clone(),
                });
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties" | "maxProperties"
            | "minContains" | "maxContains"
                if count(v).is_none() =>
            {
                return Err(malformed());
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" if !v.is_number() => {
                return Err(malformed());
            }
            "multipleOf" if !v.as_f64().is_some_and(|m| m > 0.0) => return Err(malformed()),
            "uniqueItems" if !v.is_boolean() => return Err(malformed()),
            "$ref" => {
                let r = v.as_str().ok_or_else(malformed)?;
                let pointer = r.strip_prefix('#').map(unescape_fragment);
                let Some((pointer, target)) = pointer.and_then(|p| Some((p.clone(), root.pointer(&p)?)))
                else {
                    return Err(SchemaError::BadRef {
                        reference: r.to_string(),
                        at: at.clone(),
                    });
                };
                if visited.insert(pointer.clone()) {
                    let mut target_at = pointer;
                    walk(root, target, &mut target_at, patterns, visited, depth + 1)?;
                }
            }
            "pattern" => {
                let p = v.as_str().ok_or_else(malformed)?;
                compile_pattern(p, patterns)?;
            }
            "required" => {
                if !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                    return Err(malformed());
                }
            }
            "type" => {
                let ok = match v {
                    Value::String(s) => is_type_name(s),
                    Value::Array(a) => a.iter().all(|t| t.as_str().is_some_and(is_type_name)),
                    _ => false,
                };
                if !ok {
                    return Err(malformed());
                }
            }
            "enum" if !v.is_array() => return Err(malformed()),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                let a = v.as_array().ok_or_else(malformed)?;
                for (i, s) in a.iter().enumerate() {
                    let keep = enter(at, k);
                    enter(at, &i.to_string());
                    walk(root, s, at, patterns, visited, depth + 1)?;
                    at.truncate(keep);
                }
            }
            "not"
            | "if"
            | "then"
            | "else"
            | "items"
            | "contains"
            | "additionalProperties"
            | "propertyNames" => {
                // Each of these is one schema. An array (draft-04 tuple `items`) is not a schema
                // this validator evaluates; accepting it used to skip the keyword and let every
                // element through.
                if !v.is_object() && !v.is_boolean() {
                    return Err(malformed());
                }
                let keep = enter(at, k);
                walk(root, v, at, patterns, visited, depth + 1)?;
                at.truncate(keep);
            }
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                let o = v.as_object().ok_or_else(malformed)?;
                for (name, s) in o {
                    if k == "patternProperties" {
                        compile_pattern(name, patterns)?;
                    }
                    let keep = enter(at, k);
                    enter(at, name);
                    walk(root, s, at, patterns, visited, depth + 1)?;
                    at.truncate(keep);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `n` against the bound `m`: exact for two integers, else as f64 (`None` when either is NaN).
fn num_cmp(n: &serde_json::Number, m: &serde_json::Number) -> Option<std::cmp::Ordering> {
    let int = |v: &serde_json::Number| v.as_i64().map(i128::from).or_else(|| v.as_u64().map(i128::from));
    match (int(n), int(m)) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        _ => n.as_f64()?.partial_cmp(&m.as_f64()?),
    }
}

/// The value of a count keyword (`minLength`, `maxItems`, ...): a non-negative integer, which in
/// 2020-12 includes a float with a zero fractional part (`2.0`). `None` for anything else.
fn count(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            let f = n.as_f64()?;
            // 2^64 as f64; below it every integral f64 converts exactly.
            (f >= 0.0 && f.fract() == 0.0 && f < 18_446_744_073_709_551_616.0).then_some(f as u64)
        }),
        _ => None,
    }
}

/// `multipleOf`: exact for two integers; for floats the quotient must be within a few units in the
/// last place of a whole number. A tolerance relative to the quotient alone (`1e-9 * q`) reached
/// 0.5 at q = 5e8, so `1000000001` passed as a multiple of 2.
fn is_multiple(n: &serde_json::Number, divisor: Option<&Value>, x: f64, m: f64) -> bool {
    let int = |v: &serde_json::Number| v.as_i64().map(i128::from).or_else(|| v.as_u64().map(i128::from));
    if let (Some(a), Some(Value::Number(d))) = (int(n), divisor)
        && let Some(b) = int(d)
        && b > 0
    {
        return a % b == 0;
    }
    let q = x / m;
    (q - q.round()).abs() <= 1e-9_f64.max(q.abs() * 4.0 * f64::EPSILON)
}

fn compile_pattern(p: &str, patterns: &mut HashMap<String, Regex>) -> Result<(), SchemaError> {
    if patterns.contains_key(p) {
        return Ok(());
    }
    let re = Regex::new(&ecma_pattern(p)).map_err(|e| SchemaError::BadPattern {
        pattern: p.to_string(),
        message: e.to_string(),
    })?;
    patterns.insert(p.to_string(), re);
    Ok(())
}

/// An ECMA-262 pattern (the dialect JSON Schema names) in the `regex` crate's syntax, for the
/// constructs whose meaning differs: `.` matches no line terminator (`\n`, `\r`, U+2028, U+2029;
/// `regex` excludes only `\n`), and `\d`, `\D`, `\w`, `\W` are ASCII classes (`regex` makes
/// them Unicode). Inside a character class a `.` is literal and a `[` is a literal bracket.
fn ecma_pattern(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 16);
    let mut chars = p.chars();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let ascii = match chars.next() {
                    Some('d') => "digit",
                    Some('D') => "^digit",
                    Some('w') => "word",
                    Some('W') => "^word",
                    Some(e) => {
                        out.push('\\');
                        out.push(e);
                        continue;
                    }
                    None => {
                        out.push('\\');
                        continue;
                    }
                };
                if in_class {
                    out.push_str(&format!("[:{ascii}:]"));
                } else {
                    out.push_str(&format!("[[:{ascii}:]]"));
                }
            }
            '[' if in_class => out.push_str("\\["),
            '[' => {
                in_class = true;
                out.push('[');
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            '.' if !in_class => out.push_str("[^\\n\\r\\x{2028}\\x{2029}]"),
            c => out.push(c),
        }
    }
    out
}

fn is_type_name(s: &str) -> bool {
    matches!(
        s,
        "null" | "boolean" | "object" | "array" | "number" | "string" | "integer"
    )
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        // 2020-12: any number with a zero fractional part is an integer (1.0 included).
        "integer" => match v {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            }
            _ => false,
        },
        _ => false,
    }
}

fn check_type(t: &Value, inst: &Value, path: &str, out: &mut Vec<Violation>) {
    let ok = match t {
        Value::String(s) => type_matches(s, inst),
        Value::Array(a) => a.iter().filter_map(Value::as_str).any(|s| type_matches(s, inst)),
        _ => true,
    };
    if !ok {
        push(
            out,
            path,
            "type",
            format!("{} is not of type {}", type_name(inst), short(t)),
        );
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// JSON equality with numbers compared by value (`1` equals `1.0`), as JSON Schema requires.
pub fn json_eq(a: &Value, b: &Value) -> bool {
    #[cfg(test)]
    JSON_EQ_CALLS.with(|n| n.set(n.get() + 1));
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => match (x.as_u64(), y.as_u64()) {
                (Some(i), Some(j)) => i == j,
                _ => x.as_f64() == y.as_f64(),
            },
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

#[cfg(test)]
thread_local! {
    /// `json_eq` calls, for the test that pins `uniqueItems` as linear.
    static JSON_EQ_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The `uniqueItems` key of a value: equal keys for values equal as numbers, strings, arrays and
/// objects (keys sorted). A number counts by its mathematical value: an integral one (`1`, `1.0`)
/// as its integer, any other one as its shortest round-trip decimal, so two keys are equal exactly
/// when the values are (as the reference validator compares them). Values equal by [`json_eq`]
/// have one key, except an integer and a float above 2^53 that only round to each other.
fn unique_key(value: &Value) -> String {
    fn put(value: &Value, out: &mut String) {
        match value {
            Value::Number(n) => {
                out.push('n');
                if let Some(i) = n.as_i64() {
                    out.push_str(&i.to_string());
                } else if let Some(u) = n.as_u64() {
                    out.push_str(&u.to_string());
                } else if let Some(f) = n.as_f64() {
                    if f.fract() == 0.0 && f.abs() < 1e38 {
                        out.push_str(&(f as i128).to_string());
                    } else {
                        out.push_str(&format!("{f:?}"));
                    }
                }
            }
            Value::Array(items) => {
                out.push('[');
                for item in items {
                    put(item, out);
                    out.push(',');
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by_key(|(k, _)| *k);
                out.push('{');
                for (k, v) in entries {
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    put(v, out);
                    out.push(',');
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    put(value, &mut out);
    out
}

/// Of several failing alternatives, the errors of the one that came closest, so an `anyOf`/`oneOf`
/// failure names the real problem rather than "matches nothing": fewest violations, and on a tie
/// the alternative of the value's own type (a wrong field in an object says so, not "not a string").
fn closest(results: Vec<Vec<Violation>>) -> Vec<Violation> {
    results
        .into_iter()
        .min_by_key(|r| (r.len(), r.iter().any(|v| v.keyword == "type")))
        .unwrap_or_default()
}

fn push(out: &mut Vec<Violation>, path: &str, keyword: &'static str, message: String) {
    out.push(Violation {
        instance_path: path.to_string(),
        keyword,
        message,
    });
}

/// Appends one JSON-pointer segment to `path`; returns the length to truncate back to.
fn enter(path: &mut String, segment: &str) -> usize {
    let keep = path.len();
    path.push('/');
    for c in segment.chars() {
        match c {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            c => path.push(c),
        }
    }
    keep
}

/// A URI fragment (`/$defs/x`) as a JSON pointer: every `%XX` escape decoded once (RFC 3986), so
/// `%2522` is the text `%22`, not a quote. A `%` not followed by two hex digits stays as it is.
fn unescape_fragment(fragment: &str) -> String {
    let bytes = fragment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    short_str(&s)
}

fn short_str(s: &str) -> String {
    if s.chars().count() > 80 {
        let head: String = s.chars().take(77).collect();
        format!("{head}...")
    } else {
        s.to_string()
    }
}

fn short_list(values: &[Value]) -> String {
    let all: Vec<String> = values.iter().map(Value::to_string).collect();
    short_str(&all.join(", "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema(v: Value) -> Schema {
        match Schema::compile(v) {
            Ok(s) => s,
            Err(e) => panic!("schema: {e}"),
        }
    }

    #[test]
    fn types_and_integer_rule() {
        let s = schema(json!({"type": "integer"}));
        assert!(s.is_valid(&json!(3)));
        assert!(s.is_valid(&json!(3.0)), "2020-12: 3.0 is an integer");
        assert!(!s.is_valid(&json!(3.5)));
        assert!(!s.is_valid(&json!("3")));
        let s = schema(json!({"type": ["string", "null"]}));
        assert!(s.is_valid(&json!(null)) && s.is_valid(&json!("a")) && !s.is_valid(&json!(1)));
    }

    #[test]
    fn objects_required_and_additional() {
        let s = schema(json!({
            "type": "object", "additionalProperties": false,
            "properties": {"a": {"type": "number", "minimum": 0}}, "required": ["a"]
        }));
        assert!(s.is_valid(&json!({"a": 1})));
        let v = s.validate(&json!({"b": 1}));
        let kinds: Vec<&str> = v.iter().map(|x| x.keyword).collect();
        assert_eq!(kinds, ["required", "additionalProperties"]);
        assert_eq!(v[0].instance_path, "/a");
        assert_eq!(v[1].instance_path, "/b");
        let v = s.validate(&json!({"a": -1}));
        assert_eq!(v[0].keyword, "minimum");
        assert_eq!(v[0].instance_path, "/a");
    }

    #[test]
    fn arrays_strings_and_refs() {
        let s = schema(json!({
            "$defs": {"v": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2}},
            "type": "object",
            "properties": {
                "p": {"$ref": "#/$defs/v"},
                "s": {"type": "string", "pattern": "^[a-z]+$", "maxLength": 3},
                "u": {"type": "array", "uniqueItems": true},
                "t": {"type": "array", "prefixItems": [{"const": "x"}], "items": {"type": "integer"}}
            }
        }));
        assert!(s.is_valid(&json!({"p": [1, 2], "s": "ab", "u": [1, 2], "t": ["x", 1, 2]})));
        assert_eq!(s.validate(&json!({"p": [1]}))[0].keyword, "minItems");
        assert_eq!(s.validate(&json!({"p": [1, "a"]}))[0].instance_path, "/p/1");
        assert_eq!(s.validate(&json!({"s": "aB"}))[0].keyword, "pattern");
        assert_eq!(s.validate(&json!({"s": "abcd"}))[0].keyword, "maxLength");
        assert_eq!(s.validate(&json!({"u": [1, 1.0]}))[0].keyword, "uniqueItems");
        assert_eq!(s.validate(&json!({"t": ["y", 1]}))[0].keyword, "const");
        assert_eq!(s.validate(&json!({"t": ["x", 1.5]}))[0].instance_path, "/t/1");
    }

    #[test]
    fn combinators_report_the_closest_branch() {
        let s = schema(json!({
            "oneOf": [
                {"type": "object", "properties": {"type": {"const": "a"}, "x": {"type": "number"}},
                 "required": ["type", "x"]},
                {"type": "object", "properties": {"type": {"const": "b"}, "y": {"type": "string"}},
                 "required": ["type", "y"]}
            ]
        }));
        assert!(s.is_valid(&json!({"type": "a", "x": 1})));
        let v = s.validate(&json!({"type": "b", "y": 3}));
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].instance_path, "/y");
        let s = schema(json!({"not": {"required": ["cost"]}}));
        assert!(!s.is_valid(&json!({"cost": 1})));
        let s = schema(
            json!({"if": {"properties": {"k": {"const": 1}}}, "then": {"required": ["a"]},
                              "else": {"required": ["b"]}}),
        );
        assert!(s.is_valid(&json!({"k": 1, "a": 0})) && !s.is_valid(&json!({"k": 1, "b": 0})));
        assert!(s.is_valid(&json!({"k": 2, "b": 0})));
    }

    /// Draft-04 tuple `items` is an array of schemas. This validator does not evaluate that form;
    /// compiling it used to succeed and then skip the keyword, so every element passed.
    #[test]
    fn items_as_an_array_is_refused() {
        let err = Schema::compile(json!({"type": "array", "items": [{"type": "string"}]}));
        assert!(
            matches!(err, Err(SchemaError::Malformed { ref keyword, .. }) if keyword == "items"),
            "{err:?}"
        );
    }

    /// `items: false` is a boolean schema and still rejects elements past `prefixItems`.
    #[test]
    fn items_false_still_rejects_extra_elements() {
        let s = schema(json!({
            "type": "array",
            "prefixItems": [{"type": "string"}],
            "items": false
        }));
        assert!(s.is_valid(&json!(["a"])));
        assert!(!s.is_valid(&json!(["a", 1])));
        assert!(!s.is_valid(&json!(["a", "b"])));
    }

    #[test]
    fn unsupported_and_bad_schemas_are_refused() {
        assert!(matches!(
            Schema::compile(json!({"unevaluatedProperties": false})),
            Err(SchemaError::Unsupported { .. })
        ));
        assert!(matches!(
            Schema::compile(json!({"properties": {"a": {"$ref": "other.json#/x"}}})),
            Err(SchemaError::BadRef { .. })
        ));
        assert!(matches!(
            Schema::compile(json!({"$ref": "#/$defs/missing"})),
            Err(SchemaError::BadRef { .. })
        ));
        assert!(matches!(
            Schema::compile(json!({"pattern": "("})),
            Err(SchemaError::BadPattern { .. })
        ));
        assert!(matches!(
            Schema::compile(json!({"$schema": "http://json-schema.org/draft-07/schema#"})),
            Err(SchemaError::Dialect(_))
        ));
        // Unknown non-vocabulary keywords are ignored, as 2020-12 says.
        assert!(Schema::compile(json!({"x-note": 1, "format": "date"})).is_ok());
    }

    /// corr-agent (ADR-5597): branches that `$ref` back to the root made validation exponential
    /// (3 ^ 33 evaluations here, a hung world thread for `configurator.contract_check`). The
    /// budget stops it and the instance is reported invalid. integ-v048e (ADR-5922): one budget
    /// (corr-configurator's, keyword `maxSteps`) replaces ADR-5597's fixed `maxWork` budget.
    #[test]
    fn self_referencing_branches_stop_at_the_work_budget() {
        for keyword in ["anyOf", "allOf"] {
            let s = schema(json!({ keyword: [{"$ref": "#"}, {"$ref": "#"}, {"$ref": "#"}] }));
            let errs = s.validate(&json!({"a": 1}));
            assert!(
                errs.iter().any(|e| e.keyword == "maxSteps"),
                "{keyword}: {errs:?}"
            );
        }
        // `not` of an exhausted branch must not turn into a pass.
        let s = schema(json!({"not": {"anyOf": [{"$ref": "#"}, {"$ref": "#"}, {"$ref": "#"}]}}));
        assert!(!s.is_valid(&json!(1)));
        // The budget is per call: a small schema still validates afterwards.
        assert!(schema(json!({"type": "integer"})).is_valid(&json!(1)));
    }

    #[test]
    fn deep_instances_stop_at_the_depth_limit() {
        let s = schema(
            json!({"$defs": {"n": {"type": "array", "items": {"$ref": "#/$defs/n"}}},
                              "$ref": "#/$defs/n"}),
        );
        let mut v = json!([]);
        for _ in 0..200 {
            v = json!([v]);
        }
        let errs = s.validate(&v);
        assert!(errs.iter().any(|e| e.keyword == "maxDepth"));
    }

    /// Audit fix (sprint-fix-audit-jw, finding 8): `uniqueItems` finds a repeat by hashing, not by
    /// comparing every pair, and still finds exactly the repeats the pairwise check found.
    #[test]
    fn unique_items_is_linear_and_finds_the_same_repeats() {
        let s = schema(json!({"type": "array", "uniqueItems": true}));
        let distinct: Vec<Value> = (0..5000).map(|i| json!({"k": i, "s": format!("v{i}")})).collect();
        JSON_EQ_CALLS.with(|n| n.set(0));
        assert!(s.is_valid(&Value::Array(distinct.clone())));
        assert_eq!(
            JSON_EQ_CALLS.with(std::cell::Cell::get),
            0,
            "no pairwise comparison"
        );
        let mut repeated = distinct;
        repeated.push(json!({"s": "v17", "k": 17.0}));
        assert!(
            !s.is_valid(&Value::Array(repeated)),
            "key order and 17 vs 17.0 do not matter"
        );
        for (a, b, same) in [
            (json!(1), json!(1.0), true),
            (json!(0.0), json!(-0.0), true),
            (json!(0.1), json!(0.1), true),
            (json!(0.1), json!(0.2), false),
            (json!("1"), json!(1), false),
            (json!([1, [2]]), json!([1.0, [2.0]]), true),
            (json!([1, 2]), json!([2, 1]), false),
            (json!({"a": null}), json!({"a": false}), false),
            (json!(u64::MAX), json!(u64::MAX), true),
            (json!(-5), json!(-5.0), true),
        ] {
            assert_eq!(s.is_valid(&json!([a.clone(), b.clone()])), !same, "{a} {b}");
            assert_eq!(json_eq(&a, &b), same, "{a} {b}: the pairwise rule agrees");
        }
    }

    /// Audit (sprint-corr-configurator): an `allOf` naming the same `$ref` twice doubles the work
    /// at every level, about 2^32 evaluations of one `null` inside the depth limit, so an agent
    /// call with such a schema never returned. The budget stops it and reports `maxSteps`.
    #[test]
    fn a_doubling_ref_schema_stops_at_the_eval_budget() {
        let s = schema(json!({
            "$defs": {"a": {"allOf": [{"$ref": "#/$defs/a"}, {"$ref": "#/$defs/a"}]}},
            "$ref": "#/$defs/a"
        }));
        let errs = s.validate(&Value::Null);
        assert!(errs.iter().any(|e| e.keyword == "maxSteps"), "{errs:?}");
        // The budget is per call: an ordinary schema on this thread is unaffected afterwards.
        let plain = schema(json!({"type": "array", "items": {"type": "integer"}}));
        let big: Vec<Value> = (0..20_000).map(|i| json!(i)).collect();
        assert!(plain.is_valid(&Value::Array(big)));
        // A chain of distinct definitions doubles just the same.
        let mut defs = serde_json::Map::new();
        for i in 0..40 {
            let next = json!({"$ref": format!("#/$defs/d{}", i + 1)});
            defs.insert(format!("d{i}"), json!({"allOf": [next.clone(), next]}));
        }
        defs.insert("d40".into(), json!({"type": "null"}));
        let chain = schema(json!({"$defs": defs, "$ref": "#/$defs/d0"}));
        assert!(!chain.is_valid(&Value::Null), "stopped, so not reported valid");
    }

    /// Audit (sprint-corr-configurator): a `$ref` may point outside the keywords `walk` visits; its
    /// target's pattern was never compiled (so it never failed) and its unsupported keywords and
    /// broken `$ref`s were never refused.
    #[test]
    fn a_ref_target_outside_the_walked_keywords_is_compiled_and_checked() {
        let s = schema(json!({"x-lib": {"s": {"type": "string", "pattern": "^a$"}}, "$ref": "#/x-lib/s"}));
        assert!(s.is_valid(&json!("a")));
        assert!(!s.is_valid(&json!("b")), "the target's pattern applies");
        assert!(matches!(
            Schema::compile(json!({"x": {"unevaluatedProperties": false}, "$ref": "#/x"})),
            Err(SchemaError::Unsupported { .. })
        ));
        assert!(matches!(
            Schema::compile(json!({"x": {"$ref": "other.json#/y"}, "$ref": "#/x"})),
            Err(SchemaError::BadRef { .. })
        ));
        // A cycle of references still compiles (each target is walked once).
        let cyc = schema(
            json!({"$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}}, "type": "null"}),
        );
        assert!(cyc.is_valid(&Value::Null));
    }

    /// Audit (sprint-corr-configurator): the old tolerance `1e-9 * q` reached 0.5 at q = 5e8, so
    /// every odd number from 1000000001 up passed `multipleOf: 2`.
    #[test]
    fn multiple_of_is_exact_for_large_integers() {
        let two = schema(json!({"multipleOf": 2}));
        assert!(!two.is_valid(&json!(1_000_000_001_u64)));
        assert!(!two.is_valid(&json!(9_007_199_254_740_993_u64)));
        assert!(two.is_valid(&json!(1_000_000_002_u64)));
        assert!(two.is_valid(&json!(-4)));
        assert!(
            !two.is_valid(&json!(1_000_000_001.0)),
            "a float spelling of an odd number"
        );
        let tenth = schema(json!({"multipleOf": 0.01}));
        assert!(tenth.is_valid(&json!(1234.56)));
        assert!(tenth.is_valid(&json!(1_000_000.01)));
        assert!(!tenth.is_valid(&json!(1234.565)));
    }

    /// Audit LOW (sprint-corr-configurator, fixed in corr-low-editor): `%25` was decoded first and
    /// `%22` after it, so `%2522` (an escaped `%22`) became `"`. A URI fragment is decoded once.
    #[test]
    fn a_ref_fragment_is_percent_decoded_once() {
        let s = schema(json!({
            "$defs": {"a%22b": {"const": 1}, "a\"b": {"const": 2}, "t~x": {"const": 3}},
            "$ref": "#/$defs/a%2522b"
        }));
        assert!(s.is_valid(&json!(1)), "`%2522` names the key `a%22b`");
        assert!(!s.is_valid(&json!(2)));
        let brace = schema(json!({"$defs": {"t{x": {"const": 3}}, "$ref": "#/$defs/t%7Bx"}));
        assert!(brace.is_valid(&json!(3)), "any %XX escape is decoded");
    }

    /// Audit LOW (sprint-corr-configurator): bounds were compared as f64, so `9007199254740993`
    /// (2^53 + 1) read as 2^53 and passed `maximum: 9007199254740992`.
    #[test]
    fn integer_bounds_are_exact_past_two_to_the_53() {
        let max = schema(json!({"maximum": 9_007_199_254_740_992_u64}));
        assert!(max.is_valid(&json!(9_007_199_254_740_992_u64)));
        assert!(!max.is_valid(&json!(9_007_199_254_740_993_u64)));
        let ex = schema(json!({"exclusiveMinimum": 9_007_199_254_740_992_u64}));
        assert!(ex.is_valid(&json!(9_007_199_254_740_993_u64)));
        assert!(!ex.is_valid(&json!(9_007_199_254_740_992_u64)));
        let min = schema(json!({"minimum": -9_007_199_254_740_992_i64}));
        assert!(!min.is_valid(&json!(-9_007_199_254_740_993_i64)));
        // A float bound against an integer value still works.
        let f = schema(json!({"maximum": 1.5}));
        assert!(f.is_valid(&json!(1)) && !f.is_valid(&json!(2)));
    }

    /// Audit LOW (sprint-corr-configurator): a count keyword holding `2.0` (a valid 2020-12
    /// integer) was read as absent, and one holding a string (`"3"`) compiled and was ignored.
    #[test]
    fn count_keywords_take_integral_floats_and_refuse_other_values() {
        let s = schema(json!({"minLength": 2.0, "maxItems": 1.0}));
        assert!(!s.is_valid(&json!("a")), "minLength 2.0 applies");
        assert!(s.is_valid(&json!("ab")));
        assert!(!s.is_valid(&json!([1, 2])), "maxItems 1.0 applies");
        for bad in [
            json!({"maxLength": "3"}),
            json!({"minItems": -1}),
            json!({"minProperties": 1.5}),
            json!({"maximum": "10"}),
            json!({"multipleOf": 0}),
            json!({"uniqueItems": "yes"}),
            json!({"properties": {"a": {"minContains": "1"}}}),
        ] {
            assert!(
                matches!(Schema::compile(bad.clone()), Err(SchemaError::Malformed { .. })),
                "{bad} must be refused"
            );
        }
    }

    /// Audit LOW (sprint-corr-configurator): an embedded `$id` starts a new resource whose
    /// fragment references resolve against it; this validator resolves them against the root, so
    /// such a schema is refused rather than evaluated wrongly. The root `$id` stays allowed.
    #[test]
    fn an_embedded_id_is_refused() {
        assert!(Schema::compile(json!({"$id": "https://x.invalid/root.json", "type": "null"})).is_ok());
        assert!(matches!(
            Schema::compile(json!({"$defs": {"x": {"$id": "https://x.invalid/x.json", "$ref": "#/a"}}})),
            Err(SchemaError::Unsupported { .. })
        ));
        assert!(matches!(
            Schema::compile(json!({"properties": {"p": {"$id": "p.json"}}})),
            Err(SchemaError::Unsupported { .. })
        ));
    }

    /// Audit LOW (sprint-corr-configurator): JSON Schema patterns are ECMA-262. The Rust engine's
    /// `.` also matched `\r`, U+2028 and U+2029, and its `\d`/`\w` matched non-ASCII digits and
    /// letters, so the shipped facet pattern `^[^=]{1,120}=.{1,200}$` accepted `a=b\rc`.
    #[test]
    fn patterns_follow_ecma_262_for_dot_digit_and_word() {
        let facet = schema(json!({"pattern": "^[^=]{1,120}=.{1,200}$"}));
        assert!(facet.is_valid(&json!("Metal=Gold")));
        for bad in ["a=b\rc", "a=b\u{2028}c", "a=b\u{2029}c", "a=b\nc"] {
            assert!(!facet.is_valid(&json!(bad)), "{bad:?}");
        }
        let digits = schema(json!({"pattern": "^\\d+$"}));
        assert!(digits.is_valid(&json!("0123456789")));
        assert!(
            !digits.is_valid(&json!("\u{0663}")),
            "ARABIC-INDIC DIGIT THREE is not \\d"
        );
        let not_digit = schema(json!({"pattern": "^\\D$"}));
        assert!(not_digit.is_valid(&json!("\u{0663}")));
        let word = schema(json!({"pattern": "^\\w+$"}));
        assert!(word.is_valid(&json!("a_Z9")));
        assert!(!word.is_valid(&json!("\u{e9}")), "é is not \\w");
        let class = schema(json!({"pattern": "^[\\d.]+$"}));
        assert!(class.is_valid(&json!("1.5")), "`.` in a class stays a dot");
        assert!(!class.is_valid(&json!("1x5")));
        let escaped = schema(json!({"pattern": "^a\\.b$"}));
        assert!(escaped.is_valid(&json!("a.b")) && !escaped.is_valid(&json!("axb")));
        let not_in_class = schema(json!({"pattern": "^[^\\d]$"}));
        assert!(not_in_class.is_valid(&json!("\u{0663}")) && !not_in_class.is_valid(&json!("3")));
        // The same pattern used as a `patternProperties` key follows the same rules.
        let props = schema(json!({"patternProperties": {"^\\d$": {"const": 1}}}));
        assert!(
            props.is_valid(&json!({"\u{0663}": 2})),
            "the key does not match, so no rule applies"
        );
        assert!(!props.is_valid(&json!({"3": 2})));
    }
}
