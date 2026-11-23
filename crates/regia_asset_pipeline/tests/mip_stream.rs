// SPDX-License-Identifier: MPL-2.0
//! Mip streaming metadata end to end (T2.14, ADR-1942): the cook writes it, a pak keeps it, the
//! renderer's texture reader and the cooked-pack report see it.

use regia_asset::cooked_pack::summarize_pak;
use regia_asset::{
    AssetId, AssetManifest, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedTexture, ManifestEntry,
    MipStreamMeta, PakPayload, PakReader, TextureCookUsage, cooked_file_name, encode_cooked_file,
};
use regia_asset_pipeline::astc::AstcContent;
use regia_asset_pipeline::mip_stream::{MIN_DETAIL_PX, cooked_mip_stream};
use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
use regia_asset_pipeline::{
    TextureCookQuality, TextureImporterSettings, cook_texture_rgba8, transcode_bcn_ktx2_to_astc,
};
use regia_render::CompressedTexture;
use regia_test_utils::unique_temp_dir;

fn flat(size: u32) -> Vec<u8> {
    vec![90; (size * size * 4) as usize]
}

fn noise(size: u32) -> Vec<u8> {
    (0..size * size * 4)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect()
}

fn cook(rgba: &[u8], size: u32, platform: &str) -> CookedTexture {
    cook_texture_rgba8(
        size,
        size,
        rgba,
        TextureCookUsage::ColorSrgb,
        platform,
        TextureCookQuality::Low,
    )
    .expect("cook")
}

fn file(id: AssetId, texture: &CookedTexture) -> Vec<u8> {
    let payload = texture.to_postcard().expect("postcard");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::texture().0,
        asset_id: id,
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).expect("encode")
}

#[test]
fn the_renderer_reads_what_the_cook_wrote() {
    for platform in ["linux", "android"] {
        let cooked = cook(&flat(64), 64, platform);
        let texture = CompressedTexture::from_cooked(&cooked).expect("reads");
        assert_eq!(
            texture.mip_stream,
            Some(MipStreamMeta::new(MIN_DETAIL_PX)),
            "{platform}"
        );
        let reread =
            CookedTexture::from_postcard(&cooked.to_postcard().expect("postcard")).expect("frozen shape");
        assert_eq!(reread, cooked, "{platform}");
    }
    let detailed = cook(&noise(64), 64, "linux");
    let texture = CompressedTexture::from_cooked(&detailed).expect("reads");
    assert_eq!(texture.mip_stream, Some(MipStreamMeta::new(64)));
}

#[test]
fn a_cook_is_repeatable_byte_for_byte() {
    assert_eq!(cook(&noise(32), 32, "linux"), cook(&noise(32), 32, "linux"));
}

#[test]
fn the_transcode_to_astc_keeps_the_metadata() {
    let cooked = cook(&flat(64), 64, "linux");
    let astc = transcode_bcn_ktx2_to_astc(&cooked.ktx2, TextureCookQuality::Low, AstcContent::Normal)
        .expect("transcode");
    let reader = ktx2::Reader::new(&astc[..]).expect("reads");
    assert_eq!(
        MipStreamMeta::from_reader(&reader),
        Some(MipStreamMeta::new(MIN_DETAIL_PX))
    );
}

#[test]
fn the_pack_report_counts_it() {
    let dir = unique_temp_dir("mip-stream-pak");
    let cooked_dir = dir.join("cooked");
    std::fs::create_dir_all(&cooked_dir).expect("cooked dir");
    let settings = TextureImporterSettings {
        stream: Some(false),
        ..TextureImporterSettings::default()
    };
    let older = {
        let plain = regia_asset_pipeline::write_ktx2_bcn(
            4,
            4,
            regia_asset_pipeline::BcnFormat::Bc7Srgb,
            &[vec![0u8; 16]],
        )
        .expect("ktx2");
        CookedTexture {
            width: 4,
            height: 4,
            usage: TextureCookUsage::ColorSrgb,
            ktx2: plain.into(),
        }
    };
    assert_eq!(cooked_mip_stream(&older), None);
    let textures = [
        cook(&flat(64), 64, "linux"),
        cook(&noise(64), 64, "linux"),
        settings
            .apply_stream(cook(&noise(32), 32, "linux"))
            .expect("stamp"),
        older,
    ];
    let mut entries = Vec::new();
    let mut manifest = Vec::new();
    for (i, texture) in textures.iter().enumerate() {
        let id = AssetId::new_random();
        std::fs::write(cooked_dir.join(cooked_file_name(id)), file(id, texture)).expect("write");
        entries.push((id, AssetTypeTag::texture().0));
        manifest.push(ManifestEntry {
            path: format!("assets/t{i}.png"),
            id,
            type_path: "regia_asset::CookedTexture".to_owned(),
        });
    }
    let pak = dir.join("textures.fpak");
    write_pak(
        &pak,
        &cooked_dir,
        &entries,
        &AssetManifest { entries: manifest },
        &PakWriteOptions::default(),
    )
    .expect("pak");
    let reader = std::sync::Arc::new(PakReader::open(&pak).expect("open"));
    let summary = summarize_pak(&reader, "textures", "linux", true);
    assert_eq!(summary.textures, 4);
    assert_eq!(summary.unreadable, 0);
    assert_eq!(summary.mip_metadata_textures, 3, "the older cook has none");
    assert_eq!(summary.mip_capped_textures, 1, "only the flat one");
    assert_eq!(summary.unstreamed_textures, 1);

    let quick = summarize_pak(&reader, "textures", "linux", false);
    assert_eq!(
        quick.mip_metadata_textures, 0,
        "reading the table of contents opens no texture"
    );

    let form_of = |index: usize| {
        regia_asset::cooked_pack::texture_form(&PakPayload::from(file(
            AssetId::new_random(),
            &textures[index],
        )))
        .expect("form")
    };
    assert_eq!(form_of(0).full_px, 64);
    assert_eq!(form_of(0).mip_stream, Some(MipStreamMeta::new(MIN_DETAIL_PX)));
    assert_eq!(form_of(3).mip_stream, None);
}
