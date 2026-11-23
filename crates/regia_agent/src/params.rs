// SPDX-License-Identifier: MPL-2.0
//! Typed optional-parameter readers (ADR-2622).
//!
//! `params.get(k).and_then(Value::as_str)` reads a wrong-typed value as an absent one, so a call
//! like `{"count": "5"}` silently used the default. These helpers keep the old meaning for valid
//! input (absent or `null` is `None`) and refuse a present value of the wrong type with
//! `invalid_params`, naming the parameter (`data.param`) and the wanted type (`data.expected`).
//! `tests/typed_params_ratchet.rs` fails on any new `.get(..).and_then(Value::as_*)` site.

use serde_json::{Map, Value};

use crate::protocol::RpcError;

/// Something a parameter can be looked up in: a JSON object value or a bare map.
pub(crate) trait ParamSource<'a>: Copy {
    /// The value under `key`, if the source is an object that has it.
    fn param(self, key: &str) -> Option<&'a Value>;
}

impl<'a> ParamSource<'a> for &'a Value {
    fn param(self, key: &str) -> Option<&'a Value> {
        self.get(key)
    }
}

impl<'a> ParamSource<'a> for &'a Map<String, Value> {
    fn param(self, key: &str) -> Option<&'a Value> {
        self.get(key)
    }
}

fn opt<'a, T>(
    src: impl ParamSource<'a>,
    key: &str,
    expected: &str,
    read: impl FnOnce(&'a Value) -> Option<T>,
) -> Result<Option<T>, RpcError> {
    match src.param(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => read(v)
            .map(Some)
            .ok_or_else(|| RpcError::wrong_type(key, expected)),
    }
}

/// Refuses a present, non-`null` parameter of `method` whose JSON type is not the one the
/// method's schema declares (`string`, `number`, `integer`, `boolean`, `array`, `object`; `any`
/// and unknown methods are not checked), with the same error as the typed readers. For the paths
/// that answer without reading their parameters (a listing, a plugin that is not installed), so a
/// wrong-typed value is refused there too (ADR-2622, ADR-2740).
pub(crate) fn check_types(method: &str, params: &Value) -> Result<(), RpcError> {
    check_types_except(method, params, &[])
}

/// Parameters the server reads from any call before a handler runs (`server::timeout_for`), so a
/// handler that refuses unknown parameters still accepts them.
const ENVELOPE_PARAMS: &[&str] = &["timeout_ms"];

/// [`check_types`] for a method that takes every parameter by name (ADR-5957): `params` must be
/// an object (or absent), every key must be a parameter of the method's schema (or `timeout_ms`,
/// which the server reads), and every declared parameter except those in `mixed` must have its
/// schema's JSON type. `mixed` lists parameters whose schema kind is narrower than what the handler
/// accepts (a color given as `"#RRGGBB"` or as an RGBA array): the handler checks those itself.
/// An unknown key is `invalid_params` with `data.param` naming it, like a wrong-typed one.
pub(crate) fn check_known_and_types(method: &str, params: &Value, mixed: &[&str]) -> Result<(), RpcError> {
    let Some(schema) = crate::schema::METHODS.iter().find(|m| m.name == method) else {
        return Ok(());
    };
    match params {
        Value::Null => return Ok(()),
        Value::Object(map) => {
            if let Some(unknown) = map.keys().find(|k| {
                !ENVELOPE_PARAMS.contains(&k.as_str()) && !schema.params.iter().any(|p| p.name == k.as_str())
            }) {
                return Err(unknown_param(method, unknown));
            }
        }
        _ => return Err(RpcError::invalid_params("params must be an object")),
    }
    check_types_except(method, params, mixed)
}

/// `invalid_params` for a key `method` does not take, with `data.param` naming it.
pub(crate) fn unknown_param(method: &str, key: &str) -> RpcError {
    let mut err = RpcError::invalid_params(format!("{method} has no parameter `{key}`"));
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("param".into(), Value::String(key.into()));
    }
    err
}

fn check_types_except(method: &str, params: &Value, skip: &[&str]) -> Result<(), RpcError> {
    let Some(schema) = crate::schema::METHODS.iter().find(|m| m.name == method) else {
        return Ok(());
    };
    for param in schema.params.iter().filter(|p| !skip.contains(&p.name)) {
        let Some(value) = params.get(param.name).filter(|v| !v.is_null()) else {
            continue;
        };
        let expected = match param.kind {
            "string" if !value.is_string() => "a string",
            "number" if !value.is_number() => "a number",
            "integer" if !(value.is_i64() || value.is_u64()) => "an integer",
            "boolean" if !value.is_boolean() => "a boolean",
            "array" if !value.is_array() => "an array",
            "object" if !value.is_object() => "an object",
            _ => continue,
        };
        return Err(RpcError::wrong_type(param.name, expected));
    }
    Ok(())
}

/// An optional string parameter.
pub(crate) fn opt_str<'a>(src: impl ParamSource<'a>, key: &str) -> Result<Option<&'a str>, RpcError> {
    opt(src, key, "a string", Value::as_str)
}

/// An optional boolean parameter.
pub(crate) fn opt_bool<'a>(src: impl ParamSource<'a>, key: &str) -> Result<Option<bool>, RpcError> {
    opt(src, key, "a boolean", Value::as_bool)
}

/// An optional non-negative integer parameter.
pub(crate) fn opt_u64<'a>(src: impl ParamSource<'a>, key: &str) -> Result<Option<u64>, RpcError> {
    opt(src, key, "a non-negative integer", Value::as_u64)
}

/// An optional integer parameter.
pub(crate) fn opt_i64<'a>(src: impl ParamSource<'a>, key: &str) -> Result<Option<i64>, RpcError> {
    opt(src, key, "an integer", Value::as_i64)
}

/// An optional number parameter.
pub(crate) fn opt_f64<'a>(src: impl ParamSource<'a>, key: &str) -> Result<Option<f64>, RpcError> {
    opt(src, key, "a number", Value::as_f64)
}

/// An optional array parameter.
pub(crate) fn opt_array<'a>(
    src: impl ParamSource<'a>,
    key: &str,
) -> Result<Option<&'a Vec<Value>>, RpcError> {
    opt(src, key, "an array", Value::as_array)
}

/// An optional object parameter.
pub(crate) fn opt_object<'a>(
    src: impl ParamSource<'a>,
    key: &str,
) -> Result<Option<&'a Map<String, Value>>, RpcError> {
    opt(src, key, "an object", Value::as_object)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn absent_and_null_are_none() {
        let p = json!({ "n": null });
        assert_eq!(opt_u64(&p, "n").expect("reads"), None);
        assert_eq!(opt_u64(&p, "missing").expect("reads"), None);
        assert_eq!(opt_str(&json!(3), "k").expect("reads"), None);
    }

    #[test]
    fn right_type_is_read() {
        let p = json!({ "s": "a", "b": true, "u": 5, "i": -2, "f": 1.5, "a": [1], "o": {} });
        assert_eq!(opt_str(&p, "s").expect("reads"), Some("a"));
        assert_eq!(opt_bool(&p, "b").expect("reads"), Some(true));
        assert_eq!(opt_u64(&p, "u").expect("reads"), Some(5));
        assert_eq!(opt_i64(&p, "i").expect("reads"), Some(-2));
        assert_eq!(opt_f64(&p, "f").expect("reads"), Some(1.5));
        assert_eq!(opt_array(&p, "a").expect("reads").map(Vec::len), Some(1));
        assert!(opt_object(&p, "o").expect("reads").is_some());
    }

    #[test]
    fn wrong_type_names_param_and_expected() {
        let p = json!({ "count": "5", "neg": -1 });
        let err = opt_u64(&p, "count").expect_err("refused");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let data = err.data.expect("reads");
        assert_eq!(data["param"], "count");
        assert_eq!(data["expected"], "a non-negative integer");
        assert_eq!(data["code"], "F0002");
        assert!(opt_u64(&p, "neg").is_err());
        assert!(opt_str(&p, "count").is_ok());
        assert!(opt_bool(&p, "count").is_err());
    }

    #[test]
    fn check_types_refuses_by_the_schema_kind_and_skips_null() {
        // `xr.walk_set`: `active` boolean, `locomotion` string, `move_speed_mps` number.
        let ok = json!({ "active": true, "locomotion": "smooth", "move_speed_mps": 1.5, "preset": null });
        assert!(check_types("xr.walk_set", &ok).is_ok());
        let err = check_types("xr.walk_set", &json!({ "active": "yes" })).expect_err("refused");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let data = err.data.expect("data");
        assert_eq!(data["param"], "active");
        assert_eq!(data["expected"], "a boolean");
        assert!(check_types("xr.walk_set", &json!({ "move_speed_mps": "fast" })).is_err());
        assert!(check_types("no.such_method", &json!({ "x": 1 })).is_ok());
    }

    #[test]
    fn check_known_and_types_refuses_unknown_keys_and_wrong_types_but_not_mixed_ones() {
        // `ui.world_markers`: `label` string, `size` number, `color` string or RGBA array.
        let ok = json!({ "label": "A", "size": 4, "color": [1, 0, 0, 1], "timeout_ms": 50, "plate": null });
        assert!(check_known_and_types("ui.world_markers", &ok, &["color"]).is_ok());
        assert!(check_known_and_types("ui.world_markers", &Value::Null, &[]).is_ok());
        let err =
            check_known_and_types("ui.world_markers", &json!({ "lable": "A" }), &[]).expect_err("unknown");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        assert_eq!(err.data.as_ref().expect("data")["param"], "lable");
        let err = check_known_and_types("ui.world_markers", &json!({ "size": "4" }), &[]).expect_err("typed");
        assert_eq!(err.data.as_ref().expect("data")["param"], "size");
        assert!(check_known_and_types("ui.world_markers", &json!([1]), &[]).is_err());
        // Without the `mixed` exemption the schema's `string` refuses the array form.
        assert!(check_known_and_types("ui.world_markers", &json!({ "color": [1, 0, 0, 1] }), &[]).is_err());
    }
}
