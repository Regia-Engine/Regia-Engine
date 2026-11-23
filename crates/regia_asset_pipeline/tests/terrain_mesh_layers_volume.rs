// SPDX-License-Identifier: MPL-2.0
//! ADR-10071 (spec 26 § 2a.9 `Volume`, cites ADR-7974): a signed-distance volume cooks as a mesh
//! layer through the same contract as an authored one. The volume is meshed by dual contouring,
//! clipped under the LOD-0 ground, its clip loop at a filled hole becomes the mouth (lattice cut,
//! collar, zipper), its LOD-0 mesh is the query mesh, and its chunks and LODs are cut and locked
//! like any layer's.
//!
//! Fixture: a spherical hollow (a cave) under flat synthetic ground, breaking the surface in one
//! round mouth. No DO27 data.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regia_asset::{AssetId, cooked_file_name, load_mesh_bytes};
use regia_asset_pipeline::terrain_mesh_layers::{
    MaterialResolveError, MeshLayersCookError, MeshLayersCookOutcome, MeshLayersCookRequest, VolumeSource,
    cook_mesh_layers, cook_mesh_layers_with_volumes,
};
use regia_terrain::CdlodConfig;
use regia_terrain::volume_mesh::SdfField;
use regia_world_geom::cook::GroundInput;
use regia_world_geom::{SurfaceKind, WorldGeom};

const WIDTH_M: f32 = 128.0;
const N: u32 = 257;
/// The cave: a hollow sphere whose top breaks the flat ground at y = 0.
/// Off the lattice and cell grid lines, and across the 16 m level-0 node lines at x = 0 and z = 0,
/// so the cave spans four chunks.
const CENTER: [f32; 3] = [0.25, -1.0, 0.75];
const RADIUS: f32 = 4.0;
const CELL_M: f32 = 0.5;

/// A spherical hollow in solid rock: negative (solid) outside the sphere.
struct Cave;

impl SdfField for Cave {
    fn sample(&self, p: [f32; 3]) -> f32 {
        let d = [p[0] - CENTER[0], p[1] - CENTER[1], p[2] - CENTER[2]];
        RADIUS - (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }

    fn gradient(&self, p: [f32; 3]) -> [f32; 3] {
        let d = [p[0] - CENTER[0], p[1] - CENTER[1], p[2] - CENTER[2]];
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
        [-d[0] / l, -d[1] / l, -d[2] / l]
    }
}

/// Flat ground at y = 0, a 0.5 m lattice.
fn flat_ground() -> GroundInput {
    GroundInput {
        lattice_n: N,
        width_m: WIDTH_M,
        offset_m: 0.0,
        step_m: 1.0 / 32.0,
        counts: vec![0; (N * N) as usize],
    }
}

struct Cooked {
    dir: PathBuf,
    outcome: MeshLayersCookOutcome,
    file: Vec<u8>,
}

impl Cooked {
    fn cooked_dir(&self) -> PathBuf {
        self.dir.join("cooked")
    }
}

fn document(fills: &str, chunk_m: f32) -> String {
    format!(
        "(version: 1, chunk_m: {chunk_m:.1}, layers: [(id: \"cave\", source: Volume(sdf: \"cave\", iso: 0.0, cell_m: {CELL_M:.2}), fills: [{fills}])])"
    )
}

fn try_cook(
    tag: &str,
    doc: &str,
    with_field: bool,
) -> (PathBuf, Result<MeshLayersCookOutcome, MeshLayersCookError>) {
    let dir = regia_test_utils::unique_temp_dir(tag);
    let ground = flat_ground();
    let out = dir.join("assets/maps/cave.meshlayers");
    let cooked = dir.join("cooked");
    let req = MeshLayersCookRequest {
        document: doc.as_bytes(),
        ground: &ground,
        terrain: None,
        read_file: &|p| Err(format!("{p}: no file")),
        map_id: AssetId::sub_asset(AssetId::default(), "volume-cave"),
        out_file: &out,
        cooked_dir: &cooked,
        cdlod: CdlodConfig::default(),
        origin: [0.0, 0.0],
        resolve_material: &|_| Err(MaterialResolveError::Missing),
    };
    let field = Cave;
    let volumes = [VolumeSource {
        sdf: "cave",
        field: &field,
        bounds: [[-6.0, -8.0, -6.0], [6.0, 4.0, 6.0]],
        fingerprint: b"sphere-cave-v1",
    }];
    let result = if with_field {
        cook_mesh_layers_with_volumes(&req, &volumes)
    } else {
        cook_mesh_layers(&req)
    };
    (dir, result)
}

fn cook_cave(tag: &str, fills: &str, chunk_m: f32) -> Cooked {
    let (dir, result) = try_cook(tag, &document(fills, chunk_m), true);
    let outcome = result.unwrap_or_else(|e| panic!("volume cook: {e} ({})", e.code()));
    let file = std::fs::read(dir.join("assets/maps/cave.meshlayers")).expect("cooked file");
    Cooked { dir, outcome, file }
}

/// The positions of a chunk LOD mesh as written.
fn lod_points(cooked: &Path, id: AssetId) -> Vec<[f32; 3]> {
    let bytes = std::fs::read(cooked.join(cooked_file_name(id))).expect("chunk LOD file");
    load_mesh_bytes(&bytes).expect("CookedMesh").mesh.positions
}

/// Spec 26 § 2a.9: a sphere cave under a filled mouth cooks; the mouth is cut in the lattice
/// with a ring; inside the mouth the query answers the cave floor (no ground, no roof: the dome
/// above the ground was clipped away); with the hole not filled the ground stays whole there.
#[test]
fn volume_layer_cooks_like_a_mesh_layer() {
    let open = cook_cave("volume-cave-open", "\"cave_mouth\"", 64.0);
    let report = open.outcome.report.as_ref().expect("cooked");
    let mouth = report
        .ground
        .mouths
        .iter()
        .find(|m| m.id == "cave_mouth")
        .expect("the filled mouth is cut");
    assert!(mouth.cut_triangles > 0, "the mouth cuts lattice triangles");
    assert_eq!(mouth.rings.len(), 1, "one ring");
    assert!(report.layers[0].triangles > 0);
    assert!(
        !open.outcome.presentation.layers[0].chunks.is_empty(),
        "the layer has chunks"
    );

    let geom = WorldGeom::from_cooked(&open.file).expect("loads");
    let state = geom.new_state();
    let (x, z) = (CENTER[0], CENTER[2]);
    let hit = geom.surface_at(&state, x, z, 0.5, 0.0);
    let under = hit.under.expect("a floor under the mouth");
    assert_eq!(
        under.kind,
        SurfaceKind::Floor,
        "the cave floor, not the ground: {hit:?}"
    );
    let floor = CENTER[1] - RADIUS;
    assert!(
        (under.y - floor).abs() <= 0.05,
        "cave floor at {} (sphere bottom {floor})",
        under.y
    );
    assert!(hit.over.is_none(), "open sky over the mouth: {hit:?}");

    let closed = cook_cave("volume-cave-closed", "", 64.0);
    let geom = WorldGeom::from_cooked(&closed.file).expect("loads");
    let hit = geom.surface_at(&geom.new_state(), x, z, 0.5, 0.0);
    let under = hit.under.expect("ground");
    assert_eq!(
        under.kind,
        SurfaceKind::Ground,
        "no fill: the ground is whole: {hit:?}"
    );
    assert!(under.y.abs() <= 1e-4);
    // The cave is still there under the ground.
    let deep = geom.surface_at(&geom.new_state(), x, z, -2.0, 0.0);
    assert_eq!(deep.under.map(|h| h.kind), Some(SurfaceKind::Floor), "{deep:?}");
    for c in [open, closed] {
        let _ = std::fs::remove_dir_all(&c.dir);
    }
}

/// No canonical triangle vertex and no chunk LOD vertex of a volume layer stands above the LOD-0
/// ground by more than 1e-4 m (here: anywhere, the clip keeps only what is under the ground), and
/// the clip really removed the dome (the unclipped sphere reaches 3 m over the ground).
#[test]
fn volume_layer_never_rises_above_the_ground_outside_mouths() {
    let c = cook_cave("volume-cave-under", "\"cave_mouth\"", 16.0);
    let geom = WorldGeom::from_cooked(&c.file).expect("loads");
    let mut lowest = f32::INFINITY;
    let mut layer_tris = 0;
    for t in geom.triangles().iter().filter(|t| t.tri.layer == 0) {
        layer_tris += 1;
        for p in t.v {
            assert!(p[1] <= 1e-4, "canonical vertex above the ground: {p:?}");
            lowest = lowest.min(p[1]);
        }
    }
    assert!(layer_tris > 100, "{layer_tris} layer triangles");
    assert!(
        lowest < CENTER[1] - RADIUS + 0.1,
        "the cave floor is there ({lowest})"
    );
    let cooked = c.cooked_dir();
    let mut lods = 0;
    for chunk in &c.outcome.presentation.layers[0].chunks {
        for lod in &chunk.lods {
            lods += 1;
            for p in lod_points(&cooked, lod.mesh) {
                assert!(
                    p[1] <= 1e-4,
                    "chunk ({}, {}) LOD vertex above the ground: {p:?}",
                    chunk.x,
                    chunk.z
                );
            }
        }
    }
    assert!(lods > 1);
    let _ = std::fs::remove_dir_all(&c.dir);
}

/// § 2a.5 for a volume layer: the cave spans several chunks; every vertex two neighboring chunks
/// share at LOD 0 is present, bit for bit, in both chunks at every LOD (locked borders), and some
/// chunk has more than one LOD.
#[test]
fn volume_chunks_share_borders() {
    let c = cook_cave("volume-cave-borders", "\"cave_mouth\"", 8.0);
    let cooked = c.cooked_dir();
    let chunks = &c.outcome.presentation.layers[0].chunks;
    assert!(chunks.len() >= 2, "{} chunks", chunks.len());
    assert!(chunks.iter().any(|ch| ch.lods.len() >= 2), "some chunk has LODs");
    let bits = |id| -> BTreeSet<[u32; 3]> {
        lod_points(&cooked, id)
            .into_iter()
            .map(|p| p.map(f32::to_bits))
            .collect()
    };
    let mut pairs = 0;
    for a in chunks {
        for b in chunks {
            let east = b.x == a.x + 1 && b.z == a.z;
            let north = b.z == a.z + 1 && b.x == a.x;
            if !(east || north) {
                continue;
            }
            let shared: BTreeSet<[u32; 3]> = bits(a.lods[0].mesh)
                .intersection(&bits(b.lods[0].mesh))
                .copied()
                .collect();
            if shared.is_empty() {
                continue;
            }
            pairs += 1;
            for side in [a, b] {
                for (i, lod) in side.lods.iter().enumerate() {
                    let have = bits(lod.mesh);
                    let missing = shared.difference(&have).count();
                    assert_eq!(
                        missing,
                        0,
                        "chunk ({}, {}) LOD {i} lost {missing} of {} border vertices",
                        side.x,
                        side.z,
                        shared.len()
                    );
                }
            }
        }
    }
    assert!(pairs > 0, "no neighboring chunks share a border");
    let _ = std::fs::remove_dir_all(&c.dir);
}

/// A `Volume` layer without the host's field is still refused by name, and a fill count the clip
/// loops do not match is `meshlayers_volume_mouths`.
#[test]
fn a_volume_layer_needs_its_field_and_one_loop_per_fill() {
    let (dir, no_field) = try_cook("volume-cave-nofield", &document("\"cave_mouth\"", 64.0), false);
    assert_eq!(no_field.err().map(|e| e.code()), Some("cook_not_built"));
    let _ = std::fs::remove_dir_all(&dir);
    let (dir, two) = try_cook("volume-cave-two", &document("\"a\", \"b\"", 64.0), true);
    assert_eq!(two.err().map(|e| e.code()), Some("meshlayers_volume_mouths"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The volume cook is a pure function of its inputs: two cooks write the same cooked file and
/// the same chunk LOD files, byte for byte.
#[test]
fn volume_cook_is_deterministic() {
    let a = cook_cave("volume-cave-det-a", "\"cave_mouth\"", 16.0);
    let b = cook_cave("volume-cave-det-b", "\"cave_mouth\"", 16.0);
    assert_eq!(a.file, b.file, "cooked file bytes");
    for (ca, cb) in a.outcome.presentation.layers[0]
        .chunks
        .iter()
        .zip(&b.outcome.presentation.layers[0].chunks)
    {
        for (la, lb) in ca.lods.iter().zip(&cb.lods) {
            let read = |dir: &Path, id| std::fs::read(dir.join(cooked_file_name(id))).expect("LOD file");
            assert_eq!(read(&a.cooked_dir(), la.mesh), read(&b.cooked_dir(), lb.mesh));
        }
    }
    for c in [a, b] {
        let _ = std::fs::remove_dir_all(&c.dir);
    }
}
