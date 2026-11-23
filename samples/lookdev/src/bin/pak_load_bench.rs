// SPDX-License-Identifier: MIT-0
//! A shipped-game map load out of real paks, timed: DO27 hero assets go through `regia cook`
//! once (`cook`), then every run mounts those paks with no source present and loads every model
//! through [`regia_asset::AssetServer`] and [`regia_render::cooked_model`], the path a
//! packaged game takes (`load`).
//!
//! ```text
//! pak_load_bench cook --asset hero/buildings/foundry [--asset ...] --out DIR   # REGIA_DO27_CONTENT
//! pak_load_bench load --paks DIR [--frame 1280x720 [--png OUT.png]]
//! pak_load_bench stats --paks DIR
//! pak_load_bench verify --paks DIR               # record the paks as verified for this binary
//! ```
//!
//! A pak cooked before the GPU-ready mesh layout converts with `pak_migrate` (in
//! `regia_asset_pipeline`; the textures stay byte-identical, so no re-cook). `verify` hashes the paks now and writes the `pak_verify`
//! record, so later `load` runs of this binary see verified paks (zero-copy from the start);
//! without it a `load` is a first launch after an update (its paks serve copies). Point
//! `XDG_CACHE_HOME` at an empty directory to force a first launch.
//!
//! `cook` writes `DIR/content/*.fpak` and `DIR/models.txt` (the model paths, one per line);
//! `stats` (also run by `cook`) prints how many texture and other entries are stored raw and how
//! many lz4-compressed, and what lz4 saved.
//!
//! `load` prints `PAK_LOAD {json}`:
//! - `device_ms`: headless device creation, before the load (a running game has its device);
//! - `mount_ms`, `cpu_load_ms`: mounting the paks, then loading every model (meshes, materials,
//!   cooked textures) in one `load_cooked_models` batch (meshes and textures decode on the
//!   compute pool);
//! - `first_frame_ms`: the first frame that draws the models (texture and mesh uploads included),
//!   with `--frame`;
//! - `resident_ms`: mount + load + first frame;
//! - `hwm_mib` (peak RSS so far), `rss_anon_mib`, `rss_file_mib` after the load and after the
//!   frame. Anonymous pages are heap; file pages are the mapped paks (reclaimable page cache);
//! - `zero_copy_paks`: mounted paks serving views of their mapping (verified) after the load.

use std::path::{Path, PathBuf};
use std::time::Instant;

use lookdev::LookdevError;
use lookdev::cooked_runtime::{self as cr, View};
use regia_core::glam::Vec3;
use regia_render::RenderDevice;

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// `VmHWM`, `RssAnon`, `RssFile` from `/proc/self/status`, MiB (zero where unavailable).
fn memory() -> (f64, f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
            .map_or(0.0, |kib| kib / 1024.0)
    };
    (field("VmHWM:"), field("RssAnon:"), field("RssFile:"))
}

/// Copies the tree `from` to `to`, collecting the LOD 0 glTFs as `prefix/<relative path>`
/// (`*_LOD1..3` files are skipped, like the harness).
fn copy_tree(from: &Path, to: &Path, prefix: &str, models: &mut Vec<String>) -> Result<(), LookdevError> {
    std::fs::create_dir_all(to).map_err(io(to))?;
    for entry in std::fs::read_dir(from).map_err(io(from))?.flatten() {
        let path = entry.path();
        let file = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            copy_tree(&path, &to.join(&file), &format!("{prefix}/{file}"), models)?;
            continue;
        }
        std::fs::copy(&path, to.join(&file)).map_err(io(&path))?;
        let lower = file.to_ascii_lowercase();
        let lod = lower.contains("_lod1") || lower.contains("_lod2") || lower.contains("_lod3");
        if lower.ends_with(".gltf") && !lod {
            models.push(format!("{prefix}/{file}"));
        }
    }
    Ok(())
}

fn cook(content: &Path, assets: &[String], out: &Path) -> Result<(), LookdevError> {
    let project = out.join("project");
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).map_err(io(&project))?;
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"do27-map\"\nid = \"do27-map\"\nversion = \"0.1.0\"\nengine = \"0.1\"\n",
    )
    .map_err(io(&project))?;
    let mut models = Vec::new();
    for rel in assets {
        let name = rel.trim_end_matches('/').rsplit('/').next().unwrap_or(rel);
        copy_tree(
            &content.join(rel),
            &project.join("assets/hero").join(name),
            &format!("assets/hero/{name}"),
            &mut models,
        )?;
    }
    models.sort();
    let run = cr::cook(&cr::regia_cli(), &project, "linux")?;
    let shipped = out.join("content");
    let _ = std::fs::remove_dir_all(&shipped);
    std::fs::create_dir_all(&shipped).map_err(io(&shipped))?;
    for pak in cr::pak_files(&run.pak_dir)? {
        let Some(file) = pak.file_name() else { continue };
        std::fs::copy(&pak, shipped.join(file)).map_err(io(&pak))?;
    }
    std::fs::write(out.join("models.txt"), models.join("\n")).map_err(io(out))?;
    let _ = std::fs::remove_dir_all(&project);
    println!(
        "cook {:.1} s, paks {:.1} MiB, {} models",
        run.elapsed.as_secs_f64(),
        run.pak_bytes as f64 / 1048576.0,
        models.len(),
    );
    stats(out)
}

/// Prints how the pak entries are stored: textures, meshes (compact or GPU-ready) and everything
/// else, raw or lz4, and how much lz4 saved on the compressed ones.
fn stats(dir: &Path) -> Result<(), LookdevError> {
    let texture = regia_asset::AssetTypeTag::texture().0;
    let compact = regia_asset::AssetTypeTag::mesh().0;
    let gpu = regia_asset::AssetTypeTag::gpu_mesh().0;
    // [textures, compact meshes, GPU-ready meshes, other] x [raw, lz4]: (count, size, stored)
    let mut rows = [[(0u64, 0u64, 0u64); 2]; 4];
    for pak in cr::pak_files(&dir.join("content"))? {
        let reader = regia_asset::PakReader::open(&pak)
            .map_err(|e| LookdevError::Config(format!("{}: {e}", pak.display())))?;
        for entry in &reader.toc.entries {
            let kind = match entry.type_tag {
                t if t == texture => 0,
                t if t == compact => 1,
                t if t == gpu => 2,
                _ => 3,
            };
            let packed = usize::from(entry.block_table_offset != 0);
            let row = &mut rows[kind][packed];
            row.0 += 1;
            row.1 += entry.size;
            row.2 += entry.compressed_size;
        }
    }
    let mib = |b: u64| b as f64 / 1048576.0;
    for (kind, name) in [(0, "textures"), (1, "meshes"), (2, "gpu_meshes"), (3, "other")] {
        let [raw, lz4] = rows[kind];
        println!(
            "PAK_STATS {name}: raw {} entries {:.1} MiB; lz4 {} entries {:.1} MiB -> {:.1} MiB stored ({:.1} % saved)",
            raw.0,
            mib(raw.1),
            lz4.0,
            mib(lz4.1),
            mib(lz4.2),
            if lz4.1 > 0 {
                100.0 * (1.0 - lz4.2 as f64 / lz4.1 as f64)
            } else {
                0.0
            },
        );
    }
    Ok(())
}

fn load(paks: &Path, frame: Option<(u32, u32)>, png: Option<&Path>) -> Result<(), LookdevError> {
    let list = paks.join("models.txt");
    let models: Vec<String> = std::fs::read_to_string(&list)
        .map_err(io(&list))?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let device_started = Instant::now();
    let device = match frame {
        Some(_) => Some(RenderDevice::headless()?),
        None => None,
    };
    let device_ms = device_started.elapsed().as_secs_f64() * 1e3;

    let started = Instant::now();
    let empty = std::env::temp_dir().join(format!("regia-pak-load-bench-{}", std::process::id()));
    let mut server = cr::mount_paks(&paks.join("content"), &empty, "linux")?;
    let mount_ms = started.elapsed().as_secs_f64() * 1e3;
    let mut meshes = Vec::new();
    let mut materials = Vec::new();
    let mut texture_bytes = 0u64;
    let ids = models
        .iter()
        .map(|rel| {
            server
                .id_for_path(rel)
                .ok_or_else(|| LookdevError::Config(format!("{rel} not in the pak manifest")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // One batch: every model's meshes and textures decode together on the pool (ADR-530).
    let batch = regia_render::cooked_model::load_cooked_models(&mut server, &ids)
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    for draws in batch {
        let base = materials.len();
        meshes.extend(draws.meshes.into_iter().map(|mut m| {
            m.material += base;
            m
        }));
        materials.extend(draws.materials);
        texture_bytes += draws.texture_bytes;
    }
    let cpu_load_ms = started.elapsed().as_secs_f64() * 1e3 - mount_ms;
    let zero_copy_paks = server.zero_copy_pak_count();
    let (load_hwm, load_anon, load_file) = memory();

    let mesh_count = meshes.len();
    let mut first_frame_ms = 0.0;
    if let (Some(device), Some((w, h))) = (device.as_ref(), frame) {
        cr::to_harness_frame(&mut meshes);
        let (lo, hi) = cr::bounds(&meshes);
        let view = View::of_bounds(lo, hi, Vec3::new(0.6, 0.35, 0.75), 0.9);
        let frame_started = Instant::now();
        // By value: the frame holds the only CPU copy of the vertices (the peak RSS is the load's).
        let pixels = cr::render_model_owned(device, std::mem::take(&mut meshes), materials, view, w, h)?;
        first_frame_ms = frame_started.elapsed().as_secs_f64() * 1e3;
        // The first frame as a PNG: two runs that must draw the same pixels (compact vs GPU-ready
        // pak, `REGIA_UPLOAD_RING=0` vs on) compare byte for byte.
        if let Some(path) = png {
            cr::save(&pixels, path)?;
        }
    } else {
        drop(materials);
    }
    let resident_ms = started.elapsed().as_secs_f64() * 1e3;
    let (hwm, anon, file) = memory();
    let _ = std::fs::remove_dir_all(&empty);
    println!(
        "PAK_LOAD {{\"models\":{},\"meshes\":{},\"texture_mib\":{:.1},\"device_ms\":{device_ms:.1},\
         \"mount_ms\":{mount_ms:.1},\"cpu_load_ms\":{cpu_load_ms:.1},\"first_frame_ms\":{first_frame_ms:.1},\
         \"resident_ms\":{resident_ms:.1},\"zero_copy_paks\":{zero_copy_paks},\"load_hwm_mib\":{load_hwm:.0},\"load_rss_anon_mib\":{load_anon:.0},\
         \"load_rss_file_mib\":{load_file:.0},\"hwm_mib\":{hwm:.0},\"rss_anon_mib\":{anon:.0},\"rss_file_mib\":{file:.0}}}",
        models.len(),
        mesh_count,
        texture_bytes as f64 / 1048576.0,
    );
    Ok(())
}

/// Hashes every pak now and records it as verified for this binary (the default record).
fn verify(paks: &Path) -> Result<(), LookdevError> {
    let options = regia_asset::PakVerifyOptions {
        start_delay: std::time::Duration::ZERO,
        bytes_per_sec: 0,
        force: true,
        ..regia_asset::PakVerifyOptions::default()
    };
    let files = cr::pak_files(&paks.join("content"))?;
    let handle = regia_asset::spawn_pak_verify(files, options)
        .ok_or_else(|| LookdevError::Config("pak verify did not start".into()))?;
    let (results, done) = handle.wait(std::time::Duration::from_secs(600));
    for (path, outcome) in &results {
        println!("PAK_VERIFY {} {outcome:?}", path.display());
    }
    if done {
        Ok(())
    } else {
        Err(LookdevError::Config("pak verify timed out".into()))
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let result = match args.get(1).map(String::as_str) {
        Some("cook") => {
            let assets: Vec<String> = args
                .iter()
                .enumerate()
                .filter(|(i, _)| *i > 0 && args[i - 1] == "--asset")
                .map(|(_, a)| a.clone())
                .collect();
            match (lookdev::do27::content_dir(), value("--out")) {
                (Some(content), Some(out)) => cook(&content, &assets, &PathBuf::from(out)),
                _ => Err(LookdevError::Config(
                    "cook needs REGIA_DO27_CONTENT and --out".into(),
                )),
            }
        }
        Some("verify") => match value("--paks") {
            Some(paks) => verify(&PathBuf::from(paks)),
            None => Err(LookdevError::Config("verify needs --paks".into())),
        },
        Some("stats") => match value("--paks") {
            Some(paks) => stats(&PathBuf::from(paks)),
            None => Err(LookdevError::Config("stats needs --paks".into())),
        },
        Some("load") => match value("--paks") {
            Some(paks) => {
                let frame = value("--frame").and_then(|s| {
                    let (w, h) = s.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                });
                let png = value("--png").map(PathBuf::from);
                load(&PathBuf::from(paks), frame, png.as_deref())
            }
            None => Err(LookdevError::Config("load needs --paks".into())),
        },
        _ => Err(LookdevError::Config(
            "usage: pak_load_bench cook --asset REL... --out DIR | load --paks DIR [--frame WxH [--png OUT]] | stats --paks DIR \
             | verify --paks DIR"
                .into(),
        )),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pak_load_bench: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
