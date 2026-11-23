// SPDX-License-Identifier: MPL-2.0
//! (d) Process RSS under the host memory budget (ADR-7891, DO27 tester item host-memory-spill),
//! alone in its own binary (its own process) so no other test's memory moves the number.
//!
//! A pak of 8 distinct static meshes (~67 MiB each, 536 MiB) is loaded through a
//! `HostMeshStore` with a 64 MiB `cpu_mesh_bytes` budget and drawn once. The RSS growth from the
//! baseline must stay within
//!
//! `budget + mapped_touched_bytes + frame_growth + FIXED_OVERHEAD`
//!
//! where each term is derived, not tuned:
//! - `budget`: the CPU mesh copies the store may keep (64 MiB);
//! - `mapped_touched_bytes` (the store's counter): the pak entries read through the pak's memory
//!   mapping. Those file pages stay mapped and resident in this process (`RssFile`) after a read;
//!   they are clean page cache the kernel can reclaim, not a CPU copy;
//! - `frame_growth`: the RSS change across the frame that uploads the meshes (render and wait
//!   for the GPU), measured. The store's copies do not change during that call (the budget acts
//!   in `after_frame`, after it), so this is the renderer's and the device's own memory: on a CPU
//!   adapter (lavapipe, the reference for this test) the "GPU" buffers and wgpu's staging are
//!   process heap and stay resident inside the allocator's blocks; on a discrete card it is the
//!   driver's host side. None of it is a CPU mesh copy;
//! - [`FIXED_OVERHEAD`] below.
//!
//! Equivalently: the load's growth minus what `after_frame` gave back is at most the budget, the
//! mapped pages and the fixed overhead.
//!
//! The baseline is taken after device creation *and* one warm-up frame of a tiny mesh: that frame
//! compiles the pipelines (lavapipe JITs them, tens of MiB of code and LLVM state) and builds the
//! render targets, none of which is mesh data. Before that, the pak is written and its writer's
//! buffers freed.
//!
//! On the base (`59ff4b61d`, no budget: every CPU copy kept), the same growth is at least the
//! 536 MiB of CPU copies plus the mapped and device terms; `REGIA_HOST_RSS_CONTROL=1` runs that
//! control here (budget unlimited) and asserts the bound then fails by more than 400 MiB.
//!
//! Linux only: RSS is `VmRSS`, and the overhead derivation relies on glibc returning each mesh
//! array to the kernel when it is freed (below).

#![cfg(target_os = "linux")]

mod host_mesh_support;

use host_mesh_support::{IDENTITY, MIB, VERTEX_BYTES, grid_scene, open_server, write_mesh_pak};
use regia_render::host_memory::{HostMemoryBudget, HostMeshStore};
use regia_render::{FrameRenderer, FrameScene, LabMaterial, RenderDevice};

/// 8 meshes of ~67 MiB of vertices: each array is larger than glibc's largest dynamic mmap
/// threshold (32 MiB on 64-bit, `DEFAULT_MMAP_THRESHOLD_MAX`), so each is its own anonymous
/// mapping and freeing it unmaps it at once (no heap fragmentation can keep a dropped copy
/// resident).
const MESHES: u32 = 8;
const VERTICES: usize = (67 * MIB / VERTEX_BYTES) as usize;
const BUDGET: u64 = 64 * MIB;

/// Everything else the load and the frame may leave resident, with its derivation:
/// - the asset server's mount: the pak header, TOC and manifest (8 entries: KiB) and the mounted
///   pak's bookkeeping;
/// - the store's and the renderer's per-mesh records (8 meshes: KiB) and the mesh arenas' records;
/// - the compute pool's decode threads: their stacks (touched pages only, tens of KiB each) and
///   their malloc arenas' small allocations (the mesh arrays themselves are mmapped, see
///   `VERTICES`), a few MiB across all threads;
/// - page-granular rounding of every mapping (KiB).
///
/// 32 MiB bounds the sum with room for a pool of 128 threads touching 128 KiB each (16 MiB); a
/// growth past it means a CPU copy or a buffer the derivation does not know about.
const FIXED_OVERHEAD: u64 = 32 * MIB;

fn rss() -> u64 {
    regia_render::process_rss_bytes().expect("VmRSS on Linux")
}

#[test]
fn rss_growth_stays_within_the_host_budget_derivation() {
    let control = std::env::var_os("REGIA_HOST_RSS_CONTROL").is_some_and(|v| v == "1");
    let dir = regia_test_utils::unique_temp_dir("host-budget-rss");
    let (root, ids) = write_mesh_pak(&dir, MESHES, VERTICES);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let after_device = rss();
    let mut renderer = FrameRenderer::new();
    let lab = grid_scene();
    let material = LabMaterial::default();

    // Warm-up: one owned tile (pipelines, targets), then the baseline.
    let mut warm = HostMeshStore::new(HostMemoryBudget::UNLIMITED);
    let tile = warm.insert_owned(
        host_mesh_support::tile_vertices(0, 4),
        host_mesh_support::TILE_INDICES.to_vec(),
    );
    {
        let base = lab.as_frame_scene();
        let scene = FrameScene {
            meshes: vec![warm.frame_mesh(tile, &material, 1, 1, IDENTITY).expect("tile")],
            ..base
        };
        renderer.render_offscreen(&device, &scene).expect("warm-up frame");
    }
    device.wait_idle().expect("idle");
    let baseline = rss();

    let server = open_server(&root);
    let reader = server.payload_reader();
    let budget = if control {
        HostMemoryBudget::UNLIMITED
    } else {
        HostMemoryBudget {
            cpu_mesh_bytes: BUDGET,
            cpu_texture_bytes: HostMemoryBudget::UNLIMITED.cpu_texture_bytes,
        }
    };
    let mut store = HostMeshStore::new(budget);
    let meshes: Vec<_> = ids
        .iter()
        .map(|&id| store.insert_pak(&reader, id).expect("load mesh"))
        .collect();
    let loaded = store.counters().cpu_mesh_bytes;
    let frame_growth;
    let after_load;
    {
        let base = lab.as_frame_scene();
        store
            .prepare(&renderer, &base.quality, meshes.iter().map(|&m| (m, &material)))
            .expect("prepare");
        let scene = FrameScene {
            meshes: meshes
                .iter()
                .enumerate()
                .map(|(i, &m)| {
                    store
                        .frame_mesh(m, &material, 1, 2 + i as u64, IDENTITY)
                        .expect("mesh")
                })
                .collect(),
            ..base
        };
        let before_frame = rss();
        renderer.render_offscreen(&device, &scene).expect("frame");
        device.wait_idle().expect("idle");
        frame_growth = rss().saturating_sub(before_frame);
        after_load = before_frame;
    }
    store.after_frame(&renderer);
    let after = rss();

    let counters = store.counters();
    let growth = after.saturating_sub(baseline);
    let bound = BUDGET + counters.mapped_touched_bytes + frame_growth + FIXED_OVERHEAD;
    let mib = |b: u64| b as f64 / MIB as f64;
    eprintln!(
        "control={control}\n\
         rss: after device {:.1} MiB, baseline {:.1} MiB, after the load {:.1} MiB, after {:.1} MiB (growth {:.1} MiB; from after device {:.1} MiB)\n\
         loaded {:.1} MiB, kept {:.1} MiB, dropped {:.1} MiB, mapped touched {:.1} MiB, frame growth {:.1} MiB\n\
         bound {:.1} MiB = budget {:.1} + mapped {:.1} + frame {:.1} + overhead {:.1}",
        mib(after_device),
        mib(baseline),
        mib(after_load),
        mib(after),
        mib(growth),
        mib(after.saturating_sub(after_device)),
        mib(loaded),
        mib(counters.cpu_mesh_bytes),
        mib(counters.dropped_bytes),
        mib(counters.mapped_touched_bytes),
        mib(frame_growth),
        mib(bound),
        mib(BUDGET),
        mib(counters.mapped_touched_bytes),
        mib(frame_growth),
        mib(FIXED_OVERHEAD),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        loaded >= 512 * MIB,
        "the pak holds at least 512 MiB of CPU geometry"
    );
    if control {
        assert!(
            growth > bound + 400 * MIB,
            "without a budget every CPU copy stays: growth {:.1} MiB vs bound {:.1} MiB",
            mib(growth),
            mib(bound)
        );
        return;
    }
    assert!(counters.cpu_mesh_bytes <= BUDGET, "{counters:?}");
    assert!(
        growth <= bound,
        "RSS grew {:.1} MiB, over the derived bound {:.1} MiB",
        mib(growth),
        mib(bound)
    );
}
