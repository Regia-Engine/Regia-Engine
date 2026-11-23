// SPDX-License-Identifier: MPL-2.0
//! Tier packaging preserves an authored DDS chain that cannot meet the cap.

use std::io::Write;
use std::sync::{Arc, Mutex};

use regia_asset::{AssetId, AssetManifest, AssetTypeTag, CookedTexture, ManifestEntry, PakReader};
use regia_asset_pipeline::chunks::{Assignment, ChunkRules, ChunkWriteOptions, write_chunks};
use regia_asset_pipeline::{BcnFormat, write_ktx2_bcn};

#[derive(Clone, Default)]
struct WarningBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for WarningBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn cooked(id: AssetId, tag: AssetTypeTag, payload: &[u8], dependencies: Vec<AssetId>) -> Vec<u8> {
    regia_asset::encode_cooked_file(
        &regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: tag.0,
            asset_id: id,
            importer_version: 1000000,
            dependencies,
            payload_len: payload.len() as u64,
        },
        payload,
    )
    .expect("cooked wrapper")
}

#[test]
fn a_single_level_2048_dds_stays_verbatim_beside_a_reduced_4096_chain_and_warns_once() {
    let scratch = regia_test_utils::TempDir::new("tier-dds-tail");
    let cache = scratch.join("cache");
    let out = scratch.join("paks");
    std::fs::create_dir_all(&cache).expect("cache");
    // An authored one-level BC1 DDS, parsed by the real DDS loader. Pack its authored
    // levels directly: this tests existing cached chains without synthesizing new mips.
    let mut dds = vec![0; 128];
    dds[..4].copy_from_slice(b"DDS ");
    for (at, value) in [
        (4, 124u32),
        (8, 0x1007),
        (12, 2048),
        (16, 2048),
        (28, 1),
        (76, 32),
        (80, 4),
        (108, 0x1000),
    ] {
        dds[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    dds[84..88].copy_from_slice(b"DXT1");
    dds.extend_from_slice(&[0xff, 0xff, 0, 0, 0, 0, 0, 0].repeat(512 * 512));
    let dds = regia_asset_pipeline::dds_import::parse_dds(&dds).expect("DDS");
    assert_eq!(dds.levels.len(), 1);
    let short = AssetId::new_random();
    let large = AssetId::new_random();
    let material = AssetId::new_random();
    let mut manifest = AssetManifest::default();
    let mut assignment = Assignment::default();
    let mut originals = Vec::new();
    for (id, name, edge, ktx2) in [
        (
            short,
            "short.dds",
            2048,
            write_ktx2_bcn(dds.width, dds.height, BcnFormat::Bc1Srgb, &dds.levels).expect("authored chain"),
        ),
        (
            large,
            "large.dds",
            4096,
            write_ktx2_bcn(
                4096,
                4096,
                BcnFormat::Bc7Srgb,
                &(0..13)
                    .map(|level| {
                        vec![level as u8; (4096u32 >> level).max(1).div_ceil(4).pow(2) as usize * 16]
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("full chain"),
        ),
    ] {
        let texture = CookedTexture {
            width: edge,
            height: edge,
            usage: regia_asset::TextureCookUsage::ColorSrgb,
            ktx2: ktx2.into(),
        };
        let bytes = cooked(
            id,
            AssetTypeTag::texture(),
            &texture.to_postcard().expect("texture"),
            vec![],
        );
        std::fs::write(cache.join(regia_asset::cooked_file_name(id)), &bytes).expect("cache file");
        manifest.entries.push(ManifestEntry {
            path: format!("assets/{name}"),
            id,
            type_path: "regia_asset::CookedTexture".into(),
        });
        assignment.chunk_of.insert(id, "textures".into());
        assignment.duplicates.insert(id, vec!["duplicate".into()]);
        originals.push(bytes);
    }
    let mut authored = regia_asset::MaterialAssetRon::default_standard();
    authored.base.base_color_map = Some(regia_asset::TextureSlot::Asset(short));
    authored.base.emissive_map = Some(regia_asset::TextureSlot::Asset(large));
    let payload = regia_asset::CookedStandardMaterial::new(authored)
        .to_postcard()
        .expect("material");
    std::fs::write(
        cache.join(regia_asset::cooked_file_name(material)),
        cooked(
            material,
            AssetTypeTag::standard_material(),
            &payload,
            vec![short, large],
        ),
    )
    .expect("material file");
    manifest.entries.push(ManifestEntry {
        path: "assets/material.material.ron".into(),
        id: material,
        type_path: "regia_asset::CookedStandardMaterial".into(),
    });
    assignment.chunk_of.insert(material, "base".into());
    manifest.write_to_cache(&cache).expect("manifest");
    write_chunks(
        &cache,
        &out,
        &assignment,
        &ChunkRules::default(),
        &ChunkWriteOptions {
            compress: false,
            build_id: "tier-dds-tail".into(),
            platform: "linux".into(),
            ..Default::default()
        },
    )
    .expect("full paks");
    let warnings = WarningBuffer::default();
    let writer = warnings.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        regia_asset_pipeline::texture_tiers::write_texture_tier_paks(
            &out,
            &[
                "LOW=1024".parse().expect("low"),
                "TINY=512".parse().expect("tiny"),
            ],
            false,
        )
        .expect("a missing tail must not abort the tier");
    });
    let log = String::from_utf8(warnings.0.lock().expect("warnings").clone()).expect("UTF-8");
    assert_eq!(log.matches("texture_tier_missing_mip").count(), 1, "{log}");
    assert!(log.contains(&short.to_string()), "warning names the asset: {log}");
    for (tier, cap) in [("LOW", 1024), ("TINY", 512)] {
        for chunk in ["textures", "duplicate"] {
            let pak =
                PakReader::open(&out.join(format!("texture_tiers/{tier}/{chunk}.fpak"))).expect("tier pak");
            assert!(pak.read_decompressed(short).expect("short") == originals[0]);
            let (_, full) = regia_asset::load_texture_bytes(&originals[1]).expect("original");
            let (_, reduced) = regia_asset::load_texture_bytes(&pak.read_decompressed(large).expect("large"))
                .expect("reduced");
            assert_eq!((reduced.width, reduced.height), (cap, cap));
            let skip = if cap == 1024 { 2 } else { 3 };
            assert_eq!(
                regia_render::CompressedTexture::from_cooked(&reduced)
                    .expect("runtime")
                    .levels,
                regia_render::CompressedTexture::from_cooked(&full)
                    .expect("full")
                    .levels[skip..]
            );
        }
    }
    assert!(std::fs::read(cache.join(regia_asset::cooked_file_name(short))).expect("cache") == originals[0]);
    let full = PakReader::open(&out.join("textures.fpak")).expect("ordinary pak");
    assert!(full.read_decompressed(short).expect("ordinary short") == originals[0]);
    assert!(full.read_decompressed(large).expect("ordinary large") == originals[1]);
}
