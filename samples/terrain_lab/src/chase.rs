// SPDX-License-Identifier: MIT-0
//! Terrain perf harness: DO27's chase bench on the DO27-shaped stand-ins (`--do27-chase`).
//!
//! A vehicle box drives across the stand-in map at 15 m/s (60 Hz steps) with a chase camera
//! 9 m behind it, through [`TerrainSharedDraw`] inside [`FrameRenderer`] on Desktop High at
//! native scale — the configuration DO27's shell renders. Two frames stay in flight, like a
//! presenting game. Prints, per variant, the frame-time distribution, the per-pass GPU times
//! and the terrain counters (cascade draws, selected and drawn nodes, node writes).
//!
//! ```text
//! cargo run -p terrain_lab -- --do27-chase                      # both maps, every variant, 2560×1440
//! cargo run -p terrain_lab -- --do27-chase --map switchbacks --variants base --frames 60 --size 640x360
//! ```
//!
//! Variants: `base`; `held` (the camera parks at the start: the floor without redraws);
//! `relief-off` (the photoreal path without strata and self-shadow); `no-terrain-shadow`
//! (terrain skipped in the sun cascades, diagnostic only); `no-vehicle` (no mesh caster).
//!
//! `--strafe` replaces the chase with a 60-frame sideways strafe at 15 m/s (the cascade
//! redraw test's motion). `--frames-csv <path>` writes one row per measured frame.
//!
//! `scatter` adds the planet's stand-in rock scatter (`samples/terrain_lab/scatter/*.scatter.ron`, byte copies of the lookdev ones,
//! class densities × `--scatter-density`, default 1.3 ≈ 5 k rocks emitted on the Switchbacks
//! chase; `--scatter-quiet` waits for the GPU before timing the scatter CPU work),
//! seated on the drawn surface with the contact map bound, and prints the scatter CPU time
//! per frame: stream update, collect + seating, contact map. `scatter-held` is `scatter` with
//! the camera parked (rocks at their fade distance must leave every shadow page clean).
//!
//! With `REGIA_SPAN_REPORT=N` set, the run ends with the N heaviest spans over every measured
//! frame of every variant (`docs/perf-log.md`).

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;

use regia_render::{
    AtmosphereParameters, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh, OutdoorParams,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedFrameInfo, SharedPass, SharedPassDraw,
    SharedUpload,
};
use regia_terrain::{
    ContactMap, RockPalette, ScatterField, ScatterSet, ScatterSource, ScatterView, TerrainDrawStats,
    TerrainMap, TerrainRelief, TerrainSharedDraw, standin_library,
};

/// The lookdev stand-in scatter sets (sizes in metres).
const MARS_SCATTER: &str = include_str!("../scatter/mars.scatter.ron");
const EUROPA_SCATTER: &str = include_str!("../scatter/europa.scatter.ron");

/// The route's scatter field, class densities scaled by `density`.
fn scatter_field(planet: Planet, map: &TerrainMap, density: f32) -> Result<ScatterField, String> {
    let (text, palette) = match planet {
        Planet::Mars => (MARS_SCATTER, RockPalette::Mars),
        Planet::Europa => (EUROPA_SCATTER, RockPalette::Europa),
    };
    let mut set = ScatterSet::from_ron(text, None).map_err(|e| e.to_string())?;
    for class in &mut set.classes {
        class.density = (class.density * density).min(64.0);
    }
    let library = standin_library(&set, palette);
    ScatterField::new(&set, ScatterSource::new(Arc::new(map.clone())), &library, 0x5ca7)
        .map_err(|e| e.to_string())
}

use crate::do27_look::{Planet, build, planet_map};

/// Vehicle speed, m/s (DO27's chase bench).
const SPEED: f32 = 15.0;
/// Simulation step per frame, seconds.
const DT: f32 = 1.0 / 60.0;
/// Chase boom: distance behind the vehicle and height above it, metres.
const BOOM_BACK: f32 = 9.0;
const BOOM_UP: f32 = 3.5;

/// One stand-in map and its route.
struct Route {
    name: &'static str,
    planet: Planet,
    /// Start XZ and the point the vehicle first heads toward.
    start: [f32; 2],
    toward: [f32; 2],
    /// Heading weave amplitude (radians) — the vehicle follows a winding road.
    weave: f32,
}

const ROUTES: [Route; 2] = [
    // Across the ice plain toward the double ridge (gentle relief).
    Route {
        name: "bore_twelve",
        planet: Planet::Europa,
        start: [96.0, -234.0],
        toward: [200.0, -130.0],
        weave: 0.20,
    },
    // Up the valley floor between mesas, toward a stepped flank (canyon relief).
    Route {
        name: "switchbacks",
        planet: Planet::Mars,
        start: [185.0, -265.0],
        toward: [330.0, -60.0],
        weave: 0.35,
    },
];

/// Wraps the terrain draw so a variant can drop it from the cascades.
struct Probe {
    inner: TerrainSharedDraw,
    skip_shadow: bool,
}

impl SharedPassDraw for Probe {
    fn prepare(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>) {
        self.inner.prepare(info, upload);
    }

    fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, kind: SharedPass) {
        if self.skip_shadow && matches!(kind, SharedPass::Shadow { .. }) {
            return;
        }
        self.inner.draw(pass, kind);
    }

    fn set_engine_csm(&mut self, views: &[wgpu::TextureView]) {
        self.inner.set_engine_csm(views);
    }

    fn set_engine_ao(&mut self, ao: Option<&wgpu::TextureView>) {
        self.inner.set_engine_ao(ao);
    }

    fn set_engine_sky_irradiance(&mut self, sky: Option<(&wgpu::TextureView, &wgpu::TextureView)>) {
        self.inner.set_engine_sky_irradiance(sky);
    }

    fn finish_prepare(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>) {
        self.inner.finish_prepare(info, upload);
    }

    fn shadow_key(&self) -> Option<u64> {
        self.inner.shadow_key()
    }

    fn cascade_shadow_key(&self, index: u32) -> Option<u64> {
        self.inner.cascade_shadow_key(index)
    }

    fn draw_shadow_region<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, index: u32) {
        if !self.skip_shadow {
            self.inner.draw_shadow_region(pass, index);
        }
    }

    fn content_key(&self) -> u64 {
        self.inner.content_key()
    }
}

/// Per-frame record.
#[derive(Clone, Copy, Default)]
struct Sample {
    wall_ms: f32,
    encode_ms: f32,
    cpu_ms: f32,
    scene_ms: f32,
    shadows_ms: f32,
    prepass_ms: f32,
    forward_ms: f32,
    cascade_draws: u64,
    region_draws: u64,
    full_cascades: u32,
    nodes: u32,
    camera_nodes: u32,
    cascade_nodes: [u32; 4],
    writes: u32,
    prepare_ms: f32,
    /// Scatter CPU: stream update, collect (incl. seating), contact map; ms.
    scatter_ms: [f32; 3],
    rocks: u32,
    /// Seats per frame: slack hits, cached after a re-check, recomputed.
    seat: [u32; 3],
    /// Mesh-dirtied sun shadow pages per cascade (cascades not redrawn whole).
    mesh_pages: [u32; 4],
    /// Caster events per cascade: moved, appeared, disappeared.
    caster_events: [[u32; 3]; 4],
}

/// `--do27-chase`: see the module docs.
pub fn run(args: &[String]) {
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let map_filter = value("--map");
    let frames: u32 = value("--frames").and_then(|v| v.parse().ok()).unwrap_or(180);
    let (width, height) = value("--size")
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((2560u32, 1440u32));
    let variants: Vec<String> = value("--variants")
        .unwrap_or_else(|| "base,held,no-vehicle,relief-off,no-terrain-shadow".into())
        .split(',')
        .map(str::to_owned)
        .collect();
    let strafe = args.iter().any(|a| a == "--strafe");
    let scatter_density: f32 = value("--scatter-density")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.3);
    let scatter_quiet = args.iter().any(|a| a == "--scatter-quiet");
    let csv = value("--frames-csv");
    let mut spans = match regia_core::profiling::RunTotals::install_from_env() {
        Ok(spans) => spans,
        Err(err) => {
            eprintln!("terrain_lab: span report logging: {err}");
            None
        }
    };
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("terrain_lab: no GPU: {err}");
            return;
        }
    };
    println!("adapter: {:?}", device.adapter.get_info().name);
    let mut csv_rows = String::from(
        "map,variant,frame,wall_ms,scene_ms,shadows_ms,prepass_ms,forward_ms,cascade_draws,full_cascades,nodes,camera_nodes,c0,c1,c2,c3,writes\n",
    );
    for route in &ROUTES {
        if map_filter.as_deref().is_some_and(|m| m != route.name) {
            continue;
        }
        let started = Instant::now();
        let map = planet_map(route.planet);
        println!(
            "{}: {}² heightfield in {:.1}s",
            route.name,
            map.width(),
            started.elapsed().as_secs_f32()
        );
        for variant in &variants {
            match chase(
                &device,
                &map,
                route,
                variant,
                frames,
                width,
                height,
                strafe,
                scatter_density,
                scatter_quiet,
                &mut spans,
            ) {
                Ok(samples) => {
                    println!("{}", summary(route.name, variant, &samples));
                    for (i, s) in samples.iter().enumerate() {
                        csv_rows.push_str(&format!(
                            "{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{},{},{},{},{}\n",
                            route.name,
                            variant,
                            i,
                            s.wall_ms,
                            s.scene_ms,
                            s.shadows_ms,
                            s.prepass_ms,
                            s.forward_ms,
                            s.cascade_draws,
                            s.full_cascades,
                            s.nodes,
                            s.camera_nodes,
                            s.cascade_nodes[0],
                            s.cascade_nodes[1],
                            s.cascade_nodes[2],
                            s.cascade_nodes[3],
                            s.writes
                        ));
                    }
                }
                Err(err) => eprintln!("terrain_lab: {} {variant}: {err}", route.name),
            }
        }
    }
    if let Some(path) = csv {
        match std::fs::File::create(&path).and_then(|mut f| f.write_all(csv_rows.as_bytes())) {
            Ok(()) => println!("wrote {path}"),
            Err(err) => eprintln!("terrain_lab: cannot write {path}: {err}"),
        }
    }
    if let Some((totals, rows)) = &spans {
        print!("{}", totals.report(*rows));
    }
}

/// Vehicle pose (XZ, heading radians) after `frame` steps.
fn vehicle_at(route: &Route, frame: u32, strafe: bool) -> ([f32; 2], f32) {
    let base = (route.toward[1] - route.start[1]).atan2(route.toward[0] - route.start[0]);
    if strafe {
        // Sideways at full speed, heading fixed: pure camera translation.
        let s = frame as f32 * SPEED * DT;
        let side = base + std::f32::consts::FRAC_PI_2;
        return (
            [route.start[0] + side.cos() * s, route.start[1] + side.sin() * s],
            base,
        );
    }
    // Integrate a winding heading so the path is a smooth road.
    let mut p = route.start;
    let mut heading = base;
    for i in 0..frame {
        let t = i as f32 * DT;
        heading = base + route.weave * (t * 0.9).sin();
        p[0] += heading.cos() * SPEED * DT;
        p[1] += heading.sin() * SPEED * DT;
    }
    (p, heading)
}

/// Column-major model matrix: yaw about +Y then translate.
fn model(pos: [f32; 3], heading: f32) -> [[f32; 4]; 4] {
    let (s, c) = heading.sin_cos();
    [
        [c, 0.0, s, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [-s, 0.0, c, 0.0],
        [pos[0], pos[1], pos[2], 1.0],
    ]
}

/// Chase camera eye and target for the vehicle pose.
fn camera(map: &TerrainMap, p: [f32; 2], heading: f32, strafe: bool) -> ([f32; 3], [f32; 3]) {
    let ground = map.sample_bilinear(p[0], p[1]);
    let (dx, dz) = (heading.cos(), heading.sin());
    if strafe {
        // Look along the heading from beside the strafe line.
        let eye = [p[0] - dx * BOOM_BACK, 0.0, p[1] - dz * BOOM_BACK];
        let eye_ground = map.sample_bilinear(eye[0], eye[2]);
        let eye = [eye[0], (ground + BOOM_UP).max(eye_ground + 1.5), eye[2]];
        return (eye, [p[0] + dx * 30.0, ground + 1.2, p[1] + dz * 30.0]);
    }
    let ex = p[0] - dx * BOOM_BACK;
    let ez = p[1] - dz * BOOM_BACK;
    let eye_ground = map.sample_bilinear(ex, ez);
    let eye = [ex, (ground + BOOM_UP).max(eye_ground + 1.5), ez];
    (eye, [p[0], ground + 1.2, p[1]])
}

#[allow(clippy::too_many_arguments)] // one bench configuration
fn chase(
    device: &RenderDevice,
    map: &TerrainMap,
    route: &Route,
    variant: &str,
    frames: u32,
    width: u32,
    height: u32,
    strafe: bool,
    scatter_density: f32,
    scatter_quiet: bool,
    spans: &mut Option<(regia_core::profiling::RunTotals, usize)>,
) -> Result<Vec<Sample>, String> {
    let relief = if variant == "relief-off" {
        TerrainRelief::off()
    } else {
        TerrainRelief::default()
    };
    let shot = crate::do27_look::chase_shot(route.planet, route.start, route.toward);
    let (mut draw, mut scene, post, preset) = build(device, map, &shot, false, relief, width, height)?;
    let stats: Arc<TerrainDrawStats> = draw.stats();
    let mut scatter = if variant == "scatter" || variant == "scatter-held" {
        let mut field = scatter_field(route.planet, map, scatter_density)?;
        field.set_parallel_collect(std::env::var_os("CHASE_SCATTER_SERIAL").is_none());
        if std::env::var_os("CHASE_NO_SEAT").is_none() {
            field.set_rendered_surface(Some(draw.surface()));
        }
        let contact = ContactMap::new(&device.device);
        draw.set_contact_map(Some(&contact));
        Some((field, contact))
    } else {
        None
    };
    // DO27 prewarms at map load, so the first measured frame compiles nothing.
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    let probe = Probe {
        inner: draw,
        skip_shadow: variant == "no-terrain-shadow",
    };
    // The vehicle: a 2.0 × 1.6 × 4.2 m box that casts into every cascade.
    let (verts, indices) = regia_render::box_mesh([1.0, 0.8, 2.1]);
    let verts = regia_render::translate_mesh(&verts, [0.0, 0.8, 0.0]);
    scene.materials = vec![LabMaterial {
        base_color: [0.35, 0.36, 0.38, 1.0],
        roughness: 0.5,
        ..LabMaterial::default()
    }];
    if variant != "no-vehicle" {
        scene.meshes = vec![LabMesh {
            vertices: verts,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }];
    }
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(preset),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(probe) as Box<dyn SharedPassDraw + Send + Sync>));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("chase target"),
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
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let warmup = 20u32;
    let mut stream_totals = [0u32; 3];
    let mut in_flight: VecDeque<wgpu::SubmissionIndex> = VecDeque::new();
    let mut samples = Vec::with_capacity(frames as usize);
    let mut last_draws = renderer.shared_shadow_draws();
    let mut last_regions = renderer.shared_shadow_region_draws();
    for i in 0..warmup + frames {
        let step = if variant == "held" || variant == "scatter-held" || i < warmup {
            0
        } else {
            i - warmup
        };
        let (p, heading) = vehicle_at(route, step, strafe);
        let ground = map.sample_bilinear(p[0], p[1]);
        let (eye, target) = camera(map, p, heading, strafe);
        scene.eye = eye;
        scene.target = target;
        // Scatter CPU, the way a host runs it each frame. `--scatter-quiet` first lets the GPU
        // go idle so a software rasteriser (lavapipe) does not compete for the cores.
        if scatter_quiet && scatter.is_some() {
            let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
        }
        let mut scatter_ms = [0.0f32; 3];
        if let Some((field, contact)) = scatter.as_mut() {
            let t = Instant::now();
            let streamed = field.update(eye);
            scatter_ms[0] = t.elapsed().as_secs_f32() * 1000.0;
            if i >= warmup {
                stream_totals[0] += streamed.sync_tiles;
                stream_totals[1] += streamed.waited_tiles;
                stream_totals[2] += streamed.queued_tiles;
            }
            let t = Instant::now();
            contact.update(&device.queue, field, eye);
            scatter_ms[2] = t.elapsed().as_secs_f32() * 1000.0;
        }
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        if let Some(mesh) = frame.meshes.first_mut() {
            // The box's long axis is +Z; face it along the heading (XZ angle from +X).
            mesh.model = model([p[0], ground, p[1]], std::f32::consts::FRAC_PI_2 - heading);
        }
        let mut rocks = 0;
        let mut seat_counts = [0u32; 3];
        if let Some((field, _)) = scatter.as_ref() {
            let view = ScatterView::look_at(
                eye,
                target,
                frame.fov_y,
                width as f32 / height as f32,
                frame.near,
                frame.far,
            );
            let t = Instant::now();
            let stats = field.collect(&view, &mut frame.meshes);
            scatter_ms[1] = t.elapsed().as_secs_f32() * 1000.0;
            rocks = stats.emitted;
            if std::env::var_os("CHASE_SCATTER_TRACE").is_some() && i >= warmup {
                println!(
                    "frame {} collect {:.3} ms considered {} fast {} checked {} computed {} emitted {}",
                    i - warmup,
                    scatter_ms[1],
                    stats.considered,
                    stats.seat_fast,
                    stats.seat_checked,
                    stats.seat_computed,
                    stats.emitted
                );
            }
            seat_counts[0] += stats.seat_fast;
            seat_counts[1] += stats.seat_checked;
            seat_counts[2] += stats.seat_computed;
        }
        let start = Instant::now();
        renderer
            .render_into(device, &frame, FrameTarget::full(&view, format, width, height))
            .map_err(|err| err.to_string())?;
        if let Some(submission) = renderer.take_submission() {
            in_flight.push_back(submission);
        }
        if in_flight.len() >= 3
            && let Some(oldest) = in_flight.pop_front()
        {
            device
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(oldest),
                    timeout: None,
                })
                .map_err(|err| err.to_string())?;
        }
        let wall = start.elapsed().as_secs_f32() * 1000.0;
        let draws = renderer.shared_shadow_draws();
        let regions = renderer.shared_shadow_region_draws();
        if i >= warmup {
            let gpu = renderer.gpu_timings();
            let cpu = renderer.cpu_timings();
            let report = renderer.shadow_cache_report();
            let mut mesh_pages = [0u32; 4];
            for page in &report.dirty_pages {
                let c = page.cascade as usize;
                if c < 4 && !report.cascade_full_redraw.get(c).copied().unwrap_or(true) {
                    mesh_pages[c] += 1;
                }
            }
            samples.push(Sample {
                wall_ms: wall,
                encode_ms: cpu.encode_ms,
                cpu_ms: cpu.total_ms,
                scene_ms: gpu.scene_ms,
                shadows_ms: gpu.shadows_ms,
                prepass_ms: gpu.prepass_ms,
                forward_ms: gpu.forward_ms,
                cascade_draws: draws - last_draws,
                region_draws: regions - last_regions,
                full_cascades: report.cascade_full_redraw.iter().filter(|f| **f).count() as u32,
                nodes: stats.selected_nodes(),
                camera_nodes: stats.camera_nodes(),
                cascade_nodes: stats.cascade_nodes(),
                writes: stats.last_node_upload().0,
                prepare_ms: stats.last_prepare_ms(),
                scatter_ms,
                rocks,
                seat: seat_counts,
                mesh_pages,
                caster_events: std::array::from_fn(|c| {
                    report.caster_events.get(c).copied().unwrap_or_default()
                }),
            });
        }
        last_draws = draws;
        last_regions = regions;
        if let Some((totals, _)) = spans.as_mut() {
            if i >= warmup {
                totals.end_frame();
            } else {
                totals.skip_frame();
            }
        }
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    if scatter.is_some() {
        println!(
            "  scatter streaming over the run: {} tiles placed on the main thread, {} awaited, {} queued in the background",
            stream_totals[0], stream_totals[1], stream_totals[2]
        );
    }
    Ok(samples)
}

fn percentile(values: &mut [f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let index = ((values.len() - 1) as f32 * p).round() as usize;
    values[index.min(values.len() - 1)]
}

fn mean(values: impl Iterator<Item = f32>) -> f32 {
    let (sum, n) = values.fold((0.0f32, 0u32), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / n as f32 }
}

fn summary(map: &str, variant: &str, samples: &[Sample]) -> String {
    let mut wall: Vec<f32> = samples.iter().map(|s| s.wall_ms).collect();
    let mut scene: Vec<f32> = samples.iter().map(|s| s.scene_ms).collect();
    let mut cpu: Vec<f32> = samples.iter().map(|s| s.cpu_ms).collect();
    let draws: u64 = samples.iter().map(|s| s.cascade_draws).sum();
    let regions: u64 = samples.iter().map(|s| s.region_draws).sum();
    let full: u32 = samples.iter().map(|s| s.full_cascades).sum();
    let writes: u32 = samples.iter().map(|s| s.writes).sum();
    let scatter = if samples.iter().any(|s| s.rocks > 0) {
        let col = |k: usize| samples.iter().map(|s| s.scatter_ms[k]).collect::<Vec<f32>>();
        let total: Vec<f32> = samples.iter().map(|s| s.scatter_ms.iter().sum()).collect();
        let stat = |mut v: Vec<f32>| {
            let p50 = percentile(&mut v, 0.5);
            let p99 = percentile(&mut v, 0.99);
            let max = v.iter().copied().fold(0.0f32, f32::max);
            let over = v.iter().filter(|x| **x > 1.0).count();
            format!("p50 {p50:.3} p99 {p99:.3} max {max:.3} (>1 ms: {over})")
        };
        format!(
            "\n  scatter CPU total {} ms (update {}, collect+seat {}, contact {}); rocks emitted {:.0}; seats/frame fast {:.0} checked {:.0} computed {:.0}",
            stat(total),
            stat(col(0)),
            stat(col(1)),
            stat(col(2)),
            mean(samples.iter().map(|s| s.rocks as f32)),
            mean(samples.iter().map(|s| s.seat[0] as f32)),
            mean(samples.iter().map(|s| s.seat[1] as f32)),
            mean(samples.iter().map(|s| s.seat[2] as f32))
        )
    } else {
        String::new()
    };
    let page_mean: Vec<String> = (0..4)
        .map(|c| {
            let ev = |k: usize| mean(samples.iter().map(|s| s.caster_events[c][k] as f32));
            format!(
                "{:.2} (moved {:.1} new {:.1} gone {:.1})",
                mean(samples.iter().map(|s| s.mesh_pages[c] as f32)),
                ev(0),
                ev(1),
                ev(2)
            )
        })
        .collect();
    let cascade_mean: Vec<String> = (0..4)
        .map(|c| format!("{:.0}", mean(samples.iter().map(|s| s.cascade_nodes[c] as f32))))
        .collect();
    format!(
        "{map:<12} {variant:<18} frames {}  wall p50 {:.2} p99 {:.2} ms  gpu scene p50 {:.2} p99 {:.2} ms  \
         shadows {:.3} prepass {:.3} forward {:.3} ms  cascade_draws {draws} (+{regions} page-region draws; mesh full redraws {full})  \
         nodes {:.0} camera {:.0} cascades [{}]  writes {writes}  prepare {:.3} ms  encode {:.2} ms  \
         cpu render_into p50 {:.2} p99 {:.2} ms  mesh-dirtied sun pages per frame [{}]{scatter}",
        samples.len(),
        percentile(&mut wall.clone(), 0.5),
        percentile(&mut wall, 0.99),
        percentile(&mut scene.clone(), 0.5),
        percentile(&mut scene, 0.99),
        mean(samples.iter().map(|s| s.shadows_ms)),
        mean(samples.iter().map(|s| s.prepass_ms)),
        mean(samples.iter().map(|s| s.forward_ms)),
        mean(samples.iter().map(|s| s.nodes as f32)),
        mean(samples.iter().map(|s| s.camera_nodes as f32)),
        cascade_mean.join(" "),
        mean(samples.iter().map(|s| s.prepare_ms)),
        mean(samples.iter().map(|s| s.encode_ms)),
        percentile(&mut cpu.clone(), 0.5),
        percentile(&mut cpu, 0.99),
        page_mean.join(" | "),
    )
}
