// SPDX-License-Identifier: MPL-2.0
//! What the lens effects pass (ADR-1863) costs a steady frame, by counters (ADR-2482): heap
//! allocations per frame against the same renderer without the pass, the allocations of the
//! pass's own recording, the GPU memory it creates, and its render passes. No wall clock.
//!
//! Its own test binary, one GPU test at a time: it installs the counting global allocator
//! (`regia_test_utils::alloc_counter`). The per-frame batches count the frame's threads, the
//! drawing thread and the encode workers where the finishes land, under the shared steady-frame
//! rule (ADR-5718): 40 warm frames and two batches that agree before anything is measured. The
//! batches used to count every thread of the process from the ninth frame on and compare two
//! batches with the first; under load a batch then took 6 more of the 8 allowed (wgpu's pools
//! still filling), and a second test of this binary counted in. The in-slot marks between which
//! the pass's `record` runs read the calling thread's counter only: the record is synchronous on
//! that thread, while a window over other threads a few hundred microseconds wide also catches an
//! encode worker finishing an earlier segment (measured up to +33 a frame under gate load, and
//! up to +1.6 between two back-to-back marks), which is scheduling, not the pass's cost
//! (ADR-2482 follow-up).
//!
//! The card-costs run of 2026-11-02 measured +125 heap allocations every frame and +65 MiB at
//! 1440p with the cinematic look; `book/agents/feature-costs.md` has the rows.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_render::lens_effects::{LensEffectsBudget, lens_effects_counters, lens_effects_memory};
use regia_render::{
    DirectionalLightUniform, FramePass, FramePassContext, FrameRenderer, FrameTarget, GraphSlot, LabMaterial,
    LabMesh, LabScene, LabVertex, LensEffects, PostSettings, RenderDevice, frame_path_counters,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc, SteadyAllocCheck, SteadyAllocRun};
use regia_test_utils::device_pools;

#[global_allocator]
static A: CountingAlloc = CountingAlloc::new();

/// The calling thread's allocations since the start (the in-slot mark counter).
fn thread_allocs() -> u64 {
    alloc_counter::thread_allocs()
}

static SERIAL_GPU: Mutex<()> = Mutex::new(());

const W: u32 = 320;
const H: u32 = 180;
const FOV_Y: f32 = 0.9;
const BATCH: u64 = 8;
const WARM: u32 = 8;

/// The shared rule with this test's own slack: no measured batch may exceed the settled one by a
/// whole allocation per frame.
const RULE: SteadyAllocCheck = SteadyAllocCheck {
    growth_slack: BATCH,
    ..SteadyAllocCheck::frames(BATCH as u32)
};

/// A small emissive square at centred frame position `c` (x right, y down), 5 m away.
fn light_at(c: [f32; 2]) -> LabMesh {
    let depth = 5.0;
    let half_h = depth * (FOV_Y * 0.5_f32).tan();
    let half_w = half_h * W as f32 / H as f32;
    let (dx, dy) = (4.0 / W as f32, 4.0 / H as f32);
    let (x0, x1) = ((c[0] - dx) * half_w, (c[0] + dx) * half_w);
    let (y0, y1) = (-(c[1] - dy) * half_h, -(c[1] + dy) * half_h);
    let z = -depth;
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x0, y1, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x1, y1, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x1, y0, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x0, y0, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }
}

fn scene(lens: LensEffects) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.lens_effects = lens;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -1.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 500.0,
            ..LabMaterial::default()
        }],
        meshes: vec![light_at([0.5, -0.4]), light_at([-0.3, 0.2])],
    }
}

/// Notes the allocation counter when it records: one before and one after the lens pass at
/// `before_post`, so their difference is what the pass's `record` allocated (the engine's code
/// and wgpu's recording of its passes; not what wgpu does at `finish` and `submit`).
struct Mark {
    name: &'static str,
    at: Arc<AtomicU64>,
}

impl FramePass for Mark {
    fn name(&self) -> &str {
        self.name
    }
    fn slot(&self) -> GraphSlot {
        GraphSlot::BeforePost
    }
    fn record(&mut self, _ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        // The calling thread's count: the pass's record runs on this thread (see the file
        // header), so this window is its record's allocations alone.
        self.at.store(thread_allocs(), Ordering::Relaxed);
        Ok(())
    }
}

struct Measured {
    /// Heap allocations per frame of the settled batch of [`BATCH`] frames, on the frame's threads.
    per_frame: f64,
    /// The steady-frame run the settled batch is from.
    run: SteadyAllocRun,
    /// Allocations between the two marks on the calling thread, per frame (over the last of the
    /// three batches after the warm): the pass's `record` alone (see the file header).
    in_slot: f64,
    /// Texture and buffer bytes created from the renderer's first frame through the warm.
    texture_bytes: u64,
    buffer_bytes: u64,
    /// Creates over the measured frames (must be 0).
    steady_creates: u64,
}

fn measure(device: &RenderDevice, lens: LensEffects, with_pass: bool) -> Measured {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lens alloc target"),
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
    let lab = scene(lens);
    let frame = lab.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    // The encode workers' allocations count with the drawing thread's (ADR-2299).
    renderer.set_encode_worker_start_hook(alloc_counter::count_this_thread);
    let (before, after) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    renderer
        .register_pass(Box::new(Mark {
            name: "mark_before",
            at: Arc::clone(&before),
        }))
        .expect("register mark");
    if with_pass {
        assert!(renderer.enable_lens_effects());
    }
    renderer
        .register_pass(Box::new(Mark {
            name: "mark_after",
            at: Arc::clone(&after),
        }))
        .expect("register mark");
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .render_into(device, &frame, FrameTarget::full(&view, format, W, H))
            .expect("frame");
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let start = frame_path_counters();
    for _ in 0..WARM {
        draw(&mut renderer);
    }
    let warm = frame_path_counters().since(start);
    // The creates window and the in-slot marks, as before: three batches after the warm.
    let mut in_slot = 0;
    for _ in 0..3 {
        in_slot = 0;
        for _ in 0..BATCH {
            draw(&mut renderer);
            in_slot += after.load(Ordering::Relaxed) - before.load(Ordering::Relaxed);
        }
    }
    let steady = frame_path_counters().since(start).since(warm);
    let label = if with_pass { "lens" } else { "plain" };
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
        Err(err) => panic!("{label}: {err}"),
    };
    Measured {
        per_frame: run.steady as f64 / BATCH as f64,
        run,
        in_slot: in_slot as f64 / BATCH as f64,
        texture_bytes: warm.texture_bytes,
        buffer_bytes: warm.buffer_bytes,
        steady_creates: steady.pipelines + steady.bind_groups + steady.buffers + steady.textures,
    }
}

/// Generous allocations per render pass or copy (begin, a few state calls and one draw, end, then
/// its share of `finish` and `submit`) on adapters other than the reference: lavapipe takes 12
/// to 16 (`oit_frame_alloc.rs`: 48 for three passes).
const WGPU_PER_PASS: f64 = 24.0;

/// Allocations of the pass's record per frame on the calling thread (lavapipe reference,
/// measured 44.0 = 352 over the 8-frame batch, exact on every run; slack under one per pass).
/// Other threads' work is excluded: the window is the synchronous record alone (file header).
const RECORD_LAVAPIPE: f64 = 54.0;

/// Allocations a lens frame adds per frame on lavapipe (measured +121).
const FRAME_LAVAPIPE: f64 = 125.0;

fn lavapipe(device: &RenderDevice) -> bool {
    device
        .adapter
        .get_info()
        .name
        .to_ascii_lowercase()
        .contains("llvmpipe")
}

#[test]
fn a_steady_lens_frame_allocates_only_what_its_passes_cost_in_wgpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    alloc_counter::count_this_thread();
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    // Every submit of this thread waits for the device before it returns (ADR-5718): the frame
    // maps buffers right after its submit (the upload ring's chunk, its readbacks), and wgpu
    // allocates a list for the first of them while that submission is in flight and none once
    // the device has finished it, which on a loaded machine comes and goes from frame to frame.
    device.wait_after_submit_on_this_thread(true);
    let plain = measure(&device, LensEffects::CINEMATIC, false);
    let lens = measure(&device, LensEffects::CINEMATIC, true);
    let budget = LensEffectsBudget::of(&LensEffects::CINEMATIC, W, H);
    let delta = lens.per_frame - plain.per_frame;
    eprintln!("plain: {}\nlens: {}", plain.run, lens.run);
    eprintln!(
        "allocations per frame: plain {:?}, lens {:?} (+{delta}); inside the pass's record {} \
         (plain marks {}); render passes {}, copies {}; texture bytes plain {} lens {} \
         (+{}), buffer bytes plain {} lens {} (+{})",
        plain.per_frame,
        lens.per_frame,
        lens.in_slot,
        plain.in_slot,
        budget.render_passes,
        budget.copies,
        plain.texture_bytes,
        lens.texture_bytes,
        lens.texture_bytes - plain.texture_bytes,
        plain.buffer_bytes,
        lens.buffer_bytes,
        lens.buffer_bytes.saturating_sub(plain.buffer_bytes),
    );
    assert_eq!(lens.steady_creates, 0, "steady lens frames created GPU objects");
    assert_eq!(plain.steady_creates, 0, "steady plain frames created GPU objects");
    // That neither frame's allocations grow is `RULE`, held by `measure`.
    // The pass's own code allocates nothing on a steady frame: what its record costs is wgpu's
    // recording of its render passes and copies (`begin_render_pass`, the pass's command list),
    // on the calling thread alone (the global count adds unrelated threads' housekeeping to the
    // window: the v0.2.17 gate saw 83.6 where the record itself costs 44.0). On the lavapipe
    // reference the count is exact for this wgpu: 44 in the record and +121 a frame (the base of
    // ADR-2482 measured 99 and +139: a `Vec` per pass, one for the groups, and a uniform write
    // every frame). Another adapter's backend allocates differently, so there only the per-pass
    // bound applies.
    let passes = f64::from(budget.render_passes + budget.copies);
    let (record_budget, frame_budget) = if lavapipe(&device) {
        (RECORD_LAVAPIPE, FRAME_LAVAPIPE)
    } else {
        (passes * WGPU_PER_PASS, passes * WGPU_PER_PASS)
    };
    assert!(
        lens.in_slot <= record_budget,
        "the pass's record allocated {} per frame for {passes} passes and copies (budget {record_budget})",
        lens.in_slot
    );
    assert!(
        delta <= frame_budget,
        "a lens frame allocates {delta} more per frame than a plain one (budget {frame_budget})"
    );
    assert!(lens_effects_counters().views_recorded.load(Ordering::Relaxed) > 0);
    // GPU memory: exactly what `lens_effects_memory` states for the view (its own images, the
    // dirt mask and the frame's per-frame copy of the HDR image), nothing else.
    let memory = lens_effects_memory(W, H, wgpu::TextureFormat::Rgba16Float, &LensEffects::CINEMATIC);
    assert_eq!(
        lens.texture_bytes - plain.texture_bytes,
        memory.texture_bytes(),
        "texture bytes the pass created: {memory:?}"
    );
}

/// The flare alone (no dirt, no aberration) draws no aberration copy, and the dirt alone records
/// no light cells: fewer passes, fewer bytes.
#[test]
fn each_effect_pays_only_for_its_own_passes() {
    let flare = LensEffects {
        dirt_intensity: 0.0,
        chromatic_aberration: 0.0,
        ..LensEffects::CINEMATIC
    };
    let dirt = LensEffects {
        dirt_intensity: 0.3,
        ..LensEffects::OFF
    };
    let aberration = LensEffects {
        chromatic_aberration: 0.002,
        ..LensEffects::OFF
    };
    let of = |effects: &LensEffects| LensEffectsBudget::of(effects, 2560, 1440);
    let cinematic = of(&LensEffects::CINEMATIC);
    // Five bright mips, the cells, the ghosts, the composite; the frame's copy and the
    // aberration's.
    assert_eq!((cinematic.render_passes, cinematic.copies), (8, 2));
    assert_eq!(of(&LensEffects::OFF), LensEffectsBudget::default());
    assert_eq!(of(&flare).copies, 1, "no aberration copy");
    // The dirt alone records no light cells and no ghosts (its ghost image stays cleared).
    assert_eq!(of(&dirt).render_passes, 6);
    // Aberration alone reads no bright image and no ghosts: the composite and both copies.
    assert_eq!(
        of(&aberration),
        LensEffectsBudget {
            render_passes: 1,
            copies: 2
        }
    );
    let full = lens_effects_memory(
        2560,
        1440,
        wgpu::TextureFormat::Rgba16Float,
        &LensEffects::CINEMATIC,
    );
    let no_ab = lens_effects_memory(2560, 1440, wgpu::TextureFormat::Rgba16Float, &flare);
    assert_eq!(no_ab.scene_copy, 0);
    assert_eq!(full.scene_copy, 2560 * 1440 * 8);
    eprintln!("1440p cinematic: {full:?}; flare only: {no_ab:?}");
}
