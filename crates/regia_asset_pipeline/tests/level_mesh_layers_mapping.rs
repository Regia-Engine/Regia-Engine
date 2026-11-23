// SPDX-License-Identifier: MPL-2.0
//! ADR-8842 (2): the mesh-layer cook of `regia cook` builds its sim ground from the cooked count
//! mapping (ADR-8471) when the terrain index carries one, the way the run-time level load builds the
//! terrain, so the sim ground and the drawn terrain cannot disagree. Before, the cook read the
//! level's width, scale and offset and ignored `LevelGround::source_mapping`: a count source cooked
//! at the wrong width and heights. A mapping whose grid center is not the origin is refused (the
//! canonical lattice is centered on the origin), never cooked offset.
//!
//! Fixtures are original: small square count grids with a nonzero datum, built here. CPU only.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regia_asset::cooked_cache_dir;
use regia_asset_pipeline::level_mesh_layers::{LevelMeshLayersJob, level_job};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_scene::format::SceneFile;
use regia_scene::level::{Level, LevelId, LevelSettings, LevelTerrain};

const PLATFORM: &str = "linux";
const LAYER_REL: &str = "maps/ridge/ridge.terrain.layers.ron";
const LEVEL: &str = "maps/ridge.level.ron";
const N: u32 = 33;
/// Meters per count and the datum: every height `(c - 1000) * 0.125` is exact in `f32`, so the
/// cook's `offset + c * step` and the load's `(c - zero) * mpc` agree bit for bit.
const MPC: f32 = 0.125;
const ZERO: u16 = 1000;
const CELL: f32 = 2.0;

fn counts() -> Vec<u16> {
    (0..N)
        .flat_map(|r| (0..N).map(move |c| 900 + ((c * 37 + r * 91) % 263) as u16))
        .collect()
}

/// A project with a schema-2 count source (first sample at `sample0` on X and Z), a level whose
/// terrain block gives legacy numbers a count source must ignore (and `world_width_m`), and a
/// `.meshlayers.ron` next to the map. Cooks the terrain, then writes the layers file (the job only
/// needs it to exist), and returns the root.
fn project(tag: &str, sample0: f32, world_width_m: Option<f32>) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    let dir = root.join("assets/maps/ridge");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        root.join("assets").join(LAYER_REL),
        include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    )
    .unwrap();
    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(N, N, counts())
        .unwrap()
        .save(dir.join("heightfield.png"))
        .unwrap();
    std::fs::write(
        dir.join("heightfield.json"),
        format!(
            r#"{{"schema": 2,
  "grid": {{"cols": {N}, "rows": {N}, "cell_m": {CELL}, "sample0_x_m": {sample0}, "sample0_z_m": {sample0},
            "row0": "south", "col0": "west"}},
  "units": {{"kind": "counts", "meters_per_count": {MPC}, "zero_counts": {ZERO}}}}}"#
        ),
    )
    .unwrap();
    let level = Level {
        id: LevelId(regia_core::id::Guid(0x8842)),
        settings: LevelSettings {
            terrain: Some(block(world_width_m)),
            ..LevelSettings::default()
        },
        scene: SceneFile::new(),
    };
    std::fs::write(root.join("assets").join(LEVEL), level.to_ron_string().unwrap()).unwrap();
    let report = cook_project(CookOptions {
        project_root: root.clone(),
        platform: PLATFORM.into(),
        incremental: false,
    })
    .expect("cook");
    // The level file is not a cook source: it ships raw, and the cook names it the way it names
    // every raw file (`regia cook` reads that as raw, `commands/cook.rs`).
    let errors: Vec<_> = report
        .errors
        .iter()
        .filter(|(_, e)| !e.starts_with("unsupported source extension"))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    std::fs::write(
        root.join("assets/maps/ridge.meshlayers.ron"),
        "(version: 1, layers: [])",
    )
    .unwrap();
    root
}

fn block(world_width_m: Option<f32>) -> LevelTerrain {
    LevelTerrain {
        layers: LAYER_REL.into(),
        map: "maps/ridge".into(),
        world_width_m,
        // A count source ignores these (ADR-8471); the old cook used them.
        height_scale_m: 40.0,
        height_offset_m: -10.0,
        // Vertex-centered, as every level before ADR-10079.
        sample_layout: regia_scene::level::TerrainSampleLayout::VertexCentered,
    }
}

fn job(
    root: &Path,
) -> Result<Option<LevelMeshLayersJob>, regia_asset_pipeline::level_mesh_layers::LevelMeshLayersError> {
    let assets = root.join("assets");
    level_job(
        &assets,
        &cooked_cache_dir(root, PLATFORM),
        &assets.join(LEVEL),
        LEVEL,
    )
}

/// A centered count source whose cell, scale and datum all differ from the legacy defaults: the
/// cooked sim ground has the run-time terrain's width, and every lattice height is the run-time
/// height of its sample, bit for bit.
#[test]
fn the_mesh_layer_ground_follows_the_cooked_count_mapping() {
    // First sample at -(n - 1) * cell / 2: the grid is centered on the origin.
    let sample0 = -((N - 1) as f32) * CELL * 0.5;
    let root = project("meshlayers-mapping-centered", sample0, None);
    let loaded = regia_terrain::load_level_terrain(
        &root.join("assets"),
        &cooked_cache_dir(&root, PLATFORM),
        LEVEL,
        Some(&block(None)),
    )
    .expect("the level loads the count source");
    assert!(loaded.source_mapping.is_some(), "the index carries the mapping");
    let map = loaded.terrain.expect("terrain").map;
    assert_eq!(map.grid_origin(), [0.0, 0.0], "a centered source");

    let job = job(&root)
        .expect("the job reads")
        .expect("the map has mesh layers");
    let g = &job.ground;
    assert_eq!(g.lattice_n, N);
    assert_eq!(
        g.width_m.to_bits(),
        map.world_width().to_bits(),
        "sim ground width {} vs the run-time terrain's {} (the cook ignored the count mapping)",
        g.width_m,
        map.world_width()
    );
    assert_eq!(g.width_m, 64.0);
    assert_eq!(g.step_m.to_bits(), MPC.to_bits(), "one count is meters_per_count");
    assert_eq!(
        g.offset_m.to_bits(),
        (-(f32::from(ZERO) * MPC)).to_bits(),
        "count 0 sits under the datum"
    );
    assert_eq!(g.counts, counts(), "counts verbatim");
    for row in 0..N {
        for col in 0..N {
            let c = g.counts[(row * N + col) as usize];
            let sim = g.offset_m + f32::from(c) * g.step_m;
            let drawn = map.height_at_sample(i64::from(col), i64::from(row));
            assert_eq!(
                sim.to_bits(),
                drawn.to_bits(),
                "sample ({col}, {row}): sim {sim} m vs drawn {drawn} m"
            );
        }
    }
    // The lattice's first and last vertices sit on the run-time terrain's first and last samples.
    let half = g.width_m * 0.5;
    assert_eq!(map.sample_to_world(0, 0), [-half, -half]);
    assert_eq!(map.sample_to_world(N - 1, N - 1), [half, half]);
    let _ = std::fs::remove_dir_all(&root);
}

/// A count source whose grid center is not the origin loads at run time (ADR-8179 grid origin),
/// but the canonical lattice is centered on the origin: the cook refuses it with
/// `meshlayers_ground_offset` instead of cooking a sim ground 45 m away from the drawn terrain.
#[test]
fn a_noncentered_count_source_is_refused_not_cooked_offset() {
    let root = project("meshlayers-mapping-offset", 13.0, None);
    let loaded = regia_terrain::load_level_terrain(
        &root.join("assets"),
        &cooked_cache_dir(&root, PLATFORM),
        LEVEL,
        Some(&block(None)),
    )
    .expect("the run-time load holds a noncentered grid");
    let center = loaded.terrain.expect("terrain").map.grid_origin();
    assert_eq!(center, [45.0, 45.0]);
    match job(&root) {
        Err(e) => {
            assert_eq!(e.code(), "meshlayers_ground_offset", "{e}");
            assert!(e.to_string().contains("45"), "the message names the center: {e}");
        }
        Ok(job) => panic!(
            "the cook made a sim ground centered on the origin while the drawn terrain is centered at {center:?}: {:?}",
            job.map(|j| (j.ground.width_m, j.ground.offset_m, j.ground.step_m))
        ),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A level `world_width_m` that contradicts the mapping is refused by the cook with the run-time
/// load's own code, so neither builds a ground the other would not.
#[test]
fn a_level_width_that_contradicts_the_mapping_is_refused_by_the_cook_too() {
    let sample0 = -((N - 1) as f32) * CELL * 0.5;
    let root = project("meshlayers-mapping-width", sample0, Some(50.0));
    let runtime = regia_terrain::load_level_terrain(
        &root.join("assets"),
        &cooked_cache_dir(&root, PLATFORM),
        LEVEL,
        Some(&block(Some(50.0))),
    )
    .map(|_| ())
    .map_err(|e| e.code());
    assert_eq!(runtime, Err("level_terrain_mapping"));
    match job(&root) {
        Err(e) => assert_eq!(e.code(), "level_terrain_mapping", "{e}"),
        Ok(job) => panic!(
            "the cook made a ground the run-time load refuses: {:?}",
            job.map(|j| j.ground.width_m)
        ),
    }
    let _ = std::fs::remove_dir_all(&root);
}
