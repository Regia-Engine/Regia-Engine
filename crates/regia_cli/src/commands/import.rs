// SPDX-License-Identifier: MPL-2.0
//! `regia import`: inspect an FBX, OBJ or USD file, or convert it to a glTF binary, with the desktop
//! pipeline's FBX importer (ADR-1602), OBJ reader or USD importer (ADR-3235;
//! `book/agents/fbx-import.md`).
//!
//! The command prints what the importer found (nodes, meshes, vertices, skins, morph targets,
//! materials, textures and where each was found, clips, the file's unit and up axis, warnings;
//! `--json` for tools). `--out` writes the converted `.glb`, the exact input the cook feeds to the
//! glTF importer, so a conversion can be checked in any glTF viewer. `regia cook` cooks `.fbx`
//! files in a project without this command.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use clap::Args;
use regia_asset_pipeline::fbx_import::{
    FbxImportReport, FbxImportSettings, FbxTextureStatus, convert_fbx_to_glb, read_fbx_file,
};
use regia_asset_pipeline::obj_import::{
    ObjImportReport, ObjImportSettings, convert_obj_to_glb, read_obj_file,
};
use regia_asset_pipeline::usd_import::{
    UsdImportReport, UsdImportSettings, UsdSource, convert_usd_to_glb, is_usd_extension,
};

/// `regia import` arguments.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The `.fbx` file (binary or ASCII), `.obj` file, or `.usda`, `.usdc`, `.usd` or `.usdz` file.
    pub source: PathBuf,
    /// Write the converted glTF binary here (`.glb`).
    #[arg(long, value_name = "GLB")]
    pub out: Option<PathBuf>,
    /// Uniform scale after the unit conversion to meters (`0.01` for centimeter data saved as
    /// meters).
    #[arg(long, default_value_t = 1.0)]
    pub scale: f32,
    /// Skip animation stacks.
    #[arg(long, default_value_t = false)]
    pub no_animation: bool,
    /// Samples per second for non-linear animation curves.
    #[arg(long, default_value_t = 30.0)]
    pub sample_rate: f32,
    /// The UV set that becomes UV0 (the set the cooked mesh keeps).
    #[arg(long)]
    pub uv_set: Option<String>,
    /// Print the report as JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl ImportArgs {
    fn settings(&self) -> FbxImportSettings {
        FbxImportSettings {
            scale: self.scale,
            import_animation: !self.no_animation,
            sample_rate: self.sample_rate,
            uv_set: self.uv_set.clone(),
            ..FbxImportSettings::default()
        }
    }
}

/// Converts the file and returns the report (and writes `--out`).
///
/// # Errors
/// An unreadable or malformed file, a limit exceeded, or `--out` not writable; the message ends
/// with the importer's error code (`fbx_parse`, `fbx_limit` …).
pub fn convert(args: &ImportArgs) -> Result<FbxImportReport> {
    let settings = args.settings();
    let bytes = read_fbx_file(&args.source, &settings.limits).map_err(|e| anyhow!("{e} ({})", e.code()))?;
    let conversion = convert_fbx_to_glb(&bytes, args.source.parent(), &settings)
        .map_err(|e| anyhow!("{}: {e} ({})", args.source.display(), e.code()))?;
    if let Some(out) = &args.out {
        write_atomic(out, &conversion.glb)?;
    }
    Ok(conversion.report)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| anyhow!("creating {}: {e}", parent.display()))?;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "glb.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // Claim a new sibling exclusively. Existing files and symlinks are never followed or removed.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| anyhow!("creating {}: {e}", tmp.display()))?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file); // Windows cannot rename the open staging file.
    let result = written.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        // Only our successfully claimed staging file is cleaned up.
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| anyhow!("writing {}: {e}", path.display()))
}

/// The human-readable report.
pub fn summary(report: &FbxImportReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "FBX {} ({}), from {:?}: unit {} m, up {}, scale {}",
        report.version,
        if report.ascii { "ASCII" } else { "binary" },
        report.creator,
        report.source_unit_meters,
        report.source_up_axis,
        report.scale
    );
    let _ = writeln!(
        out,
        "{} nodes, {} meshes, {} vertices, {} triangles, {} skins ({} joints), {} morph targets, {} materials",
        report.nodes,
        report.meshes,
        report.vertices,
        report.triangles,
        report.skins,
        report.joints,
        report.morph_targets,
        report.materials
    );
    if !report.uv_sets.is_empty() {
        let _ = writeln!(
            out,
            "UV sets: {} (the first is cooked)",
            report.uv_sets.join(", ")
        );
    }
    for texture in &report.textures {
        let status = match texture.status {
            FbxTextureStatus::Embedded => "embedded",
            FbxTextureStatus::External => "found",
            FbxTextureStatus::Missing => "MISSING",
            FbxTextureStatus::Undecodable => "UNDECODABLE",
        };
        let _ = writeln!(
            out,
            "texture {} of {:?}: {} ({status})",
            texture.role, texture.material, texture.file
        );
    }
    for clip in &report.clips {
        let _ = writeln!(
            out,
            "clip {:?}: {:.3} s, {} channels",
            clip.name, clip.duration_seconds, clip.channels
        );
    }
    for warning in &report.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

/// Converts an `.obj` file and returns its report (and writes `--out`). `--scale` applies; the
/// animation and UV-set options do not.
///
/// # Errors
/// An unreadable or malformed file, a limit exceeded, or `--out` not writable; the message ends
/// with the importer's error code (`obj_parse`, `obj_limit` …).
pub fn convert_obj(args: &ImportArgs) -> Result<ObjImportReport> {
    let settings = ObjImportSettings {
        scale: args.scale,
        ..ObjImportSettings::default()
    };
    let bytes = read_obj_file(&args.source, &settings.limits).map_err(|e| anyhow!("{e} ({})", e.code()))?;
    let conversion = convert_obj_to_glb(&bytes, args.source.parent(), &settings)
        .map_err(|e| anyhow!("{}: {e} ({})", args.source.display(), e.code()))?;
    if let Some(out) = &args.out {
        write_atomic(out, &conversion.glb)?;
    }
    Ok(conversion.report)
}

/// The human-readable OBJ report.
pub fn summary_obj(report: &ObjImportReport) -> String {
    let mut out = format!(
        "OBJ: {} objects, {} vertices, {} triangles, {} materials\n",
        report.objects, report.vertices, report.triangles, report.materials
    );
    for texture in &report.textures {
        let _ = writeln!(out, "texture {texture} (found)");
    }
    for warning in &report.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

/// Converts a `.usda`, `.usdc`, `.usd` or `.usdz` file and returns its report (and writes `--out`).
/// `--scale` applies; the animation and UV-set options do not (ADR-3235).
///
/// # Errors
/// An unreadable or malformed file, a reference cycle or over-deep chain, a limit exceeded, or
/// `--out` not writable; the message ends with the importer's error code (`usd_parse` …).
pub fn convert_usd(args: &ImportArgs) -> Result<UsdImportReport> {
    let settings = UsdImportSettings {
        scale: args.scale,
        ..UsdImportSettings::default()
    };
    let conversion = convert_usd_to_glb(UsdSource::File(&args.source), &settings)
        .map_err(|e| anyhow!("{}: {e} ({})", args.source.display(), e.code()))?;
    if let Some(out) = &args.out {
        write_atomic(out, &conversion.glb)?;
    }
    Ok(conversion.report)
}

/// The human-readable USD report.
pub fn summary_usd(report: &UsdImportReport) -> String {
    let mut out = format!(
        "USD ({}): {} layers, unit {} m, up {}, scale {}\n",
        report.format,
        report.layers.len(),
        report.meters_per_unit,
        report.up_axis,
        report.scale
    );
    let _ = writeln!(
        out,
        "{} prims, {} nodes, {} meshes ({} instanced), {} vertices, {} triangles, {} materials, {} cameras, {} lights",
        report.prims,
        report.nodes,
        report.meshes,
        report.instanced_meshes,
        report.vertices,
        report.triangles,
        report.materials,
        report.cameras,
        report.lights
    );
    for layer in &report.layers {
        let _ = writeln!(out, "layer {layer}");
    }
    for selection in &report.variant_selections {
        let _ = writeln!(out, "variant {selection}");
    }
    for texture in &report.textures {
        let _ = writeln!(out, "texture {texture} (embedded)");
    }
    for warning in &report.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

fn is_usd(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_usd_extension)
}

fn is_obj(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("obj"))
}

/// Runs `regia import`.
///
/// # Errors
/// As [`convert`] (or [`convert_obj`] for an `.obj` file, [`convert_usd`] for a USD file).
pub fn run(args: &ImportArgs) -> Result<()> {
    if is_usd(&args.source) {
        let report = convert_usd(args)?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", summary_usd(&report));
            if let Some(out) = &args.out {
                println!("wrote {}", out.display());
            }
        }
        return Ok(());
    }
    if is_obj(&args.source) {
        let report = convert_obj(args)?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", summary_obj(&report));
            if let Some(out) = &args.out {
                println!("wrote {}", out.display());
            }
        }
        return Ok(());
    }
    let report = convert(args)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", summary(&report));
        if let Some(out) = &args.out {
            println!("wrote {}", out.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset_pipeline::fbx_import::TRIANGLE_FBX;

    fn args(source: PathBuf, out: Option<PathBuf>) -> ImportArgs {
        ImportArgs {
            source,
            out,
            scale: 1.0,
            no_animation: false,
            sample_rate: 30.0,
            uv_set: None,
            json: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn audit_import_never_follows_the_legacy_staging_symlink() {
        let dir = regia_test_utils::unique_temp_dir("cli-import-staging-link");
        let out = dir.join("hero.glb");
        let secret = dir.join("user-file.txt");
        std::fs::write(&secret, b"user data").expect("user file");
        std::os::unix::fs::symlink(&secret, out.with_extension("glb.tmp")).expect("planted link");
        write_atomic(&out, b"glTF result").expect("publish import");
        assert_eq!(std::fs::read(&secret).expect("user file"), b"user data");
        assert!(
            !out.symlink_metadata()
                .expect("output metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&out).expect("output"), b"glTF result");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn import_reports_and_writes_the_converted_glb() {
        let dir = regia_test_utils::unique_temp_dir("cli-import");
        let source = dir.join("triangle.fbx");
        std::fs::write(&source, TRIANGLE_FBX).expect("write");
        let out = dir.join("out/triangle.glb");
        let report = convert(&args(source.clone(), Some(out.clone()))).expect("convert");
        assert_eq!(report.meshes, 1);
        assert_eq!(report.triangles, 1);
        let glb = std::fs::read(&out).expect("glb written");
        assert_eq!(&glb[0..4], b"glTF");
        assert!(!dir.join("out/triangle.glb.tmp").exists());
        let text = summary(&report);
        assert!(text.contains("1 meshes, 3 vertices, 1 triangles"), "{text}");
        let json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["triangles"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_converts_obj_files_too() {
        let dir = regia_test_utils::unique_temp_dir("cli-import-obj");
        let source = dir.join("quad.obj");
        std::fs::write(&source, "o Quad\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n").expect("write");
        assert!(is_obj(&source));
        let out = dir.join("quad.glb");
        let report = convert_obj(&args(source, Some(out.clone()))).expect("convert");
        assert_eq!(report.triangles, 2);
        assert_eq!(&std::fs::read(&out).expect("glb")[0..4], b"glTF");
        assert!(summary_obj(&report).contains("1 objects, 4 vertices, 2 triangles"));
        let bad = dir.join("bad.obj");
        std::fs::write(&bad, "f 1 2 3\n").expect("write");
        let err = convert_obj(&args(bad, None)).expect_err("must fail");
        assert!(err.to_string().contains("(obj_parse)"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_errors_carry_the_importer_code() {
        let dir = regia_test_utils::unique_temp_dir("cli-import-bad");
        let source = dir.join("bad.fbx");
        std::fs::write(&source, "not an fbx file").expect("write");
        let err = convert(&args(source, None)).expect_err("must fail");
        assert!(err.to_string().contains("(fbx_not_fbx)"), "{err}");
        let err = convert(&args(dir.join("absent.fbx"), None)).expect_err("must fail");
        assert!(err.to_string().contains("(fbx_io)"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_converts_usd_files_too() {
        let dir = regia_test_utils::unique_temp_dir("cli-import-usd");
        let source = dir.join("tri.usda");
        std::fs::write(
            &source,
            "#usda 1.0\n(\n    metersPerUnit = 1\n)\ndef Mesh \"Tri\"\n{\n    int[] faceVertexCounts = [3]\n    \
             int[] faceVertexIndices = [0, 1, 2]\n    point3f[] points = [(0, 0, 0), (1, 0, 0), (0, 1, 0)]\n}\n",
        )
        .expect("write");
        assert!(is_usd(&source));
        let out = dir.join("tri.glb");
        let report = convert_usd(&args(source, Some(out.clone()))).expect("convert");
        assert_eq!(report.triangles, 1);
        assert_eq!(&std::fs::read(&out).expect("glb")[0..4], b"glTF");
        assert!(summary_usd(&report).contains("1 meshes (0 instanced), 3 vertices, 1 triangles"));
        let bad = dir.join("bad.usda");
        std::fs::write(&bad, "#usda 1.0\ndef Xform \"A\" {}\n").expect("write");
        let error = convert_usd(&args(bad, None)).expect_err("no mesh");
        assert!(error.to_string().ends_with("(usd_no_mesh)"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
