// SPDX-License-Identifier: MPL-2.0
//! Wire format: JSON-RPC 2.0 over WebSocket text messages (`docs/specs/23-agent-native.md` § 2).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version reported by `agent.hello`. Additive changes keep the major version.
/// 0.2 adds the M11 runtime surface (specs/23 § 3.1–3.5, § 3.7–3.8): discovery, scene edits,
/// asset access, capture sequences, wait/expect testing, event taps and permissions.
pub const PROTOCOL_VERSION: &str = "0.2";

/// Every method of the protocol, as `agent.hello` reports them (see [`crate::schema`]).
pub fn methods() -> Vec<&'static str> {
    crate::schema::method_names()
}

/// A request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id (number or string); absent for notifications, which get no response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Method name, e.g. `scene.get_entity`.
    pub method: String,
    /// Parameters (an object).
    #[serde(default)]
    pub params: Value,
}

/// A response: exactly one of `result` and `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// The request's id.
    pub id: Value,
    /// Success value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A success response.
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    /// A failure response.
    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC error. `data.code` carries Regia Engine's stable error code (`F0xxx`, specs/23 § 4) and
/// `data.hint` a suggested fix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// Short human-readable message.
    pub message: String,
    /// `{ "code": "F0xxx", "hint": "..." }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC: invalid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC: not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC: unknown method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: bad parameters.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC: internal error.
pub const INTERNAL_ERROR: i64 = -32603;
/// Regia Engine: the referenced entity, component or field does not exist.
pub const NOT_FOUND: i64 = -32001;
/// Regia Engine: the call timed out waiting for the engine.
pub const TIMEOUT: i64 = -32002;
/// Regia Engine: rate limit exceeded.
pub const RATE_LIMITED: i64 = -32003;
/// Regia Engine: the operation is not available in this build or on this platform.
pub const UNSUPPORTED: i64 = -32004;
/// Regia Engine: the call needs a permission the project did not grant (`.regia/agent.toml`).
pub const PERMISSION_DENIED: i64 = -32005;

impl RpcError {
    /// An error with a Regia Engine code and fix hint.
    pub fn new(code: i64, regia_code: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(serde_json::json!({ "code": regia_code, "hint": hint.into() })),
        }
    }
    /// Bad or missing parameters.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(
            INVALID_PARAMS,
            "F0002",
            message,
            "check the method's parameters in the agent guide",
        )
    }
    /// A present parameter of the wrong type: `data.param` names it and `data.expected` is the JSON
    /// type (or shape) the method wanted.
    pub fn wrong_type(param: &str, expected: &str) -> Self {
        let mut err = Self::invalid_params(format!("`{param}` must be {expected}"));
        if let Some(Value::Object(data)) = err.data.as_mut() {
            data.insert("param".into(), Value::String(param.into()));
            data.insert("expected".into(), Value::String(expected.into()));
        }
        err
    }
    /// Something does not exist.
    pub fn not_found(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(NOT_FOUND, "F0003", message, hint)
    }
    /// Internal failure.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, "F0001", message, "see logs.read for details")
    }
}

/// Contents of `.regia/agent/endpoint.json`, written by the running app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    /// WebSocket URL, e.g. `ws://127.0.0.1:41234`.
    pub url: String,
    /// Process id of the app.
    pub pid: u32,
    /// Protocol version.
    pub protocol: String,
    /// Path of the bearer token file.
    pub token_file: String,
    /// Executable name.
    pub app: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_round_trip() {
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"method":"agent.hello"}"#).expect("parses");
        assert_eq!(req.method, "agent.hello");
        assert_eq!(req.params, Value::Null);
        let ok = serde_json::to_string(&Response::ok(serde_json::json!(7), serde_json::json!({"a":1})))
            .expect("serializes");
        assert_eq!(ok, r#"{"jsonrpc":"2.0","id":7,"result":{"a":1}}"#);
        let err = Response::err(
            serde_json::json!("x"),
            RpcError::not_found("no entity 5", "use scene.query_entities"),
        );
        let text = serde_json::to_string(&err).expect("serializes");
        assert!(
            text.contains(r#""code":-32001"#) && text.contains("F0003"),
            "{text}"
        );
    }

    #[test]
    fn error_constructors_carry_their_codes_and_the_json_rpc_numbers_are_distinct() {
        let cases = [
            (RpcError::invalid_params("m"), INVALID_PARAMS, "F0002"),
            (RpcError::not_found("m", "h"), NOT_FOUND, "F0003"),
            (RpcError::internal("m"), INTERNAL_ERROR, "F0001"),
        ];
        for (err, code, regia) in cases {
            assert_eq!(err.code, code);
            assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some(regia));
            assert!(err.data.as_ref().is_some_and(|d| d["hint"].is_string()));
        }
        let mut codes = [
            PARSE_ERROR,
            INVALID_REQUEST,
            METHOD_NOT_FOUND,
            INVALID_PARAMS,
            INTERNAL_ERROR,
            NOT_FOUND,
            TIMEOUT,
            RATE_LIMITED,
            UNSUPPORTED,
            PERMISSION_DENIED,
        ];
        codes.sort_unstable();
        assert!(codes.windows(2).all(|w| w[0] != w[1]), "{codes:?}");
        // The engine's own codes stay in the server-defined range of JSON-RPC (-32000..=-32099).
        for code in [NOT_FOUND, TIMEOUT, RATE_LIMITED, UNSUPPORTED, PERMISSION_DENIED] {
            assert!((-32099..=-32000).contains(&code), "{code}");
        }
    }

    #[test]
    fn requests_accept_string_ids_and_notifications_and_refuse_malformed_objects() {
        let text: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"abc","method":"m","params":{"a":1}}"#)
                .expect("string id");
        assert_eq!(text.id, Some(serde_json::json!("abc")));
        let note: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","method":"m"}"#).expect("notification");
        assert!(note.id.is_none());
        let echoed = serde_json::to_string(&note).expect("serializes");
        assert!(!echoed.contains("\"id\""), "an absent id is omitted: {echoed}");
        // A positional params array parses (handlers decide), and null ids are kept as null.
        let arr: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","id":null,"method":"m","params":[1,2]}"#)
            .expect("array");
        assert!(arr.params.is_array());
        for bad in [
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"id":1,"method":"m"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":5}"#,
            r#"[1,2,3]"#,
            r#""method""#,
            "",
            "{",
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_response_serializes_exactly_one_of_result_and_error_and_round_trips() {
        let ok = Response::ok(serde_json::json!(1), serde_json::json!(null));
        let ok_text = serde_json::to_string(&ok).expect("ok");
        assert!(!ok_text.contains("error"), "{ok_text}");
        let err = Response::err(serde_json::json!(2), RpcError::internal("x"));
        let err_text = serde_json::to_string(&err).expect("err");
        assert!(!err_text.contains("result"), "{err_text}");
        let back: Response = serde_json::from_str(&err_text).expect("parses");
        assert_eq!(back.error, err.error);
        assert!(back.result.is_none());
        let back_ok: Response = serde_json::from_str(&ok_text).expect("parses");
        assert!(back_ok.error.is_none());
        // `"result": null` is a present-but-null result on the wire; serde reads it as absent.
        // Pinned so a client change that depends on it is noticed.
        assert!(back_ok.result.is_none());
    }

    #[test]
    fn the_method_list_is_sorted_unique_and_lowercase_dotted() {
        let methods = methods();
        assert!(!methods.is_empty());
        assert!(
            methods.windows(2).all(|w| w[0] < w[1]),
            "sorted and no duplicates"
        );
        for m in methods {
            assert!(
                m.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'),
                "{m}"
            );
            assert!(
                !m.starts_with('.') && !m.ends_with('.') && !m.contains(".."),
                "{m}"
            );
        }
    }

    #[test]
    fn the_endpoint_round_trips_and_the_version_is_major_dot_minor() {
        let endpoint = Endpoint {
            url: "ws://127.0.0.1:1".into(),
            pid: u32::MAX,
            protocol: PROTOCOL_VERSION.into(),
            token_file: "t".into(),
            app: "a".into(),
        };
        let text = serde_json::to_string(&endpoint).expect("serializes");
        assert_eq!(serde_json::from_str::<Endpoint>(&text).expect("parses"), endpoint);
        assert!(
            serde_json::from_str::<Endpoint>(
                r#"{"url":"x","pid":-1,"protocol":"0.2","token_file":"","app":""}"#
            )
            .is_err()
        );
        let (major, minor) = PROTOCOL_VERSION.split_once('.').expect("major.minor");
        assert!(major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok());
    }
}
