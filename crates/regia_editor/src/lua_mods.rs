// SPDX-License-Identifier: MPL-2.0
//! Lua script mods in the edit world (ADR-3356): the editor loads a project's `mods/<id>/`
//! directories when the project opens, with the directory watch on, so editing a script hot
//! reloads it (reconciling per-entity component values) while the level stays open. The loaded
//! mods' declarations drive the inspector's script-component drawer; the `lua.*` agent methods
//! answer against the edit world.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_script_lua::{EcsModWorld, LuaMods, LuaModsPlugin, ModFiles};

/// Installs the Lua mods host into the edit world: no mods yet, the directory watch on (a look
/// every `WATCH_EVERY_FRAMES` frames), the `lua.*` agent methods and the reflected script
/// component types registered.
pub fn install(app: &mut App) {
    app.add_plugins(LuaModsPlugin::new(Vec::new()).with_watch(WATCH_EVERY_FRAMES));
}

/// Frames between two looks at the mods' directories (development hot reload).
const WATCH_EVERY_FRAMES: u32 = 60;

/// Loads every mod in `<project>/mods/<dir>/` (a directory holding `luamod.ron`), in directory
/// order. A mod that fails logs a warning and is kept, disabled, like a game load. Idempotent in
/// practice: a mod whose id is already loaded is skipped, so reopening a project does not double
/// the declarations.
pub fn load_project_mods(world: &mut World, project: &Path) {
    let mods_dir = project.join("mods");
    let Ok(entries) = std::fs::read_dir(&mods_dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    let mut loaded: Vec<String> = Vec::new();
    world.try_resource_scope::<LuaMods, _>(|world, mut mods| {
        let max_bytes = mods.limits().max_mod_bytes;
        for dir in dirs {
            let Ok(files) = ModFiles::from_dir(&dir, max_bytes) else {
                tracing::warn!(target: "regia_editor", dir = %dir.display(), "not a Lua mod directory (no readable files)");
                continue;
            };
            let id = files
                .get(regia_script_lua::LUAMOD_FILE)
                .and_then(|text| regia_script_lua::LuaModManifest::parse(text).ok())
                .map(|manifest| manifest.id);
            let Some(id) = id else { continue };
            if mods.get(&id).is_some() {
                continue;
            }
            match mods.load(files, &mut EcsModWorld::new(world)) {
                Ok(loaded_id) => loaded.push(loaded_id),
                Err(error) => {
                    tracing::warn!(target: "regia_editor", dir = %dir.display(), "Lua mod did not load: {}", error.message);
                }
            }
        }
    });
    if !loaded.is_empty() {
        tracing::info!(target: "regia_editor", mods = %loaded.join(", "), "loaded the project's Lua mods");
    }
}
