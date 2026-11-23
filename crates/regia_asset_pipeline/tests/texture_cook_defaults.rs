// SPDX-License-Identifier: MPL-2.0
//! Texture cook defaults (lane tex-budget, `specs/12` § 7.1): a standalone PNG cooks
//! block-compressed with a full mip chain by default — BC7 for colour and data, BC5 for a
//! normal map named so — and its `.meta` overrides the usage, the size and the compression.

use image::ImageEncoder;
use regia_asset::{AssetServer, AssetServerSettings, MetaSidecar};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

/// KTX2 `vkFormat` (the u32 after the 12-byte identifier, KTX 2.0 spec § 3.1).
fn vk_format(ktx2: &[u8]) -> u32 {
    u32::from_le_bytes([ktx2[12], ktx2[13], ktx2[14], ktx2[15]])
}

/// KTX2 `levelCount` (offset 40).
fn level_count(ktx2: &[u8]) -> u32 {
    u32::from_le_bytes([ktx2[40], ktx2[41], ktx2[42], ktx2[43]])
}

const VK_FORMAT_BC5_UNORM_BLOCK: u32 = 141;
const VK_FORMAT_BC7_UNORM_BLOCK: u32 = 145;
const VK_FORMAT_BC7_SRGB_BLOCK: u32 = 146;

fn png(size: u32, seed: u8) -> Vec<u8> {
    let pixels: Vec<u8> = (0..size * size)
        .flat_map(|i| [(i as u8).wrapping_mul(seed), 128, 255 - i as u8, 255])
        .collect();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, size, size, image::ExtendedColorType::Rgba8)
        .expect("encode png");
    out
}

fn meta(path: &std::path::Path, settings: &str) {
    let mut meta = MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some(settings.to_string());
    std::fs::write(
        regia_asset::meta_path_for(path),
        meta.to_ron_string().expect("meta ron"),
    )
    .expect("write meta");
}

#[test]
fn pngs_cook_to_bc7_and_bc5_with_mips_and_meta_overrides() {
    if host_platform() == "android" {
        return;
    }
    let root = std::env::temp_dir().join(format!("regia_tex_cook_{}", uuid::Uuid::new_v4()));
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    std::fs::write(assets.join("hull_albedo.png"), png(64, 3)).expect("albedo");
    std::fs::write(assets.join("hull_normal.png"), png(64, 5)).expect("normal");
    std::fs::write(assets.join("hull_orm.png"), png(64, 7)).expect("orm");
    // Overrides: a 64² map capped at 16, a normal map named like colour, pixel art kept RGBA8.
    let capped = assets.join("terrain_detail.png");
    std::fs::write(&capped, png(64, 9)).expect("capped");
    meta(&capped, "(max_size: Some(16))");
    let renamed = assets.join("wing_detail.png");
    std::fs::write(&renamed, png(64, 11)).expect("renamed");
    meta(&renamed, "(usage: Some(normal))");
    let pixel_art = assets.join("hud_icon.png");
    std::fs::write(&pixel_art, png(16, 13)).expect("icon");
    meta(&pixel_art, "(compress: Some(false))");

    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    let mut texture = |rel: &str| {
        let id = server.id_for_path(rel).expect("in the manifest");
        let handle = server.load_texture(id).expect("a cooked texture");
        server.textures().get(&handle).expect("texture").clone()
    };
    let albedo = texture("assets/hull_albedo.png");
    assert_eq!(vk_format(&albedo.ktx2), VK_FORMAT_BC7_SRGB_BLOCK);
    assert_eq!(level_count(&albedo.ktx2), 7, "64² → 1² is a full chain");
    let normal = texture("assets/hull_normal.png");
    assert_eq!(vk_format(&normal.ktx2), VK_FORMAT_BC5_UNORM_BLOCK);
    let orm = texture("assets/hull_orm.png");
    assert_eq!(vk_format(&orm.ktx2), VK_FORMAT_BC7_UNORM_BLOCK, "linear data");
    let capped = texture("assets/terrain_detail.png");
    assert_eq!((capped.width, capped.height), (16, 16));
    assert_eq!(vk_format(&capped.ktx2), VK_FORMAT_BC7_SRGB_BLOCK);
    let renamed = texture("assets/wing_detail.png");
    assert_eq!(vk_format(&renamed.ktx2), VK_FORMAT_BC5_UNORM_BLOCK);
    let id = server.id_for_path("assets/hud_icon.png").expect("icon id");
    assert!(
        server.load_texture(id).is_err(),
        "compress: false is not a CookedTexture"
    );
    let handle = server.load_image(id).expect("an RGBA8 image");
    let icon = server.images().get(&handle).expect("image");
    assert_eq!((icon.width, icon.height, icon.rgba8.len()), (16, 16, 16 * 16 * 4));
    let _ = std::fs::remove_dir_all(&root);
}
