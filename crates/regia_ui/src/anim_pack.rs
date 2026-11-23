// SPDX-License-Identifier: MPL-2.0
//! Loading a `.uianim.ron` library from the project or from a pak (ADR-3092).
//!
//! The cook ([`UIANIM_TYPE_PATH`]) stores the canonical RON text of a library, so a packaged game
//! with no `assets/` folder reads the same library its source describes. [`load_uianim_file`] reads
//! the source text when there is one (a dev tree always runs what is on disk), else the cooked pak
//! entry, validates it with [`UiAnimLibrary::from_ron`] and merges its animations into the world's
//! [`UiAnimLibrary`] resource, where `ui.play_anim` finds them by name.
//!
//! A game references an animation library by loading it by path; no `.ui.ron` names one. The path is
//! relative to `assets/`.

use regia_app::prelude::*;
use regia_asset::{AssetServer, decode_cooked_file};
use regia_core::id::TypePathHash;

use crate::anim_timeline::{AnimError, UiAnimLibrary};

/// The type path stamped on a cooked `.uianim.ron` (its payload is the canonical RON text).
pub const UIANIM_TYPE_PATH: &str = "regia_ui::anim_timeline::UiAnimLibrary";

/// Why a library file did not load.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AnimLoadError {
    /// Neither source text nor a cooked entry exists at the path, or the entry cannot be read.
    #[error("{0}")]
    Read(String),
    /// The text is not a valid library (the existing [`AnimError`], unchanged).
    #[error(transparent)]
    Invalid(#[from] AnimError),
}

fn read_source(world: &World, path: &str) -> Option<String> {
    let server = world.get_resource::<AssetServer>()?;
    // The project file, else the earliest overlay root that holds it (ADR-8845).
    let file = server.source_path(path);
    std::fs::read_to_string(file).ok()
}

fn read_cooked(world: &mut World, path: &str) -> Result<String, AnimLoadError> {
    let read = |m: String| AnimLoadError::Read(m);
    let mut server = world
        .get_resource_mut::<AssetServer>()
        .ok_or_else(|| read(format!("{path}: no asset server to read it")))?;
    let id = server
        .id_for_path(&format!("assets/{path}"))
        .ok_or_else(|| read(format!("{path}: no source file and no cooked entry")))?;
    let (file, _) = server.read_cooked(id).map_err(|e| read(format!("{path}: {e}")))?;
    let (header, payload) = decode_cooked_file(&file).map_err(|e| read(format!("{path}: {e}")))?;
    if header.type_tag != TypePathHash::of(UIANIM_TYPE_PATH) {
        return Err(read(format!(
            "{path}: the cooked asset is not a `{UIANIM_TYPE_PATH}`"
        )));
    }
    String::from_utf8(payload.to_vec()).map_err(|_| read(format!("{path}: not UTF-8")))
}

/// Reads the library at asset path `path` (source first, then the pak), merges its animations into
/// the world's [`UiAnimLibrary`] (same names are replaced) and returns the names it brought.
///
/// # Errors
/// [`AnimLoadError::Read`] when nothing is there, [`AnimLoadError::Invalid`] with the library's own
/// error when the file is malformed; the world is unchanged in both cases.
pub fn load_uianim_file(world: &mut World, path: &str) -> Result<Vec<String>, AnimLoadError> {
    let _span = tracing::debug_span!(target: "regia_ui", "uianim_load").entered();
    let text = match read_source(world, path) {
        Some(text) => text,
        None => read_cooked(world, path)?,
    };
    let lib = UiAnimLibrary::from_ron(&text)?;
    let names: Vec<String> = lib.animations.keys().cloned().collect();
    let mut target = world.get_resource_or_insert_with(UiAnimLibrary::default);
    target.animations.extend(lib.animations);
    Ok(names)
}
