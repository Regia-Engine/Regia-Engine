// SPDX-License-Identifier: MPL-2.0
//! Steady state of the counted built-in handles (ADR-7256): with counted meshes held, a released
//! image counting down a long grace, and a memory budget set, a frame of `AssetServer::update`
//! allocates nothing. The tagging allocator counts every heap allocation in this binary.

regia_test_utils::install_tagging_allocator!();

use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader,
    CookedImage, CpuMesh, cooked_cache_dir, cooked_file_name, encode_cooked_file, pack_importer_version,
};
use regia_core::memory::{MemorySnapshot, snapshot};

fn allocations(snapshot: &MemorySnapshot) -> u64 {
    snapshot
        .subsystems
        .iter()
        .map(|(_, memory)| memory.total_allocations)
        .sum()
}

fn write(cache: &std::path::Path, id: AssetId, tag: AssetTypeTag, payload: &[u8]) {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: tag.0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let bytes = encode_cooked_file(&header, payload).expect("encode");
    std::fs::write(cache.join(cooked_file_name(id)), bytes).expect("write");
}

#[test]
fn a_frame_with_counted_built_in_handles_allocates_nothing() {
    let root = regia_test_utils::unique_temp_dir("builtin-steady-alloc");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let meshes: Vec<AssetId> = (0..4).map(|_| AssetId::new_random()).collect();
    for id in &meshes {
        let mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0; 2]; 3],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        write(
            &cache,
            *id,
            AssetTypeTag::mesh_legacy_cpu(),
            &mesh.to_postcard().expect("postcard"),
        );
    }
    let image = AssetId::new_random();
    let pixels = CookedImage {
        width: 2,
        height: 2,
        rgba8: vec![7; 16],
    };
    write(
        &cache,
        image,
        AssetTypeTag::image(),
        &pixels.to_postcard().expect("postcard"),
    );

    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("open");
    server.set_unload_grace_frames(100_000);
    let held: Vec<_> = meshes
        .iter()
        .map(|id| server.load::<CpuMesh>(*id).expect("mesh"))
        .collect();
    let released = server.load::<CookedImage>(image).expect("image");
    server.wait_for_loads();
    drop(released);
    server.set_memory_budget::<CpuMesh>(Some(1 << 30));
    server.set_memory_budget::<CookedImage>(Some(1 << 30));
    for _ in 0..4 {
        server.update();
    }
    assert!(server.is_pending_unload(image));
    for id in &meshes {
        assert_eq!(server.state(*id), regia_asset::LoadState::Loaded);
    }

    let before = snapshot();
    assert!(before.tracking, "the tagging allocator is not installed");
    for _ in 0..240 {
        server.update();
    }
    let after = snapshot();
    let made = allocations(&after).saturating_sub(allocations(&before));
    assert_eq!(made, 0, "steady frames allocated {made} times");
    assert!(server.is_pending_unload(image));
    drop(held);
    let _ = std::fs::remove_dir_all(&root);
}
