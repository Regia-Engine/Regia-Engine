// SPDX-License-Identifier: MPL-2.0
//! `.texpack.ron` through the project cook (ADR-3090): the packed texture cooks like a PNG and
//! decodes to the recipe's values, its source images are inputs of the cook key (local and shared
//! cache), the live cooker recooks it when one is saved, and a bad recipe fails with its code.

use std::path::{Path, PathBuf};

use image::ImageEncoder;
use regia_asset::{AssetManifest, CookedTexture, cooked_cache_dir};
use regia_asset_pipeline::cook_cache::{SharedCacheChoice, SharedCacheConfig, SharedCacheMode};
use regia_asset_pipeline::{
    CookOptions, CookReport, affected_sources, cook_project, cook_project_with_shared_cache, host_platform,
};

fn write_png(path: &Path, w: u32, h: u32, px: [u8; 4]) {
    let rgba = px.repeat((w * h) as usize);
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    std::fs::write(path, out).expect("png");
}

const RECIPE: &str = r#"(
    format_version: 1,
    sources: { "ao": "rock_ao.png", "rough": "rock_rough.png" },
    r: Source(source: "ao", channel: "r"),
    g: Source(source: "rough", channel: "r", invert: true),
    b: Constant(0.25),
    a: Constant(1.0),
)"#;

/// `assets/rock.texpack.ron` reading a 64×64 AO map and a 32×32 roughness map (resized up).
fn write_project(root: &Path) {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    write_png(&assets.join("rock_ao.png"), 64, 64, [200, 200, 200, 255]);
    write_png(&assets.join("rock_rough.png"), 32, 32, [100, 100, 100, 255]);
    std::fs::write(assets.join("rock.texpack.ron"), RECIPE).expect("recipe");
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

/// The cooked texture of `rel` decoded: (width, height, level 0 RGBA8, the cooked file's bytes).
fn decoded(root: &Path, rel: &str) -> (u32, u32, Vec<u8>, Vec<u8>) {
    let manifest = AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == rel)
        .unwrap_or_else(|| panic!("{rel} not in the manifest"));
    assert_eq!(entry.type_path, "regia_asset::CookedTexture");
    let bytes = std::fs::read(cache(root).join(regia_asset::cooked_file_name(entry.id))).expect("cooked");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("decode");
    let texture = CookedTexture::from_postcard(payload).expect("texture");
    let blocks = regia_render::CompressedTexture::from_ktx2(&texture.ktx2).expect("ktx2");
    let level = blocks.decode_level_rgba8(0).expect("level 0");
    (level.width, level.height, level.rgba, bytes)
}

fn assert_texels(rgba: &[u8], want: [u8; 4]) {
    for px in rgba.as_chunks::<4>().0 {
        for c in 0..4 {
            assert!(
                px[c].abs_diff(want[c]) <= 2,
                "texel {px:?}, expected {want:?} (channel {c})"
            );
        }
    }
}

#[test]
fn a_recipe_cooks_to_a_texture_holding_its_channels() {
    let root = regia_test_utils::unique_temp_dir("texpack-cook");
    write_project(&root);
    let first = cook(&root);
    assert_eq!(first.cooked, 3, "two PNGs and the recipe");
    let (w, h, rgba, _) = decoded(&root, "assets/rock.texpack.ron");
    assert_eq!((w, h), (64, 64), "resized to the largest source");
    // R = ao.r, G = 1 - rough.r, B = 0.25, A = 1.
    assert_texels(&rgba, [200, 155, 64, 255]);
    // The recipe has its own `.meta` (its id and texture settings).
    assert!(regia_asset::meta_path_for(&root.join("assets/rock.texpack.ron")).is_file());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_edit_recooks_the_recipe_and_nothing_else_does() {
    let root = regia_test_utils::unique_temp_dir("texpack-key");
    write_project(&root);
    cook(&root);
    let (_, _, _, before) = decoded(&root, "assets/rock.texpack.ron");
    let again = cook(&root);
    assert_eq!(
        (again.cooked, again.skipped, again.imports_skipped),
        (0, 3, 3),
        "unchanged: every source skips without its import"
    );

    write_png(&root.join("assets/rock_rough.png"), 32, 32, [40, 40, 40, 255]);
    let edited = cook(&root);
    assert_eq!(edited.cooked, 2, "the edited PNG and the recipe that reads it");
    let (_, _, rgba, after) = decoded(&root, "assets/rock.texpack.ron");
    assert_ne!(before, after);
    assert_texels(&rgba, [200, 215, 64, 255]);

    // The recipe's own `.meta` settings are inputs too.
    let meta_path = regia_asset::meta_path_for(&root.join("assets/rock.texpack.ron"));
    let mut meta =
        regia_asset::MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("meta");
    meta.settings = Some("(max_size: Some(32))".into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("meta");
    let resized = cook(&root);
    assert_eq!(resized.cooked, 1);
    assert_eq!(decoded(&root, "assets/rock.texpack.ron").0, 32);
    assert_eq!(cook(&root).cooked, 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_live_cooker_recooks_a_recipe_when_a_source_is_saved() {
    let root = regia_test_utils::unique_temp_dir("texpack-live");
    write_project(&root);
    let assets = root.join("assets");
    let changed = vec![assets.join("rock_rough.png")];
    let affected = affected_sources(&assets, host_platform(), &changed);
    assert!(
        affected.contains(&assets.join("rock.texpack.ron")),
        "{affected:?}"
    );
    assert!(
        affected.contains(&assets.join("rock_rough.png")),
        "the PNG cooks on its own too"
    );
    // The recipe itself, and its `.meta`.
    let recipe = assets.join("rock.texpack.ron");
    assert!(affected_sources(&assets, host_platform(), std::slice::from_ref(&recipe)).contains(&recipe));
    let unrelated = assets.join("other.png");
    write_png(&unrelated, 8, 8, [1, 2, 3, 255]);
    assert!(!affected_sources(&assets, host_platform(), &[unrelated]).contains(&recipe));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_bad_recipe_fails_the_cook_with_its_code() {
    let root = regia_test_utils::unique_temp_dir("texpack-bad");
    write_project(&root);
    let assets = root.join("assets");
    let cases = [
        (
            "channel.texpack.ron",
            r#"(format_version: 1, sources: {"ao": "rock_ao.png"}, r: Source(source: "ao", channel: "q"))"#,
            "texpack_bad_channel",
        ),
        (
            "missing.texpack.ron",
            r#"(format_version: 1, sources: {"ao": "no_such.png"}, r: Source(source: "ao", channel: "r"))"#,
            "texpack_missing_source",
        ),
        (
            "loop.texpack.ron",
            r#"(format_version: 1, sources: {"me": "loop.texpack.ron"}, r: Source(source: "me", channel: "r"))"#,
            "texpack_cycle",
        ),
    ];
    for (name, text, _) in &cases {
        std::fs::write(assets.join(name), text).expect("recipe");
    }
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    for (name, _, code) in &cases {
        let rel = format!("assets/{name}");
        let error = report
            .errors
            .iter()
            .find(|(path, _)| *path == rel)
            .unwrap_or_else(|| panic!("{rel}: no error in {:?}", report.errors));
        assert!(error.1.contains(code), "{rel}: {}", error.1);
    }
    assert_eq!(report.errors.len(), cases.len(), "the good recipe still cooks");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn another_machine_fetches_the_packed_texture_from_the_shared_cache() {
    let share = regia_test_utils::unique_temp_dir("texpack-share");
    let a = regia_test_utils::unique_temp_dir("texpack-share-a");
    write_project(&a);
    let choice = SharedCacheChoice::Use(SharedCacheConfig::new(&share, SharedCacheMode::ReadWrite));
    let first = cook_project_with_shared_cache(CookOptions::for_host(a.clone()), &choice).expect("cook a");
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(first.shared_cache.as_ref().map(|s| s.published), Some(3));

    // Machine B: the same assets and `.meta` files, no `.regia/`.
    let b = regia_test_utils::unique_temp_dir("texpack-share-b");
    std::fs::create_dir_all(b.join("assets")).expect("assets");
    for entry in std::fs::read_dir(a.join("assets")).expect("assets").flatten() {
        std::fs::copy(entry.path(), b.join("assets").join(entry.file_name())).expect("copy");
    }
    let report = cook_project_with_shared_cache(CookOptions::for_host(b.clone()), &choice).expect("cook b");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let s = report.shared_cache.as_ref().expect("summary");
    assert_eq!(
        (s.hits, s.misses),
        (3, 0),
        "the recipe is fetched, not packed again"
    );
    assert_eq!(
        decoded(&b, "assets/rock.texpack.ron").3,
        decoded(&a, "assets/rock.texpack.ron").3
    );

    // A source edit on B misses the shared entry: the source images are part of the shared key.
    write_png(&b.join("assets/rock_ao.png"), 64, 64, [10, 10, 10, 255]);
    let edited = cook_project_with_shared_cache(CookOptions::for_host(b.clone()), &choice).expect("cook b");
    let s = edited.shared_cache.as_ref().expect("summary");
    assert_eq!(s.misses, 2, "the edited PNG and the recipe");
    assert_texels(&decoded(&b, "assets/rock.texpack.ron").2, [10, 155, 64, 255]);
    for dir in [&share, &a, &b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
