// SPDX-License-Identifier: MPL-2.0
//! Agent method `net.quic_status` (`book/agents/net-quic.md`, ADR-3970).
//!
//! The handler reads the process-wide QUIC counters. It does not open a socket. With `net-quic`
//! off, the answer is `enabled: false` and every counter is zero.

use regia_net::quic_status;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// `net.quic_status`: whether QUIC datagrams are built in, and the live counters.
pub fn status(params: &Value) -> Result<Value, RpcError> {
    match params {
        Value::Null => Ok(snapshot()),
        Value::Object(map) if map.is_empty() => Ok(snapshot()),
        _ => Err(RpcError::new(
            INVALID_PARAMS,
            "quic_bad_params",
            "net.quic_status takes an empty object",
            "pass {}",
        )),
    }
}

fn snapshot() -> Value {
    let status = quic_status();
    json!({
        "enabled": status.enabled,
        "bound": status.bound,
        "datagrams_sent": status.datagrams_sent,
        "datagrams_lost": status.datagrams_lost,
        "last_error": status.last_error,
    })
}

#[cfg(test)]
mod tests {
    use super::status;
    use serde_json::json;

    #[test]
    fn quic_status_rejects_garbage() {
        let err = status(&json!({"nope": 1})).expect_err("garbage");
        let code = err
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(code, Some("quic_bad_params"));
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let _method = "net.quic_status";
    }

    #[test]
    fn quic_status_empty_object_answers() {
        let value = status(&json!({})).expect("empty object");
        assert!(value.get("enabled").is_some());
        assert!(value.get("bound").is_some());
        assert!(value.get("datagrams_sent").is_some());
        assert!(value.get("datagrams_lost").is_some());
        assert!(value.get("last_error").is_some());
        let null_value = status(&serde_json::Value::Null).expect("null");
        assert_eq!(null_value.get("enabled"), value.get("enabled"));
    }
}
