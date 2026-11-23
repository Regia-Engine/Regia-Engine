// SPDX-License-Identifier: MPL-2.0
//! Steady frames with the shadow casters' two-phase cull on (ADR-2072): after the first frames,
//! a culled frame creates no pipeline, bind group, buffer or texture (the hitch gate's rule,
//! `specs/27` § 2.4), and its heap allocations do not grow from batch to batch. The off renderer's
//! count is printed beside it (`--nocapture`); the cull's extra passes cost what wgpu allocates
//! for a pass, nothing of the engine's own.
//!
//! Its own test binary: it installs the counting global allocator. The count covers the frame's
//! threads, the drawing thread and the encode workers: encoders finish on the workers, and
//! `FinishWorkers::join` finishes on the calling thread any segment a worker has not started, so a
//! count of the calling thread alone jumps by that segment's allocations whenever the scheduler
//! lets the join get there first.
//!
//! Under the shared steady-frame rule (`regia_test_utils::alloc_counter`, ADR-5718): 40 warm
//! frames and two batches that agree before anything is measured. The test counted every thread
//! of the process from the fifth frame on and compared two batches with the first; on a loaded
//! machine a batch then took 4 to 10 more now and then (wgpu's command-buffer pools still filling:
//! they refill 16 at a time, later when the device runs behind) and the test failed once in 7
//! loaded runs. Settled, six batches stay within 3 of each other here.

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    IndirectStrategy, LabMaterial, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    box_mesh, frame_path_counters,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck, SteadyAllocRun};
use regia_test_utils::device_pools;

// The shared counting allocator (ADR-3291): every thread's requests.
#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

const BATCH: u64 = 8;

/// The shared rule with this test's own slack: a list or a pool that grows adds allocations
/// every frame, so no measured batch may exceed the settled one by a whole allocation per frame.
const RULE: SteadyAllocCheck = SteadyAllocCheck {
    growth_slack: BATCH,
    ..SteadyAllocCheck::frames(BATCH as u32)
};

struct Geometry {
    ground: (Vec<LabVertex>, Vec<u32>),
    slab: (Vec<LabVertex>, Vec<u32>),
    cube: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
}

impl Geometry {
    fn mesh<'g>(
        &'g self,
        key: u64,
        mesh_key: u64,
        geometry: &'g (Vec<LabVertex>, Vec<u32>),
        at: Vec3,
    ) -> FrameMesh<'g> {
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 1,
            object_key: key,
            vertices: geometry.0.as_slice(),
            indices: geometry.1.as_slice(),
            material: &self.material,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    fn scene(&self) -> FrameScene<'_> {
        let mut meshes = vec![
            self.mesh(1, 1, &self.ground, Vec3::new(0.0, -0.05, 0.0)),
            self.mesh(2, 2, &self.slab, Vec3::new(0.0, 3.0, 0.0)),
        ];
        for i in 0..256usize {
            let (col, row) = ((i % 16) as f32, (i / 16) as f32);
            let at = Vec3::new(-2.4 + col * 0.32, 0.25, -2.4 + row * 0.32);
            meshes.push(self.mesh(i as u64 + 10, 3, &self.cube, at));
        }
        let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        FrameScene {
            width: 96,
            height: 64,
            eye: [0.0, 2.5, 7.5],
            target: [0.0, 0.5, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 40.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.5,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([-0.15, -1.0, -0.1], [1.0, 1.0, 1.0], 3.0),
            local_lights: &[],
            quality: FrameQuality {
                csm_cascade_count: 2,
                csm_resolution: 512,
                // Paging off: every cascade is redrawn (and culled) every frame.
                paged_sun_shadows: false,
                taau: false,
                ..base
            },
            meshes,
        }
    }
}

/// `(the steady-frame run, frame-path creates over the 24 frames after the first four)`. Panics
/// when the frame's allocations never settle or grow.
fn run(device: &RenderDevice, on: bool) -> (SteadyAllocRun, regia_render::FramePathCounters) {
    let geometry = Geometry {
        ground: box_mesh([8.0, 0.05, 8.0]),
        slab: box_mesh([3.2, 0.05, 3.2]),
        cube: box_mesh([0.1, 0.1, 0.1]),
        material: LabMaterial::default(),
    };
    let mut renderer = FrameRenderer::new();
    // The encode workers' allocations count with the drawing thread's (ADR-2299).
    renderer.set_encode_worker_start_hook(alloc_counter::count_this_thread);
    renderer.set_shadow_occlusion(on);
    renderer.set_shadow_occlusion_min_casters(Some(64));
    let scene = geometry.scene();
    for _ in 0..4 {
        renderer.render_offscreen(device, &scene).expect("warm frame");
    }
    assert_eq!(renderer.shadow_occlusion_stats().active, on);
    // The frame alone, without the pixel readback of `render_offscreen` (the readback's buffer map
    // allocates one list more when its copy is still in flight than when the device has already
    // finished it, which comes and goes with the machine's load), and each frame waits for the
    // device, so every frame starts from the same pool of wgpu command encoders (each pooled
    // encoder grows its own caches the first time it records a given pass, ADR-5718).
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .begin_offscreen(device, &scene, wgpu::TextureFormat::Rgba8Unorm)
            .expect("steady frame");
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    // The hitch gate's window, as before: the 24 frames after the first four create nothing.
    let before = frame_path_counters();
    for _ in 0..3 * BATCH {
        draw(&mut renderer);
    }
    let after = frame_path_counters();
    let label = if on { "on" } else { "off" };
    // Before the batches are counted, wgpu's usage-scope pool is brought to the size a frame can
    // need: one scope per thread that finishes a segment (ADR-5718). Left alone it grows by 6
    // allocations the first time one more of those threads is inside a pass at the same moment,
    // in a frame the machine's load picks.
    let fill = |renderer: &mut FrameRenderer| {
        if !device_pools::fill_usage_scope_pool(&device.device, renderer.encode_threads() + 1) {
            eprintln!("the usage-scope pool was not filled: its growth can land in a counted batch");
        }
    };
    let steady = match RULE.run_warmed(
        &mut renderer,
        alloc_counter::counted_allocs,
        draw,
        |_| false,
        fill,
    ) {
        Ok(run) => run,
        Err(err) => panic!("{label}: {err}"),
    };
    let created = regia_render::FramePathCounters {
        pipelines: after.pipelines - before.pipelines,
        bind_groups: after.bind_groups - before.bind_groups,
        buffers: after.buffers - before.buffers,
        textures: after.textures - before.textures,
        ..after
    };
    (steady, created)
}

#[test]
fn steady_culled_frames_create_nothing_and_do_not_grow() {
    alloc_counter::count_this_thread();
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    // Every submit of this thread waits for the device before it returns (ADR-5718): the frame
    // maps buffers right after its submit (the upload ring's chunk, its readbacks), and wgpu
    // allocates a list for the first of them while that submission is in flight and none once
    // the device has finished it, which on a loaded machine comes and goes from frame to frame.
    device.wait_after_submit_on_this_thread(true);
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip: the device draws no GPU objects");
        return;
    }
    let (off, off_created) = run(&device, false);
    let (on, on_created) = run(&device, true);
    eprintln!(
        "allocations per frame: off {:.3}, on {:.3}\n  off {off}\n  on {on}",
        off.steady as f64 / BATCH as f64,
        on.steady as f64 / BATCH as f64
    );
    for (label, created) in [("off", off_created), ("on", on_created)] {
        assert_eq!(created.pipelines, 0, "{label}: steady frames built pipelines");
        assert_eq!(created.bind_groups, 0, "{label}: steady frames made bind groups");
        assert_eq!(created.buffers, 0, "{label}: steady frames made buffers");
        assert_eq!(created.textures, 0, "{label}: steady frames made textures");
    }
    // That the allocations do not grow is `RULE`, held by `run` for both renderers.
}
