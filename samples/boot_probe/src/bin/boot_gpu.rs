// SPDX-License-Identifier: MIT-0
//! `boot_gpu`: the boot path's GPU steps without a window, for GPUs that cannot present to the
//! machine's display (NVIDIA on a farm's Xvfb). `boot_probe` measures the real windowed path.
//!
//! ```text
//! boot_gpu [--cache-dir DIR] [--game [--profile ultra|high|low|deck]]
//! ```
//!
//! `--game` replaces the menu tier with `boot_probe --game`'s frame (the hull close-up with the
//! profile, sky, clouds and volumetric fog), extracted from an ECS world and warmed like the
//! windowed runtime's `GameFrameWarm` (`warm_renderer` + `warm_extracted` on a Map-tier worker),
//! then draws that frame once at 1920×1080 with the warmed renderer and reports the pipelines
//! that frame still created (`after_warm_pipelines`, must be 0) and the slowest creation sites.
//!
//! Steps, each marked on the boot timeline (milliseconds since process start): `device` (the
//! adapter, the device and the persistent pipeline cache from `--cache-dir`, if given), `splash_done` (the
//! splash recorded at 1920×1080 into an offscreen target and finished on the GPU: the work the
//! first present waits for), then the warm queue's boot tier (the canonical lit frame) and menu
//! tier (the hull close-up through the lab path) on worker threads. Saves the cache and prints
//! `BOOT_GPU {json}`.
//!
//! `--late-map MB --mode inline|warm` (lane v018-stalls) is the DO27 first-scene stall without a
//! window: after the boot tier, `--game`'s frame plus `MB` MB of distinct map meshes
//! ([`boot_probe::late_map`]) appears on a "menu" that showed no scene. `inline` draws its first
//! 1920×1080 frame on this thread (v0.1.6: the frame compiles and uploads everything), `warm`
//! does what v0.1.8's new-scene warm does (the renderer warms the frame on a Map-tier worker
//! while this thread keeps presenting a cleared 1920×1080 target every ~8 ms, then draws). The
//! report's `late_map` block has the main thread's longest tick, ticks over 250 ms, the map's
//! first frame (ms, pipelines, upload MB) and when the map was drawn. Use a fresh `--cache-dir`
//! and `__GL_SHADER_DISK_CACHE=0` for a cold run.
//!
//! `--mode auto|prepare` (ADR-7883) drive the window path's own `GameFrameWarm` through
//! `GameFrameWarm::host_tick` instead: `auto` is the size-threshold new-scene warm alone (a host
//! that never prepares), `prepare` also calls `SceneReadiness::prepare` once the map is spawned
//! and reports its `SceneReady`. `--late-map-split A,B` (with `auto` or `prepare`) spawns the map
//! in two batches, `A` MB at tick 3 and `B` MB more at tick 6 (DO27 tester A1's order); the
//! prepare is called after the second batch.
//!
//! `--load-budget MIB[,PIPELINES]` (ADR-7884) draws the map's frames under a deferring
//! `FrameLoadBudget` (MIB MiB of mesh uploads and PIPELINES frame-thread graph pipelines per
//! frame); the report's `late_map.load_budget` block has the budget, the largest frame upload and
//! the frame on which nothing was deferred any more.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use regia_render::{BootSplash, PipelineWarmQueue, RenderDevice, SplashPhase, WarmTier};

fn main() -> ExitCode {
    regia_render::boot::mark_named("main");
    let args: Vec<String> = std::env::args().collect();
    let cache_dir = args
        .iter()
        .position(|a| a == "--cache-dir")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let game = args.iter().any(|a| a == "--game").then(|| {
        match args
            .iter()
            .position(|a| a == "--profile")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
        {
            Some("high") => regia_render::RenderProfilePreset::DesktopHigh,
            Some("low") => regia_render::RenderProfilePreset::DesktopLow,
            Some("deck") => regia_render::RenderProfilePreset::Deck,
            _ => regia_render::RenderProfilePreset::DesktopUltra,
        }
    });
    let late_map = args
        .iter()
        .position(|a| a == "--late-map")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u32>().ok());
    let mode = args
        .iter()
        .position(|a| a == "--mode")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default();
    let split = args
        .iter()
        .position(|a| a == "--late-map-split")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some(vec![a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?])
        });
    if mode == "auto" || mode == "prepare" {
        let batches = split.or_else(|| late_map.map(|mb| vec![mb]));
        let Some(batches) = batches else {
            eprintln!("boot_gpu: --mode auto|prepare needs --late-map MB or --late-map-split A,B");
            return ExitCode::FAILURE;
        };
        let profile = game.unwrap_or(regia_render::RenderProfilePreset::DesktopUltra);
        return match run_late_map_driven(cache_dir, profile, &batches, mode == "prepare") {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("boot_gpu: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(map_mb) = late_map {
        let warm = mode == "warm";
        let profile = game.unwrap_or(regia_render::RenderProfilePreset::DesktopUltra);
        let budget = match args.iter().position(|a| a == "--load-budget") {
            None => None,
            Some(i) => match args.get(i + 1).and_then(|v| parse_load_budget(v)) {
                Some(budget) => Some(budget),
                None => {
                    eprintln!("boot_gpu: --load-budget takes MIB[,PIPELINES] (positive integers)");
                    return ExitCode::FAILURE;
                }
            },
        };
        return match run_late_map(cache_dir, profile, map_mb, warm, budget) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("boot_gpu: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match run(cache_dir, game) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("boot_gpu: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `--load-budget MIB[,PIPELINES]`: a deferring budget of MIB MiB of mesh uploads (and PIPELINES
/// frame-thread graph pipelines) per frame.
fn parse_load_budget(value: &str) -> Option<regia_render::FrameLoadBudget> {
    let mut parts = value.split(',');
    let mib = parts.next()?.trim().parse::<u64>().ok().filter(|&m| m > 0)?;
    let pipelines = match parts.next() {
        None => None,
        Some(p) => Some(p.trim().parse::<u32>().ok().filter(|&n| n > 0)?),
    };
    if parts.next().is_some() {
        return None;
    }
    Some(regia_render::FrameLoadBudget {
        upload_bytes_per_frame: Some(mib << 20),
        new_pipelines_per_frame: pipelines,
        late: regia_render::LateDraw::Defer,
    })
}

/// `boot_probe --game`'s frame, extracted from an ECS world without a window.
fn game_frame(profile: regia_render::RenderProfilePreset) -> Result<regia_render::ExtractedFrame, String> {
    game_frame_of(profile, &regia_render::hard_surface::hull_closeup_scene())
}

/// `scene` with `boot_probe --game`'s frame configuration, extracted from an ECS world.
fn game_frame_of(
    profile: regia_render::RenderProfilePreset,
    scene: &regia_render::LabScene,
) -> Result<regia_render::ExtractedFrame, String> {
    use regia::ecs::ecs::system::RunSystemOnce;
    let mut world = regia::ecs::ecs::world::World::new();
    world.insert_resource(regia_render::RenderSettings::from_profile(profile));
    world.insert_resource(regia_render::TimeOfDay::default());
    world.insert_resource(regia_render::VolumetricFogSettings {
        density: 0.002,
        ..Default::default()
    });
    world.insert_resource(regia_render::CloudSettings {
        coverage: 0.4,
        ..Default::default()
    });
    world.init_resource::<regia_render::ExtractedFrame>();
    regia_render::spawn_lab_scene(&mut world, scene);
    world
        .run_system_once(regia_render::extract_frame)
        .map_err(|e| e.to_string())?;
    let frame = world.resource::<regia_render::ExtractedFrame>().clone();
    if frame.window_views().is_empty() {
        return Err("the game frame has no window view".into());
    }
    Ok(frame)
}

fn run(cache_dir: Option<PathBuf>, game: Option<regia_render::RenderProfilePreset>) -> Result<(), String> {
    regia_render::pipeline_cache::set_directory(cache_dir);
    let device = RenderDevice::headless().map_err(|e| e.to_string())?;
    regia_render::boot::mark_named("device");

    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let (width, height) = (1920, 1080);
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("boot_gpu splash target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut splash = BootSplash::new()
        .with_image(boot_probe::logo(384, 192))
        .with_text("REGIA ENGINE");
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("boot_gpu splash"),
        });
    splash.record(
        &device,
        &mut encoder,
        &view,
        format,
        (width, height),
        SplashPhase::Showing { opacity: 1.0 },
    );
    device.queue.submit([encoder.finish()]);
    device.wait_idle().map_err(|e| e.to_string())?;
    regia_render::boot::mark_named("splash_done");

    let queue = PipelineWarmQueue::default();
    queue.push(
        WarmTier::Boot,
        "canonical frame",
        regia_render::pipeline_warm::frame_job(regia_render::pipeline_warm::canonical_frame(), format),
    );
    let game_frame = game.map(game_frame).transpose()?;
    // The game frame warms in the format `render_offscreen` draws (its final pass included).
    let game_format = wgpu::TextureFormat::Rgba8Unorm;
    let game_warm = game_frame.clone().map(|frame| {
        queue.warm_renderer(
            WarmTier::Map,
            "game frame",
            regia_render::FrameRenderer::new(),
            move |renderer, device| {
                renderer
                    .warm_extracted(device, &frame, game_format, true)
                    .map_err(|e| e.to_string())
            },
        )
    });
    if game_warm.is_none() {
        queue.push(
            WarmTier::Menu,
            "hull close-up",
            Box::new(|device: &RenderDevice| {
                regia_render::render_lab_scene(device, &regia_render::hard_surface::hull_closeup_scene())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }),
        );
    }
    let started = Instant::now();
    queue.start(&device);
    let timeout = Duration::from_secs(300);
    if queue.wait_tier(WarmTier::Boot, timeout) {
        regia_render::boot::mark_named("boot_tier_ready");
    }
    if queue.wait_tier(WarmTier::Map, timeout) {
        regia_render::boot::mark_named(if game_warm.is_some() {
            "game_warm_done"
        } else {
            "menu_tier_ready"
        });
    }
    let warm_ms = started.elapsed().as_secs_f64() * 1000.0;
    // The first game frame at full size with the warmed renderer: what it still compiles.
    let mut game_report = serde_json::Value::Null;
    // The warmed renderer lives on while the background builds its deferred variants (a
    // dropped renderer's variants are no longer wanted).
    let mut kept = None;
    if let (Some(handle), Some(frame)) = (&game_warm, &game_frame)
        && let Some(warmed) = handle.take()
    {
        let mut after = None;
        let mut first_ms = None;
        let mut outside_sites = Vec::new();
        if let Some(mut renderer) = warmed.renderer {
            let (view_index, _) = frame.window_views()[0];
            let scene = frame.frame_scene(view_index, 1920, 1080);
            // Pipelines created outside warm jobs: this frame's own (deferred variants the
            // background builds meanwhile count for the optional tier, not here).
            let before = queue.progress().pipelines_outside;
            let t = Instant::now();
            renderer
                .render_offscreen(&device, &scene)
                .map_err(|e| e.to_string())?;
            first_ms = Some(t.elapsed().as_secs_f32() * 1000.0);
            let progress = queue.progress();
            after = Some(progress.pipelines_outside - before);
            outside_sites = progress.outside_sites;
            kept = Some(renderer);
        }
        game_report = serde_json::json!({
            "ms": warmed.ms, "pipelines": warmed.pipelines, "error": warmed.error,
            "after_warm_pipelines": after, "first_frame_ms": first_ms,
            "outside_sites": outside_sites.iter().map(|(s, n)| format!("{s} x{n}")).collect::<Vec<_>>(),
        });
    }
    // Background work finishes before the cache is saved: optional-tier jobs and the deferred
    // pipeline variants `take()` released (so a shipped cache holds them too).
    let optional_ready = queue.wait_tier(WarmTier::Optional, timeout);
    regia_render::pipeline_warm::release_deferred();
    let deferred_start = Instant::now();
    while regia_render::pipeline_warm::deferred_pending() > 0 && deferred_start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(5));
    }
    let deferred_ms = deferred_start.elapsed().as_secs_f64() * 1000.0;
    drop(kept);
    let all_ms = started.elapsed().as_secs_f64() * 1000.0;
    let progress = queue.progress();
    // Joins the workers and releases their device clone: with `device` dropped at the end of
    // this function the Vulkan device is destroyed before the process's exit handlers run.
    queue.shutdown();
    let saved = regia_render::pipeline_cache::save_installed().and_then(Result::ok);
    let cache = regia_render::pipeline_cache::status();
    let report = serde_json::json!({
        "adapter": device.adapter.get_info().name,
        "driver": device.adapter.get_info().driver_info,
        "marks": regia_render::boot::timeline()
            .into_iter()
            .map(|(m, ms)| serde_json::json!({ "mark": m, "ms": (ms * 10.0).round() / 10.0 }))
            .collect::<Vec<_>>(),
        "warm_ms": (warm_ms * 10.0).round() / 10.0,
        "all_tiers_ms": (all_ms * 10.0).round() / 10.0,
        "optional_ready": optional_ready,
        "deferred_ms": (deferred_ms * 10.0).round() / 10.0,
        "deferred_built": progress.deferred_built,
        "game": game_report,
        "pipelines_created": regia_render::pipeline_cache::pipelines_created(),
        // `REGIA_PIPELINE_TIMING=1`: CPU totals per phase, summed over the warm threads.
        "phase_totals": regia_render::pipeline_timing_summary().map(|phases| {
            phases
                .into_iter()
                .map(|(phase, count, ms)| serde_json::json!({ "phase": phase, "count": count, "ms": (ms * 10.0).round() / 10.0 }))
                .collect::<Vec<_>>()
        }),
        "slowest_sites": regia_render::pipeline_warm::warm_site_costs(24)
            .into_iter()
            .map(|c| format!("{} x{} {:.0} ms (max {:.0})", c.site, c.pipelines, c.total_ms, c.max_ms))
            .collect::<Vec<_>>(),
        "tiers": progress.tiers.iter().filter(|t| t.jobs_total > 0).map(|t| serde_json::json!({
            "tier": t.tier, "failed": t.jobs_failed, "pipelines": t.pipelines,
            "busy_ms": t.busy_ms, "ready_after_ms": t.ready_after_ms,
        })).collect::<Vec<_>>(),
        "last_error": progress.last_error,
        "cache": {
            "enabled": cache.enabled,
            "load": cache.load.map(|l| format!("{l:?}")),
            "shipped": cache.shipped.map(|l| format!("{l:?}")),
            "load_ms": cache.load_ms,
            "saved_bytes": saved,
        },
    });
    // The bin's one line of output, for scripts.
    println!("BOOT_GPU {report}");
    Ok(())
}

/// One "menu" tick: a 1920×1080 clear submitted like a UI-only present, then a non-blocking poll.
fn menu_tick(device: &RenderDevice, view: &wgpu::TextureView) {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("boot_gpu menu"),
        });
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("boot_gpu menu clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    device.queue.submit([encoder.finish()]);
    let _ = device.device.poll(wgpu::PollType::Poll);
}

/// `--late-map`: the DO27 first-scene stall without a window (see the module docs).
fn run_late_map(
    cache_dir: Option<PathBuf>,
    profile: regia_render::RenderProfilePreset,
    map_mb: u32,
    warm: bool,
    budget: Option<regia_render::FrameLoadBudget>,
) -> Result<(), String> {
    regia_render::pipeline_cache::set_directory(cache_dir);
    let device = RenderDevice::headless().map_err(|e| e.to_string())?;
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let menu_target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("boot_gpu menu target"),
        size: wgpu::Extent3d {
            width: 1920,
            height: 1080,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let menu_view = menu_target.create_view(&wgpu::TextureViewDescriptor::default());
    let queue = PipelineWarmQueue::default();
    queue.push(
        WarmTier::Boot,
        "canonical frame",
        regia_render::pipeline_warm::frame_job(regia_render::pipeline_warm::canonical_frame(), format),
    );
    queue.start(&device);
    let timeout = Duration::from_secs(300);
    queue.wait_tier(WarmTier::Boot, timeout);
    let phase = Instant::now();
    let note = |what: &str| {
        eprintln!(
            "boot_gpu: {what} at +{:.0} ms",
            phase.elapsed().as_secs_f64() * 1000.0
        )
    };
    note("boot tier ready");
    // The map becomes resident behind the menu.
    let map = boot_probe::late_map(map_mb);
    note("map built");
    let frame = game_frame_of(profile, &map)?;
    drop(map);
    note("map extracted");
    let mesh_count = frame.meshes.len();
    let game_format = wgpu::TextureFormat::Rgba8Unorm;
    let (view_index, _) = frame.window_views()[0];
    let started = Instant::now();
    let mut last = started;
    let (mut max_tick_ms, mut over_250, mut ticks) = (0.0f32, 0u32, 0u32);
    let mut tick_done = |last: &mut Instant| {
        let now = Instant::now();
        let ms = now.duration_since(*last).as_secs_f32() * 1000.0;
        *last = now;
        max_tick_ms = max_tick_ms.max(ms);
        over_250 += u32::from(ms > 250.0);
        ticks += 1;
    };
    let mut renderer = regia_render::FrameRenderer::new();
    if let Some(budget) = budget {
        renderer.set_load_budget(budget);
    }
    let mut warm_report = serde_json::Value::Null;
    if warm {
        let shared = frame.clone();
        let handle = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |r, device| {
            r.warm_extracted_at(device, &shared, game_format, true, Some((1920, 1080)))
                .map_err(|e| e.to_string())
        });
        let back = loop {
            menu_tick(&device, &menu_view);
            tick_done(&mut last);
            if let Some(done) = handle.take() {
                break done;
            }
            if started.elapsed() > timeout {
                return Err("the map warm did not come back".into());
            }
            std::thread::sleep(Duration::from_millis(8));
        };
        note("warm back");
        warm_report = serde_json::json!({ "ms": back.ms, "pipelines": back.pipelines, "error": back.error });
        renderer = back.renderer.ok_or("the warm lost the renderer")?;
    }
    // The map's first frame on this thread, drawn like the window path (`render_into` into a
    // window-sized target: mesh SDF bricks bake on the task pool, as in a game; the offscreen
    // capture path would bake them synchronously).
    let window = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("boot_gpu window stand-in"),
        size: wgpu::Extent3d {
            width: 1920,
            height: 1080,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: game_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let window_view = window.create_view(&wgpu::TextureViewDescriptor::default());
    let target = || {
        regia_render::FrameTarget::rect(&window_view, game_format, 1920, 1080, [0, 0, 1920, 1080], false)
            .with_color(Some(&window))
    };
    let before = regia_render::frame_path_counters();
    let t = Instant::now();
    let scene = frame.frame_scene(view_index, 1920, 1080);
    renderer
        .render_into(&device, &scene, target())
        .map_err(|e| e.to_string())?;
    let first_ms = t.elapsed().as_secs_f32() * 1000.0;
    note("first map frame drawn");
    let first = regia_render::frame_path_counters().since(before);
    tick_done(&mut last);
    let drawn_after_ms = started.elapsed().as_secs_f32() * 1000.0;
    let mut max_upload = first.upload_bytes;
    let mut resident_frame = (renderer.load_budget_report().deferred_meshes == 0).then_some(1u32);
    // A few more frames, like a game that keeps drawing the map.
    for frame in 2..=31u32 {
        let before = regia_render::frame_path_counters();
        renderer
            .render_into(&device, &scene, target())
            .map_err(|e| e.to_string())?;
        max_upload = max_upload.max(regia_render::frame_path_counters().since(before).upload_bytes);
        if resident_frame.is_none() && renderer.load_budget_report().deferred_meshes == 0 {
            resident_frame = Some(frame);
        }
        tick_done(&mut last);
    }
    let budget_report = budget.map(|b| {
        serde_json::json!({
            "upload_mib_per_frame": b.upload_bytes_per_frame.map(|v| v >> 20),
            "new_pipelines_per_frame": b.new_pipelines_per_frame,
            "max_frame_upload_mb": max_upload as f64 / 1_048_576.0,
            "resident_on_frame": resident_frame,
            "bursts": renderer.load_budget_report().bursts,
        })
    });
    device.wait_idle().map_err(|e| e.to_string())?;
    queue.shutdown();
    let report = serde_json::json!({
        "adapter": device.adapter.get_info().name,
        "driver": device.adapter.get_info().driver_info,
        "late_map": {
            "mode": if warm { "warm" } else { "inline" },
            "map_mb": map_mb,
            "meshes": mesh_count,
            "main_ticks": ticks,
            "max_main_tick_ms": max_tick_ms,
            "main_ticks_over_250ms": over_250,
            "first_frame": {
                "ms": first_ms,
                "pipelines": first.pipelines,
                "upload_mb": first.upload_bytes as f64 / 1_048_576.0,
                "buffers": first.buffers,
                "blocking_polls": first.blocking_polls,
            },
            "drawn_after_ms": drawn_after_ms,
            "warm": warm_report,
            "load_budget": budget_report,
        },
        "pipelines_created": regia_render::pipeline_cache::pipelines_created(),
    });
    println!("BOOT_GPU {report}");
    Ok(())
}

/// `--mode auto|prepare` (ADR-7883): the late map through the window path's `GameFrameWarm`
/// (`host_tick`), spawned in `batches` (MB each, at ticks 3, 6, ...); `prepare` calls
/// `SceneReadiness::prepare` on the tick the last batch lands.
fn run_late_map_driven(
    cache_dir: Option<PathBuf>,
    profile: regia_render::RenderProfilePreset,
    batches: &[u32],
    prepare: bool,
) -> Result<(), String> {
    regia_render::pipeline_cache::set_directory(cache_dir);
    let device = RenderDevice::headless().map_err(|e| e.to_string())?;
    let size = (1920u32, 1080u32);
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = |label| {
        device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    };
    let window = target("boot_gpu window stand-in");
    let window_view = window.create_view(&wgpu::TextureViewDescriptor::default());
    let menu_target = target("boot_gpu menu target");
    let menu_view = menu_target.create_view(&wgpu::TextureViewDescriptor::default());
    let queue = PipelineWarmQueue::default();
    queue.push(
        WarmTier::Boot,
        "canonical frame",
        regia_render::pipeline_warm::frame_job(regia_render::pipeline_warm::canonical_frame(), format),
    );
    queue.start(&device);
    let timeout = Duration::from_secs(600);
    queue.wait_tier(WarmTier::Boot, timeout);
    // The menu: the game frame's camera without meshes; then each batch adds its meshes.
    let mut frames = Vec::new();
    let mut spawned: Option<regia_render::ExtractedFrame> = None;
    for &mb in batches {
        let batch = game_frame_of(profile, &boot_probe::late_map(mb))?;
        let next = match spawned.take() {
            None => batch,
            Some(mut so_far) => {
                so_far.meshes.extend(batch.meshes);
                so_far
            }
        };
        frames.push(next.clone());
        spawned = Some(next);
    }
    let Some(full) = spawned else {
        return Err("no batch".into());
    };
    let mut menu = full.clone();
    menu.meshes.clear();
    let mesh_count = full.meshes.len();
    let (view_index, _) = full.window_views()[0];
    let mut gw = regia_render::GameFrameWarm::default();
    gw.timeout = timeout;
    let mut readiness = regia_render::SceneReadiness::default();
    let mut runtime = regia_render::FrameRenderer::new();
    let started = Instant::now();
    let mut last = started;
    let (mut max_tick_ms, mut over_250, mut ticks) = (0.0f32, 0u32, 0u32);
    let mut first = None;
    let mut ready = None;
    let mut drawn_frames = 0u32;
    let mut tick = 0usize;
    while drawn_frames < 31 {
        if started.elapsed() > timeout {
            return Err("the map never drew".into());
        }
        // Batch k lands on tick 3 (k + 1).
        let landed = (tick / 3).min(frames.len() + 1);
        let frame = if landed == 0 {
            &menu
        } else {
            &frames[landed.min(frames.len()) - 1]
        };
        if prepare && tick == 3 * frames.len() {
            readiness.prepare("late_map");
        }
        tick += 1;
        let t = Instant::now();
        let before = regia_render::frame_path_counters();
        let decided = gw.host_tick(
            &queue,
            &mut runtime,
            frame,
            (format, true, size),
            &device,
            Some(&mut readiness),
        );
        if decided.draw {
            let scene = frame.frame_scene(view_index, size.0, size.1);
            let target = regia_render::FrameTarget::rect(
                &window_view,
                format,
                size.0,
                size.1,
                [0, 0, size.0, size.1],
                false,
            )
            .with_color(Some(&window));
            runtime
                .render_into(&device, &scene, target)
                .map_err(|e| e.to_string())?;
            let counts = regia_render::frame_path_counters().since(before);
            if let Some(edge) = gw.host_presented(decided, counts.pipelines, Some(&mut readiness)) {
                ready = Some(edge);
            }
            if first.is_none() && frame.meshes.len() == mesh_count {
                first = Some((
                    t.elapsed().as_secs_f32() * 1000.0,
                    counts,
                    decided.pending_meshes,
                    started.elapsed().as_secs_f32() * 1000.0,
                ));
            }
            if frame.meshes.len() == mesh_count {
                drawn_frames += 1;
            }
        } else {
            menu_tick(&device, &menu_view);
            std::thread::sleep(Duration::from_millis(8));
        }
        let now = Instant::now();
        let ms = now.duration_since(last).as_secs_f32() * 1000.0;
        last = now;
        max_tick_ms = max_tick_ms.max(ms);
        over_250 += u32::from(ms > 250.0);
        ticks += 1;
    }
    device.wait_idle().map_err(|e| e.to_string())?;
    queue.shutdown();
    let (first_ms, counts, pending, drawn_after_ms) = first.ok_or("the full map never drew")?;
    let report = serde_json::json!({
        "adapter": device.adapter.get_info().name,
        "driver": device.adapter.get_info().driver_info,
        "late_map": {
            "mode": if prepare { "prepare" } else { "auto" },
            "batches_mb": batches,
            "meshes": mesh_count,
            "main_ticks": ticks,
            "main_longest_ms": max_tick_ms,
            "main_ticks_over_250ms": over_250,
            "first_frame": {
                "ms": first_ms,
                "pipelines": counts.pipelines,
                "upload_mb": counts.upload_bytes as f64 / 1_048_576.0,
                "pending_meshes": pending,
                "buffers": counts.buffers,
                "blocking_polls": counts.blocking_polls,
            },
            "drawn_after_ms": drawn_after_ms,
            "warm": gw.last_report().map(|r| serde_json::json!({
                "ms": r.ms, "pipelines": r.pipelines, "mb_uploaded": r.mb_uploaded,
                "complete": r.complete, "map": r.map_id, "timed_out": r.timed_out,
            })),
            "scene_warms": gw.scene_warms(),
            "scene_ready": ready.map(|r| serde_json::json!({
                "warms": r.warms, "pipelines": r.pipelines, "uploaded_mb": r.uploaded_mb,
                "first_present_ms": r.first_present_ms, "timed_out": r.timed_out,
            })),
        },
        "pipelines_created": regia_render::pipeline_cache::pipelines_created(),
    });
    println!("BOOT_GPU {report}");
    Ok(())
}
