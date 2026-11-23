// SPDX-License-Identifier: MPL-2.0
//! Per-asset-type bytes against the heap ledger (spec 83 § 4.1, ADR-9740): with the tagging
//! allocator installed, the bytes `AssetServer::memory_by_type` lists add up to the growth of the
//! `assets` subsystem across the loads, within the documented slack (64 KiB plus 2 % of the
//! payload bytes: the server's id maps, load states and sources).
//!
//! One test in this binary, so no other test allocates under `assets` meanwhile.

use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh,
    cooked_cache_dir, cooked_file_name, encode_cooked_file, pack_importer_version,
};
use regia_core::memory::{self, Subsystem, TaggingAllocator};

#[global_allocator]
static ALLOC: TaggingAllocator = TaggingAllocator::system();

fn mesh_file(id: AssetId, triangles: u32) -> Vec<u8> {
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for t in 0..triangles {
        let base = positions.len() as u32;
        let z = t as f32;
        positions.extend_from_slice(&[[0.0, 0.0, z], [1.0, 0.0, z], [0.0, 1.0, z]]);
        indices.extend_from_slice(&[base, base + 1, base + 2]);
    }
    let n = positions.len();
    let mesh = CpuMesh {
        positions,
        normals: vec![[0.0, 0.0, 1.0]; n],
        uvs: vec![[0.0, 0.0]; n],
        tangents: Vec::new(),
        indices,
        ..Default::default()
    };
    let payload = mesh.to_postcard().expect("postcard");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh_legacy_cpu().0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).expect("encode")
}

#[test]
fn per_type_bytes_add_up_to_the_asset_subsystem_within_the_slack() {
    assert!(memory::tracking_installed());
    let root = regia_test_utils::unique_temp_dir("type-memory-ledger");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let ids: Vec<AssetId> = (0..6).map(|_| AssetId::new_random()).collect();
    for (k, id) in ids.iter().enumerate() {
        let triangles = 2_000 * (k as u32 + 1);
        std::fs::write(cache.join(cooked_file_name(*id)), mesh_file(*id, triangles)).expect("cooked mesh");
    }
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("open");
    let before = memory::snapshot().subsystem(Subsystem::Assets).live_bytes;
    for id in &ids {
        server.load_mesh(*id).expect("load");
    }
    let after = memory::snapshot().subsystem(Subsystem::Assets).live_bytes;
    let rows = server.memory_by_type();
    let mesh = rows.iter().find(|r| r.type_name == "mesh").expect("mesh row");
    assert_eq!(mesh.loaded, ids.len() as u64);
    let listed: u64 = rows.iter().map(|r| r.bytes).sum();
    let ledger = after.saturating_sub(before);
    let slack = 64 * 1024 + listed / 50;
    assert!(listed > 1_000_000, "the meshes hold real bytes: {listed}");
    assert!(
        ledger.abs_diff(listed) <= slack,
        "per-type bytes {listed} vs the assets ledger growth {ledger} (slack {slack})"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}
