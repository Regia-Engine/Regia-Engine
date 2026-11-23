// SPDX-License-Identifier: MPL-2.0
//! Per-frame cost of the effects upload stays flat over thousands of frames (DO27 v0.1.10 known
//! issue: `upload_runtime_frame` crept from 0.05 to 11.4 ms over 30 minutes, hidden or not).
//!
//! The upload re-sent the unchanged curve LUT every frame (two `queue.write_buffer` staging
//! buffers), wrote trails straight to the queue and staged spawn jobs for a frame that, with
//! the window hidden, was never drawn: staging buffers and spawn jobs piled up without bound.
//! The tests drive the runtime's `Update` for 5000 frames, rendered and not, and require the
//! device's live allocations, the spawn queue and every per-frame container to stay bounded and
//! the update's thread CPU time flat (single-threaded executor, `CLOCK_THREAD_CPUTIME_ID`).
//! Its own test binary: RSS and the allocator report are per process.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{RenderDevice, RuntimeRenderer};
use regia_vfx::gpu::{STALE_UPLOADS, TrailVertex, VfxGpuSizes};
use regia_vfx::runtime::VfxGpuHandle;
use regia_vfx::scene::VfxSceneSizes;
use regia_vfx::{VfxPlugin, VfxRuntimePlugin, VfxScene, VfxTrails};

/// The timed tests run one at a time (they share the machine's cores).
static TIMED: Mutex<()> = Mutex::new(());

/// Resident set size, KiB (Linux; 0 elsewhere).
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
        .map_or(0, |pages| pages * 4)
}

/// `(live allocations, memory blocks, allocated bytes)` of the device's allocator, if it reports.
fn allocator(device: &RenderDevice) -> Option<(usize, usize, u64)> {
    device
        .device
        .generate_allocator_report()
        .map(|r| (r.allocations.len(), r.blocks.len(), r.total_allocated_bytes))
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Probe: what `queue.write_buffer` costs per frame over many frames, with and without a submit
/// between frames (a hidden window renders and submits nothing). The 32 KiB write is the size
/// of a game's curve LUT; run it on the driver under suspicion (DO27's creep was on RADV).
#[test]
#[ignore = "diagnostic probe (prints a table)"]
fn probe_queue_writes() {
    let device = device();
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe dst"),
        size: 64 * 1024,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let a = vec![1u8; 2048];
    let b = vec![2u8; 1024];
    let big = vec![3u8; 32 * 1024];
    for mode in ["submit", "poll", "none"] {
        let frames = 20_000;
        let window = 2_000;
        let mut start = Instant::now();
        for frame in 1..=frames {
            device.queue.write_buffer(&buffer, 0, &a);
            device.queue.write_buffer(&buffer, 4096, &b);
            device.queue.write_buffer(&buffer, 8192, &big);
            match mode {
                "submit" => {
                    device.queue.submit(std::iter::empty::<wgpu::CommandBuffer>());
                }
                "poll" => {
                    let _ = device.device.poll(wgpu::PollType::Poll);
                }
                _ => {}
            }
            if frame % window == 0 {
                let us = start.elapsed().as_secs_f64() * 1e6 / f64::from(window);
                println!(
                    "write_buffer[{mode}] frame {frame}: {us:.2} us/frame, rss {} KiB, allocator {:?}",
                    rss_kib(),
                    allocator(&device)
                );
                start = Instant::now();
            }
        }
        device.queue.submit(std::iter::empty::<wgpu::CommandBuffer>());
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
        println!(
            "after flush: rss {} KiB, allocator {:?}",
            rss_kib(),
            allocator(&device)
        );
    }
}

/// An app with the effects runtime and a few long-lived smoke columns.
fn effects_app(device: &RenderDevice) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(11)));
    app.insert_resource(device.clone());
    app.init_resource::<RuntimeRenderer>();
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(regia_vfx::showcase::smoke()).expect("smoke");
    lib.add(regia_vfx::showcase::sparks()).expect("sparks");
    app.insert_resource(VfxScene::new(lib));
    app.add_plugins((VfxPlugin, VfxRuntimePlugin));
    for i in 0..4 {
        app.world_mut()
            .resource_mut::<VfxScene>()
            .spawn("smoke", Vec3::new(i as f32 * 4.0, 0.0, 0.0), Vec3::Z, 7 + i)
            .expect("spawn smoke");
    }
    app.world_mut()
        .resource_mut::<VfxScene>()
        .spawn("sparks", Vec3::new(0.0, 1.0, 0.0), Vec3::Z, 3)
        .expect("spawn sparks");
    for i in 0..20 {
        app.world_mut()
            .resource_mut::<VfxScene>()
            .spawn(
                "smoke",
                Vec3::new(i as f32 * 2.0 - 20.0, 0.0, -5.0),
                Vec3::Z,
                100 + i,
            )
            .expect("spawn smoke");
    }
    app
}

/// Probe: the effects runtime's `Update` over many frames that render nothing (hidden).
#[test]
#[ignore = "diagnostic probe (prints a table)"]
fn probe_hidden_updates() {
    let device = device();
    let mut app = effects_app(&device);
    app.update();
    assert!(
        app.world().contains_resource::<VfxGpuHandle>(),
        "effects context installed"
    );
    let frames = 30_000;
    let window = 2_000;
    let mut start = Instant::now();
    for frame in 1..=frames {
        app.update();
        if frame % window == 0 {
            let us = start.elapsed().as_secs_f64() * 1e6 / f64::from(window);
            let queued = app
                .world()
                .resource::<VfxGpuHandle>()
                .lock()
                .map(|gpu| gpu.queued_spawn_jobs());
            let scene = app.world().resource::<VfxScene>();
            println!(
                "hidden frame {frame}: {us:.2} us/update, rss {} KiB, allocator {:?}, queued spawn jobs {queued:?}, instances {}, live {}",
                rss_kib(),
                allocator(&device),
                scene.live_instances(),
                scene.estimated_live()
            );
            start = Instant::now();
        }
    }
}

/// One small offscreen frame through the runtime renderer (the effects layer draws into it).
fn render(app: &mut App, device: &RenderDevice) {
    use regia_render::{
        DirectionalLightUniform, FogSettings, FrameQuality, FrameScene, PostSettings, RenderProfile,
        RenderProfilePreset,
    };
    let post = PostSettings::default();
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let frame = FrameScene {
        width: 96,
        height: 54,
        eye: [0.0, 3.0, 20.0],
        target: [0.0, 3.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
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
        quality,
        meshes: Vec::new(),
    };
    app.world_mut()
        .resource_mut::<RuntimeRenderer>()
        .0
        .render_offscreen(device, &frame)
        .expect("frame");
}

/// Probe: `Update` cost over many frames with every frame rendered, none, or one in three
/// (a flapping hidden flag), on the real render path.
#[test]
#[ignore = "diagnostic probe (prints a table)"]
fn probe_rendered_updates() {
    let device = device();
    for mode in ["every", "third", "none"] {
        let mut app = effects_app(&device);
        app.update();
        render(&mut app, &device);
        assert!(
            app.world().contains_resource::<VfxGpuHandle>(),
            "effects context installed"
        );
        let frames = 12_000u32;
        let window = 1_000u32;
        let mut update = Duration::ZERO;
        let mut rendered = Duration::ZERO;
        for frame in 1..=frames {
            let t = Instant::now();
            app.update();
            update += t.elapsed();
            let draw = match mode {
                "every" => true,
                "third" => frame % 3 == 0,
                _ => false,
            };
            if draw {
                let t = Instant::now();
                render(&mut app, &device);
                rendered += t.elapsed();
            }
            if frame % window == 0 {
                let queued = app
                    .world()
                    .resource::<VfxGpuHandle>()
                    .lock()
                    .map(|gpu| gpu.queued_spawn_jobs());
                println!(
                    "rendered[{mode}] frame {frame}: update {:.1} us, render {:.1} us, rss {} KiB, allocator {:?}, queued spawn jobs {queued:?}, instances {}",
                    update.as_secs_f64() * 1e6 / f64::from(window),
                    rendered.as_secs_f64() * 1e6 / f64::from(window),
                    rss_kib(),
                    allocator(&device),
                    app.world().resource::<VfxScene>().live_instances(),
                );
                update = Duration::ZERO;
                rendered = Duration::ZERO;
            }
        }
    }
}

/// A short ribbon the host hands the effects every frame (DO27 tracers).
fn ribbon() -> Vec<TrailVertex> {
    let v = |x: f32, side: f32| TrailVertex {
        position: [x, 1.0 + 0.1 * side, 0.0],
        _pad: 0.0,
        uv: [x / 4.0, side],
        _pad2: [0.0; 2],
        color: [1.0, 0.6, 0.2, 1.0],
    };
    vec![
        v(0.0, -1.0),
        v(0.0, 1.0),
        v(4.0, -1.0),
        v(0.0, 1.0),
        v(4.0, 1.0),
        v(4.0, -1.0),
    ]
}

/// What a run looked like at one frame.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    allocations: usize,
    queued_spawn_jobs: usize,
    curve_uploads: u64,
    /// The scene's per-frame containers.
    scene: VfxSceneSizes,
    /// The GPU context's per-frame CPU containers.
    gpu: VfxGpuSizes,
}

fn snapshot(app: &App, device: &RenderDevice) -> Snapshot {
    let (allocations, curve_uploads, queued_spawn_jobs, gpu) = {
        let handle = app.world().resource::<VfxGpuHandle>();
        let gpu = handle.lock().expect("effects context");
        (
            allocator(device).map_or(0, |(live, _, _)| live),
            gpu.curve_uploads(),
            gpu.queued_spawn_jobs(),
            gpu.container_sizes(),
        )
    };
    Snapshot {
        allocations,
        queued_spawn_jobs,
        curve_uploads,
        scene: app.world().resource::<VfxScene>().container_sizes(),
        gpu,
    }
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// A cost stamp: the CPU time this thread has run (`regia_core::profiling::thread_cpu_time`,
/// `CLOCK_THREAD_CPUTIME_ID`), so that being descheduled on a loaded shared farm does not read as the update
/// getting slower; wall time where the platform has no thread clock. Real creep burns CPU, so it shows in both.
///
/// Not `/proc/thread-self/schedstat`: the kernel brings that file up to date only when the thread is switched
/// out, so it missed the running stretch before each reading (it read 0 µs for a whole single-threaded update and
/// for a fixed 30 µs loop, `probe_drawn_growth`), and on the multi-threaded executor it counted whatever the
/// main thread ran before its last park, which moved with the scheduler, not with the engine (ADR-214).
struct CpuStamp {
    wall: Instant,
    cpu: Option<Duration>,
}

impl CpuStamp {
    fn now() -> Self {
        Self {
            wall: Instant::now(),
            cpu: regia_core::profiling::thread_cpu_time(),
        }
    }

    fn elapsed(&self) -> Duration {
        match (self.cpu, regia_core::profiling::thread_cpu_time()) {
            (Some(start), Some(end)) => end.saturating_sub(start),
            _ => self.wall.elapsed(),
        }
    }
}

/// A fixed CPU workload (integer mixing plus a 256 KiB walk): its cost tracks the machine
/// (load, SMT siblings, clocks), not the engine.
fn reference_work(buffer: &mut [u64]) -> u64 {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for i in 0..8_000u64 {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i) ^ (x >> 17);
    }
    for (i, v) in buffer.iter_mut().enumerate() {
        *v = v.wrapping_add(x ^ i as u64);
        x = x.rotate_left(5) ^ *v;
    }
    std::hint::black_box(x)
}

/// `(voluntary, involuntary)` context switches of this thread so far (Linux; zeros elsewhere).
fn context_switches() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    (
        field("voluntary_ctxt_switches:"),
        field("nonvoluntary_ctxt_switches:"),
    )
}

/// Every schedule of `app` on the single-threaded executor: all systems run on the calling
/// thread, so its CPU time is the whole update's.
fn single_threaded(app: &mut App) {
    use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

/// One [`growth_run`] variant.
#[derive(Clone, Copy)]
struct Growth {
    /// Render a frame after every update.
    draw: bool,
    /// Hand the effects a ribbon every frame.
    trails: bool,
    /// The 25 effect instances of [`effects_app`] (otherwise none).
    instances: bool,
    /// All schedules on the single-threaded executor.
    single_threaded: bool,
    /// Per-system and per-span times (needs the [`regia_core::profiling::FrameProfiler`] installed).
    profiler: Option<&'static regia_core::profiling::FrameProfiler>,
    /// Frames to run (a multiple of 1000).
    frames: usize,
    /// Sample the containers (and the device allocator) at frame 500 and every quarter of the run.
    samples: bool,
    /// Print the update's median thread CPU time of every 100 frames.
    trace: bool,
}

/// Runs one probe variant and prints, for the last 500 frames of every 1000, the best 100-frame-window median
/// of the update's thread CPU time and wall time, of a fixed reference workload, the thread's context switches,
/// and at frame 500 and every quarter of the run every per-frame container's length; with the profiler, every
/// system's and span's mean time per window, largest growth first.
fn growth_run(device: &RenderDevice, reference_buffer: &mut [u64], label: &str, g: Growth) {
    let frames = g.frames;
    let windows: Vec<(usize, usize)> = (0..frames / 1000)
        .map(|k| (k * 1000 + 500, k * 1000 + 1000))
        .collect();
    let samples: Vec<usize> = std::iter::once(500)
        .chain((1..=4).map(|q| frames * q / 4))
        .collect();
    let mut app = if g.instances {
        effects_app(device)
    } else {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(11)));
        app.insert_resource(device.clone());
        app.init_resource::<RuntimeRenderer>();
        let mut lib = regia_vfx::scene::VfxLibrary::new();
        lib.add(regia_vfx::showcase::smoke()).expect("smoke");
        app.insert_resource(VfxScene::new(lib));
        app.add_plugins((VfxPlugin, VfxRuntimePlugin));
        app
    };
    if g.single_threaded {
        single_threaded(&mut app);
    }
    app.update();
    render(&mut app, device);
    let mut cpu = Vec::with_capacity(frames);
    let mut wall = Vec::with_capacity(frames);
    let mut reference = Vec::with_capacity(frames);
    let mut switches = Vec::with_capacity(frames + 1);
    let mut series: std::collections::HashMap<String, Vec<Duration>> = std::collections::HashMap::new();
    let mut taken = Vec::new();
    switches.push(context_switches());
    for frame in 1..=frames {
        if g.trails {
            app.world_mut().resource_mut::<VfxTrails>().0.extend(ribbon());
        }
        let r = CpuStamp::now();
        reference_work(reference_buffer);
        reference.push(r.elapsed());
        if let Some(profiler) = g.profiler {
            regia_core::profiling::take_system_times(&mut taken);
            profiler.end_frame();
        }
        let w = Instant::now();
        let t = CpuStamp::now();
        app.update();
        cpu.push(t.elapsed());
        wall.push(w.elapsed());
        switches.push(context_switches());
        if let Some(profiler) = g.profiler {
            regia_core::profiling::take_system_times(&mut taken);
            profiler.end_frame();
            let spans = profiler
                .last_frame()
                .into_iter()
                .map(|(name, time)| (format!("span {name}"), time));
            let systems = taken.iter().map(|&(slot, time, _)| {
                let name =
                    regia_core::profiling::system_name(slot).map_or_else(String::new, |n| n.to_string());
                (format!("system {name}"), time)
            });
            for (name, time) in spans.chain(systems) {
                series.entry(name).or_insert_with(|| vec![Duration::ZERO; frames])[frame - 1] = time;
            }
        }
        if g.draw {
            render(&mut app, device);
        }
        if g.samples && samples.contains(&frame) {
            let scene = app.world().resource::<VfxScene>().container_sizes();
            let gpu = app
                .world()
                .resource::<VfxGpuHandle>()
                .lock()
                .map(|gpu| gpu.container_sizes());
            let trails_cap = app.world().resource::<VfxTrails>().0.capacity();
            let lights = app
                .world()
                .resource::<regia_vfx::lights::VfxFrameLights>()
                .lights
                .len();
            println!(
                "[{label}] frame {frame}: entities {}, scene {scene:?}, gpu {gpu:?}, trails cap {trails_cap}, lights {lights}, live {}, rss {} KiB, allocator {:?}",
                app.world().entity_count(),
                app.world().resource::<VfxScene>().estimated_live(),
                rss_kib(),
                allocator(device)
            );
        }
    }
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    if g.trace {
        let medians: Vec<String> = cpu
            .chunks(100)
            .map(|chunk| format!("{:.1}", us(median(&mut chunk.to_vec()))))
            .collect();
        println!("[{label}] per 100 frames: {}", medians.join(" "));
    }
    for &(a, b) in &windows {
        let (vol, invol) = (switches[b].0 - switches[a].0, switches[b].1 - switches[a].1);
        println!(
            "[{label}] frames {a}..{b}: update cpu {:.1} us (median {:.1}), wall {:.1} us, reference {:.1} us, ctx switches vol {vol} invol {invol}",
            us(best_window_median(&cpu[a..b], 100)),
            us(median(&mut cpu[a..b].to_vec())),
            us(best_window_median(&wall[a..b], 100)),
            us(best_window_median(&reference[a..b], 100)),
        );
    }
    if g.profiler.is_some() {
        let mean = |s: &[Duration]| s.iter().map(Duration::as_secs_f64).sum::<f64>() * 1e6 / s.len() as f64;
        let mut rows: Vec<(&String, Vec<f64>)> = series
            .iter()
            .map(|(name, s)| (name, windows.iter().map(|&(a, b)| mean(&s[a..b])).collect()))
            .collect();
        rows.sort_by(|x, y| {
            let growth = |v: &Vec<f64>| v.last().copied().unwrap_or(0.0) - v.first().copied().unwrap_or(0.0);
            growth(&y.1).total_cmp(&growth(&x.1))
        });
        for (name, means) in rows.iter().take(14) {
            let cells: Vec<String> = means.iter().map(|m| format!("{m:.2}")).collect();
            println!("[{label}] {name}: {} us", cells.join(" "));
        }
        let systems: Vec<String> = windows
            .iter()
            .map(|&(a, b)| {
                let total: f64 = series
                    .iter()
                    .filter(|(name, _)| name.starts_with("system "))
                    .map(|(_, s)| mean(&s[a..b]))
                    .sum();
                format!("{total:.1}")
            })
            .collect();
        println!("[{label}] all systems: {} us", systems.join(" "));
    }
}

/// Probe: what grows in the drawn path (5000 frames per variant): drawn (the gate test's app), drawn on the
/// single-threaded executor, hidden, drawn without trails, drawn with no effect instance; then drawn with the
/// engine's per-system profiler on both executors.
#[test]
#[ignore = "diagnostic probe (prints a table)"]
fn probe_drawn_growth() {
    let device = device();
    let mut buffer = vec![0u64; 32 * 1024];
    let base = Growth {
        draw: true,
        trails: true,
        instances: true,
        single_threaded: false,
        profiler: None,
        frames: 5000,
        samples: true,
        trace: false,
    };
    growth_run(&device, &mut buffer, "drawn", base);
    let st = Growth {
        single_threaded: true,
        ..base
    };
    growth_run(&device, &mut buffer, "drawn-st", st);
    growth_run(&device, &mut buffer, "hidden", Growth { draw: false, ..base });
    growth_run(
        &device,
        &mut buffer,
        "drawn-notrails",
        Growth {
            trails: false,
            ..base
        },
    );
    growth_run(
        &device,
        &mut buffer,
        "drawn-empty",
        Growth {
            instances: false,
            ..base
        },
    );
    let profiler = installed_profiler();
    growth_run(
        &device,
        &mut buffer,
        "drawn-profiled",
        Growth { profiler, ..base },
    );
    growth_run(
        &device,
        &mut buffer,
        "drawn-st-profiled",
        Growth { profiler, ..st },
    );
}

/// Probe: what grows in the hidden path on the single-threaded executor (10 000 frames per variant, the
/// update's median per 100 frames): hidden twice, hidden without the container samples, hidden without trails,
/// hidden with no effect instance, then hidden and drawn with the per-system profiler.
#[test]
#[ignore = "diagnostic probe (prints a table)"]
fn probe_hidden_growth() {
    let device = device();
    let mut buffer = vec![0u64; 32 * 1024];
    let base = Growth {
        draw: false,
        trails: true,
        instances: true,
        single_threaded: true,
        profiler: None,
        frames: 10_000,
        samples: true,
        trace: true,
    };
    growth_run(&device, &mut buffer, "hidden-st", base);
    growth_run(&device, &mut buffer, "hidden-st-again", base);
    growth_run(
        &device,
        &mut buffer,
        "hidden-st-nosamples",
        Growth {
            samples: false,
            ..base
        },
    );
    growth_run(
        &device,
        &mut buffer,
        "hidden-st-notrails",
        Growth {
            trails: false,
            ..base
        },
    );
    growth_run(
        &device,
        &mut buffer,
        "hidden-st-empty",
        Growth {
            instances: false,
            ..base
        },
    );
    growth_run(&device, &mut buffer, "drawn-st", Growth { draw: true, ..base });
    let profiler = installed_profiler();
    growth_run(
        &device,
        &mut buffer,
        "hidden-st-profiled",
        Growth { profiler, ..base },
    );
}

/// The engine's frame profiler as the process subscriber's layer (once per process; the probes only).
fn installed_profiler() -> Option<&'static regia_core::profiling::FrameProfiler> {
    static PROFILER: std::sync::OnceLock<regia_core::profiling::FrameProfiler> = std::sync::OnceLock::new();
    let profiler = PROFILER.get_or_init(|| {
        let profiler = regia_core::profiling::FrameProfiler::new();
        let _log = regia_core::log::init(regia_core::log::LogConfig {
            profiler: Some(profiler.clone()),
            panic_hook: false,
            ..Default::default()
        });
        profiler
    });
    Some(profiler)
}

/// The smallest median over consecutive `window`-frame chunks of `samples`.
fn best_window_median(samples: &[Duration], window: usize) -> Duration {
    samples
        .chunks(window)
        .filter(|chunk| chunk.len() == window)
        .map(|chunk| median(&mut chunk.to_vec()))
        .min()
        .unwrap_or_default()
}

/// One run's snapshots at frames 500, 1500 and 5000 and its `Update` cost at frames 4500..5000, next to a young
/// twin's over the same frames.
#[derive(Clone, Copy)]
struct Run {
    at_500: Snapshot,
    at_1500: Snapshot,
    at_5000: Snapshot,
    /// Median of the 100-frame medians of the update's thread CPU time: the app at frames 1000..1500 and
    /// 4500..5000 (shown, not compared: the machine's load changes in between), and its young twin at its own
    /// frames 1000..1500, run interleaved with the app's 4500..5000.
    early: Duration,
    late: Duration,
    twin: Duration,
    /// Most `(time, jobs)` entries the update-window history may hold: one per drawn frame within the longest
    /// lifetime plus a quarter second.
    history_bound: usize,
}

/// The median of the medians of consecutive `window`-frame chunks of `samples`: real creep lifts every chunk,
/// while a burst of load lifts, and a phase of the effects lowers, only some of them.
fn mid_window_median(samples: &[Duration], window: usize) -> Duration {
    let mut medians: Vec<Duration> = samples
        .chunks(window)
        .filter(|chunk| chunk.len() == window)
        .map(|chunk| median(&mut chunk.to_vec()))
        .collect();
    median(&mut medians)
}

/// An effects app under test with its own device: [`effects_app`] on the single-threaded executor, plus an
/// optional [`injected_creep`].
struct Subject {
    device: RenderDevice,
    app: App,
    /// The update's thread CPU time per frame.
    times: Vec<Duration>,
}

impl Subject {
    fn new(creep_per_frame: usize) -> Self {
        let device = device();
        let mut app = effects_app(&device);
        if creep_per_frame > 0 {
            app.add_systems(Update, injected_creep(creep_per_frame));
        }
        single_threaded(&mut app);
        app.update();
        render(&mut app, &device);
        assert!(
            app.world().contains_resource::<VfxGpuHandle>(),
            "effects context installed"
        );
        Self {
            device,
            app,
            times: Vec::with_capacity(5000),
        }
    }

    /// One frame: trails handed over, the update timed, a frame rendered when `draw`.
    fn frame(&mut self, draw: bool) {
        self.app
            .world_mut()
            .resource_mut::<VfxTrails>()
            .0
            .extend(ribbon());
        let t = CpuStamp::now();
        self.app.update();
        self.times.push(t.elapsed());
        if draw {
            render(&mut self.app, &self.device);
        }
    }
}

/// Runs 5000 frames (trails handed over every frame, rendered when `draw`) and, for the last 1500 of them, a
/// young twin of the same app beside it.
///
/// What creeps grows with the frames an app has run, so the late cost is compared with a twin that is 3500
/// frames younger, its frames interleaved one by one with the app's (alternating which goes first): load on a
/// shared machine then lands on both alike. Comparing the app with itself 3500 frames earlier did not measure
/// the engine (ADR-214, `probe_drawn_growth`, `probe_hidden_growth`, 3080 box unloaded):
/// - `/proc/thread-self/schedstat` is brought up to date only when the thread is switched out: it read 0 µs for
///   a whole single-threaded update. The update is timed with `CLOCK_THREAD_CPUTIME_ID` instead.
/// - On the multi-threaded executor the main thread's share of the systems moved with the scheduler (+20 % with
///   no effect instance at all, every system's own time flat): the apps run on the single-threaded executor.
/// - The scene sits at the smoke budget (8192 live), so its cost dips while spawns are refused, a few hundred
///   frames every ~7.7 s; the best 100-frame window caught such a dip on one side only (+20 % on a flat
///   20.5 µs). The median of the five window medians does not.
/// - Thread CPU time still stretched by 40 % under a load ramp (shared caches, SMT siblings, clocks).
///
/// The app's own frames 1000..1500 (the population is steady after the 7.5 s smoke lifetime, 682 frames) are
/// reported beside it.
fn run_frames(draw: bool) -> Run {
    run_frames_with(draw, 0)
}

/// A deliberate creep: an `Update` system that sums a list growing by `per_frame` numbers a frame.
fn injected_creep(per_frame: usize) -> impl FnMut() + Send + Sync + 'static {
    let mut list: Vec<u64> = Vec::new();
    move || {
        let len = list.len() as u64;
        list.extend((0..per_frame as u64).map(|i| i ^ len));
        std::hint::black_box(list.iter().fold(0u64, |a, &b| a.wrapping_add(b)));
    }
}

/// [`run_frames`] with [`injected_creep`] of `creep_per_frame` numbers a frame (0: none) in both apps.
fn run_frames_with(draw: bool, creep_per_frame: usize) -> Run {
    let mut old = Subject::new(creep_per_frame);
    let mut young: Option<Subject> = None;
    let (mut at_500, mut at_1500) = (None, None);
    for frame in 1..=5000usize {
        if frame == 3501 {
            young = Some(Subject::new(creep_per_frame));
        }
        match young.as_mut() {
            Some(young) if frame % 2 == 0 => {
                young.frame(draw);
                old.frame(draw);
            }
            Some(young) => {
                old.frame(draw);
                young.frame(draw);
            }
            None => old.frame(draw),
        }
        match frame {
            500 => at_500 = Some(snapshot(&old.app, &old.device)),
            1500 => at_1500 = Some(snapshot(&old.app, &old.device)),
            _ => {}
        }
    }
    let young = young.expect("young twin");
    let run = Run {
        at_500: at_500.expect("frame 500"),
        at_1500: at_1500.expect("frame 1500"),
        at_5000: snapshot(&old.app, &old.device),
        early: mid_window_median(&old.times[1000..1500], 100),
        late: mid_window_median(&old.times[4500..5000], 100),
        twin: mid_window_median(&young.times[1000..1500], 100),
        history_bound: {
            let max_lifetime = old.app.world().resource::<VfxScene>().library().max_lifetime();
            ((max_lifetime + 0.25) / 0.011).ceil() as usize + 1
        },
    };
    println!(
        "draw={draw}: frame 500 {:?}; frame 1500 {:?}; frame 5000 {:?}; update at frames 1000..1500 {:?}, at 4500..5000 {:?}, young twin beside it {:?} ({:+.1} %); rss {} KiB",
        run.at_500,
        run.at_1500,
        run.at_5000,
        run.early,
        run.late,
        run.twin,
        (run.late.as_secs_f64() / run.twin.as_secs_f64() - 1.0) * 100.0,
        rss_kib()
    );
    run
}

/// Common to both runs: no GPU allocations accumulate between frame 500 and 5000, the curve
/// LUT is uploaded once (not once a frame), no per-frame container grows once the particle
/// population is steady (frame 1500: the longest smoke lifetime is 7.5 s, 682 frames), and the
/// `Update` cost at frame 5000 is within 10 % of a 3500-frame-younger twin's beside it.
fn assert_flat(run: &Run) {
    let Run {
        at_500,
        at_1500,
        at_5000,
        early,
        late,
        twin,
        history_bound,
    } = *run;
    assert!(
        at_5000.allocations <= at_500.allocations,
        "live GPU allocations grew from {} at frame 500 to {} at frame 5000 (queue writes piling up)",
        at_500.allocations,
        at_5000.allocations
    );
    assert_eq!(
        at_5000.curve_uploads, 1,
        "the unchanged curve LUT was uploaded {} times",
        at_5000.curve_uploads
    );
    // Structures the frame count could grow: none may, from the steady state on.
    let (s0, s1) = (at_1500.scene, at_5000.scene);
    assert_eq!(
        (s1.instance_slots, s1.free_slots, s1.gpu_slots, s1.pending_jobs),
        (s0.instance_slots, s0.free_slots, s0.gpu_slots, s0.pending_jobs),
        "scene containers changed between frame 1500 and 5000: {s0:?} -> {s1:?}"
    );
    // Death records follow the live particles (random lifetimes: a few percent either way).
    assert!(
        s1.death_records * 10 <= s0.death_records * 11,
        "death records grew from {} at frame 1500 to {} at frame 5000",
        s0.death_records,
        s1.death_records
    );
    let (g0, g1) = (at_1500.gpu, at_5000.gpu);
    assert!(
        g1.spawn_history <= history_bound && g1.spawn_history <= g0.spawn_history,
        "update-window history grew: {} at frame 1500, {} at frame 5000 (bound {history_bound})",
        g0.spawn_history,
        g1.spawn_history
    );
    assert_eq!(
        (
            g1.spawn_jobs_capacity,
            g1.trail_vertices,
            g1.pending_decals,
            g1.render_pipes,
            g1.free_emitter_slots
        ),
        (
            g0.spawn_jobs_capacity,
            g0.trail_vertices,
            g0.pending_decals,
            g0.render_pipes,
            g0.free_emitter_slots
        ),
        "GPU-context containers changed between frame 1500 and 5000: {g0:?} -> {g1:?}"
    );
    assert!(
        late.as_secs_f64() <= twin.as_secs_f64() * 1.10,
        "update cost crept: {late:?} at frames 4500..5000, {twin:?} for a 3500-frame-younger twin beside it \
         (the app at frames 1000..1500: {early:?})"
    );
}

/// A hidden window: the host keeps updating, nothing is drawn or submitted. The upload queues
/// no buffer writes and drops spawn jobs of frames nobody saw.
#[test]
fn hidden_updates_queue_nothing_and_stay_flat() {
    let _timed = TIMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let run = run_frames(false);
    assert_flat(&run);
    let (at_500, at_5000) = (run.at_500, run.at_5000);
    // At most `STALE_UPLOADS` frames of jobs: jobs that waited that many undrawn updates are
    // dropped (25 emitting instances schedule a few dozen a frame); before, the queue filled to
    // its 65 536-job cap and every later push warned.
    for (frame, at) in [(500, at_500), (5000, at_5000)] {
        assert!(
            at.queued_spawn_jobs < 1_024,
            "{} spawn jobs queued at frame {frame} while hidden (dropped after {STALE_UPLOADS} undrawn updates)",
            at.queued_spawn_jobs
        );
    }
}

/// A visible window: every frame drawn.
#[test]
fn rendered_updates_stay_flat() {
    let _timed = TIMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let run = run_frames(true);
    assert_flat(&run);
    assert_eq!(
        run.at_5000.queued_spawn_jobs, 0,
        "a drawn frame takes every queued job"
    );
}

/// The measurement sees creep of the size the gate reported (about 15 ns a frame): an injected system whose
/// cost grows by about 16 ns a frame (64 more numbers summed) fails the same 10 % check.
#[test]
#[ignore = "diagnostic probe (sensitivity of the creep check)"]
fn probe_detects_injected_creep() {
    let _timed = TIMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let run = run_frames_with(true, 64);
    assert!(
        run.late.as_secs_f64() > run.twin.as_secs_f64() * 1.10,
        "an injected 16 ns/frame creep went unseen: {:?} at frame 5000, young twin {:?}",
        run.late,
        run.twin
    );
}
