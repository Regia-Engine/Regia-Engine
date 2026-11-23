// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a rainy frame (spec 99 § 12: "no per-frame allocation"; ADR-3249). The DO27
//! chase measured +194 allocations per frame with `storm` rain, +181 of them the empty VFX layer
//! (fixed by ADR-2585) and +13 the precipitation itself.
//!
//! A counting wrapper around the runtime's translucent layer counts the heap allocations made
//! while the renderer is inside one of the layer's calls (its simulate, its draws, its occlusion
//! request and binding), plus the precipitation step the ECS runs each frame. Once the warm frames
//! are done, a steady rainy frame makes none of regia_vfx's own there: what is left is wgpu's one
//! uniform upload a frame (measured against a bare `queue.write_buffer`) and the render pass's
//! command-list growth. The frame's own share (the renderer's passes, the occlusion view) is
//! printed for the report, not asserted here: it lives in `regia_render`.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `empty_layer_alloc.rs`). Fixed frame counts, no wall clock.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    LayerCamera, PrecipOcclusionBinding, PrecipOcclusionRequest, QualityTier, RenderDevice, TranslucentLayer,
};
use regia_vfx::gpu::LAYER_WARM_FRAMES;
use regia_vfx::precip::{PrecipState, PrecipitationGpu};
use regia_vfx::runtime::translucent_layer_with_precipitation;
use regia_vfx::{
    PrecipBudget, PrecipIntegrator, PrecipitationHandle, PrecipitationSettings, VfxGpu, VfxProfile,
    WeatherConfig, WeatherState,
};

struct Counting;
thread_local! {
    /// Whether this thread's allocations count toward the frame: the drawing thread and the
    /// renderer's encode workers.
    static COUNTED: Cell<bool> = const { Cell::new(false) };
    /// Depth of the layer calls this thread is inside (its allocations are the layer's).
    static IN_LAYER: Cell<u32> = const { Cell::new(0) };
    /// The layer call this thread is inside (an index into [`CALLS`]).
    static CALL: Cell<usize> = const { Cell::new(0) };
}
/// The layer's calls, for the per-call breakdown the test prints.
const CALLS: [&str; 13] = [
    "step",
    "simulate",
    "draw",
    "participates_in_hdr",
    "is_active",
    "draw_hdr",
    "draw_decals",
    "bind_decal_targets",
    "wants_scene_after_decals",
    "has_decals",
    "occlusion_request",
    "bind_occlusion",
    "note_colour_format",
];
/// Allocations per layer call.
static PER_CALL: [AtomicU64; 13] = [const { AtomicU64::new(0) }; 13];
/// Allocations of the counted threads.
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// Allocations made inside a layer call, on any thread.
static LAYER_ALLOCS: AtomicU64 = AtomicU64::new(0);

fn count_this_thread() {
    let _ = COUNTED.try_with(|c| c.set(true));
}

fn note() {
    if COUNTED.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
    if IN_LAYER.try_with(Cell::get).unwrap_or(0) > 0 {
        LAYER_ALLOCS.fetch_add(1, Ordering::Relaxed);
        let call = CALL.try_with(Cell::get).unwrap_or(0);
        PER_CALL[call.min(CALLS.len() - 1)].fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The tests share the counters: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs `f` with this thread's allocations counted as the layer's, against call `name`.
fn in_layer<R>(name: &str, f: impl FnOnce() -> R) -> R {
    let index = CALLS.iter().position(|c| *c == name).unwrap_or(0);
    let previous = CALL.try_with(|c| c.replace(index)).unwrap_or(0);
    let _ = IN_LAYER.try_with(|c| c.set(c.get() + 1));
    let out = f();
    let _ = IN_LAYER.try_with(|c| c.set(c.get().saturating_sub(1)));
    let _ = CALL.try_with(|c| c.set(previous));
    out
}

/// The per-call counts, named, where non-zero.
fn per_call() -> Vec<(&'static str, u64)> {
    CALLS
        .iter()
        .zip(&PER_CALL)
        .map(|(name, n)| (*name, n.load(Ordering::Relaxed)))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// The runtime's layer, every call counted ([`in_layer`]).
struct Counted(Arc<dyn TranslucentLayer>);

impl TranslucentLayer for Counted {
    fn simulate(&self, depth: &wgpu::TextureView, camera: &LayerCamera, encoder: &mut wgpu::CommandEncoder) {
        in_layer("simulate", || self.0.simulate(depth, camera, encoder));
    }
    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        in_layer("draw", || self.0.draw(pass, format));
    }
    fn participates_in_hdr(&self) -> bool {
        in_layer("participates_in_hdr", || self.0.participates_in_hdr())
    }
    fn is_active(&self) -> bool {
        in_layer("is_active", || self.0.is_active())
    }
    fn draw_hdr(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        in_layer("draw_hdr", || self.0.draw_hdr(pass, format));
    }
    fn draw_decals(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        in_layer("draw_decals", || self.0.draw_decals(pass, format));
    }
    fn bind_decal_targets(&self, receiver: Option<&wgpu::TextureView>, scene: Option<&wgpu::TextureView>) {
        in_layer("bind_decal_targets", || {
            self.0.bind_decal_targets(receiver, scene)
        });
    }
    fn wants_scene_after_decals(&self) -> bool {
        in_layer("wants_scene_after_decals", || self.0.wants_scene_after_decals())
    }
    fn has_decals(&self) -> bool {
        in_layer("has_decals", || self.0.has_decals())
    }
    fn precipitation_occlusion_request(&self) -> Option<PrecipOcclusionRequest> {
        in_layer("occlusion_request", || self.0.precipitation_occlusion_request())
    }
    fn bind_precipitation_occlusion(&self, binding: Option<&PrecipOcclusionBinding<'_>>) {
        in_layer("bind_occlusion", || self.0.bind_precipitation_occlusion(binding));
    }
    fn note_colour_format(&self, format: wgpu::TextureFormat) {
        in_layer("note_colour_format", || self.0.note_colour_format(format));
    }
}

const W: u32 = 160;
const H: u32 = 96;
const DT: f32 = 1.0 / 60.0;
/// Frames per counted batch.
const BATCH: u64 = 8;
/// Frames drawn before counting: past the layer's warm frames, the pipelines and the
/// occlusion view's first draw.
const WARM_FRAMES: u32 = LAYER_WARM_FRAMES + 24;
/// Batches counted; the least of them is the steady count (a lone background event lands in one
/// batch, not in all).
const MEASURED: usize = 6;

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    (
        corners
            .into_iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// A floor and a slab roof over half of it (the occlusion view has a caster).
fn scene() -> LabScene {
    let (floor_v, floor_i) = quad(
        [
            [-60.0, 0.0, -60.0],
            [60.0, 0.0, -60.0],
            [60.0, 0.0, 60.0],
            [-60.0, 0.0, 60.0],
        ],
        [0.0, 1.0, 0.0],
    );
    let (roof_v, roof_i) = quad(
        [
            [-60.0, 3.0, -60.0],
            [-0.5, 3.0, -60.0],
            [-0.5, 3.0, 60.0],
            [-60.0, 3.0, 60.0],
        ],
        [0.0, -1.0, 0.0],
    );
    LabScene {
        width: W,
        height: H,
        eye: [-1.0, 1.6, 8.0],
        target: [-1.0, 1.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 120.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.05, 0.05, 0.06, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            LabMesh {
                vertices: floor_v,
                indices: floor_i,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: roof_v,
                indices: roof_i,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn state(weather: WeatherState) -> PrecipState {
    let config = WeatherConfig::default();
    let settings = PrecipitationSettings {
        enabled: true,
        ..PrecipitationSettings::from_config(&config)
    };
    PrecipState {
        integrator: PrecipIntegrator::default(),
        rates: weather.rates(&config),
        config,
        settings,
        quality: QualityTier::High,
        active: true,
        clock_s: 1.0,
        wind_m_per_s: [3.0, 0.0, 1.0],
        ..PrecipState::default()
    }
}

/// Per-batch allocations: the frame's (every counted thread) and the layer's.
struct Batches {
    frame: Vec<u64>,
    layer: Vec<u64>,
}

impl Batches {
    fn least_frame(&self) -> u64 {
        self.frame.iter().copied().min().unwrap_or(u64::MAX)
    }
    fn least_layer(&self) -> u64 {
        self.layer.iter().copied().min().unwrap_or(u64::MAX)
    }
}

/// A fresh renderer with the runtime layer and `weather`'s precipitation, stepped like the ECS
/// does (the integrator and the clock advance every frame) and drawn at a moving eye.
fn steady(device: &RenderDevice, weather: WeatherState, occlusion: bool) -> Batches {
    let name = if weather == WeatherState::CLEAR {
        "clear"
    } else if weather == WeatherState::STORM && occlusion {
        "storm"
    } else if weather == WeatherState::STORM {
        "storm, no occlusion view"
    } else {
        "blizzard"
    };
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_encode_worker_start_hook(count_this_thread);
    let gpu = VfxGpu::new(device, VfxProfile::default()).expect("vfx gpu");
    let handle = PrecipitationHandle::default();
    if let Ok(mut slot) = handle.state.lock() {
        *slot = state(weather);
        slot.settings.occlusion = occlusion;
    }
    if let Ok(mut slot) = handle.gpu.lock() {
        *slot = PrecipitationGpu::new(device, &VfxProfile::default()).ok();
    }
    let layer =
        translucent_layer_with_precipitation(Arc::new(Mutex::new(gpu)), handle.clone(), device.queue.clone());
    renderer.set_translucent_layers(vec![Arc::new(Counted(layer)) as Arc<dyn TranslucentLayer>]);
    renderer.set_precipitation_occlusion_budget(
        PrecipBudget::for_quality(QualityTier::High)
            .occlusion
            .filter(|_| occlusion),
        16_384,
    );
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut frame_index = 0u32;
    let mut draw = || {
        // The ECS step (`step_precipitation`'s state writes), counted as the layer's.
        in_layer("step", || {
            if let Ok(mut guard) = handle.state.lock() {
                let state = &mut *guard;
                state
                    .integrator
                    .step(&state.rates, &state.config, [3.0, 1.0], &budget, DT);
                state.clock_s += f64::from(DT);
                state.since_draw_s += DT;
            }
        });
        // A slow walk: the streaks see a moving eye, the occlusion tile stays put.
        let mut scene = lab.clone();
        scene.eye[2] -= 0.01 * frame_index as f32;
        scene.target[2] -= 0.01 * frame_index as f32;
        frame_index += 1;
        renderer
            .render_offscreen(device, &scene.as_frame_scene_with_quality(FrameQuality::LAB))
            .expect("frame");
    };
    for _ in 0..WARM_FRAMES {
        draw();
    }
    for counter in &PER_CALL {
        counter.store(0, Ordering::Relaxed);
    }
    let mut frame = Vec::with_capacity(MEASURED);
    let mut layer = Vec::with_capacity(MEASURED);
    for _ in 0..MEASURED {
        let (f0, l0) = (
            ALLOCS.load(Ordering::Relaxed),
            LAYER_ALLOCS.load(Ordering::Relaxed),
        );
        for _ in 0..BATCH {
            draw();
        }
        frame.push(ALLOCS.load(Ordering::Relaxed) - f0);
        layer.push(LAYER_ALLOCS.load(Ordering::Relaxed) - l0);
    }
    println!(
        "{name}: layer allocations by call over {MEASURED} batches: {:?}",
        per_call()
    );
    Batches { frame, layer }
}

/// Steady rain and snow allocate nothing inside the VFX layer and the precipitation step; the
/// whole frame's share against clear weather is printed for the report.
#[test]
fn steady_precipitation_allocates_nothing_of_regia_vfx_in_the_layer() {
    let _serial = serial();
    count_this_thread();
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let clear = steady(&device, WeatherState::CLEAR, true);
    let storm = steady(&device, WeatherState::STORM, true);
    let blizzard = steady(&device, WeatherState::BLIZZARD, true);
    // The same storm without the occlusion view: the difference is the view's share of the
    // frame (`regia_render`'s `frame/weather.rs`), the rest the renderer's layer passes.
    let open_sky = steady(&device, WeatherState::STORM, false);
    println!(
        "storm without the occlusion view: frame {:?} layer {:?}; the view's share per frame {:+.2}, \
         the layer passes' (storm without the view over clear) {:+.2}",
        open_sky.frame,
        open_sky.layer,
        (storm.least_frame() as f64 - open_sky.least_frame() as f64) / BATCH as f64,
        (open_sky.least_frame() as f64 - clear.least_frame() as f64) / BATCH as f64,
    );
    println!(
        "heap allocations per batch of {BATCH} frames: clear frame {:?} layer {:?}; storm frame {:?} \
         layer {:?}; blizzard frame {:?} layer {:?}",
        clear.frame, clear.layer, storm.frame, storm.layer, blizzard.frame, blizzard.layer
    );
    println!(
        "per frame, least batch: storm {:+.2} over clear ({:.2} in the layer), blizzard {:+.2} ({:.2} in \
         the layer)",
        (storm.least_frame() as f64 - clear.least_frame() as f64) / BATCH as f64,
        storm.least_layer() as f64 / BATCH as f64,
        (blizzard.least_frame() as f64 - clear.least_frame() as f64) / BATCH as f64,
        blizzard.least_layer() as f64 / BATCH as f64,
    );
    assert_eq!(clear.least_layer(), 0, "clear weather: {:?}", clear.layer);
    // What is left inside the layer is wgpu's, not regia_vfx's: the one uniform upload a frame
    // (`queue.write_buffer`'s staging copy, measured here on this device) and the frame's render
    // pass growing its command list as the drops record into it (at most two growths for the
    // rain, flake and splash commands). The base allocated 32 a frame here (the idle effects
    // context's frame upload, simulation and soft passes, and their draws).
    let upload = upload_cost(&device);
    let bound = BATCH * (upload + RECORDING_GROWTH);
    println!("bound per batch: {BATCH} x (one upload {upload} + recording {RECORDING_GROWTH}) = {bound}");
    for (name, batches) in [
        ("storm", &storm),
        ("blizzard", &blizzard),
        ("storm without the view", &open_sky),
    ] {
        assert!(
            batches.least_layer() <= bound,
            "{name}: the layer allocates {:?} per batch of {BATCH}, over {bound} (one upload of {upload} and \
             {RECORDING_GROWTH} recording growths a frame)",
            batches.layer
        );
    }
}

/// Allocations a fresh render pass's command list may make while the drops record into it, a frame.
const RECORDING_GROWTH: u64 = 2;

/// Heap allocations of one bare `queue.write_buffer` of a uniform on `device` (rounded up), after a
/// warm-up: what the GPU API spends on one upload.
fn upload_cost(device: &RenderDevice) -> u64 {
    let queue = &device.queue;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe uniform"),
        size: 1024,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bytes = [0u8; 1024];
    for _ in 0..16 {
        queue.write_buffer(&buffer, 0, &bytes);
        submit_and_drain(device);
    }
    let calls = 48u64;
    let before = LAYER_ALLOCS.load(Ordering::Relaxed);
    for _ in 0..calls {
        in_layer("step", || queue.write_buffer(&buffer, 0, &bytes));
        submit_and_drain(device);
    }
    (LAYER_ALLOCS.load(Ordering::Relaxed) - before).div_ceil(calls)
}

/// Submits the queue's pending writes and waits until the device has finished them.
///
/// What one upload allocates inside wgpu depends on what the device has finished: the queue's
/// encoder takes its command buffer from a free list that completed submissions refill, and when
/// the list is empty (the device is behind, on a loaded machine) it allocates 16 more, three heap
/// allocations that land in whichever upload came next. Measured under load without the wait:
/// one call of 3000 cost 9 instead of 6 in 2 runs of 40; with it, 40 runs of 40 were 6 every call
/// (ADR-5718). Draining makes every upload start from the same state, so two upload sequences
/// compare exactly.
fn submit_and_drain(device: &RenderDevice) {
    device.queue.submit([]);
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// What the precipitation's per-frame upload costs on the heap: its `prepare` (the CPU frame and
/// one uniform write) against a bare `queue.write_buffer` of a uniform, the GPU API's own upload
/// (the staging copy wgpu makes for every write). The prepare adds nothing to its one upload.
#[test]
fn the_precipitation_prepare_adds_nothing_to_its_upload() {
    use regia_vfx::gpu::VfxLighting;
    use regia_vfx::precip::{PrecipTarget, PrecipView};
    let _serial = serial();
    count_this_thread();
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let queue = &device.queue;
    let mut precip = PrecipitationGpu::new(&device, &VfxProfile::default()).expect("precipitation");
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe uniform"),
        size: 1024,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut state = state(WeatherState::STORM);
    let view = |frame: u32| PrecipView {
        camera: regia_vfx::VfxCamera {
            eye: regia_core::math::Vec3::new(0.0, 1.6, 8.0 - 0.01 * frame as f32),
            target: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            fov_y: 1.0,
            near: 0.1,
            far: 120.0,
        },
        lighting: VfxLighting::default(),
        target: PrecipTarget {
            width: W,
            height: H,
            sky_rows: [[0.0; 4]; 2],
            target_mode: 0.0,
            cloud_coverage: 0.0,
        },
    };
    let bytes = [0u8; 1024];
    let calls = 48u64;
    // Warm both paths (the queue's first submissions set up its pools).
    // Every submit waits for the device (`submit_and_drain`): each upload below then starts from
    // the same queue state, and the two sequences compare exactly.
    for frame in 0..16 {
        queue.write_buffer(&buffer, 0, &bytes);
        let drawn = state.frame_at([0.0, 1.6, 8.0], [0.0, -0.6, -8.0]);
        precip.prepare(queue, &drawn, &view(frame));
        submit_and_drain(&device);
    }
    let before = LAYER_ALLOCS.load(Ordering::Relaxed);
    for _ in 0..calls {
        in_layer("step", || queue.write_buffer(&buffer, 0, &bytes));
        submit_and_drain(&device);
    }
    let write = LAYER_ALLOCS.load(Ordering::Relaxed) - before;
    let before = LAYER_ALLOCS.load(Ordering::Relaxed);
    for frame in 0..calls as u32 {
        let drawn = in_layer("step", || state.frame_at([0.0, 1.6, 8.0], [0.0, -0.6, -8.0]));
        in_layer("simulate", || precip.prepare(queue, &drawn, &view(frame)));
        submit_and_drain(&device);
    }
    let prepare = LAYER_ALLOCS.load(Ordering::Relaxed) - before;
    println!(
        "heap allocations per call over {calls} calls: queue.write_buffer {:.2}, precipitation frame + prepare {:.2}",
        write as f64 / calls as f64,
        prepare as f64 / calls as f64
    );
    assert!(
        prepare <= write,
        "the prepare allocates {prepare} against its one upload's {write}"
    );
}
