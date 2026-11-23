// SPDX-License-Identifier: MPL-2.0
//! Where a data table's bytes come from at run time (spec 51 § 4.4, ADR-2012): source
//! text when it exists (the VFS, else the project's `assets/`), else the cooked payload the asset
//! server holds for the path. A dev tree therefore always runs what is on disk, however stale its
//! last cook, and a packaged game, which ships these tables only in its paks, reads them cooked.

use regia_app::prelude::*;
use regia_asset::{AssetServer, decode_cooked_file};
use regia_core::id::TypePathHash;

/// The bytes of one table.
pub enum TableBytes {
    /// Source text.
    Text(String),
    /// A cooked payload (the part after the cooked header).
    Cooked(Vec<u8>),
}

fn read_text(world: &World, path: &str) -> Result<String, String> {
    if let Some(vfs) = world.get_resource::<regia_platform::Vfs>()
        && let Ok(vfs_path) = regia_platform::VfsPath::parse(&format!("assets://{path}"))
        && let Ok(bytes) = vfs.read_sync(&vfs_path)
    {
        return String::from_utf8(bytes.to_vec()).map_err(|_| format!("{path} is not UTF-8"));
    }
    let server = world
        .get_resource::<AssetServer>()
        .ok_or_else(|| format!("{path}: no VFS and no asset server to read it"))?;
    // The project file, else the earliest overlay root that holds it (ADR-8845).
    let file = server.source_path(path);
    std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))
}

/// The cooked payload of asset path `path`, or `None` when no pak or cache knows the path.
fn read_cooked(world: &mut World, path: &str, type_path: &str) -> Option<Result<Vec<u8>, String>> {
    let mut server = world.get_resource_mut::<AssetServer>()?;
    let id = server.id_for_path(&format!("assets/{path}"))?;
    Some(
        server
            .read_cooked(id)
            .map_err(|e| e.to_string())
            .and_then(|(file, _)| {
                let (header, payload) = decode_cooked_file(&file).map_err(|e| format!("{path}: {e}"))?;
                if header.type_tag != TypePathHash::of(type_path) {
                    return Err(format!("{path}: the cooked asset is not a `{type_path}`"));
                }
                Ok(payload.to_vec())
            }),
    )
}

/// Reads asset path `path`: its source text, else its cooked `type_path` payload.
///
/// # Errors
///
/// A message when neither exists, or the cooked asset cannot be read or is not a `type_path`.
pub fn read_table(world: &mut World, path: &str, type_path: &str) -> Result<TableBytes, String> {
    match read_text(world, path) {
        Ok(text) => Ok(TableBytes::Text(text)),
        Err(error) => match read_cooked(world, path, type_path) {
            Some(cooked) => cooked.map(TableBytes::Cooked),
            None => Err(error),
        },
    }
}

/// Reads asset path `path` as a packaged game should (ADR-9370): when the asset server has paks
/// mounted, the pak's cooked `type_path` payload first, so raw copies a package ships beside its
/// paks do not win over the cook; otherwise (a dev tree) as [`read_table`].
///
/// # Errors
///
/// As [`read_table`].
pub fn read_table_packaged_first(
    world: &mut World,
    path: &str,
    type_path: &str,
) -> Result<TableBytes, String> {
    let packaged = world
        .get_resource::<AssetServer>()
        .is_some_and(AssetServer::has_paks);
    if packaged && let Some(Ok(payload)) = read_cooked(world, path, type_path) {
        return Ok(TableBytes::Cooked(payload));
    }
    read_table(world, path, type_path)
}
