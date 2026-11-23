// SPDX-License-Identifier: MIT-0
//! The tunnels showcase scene (ADR-8825): `tunnels_scene/` is its generator's output, it loads with the
//! Switchbacks tunnels armed (cooked mouths, the lattice cut, tube meshes and lights, indoor cells), and a
//! broken `tunnels/` set is refused with its own code. CPU only: no GPU.
//!
//! Write the scene: `REGIA_WRITE_TUNNELS_SCENE=1 cargo test -p showcase --test tunnels_scene`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regia_test_utils::unique_temp_dir;
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::passages::PassagesDocument;
use showcase::ErrorCode;
use showcase::features::FeatureOptions;
use showcase::scene::{SceneData, load_scene_with};
use showcase::tunnels::{TUNNELS_FILE, TunnelsFile, load_tunnels};
use showcase::tunnels_example::tunnels_files;

fn scene_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tunnels_scene")
}

fn passages() -> String {
    PassagesDocument::from_layout(for_map("switchbacks").expect("fixture"))
        .to_ron()
        .expect("passages ron")
}

/// The loaded scene, shared by the tests (the load cooks the tunnels once).
fn scene() -> &'static SceneData {
    static SCENE: OnceLock<SceneData> = OnceLock::new();
    SCENE.get_or_init(|| {
        load_scene_with(&scene_dir(), &FeatureOptions::default()).expect("the tunnels scene loads")
    })
}

/// A PNG's header fields and every decoded sample, or `None` when `bytes` is no PNG.
fn png_image(bytes: &[u8]) -> Option<(png::ColorType, png::BitDepth, u32, u32, Vec<u8>)> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some((info.color_type, info.bit_depth, info.width, info.height, buf))
}

/// Text files are compared byte for byte. A PNG is compared by its decoded image, as the example
/// scene's are (ADR-7386): the deflate stream is not the content, and `flate2`'s `zlib-rs` backend
/// picks its compressor's code path per CPU, so two hosts write other bytes for the same samples
/// (the v0.7.0 rc2 gate host did; ADR-10394).
#[test]
fn the_committed_tunnels_scene_matches_its_generator() {
    let files = tunnels_files(&passages()).expect("generator");
    if std::env::var_os("REGIA_WRITE_TUNNELS_SCENE").is_some() {
        showcase::tunnels_example::write_tunnels_scene(&scene_dir(), &passages()).expect("write");
    }
    let mut stale = Vec::new();
    for (rel, bytes) in &files {
        let committed = std::fs::read(scene_dir().join(rel)).unwrap_or_default();
        let same = if rel.ends_with(".png") {
            let generated = png_image(bytes);
            assert!(generated.is_some(), "the generator's {rel} does not decode");
            png_image(&committed) == generated
        } else {
            &committed == bytes
        };
        if !same {
            stale.push(*rel);
        }
    }
    assert!(
        stale.is_empty(),
        "tunnels_scene differs from its generator in {stale:?}; regenerate with REGIA_WRITE_TUNNELS_SCENE=1"
    );
}

#[test]
fn the_tunnels_scene_loads_armed() {
    let data = scene();
    assert!(
        data.features.names().contains(&"tunnels"),
        "{:?}",
        data.features.names()
    );
    let plan = data.features.tunnels.as_ref().expect("tunnels armed");
    eprintln!(
        "tunnels: {} mouths, {} meshes, {} triangles, {} lights, {} cells, {} portals, sim {} B, cooked {} B",
        plan.mouths.len(),
        plan.meshes.len(),
        plan.triangles,
        plan.lights.len(),
        plan.viscells.cells.len(),
        plan.viscells.portals.len(),
        plan.sim_bytes,
        plan.cooked_bytes
    );
    // Switchbacks has four openings, and the document fills every one.
    let mut ids: Vec<&str> = plan.mouths.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["east", "mid_east", "mid_west", "west"]);
    for mouth in &plan.mouths {
        assert!(mouth.cut_triangles > 0, "mouth {} cuts the ground", mouth.id);
        assert!(mouth.ring_points >= 3, "mouth {} has a ring", mouth.id);
        let hole = plan.holes.named.iter().find(|h| h.id == mouth.id);
        assert!(
            hole.is_some_and(|h| h.open),
            "mouth {} is an open named hole",
            mouth.id
        );
    }
    // The terrain draws the cooked lattice cut (the heightfield is the cook's lattice).
    let cut = plan
        .lattice
        .as_ref()
        .expect("the lattice cut applies to the 4096² heightfield");
    for mouth in &plan.mouths {
        let [x, _, z] = mouth.centre;
        let inside_cut = (-8..=8).any(|dx| (-8..=8).any(|dz| cut.cuts(x + dx as f32, z + dz as f32)));
        assert!(inside_cut, "the drawn cut opens near mouth {}", mouth.id);
    }
    assert!(
        !plan.meshes.is_empty() && plan.triangles > 1000,
        "the tubes have a render mesh"
    );
    assert!(
        plan.lights.len() >= 10,
        "the tubes are lit: {} lights",
        plan.lights.len()
    );
    for light in &plan.lights {
        assert!(light[1] < -256.0, "a tube light sits under the plain: {light:?}");
    }
    assert!(
        plan.viscells.cells.len() > 1,
        "indoor cells beside the outdoor cell"
    );
    assert!(
        plan.viscells.portals.len() >= plan.mouths.len(),
        "a portal per mouth at least"
    );
}

/// The view pairs of `perf_views.json`: each tube view's eye is in an indoor cell, its outdoor twin's on the
/// plain in the outdoor cell, both looking the same way over the same XZ.
#[test]
fn the_view_pairs_are_inside_and_above_the_same_tube() {
    let data = scene();
    let plan = data.features.tunnels.as_ref().expect("tunnels armed");
    let text = std::fs::read_to_string(scene_dir().join("perf_views.json")).expect("views");
    let views = showcase::perf_table::parse_views(&text).expect("views parse");
    let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
    for pair in ["west", "east"] {
        let tube = views
            .iter()
            .find(|v| v.name == format!("tube_{pair}"))
            .expect("tube view");
        let out = views
            .iter()
            .find(|v| v.name == format!("outdoor_{pair}"))
            .expect("outdoor view");
        let key = |v: &showcase::perf_table::View, k: usize| -> [f32; 3] {
            let p = &v.params["path"]["keys"][k]["position"];
            [0, 1, 2].map(|i| p[i].as_f64().expect("number") as f32)
        };
        for k in 0..2 {
            let (t, o) = (key(tube, k), key(out, k));
            assert_eq!([t[0], t[2]], [o[0], o[2]], "{pair} key {k}: the same XZ");
            assert!(
                o[1] > t[1] + 15.0,
                "{pair} key {k}: the outdoor eye is over the rock"
            );
            let cell = plan.viscells.locate_index(regia_core::math::Vec3::from_array(t));
            let outdoor = plan.viscells.index_of(regia_render::viscells::OUTDOOR_CELL);
            assert_ne!(
                Some(cell),
                outdoor,
                "{pair} key {k}: the tube eye is in an indoor cell"
            );
        }
    }
    assert!(names.contains(&"mouth_drive"), "{names:?}");
}

#[test]
fn a_broken_tunnels_file_is_refused_with_its_code() {
    let path = Path::new(TUNNELS_FILE);
    let good = std::fs::read_to_string(scene_dir().join(TUNNELS_FILE)).expect("tunnels.ron");
    assert!(TunnelsFile::from_ron(&good, path).is_ok());
    for (from, to) in [
        ("version: 1", "version: 2"),
        (
            "\"switchbacks.meshlayers.ron\"",
            "\"../switchbacks.meshlayers.ron\"",
        ),
        ("spacing_m: 40.0", "spacing_m: 1.0"),
        ("version: 1", "version: 1, extra: 0"),
    ] {
        let broken = good.replacen(from, to, 1);
        let err = TunnelsFile::from_ron(&broken, path).expect_err(to);
        assert_eq!(err.code, ErrorCode::TunnelsMalformed, "{to}: {err}");
        assert_eq!(err.code.as_str(), "tunnels_malformed");
    }
}

/// A small scene directory with `tunnels.ron` naming `document`, and a square flat map of `n` samples.
fn small_tunnels(tag: &str, document: &str, n: u32) -> (PathBuf, regia_terrain::TerrainMap) {
    let dir = unique_temp_dir(tag);
    let good = std::fs::read_to_string(scene_dir().join(TUNNELS_FILE)).expect("tunnels.ron");
    std::fs::create_dir_all(dir.join("tunnels")).expect("dir");
    std::fs::write(dir.join(TUNNELS_FILE), good).expect("write");
    std::fs::write(dir.join("tunnels/switchbacks.meshlayers.ron"), document).expect("write");
    let heights = vec![-256.0; (n * n) as usize];
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 1280.0, 1.0, 0.0).expect("map");
    (dir, map)
}

#[test]
fn a_ground_off_the_lattice_or_a_failed_cook_is_refused() {
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"missing.passages.ron\"))])\n";
    // Not square.
    let (dir, _) = small_tunnels("tunnels-ground", doc, 64);
    let wide = regia_terrain::TerrainMap::from_heights(64, 32, vec![-256.0; 64 * 32], 1280.0, 1.0, 0.0)
        .expect("map");
    let err = load_tunnels(&dir, &wide).expect_err("not square");
    assert_eq!(err.code, ErrorCode::TunnelsGroundMismatch, "{err}");
    // A sample between two counts (half a count up).
    let off =
        regia_terrain::TerrainMap::from_heights(64, 64, vec![-256.0 + 1.0 / 64.0; 64 * 64], 1280.0, 1.0, 0.0)
            .expect("map");
    let err = load_tunnels(&dir, &off).expect_err("off the counts");
    assert_eq!(err.code, ErrorCode::TunnelsGroundMismatch, "{err}");
    assert_eq!(err.code.as_str(), "tunnels_ground_mismatch");
    // The document names a passages file that is not there: the cook refuses it.
    let (dir, map) = small_tunnels("tunnels-cook", doc, 64);
    let err = load_tunnels(&dir, &map).expect_err("missing passages");
    assert_eq!(err.code, ErrorCode::TunnelsCookFailed, "{err}");
    assert!(err.message.contains("input_missing"), "{err}");
    assert_eq!(err.code.as_str(), "tunnels_cook_failed");
    // No tunnels.ron: off, as before.
    let empty = unique_temp_dir("tunnels-off");
    assert!(load_tunnels(&empty, &map).expect("no file").is_none());
}
