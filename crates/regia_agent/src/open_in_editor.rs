// SPDX-License-Identifier: MPL-2.0
//! `agent.open_in_editor` (`docs/specs/23-agent-native.md` § 3.6, ADR-2346): asks the running
//! editor to show something to the humans watching an agent work.
//!
//! The runtime has no editor, so the method answers `editor_not_running` unless the host installed
//! an [`OpenInEditorHook`] (the editor does, in `regia_editor::agent_ext::install`). The request is
//! validated here, so every host sees the same checked request: exactly one of `entity`, `asset`
//! or `file`; paths project-relative and confined to the project (`req_path`).
//!
//! Opening a `file` runs the owner's code editor. The editor host
//! (`regia_editor` log links, `code_editor_command`, called from `open_target`) tries the
//! preference `code.editor_command` first, then `[editor] code_editor` in `.regia/agent.toml`
//! (`["code", "-g", "{file}:{line}"]`). `{file}` (the absolute path) and `{line}` are substituted
//! per argument, never through a shell. `open_in_editor_no_code_editor` is answered only when both
//! are unset. The helper in this module reads only the project file; the preference check is the
//! host's. Covered by `agent_open_in_editor_reads_the_preference_before_the_project_config`.

use std::path::Path;

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::jobs::reason_error;
use crate::protocol::{INVALID_PARAMS, RpcError, UNSUPPORTED};

/// A checked open request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// Select this entity (its bits).
    Entity(u64),
    /// Open this project-relative asset.
    Asset(String),
    /// Open this project-relative file at a 1-based line in the code editor.
    File {
        /// The project-relative path.
        path: String,
        /// The 1-based line.
        line: u64,
    },
}

/// The host's handler: opens a checked target (`dry_run` reports without opening).
pub type OpenHandler = fn(&mut World, &OpenTarget, bool) -> Result<Value, RpcError>;

/// Installed by an editor host so `agent.open_in_editor` reaches it.
#[derive(Resource, Clone, Copy)]
pub struct OpenInEditorHook(pub OpenHandler);

/// Reads and checks the request.
///
/// # Errors
/// `F0002` when not exactly one target is named or a path leaves the project.
pub fn parse_target(world: &World, params: &Value) -> Result<OpenTarget, RpcError> {
    let entity = params.get("entity");
    let asset = params.get("asset");
    let file = params.get("file");
    let named = [entity, asset, file].iter().filter(|v| v.is_some()).count();
    if named != 1 {
        return Err(RpcError::invalid_params(
            "name exactly one of `entity`, `asset` or `file`",
        ));
    }
    let root = crate::req_path::project_root(world);
    if let Some(entity) = entity {
        return entity
            .as_u64()
            .map(OpenTarget::Entity)
            .ok_or_else(|| RpcError::invalid_params("`entity` is an entity id (integer)"));
    }
    if let Some(asset) = asset {
        let Some(rel) = asset.as_str().filter(|s| !s.is_empty()) else {
            return Err(RpcError::invalid_params("`asset` is a project-relative path"));
        };
        crate::req_path::project_path_in(&root, "asset", rel)?;
        return Ok(OpenTarget::Asset(rel.replace('\\', "/")));
    }
    let Some(rel) = file.and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Err(RpcError::invalid_params("`file` is a project-relative path"));
    };
    crate::req_path::project_path_in(&root, "file", rel)?;
    let line = params
        .get("line")
        .map_or(Some(1), Value::as_u64)
        .filter(|l| *l >= 1);
    let Some(line) = line else {
        return Err(RpcError::invalid_params("`line` is a 1-based line number"));
    };
    Ok(OpenTarget::File {
        path: rel.replace('\\', "/"),
        line,
    })
}

/// The code editor command for `file` at `line`, from `[editor] code_editor` in the project's
/// `.regia/agent.toml`.
///
/// # Errors
/// `open_in_editor_no_code_editor` when none is configured (or the file cannot be read).
pub fn code_editor_command(root: &Path, file: &Path, line: u64) -> Result<Vec<String>, RpcError> {
    let config = regia_core::brand::project_state_dir(root).join(crate::safety::AGENT_TOML);
    let template = std::fs::read_to_string(&config)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            let list = table.get("editor")?.get("code_editor")?.as_array()?.clone();
            list.iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
        })
        .filter(|list| list.first().is_some_and(|program| !program.is_empty()));
    let Some(template) = template else {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "open_in_editor_no_code_editor",
            "no code editor is configured: set [editor] code_editor = [\"<program>\", \"{file}:{line}\"] in .regia/agent.toml",
        ));
    };
    let file = file.display().to_string();
    let line = line.to_string();
    Ok(template
        .iter()
        .map(|arg| arg.replace("{file}", &file).replace("{line}", &line))
        .collect())
}

/// `agent.open_in_editor`.
pub(crate) fn open_in_editor(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = parse_target(world, params)?;
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let Some(OpenInEditorHook(handler)) = world.get_resource::<OpenInEditorHook>().copied() else {
        return Err(reason_error(
            UNSUPPORTED,
            "F0002",
            "editor_not_running",
            "this app is not the editor; connect to a running editor (regia mcp attaches to the editor's endpoint)",
        ));
    };
    let mut answer = handler(world, &target, dry_run)?;
    if dry_run && let Some(object) = answer.as_object_mut() {
        object.insert("dry_run".into(), json!(true));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_at(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    #[test]
    fn exactly_one_target_and_confined_paths() {
        let root = std::env::temp_dir();
        let world = world_at(&root);
        assert_eq!(
            parse_target(&world, &json!({ "entity": 7 })),
            Ok(OpenTarget::Entity(7))
        );
        assert_eq!(
            parse_target(&world, &json!({ "asset": "prefabs/door.prefab.ron" })),
            Ok(OpenTarget::Asset("prefabs/door.prefab.ron".into()))
        );
        assert_eq!(
            parse_target(&world, &json!({ "file": "src/lib.rs", "line": 12 })),
            Ok(OpenTarget::File {
                path: "src/lib.rs".into(),
                line: 12
            })
        );
        for bad in [
            json!({}),
            json!({ "entity": 1, "asset": "a" }),
            json!({ "asset": "../outside.ron" }),
            json!({ "file": "/etc/passwd" }),
            json!({ "file": "src/lib.rs", "line": 0 }),
            json!({ "entity": "seven" }),
        ] {
            assert!(parse_target(&world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn without_an_editor_the_method_says_so() {
        let mut world = world_at(&std::env::temp_dir());
        let error = open_in_editor(&mut world, &json!({ "entity": 1 })).expect_err("no editor");
        assert_eq!(
            error.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "editor_not_running"
        );
    }

    #[test]
    fn the_hook_receives_the_checked_target() {
        fn hook(_: &mut World, target: &OpenTarget, dry_run: bool) -> Result<Value, RpcError> {
            Ok(json!({ "opened": format!("{target:?}"), "was_dry": dry_run }))
        }
        let mut world = world_at(&std::env::temp_dir());
        world.insert_resource(OpenInEditorHook(hook));
        let answer = open_in_editor(&mut world, &json!({ "entity": 3, "dry_run": true })).expect("opened");
        assert_eq!(answer["opened"], "Entity(3)");
        assert_eq!(answer["dry_run"], true);
    }

    #[test]
    fn the_code_editor_command_comes_from_the_project_config_only() {
        let root = regia_test_utils::unique_temp_dir("open-in-editor");
        let file = root.join("src/lib.rs");
        let none = code_editor_command(&root, &file, 3).expect_err("unconfigured");
        assert_eq!(
            none.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "open_in_editor_no_code_editor"
        );
        let state = regia_core::brand::project_state_dir(&root);
        std::fs::create_dir_all(&state).expect("state dir");
        std::fs::write(
            state.join(crate::safety::AGENT_TOML),
            "[editor]\ncode_editor = [\"code\", \"-g\", \"{file}:{line}\"]\n",
        )
        .expect("config");
        let command = code_editor_command(&root, &file, 3).expect("configured");
        assert_eq!(
            command,
            ["code".to_string(), "-g".into(), format!("{}:3", file.display())]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
