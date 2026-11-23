// SPDX-License-Identifier: MPL-2.0
//! `regia viewer`: product viewer packages (track JW, ADR-2540, book `agents/viewer-pack.md`).
//!
//! - `regia viewer pack <kit> <recipe> -o <dir>`: cook a product package (quantized meshes, recipe,
//!   option data, material entries, manifest with BLAKE3 hashes) and print its size report;
//! - `regia viewer verify <dir>`: check every hash and decode the package;
//! - `regia viewer standin -o <dir>`: write the generic stand-in kit, recipes, material library
//!   and option records (no supplier data), to cook and test against until real kits exist;
//!   `--pack` cooks them and writes the static demo page beside the packages.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_viewer_pack::cook::{self, CookOptions};

/// `regia viewer` subcommands.
#[derive(Debug, Subcommand)]
pub enum ViewerCommand {
    /// Cook a product package from a part kit folder and a piece recipe; prints the size report.
    Pack(PackArgs),
    /// Verify a package folder: every file's size and BLAKE3, the package hash, the mesh file.
    Verify(VerifyArgs),
    /// Write the generic stand-in kit, recipes, material library and option records.
    Standin(StandinArgs),
}

/// `regia viewer pack`.
#[derive(Debug, Args)]
pub struct PackArgs {
    /// The part kit folder (`kit.json` of `jewelry.part_kit/1` plus its GLB).
    pub kit: PathBuf,
    /// The piece recipe (`jewelry.piece_recipe/1`).
    pub recipe: PathBuf,
    /// Output package folder (created; the package's files in it are replaced).
    #[arg(short = 'o', long = "out")]
    pub out: PathBuf,
    /// Another kit folder the recipe uses (repeatable; e.g. the shared kit beside a design kit).
    #[arg(long = "kit")]
    pub extra_kits: Vec<PathBuf>,
    /// The product's option record; copied into options.json without cost fields.
    #[arg(long)]
    pub option_graph: Option<PathBuf>,
    /// A material library (`jewelry.material_library/1`); the entries the options pick are kept.
    #[arg(long)]
    pub materials: Option<PathBuf>,
    /// Longest texture side written, in pixels.
    #[arg(long, default_value_t = 512)]
    pub max_texture_px: u32,
    /// Print the manifest, the size report and the warnings as one JSON object.
    #[arg(long)]
    pub json: bool,
    /// Exit 1 when the package is over the size budget.
    #[arg(long)]
    pub strict_budget: bool,
}

/// `regia viewer verify`.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// The package folder.
    pub dir: PathBuf,
    /// Print the size report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// `regia viewer standin`.
#[derive(Debug, Args)]
pub struct StandinArgs {
    /// Output folder.
    #[arg(short = 'o', long = "out")]
    pub out: PathBuf,
    /// Also cook every stand-in recipe into `<out>/packages/<name>/` and write the static demo page
    /// into `<out>/demo/`.
    #[arg(long)]
    pub pack: bool,
}

fn kib(b: u64) -> String {
    format!("{:.1} KiB", b as f64 / 1024.0)
}

/// Run a `regia viewer` subcommand.
pub fn run(command: &ViewerCommand) -> Result<()> {
    match command {
        ViewerCommand::Pack(a) => {
            let opts = CookOptions {
                extra_kits: a.extra_kits.clone(),
                option_graph: a.option_graph.clone(),
                materials: a.materials.clone(),
                max_texture_px: a.max_texture_px,
                gzip: true,
            };
            let r = cook::pack(&a.kit, &a.recipe, &a.out, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
            if a.json {
                let doc = serde_json::json!({"manifest": r.manifest, "report": r.report.to_json(), "warnings": r.warnings});
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                println!(
                    "package {} -> {}",
                    r.manifest["package_id"].as_str().unwrap_or("?"),
                    a.out.display()
                );
                for f in &r.report.files {
                    let gz = f.gzip_bytes.map(kib).unwrap_or_default();
                    println!(
                        "  {:<28} {:<10} {:>12}  gzip {:>12}",
                        f.path,
                        f.kind,
                        kib(f.bytes),
                        gz
                    );
                }
                println!(
                    "  total {} (gzip {}), {} meshes, {} triangles; budget {}: {}",
                    kib(r.report.total_bytes),
                    r.report.total_gzip_bytes.map(kib).unwrap_or_default(),
                    r.report.meshes,
                    r.report.triangles,
                    kib(r.report.budget_bytes),
                    if r.report.within_budget { "ok" } else { "OVER" }
                );
                for w in &r.warnings {
                    println!("  warning: {w}");
                }
            }
            if a.strict_budget && !r.report.within_budget {
                bail!("package is over the size budget");
            }
            Ok(())
        }
        ViewerCommand::Verify(a) => {
            let pkg = regia_viewer_pack::read_dir(&a.dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            let manifest_bytes = std::fs::metadata(a.dir.join(regia_viewer_pack::MANIFEST_FILE))
                .map(|m| m.len())
                .unwrap_or(0);
            let report = regia_viewer_pack::SizeReport::of(&pkg, manifest_bytes);
            if a.json {
                let doc = serde_json::json!({"ok": true, "package_hash": pkg.manifest.package_hash, "report": report.to_json()});
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                println!(
                    "ok: {} files, {} meshes, {} triangles, {} ({})",
                    pkg.manifest.files.len(),
                    pkg.meshes.len(),
                    pkg.triangles(),
                    kib(report.total_bytes),
                    pkg.manifest.package_hash
                );
            }
            Ok(())
        }
        ViewerCommand::Standin(a) => {
            let set = regia_viewer_pack::standin::write(&a.out).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("kit      {}", set.kit_dir.display());
            println!("library  {}", set.library.display());
            for (r, og) in set.recipes.iter().zip(&set.option_graphs) {
                println!("recipe   {}", r.display());
                if a.pack {
                    let name = r.file_stem().and_then(|s| s.to_str()).unwrap_or("package");
                    let out = a.out.join("packages").join(name);
                    let opts = CookOptions {
                        option_graph: Some(og.clone()),
                        materials: Some(set.library.clone()),
                        ..CookOptions::default()
                    };
                    let res = cook::pack(&set.kit_dir, r, &out, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
                    println!(
                        "package  {} ({}, gzip {})",
                        out.display(),
                        kib(res.report.total_bytes),
                        res.report.total_gzip_bytes.map(kib).unwrap_or_default()
                    );
                }
            }
            if a.pack {
                let demo =
                    regia_viewer_pack::standin::write_demo(&a.out).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!(
                    "demo     {}/index.html?product=../packages/<name> (serve {})",
                    demo.display(),
                    a.out.display()
                );
            }
            Ok(())
        }
    }
}
