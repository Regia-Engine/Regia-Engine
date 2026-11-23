// SPDX-License-Identifier: MIT-0
//! Scene loading: the example scene loads, every kind of bad scene is refused with its error code
//! (never a panic), the committed example matches its generator, scatter placement is
//! deterministic and the book page documents every error code. No GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regia_test_utils::unique_temp_dir;
use showcase::app::scatter_entities;
use showcase::example::{LANDING, example_files, write_example_scene};
use showcase::scene::{SceneData, load_scene};
use showcase::{ErrorCode, ShowcaseError};

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

/// A fresh copy of the generated example scene.
fn fresh(tag: &str) -> PathBuf {
    let dir = unique_temp_dir(tag);
    write_example_scene(&dir).expect("write example");
    dir
}

/// Replaces the text of `rel` inside `dir`.
fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).expect("read")
}

fn load_err(dir: &Path) -> ShowcaseError {
    load_scene(dir).expect_err("the scene must be refused")
}

fn loaded(dir: &Path) -> SceneData {
    load_scene(dir).expect("scene loads")
}

/// A PNG's header fields and every decoded sample.
fn png_image(bytes: &[u8], what: &str) -> (png::ColorType, png::BitDepth, u32, u32, Vec<u8>) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    buf.truncate(info.buffer_size());
    (info.color_type, info.bit_depth, info.width, info.height, buf)
}

/// Text files are compared byte for byte. A PNG is compared by its decoded image: the deflate
/// stream is not the content, and `flate2`'s `zlib-rs` backend picks its compressor's code path
/// per CPU, so an aarch64 host (the M5) writes other bytes for the same samples (ADR-7386).
#[test]
fn the_committed_example_matches_its_generator() {
    for (rel, bytes) in example_files().expect("generate") {
        let committed = std::fs::read(example_dir().join(rel))
            .unwrap_or_else(|e| panic!("example_scene/{rel}: {e} (regenerate with --write-example)"));
        let hint = "run `cargo run -p showcase -- --write-example samples/showcase/example_scene`";
        if rel.ends_with(".png") {
            assert!(
                png_image(&committed, rel) == png_image(&bytes, rel),
                "example_scene/{rel}'s image differs from the generator's; {hint}"
            );
        } else {
            assert_eq!(
                committed, bytes,
                "example_scene/{rel} differs from the generator; {hint}"
            );
        }
    }
}

/// ADR-5792: the example's plain decal is the dark core of its scorch prefab. At 9 m a side it was
/// an 18 m dark square around the landing site, read as a square shadow in every shot of it.
#[test]
fn the_examples_plain_decal_stays_inside_its_scorch() {
    let scene = loaded(&example_dir());
    let scorch = regia_vfx::decal_format::builtin_prefab("scorch").expect("the built-in scorch");
    assert!(
        scene
            .spawns
            .decals
            .iter()
            .any(|d| d.prefab.as_deref() == Some("scorch")),
        "the example places the scorch prefab"
    );
    let plain: Vec<_> = scene
        .spawns
        .decals
        .iter()
        .filter(|d| d.prefab.is_none())
        .collect();
    assert_eq!(
        plain.len(),
        1,
        "the example keeps one plain decal (the message path)"
    );
    for decal in plain {
        assert!(
            decal.half_size.0 <= scorch.half_size[0] && decal.half_size.1 <= scorch.half_size[1],
            "the plain decal ({:?} m) reaches past the scorch ({:?} m)",
            decal.half_size,
            scorch.half_size
        );
    }
}

#[test]
fn the_example_scene_loads_with_every_part() {
    let scene = loaded(&example_dir());
    assert_eq!((scene.map.width(), scene.map.height()), (257, 257));
    assert_eq!(scene.layers.layers.len(), 3);
    assert!(scene.material.is_some(), "procedural layer textures are baked");
    let set = scene.scatter.as_ref().expect("scatter set");
    assert_eq!(set.classes.len(), 2);
    assert_eq!(
        scene.library.len(),
        3,
        "rock_a, rock_b and pine have stand-in meshes"
    );
    let track = scene.track.as_ref().expect("track");
    assert_eq!(track.entity, "pod");
    assert_eq!(scene.spawns.effects.len() + scene.spawns.decals.len(), 5);
    for name in ["dust_trail", "explosion", "smoke"] {
        assert!(
            scene.vfx_defs.iter().any(|d| d.name == name),
            "effect {name} is available"
        );
    }
    assert_eq!(scene.sky.changes.len(), 1);
}

#[test]
fn a_scene_loads_twice_to_equal_data() {
    let a = loaded(&example_dir());
    let b = loaded(&example_dir());
    assert_eq!(a.map.heights(), b.map.heights());
    assert_eq!(a.layers, b.layers);
    assert_eq!(a.track, b.track);
    assert_eq!(a.spawns, b.spawns);
    assert_eq!(a.sky, b.sky);
}

#[test]
fn the_landing_area_is_flat() {
    let scene = loaded(&example_dir());
    let h = scene.map.sample_bilinear(LANDING.0, LANDING.1);
    for (dx, dz) in [(10.0, 0.0), (-10.0, 5.0), (0.0, -12.0), (8.0, 8.0)] {
        let other = scene.map.sample_bilinear(LANDING.0 + dx, LANDING.1 + dz);
        assert!(
            (h - other).abs() < 1.0,
            "landing area varies by {} m",
            (h - other).abs()
        );
    }
}

#[test]
fn a_missing_directory_is_scene_dir_missing() {
    let dir = unique_temp_dir("showcase-missing");
    let err = load_err(&dir.join("nope"));
    assert_eq!(err.code, ErrorCode::SceneDirMissing);
    // A file is not a directory either.
    write(&dir, "afile", "x");
    assert_eq!(load_err(&dir.join("afile")).code, ErrorCode::SceneDirMissing);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_without_a_manifest_is_scene_manifest_missing() {
    let dir = unique_temp_dir("showcase-empty");
    assert_eq!(load_err(&dir).code, ErrorCode::SceneManifestMissing);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_manifests_are_scene_manifest_malformed() {
    let dir = fresh("showcase-manifest");
    let good = read(&dir, "scene.ron");
    let cases: Vec<(&str, String)> = vec![
        ("garbage", "this is not ron".to_string()),
        ("future version", good.replace("version: 1", "version: 9")),
        (
            "zero width",
            good.replace("world_width: 1024.0", "world_width: 0.0"),
        ),
        ("unknown field", good.replacen("(\n", "(\n    surprise: 1,\n", 1)),
        ("absolute path", good.replace("\"sky.ron\"", "\"/etc/passwd\"")),
        ("parent path", good.replace("\"sky.ron\"", "\"../sky.ron\"")),
        ("empty path", good.replace("\"sky.ron\"", "\"\"")),
        (
            "fov out of range",
            good.replace("fov_deg: 45.0", "fov_deg: 400.0"),
        ),
    ];
    for (what, text) in cases {
        write(&dir, "scene.ron", &text);
        let err = load_err(&dir);
        assert_eq!(err.code, ErrorCode::SceneManifestMalformed, "{what}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_named_file_that_is_absent_is_scene_file_missing() {
    for rel in [
        "heightfield.png",
        "terrain.layers.ron",
        "scatter/example.scatter.ron",
        "sky.ron",
        "track.ron",
        "spawns.ron",
    ] {
        let dir = fresh("showcase-absent");
        std::fs::remove_file(dir.join(rel)).expect("remove");
        let err = load_err(&dir);
        assert_eq!(err.code, ErrorCode::SceneFileMissing, "{rel}: {err}");
        assert!(
            err.path.ends_with(rel),
            "{rel}: the error names the file: {}",
            err.path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn png_bytes(width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(data).expect("data");
        writer.finish().expect("finish");
    }
    out
}

#[test]
fn bad_heightfields_are_heightfield_malformed() {
    let dir = fresh("showcase-heightfield");
    let path = dir.join("heightfield.png");
    let eight = png_bytes(4, 4, png::ColorType::Grayscale, png::BitDepth::Eight, &[0; 16]);
    let rgb = png_bytes(4, 4, png::ColorType::Rgb, png::BitDepth::Eight, &[0; 48]);
    let tiny = png_bytes(1, 1, png::ColorType::Grayscale, png::BitDepth::Sixteen, &[0, 0]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("8-bit", eight),
        ("rgb", rgb),
        ("1x1", tiny),
        ("garbage", b"definitely not a png".to_vec()),
        ("empty", Vec::new()),
    ];
    for (what, bytes) in cases {
        std::fs::write(&path, bytes).expect("write");
        let err = load_err(&dir);
        assert_eq!(err.code, ErrorCode::HeightfieldMalformed, "{what}: {err}");
    }
    // A truncated real PNG.
    let good = example_files()
        .expect("files")
        .into_iter()
        .find(|(rel, _)| *rel == "heightfield.png")
        .expect("heightfield")
        .1;
    std::fs::write(&path, &good[..good.len() / 2]).expect("write");
    assert_eq!(load_err(&dir).code, ErrorCode::HeightfieldMalformed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_layers_scatter_sky_track_and_spawns_each_have_their_code() {
    let cases: [(&str, ErrorCode); 5] = [
        ("terrain.layers.ron", ErrorCode::LayersMalformed),
        ("scatter/example.scatter.ron", ErrorCode::ScatterMalformed),
        ("sky.ron", ErrorCode::SkyMalformed),
        ("track.ron", ErrorCode::TrackMalformed),
        ("spawns.ron", ErrorCode::SpawnsMalformed),
    ];
    for (rel, code) in cases {
        let dir = fresh("showcase-bad-file");
        write(&dir, rel, "garbage (((");
        let err = load_err(&dir);
        assert_eq!(err.code, code, "{rel}: {err}");
        assert!(err.path.ends_with(rel), "{rel}: {}", err.path.display());
        // An empty file is malformed too, not a panic.
        write(&dir, rel, "");
        assert_eq!(load_err(&dir).code, code, "{rel} empty");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn spawns_naming_unknown_things_are_refused() {
    let dir = fresh("showcase-spawns");
    let good = read(&dir, "spawns.ron");
    write(
        &dir,
        "spawns.ron",
        &good.replace("\"explosion\"", "\"made_up_effect\""),
    );
    let err = load_err(&dir);
    assert_eq!(err.code, ErrorCode::SpawnUnknownEffect, "{err}");
    assert!(err.message.contains("made_up_effect"));
    write(
        &dir,
        "spawns.ron",
        &good.replace("\"scorch\"", "\"made_up_decal\""),
    );
    let err = load_err(&dir);
    assert_eq!(err.code, ErrorCode::SpawnUnknownDecal, "{err}");
    assert!(err.message.contains("made_up_decal"));
    // The built-in prefabs and showcase effects are all accepted.
    write(&dir, "spawns.ron", &good);
    assert!(load_scene(&dir).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn entity_spawns_without_a_track_are_refused() {
    let dir = fresh("showcase-needs-track");
    edit_manifest(&dir, |m| m.track = None);
    assert_eq!(load_err(&dir).code, ErrorCode::SpawnNeedsTrack);
    // Without the spawn file too, a scene needs no track.
    edit_manifest(&dir, |m| m.spawns = None);
    let scene = loaded(&dir);
    assert!(scene.track.is_none() && scene.spawns.effects.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rewrites the manifest of `dir` through the engine's own types (no text matching).
fn edit_manifest(dir: &Path, edit: impl FnOnce(&mut showcase::scene::Manifest)) {
    let mut manifest: showcase::scene::Manifest =
        ron::from_str(&read(dir, "scene.ron")).expect("manifest parses");
    edit(&mut manifest);
    let text = ron::ser::to_string_pretty(&manifest, regia_core::ron_pretty()).expect("serialize");
    write(dir, "scene.ron", &text);
}

#[test]
fn a_material_block_must_match_the_layer_stack() {
    use showcase::scene::LayerMaterial;
    let dir = fresh("showcase-material");
    edit_manifest(&dir, |m| {
        m.material.as_mut().expect("material").layers.pop();
    });
    let err = load_err(&dir);
    assert_eq!(err.code, ErrorCode::MaterialsMalformed, "{err}");
    let _ = std::fs::remove_dir_all(&dir);

    let dir = fresh("showcase-material-style");
    edit_manifest(&dir, |m| {
        m.material.as_mut().expect("material").layers[0] = LayerMaterial::Style {
            style: "Lava".into(),
            size: 256,
            seed: 1,
        };
    });
    assert_eq!(load_err(&dir).code, ErrorCode::MaterialsMalformed);
    edit_manifest(&dir, |m| {
        m.material.as_mut().expect("material").layers[0] = LayerMaterial::Style {
            style: "Grass".into(),
            size: 100,
            seed: 1,
        };
    });
    assert_eq!(
        load_err(&dir).code,
        ErrorCode::MaterialsMalformed,
        "size must be a power of two"
    );
    // A texture file that is absent is a missing file; one that is not a PNG is a materials error.
    edit_manifest(&dir, |m| {
        m.material.as_mut().expect("material").layers[0] = LayerMaterial::Files {
            albedo: "textures/none.png".into(),
            normal: None,
            height: None,
            orm: None,
        };
    });
    assert_eq!(load_err(&dir).code, ErrorCode::SceneFileMissing);
    write(&dir, "textures/none.png", "not a png");
    assert_eq!(load_err(&dir).code, ErrorCode::MaterialsMalformed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn texture_files_load_as_layer_materials() {
    use showcase::scene::LayerMaterial;
    let dir = fresh("showcase-material-files");
    let side = 64u32;
    let rgb: Vec<u8> = (0..side * side).flat_map(|i| [(i % 251) as u8, 90, 60]).collect();
    let bytes = png_bytes(side, side, png::ColorType::Rgb, png::BitDepth::Eight, &rgb);
    std::fs::create_dir_all(dir.join("textures")).expect("mkdir");
    std::fs::write(dir.join("textures/albedo.png"), bytes).expect("write");
    edit_manifest(&dir, |m| {
        let layers = &mut m.material.as_mut().expect("material").layers;
        for layer in layers.iter_mut() {
            *layer = LayerMaterial::Files {
                albedo: "textures/albedo.png".into(),
                normal: None,
                height: None,
                orm: None,
            };
        }
    });
    let scene = loaded(&dir);
    let material = scene.material.expect("material");
    assert_eq!(material.layers.len(), 3);
    let first = material.layers[0].as_ref().expect("textures");
    assert_eq!((first.albedo.width, first.albedo.height), (side, side));
    assert!(!first.use_orm);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn error_codes_are_lowercase_ascii_digits_and_underscores_and_unique() {
    let mut seen = std::collections::HashSet::new();
    for code in ErrorCode::ALL {
        let text = code.as_str();
        assert!(!text.is_empty());
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{text}"
        );
        assert!(seen.insert(text), "{text} is listed twice");
    }
}

#[test]
fn the_book_page_documents_every_error_code() {
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/agents/showcase.md");
    let text = std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
    for code in ErrorCode::ALL {
        assert!(
            text.contains(&format!("`{}`", code.as_str())),
            "book/agents/showcase.md does not document `{}`",
            code.as_str()
        );
    }
}

#[test]
fn scatter_placement_is_deterministic_bounded_and_clear_of_the_landing() {
    let scene = loaded(&example_dir());
    let (a, placed_a) = scatter_entities(&scene).expect("place");
    let (b, placed_b) = scatter_entities(&scene).expect("place again");
    assert_eq!(a, b, "placement is a pure function of the scene");
    assert_eq!(placed_a, placed_b);
    assert!(placed_a > 200, "the example places {placed_a} instances");
    assert!(a.len() <= scene.manifest.max_scatter_instances as usize);
    assert!(a.iter().any(|e| e.mesh == "pine") && a.iter().any(|e| e.mesh == "rock_a"));
    for e in &a {
        let t = e.transform.translation;
        let d = ((t.x - LANDING.0).powi(2) + (t.z - LANDING.1).powi(2)).sqrt();
        assert!(
            d >= 44.0,
            "instance {d} m from the landing centre (exclusion is 45 m)"
        );
        let ground = scene.map.sample_bilinear(t.x, t.z);
        assert!(
            (t.y - ground).abs() < 3.0 + e.transform.scale.x,
            "instance floats {} m above the ground",
            t.y - ground
        );
    }
}

#[test]
fn scatter_is_thinned_evenly_to_the_cap() {
    let mut scene = loaded(&example_dir());
    let (all, placed) = scatter_entities(&scene).expect("place");
    assert_eq!(all.len(), placed);
    scene.manifest.max_scatter_instances = 100;
    let (few, placed_again) = scatter_entities(&scene).expect("place");
    assert_eq!(placed_again, placed);
    assert_eq!(few.len(), 100);
    // The kept instances are a subsequence of the full list.
    let mut it = all.iter();
    assert!(few.iter().all(|f| it.any(|a| a == f)), "thinning keeps the order");
}

/// Writes a one-triangle glTF (+ `.bin`) at `dir/rel`, 1 m tall, pivot at the base, under a root node that scales
/// it by 2.
fn write_triangle_gltf(dir: &Path, rel: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let positions: [f32; 9] = [-0.5, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.5, 0.0];
    let normals: [f32; 9] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0];
    let mut bin: Vec<u8> = Vec::new();
    for v in positions.iter().chain(&normals) {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    for i in [0u16, 1, 2, 0] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let stem = path.file_stem().expect("stem").to_string_lossy().into_owned();
    std::fs::write(path.with_file_name(format!("{stem}.bin")), &bin).expect("bin");
    let json = format!(
        r#"{{"asset": {{"version": "2.0"}}, "scene": 0, "scenes": [{{"nodes": [1]}}],
  "nodes": [{{"name": "{stem}", "mesh": 0}}, {{"name": "{stem}_root", "children": [0], "scale": [2, 2, 2]}}],
  "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0, "NORMAL": 1}}, "indices": 2}}]}}],
  "buffers": [{{"uri": "{stem}.bin", "byteLength": {len}}}],
  "bufferViews": [{{"buffer": 0, "byteOffset": 0, "byteLength": 36}}, {{"buffer": 0, "byteOffset": 36, "byteLength": 36}},
                  {{"buffer": 0, "byteOffset": 72, "byteLength": 6}}],
  "accessors": [{{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-0.5, 0, 0], "max": [0.5, 0.5, 0]}},
                {{"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"}},
                {{"bufferView": 2, "componentType": 5123, "count": 3, "type": "SCALAR"}}]}}"#,
        len = bin.len()
    );
    std::fs::write(&path, json).expect("gltf");
}

#[test]
fn gltf_scatter_meshes_load_beside_the_scatter_file() {
    let dir = fresh("showcase-gltf-scatter");
    let set = read(&dir, "scatter/example.scatter.ron").replace("\"rock_a\"", "\"meshes/tri/tri.gltf\"");
    write(&dir, "scatter/example.scatter.ron", &set);
    write_triangle_gltf(&dir, "scatter/meshes/tri/tri.gltf");
    let scene = loaded(&dir);
    let model = scene
        .models
        .get("meshes/tri/tri.gltf")
        .expect("the glTF is a model");
    // No stand-in for it: the library entry (what the streamed field draws, ADR-1415) is the glTF's own triangle
    // (a stand-in rock has hundreds), with the generated coarse levels of a one-level file.
    let asset = scene
        .library
        .get("meshes/tri/tri.gltf")
        .expect("the streamed field's asset");
    assert_eq!(asset.lods[0].indices.len(), 3, "no stand-in for it");
    assert!(asset.parts.is_empty());
    assert_eq!(model.parts.len(), 1);
    assert_eq!(model.triangles_lod0, 1);
    let part = &model.parts[0];
    let top = part
        .mesh
        .vertices
        .iter()
        .map(|v| v.position[1])
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        (top - 1.0).abs() < 1e-5,
        "the root node's scale applies: top at {top}"
    );
    assert_eq!(part.lods.levels.len(), 1, "a triangle has no coarser level");
    let (instances, _) = scatter_entities(&scene).expect("placement");
    assert!(
        instances.iter().any(|i| i.mesh == "meshes/tri/tri.gltf"),
        "instances use the glTF mesh name"
    );

    // A stand-in binding still wins over the file.
    edit_manifest(&dir, |m| {
        m.stand_ins.push(showcase::scene::StandInBinding {
            mesh: "meshes/tri/tri.gltf".into(),
            stand_in: showcase::scene::StandIn::Rock {
                color: (0.2, 0.2, 0.2),
            },
        });
    });
    let scene = loaded(&dir);
    assert!(scene.models.is_empty() && scene.library.contains_key("meshes/tri/tri.gltf"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_and_malformed_gltf_scatter_meshes_have_their_codes() {
    let dir = fresh("showcase-gltf-bad");
    let set = read(&dir, "scatter/example.scatter.ron").replace("\"rock_a\"", "\"meshes/tri/tri.glb\"");
    write(&dir, "scatter/example.scatter.ron", &set);
    assert_eq!(load_err(&dir).code, ErrorCode::ScatterMeshMissing);
    write(&dir, "scatter/meshes/tri/tri.glb", "not a glb");
    assert_eq!(load_err(&dir).code, ErrorCode::ScatterMeshMalformed);
    let _ = std::fs::remove_dir_all(&dir);
}
