// SPDX-License-Identifier: MPL-2.0
//! `viewer.view_simulate` and `viewer.pack_verify`: the product viewer's drag view and packages
//! (track JW, ADR-2540, `book/agents/viewer-view.md`, `book/agents/viewer-pack.md`).
//!
//! Both are pure reads: `view_simulate` runs a scripted input sequence through a fresh drag view
//! (no world state), `pack_verify` reads one package folder inside the project. A damaged package
//! is a result with `ok: false` and the refusal code, not a call error; a malformed script or a
//! path outside the project answers `F0002`.

use regia_app::prelude::World;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn refused(code: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": code,
            "hint": "see book/agents/viewer-view.md for the script format and the limits",
        })),
    }
}

/// `viewer.view_simulate { recipe? | view_range?, camera_default?, viewport?, events? }`.
pub fn view_simulate(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !params.is_object() {
        return Err(refused("view_bad_event", "params is an object (the script)"));
    }
    // The simulator reads a non-object `recipe` as absent (the defaults): refuse it instead
    // (ADR-2622). Its other parameters are refused with view reasons there.
    crate::params::opt_object(params, "recipe")?;
    regia_viewer_view::simulate(params).map_err(|e| refused(e.code, e.message))
}

/// `viewer.pack_verify { dir }`: verify a package folder (project-relative).
pub fn pack_verify(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(rel) = params.get("dir").and_then(Value::as_str) else {
        return Err(RpcError::invalid_params(
            "`dir` (a project-relative package folder) is required",
        ));
    };
    let dir = crate::req_path::project_path(world, "dir", rel)?;
    match regia_viewer_pack::read_dir(&dir) {
        Ok(pkg) => {
            let manifest_bytes = std::fs::metadata(dir.join(regia_viewer_pack::MANIFEST_FILE))
                .map(|m| m.len())
                .unwrap_or(0);
            let report = regia_viewer_pack::SizeReport::of(&pkg, manifest_bytes);
            let meshes: Vec<Value> = pkg
                .meshes
                .iter()
                .map(|m| {
                    json!({"name": m.name, "material_slot": m.material_slot,
                                "vertices": m.positions.len(), "triangles": m.triangles()})
                })
                .collect();
            let groups: Vec<Value> = pkg.options["groups"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|g| {
                    json!({"group": g["group"], "visual": g["visual"],
                                "values": g["values"].as_array().map_or(0, Vec::len)})
                })
                .collect();
            Ok(json!({
                "ok": true,
                "package_id": pkg.manifest.doc["package_id"],
                "package_hash": pkg.manifest.package_hash,
                "format_version": regia_viewer_pack::FORMAT_VERSION,
                "meshes": meshes,
                "groups": groups,
                "view_range": pkg.recipe.get("view_range"),
                "report": report.to_json(),
            }))
        }
        Err(e) => Ok(json!({"ok": false, "reason": e.code, "message": e.message})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulate_answers_and_refuses_with_reasons() {
        let mut world = World::new();
        let out =
            view_simulate(&mut world, &json!({"events": [{"wheel": 3}, {"settle": true}]})).expect("ok");
        assert!(out["shown"]["zoom"].as_f64().unwrap_or(0.0) > 1.0);
        let e = view_simulate(&mut world, &json!({"events": [{"warp": 1}]})).expect_err("refused");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("view_bad_event"))
        );
        let e = view_simulate(&mut world, &json!({"view_range": {"spin": "wobble"}})).expect_err("refused");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("view_range_bad_spin"))
        );
    }

    #[test]
    fn a_recipe_that_is_not_an_object_is_refused_not_defaulted() {
        let mut world = World::new();
        let e = view_simulate(&mut world, &json!({"recipe": "not an object"})).expect_err("refused");
        assert_eq!(e.code, crate::protocol::INVALID_PARAMS);
        assert_eq!(e.data.as_ref().map(|d| d["param"].clone()), Some(json!("recipe")));
        assert!(
            view_simulate(&mut world, &json!({"recipe": null})).is_ok(),
            "null is absent"
        );
    }

    #[test]
    fn pack_verify_refuses_paths_outside_the_project() {
        let mut world = World::new();
        let e = pack_verify(&mut world, &json!({"dir": "../outside"})).expect_err("refused");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("path_outside_project"))
        );
        let missing = pack_verify(&mut world, &json!({"dir": "no-such-package-dir"})).expect("result");
        assert_eq!(missing["ok"], false);
        assert_eq!(missing["reason"], "pack_missing_file");
    }
}
