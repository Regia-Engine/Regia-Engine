// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady scattering frame (ADR-2049): once its targets and groups exist, a
//! steady frame grows nothing, and it stays close to the same frame without the blur.
//!
//! Counted on the frame's threads together, the drawing thread and the renderer's encode workers,
//! under the shared steady-frame rule (`regia_test_utils::alloc_counter`, ADR-5718). This test
//! counted the drawing thread alone and compared three batches with the first; on a loaded machine
//! the drawing thread finishes an encode segment no worker has started yet (ADR-2299), a batch
//! then held that segment's allocations, and the merge gate flagged the test five times
//! ("failed beside other binaries, passed alone"). The frame did the same work each time.
//!
//! Its own test binary, one test: it installs the counting global allocator, and a second test
//! running beside it would change how much the frame's background work allocates here.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SubsurfaceFactors, SubsurfaceProfile,
    box_mesh, translate_mesh,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck};
use regia_test_utils::device_pools;

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

const W: u32 = 160;
const H: u32 = 96;
const BATCH: u64 = 8;
/// The shared rule for batches of 8 frames: 40 warm frames, settled when two batches agree
/// within one allocation per frame, then 6 measured batches, none more than 4 allocations per
/// frame over the settled one (the slack this test always had), and no lone batch forgiven.
const RULE: SteadyAllocCheck = SteadyAllocCheck::frames(BATCH as u32);

/// The most the blur may add per frame, on the drawing thread and the encode workers together:
/// two render passes and a few clones, nothing per draw or per pixel. Measured 11 on lavapipe (8
/// on the drawing thread, 3 on the workers). The bound was 32 on the drawing thread alone; it
/// stays 32 on the total, which now also holds the workers' share.
const SCATTER_PER_FRAME: u64 = 32;

fn scene() -> LabScene {
    let (gv, gi) = box_mesh([20.0, 0.05, 20.0]);
    let (bv, bi) = box_mesh([0.5, 0.5, 0.5]);
    let mut meshes = vec![LabMesh {
        vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
        indices: gi,
        material: 0,
        ..LabMesh::unskinned()
    }];
    for i in -4..=4 {
        meshes.push(LabMesh {
            vertices: translate_mesh(&bv, [i as f32 * 1.5, 1.0, 0.0]),
            indices: bi.clone(),
            material: 0,
            ..LabMesh::unskinned()
        });
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 8.0, 8.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 20000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            subsurface: SubsurfaceFactors {
                radius_mm: 300.0,
                ..SubsurfaceFactors::new(SubsurfaceProfile::Skin)
            },
            ..LabMaterial::default()
        }],
        meshes,
    }
}

#[test]
fn a_steady_scattering_frame_allocates_the_same_every_frame() {
    alloc_counter::count_this_thread();
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    // Every submit of this thread waits for the device before it returns (ADR-5718): the frame
    // maps buffers right after its submit (the upload ring's chunk, its readbacks), and wgpu
    // allocates a list for the first of them while that submission is in flight and none once
    // the device has finished it, which on a loaded machine comes and goes from frame to frame.
    device.wait_after_submit_on_this_thread(true);
    let lab = scene();
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
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
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut last = [0u64; 2];
    for (slot, scatter) in [false, true].into_iter().enumerate() {
        quality.subsurface_scattering = scatter;
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        let mut renderer = FrameRenderer::new();
        // The encode workers' allocations count with the drawing thread's (ADR-2299).
        renderer.set_encode_worker_start_hook(alloc_counter::count_this_thread);
        let draw = |renderer: &mut FrameRenderer| {
            renderer
                .render_into(&device, &frame, FrameTarget::full(&view, format, W, H))
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
        let run = match RULE.run_warmed(
            &mut renderer,
            alloc_counter::counted_allocs,
            draw,
            |_| false,
            fill,
        ) {
            Ok(run) => run,
            Err(err) => panic!("scatter {scatter}: {err}"),
        };
        println!("steady frame, scatter {scatter}, heap allocations per batch of {BATCH} frames: {run}");
        assert_eq!(renderer.subsurface_stats().reason.is_none(), scatter);
        last[slot] = run.steady;
    }
    println!(
        "the blur adds {} allocations per frame on the frame's threads",
        (last[1] as f64 - last[0] as f64) / BATCH as f64
    );
    // Two render passes and a few clones per frame, nothing per draw or per pixel.
    assert!(
        last[1] < last[0] + SCATTER_PER_FRAME * BATCH,
        "the blur allocates too much per frame: {last:?}"
    );
}
