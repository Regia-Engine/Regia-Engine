// SPDX-License-Identifier: MPL-2.0
//! Project-scoped external image identity and encode reuse (ADR-8055).
//!
//! glTF import results keep their local sub paths for the importer cache and voxel cooks.
//! The published manifest maps those ids to the owning image's path. No cooked format changes.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_asset::{AssetId, ManifestEntry, TextureCookUsage, cooked_file_name};

use crate::db::AssetDatabase;
use crate::texture_settings::{TextureImporterSettings, usage_from_file_name};
use crate::{CookError, TextureCookQuality};

const VARIANT: &str = "gltf_texture/";
const USAGES: [TextureCookUsage; 5] = [
    TextureCookUsage::ColorSrgb,
    TextureCookUsage::ColorLinear,
    TextureCookUsage::NormalMap,
    TextureCookUsage::Mask,
    TextureCookUsage::Ui,
];

struct Source {
    id: AssetId,
    path: String,
    default_key: String,
    importer_version: u32,
    heightfield: bool,
}

#[derive(Default)]
struct SharedTextures {
    root: PathBuf,
    cache: PathBuf,
    sources: BTreeMap<PathBuf, Source>,
    paths: BTreeMap<AssetId, String>,
    owners: BTreeMap<AssetId, String>,
    importer_versions: BTreeMap<AssetId, u32>,
    reserved_ids: BTreeSet<AssetId>,
    checked_metadata: bool,
    pending: BTreeMap<AssetId, [u8; 32]>,
    written: BTreeMap<AssetId, [u8; 32]>,
}

thread_local! {
    static SHARED: RefCell<Option<SharedTextures>> = const { RefCell::new(None) };
}

/// Owns one cook's image sharing state, restoring any outer scope on drop.
pub(crate) struct Scope(Option<SharedTextures>);

impl Scope {
    /// Prepare identities before input hashing: creating an image's .meta must not invalidate
    /// the first cook's input key. Only external files within the canonical project qualify.
    pub(crate) fn enter(
        project_root: &Path,
        cache: &Path,
        sidecars: &BTreeSet<PathBuf>,
    ) -> Result<Self, CookError> {
        let root = std::fs::canonicalize(project_root)?;
        let mut state = SharedTextures {
            root,
            cache: cache.to_path_buf(),
            ..SharedTextures::default()
        };
        // A model can replace an external image with an embedded one. Its old .meta then
        // still names the image-owned id; it must not make the new private image overwrite
        // that source (or another model's image). Keep authored private remaps otherwise.
        if let Ok(Some(manifest)) = regia_asset::AssetManifest::read_from_cache(cache) {
            state.reserved_ids.extend(
                manifest
                    .entries
                    .iter()
                    .filter(|row| {
                        matches!(
                            row.type_path.as_str(),
                            "regia_asset::CookedTexture" | "regia_asset::CookedImage"
                        ) && (!row.path.contains('#') || row.path.contains("#gltf_texture/"))
                    })
                    .map(|row| row.id),
            );
        }
        for path in sidecars {
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "tga" | "bmp") {
                continue;
            }
            let Ok(canonical) = std::fs::canonicalize(path) else {
                continue;
            };
            let Ok(rel) = canonical.strip_prefix(&state.root) else {
                continue;
            };
            let rel = regia_asset::normalise_asset_path(rel);
            let canonical_ext = canonical
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            // The image's `.meta` (and the alias's, read below) obey the same project boundary as
            // the image URI: a `.meta` symlink must not read outside it. A sidecar that leaves the
            // project or does not parse is skipped here, not a failed cook: the owning model's own
            // import refuses it and names the file (audit-v035 Q-06, `ensure_gltf_image_metas`).
            let metas_inside = [
                regia_asset::meta_path_for(&canonical),
                regia_asset::meta_path_for(path),
            ]
            .iter()
            .all(|meta| regia_core::project_path::check_under_root(&state.root, meta).is_ok());
            if !metas_inside {
                continue;
            }
            let Ok(meta) = AssetDatabase::load_or_create_meta(&canonical, &canonical_ext) else {
                continue;
            };
            let defaults = settings(project_root, &rel, &canonical_ext, meta.settings.as_deref());
            let default_usage = defaults.usage_or(usage_from_file_name(&canonical));
            let heightfield = defaults.heightfield == Some(true);
            state.sources.insert(
                canonical.clone(),
                Source {
                    id: meta.id,
                    path: rel.clone(),
                    default_key: if heightfield {
                        "heightfield".into()
                    } else {
                        effective_key(&defaults, default_usage, TextureCookQuality::High)
                    },
                    importer_version: crate::cook::importer_version_for(&canonical, &canonical_ext),
                    heightfield,
                },
            );
            // A lexical alias may have its own .meta settings. The file's identity still belongs
            // to the canonical source. Pre-register every usage for skip/cache-hit outcomes too.
            let logical_meta = crate::texture_settings::TextureImporterSettings::for_source(path);
            let logical_rel = crate::import_rules::project_rel(project_root, path).unwrap_or(rel);
            let logical = settings(
                project_root,
                &logical_rel,
                &ext,
                Some(&logical_meta.to_meta_string()),
            );
            for usage in USAGES {
                for quality in [TextureCookQuality::High, TextureCookQuality::Low] {
                    state.identity(&canonical, &logical, logical.usage_or(usage), quality);
                }
            }
        }
        Ok(Self(SHARED.with(|s| s.borrow_mut().replace(state))))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        SHARED.with(|s| *s.borrow_mut() = self.0.take());
    }
}

fn settings(root: &Path, rel: &str, ext: &str, explicit: Option<&str>) -> TextureImporterSettings {
    let resolved = crate::import_rules::resolve_for_cook(root, rel, ext, explicit);
    TextureImporterSettings::from_meta_string(Some(&resolved))
}

/// Whether a referenced image explicitly requests a standalone heightfield. Resolve the same
/// rule/meta precedence as its cook, even if the image is temporarily unreadable.
pub(crate) fn standalone_is_heightfield(root: &Path, path: &Path) -> bool {
    let Some(rel) = crate::import_rules::project_rel(root, path) else {
        return false;
    };
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
        return false;
    }
    let explicit = std::fs::read(regia_asset::meta_path_for(path))
        .ok()
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
        .and_then(|meta| meta.settings);
    settings(root, &rel, &ext, explicit.as_deref()).heightfield == Some(true)
}

/// Effective RGBA8 settings, independent of optional-field spelling and of the glTF parent.
/// Platform output/policy lives in a separate cooked directory, as for standalone image ids.
fn effective_key(
    settings: &TextureImporterSettings,
    usage: TextureCookUsage,
    quality: TextureCookQuality,
) -> String {
    let green = if usage == TextureCookUsage::NormalMap {
        settings.normal_green_or_default()
    } else {
        Default::default()
    };
    let order = if usage == TextureCookUsage::ColorLinear {
        settings.orm_channel_order_or_default()
    } else {
        Default::default()
    };
    let cutoff = settings.alpha_cutoff.filter(|v| {
        v.is_finite() && *v > 0.0 && *v <= 1.0 && crate::alpha_coverage::coverage_channel(usage).is_some()
    });
    format!(
        "v1;usage={usage:?};max={:?};compress={};quality={:?};stream={};min={};green={:?};orm={:?};cutoff={:?}",
        settings.max_size.filter(|n| *n > 0),
        settings.compress(),
        settings.quality_or(quality),
        settings.stream.unwrap_or(true),
        settings.stream_min_px.unwrap_or(0),
        green,
        order,
        cutoff.map(f32::to_bits)
    )
}

impl SharedTextures {
    fn reserve_metadata_ids(&mut self) {
        if self.checked_metadata {
            return;
        }
        self.checked_metadata = true;
        // Clean checkouts have .meta ownership but no manifest. Primary asset ids never
        // belong to a private texture; repeated texture ids in model metadata came from a
        // shared image and must become private again if those images are now embedded.
        let mut texture_ids = BTreeSet::new();
        for source in AssetDatabase::discover_sources(&self.root.join("assets")) {
            let Ok(bytes) = std::fs::read(regia_asset::meta_path_for(&source)) else {
                continue;
            };
            let Ok(meta) = regia_asset::MetaSidecar::from_ron(&bytes) else {
                continue;
            };
            self.reserved_ids.insert(meta.id);
            for sub in &meta.sub_assets {
                if sub
                    .path
                    .strip_prefix("texture/")
                    .is_some_and(|index| index.parse::<usize>().is_ok())
                    && !texture_ids.insert(sub.id)
                {
                    self.reserved_ids.insert(sub.id);
                }
            }
        }
    }

    fn identity(
        &mut self,
        canonical: &Path,
        settings: &TextureImporterSettings,
        usage: TextureCookUsage,
        quality: TextureCookQuality,
    ) -> Option<AssetId> {
        let source = self.sources.get(canonical)?;
        let key = effective_key(settings, usage, quality);
        let (id, path) = if key == source.default_key {
            (source.id, source.path.clone())
        } else {
            let sub = format!("{VARIANT}{}", blake3::hash(key.as_bytes()).to_hex());
            (
                AssetId::sub_asset(source.id, &sub),
                format!("{}#{sub}", source.path),
            )
        };
        self.paths.insert(id, path);
        self.owners.insert(id, source.path.clone());
        self.importer_versions.insert(id, source.importer_version);
        Some(id)
    }
}

/// The shared image's importer revision, independent of which model writes it first.
pub(crate) fn importer_version(id: AssetId) -> Option<u32> {
    SHARED.with(|s| s.borrow().as_ref()?.importer_versions.get(&id).copied())
}

/// Image sources whose registry records a live cook can update outside its model paths.
pub(crate) fn source_paths() -> Vec<String> {
    SHARED.with(|s| {
        s.borrow().as_ref().map_or_else(Vec::new, |state| {
            state.sources.values().map(|s| s.path.clone()).collect()
        })
    })
}

/// A canonical side input's project-relative spelling for relocation-stable cook hashes.
pub(crate) fn project_relative_path(path: &Path) -> Option<String> {
    SHARED.with(|s| crate::import_rules::project_rel(&s.borrow().as_ref()?.root, path))
}

/// A shared id only for a prepared external image within this cook's project.
pub(crate) fn identity(
    image: &gltf::Image<'_>,
    base: Option<&Path>,
    settings: &TextureImporterSettings,
    usage: TextureCookUsage,
    quality: TextureCookQuality,
) -> Option<AssetId> {
    let gltf::image::Source::Uri { uri, .. } = image.source() else {
        return None;
    };
    if uri.starts_with("data:") {
        return None;
    }
    let path = base?.join(crate::gltf_import::decode_gltf_uri(uri)?);
    let canonical = std::fs::canonicalize(path).ok()?;
    SHARED.with(|s| {
        s.borrow_mut()
            .as_mut()?
            .identity(&canonical, settings, usage, quality)
    })
}

/// Preserve private remaps, but release a former external image's shared identity when the
/// current image no longer qualifies for sharing (for example, after embedding it).
pub(crate) fn private_id(parent: AssetId, path: &str) -> AssetId {
    let previous = crate::subasset_remap::sub_id(parent, path);
    let derived = AssetId::sub_asset(parent, path);
    if previous == derived {
        return previous;
    }
    let was_shared = SHARED.with(|s| {
        let mut shared = s.borrow_mut();
        let Some(state) = shared.as_mut() else {
            return false;
        };
        if state.reserved_ids.contains(&previous) || state.paths.contains_key(&previous) {
            return true;
        }
        // Only an unrecognized stored remap needs the clean-checkout fallback. Ordinary
        // imports and incremental cooks do not scan every source's metadata again.
        state.reserve_metadata_ids();
        state.reserved_ids.contains(&previous)
    });
    if was_shared { derived } else { previous }
}

/// Reuse only files this cook finished writing with the same fitted pixels. The source can be
/// edited while cooking, so path/settings alone are not enough. No texture-sized memory cache.
pub(crate) fn cached(id: AssetId, width: u32, height: u32, pixels: &[u8]) -> Option<(String, Vec<u8>)> {
    SHARED.with(|s| {
        let mut shared = s.borrow_mut();
        let state = shared.as_mut()?;
        if !state.paths.contains_key(&id) {
            return None;
        }
        let mut hash = blake3::Hasher::new();
        hash.update(&width.to_le_bytes());
        hash.update(&height.to_le_bytes());
        hash.update(pixels);
        let hash = *hash.finalize().as_bytes();
        state.pending.insert(id, hash);
        if state.written.get(&id) != Some(&hash) {
            return None;
        }
        // A missing or invalid output must be replaced after the caller re-encodes it.
        state.written.remove(&id);
        let bytes = std::fs::read(state.cache.join(cooked_file_name(id))).ok()?;
        let (header, payload) = regia_asset::decode_cooked_file(&bytes).ok()?;
        let ty = if header.type_tag == regia_asset::AssetTypeTag::texture().0 {
            "regia_asset::CookedTexture"
        } else if header.type_tag == regia_core::id::TypePathHash::of("regia_asset::CookedImage") {
            "regia_asset::CookedImage"
        } else {
            return None;
        };
        if header.asset_id != id {
            return None;
        }
        state.written.insert(id, hash);
        Some((ty.into(), payload.to_vec()))
    })
}

/// A file with this input has already been published during this cook.
pub(crate) fn already_written(id: AssetId) -> bool {
    SHARED.with(|s| {
        s.borrow().as_ref().is_some_and(|state| {
            state
                .pending
                .get(&id)
                .is_some_and(|hash| state.written.get(&id) == Some(hash))
                && state.cache.join(cooked_file_name(id)).is_file()
        })
    })
}

/// Record success only after the atomic cooked-file write.
pub(crate) fn written(id: AssetId) {
    SHARED.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut()
            && let Some(hash) = state.pending.get(&id)
        {
            state.written.insert(id, *hash);
        }
    });
}

/// Publish one row per shared image, including outcomes restored from importer/shared caches.
/// Remove retired variants only when no surviving cooked asset references them, including in live cooks.
pub(crate) fn publish(manifest: &mut BTreeMap<String, ManifestEntry>) -> Vec<String> {
    SHARED.with(|s| {
        let shared = s.borrow();
        let Some(state) = shared.as_ref() else {
            return Vec::new();
        };
        publish_manifest(state, manifest)
    })
}

fn publish_manifest(state: &SharedTextures, manifest: &mut BTreeMap<String, ManifestEntry>) -> Vec<String> {
    let source_paths: BTreeSet<&str> = state
        .sources
        .values()
        .filter(|s| !s.heightfield)
        .map(|s| s.path.as_str())
        .collect();
    let dependencies: BTreeSet<AssetId> = manifest
        .values()
        .filter_map(|row| {
            crate::registry_build::read_cooked_header(&state.cache.join(cooked_file_name(row.id)))
        })
        .flat_map(|header| header.dependencies)
        .collect();
    let mut used: BTreeMap<String, BTreeSet<AssetId>> = BTreeMap::new();
    let mut replacements = BTreeMap::new();
    manifest.retain(|path, row| {
        // A removed heightfield opt-in must retire the old standalone row, including when
        // every current glTF view uses a non-default texture variant.
        if source_paths.contains(path.as_str()) && row.type_path == "regia_asset::Heightfield" {
            return false;
        }
        let shared_row = matches!(
            row.type_path.as_str(),
            "regia_asset::CookedTexture" | "regia_asset::CookedImage"
        );
        if shared_row
            && (source_paths.contains(path.as_str()) || path.contains("#gltf_texture/"))
            && !dependencies.contains(&row.id)
        {
            return false;
        }
        if let Some(canonical) = state.paths.get(&row.id) {
            let mut entry = row.clone();
            entry.path.clone_from(canonical);
            // A newly imported/restored model row wins over the previous canonical row,
            // especially when an image changes between RGBA8 and compressed texture types.
            if path == canonical {
                replacements.entry(canonical.clone()).or_insert(entry);
            } else {
                replacements.insert(canonical.clone(), entry);
            }
            if let Some(owner) = state.owners.get(&row.id) {
                used.entry(owner.clone()).or_default().insert(row.id);
            }
            return false;
        }
        true
    });
    for row in replacements.into_values() {
        manifest.insert(row.path.clone(), row);
    }
    used.into_iter().filter(|(_, ids)| ids.len() > 1).map(|(source, ids)| {
        let warning = format!("shared_texture_settings: {source} has {} different effective texture settings; keeping separate cooked ids: {}",
            ids.len(), ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
        warning
    }).collect()
}
