// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady translucent frame (ADR-2065): once its targets and groups exist, a
//! steady frame grows nothing, and it stays close to the same frame sorted.
//!
//! Its own test binary, one GPU test: it installs a counting global allocator, and a second GPU
//! test running beside it would change how much the frame's background work allocates here. The
//! other test is the growth rule on fixed numbers (CPU only, nothing to count on the frame's
//! thread).

use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh,
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
            material: 1,
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
        // The shadow passes' own cadence moves a frame's count by tens, with the knob off too.
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 20000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial::default(),
            LabMaterial {
                base_color: [0.8, 0.4, 0.2, 0.5],
                alpha_mode: ALPHA_BLEND,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

// The steady-frame rule of `hwrt_frame_alloc.rs` (ADR-2240): warm, then settle, then measure.
// ADR-2299: the frame's encode workers are counted with the drawing thread, so a lone frame's
// extra work (+48..+190 once: a segment the drawing thread finished because no worker had started
// it) is gone, and every measured batch must be within the slack.

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
/// The most a measured batch may exceed the settled one: 4 allocations per frame, the slack this
/// test always had (`[1633, 1632, ...]` on a loaded machine); a target rebuilt every frame adds
/// hundreds.
const GROWTH: u64 = 4 * BATCH;

/// The most the sorted frame may allocate per frame over the unsorted one. Until ADR-2299 the
/// budget was 32 on the drawing thread alone, which then held the sorted passes' wgpu `finish`
/// only when it finished a segment for a late worker (so the drawing-thread count moved by a
/// segment from batch to batch under load). Counted with the encode workers, the difference is
/// exact: 48 per frame on lavapipe in every run (3016 - 2632 per batch). The scene has 9
/// translucent draws, so one allocation per draw (+9) is over the budget.
const SORTED_PER_FRAME: u64 = 56;

/// The growth check on `measured` batches against the `steady` one: no batch may exceed
/// `steady + GROWTH`, not even once (ADR-2299: with the encode workers counted, a lone frame's
/// extra work is gone).
fn check_growth(steady: u64, measured: &[u64]) -> Result<(), String> {
    RULE.check(steady, measured)
}

#[test]
fn the_growth_check_tells_noise_from_growth() {
    // Steady.
    assert!(check_growth(1633, &[1633, 1632, 1634, 1633, 1633, 1632]).is_ok());
    // The gate's lone event (+131 in one batch, v0.2.12 candidate) is no longer allowed.
    assert_eq!(
        check_growth(1633, &[1632, 1763, 1633, 1633, 1632, 1633]),
        Err(format!(
            "batches [1] exceed the settled batch (1633) by more than {GROWTH}"
        ))
    );
    // A step that stays up.
    assert!(check_growth(1633, &[1633, 1633, 1633, 1763, 1763, 1763]).is_err());
    // A target rebuilt per frame: hundreds per frame from the first measured batch on.
    assert!(check_growth(1633, &[1633 + 8 * 200, 1633 + 8 * 200]).is_err());
    // One more allocation per frame every batch: +8 per batch, past the slack by the fifth.
    let creeping: Vec<u64> = (1..=6).map(|k| 1633 + 8 * k).collect();
    assert_eq!(
        check_growth(1633, &creeping),
        Err(format!(
            "batches [4, 5] exceed the settled batch (1633) by more than {GROWTH}"
        ))
    );
    // Neither in the last batch.
    assert!(check_growth(1633, &[1633, 1633, 1633, 1633, 1633, 1763]).is_err());
}

#[test]
fn a_steady_translucent_frame_allocates_the_same_every_frame() {
    count_this_thread();
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
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
    for (slot, oit) in [false, true].into_iter().enumerate() {
        quality.order_independent_transparency = oit;
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
            Err(err) => panic!("oit {oit}: a steady frame (batches of {BATCH} frames): {err}"),
        };
        let (warm, settling, measured) = (run.warm_frames, &run.settling, &run.measured);
        println!(
            "steady frame, oit {oit}: {warm} warm frames, settling batches {settling:?}, heap \
             allocations per batch of {BATCH} frames {measured:?}"
        );
        assert_eq!(renderer.oit_stats().reason.is_none(), oit);
        last[slot] = run.steady;
    }
    // Three render passes, three pool lookups and a few clones per frame, nothing per draw or per
    // pixel: the sorted frame pays the same per draw, in its own pass.
    assert!(
        last[1] < last[0] + SORTED_PER_FRAME * BATCH,
        "the pass allocates too much per frame: {last:?}"
    );
}
