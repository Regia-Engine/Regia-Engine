// SPDX-License-Identifier: MPL-2.0
//! `regia cook` — headless asset cook (specs/12 § 6) with automatic pak generation.

use std::path::PathBuf;

use clap::Args;
use regia_asset_pipeline::cook_cache::{self, SharedCacheChoice, SharedCacheConfig, SharedCacheMode};
use regia_asset_pipeline::{
    CookOptions, cook_project_with_shared_cache, gpu_meshes_for, host_platform, texture_stats,
};

use crate::commands::ship_check;

/// Cook project assets for a platform.
#[derive(Debug, Args)]
pub struct CookArgs {
    /// Project directory (contains `assets/` and `Regia.toml`).
    #[arg(long, default_value = ".", global = true)]
    pub project: PathBuf,
    /// Target platform (`linux`, `windows`, `android`). Defaults to host.
    #[arg(long, global = true)]
    pub platform: Option<String>,
    /// `fast` writes uncompressed paks. `final` (default) compresses them and first runs the
    /// provenance-policy gate, plus the unreviewed-asset gate when the manifest requires it.
    #[arg(long, default_value = "final")]
    pub quality: String,
    /// Delete all cooked output and rebuild from scratch.
    #[arg(long)]
    pub clean: bool,
    /// Print the canonical reference budget (4096×4096 heightfield, 8192×8192 macro, two masks,
    /// six 4096×4096 layers, two maps), then this project's cooked terrain cache statistics and
    /// the cooked textures' formats per family (ASTC by block size, BC, uncompressed), then exit.
    #[arg(long)]
    pub stats: bool,
    /// With `--stats`: print only the texture format report, as JSON (`{ "textures": … }`).
    /// With `--size-report`: print the size report as JSON (schema 1). With `cache prune` or
    /// `cache stats`: print the result as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Do not cook: measure every image of the project (standalone or in a glTF) as the desktop cook (BCn),
    /// the Android ASTC cook (at the project's `[cook.textures.android]` profile) and ETC2 / EAC
    /// would write it: chain bytes and level-0 PSNR per texture, with totals (ADR-2063).
    #[arg(long, conflicts_with_all = ["stats", "ship", "clean", "gpu_meshes", "pack_report"])]
    pub size_report: bool,
    /// Write cooked packs (ADR-1594): desktop meshes GPU-ready (as `--gpu-meshes`), blobs lz4 saves
    /// less than 15 % of stored raw (block-compressed textures load as mapping views, ADR-7971), then inspect
    /// every written pak and fail when an entry would be transcoded or cannot load at runtime
    /// (a texture the platform cannot sample as stored, a supercompressed KTX2, a legacy mesh).
    /// Writes `cooked_pack.json` next to the paks.
    #[arg(long, conflicts_with = "stats")]
    pub ship: bool,
    /// Store the meshes of desktop paks GPU-ready (the renderer's vertices, every LOD: no decode at
    /// load), as `REGIA_PAK_GPU_MESHES=1` does. Android paks keep the compact form.
    #[arg(long, conflicts_with = "stats")]
    pub gpu_meshes: bool,
    /// Do not cook: inspect the platform's existing paks (`build/paks/<platform>`) and print the
    /// cooked-pack report as JSON; exits non-zero when a pak has an error finding.
    #[arg(long, conflicts_with_all = ["stats", "ship", "clean", "gpu_meshes"])]
    pub pack_report: bool,
    /// Use this shared cook cache (ADR-2612): a directory, local or a mounted network share, or
    /// an `http://` cache server, that every machine of the team points at. Sources whose cooked outputs it holds are fetched
    /// instead of imported; what this cook imports is published (unless `--read-only`). Replaces
    /// the project's `[cook.shared_cache]`.
    #[arg(long, value_name = "PATH", global = true)]
    pub shared_cache: Option<PathBuf>,
    /// Use the shared cook cache (`--shared-cache` or the project's) for reading only: never
    /// create, touch or replace a file in it.
    #[arg(long)]
    pub read_only: bool,
    /// Do not use the project's shared cook cache for this cook.
    #[arg(long)]
    pub no_shared_cache: bool,
    /// After the cook, derive every lightmap's package tiers from the bake (ADR-3120; the tiers of
    /// `[lightmaps.tiers]` over the default ULTRA / HIGH / LOW / ULTRA_LOW table): writes each tier's
    /// cooked lightmap under the cooked cache's `lightmap_tiers/<tier>/` and the report
    /// `lightmap_tiers.json` (per lightmap and tier: density, encoding, bytes, budget verdict, seams),
    /// and prints the per-tier totals (with `--json`, the report on stdout).
    #[arg(long, conflicts_with_all = ["stats", "size_report", "pack_report"])]
    pub lightmap_tiers: bool,
    /// Before cooking, write the `.meta` sidecars this `.assetids.ron` manifest records that are
    /// missing (`regia assets ids import`, ADR-8460), so sources not in version control keep their
    /// ids; a sidecar that names another id stops the cook (`asset_id_conflict`).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["stats", "size_report", "pack_report"])]
    pub ids: Option<PathBuf>,
    /// Derive alternative pak directories from cached texture mips, e.g. LOW=1024, HIGH=2048,
    /// ULTRA=0 (full chain). Repeatable; writes texture_tiers/<NAME>/ beside the ordinary paks.
    #[arg(long = "texture-tier", value_name = "NAME=MAX_EDGE_PX", conflicts_with_all = ["stats", "size_report", "pack_report"])]
    pub texture_tier: Vec<regia_asset_pipeline::texture_tiers::TextureTier>,
    /// `cache prune | stats`: maintain the shared cook cache instead of cooking.
    #[command(subcommand)]
    pub action: Option<CookAction>,
}

/// `regia cook <action>`.
#[derive(Debug, clap::Subcommand)]
pub enum CookAction {
    /// The shared cook cache (ADR-2612): `--shared-cache PATH` or the project's
    /// `[cook.shared_cache]`.
    #[command(subcommand)]
    Cache(CacheAction),
}

/// `regia cook cache <action>`.
#[derive(Debug, clap::Subcommand)]
pub enum CacheAction {
    /// Remove least-recently-used entries until the cache holds at most `--max-size`, and
    /// unfinished writes older than an hour.
    Prune {
        /// Size limit: bytes, or a number with K, M, G or T (binary), e.g. `500G`.
        #[arg(long)]
        max_size: String,
    },
    /// Print the cache's entry count and size, and the last cook's shared-cache summary for the
    /// platform.
    Stats,
}

/// Whether the paks of `platform` store meshes GPU-ready: `--gpu-meshes` or `--ship`, or the
/// `REGIA_PAK_GPU_MESHES` variable (`env_on`); never for Android.
///
/// # Errors
///
/// `--gpu-meshes` was asked for an Android cook.
fn gpu_meshes_choice(args: &CookArgs, platform: &str, env_on: bool) -> anyhow::Result<bool> {
    let android = platform == "android";
    if android && args.gpu_meshes {
        anyhow::bail!("--gpu-meshes is a desktop option: Android paks keep compact meshes");
    }
    Ok(!android && (args.gpu_meshes || args.ship || env_on))
}

/// The paks' `PakWriteOptions::min_ratio`: `--ship` stores a blob raw when lz4 saves less than
/// `SHIP_MIN_RATIO` of it (block-compressed textures then load as mapping views, ADR-7971); other
/// cooks keep every blob lz4 shrinks at all.
fn pak_min_ratio(args: &CookArgs) -> f32 {
    if args.ship {
        regia_asset_pipeline::pak_writer::SHIP_MIN_RATIO
    } else {
        0.0
    }
}

/// Errors that must abort packaging vs. benign ones.
///
/// `unsupported source extension` means the project holds a non-cookable file
/// (README, notes, `.txt` data the game reads through `assets://`, not a pak
/// asset). Those are skipped with a warning — the shipped package simply does
/// not contain them, matching the pak format (cooked assets only,
/// `docs/specs/12-assets.md` § 5). Everything else is fatal.
fn benign_cook_error(err: &str) -> bool {
    err.starts_with("unsupported source extension")
}

/// Print a cook report: benign skips as warnings, real errors as failures.
/// Returns `Err` when any non-benign error occurred.
pub(crate) fn report_cook_outcome(report: &regia_asset_pipeline::CookReport) -> anyhow::Result<()> {
    let mut fatal = 0usize;
    let mut skipped_files = 0usize;
    for (path, err) in &report.errors {
        if benign_cook_error(err) {
            skipped_files += 1;
            eprintln!("  skip {path}: {err} (not a cookable asset; not shipped in the pak)");
        } else {
            fatal += 1;
            eprintln!("  {path}: {err}");
        }
    }
    for warning in &report.warnings {
        eprintln!("  warn {warning}");
    }
    if let Some(shared) = &report.shared_cache {
        eprintln!("cook: {}", shared.line());
    }
    if report.overlay_fill.configured {
        eprintln!(
            "cook: overlay fill {} file(s), {} bytes from {} root(s)",
            report.overlay_fill.files,
            report.overlay_fill.bytes,
            report.overlay_fill.roots.len()
        );
    }
    eprintln!(
        "cook: {} cooked, {} skipped, {} errors, {} non-cookable files skipped ({:.1} ms; \
         {} skipped without import, {:.1} MiB hashed)",
        report.cooked,
        report.skipped,
        fatal,
        skipped_files,
        report.elapsed_ms,
        report.imports_skipped,
        report.hashed_bytes as f64 / (1024.0 * 1024.0),
    );
    // Bytes before and after per font, and over all of them (ADR-2697).
    for font in &report.font_subsets {
        eprintln!("  font {}", regia_asset_pipeline::font_subset::summary_line(font));
    }
    if !report.font_subsets.is_empty() {
        let before: u64 = report.font_subsets.iter().map(|f| f.bytes_before).sum();
        let after: u64 = report.font_subsets.iter().map(|f| f.bytes_after).sum();
        let subset = report.font_subsets.iter().filter(|f| f.subset).count();
        eprintln!(
            "cook: {} font(s), {subset} subset: {before} -> {after} bytes",
            report.font_subsets.len()
        );
    }
    // What each sound ships as, and its bytes before and after (ADR-3230).
    for sound in &report.audio_ship {
        match sound.quality {
            Some(q) => eprintln!(
                "  audio {}: vorbis q{q:.2}, {} -> {} bytes",
                sound.path, sound.source_bytes, sound.shipped_bytes
            ),
            None => eprintln!("  audio {}: keep, {} bytes", sound.path, sound.source_bytes),
        }
    }
    if report.audio_ship.iter().any(|s| s.mode == "vorbis") {
        let before: u64 = report.audio_ship.iter().map(|s| s.source_bytes).sum();
        let after: u64 = report.audio_ship.iter().map(|s| s.shipped_bytes).sum();
        eprintln!(
            "cook: {} sound(s), {before} -> {after} bytes shipped",
            report.audio_ship.len()
        );
    }
    // The permutation report (master plan § 5.3 item 13, ADR-2026); `regia package` fails a
    // shipping package over budget.
    for line in &report.permutations {
        eprintln!("  permutations {line}");
    }
    for line in &report.material_graph_checks {
        eprintln!("  material graphs {line}");
    }
    for problem in &report.permutation_problems {
        eprintln!("  warn permutation budget exceeded: {problem}");
    }
    if let Some(cost) = &report.shader_cost {
        eprint!("{}", format_shader_cost(cost));
    }
    if !report.graph_permutations.is_empty() {
        eprintln!(
            "cook: {} forward shader permutation(s) from material graphs (budget {} per process)",
            report.graph_permutations.len(),
            report.permutation_budget,
        );
    }
    if fatal > 0 {
        anyhow::bail!("cook finished with {fatal} errors");
    }
    Ok(())
}

/// The platform tag of a cook: `--platform` (or the host's), trimmed and lowercased, then
/// validated. `cook_project` lowercases its own copy, so every path and check here must use the
/// same form: `--platform Android --ship` cooked into `.regia/cooked/android` but packed the empty
/// `.regia/cooked/Android` and passed the ship check with no entries (audit-seq High).
fn platform_tag(args: &CookArgs) -> anyhow::Result<String> {
    let given = match args.platform.as_deref() {
        Some(given) => given,
        None => host_platform(),
    };
    let platform = given.trim().to_ascii_lowercase();
    validate_platform(&platform)?;
    Ok(platform)
}

/// Reject platform tags that are not a single safe path segment: the tag is joined into
/// `.regia/cooked/<tag>` and `build/paks/<tag>` (and `--clean` deletes the former), so `..`
/// or separators must never get through.
fn validate_platform(platform: &str) -> anyhow::Result<()> {
    let ok = !platform.is_empty()
        && platform
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        anyhow::bail!(
            "--platform must be a short tag of ascii letters, digits, `-` and `_` (e.g. `linux`), got `{platform}`"
        );
    }
    Ok(())
}

/// The shared cook cache a cook uses: `--no-shared-cache`, `--shared-cache PATH` (read-write
/// unless `--read-only`), else the project's, read only with `--read-only`.
fn shared_cache_choice(args: &CookArgs) -> anyhow::Result<SharedCacheChoice> {
    let mode = if args.read_only {
        SharedCacheMode::Read
    } else {
        SharedCacheMode::ReadWrite
    };
    // Checked here, not by clap: `--shared-cache` is global (also after `cache`), where
    // `--no-shared-cache` does not exist.
    if args.no_shared_cache && args.shared_cache.is_some() {
        anyhow::bail!("--shared-cache and --no-shared-cache contradict each other");
    }
    Ok(if args.no_shared_cache {
        SharedCacheChoice::Off
    } else if let Some(path) = &args.shared_cache {
        SharedCacheChoice::Use(SharedCacheConfig::new(path, mode))
    } else if args.read_only {
        match SharedCacheConfig::for_project(&args.project).map_err(anyhow::Error::msg)? {
            Some(config) => SharedCacheChoice::Use(SharedCacheConfig {
                mode: SharedCacheMode::Read,
                ..config
            }),
            None => anyhow::bail!("--read-only needs --shared-cache or a [cook.shared_cache] in Regia.toml"),
        }
    } else {
        SharedCacheChoice::Project
    })
}

/// `regia cook cache prune | stats` (ADR-2612).
fn run_cache_action(args: &CookArgs, action: &CacheAction) -> anyhow::Result<()> {
    let config = match &args.shared_cache {
        Some(path) => SharedCacheConfig::new(path, SharedCacheMode::Read),
        None => SharedCacheConfig::for_project(&args.project)
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no shared cook cache: pass --shared-cache PATH or set [cook.shared_cache] in Regia.toml"
                )
            })?,
    };
    match action {
        CacheAction::Prune { max_size } => {
            let max = cook_cache::parse_size(max_size).map_err(anyhow::Error::msg)?;
            let backend = cook_cache::open_backend(&SharedCacheConfig {
                mode: SharedCacheMode::ReadWrite,
                ..config
            })
            .map_err(anyhow::Error::msg)?;
            let report = cook_cache::prune(backend.as_ref(), max)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "shared cook cache {}: removed {} of {} entries ({:.1} of {:.1} MiB), {} stale temp files, {} could not be removed",
                    backend.location(),
                    report.removed,
                    report.entries_before,
                    report.bytes_removed as f64 / (1024.0 * 1024.0),
                    report.bytes_before as f64 / (1024.0 * 1024.0),
                    report.stale_temps_removed,
                    report.failed,
                );
            }
        }
        CacheAction::Stats => {
            // Stats only read: never create the layout of a `read_write` cache.
            let backend = cook_cache::open_backend(&SharedCacheConfig {
                mode: SharedCacheMode::Read,
                ..config
            })
            .map_err(anyhow::Error::msg)?;
            let (entries, bytes) = cook_cache::cache_totals(backend.as_ref())?;
            let platform = platform_tag(args)?;
            let last = cook_cache::last_summary(&regia_asset::cooked_cache_dir(&args.project, &platform));
            if args.json {
                let value = serde_json::json!({
                    "location": backend.location(),
                    "entries": entries,
                    "bytes": bytes,
                    "engine_build_id": cook_cache::engine_build_id(),
                    "platform": platform,
                    "last_cook": last,
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!(
                    "shared cook cache {}: {entries} entries, {:.1} MiB",
                    backend.location(),
                    bytes as f64 / (1024.0 * 1024.0)
                );
                match last {
                    Some(summary) => println!("last {platform} cook: {}", summary.line()),
                    None => println!("last {platform} cook: no shared cache used"),
                }
            }
        }
    }
    Ok(())
}

/// The shipped forward-shader cost section of `regia cook --size-report`: one line per profile.
/// The compile cost is an estimate and the naga time a measurement; the lines say so.
#[must_use]
pub fn format_shader_cost(report: &regia_render::ShaderCostReport) -> String {
    let mut out = format!("shipped forward shaders ({}):\n", report.platform);
    for row in &report.profiles {
        out.push_str(&format!("  {}\n", row.summary_line()));
    }
    out
}

/// Run the cook.
pub fn run(args: &CookArgs) -> anyhow::Result<()> {
    regia_asset_pipeline::texture_tiers::validate_texture_tiers(&args.texture_tier)?;
    if args.action.is_some() && !args.texture_tier.is_empty() {
        anyhow::bail!("texture_tier_invalid: --texture-tier requires a cook, not a cache action");
    }
    if let Some(CookAction::Cache(action)) = &args.action {
        return run_cache_action(args, action);
    }
    if args.size_report {
        let report = regia_asset_pipeline::texture_size_report::texture_size_report(&args.project)
            .map_err(anyhow::Error::msg)?;
        // The shipped forward-shader cost of the platform's profiles (ADR-2664).
        let platform = platform_tag(args)?;
        let mut warnings = Vec::new();
        let shaders = regia_asset_pipeline::permutation_cook::project_shader_cost(
            &args.project,
            &platform,
            &mut warnings,
        );
        for warning in &warnings {
            eprintln!("  warn {warning}");
        }
        if args.json {
            let mut value = serde_json::to_value(&report)?;
            if let Some(object) = value.as_object_mut() {
                object.insert("shaders".to_string(), serde_json::to_value(&shaders)?);
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            print!(
                "{}",
                regia_asset_pipeline::texture_size_report::format_size_report(&report)
            );
            print!("{}", format_shader_cost(&shaders));
        }
        return Ok(());
    }
    // Terrain-data budget report: computed from the canonical DO27 map dimensions (spec 26 § 1)
    // and independent of the project's actual assets. Recorded in docs/evidence/terrain-data/budgets.md.
    if args.stats && args.json {
        let platform = platform_tag(args)?;
        let cache_dir = regia_asset::cooked_cache_dir(&args.project, &platform);
        let stats = texture_stats::texture_format_stats(&cache_dir, &platform).map_err(anyhow::Error::msg)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "textures": stats }))?
        );
        return Ok(());
    }
    if args.stats {
        let platform = platform_tag(args)?;
        // Block sizes follow the platform: Android counts every slot at ASTC 4x4's 16 bytes per
        // block (ADR-5294), desktop counts AO and height at BC4's 8.
        let params = regia_asset_pipeline::TerrainMapParams::default();
        let budget = regia_asset_pipeline::terrain_map_budget_for_platform(&params, &platform);
        println!(
            "{}",
            regia_asset_pipeline::format_budget_report_for_platform(&params, &budget, &platform)
        );
        // Actual tiled sizes, per slot and per map, from the cooked cache.
        let cache_dir = regia_asset::cooked_cache_dir(&args.project, &platform);
        match regia_asset_pipeline::format_terrain_stats(&cache_dir) {
            Ok(Some(stats)) => println!("{stats}"),
            Ok(None) => eprintln!("stats: no cooked terrain in {}", cache_dir.display()),
            Err(e) => anyhow::bail!("stats: failed to read terrain stats: {e}"),
        }
        match texture_stats::texture_format_stats(&cache_dir, &platform) {
            Ok(Some(stats)) => print!("{}", texture_stats::format_texture_stats(&stats)),
            Ok(None) => eprintln!("stats: no cooked textures in {}", cache_dir.display()),
            Err(e) => eprintln!("stats: failed to read texture stats: {e}"),
        }
        return Ok(());
    }

    let value = cook_and_pack(args)?;
    if args.plain_json() {
        crate::json_out::emit(&value);
    }
    Ok(())
}

impl CookArgs {
    /// A plain cook with `--json` (no report flag, no `cache` action): the cook prints one compact
    /// result object on stdout and everything else on stderr (spec 21 § 1, ADR-7217).
    #[must_use]
    pub fn plain_json(&self) -> bool {
        self.json
            && self.action.is_none()
            && !self.stats
            && !self.size_report
            && !self.lightmap_tiers
            && !self.pack_report
    }
}

/// The cook and pack of [`run`] past the report-only flags; answers the `--json` result object of
/// a plain cook: `ok`, `platform`, `quality`, `texture_quality`, `pak_dir`, `paks`, `cooked`,
/// `skipped`, `imports_skipped`, `warnings`, `elapsed_ms`, `lint` and `overlay_fill`
/// (`Value::Null` for `--pack-report`).
fn cook_and_pack(args: &CookArgs) -> anyhow::Result<serde_json::Value> {
    let platform = platform_tag(args)?;
    let pak_dir = args.project.join("build").join("paks").join(&platform);

    if args.pack_report {
        let report = ship_check::cooked_pack_report(&ship_check::paks_in(&pak_dir)?, &platform)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.errors > 0 {
            anyhow::bail!("{} cooked-pack error finding(s)", report.errors);
        }
        return Ok(serde_json::Value::Null);
    }

    // Validate quality.
    if args.quality != "fast" && args.quality != "final" {
        anyhow::bail!("--quality must be `fast` or `final`, got `{}`", args.quality);
    }
    // `--ids` (ADR-8460): the missing sidecars of sources not in version control get their
    // recorded ids before anything reads a `.meta`; a conflicting sidecar stops the cook here.
    if let Some(file) = &args.ids {
        let report = super::assets_ids::import_file(&args.project, file, false)?;
        eprintln!("{}", super::assets_ids::import_summary(&report));
    }
    // `[provenance] require_review_for_shipping` (specs/23 § 5, ADR-2346): a final cook refuses
    // unreviewed agent- and generator-made assets.
    if args.quality == "final" {
        super::review_gate::check(&args.project)?;
    }
    // `[kinds] strictness` (spec 101 § 5): warn prints the kind lint, deny stops the cook.
    super::describe::cook_kind_lint(&args.project)?;

    // Checked before `--clean` deletes anything.
    let shared = shared_cache_choice(args)?;

    // Clean if requested.
    if args.clean {
        let cache_dir = regia_asset::cooked_cache_dir(&args.project, &platform);
        if cache_dir.is_dir() {
            tracing::info!("cleaning {}", cache_dir.display());
            std::fs::remove_dir_all(&cache_dir)?;
        }
    }

    // Source diagnostics run before the cooked cache is published (ADR-8390).
    let lint = super::assets_validate::enforce_publish_lint(&args.project, false)?;

    // The pak directory is rewritten below: until the pack finishes it is no fresh pack for
    // `regia package` (ADR-7413).
    super::package::remove_pack_stamp(&pak_dir)?;
    let mut cook_report = cook_project_with_shared_cache(
        CookOptions {
            project_root: args.project.clone(),
            platform: platform.clone(),
            incremental: !args.clean,
        },
        &shared,
    )?;
    // The shipped forward-shader cost per profile (ADR-2664): composing takes seconds, so the
    // library cook leaves it to the command.
    cook_report.attach_shader_cost(&args.project, &platform);

    report_cook_outcome(&cook_report)?;
    crate::commands::plugin::cook_extensions(&args.project, &platform, !args.clean)?;

    // Validate the cooked registry (spec 32 § 4.5; `cook: Off | Warn | Fail` in config/validation.ron).
    super::assets_validate::cook_validation(&args.project, &platform)?;

    // Generate pak files from cooked assets.
    let cache_dir = regia_asset::cooked_cache_dir(&args.project, &platform);
    let compress = args.quality == "final";
    let gpu_meshes = gpu_meshes_choice(args, &platform, gpu_meshes_for(&cache_dir))?;
    let mut paks = pack_paks(
        &args.project,
        &platform,
        &cache_dir,
        &pak_dir,
        (compress, pak_min_ratio(args)),
        gpu_meshes,
    )?;

    // Tier paks follow the ordinary paks' lz4 minimum saving (ADR-7971, ADR-8574).
    let tier_paks = regia_asset_pipeline::texture_tiers::write_texture_tier_paks_with(
        &pak_dir,
        &args.texture_tier,
        compress,
        pak_min_ratio(args),
    )?;
    for pak in &tier_paks {
        eprintln!("texture tier pak: {}", pak.display());
    }
    paks.extend(tier_paks);

    crate::commands::plugin::write_cooked_record(&args.project, &platform, &pak_dir)?;
    // The stamp records the options: a fast or GPU-mesh pack is no package pack (ADR-7612).
    super::package::write_pack_stamp_with(
        &args.project,
        &pak_dir,
        super::package::PackOptions { compress, gpu_meshes },
    )?;

    if args.lightmap_tiers {
        cook_lightmap_tiers(&args.project, &platform, &cache_dir, args.json)?;
    }

    if args.ship {
        // Write the report before judging it, so a failed check leaves the evidence on disk.
        let report = ship_check::cooked_pack_report(&paks, &platform)?;
        write_pack_report(&pak_dir, &report)?;
        eprint!("{}", report.to_text());
        if report.errors > 0 {
            anyhow::bail!(
                "cooked-pack check failed: {} error finding(s) (see {})",
                report.errors,
                pak_dir.join(PACK_REPORT_FILE).display()
            );
        }
    }

    let texture_quality = regia_asset_pipeline::ProjectTextureQuality::for_project(&args.project)
        .unwrap_or_default()
        .label();
    let absolute = |p: &std::path::Path| {
        std::path::absolute(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    Ok(serde_json::json!({
        "ok": true,
        "platform": platform,
        "quality": args.quality,
        "texture_quality": texture_quality,
        "pak_dir": absolute(&pak_dir),
        "paks": paks.iter().map(|p| absolute(p)).collect::<Vec<_>>(),
        "cooked": cook_report.cooked,
        "skipped": cook_report.skipped,
        "imports_skipped": cook_report.imports_skipped,
        "warnings": cook_report.warnings.len(),
        "elapsed_ms": cook_report.elapsed_ms,
        "lint": lint,
        "overlay_fill": {
            "configured": cook_report.overlay_fill.configured,
            "files": cook_report.overlay_fill.files,
            "bytes": cook_report.overlay_fill.bytes,
        },
    }))
}

/// True when `Manifest::load` failed because no manifest file is there. A file that exists
/// but cannot be read, or that does not validate, is a different error: cooking must not
/// replace it with the default tier table.
pub(crate) fn manifest_file_is_missing(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// `regia cook --lightmap-tiers` (ADR-3120): every cooked lightmap's package tiers, the report
/// `lightmap_tiers.json` beside the cooked cache, and the per-tier totals (stderr; with `json`, the
/// report on stdout).
///
/// # Errors
///
/// The project manifest is present but cannot be read or does not validate (a bad
/// `[lightmaps]` value is not replaced by the default tier table), a bad `[lightmaps.tiers]`
/// table, or a lightmap that does not read or derive. A missing manifest still uses the
/// default tier table.
pub fn cook_lightmap_tiers(
    project: &std::path::Path,
    platform: &str,
    cache_dir: &std::path::Path,
    json: bool,
) -> anyhow::Result<serde_json::Value> {
    use regia_asset_pipeline::lightmap_tiers::{self, TierVerdict};
    let lightmaps_config = match crate::manifest::Manifest::load(project) {
        Ok(manifest) => manifest.lightmaps,
        Err(err) if manifest_file_is_missing(&err) => crate::manifest::LightmapsConfig::default(),
        Err(err) => return Err(err),
    };
    let table = lightmaps_config.tier_table()?;
    let manifest = regia_asset::AssetManifest::read_from_cache(cache_dir)
        .map_err(|e| anyhow::anyhow!("reading the cooked manifest in {}: {e}", cache_dir.display()))?
        .unwrap_or_default();
    let lightmaps = lightmap_tiers::cook_project_lightmap_tiers(cache_dir, &manifest, &table, platform)
        .map_err(anyhow::Error::msg)?;
    // Per tier: the bytes the tier package ships, and the lightmaps it leaves out.
    let mut totals: std::collections::BTreeMap<&str, (u64, usize, usize)> = std::collections::BTreeMap::new();
    for l in &lightmaps {
        for t in &l.tiers {
            let slot = totals.entry(t.tier.as_str()).or_default();
            if t.verdict.ships() {
                slot.0 += t.bytes;
                slot.1 += 1;
            } else {
                slot.2 += 1;
            }
            if t.verdict == TierVerdict::OverBudget {
                eprintln!(
                    "  warn lightmap_tier_over_budget: {} at tier {} is {} bytes, over the tier's budget of {} bytes: that tier ships without it",
                    l.path,
                    t.tier,
                    t.bytes,
                    t.budget_bytes.unwrap_or(0)
                );
            }
        }
    }
    let tiers: Vec<serde_json::Value> = totals
        .iter()
        .map(|(tier, (bytes, shipped, left_out))| {
            serde_json::json!({ "tier": tier, "bytes": bytes, "lightmaps": shipped, "left_out": left_out })
        })
        .collect();
    let value = serde_json::json!({
        "schema": 1,
        "platform": platform,
        "report": cache_dir.join(lightmap_tiers::TIER_REPORT_FILE).display().to_string(),
        "tiers": tiers,
        "lightmaps": lightmaps,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        for (tier, (bytes, shipped, left_out)) in &totals {
            eprintln!("lightmap tier {tier}: {bytes} bytes in {shipped} lightmap(s), {left_out} left out");
        }
    }
    Ok(value)
}

/// The cooked-pack report `regia cook --ship` writes next to the paks.
pub const PACK_REPORT_FILE: &str = "cooked_pack.json";

/// Writes `report` as `<pak_dir>/cooked_pack.json` (temp file + rename: a reader never sees half).
fn write_pack_report(pak_dir: &std::path::Path, report: &ship_check::CookedPackReport) -> anyhow::Result<()> {
    let path = pak_dir.join(PACK_REPORT_FILE);
    let tmp = pak_dir.join(format!(".{PACK_REPORT_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(report)?)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Packs the cooked cache into one pak per chunk of the project's `[cook.chunks]` rules (only
/// `base.fpak` without rules) plus `content-manifest.ron` and `chunk-map.bin` (specs/40), meshes
/// GPU-ready when `gpu_meshes`, and returns the paks written. A failure fails the command: a
/// green `regia cook` must mean the paks on disk match the cook.
fn pack_paks(
    project: &std::path::Path,
    platform: &str,
    cache_dir: &std::path::Path,
    pak_dir: &std::path::Path,
    (compress, min_ratio): (bool, f32),
    gpu_meshes: bool,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    // The content line's lz4 min_ratio (ADR-8574 (6)) and the main line's structured error chain
    // (`cook_pack_preserves_structured_registry_errors`): context, not a flattened message.
    let paks = super::content::pack_project_chunks_with_ratio(
        project,
        cache_dir,
        pak_dir,
        platform,
        (compress, min_ratio),
        gpu_meshes,
    )
    .map_err(|e| e.context("pak generation failed"))?;
    eprintln!(
        "pak: wrote {} pak file(s) to {}{}",
        paks.len(),
        pak_dir.display(),
        if gpu_meshes { " (GPU-ready meshes)" } else { "" }
    );
    Ok(paks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset_pipeline::CookReport;

    fn report_with(error: &str) -> CookReport {
        CookReport {
            errors: vec![("assets/a.png".to_owned(), error.to_owned())],
            ..CookReport::default()
        }
    }

    #[test]
    fn final_cook_runs_the_provenance_gate_and_fast_skips_it() {
        let dir = regia_test_utils::unique_temp_dir("cook-final-gate");
        let source = dir.join("assets/agent.material.ron");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("assets");
        std::fs::write(&source, b"x").expect("source");
        let mut meta = regia_asset::MetaSidecar::new("material_ron", regia_asset::AssetId::new_random());
        meta.provenance = Some(regia_asset::Provenance {
            tool: regia_asset::ProvenanceTool::Agent,
            reviewer: regia_asset::ReviewerState::Pending,
            ..regia_asset::Provenance::default()
        });
        let meta_path = regia_asset::meta_path_for(&source);
        std::fs::write(meta_path, meta.to_ron_string().expect("ron")).expect("meta");
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(
            dir.join("config/provenance.ron"),
            "(review: (required_for: [agent]), kinds: { \"*\": (required: [\"tool\"]) })",
        )
        .expect("policy");
        // The contradictory cache flags fail after the gate, so `fast` never cooks.
        let cache_flags = |quality: &str| CookArgs {
            project: dir.clone(),
            quality: quality.into(),
            no_shared_cache: true,
            shared_cache: Some(dir.join("cache")),
            ..args("linux")
        };
        let refused = run(&cache_flags("final")).expect_err("final runs the gate");
        assert!(refused.to_string().contains("provenance_policy"), "{refused}");
        let skipped = run(&cache_flags("fast")).expect_err("fast still hits the cache-flag check");
        let skipped = skipped.to_string();
        assert!(
            skipped.contains("--shared-cache and --no-shared-cache contradict"),
            "{skipped}"
        );
        assert!(!skipped.contains("provenance_policy"), "{skipped}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pak_failure_fails_the_cook() {
        let dir = regia_test_utils::unique_temp_dir("cook_pak_fail");
        std::fs::create_dir_all(&dir).expect("test setup");
        // A regular file where the pak directory should be: the write cannot succeed.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").expect("test setup");
        let err = pack_paks(&dir, "linux", &dir, &blocker.join("paks"), (false, 0.0), false)
            .expect_err("expected an error");
        assert!(err.to_string().contains("pak generation failed"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cook_pack_preserves_structured_registry_errors() {
        let dir = regia_test_utils::unique_temp_dir("cook-pack-integrity-error");
        let path = regia_asset::registry::cache_registry_path(&dir);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, b"broken registry").expect("fixture");
        std::fs::rename(tmp, path).expect("publish fixture");
        let error = pack_paks(&dir, "linux", &dir, &dir.join("paks"), (false, 0.0), false)
            .expect_err("corrupt registry");
        let value = crate::json_out::failure("cook_failed", &error);
        assert_eq!(value["error"]["code"], "content_registry_invalid");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn platform_tag_must_be_a_single_safe_segment() {
        for ok in ["linux", "windows", "android", "macos", "linux-arm64", "x_1"] {
            assert!(validate_platform(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "..",
            "../../../.ssh",
            "a/b",
            "a\\b",
            "/abs",
            ".",
            "a b",
            "a\n",
        ] {
            assert!(validate_platform(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_platform_tag_is_trimmed_and_lowercased_before_any_path_is_built() {
        for (given, tag) in [
            ("Android", "android"),
            (" LINUX ", "linux"),
            ("windows", "windows"),
        ] {
            assert_eq!(platform_tag(&args(given)).expect(given), tag);
        }
        assert!(platform_tag(&args("../Android")).is_err());
        assert!(platform_tag(&args("  ")).is_err());
    }

    fn args(platform: &str) -> CookArgs {
        CookArgs {
            project: ".".into(),
            platform: Some(platform.into()),
            quality: "final".into(),
            clean: false,
            stats: false,
            json: false,
            ship: false,
            gpu_meshes: false,
            pack_report: false,
            size_report: false,
            lightmap_tiers: false,
            ids: None,
            texture_tier: Vec::new(),
            shared_cache: None,
            read_only: false,
            no_shared_cache: false,
            action: None,
        }
    }

    #[test]
    fn gpu_meshes_follow_the_flags_and_never_reach_android() {
        let plain = args("linux");
        assert!(!gpu_meshes_choice(&plain, "linux", false).expect("plain"));
        assert!(gpu_meshes_choice(&plain, "linux", true).expect("env var"));
        let ship = CookArgs {
            ship: true,
            ..args("windows")
        };
        assert!(gpu_meshes_choice(&ship, "windows", false).expect("ship"));
        assert!(!gpu_meshes_choice(&ship, "android", true).expect("android ship keeps compact"));
        let flag = CookArgs {
            gpu_meshes: true,
            ..args("android")
        };
        assert!(gpu_meshes_choice(&flag, "linux", false).expect("flag"));
        let err = gpu_meshes_choice(&flag, "android", false).expect_err("android refuses the flag");
        assert!(err.to_string().contains("desktop"), "{err}");
    }

    /// DO27 COOKED_PACKS_DESIGN pak-min-ratio (ADR-7971): `--ship` writes with the 15 % minimum
    /// lz4 saving, on every platform; a plain cook keeps any saving.
    #[test]
    fn ship_cooks_store_poorly_compressing_blobs_raw() {
        assert_eq!(pak_min_ratio(&args("linux")), 0.0);
        for platform in ["linux", "windows", "android"] {
            let ship = CookArgs {
                ship: true,
                ..args(platform)
            };
            assert_eq!(pak_min_ratio(&ship), 0.15, "{platform}");
        }
    }

    /// `--json` alone is a plain cook since ADR-7217 (one result object, tests/cook_json_cli.rs);
    /// with a report flag it prints that report. `--size-report` measures a project without
    /// cooking it, and refuses a bad `[cook.textures]` table (ADR-2063).
    #[test]
    fn json_alone_is_a_plain_cook_and_the_size_report_runs_without_one() {
        use image::ImageEncoder;
        // `--json` alone is a plain cook with one result object since ADR-7217 (tests/cook_json_cli.rs);
        // with a report flag it prints that report.
        let json_alone = CookArgs {
            json: true,
            ..args("linux")
        };
        assert!(json_alone.plain_json());
        assert!(
            !CookArgs {
                size_report: true,
                ..json_alone
            }
            .plain_json()
        );

        let root = regia_test_utils::unique_temp_dir("cook_size_report");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let pixels: Vec<u8> = (0..16 * 16)
            .flat_map(|i| [i as u8, 128, 255 - i as u8, 255])
            .collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 16, 16, image::ExtendedColorType::Rgba8)
            .expect("png");
        std::fs::write(root.join("assets/tile.png"), png).expect("png");
        let report = CookArgs {
            project: root.clone(),
            size_report: true,
            json: true,
            ..args("linux")
        };
        run(&report).expect("size report");
        assert!(!root.join(".regia").exists(), "the size report cooks nothing");
        std::fs::write(
            root.join("Regia.toml"),
            "[cook.textures.android]\nformat = \"bc\"\n",
        )
        .expect("toml");
        let err = run(&report).expect_err("bad table");
        assert!(err.to_string().contains("format `bc`"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The size report's shader section labels the estimate and the measurement apart (ADR-2664).
    #[test]
    fn the_shader_section_labels_the_estimate_and_the_measurement() {
        let report = regia_render::ShaderCostReport {
            format_version: regia_render::SHADER_COST_REPORT_VERSION,
            platform: "android".into(),
            profiles: vec![regia_render::shader_cost_row(
                "android",
                regia_render::RenderProfilePreset::MobileLow,
                regia_render::ShaderBytesBudget {
                    max_wgsl_bytes: u64::MAX,
                },
                0,
            )],
        };
        let text = format_shader_cost(&report);
        assert!(text.starts_with("shipped forward shaders (android):\n"), "{text}");
        assert!(
            text.contains("MobileLow:") && text.contains("B WGSL") && text.contains("B SPIR-V"),
            "{text}"
        );
        assert!(
            text.contains("(estimate)") && text.contains("(measured)"),
            "{text}"
        );
        assert!(!text.contains("OVER BUDGET"), "{text}");
    }

    #[test]
    fn pack_report_is_written_atomically() {
        let dir = regia_test_utils::unique_temp_dir("cook_pack_report");
        std::fs::create_dir_all(&dir).expect("test setup");
        let report = ship_check::CookedPackReport {
            platform: "linux".into(),
            paks: Vec::new(),
            errors: 0,
            warnings: 0,
        };
        write_pack_report(&dir, &report).expect("write");
        let text = std::fs::read_to_string(dir.join(PACK_REPORT_FILE)).expect("read");
        let json: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(json["platform"], "linux");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [PACK_REPORT_FILE], "no temp file is left behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_cook_errors_are_fatal() {
        let err = report_cook_outcome(&report_with("decode failed")).expect_err("fatal");
        assert!(format!("{err:#}").contains("1 errors"), "{err:#}");
    }

    #[test]
    fn unsupported_extension_is_only_a_skip() {
        report_cook_outcome(&report_with("unsupported source extension .txt")).expect("benign");
        report_cook_outcome(&CookReport::default()).expect("empty report");
    }

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        cook: CookArgs,
    }

    fn parse(words: &[&str]) -> Result<CookArgs, clap::Error> {
        use clap::Parser;
        Wrap::try_parse_from(std::iter::once("cook").chain(words.iter().copied())).map(|w| w.cook)
    }

    /// ADR-2612: `--shared-cache PATH` is read-write unless `--read-only`, `--no-shared-cache`
    /// turns the project's off, and `cache prune | stats` take the path after the action.
    #[test]
    fn the_shared_cache_flags_pick_the_cache() {
        let rw = parse(&["--shared-cache", "/share"]).expect("parse");
        assert_eq!(
            shared_cache_choice(&rw).expect("choice"),
            SharedCacheChoice::Use(SharedCacheConfig::new("/share", SharedCacheMode::ReadWrite))
        );
        let ro = parse(&["--shared-cache", "/share", "--read-only"]).expect("parse");
        assert_eq!(
            shared_cache_choice(&ro).expect("choice"),
            SharedCacheChoice::Use(SharedCacheConfig::new("/share", SharedCacheMode::Read))
        );
        let off = parse(&["--no-shared-cache"]).expect("parse");
        assert_eq!(shared_cache_choice(&off).expect("choice"), SharedCacheChoice::Off);
        assert_eq!(
            shared_cache_choice(&parse(&[]).expect("parse")).expect("choice"),
            SharedCacheChoice::Project
        );
        let both = parse(&["--shared-cache", "/s", "--no-shared-cache"]).expect("parse");
        assert!(shared_cache_choice(&both).is_err());
        let root = regia_test_utils::unique_temp_dir("cook_read_only_without_cache");
        let lone = parse(&["--read-only", "--project", &root.to_string_lossy()]).expect("parse");
        assert!(shared_cache_choice(&lone).is_err(), "--read-only needs a cache");
        std::fs::remove_dir_all(&root).ok();

        let prune =
            parse(&["cache", "prune", "--max-size", "1G", "--shared-cache", "/share"]).expect("parse");
        assert!(matches!(
            prune.action,
            Some(CookAction::Cache(CacheAction::Prune { ref max_size })) if max_size == "1G"
        ));
        assert_eq!(
            prune.shared_cache.as_deref(),
            Some(std::path::Path::new("/share"))
        );
        assert!(parse(&["cache", "stats", "--json"]).expect("parse").json);
    }

    #[test]
    fn cache_prune_and_stats_run_on_a_directory() {
        let share = regia_test_utils::unique_temp_dir("cook_cache_cli_share");
        let backend = cook_cache::DirBackend::open(&share, SharedCacheMode::ReadWrite).expect("open");
        use cook_cache::CookCacheBackend;
        for i in 0..3u8 {
            backend.put(&[i; 32], &[i; 1024]).expect("put");
        }
        let share_arg = share.to_string_lossy().into_owned();
        let stats = parse(&["cache", "stats", "--json", "--shared-cache", &share_arg]).expect("parse");
        run(&stats).expect("stats");
        let prune =
            parse(&["cache", "prune", "--max-size", "2K", "--shared-cache", &share_arg]).expect("parse");
        run(&prune).expect("prune");
        assert_eq!(backend.list().expect("list").len(), 2);
        let bad = parse(&[
            "cache",
            "prune",
            "--max-size",
            "lots",
            "--shared-cache",
            &share_arg,
        ])
        .expect("parse");
        assert!(run(&bad).is_err());
        let root = regia_test_utils::unique_temp_dir("cook_cache_cli_project");
        let none = parse(&["cache", "stats", "--project", &root.to_string_lossy()]).expect("parse");
        let err = run(&none).expect_err("no cache configured");
        assert!(err.to_string().contains("--shared-cache"), "{err}");
        std::fs::remove_dir_all(&share).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A manifest that does not validate is an error of the tier cook. It is not replaced by
    /// the default tier table (which would report a lightmap `Fits` against the wrong budget).
    /// A project with no manifest still uses that default table.
    #[test]
    fn a_bad_lightmap_manifest_is_not_replaced_by_the_default_table() {
        let project = regia_test_utils::unique_temp_dir("cook_bad_lightmap_manifest");
        let cache = project.join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::write(
            project.join("Regia.toml"),
            "[project]\nname = \"Hello\"\nid = \"com.example.hello\"\n[lightmaps]\nseam_budget = nan\n",
        )
        .expect("manifest");
        let err = cook_lightmap_tiers(&project, "linux", &cache, false).expect_err("bad seam budget");
        assert!(format!("{err:#}").contains("seam_budget"), "{err:#}");
        assert!(
            !cache.join("lightmap_tiers.json").exists(),
            "a refused manifest writes no tier report"
        );
        std::fs::write(
            project.join("Regia.toml"),
            "[project]\nname = \"Hello\"\nid = \"com.example.hello\"\n[lightmaps.tiers.Nope]\nbudget_mib = 1\n",
        )
        .expect("bad tier");
        let err = cook_lightmap_tiers(&project, "linux", &cache, false).expect_err("bad tier name");
        assert!(format!("{err:#}").contains("lightmap_tier_rule"), "{err:#}");
        std::fs::remove_file(project.join("Regia.toml")).expect("remove");
        cook_lightmap_tiers(&project, "linux", &cache, false).expect("no manifest uses the defaults");
        std::fs::remove_dir_all(&project).ok();
    }
}
