// SPDX-License-Identifier: MPL-2.0
//! The project's Android texture policy end to end (ADR-2063, `specs/12` § 7.1):
//! `[cook.textures.android]` in `Regia.toml` switches the Android cook between ETC2 / EAC and
//! ASTC at a profile, recooking only when the policy changes; the cooked chains keep every mip
//! and decode on the runtime's CPU path (a device without the format) above a PSNR floor; a bad
//! table fails the cook; `texture_size_report` measures the same project against the BC path.

use image::ImageEncoder;
use regia_asset::{AssetServer, AssetServerSettings, CookedTexture};
use regia_asset_pipeline::texture_size_report::texture_size_report;
use regia_asset_pipeline::{AndroidTextureFormat, AstcProfile, CookOptions, TexturePolicy, cook_project};
use regia_render::compressed_texture::CompressedTexture;

/// A 64×64 smooth RGBA PNG with a per-file `seed` (1 to 9), and its pixels.
fn png(seed: u32) -> (Vec<u8>, Vec<u8>) {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            // Smooth ramps that never wrap (no hard edges): every value stays below 256.
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
    (out, pixels)
}

/// The images of the test project: colour and a normal map (by file-name suffix) and a
/// one-channel mask (by its `.meta`).
const IMAGES: [(&str, u32); 3] = [
    ("assets/rock_albedo.png", 1),
    ("assets/rock_normal.png", 2),
    ("assets/rock_ao.png", 3),
];

fn write_project(root: &std::path::Path, table: &str) {
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    for (rel, seed) in IMAGES {
        std::fs::write(root.join(rel), png(seed).0).expect("png");
    }
    write_meta(&root.join("assets/rock_ao.png"), "(usage: Some(mask))");
    set_table(root, table);
}

fn write_meta(image: &std::path::Path, settings: &str) {
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some(settings.into());
    std::fs::write(
        regia_asset::meta_path_for(image),
        meta.to_ron_string().expect("meta"),
    )
    .expect("meta");
}

fn set_table(root: &std::path::Path, table: &str) {
    std::fs::write(
        root.join("Regia.toml"),
        format!("[project]\nname = \"policy\"\nid = \"com.example.policy\"\n{table}"),
    )
    .expect("Regia.toml");
}

fn cook(root: &std::path::Path) -> regia_asset_pipeline::CookReport {
    cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: "android".into(),
        incremental: true,
    })
    .expect("cook")
}

/// The cooked texture of each test image, in [`IMAGES`] order.
fn cooked(root: &std::path::Path) -> Vec<CookedTexture> {
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "android".into(),
        content_dir: None,
    })
    .expect("server");
    IMAGES
        .iter()
        .map(|(rel, _)| {
            let id = server.id_for_path(rel).expect("in the manifest");
            let handle = server.load_texture(id).expect("a cooked texture");
            server.textures().get(&handle).expect("texture").clone()
        })
        .collect()
}

fn formats(textures: &[CookedTexture]) -> Vec<ktx2::Format> {
    textures
        .iter()
        .map(|t| {
            ktx2::Reader::new(&t.ktx2[..])
                .expect("ktx2")
                .header()
                .format
                .expect("format")
        })
        .collect()
}

#[test]
fn the_policy_switches_the_android_cook_and_the_cpu_path_decodes_it() {
    use ktx2::Format as F;
    let root = regia_test_utils::unique_temp_dir("android_texture_policy");
    write_project(&root, "[cook.textures.android]\nformat = \"etc2\"\n");

    let first = cook(&root);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let etc2 = cooked(&root);
    assert_eq!(
        formats(&etc2),
        [
            F::ETC2_R8G8B8_SRGB_BLOCK,
            F::EAC_R11G11_UNORM_BLOCK,
            F::EAC_R11_UNORM_BLOCK
        ]
    );
    // Mips preserved, and the CPU path of a device without ETC2 decodes every level; level 0
    // stays above the floor (EAC channels compared as stored).
    for ((texture, (_, seed)), channels) in etc2.iter().zip(IMAGES).zip([&[0usize, 1, 2][..], &[0, 1], &[0]])
    {
        let compressed = CompressedTexture::from_cooked(texture).expect("loads");
        assert_eq!(compressed.levels.len(), 7, "64 → 1 is seven levels");
        let chain = compressed.decode_chain_rgba8().expect("cpu decode");
        let sizes: Vec<u32> = chain.iter().map(|l| l.width).collect();
        assert_eq!(sizes, [64, 32, 16, 8, 4, 2, 1]);
        let source = png(seed).1;
        let db = psnr(&source, &chain[0].rgba, channels);
        assert!(db > 30.0, "{:?}: {db:.1} dB", compressed.format);
    }

    // Incremental: the same policy skips everything.
    let again = cook(&root);
    assert_eq!(again.cooked, 0, "unchanged policy, nothing recooks");

    // ASTC at the low profile: 6 does not divide 64, so colour and the mask take 8×8; normals 4×4.
    set_table(
        &root,
        "[cook.textures.android]\nformat = \"astc\"\nastc_profile = \"low\"\n",
    );
    let low = cook(&root);
    assert!(low.errors.is_empty(), "{:?}", low.errors);
    assert!(low.cooked >= IMAGES.len(), "a policy change recooks the textures");
    let astc_low = cooked(&root);
    assert_eq!(
        formats(&astc_low),
        [
            F::ASTC_8x8_SRGB_BLOCK,
            F::ASTC_4x4_UNORM_BLOCK,
            F::ASTC_8x8_UNORM_BLOCK
        ]
    );
    let albedo = CompressedTexture::from_cooked(&astc_low[0]).expect("loads");
    let chain = albedo.decode_chain_rgba8().expect("cpu decode of astc");
    assert_eq!(chain.len(), 7);
    let db = psnr(&png(1).1, &chain[0].rgba, &[0, 1, 2]);
    assert!(db > 30.0, "ASTC 8x8: {db:.1} dB");

    // No table: the default Android cook (ASTC 4×4 everywhere).
    set_table(&root, "");
    let default = cook(&root);
    assert!(default.errors.is_empty(), "{:?}", default.errors);
    assert_eq!(
        formats(&cooked(&root)),
        [
            F::ASTC_4x4_SRGB_BLOCK,
            F::ASTC_4x4_UNORM_BLOCK,
            F::ASTC_4x4_UNORM_BLOCK
        ]
    );

    // A bad table fails the cook before anything cooks.
    set_table(&root, "[cook.textures.android]\nformat = \"pvrtc\"\n");
    let bad = cook(&root);
    assert_eq!(bad.cooked, 0);
    assert!(
        bad.errors
            .iter()
            .any(|(path, e)| path == "Regia.toml" && e.contains("pvrtc")),
        "{:?}",
        bad.errors
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_size_report_compares_the_android_cook_with_the_bc_path() {
    let root = regia_test_utils::unique_temp_dir("texture_size_report");
    write_project(&root, "[cook.textures.android]\nformat = \"etc2\"\n");
    // An uncompressed image and a file that is not an image are listed as skipped.
    let ui = root.join("assets/hud.png");
    std::fs::write(&ui, png(9).0).expect("png");
    write_meta(&ui, "(compress: Some(false))");
    std::fs::write(root.join("assets/broken.png"), b"not a png").expect("broken");

    let report = texture_size_report(&root).expect("report");
    assert_eq!(report.schema, 1);
    assert_eq!(
        report.android_policy,
        TexturePolicy {
            android_format: AndroidTextureFormat::Etc2,
            astc_profile: AstcProfile::PerTexture,
        }
    );
    let paths: Vec<&str> = report.textures.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "assets/rock_albedo.png",
            "assets/rock_ao.png",
            "assets/rock_normal.png"
        ]
    );
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|s| (s.path.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(
        skipped.contains(&("assets/hud.png", "compress: false")),
        "{skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|(p, r)| *p == "assets/broken.png" && r.starts_with("does not decode")),
        "{skipped:?}"
    );
    let by_usage = |usage: &str| {
        report
            .textures
            .iter()
            .find(|t| t.usage == usage)
            .unwrap_or_else(|| panic!("{usage}"))
    };
    let colour = by_usage("color_srgb");
    assert_eq!(colour.bc.format, "BC7_SRGB");
    assert_eq!(colour.astc.format, "ASTC_4x4");
    assert_eq!(colour.etc2.format, "ETC2_R8G8B8_SRGB");
    assert_eq!(colour.etc2.bytes * 2, colour.bc.bytes, "ETC2 RGB8 is half of BC7");
    let normal = by_usage("normal_map");
    assert_eq!(
        (normal.bc.format.as_str(), normal.etc2.format.as_str()),
        ("BC5_UNORM", "EAC_R11G11_UNORM")
    );
    let mask = by_usage("mask");
    assert_eq!(mask.bc.format, "BC4_UNORM");
    for t in &report.textures {
        for c in [&t.bc, &t.astc, &t.etc2] {
            assert!(c.psnr_db > 30.0, "{} {}: {:.1} dB", t.path, c.format, c.psnr_db);
        }
    }
    let totals = &report.totals;
    assert_eq!(totals.android_bytes, totals.etc2_bytes, "the policy cooks ETC2");
    assert_eq!(
        totals.bc_bytes,
        report.textures.iter().map(|t| t.bc.bytes).sum::<u64>()
    );
    // Deterministic, and the text form carries every row and the totals.
    assert_eq!(texture_size_report(&root).expect("again"), report);
    let text = regia_asset_pipeline::texture_size_report::format_size_report(&report);
    assert!(text.contains("format etc2"), "{text}");
    assert!(
        text.contains("assets/rock_normal.png") && text.contains("% of BC"),
        "{text}"
    );
    // The report writes nothing: no `.meta` appears for an image without one.
    assert!(!regia_asset::meta_path_for(&root.join("assets/rock_albedo.png")).exists());
    std::fs::remove_dir_all(&root).ok();
}

/// A `.glb` with two embedded PNGs: a base colour and a normal map of one material, by slot (the
/// file names say nothing about usage).
fn glb_with_two_images() -> Vec<u8> {
    let (colour, normal) = (png(4).0, png(5).0);
    let mut bin = colour.clone();
    bin.extend_from_slice(&normal);
    bin.resize(bin.len().next_multiple_of(4), 0);
    let json = format!(
        concat!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":{bin}}}],"#,
            r#""bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{a}}},"#,
            r#"{{"buffer":0,"byteOffset":{a},"byteLength":{b}}}],"#,
            r#""images":[{{"bufferView":0,"mimeType":"image/png"}},{{"bufferView":1,"mimeType":"image/png"}}],"#,
            r#""textures":[{{"source":0}},{{"source":1}}],"#,
            r#""materials":[{{"pbrMetallicRoughness":{{"baseColorTexture":{{"index":0}}}},"#,
            r#""normalTexture":{{"index":1}}}}]}}"#
        ),
        bin = bin.len(),
        a = colour.len(),
        b = normal.len(),
    );
    let mut json = json.into_bytes();
    json.resize(json.len().next_multiple_of(4), b' ');
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

/// The report lists a glTF's images under the cook's sub-asset paths with the usage of the
/// material slot they fill, honours an external image's `.meta`, counts them in the totals, and
/// lists a file that does not parse as skipped. It writes no `.meta` for the glTF.
#[test]
fn the_size_report_measures_the_images_of_gltf_files() {
    let root = regia_test_utils::unique_temp_dir("gltf_size_report");
    std::fs::create_dir_all(root.join("assets/models")).expect("assets");
    set_table(&root, "");
    std::fs::write(root.join("assets/models/a.glb"), glb_with_two_images()).expect("glb");
    std::fs::write(root.join("assets/models/broken.glb"), b"not a glb").expect("broken");
    // A `.gltf` with one external image kept uncompressed by its `.meta`. The image lies under
    // `assets/` too, so the standalone pass lists it as well (skipped for the same reason).
    std::fs::write(
        root.join("assets/models/b.gltf"),
        r#"{"asset":{"version":"2.0"},"images":[{"uri":"hud.png"}]}"#,
    )
    .expect("gltf");
    let hud = root.join("assets/models/hud.png");
    std::fs::write(&hud, png(6).0).expect("png");
    write_meta(&hud, "(compress: Some(false))");

    let report = texture_size_report(&root).expect("report");
    let paths: Vec<&str> = report.textures.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(
        paths,
        ["assets/models/a.glb#texture/0", "assets/models/a.glb#texture/1"]
    );
    assert_eq!(report.textures[0].usage, "color_srgb");
    assert_eq!(report.textures[0].bc.format, "BC7_SRGB");
    assert_eq!(report.textures[0].astc.format, "ASTC_4x4");
    assert_eq!(report.textures[1].usage, "normal_map");
    assert_eq!(report.textures[1].bc.format, "BC5_UNORM");
    assert_eq!(report.textures[1].etc2.format, "EAC_R11G11_UNORM");
    for t in &report.textures {
        for c in [&t.bc, &t.astc, &t.etc2] {
            assert!(c.psnr_db > 30.0, "{} {}: {:.1} dB", t.path, c.format, c.psnr_db);
        }
    }
    assert_eq!(
        report.totals.bc_bytes,
        report.textures[0].bc.bytes + report.textures[1].bc.bytes
    );
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|s| (s.path.as_str(), s.reason.as_str()))
        .collect();
    assert!(
        skipped.contains(&("assets/models/b.gltf#texture/0", "compress: false")),
        "{skipped:?}"
    );
    assert!(
        skipped.contains(&("assets/models/hud.png", "compress: false")),
        "{skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|(p, r)| *p == "assets/models/broken.glb" && r.starts_with("does not decode")),
        "{skipped:?}"
    );
    assert_eq!(skipped.len(), 3, "{skipped:?}");
    assert_eq!(texture_size_report(&root).expect("again"), report);
    assert!(!regia_asset::meta_path_for(&root.join("assets/models/a.glb")).exists());
    std::fs::remove_dir_all(&root).ok();
}

/// PSNR (dB) over the listed channels (same index in both images).
fn psnr(source: &[u8], decoded: &[u8], channels: &[usize]) -> f64 {
    let mut sum = 0f64;
    let mut n = 0f64;
    for (s, d) in source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
        for &c in channels {
            let e = f64::from(s[c]) - f64::from(d[c]);
            sum += e * e;
            n += 1.0;
        }
    }
    if sum == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
}
