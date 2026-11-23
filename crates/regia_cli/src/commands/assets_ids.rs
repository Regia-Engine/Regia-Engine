// SPDX-License-Identifier: MPL-2.0
//! `regia assets ids export | import`: the asset id manifest `.assetids.ron` (ADR-8460,
//! `book/user-guide/asset-ids.md`). Sources kept out of version control keep their ids across
//! checkouts: export records every sidecar's id, import writes the missing sidecars back, and
//! `regia cook --ids <file>` imports before it cooks, so a patch ships only what changed.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_asset::asset_ids::{
    AssetIdsError, AssetIdsManifest, DEFAULT_ASSET_IDS_FILE, ImportReport, export_asset_ids,
    import_asset_ids, write_asset_ids,
};
use serde_json::json;

/// `regia assets ids` actions.
#[derive(Debug, Subcommand)]
pub enum AssetsIdsCommand {
    /// Write the id of every source's `.meta` under `assets/` to a `.assetids.ron` manifest
    /// (path, id, importer; sorted, byte-stable).
    Export(AssetsIdsExportArgs),
    /// Write the `.meta` sidecars a manifest records that are missing; never replaces one that
    /// names another id (`asset_id_conflict`, nothing written).
    Import(AssetsIdsImportArgs),
}

/// `regia assets ids export` arguments.
#[derive(Debug, Args)]
pub struct AssetsIdsExportArgs {
    /// Project directory (contains `assets/` and `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// The manifest to write (default `<project>/asset_ids.assetids.ron`; `-` prints it on stdout).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Print a JSON summary on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia assets ids import` arguments.
#[derive(Debug, Args)]
pub struct AssetsIdsImportArgs {
    /// The `.assetids.ron` manifest to read.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
    /// Project directory (contains `assets/` and `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Report what would be written without writing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Print a JSON summary on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `anyhow` error carrying the stable code first (`asset_id_conflict: ...`), with each conflict
/// listed.
fn coded(error: &AssetIdsError) -> anyhow::Error {
    let mut text = format!("{}: {error}", error.code());
    if let AssetIdsError::Conflict(report) = error {
        for c in &report.conflicts {
            if let Some(other) = &c.shared_with {
                text.push_str(&format!(
                    "\n  {}: the manifest records {}, and {other} on disk has the same id (one id cooks to one file)",
                    c.path, c.recorded
                ));
                continue;
            }
            match (&c.existing, &c.reason) {
                (Some(existing), _) => text.push_str(&format!(
                    "\n  {}: the manifest records {} but its .meta names {existing}",
                    c.path, c.recorded
                )),
                (None, reason) => text.push_str(&format!(
                    "\n  {}: the manifest records {} but its .meta does not read ({})",
                    c.path,
                    c.recorded,
                    reason.as_deref().unwrap_or("unknown")
                )),
            }
        }
        text.push_str(
            "\n  nothing was written; fix or remove those sidecars or the replaced sources, or the manifest rows",
        );
    }
    anyhow::anyhow!(text)
}

/// The project's `assets/` folder.
fn assets_root(project: &Path) -> PathBuf {
    project.join("assets")
}

/// Runs an `assets ids` action and returns what to print.
///
/// # Errors
///
/// The manifest does not read or is invalid (`asset_ids_unreadable`, `asset_ids_invalid`), a
/// sidecar does not parse on export (`asset_ids_bad_meta`), a write fails
/// (`asset_ids_write_failed`), or an import conflicts (`asset_id_conflict`).
pub fn run(command: &AssetsIdsCommand) -> Result<String> {
    match command {
        AssetsIdsCommand::Export(args) => export(args),
        AssetsIdsCommand::Import(args) => import(args),
    }
}

/// `regia assets ids export`.
///
/// # Errors
///
/// See [`run`].
pub fn export(args: &AssetsIdsExportArgs) -> Result<String> {
    let manifest = export_asset_ids(&assets_root(&args.project)).map_err(|e| coded(&e))?;
    let to_stdout = args.out.as_deref().is_some_and(|p| p == Path::new("-"));
    if to_stdout {
        if args.json {
            bail!("--json needs a manifest file: drop `--out -` or `--json`");
        }
        return Ok(manifest.to_ron().trim_end().to_string());
    }
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.project.join(DEFAULT_ASSET_IDS_FILE));
    write_asset_ids(&out, &manifest).map_err(|e| coded(&e))?;
    if args.json {
        return Ok(serde_json::to_string_pretty(&json!({
            "file": out.display().to_string(),
            "entries": manifest.entries.len(),
        }))?);
    }
    Ok(format!(
        "wrote {} ids to {}",
        manifest.entries.len(),
        out.display()
    ))
}

/// Imports `file` into `project`'s `assets/` (the step `regia cook --ids` runs first).
///
/// # Errors
///
/// See [`run`].
pub fn import_file(project: &Path, file: &Path, dry_run: bool) -> Result<ImportReport> {
    let manifest = AssetIdsManifest::read(file).map_err(|e| coded(&e))?;
    import_asset_ids(&assets_root(project), &manifest, dry_run).map_err(|e| coded(&e))
}

/// One line summarizing an import.
#[must_use]
pub fn import_summary(report: &ImportReport) -> String {
    format!(
        "asset ids: {} {}, {} kept, {} without a source",
        report.written.len(),
        if report.applied {
            "written"
        } else {
            "to write (dry run)"
        },
        report.kept.len(),
        report.missing_source.len()
    )
}

/// `regia assets ids import`.
///
/// # Errors
///
/// See [`run`].
pub fn import(args: &AssetsIdsImportArgs) -> Result<String> {
    let report = import_file(&args.project, &args.file, args.dry_run)?;
    if args.json {
        return Ok(serde_json::to_string_pretty(&report)?);
    }
    let mut text = import_summary(&report);
    for path in &report.written {
        text.push_str(&format!("\n  write {path}.meta"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::cook::CookArgs;
    use regia_asset::pak_diff::{DEFAULT_CHUNK_SIZE, diff_sides};

    /// A project with two chunks: `base` (the steel material and a small PNG) and `extra` (the
    /// copper material), like a game whose textures are not in version control.
    fn fixture_project(label: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(label);
        let assets = root.join("assets");
        for (rel, name) in [
            ("materials/steel.material.ron", "Steel"),
            ("extra/copper.material.ron", "Copper"),
        ] {
            write_material(&assets.join(rel), name, [0.5, 0.5, 0.5, 1.0]);
        }
        let mut image = image::RgbaImage::new(8, 8);
        for (x, y, px) in image.enumerate_pixels_mut() {
            *px = image::Rgba([(x * 30) as u8, (y * 30) as u8, 90, 255]);
        }
        std::fs::create_dir_all(assets.join("textures")).expect("textures dir");
        image.save(assets.join("textures/rock.png")).expect("png");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"ids\"\nid = \"com.example.ids\"\n\n\
             [cook.chunks.extra]\ninclude = [\"extra/**\"]\ndelivery = \"on_demand\"\n",
        )
        .expect("Regia.toml");
        root
    }

    fn write_material(path: &Path, name: &str, base_color: [f32; 4]) {
        let mut material = regia_asset::MaterialAssetRon::default_standard();
        material.name = Some(name.to_string());
        material.base.base_color = base_color;
        let text = material.to_ron_string().expect("material ron");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("write material");
    }

    fn cook_args(project: &Path, ids: Option<PathBuf>) -> CookArgs {
        CookArgs {
            project: project.to_path_buf(),
            platform: Some("linux".into()),
            quality: "fast".into(),
            clean: true,
            stats: false,
            json: false,
            ship: false,
            gpu_meshes: false,
            pack_report: false,
            size_report: false,
            lightmap_tiers: false,
            shared_cache: None,
            read_only: false,
            no_shared_cache: true,
            ids,
            texture_tier: Vec::new(),
            action: None,
        }
    }

    /// Cooks with `--clean` (a fresh checkout has no cooked cache) and copies the paks to
    /// `<root>/builds/<name>`.
    fn cook_build(project: &Path, ids: Option<PathBuf>, name: &str) -> PathBuf {
        crate::commands::cook::run(&cook_args(project, ids)).expect("cook");
        let paks = project.join("build/paks/linux");
        let out = project.join("builds").join(name);
        std::fs::create_dir_all(&out).expect("build dir");
        for entry in std::fs::read_dir(&paks).expect("paks") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|e| e == "fpak") {
                std::fs::copy(&path, out.join(path.file_name().expect("name"))).expect("copy pak");
            }
        }
        out
    }

    /// Deletes every `.meta` under `assets/`: the binaries' sidecars are not in version control.
    fn drop_sidecars(project: &Path) -> usize {
        let mut n = 0;
        let mut stack = vec![project.join("assets")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "meta") {
                    std::fs::remove_file(&path).expect("rm meta");
                    n += 1;
                }
            }
        }
        n
    }

    fn copper_id(project: &Path) -> regia_asset::AssetId {
        let meta = regia_asset::meta_path_for(&project.join("assets/extra/copper.material.ron"));
        regia_asset::MetaSidecar::from_ron(&std::fs::read(meta).expect("copper meta"))
            .expect("meta")
            .id
    }

    /// C-01 acceptance: cook, lose the sidecars, import the ids, recook: the paks match. Then
    /// change one asset and recook: the pak diff against the first build shows that asset in its
    /// own pak and nothing else, so the patch ships one pak.
    #[test]
    fn recook_keeps_ids_so_patch_ships_one_pak() {
        let project = fixture_project("ids-recook");
        let first = cook_build(&project, None, "first");
        let copper = copper_id(&project);
        let manifest = project.join("ids.assetids.ron");
        export(&AssetsIdsExportArgs {
            project: project.clone(),
            out: Some(manifest.clone()),
            json: false,
        })
        .expect("export");
        assert_eq!(
            drop_sidecars(&project),
            3,
            "steel, copper and the PNG had sidecars"
        );

        let again = cook_build(&project, Some(manifest.clone()), "again");
        let diff = diff_sides(&first, &again, DEFAULT_CHUNK_SIZE).expect("diff");
        assert_eq!(
            (diff.totals.added, diff.totals.removed, diff.totals.changed),
            (0, 0, 0),
            "a recook after the sidecars were lost changed the paks: {}",
            serde_json::to_string_pretty(&diff).unwrap_or_default()
        );
        assert_eq!(copper_id(&project), copper);

        write_material(
            &project.join("assets/extra/copper.material.ron"),
            "Copper",
            [0.9, 0.4, 0.2, 1.0],
        );
        let patched = cook_build(&project, Some(manifest), "patched");
        let diff = diff_sides(&first, &patched, DEFAULT_CHUNK_SIZE).expect("diff");
        let report = serde_json::to_string_pretty(&diff).unwrap_or_default();
        assert!(diff.paks.len() >= 2, "two chunks, two paks: {report}");
        for pak in &diff.paks {
            let moved = pak.totals.added + pak.totals.removed + pak.totals.changed + pak.totals.moved;
            if pak.pak.contains("extra") {
                assert_eq!(pak.totals.changed, 1, "{report}");
                assert_eq!(moved, 1, "{report}");
                let changed = pak
                    .assets
                    .iter()
                    .find(|a| a.change == regia_asset::pak_diff::AssetChange::Changed)
                    .expect("the changed asset");
                assert_eq!(changed.id, copper, "{report}");
            } else {
                assert_eq!(moved, 0, "{} changed: {report}", pak.pak);
                assert_eq!(pak.estimate.download_bytes, 0, "{} ships: {report}", pak.pak);
            }
        }
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn cli_import_reports_conflicts_with_the_code() {
        let project = fixture_project("ids-cli-conflict");
        // Sidecars as a cook would write them.
        let assets = project.join("assets");
        for rel in [
            "materials/steel.material.ron",
            "extra/copper.material.ron",
            "textures/rock.png",
        ] {
            let source = assets.join(rel);
            let meta = regia_asset::MetaSidecar::new("x", regia_asset::AssetId::new_random());
            std::fs::write(
                regia_asset::meta_path_for(&source),
                meta.to_ron_string().expect("meta"),
            )
            .expect("meta");
        }
        let out = export(&AssetsIdsExportArgs {
            project: project.clone(),
            out: None,
            json: true,
        })
        .expect("export");
        let summary: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(summary["entries"], 3);
        let file = project.join(DEFAULT_ASSET_IDS_FILE);
        assert!(file.is_file());
        let stdout = export(&AssetsIdsExportArgs {
            project: project.clone(),
            out: Some("-".into()),
            json: false,
        })
        .expect("stdout");
        assert_eq!(
            format!("{stdout}\n"),
            std::fs::read_to_string(&file).expect("read")
        );

        // Replace one sidecar's id: the import refuses and writes nothing.
        let steel = regia_asset::meta_path_for(&assets.join("materials/steel.material.ron"));
        std::fs::write(
            &steel,
            regia_asset::MetaSidecar::new("material_ron", regia_asset::AssetId::new_random())
                .to_ron_string()
                .expect("meta"),
        )
        .expect("meta");
        let copper = regia_asset::meta_path_for(&assets.join("extra/copper.material.ron"));
        std::fs::remove_file(&copper).expect("rm");
        let err = import(&AssetsIdsImportArgs {
            file: file.clone(),
            project: project.clone(),
            dry_run: false,
            json: false,
        })
        .expect_err("conflict");
        let text = format!("{err:#}");
        assert!(text.starts_with("asset_id_conflict:"), "{text}");
        assert!(text.contains("materials/steel.material.ron"), "{text}");
        assert!(!copper.exists(), "an import with a conflict wrote a sidecar");
        // `regia cook --ids` stops on the same conflict before cooking.
        let cook = crate::commands::cook::run(&cook_args(&project, Some(file.clone())));
        let text = format!("{:#}", cook.expect_err("cook conflict"));
        assert!(text.contains("asset_id_conflict"), "{text}");
        assert!(!project.join("build/paks/linux").exists(), "the cook ran");
        // A manifest that does not read.
        let err = import(&AssetsIdsImportArgs {
            file: project.join("none.assetids.ron"),
            project: project.clone(),
            dry_run: true,
            json: true,
        })
        .expect_err("missing");
        assert!(format!("{err:#}").starts_with("asset_ids_unreadable:"));
        let _ = std::fs::remove_dir_all(&project);
    }
}
