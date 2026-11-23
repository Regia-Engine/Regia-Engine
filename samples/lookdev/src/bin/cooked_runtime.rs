// SPDX-License-Identifier: MIT-0
//! DO27's hero assets through the shipped-game path: `regia cook` a temporary project holding a
//! copy of one hero asset directory from `REGIA_DO27_CONTENT` (never the repo), mount its pak
//! with no source present, and render the cooked model beside the direct glTF path.
//!
//! `cooked_runtime --asset hero/buildings/silo [--asset ...] [--out DIR] [--size WxH]`
//!
//! Prints per asset: cook time, pak size, the cooked/direct frame difference, the material-texture
//! bytes (direct RGBA8 chains vs cooked block-compressed chains, and the device's allocator
//! report for both), and writes `<name>_{direct,cooked,diff_x8}.png` under `--out`.

use std::path::{Path, PathBuf};

use lookdev::LookdevError;
use lookdev::cooked_runtime::{self as cr, View};
use lookdev::hero::{self, HeroAsset};
use regia_core::glam::Vec3;
use regia_render::{LabMaterial, LabMesh, RenderDevice};

fn copy_dir(from: &Path, to: &Path) -> Result<Vec<PathBuf>, LookdevError> {
    let io = |p: &Path| {
        let p = p.display().to_string();
        move |source| LookdevError::Io { path: p, source }
    };
    std::fs::create_dir_all(to).map_err(io(to))?;
    let mut gltfs = Vec::new();
    for entry in std::fs::read_dir(from).map_err(io(from))?.flatten() {
        let path = entry.path();
        if path.is_file() {
            let dest = to.join(entry.file_name());
            std::fs::copy(&path, &dest).map_err(io(&path))?;
            if path.extension().is_some_and(|e| e == "gltf") {
                gltfs.push(dest);
            }
        }
    }
    gltfs.sort();
    Ok(gltfs)
}

/// The LOD 0 glTFs (`*_LOD1..3` files are skipped, like the harness).
fn lod0(files: Vec<PathBuf>) -> Vec<PathBuf> {
    files
        .into_iter()
        .filter(|f| {
            let n = f
                .file_stem()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            !(n.contains("_lod1") || n.contains("_lod2") || n.contains("_lod3"))
        })
        .collect()
}

/// One frame on a fresh device, so its allocator report holds this frame's textures only.
fn render_fresh(
    meshes: &[LabMesh],
    materials: Vec<LabMaterial>,
    view: View,
    size: (u32, u32),
) -> Result<cr::Frame, LookdevError> {
    let device = RenderDevice::headless()?;
    cr::render_model(&device, meshes, materials, view, size.0, size.1)
}

fn run_asset(content: &Path, rel: &str, out: &Path, size: (u32, u32)) -> Result<(), LookdevError> {
    let name = rel
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .to_string();
    let project = std::env::temp_dir().join(format!("regia-cooked-runtime-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).map_err(|source| LookdevError::Io {
        path: project.display().to_string(),
        source,
    })?;
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"do27-hero\"\nid = \"do27-hero\"\nversion = \"0.1.0\"\nengine = \"0.1\"\n",
    )
    .map_err(|source| LookdevError::Io {
        path: project.display().to_string(),
        source,
    })?;
    let files = lod0(copy_dir(
        &content.join(rel),
        &project.join("assets/hero").join(&name),
    )?);
    if files.is_empty() {
        return Err(LookdevError::Config(format!("{rel}: no glTF")));
    }
    let cli = cr::regia_cli();
    let run = cr::cook(&cli, &project, "linux")?;
    println!(
        "{name}: cook {:.1} s, pak {:.1} MiB",
        run.elapsed.as_secs_f64(),
        run.pak_bytes as f64 / 1048576.0
    );
    let ship = project.join("ship");
    let shipped = ship.join("content");
    std::fs::create_dir_all(&shipped).map_err(|source| LookdevError::Io {
        path: shipped.display().to_string(),
        source,
    })?;
    for pak in cr::pak_files(&run.pak_dir)? {
        if let Some(file) = pak.file_name() {
            std::fs::copy(&pak, shipped.join(file)).map_err(|source| LookdevError::Io {
                path: pak.display().to_string(),
                source,
            })?;
        }
    }
    let started = std::time::Instant::now();
    let mut server = cr::mount_paks(&shipped, &ship.join("runtime"), "linux")?;
    let mut meshes = Vec::new();
    let mut materials = Vec::new();
    let mut texture_bytes = 0;
    for file in &files {
        let rel_path = format!(
            "assets/hero/{name}/{}",
            file.file_name().map(|f| f.to_string_lossy()).unwrap_or_default()
        );
        let id = server
            .id_for_path(&rel_path)
            .ok_or_else(|| LookdevError::Config(format!("{rel_path} not in the pak manifest")))?;
        let draws = regia_render::cooked_model::load_cooked_model(&mut server, id)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        let base = materials.len();
        meshes.extend(draws.meshes.into_iter().map(|mut m| {
            m.material += base;
            m
        }));
        materials.extend(draws.materials);
        texture_bytes += draws.texture_bytes;
    }
    println!("{name}: pak load {:.0} ms", started.elapsed().as_secs_f64() * 1e3);
    cr::to_harness_frame(&mut meshes);
    let direct = hero::load(
        &HeroAsset {
            name: name.clone(),
            files: files.clone(),
            planet: lookdev::Planet::Mars,
        },
        8192,
    )?;
    let (lo, hi) = cr::bounds(&direct.meshes);
    let view = View::of_bounds(lo, hi, Vec3::new(0.6, 0.35, 0.75), 0.9);
    let a = render_fresh(&direct.meshes, direct.materials.clone(), view, size)?;
    let b = render_fresh(&meshes, materials, view, size)?;
    let (vram_direct, vram_cooked) = (a.texture_vram, b.texture_vram);
    let stats = cr::diff(&a.pixels, &b.pixels);
    println!("{name}: cooked vs direct {stats:?}");
    println!(
        "{name}: material textures direct RGBA8 {:.1} MiB (device {}), cooked {:.1} MiB (device {})",
        cr::rgba8_material_bytes(&direct.materials) as f64 / 1048576.0,
        vram_direct.map_or("n/a".into(), |b| format!("{:.1} MiB", b as f64 / 1048576.0)),
        texture_bytes as f64 / 1048576.0,
        vram_cooked.map_or("n/a".into(), |b| format!("{:.1} MiB", b as f64 / 1048576.0)),
    );
    cr::save(&a, &out.join(format!("{name}_direct.png")))?;
    cr::save(&b, &out.join(format!("{name}_cooked.png")))?;
    cr::save(
        &cr::diff_image(&a, &b, 8),
        &out.join(format!("{name}_diff_x8.png")),
    )?;
    let _ = std::fs::remove_dir_all(&project);
    Ok(())
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let Some(content) = lookdev::do27::content_dir() else {
        eprintln!("REGIA_DO27_CONTENT is not set");
        return std::process::ExitCode::FAILURE;
    };
    let out = PathBuf::from(value("--out").unwrap_or_else(|| "cooked-runtime-out".into()));
    let size = value("--size")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((960, 540));
    let assets: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && args[i - 1] == "--asset")
        .map(|(_, a)| a)
        .collect();
    let mut failed = false;
    for rel in assets {
        if let Err(e) = run_asset(&content, rel, &out, size) {
            eprintln!("{rel}: {e}");
            failed = true;
        }
    }
    if failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
