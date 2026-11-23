// SPDX-License-Identifier: MPL-2.0
//! Alpha-coverage-preserving mips (ADR-3090): a sparse alpha mask (a canopy card) keeps mip 0's
//! coverage down the whole chain with `alpha_cutoff` set, on every encoder; without the setting the
//! cook's output is byte for byte what it was, and the setting is part of the cook key.
//!
//! Run with `--nocapture` to see the coverage tables the report quotes.

use std::path::Path;

use image::ImageEncoder;
use regia_asset::{CookedTexture, MetaSidecar, TextureCookUsage, cooked_cache_dir, meta_path_for};
use regia_asset_pipeline::alpha_coverage::{chain_coverage, coverage, coverage_mip_chain};
use regia_asset_pipeline::{
    AndroidTextureFormat, CookOptions, TextureCookQuality, TextureImporterSettings, TexturePolicy,
    cook_project, cook_texture_rgba8, cook_texture_rgba8_coverage, host_platform, with_texture_policy,
};

const SIZE: u32 = 256;
const CUTOFF: f32 = 0.5;

/// A 256×256 canopy card: about 300 small leaves (radius 1.5 to 4 texels, a soft one-texel edge)
/// on a transparent background, deterministic.
fn leaves() -> Vec<u8> {
    let mut state = 0x9e37_79b9_u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut alpha = vec![0f32; (SIZE * SIZE) as usize];
    for _ in 0..300 {
        let cx = (next() % SIZE) as f32;
        let cy = (next() % SIZE) as f32;
        let r = 1.5 + (next() % 1000) as f32 / 1000.0 * 2.5;
        let (x0, x1) = (
            (cx - r - 2.0).max(0.0) as u32,
            ((cx + r + 2.0) as u32).min(SIZE - 1),
        );
        let (y0, y1) = (
            (cy - r - 2.0).max(0.0) as u32,
            ((cy + r + 2.0) as u32).min(SIZE - 1),
        );
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let a = (r + 0.5 - d).clamp(0.0, 1.0);
                let i = (y * SIZE + x) as usize;
                alpha[i] = alpha[i].max(a);
            }
        }
    }
    let mut out = Vec::with_capacity(alpha.len() * 4);
    for (i, a) in alpha.iter().enumerate() {
        let g = 90 + (i % 7) as u8 * 10;
        out.extend_from_slice(&[40, g, 30, (a * 255.0).round() as u8]);
    }
    out
}

/// How far a level of `n` texels may sit from the target: 2 % for 8×8 and larger, and the best a
/// level can do for smaller ones (half a texel's share; one texel is 1/n of the coverage).
fn allowed(n: u32, two_percent: f32) -> f32 {
    if n >= 64 {
        two_percent
    } else {
        0.5 / n as f32 + 1e-6
    }
}

#[test]
fn coverage_falls_without_the_setting_and_stays_within_two_percent_with_it() {
    let rgba = leaves();
    let off = coverage_mip_chain(SIZE, SIZE, &rgba, TextureCookUsage::ColorSrgb, None);
    let on = coverage_mip_chain(SIZE, SIZE, &rgba, TextureCookUsage::ColorSrgb, Some(CUTOFF));
    let c_off = chain_coverage(&off, 3, CUTOFF);
    let c_on = chain_coverage(&on, 3, CUTOFF);
    let c0 = c_off[0];
    assert_eq!(c0, c_on[0], "mip 0 is never scaled");
    assert!((0.05..0.5).contains(&c0), "a sparse mask: {c0}");
    eprintln!("level  size      off      on   (mip 0 coverage {:.4})", c0);
    for (i, ((w, h, _), (off, on))) in on.iter().zip(c_off.iter().zip(&c_on)).enumerate() {
        eprintln!("{i:>5}  {w:>3}x{h:<3}  {off:.4}  {on:.4}");
    }
    // Off: the far levels thin out (the problem this fixes), most of the canopy gone by 16x16.
    let at_16 = c_off[4];
    assert!(at_16 < c0 * 0.5, "16x16 without the setting: {at_16} vs {c0}");
    // On: within 2 % of mip 0 at every level of 64 texels or more, the closest possible below.
    for (i, ((w, h, _), c)) in on.iter().zip(&c_on).enumerate() {
        let d = (c - c0).abs();
        assert!(d <= allowed(w * h, 0.02), "level {i} ({w}x{h}): {c} vs {c0}");
    }
    // The colour channels are untouched.
    for ((_, _, a), (_, _, b)) in off.iter().zip(&on) {
        assert!(
            a.as_chunks::<4>()
                .0
                .iter()
                .zip(b.as_chunks::<4>().0)
                .all(|(p, q)| p[..3] == q[..3])
        );
    }
}

/// Every level of a cooked KTX2, decoded to RGBA8 (the runtime's CPU decoder).
fn decoded_levels(ktx2: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    let texture = regia_render::CompressedTexture::from_ktx2(ktx2).expect("ktx2");
    (0..)
        .map_while(|level| texture.decode_level_rgba8(level))
        .map(|t| (t.width, t.height, t.rgba))
        .collect()
}

#[test]
fn every_encoder_decodes_back_to_the_chain_coverage() {
    let rgba = leaves();
    let reference = coverage_mip_chain(SIZE, SIZE, &rgba, TextureCookUsage::ColorSrgb, Some(CUTOFF));
    let target = chain_coverage(&reference, 3, CUTOFF);
    let etc2 = TexturePolicy {
        android_format: AndroidTextureFormat::Etc2,
        ..TexturePolicy::default()
    };
    // (name, platform, quality, policy, tolerance at 8x8 and larger, smallest edge checked).
    // ASTC 8x8 (Android's low quality, 2 bits a texel) is checked down to 32x32 (16 blocks): below
    // that one block holds a quarter of the level or more, and its single endpoint pair cannot
    // keep a sparse alpha pattern (measured: 16x16 decodes to about half the coverage).
    let encoders = [
        (
            "bc7",
            "linux",
            TextureCookQuality::High,
            TexturePolicy::default(),
            0.02,
            1,
        ),
        (
            "bc3",
            "linux",
            TextureCookQuality::Low,
            TexturePolicy::default(),
            0.02,
            1,
        ),
        (
            "astc 4x4",
            "android",
            TextureCookQuality::High,
            TexturePolicy::default(),
            0.02,
            1,
        ),
        (
            "astc 8x8",
            "android",
            TextureCookQuality::Low,
            TexturePolicy::default(),
            0.03,
            32,
        ),
        ("etc2 rgba8", "android", TextureCookQuality::High, etc2, 0.02, 1),
    ];
    let mut failures = Vec::new();
    for (name, platform, quality, policy, tolerance, min_edge) in encoders {
        let cooked = with_texture_policy(policy, || {
            cook_texture_rgba8_coverage(
                SIZE,
                SIZE,
                &rgba,
                TextureCookUsage::ColorSrgb,
                platform,
                quality,
                Some(CUTOFF),
            )
        })
        .expect("cook");
        let levels = decoded_levels(&cooked.ktx2);
        assert_eq!(levels.len(), reference.len(), "{name}: the whole chain");
        let mut line = format!("{name:<10}");
        for (i, ((w, h, px), t)) in levels.iter().zip(&target).enumerate() {
            let c = coverage(px, 3, CUTOFF);
            line.push_str(&format!(" {c:.4}"));
            if (*w).min(*h) < min_edge {
                continue;
            }
            // The encoder's error against the chain it was given.
            if (c - t).abs() > allowed(w * h, tolerance) {
                failures.push(format!(
                    "{name} level {i} ({w}x{h}): decoded {c:.4}, chain {t:.4}"
                ));
            }
            // Against mip 0 as well: the decoded chain keeps the canopy.
            if (c - target[0]).abs() > allowed(w * h, tolerance + 0.02) {
                failures.push(format!(
                    "{name} level {i} ({w}x{h}): decoded {c:.4}, mip 0 {:.4}",
                    target[0]
                ));
            }
        }
        eprintln!("{line}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn without_the_setting_every_cook_is_byte_identical() {
    let rgba = leaves();
    let etc2 = TexturePolicy {
        android_format: AndroidTextureFormat::Etc2,
        ..TexturePolicy::default()
    };
    for (platform, policy) in [
        ("linux", TexturePolicy::default()),
        ("android", TexturePolicy::default()),
        ("android", etc2),
    ] {
        for usage in [
            TextureCookUsage::ColorSrgb,
            TextureCookUsage::ColorLinear,
            TextureCookUsage::Mask,
            TextureCookUsage::NormalMap,
        ] {
            for quality in [TextureCookQuality::High, TextureCookQuality::Low] {
                let plain = with_texture_policy(policy, || {
                    cook_texture_rgba8(64, 64, &rgba[..64 * 64 * 4], usage, platform, quality)
                })
                .expect("plain");
                let none = with_texture_policy(policy, || {
                    cook_texture_rgba8_coverage(64, 64, &rgba[..64 * 64 * 4], usage, platform, quality, None)
                })
                .expect("none");
                assert_eq!(plain, none, "{platform} {usage:?} {quality:?}");
            }
        }
        // A normal map has no coverage channel: the setting changes nothing.
        let plain = with_texture_policy(policy, || {
            cook_texture_rgba8(
                64,
                64,
                &rgba[..64 * 64 * 4],
                TextureCookUsage::NormalMap,
                platform,
                TextureCookQuality::High,
            )
        })
        .expect("plain");
        let set = with_texture_policy(policy, || {
            cook_texture_rgba8_coverage(
                64,
                64,
                &rgba[..64 * 64 * 4],
                TextureCookUsage::NormalMap,
                platform,
                TextureCookQuality::High,
                Some(0.5),
            )
        })
        .expect("set");
        assert_eq!(plain, set, "{platform} normal map");
    }
    // The settings string existing cook caches were keyed on is unchanged without the field.
    assert_eq!(
        TextureImporterSettings::default().fingerprint(),
        "usage=None;max=None;compress=None;quality=None"
    );
    let set = TextureImporterSettings::from_meta_string(Some("(alpha_cutoff: Some(0.5))"));
    assert_eq!(set.alpha_cutoff, Some(0.5));
    assert_ne!(
        set.fingerprint(),
        TextureImporterSettings::default().fingerprint()
    );
    assert_ne!(
        set.fingerprint(),
        TextureImporterSettings::from_meta_string(Some("(alpha_cutoff: Some(0.4))")).fingerprint()
    );
    // Out of range or on a normal map: ignored.
    for bad in [
        "(alpha_cutoff: Some(0.0))",
        "(alpha_cutoff: Some(1.5))",
        "(alpha_cutoff: Some(-1.0))",
    ] {
        assert_eq!(
            TextureImporterSettings::from_meta_string(Some(bad)).coverage_cutoff(TextureCookUsage::ColorSrgb),
            None,
            "{bad}"
        );
    }
    assert_eq!(set.coverage_cutoff(TextureCookUsage::NormalMap), None);
    assert_eq!(set.coverage_cutoff(TextureCookUsage::ColorSrgb), Some(0.5));
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    std::fs::write(path, out).expect("png");
}

fn cooked_texture(root: &Path, rel: &str) -> Vec<u8> {
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == rel)
        .unwrap_or_else(|| panic!("{rel} not in the manifest"));
    assert_eq!(entry.type_path, "regia_asset::CookedTexture");
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(entry.id))).expect("cooked");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("decode");
    CookedTexture::from_postcard(payload)
        .expect("texture")
        .ktx2
        .to_vec()
}

#[test]
fn the_setting_is_part_of_the_cook_key_and_nothing_else_recooks() {
    let root = regia_test_utils::unique_temp_dir("coverage-cook-key");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let png = assets.join("canopy_albedo.png");
    write_png(&png, SIZE, SIZE, &leaves());
    let cook = || {
        let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        report
    };
    let first = cook();
    assert_eq!(first.cooked, 1);
    let plain = cooked_texture(&root, "assets/canopy_albedo.png");
    // The default cook is the plain cook of the same pixels.
    let direct = cook_texture_rgba8(
        SIZE,
        SIZE,
        &leaves(),
        TextureCookUsage::ColorSrgb,
        host_platform(),
        TextureCookQuality::High,
    )
    .expect("direct");
    assert_eq!(
        plain,
        direct.ktx2.to_vec(),
        "byte-identical to the cook without the setting"
    );

    let again = cook();
    assert_eq!((again.cooked, again.skipped, again.imports_skipped), (0, 1, 1));

    let meta_path = meta_path_for(&png);
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("meta parses");
    meta.settings = Some("(alpha_cutoff: Some(0.5))".into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("write meta");
    let set = cook();
    assert_eq!((set.cooked, set.imports_skipped), (1, 0), "the setting recooks");
    let covered = cooked_texture(&root, "assets/canopy_albedo.png");
    assert_ne!(covered, plain);
    let levels = decoded_levels(&covered);
    let c0 = coverage(&levels[0].2, 3, CUTOFF);
    let c4 = coverage(&levels[4].2, 3, CUTOFF);
    assert!(
        (c4 - c0).abs() < 0.03,
        "16x16 after the project cook: {c4} vs {c0}"
    );

    let unchanged = cook();
    assert_eq!(
        (unchanged.cooked, unchanged.skipped),
        (0, 1),
        "no change, no recook"
    );
    let _ = std::fs::remove_dir_all(&root);
}
