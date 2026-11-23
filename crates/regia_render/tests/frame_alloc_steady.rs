// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady frame (ADR-2018's "no per-frame allocation regression").
//!
//! A refracting glass frame in Desktop High (linear HDR target, refraction copy, SSAO) is drawn
//! until warm and settled, then six batches of frames are counted with the counting global
//! allocator (the drawing thread and the renderer's encode workers, ADR-2299: counting the
//! drawing thread alone moved a segment's allocations into a batch whenever the drawing thread
//! finished a segment no worker had started yet, the old +77/+150 flake under load). A measured
//! batch must not grow by a frame's worth over the settled one: a target that is rebuilt, or a
//! pool that grows, adds allocations on every frame. The per-frame count is printed
//! (`--nocapture`) so a run at two commits compares the frame's allocations.
//!
//! The rule is the shared one (`regia_test_utils::alloc_counter`, ADR-5718) with this test's own
//! slack. It used to warm 12 frames and compare two batches with the first; wgpu's pools were
//! then still filling, and under load a later batch took 6 more of the 7 allowed.
//!
//! Its own test binary: it installs the counting global allocator.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial,
    LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck};
use regia_test_utils::device_pools;

// The shared counting allocator (ADR-3291): the requests of the threads that called
// `count_this_thread`: the drawing thread, and each encode worker
// (`FrameRenderer::set_encode_worker_start_hook`, ADR-2299).
#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

const W: u32 = 160;
const H: u32 = 96;
const BATCH: u64 = 8;
/// The shared rule with this test's own slack: a measured batch must stay under the settled one
/// plus one allocation per frame (the frame's own lists vary by an allocation now and then).
const RULE: SteadyAllocCheck = SteadyAllocCheck {
    growth_slack: BATCH - 1,
    ..SteadyAllocCheck::frames(BATCH as u32)
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A floor with bright bars under a refracting glass pane, Desktop High (SSAO on, linear HDR
/// target, screen-space refraction on, TAAU off): the frame takes the HDR target, the
/// refraction copy and the SSAO targets every frame.
fn glass_scene() -> (LabScene, PostSettings, FrameQuality) {
    let slab = |half: [f32; 3], model: Mat4, material: usize| {
        let (vertices, indices) = regia_render::box_mesh(half);
        LabMesh {
            vertices,
            indices,
            material,
            model: model.to_cols_array_2d(),
            ..LabMesh::unskinned()
        }
    };
    let floor = LabMaterial {
        base_color: [0.4, 0.4, 0.4, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let bar = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 0.9, 0.8],
        emissive_intensity: 3000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        transmission_thickness: 0.05,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let mut meshes = vec![slab(
        [10.0, 0.005, 10.0],
        Mat4::from_translation(Vec3::new(0.0, -1.005, 0.0)),
        0,
    )];
    for i in -6..=6 {
        meshes.push(slab(
            [0.125, 0.2, 10.0],
            Mat4::from_translation(Vec3::new(i as f32 * 0.7, -0.8, 0.0)),
            1,
        ));
    }
    meshes.push(slab([1.2, 0.005, 1.2], Mat4::from_rotation_z(0.3), 2));
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 4.0, 2.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 20000.0),
        local_lights: Vec::new(),
        materials: vec![floor, bar, glass],
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.screen_space_refraction = true;
    (scene, post, quality)
}

#[test]
fn a_steady_glass_frame_allocates_the_same_every_frame() {
    alloc_counter::count_this_thread();
    let device = device();
    // Every submit of this thread waits for the device before it returns (ADR-5718): the frame
    // maps buffers right after its submit (the upload ring's chunk, its readbacks), and wgpu
    // allocates a list for the first of them while that submission is in flight and none once
    // the device has finished it, which on a loaded machine comes and goes from frame to frame.
    device.wait_after_submit_on_this_thread(true);
    let (scene, post, quality) = glass_scene();
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("steady target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = FrameRenderer::new();
    renderer.set_encode_worker_start_hook(alloc_counter::count_this_thread);
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .render_into(&device, &frame, FrameTarget::full(&view, format, W, H))
            .expect("frame");
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    // A target rebuilt every frame, or a pool that grows, adds several allocations per frame;
    // the frame's own lists vary by an allocation now and then (measured: 3168, 3169, 3168).
    // Before the batches are counted, wgpu's usage-scope pool is brought to the size a frame can
    // need: one scope per thread that finishes a segment (ADR-5718). Left alone it grows by 6
    // allocations the first time one more of those threads is inside a pass at the same moment,
    // in a frame the machine's load picks.
    let fill = |renderer: &mut FrameRenderer| {
        if !device_pools::fill_usage_scope_pool(&device.device, renderer.encode_threads() + 1) {
            eprintln!("the usage-scope pool was not filled: its growth can land in a counted batch");
        }
    };
    let run = match RULE.run_warmed(
        &mut renderer,
        alloc_counter::counted_allocs,
        draw,
        |_| false,
        fill,
    ) {
        Ok(run) => run,
        Err(err) => panic!("a steady glass frame: {err}"),
    };
    println!(
        "steady glass frame, heap allocations per batch of {BATCH} frames (per frame {:.1}): {run}",
        run.steady as f64 / BATCH as f64
    );
}
