// SPDX-License-Identifier: MPL-2.0
//! Counted meshes on the visibility buffer's cluster path (ADR-7462): a counted mesh (no CPU
//! vertices) draws its clusters from its GPU upload like its CPU twin with the same cooked
//! meshlets, its GPU-written vertices survive the cluster arena's growth and a hot reload, and a
//! steady frame allocates no more than the CPU twin's. Lavapipe is enough; every count is by frame,
//! no clock.
//!
//! Its own test binary: it installs the shared counting allocator. The tests share
//! `tests/asset_cooked_model.rs`'s fixtures (copied) but not its process: in that binary, with
//! these tests added, w7's forward-profile check `an_app_material_draws_a_reloaded_counted_texture_on_the_next_frame`
//! failed 3 of 3 full serial runs with the constant +12 allocations per 4 frames ADR-7428 (3) left
//! open, and passed 4 of 4 without them (the two tests beside it alone pass; the trigger is the
//! process's test mix, not one test).
//!
//! The steady counts follow the shared rule of `regia_test_utils::alloc_counter` (ADR-5718,
//! ADR-7703): a frame's threads are counted together (the drawing thread and the renderer's encode
//! workers), the counted frame has no pixel readback, and every submit of the drawing thread waits
//! for the device. Counting the drawing thread alone, a 5090 moved a 37-allocation encode segment
//! between the drawing thread and a worker from frame to frame (which renderer's floor caught the
//! segment-free frame was chance: pregate card 6, 1f50665e0).

use std::path::Path;

use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader,
    CookedImage, CookedIndexWidth, CookedMesh, CookedMeshLod, CookedModel, CookedModelPart,
    CookedStandardMaterial, MaterialAssetRon, TextureSlot, cooked_cache_dir, cooked_file_name,
    encode_cooked_file,
};
use regia_render::cooked_model::{load_cooked_model, load_cooked_models_counted, model_id};
use regia_render::{
    AssetMeshUploads, DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabScene, RenderDevice,
    asset_mesh_key,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291, ADR-5718).
#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

/// The renderer the steady checks measure: its encode workers count their allocations with the
/// drawing thread's ([`alloc_counter::count_this_thread`]), so a segment that moves between them
/// does not move the count (ADR-2299). Set before the first frame: the workers start then.
fn counted_renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_encode_worker_start_hook(alloc_counter::count_this_thread);
    renderer
}

/// Serializes the tests that count allocations against every other test of this binary
/// (ADR-7428). The counter is per thread, but the App's executor and the task pools are shared:
/// another test's task can run on the measuring thread (a scope runs tasks on its caller while it
/// waits) and land in its count. A measuring test takes the write side, every other test the read
/// side, so the others still run in parallel among themselves.
static SERIAL: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// The write side of [`SERIAL`]: no other test of the binary runs meanwhile.
fn measuring() -> std::sync::RwLockWriteGuard<'static, ()> {
    // A test that panicked holding the lock poisons it; the lock guards no data.
    SERIAL.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The read side of [`SERIAL`]: runs beside other non-measuring tests.
fn sharing() -> std::sync::RwLockReadGuard<'static, ()> {
    SERIAL.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn put(cache: &Path, id: AssetId, tag: AssetTypeTag, payload: &[u8]) {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: tag.0,
        asset_id: id,
        importer_version: 1,
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, payload).expect("file");
    let path = cache.join(cooked_file_name(id));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, file).expect("write");
    std::fs::rename(&tmp, &path).expect("rename");
}

/// A compact cooked quad (16-bit indices, one LOD) tilted toward the camera, as the pipeline packs
/// it: the compact path rebuilds MikkTSpace tangents at load.
fn compact_quad() -> CookedMesh {
    let positions = [
        [-0.8f32, 0.0, -0.8],
        [0.8, 0.0, -0.8],
        [0.8, 0.1, 0.8],
        [-0.8, 0.1, 0.8],
    ];
    let uvs = [[0u16, 0], [0x3C00, 0], [0x3C00, 0x3C00], [0, 0x3C00]];
    let mut mesh = CookedMesh {
        aabb_min: [-0.8, 0.0, -0.8],
        aabb_max: [0.8, 0.1, 0.8],
        bounding_sphere: [0.0, 0.05, 0.0, 1.2],
        stream_positions: Vec::new(),
        stream_normals_tangents: Vec::new(),
        stream_uv0: Vec::new(),
        indices: Vec::new(),
        index_width: CookedIndexWidth::U16,
        vertex_count: 4,
        lods: vec![CookedMeshLod {
            screen_relative_size: 1.0,
            index_byte_offset: 0,
            index_byte_length: 12,
        }],
        submeshes: Vec::new(),
        lod_metric: regia_asset::LOD_METRIC_ERROR,
        format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
        skin: None,
        stream_color0: Vec::new(),
        morph_targets: Vec::new(),
        skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
    };
    for (p, uv) in positions.iter().zip(uvs) {
        for c in p {
            mesh.stream_positions.extend_from_slice(&c.to_le_bytes());
        }
        let (x, y) = regia_asset::encode_octahedral_normal([0.0, 0.995, -0.0995]);
        for w in [x, y, 0, -1] {
            mesh.stream_normals_tangents.extend_from_slice(&w.to_le_bytes());
        }
        for h in uv {
            mesh.stream_uv0.extend_from_slice(&h.to_le_bytes());
        }
    }
    for i in [0u16, 2, 1, 0, 3, 2] {
        mesh.indices.extend_from_slice(&i.to_le_bytes());
    }
    mesh
}

/// A cooked model in a dev cache: one compact quad with a material whose base color is a small
/// RGBA8 image (a checker, so a wrong UV or vertex shows in the pixels). Returns the project root,
/// the glTF id and the mesh id.
fn project() -> (std::path::PathBuf, AssetId, AssetId) {
    let (root, gltf, mesh, _) = project_with_image();
    (root, gltf, mesh)
}

/// [`project`] and the base-color image's id.
fn project_with_image() -> (std::path::PathBuf, AssetId, AssetId, AssetId) {
    project_with_material(|authored, image| {
        authored.base.base_color_map = Some(TextureSlot::Asset(image));
    })
}

/// [`project_with_image`] with the material's slots set by `slots` (given the image's id).
fn project_with_material(
    slots: impl FnOnce(&mut MaterialAssetRon, AssetId),
) -> (std::path::PathBuf, AssetId, AssetId, AssetId) {
    project_with(compact_quad(), slots)
}

/// [`project_with_material`] with `cooked` as the model's mesh.
fn project_with(
    cooked: CookedMesh,
    slots: impl FnOnce(&mut MaterialAssetRon, AssetId),
) -> (std::path::PathBuf, AssetId, AssetId, AssetId) {
    let root = regia_test_utils::unique_temp_dir("render-asset-cooked-model");
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    std::fs::create_dir_all(&cache).expect("cache dir");
    let (gltf, mesh, material, image) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    let mut rgba8 = Vec::new();
    for i in 0..16u8 {
        let on = (i % 4 + i / 4) % 2 == 0;
        rgba8.extend_from_slice(if on {
            &[230, 60, 40, 255]
        } else {
            &[40, 90, 230, 255]
        });
    }
    let pixels = CookedImage {
        width: 4,
        height: 4,
        rgba8,
    };
    put(
        &cache,
        image,
        AssetTypeTag::image(),
        &pixels.to_postcard().expect("image"),
    );
    let mut authored = MaterialAssetRon::default_standard();
    slots(&mut authored, image);
    put(
        &cache,
        material,
        AssetTypeTag::standard_material(),
        &CookedStandardMaterial::new(authored)
            .to_postcard()
            .expect("material"),
    );
    put(
        &cache,
        mesh,
        AssetTypeTag::mesh(),
        &cooked.to_postcard().expect("mesh"),
    );
    let model = CookedModel {
        version: regia_asset::COOKED_MODEL_VERSION,
        parts: vec![CookedModelPart {
            mesh,
            material: Some(material),
            node: "Quad".into(),
        }],
    };
    put(
        &cache,
        model_id(gltf),
        AssetTypeTag::model(),
        &model.to_postcard().expect("model"),
    );
    (root, gltf, mesh, image)
}

/// The shared headless device, or `None` (the test skips) when there is no adapter. With
/// `REGIA_GPU_STRICT=1` (a card run) a missing adapter fails the test instead of skipping it, so a
/// run that lost its adapters cannot count as passed.
fn adapter() -> Option<RenderDevice> {
    match RenderDevice::shared_headless() {
        Ok(device) => Some(device),
        Err(err) => {
            assert!(
                std::env::var("REGIA_GPU_STRICT").as_deref() != Ok("1"),
                "REGIA_GPU_STRICT=1 and no adapter: {err}"
            );
            eprintln!("SKIP: no adapter ({err})");
            None
        }
    }
}

fn open(root: &Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: regia_asset::current_platform(),
        content_dir: None,
    })
    .expect("open server")
}

/// The lab scene both paths draw (shadows on: the counted mesh casts too).
fn lab(materials: Vec<regia_render::LabMaterial>, meshes: Vec<regia_render::LabMesh>) -> LabScene {
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 2.2, 1.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 1.0, 1.0], 2.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Allocations per batch of `frames` steady frames after a warm-up, over `batches` batches: the
/// frame's recording and submission on the drawing thread and its encode workers together, without
/// the pixel readback (ADR-5718: a map right after a submit costs one allocation more while the
/// submission is in flight; the test's submits wait for the device, [`measured`]). Each frame ends
/// with the device drained, so every frame starts from the same wgpu pools.
fn steady_allocs(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &FrameScene<'_>,
    batches: usize,
    frames: u64,
) -> Vec<u64> {
    for _ in 0..8 {
        steady_frame(device, renderer, scene);
    }
    // wgpu's usage-scope pool at the size a frame can need, one scope per thread that finishes a
    // segment (ADR-5718): left alone it grows by 6 allocations in a frame the host's load picks.
    if !regia_test_utils::device_pools::fill_usage_scope_pool(&device.device, renderer.encode_threads() + 1) {
        eprintln!("the usage-scope pool was not filled: its growth can land in a counted batch");
    }
    for _ in 0..2 {
        steady_frame(device, renderer, scene);
    }
    (0..batches)
        .map(|_| steady_batch(device, renderer, scene, frames))
        .collect()
}

/// One steady frame: drawn without the pixel readback, then the device drained.
fn steady_frame(device: &RenderDevice, renderer: &mut FrameRenderer, scene: &FrameScene<'_>) {
    renderer
        .begin_offscreen(device, scene, wgpu::TextureFormat::Rgba8Unorm)
        .expect("steady frame");
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// One batch of [`steady_allocs`]: the counted threads' allocations over `frames` frames.
fn steady_batch(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &FrameScene<'_>,
    frames: u64,
) -> u64 {
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("idle");
    let before = alloc_counter::counted_allocs();
    for _ in 0..frames {
        steady_frame(device, renderer, scene);
    }
    alloc_counter::counted_allocs() - before
}

/// While alive, the calling (drawing) thread counts with the encode workers ([`counted_renderer`])
/// and each of its submits waits for the device (`RenderDevice::wait_after_submit_on_this_thread`,
/// ADR-5718). [`measured`] starts it.
struct Measured<'a>(&'a RenderDevice);

impl Drop for Measured<'_> {
    fn drop(&mut self) {
        self.0.wait_after_submit_on_this_thread(false);
        alloc_counter::stop_counting_this_thread();
    }
}

/// Turns on the shared rule's counting for the calling thread until the guard drops.
fn measured(device: &RenderDevice) -> Measured<'_> {
    alloc_counter::count_this_thread();
    device.wait_after_submit_on_this_thread(true);
    Measured(device)
}

/// The post stack the visibility-buffer path needs (a linear HDR frame), with nothing temporal.
fn vis_post() -> regia_render::PostSettings {
    regia_render::PostSettings {
        taa: false,
        auto_exposure: false,
        bloom: false,
        exposure_ev: 4.0,
        ..regia_render::PostSettings::default()
    }
}

/// The cooker's clusters of `vertices` / `indices` (positions only).
fn meshlets_of(
    vertices: &[regia_render::LabVertex],
    indices: &[u32],
) -> std::sync::Arc<regia_render::visbuffer::MeshletMesh> {
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let mesh = regia_asset_pipeline::build_meshlet_mesh(
        &positions,
        indices,
        &regia_asset_pipeline::MeshletSettings::default(),
    )
    .expect("meshlet cook");
    std::sync::Arc::new(mesh)
}

/// The second of two frames (a fresh renderer's first frame differs from its second on its own).
fn second_frame(device: &RenderDevice, renderer: &mut FrameRenderer, scene: &FrameScene<'_>) -> Vec<u8> {
    renderer.render_offscreen(device, scene).expect("frame");
    renderer.render_offscreen(device, scene).expect("frame").pixels
}

/// ADR-7462: on the visibility-buffer profile a counted mesh (no CPU vertices) draws through the
/// cluster path from its upload: one vertex repack, its clusters resident, no forward fallback,
/// and the same pixels as its CPU twin (`load_cooked_model`, the same vertices on the CPU) with the
/// same cooked meshlets on the same profile. Its GPU-written vertices survive the cluster arena's
/// growth (a dense mesh added later doubles the geometry stream; the mirror holds no copy of them),
/// and a steady frame allocates no more than the CPU twin's.
#[test]
fn a_counted_mesh_draws_its_clusters_from_the_upload_like_its_cpu_twin_on_the_visibility_buffer() {
    let _serial = measuring();
    let Some(device) = adapter() else {
        return;
    };
    let (root, gltf, mesh_id) = project();
    let post = vis_post();

    // The CPU twin: the same cooked mesh's vertices on the CPU, its cooked clusters inserted.
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let meshlets = meshlets_of(&draws.meshes[0].vertices, &draws.meshes[0].indices);
    let mut cpu_lab = lab(draws.materials.clone(), draws.meshes.clone());
    cpu_lab.post = Some(post.clone());
    let mut cpu_scene = cpu_lab.as_frame_scene();
    cpu_scene.quality.visibility_buffer = true;
    let mut cpu_renderer = counted_renderer();
    cpu_renderer.insert_meshlets(cpu_scene.meshes[0].mesh_key, std::sync::Arc::clone(&meshlets));
    let want = second_frame(&device, &mut cpu_renderer, &cpu_scene);
    let control = cpu_renderer.visbuffer_stats();
    if control.frames == 0 {
        eprintln!("SKIP: the device has no visibility-buffer path");
        return;
    }
    assert_eq!(
        control.resident_meshes, 1,
        "the CPU twin draws its clusters: {control:?}"
    );

    // The counted path: the same clusters under the asset's key.
    let mut server = open(&root);
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    let mut models = load_cooked_models_counted(&mut server, &[gltf]).expect("counted path");
    let model = models.remove(0);
    uploads.flush();
    let mut base = lab(model.materials.clone(), Vec::new());
    base.post = Some(post);
    let mut scene = base.as_frame_scene();
    scene.quality.visibility_buffer = true;
    model.push_frame_meshes(cpu_scene.meshes[0].model, 0x0A55_E701, &mut scene.meshes);
    assert!(scene.meshes[0].vertices.is_empty(), "no CPU vertices in the draw");
    assert_eq!(scene.meshes[0].mesh_key, asset_mesh_key(mesh_id));
    let mut renderer = counted_renderer();
    renderer.set_asset_meshes(Some(uploads.clone()));
    assert_eq!(
        renderer.asset_mesh_counts(asset_mesh_key(mesh_id)),
        Some((meshlets.base_vertex_count(), meshlets.source_index_count as usize)),
        "the upload's counts are the clusters'"
    );
    renderer.insert_meshlets(asset_mesh_key(mesh_id), std::sync::Arc::clone(&meshlets));
    let got = second_frame(&device, &mut renderer, &scene);
    let stats = renderer.visbuffer_stats();
    assert_eq!(
        (stats.resident_meshes, stats.instances),
        (control.resident_meshes, control.instances),
        "the counted mesh takes the cluster path: {stats:?}"
    );
    let counted = renderer.counted_vis_stats();
    let clusters = renderer.visbuffer_arena_stats().clusters;
    assert_eq!(
        counted,
        regia_render::visbuffer::CountedVisStats {
            meshes: 1,
            clusters,
            repacks: 1,
            forward_fallbacks: 0,
            kept_meshlet_bytes: (std::mem::size_of_val(meshlets.meshlets.as_slice())
                + std::mem::size_of_val(meshlets.vertices.as_slice())
                + std::mem::size_of_val(meshlets.triangles.as_slice()))
                as u64,
        },
        "one repack, no forward fallback, the kept cooked clusters reported (ADR-7542)"
    );
    assert_eq!(clusters, cpu_renderer.visbuffer_arena_stats().clusters);
    assert_eq!(got.len(), want.len());
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 0, "the counted clusters draw the CPU twin's pixels");
    let corner = &want[..4];
    let covered = want
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px.as_slice() != corner)
        .count();
    assert!(covered > 64 * 64 / 8, "the quad is drawn: {covered} pixels");

    // A dense rock added to both frames doubles the geometry stream: the counted quad's
    // GPU-written vertices come over with the GPU copy (the mirror has zeros there).
    let (rock_vertices, rock_indices) = regia_render::uv_sphere(0.3, 96, 48);
    let rock_meshlets = meshlets_of(&rock_vertices, &rock_indices);
    let rock_key = 0x0B0C_4000_0000_0008;
    let growths = (
        cpu_renderer.visbuffer_arena_stats().growths,
        renderer.visbuffer_arena_stats().growths,
    );
    let rock = |template: &FrameMesh<'_>| -> [[f32; 4]; 4] {
        let mut m = template.model;
        m[3][0] += 0.9;
        m[3][2] -= 0.6;
        m
    };
    for (r, s) in [(&mut cpu_renderer, &mut cpu_scene), (&mut renderer, &mut scene)] {
        r.insert_meshlets(rock_key, std::sync::Arc::clone(&rock_meshlets));
        let template = s.meshes[0];
        s.meshes.push(FrameMesh {
            mesh_key: rock_key,
            object_key: 0x0B0C_0004,
            vertices: &rock_vertices,
            indices: &rock_indices,
            model: rock(&template),
            ..template
        });
    }
    let want = second_frame(&device, &mut cpu_renderer, &cpu_scene);
    let got = second_frame(&device, &mut renderer, &scene);
    assert!(
        renderer.visbuffer_arena_stats().growths > growths.1
            && cpu_renderer.visbuffer_arena_stats().growths > growths.0,
        "the rock grew both arenas"
    );
    assert_eq!(renderer.visbuffer_stats().resident_meshes, 2);
    assert_eq!(
        renderer.counted_vis_stats().repacks,
        1,
        "the growth repacks nothing"
    );
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "after the growth the counted clusters still draw the CPU twin's pixels"
    );

    // Steady frames on the cluster path allocate nothing new: the counted mesh allocates no more
    // than its CPU twin. Counted by the shared rule (ADR-5718, ADR-7703: [`measured`]): the drawing
    // thread and the encode workers together, so the 37-allocation segment a worker sometimes
    // finishes instead of the drawing thread stays in every frame's count.
    // (1) Per frame, over 64 frames: a constant per-frame addition is in every frame, the floor
    // included.
    let measuring_threads = measured(&device);
    let cpu_counts = steady_allocs(&device, &mut cpu_renderer, &cpu_scene, 64, 1);
    let counted_counts = steady_allocs(&device, &mut renderer, &scene, 64, 1);
    eprintln!(
        "visibility-buffer steady allocations per frame: cpu {cpu_counts:?}, counted {counted_counts:?}"
    );
    let floor = |c: &[u64]| c.iter().copied().min().unwrap_or(0);
    assert!(
        floor(&counted_counts) <= floor(&cpu_counts),
        "a counted mesh on the cluster path adds per-frame allocations: {counted_counts:?} vs {cpu_counts:?}"
    );
    // (2) Per 4 frames, w7's batch comparison (ADR-7428 (3)) on this profile: an allocation every
    // second, third or fourth frame is in every batch, the floor included, which (1) cannot see.
    // The batches interleave the two renderers so both run under the same load.
    let (mut cpu_batches, mut counted_batches) = (Vec::new(), Vec::new());
    for _ in 0..32 {
        cpu_batches.push(steady_batch(&device, &mut cpu_renderer, &cpu_scene, 4));
        counted_batches.push(steady_batch(&device, &mut renderer, &scene, 4));
    }
    eprintln!(
        "visibility-buffer steady allocations per 4 frames: cpu {cpu_batches:?}, counted {counted_batches:?}"
    );
    assert!(
        floor(&counted_batches) <= floor(&cpu_batches),
        "a counted mesh on the cluster path adds allocations every few frames: {counted_batches:?} vs {cpu_batches:?}"
    );
    drop(measuring_threads);
    assert_eq!(
        renderer.counted_vis_stats().repacks,
        1,
        "steady frames repack nothing"
    );
    assert_eq!(renderer.counted_vis_stats().forward_fallbacks, 0);
    assert_eq!(uploads.counters(), (1, 0), "frames upload nothing");
    drop((model, server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-7462: a hot reload of a counted mesh on the visibility-buffer profile keeps its cooked
/// clusters. The renderer drops the old ones with the old upload, inserts the meshlets it drew with
/// again, and repacks the new upload's vertices once (the reload keeps the counts): the frame draws
/// the new geometry through the cluster path with the pixels of a CPU twin of the new vertices with
/// the same meshlets, at the same frame number.
#[test]
fn a_reloaded_counted_mesh_keeps_its_clusters_on_the_visibility_buffer() {
    let _serial = sharing();
    let Some(device) = adapter() else {
        return;
    };
    let (root, gltf, mesh_id) = project();
    let post = vis_post();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let meshlets = meshlets_of(&draws.meshes[0].vertices, &draws.meshes[0].indices);
    let model_matrix = lab(draws.materials.clone(), draws.meshes.clone()).meshes[0].model;

    let mut server = open(&root);
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    let model = load_cooked_models_counted(&mut server, &[gltf])
        .expect("counted path")
        .remove(0);
    let mut base = lab(model.materials.clone(), Vec::new());
    base.post = Some(post.clone());
    let mut scene = base.as_frame_scene();
    scene.quality.visibility_buffer = true;
    model.push_frame_meshes(model_matrix, 0x0A55_E703, &mut scene.meshes);
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    renderer.insert_meshlets(asset_mesh_key(mesh_id), std::sync::Arc::clone(&meshlets));
    let before = second_frame(&device, &mut renderer, &scene);
    if renderer.visbuffer_stats().frames == 0 {
        eprintln!("SKIP: the device has no visibility-buffer path");
        return;
    }
    assert_eq!(renderer.counted_vis_stats().repacks, 1);

    // The cooked mesh shrinks to half its size (the same counts): a reload uploads it again.
    let mut half = compact_quad();
    let mut positions = Vec::new();
    for c in half.stream_positions.as_chunks::<4>().0 {
        positions.extend_from_slice(&(f32::from_le_bytes(*c) * 0.5).to_le_bytes());
    }
    half.stream_positions = positions;
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    put(
        &cache,
        mesh_id,
        AssetTypeTag::mesh(),
        &half.to_postcard().expect("mesh"),
    );
    server.reload_id(mesh_id).expect("reload");
    assert_eq!(uploads.counters(), (2, 0), "uploaded again, nothing unloaded");
    let got = second_frame(&device, &mut renderer, &scene);
    assert_ne!(got, before, "the reloaded geometry is drawn");
    let stats = renderer.counted_vis_stats();
    assert_eq!(
        (stats.meshes, stats.repacks, stats.forward_fallbacks),
        (1, 2, 0),
        "the reload keeps the clusters and repacks the new upload once: {stats:?}"
    );
    assert_eq!(renderer.visbuffer_stats().resident_meshes, 1);

    // The CPU twin of the reloaded mesh with the same meshlets, at the same frame number.
    let mut new_server = open(&root);
    let reloaded = load_cooked_model(&mut new_server, gltf).expect("cpu path of the reload");
    let mut cpu_lab = lab(reloaded.materials.clone(), reloaded.meshes.clone());
    cpu_lab.post = Some(post);
    let mut cpu_scene = cpu_lab.as_frame_scene();
    cpu_scene.quality.visibility_buffer = true;
    let mut cpu_renderer = FrameRenderer::new();
    cpu_renderer.insert_meshlets(cpu_scene.meshes[0].mesh_key, std::sync::Arc::clone(&meshlets));
    second_frame(&device, &mut cpu_renderer, &cpu_scene);
    let want = second_frame(&device, &mut cpu_renderer, &cpu_scene);
    assert_eq!(cpu_renderer.visbuffer_stats().resident_meshes, 1);
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the reloaded counted clusters draw the CPU twin's pixels"
    );
    drop((model, server, cpu_server, new_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// [`compact_quad`] re-cooked with its vertices 1 and 2 swapped (the index list follows them): the
/// same quad and the same vertex and index counts, but other cooked clusters. The quad's old
/// clusters over these vertices draw other triangles (one back-facing, one half of the quad).
fn swapped_quad() -> CookedMesh {
    let mut mesh = compact_quad();
    let swap = |stream: &mut Vec<u8>, stride: usize| {
        let (one, two) = (
            stream[stride..2 * stride].to_vec(),
            stream[2 * stride..3 * stride].to_vec(),
        );
        stream[stride..2 * stride].copy_from_slice(&two);
        stream[2 * stride..3 * stride].copy_from_slice(&one);
    };
    swap(&mut mesh.stream_positions, 12);
    swap(&mut mesh.stream_normals_tangents, 8);
    swap(&mut mesh.stream_uv0, 4);
    mesh.indices.clear();
    for i in [0u16, 1, 2, 0, 3, 1] {
        mesh.indices.extend_from_slice(&i.to_le_bytes());
    }
    mesh
}

/// ADR-7462, rev-claude-60 finding 1: one re-import re-cooks a counted mesh's `.fmesh` and its
/// `.fmlt` with the same counts but other clusters, and both reload in the same App update. The
/// renderer's release of the old upload (`CountedMeshPlugin`) and the meshlet bridge's
/// replacement of the clusters run in either order; in both, the next frame draws the NEW clusters
/// through the cluster path (resident, one more repack, no forward fallback) with the pixels of a
/// CPU twin of the re-cooked mesh with the new clusters, at the same frame number. Before the fix,
/// with the bridge first, the release deleted the new clusters (the mesh drew forward until the
/// next re-cook).
#[test]
fn a_counted_mesh_reloaded_with_new_meshlets_in_one_update_draws_them_in_either_order() {
    let _serial = sharing();
    let Some(device) = adapter() else {
        return;
    };
    for bridge_first in [false, true] {
        let (root, gltf, mesh_id) = project();
        let post = vis_post();
        let mut cpu_server = open(&root);
        let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
        let old_meshlets = meshlets_of(&draws.meshes[0].vertices, &draws.meshes[0].indices);
        let model_matrix = lab(draws.materials.clone(), draws.meshes.clone()).meshes[0].model;

        let mut server = open(&root);
        let uploads = AssetMeshUploads::new(&device);
        server.register_prepare(uploads.clone());
        let model = load_cooked_models_counted(&mut server, &[gltf])
            .expect("counted path")
            .remove(0);
        let mut base = lab(model.materials.clone(), Vec::new());
        base.post = Some(post.clone());
        let mut scene = base.as_frame_scene();
        scene.quality.visibility_buffer = true;
        model.push_frame_meshes(model_matrix, 0x0A55_E705, &mut scene.meshes);
        let key = asset_mesh_key(mesh_id);
        let mut renderer = FrameRenderer::new();
        renderer.set_asset_meshes(Some(uploads.clone()));
        renderer.insert_meshlets(key, std::sync::Arc::clone(&old_meshlets));
        second_frame(&device, &mut renderer, &scene);
        if renderer.visbuffer_stats().frames == 0 {
            eprintln!("SKIP: the device has no visibility-buffer path");
            return;
        }
        assert_eq!(renderer.counted_vis_stats().repacks, 1);

        // The re-import: the same counts, other clusters.
        let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
        put(
            &cache,
            mesh_id,
            AssetTypeTag::mesh(),
            &swapped_quad().to_postcard().expect("mesh"),
        );
        server.reload_id(mesh_id).expect("reload");
        assert_eq!(uploads.counters(), (2, 0), "uploaded again, nothing unloaded");
        let mut new_server = open(&root);
        let reloaded = load_cooked_model(&mut new_server, gltf).expect("cpu path of the re-cook");
        let new_meshlets = meshlets_of(&reloaded.meshes[0].vertices, &reloaded.meshes[0].indices);
        assert_eq!(
            (new_meshlets.base_vertex_count(), new_meshlets.source_index_count),
            (old_meshlets.base_vertex_count(), old_meshlets.source_index_count),
            "the same counts"
        );
        assert!(*new_meshlets != *old_meshlets, "other clusters");

        // The update: the renderer's release of the old upload and the bridge's replacement.
        if !bridge_first {
            renderer.release_unloaded_asset_meshes();
        }
        renderer.remove_meshlets(key);
        renderer.insert_meshlets(key, std::sync::Arc::clone(&new_meshlets));
        if bridge_first {
            renderer.release_unloaded_asset_meshes();
        }
        let got = second_frame(&device, &mut renderer, &scene);
        let stats = renderer.counted_vis_stats();
        assert_eq!(
            (stats.meshes, stats.repacks, stats.forward_fallbacks),
            (1, 2, 0),
            "bridge first: {bridge_first}: the new clusters are resident and repacked once: {stats:?}"
        );
        assert_eq!(renderer.visbuffer_stats().resident_meshes, 1);

        // The CPU twin of the re-cooked mesh with the new clusters, at the same frame number.
        let mut cpu_lab = lab(reloaded.materials.clone(), reloaded.meshes.clone());
        cpu_lab.post = Some(post);
        let mut cpu_scene = cpu_lab.as_frame_scene();
        cpu_scene.quality.visibility_buffer = true;
        let mut cpu_renderer = FrameRenderer::new();
        cpu_renderer.insert_meshlets(cpu_scene.meshes[0].mesh_key, std::sync::Arc::clone(&new_meshlets));
        second_frame(&device, &mut cpu_renderer, &cpu_scene);
        let want = second_frame(&device, &mut cpu_renderer, &cpu_scene);
        assert_eq!(cpu_renderer.visbuffer_stats().resident_meshes, 1);
        let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(
            differing, 0,
            "bridge first: {bridge_first}: the re-cooked clusters draw the CPU twin's pixels"
        );
        drop((model, server, cpu_server, new_server));
        let _ = std::fs::remove_dir_all(&root);
    }
}
