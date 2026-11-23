// SPDX-License-Identifier: MPL-2.0
//! `regia assets query` and `regia assets tags`: search the cook's asset registry without loading
//! any asset (`docs/specs/content-data.md` § 2.4, book `user-guide/asset-registry.md`).
//!
//! The registry is `asset_registry.bin` in the project's cooked cache for `--platform` (written by
//! every cook), or the file given with `--registry` (a `<pak>.registry.bin` next to a shipped pak).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_asset::registry::{AssetRegistry, RegistryEntry, RegistryQuery, TagFilter, cache_registry_path};
use serde_json::{Value, json};

/// `regia assets` subcommands.
#[derive(Debug, Subcommand)]
pub enum AssetsCommand {
    /// Find assets by tag, type and path, or walk one asset's dependencies or referencers.
    Query(AssetsQueryArgs),
    /// List tag keys with how many assets carry each, or one key's values.
    Tags(AssetsTagsArgs),
    /// Chunks of the project: delivery, asset count, disk and memory size, size by type (specs/40).
    Chunks(super::content::AssetsChunksArgs),
    /// Why an asset is in its chunk: the rule, or the referrer chain to a direct asset.
    Why(super::content::AssetsWhyArgs),
    /// Run the asset validators over the cooked registry (exit 1 on failure, 2 on a bad config).
    Validate(super::assets_validate::AssetsValidateArgs),
    /// Reference graph of one or more assets: referrers and dependencies, depth and width limited.
    Graph(super::assets_validate::AssetsGraphArgs),
    /// Footprint of an asset and everything it pulls in, with exclusive and shared bytes.
    SizeMap(super::assets_validate::AssetsSizeMapArgs),
    /// Write a CSV table of assets with their sizes, reference counts and closure sizes.
    Audit(super::assets_validate::AssetsAuditArgs),
    /// A streamed level's partition: cells, entities and bytes per cell, warnings (spec 78).
    StreamReport(super::stream_report::StreamReportArgs),
    /// Report and optionally write a font subset for the shipped or named locales (ADR-2697).
    FontSubset(super::assets_font_subset::AssetsFontSubsetArgs),
    /// Make an Ed25519 key pair for signing content manifests (private key outside the project).
    Keygen(super::assets_trust::AssetsKeygenArgs),
    /// Sign a content manifest: writes `content-manifest.ron.sig` next to it (ADR-2710).
    Sign(super::assets_trust::AssetsSignArgs),
    /// Effective import settings per source and the rule findings: `import_rule_unused`,
    /// `import_rule_unknown_field`, bad values (exit 1 on findings, 2 on a broken config).
    ImportRules(super::assets_import_rules::ImportRulesArgs),
    /// What shipping the new build's paks over the old one's costs: per-asset changes, chunk
    /// comparison and an estimated patch download (spec 22 T2.14, ADR-3322).
    PakDiff(super::assets_pak_diff::AssetsPakDiffArgs),
    /// Detect PBR texture sets in a folder and generate one `.material.ron` per set
    /// (spec 22 T2.14, ADR-3320; dry run unless `--write`).
    TextureSets(super::assets_texture_sets::AssetsTextureSetsArgs),
    /// Judge glTF assets against a `.contract.ron` and write a vendor report (ADR-8450).
    ContractCheck(super::assets_contract::AssetsContractCheckArgs),
    /// Check glTF source material metadata; `--fix` atomically repairs authoring violations.
    LintMaterials(super::assets_lint_materials::MaterialLintArgs),
    /// Stable ids for sources not in version control: `export` writes every `.meta` id to a
    /// `.assetids.ron` manifest, `import` writes the missing sidecars back (ADR-8460).
    #[command(subcommand)]
    Ids(super::assets_ids::AssetsIdsCommand),
    /// Check glTF source geometry. Findings fail the command. The command does not repair sources.
    LintGeometry(super::assets_lint_geometry::GeometryLintArgs),
    /// Move pack provenance records (`provenance/<pack>.toml`) into each source's `.meta`
    /// provenance block; all or nothing, idempotent (ADR-8834; dry run with `--dry-run`).
    ProvenanceImport(super::assets_provenance_import::AssetsProvenanceImportArgs),
}

/// Where the registry is read from.
#[derive(Debug, Args)]
pub struct RegistrySource {
    /// Project directory (contains `assets/` and `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Cooked platform whose registry to read (`linux`, `windows`, `android`). Defaults to host.
    #[arg(long)]
    pub platform: Option<String>,
    /// Read this registry file instead (`base.registry.bin` next to a pak, or a cache's
    /// `asset_registry.bin`).
    #[arg(long, value_name = "FILE")]
    pub registry: Option<PathBuf>,
    /// Print JSON on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia assets query` arguments.
#[derive(Debug, Args)]
pub struct AssetsQueryArgs {
    /// Registry location and output form.
    #[command(flatten)]
    pub source: RegistrySource,
    /// Tag filter, `key` (present) or `key=value` (equal); repeat for AND.
    #[arg(long = "tag", value_name = "KEY[=VALUE]")]
    pub tags: Vec<String>,
    /// Cooked type path, or its last segments (`CookedTexture`).
    #[arg(long = "type", value_name = "TYPE")]
    pub type_path: Option<String>,
    /// Path glob (`assets/props/**`): `*` inside a folder, `**` across folders.
    #[arg(long, value_name = "GLOB")]
    pub path: Option<String>,
    /// Substring of the path, any case.
    #[arg(long, value_name = "TEXT")]
    pub contains: Option<String>,
    /// List what this asset (id or path) depends on instead of searching.
    #[arg(long, value_name = "ASSET", conflicts_with = "refs")]
    pub deps: Option<String>,
    /// List what references this asset (id or path) instead of searching.
    #[arg(long, value_name = "ASSET")]
    pub refs: Option<String>,
    /// With `--deps` / `--refs`: follow the graph all the way, not one step.
    #[arg(long, default_value_t = false)]
    pub transitive: bool,
    /// Most records printed.
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
}

/// `regia assets tags` arguments.
#[derive(Debug, Args)]
pub struct AssetsTagsArgs {
    /// Registry location and output form.
    #[command(flatten)]
    pub source: RegistrySource,
    /// List this key's values instead of the keys.
    #[arg(long)]
    pub key: Option<String>,
}

/// Runs an `assets` subcommand, printing to stdout.
///
/// # Errors
///
/// No registry, a registry that does not decode, a bad filter or an unknown asset.
pub fn run(command: &AssetsCommand) -> Result<()> {
    let out = match command {
        AssetsCommand::Query(args) => query(args)?,
        AssetsCommand::Tags(args) => tags(args)?,
        AssetsCommand::Chunks(args) if args.max_size.is_some() || args.max_total.is_some() => {
            let (text, over) = super::assets_budget::gate(args)?;
            println!("{text}");
            if over {
                bail!("content_size_budget: chunks over their size budget (table above)");
            }
            return Ok(());
        }
        AssetsCommand::Chunks(args) => super::content::chunks_report(args)?,
        AssetsCommand::Why(args) => super::content::why_report(args)?,
        AssetsCommand::Validate(args) => {
            let (text, failed) = super::assets_validate::validate(args)?;
            println!("{text}");
            if failed {
                bail!("asset validation failed");
            }
            return Ok(());
        }
        AssetsCommand::Graph(args) => super::assets_validate::graph(args)?,
        AssetsCommand::SizeMap(args) => super::assets_validate::size_map_text(args)?,
        AssetsCommand::Audit(args) => {
            let rows = super::assets_validate::audit(args)?;
            println!("wrote {rows} rows to {}", args.csv.display());
            return Ok(());
        }
        AssetsCommand::StreamReport(args) => super::stream_report::run(args)?,
        AssetsCommand::FontSubset(args) => super::assets_font_subset::run(args)?,
        AssetsCommand::Keygen(args) => super::assets_trust::keygen(args)?,
        AssetsCommand::Sign(args) => super::assets_trust::sign(args)?,
        AssetsCommand::ImportRules(args) => {
            super::assets_import_rules::run(args)?;
            return Ok(());
        }
        AssetsCommand::PakDiff(args) => super::assets_pak_diff::run(args)?,
        AssetsCommand::TextureSets(args) => super::assets_texture_sets::run(args)?,
        AssetsCommand::ContractCheck(args) => {
            super::assets_contract::run(args)?;
            return Ok(());
        }
        AssetsCommand::LintMaterials(args) => return super::assets_lint_materials::run(args),
        AssetsCommand::Ids(command) => super::assets_ids::run(command)?,
        AssetsCommand::LintGeometry(args) => return super::assets_lint_geometry::run(args),
        AssetsCommand::ProvenanceImport(args) => super::assets_provenance_import::run(args)?,
    };
    println!("{out}");
    Ok(())
}

/// Reads the registry named by `source`.
///
/// # Errors
///
/// The file is missing or does not decode.
pub fn load(source: &RegistrySource) -> Result<AssetRegistry> {
    let path = match &source.registry {
        Some(file) => file.clone(),
        None => {
            let platform = source
                .platform
                .clone()
                .unwrap_or_else(|| regia_asset_pipeline::host_platform().to_string());
            cache_registry_path(&regia_asset::cooked_cache_dir(&source.project, &platform))
        }
    };
    read(&path)
}

fn read(path: &Path) -> Result<AssetRegistry> {
    match AssetRegistry::read_from(path).with_context(|| format!("reading {}", path.display()))? {
        Some(registry) => Ok(registry),
        None => bail!(
            "no asset registry at {} (run `regia cook` first, or pass --registry)",
            path.display()
        ),
    }
}

fn record_json(entry: &RegistryEntry) -> Value {
    json!({
        "id": entry.id.to_string(),
        "path": entry.path,
        "type_path": entry.type_path,
        "tags": entry.tags,
        "size": entry.size,
        "dependencies": entry.dependencies.iter().map(ToString::to_string).collect::<Vec<_>>(),
    })
}

fn record_line(entry: &RegistryEntry) -> String {
    let tags: Vec<String> = entry.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{}  {}  [{}]", entry.path, entry.type_path, tags.join(" "))
}

/// The text (or JSON) `regia assets query` prints.
///
/// # Errors
///
/// See [`run`].
pub fn query(args: &AssetsQueryArgs) -> Result<String> {
    let registry = load(&args.source)?;
    query_registry(&registry, args)
}

/// [`query`] over an already loaded registry.
///
/// # Errors
///
/// A bad tag filter or an unknown `--deps` / `--refs` asset.
pub fn query_registry(registry: &AssetRegistry, args: &AssetsQueryArgs) -> Result<String> {
    let walk_from = args
        .deps
        .as_deref()
        .map(|a| (a, true))
        .or_else(|| args.refs.as_deref().map(|a| (a, false)));
    let (total, entries, missing) = match walk_from {
        Some((asset, deps)) => {
            let Some(start) = registry.resolve(asset) else {
                bail!("the asset registry has no record for {asset}");
            };
            let walk = if deps {
                registry.dependencies(start.id, args.transitive)
            } else {
                registry.referencers(start.id, args.transitive)
            };
            let total = walk.entries.len();
            let entries: Vec<&RegistryEntry> = walk.entries.into_iter().take(args.limit).collect();
            (total, entries, walk.missing)
        }
        None => {
            let tags = args
                .tags
                .iter()
                .map(|t| TagFilter::parse(t))
                .collect::<Result<Vec<_>, _>>()?;
            let matches = registry.query(&RegistryQuery {
                path: args.path.clone(),
                path_contains: args.contains.clone(),
                type_path: args.type_path.clone(),
                tags,
                limit: Some(args.limit),
            });
            (matches.total, matches.entries, Vec::new())
        }
    };
    if args.source.json {
        let value = json!({
            "total": total,
            "returned": entries.len(),
            "entries": entries.iter().map(|e| record_json(e)).collect::<Vec<_>>(),
            "missing": missing.iter().map(ToString::to_string).collect::<Vec<_>>(),
        });
        return Ok(serde_json::to_string_pretty(&value)?);
    }
    let mut lines: Vec<String> = entries.iter().map(|e| record_line(e)).collect();
    for id in &missing {
        lines.push(format!("(no record)  {id}"));
    }
    lines.push(format!("{} of {total} shown", entries.len()));
    Ok(lines.join("\n"))
}

/// The text (or JSON) `regia assets tags` prints.
///
/// # Errors
///
/// See [`run`].
pub fn tags(args: &AssetsTagsArgs) -> Result<String> {
    let registry = load(&args.source)?;
    let counts = match &args.key {
        Some(key) => registry.tag_values(key),
        None => registry.tag_keys(),
    };
    if args.source.json {
        return Ok(serde_json::to_string_pretty(&counts)?);
    }
    Ok(counts
        .iter()
        .map(|(name, n)| format!("{name}  {n}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetId;
    use regia_asset::registry::automatic_tags;
    use regia_core::id::Guid;

    fn rec(n: u128, path: &str, deps: &[u128]) -> RegistryEntry {
        RegistryEntry {
            id: AssetId(Guid::from_u128(n)),
            path: path.into(),
            type_path: "regia_asset::CookedTexture".into(),
            dependencies: deps.iter().map(|d| AssetId(Guid::from_u128(*d))).collect(),
            tags: automatic_tags(path),
            size: 1,
        }
    }

    fn args(source: RegistrySource) -> AssetsQueryArgs {
        AssetsQueryArgs {
            source,
            tags: Vec::new(),
            type_path: None,
            path: None,
            contains: None,
            deps: None,
            refs: None,
            transitive: false,
            limit: 200,
        }
    }

    #[test]
    fn query_and_walk_from_a_registry_file() {
        let dir = regia_test_utils::unique_temp_dir("cli-assets-query");
        let file = dir.join("base.registry.bin");
        AssetRegistry::from_entries([
            rec(1, "assets/props/a.png", &[2]),
            rec(2, "assets/props/b.png", &[]),
            rec(3, "assets/ui/c.png", &[2]),
        ])
        .write_to(&file)
        .expect("write");
        let source = || RegistrySource {
            project: dir.clone(),
            platform: None,
            registry: Some(file.clone()),
            json: true,
        };
        let mut q = args(source());
        q.tags = vec!["folder=props".into()];
        let out: Value = serde_json::from_str(&query(&q).expect("query")).expect("json");
        assert_eq!(out["total"], 2);
        let mut q = args(source());
        q.refs = Some("props/b.png".into());
        let out: Value = serde_json::from_str(&query(&q).expect("refs")).expect("json");
        assert_eq!(out["total"], 2);
        let mut q = args(source());
        q.deps = Some("nope.png".into());
        assert!(query(&q).is_err());
        let mut q = args(source());
        q.tags = vec!["bad key".into()];
        assert!(query(&q).is_err());
        let t = tags(&AssetsTagsArgs {
            source: RegistrySource {
                json: false,
                ..source()
            },
            key: Some("folder".into()),
        })
        .expect("tags");
        assert_eq!(t, "props  2\nui  1");
        let missing = RegistrySource {
            registry: Some(dir.join("none.bin")),
            ..source()
        };
        assert!(load(&missing).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
