// SPDX-License-Identifier: MPL-2.0
//! The live cooker and overlay asset roots (ADR-8845): it fills a glTF's missing buffer and image
//! from `[cook.overlay_roots]` in the batch cook's order and writes the cooked output the batch
//! cook writes, and it watches the overlay roots.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use image::ImageEncoder;
use regia_asset::{ReloadMessage, WatchOutcome, cooked_cache_dir};
use regia_asset_pipeline::{
    CookOptions, LiveCooker, LiveCookerConfig, cook_project, cook_sources, host_platform,
};

/// A 64x64 RGBA PNG whose pixels depend on `seed`.
fn png(seed: u8) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&[((x * 4) as u8) ^ seed, (y * 4) as u8, seed, 255]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

/// The quad's buffer, scaled by `scale` so two versions differ.
fn quad_bin(scale: f32) -> Vec<u8> {
    let positions: [[f32; 3]; 4] = [
        [-scale, 0.0, -scale],
        [scale, 0.0, -scale],
        [scale, 0.0, scale],
        [-scale, 0.0, scale],
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
    bin
}

const GLTF: &str = r#"{
  "asset": {"version": "2.0"},
  "scene": 0,
  "scenes": [{"nodes": [0]}],
  "nodes": [{"mesh": 0, "name": "Crate"}],
  "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0, "mode": 4}]}],
  "materials": [{"name": "crate", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
  "textures": [{"source": 0}],
  "images": [{"uri": "crate_albedo.png"}],
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

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, bytes).expect("write");
}

/// A project whose `props/crate.gltf` is in `assets/` and whose overlay roots are `layers/base`
/// (holding a decoy buffer) then `layers/hd`.
fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    write(
        &root.join("Regia.toml"),
        b"[project]\nname = \"overlay\"\n\n[cook.overlay_roots]\npaths = [\"layers/base\", \"layers/hd\"]\n",
    );
    write(&root.join("assets/props/crate.gltf"), GLTF.as_bytes());
    write(&root.join("layers/base/props/other.bin"), b"decoy");
    // The directory exists before any watcher starts, so a new file in it is a plain create event.
    std::fs::create_dir_all(root.join("layers/hd/props")).expect("hd");
    root
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

/// Every `.fckd` in the cook cache: name -> bytes.
fn cooked_blobs(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let Ok(dir) = std::fs::read_dir(cache(root)) else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".fckd") {
            out.insert(name, std::fs::read(entry.path()).expect("read"));
        }
    }
    out
}

#[test]
fn the_live_cook_fills_from_the_overlay_and_writes_what_the_batch_cook_writes() {
    let root = project("live_overlay_parity");
    write(&root.join("layers/hd/props/crate.bin"), &quad_bin(1.0));
    write(&root.join("layers/hd/props/crate_albedo.png"), &png(3));
    let gltf = root.join("assets/props/crate.gltf");

    // The live path first: only the glTF is a target.
    let options = CookOptions::for_host(root.clone());
    let targets: BTreeSet<PathBuf> = std::iter::once(gltf.clone()).collect();
    let results = cook_sources(&options, &targets).expect("live cook");
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].error, None, "the live cook reads the overlay payload");
    assert!(!results[0].ids.is_empty());
    let live = cooked_blobs(&root);
    for id in &results[0].ids {
        assert!(
            live.contains_key(&format!("{id}.fckd")),
            "{id} cooked: {:?}",
            live.keys()
        );
    }
    assert_eq!(
        std::fs::read(root.join("assets/props/crate.bin")).expect("filled"),
        quad_bin(1.0),
        "filled from layers/hd, not the decoy root"
    );

    // The batch path on the same project: remove the filled payloads and the cache (the `.meta`
    // sidecars stay, so the ids stay), then cook the whole project.
    std::fs::remove_file(root.join("assets/props/crate.bin")).expect("rm bin");
    std::fs::remove_file(root.join("assets/props/crate_albedo.png")).expect("rm png");
    std::fs::remove_dir_all(cache(&root)).expect("rm cache");
    let report = cook_project(options.clone()).expect("batch cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.overlay_fill.files, 2);
    let batch = cooked_blobs(&root);
    for (name, bytes) in &live {
        assert_eq!(
            batch.get(name),
            Some(bytes),
            "{name}: the batch cook writes the same blob"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_project_payload_wins_over_the_overlay_in_the_live_cook() {
    let root = project("live_overlay_project_wins");
    write(&root.join("assets/props/crate.bin"), &quad_bin(2.0));
    write(&root.join("assets/props/crate_albedo.png"), &png(5));
    write(&root.join("layers/hd/props/crate.bin"), &quad_bin(1.0));
    let options = CookOptions::for_host(root.clone());
    let targets: BTreeSet<PathBuf> = std::iter::once(root.join("assets/props/crate.gltf")).collect();
    let results = cook_sources(&options, &targets).expect("live cook");
    assert_eq!(results[0].error, None);
    assert_eq!(
        std::fs::read(root.join("assets/props/crate.bin")).expect("kept"),
        quad_bin(2.0),
        "the project's own buffer is not replaced"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_bad_overlay_table_fails_the_live_cook_as_it_fails_the_batch_cook() {
    let root = project("live_overlay_bad_table");
    write(
        &root.join("Regia.toml"),
        b"[project]\nname = \"overlay\"\n\n[cook.overlay_roots]\npaths = 3\n",
    );
    let options = CookOptions::for_host(root.clone());
    let targets: BTreeSet<PathBuf> = std::iter::once(root.join("assets/props/crate.gltf")).collect();
    let error = cook_sources(&options, &targets).expect_err("bad table");
    assert!(error.to_string().contains("[cook.overlay_roots]"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_new_overlay_payload_recooks_the_gltf_through_the_live_cooker() {
    let root = project("live_overlay_watch");
    write(&root.join("assets/props/crate_albedo.png"), &png(7));
    let (tx, rx) = std::sync::mpsc::channel();
    let cooker = LiveCooker::spawn(LiveCookerConfig::for_host(root.clone()), tx).expect("live cooker");
    assert!(
        cooker.roots().iter().any(|r| r.ends_with("layers/hd")),
        "overlay roots are watched: {:?}",
        cooker.roots()
    );
    // The hole is filled by a file that appears in the overlay root while the cooker runs.
    write(&root.join("layers/hd/props/crate.bin"), &quad_bin(1.0));
    // The bound only ends a failing run; the pass condition is the message itself.
    let give_up = Instant::now() + Duration::from_secs(30);
    let mut reloaded = false;
    while !reloaded {
        let left = give_up.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ReloadMessage::Status(entry)) if entry.path == "assets/props/crate.gltf" => {
                assert!(matches!(entry.outcome, WatchOutcome::Reloaded), "{entry:?}");
                reloaded = true;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(cooker);
    assert!(
        reloaded,
        "no live cook of props/crate.gltf after its overlay buffer appeared"
    );
    assert_eq!(
        std::fs::read(root.join("assets/props/crate.bin")).expect("filled"),
        quad_bin(1.0)
    );
    let _ = std::fs::remove_dir_all(&root);
}
