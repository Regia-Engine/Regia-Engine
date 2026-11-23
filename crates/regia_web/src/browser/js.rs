// SPDX-License-Identifier: MPL-2.0
//! JSON to JS values and back, and errors in the shape the page sees. The player speaks `serde_json`, so
//! `JSON.parse` and `JSON.stringify` are the whole bridge and the module carries no second serializer.

use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};

use crate::WebError;

/// A JSON value as a JS value.
pub fn to_js(value: &Value) -> Result<JsValue, JsValue> {
    let text = serde_json::to_string(value)
        .map_err(|e| error_value(&WebError::new("bad_argument", e.to_string())))?;
    js_sys::JSON::parse(&text)
}

/// A JS value as JSON. `undefined` reads as null. A value `JSON.stringify` has no text for (a function, a
/// symbol: it answers `undefined` without throwing) is a `bad_argument`, never an uncoded throw.
pub fn from_js(value: &JsValue) -> Result<Value, WebError> {
    if value.is_undefined() || value.is_null() {
        return Ok(Value::Null);
    }
    let text = js_sys::JSON::stringify(value)
        .map_err(|_| WebError::new("bad_argument", "the argument cannot be turned into JSON"))?
        .as_string();
    crate::error::argument_json(text.as_deref())
}

/// The thrown or rejected object: a JS `Error` with a `code` property.
pub fn error_value(error: &WebError) -> JsValue {
    let e = js_sys::Error::new(&error.message);
    let value: JsValue = e.into();
    // Setting a property on a fresh Error cannot fail.
    let _ = js_sys::Reflect::set(&value, &"code".into(), &error.code.into());
    value
}

/// A short readable form of a value a browser API rejected with.
pub fn describe(value: &JsValue) -> String {
    if let Some(e) = value.dyn_ref::<js_sys::Error>() {
        return String::from(e.message());
    }
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
