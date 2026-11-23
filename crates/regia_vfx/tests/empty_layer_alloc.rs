// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a frame with an empty VFX layer (ADR-2585): once the layer's warm frames
//! are done, a steady frame with the layer and nothing in it allocates what the same frame
//! without the layer does. The DO27 chase measured +181 allocations per frame for the empty
//! layer (`docs/perf-log.md`, `vfx-layer`): its simulate pass, the HDR copy for it and its decal
//! and HDR passes ran every frame.
//!
//! Its own test binary, one GPU test: it installs a counting global allocator (the pattern of
//! `regia_render/tests/oit_frame_alloc.rs`, ADR-2299). Fixed frame counts, no wall clock.

use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice,
};
use regia_vfx::gpu::LAYER_WARM_FRAMES;
use regia_vfx::{VfxGpu, VfxProfile, translucent_layer};

use regia_test_utils::alloc_counter::{self, CountingAlloc, count_this_thread};

// The shared counting allocator (ADR-3291): the requests of the threads that called
// `count_this_thread`: the drawing thread, and each encode worker
// (`FrameRenderer::set_encode_worker_start_hook`, ADR-2299).
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// Allocations of the counted threads so far (a frame's workers are done when `render_into`
/// returns).
fn allocs() -> u64 {
    alloc_counter::counted_allocs()
}

const W: u32 = 96;
const H: u32 = 64;
/// Frames per counted batch.
const BATCH: u64 = 8;
/// Frames drawn before counting: past the layer's warm frames and the first idle frame.
const WARM_FRAMES: u32 = LAYER_WARM_FRAMES + 24;
/// Batches counted; the least of them is the frame's steady count (a lone background event
/// lands in one batch, not in all).
const MEASURED: usize = 6;
/// The most the frame with the empty layer may allocate over the frame without it, per batch:
/// one allocation per frame. The base allocated tens per frame more (a compute pass, two render
/// passes and an HDR copy recorded for nothing).
const SLACK: u64 = BATCH;

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 256,
        decal_capacity: 16,
        sort: true,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 256,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

fn ground() -> Vec<LabVertex> {
    let half = 30.0;
    [[-half, -half], [half, -half], [half, half], [-half, half]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

const GROUND_INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

/// The least heap allocations of `BATCH` steady frames of a fresh renderer, with an empty layer
/// or without one.
fn steady_batch(device: &RenderDevice, with_layer: bool) -> (u64, Vec<u64>) {
    let vertices = ground();
    let material = LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    // Single sample, HDR, post on: the frame shape where the layer drew into the HDR image.
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 15.0,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_encode_worker_start_hook(count_this_thread);
    if with_layer {
        let gpu = VfxGpu::new(device, profile()).expect("vfx gpu");
        renderer.set_translucent_layers(vec![translucent_layer(Arc::new(Mutex::new(gpu)))]);
    }
    let mut draw = || {
        let frame = FrameScene {
            width: W,
            height: H,
            eye: [0.0, 3.0, 8.0],
            target: [0.0, 0.6, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 60.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 15.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: Some(&post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 100_000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &GROUND_INDICES,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        renderer.render_offscreen(device, &frame).expect("frame");
    };
    for _ in 0..WARM_FRAMES {
        draw();
    }
    let measured: Vec<u64> = (0..MEASURED)
        .map(|_| {
            let before = allocs();
            for _ in 0..BATCH {
                draw();
            }
            allocs() - before
        })
        .collect();
    let least = measured.iter().copied().min().unwrap_or(u64::MAX);
    (least, measured)
}

#[test]
fn an_empty_layer_adds_no_heap_allocation_per_frame() {
    count_this_thread();
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let (bare, bare_batches) = steady_batch(&device, false);
    let (empty, empty_batches) = steady_batch(&device, true);
    println!(
        "heap allocations per batch of {BATCH} frames: no layer {bare_batches:?}, empty layer \
         {empty_batches:?}"
    );
    assert!(
        empty <= bare + SLACK,
        "an empty VFX layer adds {} heap allocations per batch of {BATCH} frames (no layer \
         {bare}, empty layer {empty})",
        empty.saturating_sub(bare)
    );
}
