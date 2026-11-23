// SPDX-License-Identifier: MIT-0
//! Reference path-traced ground truth for the parity plates (`docs/specs/27-renderer-excellence.md`
//! § 2): the same frame the real-time renderer draws ([`crate::look_r2::render_shot_with`]),
//! traced by `regia_pt` with the map's heightfield at full resolution, the planet's sky table
//! and sun disc, the planet's air and the level's height fog as media
//! ([`regia_pt::extract::media_from_outdoor`]), the level's sky bodies with their fill
//! ([`regia_pt::extract::sky_bodies_from_outdoor`]) and the terrain shader's own albedo
//! ([`regia_pt::terrain_bake`], baked by `look_r2` before the first frame). Terrain the camera
//! sees takes the frame's own per-pixel albedo and shading normal
//! ([`regia_pt::PtViewAlbedo`], rendered from the shot camera by `look_r2`), so the plates
//! compare light transport on the same material. Then the frame is scored three ways against
//! the real-time frame's linear HDR:
//!
//! - **beauty**: PT `beauty` vs the real-time HDR;
//! - **direct**: PT `direct` vs the real-time frame with every indirect term off;
//! - **GI**: PT `indirect_diffuse + indirect_specular` vs real-time (full − direct).
//!
//! [`terrain_albedo`] is the v1 CPU reconstruction (layer stack × macro map, no strata or
//! relief), kept for comparison; the plates use the shader bake.

use std::path::{Path, PathBuf};

use regia_pt::extract::{
    ExtractOptions, atmosphere_sky, from_frame, media_from_outdoor, sky_bodies_from_outdoor, star_radiance,
};
use regia_pt::{Aov, PathTracer, PtHeightfield, PtImage, PtMaterial, PtSettings, PtTexture};
use regia_render::compare::{self, CompareReport, DEFAULT_PPD};
use regia_terrain::{TerrainLayers, TerrainMap};

use crate::LookdevError;
use crate::look_r2::ShotCapture;

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// An 8-bit PNG decoded to linear RGB (sRGB-decoded).
struct LinearPng {
    width: u32,
    height: u32,
    rgb: Vec<[f32; 3]>,
}

fn read_png_linear(path: &Path) -> Result<LinearPng, LookdevError> {
    let file = std::fs::File::open(path).map_err(io(path))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| LookdevError::Png(format!("{}: {e}", path.display())))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| LookdevError::Png(format!("{}: too large", path.display())))?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| LookdevError::Png(format!("{}: {e}", path.display())))?;
    let ch = info.color_type.samples();
    // A lookup table: 256 sRGB codes → linear.
    let lut: Vec<f32> = (0..256).map(|v| srgb_to_linear(v as f32 / 255.0)).collect();
    let rgb = buf[..info.buffer_size()]
        .chunks_exact(ch)
        .map(|p| {
            if ch >= 3 {
                [lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]]
            } else {
                [lut[p[0] as usize]; 3]
            }
        })
        .collect();
    Ok(LinearPng {
        width: info.width,
        height: info.height,
        rgb,
    })
}

fn mean_rgb(v: &[[f32; 3]]) -> [f32; 3] {
    let n = v.len().max(1) as f64;
    let mut s = [0.0f64; 3];
    for c in v {
        for k in 0..3 {
            s[k] += f64::from(c[k]);
        }
    }
    [(s[0] / n) as f32, (s[1] / n) as f32, (s[2] / n) as f32]
}

/// The terrain albedo over the whole map at `resolution`² texels (row 0 at −Z).
///
/// # Errors
///
/// A texture of the map directory failed to decode.
pub fn terrain_albedo(
    map: &TerrainMap,
    layers: &TerrainLayers,
    map_dir: Option<&Path>,
    resolution: u32,
) -> Result<PtTexture, LookdevError> {
    let _span = tracing::info_span!("pt_terrain_albedo").entered();
    // Per-layer albedo: tint × mean of the layer's albedo texture when the drop has one.
    let mut layer_albedo = Vec::with_capacity(layers.count());
    let mut textured = Vec::new();
    for i in 0..layers.count() {
        let Some(l) = layers.layer(i) else { continue };
        let path = map_dir.map(|d| d.join("textures/detail").join(&l.name).join("albedo.png"));
        match path.filter(|p| p.exists()) {
            Some(p) => {
                let mean = mean_rgb(&read_png_linear(&p)?.rgb);
                let a = [
                    l.albedo[0] * mean[0],
                    l.albedo[1] * mean[1],
                    l.albedo[2] * mean[2],
                ];
                textured.push(mean);
                layer_albedo.push((a, true, l.albedo));
            }
            None => layer_albedo.push((l.albedo, false, l.albedo)),
        }
    }
    // Layers without a texture take the mean texture of the textured ones (the cook fills
    // every slot), times their own tint.
    let fill = if textured.is_empty() {
        [1.0; 3]
    } else {
        mean_rgb(&textured)
    };
    let layer_albedo: Vec<[f32; 3]> = layer_albedo
        .into_iter()
        .map(|(a, has_tex, tint)| {
            if has_tex {
                a
            } else {
                [tint[0] * fill[0], tint[1] * fill[1], tint[2] * fill[2]]
            }
        })
        .collect();
    // Macro albedo: cropped to the map and normalised to its mean (the real-time terrain's tint).
    let mut macro_strength = 0.0f32;
    let mut macro_img: Option<(LinearPng, f32, f32)> = None;
    if let Some(dir) = map_dir {
        let png_path = dir.join("textures/macro_albedo.png");
        let meta_path = dir.join("textures/macro.json");
        let mat_path = dir.join("textures/terrain.material.json");
        if png_path.exists() && meta_path.exists() {
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&meta_path).map_err(io(&meta_path))?)
                    .map_err(|e| LookdevError::Json(e.to_string()))?;
            let half = meta["half_m"].as_f64().unwrap_or(0.0) as f32;
            let mpp = meta["metres_per_px"].as_f64().unwrap_or(0.0) as f32;
            if half > 0.0 && mpp > 0.0 {
                macro_strength = std::fs::read_to_string(&mat_path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v["macro_strength"].as_f64())
                    .unwrap_or(0.85) as f32;
                macro_img = Some((read_png_linear(&png_path)?, half, mpp));
            }
        }
    }
    let res = resolution.max(2);
    let half_w = map.world_width() * 0.5;
    let at_world = |u: f32, v: f32| (u * map.world_width() - half_w, v * map.world_width() - half_w);
    let macro_sample = |x: f32, z: f32| -> Option<[f32; 3]> {
        let (img, half, mpp) = macro_img.as_ref()?;
        let flip = std::env::var_os("PT_MACRO_FLIP").is_some();
        let px = ((x + half) / mpp).clamp(0.0, (img.width - 1) as f32) as u32;
        let mut py = ((z + half) / mpp).clamp(0.0, (img.height - 1) as f32) as u32;
        if flip {
            py = img.height - 1 - py;
        }
        img.rgb.get((py * img.width + px) as usize).copied()
    };
    // Mean of the macro map over the map's footprint (the shader's normalisation).
    let mut macro_mean = [1.0f32; 3];
    if macro_img.is_some() {
        let mut acc = Vec::new();
        for j in 0..256 {
            for i in 0..256 {
                let (x, z) = at_world((i as f32 + 0.5) / 256.0, (j as f32 + 0.5) / 256.0);
                if let Some(c) = macro_sample(x, z) {
                    acc.push(c);
                }
            }
        }
        macro_mean = mean_rgb(&acc).map(|v| v.max(1e-3));
    }
    let mut rgba = Vec::with_capacity((res * res * 4) as usize);
    for j in 0..res {
        for i in 0..res {
            let (x, z) = at_world((i as f32 + 0.5) / res as f32, (j as f32 + 0.5) / res as f32);
            let n = map.normal_at(x, z);
            let slope = n[1].clamp(-1.0, 1.0).acos();
            let alt = map.sample_bilinear(x, z);
            let w = layers.weights(slope, alt);
            let total: f32 = w.iter().sum();
            let mut a = [0.0f32; 3];
            if total > 1e-5 {
                for (wi, la) in w.iter().zip(&layer_albedo) {
                    for k in 0..3 {
                        a[k] += wi * la[k];
                    }
                }
                a = a.map(|v| v / total);
            } else {
                a = layer_albedo.first().copied().unwrap_or([0.3; 3]);
            }
            if let Some(m) = macro_sample(x, z) {
                for k in 0..3 {
                    let tint = m[k] / macro_mean[k];
                    a[k] *= 1.0 + (tint - 1.0) * macro_strength;
                }
            }
            for v in a {
                // Stored sRGB-encoded (the tracer decodes on sampling).
                let c = v.clamp(0.0, 1.0);
                let s = if c <= 0.003_130_8 {
                    c * 12.92
                } else {
                    1.055 * c.powf(1.0 / 2.4) - 0.055
                };
                rgba.push((s * 255.0 + 0.5) as u8);
            }
            rgba.push(255);
        }
    }
    Ok(PtTexture {
        width: res,
        height: res,
        rgba: std::sync::Arc::from(rgba),
        srgb: true,
    })
}

/// Settings of one plate trace.
#[derive(Clone, Copy, Debug)]
pub struct PlateTrace {
    /// Samples per pixel.
    pub spp: u32,
    /// Maximum path length.
    pub max_bounces: u32,
    /// Trace seed (two traces of one shot with different seeds have independent noise).
    pub seed: u32,
}

/// GI where the attribution is unambiguous: pixels the tracer sees in full sun shadow, where
/// both renderers show indirect light only (beauty vs beauty, no direct-only frame needed).
#[derive(Clone, Debug, PartialEq)]
pub struct ShadowGi {
    /// Pixels in the mask.
    pub pixels: usize,
    /// Mean linear luminance of the tracer there.
    pub mean_pt: f32,
    /// Mean linear luminance of the real-time frame there.
    pub mean_rt: f32,
    /// RMSE of linear luminance over the mask divided by the tracer's mean.
    pub rel_rmse: f32,
    /// Mean of the beauty FLIP map over the mask (frame exposure).
    pub flip_mean: f32,
    /// 95th percentile of the beauty FLIP map over the mask.
    pub flip_p95: f32,
}

impl ShadowGi {
    fn to_json(&self) -> String {
        // JSON has no inf/NaN (an empty mask has no relative error): those become `null`.
        let num = |v: f32| {
            if v.is_finite() {
                format!("{v:.6e}")
            } else {
                "null".into()
            }
        };
        format!(
            "{{\"pixels\":{},\"mean_pt\":{},\"mean_rt\":{},\"ratio_rt_pt\":{},\"rel_rmse\":{},\"flip_mean\":{},\"flip_p95\":{}}}",
            self.pixels,
            num(self.mean_pt),
            num(self.mean_rt),
            num(self.mean_rt / self.mean_pt),
            num(self.rel_rmse),
            num(self.flip_mean),
            num(self.flip_p95)
        )
    }
}

fn shadow_gi(img: &PtImage, rt: &[[f32; 3]], flip: &[f32]) -> ShadowGi {
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let idx: Vec<usize> = (0..img.beauty.len())
        .filter(|&i| img.depth[i] > 0.0 && img.shadow[i] < 0.02 && img.emissive[i].iter().all(|v| *v == 0.0))
        .collect();
    let n = idx.len().max(1) as f64;
    let (mut sp, mut sr, mut se) = (0.0f64, 0.0f64, 0.0f64);
    let mut f: Vec<f32> = Vec::with_capacity(idx.len());
    for &i in &idx {
        let (p, r) = (f64::from(luma(img.beauty[i])), f64::from(luma(rt[i])));
        sp += p;
        sr += r;
        se += (p - r) * (p - r);
        f.push(flip[i]);
    }
    f.sort_by(f32::total_cmp);
    let mean_pt = sp / n;
    let p95 = ((f.len() as f32 * 0.95) as usize).min(f.len().saturating_sub(1));
    ShadowGi {
        pixels: idx.len(),
        mean_pt: mean_pt as f32,
        mean_rt: (sr / n) as f32,
        rel_rmse: ((se / n).sqrt() / mean_pt.max(1e-12)) as f32,
        flip_mean: (f.iter().map(|v| f64::from(*v)).sum::<f64>() / n) as f32,
        flip_p95: f.get(p95).copied().unwrap_or(0.0),
    }
}

/// Indirect irradiance at the visible surfaces: the GPU probes' screen lookup against the
/// tracer, albedo removed on both sides (irradiance / π; the tracer's `indirect_diffuse` over
/// its first-hit albedo).
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeIrradiance {
    /// Mean probe luminance / π over the tracer's sun-shadow mask.
    pub shadow_probe: f32,
    /// Mean tracer indirect irradiance / π over the same pixels.
    pub shadow_pt: f32,
    /// Mean probe luminance / π over fully sunlit pixels.
    pub lit_probe: f32,
    /// Mean tracer indirect irradiance / π over the same pixels.
    pub lit_pt: f32,
    /// Real-time albedo implied in shadow (frame luminance / probe irradiance), mean.
    pub shadow_albedo_rt: f32,
    /// Tracer first-hit albedo luminance in shadow, mean.
    pub shadow_albedo_pt: f32,
    /// Mean probe irradiance / π (rgb) in shadow.
    pub shadow_probe_rgb: [f32; 3],
    /// Mean tracer indirect irradiance / π (rgb) in shadow.
    pub shadow_pt_rgb: [f32; 3],
    /// Mean real-time indirect radiance (full − direct, rgb) in shadow.
    pub shadow_rt_gi_rgb: [f32; 3],
    /// Mean tracer indirect radiance (indirect diffuse + specular, rgb) in shadow.
    pub shadow_pt_gi_rgb: [f32; 3],
    /// The probe point.
    pub point: Option<ProbePoint>,
}

/// One compared point: the shadow pixel nearest the shadow mask's centroid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbePoint {
    /// Pixel (real-time frame orientation).
    pub pixel: [u32; 2],
    /// Tracer depth there.
    pub depth: f32,
    /// Probe irradiance / π (rgb), 9×9 mean.
    pub probe: [f32; 3],
    /// Tracer indirect irradiance / π (rgb), 9×9 mean.
    pub pt: [f32; 3],
}

impl ProbeIrradiance {
    fn to_json(&self) -> String {
        let point = self.point.map_or_else(
            || "null".to_string(),
            |ProbePoint { pixel: px, depth: d, probe: a, pt: b }| {
                format!(
                    "{{\"pixel\":[{},{}],\"depth\":{d:.3},\"probe\":[{:.4},{:.4},{:.4}],\"pt\":[{:.4},{:.4},{:.4}]}}",
                    px[0], px[1], a[0], a[1], a[2], b[0], b[1], b[2]
                )
            },
        );
        format!(
            "{{\"shadow_probe\":{:.5},\"shadow_pt\":{:.5},\"lit_probe\":{:.5},\"lit_pt\":{:.5},\"shadow_albedo_rt\":{:.4},\"shadow_albedo_pt\":{:.4},\"shadow_probe_rgb\":{:?},\"shadow_pt_rgb\":{:?},\"shadow_rt_gi_rgb\":{:?},\"shadow_pt_gi_rgb\":{:?},\"point\":{point}}}",
            self.shadow_probe,
            self.shadow_pt,
            self.lit_probe,
            self.lit_pt,
            self.shadow_albedo_rt,
            self.shadow_albedo_pt,
            self.shadow_probe_rgb,
            self.shadow_pt_rgb,
            self.shadow_rt_gi_rgb,
            self.shadow_pt_gi_rgb
        )
    }
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Probe vs tracer irradiance, plus a side-by-side picture (probe | tracer, one exposure, the
/// probe point marked) written to `image`.
fn probe_irradiance(
    img: &PtImage,
    full: &[[f32; 3]],
    rt_gi: &[[f32; 3]],
    pt_gi: &[[f32; 3]],
    screen: &(u32, u32, Vec<regia_render::DdgiScreenTexel>),
    image: &Path,
    mirror: bool,
) -> Result<ProbeIrradiance, LookdevError> {
    let (w, h) = (img.width, img.height);
    let (sw, sh, texels) = (screen.0, screen.1, &screen.2);
    // The probes' linear irradiance field at the tracer's own first-hit normal.
    let probe_at = |x: u32, y: u32| -> Option<[f32; 3]> {
        let t = texels.get(((y / 2).min(sh - 1) * sw + (x / 2).min(sw - 1)) as usize)?;
        let n = img.normal[(y * w + x) as usize];
        (t.coverage > 0.0).then(|| t.at(n))
    };
    let pt_at = |i: usize| -> Option<[f32; 3]> {
        let a = img.albedo[i];
        if img.depth[i] <= 0.0 || luma(a) < 0.02 || img.emissive[i].iter().any(|v| *v != 0.0) {
            return None;
        }
        let d = img.indirect_diffuse[i];
        Some(std::array::from_fn(|k| d[k] / a[k].max(1e-3)))
    };
    let (mut sp, mut st, mut lp, mut lt, mut ns, mut nl, mut ar, mut at) =
        (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0usize, 0usize, 0.0f64, 0.0f64);
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let mut rgb = [[0.0f64; 3]; 4];
    let mut probe_plane = vec![[0.0f32; 3]; (w * h) as usize];
    let mut pt_plane = vec![[0.0f32; 3]; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            let (Some(pr), Some(pt)) = (probe_at(x, y), pt_at(i)) else {
                continue;
            };
            probe_plane[i] = pr;
            pt_plane[i] = pt;
            if img.shadow[i] < 0.02 {
                sp += f64::from(luma(pr));
                st += f64::from(luma(pt));
                ar += f64::from(luma(full[i]) / luma(pr).max(1e-6));
                at += f64::from(luma(img.albedo[i]));
                cx += f64::from(x);
                cy += f64::from(y);
                ns += 1;
                for (acc, c) in rgb.iter_mut().zip([pr, pt, rt_gi[i], pt_gi[i]]) {
                    for k in 0..3 {
                        acc[k] += f64::from(c[k]);
                    }
                }
            } else if img.shadow[i] > 0.98 {
                lp += f64::from(luma(pr));
                lt += f64::from(luma(pt));
                nl += 1;
            }
        }
    }
    let mean = |v: f64, n: usize| (v / n.max(1) as f64) as f32;
    // The probe point: the shadow pixel nearest the shadow mask's centroid.
    let point = (ns > 0).then(|| {
        let (cx, cy) = (cx / ns as f64, cy / ns as f64);
        let mut best = (f64::MAX, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                if img.shadow[i] < 0.02 && pt_at(i).is_some() && probe_at(x, y).is_some() {
                    let d = (f64::from(x) - cx).powi(2) + (f64::from(y) - cy).powi(2);
                    if d < best.0 {
                        best = (d, x, y);
                    }
                }
            }
        }
        let (_, px, py) = best;
        let (mut a, mut b, mut n) = ([0.0f32; 3], [0.0f32; 3], 0.0f32);
        for y in py.saturating_sub(4)..(py + 5).min(h) {
            for x in px.saturating_sub(4)..(px + 5).min(w) {
                let i = (y * w + x) as usize;
                if let (Some(pr), Some(pt)) = (probe_at(x, y), pt_at(i)) {
                    for k in 0..3 {
                        a[k] += pr[k];
                        b[k] += pt[k];
                    }
                    n += 1.0;
                }
            }
        }
        let n = n.max(1.0);
        ProbePoint {
            pixel: [px, py],
            depth: img.depth[(py * w + px) as usize],
            probe: a.map(|v| v / n),
            pt: b.map(|v| v / n),
        }
    });
    // One exposure for both halves: the tracer's mean over every compared pixel, mid-grey.
    let all_mean = {
        let (s, n) = pt_plane
            .iter()
            .filter(|c| luma(**c) > 0.0)
            .fold((0.0f64, 0usize), |(s, n), c| (s + f64::from(luma(*c)), n + 1));
        (s / n.max(1) as f64).max(1e-6) as f32
    };
    let ev = regia_render::ev100_from_luminance(all_mean);
    let mut left = regia_pt::output::preview_rgba8(&probe_plane, ev);
    let mut right = regia_pt::output::preview_rgba8(&pt_plane, ev);
    // Mirror each half on its own (DO27 content) so the probes stay on the left.
    if mirror {
        crate::do27::mirror_columns(&mut left, w);
        crate::do27::mirror_columns(&mut right, w);
    }
    let gap = 8u32;
    let ow = w * 2 + gap;
    let mut rgba = vec![0u8; (ow * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let s = ((y * w + x) * 4) as usize;
            for (plane, ox) in [(&left, 0u32), (&right, w + gap)] {
                let d = ((y * ow + x + ox) * 4) as usize;
                rgba[d..d + 4].copy_from_slice(&plane[s..s + 4]);
            }
        }
    }
    if let Some(ProbePoint { pixel: [px, py], .. }) = point {
        let px = if mirror { w - 1 - px } else { px };
        for ox in [0u32, w + gap] {
            for t in -10i32..=10 {
                for (x, y) in [(px as i32 + t, py as i32), (px as i32, py as i32 + t)] {
                    if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                        let d = ((y as u32 * ow + x as u32 + ox) * 4) as usize;
                        rgba[d..d + 4].copy_from_slice(&[255, 0, 255, 255]);
                    }
                }
            }
        }
    }
    write_rgba(image, ow, h, &rgba, false)?;
    dump_raw(&image.with_extension("probe.f32"), &probe_plane)?;
    Ok(ProbeIrradiance {
        shadow_probe: mean(sp, ns),
        shadow_pt: mean(st, ns),
        lit_probe: mean(lp, nl),
        lit_pt: mean(lt, nl),
        shadow_albedo_rt: mean(ar, ns),
        shadow_albedo_pt: mean(at, ns),
        shadow_probe_rgb: rgb[0].map(|v| mean(v, ns)),
        shadow_pt_rgb: rgb[1].map(|v| mean(v, ns)),
        shadow_rt_gi_rgb: rgb[2].map(|v| mean(v, ns)),
        shadow_pt_gi_rgb: rgb[3].map(|v| mean(v, ns)),
        point,
    })
}

/// The scores of one plate.
#[derive(Clone, Debug)]
pub struct PlateScores {
    /// Shot name.
    pub shot: String,
    /// Exposure the FLIP/SSIM pair and the previews used.
    pub ev100: f32,
    /// PT beauty vs real-time HDR.
    pub beauty: CompareReport,
    /// PT direct vs real-time direct-only HDR.
    pub direct: CompareReport,
    /// PT indirect vs real-time (full − direct). The real-time terrain keeps indirect terms the
    /// frame switches do not remove (horizon bounce, engine sky irradiance), so this under-counts
    /// the real-time GI; [`Self::shadow`] is the attribution-free GI score.
    pub gi: CompareReport,
    /// GI in full sun shadow.
    pub shadow: ShadowGi,
    /// Probe vs tracer irradiance (frames rendered with GPU probes).
    pub probes: Option<ProbeIrradiance>,
    /// Tracer metadata.
    pub pt: regia_pt::PtMetadata,
}

impl PlateScores {
    /// JSON object text.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"shot\":\"{}\",\"ev100\":{:.3},\"beauty\":{},\"direct\":{},\"gi\":{},\"shadow_gi\":{},\"probes\":{},\"pt\":{}}}",
            self.shot,
            self.ev100,
            self.beauty.to_json(),
            self.direct.to_json(),
            self.gi.to_json(),
            self.shadow.to_json(),
            self.probes
                .as_ref()
                .map_or_else(|| "null".to_string(), ProbeIrradiance::to_json),
            serde_json::to_string(&self.pt).unwrap_or_else(|_| "null".into())
        )
    }
}

/// Diagnostic (`PT_PLATES_RAW=1`): `plane` as raw little-endian f32 RGB (row 0 at the top),
/// for offline per-pixel analysis.
fn dump_raw(path: &Path, plane: &[[f32; 3]]) -> Result<(), LookdevError> {
    if std::env::var_os("PT_PLATES_RAW").is_none() {
        return Ok(());
    }
    let bytes: Vec<u8> = plane.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(path, bytes).map_err(io(path))
}

/// Writes linear RGB layers (`<name>.R/G/B`) as a float EXR.
///
/// # Errors
///
/// The file could not be written.
pub fn write_linear_exr(
    path: &Path,
    w: u32,
    h: u32,
    layers: &[(&str, &[[f32; 3]])],
) -> Result<(), LookdevError> {
    let mut channels = Vec::new();
    for (name, plane) in layers {
        for (k, c) in ["R", "G", "B"].iter().enumerate() {
            channels.push(regia_pt::exr::ExrChannel {
                name: format!("{name}.{c}"),
                data: plane.iter().map(|p| p[k]).collect(),
            });
        }
    }
    regia_pt::exr::write(path, w, h, &channels, &[]).map_err(|e| LookdevError::Config(e.to_string()))
}

/// `tiles` (each `w × h` RGBA8) box-downsampled by 2 into a grid `cols` wide: a `w × (rows·h/2)`
/// image for two columns.
fn contact_sheet(tiles: &[Vec<u8>], w: u32, h: u32, cols: u32) -> Vec<u8> {
    let (tw, th) = (w / 2, h / 2);
    let rows = (tiles.len() as u32).div_ceil(cols);
    let (sw, sh) = (tw * cols, th * rows);
    let mut out = vec![0u8; (sw * sh * 4) as usize];
    for (k, tile) in tiles.iter().enumerate() {
        let (ox, oy) = ((k as u32 % cols) * tw, (k as u32 / cols) * th);
        for y in 0..th {
            for x in 0..tw {
                for c in 0..4 {
                    let at = |dx: u32, dy: u32| {
                        u32::from(tile[(((2 * y + dy) * w + 2 * x + dx) * 4 + c) as usize])
                    };
                    let v = (at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4;
                    out[(((oy + y) * sw + ox + x) * 4 + c) as usize] = v as u8;
                }
            }
        }
    }
    out
}

fn write_rgba(path: &Path, w: u32, h: u32, rgba: &[u8], mirror: bool) -> Result<(), LookdevError> {
    let mut px = rgba.to_vec();
    if mirror {
        crate::do27::mirror_columns(&mut px, w);
    }
    regia_pt::output::write_png(path, w, h, &px).map_err(|e| LookdevError::Config(e.to_string()))
}

/// Traces the frame of `capture` (the same scene [`trace_and_score`] scores: the frame's meshes
/// at their pose, the full-resolution heightfield with the terrain shader's albedo, the planet's
/// sky, media and sky bodies) and returns every AOV. `shot` prefixes the progress lines.
///
/// # Errors
///
/// The tracer failed.
pub fn trace_capture(
    tracer: &PathTracer,
    capture: &ShotCapture<'_>,
    shot: &str,
    settings: PlateTrace,
) -> Result<PtImage, LookdevError> {
    let frame = capture.frame;
    let albedo = capture.terrain_albedo.clone();
    let terrain_material = PtMaterial {
        base_color: [1.0; 4],
        roughness: 0.9,
        ..PtMaterial::default()
    };
    // The real-time sky table is built at the eye's world Y (`OutdoorState::apply_sun`).
    let sky = atmosphere_sky(&capture.planet, capture.toward_sun, frame.eye[1].max(1.0));
    let mut scene = from_frame(
        frame,
        &ExtractOptions {
            sky: Some(sky),
            sun_angular_radius: capture.planet.sun_angular_radius,
            terrain: Some((
                PtHeightfield::from_terrain_map(capture.map, 0),
                terrain_material,
                Some(albedo),
            )),
            skip_blended: true,
            lens: None,
            // Diagnostic: PT_PLATES_NO_MEDIA=1 traces without air and fog (sky table only).
            media: if std::env::var_os("PT_PLATES_NO_MEDIA").is_some() {
                regia_pt::PtMedia::default()
            } else {
                let mut m = media_from_outdoor(&capture.outdoor);
                // Diagnostic: PT_PLATES_MEDIA=air|haze traces one medium alone (the volumetric
                // AOV then splits the in-scatter by medium).
                match std::env::var("PT_PLATES_MEDIA").as_deref() {
                    Ok("air") => m.layers.retain(|l| l.aerial),
                    Ok("haze") => m.layers.retain(|l| !l.aerial),
                    _ => {}
                }
                m
            },
            sky_bodies: sky_bodies_from_outdoor(&capture.outdoor, capture.toward_sun),
            stars: star_radiance(&capture.planet),
            ..ExtractOptions::default()
        },
    );
    // Camera rays that hit the terrain take the albedo and shading normal the frame shaded
    // that pixel with, and so do bounce hits on terrain the camera sees. Diagnostics:
    // PT_PLATES_MAP_ALBEDO=1 keeps the map-wide bake and the smooth heightfield normal (the
    // reference before the view inputs); PT_PLATES_NO_VIEW_NORMAL=1 takes the view albedo only;
    // PT_PLATES_VIEW_FIRST_HIT=1 limits the view inputs to camera rays.
    if let Some(view) = capture
        .terrain_view_albedo
        .as_ref()
        .filter(|_| std::env::var_os("PT_PLATES_MAP_ALBEDO").is_none())
    {
        let normal = std::env::var_os("PT_PLATES_NO_VIEW_NORMAL")
            .is_none()
            .then(|| view.normal.clone());
        scene.set_terrain_view_albedo(view.view_proj, view.eye, view.albedo.clone(), normal);
        if let Some(v) = scene.terrain_view_albedo.as_mut() {
            v.all_bounces = std::env::var_os("PT_PLATES_VIEW_FIRST_HIT").is_none();
        }
    }
    let pt_settings = PtSettings {
        spp: settings.spp,
        max_bounces: settings.max_bounces,
        seed: settings.seed,
        rr_start: 3,
        jitter: true,
        samples_per_submit: 4,
    };
    let started = std::time::Instant::now();
    let mut session = tracer
        .begin(&scene, &pt_settings)
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    let total = settings.spp;
    session
        .run_with(total, |done| {
            if done % 64 == 0 || done == total {
                println!(
                    "  {shot}: {done}/{total} spp, {:.1} s",
                    started.elapsed().as_secs_f32()
                );
            }
            true
        })
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    session.image().map_err(|e| LookdevError::Config(e.to_string()))
}

/// Traces `capture` and scores the real-time frame against it, writing into `out`:
/// `<shot>_pt.exr` (all AOVs), `<shot>_rt.exr` (real-time `full`, `direct`, `gi` layers),
/// previews and FLIP maps for the three comparisons, and `<shot>_scores.json`.
///
/// # Errors
///
/// The trace, the albedo bake or a file write failed.
pub fn trace_and_score(
    tracer: &PathTracer,
    capture: &ShotCapture<'_>,
    shot: &str,
    settings: PlateTrace,
    out: &Path,
    mirror: bool,
) -> Result<PlateScores, LookdevError> {
    let frame = capture.frame;
    let (w, h) = (frame.width, frame.height);
    let img = trace_capture(tracer, capture, shot, settings)?;
    std::fs::create_dir_all(out).map_err(io(out))?;
    let path = |suffix: &str| -> PathBuf { out.join(format!("{shot}_{suffix}")) };
    img.write_exr(&path("pt.exr"), &[])
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    img.write_metadata_json(&path("pt.json"))
        .map_err(|e| LookdevError::Config(e.to_string()))?;

    let full = &capture.hdr_full.rgb;
    let direct = &capture.hdr_direct.rgb;
    if full.len() != img.beauty.len() || direct.len() != img.beauty.len() {
        return Err(LookdevError::Config("real-time HDR and PT sizes differ".into()));
    }
    let rt_gi: Vec<[f32; 3]> = full
        .iter()
        .zip(direct)
        .map(|(f, d)| {
            [
                (f[0] - d[0]).max(0.0),
                (f[1] - d[1]).max(0.0),
                (f[2] - d[2]).max(0.0),
            ]
        })
        .collect();
    for (name, plane) in [
        ("rt_full", full.as_slice()),
        ("rt_direct", direct.as_slice()),
        ("pt_beauty", img.beauty.as_slice()),
        ("pt_direct", img.direct.as_slice()),
        ("pt_albedo", img.albedo.as_slice()),
        ("pt_normal", img.normal.as_slice()),
        ("pt_indirect_diffuse", img.indirect_diffuse.as_slice()),
    ] {
        dump_raw(&path(&format!("{name}.f32")), plane)?;
    }
    let pt_gi: Vec<[f32; 3]> = img
        .indirect_diffuse
        .iter()
        .zip(&img.indirect_specular)
        .map(|(a, b)| [a[0] + b[0], a[1] + b[1], a[2] + b[2]])
        .collect();
    // The frame's terrain albedo per pixel (linear; 0 where it drew no terrain), beside the
    // tracer's first-hit `albedo` AOV.
    let rt_albedo: Option<Vec<[f32; 3]>> = capture.terrain_view_albedo.as_ref().map(|v| {
        v.albedo
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                if p[3] == 0 {
                    [0.0; 3]
                } else {
                    [0, 1, 2].map(|k| srgb_to_linear(f32::from(p[k]) / 255.0))
                }
            })
            .collect()
    });
    let mut layers: Vec<(&str, &[[f32; 3]])> = vec![("full", full), ("direct", direct), ("gi", &rt_gi)];
    if let Some(a) = rt_albedo.as_deref().filter(|a| a.len() == full.len()) {
        layers.push(("albedo", a));
    }
    write_linear_exr(&path("rt.exr"), w, h, &layers)?;
    // One exposure for every view of the shot: metered on the PT beauty like the runtime meter
    // (log-average luminance → EV100), so both renderers pass through the same AgX.
    let log_avg = {
        let mut s = 0.0f64;
        for c in &img.beauty {
            let y = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            s += f64::from(y.max(1e-4)).ln();
        }
        (s / img.beauty.len().max(1) as f64).exp() as f32
    };
    let ev100 = regia_render::ev100_from_luminance(log_avg);
    let score = |a: &[[f32; 3]],
                 b: &[[f32; 3]],
                 name: &str,
                 ev: f32|
     -> Result<(CompareReport, Vec<f32>), LookdevError> {
        let (report, flip) = compare::compare_linear(a, b, w, h, ev, DEFAULT_PPD);
        write_rgba(
            &path(&format!("{name}_flip.png")),
            w,
            h,
            &compare::flip_heatmap_rgba8(&flip.map),
            mirror,
        )?;
        Ok((report, flip.map))
    };
    let (beauty, beauty_flip) = score(&img.beauty, full, "beauty", ev100)?;
    let (direct_s, _) = score(&img.direct, direct, "direct", ev100)?;
    // GI is a fraction of the frame; expose it 2 stops brighter so FLIP sees its structure.
    let (gi, _) = score(&pt_gi, &rt_gi, "gi", ev100 - 2.0)?;
    let shadow = shadow_gi(&img, full, &beauty_flip);
    for (name, plane, ev) in [
        ("pt_beauty", &img.beauty, ev100),
        ("rt_beauty", full, ev100),
        ("pt_gi", &pt_gi, ev100 - 2.0),
        ("rt_gi", &rt_gi, ev100 - 2.0),
        ("pt_direct", &img.direct, ev100),
        ("rt_direct", direct, ev100),
    ] {
        write_rgba(
            &path(&format!("{name}.png")),
            w,
            h,
            &regia_pt::output::preview_rgba8(plane, ev),
            mirror,
        )?;
    }
    let albedo_rgba = regia_pt::output::preview_rgba8(&img.rgb(Aov::Albedo), 0.0);
    write_rgba(&path("pt_albedo.png"), w, h, &albedo_rgba, mirror)?;
    // Contact sheet at half size: real-time | PT beauty, real-time | PT GI, beauty FLIP | albedo.
    let tiles = [
        regia_pt::output::preview_rgba8(full, ev100),
        regia_pt::output::preview_rgba8(&img.beauty, ev100),
        regia_pt::output::preview_rgba8(&rt_gi, ev100 - 2.0),
        regia_pt::output::preview_rgba8(&pt_gi, ev100 - 2.0),
        compare::flip_heatmap_rgba8(&beauty_flip),
        albedo_rgba,
    ];
    // Each tile is mirrored like the previews (DO27's handedness), the layout is not: the
    // real-time frame stays on the left.
    let tiles = tiles.map(|mut t| {
        if mirror {
            crate::do27::mirror_columns(&mut t, w);
        }
        t
    });
    let sheet = contact_sheet(&tiles, w, h, 2);
    write_rgba(&path("sheet.png"), w, h * 3 / 2, &sheet, false)?;
    let probes = capture
        .probe_screen
        .as_ref()
        .map(|screen| {
            probe_irradiance(
                &img,
                full,
                &rt_gi,
                &pt_gi,
                screen,
                &path("probe_vs_pt_irradiance.png"),
                mirror,
            )
        })
        .transpose()?;
    let scores = PlateScores {
        shot: shot.to_string(),
        ev100,
        beauty,
        direct: direct_s,
        gi,
        shadow,
        probes,
        pt: img.metadata.clone(),
    };
    std::fs::write(path("scores.json"), scores.to_json()).map_err(io(&path("scores.json")))?;
    Ok(scores)
}
