// SPDX-License-Identifier: MPL-2.0
//! Texture importer settings edge cases: the box-filter fit on odd and degenerate sizes, the cache
//! fingerprint, file-name usage rules and `.meta` parsing. Pure CPU, no clock, no goldens.

use std::path::Path;

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::{
    GenerateLodsSettings, GltfImporterSettings, TextureCookQuality, TextureImporterSettings,
    TextureQualitySetting, TextureUsageSetting, usage_from_file_name,
};

fn capped(max: u32) -> TextureImporterSettings {
    TextureImporterSettings {
        max_size: Some(max),
        ..TextureImporterSettings::default()
    }
}

#[test]
fn fit_rounds_the_box_average_to_nearest() {
    // One 2x2 block, red channel 0, 0, 0, 3: sum 3, (3 + 2) / 4 = 1.
    let rgba = vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 3, 0, 0, 255];
    let (w, h, out) = capped(1).fit(2, 2, rgba);
    assert_eq!((w, h), (1, 1));
    assert_eq!(out, vec![1, 0, 0, 255]);
    // 255 everywhere stays 255 (no overflow in the u8 cast).
    let (_, _, out) = capped(1).fit(2, 2, vec![255; 16]);
    assert_eq!(out, vec![255; 4]);
}

#[test]
fn fit_reaches_the_cap_for_odd_and_one_pixel_edges() {
    for (w, h, max, expect) in [
        (5u32, 3u32, 2u32, (2u32, 1u32)),
        (7, 7, 3, (3, 3)),
        (1, 64, 8, (1, 8)),
        (64, 1, 8, (8, 1)),
        (3, 1, 1, (1, 1)),
        (4, 4, 1, (1, 1)),
    ] {
        let rgba = vec![200u8; (w * h * 4) as usize];
        let (ow, oh, out) = capped(max).fit(w, h, rgba);
        assert_eq!((ow, oh), expect, "{w}x{h} cap {max}");
        assert_eq!(out.len(), (ow * oh * 4) as usize, "{w}x{h} cap {max}");
        assert!(out.iter().all(|&b| b == 200), "a flat image stays flat");
    }
}

#[test]
fn fit_ignores_a_zero_cap_and_never_loops_on_a_one_pixel_image() {
    let rgba: Vec<u8> = (0..16).collect();
    assert_eq!(capped(0).fit(2, 2, rgba.clone()), (2, 2, rgba.clone()));
    assert_eq!(capped(1).fit(1, 1, vec![9, 8, 7, 6]), (1, 1, vec![9, 8, 7, 6]));
    // A cap of exactly the largest edge changes nothing.
    assert_eq!(capped(2).fit(2, 2, rgba.clone()), (2, 2, rgba));
}

#[test]
fn fit_tolerates_a_short_buffer_without_panicking() {
    let (w, h, out) = capped(1).fit(4, 4, vec![255; 8]);
    assert_eq!((w, h), (1, 1));
    assert_eq!(out.len(), 4);
}

#[test]
fn fit_is_deterministic() {
    let rgba: Vec<u8> = (0..16 * 16 * 4).map(|i| (i * 37 % 251) as u8).collect();
    let a = capped(4).fit(16, 16, rgba.clone());
    let b = capped(4).fit(16, 16, rgba);
    assert_eq!(a, b);
}

#[test]
fn every_setting_changes_the_fingerprint() {
    let base = TextureImporterSettings::default();
    let variants = [
        TextureImporterSettings {
            usage: Some(TextureUsageSetting::Color),
            ..base.clone()
        },
        TextureImporterSettings {
            usage: Some(TextureUsageSetting::Ui),
            ..base.clone()
        },
        TextureImporterSettings {
            max_size: Some(512),
            ..base.clone()
        },
        TextureImporterSettings {
            max_size: Some(1024),
            ..base.clone()
        },
        TextureImporterSettings {
            compress: Some(false),
            ..base.clone()
        },
        TextureImporterSettings {
            compress: Some(true),
            ..base.clone()
        },
        TextureImporterSettings {
            quality: Some(TextureQualitySetting::Low),
            ..base.clone()
        },
        TextureImporterSettings {
            quality: Some(TextureQualitySetting::High),
            ..base.clone()
        },
    ];
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(base.fingerprint());
    for v in &variants {
        assert!(seen.insert(v.fingerprint()), "collision for {v:?}");
    }
    assert_eq!(
        base.fingerprint(),
        TextureImporterSettings::default().fingerprint()
    );
}

#[test]
fn explicit_defaults_are_equivalent_to_unset_for_cooking() {
    let explicit =
        TextureImporterSettings::from_meta_string(Some("(compress: Some(true), quality: Some(high))"));
    assert!(explicit.compress());
    assert_eq!(
        explicit.quality_or(TextureCookQuality::Low),
        TextureCookQuality::High
    );
}

#[test]
fn every_usage_setting_maps_to_its_cook_usage() {
    for (setting, usage) in [
        ("color", TextureCookUsage::ColorSrgb),
        ("linear", TextureCookUsage::ColorLinear),
        ("normal", TextureCookUsage::NormalMap),
        ("mask", TextureCookUsage::Mask),
        ("ui", TextureCookUsage::Ui),
    ] {
        let s = TextureImporterSettings::from_meta_string(Some(&format!("(usage: Some({setting}))")));
        assert_eq!(s.usage_or(TextureCookUsage::ColorSrgb), usage, "{setting}");
    }
}

#[test]
fn bad_or_odd_meta_strings_fall_back_per_string_not_per_field() {
    let defaults = TextureImporterSettings::default();
    for text in [
        "",
        "   \n\t",
        "()",
        "(max_size: Some(-3))",
        "(usage: Some(shiny))",
        "(max_size: Some(1.5))",
        "garbage",
    ] {
        let parsed = TextureImporterSettings::from_meta_string(Some(text));
        assert_eq!(parsed, defaults, "{text:?}");
    }
    // A bad field discards the good ones beside it: the whole string is the defaults.
    let mixed = TextureImporterSettings::from_meta_string(Some("(max_size: Some(64), usage: Some(shiny))"));
    assert_eq!(mixed, defaults);
    // Unknown keys (the glTF importer's) beside valid ones are ignored.
    let extra = TextureImporterSettings::from_meta_string(Some("(max_size: Some(64), meshlets: true)"));
    assert_eq!(extra.max_size, Some(64));
}

#[test]
fn file_name_usage_edge_cases() {
    let u = |n: &str| usage_from_file_name(Path::new(n));
    assert_eq!(u(""), TextureCookUsage::ColorSrgb);
    assert_eq!(u("___.png"), TextureCookUsage::ColorSrgb);
    assert_eq!(u("a__n.png"), TextureCookUsage::NormalMap);
    assert_eq!(
        u("dir.d/hull_N"),
        TextureCookUsage::NormalMap,
        "directories never count"
    );
    assert_eq!(
        u("normal_albedo.png"),
        TextureCookUsage::ColorSrgb,
        "the last word decides"
    );
    assert_eq!(u("albedo_normal.png"), TextureCookUsage::NormalMap);
    assert_eq!(u("Hull.NORMAL.PNG"), TextureCookUsage::NormalMap);
    assert_eq!(u("hull.normal.v2.png"), TextureCookUsage::ColorSrgb);
    assert_eq!(u("héros_ao.png"), TextureCookUsage::ColorLinear);
    assert_eq!(u("日本_n.png"), TextureCookUsage::NormalMap);
    for linear in [
        "x_curvature.png",
        "x_cavity.png",
        "x_height.png",
        "x_metallicroughness.png",
        "x_opacity.png",
    ] {
        assert_eq!(u(linear), TextureCookUsage::ColorLinear, "{linear}");
    }
}

#[test]
fn gltf_settings_clamp_lod_counts_and_keep_ratios() {
    let none = GltfImporterSettings::default();
    assert!(none.mesh_lods().is_none());
    for (count, expect) in [(0u32, 1u32), (1, 1), (3, 3), (4, 4), (5, 4), (u32::MAX, 4)] {
        let s = GltfImporterSettings {
            generate_lods: Some(GenerateLodsSettings {
                count,
                ratios: vec![0.5, 0.25],
            }),
            ..GltfImporterSettings::default()
        };
        let lods = s.mesh_lods().expect("lods");
        assert_eq!(lods.count, expect, "count {count}");
        assert_eq!(lods.ratios, vec![0.5, 0.25]);
    }
    let parsed = GltfImporterSettings::from_meta_string(Some("(generate_lods: Some((count: 2)))"));
    let lods = parsed.mesh_lods().expect("lods");
    assert_eq!((lods.count, lods.ratios.len()), (2, 0), "ratios default to empty");
}
