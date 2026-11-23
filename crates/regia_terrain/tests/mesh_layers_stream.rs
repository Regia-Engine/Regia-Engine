// SPDX-License-Identifier: MPL-2.0
//! Plan row W-16 (spec 26 § 2a.5, ADR-8398): mesh-layer chunks change LOD by the terrain's own
//! distance metric and stream around the views without ever leaving a chunk with no mesh.
//!
//! These drive [`TerrainLayerStream`] with counters and fixed steps (no frames, no clock).

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_asset::AssetId;
use regia_terrain::cdlod::{QuadNode, node_world_size};
use regia_terrain::{
    CdlodConfig, ChunkKey, CookedChunkLod, CookedLayer, CookedLayerChunk, LayerChunkGrid, TerrainLayerStream,
    TerrainMeshLayersCooked,
};

const SPAN: f32 = 1024.0;
const ROOT: u32 = 6;

fn empty_mesh() -> Arc<regia_render::CpuMesh> {
    Arc::new(regia_render::CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    })
}

/// Two layers over every chunk of a 16 x 16 grid (chunk level 2, 64 m), four LODs each.
fn cooked() -> TerrainMeshLayersCooked {
    let config = CdlodConfig::default();
    let grid = LayerChunkGrid::new(
        SPAN,
        ROOT,
        config.grid_size,
        config.range_factor,
        [0.0, 0.0],
        64.0,
    );
    assert_eq!(grid.chunk_level, 2);
    let root = AssetId::sub_asset(AssetId::default(), "w16-fixture");
    let n = grid.chunks_per_side();
    let layers = (0..2)
        .map(|li| CookedLayer {
            id: format!("layer{li}"),
            map_layer: li as u8,
            cast_shadows: true,
            collider: false,
            material: String::new(),
            material_id: None,
            chunks: (0..n)
                .flat_map(|z| (0..n).map(move |x| (x, z)))
                .map(|(x, z)| CookedLayerChunk {
                    x,
                    z,
                    bounds: [[0.0; 3], [1.0; 3]],
                    lods: (0..4u8)
                        .map(|lod| CookedChunkLod {
                            mesh: AssetId::sub_asset(root, &format!("{li}/{x}_{z}/{lod}")),
                            triangles: 1000 >> lod,
                            bytes: 10_000 >> lod,
                            error_m: 0.0,
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        chunk_importer: 0,
        grid,
        layers,
    }
}

/// A deterministic eye in `[-600, 600)^2`.
fn eye(i: u32) -> [f32; 2] {
    let mut s = 0x9e37_79b9u32.wrapping_mul(i + 1);
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s % 1200) as f32 - 600.0
    };
    [next(), next()]
}

/// Spec 26 § 2a.5: for 32 eye positions the drawn LOD of every chunk equals the count of levels
/// `l` with `rect_distance >= range(l)`, capped at the LOD count, where `rect_distance` is
/// `QuadNode::rect_distance` and `range(l) = range_factor * node_world_size(l)`: the terrain's own
/// selection metric (`cdlod.rs`).
#[test]
fn lod_follows_the_terrain_ranges() {
    let cooked = cooked();
    let config = CdlodConfig::default();
    let mut stream = TerrainLayerStream::new(f32::INFINITY, u64::MAX);
    stream.max_loads_per_step = usize::MAX;
    let mut fetch = |_: AssetId| Ok(empty_mesh());
    let mut seen = [0u32; 4];
    for i in 0..32 {
        let p = eye(i);
        let draws = stream.step(&cooked, &[p], &mut fetch);
        assert_eq!(draws.len(), cooked.chunk_count());
        for d in draws {
            let node = QuadNode {
                level: cooked.grid.chunk_level,
                x: d.key.x,
                z: d.key.z,
            };
            let dist = node.rect_distance(SPAN, ROOT, p);
            let want = (0..3u32)
                .filter(|&l| dist >= config.range_factor * node_world_size(SPAN, ROOT, l))
                .count()
                .min(3);
            let (lod, _) = d.draw.expect("every chunk draws within an infinite radius");
            assert_eq!(usize::from(lod), want, "eye {p:?} chunk {:?} at {dist} m", d.key);
            seen[want] += 1;
        }
    }
    // The eyes exercise every LOD.
    assert!(seen.iter().all(|&n| n > 0), "LOD coverage {seen:?}");
}

/// Spec 26 § 2a.5: a source driven across the map with a budget for three finer LODs. At every
/// step every chunk keeps its coarsest LOD resident, and every wanted chunk draws. The coarsest
/// set loads once; finer LODs come and go.
#[test]
fn coarsest_lod_never_leaves() {
    let cooked = cooked();
    let mut stream = TerrainLayerStream::new(200.0, 3 * 5_000);
    let mut fetches: BTreeMap<AssetId, u32> = BTreeMap::new();
    let mut fetch = |id: AssetId| {
        *fetches.entry(id).or_default() += 1;
        Ok(empty_mesh())
    };
    let mut drawn_total = 0u32;
    for step in 0..64 {
        let t = step as f32 / 63.0;
        let p = [-600.0 + 1200.0 * t, -560.0 + 1100.0 * t];
        let draws = stream.step(&cooked, &[p], &mut fetch);
        let stats = stream.stats();
        for (li, layer) in cooked.layers.iter().enumerate() {
            for c in &layer.chunks {
                let key = ChunkKey {
                    layer: li as u16,
                    z: c.z,
                    x: c.x,
                };
                let resident = stream.resident_lods(key);
                assert!(
                    resident.contains(&3),
                    "step {step}: {key:?} lost its coarsest LOD ({resident:?})"
                );
            }
        }
        assert_eq!(stats.resident[3] as usize, cooked.chunk_count(), "step {step}");
        assert!(
            stats.bytes - stats.coarsest_bytes <= stats.budget_bytes,
            "step {step}: {stats:?}"
        );
        let wanted = draws.iter().filter(|d| d.draw.is_some()).count() as u32;
        assert_eq!(wanted, stats.wanted, "step {step}: a wanted chunk draws nothing");
        drawn_total += wanted;
    }
    let stats = stream.stats();
    assert!(drawn_total > 0);
    assert!(
        stats.evictions > 0,
        "finer LODs were dropped as the source moved: {stats:?}"
    );
    // The coarsest LOD of each chunk was fetched exactly once.
    for layer in &cooked.layers {
        for c in &layer.chunks {
            assert_eq!(fetches.get(&c.lods[3].mesh), Some(&1));
        }
    }
    assert!(fetches.len() > cooked.chunk_count(), "finer LODs were loaded");
}

/// A failed coarsest load draws nothing for that chunk and never panics; the other chunks draw.
#[test]
fn a_failed_chunk_load_draws_nothing_and_is_not_retried() {
    let cooked = cooked();
    let bad = cooked.layers[0].chunks[0].lods[3].mesh;
    let mut stream = TerrainLayerStream::new(f32::INFINITY, 0);
    let mut calls = 0u32;
    let mut fetch = |id: AssetId| {
        if id == bad {
            calls += 1;
            Err("missing".to_string())
        } else {
            Ok(empty_mesh())
        }
    };
    for _ in 0..3 {
        let draws = stream.step(&cooked, &[[0.0, 0.0]], &mut fetch);
        let none: Vec<_> = draws.iter().filter(|d| d.draw.is_none()).map(|d| d.key).collect();
        assert_eq!(none, vec![ChunkKey { layer: 0, z: 0, x: 0 }]);
    }
    assert_eq!(calls, 1);
    assert_eq!(stream.stats().failed, 1);
}

/// A mesh of `vertices` render vertices and `indices` indices (zeroed: the stream never reads them).
fn sized_mesh(vertices: usize, indices: usize) -> Arc<regia_render::CpuMesh> {
    Arc::new(regia_render::CpuMesh {
        vertices: vec![
            regia_render::LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]);
            vertices
        ],
        indices: vec![0; indices],
    })
}

/// Plan D9 (ADR-8873): `resident_bytes` is the bytes the resident meshes hold (their render
/// vertices and `u32` indices), not the cooked record bytes `bytes` counts. A source driven across
/// the map: at every step `resident_bytes` equals the sum over every resident LOD of the mesh the
/// stream was handed, and `coarsest_resident_bytes` that sum over the coarsest set. The fixture's
/// meshes hold more than their cooked records (as cooked chunk meshes do: a 96-byte render vertex
/// against a packed cooked vertex), so the two counters differ.
#[test]
fn resident_bytes_count_the_meshes_the_stream_holds() {
    let cooked = cooked();
    let mut stream = TerrainLayerStream::new(200.0, 3 * 5_000);
    // LOD l of every chunk: 100 >> l vertices and 3 * (1000 >> l) indices.
    let mut sizes: BTreeMap<AssetId, u64> = BTreeMap::new();
    for layer in &cooked.layers {
        for c in &layer.chunks {
            for (lod, rec) in c.lods.iter().enumerate() {
                let (v, i) = (100usize >> lod, 3 * (1000usize >> lod));
                sizes.insert(
                    rec.mesh,
                    (v * size_of::<regia_render::LabVertex>() + i * 4) as u64,
                );
            }
        }
    }
    let mut fetch = |id: AssetId| {
        let lod = cooked
            .layers
            .iter()
            .flat_map(|l| l.chunks.iter())
            .find_map(|c| c.lods.iter().position(|r| r.mesh == id))
            .expect("a fixture mesh");
        Ok(sized_mesh(100 >> lod, 3 * (1000 >> lod)))
    };
    let mut finer_seen = false;
    for step in 0..32 {
        let t = step as f32 / 31.0;
        let p = [-600.0 + 1200.0 * t, -560.0 + 1100.0 * t];
        stream.step(&cooked, &[p], &mut fetch);
        let stats = stream.stats();
        let (mut held, mut coarsest) = (0u64, 0u64);
        for (li, layer) in cooked.layers.iter().enumerate() {
            for c in &layer.chunks {
                let key = ChunkKey {
                    layer: li as u16,
                    z: c.z,
                    x: c.x,
                };
                for lod in stream.resident_lods(key) {
                    let b = sizes[&c.lods[usize::from(lod)].mesh];
                    held += b;
                    if usize::from(lod) == c.lods.len() - 1 {
                        coarsest += b;
                    } else {
                        finer_seen = true;
                    }
                }
            }
        }
        assert_eq!(stats.resident_bytes, held, "step {step}: {stats:?}");
        assert_eq!(stats.coarsest_resident_bytes, coarsest, "step {step}: {stats:?}");
        assert!(stats.resident_bytes > stats.bytes, "step {step}: {stats:?}");
    }
    assert!(finer_seen, "the source streamed finer LODs");
}
