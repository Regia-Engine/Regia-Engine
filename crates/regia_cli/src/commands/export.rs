// SPDX-License-Identifier: MPL-2.0
//! `regia export glb`: a level, a prefab or a list of entities as one `.glb` for a DCC tool
//! (`regia_asset_pipeline::scene_export`, ADR-2554).
//!
//! `--level` and `--prefab` name the scene file (`.level.ron`, `.level.d/` or `.prefab.ron`; they
//! are the same format). `--entities` limits the export to those entities (guids, comma
//! separated) and their descendants. `--out` is the `.glb`. Exit 0 on success; a warning line is
//! printed for everything left out. `--json` prints one summary object.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_asset_pipeline::Forward;
use regia_asset_pipeline::scene_export::{ExportOptions, export_file};
use regia_core::id::Guid;

/// `regia export` subcommands.
#[derive(Debug, Subcommand)]
pub enum ExportCommand {
    /// Write a level, a prefab or some entities as a `.glb` (glTF 2.0 binary).
    Glb(GlbArgs),
}

/// `regia export glb`.
#[derive(Debug, Args)]
pub struct GlbArgs {
    /// The level to export (`.level.ron` or `.level.d/`).
    #[arg(long, conflicts_with = "prefab", required_unless_present = "prefab")]
    pub level: Option<PathBuf>,
    /// The prefab to export (`.prefab.ron`).
    #[arg(long)]
    pub prefab: Option<PathBuf>,
    /// Only these entities (guids, comma separated) and their descendants.
    #[arg(long, value_delimiter = ',')]
    pub entities: Vec<String>,
    /// The `.glb` to write.
    #[arg(long)]
    pub out: PathBuf,
    /// The project root that `assets/...` paths resolve against (default: found from the scene
    /// file: the nearest `Regia.toml`, else the parent of the nearest `assets` directory).
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Keep Regia Engine's axes instead of converting to glTF's +Z forward (no root axis node).
    #[arg(long)]
    pub keep_axes: bool,
    /// Export skins of skinned source meshes.
    #[arg(long)]
    pub skins: bool,
    /// Export animation clips of the source meshes.
    #[arg(long)]
    pub animations: bool,
    /// One JSON object instead of text.
    #[arg(long)]
    pub json: bool,
}

/// Runs `regia export`.
pub fn run(command: &ExportCommand) -> Result<()> {
    match command {
        ExportCommand::Glb(args) => glb(args),
    }
}

fn glb(args: &GlbArgs) -> Result<()> {
    let scene = match (&args.level, &args.prefab) {
        (Some(path), None) | (None, Some(path)) => path.as_path(),
        _ => bail!("give exactly one of --level and --prefab"),
    };
    let entities = if args.entities.is_empty() {
        None
    } else {
        let mut guids = Vec::with_capacity(args.entities.len());
        for text in &args.entities {
            let guid: Guid = text
                .trim()
                .parse()
                .with_context(|| format!("`{text}` is not an entity guid"))?;
            guids.push(guid);
        }
        Some(guids)
    };
    let project = match &args.project {
        Some(p) => p.clone(),
        None => find_project(scene),
    };
    let options = ExportOptions {
        entities,
        forward: if args.keep_axes {
            Forward::Keep
        } else {
            Forward::Convert
        },
        skins: args.skins,
        animations: args.animations,
    };
    let report = export_file(scene, &project, &args.out, &options)
        .map_err(|e| anyhow::anyhow!("{}: {e}", e.code()))?;
    if args.json {
        let summary = serde_json::json!({
            "out": args.out.display().to_string(),
            "bytes": report.bytes,
            "nodes": report.nodes,
            "meshes": report.meshes,
            "triangles": report.triangles,
            "materials": report.materials,
            "images": report.images,
            "lights": report.lights,
            "cameras": report.cameras,
            "skins": report.skins,
            "animations": report.animations,
            "warnings": report.warnings,
        });
        println!("{summary}");
    } else {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        println!(
            "wrote {} ({} bytes): {} nodes, {} meshes, {} triangles, {} materials, {} images, {} lights, {} cameras",
            args.out.display(),
            report.bytes,
            report.nodes,
            report.meshes,
            report.triangles,
            report.materials,
            report.images,
            report.lights,
            report.cameras
        );
    }
    Ok(())
}

/// The project root of a scene file: the nearest `Regia.toml`, else the parent of the nearest
/// `assets` directory, else the file's directory.
fn find_project(scene: &Path) -> PathBuf {
    let start = std::path::absolute(scene).unwrap_or_else(|_| scene.to_path_buf());
    let dir = start.parent().map_or_else(|| start.clone(), Path::to_path_buf);
    if let Some(found) = dir.ancestors().find(|a| regia_core::brand::is_project_root(a)) {
        return found.to_path_buf();
    }
    if let Some(assets) = dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "assets"))
        && let Some(parent) = assets.parent()
    {
        return parent.to_path_buf();
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_is_found_from_a_level_under_assets() {
        let dir = regia_test_utils::unique_temp_dir("export_cli");
        let level = dir.join("assets/maps/a.level.ron");
        std::fs::create_dir_all(level.parent().expect("parent")).expect("dir");
        std::fs::write(&level, "x").expect("write");
        assert_eq!(find_project(&level), dir);
    }

    #[test]
    fn a_level_is_exported_from_the_command_line_arguments() {
        let dir = regia_test_utils::unique_temp_dir("export_cli_run");
        let level = dir.join("assets/maps/a.level.ron");
        std::fs::create_dir_all(level.parent().expect("parent")).expect("dir");
        std::fs::write(
            &level,
            r#"SceneFile(format_version: 1, entities: [
                (guid: "00000000-0000-0000-0000-000000000001", name: "Cube", components: {
                    "regia_render::runtime::MeshSource": (mesh: "builtin:cube", material: ""),
                }),
            ])"#,
        )
        .expect("write");
        let out = dir.join("out/a.glb");
        glb(&GlbArgs {
            level: Some(level),
            prefab: None,
            entities: Vec::new(),
            out: out.clone(),
            project: None,
            keep_axes: false,
            skins: false,
            animations: false,
            json: true,
        })
        .expect("export");
        let bytes = std::fs::read(out).expect("glb");
        assert_eq!(&bytes[0..4], b"glTF");
    }

    #[test]
    fn a_bad_entity_guid_is_reported() {
        let error = glb(&GlbArgs {
            level: Some(PathBuf::from("none.level.ron")),
            prefab: None,
            entities: vec!["nope".into()],
            out: PathBuf::from("x.glb"),
            project: None,
            keep_axes: false,
            skins: false,
            animations: false,
            json: false,
        })
        .expect_err("bad guid");
        assert!(error.to_string().contains("not an entity guid"));
    }
}
