// SPDX-License-Identifier: MPL-2.0
//! `detsim.hover_hull_check`: parse a `.hoverhull.ron` on the host (ADR-8512).
//!
//! The lockstep handshake compares the hull handshake
//! ([`hull_handshake_bytes`](regia_detsim::hover::hull_handshake_bytes): params, flight-mode byte,
//! limits), returned here as `handshake_hex`, with an omitted `limits` block or flight mode filled
//! with the engine defaults ([`HoverLimits::DEFAULT`](regia_detsim::hover::HoverLimits::DEFAULT),
//! `air_time`). `bytes_hex` is the params part only
//! ([`HoverParams::to_le_bytes`](regia_detsim::hover::HoverParams::to_le_bytes)). The JSON numbers
//! are not the handshake.

use std::path::Path;

use regia_app::prelude::*;
use regia_detsim::hover::{FlightMode, HoverLimits};
use regia_framework::det_hover::{HoverHullAsset, HoverHullError};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Largest hull text `text` or a `path` file may hold (1 MiB, the contract's `text` cap).
const MAX_TEXT: usize = 1 << 20;

/// `detsim.hover_hull_check`: exactly one of `text` or `path`.
pub fn hover_hull_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = optional_string(params, "text")?;
    let path = optional_string(params, "path")?;
    let text = match (text, path) {
        (Some(text), None) => text.to_string(),
        (None, Some(path)) => read_hull_file(world, path)?,
        _ => {
            return Err(RpcError::invalid_params("give exactly one of `text` or `path`"));
        }
    };
    let asset = HoverHullAsset::from_ron(&text).map_err(hull_error)?;
    checked_json(&asset).map_err(hull_error)
}

fn optional_string<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(RpcError::wrong_type(key, "a string")),
    }
}

fn read_hull_file(world: &World, path: &str) -> Result<String, RpcError> {
    let confined = crate::render_out::confine_path(world, "path", Path::new(path))?;
    let meta = std::fs::metadata(&confined)
        .map_err(|err| hull_error(HoverHullError::Parse(format!("cannot read the hull file: {err}"))))?;
    if !meta.is_file() {
        return Err(hull_error(HoverHullError::Parse(
            "path is not a file".to_string(),
        )));
    }
    if meta.len() > MAX_TEXT as u64 {
        return Err(hull_error(HoverHullError::Parse(format!(
            "file is {} bytes (at most {MAX_TEXT})",
            meta.len()
        ))));
    }
    let bytes = std::fs::read(&confined)
        .map_err(|err| hull_error(HoverHullError::Parse(format!("cannot read the hull file: {err}"))))?;
    if bytes.len() > MAX_TEXT {
        return Err(hull_error(HoverHullError::Parse(format!(
            "file is {} bytes (at most {MAX_TEXT})",
            bytes.len()
        ))));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| hull_error(HoverHullError::Parse("hover hull file is not UTF-8".to_string())))?;
    Ok(text.to_string())
}

fn checked_json(asset: &HoverHullAsset) -> Result<Value, HoverHullError> {
    let handshake = asset
        .clone()
        .into_parts()?
        .handshake_bytes(&HoverLimits::DEFAULT, FlightMode::default());
    Ok(json!({
        "ok": true,
        "name": asset.name,
        "format_version": asset.format_version,
        "flight": asset.flight,
        "notes": asset.notes,
        "limits": asset.limits,
        "params": asset.params,
        "bytes_hex": bytes_hex(&asset.params.to_le_bytes()),
        "handshake_hex": bytes_hex(&handshake),
    }))
}

fn bytes_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn hull_error(error: HoverHullError) -> RpcError {
    let mut err = RpcError::invalid_params(format!("[{}] {error}", error.code()));
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("format_code".into(), Value::String(error.code().into()));
        if let Some(field) = error.field_name() {
            data.insert("field".into(), Value::String(field.to_string()));
        }
    }
    err
}

#[cfg(test)]
mod tests {
    use regia_test_utils::{TempDir, write_atomic};
    use serde_json::json;

    use super::*;
    use crate::{AgentSettings, LocalReply, call_local};

    fn call(app: &mut App, params: Value) -> Result<Value, RpcError> {
        match call_local(app.world_mut(), "detsim.hover_hull_check", params, None) {
            LocalReply::Done(result) => result,
            LocalReply::Pending(_) => panic!("detsim.hover_hull_check deferred"),
        }
    }

    fn format_code(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|data| data.get("format_code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn detsim_hover_hull_check_resolves_text_and_path() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);

        let ok = call(
            &mut app,
            json!({
                "text": "(format_version: 1, name: \"scout\", params: (ride_height: -0.0))",
                "path": null
            }),
        )
        .expect("text");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["name"], "scout");
        assert_eq!(ok["format_version"], 1);
        assert!(ok["limits"].is_null());
        let hex = ok["bytes_hex"].as_str().expect("bytes_hex");
        assert!(hex.starts_with("00000080"), "{hex}");
        assert_eq!(hex.len(), regia_detsim::hover::HoverParams::BYTES * 2);
        let handshake = ok["handshake_hex"].as_str().expect("handshake_hex");
        assert_eq!(handshake.len(), regia_detsim::hover::HULL_HANDSHAKE_BYTES * 2);
        assert!(handshake.starts_with(hex), "params first: {handshake}");
        assert_eq!(&handshake[360..362], "00", "air_time when the file names no mode");

        // Same params, other limits or flight mode: `bytes_hex` matches, `handshake_hex` does not.
        let mut hull = |body: &str| {
            let text = format!("(format_version: 1, name: \"h\"{body})");
            call(&mut app, json!({ "text": text })).expect("hull")
        };
        let plain = hull("");
        for other in [
            hull(", limits: (hop_apex_m: 10.0)"),
            hull(", limits: (roll_lean_max: -0.0)"),
            hull(", flight: Some(\"clamped\")"),
        ] {
            assert_eq!(other["bytes_hex"], plain["bytes_hex"]);
            assert_ne!(other["handshake_hex"], plain["handshake_hex"]);
        }
        assert_eq!(
            &hull(", flight: Some(\"clamped\")")["handshake_hex"]
                .as_str()
                .expect("hex")[360..362],
            "01"
        );
        assert_eq!(hull(", limits: ()")["handshake_hex"], plain["handshake_hex"]);

        let finite = call(
            &mut app,
            json!({ "text": "(format_version: 1, name: \"hull\", params: (ride_height: NaN))" }),
        )
        .expect_err("nan");
        assert_eq!(format_code(&finite), Some("not_finite"));
        let data = finite.data.as_ref().expect("error data");
        assert_eq!(data["code"], "F0002");
        assert_eq!(data["field"], "ride_height");

        let neither = call(&mut app, json!({})).expect_err("neither");
        assert!(neither.message.contains("exactly one"));
        assert!(format_code(&neither).is_none());

        let both = call(
            &mut app,
            json!({ "text": "(format_version: 1, name: \"a\")", "path": "a.hoverhull.ron" }),
        )
        .expect_err("both");
        assert!(both.message.contains("exactly one"));

        let wrong = call(&mut app, json!({ "text": 1 })).expect_err("type");
        assert_eq!(wrong.data.as_ref().expect("error data")["param"], "text");

        let dir = TempDir::new("hoverhull-check");
        let file = dir.join("scout.hoverhull.ron");
        write_atomic(
            &file,
            b"(format_version: 1, name: \"from-path\", params: (ride_height: 2.0))\n",
        )
        .expect("write hull");
        app.insert_resource(AgentSettings {
            project_dir: Some(dir.path().to_path_buf()),
        });
        let from_path = call(&mut app, json!({ "path": "scout.hoverhull.ron" })).expect("path");
        assert_eq!(from_path["name"], "from-path");
        assert_eq!(from_path["params"]["ride_height"], 2.0);

        let bad = dir.join("bad.hoverhull.ron");
        write_atomic(&bad, &[0xff, 0xfe]).expect("write bytes");
        let utf8 = call(&mut app, json!({ "path": "bad.hoverhull.ron" })).expect_err("utf8");
        assert_eq!(format_code(&utf8), Some("parse_error"));
    }
}
