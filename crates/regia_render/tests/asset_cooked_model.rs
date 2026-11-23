// SPDX-License-Identifier: MPL-2.0
//! The frame renderer's cooked-model path on counted meshes (ADR-7284): a cooked model loaded with
//! `load_cooked_models_counted` uploads each mesh once through `AssetMeshUploads`, the asset server
//! drops the CPU streams, `assets.handles` and the per-type budget count the upload once, the frame
//! renderer draws it from the uploaded buffers byte-identically to the CPU path
//! (`load_cooked_model`), dropping the last handle frees it after the grace frames, and a steady
//! frame allocates no more than the same frame on the CPU path. Lavapipe is enough; every count is
//! by `update` call or frame, no clock.
//!
//! Its own test binary: it installs a counting global allocator.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;

use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader,
    CookedImage, CookedIndexWidth, CookedMesh, CookedMeshLod, CookedModel, CookedModelPart,
    CookedStandardMaterial, CpuMesh, MaterialAssetRon, TextureSlot, cooked_cache_dir, cooked_file_name,
    encode_cooked_file,
};
use regia_render::cooked_model::{load_cooked_model, load_cooked_models_counted, model_id};
use regia_render::{
    AssetMeshUploads, DirectionalLightUniform, FrameMesh, FrameRenderer, LabScene, RenderDevice,
    asset_mesh_key,
};

struct Counting;
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

/// Allocations on every thread of the process (ADR-8635). [`paired_frame_allocs`] counts a frame's
/// work with this one: a frame runs part of its work as tasks on the task pools, and the measuring
/// thread runs whichever of them it picks up while it waits, so the per-thread [`allocs`] moved by
/// a task's allocations (+24, +33, +57 on about half the frames of either path) with no change in
/// either path. Only the measuring tests use it, while they hold [`measuring`], so no other test's
/// work runs meanwhile.
static ALL_ALLOCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn all_allocs() -> u64 {
    ALL_ALLOCS.load(std::sync::atomic::Ordering::SeqCst)
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        ALL_ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        ALL_ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

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

/// A measuring test's own device (ADR-8635), opened while it holds [`measuring`].
///
/// wgpu-core sizes a command buffer's resource trackers to the highest tracker index a frame
/// touches, growing them by amortized reallocations. On the binary's shared device
/// (`RenderDevice::shared_headless`) the indices of a path's buffers depend on what the tests that
/// ran earlier created and freed, so one path's frames could land one capacity step higher than
/// the other's and count 1-4 more allocations per frame with no change in either path (traced in
/// the v06-pregate-reds-d lane: `Vec<Option<Arc<Buffer>>>` and `Vec<BufferUses>` resizes). A fresh
/// device gives both paths the same index space on every run. No other test of the binary runs
/// while [`measuring`] is held, so creating and dropping this device never overlaps another
/// test's GPU work (the ADR-1439 / ADR-2113 condition). Declare the device after the guard so it
/// drops first.
fn own_device() -> Result<RenderDevice, regia_render::RenderError> {
    RenderDevice::headless()
}

/// The read side of [`SERIAL`]: runs beside other non-measuring tests.
fn sharing() -> std::sync::RwLockReadGuard<'static, ()> {
    SERIAL.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

const GRACE: u32 = 3;

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

/// Warm-up frames of each path before [`paired_frame_allocs`] counts.
const WARM_UP: usize = 8;
/// Rounds of [`paired_frame_allocs`]: one counted frame of each path per round.
const ROUNDS: usize = 48;

/// Single steady frames of the CPU path and the counted path, measured the same way (ADR-8598):
/// after [`WARM_UP`] frames of each, [`ROUNDS`] rounds of one frame of each, the order swapped
/// every round (so a device-wide event on every other submit cannot land on one path only).
/// Returns each path's allocation count per frame.
///
/// A frame's count is not a constant: work that the GPU finished before or after a check inside
/// the frame (a readback's map callback, a ring slot freed) takes a different branch, so some
/// frames of either path count a few dozen more or less (pregate 0f89ab970 card 6, 4-frame sums:
/// cpu `[1635, 1604, 1604, 1637, 1604, ...]`, counted `[1604, 1605, 1691, 1666, ...]`). The old
/// check compared the lowest 4-frame sum of each path, which one lucky or unlucky batch moves:
/// it failed both ways on cards and on lavapipe with no change in either path.
///
/// The count is process-wide ([`all_allocs`], ADR-8635), so a frame counts its pool tasks whichever
/// thread runs them. Before each counted frame the device is waited idle (outside the count), so the
/// map callbacks and frees of earlier frames' GPU work run there, not inside the next frame.
fn paired_frame_allocs(
    device: &RenderDevice,
    mut cpu: impl FnMut(),
    mut counted: impl FnMut(),
) -> (Vec<u64>, Vec<u64>) {
    for _ in 0..WARM_UP {
        cpu();
        counted();
    }
    let count = |frame: &mut dyn FnMut()| {
        device
            .wait_idle()
            .expect("settle the device before a counted frame");
        let before = all_allocs();
        frame();
        all_allocs() - before
    };
    let (mut cpu_counts, mut counted_counts) = (Vec::with_capacity(ROUNDS), Vec::with_capacity(ROUNDS));
    for round in 0..ROUNDS {
        if round.is_multiple_of(2) {
            cpu_counts.push(count(&mut cpu));
            counted_counts.push(count(&mut counted));
        } else {
            counted_counts.push(count(&mut counted));
            cpu_counts.push(count(&mut cpu));
        }
    }
    (cpu_counts, counted_counts)
}

/// The median of a path's per-frame counts, for the log line.
fn median(counts: &[u64]) -> u64 {
    let mut sorted = counts.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// Per pair of rounds (`2k`, `2k + 1`) of [`paired_frame_allocs`]: the counted path's two frames
/// minus the CPU path's two frames (ADR-8635).
///
/// Each path's counts alternate with the device's submit parity, not with the path: the first
/// frame of a round counts 3 more than the second whichever path draws it (RTX 5090, counted model:
/// `[215, 212, 215, 212, ...]` for the path that went first in even rounds, `[212, 215, ...]` for
/// the other). Half of each path's frames sit in each level, so a per-path median lands on the
/// boundary and flips with one frame. The order swaps every round, so in a pair of rounds each path
/// draws once first and once second, and the parity cancels.
fn pair_differences(cpu: &[u64], counted: &[u64]) -> Vec<i64> {
    let sums = |counts: &[u64]| -> Vec<i64> {
        counts
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[a, b]| (a + b) as i64)
            .collect()
    };
    sums(counted).iter().zip(sums(cpu)).map(|(c, p)| c - p).collect()
}

/// The most the typical pair difference may be (ADR-8635). One allocation the counted path adds
/// per frame moves every pair difference by 2, one added every other frame by 1; the jitter left
/// in steady frames is one allocation either way on either path, which averages out near 0.
const MOST_TYPICAL_DIFFERENCE: f64 = 0.5;

/// The typical pair difference: the mean of the middle half of [`pair_differences`] (a quarter
/// dropped at each end, so a few frames where GPU work finished at another moment cannot move it).
fn typical_difference(differences: &[i64]) -> f64 {
    let mut sorted = differences.to_vec();
    sorted.sort_unstable();
    let quarter = sorted.len() / 4;
    let middle = &sorted[quarter..sorted.len() - quarter];
    if middle.is_empty() {
        return 0.0;
    }
    middle.iter().sum::<i64>() as f64 / middle.len() as f64
}

/// Asserts that the counted path's steady frames allocate no more than the CPU path's: the typical
/// pair difference is at most [`MOST_TYPICAL_DIFFERENCE`].
fn assert_no_added_frame_allocs(what: &str, cpu: &[u64], counted: &[u64]) {
    let differences = pair_differences(cpu, counted);
    let typical = typical_difference(&differences);
    eprintln!(
        "steady allocations per frame ({what}): typical pair difference {typical:.3} {differences:?}; cpu \
         median {} {cpu:?}, counted median {} {counted:?}",
        median(cpu),
        median(counted)
    );
    assert!(
        typical <= MOST_TYPICAL_DIFFERENCE,
        "{what} adds per-frame allocations: typical pair difference {typical:.3} over \
         {MOST_TYPICAL_DIFFERENCE} ({differences:?}; counted {counted:?} vs cpu {cpu:?})"
    );
}

/// The check itself, on counts recorded from this binary (RTX 5090 idx 0, load ~300, process-wide
/// counts, the counted texture of `an_app_material_draws_a_reloaded_counted_texture_on_the_next_frame`):
/// they pass; the same counted frames with one more allocation each, or one more every other
/// frame, fail; four pairs hit by a large outlier on the counted path still pass.
#[test]
fn the_steady_frame_check_passes_recorded_frames_and_fails_one_added_allocation() {
    let cpu: [u64; 48] = [
        542, 539, 542, 540, 543, 540, 542, 540, 543, 540, 542, 540, 542, 540, 543, 540, 543, 539, 542, 540,
        543, 539, 542, 539, 542, 540, 542, 540, 543, 540, 543, 539, 542, 539, 543, 539, 542, 539, 542, 540,
        542, 539, 542, 539, 542, 539, 542, 539,
    ];
    let counted: [u64; 48] = [
        539, 542, 539, 542, 540, 543, 540, 543, 540, 542, 539, 542, 539, 543, 540, 543, 540, 542, 539, 542,
        540, 543, 540, 542, 539, 543, 539, 542, 540, 543, 540, 543, 539, 542, 540, 543, 539, 542, 539, 542,
        540, 542, 539, 542, 539, 542, 540, 543,
    ];
    let fails = |counted: &[u64]| {
        std::panic::catch_unwind(|| assert_no_added_frame_allocs("synthetic", &cpu, counted)).is_err()
    };
    assert_no_added_frame_allocs("recorded", &cpu, &counted);
    let one_more: Vec<u64> = counted.iter().map(|c| c + 1).collect();
    assert!(fails(&one_more), "one added allocation per frame fails the check");
    let every_other: Vec<u64> = counted
        .iter()
        .enumerate()
        .map(|(i, c)| c + (i % 2) as u64)
        .collect();
    assert!(
        fails(&every_other),
        "one added allocation every other frame fails the check"
    );
    let one_fewer: Vec<u64> = counted.iter().map(|c| c - 1).collect();
    assert!(!fails(&one_fewer), "one allocation fewer per frame passes");
    let outliers: Vec<u64> = counted
        .iter()
        .enumerate()
        .map(|(i, c)| if i < 8 && i.is_multiple_of(2) { c + 140 } else { *c })
        .collect();
    assert!(
        !fails(&outliers),
        "four outlier pairs on the counted path do not decide the check"
    );
    assert!(typical_difference(&[]).abs() < f64::EPSILON);
}

#[test]
fn a_counted_cooked_model_draws_from_its_upload_like_the_cpu_path_and_frees_after_the_grace() {
    let _serial = measuring();
    let Ok(device) = own_device() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, mesh_id) = project();

    // The CPU path (the base renderer's): `load_cooked_model` builds LabMeshes.
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_lab = lab(draws.materials.clone(), draws.meshes.clone());
    let cpu_scene = cpu_lab.as_frame_scene();
    let mut cpu_renderer = FrameRenderer::new();
    let want = cpu_renderer
        .render_offscreen(&device, &cpu_scene)
        .expect("cpu frame")
        .pixels;

    // The counted path.
    let mut server = open(&root);
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    server.set_unload_grace_frames(GRACE);
    let mut models = load_cooked_models_counted(&mut server, &[gltf]).expect("counted path");
    assert_eq!(models.len(), 1);
    let model = models.remove(0);
    assert_eq!(model.parts.len(), 1);
    assert_eq!(model.parts[0].mesh.id(), mesh_id);
    uploads.flush();

    // Uploaded once; the CPU streams dropped; counted once in the report and the budget.
    assert_eq!(uploads.counters(), (1, 0), "one upload");
    let cpu = server.get(&model.parts[0].mesh).expect("loaded");
    assert!(
        cpu.positions.is_empty() && cpu.indices.is_empty(),
        "the CPU mesh is dropped after the upload"
    );
    assert_eq!(server.cpu_data_kept(mesh_id), Some(false));
    let gpu = uploads.resident_bytes();
    assert!(gpu > 0);
    assert_eq!(server.gpu_memory_use::<CpuMesh>(), gpu);
    assert_eq!(server.memory_use::<CpuMesh>(), 0, "no CPU bytes left");
    assert_eq!(
        server.budgeted_memory_use::<CpuMesh>(),
        gpu,
        "the budget counts the upload once"
    );
    let report = server.handle_report(Some("mesh"));
    let rows: Vec<_> = report.assets.iter().filter(|r| r.id == mesh_id).collect();
    assert_eq!(rows.len(), 1, "one row for the mesh");
    assert_eq!(
        (rows[0].strong, rows[0].gpu_bytes, rows[0].cpu_bytes),
        (1, gpu, 0)
    );
    assert!(!rows[0].cpu_data_kept);
    let kind = report.kinds.iter().find(|k| k.kind == "mesh").expect("mesh kind");
    assert_eq!((kind.loaded, kind.gpu_bytes, kind.cpu_bytes), (1, gpu, 0));

    // The frame renderer draws it from the upload: byte-identical to the CPU path.
    let base = lab(model.materials.clone(), Vec::new());
    let mut scene = base.as_frame_scene();
    model.push_frame_meshes(cpu_scene.meshes[0].model, 0x0A55_E701, &mut scene.meshes);
    assert_eq!(scene.meshes.len(), 1);
    assert!(scene.meshes[0].vertices.is_empty(), "no CPU vertices in the draw");
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    let got = renderer
        .render_offscreen(&device, &scene)
        .expect("counted frame")
        .pixels;
    assert_eq!(renderer.adopted_asset_mesh_count(), 1);
    assert_eq!(got.len(), want.len());
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 0, "the counted draw is byte-identical to the CPU path");
    let corner = &want[..4];
    let covered = want
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px.as_slice() != corner)
        .count();
    assert!(covered > 64 * 64 / 8, "the quad is drawn: {covered} pixels");

    // A steady frame on the counted path allocates no more than the CPU path's.
    let (cpu_counts, counted_counts) = paired_frame_allocs(
        &device,
        || {
            cpu_renderer
                .render_offscreen(&device, &cpu_scene)
                .expect("steady cpu frame");
        },
        || {
            renderer
                .render_offscreen(&device, &scene)
                .expect("steady counted frame");
        },
    );
    assert_no_added_frame_allocs("a counted model", &cpu_counts, &counted_counts);
    assert_eq!(uploads.counters(), (1, 0), "frames upload nothing");

    // Dropping the last handle frees the upload after exactly the grace frames.
    let materials = model.materials.clone();
    drop(model);
    for frame in 1..GRACE {
        server.update();
        assert!(
            uploads.buffers(mesh_id).is_some(),
            "still resident after {frame} of {GRACE} grace frames"
        );
    }
    server.update();
    assert!(
        uploads.buffers(mesh_id).is_none(),
        "freed on the last grace frame"
    );
    assert_eq!(uploads.counters(), (1, 1));
    assert_eq!(server.gpu_memory_use::<CpuMesh>(), 0);

    // The renderer drops its entry at the next frame; a draw of the unloaded key draws nothing.
    let base = lab(materials, Vec::new());
    let mut scene = base.as_frame_scene();
    let template = FrameMesh {
        mesh_key: asset_mesh_key(mesh_id),
        ..cpu_scene.meshes[0]
    };
    scene.meshes.push(FrameMesh {
        vertices: &[],
        indices: &[],
        material: &base.materials[0],
        ..template
    });
    let empty = renderer
        .render_offscreen(&device, &scene)
        .expect("frame after unload")
        .pixels;
    assert_eq!(
        renderer.adopted_asset_mesh_count(),
        0,
        "the entry went with the asset"
    );
    assert!(
        empty
            .as_chunks::<4>()
            .0
            .iter()
            .all(|px| px.as_slice() == &empty[..4]),
        "an unloaded counted mesh is not drawn"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A hot reload of a counted mesh uploads it again under the same frame key; the renderer drops its
/// adopted entry at the start of the next frame and draws the new buffers (the new geometry), with
/// no CPU vertices.
#[test]
fn a_reloaded_counted_mesh_draws_its_new_upload() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, mesh_id) = project();
    let mut server = open(&root);
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    let model = load_cooked_models_counted(&mut server, &[gltf])
        .expect("counted path")
        .remove(0);
    let base = lab(model.materials.clone(), Vec::new());
    let mut scene = base.as_frame_scene();
    model.push_frame_meshes(
        [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        0x0A55_E702,
        &mut scene.meshes,
    );
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    let before = renderer.render_offscreen(&device, &scene).expect("frame").pixels;

    // The cooked mesh shrinks to half its size: a reload uploads it again.
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
    assert!(
        server
            .get(&model.parts[0].mesh)
            .is_some_and(|m| m.positions.is_empty()),
        "the reloaded CPU copy is dropped too"
    );

    let after = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
    assert_eq!(renderer.adopted_asset_mesh_count(), 1);
    let corner = &before[..4];
    let covered = |px: &[u8]| {
        px.as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p.as_slice() != corner)
            .count()
    };
    assert!(
        covered(&after) < covered(&before),
        "the smaller reloaded mesh covers fewer pixels: {} vs {}",
        covered(&after),
        covered(&before)
    );
    assert!(covered(&after) > 0, "the reloaded mesh is drawn");
    drop((model, server));
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// ADR-7330: the ECS path, counted textures, the second UV set and eviction.
// ---------------------------------------------------------------------------------------------

/// Texture-camera asset id of the App tests.
const VIEW: u64 = 0x7330;

/// The cooked cache of `project()` packed into an uncompressed pak under a fresh root, so the App
/// reads every asset from the pak (its own project root has no cooked cache).
fn pak_project() -> (std::path::PathBuf, std::path::PathBuf, AssetId, AssetId) {
    pak_project_with(compact_quad())
}

/// [`pak_project`] with `cooked` as the model's mesh.
fn pak_project_with(cooked: CookedMesh) -> (std::path::PathBuf, std::path::PathBuf, AssetId, AssetId) {
    let (cache_root, gltf, mesh, _) = project_with(cooked, |authored, image| {
        authored.base.base_color_map = Some(TextureSlot::Asset(image));
    });
    let cache = cooked_cache_dir(&cache_root, &regia_asset::current_platform());
    let root = regia_test_utils::unique_temp_dir("render-asset-counted-pak");
    let content = root.join("content");
    std::fs::create_dir_all(&content).expect("content dir");
    let manifest = regia_asset::AssetManifest {
        entries: vec![regia_asset::ManifestEntry {
            path: "meshes/quad.mesh".into(),
            id: mesh,
            type_path: String::new(),
        }],
    };
    regia_asset_pipeline::pak_writer::write_pak(
        &content.join("base.fpak"),
        &cache,
        &[(mesh, regia_core::id::TypePathHash(0))],
        &manifest,
        &regia_asset_pipeline::pak_writer::PakWriteOptions {
            compress: false,
            ..Default::default()
        },
    )
    .expect("write pak");
    (cache_root, root, gltf, mesh)
}

/// An App with a 64² texture camera on the lab view, the counted uploads wired, no visibility
/// buffer (its clusters need CPU vertices, so both paths draw forward).
fn counted_app(device: &RenderDevice, root: &Path) -> regia_app::App {
    use regia_app::prelude::*;
    let mut app = App::new();
    app.insert_resource(device.clone());
    app.add_plugins((
        regia_app::MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        regia_scene::ScenePlugin,
        regia_asset::AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.to_path_buf(),
                platform: regia_asset::current_platform(),
                content_dir: Some(root.join("content")),
            },
        },
        regia_render::CountedMeshPlugin,
    ));
    let mut settings =
        regia_render::RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopLow);
    settings.profile.visibility_buffer = false;
    app.insert_resource(settings);
    // The lab view of `lab()`: eye (0, 2.2, 1.2) looking at the origin.
    use regia_core::glam::{Affine3A, Vec3};
    let view =
        regia_core::glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 2.2, 1.2), Vec3::ZERO, Vec3::Y);
    let camera = regia_scene::Transform::from_affine(Affine3A::from_mat4(view.inverse()));
    app.world_mut().spawn((
        camera,
        regia_scene::GlobalTransform(camera.compute_affine()),
        regia_render::Camera {
            target: regia_render::RenderTarget::Texture { asset_id: VIEW },
            ..regia_render::Camera::default()
        },
        regia_render::TextureViewSettings {
            size: [64, 64],
            ..regia_render::TextureViewSettings::default()
        },
    ));
    app.world_mut().spawn(regia_render::DirectionalLight {
        direction: [0.2, -1.0, 0.1],
        color: [1.0; 3],
        illuminance: 20_000.0,
        cast_shadows: true,
        static_shadow: false,
    });
    app
}

fn spawn_mesh(app: &mut regia_app::App, renderer: regia_render::MeshRenderer) {
    let transform = regia_scene::Transform::default();
    app.world_mut().spawn((
        transform,
        regia_scene::GlobalTransform(transform.compute_affine()),
        regia_render::MeshRenderer {
            // The GI proxy brick needs CPU vertices (spec 12 § 3): both paths go without it.
            gi_proxy: false,
            ..renderer
        },
    ));
}

fn view_pixels(app: &mut regia_app::App, device: &RenderDevice) -> Vec<u8> {
    app.world_mut()
        .resource_mut::<regia_render::RuntimeRenderer>()
        .0
        .read_texture_target(device, VIEW)
        .expect("texture view rendered")
        .pixels
}

/// Allocations per batch of `frames` App updates after a warm-up, over `batches` batches.
fn steady_app_allocs(app: &mut regia_app::App, batches: usize, frames: usize) -> Vec<u64> {
    for _ in 0..8 {
        app.update();
    }
    (0..batches)
        .map(|_| {
            let before = allocs();
            for _ in 0..frames {
                app.update();
            }
            allocs() - before
        })
        .collect()
}

/// Item 1: a `MeshRenderer` on a counted pak mesh (`MeshRenderer::counted`) extracts with no CPU
/// vertices, draws its upload with the same pixels as the CPU path's `MeshRenderer`, the asset
/// server drops the CPU streams (`assets.handles` row: GPU bytes, no CPU bytes), a steady frame
/// allocates no more than the CPU path's, and despawning the entity unloads the mesh after the
/// grace frames.
#[test]
fn an_app_draws_a_counted_mesh_renderer_like_the_cpu_path_with_no_cpu_streams() {
    let _serial = measuring();
    let Ok(device) = own_device() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (cache_root, root, gltf, mesh_id) = pak_project();
    let material = std::sync::Arc::new(regia_render::LabMaterial::default());

    // The CPU path: the vertices `load_cooked_model` builds, in an `Arc<CpuMesh>`.
    let mut cpu_server = open(&cache_root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_mesh = std::sync::Arc::new(regia_render::CpuMesh {
        vertices: draws.meshes[0].vertices.clone(),
        indices: draws.meshes[0].indices.clone(),
    });
    let mut cpu_app = counted_app(&device, &root);
    cpu_app.update(); // as the counted App: one frame with no mesh first
    spawn_mesh(
        &mut cpu_app,
        regia_render::MeshRenderer::new(cpu_mesh, material.clone()),
    );

    // The counted path: the same mesh from the pak, as a counted handle.
    let mut app = counted_app(&device, &root);
    app.update(); // wires the uploaders
    app.world_mut()
        .resource_mut::<AssetServer>()
        .set_unload_grace_frames(GRACE);
    let handle = app
        .world_mut()
        .resource_mut::<AssetServer>()
        .load::<CpuMesh>(mesh_id)
        .expect("counted load");
    app.world_mut().resource_mut::<AssetServer>().wait_for_loads();
    spawn_mesh(
        &mut app,
        regia_render::MeshRenderer::counted(handle, material.clone()),
    );
    for _ in 0..4 {
        app.update();
        cpu_app.update();
    }

    let uploads = app
        .world()
        .resource::<regia_render::CountedAssetUploads>()
        .meshes
        .clone();
    assert_eq!(uploads.counters(), (1, 0), "one upload");
    {
        let extracted = app.world().resource::<regia_render::ExtractedFrame>();
        assert_eq!(extracted.meshes.len(), 1, "the counted mesh is extracted");
        assert!(
            extracted.meshes[0].mesh.vertices.is_empty() && extracted.meshes[0].mesh.indices.is_empty(),
            "no CPU vertices in the extracted frame"
        );
        assert_eq!(
            regia_render::counted_mesh_id(&extracted.meshes[0].mesh),
            Some(mesh_id)
        );
        assert_eq!(
            regia_render::mesh_key(&extracted.meshes[0].mesh),
            asset_mesh_key(mesh_id)
        );
    }
    assert_eq!(
        app.world()
            .resource::<regia_render::RuntimeRenderer>()
            .0
            .adopted_asset_mesh_count(),
        1
    );
    let server = app.world().resource::<AssetServer>();
    assert_eq!(server.cpu_data_kept(mesh_id), Some(false));
    let report = server.handle_report(Some("mesh"));
    let row = report
        .assets
        .iter()
        .find(|r| r.id == mesh_id)
        .expect("handle row");
    assert_eq!(
        (row.cpu_bytes, row.gpu_bytes),
        (0, uploads.resident_bytes()),
        "assets.handles: the CPU streams dropped, the upload counted once"
    );
    assert!(row.gpu_bytes > 0);
    assert!(row.strong >= 1, "the stand-in holds the asset");

    let want = view_pixels(&mut cpu_app, &device);
    let got = view_pixels(&mut app, &device);
    assert_eq!(got.len(), want.len());
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the counted MeshRenderer draws the CPU path's pixels"
    );
    let corner = &want[..4];
    let covered = want
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px.as_slice() != corner)
        .count();
    assert!(covered > 64 * 64 / 8, "the quad is drawn: {covered} pixels");

    // Steady frames: the counted path adds no allocation over the CPU path (typical frames). The App's
    // multi-threaded executor moves systems between threads and the counter is per thread, so
    // its numbers are only printed; the check renders each App's extracted frame (the ECS path:
    // `frame_scene` over the extracted meshes, the counted one with no vertices) on this thread.
    let cpu_app_counts = steady_app_allocs(&mut cpu_app, 4, 4);
    let app_counts = steady_app_allocs(&mut app, 4, 4);
    eprintln!(
        "steady App updates per 4 frames (informational): cpu {cpu_app_counts:?}, counted {app_counts:?}"
    );
    let cpu_frame = cpu_app.world().resource::<regia_render::ExtractedFrame>().clone();
    let counted_frame = app.world().resource::<regia_render::ExtractedFrame>().clone();
    let mut cpu_renderer = FrameRenderer::new();
    let mut counted_renderer = FrameRenderer::new();
    counted_renderer.set_asset_meshes(Some(uploads.clone()));
    let draw = |frame: &regia_render::ExtractedFrame, renderer: &mut FrameRenderer| {
        let view = frame.texture_views()[0].0;
        let scene = frame.frame_scene(view, 64, 64);
        renderer.render_offscreen(&device, &scene).expect("frame");
    };
    // Interleaved single frames, the order swapped every round: both paths are measured under the
    // same conditions (`paired_frame_allocs`).
    let (cpu_counts, counted_counts) = paired_frame_allocs(
        &device,
        || draw(&cpu_frame, &mut cpu_renderer),
        || draw(&counted_frame, &mut counted_renderer),
    );
    assert_eq!(counted_renderer.adopted_asset_mesh_count(), 1);
    assert_no_added_frame_allocs("a counted MeshRenderer", &cpu_counts, &counted_counts);
    drop((cpu_frame, counted_frame, cpu_renderer, counted_renderer));
    assert_eq!(uploads.counters(), (1, 0), "frames upload nothing");

    // Despawn: the stand-in's last clone goes; the next extraction drops its handle, and the mesh
    // unloads after the grace frames (GRACE frames, counted by updates).
    let entities: Vec<_> = app
        .world_mut()
        .query_filtered::<regia_app::prelude::Entity, regia_app::prelude::With<regia_render::MeshRenderer>>()
        .iter(app.world())
        .collect();
    for entity in entities {
        app.world_mut().despawn(entity);
    }
    // The first update's extraction drops the stand-in and its handle; the server's next GRACE
    // updates count the grace down.
    let mut frees_after = None;
    for frame in 1..=4 * GRACE {
        app.update();
        if uploads.counters().1 == 1 {
            frees_after = Some(frame);
            break;
        }
    }
    assert_eq!(
        frees_after,
        Some(GRACE + 1),
        "the counted mesh unloads after the grace once the entity is gone"
    );
    assert_eq!(uploads.resident_count(), 0);
    // In the same update, although the empty scene draws no frame: the buffers are not pinned.
    assert_eq!(
        app.world()
            .resource::<regia_render::RuntimeRenderer>()
            .0
            .adopted_asset_mesh_count(),
        0,
        "the renderer dropped its adopted entry with the unload"
    );
    drop((app, cpu_app, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// Item 2: two materials whose base color is the same counted image make one GPU upload (the
/// renderer's own texture cache gets no entry for it), sample it like the CPU path's RGBA8 slot,
/// and the texture is freed after the grace frames once both materials and the renderer let go.
#[test]
fn two_materials_sharing_a_counted_texture_make_one_upload_freed_after_the_grace() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, _mesh, image_id) = project_with_image();
    // The CPU control: the cooked model's own material (its base color an RGBA8 `LabTexture`).
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_material = draws.materials[0].clone();
    assert!(
        cpu_material.albedo.is_some(),
        "the control samples an RGBA8 base color"
    );

    let mut server = open(&root);
    let textures = regia_render::AssetTextureUploads::new(&device);
    server.register_prepare(textures.cooked_textures());
    server.register_prepare(textures.images());
    server.set_unload_grace_frames(GRACE);
    textures.set_srgb(image_id, true);
    let handle = server
        .load::<regia_asset::CookedImage>(image_id)
        .expect("counted image");
    server.wait_for_loads();
    server.update();
    textures.flush();
    assert_eq!(textures.counters(), (1, 0), "one upload");
    assert_eq!(server.cpu_data_kept(image_id), Some(false));
    let gpu = textures.resident_bytes();
    assert!(gpu > 0);
    let report = server.handle_report(None);
    let row = report
        .assets
        .iter()
        .find(|r| r.id == image_id)
        .expect("image row");
    assert_eq!(
        (row.cpu_bytes, row.gpu_bytes),
        (0, gpu),
        "counted once, on the GPU"
    );

    // Two materials, one counted image.
    let counted = |roughness: f32| {
        let mut m = cpu_material.clone();
        m.albedo = None;
        m.compressed = None;
        m.roughness = roughness;
        m.images.albedo = Some(regia_render::MaterialImage::Asset(
            regia_render::AssetImage::image(handle.clone(), &textures),
        ));
        m
    };
    let (a, b) = (
        counted(cpu_material.roughness),
        counted(cpu_material.roughness * 0.5 + 0.25),
    );
    let mut cpu_b = cpu_material.clone();
    cpu_b.roughness = b.roughness;
    let render = |materials: Vec<regia_render::LabMaterial>| {
        let base = lab(materials, Vec::new());
        let mut scene = base.as_frame_scene();
        let cpu = lab(draws.materials.clone(), draws.meshes.clone());
        let cpu_scene = cpu.as_frame_scene();
        let template = cpu_scene.meshes[0];
        for (i, x) in [-0.45f32, 0.45].into_iter().enumerate() {
            let mut model = template.model;
            model[3][0] = x;
            scene.meshes.push(FrameMesh {
                object_key: 0x7330_0000 + i as u64,
                material_key: 0x7330_1000 + i as u64,
                material: &base.materials[i],
                vertices: &draws.meshes[0].vertices,
                indices: &draws.meshes[0].indices,
                model,
                ..template
            });
        }
        let mut renderer = FrameRenderer::new();
        let pixels = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        let report = renderer.texture_memory();
        (pixels, report)
    };
    let (want, cpu_report) = render(vec![cpu_material.clone(), cpu_b.clone()]);
    let (got, report) = render(vec![a.clone(), b.clone()]);
    assert_eq!(
        textures.counters(),
        (1, 0),
        "drawing two materials uploads nothing more"
    );
    let slots = |r: &regia_render::TextureMemoryReport| {
        r.textures.iter().map(|t| (t.slot, t.width)).collect::<Vec<_>>()
    };
    assert!(
        report.textures.len() < cpu_report.textures.len(),
        "the renderer made no base-color upload of its own: counted {:?}, cpu {:?}",
        slots(&report),
        slots(&cpu_report)
    );
    let differing = got.iter().zip(&want).filter(|(x, y)| x != y).count();
    assert_eq!(
        differing, 0,
        "the counted texture samples like the CPU path's RGBA8 slot"
    );

    // Both materials and the handle go: freed on exactly the last grace frame.
    drop((a, b, handle));
    for frame in 1..GRACE {
        server.update();
        assert_eq!(
            textures.resident_count(),
            1,
            "still resident after {frame} of {GRACE}"
        );
    }
    server.update();
    assert_eq!(textures.counters(), (1, 1), "freed after the grace");
    assert_eq!(textures.resident_count(), 0);
    assert_eq!(server.gpu_memory_use::<regia_asset::CookedImage>(), 0);
    drop((server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 3: a counted mesh that is a lightmap receiver (UV2 for every uploaded vertex, a 1:1
/// unwrap) is drawn lightmapped from its upload and matches the CPU path's receiver.
#[test]
fn a_lightmapped_counted_mesh_matches_its_cpu_path_receiver() {
    let _serial = sharing();
    use regia_asset::lightmap::{
        Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
    };
    use regia_render::lightmap::LightmapSettings;
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, mesh_id) = project();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let uv2 = vec![[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let irr: Vec<[f32; 3]> = (0..2)
        .flat_map(|_| 0..8u32)
        .map(|x| match x {
            0..=3 => [0.0; 3],
            4 | 5 => [3_000.0; 3],
            _ => [30_000.0; 3],
        })
        .collect();
    let map = std::sync::Arc::new(
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 8,
                height: 2,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "receiver".into(),
                    source_vertices: 4,
                    rect: [4, 0, 4, 2],
                    scale_offset: [0.5, 1.0, 0.5, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 2.56,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3],
                    uv: uv2.clone(),
                    indices: draws.meshes[0].indices.clone(),
                }],
            },
            &irr,
            vec![Coverage::BAKED; 16],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid lightmap"),
    );
    let settings = LightmapSettings {
        enabled: true,
        ..LightmapSettings::default()
    };
    const RECEIVER: u64 = 0x7330_0A11;

    // CPU control: the receiver mesh built from CPU vertices, UV2 as its second set.
    let built =
        regia_render::lightmap::receiver_mesh(&map, 0, &draws.meshes[0].vertices, &draws.meshes[0].indices)
            .expect("receiver");
    let cpu_lab = lab(draws.materials.clone(), draws.meshes.clone());
    let cpu_scene = cpu_lab.as_frame_scene();
    let template = FrameMesh {
        object_key: RECEIVER,
        ..cpu_scene.meshes[0]
    };
    let mut cpu_frame = cpu_lab.as_frame_scene();
    cpu_frame.meshes = vec![FrameMesh {
        vertices: &built.vertices,
        indices: &built.indices,
        uv1: &built.uv2,
        ..template
    }];
    let mut cpu_renderer = FrameRenderer::new();
    cpu_renderer.set_lightmap(Some(map.clone()), settings);
    cpu_renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
    let want = cpu_renderer
        .render_offscreen(&device, &cpu_frame)
        .expect("cpu")
        .pixels;
    let cpu_stats = cpu_renderer.lightmap_forward_stats();
    assert!(
        cpu_stats.active && cpu_stats.drawn == 1,
        "the CPU receiver is lightmapped: {cpu_stats:?}"
    );

    // Counted: the upload with the same UV2 as the draw's second set.
    let mut server = open(&root);
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    let handle = server.load::<CpuMesh>(mesh_id).expect("counted");
    server.wait_for_loads();
    server.update();
    let mut frame = cpu_lab.as_frame_scene();
    frame.meshes = vec![FrameMesh {
        mesh_key: asset_mesh_key(mesh_id),
        vertices: &[],
        indices: &[],
        uv1: &uv2,
        ..template
    }];
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    renderer.set_lightmap(Some(map.clone()), settings);
    renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
    let got = renderer
        .render_offscreen(&device, &frame)
        .expect("counted")
        .pixels;
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(
        (stats.drawn, stats.unsupported),
        (1, 0),
        "the counted receiver is lightmapped: {stats:?}"
    );
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the lightmapped counted mesh matches the CPU receiver"
    );
    drop((handle, server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 4: under memory pressure the mesh cache evicts only its own (CPU-path) entries; an
/// adopted counted mesh stays, and its bytes are reported apart from the cache's.
#[test]
fn pressure_eviction_frees_only_cpu_path_meshes_and_reports_adopted_bytes_apart() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, mesh_id) = project();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let mut server = open(&root);
    let uploads = AssetMeshUploads::new(&device);
    server.register_prepare(uploads.clone());
    let handle = server.load::<CpuMesh>(mesh_id).expect("counted");
    server.wait_for_loads();
    server.update();

    let lab_scene = lab(draws.materials.clone(), draws.meshes.clone());
    let both = lab_scene.as_frame_scene();
    let cpu_draw = both.meshes[0];
    let mut scene = lab_scene.as_frame_scene();
    scene.meshes.push(FrameMesh {
        mesh_key: asset_mesh_key(mesh_id),
        object_key: 0x7330_0E01,
        vertices: &[],
        indices: &[],
        ..cpu_draw
    });
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    renderer.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(renderer.adopted_asset_mesh_count(), 1);
    let meshes_before = renderer.cache_stats().meshes;

    // Nothing drawn for a while, then memory pressure: every idle CPU-path mesh goes.
    let empty = lab(draws.materials.clone(), Vec::new());
    let empty_scene = empty.as_frame_scene();
    // Longer than the pressure sweep's idle floor (30 frames).
    for _ in 0..32 {
        renderer
            .render_offscreen(&device, &empty_scene)
            .expect("idle frame");
    }
    renderer.trim_memory();
    renderer
        .render_offscreen(&device, &empty_scene)
        .expect("pressure frame");
    let stats = renderer.cache_stats();
    let budgets = renderer.cache_budgets();
    assert!(
        stats.meshes_evicted >= 1,
        "the CPU-path mesh was evicted: {stats:?}"
    );
    assert_eq!(
        renderer.adopted_asset_mesh_count(),
        1,
        "the adopted counted mesh is never evicted"
    );
    assert_eq!(
        stats.meshes,
        meshes_before - stats.meshes_evicted as usize,
        "{stats:?}"
    );
    assert_eq!(budgets.adopted_meshes, 1);
    assert_eq!(
        budgets.adopted_mesh_bytes,
        uploads.resident_bytes(),
        "adopted bytes reported apart"
    );
    assert_eq!(budgets.mesh_bytes, 0, "no CPU-path mesh bytes left: {budgets:?}");
    assert_eq!(
        uploads.counters(),
        (1, 0),
        "eviction freed nothing the asset server owns"
    );
    drop((handle, server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 2 on the cooked-model path: `load_cooked_models_counted_textured` loads the base maps as
/// counted textures (one upload, no CPU copy) bound through `MaterialImage::Asset`, and the model
/// draws the CPU path's pixels.
#[test]
fn a_counted_textured_cooked_model_binds_counted_maps_and_draws_like_the_cpu_path() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, _mesh, image_id) = project_with_image();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_lab = lab(draws.materials.clone(), draws.meshes.clone());
    let cpu_scene = cpu_lab.as_frame_scene();
    let want = FrameRenderer::new()
        .render_offscreen(&device, &cpu_scene)
        .expect("cpu frame")
        .pixels;

    let mut server = open(&root);
    let meshes = AssetMeshUploads::new(&device);
    let textures = regia_render::AssetTextureUploads::new(&device);
    server.register_prepare(meshes.clone());
    server.register_prepare(textures.cooked_textures());
    server.register_prepare(textures.images());
    let model =
        regia_render::cooked_model::load_cooked_models_counted_textured(&mut server, &[gltf], &textures)
            .expect("counted textured")
            .remove(0);
    server.wait_for_loads();
    server.update();
    assert_eq!(textures.counters(), (1, 0), "the base color uploads once");
    assert_eq!(
        server.cpu_data_kept(image_id),
        Some(false),
        "its CPU copy dropped"
    );
    let material = &model.materials[0];
    assert!(material.albedo.is_none(), "no CPU base color");
    assert!(
        matches!(&material.images.albedo, Some(regia_render::MaterialImage::Asset(a)) if a.id() == image_id),
        "the base color binds the counted image"
    );

    let base = lab(model.materials.clone(), Vec::new());
    let mut scene = base.as_frame_scene();
    model.push_frame_meshes(cpu_scene.meshes[0].model, 0x7330_7E01, &mut scene.meshes);
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(meshes.clone()));
    let got = renderer
        .render_offscreen(&device, &scene)
        .expect("counted frame")
        .pixels;
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "counted mesh and counted base color draw the CPU path's pixels"
    );
    drop((renderer, model, server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// A one-entry lightmap (`receiver`) over the right half of an 8 × 2 atlas, dim on the entry's
/// left, bright on its right, with the given unwrap.
fn quad_lightmap(unwrap: regia_asset::lightmap::MeshUnwrap) -> regia_asset::lightmap::Lightmap {
    use regia_asset::lightmap::{
        Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout,
    };
    let irr: Vec<[f32; 3]> = (0..2)
        .flat_map(|_| 0..8u32)
        .map(|x| match x {
            0..=3 => [0.0; 3],
            4 | 5 => [3_000.0; 3],
            _ => [30_000.0; 3],
        })
        .collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "receiver".into(),
                source_vertices: 4,
                rect: [4, 0, 4, 2],
                scale_offset: [0.5, 1.0, 0.5, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 2.56,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![unwrap],
        },
        &irr,
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

/// Item 3 in an App: a `LightmapReceiver` on a counted `MeshRenderer` whose entry's unwrap is 1:1
/// with the upload takes the entry's UV2 (no CPU vertices) and draws the CPU path's receiver
/// pixels; an unwrap that splits vertices is refused with `lightmap_receiver_counted`.
#[test]
fn an_app_lightmap_receiver_on_a_counted_mesh_draws_like_the_cpu_receiver() {
    let _serial = sharing();
    use regia_asset::lightmap::MeshUnwrap;
    use regia_render::lightmap::{
        InstalledLightmap, LightmapReceiver, LightmapReceiverReport, LightmapSettings,
    };
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (cache_root, root, gltf, mesh_id) = pak_project();
    let mut cpu_server = open(&cache_root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let one_to_one = || MeshUnwrap {
        xref: vec![0, 1, 2, 3],
        uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        indices: draws.meshes[0].indices.clone(),
    };
    let material = std::sync::Arc::new(regia_render::LabMaterial::default());
    let setup = |app: &mut regia_app::App, unwrap: MeshUnwrap| {
        app.insert_resource(LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        });
        let mut installed = InstalledLightmap::default();
        installed.install("test", quad_lightmap(unwrap));
        app.insert_resource(installed);
    };
    let spawn_receiver = |app: &mut regia_app::App, renderer: regia_render::MeshRenderer| {
        let transform = regia_scene::Transform::default();
        app.world_mut().spawn((
            transform,
            regia_scene::GlobalTransform(transform.compute_affine()),
            regia_render::MeshRenderer {
                gi_proxy: false,
                ..renderer
            },
            LightmapReceiver {
                entry: "receiver".into(),
            },
        ));
    };

    let mut cpu_app = counted_app(&device, &root);
    setup(&mut cpu_app, one_to_one());
    cpu_app.update();
    let cpu_mesh = std::sync::Arc::new(regia_render::CpuMesh {
        vertices: draws.meshes[0].vertices.clone(),
        indices: draws.meshes[0].indices.clone(),
    });
    spawn_receiver(
        &mut cpu_app,
        regia_render::MeshRenderer::new(cpu_mesh, material.clone()),
    );

    let mut app = counted_app(&device, &root);
    setup(&mut app, one_to_one());
    app.update();
    let handle = app
        .world_mut()
        .resource_mut::<AssetServer>()
        .load::<CpuMesh>(mesh_id)
        .expect("counted load");
    app.world_mut().resource_mut::<AssetServer>().wait_for_loads();
    spawn_receiver(
        &mut app,
        regia_render::MeshRenderer::counted(handle, material.clone()),
    );
    for _ in 0..4 {
        app.update();
        cpu_app.update();
    }
    for (name, a) in [("cpu", &cpu_app), ("counted", &app)] {
        let report = a.world().resource::<LightmapReceiverReport>();
        assert_eq!(
            (report.receivers, report.applied, report.errors.len()),
            (1, 1, 0),
            "{name}: the receiver is applied: {:?}",
            report.errors
        );
    }
    {
        let extracted = app.world().resource::<regia_render::ExtractedFrame>();
        assert!(
            extracted.meshes[0].mesh.vertices.is_empty(),
            "still no CPU vertices"
        );
        assert_eq!(
            extracted.meshes[0].uv1.as_ref().map(|uv| uv.len()),
            Some(4),
            "the entry's UV2"
        );
    }
    let stats = app
        .world()
        .resource::<regia_render::RuntimeRenderer>()
        .0
        .lightmap_forward_stats();
    assert!(stats.receivers == 1 && stats.unsupported == 0, "{stats:?}");
    let want = view_pixels(&mut cpu_app, &device);
    let got = view_pixels(&mut app, &device);
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the counted receiver draws the CPU receiver's pixels"
    );

    // An unwrap that splits a vertex has no 1:1 UV2 for the upload: refused, with its code.
    let mut split = one_to_one();
    split.xref.push(0);
    split.uv.push([0.0, 0.0]);
    if let Some(last) = split.indices.last_mut() {
        *last = 4;
    }
    let mut installed = InstalledLightmap::default();
    installed.install("split", quad_lightmap(split));
    app.insert_resource(installed);
    app.update();
    let report = app.world().resource::<LightmapReceiverReport>();
    assert_eq!(report.applied, 0);
    assert_eq!(
        report.errors.first().map(|e| e.code),
        Some("lightmap_receiver_counted"),
        "{:?}",
        report.errors
    );
    drop((app, cpu_app, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache_root);
}

// ---------------------------------------------------------------------------------------------
// ADR-8610 (w6): counted textures in both color spaces, hot reload in an App, graph slots and the
// foliage-sway twin, skinned counted meshes.

/// A server reading `root`'s dev cache with counted texture uploads registered.
fn counted_texture_server(
    device: &RenderDevice,
    root: &Path,
) -> (AssetServer, regia_render::AssetTextureUploads) {
    let mut server = open(root);
    let textures = regia_render::AssetTextureUploads::new(device);
    server.register_prepare(textures.cooked_textures());
    server.register_prepare(textures.images());
    server.set_unload_grace_frames(GRACE);
    (server, textures)
}

/// The `assets.handles` row of `id`: `(cpu_bytes, gpu_bytes)`.
fn handle_bytes(server: &AssetServer, id: AssetId) -> (u64, u64) {
    let report = server.handle_report(None);
    let row = report.assets.iter().find(|r| r.id == id).expect("handle row");
    (row.cpu_bytes, row.gpu_bytes)
}

/// GPU bytes of one 4 × 4 RGBA8 texture with its full mip chain (4², 2², 1² texels).
const ONE_4X4_CHAIN: u64 = (16 + 4 + 1) * 4;

/// w6 item 1: one counted image in a color slot (base color, sRGB) and a linear slot (packed
/// ORM) samples each slot in its own color space, like the CPU path's two uploads of the same
/// bytes, from one GPU texture (two views), whichever space it was uploaded for.
#[test]
fn a_counted_texture_in_a_color_and_a_linear_slot_samples_each_like_the_cpu_paths_two_uploads() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, _mesh, image_id) = project_with_image();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    // The CPU control: the base color image also as the packed ORM map (two uploads of the same
    // bytes, sRGB and linear).
    let mut cpu_material = draws.materials[0].clone();
    let albedo = cpu_material.albedo.clone().expect("an RGBA8 base color");
    cpu_material.occlusion_map = Some(albedo);
    cpu_material.orm_packed = true;
    cpu_material.occlusion_strength = 1.0;
    let cpu_lab = lab(vec![cpu_material.clone()], draws.meshes.clone());
    let mut cpu_renderer = FrameRenderer::new();
    let want = cpu_renderer
        .render_offscreen(&device, &cpu_lab.as_frame_scene())
        .expect("cpu frame")
        .pixels;
    let cpu_slots: Vec<&str> = cpu_renderer
        .texture_memory()
        .textures
        .iter()
        .map(|t| t.slot)
        .collect();
    assert!(
        cpu_slots.contains(&"base_color") && cpu_slots.contains(&"orm"),
        "the control uploads both slots: {cpu_slots:?}"
    );

    for srgb_upload in [true, false] {
        let (mut server, textures) = counted_texture_server(&device, &root);
        textures.set_srgb(image_id, srgb_upload);
        let handle = server.load::<CookedImage>(image_id).expect("counted image");
        server.wait_for_loads();
        server.update();
        textures.flush();
        assert_eq!(textures.counters(), (1, 0), "one upload (sRGB {srgb_upload})");
        assert_eq!(
            handle_bytes(&server, image_id),
            (0, ONE_4X4_CHAIN),
            "assets.handles: one GPU texture, no CPU copy (sRGB {srgb_upload})"
        );
        let image = regia_render::AssetImage::image(handle.clone(), &textures);
        let mut material = cpu_material.clone();
        material.albedo = None;
        // A 1 × 1 placeholder keeps the packed-ORM flag; the counted image wins the slot.
        material.occlusion_map = Some(regia_render::LabTexture {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        });
        material.images.albedo = Some(regia_render::MaterialImage::Asset(image.clone()));
        material.images.occlusion = Some(regia_render::MaterialImage::Asset(image));
        let counted_lab = lab(vec![material], draws.meshes.clone());
        let mut renderer = FrameRenderer::new();
        let got = renderer
            .render_offscreen(&device, &counted_lab.as_frame_scene())
            .expect("counted frame")
            .pixels;
        let slots: Vec<&str> = renderer
            .texture_memory()
            .textures
            .iter()
            .map(|t| t.slot)
            .collect();
        assert!(
            !slots.contains(&"base_color") && !slots.contains(&"orm"),
            "the renderer made no upload of its own for the counted slots: {slots:?}"
        );
        assert_eq!(
            textures.counters(),
            (1, 0),
            "drawing both slots uploads nothing more"
        );
        assert_eq!(textures.resident_count(), 1, "one GPU texture");
        let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(
            differing, 0,
            "uploaded for sRGB {srgb_upload}: each slot samples like the CPU path's own upload"
        );
        drop((renderer, counted_lab, handle, server));
    }
    drop(cpu_server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A 4 × 4 RGBA8 image of one color.
fn solid_image(rgba: [u8; 4]) -> CookedImage {
    CookedImage {
        width: 4,
        height: 4,
        rgba8: rgba.repeat(16),
    }
}

/// w6 item 2: in an App, a counted texture replaced on disk and reloaded shows in the very next
/// rendered frame, without touching the material (its `Arc` and its key stay), pixel for pixel
/// like a CPU-path App whose material switches to the new image at the same frame (so both carry
/// the same frame history); a steady frame of the counted material allocates no more than the
/// CPU path's.
#[test]
fn an_app_material_draws_a_reloaded_counted_texture_on_the_next_frame() {
    let _serial = measuring();
    let Ok(device) = own_device() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, _mesh, image_id) = project_with_image();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let mesh = std::sync::Arc::new(regia_render::CpuMesh {
        vertices: draws.meshes[0].vertices.clone(),
        indices: draws.meshes[0].indices.clone(),
    });
    let green = [30u8, 200, 70, 255];

    // The control: the CPU-path material (the checker as an RGBA8 map) until the reload frame,
    // then a material holding the new (green) image.
    let mut cpu_green = draws.materials[0].clone();
    cpu_green.albedo = Some(regia_render::LabTexture {
        width: 4,
        height: 4,
        rgba: green.repeat(16),
    });
    let mut cpu_app = counted_app(&device, &root);
    cpu_app.update();
    spawn_mesh(
        &mut cpu_app,
        regia_render::MeshRenderer::new(mesh.clone(), std::sync::Arc::new(draws.materials[0].clone())),
    );

    // The counted App: the checker image as a counted texture in the material's base color.
    let mut app = counted_app(&device, &root);
    app.update(); // wires the uploaders
    let textures = app
        .world()
        .resource::<regia_render::CountedAssetUploads>()
        .textures
        .clone();
    textures.set_srgb(image_id, true);
    let handle = {
        let mut server = app.world_mut().resource_mut::<AssetServer>();
        let handle = server.load::<CookedImage>(image_id).expect("counted image");
        server.wait_for_loads();
        handle
    };
    let mut material = draws.materials[0].clone();
    material.albedo = None;
    material.images.albedo = Some(regia_render::MaterialImage::Asset(
        regia_render::AssetImage::image(handle, &textures),
    ));
    let material = std::sync::Arc::new(material);
    let key = regia_render::shading_model::material_key(&material);
    spawn_mesh(
        &mut app,
        regia_render::MeshRenderer::new(mesh.clone(), material.clone()),
    );
    for _ in 0..4 {
        app.update();
        cpu_app.update();
    }
    assert_eq!(textures.counters(), (1, 0), "one upload");
    let before = view_pixels(&mut app, &device);
    let cpu_before = view_pixels(&mut cpu_app, &device);
    assert_eq!(
        before.iter().zip(&cpu_before).filter(|(a, b)| a != b).count(),
        0,
        "before the reload the counted texture draws like the CPU path's checker"
    );

    // Replace the image on disk and reload it (what the App's hot reload does on a change).
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    put(
        &cache,
        image_id,
        AssetTypeTag::image(),
        &solid_image(green).to_postcard().expect("image"),
    );
    app.world_mut()
        .resource_mut::<AssetServer>()
        .reload_id(image_id)
        .expect("reload");
    assert_eq!(textures.counters(), (2, 0), "the reload uploads again");
    // The control switches to the green image for the same frame.
    let cpu_entities: Vec<_> = cpu_app
        .world_mut()
        .query_filtered::<regia_app::prelude::Entity, regia_app::prelude::With<regia_render::MeshRenderer>>()
        .iter(cpu_app.world())
        .collect();
    for entity in cpu_entities {
        if let Some(mut renderer) = cpu_app.world_mut().get_mut::<regia_render::MeshRenderer>(entity) {
            renderer.material = std::sync::Arc::new(cpu_green.clone());
        }
    }
    // The next rendered frame.
    app.update();
    cpu_app.update();
    let after = view_pixels(&mut app, &device);
    let want = view_pixels(&mut cpu_app, &device);
    assert_eq!(
        regia_render::shading_model::material_key(&material),
        key,
        "the material was not touched"
    );
    assert_ne!(after, before, "the frame changed with the texture");
    let differing = after.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the frame after the reload draws the new image, like the CPU path's switched material"
    );
    assert_eq!(
        textures.resident_count(),
        1,
        "the old texture went with the reload"
    );

    // Steady frames: the counted material adds no per-frame allocation (typical frames of each
    // App's extracted frame, interleaved and rendered on this thread; see the ECS mesh test).
    let cpu_frame = cpu_app.world().resource::<regia_render::ExtractedFrame>().clone();
    let counted_frame = app.world().resource::<regia_render::ExtractedFrame>().clone();
    let mut cpu_renderer = FrameRenderer::new();
    let mut counted_renderer = FrameRenderer::new();
    let draw = |frame: &regia_render::ExtractedFrame, renderer: &mut FrameRenderer| {
        let view = frame.texture_views()[0].0;
        let scene = frame.frame_scene(view, 64, 64);
        renderer.render_offscreen(&device, &scene).expect("frame");
    };
    let (cpu_counts, counted_counts) = paired_frame_allocs(
        &device,
        || draw(&cpu_frame, &mut cpu_renderer),
        || draw(&counted_frame, &mut counted_renderer),
    );
    assert_no_added_frame_allocs("a counted texture", &cpu_counts, &counted_counts);
    assert_eq!(textures.counters(), (2, 0), "frames upload nothing");
    drop((cpu_frame, counted_frame, cpu_renderer, counted_renderer));
    drop((app, cpu_app, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// w6 item 3: two material-graph materials (a foliage-sway twin and the still graph material it
/// leaves) holding one counted image in graph texture slot 0 make one upload, draw the CPU
/// path's pixels, and the texture is freed after the grace frames once both let go.
#[test]
fn two_graph_materials_one_a_sway_twin_sharing_a_counted_texture_make_one_upload_freed_after_the_grace() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, _mesh, image_id) = project_with_image();
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_material = draws.materials[0].clone();
    let sway = regia_render::FoliageSway::default();
    let cpu_twin = sway.adopt(&cpu_material).expect("sway twin");
    let cpu_still = regia_render::FoliageSway::still(&cpu_twin);
    assert!(
        cpu_still.surface_graph.is_some(),
        "the still material is a graph material"
    );

    let (mut server, textures) = counted_texture_server(&device, &root);
    textures.set_srgb(image_id, true);
    let handle = server.load::<CookedImage>(image_id).expect("counted image");
    server.wait_for_loads();
    server.update();
    textures.flush();
    assert_eq!(textures.counters(), (1, 0));
    assert_eq!(handle_bytes(&server, image_id), (0, ONE_4X4_CHAIN));
    let mut plain = cpu_material.clone();
    plain.albedo = None;
    plain.compressed = None;
    plain.images.albedo = Some(regia_render::MaterialImage::Asset(
        regia_render::AssetImage::image(handle.clone(), &textures),
    ));
    let twin = sway.adopt(&plain).expect("sway twin");
    let still = regia_render::FoliageSway::still(&twin);

    let render = |materials: Vec<regia_render::LabMaterial>| {
        let base = lab(materials, Vec::new());
        let mut scene = base.as_frame_scene();
        let cpu = lab(draws.materials.clone(), draws.meshes.clone());
        let cpu_scene = cpu.as_frame_scene();
        let template = cpu_scene.meshes[0];
        for (i, x) in [-0.45f32, 0.45].into_iter().enumerate() {
            let mut model = template.model;
            model[3][0] = x;
            scene.meshes.push(FrameMesh {
                object_key: 0x8610_0000 + i as u64,
                material_key: 0x8610_1000 + i as u64,
                material: &base.materials[i],
                vertices: &draws.meshes[0].vertices,
                indices: &draws.meshes[0].indices,
                model,
                ..template
            });
        }
        let mut renderer = FrameRenderer::new();
        let pixels = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        (pixels, renderer.texture_memory())
    };
    let (want, cpu_report) = render(vec![cpu_still.clone(), cpu_twin.clone()]);
    let (got, report) = render(vec![still.clone(), twin.clone()]);
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "both graph materials sample the counted texture like the CPU path's graph slots"
    );
    assert_eq!(textures.counters(), (1, 0), "drawing both uploads nothing more");
    assert!(
        report.textures.len() < cpu_report.textures.len(),
        "the renderer made no graph-slot upload of its own: {} vs {}",
        report.textures.len(),
        cpu_report.textures.len()
    );
    // The sway twin gives the counted image back to the plain material.
    let back = regia_render::FoliageSway::plain(&twin).expect("a sway twin");
    assert!(
        matches!(&back.images.albedo, Some(regia_render::MaterialImage::Asset(a)) if a.id() == image_id),
        "the plain material binds the counted image again"
    );

    drop((plain, twin, still, back, handle));
    for frame in 1..GRACE {
        server.update();
        assert_eq!(
            textures.resident_count(),
            1,
            "still resident after {frame} of {GRACE}"
        );
    }
    server.update();
    assert_eq!(textures.counters(), (1, 1), "freed after the grace");
    assert_eq!(server.gpu_memory_use::<CookedImage>(), 0);
    drop((server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// [`compact_quad`] with a two-joint skin: the far edge (z = -0.8) follows joint 0, the near
/// corner (+0.8, +0.8) joint 1, and the other near corner blends both half and half.
fn skinned_quad() -> CookedMesh {
    let mut mesh = compact_quad();
    let joints: [[u8; 4]; 4] = [[0, 0, 0, 0], [0, 0, 0, 0], [1, 0, 0, 0], [0, 1, 0, 0]];
    let weights: [[u16; 4]; 4] = [
        [65535, 0, 0, 0],
        [65535, 0, 0, 0],
        [65535, 0, 0, 0],
        [32768, 32767, 0, 0],
    ];
    mesh.skin = Some(regia_asset::CookedSkin {
        joint_names: vec!["root".into(), "tip".into()],
        joint_width: regia_asset::CookedJointWidth::U8,
        stream_joints0: joints.iter().flatten().copied().collect(),
        stream_weights0: weights.iter().flatten().flat_map(|w| w.to_le_bytes()).collect(),
        stream_joints1: Vec::new(),
        stream_weights1: Vec::new(),
    });
    mesh
}

/// w6 item 4: a skinned `MeshRenderer` on a counted mesh draws from the adopted upload (the
/// skinning reads the GPU vertex buffer, the joints stay CPU), posed at two frames, with the CPU
/// path's skinned twin's pixels, and the asset server keeps no CPU copy.
#[test]
fn an_app_draws_a_skinned_counted_mesh_like_its_cpu_twin_at_two_poses() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, _gltf, mesh_id) = project();
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    put(
        &cache,
        mesh_id,
        AssetTypeTag::mesh(),
        &skinned_quad().to_postcard().expect("mesh"),
    );
    let material = std::sync::Arc::new(regia_render::LabMaterial::default());
    // The CPU twin: the skinned vertices `CpuMesh::from_cooked` builds.
    let mut cpu_server = open(&root);
    let cpu_handle = cpu_server.load::<CpuMesh>(mesh_id).expect("cpu mesh");
    cpu_server.wait_for_loads();
    let cpu_mesh = {
        let asset = cpu_server.get(&cpu_handle).expect("decoded");
        assert!(asset.skin.is_some(), "the fixture is skinned");
        std::sync::Arc::new(regia_render::CpuMesh::from_cooked(asset).expect("skinned twin"))
    };
    let spawn_skinned = |app: &mut regia_app::App, renderer: regia_render::MeshRenderer| {
        let transform = regia_scene::Transform::default();
        app.world_mut()
            .spawn((
                transform,
                regia_scene::GlobalTransform(transform.compute_affine()),
                regia_render::MeshRenderer {
                    gi_proxy: false,
                    ..renderer
                },
                regia_render::SkinJoints {
                    palette: vec![regia_render::JointMatrix::identity(); 2],
                },
            ))
            .id()
    };
    let mut cpu_app = counted_app(&device, &root);
    cpu_app.update();
    let cpu_entity = spawn_skinned(
        &mut cpu_app,
        regia_render::MeshRenderer::new(cpu_mesh, material.clone()),
    );

    let mut app = counted_app(&device, &root);
    app.update(); // wires the uploaders
    let handle = {
        let mut server = app.world_mut().resource_mut::<AssetServer>();
        let handle = server.load::<CpuMesh>(mesh_id).expect("counted load");
        server.wait_for_loads();
        handle
    };
    let entity = spawn_skinned(&mut app, regia_render::MeshRenderer::counted(handle, material));
    let uploads = app
        .world()
        .resource::<regia_render::CountedAssetUploads>()
        .meshes
        .clone();

    let poses = [
        // The tip lifts 0.3 m.
        vec![
            regia_render::JointMatrix::identity(),
            regia_render::JointMatrix::translation(0.0, 0.3, 0.0),
        ],
        // The tip swings and the root slides.
        vec![
            regia_render::JointMatrix::translation(0.1, 0.0, 0.0),
            regia_render::JointMatrix::from_mat4(
                regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(-0.2, 0.1, 0.0))
                    * regia_core::glam::Mat4::from_rotation_y(0.4),
            ),
        ],
    ];
    let mut frames = Vec::new();
    for palette in &poses {
        for (a, e) in [(&mut app, entity), (&mut cpu_app, cpu_entity)] {
            a.world_mut().entity_mut(e).insert(regia_render::SkinJoints {
                palette: palette.clone(),
            });
        }
        for _ in 0..3 {
            app.update();
            cpu_app.update();
        }
        let got = view_pixels(&mut app, &device);
        let want = view_pixels(&mut cpu_app, &device);
        let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(
            differing,
            0,
            "the skinned counted mesh draws its CPU twin's pixels at pose {}",
            frames.len()
        );
        let corner = &want[..4];
        let covered = want
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px.as_slice() != corner)
            .count();
        assert!(covered > 64 * 64 / 8, "the quad is drawn: {covered} pixels");
        frames.push(got);
    }
    assert_ne!(frames[0], frames[1], "the two poses differ");

    assert_eq!(uploads.counters(), (1, 0), "one upload");
    {
        let extracted = app.world().resource::<regia_render::ExtractedFrame>();
        assert_eq!(extracted.meshes.len(), 1);
        assert!(
            extracted.meshes[0].mesh.vertices.is_empty(),
            "no CPU vertices in the extracted frame"
        );
        assert_eq!(extracted.meshes[0].joints.len(), 2, "the joints stay CPU");
    }
    let server = app.world().resource::<AssetServer>();
    assert_eq!(server.cpu_data_kept(mesh_id), Some(false));
    assert_eq!(
        handle_bytes(server, mesh_id),
        (0, uploads.resident_bytes()),
        "assets.handles: no CPU copy, the skinned upload counted once"
    );
    drop((app, cpu_app, cpu_handle, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// w7-counted-meshes (ADR-7428)
// ---------------------------------------------------------------------------------------------

/// w7 item 1: one counted image named by a material's base color and by its detail albedo. The
/// base color binds the counted upload; the detail slot is decoded as on the CPU path (it was the
/// 1×1 white stand-in), so the model draws its CPU twin's pixels.
#[test]
fn a_counted_base_map_that_is_also_the_detail_albedo_draws_like_the_cpu_path() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, gltf, _mesh, image_id) = project_with_material(|authored, image| {
        authored.base.base_color_map = Some(TextureSlot::Asset(image));
        authored.hard_surface = Some(regia_asset::HardSurfaceParams {
            detail: Some(regia_asset::DetailMapParams {
                albedo: Some(TextureSlot::Asset(image)),
                uv_scale: [3.0, 3.0],
                ..regia_asset::DetailMapParams::default()
            }),
            ..regia_asset::HardSurfaceParams::default()
        });
    });
    let mut cpu_server = open(&root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_detail = draws.materials[0]
        .detail_albedo
        .as_ref()
        .map(|t| (t.width, t.height));
    assert_eq!(cpu_detail, Some((4, 4)), "the CPU path decodes the detail map");
    let cpu_lab = lab(draws.materials.clone(), draws.meshes.clone());
    let cpu_scene = cpu_lab.as_frame_scene();
    let want = FrameRenderer::new()
        .render_offscreen(&device, &cpu_scene)
        .expect("cpu frame")
        .pixels;

    let mut server = open(&root);
    let meshes = AssetMeshUploads::new(&device);
    let textures = regia_render::AssetTextureUploads::new(&device);
    server.register_prepare(meshes.clone());
    server.register_prepare(textures.cooked_textures());
    server.register_prepare(textures.images());
    let model =
        regia_render::cooked_model::load_cooked_models_counted_textured(&mut server, &[gltf], &textures)
            .expect("counted textured")
            .remove(0);
    server.wait_for_loads();
    server.update();
    assert_eq!(textures.counters(), (1, 0), "the base color uploads once");
    let material = &model.materials[0];
    assert!(
        matches!(&material.images.albedo, Some(regia_render::MaterialImage::Asset(a)) if a.id() == image_id),
        "the base color binds the counted image"
    );
    assert_eq!(
        material.detail_albedo.as_ref().map(|t| (t.width, t.height)),
        cpu_detail,
        "the detail albedo is the decoded image, not a stand-in"
    );

    let base = lab(model.materials.clone(), Vec::new());
    let mut scene = base.as_frame_scene();
    model.push_frame_meshes(cpu_scene.meshes[0].model, 0x7330_7E02, &mut scene.meshes);
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(meshes.clone()));
    let got = renderer
        .render_offscreen(&device, &scene)
        .expect("counted frame")
        .pixels;
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "a counted base map that is also the detail albedo draws the CPU path's pixels"
    );
    drop((renderer, model, server, cpu_server));
    let _ = std::fs::remove_dir_all(&root);
}

/// The `MeshRenderer` entities of `app`.
fn mesh_entities(app: &mut regia_app::App) -> Vec<regia_app::prelude::Entity> {
    app.world_mut()
        .query_filtered::<regia_app::prelude::Entity, regia_app::prelude::With<regia_render::MeshRenderer>>()
        .iter(app.world())
        .collect()
}

/// w7 item 2: a counted mesh's entity despawned and a new one spawned on the same asset between
/// two frames, after its stand-in died (the entity held the stand-in's last clone: the extracted
/// frame of the last update is cleared first, as a host that drops it after drawing would). The
/// new stand-in reuses the key, so the renderer keeps its adopted entry: nothing is released, and
/// both frames draw the CPU control's pixels (the control App despawns and respawns its CPU mesh
/// at the same frame).
#[test]
fn a_counted_mesh_respawned_between_two_frames_keeps_its_adopted_entry() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (cache_root, root, gltf, mesh_id) = pak_project();
    let material = std::sync::Arc::new(regia_render::LabMaterial::default());
    let mut cpu_server = open(&cache_root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_mesh = std::sync::Arc::new(regia_render::CpuMesh {
        vertices: draws.meshes[0].vertices.clone(),
        indices: draws.meshes[0].indices.clone(),
    });
    let mut cpu_app = counted_app(&device, &root);
    cpu_app.update();
    spawn_mesh(
        &mut cpu_app,
        regia_render::MeshRenderer::new(cpu_mesh.clone(), material.clone()),
    );

    let mut app = counted_app(&device, &root);
    app.update();
    let handle = app
        .world_mut()
        .resource_mut::<AssetServer>()
        .load::<CpuMesh>(mesh_id)
        .expect("counted load");
    app.world_mut().resource_mut::<AssetServer>().wait_for_loads();
    spawn_mesh(
        &mut app,
        regia_render::MeshRenderer::counted(handle.clone(), material.clone()),
    );
    for _ in 0..4 {
        app.update();
        cpu_app.update();
    }
    let stats = |app: &regia_app::App| {
        let renderer = &app.world().resource::<regia_render::RuntimeRenderer>().0;
        (
            renderer.cache_stats().meshes_released,
            renderer.adopted_asset_mesh_count(),
        )
    };
    let before = stats(&app);
    assert_eq!(before.1, 1, "the counted mesh is adopted");
    let first = view_pixels(&mut app, &device);
    assert_eq!(
        first,
        view_pixels(&mut cpu_app, &device),
        "the frame before the respawn draws the CPU control's pixels"
    );
    let old_stand_in = {
        let extracted = app.world().resource::<regia_render::ExtractedFrame>();
        std::sync::Arc::downgrade(&extracted.meshes[0].mesh)
    };

    // Despawn and respawn between two frames; the old stand-in dies first.
    for entity in mesh_entities(&mut app) {
        app.world_mut().despawn(entity);
    }
    app.world_mut()
        .resource_mut::<regia_render::ExtractedFrame>()
        .meshes
        .clear();
    assert_eq!(old_stand_in.strong_count(), 0, "the old stand-in is gone");
    spawn_mesh(
        &mut app,
        regia_render::MeshRenderer::counted(handle.clone(), material.clone()),
    );
    for entity in mesh_entities(&mut cpu_app) {
        cpu_app.world_mut().despawn(entity);
    }
    spawn_mesh(
        &mut cpu_app,
        regia_render::MeshRenderer::new(cpu_mesh.clone(), material.clone()),
    );
    for frame in 0..2 {
        app.update();
        cpu_app.update();
        assert_eq!(
            stats(&app),
            before,
            "frame {frame} after the respawn: the adopted entry is kept (no release, no re-adoption)"
        );
        let got = view_pixels(&mut app, &device);
        let want = view_pixels(&mut cpu_app, &device);
        let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(
            differing, 0,
            "frame {frame} after the respawn draws the CPU control's pixels"
        );
        let corner = &want[..4];
        let covered = want
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px.as_slice() != corner)
            .count();
        assert!(covered > 64 * 64 / 8, "the quad is drawn: {covered} pixels");
    }
    let uploads = app
        .world()
        .resource::<regia_render::CountedAssetUploads>()
        .meshes
        .clone();
    assert_eq!(uploads.counters(), (1, 0), "one upload, never freed");
    drop((app, cpu_app, cpu_server, handle));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// A compact cooked pyramid over a 1.6 m square with three LODs sharing its five vertices: LOD 0
/// the four sides around the raised centre, LOD 1 the flat square, LOD 2 one half of it. The
/// corner normals lean outward and the centre's points up, so each level shades differently.
/// `screen` holds LOD 0's and LOD 1's switch sizes (LOD 2 is the fallback, 0).
fn lod_pyramid(screen: [f32; 2]) -> CookedMesh {
    let positions = [
        [-0.8f32, 0.0, -0.8],
        [0.8, 0.0, -0.8],
        [0.8, 0.0, 0.8],
        [-0.8, 0.0, 0.8],
        [0.0, 0.5, 0.0],
    ];
    let uvs = [
        [0u16, 0],
        [0x3C00, 0],
        [0x3C00, 0x3C00],
        [0, 0x3C00],
        [0x3800, 0x3800],
    ];
    let lod0: [u16; 12] = [0, 3, 4, 3, 2, 4, 2, 1, 4, 1, 0, 4];
    let lod1: [u16; 6] = [0, 3, 2, 0, 2, 1];
    let lod2: [u16; 3] = [0, 3, 2];
    let mut mesh = CookedMesh {
        aabb_min: [-0.8, 0.0, -0.8],
        aabb_max: [0.8, 0.5, 0.8],
        bounding_sphere: [0.0, 0.25, 0.0, 1.2],
        stream_positions: Vec::new(),
        stream_normals_tangents: Vec::new(),
        stream_uv0: Vec::new(),
        indices: Vec::new(),
        index_width: CookedIndexWidth::U16,
        vertex_count: 5,
        lods: vec![
            CookedMeshLod {
                screen_relative_size: screen[0],
                index_byte_offset: 0,
                index_byte_length: 24,
            },
            CookedMeshLod {
                screen_relative_size: screen[1],
                index_byte_offset: 24,
                index_byte_length: 12,
            },
            CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 36,
                index_byte_length: 6,
            },
        ],
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
        let lean = [p[0] * 0.6, 1.0, p[2] * 0.6];
        let len = (lean[0] * lean[0] + lean[1] * lean[1] + lean[2] * lean[2]).sqrt();
        let (x, y) = regia_asset::encode_octahedral_normal([lean[0] / len, lean[1] / len, lean[2] / len]);
        for w in [x, y, 0, -1] {
            mesh.stream_normals_tangents.extend_from_slice(&w.to_le_bytes());
        }
        for h in uv {
            mesh.stream_uv0.extend_from_slice(&h.to_le_bytes());
        }
    }
    for i in lod0.iter().chain(&lod1).chain(&lod2) {
        mesh.indices.extend_from_slice(&i.to_le_bytes());
    }
    mesh
}

/// Moves the one `MeshRenderer` entity of `app` to `at`.
fn place_mesh(app: &mut regia_app::App, at: regia_core::glam::Vec3) {
    let transform = regia_scene::Transform::from_affine(regia_core::glam::Affine3A::from_translation(at));
    for entity in mesh_entities(app) {
        let mut e = app.world_mut().entity_mut(entity);
        e.insert((
            transform,
            regia_scene::GlobalTransform(transform.compute_affine()),
        ));
    }
}

/// w7 item 4: a counted mesh with a three-level `MeshLodChain` draws each level from the adopted
/// upload: at three distances (one per colour LOD; the shadow takes its own coarser level) the
/// counted App's pixels equal its CPU twin's (the CPU App draws `load_cooked_model`'s vertices
/// with the same chain), `assets.handles` shows no CPU bytes, the mesh uploads once and each of
/// the two coarser levels uploads its index list once (its colour, shadow and later frames share
/// it). The chain comes from the counted asset's payload (`MeshLodChain::from_cooked_asset`).
#[test]
fn a_counted_mesh_draws_each_level_of_its_lod_chain_like_its_cpu_twin() {
    use regia_core::glam::Vec3;
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    // The lab view: eye (0, 2.2, 1.2) looking at the origin. The mesh moves away along that
    // line; each distance's projected size picks one colour LOD (thresholds halfway between
    // neighbouring sizes, in the selector's own terms: the tier's colour factor, 1080p).
    let eye = Vec3::new(0.0, 2.2, 1.2);
    let away = (-eye).normalize();
    let steps = [0.0f32, 2.5, 7.5];
    let fov = regia_render::Camera::default().fov_y;
    let settings = regia_render::RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopLow);
    let factor = regia_render::MeshLodScales::of_profile(&settings.profile).color_factor(1.0);
    let threshold_scale = regia_asset::lod_threshold_scale(1080);
    let sizes = steps.map(|k| {
        let centre = away * k + Vec3::new(0.0, 0.25, 0.0);
        regia_asset::projected_screen_size(1.2, (centre - eye).length(), fov) * factor
    });
    let screen = [
        (sizes[0] * sizes[1]).sqrt() / threshold_scale,
        (sizes[1] * sizes[2]).sqrt() / threshold_scale,
    ];
    let cooked = lod_pyramid(screen);
    let (cache_root, root, gltf, mesh_id) = pak_project_with(cooked.clone());
    let material = std::sync::Arc::new(regia_render::LabMaterial::default());

    let mut cpu_server = open(&cache_root);
    let draws = load_cooked_model(&mut cpu_server, gltf).expect("cpu path");
    let cpu_mesh = std::sync::Arc::new(regia_render::CpuMesh {
        vertices: draws.meshes[0].vertices.clone(),
        indices: draws.meshes[0].indices.clone(),
    });
    assert_eq!(cpu_mesh.vertices.len(), 5, "every level's vertices");
    let cpu_chain = regia_render::MeshLodChain::from_cooked(&cooked).expect("cpu chain");
    let mut cpu_app = counted_app(&device, &root);
    cpu_app.insert_resource(regia_render::MeshLodCrossfade(false));
    cpu_app.update();
    spawn_mesh(
        &mut cpu_app,
        regia_render::MeshRenderer::new(cpu_mesh.clone(), material.clone()),
    );
    for entity in mesh_entities(&mut cpu_app) {
        cpu_app.world_mut().entity_mut(entity).insert(cpu_chain.clone());
    }

    let mut app = counted_app(&device, &root);
    app.insert_resource(regia_render::MeshLodCrossfade(false));
    app.update();
    let (handle, chain) = {
        let mut server = app.world_mut().resource_mut::<AssetServer>();
        let handle = server.load::<CpuMesh>(mesh_id).expect("counted load");
        server.wait_for_loads();
        let chain = regia_render::MeshLodChain::from_cooked_asset(&server.payload_reader(), mesh_id)
            .expect("chain read")
            .expect("three levels");
        (handle, chain)
    };
    assert_eq!(chain.levels.len(), 3);
    for (got, want) in chain.levels.iter().zip(&cpu_chain.levels) {
        assert_eq!(got.indices, want.indices, "the payload's chain is the cooked one");
        assert_eq!(got.screen_size.to_bits(), want.screen_size.to_bits());
    }
    spawn_mesh(
        &mut app,
        regia_render::MeshRenderer::counted(handle.clone(), material.clone()),
    );
    for entity in mesh_entities(&mut app) {
        app.world_mut().entity_mut(entity).insert(chain.clone());
    }
    let uploads = app
        .world()
        .resource::<regia_render::CountedAssetUploads>()
        .meshes
        .clone();

    let mut frames = Vec::new();
    for (lod, &k) in steps.iter().enumerate() {
        place_mesh(&mut app, away * k);
        place_mesh(&mut cpu_app, away * k);
        for _ in 0..3 {
            app.update();
            cpu_app.update();
        }
        let colour_key = regia_render::mesh_lod_cache_key(lod, false);
        for (name, a) in [("counted", &app), ("cpu", &cpu_app)] {
            let extracted = a.world().resource::<regia_render::ExtractedFrame>();
            assert!(
                extracted.meshes.iter().any(|m| m.lod_key == colour_key),
                "{name}: distance {k} draws colour LOD {lod}: {:?}",
                extracted.meshes.iter().map(|m| m.lod_key).collect::<Vec<_>>()
            );
        }
        let got = view_pixels(&mut app, &device);
        let want = view_pixels(&mut cpu_app, &device);
        let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(
            differing, 0,
            "LOD {lod}: the counted chain draws its CPU twin's pixels"
        );
        let corner = &want[..4];
        let covered = want
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px.as_slice() != corner)
            .count();
        assert!(covered > 16, "LOD {lod} is drawn: {covered} pixels");
        frames.push(got);
    }
    assert_ne!(frames[0], frames[1]);
    assert_ne!(frames[1], frames[2]);

    let renderer = &app.world().resource::<regia_render::RuntimeRenderer>().0;
    assert_eq!(uploads.counters(), (1, 0), "the mesh uploads once");
    assert_eq!(
        renderer.counted_lod_index_uploads(),
        2,
        "LOD 1 and LOD 2 upload their index lists once each (LOD 0 is the upload's own)"
    );
    assert_eq!(renderer.adopted_asset_mesh_count(), 1);
    let server = app.world().resource::<AssetServer>();
    assert_eq!(server.cpu_data_kept(mesh_id), Some(false));
    assert_eq!(
        handle_bytes(server, mesh_id),
        (0, uploads.resident_bytes()),
        "assets.handles: no CPU copy, the upload counted once"
    );
    drop((app, cpu_app, cpu_server, handle));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache_root);
}

// ---------------------------------------------------------------------------------------------
// w10-counted-deformers (ADR-7721)
// ---------------------------------------------------------------------------------------------

/// The draw keys the deformer tests give the kept-CPU control (a CPU mesh key) and both paths'
/// object (one instance each).
const KEPT_KEY: u64 = 0x7721 << 3;
const OBJECT: u64 = 0x000B_1EC7;

/// `compact_quad` with one morph target that lifts its far edge (vertices 2 and 3) 0.4 m.
fn morphing_quad() -> CookedMesh {
    let mut mesh = compact_quad();
    let mut deltas = Vec::new();
    for _ in 0..2 {
        for c in [0.0f32, 0.4, 0.0] {
            deltas.extend_from_slice(&regia_asset::f32_to_f16_round(c).to_le_bytes());
        }
    }
    mesh.morph_targets = vec![regia_asset::CookedMorphTarget {
        name: "lift".into(),
        vertex_indices: vec![2, 3],
        position_deltas: deltas,
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
        delta_min: [0.0, 0.4, 0.0],
        delta_max: [0.0, 0.4, 0.0],
    }];
    mesh
}

/// `mesh_id` loaded with `keep_cpu_data` on a server with an uploader: the control the counted
/// deformers are compared with (the CPU vertices `CpuMesh::from_cooked` builds).
fn kept_cpu_mesh(
    device: &RenderDevice,
    root: &Path,
    mesh_id: AssetId,
) -> (
    AssetServer,
    regia_asset::StrongHandle<CpuMesh>,
    regia_render::CpuMesh,
) {
    let mut server = open(root);
    let uploads = AssetMeshUploads::new(device);
    server.register_prepare(uploads);
    let handle = server
        .load_with::<CpuMesh>(mesh_id, regia_asset::LoadSettings { keep_cpu_data: true })
        .expect("kept load");
    server.wait_for_loads();
    assert_eq!(server.cpu_data_kept(mesh_id), Some(true));
    let render = {
        let asset = server.get(&handle).expect("kept mesh");
        regia_render::CpuMesh::from_cooked(asset).expect("render mesh")
    };
    (server, handle, render)
}

/// `mesh_id` loaded counted (no CPU copy kept) through `uploads`.
fn counted_server(
    root: &Path,
    uploads: &AssetMeshUploads,
    mesh_id: AssetId,
) -> (AssetServer, regia_asset::StrongHandle<CpuMesh>) {
    let mut server = open(root);
    server.register_prepare(uploads.clone());
    let handle = server.load::<CpuMesh>(mesh_id).expect("counted load");
    server.wait_for_loads();
    uploads.flush();
    assert_eq!(server.cpu_data_kept(mesh_id), Some(false));
    assert!(
        server.get(&handle).is_some_and(|mesh| mesh.positions.is_empty()),
        "the counted mesh keeps no CPU vertices"
    );
    (server, handle)
}

/// Pixels whose RGB differ by more than `levels`.
fn pixels_over(a: &[u8], b: &[u8], levels: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| (0..3).any(|k| p[k].abs_diff(q[k]) > levels))
        .count()
}

/// Item 1: a counted mesh with a morph target, drawn at weight 0.5 with the targets its upload
/// packed (`AssetMeshUploads::morph_targets`), draws the pixels of the same mesh loaded with
/// `keep_cpu_data` and drawn from its CPU vertices with `MorphTargets::from_cooked`. The counted
/// asset keeps no CPU vertices; its deformed copy is a GPU copy of the adopted vertex buffer.
#[test]
fn a_counted_morphing_mesh_at_half_weight_draws_like_its_kept_cpu_copy() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, _gltf, mesh_id) = project();
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    put(
        &cache,
        mesh_id,
        AssetTypeTag::mesh(),
        &morphing_quad().to_postcard().expect("mesh"),
    );
    let (kept_server, kept_handle, kept) = kept_cpu_mesh(&device, &root, mesh_id);
    let kept_targets = {
        let asset = kept_server.get(&kept_handle).expect("kept mesh");
        assert_eq!(asset.morph_targets.len(), 1, "the payload carries the target");
        regia_render::MorphTargets::from_cooked(&kept, asset)
            .expect("targets")
            .expect("one target")
    };

    let uploads = AssetMeshUploads::new(&device);
    let (server, _handle) = counted_server(&root, &uploads, mesh_id);
    let targets = uploads
        .morph_targets(mesh_id)
        .expect("the upload packed the morph targets");
    assert_eq!(targets.names(), kept_targets.names());
    assert_eq!(
        (
            targets.vertex_count(),
            targets.moved_vertex_count(),
            targets.gpu_bytes()
        ),
        (
            kept_targets.vertex_count(),
            kept_targets.moved_vertex_count(),
            kept_targets.gpu_bytes()
        ),
        "the same packed set as the CPU path's"
    );

    let material = regia_render::LabMaterial {
        base_color: [0.8, 0.7, 0.6, 1.0],
        ..regia_render::LabMaterial::default()
    };
    let control = lab(
        vec![material],
        vec![regia_render::LabMesh {
            vertices: kept.vertices.clone(),
            indices: kept.indices.clone(),
            material: 0,
            ..regia_render::LabMesh::unskinned()
        }],
    );
    let mut cpu_scene = control.as_frame_scene();
    cpu_scene.meshes[0].mesh_key = KEPT_KEY;
    cpu_scene.meshes[0].object_key = OBJECT;
    let mut scene = control.as_frame_scene();
    scene.meshes[0] = FrameMesh {
        mesh_key: asset_mesh_key(mesh_id),
        object_key: OBJECT,
        vertices: &[],
        indices: &[],
        ..scene.meshes[0]
    };

    let instance =
        |targets: &std::sync::Arc<regia_render::MorphTargets>, weight: f32| regia_render::MorphInstance {
            object_key: OBJECT,
            targets: std::sync::Arc::clone(targets),
            weights: vec![weight],
            mode: regia_render::MorphMode::Game,
        };
    let mut cpu_renderer = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    let rest = renderer
        .render_offscreen(&device, &scene)
        .expect("rest frame")
        .pixels;
    let kept_targets = std::sync::Arc::new(kept_targets);
    cpu_renderer.set_morph_instances(&[instance(&kept_targets, 0.5)]);
    renderer.set_morph_instances(&[instance(&targets, 0.5)]);
    let (mut want, mut got) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        want = cpu_renderer
            .render_offscreen(&device, &cpu_scene)
            .expect("kept frame")
            .pixels;
        got = renderer
            .render_offscreen(&device, &scene)
            .expect("counted frame")
            .pixels;
    }
    let stats = renderer.morph_stats();
    eprintln!("counted morph stats: {stats:?}");
    assert_eq!(
        (stats.drawn, stats.counted, stats.mismatched),
        (1, 1, 0),
        "the counted instance is deformed from its upload"
    );
    assert_eq!(
        cpu_renderer.morph_stats().counted,
        0,
        "the control draws CPU vertices"
    );
    assert!(
        pixels_over(&want, &rest, 8) > 64,
        "the control's weight 0.5 moves the quad: {} pixels",
        pixels_over(&want, &rest, 8)
    );
    let differing = got.iter().zip(&want).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the counted mesh at weight 0.5 draws its kept CPU copy's pixels"
    );
    assert_eq!(uploads.counters(), (1, 0), "frames upload nothing");
    assert_eq!(server.cpu_data_kept(mesh_id), Some(false));
    assert_eq!(
        handle_bytes(&server, mesh_id),
        (0, uploads.resident_bytes()),
        "no CPU copy; the upload counted once"
    );
    drop((server, kept_server, kept_handle));
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 2: a skinned counted mesh on the compute-skinning path skins from its adopted vertex
/// buffer (the four strongest influences it uploaded, ADR-8610): `skinning_stats` shows the twin
/// path (`instances` and `counted`), the frame is byte-identical to the kept CPU copy's compute
/// twin, and within tolerance of the vertex-shader path.
#[test]
fn a_skinned_counted_mesh_on_the_compute_path_skins_from_its_upload() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, _gltf, mesh_id) = project();
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    put(
        &cache,
        mesh_id,
        AssetTypeTag::mesh(),
        &skinned_quad().to_postcard().expect("mesh"),
    );
    let (kept_server, kept_handle, kept) = kept_cpu_mesh(&device, &root, mesh_id);
    let uploads = AssetMeshUploads::new(&device);
    let (server, _handle) = counted_server(&root, &uploads, mesh_id);

    let palette = [
        regia_render::JointMatrix::translation(0.05, 0.0, 0.0),
        regia_render::JointMatrix::from_mat4(
            regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(-0.2, 0.3, 0.0))
                * regia_core::glam::Mat4::from_rotation_y(0.4),
        ),
    ];
    let control = lab(
        vec![regia_render::LabMaterial::default()],
        vec![regia_render::LabMesh {
            vertices: kept.vertices.clone(),
            indices: kept.indices.clone(),
            material: 0,
            ..regia_render::LabMesh::unskinned()
        }],
    );
    let mut cpu_scene = control.as_frame_scene();
    cpu_scene.meshes[0].mesh_key = KEPT_KEY;
    cpu_scene.meshes[0].object_key = OBJECT;
    cpu_scene.meshes[0].joints = &palette;
    let mut scene = control.as_frame_scene();
    scene.meshes[0] = FrameMesh {
        mesh_key: asset_mesh_key(mesh_id),
        object_key: OBJECT,
        vertices: &[],
        indices: &[],
        joints: &palette,
        ..scene.meshes[0]
    };

    let mut cpu_compute = FrameRenderer::new();
    cpu_compute.set_compute_skinning(true);
    let mut compute = FrameRenderer::new();
    compute.set_compute_skinning(true);
    compute.set_asset_meshes(Some(uploads.clone()));
    let mut vertex_path = FrameRenderer::new();
    vertex_path.set_asset_meshes(Some(uploads.clone()));
    let (mut kept_px, mut got, mut vs) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..3 {
        kept_px = cpu_compute
            .render_offscreen(&device, &cpu_scene)
            .expect("kept compute frame")
            .pixels;
        got = compute
            .render_offscreen(&device, &scene)
            .expect("counted compute frame")
            .pixels;
        vs = vertex_path
            .render_offscreen(&device, &scene)
            .expect("counted vertex-shader frame")
            .pixels;
    }
    let stats = compute.skinning_stats();
    eprintln!("counted compute skinning: {stats:?}");
    assert_eq!(stats.unsupported, None, "lavapipe runs the kernel");
    assert_eq!(
        (stats.instances, stats.counted),
        (1, 1),
        "the counted instance is drawn from a compute twin of its upload"
    );
    assert_eq!(
        vertex_path.skinning_stats().instances,
        0,
        "the control stays on the vertex shader"
    );
    assert_eq!(cpu_compute.skinning_stats().counted, 0);
    let differing = got.iter().zip(&kept_px).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the counted compute twin draws the kept CPU copy's compute twin's pixels"
    );
    let over = pixels_over(&got, &vs, 3);
    assert!(
        over <= 64 * 64 / 200,
        "the compute twin matches the vertex-shader path within 3 levels on all but 0.5 % of the pixels: {over} over"
    );
    let corner = &vs[..4];
    let covered = vs
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px.as_slice() != corner)
        .count();
    assert!(covered > 64 * 64 / 8, "the quad is drawn: {covered} pixels");
    assert_eq!(uploads.counters(), (1, 0), "frames upload nothing");
    assert_eq!(
        handle_bytes(&server, mesh_id),
        (0, uploads.resident_bytes()),
        "no CPU copy; the upload counted once"
    );
    drop((server, kept_server, kept_handle));
    let _ = std::fs::remove_dir_all(&root);
}

/// A compact cooked box of half extents `half` (24 vertices, outward normals, 16-bit indices,
/// counter-clockwise from outside).
fn compact_box(half: [f32; 3]) -> CookedMesh {
    let mut mesh = compact_quad();
    mesh.stream_positions.clear();
    mesh.stream_normals_tangents.clear();
    mesh.stream_uv0.clear();
    mesh.indices.clear();
    let mut index = 0u16;
    for axis in 0..3 {
        for sign in [-1.0f32, 1.0] {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let mut normal = [0.0f32; 3];
            normal[axis] = sign;
            for (cu, cv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let mut p = [0.0f32; 3];
                p[axis] = sign * half[axis];
                p[u] = cu * half[u];
                p[v] = cv * half[v];
                for c in p {
                    mesh.stream_positions.extend_from_slice(&c.to_le_bytes());
                }
                let (x, y) = regia_asset::encode_octahedral_normal(normal);
                for w in [x, y, 0, -1] {
                    mesh.stream_normals_tangents.extend_from_slice(&w.to_le_bytes());
                }
                for h in [(cu > 0.0) as u16 * 0x3C00, (cv > 0.0) as u16 * 0x3C00] {
                    mesh.stream_uv0.extend_from_slice(&h.to_le_bytes());
                }
            }
            let order: [u16; 6] = if sign > 0.0 {
                [0, 1, 2, 0, 2, 3]
            } else {
                [0, 2, 1, 0, 3, 2]
            };
            for i in order {
                mesh.indices.extend_from_slice(&(index + i).to_le_bytes());
            }
            index += 4;
        }
    }
    mesh.vertex_count = 24;
    mesh.lods[0].index_byte_length = 36 * 2;
    mesh.aabb_min = [-half[0], -half[1], -half[2]];
    mesh.aabb_max = half;
    let r = (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt();
    mesh.bounding_sphere = [0.0, 0.0, 0.0, r];
    mesh
}

/// Item 3: the probes trace a counted box (no CPU vertices) with a signed-distance brick baked
/// from a GPU readback of its upload: the probes beside it read what they read beside the same
/// box loaded with `keep_cpu_data` and drawn from its CPU vertices (every probe's last rays, within
/// the probe tests' tolerance), with one readback and no CPU copy kept.
#[test]
fn a_probe_beside_a_counted_box_reads_like_beside_its_kept_cpu_copy() {
    let _serial = sharing();
    let Ok(device) = RenderDevice::shared_headless() else {
        eprintln!("SKIP: no adapter");
        return;
    };
    let (root, _gltf, mesh_id) = project();
    let cache = cooked_cache_dir(&root, &regia_asset::current_platform());
    put(
        &cache,
        mesh_id,
        AssetTypeTag::mesh(),
        &compact_box([1.0, 1.0, 1.0]).to_postcard().expect("mesh"),
    );
    let (kept_server, kept_handle, kept) = kept_cpu_mesh(&device, &root, mesh_id);
    let uploads = AssetMeshUploads::new(&device);
    let (server, _handle) = counted_server(&root, &uploads, mesh_id);

    let quality = {
        let mut q = regia_render::FrameQuality::from_profile(&regia_render::RenderProfile::from_preset(
            regia_render::RenderProfilePreset::DesktopHigh,
        ));
        q.taau = false;
        q.render_scale_permille = 1000;
        q
    };
    let mut post = regia_render::PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
    let control = LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 1.7, 2.0],
        target: [0.0, 1.0, -3.0],
        fov_y: 1.0,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([-0.4, -0.75, -0.5], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.7, 0.4, 0.3, 1.0],
            roughness: 0.9,
            ..regia_render::LabMaterial::default()
        }],
        meshes: vec![regia_render::LabMesh {
            vertices: kept.vertices.clone(),
            indices: kept.indices.clone(),
            material: 0,
            model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(0.0, 1.0, -3.0))
                .to_cols_array_2d(),
            ..regia_render::LabMesh::unskinned()
        }],
    };
    let mut cpu_scene = control.as_frame_scene_with_quality(quality);
    cpu_scene.meshes[0].mesh_key = KEPT_KEY;
    cpu_scene.meshes[0].object_key = OBJECT;
    let mut scene = control.as_frame_scene_with_quality(quality);
    scene.meshes[0] = FrameMesh {
        mesh_key: asset_mesh_key(mesh_id),
        object_key: OBJECT,
        vertices: &[],
        indices: &[],
        ..scene.meshes[0]
    };
    let settings = regia_render::DdgiSettings {
        cascades: 2,
        dims: [12, 8, 12],
        spacing: 1.0,
        update_divisor: 1,
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..regia_render::DdgiSettings::default()
    };
    let mut cpu_renderer = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    renderer.set_asset_meshes(Some(uploads.clone()));
    for r in [&mut cpu_renderer, &mut renderer] {
        r.set_ddgi_settings(settings.clone());
        r.set_ddgi_enabled(Some(true));
    }
    for _ in 0..6 {
        cpu_renderer
            .render_offscreen(&device, &cpu_scene)
            .expect("kept frame");
        renderer.render_offscreen(&device, &scene).expect("counted frame");
    }
    assert!(
        cpu_renderer.ddgi_brick_resident(KEPT_KEY),
        "the control's box has a brick"
    );
    assert!(
        renderer.ddgi_brick_resident(asset_mesh_key(mesh_id)),
        "the counted box has a brick"
    );
    assert_eq!(uploads.gi_readbacks(), 1, "one readback, on the brick's miss");
    let want = cpu_renderer
        .ddgi_read_back(&device)
        .expect("kept read back")
        .expect("the control's probes ran");
    let got = renderer
        .ddgi_read_back(&device)
        .expect("counted read back")
        .expect("the counted frame's probes ran");
    assert_eq!(want.instances, 1, "the control traces its box");
    assert_eq!(got.instances, want.instances, "the counted box is traced");
    assert_eq!(got.rays.len(), want.rays.len());
    // Probes on the box's side (cascade 0, x -2..=2, y 0..=2, z -1..=1 around the box at z -3).
    let mut hits = 0;
    for x in -2..=2 {
        for y in 0..=2 {
            for z in -5..=-1 {
                let slot = want.slot(0, [x, y, z]) as usize;
                for (a, b) in want.rays[slot * 64..slot * 64 + 64]
                    .iter()
                    .zip(&got.rays[slot * 64..slot * 64 + 64])
                {
                    let (ta, tb) = (a[3], b[3]);
                    if (0.0..1.0e8).contains(&ta) {
                        hits += 1;
                    }
                    assert!(
                        (ta - tb).abs() <= 1e-3 * ta.abs().max(1.0),
                        "probe {:?}: hit distance {ta} vs {tb}",
                        [x, y, z]
                    );
                    for k in 0..3 {
                        assert!(
                            (a[k] - b[k]).abs() <= 0.01 * a[k].abs().max(1.0),
                            "probe {:?}: radiance {a:?} vs {b:?}",
                            [x, y, z]
                        );
                    }
                }
            }
        }
    }
    assert!(hits > 100, "the probes beside the box hit it: {hits} rays");
    assert_eq!(
        handle_bytes(&server, mesh_id),
        (0, uploads.resident_bytes()),
        "no CPU copy kept; the readback is dropped after the bake"
    );
    drop((server, kept_server, kept_handle));
    let _ = std::fs::remove_dir_all(&root);
}
