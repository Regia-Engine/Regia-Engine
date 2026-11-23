// SPDX-License-Identifier: MPL-2.0
//! **Make unique** (spec addendum § 7.11, ADR-7786): an asset field whose asset other files also
//! use gets its own copy. The asset file is copied to a new file next to the open level (next to
//! the asset when the level is unsaved or outside `assets/`) and the field is pointed at the copy
//! as one undo step. The copy gets no `.meta`: the importer gives it its own asset id, so it never
//! shares the original's identity.
//!
//! The new name is `<stem>_unique.<extensions>` (`rock.material.ron` → `rock_unique.material.ron`),
//! then `_unique2`, `_unique3`, … while the name is taken. Undo points the field back; the copied
//! file stays (delete it from the Content Browser).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;

use crate::data_panels::PanelError;

/// What a make-unique did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeUnique {
    /// The asset the field held (project-relative).
    pub from: String,
    /// The copy the field holds now (project-relative).
    pub to: String,
    /// The value written to the field (the same form as the old value: with or without `assets/`).
    pub value: String,
    /// Undo entries after the edit.
    pub undoable: usize,
}

/// `rock.material.ron` → (`rock`, `.material.ron`); `noext` → (`noext`, ``).
fn split_name(name: &str) -> (&str, &str) {
    match name.find('.') {
        Some(0) | None => (name, ""),
        Some(dot) => name.split_at(dot),
    }
}

/// The project-relative path of the copy: in `folder` (project-relative), named after `asset`'s
/// file, the first `_unique`, `_unique2`, … name `exists` says is free.
#[must_use]
pub fn unique_name(folder: &str, asset: &str, exists: impl Fn(&str) -> bool) -> String {
    let file = asset.rsplit('/').next().unwrap_or(asset);
    let (stem, extensions) = split_name(file);
    let folder = folder.trim_end_matches('/');
    (1..)
        .map(|n| {
            let suffix = if n == 1 { String::new() } else { n.to_string() };
            format!("{folder}/{stem}_unique{suffix}{extensions}")
        })
        .find(|candidate| !exists(candidate))
        .unwrap_or_default()
}

/// The folder (project-relative) the copy goes to: the open level's folder when the level is
/// saved under `<root>/assets`, else the asset's own folder.
fn destination_folder(root: &Path, level: Option<&Path>, asset_rel: &str) -> String {
    let assets = root.join("assets");
    if let Some(folder) = level.and_then(Path::parent)
        && let Ok(rel) = folder.strip_prefix(root)
        && folder.starts_with(&assets)
    {
        return rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
    }
    asset_rel
        .rsplit_once('/')
        .map_or_else(|| "assets".to_owned(), |(folder, _)| folder.to_owned())
}

/// Copies `from` to `to` through a temporary file in the same folder, then a rename, so a reader
/// never sees half a file.
fn copy_atomic(from: &Path, to: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(from)?;
    let tmp = to.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, to).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Makes the asset of field `path` of `component` on `entity` unique (see the module docs).
///
/// # Errors
///
/// `make_unique_not_asset` (the field holds no asset path), `make_unique_builtin` (a built-in
/// primitive has no file), `make_unique_no_project`, `make_unique_missing` (the asset file is not
/// there), `make_unique_io` (the copy failed), or the edit's own code (`entity_missing`, …).
pub fn make_unique(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<MadeUnique, PanelError> {
    let current = crate::command::read_property(world, entity, component, path)
        .map_err(|e| PanelError::new("make_unique_not_asset", e.to_string()))?;
    let value = current
        .try_downcast_ref::<String>()
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            PanelError::new(
                "make_unique_not_asset",
                format!("`{component}.{path}` holds no asset path"),
            )
        })?;
    if value.starts_with("builtin:") {
        return Err(PanelError::new(
            "make_unique_builtin",
            format!("{value} is a built-in primitive, not a file"),
        ));
    }
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| PanelError::new("make_unique_no_project", "no project is open"))?;
    let normalized = value.trim().replace('\\', "/");
    let from = if normalized.starts_with("assets/") {
        normalized.clone()
    } else {
        format!("assets/{normalized}")
    };
    let source = root.join(&from);
    if !source.is_file() {
        return Err(PanelError::new(
            "make_unique_missing",
            format!("{from} is not a file in the project"),
        ));
    }
    let level: Option<PathBuf> = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone());
    let folder = destination_folder(&root, level.as_deref(), &from);
    let to = unique_name(&folder, &from, |candidate| root.join(candidate).exists());
    let target = root.join(&to);
    copy_atomic(&source, &target)
        .map_err(|e| PanelError::new("make_unique_io", format!("copy {from} to {to}: {e}")))?;
    let written = if normalized.starts_with("assets/") {
        to.clone()
    } else {
        to.strip_prefix("assets/").unwrap_or(&to).to_owned()
    };
    let report = super::multi::edit_value(world, &[entity], component, path, Box::new(written.clone()))
        .map_err(|e| {
            let _ = std::fs::remove_file(&target);
            PanelError::new(e.code, e.message)
        })?;
    crate::content_index::note_changed(world, &[target]);
    Ok(MadeUnique {
        from,
        to,
        value: written,
        undoable: report.undo_depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_keep_compound_extensions_and_count_up() {
        let taken = ["assets/levels/rock_unique.material.ron"];
        assert_eq!(
            unique_name("assets/levels", "assets/m/rock.material.ron", |c| taken
                .contains(&c)),
            "assets/levels/rock_unique2.material.ron"
        );
        assert_eq!(
            unique_name("assets/m/", "assets/m/noext", |_| false),
            "assets/m/noext_unique"
        );
        assert_eq!(split_name(".hidden"), (".hidden", ""));
    }

    #[test]
    fn the_copy_goes_next_to_a_level_under_assets_else_next_to_the_asset() {
        let root = Path::new("/p");
        assert_eq!(
            destination_folder(
                root,
                Some(Path::new("/p/assets/levels/a.level.ron")),
                "assets/m/x.png"
            ),
            "assets/levels"
        );
        assert_eq!(
            destination_folder(root, Some(Path::new("/elsewhere/a.level.ron")), "assets/m/x.png"),
            "assets/m"
        );
        assert_eq!(destination_folder(root, None, "assets/m/x.png"), "assets/m");
    }
}
