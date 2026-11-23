// SPDX-License-Identifier: MPL-2.0
//! The SVG importer end to end through `cook_project` (ADR-3350): an `.svg` cooks to a texture of the
//! size its `.meta` asks for, two cooks of the same source give the same bytes, and a document the
//! importer refuses is a cook error that names the error code.

use std::path::Path;

use regia_asset::{
    AssetId, AssetManifest, CookedImage, MetaSidecar, cooked_cache_dir, cooked_file_name, decode_cooked_file,
};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};

const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" viewBox="0 0 8 8">
<rect x="2" y="2" width="4" height="4" fill="#3366ff"/></svg>"##;

fn write_meta(svg: &Path, settings: &str) {
    let mut meta = MetaSidecar::new("svg", AssetId::new_random());
    meta.settings = Some(settings.to_string());
    let text = meta.to_ron_string().expect("meta");
    std::fs::write(regia_asset::meta_path_for(svg), text).expect("write meta");
}

fn project(name: &str, settings: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(name);
    let dir = root.join("assets/ui");
    std::fs::create_dir_all(&dir).expect("dirs");
    let svg = dir.join("icon.svg");
    std::fs::write(&svg, ICON).expect("svg");
    write_meta(&svg, settings);
    root
}

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: host_platform().to_string(),
        incremental: true,
    })
    .expect("cook")
}

fn payload(root: &Path, rel: &str) -> (String, Vec<u8>) {
    let cache = cooked_cache_dir(root, host_platform());
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
    (entry.type_path.clone(), payload.to_vec())
}

#[test]
fn an_svg_cooks_to_the_requested_size_with_known_pixels_and_the_same_bytes_twice() {
    // `compress: false` keeps RGBA8 so the pixels can be read back.
    let settings = "(sizes_px: [32], compress: Some(false))";
    let a = project("svg-cook-a", settings);
    let b = project("svg-cook-b", settings);
    for root in [&a, &b] {
        let report = cook(root);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.cooked, 1, "{report:?}");
    }
    let (type_path, bytes_a) = payload(&a, "assets/ui/icon.svg");
    assert_eq!(type_path, "regia_asset::CookedImage");
    let (_, bytes_b) = payload(&b, "assets/ui/icon.svg");
    assert_eq!(bytes_a, bytes_b, "two cooks of one source give one payload");
    let img = CookedImage::from_postcard(&bytes_a).expect("image");
    assert_eq!((img.width, img.height), (32, 32));
    let at = |x: u32, y: u32| {
        let i = ((y * img.width + x) * 4) as usize;
        [img.rgba8[i], img.rgba8[i + 1], img.rgba8[i + 2], img.rgba8[i + 3]]
    };
    assert_eq!(at(16, 16), [0x33, 0x66, 0xff, 255], "inside the square");
    assert_eq!(at(4, 4), [0, 0, 0, 0], "outside is transparent");
    assert_eq!(at(7, 16), [0, 0, 0, 0], "the square starts at 8 px at 4x");
    assert_eq!(at(8, 16), [0x33, 0x66, 0xff, 255]);
    for root in [a, b] {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn a_scale_setting_multiplies_the_documents_size() {
    let root = project("svg-cook-scale", "(scale: Some(3.0), compress: Some(false))");
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let (_, bytes) = payload(&root, "assets/ui/icon.svg");
    let img = CookedImage::from_postcard(&bytes).expect("image");
    assert_eq!((img.width, img.height), (24, 24));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_refused_document_is_a_cook_error_with_its_code() {
    let root = regia_test_utils::unique_temp_dir("svg-cook-bad");
    let dir = root.join("assets/ui");
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::write(
        dir.join("evil.svg"),
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4">
<use xlink:href="file:///etc/passwd#x"/></svg>"##,
    )
    .expect("svg");
    std::fs::write(
        dir.join("huge.svg"),
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><!-- {} --></svg>"##,
            "x".repeat(regia_asset_pipeline::svg_import::SVG_MAX_BYTES)
        ),
    )
    .expect("svg");
    let report = cook(&root);
    assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
    let text = |name: &str| {
        report
            .errors
            .iter()
            .find(|(p, _)| p.ends_with(name))
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    };
    assert!(
        text("evil.svg").contains("svg_external_ref"),
        "{}",
        text("evil.svg")
    );
    assert!(text("huge.svg").contains("svg_too_large"), "{}", text("huge.svg"));
    let _ = std::fs::remove_dir_all(root);
}
