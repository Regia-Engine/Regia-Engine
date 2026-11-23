// SPDX-License-Identifier: MPL-2.0
//! The project's `[cook] texture_quality` end to end (`docs/specs/21-tooling-and-packaging.md`
//! § 2, ADR-7217): `"final"` (the default) cooks textures at the high quality (BC7 for an opaque
//! sRGB albedo), `"draft"` at the low one (BC1); switching recooks; a texture's own `.meta`
//! `quality` still wins; an unknown value fails the cook with a message naming the key.

use image::ImageEncoder;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::{CookOptions, cook_project};

/// A 64×64 smooth opaque RGBA PNG.
fn png(seed: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&[
                (x * 3 + seed * 10) as u8,
                (y * 3 + seed * 5) as u8,
                ((x + y) * 3 / 2 + seed * 7) as u8,
                255,
            ]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

const ALBEDO: &str = "assets/rock_albedo.png";
const PINNED: &str = "assets/sign_albedo.png";

fn set_manifest(root: &std::path::Path, cook_table: &str) {
    std::fs::write(
        root.join("Regia.toml"),
        format!("[project]\nname = \"quality\"\nid = \"com.example.quality\"\n{cook_table}"),
    )
    .expect("Regia.toml");
}

fn write_project(root: &std::path::Path, cook_table: &str) {
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join(ALBEDO), png(1)).expect("png");
    std::fs::write(root.join(PINNED), png(2)).expect("png");
    // This texture pins its own quality: the project setting must not override it.
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some("(quality: Some(high))".into());
    std::fs::write(
        regia_asset::meta_path_for(&root.join(PINNED)),
        meta.to_ron_string().expect("meta"),
    )
    .expect("meta");
    set_manifest(root, cook_table);
}

fn cook(root: &std::path::Path) -> regia_asset_pipeline::CookReport {
    cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        incremental: true,
    })
    .expect("cook")
}

/// The KTX2 format of each cooked test image, `[ALBEDO, PINNED]`.
fn formats(root: &std::path::Path) -> Vec<ktx2::Format> {
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    [ALBEDO, PINNED]
        .iter()
        .map(|rel| {
            let id = server.id_for_path(rel).expect("in the manifest");
            let handle = server.load_texture(id).expect("a cooked texture");
            let texture = server.textures().get(&handle).expect("texture").clone();
            ktx2::Reader::new(&texture.ktx2[..])
                .expect("ktx2")
                .header()
                .format
                .expect("format")
        })
        .collect()
}

#[test]
fn draft_cooks_textures_at_the_low_quality_and_final_at_the_high_one() {
    use ktx2::Format as F;
    let root = regia_test_utils::unique_temp_dir("cook_texture_quality");
    write_project(&root, "[cook]\ntexture_quality = \"final\"\n");

    let first = cook(&root);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let final_formats = formats(&root);
    assert_eq!(
        final_formats,
        [F::BC7_SRGB_BLOCK, F::BC7_SRGB_BLOCK],
        "final = high (BC7)"
    );

    // Draft: the albedo recooks at the low quality (BC1); the pinned texture keeps its `.meta`.
    set_manifest(&root, "[cook]\ntexture_quality = \"draft\"\n");
    let draft = cook(&root);
    assert!(draft.errors.is_empty(), "{:?}", draft.errors);
    let draft_formats = formats(&root);
    assert_ne!(
        draft_formats[0], final_formats[0],
        "texture_quality = \"draft\" must change the cooked texture settings"
    );
    assert!(
        matches!(draft_formats[0], F::BC1_RGB_SRGB_BLOCK | F::BC1_RGBA_SRGB_BLOCK),
        "draft albedo: {:?}",
        draft_formats[0]
    );
    assert_eq!(
        draft_formats[1],
        F::BC7_SRGB_BLOCK,
        "a .meta quality wins over the project's"
    );

    // Back to final (the key's default spelling: no key at all) recooks at the high quality.
    set_manifest(&root, "");
    let back = cook(&root);
    assert!(back.errors.is_empty(), "{:?}", back.errors);
    assert_eq!(formats(&root), final_formats);
}

#[test]
fn an_unknown_texture_quality_fails_the_cook_naming_the_key() {
    let root = regia_test_utils::unique_temp_dir("cook_texture_quality_bad");
    write_project(&root, "[cook]\ntexture_quality = \"ultra\"\n");
    let report = cook(&root);
    assert!(
        report.errors.iter().any(|(file, err)| file == "Regia.toml"
            && err.contains("texture_quality")
            && err.contains("ultra")),
        "{:?}",
        report.errors
    );
}
