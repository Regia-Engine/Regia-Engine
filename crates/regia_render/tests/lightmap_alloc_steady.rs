// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady frame with lightmaps (ADR-2107's "no per-frame allocation
//! regression"). A lightmap that nothing receives costs a frame nothing, and steady receiver frames,
//! GPU-driven or on the CPU loop, do not grow: the tables and their bind groups are reused. The
//! per-frame counts are printed (`--nocapture`).
//!
//! Counted on the frame's threads together, the drawing thread and the renderer's encode workers,
//! under the shared steady-frame rule (`regia_test_utils::alloc_counter`, ADR-5718). The test
//! counted the drawing thread alone and read floors over six batches, because "one batch in many
//! takes about 40 more for a reason not tied to frames": that was an encode segment the drawing
//! thread finished itself because no worker had started it yet (ADR-2299), 48 allocations that a
//! worker makes in every other frame.
//!
//! Its own test binary: it installs the counting global allocator.

use std::sync::Arc;

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, IndirectStrategy,
    LabMaterial, LabMesh, LabScene, LabVertex, RenderDevice,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck, SteadyAllocRun};
use regia_test_utils::device_pools;

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

const BATCH: u64 = 8;
/// The shared rule with this test's own slack: no measured batch (of six, after 40 warm frames
/// and two batches that agree) may exceed the settled one by a whole allocation per frame.
const RULE: SteadyAllocCheck = SteadyAllocCheck {
    growth_slack: BATCH,
    ..SteadyAllocCheck::frames(BATCH as u32)
};
const OBJECTS: usize = 6;
const MESH: u64 = 0x0A11_0301 << 3;

fn quad() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([-0.1, -0.1, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.1, -0.1, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.1, 0.1, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-0.1, 0.1, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn lightmap() -> Arc<Lightmap> {
    Arc::new(
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 2,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "receiver".into(),
                    source_vertices: 4,
                    rect: [0, 0, 4, 2],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3],
                    uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                    indices: vec![0, 1, 2, 0, 2, 3],
                }],
            },
            &[[9_000.0; 3]; 8],
            vec![Coverage::BAKED; 8],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid lightmap"),
    )
}

/// The steady-frame rule on `renderer` drawing the receivers: its settled batch and the six
/// measured ones. Panics when the frame's allocations never settle or grow.
fn measure(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    map: &Lightmap,
    gpu_driven: bool,
) -> SteadyAllocRun {
    // The encode workers' allocations count with the drawing thread's (ADR-2299).
    renderer.set_encode_worker_start_hook(alloc_counter::count_this_thread);
    let (v, i) = quad();
    let built = receiver_mesh(map, 0, &v, &i).expect("the baked mesh");
    let lab = LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 0.0, 2.0],
        target: [0.0; 3],
        fov_y: std::f32::consts::FRAC_PI_3,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices: v.clone(),
            indices: i.clone(),
            ..LabMesh::unskinned()
        }],
    };
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let meshes: Vec<FrameMesh<'_>> = (0..OBJECTS)
        .map(|n| FrameMesh {
            mesh_key: MESH,
            object_key: 0x0B1E_C901 + n as u64,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [n as f32 * 0.25 - 0.6, 0.0, 0.0, 1.0],
            ],
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..template
        })
        .collect();
    let scene = FrameScene {
        quality: FrameQuality {
            gpu_driven,
            ..base.quality
        },
        meshes,
        ..base.clone()
    };
    // The frame alone, without the pixel readback of `render_offscreen`, and each frame waits for
    // the device (ADR-5718). The readback maps a buffer right after its copy is submitted, and
    // wgpu allocates one list more when that submission is still in flight than when the device
    // has already finished it: one allocation a frame that comes and goes with the machine's load
    // (a batch 1 to 8 lower, traced to `LifetimeTracker::map` under `read_rgba8`). And wgpu's
    // Vulkan backend pools its command encoders, each with its own framebuffer cache that grows the
    // first time that encoder records a given pass; drained, every frame starts from the same
    // pool (undrained, a batch took 3 to 10 more now and then on an RTX 5090 with a loaded CPU,
    // traced to `wgpu_hal::vulkan::CommandEncoder::make_framebuffer`).
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .begin_offscreen(device, &scene, wgpu::TextureFormat::Rgba8Unorm)
            .expect("frame");
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    // Before the batches are counted, wgpu's usage-scope pool is brought to the size a frame can
    // need: one scope per thread that finishes a segment (ADR-5718). Left alone it grows by 6
    // allocations the first time one more of those threads is inside a pass at the same moment,
    // in a frame the machine's load picks.
    let fill = |renderer: &mut FrameRenderer| {
        if !device_pools::fill_usage_scope_pool(&device.device, renderer.encode_threads() + 1) {
            eprintln!("the usage-scope pool was not filled: its growth can land in a counted batch");
        }
    };
    match RULE.run_warmed(renderer, alloc_counter::counted_allocs, draw, |_| false, fill) {
        Ok(run) => run,
        Err(err) => panic!("gpu_driven {gpu_driven}: {err}"),
    }
}

fn receivers() -> Vec<(u64, u32)> {
    (0..OBJECTS).map(|n| (0x0B1E_C901 + n as u64, 0)).collect()
}

#[test]
fn receivers_do_not_grow_a_steady_frame_and_a_lightmap_nothing_receives_costs_nothing() {
    alloc_counter::count_this_thread();
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    // Every submit of this thread waits for the device before it returns (ADR-5718): the frame
    // maps buffers right after its submit (the upload ring's chunk, its readbacks), and wgpu
    // allocates a list for the first of them while that submission is in flight and none once
    // the device has finished it, which on a loaded machine comes and goes from frame to frame.
    device.wait_after_submit_on_this_thread(true);
    let map = lightmap();
    let settings = LightmapSettings {
        enabled: true,
        ..LightmapSettings::default()
    };
    let paths: &[bool] = if IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        &[false, true]
    } else {
        &[false]
    };
    for &gpu_driven in paths {
        let plain = measure(&device, &mut FrameRenderer::new(), &map, gpu_driven);

        let mut unused = FrameRenderer::new();
        unused.set_lightmap(Some(map.clone()), settings);
        let unused = measure(&device, &mut unused, &map, gpu_driven);

        let mut lit = FrameRenderer::new();
        lit.set_lightmap(Some(map.clone()), settings);
        lit.set_lightmap_receivers(&receivers());
        // Steady receiver frames do not grow: `measure` holds the rule.
        let lit_run = measure(&device, &mut lit, &map, gpu_driven);
        assert_eq!(lit.lightmap_forward_stats().drawn, OBJECTS);

        eprintln!("gpu_driven {gpu_driven}: plain {plain}\n  unused {unused}\n  receivers {lit_run}");
        assert!(
            unused.steady <= plain.steady + BATCH,
            "gpu_driven {gpu_driven}: a lightmap nothing receives allocates: {unused} vs {plain}"
        );
    }
}
