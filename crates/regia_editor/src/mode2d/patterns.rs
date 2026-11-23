// SPDX-License-Identifier: MPL-2.0
//! Saved tile patterns (ADR-7853): the stamp saved under a name in the tileset's
//! `.tilepatterns.ron` sidecar (`regia_2d::tilepatterns`, next to the tileset image), a pattern
//! palette in the 2D tab, and `editor.tile_pattern_list` / `_save` / `_use` / `_delete`.
//!
//! The library is its own small document: save and delete write the sidecar at once (atomically);
//! they are not on the map's undo stack. Using a pattern loads it into the stamp and selects the
//! stamp brush.

use std::path::{Path, PathBuf};

use regia_2d::tilemap::GID_MASK;
use regia_2d::tilepatterns::{TilePattern, TilePatternLibrary, tilepatterns_path};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::{Brush2d, Mode2d, Mode2dError, mode2d_error, param};
use crate::viewport_agent::Handler;

/// The library of one tileset, as loaded from (or about to be written to) its sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternDoc {
    /// The tileset's index in the map.
    pub tileset: usize,
    /// The sidecar path.
    pub path: PathBuf,
    /// The patterns.
    pub library: TilePatternLibrary,
}

/// Largest sidecar read, bytes.
const MAX_PATTERN_FILE: u64 = 16 << 20;

fn read(path: &Path, tileset: &str) -> Result<TilePatternLibrary, Mode2dError> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TilePatternLibrary::new(tileset)),
        Err(e) => return Err(Mode2dError::Patterns(format!("{}: {e}", path.display()))),
        Ok(meta) if meta.len() > MAX_PATTERN_FILE => {
            return Err(Mode2dError::Patterns(format!(
                "{} is larger than {MAX_PATTERN_FILE} bytes",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Mode2dError::Patterns(format!("{}: {e}", path.display())))?;
    TilePatternLibrary::from_ron(&text).map_err(|e| Mode2dError::Patterns(format!("{}: {e}", path.display())))
}

fn write(doc: &PatternDoc) -> Result<(), Mode2dError> {
    let text = doc
        .library
        .to_ron()
        .map_err(|e| Mode2dError::Patterns(e.to_string()))?;
    crate::atomic_write::write_atomic(&doc.path, text.as_bytes()).map_err(Mode2dError::Patterns)
}

impl Mode2d {
    /// The tileset index named `name`, or (no name) the tileset of the stamp's first tile, or 0.
    fn pattern_tileset(&self, name: Option<&str>) -> Result<usize, Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        if let Some(name) = name {
            return regia_2d::tilepatterns::tileset_named(&doc.map, name)
                .map(|(i, _)| i)
                .ok_or_else(|| Mode2dError::BadParams(format!("the map has no tileset `{name}`")));
        }
        let from_stamp = self.stamp.as_ref().and_then(|s| {
            s.cells
                .iter()
                .find(|&&raw| raw & GID_MASK != 0)
                .and_then(|&raw| doc.map.tileset_for(raw & GID_MASK))
                .map(|(i, _)| i)
        });
        match from_stamp {
            Some(i) => Ok(i),
            None if doc.map.tilesets.is_empty() => {
                Err(Mode2dError::BadParams("the map has no tileset".into()))
            }
            None => Ok(0),
        }
    }

    /// The pattern library of tileset `tileset` (read from its sidecar once, then kept).
    pub fn pattern_library(&mut self, tileset: usize) -> Result<&PatternDoc, Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        let set = doc
            .map
            .tilesets
            .get(tileset)
            .ok_or_else(|| Mode2dError::BadParams(format!("no tileset {tileset}")))?;
        let path = tilepatterns_path(&doc.path, set);
        if self
            .patterns
            .as_ref()
            .is_none_or(|p| p.tileset != tileset || p.path != path)
        {
            // A broken sidecar of another tileset must not leave the previous library on screen
            // (review g17 row 7): the cache goes with the error.
            let library = match read(&path, &set.name) {
                Ok(library) => library,
                Err(e) => {
                    self.patterns = None;
                    return Err(e);
                }
            };
            self.patterns = Some(PatternDoc {
                tileset,
                path,
                library,
            });
        }
        self.patterns.as_ref().ok_or(Mode2dError::NoMap)
    }

    /// Saves the stamp as pattern `name` of its tileset and writes the sidecar.
    pub fn save_pattern(&mut self, name: &str, tileset: Option<&str>) -> Result<(), Mode2dError> {
        let index = self.pattern_tileset(tileset)?;
        let stamp = self.stamp.clone().ok_or(Mode2dError::NoStamp)?;
        let set = self
            .doc
            .as_ref()
            .and_then(|d| d.map.tilesets.get(index))
            .cloned()
            .ok_or(Mode2dError::NoMap)?;
        let pattern =
            TilePattern::from_stamp(name, &stamp, &set).map_err(|e| Mode2dError::Patterns(e.to_string()))?;
        let mut doc = self.pattern_library(index)?.clone();
        doc.library
            .put(pattern)
            .map_err(|e| Mode2dError::Patterns(e.to_string()))?;
        write(&doc)?;
        self.patterns = Some(doc);
        Ok(())
    }

    /// Loads pattern `name` into the stamp and selects the stamp brush.
    pub fn use_pattern(&mut self, name: &str, tileset: Option<&str>) -> Result<(), Mode2dError> {
        let index = self.pattern_tileset(tileset)?;
        let pattern = self
            .pattern_library(index)?
            .library
            .get(name)
            .cloned()
            .ok_or_else(|| Mode2dError::NoPattern(name.into()))?;
        let set = self
            .doc
            .as_ref()
            .and_then(|d| d.map.tilesets.get(index))
            .ok_or(Mode2dError::NoMap)?;
        let stamp = pattern
            .to_stamp(set)
            .map_err(|e| Mode2dError::Patterns(e.to_string()))?;
        self.stamp = Some(stamp);
        self.brush = Brush2d::Stamp;
        Ok(())
    }

    /// Deletes pattern `name` and writes the sidecar.
    pub fn delete_pattern(&mut self, name: &str, tileset: Option<&str>) -> Result<(), Mode2dError> {
        let index = self.pattern_tileset(tileset)?;
        let mut doc = self.pattern_library(index)?.clone();
        if !doc.library.remove(name) {
            return Err(Mode2dError::NoPattern(name.into()));
        }
        write(&doc)?;
        self.patterns = Some(doc);
        Ok(())
    }
}

/// The pattern list of a library.
#[must_use]
pub fn patterns_json(doc: &PatternDoc) -> Value {
    json!({
        "tileset": doc.library.tileset,
        "path": doc.path.display().to_string(),
        "patterns": doc.library.patterns.iter().map(|p| json!({ "name": p.name, "width": p.width, "height": p.height })).collect::<Vec<_>>(),
    })
}

/// Runs one pattern action (`list`, `save`, `use`, `delete`) and returns the library's list.
pub fn pattern_action(mode: &mut Mode2d, action: &str, params: &Value) -> Result<Value, Mode2dError> {
    let tileset = params.get("tileset").and_then(Value::as_str);
    let name = || {
        params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Mode2dError::BadParams("`name` is the pattern's name".into()))
    };
    match action {
        "list" => {}
        "save" => mode.save_pattern(name()?, tileset)?,
        "use" => mode.use_pattern(name()?, tileset)?,
        "delete" => mode.delete_pattern(name()?, tileset)?,
        other => {
            return Err(Mode2dError::BadParams(format!(
                "unknown pattern action `{other}`"
            )));
        }
    }
    let index = mode.pattern_tileset(tileset)?;
    let mut out = patterns_json(mode.pattern_library(index)?);
    out["brush"] = super::brush_json(&mode.brush);
    Ok(out)
}

fn handler(action: &'static str) -> impl Fn(&mut World, &Value) -> Result<Value, RpcError> {
    move |world, params| {
        let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
        let result = pattern_action(&mut mode, action, params);
        world.insert_resource(mode);
        result.map_err(|e| mode2d_error(&e))
    }
}

fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    handler("list")(world, params)
}
fn save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    handler("save")(world, params)
}
fn use_(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    handler("use")(world, params)
}
fn delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    handler("delete")(world, params)
}

const LIST_PARAMS: &[ParamSchema] = &[param(
    "tileset",
    "string",
    false,
    "The tileset's name (default: the stamp's tileset, else the first).",
)];
const NAMED_PARAMS: &[ParamSchema] = &[
    param("name", "string", true, "The pattern's name (unique per tileset)."),
    param(
        "tileset",
        "string",
        false,
        "The tileset's name (default: the stamp's tileset, else the first).",
    ),
];

/// The result shape of every pattern method.
const RESULT: &str = "{ tileset, path, patterns: [{ name, width, height }], brush }";

/// The pattern methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.tile_pattern_list",
                tool: "tile_pattern_list",
                description: "Lists the saved tile patterns of a tileset of the 2D mode's map (its .tilepatterns.ron \
                    next to the tileset image). Errors in data.mode2d_error: mode2d_bad_params, mode2d_no_map, \
                    mode2d_bad_patterns.",
                params: LIST_PARAMS,
                result: RESULT,
                mutating: false,
                permission: Permission::Read,
            },
            list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tile_pattern_save",
                tool: "tile_pattern_save",
                description: "Saves the 2D mode's stamp as a named pattern of its tileset (replacing one of that name) \
                    and writes the tileset's .tilepatterns.ron at once (not on the undo stack). Errors in \
                    data.mode2d_error: mode2d_bad_params, mode2d_no_map, mode2d_no_stamp, mode2d_bad_patterns.",
                params: NAMED_PARAMS,
                result: RESULT,
                mutating: true,
                permission: Permission::EditScene,
            },
            save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tile_pattern_use",
                tool: "tile_pattern_use",
                description: "Loads a saved pattern into the 2D mode's stamp and selects the stamp brush (paint it \
                    with editor.tile_stroke). Errors in data.mode2d_error: mode2d_bad_params, mode2d_no_map, \
                    mode2d_no_pattern, mode2d_bad_patterns.",
                params: NAMED_PARAMS,
                result: RESULT,
                mutating: true,
                permission: Permission::EditScene,
            },
            use_ as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tile_pattern_delete",
                tool: "tile_pattern_delete",
                description: "Deletes a saved pattern and writes the tileset's .tilepatterns.ron at once. Errors in \
                    data.mode2d_error: mode2d_bad_params, mode2d_no_map, mode2d_no_pattern, mode2d_bad_patterns.",
                params: NAMED_PARAMS,
                result: RESULT,
                mutating: true,
                permission: Permission::EditScene,
            },
            delete as Handler,
        ),
    ]
}
