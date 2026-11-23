// SPDX-License-Identifier: MPL-2.0
//! The host memory budget on the engine's CPU mesh copies (ADR-7891, DO27 tester item
//! host-memory-spill): a pak of 512 MiB of distinct static meshes, drawn once under a 64 MiB
//! `cpu_mesh_bytes` budget, keeps at most 64 MiB of CPU copies; the dropped meshes draw from their
//! GPU buffers with the same pixels; after the GPU buffers go (the device-loss path) they are
//! re-read from the pak and draw the same pixels again; a mesh a CPU consumer pins is never
//! dropped. Counters, fixed frames, lavapipe; no wall clock.

mod host_mesh_support;

use host_mesh_support::{IDENTITY, MIB, VERTEX_BYTES, grid_scene, open_server, write_mesh_pak};
use regia_render::host_memory::{HostMemoryBudget, HostMeshId, HostMeshStore};
use regia_render::{FrameRenderer, FrameScene, LabMaterial, RenderDevice};

/// 64 meshes of (just over) 8 MiB of vertices each: 512 MiB of distinct CPU geometry.
const MESHES: u32 = 64;
const VERTICES: usize = (8 * MIB).div_ceil(VERTEX_BYTES) as usize;
const BUDGET: u64 = 64 * MIB;

/// One GPU test at a time in this binary (device churn, ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

struct Fixture {
    root: std::path::PathBuf,
    server: regia_asset::AssetServer,
    ids: Vec<regia_asset::AssetId>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(tag: &str) -> Fixture {
    let dir = regia_test_utils::unique_temp_dir(tag);
    let (root, ids) = write_mesh_pak(&dir, MESHES, VERTICES);
    let server = open_server(&root);
    assert_eq!(server.pak_count(), 1, "the pak mounts");
    Fixture { root, server, ids }
}

/// Draws every mesh of `meshes` once: prepare, build the scene, render, enforce the budget.
/// Returns the frame's pixels and how many meshes `prepare` re-read.
fn frame(
    store: &mut HostMeshStore,
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    meshes: &[HostMeshId],
    material: &LabMaterial,
) -> (Vec<u8>, u32) {
    let lab = grid_scene();
    let base = lab.as_frame_scene();
    let reread = store
        .prepare(renderer, &base.quality, meshes.iter().map(|&m| (m, material)))
        .expect("prepare");
    let pixels = {
        let scene = FrameScene {
            meshes: meshes
                .iter()
                .enumerate()
                .map(|(i, &m)| {
                    store
                        .frame_mesh(m, material, 1, 1 + i as u64, IDENTITY)
                        .expect("frame mesh")
                })
                .collect(),
            ..base
        };
        renderer.render_offscreen(device, &scene).expect("frame").pixels
    };
    store.after_frame(renderer);
    (pixels, reread)
}

fn load_all(store: &mut HostMeshStore, fixture: &Fixture) -> Vec<HostMeshId> {
    let reader = fixture.server.payload_reader();
    fixture
        .ids
        .iter()
        .map(|&id| store.insert_pak(&reader, id).expect("load mesh"))
        .collect()
}

fn lit_pixels(pixels: &[u8]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
        .count()
}

/// (a) and the resident draw: 512 MiB drawn once under a 64 MiB budget keeps at most 64 MiB of
/// CPU copies (counter equality), and the next frame draws the dropped meshes from their GPU
/// buffers, pixel-equal and with no re-read.
#[test]
fn a_512_mib_pak_drawn_once_keeps_at_most_the_64_mib_budget() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture("host-budget-a");
    let device = device();
    let mut renderer = FrameRenderer::new();
    let mut store = HostMeshStore::new(HostMemoryBudget {
        cpu_mesh_bytes: BUDGET,
        cpu_texture_bytes: HostMemoryBudget::UNLIMITED.cpu_texture_bytes,
    });
    let meshes = load_all(&mut store, &fixture);
    let loaded = store.counters();
    assert!(loaded.cpu_mesh_bytes >= 512 * MIB, "{loaded:?}");
    assert_eq!(loaded.unsourced_bytes, 0, "every mesh has a pak source");
    let material = LabMaterial::default();

    let (first, reread) = frame(&mut store, &mut renderer, &device, &meshes, &material);
    let after = store.counters();
    eprintln!("loaded {loaded:?}\nafter the first frame {after:?}");
    assert_eq!(reread, 0, "the first frame has every CPU copy");
    assert!(
        after.cpu_mesh_bytes <= BUDGET,
        "counted CPU mesh bytes {} over the {BUDGET} budget: {after:?}",
        after.cpu_mesh_bytes
    );
    assert_eq!(
        after.cpu_mesh_bytes + after.dropped_bytes,
        loaded.cpu_mesh_bytes,
        "{after:?}"
    );
    assert!(after.drops >= u64::from(MESHES) - 8, "{after:?}");
    assert_eq!(after.over_budget_ticks, 0, "{after:?}");
    assert!(lit_pixels(&first) > 0, "the tiles draw");
    // The process-wide counters (render.memory_budget, MemoryReport::host) carry this store.
    let report = regia_render::memory_report(&device);
    assert!(
        report.host.dropped_bytes >= after.dropped_bytes,
        "{:?}",
        report.host
    );

    // The next frame: the dropped meshes are resident draws (no bytes, no re-read).
    let (second, reread) = frame(&mut store, &mut renderer, &device, &meshes, &material);
    assert_eq!(reread, 0, "a resident mesh needs no CPU copy");
    assert_eq!(store.counters().rereads, 0);
    assert!(
        second == first,
        "resident draws must draw the first frame's pixels"
    );
    assert!(store.counters().cpu_mesh_bytes <= BUDGET);
}

/// (b) The device-loss / re-upload path: with the GPU buffers dropped (`release_meshes`, the
/// hook a device loss or an eviction takes), every dropped mesh is re-read from the pak
/// (`rereads` == the dropped count) and the frame matches the first one pixel for pixel.
#[test]
fn dropped_gpu_buffers_re_read_every_dropped_mesh_from_the_pak() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture("host-budget-b");
    let device = device();
    let mut renderer = FrameRenderer::new();
    let mut store = HostMeshStore::new(HostMemoryBudget {
        cpu_mesh_bytes: BUDGET,
        cpu_texture_bytes: HostMemoryBudget::UNLIMITED.cpu_texture_bytes,
    });
    let meshes = load_all(&mut store, &fixture);
    let material = LabMaterial::default();
    let (first, _) = frame(&mut store, &mut renderer, &device, &meshes, &material);
    let dropped: Vec<HostMeshId> = meshes
        .iter()
        .copied()
        .filter(|&m| !store.has_cpu_copy(m).expect("known"))
        .collect();
    assert!(dropped.len() >= MESHES as usize - 8, "{:?}", store.counters());
    let keys: Vec<u64> = meshes.iter().map(|&m| store.mesh_key(m).expect("key")).collect();
    renderer.release_meshes(&keys);
    assert!(keys.iter().all(|&k| !renderer.mesh_buffers_resident(k)));
    let before = store.counters();
    let (again, reread) = frame(&mut store, &mut renderer, &device, &meshes, &material);
    let after = store.counters();
    eprintln!("before {before:?}\nafter {after:?}");
    assert_eq!(
        reread as usize,
        dropped.len(),
        "prepare re-reads each dropped mesh"
    );
    assert_eq!(after.rereads - before.rereads, dropped.len() as u64, "{after:?}");
    assert!(after.reread_bytes > before.reread_bytes);
    assert!(
        again == first,
        "the re-read meshes must draw the first frame's pixels"
    );
    assert!(
        after.cpu_mesh_bytes <= BUDGET,
        "dropped again after the upload: {after:?}"
    );
}

/// (c) A mesh a CPU consumer holds (a collider build) is never dropped, even with a zero budget;
/// the frame then ends over budget and says so.
#[test]
fn a_pinned_mesh_is_never_dropped() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture("host-budget-c");
    let device = device();
    let mut renderer = FrameRenderer::new();
    let mut store = HostMeshStore::new(HostMemoryBudget {
        cpu_mesh_bytes: 0,
        cpu_texture_bytes: HostMemoryBudget::UNLIMITED.cpu_texture_bytes,
    });
    let meshes = load_all(&mut store, &fixture);
    let collider = meshes[3];
    store.pin(collider).expect("pin");
    let material = LabMaterial::default();
    for _ in 0..3 {
        let _ = frame(&mut store, &mut renderer, &device, &meshes, &material);
        assert!(store.has_cpu_copy(collider).expect("known"), "pinned");
    }
    let c = store.counters();
    eprintln!("{c:?}");
    assert_eq!(
        c.cpu_mesh_bytes, c.pinned_bytes,
        "only the pinned copy stays: {c:?}"
    );
    assert!(c.pinned_bytes >= VERTICES as u64 * VERTEX_BYTES);
    assert_eq!(
        c.over_budget_ticks, 3,
        "every frame ends over a zero budget: {c:?}"
    );
    assert_eq!(c.drops, u64::from(MESHES) - 1, "{c:?}");
    // The consumer lets go: the next frame drops it too.
    store.unpin(collider).expect("unpin");
    let _ = frame(&mut store, &mut renderer, &device, &meshes, &material);
    assert!(!store.has_cpu_copy(collider).expect("known"));
    assert_eq!(store.counters().cpu_mesh_bytes, 0);
    assert_eq!(store.counters().pinned_bytes, 0);
}

/// A late CPU reader gets the bytes back from the pak, equal to what was loaded.
#[test]
fn a_late_cpu_reader_re_reads_the_same_bytes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture("host-budget-late");
    let device = device();
    let mut renderer = FrameRenderer::new();
    let mut store = HostMeshStore::new(HostMemoryBudget {
        cpu_mesh_bytes: 0,
        cpu_texture_bytes: HostMemoryBudget::UNLIMITED.cpu_texture_bytes,
    });
    let meshes = load_all(&mut store, &fixture);
    let (v0, i0) = {
        let (v, i) = store.cpu_geometry(meshes[0]).expect("bytes");
        (bytemuck::cast_slice::<_, u8>(v).to_vec(), i.to_vec())
    };
    let material = LabMaterial::default();
    let _ = frame(&mut store, &mut renderer, &device, &meshes, &material);
    assert!(!store.has_cpu_copy(meshes[0]).expect("known"));
    let (v, i) = store.cpu_geometry(meshes[0]).expect("re-read");
    assert_eq!(bytemuck::cast_slice::<_, u8>(v), v0.as_slice());
    assert_eq!(i, i0.as_slice());
    assert_eq!(store.counters().rereads, 1);
}
