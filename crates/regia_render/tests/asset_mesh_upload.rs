// SPDX-License-Identifier: MPL-2.0
//! Counted built-in meshes on a real device (ADR-7256): `AssetMeshUploads` uploads a mesh loaded
//! with `AssetServer::load::<CpuMesh>`, the server then drops the CPU streams, and dropping the last
//! handle destroys the GPU buffers after exactly the grace frames (counted in `update` calls, no
//! clock). Lavapipe is enough; the test skips without an adapter.

use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh,
    LoadSettings, cooked_cache_dir, cooked_file_name, encode_cooked_file, pack_importer_version,
};
use regia_render::{ASSET_MESH_VERTEX_STRIDE, AssetMeshUploads, RenderDevice};

fn write_mesh(cache: &std::path::Path, id: AssetId) {
    let mesh = CpuMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        uvs: vec![[0.0, 0.0]; 4],
        indices: vec![0, 1, 2, 2, 1, 3],
        ..Default::default()
    };
    let payload = mesh.to_postcard().expect("postcard");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh_legacy_cpu().0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let bytes = encode_cooked_file(&header, &payload).expect("encode");
    std::fs::write(cache.join(cooked_file_name(id)), bytes).expect("write");
}

#[test]
fn a_counted_mesh_frees_its_gpu_buffers_after_exactly_the_grace_frames() {
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let root = regia_test_utils::unique_temp_dir("render-asset-mesh-upload");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    let (id, kept) = (AssetId::new_random(), AssetId::new_random());
    write_mesh(&cache, id);
    write_mesh(&cache, kept);
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("open");
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    server.set_unload_grace_frames(4);

    let handle = server.load::<CpuMesh>(id).expect("load");
    let kept_handle = server
        .load_with::<CpuMesh>(kept, LoadSettings { keep_cpu_data: true })
        .expect("load kept");
    server.wait_for_loads();
    uploads.flush();
    let buffers = uploads.buffers(id).expect("uploaded");
    assert_eq!(buffers.index_count, 6);
    assert_eq!(buffers.vertex.size(), 4 * ASSET_MESH_VERTEX_STRIDE);
    assert_eq!(server.gpu_memory_use::<CpuMesh>(), 2 * buffers.bytes());
    let cpu = server.get(&handle).expect("value");
    assert!(
        cpu.positions.is_empty() && cpu.indices.is_empty(),
        "CPU streams dropped after upload"
    );
    assert_eq!(
        server.get(&kept_handle).map(|m| m.indices.len()),
        Some(6),
        "keep_cpu_data keeps them"
    );

    // Re-acquired inside the grace: kept, not uploaded again.
    drop(handle);
    for _ in 0..3 {
        server.update();
    }
    let again = server.load::<CpuMesh>(id).expect("again");
    for _ in 0..8 {
        server.update();
    }
    assert_eq!(uploads.counters(), (2, 0));

    drop(again);
    for frame in 1..=3 {
        server.update();
        assert!(
            uploads.buffers(id).is_some(),
            "still resident after {frame} of 4 grace frames"
        );
    }
    server.update();
    uploads.flush();
    assert!(uploads.buffers(id).is_none(), "freed on the fourth frame");
    assert_eq!(uploads.counters(), (2, 1));
    assert_eq!(uploads.resident_count(), 1, "the other mesh is still held");
    assert_eq!(server.gpu_memory_use::<CpuMesh>(), uploads.resident_bytes());
    drop(kept_handle);
    let _ = std::fs::remove_dir_all(&root);
}
