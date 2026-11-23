// SPDX-License-Identifier: MPL-2.0
//! Checks the params of an extension agent method against its `params_schema` before the handler
//! runs (`docs/specs/57-plugin-extension-api.md` § 3.3.4, ADR-2136).
//!
//! The engine has no JSON Schema dependency, so this is a bounded subset of JSON Schema
//! draft 2020-12 (<https://json-schema.org/draft/2020-12/json-schema-validation>): `type`
//! (`object`, `array`, `string`, `number`, `integer`, `boolean`, `null`, or a list of them),
//! `enum`, `const`, `required`, `properties`, `additionalProperties` (a boolean or a schema),
//! `items`, `minItems`, `maxItems`, `minLength`, `maxLength` (in Unicode scalar values),
//! `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` (numbers), `allOf`, `anyOf`,
//! `oneOf` and `not`. Every other keyword (`pattern`, `$ref`, `format`, ...) is ignored, so a
//! schema that uses one is checked only as far as this subset reaches; the handler still owns
//! its own validation (it is trusted native code, spec § 2.7). A boolean schema is honored
//! (`true` accepts everything, `false` nothing).
//!
//! The walk is bounded: values and schemas nested deeper than [`MAX_DEPTH`] are refused with
//! the rule `depth`, so a hostile request cannot exhaust the stack. The first violation in a
//! fixed order (object keys ascending, array items by index) is reported, so the answer for one
//! input never varies.

use serde_json::{Map, Value};

/// How deep the check follows nested values and schemas before it refuses.
pub const MAX_DEPTH: usize = 32;

/// The first way the params break the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamsViolation {
    /// JSON Pointer (RFC 6901) of the offending value inside the params; empty for the root.
    pub path: String,
    /// The schema keyword that failed (`type`, `required`, `enum`, ..., or `depth`).
    pub rule: &'static str,
    /// A sentence for the caller.
    pub detail: String,
}

impl std::fmt::Display for ParamsViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let at = if self.path.is_empty() { "/" } else { &self.path };
        write!(f, "{at}: {} ({})", self.detail, self.rule)
    }
}

/// Checks `params` against `schema`. Absent params (`null`) count as `{}`, because the
/// dispatcher passes `null` when a request carries none and a method schema is an object.
///
/// # Errors
///
/// The first [`ParamsViolation`] in key and index order.
pub fn check_params(schema: &Value, params: &Value) -> Result<(), ParamsViolation> {
    let empty = Value::Object(Map::new());
    let params = if params.is_null() { &empty } else { params };
    let mut path = String::new();
    check(schema, params, &mut path, 0)
}

fn violation(path: &str, rule: &'static str, detail: String) -> ParamsViolation {
    ParamsViolation {
        path: path.to_owned(),
        rule,
        detail,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if is_integer(n) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_integer(n: &serde_json::Number) -> bool {
    n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => matches!(value, Value::Number(n) if is_integer(n)),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        // An unknown type name matches nothing, as the standard reads.
        _ => false,
    }
}

/// Appends one RFC 6901 reference token (`~` as `~0`, `/` as `~1`).
fn push_token(path: &mut String, token: &str) {
    path.push('/');
    for c in token.chars() {
        match c {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            c => path.push(c),
        }
    }
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

fn count(schema: &Map<String, Value>, key: &str) -> Option<u64> {
    schema.get(key).and_then(Value::as_u64)
}

fn check(schema: &Value, value: &Value, path: &mut String, depth: usize) -> Result<(), ParamsViolation> {
    if depth > MAX_DEPTH {
        return Err(violation(
            path,
            "depth",
            format!("nested deeper than {MAX_DEPTH} levels"),
        ));
    }
    let schema = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => {
            return Err(violation(path, "false", "no value is allowed here".into()));
        }
        Value::Object(map) => map,
        // A malformed (non-object) schema checks nothing; registration does not police it.
        _ => return Ok(()),
    };
    check_type(schema, value, path)?;
    check_values(schema, value, path)?;
    check_numbers(schema, value, path)?;
    check_lengths(schema, value, path)?;
    check_object(schema, value, path, depth)?;
    check_array(schema, value, path, depth)?;
    check_combinators(schema, value, path, depth)
}

fn check_type(schema: &Map<String, Value>, value: &Value, path: &str) -> Result<(), ParamsViolation> {
    let ok = match schema.get("type") {
        None => true,
        Some(Value::String(name)) => matches_type(name, value),
        Some(Value::Array(names)) => names
            .iter()
            .any(|n| n.as_str().is_some_and(|n| matches_type(n, value))),
        Some(_) => true,
    };
    if ok {
        Ok(())
    } else {
        let wanted = schema.get("type").map(ToString::to_string).unwrap_or_default();
        Err(violation(
            path,
            "type",
            format!("expected {wanted}, got {}", type_name(value)),
        ))
    }
}

/// JSON equality with numbers compared by value (`1` equals `1.0`), as the standard asks.
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x == y || x.as_f64().is_some_and(|x| y.as_f64() == Some(x)),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => a == b,
    }
}

fn check_values(schema: &Map<String, Value>, value: &Value, path: &str) -> Result<(), ParamsViolation> {
    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.iter().any(|a| json_equal(a, value))
    {
        return Err(violation(
            path,
            "enum",
            format!("{value} is not one of {}", Value::Array(allowed.clone())),
        ));
    }
    if let Some(expected) = schema.get("const")
        && !json_equal(expected, value)
    {
        return Err(violation(
            path,
            "const",
            format!("expected {expected}, got {value}"),
        ));
    }
    Ok(())
}

/// A numeric keyword, the test it applies to the value and its bound, and the operator it reads as.
type NumericRule = (&'static str, fn(f64, f64) -> bool, &'static str);

fn check_numbers(schema: &Map<String, Value>, value: &Value, path: &str) -> Result<(), ParamsViolation> {
    let Some(v) = number(value) else {
        return Ok(());
    };
    let rules: [NumericRule; 4] = [
        ("minimum", |v, b| v >= b, ">="),
        ("maximum", |v, b| v <= b, "<="),
        ("exclusiveMinimum", |v, b| v > b, ">"),
        ("exclusiveMaximum", |v, b| v < b, "<"),
    ];
    for (key, holds, op) in rules {
        if let Some(bound) = schema.get(key).and_then(number)
            && !holds(v, bound)
        {
            return Err(violation(path, key, format!("{value} must be {op} {bound}")));
        }
    }
    Ok(())
}

fn check_lengths(schema: &Map<String, Value>, value: &Value, path: &str) -> Result<(), ParamsViolation> {
    let (len, min_key, max_key, unit) = match value {
        Value::String(s) => (s.chars().count() as u64, "minLength", "maxLength", "characters"),
        Value::Array(a) => (a.len() as u64, "minItems", "maxItems", "items"),
        _ => return Ok(()),
    };
    if let Some(min) = count(schema, min_key)
        && len < min
    {
        return Err(violation(
            path,
            min_key,
            format!("{len} {unit}, at least {min} needed"),
        ));
    }
    if let Some(max) = count(schema, max_key)
        && len > max
    {
        return Err(violation(
            path,
            max_key,
            format!("{len} {unit}, at most {max} allowed"),
        ));
    }
    Ok(())
}

fn check_object(
    schema: &Map<String, Value>,
    value: &Value,
    path: &mut String,
    depth: usize,
) -> Result<(), ParamsViolation> {
    let Value::Object(object) = value else {
        return Ok(());
    };
    if let Some(Value::Array(required)) = schema.get("required") {
        let mut missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| !object.contains_key(*key))
            .collect();
        missing.sort_unstable();
        if let Some(first) = missing.first() {
            return Err(violation(
                path,
                "required",
                format!("missing required field `{first}`"),
            ));
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    let additional = schema.get("additionalProperties");
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort_unstable();
    for key in keys {
        let child = &object[key.as_str()];
        let sub = match properties.and_then(|p| p.get(key.as_str())) {
            Some(sub) => sub,
            None => match additional {
                Some(Value::Bool(false)) => {
                    return Err(violation(
                        path,
                        "additionalProperties",
                        format!("unknown field `{key}`"),
                    ));
                }
                Some(sub) => sub,
                None => continue,
            },
        };
        let len = path.len();
        push_token(path, key);
        let result = check(sub, child, path, depth + 1);
        path.truncate(len);
        result?;
    }
    Ok(())
}

fn check_array(
    schema: &Map<String, Value>,
    value: &Value,
    path: &mut String,
    depth: usize,
) -> Result<(), ParamsViolation> {
    let (Value::Array(items), Some(sub)) = (value, schema.get("items")) else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        let len = path.len();
        push_token(path, &index.to_string());
        let result = check(sub, item, path, depth + 1);
        path.truncate(len);
        result?;
    }
    Ok(())
}

fn check_combinators(
    schema: &Map<String, Value>,
    value: &Value,
    path: &mut String,
    depth: usize,
) -> Result<(), ParamsViolation> {
    if let Some(Value::Array(all)) = schema.get("allOf") {
        for sub in all {
            check(sub, value, path, depth + 1)?;
        }
    }
    if let Some(Value::Array(any)) = schema.get("anyOf") {
        let mut first = None;
        let mut matched = false;
        for sub in any {
            match check(sub, value, path, depth + 1) {
                Ok(()) => {
                    matched = true;
                    break;
                }
                Err(e) if e.rule == "depth" => return Err(e),
                Err(e) => {
                    first.get_or_insert(e);
                }
            }
        }
        if !matched && !any.is_empty() {
            let why = first.map_or_else(String::new, |e| format!("; first branch: {e}"));
            return Err(violation(path, "anyOf", format!("matches no listed schema{why}")));
        }
    }
    if let Some(Value::Array(one)) = schema.get("oneOf") {
        let mut matched = 0usize;
        for sub in one {
            match check(sub, value, path, depth + 1) {
                Ok(()) => matched += 1,
                Err(e) if e.rule == "depth" => return Err(e),
                Err(_) => {}
            }
        }
        if matched != 1 {
            return Err(violation(
                path,
                "oneOf",
                format!("matches {matched} of the listed schemas, exactly one needed"),
            ));
        }
    }
    if let Some(sub) = schema.get("not") {
        match check(sub, value, path, depth + 1) {
            Ok(()) => {
                return Err(violation(path, "not", "matches a schema it must not".into()));
            }
            Err(e) if e.rule == "depth" => return Err(e),
            Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(schema: &Value, params: &Value) -> Option<(&'static str, String)> {
        check_params(schema, params).err().map(|v| (v.rule, v.path))
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["name", "count"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "minLength": 1, "maxLength": 4 },
                "count": { "type": "integer", "minimum": 0, "exclusiveMaximum": 10 },
                "mode": { "enum": ["fast", "slow"] },
                "tags": { "type": "array", "maxItems": 2, "items": { "type": "string" } },
                "a/b~c": { "const": 1 },
                "pick": { "oneOf": [ { "type": "string" }, { "type": "integer" } ] },
                "either": { "anyOf": [ { "type": "null" }, { "type": "boolean" } ] },
                "nope": { "not": { "type": "string" } }
            }
        })
    }

    #[test]
    fn a_valid_call_passes() {
        let s = schema();
        assert_eq!(rule(&s, &json!({ "name": "ab", "count": 3 })), None);
        assert_eq!(
            rule(
                &s,
                &json!({ "name": "abcd", "count": 9.0, "mode": "slow", "tags": ["x"], "a/b~c": 1.0,
                         "pick": 4, "either": false, "nope": 2 })
            ),
            None
        );
    }

    #[test]
    fn each_rule_names_itself_and_the_path() {
        let s = schema();
        let base = |extra: Value| {
            let mut v = json!({ "name": "ab", "count": 1 });
            if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
                m.extend(e);
            }
            v
        };
        assert_eq!(
            rule(&s, &json!({ "name": "ab" })),
            Some(("required", String::new()))
        );
        assert_eq!(rule(&s, &json!([])), Some(("type", String::new())));
        assert_eq!(
            rule(&s, &base(json!({ "name": 3 }))),
            Some(("type", String::from("/name")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "name": "" }))),
            Some(("minLength", String::from("/name")))
        );
        // Length counts characters, not bytes.
        assert_eq!(rule(&s, &base(json!({ "name": "éééé" }))), None);
        assert_eq!(
            rule(&s, &base(json!({ "name": "abcde" }))),
            Some(("maxLength", String::from("/name")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "count": -1 }))),
            Some(("minimum", String::from("/count")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "count": 10 }))),
            Some(("exclusiveMaximum", String::from("/count")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "count": 1.5 }))),
            Some(("type", String::from("/count")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "mode": "warp" }))),
            Some(("enum", String::from("/mode")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "tags": ["a", "b", "c"] }))),
            Some(("maxItems", String::from("/tags")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "tags": ["a", 2] }))),
            Some(("type", String::from("/tags/1")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "a/b~c": 2 }))),
            Some(("const", String::from("/a~1b~0c")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "pick": true }))),
            Some(("oneOf", String::from("/pick")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "either": 1 }))),
            Some(("anyOf", String::from("/either")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "nope": "x" }))),
            Some(("not", String::from("/nope")))
        );
        assert_eq!(
            rule(&s, &base(json!({ "zzz": 1 }))),
            Some(("additionalProperties", String::new()))
        );
    }

    #[test]
    fn null_params_count_as_an_empty_object() {
        assert_eq!(rule(&json!({ "type": "object" }), &Value::Null), None);
        assert_eq!(
            rule(&json!({ "type": "object", "required": ["id"] }), &Value::Null),
            Some(("required", String::new()))
        );
    }

    #[test]
    fn the_first_violation_is_in_key_order() {
        let s = json!({ "type": "object", "required": ["z", "b", "m"],
                        "properties": { "a": { "type": "string" }, "c": { "type": "string" } } });
        let v = check_params(&s, &json!({})).expect_err("missing");
        assert!(v.detail.contains("`b`"), "{v}");
        let v = check_params(&s, &json!({ "z": 0, "b": 0, "m": 0, "c": 1, "a": 1 })).expect_err("types");
        assert_eq!(v.path, "/a");
    }

    #[test]
    fn boolean_and_odd_schemas() {
        assert_eq!(rule(&json!(true), &json!({ "x": 1 })), None);
        assert_eq!(rule(&json!(false), &json!({})), Some(("false", String::new())));
        // Malformed or unknown keywords check nothing.
        assert_eq!(rule(&json!("object"), &json!(3)), None);
        assert_eq!(
            rule(&json!({ "pattern": "^a$", "format": "uri" }), &json!({})),
            None
        );
        assert_eq!(
            rule(&json!({ "type": ["string", "null"] }), &json!({})),
            Some(("type", String::new()))
        );
        assert_eq!(
            rule(&json!({ "type": "widget" }), &json!({})),
            Some(("type", String::new()))
        );
        // `additionalProperties` may be a schema.
        let s = json!({ "additionalProperties": { "type": "number" } });
        assert_eq!(rule(&s, &json!({ "k": "v" })), Some(("type", String::from("/k"))));
    }

    #[test]
    fn deep_values_and_schemas_stop_at_the_bound() {
        // A schema that recurses through `items` as deep as the value goes.
        let mut s = json!({});
        for _ in 0..(MAX_DEPTH + 8) {
            s = json!({ "type": "array", "items": s });
        }
        let mut v = json!(0);
        for _ in 0..(MAX_DEPTH + 8) {
            v = json!([v]);
        }
        let err = check_params(&s, &v).expect_err("too deep");
        assert_eq!(err.rule, "depth");
        // A shallow value under the same schema is fine.
        assert_eq!(rule(&s, &json!([])), None);
        // Self-nesting combinators also stop.
        let mut s = json!({ "type": "object" });
        for _ in 0..(MAX_DEPTH + 8) {
            s = json!({ "anyOf": [s] });
        }
        assert_eq!(check_params(&s, &json!({})).expect_err("deep").rule, "depth");
    }

    #[test]
    fn no_panic_on_arbitrary_pairs() {
        // A fixed pseudo-random walk over small values and schemas: the check is total.
        let values = [
            json!(null),
            json!(true),
            json!(-1),
            json!(2.5),
            json!(u64::MAX),
            json!("s"),
            json!([1, "a", null]),
            json!({ "k": [ {} ] }),
        ];
        let schemas = [
            json!({ "minimum": "x" }),
            json!({ "required": "id" }),
            json!({ "required": [1, null] }),
            json!({ "properties": [] }),
            json!({ "items": 7 }),
            json!({ "enum": "a" }),
            json!({ "anyOf": [] }),
            json!({ "oneOf": [true, true] }),
            json!({ "maxLength": -1, "minItems": 1.5 }),
            schema(),
        ];
        let mut seen = 0usize;
        for s in &schemas {
            for v in &values {
                let _ = check_params(s, v);
                seen += 1;
            }
        }
        assert_eq!(seen, schemas.len() * values.len());
        // Two matching branches of oneOf fail, as the standard reads.
        assert_eq!(
            rule(&json!({ "oneOf": [true, true] }), &json!(1)),
            Some(("oneOf", String::new()))
        );
    }
}
