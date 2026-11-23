// SPDX-License-Identifier: MPL-2.0
//! Plan row W-31 (plan § 4.7, ADR-8662): the mesh-layer geometry the run time holds on the low
//! tiers, by counters. The cooked Switchbacks layer (every mouth filled, DO27's layout as data on
//! flat synthetic ground) streams with the shipped `TerrainLayerStream` settings along every tube
//! sample; the peak resident vertex and index bytes are the layer VRAM (the GPU buffers the chunk
//! meshes upload) and the CPU copies the stream holds are the layer RAM. Since ADR-8873 (plan D9)
//! both read the meshes' real bytes (`LayerStreamStats::resident_bytes`), not the cooked records.
//!
//! Bars: ULTRA_LOW (T3 handover § 7, the owner's floor): layer geometry at most 24 MB of VRAM and
//! 32 MB of RAM; the Deck (DO27's proposal, question Q4): layer meshes at most 64 MB.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use regia_asset::{AssetId, cooked_file_name, load_mesh_bytes};
use regia_asset_pipeline::terrain_mesh_layers::{
    MaterialResolveError, MeshLayersCookError, MeshLayersCookRequest, cook_mesh_layers,
};
use regia_terrain::{CdlodConfig, TerrainLayerStream};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, GroundInput};
use regia_world_geom::passages::PassagesDocument;

const WIDTH_M: f32 = 1280.0;
/// ULTRA_LOW layer geometry in VRAM, bytes.
const ULTRA_LOW_LAYER_VRAM: u64 = 24_000_000;
/// ULTRA_LOW layer geometry in RAM, bytes.
const ULTRA_LOW_LAYER_RAM: u64 = 32_000_000;
/// The Deck's layer meshes, bytes.
const DECK_LAYER_BYTES: u64 = 64_000_000;

/// The cooked Switchbacks layer: its presentation, the cooked chunk directory and the samples of
/// every tube (XZ).
fn switchbacks() -> (
    regia_terrain::TerrainMeshLayersCooked,
    PathBuf,
    PathBuf,
    Vec<[f32; 2]>,
) {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), WIDTH_M).with_zero_counts(16384);
    let layout = for_map("switchbacks").unwrap();
    let sampled = layout.sample(Some(&terrain));
    let eyes: Vec<[f32; 2]> = sampled
        .passages
        .iter()
        .flat_map(|p| p.samples.iter().map(|s| [s.p[0], s.p[2]]))
        .collect();
    let mouths = regia_world_geom::cut::passage_mouths(&layout, &sampled);
    let fills: Vec<String> = mouths.iter().map(|m| format!("{:?}", m.id)).collect();
    let ron = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let files: BTreeMap<String, Vec<u8>> = [("p".to_string(), ron.into_bytes())].into();
    let document = format!(
        "(version: 1, ground_tile_cells: 64, layers: [(id: \"tubes\", source: Passages(path: \"p\"), fills: [{}])])",
        fills.join(", ")
    );
    let n = 2049u32;
    let ground = GroundInput {
        lattice_n: n,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; (n * n) as usize],
    };
    let dir = regia_test_utils::unique_temp_dir("w31_layer_memory");
    let out_file = dir.join("assets/maps/arena.meshlayers");
    let cooked_dir = dir.join("cooked");
    let read = |p: &str| files.get(p).cloned().ok_or_else(|| format!("{p}: not found"));
    let run = |document: &str| {
        cook_mesh_layers(&MeshLayersCookRequest {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
            map_id: AssetId::sub_asset(AssetId::default(), "w31-switchbacks"),
            out_file: &out_file,
            cooked_dir: &cooked_dir,
            cdlod: CdlodConfig::default(),
            origin: [0.0, 0.0],
            resolve_material: &|_| Err(MaterialResolveError::Missing),
        })
    };
    // ADR-8359/ADR-8436: a test cook waives the two known Switchbacks ramp facets by hash and
    // recooks (the waiver leaves the cooked bytes alone); any other refusal still fails.
    let outcome = match run(&document) {
        Err(MeshLayersCookError::Geom(CookError::Checks(report))) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&document, &report).expect("waiver"))
        }
        other => other,
    }
    .expect("cook");
    (outcome.presentation, cooked_dir, dir, eyes)
}

/// Resident bytes of a CPU mesh's vectors.
fn cpu_bytes(mesh: &regia_render::CpuMesh) -> u64 {
    (mesh.vertices.capacity() * std::mem::size_of::<regia_render::LabVertex>() + mesh.indices.capacity() * 4)
        as u64
}

/// Plan § 4.7 ULTRA_LOW row and Q4's Deck row, by counters: streaming the cooked Switchbacks
/// layer along every tube sample with the shipped stream settings, the resident layer geometry
/// (the stream's own vertex and index count, which is what the chunk meshes upload) stays within
/// 24 MB, the CPU copies the stream holds within 32 MB, and both within the Deck's 64 MB.
#[test]
fn layer_vram_bytes_within_budget() {
    let (cooked, cooked_dir, dir, eyes) = switchbacks();
    let all_lods: u64 = cooked
        .layers
        .iter()
        .flat_map(|l| l.chunks.iter())
        .flat_map(|c| c.lods.iter())
        .map(|l| l.bytes)
        .sum();
    let mut stream = TerrainLayerStream::default();
    let held: std::cell::RefCell<BTreeMap<AssetId, u64>> = std::cell::RefCell::new(BTreeMap::new());
    let mut fetch = |id: AssetId| -> Result<Arc<regia_render::CpuMesh>, String> {
        let bytes = std::fs::read(cooked_dir.join(cooked_file_name(id))).map_err(|e| e.to_string())?;
        let loaded = load_mesh_bytes(&bytes).map_err(|e| e.to_string())?;
        let mesh = regia_render::CpuMesh::from_cooked_rigid(&loaded.mesh);
        held.borrow_mut().insert(id, cpu_bytes(&mesh));
        Ok(Arc::new(mesh))
    };
    let mut peak_cooked = 0u64;
    let mut peak_vram = 0u64;
    let mut peak_ram = 0u64;
    let mut steps = 0u32;
    for eye in &eyes {
        // Settle at each eye: the stream loads `max_loads_per_step` finer LODs a step.
        for _ in 0..64 {
            stream.step(&cooked, &[*eye], &mut fetch);
            steps += 1;
            let stats = stream.stats();
            peak_cooked = peak_cooked.max(stats.bytes);
            // D9 (ADR-8873): the GPU buffers of a chunk mesh hold its 96-byte render vertices and
            // `u32` indices, the bytes the stream's `resident_bytes` counts (not the cooked records).
            peak_vram = peak_vram.max(stats.resident_bytes);
            if stats.pending == 0 {
                break;
            }
        }
        // The CPU copies resident now: every LOD the stream still holds.
        let held = held.borrow();
        let ram: u64 = cooked
            .layers
            .iter()
            .enumerate()
            .flat_map(|(li, l)| l.chunks.iter().map(move |c| (li, c)))
            .flat_map(|(li, c)| {
                let key = regia_terrain::ChunkKey {
                    layer: li as u16,
                    z: c.z,
                    x: c.x,
                };
                stream
                    .resident_lods(key)
                    .into_iter()
                    .map(|lod| held.get(&c.lods[usize::from(lod)].mesh).copied().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .sum();
        peak_ram = peak_ram.max(ram);
        // D9 (ADR-8873): the stream's own counter is the CPU copies it holds, counted here apart.
        assert_eq!(
            stream.stats().resident_bytes,
            ram,
            "eye {eye:?}: {:?}",
            stream.stats()
        );
    }
    let stats = stream.stats();
    eprintln!(
        "LAYER_MEMORY switchbacks chunks={} all_lods_bytes={all_lods} coarsest_bytes={} coarsest_resident_bytes={} budget_bytes={} peak_cooked={peak_cooked} peak_vram={peak_vram} peak_ram={peak_ram} eyes={} steps={steps}",
        cooked.chunk_count(),
        stats.coarsest_bytes,
        stats.coarsest_resident_bytes,
        stats.budget_bytes,
        eyes.len()
    );
    assert!(peak_vram > 0 && peak_ram > 0, "nothing streamed");
    assert!(
        peak_vram <= ULTRA_LOW_LAYER_VRAM,
        "layer VRAM peak {peak_vram} B over ULTRA_LOW's {ULTRA_LOW_LAYER_VRAM} B"
    );
    assert!(
        peak_ram <= ULTRA_LOW_LAYER_RAM,
        "layer RAM peak {peak_ram} B over ULTRA_LOW's {ULTRA_LOW_LAYER_RAM} B"
    );
    assert!(peak_vram.max(peak_ram) <= DECK_LAYER_BYTES);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plan D9 (ADR-8873): a chunk LOD loaded through the asset server (the level path,
/// `drive_layer_stream` with an `AssetServer`) is held once, by the stream. Before, the load was a
/// pinned server mesh: every LOD ever streamed kept a second decoded copy in the server for the
/// life of the server, evicted LODs included. Streaming the cooked Switchbacks layer along the
/// tubes through a server over the cook's cache leaves no mesh in the server and no pinned id.
#[test]
fn server_loaded_chunks_keep_one_cpu_copy() {
    let (cooked, cooked_dir, dir, eyes) = switchbacks();
    let root = dir.join("project");
    let platform = regia_asset::current_platform();
    let cache = regia_asset::cooked_cache_dir(&root, &platform);
    std::fs::create_dir_all(cache.parent().expect("cache parent")).expect("project dirs");
    std::fs::rename(&cooked_dir, &cache).expect("the cook's chunks become the project's cache");
    let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root,
        platform,
        content_dir: None,
    })
    .expect("server");
    let mut stream = TerrainLayerStream::default();
    let ids: Vec<AssetId> = cooked
        .layers
        .iter()
        .flat_map(|l| l.chunks.iter())
        .flat_map(|c| c.lods.iter().map(|l| l.mesh))
        .collect();
    {
        let mut fetch = |id: AssetId| regia_terrain::load_layer_chunk_mesh(Some(&mut server), None, id);
        for eye in eyes.iter().step_by(8) {
            for _ in 0..64 {
                stream.step(&cooked, &[*eye], &mut fetch);
                if stream.stats().pending == 0 {
                    break;
                }
            }
        }
    }
    let stats = stream.stats();
    eprintln!(
        "LAYER_SERVER_COPIES loads={} failed={} resident_bytes={} server_meshes={} pinned={}",
        stats.loads,
        stats.failed,
        stats.resident_bytes,
        server.meshes().len(),
        ids.iter().filter(|id| server.is_pinned(**id)).count()
    );
    assert!(
        stats.loads > 0 && stats.failed == 0 && stats.resident_bytes > 0,
        "{stats:?}"
    );
    assert_eq!(
        server.meshes().len(),
        0,
        "the server keeps a second copy of streamed chunk meshes ({} loads)",
        stats.loads
    );
    assert!(
        ids.iter().all(|id| !server.is_pinned(*id)),
        "a chunk mesh id is pinned in the server"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
