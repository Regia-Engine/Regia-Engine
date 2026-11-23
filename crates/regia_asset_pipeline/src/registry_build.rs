// SPDX-License-Identifier: MPL-2.0
//! Builds the asset registry at cook time (`docs/specs/content-data.md` § 2.1).
//!
//! One [`RegistryEntry`] per manifest entry: id, path and type from the manifest, dependencies from
//! the cooked file's header (only the first bytes of the file are read, never the payload), tags
//! from [`automatic_tags`], the importer recorded in the asset database, and the project's
//! `config/asset_tags.ron` rules, and the cooked file's size.
//!
//! [`write_cache_registry`] rebuilds the whole registry after a full cook; [`update_cache_registry`]
//! replaces only the records of the sources a live cook just re-cooked, so the editor's registry
//! stays current without a rebuild; [`write_pak_registry`] writes the registry of one pak's
//! entries next to it.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use regia_asset::registry::{
    AssetRegistry, RegistryEntry, RegistryError, TagRules, automatic_tags, cache_registry_path,
    pak_registry_path, source_path,
};
use regia_asset::{AssetId, AssetManifest, COOKED_MAGIC, CookedHeader, ManifestEntry, cooked_file_name};

use crate::db::AssetDatabase;

/// Bytes read from the front of a cooked file to decode its header. A header with more
/// dependencies than fit is read again from the whole file.
const HEADER_PREFIX: usize = 16 * 1024;

/// What a registry build did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryBuildReport {
    /// Records written.
    pub records: usize,
    /// Records whose cooked file was missing or had no readable header (no dependencies recorded).
    pub without_header: usize,
    /// Records inserted, replaced or removed by an incremental update (0 for a full build).
    pub changed: usize,
    /// Problems that did not stop the build (a bad tag rules file).
    pub warnings: Vec<String>,
}

/// Reads the cooked header of `path` from its first bytes. `None` when the file is missing or is
/// not a cooked file.
pub fn read_cooked_header(path: &Path) -> Option<CookedHeader> {
    let file = std::fs::File::open(path).ok()?;
    let mut prefix = Vec::with_capacity(HEADER_PREFIX);
    file.take(HEADER_PREFIX as u64).read_to_end(&mut prefix).ok()?;
    match decode_header(&prefix) {
        Some(header) => Some(header),
        // A long dependency list: the header did not fit in the prefix.
        None if prefix.len() == HEADER_PREFIX => decode_header(&std::fs::read(path).ok()?),
        None => None,
    }
}

fn decode_header(bytes: &[u8]) -> Option<CookedHeader> {
    if bytes.len() < 4 || bytes[..4] != COOKED_MAGIC {
        return None;
    }
    postcard::take_from_bytes::<CookedHeader>(&bytes[4..])
        .ok()
        .map(|(h, _)| h)
}

/// The registry records of `entries` (manifest entries) over the cooked files in `cache_dir`.
/// Returns the records and how many had no readable cooked header.
pub fn registry_entries(
    cache_dir: &Path,
    entries: &[ManifestEntry],
    rules: &TagRules,
    db: &AssetDatabase,
) -> (Vec<RegistryEntry>, usize) {
    let mut without_header = 0;
    let records = entries
        .iter()
        .map(|entry| {
            let file = cache_dir.join(cooked_file_name(entry.id));
            let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let dependencies: Vec<AssetId> = match read_cooked_header(&file) {
                Some(header) => header
                    .dependencies
                    .into_iter()
                    .filter(|d| *d != entry.id)
                    .collect(),
                None => {
                    without_header += 1;
                    Vec::new()
                }
            };
            let mut tags = automatic_tags(&entry.path);
            if let Some(row) = db.entries.get(source_path(&entry.path)) {
                tags.insert("importer".to_string(), row.importer.clone());
            }
            tags.extend(crate::data_cook::registry_tags(&entry.type_path, &file));
            // Localized variants (spec 36 § 6): `localized/<L>/<rel>` names its locale and its base.
            if let Some((locale, base)) = regia_asset::localized::localized_of(&entry.path) {
                tags.insert("localized".to_string(), locale.to_string());
                tags.insert("localized_of".to_string(), base.to_string());
            }
            tags.extend(rules.tags_for(&entry.path));
            RegistryEntry {
                id: entry.id,
                path: entry.path.clone(),
                type_path: entry.type_path.clone(),
                dependencies,
                tags,
                size,
            }
        })
        .collect();
    (records, without_header)
}

/// Rebuilds `asset_registry.bin` in `cache_dir` from its manifest; `db` supplies the importers.
///
/// # Errors
///
/// The manifest cannot be read, or the registry cannot be written.
pub fn write_cache_registry(
    project_root: &Path,
    cache_dir: &Path,
    db: &AssetDatabase,
) -> Result<RegistryBuildReport, RegistryError> {
    let _span = tracing::info_span!(target: "regia_asset_pipeline", "registry_build").entered();
    let manifest = read_manifest(cache_dir)?;
    let (rules, warnings) = TagRules::load(project_root);
    let (records, without_header) = registry_entries(cache_dir, &manifest.entries, &rules, db);
    let registry = AssetRegistry::from_entries(records);
    registry.write_if_changed(&cache_registry_path(cache_dir))?;
    Ok(RegistryBuildReport {
        records: registry.len(),
        without_header,
        changed: 0,
        warnings,
    })
}

/// Replaces the records of `sources` (project-relative source paths, `assets/...`) in the cache
/// registry with their current manifest entries: the incremental update after a live re-cook.
/// Without a registry on disk (or with one that does not decode) it rebuilds the whole registry.
///
/// # Errors
///
/// The manifest cannot be read, or the registry cannot be written.
pub fn update_cache_registry(
    project_root: &Path,
    cache_dir: &Path,
    sources: &[String],
    db: &AssetDatabase,
) -> Result<RegistryBuildReport, RegistryError> {
    let path = cache_registry_path(cache_dir);
    let mut registry = match AssetRegistry::read_from(&path) {
        Ok(Some(registry)) => registry,
        Ok(None) | Err(_) => return write_cache_registry(project_root, cache_dir, db),
    };
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "registry_update", sources = sources.len())
            .entered();
    let manifest = read_manifest(cache_dir)?;
    let (rules, warnings) = TagRules::load(project_root);
    let wanted: BTreeSet<&str> = sources.iter().map(String::as_str).collect();
    let mut changed = 0;
    let mut without_header = 0;
    for source in &wanted {
        let of_source: Vec<ManifestEntry> = manifest
            .entries
            .iter()
            .filter(|e| source_path(&e.path) == *source)
            .cloned()
            .collect();
        let (records, missing) = registry_entries(cache_dir, &of_source, &rules, db);
        without_header += missing;
        changed += registry.replace_source(source, records);
    }
    if changed > 0 {
        registry.write_to(&path)?;
    }
    Ok(RegistryBuildReport {
        records: registry.len(),
        without_header,
        changed,
        warnings,
    })
}

/// Writes the registry of `pak_manifest`'s entries next to `pak` (`base.fpak` →
/// `base.registry.bin`). Returns the registry path.
///
/// # Errors
///
/// The registry cannot be written.
pub fn write_pak_registry(
    project_root: Option<&Path>,
    cooked_dir: &Path,
    pak: &Path,
    pak_manifest: &AssetManifest,
) -> Result<PathBuf, RegistryError> {
    let (rules, _) = project_root.map(TagRules::load).unwrap_or_default();
    let db = project_root
        .and_then(|root| AssetDatabase::open(root).ok())
        .unwrap_or_default();
    let (records, _) = registry_entries(cooked_dir, &pak_manifest.entries, &rules, &db);
    let out = pak_registry_path(pak);
    AssetRegistry::from_entries(records).write_to(&out)?;
    Ok(out)
}

/// The project root of a cooked cache directory (`<root>/.regia/cooked/<platform>`), when the
/// directory has that shape.
pub fn project_root_of_cache(cache_dir: &Path) -> Option<&Path> {
    let root = cache_dir.parent()?.parent()?.parent()?;
    root.join("assets").is_dir().then_some(root)
}

fn read_manifest(cache_dir: &Path) -> Result<AssetManifest, RegistryError> {
    match AssetManifest::read_from_cache(cache_dir) {
        Ok(manifest) => Ok(manifest.unwrap_or_default()),
        Err(regia_asset::ManifestError::Io(e)) => Err(RegistryError::Io(e)),
        Err(regia_asset::ManifestError::Postcard(e)) => Err(RegistryError::Postcard(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_variants_get_their_locale_and_base_as_tags() {
        let dir = regia_test_utils::unique_temp_dir("registry-localized");
        let entry = |path: &str| ManifestEntry {
            path: path.to_string(),
            id: AssetId::new_random(),
            type_path: "regia_asset::CookedTexture".to_string(),
        };
        let entries = [entry("ui/sign.png"), entry("localized/fr-CA/ui/sign.png")];
        let (records, _) = registry_entries(&dir, &entries, &TagRules::default(), &AssetDatabase::default());
        assert!(!records[0].tags.contains_key("localized"));
        assert_eq!(
            records[1].tags.get("localized").map(String::as_str),
            Some("fr-CA")
        );
        assert_eq!(
            records[1].tags.get("localized_of").map(String::as_str),
            Some("ui/sign.png")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
