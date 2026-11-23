// SPDX-License-Identifier: MPL-2.0
//! Cooks `.fluidcache` bakes of the particle liquid (`docs/formats/fluidcache.md`, ADR-2441) into
//! cooked files, so they ship inside paks (ADR-2577).
//!
//! The cook validates the file with every reader check (counts against the remaining bytes, delta
//! ranges, magic and version) and stores it in canonical form through `regia_fluid::cache::cook`: a
//! v1 file re-encodes to itself, and a future version's migrator plugs in there, so a pak always
//! carries the current version. A damaged file is a cook error that starts with its `fluidcache_*`
//! code. The payload depends only on the file's bytes, so cooking is byte-reproducible.

use std::path::Path;

use regia_fluid::cache::{CacheError, FluidCache};

use crate::AssetDatabase;
use crate::cook::{CookError, ImportedCook};

/// Importer name of `.fluidcache` sources (the cook key and the meta file).
pub const FLUIDCACHE_IMPORTER: &str = "fluidcache";

/// The reflect path of the fluid cache a cooked `.fluidcache` carries.
pub const FLUIDCACHE_TYPE_PATH: &str = "regia_fluid::cache::FluidCache";

/// Why a `.fluidcache` did not cook.
#[derive(Debug, thiserror::Error)]
pub enum FluidCookError {
    /// The cache is damaged, truncated, too large or of an unknown version.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

impl FluidCookError {
    /// The stable lowercase code (`fluidcache_*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cache(e) => e.code(),
        }
    }
}

/// A cooked `.fluidcache`.
#[derive(Debug, Clone, PartialEq)]
pub struct CookedFluidCache {
    /// The canonical file at the current version (the cooked payload).
    pub payload: Vec<u8>,
    /// Frames it holds.
    pub frames: usize,
    /// Whether it carries the whitewater stream.
    pub whitewater: bool,
}

/// Validates `bytes` as a `.fluidcache` and returns its canonical form.
pub fn cook_fluidcache(bytes: &[u8]) -> Result<CookedFluidCache, FluidCookError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "cook_fluidcache", bytes = bytes.len()).entered();
    let payload = regia_fluid::cache::cook(bytes)?;
    let cache = FluidCache::from_bytes(&payload)?;
    Ok(CookedFluidCache {
        payload,
        frames: cache.frame_count(),
        whitewater: cache.has_whitewater(),
    })
}

/// Whether `name` is a `.fluidcache` file name.
#[must_use]
pub fn is_fluidcache(name: &str) -> bool {
    crate::cook::name_ends_with(name, ".fluidcache")
}

/// The cook dispatch row of a `.fluidcache` (`cook_one`): importer, settings hash (the payload's,
/// since the payload is the whole input), the meta file's id, type path and payload.
pub(crate) fn cook_fluidcache_source(source: &Path, bytes: &[u8]) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, FLUIDCACHE_IMPORTER).map_err(CookError::Db)?;
    let cooked = cook_fluidcache(bytes).map_err(|e| CookError::Fluid(format!("{}: {e}", e.code())))?;
    Ok((
        FLUIDCACHE_IMPORTER.to_string(),
        *blake3::hash(&cooked.payload).as_bytes(),
        meta.id,
        FLUIDCACHE_TYPE_PATH.to_string(),
        cooked.payload,
        vec![],
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use regia_asset::{AssetManifest, cooked_cache_dir};
    use regia_core::math::Vec3;
    use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};

    use super::*;
    use crate::{CookOptions, cook_project, pack_cooked_directory};

    /// A short bake of a falling block (whitewater on).
    fn bake() -> FluidCache {
        let params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::new(0.4, 0.4, 0.1),
            cell_size: 0.05,
            particle_cap: 4_000,
            ..FluidParams::default()
        };
        let mut sim = FluidSim::new(params).expect("sim");
        sim.add_emitter(Emitter {
            shape: Shape::Box {
                center: Vec3::new(0.1, 0.25, 0.05),
                half_extents: Vec3::new(0.08, 0.1, 0.05),
            },
            velocity: Vec3::ZERO,
            kind: EmitterKind::Fill,
            done: false,
        })
        .expect("fill");
        let mut cache = FluidCache::for_sim(&sim, true);
        for _ in 0..12 {
            sim.step();
            cache.record(&sim).expect("record");
        }
        cache
    }

    /// The cook keeps a valid cache byte for byte and refuses a damaged one with its code.
    #[test]
    fn fluidcache_cooks_canonically_and_damaged_ones_fail() {
        let bytes = bake().to_bytes();
        let cooked = cook_fluidcache(&bytes).expect("cook");
        assert_eq!(cooked.payload, bytes);
        assert_eq!(cooked.frames, 12);
        assert!(cooked.whitewater);
        assert_eq!(cook_fluidcache(&cooked.payload).expect("again"), cooked);
        let e = cook_fluidcache(&bytes[..bytes.len() / 2]).expect_err("truncated");
        assert!(e.code().starts_with("fluidcache_"), "{}", e.code());
        assert!(is_fluidcache("pour.fluidcache") && !is_fluidcache("pour.fluidcache.bak"));
    }

    /// A project's `.fluidcache` cooks to a manifest entry, packs into the base pak, and the pak's
    /// entry decodes to the same cache (every frame equal); a damaged file is a cook error that
    /// names the file and starts with its `fluidcache_*` code.
    #[test]
    fn fluidcache_round_trips_through_cook_and_pak() {
        let cache = bake();
        let bytes = cache.to_bytes();
        let root = regia_test_utils::unique_temp_dir("cook-fluidcache");
        let dir = root.join("assets").join("fx");
        std::fs::create_dir_all(&dir).expect("assets dir");
        std::fs::write(dir.join("pour.fluidcache"), &bytes).expect("write");
        std::fs::write(dir.join("bad.fluidcache"), b"RFLC\x01\x00\x00\x00junk").expect("write bad");
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: false,
        })
        .expect("cook");
        assert_eq!(report.cooked, 1, "{report:?}");
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].0.ends_with("fx/bad.fluidcache"),
            "{:?}",
            report.errors
        );
        assert!(report.errors[0].1.contains("fluidcache_"), "{:?}", report.errors);

        let cooked_dir = cooked_cache_dir(&root, "linux");
        let manifest = AssetManifest::read_from_cache(&cooked_dir)
            .expect("manifest")
            .expect("present");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == "assets/fx/pour.fluidcache")
            .expect("entry");
        assert_eq!(entry.type_path, FLUIDCACHE_TYPE_PATH);

        let content = root.join("content");
        let paks = pack_cooked_directory(&cooked_dir, &content, &BTreeMap::new(), true).expect("pack");
        assert_eq!(paks.len(), 1);
        let reader = regia_asset::PakReader::open(&paks[0]).expect("open pak");
        let file = reader.read_decompressed(entry.id).expect("pak entry");
        let (_, payload) = regia_asset::decode_cooked_file(&file).expect("decode");
        assert_eq!(payload, bytes.as_slice(), "the pak carries the cache unchanged");
        let back = FluidCache::from_bytes(payload).expect("loads");
        assert_eq!(back, cache);
        for i in 0..cache.frame_count() {
            assert_eq!(back.play(i).expect("frame"), cache.play(i).expect("frame"));
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
