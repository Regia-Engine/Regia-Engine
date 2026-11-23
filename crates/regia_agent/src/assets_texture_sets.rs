// SPDX-License-Identifier: MPL-2.0
//! `assets.texture_sets` (spec 22 T2.14, ADR-3320, `book/agents/assets.md`): detect PBR
//! texture sets in a project folder and generate one `.material.ron` per set — the agent
//! surface of `regia assets texture-sets`. Detection runs against the project's
//! `Regia.toml` `[import.texture_sets]` table; nothing is written unless `write: true`, and
//! an existing material is never touched.

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `assets.texture_sets`: `{ folder, write?, recursive? }` -> the set report.
///
/// Error codes: `F0002` (a parameter is missing or of the wrong type, or `folder` escapes the
/// project — `data.reason` `path_outside_project`), `F0003` (`write: true` failed on disk,
/// `data.path` names the file), `F0016` (the folder does not exist under the project).
pub(crate) fn texture_sets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let folder = crate::params::opt_str(params, "folder")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`folder` is required (a project-relative folder)"))?
        .to_string();
    let write = crate::params::opt_bool(params, "write")?.unwrap_or(false);
    let recursive = crate::params::opt_bool(params, "recursive")?.unwrap_or(false);
    let root = crate::req_path::project_root(world);
    let full = crate::req_path::project_path(world, "folder", &folder)?;
    if !full.is_dir() {
        return Err(RpcError::new(
            crate::protocol::NOT_FOUND,
            "F0016",
            format!("no folder {folder} under the project"),
            "check the path with assets.list, or create the folder first",
        ));
    }
    let settings = regia_asset::texture_sets::TextureSetSettings::for_project(&root)
        .map_err(|e| RpcError::internal(format!("Regia.toml [import.texture_sets]: {e}")))?;
    let scan = regia_asset::texture_sets::scan(&full, recursive, &settings)
        .map_err(|e| RpcError::internal(format!("scanning {folder}: {e}")))?;
    let sets = regia_asset::texture_sets::generate(&scan, &settings, write).map_err(|e| write_error(&e))?;
    Ok(json!({
        "folder": folder,
        "write": write,
        "sets": sets,
        "leftovers": scan.leftovers,
    }))
}

/// The `F0003` disk error of a failed write, with the path and reason.
fn write_error(error: &regia_asset::texture_sets::TextureSetError) -> RpcError {
    RpcError {
        code: crate::protocol::INTERNAL_ERROR,
        message: format!("texture set generation failed: {error}"),
        data: Some(serde_json::json!({
            "code": "F0003",
            "path": error.path.display().to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    #[test]
    fn texture_sets_detects_and_writes_in_a_project_folder() {
        let dir = regia_test_utils::unique_temp_dir("agent-texset");
        let folder = dir.join("assets/rocks");
        std::fs::create_dir_all(&folder).expect("folder");
        let png = |name: &str| {
            let mut rgba = Vec::with_capacity(4 * 4 * 4);
            for i in 0..16u8 {
                rgba.extend_from_slice(&[i, 0, 0, 255]);
            }
            let mut bytes = Vec::new();
            image::codecs::png::PngEncoder::new(&mut bytes)
                .write_image(&rgba, 4, 4, image::ExtendedColorType::Rgba8)
                .expect("png");
            std::fs::write(folder.join(name), bytes).expect("write");
        };
        png("boulder_BaseColor.png");
        png("boulder_Normal.png");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });

        // Dry run: nothing on disk, the report shows both slots without ids.
        let dry = texture_sets(&mut world, &json!({ "folder": "assets/rocks" })).expect("dry run");
        assert_eq!(dry["sets"][0]["name"], "boulder");
        assert!(!dry["sets"][0]["written"].as_bool().expect("bool"));
        assert!(!folder.join("boulder.material.ron").exists());

        // Write: the material and the member metas appear, and a second run reports the
        // existing material untouched.
        let done =
            texture_sets(&mut world, &json!({ "folder": "assets/rocks", "write": true })).expect("write");
        assert!(done["sets"][0]["written"].as_bool().expect("bool"));
        assert!(folder.join("boulder.material.ron").is_file());
        assert!(regia_asset::meta_path_for(&folder.join("boulder_BaseColor.png")).is_file());
        let again =
            texture_sets(&mut world, &json!({ "folder": "assets/rocks", "write": true })).expect("again");
        assert!(again["sets"][0]["material_exists"].as_bool().expect("bool"));
        assert!(!again["sets"][0]["written"].as_bool().expect("bool"));

        // Errors: a missing folder, an escaping path and a wrong parameter type.
        let missing = texture_sets(&mut world, &json!({ "folder": "assets/none" })).expect_err("no folder");
        assert_eq!(missing.data.as_ref().expect("data")["code"], "F0016");
        let escape = texture_sets(&mut world, &json!({ "folder": "../out" })).expect_err("escape");
        assert_eq!(escape.data.as_ref().expect("data")["code"], "F0002");
        let bad = texture_sets(&mut world, &json!({ "folder": 3 })).expect_err("bad type");
        assert_eq!(bad.code, crate::protocol::INVALID_PARAMS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
