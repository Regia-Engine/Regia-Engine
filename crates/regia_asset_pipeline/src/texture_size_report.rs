// SPDX-License-Identifier: MPL-2.0
//! `regia cook --size-report` (ADR-2063): every image of a project measured as the desktop cook
//! writes it (BCn), as the Android cook writes it in ASTC at the project's profile and as
//! ETC2 / EAC: bytes of the full mip chain and the level-0 PSNR of each, plus the RGBA8 size and
//! totals. Deterministic for a given project (sources sorted by path, a glTF's images in document
//! order).
//!
//! Measured: PNG, JPEG, TGA and BMP files under `assets/` with their `.meta` settings (usage,
//! `max_size`, `quality`; nothing is written), and the images of `.gltf` and `.glb` files, listed
//! as `assets/model.glb#texture/0` (the cook's sub-asset path) with the usage of the material slot
//! they fill. An external image a `.gltf` references is cooked twice when it also lies under
//! `assets/`, so it is listed twice. Not measured, and listed as skipped with the reason: 16-bit
//! heightfields, images with `compress: false`, images smaller than one block, files that do not
//! decode. Terrain arrays a host cooks with [`crate::cook_terrain_material`] and the images of
//! other model formats are not listed.

use std::path::Path;

use regia_asset::TextureCookUsage;

use crate::gltf_import::visit_gltf_images;
use crate::texture_compress::{
    TextureCookError, TextureCookQuality, android_format_report, cook_texture_rgba8,
};
use crate::{AndroidTextureFormat, TextureImporterSettings, TexturePolicy, usage_from_file_name};

/// The JSON schema version of [`TextureSizeReport`].
pub const SIZE_REPORT_SCHEMA: u32 = 1;

/// One codec's measurement of a texture.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CodecSize {
    /// The block format (`BC7_SRGB`, `ASTC_4x4`, `ETC2_R8G8B8_SRGB`, ...).
    pub format: String,
    /// Bytes of the full mip chain (block payload, no container).
    pub bytes: u64,
    /// PSNR (dB) of the decoded level 0 against the source over the channels the usage stores;
    /// 99.0 for an exact match.
    pub psnr_db: f64,
}

/// One texture of the report.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TextureSizeRow {
    /// Project-relative path (`assets/...`; `assets/model.glb#texture/0` for a glTF's image).
    pub path: String,
    /// The usage the cook gives it (`color_srgb`, `color_linear`, `normal_map`, `mask`, `ui`).
    pub usage: String,
    /// Width after `max_size`.
    pub width: u32,
    /// Height after `max_size`.
    pub height: u32,
    /// Bytes of the full RGBA8 mip chain.
    pub rgba8_bytes: u64,
    /// The desktop cook (BCn).
    pub bc: CodecSize,
    /// The Android ASTC cook at the project's profile.
    pub astc: CodecSize,
    /// The Android ETC2 / EAC cook.
    pub etc2: CodecSize,
}

/// Totals over every measured texture.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SizeTotals {
    /// RGBA8 chains.
    pub rgba8_bytes: u64,
    /// BCn chains.
    pub bc_bytes: u64,
    /// ASTC chains.
    pub astc_bytes: u64,
    /// ETC2 / EAC chains.
    pub etc2_bytes: u64,
    /// What the Android cook writes under the project's policy (the ASTC or the ETC2 total).
    pub android_bytes: u64,
}

/// An image the report did not measure.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SkippedImage {
    /// Project-relative path.
    pub path: String,
    /// Why (`compress: false`, a heightfield, smaller than a block, does not decode).
    pub reason: String,
}

/// The size report of one project.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TextureSizeReport {
    /// [`SIZE_REPORT_SCHEMA`].
    pub schema: u32,
    /// The Android policy the project cooks with (`[cook.textures.android]`).
    pub android_policy: TexturePolicy,
    /// Every measured texture, by path.
    pub textures: Vec<TextureSizeRow>,
    /// Images that were not measured.
    pub skipped: Vec<SkippedImage>,
    /// Totals.
    pub totals: SizeTotals,
}

/// `TextureCookUsage` in the report's snake_case spelling.
fn usage_label(usage: TextureCookUsage) -> &'static str {
    match usage {
        TextureCookUsage::ColorSrgb => "color_srgb",
        TextureCookUsage::ColorLinear => "color_linear",
        TextureCookUsage::NormalMap => "normal_map",
        TextureCookUsage::Mask => "mask",
        TextureCookUsage::Ui => "ui",
    }
}

/// `(source channel, decoded channel)` pairs a usage stores in BCn and ETC2 (BC5 / RG11: X, Y;
/// BC4 / R11: R; colour RGB, or RGBA when the image has alpha).
fn channels(usage: TextureCookUsage, alpha: bool) -> &'static [(usize, usize)] {
    match usage {
        TextureCookUsage::NormalMap => &[(0, 0), (1, 1)],
        TextureCookUsage::Mask => &[(0, 0)],
        _ if alpha => &[(0, 0), (1, 1), (2, 2), (3, 3)],
        _ => &[(0, 0), (1, 1), (2, 2)],
    }
}

/// PSNR (dB) between two RGBA8 images over `pairs`; 99.0 when identical.
fn psnr(source: &[u8], decoded: &[u8], pairs: &[(usize, usize)]) -> f64 {
    let mut sum = 0f64;
    let mut n = 0f64;
    for (s, d) in source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
        for &(sc, dc) in pairs {
            let e = f64::from(s[sc]) - f64::from(d[dc]);
            sum += e * e;
            n += 1.0;
        }
    }
    if sum == 0.0 || n == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
}

/// The desktop (BCn) cook of one image: format, chain bytes and level-0 PSNR.
fn measure_bc(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
) -> Result<CodecSize, TextureCookError> {
    use ktx2::Format as F;
    use regia_asset::bcn_decode::{BcnKind, decode_bcn};
    let bad = |what: String| TextureCookError::Transcode(what);
    let cooked = cook_texture_rgba8(width, height, rgba, usage, "linux", quality)?;
    let reader = ktx2::Reader::new(&cooked.ktx2[..]).map_err(|e| bad(e.to_string()))?;
    let format = reader
        .header()
        .format
        .ok_or_else(|| bad("undefined format".into()))?;
    let (kind, name) = match format {
        F::BC1_RGB_SRGB_BLOCK | F::BC1_RGBA_SRGB_BLOCK => (BcnKind::Bc1, "BC1_SRGB"),
        F::BC1_RGB_UNORM_BLOCK | F::BC1_RGBA_UNORM_BLOCK => (BcnKind::Bc1, "BC1_UNORM"),
        F::BC3_SRGB_BLOCK => (BcnKind::Bc3, "BC3_SRGB"),
        F::BC3_UNORM_BLOCK => (BcnKind::Bc3, "BC3_UNORM"),
        F::BC4_UNORM_BLOCK => (BcnKind::Bc4, "BC4_UNORM"),
        F::BC5_UNORM_BLOCK => (BcnKind::Bc5, "BC5_UNORM"),
        F::BC7_SRGB_BLOCK => (BcnKind::Bc7, "BC7_SRGB"),
        F::BC7_UNORM_BLOCK => (BcnKind::Bc7, "BC7_UNORM"),
        other => return Err(bad(format!("unexpected desktop format {other:?}"))),
    };
    let bytes = reader.levels().map(|l| l.data.len() as u64).sum();
    let level0 = reader.levels().next().ok_or_else(|| bad("no levels".into()))?;
    let decoded = decode_bcn(kind, width, height, level0.data).map_err(|e| bad(e.to_string()))?;
    let alpha = rgba.as_chunks::<4>().0.iter().any(|p| p[3] < 255);
    Ok(CodecSize {
        format: name.to_string(),
        bytes,
        psnr_db: psnr(rgba, &decoded, channels(usage, alpha)),
    })
}

/// Measures one RGBA8 image for the report: its BCn, ASTC (at `quality` after the policy's
/// profile) and ETC2 / EAC chains.
///
/// # Errors
/// The image is smaller than one block, the buffer does not match the size, or encoding fails.
pub fn measure_texture(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
    policy: TexturePolicy,
) -> Result<(u64, CodecSize, CodecSize, CodecSize), TextureCookError> {
    let _span = tracing::info_span!("texture_size_measure", width, height).entered();
    let bc = measure_bc(width, height, rgba, usage, quality)?;
    let android = android_format_report(width, height, rgba, usage, policy.astc_profile.quality(quality))?;
    Ok((
        android.rgba8_bytes,
        bc,
        CodecSize {
            format: format!("ASTC_{}", android.astc_block),
            bytes: android.astc_bytes,
            psnr_db: android.astc_psnr_db,
        },
        CodecSize {
            format: android.etc2_format,
            bytes: android.etc2_bytes,
            psnr_db: android.etc2_psnr_db,
        },
    ))
}

/// The size report of the project at `project_root` (see the module docs).
///
/// # Errors
/// `Regia.toml` holds a bad `[cook.textures]` table.
pub fn texture_size_report(project_root: &Path) -> Result<TextureSizeReport, String> {
    let policy = TexturePolicy::for_project(project_root, "android")?;
    let assets = project_root.join("assets");
    let mut sources: Vec<_> = crate::AssetDatabase::discover_sources(&assets)
        .into_iter()
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "tga" | "bmp" | "gltf" | "glb"
                )
            })
        })
        .collect();
    sources.sort();
    let mut report = TextureSizeReport {
        schema: SIZE_REPORT_SCHEMA,
        android_policy: policy,
        textures: Vec::new(),
        skipped: Vec::new(),
        totals: SizeTotals::default(),
    };
    for source in sources {
        let rel = source
            .strip_prefix(project_root)
            .unwrap_or(&source)
            .to_string_lossy()
            .replace('\\', "/");
        if is_gltf(&source) {
            measure_gltf(&source, &rel, policy, &mut report);
        } else {
            let measured = measure_source(&source, policy);
            report.add(rel, measured);
        }
    }
    let t = &mut report.totals;
    for row in &report.textures {
        t.rgba8_bytes += row.rgba8_bytes;
        t.bc_bytes += row.bc.bytes;
        t.astc_bytes += row.astc.bytes;
        t.etc2_bytes += row.etc2.bytes;
    }
    t.android_bytes = match policy.android_format {
        AndroidTextureFormat::Astc => t.astc_bytes,
        AndroidTextureFormat::Etc2 => t.etc2_bytes,
    };
    Ok(report)
}

impl TextureSizeReport {
    /// Records one image under `path`: a row, or a skipped entry (`Ok(None)`: its settings keep
    /// it uncompressed; `Err`: the reason).
    fn add(&mut self, path: String, measured: Result<Option<TextureSizeRow>, String>) {
        match measured {
            Ok(Some(row)) => self.textures.push(TextureSizeRow { path, ..row }),
            Ok(None) => self.skipped.push(SkippedImage {
                path,
                reason: "compress: false".to_string(),
            }),
            Err(reason) => self.skipped.push(SkippedImage { path, reason }),
        }
    }
}

/// Whether `source` is a `.gltf` or `.glb` file.
fn is_gltf(source: &Path) -> bool {
    source
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "gltf" | "glb"))
}

/// Every image of the glTF at `source` measured as `{rel}#texture/{index}`; a file that does not
/// parse is one skipped entry.
fn measure_gltf(source: &Path, rel: &str, policy: TexturePolicy, report: &mut TextureSizeReport) {
    let visited = visit_gltf_images(source, |index, input| {
        let measured = input
            .map_err(|e| format!("does not decode: {e}"))
            .and_then(|input| {
                if !input.settings.compress() {
                    return Ok(None);
                }
                let quality = input.settings.quality_or(TextureCookQuality::High);
                let (width, height, rgba) = input.image;
                measure_image(width, height, &rgba, input.usage, quality, policy).map(Some)
            });
        report.add(format!("{rel}#texture/{index}"), measured);
    });
    if let Err(e) = visited {
        report.add(rel.to_string(), Err(format!("does not decode: {e}")));
    }
}

/// One RGBA8 image measured, as a row without its path.
fn measure_image(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    quality: TextureCookQuality,
    policy: TexturePolicy,
) -> Result<TextureSizeRow, String> {
    if width < 4 || height < 4 {
        return Err(format!("{width}x{height} is smaller than one block"));
    }
    let (rgba8_bytes, bc, astc, etc2) =
        measure_texture(width, height, rgba, usage, quality, policy).map_err(|e| e.to_string())?;
    Ok(TextureSizeRow {
        path: String::new(),
        usage: usage_label(usage).to_string(),
        width,
        height,
        rgba8_bytes,
        bc,
        astc,
        etc2,
    })
}

/// One image file measured; `Ok(None)` when its settings keep it uncompressed, `Err` with the
/// reason when it is not measured.
fn measure_source(source: &Path, policy: TexturePolicy) -> Result<Option<TextureSizeRow>, String> {
    let bytes = std::fs::read(source).map_err(|e| format!("unreadable: {e}"))?;
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let settings = TextureImporterSettings::for_source(source);
    if !settings.compress() {
        return Ok(None);
    }
    let image = match ext.as_str() {
        "tga" => crate::cook::import_image_rgba8_as(&bytes, image::ImageFormat::Tga),
        "bmp" => crate::cook::import_image_rgba8_as(&bytes, image::ImageFormat::Bmp),
        _ if crate::import_png_height(&bytes).is_ok() => {
            return Err("16-bit heightfield (not a texture)".to_string());
        }
        _ => crate::import_png_rgba8(&bytes),
    }
    .map_err(|e| format!("does not decode: {e}"))?;
    let usage = settings.usage_or(usage_from_file_name(source));
    let (width, height, rgba) = settings.fit(image.width, image.height, image.rgba8);
    let quality = settings.quality_or(TextureCookQuality::High);
    measure_image(width, height, &rgba, usage, quality, policy).map(Some)
}

/// The report as a fixed-width table with totals (what `regia cook --size-report` prints).
#[must_use]
pub fn format_size_report(report: &TextureSizeReport) -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut out = format!(
        "Android texture policy: format {}, ASTC profile {}\n",
        report.android_policy.android_format.label(),
        report.android_policy.astc_profile.label()
    );
    out.push_str(&format!(
        "{:<40} {:>9} {:>9} {:<10} {:>6} {:<9} {:>6} {:<18} {:>6}\n",
        "texture", "size", "BC MiB", "BC", "dB", "ASTC", "dB", "ETC2", "dB"
    ));
    for r in &report.textures {
        out.push_str(&format!(
            "{:<40} {:>9} {:>9.3} {:<10} {:>6.1} {:<9} {:>6.1} {:<18} {:>6.1}\n",
            r.path,
            format!("{}x{}", r.width, r.height),
            mib(r.bc.bytes),
            r.bc.format,
            r.bc.psnr_db,
            r.astc.format,
            r.astc.psnr_db,
            r.etc2.format,
            r.etc2.psnr_db,
        ));
    }
    for s in &report.skipped {
        out.push_str(&format!("{:<40} skipped: {}\n", s.path, s.reason));
    }
    let t = &report.totals;
    let ratio = |b: u64| {
        if t.bc_bytes == 0 {
            0.0
        } else {
            b as f64 / t.bc_bytes as f64 * 100.0
        }
    };
    out.push_str(&format!(
        "total ({} textures): RGBA8 {:.2} MiB, BC {:.2} MiB, ASTC {:.2} MiB ({:.0}% of BC), ETC2/EAC {:.2} MiB ({:.0}% of BC); Android cook {:.2} MiB\n",
        report.textures.len(),
        mib(t.rgba8_bytes),
        mib(t.bc_bytes),
        mib(t.astc_bytes),
        ratio(t.astc_bytes),
        mib(t.etc2_bytes),
        ratio(t.etc2_bytes),
        mib(t.android_bytes),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth test image.
    fn smooth(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, 128, 255]
            })
            .collect()
    }

    #[test]
    fn a_texture_is_measured_in_all_three_families() {
        let (w, h) = (32, 32);
        let rgba = smooth(w, h);
        let (rgba8, bc, astc, etc2) = measure_texture(
            w,
            h,
            &rgba,
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::High,
            TexturePolicy::default(),
        )
        .expect("measure");
        // 32 → 1 is six levels: 64 + 16 + 4 + 1 + 1 + 1 = 87 blocks.
        assert_eq!(rgba8, (1024 + 256 + 64 + 16 + 4 + 1) * 4);
        assert_eq!((bc.format.as_str(), bc.bytes), ("BC7_SRGB", 87 * 16));
        assert_eq!((astc.format.as_str(), astc.bytes), ("ASTC_4x4", 87 * 16));
        assert_eq!((etc2.format.as_str(), etc2.bytes), ("ETC2_R8G8B8_SRGB", 87 * 8));
        for c in [&bc, &astc, &etc2] {
            assert!(c.psnr_db > 30.0, "{c:?}");
        }
        // The low profile takes 8×8 on a power-of-two size (6 does not divide 32).
        let low = TexturePolicy {
            astc_profile: crate::AstcProfile::Low,
            ..TexturePolicy::default()
        };
        let (_, _, astc_low, _) = measure_texture(
            w,
            h,
            &rgba,
            TextureCookUsage::ColorSrgb,
            TextureCookQuality::High,
            low,
        )
        .expect("measure");
        assert_eq!(astc_low.format, "ASTC_8x8");
        assert!(astc_low.bytes < astc.bytes);
    }
}
