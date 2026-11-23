// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady traced frame (ADR-2035): once the traced scene is built, a steady
//! frame grows nothing, whichever tracer runs on the adapter's default device.
//!
//! Its own test binary, one GPU test: it installs a counting global allocator, and a second GPU
//! test running beside it would change how much the frame's background work allocates here (the
//! other test only checks the growth rule on fixed numbers).
//!
//! The counter counts the frame's own threads: the drawing thread and the renderer's encode
//! workers (wgpu's and the renderer's calls made from them; the driver's C allocations and the
//! task pool's threads are not counted). The frame still does start-up work for a while, and some
//! of it is paced by other threads: the probes do extra work until their traced scene has been unchanged for a few frames
//! (frames 1 to 9 allocate about 47 more each on lavapipe), and a probe brick baking on the task
//! pool is installed by whichever frame comes after it lands. With only 12 warm frames the v0.2.11
//! gate measured `Compute: [1798, 1888, 1888]` on an RTX 5090: a step during the first batch, not
//! growth. So the frame is warmed for `WARM_FRAMES` and until no brick is baking, then until two
//! batches agree, and only then measured.
//!
//! Until ADR-2299 the counter was the drawing thread's alone, and one frame in a few hundred
//! allocated about 48 more (once 189) than its neighbors, on lavapipe and on the 5090 (the v0.2.11
//! gate's `Screen: [1707, 1755, 1707]`), so the check allowed one batch over the slack. A
//! per-allocation backtrace showed each one: `FinishWorkers::join` finishes on the drawing thread a
//! segment no encode worker has started yet (a descheduled worker), and that segment's `finish`
//! allocations moved to the drawing thread for one frame. The frame allocated no more; the work
//! moved. With the workers counted, every batch must be within the slack.

use regia_render::hwrt::RtShadowRequest;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh,
};

use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck, count_this_thread};

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

const W: u32 = 160;
const H: u32 = 96;
const BATCH: u64 = 8;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

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
        materials: vec![LabMaterial::default()],
        meshes,
    }
}

/// The steady-frame rule (`SteadyAllocCheck`, ADR-3291) with this test's numbers: 40 warm
/// frames, then more while probe bricks are still baking, up to 2000 (a fixed frame count, no
/// clock); settled when two consecutive batches agree within 1 allocation per frame, at most 12
/// batches; 6 measured batches within `GROWTH`; no lone batch (ADR-2299).
const RULE: SteadyAllocCheck = SteadyAllocCheck {
    batch_frames: BATCH as u32,
    warm_frames: 40,
    max_warm_frames: 2000,
    settle_slack: Some(BATCH),
    max_settle_batches: 12,
    measured_batches: 6,
    growth_slack: GROWTH,
    lone_batch: false,
};
/// The most a measured batch may exceed the settled one: 4 allocations per frame. Steady batches
/// on a quiet frame agree within about 4 per batch (2056, 2071, 2063 on a loaded machine; 1730,
/// 1732, 1728 on an RTX 5090); a target that starts being rebuilt every frame adds hundreds.
const GROWTH: u64 = 4 * BATCH;

/// The growth check on `measured` batches against the `steady` one: no batch may exceed
/// `steady + GROWTH`, not even once (ADR-2299: with the encode workers counted, a lone frame's
/// extra work is gone).
fn check_growth(steady: u64, measured: &[u64]) -> Result<(), String> {
    RULE.check(steady, measured)
}

#[test]
fn the_growth_check_tells_noise_from_growth() {
    // Steady, with the noise measured on real drivers.
    assert!(check_growth(1888, &[1888, 1890, 1887, 1888, 1889, 1888]).is_ok());
    assert!(check_growth(1707, &[1708, 1708, 1707, 1706, 1708, 1739]).is_ok());
    // One frame's lone extra work (+48) is no longer allowed (ADR-2299).
    assert_eq!(
        check_growth(1712, &[1710, 1712, 1760, 1712, 1713, 1712]),
        Err(format!(
            "batches [2] exceed the settled batch (1712) by more than {GROWTH}"
        ))
    );
    // Neither in the last batch.
    assert!(check_growth(1712, &[1712, 1712, 1712, 1712, 1712, 1760]).is_err());
    // A step that stays up.
    assert!(check_growth(1712, &[1712, 1712, 1712, 1760, 1760, 1760]).is_err());
    // A target rebuilt per frame from the first measured batch on: hundreds per frame.
    assert!(check_growth(1707, &[1707 + 8 * 200, 1707 + 8 * 200]).is_err());
    // One more allocation per frame every batch: +8 per batch, past the slack by the fifth.
    let creeping: Vec<u64> = (1..=6).map(|k| 1707 + 8 * k).collect();
    assert_eq!(
        check_growth(1707, &creeping),
        Err(format!(
            "batches [4, 5] exceed the settled batch (1707) by more than {GROWTH}"
        ))
    );
    // Ten more allocations per frame every batch: over from the first batch.
    let growing: Vec<u64> = (1..=6).map(|k| 1707 + 80 * k).collect();
    assert!(check_growth(1707, &growing).is_err());
    // A step down (the probes going idle) is not growth.
    assert!(check_growth(1712, &[1712, 1712, 1672, 1672, 1672, 1672]).is_ok());
}

#[test]
fn a_steady_traced_frame_allocates_the_same_every_frame() {
    count_this_thread();
    let device = device();
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
    for request in [
        RtShadowRequest::Off,
        RtShadowRequest::Screen,
        RtShadowRequest::Compute,
    ] {
        quality.sun_shadow_trace = request;
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        let mut renderer = FrameRenderer::new();
        renderer.set_encode_worker_start_hook(count_this_thread);
        let draw = |renderer: &mut FrameRenderer| {
            renderer
                .render_into(&device, &frame, FrameTarget::full(&view, format, W, H))
                .expect("frame");
            let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
        };
        // `RULE`: warm (pipelines, pools, then every probe brick baking on the task pool
        // installed, bounded by a frame count), settle, then measure.
        let run = match RULE.run(&mut renderer, allocs, draw, |r| r.ddgi_pending_bakes() > 0) {
            Ok(run) => run,
            Err(err) => panic!("{request:?}: a steady frame (batches of {BATCH} frames): {err}"),
        };
        let (warm, settling, measured) = (run.warm_frames, &run.settling, &run.measured);
        println!(
            "steady frame {request:?}: {warm} warm frames (probes active {}), settling batches \
             {settling:?}, heap allocations per batch of {BATCH} frames {measured:?}",
            renderer.ddgi_active()
        );
        if request != RtShadowRequest::Off {
            // A traced scene rebuilt per frame.
            assert_eq!(renderer.sun_trace_report().scene_builds, 1, "{request:?}");
        }
    }
}
