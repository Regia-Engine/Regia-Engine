// SPDX-License-Identifier: MPL-2.0
//! Plan row W-16 (spec 26 § 2a.5 and § 2a.10, ADR-8398): the mesh-layer cook writes W-06's
//! canonical section unchanged, cuts each layer on the CDLOD node grid, simplifies chunk LODs with
//! their borders and mouth loops locked, writes nothing for an unchanged input, and the run-time
//! load streams the chunks as mesh entities.
//!
//! Fixtures: DO27's passage layouts (`regia_world_geom`'s `fixtures` feature, data only) on flat
//! synthetic ground at the canyon floor; no DO27 map data.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetId, cooked_file_name, load_mesh_bytes};
use regia_asset_pipeline::terrain_mesh_layers::{
    MaterialResolveError, MeshLayersCookOutcome, MeshLayersCookRequest, cook_mesh_layers,
};
use regia_terrain::cdlod::QuadNode;
use regia_terrain::{CdlodConfig, TerrainLayerChunk, TerrainLayerStream, TerrainMeshLayersCooked};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{CookedMeshLayers, encode_canonical, split_file};
use regia_world_geom::passages::PassagesDocument;

const WIDTH_M: f32 = 1280.0;

fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), WIDTH_M).with_zero_counts(16384)
}

/// Flat ground at -256 m, `n` samples per side.
fn canyon_ground(n: u32) -> GroundInput {
    GroundInput {
        lattice_n: n,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; (n * n) as usize],
    }
}

struct Fixture {
    dir: PathBuf,
    document: String,
    files: BTreeMap<String, Vec<u8>>,
    ground: GroundInput,
    map_id: AssetId,
}

impl Fixture {
    /// The fixture with ADR-8359's K1 waiver rows in its document when the cook of these inputs
    /// is refused only by `ramp_facet_slope_over_limit` (ADR-8359 Consequences: a test cook that
    /// fails only that check authors a waiver and recooks; ADR-8447). On this flat canyon floor
    /// that is Switchbacks' two known 1.58 ramp facets plus the at-grade aprons of Atacama Rail
    /// and The Cinder Steps, which drop from their authored heights to the -256 m fixture ground,
    /// the same facets `regia_world_geom`'s `meshlayers_formats` fixtures waive on this ground.
    /// The waiver is hashed on the cooked triangles and source-only, so the canonical section
    /// (and every byte these tests compare) is unchanged; any other refusal, or a lapsed hash,
    /// still fails [`Fixture::cook`].
    fn with_k1_waiver(mut self) -> Self {
        let read = |p: &str| {
            self.files
                .get(p)
                .cloned()
                .ok_or_else(|| format!("{p}: not found"))
        };
        let terrain = canyon_terrain();
        let refused = cook(&CookInputs {
            document: self.document.as_bytes(),
            ground: &self.ground,
            terrain: Some(&terrain),
            read_file: &read,
        });
        if let Err(CookError::Checks(report)) = refused
            && only_ramp_facet_failures(&report)
        {
            self.document = waive_ramp_facets(&self.document, &report).expect("waive");
        }
        self
    }

    fn out_file(&self) -> PathBuf {
        self.dir.join("assets/maps/arena.meshlayers")
    }

    fn cooked_dir(&self) -> PathBuf {
        self.dir.join("cooked")
    }

    fn cook(&self) -> MeshLayersCookOutcome {
        let read = |p: &str| {
            self.files
                .get(p)
                .cloned()
                .ok_or_else(|| format!("{p}: not found"))
        };
        let terrain = canyon_terrain();
        let out = self.out_file();
        let cooked = self.cooked_dir();
        cook_mesh_layers(&MeshLayersCookRequest {
            document: self.document.as_bytes(),
            ground: &self.ground,
            terrain: Some(&terrain),
            read_file: &read,
            map_id: self.map_id,
            out_file: &out,
            cooked_dir: &cooked,
            cdlod: CdlodConfig::default(),
            origin: [0.0, 0.0],
            resolve_material: &|_| Err(MaterialResolveError::Missing),
        })
        .expect("cook")
    }
}

/// Three passage layers (DO27's three layouts as data), no filled holes; the second draws on pick
/// layer 2 without shadows.
fn three_layers(tag: &str) -> Fixture {
    let mut files = BTreeMap::new();
    let mut layers = Vec::new();
    for (i, map) in ["switchbacks", "atacama_rail", "cinder_steps"]
        .into_iter()
        .enumerate()
    {
        let path = format!("{map}.passages.ron");
        let ron = PassagesDocument::from_layout(for_map(map).unwrap())
            .to_ron()
            .unwrap();
        files.insert(path.clone(), ron.into_bytes());
        let extra = if i == 1 {
            ", map_layer: 2, cast_shadows: false"
        } else {
            ""
        };
        layers.push(format!(
            "(id: \"{map}\", source: Passages(path: \"{path}\"){extra})"
        ));
    }
    Fixture {
        dir: regia_test_utils::unique_temp_dir(tag),
        document: format!(
            "(version: 1, ground_tile_cells: 64, layers: [{}])",
            layers.join(", ")
        ),
        files,
        ground: canyon_ground(1025),
        map_id: AssetId::sub_asset(AssetId::default(), "w16-three-layers"),
    }
    .with_k1_waiver()
}

/// Switchbacks with every mouth filled.
fn switchbacks_filled(tag: &str) -> Fixture {
    let terrain = canyon_terrain();
    let layout = for_map("switchbacks").unwrap();
    let mouths = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&terrain)));
    let fills: Vec<String> = mouths.iter().map(|m| format!("{:?}", m.id)).collect();
    let ron = PassagesDocument::from_layout(layout).to_ron().unwrap();
    Fixture {
        dir: regia_test_utils::unique_temp_dir(tag),
        document: format!(
            "(version: 1, ground_tile_cells: 64, layers: [(id: \"tubes\", source: Passages(path: \"p\"), fills: [{}])])",
            fills.join(", ")
        ),
        files: [("p".to_string(), ron.into_bytes())].into(),
        ground: canyon_ground(2049),
        map_id: AssetId::sub_asset(AssetId::default(), "w16-switchbacks"),
    }
    .with_k1_waiver()
}

/// Review proof: the switch package prescribes upgrade-project, then an ordinary cook.
/// A synthetic v1 cache with the pre-K8 mouth cuts must not survive that sequence.
/// Only the legacy cut bitmap is reconstructed; this is not a historical golden.
#[test]
fn switch_package_upgrade_then_cook_rebuilds_legacy_skylight_cuts() {
    use regia_world_geom::cut::{cut_bitmaps, cut_lattice, passage_mouths};

    #[derive(serde::Serialize)]
    struct LayerV2<'a> {
        id: &'a str,
        map_layer: u8,
        cast_shadows: bool,
        material: &'a str,
        material_id: Option<AssetId>,
        chunks: &'a [regia_terrain::CookedLayerChunk],
    }
    #[derive(serde::Serialize)]
    struct SectionV2<'a> {
        version: u32,
        grid: regia_terrain::LayerChunkGrid,
        layers: Vec<LayerV2<'a>>,
        chunk_importer: u32,
    }

    let f = switchbacks_filled("switch-package-legacy-skylight");
    let fresh = f.cook();
    let mut file = CookedMeshLayers::decode(&std::fs::read(f.out_file()).unwrap()).unwrap();
    let expected_cuts = file.canonical.cut_bitmaps.clone();
    let layout = for_map("switchbacks").unwrap();
    // v0.6's cook derives mouths from the authored layout, before open_skylight_shafts.
    // See Regia commit 886eb2087, which changes that input to the meshed layout.
    let mouths = passage_mouths(&layout, &layout.sample(Some(&canyon_terrain())));
    let old_cut = cut_lattice(&file.canonical.lattice, &f.ground.counts, &mouths).unwrap();
    file.canonical.cut_bitmaps = cut_bitmaps(&file.canonical.lattice, &old_cut.cut);
    assert_ne!(file.canonical.cut_bitmaps, expected_cuts);
    file.header.version = 1;
    file.canonical.canonical_version = 1;
    let old_chunks = SectionV2 {
        version: 2,
        grid: fresh.presentation.grid,
        layers: fresh
            .presentation
            .layers
            .iter()
            .map(|layer| LayerV2 {
                id: &layer.id,
                map_layer: layer.map_layer,
                cast_shadows: layer.cast_shadows,
                material: &layer.material,
                material_id: layer.material_id,
                chunks: &layer.chunks,
            })
            .collect(),
        chunk_importer: fresh.presentation.chunk_importer,
    };
    let mut legacy = regia_world_geom::cooked::COOKED_MAGIC.to_vec();
    legacy.extend(postcard::to_allocvec(&file.header).unwrap());
    legacy.extend(postcard::to_allocvec(&postcard::to_allocvec(&file.canonical).unwrap()).unwrap());
    legacy.extend(postcard::to_allocvec(&file.presentation).unwrap());
    legacy.extend(postcard::to_allocvec(&postcard::to_allocvec(&old_chunks).unwrap()).unwrap());
    // The real upgrade-project callback (upgrade_project.rs:120), not a test migrator.
    let upgraded = regia_terrain::upgrade_cooked_mesh_layers(&legacy).unwrap();
    let migrated = CookedMeshLayers::decode(&upgraded).unwrap();
    assert_eq!(migrated.header.version, 2);
    assert_eq!(migrated.canonical.cut_bitmaps, file.canonical.cut_bitmaps);
    regia_test_utils::write_atomic(&f.out_file(), &upgraded).unwrap();

    // The real cook_level_job path ultimately calls the same cook_mesh_layers entry point.
    let again = f.cook();
    let after = CookedMeshLayers::decode(&std::fs::read(f.out_file()).unwrap()).unwrap();
    let repaired_bytes = std::fs::read(f.out_file()).unwrap();
    let unchanged = f.cook();
    assert!(!unchanged.cooked, "the repaired cache must skip the next cook");
    assert!(unchanged.written.is_empty());
    assert_eq!(std::fs::read(f.out_file()).unwrap(), repaired_bytes);
    // Control for the proposed documentation fix: --clean removes the cooked cache.
    // Losing the chunk LOD files must force the same entry point to rebuild the cuts.
    std::fs::remove_dir_all(f.cooked_dir()).unwrap();
    let clean = f.cook();
    let cleaned = CookedMeshLayers::decode(&std::fs::read(f.out_file()).unwrap()).unwrap();
    std::fs::remove_dir_all(&f.dir).unwrap();
    assert!(clean.cooked);
    assert!(
        cleaned.canonical.cut_bitmaps == expected_cuts,
        "clean cook repairs cuts"
    );
    assert!(
        after.canonical.cut_bitmaps == expected_cuts,
        "upgrade-project then regia cook retained pre-K8 cuts; cooked={}, old tiles={}, fresh tiles={}",
        again.cooked,
        after.canonical.cut_bitmaps.len(),
        expected_cuts.len(),
    );
    assert!(again.cooked, "the upgrade must force a source recook");
}

/// A package upgraded by the previous migrator has a current header but still carries the
/// revision-1 input hash. Repair it too; upgrading it again deliberately changes no bytes.
#[test]
fn an_already_upgraded_skylight_cache_is_recooked_once() {
    use regia_world_geom::cut::{cut_bitmaps, cut_lattice, passage_mouths};

    let f = switchbacks_filled("switch-package-already-upgraded-skylight");
    f.cook();
    let mut file = CookedMeshLayers::decode(&std::fs::read(f.out_file()).unwrap()).unwrap();
    let expected_cuts = file.canonical.cut_bitmaps.clone();
    let layout = for_map("switchbacks").unwrap();
    let mouths = passage_mouths(&layout, &layout.sample(Some(&canyon_terrain())));
    let old_cut = cut_lattice(&file.canonical.lattice, &f.ground.counts, &mouths).unwrap();
    file.canonical.cut_bitmaps = cut_bitmaps(&file.canonical.lattice, &old_cut.cut);
    assert_ne!(file.canonical.cut_bitmaps, expected_cuts);

    // Reconstruct the old cache key independently, using its frozen length-prefixed inputs.
    // This fixture has one named file, so BTreeMap iteration is also document order.
    assert_eq!(f.files.len(), 1);
    let mut hash = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(b"regia-meshlayers-input/1");
    field(f.document.as_bytes());
    let mut ground = Vec::new();
    ground.extend(f.ground.lattice_n.to_le_bytes());
    ground.extend(f.ground.width_m.to_bits().to_le_bytes());
    ground.extend(f.ground.offset_m.to_bits().to_le_bytes());
    ground.extend(f.ground.step_m.to_bits().to_le_bytes());
    for count in &f.ground.counts {
        ground.extend(count.to_le_bytes());
    }
    field(&ground);
    for (path, bytes) in &f.files {
        field(path.as_bytes());
        field(bytes);
    }
    file.header.input_hash = *hash.finalize().as_bytes();
    let stale = file.encode().unwrap();
    assert_eq!(regia_terrain::upgrade_cooked_mesh_layers(&stale).unwrap(), stale);
    regia_test_utils::write_atomic(&f.out_file(), &stale).unwrap();

    let repaired = f.cook();
    let repaired_bytes = std::fs::read(f.out_file()).unwrap();
    let after = CookedMeshLayers::decode(&repaired_bytes).unwrap();
    assert!(
        after.canonical.cut_bitmaps == expected_cuts,
        "already-upgraded cache kept legacy skylight cuts; cooked={}, old tiles={}, fresh tiles={}",
        repaired.cooked,
        after.canonical.cut_bitmaps.len(),
        expected_cuts.len(),
    );
    assert!(repaired.cooked);
    let unchanged = f.cook();
    assert!(!unchanged.cooked);
    assert!(unchanged.written.is_empty());
    assert_eq!(std::fs::read(f.out_file()).unwrap(), repaired_bytes);
    std::fs::remove_dir_all(&f.dir).unwrap();
}

/// The positions of a chunk LOD mesh as written, by bits.
fn lod_positions(cooked: &Path, id: AssetId) -> BTreeSet<[u32; 3]> {
    let bytes = std::fs::read(cooked.join(cooked_file_name(id))).expect("chunk LOD file");
    let mesh = load_mesh_bytes(&bytes).expect("CookedMesh").mesh;
    mesh.positions.iter().map(|p| p.map(f32::to_bits)).collect()
}

/// The XZ chunk of a point, by the quadtree's own node rectangles (`QuadNode::world_rect`).
fn node_of(grid: &regia_terrain::LayerChunkGrid, x: f32, z: f32) -> (u32, u32) {
    let n = grid.chunks_per_side();
    let find = |v: f32, axis: usize| {
        (0..n)
            .find(|&i| {
                let node = QuadNode {
                    level: grid.chunk_level,
                    x: if axis == 0 { i } else { 0 },
                    z: if axis == 0 { 0 } else { i },
                };
                let (mx, mz, size) = node.world_rect(grid.span_m, grid.root_level);
                let lo = if axis == 0 { mx } else { mz };
                v >= lo && v < lo + size
            })
            .expect("inside the grid")
    };
    (find(x, 0), find(z, 1))
}

/// Spec 26 § 2a.5: a 3-layer source cooks to chunks whose rects are the chunk-level nodes the
/// layers touch; the borders of neighboring chunks share bit-identical vertices at every LOD pair
/// (locked borders); a recook of the same input writes nothing.
#[test]
fn cook_chunks_on_the_node_grid() {
    let f = three_layers("w16_chunks");
    let first = f.cook();
    assert!(first.cooked);
    assert!(!first.written.is_empty());
    let p = &first.presentation;
    let grid = p.grid;
    assert_eq!(p.layers.len(), 3);
    // The chunk level is the coarsest node at most chunk_m (64 m): 1280 m / 2^5 = 40 m.
    assert_eq!(grid.root_level, 6);
    assert_eq!(grid.chunk_level, 1);
    assert_eq!(
        regia_terrain::cdlod::node_world_size(grid.span_m, grid.root_level, 1),
        40.0
    );
    let file = CookedMeshLayers::decode(&std::fs::read(f.out_file()).unwrap()).unwrap();
    // Chunks = the nodes the layer's triangle centroids fall in.
    for (li, layer) in p.layers.iter().enumerate() {
        let mut touched = BTreeSet::new();
        for (o, t) in file.canonical.triangles_sorted() {
            // The cook drops a triangle that welds to a point or an edge, as the chunk mesh does.
            if usize::from(t.layer) != li || t.v[0] == t.v[1] || t.v[1] == t.v[2] || t.v[0] == t.v[2] {
                continue;
            }
            let m = t.v.map(|v| {
                [
                    regia_world_geom::mm_to_m(o[0], v[0]),
                    regia_world_geom::mm_to_m(o[1], v[2]),
                ]
            });
            let c = [
                (m[0][0] + m[1][0] + m[2][0]) / 3.0,
                (m[0][1] + m[1][1] + m[2][1]) / 3.0,
            ];
            touched.insert(node_of(&grid, c[0], c[1]));
        }
        let chunks: BTreeSet<(u32, u32)> = layer.chunks.iter().map(|c| (c.x, c.z)).collect();
        assert!(touched.len() > 1, "layer {li} spans several chunks");
        // A chunk whose triangles all welded away would be missing; none are.
        assert_eq!(chunks, touched, "layer {li}");
        assert!(layer.chunks.iter().all(|c| c.lods.len() == 4));
        assert!(
            layer
                .chunks
                .iter()
                .any(|c| c.lods[3].triangles < c.lods[0].triangles),
            "layer {li}: the coarse LODs simplify"
        );
    }
    assert!(!p.layers[1].cast_shadows);
    assert_eq!(p.layers[1].map_layer, 2);
    // Locked borders: the vertices two neighbors share at LOD 0 are in every LOD of both.
    let cooked = f.cooked_dir();
    let mut shared_checked = 0usize;
    for layer in &p.layers {
        let by_xz: BTreeMap<(u32, u32), &regia_terrain::CookedLayerChunk> =
            layer.chunks.iter().map(|c| ((c.x, c.z), c)).collect();
        for (&(x, z), a) in &by_xz {
            for (nx, nz) in [(x + 1, z), (x, z + 1)] {
                let Some(b) = by_xz.get(&(nx, nz)) else {
                    continue;
                };
                let a0 = lod_positions(&cooked, a.lods[0].mesh);
                let b0 = lod_positions(&cooked, b.lods[0].mesh);
                let shared: BTreeSet<[u32; 3]> = a0.intersection(&b0).copied().collect();
                if shared.is_empty() {
                    continue;
                }
                shared_checked += shared.len();
                for la in &a.lods {
                    let pa = lod_positions(&cooked, la.mesh);
                    for lb in &b.lods {
                        let pb = lod_positions(&cooked, lb.mesh);
                        assert!(
                            shared.is_subset(&pa),
                            "{} chunk {x}_{z}: a border vertex left a LOD",
                            layer.id
                        );
                        assert!(
                            shared.is_subset(&pb),
                            "{} chunk {nx}_{nz}: a border vertex left a LOD",
                            layer.id
                        );
                    }
                }
            }
        }
    }
    assert!(shared_checked > 0, "some neighbors share a border");
    // A recook of the same input writes nothing.
    let before = std::fs::read(f.out_file()).unwrap();
    let again = f.cook();
    assert!(!again.cooked);
    assert!(again.written.is_empty(), "{:?}", again.written);
    assert_eq!(again.presentation, first.presentation);
    assert_eq!(std::fs::read(f.out_file()).unwrap(), before);
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// Amendment A1: the stream cook's canonical section and header are W-06's, byte for byte (one
/// writer, one `content_hash`).
#[test]
fn stream_cook_writes_the_same_canonical_bytes_as_regia_cook() {
    let f = three_layers("w16_canonical");
    f.cook();
    let ours = split_file(&std::fs::read(f.out_file()).unwrap()).unwrap();
    let read = |p: &str| f.files.get(p).cloned().ok_or_else(|| format!("{p}: not found"));
    let terrain = canyon_terrain();
    let theirs = cook(&CookInputs {
        document: f.document.as_bytes(),
        ground: &f.ground,
        terrain: Some(&terrain),
        read_file: &read,
    })
    .unwrap()
    .file;
    assert_eq!(ours.canonical, encode_canonical(&theirs.canonical).unwrap());
    assert_eq!(ours.header, theirs.header);
    assert_eq!(
        regia_world_geom::cooked::content_hash_of_bytes(&ours.canonical),
        regia_world_geom::cooked::content_hash_of(&theirs.canonical).unwrap()
    );
    assert!(!ours.chunks.is_empty());
    // W-07's presentation section (the render meshes) is the canonical cook's, byte for byte (ADR-8576).
    assert_eq!(ours.presentation, theirs.presentation);
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// Spec 26 § 2a.4 step 6 and § 2a.5: the mouth loop (every canonical vertex within the layer's
/// `max_stitch_gap_m` of a ring vertex, which holds the ring, the band and the authored loop)
/// exists bit for bit in every LOD of each chunk holding it.
#[test]
fn mouth_loops_survive_every_lod() {
    let f = switchbacks_filled("w16_mouths");
    let out = f.cook();
    let report = out.report.as_ref().expect("cooked");
    assert!(!report.ground.mouths.is_empty());
    let file = CookedMeshLayers::decode(&std::fs::read(f.out_file()).unwrap()).unwrap();
    let lattice = file.canonical.lattice;
    let ring: Vec<[i64; 2]> = report
        .ground
        .mouths
        .iter()
        .flat_map(|m| m.rings.iter().flatten())
        .map(|&(i, j)| [lattice.vertex_mm(i), lattice.vertex_mm(j)])
        .collect();
    let mut loop_bits: BTreeSet<[u32; 3]> = BTreeSet::new();
    for (o, t) in file.canonical.triangles_sorted() {
        for v in t.v {
            let (x, z) = (i64::from(o[0] + v[0]), i64::from(o[1] + v[2]));
            if ring
                .iter()
                .any(|r| (x - r[0]).pow(2) + (z - r[1]).pow(2) <= 1000 * 1000)
            {
                loop_bits.insert([
                    regia_world_geom::mm_to_m(o[0], v[0]).to_bits(),
                    regia_world_geom::mm_to_m(0, v[1]).to_bits(),
                    regia_world_geom::mm_to_m(o[1], v[2]).to_bits(),
                ]);
            }
        }
    }
    assert!(!loop_bits.is_empty());
    let cooked = f.cooked_dir();
    let mut checked = 0usize;
    let mut simplified = false;
    for c in &out.presentation.layers[0].chunks {
        let lod0 = lod_positions(&cooked, c.lods[0].mesh);
        let here: BTreeSet<[u32; 3]> = lod0.intersection(&loop_bits).copied().collect();
        if here.is_empty() {
            continue;
        }
        simplified |= c.lods.iter().any(|l| l.triangles < c.lods[0].triangles);
        for (i, lod) in c.lods.iter().enumerate() {
            let p = lod_positions(&cooked, lod.mesh);
            let missing = here.difference(&p).count();
            assert_eq!(
                missing, 0,
                "chunk {}_{} LOD {i} lost {missing} mouth loop vertices",
                c.x, c.z
            );
        }
        checked += here.len();
    }
    assert!(checked > 0);
    assert!(
        simplified,
        "a chunk with a mouth loop also simplified, so the lock did work"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// The cooked file round-trips (canonical through `WorldGeom`, the chunk section through its decoder),
/// and a presentation written by a newer engine is cooked again, back to the same bytes.
#[test]
fn cooked_round_trip_and_newer_presentation_recooks() {
    let f = three_layers("w16_roundtrip");
    let first = f.cook();
    let bytes = std::fs::read(f.out_file()).unwrap();
    let file = CookedMeshLayers::decode(&bytes).unwrap();
    assert!(regia_world_geom::WorldGeom::from_cooked(&bytes).is_ok());
    assert_eq!(
        TerrainMeshLayersCooked::decode(&file.chunks).unwrap(),
        Some(first.presentation.clone())
    );
    let mut newer = first.presentation.clone();
    newer.version += 1;
    let stale = CookedMeshLayers {
        chunks: newer.encode().unwrap(),
        ..file
    };
    std::fs::write(f.out_file(), stale.encode().unwrap()).unwrap();
    assert_eq!(
        TerrainMeshLayersCooked::decode(&stale.chunks).map_err(|e| e.code()),
        Err("meshlayers_presentation_newer")
    );
    let again = f.cook();
    assert!(again.cooked, "a newer presentation recooks");
    assert_eq!(again.written, vec![f.out_file()], "only the cooked file changes");
    assert_eq!(std::fs::read(f.out_file()).unwrap(), bytes);
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// End to end: the level load reads the cooked file next to the map, and the stream spawns chunk
/// entities (mesh, pick layer, shadow flag, chunk tag) around a view through the cooked cache.
#[test]
fn cooked_layers_load_and_stream_as_chunk_entities() {
    use regia_scene::stream_plan::{SourceId, SourceView};
    use regia_scene::streaming::StreamViews;

    let f = three_layers("w16_e2e");
    let out = f.cook();
    let layers = regia_terrain::load_mesh_layers(&f.out_file(), &f.cooked_dir()).expect("layers load");
    assert_eq!(layers.presentation(), Some(&out.presentation));
    // A view on a chunk of layer 1.
    let c = &out.presentation.layers[1].chunks[0];
    let (mx, mz, size) = out.presentation.grid.chunk_rect(c.x, c.z);
    let mut app = App::new();
    app.insert_resource(TerrainLayerStream::new(100.0, u64::MAX));
    app.insert_resource(StreamViews(vec![SourceView::at(
        SourceId::Pin(1),
        [f64::from(mx + size * 0.5), 0.0, f64::from(mz + size * 0.5)],
    )]));
    app.add_systems(Update, regia_terrain::mesh_layer::stream::drive_layer_stream);
    app.world_mut().spawn(layers);
    app.update();
    let mut q = app.world_mut().query::<(
        &TerrainLayerChunk,
        &regia_render::MeshRenderer,
        &regia_scene::components::MapLayer,
        Option<&regia_render::NotShadowCaster>,
    )>();
    let spawned: Vec<(u16, u32, u32, u8, usize, u8, bool)> = q
        .iter(app.world())
        .map(|(t, m, l, s)| {
            (
                t.layer,
                t.node.x,
                t.node.z,
                t.lod,
                m.mesh.indices.len(),
                l.0,
                s.is_some(),
            )
        })
        .collect();
    assert!(!spawned.is_empty());
    let mine = spawned
        .iter()
        .find(|s| s.0 == 1 && s.1 == c.x && s.2 == c.z)
        .expect("the viewed chunk draws");
    assert_eq!(mine.3, 0, "the view is inside the chunk: LOD 0");
    assert!(mine.4 > 0);
    assert_eq!((mine.5, mine.6), (2, true), "layer 1: pick layer 2, no shadows");
    let stats = app.world().resource::<TerrainLayerStream>().stats();
    assert_eq!(stats.resident[3] as usize, out.presentation.chunk_count());
    // Moving the view far away despawns every chunk entity; the coarsest set stays resident.
    app.insert_resource(StreamViews(vec![SourceView::at(
        SourceId::Pin(1),
        [1.0e6, 0.0, 1.0e6],
    )]));
    app.update();
    let left = app
        .world_mut()
        .query::<&TerrainLayerChunk>()
        .iter(app.world())
        .count();
    assert_eq!(left, 0);
    let stats = app.world().resource::<TerrainLayerStream>().stats();
    assert_eq!(stats.resident[3] as usize, out.presentation.chunk_count());
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// A chunk vertex's normal bits, UV bits, and the `(x, z, lod)` of the chunk LOD that held it first.
type SeenVertex = ([u32; 3], [u32; 2], (u32, u32, usize));

/// ADR-8451 decision 6: a vertex on a chunk border has the same normal (bit for bit) in every chunk
/// that holds it, at every LOD, so the lighting shows no seam at a border, nor where a chunk at one
/// LOD meets a neighbor at another. The box-projected UVs, which follow the normal, match too.
#[test]
fn normals_match_on_both_sides_of_every_chunk_border() {
    let f = three_layers("w16-shared-normals");
    let outcome = f.cook();
    let cooked = f.cooked_dir();
    let mut shared = 0usize;
    let mut curved = 0usize;
    for layer in &outcome.presentation.layers {
        // Position bits -> (normal bits, uv bits, the chunk and LOD that wrote it first).
        let mut seen: BTreeMap<[u32; 3], SeenVertex> = BTreeMap::new();
        for c in &layer.chunks {
            for (i, lod) in c.lods.iter().enumerate() {
                let bytes = std::fs::read(cooked.join(cooked_file_name(lod.mesh))).unwrap();
                let mesh = load_mesh_bytes(&bytes).unwrap().mesh;
                assert_eq!(mesh.normals.len(), mesh.positions.len());
                for ((p, n), uv) in mesh.positions.iter().zip(&mesh.normals).zip(&mesh.uvs) {
                    let key = p.map(f32::to_bits);
                    let here = (n.map(f32::to_bits), uv.map(f32::to_bits), (c.x, c.z, i));
                    match seen.get(&key) {
                        None => {
                            seen.insert(key, here);
                        }
                        Some(first) => {
                            assert_eq!(
                                (first.0, first.1),
                                (here.0, here.1),
                                "layer {} vertex {p:?}: normal or UV of chunk {:?} differs from chunk {:?}",
                                layer.id,
                                here.2,
                                first.2
                            );
                            if (first.2.0, first.2.1) != (c.x, c.z) {
                                shared += 1;
                                if n[1].abs() < 0.99 {
                                    curved += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(shared > 0, "the fixture has vertices on chunk borders");
    assert!(
        curved > 0,
        "and some of them are on walls, where per-chunk normals differ"
    );
}

/// Plan row W-17 item 4 (spec 26 § 2a.6, ADR-8537): the frame's extraction draws chunk entities of
/// a casting layer into the shadow views (`ShadowCast::All`) and keeps a `cast_shadows: false`
/// layer's chunks out of them (`ShadowCast::Off`). The chunk entities are the stream's own (no
/// extra code: `NotShadowCaster` on a non-casting layer).
#[test]
fn chunk_entities_cast_unless_told_not_to() {
    use regia_scene::stream_plan::{SourceId, SourceView};
    use regia_scene::streaming::StreamViews;

    let f = three_layers("w17_shadows");
    let out = f.cook();
    let layers = regia_terrain::load_mesh_layers(&f.out_file(), &f.cooked_dir()).expect("layers load");
    let casts: Vec<bool> = out.presentation.layers.iter().map(|l| l.cast_shadows).collect();
    assert_eq!(casts, vec![true, false, true], "layer 1 is the non-casting one");
    let mut app = App::new();
    app.insert_resource(TerrainLayerStream::new(f32::INFINITY, u64::MAX));
    app.insert_resource(StreamViews(vec![SourceView::at(
        SourceId::Pin(1),
        [0.0, 0.0, 0.0],
    )]));
    app.add_systems(Update, regia_terrain::mesh_layer::stream::drive_layer_stream);
    app.init_resource::<regia_render::ExtractedFrame>();
    app.world_mut().spawn((
        regia_scene::Transform::default(),
        regia_scene::GlobalTransform::default(),
        regia_render::Camera::default(),
    ));
    app.world_mut().spawn(layers);
    // Let the stream settle (LOD swaps replace entities), then extract one frame. The stream's
    // commands apply at the end of `Update`, before `Render`: the entities listed after the update
    // are the ones the extraction saw. A LOD swap cross-fades over `fade_frames` (ADR-8842), and
    // the outgoing LOD of a fade casts no shadow by design (`MeshLodFade`), so the frame extracted
    // here is one with no fade left.
    for _ in 0..3 {
        app.update();
    }
    let mut settle = 0;
    while settle < 64
        && app
            .world_mut()
            .query::<&regia_render::MeshLodFade>()
            .iter(app.world())
            .next()
            .is_some()
    {
        app.update();
        settle += 1;
    }
    assert!(
        app.world_mut()
            .query::<&regia_render::MeshLodFade>()
            .iter(app.world())
            .next()
            .is_none(),
        "the layer stream's LOD fades did not finish in {settle} more frames"
    );
    app.add_systems(Render, regia_render::extract_frame);
    app.update();
    // A chunk LOD with no triangles draws nothing and is not extracted; every other one is.
    let (chunks, empty): (BTreeMap<Entity, u16>, usize) = {
        let mut q = app
            .world_mut()
            .query::<(Entity, &TerrainLayerChunk, &regia_render::MeshRenderer)>();
        let mut chunks = BTreeMap::new();
        let mut empty = 0usize;
        for (e, c, m) in q.iter(app.world()) {
            if m.mesh.indices.is_empty() || m.mesh.vertices.is_empty() {
                empty += 1;
            } else {
                chunks.insert(e, c.layer);
            }
        }
        (chunks, empty)
    };
    println!("{} chunk entities with triangles, {empty} empty", chunks.len());
    let by_layer = |l: u16| chunks.values().filter(|&&x| x == l).count();
    assert!(
        by_layer(0) > 0 && by_layer(1) > 0 && by_layer(2) > 0,
        "every layer streams chunk entities: {chunks:?}"
    );
    let extracted = app.world().resource::<regia_render::ExtractedFrame>();
    let mut seen = 0usize;
    for mesh in &extracted.meshes {
        let Some(&layer) = chunks.get(&mesh.entity) else {
            continue;
        };
        let want = if casts[usize::from(layer)] {
            regia_render::ShadowCast::All
        } else {
            regia_render::ShadowCast::Off
        };
        assert_eq!(mesh.shadow, want, "chunk {:?} of layer {layer}", mesh.entity);
        seen += 1;
    }
    assert_eq!(
        seen,
        chunks.len(),
        "every chunk entity with triangles is extracted"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// ADR-8872 (spec 26 § 2a.6 "Path tracer", ADR-8537 deferred item 2): the reference trace sees
/// every drawn layer chunk at LOD 0. With the view on one chunk, the stream draws chunks at LOD 0
/// and coarser; after `reference_chunks_lod0` each drawn chunk's export instances point at a mesh equal to
/// its cooked LOD 0 file (read from the layers' cooked directory, no asset server), chunks drawn
/// at LOD 0 keep their mesh, an instance that is not a chunk is untouched, and nothing fails.
#[test]
fn reference_export_traces_drawn_chunks_at_lod0() {
    use regia_scene::stream_plan::{SourceId, SourceView};
    use regia_scene::streaming::StreamViews;

    let f = three_layers("w09_lod0");
    let out = f.cook();
    let layers = regia_terrain::load_mesh_layers(&f.out_file(), &f.cooked_dir()).expect("layers load");
    // The view on the center of layer 1's first chunk: that chunk draws at LOD 0, far ones coarser.
    let c = &out.presentation.layers[1].chunks[0];
    let (mx, mz, size) = out.presentation.grid.chunk_rect(c.x, c.z);
    let mut app = App::new();
    app.insert_resource(TerrainLayerStream::new(f32::INFINITY, u64::MAX));
    app.insert_resource(StreamViews(vec![SourceView::at(
        SourceId::Pin(1),
        [f64::from(mx + size * 0.5), 0.0, f64::from(mz + size * 0.5)],
    )]));
    app.add_systems(Update, regia_terrain::mesh_layer::stream::drive_layer_stream);
    app.world_mut().spawn(layers);
    for _ in 0..8 {
        app.update();
    }
    // The frame's export, as the renderer keys it: one instance per drawn chunk, plus one
    // instance that is not a chunk.
    let mut export = regia_render::FrameExport {
        frame_index: 0,
        eye: [0.0; 3],
        target: [0.0, 0.0, -1.0],
        fov_y: 1.0,
        width: 8,
        height: 8,
        sun: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        fill_light: None,
        extra_directional: Vec::new(),
        ambient_lux: 0.0,
        ibl: false,
        outdoor: regia_render::OutdoorParams::default(),
        local_lights: Vec::new(),
        instances: Vec::new(),
        meshes: std::collections::HashMap::new(),
        materials: std::collections::HashMap::new(),
        meshes_copied: 0,
    };
    let instance = |mesh_key: u64| regia_render::ExportedInstance {
        mesh_key,
        material_key: 1,
        object_key: mesh_key,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        joints: Vec::new(),
    };
    let mut drawn: Vec<(TerrainLayerChunk, u64)> = Vec::new();
    {
        let mut q = app
            .world_mut()
            .query::<(&TerrainLayerChunk, &regia_render::MeshRenderer)>();
        for (c, m) in q.iter(app.world()) {
            let key = regia_render::runtime::mesh_key(&m.mesh);
            export.meshes.insert(key, m.mesh.clone());
            export.instances.push(instance(key));
            drawn.push((*c, key));
        }
    }
    let other = 0x1234_u64;
    export.meshes.insert(
        other,
        std::sync::Arc::new(regia_render::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        }),
    );
    export.instances.push(instance(other));
    let coarse = drawn.iter().filter(|(c, _)| c.lod > 0).count();
    let fine = drawn.len() - coarse;
    println!("{} drawn chunks: {fine} at LOD 0, {coarse} coarser", drawn.len());
    assert!(coarse > 0 && fine > 0, "the view draws LOD 0 and coarser chunks");

    let got = regia_terrain::reference_chunks_lod0(app.world_mut(), &mut export);
    assert_eq!(
        got,
        regia_terrain::ReferenceChunks {
            swapped: coarse as u32,
            at_lod0: fine as u32,
            failed: 0,
        }
    );
    let cooked = f.cooked_dir();
    for (i, (c, key)) in drawn.iter().enumerate() {
        let traced = export.instances[i].mesh_key;
        let mesh = export.meshes.get(&traced).expect("the traced mesh");
        if c.lod == 0 {
            assert_eq!(traced, *key, "a LOD-0 chunk keeps its drawn mesh");
            continue;
        }
        let chunk = out.presentation.layers[usize::from(c.layer)]
            .chunks
            .iter()
            .find(|k| (k.x, k.z) == (c.node.x, c.node.z))
            .expect("cooked chunk");
        let bytes = std::fs::read(cooked.join(cooked_file_name(chunk.lods[0].mesh))).unwrap();
        let want = regia_render::CpuMesh::from_cooked_rigid(&load_mesh_bytes(&bytes).unwrap().mesh);
        assert_eq!(
            (mesh.vertices.len(), mesh.indices.len()),
            (want.vertices.len(), want.indices.len()),
            "chunk {:?} of layer {} traces its LOD 0",
            (c.node.x, c.node.z),
            c.layer
        );
        assert!(
            mesh.vertices
                .iter()
                .zip(&want.vertices)
                .all(|(a, b)| a.position == b.position),
            "chunk {:?} of layer {}: LOD 0 positions",
            (c.node.x, c.node.z),
            c.layer
        );
        assert_eq!(
            traced,
            regia_terrain::REFERENCE_CHUNK_TAG
                | (u64::from(c.layer) << 32)
                | (u64::from(c.node.z) << 16)
                | u64::from(c.node.x)
        );
    }
    assert_eq!(
        export.instances.last().map(|i| i.mesh_key),
        Some(other),
        "an instance that is not a chunk is untouched"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}
