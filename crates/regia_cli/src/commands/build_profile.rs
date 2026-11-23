// SPDX-License-Identifier: MPL-2.0
//! Named build profiles (`[profiles.<name>]` in `Regia.toml`): the dependency closure of a
//! profile's entry levels, the `--dry-run` size report, the profile's pak, and the staging of a
//! package's content with its `ship_check`.
//!
//! The closure follows two kinds of references:
//!
//! - **Cooked assets** list the assets they depend on in their cooked header
//!   (`CookedHeader::dependencies`); the walk follows them transitively.
//! - **Raw files** (levels, UI layouts, other data read through `assets://`) are read by the root
//!   resolver the chunk cook shares (`regia_asset_pipeline::closure_roots`, CT-25, ADR-8477): UI
//!   layouts through their typed image/mask fields, levels through their decoded string literals,
//!   unknown text formats through a labeled conservative fallback scan. A string that names a raw
//!   file (`levels/b.level.ron`, with or without an `assets/` or `assets://` prefix), a cooked asset
//!   path, an asset id or a level id pulls that file in, and raw files are read in turn. Every root
//!   keeps the reason it ships (`Closure::roots`).
//!
//! Nothing is obfuscated or encrypted: a profile only decides *which* files ship. Anyone can unpack
//! a shipped `.fpak` (docs/specs/21 § 4, "Build profiles").

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_asset::{
    AssetId, AssetManifest, ManifestEntry, cooked_cache_dir, cooked_file_name, decode_cooked_file,
};
use regia_asset_pipeline::closure_roots::{
    self, RootDecl, RootEvidence, RootInputs, RootKind, resolve_roots,
};
use regia_core::id::TypePathHash;

use crate::commands::package::{PackageArgs, Platform, Profile, stage_raw_files};
use crate::commands::server_content::{self, ServerContent};
use crate::commands::ship_check;
use crate::manifest::{BUILTIN_PROFILES, BUILTIN_SERVER_PROFILE, BuildProfile, Manifest};

/// Pak file names a package's `content/` may hold (`ensure_paks` writes one pak, `base.fpak`).
pub const EXPECTED_PAKS: &[&str] = &["base.fpak"];

/// A `--profile` selection resolved against the manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProfile {
    /// The name given on the command line.
    pub name: String,
    /// Cargo profile of the build.
    pub cargo: Profile,
    /// The manifest entry for a named profile; `None` for the built-in `dev` / `shipping` /
    /// `server`, which ship every asset of the project (`server`: through the kind filter).
    pub spec: Option<BuildProfile>,
    /// A dedicated-server build (spec 76 § 3).
    pub server: bool,
}

impl ResolvedProfile {
    /// Whether an agent server may be linked into this profile's shipping binary.
    pub fn allow_agent(&self) -> bool {
        self.spec.as_ref().is_some_and(|s| s.allow_agent)
    }
}

/// Resolves `name` (`dev`, `shipping` or a `[profiles.<name>]` entry).
///
/// # Errors
///
/// The name is neither built in nor defined in the manifest.
pub fn resolve(manifest: &Manifest, name: &str) -> Result<ResolvedProfile> {
    match name {
        "dev" => Ok(ResolvedProfile {
            name: name.into(),
            cargo: Profile::Dev,
            spec: None,
            server: false,
        }),
        "shipping" => Ok(ResolvedProfile {
            name: name.into(),
            cargo: Profile::Shipping,
            spec: None,
            server: false,
        }),
        BUILTIN_SERVER_PROFILE if !manifest.profiles.contains_key(BUILTIN_SERVER_PROFILE) => {
            Ok(ResolvedProfile {
                name: name.into(),
                cargo: Profile::Shipping,
                spec: None,
                server: true,
            })
        }
        other => {
            let Some(spec) = manifest.profiles.get(other) else {
                let mut known: Vec<&str> = BUILTIN_PROFILES.to_vec();
                known.push(BUILTIN_SERVER_PROFILE);
                known.extend(manifest.profiles.keys().map(String::as_str));
                bail!(
                    "unknown build profile `{other}`; known profiles: {} (define `[profiles.{other}]` in Regia.toml)",
                    known.join(", ")
                );
            };
            Ok(ResolvedProfile {
                name: other.into(),
                cargo: if spec.base == "dev" {
                    Profile::Dev
                } else {
                    Profile::Shipping
                },
                server: spec.is_server(),
                spec: Some(spec.clone()),
            })
        }
    }
}

/// Extra `cargo build` arguments a profile adds (`--no-default-features`, `--features`). A server
/// profile always builds `--no-default-features --features server` plus its own features.
pub fn cargo_extra_args(profile: &ResolvedProfile) -> Vec<String> {
    let mut out = Vec::new();
    if profile.server {
        let mut features = vec!["server".to_string()];
        if let Some(spec) = &profile.spec {
            features.extend(spec.features.iter().filter(|f| *f != "server").cloned());
        }
        out.push("--no-default-features".to_string());
        out.push("--features".to_string());
        out.push(features.join(","));
        return out;
    }
    if let Some(spec) = &profile.spec {
        if spec.no_default_features {
            out.push("--no-default-features".to_string());
        }
        if !spec.features.is_empty() {
            out.push("--features".to_string());
            out.push(spec.features.join(","));
        }
    }
    out
}

/// One cooked asset in a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureAsset {
    /// Manifest path (`assets/…`).
    pub path: String,
    /// Asset id.
    pub id: AssetId,
    /// Reflect type path of the primary cooked type.
    pub type_path: String,
    /// Size of the cooked file in bytes.
    pub bytes: u64,
}

/// One raw file (copied verbatim next to the binary) in a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureRaw {
    /// Path under `assets/`, `/`-separated.
    pub path: String,
    /// Size in bytes.
    pub bytes: u64,
}

/// The files a profile ships.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Closure {
    /// Cooked assets that go into the profile's pak, sorted by path then id.
    pub assets: Vec<ClosureAsset>,
    /// Raw files that go into the package's `assets/`, sorted by path.
    pub raw: Vec<ClosureRaw>,
    /// Cooked assets of the project left out, and their cooked size.
    pub excluded_assets: Vec<ClosureAsset>,
    /// Raw files of the project left out.
    pub excluded_raw: Vec<ClosureRaw>,
    /// Why each root of a named profile is in the closure, and how its raw files were scanned
    /// (empty for the built-in profiles, which ship everything).
    pub roots: RootEvidence,
}

impl Closure {
    /// Total cooked bytes shipped.
    pub fn asset_bytes(&self) -> u64 {
        self.assets.iter().map(|a| a.bytes).sum()
    }

    /// Total raw bytes shipped.
    pub fn raw_bytes(&self) -> u64 {
        self.raw.iter().map(|r| r.bytes).sum()
    }

    /// The raw paths as a set (the filter of the raw copy).
    pub fn raw_set(&self) -> BTreeSet<String> {
        self.raw.iter().map(|r| r.path.clone()).collect()
    }
}

/// `assets/x`, `assets://x` and `./x` all name `x` under `assets/`.
pub(super) fn normalize(path: &str) -> String {
    closure_roots::normalize_asset_path(path)
}

/// Glob match over `/`-separated paths: `*` any run without `/`, `?` one character, `**` anything.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    closure_roots::glob_match(pattern, path)
}

/// The root declarations of a named profile: its `entry_levels` and its `include` globs (explicit
/// dynamic roots), named as the root report shows them.
fn profile_root_decls(spec: &BuildProfile) -> Vec<RootDecl> {
    let levels = spec.entry_levels.iter().enumerate().map(|(i, level)| {
        RootDecl::new(
            RootKind::EntryLevel,
            format!("[profiles] entry_levels[{i}]"),
            level.clone(),
        )
    });
    let includes = spec.include.iter().enumerate().map(|(i, glob)| {
        RootDecl::new(
            RootKind::Declared,
            format!("[profiles] include[{i}]"),
            glob.clone(),
        )
    });
    levels.chain(includes).collect()
}

/// Computes the files `spec` ships for `platform`; `None` is the whole project (the built-in
/// profiles). The platform's cooked cache must exist (`regia cook`, or the auto-cook of
/// `regia package`). The roots come from the resolver the chunk cook shares
/// ([`regia_asset_pipeline::closure_roots::resolve_roots`], CT-25, ADR-8477): the entry levels, the
/// startup UI layouts, the levels' bakes and the `include` globs, with the raw files they name.
///
/// # Errors
///
/// No cooked cache, an entry level that is neither a raw file nor a cooked asset, an unreadable raw
/// file, a malformed level or UI layout (`content_root_*`), or an unreadable cooked file.
pub fn compute_closure(project_dir: &Path, platform: &str, spec: Option<&BuildProfile>) -> Result<Closure> {
    let cooked_dir = cooked_cache_dir(project_dir, platform);
    let manifest = AssetManifest::read_from_cache(&cooked_dir)
        .map_err(|e| anyhow::anyhow!("reading the cooked manifest in {}: {e}", cooked_dir.display()))?
        .unwrap_or_default();
    let registry = super::content::project_registry(&cooked_dir)?;
    let raw_all = closure_roots::raw_universe(project_dir).map_err(|e| super::content::root_error(&e))?;

    // Keep missing files in the index: the integrity pass must diagnose them, not lose roots.
    let mut by_id: HashMap<AssetId, &ManifestEntry> = HashMap::new();
    let mut sizes: HashMap<AssetId, u64> = HashMap::new();
    for entry in &manifest.entries {
        let file = cooked_dir.join(cooked_file_name(entry.id));
        if let Ok(meta) = std::fs::metadata(&file) {
            sizes.insert(entry.id, meta.len());
        }
        by_id.insert(entry.id, entry);
    }
    let to_asset = |e: &ManifestEntry| ClosureAsset {
        path: e.path.clone(),
        id: e.id,
        type_path: e.type_path.clone(),
        bytes: sizes.get(&e.id).copied().unwrap_or(0),
    };

    let mut integrity_roots = BTreeSet::new();
    let mut roots = RootEvidence::default();
    let (mut assets, raw): (BTreeMap<AssetId, ClosureAsset>, BTreeMap<String, ClosureRaw>);
    match spec {
        None => {
            assets = by_id.values().map(|e| (e.id, to_asset(e))).collect();
            raw = raw_all
                .iter()
                .map(|(p, b)| {
                    (
                        p.clone(),
                        ClosureRaw {
                            path: p.clone(),
                            bytes: *b,
                        },
                    )
                })
                .collect();
        }
        Some(spec) => {
            let excluded = |path: &str| spec.exclude.iter().any(|g| glob_match(g, path));
            let mut cooked: Vec<(AssetId, String)> =
                manifest.entries.iter().map(|e| (e.id, e.path.clone())).collect();
            cooked.sort();
            let inputs = RootInputs {
                assets_dir: project_dir.join("assets"),
                raw: &raw_all,
                cooked: &cooked,
                exclude: &excluded,
            };
            roots = resolve_roots(&inputs, &profile_root_decls(spec), true)
                .map_err(|e| super::content::root_error(&e))?;
            for decl in &roots.unmatched {
                tracing::warn!(declared_by = %decl, "build profile include glob matches nothing");
            }
            raw = roots
                .raw
                .keys()
                .map(|p| {
                    (
                        p.clone(),
                        ClosureRaw {
                            path: p.clone(),
                            bytes: raw_all.get(p).copied().unwrap_or(0),
                        },
                    )
                })
                .collect();
            assets = BTreeMap::new();
            let mut asset_queue: VecDeque<AssetId> = VecDeque::new();
            for id in roots.cooked.keys() {
                if let Some(e) = by_id.get(id) {
                    integrity_roots.insert(*id);
                    assets.insert(*id, to_asset(e));
                    asset_queue.push_back(*id);
                }
            }
            // Cooked assets name cooked assets: the registry's dependency lists and each cooked
            // header's.
            while let Some(id) = asset_queue.pop_front() {
                let file = cooked_dir.join(cooked_file_name(id));
                let mut dependencies: BTreeSet<AssetId> = registry
                    .by_id(id)
                    .into_iter()
                    .flat_map(|e| e.dependencies.iter().copied())
                    .collect();
                match std::fs::read(&file) {
                    Ok(bytes) => {
                        let (header, _) = decode_cooked_file(&bytes)
                            .map_err(|e| anyhow::anyhow!("decoding {}: {e}", file.display()))?;
                        dependencies.extend(header.dependencies);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
                }
                for dep in dependencies {
                    if let Some(e) = by_id.get(&dep)
                        && !excluded(&normalize(&e.path))
                        && assets.insert(dep, to_asset(e)).is_none()
                    {
                        asset_queue.push_back(dep);
                    }
                }
            }
        }
    }

    check_profile_integrity(
        &cooked_dir,
        &registry,
        &integrity_roots.into_iter().collect::<Vec<_>>(),
        &assets.keys().copied().collect(),
    )?;

    let excluded_assets = by_id
        .values()
        .filter(|e| !assets.contains_key(&e.id))
        .map(|e| to_asset(e))
        .collect::<Vec<_>>();
    let excluded_raw = raw_all
        .iter()
        .filter(|(p, _)| !raw.contains_key(*p))
        .map(|(p, b)| ClosureRaw {
            path: p.clone(),
            bytes: *b,
        })
        .collect();
    let mut assets: Vec<ClosureAsset> = assets.into_values().collect();
    assets.sort_by(|a, b| (&a.path, a.id).cmp(&(&b.path, b.id)));
    let mut excluded_assets = excluded_assets;
    excluded_assets.sort_by(|a, b| (&a.path, a.id).cmp(&(&b.path, b.id)));
    Ok(Closure {
        assets,
        raw: raw.into_values().collect(),
        excluded_assets,
        excluded_raw,
        roots,
    })
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The `--dry-run` size report: the closure file by file, the totals, what was left out, and the
/// build the profile would run.
pub fn size_report(profile: &ResolvedProfile, platform: &str, closure: &Closure) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "build profile `{}` ({} build) for {platform}\n",
        profile.name,
        match profile.cargo {
            Profile::Dev => "dev",
            Profile::Shipping => "shipping",
        }
    ));
    match &profile.spec {
        Some(spec) => out.push_str(&format!(
            "  entry levels: {}\n  cargo: {}{}\n",
            spec.entry_levels.join(", "),
            if spec.no_default_features {
                "--no-default-features "
            } else {
                ""
            },
            if spec.features.is_empty() {
                String::new()
            } else {
                format!("--features {}", spec.features.join(","))
            }
        )),
        None => out.push_str("  built-in profile: every asset of the project ships\n"),
    }
    out.push_str(&format!(
        "  shipping checks: agent server {}, dev-assets and editor code never; no stray pak; no editor content\n",
        if profile.allow_agent() { "allowed" } else { "never" }
    ));
    out.push_str(&format!(
        "\ncooked assets in content/base.fpak ({}):\n",
        closure.assets.len()
    ));
    for a in &closure.assets {
        out.push_str(&format!("  {:>12} B  {}  [{}]\n", a.bytes, a.path, a.id));
    }
    out.push_str(&format!("\nraw files in assets/ ({}):\n", closure.raw.len()));
    for r in &closure.raw {
        out.push_str(&format!("  {:>12} B  {}\n", r.bytes, r.path));
    }
    out.push_str(&format!(
        "\nships:    {} cooked ({}) + {} raw ({}) = {}\n",
        closure.assets.len(),
        mib(closure.asset_bytes()),
        closure.raw.len(),
        mib(closure.raw_bytes()),
        mib(closure.asset_bytes() + closure.raw_bytes()),
    ));
    let ex_assets: u64 = closure.excluded_assets.iter().map(|a| a.bytes).sum();
    let ex_raw: u64 = closure.excluded_raw.iter().map(|r| r.bytes).sum();
    out.push_str(&format!(
        "left out: {} cooked ({}) + {} raw ({})\n",
        closure.excluded_assets.len(),
        mib(ex_assets),
        closure.excluded_raw.len(),
        mib(ex_raw),
    ));
    out.push_str(&roots_report(closure));
    out.push_str("note: compressed pak size differs; paks are not obfuscated or encrypted.\n");
    out
}

/// The `roots` section of the `--dry-run` report: why each cooked root and raw file of a named
/// profile ships, and which raw files only had the conservative fallback scan (CT-25, ADR-8477).
pub fn roots_report(closure: &Closure) -> String {
    let roots = &closure.roots;
    if roots.cooked.is_empty() && roots.raw.is_empty() {
        return String::new();
    }
    let paths: HashMap<AssetId, &str> = closure.assets.iter().map(|a| (a.id, a.path.as_str())).collect();
    let path_of = |id: AssetId| paths.get(&id).map_or_else(|| id.to_string(), |p| (*p).to_owned());
    let report = roots.report(&path_of);
    let mut out = format!("\nroots ({}):\n", report.records.len());
    for r in &report.records {
        out.push_str(&format!(
            "  {:<12} {}  <- {}{}{}\n",
            r.why.kind.name(),
            r.target,
            r.why.declared_by,
            r.why
                .via
                .as_ref()
                .map_or_else(String::new, |via| format!(" via assets/{via}")),
            if r.why.boot { " (boot)" } else { "" }
        ));
    }
    out.push_str(&format!(
        "root coverage: {} typed, {} binary, {} fallback{}\n",
        report.coverage.typed,
        report.coverage.binary,
        report.coverage.fallback.len(),
        if report.coverage.complete {
            String::new()
        } else {
            " (incomplete: unknown formats were scanned with the generic fallback)".to_owned()
        }
    ));
    for path in &report.coverage.fallback {
        out.push_str(&format!("  fallback scan: assets/{path}\n"));
    }
    for decl in &report.unmatched {
        out.push_str(&format!("  matched nothing: {decl}\n"));
    }
    out
}

/// Directory of the profile's own pak: `<project>/build/profiles/<name>/paks/<platform>`.
pub fn profile_pak_dir(project_dir: &Path, name: &str, platform: &str) -> PathBuf {
    project_dir
        .join("build")
        .join("profiles")
        .join(name)
        .join("paks")
        .join(platform)
}

/// Check both sources of dependency edges: the registry and the shipped cooked headers. A stale
/// registry may omit an edge; neither source may erase a required edge reported by the other.
fn check_profile_integrity(
    cache: &Path,
    registry: &regia_asset::registry::AssetRegistry,
    roots: &[AssetId],
    shipped: &BTreeSet<AssetId>,
) -> Result<()> {
    let mut entries = Vec::with_capacity(registry.len());
    for entry in registry.entries() {
        let mut entry = entry.clone();
        if shipped.contains(&entry.id) {
            let file = cache.join(cooked_file_name(entry.id));
            match std::fs::read(&file) {
                Ok(bytes) => {
                    let (header, _) = decode_cooked_file(&bytes)
                        .map_err(|e| anyhow::anyhow!("decoding {}: {e}", file.display()))?;
                    entry.dependencies.extend(header.dependencies);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
            }
        }
        entries.push(entry);
    }
    let graph = regia_asset::registry::AssetRegistry::from_entries(entries);
    let optional = regia_asset_pipeline::chunks::optional_references(&graph);
    let available = super::content::available_cooked(cache)?;
    // A previously computed Closure carries members, not traversal state. Start its recheck at
    // source assets so ID ordering cannot truncate a diagnostic chain. Cyclic components are
    // still covered by the shared walk's deterministic remaining-member pass.
    let incoming: BTreeSet<AssetId> = graph
        .entries()
        .filter(|e| shipped.contains(&e.id))
        .flat_map(|e| e.dependencies.iter().copied())
        .collect();
    let mut integrity_roots = roots.to_vec();
    integrity_roots.extend(shipped.difference(&incoming));
    let report = regia_asset_pipeline::chunks::check_dependency_integrity(
        &graph,
        &integrity_roots,
        shipped,
        &optional,
        Some(&available),
    );
    super::content::require_integrity(&report.problems)
}

/// Writes `<pak_dir>/base.fpak` holding exactly the closure's cooked assets (the directory is
/// emptied of `.fpak` files first, so an older profile run leaves nothing behind). With a
/// `lightmap_tier` (ADR-3120) every lightmap is that tier's, derived from the cooked bake, and a
/// lightmap the tier ships without is left out; the plan says which.
///
/// # Errors
///
/// A cooked file of the closure is missing, or the pak cannot be written.
pub fn write_profile_pak(
    project_dir: &Path,
    platform: &str,
    closure: &Closure,
    pak_dir: &Path,
    lightmap_tier: Option<&str>,
) -> Result<regia_asset_pipeline::lightmap_tiers::TierPakPlan> {
    write_profile_pak_with_diet(project_dir, platform, closure, pak_dir, lightmap_tier, None)
        .map(|(plan, _)| plan)
}

/// [`write_profile_pak`] under a cook diet (`[content.diet]` and `[profiles.<name>.diet]`,
/// ADR-8461): the variants it does not pick are left out, the meshes the triangle ceiling trims ship
/// without their finest LODs, and `<pak_dir>/diet-report.json` records what changed (a stale report
/// is removed when there is no diet). `diet` is the profile's name and its rules.
///
/// # Errors
///
/// As [`write_profile_pak`], or a cooked file the diet reads does not decode.
pub fn write_profile_pak_with_diet(
    project_dir: &Path,
    platform: &str,
    closure: &Closure,
    pak_dir: &Path,
    lightmap_tier: Option<&str>,
    diet: Option<(&str, &regia_asset_pipeline::cook_diet::DietRules)>,
) -> Result<(
    regia_asset_pipeline::lightmap_tiers::TierPakPlan,
    Option<regia_asset_pipeline::cook_diet::DietReport>,
)> {
    let cooked_dir = cooked_cache_dir(project_dir, platform);
    let registry = super::content::project_registry(&cooked_dir)?;
    let shipped = closure.assets.iter().map(|a| a.id).collect();
    check_profile_integrity(&cooked_dir, &registry, &[], &shipped)?;
    std::fs::create_dir_all(pak_dir)?;
    for entry in std::fs::read_dir(pak_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "fpak") {
            std::fs::remove_file(path)?;
        }
    }
    let report_path = pak_dir.join(regia_asset_pipeline::cook_diet::DIET_REPORT_FILE);
    if report_path.is_file() {
        std::fs::remove_file(&report_path)?;
    }
    let mut plan = lightmap_tier_plan(project_dir, platform, closure, lightmap_tier)?;
    let mut report = None;
    if let Some((profile, rules)) = diet {
        let assets: Vec<regia_asset_pipeline::cook_diet::DietAsset> = closure
            .assets
            .iter()
            .filter(|a| !plan.dropped.contains(&a.id))
            .map(|a| regia_asset_pipeline::cook_diet::DietAsset {
                path: a.path.clone(),
                id: a.id,
                type_path: a.type_path.clone(),
            })
            .collect();
        let diet_plan =
            regia_asset_pipeline::cook_diet::plan_diet(&cooked_dir, &assets, rules, profile, platform)
                .map_err(|e| anyhow::anyhow!("the cook diet of profile `{profile}`: {e}"))?;
        print_diet(&diet_plan.report);
        regia_asset_pipeline::cook_diet::write_report(&report_path, &diet_plan.report)
            .map_err(|e| anyhow::anyhow!("writing the diet report: {e}"))?;
        plan.dropped.extend(diet_plan.dropped.iter().copied());
        for (id, bytes) in diet_plan.overrides {
            plan.overrides.insert(id, bytes);
        }
        report = Some(diet_plan.report);
    }
    let shipped = closure.assets.iter().filter(|a| !plan.dropped.contains(&a.id));
    let entries: Vec<(AssetId, TypePathHash)> = shipped
        .clone()
        .map(|a| (a.id, TypePathHash::of(&a.type_path)))
        .collect();
    let manifest = AssetManifest {
        entries: shipped
            .map(|a| ManifestEntry {
                path: a.path.clone(),
                id: a.id,
                type_path: a.type_path.clone(),
            })
            .collect(),
    };
    regia_asset_pipeline::lightmap_tiers::write_pak_with_overrides(
        &pak_dir.join("base.fpak"),
        &cooked_dir,
        &entries,
        &manifest,
        &regia_asset_pipeline::PakWriteOptions::default(),
        &plan.overrides,
    )
    .map_err(|e| anyhow::anyhow!("writing the profile pak: {e}"))?;
    Ok((plan, report))
}

/// One line summing up a diet report.
fn diet_summary(report: &regia_asset_pipeline::cook_diet::DietReport) -> String {
    let left_out: usize = report.variants.iter().map(|g| g.left_out.len()).sum();
    format!(
        "diet of profile {}: {} texture(s) capped, {} mesh(es) lose LODs, {} variant(s) left out; triangles {} -> {} (ceiling {}), cooked bytes {} -> {}",
        report.profile,
        report.textures.len(),
        report.lods.len(),
        left_out,
        report.before.triangles,
        report.after.triangles,
        report.max_triangles,
        report.before.bytes,
        report.after.bytes
    )
}

/// The diet's summary line on stderr, then one line per warning.
fn print_diet(report: &regia_asset_pipeline::cook_diet::DietReport) {
    eprintln!("package: {}", diet_summary(report));
    for warning in &report.warnings {
        eprintln!("  warn {warning}");
    }
}

/// The lightmap tier `tier` of the closure's cooked lightmaps (ADR-3120), with one line per lightmap
/// on stderr; an empty plan without a tier.
///
/// # Errors
///
/// An unknown tier, a bad `[lightmaps.tiers]` table, or a lightmap that does not derive.
fn lightmap_tier_plan(
    project_dir: &Path,
    platform: &str,
    closure: &Closure,
    tier: Option<&str>,
) -> Result<regia_asset_pipeline::lightmap_tiers::TierPakPlan> {
    let Some(tier) = tier else {
        return Ok(regia_asset_pipeline::lightmap_tiers::TierPakPlan::default());
    };
    // As in `cook`: only a MISSING manifest falls back to the default tier table; one that
    // exists but cannot be read or validated must fail, not be silently replaced.
    let table = match Manifest::load(project_dir) {
        Ok(manifest) => manifest.lightmaps,
        Err(err) if super::cook::manifest_file_is_missing(&err) => {
            crate::manifest::LightmapsConfig::default()
        }
        Err(err) => return Err(err),
    }
    .tier_table()?;
    let rule = table.rule(tier).map_err(anyhow::Error::msg)?;
    let ids: Vec<AssetId> = closure
        .assets
        .iter()
        .filter(|a| a.type_path == regia_asset_pipeline::lightmap_tiers::LIGHTMAP_TYPE_PATH)
        .map(|a| a.id)
        .collect();
    let plan = regia_asset_pipeline::lightmap_tiers::plan_tier_pak(
        &cooked_cache_dir(project_dir, platform),
        &ids,
        tier,
        rule,
        platform,
    )
    .map_err(anyhow::Error::msg)?;
    for (id, r) in &plan.reports {
        let path = closure
            .assets
            .iter()
            .find(|a| a.id == *id)
            .map_or("?", |a| a.path.as_str());
        match r.verdict {
            regia_asset_pipeline::lightmap_tiers::TierVerdict::Fits => eprintln!(
                "package: lightmap {path} at tier {tier}: {} texels/m, {}x{}, {}{}, {} bytes",
                r.density,
                r.width,
                r.height,
                r.encoding,
                if r.directional { ", directional" } else { "" },
                r.bytes
            ),
            regia_asset_pipeline::lightmap_tiers::TierVerdict::OverBudget => eprintln!(
                "  warn lightmap_tier_over_budget: {path} at tier {tier} is {} bytes, over the tier's budget of {} bytes: the package ships without it (the probe light lights its receivers)",
                r.bytes,
                r.budget_bytes.unwrap_or(0)
            ),
            regia_asset_pipeline::lightmap_tiers::TierVerdict::None => {
                eprintln!("package: tier {tier} ships no lightmap ({path} left out)");
            }
        }
    }
    Ok(plan)
}

/// Stages a package's content: the paks into `content_dir`, the raw assets into `assets_dir`
/// (both from the profile's closure when it has one), then runs `ship_check` over `root` for
/// shipping builds: no stray pak, no editor content, no forbidden feature in the game's
/// `Cargo.toml`, no runtime glTF / PNG importer, and paks that pass the cooked-pack check
/// (ADR-1594).
///
/// # Errors
///
/// A copy fails, or the shipping package fails `ship_check`.
pub fn stage_content(
    project_dir: &Path,
    platform: &str,
    profile: &ResolvedProfile,
    root: &Path,
    content_dir: &Path,
    assets_dir: &Path,
    copy_plain_raw: impl FnOnce() -> Result<usize>,
) -> Result<()> {
    let closure = compute_closure(project_dir, platform, profile.spec.as_ref())?;
    // Start from a clean slate so a previous run (another profile, a dev package) cannot leak into
    // this one. On Android the assets dir is the APK assets root: nothing but this step fills it
    // (the notices are copied afterwards), and a kept Gradle project would otherwise ship the last
    // package's test files and levels.
    if content_dir.exists() {
        std::fs::remove_dir_all(content_dir)?;
    }
    if assets_dir.exists() {
        std::fs::remove_dir_all(assets_dir)?;
    }
    std::fs::create_dir_all(content_dir)?;
    let pak_dir = match &profile.spec {
        Some(_) => {
            let dir = profile_pak_dir(project_dir, &profile.name, platform);
            let spec = profile.spec.as_ref();
            let diet = regia_asset_pipeline::cook_diet::DietRules::for_profile(
                project_dir,
                Some(profile.name.as_str()),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            write_profile_pak_with_diet(
                project_dir,
                platform,
                &closure,
                &dir,
                spec.and_then(|s| s.lightmap_tier.as_deref()),
                diet.as_ref().map(|rules| (profile.name.as_str(), rules)),
            )?;
            stage_raw_files(
                project_dir,
                assets_dir,
                &closure.raw_set(),
                profile.cargo,
                platform,
            )?;
            dir
        }
        None => {
            copy_plain_raw()?;
            project_dir.join("build").join("paks").join(platform)
        }
    };
    // Fonts ship as raw files: the staged copies become the subsets of the shipped locales (ADR-2697).
    let fonts = regia_asset_pipeline::font_subset::stage_fonts(project_dir, assets_dir)
        .map_err(|e| anyhow::anyhow!("staging fonts: {e}"))?;
    for warning in &fonts.warnings {
        eprintln!("  warn {warning}");
    }
    if fonts.fonts > 0 {
        eprintln!(
            "package: {} font(s), {} subset: {} -> {} bytes",
            fonts.fonts, fonts.subset, fonts.bytes_before, fonts.bytes_after
        );
    }
    // The cook's loudness table ships next to the sounds it names (ADR-4898): without it a packaged
    // game has nothing to turn normalization on with.
    if let Some(table) =
        regia_asset_pipeline::audio_cook::stage_loudness_table(project_dir, platform, assets_dir)
            .map_err(|e| anyhow::anyhow!("staging the loudness table: {e}"))?
    {
        eprintln!("package: loudness table staged at {}", table.display());
    }
    // A chunked cook (specs/40): install chunks and the manifest go into the package, the other
    // chunks into `dist/<platform>/chunks` for a static file host.
    let external_dir = project_dir.join("dist").join(platform).join("chunks");
    let chunked = if profile.spec.is_none() && pak_dir.is_dir() {
        super::content::stage_chunks(&pak_dir, content_dir, &external_dir)?
    } else {
        None
    };
    if chunked.is_none() {
        copy_paks(&pak_dir, content_dir)?;
    } else {
        // A raw file (a sound ships raw as well as cooked) of a pack or host chunk travels in that
        // chunk's pak, never loose in the base package (ADR-7696).
        let removed = super::content::unstage_chunk_raw_files(project_dir, &pak_dir, assets_dir)?;
        if !removed.is_empty() {
            eprintln!(
                "package: {} raw file(s) left to their chunks, not the base package",
                removed.len()
            );
        }
    }
    // Every package carries the gameplay manifest: a client needs it to join (spec 77 § 4.1).
    let game_version = Manifest::load(project_dir).map_or_else(|_| String::new(), |m| m.project.version);
    server_content::write_net_manifest(project_dir, platform, &game_version, content_dir)?;
    stage_permutation_list(project_dir, platform, content_dir)?;
    run_ship_checks(
        project_dir,
        platform,
        profile,
        root,
        content_dir,
        chunked.as_deref(),
    )
}

/// Copies the paks of `pak_dir` and their registries into `content_dir`.
fn copy_paks(pak_dir: &Path, content_dir: &Path) -> Result<()> {
    if pak_dir.is_dir() {
        for entry in std::fs::read_dir(pak_dir)? {
            let entry = entry?;
            // Paks and the asset registry written next to each (`base.registry.bin`, spec
            // content-data § 2.1), which the runtime reads for registry queries.
            let registry = entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(regia_asset::registry::PAK_REGISTRY_SUFFIX));
            if registry || entry.path().extension().is_some_and(|e| e == "fpak") {
                std::fs::copy(entry.path(), content_dir.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

/// The content half of `ship_check` for shipping builds.
fn run_ship_checks(
    project_dir: &Path,
    platform: &str,
    profile: &ResolvedProfile,
    root: &Path,
    content_dir: &Path,
    chunked: Option<&[String]>,
) -> Result<()> {
    if profile.cargo == Profile::Shipping {
        let expected: Vec<String> = chunked.map_or_else(
            || EXPECTED_PAKS.iter().map(|s| (*s).to_string()).collect(),
            <[String]>::to_vec,
        );
        ship_check::check_package_tree(root, content_dir, &expected)?;
        let cargo_toml = project_dir.join("Cargo.toml");
        let cargo_text = std::fs::read_to_string(&cargo_toml).ok();
        // Named profiles only: plain `shipping` keeps working for games on regia's default features.
        let enforce = profile.spec.is_some() || profile.server;
        if let Some(text) = cargo_text.as_deref() {
            if enforce {
                ship_check::check_cargo_features(text, profile.allow_agent())
                    .with_context(|| format!("in {}", cargo_toml.display()))?;
            }
            // The glTF / PNG importer as a runtime dependency (ADR-1594): a warning on plain shipping.
            ship_check::check_cargo_runtime_importers(text, enforce)
                .with_context(|| format!("in {}", cargo_toml.display()))?;
        }
        // Cooked packs (ADR-1594): no staged pak entry is transcoded at load or fails to load.
        ship_check::check_cooked_paks(&ship_check::paks_in(content_dir)?, platform)?;
        // Chart seams of any baked lightmap (ADR-2024; a project with none never meets this).
        let budget = crate::manifest::Manifest::load(project_dir)
            .map_or_else(|_| crate::manifest::LightmapsConfig::default(), |m| m.lightmaps)
            .seam_budget;
        ship_check::check_lightmap_seams(&ship_check::paks_in(content_dir)?, budget)?;
    }
    Ok(())
}

/// Copies the cooked permutation list beside the paks (ADR-2026): the runtime reads it from the
/// content directory to install its guard. A cache without a list ships none, and that game runs
/// without the check.
fn stage_permutation_list(project_dir: &Path, platform: &str, content_dir: &Path) -> Result<()> {
    let name = regia_render::permutation_policy::PERMUTATION_MANIFEST_FILE;
    let source = regia_asset::cooked_cache_dir(project_dir, platform).join(name);
    if source.is_file() {
        std::fs::copy(&source, content_dir.join(name))
            .with_context(|| format!("staging {}", source.display()))?;
    }
    Ok(())
}

/// Stages a server package's content: the filtered closure's pak into `content_dir` as `base.fpak`,
/// its raw files into `assets_dir`, `net-manifest.ron`, then the shipping checks (spec 76 § 3).
///
/// # Errors
///
/// A copy fails, the content holds a presentation kind, or a shipping check fails.
pub fn stage_server_content(
    project_dir: &Path,
    platform: &str,
    profile: &ResolvedProfile,
    root: &Path,
    game_version: &str,
) -> Result<ServerContent> {
    let content_dir = root.join("content");
    let assets_dir = root.join("assets");
    for dir in [&content_dir, &assets_dir] {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
    }
    std::fs::create_dir_all(&content_dir)?;
    let closure = compute_closure(project_dir, platform, profile.spec.as_ref())?;
    let content = server_content::filter_closure(project_dir, platform, &closure)?;
    let dir = profile_pak_dir(project_dir, &profile.name, &format!("server-{platform}"));
    write_profile_pak(project_dir, platform, &content.kept, &dir, None)?;
    stage_raw_files(
        project_dir,
        &assets_dir,
        &content.kept.raw_set(),
        profile.cargo,
        platform,
    )?;
    copy_paks(&dir, &content_dir)?;
    server_content::check_staged_content(&content, root)?;
    server_content::write_net_manifest(project_dir, platform, game_version, &content_dir)?;
    run_ship_checks(project_dir, platform, profile, root, &content_dir, None)?;
    Ok(content)
}

/// `regia package --dry-run`: cooks if needed (done by the caller), then prints the closure's size
/// report and returns it. A server profile also lists what the kind filter removed.
///
/// # Errors
///
/// The closure cannot be computed.
pub fn dry_run(project_dir: &Path, profile: &ResolvedProfile, args: &PackageArgs) -> Result<String> {
    let platform = platform_name(args.platform);
    let closure = compute_closure(project_dir, platform, profile.spec.as_ref())?;
    if profile.server {
        let content = server_content::filter_closure(project_dir, platform, &closure)?;
        return Ok(server_content::dry_run_report(profile, platform, &content));
    }
    let mut report = size_report(profile, platform, &closure);
    // A named profile's cook diet (ADR-8461): what the pak would trim, without writing it.
    if profile.spec.is_some()
        && let Some(rules) =
            regia_asset_pipeline::cook_diet::DietRules::for_profile(project_dir, Some(profile.name.as_str()))
                .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        let assets: Vec<regia_asset_pipeline::cook_diet::DietAsset> = closure
            .assets
            .iter()
            .map(|a| regia_asset_pipeline::cook_diet::DietAsset {
                path: a.path.clone(),
                id: a.id,
                type_path: a.type_path.clone(),
            })
            .collect();
        let plan = regia_asset_pipeline::cook_diet::plan_diet(
            &cooked_cache_dir(project_dir, platform),
            &assets,
            &rules,
            &profile.name,
            platform,
        )
        .map_err(|e| anyhow::anyhow!("the cook diet of profile `{}`: {e}", profile.name))?;
        report.push_str(&diet_summary(&plan.report));
        report.push('\n');
        for warning in &plan.report.warnings {
            report.push_str(&format!("  warn {warning}\n"));
        }
    }
    Ok(report)
}

/// The cooked-cache platform tag of `platform`.
#[must_use]
pub fn platform_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "linux",
        Platform::Windows => "windows",
        Platform::Android => "android",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::level_assets::LevelSourceIndex;

    /// A scratch project removed on drop.
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("regia_cli_profile_{label}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("assets/levels")).expect("assets dir");
            Fixture(dir)
        }
        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join("assets").join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
        }
        fn material(&self, rel: &str, name: &str) {
            let mut mat = regia_asset::MaterialAssetRon::default_standard();
            mat.name = Some(name.to_string());
            self.write(rel, &mat.to_ron_string().expect("material ron"));
        }
        fn cook(&self) {
            let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
                project_root: self.0.clone(),
                platform: "linux".into(),
                incremental: true,
            })
            .expect("cook");
            crate::commands::cook::report_cook_outcome(&report).expect("cook outcome");
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn spec(entry: &[&str]) -> BuildProfile {
        BuildProfile {
            entry_levels: entry.iter().map(|s| (*s).to_string()).collect(),
            ..BuildProfile::default()
        }
    }

    /// Two levels: `main` uses `rock` and opens `second`, which uses `moss`; `unused` and a debug
    /// level are never referenced.
    fn project(label: &str) -> Fixture {
        let f = Fixture::new(label);
        f.material("materials/rock.material.ron", "rock");
        f.material("materials/moss.material.ron", "moss");
        f.material("materials/unused.material.ron", "unused");
        f.write(
            "levels/main.level.ron",
            r#"(entities: [(name: "a", components: {"mat": (material: "assets://materials/rock.material.ron", next: "levels/second.level.ron")})])"#,
        );
        f.write(
            "levels/second.level.ron",
            r#"(entities: [(name: "b", material: "materials/moss.material.ron")])"#,
        );
        f.write("levels/debug.level.ron", "(entities: [])");
        f.write("config/about.txt", "about");
        f.cook();
        f
    }

    fn paths(c: &Closure) -> (Vec<&str>, Vec<&str>) {
        (
            c.assets.iter().map(|a| a.path.as_str()).collect(),
            c.raw.iter().map(|r| r.path.as_str()).collect(),
        )
    }

    #[test]
    fn closure_follows_levels_and_nothing_else() -> Result<()> {
        let f = project("closure");
        let c = compute_closure(&f.0, "linux", Some(&spec(&["levels/main.level.ron"])))?;
        let (assets, raw) = paths(&c);
        assert_eq!(raw, ["levels/main.level.ron", "levels/second.level.ron"]);
        assert_eq!(assets.len(), 2, "{assets:?}");
        assert!(
            assets.iter().any(|p| p.ends_with("rock.material.ron")),
            "{assets:?}"
        );
        assert!(
            assets.iter().any(|p| p.ends_with("moss.material.ron")),
            "{assets:?}"
        );
        let left: Vec<&str> = c.excluded_assets.iter().map(|a| a.path.as_str()).collect();
        assert!(
            left.iter().any(|p| p.ends_with("unused.material.ron")),
            "{left:?}"
        );
        let left_raw: Vec<&str> = c.excluded_raw.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(left_raw, ["config/about.txt", "levels/debug.level.ron"]);
        Ok(())
    }

    /// A level that names its baked GI volume pulls the cooked `.fgiv` into the closure (as a
    /// cooked asset, not a raw file), and an unrelated volume stays out.
    #[test]
    fn closure_follows_a_levels_baked_gi_volume() -> Result<()> {
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
        let bytes = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)?
            .to_bytes()?;
        let f = Fixture::new("fgiv");
        for name in ["maps/used.fgiv", "maps/other.fgiv"] {
            let path = f.0.join("assets").join(name);
            std::fs::create_dir_all(path.parent().expect("parent"))?;
            std::fs::write(path, &bytes)?;
        }
        f.write(
            "levels/main.level.ron",
            r#"SceneFile(format_version: 10, entities: [], resources: {"regia_scene::level::LevelSettings": (look: (baked_gi: Some((volume: "maps/used.fgiv"))))})"#,
        );
        f.cook();
        let c = compute_closure(&f.0, "linux", Some(&spec(&["levels/main.level.ron"])))?;
        let (assets, raw) = paths(&c);
        assert_eq!(assets, ["assets/maps/used.fgiv"]);
        assert_eq!(raw, ["levels/main.level.ron"], "the volume ships cooked, not raw");
        let left: Vec<&str> = c.excluded_assets.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(left, ["assets/maps/other.fgiv"]);
        Ok(())
    }

    #[test]
    fn include_and_exclude_globs() -> Result<()> {
        let f = project("globs");
        let mut s = spec(&["levels/main.level.ron"]);
        s.include = vec!["config/**".into(), "materials/unused.*".into()];
        s.exclude = vec!["levels/second.level.ron".into()];
        let c = compute_closure(&f.0, "linux", Some(&s))?;
        let (assets, raw) = paths(&c);
        assert_eq!(raw, ["config/about.txt", "levels/main.level.ron"]);
        assert!(
            assets.iter().any(|p| p.ends_with("unused.material.ron")),
            "{assets:?}"
        );
        assert!(
            !assets.iter().any(|p| p.ends_with("moss.material.ron")),
            "moss was only reachable through the excluded level"
        );
        Ok(())
    }

    #[test]
    fn missing_entry_level_is_an_error_and_builtin_ships_everything() -> Result<()> {
        let f = project("missing");
        let err =
            compute_closure(&f.0, "linux", Some(&spec(&["levels/nope.level.ron"]))).expect_err("must fail");
        assert!(err.to_string().contains("levels/nope.level.ron"), "{err}");
        let all = compute_closure(&f.0, "linux", None)?;
        assert_eq!(all.assets.len(), 3);
        assert_eq!(all.raw.len(), 4);
        assert!(all.excluded_assets.is_empty() && all.excluded_raw.is_empty());
        Ok(())
    }

    #[test]
    fn dry_run_report_lists_the_closure() -> Result<()> {
        let f = project("report");
        let manifest = Manifest::parse(
            "[project]\nname = \"x\"\nid = \"a.b\"\n[profiles.demo]\nentry_levels = [\"levels/main.level.ron\"]\n",
        )?;
        let resolved = resolve(&manifest, "demo")?;
        assert_eq!(resolved.cargo, Profile::Shipping);
        let closure = compute_closure(&f.0, "linux", resolved.spec.as_ref())?;
        let text = size_report(&resolved, "linux", &closure);
        assert!(
            text.contains("levels/second.level.ron") && text.contains("rock.material.ron"),
            "{text}"
        );
        assert!(text.contains("left out: 1 cooked"), "{text}");
        let left_out: Vec<&str> = closure.excluded_assets.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(left_out, ["assets/materials/unused.material.ron"], "{text}");
        assert!(text.contains("not obfuscated"), "{text}");
        let err = resolve(&manifest, "nope").expect_err("must fail").to_string();
        assert!(err.contains("demo") && err.contains("shipping"), "{err}");
        Ok(())
    }

    #[test]
    fn profile_pak_holds_exactly_the_closure() -> Result<()> {
        let f = project("pak");
        let c = compute_closure(&f.0, "linux", Some(&spec(&["levels/main.level.ron"])))?;
        let dir = profile_pak_dir(&f.0, "demo", "linux");
        write_profile_pak(&f.0, "linux", &c, &dir, None)?;
        let reader = regia_asset::PakReader::open(&dir.join("base.fpak"))?;
        let mut in_pak: Vec<_> = reader.toc.entries.iter().map(|e| e.id).collect();
        let mut expected: Vec<_> = c.assets.iter().map(|a| a.id).collect();
        in_pak.sort();
        expected.sort();
        assert_eq!(in_pak, expected);
        Ok(())
    }

    /// A texture whose only reference is a startup `.ui.ron` (an `image` by asset path, ADR-3148)
    /// ships in the profile pak; a picture named only by a nested layout, an icon name and a texture
    /// nothing names stay out.
    #[test]
    fn a_ui_layouts_named_picture_ships_in_the_profile_pak() -> Result<()> {
        let f = project("ui-image");
        let png = |rel: &str| {
            let path = f.0.join("assets").join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 255]))
                .save(&path)
                .expect("png");
        };
        png("ui/panel.png");
        png("ui/mask.png");
        png("ui/nested-only.png");
        png("ui/unused.png");
        f.write(
            "ui/hud.ui.ron",
            r#"(version: 9, root: (children: [
                (name: "panel", image: (asset: "assets/ui/panel.png")),
                (name: "gear", image: (icon: "ui/unused.png")),
                (name: "group", mask: (asset: "ui/mask.png")),
            ]))"#,
        );
        f.write(
            "ui/screens/extra.ui.ron",
            r#"(version: 9, root: (image: (asset: "assets/ui/nested-only.png")))"#,
        );
        f.cook();
        let c = compute_closure(&f.0, "linux", Some(&spec(&["levels/main.level.ron"])))?;
        let (assets, raw) = paths(&c);
        assert!(raw.contains(&"ui/hud.ui.ron"), "{raw:?}");
        assert!(
            !raw.contains(&"ui/screens/extra.ui.ron"),
            "not loaded at start: {raw:?}"
        );
        let has = |name: &str| assets.iter().any(|p| p.ends_with(name));
        assert!(has("ui/panel.png"), "{assets:?}");
        assert!(has("ui/mask.png"), "{assets:?}");
        assert!(!has("nested-only.png"), "{assets:?}");
        assert!(!has("ui/unused.png"), "an icon name is not a path: {assets:?}");
        let panel = c
            .assets
            .iter()
            .find(|a| a.path.ends_with("ui/panel.png"))
            .map(|a| a.id)
            .expect("panel");
        let dir = profile_pak_dir(&f.0, "demo", "linux");
        write_profile_pak(&f.0, "linux", &c, &dir, None)?;
        let reader = regia_asset::PakReader::open(&dir.join("base.fpak"))?;
        assert!(
            reader.toc.entries.iter().any(|e| e.id == panel),
            "the pak holds the texture only the layout names"
        );
        Ok(())
    }

    /// A startup layout's component `use` target ships as a raw file beside the texture the layout
    /// names (the typed picture walk must not hide `use` edges, ADR-3147 and ADR-3148).
    #[test]
    fn a_startup_layouts_use_target_and_named_texture_both_ship() -> Result<()> {
        let f = project("ui-use");
        let png = f.0.join("assets").join("ui/panel.png");
        std::fs::create_dir_all(png.parent().expect("parent")).expect("dir");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 255]))
            .save(&png)
            .expect("png");
        f.write(
            "ui/hud.ui.ron",
            r#"(version: 9, root: (children: [
                (name: "panel", image: (asset: "assets/ui/panel.png")),
                (name: "bar", use: "ui/parts/health_bar.ui.ron"),
            ]))"#,
        );
        f.write("ui/parts/health_bar.ui.ron", r#"(version: 9, root: ())"#);
        f.cook();
        let c = compute_closure(&f.0, "linux", Some(&spec(&["levels/main.level.ron"])))?;
        let (assets, raw) = paths(&c);
        assert!(raw.contains(&"ui/hud.ui.ron"), "{raw:?}");
        assert!(
            raw.contains(&"ui/parts/health_bar.ui.ron"),
            "the used component ships: {raw:?}"
        );
        assert!(
            assets.iter().any(|p| p.ends_with("ui/panel.png")),
            "the named texture ships: {assets:?}"
        );
        Ok(())
    }

    /// An unreadable Regia.toml must fail the tier plan, not silently swap in the default tier
    /// table the way `cook` refuses (corr-low cli: build_profile fell back on ANY load error).
    #[test]
    fn an_unreadable_manifest_is_an_error_not_the_default_tiers() {
        let dir = regia_test_utils::unique_temp_dir("profile_tier_manifest");
        std::fs::create_dir_all(dir.join("Regia.toml")).expect("a dir where the manifest goes");
        let err = lightmap_tier_plan(&dir, "linux", &Closure::default(), Some("low"))
            .expect_err("an unreadable manifest must not fall back to the default tier table");
        assert!(
            !err.to_string().contains("lightmap_tier_unknown"),
            "the failure is the manifest, not the tier: {err:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A 64 × 32 directional lightmap at 16 texels per metre: one quad chart, every texel baked.
    fn tier_test_map() -> Result<regia_asset::lightmap::Lightmap> {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap,
        };
        let (w, h) = (64u32, 32u32);
        let n = (w * h) as usize;
        let mut map = Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 16.0,
                padding: 2,
                entries: vec![LightmapEntry {
                    name: "yard".into(),
                    source_vertices: 4,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 8.0,
                    texels_per_metre: 16.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2, 0, 2, 3],
                }],
            },
            &(0..n)
                .map(|i| [1.0 + (i % 64) as f32, 2.0, 3.0])
                .collect::<Vec<_>>(),
            vec![Coverage::BAKED; n],
            LightmapBakeInfo::default(),
        )?;
        map.set_directions(&vec![[0.0, 0.7, 0.0, 0.8]; n])?;
        Ok(map)
    }

    /// A profile's `lightmap_tier` (ADR-3120): the pak holds that tier's lightmap, derived from the
    /// cooked bake (ULTRA_LOW: 2 texels per metre, BC6H, no directions); a tier whose rule is `none`
    /// leaves the lightmap out of the pak; no tier ships the cooked lightmap as it is.
    #[test]
    fn a_profile_tier_ships_its_derived_lightmap() -> Result<()> {
        let f = Fixture::new("lightmap_tier");
        std::fs::write(
            f.0.join("Regia.toml"),
            "[project]\nname = \"Tiers\"\nid = \"com.example.tiers\"\n\n[lightmaps.tiers.off]\nnone = true\n",
        )?;
        std::fs::create_dir_all(f.0.join("assets/maps"))?;
        tier_test_map()?.write(&f.0.join("assets/maps/yard.flmap"))?;
        f.write(
            "levels/main.level.ron",
            r#"(entities: [(name: "a", components: {"lm": (file: "assets://maps/yard.flmap")})])"#,
        );
        f.cook();
        let c = compute_closure(&f.0, "linux", Some(&spec(&["levels/main.level.ron"])))?;
        let lightmap = c
            .assets
            .iter()
            .find(|a| a.path.ends_with("yard.flmap"))
            .expect("the closure holds the lightmap")
            .id;
        let read = |dir: &Path| -> Result<Option<regia_asset::lightmap::Lightmap>> {
            let reader = regia_asset::PakReader::open(&dir.join("base.fpak"))?;
            if reader.entry(lightmap).is_none() {
                return Ok(None);
            }
            let file = reader.read_decompressed(lightmap)?;
            let (_, payload) = regia_asset::decode_cooked_file(&file)?;
            Ok(Some(regia_asset::lightmap::Lightmap::from_bytes(payload)?))
        };
        let dir = profile_pak_dir(&f.0, "low", "linux");
        let plan = write_profile_pak(&f.0, "linux", &c, &dir, Some("ultra_low"))?;
        assert_eq!(plan.reports.len(), 1);
        assert!(plan.dropped.is_empty());
        let derived = read(&dir)?.expect("the tier's lightmap ships");
        assert_eq!(derived.density, 2.0);
        // 8 × 4 at the tier; the bake's padding over eight leaves no whole gutter texel, so the
        // chart is repacked with ULTRA_LOW's padding on every side (ADR-5125).
        let pad = regia_asset_pipeline::lightmap_tiers::BLOCK_TIER_CHART_PADDING;
        assert_eq!((derived.width, derived.height), (8 + 2 * pad, 4 + 2 * pad));
        assert!(!derived.is_directional());
        assert_eq!(
            derived.texel_encoding(),
            regia_asset::lightmap::LightmapTexelEncoding::Bc6h
        );
        assert_eq!(plan.lightmap_bytes(), derived.to_bytes()?.len() as u64);

        let off = profile_pak_dir(&f.0, "off", "linux");
        let plan = write_profile_pak(&f.0, "linux", &c, &off, Some("off"))?;
        assert!(plan.dropped.contains(&lightmap));
        assert!(read(&off)?.is_none(), "the `none` tier ships no lightmap");

        let plain = profile_pak_dir(&f.0, "plain", "linux");
        write_profile_pak(&f.0, "linux", &c, &plain, None)?;
        assert_eq!(read(&plain)?.expect("ships").density, 16.0);

        let err = write_profile_pak(&f.0, "linux", &c, &plain, Some("medium")).expect_err("unknown tier");
        assert!(format!("{err:#}").contains("lightmap_tier_unknown"), "{err:#}");
        Ok(())
    }

    /// The same profile tier per platform (ADR-5382, ledger L03): the default table's ULTRA_LOW
    /// (`auto`) packs ASTC HDR blocks into the Android pak and BC6H into the desktop pak, LOW
    /// keeps the bake's encoding on both, and each pak passes the package's lightmap check
    /// (`ship_check::check_lightmap_seams`, which reads the tier's header from the pak).
    #[test]
    fn a_profile_tier_packs_astc_hdr_for_android_and_bc6h_for_the_desktop() -> Result<()> {
        use regia_asset::lightmap::LightmapTexelEncoding;
        let f = Fixture::new("lightmap_tier_platforms");
        std::fs::write(
            f.0.join("Regia.toml"),
            "[project]\nname = \"Tiers\"\nid = \"com.example.tiers\"\n",
        )?;
        std::fs::create_dir_all(f.0.join("assets/maps"))?;
        tier_test_map()?.write(&f.0.join("assets/maps/yard.flmap"))?;
        f.write(
            "levels/main.level.ron",
            r#"(entities: [(name: "a", components: {"lm": (file: "assets://maps/yard.flmap")})])"#,
        );
        for (platform, ultra_low) in [
            ("android", LightmapTexelEncoding::AstcHdr),
            ("linux", LightmapTexelEncoding::Bc6h),
        ] {
            let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
                project_root: f.0.clone(),
                platform: platform.into(),
                incremental: true,
            })?;
            crate::commands::cook::report_cook_outcome(&report)?;
            let c = compute_closure(&f.0, platform, Some(&spec(&["levels/main.level.ron"])))?;
            let lightmap = c
                .assets
                .iter()
                .find(|a| a.path.ends_with("yard.flmap"))
                .expect("the closure holds the lightmap")
                .id;
            for (tier, encoding, directional) in [
                ("ultra_low", ultra_low, false),
                ("low", LightmapTexelEncoding::Rgb9e5, true),
            ] {
                let dir = profile_pak_dir(&f.0, tier, platform);
                let plan = write_profile_pak(&f.0, platform, &c, &dir, Some(tier))?;
                assert!(plan.dropped.is_empty(), "{platform} {tier}");
                let pak = dir.join("base.fpak");
                let reader = regia_asset::PakReader::open(&pak)?;
                let file = reader.read_decompressed(lightmap)?;
                let (_, payload) = regia_asset::decode_cooked_file(&file)?;
                let derived = regia_asset::lightmap::Lightmap::from_bytes(payload)?;
                assert_eq!(derived.texel_encoding(), encoding, "{platform} {tier}");
                assert_eq!(derived.is_directional(), directional, "{platform} {tier}");
                assert_eq!(plan.lightmap_bytes(), payload.len() as u64, "{platform} {tier}");
                let seams = crate::commands::ship_check::check_lightmap_seams(
                    std::slice::from_ref(&pak),
                    crate::manifest::LightmapsConfig::default().seam_budget,
                )?;
                assert_eq!(seams.lightmaps, 1, "{platform} {tier}");
                assert!(seams.over.is_empty(), "{platform} {tier}");
            }
        }
        Ok(())
    }

    #[test]
    fn stray_pak_fails_a_shipping_stage_and_profile_stage_is_clean() -> Result<()> {
        let f = project("stage");
        let manifest = Manifest::parse(
            "[project]\nname = \"x\"\nid = \"a.b\"\n[profiles.demo]\nentry_levels = [\"levels/main.level.ron\"]\n",
        )?;
        let demo = resolve(&manifest, "demo")?;
        let pkg = f.0.join("pkg");
        let stage = |p: &ResolvedProfile| {
            stage_content(
                &f.0,
                "linux",
                p,
                &pkg,
                &pkg.join("content"),
                &pkg.join("assets"),
                || {
                    std::fs::create_dir_all(pkg.join("assets"))?;
                    Ok(0)
                },
            )
        };
        stage(&demo)?;
        assert!(pkg.join("content/base.fpak").is_file());
        assert!(pkg.join("assets/levels/second.level.ron").is_file());
        assert!(!pkg.join("assets/levels/debug.level.ron").exists());

        // The built-in shipping profile copies every pak in build/paks: a leftover one is stray.
        let paks = f.0.join("build/paks/linux");
        std::fs::create_dir_all(&paks)?;
        std::fs::write(paks.join("base.fpak"), b"x")?;
        std::fs::write(paks.join("old_dlc.fpak"), b"x")?;
        let shipping = resolve(&manifest, "shipping")?;
        let err = stage(&shipping).expect_err("must fail").to_string();
        assert!(err.contains("stray pak") && err.contains("old_dlc.fpak"), "{err}");
        // A dev package is not checked.
        stage(&resolve(&manifest, "dev")?)?;
        Ok(())
    }

    /// A level in directory form with a functional test, placing `room` by id; `room` is a plain
    /// file with its sidecar.
    fn levels_project(label: &str) -> Result<(Fixture, regia_core::id::Guid)> {
        use regia_scene::format::SCENE_FORMAT_VERSION as VERSION;
        let f = project(label);
        let room_id = regia_core::id::Guid::from_u128(0x0e);
        let assets = f.0.join("assets");
        f.write(
            "levels/room.level.ron",
            &format!("SceneFile(format_version: {VERSION}, entities: [])\n"),
        );
        std::fs::write(
            regia_asset::meta_path_for(&assets.join("levels/room.level.ron")),
            regia_asset::MetaSidecar::new("level", regia_asset::AssetId(room_id)).to_ron_string()?,
        )?;
        f.write(
            "levels/hub.level.ron",
            &format!(
                r#"SceneFile(format_version: {VERSION}, entities: [
                    (guid: "{}", components: {{
                        "regia_scene::placed::PlacedLevel": (level: "asset:{room_id}", mode: Inline, filter: (layers: [])),
                    }}),
                    (guid: "{}", components: {{
                        "regia_framework::functional::FunctionalTest": (format_version: 1),
                    }}),
                ])"#,
                regia_core::id::Guid::from_u128(1),
                regia_core::id::Guid::from_u128(2),
            ),
        );
        let hub = assets.join("levels/hub.level.ron");
        regia_scene::level_dir::split_level_file(&hub, &assets.join("levels/hub.level.d"))?;
        std::fs::remove_file(hub)?;
        f.write(
            "levels/door.functest.ron",
            "FuncTestScript(format_version: 1, steps: [])",
        );
        Ok((f, room_id))
    }

    #[test]
    fn a_closure_takes_a_directory_level_and_what_it_places_by_id() -> Result<()> {
        let (f, _) = levels_project("closure_levels")?;
        for entry in ["levels/hub.level.d", "levels/hub.level.ron"] {
            let c = compute_closure(&f.0, "linux", Some(&spec(&[entry])))?;
            let (_, raw) = paths(&c);
            assert!(raw.contains(&"levels/room.level.ron"), "{entry}: {raw:?}");
            assert!(
                raw.iter().any(|p| p.starts_with("levels/hub.level.d/")),
                "{entry}: {raw:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_named_shipping_profile_runs_the_level_step() -> Result<()> {
        let (f, room_id) = levels_project("named_strip")?;
        let manifest = Manifest::parse(
            "[project]\nname = \"x\"\nid = \"a.b\"\n[profiles.demo]\nentry_levels = [\"levels/hub.level.d\"]\n\
             include = [\"levels/*.functest.ron\"]\n\
             [profiles.qa]\nbase = \"dev\"\nentry_levels = [\"levels/hub.level.d\"]\n\
             include = [\"levels/*.functest.ron\"]\n",
        )?;
        let pkg = f.0.join("pkg");
        let assets = pkg.join("assets");
        let stage = |name: &str| {
            stage_content(
                &f.0,
                "linux",
                &resolve(&manifest, name)?,
                &pkg,
                &pkg.join("content"),
                &assets,
                || Ok(0),
            )
        };
        stage("demo")?;
        assert!(
            !assets.join("levels/hub.level.d").exists(),
            "a directory ships as one file"
        );
        let hub = std::fs::read_to_string(assets.join("levels/hub.level.ron"))?;
        assert!(!hub.contains("FunctionalTest"), "{hub}");
        assert!(!assets.join("levels/door.functest.ron").exists());
        assert!(
            assets.join("levels/room.level.ron").is_file(),
            "the placed level ships"
        );
        let index = LevelSourceIndex::scan(&assets);
        assert_eq!(
            index.levels.get(&room_id),
            Some(&assets.join("levels/room.level.ron")),
            "and it can be found by id in the package"
        );

        stage("qa")?;
        let hub = std::fs::read_to_string(assets.join("levels/hub.level.ron"))?;
        assert!(
            hub.contains("FunctionalTest"),
            "a dev-based profile keeps its tests"
        );
        assert!(assets.join("levels/door.functest.ron").is_file());
        Ok(())
    }

    #[test]
    fn agent_feature_fails_a_shipping_stage() -> Result<()> {
        let f = project("agent");
        std::fs::write(
            f.0.join("Cargo.toml"),
            "[package]\nname = \"g\"\n[dependencies]\nregia = { path = \"x\", features = [\"agent\"] }\n",
        )?;
        let manifest = Manifest::parse(
            "[project]\nname = \"x\"\nid = \"a.b\"\n[profiles.demo]\nentry_levels = [\"levels/main.level.ron\"]\n\
             [profiles.playtest]\nentry_levels = [\"levels/main.level.ron\"]\nallow_agent = true\n",
        )?;
        let pkg = f.0.join("pkg");
        let stage = |p: &ResolvedProfile| {
            stage_content(
                &f.0,
                "linux",
                p,
                &pkg,
                &pkg.join("content"),
                &pkg.join("assets"),
                || Ok(0),
            )
        };
        let err = stage(&resolve(&manifest, "demo")?).expect_err("must fail");
        assert!(format!("{err:#}").contains("agent"), "{err:#}");
        stage(&resolve(&manifest, "playtest")?)?;
        Ok(())
    }

    #[test]
    fn restaging_an_apk_assets_root_clears_leftovers_from_an_earlier_run() -> Result<()> {
        let f = project("restage_apk");
        let apk = f.0.join("gradle/app/src/main/assets");
        std::fs::create_dir_all(apk.join("content"))?;
        std::fs::write(apk.join("smoke.functest.ron"), b"x")?;
        std::fs::write(apk.join("old.level.ron"), b"x")?;
        std::fs::write(apk.join("content/old.fpak"), b"x")?;
        let plain = ResolvedProfile {
            name: "shipping".into(),
            cargo: Profile::Shipping,
            spec: None,
            server: false,
        };
        stage_content(&f.0, "android", &plain, &apk, &apk.join("content"), &apk, || {
            Ok(0)
        })?;
        assert!(!apk.join("smoke.functest.ron").exists());
        assert!(!apk.join("old.level.ron").exists());
        assert!(!apk.join("content/old.fpak").exists());
        assert!(apk.join("content").is_dir());
        Ok(())
    }

    #[test]
    fn restaging_clears_leftovers_from_an_earlier_run() -> Result<()> {
        let f = project("restage");
        let pkg = f.0.join("pkg");
        std::fs::create_dir_all(pkg.join("assets"))?;
        std::fs::create_dir_all(pkg.join("content"))?;
        std::fs::write(pkg.join("assets/old.txt"), b"x")?;
        std::fs::write(pkg.join("content/old.fpak"), b"x")?;
        std::fs::write(pkg.join("game"), b"binary")?;
        let plain = ResolvedProfile {
            name: "shipping".into(),
            cargo: Profile::Shipping,
            spec: None,
            server: false,
        };
        stage_content(
            &f.0,
            "linux",
            &plain,
            &pkg,
            &pkg.join("content"),
            &pkg.join("assets"),
            || Ok(0),
        )?;
        assert!(!pkg.join("assets/old.txt").exists());
        assert!(!pkg.join("content/old.fpak").exists());
        assert!(pkg.join("game").is_file(), "the binary is untouched");
        Ok(())
    }

    #[test]
    fn manifest_rejects_bad_profiles() {
        let head = "[project]\nname = \"x\"\nid = \"a.b\"\n";
        for bad in [
            "[profiles.shipping]\nentry_levels = [\"a\"]\n",
            "[profiles.p]\n",
            "[profiles.p]\nentry_levels = [\"a\"]\nbase = \"fast\"\n",
            "[profiles.p]\nentry_levels = [\"a\"]\nfeatures = [\"dev-assets\"]\n",
            "[profiles.p]\nentry_levels = [\"a\"]\nfeatures = [\"regia/agent\"]\n",
        ] {
            assert!(Manifest::parse(&format!("{head}{bad}")).is_err(), "{bad}");
        }
        assert!(Manifest::parse(&format!("{head}[profiles.p]\nentry_levels = [\"a\"]\nfeatures = [\"regia/agent\"]\nallow_agent = true\n")).is_ok());
    }

    /// Spec 76 § 3 (audit 2026-11-10): the `target` key and the built-in `server` profile.
    #[test]
    fn profile_target_and_the_builtin_server_profile() {
        let head = "[project]\nname = \"x\"\nid = \"a.b\"\n";
        let bad = format!("{head}[profiles.p]\nentry_levels = [\"a\"]\ntarget = \"android\"\n");
        let err = Manifest::parse(&bad).expect_err("unknown target").to_string();
        assert!(err.contains("target must be"), "{err}");
        let m = Manifest::parse(&format!(
            "{head}[profiles.srv]\nentry_levels = [\"a\"]\ntarget = \"server\"\nfeatures = [\"bots\"]\n\
             [profiles.cli]\nentry_levels = [\"a\"]\ntarget = \"client\"\n\
             [profiles.plain]\nentry_levels = [\"a\"]\n"
        ))
        .expect("manifest");
        let srv = resolve(&m, "srv").expect("srv");
        assert!(srv.server);
        assert_eq!(
            cargo_extra_args(&srv),
            ["--no-default-features", "--features", "server,bots"]
        );
        assert!(!resolve(&m, "cli").expect("cli").server);
        assert!(!resolve(&m, "plain").expect("plain").server, "absent = client");
        // The built-in `server` profile: shipping, server build, whole project (ADR-1886 (8)).
        let builtin = resolve(&m, BUILTIN_SERVER_PROFILE).expect("built-in server");
        assert!(builtin.server && builtin.spec.is_none());
        assert_eq!(builtin.cargo, Profile::Shipping);
        assert_eq!(
            cargo_extra_args(&builtin),
            ["--no-default-features", "--features", "server"]
        );
        // A `[profiles.server]` table replaces it.
        let own = Manifest::parse(&format!(
            "{head}[profiles.server]\nentry_levels = [\"a\"]\nbase = \"dev\"\n"
        ))
        .expect("own server profile");
        let r = resolve(&own, "server").expect("own");
        assert_eq!(r.cargo, Profile::Dev);
        assert!(r.spec.is_some() && !r.server, "its own table decides the target");
    }

    #[test]
    fn the_cooked_permutation_list_is_staged_beside_the_paks() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("stage_permutations");
        let content = dir.join("staging/content");
        std::fs::create_dir_all(&content)?;
        let name = regia_render::permutation_policy::PERMUTATION_MANIFEST_FILE;
        // No list cooked: nothing staged, and not an error (that game runs without the check).
        stage_permutation_list(&dir, "linux", &content)?;
        assert!(!content.join(name).exists());
        let cache = regia_asset::cooked_cache_dir(&dir, "linux");
        std::fs::create_dir_all(&cache)?;
        std::fs::write(cache.join(name), "{}")?;
        stage_permutation_list(&dir, "linux", &content)?;
        assert_eq!(std::fs::read_to_string(content.join(name))?, "{}");
        // A plain file in the content directory passes the package tree check.
        ship_check::check_package_tree(&dir.join("staging"), &content, &[])?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_match("levels/*.ron", "levels/a.ron"));
        assert!(!glob_match("levels/*.ron", "levels/x/a.ron"));
        assert!(glob_match("levels/**", "levels/x/a.ron"));
        assert!(glob_match("assets://a/?.txt", "a/b.txt"));
        assert!(!glob_match("a/?.txt", "a/bb.txt"));
    }

    #[test]
    fn cargo_extra_args_follow_the_profile() {
        let mut s = spec(&["a"]);
        s.no_default_features = true;
        s.features = vec!["ui".into(), "quick".into()];
        let p = ResolvedProfile {
            name: "p".into(),
            cargo: Profile::Shipping,
            spec: Some(s),
            server: false,
        };
        assert_eq!(
            cargo_extra_args(&p),
            ["--no-default-features", "--features", "ui,quick"]
        );
    }

    /// PCM16 mono WAV bytes of one second of a 1 kHz sine at 48 kHz.
    fn tone_wav() -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..48_000u32 {
            let s = ((f64::from(i) / 48_000.0) * 1000.0 * std::f64::consts::TAU).sin() * 0.1;
            data.extend_from_slice(&((s * 32_767.0) as i16).to_le_bytes());
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.extend_from_slice(&96_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// ADR-4898: a stage ships the cooked loudness table beside the sounds,
    /// and a raw `assets/loudness.ron` in its place stops the stage instead of being overwritten.
    #[test]
    fn the_cooked_loudness_table_ships_in_the_staged_assets() -> Result<()> {
        let f = project("stage_loudness");
        let sfx = f.0.join("assets/sfx");
        std::fs::create_dir_all(&sfx)?;
        std::fs::write(sfx.join("tone.wav"), tone_wav())?;
        f.cook();
        let cooked = std::fs::read(regia_asset::cooked_cache_dir(&f.0, "linux").join("loudness.ron"))?;
        assert!(String::from_utf8_lossy(&cooked).contains("assets/sfx/tone.wav"));
        let plain = ResolvedProfile {
            name: "shipping".into(),
            cargo: Profile::Shipping,
            spec: None,
            server: false,
        };
        let pkg = f.0.join("pkg");
        let assets = pkg.join("assets");
        let copy = || copy_raw_assets_for_test(&f.0, &assets);
        stage_content(&f.0, "linux", &plain, &pkg, &pkg.join("content"), &assets, copy)?;
        assert_eq!(std::fs::read(assets.join("loudness.ron"))?, cooked);
        assert!(assets.join("sfx/tone.wav").is_file(), "the sound ships beside it");

        // A raw file of the same name is a stage error, and it is not overwritten.
        f.write("loudness.ron", "(raw: true)");
        let err = stage_content(&f.0, "linux", &plain, &pkg, &pkg.join("content"), &assets, || {
            copy_raw_assets_for_test(&f.0, &assets)
        })
        .expect_err("conflict");
        assert!(format!("{err:#}").contains("loudness_table_conflict"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(assets.join("loudness.ron"))?,
            "(raw: true)"
        );
        Ok(())
    }

    /// The plain raw-asset copy `regia package` runs for a stage without a profile spec.
    fn copy_raw_assets_for_test(project: &Path, assets: &Path) -> Result<usize> {
        let mut copied = 0;
        for rel in ["sfx/tone.wav", "loudness.ron"] {
            let from = project.join("assets").join(rel);
            if from.is_file() {
                let to = assets.join(rel);
                std::fs::create_dir_all(to.parent().expect("parent"))?;
                std::fs::copy(&from, &to)?;
                copied += 1;
            }
        }
        Ok(copied)
    }
}

/// Bakes of levels reached through placements ship in a profile's pak (spec 62 § 8, ADR-7262).
#[cfg(test)]
mod placed_bake_tests {
    use std::collections::BTreeMap;

    use regia_core::id::Guid;
    use regia_procgen::exec::{Collision, Instance, InstanceSet, RunOutput};
    use regia_procgen::pgout::{PgoutHeader, encode};
    use regia_scene::format::{SceneEntity, SceneFile};

    use super::*;

    const SUB_ID: Guid = Guid(0x5b2);
    const SUB_GEN: Guid = Guid(0x6e6);
    const SUB_EDITOR_GEN: Guid = Guid(0x6e7);
    const OTHER_GEN: Guid = Guid(0x0e8);
    const PLACEMENT: Guid = Guid(0x91ac);

    fn bake() -> Vec<u8> {
        let inst = |x: f32| Instance {
            translation: [x, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.0; 4],
        };
        let output = RunOutput {
            instance_sets: vec![InstanceSet {
                mesh: "rock.glb".into(),
                material: None,
                cast_shadows: true,
                collision: Collision::None,
                draw_distance_m: 100.0,
                fade_m: 0.0,
                cull_below_px: 2.0,
                instances: vec![inst(1.0), inst(2.0), inst(3.0)],
                layer: None,
            }],
            ..RunOutput::default()
        };
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 3,
            input_hash: [4; 32],
            output_hash: output.hash(),
            quality: 5,
        };
        encode(&header, &output)
    }

    fn record(guid: Guid, type_path: &str, ron_text: String) -> SceneEntity {
        let mut components = BTreeMap::new();
        components.insert(
            type_path.to_string(),
            ron::value::RawValue::from_boxed_ron(ron_text.into_boxed_str()).expect("raw ron"),
        );
        SceneEntity {
            guid,
            name: None,
            parent: None,
            components,
            prefab_instance: None,
        }
    }

    fn generator(guid: Guid, editor_only: bool) -> SceneEntity {
        record(
            guid,
            "regia_procgen::world::ProcGenerator",
            format!("(graph: \"g.pgraph.ron\", editor_only: {editor_only})"),
        )
    }

    fn write_level(assets: &Path, rel: &str, entities: Vec<SceneEntity>) {
        let mut scene = SceneFile::new();
        scene.entities = entities;
        let path = assets.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("level dir");
        std::fs::write(&path, scene.to_ron_string().expect("level text")).expect("level");
    }

    fn write_bake(assets: &Path, level: &str, guid: Guid) {
        let dir = assets.join(regia_procgen::world::level_bake_dir(Path::new(level)));
        std::fs::create_dir_all(&dir).expect("bake dir");
        std::fs::write(dir.join(format!("{guid}.pgout")), bake()).expect("bake");
    }

    /// `levels/main.level.ron` (the entry level) places `levels/sub.level.ron` by asset id; the
    /// sublevel holds a shipping and an `editor_only` generator with a bake each, and
    /// `levels/other.level.ron`, which nothing reaches, has a bake of its own. Cooked for `platform`.
    fn project(label: &str, platform: &str) -> PathBuf {
        project_with(label, platform, |_, _| {})
    }

    /// [`project`] with `extra` adding records (and their bakes) to the sublevel before the cook.
    fn project_with(
        label: &str,
        platform: &str,
        extra: impl FnOnce(&Path, &mut Vec<SceneEntity>),
    ) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(label);
        let assets = root.join("assets");
        let mut sub = vec![generator(SUB_GEN, false), generator(SUB_EDITOR_GEN, true)];
        extra(&assets, &mut sub);
        write_level(&assets, "levels/sub.level.ron", sub);
        let sidecar = regia_asset::MetaSidecar::new("level", AssetId(SUB_ID));
        std::fs::write(
            regia_asset::meta_path_for(&assets.join("levels/sub.level.ron")),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("sub meta");
        write_level(
            &assets,
            "levels/main.level.ron",
            vec![record(
                PLACEMENT,
                regia_scene::placed::PLACED_LEVEL_PATH,
                format!("(level: \"asset:{SUB_ID}\", mode: Inline)"),
            )],
        );
        write_level(
            &assets,
            "levels/other.level.ron",
            vec![generator(OTHER_GEN, false)],
        );
        write_bake(&assets, "levels/sub.level.ron", SUB_GEN);
        write_bake(&assets, "levels/sub.level.ron", SUB_EDITOR_GEN);
        write_bake(&assets, "levels/other.level.ron", OTHER_GEN);
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: root.clone(),
            platform: platform.to_string(),
            incremental: false,
        })
        .expect("cook");
        // Levels are raw files (the cook lists them as unsupported); every bake must cook.
        assert!(
            !report.errors.iter().any(|(p, _)| p.ends_with(".pgout")),
            "{:?}",
            report.errors
        );
        root
    }

    fn spec() -> BuildProfile {
        BuildProfile {
            entry_levels: vec!["levels/main.level.ron".to_string()],
            ..BuildProfile::default()
        }
    }

    /// The acceptance of item 1: a profile whose entry level places a sublevel lists the sublevel's
    /// bake in its pak; the editor-only bake and the bake of an unreached level stay out.
    #[test]
    fn a_placed_sublevels_bake_ships_in_the_profile_pak() -> Result<()> {
        let root = project("profile-placed-bakes", "linux");
        let closure = compute_closure(&root, "linux", Some(&spec()))?;
        let pak_dir = root.join("build/paks/linux");
        write_profile_pak(&root, "linux", &closure, &pak_dir, None)?;
        let pak = regia_asset::PakReader::open(&pak_dir.join("base.fpak"))
            .map_err(|e| anyhow::anyhow!("open pak: {e}"))?;
        let listed: Vec<&str> = pak.manifest().iter().map(|e| e.path.as_str()).collect();
        let sub = format!("assets/levels/sub.procgen/{SUB_GEN}.pgout");
        assert!(
            listed.contains(&sub.as_str()),
            "the sublevel's bake ships: {listed:?}"
        );
        assert!(
            !listed.iter().any(|p| p.contains(&SUB_EDITOR_GEN.to_string())),
            "editor_only bakes stay out: {listed:?}"
        );
        assert!(
            !listed.iter().any(|p| p.contains("other.procgen")),
            "an unreached level's bakes stay out: {listed:?}"
        );
        drop(pak);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// The runtime half: with the loose bakes and the cooked cache gone, the game opens the entry
    /// level, the placement fills, and the sublevel's generator loads its bake from the pak.
    #[test]
    fn the_game_loads_a_placed_sublevels_bake_from_the_pak() -> Result<()> {
        use regia_app::prelude::*;
        use regia_procgen::world::{
            ProcBakeOrigin, ProcBakeSource, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator,
            ProcGraphSource, ProcLevelBakeDirs, ProcOutput, level_bake_dir,
        };

        let root = project("profile-placed-bakes-runtime", "android");
        let closure = compute_closure(&root, "android", Some(&spec()))?;
        let pak_dir = root.join("build/paks/android");
        write_profile_pak(&root, "android", &closure, &pak_dir, None)?;
        let assets = root.join("assets");
        std::fs::remove_dir_all(assets.join("levels/sub.procgen"))?;
        std::fs::remove_dir_all(assets.join("levels/other.procgen"))?;
        std::fs::remove_dir_all(cooked_cache_dir(&root, "android"))?;
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "android".to_string(),
            content_dir: Some(pak_dir.clone()),
        })
        .map_err(|e| anyhow::anyhow!("asset server: {e}"))?;

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let main_path = assets.join("levels/main.level.ron");
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(BTreeMap::new()),
            bake_dir: Some(level_bake_dir(&main_path)),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.insert_resource(server);
        let report = regia_framework::level_assets::load_level_source(app.world_mut(), &main_path)
            .map_err(|e| anyhow::anyhow!("load: {e}"))?;
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        let loaded = app
            .world()
            .resource::<regia_framework::level_assets::LoadedLevelSources>()
            .clone();
        {
            let mut dirs = app.world_mut().resource_mut::<ProcLevelBakeDirs>();
            for (path, id) in &loaded.levels {
                dirs.0.insert(*id, level_bake_dir(path));
            }
        }
        let main = report.level.expect("main id");
        regia_scene::level::open_level(app.world_mut(), main, regia_scene::level::LoadMode::Replace)
            .map_err(|e| anyhow::anyhow!("open: {e}"))?;
        for _ in 0..5 {
            app.update();
        }
        let world = app.world_mut();
        let found: Vec<Entity> = world
            .query_filtered::<Entity, With<ProcGenerator>>()
            .iter(world)
            .collect();
        let shipped = regia_scene::placed::derive_guid(PLACEMENT, SUB_GEN);
        let generator = found
            .iter()
            .copied()
            .find(|e| {
                world
                    .get::<regia_scene::components::EntityGuid>(*e)
                    .is_some_and(|g| g.0 == shipped)
            })
            .expect("the shipping generator");
        let output = world
            .get::<ProcOutput>(generator)
            .expect("the sublevel's bake loaded from the pak");
        assert_eq!(output.0.instance_count(), 3);
        assert_eq!(
            world.get::<ProcGenState>(generator).map(|s| s.evaluations),
            Some(0),
            "loaded, not run"
        );
        let origin = world.get::<ProcBakeOrigin>(generator).expect("origin");
        assert_eq!(origin.source, ProcBakeSource::Pak);
        assert_eq!(origin.level, Some(regia_scene::level::LevelId(SUB_ID)));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    const SUB_TILES_GEN: Guid = Guid(0x7c7);

    /// `in.self → sample.region → points.jitter → spawn.mesh` as `s.pgraph.ron`.
    fn scatter_graphs() -> BTreeMap<String, String> {
        use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
        let node = |ty: &str, settings: &[(&str, &str)]| {
            settings.iter().fold(NodeEntry::new(ty), |n, (k, v)| {
                n.with(k, SettingValue::parse(v).expect("setting"))
            })
        };
        let graph = ProcGraph {
            nodes: [
                (1, node("in.self", &[])),
                (
                    2,
                    node("sample.region", &[("spacing_m", "5.0"), ("jitter", "0.6")]),
                ),
                (3, node("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
            ]
            .into_iter()
            .collect(),
            edges: vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")],
            ..ProcGraph::default()
        };
        BTreeMap::from([("s.pgraph.ron".to_string(), graph.to_ron())])
    }

    /// A `Baked` generator that streams its cells around tagged anchors (spec 66 § 4.3).
    fn tiled_generator() -> regia_procgen::world::ProcGenerator {
        use regia_procgen::tiles::{AnchorSettings, AnchorSource, TileSettings};
        use regia_procgen::world::{ProcBounds, ProcGenerator};
        ProcGenerator {
            bounds: ProcBounds::Box {
                half_extents: regia_core::math::Vec3::new(128.0, 10.0, 128.0),
            },
            seed: Some(11),
            tiles: Some(TileSettings {
                cell_m: 64.0,
                runtime: Some(AnchorSettings {
                    anchors: AnchorSource::Tagged,
                    base_reach_m: 80.0,
                    ..AnchorSettings::default()
                }),
                ..TileSettings::default()
            }),
            ..ProcGenerator::new("s.pgraph.ron")
        }
    }

    /// Adds the tiled generator `SUB_TILES_GEN` to the sublevel and bakes its cells beside it, as
    /// `regia procgen bake --level sub` does.
    fn add_tiled_generator(assets: &Path, sub: &mut Vec<SceneEntity>) {
        use regia_app::prelude::*;
        use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource, level_bake_dir};
        let mut scratch = App::new();
        scratch.add_plugins(regia_scene::ScenePlugin);
        scratch.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(scatter_graphs()),
            bake_dir: Some(level_bake_dir(&assets.join("levels/sub.level.ron"))),
            ..ProcGenSettings::default()
        });
        scratch.add_plugins(ProcGenPlugin);
        let e = scratch
            .world_mut()
            .spawn((
                tiled_generator(),
                regia_scene::components::EntityGuid(SUB_TILES_GEN),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        let scene = regia_scene::spawn::snapshot_scene(
            scratch.world_mut(),
            &regia_scene::spawn::SnapshotFilter::default(),
        )
        .expect("snapshot");
        sub.extend(scene.entities.into_iter().filter(|r| r.guid == SUB_TILES_GEN));
        regia_procgen::world::bake(scratch.world_mut(), e).unwrap_or_else(|(c, m)| panic!("bake: {c}: {m}"));
    }

    /// Acceptance (ADR-7287 (1)), packaged: an anchored generator of a placed sublevel streams its
    /// cells from the sublevel's bake in the pak (loose bakes and the cooked cache deleted).
    #[test]
    fn the_game_streams_a_placed_sublevels_cells_from_the_pak() -> Result<()> {
        use regia_app::prelude::*;
        use regia_procgen::tiles::{ProcGenAnchor, ProcGenTiles};
        use regia_procgen::world::{
            ProcBakeOrigin, ProcBakeSource, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator,
            ProcGraphSource, ProcLevelBakeDirs, level_bake_dir,
        };

        let root = project_with("profile-placed-cells-runtime", "android", add_tiled_generator);
        let closure = compute_closure(&root, "android", Some(&spec()))?;
        let pak_dir = root.join("build/paks/android");
        write_profile_pak(&root, "android", &closure, &pak_dir, None)?;
        let assets = root.join("assets");
        std::fs::remove_dir_all(assets.join("levels/sub.procgen"))?;
        std::fs::remove_dir_all(cooked_cache_dir(&root, "android"))?;
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "android".to_string(),
            content_dir: Some(pak_dir.clone()),
        })
        .map_err(|e| anyhow::anyhow!("asset server: {e}"))?;

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let main_path = assets.join("levels/main.level.ron");
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(scatter_graphs()),
            bake_dir: Some(level_bake_dir(&main_path)),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.insert_resource(server);
        let report = regia_framework::level_assets::load_level_source(app.world_mut(), &main_path)
            .map_err(|e| anyhow::anyhow!("load: {e}"))?;
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        let loaded = app
            .world()
            .resource::<regia_framework::level_assets::LoadedLevelSources>()
            .clone();
        {
            let mut dirs = app.world_mut().resource_mut::<ProcLevelBakeDirs>();
            for (path, id) in &loaded.levels {
                dirs.0.insert(*id, level_bake_dir(path));
            }
        }
        let main = report.level.expect("main id");
        regia_scene::level::open_level(app.world_mut(), main, regia_scene::level::LoadMode::Replace)
            .map_err(|e| anyhow::anyhow!("open: {e}"))?;
        app.world_mut().spawn((
            ProcGenAnchor::default(),
            regia_scene::Transform::default(),
            regia_scene::GlobalTransform::default(),
        ));
        for _ in 0..8 {
            app.update();
        }
        let world = app.world_mut();
        let found: Vec<Entity> = world
            .query_filtered::<Entity, With<ProcGenerator>>()
            .iter(world)
            .collect();
        let shipped = regia_scene::placed::derive_guid(PLACEMENT, SUB_TILES_GEN);
        let generator = found
            .iter()
            .copied()
            .find(|e| {
                world
                    .get::<regia_scene::components::EntityGuid>(*e)
                    .is_some_and(|g| g.0 == shipped)
            })
            .expect("the anchored generator");
        let tiles = world.resource::<ProcGenTiles>();
        assert_eq!(
            tiles.errors.get(&generator),
            None,
            "the cell bake was found in the pak"
        );
        let loaded = tiles
            .generators
            .get(&generator)
            .map_or(0, |g| g.counters.cells_loaded);
        assert!(loaded > 0, "cells streamed from the pak: {loaded}");
        assert_eq!(
            world.get::<ProcGenState>(generator).map(|s| s.evaluations),
            Some(0),
            "loaded, not run"
        );
        let origin = world.get::<ProcBakeOrigin>(generator).expect("origin");
        assert_eq!(origin.source, ProcBakeSource::Pak);
        assert_eq!(origin.level, Some(regia_scene::level::LevelId(SUB_ID)));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    // ADR-7331: a generator inside a prefab template of the placed sublevel, packaged.

    const TPL_ASSET: Guid = Guid(0x7e3a);
    const TPL_GEN_LOCAL: Guid = Guid(0x7e31);
    const TPL_INST: Guid = Guid(0x7e1a);

    /// A world with the scene and generator plugins (their types registered).
    fn scratch_app() -> regia_app::prelude::App {
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.add_plugins(regia_procgen::world::ProcGenPlugin);
        app
    }

    /// The prefab `TPL_ASSET`: one entity holding a `Baked` generator (a prefab internal).
    fn prefab_template(app: &regia_app::prelude::App) -> regia_scene::prefab::PrefabTemplate {
        let mut scene = SceneFile::new();
        scene.entities = vec![generator(TPL_GEN_LOCAL, false)];
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let registry = registry.read();
        regia_scene::prefab::PrefabTemplate::from_scene_file(TPL_ASSET, &scene, &registry).expect("template")
    }

    /// The sublevel's record: one instance `TPL_INST` of the prefab.
    fn template_instance() -> SceneEntity {
        SceneEntity {
            guid: TPL_INST,
            prefab_instance: Some(regia_scene::format::PrefabInstanceData::plain(
                regia_scene::format::SceneAssetRef(TPL_ASSET),
            )),
            ..SceneEntity::default()
        }
    }

    /// The guid the sublevel opened on its own gives the template's generator (what
    /// `regia procgen bake --level sub` names its file after).
    fn template_generator_guid_in_sub() -> Guid {
        use regia_app::prelude::*;
        let mut app = scratch_app();
        let t = prefab_template(&app);
        app.world_mut()
            .resource_mut::<regia_scene::prefab::PrefabLibrary>()
            .insert(t);
        let mut sub = SceneFile::new();
        sub.entities = vec![template_instance()];
        regia_scene::spawn::spawn_scene(app.world_mut(), &sub, &Default::default()).expect("spawn sub");
        let world = app.world_mut();
        let found: Vec<Guid> = world
            .query_filtered::<&regia_scene::components::EntityGuid, With<regia_scene::prefab::PrefabLink>>()
            .iter(world)
            .map(|g| g.0)
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        found[0]
    }

    /// Acceptance (ADR-7331 (1)), packaged: a generator inside a prefab template of the placed
    /// sublevel loads the sublevel's bake from the pak (loose bakes and the cooked cache deleted)
    /// without evaluating.
    #[test]
    fn the_game_loads_a_placed_template_generators_bake_from_the_pak() -> Result<()> {
        use regia_app::prelude::*;
        use regia_procgen::world::{
            ProcBakeOrigin, ProcBakeSource, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator,
            ProcGraphSource, ProcLevelBakeDirs, ProcOutput, level_bake_dir,
        };
        let source = template_generator_guid_in_sub();
        let root = project_with("profile-placed-template-runtime", "android", |assets, sub| {
            sub.push(template_instance());
            write_bake(assets, "levels/sub.level.ron", source);
        });
        let closure = compute_closure(&root, "android", Some(&spec()))?;
        let pak_dir = root.join("build/paks/android");
        write_profile_pak(&root, "android", &closure, &pak_dir, None)?;
        let assets = root.join("assets");
        std::fs::remove_dir_all(assets.join("levels/sub.procgen"))?;
        std::fs::remove_dir_all(cooked_cache_dir(&root, "android"))?;
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "android".to_string(),
            content_dir: Some(pak_dir.clone()),
        })
        .map_err(|e| anyhow::anyhow!("asset server: {e}"))?;

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let main_path = assets.join("levels/main.level.ron");
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(BTreeMap::new()),
            bake_dir: Some(level_bake_dir(&main_path)),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.insert_resource(server);
        let t = prefab_template(&app);
        app.world_mut()
            .resource_mut::<regia_scene::prefab::PrefabLibrary>()
            .insert(t);
        let report = regia_framework::level_assets::load_level_source(app.world_mut(), &main_path)
            .map_err(|e| anyhow::anyhow!("load: {e}"))?;
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        let loaded = app
            .world()
            .resource::<regia_framework::level_assets::LoadedLevelSources>()
            .clone();
        {
            let mut dirs = app.world_mut().resource_mut::<ProcLevelBakeDirs>();
            for (path, id) in &loaded.levels {
                dirs.0.insert(*id, level_bake_dir(path));
            }
        }
        let main = report.level.expect("main id");
        regia_scene::level::open_level(app.world_mut(), main, regia_scene::level::LoadMode::Replace)
            .map_err(|e| anyhow::anyhow!("open: {e}"))?;
        for _ in 0..5 {
            app.update();
        }
        let world = app.world_mut();
        let found: Vec<(Entity, Guid)> = world
            .query_filtered::<(Entity, &regia_scene::components::EntityGuid), (
                With<ProcGenerator>,
                With<regia_scene::prefab::PrefabLink>,
            )>()
            .iter(world)
            .map(|(e, g)| (e, g.0))
            .collect();
        assert_eq!(found.len(), 1, "the template generator spawned: {found:?}");
        let (generator, member) = found[0];
        assert_ne!(member, source, "the placement derives the member's guid");
        let output = world
            .get::<ProcOutput>(generator)
            .expect("the template generator's bake loaded from the pak");
        assert_eq!(output.0.instance_count(), 3);
        assert_eq!(
            world.get::<ProcGenState>(generator).map(|s| s.evaluations),
            Some(0),
            "loaded, not run"
        );
        let origin = world.get::<ProcBakeOrigin>(generator).expect("origin");
        assert_eq!(origin.source, ProcBakeSource::Pak);
        assert_eq!(origin.level, Some(regia_scene::level::LevelId(SUB_ID)));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// A cooked mesh over three vertices with one LOD per entry of `tris`, one submesh on LOD0.
    fn diet_mesh(tris: &[u32]) -> regia_asset::CookedMesh {
        let mut indices = Vec::new();
        let mut lods = Vec::new();
        for (level, t) in tris.iter().enumerate() {
            let offset = indices.len() as u32;
            for _ in 0..*t {
                for i in [0u16, 1, 2] {
                    indices.extend_from_slice(&i.to_le_bytes());
                }
            }
            lods.push(regia_asset::CookedMeshLod {
                screen_relative_size: if level + 1 == tris.len() { 0.0 } else { 0.5 },
                index_byte_offset: offset,
                index_byte_length: indices.len() as u32 - offset,
            });
        }
        let submeshes = vec![regia_asset::CookedSubmesh {
            index_byte_offset: 0,
            index_byte_length: lods[0].index_byte_length,
            material_slot: 0,
        }];
        regia_asset::CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0; 3],
            bounding_sphere: [0.5, 0.5, 0.5, 0.9],
            stream_positions: vec![0; 36],
            stream_normals_tangents: vec![0; 24],
            stream_uv0: vec![0; 12],
            indices,
            index_width: regia_asset::CookedIndexWidth::U16,
            vertex_count: 3,
            lods,
            submeshes,
            lod_metric: regia_asset::LOD_METRIC_ERROR,
            format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
        }
    }

    /// A profile's cook diet (ADR-8461, row C-04): `[content.diet]` with the profile's
    /// `[profiles.<name>.diet]` on top. The pak leaves out the variant the rule does not pick, ships
    /// the largest mesh without its finest LOD, keeps the other meshes whole, and
    /// `diet-report.json` beside it records the totals; without a diet no report is left behind.
    #[test]
    fn a_profile_diet_trims_lods_and_variants_in_the_pak() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("regia_cli_profile_diet");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"Diet\"\nid = \"com.example.diet\"\n\n[content.diet]\nmax_triangles = 1000\n\
             variants = [{ glob = \"props/rocks/*\", pick = \"first\" }]\n\n[profiles.lean.diet]\nmax_triangles = 100\n",
        )?;
        let cooked = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cooked)?;
        let mesh_type = "regia_asset::CookedMesh";
        let assets: Vec<ClosureAsset> = [
            (1u128, "assets/hulls/big.glb", vec![90u32, 30, 5]),
            (2, "assets/hulls/small.glb", vec![20, 4]),
            (3, "assets/props/rocks/rock_a.glb", vec![2]),
            (4, "assets/props/rocks/rock_b.glb", vec![2]),
        ]
        .into_iter()
        .map(|(n, path, tris)| {
            let id = AssetId(regia_core::id::Guid::from_u128(n));
            let payload = diet_mesh(&tris).to_postcard().expect("postcard");
            let header = regia_asset::CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: TypePathHash::of(mesh_type),
                asset_id: id,
                importer_version: 1,
                dependencies: Vec::new(),
                payload_len: payload.len() as u64,
            };
            let file = regia_asset::encode_cooked_file(&header, &payload).expect("encode");
            std::fs::write(cooked.join(cooked_file_name(id)), &file).expect("cooked file");
            ClosureAsset {
                path: path.to_string(),
                id,
                type_path: mesh_type.to_string(),
                bytes: file.len() as u64,
            }
        })
        .collect();
        // The cook writes a registry and an asset manifest beside the cooked files, and a profile pak
        // refuses shipped ids the registry does not know (`unknown_registry_id`) or whose cooked file
        // the manifest does not list (`missing_cooked_file`, ADR-8360): this hand-made cache needs both.
        AssetManifest {
            entries: assets
                .iter()
                .map(|a| ManifestEntry {
                    path: a.path.clone(),
                    id: a.id,
                    type_path: a.type_path.clone(),
                })
                .collect(),
        }
        .write_to_cache(&cooked)?;
        regia_asset::registry::AssetRegistry::from_entries(assets.iter().map(|a| {
            regia_asset::registry::RegistryEntry {
                id: a.id,
                path: a.path.clone(),
                type_path: a.type_path.clone(),
                dependencies: Vec::new(),
                tags: BTreeMap::new(),
                size: a.bytes,
            }
        }))
        .write_to(&regia_asset::registry::cache_registry_path(&cooked))?;
        let closure = Closure {
            assets: assets.clone(),
            ..Closure::default()
        };
        let lean =
            regia_asset_pipeline::cook_diet::DietRules::for_profile(&root, Some("lean"))?.expect("diet");
        assert_eq!(
            lean.max_triangles, 100,
            "the profile's key replaces the project's"
        );
        let dir = profile_pak_dir(&root, "lean", "linux");
        let (_, report) =
            write_profile_pak_with_diet(&root, "linux", &closure, &dir, None, Some(("lean", &lean)))?;
        let report = report.expect("a diet report");
        // 90 + 20 + 2 = 112 > 100 once rock_b is left out: big loses LOD0 → 30 + 20 + 2 = 52.
        // `before` counts the whole closure, rock_b included: 114.
        assert_eq!(report.before.triangles, 114);
        assert_eq!(report.after.triangles, 52);
        assert!(report.ceiling_reached);
        assert_eq!(report.lods.len(), 1);
        assert_eq!(report.lods[0].path, "assets/hulls/big.glb");

        let reader = regia_asset::PakReader::open(&dir.join("base.fpak"))?;
        assert!(
            reader.entry(assets[3].id).is_none(),
            "the variant `first` does not pick is left out"
        );
        assert!(reader.entry(assets[2].id).is_some(), "the picked variant ships");
        let lods = |id: AssetId| -> Result<Vec<u64>> {
            let file = reader.read_decompressed(id)?;
            let (_, payload) = regia_asset::decode_cooked_file(&file)?;
            let mesh = regia_asset::CookedMesh::from_postcard(payload)?;
            Ok(regia_asset_pipeline::cook_diet::mesh_lod_triangles(&mesh))
        };
        assert_eq!(
            lods(assets[0].id)?,
            vec![30, 5],
            "the big mesh ships without its finest LOD"
        );
        assert_eq!(lods(assets[1].id)?, vec![20, 4], "the small mesh ships whole");

        let json: regia_asset_pipeline::cook_diet::DietReport =
            serde_json::from_slice(&std::fs::read(dir.join("diet-report.json"))?)?;
        assert_eq!(json, report);
        let line = diet_summary(&report);
        assert!(
            line.contains("1 mesh(es) lose LODs, 1 variant(s) left out; triangles 114 -> 52 (ceiling 100)"),
            "{line}"
        );

        // Without a diet the pak ships everything and the stale report goes.
        write_profile_pak(&root, "linux", &closure, &dir, None)?;
        assert!(
            !dir.join("diet-report.json").exists(),
            "a stale diet report is removed"
        );
        let reader = regia_asset::PakReader::open(&dir.join("base.fpak"))?;
        assert!(reader.entry(assets[3].id).is_some());
        drop(reader);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
