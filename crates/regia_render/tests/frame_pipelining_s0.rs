// SPDX-License-Identifier: MPL-2.0
//! Frame pipelining step S0 (ADR-2074): where the frame thread's time goes, measured from the
//! renderer's own `tracing` spans (`--ignored`, prints numbers).
//!
//! A test-local `tracing` layer records every span's enter and exit (name, parent, thread) on
//! one clock. The bench renders a dense, moving Desktop High scene (a ground plane and a grid
//! of boxes of several meshes and materials, sun shadows, depth prepass, IBL, post with TAA)
//! into an offscreen target with two frames in flight, like a presenting game. Per frame it
//! splits the time between two `render_into` starts into:
//!
//! - `begin`: the non-blocking readback pump (`frame_begin`);
//! - `prepare`: frame setup, draw lists, shadow plan, instance lists, bindings;
//! - `record`: every other direct child span of `encode` (the passes: shadows, prepass, cull,
//!   forward, translucents, post, …), also listed by name;
//! - `glue`: time inside `encode` no child span covers (unspanned recording and prepare);
//! - `submit`: the record-to-submit gap at the end of the frame: `finish_join` (the last
//!   segment's finish `finish_tail`, inline finishes of segments no worker took, the wait for
//!   the workers)
//!   plus `queue.submit`;
//! - `wait`: the bench's latency-2 backpressure wait for frame N-2 (stands in for a blocking
//!   swapchain acquire): the frame thread is idle on the GPU;
//! - `host`: the rest of the interval (the bench building the next scene).
//!
//! It also prints the encode workers' busy time (`finish_segment` on other threads), the
//! first-cut-to-submit latency (how long the first finished segment waits before the GPU
//! sees any of the frame), and the GPU frame time from timestamps (`GpuTimings::frame_ms`,
//! when the adapter has them) against the interval: the GPU's idle share.
//!
//! A second table answers the render-thread question directly: a fixed spin stands in for the
//! game's simulation (`REGIA_S0_SIM_MS`, default `0,1,3,8` ms a frame), and the same frames are
//! rendered inline on that thread, then through `with_render_thread` (a channel of capacity one,
//! with no extract cost and nothing shared: what a pipelined renderer does at best). It prints the
//! simulation thread's frame interval both ways and the gain.
//!
//! `REGIA_FRAME_PIPELINING=1` measures with the frame's pipelining switch on.
//!
//! The bench runs once per encode-worker count in `REGIA_S0_THREADS` (default `0,2,4`).
//! `REGIA_S0_WIDTH`, `REGIA_S0_HEIGHT` (default 640 × 360), `REGIA_S0_GRID` (boxes per side,
//! default 40: 1,600 boxes) and `REGIA_S0_FRAMES` (measured frames, default 60) size it;
//! `REGIA_S0_GPU_DRIVEN=0` takes the CPU draw loop (one bind and one draw per object).
//!
//! ```text
//! VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
//!   cargo test -p regia_render --release --test frame_pipelining_s0 -- --ignored --nocapture
//! ```
//!
//! On one card of a shared machine: `REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1` (and no
//! `VK_ICD_FILENAMES`). The numbers go into `docs/perf-log.md` and the lane report.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use regia_render::{
    CpuMesh, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, RenderThreadSettings, with_render_thread,
};
use tracing::Subscriber;
use tracing::span::Id;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

/// Frames rendered before measuring (pipeline compiles, caches, TAA history).
const WARM: usize = 20;
/// Frames in flight (the swapchain latency a presenting game runs with).
const IN_FLIGHT: usize = 2;
/// Top bits of the bench's keys (clear of the renderer's reserved top bytes).
const KEY_BASE: u64 = 0x00C5_0000_0000_0000;
/// Box mesh variants (distinct mesh keys, so the frame batches several meshes).
const MESHES: usize = 8;
/// Materials the boxes cycle through.
const MATERIALS: usize = 32;

/// One closed enter/exit interval of a span.
#[derive(Clone, Copy, Debug)]
struct Rec {
    name: &'static str,
    parent: Option<&'static str>,
    thread: usize,
    start: Instant,
    end: Instant,
}

static RECORDS: Mutex<Vec<Rec>> = Mutex::new(Vec::new());
static NEXT_THREAD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static THREAD: usize = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
}

fn thread_index() -> usize {
    THREAD.with(|t| *t)
}

/// The enter time of an open span.
struct Entered(Instant);

/// Records every span interval into `RECORDS`.
struct Timeline;

impl<S> Layer<S> for Timeline
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(Entered(Instant::now()));
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let end = Instant::now();
        let Some(span) = ctx.span(id) else { return };
        let Some(Entered(start)) = span.extensions_mut().remove::<Entered>() else {
            return;
        };
        let rec = Rec {
            name: span.name(),
            parent: span.parent().map(|p| p.name()),
            thread: thread_index(),
            start,
            end,
        };
        RECORDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(rec);
    }
}

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn ms(a: Instant, b: Instant) -> f64 {
    b.saturating_duration_since(a).as_secs_f64() * 1000.0
}

/// An axis-aligned box of half extents `h`, 24 vertices with face normals.
fn cuboid(h: [f32; 3]) -> CpuMesh {
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [0.0, 0.0, -1.0], [1.0, 0.0, 0.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                (n[0] + u[0] * su + v[0] * sv) * h[0],
                (n[1] + u[1] * su + v[1] * sv) * h[1],
                (n[2] + u[2] * su + v[2] * sv) * h[2],
            ];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                n,
                [su * 0.5 + 0.5, sv * 0.5 + 0.5],
            ));
        }
        // Counter-clockwise seen from outside along `n` (u × v = n for every face above).
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    CpuMesh { vertices, indices }
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

fn material(i: usize) -> LabMaterial {
    let k = i as f32 / MATERIALS as f32;
    LabMaterial {
        base_color: [0.25 + 0.6 * k, 0.6 - 0.4 * k, 0.3 + 0.2 * (k * 7.0).fract(), 1.0],
        roughness: 0.3 + 0.6 * (k * 3.0).fract(),
        metallic: if i.is_multiple_of(5) { 1.0 } else { 0.0 },
        ..LabMaterial::default()
    }
}

/// The lab scene the frames start from: the ground plane, camera, sun.
fn lab(width: u32, height: u32, ground: &CpuMesh) -> LabScene {
    LabScene {
        width,
        height,
        eye: [0.0, 12.0, 40.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.35], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.35, 0.4, 0.3, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: ground.vertices.clone(),
            indices: ground.indices.clone(),
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// Per-frame split, milliseconds.
#[derive(Clone, Copy, Debug, Default)]
struct Split {
    interval: f64,
    render_into: f64,
    begin: f64,
    prepare: f64,
    record: f64,
    glue: f64,
    submit: f64,
    finish_join: f64,
    finish_tail: f64,
    join_inline: f64,
    wait: f64,
    host: f64,
    workers_busy: f64,
    first_cut_to_submit: f64,
    gpu: f64,
}

const PREPARE: [&str; 5] = [
    "frame_setup",
    "draw_list",
    "shadow_plan",
    "instance_lists",
    "frame_bindings",
];

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

fn p50(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

struct Run {
    threads: usize,
    splits: Vec<Split>,
    passes: BTreeMap<&'static str, f64>,
    prepares: BTreeMap<&'static str, f64>,
    inside_prepare: BTreeMap<(&'static str, &'static str), f64>,
}

/// What every frame of the bench draws.
struct SceneData {
    width: u32,
    height: u32,
    grid: usize,
    frames: usize,
    boxes: Vec<CpuMesh>,
    materials: Vec<LabMaterial>,
    lab: LabScene,
    quality: FrameQuality,
    post: PostSettings,
}

impl SceneData {
    fn new() -> Self {
        let width = env_or("REGIA_S0_WIDTH", 640) as u32;
        let height = env_or("REGIA_S0_HEIGHT", 360) as u32;
        let ground = cuboid([60.0, 0.1, 60.0]);
        let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        profile.render_scale = 1.0;
        // 0: the CPU draw loop, one bind and one draw per object (the worst case for recording,
        // and how the Mobile presets draw).
        profile.gpu_driven = env_or("REGIA_S0_GPU_DRIVEN", 1) != 0;
        Self {
            width,
            height,
            grid: env_or("REGIA_S0_GRID", 40),
            frames: env_or("REGIA_S0_FRAMES", 60),
            boxes: (0..MESHES)
                .map(|i| cuboid([0.3 + 0.05 * i as f32, 0.4 + 0.15 * i as f32, 0.3]))
                .collect(),
            materials: (0..MATERIALS).map(material).collect(),
            lab: lab(width, height, &ground),
            quality: FrameQuality::from_profile(&profile),
            post: PostSettings {
                auto_exposure: false,
                exposure_ev: 14.5,
                bloom: true,
                taa: true,
                ..PostSettings::golden_deterministic()
            },
        }
    }

    /// The base scene (post, IBL) and its meshes: the ground, then the grid of boxes.
    fn scene(&self) -> (FrameScene<'_>, Vec<FrameMesh<'_>>) {
        let mut base = self.lab.as_frame_scene_with_quality(self.quality);
        base.post = Some(&self.post);
        base.ibl = true;
        let template = base.meshes[0];
        let (grid, spacing) = (self.grid, 2.4);
        let half = grid as f32 * spacing * 0.5;
        let mut meshes: Vec<FrameMesh<'_>> = vec![template];
        for i in 0..grid * grid {
            let (gx, gz) = ((i % grid) as f32, (i / grid) as f32);
            let mesh = &self.boxes[i % MESHES];
            meshes.push(FrameMesh {
                mesh_key: KEY_BASE | 0x1_0000 | (i % MESHES) as u64,
                material_key: KEY_BASE | 0x2_0000 | (i % MATERIALS) as u64,
                object_key: KEY_BASE | 0x3_0000 | i as u64,
                vertices: &mesh.vertices,
                indices: &mesh.indices,
                material: &self.materials[i % MATERIALS],
                model: translate(gx * spacing - half, 0.6, gz * spacing - half),
                ..template
            });
        }
        (base, meshes)
    }

    fn target(&self, device: &RenderDevice) -> wgpu::Texture {
        device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("s0 target"),
            size: wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }
}

/// The scene of frame `frame`: the camera orbits, so shadow caches, SSAO and the temporal passes
/// never settle.
fn orbit<'a>(base: &FrameScene<'a>, meshes: &[FrameMesh<'a>], frame: usize) -> FrameScene<'a> {
    let angle = frame as f32 * 0.01;
    FrameScene {
        eye: [40.0 * angle.sin(), 12.0, 40.0 * angle.cos()],
        target: [0.0, 0.0, 0.0],
        meshes: meshes.to_vec(),
        ..base.clone()
    }
}

/// Waits for the frame `IN_FLIGHT` before the last (a blocking swapchain acquire's stand-in).
fn backpressure(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    in_flight: &mut VecDeque<wgpu::SubmissionIndex>,
) {
    if let Some(submission) = renderer.take_submission() {
        in_flight.push_back(submission);
    }
    if in_flight.len() > IN_FLIGHT
        && let Some(oldest) = in_flight.pop_front()
    {
        let _ = device.device.poll(wgpu::PollType::Wait {
            submission_index: Some(oldest),
            timeout: None,
        });
    }
}

/// Burns `ms` milliseconds on this thread: the game's simulation.
fn simulate(ms: usize) {
    let started = Instant::now();
    while started.elapsed().as_secs_f64() * 1000.0 < ms as f64 {
        std::hint::spin_loop();
    }
}

/// The simulation thread's mean frame interval with the frames rendered inline or on a second
/// thread behind a channel of capacity one.
fn sim_interval(device: &RenderDevice, data: &SceneData, sim_ms: usize, threaded: bool) -> f64 {
    let (base, meshes) = data.scene();
    let target = data.target(device);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut renderer = FrameRenderer::new();
    let mut in_flight = VecDeque::new();
    let mut stamps = Vec::with_capacity(WARM + data.frames);
    if threaded {
        let settings = RenderThreadSettings {
            frames_in_flight: IN_FLIGHT,
            ..RenderThreadSettings::default()
        };
        let view = &view;
        let stamps = &mut stamps;
        with_render_thread(&mut renderer, device, settings, |host| {
            for frame in 0..WARM + data.frames {
                simulate(sim_ms);
                let target = FrameTarget::full(view, format, data.width, data.height);
                host.submit(orbit(&base, &meshes, frame), target)
                    .expect("the render thread is alive");
                stamps.push(Instant::now());
            }
        })
        .expect("the render thread started");
    } else {
        for frame in 0..WARM + data.frames {
            simulate(sim_ms);
            renderer
                .render_into(
                    device,
                    &orbit(&base, &meshes, frame),
                    FrameTarget::full(&view, format, data.width, data.height),
                )
                .unwrap_or_else(|err| panic!("frame {frame}: {err}"));
            backpressure(device, &mut renderer, &mut in_flight);
            stamps.push(Instant::now());
        }
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    RECORDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    mean(stamps[WARM..].windows(2).map(|w| ms(w[0], w[1])))
}

fn run(device: &RenderDevice, threads: usize) -> Run {
    let data = SceneData::new();
    let (width, height, frames) = (data.width, data.height, data.frames);
    let (base, meshes) = data.scene();
    let target = data.target(device);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = FrameRenderer::new();
    renderer.set_encode_threads(threads);
    renderer.set_gpu_timing(device, true);
    let render_thread = thread_index();
    let mut in_flight: VecDeque<wgpu::SubmissionIndex> = VecDeque::new();
    let mut waits = Vec::with_capacity(WARM + frames);
    let mut gpu_ms = Vec::with_capacity(WARM + frames);
    let mut measure_from = None;
    for frame in 0..WARM + frames + 1 {
        if frame == WARM {
            measure_from = Some(Instant::now());
        }
        let scene = orbit(&base, &meshes, frame);
        renderer
            .render_into(
                device,
                &scene,
                FrameTarget::full(&view, wgpu::TextureFormat::Rgba8UnormSrgb, width, height),
            )
            .unwrap_or_else(|err| panic!("frame {frame}: {err}"));
        gpu_ms.push(f64::from(renderer.gpu_timings().frame_ms));
        let started = Instant::now();
        backpressure(device, &mut renderer, &mut in_flight);
        waits.push((started, Instant::now()));
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    let from = measure_from.expect("measured frames");
    let records: Vec<Rec> =
        std::mem::take(&mut *RECORDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
            .into_iter()
            .filter(|r| r.start >= from)
            .collect();
    let frame_spans: Vec<&Rec> = records
        .iter()
        .filter(|r| r.thread == render_thread && r.name == "render_into")
        .collect();
    assert!(
        frame_spans.len() > frames,
        "every measured frame has a `render_into` span ({} of {})",
        frame_spans.len(),
        frames + 1
    );
    let mut splits = Vec::with_capacity(frames);
    let mut passes: BTreeMap<&'static str, f64> = BTreeMap::new();
    let mut prepares: BTreeMap<&'static str, f64> = BTreeMap::new();
    let mut inside_prepare: BTreeMap<(&'static str, &'static str), f64> = BTreeMap::new();
    for (i, pair) in frame_spans.windows(2).enumerate() {
        let (this, next) = (pair[0], pair[1]);
        let inside = |r: &&Rec| r.start >= this.start && r.start < next.start;
        let mut s = Split {
            interval: ms(this.start, next.start),
            render_into: ms(this.start, this.end),
            ..Split::default()
        };
        let mut encode = 0.0;
        let mut children = 0.0;
        let mut submit_end = None;
        let mut first_cut = None::<Instant>;
        for r in records.iter().filter(inside) {
            let d = ms(r.start, r.end);
            if r.thread != render_thread {
                if r.name == "finish_segment" {
                    s.workers_busy += d;
                    first_cut = Some(first_cut.map_or(r.start, |c| c.min(r.start)));
                }
                continue;
            }
            match (r.name, r.parent) {
                ("frame_begin", Some("render_into")) => s.begin += d,
                ("encode", Some("render_into")) => encode += d,
                ("submit", Some("encode")) => {
                    s.submit += d;
                    children += d;
                    submit_end = Some(r.end);
                }
                ("finish_join", _) => s.finish_join += d,
                ("finish_tail", Some("finish_join")) => s.finish_tail += d,
                ("finish_segment", Some("finish_join")) => s.join_inline += d,
                (name, Some("encode")) if PREPARE.contains(&name) => {
                    s.prepare += d;
                    children += d;
                    *prepares.entry(name).or_default() += d;
                }
                (name, Some(parent)) if PREPARE.contains(&parent) => {
                    *inside_prepare.entry((parent, name)).or_default() += d;
                }
                (name, Some("encode")) => {
                    s.record += d;
                    children += d;
                    *passes.entry(name).or_default() += d;
                }
                _ => {}
            }
        }
        s.glue = (encode - children).max(0.0);
        let frame_index = WARM + i;
        if let Some(&(a, b)) = waits.get(frame_index) {
            s.wait = ms(a, b);
        }
        s.host = (s.interval - s.render_into - s.wait).max(0.0);
        if let (Some(cut), Some(end)) = (first_cut, submit_end) {
            s.first_cut_to_submit = ms(cut, end);
        }
        s.gpu = gpu_ms.get(frame_index).copied().unwrap_or(0.0);
        splits.push(s);
    }
    let n = splits.len().max(1) as f64;
    for v in passes
        .values_mut()
        .chain(prepares.values_mut())
        .chain(inside_prepare.values_mut())
    {
        *v /= n;
    }
    Run {
        threads,
        splits,
        passes,
        prepares,
        inside_prepare,
    }
}

fn print(adapter: &str, run: &Run) -> String {
    let s = &run.splits;
    let m = |f: fn(&Split) -> f64| mean(s.iter().map(f));
    let interval = m(|s| s.interval);
    let pct = |v: f64| if interval > 0.0 { 100.0 * v / interval } else { 0.0 };
    let render_into = m(|s| s.render_into);
    let gpu = m(|s| s.gpu);
    println!(
        "s0 {adapter} threads={}: interval mean {interval:.3} ms (p50 {:.3}); render_into {render_into:.3} ({:.1} %)",
        run.threads,
        p50(s.iter().map(|s| s.interval).collect()),
        pct(render_into),
    );
    for (label, v) in [
        ("begin (readback pump)", m(|s| s.begin)),
        ("prepare", m(|s| s.prepare)),
        ("record (pass spans)", m(|s| s.record)),
        ("glue (unspanned, in encode)", m(|s| s.glue)),
        ("submit (record-to-submit gap)", m(|s| s.submit)),
        ("  of it finish_join", m(|s| s.finish_join)),
        ("  of it the last segment's finish", m(|s| s.finish_tail)),
        ("  of it inline segment finishes", m(|s| s.join_inline)),
        (
            "  of it waiting for the workers",
            m(|s| (s.finish_join - s.finish_tail - s.join_inline).max(0.0)),
        ),
        ("wait on GPU (latency 2)", m(|s| s.wait)),
        ("host (bench scene build)", m(|s| s.host)),
    ] {
        println!("  {label:<34} {v:8.3} ms  {:5.1} %", pct(v));
    }
    println!(
        "  encode workers busy {:.3} ms/frame; first cut to submit {:.3} ms; GPU frame (stamps) {gpu:.3} ms, GPU idle {:.1} % of the interval",
        m(|s| s.workers_busy),
        m(|s| s.first_cut_to_submit),
        if gpu > 0.0 {
            100.0 * (1.0 - gpu / interval).max(0.0)
        } else {
            f64::NAN
        },
    );
    let mut by_cost: Vec<(&&str, &f64)> = run.passes.iter().collect();
    by_cost.sort_by(|a, b| b.1.total_cmp(a.1));
    let list: Vec<String> = by_cost
        .iter()
        .take(12)
        .map(|(n, v)| format!("{n} {v:.3}"))
        .collect();
    println!("  pass spans (ms/frame): {}", list.join(", "));
    let list: Vec<String> = run.prepares.iter().map(|(n, v)| format!("{n} {v:.3}")).collect();
    println!("  prepare spans (ms/frame): {}", list.join(", "));
    let mut inside: Vec<_> = run.inside_prepare.iter().collect();
    inside.sort_by(|a, b| b.1.total_cmp(a.1));
    let list: Vec<String> = inside
        .iter()
        .take(12)
        .map(|((p, n), v)| format!("{p}>{n} {v:.3}"))
        .collect();
    println!("  inside prepare (ms/frame): {}", list.join(", "));
    format!(
        "| {adapter} | {} | {interval:.2} | {render_into:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {gpu:.2} |",
        run.threads,
        m(|s| s.begin),
        m(|s| s.prepare),
        m(|s| s.record),
        m(|s| s.glue),
        m(|s| s.submit),
        m(|s| s.wait),
        m(|s| s.workers_busy),
    )
}

#[test]
#[ignore = "bench: prints numbers"]
fn frame_pipelining_s0() {
    tracing_subscriber::registry().with(Timeline).init();
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let info = device.adapter.get_info();
    let adapter = info.name.clone();
    println!(
        "frame pipelining S0 on {} ({:?}, driver {} {})",
        info.name, info.backend, info.driver, info.driver_info
    );
    if cfg!(debug_assertions) {
        println!("WARNING: a debug build; run with --release for numbers worth recording");
    }
    if !device.capabilities.timestamp_query {
        println!("no in-encoder timestamp queries: GPU frame times read 0");
    }
    let threads: Vec<usize> = std::env::var("REGIA_S0_THREADS")
        .unwrap_or_else(|_| "0,2,4".into())
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    let mut rows = Vec::new();
    for t in threads {
        let run = run(&device, t);
        rows.push(print(&adapter, &run));
    }
    println!();
    println!(
        "| adapter | encode threads | interval | render_into | begin | prepare | record | glue | submit | wait on GPU | workers busy | GPU frame |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for row in rows {
        println!("{row}");
    }
    print_targets();
    let data = SceneData::new();
    println!();
    println!("| adapter | simulation ms | inline interval | render thread interval | gain |");
    println!("|---|---|---|---|---|");
    let sims: Vec<usize> = std::env::var("REGIA_S0_SIM_MS")
        .unwrap_or_else(|_| "0,1,3,8".into())
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    for sim in sims {
        let inline = sim_interval(&device, &data, sim, false);
        let threaded = sim_interval(&device, &data, sim, true);
        println!(
            "| {adapter} | {sim} | {inline:.2} | {threaded:.2} | {:.1} % |",
            100.0 * (1.0 - threaded / inline)
        );
    }
}

/// The frame's registered render targets by size: what transient aliasing could reuse.
fn print_targets() {
    let mut targets = regia_render::resource_inspector::list_resources();
    targets.sort_by_key(|t| std::cmp::Reverse(t.bytes));
    let total: u64 = targets.iter().map(|t| t.bytes).sum();
    println!();
    println!(
        "registered render targets: {} textures, {:.1} MiB",
        targets.len(),
        total as f64 / 1048576.0
    );
    for t in targets.iter().take(40) {
        println!(
            "  {:<36} {:>12} {:>5}x{:<5} layers {} mips {} samples {} {:8.2} MiB",
            t.label,
            t.format,
            t.width,
            t.height,
            t.layers,
            t.mips,
            t.samples,
            t.bytes as f64 / 1048576.0,
        );
    }
}

/// The S0 scene in a presenting window (ADR-2798): whether the render thread's gain survives a
/// swapchain and a compositor, which the offscreen run above cannot say (`--ignored`, prints numbers).
///
/// It opens a window of `REGIA_S0_WIDTH` × `REGIA_S0_HEIGHT` and, for each simulation cost in
/// `REGIA_S0_SIM_MS`, renders the same orbiting frames twice: inline (simulate, acquire, record,
/// submit, present on one thread) and through `with_render_thread`, where the host simulates and
/// posts the whole frame (acquire, record, submit, present) to the render thread
/// (`RenderThreadHost::post`). Per frame it records on one clock:
///
/// - `interval`: the simulation thread's frame interval (what a game's tick rate sees);
/// - `redraw wait`: from the frame's `request_redraw` to the next `RedrawRequested` (the
///   compositor's frame callback on Wayland, the study's H1); 0 with `REGIA_S0_LOOP=poll`, which
///   runs frames from `about_to_wait` instead;
/// - `hand-off`: the simulation thread after its simulation: the whole frame inline, the wait for
///   the channel with a render thread;
/// - `acquire`, `record`, `present`: the presenting thread's `get_current_texture`, `render_into`
///   (prepare, record, submit) and `pre_present_notify` + `present` (the study's H3);
/// - `latency`: from the start of the frame's simulation (where a game samples input) to the
///   return of its `present`: what the render thread adds to input latency, before the display's own
///   queue.
///
/// `REGIA_S0_QUEUE` lists the render thread's queue depths to run (`RenderThreadSettings::
/// queued_frames`, default `1,0`: one queued frame, then a rendezvous).
/// `REGIA_S0_PRESENT` is `fifo` (default, vsync), `mailbox` or `immediate`; `REGIA_S0_LATENCY` the
/// swapchain's `desired_maximum_frame_latency` (default 2, the engine's); `REGIA_S0_ENCODE` the
/// encode threads (default 2). On a shared Linux host without a display, run it under `xvfb-run -a`
/// (X11, no vsync: lavapipe presents with a CPU copy). The numbers that decide ADR-2074 S5 come from a
/// card on a real display: Wayland (Hyprland) for H1, and X11 or Windows.
///
/// ```text
/// xvfb-run -a -s "-screen 0 1920x1080x24" env VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
///   cargo test -p regia_render --release --test frame_pipelining_s0 -- --ignored --nocapture frame_pipelining_s0_window
/// ```
#[test]
#[ignore = "bench: opens a window, prints numbers"]
fn frame_pipelining_s0_window() {
    window_bench::run();
}

#[cfg(not(any(target_os = "linux", windows)))]
mod window_bench {
    /// The window bench needs a winit event loop off the main thread (Linux and Windows only).
    pub fn run() {
        println!("frame_pipelining_s0_window: skipped (Linux and Windows only)");
    }
}

#[cfg(any(target_os = "linux", windows))]
mod window_bench {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Instant;

    use regia_render::{
        FrameRenderer, FrameScene, FrameTarget, RenderDevice, RenderThreadSettings, create_window_instance,
        instance_backends, with_render_thread,
    };
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
    use winit::window::{Window, WindowId};

    use super::{SceneData, WARM, env_or, mean, ms, orbit, p50, simulate};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Drive {
        /// A frame per `RedrawRequested`, the next one requested at the end of the frame (the
        /// engine's runner).
        Redraw,
        /// A frame per `about_to_wait` under `ControlFlow::Poll` (no compositor gating).
        Poll,
    }

    /// One frame on the simulation thread.
    #[derive(Clone, Copy)]
    struct HostStamp {
        start: Instant,
        sim_end: Instant,
        handed: Instant,
        redraw_wait: f64,
    }

    /// One frame on the presenting thread.
    #[derive(Clone, Copy, Default)]
    struct PresentStamp {
        acquire: f64,
        record: f64,
        present: f64,
        present_end: Option<Instant>,
        gpu: f64,
        skipped: bool,
    }

    /// What presents a frame: the surface and the window it belongs to.
    struct Presenter<'a> {
        device: &'a RenderDevice,
        surface: &'a wgpu::Surface<'static>,
        config: &'a wgpu::SurfaceConfiguration,
        window: &'a Window,
    }

    impl Presenter<'_> {
        fn frame(&self, renderer: &mut FrameRenderer, scene: &FrameScene<'_>) -> PresentStamp {
            let started = Instant::now();
            let acquired = self.surface.get_current_texture();
            let acquired_at = Instant::now();
            let texture = match acquired {
                wgpu::CurrentSurfaceTexture::Success(texture)
                | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
                other => {
                    eprintln!("acquire: {other:?}; reconfiguring");
                    self.surface.configure(&self.device.device, self.config);
                    return PresentStamp {
                        acquire: ms(started, acquired_at),
                        skipped: true,
                        ..PresentStamp::default()
                    };
                }
            };
            let view = texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let target = FrameTarget::full(&view, self.config.format, self.config.width, self.config.height);
            if let Err(err) = renderer.render_into(self.device, scene, target) {
                eprintln!("frame failed: {err}");
            }
            let recorded = Instant::now();
            self.window.pre_present_notify();
            self.device.queue.present(texture);
            let presented = Instant::now();
            PresentStamp {
                acquire: ms(started, acquired_at),
                record: ms(acquired_at, recorded),
                present: ms(recorded, presented),
                present_end: Some(presented),
                gpu: f64::from(renderer.gpu_timings().frame_ms),
                skipped: false,
            }
        }
    }

    /// Opens the window once.
    struct Setup {
        size: PhysicalSize<u32>,
        window: Option<Arc<Window>>,
        error: Option<String>,
    }

    impl ApplicationHandler for Setup {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_none() {
                let attributes = Window::default_attributes()
                    .with_title("Regia S0 window bench")
                    .with_inner_size(self.size)
                    .with_resizable(false);
                match event_loop.create_window(attributes) {
                    Ok(window) => self.window = Some(Arc::new(window)),
                    Err(err) => self.error = Some(err.to_string()),
                }
            }
            event_loop.exit();
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    /// One measured run: `step` renders (inline) or hands over (threaded) frame `n`.
    struct Phase<'w, F: FnMut(usize)> {
        window: &'w Window,
        drive: Drive,
        sim_ms: usize,
        total: usize,
        frame: usize,
        requested: Option<Instant>,
        host: Vec<HostStamp>,
        closed: bool,
        step: F,
    }

    impl<F: FnMut(usize)> Phase<'_, F> {
        fn tick(&mut self, event_loop: &ActiveEventLoop) {
            if self.frame >= self.total {
                event_loop.exit();
                return;
            }
            let start = Instant::now();
            let redraw_wait = self.requested.take().map_or(0.0, |r| ms(r, start));
            simulate(self.sim_ms);
            let sim_end = Instant::now();
            (self.step)(self.frame);
            let handed = Instant::now();
            self.host.push(HostStamp {
                start,
                sim_end,
                handed,
                redraw_wait,
            });
            self.frame += 1;
            if self.drive == Drive::Redraw {
                self.window.request_redraw();
                self.requested = Some(Instant::now());
            }
        }
    }

    impl<F: FnMut(usize)> ApplicationHandler for Phase<'_, F> {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            event_loop.set_control_flow(match self.drive {
                Drive::Redraw => ControlFlow::Wait,
                Drive::Poll => ControlFlow::Poll,
            });
            if self.drive == Drive::Redraw && self.frame == 0 {
                self.window.request_redraw();
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::RedrawRequested if self.drive == Drive::Redraw => self.tick(event_loop),
                WindowEvent::CloseRequested => {
                    self.closed = true;
                    event_loop.exit();
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.drive == Drive::Poll {
                self.tick(event_loop);
            }
        }
    }

    /// A row of the table: means over the measured frames (after `WARM`).
    struct Row {
        interval: f64,
        interval_p50: f64,
        sim: f64,
        redraw_wait: f64,
        hand_off: f64,
        acquire: f64,
        record: f64,
        present: f64,
        latency: f64,
        latency_p50: f64,
        gpu: f64,
        skipped: usize,
    }

    fn row(host: &[HostStamp], present: &[PresentStamp]) -> Row {
        let host = host.get(WARM..).unwrap_or(&[]);
        let present = present.get(WARM..).unwrap_or(&[]);
        let intervals: Vec<f64> = host.windows(2).map(|w| ms(w[0].start, w[1].start)).collect();
        let latencies: Vec<f64> = host
            .iter()
            .zip(present)
            .filter_map(|(h, p)| p.present_end.map(|end| ms(h.start, end)))
            .collect();
        Row {
            interval: mean(intervals.iter().copied()),
            interval_p50: p50(intervals),
            sim: mean(host.iter().map(|h| ms(h.start, h.sim_end))),
            redraw_wait: mean(host.iter().map(|h| h.redraw_wait)),
            hand_off: mean(host.iter().map(|h| ms(h.sim_end, h.handed))),
            acquire: mean(present.iter().map(|p| p.acquire)),
            record: mean(present.iter().map(|p| p.record)),
            present: mean(present.iter().map(|p| p.present)),
            latency: mean(latencies.iter().copied()),
            latency_p50: p50(latencies),
            gpu: mean(present.iter().map(|p| p.gpu)),
            skipped: present.iter().filter(|p| p.skipped).count(),
        }
    }

    fn print_row(label: &str, sim_ms: usize, mode: &str, r: &Row) {
        println!(
            "| {label} | {sim_ms} | {mode} | {:.2} ({:.2}) | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} ({:.2}) | {:.2} | {} |",
            r.interval,
            r.interval_p50,
            r.sim,
            r.redraw_wait,
            r.hand_off,
            r.acquire,
            r.record,
            r.present,
            r.latency,
            r.latency_p50,
            r.gpu,
            r.skipped,
        );
    }

    fn event_loop() -> EventLoop<()> {
        let mut builder = EventLoop::builder();
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::EventLoopBuilderExtWayland;
            EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
        }
        #[cfg(windows)]
        {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        }
        match builder.build() {
            Ok(event_loop) => event_loop,
            Err(err) => panic!("no event loop (no display? run under xvfb-run -a): {err}"),
        }
    }

    fn present_mode() -> wgpu::PresentMode {
        match std::env::var("REGIA_S0_PRESENT").as_deref().map(str::trim) {
            Ok("mailbox") => wgpu::PresentMode::Mailbox,
            Ok("immediate") => wgpu::PresentMode::Immediate,
            _ => wgpu::PresentMode::Fifo,
        }
    }

    pub fn run() {
        let data = SceneData::new();
        let mut event_loop = event_loop();
        let mut setup = Setup {
            size: PhysicalSize::new(data.width, data.height),
            window: None,
            error: None,
        };
        if let Err(err) = event_loop.run_app_on_demand(&mut setup) {
            panic!("event loop: {err}");
        }
        let window = match (setup.window, setup.error) {
            (Some(window), _) => window,
            (None, error) => panic!("no window: {}", error.unwrap_or_default()),
        };
        let mut descriptor =
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(event_loop.owned_display_handle()));
        descriptor.backends = instance_backends();
        descriptor.flags = wgpu::InstanceFlags::from_build_config().with_env();
        let instance = create_window_instance(descriptor);
        let surface = match instance.create_surface(Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(err) => panic!("no surface: {err}"),
        };
        let device = match RenderDevice::new(instance, Some(&surface)) {
            Ok(device) => device,
            Err(err) => panic!("no device for the window: {err}"),
        };
        let size = window.inner_size();
        let Some(mut config) = surface.get_default_config(&device.adapter, size.width, size.height) else {
            panic!("the adapter cannot present to this window");
        };
        let caps = surface.get_capabilities(&device.adapter);
        if let Some(format) = caps.formats.iter().copied().find(wgpu::TextureFormat::is_srgb) {
            config.format = format;
        }
        let wanted = present_mode();
        config.present_mode = if caps.present_modes.contains(&wanted) {
            wanted
        } else {
            println!("{wanted:?} not offered ({:?}); using Fifo", caps.present_modes);
            wgpu::PresentMode::Fifo
        };
        config.desired_maximum_frame_latency = env_or("REGIA_S0_LATENCY", 2) as u32;
        surface.configure(&device.device, &config);
        let info = device.adapter.get_info();
        let drive = match std::env::var("REGIA_S0_LOOP").as_deref().map(str::trim) {
            Ok("poll") => Drive::Poll,
            _ => Drive::Redraw,
        };
        let encode = env_or("REGIA_S0_ENCODE", 2);
        println!(
            "frame pipelining S0 in a window on {} ({:?}, driver {} {}): {}x{} {:?}, {:?}, latency {}, loop {drive:?}, {} boxes, {encode} encode threads, {} frames",
            info.name,
            info.backend,
            info.driver,
            info.driver_info,
            config.width,
            config.height,
            config.format,
            config.present_mode,
            config.desired_maximum_frame_latency,
            data.grid * data.grid,
            data.frames,
        );
        if cfg!(debug_assertions) {
            println!("WARNING: a debug build; run with --release for numbers worth recording");
        }
        let sims: Vec<usize> = std::env::var("REGIA_S0_SIM_MS")
            .unwrap_or_else(|_| "0,1,3,8".into())
            .split(',')
            .filter_map(|t| t.trim().parse().ok())
            .collect();
        let queues: Vec<usize> = std::env::var("REGIA_S0_QUEUE")
            .unwrap_or_else(|_| "1,0".into())
            .split(',')
            .filter_map(|t| t.trim().parse().ok())
            .collect();
        let presenter = Presenter {
            device: &device,
            surface: &surface,
            config: &config,
            window: &window,
        };
        let (base, meshes) = data.scene();
        let label = info.name.clone();
        let mut lines = Vec::new();
        for sim_ms in sims {
            let total = WARM + data.frames;
            // Inline.
            let mut renderer = FrameRenderer::new();
            renderer.set_encode_threads(encode);
            renderer.set_gpu_timing(&device, true);
            let mut presents = Vec::with_capacity(total);
            let host = {
                let mut phase = Phase {
                    window: &window,
                    drive,
                    sim_ms,
                    total,
                    frame: 0,
                    requested: None,
                    host: Vec::with_capacity(total),
                    closed: false,
                    step: |frame: usize| {
                        let scene = orbit(&base, &meshes, frame);
                        presents.push(presenter.frame(&mut renderer, &scene));
                    },
                };
                if let Err(err) = event_loop.run_app_on_demand(&mut phase) {
                    panic!("event loop: {err}");
                }
                assert!(!phase.closed, "the window was closed");
                phase.host
            };
            let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
            lines.push((sim_ms, "inline".to_owned(), row(&host, &presents)));
            // Render thread, once per queue depth.
            for &queued_frames in &queues {
                let mut renderer = FrameRenderer::new();
                renderer.set_encode_threads(encode);
                renderer.set_gpu_timing(&device, true);
                let threaded: Mutex<Vec<(usize, PresentStamp)>> = Mutex::new(Vec::with_capacity(total));
                let settings = RenderThreadSettings {
                    frames_in_flight: 0,
                    queued_frames,
                };
                let (host, _) = match with_render_thread(&mut renderer, &device, settings, |thread| {
                    let (presenter, threaded, base, meshes) = (&presenter, &threaded, &base, &meshes);
                    let mut phase = Phase {
                        window: &window,
                        drive,
                        sim_ms,
                        total,
                        frame: 0,
                        requested: None,
                        host: Vec::with_capacity(total),
                        closed: false,
                        step: |frame: usize| {
                            let scene: FrameScene<'_> = orbit(base, meshes, frame);
                            let posted = thread.post(move |renderer| {
                                let stamp = presenter.frame(renderer, &scene);
                                threaded
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .push((frame, stamp));
                            });
                            if let Err(err) = posted {
                                panic!("the render thread stopped: {err}");
                            }
                        },
                    };
                    if let Err(err) = event_loop.run_app_on_demand(&mut phase) {
                        panic!("event loop: {err}");
                    }
                    assert!(!phase.closed, "the window was closed");
                    phase.host
                }) {
                    Ok(result) => result,
                    Err(err) => panic!("no render thread: {err}"),
                };
                let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
                let mut presents = threaded.into_inner().unwrap_or_else(PoisonError::into_inner);
                presents.sort_by_key(|(frame, _)| *frame);
                let presents: Vec<PresentStamp> = presents.into_iter().map(|(_, p)| p).collect();
                let mut threaded_row = row(&host, &presents);
                threaded_row.skipped += total.saturating_sub(presents.len());
                lines.push((
                    sim_ms,
                    format!("render thread, queue {queued_frames}"),
                    threaded_row,
                ));
            }
        }
        println!();
        println!(
            "| adapter | simulation ms | mode | interval (p50) | sim | redraw wait | hand-off | acquire | record | present | latency (p50) | GPU frame | skipped |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for (sim_ms, mode, r) in &lines {
            print_row(&label, *sim_ms, mode, r);
        }
        println!();
        println!(
            "| adapter | simulation ms | mode | inline interval | render thread interval | gain | latency change |"
        );
        println!("|---|---|---|---|---|---|---|");
        let mut inline: Option<&Row> = None;
        for (sim_ms, mode, r) in &lines {
            if mode == "inline" {
                inline = Some(r);
                continue;
            }
            if let Some(base) = inline {
                println!(
                    "| {label} | {sim_ms} | {mode} | {:.2} | {:.2} | {:.1} % | {:+.2} ms |",
                    base.interval,
                    r.interval,
                    100.0 * (1.0 - r.interval / base.interval),
                    r.latency - base.latency,
                );
            }
        }
    }
}
