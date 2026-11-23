// SPDX-License-Identifier: MPL-2.0
//! Headless cook (`docs/specs/12-assets.md` § 6, CLI `regia cook`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use regia_asset::{
    AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, CookedStandardMaterial, META_FORMAT_VERSION,
    ManifestEntry, MaterialAssetRon, MetaSidecar, cooked_cache_dir, cooked_file_name, encode_cooked_file,
    load_material_bytes, meta_path_for, normalise_asset_path, pack_importer_version,
};
use tracing::{info, instrument};

use crate::cook_inputs::{CookInputs, SourceInputs};
use crate::cook_ledger::{CookLedger, write_atomic};
use crate::db::{AssetDatabase, DbEntry};
use crate::platform::host_platform;
use regia_animation::compress::CompressionSettings;
use regia_animation::skeleton::SkeletonSocket;
use regia_asset::SubAssetEntry;

use crate::gltf_anim::{GltfAnimSettings, import_gltf_animation_with_base};
use crate::{
    Forward, GltfImportResult, ImportError, import_gltf_path_with_lod, import_png_height, import_png_rgba8,
    texture_compress::TextureCookError,
    texture_settings::{TextureImporterSettings, usage_from_file_name},
};

/// Output of a single importer before cache keying.
pub(crate) type ImportedCook =
    Result<(String, [u8; 32], AssetId, String, Vec<u8>, Vec<ManifestEntry>), CookError>;

/// Result of a project cook.
#[derive(Clone, Debug, Default)]
pub struct CookReport {
    /// Per sound: its ship mode (`keep` or `vorbis`) and bytes before and after (ADR-3230).
    pub audio_ship: Vec<crate::audio_transcode::AudioShipEntry>,
    /// Assets written this run.
    pub cooked: usize,
    /// Assets skipped (cache hit).
    pub skipped: usize,
    /// Of [`Self::skipped`], the sources skipped without running their importer: every input
    /// unchanged since their last cook (ADR-530). The others were imported, then matched.
    pub imports_skipped: usize,
    /// Bytes read this cook to hash sources and their inputs. A file whose size and
    /// modification time match its last hash is not read (ADR-530).
    pub hashed_bytes: u64,
    /// Failures by source path.
    pub errors: Vec<(String, String)>,
    /// Non-fatal warnings (e.g. missing optional terrain slots).
    pub warnings: Vec<String>,
    /// Wall time.
    pub elapsed_ms: u128,
    /// The material-graph shader permutations per render profile of the platform, one line each
    /// (master plan § 5.5a, ADR-2026; the full list is `permutations.json` in the cooked cache).
    pub permutations: Vec<String>,
    /// Profiles over their permutation budget (`regia package` fails a shipping package on any).
    /// Also every material-graph set over a gate device class's binding limits and cooked graph
    /// code a host shader rejects (ADR-4901).
    pub permutation_problems: Vec<String>,
    /// The material-graph checks beyond the pipeline count (ADR-4901): one line per device limit
    /// class of the platform's profiles (each graph code's worst bindings per stage) and one for
    /// the cooked static switches.
    pub material_graph_checks: Vec<String>,
    /// The distinct forward-shader define sets the project's cooked material graphs need
    /// (`MATERIAL_GRAPH` plus `_VERTEX` / `_UV1` by what the graph uses), sorted; at most 4.
    /// Counts cache hits too. Each is one more forward pipeline per host configuration.
    pub graph_permutations: Vec<Vec<String>>,
    /// `regia_render::FORWARD_PERMUTATION_BUDGET`, the ceiling one process should compose.
    pub permutation_budget: usize,
    /// What the cook did with the shared cook cache (ADR-2612); `None` without one.
    pub shared_cache: Option<crate::cook_cache::SharedCacheSummary>,
    /// Files copied into `assets/` from overlay roots before this cook (ADR-8155).
    /// `configured` is false when no overlay roots are set, and the vectors are empty.
    pub overlay_fill: crate::overlay_roots::OverlayFillSummary,
    /// The fonts of the cooked cache with their bytes before and after subsetting (ADR-2697): the ones this cook ran
    /// and, read back from their cooked rows, the ones an incremental cook skipped.
    pub font_subsets: Vec<crate::font_subset::FontSubsetSummary>,
    /// The shipped forward-shader cost per profile of the platform: permutations, generated WGSL
    /// and SPIR-V bytes, an ESTIMATED compile cost and the MEASURED naga time kept apart
    /// (ADR-2664). `None` until [`Self::attach_shader_cost`] runs: composing the shaders takes
    /// seconds, so [`cook_project`] leaves it to the caller (`regia cook` does it).
    pub shader_cost: Option<regia_render::ShaderCostReport>,
}

impl CookReport {
    /// Fills [`Self::shader_cost`] for `platform` with the project's `max_shader_bytes` budgets
    /// and adds a problem to [`Self::permutation_problems`] for every profile over its WGSL
    /// bytes budget (`regia package` fails a shipping package on any).
    pub fn attach_shader_cost(&mut self, project_root: &Path, platform: &str) {
        let report = crate::permutation_cook::project_shader_cost(project_root, platform, &mut self.warnings);
        self.permutation_problems.extend(report.budget_problems());
        self.shader_cost = Some(report);
    }
}

/// Cook options.
#[derive(Clone, Debug)]
pub struct CookOptions {
    /// Project root (contains `assets/`).
    pub project_root: PathBuf,
    /// Platform tag (`linux`, `windows`, ...). Defaults to host.
    pub platform: String,
    /// When true, only recook changed sources (default).
    pub incremental: bool,
}

impl CookOptions {
    /// Cook for the host platform.
    pub fn for_host(project_root: PathBuf) -> Self {
        Self {
            project_root,
            platform: host_platform().to_string(),
            incremental: true,
        }
    }
}

/// Cook every asset under `assets/`, with the project's shared cook cache (`Regia.toml`
/// `[cook.shared_cache]`, off by default; ADR-2612).
pub fn cook_project(options: CookOptions) -> Result<CookReport, CookError> {
    cook_project_with_shared_cache(options, &crate::cook_cache::SharedCacheChoice::Project)
}

/// [`cook_project`] with the shared cook cache `shared` picks (ADR-2612): sources whose outputs
/// the cache holds are fetched instead of imported, and a read-write cache receives what this
/// cook imports.
#[instrument(skip_all, fields(project = %options.project_root.display(), platform = %options.platform))]
pub fn cook_project_with_shared_cache(
    mut options: CookOptions,
    shared: &crate::cook_cache::SharedCacheChoice,
) -> Result<CookReport, CookError> {
    let started = Instant::now();
    // The platform tag picks the cache directory and the Android/desktop code paths by exact
    // string; `Android` on a case-insensitive volume would otherwise share `android`'s directory
    // while taking the desktop paths.
    options.platform = options.platform.trim().to_ascii_lowercase();
    // Checked before anything touches the disk: the tag is joined into `.regia/cooked/<tag>`.
    check_platform_tag(&options.platform)?;
    let assets_root = options.project_root.join("assets");
    let cache_dir = cooked_cache_dir(&options.project_root, &options.platform);
    std::fs::create_dir_all(&cache_dir)?;

    // Held until the cook returns: another cook of this project waits instead of losing rows.
    let _cook_lock = crate::db::CookLock::acquire(&options.project_root);
    let mut db = AssetDatabase::open(&options.project_root)?;
    // Orphaned `.meta` files go back next to their moved sources before anything would give a
    // moved source a new id (ADR-2428).
    let repaired = db.repair_orphaned_metas(&options.project_root, true);
    let mut ledger = CookLedger::open(&cache_dir, Some(&options.project_root), &db);
    let mut report = CookReport::default();
    report.warnings.extend(repaired.warning_lines());
    let mut manifest_map = read_manifest_map(&cache_dir, &mut report.warnings);
    let mut inputs = CookInputs::open(&options.project_root, &options.platform);
    // A bad `[cook.textures]` table fails the cook before anything is cooked with the wrong
    // formats (ADR-2063).
    if let Some(err) = inputs.texture_policy_error() {
        report.errors.push(("Regia.toml".to_string(), err.to_string()));
        report.elapsed_ms = started.elapsed().as_millis();
        return Ok(report);
    }
    // A bad `[cook] texture_quality` fails the cook the same way (ADR-7217).
    if let Some(err) = inputs.texture_quality_error() {
        report.errors.push(("Regia.toml".to_string(), err.to_string()));
        report.elapsed_ms = started.elapsed().as_millis();
        return Ok(report);
    }
    // A bad `[content.diet]` fails the cook the same way (ADR-8461): no texture may cook without
    // the cap the project asked for.
    if let Some(err) = inputs.diet_error() {
        report.errors.push(("Regia.toml".to_string(), err.to_string()));
        report.elapsed_ms = started.elapsed().as_millis();
        return Ok(report);
    }
    // A bad `config/import_rules.ron` fails the cook the same way (ADR-3321): no source may cook
    // with settings its rules could not contribute.
    if let Some(err) = inputs.import_rules_error() {
        report.errors.push((
            regia_asset::import_rules::IMPORT_RULES_FILE.to_string(),
            err.to_string(),
        ));
        report.elapsed_ms = started.elapsed().as_millis();
        return Ok(report);
    }
    // A bad `[cook.shared_cache]` table fails the cook the same way; an unreachable cache is a
    // warning (ADR-2612).
    if let Err(err) = crate::cook_cache::attach(&mut inputs, shared, &mut report.warnings) {
        report.errors.push(("Regia.toml".to_string(), err));
        report.elapsed_ms = started.elapsed().as_millis();
        return Ok(report);
    }
    // Missing glTF buffers and images are filled from `[cook.overlay_roots]` before discovery,
    // so the cook sees one tree. A bad table fails the cook the same way as the shared cache
    // (ADR-8155). The live cooker fills its targets the same way (ADR-8845).
    match crate::overlay_roots::fill_for_project(&options.project_root, &assets_root) {
        Ok(fill) => {
            if fill.configured {
                for missing in &fill.missing {
                    report
                        .warnings
                        .push(format!("overlay root {missing} is not a directory; skipped"));
                }
                for err in &fill.errors {
                    report.errors.push(("overlay fill".to_string(), err.clone()));
                }
            }
            if let Err(err) = crate::overlay_roots::sync_summary(&cache_dir, &fill) {
                report
                    .warnings
                    .push(format!("overlay fill summary was not written: {err}"));
            }
            report.overlay_fill = fill;
        }
        Err(err) => {
            report.errors.push(("Regia.toml".to_string(), err));
            report.elapsed_ms = started.elapsed().as_millis();
            return Ok(report);
        }
    }
    // The LOD class table's own problems, once per cook (each glTF re-reads it).
    report
        .warnings
        .extend(crate::lod_classes::LodClassTable::load(&options.project_root).1);

    let sources = AssetDatabase::discover_sources(&assets_root);
    let sidecars = gltf_inputs(&sources);
    let _shared_textures =
        crate::gltf_texture_share::Scope::enter(&options.project_root, &cache_dir, &sidecars)?;
    // Montage slots against the project's slot tables (spec 34 M1, ADR-7216): an unknown slot is
    // a load warning but a cook error. Montages still ship raw; this only checks them.
    report
        .errors
        .extend(crate::montage_cook::check_montage_slots(&sources, &assets_root));

    // Terrain sources cook through the tiled path (a separate pass below); their inputs are
    // consumed by that path, not cooked generically. Android writes the same tiles as ASTC 4×4
    // (`docs/formats/terrain.md` § 7).
    let terrains = crate::terrain_cook::discover_terrains(&sources);
    let terrain_inputs = terrain_input_files_for(&sources, &options.platform);
    // Remove manifest and asset-db entries for terrain input paths so an incremental recook
    // does not keep generic Heightfield/BC7 assets cooked before the tiled terrain path existed.
    for input in &terrain_inputs {
        let rel = input
            .strip_prefix(&assets_root)
            .map(normalise_asset_path)
            .unwrap_or_else(|_| normalise_asset_path(input));
        let rel_path = format!("assets/{rel}");
        if let Some(entry) = db.entries.remove(&rel_path) {
            remove_cached(&cache_dir, &entry.cooked_rel);
        }
        ledger.forget(&rel_path);
        manifest_map.remove(&rel_path);
    }

    // Data tables last: a chooser records the ids of the assets it names (`data_cook`).
    let (tables, others): (Vec<&PathBuf>, Vec<&PathBuf>) = sources
        .iter()
        .partition(|source| crate::data_cook::is_data_table(source));
    // Motion databases after the glTF files whose cooked skeletons and clips they read (ADR-2409).
    let (motion_dbs, others): (Vec<&PathBuf>, Vec<&PathBuf>) = others
        .into_iter()
        .partition(|source| crate::motiondb_cook::is_motiondb(source));
    for source in others.into_iter().chain(motion_dbs).chain(tables) {
        let rel = source
            .strip_prefix(&assets_root)
            .map(normalise_asset_path)
            .unwrap_or_else(|_| normalise_asset_path(source));
        let rel_path = format!("assets/{rel}");
        // Buffers and images a `.gltf` references by URI are imported through that glTF.
        // Only an explicit heightfield setting keeps a second standalone output.
        if sidecars.contains(source)
            && !crate::gltf_texture_share::standalone_is_heightfield(&options.project_root, source)
        {
            // The image's primary id may now hold a shared texture. Forget the standalone
            // skip proof so re-enabling its heightfield rule cannot reuse that texture blob.
            db.entries.remove(&rel_path);
            ledger.forget(&rel_path);
            inputs.forget(&rel_path);
            continue;
        }
        // Terrain layer files and their input textures cook through the tiled path.
        if is_terrain_layer(source) || terrain_inputs.contains(source) {
            continue;
        }
        // Material functions cook into the graphs that call them.
        if crate::matgraph_cook::is_material_function(source) {
            continue;
        }
        // Tiled tilesets are read through the maps that use them.
        if crate::tiled_import::is_tiled_tileset(source) {
            continue;
        }
        // Mesh-layer sources and cooked layers cook in the level pass below (ADR-8451).
        if crate::level_mesh_layers::is_mesh_layer_file(source) {
            continue;
        }
        // The bakes of `editor_only` generators stay out of the pak (spec 62 § 8, ADR-2421): drop
        // any entry an earlier cook made before the generator became editor-only.
        if is_pgout(source) && !regia_procgen::cook::bake_ships(source) {
            manifest_map.retain(|k, _| k != &rel_path);
            if let Some(entry) = db.entries.remove(&rel_path) {
                remove_cached(&cache_dir, &entry.cooked_rel);
            }
            ledger.forget(&rel_path);
            continue;
        }

        let cook_result = cook_one(
            source,
            &rel_path,
            &cache_dir,
            &db,
            &ledger,
            options.incremental,
            &mut inputs,
        );
        // Sub-asset re-maps and missing sub-assets of this source's re-import (ADR-2428).
        report.warnings.extend(crate::subasset_remap::take_warnings());
        // Unsupported and approximated MaterialX nodes (ADR-3341).
        report
            .warnings
            .extend(crate::materialx_import::take_cook_warnings());
        match cook_result {
            Ok(outcome) => {
                if outcome.skipped {
                    report.skipped += 1;
                } else {
                    report.cooked += 1;
                }
                drop_unproduced_sub_assets(&mut manifest_map, &cache_dir, &rel_path, &outcome);
                record_outcome(&mut db, &mut ledger, &mut manifest_map, &rel_path, outcome);
            }
            // A typed data table ships raw as well (ADR-2132): one that does not cook stays out of the pak
            // and its load in the game reports the same issues, so the cook goes on with a warning.
            Err(e) if crate::data_cook::is_typed_data_table(source) => {
                manifest_map.retain(|k, _| k != &rel_path);
                if let Some(entry) = db.entries.remove(&rel_path) {
                    remove_cached(&cache_dir, &entry.cooked_rel);
                }
                ledger.forget(&rel_path);
                report
                    .warnings
                    .push(format!("data table {rel_path} not cooked (it ships raw): {e}"));
            }
            Err(e) => report.errors.push((rel_path, e.to_string())),
        }
    }

    // Terrain pass: cook each terrain map through the tiled path (BCn on desktop, ASTC 4×4 on
    // Android).
    for terrain in &terrains {
        let rel = terrain
            .source
            .strip_prefix(&assets_root)
            .map(normalise_asset_path)
            .unwrap_or_else(|_| normalise_asset_path(&terrain.source));
        let rel_path = format!("assets/{rel}");
        // The terrain skip trusts its index file; a tile or texture file missing from the cache
        // (a partial clean, a failed copy) makes this cook a full one, so no package ships an
        // index naming a tile that is not there.
        let incremental = options.incremental && sub_files_present(&manifest_map, &cache_dir, &rel_path);
        match crate::terrain_cook::cook_terrain_with_ledger(
            &terrain.source,
            &rel_path,
            &cache_dir,
            &options.platform,
            &ledger,
            incremental,
        ) {
            Ok(outcome) => {
                if outcome.empty {
                    // A layer file with no heightfield or layer texture is not a terrain: drop any
                    // stale entries for it and leave it uncooked.
                    manifest_map.retain(|k, _| k != &rel_path && !k.starts_with(&format!("{rel_path}#")));
                    if let Some(entry) = db.entries.remove(&rel_path) {
                        remove_cached(&cache_dir, &entry.cooked_rel);
                    }
                    ledger.forget(&rel_path);
                    report.warnings.push(format!(
                        "terrain {rel_path}: no resolvable heightfield or layer texture; not cooked"
                    ));
                    continue;
                }
                if outcome.skipped {
                    report.skipped += 1;
                } else {
                    report.cooked += 1;
                }
                report.warnings.extend(outcome.warnings.iter().cloned());
                record_outcome(
                    &mut db,
                    &mut ledger,
                    &mut manifest_map,
                    &rel_path,
                    CookOutcome::from(outcome),
                );
            }
            Err(e) => report.errors.push((rel_path, e.to_string())),
        }
    }

    // Mesh layers of every level that has them, from its terrain block and the heightfield the
    // terrain pass just cooked (ADR-8451). Before the streamed levels: no cell reads them.
    crate::level_mesh_layers::cook_project_mesh_layers(
        &assets_root,
        &cache_dir,
        &mut manifest_map,
        &mut report,
    );

    // Streamed levels (spec 78 § 6.4): cells, the always-loaded set, the world index.
    crate::stream_cook::cook_project_levels(
        &assets_root,
        &cache_dir,
        &options.platform,
        &mut manifest_map,
        &mut report,
    );

    // Rows of sources deleted since the last cook leave the database, the ledger and the
    // manifest (and their cooked files the cache), so a package never ships them.
    drop_deleted_sources(
        &options.project_root,
        &cache_dir,
        &mut db,
        &mut ledger,
        &mut manifest_map,
    );
    report
        .warnings
        .extend(crate::gltf_texture_share::publish(&mut manifest_map));

    // The shared cache's summary goes into the ledger (ADR-2612).
    report.shared_cache = crate::cook_cache::finish(&mut inputs, &mut report.warnings);
    if let Some(summary) = &report.shared_cache {
        ledger.set_shared_summary(summary.clone());
        crate::cook_cache::write_summary_json(&cache_dir, summary, &mut report.warnings);
    }
    // Derived data, like the registry: a write failure is a warning.
    report.font_subsets =
        crate::font_subset::report_rows(&cache_dir, &db, crate::font_subset::take_summaries());
    if let Err(e) = crate::font_subset::write_report(&cache_dir, &report.font_subsets) {
        report
            .warnings
            .push(format!("font subset report not written: {e}"));
    }

    // The ledger before the database and the manifest: a cook killed in between leaves a
    // ledger that only names outputs this cache holds, and the legacy adoption in
    // `CookLedger::open` never sees a missing ledger again.
    ledger.save()?;
    db.save(&options.project_root)?;
    AssetManifest::from_map(manifest_map)
        .write_to_cache(&cache_dir)
        .map_err(CookError::Manifest)?;
    // The asset registry (tags, types, dependencies) next to the manifest. Derived data: a failure
    // is a warning, the next cook writes it again.
    match crate::registry_build::write_cache_registry(&options.project_root, &cache_dir, &db) {
        Ok(built) => report.warnings.extend(built.warnings),
        Err(e) => report.warnings.push(format!("asset registry not written: {e}")),
    }
    // The loudness table of every cooked sound (ADR-1855). Derived data, like the registry.
    match crate::audio_cook::write_loudness_table(&options.project_root, &cache_dir, &db) {
        Ok((_, problems)) => report.warnings.extend(problems),
        Err(e) => report.warnings.push(format!("loudness table not written: {e}")),
    }
    // What each sound ships as, and its bytes before and after (ADR-3230).
    match crate::audio_transcode::write_audio_ship_report(
        &options.project_root,
        &cache_dir,
        &options.platform,
        &db,
    ) {
        Ok(lines) => report.audio_ship = lines,
        Err(e) => report
            .warnings
            .push(format!("audio ship report not written: {e}")),
    }
    // A cache: when it cannot be written the next cook hashes and imports again.
    if let Err(e) = inputs.save() {
        tracing::debug!(target: "regia_asset_pipeline", "cook inputs not saved: {e}");
    }
    report.graph_permutations = cooked_graph_permutations(&db, &cache_dir, &mut report.warnings);
    report.permutation_budget = regia_render::FORWARD_PERMUTATION_BUDGET;
    report.imports_skipped = inputs.counters.imports_skipped;
    report.hashed_bytes = inputs.counters.hashed_bytes;

    // Shader permutations per profile (ADR-2026): derived data like the registry, so a write
    // failure is a warning.
    let permutations = crate::permutation_cook::project_permutations(
        &options.project_root,
        &db,
        &cache_dir,
        &options.platform,
        &mut report.warnings,
    );
    report.permutations = permutations
        .profiles
        .iter()
        .map(regia_render::permutation_policy::ProfilePermutations::summary_line)
        .collect();
    report.permutation_problems = permutations.budget_problems();
    let graph_checks =
        crate::permutation_cook::graph_cook_checks(&db, &cache_dir, &options.platform, &mut report.warnings);
    report.material_graph_checks = graph_checks.lines;
    report.permutation_problems.extend(graph_checks.problems);
    if let Err(e) = crate::permutation_cook::write_manifest(&cache_dir, &permutations) {
        report
            .warnings
            .push(format!("permutation manifest not written: {e}"));
    }

    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

/// True when the file name `name` ends with `suffix`, ignoring ASCII case. Multi-part suffixes
/// (`.material.ron`) must match the way the single extension does (lowercased), because Explorer
/// and Finder preserve whatever case the author typed.
pub(crate) fn name_ends_with(name: &str, suffix: &str) -> bool {
    let (n, s) = (name.as_bytes(), suffix.as_bytes());
    n.len() >= s.len() && n[n.len() - s.len()..].eq_ignore_ascii_case(s)
}

/// Deletes a cooked file named by the asset database. The database is a cache file that can travel
/// with a shared project, so a name that is absolute, holds `..` or leads out of the cache through a
/// symlink is ignored instead of deleted (ADR-1816).
pub(crate) fn remove_cached(cache_dir: &Path, cooked_rel: &str) {
    match regia_core::project_path::resolve_write(cache_dir, cooked_rel) {
        Ok(path) => {
            let _ = std::fs::remove_file(path);
        }
        Err(e) => tracing::warn!(
            target: "regia_asset_pipeline",
            "asset db names a cooked file outside the cache ({}): `{cooked_rel}` left alone",
            e.code()
        ),
    }
}

/// Joins a project-relative `rel` named by a content file onto the project root, confined to it: no
/// absolute, drive or UNC path, no `..`, no symlink leading out (ADR-1816). A missing file is not an
/// error here (the caller's read reports it).
///
/// # Errors
///
/// A message naming the rule `rel` breaks.
pub(crate) fn join_in_project(project_root: &Path, rel: &str) -> Result<PathBuf, String> {
    // `\` is a separator on every host, as in `join_portable`.
    let rel = rel.replace('\\', "/");
    regia_core::project_path::resolve_write(project_root, &rel)
        .map_err(|e| format!("`{rel}` must stay inside the project ({}): {e}", e.code()))
}

/// What a path named by a content file in `dir` must stay under: the project above `dir`. A folder
/// that reaches the project only through a symlink (a kit linked into `assets/`) really lives
/// elsewhere, so the bound is the target of the deepest link on the way to it: `kit/models` still
/// reaches `kit/textures`. `None` for a file outside any project (an inspected download, a test's
/// temp directory), which has nothing to leak.
fn content_bound(dir: &Path) -> Option<PathBuf> {
    use regia_core::brand::{LEGACY_MANIFEST_FILE, MANIFEST_FILE};
    let is_project = |a: &&Path| a.join(MANIFEST_FILE).is_file() || a.join(LEGACY_MANIFEST_FILE).is_file();
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let canonical = dir.canonicalize().ok()?;
    if let Some(project) = canonical.ancestors().find(is_project) {
        return Some(project.to_path_buf());
    }
    let spelled = std::path::absolute(dir).ok()?;
    let project = spelled.ancestors().find(is_project)?;
    let link = spelled
        .ancestors()
        .take_while(|a| *a != project)
        .find(|a| a.is_symlink());
    Some(link.and_then(|l| l.canonicalize().ok()).unwrap_or(canonical))
}

/// `path` with its `.` and `..` segments folded by text, as Win32 opens it: a `..` after a folder that
/// does not exist would otherwise hide from a check that walks to the deepest existing ancestor.
fn lexically_clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Join a path a content file names (`rel`, relative to that file) onto its directory `base` the same
/// way on every host: `\\` is a separator, and an absolute path or a drive prefix (`C:/x`, which
/// Windows treats as absolute and Linux as a relative name) is an error rather than an escape from
/// `base`. `..` reaches sibling folders, as kits do, but never past the project `base` is in: a shared
/// glTF that names `../../../home/me/.config/x.png` would otherwise be cooked into the build. Every
/// other segment takes the shared rule (`regia_core::project_path::check_relative`, ADR-4922): no
/// device name, no `:` stream, no name Windows reads as `..`.
///
/// # Errors
///
/// A message naming the offending path.
pub(crate) fn join_portable(base: &Path, rel: &str) -> Result<PathBuf, String> {
    join_bounded(base, rel, content_bound(base).as_deref())
}

/// [`join_portable`] with the bound given: the joined path, `..` folded by text and symlinks
/// resolved, must stay under `bound` (no bound check when `None`).
///
/// # Errors
///
/// A message naming the offending path and the rule's code.
pub(crate) fn join_bounded(base: &Path, rel: &str, bound: Option<&Path>) -> Result<PathBuf, String> {
    let rel = rel.replace('\\', "/");
    let bytes = rel.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if rel.starts_with('/') || drive {
        return Err(format!(
            "`{rel}` is absolute; use a path relative to the referencing file"
        ));
    }
    // `..` is allowed here (it is bounded below); every named segment takes the shared rule.
    for segment in rel.split('/').filter(|s| !matches!(*s, "" | "." | "..")) {
        regia_core::project_path::check_relative(segment)
            .map_err(|e| format!("`{rel}` is not a portable path ({}): {e}", e.code()))?;
    }
    let joined = base.join(&rel);
    if let Some(bound) = bound {
        // By text for a `..` behind a missing folder, as given for a `..` behind a symlink.
        let start = if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            base
        };
        let folded = lexically_clean(&start.canonicalize().map_err(|e| e.to_string())?.join(&rel));
        [folded.as_path(), start.join(&rel).as_path()]
            .into_iter()
            .try_for_each(|p| regia_core::project_path::check_under_root(bound, p))
            .map_err(|e| format!("`{rel}` leaves the project ({}): {e}", e.code()))?;
    }
    Ok(joined)
}

/// What every reference of a source in `dir` must stay under when the source is cooked on its own:
/// the project around it ([`join_portable`]'s bound), else `dir` itself, so a source outside any
/// project reaches only the files below its own folder (finding 19, ADR-4922). `None` when `dir`
/// cannot be resolved.
pub(crate) fn reference_bound(dir: &Path) -> Option<PathBuf> {
    content_bound(dir).or_else(|| {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        dir.canonicalize().ok()
    })
}

/// A file a model names (`name`, `/` or `\\` separated) inside `dir`, as a clean `/`-separated path
/// relative to it, when the file exists there: the shared rule (no `..`, no absolute, drive or
/// device name, ADR-4922), and inside a project the file, symlinks resolved, stays in the project
/// ([`join_portable`]'s bound). The OBJ `mtllib` and texture references and the FBX texture
/// references resolve through this.
pub(crate) fn inside_dir(dir: &Path, name: &str) -> Option<String> {
    let normal = name.trim().replace('\\', "/");
    let clean = regia_core::project_path::check_relative(&normal).ok()?;
    let clean: Vec<String> = clean
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let clean = clean.join("/");
    (dir.join(&clean).is_file() && join_portable(dir, &clean).is_ok()).then_some(clean)
}

/// The cache manifest's entries by path. A manifest that does not decode is a damaged cache file
/// (a cook killed mid-write before writes were atomic): it is reported as a warning and rebuilt
/// by this cook, which records every source again, instead of failing every cook after it.
/// The distinct forward define sets of every cooked material graph in `db` (instances carry
/// their parent's compiled code, so they count by what they compiled to). A graph whose cooked
/// file cannot be read adds a warning and is left out.
fn cooked_graph_permutations(
    db: &AssetDatabase,
    cache_dir: &Path,
    warnings: &mut Vec<String>,
) -> Vec<Vec<String>> {
    let mut sets = std::collections::BTreeSet::new();
    for (path, entry) in &db.entries {
        if !(name_ends_with(path, ".matgraph.ron")
            || name_ends_with(path, ".matinst.ron")
            || name_ends_with(path, ".mtlx"))
        {
            continue;
        }
        let graph = std::fs::read(cache_dir.join(&entry.cooked_rel))
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                let (_, payload) = regia_asset::decode_cooked_file(&bytes).map_err(|e| e.to_string())?;
                regia_matgraph::CookedMaterialGraph::from_postcard(payload).map_err(|e| e.to_string())
            });
        match graph {
            Ok(g) => {
                sets.insert(regia_render::graph_permutation_defines(
                    g.vertex_wgsl.is_some(),
                    g.uses_uv1,
                ));
            }
            Err(e) => warnings.push(format!(
                "{path}: permutation count skipped, cooked graph unreadable: {e}"
            )),
        }
    }
    sets.into_iter().collect()
}

pub(crate) fn read_manifest_map(
    cache_dir: &Path,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, ManifestEntry> {
    let mut map = BTreeMap::new();
    match AssetManifest::read_from_cache(cache_dir) {
        Ok(Some(existing)) => {
            for e in existing.entries {
                map.insert(e.path.clone(), e);
            }
        }
        Ok(None) => {}
        Err(error) => warnings.push(format!(
            "cache manifest {} is damaged ({error}); rebuilding it",
            cache_dir.join("asset_manifest.bin").display()
        )),
    }
    map
}

/// True when `source` is a terrain layer file (cooked through the tiled terrain path).
pub(crate) fn is_terrain_layer(source: &Path) -> bool {
    source
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| crate::cook::name_ends_with(n, "terrain.layers.ron"))
}

/// Referenced buffers and images cooked only through a `.gltf` or `.glb`, with no standalone
/// database row. Explicit heightfield settings retain a standalone output and are excluded.
/// Sources must be under the project's `assets/` directory, as returned by asset discovery.
/// Shared with `regia_cli`'s pak-freshness check: these are the sources `cook_project` skips.
pub fn gltf_sidecars(sources: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    gltf_inputs(sources)
        .into_iter()
        .filter(|path| {
            // This public freshness helper has no root parameter. Prefer an authored project
            // manifest; synthetic/minimal projects still have the standard assets/ directory.
            let root = path
                .ancestors()
                .find(|dir| {
                    dir.join(regia_core::brand::MANIFEST_FILE).is_file()
                        || dir.join(regia_core::brand::LEGACY_MANIFEST_FILE).is_file()
                })
                .or_else(|| {
                    path.ancestors()
                        .find(|dir| dir.file_name().is_some_and(|name| name == "assets"))
                        .and_then(Path::parent)
                });
            !root.is_some_and(|root| crate::gltf_texture_share::standalone_is_heightfield(root, path))
        })
        .collect()
}

/// Every referenced input spelling, including images that also cook as standalone heightfields.
pub(crate) fn gltf_inputs(sources: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    for gltf_path in sources {
        for path in gltf_sidecars_of(gltf_path) {
            // Discovery spells `assets/image.png`, even when the URI was `../image.png`.
            // Both spellings are consumed by the glTF; do not also run a standalone encode.
            let clean = lexically_clean(&path);
            if clean == path
                || path
                    .canonicalize()
                    .ok()
                    .zip(clean.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b)
            {
                out.insert(clean);
            }
            out.insert(path);
        }
    }
    out
}

/// The files one `.gltf` or `.glb` references by relative URI (empty for anything else, or an
/// unreadable file). See [`gltf_sidecars`]. A `.glb` may name external buffers and images too
/// (glTF 2.0 § 4.4); they are part of its import and of its cache key (audit-v035-cook M2).
pub(crate) fn gltf_sidecars_of(gltf_path: &Path) -> Vec<PathBuf> {
    let mut paths = gltf_uri_inputs_of(gltf_path, false);
    let canonical: Vec<_> = paths
        .iter()
        .filter_map(|path| {
            let resolved = std::fs::canonicalize(path).ok()?;
            let absolute = std::path::absolute(path).ok()?;
            (resolved != lexically_clean(&absolute)).then_some(resolved)
        })
        .collect();
    // A symlink's own .meta supplies its overrides, while the canonical image's .meta owns
    // its shared id. Both must participate in the incremental/shared-cache input key.
    paths.extend(canonical);
    paths
}

/// Give external images authoring identities before either local or shared cache lookup.
/// Images still cook as the owning glTF's sub-assets; buffers have no image metadata.
fn ensure_gltf_image_metas(source: &Path) -> Result<(), CookError> {
    let bound = source.parent().and_then(reference_bound);
    for image in gltf_uri_inputs_of(source, true) {
        if !image.is_file() {
            continue; // The importer reports the missing image; do not mint an orphan metadata file.
        }
        // Check the metadata path too: an existing `.meta` symlink must not read outside
        // the same boundary used for the image URI (including linked content kits).
        if let Some(bound) = &bound {
            regia_core::project_path::check_under_root(bound, &meta_path_for(&image))
                .map_err(|e| CookError::Import(ImportError::Gltf(e.to_string())))?;
        }
        let ext = image.extension().and_then(|s| s.to_str()).unwrap_or("png");
        AssetDatabase::load_or_create_meta(&image, &ext.to_ascii_lowercase()).map_err(|error| {
            CookError::Import(ImportError::Image(format!(
                "{}: {error}",
                meta_path_for(&image).display()
            )))
        })?;
    }
    Ok(())
}

/// The JSON chunk of a `.glb` (glTF 2.0 § 4.4: 12-byte header, then the JSON chunk first), read
/// without the binary chunk; `None` when the file is not a readable GLB.
fn glb_json_chunk(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 20];
    file.read_exact(&mut head).ok()?;
    if &head[0..4] != b"glTF" || &head[16..20] != b"JSON" {
        return None;
    }
    let len = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    let total = file.metadata().ok()?.len();
    if u64::from(len) > total.saturating_sub(20) {
        return None;
    }
    let mut json = Vec::new();
    file.take(u64::from(len)).read_to_end(&mut json).ok()?;
    Some(json)
}

/// The files a `.gltf` or `.glb` references by relative URI (external buffers and images; empty
/// for anything else or an unreadable file).
fn gltf_uri_inputs_of(gltf_path: &Path, images_only: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    {
        let ext = gltf_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_glb = ext.eq_ignore_ascii_case("glb");
        if !is_glb && !ext.eq_ignore_ascii_case("gltf") {
            return out;
        }
        let Some(dir) = gltf_path.parent() else { return out };
        let read = if is_glb {
            glb_json_chunk(gltf_path)
        } else {
            std::fs::read(gltf_path).ok()
        };
        let Some(bytes) = read else {
            return out;
        };
        out.extend(gltf_uri_inputs_from_bytes(&bytes, Some(dir), images_only));
    }
    out
}

/// The files a `.gltf` JSON or `.glb` binary references by relative URI (external buffers and
/// images), joined against `dir`; empty when the document does not parse or `dir` is `None`.
/// Used by [`gltf_uri_inputs_of`] and by the converted FBX/OBJ cook, whose glb is in memory.
fn gltf_uri_inputs_from_bytes(bytes: &[u8], dir: Option<&Path>, images_only: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(dir) = dir else { return out };
    // Reject a hostile GLB header (declared length < 12) up front: the `gltf` crate reads it
    // as `length - 12` and underflow-panics in debug, and a `catch_unwind` cannot contain that
    // under `panic = "abort"` (shipping). A malformed sidecar is simply skipped.
    if crate::gltf_import::reject_hostile_glb_header(bytes).is_err() {
        return out;
    }
    let Ok((doc, raw)) = crate::gltf_import::checked_parse(bytes) else {
        return out;
    };
    // A fallback buffer's file is never read (`EXT_meshopt_compression`), so it is no input.
    let buffer_uris = doc
        .buffers()
        .filter(|_| !images_only)
        .filter(|b| !crate::gltf_normalize::is_fallback_buffer(&raw, b.index()))
        .filter_map(|b| match b.source() {
            gltf::buffer::Source::Uri(uri) => Some(uri),
            gltf::buffer::Source::Bin => None,
        });
    let image_uris = doc.images().filter_map(|i| match i.source() {
        gltf::image::Source::Uri { uri, .. } => Some(uri),
        gltf::image::Source::View { .. } => None,
    });
    for uri in buffer_uris.chain(image_uris) {
        if uri.contains(':') {
            continue;
        }
        let decoded = crate::gltf_import::decode_gltf_uri(uri).unwrap_or_else(|| uri.to_string());
        // A URI outside the project is not a sidecar: the import reports it.
        out.extend(join_portable(dir, &decoded).ok());
    }
    out
}

/// The terrain input files the tiled terrain path consumes. Desktop and Android both cook this
/// set through the tiled path (BCn or ASTC 4×4), so neither records a generic database row for
/// them. `cook_project` skips exactly this set (the terrain layer files themselves are handled in
/// the terrain pass), so the pak-freshness check in `regia_cli` must skip the same set. Shared so
/// the two stay in sync. `platform` is part of the signature callers already use; the set does
/// not depend on it.
pub fn terrain_input_files_for(sources: &[PathBuf], platform: &str) -> std::collections::BTreeSet<PathBuf> {
    let _ = platform;
    let mut out = std::collections::BTreeSet::new();
    for terrain in crate::terrain_cook::discover_terrains(sources) {
        if let Ok(files) = crate::terrain_cook::terrain_input_files(&terrain.source) {
            out.extend(files);
        }
    }
    out
}

pub(crate) struct CookOutcome {
    pub(crate) primary_id: AssetId,
    pub(crate) type_path: String,
    pub(crate) source_hash: [u8; 32],
    pub(crate) settings_hash: [u8; 32],
    pub(crate) importer: String,
    pub(crate) importer_version: u32,
    pub(crate) cache_key: [u8; 32],
    pub(crate) cooked_rel: String,
    pub(crate) skipped: bool,
    pub(crate) sub_manifest: Vec<ManifestEntry>,
}

impl From<crate::terrain_cook::TerrainCookOutcome> for CookOutcome {
    fn from(t: crate::terrain_cook::TerrainCookOutcome) -> Self {
        Self {
            primary_id: t.primary_id,
            type_path: t.type_path,
            source_hash: t.source_hash,
            settings_hash: t.settings_hash,
            importer: t.importer,
            importer_version: t.importer_version,
            cache_key: t.cache_key,
            cooked_rel: t.cooked_rel,
            skipped: t.skipped,
            sub_manifest: t.sub_manifest,
        }
    }
}

/// Importers whose outcome lists every sub-asset of the source (`sub_manifest` is complete, also
/// on a skip): a recook drops the source's sub rows it no longer produces.
const SUB_ASSET_IMPORTERS: &[&str] = &["gltf", "fbx", "obj", "ply", "stl", "bvh", "ldtk", "usd"];

/// Removes `rel_path#…` manifest rows (and their cooked files) that a model's recook no longer
/// produces: a mesh or material deleted from the file must not stay loadable from the package.
fn drop_unproduced_sub_assets(
    manifest_map: &mut BTreeMap<String, ManifestEntry>,
    cache_dir: &Path,
    rel_path: &str,
    outcome: &CookOutcome,
) {
    if !SUB_ASSET_IMPORTERS.contains(&outcome.importer.as_str()) {
        return;
    }
    let prefix = format!("{rel_path}#");
    let produced: BTreeSet<&str> = outcome.sub_manifest.iter().map(|m| m.path.as_str()).collect();
    let stale: Vec<String> = manifest_map
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, _)| !produced.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect();
    for key in stale {
        if let Some(entry) = manifest_map.remove(&key)
            && !outcome.sub_manifest.iter().any(|m| m.id == entry.id)
            && !manifest_map.values().any(|m| m.id == entry.id)
        {
            remove_cached(cache_dir, &cooked_file_name(entry.id));
        }
    }
}

/// Whether the last cook's `rel_path#…` manifest rows all have their cooked file in the cache
/// (`false` when there are none: nothing proves the outputs are there).
fn sub_files_present(
    manifest_map: &BTreeMap<String, ManifestEntry>,
    cache_dir: &Path,
    rel_path: &str,
) -> bool {
    let prefix = format!("{rel_path}#");
    let mut rows = manifest_map
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .peekable();
    rows.peek().is_some() && rows.all(|(_, entry)| cache_dir.join(cooked_file_name(entry.id)).is_file())
}

/// Drops every database row and manifest row (primary and `#` sub rows) whose `assets/` source no
/// longer exists in the project, deleting their cooked files. Extension rows (`ext://`) are
/// pruned by the extension cook.
///
/// A cooked file is named by its id alone, and a source moved with its `.meta` keeps its ids
/// (ADR-2528): the rows this cook just wrote for the new path own the same files as the old path's
/// rows. So the files go only after every gone row has left, and only those no remaining row names.
fn drop_deleted_sources(
    project_root: &Path,
    cache_dir: &Path,
    db: &mut AssetDatabase,
    ledger: &mut CookLedger,
    manifest_map: &mut BTreeMap<String, ManifestEntry>,
) {
    let gone = |rel: &str| rel.starts_with("assets/") && !project_root.join(rel).exists();
    let mut dropped_files: Vec<String> = Vec::new();
    let gone_rows: Vec<String> = db
        .entries
        .keys()
        .filter(|rel| gone(rel.as_str()))
        .cloned()
        .collect();
    for rel in gone_rows {
        if let Some(entry) = db.entries.remove(&rel) {
            dropped_files.push(entry.cooked_rel);
        }
        ledger.forget(&rel);
    }
    let gone_manifest: Vec<String> = manifest_map
        .keys()
        .filter(|key| gone(key.split('#').next().unwrap_or(key.as_str())))
        .cloned()
        .collect();
    for key in gone_manifest {
        if let Some(entry) = manifest_map.remove(&key) {
            dropped_files.push(cooked_file_name(entry.id));
        }
    }
    if dropped_files.is_empty() {
        return;
    }
    let still_named: BTreeSet<String> = db
        .entries
        .values()
        .map(|entry| entry.cooked_rel.clone())
        .chain(manifest_map.values().map(|entry| cooked_file_name(entry.id)))
        .collect();
    for file in dropped_files {
        if !still_named.contains(&file) {
            remove_cached(cache_dir, &file);
        }
    }
}

/// Writes one cooked source's database row, its key in this platform's ledger and its manifest
/// entries (primary plus sub-assets).
pub(crate) fn record_outcome(
    db: &mut AssetDatabase,
    ledger: &mut CookLedger,
    manifest_map: &mut BTreeMap<String, ManifestEntry>,
    rel_path: &str,
    outcome: CookOutcome,
) {
    ledger.record(rel_path, outcome.cache_key);
    // A font ships as a raw file (`font_subset::stage_fonts`): its cooked bytes stay out of every pak.
    let in_manifest = outcome.importer != crate::font_subset::FONT_IMPORTER;
    db.entries.insert(
        rel_path.to_string(),
        DbEntry {
            source_path: rel_path.to_string(),
            asset_id: outcome.primary_id,
            source_hash: outcome.source_hash,
            settings_hash: outcome.settings_hash,
            importer: outcome.importer,
            importer_version: outcome.importer_version,
            cache_key: outcome.cache_key,
            cooked_rel: outcome.cooked_rel,
        },
    );
    if !in_manifest {
        manifest_map.remove(rel_path);
        return;
    }
    manifest_map.insert(
        rel_path.to_string(),
        ManifestEntry {
            path: rel_path.to_string(),
            id: outcome.primary_id,
            type_path: outcome.type_path,
        },
    );
    // A renamed sub-asset keeps its id under its new path (ADR-2428): the old path's row goes, so
    // one id never has two paths. The rows of `rel_path#` are one contiguous key range.
    if !outcome.skipped && !outcome.sub_manifest.is_empty() {
        let prefix = format!("{rel_path}#");
        let moved: Vec<String> = manifest_map
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter(|(key, entry)| {
                outcome
                    .sub_manifest
                    .iter()
                    .any(|sub| sub.id == entry.id && &sub.path != *key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in moved {
            manifest_map.remove(&key);
        }
    }
    for sub in outcome.sub_manifest {
        manifest_map.insert(sub.path.clone(), sub);
    }
}

/// Packed importer version. glTF and `.material.ron` are at least 1.2.0 because their
/// cooked output embeds [`CookedStandardMaterial`] version 3 (tagged texture
/// slots plus the v2 hard-surface fields). glTF is 1.8.0: meshes cook at cooked mesh format v2
/// (skin joints and weights, `COLOR_0`, sparse morph targets; ADR-226). 1.7.0: generated mesh
/// LOD screen sizes are the simplifier's error (the payload gained `lod_metric`) on top of 1.6.0.
/// 1.6.0: its packed ORM's roughness
/// mips carry the normal map's variance (Toksvig, ADR-091, 1.3.0), it cooks a `model`
/// sub-asset (1.4.0), and generated tangents have their bitangent pointing up the image as
/// glTF normal textures expect (look-r2; earlier cooks flipped the normal map's green
/// channel, 1.5.0). 1.6.0 drops coincident duplicate triangles (shadow-detail). Older rows
/// miss the cache key and are rewritten. Other importers stay at 1.0.0 (PNG/JPEG: below).
///
/// PNG/JPEG are 1.1.0 (lane tex-budget): the usage follows the file name and the `.meta`
/// overrides (`max_size`, `compress`, `quality`), so a normal map that 1.0.0 cooked as sRGB BC7
/// recooks as BC5. The cooked format (`CookedTexture`, KTX2) is unchanged.
/// The Android texture cook's ASTC policy, in the Android cache key ([`cook_one`]): `astc-2`
/// (lane android-astc) cooks 4×4 at the default quality for every usage and keeps every mip
/// down to 1×1; earlier Android cooks used 6×6/5×5/8×8 where the size allowed and dropped the
/// levels smaller than a block. The desktop (BCn) output is unchanged, so desktop rows keep
/// their keys.
const ANDROID_ASTC_REVISION: &str = "astc-2";

/// glTF importer version packed into cooked cache keys and the source-lint decode cache.
/// 1.14.2: both 1.14.0 lines (node clips, ADR-8157; external image ids, ADR-8055) and the
/// 1.14.1 heightfield opt-in (ADR-8143) in one importer (ADR-8574); older notes below.
#[must_use]
pub fn gltf_importer_version() -> u32 {
    pack_importer_version(1, 14, 2)
}

pub(crate) fn importer_version_for(source: &Path, ext: &str) -> u32 {
    let material_ron = ext == "ron"
        && source
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| crate::cook::name_ends_with(n, ".material.ron"));
    if matches!(ext, "glb" | "gltf") {
        // 1.14.2: both 1.14.0 lines below in one importer, so a cache from either re-cooks (ADR-8574).
        // 1.14.1: referenced L16 images need an explicit heightfield opt-in (ADR-8143).
        // 1.14.0: external project images use standalone/variant ids (ADR-8055).
        // 1.13.1: authored LOD defaults halve past LOD2 (ADR-7869); invalidate cached chains.
        // 1.14.0: plain-node TRS channels become cooked node clips (ADR-8157).
        // 1.13.0: `KHR_mesh_quantization`, `EXT_meshopt_compression`, `EXT_mesh_gpu_instancing`,
        // `EXT_texture_webp` and `KHR_materials_variants` are read, and required extensions the
        // importer cannot read stop the import (ADR-3016; the lane wrote 1.12.0, which ADR-2732
        // had taken meanwhile, so the integration renumbered it).
        // 1.12.0: `extras.regia_transmission_roughness` reaches the cooked material (ADR-2732).
        // 1.11.0: `KHR_materials_iridescence` reaches the cooked material (ADR-1977).
        // 1.10.0: a material's `REGIA_materials_detail` reaches `HardSurfaceParams::detail`.
        // 1.9.0: nested nodes convert once (not per level), non-indexed primitives get face
        // normals, duplicate node, material, skin and clip names get distinct sub-asset ids.
        // 1.8.0: cooked mesh format v2 (skin, colour and morph blocks; skinned meshes were
        // cooked rigid before). 1.7.0: generated LOD `screen_relative_size` is the meshopt error at one pixel on a
        // 1080p display, and the payload records `lod_metric` (v020-lod-select). 1.6.0 dropped
        // coincident duplicate triangles (shadow-detail). 1.5.0: glTF-convention generated
        // tangents (look-r2) on top of 1.4.0's `model` sub-asset (cooked-runtime) and 1.3.0's
        // Toksvig ORM mips (spec-aa).
        gltf_importer_version()
    } else if matches!(ext, "fbx" | "obj" | "ply" | "stl") {
        // The FBX (ADR-1602) and OBJ importers convert to glTF and run the glTF importer, so
        // their outputs follow glTF 1.11.0's; bump them when either side changes. 1.10.0 and 1.13.0
        // read extensions neither converter writes, and so did 1.12.0's extras; 1.1.0 is the
        // material v4 output (ADR-1977). 1.2.0: generated LODs simplify with the normal and
        // UV0 weights (the glTF path keys that with its `lod_attr` term; the converted key has
        // none, so a cached FBX/OBJ primary mesh kept its old LODs).
        // 1.2.1: follow glTF's corrected authored LOD defaults (ADR-7869).
        pack_importer_version(1, 2, 1)
    } else if ext == "psd" {
        // ADR-3244: the PSD importer's first version.
        pack_importer_version(1, 0, 0)
    } else if ext == "bvh" {
        // ADR-2532: the BVH importer's first version.
        pack_importer_version(1, 0, 0)
    } else if crate::usd_import::is_usd_extension(ext) {
        // ADR-3235: the USD importer's first version (converted to glTF like FBX and OBJ; bump it
        // when either side changes).
        // 1.0.1: follow glTF's corrected authored LOD defaults (ADR-7869).
        pack_importer_version(1, 0, 1)
    } else if material_ron {
        // 1.5.0: the cooked header lists the asset ids the material's texture slots name
        // (texture-set materials, ADR-3320), so a pak's chunking pulls the maps in. 1.4.0:
        // material v5 output, `transmission_roughness` reaches the cook (ADR-2075).
        pack_importer_version(1, 5, 0)
    } else if matches!(ext, "png" | "jpg" | "jpeg") {
        // 1.2.0: the shared texture-suffix table (ADR-3320) maps a `_NormalDX` source to the
        // normal-map usage, and the `.meta` `normal_green` / `orm_channel_order` settings of
        // spec 22 T2.14 flip the green / swizzle the packed channels at cook. 1.1.0 (lane
        // tex-budget) came first.
        pack_importer_version(1, 2, 0)
    } else if ext == "svg" {
        // ADR-3350: the SVG importer's first version.
        pack_importer_version(1, 0, 0)
    } else if ext == "dds" || crate::audio_cook::is_audio_extension(ext) {
        // ADR-1855: DDS pass-through/decode and loudness at import, first versions; TGA and BMP
        // take the default below.
        pack_importer_version(1, 0, 0)
    } else {
        pack_importer_version(1, 0, 0)
    }
}

/// Cooks one source. Incrementally, a glTF or PNG/JPEG whose inputs are unchanged since its
/// last cook is skipped without running its importer (`cook_inputs`, ADR-530); every other
/// source is imported and skipped only when its cache key matches.
pub(crate) fn cook_one(
    source: &Path,
    rel_path: &str,
    cache_dir: &Path,
    db: &AssetDatabase,
    ledger: &CookLedger,
    incremental: bool,
    inputs: &mut CookInputs,
) -> Result<CookOutcome, CookError> {
    // The full cook stops before its loop on a bad rules file; the live and extension cooks
    // reach every source through here, and `resolve_for_cook` reads a bad file as "no rules".
    if let Some(err) = inputs.import_rules_error() {
        return Err(CookError::ImportRules(format!(
            "{}: {err}",
            regia_asset::import_rules::IMPORT_RULES_FILE
        )));
    }
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // A channel-pack recipe keys and dispatches as `texpack.ron`, so its source images are the
    // input key's side files (ADR-3090).
    let ext = if crate::texture_pack::is_texpack(source) {
        "texpack.ron".to_string()
    } else {
        ext
    };
    let project_root = inputs.project_root().to_path_buf();
    if matches!(ext.as_str(), "gltf" | "glb") {
        ensure_gltf_image_metas(source)?;
    }
    let platform = inputs.platform().to_string();
    let policy = inputs.texture_policy();
    // The Android rows key on the ASTC policy revision plus the project's texture policy
    // (`[cook.textures.android]`, ADR-2063; empty for the default, so older caches stay valid).
    let mut revision = if platform == "android" {
        format!("{ANDROID_ASTC_REVISION}{}", policy.key_text())
    } else {
        String::new()
    };
    // Every platform keys on a `draft` texture quality (spec 21 § 2, ADR-7217); `final` adds
    // nothing, so caches cooked before the key existed stay valid.
    let texture_quality = inputs.texture_quality();
    revision.push_str(&texture_quality.key_text());
    // The cook diet's edge cap of this source's textures (ADR-8461): keyed per source, empty
    // without one, so a cap change re-cooks only the sources it covers.
    let diet_cap = inputs.diet_texture_cap(rel_path);
    revision.push_str(&crate::cook_diet::cap_key_text(diet_cap));
    let src = SourceInputs {
        source,
        rel_path,
        project_root: &project_root,
        platform: &platform,
        ext: &ext,
        importer_version: importer_version_for(source, &ext),
        revision: &revision,
    };
    // The source's hash: from its stamp when the file is unchanged (no read), else read once.
    let (source_hash, read) = inputs.hash_file(source)?;
    let skippable = crate::cook_inputs::skippable(&ext);
    if incremental && skippable {
        let sidecars = inputs.recorded_sidecars(rel_path);
        if let Some(key) = inputs.input_key(&src, &source_hash, &sidecars)
            && let Some(outcome) = inputs.try_skip(
                rel_path,
                &key,
                &source_hash,
                src.importer_version,
                db.entries.get(rel_path),
                cache_dir,
            )
            // The shared database row may describe another platform's cook (ADR-875): this
            // platform's ledger must name the same key.
            && ledger.is_current(rel_path, &outcome.cache_key)
            // As on the imported path: a glTF's cooked materials must still decode.
            && (!matches!(ext.as_str(), "gltf" | "glb")
                || material_outputs_current(source, cache_dir, outcome.primary_id, &outcome.type_path))
        {
            inputs.counters.imports_skipped += 1;
            crate::cook_cache::publish_for(inputs, &src, &source_hash, &outcome, cache_dir, true);
            return Ok(outcome);
        }
    }
    // The shared cook cache (ADR-2612): outputs another machine cooked from the same inputs.
    // A stamp's hash may be stale (a file rewritten with its size and mtime kept), and a shared
    // entry must never be fetched or published under one: with a cache attached the source is
    // read and hashed again here, and the import below uses those bytes.
    let (source_hash, read) = if read.is_none() && skippable && inputs.shared_active() {
        let (hash, bytes) = inputs.hash_file_fresh(source)?;
        (hash, Some(bytes))
    } else {
        (source_hash, read)
    };
    if skippable && let Some(outcome) = crate::cook_cache::fetch_for(inputs, &src, &source_hash, cache_dir) {
        return Ok(outcome);
    }
    let bytes = match read {
        Some(bytes) => bytes,
        None => std::fs::read(source)?,
    };
    // The other files the import reads, hashed before it: an edit to one while it runs must not
    // be recorded as cooked.
    let sidecars = if skippable {
        crate::cook_inputs::sidecars_for(source, &ext)
    } else {
        Vec::new()
    };
    let before = inputs.side_inputs(&project_root, &ext, &sidecars);
    // A PNG's own `.meta` is read by its import; a glTF rewrites its own on every import.
    let own_meta = (skippable && !matches!(ext.as_str(), "gltf" | "glb")).then(|| meta_path_for(source));
    let meta_before = own_meta
        .as_ref()
        .and_then(|meta| inputs.hash_file(meta).ok().map(|(hash, _)| hash));
    // The import re-reads the rules file; the key hashes the snapshot taken when the cook opened.
    let rules_before = skippable && inputs.import_rules_unchanged();
    let outcome = match crate::import_rules::with_cook_project(&project_root, || {
        crate::texture_policy::with_texture_policy(policy, || {
            crate::project_texture_quality::with_default_texture_quality(
                texture_quality.cook_quality(),
                || {
                    crate::cook_diet::with_texture_cap(diet_cap, || {
                        cook_imported(&src, &bytes, source_hash, cache_dir, ledger, incremental)
                    })
                },
            )
        })
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            // A failed import may have written part of its outputs: never skip on them.
            inputs.forget(rel_path);
            return Err(e);
        }
    };
    if skippable {
        // Keyed on the state the cook left (it may have written the `.meta`).
        // Not recorded (imported next time): an input edited while the import ran, or a glTF
        // URI the sidecar list cannot follow.
        let meta_after = own_meta
            .as_ref()
            .and_then(|meta| inputs.hash_file(meta).ok().map(|(hash, _)| hash));
        let unchanged = inputs.side_inputs(&project_root, &ext, &sidecars) == before
            && (meta_before.is_none() || meta_after == meta_before)
            && rules_before
            && inputs.import_rules_unchanged();
        let key = if unchanged && gltf_uris_covered(source, &ext) {
            inputs.input_key(&src, &source_hash, &sidecars)
        } else {
            None
        };
        inputs.record(rel_path, key, &outcome, sidecars);
        crate::cook_cache::publish_for(inputs, &src, &source_hash, &outcome, cache_dir, false);
    }
    Ok(outcome)
}

/// Whether every file a glTF reads by URI is in its sidecar list ([`gltf_sidecars_of`]), so
/// the import skip keys on it: a `.gltf` whose relative URIs need no decoding beyond `%20` and
/// name existing files. A `.glb` with external URIs, a `file:` URI or another percent-escape
/// is not followed there, so such a source imports on every cook, as before ADR-530. `true` for
/// non-glTF sources.
pub(crate) fn gltf_uris_covered(source: &Path, ext: &str) -> bool {
    if !matches!(ext, "gltf" | "glb") {
        return true;
    }
    let Ok(bytes) = std::fs::read(source) else {
        return false;
    };
    if crate::gltf_import::reject_hostile_glb_header(&bytes).is_err() {
        return false;
    }
    let Ok((doc, raw)) = crate::gltf_import::checked_parse(&bytes) else {
        return false;
    };
    let Some(dir) = source.parent() else {
        return false;
    };
    // A fallback buffer's file is never read (`EXT_meshopt_compression`), so it need not exist.
    let buffer_uris = doc
        .buffers()
        .filter(|b| !crate::gltf_normalize::is_fallback_buffer(&raw, b.index()))
        .filter_map(|b| match b.source() {
            gltf::buffer::Source::Uri(uri) => Some(uri),
            gltf::buffer::Source::Bin => None,
        });
    let image_uris = doc.images().filter_map(|i| match i.source() {
        gltf::image::Source::Uri { uri, .. } => Some(uri),
        gltf::image::Source::View { .. } => None,
    });
    let mut uris = buffer_uris
        .chain(image_uris)
        .filter(|uri| !uri.starts_with("data:"));
    uris.all(|uri| {
        let decoded = uri.replace("%20", " ");
        ext == "gltf" && !decoded.contains(['%', ':']) && dir.join(decoded).is_file()
    })
}

/// [`cook_one`] after the skip check: runs the importer on `bytes`, then skips the write when
/// the importer's cache key is unchanged.
fn cook_imported(
    src: &SourceInputs<'_>,
    bytes: &[u8],
    source_hash: [u8; 32],
    cache_dir: &Path,
    ledger: &CookLedger,
    incremental: bool,
) -> Result<CookOutcome, CookError> {
    let SourceInputs {
        source,
        rel_path,
        project_root,
        platform,
        ext,
        importer_version,
        revision,
        ..
    } = *src;
    // ADR-7972: an importer compiled out of this build is the source's error, never a skip.
    if let Some(disabled) = crate::disabled_importer(ext) {
        return Err(CookError::Import(disabled));
    }
    let mut dependencies = vec![];
    let (importer, settings_hash, primary_id, type_path, payload, sub_manifest) = match ext {
        "ron" | "csv" if crate::data_cook::is_data_table(source) => {
            let cooked = crate::data_cook::cook_table(source, bytes, project_root)?;
            dependencies = cooked.dependencies;
            (
                cooked.importer,
                cooked.settings_hash,
                cooked.id,
                cooked.type_path,
                cooked.payload,
                vec![],
            )
        }
        "glb" | "gltf" => cook_gltf(
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
        )?,
        #[cfg(feature = "fbx_import")]
        "fbx" => {
            crate::subasset_remap::with_stable_sub_ids(source, rel_path, project_root, cache_dir, || {
                cook_fbx(
                    source,
                    bytes,
                    rel_path,
                    project_root,
                    cache_dir,
                    importer_version,
                    platform,
                )
            })?
        }
        "bvh" => cook_bvh(source, bytes, rel_path, project_root, cache_dir, importer_version)?,
        "obj" => cook_obj(
            source,
            bytes,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
        )?,
        "ply" => cook_ply(
            source,
            bytes,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
        )?,
        "stl" => cook_stl(
            source,
            bytes,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
        )?,
        "psd" => cook_psd(source, bytes, platform)?,
        #[cfg(feature = "usd_import")]
        "usd" | "usda" | "usdc" | "usdz" => cook_usd(
            source,
            bytes,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
        )?,
        "png" | "jpg" | "jpeg" | "tga" | "bmp" => cook_png(
            source,
            bytes,
            ext,
            rel_path,
            project_root,
            importer_version,
            platform,
        )?,
        #[cfg(feature = "svg_import")]
        "svg" => {
            let cooked = crate::svg_import::cook_svg(source, bytes, platform)?;
            (
                crate::svg_import::SVG_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                cooked.type_path,
                cooked.payload,
                vec![],
            )
        }
        "dds" => {
            let cooked = crate::dds_import::cook_dds(source, bytes, rel_path, project_root, platform)?;
            (
                crate::dds_import::DDS_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                cooked.type_path,
                cooked.payload,
                vec![],
            )
        }
        "exr" | "hdr" => {
            let cooked = crate::hdr_import::cook_hdr(source, bytes, ext, rel_path, project_root, platform)?;
            (
                crate::hdr_import::HDR_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                crate::hdr_import::HDR_TYPE_PATH.to_string(),
                cooked.payload,
                vec![],
            )
        }
        ext if crate::audio_cook::is_audio_extension(ext) => {
            let cooked = crate::audio_cook::cook_audio(
                source,
                bytes,
                ext,
                project_root,
                rel_path,
                cache_dir,
                platform,
            )?;
            (
                crate::audio_cook::AUDIO_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                crate::audio_cook::COOKED_SOUND_TYPE.to_string(),
                cooked.payload,
                vec![],
            )
        }
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".material.ron")) =>
        {
            // The cooked header lists the asset ids the material's texture slots name
            // (texture-set materials, ADR-3320), so a pak's chunking pulls the maps in.
            dependencies = MaterialAssetRon::from_ron(bytes)
                .map(|mat| material_texture_deps(&mat))
                .unwrap_or_default();
            cook_material_ron(source, bytes, importer_version)?
        }
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".scatter.ron")) =>
        {
            cook_scatter_ron(source, bytes)?
        }
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".atlas.ron")) =>
        {
            cook_atlas_ron(source, bytes)?
        }
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".tilemap.ron")) =>
        {
            cook_tilemap_source(source, bytes, "tilemap_ron")?
        }
        "tmx" | "tmj" => cook_tilemap_source(source, bytes, "tiled")?,
        "aseprite" | "ase" => cook_aseprite(source, bytes, rel_path, cache_dir, importer_version)?,
        "ldtk" => cook_ldtk(source, bytes, rel_path, cache_dir, importer_version)?,
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".autotile.ron")) =>
        {
            cook_autotile_ron(source, bytes)?
        }
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".cloth.ron")) =>
        {
            cook_cloth_ron(source, false)?
        }
        "ron"
            if source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".soft.ron")) =>
        {
            cook_cloth_ron(source, true)?
        }
        "ron" if crate::motiondb_cook::is_motiondb(source) => {
            let (cooked, id) = cook_motiondb_ron(source, bytes, project_root, cache_dir)?;
            dependencies = cooked.dependencies;
            (
                crate::motiondb_cook::MOTIONDB_IMPORTER.to_string(),
                // The clips and the skeleton are inputs too: key on the payload.
                *blake3::hash(&cooked.payload).as_bytes(),
                id,
                regia_animation::motion_matching::MOTION_DATABASE_TYPE_PATH.to_string(),
                cooked.payload,
                vec![],
            )
        }
        // Channel-pack recipes (`.texpack.ron`, ADR-3090): `cook_one` gives them their own key ext.
        "texpack.ron" => {
            let cooked = crate::texture_pack::cook_texpack(source, bytes, platform)?;
            (
                crate::texture_pack::TEXPACK_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                cooked.type_path,
                cooked.payload,
                vec![],
            )
        }
        "fgiv" => cook_baked_gi(source, bytes)?,
        // A `.cube` colour LUT a level look or a post volume names (ADR-4830): parsed with the
        // renderer's rules, then stored unchanged, so the file ships in the pak and a packaged
        // client loads it through the asset server instead of a filesystem path.
        "cube" => cook_color_lut(source, bytes)?,
        // A generator's baked output (spec 62 § 8, ADR-7214): checked, then stored unchanged, so it
        // ships in the pak and a packaged game (Android: no loose files) reads it there.
        "pgout" => cook_pgout(source, bytes)?,
        // A particle-liquid bake (ADR-2441), validated and stored canonical (ADR-2577).
        "fluidcache" => crate::fluid_cook::cook_fluidcache_source(source, bytes)?,
        // IES LM-63 photometric profiles (ADR-2592).
        "ies" => {
            let (importer, hash, id, type_path, payload) = crate::ies_cook::cook_ies_source(source, bytes)?;
            (importer, hash, id, type_path, payload, vec![])
        }
        "flmap" => cook_lightmap(source, bytes, platform)?,
        "ron"
            if source.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                crate::cook::name_ends_with(n, ".matgraph.ron") || name_ends_with(n, ".matinst.ron")
            }) =>
        {
            let instance = source
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, ".matinst.ron"));
            let cooked =
                crate::matgraph_cook::cook_matgraph(source, bytes, project_root, cache_dir, instance)?;
            (
                cooked.importer,
                cooked.settings_hash,
                cooked.id,
                crate::matgraph_cook::type_path(),
                cooked.payload,
                cooked.sub_manifest,
            )
        }
        // MaterialX documents import onto a material graph and cook like one (ADR-3341).
        "mtlx" => {
            let cooked =
                crate::materialx_import::cook_mtlx(source, rel_path, bytes, project_root, cache_dir)?;
            (
                cooked.importer,
                cooked.settings_hash,
                cooked.id,
                crate::matgraph_cook::type_path(),
                cooked.payload,
                cooked.sub_manifest,
            )
        }
        // Extension content of a type only the extension's own loader reads (specs/57 § 3.3.2).
        _ if rel_path.starts_with(crate::extension_cook::EXTENSION_PREFIX) => {
            cook_extension_raw(source, bytes)?
        }
        // Fonts (ADR-2697): the subset for the shipped locales, into the cache only; `regia package` staged it.
        ext if crate::font_subset::is_font_extension(ext) => {
            let cooked = crate::font_subset::cook_font(source, rel_path, bytes, project_root)?;
            (
                crate::font_subset::FONT_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                crate::font_subset::COOKED_FONT_TYPE.to_string(),
                cooked.payload,
                vec![],
            )
        }
        // Video (specs/50 § 4, ADR-2410): copied and validated, never transcoded.
        ext if crate::webm_cook::is_video_extension(ext) => {
            let cooked = crate::webm_cook::cook_video(source, bytes)?;
            (
                crate::webm_cook::WEBM_IMPORTER.to_string(),
                cooked.settings_hash,
                cooked.id,
                crate::webm_cook::COOKED_VIDEO_TYPE.to_string(),
                cooked.payload,
                vec![],
            )
        }
        ext if crate::webm_cook::is_refused_video_extension(ext) => {
            return Err(crate::webm_cook::refuse_container(source, ext));
        }
        // Ambience files (ADR-2721): validated, cooked as their text.
        "ron" if crate::audio_cook::is_ambience(source) => {
            let (id, settings_hash, payload) = crate::audio_cook::cook_ambience(source, bytes)?;
            (
                crate::audio_cook::AMBIENCE_IMPORTER.to_string(),
                settings_hash,
                id,
                crate::audio_cook::AMBIENCE_TYPE.to_string(),
                payload,
                vec![],
            )
        }
        "ron" if crate::webm_cook::is_video_ron(source) => {
            let (id, settings_hash, type_path, payload) = crate::webm_cook::cook_video_ron(source, bytes)?;
            (
                crate::webm_cook::VIDEO_RON_IMPORTER.to_string(),
                settings_hash,
                id,
                type_path.to_string(),
                payload,
                vec![],
            )
        }
        other => {
            return Err(CookError::UnsupportedExtension(other.to_string()));
        }
    };

    let mut key_hasher = blake3::Hasher::new();
    key_hasher.update(&source_hash);
    key_hasher.update(&settings_hash);
    key_hasher.update(importer.as_bytes());
    key_hasher.update(&importer_version.to_le_bytes());
    if !revision.is_empty() {
        // Android rows also key on the ASTC policy and the project's texture policy, and every
        // platform on a `draft` texture quality (the revision text, ADR-7217), so a policy change
        // rewrites only them. An empty revision (desktop, `final`) keeps the historical key.
        key_hasher.update(revision.as_bytes());
    }
    let cache_key = *key_hasher.finalize().as_bytes();

    let cooked_rel = cooked_file_name(primary_id);
    // This platform's ledger, not the shared database row: another platform may have cooked
    // these bytes since this cache was written (ADR-875).
    if incremental && ledger.is_current(rel_path, &cache_key) && cache_dir.join(&cooked_rel).is_file() {
        let embeds_material = matches!(
            importer.as_str(),
            "gltf" | "fbx" | "obj" | "ply" | "stl" | "usd" | "material_ron"
        );
        if !embeds_material || material_outputs_current(source, cache_dir, primary_id, &type_path) {
            return Ok(CookOutcome {
                primary_id,
                type_path,
                source_hash,
                settings_hash,
                importer,
                importer_version,
                cache_key,
                cooked_rel,
                skipped: true,
                sub_manifest,
            });
        }
        info!(
            target: "regia_asset_pipeline",
            path = %rel_path,
            "cooked material payload is stale; recooking"
        );
    }

    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: type_tag_for_path(&type_path),
        asset_id: primary_id,
        importer_version,
        dependencies,
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, &payload).map_err(CookError::Cooked)?;
    write_atomic(&cache_dir.join(&cooked_rel), &file)?;

    // Write/update meta on disk.
    let meta_path = meta_path_for(source);
    if !meta_path.is_file() {
        let meta = MetaSidecar::new(&importer, primary_id);
        let ron = meta.to_ron_string().map_err(CookError::Meta)?;
        if let Some(parent) = meta_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // No-replace: never overwrite a `.meta` (and its id) another writer published meanwhile.
        crate::cook_ledger::write_new_atomic(&meta_path, ron.as_bytes())?;
    }

    Ok(CookOutcome {
        primary_id,
        type_path,
        source_hash,
        settings_hash,
        importer,
        importer_version,
        cache_key,
        cooked_rel,
        skipped: false,
        sub_manifest,
    })
}

/// Writes the cook's sub-asset list into the source's `.meta`, merged into the sidecar as it is on
/// disk now rather than the copy read when the cook started: an importer-settings (or provenance)
/// edit saved during a long cook survives, and its new settings hash recooks the source next time
/// (audit concurrency-1103 pipeline M2). A sidecar that no longer parses or names another id is
/// replaced by the cook's copy, as before.
fn write_meta_sub_assets(
    source: &Path,
    used: &MetaSidecar,
    sub_assets: Vec<regia_asset::SubAssetEntry>,
) -> Result<(), CookError> {
    let meta_path = meta_path_for(source);
    let on_disk = std::fs::read(&meta_path)
        .ok()
        .and_then(|bytes| MetaSidecar::from_ron(&bytes).ok())
        .filter(|current| current.id == used.id && current.format_version == used.format_version);
    if on_disk
        .as_ref()
        .is_some_and(|current| current.sub_assets == sub_assets)
    {
        return Ok(());
    }
    let mut out = on_disk.unwrap_or_else(|| used.clone());
    out.sub_assets = sub_assets;
    let ron = out.to_ron_string().map_err(CookError::Meta)?;
    write_atomic(&meta_path, ron.as_bytes())?;
    Ok(())
}

/// True when every cooked standard material this source produced still loads.
///
/// A version-1 blob fails [`load_material_bytes`]. The caller then rewrites the
/// cook instead of leaving the game with a file it cannot decode.
fn material_outputs_current(source: &Path, cache_dir: &Path, primary_id: AssetId, type_path: &str) -> bool {
    // A database row or `.meta` written before the rename names the legacy crate (ADR-1429).
    let type_path = regia_core::brand::current_type_path(type_path);
    if type_path == "regia_asset::CookedStandardMaterial"
        && !cooked_material_is_current(cache_dir, primary_id)
    {
        return false;
    }
    let meta_path = meta_path_for(source);
    if !meta_path.is_file() {
        return true;
    }
    let Ok(bytes) = std::fs::read(&meta_path) else {
        return false;
    };
    let Ok(meta) = MetaSidecar::from_ron(&bytes) else {
        return false;
    };
    meta.sub_assets.iter().all(|sub| {
        // A `.meta` written before the rename names the legacy crate (ADR-1429).
        regia_core::brand::current_type_path(&sub.type_path) != "regia_asset::CookedStandardMaterial"
            || cooked_material_is_current(cache_dir, sub.id)
    })
}

fn cooked_material_is_current(cache_dir: &Path, id: AssetId) -> bool {
    let path = cache_dir.join(cooked_file_name(id));
    match std::fs::read(&path) {
        Ok(bytes) => load_material_bytes(&bytes).is_ok(),
        Err(_) => false,
    }
}

fn type_tag_for_path(type_path: &str) -> regia_core::id::TypePathHash {
    regia_core::id::TypePathHash::of(type_path)
}

/// Cooks a glTF with stable sub-asset ids: the `.meta`'s stored ids, and renamed sub-assets
/// re-mapped to their old ids (ADR-2428, [`crate::subasset_remap`]).
fn cook_gltf(
    source: &Path,
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    crate::subasset_remap::with_stable_sub_ids(source, rel_path, project_root, cache_dir, || {
        cook_gltf_once(
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
        )
    })
}

/// `meta.settings` with the project's import rules merged in: `config/import_rules.ron`'s
/// matching rules first (later rules win), the explicit `.meta` fields on top (ADR-3321).
/// `None` when nothing sets any field, exactly the `settings` the meta would carry without rules.
fn resolved_meta_settings<'a>(
    meta: &'a MetaSidecar,
    project_root: &Path,
    rel_path: &str,
    importer: &str,
) -> Option<std::borrow::Cow<'a, str>> {
    let resolved =
        crate::import_rules::resolve_for_cook(project_root, rel_path, importer, meta.settings.as_deref());
    (!resolved.is_empty()).then_some(resolved)
}

fn cook_gltf_once(
    source: &Path,
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "gltf").map_err(CookError::Db)?;
    if meta.format_version != META_FORMAT_VERSION {
        return Err(CookError::Meta(regia_asset::MetaError::UnsupportedVersion {
            found: meta.format_version,
            supported: META_FORMAT_VERSION,
        }));
    }
    let source_bytes = std::fs::read(source)?;
    let resolved = resolved_meta_settings(&meta, project_root, rel_path, "gltf");
    // Authored LOD screen sizes: the `_LODn` nodes' extras, then the project's class table
    // (`lod_classes.toml`, ADR-260). `cook_project` reports the table's own problems once.
    let (lod_table, _) = crate::lod_classes::LodClassTable::load(project_root);
    let (lod, lod_warnings) =
        crate::lod_classes::LodThresholdInputs::resolve(&source_bytes, rel_path, lod_table.as_ref());
    for warning in lod_warnings {
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
    }
    // A file the class table reaches keys its cache on the class's sizes (its node extras are in
    // the source hash); every other file keeps its key, so adding the table recooks only them.
    // Likewise the files a `.gltf` references and its `.meta` settings (ADR-875): a `.glb`
    // without settings keeps the key it always had.
    let inputs = gltf_inputs_digest(source, resolved.as_deref());
    let parsed = crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref());
    // The historical key is `forward=convert` with scale 1. A `.meta` that names `Keep` or a
    // scale other than 1 changes the baked bytes, so those files take an extra term and recook.
    // Files that only set other keys keep the key they had (the inputs digest already covers the
    // settings string). Generated LODs also recook: simplification now weighs normals and UV0.
    let forward = crate::gltf_settings::explicit_forward(resolved.as_deref()).unwrap_or(Forward::Convert);
    let scale = parsed.import_scale();
    let axis_term = (forward == Forward::Keep || (scale - 1.0).abs() > f32::EPSILON)
        .then_some((u8::from(forward == Forward::Keep), scale));
    let settings_hash = with_lossless_clips_term(
        gltf_settings_hash(
            lod.class.as_deref(),
            lod.class_sizes.as_deref(),
            inputs,
            axis_term,
            parsed.generate_lods.is_some(),
        ),
        parsed.lossless_clips,
    );
    let imported = import_gltf_path_with_lod(
        source,
        meta.id,
        forward,
        platform,
        resolved.as_deref(),
        crate::project_texture_quality::default_texture_quality(),
        &lod,
    )
    .map_err(CookError::Import)?;
    let payload = imported.primary_mesh.to_postcard().map_err(CookError::Postcard)?;
    // Skeletons and clips are imported and checked before the first cache write, and the mesh
    // sub-assets are written after the opt-in cooks: a recook refused on a clip leaves the files
    // the previous cook's database row names as they were (audit asset_pipeline1 #4).
    // The `.meta` `clips` options (skip, rename, split by range; ADR-7904) reach the clip import:
    // the settings string is already in the cook key, so changing them recooks.
    let anim_settings = GltfAnimSettings {
        forward,
        scale,
        clips: parsed.clip_options(),
        ..GltfAnimSettings::default()
    };
    let anim = prepare_gltf_animation(
        rel_path,
        importer_version,
        meta.id,
        &source_bytes,
        source.parent(),
        &anim_settings,
        &ClipCookOptions::from_settings(&parsed),
    )?;
    let mut sub_manifest = gltf_sub_asset_manifest(rel_path, &imported);
    let mut meta_entries = imported.meta_entries.clone();

    // -- Meshlet sub-assets (visibility buffer), opt-in per asset ---
    if crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref()).meshlets {
        let meshlets = crate::meshlet_cook::cook_meshlet_sub_assets(
            cache_dir,
            rel_path,
            importer_version,
            meta.id,
            imported.sub_assets.iter().map(|sub| {
                (
                    sub.path.as_str(),
                    sub.id,
                    sub.type_path.as_str(),
                    sub.payload.as_slice(),
                )
            }),
        )?;
        if meshlets.built > 0 {
            info!(target: "regia_asset_pipeline", path = %rel_path, built = meshlets.built, "cooked meshlets");
        }
        sub_manifest.extend(meshlets.manifest);
        meta_entries.extend(meshlets.meta);
    }

    // -- Voxel far proxies (past the last cluster LOD, ADR-2445), opt-in per asset ---
    let gltf_settings = crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref());
    if let Some(voxel) = gltf_settings.voxel_far {
        // The facts the cooked material does not carry (ADR-3227): vertex_ao and the base color
        // transform, so a masked material's coverage test matches what the source means.
        let source = crate::voxel_far_cook::VoxelFarSource::from_gltf(
            gltf_settings.vertex_ao,
            &imported.material_ids,
            &imported.extensions,
        );
        let voxels = crate::voxel_far_cook::cook_voxel_far_sub_assets_from(
            cache_dir,
            rel_path,
            importer_version,
            meta.id,
            &voxel,
            &source,
            imported.sub_assets.iter().map(|sub| {
                (
                    sub.path.as_str(),
                    sub.id,
                    sub.type_path.as_str(),
                    sub.payload.as_slice(),
                )
            }),
        )?;
        if voxels.built > 0 {
            info!(target: "regia_asset_pipeline", path = %rel_path, built = voxels.built, "cooked voxel far proxies");
        }
        sub_manifest.extend(voxels.manifest);
        meta_entries.extend(voxels.meta);
    }

    // -- Pre-fractured chunks (destruction), opt-in per asset ---
    cook_fracture_settings(
        &FractureCookCtx {
            cache_dir,
            rel_path,
            importer_version,
            meta_id: meta.id,
            primary: &payload,
            imported: &imported,
        },
        resolved.as_deref(),
        (&mut sub_manifest, &mut meta_entries),
    )?;

    // -- Collision hulls (`collision: ConvexDecomposition`), opt-in per asset ---
    {
        let settings = crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref());
        if settings.collision == crate::gltf_settings::CollisionImport::ConvexDecomposition {
            let hulls = crate::collision_cook::cook_collision_sub_assets(
                cache_dir,
                rel_path,
                importer_version,
                meta.id,
                &settings.collision_hulls,
                imported.sub_assets.iter().map(|sub| {
                    (
                        sub.path.as_str(),
                        sub.id,
                        sub.type_path.as_str(),
                        sub.payload.as_slice(),
                    )
                }),
            )?;
            sub_manifest.extend(hulls.manifest);
            meta_entries.extend(hulls.meta);
        }
    }

    // -- Mesh and animation sub-assets, prepared above ---
    write_gltf_sub_assets(cache_dir, importer_version, &imported)?;
    let anim_sub = anim.write(cache_dir)?;
    sub_manifest.extend(anim_sub.manifest);
    meta_entries.extend(anim_sub.meta);

    write_meta_sub_assets(source, &meta, meta_entries)?;
    let primary = meta.id;
    // A file with `collision_*` proxies keys on the collision-set revision too (ADR-2341), so a
    // cache cooked before proxies were cooked is rewritten; every other file keeps its key.
    let settings_hash = if imported.collision.is_some() {
        let mut h = blake3::Hasher::new();
        h.update(&settings_hash);
        h.update(b"collision-set");
        h.update(&regia_asset::COLLISION_SET_FORMAT_VERSION.to_le_bytes());
        *h.finalize().as_bytes()
    } else {
        settings_hash
    };
    // The same for `EXT_mesh_gpu_instancing` (ADR-3016).
    let settings_hash = if imported.instances.is_some() {
        let mut h = blake3::Hasher::new();
        h.update(&settings_hash);
        h.update(b"prefab-instances");
        h.update(&regia_asset::PREFAB_INSTANCES_FORMAT_VERSION.to_le_bytes());
        *h.finalize().as_bytes()
    } else {
        settings_hash
    };
    Ok((
        "gltf".to_string(),
        settings_hash,
        primary,
        "regia_asset::CookedMesh".to_string(),
        payload,
        sub_manifest,
    ))
}

/// The running glTF cook's inputs the `.meta` `fracture` block's sub-assets need.
struct FractureCookCtx<'a> {
    cache_dir: &'a Path,
    rel_path: &'a str,
    importer_version: u32,
    meta_id: regia_asset::AssetId,
    primary: &'a [u8],
    imported: &'a crate::gltf_import::GltfImportResult,
}

/// The `.meta` `fracture` block's sub-assets (ADR-1967), when the file has one; the rows are added
/// to `rows` (manifest, `.meta`).
fn cook_fracture_settings(
    ctx: &FractureCookCtx<'_>,
    settings: Option<&str>,
    rows: (&mut Vec<ManifestEntry>, &mut Vec<SubAssetEntry>),
) -> Result<(), CookError> {
    let settings = crate::gltf_settings::GltfImporterSettings::from_meta_string(settings);
    let Some(fracture) = settings.fracture else {
        return Ok(());
    };
    let cooked = crate::fracture_cook::cook_fracture_sub_assets(
        ctx.cache_dir,
        ctx.rel_path,
        ctx.importer_version,
        ctx.meta_id,
        &fracture,
        ctx.primary,
        ctx.imported
            .sub_assets
            .iter()
            .map(|sub| (sub.path.as_str(), sub.type_path.as_str(), sub.payload.as_slice())),
    )?;
    if cooked.built {
        info!(target: "regia_asset_pipeline", path = %ctx.rel_path, chunks = cooked.chunks, "cooked fracture chunks");
    }
    rows.0.extend(cooked.manifest);
    rows.1.extend(cooked.meta);
    Ok(())
}

/// Cooks an `.fbx` (ADR-1602): converts it to an in-memory glTF binary and cooks that exactly as
/// [`cook_gltf`] cooks a `.glb` ([`cook_converted_glb`]), so the cooked formats are the glTF
/// importer's. The `.meta` keeps importer `fbx`.
#[cfg(feature = "fbx_import")]
fn cook_fbx(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    let meta = load_converted_meta(source, "fbx")?;
    let resolved = resolved_meta_settings(&meta, project_root, rel_path, "fbx");
    let settings = crate::fbx_import::FbxImportSettings::try_from_meta_string(resolved.as_deref())
        .map_err(|e| CookError::Import(ImportError::Fbx(e)))?;
    if bytes.len() as u64 > settings.limits.max_file_bytes {
        return Err(CookError::Import(ImportError::Fbx(
            crate::fbx_import::FbxError::TooLarge {
                size: bytes.len() as u64,
                limit: settings.limits.max_file_bytes,
            },
        )));
    }
    let conversion = crate::fbx_import::convert_fbx_to_glb(bytes, source.parent(), &settings)
        .map_err(|e| CookError::Import(ImportError::Fbx(e)))?;
    let inputs = crate::fbx_import::fbx_inputs_hash(
        &settings,
        resolved.as_deref(),
        source.parent(),
        &conversion.report,
    );
    info!(
        target: "regia_asset_pipeline",
        path = %rel_path,
        meshes = conversion.report.meshes,
        triangles = conversion.report.triangles,
        clips = conversion.report.clips.len(),
        "converted FBX"
    );
    cook_converted_glb(
        ConvertedSource {
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
            importer: "fbx",
            inputs,
        },
        &meta,
        &conversion.glb,
    )
}

/// Cooks an `.obj` (pure Rust, `obj_import`) through the same converted-glTF path as FBX.
fn cook_obj(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    let meta = load_converted_meta(source, "obj")?;
    let resolved = resolved_meta_settings(&meta, project_root, rel_path, "obj");
    let settings = crate::obj_import::ObjImportSettings::try_from_meta_string(resolved.as_deref())
        .map_err(|e| CookError::Import(ImportError::Obj(e)))?;
    let conversion = crate::obj_import::convert_obj_to_glb(bytes, source.parent(), &settings)
        .map_err(|e| CookError::Import(ImportError::Obj(e)))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"obj;meta=");
    hasher.update(resolved.as_deref().unwrap_or_default().as_bytes());
    let dir = source.parent().unwrap_or_else(|| Path::new(""));
    // The `.mtl` libraries and textures the conversion read.
    let mut files: Vec<String> = conversion.report.textures.clone();
    for line in String::from_utf8_lossy(bytes).lines() {
        if let Some(rest) = line.trim().strip_prefix("mtllib") {
            files.extend(rest.split_whitespace().map(str::to_string));
        }
    }
    for file in files {
        hasher.update(b";input=");
        hasher.update(file.as_bytes());
        match std::fs::read(dir.join(&file)) {
            Ok(bytes) => hasher.update(blake3::hash(&bytes).as_bytes()),
            Err(_) => hasher.update(b"<missing>"),
        };
    }
    cook_converted_glb(
        ConvertedSource {
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
            importer: "obj",
            inputs: *hasher.finalize().as_bytes(),
        },
        &meta,
        &conversion.glb,
    )
}

/// Cooks a `.ply` (pure Rust, `ply_import`) through the same converted-glTF path as OBJ.
fn cook_ply(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    let meta = load_converted_meta(source, "ply")?;
    let settings = crate::ply_import::PlyImportSettings::try_from_meta_string(meta.settings.as_deref())
        .map_err(|e| CookError::Import(ImportError::Ply(e)))?;
    let conversion = crate::ply_import::convert_ply_to_glb(bytes, &settings)
        .map_err(|e| CookError::Import(ImportError::Ply(e)))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ply;meta=");
    hasher.update(meta.settings.as_deref().unwrap_or_default().as_bytes());
    cook_converted_glb(
        ConvertedSource {
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
            importer: "ply",
            inputs: *hasher.finalize().as_bytes(),
        },
        &meta,
        &conversion.glb,
    )
}

/// Cooks an `.stl` (pure Rust, `stl_import`) through the same converted-glTF path as OBJ.
fn cook_stl(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    let meta = load_converted_meta(source, "stl")?;
    let settings = crate::stl_import::StlImportSettings::try_from_meta_string(meta.settings.as_deref())
        .map_err(|e| CookError::Import(ImportError::Stl(e)))?;
    let conversion = crate::stl_import::convert_stl_to_glb(bytes, &settings)
        .map_err(|e| CookError::Import(ImportError::Stl(e)))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"stl;meta=");
    hasher.update(meta.settings.as_deref().unwrap_or_default().as_bytes());
    cook_converted_glb(
        ConvertedSource {
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
            importer: "stl",
            inputs: *hasher.finalize().as_bytes(),
        },
        &meta,
        &conversion.glb,
    )
}

/// Cooks a `.psd` (pure Rust, `psd_import`) as a `.png` is cooked: decode to RGBA8, then the
/// texture settings (usage, `max_size`, compression, mips) apply. The `layer` and `alpha`
/// settings of the `.meta` pick what is decoded.
fn cook_psd(source: &Path, bytes: &[u8], platform: &str) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "psd").map_err(CookError::Db)?;
    let psd_err = |e: crate::psd_import::PsdError| CookError::Import(ImportError::Psd(e));
    let psd = crate::psd_import::PsdImportSettings::try_from_meta_string(meta.settings.as_deref())
        .map_err(psd_err)?;
    let settings = TextureImporterSettings::from_meta_string(meta.settings.as_deref());
    let usage = settings.usage_or(usage_from_file_name(source));
    let settings_hash = *blake3::hash(
        format!(
            "{};psd;{};auto={usage:?}",
            settings.fingerprint(),
            psd.fingerprint()
        )
        .as_bytes(),
    )
    .as_bytes();
    let image = crate::psd_import::decode_psd(bytes, &psd).map_err(psd_err)?;
    let img = regia_asset::CookedImage {
        width: image.width,
        height: image.height,
        rgba8: image.rgba8,
    };
    img.validate()
        .map_err(|e| CookError::Import(ImportError::Image(e.to_string())))?;
    let (type_path, payload) = cook_rgba8_payload(img, &settings, usage, platform)?;
    Ok((
        crate::psd_import::PSD_IMPORTER.to_string(),
        settings_hash,
        meta.id,
        type_path,
        payload,
        vec![],
    ))
}

/// Cooks a `.usda`, `.usdc`, `.usd` or `.usdz` (ADR-3235, `usd_import`) through the same converted-glTF path as
/// FBX and OBJ. The input digest covers every layer and texture file the conversion read.
#[cfg(feature = "usd_import")]
fn cook_usd(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
    platform: &str,
) -> ImportedCook {
    use crate::usd_import::{USD_IMPORTER, UsdImportSettings, UsdSource, convert_usd_to_glb};
    let usd_err = |e: crate::usd_import::UsdError| CookError::Import(ImportError::Usd(e));
    let meta = load_converted_meta(source, USD_IMPORTER)?;
    let settings = UsdImportSettings::try_from_meta_string(meta.settings.as_deref()).map_err(usd_err)?;
    if bytes.len() as u64 > settings.limits.max_file_bytes {
        return Err(usd_err(crate::usd_import::UsdError::TooLarge {
            size: bytes.len() as u64,
            limit: settings.limits.max_file_bytes,
        }));
    }
    let conversion = convert_usd_to_glb(UsdSource::File(source), &settings).map_err(usd_err)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"usd;meta=");
    hasher.update(meta.settings.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b";inputs=");
    hasher.update(&conversion.inputs);
    info!(
        target: "regia_asset_pipeline",
        path = %rel_path,
        layers = conversion.report.layers.len(),
        meshes = conversion.report.meshes,
        triangles = conversion.report.triangles,
        "converted USD"
    );
    cook_converted_glb(
        ConvertedSource {
            source,
            rel_path,
            project_root,
            cache_dir,
            importer_version,
            platform,
            importer: USD_IMPORTER,
            inputs: *hasher.finalize().as_bytes(),
        },
        &meta,
        &conversion.glb,
    )
}

/// Cooks a `.bvh` motion-capture file (ADR-2532): its skeleton is the primary output, its clip is the
/// `clip/<name>` sub-asset, and with a `retarget` setting the clip retargeted to the named project
/// skeleton is `retarget/<name>` (that skeleton's copy: `retarget/skeleton/<joint>`).
fn cook_bvh(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    project_root: &Path,
    cache_dir: &Path,
    importer_version: u32,
) -> ImportedCook {
    let meta = load_converted_meta(source, "bvh")?;
    let bvh_err = |e: crate::bvh_import::BvhError| CookError::Import(ImportError::Bvh(e));
    let resolved = resolved_meta_settings(&meta, project_root, rel_path, "bvh");
    let settings =
        crate::bvh_import::BvhImportSettings::try_from_meta_string(resolved.as_deref()).map_err(bvh_err)?;
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let import = crate::bvh_import::convert_bvh(bytes, stem, &settings).map_err(bvh_err)?;
    crate::motiondb_cook::scan_clip_finite(&import.clip).map_err(|e| CookError::Animation(e.to_string()))?;
    info!(
        target: "regia_asset_pipeline",
        path = %rel_path,
        joints = import.skeleton.len(),
        keys_before = import.keys_before,
        keys_after = import.keys_after,
        "converted BVH"
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bvh;settings=");
    hasher.update(settings.fingerprint().as_bytes());
    // The retarget target is read at cook time, so its bytes are an input.
    let mut retargeted = None;
    if let Some(retarget) = &settings.retarget {
        // The target is a project file (ADR-5869): an absolute path or a `..` out of the project
        // read any host glTF into the cook.
        let target_path =
            join_bounded(project_root, &retarget.target, Some(project_root)).map_err(|why| {
                bvh_err(crate::bvh_import::BvhError::Retarget(format!(
                    "target {:?}: {why}",
                    retarget.target
                )))
            })?;
        let target_bytes = std::fs::read(&target_path).map_err(|e| {
            bvh_err(crate::bvh_import::BvhError::Retarget(format!(
                "cannot read target {:?}: {e}",
                retarget.target
            )))
        })?;
        hasher.update(b";target=");
        hasher.update(blake3::hash(&target_bytes).as_bytes());
        let anim = import_gltf_animation_with_base(
            &target_bytes,
            target_path.parent(),
            &GltfAnimSettings::default(),
        )
        .map_err(|e| {
            bvh_err(crate::bvh_import::BvhError::Retarget(format!(
                "target {:?}: {e}",
                retarget.target
            )))
        })?;
        let skin = anim.skins.into_iter().nth(retarget.skin).ok_or_else(|| {
            bvh_err(crate::bvh_import::BvhError::Retarget(format!(
                "target {:?} has no skin {}",
                retarget.target, retarget.skin
            )))
        })?;
        let clip = crate::bvh_import::retarget_import(&import, &skin.skeleton, &retarget.bone_map)
            .map_err(bvh_err)?;
        retargeted = Some((skin.skeleton, clip));
    }
    let settings_hash = *hasher.finalize().as_bytes();

    let parent_id = meta.id;
    let mut manifest = Vec::new();
    let mut entries: Vec<SubAssetEntry> = Vec::new();
    let mut write_sub =
        |path: String, type_path: &str, payload: Vec<u8>, deps: Vec<AssetId>| -> Result<AssetId, CookError> {
            let id = crate::subasset_remap::sub_id(parent_id, &path);
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: type_tag_for_path(type_path),
                asset_id: id,
                importer_version,
                dependencies: deps,
                payload_len: payload.len() as u64,
            };
            let file = encode_cooked_file(&header, &payload).map_err(CookError::Cooked)?;
            write_atomic(&cache_dir.join(cooked_file_name(id)), &file)?;
            manifest.push(ManifestEntry {
                path: format!("{rel_path}#{path}"),
                id,
                type_path: type_path.to_string(),
            });
            entries.push(SubAssetEntry {
                path,
                id,
                type_path: type_path.to_string(),
            });
            Ok(id)
        };
    let clip_name = import.clip.name.clone();
    write_sub(
        format!("clip/{clip_name}"),
        "regia_animation::AnimationClip",
        cooked_clip_payload(&import.clip, Some(&import.skeleton), settings.lossless_clips)?,
        vec![parent_id],
    )?;
    if let Some((target_skeleton, clip)) = retargeted {
        crate::motiondb_cook::scan_clip_finite(&clip).map_err(|e| CookError::Animation(e.to_string()))?;
        let first = target_skeleton
            .joint_names
            .first()
            .cloned()
            .unwrap_or_else(|| "skeleton".into());
        let skeleton_id = write_sub(
            format!("retarget/skeleton/{first}"),
            "regia_animation::Skeleton",
            postcard::to_allocvec(&target_skeleton).map_err(CookError::Postcard)?,
            vec![],
        )?;
        write_sub(
            format!("retarget/{clip_name}"),
            "regia_animation::AnimationClip",
            cooked_clip_payload(&clip, Some(&target_skeleton), settings.lossless_clips)?,
            vec![skeleton_id],
        )?;
    }
    write_meta_sub_assets(source, &meta, entries)?;
    let payload = postcard::to_allocvec(&import.skeleton).map_err(CookError::Postcard)?;
    Ok((
        "bvh".to_string(),
        settings_hash,
        meta.id,
        "regia_animation::Skeleton".to_string(),
        payload,
        manifest,
    ))
}

/// The `.meta` of a converted source (FBX, OBJ), created with `importer` when missing.
fn load_converted_meta(source: &Path, importer: &str) -> Result<MetaSidecar, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, importer).map_err(CookError::Db)?;
    if meta.format_version != META_FORMAT_VERSION {
        return Err(CookError::Meta(regia_asset::MetaError::UnsupportedVersion {
            found: meta.format_version,
            supported: META_FORMAT_VERSION,
        }));
    }
    Ok(meta)
}

/// A source another importer converted to an in-memory glTF binary.
struct ConvertedSource<'a> {
    source: &'a Path,
    rel_path: &'a str,
    project_root: &'a Path,
    cache_dir: &'a Path,
    importer_version: u32,
    platform: &'a str,
    /// The importer name recorded in the database and `.meta`.
    importer: &'static str,
    /// Digest of the converter's settings and the files it read besides the source.
    inputs: [u8; 32],
}

/// Cooks a converted glTF binary as [`cook_gltf`] cooks a `.glb`: the LOD class table, meshes,
/// materials, textures (relative URIs against the source's directory), meshlets, skeletons and
/// clips, then the `.meta` with every sub-asset.
fn cook_converted_glb(src: ConvertedSource<'_>, meta: &MetaSidecar, glb: &[u8]) -> ImportedCook {
    let ConvertedSource {
        source,
        rel_path,
        project_root,
        cache_dir,
        importer_version,
        platform,
        importer,
        inputs,
    } = src;
    // The project's LOD class table reaches a converted source by its path, as it reaches a glTF.
    let (lod_table, _) = crate::lod_classes::LodClassTable::load(project_root);
    let (lod, lod_warnings) =
        crate::lod_classes::LodThresholdInputs::resolve(glb, rel_path, lod_table.as_ref());
    for warning in lod_warnings {
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"forward=convert;inputs=");
    hasher.update(&inputs);
    if let Some(sizes) = lod.class_sizes.as_deref() {
        hasher.update(b";lod_class=");
        hasher.update(lod.class.as_deref().unwrap_or_default().as_bytes());
        for size in sizes {
            hasher.update(&size.to_le_bytes());
        }
    }
    // The images the converted glb names by URI take the project's import rules like a glTF's
    // (`import_rules::referenced_settings`, ADR-3321), so the rules they match are an input of
    // this cook: editing one must change the key, or the ledger keeps the stale output (the
    // glTF path hashes these in `input_key`'s sidecar_rule term; rev-corr-assets,
    // sprint-corr-asset-pipeline-a 2).
    if let Ok(Some(rules)) = crate::import_rules::for_project_cached(project_root) {
        for path in gltf_uri_inputs_from_bytes(glb, source.parent(), false) {
            let Some(rel) = crate::import_rules::project_rel(project_root, &path) else {
                continue;
            };
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            for (_, rule) in rules.matched_rules(&rel, crate::import_rules::importer_for_ext(&ext)) {
                hasher.update(b";sidecar_rule=");
                hasher.update(rel.as_bytes());
                hasher.update(b"|");
                hasher.update(crate::import_rules::rule_key_text(rule).as_bytes());
            }
        }
    }
    let resolved = resolved_meta_settings(meta, project_root, rel_path, importer);
    // The converter's own digest need not cover the glTF keys of the `.meta` (ADR-7819).
    let clip_options = ClipCookOptions::from_settings(
        &crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref()),
    );
    let settings_hash = with_contact_curves_term(
        with_lossless_clips_term(*hasher.finalize().as_bytes(), clip_options.lossless),
        clip_options.contact_curves.as_ref(),
    );
    let imported = crate::gltf_import::import_gltf_converted(
        glb,
        source.parent(),
        meta.id,
        Forward::Convert,
        platform,
        resolved.as_deref(),
        crate::project_texture_quality::default_texture_quality(),
        &lod,
    )
    .map_err(CookError::Import)?;
    let payload = imported.primary_mesh.to_postcard().map_err(CookError::Postcard)?;
    // As in `cook_gltf_once`: check skeletons and clips before the first cache write, write the
    // mesh sub-assets last (audit asset_pipeline1 #4).
    // The source importer (FBX and the rest) already applied its own unit scale. This pass
    // keeps Convert and scale 1 so a `.meta` `scale: 0.01` is not applied a second time.
    // The `.meta` `clips` options apply to the converted scene's animations too (ADR-7904).
    let clip_imports =
        crate::gltf_settings::GltfImporterSettings::from_meta_string_converted(resolved.as_deref())
            .clip_options();
    let anim = prepare_gltf_animation(
        rel_path,
        importer_version,
        meta.id,
        glb,
        source.parent(),
        &GltfAnimSettings {
            clips: clip_imports,
            ..GltfAnimSettings::default()
        },
        &clip_options,
    )?;
    let mut sub_manifest = gltf_sub_asset_manifest(rel_path, &imported);
    let mut meta_entries = imported.meta_entries.clone();
    if crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref()).meshlets {
        let meshlets = crate::meshlet_cook::cook_meshlet_sub_assets(
            cache_dir,
            rel_path,
            importer_version,
            meta.id,
            imported.sub_assets.iter().map(|sub| {
                (
                    sub.path.as_str(),
                    sub.id,
                    sub.type_path.as_str(),
                    sub.payload.as_slice(),
                )
            }),
        )?;
        sub_manifest.extend(meshlets.manifest);
        meta_entries.extend(meshlets.meta);
    }
    let gltf_settings = crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref());
    if let Some(voxel) = gltf_settings.voxel_far {
        // The facts the cooked material does not carry (ADR-3227): vertex_ao and the base color
        // transform, so a masked material's coverage test matches what the source means.
        let source = crate::voxel_far_cook::VoxelFarSource::from_gltf(
            gltf_settings.vertex_ao,
            &imported.material_ids,
            &imported.extensions,
        );
        let voxels = crate::voxel_far_cook::cook_voxel_far_sub_assets_from(
            cache_dir,
            rel_path,
            importer_version,
            meta.id,
            &voxel,
            &source,
            imported.sub_assets.iter().map(|sub| {
                (
                    sub.path.as_str(),
                    sub.id,
                    sub.type_path.as_str(),
                    sub.payload.as_slice(),
                )
            }),
        )?;
        sub_manifest.extend(voxels.manifest);
        meta_entries.extend(voxels.meta);
    }
    cook_fracture_settings(
        &FractureCookCtx {
            cache_dir,
            rel_path,
            importer_version,
            meta_id: meta.id,
            primary: &payload,
            imported: &imported,
        },
        resolved.as_deref(),
        (&mut sub_manifest, &mut meta_entries),
    )?;
    {
        let settings = crate::gltf_settings::GltfImporterSettings::from_meta_string(resolved.as_deref());
        if settings.collision == crate::gltf_settings::CollisionImport::ConvexDecomposition {
            let hulls = crate::collision_cook::cook_collision_sub_assets(
                cache_dir,
                rel_path,
                importer_version,
                meta.id,
                &settings.collision_hulls,
                imported.sub_assets.iter().map(|sub| {
                    (
                        sub.path.as_str(),
                        sub.id,
                        sub.type_path.as_str(),
                        sub.payload.as_slice(),
                    )
                }),
            )?;
            sub_manifest.extend(hulls.manifest);
            meta_entries.extend(hulls.meta);
        }
    }

    write_gltf_sub_assets(cache_dir, importer_version, &imported)?;
    let anim_sub = anim.write(cache_dir)?;
    sub_manifest.extend(anim_sub.manifest);
    meta_entries.extend(anim_sub.meta);

    write_meta_sub_assets(source, meta, meta_entries)?;
    Ok((
        importer.to_string(),
        settings_hash,
        meta.id,
        "regia_asset::CookedMesh".to_string(),
        payload,
        sub_manifest,
    ))
}

/// One clip's cooked `cooked-clip` v3 payload: the lossless `Raw` body, bit-exact to `clip`, when
/// the source's `.meta` says `lossless_clips: true` (ADR-7819), else the joint tracks compressed
/// with the default tolerances over `skeleton`'s chain lengths (ADR-7227).
fn cooked_clip_payload(
    clip: &regia_animation::AnimationClip,
    skeleton: Option<&regia_animation::Skeleton>,
    lossless: bool,
) -> Result<Vec<u8>, CookError> {
    if lossless {
        clip.to_cooked_payload()
    } else {
        clip.to_compressed_cooked_payload(skeleton, &CompressionSettings::default())
    }
    .map_err(CookError::Postcard)
}

/// The `.meta` settings that shape the clips a glTF cook writes.
#[derive(Clone, Debug, Default)]
struct ClipCookOptions {
    /// `lossless_clips` (ADR-7819).
    lossless: bool,
    /// `contact_curves` (ADR-8308).
    contact_curves: Option<regia_animation::foot_ik::contact::ContactCurveSettings>,
}

impl ClipCookOptions {
    fn from_settings(settings: &crate::gltf_settings::GltfImporterSettings) -> Self {
        Self {
            lossless: settings.lossless_clips,
            contact_curves: settings.contact_curves.clone(),
        }
    }
}

/// Label the feet of `clip` and store its `contact.<foot>` curves (ADR-8308). A skin without the
/// feet (a prop in the same file) or bad thresholds leave the clip as it is, with a warning: the
/// curves are an aid, never a reason to refuse a clip.
fn cook_contact_curves(
    rel_path: &str,
    clip: &mut regia_animation::AnimationClip,
    skeleton: &regia_animation::Skeleton,
    settings: &regia_animation::foot_ik::contact::ContactCurveSettings,
) {
    if let Err(error) = regia_animation::foot_ik::contact::add_contact_curves(clip, skeleton, settings) {
        tracing::warn!(
            target: "regia_asset_pipeline",
            source = rel_path,
            clip = clip.name.as_str(),
            "contact curves not cooked: {error}"
        );
    }
}

/// `settings_hash` with the contact-curve term (ADR-8308) mixed in when the setting is on, for
/// sources whose settings hash does not already cover the `.meta` settings string (converted
/// FBX/USD files): a source that leaves it off keeps its cache key.
fn with_contact_curves_term(
    settings_hash: [u8; 32],
    contact_curves: Option<&regia_animation::foot_ik::contact::ContactCurveSettings>,
) -> [u8; 32] {
    let Some(settings) = contact_curves else {
        return settings_hash;
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(&settings_hash);
    hasher.update(b";clips=contact_curves:");
    hasher.update(format!("{settings:?}").as_bytes());
    *hasher.finalize().as_bytes()
}

/// `settings_hash` with the lossless-clip term (ADR-7819) mixed in when the switch is on: a source
/// that leaves it off keeps the cache key it had, and flipping it recooks the source.
fn with_lossless_clips_term(settings_hash: [u8; 32], lossless_clips: bool) -> [u8; 32] {
    if !lossless_clips {
        return settings_hash;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&settings_hash);
    hasher.update(b";clips=lossless");
    *hasher.finalize().as_bytes()
}

/// The glTF settings hash: `forward=convert`, then an axis term when the bake is not Convert at
/// scale 1 (`tag` 0 = Convert, 1 = Keep, then the scale's little-endian bits), then
/// `lod_attr=normals+uv0` when generated LODs are on, then the LOD class's sizes when the
/// project's class table reaches the file, then the [`gltf_inputs_digest`] when the file reads
/// other files or has `.meta` settings. With none of those it is the hash of `forward=convert`
/// alone, the key every such file had before the extra terms existed, so adding a term recooks
/// only the files it applies to.
pub(crate) fn gltf_settings_hash(
    lod_class: Option<&str>,
    class_sizes: Option<&[f32]>,
    inputs: Option<[u8; 32]>,
    axis: Option<(u8, f32)>,
    attribute_lods: bool,
) -> [u8; 32] {
    let extra_axis = axis.filter(|(tag, scale)| *tag != 0 || (*scale - 1.0).abs() > f32::EPSILON);
    if class_sizes.is_none() && inputs.is_none() && extra_axis.is_none() && !attribute_lods {
        return *blake3::hash(b"forward=convert").as_bytes();
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"forward=convert");
    if let Some((tag, scale)) = extra_axis {
        hasher.update(b";axis=");
        hasher.update(&[tag]);
        hasher.update(&scale.to_le_bytes());
    }
    if attribute_lods {
        hasher.update(b";lod_attr=normals+uv0");
    }
    if let Some(sizes) = class_sizes {
        hasher.update(b";lod_class=");
        hasher.update(lod_class.unwrap_or_default().as_bytes());
        for size in sizes {
            hasher.update(&size.to_le_bytes());
        }
    }
    if let Some(digest) = inputs {
        hasher.update(b";inputs=");
        hasher.update(&digest);
    }
    *hasher.finalize().as_bytes()
}

/// Digest of what a glTF cook reads besides the file's own bytes: its `.meta` `settings` string
/// (meshlets, generated LODs, the closed-mesh hint) and every file a `.gltf` references by relative
/// URI (`.bin` buffers and images, [`gltf_sidecars_of`]) with each one's own `.meta`, whose
/// texture overrides the import applies. A referenced file is named by its path relative to the
/// glTF, so moving the project keeps the key; a missing one hashes as `<missing>` (the import then
/// fails and reports it). `None` when there is nothing besides the file (every `.glb` without
/// settings).
pub(crate) fn gltf_inputs_digest(source: &Path, meta_settings: Option<&str>) -> Option<[u8; 32]> {
    let settings = meta_settings.filter(|s| !s.trim().is_empty());
    let sidecars = gltf_sidecars_of(source);
    if settings.is_none() && sidecars.is_empty() {
        return None;
    }
    let dir = source.parent().unwrap_or_else(|| Path::new(""));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"settings=");
    hasher.update(settings.unwrap_or_default().as_bytes());
    for path in sidecars {
        let name = path
            .strip_prefix(dir)
            .map(normalise_asset_path)
            .unwrap_or_else(|_| {
                crate::gltf_texture_share::project_relative_path(&path)
                    .unwrap_or_else(|| normalise_asset_path(&path))
            });
        hasher.update(b";input=");
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        match std::fs::read(&path) {
            Ok(bytes) => hasher.update(blake3::hash(&bytes).as_bytes()),
            Err(_) => hasher.update(b"<missing>"),
        };
        if let Ok(meta) = std::fs::read(meta_path_for(&path)) {
            hasher.update(b";meta=");
            hasher.update(blake3::hash(&meta).as_bytes());
        }
    }
    Some(*hasher.finalize().as_bytes())
}

/// The manifest rows of the glTF import's mesh, material and texture sub-assets, in import order.
fn gltf_sub_asset_manifest(rel_path: &str, imported: &GltfImportResult) -> Vec<ManifestEntry> {
    imported
        .sub_assets
        .iter()
        .map(|sub| ManifestEntry {
            path: format!("{rel_path}#{}", sub.path),
            id: sub.id,
            type_path: sub.type_path.clone(),
        })
        .collect()
}

/// Writes the glTF import's mesh, material and texture sub-assets ([`gltf_sub_asset_manifest`]
/// lists them). Called once every check of the cook has passed, so a refused recook leaves the
/// files of the previous cook as they were.
fn write_gltf_sub_assets(
    cache_dir: &Path,
    importer_version: u32,
    imported: &GltfImportResult,
) -> Result<(), CookError> {
    for sub in &imported.sub_assets {
        if crate::gltf_texture_share::already_written(sub.id) {
            continue;
        }
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: type_tag_for_path(&sub.type_path),
            asset_id: sub.id,
            importer_version: crate::gltf_texture_share::importer_version(sub.id).unwrap_or(importer_version),
            dependencies: sub.dependencies.clone(),
            payload_len: sub.payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &sub.payload).map_err(CookError::Cooked)?;
        write_atomic(&cache_dir.join(cooked_file_name(sub.id)), &file)?;
        crate::gltf_texture_share::written(sub.id);
    }
    Ok(())
}

/// A glTF file's skeleton and clip sub-assets, imported, checked and encoded, not yet written.
struct PreparedAnim {
    /// Cooked file bytes per sub-asset id, in manifest order.
    files: Vec<(AssetId, Vec<u8>)>,
    manifest: Vec<ManifestEntry>,
    meta: Vec<SubAssetEntry>,
}

impl PreparedAnim {
    /// Writes every prepared file into `cache_dir`.
    fn write(self, cache_dir: &Path) -> Result<AnimSubResult, CookError> {
        for (id, file) in &self.files {
            write_atomic(&cache_dir.join(cooked_file_name(*id)), file)?;
        }
        Ok(AnimSubResult {
            manifest: self.manifest,
            meta: self.meta,
        })
    }
}

struct AnimSubResult {
    manifest: Vec<ManifestEntry>,
    meta: Vec<SubAssetEntry>,
}

/// Imports, checks (finite keys) and encodes the skeleton and clip sub-assets of a glTF file that
/// has skins or animations, writing nothing: a skin or clip the cook refuses must not leave the
/// skins and clips before it rewritten under the previous cook's database row (audit
/// asset_pipeline1 #4). [`PreparedAnim::write`] writes them.
///
/// `clips` carries the `.meta` clip settings: `lossless_clips` (ADR-7819) stores every clip as a
/// lossless `Raw` body instead of the compressed one; `contact_curves` (ADR-8308) labels the feet
/// of every skinned clip and stores the labels as `contact.<foot>` curves first.
fn prepare_gltf_animation(
    rel_path: &str,
    importer_version: u32,
    parent_id: AssetId,
    source_bytes: &[u8],
    base: Option<&Path>,
    anim: &GltfAnimSettings,
    clips: &ClipCookOptions,
) -> Result<PreparedAnim, CookError> {
    let lossless_clips = clips.lossless;
    let mut files = Vec::new();
    let mut manifest = Vec::new();
    let mut meta: Vec<SubAssetEntry> = Vec::new();
    let mut used_clip_paths = BTreeSet::new();
    let mut used_nodeclip_paths = BTreeSet::new();
    let anim_import = import_gltf_animation_with_base(source_bytes, base, anim)
        .map_err(|e| CookError::Import(ImportError::Gltf(e.to_string())))?;
    for (skin_index, skin) in anim_import.skins.into_iter().enumerate() {
        // Skeleton sub-asset
        let skeleton_path = if skin.skeleton.joint_names.is_empty() {
            format!("skeleton/{skin_index}")
        } else {
            let name = skin
                .skeleton
                .joint_names
                .first()
                .map(|n| n.as_str())
                .unwrap_or("unnamed");
            format!("skeleton/{name}")
        };
        // Two skins whose first joints share a name must not share a sub-asset id.
        let skeleton_path = if meta.iter().any(|e| e.path == skeleton_path) {
            format!("{skeleton_path}~{skin_index}")
        } else {
            skeleton_path
        };
        let skeleton_id = crate::subasset_remap::sub_id(parent_id, &skeleton_path);

        // Populate sockets on the skeleton
        let mut skeleton = skin.skeleton;
        skeleton.sockets = skin
            .sockets
            .iter()
            .map(|s| SkeletonSocket {
                name: s.name.clone(),
                joint: s.joint.clone(),
                offset: s.offset,
            })
            .collect();

        let skeleton_payload = postcard::to_allocvec(&skeleton).map_err(CookError::Postcard)?;
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: type_tag_for_path("regia_animation::Skeleton"),
            asset_id: skeleton_id,
            importer_version,
            dependencies: vec![],
            payload_len: skeleton_payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &skeleton_payload).map_err(CookError::Cooked)?;
        files.push((skeleton_id, file));
        manifest.push(ManifestEntry {
            path: format!("{rel_path}#{skeleton_path}"),
            id: skeleton_id,
            type_path: "regia_animation::Skeleton".to_string(),
        });
        meta.push(SubAssetEntry {
            path: skeleton_path,
            id: skeleton_id,
            type_path: "regia_animation::Skeleton".to_string(),
        });

        // AnimationClip sub-assets
        for mut clip in skin.clips {
            crate::motiondb_cook::scan_clip_finite(&clip).map_err(|e| CookError::Animation(e.to_string()))?;
            if let Some(contacts) = &clips.contact_curves {
                cook_contact_curves(rel_path, &mut clip, &skeleton, contacts);
            }
            let clip_name = if clip.name.is_empty() {
                format!("unnamed_{}", clip.tracks.len())
            } else {
                clip.name.clone()
            };
            // One animation drives every skin it targets, so its clip name repeats per skin: the
            // later skins get `~<skin index>`; `unique_sub_path` then separates clips that share a
            // name within one skin.
            let mut clip_path = format!("clip/{clip_name}");
            if used_clip_paths.contains(&clip_path) {
                clip_path = format!("clip/{clip_name}~{skin_index}");
            }
            let clip_path = unique_sub_path(&mut used_clip_paths, clip_path);
            let clip_id = crate::subasset_remap::sub_id(parent_id, &clip_path);
            let clip_payload = cooked_clip_payload(&clip, Some(&skeleton), lossless_clips)?;
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: type_tag_for_path("regia_animation::AnimationClip"),
                asset_id: clip_id,
                importer_version,
                dependencies: vec![skeleton_id],
                payload_len: clip_payload.len() as u64,
            };
            let file = encode_cooked_file(&header, &clip_payload).map_err(CookError::Cooked)?;
            files.push((clip_id, file));
            manifest.push(ManifestEntry {
                path: format!("{rel_path}#{clip_path}"),
                id: clip_id,
                type_path: "regia_animation::AnimationClip".to_string(),
            });
            meta.push(SubAssetEntry {
                path: clip_path,
                id: clip_id,
                type_path: "regia_animation::AnimationClip".to_string(),
            });
        }
    }
    // Per-node morph clips (ADR-297): independent of skins, so a file of plain morphing meshes
    // cooks its animations too. Their own sub-asset path keeps them apart from skin clips.
    for clip in anim_import.node_clips {
        crate::motiondb_cook::scan_clip_finite(&clip).map_err(|e| CookError::Animation(e.to_string()))?;
        let clip_name = if clip.name.is_empty() {
            format!("unnamed_{}", clip.morph_tracks.len())
        } else {
            clip.name.clone()
        };
        let clip_path = unique_sub_path(&mut used_nodeclip_paths, format!("nodeclip/{clip_name}"));
        let clip_id = crate::subasset_remap::sub_id(parent_id, &clip_path);
        let clip_payload = cooked_clip_payload(&clip, None, lossless_clips)?;
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: type_tag_for_path("regia_animation::AnimationClip"),
            asset_id: clip_id,
            importer_version,
            dependencies: vec![],
            payload_len: clip_payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &clip_payload).map_err(CookError::Cooked)?;
        files.push((clip_id, file));
        manifest.push(ManifestEntry {
            path: format!("{rel_path}#{clip_path}"),
            id: clip_id,
            type_path: "regia_animation::AnimationClip".to_string(),
        });
        meta.push(SubAssetEntry {
            path: clip_path,
            id: clip_id,
            type_path: "regia_animation::AnimationClip".to_string(),
        });
    }
    Ok(PreparedAnim {
        files,
        manifest,
        meta,
    })
}

/// Cooks a standalone raster image (PNG; JPEG, TGA and BMP through the same path, ADR-1855):
/// block-compressed with full mips by default, its usage from the file name
/// ([`usage_from_file_name`]: a `_normal` map cooks BC5, an `_orm` map linear BC7), every choice
/// overridable in its `.meta` ([`TextureImporterSettings`]).
fn cook_png(
    source: &Path,
    bytes: &[u8],
    ext: &str,
    rel_path: &str,
    project_root: &Path,
    _importer_version: u32,
    platform: &str,
) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, ext).map_err(CookError::Db)?;
    let resolved = resolved_meta_settings(&meta, project_root, rel_path, ext);
    let settings = TextureImporterSettings::from_meta_string(resolved.as_deref());
    let usage = settings.usage_or(usage_from_file_name(source));
    let settings_hash =
        *blake3::hash(format!("{};auto={usage:?}", settings.fingerprint()).as_bytes()).as_bytes();
    // Heightfields use 16-bit grayscale PNG; everything else is RGBA8. TGA and BMP have no magic
    // number the decoder can guess from, so they are decoded by extension.
    let raster = match ext {
        "tga" => Some(image::ImageFormat::Tga),
        "bmp" => Some(image::ImageFormat::Bmp),
        _ => None,
    };
    let (type_path, payload) = if raster.is_none()
        && settings.heightfield != Some(false)
        && (settings.heightfield == Some(true) || import_png_height(bytes).is_ok())
    {
        let hf = import_png_height(bytes).map_err(CookError::Import)?;
        // The loader refuses a cooked heightfield over the edge cap (finding 33, ADR-4922); a
        // larger terrain cooks through the tiled terrain path instead.
        if hf.width > regia_asset::HEIGHTFIELD_MAX_EDGE || hf.height > regia_asset::HEIGHTFIELD_MAX_EDGE {
            return Err(CookError::Import(ImportError::Image(format!(
                "a {} x {} heightfield has a side over {} samples: cook it as a tiled terrain",
                hf.width,
                hf.height,
                regia_asset::HEIGHTFIELD_MAX_EDGE
            ))));
        }
        (
            "regia_asset::Heightfield".to_string(),
            hf.to_postcard().map_err(CookError::Postcard)?,
        )
    } else {
        let mut img = match raster {
            Some(format) => import_image_rgba8_as(bytes, format),
            None => import_png_rgba8(bytes),
        }
        .map_err(CookError::Import)?;
        // The channel conventions of spec 22 T2.14 (ADR-3320), from the `.meta`: a
        // DirectX-convention normal's green flips, a packed map in another channel order
        // swizzles to ORM. Both at cook, never on disk.
        crate::texture_settings::apply_channel_transforms(
            &mut img.rgba8,
            usage,
            settings.normal_green_or_default(),
            settings.orm_channel_order_or_default(),
        );
        cook_rgba8_payload(img, &settings, usage, platform)?
    };
    Ok((
        "png".to_string(),
        settings_hash,
        meta.id,
        type_path.clone(),
        payload,
        vec![],
    ))
}

/// Decodes a raster image of a known container format to RGBA8.
pub(crate) fn import_image_rgba8_as(
    bytes: &[u8],
    format: image::ImageFormat,
) -> Result<regia_asset::CookedImage, ImportError> {
    let img =
        image::load_from_memory_with_format(bytes, format).map_err(|e| ImportError::Image(e.to_string()))?;
    let rgba8 = img.into_rgba8();
    let (width, height) = rgba8.dimensions();
    let cooked = regia_asset::CookedImage {
        width,
        height,
        rgba8: rgba8.into_raw(),
    };
    cooked.validate().map_err(|e| ImportError::Image(e.to_string()))?;
    Ok(cooked)
}

/// The cooked texture payload of an RGBA8 image under `settings`: resized to `max_size`, then
/// block-compressed with full mips for `platform` (or kept RGBA8 when `compress` is off or the
/// image is smaller than a block). Returns the type path and the postcard payload.
pub(crate) fn cook_rgba8_payload(
    img: regia_asset::CookedImage,
    settings: &TextureImporterSettings,
    usage: regia_asset::TextureCookUsage,
    platform: &str,
) -> Result<(String, Vec<u8>), CookError> {
    let (width, height, rgba8) = settings.fit(img.width, img.height, img.rgba8);
    let img = regia_asset::CookedImage { width, height, rgba8 };
    // `compress: false` (or an image too small for a block) stays RGBA8.
    let cooked = settings.compress().then(|| {
        crate::texture_compress::cook_texture_rgba8_coverage(
            img.width,
            img.height,
            &img.rgba8,
            usage,
            platform,
            settings.quality_or(crate::project_texture_quality::default_texture_quality()),
            settings.coverage_cutoff(usage),
        )
    });
    match cooked {
        Some(Ok(tex)) => {
            let tex = settings
                .apply_stream(tex)
                .map_err(|e| CookError::Import(ImportError::Image(e.to_string())))?;
            Ok((
                "regia_asset::CookedTexture".to_string(),
                tex.to_postcard().map_err(CookError::Postcard)?,
            ))
        }
        None | Some(Err(TextureCookError::TooSmall { .. })) => Ok((
            "regia_asset::CookedImage".to_string(),
            img.to_postcard().map_err(CookError::Postcard)?,
        )),
        Some(Err(e)) => Err(CookError::Import(ImportError::Image(e.to_string()))),
    }
}

fn cook_material_ron(source: &Path, bytes: &[u8], _importer_version: u32) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "material_ron").map_err(CookError::Db)?;
    let mat = MaterialAssetRon::from_ron(bytes).map_err(CookError::Material)?;
    let settings_hash = *blake3::hash(bytes).as_bytes();
    let id = meta.id;
    let cooked = CookedStandardMaterial::new(mat);
    let payload = cooked.to_postcard().map_err(CookError::Postcard)?;
    Ok((
        "material_ron".to_string(),
        settings_hash,
        id,
        "regia_asset::CookedStandardMaterial".to_string(),
        payload,
        vec![],
    ))
}

/// The asset ids a material's texture slots name (`Asset` slots only; a `Path` slot is resolved
/// by the cook's manifest, not here), sorted and deduped: the cooked header's dependencies, so
/// chunking a material pulls its maps into the same pak (ADR-3320).
fn material_texture_deps(mat: &MaterialAssetRon) -> Vec<AssetId> {
    let mut deps = Vec::new();
    let mut push_slot = |slot: &Option<regia_asset::TextureSlot>| {
        if let Some(regia_asset::TextureSlot::Asset(id)) = slot {
            deps.push(*id);
        }
    };
    push_slot(&mat.base.base_color_map);
    push_slot(&mat.base.normal_map);
    push_slot(&mat.base.orm_map);
    push_slot(&mat.base.emissive_map);
    push_slot(&mat.clearcoat.as_ref().and_then(|c| c.normal_map.clone()));
    if let Some(hs) = &mat.hard_surface {
        push_slot(&hs.detail.as_ref().and_then(|d| d.albedo.clone()));
        push_slot(&hs.detail.as_ref().and_then(|d| d.normal.clone()));
        push_slot(&hs.detail.as_ref().and_then(|d| d.roughness.clone()));
        push_slot(&hs.detail.as_ref().and_then(|d| d.mask.clone()));
        push_slot(&hs.parallax.as_ref().and_then(|p| p.height_map.clone()));
        push_slot(&hs.edge_wear.as_ref().and_then(|e| e.curvature_map.clone()));
    }
    push_slot(&mat.thin_film.as_ref().and_then(|f| f.factor_map.clone()));
    push_slot(&mat.thin_film.as_ref().and_then(|f| f.thickness_map.clone()));
    // After `push_slot`: that closure holds `deps`, so these pushes stay outside it.
    for layer in &mat.layers {
        if let regia_asset::TextureSlot::Asset(id) = &layer.mask {
            deps.push(*id);
        }
        if let Some(regia_asset::TextureSlot::Asset(id)) = &layer.normal_map {
            deps.push(*id);
        }
    }
    deps.sort();
    deps.dedup();
    deps
}

/// `.scatter.ron` (v1 or v2): validated, migrated to v2 and stored in its cooked form
/// ([`regia_terrain::ScatterSet::to_cooked`]). Placement is not baked: it is deterministic per
/// cell and streams at runtime, so the pak carries only the rules (ADR-047).
fn cook_scatter_ron(source: &Path, bytes: &[u8]) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "scatter_ron").map_err(CookError::Db)?;
    let text = std::str::from_utf8(bytes).map_err(|e| CookError::Scatter(format!("not UTF-8: {e}")))?;
    let set =
        regia_terrain::ScatterSet::from_ron(text, None).map_err(|e| CookError::Scatter(e.to_string()))?;
    let payload = set.to_cooked().map_err(|e| CookError::Scatter(e.to_string()))?;
    Ok((
        "scatter_ron".to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        regia_terrain::SCATTER_TYPE_PATH.to_string(),
        payload,
        vec![],
    ))
}

/// `.cloth.ron` / `.soft.ron` (spec 55 § 4): the glTF nodes they name resolve relative to the
/// source; the key takes the payload's hash because those glTF files are inputs too.
fn cook_cloth_ron(source: &Path, soft: bool) -> ImportedCook {
    let importer = if soft { "soft_ron" } else { "cloth_ron" };
    let meta = AssetDatabase::load_or_create_meta(source, importer).map_err(CookError::Db)?;
    let cooked = if soft {
        crate::cloth_cook::cook_soft_payload(source)
    } else {
        crate::cloth_cook::cook_cloth_payload(source)
    };
    let payload = cooked.map_err(|e| CookError::Cloth(format!("{}: {e}", e.code())))?;
    let type_path = if soft {
        regia_physics::cloth::asset::COOKED_SOFT_TYPE_PATH
    } else {
        regia_physics::cloth::asset::COOKED_CLOTH_TYPE_PATH
    };
    Ok((
        importer.to_string(),
        *blake3::hash(&payload).as_bytes(),
        meta.id,
        type_path.to_string(),
        payload,
        vec![],
    ))
}

/// `.motiondb.ron` (spec 52 § 4, ADR-2409): its skeleton and clips are glTF sub-assets read from
/// the cooked cache (`cook_project` cooks motion databases after the glTF files).
fn cook_motiondb_ron(
    source: &Path,
    bytes: &[u8],
    project_root: &Path,
    cache_dir: &Path,
) -> Result<(crate::motiondb_cook::CookedMotionDb, AssetId), CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, crate::motiondb_cook::MOTIONDB_IMPORTER)
        .map_err(CookError::Db)?;
    let text = std::str::from_utf8(bytes)
        .map_err(|e| CookError::Animation(format!("motiondb_parse: not UTF-8: {e}")))?;
    let mut inputs = crate::motiondb_cook::ProjectInputs::new(project_root, cache_dir);
    let cooked = crate::motiondb_cook::cook_motiondb(text, &mut inputs)
        .map_err(|e| CookError::Animation(e.to_string()))?;
    Ok((cooked, meta.id))
}

/// `.fgiv` (baked GI volume, ADR-206): checked, then stored unchanged. The file is already the
/// GPU-ready form the renderer uploads (`docs/formats/baked-gi.md`), so the cooked payload is the
/// file's own bytes; a file of another version is a cook error rather than a silent skip.
fn cook_baked_gi(source: &Path, bytes: &[u8]) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "fgiv").map_err(CookError::Db)?;
    regia_asset::baked_gi::BakedGiVolume::from_bytes(bytes).map_err(|e| CookError::BakedGi(e.to_string()))?;
    Ok((
        "fgiv".to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        BAKED_GI_TYPE_PATH.to_string(),
        bytes.to_vec(),
        vec![],
    ))
}

/// `.cube` colour LUT (`docs/formats/scene-files.md`, `docs/formats/post-volume-v3.md`): parsed
/// with the renderer's rules, then stored unchanged. The cooked payload is the file's own text;
/// a malformed file is a cook error rather than a silent skip, like `.fgiv`. A file over the
/// runtime's `ColorLut::MAX_FILE_BYTES` cap is refused here: every runtime load of it would fail.
fn cook_color_lut(source: &Path, bytes: &[u8]) -> ImportedCook {
    if bytes.len() as u64 > regia_render::lut::ColorLut::MAX_FILE_BYTES {
        return Err(CookError::ColorLut(format!(
            "{} is larger than {} bytes",
            source.display(),
            regia_render::lut::ColorLut::MAX_FILE_BYTES
        )));
    }
    let meta = AssetDatabase::load_or_create_meta(source, "cube").map_err(CookError::Db)?;
    let text = std::str::from_utf8(bytes).map_err(|e| CookError::ColorLut(format!("not UTF-8: {e}")))?;
    regia_render::lut::ColorLut::parse_cube(text).map_err(|e| CookError::ColorLut(e.to_string()))?;
    Ok((
        "cube".to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        COLOR_LUT_TYPE_PATH.to_string(),
        bytes.to_vec(),
        vec![],
    ))
}

/// The reflect path of the colour LUT a cooked `.cube` carries.
pub const COLOR_LUT_TYPE_PATH: &str = "regia_render::lut::ColorLut";

/// Whether `source` is a procedural bake file (`.pgout`).
fn is_pgout(source: &Path) -> bool {
    source
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pgout"))
}

/// A `.pgout` bake: decoded once (magic, version, hashes), then stored byte for byte under
/// [`regia_procgen::pgout::PGOUT_COOKED_TYPE_PATH`]. An older version ships as it is (the loader
/// reads versions 1 to the current one).
fn cook_pgout(source: &Path, bytes: &[u8]) -> ImportedCook {
    regia_procgen::pgout::decode(bytes)
        .map_err(|e| CookError::Pgout(format!("{}: {e}", source.display())))?;
    let meta = AssetDatabase::load_or_create_meta(source, "pgout").map_err(CookError::Db)?;
    Ok((
        "pgout".to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        regia_procgen::pgout::PGOUT_COOKED_TYPE_PATH.to_string(),
        bytes.to_vec(),
        vec![],
    ))
}

/// Extension content stored byte for byte; the extension's `AssetLoaderDesc` decodes it.
fn cook_extension_raw(source: &Path, bytes: &[u8]) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "ext_raw").map_err(CookError::Db)?;
    Ok((
        "ext_raw".to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        EXTENSION_CONTENT_TYPE_PATH.to_string(),
        bytes.to_vec(),
        vec![],
    ))
}

/// The reflect path of extension content stored as it is.
pub const EXTENSION_CONTENT_TYPE_PATH: &str = "regia_asset::ExtensionContent";

/// The reflect path of the baked GI volume a cooked `.fgiv` carries.
pub const BAKED_GI_TYPE_PATH: &str = "regia_asset::baked_gi::BakedGiVolume";

/// `.flmap` (lightmap, ADR-860, ADR-1556): read with every older version's rules, then stored at
/// the current version (`regia_asset::lightmap::Lightmap::to_bytes`). The file is already the
/// GPU-ready form (`docs/formats/lightmap.md`), so the cooked payload is a `.flmap` the runtime
/// reads with `Lightmap::from_bytes`; a damaged or newer file is a cook error. The `.meta`
/// settings may ask for block-compressed texels (`crate::lightmap_cook`, ADR-2314): BC6H, ASTC
/// HDR, or `auto` by `platform`; by default the texels are kept as they are.
fn cook_lightmap(source: &Path, bytes: &[u8], platform: &str) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "flmap").map_err(CookError::Db)?;
    let settings = crate::lightmap_cook::LightmapCookSettings::from_meta_string(meta.settings.as_deref())
        .map_err(CookError::Lightmap)?;
    let mut map = regia_asset::lightmap::Lightmap::from_bytes(bytes)
        .map_err(|e| CookError::Lightmap(format!("{}: {e}", e.code())))?;
    let encoding = settings.encoding.for_platform(platform);
    let directions = settings.directions.for_platform(platform);
    // The default keeps the source's own encoding (and the settings hash it always had).
    let settings_hash = if encoding == regia_asset::lightmap::LightmapTexelEncoding::Rgb9e5
        && directions == regia_asset::lightmap::LightmapDirectionEncoding::Rgba8
    {
        *blake3::hash(bytes).as_bytes()
    } else {
        let _span = tracing::info_span!("cook_lightmap_encode", encoding = encoding.name()).entered();
        map.encode_texels(encoding)
            .map_err(|e| CookError::Lightmap(format!("{}: {e}", e.code())))?;
        // The direction blocks (ADR-2635); a lightmap without directions is unchanged.
        crate::lightmap_cook::encode_lightmap_directions(&mut map, directions)
            .map_err(CookError::Lightmap)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(bytes);
        hasher.update(b"lightmap-encoding:");
        hasher.update(encoding.name().as_bytes());
        if directions != regia_asset::lightmap::LightmapDirectionEncoding::Rgba8 {
            hasher.update(b"lightmap-directions:");
            hasher.update(directions.name().as_bytes());
        }
        *hasher.finalize().as_bytes()
    };
    let payload = map
        .to_bytes()
        .map_err(|e| CookError::Lightmap(format!("{}: {e}", e.code())))?;
    Ok((
        "flmap".to_string(),
        settings_hash,
        meta.id,
        LIGHTMAP_TYPE_PATH.to_string(),
        payload,
        vec![],
    ))
}

/// The reflect path of the lightmap a cooked `.flmap` carries.
pub const LIGHTMAP_TYPE_PATH: &str = "regia_asset::lightmap::Lightmap";

/// Hash of the files a 2D source reads besides itself (atlas images, tileset images), so editing
/// one recooks the atlas or map that uses it.
pub(crate) fn input_files_hash(own: &[u8], files: impl IntoIterator<Item = std::path::PathBuf>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(own);
    for path in files {
        hasher.update(path.to_string_lossy().as_bytes());
        match std::fs::read(&path) {
            Ok(bytes) => hasher.update(blake3::hash(&bytes).as_bytes()),
            Err(_) => hasher.update(b"<missing>"),
        };
    }
    *hasher.finalize().as_bytes()
}

/// The image files of an atlas source, resolved exactly as the packer resolves them
/// (`join_portable`, so a Windows-authored `a\\b.png` names the same file the cook reads).
fn atlas_image_paths(source: &regia_2d::AtlasSource, dir: &Path) -> Vec<PathBuf> {
    source
        .sprites
        .iter()
        .filter_map(|sp| join_portable(dir, &sp.image).ok())
        .collect()
}

/// `.atlas.ron`: packed into a cooked [`regia_2d::SpriteAtlas`] (`texture_packer`).
fn cook_atlas_ron(source: &Path, bytes: &[u8]) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "atlas_ron").map_err(CookError::Db)?;
    let text = std::str::from_utf8(bytes).map_err(|e| CookError::Sprite2d(format!("not UTF-8: {e}")))?;
    let dir = source.parent().unwrap_or(Path::new(""));
    let atlas = crate::atlas_cook::cook_atlas(text, dir).map_err(|e| CookError::Sprite2d(e.to_string()))?;
    let images = regia_2d::AtlasSource::from_ron(text)
        .map(|s| atlas_image_paths(&s, dir))
        .unwrap_or_default();
    let payload = atlas
        .to_postcard()
        .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    Ok((
        "atlas_ron".to_string(),
        input_files_hash(bytes, images),
        meta.id,
        regia_2d::atlas::ATLAS_TYPE_PATH.to_string(),
        payload,
        vec![],
    ))
}

/// `.tilemap.ron`, or a Tiled `.tmx`/`.tmj` imported into one: cooked into a
/// [`regia_2d::CookedTileMap`] with its tileset images embedded (RGBA8).
fn cook_tilemap_source(source: &Path, bytes: &[u8], importer: &str) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, importer).map_err(CookError::Db)?;
    let map = if importer == "tiled" {
        let imported =
            crate::tiled_import::import_tiled(source).map_err(|e| CookError::Sprite2d(e.to_string()))?;
        for warning in &imported.warnings {
            tracing::warn!(target: "regia_asset_pipeline", path = %source.display(), "{warning}");
        }
        imported.map
    } else {
        let text = std::str::from_utf8(bytes).map_err(|e| CookError::Sprite2d(format!("not UTF-8: {e}")))?;
        regia_2d::TileMapAsset::from_ron(text).map_err(|e| CookError::Sprite2d(e.to_string()))?
    };
    let dir = source.parent().unwrap_or(Path::new(""));
    // The imported map (which includes external tilesets) plus the tileset images key the cache.
    let map_ron = map.to_ron().map_err(|e| CookError::Sprite2d(e.to_string()))?;
    let images = map
        .tilesets
        .iter()
        .map(|t| join_portable(dir, &t.image))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CookError::Sprite2d)?;
    let settings_hash = input_files_hash(map_ron.as_bytes(), images);
    let cooked =
        crate::tiled_import::cook_tilemap(map, dir).map_err(|e| CookError::Sprite2d(e.to_string()))?;
    let payload = cooked
        .to_postcard()
        .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    Ok((
        importer.to_string(),
        settings_hash,
        meta.id,
        regia_2d::tilemap::TILEMAP_TYPE_PATH.to_string(),
        payload,
        vec![],
    ))
}

/// An Aseprite `.aseprite`/`.ase` (ADR-3358): composited frames packed as clip strips through
/// the atlas packer into the cooked [`regia_2d::SpriteAtlas`] (no new runtime format), with the
/// import report (frames, tags, layers, warnings) written for `import.report`.
fn cook_aseprite(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    cache_dir: &Path,
    _importer_version: u32,
) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "aseprite").map_err(CookError::Db)?;
    let settings =
        crate::aseprite_import::AsepriteImportSettings::try_from_meta_string(meta.settings.as_deref())
            .map_err(|e| CookError::Import(e.into()))?;
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("sprite")
        .to_string();
    let imported =
        crate::aseprite_import::import_aseprite(bytes, &stem).map_err(|e| CookError::Import(e.into()))?;
    for warning in &imported.warnings {
        tracing::warn!(target: "regia_asset_pipeline", path = %source.display(), "{warning}");
    }
    let entries: Vec<crate::atlas_cook::Entry> = imported
        .sprites
        .iter()
        .map(|sprite| {
            let image = image::RgbaImage::from_raw(
                sprite.image.width,
                sprite.image.height,
                sprite.image.rgba8.clone(),
            )
            .ok_or_else(|| CookError::Sprite2d(format!("sprite `{}` has bad pixels", sprite.name)))?;
            Ok(crate::atlas_cook::Entry::decoded(
                sprite.name.clone(),
                image,
                sprite.pivot,
                sprite.nine_slice,
            ))
        })
        .collect::<Result<Vec<_>, CookError>>()?;
    let sprite_count = entries.len();
    let atlas = crate::atlas_cook::pack_entries(
        entries,
        settings.max_size,
        settings.padding,
        settings.extrude,
        settings.trim,
    )
    .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    let payload = atlas
        .to_postcard()
        .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    info!(
        target: "regia_asset_pipeline",
        path = %rel_path,
        frames = imported.frames.len(),
        clips = sprite_count,
        "imported Aseprite"
    );
    let data = crate::import_report::aseprite_report_data(&imported);
    crate::import_report::ImportReports::load(cache_dir)
        .upsert_and_save(
            cache_dir,
            rel_path,
            crate::import_report::ImportReportEntry {
                importer: "aseprite".to_string(),
                data,
                warnings: imported.warnings.clone(),
            },
        )
        .map_err(CookError::Io)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aseprite;settings=");
    hasher.update(settings.fingerprint().as_bytes());
    hasher.update(blake3::hash(bytes).as_bytes());
    Ok((
        "aseprite".to_string(),
        *hasher.finalize().as_bytes(),
        meta.id,
        regia_2d::atlas::ATLAS_TYPE_PATH.to_string(),
        payload,
        vec![],
    ))
}

/// An LDtk `.ldtk` (ADR-3358): the whole world as the primary cooked [`regia_2d::CookedTileMap`]
/// (levels merged, world positions as layer offsets) plus one `level/<identifier>` sub-asset map
/// per level with stable ids; the import report is written for `import.report`.
fn cook_ldtk(
    source: &Path,
    bytes: &[u8],
    rel_path: &str,
    cache_dir: &Path,
    importer_version: u32,
) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "ldtk").map_err(CookError::Db)?;
    let dir = source.parent().unwrap_or(Path::new(""));
    let imported = crate::ldtk_import::import_ldtk(bytes, dir).map_err(|e| CookError::Import(e.into()))?;
    for warning in &imported.warnings {
        tracing::warn!(target: "regia_asset_pipeline", path = %source.display(), "{warning}");
    }
    // The cache key covers the source, the external level files and the tileset images.
    let images = imported
        .merged
        .tilesets
        .iter()
        .map(|t| join_portable(dir, &t.image))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CookError::Sprite2d)?;
    let settings_hash = input_files_hash(bytes, imported.side_files.iter().chain(images.iter()).cloned());
    // The report is saved once the maps cooked: a failed cook must not leave one behind.
    let data = crate::import_report::ldtk_report_data(&imported);
    let warnings = imported.warnings.clone();

    let parent_id = meta.id;
    let mut manifest = Vec::new();
    let mut entries: Vec<SubAssetEntry> = Vec::new();
    let level_count = imported.levels.len();
    for (level_index, level_map) in imported.levels.into_iter().enumerate() {
        let identifier = imported.level_infos[level_index].identifier.clone();
        let path = format!("level/{identifier}");
        let id = crate::subasset_remap::sub_id(parent_id, &path);
        let cooked = crate::tiled_import::cook_tilemap(level_map, dir)
            .map_err(|e| CookError::Sprite2d(e.to_string()))?;
        let payload = cooked
            .to_postcard()
            .map_err(|e| CookError::Sprite2d(e.to_string()))?;
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: type_tag_for_path(regia_2d::tilemap::TILEMAP_TYPE_PATH),
            asset_id: id,
            importer_version,
            dependencies: vec![parent_id],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).map_err(CookError::Cooked)?;
        write_atomic(&cache_dir.join(cooked_file_name(id)), &file)?;
        manifest.push(ManifestEntry {
            path: format!("{rel_path}#{path}"),
            id,
            type_path: regia_2d::tilemap::TILEMAP_TYPE_PATH.to_string(),
        });
        entries.push(SubAssetEntry {
            path,
            id,
            type_path: regia_2d::tilemap::TILEMAP_TYPE_PATH.to_string(),
        });
    }
    write_meta_sub_assets(source, &meta, entries)?;
    let cooked = crate::tiled_import::cook_tilemap(imported.merged, dir)
        .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    let payload = cooked
        .to_postcard()
        .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    crate::import_report::ImportReports::load(cache_dir)
        .upsert_and_save(
            cache_dir,
            rel_path,
            crate::import_report::ImportReportEntry {
                importer: "ldtk".to_string(),
                data,
                warnings,
            },
        )
        .map_err(CookError::Io)?;
    info!(
        target: "regia_asset_pipeline",
        path = %rel_path,
        levels = level_count,
        "imported LDtk world"
    );
    Ok((
        "ldtk".to_string(),
        settings_hash,
        meta.id,
        regia_2d::tilemap::TILEMAP_TYPE_PATH.to_string(),
        payload,
        manifest,
    ))
}

/// `.autotile.ron` (specs/86, ADR-2597): validated and written as a [`regia_2d::CookedAutoTileSet`]
/// so a shipped game loads its rules from the pak instead of parsing RON at run time.
fn cook_autotile_ron(source: &Path, bytes: &[u8]) -> ImportedCook {
    let meta = AssetDatabase::load_or_create_meta(source, "autotile_ron").map_err(CookError::Db)?;
    let text = std::str::from_utf8(bytes).map_err(|e| CookError::Sprite2d(format!("not UTF-8: {e}")))?;
    let set = regia_2d::AutoTileSet::from_ron(text).map_err(|e| CookError::Sprite2d(e.to_string()))?;
    let payload = regia_2d::CookedAutoTileSet::new(set)
        .and_then(|c| c.to_postcard())
        .map_err(|e| CookError::Sprite2d(e.to_string()))?;
    Ok((
        "autotile_ron".to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        regia_2d::AUTOTILE_TYPE_PATH.to_string(),
        payload,
        vec![],
    ))
}

/// The platform tags a cook accepts besides the host's own (`host_platform`): the shipped
/// targets and the testing-only macOS (ADR-4898).
const COOK_PLATFORMS: &[&str] = &["linux", "windows", "android", "macos"];

/// Refuses a platform tag (already trimmed and lowercased) that is not in `COOK_PLATFORMS` and
/// is not the host's: an empty tag, a path (`..`, separators, a drive) or an unknown target would
/// otherwise cook into `.regia/cooked/<tag>` with the desktop code paths.
fn check_platform_tag(platform: &str) -> Result<(), CookError> {
    if COOK_PLATFORMS.contains(&platform) || platform == crate::host_platform() {
        return Ok(());
    }
    Err(CookError::UnsupportedPlatform(format!(
        "cook_platform_unsupported: `{}` is not a cook platform (one of {})",
        platform.escape_debug(),
        COOK_PLATFORMS.join(", ")
    )))
}

/// Cook errors.
#[derive(Debug, thiserror::Error)]
pub enum CookError {
    /// A 2D source (`.atlas.ron`, `.tilemap.ron`, `.tmx`, `.tmj`) does not import.
    #[error("2D asset error: {0}")]
    Sprite2d(String),

    /// A `.texpack.ron` channel-pack recipe does not cook (the message starts with its
    /// `texpack_*` code, `docs/formats/texpack-ron.md`; ADR-3090).
    #[error("texture pack error: {0}")]
    TexturePack(String),

    /// A `.scatter.ron` is invalid (bad version, class or field).
    #[error("scatter error: {0}")]
    Scatter(String),

    /// An `.ies` photometric profile does not parse or emits no light (ADR-2592).
    #[error("ies error: {0}")]
    Ies(String),

    /// A `.cloth.ron` or `.soft.ron` does not cook (the message starts with its error code).
    #[error("cloth error: {0}")]
    Cloth(String),

    /// A `.motiondb.ron` does not cook, or a glTF clip has a NaN or infinite value (the message
    /// starts with its code: `clip_non_finite`, `motiondb_*`; ADR-2409).
    #[error("animation error: {0}")]
    Animation(String),

    /// A curve table, chooser or smart object definition has errors (they fail the cook).
    #[error("data table error: {0}")]
    DataTable(String),

    /// A `.matgraph.ron` / `.matinst.ron` does not load, resolve or compile.
    #[error("material graph error: {0}")]
    MaterialGraph(String),

    /// Import failure.
    #[error("import error: {0}")]
    Import(ImportError),

    /// Database.
    #[error("database error: {0}")]
    Db(#[from] crate::db::DbError),

    /// Meta.
    #[error("meta error: {0}")]
    Meta(regia_asset::MetaError),

    /// Material.
    #[error("material error: {0}")]
    Material(regia_asset::MaterialError),

    /// Manifest IO.
    #[error("manifest error: {0}")]
    Manifest(regia_asset::ManifestError),

    /// Cooked wrapper.
    #[error("cooked error: {0}")]
    Cooked(regia_asset::CookedIoError),

    /// Postcard.
    #[error("postcard error: {0}")]
    Postcard(postcard::Error),

    /// Unsupported extension.
    #[error("unsupported source extension: {0}")]
    UnsupportedExtension(String),

    /// The cook's platform tag is not one the engine cooks for (ADR-4898; the message starts with
    /// `cook_platform_unsupported`). Nothing was written.
    #[error("{0}")]
    UnsupportedPlatform(String),

    /// A font's own settings are bad (ADR-2697; the message holds its `font_subset_*` code). A font file that does
    /// not read is a warning, not this.
    #[error("font cook error: {0}")]
    Font(String),

    /// A sound source does not decode (ADR-1855).
    #[error("audio import error: {0}")]
    Audio(String),

    /// A `.dds` texture is malformed or of a kind the importer refuses (ADR-1855; the message
    /// starts with its `dds_*` code).
    #[error("dds import error: {0}")]
    Dds(String),

    /// A `.fgiv` baked GI volume is damaged or of another version.
    #[error("baked GI volume error: {0}")]
    BakedGi(String),
    /// A `.cube` colour LUT does not parse.
    #[error("colour LUT error: {0}")]
    ColorLut(String),

    /// A `.pgout` bake does not decode (ADR-7214; the message carries its `pgout_*` code).
    #[error("procedural bake error: {0}")]
    Pgout(String),

    /// A `.flmap` lightmap is damaged or newer than this build (the message starts with its
    /// `lightmap_*` code).
    #[error("lightmap error: {0}")]
    Lightmap(String),

    /// Terrain cook failure.
    #[error("terrain cook error: {0}")]
    Terrain(#[from] crate::terrain_cook::TerrainCookError),

    /// Meshlet cook failure (`meshlets: true`).
    #[error("meshlet cook error: {0}")]
    Meshlets(#[from] crate::meshlet_cook::MeshletCookError),
    /// Voxel far proxy cook failure (`voxel_far: Some((...))`, ADR-2445).
    #[error("voxel far cook error: {0}")]
    VoxelFar(#[from] crate::voxel_far_cook::VoxelFarCookError),
    /// Collision hull cook failure (`collision: ConvexDecomposition`).
    #[error("collision cook error: {0}")]
    Collision(#[from] crate::collision_cook::CollisionCookError),

    /// Fracture cook failure (`fracture: Some((...))`).
    #[error("fracture cook error: {0}")]
    Fracture(#[from] crate::fracture_cook::FractureCookError),

    /// A video file or `.video.ron` does not cook (specs/50 § 4; the message starts with its
    /// protocol code, for example `container_invalid` for a truncated file, ADR-2410).
    #[error("{0}")]
    Video(String),

    /// IO.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A `.fluidcache` is damaged, truncated or of an unknown version (the message starts with
    /// its `fluidcache_*` code, ADR-2577).
    #[error("fluid cache error: {0}")]
    Fluid(String),

    /// The project's `config/import_rules.ron` does not read or parse (ADR-3321): no source
    /// cooks with settings its rules could not contribute, on every cook path (full, live and
    /// extension cooks).
    #[error("import rules error: {0}")]
    ImportRules(String),
}

/// Returns `base` if unused, else `base~2`, `base~3`, ... and records the result in `used`, so
/// clips sharing a name never overwrite each other's cooked file.
fn unique_sub_path(used: &mut BTreeSet<String>, base: String) -> String {
    let mut candidate = base.clone();
    let mut n = 2u32;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}~{n}");
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {

    /// A cook keys every source on the rules snapshot taken when it opened, but the import
    /// re-reads `config/import_rules.ron`: a source imported after the file was edited must not
    /// be recorded (nor published to a shared cache) under the snapshot's key, or reverting the
    /// edit later skips onto the output of the edited rules.
    #[test]
    fn a_source_imported_after_a_rules_edit_is_not_recorded() {
        use image::ImageEncoder as _;
        let root = regia_test_utils::unique_temp_dir("cook-rules-mid-cook");
        let ui = root.join("assets/ui");
        std::fs::create_dir_all(&ui).expect("ui dir");
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[200u8; 8 * 8 * 4], 8, 8, image::ExtendedColorType::Rgba8)
            .expect("png");
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for name in ["a.png", "b.png"] {
            std::fs::write(ui.join(name), &png).expect("png");
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(ui.join(name))
                .expect("open");
            file.set_modified(then).expect("mtime");
        }
        let rules = root.join(regia_asset::import_rules::IMPORT_RULES_FILE);
        std::fs::create_dir_all(rules.parent().expect("config")).expect("config dir");
        std::fs::write(
            &rules,
            r#"(rules: [(glob: "assets/ui/**", settings: "(compress: Some(false))")])"#,
        )
        .expect("rules");

        let cache_dir = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache_dir).expect("cache");
        let db = super::AssetDatabase::open(&root).expect("db");
        let ledger = super::CookLedger::open(&cache_dir, Some(&root), &db);
        let mut inputs = super::CookInputs::open(&root, "linux");
        let cook = |inputs: &mut super::CookInputs, name: &str| {
            let rel = format!("assets/ui/{name}");
            super::cook_one(&ui.join(name), &rel, &cache_dir, &db, &ledger, true, inputs).expect("cooks");
            inputs.has_record(&rel)
        };
        assert!(
            cook(&mut inputs, "a.png"),
            "control: an unedited cook is recorded"
        );
        std::fs::write(
            &rules,
            r#"(rules: [(glob: "assets/ui/**", settings: "(compress: Some(true))")])"#,
        )
        .expect("edit rules");
        assert!(
            !cook(&mut inputs, "b.png"),
            "b.png cooked with the edited rules under the old rules' key"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An import rule that matches an image an FBX/OBJ conversion references reaches the cook
    /// (`import_rules::referenced_settings`, ADR-3321), so the converted key must hash it:
    /// editing the rule must recook the converted source, not skip onto the stale output
    /// (rev-corr-assets, sprint-corr-asset-pipeline-a 2).
    #[test]
    fn a_rule_edit_recooks_a_converted_source_referencing_the_image() {
        let root = regia_test_utils::unique_temp_dir("cook-converted-rule-recook");
        let model = root.join("assets/model");
        std::fs::create_dir_all(&model).expect("model dir");
        std::fs::write(
            model.join("a.obj"),
            "mtllib a.mtl\no tri\nv 0 0 0\nv 1 0 0\nv 0 1 0\nusemtl red\nf 1 2 3\n",
        )
        .expect("obj");
        std::fs::write(model.join("a.mtl"), "newmtl red\nKd 1 0 0\nmap_Kd tex.png\n").expect("mtl");
        use image::ImageEncoder as _;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[200u8; 8 * 8 * 4], 8, 8, image::ExtendedColorType::Rgba8)
            .expect("png");
        std::fs::write(model.join("tex.png"), &png).expect("png");
        let rules = root.join(regia_asset::import_rules::IMPORT_RULES_FILE);
        std::fs::create_dir_all(rules.parent().expect("config")).expect("config dir");
        let write_rules = |text: &str| std::fs::write(&rules, text).expect("rules");
        write_rules(
            r#"(rules: [(glob: "assets/model/**", importer: "png", settings: "(compress: Some(false))")])"#,
        );

        let cache_dir = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache_dir).expect("cache");
        let rel = "assets/model/a.obj";
        let cook = || {
            let mut db = super::AssetDatabase::open(&root).expect("db");
            let mut ledger = super::CookLedger::open(&cache_dir, Some(&root), &db);
            let mut inputs = super::CookInputs::open(&root, "linux");
            let outcome = super::cook_one(
                &model.join("a.obj"),
                rel,
                &cache_dir,
                &db,
                &ledger,
                true,
                &mut inputs,
            )
            .expect("cooks");
            // The project cook records the key through record_outcome (the ledger drops keys
            // whose source has no database row) and saves both.
            let mut manifest_map = std::collections::BTreeMap::new();
            let skipped = outcome.skipped;
            super::record_outcome(&mut db, &mut ledger, &mut manifest_map, rel, outcome);
            db.save(&root).expect("db saved");
            ledger.save().expect("ledger saved");
            skipped
        };
        assert!(!cook(), "the first cook imports");
        assert!(cook(), "an unedited second cook skips on the ledger");
        write_rules(
            r#"(rules: [(glob: "assets/model/**", importer: "png", settings: "(compress: Some(true))")])"#,
        );
        assert!(
            !cook(),
            "editing the rule the referenced image matches must recook the converted source"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Changed default LOD thresholds invalidate the glTF cooker and every converter that
    /// feeds it, including shared-cache hits; this is not a payload-format revision.
    #[test]
    fn lod_default_change_invalidates_importer_revisions() {
        for (ext, previous) in [
            ("gltf", (1, 13, 0)),
            ("glb", (1, 13, 0)),
            ("fbx", (1, 2, 0)),
            ("obj", (1, 2, 0)),
            ("ply", (1, 2, 0)),
            ("stl", (1, 2, 0)),
            ("usd", (1, 0, 0)),
            ("usda", (1, 0, 0)),
            ("usdc", (1, 0, 0)),
            ("usdz", (1, 0, 0)),
        ] {
            assert!(
                super::importer_version_for(std::path::Path::new(&format!("model.{ext}")), ext)
                    > regia_asset::pack_importer_version(previous.0, previous.1, previous.2),
                "{ext} must invalidate pre-ADR-7869 cached LOD thresholds"
            );
        }
    }

    /// The FBX and OBJ converters share the glTF mesh cook, whose generated LODs changed
    /// (normal and UV0 weights): their importer version must move past 1.1.0 or a cached
    /// primary mesh keeps the old LODs.
    #[test]
    fn converted_importers_recook_after_the_attribute_lod_change() {
        let floor = regia_asset::pack_importer_version(1, 2, 0);
        for (name, ext) in [("a.fbx", "fbx"), ("a.obj", "obj")] {
            assert!(
                super::importer_version_for(std::path::Path::new(name), ext) >= floor,
                "{ext} importer version"
            );
        }
    }

    /// Files that bake Convert at scale 1, with no generated LODs, keep the historical
    /// `forward=convert` key. Keep, a non-1 scale, or `generate_lods` add a term.
    #[test]
    fn gltf_axis_and_attribute_lods_change_only_their_cache_key() {
        let historical = *blake3::hash(b"forward=convert").as_bytes();
        assert_eq!(
            super::gltf_settings_hash(None, None, None, None, false),
            historical
        );
        assert_eq!(
            super::gltf_settings_hash(None, None, None, Some((0, 1.0)), false),
            historical,
            "Convert at scale 1 is the historical key"
        );
        let keep = super::gltf_settings_hash(None, None, None, Some((1, 1.0)), false);
        let scaled = super::gltf_settings_hash(None, None, None, Some((0, 0.01)), false);
        let lods = super::gltf_settings_hash(None, None, None, None, true);
        assert_ne!(keep, historical);
        assert_ne!(scaled, historical);
        assert_ne!(lods, historical);
        assert_ne!(keep, scaled);
        let inputs = [7u8; 32];
        let with_inputs = super::gltf_settings_hash(None, None, Some(inputs), None, false);
        assert_ne!(with_inputs, historical);
        assert_eq!(
            super::gltf_settings_hash(None, None, Some(inputs), Some((0, 1.0)), false),
            with_inputs,
            "a default axis does not recook a file that already hashes its settings string"
        );
        assert_ne!(
            super::gltf_settings_hash(None, None, Some(inputs), Some((1, 1.0)), false),
            with_inputs
        );
        assert_ne!(
            super::gltf_settings_hash(None, None, Some(inputs), None, true),
            with_inputs
        );
    }

    /// A layer mask or normal is a texture the material samples. The pak dependency list has to
    /// name those asset ids or the chunk ships the material without its overlay maps.
    #[test]
    fn audfix_material_layer_asset_slots_are_pak_dependencies() {
        use std::str::FromStr;
        let mask = AssetId::from_str("67e55044-10b1-426f-9247-bb680e5fe0c8").expect("guid");
        let normal = AssetId::from_str("67e55044-10b1-426f-9247-bb680e5fe0c9").expect("guid");
        let mut mat = MaterialAssetRon::default_standard();
        mat.layers.push(regia_asset::MaterialLayer {
            name: "dust".into(),
            mask: regia_asset::TextureSlot::Asset(mask),
            base_color: [1.0, 1.0, 1.0, 1.0],
            roughness: 1.0,
            metallic: 0.0,
            triplanar: false,
            normal_map: Some(regia_asset::TextureSlot::Asset(normal)),
        });
        let deps = super::material_texture_deps(&mat);
        assert!(deps.contains(&mask), "{deps:?}");
        assert!(deps.contains(&normal), "{deps:?}");
    }

    /// A Windows-authored `sprites\\hero.png` is keyed by the file the packer reads, so editing the
    /// image changes the key (audit F1: it hashed the literal backslash name, a constant on Linux).
    #[test]
    fn atlas_key_hashes_the_file_a_backslash_path_names() {
        let root = regia_test_utils::unique_temp_dir("cook-atlas-backslash");
        std::fs::create_dir_all(root.join("sprites")).expect("dir");
        let png = |v: u8| {
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([v, 0, 0, 255]));
            img.save(root.join("sprites/hero.png")).expect("png");
        };
        let src = root.join("hero.atlas.ron");
        let text = r#"(version: 1, padding: 0, sprites: [(name: "hero", image: "sprites\\hero.png")])"#;
        std::fs::write(&src, text).expect("atlas");
        png(10);
        let key = || super::cook_atlas_ron(&src, text.as_bytes()).expect("cooks").1;
        let before = key();
        png(200);
        assert_ne!(before, key(), "editing the image must change the key");
    }

    /// A tampered asset database cannot make the cook delete files outside the cache (ADR-1816).
    #[test]
    fn remove_cached_deletes_only_inside_the_cache() {
        let base = regia_test_utils::unique_temp_dir("cook-remove-cached");
        let (cache, outside) = (base.join("cache"), base.join("outside"));
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(cache.join("a.fckd"), "x").expect("cooked");
        std::fs::write(outside.join("keep.txt"), "x").expect("outside file");
        let abs = outside.join("keep.txt").display().to_string();
        for name in [abs.as_str(), "../outside/keep.txt", "C:/keep.txt"] {
            super::remove_cached(&cache, name);
        }
        assert!(
            outside.join("keep.txt").is_file(),
            "nothing outside the cache is deleted"
        );
        super::remove_cached(&cache, "a.fckd");
        assert!(!cache.join("a.fckd").exists(), "a cache file is deleted");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Repeated clip names get distinct sub-asset paths instead of overwriting one file.
    #[test]
    fn unique_sub_path_dedups_repeated_names() {
        let mut used = std::collections::BTreeSet::new();
        assert_eq!(super::unique_sub_path(&mut used, "clip/Idle".into()), "clip/Idle");
        assert_eq!(
            super::unique_sub_path(&mut used, "clip/Idle".into()),
            "clip/Idle~2"
        );
        assert_eq!(
            super::unique_sub_path(&mut used, "clip/Idle".into()),
            "clip/Idle~3"
        );
        assert_eq!(super::unique_sub_path(&mut used, "clip/Run".into()), "clip/Run");
    }

    /// A v1 `.scatter.ron` cooks to a validated v2 cooked set; a broken one is a cook error.
    #[test]
    fn scatter_ron_cooks_to_a_v2_cooked_set() {
        let dir = std::env::temp_dir().join(format!("regia-cook-scatter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("rocks.scatter.ron");
        let v1 = regia_terrain::ScatterRule::default().to_ron().expect("v1 text");
        std::fs::write(&file, &v1).expect("write");
        let (importer, _, _, type_path, payload, _) = cook_scatter_ron(&file, v1.as_bytes()).expect("cook");
        assert_eq!(importer, "scatter_ron");
        assert_eq!(type_path, regia_terrain::SCATTER_TYPE_PATH);
        let set = regia_terrain::ScatterSet::from_cooked(&payload, None).expect("cooked set");
        assert_eq!(set.version, regia_terrain::SCATTER_SET_VERSION);
        assert_eq!(set.classes.len(), 1);
        let broken = dir.join("broken.scatter.ron");
        let text = "ScatterSet(version: 2, seed: 1, classes: [ScatterClass(name: \"x\", meshes: [])])";
        std::fs::write(&broken, text).expect("write");
        assert!(matches!(
            cook_scatter_ron(&broken, text.as_bytes()),
            Err(CookError::Scatter(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
    use super::*;

    /// A project's `.fgiv` cooks to a manifest entry whose payload is the file's own bytes, and a
    /// damaged one is a cook error that names the file.
    #[test]
    fn fgiv_cooks_unchanged_and_a_damaged_one_is_an_error() {
        use regia_asset::baked_gi::{
            BakeInfo, BakeLights, BakedGiVolume, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta,
            TerrainLayer, VolumeLayout,
        };
        let layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [2, 2],
                levels: vec![1.0],
                ground: vec![0.0; 4],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [[[0.5, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let volume = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let bytes = volume.to_bytes().expect("encode");

        let root = regia_test_utils::unique_temp_dir("cook-fgiv");
        let maps = root.join("assets").join("maps");
        std::fs::create_dir_all(&maps).expect("maps dir");
        std::fs::write(maps.join("a.fgiv"), &bytes).expect("write");
        std::fs::write(maps.join("bad.fgiv"), b"not a volume").expect("write bad");
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: false,
        })
        .expect("cook");
        assert_eq!(report.cooked, 1, "{report:?}");
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].0.ends_with("maps/bad.fgiv"),
            "{:?}",
            report.errors
        );

        let cache = cooked_cache_dir(&root, "linux");
        let manifest = AssetManifest::read_from_cache(&cache)
            .expect("manifest")
            .expect("present");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == "assets/maps/a.fgiv")
            .expect("entry");
        assert_eq!(entry.type_path, BAKED_GI_TYPE_PATH);
        let cooked = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
        let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("decode");
        assert_eq!(payload, bytes.as_slice(), "the payload is the .fgiv unchanged");
        let back = BakedGiVolume::from_bytes(payload).expect("loads");
        assert_eq!(back.probe_count(), 4);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A project's `.cube` colour LUT cooks to a manifest entry whose payload is the file's own
    /// text (a level look or a post volume loads it through the asset server, ADR-4830), and a
    /// malformed one is a cook error that names the file.
    #[test]
    fn cube_lut_cooks_unchanged_and_a_malformed_one_is_an_error() {
        let text = format!("LUT_3D_SIZE 2\n{}", "0 0 0\n".repeat(8));
        let root = regia_test_utils::unique_temp_dir("cook-cube-lut");
        let looks = root.join("assets").join("looks");
        std::fs::create_dir_all(&looks).expect("looks dir");
        std::fs::write(looks.join("hall.cube"), &text).expect("write");
        std::fs::write(looks.join("bad.cube"), "LUT_3D_SIZE nope\n").expect("write bad");
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: false,
        })
        .expect("cook");
        assert_eq!(report.cooked, 1, "{report:?}");
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].0.ends_with("looks/bad.cube"),
            "{:?}",
            report.errors
        );

        let cache = cooked_cache_dir(&root, "linux");
        let manifest = AssetManifest::read_from_cache(&cache)
            .expect("manifest")
            .expect("present");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == "assets/looks/hall.cube")
            .expect("entry");
        assert_eq!(entry.type_path, COLOR_LUT_TYPE_PATH);
        let cooked = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
        let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("decode");
        assert_eq!(payload, text.as_bytes(), "the payload is the .cube unchanged");
        assert_eq!(
            regia_render::lut::ColorLut::parse_cube(std::str::from_utf8(payload).expect("utf-8"))
                .expect("parses")
                .size(),
            2
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The cook is the gate for the runtime's 16 MiB cap: an over-cap `.cube` is a cook error
    /// that names the file, not a cooked asset whose every runtime load then fails.
    #[test]
    fn an_over_cap_cube_lut_is_a_cook_error() {
        let mut text = format!("LUT_3D_SIZE 2\n{}", "0 0 0\n".repeat(8));
        let pad = regia_render::lut::ColorLut::MAX_FILE_BYTES as usize - text.len(); // '#' + pad puts it over
        text.push('#');
        text.extend(std::iter::repeat_n('x', pad));
        assert!(text.len() as u64 > regia_render::lut::ColorLut::MAX_FILE_BYTES);
        let root = regia_test_utils::unique_temp_dir("cook-cube-lut-over-cap");
        let looks = root.join("assets").join("looks");
        std::fs::create_dir_all(&looks).expect("looks dir");
        std::fs::write(looks.join("big.cube"), &text).expect("write");
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: false,
        })
        .expect("cook");
        assert_eq!(report.cooked, 0, "{report:?}");
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].0.ends_with("looks/big.cube"),
            "{:?}",
            report.errors
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A project's `.flmap` cooks to a manifest entry whose payload is the lightmap at the current
    /// version (a v1 file comes out migrated, its texels unchanged), and a damaged one is a cook
    /// error that names the file and the reader's code.
    #[test]
    fn flmap_cooks_at_the_current_version_and_a_damaged_one_is_an_error() {
        use regia_asset::lightmap::{LIGHTMAP_FORMAT_VERSION, Lightmap, lightmap_version};
        let v1: &[u8] = include_bytes!("../../regia_asset/tests/fixtures/lightmap-v1.flmap");
        let root = regia_test_utils::unique_temp_dir("cook-flmap");
        let maps = root.join("assets").join("maps");
        std::fs::create_dir_all(&maps).expect("maps dir");
        std::fs::write(maps.join("a.flmap"), v1).expect("write");
        std::fs::write(maps.join("bad.flmap"), b"FLMP not a lightmap").expect("write bad");
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: false,
        })
        .expect("cook");
        assert_eq!(report.cooked, 1, "{report:?}");
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].0.ends_with("maps/bad.flmap"),
            "{:?}",
            report.errors
        );
        assert!(report.errors[0].1.contains("lightmap_"), "{:?}", report.errors);

        let cache = cooked_cache_dir(&root, "linux");
        let manifest = AssetManifest::read_from_cache(&cache)
            .expect("manifest")
            .expect("present");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == "assets/maps/a.flmap")
            .expect("entry");
        assert_eq!(entry.type_path, LIGHTMAP_TYPE_PATH);
        let cooked = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
        let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("decode");
        assert_eq!(
            lightmap_version(payload).expect("version"),
            LIGHTMAP_FORMAT_VERSION
        );
        assert_eq!(
            Lightmap::from_bytes(payload).expect("loads"),
            Lightmap::from_bytes(v1).expect("v1 loads"),
            "the cooked lightmap is the source's content"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A lightmap whose `.meta` asks for `auto` cooks to BC6H blocks on desktop and ASTC HDR on
    /// Android (ADR-2314); without settings the texels are kept; bad settings are a cook error.
    #[test]
    fn flmap_encoding_settings_pick_blocks_per_platform() {
        use regia_asset::lightmap::{Lightmap, LightmapTexelEncoding};
        let v1: &[u8] = include_bytes!("../../regia_asset/tests/fixtures/lightmap-v1.flmap");
        let cooked_map = |settings: Option<&str>, platform: &str| -> Result<Lightmap, String> {
            let root = regia_test_utils::unique_temp_dir("cook-flmap-encoding");
            let maps = root.join("assets").join("maps");
            std::fs::create_dir_all(&maps).expect("maps dir");
            let source = maps.join("a.flmap");
            std::fs::write(&source, v1).expect("write");
            let mut meta = regia_asset::MetaSidecar::new("flmap", regia_asset::AssetId::new_random());
            meta.settings = settings.map(str::to_string);
            std::fs::write(
                regia_asset::meta_path_for(&source),
                meta.to_ron_string().expect("ron"),
            )
            .expect("meta");
            let report = cook_project(CookOptions {
                project_root: root.clone(),
                platform: platform.to_string(),
                incremental: false,
            })
            .expect("cook");
            let out = if let Some((_, e)) = report.errors.first() {
                Err(e.clone())
            } else {
                let cache = cooked_cache_dir(&root, platform);
                let manifest = AssetManifest::read_from_cache(&cache)
                    .expect("manifest")
                    .expect("present");
                let entry = manifest
                    .entries
                    .iter()
                    .find(|e| e.path == "assets/maps/a.flmap")
                    .expect("entry");
                let cooked = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
                let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("decode");
                Ok(Lightmap::from_bytes(payload).expect("loads"))
            };
            std::fs::remove_dir_all(&root).ok();
            out
        };
        let source = Lightmap::from_bytes(v1).expect("v1");
        let plain = cooked_map(None, "linux").expect("plain");
        assert_eq!(plain.texel_encoding(), LightmapTexelEncoding::Rgb9e5);
        assert_eq!(plain.texels, source.texels);
        let desktop = cooked_map(Some("(encoding: auto)"), "linux").expect("desktop");
        assert_eq!(desktop.texel_encoding(), LightmapTexelEncoding::Bc6h);
        let phone = cooked_map(Some("(encoding: auto)"), "android").expect("android");
        assert_eq!(phone.texel_encoding(), LightmapTexelEncoding::AstcHdr);
        assert_eq!(phone.entries, source.entries);
        let e = cooked_map(Some("(encoding: dxt1)"), "linux").expect_err("bad settings");
        assert!(e.contains("lightmap_cook_settings"), "{e}");
    }

    /// Clusters baked into a lightmap (ADR-2107) survive the cook: a package has them.
    #[test]
    fn flmap_clusters_survive_the_cook() {
        use regia_asset::lightmap::Lightmap;
        let v1: &[u8] = include_bytes!("../../regia_asset/tests/fixtures/lightmap-v1.flmap");
        let mut map = Lightmap::from_bytes(v1).expect("v1 loads");
        map.meshlets = vec![b"first".to_vec(), b"second".to_vec()];
        let root = regia_test_utils::unique_temp_dir("cook-flmap-clusters");
        let maps = root.join("assets").join("maps");
        std::fs::create_dir_all(&maps).expect("maps dir");
        map.write(&maps.join("a.flmap")).expect("write");
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: false,
        })
        .expect("cook");
        assert_eq!((report.cooked, report.errors.len()), (1, 0), "{report:?}");
        let cache = cooked_cache_dir(&root, "linux");
        let manifest = AssetManifest::read_from_cache(&cache)
            .expect("manifest")
            .expect("present");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == "assets/maps/a.flmap")
            .expect("entry");
        let cooked = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
        let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("decode");
        assert_eq!(Lightmap::from_bytes(payload).expect("loads"), map);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gltf_sidecars_are_the_buffers_and_images_it_references() {
        let dir = std::env::temp_dir().join(format!("regia-cook-sidecars-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let gltf = r#"{"asset":{"version":"2.0"},
            "buffers":[{"uri":"scene.bin","byteLength":4}],
            "images":[{"uri":"tex%20a.jpg"},{"uri":"data:image/png;base64,AAAA"}]}"#;
        std::fs::write(dir.join("scene.gltf"), gltf).expect("write");
        let sources = vec![
            dir.join("scene.gltf"),
            dir.join("scene.bin"),
            dir.join("tex a.jpg"),
            dir.join("standalone.png"),
        ];
        let sidecars = gltf_sidecars(&sources);
        assert!(sidecars.contains(&dir.join("scene.bin")));
        assert!(sidecars.contains(&dir.join("tex a.jpg")));
        assert!(!sidecars.contains(&dir.join("standalone.png")));
        assert!(!sidecars.contains(&dir.join("scene.gltf")));
        assert_eq!(sidecars.len(), 2, "data: URIs are not files");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_part_suffixes_match_ignoring_case() {
        assert!(name_ends_with("Level.Terrain.Layers.ron", "terrain.layers.ron"));
        assert!(name_ends_with("Hero.MATERIAL.RON", ".material.ron"));
        assert!(name_ends_with("a.matfunc.ron", ".matfunc.ron"));
        assert!(!name_ends_with("ron", ".material.ron"));
        assert!(!name_ends_with("a.material.png", ".material.ron"));
        assert!(is_terrain_layer(Path::new("x/Level.Terrain.Layers.ron")));
        assert!(crate::matgraph_cook::is_material_function(Path::new(
            "F.MatFunc.Ron"
        )));
    }

    #[test]
    fn join_portable_normalizes_separators_and_rejects_absolute_paths() {
        let base = Path::new("maps");
        assert_eq!(
            join_portable(base, r"sets\terrain.tsx"),
            Ok(base.join("sets/terrain.tsx"))
        );
        assert_eq!(join_portable(base, "a/b.png"), Ok(base.join("a/b.png")));
        assert!(join_portable(base, "C:/sets/t.tsx").is_err());
        assert!(join_portable(base, r"c:\sets\t.tsx").is_err());
        assert!(join_portable(base, "/etc/passwd").is_err());
        assert!(join_portable(base, r"\\server\share").is_err());

        // Pathhelper audit M1: inside a project `..` reaches siblings, never past the root; a real
        // folder, a missing one and a symlink cannot change that.
        let top = regia_test_utils::unique_temp_dir("cook_join_portable");
        let project = top.join("project");
        let models = project.join("assets/models");
        std::fs::create_dir_all(&models).expect("dirs");
        std::fs::create_dir_all(project.join("assets/textures")).expect("dirs");
        std::fs::write(project.join(regia_core::brand::MANIFEST_FILE), "").expect("manifest");
        std::fs::write(top.join("secret.png"), "x").expect("secret");
        assert!(join_portable(&models, "../textures/a.png").is_ok());
        assert!(join_portable(&models, "../../../secret.png").is_err());
        assert!(join_portable(&models, "missing/../../../../secret.png").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&top, models.join("out")).expect("link");
            assert!(join_portable(&models, "out/secret.png").is_err());
        }
        // A kit linked into the project lives elsewhere: its own files, not what lies around it.
        #[cfg(unix)]
        {
            let kit = top.join("kit");
            std::fs::create_dir_all(kit.join("textures")).expect("dirs");
            std::fs::create_dir_all(kit.join("models")).expect("dirs");
            std::os::unix::fs::symlink(&kit, project.join("assets/kit")).expect("link");
            let linked = project.join("assets/kit/models");
            assert!(join_portable(&linked, "../textures/a.png").is_ok());
            assert!(join_portable(&project.join("assets/kit"), "../../../secret.png").is_err());
            assert!(join_portable(&project.join("assets/kit"), "../secret.png").is_err());
            assert!(join_portable(&linked, "../../secret.png").is_err());
            // A link inside the kit: the deepest link bounds, so its own files stay reachable.
            let shared = top.join("shared");
            std::fs::create_dir_all(shared.join("sub")).expect("dirs");
            std::os::unix::fs::symlink(&shared, kit.join("shared")).expect("link");
            let nested = project.join("assets/kit/shared/sub");
            assert!(join_portable(&nested, "../a.png").is_ok());
            assert!(join_portable(&nested, "../../secret.png").is_err());
        }
        // No project above: nothing to confine to, and the absolute rule still holds.
        let loose = top.join("loose/sub");
        std::fs::create_dir_all(&loose).expect("dirs");
        assert!(join_portable(&loose, "../a.png").is_ok());
        assert!(join_portable(&loose, "/etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&top);
        // Content paths joined onto the project root stay inside it (ADR-1816).
        let root = regia_test_utils::unique_temp_dir("cook-join-in-project");
        assert_eq!(
            join_in_project(&root, "mat/a.matgraph.ron"),
            Ok(root.join("mat/a.matgraph.ron"))
        );
        for bad in ["../x.ron", "/etc/passwd", "C:/x.ron", r"\\server\share\x"] {
            assert!(join_in_project(&root, bad).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit concurrency-1103 pipeline M2: the glTF cook writes its sub-asset list into the
    /// `.meta` as it is on disk now, so an importer-settings edit saved during the cook is kept
    /// (it was overwritten by the copy read when the cook started).
    #[test]
    fn the_sub_asset_write_keeps_a_settings_edit_made_during_the_cook() {
        let root = regia_test_utils::unique_temp_dir("cook-meta-merge");
        let source = root.join("hero.glb");
        let used = MetaSidecar::new("gltf", AssetId::new_random());
        let mut edited = used.clone();
        edited.settings = Some("(scale: 2.0)".into());
        std::fs::write(meta_path_for(&source), edited.to_ron_string().expect("ron")).expect("meta");
        let subs = vec![SubAssetEntry {
            path: "mesh0".into(),
            id: AssetId::new_random(),
            type_path: "regia_render::Mesh".into(),
        }];
        write_meta_sub_assets(&source, &used, subs.clone()).expect("write");
        let on_disk =
            MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("read")).expect("parse");
        assert_eq!(
            on_disk.settings.as_deref(),
            Some("(scale: 2.0)"),
            "the edit survives"
        );
        assert_eq!(on_disk.sub_assets, subs);
        assert_eq!(on_disk.id, used.id);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit concurrency-1103 asset H3 / pipeline H2: a cook holds the project's cook lock
    /// (`.regia/cook.lock`) from opening the database to its final saves, so a second cook (the
    /// live cooker, `regia cook`, another process) waits instead of saving a stale snapshot over
    /// the first one's rows. The test holds the lock itself and checks the cook does not finish
    /// until it is released (the bound only decides how long a missing lock gets to show itself).
    #[test]
    fn cook_waits_for_the_project_cook_lock() {
        let root = regia_test_utils::unique_temp_dir("cook-lock-wait");
        std::fs::create_dir_all(root.join("assets")).expect("assets dir");
        let state = regia_core::brand::project_state_dir(&root);
        std::fs::create_dir_all(&state).expect("state dir");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("cook.lock"))
            .expect("lock file");
        lock.lock().expect("take the cook lock");
        for which in ["cook_project", "cook_sources"] {
            let (tx, rx) = std::sync::mpsc::channel();
            let project = root.clone();
            let worker = std::thread::spawn(move || {
                let options = CookOptions {
                    project_root: project.clone(),
                    platform: "linux".to_string(),
                    incremental: true,
                };
                let ok = if which == "cook_project" {
                    cook_project(options).is_ok()
                } else {
                    crate::live::cook_sources(&options, &BTreeSet::new()).is_ok()
                };
                let _ = tx.send(ok);
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_secs(3)).is_err(),
                "{which} finished while another cook held the project's cook lock"
            );
            lock.unlock().expect("release the cook lock");
            assert!(
                rx.recv().expect("cook result"),
                "{which} failed after the lock was released"
            );
            worker.join().expect("cook thread");
            lock.lock().expect("retake the cook lock");
        }
        lock.unlock().expect("release");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// GLB: one triangle (its second corner at `x = 1 + shift`), a two-joint skin whose bone sits
    /// at `y = shift`, and two clips on the bone: `a` (keys move with `shift`) and `b`, whose
    /// second key is NaN when `nan_in_b`.
    fn two_clip_skin_glb(shift: f32, nan_in_b: bool) -> Vec<u8> {
        let f = |values: &[f32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let mut bin = Vec::new();
        bin.extend(f(&[0.0, 0.0, 0.0, 1.0 + shift, 0.0, 0.0, 0.0, 1.0, 0.0])); // 0..36
        bin.extend(f(&[0.0, 1.0])); // 36..44
        bin.extend(f(&[0.0, shift, 0.0, 0.0, 2.0 + shift, 0.0])); // 44..68
        let b_end = if nan_in_b { f32::NAN } else { 3.0 };
        bin.extend(f(&[0.0, 1.0, 0.0, 0.0, b_end, 0.0])); // 68..92
        for i in [0u32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes()); // 92..104
        }
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0,2]}}],
            "nodes":[{{"name":"root","children":[1]}},{{"name":"bone","translation":[0.0,{shift},0.0]}},
                {{"name":"body","mesh":0}}],
            "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":4}}]}}],
            "skins":[{{"joints":[0,1]}}],
            "animations":[
                {{"name":"a","channels":[{{"sampler":0,"target":{{"node":1,"path":"translation"}}}}],
                  "samplers":[{{"input":1,"output":2,"interpolation":"LINEAR"}}]}},
                {{"name":"b","channels":[{{"sampler":0,"target":{{"node":1,"path":"translation"}}}}],
                  "samplers":[{{"input":1,"output":3,"interpolation":"LINEAR"}}]}}],
            "accessors":[
                {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[{max_x},1.0,0.0]}},
                {{"bufferView":1,"componentType":5126,"count":2,"type":"SCALAR","min":[0.0],"max":[1.0]}},
                {{"bufferView":2,"componentType":5126,"count":2,"type":"VEC3"}},
                {{"bufferView":3,"componentType":5126,"count":2,"type":"VEC3"}},
                {{"bufferView":4,"componentType":5125,"count":3,"type":"SCALAR"}}],
            "bufferViews":[
                {{"buffer":0,"byteOffset":0,"byteLength":36}},
                {{"buffer":0,"byteOffset":36,"byteLength":8}},
                {{"buffer":0,"byteOffset":44,"byteLength":24}},
                {{"buffer":0,"byteOffset":68,"byteLength":24}},
                {{"buffer":0,"byteOffset":92,"byteLength":12}}],
            "buffers":[{{"byteLength":104}}]}}"#,
            max_x = 1.0 + shift,
        );
        let mut json = json.into_bytes();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    /// Every file under `dir` with its bytes.
    fn cache_snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(dir)
            .expect("cache dir")
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (name, std::fs::read(e.path()).expect("cache file"))
            })
            .collect()
    }

    /// Audit asset_pipeline1 #4: a recook refused on its second clip (a NaN key) wrote the new
    /// mesh sub-assets, skeleton and first clip over the files the previous cook's database row
    /// still names. The refused cook writes nothing now.
    #[test]
    fn a_gltf_recook_refused_on_a_clip_leaves_the_previous_cook_files() {
        let root = regia_test_utils::unique_temp_dir("cook-gltf-refused-recook");
        let cache = root.join(".regia/cooked/linux");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::create_dir_all(&cache).expect("cache");
        let source = root.join("assets/hero.glb");
        let cook = || cook_gltf_once(&source, "hero.glb", &root, &cache, 1, "linux");

        std::fs::write(&source, two_clip_skin_glb(0.0, false)).expect("glb");
        let (_, _, _, _, _, manifest) = cook().expect("first cook");
        for wanted in ["#skeleton/root", "#clip/a", "#clip/b"] {
            assert!(
                manifest.iter().any(|m| m.path.ends_with(wanted)),
                "{wanted} in {:?}",
                manifest.iter().map(|m| &m.path).collect::<Vec<_>>()
            );
        }
        let before = cache_snapshot(&cache);
        let meta_before = std::fs::read(meta_path_for(&source)).expect("meta");

        // The same asset changes (mesh, bone, clip a) and its clip b is no longer finite.
        std::fs::write(&source, two_clip_skin_glb(0.5, true)).expect("glb");
        let error = cook().expect_err("a NaN clip key refuses the cook");
        assert!(matches!(error, CookError::Animation(_)), "{error}");
        let after = cache_snapshot(&cache);
        let changed: Vec<&String> = after
            .iter()
            .filter(|(name, bytes)| before.get(*name) != Some(*bytes))
            .map(|(name, _)| name)
            .collect();
        assert!(changed.is_empty(), "the refused cook wrote {changed:?}");
        assert_eq!(std::fs::read(meta_path_for(&source)).expect("meta"), meta_before);

        // Fixed again, the recook writes the new bytes.
        std::fs::write(&source, two_clip_skin_glb(0.5, false)).expect("glb");
        cook().expect("fixed cook");
        assert_ne!(cache_snapshot(&cache), before, "the fixed cook rewrote its files");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7904 (P-151): the `.meta` `clips` options reach the clip cook. On the base the cook
    /// passed the default (empty) options (cook.rs `GltfAnimSettings { forward, scale, .. }`),
    /// so a split take still cooked as one clip and a skipped one cooked anyway.
    #[test]
    fn meta_clip_options_split_skip_and_rename_the_cooked_clips() {
        let root = regia_test_utils::unique_temp_dir("cook-gltf-clip-options");
        let cache = root.join(".regia/cooked/linux");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::create_dir_all(&cache).expect("cache");
        let source = root.join("assets/hero.glb");
        std::fs::write(&source, two_clip_skin_glb(0.0, false)).expect("glb");
        let mut meta = MetaSidecar::new("gltf", AssetId::new_random());
        meta.settings = Some(
            "(clips: [(animation: \"a\", ranges: [(name: \"rise\", start: 0.0, end: 0.5), \
             (name: \"top\", start: 0.5, end: 1.0)]), (animation: \"b\", skip: true)])"
                .into(),
        );
        std::fs::write(meta_path_for(&source), meta.to_ron_string().expect("ron")).expect("meta");
        let (_, _, _, _, _, manifest) =
            cook_gltf_once(&source, "hero.glb", &root, &cache, 1, "linux").expect("cook");
        let paths: Vec<&String> = manifest.iter().map(|m| &m.path).collect();
        for wanted in ["#clip/rise", "#clip/top"] {
            assert!(paths.iter().any(|p| p.ends_with(wanted)), "{wanted} in {paths:?}");
        }
        for unwanted in ["#clip/a", "#clip/b"] {
            assert!(
                !paths.iter().any(|p| p.ends_with(unwanted)),
                "{unwanted} in {paths:?}"
            );
        }

        // A rename keeps one clip under the new name.
        meta.settings = Some("(clips: [(animation: \"b\", name: Some(\"jump\"))])".into());
        std::fs::write(meta_path_for(&source), meta.to_ron_string().expect("ron")).expect("meta");
        let (_, _, _, _, _, manifest) =
            cook_gltf_once(&source, "hero.glb", &root, &cache, 1, "linux").expect("recook");
        let paths: Vec<&String> = manifest.iter().map(|m| &m.path).collect();
        for wanted in ["#clip/a", "#clip/jump"] {
            assert!(paths.iter().any(|p| p.ends_with(wanted)), "{wanted} in {paths:?}");
        }
        assert!(!paths.iter().any(|p| p.ends_with("#clip/b")), "{paths:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Full type path of saved editor review markers (ADR-7841). Kept as a string so the
/// cooker never needs to depend on the desktop editor.
pub const REVIEW_NOTE_TYPE: &str = "regia_editor::review_notes::ReviewNote";

/// Removes review markers and their subtrees, including prefab-added children, before
/// streaming or packaging. The authored level remains unchanged.
#[must_use]
pub fn strip_review_notes(scene: &regia_scene::format::SceneFile) -> regia_scene::format::SceneFile {
    regia_scene::layers::strip_component_holders(scene, REVIEW_NOTE_TYPE).0
}

#[cfg(test)]
mod review_note_tests {
    use super::*;
    use regia_core::id::Guid;
    use regia_scene::format::{SceneEntity, SceneFile};

    #[test]
    fn cook_review_notes_strips_text_and_keeps_attached_content() {
        let content = SceneEntity {
            guid: Guid::from_u128(1),
            name: Some("Shipping object".into()),
            ..Default::default()
        };
        let marker = SceneEntity { guid: Guid::from_u128(2), parent: Some(content.guid), components: std::collections::BTreeMap::from([
            (REVIEW_NOTE_TYPE.to_owned(), ron::value::RawValue::from_boxed_ron("(text: \"private review\", author: \"Private author\", created: 1, resolved: false, thread: [])".into()).expect("note"))
        ]), ..Default::default() };
        let mut source = SceneFile::new();
        source.entities = vec![content, marker];
        let clean = strip_review_notes(&source);
        assert_eq!(clean.entities.len(), 1);
        assert_eq!(source.entities.len(), 2, "authoring source is untouched");
        let (cooked, _) =
            regia_scene::cooked::CookedScene::cook(&clean, &regia_ecs::reflect::TypeRegistry::default())
                .expect("cook");
        let bytes = cooked.to_bytes().expect("bytes");
        for secret in ["ReviewNote", "private review", "Private author"] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "{secret} must not ship"
            );
        }
    }
}
