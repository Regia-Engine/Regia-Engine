// SPDX-License-Identifier: MPL-2.0
//! Content chunks in the CLI (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.3-2.4, § 5):
//! chunk paks at cook time, staging per delivery at package time, `regia assets chunks`,
//! `regia assets why` and `regia content dlc`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_asset::content::{
    BASE_CHUNK, CHUNK_MAP_FILE, CHUNK_WHY_FILE, CONTENT_HOST_FILE, CONTENT_MANIFEST_FILE, CONTENT_TRUST_FILE,
    ContentHost, ContentManifest, Delivery,
};
use regia_asset::registry::{AssetRegistry, RegistryEntry, cache_registry_path};
use regia_asset::{AssetId, AssetManifest, cooked_cache_dir};
use regia_asset_pipeline::chunks::{
    Assignment, ChunkProblem, ChunkRules, ChunkWriteOptions, Reason, Severity, assign_with_roots,
    check_android_pack_limits, check_assignment_integrity, check_references_with, optional_references,
    summarize_chunks, why_table, write_chunks, write_dlc,
};
use regia_asset_pipeline::closure_roots::{RootError, RootEvidence};
use serde_json::{Value, json};

/// `regia content` subcommands.
#[derive(Debug, Subcommand)]
pub enum ContentCommand {
    /// Cook one chunk against a frozen base build (a DLC, or a patch with `overrides = true`).
    Dlc(ContentDlcArgs),
    /// Serve a chunks folder (`dist/<platform>/chunks`) over HTTP with `Range` support, for tests and
    /// manual checks. No directory listing; not for production.
    Serve(ContentServeArgs),
}

/// `regia content serve` arguments.
#[derive(Debug, Args)]
pub struct ContentServeArgs {
    /// The directory to serve: the game fetches `<url>/<build_id>/<file>`, so serve the parent of a
    /// `<build_id>/` folder.
    pub dir: PathBuf,
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Port (0 picks a free one).
    #[arg(long, default_value_t = 8787)]
    pub port: u16,
}

/// `regia content dlc` arguments.
#[derive(Debug, Args)]
pub struct ContentDlcArgs {
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Cooked platform (defaults to host).
    #[arg(long)]
    pub platform: Option<String>,
    /// The chunk to cook (a `[cook.chunks.<name>]` rule).
    #[arg(long)]
    pub name: String,
    /// Directory of the frozen base: its `content-manifest.ron` and `base.registry.bin`.
    #[arg(long, value_name = "DIR")]
    pub against: PathBuf,
    /// Output directory (default `dist/<platform>/chunks`).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// `regia assets chunks` arguments.
#[derive(Debug, Args)]
pub struct AssetsChunksArgs {
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Cooked platform (defaults to host).
    #[arg(long)]
    pub platform: Option<String>,
    /// Print JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Size budget of each chunk (bytes, or with a unit: `KiB`, `MiB`, `GiB`; `K`/`M`/`G` alike).
    /// Over budget: the budget table, and a non-zero exit (ADR-2710).
    #[arg(long, value_name = "SIZE")]
    pub max_size: Option<String>,
    /// Size budget of all chunks together.
    #[arg(long, value_name = "SIZE")]
    pub max_total: Option<String>,
    /// What a budget measures: `disk` (pak bytes, what a player downloads; needs the paks of a
    /// cook) or `memory` (decompressed cooked bytes).
    #[arg(long, value_name = "WHAT", default_value = "disk")]
    pub measure: super::assets_budget::Measure,
}

/// `regia assets why` arguments.
#[derive(Debug, Args)]
pub struct AssetsWhyArgs {
    /// The asset: id, `assets/...` path, or a path under `assets/`.
    pub asset: String,
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Cooked platform (defaults to host).
    #[arg(long)]
    pub platform: Option<String>,
    /// Print JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Runs a `content` subcommand.
///
/// # Errors
/// See the subcommand.
pub fn run(command: &ContentCommand) -> Result<()> {
    match command {
        ContentCommand::Dlc(args) => {
            let out = dlc(args)?;
            println!("dlc: wrote {}", out.display());
            Ok(())
        }
        ContentCommand::Serve(args) => {
            if !args.dir.is_dir() {
                bail!("{} is not a directory", args.dir.display());
            }
            let listener = std::net::TcpListener::bind((args.bind.as_str(), args.port))
                .with_context(|| format!("binding {}:{}", args.bind, args.port))?;
            println!(
                "serving {} at http://{}",
                args.dir.display(),
                listener.local_addr()?
            );
            regia_asset::content::serve_dir(
                listener,
                args.dir.clone(),
                std::sync::Arc::new(regia_asset::content::ServeStats::default()),
            );
            Ok(())
        }
    }
}

fn platform_of(platform: Option<&String>) -> String {
    platform
        .cloned()
        .unwrap_or_else(|| regia_asset_pipeline::host_platform().to_string())
}

/// The chunk rules of the project's manifest: `Regia.toml`, or the legacy `Ferrite.toml` of a
/// project that has only that one (empty when neither exists).
///
/// # Errors
/// The file does not read or a rule is invalid.
pub fn project_rules(project: &Path) -> Result<ChunkRules> {
    let Some(path) = regia_core::brand::project_manifest(project) else {
        return Ok(ChunkRules::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let rules =
                ChunkRules::from_manifest_toml(&text).with_context(|| format!("in {}", path.display()))?;
            for warning in &rules.warnings {
                eprintln!("warning: {}: {warning}", path.display());
            }
            Ok(rules)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ChunkRules::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The project's chunk root evidence (CT-25, ADR-8477): the pipeline's
/// [`regia_asset_pipeline::chunks::startup_roots`], the resolver named build profiles share. A
/// missing startup level, an unreadable raw file or a malformed level or UI layout fails with its
/// `content_root_*` code.
pub(crate) fn project_roots(
    project: &Path,
    registry: &AssetRegistry,
    rules: &ChunkRules,
) -> Result<RootEvidence> {
    regia_asset_pipeline::chunks::startup_roots(project, registry, rules).map_err(|e| root_error(&e).into())
}

/// A root-resolution failure as a shipping-integrity error, so CLI JSON and the agent's package
/// job report keep the `content_root_*` code.
pub(crate) fn root_error(error: &RootError) -> ContentIntegrityError {
    let text = error.to_string();
    let message = text
        .strip_prefix(error.code())
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(&text)
        .to_owned();
    ContentIntegrityError {
        reason: error.code(),
        problems: vec![json!({"reason": error.code(), "root_path": [error.subject()],
            "detail": error.code(), "message": message, "severity": "error"})],
        message,
    }
}

/// The JSON of a root report (`roots` in `regia assets chunks --json`).
pub(crate) fn roots_json(registry: &AssetRegistry, roots: &RootEvidence) -> Value {
    let path_of = |id: AssetId| {
        registry
            .by_id(id)
            .map_or_else(|| id.to_string(), |e| e.path.clone())
    };
    serde_json::to_value(roots.report(&path_of)).unwrap_or(Value::Null)
}

/// A shipping-integrity failure, preserved by CLI JSON and the agent's package job report.
#[derive(Debug, thiserror::Error)]
#[error("{reason}: {message}")]
pub struct ContentIntegrityError {
    /// Stable `content_dependency_missing` or `content_registry_invalid` code.
    pub reason: &'static str,
    /// Structured diagnostics, including `root_path`, `detail` and the target ID when known.
    pub problems: Vec<Value>,
    message: String,
}

fn registry_error(path: &Path, error: impl std::fmt::Display) -> ContentIntegrityError {
    let path = path.to_string_lossy().into_owned();
    ContentIntegrityError {
        reason: "content_registry_invalid",
        problems: vec![json!({"reason": "content_registry_invalid", "root_path": [path],
            "detail": "malformed_registry", "message": error.to_string(), "severity": "error"})],
        message: format!("{path}: {error}"),
    }
}

fn problem_json(p: &ChunkProblem) -> Value {
    json!({
        "reason": p.reason, "chunk": p.chunk, "message": p.message,
        "severity": if p.severity == Severity::Warning { "warning" } else { "error" },
        "detail": p.detail, "root_path": p.root_path, "asset": p.asset.to_string(),
        "target": p.target.to_string(),
    })
}

pub(crate) fn require_integrity(problems: &[ChunkProblem]) -> Result<()> {
    if problems.is_empty() {
        return Ok(());
    }
    Err(ContentIntegrityError {
        reason: "content_dependency_missing",
        problems: problems.iter().map(problem_json).collect(),
        message: problems
            .iter()
            .map(|p| p.message.as_str())
            .collect::<Vec<_>>()
            .join("\n  "),
    }
    .into())
}

/// The cook's registry for `cache_dir`, or legacy records from an absent registry's manifest
/// and any declared cooked-header dependencies.
/// A present but unreadable registry is never replaced by a dependency-free fallback.
///
/// # Errors
/// `content_registry_invalid` for an unreadable registry or fallback manifest.
pub fn project_registry(cache_dir: &Path) -> Result<AssetRegistry> {
    let path = cache_registry_path(cache_dir);
    if let Some(registry) = AssetRegistry::read_from(&path).map_err(|e| registry_error(&path, e))? {
        return Ok(registry);
    }
    let manifest = AssetManifest::read_from_cache(cache_dir)
        .map_err(|e| registry_error(&cache_dir.join("asset_manifest.bin"), e))?
        .unwrap_or_default();
    let entries = manifest
        .entries
        .into_iter()
        .map(|e| -> Result<RegistryEntry> {
            let file = cache_dir.join(regia_asset::cooked_file_name(e.id));
            let dependencies = match std::fs::read(&file) {
                Ok(bytes) if bytes.starts_with(&regia_asset::COOKED_MAGIC) => {
                    regia_asset::decode_cooked_file(&bytes)
                        .with_context(|| format!("decoding {} for the legacy registry", file.display()))?
                        .0
                        .dependencies
                }
                // Legacy paks can contain an unwrapped payload. There is no declared header edge.
                Ok(_) => Vec::new(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
            };
            Ok(RegistryEntry {
                id: e.id,
                path: e.path,
                type_path: e.type_path,
                dependencies,
                tags: Default::default(),
                size: 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(AssetRegistry::from_entries(entries))
}

/// IDs the writer can publish: both manifested and present as cooked files.
pub(crate) fn available_cooked(cache: &Path) -> Result<BTreeSet<AssetId>> {
    let manifest = AssetManifest::read_from_cache(cache)
        .map_err(|e| registry_error(&cache.join("asset_manifest.bin"), e))?
        .unwrap_or_default();
    Ok(manifest
        .entries
        .iter()
        .filter(|e| cache.join(regia_asset::cooked_file_name(e.id)).is_file())
        .map(|e| e.id)
        .collect())
}

/// `<version>-<build_number>-<cook hash>`: the cook hash is the first 12 hex digits of the BLAKE3
/// of the cache's asset manifest. The version is free text in `regia.toml` (the Android
/// `versionName`), but the id is a path segment of every chunk request (`<base_url>/<build_id>/`):
/// [`build_id_version`] maps it to a safe segment, so the cook never writes an id the runtime
/// refuses (`regia_asset::content::validate_build_id`).
///
/// # Errors
/// The cache has no asset manifest (no cook finished there) or it does not read: the id must
/// change with the content, and hashing nothing would give every such cache the same id
/// (ADR-7612).
pub fn build_id(project: &Path, cache_dir: &Path) -> Result<String> {
    let (version, build) = crate::manifest::Manifest::load(project)
        .map(|m| (m.project.version, m.project.build_number))
        .unwrap_or_else(|_| ("0.0.0".to_owned(), 0));
    let path = cache_dir.join("asset_manifest.bin");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "no asset manifest at {}: no cook finished for this cache, so there is no content to name a build \
             id after (run `regia cook` for the platform, then pack again)",
            path.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("reading {} for the build id", path.display())),
    };
    let hash = blake3::hash(&bytes).to_hex();
    Ok(format!("{}-{build}-{}", build_id_version(&version), &hash[..12]))
}

/// The `[project] version` as a build-id segment: ASCII letters, digits, `.`, `_`, `+` and `-`
/// kept, anything else (`/`, `\`, spaces, control characters, non-ASCII) becomes `_`, a run of
/// dots is cut to one (no `..`), and the result is at most 64 bytes (`0.0.0` when empty).
#[must_use]
pub fn build_id_version(version: &str) -> String {
    let mut out = String::with_capacity(version.len());
    for c in version.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
            c
        } else {
            '_'
        };
        if c == '.' && out.ends_with('.') {
            continue;
        }
        if out.len() >= 64 {
            break;
        }
        out.push(c);
    }
    if out.is_empty() { "0.0.0".to_owned() } else { out }
}

/// Rechecks a reused package's source closure; freshness is not proof of completeness.
pub(crate) fn check_project_integrity(project: &Path, cache: &Path) -> Result<()> {
    let rules = project_rules(project)?;
    let registry = project_registry(cache)?;
    let roots = project_roots(project, &registry, &rules)?;
    let assignment = assign_with_roots(&registry, &rules, &roots)?;
    let available = available_cooked(cache)?;
    require_integrity(
        &check_assignment_integrity(
            &registry,
            &assignment,
            &rules,
            &optional_references(&registry),
            Some(&available),
        )
        .problems,
    )
}

/// Assigns the project's assets to chunks and writes their paks, `content-manifest.ron`,
/// `chunk-map.bin` and `chunk-why.bin` into `pak_dir`. Without rules this is `base.fpak` (byte-identical to the
/// single-pak cook) plus a one-chunk manifest. Reference errors fail the cook.
///
/// # Errors
/// Bad rules, reference errors, or a write failure.
pub fn pack_project_chunks(
    project: &Path,
    cache_dir: &Path,
    pak_dir: &Path,
    platform: &str,
    compress: bool,
    gpu_meshes: bool,
) -> Result<Vec<PathBuf>> {
    pack_project_chunks_with_ratio(project, cache_dir, pak_dir, platform, (compress, 0.0), gpu_meshes)
}

/// [`pack_project_chunks`] with a `PakWriteOptions::min_ratio` for the compressed chunks: an entry
/// lz4 saves less than that fraction of is stored raw (`regia cook --ship`, ADR-7971).
///
/// # Errors
/// Bad rules, reference errors, or a write failure.
pub fn pack_project_chunks_with_ratio(
    project: &Path,
    cache_dir: &Path,
    pak_dir: &Path,
    platform: &str,
    (compress, min_ratio): (bool, f32),
    gpu_meshes: bool,
) -> Result<Vec<PathBuf>> {
    let rules = project_rules(project)?;
    let registry = project_registry(cache_dir)?;
    let roots = project_roots(project, &registry, &rules)?;
    let assignment = assign_with_roots(&registry, &rules, &roots)?;
    if !roots.typed_complete() {
        eprintln!(
            "warning: content_root_fallback: {} raw file(s) of unknown formats were scanned with the generic \
             quoted-string fallback (`regia assets chunks --json` lists them under roots.coverage)",
            roots.fallback_files().len()
        );
    }
    let optional = optional_references(&registry);
    let available = available_cooked(cache_dir)?;
    require_integrity(
        &check_assignment_integrity(&registry, &assignment, &rules, &optional, Some(&available)).problems,
    )?;
    let problems = check_references_with(&registry, &assignment, &rules, &optional_references(&registry));
    for warning in problems.iter().filter(|p| p.severity == Severity::Warning) {
        eprintln!("warning: {}: {}", warning.reason, warning.message);
    }
    // Each line leads with its reason code (`chunk_base_reference`, `chunk_unlisted_reference`,
    // `chunk_cycle`), as `pack_limit` does, so a CI log names the spec 40 § 5 code.
    let errors: Vec<String> = problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .map(|p| format!("{}: {}", p.reason, p.message))
        .collect();
    if !errors.is_empty() {
        bail!(
            "chunk reference check failed ({} error(s)):\n  {}",
            errors.len(),
            errors.join("\n  ")
        );
    }
    let opts = ChunkWriteOptions {
        compress,
        min_ratio,
        gpu_meshes,
        build_id: build_id(project, cache_dir)?,
        platform: platform.to_owned(),
        compatible_with: Vec::new(),
    };
    let (manifest, report) = write_chunks(cache_dir, pak_dir, &assignment, &rules, &opts)?;
    if !assignment.unshipped.is_empty() {
        // `content = "closure"` (ADR-8092): say how much the entry points left out, so a missing
        // entry shows at the cook, not as a NotFound in the game.
        eprintln!(
            "pak: {} cooked asset(s) reached from no entry point ship in no chunk (content = \"closure\"; \
             `regia assets chunks` lists them)",
            assignment.unshipped.len()
        );
    }
    // The runtime `content.why` reason chain (ADR-1876).
    why_table(&registry, &assignment, &rules)
        .write_to_dir(pak_dir)
        .map_err(|e| anyhow::anyhow!("writing {CHUNK_WHY_FILE}: {e}"))?;
    // The package does not ship `Regia.toml`: the host of `external` chunks travels in this record.
    match &rules.base_url {
        Some(url) => {
            // The game refuses plain http:// to another machine unless a developer opts in
            // (audit v0.2.12): say so at the cook, not when a player's download fails.
            if regia_asset::content::HttpUrl::parse(url).is_ok_and(|u| !u.https && !u.is_loopback()) {
                eprintln!(
                    "warning: [content] base_url {url:?} is plain http:// to another machine; the game refuses it \
                     unless REGIA_CONTENT_ALLOW_HTTP=1 is set (use https:// for players)"
                );
            }
            ContentHost::new(url.as_str())
                .write_to_dir(pak_dir)
                .map_err(|e| anyhow::anyhow!("writing {CONTENT_HOST_FILE}: {e}"))?;
        }
        None => {
            let _ = std::fs::remove_file(pak_dir.join(CONTENT_HOST_FILE));
        }
    }
    // The keys that may sign a fetched manifest travel the same way (ADR-2710).
    match super::assets_trust::project_trust(project)? {
        Some(trust) => trust
            .write_to_dir(pak_dir)
            .map_err(|e| anyhow::anyhow!("writing {CONTENT_TRUST_FILE}: {e}"))?,
        None => {
            let _ = std::fs::remove_file(pak_dir.join(CONTENT_TRUST_FILE));
        }
    }
    if platform == "android" {
        // Store limits are checked before any Android build step (spec 40 § 2.6).
        let warnings = check_android_pack_limits(&manifest, &rules.android_limits())
            .map_err(|e| anyhow::anyhow!("{}: {e}", e.reason()))?;
        if manifest.chunks.len() > 1 {
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
        }
    }
    if !report.kept.is_empty() {
        eprintln!("pak: kept unchanged chunk(s): {}", report.kept.join(", "));
    }
    check_unwritten(&report.unwritten, unwritten_allowed())?;
    Ok(report.paks)
}

/// The environment variable that lets a pack go on when assets assigned to chunks have no cooked
/// file (ADR-7612): `1`, `on` or `true`. For a deliberate partial pack only; those assets ship in
/// no pak and the game reads a plain NotFound for them.
pub const ALLOW_UNWRITTEN_ENV: &str = "REGIA_ALLOW_UNWRITTEN_ASSETS";

/// Whether [`ALLOW_UNWRITTEN_ENV`] is set to an on value.
fn unwritten_allowed() -> bool {
    regia_core::brand::env_var(ALLOW_UNWRITTEN_ENV)
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "on" | "true"))
}

/// Most asset ids a `chunk_asset_unwritten` message names.
const UNWRITTEN_LISTED: usize = 8;

/// Fails the pack when the assignment put assets in chunks whose cooked file is missing (a cook
/// that skipped or failed them with only a warning): they would ship in no pak and be left out of
/// `chunk-map.bin`, so the game reads a plain NotFound instead of `unavailable:<chunk>`
/// (deep-zai-assets A9, ADR-7612). With `allow` it warns, as it used to.
fn check_unwritten(unwritten: &[AssetId], allow: bool) -> Result<()> {
    let Some(first) = unwritten.first() else {
        return Ok(());
    };
    if allow {
        eprintln!(
            "warning: chunk_asset_unwritten: {} asset(s) assigned to chunks have no cooked file (first: {first}); \
             they ship in no pak ({ALLOW_UNWRITTEN_ENV} is set)",
            unwritten.len()
        );
        return Ok(());
    }
    let listed: Vec<String> = unwritten
        .iter()
        .take(UNWRITTEN_LISTED)
        .map(ToString::to_string)
        .collect();
    bail!(
        "chunk_asset_unwritten: {} asset(s) assigned to chunks have no cooked file, so they would ship in no pak: \
         {}{}; cook again and fix the cook's warnings for them, or set {ALLOW_UNWRITTEN_ENV}=1 to pack without them",
        unwritten.len(),
        listed.join(", "),
        if unwritten.len() > UNWRITTEN_LISTED {
            ", ..."
        } else {
            ""
        }
    )
}

/// Refuses a chunk pak whose length or BLAKE3 is not what its manifest entry records (ADR-7413):
/// a half-written or replaced file in the pak directory must not reach a package, where the
/// runtime would mount it and fall back to older versions of its assets with only a warning.
pub(crate) fn check_chunk_file(pak_dir: &Path, chunk: &regia_asset::content::ChunkEntry) -> Result<()> {
    let path = pak_dir.join(&chunk.file);
    let size = std::fs::metadata(&path)
        .with_context(|| format!("staging chunk {}: {}", chunk.name, path.display()))?
        .len();
    let hash = if size == chunk.size {
        regia_asset::content::hash_file(&path)
            .with_context(|| format!("staging chunk {}: {}", chunk.name, path.display()))?
    } else {
        String::new()
    };
    if size != chunk.size || hash != chunk.hash {
        bail!(
            "chunk_mismatch: {} ({}) is {size} bytes, blake3 {hash:?}; the content manifest records {} bytes, \
             blake3 {:?}. The pak directory is stale or damaged: delete {} and package again",
            chunk.name,
            path.display(),
            chunk.size,
            chunk.hash,
            pak_dir.display()
        );
    }
    Ok(())
}

/// Stages a chunked build into a package: `install` chunks (with their registries), the manifest
/// and the chunk map into `content_dir`; every other chunk with a copy of the manifest into
/// `external_dir` (`dist/<platform>/chunks`, uploadable to a static file host unchanged). Returns
/// the pak names staged into `content_dir`, or `None` when `pak_dir` has no manifest.
///
/// # Errors
/// IO errors, or a manifest that does not parse.
pub fn stage_chunks(pak_dir: &Path, content_dir: &Path, external_dir: &Path) -> Result<Option<Vec<String>>> {
    let Some(manifest) = ContentManifest::read_from_dir(pak_dir)? else {
        return Ok(None);
    };
    let mut staged = Vec::new();
    let mut external = false;
    for chunk in &manifest.chunks {
        let registry = format!(
            "{}{}",
            Path::new(&chunk.file)
                .file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
            regia_asset::registry::PAK_REGISTRY_SUFFIX
        );
        let dest = if chunk.delivery == Delivery::Install || chunk.name == BASE_CHUNK {
            staged.push(chunk.file.clone());
            content_dir
        } else {
            external = true;
            std::fs::create_dir_all(external_dir)?;
            external_dir
        };
        check_chunk_file(pak_dir, chunk)?;
        std::fs::copy(pak_dir.join(&chunk.file), dest.join(&chunk.file))
            .with_context(|| format!("staging chunk {}", chunk.name))?;
        if pak_dir.join(&registry).is_file() {
            std::fs::copy(pak_dir.join(&registry), dest.join(&registry))?;
        }
    }
    for file in [CONTENT_MANIFEST_FILE, CHUNK_MAP_FILE, CHUNK_WHY_FILE] {
        if pak_dir.join(file).is_file() {
            std::fs::copy(pak_dir.join(file), content_dir.join(file))?;
            if external {
                std::fs::copy(pak_dir.join(file), external_dir.join(file))?;
            }
        }
    }
    for record in [CONTENT_HOST_FILE, CONTENT_TRUST_FILE] {
        if pak_dir.join(record).is_file() {
            std::fs::copy(pak_dir.join(record), content_dir.join(record))?;
        } else {
            let _ = std::fs::remove_file(content_dir.join(record));
        }
    }
    Ok(Some(staged))
}

/// Removes from `assets_dir` (the package's copy of the project's raw files) every file whose
/// asset a chunk other than `install`/base carries, and returns the removed `assets/`-relative
/// paths. Such a file's cooked form travels in its chunk's pak (a store asset pack or the static
/// host), so a loose copy in the base package would ship the chunk's bytes twice and make the
/// delivery pointless: a sound (sounds ship raw as well as cooked) in an on-demand chunk put the
/// whole pack into the base APK (ADR-7696). A file with no `.meta`, or whose asset no chunk lists
/// (raw-only data), stays.
///
/// # Errors
/// The manifest or chunk map in `pak_dir` does not read, or a staged file cannot be removed.
pub fn unstage_chunk_raw_files(project_dir: &Path, pak_dir: &Path, assets_dir: &Path) -> Result<Vec<String>> {
    let Some(manifest) = ContentManifest::read_from_dir(pak_dir)? else {
        return Ok(Vec::new());
    };
    let Some(map) = regia_asset::content::ChunkMap::read_from_dir(pak_dir)? else {
        return Ok(Vec::new());
    };
    let assets = project_dir.join("assets");
    let mut removed = Vec::new();
    for source in regia_asset_pipeline::AssetDatabase::discover_sources(&assets) {
        let Ok(bytes) = std::fs::read(regia_asset::meta_path_for(&source)) else {
            continue;
        };
        let Ok(meta) = regia_asset::MetaSidecar::from_ron(&bytes) else {
            continue;
        };
        let Some(chunk) = map.chunk_of(meta.id).and_then(|name| manifest.chunk(name)) else {
            continue;
        };
        if chunk.delivery == Delivery::Install || chunk.name == BASE_CHUNK {
            continue;
        }
        let Ok(rel) = source.strip_prefix(&assets) else {
            continue;
        };
        let staged = assets_dir.join(rel);
        if staged.is_file() {
            std::fs::remove_file(&staged).with_context(|| format!("unstaging {}", staged.display()))?;
            removed.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    removed.sort();
    Ok(removed)
}

struct Loaded {
    rules: ChunkRules,
    registry: AssetRegistry,
    roots: RootEvidence,
    assignment: Assignment,
    pak_dir: PathBuf,
    cache_dir: PathBuf,
}

fn load(project: &Path, platform: Option<&String>) -> Result<Loaded> {
    let platform = platform_of(platform);
    let cache_dir = cooked_cache_dir(project, &platform);
    let rules = project_rules(project)?;
    let registry = project_registry(&cache_dir)?;
    if registry.is_empty() {
        bail!(
            "no cooked assets for {platform} in {} (run `regia cook` first)",
            cache_dir.display()
        );
    }
    let roots = project_roots(project, &registry, &rules)?;
    let assignment = assign_with_roots(&registry, &rules, &roots)?;
    Ok(Loaded {
        rules,
        registry,
        roots,
        assignment,
        pak_dir: project.join("build").join("paks").join(&platform),
        cache_dir,
    })
}

/// The text (or JSON) `regia assets chunks` prints.
///
/// # Errors
/// No cooked registry, or bad rules.
pub fn chunks_report(args: &AssetsChunksArgs) -> Result<String> {
    let l = match load(&args.project, args.platform.as_ref()) {
        Ok(loaded) => loaded,
        Err(e) if args.json => {
            if let Some(integrity) = e.downcast_ref::<ContentIntegrityError>() {
                return Ok(serde_json::to_string_pretty(&json!({
                    "chunks": [], "problems": integrity.problems, "complete": false,
                }))?);
            }
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    let (rows, problems) = rows_of(&l)?;
    let unshipped = unshipped_paths(&l.registry, &l.assignment);
    if args.json {
        return Ok(serde_json::to_string_pretty(&json!({
            "chunks": rows,
            "complete": !problems.iter().any(|p| p["severity"] == "error"),
            "problems": problems,
            "content": l.rules.content.name(),
            "unshipped": unshipped,
            "roots": roots_json(&l.registry, &l.roots),
        }))?);
    }
    let mut text = chunks_text(&rows, &problems);
    text.push_str(&roots_text(&l.roots));
    if !unshipped.is_empty() {
        text.push_str(&format!(
            "\nunshipped: {} asset(s) no entry point reaches (content = \"closure\")",
            unshipped.len()
        ));
        for path in unshipped.iter().take(MAX_UNSHIPPED_LISTED) {
            text.push_str(&format!("\n    {path}"));
        }
        if unshipped.len() > MAX_UNSHIPPED_LISTED {
            text.push_str(&format!(
                "\n    ... and {} more (--json lists every one)",
                unshipped.len() - MAX_UNSHIPPED_LISTED
            ));
        }
    }
    Ok(text)
}

/// The root summary line of the text form of `regia assets chunks` (and its fallback files).
fn roots_text(roots: &RootEvidence) -> String {
    let boot = roots.boot_ids().len();
    let mut out = format!(
        "\nroots: {} cooked ({boot} boot), {} raw file(s); typed coverage {}",
        roots.cooked.len(),
        roots.raw.len(),
        if roots.typed_complete() {
            "complete"
        } else {
            "incomplete"
        }
    );
    for path in roots.fallback_files() {
        out.push_str(&format!("\n    fallback scan: assets/{path}"));
    }
    for decl in &roots.unmatched {
        out.push_str(&format!("\n    matched nothing: {decl}"));
    }
    out
}

/// Most unshipped paths the text form of `regia assets chunks` lists.
const MAX_UNSHIPPED_LISTED: usize = 20;

/// The registry paths of the assets that ship in no chunk (`content = "closure"`, ADR-8092), sorted.
fn unshipped_paths(registry: &AssetRegistry, assignment: &Assignment) -> Vec<String> {
    let mut paths: Vec<String> = assignment
        .unshipped
        .iter()
        .map(|id| {
            registry
                .by_id(*id)
                .map_or_else(|| id.to_string(), |e| e.path.clone())
        })
        .collect();
    paths.sort();
    paths
}

/// The pak directory of a cooked platform (`build/paks/<platform>`), where chunk paks are written.
#[must_use]
pub fn pak_dir_of(project: &Path, platform: Option<&String>) -> PathBuf {
    project.join("build").join("paks").join(platform_of(platform))
}

/// The rows of `regia assets chunks` (name, delivery, assets, disk and memory bytes, requires,
/// mount order, size by type) and the reference problems.
///
/// # Errors
/// No cooked registry, or bad rules.
pub fn chunk_rows(project: &Path, platform: Option<&String>) -> Result<(Vec<Value>, Vec<Value>)> {
    rows_of(&load(project, platform)?)
}

/// [`chunk_rows`] of a loaded project.
fn rows_of(l: &Loaded) -> Result<(Vec<Value>, Vec<Value>)> {
    // One row per chunk: name, delivery, assets, disk bytes (pak), memory bytes (cooked), requires,
    // mount order, size by asset type.
    let rows: Vec<Value> = summarize_chunks(&l.registry, &l.assignment, &l.rules, &l.cache_dir, &l.pak_dir)?
        .into_iter()
        .map(|c| {
            json!({
                "name": c.name,
                "delivery": c.delivery.name(),
                "assets": c.assets,
                "disk_bytes": c.disk_bytes,
                "memory_bytes": c.memory_bytes,
                "requires": c.requires,
                "mount_order": c.mount_order,
                "by_type": c.by_type,
            })
        })
        .collect();
    let optional = optional_references(&l.registry);
    let available = available_cooked(&l.cache_dir)?;
    let mut problems = check_references_with(&l.registry, &l.assignment, &l.rules, &optional);
    // The graph-only check above supplies required-edge errors; add file completeness once.
    problems.retain(|p| p.reason != "content_dependency_missing");
    problems.extend(
        check_assignment_integrity(&l.registry, &l.assignment, &l.rules, &optional, Some(&available))
            .problems,
    );
    let problems = problems.iter().map(problem_json).collect();
    Ok((rows, problems))
}

/// The text table of `regia assets chunks`.
fn chunks_text(rows: &[Value], problems: &[Value]) -> String {
    let mut out = format!(
        "{:<24} {:<12} {:>7} {:>14} {:>14}  requires\n",
        "chunk", "delivery", "assets", "disk bytes", "memory bytes"
    );
    for r in rows {
        out.push_str(&format!(
            "{:<24} {:<12} {:>7} {:>14} {:>14}  {}\n",
            r["name"].as_str().unwrap_or(""),
            r["delivery"].as_str().unwrap_or(""),
            r["assets"],
            r["disk_bytes"],
            r["memory_bytes"],
            r["requires"].as_array().map_or_else(String::new, |a| {
                a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")
            }),
        ));
        if let Some(types) = r["by_type"].as_object() {
            for (ty, bytes) in types {
                out.push_str(&format!("    {ty:<40} {bytes:>14}\n"));
            }
        }
    }
    for p in problems {
        out.push_str(&format!(
            "{}: {}\n",
            p["severity"].as_str().unwrap_or("error"),
            p["message"].as_str().unwrap_or("")
        ));
    }
    out.trim_end().to_owned()
}

/// `content.why` / `regia assets why` data for one asset. An asset that ships in no chunk
/// (`content = "closure"` and no entry point reaches it, ADR-8092) answers `chunk: null` and
/// `reason: "unshipped"`.
///
/// # Errors
/// The asset is unknown.
pub fn why_value(registry: &AssetRegistry, assignment: &Assignment, asset: &str) -> Result<Value> {
    let Some(entry) = registry.resolve(asset) else {
        bail!("asset_not_found: the asset registry has no record for {asset}");
    };
    if assignment.unshipped.contains(&entry.id) {
        return Ok(json!({
            "asset": entry.path,
            "id": entry.id.to_string(),
            "chunk": Value::Null,
            "reason": "unshipped",
            "via": [entry.path],
        }));
    }
    let chunk = assignment
        .chunk_of
        .get(&entry.id)
        .cloned()
        .unwrap_or_else(|| BASE_CHUNK.to_owned());
    let reason = assignment
        .reason
        .get(&entry.id)
        .cloned()
        .unwrap_or(Reason::Default);
    let via: Vec<String> = assignment
        .via_chain(entry.id)
        .into_iter()
        .map(|id| {
            registry
                .by_id(id)
                .map_or_else(|| id.to_string(), |e| e.path.clone())
        })
        .collect();
    let mut value = json!({
        "asset": entry.path,
        "id": entry.id.to_string(),
        "chunk": chunk,
        "reason": reason.kind(),
        "via": via,
    });
    if let Reason::Rule { chunk, rule_index } = &reason {
        value["rule"] = json!({ "chunk": chunk, "index": rule_index });
    }
    if let Some(copies) = assignment.duplicates.get(&entry.id) {
        value["duplicated_in"] = json!(copies);
    }
    Ok(value)
}

/// The text (or JSON) `regia assets why` prints.
///
/// # Errors
/// No cooked registry, bad rules, or an unknown asset.
pub fn why_report(args: &AssetsWhyArgs) -> Result<String> {
    let l = load(&args.project, args.platform.as_ref())?;
    let mut value = why_value(&l.registry, &l.assignment, &args.asset)?;
    if let Some(root) = l
        .registry
        .resolve(&args.asset)
        .and_then(|entry| l.roots.cooked.get(&entry.id))
    {
        value["root"] = serde_json::to_value(root).unwrap_or(Value::Null);
    }
    if args.json {
        return Ok(serde_json::to_string_pretty(&value)?);
    }
    if value["reason"] == "unshipped" {
        return Ok(format!(
            "{} ships in no chunk (unshipped): content = \"closure\" and no entry point reaches it",
            value["asset"].as_str().unwrap_or("")
        ));
    }
    let via: Vec<&str> = value["via"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let root = match value.get("root") {
        Some(root) => format!(
            "\n  root: {} declared by {}{}{}",
            root["kind"].as_str().unwrap_or(""),
            root["declared_by"].as_str().unwrap_or(""),
            root["via"]
                .as_str()
                .map_or_else(String::new, |via| format!(", named by assets/{via}")),
            if root["boot"] == true { " (boot)" } else { "" }
        ),
        None => String::new(),
    };
    Ok(format!(
        "{} is in chunk {} ({}){}{root}",
        value["asset"].as_str().unwrap_or(""),
        value["chunk"].as_str().unwrap_or(""),
        value["reason"].as_str().unwrap_or(""),
        if via.len() > 1 {
            format!("\n  via {}", via.join(" <- "))
        } else {
            String::new()
        }
    ))
}

/// `regia content dlc`: cooks `args.name` against the base in `args.against`.
///
/// # Errors
/// The base manifest or registry is missing, a collision, or a write failure.
pub fn dlc(args: &ContentDlcArgs) -> Result<PathBuf> {
    let platform = platform_of(args.platform.as_ref());
    let cache_dir = cooked_cache_dir(&args.project, &platform);
    let rules = project_rules(&args.project)?;
    let registry = project_registry(&cache_dir)?;
    let base_manifest = ContentManifest::read_from_dir(&args.against)?
        .with_context(|| format!("no {CONTENT_MANIFEST_FILE} in {}", args.against.display()))?;
    let base_registry_path = args.against.join(format!(
        "{BASE_CHUNK}{}",
        regia_asset::registry::PAK_REGISTRY_SUFFIX
    ));
    let base_registry = AssetRegistry::read_from(&base_registry_path)?
        .with_context(|| format!("no base registry at {}", base_registry_path.display()))?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.project.join("dist").join(&platform).join("chunks"));
    let opts = ChunkWriteOptions {
        compress: true,
        min_ratio: 0.0,
        gpu_meshes: regia_asset_pipeline::gpu_meshes_for(&cache_dir),
        build_id: build_id(&args.project, &cache_dir)?,
        platform,
        compatible_with: Vec::new(),
    };
    write_dlc(
        &cache_dir,
        &out,
        &registry,
        &rules,
        &args.name,
        (&base_manifest, &base_registry),
        &opts,
    )?;
    Ok(out.join(format!("{}.fpak", args.name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::ManifestEntry;
    use regia_asset_pipeline::chunks::{assign, startup_assets};

    /// A project with a cooked cache (no real cook): `ui/menu.png` (base) and
    /// `levels/arena/a.glb` (an on-demand chunk).
    fn project(tag: &str, rules: &str) -> (PathBuf, PathBuf, [AssetId; 2]) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::write(
            root.join(crate::manifest::MANIFEST_FILE),
            format!("[project]\nname = \"T\"\nid = \"com.example.t\"\nversion = \"1.2.0\"\nbuild_number = 7\n{rules}"),
        )
        .expect("toml");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let ids = [AssetId::new_random(), AssetId::new_random()];
        let mut manifest = AssetManifest::default();
        for (id, path) in ids
            .iter()
            .zip(["assets/ui/menu.png", "assets/levels/arena/a.glb"])
        {
            std::fs::write(cache.join(regia_asset::cooked_file_name(*id)), path.repeat(100)).expect("cooked");
            manifest.entries.push(ManifestEntry {
                path: path.into(),
                id: *id,
                type_path: "regia_asset::CookedData".into(),
            });
        }
        manifest.write_to_cache(&cache).expect("manifest");
        (root, cache, ids)
    }

    const ARENA: &str = "[cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\ndelivery = \"on_demand\"\n";

    /// deep-zai-assets A9 (w9-pack-stamp-and-content): an asset the assignment puts in a chunk but
    /// whose cooked file is gone ships in no pak and is left out of the chunk map, so the game reads
    /// a plain NotFound. The pack used to print a warning and succeed.
    #[test]
    fn assets_assigned_to_chunks_without_a_cooked_file_fail_the_pack() {
        let (root, cache, ids) = project("cli-content-unwritten", ARENA);
        std::fs::remove_file(cache.join(regia_asset::cooked_file_name(ids[1]))).expect("drop cooked file");
        let pak_dir = root.join("build/paks/linux");
        let error = pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false)
            .err()
            .unwrap_or_else(|| panic!("a pack with an asset in no pak succeeded"));
        let text = format!("{error:#}");
        assert!(text.contains("chunk_asset_unwritten"), "{text}");
        assert!(
            text.contains(&ids[1].to_string()),
            "the message names the asset: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7612: `REGIA_ALLOW_UNWRITTEN_ASSETS` turns the `chunk_asset_unwritten` error back into
    /// the old warning (the environment is read by `pack_project_chunks`; the check takes it as a
    /// parameter so no test touches the process environment). Nothing unwritten passes either way.
    #[test]
    fn the_allow_flag_turns_unwritten_assets_into_a_warning() {
        let ids: Vec<AssetId> = (0..10).map(|_| AssetId::new_random()).collect();
        assert!(check_unwritten(&[], false).is_ok());
        assert!(check_unwritten(&ids, true).is_ok());
        let text = format!("{:#}", check_unwritten(&ids, false).expect_err("refused"));
        assert!(text.starts_with("chunk_asset_unwritten: 10 asset(s)"), "{text}");
        assert!(
            text.contains(ALLOW_UNWRITTEN_ENV),
            "the message names the flag: {text}"
        );
        assert!(text.contains(&ids[UNWRITTEN_LISTED - 1].to_string()), "{text}");
        assert!(
            !text.contains(&ids[UNWRITTEN_LISTED].to_string()),
            "the list is capped: {text}"
        );
    }

    /// deep-zai-assets A10 (w9-pack-stamp-and-content): the build id hashed a missing
    /// `asset_manifest.bin` as empty bytes, so every cache without one (a first cook that failed)
    /// minted the same id, and the chunk URLs no longer changed with the content.
    #[test]
    fn a_cache_without_an_asset_manifest_mints_no_build_id() {
        let (root, cache, _) = project("cli-content-no-manifest", ARENA);
        std::fs::remove_file(cache.join("asset_manifest.bin")).expect("drop manifest");
        let pak_dir = root.join("build/paks/linux");
        let error = pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false)
            .err()
            .unwrap_or_else(|| panic!("a pack without an asset manifest minted a build id"));
        let text = format!("{error:#}");
        assert!(text.contains("asset_manifest.bin"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// deep-zai-assets finding 4 (ADR-7413): a chunk pak that no longer matches the size and
    /// BLAKE3 its manifest entry records (a half-written or replaced file in `build/paks/`) must
    /// not be staged into a package; it used to be copied unchecked.
    #[test]
    fn a_chunk_pak_that_does_not_match_its_manifest_entry_is_not_staged() {
        let (root, cache, _) = project("cli-content-stage-check", ARENA);
        let pak_dir = root.join("build/paks/linux");
        pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        let content = root.join("stage/content");
        let external = root.join("dist/linux/chunks");
        std::fs::create_dir_all(&content).expect("mkdir");
        stage_chunks(&pak_dir, &content, &external).expect("an intact pack stages");

        // Same length, one byte changed: the size check alone would pass it.
        let arena = pak_dir.join("arena.fpak");
        let mut bytes = std::fs::read(&arena).expect("read arena");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&arena, &bytes).expect("corrupt arena");
        let error = stage_chunks(&pak_dir, &content, &external)
            .err()
            .unwrap_or_else(|| panic!("a corrupt chunk was staged"));
        assert!(format!("{error:#}").contains("chunk_mismatch"), "{error:#}");

        // A truncated base pak is refused too.
        bytes[last] ^= 0xff;
        std::fs::write(&arena, &bytes).expect("restore arena");
        let base = pak_dir.join("base.fpak");
        let base_bytes = std::fs::read(&base).expect("read base");
        std::fs::write(&base, &base_bytes[..base_bytes.len() / 2]).expect("truncate base");
        assert!(
            stage_chunks(&pak_dir, &content, &external).is_err(),
            "a truncated base was staged"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `[content] base_url` reaches the package as `content-host.ron`; removing the key removes it.
    #[test]
    fn base_url_is_staged_as_the_host_record() {
        let toml = format!("[content]\nbase_url = \"https://cdn.example.com/game\"\n{ARENA}");
        let (root, cache, _) = project("cli-content-host", &toml);
        let pak_dir = root.join("build/paks/linux");
        pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        let content = root.join("stage/content");
        let external = root.join("dist/linux/chunks");
        std::fs::create_dir_all(&content).expect("mkdir");
        stage_chunks(&pak_dir, &content, &external).expect("stage");
        let host = ContentHost::read_from_dir(&content)
            .expect("read")
            .expect("record");
        assert_eq!(host.base_url, "https://cdn.example.com/game");
        assert!(
            !external.join(CONTENT_HOST_FILE).exists(),
            "the host needs no copy of its own address"
        );

        std::fs::write(root.join("Regia.toml"), ARENA).expect("toml");
        pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        assert!(!pak_dir.join(CONTENT_HOST_FILE).exists());
    }

    /// ADR-2710: `[content] trusted_keys` reaches the package as `content-trust.ron`; removing the
    /// keys removes it from the pak directory and from a re-staged package; bad keys fail the cook.
    #[test]
    fn trusted_keys_are_staged_as_the_trust_record() {
        let key = regia_asset::content::format_public_key(&[6; 32]);
        let toml = format!("[content]\ntrusted_keys = [\"{key}\"]\nsignature_policy = \"optional\"\n{ARENA}");
        let (root, cache, _) = project("cli-content-trust", &toml);
        let pak_dir = root.join("build/paks/linux");
        pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        let content = root.join("stage/content");
        let external = root.join("dist/linux/chunks");
        std::fs::create_dir_all(&content).expect("mkdir");
        stage_chunks(&pak_dir, &content, &external).expect("stage");
        let trust = regia_asset::content::ContentTrust::read_from_dir(&content)
            .expect("read")
            .expect("record");
        assert_eq!(trust.keys, vec![key.clone()]);
        assert_eq!(trust.policy, regia_asset::content::SignaturePolicy::Optional);
        assert!(
            !external.join(CONTENT_TRUST_FILE).exists(),
            "the host does not serve the game's trust"
        );

        std::fs::write(root.join("Regia.toml"), ARENA).expect("toml");
        pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        assert!(!pak_dir.join(CONTENT_TRUST_FILE).exists());
        stage_chunks(&pak_dir, &content, &external).expect("stage");
        assert!(
            !content.join(CONTENT_TRUST_FILE).exists(),
            "a stale record is removed"
        );

        let bad = format!("[content]\ntrusted_keys = [\"{key}\", \"ed25519:00\"]\n{ARENA}");
        std::fs::write(root.join("Regia.toml"), bad).expect("toml");
        assert!(pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Levels audit M2: the pin of spec 40 § 2.2 step 4 never received the startup level, so a glob
    /// over `levels/**` put the first level's assets in an on-demand chunk and the game booted into
    /// `unavailable:arena`.
    #[test]
    fn the_startup_level_keeps_its_assets_in_base() {
        let glob = format!("startup_level = \"levels/arena/main.level.ron\"\n{ARENA}");
        let (root, cache, ids) = project("cli-content-startup", &glob);
        let level = root.join("assets/levels/arena/main.level.ron");
        std::fs::create_dir_all(level.parent().expect("parent")).expect("mkdir");
        std::fs::write(&level, "(entities: [(mesh: \"levels/arena/a.glb#mesh/Body\")])").expect("level");
        let registry = project_registry(&cache).expect("registry");
        assert_eq!(startup_assets(&root, &registry), [ids[1]]);
        let loaded = assign(
            &registry,
            &project_rules(&root).expect("rules"),
            &startup_assets(&root, &registry),
        )
        .expect("assign");
        assert_eq!(
            loaded.chunk_of[&ids[1]], BASE_CHUNK,
            "the level's mesh is in base"
        );

        // Without a startup level the same glob keeps it in the chunk.
        let (root, cache, ids) = project("cli-content-no-startup", ARENA);
        let registry = project_registry(&cache).expect("registry");
        assert!(startup_assets(&root, &registry).is_empty());
        let loaded = assign(&registry, &project_rules(&root).expect("rules"), &[]).expect("assign");
        assert_eq!(loaded.chunk_of[&ids[1]], "arena");
    }

    /// w10-v051-smalls 4: a project with only the legacy `Ferrite.toml` lost its chunk rules and
    /// its startup pin (both read `Regia.toml` alone), so it packed one flat base chunk.
    #[test]
    fn a_legacy_only_project_keeps_its_chunk_rules_and_startup_pin() {
        let glob = format!("startup_level = \"levels/arena/main.level.ron\"\n{ARENA}");
        let (root, cache, ids) = project("cli-content-legacy-manifest", &glob);
        std::fs::rename(
            root.join(crate::manifest::MANIFEST_FILE),
            root.join(regia_core::brand::LEGACY_MANIFEST_FILE),
        )
        .expect("legacy name");
        let level = root.join("assets/levels/arena/main.level.ron");
        std::fs::create_dir_all(level.parent().expect("parent")).expect("mkdir");
        std::fs::write(&level, "(entities: [(mesh: \"levels/arena/a.glb#mesh/Body\")])").expect("level");
        let registry = project_registry(&cache).expect("registry");
        assert_eq!(
            startup_assets(&root, &registry),
            [ids[1]],
            "the pin reads the legacy manifest"
        );
        let rules = project_rules(&root).expect("rules");
        let loaded = assign(&registry, &rules, &[]).expect("assign");
        assert_eq!(loaded.chunk_of[&ids[1]], "arena", "the legacy chunk rules apply");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit v0.2.12 (security): `version = "2026/11/01"` or a control character went into the
    /// build id, a path segment of every chunk request.
    #[test]
    fn a_free_text_version_makes_a_build_id_the_runtime_accepts() {
        for (version, want) in [
            ("1.2.0", "1.2.0"),
            ("2026/11/01", "2026_11_01"),
            ("1.0 beta\r\nX: y", "1.0_beta__X__y"),
            ("../..", "._."),
            ("", "0.0.0"),
            ("1.0+rc.2", "1.0+rc.2"),
        ] {
            let got = build_id_version(version);
            assert_eq!(got, want, "{version:?}");
            let id = format!("{got}-7-0123456789ab");
            assert!(regia_asset::content::validate_build_id(&id).is_ok(), "{id:?}");
        }
        assert_eq!(build_id_version(&"9".repeat(500)).len(), 64);
    }

    /// ADR-7696: a raw file whose asset an on-demand chunk carries leaves the package's raw copy
    /// (it travels in the chunk's pak); base files, files with no `.meta`, assets no chunk lists and
    /// a cook without chunks are untouched.
    #[test]
    fn raw_files_of_pack_chunks_leave_the_base_package() {
        let (root, cache, ids) = project("cli-content-unstage", ARENA);
        let pak_dir = root.join("build/paks/linux");
        let staged = root.join("stage/assets");
        let unlisted = AssetId::new_random();
        let files = [
            ("ui/menu.png", Some(ids[0])),
            ("levels/arena/a.glb", Some(ids[1])),
            ("levels/arena/notes.txt", None),
            ("levels/arena/loose.wav", Some(unlisted)),
        ];
        for (rel, id) in files {
            for dir in [root.join("assets"), staged.clone()] {
                let path = dir.join(rel);
                std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                std::fs::write(&path, rel).expect("file");
            }
            if let Some(id) = id {
                let meta = regia_asset::MetaSidecar::new("test", id);
                std::fs::write(
                    regia_asset::meta_path_for(&root.join("assets").join(rel)),
                    meta.to_ron_string().expect("ron"),
                )
                .expect("meta");
            }
        }
        std::fs::create_dir_all(&pak_dir).expect("mkdir");
        assert!(
            unstage_chunk_raw_files(&root, &pak_dir, &staged)
                .expect("no manifest")
                .is_empty(),
            "a cook without chunks keeps every raw file"
        );
        pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        let removed = unstage_chunk_raw_files(&root, &pak_dir, &staged).expect("unstage");
        assert_eq!(removed, ["levels/arena/a.glb"]);
        assert!(!staged.join("levels/arena/a.glb").exists());
        for kept in ["ui/menu.png", "levels/arena/notes.txt", "levels/arena/loose.wav"] {
            assert!(staged.join(kept).is_file(), "{kept} stays");
        }
        assert!(
            root.join("assets/levels/arena/a.glb").is_file(),
            "the project's own file is never touched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cook_writes_chunk_paks_and_package_stages_by_delivery() {
        let (root, cache, ids) = project("cli-content-stage", ARENA);
        let pak_dir = root.join("build/paks/linux");
        let paks = pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        assert_eq!(paks, [pak_dir.join("base.fpak"), pak_dir.join("arena.fpak")]);
        let manifest = ContentManifest::read_from_dir(&pak_dir)
            .expect("read")
            .expect("manifest");
        assert!(manifest.build_id.starts_with("1.2.0-7-"), "{}", manifest.build_id);
        assert_eq!(
            manifest.chunk("arena").expect("arena").delivery,
            Delivery::OnDemand
        );

        let content = root.join("stage/content");
        let external = root.join("dist/linux/chunks");
        std::fs::create_dir_all(&content).expect("mkdir");
        let staged = stage_chunks(&pak_dir, &content, &external)
            .expect("stage")
            .expect("chunked");
        assert_eq!(staged, ["base.fpak"]);
        assert!(content.join(CONTENT_MANIFEST_FILE).is_file());
        assert!(content.join(CHUNK_MAP_FILE).is_file());
        assert!(
            content.join(CHUNK_WHY_FILE).is_file(),
            "the reason table is staged"
        );
        assert!(
            !content.join("arena.fpak").exists(),
            "on-demand chunks stay out of the package"
        );
        assert!(external.join("arena.fpak").is_file());
        assert!(external.join(CONTENT_MANIFEST_FILE).is_file());
        let map = regia_asset::content::ChunkMap::read_from_dir(&content)
            .expect("map")
            .expect("present");
        assert_eq!(map.chunk_of(ids[1]), Some("arena"));
        let why = regia_asset::content::ChunkWhy::read_from_dir(&content)
            .expect("why")
            .expect("present");
        let answer = why.explain(ids[1]).expect("explained");
        assert_eq!((answer.chunk.as_str(), answer.reason), ("arena", "rule"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_list_chunks_and_explain_an_asset() {
        let (root, cache, _) = project("cli-content-report", ARENA);
        pack_project_chunks(
            &root,
            &cache,
            &root.join("build/paks/linux"),
            "linux",
            false,
            false,
        )
        .expect("pack");
        let args = AssetsChunksArgs {
            project: root.clone(),
            platform: Some("linux".into()),
            json: true,
            max_size: None,
            max_total: None,
            measure: super::super::assets_budget::Measure::Disk,
        };
        let value: Value = serde_json::from_str(&chunks_report(&args).expect("report")).expect("json");
        let chunks = value["chunks"].as_array().expect("chunks");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1]["name"], "arena");
        assert_eq!(chunks[1]["delivery"], "on_demand");
        assert!(chunks[1]["disk_bytes"].as_u64().is_some_and(|b| b > 0));
        assert!(chunks[1]["memory_bytes"].as_u64().is_some_and(|b| b > 0));
        // The size budget gate (ADR-2710) measures the paks: one byte under the arena pak fails
        // the per-chunk budget, the pak's own size passes.
        let arena_disk = chunks[1]["disk_bytes"].as_u64().expect("disk bytes");
        let largest = chunks
            .iter()
            .filter_map(|c| c["disk_bytes"].as_u64())
            .max()
            .expect("sizes");
        let total: u64 = chunks.iter().filter_map(|c| c["disk_bytes"].as_u64()).sum();
        let gate = |max_size: Option<u64>, max_total: Option<u64>| {
            super::super::assets_budget::gate(&AssetsChunksArgs {
                project: root.clone(),
                platform: Some("linux".into()),
                json: true,
                max_size: max_size.map(|b| b.to_string()),
                max_total: max_total.map(|b| b.to_string()),
                measure: super::super::assets_budget::Measure::Disk,
            })
            .expect("gate")
        };
        let (json_text, over) = gate(Some(arena_disk - 1), None);
        assert!(over);
        let report: Value = serde_json::from_str(&json_text).expect("json");
        assert!(
            report["over_budget"]
                .as_array()
                .expect("list")
                .contains(&json!("arena")),
            "{report}"
        );
        assert_eq!(report["chunks"][1]["bytes"], json!(arena_disk));
        assert!(!gate(Some(largest), Some(total)).1);
        let (json_text, over) = gate(None, Some(total - 1));
        assert!(over);
        let report: Value = serde_json::from_str(&json_text).expect("json");
        assert_eq!(report["total"]["over"], json!(true));
        let text = chunks_report(&AssetsChunksArgs { json: false, ..args }).expect("text");
        assert!(text.contains("arena") && text.contains("on_demand"), "{text}");
        let why = why_report(&AssetsWhyArgs {
            asset: "levels/arena/a.glb".into(),
            project: root.clone(),
            platform: Some("linux".into()),
            json: true,
        })
        .expect("why");
        let why: Value = serde_json::from_str(&why).expect("json");
        assert_eq!(
            (why["chunk"].as_str(), why["reason"].as_str()),
            (Some("arena"), Some("rule"))
        );
        let missing = why_report(&AssetsWhyArgs {
            asset: "nope.png".into(),
            project: root.clone(),
            platform: Some("linux".into()),
            json: false,
        })
        .expect_err("unknown");
        assert!(missing.to_string().contains("asset_not_found"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Row C-03 (ADR-8092) end to end through the CLI: with `content = "closure"` the cook writes
    /// no pak for an asset no entry point reaches, the chunk map leaves it out, and `regia assets
    /// chunks` / `regia assets why` name it as unshipped.
    #[test]
    fn closure_cook_ships_no_unreferenced_asset() {
        let rules =
            "[cook.chunk_options]\ncontent = \"closure\"\n[cook.chunks.base]\nentry = [\"ui/menu.png\"]\n";
        let (root, cache, ids) = project("cli-content-closure", &format!("{rules}{ARENA}"));
        let pak_dir = root.join("build/paks/linux");
        let paks = pack_project_chunks(&root, &cache, &pak_dir, "linux", true, false).expect("pack");
        assert_eq!(
            paks,
            [pak_dir.join("base.fpak")],
            "the arena chunk has no shipped asset"
        );
        let map = regia_asset::content::ChunkMap::read_from_dir(&pak_dir)
            .expect("map")
            .expect("present");
        assert_eq!(map.chunk_of(ids[0]), Some(BASE_CHUNK));
        assert_eq!(
            map.chunk_of(ids[1]),
            None,
            "the unreferenced asset is in no chunk"
        );
        // The single base pak lists its members only, not the whole cache.
        let pak = regia_asset::pak::PakReader::open(&pak_dir.join("base.fpak")).expect("open pak");
        let listed: Vec<AssetId> = pak.manifest().iter().map(|e| e.id).collect();
        assert_eq!(listed, [ids[0]]);
        let args = AssetsChunksArgs {
            project: root.clone(),
            platform: Some("linux".into()),
            json: true,
            max_size: None,
            max_total: None,
            measure: super::super::assets_budget::Measure::Disk,
        };
        let value: Value = serde_json::from_str(&chunks_report(&args).expect("report")).expect("json");
        assert_eq!(value["content"], "closure");
        assert_eq!(value["unshipped"], json!(["assets/levels/arena/a.glb"]));
        let text = chunks_report(&AssetsChunksArgs { json: false, ..args }).expect("text");
        assert!(
            text.contains("unshipped: 1 asset(s)") && text.contains("assets/levels/arena/a.glb"),
            "{text}"
        );
        let why = |json: bool| {
            why_report(&AssetsWhyArgs {
                asset: "levels/arena/a.glb".into(),
                project: root.clone(),
                platform: Some("linux".into()),
                json,
            })
            .expect("why")
        };
        let answer: Value = serde_json::from_str(&why(true)).expect("json");
        assert_eq!(answer["reason"], "unshipped");
        assert_eq!(answer["chunk"], Value::Null);
        assert!(why(false).contains("ships in no chunk"), "{}", why(false));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_base_reference_into_a_chunk_fails_the_cook() {
        let (root, cache, ids) = project("cli-content-baseref", ARENA);
        // The menu (base) depends on the arena asset: rewrite the registry with that edge.
        let mut registry = project_registry(&cache).expect("registry");
        let menu = registry.by_id(ids[0]).cloned().expect("menu");
        registry.apply_changes([regia_asset::registry::RegistryChange::Upsert(RegistryEntry {
            dependencies: vec![ids[1]],
            ..menu
        })]);
        registry.write_to(&cache_registry_path(&cache)).expect("registry");
        let err =
            pack_project_chunks(&root, &cache, &root.join("paks"), "linux", false, false).expect_err("ref");
        assert!(err.to_string().contains("chunk reference check failed"), "{err}");
        assert!(
            err.to_string().contains("chunk_base_reference: base asset"),
            "the reason code reaches the CLI user: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn android_pack_limits_fail_the_cook() {
        let rules = "[cook.chunks.a]\ninclude = [\"ui/**\"]\ndelivery = \"fast_follow\"\n\
                     [cook.chunks.b]\ninclude = [\"levels/**\"]\ndelivery = \"fast_follow\"\n";
        let (root, _, _) = project("cli-content-android", rules);
        let cache = cooked_cache_dir(&root, "android");
        std::fs::create_dir_all(&cache).expect("cache");
        for entry in std::fs::read_dir(cooked_cache_dir(&root, "linux")).expect("linux cache") {
            let entry = entry.expect("entry");
            std::fs::copy(entry.path(), cache.join(entry.file_name())).expect("copy");
        }
        let err = pack_project_chunks(&root, &cache, &root.join("paks"), "android", false, false)
            .expect_err("limit");
        assert!(err.to_string().starts_with("pack_limit"), "{err}");
        // The same rules are fine on desktop (no store packs there).
        let cache_linux = cooked_cache_dir(&root, "linux");
        pack_project_chunks(
            &root,
            &cache_linux,
            &root.join("paks_linux"),
            "linux",
            false,
            false,
        )
        .expect("desktop");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The project's `[content] android_pack_max_size` reaches the Android cook: a pack larger than
    /// it fails with `pack_limit` naming the pack; the same project cooks for desktop (ADR-7236).
    #[test]
    fn android_pack_size_caps_fail_the_cook() {
        let rules = "[cook.chunks.arena]\ninclude = [\"levels/**\"]\ndelivery = \"on_demand\"\n\
                     [content]\nandroid_pack_max_size = 1\n";
        let (root, _, _) = project("cli-content-android-size", rules);
        let cache = cooked_cache_dir(&root, "android");
        std::fs::create_dir_all(&cache).expect("cache");
        for entry in std::fs::read_dir(cooked_cache_dir(&root, "linux")).expect("linux cache") {
            let entry = entry.expect("entry");
            std::fs::copy(entry.path(), cache.join(entry.file_name())).expect("copy");
        }
        let err = pack_project_chunks(&root, &cache, &root.join("paks"), "android", false, false)
            .expect_err("size cap");
        let text = err.to_string();
        assert!(text.starts_with("pack_limit"), "{text}");
        assert!(text.contains("arena") && text.contains("pack size"), "{text}");
        let cache_linux = cooked_cache_dir(&root, "linux");
        pack_project_chunks(
            &root,
            &cache_linux,
            &root.join("paks_linux"),
            "linux",
            false,
            false,
        )
        .expect("desktop has no store caps");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn content_command_parses() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            command: ContentCommand,
        }
        assert!(Cli::try_parse_from(["t", "dlc", "--name", "island", "--against", "/base"]).is_ok());
        assert!(
            Cli::try_parse_from(["t", "dlc", "--name", "island"]).is_err(),
            "--against is required"
        );
        assert!(Cli::try_parse_from(["t", "serve", "dist/linux/chunks", "--port", "0"]).is_ok());
    }
}
