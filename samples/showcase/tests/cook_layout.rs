// SPDX-License-Identifier: MIT-0
//! The cook-convention scene layout (`docs/formats/terrain.md` § 9) read without a `scene.ron`: extents from
//! `heightfield.json`, layer textures from the layer file, splat packs from `macro/`, and each malformed variant
//! refused with its error code. Sizes are data-driven: an 8192 x 8192 heightfield loads. No GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regia_test_utils::unique_temp_dir;
use showcase::ErrorCode;
use showcase::scene::{COOK_LAYERS, SceneLayout, load_scene};

fn png_bytes(width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(data).expect("data");
        writer.finish().expect("finish");
    }
    out
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// A `side`² heightfield ramping from 0 at column 0 to 65535 at the last column.
fn ramp(side: u32) -> Vec<u8> {
    (0..side * side)
        .flat_map(|i| {
            let x = i % side;
            let v = (u64::from(x) * 65535 / u64::from(side - 1)) as u16;
            v.to_be_bytes()
        })
        .collect()
}

fn rgba(side: u32, texel: [u8; 4]) -> Vec<u8> {
    png_bytes(
        side,
        side,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &texel.repeat((side * side) as usize),
    )
}

const LAYERS: &str = r#"(
    version: 3,
    wetness_default: 0.0,
    layers: [
        (name: "mud", albedo: (1.0, 1.0, 1.0), roughness: 0.9, tiling: 0.5, slope_min: 0.0, slope_max: 1.6,
         altitude_min: -1000.0, altitude_max: 1000.0, blend_sharpness: 0.12, triplanar: false,
         albedo_texture: Some("detail/mud/albedo.png"), normal_texture: Some("detail/mud/normal.png"),
         height_texture: None, orm_texture: Some("detail/mud/orm.png"), detail_normal_strength: 1.0, height_blend: 0.0),
        (name: "snow", albedo: (1.0, 1.0, 1.0), roughness: 0.6, tiling: 0.5, slope_min: 0.0, slope_max: 1.6,
         altitude_min: -1000.0, altitude_max: 1000.0, blend_sharpness: 0.12, triplanar: false,
         albedo_texture: Some("detail/snow/albedo.png"), normal_texture: None,
         height_texture: Some("detail/snow/height.png"), orm_texture: None, detail_normal_strength: 1.0, height_blend: 0.0),
    ],
)"#;

/// A cook-layout scene: a `side`² heightfield over `size_m` metres between `min_m` and `max_m`, two textured
/// layers, both splat packs (32²) and the example's sky.
fn cook_scene(tag: &str, side: u32, size_m: f32) -> PathBuf {
    let dir = unique_temp_dir(tag);
    write(
        &dir,
        "terrain/heightfield.png",
        &png_bytes(
            side,
            side,
            png::ColorType::Grayscale,
            png::BitDepth::Sixteen,
            &ramp(side),
        ),
    );
    write(
        &dir,
        "terrain/heightfield.json",
        format!(
            "{{\"size\": {side}, \"size_m\": {size_m}, \"min_m\": 10.0, \"max_m\": 110.0, \"origin\": [0, 0, 0], \"note\": \"x\"}}"
        )
        .as_bytes(),
    );
    write(&dir, COOK_LAYERS, LAYERS.as_bytes());
    for (rel, texel) in [
        ("detail/mud/albedo.png", [90, 70, 50, 255]),
        ("detail/mud/normal.png", [128, 128, 255, 255]),
        ("detail/mud/orm.png", [255, 230, 0, 255]),
        ("detail/snow/albedo.png", [230, 235, 240, 255]),
        ("detail/snow/height.png", [128, 128, 128, 255]),
    ] {
        write(&dir, &format!("terrain/{rel}"), &rgba(16, texel));
    }
    write(&dir, "terrain/macro/pack0.png", &rgba(32, [255, 0, 0, 0]));
    write(&dir, "terrain/macro/pack1.png", &rgba(32, [0, 0, 0, 0]));
    let sky = Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene/sky.ron");
    std::fs::copy(sky, dir.join("sky.ron")).expect("sky");
    dir
}

fn code(dir: &Path) -> ErrorCode {
    load_scene(dir).expect_err("the scene must be refused").code
}

#[test]
fn a_cook_layout_loads_with_its_extents_layers_and_splat_packs() {
    let dir = cook_scene("showcase-cook-ok", 65, 300.0);
    let scene = load_scene(&dir).expect("cook layout loads");
    assert_eq!(scene.layout, SceneLayout::Cook);
    assert_eq!((scene.map.width(), scene.map.height()), (65, 65));
    assert!(
        (scene.map.world_width() - 300.0).abs() < 1e-3,
        "size_m is the world width"
    );
    let (lo, hi) = scene.map.min_max_height();
    assert!(
        (lo - 10.0).abs() < 1e-3 && (hi - 110.0).abs() < 1e-2,
        "min_m..max_m: {lo}..{hi}"
    );
    let material = scene.material.as_ref().expect("both layers name textures");
    assert_eq!(material.layers.len(), 2);
    let mud = material.layers[0].as_ref().expect("mud textures");
    assert!(mud.use_orm, "the mud layer's orm map is used");
    let [pack0, pack1] = scene.splat.as_ref().expect("both packs");
    assert_eq!((pack0.width, pack1.width), (32, 32));
    assert_eq!(scene.manifest.sky, "sky.ron");
    assert!(scene.scatter.is_none() && scene.track.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_scene_ron_wins_over_the_cook_layout() {
    let dir = cook_scene("showcase-cook-manifest", 33, 64.0);
    showcase::example::write_example_scene(&dir).expect("example");
    let scene = load_scene(&dir).expect("manifest scene loads");
    assert_eq!(scene.layout, SceneLayout::Manifest);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_cook_layouts_are_refused_with_their_codes() {
    // No heightfield.json.
    let dir = cook_scene("showcase-cook-nojson", 33, 64.0);
    std::fs::remove_file(dir.join("terrain/heightfield.json")).expect("rm");
    assert_eq!(code(&dir), ErrorCode::SceneFileMissing);

    // max_m under min_m, a negative size, garbage.
    for json in [
        r#"{"size_m": 64, "min_m": 10, "max_m": 5}"#,
        r#"{"size_m": -1, "min_m": 0, "max_m": 5}"#,
        r#"{"min_m": 0, "max_m": 5}"#,
        "not json",
    ] {
        write(&dir, "terrain/heightfield.json", json.as_bytes());
        assert_eq!(code(&dir), ErrorCode::HeightfieldMalformed, "{json}");
    }
    // The PNG is not the size the json says.
    write(
        &dir,
        "terrain/heightfield.json",
        br#"{"size": 8192, "size_m": 64, "min_m": 0, "max_m": 5}"#,
    );
    assert_eq!(code(&dir), ErrorCode::HeightfieldMalformed);
    let _ = std::fs::remove_dir_all(&dir);

    // One splat pack, packs of two sizes, a pack that is not a PNG.
    let dir = cook_scene("showcase-cook-packs", 33, 64.0);
    std::fs::remove_file(dir.join("terrain/macro/pack1.png")).expect("rm");
    assert_eq!(code(&dir), ErrorCode::SceneFileMissing);
    write(&dir, "terrain/macro/pack1.png", &rgba(16, [0, 0, 0, 0]));
    assert_eq!(code(&dir), ErrorCode::SplatMalformed);
    write(&dir, "terrain/macro/pack1.png", b"garbage");
    assert_eq!(code(&dir), ErrorCode::SplatMalformed);
    let _ = std::fs::remove_dir_all(&dir);

    // One layer with textures and one without.
    let dir = cook_scene("showcase-cook-mixed", 33, 64.0);
    let mixed = LAYERS.replacen("Some(\"detail/snow/albedo.png\")", "None", 1);
    write(&dir, COOK_LAYERS, mixed.as_bytes());
    assert_eq!(code(&dir), ErrorCode::MaterialsMalformed);
    // A layer texture that is missing.
    write(&dir, COOK_LAYERS, LAYERS.as_bytes());
    std::fs::remove_file(dir.join("terrain/detail/snow/height.png")).expect("rm");
    assert_eq!(code(&dir), ErrorCode::SceneFileMissing);
    let _ = std::fs::remove_dir_all(&dir);

    // No sky.
    let dir = cook_scene("showcase-cook-nosky", 33, 64.0);
    std::fs::remove_file(dir.join("sky.ron")).expect("rm");
    assert_eq!(code(&dir), ErrorCode::SceneFileMissing);
    let _ = std::fs::remove_dir_all(&dir);

    // Neither layout.
    let dir = unique_temp_dir("showcase-cook-empty");
    assert_eq!(code(&dir), ErrorCode::SceneManifestMissing);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_8192_heightfield_loads_at_one_metre_cells() {
    // The final showcase map: 8192 samples a side over 8191 m (1 m cells), generated here.
    let side = 8192u32;
    let dir = cook_scene("showcase-cook-8192", side, (side - 1) as f32);
    let scene = load_scene(&dir).expect("8192 map loads");
    assert_eq!((scene.map.width(), scene.map.height()), (side, side));
    assert!((scene.map.world_width() - 8191.0).abs() < 1e-3);
    // Column c sits at x = c - 4095.5 m: the ramp's height at x = 0 is the middle of 10..110 m.
    let mid = scene.map.sample_bilinear(0.0, 0.0);
    assert!((mid - 60.0).abs() < 0.05, "centred, 1 m cells: {mid}");
    let _ = std::fs::remove_dir_all(&dir);
}
