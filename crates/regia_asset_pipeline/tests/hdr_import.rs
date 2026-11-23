// SPDX-License-Identifier: MPL-2.0
//! The HDR importer end to end through `cook_project` (ADR-2546): `.exr` and `.hdr` sources cook
//! to KTX2 (BC6H on the host, RGBA16F for Android), a NaN sample is a cook warning, and the
//! cooked file loads through the same reader the runtime uses.

use std::path::Path;

use regia_asset::hdr_texture::decode_hdr_ktx2;
use regia_asset::{AssetManifest, CookedTexture, cooked_cache_dir, cooked_file_name, decode_cooked_file};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};

fn exr(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 4]) -> Vec<u8> {
    let mut img = image::Rgba32FImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba(f(x, y));
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba32F(img)
        .write_to(&mut out, image::ImageFormat::OpenExr)
        .expect("exr");
    out.into_inner()
}

fn hdr(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::Rgb32FImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgb([1.0 + x as f32, 2.0 + y as f32, 4.0]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb32F(img)
        .write_to(&mut out, image::ImageFormat::Hdr)
        .expect("hdr");
    out.into_inner()
}

fn write_project(root: &Path) {
    let dir = root.join("assets/sky");
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::write(
        dir.join("day.exr"),
        exr(32, 16, |x, y| [1.0 + x as f32, 2.0 + y as f32, 3.0, 1.0]),
    )
    .expect("exr");
    std::fs::write(
        dir.join("bad.exr"),
        exr(8, 8, |x, y| {
            if (x, y) == (2, 3) {
                [f32::NAN, 1.0, 1.0, 1.0]
            } else {
                [1.0, 1.0, 1.0, 1.0]
            }
        }),
    )
    .expect("exr");
    std::fs::write(dir.join("dusk.hdr"), hdr(16, 8)).expect("hdr");
}

fn cook(root: &Path, platform: &str) -> CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: platform.to_string(),
        incremental: true,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

fn texture(root: &Path, platform: &str, rel: &str) -> (String, CookedTexture) {
    let cache = cooked_cache_dir(root, platform);
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a manifest");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == rel)
        .expect("manifest entry");
    let bytes = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
    let (_, payload) = decode_cooked_file(&bytes).expect("decode");
    (
        entry.type_path.clone(),
        CookedTexture::from_postcard(payload).expect("texture"),
    )
}

#[test]
fn exr_and_hdr_sources_cook_to_bc6h_and_a_nan_is_a_warning() {
    let root = regia_test_utils::unique_temp_dir("hdr-cook");
    write_project(&root);
    let report = cook(&root, host_platform());
    assert_eq!(report.cooked, 3, "{report:?}");
    let nan: Vec<&String> = report.warnings.iter().filter(|w| w.contains("NaN")).collect();
    assert_eq!(nan.len(), 1, "one warning, for bad.exr: {:?}", report.warnings);
    assert!(nan[0].contains("bad.exr") && nan[0].contains('1'), "{}", nan[0]);
    for (rel, w, h, mips) in [
        ("assets/sky/day.exr", 32, 16, 6),
        ("assets/sky/dusk.hdr", 16, 8, 5),
        ("assets/sky/bad.exr", 8, 8, 4),
    ] {
        let (type_path, tex) = texture(&root, host_platform(), rel);
        assert_eq!(type_path, "regia_asset::CookedTexture", "{rel}");
        assert_eq!((tex.width, tex.height), (w, h), "{rel}");
        let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
        assert_eq!(
            reader.header().format,
            Some(ktx2::Format::BC6H_UFLOAT_BLOCK),
            "{rel}"
        );
        assert_eq!(reader.levels().count(), mips, "{rel}");
        assert!(decode_hdr_ktx2(&tex.ktx2).is_ok(), "{rel}");
    }
    // The NaN texel reads as 0 (its block is encoded from the replaced value).
    let (_, tex) = texture(&root, host_platform(), "assets/sky/bad.exr");
    let level = decode_hdr_ktx2(&tex.ktx2).expect("decode");
    assert!(
        level.faces[0]
            .texels
            .iter()
            .all(|t| t.iter().all(|c| c.is_finite()))
    );
    // A second cook finds nothing to do.
    let again = cook(&root, host_platform());
    assert_eq!(again.cooked, 0, "{again:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_android_cook_stores_rgba16f() {
    let root = regia_test_utils::unique_temp_dir("hdr-android");
    write_project(&root);
    cook(&root, "android");
    let (_, tex) = texture(&root, "android", "assets/sky/day.exr");
    let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
    assert_eq!(reader.header().format, Some(ktx2::Format::R16G16B16A16_SFLOAT));
    let level = decode_hdr_ktx2(&tex.ktx2).expect("decode");
    // Texel (3, 2) of the source is (4, 4, 3) exactly representable in half precision.
    assert_eq!(level.faces[0].texels[2 * 32 + 3], [4.0, 4.0, 3.0]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Mutated sources and mutated cooked files never panic: the decoder and the CPU reader refuse them
/// with an error or read them, whichever they are.
#[test]
fn hostile_hdr_inputs_never_panic() {
    use regia_asset_pipeline::TextureImporterSettings;
    use regia_asset_pipeline::hdr_import::{cook_hdr_ktx2, decode_hdr_source};
    use regia_test_utils::fuzz::{FuzzRng, mutate_bytes};

    let source = exr(16, 8, |x, y| [1.0 + x as f32, 2.0 + y as f32, 3.0, 1.0]);
    let radiance = hdr(16, 8);
    let decoded = decode_hdr_source(&source, "exr").expect("fixture");
    let (cooked, _) = cook_hdr_ktx2(&decoded, &TextureImporterSettings::default(), "linux").expect("cook");
    let cooked = cooked.ktx2.to_vec();
    for seed in 1..=300u64 {
        let mut rng = FuzzRng::new(seed);
        let _ = decode_hdr_source(&mutate_bytes(&source, &mut rng), "exr");
        let _ = decode_hdr_source(&mutate_bytes(&radiance, &mut rng), "hdr");
        let _ = decode_hdr_ktx2(&mutate_bytes(&cooked, &mut rng));
    }
    assert!(decode_hdr_ktx2(&cooked).is_ok());
}
