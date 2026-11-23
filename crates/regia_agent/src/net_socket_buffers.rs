// SPDX-License-Identifier: MPL-2.0
//! Agent method `net.socket_buffers` (`book/agents/net-socket-buffers.md`, ADR-7998).
//!
//! The call binds a new `127.0.0.1:0` UDP socket, applies the requested buffers and DSCP EF
//! mark, and returns the kernel read-back. It does not read the live game socket.

use std::net::{Ipv4Addr, SocketAddr};

use regia_net::{DatagramSocket, MAX_SOCKET_BUFFER_BYTES, SocketBufferSettings, UdpTransport};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// `net.socket_buffers`: bind loopback, apply buffer settings, report the kernel grant.
pub fn socket_buffers(params: &Value) -> Result<Value, RpcError> {
    let empty = Map::new();
    let map = match params {
        Value::Null => &empty,
        Value::Object(map) => map,
        _ => return Err(bad("net.socket_buffers takes an object")),
    };
    for key in map.keys() {
        if !matches!(key.as_str(), "recv_bytes" | "send_bytes" | "dscp_ef") {
            return Err(bad("unknown parameter"));
        }
    }
    let defaults = SocketBufferSettings::default();
    let recv = int_field(map, "recv_bytes", u64_size(defaults.recv_bytes)?)?;
    let send = int_field(map, "send_bytes", u64_size(defaults.send_bytes)?)?;
    let dscp_ef = bool_field(map, "dscp_ef")?;
    let settings = SocketBufferSettings {
        recv_bytes: fit(recv)?,
        send_bytes: fit(send)?,
        dscp_ef,
    };
    let transport =
        UdpTransport::bind_with(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), settings).map_err(|err| {
            RpcError::new(
                INVALID_PARAMS,
                "socket_buffers_bind",
                format!("could not bind 127.0.0.1:0: {err}"),
                "the host must accept a loopback UDP port",
            )
        })?;
    let report = transport.buffer_report();
    let local_addr = transport.local_addr().map_err(|err| {
        RpcError::new(
            INVALID_PARAMS,
            "socket_buffers_bind",
            format!("bound socket has no local address: {err}"),
            "the host must accept a loopback UDP port",
        )
    })?;
    Ok(json!({
        "requested_recv": report.requested_recv,
        "requested_send": report.requested_send,
        "confirmed_recv": report.confirmed_recv,
        "confirmed_send": report.confirmed_send,
        "meets_request": report.meets_request(),
        "below_half": report.below_half_request(),
        "dscp_ef_applied": report.dscp_ef_applied,
        "read_back": report.read_back,
        "dont_fragment": report.dont_fragment,
        "local_addr": local_addr.to_string(),
    }))
}

fn u64_size(bytes: usize) -> Result<u64, RpcError> {
    u64::try_from(bytes).map_err(|_| bad("default buffer size does not fit this host"))
}

fn fit(bytes: u64) -> Result<usize, RpcError> {
    usize::try_from(bytes).map_err(|_| bad("buffer size does not fit this host"))
}

/// `MAX_SOCKET_BUFFER_BYTES` as `u64`. A ceiling that does not fit rejects every size.
fn max_request_bytes() -> u64 {
    u64::try_from(MAX_SOCKET_BUFFER_BYTES).unwrap_or(0)
}

fn int_field(map: &Map<String, Value>, key: &str, default: u64) -> Result<u64, RpcError> {
    let Some(value) = map.get(key).filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let Some(parsed) = value.as_u64() else {
        return Err(bad(&format!(
            "{key} must be an integer from 1 to {MAX_SOCKET_BUFFER_BYTES}"
        )));
    };
    if parsed == 0 || parsed > max_request_bytes() {
        return Err(bad(&format!("{key} must be from 1 to {MAX_SOCKET_BUFFER_BYTES}")));
    }
    Ok(parsed)
}

fn bool_field(map: &Map<String, Value>, key: &str) -> Result<bool, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(bad(&format!("{key} must be a boolean"))),
    }
}

fn bad(message: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "socket_buffers_bad_params",
        message,
        "pass recv_bytes and send_bytes from 1 to 67108864, and dscp_ef as a boolean; {} uses the defaults",
    )
}

#[cfg(test)]
mod tests {
    use super::socket_buffers;
    use serde_json::json;

    fn code(value: &serde_json::Value) -> Option<&str> {
        value.as_str()
    }

    fn err_code(params: serde_json::Value) -> String {
        let err = socket_buffers(&params).expect_err("refused");
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(code)
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn socket_buffers_rejects_bad_params() {
        let _method = "net.socket_buffers";
        assert_eq!(err_code(json!("nope")), "socket_buffers_bad_params");
        assert_eq!(err_code(json!({ "recv_bytes": 0 })), "socket_buffers_bad_params");
        assert_eq!(
            err_code(json!({ "send_bytes": 67_108_865 })),
            "socket_buffers_bad_params"
        );
        assert_eq!(err_code(json!({ "dscp_ef": "yes" })), "socket_buffers_bad_params");
        assert_eq!(err_code(json!({ "other": 1 })), "socket_buffers_bad_params");
        assert_eq!(regia_net::MAX_SOCKET_BUFFER_BYTES, 67_108_864);
    }

    #[test]
    fn socket_buffers_empty_object_reports_a_loopback_socket() {
        let value = socket_buffers(&json!({})).expect("loopback bind");
        assert_eq!(value["requested_recv"], json!(2_097_152));
        assert_eq!(value["requested_send"], json!(524_288));
        assert!(value["confirmed_recv"].is_number());
        assert!(value["confirmed_send"].is_number());
        assert!(value["meets_request"].is_boolean());
        assert!(value["below_half"].is_boolean());
        assert!(value["dscp_ef_applied"].is_boolean());
        assert!(value["read_back"].is_boolean());
        // Don't-fragment (ADR-8893): the platforms with a backend set it on every socket.
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows))]
        assert_eq!(value["dont_fragment"], json!(true), "{value}");
        let addr = value["local_addr"].as_str().expect("addr");
        assert!(addr.starts_with("127.0.0.1:"), "{addr}");
        let sized = socket_buffers(&json!({ "recv_bytes": 65536, "send_bytes": 65536, "dscp_ef": false }))
            .expect("sized bind");
        assert_eq!(sized["requested_recv"], json!(65_536));
        assert_eq!(sized["requested_send"], json!(65_536));
        assert_eq!(sized["dscp_ef_applied"], json!(false));
    }
}
