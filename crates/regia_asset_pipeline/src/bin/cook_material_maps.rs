// SPDX-License-Identifier: MPL-2.0
//! `cook_material_maps`: cooks the material maps a glTF names into BC7 KTX2 files for the
//! renderer's pre-compressed slots (`regia_render::CompressedMaps`).
//!
//! ```text
//! cook_material_maps <file.gltf> <out dir> [--max 4096]
//! ```
//!
//! Base colour and emissive cook as BC7 sRGB; normal, occlusion and metallic-roughness as
//! BC7 linear RGB (the forward shader reads normals as `xyz`, so no BC5 variant is needed).
//! Each image becomes `<out dir>/<image stem>.ktx2`, its full mip chain included; `--max`
//! box-downsamples larger images first. Images are cooked on all cores.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::{TextureCookQuality, cook_texture_rgba8};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(n) => {
            println!("cooked {n} maps");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("cook_material_maps: {err}");
            ExitCode::from(1)
        }
    }
}

/// `(image path, usage)` for every image a material slot names (first usage wins).
fn wanted(gltf: &Path) -> anyhow::Result<Vec<(PathBuf, TextureCookUsage)>> {
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(gltf)?)?;
    let base = gltf.parent().unwrap_or(Path::new("."));
    let empty = Vec::new();
    let images = doc["images"].as_array().unwrap_or(&empty);
    let textures = doc["textures"].as_array().unwrap_or(&empty);
    let mut out: Vec<(PathBuf, TextureCookUsage)> = Vec::new();
    for mat in doc["materials"].as_array().unwrap_or(&empty) {
        let pbr = &mat["pbrMetallicRoughness"];
        for (info, usage) in [
            (&pbr["baseColorTexture"], TextureCookUsage::ColorSrgb),
            (&mat["emissiveTexture"], TextureCookUsage::ColorSrgb),
            (&mat["normalTexture"], TextureCookUsage::ColorLinear),
            (&mat["occlusionTexture"], TextureCookUsage::ColorLinear),
            (&pbr["metallicRoughnessTexture"], TextureCookUsage::ColorLinear),
        ] {
            let uri = info["index"]
                .as_u64()
                .and_then(|t| textures.get(t as usize))
                .and_then(|t| t["source"].as_u64())
                .and_then(|i| images.get(i as usize))
                .and_then(|i| i["uri"].as_str());
            if let Some(uri) = uri {
                let path = base.join(uri);
                if !out.iter().any(|(p, _)| *p == path) {
                    out.push((path, usage));
                }
            }
        }
    }
    Ok(out)
}

fn cook_one(path: &Path, usage: TextureCookUsage, max: u32, out_dir: &Path) -> anyhow::Result<()> {
    let mut image = image::open(path)?.to_rgba8();
    while image.width().max(image.height()) > max.max(4) {
        let (w, h) = ((image.width() / 2).max(1), (image.height() / 2).max(1));
        image = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
    }
    let cooked = cook_texture_rgba8(
        image.width(),
        image.height(),
        image.as_raw(),
        usage,
        "linux",
        TextureCookQuality::High,
    )?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("{}: no file stem", path.display()))?;
    // Temp + rename: a reader never sees a half-written map (stems are unique, see `run`).
    let target = out_dir.join(format!("{stem}.ktx2"));
    let tmp = out_dir.join(format!("{stem}.ktx2.{}.tmp", std::process::id()));
    std::fs::write(&tmp, cooked.ktx2).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    std::fs::rename(&tmp, &target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Two images whose outputs would both be `<out dir>/<stem>.ktx2`.
fn stem_clash(jobs: &[(PathBuf, TextureCookUsage)]) -> Option<(&Path, &Path)> {
    // Case-folded: on a case-insensitive output folder `Rock` and `rock` are one file (audit F11).
    let mut seen: std::collections::HashMap<String, &Path> = std::collections::HashMap::new();
    for (path, _) in jobs {
        let Some(stem) = path.file_stem() else {
            continue; // `cook_one` reports it
        };
        if let Some(first) = seen.insert(stem.to_string_lossy().to_lowercase(), path) {
            return Some((first, path));
        }
    }
    None
}

fn run(args: &[String]) -> anyhow::Result<usize> {
    let (Some(gltf), Some(out_dir)) = (args.first(), args.get(1)) else {
        anyhow::bail!("usage: cook_material_maps <file.gltf> <out dir> [--max 4096]");
    };
    let max = args
        .iter()
        .position(|a| a == "--max")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096u32);
    let out_dir = PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir)?;
    let jobs = wanted(Path::new(gltf))?;
    // Outputs are named by stem: two workers would otherwise write one file at once.
    if let Some((first, second)) = stem_clash(&jobs) {
        anyhow::bail!(
            "{} and {} would both cook to the same .ktx2 (same file stem); rename one",
            first.display(),
            second.display()
        );
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let errors: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(jobs.len().max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut errors = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((path, usage)) = jobs.get(i) else { break };
                        if let Err(e) = cook_one(path, *usage, max, &out_dir) {
                            errors.push(format!("{}: {e}", path.display()));
                        }
                    }
                    errors
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| {
                w.join()
                    .unwrap_or_else(|_| vec!["cook thread panicked".to_owned()])
            })
            .collect()
    });
    if let Some(first) = errors.first() {
        anyhow::bail!("{} of {} maps failed; first: {first}", errors.len(), jobs.len());
    }
    Ok(jobs.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit concurrency-1103 asset L7 / pipeline L3: two images with one file stem
    /// (`a/rock.png`, `b/rock.png`) both targeted `rock.ktx2` and were written by two workers at
    /// once; the tool still reported both cooked. It now refuses the clash.
    #[test]
    fn images_sharing_a_stem_are_refused() {
        let dir = regia_test_utils::unique_temp_dir("cook-material-maps-stems");
        for sub in ["a", "b"] {
            std::fs::create_dir_all(dir.join(sub)).expect("dir");
            image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 100, 50, 255]))
                .save(dir.join(sub).join("rock.png"))
                .expect("png");
        }
        let gltf = dir.join("scene.gltf");
        std::fs::write(
            &gltf,
            r#"{"materials":[{"pbrMetallicRoughness":{"baseColorTexture":{"index":0}}},
                {"pbrMetallicRoughness":{"baseColorTexture":{"index":1}}}],
                "textures":[{"source":0},{"source":1}],
                "images":[{"uri":"a/rock.png"},{"uri":"b/rock.png"}]}"#,
        )
        .expect("gltf");
        let out = dir.join("out");
        let result = run(&[gltf.display().to_string(), out.display().to_string()]);
        let err = result.expect_err("two images that both become rock.ktx2 must be refused");
        assert!(err.to_string().contains("rock"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod stem_tests {
    use super::*;

    /// audit F11: stems that differ only by case clash.
    #[test]
    fn stems_differing_by_case_clash() {
        let jobs = vec![
            (PathBuf::from("tex/A/Rock.png"), TextureCookUsage::ColorSrgb),
            (PathBuf::from("tex/B/rock.png"), TextureCookUsage::ColorSrgb),
        ];
        assert!(stem_clash(&jobs).is_some());
        assert!(stem_clash(&jobs[..1]).is_none());
    }
}
