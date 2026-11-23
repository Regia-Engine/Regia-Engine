// SPDX-License-Identifier: MPL-2.0
//! The thin film from a glTF to a frame (ADR-1977): `regia cook --ship` cooks a material with
//! `KHR_materials_iridescence` and two block-compressed maps, the sources and the cooked cache are
//! deleted, and the model loads from the pak alone with its film and its maps' channels, and the
//! film changes the frame.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::ImageEncoder;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_render::cooked_model::load_cooked_model;

/// A 64×64 RGBA PNG (large enough to block-compress) from `pixel(x, y)`.
fn png(pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&pixel(x, y));
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

/// Red channel of the coverage map on the left half, zero on the right.
const COVERED: u8 = 255;
/// Green channel of the thickness map.
const THICKNESS: u8 = 128;

/// A quad on the XZ plane facing +Y, `feather.gltf` with its buffer and maps beside it.
///
/// The film's index (2.0, melanin-like) must differ from the base's: a dielectric base of F0 0.04
/// has index 1.5, and a film of 1.5 on it has no lower interface, so it shows (rightly) almost
/// nothing. The rough lobe spreads the sun's highlight over both halves of the quad.
fn write_project(root: &Path) {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    let positions: [[f32; 3]; 4] = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ];
    let uvs: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let indices: [u16; 6] = [0, 2, 1, 0, 3, 2];
    let mut bin = Vec::new();
    for p in positions {
        p.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for _ in 0..4 {
        [0.0f32, 1.0, 0.0]
            .iter()
            .for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for uv in uvs {
        uv.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    indices
        .iter()
        .for_each(|i| bin.extend_from_slice(&i.to_le_bytes()));
    assert_eq!(bin.len(), 140);
    let json = r#"{
  "asset": {"version": "2.0"},
  "extensionsUsed": ["KHR_materials_iridescence"],
  "scene": 0,
  "scenes": [{"nodes": [0]}],
  "nodes": [{"mesh": 0, "name": "Feather"}],
  "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0, "mode": 4}]}],
  "materials": [{"name": "feather",
    "pbrMetallicRoughness": {"baseColorFactor": [0.02, 0.03, 0.05, 1.0], "metallicFactor": 0.0, "roughnessFactor": 0.6},
    "extensions": {"KHR_materials_iridescence": {
      "iridescenceFactor": 1.0, "iridescenceTexture": {"index": 0},
      "iridescenceIor": 2.0, "iridescenceThicknessMinimum": 200.0, "iridescenceThicknessMaximum": 600.0,
      "iridescenceThicknessTexture": {"index": 1}}}}],
  "textures": [{"source": 0}, {"source": 1}],
  "images": [{"uri": "feather_film.png"}, {"uri": "feather_thickness.png"}],
  "buffers": [{"byteLength": 140, "uri": "feather.bin"}],
  "bufferViews": [
    {"buffer": 0, "byteOffset": 0, "byteLength": 48, "target": 34962},
    {"buffer": 0, "byteOffset": 48, "byteLength": 48, "target": 34962},
    {"buffer": 0, "byteOffset": 96, "byteLength": 32, "target": 34962},
    {"buffer": 0, "byteOffset": 128, "byteLength": 12, "target": 34963}
  ],
  "accessors": [
    {"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-1, 0, -1], "max": [1, 0, 1]},
    {"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3"},
    {"bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2"},
    {"bufferView": 3, "componentType": 5123, "count": 6, "type": "SCALAR"}
  ]
}"#;
    std::fs::write(assets.join("feather.gltf"), json).expect("gltf");
    std::fs::write(assets.join("feather.bin"), bin).expect("bin");
    let film = png(|x, _| [if x < 32 { COVERED } else { 0 }, 40, 40, 255]);
    std::fs::write(assets.join("feather_film.png"), film).expect("film map");
    let thickness = png(|_, _| [40, THICKNESS, 40, 255]);
    std::fs::write(assets.join("feather_thickness.png"), thickness).expect("thickness map");
}

fn pak_dir(root: &Path) -> PathBuf {
    root.join("build").join("paks").join("linux")
}

/// Deletes the sources and the cooked cache and moves the paks into `<root>/game/content`.
fn ship_layout(root: &Path) -> PathBuf {
    std::fs::remove_dir_all(root.join("assets")).expect("remove sources");
    std::fs::remove_dir_all(root.join(".regia")).expect("remove cooked cache");
    let game = root.join("game");
    let content = game.join("content");
    std::fs::create_dir_all(&content).expect("content dir");
    for entry in std::fs::read_dir(pak_dir(root)).expect("pak dir").flatten() {
        if entry.path().extension().is_some_and(|e| e == "fpak") {
            std::fs::rename(entry.path(), content.join(entry.file_name())).expect("move pak");
        }
    }
    game
}

#[test]
fn a_gltf_film_cooks_loads_from_the_pak_alone_and_draws() {
    let root = regia_test_utils::unique_temp_dir("thin_film_pak");
    write_project(&root);
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("cook")
        .arg("--project")
        .arg(&root)
        .args(["--platform", "linux", "--ship"])
        .env_remove("REGIA_PAK_GPU_MESHES")
        .env_remove("FERRITE_PAK_GPU_MESHES")
        .output()
        .expect("run regia cook");
    assert!(
        out.status.success(),
        "regia cook failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let game = ship_layout(&root);
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: game.clone(),
        platform: "linux".into(),
        content_dir: Some(game.join("content")),
    })
    .expect("open the asset server");
    let gltf = server
        .id_for_path("assets/feather.gltf")
        .expect("the pak's manifest names the glTF");
    let draws = load_cooked_model(&mut server, gltf).expect("the model loads from the pak");
    let material = &draws.materials[0];
    let film = material.thin_film;
    assert_eq!(film.factor, 1.0);
    assert_eq!(film.ior, 2.0);
    assert_eq!((film.thickness_min, film.thickness_max), (200.0, 600.0));
    // Both maps survived the cook, in their channels: coverage in R, thickness in G.
    let map = material.thin_film_map.as_ref().expect("the packed film map");
    assert!(
        map.width >= 32 && map.height >= 32,
        "{}x{}",
        map.width,
        map.height
    );
    let texel = |x: u32, y: u32| {
        let i = ((y * map.height / 64 * map.width + x * map.width / 64) * 4) as usize;
        [map.rgba[i], map.rgba[i + 1]]
    };
    let (left, right) = (texel(8, 32), texel(56, 32));
    assert!(left[0] > 235, "covered side: {left:?}");
    assert!(right[0] < 20, "uncovered side: {right:?}");
    for t in [left, right] {
        assert!(t[1].abs_diff(THICKNESS) < 16, "thickness in G: {t:?}");
    }
    headless_frames(&draws);
    let _ = std::fs::remove_dir_all(&root);
}

/// The film changes the covered half of the quad and leaves the uncovered half alone.
fn headless_frames(draws: &regia_render::cooked_model::CookedModelDraws) {
    use regia_render::{DirectionalLightUniform, LabScene, RenderDevice, render_lab_scene};
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let render = |materials: Vec<regia_render::LabMaterial>| {
        let scene = LabScene {
            width: 64,
            height: 64,
            eye: [0.0, 2.6, 0.01],
            target: [0.0, 0.0, 0.0],
            fov_y: 50.0_f32.to_radians(),
            near: 0.1,
            far: 20.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: true,
            exposure_ev: 13.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: None,
            // Straight down the view axis (but for the small tilt that keeps it off the eye), so the
            // highlight sits on the quad's center line, across both halves of the map.
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.1], [1.0, 1.0, 1.0], 20_000.0),
            local_lights: Vec::new(),
            materials,
            meshes: draws.meshes.clone(),
        };
        render_lab_scene(&device, &scene)
            .expect("render the cooked model")
            .pixels
    };
    let with = render(draws.materials.clone());
    let mut plain = draws.materials.clone();
    plain[0].thin_film.factor = 0.0;
    let without = render(plain);
    let differing = |range: std::ops::Range<usize>| {
        (0..64)
            .flat_map(|y| range.clone().map(move |x| (y * 64 + x) * 4))
            .filter(|&i| with[i..i + 3] != without[i..i + 3])
            .count()
    };
    // The cook's default forward conversion turns the quad about +Y, so which half of the screen
    // holds the covered half of the map is the cook's business: one half moves, the other does not.
    let (a, b) = (differing(10..30), differing(36..54));
    let (covered, uncovered) = (a.max(b), a.min(b));
    assert!(
        covered > 400,
        "{covered} pixels of the covered half moved ({a} and {b} by screen half)"
    );
    assert!(
        uncovered < covered / 20,
        "{uncovered} pixels of the uncovered half moved"
    );
}
