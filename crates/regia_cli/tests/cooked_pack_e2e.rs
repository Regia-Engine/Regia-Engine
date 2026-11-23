// SPDX-License-Identifier: MPL-2.0
//! Cooked packs end to end (ADR-1594, DO27 request DO-3): `regia cook --ship` turns a textured
//! glTF into a pak with a GPU-ready mesh and BC textures, the sources and the cooked cache are
//! deleted, and the model loads from the pak alone (no glTF or PNG left to parse) and draws in a
//! headless frame. A plain cook of the same project keeps the mesh compact: the counters tell the
//! two apart.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::ImageEncoder;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_render::cooked_model::load_cooked_model;
use serde_json::Value;

/// A 64×64 RGBA PNG (large enough to block-compress) whose pixels depend on `seed`.
fn png(seed: u8) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&[((x * 4) as u8) ^ seed, (y * 4) as u8, 128, 255]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

/// A textured quad on the XZ plane facing +Y: `crate.gltf` with its buffer and two images
/// (base color and normal map) beside it, referenced by URI.
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
  "scene": 0,
  "scenes": [{"nodes": [0]}],
  "nodes": [{"mesh": 0, "name": "Crate"}],
  "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0, "mode": 4}]}],
  "materials": [{"name": "crate", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}, "normalTexture": {"index": 1}}],
  "textures": [{"source": 0}, {"source": 1}],
  "images": [{"uri": "crate_albedo.png"}, {"uri": "crate_normal.png"}],
  "buffers": [{"byteLength": 140, "uri": "crate.bin"}],
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
    std::fs::write(assets.join("crate.gltf"), json).expect("gltf");
    std::fs::write(assets.join("crate.bin"), bin).expect("bin");
    std::fs::write(assets.join("crate_albedo.png"), png(1)).expect("albedo");
    std::fs::write(assets.join("crate_normal.png"), png(2)).expect("normal");
}

/// Runs `regia cook --project <root> --platform linux <extra>` with the GPU-ready mesh variable
/// cleared, so only the flags decide.
fn regia_cook(root: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("cook")
        .arg("--project")
        .arg(root)
        .args(["--platform", "linux"])
        .args(extra)
        .env_remove("REGIA_PAK_GPU_MESHES")
        .env_remove("FERRITE_PAK_GPU_MESHES")
        .output()
        .expect("run regia cook")
}

fn pak_dir(root: &Path) -> PathBuf {
    root.join("build").join("paks").join("linux")
}

/// Deletes the sources and the cooked cache and moves the paks into `<root>/game/content`: what is
/// left is what a shipped game has.
fn ship_layout(root: &Path) -> PathBuf {
    std::fs::remove_dir_all(root.join("assets")).expect("remove sources");
    std::fs::remove_dir_all(root.join(".regia")).expect("remove cooked cache");
    let game = root.join("game");
    let content = game.join("content");
    std::fs::create_dir_all(&content).expect("content dir");
    for entry in std::fs::read_dir(pak_dir(root)).expect("pak dir").flatten() {
        let name = entry.file_name();
        if entry.path().extension().is_some_and(|e| e == "fpak") {
            std::fs::rename(entry.path(), content.join(&name)).expect("move pak");
        }
    }
    game
}

fn open_game(game: &Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: game.to_path_buf(),
        platform: "linux".into(),
        content_dir: Some(game.join("content")),
    })
    .expect("open the asset server")
}

fn assert_success(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ship_cook_loads_gpu_ready_meshes_and_bc_textures_from_the_pak_alone() {
    let root = regia_test_utils::unique_temp_dir("cooked_pack_ship");
    write_project(&root);

    let out = regia_cook(&root, &["--ship"]);
    assert_success(&out, "regia cook --ship");

    // The report the cook wrote next to the paks.
    let text = std::fs::read_to_string(pak_dir(&root).join("cooked_pack.json")).expect("cooked_pack.json");
    let report: Value = serde_json::from_str(&text).expect("report json");
    assert_eq!(report["platform"], "linux");
    assert_eq!(report["errors"], 0, "{report:#}");
    assert_eq!(report["warnings"], 0, "{report:#}");
    let summary = &report["paks"][0]["summary"];
    // The glTF's primary mesh and its primitive's sub-asset mesh: both GPU-ready, none compact.
    assert!(summary["meshes"]["gpu_ready"].as_u64() >= Some(1), "{summary:#}");
    assert!(summary["meshes"].get("compact").is_none(), "{summary:#}");
    let textures = summary["textures"].as_u64().unwrap_or_default();
    assert!(textures >= 2, "base color and normal map: {summary:#}");
    assert_eq!(
        summary["texture_families"]["bc"].as_u64(),
        Some(textures),
        "{summary:#}"
    );
    assert_eq!(summary["textures_inspected"], true);

    // `--pack-report` reads the same paks without cooking.
    let out = regia_cook(&root, &["--pack-report"]);
    assert_success(&out, "regia cook --pack-report");
    let again: Value = serde_json::from_slice(&out.stdout).expect("pack report json");
    assert_eq!(again["paks"][0]["summary"], *summary);

    let game = ship_layout(&root);
    let mut server = open_game(&game);
    assert_eq!(server.pak_count(), 1);
    let gltf = server
        .id_for_path("assets/crate.gltf")
        .expect("the pak's manifest names the glTF");
    let draws = load_cooked_model(&mut server, gltf).expect("the model loads from the pak");
    assert_eq!(draws.meshes.len(), 1);
    assert_eq!(draws.meshes[0].indices.len(), 6);
    assert!(
        draws.texture_bytes > 0,
        "the material carries block-compressed chains"
    );
    let compressed = draws.materials[0]
        .compressed
        .as_ref()
        .expect("compressed maps on the material");
    assert!(compressed.albedo.is_some() && compressed.normal.is_some());

    let forms = server.load_forms();
    assert_eq!(forms.gpu_meshes, 1, "{forms:?}");
    assert_eq!(forms.compact_meshes + forms.legacy_meshes, 0, "{forms:?}");
    assert_eq!(forms.textures, 2, "{forms:?}");
    assert_eq!(forms.images_rgba8, 0, "{forms:?}");
    assert_eq!(
        forms.cache_loads, 0,
        "nothing came from a cooked cache: {forms:?}"
    );
    assert!(forms.pak_loads >= 3, "{forms:?}");

    // The mounted pak, as `assets.cooked_pack` reports it.
    let mounted = server.mounted_pak_summaries(true);
    assert_eq!(mounted.len(), 1);
    assert!(regia_asset::cooked_pack::check_pak_summary(&mounted[0], "linux").is_empty());

    headless_frame(draws);
    let _ = std::fs::remove_dir_all(&root);
}

/// Draws the loaded model in one headless frame (skipped without an adapter).
fn headless_frame(draws: regia_render::cooked_model::CookedModelDraws) {
    use regia_render::{DirectionalLightUniform, LabScene, RenderDevice, render_lab_scene};
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip the headless frame: {err}");
            return;
        }
    };
    let scene = LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 2.2, 1.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 1.0, 1.0], 2.0),
        local_lights: Vec::new(),
        materials: draws.materials,
        meshes: draws.meshes,
    };
    let frame = render_lab_scene(&device, &scene).expect("render the cooked model");
    assert_eq!(frame.pixels.len(), 64 * 64 * 4);
    // The quad covers the middle of the frame; the corner is the clear color.
    let px = |x: usize, y: usize| {
        let i = (y * 64 + x) * 4;
        [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]]
    };
    let corner = px(0, 0);
    let covered = (0..64)
        .flat_map(|y| (0..64).map(move |x| (x, y)))
        .filter(|&(x, y)| px(x, y) != corner)
        .count();
    assert!(
        covered > 64 * 64 / 8,
        "the quad is drawn: {covered} pixels differ from the corner"
    );
}

#[test]
fn plain_cook_keeps_compact_meshes_and_says_so() {
    let root = regia_test_utils::unique_temp_dir("cooked_pack_plain");
    write_project(&root);

    let out = regia_cook(&root, &[]);
    assert_success(&out, "regia cook");
    assert!(
        !pak_dir(&root).join("cooked_pack.json").exists(),
        "only --ship writes the report"
    );

    // The report flags the compact mesh as a warning: the command still succeeds.
    let out = regia_cook(&root, &["--pack-report"]);
    assert_success(&out, "regia cook --pack-report");
    let report: Value = serde_json::from_slice(&out.stdout).expect("pack report json");
    assert_eq!(report["errors"], 0, "{report:#}");
    assert_eq!(
        report["warnings"], 1,
        "one finding for all the compact meshes: {report:#}"
    );
    assert_eq!(report["paks"][0]["findings"][0]["code"], "compact_meshes");
    assert_eq!(report["paks"][0]["findings"][0]["severity"], "warning");
    let example = report["paks"][0]["findings"][0]["examples"][0]
        .as_str()
        .unwrap_or_default();
    assert!(example.starts_with("assets/crate.gltf"), "{report:#}");

    let game = ship_layout(&root);
    let mut server = open_game(&game);
    let gltf = server.id_for_path("assets/crate.gltf").expect("glTF in the pak");
    load_cooked_model(&mut server, gltf).expect("loads");
    let forms = server.load_forms();
    assert_eq!((forms.gpu_meshes, forms.compact_meshes), (0, 1), "{forms:?}");
    assert!(!forms.cooked_only(), "a compact mesh is load-time work");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gpu_meshes_flag_is_refused_for_android() {
    let root = regia_test_utils::unique_temp_dir("cooked_pack_android");
    std::fs::create_dir_all(root.join("assets")).expect("assets dir");
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("cook")
        .arg("--project")
        .arg(&root)
        .args(["--platform", "android", "--gpu-meshes"])
        .output()
        .expect("run regia cook");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("desktop option"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// audit-seq (High): the CLI built the cache, pak and report paths from the tag as typed while
/// the cook lowercased its own copy, so `--platform Linux --ship` packed an empty cache and the
/// ship check passed with no entries. Every path now uses the lowercased tag.
#[test]
fn a_mixed_case_platform_tag_ships_the_cache_it_cooked() {
    let root = regia_test_utils::unique_temp_dir("cooked_pack_case");
    write_project(&root);
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("cook")
        .arg("--project")
        .arg(&root)
        .args(["--platform", "Linux", "--ship"])
        .env_remove("REGIA_PAK_GPU_MESHES")
        .env_remove("FERRITE_PAK_GPU_MESHES")
        .output()
        .expect("run regia cook");
    assert_success(&out, "regia cook --platform Linux --ship");
    let text = std::fs::read_to_string(pak_dir(&root).join("cooked_pack.json")).expect("cooked_pack.json");
    let report: Value = serde_json::from_str(&text).expect("report json");
    assert_eq!(report["platform"], "linux");
    let summary = &report["paks"][0]["summary"];
    assert!(summary["meshes"]["gpu_ready"].as_u64() >= Some(1), "{summary:#}");
    assert!(summary["textures"].as_u64() >= Some(1), "{summary:#}");
    assert!(
        !root.join("build").join("paks").join("Linux").exists() || cfg!(any(windows, target_os = "macos")),
        "no pak directory under the tag as typed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Adds a `COLOR_0` stream (`VEC4` float, one color per vertex) to the project `write_project` wrote.
fn add_color0(root: &Path, colors: [[f32; 4]; 4]) {
    let assets = root.join("assets");
    let mut bin = std::fs::read(assets.join("crate.bin")).expect("bin");
    let offset = bin.len();
    for c in colors {
        c.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    let text = std::fs::read_to_string(assets.join("crate.gltf")).expect("gltf");
    let mut gltf: Value = serde_json::from_str(&text).expect("gltf json");
    gltf["buffers"][0]["byteLength"] = bin.len().into();
    let views = gltf["bufferViews"].as_array_mut().expect("views");
    views.push(serde_json::json!({"buffer": 0, "byteOffset": offset, "byteLength": 64, "target": 34962}));
    let view = views.len() - 1;
    let accessors = gltf["accessors"].as_array_mut().expect("accessors");
    accessors
        .push(serde_json::json!({"bufferView": view, "componentType": 5126, "count": 4, "type": "VEC4"}));
    let accessor = accessors.len() - 1;
    gltf["meshes"][0]["primitives"][0]["attributes"]["COLOR_0"] = accessor.into();
    std::fs::write(assets.join("crate.bin"), bin).expect("bin");
    std::fs::write(assets.join("crate.gltf"), gltf.to_string()).expect("gltf");
}

/// Adds one morph target (`POSITION` deltas, `VEC3` float with the bounds glTF requires) to the
/// primitive `write_project` wrote.
fn add_morph_target(root: &Path, deltas: [[f32; 3]; 4]) {
    let assets = root.join("assets");
    let mut bin = std::fs::read(assets.join("crate.bin")).expect("bin");
    let offset = bin.len();
    for d in deltas {
        d.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    let bound = |pick: fn(f32, f32) -> f32| {
        (0..3)
            .map(|axis| deltas.iter().map(|d| d[axis]).fold(deltas[0][axis], pick))
            .collect::<Vec<f32>>()
    };
    let text = std::fs::read_to_string(assets.join("crate.gltf")).expect("gltf");
    let mut gltf: Value = serde_json::from_str(&text).expect("gltf json");
    gltf["buffers"][0]["byteLength"] = bin.len().into();
    let views = gltf["bufferViews"].as_array_mut().expect("views");
    views.push(serde_json::json!({"buffer": 0, "byteOffset": offset, "byteLength": 48, "target": 34962}));
    let view = views.len() - 1;
    let accessors = gltf["accessors"].as_array_mut().expect("accessors");
    accessors.push(serde_json::json!({
        "bufferView": view, "componentType": 5126, "count": 4, "type": "VEC3",
        "min": bound(f32::min), "max": bound(f32::max),
    }));
    let accessor = accessors.len() - 1;
    gltf["meshes"][0]["primitives"][0]["targets"] = serde_json::json!([{ "POSITION": accessor }]);
    gltf["meshes"][0]["weights"] = serde_json::json!([0.0]);
    std::fs::write(assets.join("crate.bin"), bin).expect("bin");
    std::fs::write(assets.join("crate.gltf"), gltf.to_string()).expect("gltf");
}

/// DO27 request `20261115-1926-COOKED_PACKS_DESIGN-gpu-ready-color-skin-morph`, the morph part
/// (ADR-10080, GPU mesh v3): `regia cook --platform linux --ship` on a glTF with a morph target
/// stores the primitive GPU-ready (no `compact_meshes` finding; with layout v2 it stayed compact),
/// and `AssetServer::load_mesh` returns the target from the pak alone.
#[test]
fn ship_cook_stores_a_morphing_mesh_gpu_ready_and_keeps_its_target() {
    let root = regia_test_utils::unique_temp_dir("cooked_pack_morph");
    write_project(&root);
    add_morph_target(
        &root,
        [
            [0.0, 0.5, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.25, 0.0],
            [0.0, 0.0, 0.0],
        ],
    );

    let out = regia_cook(&root, &["--ship"]);
    assert_success(&out, "regia cook --ship");
    let text = std::fs::read_to_string(pak_dir(&root).join("cooked_pack.json")).expect("cooked_pack.json");
    let report: Value = serde_json::from_str(&text).expect("report json");
    assert_eq!(report["errors"], 0, "{report:#}");
    assert_eq!(report["warnings"], 0, "no compact_meshes finding: {report:#}");
    let summary = &report["paks"][0]["summary"];
    assert!(summary["meshes"]["gpu_ready"].as_u64() >= Some(1), "{summary:#}");
    assert!(summary["meshes"].get("compact").is_none(), "{summary:#}");

    let game = ship_layout(&root);
    let mut server = open_game(&game);
    let gltf = server.id_for_path("assets/crate.gltf").expect("glTF in the pak");
    let model = server
        .load_model(regia_render::cooked_model::model_id(gltf))
        .expect("model");
    let handle = server
        .load_mesh(model.parts[0].mesh)
        .expect("the primitive's mesh");
    let mesh = server.meshes().get(&handle).expect("mesh");
    assert_eq!(
        mesh.morph_targets.len(),
        1,
        "the morph target survives the GPU-ready pak entry"
    );
    let target = &mesh.morph_targets[0];
    assert_eq!(target.vertex_indices.len(), 2, "two vertices move: {target:?}");
    let mut moved: Vec<f32> = target.position_deltas.iter().map(|d| d[1]).collect();
    moved.sort_by(f32::total_cmp);
    assert_eq!(moved, [0.25, 0.5], "{target:?}");
    load_cooked_model(&mut server, gltf).expect("the model loads from the pak");
    let forms = server.load_forms();
    assert_eq!(forms.compact_meshes + forms.legacy_meshes, 0, "{forms:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// DO27 request `20261115-1926-COOKED_PACKS_DESIGN-gpu-ready-color-skin-morph` (ADR-7919):
/// `regia cook --platform linux --ship` on a glTF with `COLOR_0` stores the primitive GPU-ready
/// (no `compact_meshes` finding), `AssetServer::load_mesh` returns the cooked colors from the pak
/// alone, and `load_cooked_model` loads it with no compact decode.
#[test]
fn ship_cook_stores_a_vertex_colored_mesh_gpu_ready_and_keeps_color0() {
    let root = regia_test_utils::unique_temp_dir("cooked_pack_color0");
    write_project(&root);
    let colors = [
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 1.0],
        [0.5, 0.25, 0.75, 1.0],
    ];
    add_color0(&root, colors);

    let out = regia_cook(&root, &["--ship"]);
    assert_success(&out, "regia cook --ship");
    let text = std::fs::read_to_string(pak_dir(&root).join("cooked_pack.json")).expect("cooked_pack.json");
    let report: Value = serde_json::from_str(&text).expect("report json");
    assert_eq!(report["errors"], 0, "{report:#}");
    assert_eq!(report["warnings"], 0, "no compact_meshes finding: {report:#}");
    let summary = &report["paks"][0]["summary"];
    assert!(summary["meshes"]["gpu_ready"].as_u64() >= Some(1), "{summary:#}");
    assert!(summary["meshes"].get("compact").is_none(), "{summary:#}");

    let game = ship_layout(&root);
    let mut server = open_game(&game);
    let gltf = server.id_for_path("assets/crate.gltf").expect("glTF in the pak");
    let model = server
        .load_model(regia_render::cooked_model::model_id(gltf))
        .expect("model");
    assert_eq!(model.parts.len(), 1);
    let handle = server
        .load_mesh(model.parts[0].mesh)
        .expect("the primitive's mesh");
    let mesh = server.meshes().get(&handle).expect("mesh");
    assert_eq!(mesh.colors.len(), 4, "COLOR_0 survives the GPU-ready pak entry");
    // The cook reorders vertices (vertex-fetch optimization): pair each loaded vertex with the
    // authored one at the same position (`write_project`'s four distinct corners). The glTF has no
    // `.meta` `forward`, so the cook bakes the project default `Convert`, a 180° yaw about +Y
    // (51ef45894, `GltfForward::Convert`): an authored corner (x, y, z) loads at (-x, y, -z).
    let authored: [[f32; 3]; 4] = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ]
    .map(|[x, y, z]| [-x, y, -z]);
    for (position, got) in mesh.positions.iter().zip(&mesh.colors) {
        let at = authored
            .iter()
            .position(|a| a.iter().zip(position).all(|(a, p)| (a - p).abs() <= 1e-3))
            .unwrap_or_else(|| panic!("loaded vertex {position:?} is not an authored corner"));
        for (g, w) in got.iter().zip(colors[at]) {
            // The cooked stream is unorm16.
            assert!((g - w).abs() <= 1.0 / 65535.0, "{position:?}: {:?}", mesh.colors);
        }
    }
    load_cooked_model(&mut server, gltf).expect("the model loads from the pak");
    let forms = server.load_forms();
    assert_eq!(forms.compact_meshes + forms.legacy_meshes, 0, "{forms:?}");
    assert!(forms.gpu_meshes >= 1, "{forms:?}");
    let _ = std::fs::remove_dir_all(&root);
}
