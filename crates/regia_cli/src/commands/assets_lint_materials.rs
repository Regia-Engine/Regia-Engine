// SPDX-License-Identifier: MPL-2.0
//! `regia assets lint-materials`: source material checks and opt-in repair (ADR-8061, ADR-8250).
use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use regia_asset_pipeline::gltf_material_lint::{
    MaterialLintProfile, MaterialLintSettings, importer_capability,
};
use regia_asset_pipeline::source_lint_cache::adapt_material;
use serde_json::json;

/// CLI selection for `MaterialLintProfile`.
///
/// `compatibility` changes authored appearance. The default is `standard`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MaterialProfileFlag {
    /// Preserve glTF factor multiplication, missing-factor defaults and surface tint.
    Standard,
    /// Select ORM=1, dielectric missing-metal and volume-tint migration. These change appearance.
    Compatibility,
}

impl From<MaterialProfileFlag> for MaterialLintProfile {
    fn from(flag: MaterialProfileFlag) -> Self {
        match flag {
            MaterialProfileFlag::Standard => Self::Standard,
            MaterialProfileFlag::Compatibility => Self::Compatibility,
        }
    }
}

/// glTF material lint arguments.
#[derive(Debug, Args)]
pub struct MaterialLintArgs {
    /// Source .gltf/.glb files or folders (recursive, symlink folders not followed).
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Atomically replace material metadata in each source with repairs.
    #[arg(long)]
    pub fix: bool,
    /// JSON authoring policy; omitted fields use the documented defaults.
    /// A `profile` in this file is overridden by `--profile`.
    #[arg(long, value_name = "JSON")]
    pub settings: Option<PathBuf>,
    /// Authoring profile. `standard` preserves valid glTF appearance.
    /// `compatibility` selects appearance-changing ORM=1, dielectric missing-metal
    /// and volume-tint repairs. Overrides `profile` in `--settings`.
    #[arg(long, value_enum)]
    pub profile: Option<MaterialProfileFlag>,
    /// Skip files and folders whose names begin with an underscore.
    #[arg(long)]
    pub skip_staging: bool,
    /// Print one JSON report, including per-file failures.
    #[arg(long)]
    pub json: bool,
}

/// Inspect all selected sources. Exit fails when any file fails or findings remain.
///
/// # Errors
/// Bad paths, policy, file IO, or unresolved material findings.
pub fn run(args: &MaterialLintArgs) -> Result<()> {
    let mut settings: MaterialLintSettings = match &args.settings {
        Some(path) => serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("reading policy {}", path.display()))?,
        )
        .context("gltf_material_settings: invalid policy JSON")?,
        None => MaterialLintSettings::default(),
    };
    if let Some(profile) = args.profile {
        settings.profile = profile.into();
    }
    let mut files = BTreeSet::new();
    for path in &args.paths {
        if path.is_dir() {
            if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
                bail!(
                    "gltf_material_io: source folder must not be a symlink: {}",
                    path.display()
                );
            }
            for entry in walkdir::WalkDir::new(path)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    !args.skip_staging || e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('_')
                })
            {
                let entry = entry.with_context(|| format!("scanning {}", path.display()))?;
                if entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.eq_ignore_ascii_case("gltf") || s.eq_ignore_ascii_case("glb"))
                {
                    files.insert(entry.into_path());
                }
            }
        } else {
            files.insert(path.clone());
        }
    }
    if files.is_empty() {
        bail!("gltf_material_io: no glTF sources selected");
    }
    let mut per_file = Vec::new();
    let mut failed = false;
    for path in files {
        match adapt_material(&path, &settings, args.fix) {
            Ok(report) => {
                let ok = report.remaining.is_empty() && report.compatible;
                failed |= !ok;
                per_file.push(json!({"path":path.to_string_lossy(),"ok":ok,"report":report}));
            }
            Err(error) => {
                failed = true;
                per_file.push(json!({"path":path.to_string_lossy(),"ok":false,"error":{"code":error.code,"message":error.message}}));
            }
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": !failed,
                "profile": settings.profile.name(),
                "policy": settings.profile.summary(),
                "importer": importer_capability(),
                "files": per_file
            }))?
        );
    } else {
        println!(
            "profile: {} — {}",
            settings.profile.name(),
            settings.profile.summary()
        );
        for entry in &per_file {
            println!(
                "{}: {}",
                entry["path"].as_str().unwrap_or(""),
                if entry["ok"] == true { "OK" } else { "FAILED" }
            );
            if let Some(error) = entry.get("error") {
                println!("  {}: {}", error["code"], error["message"]);
            }
            if let Some(findings) = entry["report"]["findings"].as_array() {
                for f in findings {
                    println!(
                        "  {} {} {}: {}",
                        f["effect"].as_str().unwrap_or("preserves_appearance"),
                        f["code"],
                        f["pointer"],
                        f["message"]
                    );
                }
            }
            if entry["report"]["changed"] == true {
                println!("  repaired material metadata");
            }
            if let Some(compatible) = entry["report"]["compatible"].as_bool() {
                println!("  compatible: {compatible}");
            }
            if let Some(extensions) = entry["report"]["extensions"].as_array() {
                for extension in extensions {
                    println!(
                        "  extension {} required={} {}",
                        extension["name"].as_str().unwrap_or(""),
                        extension["required"].as_bool().unwrap_or(false),
                        extension["status"].as_str().unwrap_or("")
                    );
                }
            }
        }
    }
    if failed {
        bail!("glTF material lint failed; see the report above");
    }
    Ok(())
}
