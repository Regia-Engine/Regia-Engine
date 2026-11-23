// SPDX-License-Identifier: MPL-2.0
//! `anim.gait_validate` (gap G7): check a `.gait.ron` gait settings file without loading it into a
//! rig. The checks are the loader's own (`regia_animation::gait_asset::GaitAsset::from_ron`): the
//! version first, then the typed parse, the leg list and the stepper's `GaitParams::validate`, so an
//! agent sees the codes a running game would log.

use regia_animation::gait_asset::{GAIT_FORMAT_VERSION, GaitAsset, GaitAssetError};
use regia_ecs::prelude::World;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Largest `text` accepted, in bytes (a gait file is a few kilobytes).
pub(crate) const MAX_GAIT_TEXT: usize = 1 << 20;

/// `anim.gait_validate { path | text }`: `{ path, ok, format_version, legs, patterns, errors: [{ code,
/// message }] }`. A file that does not validate is an answer (`ok: false`), not an RPC error.
pub(crate) fn gait_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text_param = crate::params::opt_str(params, "text")?;
    let path_param = crate::params::opt_str(params, "path")?;
    let (label, text) = match (text_param, path_param) {
        (Some(text), _) => {
            if text.len() > MAX_GAIT_TEXT {
                return Err(RpcError::invalid_params(format!(
                    "text is {} bytes; the limit is {MAX_GAIT_TEXT}",
                    text.len()
                )));
            }
            (Value::Null, Ok(text.to_string()))
        }
        (None, Some(rel)) => {
            let full = crate::req_path::project_file(world, "path", rel)?;
            let read =
                std::fs::read_to_string(&full).map_err(|error| GaitAssetError::Io(format!("{rel}: {error}")));
            (json!(rel), read)
        }
        (None, None) => {
            return Err(RpcError::invalid_params(
                "pass `path` (a project-relative .gait.ron) or `text` (its contents)",
            ));
        }
    };
    let checked = text.and_then(|text| GaitAsset::from_ron(&text));
    Ok(match checked {
        Ok(asset) => json!({
            "path": label,
            "ok": true,
            "format_version": GAIT_FORMAT_VERSION,
            "legs": asset.params.legs(),
            "patterns": asset.params.patterns.count,
            "errors": [],
        }),
        Err(error) => json!({
            "path": label,
            "ok": false,
            "format_version": Value::Null,
            "legs": Value::Null,
            "patterns": Value::Null,
            "errors": [{ "code": error.code(), "message": error.to_string() }],
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = include_str!("../tests/fixtures/format-corpus/gait/v1.ron");

    fn call(params: Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        gait_validate(&mut world, &params)
    }

    #[test]
    fn a_good_text_validates() {
        let out = call(json!({ "text": GOOD })).expect("answers");
        assert_eq!(out["ok"], true);
        assert_eq!(out["legs"], 6);
        assert_eq!(out["patterns"], 1);
        assert_eq!(out["errors"], json!([]));
    }

    #[test]
    fn errors_carry_the_loader_codes() {
        for (edit, code) in [
            (
                GOOD.replacen("step_length: 0.6", "step_length: -1.0", 1),
                "gait_settings",
            ),
            (
                GOOD.replacen("format_version: 1", "format_version: 9", 1),
                "gait_version",
            ),
            (
                GOOD.replacen("patterns: [Tripod]", "patterns: [Trot]", 1),
                "gait_legs",
            ),
            (
                GOOD.replacen("patterns: [Tripod]", "patterns: [Wave(3)]", 1),
                "gait_pattern",
            ),
            ("(format_version: 1, bogus: 1)".to_string(), "gait_parse"),
        ] {
            let out = call(json!({ "text": edit })).expect("answers");
            assert_eq!(out["ok"], false, "{code}");
            assert_eq!(out["errors"][0]["code"], code, "{out}");
        }
    }

    #[test]
    fn neither_path_nor_text_is_invalid_params() {
        assert!(call(json!({})).is_err());
    }
}
