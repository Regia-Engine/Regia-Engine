// SPDX-License-Identifier: MPL-2.0
//! Headless Desktop High session for the alpha rows that need a device.
//!
//! Boot, ready, hitches, memory, look, terrain and quit go through [`lookdev::chase`]
//! — the same drive as `do27_chase --soak minute` — on Switchbacks and Bore Twelve.
//! A tunnel-interior plate writes the mouth screenshot and draws the placed scatter.
//! There is no lab stand-in.

use std::path::Path;
use std::time::Instant;

use lookdev::chase::{ChaseOptions, ChaseRun, runtime_device};
use lookdev::look_r2::{MarsGrade, R2Planets, R2Shot};
use lookdev::soak::{EVENT_CUT, SoakOptions};
use lookdev::{LookdevError, Planet};
use regia_core::tasks::block_on;
use regia_render::{PipelineWarmQueue, RenderDevice, WarmTier};
use regia_ui::{OffscreenTarget, UiColor, UiQuad, UiRenderer};

use super::report::{RgbFrame, frame_has_colour, frame_shows_mouth, rgb_frame};

/// What the headless session measured.
pub(crate) struct GpuFacts {
    /// Adapter name.
    pub adapter: String,
    /// Device creation plus the branding frame, milliseconds (the alpha `boot → branding` budget).
    pub boot_ms: f64,
    /// Device creation, milliseconds.
    pub device_ms: f64,
    /// The branding/splash frame, milliseconds (first present; must be under 1 s cold).
    pub splash_ms: f64,
    /// Pipeline and scatter prewarm of the first map (incl. the comprehensive warm), milliseconds.
    pub prewarm_ms: f64,
    /// First game frame after that prewarm, milliseconds.
    pub first_present_ms: f64,
    /// The Switchbacks soak frame has real RGB colour.
    pub colour_ok: bool,
    /// Luminance of that frame.
    pub colour: RgbFrame,
    /// Camera cuts the soaks counted.
    pub camera_cuts: u64,
    /// One hitch report per map, measured frames only.
    pub hitches: Vec<(String, regia_render::HitchReport)>,
    /// Frame-path creates after warm-up per map, by kind and label (`Texture "sssr depth 0" x4`),
    /// so a failing Hitches row names what was created.
    pub hitch_creates: Vec<(String, String)>,
    /// Resident material-texture bytes (the smaller of the two maps).
    pub resident_bytes: u64,
    /// Budget those bytes were checked against.
    pub budget_bytes: u64,
    /// Either map exceeded its texture budget.
    pub over_budget: bool,
    /// Uncompressed material textures (the larger count).
    pub uncompressed: usize,
    /// Process RSS after the alpha chases and before the v0.1.5 scene rows, bytes
    /// (`u64::MAX` when unreported). Row 5 gates this sample. The ultrawide plate,
    /// the black-sky renderer and the many-units renderer are not included.
    pub rss_bytes: u64,
    /// The device exposes BC texture compression.
    pub bc7: bool,
    /// Host-checklist errors, prefixed with the map.
    pub checklist_errors: Vec<String>,
    /// Host-checklist differences across the maps and the mouth plate.
    pub checklist_diffs: usize,
    /// Difference lines (capped).
    pub checklist_lines: Vec<String>,
    /// `render.problems` lines other than texture-budget notes.
    pub frame_problems: Vec<String>,
    /// Per-map notes.
    pub level_notes: String,
    /// Switchbacks soak frame, PNG.
    pub screenshot: String,
    /// Switchbacks tunnel-interior frame, PNG.
    pub mouth_screenshot: String,
    /// Luminance of the mouth frame.
    pub mouth_colour: RgbFrame,
    /// The mouth frame shows a dark opening and a lit surround.
    pub mouth_ok: bool,
    /// Mean scatter rocks drawn on the Switchbacks soak.
    pub scatter_rocks: f32,
    /// Mean scatter rocks drawn on the tunnel plate.
    pub mouth_rocks: f32,
    /// Every run installed the shared terrain draw.
    pub shared_draw: bool,
    /// Switchbacks exposure bias after the nasa_dark grade, EV.
    pub mars_exposure_ev: f32,
    /// A coloured quad was drawn into the HUD target and read back.
    pub hud_ok: bool,
    /// HUD evidence.
    pub hud_note: String,
    /// Vulkan validation errors popped after the renderers dropped.
    pub validation: Vec<String>,
    /// Boot tier closed the lobby, then the lobby tier reached 100%.
    pub warm_ok: bool,
    /// Warm-queue evidence.
    pub warm_note: String,
    /// Shutdown evidence.
    pub quit_note: String,
    /// The 3840×1600 tunnel plate passed the existing color check.
    pub ultrawide_ok: bool,
    /// Evidence for that plate.
    pub ultrawide_note: String,
    /// Whether the airless TAAU jump stayed within the silhouette cap.
    pub black_sky_ok: bool,
    /// Measured silhouette gain.
    pub black_sky_evidence: String,
    /// Why the airless row passed or failed.
    pub black_sky_reason: String,
    /// The many-units run produced finite encode times. Smoke reports them and does
    /// not gate the slack; a missing or non-finite run fails the row.
    pub units_measured: bool,
    /// Measured encode and `render_into` percentiles.
    pub units_evidence: String,
    /// Why the units row passed or failed.
    pub units_reason: String,
}

/// Inputs for [`session`].
pub(crate) struct SessionArgs<'a> {
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Unmeasured frames of the tunnel plate.
    pub warmup: u32,
    /// Measured frames of the tunnel plate.
    pub frames: u32,
    /// Measured seconds of the hitch soak on each map.
    pub soak_seconds: f32,
    /// Directory for the PNGs.
    pub out_dir: &'a Path,
    /// `REGIA_DO27_CONTENT`, when set. Read only.
    pub content: Option<&'a Path>,
}

/// Opens one headless device, runs the chase soaks and pops the validation scope.
///
/// # Errors
///
/// Content is missing, the adapter is a CPU rasteriser, a chase frame failed, or a
/// screenshot could not be read.
pub(crate) fn session(args: &SessionArgs<'_>) -> Result<GpuFacts, String> {
    let _span = tracing::info_span!("smoke_gpu").entered();
    let started = Instant::now();
    let Some(content) = args.content else {
        return Err("REGIA_DO27_CONTENT is not a directory; the chase has nothing to load".into());
    };
    // Persistent pipeline cache: a second run of this command is a warm boot and compiles almost
    // nothing. `REGIA_PIPELINE_CACHE_DIR` overrides; otherwise the cache lives beside the smoke
    // output. The content salt folds DO27's shader/material version into the cache key.
    if regia_render::pipeline_cache::directory().is_none() {
        regia_render::pipeline_cache::set_directory(Some(args.out_dir.join("pipeline-cache")));
    }
    regia_render::pipeline_cache::set_content_salt("do27-smoke-v1");
    let device = runtime_device().map_err(|err| err.to_string())?;
    let device_ms = elapsed_ms(started);
    let info = device.adapter.get_info();
    if info.device_type == wgpu::DeviceType::Cpu {
        return Err(format!(
            "adapter {} is a CPU rasteriser; the soak needs the hardware GPU",
            info.name
        ));
    }
    // First present: the branding frame the game shows before any prewarm or level load. It
    // builds one splash pipeline, so it stays under a second even on a cold driver cache.
    let splash_ms = present_branding(&device, args.width.min(2560), args.height.min(1440));
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let outcome = run_session(&device, args, content, device_ms, splash_ms);
    if device.device.poll(wgpu::PollType::Poll).is_err() {
        tracing::warn!(target: "xtask", "smoke device poll failed");
    }
    let validation = match block_on(scope.pop()) {
        Some(err) => vec![err.to_string()],
        None => Vec::new(),
    };
    drop(device);
    match outcome {
        Ok(mut facts) => {
            facts.validation = validation;
            if facts.quit_note.is_empty() {
                facts.quit_note = "device dropped".to_owned();
            }
            Ok(facts)
        }
        Err(err) => {
            if validation.is_empty() {
                Err(err)
            } else {
                Err(format!("{err}; validation {}", validation.join(" | ")))
            }
        }
    }
}

fn run_session(
    device: &RenderDevice,
    args: &SessionArgs<'_>,
    content: &Path,
    device_ms: f64,
    splash_ms: f64,
) -> Result<GpuFacts, String> {
    let width = args.width.clamp(16, 3840);
    let height = args.height.clamp(16, 2160);
    std::fs::create_dir_all(args.out_dir)
        .map_err(|err| format!("creating {}: {err}", args.out_dir.display()))?;
    let mut soak = SoakOptions::MINUTE;
    soak.seconds = args.soak_seconds.clamp(5.0, 60.0);
    let mut planets = R2Planets::default();

    tracing::info!(
        target: "xtask",
        "smoke soak {:.0} s at {width}x{height} on {}",
        soak.seconds,
        device.capabilities.adapter_name
    );
    let switchbacks_png = args.out_dir.join("switchbacks-soak.png");
    let mars = chase(
        device,
        &mut planets,
        content,
        Planet::Mars,
        &drive_options(width, height, soak, &switchbacks_png),
    )?;
    let europa_png = args.out_dir.join("bore-twelve-soak.png");
    let europa = chase(
        device,
        &mut planets,
        content,
        Planet::Europa,
        &drive_options(width, height, soak, &europa_png),
    )?;
    let mouth_png = args.out_dir.join("switchbacks-mouths.png");
    let mouths = chase(
        device,
        &mut planets,
        content,
        Planet::Mars,
        &mouth_options(width, height, args.warmup, args.frames, &mouth_png),
    )?;
    // Row 5. Defined point: the three alpha chases have returned and the v0.1.5
    // scene rows have not started. The ultrawide chase keeps its renderer alive
    // for the rest of this function, and the black-sky and many-units renderers
    // allocate their own targets; either would move this sample.
    let rss_bytes = regia_render::process_rss_bytes().unwrap_or(u64::MAX);
    let ultra = lookdev::do27_bench::ULTRAWIDE;
    let ultra_png = args.out_dir.join("ultrawide.png");
    let _ultrawide = chase(
        device,
        &mut planets,
        content,
        Planet::Mars,
        &mouth_options(ultra.0, ultra.1, 1, 1, &ultra_png),
    )?;
    let (black_sky_ok, black_sky_evidence, black_sky_reason) = match lookdev::black_sky::run(device) {
        Ok(report) => (report.passed, report.evidence(), report.reason),
        Err(err) => (false, err.to_string(), err.to_string()),
    };
    let (units_measured, units_evidence, units_reason) = match lookdev::moving_units::run(device) {
        Ok(report) => {
            let finite = report.hull.encode_p50_ms.is_finite() && report.control.encode_p50_ms.is_finite();
            (finite, report.evidence(), report.reason)
        }
        Err(err) => (false, err.to_string(), err.to_string()),
    };

    let colour = read_colour(&switchbacks_png)?;
    let mouth_colour = read_colour(&mouth_png)?;
    let ultra_colour = read_colour(&ultra_png)?;
    let ultrawide_ok = frame_has_colour(&ultra_colour);
    let ultrawide_note = format!(
        "{}x{} mouth plate mean {:.1} stddev {:.1} near-white {:.3}, {}",
        ultra.0,
        ultra.1,
        ultra_colour.mean,
        ultra_colour.stddev,
        ultra_colour.near_white,
        ultra_png.display()
    );
    let (warm_ok, warm_note, quit_note) = warm_and_shutdown(device);
    let (hud_ok, hud_note) = draw_hud(device);

    let mut checklist_errors = Vec::new();
    let mut checklist_lines = Vec::new();
    let mut checklist_diffs = 0usize;
    let mut frame_problems = Vec::new();
    let mut notes = Vec::new();
    let mut shared = true;
    let mut resident = u64::MAX;
    let mut budget = 0u64;
    let mut over = false;
    let mut uncompressed = 0usize;
    for (label, run) in [
        ("switchbacks", &mars),
        ("bore_twelve", &europa),
        ("mouths", &mouths),
    ] {
        checklist_diffs += run.checklist_diffs;
        for line in &run.checklist_errors {
            checklist_errors.push(format!("{label} {line}"));
        }
        for line in run.checklist_diff_lines.iter().take(4) {
            if checklist_lines.len() < 12 {
                checklist_lines.push(format!("{label} {line}"));
            }
        }
        for line in &run.frame_problems {
            frame_problems.push(format!("{label} {line}"));
        }
        shared &= run.shared_draw;
        resident = resident.min(run.resident_bytes);
        budget = budget.max(run.texture_budget_bytes);
        over |= run.texture_over_budget;
        uncompressed = uncompressed.max(run.uncompressed_textures);
        notes.push(format!(
            "{label}: rocks {:.0}, shared {}, exposure {:+.2} EV, checklist diffs {}, resident {} bytes",
            rocks_mean(run),
            run.shared_draw,
            run.exposure_bias_ev,
            run.checklist_diffs,
            run.resident_bytes
        ));
    }
    if resident == u64::MAX {
        resident = 0;
    }

    Ok(GpuFacts {
        adapter: device.capabilities.adapter_name.clone(),
        boot_ms: device_ms + splash_ms,
        device_ms,
        splash_ms,
        prewarm_ms: f64::from(mars.prewarm_ms),
        first_present_ms: f64::from(mars.first_present_ms),
        colour_ok: frame_has_colour(&colour),
        colour,
        camera_cuts: cuts(&mars) + cuts(&europa),
        hitches: vec![
            ("switchbacks".to_owned(), mars.hitch_report()),
            ("bore_twelve".to_owned(), europa.hitch_report()),
        ],
        hitch_creates: vec![
            ("switchbacks".to_owned(), creates_by_label(&mars)),
            ("bore_twelve".to_owned(), creates_by_label(&europa)),
        ],
        resident_bytes: resident,
        budget_bytes: budget,
        over_budget: over,
        uncompressed,
        rss_bytes,
        bc7: device.capabilities.texture_compression_bc,
        checklist_errors,
        checklist_diffs,
        checklist_lines,
        frame_problems,
        level_notes: notes.join("; "),
        screenshot: switchbacks_png.display().to_string(),
        mouth_screenshot: mouth_png.display().to_string(),
        mouth_colour,
        mouth_ok: frame_shows_mouth(&mouth_colour),
        scatter_rocks: rocks_mean(&mars),
        mouth_rocks: rocks_mean(&mouths),
        shared_draw: shared,
        mars_exposure_ev: mars.exposure_bias_ev,
        hud_ok,
        hud_note,
        validation: Vec::new(),
        warm_ok,
        warm_note,
        quit_note,
        ultrawide_ok,
        ultrawide_note,
        black_sky_ok,
        black_sky_evidence,
        black_sky_reason,
        units_measured,
        units_evidence,
        units_reason,
    })
}

/// Rows 19–21 only: the 3840×1600 mouth plate, the airless sky, and many units.
///
/// A missing content directory fails the ultrawide row and still runs the other two.
/// CPU adapters are allowed so a lavapipe check can run; the nightly evidence is the
/// hardware GPU.
///
/// # Errors
///
/// The device could not be opened, or the output directory could not be created.
pub(crate) fn scenes(args: &SessionArgs<'_>) -> Result<GpuFacts, String> {
    let _span = tracing::info_span!("smoke_scenes").entered();
    std::fs::create_dir_all(args.out_dir)
        .map_err(|err| format!("creating {}: {err}", args.out_dir.display()))?;
    if regia_render::pipeline_cache::directory().is_none() {
        regia_render::pipeline_cache::set_directory(Some(args.out_dir.join("pipeline-cache")));
    }
    let device = runtime_device().map_err(|err| err.to_string())?;
    let info = device.adapter.get_info();
    let mut facts = blank_facts(info.name);
    let ultra = lookdev::do27_bench::ULTRAWIDE;
    if let Some(content) = args.content {
        let ultra_png = args.out_dir.join("ultrawide.png");
        let mut planets = R2Planets::default();
        match chase(
            &device,
            &mut planets,
            content,
            Planet::Mars,
            &mouth_options(ultra.0, ultra.1, 1, 1, &ultra_png),
        ) {
            Ok(_) => match read_colour(&ultra_png) {
                Ok(colour) => {
                    facts.ultrawide_ok = frame_has_colour(&colour);
                    facts.ultrawide_note = format!(
                        "{}x{} mouth plate mean {:.1} stddev {:.1} near-white {:.3}, {}",
                        ultra.0,
                        ultra.1,
                        colour.mean,
                        colour.stddev,
                        colour.near_white,
                        ultra_png.display()
                    );
                }
                Err(err) => facts.ultrawide_note = err,
            },
            Err(err) => facts.ultrawide_note = err,
        }
    } else {
        facts.ultrawide_note =
            "REGIA_DO27_CONTENT is not a directory; the ultrawide plate was not rendered".to_owned();
    }
    match lookdev::black_sky::run(&device) {
        Ok(report) => {
            facts.black_sky_ok = report.passed;
            facts.black_sky_evidence = report.evidence();
            facts.black_sky_reason = report.reason;
        }
        Err(err) => {
            facts.black_sky_evidence = err.to_string();
            facts.black_sky_reason = facts.black_sky_evidence.clone();
        }
    }
    match lookdev::moving_units::run(&device) {
        Ok(report) => {
            let finite = report.hull.encode_p50_ms.is_finite() && report.control.encode_p50_ms.is_finite();
            facts.units_measured = finite;
            facts.units_evidence = report.evidence();
            facts.units_reason = report.reason;
        }
        Err(err) => {
            facts.units_evidence = err.to_string();
            facts.units_reason = facts.units_evidence.clone();
        }
    }
    Ok(facts)
}

fn zero_rgb() -> RgbFrame {
    RgbFrame {
        pixels: 0,
        mean: 0.0,
        stddev: 0.0,
        dark: 0.0,
        mid: 0.0,
        near_white: 0.0,
    }
}

fn blank_facts(adapter: String) -> GpuFacts {
    GpuFacts {
        adapter,
        boot_ms: 0.0,
        device_ms: 0.0,
        splash_ms: 0.0,
        prewarm_ms: 0.0,
        first_present_ms: 0.0,
        colour_ok: false,
        colour: zero_rgb(),
        camera_cuts: 0,
        hitches: Vec::new(),
        hitch_creates: Vec::new(),
        resident_bytes: 0,
        budget_bytes: 0,
        over_budget: false,
        uncompressed: 0,
        rss_bytes: u64::MAX,
        bc7: false,
        checklist_errors: Vec::new(),
        checklist_diffs: 0,
        checklist_lines: Vec::new(),
        frame_problems: Vec::new(),
        level_notes: String::new(),
        screenshot: String::new(),
        mouth_screenshot: String::new(),
        mouth_colour: zero_rgb(),
        mouth_ok: false,
        scatter_rocks: 0.0,
        mouth_rocks: 0.0,
        shared_draw: false,
        mars_exposure_ev: 0.0,
        hud_ok: false,
        hud_note: String::new(),
        validation: Vec::new(),
        warm_ok: false,
        warm_note: String::new(),
        quit_note: String::new(),
        ultrawide_ok: false,
        ultrawide_note: String::new(),
        black_sky_ok: false,
        black_sky_evidence: String::new(),
        black_sky_reason: String::new(),
        units_measured: false,
        units_evidence: String::new(),
        units_reason: String::new(),
    }
}

fn drive_options(width: u32, height: u32, soak: SoakOptions, png: &Path) -> ChaseOptions {
    ChaseOptions {
        width,
        height,
        warmup: 120,
        soak: Some(soak),
        png: Some(png.to_path_buf()),
        prewarm: true,
        async_sky: true,
        trace: true,
        mars_grade: Some(MarsGrade::NasaDark),
        ..ChaseOptions::default()
    }
}

fn mouth_options(width: u32, height: u32, warmup: u32, frames: u32, png: &Path) -> ChaseOptions {
    ChaseOptions {
        width,
        height,
        warmup: warmup.clamp(1, 32),
        frames: frames.clamp(1, 16),
        held: true,
        plate: Some(R2Shot::TunnelInterior),
        png: Some(png.to_path_buf()),
        prewarm: true,
        async_sky: true,
        mars_grade: Some(MarsGrade::NasaDark),
        ..ChaseOptions::default()
    }
}

fn chase(
    device: &RenderDevice,
    planets: &mut R2Planets,
    content: &Path,
    planet: Planet,
    options: &ChaseOptions,
) -> Result<ChaseRun, String> {
    let _span = tracing::info_span!("smoke_chase", map = planet.name()).entered();
    lookdev::chase::run(device, planets, content, planet, options).map_err(lookdev_err)
}

fn lookdev_err(err: LookdevError) -> String {
    err.to_string()
}

fn rocks_mean(run: &ChaseRun) -> f32 {
    if run.samples.is_empty() {
        return 0.0;
    }
    run.samples.iter().map(|sample| sample.rocks as f32).sum::<f32>() / run.samples.len() as f32
}

fn cuts(run: &ChaseRun) -> u64 {
    run.samples
        .iter()
        .filter(|sample| sample.events & EVENT_CUT != 0)
        .count() as u64
}

fn read_colour(path: &Path) -> Result<RgbFrame, String> {
    let (_w, _h, pixels) = lookdev::default_look::read_png(path).map_err(|err| err.to_string())?;
    Ok(rgb_frame(&pixels))
}

fn draw_hud(device: &RenderDevice) -> (bool, String) {
    let target = OffscreenTarget::new(device, 64, 64);
    target.clear(device, [0.0, 0.0, 0.0, 1.0]);
    let mut ui = UiRenderer::default();
    ui.set_quads(vec![UiQuad::fill(
        [8.0, 8.0],
        [48.0, 32.0],
        UiColor::linear(0.85, 0.12, 0.05, 1.0),
    )]);
    target.draw_ui(device, &mut ui);
    match target.read(device) {
        Ok(pixels) => {
            let frame = rgb_frame(&pixels);
            let ok = frame_has_colour(&frame) && frame.mid + frame.dark > 0.5;
            (
                ok,
                format!(
                    "OffscreenTarget 64x64 drew a quad, mean {:.1} stddev {:.1}",
                    frame.mean, frame.stddev
                ),
            )
        }
        Err(err) => (false, format!("HUD readback failed: {err}")),
    }
}

/// Runs a boot job, checks the lobby tier is still closed, finishes it, then shuts the
/// queue down the way the runtime does on `WillTerminate`.
fn warm_and_shutdown(device: &RenderDevice) -> (bool, String, String) {
    let queue = PipelineWarmQueue::new(1);
    queue.set_engine_jobs(false);
    queue.push(WarmTier::Boot, "smoke-boot", |_| Ok(()));
    queue.push(WarmTier::Lobby, "smoke-lobby", |_| Ok(()));
    let before = queue.progress();
    let ran_boot = queue.run_inline(device, WarmTier::Boot);
    let mid = queue.progress();
    let ran_lobby = queue.run_inline(device, WarmTier::Lobby);
    let after = queue.progress();
    let gated = !before.is_tier_ready(WarmTier::Lobby)
        && mid.is_tier_ready(WarmTier::Boot)
        && !mid.is_tier_ready(WarmTier::Lobby)
        && after.is_tier_ready(WarmTier::Lobby)
        && ran_boot == 1
        && ran_lobby == 1
        && after.last_error.is_none();
    let note = format!(
        "warm boot ready {} then lobby ready {}, jobs {ran_boot}+{ran_lobby}, fraction {:.2}",
        mid.is_tier_ready(WarmTier::Boot),
        after.is_tier_ready(WarmTier::Lobby),
        after.fraction()
    );
    queue.shutdown();
    let saved = match regia_render::pipeline_cache::save_installed() {
        Some(Ok(bytes)) => format!("pipeline cache saved {bytes} bytes"),
        Some(Err(err)) => format!("pipeline cache save failed: {err}"),
        None => "no pipeline cache installed".to_owned(),
    };
    let quit = format!("warm queue shut down; {saved}; device dropped after the scope");
    (gated, note, quit)
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

/// Renders the branding/splash frame to a small offscreen target and returns its wall time:
/// the "first present" the alpha boot row must keep under a second. It builds exactly one
/// pipeline (the splash quad), so it is fast even on a cold driver cache.
fn present_branding(device: &RenderDevice, width: u32, height: u32) -> f64 {
    let started = Instant::now();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let (w, h) = (width.max(16), height.max(16));
    let tex = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("boot branding"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let mut splash = regia_render::BootSplash::new().with_text("REGIA");
    splash.background = [0.02, 0.02, 0.03, 1.0];
    splash.fade_in = std::time::Duration::ZERO;
    let phase = splash.advance(Instant::now());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("boot branding"),
        });
    splash.record(device, &mut encoder, &view, format, (w, h), phase);
    device.queue.submit([encoder.finish()]);
    if device.device.poll(wgpu::PollType::wait_indefinitely()).is_err() {
        tracing::warn!(target: "xtask", "branding poll failed");
    }
    started.elapsed().as_secs_f64() * 1_000.0
}

/// The run's traced frame-path creates as `Kind "label" xN`, most frequent first (empty when
/// nothing was created after warm-up).
fn creates_by_label(run: &lookdev::chase::ChaseRun) -> String {
    let mut counts: std::collections::BTreeMap<(String, &str), usize> = std::collections::BTreeMap::new();
    for (_, event) in &run.events {
        *counts
            .entry((format!("{:?}", event.kind), event.label.as_str()))
            .or_default() += 1;
    }
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.iter()
        .map(|((kind, label), n)| format!("{kind} {label:?} x{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}
